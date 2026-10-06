# Brazil fields: cloud data to a sharded, two-layer map

Build a PMTiles map of the soy belt around Sorriso, Mato Grosso, with field
polygons up close and a density grid at low zooms. The source is
[Fields of the World](https://fieldsofthe.world/) (FTW): about 3.2 billion
field polygons on [Source Cooperative](https://source.coop/ftw/global-data),
stored as one GeoParquet file per country subdivision. You only download
the row groups needed for this area.

This extends the
[Madagascar tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/madagascar/)
with:

- remote reads with `--files-from`, `--bbox`, and `--filter`
- DuckDB and the `tylertoo` Python package to derive a second layer
- `shard-plan` and `merge` to split tiling into independent jobs
- `pyramid` to combine layers with separate zoom ranges

Each step is a script in
[`examples/brazil-fields/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/brazil-fields).
CI runs the scripts against live data on each pull request and checks the
code and output shown here.

## Before you begin

| Tool | Install | Used for |
| --- | --- | --- |
| `tylertoo` | `cargo install tylertoo` | Overviews, sharded tiling, merging, pyramids |
| `tylertoo` Python package and DuckDB | `pip install tylertoo duckdb` | Validation and the density layer |
| `gpio` | `pip install geoparquet-io` | Row-group bounds for the shard plan |

Run the steps in order in an empty working directory. Step 1 downloads
about 120 MB; the remaining steps run locally in seconds.

## 1. Extract a window from three remote files

The Mato Grosso file is 1.1 GB; this window covers about 4% of it.
tylertoo reads each file's Parquet footer, then fetches the row groups whose
bounds intersect `--bbox`. FTW stores features in spatial order and records
row-group bounds, allowing tylertoo to skip the rest.

`--files-from` treats a list of local paths or URLs as one dataset, in the
listed order. Goiás and Mato Grosso do Sul lie outside the window, so each
costs only a footer read. `--filter` uses `metrics:area` to drop fields
smaller than one hectare. The output is an overview file whose finest
level contains the extract.

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

Five of 167 row groups matched: 4% of the three files' bytes. The download
accounts for most of the run time. Expanding the window to a whole state
requires more row-group reads; the footer count stays the same.

## 2. Derive the layers with Python and DuckDB

`tylertoo.validate` checks the GeoParquet overview against the
`geo:overviews` spec and returns a `dict`. DuckDB then exports its finest
level as `fields-raw.parquet` for tiling in step 3, and builds a density
grid for low zooms.

At z5, a pixel spans about 5 km, so individual fields are too small to see.
The grid uses 0.01° cells, each recording a field count and total area.

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

The 534 fields cover 36,733 ha, averaging 69 ha each: large, regular fields
typical of Mato Grosso's soy farms.

## 3. Tile the fields as a sharded build

A sharded build splits fine zooms into jobs that can run concurrently on
separate machines. This small window runs four shards sequentially in
seconds, using the same commands you would run on a cluster:

1. `gpio convert geoparquet` adds the row-group bounds and bbox covering
   missing from DuckDB's export.
2. `shard-plan` uses those bounds to divide tile space at z10, the pivot
   zoom, into ranges with roughly equal row counts.
3. The coarse job builds z8–z9, below the pivot, and saves `convert.plan`.
   Every shard uses this plan to generalize the data consistently.
4. Each shard builds z10–z14 for its range. `merge` copies the disjoint tiles
   from all five archives into one archive without recomputing them.

The window is centered on the corner of four z10 tiles, giving each shard
some fields to tile.

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

Each shard gets about 134 of the 534 fields. "191 unique" confirms the jobs
produced no duplicate tiles.

## 4. Combine both layers with `pyramid`

`pyramid` combines zoom bands into one archive. It tiles GeoParquet inputs
and copies tiles from PMTiles inputs. Here, `density` covers z0–z7 and
`fields` covers z8–z14. Style the map with filled density cells colored by
their `fields` count and outlined field polygons.

`tylertoo stats` reports tile sizes at each zoom.

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

The density tiles stay around 3 KB or less, holding just 266 squares. Field
tiles are heaviest at z11–z12, where each tile contains many whole fields,
and shrink at higher zooms. Drop
`brazil.pmtiles` onto [pmtiles.io](https://pmtiles.io/) to see both layers.

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
  [scaling guide](https://geoparquet-io.github.io/tylertoo/guides/scaling/#sharded-builds)
  covers sizing the jobs and running them under a scheduler.
- The [remote reads guide](https://geoparquet-io.github.io/tylertoo/guides/remote-reads/)
  covers `--filter` syntax, `--files-from` manifests, and row-group pruning.
- The [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
  explains every generalization knob.
- The [Python reference](https://geoparquet-io.github.io/tylertoo/reference/python/)
  lists `overview`, `export_pmtiles`, and `validate`.
