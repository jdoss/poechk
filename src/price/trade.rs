//! Official pathofexile.com trade API: real listings for any item.
//!
//! The flow is `build_search_body` → POST `/api/trade/search/{league}` →
//! GET `/api/trade/fetch/{ids}?query={id}` (<=10 ids) → listing prices. Requests
//! go through a file-based rate limiter (see `ratelimit`) that honours GGG's
//! `x-rate-limit-*` headers across `check` processes. Every request and its raw
//! response is recorded to the check log (see `checklog`).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::checklog::{self, CheckLog};
use crate::config::Config;
use crate::item::mods::ModType;
use crate::item::{ParsedItem, Rarity};
use crate::price::PriceQuote;
use crate::price::ratelimit::{Bucket, RateLimiter};

const USER_AGENT: &str = concat!(
    "poechk/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/jdoss/poechk)"
);

/// How many cheapest listings to fetch (the fetch endpoint caps at 10 ids).
const FETCH_LIMIT: usize = 10;

/// Build the `POST /api/trade/search` body for an item: search by base type
/// (plus name for uniques), with one stat filter per resolved affix at its
/// current roll, sorted cheapest-first.
/// Per-affix trade filter settings (one per `item.mods` entry).
#[derive(Debug, Clone, Copy, Default)]
pub struct FilterSpec {
    pub enabled: bool,
    /// Search the mod as a plain explicit instead of its special type
    /// (fractured/crafted/…), matching it on any item regardless of provenance.
    pub as_explicit: bool,
    /// Search the mod's per-stat pseudo total (item-wide sum) when available.
    /// Takes precedence over `as_explicit`.
    pub use_pseudo: bool,
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// The trade "status" filter — which sellers/listings to include.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Status {
    /// Secured market listings only: buyable outright, no in-person trade.
    #[default]
    InstantBuyout,
    /// Any online seller, in-person trades included.
    Online,
    /// Everything, including offline listings.
    Any,
}

impl Status {
    /// The trade API `status.option` value. `available` is not the instant
    /// tier — it means "online and priced", which still includes in-person
    /// trades; only `securable` restricts the search to market listings.
    pub fn option(self) -> &'static str {
        match self {
            Status::InstantBuyout => "securable",
            Status::Online => "online",
            Status::Any => "any",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Status::InstantBuyout => "Instant Buyout",
            Status::Online => "Online",
            Status::Any => "Any",
        }
    }
    pub fn next(self) -> Status {
        match self {
            Status::InstantBuyout => Status::Online,
            Status::Online => Status::Any,
            Status::Any => Status::InstantBuyout,
        }
    }
}

/// Item-level trade filters that are not per-affix.
#[derive(Debug, Clone, Copy, Default)]
pub struct MiscFilters {
    pub status: Status,
    /// None = any; Some(true) = corrupted only; Some(false) = uncorrupted only.
    pub corrupted: Option<bool>,
    /// Minimum item level.
    pub ilvl_min: Option<u32>,
    /// Maximum item level.
    pub ilvl_max: Option<u32>,
    /// Minimum total socket count.
    pub sockets_min: Option<u8>,
    /// Minimum size of the largest linked group.
    pub links_min: Option<u8>,
    /// Minimum total DPS (weapons).
    pub dps_min: Option<f64>,
    /// Minimum physical DPS (weapons).
    pub pdps_min: Option<f64>,
    /// Minimum elemental DPS (weapons).
    pub edps_min: Option<f64>,
    /// Search this exact base type instead of its class. Off by default: one
    /// base is usually too thin a market to price against.
    pub exact_base: bool,
}

/// The outcome of a price search: the trade-site URL for it, the total match
/// count, and the cheapest listings.
#[derive(Debug, Clone)]
pub struct PriceResult {
    /// pathofexile.com URL reproducing this search (item search or exchange).
    pub url: String,
    pub total: u32,
    pub quotes: Vec<PriceQuote>,
    /// poe.ninja reference value in chaos (bulk items; sourced from the
    /// in-game currency exchange where applicable).
    pub ninja_chaos: Option<f64>,
}

pub fn build_search_body(item: &ParsedItem, filters: &[FilterSpec], misc: &MiscFilters) -> Value {
    let stat_filters = merge_by_id(
        item.mods
            .iter()
            .enumerate()
            .filter_map(|(i, m)| filters.get(i).and_then(|spec| stat_filter(m, spec))),
    );

    let mut query = json!({
        "status": { "option": misc.status.option() },
        "stats": [ { "type": "and", "filters": stat_filters } ],
    });
    // Uniques are found by name; the base type alone is ambiguous.
    if item.rarity == Some(Rarity::Unique)
        && let Some(name) = &item.name
    {
        query["name"] = json!(name);
    }
    match search_class(item, misc) {
        Some(category) => query["filters"] = filter_groups(misc, Some(category)),
        None => {
            query["type"] = json!(item.base_type);
            if let Some(groups) = item_filter_groups(misc) {
                query["filters"] = groups;
            }
        }
    }

    json!({ "query": query, "sort": { "price": "asc" } })
}

/// The trade category to search instead of the base type, or `None` to search
/// the base type itself.
///
/// A unique keeps its base type: it is already pinned by name, and widening to
/// the class would pull in every other unique sharing it.
fn search_class(item: &ParsedItem, misc: &MiscFilters) -> Option<&'static str> {
    if misc.exact_base || item.rarity == Some(Rarity::Unique) {
        return None;
    }
    crate::price::category::trade_category(item)
}

/// `item_filter_groups` with the type-filter group carrying `category` added.
fn filter_groups(misc: &MiscFilters, category: Option<&str>) -> Value {
    let mut groups = match item_filter_groups(misc) {
        Some(Value::Object(map)) => map,
        _ => serde_json::Map::new(),
    };
    if let Some(category) = category {
        groups.insert(
            "type_filters".to_string(),
            json!({ "filters": { "category": { "option": category } } }),
        );
    }
    Value::Object(groups)
}

/// Collapse filters that target the same trade stat id into one.
///
/// Several rows can resolve to a single id — a mod searched as its per-stat
/// pseudo total lands on the same `pseudo.pseudo_total_life` as the folded
/// total-life row. Sending both leaves the trade site quietly applying the
/// tighter bound while the looser row looks like it is doing something, so keep
/// the narrowest bound and drop the duplicate. An enabled row wins over a
/// disabled one, since disabled carries no bound at all.
fn merge_by_id(filters: impl Iterator<Item = Value>) -> Vec<Value> {
    let mut merged: Vec<Value> = Vec::new();
    for filter in filters {
        let Some(existing) = merged.iter_mut().find(|f| f["id"] == filter["id"]) else {
            merged.push(filter);
            continue;
        };
        if filter["disabled"] == Value::Bool(false) {
            existing["disabled"] = Value::Bool(false);
        }
        narrow(existing, &filter, "min", f64::max);
        narrow(existing, &filter, "max", f64::min);
    }
    merged
}

