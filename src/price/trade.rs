//! Official pathofexile.com trade API: real listings for any item.
//!
//! The flow is `build_search_body` → POST `/api/trade/search/{league}` →
//! GET `/api/trade/fetch/{ids}?query={id}` (<=10 ids) → listing prices. Requests
//! go through a file-based rate limiter (see `ratelimit`) that honours GGG's
//! `x-rate-limit-*` headers across `check` processes. The HTTP client and rate
//! limiter land next; this file currently builds the request and models the
//! responses.

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
pub fn build_search_body(item: &ParsedItem) -> Value {
    // A unique is identified by its name, and its affix rolls vary widely, so
    // filtering on them returns nothing useful — leave them disabled by default
    // (the user enables/tightens specific ones in the interactive overlay, M4).
    let is_unique = item.rarity == Some(Rarity::Unique);
    let filters: Vec<Value> = item
        .mods
        .iter()
        .filter_map(|m| stat_filter(m, is_unique))
        .collect();

    // "any" (not "online") for valuation: fairly-priced sellers are often
    // offline, so online-only skews high. The online/any toggle lands in M4.
    let mut query = json!({
        "status": { "option": "any" },
        "type": item.base_type,
        "stats": [ { "type": "and", "filters": filters } ],
    });
    if is_unique
        && let Some(name) = &item.name
    {
        query["name"] = json!(name);
    }

    json!({ "query": query, "sort": { "price": "asc" } })
}

/// One trade stat filter for a resolved mod, or `None` if it has no trade id.
fn stat_filter(m: &crate::item::mods::ParsedMod, disabled: bool) -> Option<Value> {
    let id = m.trade_ids.first()?;
    let mut filter = json!({ "id": id, "disabled": disabled });
    if !disabled
        && let Some(roll) = m.roll()
    {
        filter["value"] = json!({ "min": roll.floor() });
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
    /// (cheapest-first).
    pub fn price(&self, item: &ParsedItem) -> anyhow::Result<(u32, Vec<PriceQuote>)> {
        let body = build_search_body(item);
        let limiter = RateLimiter::open()?;

        limiter.wait("search");
        let search = self.search(&body, &limiter)?;
        if search.result.is_empty() {
            return Ok((search.total, Vec::new()));
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
        Ok((search.total, quotes))
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
            template: stat_ref.to_string(),
            rolls: roll.map(|r| vec![r]).unwrap_or_default(),
            stat_ref: stat_ref.to_string(),
            trade_ids: vec![id.to_string()],
        }
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

        let body = build_search_body(&item);

        assert_eq!(body["query"]["type"], "Jewelled Foil");
        assert_eq!(body["query"]["status"]["option"], "any");
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
        let body = build_search_body(&item);
        assert_eq!(body["query"]["name"], "Whispers of Infinity");
        assert_eq!(body["query"]["type"], "Seaglass Amulet");
        // Present but disabled, so the search returns every listing of the unique.
        let filters = body["query"]["stats"][0]["filters"].as_array().unwrap();
        assert_eq!(filters[0]["disabled"], true);
        assert!(filters[0].get("value").is_none());
    }
}
