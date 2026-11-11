<!-- --8<-- [start:build] -->
# Brazil fields: a whole country from cloud data to tiles

Stream Brazil's 2025 field predictions from a cloud bucket into one PMTiles
archive with one `tylertoo tiles` command. Then split the same build into
independent jobs that can run at the same time on a cluster.

The input is the 45 GeoParquet files under Brazil's prefix on
[Source Cooperative](https://source.coop/ftw/global-data): 11.6 GB with
55,500,503 rows. The data holds predictions for 2024 and 2025, with about
27.5 million rows per year. The `determination:datetime` column tells the
years apart. Without the year in the filter, the map draws most fields
twice, once per year. Keeping only 2025 fields of one hectare or more
leaves 4,173,713 fields, or 7.5% of the rows, covering 103.3 million hectares.
The map shows A5 density cells at z0-z7 and field polygons at z8-z14.

Each step is a script in
[`examples/brazil-fields/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/brazil-fields).

## Before you begin

Install `tylertoo` with `cargo install tylertoo`. You don't need credentials:
requests are anonymous, and `tylertoo` logs a warning. Run the steps in
order in an empty working directory. Each step downloads the 11.6 GB.

On a 4-core machine with 16 GB of memory, step 1 took about 20 minutes,
peaked at 12 GB of memory, and needed about 21 GB of free disk for its
temporary files and the 3.3 GB archive. The archive was byte-identical to
the 64-core run. If the default disk is short on space, set `TMPDIR` to a
roomier disk. Step 2 runs five jobs in sequence, so it takes longer on one
machine.

## 1. Tile all of Brazil straight from the bucket

Point `AWS_ENDPOINT_URL` at Source Cooperative. The `--filter` keeps 2025
fields of one hectare or more using `determination:datetime` and
`metrics:area`, which stores square meters. The column names need double
quotes in the filter, so the whole filter is in double quotes and the
inner ones are escaped.

This run reads 875 of the 879 row groups and downloads all 11.6 GB once.
Both years share row groups, so the year filter cannot skip downloads.
The command counts the A5 cells first and then tiles the fields from the
same downloaded bytes.

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
  857,315 tiles across z0..z14 in 174.16s
...
  3 tile(s) still over the size limit after the size valve, written as is
...
 z    tiles          total     mean      p50      p99      max
 0        1          1,663    1,663    1,663    1,663    1,663
 1        2          6,305    3,152    1,895    4,410    4,410
 2        2         13,775    6,887    2,539   11,236   11,236
 3        4         38,872    9,718    3,685   25,405   25,405
 4        8        119,085   14,885    3,849   59,644   59,644
 5       15        370,907   24,727    7,067   85,940   85,940
 6       40      1,176,585   29,414   13,815   80,543   80,543
 7      120      3,605,503   30,045   20,952   73,098   73,800
 8      408     88,559,434  217,057  151,525  510,604  560,436
 9    1,391    176,905,218  127,178   76,355  499,280  511,638
10    4,725    248,244,053   52,538   32,908  296,306  509,116
11   16,499    359,181,678   21,769   13,682  123,829  277,193
12   57,165    558,874,935    9,776    6,240   53,571  118,290
13  188,497    838,446,327    4,448    3,106   21,519   43,053
14  588,438  1,043,813,089    1,773    1,344    7,163   13,556
```

The archive `brazil.pmtiles` holds 857,315 tiles across z0-z14 and is
3.3 GB. On a 64-core machine, the run took 4 minutes 19 seconds. The
intermediate overview file was 7.3 GiB and was removed after the export.

Across z0-z7, the map has 185,261 A5 cells, including 129,558 at z7.
At every cell zoom, `point_count` sums to 4,173,713.

Tiles are heaviest at z8-z9, where whole fields are drawn over the dense
soy belt. The size valve merges features to keep tiles under the 500 KB
compressed cap. Three tiles remain over the cap and are written anyway;
[issue #873](https://github.com/geoparquet-io/tylertoo/issues/873) tracks this.
Drop `brazil.pmtiles` onto [pmtiles.io](https://pmtiles.io/) to inspect it.

## 2. Split the same build into shard jobs

To distribute the build across a cluster, use `shard-plan` to cut the z10
tile space, the pivot, into four shards of about 13.9 million rows each.
The plan counts rows before the filter. The coarse job reads the whole
prefix and draws z0-z9: the cells and the z8-z9 fields. It saves the convert plan with `--save-plan`. Each shard
loads that plan with `--plan` and draws z10-z14 for its slice.

Give every job the same options through the `OPTIONS` array. The plan's
fingerprint covers them, including `--band`, and `tylertoo` refuses a
mismatched job. A cell band must end below the pivot.

The script runs the four shard jobs in sequence. On a cluster, they run on
separate machines at the same time. Once the jobs finish, `merge`
concatenates the five archives.

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
  1,991 tiles across z0..z9 in 137.83s
...
✓ Converted admin:country_code=BR → shard-0.pmtiles
  337,242 tiles across z10..z14 in 145.08s
...
✓ Converted admin:country_code=BR → shard-1.pmtiles
  273,749 tiles across z10..z14 in 119.98s
...
✓ Converted admin:country_code=BR → shard-2.pmtiles
  110,400 tiles across z10..z14 in 64.83s
...
✓ Converted admin:country_code=BR → shard-3.pmtiles
  133,933 tiles across z10..z14 in 79.99s
...
✓ 857,315 tiles (856,916 unique) from 5 archive(s), z0..z14 in 4.32s
...
10    4,725    248,244,039   52,538   32,908  296,306  509,140
...
```

On the same 64-core machine, the coarse job drew 1,991 tiles in 2 minutes
18 seconds. The four shard jobs drew 337,242, 273,749, 110,400, and 133,933
tiles, respectively, and took 1 to 2.5 minutes each. Merging took 4 seconds.
The whole script took 10.5 minutes on that machine.

The merged `sharded.pmtiles` has the same 857,315 tiles as step 1. All but
4 are byte-identical. Those 4 are z10 tiles at the 500 KB cap, where the
size valve cut slightly differently within a shard. That is why the z10
total differs slightly from step 1.
[Issue #854](https://github.com/geoparquet-io/tylertoo/issues/854) tracks this
difference.

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

- To tile a smaller area, add `--bbox`. The
  [remote reads guide](https://geoparquet-io.github.io/tylertoo/guides/remote-reads/)
  covers it, along with `--filter` syntax, `--files-from` manifests, and
  row-group pruning.
- To run shard jobs on a scheduler, see the
  [scaling guide](https://geoparquet-io.github.io/tylertoo/guides/scaling/#sharded-builds).
- The [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
  explains every generalization setting, including `--band`.
- Use `tylertoo pyramid` to combine bands from separate inputs or archives,
  such as cells from another dataset. See the same tuning reference.
- The [Python reference](https://geoparquet-io.github.io/tylertoo/reference/python/)
  lists `convert`, which takes the same bands as `bands=[...]`.
<!-- --8<-- [end:run] -->
