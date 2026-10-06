# Python tutorial: overviews you can query

This tutorial runs the tylertoo pipeline from Python, then treats its main
artifact as data. The **overview file** that `tylertoo.overview` writes is
plain GeoParquet with every zoom level inside, so DuckDB, pandas, or any
Parquet reader can open it. You build it once, query it like a table, and
export it to PMTiles as many times as you need.

The input is the same file as the
[CLI tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/cli/): 17,465
admin-4 boundary polygons for Madagascar. Each step below is a script in
[`examples/python-madagascar/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/python-madagascar).
CI runs every script on each pull request and checks that this page shows the
same code and the same output.

## Before you begin

```bash
pip install tylertoo duckdb
```

Run the scripts in order from one empty working directory. Each one reads the
files the one before it wrote.

## 1. Build the overview

`tylertoo.overview` reads the input and writes the multi-resolution file. It
returns a report as a plain `dict`, which a pipeline can log or assert on;
this step prints it.

This file has no GeoParquet `geo` metadata. tylertoo reads it anyway: it logs
a warning and treats the WKB `geometry` column as lon/lat WGS84. For larger files,
prepare the input with `gpio` first, as the CLI tutorial shows.

File: `01_overview.py`

```python
"""Build a multi-resolution overview file from a GeoParquet input."""

from pathlib import Path
from urllib.request import urlretrieve

import tylertoo

SRC = "fieldmaps-madagascar-adm4.parquet"
URL = f"https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/{SRC}"

if not Path(SRC).exists():
    urlretrieve(URL, SRC)

report = tylertoo.overview(SRC, "madagascar-ov.parquet", min_zoom=1, max_zoom=10)

print(f"{report['input_features']:,} features -> {report['total_rows']:,} rows")
print("zoom  level  features   vertices")
for lvl in report["levels"]:
    print(
        f"{lvl['zoom']:>4}  {lvl['level']:>5}"
        f"  {lvl['feature_count']:>8,}  {lvl['vertex_count']:>9,}"
    )
```

```text
17,465 features -> 43,848 rows
zoom  level  features   vertices
   1      0        40        173
   2      1       291      1,315
   3      2       517      2,859
   4      3       863      6,618
   5      4     1,428     14,872
   6      5     2,356     32,462
   7      6     3,888     68,155
   8      7     6,415    133,597
   9      8    10,585    249,642
  10      9    17,465  1,749,880
```

Each level covers one zoom. Level 0 keeps the 40 polygons large enough to see
at z1, and level 9 keeps all 17,465 with their full 1.7 million vertices. The
same knobs as the CLI are keyword arguments: `simplify_factor`,
`polygon_visibility`, `collapse`, `bbox`, and the rest. The
[Python reference](https://geoparquet-io.github.io/tylertoo/reference/python/)
lists them all.

## 2. Query the overview with DuckDB

Every row carries the `level` it belongs to. A query picks a resolution with
`WHERE level = N` and gets a complete, self-contained map at that scale.

File: `02_query.py`

```python
"""Query the overview file as plain GeoParquet with DuckDB."""

import duckdb

OV = "madagascar-ov.parquet"

# Every row carries the `level` it belongs to; one GROUP BY shows the pyramid.
duckdb.sql(f"""
    SELECT level, count(*) AS features
    FROM '{OV}'
    GROUP BY level
    ORDER BY level
""").show()

# Which districts are still on the map at level 1 (z2)?
duckdb.sql(f"""
    SELECT adm2_name AS district, count(*) AS features
    FROM '{OV}'
    WHERE level = 1
    GROUP BY district
    ORDER BY features DESC, district
    LIMIT 5
""").show()

# Pull one level out as its own GeoParquet file.
duckdb.sql(f"COPY (SELECT * FROM '{OV}' WHERE level = 4) TO 'level4.parquet'")
print(duckdb.sql("SELECT count(*) FROM 'level4.parquet'").fetchone()[0], "rows")
```

```text
┌───────┬──────────┐
│ level │ features │
│ int32 │  int64   │
├───────┼──────────┤
│     0 │       40 │
...
│     9 │    17465 │
└───────┴──────────┘
...
┌─────────────┬──────────┐
│  district   │ features │
│   varchar   │  int64   │
├─────────────┼──────────┤
│ Ihosy       │       17 │
│ Mahabo      │       15 │
│ Manja       │       13 │
│ Ikalamavony │       12 │
│ Miandrivazo │       11 │
└─────────────┴──────────┘

1428 rows
```

The second query asks what a reader sees at z2. Ihosy and Mahabo keep the
most polygons there, because their fokontany (the admin-4 units) are large
enough to clear the visibility gate at 9.8 km per pixel.
The last statement copies level 4 to its own file, a ready-made
generalization of the whole country at z5.

## 3. Validate and export

`tylertoo.validate` checks the file against the `geo:overviews` specification
and returns each check as a `dict`. `tylertoo.export_pmtiles` then writes the
archive. Because the overview is the durable artifact, a second export with
different settings needs no rebuild. Here the second export uses a coarser
tile grid (`extent=1024`) for a lighter archive.

File: `03_export.py`

```python
"""Validate the overview, then export it twice with different settings."""

from pathlib import Path

import tylertoo

OV = "madagascar-ov.parquet"

result = tylertoo.validate(OV)
failed = [c["name"] for c in result["checks"] if not c["passed"]]
print(f"valid={result['valid']}  checks={len(result['checks'])}  failed={failed}")

# The overview is the durable artifact: each export reuses it as built.
for name, extent in [("full", 4096), ("light", 1024)]:
    out = f"madagascar-{name}.pmtiles"
    report = tylertoo.export_pmtiles(
        OV, out, layer_name="boundaries", min_zoom=1, extent=extent
    )
    size = Path(out).stat().st_size
    print(
        f"{out}: z{report['min_zoom']}-z{report['max_zoom']},"
        f" {report['total_tiles']} tiles, {size:,} bytes"
    )
```

```text
valid=True  checks=12  failed=[]
madagascar-full.pmtiles: z1-z10, 752 tiles, 7,013,316 bytes
madagascar-light.pmtiles: z1-z10, 752 tiles, 5,854,763 bytes
```

The light archive is about 16% smaller. It stores vertex positions on a 1024-unit
grid per tile instead of 4096, which is often enough for admin boundaries
viewed at their own zoom. Both archives open on
[pmtiles.io](https://pmtiles.io/).

## Next steps

- The [CLI tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/cli/)
  adds input preparation with `gpio`, region previews, tile statistics, and
  decoding.
- The [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
  explains every generalization knob.
- The CLI has `decode`, `stats`, `merge`, and sharded builds, which the
  Python module does not expose yet.
