# Madagascar boundaries: from a raw export to PMTiles

Convert 17,465 admin-4 boundary polygons for Madagascar (28 MB), from the
[fieldmaps.io](https://fieldmaps.io/) global boundaries, into a PMTiles archive
using the command line. First, build an **overview file**: queryable
GeoParquet containing your data at every zoom level.

Each step is a script in
[`examples/madagascar-boundaries/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/madagascar-boundaries).
CI runs the scripts on every pull request and checks this page's code and output.

## Before you begin

You need three tools on your `PATH`:

| Tool | Install | Used for |
| --- | --- | --- |
| `tylertoo` | `cargo install tylertoo` | Overviews, tiles, validation, decoding |
| `gpio` | `pip install geoparquet-io` | Sorting and repacking the input |
| `duckdb` | `pip install duckdb-cli` | Adding GeoParquet metadata |

Run the scripts in order from the same empty working directory. Each step
reads the previous step's files.

## 1. Prepare the input

tylertoo reads WGS84 GeoParquet. Hilbert sorting puts geographic neighbors
near each other on disk; row groups of a few megabytes or more improve read
performance. Raw database and Spark exports often need both adjustments.

This export also lacks `geo` metadata, despite having valid Well-Known Binary
(WKB) geometry. A DuckDB round trip adds the metadata so GeoParquet tools can
identify the geometry column. Then
[geoparquet-io](https://github.com/geoparquet-io/geoparquet-io) (`gpio`) sorts
and repacks the file.

File: `01-prepare.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

# Download the input once: 17,465 admin-4 boundary polygons (28 MB).
SRC=fieldmaps-madagascar-adm4.parquet
if [ ! -f "$SRC" ]; then
  curl -fsSLO "https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/$SRC"
fi

# The export holds WKB geometry but no GeoParquet `geo` metadata.
# DuckDB's spatial extension writes the metadata on a round trip.
duckdb -c "
  INSTALL spatial;
  LOAD spatial;
  COPY (
    SELECT * REPLACE (ST_GeomFromWKB(geometry) AS geometry)
    FROM '$SRC'
  ) TO 'raw.parquet' (FORMAT parquet);
"

# Hilbert-sort, add a bbox column, compress with ZSTD, and pack 16 MB
# row groups, all in one gpio pass.
gpio convert geoparquet raw.parquet prepared.parquet --row-group-size-mb 16
gpio inspect prepared.parquet
```

```text
Output: prepared.parquet (17.12 MB)
✓ Output passes GeoParquet validation (metadata checks)
...
Rows: 17,465
Row Groups: 3
Compression: ZSTD
GeoParquet Version: 1.1.0
CRS: OGC:CRS84 (default)
Geometry Types: MultiPolygon
Bbox: [43.187051, -25.606140, 50.493403, -11.949812]
...
```

`gpio inspect` confirms lon/lat WGS84 (`OGC:CRS84`) and three
Zstandard-compressed row groups. For projected data, run
`gpio convert reproject input.parquet wgs84.parquet -d EPSG:4326` before sorting.

## 2. Preview one region

Test settings on a small region with `--bbox`, which takes lon/lat bounds as
`xmin,ymin,xmax,ymax`. tylertoo reads only row groups whose bounds intersect
the box, so a city preview from a country file finishes in seconds.

File: `02-preview.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

# Build a quick overview of Antananarivo only. --bbox reads just the row
# groups whose bounds touch the box (xmin,ymin,xmax,ymax in lon/lat).
tylertoo overview prepared.parquet preview-ov.parquet \
  --max-zoom 12 \
  --bbox=47.3,-19.1,47.7,-18.7
```

```text
✓ Overview prepared.parquet → preview-ov.parquet  (Duplicating mode)
  943 input features → 3,162 rows across 10 levels in 0.21s
  lvl        gsd(m)    features    vertices         bytes
    0       4891.97           1           4  4.18 KiB
    1       2445.98          30         124  8.23 KiB
    2       1222.99         246       1,070  31.22 KiB
    3        611.50         256       1,450  35.54 KiB
    4        305.75         256       2,025  41.45 KiB
    5        152.87         256       2,753  48.67 KiB
    6         76.44         256       3,562  56.71 KiB
    7         38.22         346       6,019  86.70 KiB
    8         19.11         572      11,863  149.89 KiB
    9          9.55         943      58,321  530.90 KiB
  note: 3 empty level(s) omitted (z0, z1, z2) — no features visible at those scales; the pyramid starts at the coarsest non-empty level
```

The box contains 943 polygons. `gsd` (ground sample distance) is the size of
one pixel in meters. At z0–z2, pixels span 10–40 km: all polygons fall below
the visibility gate, so those levels are omitted. Add `--collapse` to show
small polygons as dots at coarse zooms. The
[tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
explains the available settings.

## 3. Build and validate the overview

`tylertoo overview` writes all levels to one GeoParquet file with a `level`
column. Each level thins and simplifies features for its scale; the finest
preserves the source geometry.

`--max-zoom` defaults to 6 for a continental view. Use z10 to see individual
communes, or `--max-zoom auto` to estimate a zoom from feature size and spacing.

`tylertoo validate` checks `geo:overviews` metadata, zoom-to-resolution
mapping, and per-level structure.

File: `03-overview.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

# Build the full pyramid, z1 to z10, then check it against the spec.
tylertoo overview prepared.parquet madagascar-ov.parquet \
  --min-zoom 1 \
  --max-zoom 10
tylertoo validate madagascar-ov.parquet
```

```text
✓ Overview prepared.parquet → madagascar-ov.parquet  (Duplicating mode)
  17,465 input features → 43,848 rows across 10 levels in 0.46s
  lvl        gsd(m)    features    vertices         bytes
    0      19567.88          40         173  10.71 KiB
    1       9783.94         291       1,315  44.74 KiB
    2       4891.97         517       2,859  80.76 KiB
    3       2445.98         863       6,618  151.04 KiB
    4       1222.99       1,428      14,872  288.45 KiB
    5        611.50       2,356      32,462  547.00 KiB
    6        305.75       3,888      68,155  1.00 MiB
    7        152.87       6,415     133,597  1.80 MiB
    8         76.44      10,585     249,642  3.06 MiB
    9         38.22      17,465   1,749,880  17.45 MiB
Validating madagascar-ov.parquet
...
✓ valid overview file (12 checks passed)
```

At 19.6 km per pixel, 40 polygons remain. From level 2 to level 8, the count
grows about 1.65 times per zoom, the default `--drop-rate`. Level 9 holds all
17,465 polygons at full detail. Any Parquet reader can open the file; the
[Brazil tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/brazil/)
shows how to query an overview with DuckDB.

## 4. Export PMTiles

`tylertoo export-pmtiles` cuts the overview levels into vector tiles without
recomputing geometry. `--layer-name` sets the MVT source layer used by your
map style; `--min-zoom 1` starts at the overview's first zoom.

`tylertoo stats` reads the archive directory and reports tile sizes by zoom
to help identify slow-loading tiles.

File: `04-export.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

# Cut the overview into vector tiles, then report tile weight per zoom.
tylertoo export-pmtiles madagascar-ov.parquet madagascar.pmtiles \
  --layer-name boundaries \
  --min-zoom 1
tylertoo stats madagascar.pmtiles
```

```text
Exporting madagascar-ov.parquet → madagascar.pmtiles
  mode=duplicating zooms z1..z10
  z1  (level 0):       1 tiles,        40 features
  z2  (level 1):       1 tiles,       291 features
...
  z10 (level 9):     524 tiles,     26831 features, 8 collapsed at extent

✓ 752 tiles, 61885 features, 0 oversized tiles in 0.74s
...
 z  tiles      total    mean     p50     p99     max
 1      1      4,410   4,410   4,410   4,410   4,410
...
10    524  4,057,907   7,744   6,100  28,397  88,323

Largest 10 tile(s):
 z    x    y   bytes
10  647  566  88,323
...
```

Polygons crossing tile edges appear in every tile they touch, so tile
features outnumber overview features. Clipped slivers that vanish on the
tile's 4096-unit grid are omitted and counted as "collapsed at extent".
The largest tile is 88 KB at z10, below the default 500 KB limit.

View `madagascar.pmtiles` at [pmtiles.io](https://pmtiles.io/), or host it on
any server that supports HTTP range requests. No tile server is needed.

## 5. Decode tiles back to GeoParquet

`tylertoo decode` extracts chosen zooms from any PMTiles v3 vector archive
to GeoParquet. Use it to inspect a map's features at a zoom or compare
archives in SQL.

File: `05-decode.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

# Read the z8 tiles back out as GeoParquet.
tylertoo decode madagascar.pmtiles z8.parquet --zoom 8
```

```text
Decoding madagascar.pmtiles → z8.parquet
  zooms z8..z8, layers: boundaries

✓ 8,268 features from 44 tiles (0 skipped as degenerate) in 0.13s
```

Decoded geometry retains tile simplification and clipping; a feature spanning
two tiles appears twice. The `zoom`, `layer`, and `mvt_id` columns identify
each row's origin.

## Run the whole workflow

Run all five scripts from a fresh directory:

```bash
git clone https://github.com/geoparquet-io/tylertoo.git
mkdir madagascar
cd madagascar
for step in ../tylertoo/examples/madagascar-boundaries/0*.sh; do
  bash "$step"
done
```

## Next steps

- Combine steps 3 and 4 with
  `tylertoo prepared.parquet madagascar.pmtiles --max-zoom 10`. Add
  `--keep-overview madagascar-ov.parquet` to save the overview too.
- The [Brazil tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/brazil/)
  reads cloud data with `--files-from`, `--bbox`, and `--filter`, splits the
  tiling across shards, and combines two layers with `pyramid`.
- The [CLI reference](https://geoparquet-io.github.io/tylertoo/reference/cli/)
  lists every flag with its default.
