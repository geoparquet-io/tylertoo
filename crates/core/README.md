# tylertoo

[![CI](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml/badge.svg)](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/geoparquet-io/tylertoo/branch/main/graph/badge.svg)](https://codecov.io/gh/geoparquet-io/tylertoo)
[![Crates.io](https://img.shields.io/crates/v/tylertoo?color=blue)](https://crates.io/crates/tylertoo)
[![PyPI](https://img.shields.io/pypi/v/tylertoo?color=blue)](https://pypi.org/project/tylertoo/)

Fast GeoParquet → PMTiles converter in Rust.

**tylertoo** takes its name from ["Tippecanoe and Tyler Too"](https://en.wikipedia.org/wiki/Tippecanoe_and_Tyler_Too),
the 1840 U.S. campaign slogan. It's a nod to [tippecanoe](https://github.com/felt/tippecanoe),
the vector-tile tool this project measures itself against — tylertoo runs alongside it.

**Features:**
- COG-style multi-resolution **overviews embedded in GeoParquet** (`tylertoo overview`) — the file stays valid, exact, SQL-queryable GeoParquet
- PMTiles export from an overview file (`tylertoo export-pmtiles`)
- One-shot GeoParquet → PMTiles (`tylertoo tiles`, or the bare form)
- Quality ladder tuned against tippecanoe: class ranking (Overture auto-detect), visibility gates, density budget, point clustering, line coalescing
- Memory-bounded streaming conversion — a 632k-polygon / 38M-vertex file converts to a full z0–14 overview pyramid in ~45 s at ~1.4 GB peak RSS, or a default z0–6 pyramid in ~7 s at ~0.4 GB (16-core machine)
- Remote inputs (`s3://`, `https://`, `gs://`) read via byte-range requests — with `--bbox`, extract a city from a remote country-scale file while downloading only the matching row groups ([Remote Reads](docs/remote-reads.md))
- Spec validation (`tylertoo validate`)
- PMTiles → GeoParquet decoding (`tylertoo decode`) — tippecanoe-decode
  semantics, any PMTiles v3 MVT archive

> **⚠️ Work in Progress**:
> Code is generated with Claude; take it with a grain of salt.
> --Nissim

## Install

```bash
cargo install tylertoo    # CLI
pip install tylertoo      # Python
```

## Usage

```bash
# One-shot: GeoParquet in, PMTiles out
tylertoo input.parquet output.pmtiles --min-zoom 0 --max-zoom 14
```

### The Two-Step Workflow

The overview GeoParquet file is the interesting artifact — keep it:

```bash
# 1. Embed multi-resolution levels in a GeoParquet file
tylertoo overview input.parquet overviews.parquet \
  --min-zoom 0 --max-zoom 14

# 2. Validate against the spec
tylertoo validate overviews.parquet

# 3. Export a PMTiles archive for map rendering
tylertoo export-pmtiles overviews.parquet output.pmtiles
```

All tuning knobs live on `overview` / `export-pmtiles` — see
[Overview Tuning](docs/OVERVIEW_TUNING.md). Defaults are calibrated on
rendered corpus sweeps and are meant to look right out of the box.

### Decoding PMTiles back to GeoParquet

```bash
# Extract one zoom of any PMTiles v3 vector archive as GeoParquet
tylertoo decode input.pmtiles output.parquet --zoom 14
```

The output is the **tiled representation** (simplified, clipped, duplicated
across tiles and zooms — no round-trip guarantee), with `zoom`/`layer`/
`mvt_id` provenance columns for filtering. See
[Decoding PMTiles](docs/decode.md).

### Input Preparation

Inputs must be WGS84 (EPSG:4326), and should be Hilbert-sorted with sane
row groups. Use [geoparquet-io](https://github.com/geoparquet-io/geoparquet-io):

```bash
gpio convert reproject input.parquet prepared.parquet \
  -d EPSG:4326 --hilbert --row-group-size 100000
```

### Python

```python
from tylertoo import overview, export_pmtiles, validate

overview("input.parquet", "overviews.parquet", min_zoom=0, max_zoom=14)
validate("overviews.parquet")
export_pmtiles("overviews.parquet", "output.pmtiles")

# One-shot facade (deprecated in favor of the two-step API)
from tylertoo import convert
convert("input.parquet", "output.pmtiles", min_zoom=0, max_zoom=14)
```

## Documentation

- **[Getting Started](docs/getting-started.md)** — Installation, basic usage, the two-step workflow
- **[Overview Tuning](docs/OVERVIEW_TUNING.md)** — Every generalization knob explained
- **[Decoding PMTiles](docs/decode.md)** — PMTiles → GeoParquet, limitations included
- **[API Reference](docs/api-reference.md)** — CLI flags, Python API, Rust API
- **[Advanced Usage](docs/advanced-usage.md)** — Input optimization, memory, remote reads, CI/CD
- **[Remote Reads](docs/remote-reads.md)** — Converting directly from s3://, https://, gs:// inputs, and querying overview files in place with DuckDB
- **[Format Spec (draft)](context/OVERVIEWS_SPEC.md)** — The `geo:overviews` format contract

## Development

```bash
git clone https://github.com/geoparquet-io/tylertoo.git && cd tylertoo
git config core.hooksPath .githooks
cargo build && cargo check
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [DEVELOPMENT.md](DEVELOPMENT.md) for details.

## License

Apache-2.0
