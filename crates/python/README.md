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

### Multi-Resolution Overviews (Python)

The overview pipeline builds COG-style vector overviews: a single
level-banded GeoParquet file with progressively generalized copies of the
data, plus a PMTiles exporter and a spec validator. All CLI knobs are
exposed with identical defaults.

```python
from gpq_tiles import overview, validate, export_pmtiles

# Polygon workflow (e.g. Moldova buildings/admin areas):
# thin + simplify across z0-z10, then export tiles.
report = overview(
    "moldova.parquet", "moldova-overviews.parquet",
    min_zoom=0, max_zoom=10,
    simplify_factor=1.0,        # RDP tolerance = factor * level GSD
    polygon_visibility=4.0,     # drop polygons smaller than 4 GSDs
    drop_rate=1.65,             # per-level density budget
)
for lvl in report["levels"]:
    print(lvl["level"], lvl["gsd"], lvl["feature_count"])

result = validate("moldova-overviews.parquet")
assert result["valid"], [c for c in result["checks"] if not c["passed"]]

export_pmtiles(
    "moldova-overviews.parquet", "moldova.pmtiles",
    layer_name="buildings",
)

# Clustered point workflow (e.g. NYC trees): each level's surviving
# point absorbs its grid-cell neighbors (point_count column), and
# numeric attributes aggregate across each cluster.
overview(
    "nyc-trees.parquet", "nyc-trees-overviews.parquet",
    min_zoom=8, max_zoom=14,
    cluster=True,                                  # point_thinning defaults to 16.0
    accumulate_attributes={"health_score": "mean"},
    sort_key="diameter", sort_direction="desc",    # biggest tree wins the cell
)
export_pmtiles("nyc-trees-overviews.parquet", "nyc-trees.pmtiles",
               layer_name="trees")
```

Every knob of `gpq-tiles overview` is available: `mode`
("duplicating"/"partitioning"), `gsds`/`gsd_base`, `sort_key`/`sort_direction`,
`class_rank_column`/`class_ranks`/`class_rank_unknown`, `no_auto_rank`,
`simplify_factor`/`collapse`, per-kind `*_thinning` and `*_visibility`
factors, density budget (`density_drop`, `drop_rate`, `drop_gamma`),
clustering (`cluster`, `accumulate_attributes`), line coalescing
(`coalesce_lines`, `coalesce_snap`, `coalesce_junction_angle`,
`coalesce_max_level_rows`), writer options (`row_group_size`,
`full_column_stats`, `cogp_compat`), and streaming controls (`streaming`,
`read_batch_size`). See `help(gpq_tiles.overview)`.

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
