# Madagascar boundaries: from GeoParquet to PMTiles

Convert 17,465 admin-4 boundary polygons for Madagascar (17 MB), from the
[fieldmaps.io](https://fieldmaps.io/) global boundaries, into a PMTiles archive
using the command line. First, build an **overview file**: queryable
GeoParquet containing your data at every zoom level.

Each step is a script in
[`examples/madagascar-boundaries/`](https://github.com/geoparquet-io/tylertoo/tree/main/examples/madagascar-boundaries).
CI runs the scripts on every pull request and checks this page's code and output.

## Before you begin

You need two tools on your `PATH`:

| Tool | Install | Used for |
| --- | --- | --- |
| `tylertoo` | `cargo install tylertoo` | Overviews, tiles, validation, decoding |
| `gpio` | `pip install geoparquet-io` | Checking the input |

Run the scripts in order from the same empty working directory. Each step
reads the previous step's files.

## 1. Check the input

tylertoo reads GeoParquet in any coordinate reference system (CRS) that
has an EPSG code, and reprojects it to WGS84 as it reads. It keeps the
input's row order, so the rows must be sorted by location; tylertoo warns
when they are not. This tutorial's input is already prepared. To prepare
your own file, run `gpio convert geoparquet input.parquet output.parquet`,
which sorts rows in Hilbert order, adds a bbox column, and compresses with
ZSTD in one pass.

File: `01-check.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

# Download the input once: 17,465 admin-4 boundary polygons (17 MB),
# already GeoParquet, Hilbert-sorted, and ZSTD-compressed.
SRC=fieldmaps-madagascar-adm4-hilbert.parquet
if [ ! -f "$SRC" ]; then
  curl -fsSLO "https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/$SRC"
fi

# Check compression, bbox covering, row groups, and spatial order.
# --strict fails on warnings too.
gpio check all "$SRC" --strict
```

```text
Number of row groups: 1
...
✓ Found bbox column 'bbox' with proper metadata covering
...
✓ ZSTD compression on geometry column 'geometry'
...
✓ Data appears to be spatially ordered
...
  ✓ 30 checks passed
```

## 2. Preview one region

Test settings on a small region with `--bbox`, which takes lon/lat bounds as
`xmin,ymin,xmax,ymax`. `--bbox` keeps features whose bounding boxes intersect
the box, and skips row groups outside it. This file has one row group, so
the preview filters features without skipping any row groups.
Write `--bbox=` with an equals sign, so a negative first coordinate is not
read as a flag.

File: `02-preview.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

# Build a quick overview of Antananarivo only. --bbox reads just the row
# groups whose bounds touch the box (xmin,ymin,xmax,ymax in lon/lat).
tylertoo overview fieldmaps-madagascar-adm4-hilbert.parquet preview-ov.parquet \
  --max-zoom 12 \
  --bbox=47.3,-19.1,47.7,-18.7
```

```text
✓ Overview fieldmaps-madagascar-adm4-hilbert.parquet → preview-ov.parquet  (Duplicating mode)
  943 input features → 7,642 rows across 13 levels in 0.18s
  lvl        gsd(m)    features    vertices         bytes
    0      39135.76           1           5  3.99 KiB
    1      19567.88           6          30  4.84 KiB
    2       9783.94          24         120  6.76 KiB
    3       4891.97          99         494  13.85 KiB
    4       2445.98         345       1,699  35.06 KiB
    5       1222.99         668       3,120  63.40 KiB
    6        611.50         856       4,219  81.99 KiB
    7        305.75         929       5,602  97.16 KiB
    8        152.87         942       7,533  112.94 KiB
    9         76.44         943      10,088  132.27 KiB
   10         38.22         943      13,262  156.60 KiB
   11         19.11         943      17,266  187.85 KiB
   12          9.55         943      58,321  517.75 KiB
```

The box contains 943 polygons. `gsd` (ground sample distance) is the size of
one pixel in meters. At z0–z2, pixels span 10–40 km, wider than any of these
polygons. tylertoo draws a polygon below the visibility gate as a small
placeholder square, so even the coarsest levels show where the polygons
are. Use `--no-collapse` to drop them or `--collapse` to draw dots.
Run `tylertoo overview --help` to see the available settings.

## 3. Build and validate the overview

`tylertoo overview` writes all levels to one GeoParquet file with a `level`
column. Each level thins and simplifies features for its scale. The finest
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
tylertoo overview fieldmaps-madagascar-adm4-hilbert.parquet madagascar-ov.parquet \
  --min-zoom 1 \
  --max-zoom 10
tylertoo validate madagascar-ov.parquet
```

```text
✓ Overview fieldmaps-madagascar-adm4-hilbert.parquet → madagascar-ov.parquet  (Duplicating mode)
  17,465 input features → 134,717 rows across 10 levels in 0.42s
  lvl        gsd(m)    features    vertices         bytes
    0      19567.88       1,558       7,763  157.80 KiB
    1       9783.94       4,691      22,924  450.76 KiB
    2       4891.97      10,102      48,691  957.85 KiB
    3       2445.98      14,836      73,412  1.39 MiB
    4       1222.99      16,572      96,022  1.65 MiB
    5        611.50      17,174     130,279  1.95 MiB
    6        305.75      17,393     177,644  2.34 MiB
    7        152.87      17,461     239,621  2.88 MiB
    8         76.44      17,465     322,984  3.59 MiB
    9         38.22      17,465   1,749,880  17.18 MiB
Validating madagascar-ov.parquet
...
✓ valid overview file (12 checks passed)
```

At 19.6 km per pixel, 1,558 features remain: the polygons large enough to
see, plus placeholder squares for the smaller ones. The count grows at each
level until level 8 holds every polygon. The density budget (`--drop-rate`)
thins only points by default, so boundaries that tile the island keep
their coverage at every zoom. Level 9 holds all 17,465 polygons at full
detail. Any Parquet reader, such as DuckDB, can open the file.

## 4. Export PMTiles

`tylertoo export-pmtiles` cuts the overview levels into vector tiles without
recomputing geometry. `--layer-name` sets the MVT source layer used by your
map style. `--min-zoom 1` starts at the overview's first zoom.

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
  z1  (level 0):       1 tiles,      1558 features
...
  z5  (level 4):       4 tiles,     14958 features, 1 oversized, 12 collapsed at extent
...
  z10 (level 9):     524 tiles,     25838 features, 12 collapsed at extent

✓ 751 tiles, 157767 features, 1 oversized tiles in 0.29s
...
 z  tiles      total     mean      p50      p99      max
 1      1     69,972   69,972   69,972   69,972   69,972
...
10    524  3,799,853    7,251    5,819   26,534   84,168

Largest 10 tile(s):
z   x   y    bytes
4  10   8  402,253
...
```

Polygons crossing tile edges appear in every tile they touch, so tile
features outnumber overview features. tylertoo omits clipped slivers that
vanish on the tile's 4096-unit grid and counts them as "collapsed at extent".
The largest tile is 402,253 bytes (about 402 KB) compressed, at z4 (4/10/8).
One tile, at z5, went over the default limit of 500 KB compressed (and 2 MB
before compression). The size valve merged its densest polygons into
neighbors until it fit. The export counts this tile as "oversized".

View `madagascar.pmtiles` at [pmtiles.io](https://pmtiles.io/), or host it on
any server that supports HTTP range requests. You don't need a tile server.

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

✓ 20,350 features from 44 tiles (0 skipped as degenerate) in 0.10s
```

Decoded geometry retains tile simplification and clipping. A feature spanning
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
  `tylertoo fieldmaps-madagascar-adm4-hilbert.parquet madagascar.pmtiles --max-zoom 10`. Add
  `--keep-overview madagascar-ov.parquet` to save the overview too.
- The [Brazil tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/brazil/)
  tiles a whole country from a cloud bucket with `--filter`, draws density
  cells with `--band`, and splits the build into shard jobs.
- The [CLI reference](https://geoparquet-io.github.io/tylertoo/reference/cli/)
  lists every flag with its default.
