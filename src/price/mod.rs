//! Pricing sources. Each source turns a parsed item into zero or more quotes.

pub mod ninja;
pub mod poeprices;
pub mod ratelimit;
pub mod trade;

use serde::{Deserialize, Serialize};

use crate::item::ParsedItem;

/// A single price data point for an item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceQuote {
    pub amount: f64,
    pub currency: String,
    /// Which source produced this quote (e.g. "poe.ninja", "trade").
    pub source: String,
}

/// A source of price information for parsed items.
pub trait PriceSource {
    /// Human-readable source name.
    fn name(&self) -> &'static str;

    /// Look up prices for `item`. Returns an empty vec when the source simply
    /// has no data for this item kind (which is not an error).
    fn price(&self, item: &ParsedItem) -> anyhow::Result<Vec<PriceQuote>>;
}
