//! Folds parsed affixes into pseudo trade filters.
//!
//! The trade API exposes `pseudo.*` stats that sum related mods — total
//! elemental resistance, total life, total chaos resistance. Searching those
//! finds comparables whose resistances are spread differently across affixes,
//! which per-stat filters never match. Contributor weights follow Awakened PoE
//! Trade: single resists ×1, dual ×2, all-res ×3; strength adds 0.5 life.

use crate::item::ParsedItem;
use crate::item::mods::{ModType, ParsedMod};

/// A contributor: (canonical stat ref, weight applied to its roll).
type Rule = (&'static str, f64);

struct PseudoDef {
    /// Display template; `#` is replaced with the computed total.
    label: &'static str,
    /// Stable pathofexile.com trade stat id.
    trade_id: &'static str,
    rules: &'static [Rule],
    /// Disable contributors matching this predicate once folded (they are
    /// represented by the pseudo row).
    disable_contributors: bool,
}

const TOTAL_ELE_RES: PseudoDef = PseudoDef {
    label: "+#% total Elemental Resistance",
    trade_id: "pseudo.pseudo_total_elemental_resistance",
    rules: &[
        ("+#% to Fire Resistance", 1.0),
        ("+#% to Cold Resistance", 1.0),
        ("+#% to Lightning Resistance", 1.0),
        ("+#% to Fire and Cold Resistances", 2.0),
        ("+#% to Fire and Lightning Resistances", 2.0),
        ("+#% to Cold and Lightning Resistances", 2.0),
        ("+#% to all Elemental Resistances", 3.0),
        ("+#% to Fire and Chaos Resistances", 1.0),
        ("+#% to Cold and Chaos Resistances", 1.0),
        ("+#% to Lightning and Chaos Resistances", 1.0),
    ],
    disable_contributors: true,
};

const TOTAL_CHAOS_RES: PseudoDef = PseudoDef {
    label: "+#% total to Chaos Resistance",
    trade_id: "pseudo.pseudo_total_chaos_resistance",
    rules: &[
        ("+#% to Chaos Resistance", 1.0),
        ("+#% to Fire and Chaos Resistances", 1.0),
        ("+#% to Cold and Chaos Resistances", 1.0),
        ("+#% to Lightning and Chaos Resistances", 1.0),
    ],
    disable_contributors: true,
};

const TOTAL_LIFE: PseudoDef = PseudoDef {
    label: "+# total maximum Life",
    trade_id: "pseudo.pseudo_total_life",
    rules: &[
        ("+# to maximum Life", 1.0),
        ("+# to Strength", 0.5),
        ("+# to Strength and Dexterity", 0.5),
        ("+# to Strength and Intelligence", 0.5),
        ("+# to all Attributes", 0.5),
    ],
    // Attributes are their own want; only the flat-life row is subsumed.
    disable_contributors: false,
};

const PSEUDO_DEFS: [&PseudoDef; 3] = [&TOTAL_ELE_RES, &TOTAL_CHAOS_RES, &TOTAL_LIFE];

/// The per-stat pseudo id for a stat, if the trade API has one. These sum the
/// stat across all of an item's mods (so "+48% to Lightning Resistance"
/// searched as pseudo matches items whose combined lightning res is 48+).
pub fn per_stat_pseudo_id(stat_ref: &str) -> Option<&'static str> {
    Some(match stat_ref {
        "+#% to Fire Resistance" => "pseudo.pseudo_total_fire_resistance",
        "+#% to Cold Resistance" => "pseudo.pseudo_total_cold_resistance",
        "+#% to Lightning Resistance" => "pseudo.pseudo_total_lightning_resistance",
        "+#% to Chaos Resistance" => "pseudo.pseudo_total_chaos_resistance",
        "+#% to all Elemental Resistances" => "pseudo.pseudo_total_all_elemental_resistances",
        "+# to maximum Life" => "pseudo.pseudo_total_life",
        "+# to maximum Mana" => "pseudo.pseudo_total_mana",
        "+# to maximum Energy Shield" => "pseudo.pseudo_total_energy_shield",
        "+# to Strength" => "pseudo.pseudo_total_strength",
        "+# to Dexterity" => "pseudo.pseudo_total_dexterity",
        "+# to Intelligence" => "pseudo.pseudo_total_intelligence",
        "+# to all Attributes" => "pseudo.pseudo_total_all_attributes",
        _ => return None,
    })
}

