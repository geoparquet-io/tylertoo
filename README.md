# tylertoo

[![CI](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml/badge.svg)](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/geoparquet-io/tylertoo/branch/main/graph/badge.svg)](https://codecov.io/gh/geoparquet-io/tylertoo)
[![Crates.io](https://img.shields.io/crates/v/tylertoo?color=blue)](https://crates.io/crates/tylertoo)
[![PyPI](https://img.shields.io/pypi/v/tylertoo?color=blue)](https://pypi.org/project/tylertoo/)

Fast GeoParquet → PMTiles converter in Rust.

**tylertoo** takes its name from ["Tippecanoe and Tyler Too"](https://en.wikipedia.org/wiki/Tippecanoe_and_Tyler_Too),
the 1840 U.S. campaign slogan. It's a nod to [tippecanoe](https://github.com/felt/tippecanoe),
the vector-tile tool this project measures itself against — tylertoo runs alongside it.

> **⚠️ Work in progress:** code is generated with Claude; take it with a grain of salt. —Nissim

## Features

- Multi-resolution **overviews embedded in valid, SQL-queryable GeoParquet**.
- One-shot GeoParquet → PMTiles, or a two-step overview → export workflow.
- Memory-bounded streaming conversion that scales past RAM.
- Remote inputs (`s3://`, `https://`, `gs://`) read via byte-range requests.
- Attribute filtering (`--filter` / `--where`) with row-group statistics pushdown.
- A quality ladder tuned against tippecanoe, plus PMTiles decoding and spec validation.

## Install

```bash
cargo install tylertoo    # CLI
pip install tylertoo      # Python
```

## Quick start

```bash
# One-shot: GeoParquet in, PMTiles out
tylertoo input.parquet output.pmtiles --min-zoom 0 --max-zoom 14

# Or two steps, keeping the reusable overview file
tylertoo overview input.parquet overviews.parquet --max-zoom 14
tylertoo export-pmtiles overviews.parquet output.pmtiles
```

## Documentation

- **[Getting Started](https://geoparquet-io.github.io/tylertoo/getting-started/)** — install, one-shot conversion, the two-step workflow.
- **[Diving Deeper](https://geoparquet-io.github.io/tylertoo/diving-deeper/)** — input prep, zoom tuning, remote/multi-file input, bounded memory.
- **[Reference](https://geoparquet-io.github.io/tylertoo/reference/)** — generated CLI, Python, and Rust API surface.

## Development

```bash
git clone https://github.com/geoparquet-io/tylertoo.git && cd tylertoo
git config core.hooksPath .githooks
cargo build && cargo check
```

## License

Apache-2.0
