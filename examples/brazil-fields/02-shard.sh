#!/usr/bin/env bash
set -euo pipefail

export AWS_ENDPOINT_URL=https://data.source.coop
BR=s3://ftw/global-data/predictions/vectors/alpha/results-by-admin-conf/admin:country_code=BR/
OPTIONS=(
  --filter '"metrics:area" >= 10000'
  --band 0-7:a5 --band 8-14:geom
  --accumulate-attribute metrics:area:sum
  --layer-name fields
)

# Cut the z10 tile space into four shards of about equal row count.
tylertoo shard-plan "$BR" --shards 4 --pivot 10 -o shards.json

# The coarse job draws the cells and z8-z9, and saves the plan every
# shard must share.
tylertoo tiles "$BR" coarse.pmtiles "${OPTIONS[@]}" \
  --shard coarse --shard-plan shards.json --save-plan convert.plan

# Each shard draws z10-z14 for its slice. On a cluster these jobs run on
# separate machines at the same time.
for i in 0 1 2 3; do
  tylertoo tiles "$BR" "shard-$i.pmtiles" "${OPTIONS[@]}" \
    --shard "$i/4" --shard-plan shards.json --plan convert.plan
done

# The jobs' tiles are disjoint, so merging them is a concatenation.
tylertoo merge sharded.pmtiles coarse.pmtiles \
  shard-0.pmtiles shard-1.pmtiles shard-2.pmtiles shard-3.pmtiles
tylertoo stats sharded.pmtiles
