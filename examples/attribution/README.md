`01-credit.sh`

```bash
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
```

```text
...
✓ Converted open-buildings.parquet → low.pmtiles
  8 tiles across z0..z10 in 0.04s
...
✓ Converted open-buildings.parquet → high.pmtiles
  79 tiles across z11..z14 in 0.05s
...
Merging 2 archive(s) → andorra.pmtiles
...
  z6           1 tiles
  z7           1 tiles
  z8           1 tiles
  z9           2 tiles
  z10          3 tiles
  z11          2 tiles
  z12          6 tiles
  z13         20 tiles
  z14         51 tiles
  name: "Andorra buildings"
  attribution: "<a href=\"https://source.coop/vida/google-microsoft-osm-open-buildings\">VIDA</a>, © OpenStreetMap contributors"
✓ 87 tiles (87 unique) from 2 archive(s), z0..z14 in 0.00s
```
