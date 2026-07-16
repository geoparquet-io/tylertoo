#!/usr/bin/env bash
# Unified performance sweep.
#   default : inputs read over a localhost logging server -> captures bytes moved.
#   LOCAL=1 : inputs read from local disk (no server) -> clean wall/RSS/CPU;
#             outputs suffixed ".local" so a prior remote run's byte data is kept.
# The berlin-bbox selective-read cell only runs in remote mode (bytes are the point).
set -uo pipefail
export RUST_BACKTRACE=1

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BIN="$ROOT/target/release/tylertoo"
DATAROOT="$ROOT/corpus/data"
OUT="$HERE/sweep_out"
SCRATCH="$ROOT/corpus/data/bigbench/sweep_scratch"
PORT="${PORT:-8901}"
BASE="http://127.0.0.1:$PORT"
LOCAL="${LOCAL:-0}"
SFX=""; [ "$LOCAL" = 1 ] && SFX=".local"
mkdir -p "$OUT" "$SCRATCH"

[ -x "$BIN" ] || { echo "build release binary first" >&2; exit 1; }
COMMIT="$(cd "$ROOT" && git rev-parse --short HEAD)"

if [ "$LOCAL" != 1 ]; then
  python3 "$HERE/logging_server.py" "$DATAROOT" "$PORT" >"$OUT/server.log" 2>&1 &
  SRV=$!
  trap 'kill $SRV 2>/dev/null' EXIT
  for _ in $(seq 1 30); do curl -sf "$BASE/__stats" >/dev/null 2>&1 && break; sleep 0.3; done
  curl -sf "$BASE/__stats" >/dev/null 2>&1 || { echo "server failed"; exit 1; }
fi

run_cell() {
  local label="$1" relpath="$2"; shift 2
  local file="$DATAROOT/$relpath"
  [ -e "$file" ] || { echo "=== $label MISSING $relpath ==="; return; }
  local size; size=$(stat -c%s "$file")
  local tf="$OUT/$label$SFX.time.txt" rf="$OUT/$label$SFX.report.json" dst="$SCRATCH/$label.parquet"
  local input
  if [ "$LOCAL" = 1 ]; then
    input="$file"
  else
    input="$BASE/$relpath"
    curl -sf "$BASE/__reset" >/dev/null
  fi

  echo "=== $label$SFX start $(date -u +%H:%M:%SZ) ==="
  rm -f "$dst"
  timeout "${TIMEOUT:-3600}" /usr/bin/time -v -o "$tf" "$BIN" overview "$input" "$dst" \
    --min-zoom 0 --max-zoom 14 --report "$rf" "$@" >"$OUT/$label$SFX.log" 2>&1
  local rc=$?
  local stats="(local)"
  [ "$LOCAL" != 1 ] && stats=$(curl -sf "$BASE/__stats")
  echo "  exit=$rc  size=$size  stats=$stats"
  grep -E "Elapsed \(wall|Maximum resident|Percent of CPU" "$tf" 2>/dev/null
  rm -f "$dst"
}

echo "############ SWEEP commit=$COMMIT LOCAL=$LOCAL $(date -u) ############"
run_cell points-nyc        gpio/points-nyc-medium.parquet --mode duplicating
run_cell segments          bigbench/gpio/overture-germany-segments.parquet --mode duplicating
run_cell buildings         bigbench/gpio/overture-germany-buildings.parquet --mode duplicating
run_cell fieldmaps-dup     bigbench/gpio/fieldmaps-adm4.parquet --mode duplicating
run_cell fieldmaps-par     bigbench/gpio/fieldmaps-adm4.parquet --mode partitioning
run_cell buildings-speed   bigbench/gpio/overture-germany-buildings.parquet --mode duplicating --profile speed
run_cell buildings-bounded bigbench/gpio/overture-germany-buildings.parquet --mode duplicating --profile bounded
[ "$LOCAL" != 1 ] && run_cell berlin-bbox bigbench/gpio/overture-germany-buildings.parquet --mode duplicating --bbox 13.35,52.48,13.47,52.55
echo "############ DONE $(date -u) ############"
