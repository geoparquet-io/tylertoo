#!/usr/bin/env bash
#
# run_conversion.sh - conversion-cost table: wall time + peak RSS.
#
#   overview  : `tylertoo overview` (duplicating, default knobs, z0..14)
#               reading the gpio GeoParquet NATIVELY.
#   tippecanoe: the documented golden workflow -- `gpio convert geojson
#               <src> | tippecanoe -P <recorded flags>` -- since tippecanoe
#               cannot read GeoParquet directly (v2.49). The pipeline's
#               wall time and peak RSS therefore INCLUDE the mandatory
#               GeoParquet->GeoJSON decode that the native overview path
#               avoids; this is called out in RESULTS.md.
#
# Both wrapped in `/usr/bin/time -v`. Datasets: line + polygon medium and
# Moldova large. tippecanoe uses the exact recorded golden flags verbatim.
#
# Outputs corpus/data/bench/conversion/<id>.<tool>.time.txt + markdown.
# For the overview side it reuses the timing captured when the bench
# overview files were regenerated (corpus/data/bench/overviews/<id>.dup.
# time.txt) unless FORCE=1.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BIN="${GPQ_BIN:-$ROOT/target/release/tylertoo}"
GPIO="$ROOT/corpus/data/gpio"
OVOUT="$ROOT/corpus/data/bench/overviews"
GOLD="$ROOT/corpus/data/goldens/tippecanoe"
OUT="$ROOT/corpus/data/bench/conversion"
mkdir -p "$OUT"

DATASETS="${GPQ_ONLY:-lines-portland-medium polygons-portland-medium polygons-ftw-moldova-large}"

elapsed() { grep -oP 'Elapsed.*: \K[0-9:.]+' "$1"; }
rss_mb()  { awk '/Maximum resident/{printf "%d", $NF/1024}' "$1"; }

for id in $DATASETS; do
  in="$GPIO/$id.parquet"

  # ---- overview (reuse regen timing unless FORCE) ----
  ovtime="$OVOUT/$id.dup.time.txt"
  if [ "${FORCE:-0}" = 1 ] || [ ! -s "$ovtime" ]; then
    ovtime="$OUT/$id.overview.time.txt"
    /usr/bin/time -v "$BIN" overview "$in" "$OUT/$id.dup.parquet" \
      --mode duplicating --min-zoom 0 --max-zoom 14 \
      >/dev/null 2>"$ovtime"
    cp -f "$ovtime" "$OUT/$id.overview.time.txt"
  else
    cp -f "$ovtime" "$OUT/$id.overview.time.txt"
  fi

  # ---- tippecanoe (recorded golden flags, gpio->geojson pipe) ----
  # flags.txt line already encodes: -Z0 -z14 -l data <dataset flags>
  flags=$(grep -oP '^flags: \K.*' "$GOLD/$id.flags.txt")
  /usr/bin/time -v bash -c \
    "gpio convert geojson '$in' | tippecanoe -P $flags -o '$OUT/$id.pmtiles' -f" \
    >/dev/null 2>"$OUT/$id.tippecanoe.time.txt"
  echo "done $id"
done

echo
echo "| dataset | overview wall | overview peak RSS | tippecanoe(+gpio) wall | tippecanoe(+gpio) peak RSS |"
echo "|---|---|---|---|---|"
for id in $DATASETS; do
  ovt="$OUT/$id.overview.time.txt"
  tpt="$OUT/$id.tippecanoe.time.txt"
  echo "| $id | $(elapsed "$ovt") | $(rss_mb "$ovt") MB | $(elapsed "$tpt") | $(rss_mb "$tpt") MB |"
done