/// Tighten `target`'s `key` bound towards `other`'s using `pick`, taking
/// `other`'s outright when `target` has no bound of that kind.
fn narrow(target: &mut Value, other: &Value, key: &str, pick: fn(f64, f64) -> f64) {
    let Some(incoming) = other["value"].get(key).and_then(Value::as_f64) else {
        return;
    };
    let tightened = match target["value"].get(key).and_then(Value::as_f64) {
        Some(current) => pick(current, incoming),
        None => incoming,
    };
    if !target["value"].is_object() {
        target["value"] = json!({});
    }
    target["value"][key] = json!(tightened);
}

/// A `{"min": …, "max": …}` bound, or `None` when neither end is set.
fn bound(min: Option<impl Into<Value>>, max: Option<impl Into<Value>>) -> Option<Value> {
    let mut range = serde_json::Map::new();
    if let Some(min) = min {
        range.insert("min".to_string(), min.into());
    }
    if let Some(max) = max {
        range.insert("max".to_string(), max.into());
    }
    (!range.is_empty()).then(|| Value::Object(range))
}

/// The `query.filters` groups for the non-affix filters, or `None` when none of
/// them are set.
fn item_filter_groups(misc: &MiscFilters) -> Option<Value> {
    let mut groups = serde_json::Map::new();

    let mut misc_filters = serde_json::Map::new();
    if let Some(corrupted) = misc.corrupted {
        let option = if corrupted { "true" } else { "false" };
        misc_filters.insert("corrupted".to_string(), json!({ "option": option }));
    }
    if let Some(ilvl) = bound(misc.ilvl_min, misc.ilvl_max) {
        misc_filters.insert("ilvl".to_string(), ilvl);
    }
    if !misc_filters.is_empty() {
        groups.insert(
            "misc_filters".to_string(),
            json!({ "filters": Value::Object(misc_filters) }),
        );
    }

    let mut socket_filters = serde_json::Map::new();
    if let Some(sockets) = bound(misc.sockets_min, None::<u8>) {
        socket_filters.insert("sockets".to_string(), sockets);
    }
    if let Some(links) = bound(misc.links_min, None::<u8>) {
        socket_filters.insert("links".to_string(), links);
    }
    if !socket_filters.is_empty() {
        groups.insert(
            "socket_filters".to_string(),
            json!({ "filters": Value::Object(socket_filters) }),
        );
    }

    let mut weapon_filters = serde_json::Map::new();
    for (key, value) in [
        ("dps", misc.dps_min),
        ("pdps", misc.pdps_min),
        ("edps", misc.edps_min),
    ] {
        if let Some(range) = bound(value, None::<f64>) {
            weapon_filters.insert(key.to_string(), range);
        }
    }
    if !weapon_filters.is_empty() {
        groups.insert(
            "weapon_filters".to_string(),
            json!({ "filters": Value::Object(weapon_filters) }),
        );
    }

    (!groups.is_empty()).then(|| Value::Object(groups))
}

/// The pathofexile.com trade URL for a completed search, to open in a browser.
pub fn search_url(league: &str, search_id: &str) -> String {
    format!(
        "https://www.pathofexile.com/trade/search/{}/{}",
        league.replace(' ', "%20"),
        search_id
    )
}


#[derive(Deserialize)]
struct ApiLeague {
    id: String,
    #[serde(default)]
    rules: Vec<LeagueRule>,
}

#[derive(Deserialize)]
struct LeagueRule {
    id: String,
}

/// Fetch the trade-searchable leagues (SSF excluded, since it can't be traded),
/// and cache the result to disk.
pub fn fetch_leagues() -> anyhow::Result<Vec<String>> {
    let url = "https://www.pathofexile.com/api/leagues?type=main&realm=pc";
    let mut resp = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(|e| anyhow::anyhow!("leagues request failed: {e}"))?;
    let leagues: Vec<ApiLeague> = resp
        .body_mut()
        .read_json()
        .map_err(|e| anyhow::anyhow!("parsing leagues response: {e}"))?;
    let ids: Vec<String> = leagues
        .into_iter()
        .filter(|league| !league.rules.iter().any(|rule| rule.id == "NoParties"))
        .map(|league| league.id)
        .collect();
    save_league_cache(&ids);
    Ok(ids)
}

/// The leagues from the on-disk cache (empty if there is none yet).
pub fn cached_leagues() -> Vec<String> {
    league_cache_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn league_cache_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("io.github", "jdoss", "poechk")
        .map(|dirs| dirs.cache_dir().join("leagues.json"))
}

fn save_league_cache(ids: &[String]) {
    let Some(path) = league_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(ids) {
        let _ = std::fs::write(path, json);
    }
}

/// Default filters: a unique's affix rolls vary widely, so it is searched by
/// name with affixes off; everything else filters on all resolved affixes at
/// their current roll. The roll seeds the floor (min = roll) normally, and the
/// cap (max = roll) for stats where less is better — a cluster jewel's added
/// passive count, where 8/8 beats 9/8. Crafted mods start disabled: any buyer
/// can re-craft them, so they shouldn't constrain the search.
///
/// Implicits start disabled too. Requiring one narrows the search far harder
/// than its contribution to price usually warrants — an Exarch/Eater pair at
/// roll can cut a body armour's matches to a single listing before any affix
/// is considered — so they are offered as a choice rather than assumed.
pub fn default_filters(item: &ParsedItem) -> Vec<FilterSpec> {
    let enabled = item.rarity != Some(Rarity::Unique);
    item.mods
        .iter()
        .map(|m| {
            FilterSpec {
                enabled: enabled && !matches!(m.mod_type, ModType::Crafted | ModType::Implicit),
                as_explicit: false,
                use_pseudo: !m.pseudo_ids.is_empty(),
                min: (!m.lower_is_better).then(|| seed_bound(m, Bound::Floor)).flatten(),
                max: m.lower_is_better.then(|| seed_bound(m, Bound::Cap)).flatten(),
            }
        })
        .collect()
}

/// Which end of the roll a seeded filter bounds.
#[derive(Clone, Copy, PartialEq)]
enum Bound {
    Floor,
    Cap,
}

/// How far past the roll to seed when the tier is unknown, as a fraction of it.
const ROLL_MARGIN: f64 = 0.1;

