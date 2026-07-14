# poechk — design & roadmap

> **Status (2026-07):** M0–M5 shipped. The daemon described below was never
> needed — `check` does everything in-process, and the poe.ninja disk cache
> plus the file-based rate limiter cover the state the daemon was meant to
> hold. poeprices.info was dropped in favor of poe.ninja (whose currency rates
> come from the in-game exchange). This document is otherwise kept as written
> for design history; README.md describes the tool as it exists.

A Path of Exile price-check overlay for the **COSMIC** desktop, in the spirit of
[Awakened PoE Trade][apt] but native to Wayland/COSMIC and written in Rust — no
Electron, no TypeScript. It reuses the process architecture of [clipbro][clipbro].

[apt]: https://github.com/SnosMe/awakened-poe-trade
[clipbro]: https://github.com/jdoss/clipbro

## Building

Build with the **rustup** toolchain, not Fedora's `/usr/bin/cargo`. The distro
rustc mis-detects as nightly-capable and the dependency build fails with `E0554`.
`rust-toolchain.toml` pins `stable`; ensure `~/.cargo/bin` precedes `/usr/bin`
on `PATH` (or call `~/.cargo/bin/cargo` directly).

```
cargo test    # via the rustup shim
```

## Decisions (2026-07-05)

- **Both games**, behind a small game-abstraction seam; one implemented first.
  Awakened PoE Trade is **PoE1-only** and MIT-licensed, so PoE1 has a complete,
  verified spec plus vendorable data. PoE2 data + `/api/trade2` endpoints come
  from the [`Exiled-Exchange-2`](https://github.com/Kvan7/Exiled-Exchange-2) fork.
  **PoE1 first (confirmed)** — targeting the new league on **2026-07-24**; PoE2
  follows at M5.
- **The trade API is the core.** The real workflow is interactive: price-check
  → see live trade listings → adjust affix filter min/max (or toggle affixes) →
  re-search. So the official pathofexile.com **trade API plus an adjustable
  stat-filter overlay** is the heart of poechk. poe.ninja (economy averages) and
  poeprices.info (one-shot rare ML estimate) are **secondary** — they can't do
  the adjust-and-re-search loop.
- **Manual Ctrl+C first**: you copy the item in-game, then trigger the hotkey;
  the overlay reads the clipboard. Synthetic input is a later, optional add.

## Architecture

One `poechk` binary plays three roles, chosen by subcommand. This mirrors
clipbro and is also the shape the COSMIC-overlay research independently landed on.

```
 COSMIC custom shortcut ──Spawn──> `poechk check` ──D-Bus──> poechk daemon
                                                                  │
   (long-lived: config, price cache, rate limiters, D-Bus svc)    │ spawns
                                                                  ▼
                                                          `poechk overlay`
                                        (layer-shell surface; renders; exits)
```

- **daemon** — long-lived; owns config, the price cache, and trade-API
  rate-limiter state; exposes a D-Bus service; spawns the overlay child.
- **overlay** — short-lived layer-shell surface (libcosmic's iced fork,
  `get_layer_surface`, `Layer::Overlay` so it floats over a fullscreen game);
  renders the price card near the cursor; exits on Escape / focus loss.
- **check** — the thin CLI bound to a COSMIC keyboard shortcut; one D-Bus call.

### Price-check flow

```
shortcut → `poechk check` → daemon:
  1. get item text onto the clipboard   (manual Ctrl+C now; injected later)
  2. poll clipboard until it starts with "Item Class: "   (~48ms poll)
  3. parse item text  → ParsedItem
  4. query the enabled price sources
  5. spawn `poechk overlay` with the result → floats near the cursor
```

### Why this shape on COSMIC

- **Global hotkey**: COSMIC has no XDG GlobalShortcuts portal (2026). The
  supported path is a COSMIC custom shortcut (RON at
  `~/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1/custom`) that
  `Spawn`s `poechk check`. It fires over XWayland games unless the game holds an
  exclusive-fullscreen keyboard grab → evdev fallback if needed (see Risks).
- **Overlay**: cosmic-comp implements wlr-layer-shell; libcosmic exposes
  `get_layer_shell`. `Layer::Overlay` renders above fullscreen surfaces.
- **Clipboard**: `wl-clipboard-rs` reads focus-independently but requires
  `COSMIC_DATA_CONTROL_ENABLED=1`; alternatively shell out to `wl-paste` as
  clipbro does.

## Module layout

| path | role | status |
|------|------|--------|
| `src/main.rs` | clap dispatch → `daemon` \| `overlay` \| `check` | M0 ✅ |
| `src/config.rs` | TOML config under XDG (game, league, realm, POESESSID) | M0 ✅ |
| `src/item/mod.rs` | `Game`, `Rarity`, `ParsedItem` model | M0 ✅ |
| `src/item/parse.rs` | section splitter + name-plate parser (+ tests) | M0 ✅ |
| `src/price/mod.rs` | `PriceSource` trait, `PriceQuote` | M0 ✅ |
| `src/price/ninja.rs` | poe.ninja economy source | M3 |
| `src/price/poeprices.rs` | poeprices.info ML source | M3 |
| `src/price/trade.rs` | official trade API source | M4 |
| `src/daemon.rs` | daemon loop, D-Bus service, overlay spawn | M1 |
| `src/overlay.rs` | layer-shell surface (libcosmic/iced) | M1 |
| `src/ipc.rs` | `poechk check` D-Bus client | M1 |

## Game abstraction

A `Game` enum plus a per-game profile providing: item-text label strings, trade
endpoints (`/api/trade` vs `/api/trade2`, host by realm), the poe.ninja slug, and
the data-file set. The parser and pricing sources are written against traits, so
adding the second game is a new profile + data, not a fork.

## Item parsing (port of the APT parser)

- Split on lines of exactly `--------`; the first section is the name plate
  (`Item Class:`, optional `Rarity:`, then name/base). — **done (M0)**
- A section-parser pipeline keyed on localized label prefixes; we hardcode
  English strings (APT parameterizes these via `client_strings.js`). — M2
- Mods: replace numeric rolls with `#` to form a template, look it up in
  `stats.ndjson` (fnv1a-32 hash + binary search) to get the trade stat id; two
  input shapes (advanced mod descriptions on/off). — M2/M4
- The Requirements block is intentionally discarded (matches APT).

## Data files

Vendored from APT `en/` (MIT; derived from GGG data via [RePoE]):

- `stats.ndjson` (~6960 lines) — mod text → trade stat id, keyed by mod type
  (explicit/implicit/fractured/enchant/crafted/pseudo/veiled).
- `items.ndjson` (~4641 lines) — base types, uniques, gems, cards, beasts;
  includes `tradeTag` for bulk exchange and poe.ninja lookup hints.

A companion fnv index is built at load. PoE2 data comes from Exiled-Exchange-2.
Attribution goes in a `NOTICE` file when the data lands (M2).

[RePoE]: https://github.com/brather1ng/RePoE

## Pricing sources

### poe.ninja (M3) — uniques, currency, cards, gems

`GET https://poe.ninja/poe1/api/economy/current/dense/overviews?league={league}&language=en`
— one economy blob; look up by a computed details id. Also supplies
Divine/Exalt→chaos conversion. No stat mapping needed.

### poeprices.info (M3) — rare-item ML prediction

`GET https://www.poeprices.info/api?i={base64url(itemText)}&l={league}&s=poechk`
→ `{ currency, min, max, pred_confidence_score, pred_explanation, error }`.
No stat mapping needed.

### Official trade API (M4) — real listings, any item

- Search: `POST https://{host}/api/trade/search/{league}` (status + name/type +
  stat filters) → an ordered array of listing ids (price asc).
- Fetch: `GET https://{host}/api/trade/fetch/{id1,…}?query={searchId}` — **≤10
  ids per call** → listing details (price, account, indexed date).
- Bulk currency: `POST https://{host}/api/trade/exchange/{league}` for items
  with a `tradeTag`.
- Host by realm/language: `en → www.pathofexile.com` (ru/tw/kr variants exist).
- League list: `GET https://{host}/api/leagues?type=main&realm=pc`.
- Optional `Cookie: POESESSID=…` raises rate limits; unauthenticated works.

This source needs the stat→trade-id mapping and the rate limiter, so it is the
largest milestone.

### Rate limiting (trade API)

Sliding-window token buckets; default 1 request / 5 s per endpoint. Read the
`x-rate-limit-rules`, `x-rate-limit-<rule>`, and `x-rate-limit-<rule>-state`
response headers to rebuild buckets dynamically; add `api_latency_seconds` of
margin to each window; **fail fast** (don't queue) when a request would wait too
long; cache search + fetch responses by a TTL derived from the windows.

## Planned dependency stack

Current (M0): `clap`, `serde`, `toml`, `directories`, `thiserror`, `anyhow`,
`tracing`, `tracing-subscriber`. Added at their milestones:

| concern | crate | milestone |
|---------|-------|-----------|
| async runtime | `tokio` | M1 |
| D-Bus service + client | `zbus` | M1 |
| layer-shell overlay | `libcosmic` (git, `wayland` feature) | M1 |
| clipboard read | `wl-clipboard-rs` (or `wl-paste` subprocess) | M1 |
| HTTP | `ureq` (blocking, matches clipbro) | M3 |
| JSON | `serde_json` | M3 |
| mod-template regex | `regex` | M2 |
| synthetic input (opt.) | virtual-keyboard / `ydotool` / XTest | M6 |
| hotkey fallback (opt.) | `evdev` | M6 |

## Risks (COSMIC-specific)

1. **Hotkey while a game holds a keyboard grab** — a COSMIC shortcut is defeated
   by an exclusive-fullscreen keyboard grab or shortcut-inhibit. Mitigate: run
   borderless-windowed; add an `evdev` fallback. *Most likely to bite.*
2. **Synthetic Ctrl+C into the XWayland game** — deferred by the manual-copy
   decision; virtual-keyboard has regressed on cosmic-comp before.
3. **libcosmic has no stable release** — pin a git rev; expect occasional churn.
4. **Clipboard needs `COSMIC_DATA_CONTROL_ENABLED=1`** (or the focus-dependent
   read path). Document as user setup.

## Milestones

- **M0 — Scaffold** ✅ CLI dispatch, item model + name-plate parser (tested),
  module stubs, config, this doc.
- **M1 — Overlay + IPC end-to-end** port clipbro's layer-shell overlay
  (`Layer::Overlay`) and D-Bus service; `poechk check` → daemon → spawn overlay
  showing a card built from the manually-copied clipboard item.
- **M2 — Full item parser + data** port the section pipeline; vendor + index
  `stats.ndjson` / `items.ndjson`; complete `ParsedItem` for PoE1.
- **M3 — poe.ninja + poeprices** economy lookup for uniques/currency/cards and
  ML prediction for rares; render real prices.
- **M4 — Official trade API** stat→trade-id mapping, search + fetch, the
  sliding-window rate limiter, and response caching.
- **M5 — PoE2 profile** second game profile: `/api/trade2`, Exiled-Exchange-2
  data, PoE2 item-format deltas.
- **M6 — Polish** config UI in the overlay, systemd self-install (from clipbro),
  optional synthetic Ctrl+C, evdev hotkey fallback, packaging.

## Credits

- [Awakened PoE Trade][apt] (MIT) — parsing pipeline and data files.
- [clipbro][clipbro] — the daemon/overlay/CLI + layer-shell skeleton.
- [RePoE] — provenance of the stat/item data.
