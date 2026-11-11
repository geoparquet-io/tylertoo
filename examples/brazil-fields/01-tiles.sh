#!/usr/bin/env bash
set -euo pipefail

# Fields of the World on Source Cooperative, which serves the S3 API.
export AWS_ENDPOINT_URL=https://data.source.coop

# Brazil is one GeoParquet file per state under this prefix, with two
# prediction years. Keep every 2025 field of one hectare or more in the
# country: A5 cells that count them at z0-z7, the fields at z8-z14.
tylertoo tiles s3://ftw/global-data/predictions/vectors/alpha/results-by-admin-conf/admin:country_code=BR/ \
  --filter "\"metrics:area\" >= 10000 AND \"determination:datetime\" >= '2025-01-01'" \
  --band 0-7:a5 --band 8-14:geom \
  --accumulate-attribute metrics:area:sum \
  --layer-name fields \
  brazil.pmtiles
tylertoo stats brazil.pmtiles
