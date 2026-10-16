<p align="center">
  <img src="https://raw.githubusercontent.com/geoparquet-io/tylertoo/main/docs/assets/logo-full.png" alt="tylertoo" width="320">
</p>

[![CI](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml/badge.svg)](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/geoparquet-io/tylertoo/branch/main/graph/badge.svg)](https://codecov.io/gh/geoparquet-io/tylertoo)
[![Crates.io](https://img.shields.io/crates/v/tylertoo?color=blue)](https://crates.io/crates/tylertoo)
[![PyPI](https://img.shields.io/pypi/v/tylertoo?color=blue)](https://pypi.org/project/tylertoo/)

GeoParquet to PMTiles with a Rust engine, CLI, and Python API. 8.6–21.3× faster than tippecanoe end-to-end on 300k–813k-feature line and polygon datasets at each tool's production settings — 5.9–16.3× with tylertoo's feature thinning switched off, and at up to 2.1× peak memory (see [benchmarks](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/RESULTS.md) for methods, caveats, and the point dataset, whose run compared unequal zoom ranges). Supports streaming, bounded-memory processing, remote reads, and sharded builds across multiple machines. Named after [“Tippecanoe and Tyler Too.”](https://en.wikipedia.org/wiki/Tippecanoe_and_Tyler_Too)

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

The intermediate `madagascar-ov.parquet` is a [`geo:overviews`](https://geoparquet-io.github.io/tylertoo/spec/overviews/) file (draft spec): a GeoParquet file that carries its own zoom pyramid and stays queryable as ordinary GeoParquet.

See the [docs site](https://geoparquet-io.github.io/tylertoo/) for end-to-end tutorials and the API reference.

## Contributing

See [CONTRIBUTING.md](https://github.com/geoparquet-io/tylertoo/blob/main/CONTRIBUTING.md)
and [DEVELOPMENT.md](https://github.com/geoparquet-io/tylertoo/blob/main/DEVELOPMENT.md).

## License

Apache-2.0
