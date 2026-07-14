//! Game-agnostic item model and the entry point for parsing PoE clipboard text.

pub mod mods;
pub mod parse;
pub mod pseudo;

use serde::{Deserialize, Serialize};

use crate::item::mods::ParsedMod;

/// Which Path of Exile the item and pricing endpoints belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Game {
    #[default]
    Poe1,
    Poe2,
}

/// Item rarity as printed on the in-game clipboard `Rarity:` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rarity {
    Normal,
    Magic,
    Rare,
    Unique,
    Gem,
    Currency,
    DivinationCard,
    Quest,
    /// Present on the `Rarity:` line but not one of the known values.
    Unknown,
}

impl Rarity {
    /// Map the text after `Rarity: ` to a [`Rarity`].
    pub fn from_label(label: &str) -> Rarity {
        match label.trim() {
            "Normal" => Rarity::Normal,
            "Magic" => Rarity::Magic,
            "Rare" => Rarity::Rare,
            "Unique" => Rarity::Unique,
            "Gem" => Rarity::Gem,
            "Currency" => Rarity::Currency,
            "Divination Card" => Rarity::DivinationCard,
            "Quest" => Rarity::Quest,
            _ => Rarity::Unknown,
        }
    }
}

/// An item influence (the "Shaper Item" / "Elder Item" / … lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Influence {
    Shaper,
    Elder,
    Crusader,
    Hunter,
    Redeemer,
    Warlord,
    SearingExarch,
    EaterOfWorlds,
}

impl Influence {
    /// Map an influence line ("Shaper Item", …) to an [`Influence`].
    pub fn from_line(line: &str) -> Option<Influence> {
        const LINES: [(&str, Influence); 8] = [
            ("Shaper Item", Influence::Shaper),
            ("Elder Item", Influence::Elder),
            ("Crusader Item", Influence::Crusader),
            ("Hunter Item", Influence::Hunter),
            ("Redeemer Item", Influence::Redeemer),
            ("Warlord Item", Influence::Warlord),
            ("Searing Exarch Item", Influence::SearingExarch),
            ("Eater of Worlds Item", Influence::EaterOfWorlds),
        ];
        LINES.iter().find(|(s, _)| *s == line).map(|(_, inf)| *inf)
    }
}

/// A parsed PoE item.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ParsedItem {
    pub game: Game,
    pub item_class: String,
    pub rarity: Option<Rarity>,
    /// The item's name (unique title or rare-generated name); `None` for items
    /// whose name plate is just a base type (currency, plain bases).
    pub name: Option<String>,
    pub base_type: String,
    /// Resolved item category (e.g. "bow"), when the base type is known.
    pub category: Option<String>,
    /// Bulk-exchange trade tag (currency, fragments, cards) when one exists.
    pub trade_tag: Option<String>,
    pub item_level: Option<u32>,
    pub quality: Option<u32>,
    /// Total number of sockets.
    pub sockets: Option<u8>,
    /// Size of the largest linked socket group.
    pub links: Option<u8>,
    /// Average physical hit damage (midpoint of the damage range).
    pub phys_damage: Option<f64>,
    /// Average elemental hit damage (sum of each range's midpoint).
    pub ele_damage: Option<f64>,
    /// Average chaos hit damage.
    pub chaos_damage: Option<f64>,
    /// Attacks per second.
    pub aps: Option<f64>,
    pub corrupted: bool,
    pub mirrored: bool,
    pub unidentified: bool,
    pub fractured: bool,
    pub synthesised: bool,
    pub split: bool,
    pub influences: Vec<Influence>,
    /// Affixes resolved to trade stat-ids.
    pub mods: Vec<ParsedMod>,
    /// Mod-like lines that did not resolve (flavour text, unsupported mods).
    pub unknown_mods: Vec<String>,
    /// The original clipboard text, kept for poeprices.info and debugging.
    pub raw_text: String,
}

impl ParsedItem {
    /// Physical DPS: average physical damage × attacks per second.
    pub fn pdps(&self) -> Option<f64> {
        Some(self.phys_damage? * self.aps?)
    }

    /// Elemental DPS: average elemental damage × attacks per second.
    pub fn edps(&self) -> Option<f64> {
        Some(self.ele_damage? * self.aps?)
    }

    /// Total DPS across physical, elemental, and chaos damage.
    pub fn total_dps(&self) -> Option<f64> {
        let aps = self.aps?;
        let damage = [self.phys_damage, self.ele_damage, self.chaos_damage];
        if damage.iter().all(Option::is_none) {
            return None;
        }
        Some(damage.iter().flatten().sum::<f64>() * aps)
    }
}
