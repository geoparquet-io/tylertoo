<!-- --8<-- [start:build] -->
# Brazil fields: a whole country from cloud data to tiles

Stream all of Brazil's field data from a cloud bucket into one PMTiles
archive with one `tylertoo tiles` command. Then split the same build into
independent jobs that can run at the same time on a cluster.

The input is the 45 GeoParquet files under Brazil's prefix on
[Source Cooperative](https://source.coop/ftw/global-data): 11.6 GB with
55,500,503 rows. Filtering for fields of one hectare or more keeps
7,872,965 fields, or 14% of the rows, covering 186.9 million hectares.
The map shows A5 density cells at z0-z7 and field polygons at z8-z14.

Each step is a script in
[`examples/brazil-fields/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/brazil-fields).

## Before you begin

Install `tylertoo` with `cargo install tylertoo`. You don't need credentials:
requests are anonymous, and `tylertoo` logs a warning. Run the steps in
order in an empty working directory. Each step downloads the 11.6 GB.

TODO(disk/runtime on a 4-core, 16 GB machine)

## 1. Tile all of Brazil straight from the bucket

Point `AWS_ENDPOINT_URL` at Source Cooperative. The `--filter` keeps fields
of one hectare or more using `metrics:area`, which stores square meters.
This run reads 876 of the 879 row groups; the filter skips the other three.
It downloads the 11.6 GB once, counts the A5 cells first, and then tiles
the fields from the same downloaded bytes.

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

# Brazil is one GeoParquet file per state under this prefix. Keep every
# field of one hectare or more in the country: A5 cells that count them at
# z0-z7, the fields themselves at z8-z14.
tylertoo tiles s3://ftw/global-data/predictions/vectors/alpha/results-by-admin-conf/admin:country_code=BR/ \
  --filter '"metrics:area" >= 10000' \
  --band 0-7:a5 --band 8-14:geom \
  --accumulate-attribute metrics:area:sum \
  --layer-name fields \
  brazil.pmtiles
tylertoo stats brazil.pmtiles
```
```text
✓ Converted admin:country_code=BR → brazil.pmtiles
  971,220 tiles across z0..z14 in 352.81s
...
  13 tile(s) still over the size limit after the size valve, written as is
...
 z    tiles          total     mean      p50      p99      max
 0        1          1,726    1,726    1,726    1,726    1,726
 1        2          6,443    3,221    1,951    4,492    4,492
 2        2         14,567    7,283    2,796   11,771   11,771
 3        4         41,592   10,398    4,272   27,366   27,366
 4        8        128,564   16,070    5,867   65,087   65,087
 5       16        402,537   25,158    9,637   88,642   88,642
 6       42      1,290,889   30,735   14,736   84,260   84,260
 7      123      4,062,789   33,030   24,243   76,936   78,333
 8      423    112,136,969  265,099  273,580  603,081  818,430
 9    1,491    280,961,239  188,438  123,085  509,433  736,964
10    5,086    458,008,076   90,052   54,776  474,443  510,423
11   17,639    676,972,823   38,379   23,509  220,599  468,592
12   61,687  1,039,601,527   16,852   10,499   92,875  205,479
13  208,780  1,507,519,816    7,220    4,820   37,222   78,852
14  675,916  1,806,029,561    2,671    1,924   12,247   24,152
```

The archive `brazil.pmtiles` holds 971,220 tiles across z0-z14 and is
5.9 GB. On a 64-core machine, the run took 7 minutes 24 seconds and peaked
at 36 GB of memory. The intermediate overview file was 13 GiB and was
removed after the export.

Across z0-z7, the map has 203,893 A5 cells, including 143,895 at z7.
At every cell zoom, `point_count` sums to 7,872,965.

Tiles are heaviest at z8-z9, where whole fields are drawn over the dense
soy belt. The size valve merges features to keep tiles under the 500 KB
compressed cap. Thirteen tiles remain over the cap and are written anyway;
[issue #873](https://github.com/geoparquet-io/tylertoo/issues/873) tracks this.
Drop `brazil.pmtiles` onto [pmtiles.io](https://pmtiles.io/) to inspect it.

## 2. Split the same build into shard jobs

To distribute the build across a cluster, use `shard-plan` to cut the z10
tile space, the pivot, into four shards of about 13.9 million rows each.
The coarse job reads the whole prefix and draws z0-z9: the cells and the
z8-z9 fields. It saves the convert plan with `--save-plan`. Each shard
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
  shard 0   tiles 349525..=855802  (506278 pivot tile(s), ~13881089 row(s), 25.0%)
  shard 1   tiles 855803..=857616  ( 1814 pivot tile(s), ~13882073 row(s), 25.0%)
  shard 2   tiles 857617..=858064  (  448 pivot tile(s), ~13899054 row(s), 25.0%)
  shard 3   tiles 858065..=1398100  (540036 pivot tile(s), ~13838304 row(s), 24.9%)
...
✓ Converted admin:country_code=BR → coarse.pmtiles
  2,112 tiles across z0..z9 in 172.87s
...
✓ Converted admin:country_code=BR → shard-0.pmtiles
  393,704 tiles across z10..z14 in 206.00s
...
✓ Converted admin:country_code=BR → shard-1.pmtiles
  305,047 tiles across z10..z14 in 176.37s
...
✓ Converted admin:country_code=BR → shard-2.pmtiles
  121,451 tiles across z10..z14 in 78.39s
...
✓ Converted admin:country_code=BR → shard-3.pmtiles
  148,906 tiles across z10..z14 in 99.51s
...
✓ 971,220 tiles (970,781 unique) from 5 archive(s), z0..z14 in 6.91s
...
10    5,086    458,008,178   90,052   54,776  474,450  510,423
11   17,639    676,972,793   38,379   23,509  220,599  468,592
...
```

On the same 64-core machine, the coarse job drew 2,112 tiles in 4 minutes.
The four shard jobs drew 393,704, 305,047, 121,451, and 148,906 tiles,
respectively, and took 1.5 to 3.5 minutes each. Merging took 7 seconds.

The merged `sharded.pmtiles` has the same 971,220 tiles as step 1. All but
37 are byte-identical. Those 37 are z10-z11 tiles at the 500 KB cap, where
the size valve made a slightly different cut within a shard. Each differs
by one or two features, which is why the z10 and z11 totals differ slightly
from step 1. [Issue #854](https://github.com/geoparquet-io/tylertoo/issues/854)
tracks this difference.

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
