#!/usr/bin/env bash
set -euo pipefail

# Download the input once: 17,465 admin-4 boundary polygons (28 MB).
SRC=fieldmaps-madagascar-adm4.parquet
if [ ! -f "$SRC" ]; then
  curl -fsSLO "https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/$SRC"
fi

# The export holds WKB geometry but no GeoParquet `geo` metadata.
# DuckDB's spatial extension writes the metadata on a round trip.
duckdb -c "
  INSTALL spatial;
  LOAD spatial;
  COPY (
    SELECT * REPLACE (ST_GeomFromWKB(geometry) AS geometry)
    FROM '$SRC'
  ) TO 'raw.parquet' (FORMAT parquet);
"

# Hilbert-sort, add a bbox column, compress with ZSTD, and pack 16 MB
# row groups, all in one gpio pass.
gpio convert geoparquet raw.parquet prepared.parquet --row-group-size-mb 16
gpio inspect prepared.parquet
