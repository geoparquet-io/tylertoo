# Brazil fields: cloud data to a sharded, two-layer map

This tutorial starts from data you never download whole. The
[Fields of the World](https://fieldsofthe.world/) predictions hold about
3.2 billion field polygons, published on
[Source Cooperative](https://source.coop/ftw/global-data) as one GeoParquet
file per country subdivision. The goal is a map of the soy belt around
Sorriso, Mato Grosso: field polygons up close, and a field-density grid when
zoomed out, in one PMTiles archive.

The tutorial uses tools the
[Madagascar tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/madagascar/)
does not:

- remote reads with `--files-from`, `--bbox`, and `--filter`;
- DuckDB and the `tylertoo` Python package, to derive a second layer;
- a sharded build: `shard-plan`, a coarse job, four shard jobs, and `merge`;
- `pyramid`, to put two layers with their own zoom ranges in one archive.

Each step is a script in
[`examples/brazil-fields/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/brazil-fields).
CI runs every script on each pull request, against the live data, and
checks that this page shows the same code and the same output.

## Before you begin

| Tool | Install | Used for |
| --- | --- | --- |
| `tylertoo` | `cargo install tylertoo` | Overviews, sharded tiling, merging, pyramids |
| `tylertoo` Python package and DuckDB | `pip install tylertoo duckdb` | Validation and the density layer |
| `gpio` | `pip install geoparquet-io` | Row-group bounds for the shard plan |

Run the steps in order from one empty working directory. Step 1 reads about
120 MB over the network; the rest runs locally in seconds.

## 1. Extract a window from three remote files

The Mato Grosso file alone is 1.1 GB, and the window you want covers about
4% of it. tylertoo reads the Parquet footer of each file first, then
fetches only the row groups whose bounding boxes touch `--bbox`. The FTW
files are sorted in space and record each row group's bounds, which is what
makes this pruning work.

`--files-from` takes a list of inputs, local paths or URLs, and treats them
as one dataset in the listed order. This step lists Goiás and Mato Grosso do
Sul too, to show that a file outside the window costs one footer read and
nothing more. `--filter` drops fields smaller than one hectare, using the
dataset's own `metrics:area` column.

The output is an overview file, as in the Madagascar tutorial. Its finest
level is the extract itself.

File: `01-extract.sh`

```bash
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
```

```text
...
bbox+attribute filter: reading 5/167 input row groups
...
remote input: 11 range requests, 119.46 MiB fetched of a 2988.72 MiB object (4.0%)
...
✓ Overview states.txt → fields-ov.parquet  (Duplicating mode)
  534 input features → 2,506 rows across 11 levels in 77.31s
  lvl        gsd(m)    features    vertices         bytes
    0       2445.98           3          21  1.15 KiB
...
   10          2.39         534     172,653  504.61 KiB
```

Five of 167 row groups matched, so tylertoo fetched 4% of the three files'
bytes. Most of the run time is that download. The same command on a whole
state reads more row groups, not more footers.

## 2. Derive the layers with Python and DuckDB

The overview file is plain GeoParquet, so the next step treats it as a table.
`tylertoo.validate` checks it against the `geo:overviews` spec first and
returns the result as a `dict`.

Then DuckDB does two things. It writes the finest level back out as
`fields-raw.parquet`, the extract the sharded build tiles in step 3. And it
builds the layer for low zooms. At z5 a pixel spans about 5 km, wider than
any field here, so single fields would vanish or turn to noise. A grid of
0.01° cells, each counting its fields and their total area, reads better at
that scale.

File: `02_layers.py`

```python
"""Check the extract, then derive the two layers the map needs."""

import duckdb

import tylertoo

OV = "fields-ov.parquet"

result = tylertoo.validate(OV)
print(f"valid={result['valid']}  checks={len(result['checks'])}")

con = duckdb.connect()
con.sql("INSTALL spatial; LOAD spatial;")
finest = con.sql(f"SELECT max(level) FROM '{OV}'").fetchone()[0]

# The finest level holds every field at full detail: it is the extract.
con.sql(f"""
    COPY (SELECT * EXCLUDE (level, coalesced_count) FROM '{OV}' WHERE level = {finest})
    TO 'fields-raw.parquet' (FORMAT parquet)
""")

# Zoomed out, single fields are too small to see. Count them per
# 0.01-degree cell (about 1 km) instead, and keep the cell as a square.
con.sql(f"""
    COPY (
        SELECT count(*) AS fields,
               round(sum("metrics:area") / 1e4, 1) AS hectares,
               ST_MakeEnvelope(gx * 0.01, gy * 0.01, (gx + 1) * 0.01, (gy + 1) * 0.01)
                   AS geometry
        FROM (
            SELECT "metrics:area",
                   floor(ST_X(ST_Centroid(geometry)) / 0.01) AS gx,
                   floor(ST_Y(ST_Centroid(geometry)) / 0.01) AS gy
            FROM '{OV}' WHERE level = {finest}
        )
        GROUP BY gx, gy
        ORDER BY gx, gy
    ) TO 'density.parquet' (FORMAT parquet)
""")

con.sql("""
    SELECT count(*) AS cells, sum(fields) AS fields,
           max(fields) AS busiest_cell, round(sum(hectares)) AS hectares
    FROM 'density.parquet'
""").show()
```

```text
valid=True  checks=12
┌───────┬────────┬──────────────┬──────────┐
│ cells │ fields │ busiest_cell │ hectares │
│ int64 │ int128 │    int64     │  double  │
├───────┼────────┼──────────────┼──────────┤
│   266 │    534 │            9 │  36733.0 │
└───────┴────────┴──────────────┴──────────┘
```

The 534 fields cover 36,733 ha, an average of 69 ha each. Large, regular
fields like these are typical of the soy farms in Mato Grosso.

## 3. Tile the fields as a sharded build

A single `tylertoo tiles` run is one process on one machine. For a country or
a continent, a sharded build splits the fine zooms into independent jobs that
can run at the same time on separate machines. This window is small, so the
four shards run here one after another in seconds. The commands are the ones a
cluster would run.

The build has four parts:

1. `gpio convert geoparquet` records row-group bounds and a bbox covering.
   `shard-plan` balances shards by those bounds, and DuckDB's export in step 2
   did not write them.
2. `shard-plan` cuts the tile space at a pivot zoom, here z10, into ranges of
   about equal row count.
3. The coarse job builds the zooms below the pivot, z8 and z9, and saves
   `convert.plan`. Every shard reads that plan, so all jobs generalize the
   data the same way.
4. Each shard builds z10 to z14 for its own range. `merge` then concatenates
   the five archives. They hold disjoint tiles, so the merge copies tiles and
   recomputes nothing.

The window is centered on the corner of four z10 tiles, so each shard gets a
share of the work.

File: `03-shard.sh`

```bash
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
```

```text
...
Cut 4 shard(s) at pivot z10 for fields.parquet
  shard 0   tiles 349525..=855392  (505868 pivot tile(s), ~134 row(s), 25.1%)
  shard 1   tiles 855393..=855393  (    1 pivot tile(s), ~134 row(s), 25.1%)
  shard 2   tiles 855394..=855398  (    5 pivot tile(s), ~134 row(s), 25.1%)
  shard 3   tiles 855399..=1398100  (542702 pivot tile(s), ~134 row(s), 25.1%)
...
✓ Converted fields.parquet → coarse.pmtiles
  6 tiles across z8..z9 in 0.09s
...
✓ Converted fields.parquet → shard-0.pmtiles
  39 tiles across z10..z14 in 0.26s
...
✓ Converted fields.parquet → shard-3.pmtiles
  46 tiles across z10..z14 in 0.27s
Merging 5 archive(s) → fields.pmtiles
...
✓ 191 tiles (191 unique) from 5 archive(s), z8..z14 in 0.00s
```

Each shard got about 134 of the 534 fields. "191 unique" confirms that no two
jobs wrote the same tile.

## 4. Combine both layers with `pyramid`

`pyramid` builds one archive from bands, each owning a zoom range. A band can
be a GeoParquet file, tiled here, or a PMTiles archive, merged as it is. The
density grid becomes layer `density` at z0 to z7. The sharded fields archive
becomes layer `fields` at z8 to z14. A map style then needs two rules: fill
the density cells by `fields`, and outline the fields.

`tylertoo stats` reports tile weight per zoom, as in the Madagascar tutorial.

File: `04-pyramid.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

# One archive, two layers: density cells at z0-z7, fields at z8-z14.
tylertoo pyramid brazil.pmtiles \
  --band 0-7:density.parquet:density \
  --band 8-14:fields.pmtiles:fields
tylertoo stats brazil.pmtiles
```

```text
  z0-7 -> layer "density": 8 tiles
  z8-14 -> layer "fields": 191 tiles
✓ Built 2 band(s) → brazil.pmtiles (199 tiles)
 z  tiles    total    mean     p50     p99     max
 0      1      143     143     143     143     143
...
 7      1    3,142   3,142   3,142   3,142   3,142
 8      2   19,574   9,787   9,113  10,461  10,461
...
14    125  233,094   1,864   1,799   4,586   5,055
...
```

The density tiles stay near 3 KB at every zoom, since 266 squares is all they
hold. The field tiles are heaviest at z11 and z12, where one tile still
holds many whole fields, and shrink as the zoom increases. Drop `brazil.pmtiles` onto
[pmtiles.io](https://pmtiles.io/) to see both layers.

## Run the whole workflow

```bash
git clone https://github.com/geoparquet-io/tylertoo.git
mkdir brazil
cd brazil
for step in ../tylertoo/examples/brazil-fields/0*; do
  case "$step" in
    *.py) python "$step" ;;
    *) bash "$step" ;;
  esac
done
```

## Next steps

- To tile a whole state, remove `--bbox`, raise `--shards`, and run the shard
  jobs on separate machines. The
  [sharded builds guide](https://geoparquet-io.github.io/tylertoo/guides/sharded-builds/)
  covers sizing the jobs and running them under a scheduler.
- The [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
  explains `--filter` syntax and every generalization knob.
- The [Python reference](https://geoparquet-io.github.io/tylertoo/reference/python/)
  lists `overview`, `export_pmtiles`, and `validate`.
