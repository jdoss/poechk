//! Turns a printed mod line into a template + rolls and resolves its trade stat-id.
//!
//! Two clipboard formats are supported:
//! * **Standard** (Ctrl+C): the mod type is a trailing ` (implicit)` /
//!   ` (crafted)` / … suffix.
//! * **Advanced** (Ctrl+Alt+C): each mod is preceded by a `{ … Modifier … }`
//!   info line carrying the type; the mod line has `(min-max)` roll annotations
//!   and may end in ` — Unscalable Value` metadata (an em-dash tail).
//!
//! Some stats keep a literal number in their text (e.g. "per 10 Dexterity"), so
//! matching tries the fully-templated form and variants that keep one value.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::data::StatIndex;

/// A rolled value, optionally followed by an advanced `(min-max)` annotation.
static VALUE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([+-]?\d+(?:\.\d+)?)(?:\([^)]*\))?").unwrap());

/// Which affix bucket a mod line belongs to, matching the trade `ids` keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModType {
    Explicit,
    Implicit,
    Crafted,
    Enchant,
    Fractured,
    Scourge,
    Veiled,
}

impl ModType {
    /// The key used in a stat's `trade.ids` map.
    pub fn trade_key(self) -> &'static str {
        match self {
            ModType::Explicit => "explicit",
            ModType::Implicit => "implicit",
            ModType::Crafted => "crafted",
            ModType::Enchant => "enchant",
            ModType::Fractured => "fractured",
            ModType::Scourge => "scourge",
            ModType::Veiled => "veiled",
        }
    }

    /// Split a trailing ` (implicit)`/… marker off a mod line. Returns
    /// `Some(type)` only when a marker was found (standard clipboard format).
    pub fn from_suffix(line: &str) -> (Option<ModType>, &str) {
        const SUFFIXES: [(&str, ModType); 6] = [
            (" (implicit)", ModType::Implicit),
            (" (crafted)", ModType::Crafted),
            (" (enchant)", ModType::Enchant),
            (" (fractured)", ModType::Fractured),
            (" (scourge)", ModType::Scourge),
            (" (veiled)", ModType::Veiled),
        ];
        for (suffix, ty) in SUFFIXES {
            if let Some(stripped) = line.strip_suffix(suffix) {
                return (Some(ty), stripped.trim_end());
            }
        }
        (None, line)
    }

    /// Read the mod type and affix slot from an advanced `{ … Modifier … }`
    /// info line. Returns `None` for anything that is not such a line.
    pub fn from_info_line(line: &str) -> Option<(ModType, Option<Slot>)> {
        let inner = line.strip_prefix('{')?;
        let ty = if inner.contains("Implicit") {
            ModType::Implicit
        } else if inner.contains("Fractured") {
            ModType::Fractured
        } else if inner.contains("Crafted") {
            ModType::Crafted
        } else if inner.contains("Enchant") {
            ModType::Enchant
        } else {
            ModType::Explicit
        };
        let slot = if inner.contains("Prefix") {
            Some(Slot::Prefix)
        } else if inner.contains("Suffix") {
            Some(Slot::Suffix)
        } else {
            None
        };
        Some((ty, slot))
    }
}

/// Whether an affix occupies a prefix or suffix slot (known only from the
/// advanced clipboard format's `{ … }` info lines).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Slot {
    Prefix,
    Suffix,
}

/// A parsed affix resolved to its trade stat-id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedMod {
    /// The mod line without its type marker or metadata tail.
    pub text: String,
    pub mod_type: ModType,
    /// Prefix/suffix slot, when the advanced format's info line named it.
    #[serde(default)]
    pub slot: Option<Slot>,
    /// The matched template (rolls replaced by `#`).
    pub template: String,
    /// The numeric rolls, in order.
    pub rolls: Vec<f64>,
    /// Canonical English stat reference.
    pub stat_ref: String,
    /// Trade stat-ids for this mod type.
    pub trade_ids: Vec<String>,
    /// Plain-explicit ids, kept so special types (fractured/crafted/…) can
    /// optionally be searched as ordinary explicit mods.
    #[serde(default)]
    pub explicit_ids: Vec<String>,
}

