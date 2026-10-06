#!/usr/bin/env bash
set -euo pipefail

# Add a bbox covering and Hilbert order: shard-plan balances shards by
# row-group bounds, which the DuckDB export does not record.
gpio convert geoparquet fields-raw.parquet fields.parquet

# Cut the z10 tile space into four shards of about equal row count.
tylertoo shard-plan fields.parquet --shards 4 --pivot 10 -o shards.json

# The coarse job builds z8-z9, below the pivot, and saves the plan
# every shard must share.
tylertoo tiles fields.parquet coarse.pmtiles \
  --min-zoom 8 --max-zoom 14 --layer-name fields \
  --shard coarse --shard-plan shards.json --save-plan convert.plan

# Each shard builds z10-z14 for its slice. On a cluster these jobs run
# on separate machines at the same time.
for i in 0 1 2 3; do
  tylertoo tiles fields.parquet "shard-$i.pmtiles" \
    --min-zoom 8 --max-zoom 14 --layer-name fields \
    --shard "$i/4" --shard-plan shards.json --plan convert.plan
done

# The shards are disjoint, so merging them is a concatenation.
tylertoo merge fields.pmtiles coarse.pmtiles \
  shard-0.pmtiles shard-1.pmtiles shard-2.pmtiles shard-3.pmtiles
