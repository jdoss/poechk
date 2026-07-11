//! `poechk check`: read the clipboard, parse the item, and show the overlay.
//!
//! Daemonless: `check` does the work in-process and hands the parsed item to the
//! interactive overlay, which runs the trade searches. With `--copy` it first
//! injects Ctrl+Alt+C into the focused game (via wtype / the virtual-keyboard
//! protocol) so one hotkey does copy + price check, like Awakened PoE Trade.

use anyhow::Context;

use crate::{config, item, overlay};

/// Sentinel seeded onto the clipboard so the game's copy is detectable.
const COPY_SENTINEL: &str = "POECHK_AWAITING_ITEM_COPY";

/// Price-check an item: optionally copy it from the game first, then parse the
/// clipboard and show the interactive overlay.
pub fn run(copy: bool) -> anyhow::Result<()> {
    let cfg = config::load()?;
    let text = if copy { capture_item_text()? } else { read_clipboard()? };
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

/// Copy the hovered item from the game, waiting for it to land on the clipboard.
///
/// Seeds the clipboard with a sentinel (so a stale item can't be mistaken for
/// the new copy), fakes Ctrl+Alt+C into the still-focused game via XTest, then
/// polls the clipboard briefly — the same approach Awakened PoE Trade uses.
fn capture_item_text() -> anyhow::Result<String> {
    let seeded = std::process::Command::new("wl-copy")
        .arg(COPY_SENTINEL)
        .status()
        .context("running wl-copy (is wl-clipboard installed?)")?;
    anyhow::ensure!(seeded.success(), "wl-copy failed to seed the clipboard");

    crate::inject::send_copy_chord().context("injecting Ctrl+Alt+C into the game")?;

    for _ in 0..16 {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if let Some(text) = clipboard_text()
            && text != COPY_SENTINEL
            && looks_like_item(&text)
        {
            return Ok(text);
        }
    }
    anyhow::bail!("timed out waiting for the item copy — is the game focused with an item hovered?")
}

/// Whether clipboard text is plausibly a PoE item (cheap pre-parse check).
fn looks_like_item(text: &str) -> bool {
    text.starts_with("Item Class: ") || text.starts_with("Rarity: ")
}

/// The current clipboard text, or `None` if it is empty/unreadable.
fn clipboard_text() -> Option<String> {
    let output = std::process::Command::new("wl-paste")
        .args(["--no-newline", "--type", "text/plain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// Read the current clipboard text via `wl-paste`, failing loudly.
fn read_clipboard() -> anyhow::Result<String> {
    clipboard_text().context(
        "could not read the clipboard — it is empty, or COSMIC needs \
         COSMIC_DATA_CONTROL_ENABLED=1 for clipboard access",
    )
}
