# Getting started

By the end of this tutorial you will have:

- A tuned PMTiles web map built from one country's field polygons.
- A `geo:overviews` GeoParquet file you can inspect and re-export.
- A working mental model of the two-step overview → export workflow.

This tutorial tiles Brazil's 2025 field boundaries, the 43.9-million-polygon
Brazil slice of the Fields of The World predictions collection on Source
Cooperative. That full collection holds 8.2 billion rows across 629.6 GiB. The
input here is a single 4.5 GB GeoParquet file of those 43.9 million features, so
the timing and memory numbers below sit at the scale your own country-sized data
will hit.

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

A version check confirms the install before you feed it a 4.5 GB file.

```bash
tylertoo --version
```

## Preparing your GeoParquet input

tylertoo reads lon/lat WGS84 GeoParquet. It runs fastest when the file is also
gpio-optimized, with features ordered by spatial locality and packed into large
row groups. The raw Brazil file is valid GeoParquet but unsorted, so one
preparation pass pays for itself across every zoom level that follows.

### Inspecting the raw file

`gpio inspect` reports the input's CRS, row-group layout, and spatial order
before you commit to a run. On the raw Brazil file it shows OGC:CRS84, 355 row
groups averaging 13 MB, and a poor spatial overlap ratio. Those last two are
what the next step fixes.

```bash
# Source file: Fields of The World results/ collection on Source Cooperative.
gpio inspect brazil-2025-fields.parquet
```

### Sorting and resizing row groups

Hilbert sorting reorders features so that geographic neighbors sit near each
other on disk. Resizing row groups to 128 MB packs those neighbors into the
units tylertoo streams. Together they let each tile read a handful of row groups
instead of scanning the file, which is what keeps memory bounded and throughput
high later on.

The Brazil file arrives already in WGS84, so this pass sorts and repacks without
touching coordinates. A file in another projection would need `gpio` to
reproject it first.

```bash
# Hilbert-sort and resize row groups in one pass.
gpio sort hilbert \
  brazil-2025-fields.parquet \
  brazil-sorted.parquet \
  --row-group-size-mb 128 \
  --overwrite
```

## Building an overview file

`tylertoo overview` builds a multi-resolution pyramid and writes it inside a
single GeoParquet file. Each zoom level holds a thinned, simplified copy of your
features sized for that scale, so the overview grows to several times the input
size. The Brazil demo's is 9.9 GB across fifteen levels. The output is not an
opaque tile blob. It stays a GeoParquet file you can open, query, and re-export,
which is why the two-step workflow keeps it as a first-class artifact.

The `--max-zoom` flag defaults to 6, enough for a continental overview but too
coarse for street-level detail. A web map that zooms to individual fields needs
it raised. This tutorial uses 14, the finest level the Brazil demo ships.

### Previewing one region

Before committing minutes to the whole country, carve out one region with
`--bbox`. tylertoo reads only the row groups whose bounds intersect the box, so
a São Paulo-sized window finishes in seconds and shows you the representation
early. The bounds are lon/lat, ordered `xmin,ymin,xmax,ymax`.

```bash
# Optional first look: extract the São Paulo area to finish in seconds.
tylertoo overview \
  brazil-sorted.parquet \
  sp-preview-ov.parquet \
  --min-zoom 0 --max-zoom 12 \
  --bbox -48,-24,-46,-22
```

### Building the full pyramid

The full run covers all 43.9 million features across the fifteen levels from z0
to z14, so expect minutes, not the seconds a `--bbox` preview takes. For a
measured run at this scale, the [Brazil 2025 demo](demo.md) reports about 1 h
12 m to convert and 11 m to export on 16 cores. That run also reads its 40.7 GiB
of input remotely and filters it while tiling, work a prepared local file skips.

Peak memory tracks the largest row group, not the size of the file. In the demo,
export streamed all fifteen levels at a 1.56 GiB peak, and convert held to
9.6 GiB while reading 40.7 GiB over the network. A 4.5 GB input tiles without a
4.5 GB footprint, and the bound holds when the input dwarfs RAM.

```bash
tylertoo overview \
  brazil-sorted.parquet \
  brazil-ov.parquet \
  --min-zoom 0 --max-zoom 14
```

## Validating the overview against the spec

`tylertoo validate` checks the overview against the `geo:overviews`
specification, section 6.2. It confirms that the level metadata, the
zoom-to-resolution mapping, and the per-level structure agree with what a
spec-aware reader expects. A file that passes can move downstream without
further inspection.

```bash
tylertoo validate brazil-ov.parquet
```

### Querying it as plain GeoParquet

Validation aside, the overview is still a plain GeoParquet file. Any Arrow- or
Parquet-aware tool reads it, DuckDB included. The count below returns every
feature across every level, a quick check that the file materialized.

```bash
duckdb -c "SELECT COUNT(*) FROM 'brazil-ov.parquet';"
```

## Exporting a PMTiles archive

`tylertoo export-pmtiles` reads the overview and writes a PMTiles archive, one
clipped vector tile per tile coordinate. No geometry is recomputed here. The
levels built during overview become the tiles served to the map.

Each tile carries one MVT layer, named `overview` by default. Set `--layer-name`
to whatever your map style's `source-layer` expects. This tutorial names it
`fields` so the style can target the data by name.

```bash
tylertoo export-pmtiles \
  brazil-ov.parquet \
  brazil.pmtiles \
  --layer-name fields
```

## Viewing the tiles

A PMTiles archive is a single file served over HTTP range requests, so any
PMTiles-aware viewer renders it without a running tile server.

The [Brazil 2025 demo](demo.md) renders a finished version live from Source
Cooperative, a 3.4 GiB archive of 1,649,201 tiles holding all 43.9 million
predictions. It shows what tuned output looks like across zoom. Dots render from
z0 to z5, and from z6 the field polygons take over, each field staying a dot
until it is large enough to draw. That dot-to-polygon handoff comes from tuning
flags this tutorial leaves at their defaults.

Your own `brazil.pmtiles` drops into the same MapLibre and PMTiles setup, or any
other PMTiles viewer.

## Converting in one step

The bare form runs overview and export back to back, from prepared input
straight to tiles. It is the fast path when you want the archive and nothing
else.

```bash
tylertoo brazil-sorted.parquet brazil.pmtiles
```

The two-step form earns its extra command when you want the overview file
itself, whether to validate it, query it, or export it more than once with
different layer names or tile-size limits.
