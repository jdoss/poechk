//! Loads the vendored PoE data (stat translations + base items) and indexes it.
//!
//! The data files are embedded at build time so an installed binary is
//! self-contained. Parsing ~11k JSON lines takes tens of milliseconds; the
//! daemon (M3) will keep the indexes warm, but `check` rebuilds them per run
//! for now.

use std::collections::HashMap;

use serde::Deserialize;

const STATS_NDJSON: &str = include_str!("../data/poe1/en/stats.ndjson");
const ITEMS_NDJSON: &str = include_str!("../data/poe1/en/items.ndjson");

// ---------------------------------------------------------------------------
// stats.ndjson
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct Matcher {
    string: String,
    #[serde(default)]
    advanced: Option<String>,
    #[serde(default)]
    negate: Option<bool>,
    #[serde(default)]
    value: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
struct Trade {
    #[serde(default)]
    ids: HashMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
struct Stat {
    #[serde(rename = "ref")]
    ref_: String,
    matchers: Vec<Matcher>,
    trade: Trade,
}

#[derive(Debug, Clone, Deserialize)]
struct Resolve {
    /// Parallel to `stats`: which item-category (set) each member applies to;
    /// `null` marks the default member.
    #[serde(default)]
    test: Option<Vec<Option<String>>>,
}

#[derive(Debug, Clone, Deserialize)]
struct StatGroup {
    #[serde(default)]
    resolve: Option<Resolve>,
    stats: Vec<Stat>,
}

/// A stats.ndjson line is either a single stat or a group of related stats.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum StatEntry {
    Single(Stat),
    Group(StatGroup),
}

/// A resolved stat: which trade stat-ids a matched mod line maps to.
#[derive(Debug, Clone)]
pub struct StatMatch {
    /// Canonical English reference text (`ref` in the data).
    pub stat_ref: String,
    /// Trade stat-ids keyed by mod type (explicit/implicit/fractured/…).
    pub trade_ids: HashMap<String, Vec<String>>,
    /// Fixed value for non-numeric matchers (e.g. "on Hit" == 100).
    pub value: Option<f64>,
    /// Whether the roll should be negated when comparing.
    pub negate: bool,
    /// For select-group members: the item-category (set) this variant applies
    /// to — e.g. the local form of "+# to Armour" carries `ARMOUR`. `None`
    /// marks the default/global variant.
    pub category_test: Option<String>,
}

/// Whether a select-group test matches an item category. Tests are either a
/// set name (WEAPON/ARMOUR/HEIST_EQUIPMENT) or a literal category.
pub fn category_matches(test: &str, category: &str) -> bool {
    const WEAPON: [&str; 15] = [
        "Bow",
        "Claw",
        "Dagger",
        "Rune Dagger",
        "One-Handed Axe",
        "One-Handed Mace",
        "One-Handed Sword",
        "Sceptre",
        "Staff",
        "Warstaff",
        "Two-Handed Axe",
        "Two-Handed Mace",
        "Two-Handed Sword",
        "Wand",
        "Fishing Rod",
    ];
    const ARMOUR: [&str; 5] = ["Body Armour", "Boots", "Gloves", "Helmet", "Shield"];
    const HEIST: [&str; 4] = ["Heist Brooch", "Heist Cloak", "Heist Gear", "Heist Tool"];
    match test {
        "WEAPON" => WEAPON.contains(&category),
        "ARMOUR" => ARMOUR.contains(&category),
        "HEIST_EQUIPMENT" => HEIST.contains(&category),
        literal => literal == category,
    }
}

/// Maps templated mod text (rolls replaced by `#`) to candidate stats.
pub struct StatIndex {
    by_text: HashMap<String, Vec<StatMatch>>,
}

impl StatIndex {
    /// Candidate stats for a templated mod line. Empty slice if unknown.
    pub fn lookup(&self, text: &str) -> &[StatMatch] {
        self.by_text.get(text).map_or(&[], Vec::as_slice)
    }

    /// Number of distinct matcher strings indexed.
    pub fn len(&self) -> usize {
        self.by_text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_text.is_empty()
    }
}

fn index_stat(
    stat: Stat,
    category_test: Option<String>,
    by_text: &mut HashMap<String, Vec<StatMatch>>,
) {
    for matcher in &stat.matchers {
        let entry = StatMatch {
            stat_ref: stat.ref_.clone(),
            trade_ids: stat.trade.ids.clone(),
            value: matcher.value,
            negate: matcher.negate.unwrap_or(false),
            category_test: category_test.clone(),
        };
        by_text
            .entry(matcher.string.clone())
            .or_default()
            .push(entry.clone());
        if let Some(advanced) = &matcher.advanced {
            by_text.entry(advanced.clone()).or_default().push(entry);
        }
    }
}

/// Parse and index the embedded stat translations.
pub fn load_stats() -> StatIndex {
    let mut by_text: HashMap<String, Vec<StatMatch>> = HashMap::new();
    let mut skipped = 0usize;
    for line in STATS_NDJSON.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<StatEntry>(line) {
            Ok(StatEntry::Single(stat)) => index_stat(stat, None, &mut by_text),
            Ok(StatEntry::Group(group)) => {
                let tests = group.resolve.and_then(|r| r.test).unwrap_or_default();
                for (i, stat) in group.stats.into_iter().enumerate() {
                    let test = tests.get(i).cloned().flatten();
                    index_stat(stat, test, &mut by_text);
                }
            }
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "skipped unparseable stat lines");
    }
    StatIndex { by_text }
}

