//! Parses PoE clipboard text into a [`ParsedItem`].
//!
//! The clipboard format separates blocks with a line of exactly eight dashes.
//! The first block is the "name plate" (`Item Class:`, optional `Rarity:`, then
//! name/base). [`parse_name_plate`] reads just that; [`parse_item`] also walks
//! the remaining sections for meta (item level, sockets, corrupted, influences)
//! and resolves each affix to a trade stat-id, handling both the standard
//! (suffix) and advanced (`{ … }` info line) clipboard formats.

use crate::data::{ItemIndex, StatIndex};
use crate::item::mods::{ModType, Slot};
use crate::item::{Game, Influence, ParsedItem, Rarity, mods};

pub const ITEM_CLASS_PREFIX: &str = "Item Class: ";
pub const RARITY_PREFIX: &str = "Rarity: ";
pub const SECTION_SEPARATOR: &str = "--------";

/// Why a clipboard string could not be read as a PoE item.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("clipboard text is not a Path of Exile item (no Item Class or Rarity line)")]
    NotAnItem,
    #[error("item name plate ended before a base type could be read")]
    Truncated,
}

/// Split raw item text into sections, each a list of non-empty lines.
///
/// Empty sections are dropped, matching the reference parser.
pub fn split_sections(text: &str) -> Vec<Vec<&str>> {
    let mut sections: Vec<Vec<&str>> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if line == SECTION_SEPARATOR {
            if !current.is_empty() {
                sections.push(std::mem::take(&mut current));
            }
        } else if !line.trim().is_empty() {
            current.push(line);
        }
    }
    if !current.is_empty() {
        sections.push(current);
    }
    sections
}

/// Read the name plate (first section) into a [`ParsedItem`].
pub fn parse_name_plate(text: &str, game: Game) -> Result<ParsedItem, ParseError> {
    let sections = split_sections(text);
    let plate = sections.first().ok_or(ParseError::NotAnItem)?;
    let mut lines: Vec<&str> = plate.clone();

    // `Item Class:` is present in the in-game clipboard but omitted by some
    // sources (e.g. poe.ninja), so treat it as optional.
    let item_class = match lines.first().and_then(|l| l.strip_prefix(ITEM_CLASS_PREFIX)) {
        Some(rest) => {
            let class = rest.trim().to_string();
            lines.remove(0);
            class
        }
        None => String::new(),
    };

    let rarity = match lines.first().and_then(|l| l.strip_prefix(RARITY_PREFIX)) {
        Some(label) => {
            let rarity = Rarity::from_label(label);
            lines.remove(0);
            Some(rarity)
        }
        None => None,
    };

    // Require at least one of Item Class / Rarity to consider this an item.
    if item_class.is_empty() && rarity.is_none() {
        return Err(ParseError::NotAnItem);
    }

    // The last remaining line is the base type; a preceding line is the name.
    let (name, base_type) = match lines.as_slice() {
        [] => return Err(ParseError::Truncated),
        [base] => (None, (*base).to_string()),
        [name, base, ..] => (Some((*name).to_string()), (*base).to_string()),
    };

    Ok(ParsedItem {
        game,
        item_class,
        rarity,
        name,
        base_type,
        raw_text: text.to_string(),
        ..Default::default()
    })
}

