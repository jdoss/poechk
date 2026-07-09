//! poe.ninja economy pricing for uniques, currency, cards, and gems.
//!
//! Implemented in milestone M3. Fetches the economy overview blob and looks
//! items up by their poe.ninja details id; also supplies currency conversion.

use crate::item::ParsedItem;
use crate::price::{PriceQuote, PriceSource};

/// Prices items from poe.ninja's economy overview.
#[derive(Debug, Default)]
pub struct NinjaSource;

impl PriceSource for NinjaSource {
    fn name(&self) -> &'static str {
        "poe.ninja"
    }

    fn price(&self, _item: &ParsedItem) -> anyhow::Result<Vec<PriceQuote>> {
        anyhow::bail!("poe.ninja source not implemented yet (milestone M3)")
    }
}
