#!/usr/bin/env bash
set -euo pipefail

SRC=open-buildings.parquet
if [ ! -f "$SRC" ]; then
  curl -fsSLO "https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/$SRC"
fi

tylertoo tiles "$SRC" andorra.pmtiles --preset tippecanoe --layer-name buildings
tylertoo stats andorra.pmtiles
