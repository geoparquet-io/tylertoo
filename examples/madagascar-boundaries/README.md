# Build vector tiles for Madagascar's boundaries

Use tylertoo to turn Madagascar's boundary polygons from
[Field Maps](https://fieldmaps.io/) into a PMTiles archive. You'll preview
one region, build an overview for the whole country, export the tiles, and
decode one zoom level back to GeoParquet.

An **overview** is a GeoParquet file that stores geometry prepared for
multiple zoom levels. You can query it directly or use it to export tiles.

The [example scripts](https://github.com/geoparquet-io/tylertoo/tree/main/examples/madagascar-boundaries)
run in CI on every pull request. CI checks the commands and output shown on
this page.

## Before you begin

Install `tylertoo` with `cargo install tylertoo` and `gpio` with
`pip install geoparquet-io`. Make sure both commands are on your `PATH`. Run
the scripts in order from the same empty working directory.

## 1. Check the input

Download the example file and check that it's ready for tiling.

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

To prepare your own input, run
`gpio convert geoparquet input.parquet output.parquet`. This sorts rows by
location, adds a bounding box column, and compresses the file. tylertoo
keeps the input row order. It reads data in any coordinate reference system
that has an EPSG code and reprojects it to WGS84.

## 2. Preview one region

Use `--bbox` to test your settings on a small region. Use the `--bbox=` form
if the first coordinate is negative, so the command reads it as part of the
box.

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

The preview keeps the features whose bounding boxes intersect the box. The
file has one row group, so tylertoo reads all of it and filters the features.

The `gsd(m)` column shows how much ground one pixel covers. At low zooms,
tylertoo draws polygons that are too small to see as small squares.

## 3. Build and validate the overview

tylertoo writes all ten levels to one GeoParquet file and uses the `level`
column to identify them. In this file, `level = 0` corresponds to zoom
level 1. The lower zooms use simplified geometry, and the highest zoom keeps
the original boundaries.

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

`tylertoo validate` checks the overview's metadata and structure, including
how levels map to zooms. Look for `valid overview file` before exporting.

## 4. Export PMTiles

Set `--min-zoom 1` to match the overview's first zoom. If you style the
tiles on a map, use `boundaries` as the source layer.

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

The export counts one tile as `oversized` because it exceeded the size limit
before tylertoo adjusted it. The stats show the sizes of the tiles in the
finished archive.

Open `madagascar.pmtiles` in the [PMTiles viewer](https://pmtiles.io/). To
publish the archive, host it on a server that supports HTTP range requests.

## 5. Decode tiles back to GeoParquet

Decode the tiles to inspect the geometry and attributes they store.

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

The decoded geometry keeps the simplification and clipping from the tiles. A
boundary that crosses a tile edge can appear in multiple rows, one for each
tile it touches. Use `zoom`, `layer`, and `mvt_id` to inspect where a row
came from.

## Run the whole workflow

Run all five scripts from a fresh directory.

```bash
git clone https://github.com/geoparquet-io/tylertoo.git
mkdir madagascar
cd madagascar
for step in ../tylertoo/examples/madagascar-boundaries/0*.sh; do
  bash "$step"
done
```
