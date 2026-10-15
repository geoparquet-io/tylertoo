#!/usr/bin/env bash
# Which CodSpeed benches a change can move (#614).
#
#   git diff --name-only <base> HEAD | scripts/codspeed_benches.sh
#   scripts/codspeed_benches.sh --all
#
# Reads changed paths (repo-relative, one per line) on stdin and prints the
# benches from `scripts/criterion_compare.sh --list` whose code they touch,
# space-separated, or nothing. codspeed.yml runs only those on a PR; CodSpeed
# reports the rest as "skipped" with their baseline numbers (partial runs).
# Pushes to main always run --all, so every bench keeps a fresh baseline.
#
# The map below is hand-kept: each bench's entry modules plus their
# top-level `use crate::` dependencies. The static import graph is no help
# here (shared error types and test modules tie 34 of core's 49 modules
# together), so this trades completeness for a useful filter: a change that
# moves a bench only through some distant module is caught by the next run
# on main instead of on the PR. A bench with no entry runs on any
# crates/core change; a bench's own file always selects it.
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"
ALL=$(scripts/criterion_compare.sh --list)

if [ "${1:-}" = "--all" ]; then
  echo "$ALL"
  exit 0
fi

# Changes that can move every bench: the build, the bench harness, the
# generated MVT protobuf, the crate root, and this selection itself.
GLOBAL='^(Cargo\.toml|Cargo\.lock|crates/core/Cargo\.toml|crates/core/build\.rs|crates/core/proto/.*|crates/core/src/lib\.rs|crates/core/src/overview/mod\.rs|crates/core/benches/support/.*|\.github/workflows/codspeed\.yml|scripts/criterion_compare\.sh|scripts/codspeed_benches\.sh)$'

S=crates/core/src
CLIP="$S/(clip|ioverlay_clip|sutherland_hodgman|tile|world_coord)\.rs"
MVT="$S/(mvt|tile|world_coord)\.rs"
DECODE="$S/(batch_processor|wkb|wkb_column)\.rs"
declare -A PATHS=(
  [clipping]="$CLIP"
  [bbox_containment]="$CLIP"
  [pass1_decode]="$DECODE"
  [assign]="$S/overview/(assign|level)\.rs"
  [simplify_cascade]="$S/overview/(simplify|level)\.rs|$MVT|$DECODE"
  [mvt_encode]="$MVT"
  [tile_compress_dedup]="$S/(compression|dedup)\.rs|$MVT"
)

changed=$(cat)
if grep -qE "$GLOBAL" <<<"$changed"; then
  echo "$ALL"
  exit 0
fi

selected=()
for b in $ALL; do
  re=${PATHS[$b]:-crates/core/.*}
  if grep -qE "^($re|crates/core/benches/$b\.rs)$" <<<"$changed"; then
    selected+=("$b")
  fi
done
echo "${selected[*]}"