/// The bound to seed a filter with: the mod's tier limit when the copy carried
/// it, else a margin past the roll.
///
/// Seeding the exact roll asks for items that match or beat this one on every
/// enabled mod at once, which even a near-identical item fails — a dagger
/// rolling `Adds 83 to 177 Cold Damage` averages 130 against this item's 131.5
/// and drops out, though both are the same tier of the same mod. The tier limit
/// asks what a price check means: this mod, at this tier.
///
/// The margin widens away from the roll in whichever direction is worse, so a
/// negative roll still matches its own filter.
fn seed_bound(m: &crate::item::mods::ParsedMod, bound: Bound) -> Option<f64> {
    let roll = m.roll();
    let direction: fn(f64) -> f64 = match bound {
        Bound::Floor => f64::floor,
        Bound::Cap => f64::ceil,
    };
    // An enchant is picked from a list rather than rolled inside a tier, so it
    // has no near-miss to accommodate: a cluster jewel grants 8 passives or it
    // does not, and widening the cap to 8.8 only obscures that.
    if m.mod_type == ModType::Enchant {
        return Some(round_outward(roll?, direction));
    }
    let limit = match (m.tier_range, bound) {
        (Some((min, _)), Bound::Floor) => min,
        (Some((_, max)), Bound::Cap) => max,
        (None, Bound::Floor) => roll? - roll?.abs() * ROLL_MARGIN,
        (None, Bound::Cap) => roll? + roll?.abs() * ROLL_MARGIN,
    };
    let widened = round_outward(limit, direction);
    // A roll smaller than the margin rounds away to zero, which is no bound at
    // all: 0.1% Life Regeneration would stop filtering on the roll entirely.
    // There is nothing to widen into at that size, so keep the roll itself.
    if widened == 0.0 && roll.is_some_and(|r| r != 0.0) {
        return Some(round_outward(roll?, direction));
    }
    Some(widened)
}

/// Round a roll away from the item so the item itself still matches its own
/// filter — down for a floor, up for a cap. Rounding to a tenth rather than a
/// whole number keeps fractional stats meaningful: flooring a 0.1% Life
/// Regeneration roll to 0 would drop the roll from the search entirely.
fn round_outward(roll: f64, direction: fn(f64) -> f64) -> f64 {
    direction(roll * 10.0) / 10.0
}

/// One trade stat filter for a resolved mod, or `None` if it has no trade id.
fn stat_filter(m: &crate::item::mods::ParsedMod, spec: &FilterSpec) -> Option<Value> {
    let ids = if spec.use_pseudo && !m.pseudo_ids.is_empty() {
        &m.pseudo_ids
    } else if spec.as_explicit && !m.explicit_ids.is_empty() {
        &m.explicit_ids
    } else {
        &m.trade_ids
    };
    let id = ids.first()?;
    let mut filter = json!({ "id": id, "disabled": !spec.enabled });
    if !spec.enabled {
        return Some(filter);
    }
    // An option stat picks one entry from the trade site's list (which cluster
    // jewel enchant, which allocated notable) — it has no range to bound.
    if let Some(option) = m.option {
        filter["value"] = json!({ "option": option });
        return Some(filter);
    }
    let mut value = serde_json::Map::new();
    if let Some(min) = spec.min {
        value.insert("min".to_string(), json!(min));
    }
    if let Some(max) = spec.max {
        value.insert("max".to_string(), json!(max));
    }
    if !value.is_empty() {
        filter["value"] = Value::Object(value);
    }
    Some(filter)
}

/// `POST /api/trade/search/{league}` response.
#[derive(Debug, Deserialize)]
pub struct SearchResponse {
    /// The search id, echoed back to `/fetch?query=`.
    pub id: String,
    /// Listing hash ids, ordered cheapest-first.
    pub result: Vec<String>,
    pub total: u32,
}

/// `GET /api/trade/fetch/{ids}` response.
#[derive(Debug, Deserialize)]
pub struct FetchResponse {
    pub result: Vec<Option<Listing>>,
}

/// A single fetched listing.
#[derive(Debug, Deserialize)]
pub struct Listing {
    pub listing: ListingInfo,
}

#[derive(Debug, Deserialize)]
pub struct ListingInfo {
    pub price: Option<Price>,
    pub account: Account,
    /// ISO-8601 time the listing was indexed.
    #[serde(default)]
    pub indexed: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Price {
    pub amount: f64,
    pub currency: String,
}

#[derive(Debug, Deserialize)]
pub struct Account {
    pub name: String,
}

/// How long GGG's `Retry-After` asks us to hold off, when it says so.
fn retry_after(headers: &ureq::http::HeaderMap) -> Option<Duration> {
    let seconds: f64 = headers.get("retry-after")?.to_str().ok()?.trim().parse().ok()?;
    (seconds > 0.0).then(|| Duration::from_secs_f64(seconds))
}

/// GGG's shortest advertised penalty, used when a 429 carries no `Retry-After`.
const DEFAULT_PENALTY: Duration = Duration::from_secs(60);

/// Turn a 429 into a recorded penalty and an error naming the wait. Returns
/// `Ok(())` for any other status so the caller can handle it normally.
fn note_rate_limit(
    endpoint: &str,
    status: u16,
    headers: &ureq::http::HeaderMap,
    limiter: &RateLimiter,
) -> anyhow::Result<()> {
    if status != 429 {
        return Ok(());
    }
    let penalty = retry_after(headers).unwrap_or(DEFAULT_PENALTY);
    // Persist it so the next press — and any other check process — waits rather
    // than spending the request and deepening the penalty.
    limiter.penalize(endpoint, penalty);
    anyhow::bail!(
        "rate limited by the trade API — retry in {}s",
        penalty.as_secs()
    )
}

/// The sliding-window limits a response advertises, read from the
/// `x-rate-limit-<rule>` headers. Each is a `max:window:penalty` triple —
/// "5:10:60,15:60:300" means at most 5 requests per 10s and 15 per 60s. The
/// penalty is what GGG imposes on a violation, not a limit, so it is ignored.
fn buckets_from_headers(headers: &ureq::http::HeaderMap) -> Vec<Bucket> {
    let Some(rules) = headers.get("x-rate-limit-rules").and_then(|v| v.to_str().ok()) else {
        return Vec::new();
    };
    let mut buckets = Vec::new();
    for rule in rules.split(',') {
        let key = format!("x-rate-limit-{}", rule.trim().to_ascii_lowercase());
        let Some(spec) = headers.get(&key).and_then(|v| v.to_str().ok()) else {
            continue;
        };
        for bucket in spec.split(',') {
            let mut parts = bucket.split(':');
            if let (Some(max), Some(window)) = (parts.next(), parts.next())
                && let (Ok(max), Ok(window)) = (max.trim().parse::<u32>(), window.trim().parse::<f64>())
                && max > 0
                && window > 0.0
            {
                buckets.push(Bucket { max, window });
            }
        }
    }
    buckets
}

/// Read a response body to text so it can be both logged verbatim and parsed.
///
/// Separate from parsing so that a truncated transfer says so, rather than
/// reaching the parser as an empty body and surfacing as "EOF while parsing".
fn read_body(body: &mut ureq::Body, what: &str) -> anyhow::Result<String> {
    body.read_to_string()
        .map_err(|e| anyhow::anyhow!("reading {what} response: {e}"))
}

/// Prices items against the official trade search + fetch endpoints.
#[derive(Debug)]
pub struct TradeSource {
    host: String,
    league: String,
    poesessid: Option<String>,
    latency_margin: f64,
    log: CheckLog,
}

impl TradeSource {
    /// Build a trade source from user config.
    pub fn new(cfg: &Config) -> Self {
        Self {
            host: "www.pathofexile.com".to_string(),
            league: cfg.league.clone(),
            poesessid: cfg.poesessid.clone(),
            latency_margin: cfg.api_latency_seconds,
            log: CheckLog::open(),
        }
    }

