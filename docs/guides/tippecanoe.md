# Coming from tippecanoe

[tippecanoe](https://github.com/felt/tippecanoe) is tylertoo's tiling reference.
tylertoo applies the same concepts to stored overview levels in world space
before encoding tiles. `context/ARCHITECTURE.md` describes the deliberate
differences.
[`benchmarks/e2e/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/RESULTS.md)
compares speed, size, and memory use, with methods and caveats.

The defaults produce different maps: tylertoo thins features heavily at coarse
and middle zooms; tippecanoe retains more of a contiguous polygon coverage.
For admin boundaries, parcels, or a choropleth, use `--verbatim` or raise
`--gsd-base` and lower the thinning factors. See the
[tuning reference](../OVERVIEW_TUNING.md).

## Flag mapping

| tippecanoe | tylertoo | Note |
|---|---|---|
| `-z` / `-Z` maximum/minimum zoom | `--max-zoom` / `--min-zoom` | Same zoom range |
| `-zg` guess the maximum zoom | `--max-zoom auto` | Uses a bounded sample of feature extents and spacing at 256 px per tile, capped at z16. Inspired by `-zg`; the algorithm differs. Both reject input with nothing to measure |
| `-l` layer name | `--layer-name` | Set at export |
| `-L` one layer per input | `pyramid --band LO-HI:INPUT:LAYER …` with bands sharing a zoom range | Each band has its own ladder; tiles at shared zooms contain every layer |
| `-b` buffer (default 5) | `--tile-buffer` (default 8) | Tile-pixel seam buffer |
| `-y` / `-x` / `-X` property selection | `--include-property` / `--exclude-property` / `--exclude-all-properties` | Applied during scans for `overview` / `tiles`, during export for `export-pmtiles`. In both tools, an include list overrides exclusions |
| `-r` drop rate (default 2.5) | `--drop-rate` (default 1.65) | Same geometric ladder. tylertoo anchors on the full canonical count, so the default differs |
| gamma dot-dropping | `--drop-gamma` | Applied per super-cell; per-level totals stay unchanged |
| `-S` simplification | `--simplify-factor` | Ramer-Douglas-Peucker (RDP), cascading by default |
| `-M` maximum tile bytes (default 500K) | `--max-tile-size` / `--tile-size-limit` (default 500K) | Same default; 0 disables the cap |
| `--drop-fraction-as-needed` tile-size loop | `--tile-size-limit` | One drop pass; stored levels already have feature budgets |
| tiny-polygon reduction | `--collapse-square` | Accumulates area per 32×GSD patch of the level, independent of tiles, with per-feature dithering for write-time collapses |
| cluster centroid | `--cluster` | Winner keeps its own geometry and absorbs losers into `point_count` |
| `--coalesce` family | coalescing (on by default) | Chains same-class segments before gates and thinning |
| `--use-attribute-for-id` | `--feature-id` | Accepts integer or `DECIMAL(p,0)` columns. Rejects string, float, null, and negative ids. Without this flag, ids are tile-local. See [stable feature ids](../OVERVIEW_TUNING.md#stable-feature-ids-feature-id) |
| `tile-join` to merge tilesets | `merge`, or `pyramid` bands sharing a zoom range | `merge` takes archives with disjoint tiles |
| `tippecanoe-decode` | `decode` | Writes GeoParquet with one row per feature per tile, `zoom`, `layer`, and `mvt_id` columns, and simplified, clipped tile geometry |

## What only tylertoo does

- Reads GeoParquet directly. It decodes required columns, prunes row groups
  with `--bbox` and `--filter`, and reads remote objects by byte range. See
  [remote reads](remote-reads.md).
- Stores world-space overview levels in GeoParquet. Query them with DuckDB,
  export them multiple times, or validate them against the `geo:overviews` spec.
- Splits builds across machines and merges shards without re-encoding tiles.
  See [scaling](scaling.md#sharded-builds).

## What only tippecanoe does

- Reads GeoJSON, line-delimited GeoJSON, FlatGeobuf, point CSV, and GeoJSON
  on standard input. tylertoo reads GeoParquet in `EPSG:4326` or `EPSG:3857`.
  Convert other formats with `gpio`.
- Joins attributes onto finished tiles with `tile-join`. In tylertoo, join
  before tiling so the new columns work with `--filter`, `--include-property`,
  and `--feature-id`. Restore Hilbert order and row-group layout after the join:

    ```bash
    duckdb -c "LOAD spatial; COPY (
      SELECT p.*, c.population
      FROM 'parcels.parquet' p
      JOIN 'census.csv' c USING (geoid)
    ) TO 'joined.parquet' (FORMAT parquet)"
    gpio sort hilbert joined.parquet parcels-joined.parquet \
      --row-group-size-mb 128
    ```

- Writes MBTiles or a tile directory. tylertoo writes one PMTiles archive;
  convert it with `pmtiles-convert` from the Python
  [`pmtiles`](https://pypi.org/project/pmtiles/) package. The output path selects
  the format. A directory contains gzip-compressed `{z}/{x}/{y}.mvt` files
  and `metadata.json`:

    ```bash
    uvx --from pmtiles pmtiles-convert tiles.pmtiles tiles.mbtiles
    uvx --from pmtiles pmtiles-convert tiles.pmtiles tiles/
    ```
