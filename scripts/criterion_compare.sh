#!/usr/bin/env bash
# Same-machine criterion comparison: <base-rev> vs the current checkout (#448).
#
#   scripts/criterion_compare.sh <base-rev> [extra criterion args...]
#   scripts/criterion_compare.sh --list    # print the gated bench list
#
# 1. Checks <base-rev> out into a temporary git worktree (the current checkout
#    is left untouched), links its tests/fixtures/realdata to this checkout's,
#    and runs the gated benches there with `--save-baseline base`. Benches the
#    base revision doesn't define yet are skipped on that side.
# 2. Deletes every `new/` and `change/` directory, so nothing from step 1 or an
#    older run can be mistaken for a result of step 3.
# 3. Runs the same benches on the current checkout with
#    `--baseline-lenient base` (a bench with no base is recorded as new
#    instead of aborting the run).
#
# Both sides share one CARGO_TARGET_DIR and write to a dedicated
# CRITERION_HOME (default: $CARGO_TARGET_DIR/criterion-compare), wiped at the
# start, so a restored CI cache or your own target/criterion history can't
# leak into the comparison. Then gate it:
#
#   python3 scripts/criterion_gate.py --check \
#     --criterion-dir target/criterion-compare --baseline base
#
# This file is the single list of gated benches. `assign_scaling` is left out
# on purpose: it is a wall-clock thread-scaling harness, not criterion.
set -euo pipefail

BENCHES="${CRITERION_BENCHES:-clipping bbox_containment pass1_decode assign simplify_cascade mvt_encode tile_compress_dedup}"

if [ $# -lt 1 ]; then
  sed -n '2,25p' "$0"
  exit 2
fi
if [ "$1" = "--list" ]; then
  echo "$BENCHES"
  exit 0
fi
base_rev=$1
shift

repo=$(git rev-parse --show-toplevel)
cd "$repo"
base_sha=$(git rev-parse --verify "${base_rev}^{commit}")
head_sha=$(git rev-parse HEAD)

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target}"
export CRITERION_HOME="${CRITERION_HOME:-$CARGO_TARGET_DIR/criterion-compare}"
rm -rf "$CRITERION_HOME"
mkdir -p "$CRITERION_HOME"

wt=$(mktemp -d "${TMPDIR:-/tmp}/criterion-base.XXXXXX")
cleanup() { git -C "$repo" worktree remove --force "$wt" >/dev/null 2>&1 || rm -rf "$wt"; }
trap cleanup EXIT
git worktree add --detach "$wt" "$base_sha" >/dev/null
rm -rf "$wt/tests/fixtures/realdata"
ln -s "$repo/tests/fixtures/realdata" "$wt/tests/fixtures/realdata"

# Bench targets the base revision defines ([[bench]] names in its manifest).
base_targets=$(awk '/^\[\[bench\]\]/{b=1; next} b && /^name *=/{gsub(/^name *= *"|"$/, ""); print; b=0}' \
  "$wt/crates/core/Cargo.toml")

echo "== base ${base_sha} =="
for b in $BENCHES; do
  if ! grep -qx "$b" <<<"$base_targets"; then
    echo "-- $b: not defined at base, will be reported as new"
    continue
  fi
  (cd "$wt" && cargo bench -p tylertoo-core --all-features --bench "$b" -- --save-baseline base "$@")
done

find "$CRITERION_HOME" -type d \( -name new -o -name change \) -prune -exec rm -rf {} +

echo "== head ${head_sha} =="
for b in $BENCHES; do
  cargo bench -p tylertoo-core --all-features --bench "$b" -- --baseline-lenient base "$@"
done