    /// Search + fetch, returning the total match count and the cheapest listings
    /// (cheapest-first). `enabled[i]` turns the filter for `item.mods[i]` on.
    pub fn price(
        &self,
        item: &ParsedItem,
        filters: &[FilterSpec],
        misc: &MiscFilters,
    ) -> anyhow::Result<PriceResult> {
        let body = build_search_body(item, filters, misc);
        let limiter = RateLimiter::open()?;

        limiter.wait("search", self.latency_margin)?;
        let search = self.search(&body, &limiter)?;
        let url = search_url(&self.league, &search.id);
        if search.result.is_empty() {
            self.log
                .event("priced", json!({ "url": url, "total": search.total, "quotes": 0 }));
            return Ok(PriceResult {
                url,
                total: search.total,
                quotes: Vec::new(),
                ninja_chaos: None,
            });
        }
        let ids: Vec<String> = search.result.iter().take(FETCH_LIMIT).cloned().collect();

        limiter.wait("fetch", self.latency_margin)?;
        let listings = self.fetch(&ids, &search.id, &limiter)?;

        let quotes = listings
            .into_iter()
            .flatten()
            .filter_map(|listing| {
                listing.listing.price.map(|price| PriceQuote {
                    amount: price.amount,
                    currency: price.currency,
                    source: "trade".to_string(),
                })
            })
            .collect::<Vec<PriceQuote>>();
        self.log.event(
            "priced",
            json!({ "url": url, "total": search.total, "quotes": quotes.len() }),
        );
        Ok(PriceResult {
            url,
            total: search.total,
            quotes,
            ninja_chaos: None,
        })
    }

    fn search(&self, body: &Value, limiter: &RateLimiter) -> anyhow::Result<SearchResponse> {
        let url = format!(
            "https://{}/api/trade/search/{}",
            self.host,
            self.league.replace(' ', "%20")
        );
        let mut req = ureq::post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT);
        if let Some(sess) = &self.poesessid {
            req = req.header("Cookie", format!("POESESSID={sess}"));
        }
        self.log.event("search_req", json!({ "url": url, "body": body }));
        let started = Instant::now();
        let mut resp = req
            .config()
            .http_status_as_error(false)
            .build()
            .send_json(body)
            .inspect_err(|e| self.log.event("search_failed", json!({ "error": e.to_string() })))
            .map_err(|e| anyhow::anyhow!("trade search request failed: {e}"))?;
        limiter.record("search", &buckets_from_headers(resp.headers()));
        let status = resp.status().as_u16();
        let raw = read_body(resp.body_mut(), "trade search").inspect_err(|e| {
            self.log
                .event("search_failed", json!({ "status": status, "error": e.to_string() }))
        })?;
        self.log.event(
            "search_resp",
            json!({
                "status": status,
                "elapsed_ms": started.elapsed().as_millis(),
                "body": checklog::body(&raw),
            }),
        );
        note_rate_limit("search", status, resp.headers(), limiter)?;
        if !(200..300).contains(&status) {
            anyhow::bail!("trade search rejected ({})", crate::price::api_error(status, &raw));
        }
        serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing trade search response: {e}"))
    }

