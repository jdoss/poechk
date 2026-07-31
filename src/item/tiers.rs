//! The vendored affix tier ladder, and filling a parsed item's gaps from it.
//!
//! The advanced clipboard names a mod's own tier and its range but nothing
//! about the tiers around it, and the standard clipboard names neither. The
//! ladder (see `examples/vendor-tiers.rs`) supplies both, so a roll can be
//! placed among its tiers rather than only inside its own.
//!
//! What the clipboard said always wins: it describes this item, while the
//! ladder describes what the item's class can roll.

use std::collections::HashMap;

use serde::Deserialize;

use crate::item::ParsedItem;
use crate::item::mods::{ModType, ParsedMod};

const TIERS_NDJSON: &str = include_str!("../../data/poe1/en/tiers.ndjson");

/// One tier of one affix, as vendored.
#[derive(Debug, Clone, Deserialize)]
pub struct Tier {
    pub tier: u32,
    pub name: String,
    /// The lowest level of item that can roll it.
    pub level: u64,
    /// The tier's bounds, averaged across the mod's values the way a roll is,
    /// so they compare directly against a parsed roll.
    pub min: f64,
    pub max: f64,
}

#[derive(Debug, Deserialize)]
struct Ladder {
    #[serde(rename = "ref")]
    stat_ref: String,
    tag: String,
    tiers: Vec<Tier>,
}

/// Affix tier ladders, keyed by stat ref and the item tag they roll on.
#[derive(Debug, Default)]
pub struct TierIndex {
    by_ref_and_tag: HashMap<(String, String), Vec<Tier>>,
}

impl TierIndex {
    /// The tiers of `stat_ref` on an item tagged `tag`, cheapest tier last.
    pub fn ladder(&self, stat_ref: &str, tag: &str) -> Option<&[Tier]> {
        self.by_ref_and_tag
            .get(&(stat_ref.to_string(), tag.to_string()))
            .map(Vec::as_slice)
    }

    /// The tier whose range contains `roll`, searching `tags` most-specific
    /// first. `None` when no tier holds it — a corrupted or otherwise
    /// out-of-band roll has no place on the ladder.
    pub fn tier_of(&self, stat_ref: &str, tags: &[&str], roll: f64) -> Option<&Tier> {
        tags.iter().find_map(|tag| {
            self.ladder(stat_ref, tag)?
                .iter()
                .find(|t| roll >= t.min && roll <= t.max)
        })
    }
}

/// Load the vendored ladder.
pub fn load_tiers() -> TierIndex {
    let mut by_ref_and_tag = HashMap::new();
    let mut skipped = 0usize;
    for line in TIERS_NDJSON.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Ladder>(line) {
            Ok(l) => {
                by_ref_and_tag.insert((l.stat_ref, l.tag), l.tiers);
            }
            Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        tracing::warn!(skipped, "skipped unparseable tier lines");
    }
    TierIndex { by_ref_and_tag }
}

/// Fill in each affix's tier and tier range from the ladder, where the
/// clipboard did not supply them.
///
/// Only affixes are placed: an implicit, an enchantment and a corrupted mod do
/// not roll from the prefix/suffix pool the ladder describes, and matching one
/// against it by stat text alone would report a tier it cannot have.
pub fn apply(item: &mut ParsedItem, tiers: &TierIndex) {
    let tags = crate::price::category::spawn_tags(item);
    if tags.is_empty() {
        return;
    }
    for m in &mut item.mods {
        if !matches!(m.mod_type, ModType::Explicit | ModType::Fractured) {
            continue;
        }
        let Some(roll) = m.roll() else {
            continue;
        };
        let Some(tier) = tiers.tier_of(&m.stat_ref, tags, roll) else {
            continue;
        };
        fill_from(m, tier);
    }
}

/// Take what the clipboard left blank, and nothing it filled in.
fn fill_from(m: &mut ParsedMod, tier: &Tier) {
    if m.tier.is_none() {
        m.tier = Some(tier.tier);
    }
    if m.affix.is_none() {
        m.affix = Some(tier.name.clone());
    }
    if m.tier_range.is_none() {
        m.tier_range = Some((tier.min, tier.max));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vendored_ladder_loads_and_orders_daggers_cold_damage() {
        let tiers = load_tiers();
        let ladder = tiers
            .ladder("Adds # to # Cold Damage", "dagger")
            .expect("daggers roll added cold damage");

        // Tier 1 is the highest-level roll, the way the game numbers them.
        assert_eq!(ladder[0].tier, 1);
        assert_eq!(ladder[0].name, "Crystalising");
        assert_eq!((ladder[0].min, ladder[0].max), (122.0, 150.0));
        assert!(ladder.windows(2).all(|w| w[0].level > w[1].level));
        assert!(ladder.windows(2).all(|w| w[0].tier < w[1].tier));
    }

    #[test]
    fn a_roll_is_placed_on_the_tier_whose_range_contains_it() {
        let tiers = load_tiers();

        // The roll this whole feature came from: 91-172 averages 131.5.
        let t = tiers.tier_of("Adds # to # Cold Damage", &["dagger"], 131.5).unwrap();
        assert_eq!((t.tier, t.name.as_str()), (1, "Crystalising"));
        // And the listing it kept missing, at 130.
        assert_eq!(tiers.tier_of("Adds # to # Cold Damage", &["dagger"], 130.0).unwrap().tier, 1);
        // A roll below every tier belongs to none of them.
        assert!(tiers.tier_of("Adds # to # Cold Damage", &["dagger"], 0.5).is_none());
        assert!(tiers.tier_of("Adds # to # Cold Damage", &["no-such-tag"], 131.5).is_none());
    }

    fn bare_mod() -> ParsedMod {
        ParsedMod {
            text: String::new(),
            mod_type: ModType::Explicit,
            slot: None,
            affix: None,
            tier: None,
            template: String::new(),
            rolls: Vec::new(),
            tier_range: None,
            option: None,
            lower_is_better: false,
            stat_ref: String::new(),
            trade_ids: Vec::new(),
            explicit_ids: Vec::new(),
            pseudo_ids: Vec::new(),
        }
    }

    #[test]
    fn the_clipboards_own_reading_is_never_overwritten() {
        let mut m = ParsedMod {
            tier: Some(2),
            affix: Some("of Ferocity".to_string()),
            tier_range: Some((30.0, 34.0)),
            ..bare_mod()
        };
        fill_from(
            &mut m,
            &Tier {
                tier: 9,
                name: "Wrong".to_string(),
                level: 1,
                min: 1.0,
                max: 2.0,
            },
        );

        // The clipboard describes this item; the ladder describes its class.
        assert_eq!(m.tier, Some(2));
        assert_eq!(m.affix.as_deref(), Some("of Ferocity"));
        assert_eq!(m.tier_range, Some((30.0, 34.0)));
    }

    #[test]
    fn a_blank_reading_is_filled_from_the_ladder() {
        let mut m = bare_mod();
        fill_from(
            &mut m,
            &Tier {
                tier: 3,
                name: "Polar".to_string(),
                level: 63,
                min: 81.0,
                max: 100.0,
            },
        );

        assert_eq!(m.tier, Some(3));
        assert_eq!(m.affix.as_deref(), Some("Polar"));
        assert_eq!(m.tier_range, Some((81.0, 100.0)));
    }
}
