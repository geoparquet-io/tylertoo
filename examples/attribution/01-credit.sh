#!/usr/bin/env bash
set -euo pipefail

SRC=open-buildings.parquet
if [ ! -f "$SRC" ]; then
  curl -fsSLO "https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/$SRC"
fi

NAME="Andorra buildings"
CREDIT='<a href="https://source.coop/vida/google-microsoft-osm-open-buildings">VIDA</a>, © OpenStreetMap contributors'
tylertoo tiles "$SRC" low.pmtiles --max-zoom 10 \
  --layer-name buildings --name "$NAME" --attribution "$CREDIT"
tylertoo tiles "$SRC" high.pmtiles --min-zoom 11 --max-zoom 14 \
  --layer-name buildings --name "$NAME" --attribution "$CREDIT"
tylertoo merge andorra.pmtiles low.pmtiles high.pmtiles
