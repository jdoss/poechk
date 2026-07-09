//! The `check` -> `overlay` contract: a parsed item plus any price quotes.

use serde::{Deserialize, Serialize};

use crate::item::ParsedItem;
use crate::price::PriceQuote;

/// What a price check produced: the parsed item and zero or more quotes.
///
/// `check` writes this (in memory, or as JSON for the `overlay` subcommand) and
/// the overlay renders it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceCheckResult {
    pub item: ParsedItem,
    /// Cheapest fetched listings, cheapest-first.
    pub quotes: Vec<PriceQuote>,
    /// Total matching listings reported by the trade search.
    #[serde(default)]
    pub total: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{Game, ParsedItem, Rarity};

    #[test]
    fn round_trips_through_json() {
        let result = PriceCheckResult {
            item: ParsedItem {
                game: Game::Poe1,
                item_class: "Bows".into(),
                rarity: Some(Rarity::Rare),
                name: Some("Death Whisper".into()),
                base_type: "Spine Bow".into(),
                raw_text: "Item Class: Bows\nRarity: Rare\nDeath Whisper\nSpine Bow\n".into(),
                ..Default::default()
            },
            quotes: Vec::new(),
            total: 0,
        };

        let json = serde_json::to_string(&result).unwrap();
        let back: PriceCheckResult = serde_json::from_str(&json).unwrap();

        assert_eq!(back.item.game, Game::Poe1);
        assert_eq!(back.item.rarity, Some(Rarity::Rare));
        assert_eq!(back.item.name.as_deref(), Some("Death Whisper"));
        assert_eq!(back.item.base_type, "Spine Bow");
        assert!(back.quotes.is_empty());
    }
}
