# gpq-tiles

[![CI](https://github.com/geoparquet-io/gpq-tiles/actions/workflows/ci.yml/badge.svg)](https://github.com/geoparquet-io/gpq-tiles/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/geoparquet-io/gpq-tiles/branch/main/graph/badge.svg)](https://codecov.io/gh/geoparquet-io/gpq-tiles)
[![Crates.io](https://img.shields.io/crates/v/gpq-tiles?color=blue)](https://crates.io/crates/gpq-tiles)
[![PyPI](https://img.shields.io/pypi/v/gpq-tiles?color=blue)](https://pypi.org/project/gpq-tiles/)

Fast GeoParquet → PMTiles converter in Rust.

**Features:**
- Memory-bounded processing for 100GB+ files
- Spatial filtering with GeoParquet row group skip
- Tippecanoe-compatible feature dropping (`--drop-densest-as-needed`, `--drop-smallest-as-needed`)
- Point clustering (`--cluster-distance`)
- Automatic zoom levels per feature (`--zoom-by-area`)
- Property filtering (`-y`, `-x`, `-X`)
- Parallel row group reading
- gzip, zstd, and brotli compression

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

### Large File Processing (100GB+)

gpq-tiles handles datasets larger than available RAM using disk-backed external sorting and spatial bucketing. For files over 10GB, this kicks in automatically:

```bash
# Process a directory of partitioned parquet files (auto-tuned)
gpq-tiles /data/roads/ output.pmtiles --min-zoom 0 --max-zoom 14

# Force bucketed mode with explicit bucket count
gpq-tiles huge.parquet output.pmtiles --buckets 256
```

Memory usage is bounded to ~10-15GB regardless of input size.

### Spatial Filtering

Skip row groups outside your area of interest using GeoParquet covering metadata:

```bash
# Filter by tile coordinates (z/x/y)
gpq-tiles world.parquet sf-bay.pmtiles --bounds 10/163/395

# Filter by WGS84 bounding box
gpq-tiles world.parquet sf-bay.pmtiles --bounds -122.5,37.7,-122.3,37.9
```

Row groups whose bounding boxes don't intersect the filter are skipped entirely.

### Size-Based Feature Dropping

Drop the smallest features first when tiles are dense (tippecanoe parity):

```bash
gpq-tiles input.parquet output.pmtiles \
  --drop-smallest-as-needed \
  --drop-smallest-threshold 4.0  # square pixels (default)
```

Useful for:
- Building footprints (drop tiny sheds/outbuildings at high zoom)
- Dense point data (drop smallest markers)
- Polygon layers (drop single-pixel features)

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
