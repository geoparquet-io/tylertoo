#!/usr/bin/env bash
#
# run_all.sh - reproduce the full V3 overview benchmark.
#
# Steps:
#   1. regenerate the overview files (duplicating + partitioning) for the
#      four benchmark datasets with the current release binary and DEFAULT
#      knobs, zoom 0..14, capturing /usr/bin/time -v (Moldova dup ~11 min).
#   2. storage table (bench_storage.py)  [+ cogp if `cogp` is on PATH]
#   3. conversion-cost table (run_conversion.sh)
#   4. access benchmark: start the logging server, run bench_access.py,
#      render the markdown (format_access.py).
#
# All heavy outputs go under corpus/data/bench/ (gitignored). One benchmark
# process runs at a time. Re-runnable / idempotent.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BIN="${GPQ_BIN:-$ROOT/target/release/gpq-tiles}"
GPIO="$ROOT/corpus/data/gpio"
OUT="$ROOT/corpus/data/bench/overviews"
PORT="${BENCH_PORT:-8899}"
mkdir -p "$OUT" "$ROOT/corpus/data/bench/logs"

DATASETS="points-nyc-medium lines-portland-medium polygons-portland-medium polygons-ftw-moldova-large"

[ -x "$BIN" ] || { echo "build the release binary first: cargo build --release --package gpq-tiles" >&2; exit 1; }

echo "== step 1: regenerate overview files =="
for id in $DATASETS; do
  for pair in "duplicating dup" "partitioning par"; do
    set -- $pair; mode=$1; m=$2
    out="$OUT/$id.$m.parquet"
    if [ -s "$out" ] && [ "${FORCE:-0}" != 1 ]; then
      echo "  skip $id.$m (exists; FORCE=1 to rebuild)"; continue
    fi
    echo "  $id $mode"
    /usr/bin/time -v "$BIN" overview "$GPIO/$id.parquet" "$out" \
      --mode "$mode" --min-zoom 0 --max-zoom 14 \
      --report "$OUT/$id.$m.report.json" \
      >"$OUT/$id.$m.convert.log" 2>"$OUT/$id.$m.time.txt"
  done
done

echo "== step 2: viewports + storage =="
python3 "$HERE/make_viewports.py" >/dev/null
python3 "$HERE/bench_storage.py" | tee "$HERE/storage_table.md"

echo "== step 3: conversion cost =="
bash "$HERE/run_conversion.sh" | tee "$HERE/conversion_table.md"

echo "== step 4: access benchmark =="
python3 "$HERE/logging_server.py" "$ROOT/corpus/data" "$PORT" \
  >"$ROOT/corpus/data/bench/logs/server.log" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
# wait for server
for _ in $(seq 1 20); do
  curl -sf "http://127.0.0.1:$PORT/__stats" >/dev/null 2>&1 && break; sleep 0.3
done
BENCH_PORT="$PORT" uv run --with pmtiles python3 "$HERE/bench_access.py"
python3 "$HERE/format_access.py" | tee "$HERE/access_table.md"

echo "== done. See RESULTS.md; raw outputs under corpus/data/bench/ =="