// ---------------------------------------------------------------------------
// items.ndjson
// ---------------------------------------------------------------------------

/// Craftable-item metadata (present on gear base types).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Craftable {
    pub category: String,
    #[serde(default)]
    pub corrupted: Option<bool>,
}

/// Unique-item metadata.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UniqueInfo {
    pub base: String,
}

/// A base type, unique, gem, card, or beast from items.ndjson.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub name: String,
    pub ref_name: String,
    /// One of ITEM, UNIQUE, GEM, DIVINATION_CARD, CAPTURED_BEAST.
    pub namespace: String,
    #[serde(default)]
    pub trade_tag: Option<String>,
    #[serde(default)]
    pub craftable: Option<Craftable>,
    #[serde(default)]
    pub unique: Option<UniqueInfo>,
}

/// Items indexed by `namespace::name`.
pub struct ItemIndex {
    by_key: HashMap<String, Item>,
}

impl ItemIndex {
    /// A base type by its printed name.
    pub fn base_type(&self, name: &str) -> Option<&Item> {
        self.by_key.get(&format!("ITEM::{name}"))
    }

    /// A unique by its printed name.
    pub fn unique(&self, name: &str) -> Option<&Item> {
        self.by_key.get(&format!("UNIQUE::{name}"))
    }

    /// The bulk-exchange trade tag for a name (currency, fragments, cards, …).
    pub fn trade_tag(&self, name: &str) -> Option<&str> {
        ["ITEM", "DIVINATION_CARD"].iter().find_map(|ns| {
            self.by_key
                .get(&format!("{ns}::{name}"))
                .and_then(|item| item.trade_tag.as_deref())
        })
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

/// Parse and index the embedded base-item data.
pub fn load_items() -> ItemIndex {
    let mut by_key = HashMap::new();
    let mut skipped = 0usize;
    for line in ITEMS_NDJSON.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Item>(line) {
            Ok(item) => {
                by_key.insert(format!("{}::{}", item.namespace, item.name), item);
            }
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "skipped unparseable item lines");
    }
    ItemIndex { by_key }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_stats_to_trade_ids() {
        let stats = load_stats();
        assert!(!stats.is_empty());
        let matches =
            stats.lookup("# Cold Damage taken per second per Frenzy Charge while moving");
        assert!(matches.iter().any(|m| {
            m.trade_ids.get("explicit") == Some(&vec!["explicit.stat_1528823952".to_string()])
        }));
    }

    #[test]
    fn indexes_advanced_matcher_form() {
        let stats = load_stats();
        // The hoisted "advanced" form (with the hidden skill name) is indexed too.
        let matches = stats.lookup("# to Level of all Absolution(Fireball-Divine Blast) Gems");
        assert!(matches.iter().any(|m| m.stat_ref.contains("Absolution")));
    }

    #[test]
    fn resolves_base_and_unique_items() {
        let items = load_items();
        assert!(!items.is_empty());

        let fossil = items.base_type("Aberrant Fossil").expect("known base type");
        assert_eq!(fossil.namespace, "ITEM");
        assert_eq!(fossil.trade_tag.as_deref(), Some("aberrant-fossil"));

        let hooves = items.unique("Abberath's Hooves").expect("known unique");
        assert_eq!(hooves.unique.as_ref().map(|u| u.base.as_str()), Some("Goathide Boots"));
    }
}
