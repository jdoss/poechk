# Updating poechk for a new patch

poechk carries two vendored data files plus one derived from them. A patch that
adds or rewords affixes makes them stale, and stale data fails quietly: a mod
line that no longer resolves is simply dropped from the parse, so the affix
vanishes from the overlay and never reaches the search. It reads like a parsing
bug. It is almost always this.

```sh
scripts/update-data.sh
```

That runs everything automatable. The one manual step is the snapshot itself.

## What is vendored

| file | source | refreshed |
|------|--------|-----------|
| `data/poe1/en/stats.ndjson` | Awakened PoE Trade | by hand |
| `data/poe1/en/items.ndjson` | Awakened PoE Trade | by hand |
| `data/poe1/en/tiers.ndjson` | RePoE, joined to the above | `--example vendor-tiers` |

The APT files are a snapshot copied from a tagged release — the last was commit
`18a401e` (v3.29.102), recorded in `NOTICE`. APT derives them from game data and
then *curates*: it corrects stat wording, marks which stats are better rolled
low, and separates the local and global variants that print identically. None of
that is in the raw game data, which is why the snapshot is copied rather than
generated.

`tiers.ndjson` is generated here, from RePoE, and joined to the snapshot **by
stat text**. That join is the fragile part: reword a stat upstream and the
ladder stops resolving without erroring. Always regenerate it after refreshing
the snapshot.

## The steps

1. **Check the drift.** `cargo run --release --example check-data` compares the
   snapshot against every stat the trade site accepts. Read it against the
   baseline below — see [Reading the drift figures](#reading-the-drift-figures).
2. **Refresh the APT snapshot** if the figures jumped. Copy `stats.ndjson` and
   `items.ndjson` from the newest Awakened PoE Trade release into
   `data/poe1/en/`, and update the commit and version in `NOTICE`.
3. **Regenerate the ladder.** `cargo run --release --example vendor-tiers`.
   Watch its join count: it prints how many affixes resolved, and a drop means
   the snapshot reworded something the ladder was keyed on.
4. **Build and test.** Tests pin real stat ids, so a renamed id fails loudly
   here rather than in game.
5. **Record the new figures** in the baseline table below.
6. **Check a few real items** carrying the patch's new affixes, and confirm
   every line resolved. `~/.local/state/poechk/checks.jsonl` records the item,
   the exact search body, and the raw response for each check.

## Reading the drift figures

`check-data` reports, per trade stat group, how many stats the trade site
accepts that the snapshot cannot resolve. **A large number is normal.** APT
curates rather than mirrors — map mods, area mods and minion mods it omits on
purpose will never resolve and should not.

The signal is the number *moving*. These are the figures for the snapshot
currently in tree (APT `18a401e`, v3.29.102), measured 2026-08-02:

| group | live | unmatched |
|-------|------|-----------|
| Explicit | 7894 | 1536 |
| Implicit | 1604 | 97 |
| Fractured | 1820 | 135 |
| Enchant | 2037 | 992 |
| Crafted | 288 | 11 |

The ladder joined **3826** affixes at that snapshot.

A jump in `unmatched`, or a fall in the ladder's join count, means a patch
landed that the snapshot has not caught up with. Groups poechk does not parse
(Pseudo, Imbued, Scourge, Mercenary, Veiled, Delve, Ultimatum, Sanctum,
Crucible) drift on their own and are reported for information only.

## Things that also move with a patch, and are not data

- **Trade categories.** `src/price/category.rs` maps an item's class to the
  trade site's `type_filters.category` option. A new item class needs an entry,
  or that class silently falls back to searching its base type. The live list is
  at `https://www.pathofexile.com/api/trade/data/filters`.
- **The league.** `~/.config/poechk/config.toml`. The overlay's league picker
  refreshes itself from the API, so this usually needs no attention.
- **Influence tag suffixes.** `examples/vendor-tiers.rs` maps the six
  affix-granting influences to RePoE's tag suffixes. A new influence would need
  one, and would be invisible until then.

## Why the snapshot is not generated too

RePoE publishes `stat_translations.json`, which carries both the printed text
variants and the trade ids already joined — enough to build most of
`stats.ndjson` first-party and drop the dependency on APT's release cadence.

What it does not carry is the curation: which stats are better rolled low, the
local/global scoping, and the sign flip for "reduced" wording. Getting the first
of those wrong is silent — a filter seeds a cap where it should seed a floor,
and the search quietly asks for worse items than the one being priced. That is
worth doing deliberately rather than as part of a patch refresh.
