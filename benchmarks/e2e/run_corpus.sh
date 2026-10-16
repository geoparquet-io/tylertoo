#!/usr/bin/env bash
#
# run_corpus.sh — the one-command corpus benchmark (#447): end-to-end
# GeoParquet → PMTiles, tylertoo vs the pinned tippecanoe, on the
# engine-scale corpus datasets (hundreds of thousands of features, where the
# in-repo fixtures are the 1k–17k smoke test).
#
# It is a thin driver over the pieces that already exist, in order:
#
#   1. corpus/fetch.sh + corpus/optimize.sh   (idempotent; ~2–3 GB download
#      on the first run; GPQ_OVERTURE_RELEASE passes through if the pinned
#      Overture release has aged out of the bucket)
#   2. cargo build --release -p tylertoo      (skipped if the binary exists
#      and FORCE_BUILD is not set; CARGO_BUILD_JOBS respected)
#   3. benchmarks/e2e/setup_tippecanoe.sh     (the pinned tippecanoe, built
#      from source at tag + verified SHA)
#   4. run_e2e.py --corpus --quality-matched  (wall, peak RSS, CPU%, archive
#      bytes, per-zoom tile counts for both tools; see README.md for the
#      method and every asymmetry)
#   5. format_results.py on the fresh record
#
# Portable: timing and RSS come from run_e2e.py's /usr/bin/time wrap
# (`-l` on Darwin, `-v` on GNU), not from GNU-only shell parsing.
#
# Usage:
#   ./benchmarks/e2e/run_corpus.sh                 # the default four datasets
#   REPEAT=3 ./benchmarks/e2e/run_corpus.sh        # more timed repeats
#   DATASETS="polygons-ftw-moldova-large" ./benchmarks/e2e/run_corpus.sh
#   SKIP_FETCH=1 ./benchmarks/e2e/run_corpus.sh    # corpus already built
#
# The record lands in benchmarks/e2e/results-corpus-<UTC stamp>.json
# (gitignored). The committed record is corpus_results.json; replacing it is
# a deliberate act: OUT=benchmarks/e2e/corpus_results.json, then regenerate
# RESULTS.md's corpus tables with format_results.py.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

REPEAT="${REPEAT:-2}"
# The engine-scale rows: one point, one line, two polygon datasets
# (one of the polygon rows is the 632k-feature Moldova parcels set that the
# old in-memory pipeline famously lost 3.6× on).
DATASETS="${DATASETS:-points-nyc-medium lines-portland-medium polygons-portland-medium polygons-ftw-moldova-large}"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="${OUT:-$HERE/results-corpus-$STAMP.json}"

# ---- 1. corpus data (idempotent) --------------------------------------
if [ "${SKIP_FETCH:-0}" != 1 ]; then
  (cd "$ROOT/corpus" && ./fetch.sh && ./optimize.sh)
else
  echo "-- SKIP_FETCH=1: using corpus/data/gpio as it stands"
fi

# ---- 2. the release binary --------------------------------------------
BIN="$ROOT/target/release/tylertoo"
if [ ! -x "$BIN" ] || [ "${FORCE_BUILD:-0}" = 1 ]; then
  (cd "$ROOT" && cargo build --release -p tylertoo)
fi

# ---- 3. the pinned tippecanoe -----------------------------------------
"$HERE/setup_tippecanoe.sh"

# ---- 4. the measurement -----------------------------------------------
only_args=()
for d in $DATASETS; do only_args+=(--only "$d"); done

python3 "$HERE/run_e2e.py" \
  --corpus --quality-matched \
  --repeat "$REPEAT" \
  "${only_args[@]}" \
  --out "$OUT"

# ---- 5. the tables -----------------------------------------------------
echo
echo "== tables from $OUT =="
python3 "$HERE/format_results.py" "$OUT"
