#!/usr/bin/env bash
set -euo pipefail

# Fields of the World on Source Cooperative, which serves the S3 API.
export AWS_ENDPOINT_URL=https://data.source.coop

# Brazil is one GeoParquet file per state under this prefix. Keep the fields
# of one hectare or more in a 20 km window near Sorriso, Mato Grosso: A5
# cells that count them at z0-z7, the fields themselves at z8-z14.
tylertoo tiles s3://ftw/global-data/predictions/vectors/alpha/results-by-admin-conf/admin:country_code=BR/ \
  --bbox=-55.65,-12.65,-55.45,-12.45 \
  --filter '"metrics:area" >= 10000' \
  --band 0-7:a5 --band 8-14:geom \
  --accumulate-attribute metrics:area:sum \
  --layer-name fields \
  brazil.pmtiles
tylertoo stats brazil.pmtiles
