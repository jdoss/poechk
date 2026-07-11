//! Official pathofexile.com trade API: real listings for any item.
//!
//! The flow is `build_search_body` → POST `/api/trade/search/{league}` →
//! GET `/api/trade/fetch/{ids}?query={id}` (<=10 ids) → listing prices. Requests
//! go through a file-based rate limiter (see `ratelimit`) that honours GGG's
//! `x-rate-limit-*` headers across `check` processes. The HTTP client and rate
//! limiter land next; this file currently builds the request and models the
//! responses.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Config;
use crate::item::{ParsedItem, Rarity};
use crate::price::PriceQuote;
use crate::price::ratelimit::RateLimiter;

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
    pub min: Option<f64>,
    pub max: Option<f64>,
}

/// The trade "status" filter — which sellers/listings to include.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Status {
    /// Buyable now: online seller with a buyout price (Awakened's default).
    #[default]
    InstantBuyout,
    /// Any online seller.
    Online,
    /// Everything, including offline listings.
    Any,
}

impl Status {
    /// The trade API `status.option` value.
    pub fn option(self) -> &'static str {
        match self {
            Status::InstantBuyout => "available",
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
    /// Minimum total socket count.
    pub sockets_min: Option<u8>,
    /// Minimum size of the largest linked group.
    pub links_min: Option<u8>,
}

/// The outcome of a price search: the search id (for the trade URL), the total
/// match count, and the cheapest listings.
#[derive(Debug, Clone)]
pub struct PriceResult {
    pub search_id: String,
    pub total: u32,
    pub quotes: Vec<PriceQuote>,
}

pub fn build_search_body(item: &ParsedItem, filters: &[FilterSpec], misc: &MiscFilters) -> Value {
    let stat_filters: Vec<Value> = item
        .mods
        .iter()
        .enumerate()
        .filter_map(|(i, m)| filters.get(i).and_then(|spec| stat_filter(m, spec)))
        .collect();

    // "any" (not "online") for valuation: fairly-priced sellers are often
    // offline, so online-only skews high. (Online/any toggle: later.)
    let mut query = json!({
        "status": { "option": misc.status.option() },
        "type": item.base_type,
        "stats": [ { "type": "and", "filters": stat_filters } ],
    });
    // Uniques are found by name; the base type alone is ambiguous.
    if item.rarity == Some(Rarity::Unique)
        && let Some(name) = &item.name
    {
        query["name"] = json!(name);
    }
    let mut filter_groups = serde_json::Map::new();
    if let Some(corrupted) = misc.corrupted {
        let option = if corrupted { "true" } else { "false" };
        filter_groups.insert(
            "misc_filters".to_string(),
            json!({ "filters": { "corrupted": { "option": option } } }),
        );
    }
    if misc.sockets_min.is_some() || misc.links_min.is_some() {
        let mut socket_filters = serde_json::Map::new();
        if let Some(sockets) = misc.sockets_min {
            socket_filters.insert("sockets".to_string(), json!({ "min": sockets }));
        }
        if let Some(links) = misc.links_min {
            socket_filters.insert("links".to_string(), json!({ "min": links }));
        }
        filter_groups.insert(
            "socket_filters".to_string(),
            json!({ "filters": Value::Object(socket_filters) }),
        );
    }
    if !filter_groups.is_empty() {
        query["filters"] = Value::Object(filter_groups);
    }

    json!({ "query": query, "sort": { "price": "asc" } })
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
/// their current roll (min = roll, no max). Crafted mods start disabled — any
/// buyer can re-craft them, so they shouldn't constrain the search.
pub fn default_filters(item: &ParsedItem) -> Vec<FilterSpec> {
    let enabled = item.rarity != Some(Rarity::Unique);
    item.mods
        .iter()
        .map(|m| FilterSpec {
            enabled: enabled && m.mod_type != crate::item::mods::ModType::Crafted,
            as_explicit: false,
            min: m.roll().map(f64::floor),
            max: None,
        })
        .collect()
}

/// One trade stat filter for a resolved mod, or `None` if it has no trade id.
fn stat_filter(m: &crate::item::mods::ParsedMod, spec: &FilterSpec) -> Option<Value> {
    let ids = if spec.as_explicit && !m.explicit_ids.is_empty() {
        &m.explicit_ids
    } else {
        &m.trade_ids
    };
    let id = ids.first()?;
    let mut filter = json!({ "id": id, "disabled": !spec.enabled });
    if spec.enabled {
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

/// Derive the safe minimum interval between requests from GGG's rate-limit
/// headers: for each advertised `max:window` bucket, `window / max`, taking the
/// most restrictive and adding a latency margin.
fn interval_from_headers(headers: &ureq::http::HeaderMap, margin: f64) -> Option<Duration> {
    let rules = headers.get("x-rate-limit-rules")?.to_str().ok()?;
    let mut interval = 0.0_f64;
    for rule in rules.split(',') {
        let key = format!("x-rate-limit-{}", rule.trim().to_ascii_lowercase());
        let Some(spec) = headers.get(&key).and_then(|v| v.to_str().ok()) else {
            continue;
        };
        for bucket in spec.split(',') {
            let mut parts = bucket.split(':');
            if let (Some(max), Some(window)) = (parts.next(), parts.next())
                && let (Ok(max), Ok(window)) = (max.parse::<f64>(), window.parse::<f64>())
                && max > 0.0
            {
                interval = interval.max(window / max);
            }
        }
    }
    (interval > 0.0).then(|| Duration::from_secs_f64(interval + margin))
}

/// Prices items against the official trade search + fetch endpoints.
#[derive(Debug)]
pub struct TradeSource {
    host: String,
    league: String,
    poesessid: Option<String>,
    latency_margin: f64,
}

impl TradeSource {
    /// Build a trade source from user config.
    pub fn new(cfg: &Config) -> Self {
        Self {
            host: "www.pathofexile.com".to_string(),
            league: cfg.league.clone(),
            poesessid: cfg.poesessid.clone(),
            latency_margin: cfg.api_latency_seconds,
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

        limiter.wait("search");
        let search = self.search(&body, &limiter)?;
        if search.result.is_empty() {
            return Ok(PriceResult {
                search_id: search.id,
                total: search.total,
                quotes: Vec::new(),
            });
        }
        let ids: Vec<String> = search.result.iter().take(FETCH_LIMIT).cloned().collect();

        limiter.wait("fetch");
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
            .collect();
        Ok(PriceResult {
            search_id: search.id,
            total: search.total,
            quotes,
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
        let mut resp = req
            .send_json(body)
            .map_err(|e| anyhow::anyhow!("trade search request failed: {e}"))?;
        if let Some(interval) = interval_from_headers(resp.headers(), self.latency_margin) {
            limiter.record("search", interval);
        }
        resp.body_mut()
            .read_json()
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
        let mut resp = req
            .call()
            .map_err(|e| anyhow::anyhow!("trade fetch request failed: {e}"))?;
        if let Some(interval) = interval_from_headers(resp.headers(), self.latency_margin) {
            limiter.record("fetch", interval);
        }
        let parsed: FetchResponse = resp
            .body_mut()
            .read_json()
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
            stat_ref: stat_ref.to_string(),
            trade_ids: vec![id.to_string()],
            explicit_ids: vec![id.to_string()],
        }
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
            &[FilterSpec { enabled: true, as_explicit: false, min: Some(80.0), max: None }],
            &MiscFilters::default(),
        );
        assert_eq!(
            typed["query"]["stats"][0]["filters"][0]["id"],
            "fractured.stat_3299347043"
        );

        let downgraded = build_search_body(
            &item,
            &[FilterSpec { enabled: true, as_explicit: true, min: Some(80.0), max: None }],
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
        assert_eq!(body["query"]["status"]["option"], "available");
        assert_eq!(body["sort"]["price"], "asc");
        assert!(body["query"].get("name").is_none());

        let filters = body["query"]["stats"][0]["filters"].as_array().unwrap();
        assert_eq!(filters.len(), 2);
        assert_eq!(filters[0]["id"], "explicit.stat_210067635");
        assert_eq!(filters[0]["disabled"], false);
        assert_eq!(filters[0]["value"]["min"], 30.0);
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

        // Default is Instant Buyout (status "available"), no corrupted filter.
        let plain = build_search_body(&item, &[], &MiscFilters::default());
        assert_eq!(plain["query"]["status"]["option"], "available");
        assert!(plain["query"].get("filters").is_none());

        assert_eq!(
            search_url("Standard", "abc123"),
            "https://www.pathofexile.com/trade/search/Standard/abc123"
        );
    }
}