impl ParsedMod {
    /// A single representative roll: the average of the rolls (so `Adds 10 to
    /// 20` searches on 15), or `None` for a flag mod with no numbers.
    pub fn roll(&self) -> Option<f64> {
        if self.rolls.is_empty() {
            None
        } else {
            Some(self.rolls.iter().sum::<f64>() / self.rolls.len() as f64)
        }
    }
}

/// The `(start, end, value)` of each rolled value (and any annotation) in `line`.
fn value_matches(line: &str) -> Vec<(usize, usize, f64)> {
    VALUE_RE
        .captures_iter(line)
        .filter_map(|caps| {
            let whole = caps.get(0)?;
            let value = caps.get(1)?.as_str().parse::<f64>().ok()?;
            Some((whole.start(), whole.end(), value))
        })
        .collect()
}

/// Build a template from `line`, replacing each value with `#` unless its index
/// is in `keep_literal`. Returns the template and the templated (rolled) values.
fn build_template(
    line: &str,
    nums: &[(usize, usize, f64)],
    keep_literal: &[usize],
) -> (String, Vec<f64>) {
    let mut result = String::new();
    let mut rolls = Vec::new();
    let mut last = 0;
    for (idx, &(start, end, value)) in nums.iter().enumerate() {
        result.push_str(&line[last..start]);
        if keep_literal.contains(&idx) {
            result.push_str(&line[start..end]);
        } else {
            result.push('#');
            rolls.push(value);
        }
        last = end;
    }
    result.push_str(&line[last..]);
    (result, rolls)
}

/// Replace every rolled value (sign and any `(min-max)` annotation included)
/// with `#`, returning the template and the extracted rolls.
pub fn templatize(line: &str) -> (String, Vec<f64>) {
    build_template(line, &value_matches(line), &[])
}

/// Candidate templates for a mod line, most-templated first: all values as `#`,
/// then variants keeping one value literal (for stats like "per 10 Dexterity").
fn candidates(line: &str) -> Vec<(String, Vec<f64>)> {
    let nums = value_matches(line);
    if nums.is_empty() {
        return vec![(line.to_string(), Vec::new())];
    }
    let mut out = vec![build_template(line, &nums, &[])];
    for i in 0..nums.len() {
        out.push(build_template(line, &nums, &[i]));
    }
    out
}

/// Strip a trailing ` — <metadata>` tail (em-dash), e.g. "— Unscalable Value".
fn strip_metadata(line: &str) -> &str {
    line.split('\u{2014}').next().unwrap_or(line).trim()
}

