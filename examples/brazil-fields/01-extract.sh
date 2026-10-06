#!/usr/bin/env bash
set -euo pipefail

# Fields of the World publishes one GeoParquet file per Brazilian state.
# List three neighboring states; tylertoo reads them as one dataset.
BASE=https://data.source.coop/ftw/global-data/predictions/vectors/alpha/results-by-admin-conf/admin:country_code=BR
printf '%s\n' "$BASE/BR_MT.parquet" "$BASE/BR_GO.parquet" "$BASE/BR_MS.parquet" > states.txt

# Keep fields of one hectare or more in a 20 km window near Sorriso,
# Mato Grosso. Row groups outside the window are never downloaded.
tylertoo overview --files-from states.txt \
  --bbox=-55.65,-12.65,-55.45,-12.45 \
  --filter '"metrics:area" >= 10000' \
  --max-zoom 14 \
  fields-ov.parquet