/// Append pseudo rows for `item`'s mods. Returns the indices of contributor
/// mods that the pseudo rows subsume (callers disable those filters).
pub fn fold_pseudo(item: &mut ParsedItem) -> Vec<usize> {
    let mut subsumed: Vec<usize> = Vec::new();
    let mut appended: Vec<ParsedMod> = Vec::new();

    for def in PSEUDO_DEFS {
        let mut total = 0.0_f64;
        let mut contributors: Vec<usize> = Vec::new();
        for (index, parsed) in item.mods.iter().enumerate() {
            let Some(roll) = parsed.roll() else { continue };
            if let Some((_, weight)) = def
                .rules
                .iter()
                .find(|(stat_ref, _)| *stat_ref == parsed.stat_ref)
            {
                total += weight * roll;
                contributors.push(index);
            }
        }
        if contributors.is_empty() || total <= 0.0 {
            continue;
        }

        let total_display = total.floor();
        let text = def.label.replace('#', &format!("{}", total_display as i64));
        appended.push(ParsedMod {
            text,
            mod_type: ModType::Pseudo,
            slot: None,
            template: def.label.to_string(),
            rolls: vec![total_display],
            option: None,
            tier_range: None,
            lower_is_better: false,
            stat_ref: def.label.to_string(),
            trade_ids: vec![def.trade_id.to_string()],
            explicit_ids: Vec::new(),
            pseudo_ids: Vec::new(),
        });
        if def.disable_contributors {
            subsumed.extend(&contributors);
        } else if let Some(&flat) = contributors
            .iter()
            .find(|&&i| item.mods[i].stat_ref == def.rules[0].0)
        {
            subsumed.push(flat);
        }
    }

    item.mods.extend(appended);
    subsumed.sort_unstable();
    subsumed.dedup();
    subsumed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{load_items, load_stats};
    use crate::item::Game;
    use crate::item::parse::parse_item;

    fn synthetic(stat_ref: &str, roll: f64) -> ParsedMod {
        ParsedMod {
            text: stat_ref.replace('#', &roll.to_string()),
            mod_type: ModType::Explicit,
            slot: None,
            template: stat_ref.to_string(),
            rolls: vec![roll],
            option: None,
            tier_range: None,
            lower_is_better: false,
            stat_ref: stat_ref.to_string(),
            trade_ids: vec!["explicit.stat_test".to_string()],
            explicit_ids: vec![],
            pseudo_ids: vec![],
        }
    }

    #[test]
    fn sums_weights_and_subsumes_contributors() {
        let mut item = ParsedItem {
            mods: vec![
                synthetic("+#% to Fire Resistance", 30.0),
                synthetic("+#% to Fire and Cold Resistances", 12.0),
                synthetic("+#% to all Elemental Resistances", 10.0),
                synthetic("+#% to Chaos Resistance", 20.0),
                synthetic("+#% to Fire and Chaos Resistances", 8.0),
                synthetic("+# to maximum Life", 80.0),
                synthetic("+# to Strength", 40.0),
            ],
            ..Default::default()
        };
        let subsumed = fold_pseudo(&mut item);

        // 30 + 12*2 + 10*3 + 8 = 92 total elemental resistance.
        let ele = item
            .mods
            .iter()
            .find(|m| m.stat_ref == "+#% total Elemental Resistance")
            .expect("ele pseudo");
        assert_eq!(ele.roll(), Some(92.0));
        assert_eq!(ele.mod_type, ModType::Pseudo);
        assert_eq!(ele.trade_ids, vec!["pseudo.pseudo_total_elemental_resistance"]);

        // 20 + 8 = 28 chaos.
        let chaos = item
            .mods
            .iter()
            .find(|m| m.stat_ref == "+#% total to Chaos Resistance")
            .expect("chaos pseudo");
        assert_eq!(chaos.roll(), Some(28.0));

        // 80 + 0.5*40 = 100 life.
        let life = item
            .mods
            .iter()
            .find(|m| m.stat_ref == "+# total maximum Life")
            .expect("life pseudo");
        assert_eq!(life.roll(), Some(100.0));

        // All resistance contributors subsumed (0,1,2,3,4) + flat life (5),
        // but NOT strength (6).
        assert_eq!(subsumed, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn no_contributors_appends_nothing() {
        let mut item = ParsedItem {
            mods: vec![synthetic("#% increased Attack Speed", 17.0)],
            ..Default::default()
        };
        let subsumed = fold_pseudo(&mut item);
        assert!(subsumed.is_empty());
        assert_eq!(item.mods.len(), 1);
    }

    #[test]
    fn folds_real_parsed_resistances() {
        // End-to-end against the real data: refs must match what the parser
        // produces, or the rules silently never fire.
        const RING: &str = "Item Class: Rings\nRarity: Rare\nWoe Loop\nIron Ring\n--------\nItem Level: 80\n--------\n+35% to Fire Resistance\n+40% to Cold Resistance\n+62 to maximum Life\n";
        let stats = load_stats();
        let items = load_items();
        let mut item = parse_item(RING, Game::Poe1, &stats, &items).unwrap();
        let subsumed = fold_pseudo(&mut item);

        let ele = item
            .mods
            .iter()
            .find(|m| m.stat_ref == "+#% total Elemental Resistance")
            .expect("pseudo from real parse");
        assert_eq!(ele.roll(), Some(75.0));
        // Both resistance rows subsumed; the life row too (flat life).
        assert_eq!(subsumed.len(), 3);
    }
}
