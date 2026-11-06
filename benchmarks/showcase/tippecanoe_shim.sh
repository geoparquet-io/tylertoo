#!/usr/bin/env bash
# tippecanoe_shim.sh: tippecanoe end to end behind tylertoo's `tiles` argv,
# so run_manifest.py can time it as one more --bin in the same cell:
#
#   tippecanoe_shim.sh tiles IN.parquet OUT.pmtiles --min-zoom A --max-zoom B [tylertoo flags]
#
# 1. gpio converts the GeoParquet to FlatGeobuf in $TMPDIR (tippecanoe
#    cannot read GeoParquet; this is part of its real workflow).
# 2. tippecanoe tiles the FlatGeobuf with run_e2e.py's parity flags.
#
# Mapped from the tylertoo argv: --min-zoom, --max-zoom, --layer-name,
# --tile-buffer, --max-compressed-tile-size. Every other tylertoo flag has
# no tippecanoe equivalent and is ignored.
#
# Environment (remote.sh sets these through run_manifest's --bin-env):
#   TIPPECANOE_BIN    the pinned tippecanoe (setup_tippecanoe.sh); required
#   TIPPECANOE_FLAGS  extra tippecanoe flags (the corpus polygons-* flags)
#   GPIO              the gpio command (default: the pinned uvx gpio)
#   SHOWCASE_KEEP     a directory on the same filesystem: OUT is hard-linked
#                     there as tippecanoe.pmtiles, because run_manifest
#                     deletes OUT after the run
#
# Prints `showcase: fgb_convert_s=... tippecanoe_s=... fgb_bytes=...` to
# stdout, which run_manifest keeps in the run's log.
set -euo pipefail

if [ "${1:-}" != tiles ] || [ $# -lt 3 ]; then
  echo "usage: $0 tiles IN OUT [--min-zoom A] [--max-zoom B] [tylertoo flags]" >&2
  exit 2
fi
in=$2 out=$3
shift 3
zmin=0 zmax=14 layer=buildings buffer=8 cap=500000
while [ $# -gt 0 ]; do
  case $1 in
    --min-zoom) zmin=$2; shift ;;
    --max-zoom) zmax=$2; shift ;;
    --layer-name) layer=$2; shift ;;
    --tile-buffer) buffer=$2; shift ;;
    --max-compressed-tile-size) cap=$2; shift ;;
  esac
  shift
done

: "${TIPPECANOE_BIN:?set TIPPECANOE_BIN to the pinned tippecanoe}"
GPIO=${GPIO:-uvx --from geoparquet-io==1.7.0 gpio}
tmp=${TMPDIR:-/tmp}
fgb="$tmp/input.fgb"

now() { date +%s.%N; }
t0=$(now)
# shellcheck disable=SC2086  # GPIO is a command line
$GPIO convert flatgeobuf "$in" "$fgb" --overwrite
t1=$(now)
# shellcheck disable=SC2086  # TIPPECANOE_FLAGS is a flag list
"$TIPPECANOE_BIN" -o "$out" -f -Z "$zmin" -z "$zmax" -l "$layer" \
  -b "$buffer" --maximum-tile-bytes "$cap" --drop-fraction-as-needed \
  --quiet -t "$tmp" ${TIPPECANOE_FLAGS:-} "$fgb"
t2=$(now)

fgb_bytes=$(stat -c %s "$fgb")
rm -f "$fgb"
awk -v a="$t0" -v b="$t1" -v c="$t2" -v n="$fgb_bytes" \
  'BEGIN { printf "showcase: fgb_convert_s=%.3f tippecanoe_s=%.3f fgb_bytes=%d\n", b - a, c - b, n }'
if [ -n "${SHOWCASE_KEEP:-}" ]; then
  mkdir -p "$SHOWCASE_KEEP"
  ln -f "$out" "$SHOWCASE_KEEP/tippecanoe.pmtiles"
fi
