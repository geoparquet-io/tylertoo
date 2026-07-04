# gpq-tiles

[![CI](https://github.com/geoparquet-io/gpq-tiles/actions/workflows/ci.yml/badge.svg)](https://github.com/geoparquet-io/gpq-tiles/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/geoparquet-io/gpq-tiles/branch/main/graph/badge.svg)](https://codecov.io/gh/geoparquet-io/gpq-tiles)
[![Crates.io](https://img.shields.io/crates/v/gpq-tiles?color=blue)](https://crates.io/crates/gpq-tiles)
[![PyPI](https://img.shields.io/pypi/v/gpq-tiles?color=blue)](https://pypi.org/project/gpq-tiles/)

Fast GeoParquet → PMTiles converter in Rust.

**Features:**
- COG-style multi-resolution **overviews embedded in GeoParquet** (`gpq-tiles overview`) — the file stays valid, exact, SQL-queryable GeoParquet
- PMTiles export from an overview file (`gpq-tiles export-pmtiles`)
- One-shot GeoParquet → PMTiles (`gpq-tiles tiles`, or the bare form)
- Quality ladder tuned against tippecanoe: class ranking (Overture auto-detect), visibility gates, density budget, point clustering, line coalescing
- Memory-bounded streaming conversion (632k-polygon / 38M-vertex file: ~55 s, ~320 MB peak RSS)
- Spec validation (`gpq-tiles validate`)

> **⚠️ Work in Progress**:
> Code is generated with Claude; take it with a grain of salt.
> --Nissim

## Install

```bash
cargo install gpq-tiles    # CLI
pip install gpq-tiles      # Python
```

## Usage

```bash
# One-shot: GeoParquet in, PMTiles out
gpq-tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 14
```

### The Two-Step Workflow

The overview GeoParquet file is the interesting artifact — keep it:

```bash
# 1. Embed multi-resolution levels in a GeoParquet file
gpq-tiles overview input.parquet overviews.parquet \
  --min-zoom 0 --max-zoom 14

# 2. Validate against the spec
gpq-tiles validate overviews.parquet

# 3. Export a PMTiles archive for map rendering
gpq-tiles export-pmtiles overviews.parquet output.pmtiles
```

All tuning knobs live on `overview` / `export-pmtiles` — see
[Overview Tuning](docs/OVERVIEW_TUNING.md). Defaults are calibrated on
rendered corpus sweeps and are meant to look right out of the box.

### Input Preparation

Inputs must be WGS84 (EPSG:4326), and should be Hilbert-sorted with sane
row groups. Use [geoparquet-io](https://github.com/geoparquet-io/geoparquet-io):

```bash
gpio convert reproject input.parquet prepared.parquet \
  -d EPSG:4326 --hilbert --row-group-size 100000
```

### Python

```python
from gpq_tiles import overview, export_pmtiles, validate

overview("input.parquet", "overviews.parquet", min_zoom=0, max_zoom=14)
validate("overviews.parquet")
export_pmtiles("overviews.parquet", "output.pmtiles")

# One-shot facade (deprecated in favor of the two-step API)
from gpq_tiles import convert
convert("input.parquet", "output.pmtiles", min_zoom=0, max_zoom=14)
```

## Documentation

- **[Getting Started](docs/getting-started.md)** — Installation, basic usage, the two-step workflow
- **[Overview Tuning](docs/OVERVIEW_TUNING.md)** — Every generalization knob explained
- **[API Reference](docs/api-reference.md)** — CLI flags, Python API, Rust API
- **[Advanced Usage](docs/advanced-usage.md)** — Input optimization, memory, remote reads, CI/CD
- **[Format Spec (draft)](context/OVERVIEWS_SPEC.md)** — The `geo:overviews` format contract

## Development

```bash
git clone https://github.com/geoparquet-io/gpq-tiles.git && cd gpq-tiles
git config core.hooksPath .githooks
cargo build && cargo check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [DEVELOPMENT.md](DEVELOPMENT.md) for details.

## License

Apache-2.0
