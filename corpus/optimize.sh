#!/usr/bin/env bash
#
# optimize.sh - produce gpio-optimized copies of raw datasets.
#
# Reads data/raw/<id>.parquet, writes data/gpio/<id>.parquet with:
#   - Hilbert spatial ordering (gpio sort hilbert)
#   - bbox struct column + GeoParquet 1.1 covering metadata
#   - ZSTD compression, optimized row groups
#
# This is the "input contract" the overview converter (P5) assumes:
# gpio-sorted, bbox-covered GeoParquet 1.1.
#
# Idempotent: skips datasets whose gpio output is newer than raw
# (use --force to rebuild). Runs on whatever is in data/raw/.
#
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RAW="$HERE/data/raw"
GPIODIR="$HERE/data/gpio"

FORCE=0
for arg in "$@"; do
  case "$arg" in
    --force) FORCE=1 ;;
    -h|--help)
      grep '^#' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
    *) echo "Unknown arg: $arg" >&2; exit 2 ;;
  esac
done

command -v gpio >/dev/null 2>&1 || {
  echo "ERROR: 'gpio' not found." >&2
  echo "  Install: 'uv tool install geoparquet-io'" >&2
  exit 1
}

[ -d "$RAW" ] || {
  echo "No data/raw/ dir. Run ./fetch.sh first." >&2
  exit 1
}
mkdir -p "$GPIODIR"

shopt -s nullglob
INPUTS=("$RAW"/*.parquet)
[ ${#INPUTS[@]} -gt 0 ] || {
  echo "No raw parquet files. Run ./fetch.sh first." >&2
  exit 1
}

COUNT=0
for src in "${INPUTS[@]}"; do
  id="$(basename "$src" .parquet)"
  out="$GPIODIR/$id.parquet"

  if [ -f "$out" ] && [ "$FORCE" = 0 ] \
     && [ "$out" -nt "$src" ]; then
    echo "== have [$id]; skip (use --force)"
    continue
  fi

  echo ">> [$id] hilbert sort + bbox covering"
  # GeoParquet 1.1 per OVERVIEWS_SPEC (2.0 variant later).
  gpio sort hilbert "$src" "$out" \
    --add-bbox \
    --geoparquet-version 1.1 \
    --compression zstd \
    --overwrite
  echo "   wrote $out ($(du -h "$out" | cut -f1))"
  COUNT=$((COUNT + 1))
done

echo
echo "Optimized $COUNT dataset(s) into $GPIODIR"
echo "Verify any file with: gpio check all <file>"
echo "Next: ./goldens.sh"
