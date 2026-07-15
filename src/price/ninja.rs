//! poe.ninja economy reference prices, cached on disk.
//!
//! One fetch of the dense overview yields a name -> chaos map for currency,
//! fragments, scarabs, essences, cards, and the rest of the economy. The map is
//! condensed to disk and refreshed after a TTL, matching how often poe.ninja
//! itself updates. Unique-item overviews are skipped: bulk lookups are by base
//! name and unique names could collide.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const USER_AGENT: &str = concat!(
    "poechk/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/jdoss/poechk)"
);

const CACHE_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Serialize, Deserialize)]
struct Cache {
    fetched_unix: u64,
    prices: HashMap<String, f64>,
}

/// The approximate chaos value of an item by its poe.ninja name, fetching or
/// refreshing the league's cached economy map as needed.
pub fn chaos_value(league: &str, name: &str) -> Option<f64> {
    match load(league) {
        Ok(prices) => prices.get(name).copied(),
        Err(e) => {
            tracing::warn!("poe.ninja lookup unavailable: {e}");
            None
        }
    }
}

fn load(league: &str) -> anyhow::Result<HashMap<String, f64>> {
    let path = cache_path(league)?;
    if let Some(cache) = read_cache(&path)
        && now_unix().saturating_sub(cache.fetched_unix) < CACHE_TTL.as_secs()
    {
        return Ok(cache.prices);
    }
    let prices = fetch(league)?;
    let cache = Cache {
        fetched_unix: now_unix(),
        prices,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = std::fs::write(&path, json);
    }
    Ok(cache.prices)
}

fn fetch(league: &str) -> anyhow::Result<HashMap<String, f64>> {
    let url = format!(
        "https://poe.ninja/poe1/api/economy/current/dense/overviews?league={}&language=en",
        league.replace(' ', "%20")
    );
    let mut resp = ureq::get(&url)
        .header("User-Agent", USER_AGENT)
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|e| anyhow::anyhow!("poe.ninja request failed: {e}"))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        anyhow::bail!(
            "poe.ninja rejected the request ({}) — it may not track league \"{league}\"",
            crate::price::api_error(status, resp.body_mut())
        );
    }
    let blob: Value = resp
        .body_mut()
        .with_config()
        .limit(64 * 1024 * 1024)
        .read_json()
        .map_err(|e| anyhow::anyhow!("parsing poe.ninja response: {e}"))?;
    Ok(condense(&blob))
}

/// Reduce the dense overview blob to a name -> chaos map.
fn condense(blob: &Value) -> HashMap<String, f64> {
    let mut prices = HashMap::new();
    for section in ["currencyOverviews", "itemOverviews"] {
        let Some(overviews) = blob[section].as_array() else {
            continue;
        };
        for overview in overviews {
            // Skip unique overviews: name collisions with base items.
            if overview["type"].as_str().is_some_and(|t| t.starts_with("Unique")) {
                continue;
            }
            let Some(lines) = overview["lines"].as_array() else {
                continue;
            };
            for line in lines {
                if let (Some(name), Some(chaos)) = (line["name"].as_str(), line["chaos"].as_f64())
                {
                    prices.entry(name.to_string()).or_insert(chaos);
                }
            }
        }
    }
    prices
}

fn cache_path(league: &str) -> anyhow::Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("io.github", "jdoss", "poechk")
        .context("could not locate a cache directory")?;
    let slug: String = league
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Ok(dirs.cache_dir().join(format!("ninja-{slug}.json")))
}

fn read_cache(path: &PathBuf) -> Option<Cache> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn condenses_overviews_and_skips_uniques() {
        let blob = serde_json::json!({
            "currencyOverviews": [
                { "type": "Currency", "lines": [
                    { "name": "Divine Orb", "chaos": 633.8 },
                    { "name": "Chaos Orb", "chaos": 1.0 }
                ]}
            ],
            "itemOverviews": [
                { "type": "Scarab", "lines": [
                    { "name": "Titanic Scarab", "chaos": 4.2 }
                ]},
                { "type": "UniqueWeapon", "lines": [
                    { "name": "Divine Orb", "chaos": 99999.0 }
                ]}
            ]
        });
        let prices = condense(&blob);
        assert_eq!(prices.get("Divine Orb"), Some(&633.8));
        assert_eq!(prices.get("Chaos Orb"), Some(&1.0));
        assert_eq!(prices.get("Titanic Scarab"), Some(&4.2));
        // The unique overview must not clobber the currency entry.
        assert_eq!(prices.len(), 3);
    }
}
