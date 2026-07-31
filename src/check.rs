//! `poechk check`: read the clipboard, parse the item, and show the overlay.
//!
//! Daemonless: `check` does the work in-process and hands the parsed item to the
//! interactive overlay, which runs the trade searches. With `--copy` it first
//! injects Ctrl+Alt+C into the focused game (via XTest, see `inject`) so one
//! hotkey does copy + price check, like Awakened PoE Trade.

use std::time::Duration;

use anyhow::Context;
use serde_json::json;

use crate::{checklog, config, item, overlay};

/// Sentinel seeded onto the clipboard so the game's copy is detectable.
const COPY_SENTINEL: &str = "POECHK_AWAITING_ITEM_COPY";

/// Full copy attempts before giving up. Proton 11 / Wine 11 routinely drops the
/// first copy (see `capture_item_text`), so we retry the whole chord — the same
/// thing pressing the hotkey a second time does by hand.
const COPY_ATTEMPTS: usize = 4;

/// Clipboard reads per attempt, spaced by `POLL_GAP`.
const POLLS_PER_ATTEMPT: usize = 6;

/// Gap between clipboard reads while waiting for the copy to land.
const POLL_GAP: Duration = Duration::from_millis(40);

/// Price-check an item: optionally copy it from the game first, then parse the
/// clipboard and show the interactive overlay.
pub fn run(copy: bool) -> anyhow::Result<()> {
    let cfg = config::load()?;
    let log = checklog::CheckLog::open();
    log.event(
        "check_start",
        json!({
            "version": env!("CARGO_PKG_VERSION"),
            "game": cfg.game,
            "league": cfg.league,
            "copy": copy,
            // Never the value: a POESESSID is a live session cookie.
            "authenticated": cfg.poesessid.is_some(),
        }),
    );
    let text = if copy { capture_item_text()? } else { read_clipboard()? };
    let stats = crate::data::load_stats();
    let items = crate::data::load_items();

    let tiers = crate::item::tiers::load_tiers();

    let mut parsed = match item::parse::parse_item(&text, cfg.game, &stats, &items) {
        Ok(parsed) => parsed,
        Err(e) => {
            log.event("parse_failed", json!({ "clipboard": text, "error": e.to_string() }));
            tracing::warn!(
                "clipboard is not a Path of Exile item ({e}); hover an item and press Ctrl+Alt+C first"
            );
            return Ok(());
        }
    };
    // Fill in tiers the clipboard did not name, from the vendored ladder.
    item::tiers::apply(&mut parsed, &tiers);
    log.event("parsed", json!({ "clipboard": text, "item": parsed }));
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
/// Each attempt seeds the clipboard with a sentinel (so a stale item can't be
/// mistaken for the new copy), fakes Ctrl+Alt+C into the still-focused game via
/// XTest, then polls the clipboard briefly — the approach Awakened PoE Trade uses.
///
/// Proton 11 / Wine 11 regressed the game's clipboard: it commits the copy
/// late (often only once the window loses focus), so the first attempt usually
/// polls out. Re-running the whole chord flushes it — automating the "press the
/// hotkey twice" that works by hand, which no single clipboard poke reproduced.
fn capture_item_text() -> anyhow::Result<String> {
    for _ in 0..COPY_ATTEMPTS {
        seed_clipboard()?;
        crate::inject::send_copy_chord().context("injecting Ctrl+Alt+C into the game")?;
        for _ in 0..POLLS_PER_ATTEMPT {
            std::thread::sleep(POLL_GAP);
            if let Some(text) = clipboard_text()
                && text != COPY_SENTINEL
                && looks_like_item(&text)
            {
                return Ok(text);
            }
        }
    }
    anyhow::bail!(
        "timed out waiting for the item copy. Hover an item with the game focused. \
         If this persists on Proton 11, force Proton 10 (Steam → Properties → \
         Compatibility) — Wine 11 has a clipboard regression that breaks the copy."
    )
}

/// Seed the clipboard with the sentinel, replacing whatever owns it, so the
/// game's copy is distinguishable from a stale item still on the clipboard.
fn seed_clipboard() -> anyhow::Result<()> {
    let seeded = std::process::Command::new("wl-copy")
        .arg(COPY_SENTINEL)
        .status()
        .context("running wl-copy (is wl-clipboard installed?)")?;
    anyhow::ensure!(seeded.success(), "wl-copy failed to seed the clipboard");
    Ok(())
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
