//! Regenerate `data/poe1/en/tiers.ndjson`, the affix tier ladder.
//!
//! ```sh
//! cargo run --release --example vendor-tiers
//! ```
//!
//! The advanced clipboard format annotates a roll with its own tier's range
//! (`Adds 91(81-111) to …`) but says nothing about the tiers above or below it,
//! and the standard format annotates nothing at all. This derives the whole
//! ladder from RePoE's export, so a check can place a roll among its tiers
//! rather than only inside its own.
//!
//! Which affixes an item can roll is decided the way the game decides it. A
//! mod lists spawn weights against item tags **in order**, and the first tag
//! the item carries wins — so `Crystalising`, whose weights open with
//! `two_hand_weapon: 0` before `sword: 108`, is unavailable to a two-handed
//! sword however much weight `sword` carries. Reading the list as an unordered
//! set instead reports tiers an item cannot roll.
//!
//! Output is keyed by the category and influence a parsed item already knows,
//! so nothing downstream has to hold a table of RePoE's tag vocabulary. A
//! category's ladder is the union over its base types: a lookup only ever asks
//! about a mod the item in hand already has, so including one that some other
//! base in the class could roll costs nothing.
//!
//! Run it after the APT snapshot is refreshed — the join is by stat text, and a
//! ladder keyed on text APT has since reworded silently stops resolving.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

const MODS_URL: &str = "https://repoe-fork.github.io/mods.min.json";
const BASES_URL: &str = "https://repoe-fork.github.io/base_items.min.json";
const STATS: &str = "data/poe1/en/stats.ndjson";
const ITEMS: &str = "data/poe1/en/items.ndjson";
const OUT: &str = "data/poe1/en/tiers.ndjson";

/// The influences that add prefixes and suffixes, and the tag suffix each one
/// grants. Searing Exarch and Eater of Worlds are absent on purpose: they add
/// implicits, which do not roll from the affix pool this ladder describes.
const INFLUENCES: [(&str, &str); 6] = [
    ("Shaper", "shaper"),
    ("Elder", "elder"),
    ("Crusader", "crusader"),
    ("Hunter", "basilisk"),
    ("Redeemer", "eyrie"),
    ("Warlord", "adjudicator"),
];

/// RePoE writes a rolled value as its range alone, `(81-111)`, where the
/// clipboard writes the roll first. Both collapse to `#`.
static REPOE_RANGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\([+-]?[\d.]+-[+-]?[\d.]+\)").unwrap());

/// A value with no range beside it — a mod whose roll is fixed.
static BARE_NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|(?P<pre>[^\w#]))[+-]?\d+(?:\.\d+)?(?:$|(?P<post>[^\w]))").unwrap()
});

/// One tier of one affix.
#[derive(Debug, Clone)]
struct Tier {
    name: String,
    level: u64,
    /// The tier's bounds, averaged across the mod's values the way a parsed
    /// roll is, so they compare directly against `ParsedMod::tier_range`.
    min: f64,
    max: f64,
}

/// A mod as the ladder needs it: what it reads as, what it rolls, and the
/// ordered weights that decide which items get it.
struct Affix {
    stat_ref: String,
    tier: Tier,
    weights: Vec<(String, u64)>,
}

fn main() -> anyhow::Result<()> {
    let refs = apt_stat_refs()?;
    let categories = apt_categories()?;
    eprintln!("{} stat refs, {} base types from the APT snapshot", refs.len(), categories.len());

    let mods: BTreeMap<String, Value> = fetch(MODS_URL)?;
    let bases: BTreeMap<String, Value> = fetch(BASES_URL)?;
    eprintln!("{} mods, {} base items from RePoE", mods.len(), bases.len());

    let affixes = affixes(&mods, &refs);
    let by_category = tag_sets_by_category(&bases, &categories);
    eprintln!(
        "{} affixes joined, {} categories with known tags",
        affixes.len(),
        by_category.len()
    );

    let mut ladders: BTreeMap<(String, String, String), Vec<Tier>> = BTreeMap::new();
    for (category, base_tags) in &by_category {
        for (influence, suffix) in std::iter::once(("", "")).chain(INFLUENCES.map(|(i, s)| (i, s)))
        {
            let tag_sets: Vec<HashSet<String>> =
                base_tags.iter().map(|tags| influenced(tags, suffix)).collect();
            for affix in &affixes {
                if !tag_sets.iter().any(|tags| rolls_on(&affix.weights, tags)) {
                    continue;
                }
                // An influenced ladder lists only what the influence adds; the
                // rest is already in the uninfluenced one. Compared against the
                // base tags, not `tag_sets` — those already carry the influence,
                // so every affix would look like it needed none.
                if !influence.is_empty()
                    && base_tags.iter().any(|tags| rolls_on(&affix.weights, tags))
                {
                    continue;
                }
                ladders
                    .entry((affix.stat_ref.clone(), category.clone(), influence.to_string()))
                    .or_default()
                    .push(affix.tier.clone());
            }
        }
    }

    let out = render(ladders)?;
    std::fs::write(OUT, &out)?;
    eprintln!("wrote {OUT} ({} lines, {} KiB)", out.lines().count(), out.len() / 1024);
    Ok(())
}

