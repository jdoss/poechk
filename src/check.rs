//! `poechk check`: read the clipboard, parse the item, and show the overlay.
//!
//! This is the daemonless M1 path — no persistent state yet, so `check` does
//! the whole job in-process. When pricing arrives (M3) this forwards to the
//! daemon instead.

use anyhow::Context;

use crate::price::PriceQuote;
use crate::result::PriceCheckResult;
use crate::{config, item, overlay};

/// Read the clipboard, parse the item, and show the price-check overlay.
pub fn run() -> anyhow::Result<()> {
    let cfg = config::load()?;
    let text = read_clipboard()?;

    let stats = crate::data::load_stats();
    let items = crate::data::load_items();
    let parsed = match item::parse::parse_item(&text, cfg.game, &stats, &items) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                "clipboard is not a Path of Exile item ({e}); hover an item and press Ctrl+C first"
            );
            return Ok(());
        }
    };
    tracing::info!(
        class = %parsed.item_class,
        base = %parsed.base_type,
        mods = parsed.mods.len(),
        "parsed item"
    );

    let (total, quotes) = price_item(&cfg, &parsed);
    overlay::show(PriceCheckResult {
        item: parsed,
        quotes,
        total,
    })
}

/// Price an item via the trade API, returning the total match count and cheapest
/// listings (empty on error so the overlay still shows the parsed item).
fn price_item(
    cfg: &crate::config::Config,
    item: &crate::item::ParsedItem,
) -> (u32, Vec<PriceQuote>) {
    let source = crate::price::trade::TradeSource::new(cfg);
    match source.price(item) {
        Ok(result) => result,
        Err(e) => {
            tracing::warn!("trade pricing failed: {e}");
            (0, Vec::new())
        }
    }
}

/// Read the current clipboard text via `wl-paste`.
fn read_clipboard() -> anyhow::Result<String> {
    let output = std::process::Command::new("wl-paste")
        .args(["--no-newline", "--type", "text/plain"])
        .output()
        .context("running wl-paste (is wl-clipboard installed?)")?;
    if !output.status.success() {
        anyhow::bail!(
            "wl-paste returned no text — the clipboard is empty, or COSMIC needs \
             COSMIC_DATA_CONTROL_ENABLED=1 for clipboard access"
        );
    }
    String::from_utf8(output.stdout).context("clipboard text was not valid UTF-8")
}