    fn fetch(
        &self,
        ids: &[String],
        query_id: &str,
        limiter: &RateLimiter,
    ) -> anyhow::Result<Vec<Option<Listing>>> {
        let url = format!(
            "https://{}/api/trade/fetch/{}?query={}",
            self.host,
            ids.join(","),
            query_id
        );
        let mut req = ureq::get(&url)
            .header("Accept", "application/json")
            .header("User-Agent", USER_AGENT);
        if let Some(sess) = &self.poesessid {
            req = req.header("Cookie", format!("POESESSID={sess}"));
        }
        // The ids are in the url, and the search response holds the full result
        // list they came from, so logging them again would only pad the line.
        self.log.event("fetch_req", json!({ "url": url }));
        let started = Instant::now();
        let mut resp = req
            .config()
            .http_status_as_error(false)
            .build()
            .call()
            .inspect_err(|e| self.log.event("fetch_failed", json!({ "error": e.to_string() })))
            .map_err(|e| anyhow::anyhow!("trade fetch request failed: {e}"))?;
        limiter.record("fetch", &buckets_from_headers(resp.headers()));
        let status = resp.status().as_u16();
        let raw = read_body(resp.body_mut(), "trade fetch").inspect_err(|e| {
            self.log
                .event("fetch_failed", json!({ "status": status, "error": e.to_string() }))
        })?;
        self.log.event(
            "fetch_resp",
            json!({
                "status": status,
                "elapsed_ms": started.elapsed().as_millis(),
                "body": checklog::body(&raw),
            }),
        );
        note_rate_limit("fetch", status, resp.headers(), limiter)?;
        if !(200..300).contains(&status) {
            anyhow::bail!("trade fetch rejected ({})", crate::price::api_error(status, &raw));
        }
        let parsed: FetchResponse = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing trade fetch response: {e}"))?;
        Ok(parsed.result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::mods::{ModType, ParsedMod};

    fn mod_with(stat_ref: &str, id: &str, roll: Option<f64>) -> ParsedMod {
        ParsedMod {
            text: stat_ref.to_string(),
            mod_type: ModType::Explicit,
            slot: None,
            template: stat_ref.to_string(),
            rolls: roll.map(|r| vec![r]).unwrap_or_default(),
            tier_range: None,
            option: None,
            lower_is_better: false,
            stat_ref: stat_ref.to_string(),
            trade_ids: vec![id.to_string()],
            explicit_ids: vec![id.to_string()],
            pseudo_ids: vec![],
        }
    }

    /// The case from a real check: this dagger's `Adds 91 to 172 Cold Damage`
    /// averages 131.5, and a listed dagger rolling `83 to 177` averages 130 —
    /// the same mod at the same tier, dropped by seeding the exact roll.
    #[test]
    fn a_filter_seeds_the_mods_tier_floor_not_the_roll_it_happens_to_have() {
        let mut cold = mod_with("Adds # to # Cold Damage", "explicit.stat_1037193709", None);
        cold.rolls = vec![91.0, 172.0];
        cold.tier_range = Some((122.0, 150.0));
        let item = ParsedItem {
            base_type: "Ambusher".to_string(),
            rarity: Some(Rarity::Rare),
            mods: vec![cold],
            ..Default::default()
        };

        let seeded = default_filters(&item)[0].min.unwrap();
        assert_eq!(seeded, 122.0);
        assert!(seeded < 130.0, "a same-tier roll of 130 has to survive the filter");
        assert!(seeded <= 131.5, "the item still matches its own filter");
    }

    #[test]
    fn without_a_tier_the_filter_falls_back_to_a_margin_below_the_roll() {
        let mut item = ParsedItem {
            rarity: Some(Rarity::Rare),
            mods: vec![mod_with("+# to maximum Life", "explicit.stat_3299347043", Some(133.0))],
            ..Default::default()
        };
        assert_eq!(default_filters(&item)[0].min, Some(119.7));

        // The margin widens away from the roll whichever way it points, so a
        // negative roll still matches the filter seeded from it.
        item.mods = vec![mod_with("#% reduced Mana Cost", "explicit.stat_x", Some(-60.0))];
        assert_eq!(default_filters(&item)[0].min, Some(-66.0));
    }

    #[test]
    fn a_roll_too_small_to_widen_keeps_its_own_value() {
        let mut item = ParsedItem {
            rarity: Some(Rarity::Rare),
            mods: vec![mod_with("Regenerate #% of Life", "explicit.stat_y", Some(0.1))],
            ..Default::default()
        };
        // A tenth widened by a tenth rounds to zero, which is no filter at all.
        assert_eq!(default_filters(&item)[0].min, Some(0.1));

        item.mods[0].lower_is_better = true;
        assert_eq!(default_filters(&item)[0].max, Some(0.2));
    }

    fn rare_dagger() -> ParsedItem {
        ParsedItem {
            base_type: "Ambusher".to_string(),
            item_class: "Daggers".to_string(),
            category: Some("Dagger".to_string()),
            rarity: Some(Rarity::Rare),
            mods: vec![mod_with("+# to maximum Life", "explicit.stat_3299347043", Some(70.0))],
            ..Default::default()
        }
    }

    #[test]
    fn a_search_asks_for_the_item_class_rather_than_its_base_type() {
        let item = rare_dagger();
        let body = build_search_body(&item, &default_filters(&item), &MiscFilters::default());

        // One base is too thin a market to price against, so the class is the
        // default and the base type drops out of the query entirely.
        assert_eq!(
            body["query"]["filters"]["type_filters"]["filters"]["category"]["option"],
            "weapon.basedagger"
        );
        assert!(body["query"].get("type").is_none());
        // The affixes still do the discriminating.
        assert_eq!(body["query"]["stats"][0]["filters"][0]["value"]["min"], 63.0);
    }

    #[test]
    fn exact_base_swaps_the_class_back_for_the_base_type() {
        let item = rare_dagger();
        let misc = MiscFilters {
            exact_base: true,
            ..Default::default()
        };
        let body = build_search_body(&item, &default_filters(&item), &misc);

        assert_eq!(body["query"]["type"], "Ambusher");
        assert!(body["query"]["filters"].get("type_filters").is_none());
    }

    #[test]
    fn the_class_filter_joins_the_other_filter_groups_rather_than_replacing_them() {
        let item = rare_dagger();
        let misc = MiscFilters {
            ilvl_min: Some(84),
            dps_min: Some(400.0),
            ..Default::default()
        };
        let body = build_search_body(&item, &default_filters(&item), &misc);
        let groups = &body["query"]["filters"];

        assert_eq!(groups["type_filters"]["filters"]["category"]["option"], "weapon.basedagger");
        assert_eq!(groups["misc_filters"]["filters"]["ilvl"]["min"], 84);
        assert_eq!(groups["weapon_filters"]["filters"]["dps"]["min"], 400.0);
    }

    #[test]
    fn a_unique_keeps_its_base_type_however_broad_its_class_would_be() {
        let item = ParsedItem {
            base_type: "Ambusher".to_string(),
            item_class: "Daggers".to_string(),
            category: Some("Dagger".to_string()),
            name: Some("Cospri's Malice".to_string()),
            rarity: Some(Rarity::Unique),
            ..Default::default()
        };
        let body = build_search_body(&item, &[], &MiscFilters::default());

        // Widening a unique to its class would price it against every other
        // unique sharing the base.
        assert_eq!(body["query"]["name"], "Cospri's Malice");
        assert_eq!(body["query"]["type"], "Ambusher");
        assert!(body["query"]["filters"].get("type_filters").is_none());
    }

    #[test]
    fn a_class_with_no_trade_category_still_searches_its_base_type() {
        let item = ParsedItem {
            base_type: "Cemetery Map".to_string(),
            item_class: "Maps".to_string(),
            category: Some("Map".to_string()),
            rarity: Some(Rarity::Rare),
            ..Default::default()
        };
        let body = build_search_body(&item, &[], &MiscFilters::default());

        assert_eq!(body["query"]["type"], "Cemetery Map");
        assert!(body["query"].get("filters").is_none());
    }

    /// The case from a real check: a rare body armour whose Searing Exarch and
    /// Eater of Worlds implicits, required at roll, cut the trade site to one
    /// listing before any affix was applied.
    #[test]
    fn implicits_start_off_so_they_do_not_narrow_the_search_uninvited() {
        let mut endurance = mod_with(
            "Gain an Endurance Charge every # seconds",
            "implicit.stat_2555092341",
            Some(15.0),
        );
        endurance.mod_type = ModType::Implicit;
        let mut phys_as_fire = mod_with(
            "#% of Physical Damage from Hits taken as Fire Damage",
            "implicit.stat_3342989455",
            Some(10.0),
        );
        phys_as_fire.mod_type = ModType::Implicit;
        let item = ParsedItem {
            base_type: "Conquest Lamellar".to_string(),
            rarity: Some(Rarity::Rare),
            mods: vec![
                endurance,
                phys_as_fire,
                mod_with("+# to maximum Life", "explicit.stat_3299347043", Some(133.0)),
            ],
            ..Default::default()
        };

        let defaults = default_filters(&item);
        assert!(!defaults[0].enabled, "implicits are the user's to opt into");
        assert!(!defaults[1].enabled);
        assert!(defaults[2].enabled, "explicit affixes still seed the search");

        // Disabled rows still ride along so the overlay can offer them, but
        // they carry no bound and so cannot constrain the query.
        let filters = build_search_body(&item, &defaults, &MiscFilters::default())["query"]["stats"]
            [0]["filters"]
            .clone();
        assert_eq!(filters[0]["id"], "implicit.stat_2555092341");
        assert_eq!(filters[0]["disabled"], true);
        assert!(filters[0].get("value").is_none());
        assert_eq!(filters[2]["disabled"], false);
        assert_eq!(filters[2]["value"]["min"], 119.7);
    }

    #[test]
    fn socket_filters_and_crafted_default_off() {
        let mut crafted = mod_with("#% increased Armour and Evasion", "crafted.stat_x", Some(74.0));
        crafted.mod_type = ModType::Crafted;
        let item = ParsedItem {
            base_type: "Conquest Lamellar".to_string(),
            rarity: Some(Rarity::Rare),
            sockets: Some(6),
            links: Some(6),
            mods: vec![
                mod_with("+# to maximum Life", "explicit.stat_3299347043", Some(159.0)),
                crafted,
            ],
            ..Default::default()
        };

        // Crafted mods default to disabled; normal explicits stay on.
        let defaults = default_filters(&item);
        assert!(defaults[0].enabled);
        assert!(!defaults[1].enabled);

        let body = build_search_body(
            &item,
            &defaults,
            &MiscFilters {
                sockets_min: Some(6),
                links_min: Some(6),
                ..Default::default()
            },
        );
        assert_eq!(
            body["query"]["filters"]["socket_filters"]["filters"]["links"]["min"],
            6
        );
        assert_eq!(
            body["query"]["filters"]["socket_filters"]["filters"]["sockets"]["min"],
            6
        );
    }

    #[test]
    fn weapon_dps_filters() {
        let item = ParsedItem {
            base_type: "Jewelled Foil".to_string(),
            rarity: Some(Rarity::Rare),
            ..Default::default()
        };
        let body = build_search_body(
            &item,
            &[],
            &MiscFilters {
                pdps_min: Some(98.0),
                edps_min: Some(283.0),
                dps_min: Some(381.0),
                ..Default::default()
            },
        );
        let weapon = &body["query"]["filters"]["weapon_filters"]["filters"];
        assert_eq!(weapon["pdps"]["min"], 98.0);
        assert_eq!(weapon["edps"]["min"], 283.0);
        assert_eq!(weapon["dps"]["min"], 381.0);
    }

    #[test]
    fn as_explicit_downgrades_special_types() {
        let mut fractured = mod_with("+# to maximum Life", "fractured.stat_3299347043", Some(80.0));
        fractured.mod_type = ModType::Fractured;
        fractured.explicit_ids = vec!["explicit.stat_3299347043".to_string()];
        let item = ParsedItem {
            base_type: "Spine Bow".to_string(),
            rarity: Some(Rarity::Rare),
            mods: vec![fractured],
            ..Default::default()
        };

        let typed = build_search_body(
            &item,
            &[FilterSpec {
                enabled: true,
                as_explicit: false,
                use_pseudo: false,
                min: Some(80.0),
                max: None,
            }],
            &MiscFilters::default(),
        );
        assert_eq!(
            typed["query"]["stats"][0]["filters"][0]["id"],
            "fractured.stat_3299347043"
        );

        let downgraded = build_search_body(
            &item,
            &[FilterSpec {
                enabled: true,
                as_explicit: true,
                use_pseudo: false,
                min: Some(80.0),
                max: None,
            }],
            &MiscFilters::default(),
        );
        assert_eq!(
            downgraded["query"]["stats"][0]["filters"][0]["id"],
            "explicit.stat_3299347043"
        );
    }

    #[test]
    fn builds_search_body_with_stat_filters() {
        let item = ParsedItem {
            base_type: "Jewelled Foil".to_string(),
            rarity: Some(Rarity::Rare),
            mods: vec![
                mod_with("#% increased Attack Speed", "explicit.stat_210067635", Some(30.0)),
                mod_with("Hits can't be Evaded", "crafted.stat_4126210832", None),
            ],
            ..Default::default()
        };

        let body = build_search_body(&item, &default_filters(&item), &MiscFilters::default());

        assert_eq!(body["query"]["type"], "Jewelled Foil");
        assert_eq!(body["query"]["status"]["option"], "securable");
        assert_eq!(body["sort"]["price"], "asc");
        assert!(body["query"].get("name").is_none());

        let filters = body["query"]["stats"][0]["filters"].as_array().unwrap();
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0]["id"], "explicit.stat_210067635");
        assert_eq!(filters[0]["disabled"], false);
        // Seeded a margin below the 30 roll: this copy carried no tier range,
        // and the exact roll would ask for items that beat it outright.
        assert_eq!(filters[0]["value"]["min"], 27.0);
        // Flag mod: filter by presence, no value.
        assert_eq!(filters[1]["id"], "crafted.stat_4126210832");
        assert!(filters[1].get("value").is_none());
    }

