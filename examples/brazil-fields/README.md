<!-- --8<-- [start:build] -->
# Build vector tiles from cloud data

In this tutorial, you use tylertoo to build a PMTiles archive of Brazil's
fields from GeoParquet on Source Cooperative. First, you build the archive
with one command. Then you split the same build into jobs that can run on
separate machines and merge their outputs.

## Before you begin

Install tylertoo with `cargo install tylertoo` and run the scripts in an
empty directory. Source Cooperative lets you read this dataset without
credentials.

tylertoo streams the input from the bucket. The first build transfers about
11.6 GB. Allow disk space for staged input, a temporary file of about
12 GiB, and the 3.1 GB output archive. If you need to use a different disk
for temporary files, set `TMPDIR` or pass `--spill-dir PATH` to
`tylertoo tiles`.

Together, the two builds took about 35 minutes on a 64-core machine with a
40 GB memory cap.

## 1. Build tiles from the bucket

The input URL ends in `/`, which tells tylertoo to read the GeoParquet files
under that prefix as one dataset.

File: `01-tiles.sh`

```bash
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
```
```text
✓ Converted admin:country_code=BR → brazil.pmtiles
850,534 tiles across z0..z14 in 708.37s
...
attribute filter: reading 875/879 input row groups
...
(12.33 GiB, removed after export
...
2 tile(s) at z8 written at reduced extent; smallest extent 2048
...
z    tiles        total     mean      p50      p99      max
0        1        1,601    1,601    1,601    1,601    1,601
1        2        5,783    2,891    1,500    4,283    4,283
2        2       12,697    6,348    2,017   10,680   10,680
3        4       35,349    8,837    3,134   23,493   23,493
4        7      108,730   15,532    5,112   55,036   55,036
5       15      339,090   22,606    6,332   79,747   79,747
6       40    1,069,360   26,734   12,233   73,115   73,115
7      120    3,283,232   27,360   18,626   66,841   67,450
8      405   78,247,404  193,203  125,836  509,417  511,154
9    1,379  164,173,708  119,052   71,514  497,698  510,414
10    4,704  234,519,836   49,855   31,016  277,364  510,527
11   16,427  341,436,579   20,785   13,100  118,362  263,943
12   56,869  539,310,112    9,483    6,061   51,409  113,377
13  187,112  791,225,893    4,228    2,960   20,368   40,476
14  583,447  986,072,532    1,690    1,282    6,818   12,790
```

The filter keeps about 4.2 million fields. Because both years share row
groups, tylertoo still reads almost all the input from the bucket.

In the A5 cells, `point_count` stores the number of fields whose center
falls in the cell, and `metrics:area` stores their combined area. On the
field boundaries, `metrics:area` stores the area of each field.

## 2. Split the build across workers

The zoom level where `shard-plan` divides the tiles is called the *pivot*.
Cell bands must end below the pivot.

On a cluster, give every worker the same `OPTIONS` and both plan files, then
gather the five archives in one directory for `merge`. Each job reads from
the bucket on its own and can transfer up to 11.6 GB.

File: `02-shard.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

export AWS_ENDPOINT_URL=https://data.source.coop
BR=s3://ftw/global-data/predictions/vectors/alpha/results-by-admin-conf/admin:country_code=BR/
OPTIONS=(
  --filter "\"metrics:area\" >= 10000 AND \"determination:datetime\" >= '2025-01-01'"
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
  shard 0   tiles 349525..=855802  (506278 pivot tile(s), ~13881089 row(s), 25.0%)
  shard 1   tiles 855803..=857616  ( 1814 pivot tile(s), ~13882073 row(s), 25.0%)
  shard 2   tiles 857617..=858064  (  448 pivot tile(s), ~13899054 row(s), 25.0%)
  shard 3   tiles 858065..=1398100  (540036 pivot tile(s), ~13838304 row(s), 24.9%)
...
✓ Converted admin:country_code=BR → coarse.pmtiles
  1,975 tiles across z0..z9 in 131.28s
...
✓ Converted admin:country_code=BR → shard-0.pmtiles
  334,182 tiles across z10..z14 in 453.97s
...
✓ Converted admin:country_code=BR → shard-1.pmtiles
  271,771 tiles across z10..z14 in 345.37s
...
✓ Converted admin:country_code=BR → shard-2.pmtiles
  109,790 tiles across z10..z14 in 81.15s
...
✓ Converted admin:country_code=BR → shard-3.pmtiles
  132,816 tiles across z10..z14 in 111.67s
...
✓ 850,534 tiles (850,109 unique) from 5 archive(s), z0..z14 in 3.99s
...
10    4,704  234,519,671   49,855   31,016  277,364  510,527
...
```

`sharded.pmtiles` has as many tiles as `brazil.pmtiles`, but a few tiles at
zoom level 10 differ between the two builds.

<!-- --8<-- [end:build] -->
<!-- --8<-- [start:run] -->
## Run both builds from scripts

The repository includes a script for each build.

```bash
git clone https://github.com/geoparquet-io/tylertoo.git
mkdir brazil
cd brazil
bash ../tylertoo/examples/brazil-fields/01-tiles.sh
bash ../tylertoo/examples/brazil-fields/02-shard.sh
```
<!-- --8<-- [end:run] -->