/// Parse a full item: name plate, meta sections, and resolved affixes.
pub fn parse_item(
    text: &str,
    game: Game,
    stats: &StatIndex,
    items: &ItemIndex,
) -> Result<ParsedItem, ParseError> {
    let mut item = parse_name_plate(text, game)?;
    // Resolve the category first: local/global stat variants depend on it.
    item.category = resolve_category(&item, items);
    let sections = split_sections(text);
    let mut context: Option<(ModType, Option<Slot>)> = None;

    for section in sections.iter().skip(1) {
        // The weapon/armour base-stats block holds Quality alongside the
        // item-type header and numeric properties; take Quality, skip the rest.
        if is_base_stats_section(section) {
            for line in section {
                parse_meta_value(line.trim(), &mut item);
            }
            continue;
        }
        for line in section.iter().map(|l| l.trim()) {
            // An advanced `{ … Modifier … }` info line sets the type and slot
            // for the mods that follow it.
            if let Some(info) = ModType::from_info_line(line) {
                context = Some(info);
                continue;
            }
            // Fully parenthesised lines are reminder/help text, not mods.
            if line.starts_with('(') && line.ends_with(')') {
                continue;
            }
            if parse_meta_value(line, &mut item) || parse_meta_flag(line, &mut item) {
                continue;
            }
            // Skip requirements and other "Label: value" / section-head lines.
            if line.contains(": ") || line.ends_with(':') {
                continue;
            }
            match mods::parse_mod(line, stats, context, item.category.as_deref()) {
                Some(parsed) => item.mods.push(parsed),
                // Only surface unresolved lines that look like a rollable mod (a
                // space and a digit) — skips unique flavour text and lone
                // weapon-type headers.
                None if line.contains(' ') && line.bytes().any(|b| b.is_ascii_digit()) => {
                    item.unknown_mods.push(line.to_string());
                }
                None => {}
            }
        }
        context = None;
    }

    Ok(item)
}

/// Handle the `Item Level:` / `Quality:` / `Sockets:` meta lines.
fn parse_meta_value(line: &str, item: &mut ParsedItem) -> bool {
    if let Some(rest) = line.strip_prefix("Item Level: ") {
        item.item_level = rest.trim().parse().ok();
        return true;
    }
    // "Quality: +20%" or "Quality (Defence Modifiers): +20% (augmented)".
    if line.starts_with("Quality")
        && let Some(idx) = line.find(": ")
    {
        item.quality = parse_quality(&line[idx + 2..]);
        return true;
    }
    if let Some(rest) = line.strip_prefix("Sockets: ") {
        item.sockets = Some(socket_count(rest));
        item.links = Some(max_links(rest));
        return true;
    }
    false
}

/// Handle flag lines (Corrupted/Fractured/Split/…) and influences.
fn parse_meta_flag(line: &str, item: &mut ParsedItem) -> bool {
    match line {
        "Corrupted" => item.corrupted = true,
        "Mirrored" => item.mirrored = true,
        "Unidentified" => item.unidentified = true,
        "Split" => item.split = true,
        "Fractured Item" => item.fractured = true,
        "Synthesised Item" => item.synthesised = true,
        _ => {
            return match Influence::from_line(line) {
                Some(influence) => {
                    item.influences.push(influence);
                    true
                }
                None => false,
            };
        }
    }
    true
}

/// Parse `Quality: +20% (augmented)` -> 20.
fn parse_quality(rest: &str) -> Option<u32> {
    rest.trim_start_matches('+').split('%').next()?.trim().parse().ok()
}

/// Size of the largest linked socket group in a `Sockets:` value.
fn max_links(sockets: &str) -> u8 {
    sockets
        .split_whitespace()
        .map(|group| group.split('-').count() as u8)
        .max()
        .unwrap_or(0)
}

/// Total socket count in a `Sockets:` value (e.g. "R-G-G R" -> 4).
fn socket_count(sockets: &str) -> u8 {
    sockets
        .split_whitespace()
        .map(|group| group.split('-').count() as u8)
        .sum()
}

/// Resolve the item's trade category from its base type, when known.
fn resolve_category(item: &ParsedItem, items: &ItemIndex) -> Option<String> {
    items
        .base_type(&item.base_type)
        .and_then(|base| base.craftable.as_ref())
        .map(|craftable| craftable.category.clone())
}

/// Labels that mark the weapon/armour base-stats block.
const STAT_PROPERTY_PREFIXES: [&str; 14] = [
    "Physical Damage:",
    "Elemental Damage:",
    "Chaos Damage:",
    "Fire Damage:",
    "Cold Damage:",
    "Lightning Damage:",
    "Critical Strike Chance:",
    "Attacks per Second:",
    "Weapon Range:",
    "Armour:",
    "Evasion Rating:",
    "Energy Shield:",
    "Ward:",
    "Chance to Block:",
];

