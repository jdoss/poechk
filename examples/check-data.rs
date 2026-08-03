//! Report how far the vendored stat data has drifted from the live trade API.
//!
//! ```sh
//! cargo run --release --example check-data
//! ```
//!
//! `stats.ndjson` is a snapshot of Awakened PoE Trade's data, refreshed by
//! hand. When a patch adds affixes the snapshot has not caught up with, those
//! mod lines silently fail to resolve and simply vanish from a search — the
//! symptom reads as a parsing bug rather than as stale data.
//!
//! The trade site publishes every stat it will accept, so comparing the two
//! turns that silence into a number.
//!
//! The number is not a verdict. Awakened PoE Trade curates rather than mirrors
//! — plenty of stats it omits on purpose will never appear here — so a large
//! count is normal and a count of zero is not the goal. What means something is
//! the count *moving*: `docs/UPDATING.md` records the figures for the snapshot
//! in tree, and a jump against those is a patch the snapshot has not caught up
//! with. Record the new figures whenever the snapshot is refreshed.

use std::collections::{BTreeMap, HashSet};

use serde_json::Value;

const TRADE_STATS: &str = "https://www.pathofexile.com/api/trade/data/stats";
const STATS: &str = "data/poe1/en/stats.ndjson";

const USER_AGENT: &str = concat!(
    "poechk/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/jdoss/poechk)"
);

/// The trade groups poechk parses mod lines into. Drift here is what matters:
/// a missing explicit is a mod that cannot be searched.
const PARSED_GROUPS: [&str; 5] = ["Explicit", "Implicit", "Fractured", "Crafted", "Enchant"];

/// Trade-site qualifiers appended to disambiguate stats that print the same
/// text, e.g. the local `+# to maximum Energy Shield (Local)` against the
/// global one. The snapshot separates those by item category instead, so the
/// suffix has to come off before the two sides can be compared at all.
static QUALIFIER: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"\s*\((?:Local|[A-Z][a-z]+)\)$").unwrap());

fn unqualified(text: &str) -> String {
    QUALIFIER.replace(text, "").into_owned()
}

/// Groups poechk does not parse, reported for information only. Several are
/// league mechanics the overlay has no filter for; `Pseudo` is synthesised
/// rather than matched against printed text.
const EXPECTED_DRIFT: &str =
    "Pseudo, Imbued, Scourge, Mercenary, Veiled, Delve, Ultimatum, Sanctum, Crucible";

fn main() -> anyhow::Result<()> {
    let vendored = vendored_texts()?;
    eprintln!("{} stat texts in {STATS}", vendored.len());

    eprintln!("fetching {TRADE_STATS} …");
    let live: Value = ureq::get(TRADE_STATS)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .call()?
        .body_mut()
        .with_config()
        .limit(32 * 1024 * 1024)
        .read_json()?;

    println!("\n{:<12} {:>7} {:>9}  source", "group", "live", "unmatched");
    let mut samples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for group in live["result"].as_array().into_iter().flatten() {
        let (Some(label), Some(entries)) = (group["label"].as_str(), group["entries"].as_array())
        else {
            continue;
        };
        let missing: Vec<&str> = entries
            .iter()
            .filter_map(|e| e["text"].as_str())
            .filter(|text| !vendored.contains(&unqualified(text)))
            .collect();
        let parsed = PARSED_GROUPS.contains(&label);
        if parsed && !missing.is_empty() {
            samples.insert(
                label.to_string(),
                missing.iter().take(3).map(|s| s.to_string()).collect(),
            );
        }
        println!(
            "{:<12} {:>7} {:>9}  {}",
            label,
            entries.len(),
            missing.len(),
            if parsed { "parsed" } else { "not parsed" }
        );
    }

    println!("\nSample of what the trade site accepts and the snapshot does not:");
    for (group, texts) in &samples {
        for text in texts {
            println!("  {group:<10} {}", text.lines().next().unwrap_or(text));
        }
    }
    println!("\nCompare the parsed groups against the figures in docs/UPDATING.md.");
    println!("A jump means a patch landed that the snapshot has not caught up with.");
    println!("Groups poechk does not parse [{EXPECTED_DRIFT}] drift on their own.");
    Ok(())
}

/// Every stat text the vendored snapshot can resolve: refs, their matchers,
/// and select-group members, which is what a printed mod line is matched to.
fn vendored_texts() -> anyhow::Result<HashSet<String>> {
    let mut texts = HashSet::new();
    for line in std::fs::read_to_string(STATS)?.lines() {
        collect(&serde_json::from_str(line)?, &mut texts);
    }
    Ok(texts)
}

fn collect(stat: &Value, texts: &mut HashSet<String>) {
    if let Some(stat_ref) = stat["ref"].as_str() {
        texts.insert(unqualified(stat_ref));
        for matcher in stat["matchers"].as_array().into_iter().flatten() {
            if let Some(string) = matcher["string"].as_str() {
                texts.insert(unqualified(string));
            }
        }
    }
    for inner in stat["stats"].as_array().into_iter().flatten() {
        collect(inner, texts);
    }
}