    #[test]
    fn uniques_search_by_name_with_disabled_filters() {
        let item = ParsedItem {
            base_type: "Seaglass Amulet".to_string(),
            name: Some("Whispers of Infinity".to_string()),
            rarity: Some(Rarity::Unique),
            mods: vec![mod_with(
                "+# to maximum Energy Shield",
                "explicit.stat_3489782002",
                Some(81.0),
            )],
            ..Default::default()
        };
        let body = build_search_body(&item, &default_filters(&item), &MiscFilters::default());
        assert_eq!(body["query"]["name"], "Whispers of Infinity");
        assert_eq!(body["query"]["type"], "Seaglass Amulet");
        // Present but disabled, so the search returns every listing of the unique.
        let filters = body["query"]["stats"][0]["filters"].as_array().unwrap();
        assert_eq!(filters[0]["disabled"], true);
        assert!(filters[0].get("value").is_none());
    }

    #[test]
    fn cluster_jewel_searches_by_stat_id_and_caps_the_passive_count() {
        const CLUSTER_JEWEL: &str = r#"Item Class: Jewels
Rarity: Magic
Large Cluster Jewel of the Lost
--------
Item Level: 84
--------
Adds 8 Passive Skills (enchant)
1 Added Passive Skill is a Jewel Socket (enchant)
Added Small Passive Skills grant: Minions deal 10% increased Damage (enchant)
"#;
        let stats = crate::data::load_stats();
        let items = crate::data::load_items();
        let item =
            crate::item::parse::parse_item(CLUSTER_JEWEL, crate::item::Game::Poe1, &stats, &items)
                .unwrap();
        let body = build_search_body(&item, &default_filters(&item), &MiscFilters::default());

        assert_eq!(body["query"]["type"], "Large Cluster Jewel");
        let filters = body["query"]["stats"][0]["filters"].as_array().unwrap();
        let by_id = |id: &str| {
            filters
                .iter()
                .find(|f| f["id"] == id)
                .unwrap_or_else(|| panic!("no filter for {id}"))
        };

        // The enchant text is its own stat id, searched without a numeric range.
        let granted = by_id("enchant.stat_3948993189|17");
        assert!(granted["value"].get("option").is_none());
        assert!(granted["value"].get("min").is_none());

        // Fewer added passives is better, so 8 is the cap, not the floor. An
        // enchant is not widened: 8/8 stays 8/8.
        let passives = by_id("enchant.stat_3086156145");
        assert_eq!(passives["value"]["max"], 8.0);
        assert!(passives["value"].get("min").is_none());

        // More jewel sockets is better, so that one keeps its floor.
        let sockets = by_id("enchant.stat_4079888060");
        assert_eq!(sockets["value"]["min"], 1.0);
        assert!(sockets["value"].get("max").is_none());
    }

