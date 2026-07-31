//! Regenerate `data/poe1/en/tiers.ndjson`, the affix tier ladder.
//!
//! ```sh
//! cargo run --release --example vendor-tiers
//! ```
//!
//! The advanced clipboard format annotates a roll with its own tier's range
//! (`Adds 91(81-111) to …`) but says nothing about the tiers above or below it,
//! and the standard format annotates nothing at all. This derives the whole
//! ladder from RePoE's mod export, so a check can place a roll among its tiers
//! rather than only inside its own.
//!
//! RePoE keys mods by an internal id, so the ladder is joined to the vendored
//! APT stat data by mod text: RePoE's `Adds (81-111) to (163-189) Cold Damage`
//! templates to the same `Adds # to # Cold Damage` an APT matcher carries. The
//! join is reported below; text that matches nothing is dropped rather than
//! guessed at.
//!
//! Run it after the APT snapshot is refreshed — both sides describe the same
//! patch, and a ladder keyed on stat text that APT has since reworded is a
//! ladder that silently stops resolving.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

const MODS_URL: &str = "https://repoe-fork.github.io/mods.min.json";
const STATS: &str = "data/poe1/en/stats.ndjson";
const OUT: &str = "data/poe1/en/tiers.ndjson";

/// RePoE writes a rolled value as its range alone, `(81-111)`, where the
/// clipboard writes the roll first. Both collapse to `#`.
static REPOE_RANGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\([+-]?[\d.]+-[+-]?[\d.]+\)").unwrap());

/// A value with no range beside it — a mod whose roll is fixed.
static BARE_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|(?P<pre>[^\w#]))[+-]?\d+(?:\.\d+)?(?:$|(?P<post>[^\w]))")
        .unwrap());

/// One tier of one affix on one kind of item.
#[derive(Debug, Clone)]
struct Tier {
    name: String,
    level: u64,
    /// The tier's bounds, averaged across the mod's values the way a parsed
    /// roll is, so they compare directly against `ParsedMod::tier_range`.
    min: f64,
    max: f64,
}

fn main() -> anyhow::Result<()> {
    let refs = apt_stat_refs()?;
    eprintln!("{} stat refs from {STATS}", refs.len());

    eprintln!("fetching {MODS_URL} …");
    let mods: BTreeMap<String, Value> = ureq::get(MODS_URL)
        .header("User-Agent", "poechk vendor-tiers")
        .call()?
        .body_mut()
        .with_config()
        .limit(128 * 1024 * 1024)
        .read_json()?;
    eprintln!("{} mods", mods.len());

    let mut ladders: BTreeMap<(String, String), Vec<Tier>> = BTreeMap::new();
    let (mut joined, mut multiline, mut unmatched) = (0u32, 0u32, BTreeSet::new());
    for value in mods.values() {
        let Some((text, tier)) = rollable_affix(value) else {
            continue;
        };
        // A mod granting two stats prints two lines, and which of its values
        // belong to which line is not recorded. Averaging across all of them
        // would bound each line by the other's rolls, so skip it rather than
        // publish a range that is wrong.
        if text.contains('\n') {
            multiline += 1;
            continue;
        }
        let Some(stat_ref) = refs.get(&templatize(&text)) else {
            unmatched.insert(text);
            continue;
        };
        joined += 1;
        for tag in spawn_tags(value) {
            ladders
                .entry((stat_ref.clone(), tag))
                .or_default()
                .push(tier.clone());
        }
    }
    eprintln!(
        "{joined} affixes joined, {multiline} skipped as multi-line, {} unmatched",
        unmatched.len()
    );
    for text in unmatched.iter().take(5) {
        eprintln!("  no APT stat for: {text}");
    }

    let mut out = String::new();
    for ((stat_ref, tag), mut tiers) in ladders {
        // Highest required level is tier 1, the way the game and the trade
        // site number them.
        tiers.sort_by(|a, b| b.level.cmp(&a.level).then(a.name.cmp(&b.name)));
        let listed: Vec<Value> = tiers
            .iter()
            .enumerate()
            .map(|(i, t)| {
                serde_json::json!({
                    "tier": i + 1,
                    "name": t.name,
                    "level": t.level,
                    "min": t.min,
                    "max": t.max,
                })
            })
            .collect();
        let line = serde_json::json!({ "ref": stat_ref, "tag": tag, "tiers": listed });
        out.push_str(&serde_json::to_string(&line)?);
        out.push('\n');
    }
    std::fs::write(OUT, &out)?;
    eprintln!("wrote {OUT} ({} lines, {} KiB)", out.lines().count(), out.len() / 1024);
    Ok(())
}

/// Every stat text the vendored APT data can resolve, mapped from its
/// templated form to the canonical ref a parsed mod carries.
///
/// Matchers and select-group members are collected too: a quarter of the
/// ladder joins through those rather than through a top-level `ref`.
fn apt_stat_refs() -> anyhow::Result<BTreeMap<String, String>> {
    let text = std::fs::read_to_string(STATS)?;
    let mut refs = BTreeMap::new();
    for line in text.lines() {
        collect_refs(&serde_json::from_str(line)?, &mut refs);
    }
    Ok(refs)
}

fn collect_refs(stat: &Value, refs: &mut BTreeMap<String, String>) {
    if let Some(stat_ref) = stat["ref"].as_str() {
        refs.entry(templatize(stat_ref)).or_insert_with(|| stat_ref.to_string());
        for matcher in stat["matchers"].as_array().into_iter().flatten() {
            if let Some(string) = matcher["string"].as_str() {
                refs.entry(templatize(string)).or_insert_with(|| stat_ref.to_string());
            }
        }
    }
    for inner in stat["stats"].as_array().into_iter().flatten() {
        collect_refs(inner, refs);
    }
}

/// The mod's text and tier, or `None` when it is not an affix that rolls on an
/// item — implicits, corrupted mods, and enchantments have no tier ladder.
fn rollable_affix(value: &Value) -> Option<(String, Tier)> {
    if value["domain"].as_str()? != "item" {
        return None;
    }
    if !matches!(value["generation_type"].as_str()?, "prefix" | "suffix") {
        return None;
    }
    let stats = value["stats"].as_array().filter(|s| !s.is_empty())?;
    // Averaged across the mod's values, matching how a roll is averaged.
    let count = stats.len() as f64;
    let mut min = 0.0;
    let mut max = 0.0;
    for stat in stats {
        min += stat["min"].as_f64()?;
        max += stat["max"].as_f64()?;
    }
    Some((
        value["text"].as_str()?.to_string(),
        Tier {
            name: value["name"].as_str()?.to_string(),
            level: value["required_level"].as_u64().unwrap_or(0),
            min: min / count,
            max: max / count,
        },
    ))
}

/// The item tags this mod can actually roll on. `default` is the catch-all
/// weight and names no item class, so it is not a tag anything looks up by.
fn spawn_tags(value: &Value) -> Vec<String> {
    value["spawn_weights"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["weight"].as_u64().unwrap_or(0) > 0)
        .filter_map(|w| w["tag"].as_str())
        .filter(|tag| *tag != "default")
        .map(str::to_string)
        .collect()
}

/// Reduce mod text to the shape both sides share: every rolled value as `#`.
fn templatize(text: &str) -> String {
    let ranged = REPOE_RANGE.replace_all(text, "#");
    BARE_NUMBER
        .replace_all(&ranged, |caps: &regex::Captures| {
            format!(
                "{}#{}",
                caps.name("pre").map_or("", |m| m.as_str()),
                caps.name("post").map_or("", |m| m.as_str())
            )
        })
        .into_owned()
}
