//! Pricing: the official trade/exchange APIs plus poe.ninja reference values.

pub mod ninja;
pub mod ratelimit;
pub mod trade;

use serde::{Deserialize, Serialize};

/// A readable error for a non-2xx API response: the JSON `error.message` when
/// present, else a truncated body snippet.
pub(crate) fn api_error(status: u16, body: &mut ureq::Body) -> String {
    let text = body.read_to_string().unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| text.chars().take(120).collect());
    format!("HTTP {status}: {message}")
}

/// A single price data point for an item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceQuote {
    pub amount: f64,
    pub currency: String,
    /// Which source produced this quote (e.g. "trade", "bulk").
    pub source: String,
}