/// Whether an item carrying `tags` can roll a mod with these ordered weights.
///
/// The game reads the list in order and stops at the first tag the item has,
/// so a zero weight early in the list is an exclusion, not a lack of weight.
fn rolls_on(weights: &[(String, u64)], tags: &HashSet<String>) -> bool {
    weights
        .iter()
        .find(|(tag, _)| tags.contains(tag))
        .is_some_and(|(_, weight)| *weight > 0)
}

/// A base's tags plus the ones an influence grants, e.g. `dagger_basilisk`.
fn influenced(tags: &HashSet<String>, suffix: &str) -> HashSet<String> {
    if suffix.is_empty() {
        return tags.clone();
    }
    tags.iter()
        .map(|t| format!("{t}_{suffix}"))
        .chain(tags.iter().cloned())
        .collect()
}

/// Every rollable affix, joined to the stat ref a parsed mod carries.
///
/// A mod granting two stats prints one line each, and the printed lines are
/// split into separate mods by the parser, so each needs its own ladder.
fn affixes(mods: &BTreeMap<String, Value>, refs: &BTreeMap<String, String>) -> Vec<Affix> {
    let (mut unsplittable, mut unmatched) = (0u32, BTreeSet::new());
    let mut out = Vec::new();
    for value in mods.values() {
        let Some(lines) = rollable_affix(value) else {
            unsplittable += 1;
            continue;
        };
        let weights = weights(value);
        for (text, tier) in lines {
            let Some(stat_ref) = refs.get(&templatize(&text)) else {
                unmatched.insert(text);
                continue;
            };
            out.push(Affix {
                stat_ref: stat_ref.clone(),
                tier,
                weights: weights.clone(),
            });
        }
    }
    eprintln!(
        "  {unsplittable} not item affixes or unattributable, {} unmatched",
        unmatched.len()
    );
    for text in unmatched.iter().take(3) {
        eprintln!("    no APT stat for: {text}");
    }
    out
}

