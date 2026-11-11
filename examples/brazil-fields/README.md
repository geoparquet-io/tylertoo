<!-- --8<-- [start:build] -->
# Brazil fields: cloud data to a map with density cells

Build a PMTiles map of the soy belt around Sorriso, Mato Grosso, from
[Fields of the World](https://fieldsofthe.world/) (FTW). It holds about
3.2 billion field polygons on
[Source Cooperative](https://source.coop/ftw/global-data), stored as one
GeoParquet file per country subdivision. One command streams the remote
files and writes one archive: A5 density cells at low zooms and field
polygons up close. You only download the row groups needed for the area.

This extends the
[Madagascar tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/madagascar/)
with:

- remote reads of an `s3://` prefix with `--bbox` and `--filter`
- `--band` to draw cells at some zooms and features at others
- `shard-plan`, `--shard`, and `merge` to split the same build into independent jobs

Each step is a script in
[`examples/brazil-fields/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/brazil-fields).
CI runs the scripts against live data on each pull request and checks the
code and output shown here.

## Before you begin

Install `tylertoo` with `cargo install tylertoo`. No other tools are needed.
Run the steps in order in an empty working directory. Step 1 downloads
about 120 MB. Step 2 runs five jobs that each download the same 120 MB.

## 1. Tile a window straight from the bucket

Point `AWS_ENDPOINT_URL` at Source Cooperative, which serves the S3 API.
You need no credentials. tylertoo falls back to anonymous requests and
logs a warning about it.

tylertoo reads the 45 files under Brazil's prefix as one dataset.
tylertoo reads each file's footer, then fetches only row groups whose
bounds intersect `--bbox`: about 120 MiB of the 11 GB under the prefix
(1.1%). Use `--filter` to drop fields under one hectare. The `metrics:area`
column stores square meters.

Draw A5 cells at z0-z7 with `--band 0-7:a5`. Each cell holds `cell`, its id,
and `point_count`, the number of fields whose centroid falls in it.
`--accumulate-attribute metrics:area:sum` sums the field areas in
`metrics:area`. Draw field polygons at z8-z14 with `--band 8-14:geom`.
There, `metrics:area` is each field's own area.

Name the shared layer `fields` with `--layer-name`. Style by zoom or by
whether `point_count` is present. tylertoo opens the input once. It counts
the cells, then tiles the fields from the same fetched bytes. The cells
add almost no download: a fields-only run with `--min-zoom 8 --max-zoom 14`
fetches the same 120.64 MiB. The whole run takes about 4 seconds.

Choose another cell system by changing `a5` in `--band 0-7:a5`:

| System | Cells |
| --- | --- |
| `grid` | Squares aligned to the tile grid at each zoom |
| `h3` | Hexagons |
| `s2` | Quadrilaterals on a cube |
| `a5` | Pentagons with equal areas |

Cell resolution follows the zoom in every system. At every zoom, each field
counts in the cell that holds its centroid, so counts are exact in every
system.

Run the script to build the archive. `tylertoo stats` reports tile sizes
per zoom.

File: `01-tiles.sh`

```bash
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
```
```text
✓ Converted admin:country_code=BR → brazil.pmtiles
...
 z  tiles    total    mean     p50     p99     max
 0      1      166     166     166     166     166
 1      1      147     147     147     147     147
 2      1      134     134     134     134     134
 3      1      125     125     125     125     125
 4      1      166     166     166     166     166
 5      1      269     269     269     269     269
 6      1      375     375     375     375     375
 7      1      761     761     761     761     761
 8      2   29,194  14,597  13,950  15,244  15,244
 9      4   42,849  10,712  10,530  11,155  11,155
10      4   50,359  12,589  12,110  13,673  13,673
11      4   67,864  16,966  15,339  19,416  19,416
12     15  120,305   8,020   6,318  23,336  23,336
13     37  181,026   4,892   4,663  11,915  11,915
14    125  230,788   1,846   1,801   4,546   4,987
```

The run keeps 534 fields. There is one A5 pentagon at each of z0-z3,
two at z4, five at z5, eight at z6, and 23 at z7. At every cell zoom,
`point_count` sums to 534 and `metrics:area` sums to 36,733 ha.

Field tiles are heaviest at z11-z12 and shrink at higher zooms. Drop
`brazil.pmtiles` onto [pmtiles.io](https://pmtiles.io/) to inspect it.

## 2. Split the same build into shard jobs

Split the tiles into independent jobs when one machine is too slow for a
whole country. `shard-plan` cuts the z10 tile space, the pivot, into four
shards of about equal row count. The plan covers every row under the
prefix, not just the window. Here, the window's fields all fall in shard 0.
Shards 1-3 have nothing to draw. On a country-wide run, the shards balance.

Run the coarse job to draw the cells and field zooms below the pivot
(z0-z9). Save the convert plan with `--save-plan`. Run each data shard to
draw z10-z14 for its slice and load the plan with `--plan`. Data shards
skip the cells.

Give every job the same options through the `OPTIONS` array. The plan's
fingerprint covers them, including the `--band` flags. tylertoo refuses a
mismatched job. A cell band must end below the pivot. tylertoo refuses one
that reaches it before reading anything.

A shard with no rows writes an empty archive, which `merge` skips. The
jobs' tiles are disjoint, so `merge` concatenates them.

File: `02-shard.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

export AWS_ENDPOINT_URL=https://data.source.coop
BR=s3://ftw/global-data/predictions/vectors/alpha/results-by-admin-conf/admin:country_code=BR/
OPTIONS=(
  --bbox=-55.65,-12.65,-55.45,-12.45
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
```
```text
Cut 4 shard(s) at pivot z10 for s3://ftw/global-data/predictions/vectors/alpha/results-by-admin-conf/admin:country_code=BR/
...
✓ Converted admin:country_code=BR → coarse.pmtiles
...
✓ Converted admin:country_code=BR → shard-0.pmtiles
  185 tiles across z10..z14 in 1.10s
...
✓ shard 2/4 owns no input rows; wrote an empty archive at shard-2.pmtiles
...
Merging 5 archive(s) → sharded.pmtiles
...
  z0           1 tiles
...
  z7           1 tiles
  z8           2 tiles
...
  z14        125 tiles
...
 z  tiles    total    mean     p50     p99     max
 0      1      166     166     166     166     166
...
14    125  230,788   1,846   1,801   4,546   4,987
```

`sharded.pmtiles` holds the same 199 tiles as `brazil.pmtiles` from step 1,
byte for byte. The final stats match step 1.

<!-- --8<-- [end:build] -->
<!-- --8<-- [start:run] -->
## Run the whole workflow

```bash
git clone https://github.com/geoparquet-io/tylertoo.git
mkdir brazil
cd brazil
for step in ../tylertoo/examples/brazil-fields/0*.sh; do bash "$step"; done
```

## Next steps

- To tile all of Brazil, remove `--bbox`, raise `--shards`, and run the shard
  jobs on separate machines. The
  [scaling guide](https://geoparquet-io.github.io/tylertoo/guides/scaling/#sharded-builds)
  covers sizing the jobs and running them under a scheduler.
- The [remote reads guide](https://geoparquet-io.github.io/tylertoo/guides/remote-reads/)
  covers `--filter` syntax, `--files-from` manifests, and row-group pruning.
- The [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
  explains every generalization knob, including `--band`.
- Use `tylertoo pyramid` to combine bands from separate inputs or archives,
  such as cells from another dataset. See the same tuning reference.
- The [Python reference](https://geoparquet-io.github.io/tylertoo/reference/python/)
  lists `convert`, which takes the same bands as `bands=[...]`.
<!-- --8<-- [end:run] -->