/// Whether a section is the weapon/armour base-stats block.
fn is_base_stats_section(section: &[&str]) -> bool {
    section.iter().any(|line| {
        let line = line.trim_start();
        STAT_PROPERTY_PREFIXES.iter().any(|p| line.starts_with(p))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{load_items, load_stats};
    use crate::item::mods::{ModType, Slot};
    use crate::item::{Game, Rarity};

    const CHAOS_ORB: &str = "Item Class: Stackable Currency\nRarity: Currency\nChaos Orb\n--------\nStack Size: 20/10\n--------\nReforges a rare item with new random modifiers\n";

    // A real Ctrl+C item from poe.ninja: no `Item Class:` line, suffix mod types.
    const CLAW: &str = "Rarity: Rare\nFoe Hunger\nGemini Claw\n--------\nItem Level: 85\n--------\nClaw\nQuality: +20%\nPhysical Damage: 23-68\nElemental Damage: 111-182\nCritical Strike Chance: 10.65%\nAttacks per Second: 1.95\nWeapon Range: 1.1 metres\n--------\nRequirements:\nLevel: 72\nDex: 155\nInt: 121\n--------\nSockets: G-G-B\n--------\nGrants 38 Life per Enemy Hit (implicit)\n--------\nAdds 111 to 182 Cold Damage (fractured)\n30% increased Attack Speed\n+32% to Global Critical Strike Multiplier\nHits can't be Evaded (crafted)\n--------\nFractured Item\n";

    // A real Ctrl+Alt+C in-game item: advanced `{ … }` info lines carry the type.
    const SWORD: &str = r#"Item Class: Thrusting One Hand Swords
Rarity: Rare
Dragon Barb
Jewelled Foil
--------
One Handed Sword
Quality: +29% (augmented)
Physical Damage: 32-60
Elemental Damage: 96-170 (augmented)
Critical Strike Chance: 7.42% (augmented)
Attacks per Second: 2.13 (augmented)
Weapon Range: 1.4 metres
--------
Requirements:
Level: 72
Str: 159
Dex: 212
Int: 100
--------
Sockets: W-W-W
--------
Item Level: 84
--------
Quality does not increase Physical Damage (enchant)
1% increased Attack Speed per 8% Quality (enchant)
--------
{ Implicit Modifier — Damage, Critical }
+25% to Global Critical Strike Multiplier
--------
{ Fractured Prefix Modifier "Crystalising" (Tier: 1) — Damage, Elemental, Cold, Attack }
Adds 96(81-111) to 170(163-189) Cold Damage
{ Prefix Modifier "Chosen" (Tier: 1) — Damage, Elemental, Attack }
Attacks with this Weapon Penetrate 16(14-16)% Elemental Resistances
{ Master Crafted Prefix Modifier "Upgraded" — Attack }
Hits can't be Evaded — Unscalable Value
{ Suffix Modifier "of the Essence" — Attack, Speed }
30(28-30)% increased Attack Speed
{ Suffix Modifier "of Incision" (Tier: 1) — Attack, Critical }
35(35-38)% increased Critical Strike Chance
{ Suffix Modifier "of Destruction" (Tier: 1) — Damage, Critical }
+36(35-38)% to Global Critical Strike Multiplier
--------
Split
--------
Fractured Item
"#;

    #[test]
    fn parses_currency_without_a_name() {
        let item = parse_name_plate(CHAOS_ORB, Game::Poe1).unwrap();
        assert_eq!(item.item_class, "Stackable Currency");
        assert_eq!(item.rarity, Some(Rarity::Currency));
        assert_eq!(item.name, None);
        assert_eq!(item.base_type, "Chaos Orb");
    }

    #[test]
    fn rejects_non_item_text() {
        let err = parse_name_plate("just some text\nnothing special\n", Game::Poe1).unwrap_err();
        assert_eq!(err, ParseError::NotAnItem);
    }

    #[test]
    fn parses_claw_without_item_class_header() {
        let stats = load_stats();
        let items = load_items();
        let item = parse_item(CLAW, Game::Poe1, &stats, &items).unwrap();

        // Name plate parsed despite the missing `Item Class:` line.
        assert_eq!(item.item_class, "");
        assert_eq!(item.rarity, Some(Rarity::Rare));
        assert_eq!(item.name.as_deref(), Some("Foe Hunger"));
        assert_eq!(item.base_type, "Gemini Claw");
        assert_eq!(item.item_level, Some(85));
        assert_eq!(item.quality, Some(20));
        assert_eq!(item.links, Some(3));
        assert!(item.fractured);

        // Suffix-based mod typing across explicit / crafted.
        assert!(
            item.mods
                .iter()
                .any(|m| m.mod_type == ModType::Explicit && m.text == "30% increased Attack Speed")
        );
        assert!(
            item.mods
                .iter()
                .any(|m| m.mod_type == ModType::Crafted && m.stat_ref == "Hits can't be Evaded")
        );
        // The "Claw" weapon-type header is not surfaced as an unknown line.
        assert!(!item.unknown_mods.iter().any(|l| l == "Claw"));
    }

    #[test]
    fn parses_sword_advanced_mod_descriptions() {
        let stats = load_stats();
        let items = load_items();
        let item = parse_item(SWORD, Game::Poe1, &stats, &items).unwrap();

        assert_eq!(item.item_class, "Thrusting One Hand Swords");
        assert_eq!(item.name.as_deref(), Some("Dragon Barb"));
        assert_eq!(item.base_type, "Jewelled Foil");
        assert_eq!(item.item_level, Some(84));
        assert_eq!(item.quality, Some(29));
        assert_eq!(item.links, Some(3));
        assert!(item.fractured);
        assert!(item.split);

        let has = |ty: ModType, stat_ref: &str| {
            item.mods.iter().any(|m| m.mod_type == ty && m.stat_ref == stat_ref)
        };
        // Crafted via `{ Master Crafted … }` context + em-dash tail stripped.
        assert!(has(ModType::Crafted, "Hits can't be Evaded"));
        // Explicit via plain `{ Prefix/Suffix Modifier }` context.
        assert!(has(ModType::Explicit, "#% increased Attack Speed"));
        assert!(has(ModType::Explicit, "#% increased Critical Strike Chance"));
        assert!(has(
            ModType::Explicit,
            "Attacks with this Weapon Penetrate #% Elemental Resistances"
        ));

        // Prefix/suffix slots come from the info lines.
        assert!(item.mods.iter().any(|m| {
            m.stat_ref == "Adds # to # Cold Damage" && m.slot == Some(Slot::Prefix)
        }));
        assert!(item.mods.iter().any(|m| {
            m.stat_ref == "#% increased Attack Speed" && m.slot == Some(Slot::Suffix)
        }));
        // The implicit has no slot.
        assert!(item.mods.iter().any(|m| {
            m.mod_type == ModType::Implicit && m.slot.is_none()
        }));

        // The weapon-type header is skipped; `{ … }` lines are consumed.
        assert!(!item.unknown_mods.iter().any(|l| l == "One Handed Sword"));
        assert!(!item.unknown_mods.iter().any(|l| l.starts_with('{')));
    }

    #[test]
    fn parses_eldritch_chest_with_reminder_text_and_sockets() {
        const CHEST: &str = r#"Item Class: Body Armours
Rarity: Rare
Victory Coat
Conquest Lamellar
--------
Quality: +20% (augmented)
Armour: 2401 (augmented)
Evasion Rating: 2501 (augmented)
--------
Requirements:
Level: 84
Str: 173
--------
Sockets: R-G-G-R-G-R
--------
Item Level: 86
--------
{ Searing Exarch Implicit Modifier (Lesser) }
Gain an Endurance Charge every 15 seconds
{ Eater of Worlds Implicit Modifier (Greater) — Attack }
Melee Hits have 9(8-9)% chance to Fortify
(Fortifying grants an amount of Fortification based on the Damage of the Hit)
(Take 1% less Damage from Hits per Fortification. Maximum 20 Fortification. Fortification lasts 6 seconds)
--------
{ Prefix Modifier "Versatile" (Tier: 1) — Defences, Armour, Evasion }
+326(301-375) to Armour
+374(301-375) to Evasion Rating
{ Prefix Modifier "Vigorous" (Tier: 3) — Life }
+159(145-159) to maximum Life
{ Master Crafted Prefix Modifier "Upgraded" (Rank: 3) — Defences, Armour, Evasion }
74(56-74)% increased Armour and Evasion
{ Fractured Suffix Modifier "of Nullification" (Tier: 1) }
+22(20-22)% chance to Suppress Spell Damage
(40% of Damage from Suppressed Hits and Ailments they inflict is prevented)
{ Suffix Modifier "of Ephij" (Tier: 1) — Elemental, Lightning, Resistance }
+48(46-48)% to Lightning Resistance
--------
Split
Searing Exarch Item
Eater of Worlds Item
--------
Fractured Item
"#;
        let stats = load_stats();
        let items = load_items();
        let item = parse_item(CHEST, Game::Poe1, &stats, &items).unwrap();

        // 6 sockets, 6-linked.
        assert_eq!(item.sockets, Some(6));
        assert_eq!(item.links, Some(6));
        assert!(item.split);
        assert!(item.fractured);

        // Reminder/help lines in parentheses are not mods and not "unknown".
        assert!(
            !item
                .unknown_mods
                .iter()
                .any(|l| l.starts_with('(') && l.ends_with(')')),
            "parenthesised reminder text leaked: {:?}",
            item.unknown_mods
        );

        // Flat armour on a body armour resolves to the LOCAL trade stat.
        assert_eq!(item.category.as_deref(), Some("Body Armour"));
        assert!(item.mods.iter().any(|m| {
            m.text.contains("to Armour")
                && m.trade_ids.contains(&"explicit.stat_3484657501".to_string())
        }));

        // Eldritch implicits type as implicits; crafted/fractured keep types.
        assert!(item.mods.iter().any(|m| m.mod_type == ModType::Implicit));
        assert!(
            item.mods
                .iter()
                .any(|m| m.mod_type == ModType::Crafted
                    && m.stat_ref == "#% increased Armour and Evasion")
        );
        assert!(
            item.mods
                .iter()
                .any(|m| m.mod_type == ModType::Fractured && m.slot == Some(Slot::Suffix))
        );
    }

    #[test]
    fn parses_unique_amulet_with_parenthesised_quality() {
        const AMULET: &str = r#"Item Class: Amulets
Rarity: Unique
Whispers of Infinity
Seaglass Amulet
--------
Quality (Defence Modifiers): +20% (augmented)
--------
Requirements:
Level: 74
--------
Item Level: 87
--------
13% faster start of Energy Shield Recharge (implicit)
--------
9 to 23 Added Attack Chaos Damage per 100 Maximum Mana
+81 to maximum Energy Shield
60% reduced maximum Mana
Skills Cost Energy Shield instead of Mana or Life
--------
In the Atlas, you do not go mad. You are rewritten.
--------
Corrupted
--------
Note: ~b/o 25 chaos
"#;
        let stats = load_stats();
        let items = load_items();
        let item = parse_item(AMULET, Game::Poe1, &stats, &items).unwrap();

        assert_eq!(item.rarity, Some(Rarity::Unique));
        assert_eq!(item.name.as_deref(), Some("Whispers of Infinity"));
        assert_eq!(item.base_type, "Seaglass Amulet");
        assert_eq!(item.item_level, Some(87));
        assert_eq!(item.quality, Some(20)); // parenthesised "Quality (Defence Modifiers)"
        assert!(item.corrupted);
        assert!(item.mods.iter().any(|m| m.stat_ref == "+# to maximum Energy Shield"));
        assert!(item.mods.len() >= 3);
        // The flavour line ("In the Atlas…") is prose, not a mod — not flagged.
        assert!(item.unknown_mods.is_empty());
    }
}
