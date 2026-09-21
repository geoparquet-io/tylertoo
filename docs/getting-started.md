# Getting started

## The 60-second version

Three commands, on a real file you can download right now — 17,465 Madagascar
admin-4 boundary polygons (28 MB) from this repo's fixture release.

```bash
cargo install tylertoo
curl -LO https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/fieldmaps-madagascar-adm4.parquet
tylertoo fieldmaps-madagascar-adm4.parquet madagascar.pmtiles --max-zoom 10
```

Real output, with log timestamps stripped and the routine per-level lines
elided:

```text
[convert] scan complete: 17465 feature(s) from 17465 row(s)
[convert] pass 2: building 11 overview level(s) from a single read (finest level streamed last)
[rss] convert peak: 231 MiB
  intermediate overview: /var/.../T/.tylertoo-overview-p7TdMr.parquet (25.87 MiB, removed after export; --keep-overview PATH retains it)
[export] scan complete: 10 levels, single read, 0.15s
[export] level 10/10 z10 done: 17465 feats, 524 tiles, 1 partitions, 0.3s (total 1s)
✓ Converted fieldmaps-madagascar-adm4.parquet → madagascar.pmtiles
  752 tiles across z0..z10 in 1.18s
  z0 declared but empty: every feature generalized away there (see --collapse / --collapse-square), or an entry-zoom ladder holds every feature out of them
```

