//! poeprices.info machine-learning price prediction for rare items.
//!
//! Implemented in milestone M3. Sends the base64-encoded item text and returns
//! a predicted min/max with a confidence score. Needs no stat mapping.

use crate::item::ParsedItem;
use crate::price::{PriceQuote, PriceSource};

/// Prices rare items via poeprices.info's ML endpoint.
#[derive(Debug, Default)]
pub struct PoepricesSource;

impl PriceSource for PoepricesSource {
    fn name(&self) -> &'static str {
        "poeprices.info"
    }

    fn price(&self, _item: &ParsedItem) -> anyhow::Result<Vec<PriceQuote>> {
        anyhow::bail!("poeprices.info source not implemented yet (milestone M3)")
    }
}
