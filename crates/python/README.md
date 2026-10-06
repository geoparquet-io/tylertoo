# tylertoo

[![CI](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml/badge.svg)](https://github.com/geoparquet-io/tylertoo/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/geoparquet-io/tylertoo/branch/main/graph/badge.svg)](https://codecov.io/gh/geoparquet-io/tylertoo)
[![Crates.io](https://img.shields.io/crates/v/tylertoo?color=blue)](https://crates.io/crates/tylertoo)
[![PyPI](https://img.shields.io/pypi/v/tylertoo?color=blue)](https://pypi.org/project/tylertoo/)

tylertoo turns GeoParquet into PMTiles vector tiles. On the way it writes an
**overview file**: a GeoParquet file with a generalized copy of your data for
every zoom level. You can validate the overview file, query it with SQL, and
export it again without rebuilding. The CLI and the Python package are thin wrappers over one
Rust engine, which is also published as a crate.

The name nods to [tippecanoe](https://github.com/felt/tippecanoe), the
vector-tile tool tylertoo measures itself against
([benchmarks](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/RESULTS.md)).

## Quickstart

Tile 17,465 Madagascar boundary polygons (28 MB) into a PMTiles archive:

```bash
cargo install tylertoo
repo=https://github.com/geoparquet-io/tylertoo
file=fieldmaps-madagascar-adm4.parquet
curl -LO "$repo/releases/download/fixtures-v1/$file"
tylertoo "$file" madagascar.pmtiles --max-zoom 10
```

Drop `madagascar.pmtiles` onto [pmtiles.io](https://pmtiles.io/) to view it.
The same pipeline in Python, keeping the overview file:

```python
import tylertoo

tylertoo.overview("fieldmaps-madagascar-adm4.parquet", "madagascar-ov.parquet", max_zoom=10)
tylertoo.export_pmtiles("madagascar-ov.parquet", "madagascar.pmtiles")
```

## Install

```bash
cargo install tylertoo    # CLI
pip install tylertoo      # Python
```

Every [GitHub release](https://github.com/geoparquet-io/tylertoo/releases)
carries prebuilt CLI binaries for Linux, macOS, and Windows.

## Documentation

The [documentation site](https://geoparquet-io.github.io/tylertoo/) has:

- Two end-to-end tutorials:
  [a local file, from raw export to PMTiles](https://geoparquet-io.github.io/tylertoo/tutorials/madagascar/),
  and [cloud data into a sharded, two-layer archive](https://geoparquet-io.github.io/tylertoo/tutorials/brazil/)
- Guides to [scaling](https://geoparquet-io.github.io/tylertoo/guides/scaling/),
  [remote reads](https://geoparquet-io.github.io/tylertoo/guides/remote-reads/),
  and [coming from tippecanoe](https://geoparquet-io.github.io/tylertoo/guides/tippecanoe/)
- The [CLI](https://geoparquet-io.github.io/tylertoo/reference/cli/),
  [Python](https://geoparquet-io.github.io/tylertoo/reference/python/), and
  [Rust](https://docs.rs/tylertoo-core) API references, generated from source
- The [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/)
  for every generalization knob

tylertoo is working toward 1.0. The
[Road to 1.0](https://github.com/geoparquet-io/tylertoo/issues/463) issue
tracks what may still change.

## Contributing

See [CONTRIBUTING.md](https://github.com/geoparquet-io/tylertoo/blob/main/CONTRIBUTING.md)
and [DEVELOPMENT.md](https://github.com/geoparquet-io/tylertoo/blob/main/DEVELOPMENT.md).

## License

Apache-2.0