You now have a 6.7 MB `madagascar.pmtiles`. Drop it onto
[pmtiles.io](https://pmtiles.io/) to see it; the MVT source-layer is
`fieldmaps-madagascar-adm4` (the one-shot form names the layer after the
input file stem — pass `--layer-name` to override it). The z0 note is real
and worth reading: z0 is a single tile at
39 km/pixel, where every one of these polygons simplifies to nothing.
`--collapse` keeps them as representative points instead.

The rest of this page slows that down: what the intermediate overview file is,
why you would keep it, and how the same commands behave when the input is far
larger than memory.

## Installing tylertoo

tylertoo ships as a CLI binary and a Python package. Both drive the same
engine, so the choice is about where your pipeline lives. Shell scripts and
Makefiles reach for the CLI. Notebook and Airflow-style workflows import the
Python package.

This tutorial uses the CLI throughout. Every command below has a Python
equivalent with the same options.

```bash
cargo install tylertoo    # CLI (used in this tutorial)
pip install tylertoo      # Python bindings (same engine, importable)
```

Prebuilt binaries for Linux, macOS and Windows are attached to every
[GitHub Release](https://github.com/geoparquet-io/tylertoo/releases) if you
would rather not compile.

```bash
tylertoo --version
```

## Preparing your GeoParquet input

tylertoo reads lon/lat WGS84 GeoParquet. It runs fastest when the file is also
gpio-optimized, with features ordered by spatial locality and packed into large
row groups. The Madagascar fixture above is already in good shape; your own
export from a database or a Spark job usually is not, and one preparation pass
pays for itself across every zoom level that follows.

Use [geoparquet-io](https://github.com/geoparquet-io/geoparquet-io) (`gpio`)
for this — not `ogr2ogr`. The commands below were checked against gpio 1.5.0.

### Inspecting the raw file

`gpio inspect` reports the input's CRS, row-group layout, and spatial order
before you commit to a run. A raw Spark or database export typically shows
hundreds of small row groups and a poor spatial overlap ratio. Those are what
the next step fixes.

```bash
gpio inspect input.parquet
```

### Sorting and resizing row groups

Hilbert sorting reorders features so that geographic neighbors sit near each
other on disk. Resizing row groups packs those neighbors into the units
tylertoo streams. Together they let each tile read a handful of row groups
instead of scanning the file, which is what keeps memory bounded and throughput
high later on.

```bash
# Already WGS84: sort and repack in one pass.
gpio sort hilbert input.parquet prepared.parquet --row-group-size-mb 128
```

A file in another projection needs reprojecting first — `gpio sort hilbert`
carries a non-default CRS through unchanged rather than converting it.

```bash
gpio convert reproject input.parquet wgs84.parquet -d EPSG:4326
gpio sort hilbert wgs84.parquet prepared.parquet --row-group-size-mb 128
```

## Building an overview file

`tylertoo overview` builds a multi-resolution pyramid and writes it inside a
single GeoParquet file. Each zoom level holds a thinned, simplified copy of
your features sized for that scale, so the overview grows to several times the
input's feature count. The output is not an opaque tile blob. It stays a
GeoParquet file you can open, query, and re-export, which is why the two-step
workflow keeps it as a first-class artifact.

The `--max-zoom` flag defaults to 6, enough for a continental overview but too
coarse for street-level detail. A web map that zooms to individual features
needs it raised.

```bash
tylertoo overview \
  fieldmaps-madagascar-adm4.parquet \
  madagascar-ov.parquet \
  --min-zoom 1 --max-zoom 10
```

```text
✓ Overview fieldmaps-madagascar-adm4.parquet → madagascar-ov.parquet  (Duplicating mode)
  17,465 input features → 43,848 rows across 10 levels in 0.40s
  lvl        gsd(m)    features    vertices         bytes
    0      19567.88          40         173  10.65 KiB
    1       9783.94         291       1,315  43.53 KiB
    2       4891.97         517       2,859  78.52 KiB
    3       2445.98         863       6,618  146.92 KiB
    4       1222.99       1,428      14,872  281.43 KiB
    5        611.50       2,356      32,462  536.65 KiB
    6        305.75       3,888      68,155  1018.00 KiB
    7        152.87       6,415     133,597  1.79 MiB
    8         76.44      10,585     249,642  3.07 MiB
    9         38.22      17,465   1,749,880  18.86 MiB
```

Read that table top to bottom and you can see the whole idea: 40 features
survive at 19.5 km/pixel, and the finest level is the source data verbatim.

### Previewing one region

On a larger input, carve out one region with `--bbox` before committing minutes
to the whole thing. tylertoo reads only the row groups whose bounds intersect
the box, so a city-sized window finishes in seconds and shows you the
representation early. The bounds are lon/lat, ordered `xmin,ymin,xmax,ymax`.

```bash
tylertoo overview \
  prepared.parquet \
  preview-ov.parquet \
  --min-zoom 0 --max-zoom 12 \
  --bbox=-48,-24,-46,-22
```

Peak memory tracks the largest row group, not the size of the file, so a file
that dwarfs RAM still converts — see
[Keeping memory bounded](diving-deeper/bounded-memory.md).

## Validating the overview against the spec

`tylertoo validate` checks the overview against the `geo:overviews`
specification. It confirms that the level metadata, the zoom-to-resolution
mapping, and the per-level structure agree with what a spec-aware reader
expects. A file that passes can move downstream without further inspection.

```bash
tylertoo validate madagascar-ov.parquet
```

```text
Validating madagascar-ov.parquet
  [PASS] geoparquet_geo_metadata: 'geo' metadata present with geometry column(s)
  [PASS] geoparquet_covering_declared: bbox covering declared
  [PASS] overviews_key_present: 'geo:overviews' present and parses
  [PASS] overviews_version: version 0.2.0 (MAJOR 0 supported)
  [PASS] overviews_structure: levels/gsd/zoom/canonical invariants satisfied
  [PASS] mode_canonical: duplicating: canonical_level == L-1
  [PASS] level_column: 'level' is INT32 NOT NULL
  [PASS] level_footer_consistency: every row group's level stats match the footer
  [PASS] coalesce_mode: coalescing metadata on a duplicating-mode file
  [PASS] coalesce_count_column: "coalesced_count" is INT32 NOT NULL
  [PASS] coalesce_count_values: coalesced_count >= 1 everywhere; canonical level all 1
  [PASS] covering_stats: all row groups carry covering min/max statistics

✓ valid overview file (12 checks passed)
```

### Querying it as plain GeoParquet

Validation aside, the overview is still a plain GeoParquet file. Any Arrow- or
Parquet-aware tool reads it, DuckDB included.

```bash
duckdb -c "SELECT level, count(*) AS features FROM 'madagascar-ov.parquet' GROUP BY level ORDER BY level;"
```

```text
┌───────┬──────────┐
│ level │ features │
│ int32 │  int64   │
├───────┼──────────┤
│     0 │       40 │
│     1 │      291 │
│     2 │      517 │
│     3 │      863 │
│     4 │     1428 │
│     5 │     2356 │
│     6 │     3888 │
│     7 │     6415 │
│     8 │    10585 │
│     9 │    17465 │
└───────┴──────────┘
```

## Exporting a PMTiles archive

`tylertoo export-pmtiles` reads the overview and writes a PMTiles archive, one
clipped vector tile per tile coordinate. No geometry is recomputed here. The
levels built during overview become the tiles served to the map.

Each tile carries one MVT layer, named `overview` by default. Set
`--layer-name` to whatever your map style's `source-layer` expects.

```bash
tylertoo export-pmtiles \
  madagascar-ov.parquet \
  madagascar.pmtiles \
  --layer-name boundaries \
  --min-zoom 1
```

```text
  mode=duplicating zooms z1..z10
  z1  (level 0):       1 tiles,        40 features
  z2  (level 1):       1 tiles,       291 features
  z3  (level 2):       2 tiles,       858 features
  z4  (level 3):       4 tiles,      1346 features
  z5  (level 4):       4 tiles,      1818 features
  z6  (level 5):       7 tiles,      2789 features
  z7  (level 6):      16 tiles,      4826 features
  z8  (level 7):      44 tiles,      8269 features
  z9  (level 8):     149 tiles,     14834 features
  z10 (level 9):     524 tiles,     26839 features

✓ 752 tiles, 61910 features, 0 oversized tiles in 0.72s
```

Pass `export-pmtiles --min-zoom <the overview's requested minimum>` to match
what `tiles` writes — otherwise the archive header starts at the coarsest level
that actually holds features.

Per-zoom feature counts exceed the level's feature count because features that
straddle a tile boundary are clipped into each tile they touch.

## Viewing the tiles

A PMTiles archive is a single file served over HTTP range requests, so any
PMTiles-aware viewer renders it without a running tile server. Drop the file
onto [pmtiles.io](https://pmtiles.io/), or serve it locally with a server
that honors Range requests (Python's `http.server` does not):

```bash
npx serve .            # or: caddy file-server, or: pmtiles serve .
```

## Converting in one step

The bare form runs overview and export back to back, from prepared input
straight to tiles. It is the fast path when you want the archive and nothing
else.

```bash
tylertoo prepared.parquet output.pmtiles
```

The two-step form earns its extra command when you want the overview file
itself, whether to validate it, query it, or export it more than once with
different layer names or tile-size limits. `tylertoo tiles --keep-overview
overviews.parquet` gives you both from a single run.

## Bigger data: Brazil's 2025 field predictions

The Madagascar file fits in memory and finishes in a second. The workflow does
not change when the data does not.

The [Brazil 2025 demo](demo.md) tiles 43.9 million field polygons out of the
[Fields of The World](https://fieldsofthe.world/) predictions collection on
[Source Cooperative](https://source.coop/ftw/global-data) — 629.6 GiB of
GeoParquet, 8.2 billion rows in 1,000 Spark part files. There is no curated
4.5 GB Brazil extract to download: the slice is carved *during the tiling
read*, from the 52 part files whose footers intersect the Brazil bbox.

Those 52 URLs are committed to this repo as
[`demo/brazil-2025-manifest.txt`](https://github.com/geoparquet-io/tylertoo/blob/main/demo/brazil-2025-manifest.txt),
so the run is reproducible without downloading anything first:

```bash
# 1. Overview straight from the remote collection slice:
#    Brazil bbox, 2025 vintage, fields only, centroids at z0-7.
tylertoo overview \
  --files-from demo/brazil-2025-manifest.txt \
  --bbox="-74.1,-34.0,-34.7,5.4" \
  --filter "label = 'field' AND time >= '2025-01-01'" \
  --min-zoom 0 --max-zoom 14 \
  --representation "0-7:point" \
  brazil-2025-fields-ov-z14.parquet

# 2. Export the PMTiles archive.
tylertoo export-pmtiles \
  brazil-2025-fields-ov-z14.parquet \
  brazil-2025-fields.pmtiles
```

The measured run: **1 h 12 m** to convert at a 9.6 GiB peak RSS while reading
40.7 GiB over the network, then **11 m 44 s** to export 1.6 million tiles at a
1.54 GiB peak, on 16 cores. Peak memory tracks the largest row group, not the
size of the input — which is why 40 GiB of remote data converts inside 10 GiB
of RAM. [RESULTS.md](https://github.com/geoparquet-io/tylertoo/blob/main/demo/RESULTS.md)
has the full breakdown, and [the demo page](demo.md) renders the finished
archive live.

This is real money in network time, so start with `--bbox` on a small window.
The same commands with a tighter box finish in minutes.