    #[test]
    fn fractional_rolls_keep_a_tenth_of_precision() {
        let item = ParsedItem {
            base_type: "Large Cluster Jewel".to_string(),
            rarity: Some(Rarity::Magic),
            mods: vec![mod_with(
                "Added Small Passive Skills also grant: Regenerate #% of Life per Second",
                "explicit.stat_3721672021",
                Some(0.1),
            )],
            ..Default::default()
        };
        let body = build_search_body(&item, &default_filters(&item), &MiscFilters::default());
        // Flooring to a whole number would drop this roll out of the search.
        assert_eq!(body["query"]["stats"][0]["filters"][0]["value"]["min"], 0.1);
    }

    #[test]
    fn item_level_bounds_ride_along_with_the_other_misc_filters() {
        let item = ParsedItem {
            base_type: "Large Cluster Jewel".to_string(),
            rarity: Some(Rarity::Magic),
            ..Default::default()
        };

        let both = build_search_body(
            &item,
            &[],
            &MiscFilters {
                ilvl_min: Some(77),
                ilvl_max: Some(84),
                corrupted: Some(false),
                ..Default::default()
            },
        );
        let misc = &both["query"]["filters"]["misc_filters"]["filters"];
        assert_eq!(misc["ilvl"]["min"], 77);
        assert_eq!(misc["ilvl"]["max"], 84);
        // Item level shares the misc_filters group with corrupted rather than
        // replacing it.
        assert_eq!(misc["corrupted"]["option"], "false");

        // A floor alone must not imply a ceiling of zero.
        let floor_only = build_search_body(
            &item,
            &[],
            &MiscFilters {
                ilvl_min: Some(77),
                ..Default::default()
            },
        );
        let ilvl = &floor_only["query"]["filters"]["misc_filters"]["filters"]["ilvl"];
        assert_eq!(ilvl["min"], 77);
        assert!(ilvl.get("max").is_none());

        // Neither bound set leaves the group off entirely.
        let neither = build_search_body(&item, &[], &MiscFilters::default());
        assert!(neither["query"].get("filters").is_none());
    }

    #[test]
    fn rows_sharing_a_stat_id_collapse_to_the_narrowest_bound() {
        // A ring whose flat life is searched as its pseudo total, alongside the
        // folded total-life row: both land on pseudo.pseudo_total_life.
        let mut flat = mod_with("+# to maximum Life", "explicit.stat_3299347043", Some(105.0));
        flat.pseudo_ids = vec!["pseudo.pseudo_total_life".to_string()];
        let mut folded = mod_with("+# total maximum Life", "pseudo.pseudo_total_life", Some(125.0));
        folded.mod_type = ModType::Pseudo;
        let item = ParsedItem {
            base_type: "Opal Ring".to_string(),
            rarity: Some(Rarity::Rare),
            mods: vec![flat, folded],
            ..Default::default()
        };

        let both_on = FilterSpec {
            enabled: true,
            use_pseudo: true,
            ..Default::default()
        };
        let body = build_search_body(
            &item,
            &[
                FilterSpec { min: Some(105.0), ..both_on },
                FilterSpec { min: Some(125.0), ..both_on },
            ],
            &MiscFilters::default(),
        );

        let filters = body["query"]["stats"][0]["filters"].as_array().unwrap();
        assert_eq!(filters.len(), 1, "one id, one filter: {filters:?}");
        assert_eq!(filters[0]["id"], "pseudo.pseudo_total_life");
        // The trade site would apply 125 anyway; say so instead of sending both.
        assert_eq!(filters[0]["value"]["min"], 125.0);
        assert_eq!(filters[0]["disabled"], false);
    }

    #[test]
    fn merging_keeps_a_disabled_row_from_masking_an_enabled_one() {
        let mut flat = mod_with("+# to maximum Life", "explicit.stat_3299347043", Some(105.0));
        flat.pseudo_ids = vec!["pseudo.pseudo_total_life".to_string()];
        let mut folded = mod_with("+# total maximum Life", "pseudo.pseudo_total_life", Some(125.0));
        folded.mod_type = ModType::Pseudo;
        let item = ParsedItem {
            base_type: "Opal Ring".to_string(),
            rarity: Some(Rarity::Rare),
            mods: vec![flat, folded],
            ..Default::default()
        };

        // The subsumed flat row comes first and is off; the fold is on.
        let body = build_search_body(
            &item,
            &[
                FilterSpec { enabled: false, use_pseudo: true, ..Default::default() },
                FilterSpec {
                    enabled: true,
                    use_pseudo: true,
                    min: Some(125.0),
                    ..Default::default()
                },
            ],
            &MiscFilters::default(),
        );

        let filters = body["query"]["stats"][0]["filters"].as_array().unwrap();
        assert_eq!(filters.len(), 1);
        assert_eq!(filters[0]["disabled"], false);
        assert_eq!(filters[0]["value"]["min"], 125.0);
    }

    #[test]
    fn a_429_records_the_penalty_and_reports_the_wait() {
        use ureq::http::{HeaderMap, HeaderValue};

        let path = std::env::temp_dir().join("poechk-ratelimit-note429.json");
        let _ = std::fs::remove_file(&path);
        let limiter = RateLimiter::at(path);

        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("300"));

        // Anything that is not a 429 is left to the caller's normal handling.
        for ok in [200, 400, 404, 503] {
            assert!(note_rate_limit("search", ok, &headers, &limiter).is_ok(), "{ok}");
        }

