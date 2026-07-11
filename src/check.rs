//! `poechk check`: read the clipboard, parse the item, and show the overlay.
//!
//! Daemonless: `check` does the work in-process and hands the parsed item to the
//! interactive overlay, which runs the trade searches.

use anyhow::Context;

use crate::{config, item, overlay};

/// Read the clipboard, parse the item, and show the interactive price overlay.
pub fn run() -> anyhow::Result<()> {
    let cfg = config::load()?;
    let text = read_clipboard()?;
    let stats = crate::data::load_stats();
    let items = crate::data::load_items();

    let parsed = match item::parse::parse_item(&text, cfg.game, &stats, &items) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                "clipboard is not a Path of Exile item ({e}); hover an item and press Ctrl+Alt+C first"
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

    overlay::show(parsed, cfg)
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
