<!-- --8<-- [start:build] -->
# Brazil fields: a whole country from cloud data to tiles

Stream Brazil's 2025 field predictions from a cloud bucket into one PMTiles
archive with one `tylertoo tiles` command. Then split the same build into
independent jobs that can run at the same time on a cluster.

The input is the 45 GeoParquet files under Brazil's prefix on
[Source Cooperative](https://source.coop/ftw/global-data): 11.6 GB with
55,500,520 rows. The data holds about 27.5 million rows for 2024 and
28.0 million for 2025. The `determination:datetime` column identifies
each prediction's year. Without the year in the filter, the map draws most fields
twice, once per year. Keeping only 2025 fields of one hectare or more
leaves 4,173,713 fields, or 7.5% of the rows, covering 103.3 million hectares.
The map shows A5 density cells at z0-z7 and field polygons at z8-z14.

Each step is a script in
[`examples/brazil-fields/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/brazil-fields).

## Before you begin

Install `tylertoo` with `cargo install tylertoo`. You don't need credentials:
requests are anonymous, and `tylertoo` logs a warning. Run the steps in
order in an empty working directory. Step 1 downloads about 11.6 GB after
staging the columns its first pass needs. In step 2, the coarse job and
each shard download their own input. In the worst case, each of the five
jobs downloads the full dataset.

If the default disk is short on space, set `TMPDIR` to a roomier disk.
Alternatively, pass `--spill-dir PATH` to `tylertoo tiles` to choose a
directory for staged remote input and the intermediate overview. Step 2
runs five jobs in sequence, so it takes longer than step 1 on one machine.

The output and timings in both steps are from a run on November 27, 2026.
The run used a 64-core aarch64 development machine with a 40 GB memory cap.
The whole tutorial, both steps, took 35 minutes on that machine.

## 1. Tile all Brazil straight from the bucket

Point `AWS_ENDPOINT_URL` at Source Cooperative. tylertoo reads an `s3://`
prefix ending in `/` as one dataset. The `--filter` keeps 2025
fields of one hectare or more using `determination:datetime` and
`metrics:area`, which stores square meters. Enclose the whole filter in
double quotes, and escape the inner quotes around column names.

This run reads 875 of the 879 row groups. It stages 1.24 GB of the columns
the first pass needs. Then it fetches 11,062.65 MiB (about 11.6 GB, or 100%
of the object) in 5,340 range requests. Both years share row groups, so the
year filter cannot skip most downloads. The command counts the A5 cells
first and then tiles the fields.

Use `--band 0-7:a5` to draw A5 cells at z0-z7. Each cell holds its ID in
`cell` and the number of fields whose centroid falls in it in `point_count`.
Add `--accumulate-attribute metrics:area:sum` to sum the field areas in
`metrics:area`. At z8-z14, use `--band 8-14:geom` to draw field polygons,
where `metrics:area` is each field's own area.

Name the shared layer `fields` with `--layer-name`. Style by zoom or by
whether `point_count` is present.

Choose another cell system by changing `a5` in `--band 0-7:a5`:

| System | Cells |
| --- | --- |
| `grid` | Squares aligned to the tile grid at each zoom |
| `h3` | Hexagons |
| `s2` | Quadrilaterals on a cube |
| `a5` | Pentagons with equal areas |

In every system, cell resolution follows the zoom, and each field counts
in the cell that holds its centroid. This keeps counts exact at every zoom.

Run the script to build the archive. `tylertoo stats` reports tile sizes
per zoom.

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

The archive `brazil.pmtiles` holds 850,534 tiles across z0-z14 and is
3.1 GB. On the 64-core development machine with a 40 GB memory cap, step 1
took about 11 minutes 48 seconds. The command writes a 12.33 GiB
intermediate overview file and removes it after the export.

Across z0-z7, the map has 185,261 A5 cells, including 129,558 at z7,
counting each cell once. At every cell zoom z0-z7, `point_count` sums to
4,173,713 and `metrics:area` sums to 103.3 million hectares, also counting
each cell once. A decoded tile file lists a cell once per tile it touches.

Tiles are heaviest at z8-z9, where the map shows whole fields across
areas with dense soy fields. The size valve merges features to keep tiles near the size cap.
Two z8 tiles use a reduced extent of 2048 to fit the cap. No tile remains
over it.
Drop `brazil.pmtiles` onto [pmtiles.io](https://pmtiles.io/) to inspect it.

## 2. Split the same build into shard jobs

To distribute the build across a cluster, use `shard-plan` to cut the z10
tile space, the pivot, into four shards of about 13.9 million rows each.
The plan counts rows before the filter. The coarse job reads the whole
prefix and draws z0-z9: the cells and the z8-z9 fields. It saves the
convert plan with `--save-plan`. Each shard loads that plan with `--plan`
and draws z10-z14 for its slice.

Give every job the same options through the `OPTIONS` array. The plan's
fingerprint covers them, including `--band`, and `tylertoo` refuses a
mismatched job. A cell band must end below the pivot.

The script runs the four shard jobs in sequence. On a cluster, they run on
separate machines at the same time. Once the jobs finish, `merge`
concatenates the five archives.

A shard with no rows writes an empty archive and exits 0; `merge` skips empty archives and refuses archives whose tiles overlap.

In this run, shards 0-3 downloaded 11.6, 10.4, 5.2, and 6.8 GB, respectively.

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

The merged `sharded.pmtiles` has the same 850,534 tiles as step 1.
A few z10 tiles at the size cap differ between the two builds
([#854](https://github.com/geoparquet-io/tylertoo/issues/854)). The z10 total
differs slightly: 234,519,836 bytes in step 1 and 234,519,671 bytes in step 2.

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

- The [CLI reference](https://geoparquet-io.github.io/tylertoo/reference/cli/)
  describes every flag, including `--bbox`, `--filter`, and `--shard`, and
  the `shard-plan` and `merge` commands.
- The [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
  explains every generalization setting, including `--band`.
- Use `tylertoo pyramid` to combine bands from separate inputs or archives,
  such as cells from another dataset. See the same tuning reference.
- The [Python reference](https://geoparquet-io.github.io/tylertoo/reference/python/)
  lists `convert`, which takes the same bands as `bands=[...]`.
<!-- --8<-- [end:run] -->
