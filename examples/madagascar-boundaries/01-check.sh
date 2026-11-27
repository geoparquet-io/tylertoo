#!/usr/bin/env bash
set -euo pipefail

# Download the input once: 17,465 admin-4 boundary polygons (17 MB),
# already GeoParquet, Hilbert-sorted, and ZSTD-compressed.
SRC=fieldmaps-madagascar-adm4-hilbert.parquet
if [ ! -f "$SRC" ]; then
  curl -fsSLO "https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/$SRC"
fi

# Check compression, bbox covering, row groups, and spatial order.
# --strict fails on warnings too.
gpio check all "$SRC" --strict
