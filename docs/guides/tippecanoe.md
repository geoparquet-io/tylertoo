# Coming from tippecanoe

[tippecanoe](https://github.com/felt/tippecanoe) is the reference
implementation tylertoo's tiling is measured against, so its concepts carry
over. tylertoo applies them to stored overview levels in world space rather
than to each tile as it is encoded. Deliberate divergences are recorded in
`context/ARCHITECTURE.md`. Measured speed, size, and memory comparisons, with
their method and caveats, are in
[`benchmarks/e2e/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/RESULTS.md).

At defaults the two tools do not draw the same map. tylertoo's density budget
thins features hard at coarse and middle zooms, while tippecanoe keeps far more
of a contiguous polygon coverage. For admin boundaries, parcels, or a
choropleth, use `--verbatim`, or raise `--gsd-base` and lower the thinning
factors; see the [tuning reference](../OVERVIEW_TUNING.md).

## Flag mapping

| tippecanoe | tylertoo | Note |
|---|---|---|
| `-z` / `-Z` maximum/minimum zoom | `--max-zoom` / `--min-zoom` | Same zoom range |
| `-zg` guess the maximum zoom | `--max-zoom auto` | Inspired by `-zg`, not a port: a bounded sample of feature extents and spacing, resolved at 256 px per tile and capped at z16. Like `-zg`, an input with nothing to measure is an error |
| `-l` layer name | `--layer-name` | Set at export |
| `-L` one layer per input | `pyramid --band LO-HI:INPUT:LAYER …` with bands sharing a zoom range | Each band is its own ladder; tiles at shared zooms carry every layer |
| `-b` buffer (default 5) | `--tile-buffer` (default 8) | Tile-pixel seam buffer |
| `-y` / `-x` / `-X` property selection | `--include-property` / `--exclude-property` / `--exclude-all-properties` | Applied at scan time on `overview` / `tiles`, at export on `export-pmtiles`. Same precedence: an include list wins and `-x` / `-X` are then ignored |
| `-r` drop rate (default 2.5) | `--drop-rate` (default 1.65) | Same geometric ladder; tylertoo anchors on the full canonical count, so the default differs |
| gamma dot-dropping | `--drop-gamma` | Applied per super-cell, leaving per-level totals unchanged |
| `-S` simplification | `--simplify-factor` | RDP, cascading by default |
| `-M` maximum tile bytes (default 500K) | `--max-tile-size` / `--tile-size-limit` (default 500K) | Same default; 0 disables the cap |
| `--drop-fraction-as-needed` tile-size loop | `--tile-size-limit` | Single non-iterative drop pass, since levels are already budgeted |
| tiny-polygon reduction | `--collapse-square` | Area accumulator per 32×GSD patch of the level (tile-less), plus a per-feature dither for write-time collapses |
| cluster centroid | `--cluster` | Winner keeps its own geometry and absorbs losers into `point_count` |
| `--coalesce` family | coalescing (on by default) | Chains same-class segments before gates and thinning |
| `--use-attribute-for-id` | `--feature-id` | Integer or `DECIMAL(p,0)` columns only (string and float ids are rejected, not parsed); null or negative values are errors, not warnings. Without it, ids are tile-local. See [stable feature ids](../OVERVIEW_TUNING.md#stable-feature-ids-feature-id) |
| `tile-join` to merge tilesets | `merge`, or `pyramid` bands sharing a zoom range | `merge` takes archives with disjoint tiles |
| `tippecanoe-decode` | `decode` | Writes GeoParquet: one row per feature per tile, with `zoom`, `layer`, and `mvt_id` columns, in tiled (simplified, clipped) geometry |

## What only tylertoo does

- **Reads GeoParquet directly**, with no GeoJSON conversion. It decodes only
  the columns it needs, prunes row groups with `--bbox` and `--filter`, and
  reads remote objects by byte range. See [remote reads](remote-reads.md).
- **Writes an overview file.** The world-space levels live in a GeoParquet
  file you can query with DuckDB, export more than once, and validate against
  the `geo:overviews` spec.
- **Splits one build across machines** and merges the shards without
  re-encoding a tile. See [scaling](scaling.md#sharded-builds).

## What only tippecanoe does

- **Reads more formats:** GeoJSON, line-delimited GeoJSON, FlatGeobuf, point
  CSV, and GeoJSON on standard input. tylertoo reads GeoParquet in
  `EPSG:4326` or `EPSG:3857` only; convert other formats with `gpio`.
- **Joins attributes onto finished tiles** with `tile-join`. tylertoo leaves
  this out on purpose: join in Parquet, before tiling, so the new columns work
  with `--filter`, `--include-property`, and `--feature-id`. Then restore the
  Hilbert order and row-group layout the join discards:

```bash
duckdb -c "LOAD spatial; COPY (
  SELECT p.*, c.population
  FROM 'parcels.parquet' p
  JOIN 'census.csv' c USING (geoid)
) TO 'joined.parquet' (FORMAT parquet)"
gpio sort hilbert joined.parquet parcels-joined.parquet \
  --row-group-size-mb 128
```

- **Writes MBTiles or a tile directory.** tylertoo writes one PMTiles
  archive. Convert it with `pmtiles-convert` from the Python
  [`pmtiles`](https://pypi.org/project/pmtiles/) package, which picks the
  format from the output path. A directory holds gzip-compressed
  `{z}/{x}/{y}.mvt` files and a `metadata.json`:

```bash
uvx --from pmtiles pmtiles-convert tiles.pmtiles tiles.mbtiles
uvx --from pmtiles pmtiles-convert tiles.pmtiles tiles/
```
