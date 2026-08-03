#!/usr/bin/env bash
#
# Refresh poechk's vendored game data after a Path of Exile patch.
#
# Automates every step that can be: measuring how far the stat snapshot has
# drifted from the live trade API, regenerating the affix tier ladder from it,
# and checking the result still builds and passes. Copying a new Awakened PoE
# Trade snapshot stays manual — see docs/UPDATING.md for why.
#
# Usage: scripts/update-data.sh [--skip-check]

set -euo pipefail

cd "$(dirname "$0")/.."

if [[ ! -f data/poe1/en/stats.ndjson ]]; then
  echo "error: run this from a poechk checkout (data/poe1/en/stats.ndjson missing)" >&2
  exit 1
fi

# The repo pins its toolchain, but a system cargo shadowing rustup's fails to
# build it at all, so prefer rustup's explicitly.
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"
if [[ ! -x "$CARGO" ]]; then
  CARGO=cargo
fi

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

before_stats=$(wc -l <data/poe1/en/stats.ndjson)
before_items=$(wc -l <data/poe1/en/items.ndjson)
before_tiers=$(wc -l <data/poe1/en/tiers.ndjson 2>/dev/null || echo 0)

if [[ "${1:-}" != "--skip-check" ]]; then
  step "Comparing the stat snapshot against the live trade API"
  "$CARGO" run --release --quiet --example check-data
fi

step "Regenerating the affix tier ladder"
# The ladder joins to the snapshot by stat text, so it must be rebuilt after any
# snapshot refresh — a reworded stat leaves it silently unresolvable.
"$CARGO" run --release --quiet --example vendor-tiers

step "Building and testing"
"$CARGO" test --quiet
"$CARGO" clippy --all-targets --quiet

step "What moved"
printf 'stats.ndjson  %6s -> %6s lines\n' "$before_stats" "$(wc -l <data/poe1/en/stats.ndjson)"
printf 'items.ndjson  %6s -> %6s lines\n' "$before_items" "$(wc -l <data/poe1/en/items.ndjson)"
printf 'tiers.ndjson  %6s -> %6s lines\n' "$before_tiers" "$(wc -l <data/poe1/en/tiers.ndjson)"
git diff --stat -- data/ || true

cat <<'EOF'

Next:
  * If the drift figures jumped against docs/UPDATING.md, refresh the APT
    snapshot first and re-run this — the ladder is only as current as it is.
  * Record the new figures in docs/UPDATING.md.
  * Price-check a few items carrying the patch's new affixes and confirm they
    resolve; ~/.local/state/poechk/checks.jsonl records what each one asked for.
EOF
