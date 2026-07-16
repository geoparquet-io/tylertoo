# Getting Started

## Installation

### CLI (Cargo)

```bash
cargo install gpq-tiles
```

**Prerequisites:** Rust 1.75+, protoc

```bash
# macOS
brew install protobuf

# Ubuntu/Debian
apt install protobuf-compiler
```

### Python

```bash
pip install gpq-tiles
```

### From Source

```bash
git clone https://github.com/geoparquet-io/gpq-tiles.git
cd gpq-tiles
cargo build --release
```

## Input Requirements

**gpq-tiles requires WGS84 (EPSG:4326) coordinates.** If your GeoParquet file
uses a projected CRS (e.g., UTM, British National Grid), reproject first with
[geoparquet-io](https://github.com/geoparquet-io/geoparquet-io):

```bash
# Reproject to WGS84 with geoparquet-io (recommended)
gpio convert reproject input.parquet output.parquet -d EPSG:4326

# Optimize for performance at the same time
gpio convert reproject input.parquet output.parquet \
  -d EPSG:4326 --hilbert --row-group-size 100000
```

If you forget, gpq-tiles errors with the detected CRS and the reprojection
command.

**Why gpio?** The converter assumes Hilbert-sorted, bbox-covered input with
sane row-group sizing — `gpio` produces exactly that, and it is what keeps
gpq-tiles fast on large files.

**Reserved column names are auto-renamed.** The overview format adds a `level`
column (and, with `--cluster` / line coalescing, `point_count` /
`coalesced_count`). If your data already has a property with one of those names
— Overture Maps buildings carry a `level` (floor number), admin data often has
`LEVEL` — gpq-tiles does **not** reject the file. It renames the colliding
property by appending `_` (e.g. `level` → `level_`), prints a warning, and keeps
its own column authoritative. The match is case-insensitive (so `LEVEL` is
handled too), and any `--sort-key` / `--class-rank` / `--accumulate-attribute`
that named the renamed column is rewritten to follow it. No preprocessing
needed.

## The Two-Step Workflow (Recommended)

The product is the **overview GeoParquet file**: one file that embeds
COG-style multi-resolution levels alongside your exact source data. PMTiles
is an *export* of it.

```bash
# 1. Build the overview file (levels for zooms 0..14)
gpq-tiles overview input.parquet overviews.parquet \
  --min-zoom 0 --max-zoom 14

# 2. Check it against the spec
gpq-tiles validate overviews.parquet

# 3. Export a PMTiles archive for map rendering
gpq-tiles export-pmtiles overviews.parquet output.pmtiles
```

The overview file remains valid GeoParquet 1.1: query it with DuckDB,
GeoPandas, or anything else (`WHERE level = k` selects one resolution;
the finest level is your data, verbatim).

Sensible defaults are chosen from rendered corpus sweeps (auto-detected
class ranking for Overture roads/places, line coalescing on, density
budget on). Every knob is documented in
[Overview Tuning](OVERVIEW_TUNING.md).

### Useful overview options

```bash
# Point data: clustering with counts for graduated-dot rendering
gpq-tiles overview places.parquet places_ov.parquet \
  --min-zoom 0 --max-zoom 14 \
  --cluster --accumulate-attribute population:sum

# Roads: explicit class ranking (auto-detected for Overture schemas)
gpq-tiles overview roads.parquet roads_ov.parquet \
  --min-zoom 0 --max-zoom 14 \
  --class-rank road_class:motorway=5,primary=4,residential=2

# Partitioning mode: each feature stored once (COGP-compatible prefix reads)
gpq-tiles overview input.parquet out.parquet --mode partitioning

# Write a JSON conversion report
gpq-tiles overview input.parquet out.parquet --report report.json
```

### Useful export options

```bash
gpq-tiles export-pmtiles overviews.parquet output.pmtiles \
  --layer-name roads \       # MVT layer name (default: "overview")
  --tile-size-limit 500000   # optional per-tile byte cap
```

## One-Shot Conversion

When you only want the PMTiles and don't care about the intermediate file:

```bash
gpq-tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 14
# equivalently: gpq-tiles tiles input.parquet output.pmtiles ...
```

This runs overview convert into a temporary file, then export. Beyond the
essentials (`--min-zoom`, `--max-zoom`, `--layer-name`, `--tile-buffer`,
`--max-tile-size`, `--verbose`), the one-shot command also accepts every
convert-tuning knob from `overview` — quality and memory alike:

```bash
# Country-scale dot fill for a dense building layer, memory-bounded,
# in one shot (see docs/OVERVIEW_TUNING.md, "Country-scale dot fill")
gpq-tiles input.parquet output.pmtiles --max-zoom 14 \
  --polygon-visibility 0 --collapse --max-tile-size 500K \
  --profile bounded
```

A `tiles` run is equivalent to the two-step `overview` + `export-pmtiles`
chain with the same flags. See `gpq-tiles tiles --help` (flags are grouped
by heading) and the [API reference](api-reference.md) for the full set.

## Python

```python
from gpq_tiles import overview, export_pmtiles, validate, convert

# Two-step (full knob surface; see the API reference)
report = overview(
    "places.parquet", "places_ov.parquet",
    min_zoom=0, max_zoom=14,
    cluster=True,
    accumulate_attributes={"population": "sum"},
)
result = validate("places_ov.parquet")
export_pmtiles("places_ov.parquet", "places.pmtiles", layer_name="places")

# One-shot facade (deprecated in favor of the two-step API)
convert("input.parquet", "output.pmtiles", min_zoom=0, max_zoom=14)
```

## Rust

```rust
use gpq_tiles_core::overview::convert::{convert_to_overviews, ConvertOptions};
use gpq_tiles_core::overview::export::{export_pmtiles, ExportOptions};
use std::path::Path;

let opts = ConvertOptions::default(); // duplicating mode, z0..6
let report = convert_to_overviews(
    Path::new("input.parquet"),
    Path::new("overviews.parquet"),
    &opts,
)?;

let export_opts = ExportOptions::default();
export_pmtiles(
    Path::new("overviews.parquet"),
    Path::new("output.pmtiles"),
    &export_opts,
)?;
```

## Next Steps

- [Overview Tuning](OVERVIEW_TUNING.md) — what every knob does
- [API Reference](api-reference.md) — full CLI/Python/Rust surface
- [Advanced Usage](advanced-usage.md) — input optimization, memory, debugging
