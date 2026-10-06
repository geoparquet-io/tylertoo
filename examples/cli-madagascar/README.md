# CLI tutorial: Madagascar admin boundaries

This tutorial takes one real file from a raw export to a PMTiles archive you
can drop on a map, using only the command line. Along the way it builds the
artifact tylertoo is organized around: an **overview file**, a GeoParquet file
that holds every zoom level of your data and stays queryable.

The input is 17,465 admin-4 boundary polygons for Madagascar (28 MB), from
the [fieldmaps.io](https://fieldmaps.io/) global boundaries. Each step below
is a script in
[`examples/cli-madagascar/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/cli-madagascar).
CI runs every script on each pull request and checks that this page shows the
same code and the same output.

## Before you begin

You need three tools on your `PATH`:

| Tool | Install | Used for |
| --- | --- | --- |
| `tylertoo` | `cargo install tylertoo` | Overviews, tiles, validation, decoding |
| `gpio` | `pip install geoparquet-io` | Sorting and repacking the input |
| `duckdb` | `pip install duckdb-cli` | Adding GeoParquet metadata |

Run each script from one empty working directory. Every step reads the files
the step before it wrote.

## 1. Prepare the input

tylertoo reads WGS84 GeoParquet. It runs best when the file is Hilbert-sorted,
so geographic neighbors sit near each other on disk, and packed into row
groups of a few megabytes or more. A raw export from a database or a Spark job
is rarely in that shape, and one preparation pass pays off at every zoom level
that follows.

This export has a second problem. Its geometry column holds valid WKB, but the
file carries no `geo` metadata, so GeoParquet tools cannot tell which column
is the geometry. A DuckDB round trip writes that metadata. Then
[geoparquet-io](https://github.com/geoparquet-io/geoparquet-io) (`gpio`) sorts
and repacks the file in one pass.

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

`gpio inspect` confirms what tylertoo needs. The CRS is `OGC:CRS84`, which is
lon/lat WGS84, and the rows sit in three ZSTD row groups. If your own data is
in a projected CRS, add `gpio convert reproject input.parquet wgs84.parquet -d
EPSG:4326` before the sort.

## 2. Preview one region

On a large input, try your settings on a small window before you commit to the
whole file. `--bbox` takes lon/lat bounds as `xmin,ymin,xmax,ymax`. tylertoo
reads only the row groups whose bounds intersect the box, so a city-sized
preview of a country-sized file finishes in seconds.

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

The box holds 943 of the 17,465 polygons. Read the table from the top: `gsd`
is the ground sample distance, the size of one pixel in meters at that level.
At z0 to z2 a pixel spans 10 to 40 km, and every polygon in the box is smaller
than the visibility gate, so tylertoo leaves those levels out and says so. To
keep small polygons visible as dots at coarse zooms, add `--collapse`. The
[tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
covers that knob and the rest.

## 3. Build and validate the overview

`tylertoo overview` builds the full pyramid. Each level holds a thinned,
simplified copy of the features sized for its scale, and the finest level
holds the source geometry unchanged. All levels go into one GeoParquet file,
tagged by a `level` column.

`--max-zoom` defaults to 6, which suits a continental view. A map that zooms
in to individual communes needs more, so this step asks for z10. If you are
not sure how far to go, `--max-zoom auto` estimates a zoom from the size and
spacing of your features.

`tylertoo validate` then checks the file against the `geo:overviews`
specification: the level metadata, the zoom-to-resolution ladder, and the
per-level structure.

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

The table shows the idea in ten lines. At 19.6 km per pixel, 40 of the 17,465
polygons survive. The count roughly doubles with each zoom, and level 9 holds
all of them at full detail. The file is still ordinary GeoParquet, so any
Parquet reader opens it. The [Python tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/python/)
queries it with DuckDB.

## 4. Export PMTiles

`tylertoo export-pmtiles` cuts each level into vector tiles and writes one
PMTiles archive. It recomputes no geometry: the levels you built in step 3
become the tiles. `--layer-name` sets the MVT source layer your map style
refers to, and `--min-zoom 1` starts the archive at the overview's first zoom.

`tylertoo stats` then reads the archive's directory and reports how heavy the
tiles are at each zoom. Use it to find the zooms and tiles that will load
slowest.

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

Tile features outnumber level features because a polygon that crosses a tile
edge is clipped into every tile it touches. A few slivers from those clips
shrink to nothing at the tile's 4096-unit grid; the export counts them as
"collapsed at extent" and leaves them out. No tile exceeds the 500 KB default
size limit, and the largest, at z10, is 88 KB.

To see the result, drop `madagascar.pmtiles` onto
[pmtiles.io](https://pmtiles.io/). Any server that honors HTTP range requests
can host the file; there is no tile server to run.

## 5. Decode tiles back to GeoParquet

`tylertoo decode` reads any PMTiles v3 vector archive, not only ones tylertoo
wrote, and writes the features of the zooms you choose as GeoParquet. Use it
to check what a map actually shows at a zoom, or to compare two archives in
SQL.

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

The output is the tiled form of the data, not the source. Geometry is
simplified and clipped at tile edges, and a feature that spans two tiles
appears twice. The `zoom`, `layer`, and `mvt_id` columns record where each row
came from.

## Run the whole workflow

The steps write their files to the current directory, so run them from a
fresh one:

```bash
git clone https://github.com/geoparquet-io/tylertoo.git
mkdir madagascar
cd madagascar
for step in ../tylertoo/examples/cli-madagascar/0*.sh; do
  bash "$step"
done
```

## Next steps

- One command does steps 3 and 4 when you do not need the overview file:
  `tylertoo prepared.parquet madagascar.pmtiles --max-zoom 10`. Add
  `--keep-overview madagascar-ov.parquet` to keep it anyway.
- The [Python tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/python/)
  runs the same pipeline from Python and queries the overview with DuckDB.
- The [guides](https://geoparquet-io.github.io/tylertoo/guides/remote-and-multi-file/)
  cover remote inputs, sharded builds, and memory limits.
- The [CLI reference](https://geoparquet-io.github.io/tylertoo/reference/cli/)
  lists every flag with its default.