/// Resolve a printed mod line to a [`ParsedMod`], or `None` if unknown.
///
/// `context` is the (type, slot) from a preceding advanced `{ … }` info line,
/// used when the line has no ` (type)` suffix of its own. `category` is the
/// item's category ("Body Armour", "Bow", …), used to pick the right variant
/// when the same text is a local stat on some gear and global elsewhere.
pub fn parse_mod(
    line: &str,
    stats: &StatIndex,
    context: Option<(ModType, Option<Slot>)>,
    category: Option<&str>,
) -> Option<ParsedMod> {
    let (suffix_type, rest) = ModType::from_suffix(line.trim());
    let body = strip_metadata(rest);
    let (context_type, slot) = context.map_or((None, None), |(ty, slot)| (Some(ty), slot));
    let mod_type = suffix_type.or(context_type).unwrap_or(ModType::Explicit);
    let key = mod_type.trade_key();

    // Try the raw body (flag / singular-value matchers) then templated variants.
    let raw = (body.to_string(), Vec::new());
    for (candidate, candidate_rolls) in std::iter::once(raw).chain(candidates(body)) {
        let viable: Vec<&crate::data::StatMatch> = stats
            .lookup(&candidate)
            .iter()
            .filter(|stat| stat.trade_ids.contains_key(key))
            .collect();
        // Prefer the variant scoped to this item's category (e.g. the local
        // "+# to Armour" on armour pieces), then fall back to the default.
        let chosen = viable
            .iter()
            .find(|stat| match (&stat.category_test, category) {
                (Some(test), Some(category)) => crate::data::category_matches(test, category),
                _ => false,
            })
            .or_else(|| viable.iter().find(|stat| stat.category_test.is_none()))
            .copied();
        if let Some(stat) = chosen {
            let ids = &stat.trade_ids[key];
            let rolls = if candidate_rolls.is_empty() {
                stat.value.map(|v| vec![v]).unwrap_or_default()
            } else {
                candidate_rolls.clone()
            };
            let explicit_ids = if key == "explicit" {
                ids.clone()
            } else {
                stat.trade_ids.get("explicit").cloned().unwrap_or_default()
            };
            return Some(ParsedMod {
                text: body.to_string(),
                mod_type,
                slot,
                template: candidate.clone(),
                rolls,
                stat_ref: stat.stat_ref.clone(),
                trade_ids: ids.clone(),
                explicit_ids,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::load_stats;

    #[test]
    fn templatize_folds_sign_and_annotations() {
        assert_eq!(
            templatize("+45 to Dexterity"),
            ("# to Dexterity".to_string(), vec![45.0])
        );
        assert_eq!(
            templatize("Adds 96(81-111) to 170(163-189) Cold Damage"),
            ("Adds # to # Cold Damage".to_string(), vec![96.0, 170.0])
        );
    }

    #[test]
    fn candidates_include_keep_one_literal() {
        let cands = candidates("5% increased Attack Speed per 10 Dexterity");
        assert!(
            cands
                .iter()
                .any(|(t, r)| t == "#% increased Attack Speed per # Dexterity"
                    && *r == vec![5.0, 10.0])
        );
        assert!(
            cands
                .iter()
                .any(|(t, r)| t == "#% increased Attack Speed per 10 Dexterity" && *r == vec![5.0])
        );
    }

    #[test]
    fn from_info_line_reads_type_and_slot() {
        assert_eq!(
            ModType::from_info_line("{ Fractured Prefix Modifier \"Crystalising\" }"),
            Some((ModType::Fractured, Some(Slot::Prefix)))
        );
        assert_eq!(
            ModType::from_info_line("{ Master Crafted Prefix Modifier \"Upgraded\" }"),
            Some((ModType::Crafted, Some(Slot::Prefix)))
        );
        assert_eq!(
            ModType::from_info_line("{ Suffix Modifier \"of the Essence\" }"),
            Some((ModType::Explicit, Some(Slot::Suffix)))
        );
        assert_eq!(ModType::from_info_line("+10 to Strength"), None);
    }

    #[test]
    fn resolves_explicit_and_implicit_to_trade_ids() {
        let stats = load_stats();

        let ex = parse_mod("+45 to Dexterity", &stats, None, None).expect("explicit resolves");
        assert_eq!(ex.mod_type, ModType::Explicit);
        assert_eq!(ex.roll(), Some(45.0));
        assert!(ex.trade_ids.contains(&"explicit.stat_3261801346".to_string()));

        let im =
            parse_mod("+45 to Dexterity (implicit)", &stats, None, None).expect("implicit resolves");
        assert_eq!(im.mod_type, ModType::Implicit);
        assert!(im.trade_ids.contains(&"implicit.stat_3261801346".to_string()));
    }

    #[test]
    fn advanced_context_types_and_strips_metadata() {
        let stats = load_stats();
        let m = parse_mod(
            "Hits can't be Evaded \u{2014} Unscalable Value",
            &stats,
            Some((ModType::Crafted, Some(Slot::Prefix))),
            None,
        )
        .expect("crafted resolves");
        assert_eq!(m.mod_type, ModType::Crafted);
        assert_eq!(m.slot, Some(Slot::Prefix));
        assert_eq!(m.text, "Hits can't be Evaded");
        assert!(m.trade_ids.iter().any(|id| id.starts_with("crafted.")));
    }

    #[test]
    fn unknown_mod_returns_none() {
        let stats = load_stats();
        assert!(parse_mod("Totally Not A Real Mod", &stats, None, None).is_none());
    }

    #[test]
    fn picks_local_variant_on_matching_category() {
        let stats = load_stats();

        // On a body armour, "+# to Armour" is the LOCAL defence stat…
        let local = parse_mod("+326 to Armour", &stats, None, Some("Body Armour"))
            .expect("flat armour resolves");
        assert!(
            local.trade_ids.contains(&"explicit.stat_3484657501".to_string()),
            "expected local armour id, got {:?}",
            local.trade_ids
        );

        // …while with no category (e.g. a jewel-ish context) it's the global one.
        let global =
            parse_mod("+326 to Armour", &stats, None, None).expect("flat armour resolves");
        assert!(
            global.trade_ids.contains(&"explicit.stat_809229260".to_string()),
            "expected global armour id, got {:?}",
            global.trade_ids
        );
    }
}
