# Advanced Usage

Generalization knobs (thinning, visibility, simplification, ranking,
density budget, clustering, coalescing) are covered exhaustively in
[Overview Tuning](OVERVIEW_TUNING.md) — this page covers everything
around them: input preparation, memory, output layout, consuming the
results, and debugging.

## Input File Optimization

**Always prepare GeoParquet files with
[geoparquet-io](https://github.com/geoparquet-io/geoparquet-io) (gpio)
before converting.** The converter assumes Hilbert-sorted, bbox-covered,
WGS84 input — Hilbert order within each output level comes from the
sorted-input contract (the pipeline never re-sorts).

```bash
# Reproject (if needed) + Hilbert sort + sane row groups, in one pass
gpio convert reproject input.parquet prepared.parquet \
  -d EPSG:4326 --hilbert --row-group-size 100000
```

Non-WGS84 input is rejected with the exact command to fix it.

## Memory Control

Conversion streams by default (two passes; peak memory
`O(read batch + winner tables)` — a 632k-polygon / 38M-vertex file
converts in ~320 MB peak RSS). Two knobs, both content-neutral:

```bash
# Tighter memory bound (e.g. monster multipolygons, small machines)
gpq-tiles overview big.parquet out.parquet --read-batch-size 1024

# Reference in-memory pipeline (small inputs only; O(dataset) memory)
gpq-tiles overview small.parquet out.parquet --no-streaming
```

Line coalescing holds one level's candidate lines in memory; datasets
with more lines than `--coalesce-max-level-rows` (default 2,000,000)
skip coalescing per level with a warning instead of breaking the bound.

See the "Memory / streaming knobs" section of
[Overview Tuning](OVERVIEW_TUNING.md) for details.

## Output Layout for Remote Reads

Overview files are designed to be queried over HTTP range requests
(DuckDB httpfs etc.). Two knobs control the physical Parquet layout:

- `--row-group-size` (default 10000) — a **per-level** cap; smaller =
  tighter bbox pruning per viewport, more row groups.
- `--full-column-stats` — by default, per-row-group min/max stats on the
  geometry and string/binary columns are suppressed (they can bloat the
  footer to megabytes, paid on every remote query). The bbox covering
  and `level` column always keep pruning stats. Opt in only if remote
  clients push predicates on property columns.

Measured effects: [`benchmarks/overview/RESULTS.md`](https://github.com/geoparquet-io/gpq-tiles/blob/main/benchmarks/overview/RESULTS.md)
(the H1 revision note).

## Reading Overview Files Without gpq-tiles

The file is plain GeoParquet 1.1 with a `level` INT32 column and a
`geo:overviews` footer key:

```sql
-- DuckDB: coarse quick-look (level 0 = coarsest)
SELECT * FROM read_parquet('overviews.parquet') WHERE level = 0;

-- Exact source data (canonical = finest level)
SELECT * FROM read_parquet('overviews.parquet')
WHERE level = (SELECT max(level) FROM read_parquet('overviews.parquet'));
```

The format contract is `context/OVERVIEWS_SPEC.md`; a dedicated
consumption guide (DuckDB recipes, GeoPandas, browser demo) is tracked
in [#175](https://github.com/geoparquet-io/gpq-tiles/issues/175).

## Export Details

- **Compression:** exported tiles are always gzip (the
  PMTiles-viewer-safe default).
- **Zoom mapping:** each overview level maps to a Web Mercator zoom
  (explicit `levels[].zoom` in the footer, else derived from its GSD);
  the PMTiles header min/max zoom come from that range and renderers
  overzoom beyond it.
- **Tile size:** `--tile-size-limit BYTES` is a single, non-iterative
  drop pass per oversized tile — the overview density budget is the
  real sizing mechanism; the limit is a backstop.
- **Border duplication:** a feature spanning a tile seam appears in
  every tile it touches, so per-zoom exported feature totals slightly
  exceed the level counts (~0–7%).

## Conversion Reports

Both `overview` and `export-pmtiles` accept `--report PATH` (Python:
the functions return the same report as a dict): per-level feature and
vertex counts, drops per mechanism, per-zoom tile stats, oversized-tile
counts. Useful for regression-checking a tuning change:

```bash
gpq-tiles overview in.parquet out.parquet --report report.json
jq '.levels[] | {level, features, vertices}' report.json
```

## Debugging

```bash
# Spec validation (footer, level banding, canonical fidelity, invariants)
gpq-tiles validate overviews.parquet

# Phase-level timing/diagnostics from the pipeline
RUST_LOG=gpq_tiles_core::overview=debug \
  gpq-tiles overview in.parquet out.parquet

# Inspect exported tiles
pmtiles show output.pmtiles

# Inspect the overview file itself
gpio inspect overviews.parquet
```

For heap and wall-time profiling, see [Profiling](PROFILING.md).

## CI/CD Integration

```yaml
name: Generate Tiles

on:
  push:
    paths:
      - 'data/*.parquet'

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Install gpq-tiles
        run: cargo install gpq-tiles

      - name: Build overviews + tiles
        run: |
          gpq-tiles overview data/input.parquet output/overviews.parquet \
            --min-zoom 0 --max-zoom 14
          gpq-tiles validate output/overviews.parquet
          gpq-tiles export-pmtiles output/overviews.parquet output/tiles.pmtiles

      - name: Upload artifacts
        uses: actions/upload-artifact@v4
        with:
          name: tiles
          path: output/
```
