//! Pricing: the official trade/exchange APIs plus poe.ninja reference values.

pub mod ninja;
pub mod ratelimit;
pub mod trade;

use serde::{Deserialize, Serialize};

/// A single price data point for an item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceQuote {
    pub amount: f64,
    pub currency: String,
    /// Which source produced this quote (e.g. "trade", "bulk").
    pub source: String,
}
