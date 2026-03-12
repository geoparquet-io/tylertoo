# gpq-tiles

[![CI](https://github.com/geoparquet-io/gpq-tiles/actions/workflows/ci.yml/badge.svg)](https://github.com/geoparquet-io/gpq-tiles/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/geoparquet-io/gpq-tiles/branch/main/graph/badge.svg)](https://codecov.io/gh/geoparquet-io/gpq-tiles)
[![Crates.io](https://img.shields.io/crates/v/gpq-tiles?color=blue)](https://crates.io/crates/gpq-tiles)
[![PyPI](https://img.shields.io/pypi/v/gpq-tiles?color=blue)](https://pypi.org/project/gpq-tiles/)

Fast GeoParquet → PMTiles converter in Rust.

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
gpq-tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 14
```

```python
from gpq_tiles import convert

# Basic
convert("input.parquet", "output.pmtiles", min_zoom=0, max_zoom=14)

# With property filtering and progress
convert(
    "buildings.parquet", "buildings.pmtiles",
    include=["name", "height"],
    progress_callback=lambda e: print(f"{e['phase']}: {e.get('total_tiles', '...')}")
)
```

## Streaming Pipeline

For Hilbert-sorted GeoParquet files (via [`gpio optimize`](https://github.com/geoparquet-io/gpio)),
use the streaming pipeline for minimal resource usage:

```bash
# Optimal for sorted input: ~1x output size temp disk, ~100-500MB memory
gpq-tiles input.parquet output.pmtiles --sorting-strategy streaming

# Works with any input: ~2x input size temp disk, ~2GB memory (default)
gpq-tiles input.parquet output.pmtiles --sorting-strategy external
```

The streaming pipeline uses a spool-based approach:
- Features accumulate in memory per-tile
- Completed tiles flush to a temp spool
- Final PMTiles written from sorted spool

### Partitioned Directories

Supports partitioned GeoParquet directories:

```bash
# Process all .parquet files in a Hive-partitioned directory
gpq-tiles data/year=2024/ output.pmtiles
```

## Documentation

- **[Getting Started](docs/getting-started.md)** — Installation, basic usage, property filtering
- **[Advanced Usage](docs/advanced-usage.md)** — Performance tuning, streaming, CI/CD
- **[API Reference](docs/api-reference.md)** — CLI flags, Rust API, Python API

## Development

```bash
git clone https://github.com/geoparquet-io/gpq-tiles.git && cd gpq-tiles
cargo build && cargo test
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for details.

## License

Apache-2.0
