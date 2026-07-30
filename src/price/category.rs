//! Mapping an item's class onto the trade site's `type_filters` category.
//!
//! Searching one base type is often too narrow to price anything: a rare
//! Ambusher may have a handful of listings while daggers as a class have
//! thousands, and the rest of the filters do the real discriminating. Searching
//! the class instead trades an exact base for a usable sample.
//!
//! Only classes where that trade is sound are mapped. A map, a flask, a cluster
//! jewel or a heist contract is defined by its base — "any map" prices nothing —
//! so those fall back to the base type and are deliberately absent below.

use crate::item::ParsedItem;

/// The trade `type_filters.category` option to search this item's class by, or
/// `None` when the base type is the more useful search.
///
/// Categories come from the vendored item data; `item_class` breaks the one tie
/// that data cannot, since thrusting swords trade as their own class.
pub fn trade_category(item: &ParsedItem) -> Option<&'static str> {
    let category = item.category.as_deref()?;
    let option = match category {
        // Rapiers sit in the same vendored category as ordinary one-handed
        // swords but trade separately, and only the class line tells them apart.
        "One-Handed Sword" if item.item_class == "Thrusting One Hand Swords" => "weapon.rapier",
        "One-Handed Sword" => "weapon.basesword",
        "One-Handed Axe" => "weapon.oneaxe",
        "One-Handed Mace" => "weapon.onemace",
        "Two-Handed Sword" => "weapon.twosword",
        "Two-Handed Axe" => "weapon.twoaxe",
        "Two-Handed Mace" => "weapon.twomace",
        // The vendored data already splits the pairs the trade site calls
        // "base" and its variant, so each maps to the narrow option.
        "Dagger" => "weapon.basedagger",
        "Rune Dagger" => "weapon.runedagger",
        "Staff" => "weapon.basestaff",
        "Warstaff" => "weapon.warstaff",
        "Bow" => "weapon.bow",
        "Claw" => "weapon.claw",
        "Sceptre" => "weapon.sceptre",
        "Wand" => "weapon.wand",
        "Fishing Rod" => "weapon.rod",
        "Body Armour" => "armour.chest",
        "Boots" => "armour.boots",
        "Gloves" => "armour.gloves",
        "Helmet" => "armour.helmet",
        "Shield" => "armour.shield",
        "Quiver" => "armour.quiver",
        "Amulet" => "accessory.amulet",
        "Belt" => "accessory.belt",
        "Ring" => "accessory.ring",
        "Trinket" => "accessory.trinket",
        "Jewel" => "jewel.base",
        "Abyss Jewel" => "jewel.abyss",
        _ => return None,
    };
    Some(option)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(category: &str, item_class: &str) -> ParsedItem {
        ParsedItem {
            category: Some(category.to_string()),
            item_class: item_class.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn gear_classes_map_to_their_trade_category() {
        assert_eq!(trade_category(&item("Dagger", "Daggers")), Some("weapon.basedagger"));
        assert_eq!(trade_category(&item("Body Armour", "Body Armours")), Some("armour.chest"));
        assert_eq!(trade_category(&item("Ring", "Rings")), Some("accessory.ring"));
        assert_eq!(trade_category(&item("Abyss Jewel", "Abyss Jewels")), Some("jewel.abyss"));
    }

    #[test]
    fn variant_weapon_classes_stay_out_of_their_base_class() {
        // A rune dagger's spell implicit prices it apart from a plain dagger,
        // so neither search may widen into the other.
        assert_eq!(trade_category(&item("Rune Dagger", "Rune Daggers")), Some("weapon.runedagger"));
        assert_eq!(trade_category(&item("Warstaff", "Warstaves")), Some("weapon.warstaff"));
        assert_eq!(trade_category(&item("Staff", "Staves")), Some("weapon.basestaff"));
    }

    #[test]
    fn only_the_class_line_separates_a_rapier_from_a_one_handed_sword() {
        let rapier = item("One-Handed Sword", "Thrusting One Hand Swords");
        let sabre = item("One-Handed Sword", "One Hand Swords");

        assert_eq!(trade_category(&rapier), Some("weapon.rapier"));
        assert_eq!(trade_category(&sabre), Some("weapon.basesword"));
        // A missing class line is common in older copy formats; falling back to
        // the ordinary sword class beats refusing to search at all.
        assert_eq!(trade_category(&item("One-Handed Sword", "")), Some("weapon.basesword"));
    }

    #[test]
    fn classes_defined_by_their_base_have_no_category() {
        for category in ["Map", "Flask", "Cluster Jewel", "Heist Contract", "Sanctum Relic"] {
            assert_eq!(trade_category(&item(category, "")), None, "{category} must search by base");
        }
    }

    #[test]
    fn an_unresolved_base_type_has_no_category() {
        assert_eq!(trade_category(&ParsedItem::default()), None);
    }
}
