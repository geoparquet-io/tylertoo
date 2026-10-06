# tylertoo

[![CI](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml/badge.svg)](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/geoparquet-io/tylertoo/branch/main/graph/badge.svg)](https://codecov.io/gh/geoparquet-io/tylertoo)
[![Crates.io](https://img.shields.io/crates/v/tylertoo?color=blue)](https://crates.io/crates/tylertoo)
[![PyPI](https://img.shields.io/pypi/v/tylertoo?color=blue)](https://pypi.org/project/tylertoo/)

GeoParquet to PMTiles with a Rust engine, CLI, and Python API. ~7.5× faster than tippecanoe (see [benchmarks](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/RESULTS.md)). Supports streaming, bounded-memory processing, remote reads, and sharded builds across multiple machines. Named after [“Tippecanoe and Tyler Too.”](https://en.wikipedia.org/wiki/Tippecanoe_and_Tyler_Too)

## Install

```bash
cargo install tylertoo    # CLI
pip install tylertoo      # Python
```

## Quickstart

Tile 17,465 Madagascar boundary polygons (28 MB) into a PMTiles archive:

```bash
cargo install tylertoo
repo=https://github.com/geoparquet-io/tylertoo
file=fieldmaps-madagascar-adm4.parquet
curl -LO "$repo/releases/download/fixtures-v1/$file"
tylertoo "$file" madagascar.pmtiles --max-zoom 10
```

Or in Python:

```python
import tylertoo

tylertoo.overview("fieldmaps-madagascar-adm4.parquet", "madagascar-ov.parquet", max_zoom=10)
tylertoo.export_pmtiles("madagascar-ov.parquet", "madagascar.pmtiles")
```

See the [docs site](https://geoparquet-io.github.io/tylertoo/) for end-to-end tutorials and the API reference.

## Contributing

See [CONTRIBUTING.md](https://github.com/geoparquet-io/tylertoo/blob/main/CONTRIBUTING.md)
and [DEVELOPMENT.md](https://github.com/geoparquet-io/tylertoo/blob/main/DEVELOPMENT.md).

## License

Apache-2.0