/// Each poechk category's base types, as tag sets.
fn tag_sets_by_category(
    bases: &BTreeMap<String, Value>,
    categories: &HashMap<String, String>,
) -> BTreeMap<String, Vec<HashSet<String>>> {
    let mut out: BTreeMap<String, Vec<HashSet<String>>> = BTreeMap::new();
    for base in bases.values() {
        let (Some(name), Some("item")) = (base["name"].as_str(), base["domain"].as_str()) else {
            continue;
        };
        let Some(category) = categories.get(name) else {
            continue;
        };
        let tags: HashSet<String> = base["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_str())
            .map(str::to_string)
            .collect();
        if !tags.is_empty() {
            out.entry(category.clone()).or_default().push(tags);
        }
    }
    out
}

/// Serialise the ladders, numbering tiers highest-level-first.
fn render(ladders: BTreeMap<(String, String, String), Vec<Tier>>) -> anyhow::Result<String> {
    let mut out = String::new();
    for ((stat_ref, category, influence), mut tiers) in ladders {
        tiers.sort_by(|a, b| b.level.cmp(&a.level).then(a.name.cmp(&b.name)));
        tiers.dedup_by(|a, b| a.name == b.name && a.level == b.level);
        let listed: Vec<Value> = tiers
            .iter()
            .enumerate()
            .map(|(i, t)| {
                serde_json::json!({
                    "tier": i + 1, "name": t.name, "level": t.level, "min": t.min, "max": t.max,
                })
            })
            .collect();
        let line = serde_json::json!({
            "ref": stat_ref,
            "category": category,
            "influence": (!influence.is_empty()).then_some(influence),
            "tiers": listed,
        });
        out.push_str(&serde_json::to_string(&line)?);
        out.push('\n');
    }
    Ok(out)
}

fn fetch<T: serde::de::DeserializeOwned>(url: &str) -> anyhow::Result<T> {
    eprintln!("fetching {url} …");
    Ok(ureq::get(url)
        .header("User-Agent", "poechk vendor-tiers")
        .call()?
        .body_mut()
        .with_config()
        .limit(128 * 1024 * 1024)
        .read_json()?)
}

/// Every stat text the vendored APT data can resolve, from its templated form
/// to the canonical ref a parsed mod carries. Matchers and select-group members
/// are collected too: much of the ladder joins through those.
fn apt_stat_refs() -> anyhow::Result<BTreeMap<String, String>> {
    let mut refs = BTreeMap::new();
    for line in std::fs::read_to_string(STATS)?.lines() {
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

/// Base type to the category a parsed item reports, from the APT snapshot, so
/// both sides of the ladder speak poechk's vocabulary.
fn apt_categories() -> anyhow::Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for line in std::fs::read_to_string(ITEMS)?.lines() {
        let item: Value = serde_json::from_str(line)?;
        if let (Some(name), Some(category)) =
            (item["name"].as_str(), item["craftable"]["category"].as_str())
        {
            out.insert(name.to_string(), category.to_string());
        }
    }
    Ok(out)
}

/// Each printed line of a rollable affix with the tier that line rolls, or
/// `None` when it is not an item affix or its values cannot be attributed.
///
/// Counts only genuine misattributions in the caller's tally by returning
/// `None` for both, so the reported figure is read with that in mind.
///
/// A mod grants its stats in the order its lines print them, and a line's share
/// is as many stats as it has rolled values: `Adds (5-7) to (11-13) Chaos
/// Damage` takes two, the `Penetrate (14-16)%` line beneath it takes one. When
/// those counts do not add up to the stats the mod declares, the attribution is
/// a guess and the whole mod is dropped — a range bounded by another line's
/// rolls is worse than no range.
fn rollable_affix(value: &Value) -> Option<Vec<(String, Tier)>> {
    // `unveiled` is the Betrayal pool, which rolls onto items as ordinary
    // prefixes and suffixes once unveiled and so has the same tier ladder.
    if !matches!(value["domain"].as_str()?, "item" | "unveiled") {
        return None;
    }
    if !matches!(value["generation_type"].as_str()?, "prefix" | "suffix") {
        return None;
    }
    let stats = value["stats"].as_array().filter(|s| !s.is_empty())?;
    let name = value["name"].as_str()?;
    let level = value["required_level"].as_u64().unwrap_or(0);

    let lines: Vec<&str> = value["text"].as_str()?.lines().collect();
    let per_line: Vec<usize> = lines.iter().map(|l| REPOE_RANGE.find_iter(l).count()).collect();
    if per_line.iter().sum::<usize>() != stats.len() {
        return None;
    }

    let mut out = Vec::new();
    let mut taken = 0usize;
    for (line, count) in lines.iter().zip(&per_line) {
        // A line with no rolled value has no range to bound; it still prints,
        // but nothing about it is tiered.
        if *count == 0 {
            continue;
        }
        let share = &stats[taken..taken + count];
        taken += count;
        // Averaged across the line's own values, the way a parsed roll is.
        let mut min = 0.0;
        let mut max = 0.0;
        for stat in share {
            min += stat["min"].as_f64()?;
            max += stat["max"].as_f64()?;
        }
        let n = *count as f64;
        out.push((
            (*line).to_string(),
            Tier { name: name.to_string(), level, min: min / n, max: max / n },
        ));
    }
    Some(out)
}

/// The mod's spawn weights, order preserved — the order is the rule.
fn weights(value: &Value) -> Vec<(String, u64)> {
    value["spawn_weights"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| Some((w["tag"].as_str()?.to_string(), w["weight"].as_u64()?)))
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