        let err = note_rate_limit("search", 429, &headers, &limiter)
            .unwrap_err()
            .to_string();
        assert!(err.contains("300s"), "should name the wait: {err:?}");

        // The penalty was persisted, so the next attempt is refused up front
        // instead of spending a request and deepening it.
        let blocked = limiter.wait("search", 0.0).unwrap_err().to_string();
        assert!(blocked.contains("still rate limited"), "got {blocked:?}");
    }

    #[test]
    fn a_429_without_retry_after_falls_back_to_a_minute() {
        use ureq::http::HeaderMap;

        let path = std::env::temp_dir().join("poechk-ratelimit-note429-bare.json");
        let _ = std::fs::remove_file(&path);
        let limiter = RateLimiter::at(path);

        let err = note_rate_limit("fetch", 429, &HeaderMap::new(), &limiter)
            .unwrap_err()
            .to_string();
        assert!(err.contains("60s"), "expected the default penalty: {err:?}");
    }

    #[test]
    fn retry_after_is_read_when_present_and_sane() {
        use ureq::http::{HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("60"));
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(60)));

        // A 429 need not carry the header; the caller falls back.
        assert_eq!(retry_after(&HeaderMap::new()), None);

        // Garbage and non-positive values are ignored rather than trusted.
        for bogus in ["soon", "", "-5", "0"] {
            let mut headers = HeaderMap::new();
            headers.insert("retry-after", HeaderValue::from_str(bogus).unwrap());
            assert_eq!(retry_after(&headers), None, "for {bogus:?}");
        }
    }

    #[test]
    fn rate_limit_headers_parse_into_sliding_windows() {
        use ureq::http::{HeaderMap, HeaderValue};

        let mut headers = HeaderMap::new();
        headers.insert("x-rate-limit-rules", HeaderValue::from_static("Ip"));
        // The live trade-search policy, verbatim.
        headers.insert(
            "x-rate-limit-ip",
            HeaderValue::from_static("5:10:60,15:60:300,30:300:1800,600:21600:3600"),
        );

        // The third field is GGG's penalty for a violation, not a limit.
        assert_eq!(
            buckets_from_headers(&headers),
            vec![
                Bucket { max: 5, window: 10.0 },
                Bucket { max: 15, window: 60.0 },
                Bucket { max: 30, window: 300.0 },
                Bucket { max: 600, window: 21600.0 },
            ]
        );

        // A response with no rate-limit headers imposes no limits.
        assert!(buckets_from_headers(&HeaderMap::new()).is_empty());

        // A rule with no matching header is skipped rather than panicking.
        let mut partial = HeaderMap::new();
        partial.insert("x-rate-limit-rules", HeaderValue::from_static("Ip,Account"));
        partial.insert("x-rate-limit-ip", HeaderValue::from_static("5:10:60"));
        assert_eq!(
            buckets_from_headers(&partial),
            vec![Bucket { max: 5, window: 10.0 }]
        );
    }

    #[test]
    fn status_corrupted_filters_and_trade_url() {
        let item = ParsedItem {
            base_type: "Seaglass Amulet".to_string(),
            ..Default::default()
        };
        let body = build_search_body(
            &item,
            &[],
            &MiscFilters {
                corrupted: Some(true),
                status: Status::Any,
                ..Default::default()
            },
        );
        assert_eq!(body["query"]["status"]["option"], "any");
        assert_eq!(
            body["query"]["filters"]["misc_filters"]["filters"]["corrupted"]["option"],
            "true"
        );

        // Default is Instant Buyout (status "securable"), no corrupted filter.
        let plain = build_search_body(&item, &[], &MiscFilters::default());
        assert_eq!(plain["query"]["status"]["option"], "securable");
        assert!(plain["query"].get("filters").is_none());

        assert_eq!(
            search_url("Standard", "abc123"),
            "https://www.pathofexile.com/trade/search/Standard/abc123"
        );
    }

    #[test]
    fn instant_buyout_asks_for_securable_listings_only() {
        let item = ParsedItem {
            base_type: "Seaglass Amulet".to_string(),
            ..Default::default()
        };
        let status_of = |status| {
            let misc = MiscFilters { status, ..Default::default() };
            build_search_body(&item, &[], &misc)["query"]["status"]["option"].clone()
        };

        // "available" is online-and-priced, which still returns in-person
        // trades; the instant tier is "securable".
        assert_eq!(status_of(Status::InstantBuyout), "securable");
        assert_eq!(status_of(Status::Online), "online");
        assert_eq!(status_of(Status::Any), "any");

        assert_eq!(Status::InstantBuyout.next(), Status::Online);
        assert_eq!(Status::Online.next(), Status::Any);
        assert_eq!(Status::Any.next(), Status::InstantBuyout);
    }

    /// Both endpoints read their body to text (so the check log can keep it
    /// verbatim) and parse from that, so the wire shapes are worth pinning.
    /// These payloads are trimmed from real responses.
    #[test]
    fn a_search_response_parses_from_the_wire_shape() {
        let raw = r#"{"id":"8rRdMpqvUV","complexity":13,
            "result":["18087588112","e2370ad6493"],"total":2}"#;
        let parsed: SearchResponse = serde_json::from_str(raw).unwrap();

        assert_eq!(parsed.id, "8rRdMpqvUV");
        assert_eq!(parsed.total, 2);
        assert_eq!(parsed.result, ["18087588112", "e2370ad6493"]);
    }

    #[test]
    fn a_fetch_response_keeps_unpriced_and_expired_listings_in_place() {
        let raw = r#"{"result":[
            {"id":"a","listing":{"method":"psapi","indexed":"2026-07-30T18:02:11Z",
                "account":{"name":"Seller","online":null,"realm":"pc"},
                "price":{"type":"~b/o","amount":1,"currency":"divine"},
                "whisper":"@Seller hi","stash":{"name":"~b/o 1 divine"}}},
            null,
            {"id":"c","listing":{"method":"psapi",
                "account":{"name":"Other","realm":"pc"},"price":null}}
        ]}"#;
        let parsed: FetchResponse = serde_json::from_str(raw).unwrap();

        assert_eq!(parsed.result.len(), 3);
        let first = parsed.result[0].as_ref().unwrap();
        assert_eq!(first.listing.price.as_ref().unwrap().amount, 1.0);
        assert_eq!(first.listing.price.as_ref().unwrap().currency, "divine");
        assert_eq!(first.listing.account.name, "Seller");
        assert_eq!(first.listing.indexed.as_deref(), Some("2026-07-30T18:02:11Z"));
        // An expired listing comes back as null, and a listing can carry no
        // price at all; both must survive so the positions still line up.
        assert!(parsed.result[1].is_none());
        assert!(parsed.result[2].as_ref().unwrap().listing.price.is_none());
    }
}
