# Coming from tippecanoe

[tippecanoe](https://github.com/felt/tippecanoe) is tylertoo's tiling reference.
tylertoo applies the same concepts to stored overview levels in world space
before encoding tiles. `context/ARCHITECTURE.md` describes the deliberate
differences.
[`benchmarks/e2e/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/RESULTS.md)
compares speed, size, and memory use, with methods and caveats.

Like tippecanoe, tylertoo thins points by default and keeps lines and
polygons. This preserves polygon coverage at coarse and middle zooms.
Polygons too small for a zoom become placeholder squares. For oversized
tiles, tylertoo merges dense polygons into their neighbors, as
`--coalesce-densest-as-needed` does. To adjust this behavior, see
[complete tiles or small tiles](../OVERVIEW_TUNING.md#complete-tiles-or-small-tiles).

## How much faster — the skeptical reader's table

Measured end-to-end (GeoParquet in, z0–z14 PMTiles out) against **tippecanoe
v2.79.0 built from a verified pinned commit**, on the corpus datasets with
tippecanoe running its recorded flags for each (`--drop-densest-as-needed`,
`--simplification=10`, …) — both tools in a production configuration, not a
rigged one. The "thinning off" column re-runs tylertoo with `--verbatim
--simplify-factor 1.0` so it cannot win by emitting fewer features. Apple M3
Pro, median of 2 warm runs; one recorded run on a non-idle machine, so treat
ratios as approximate.

| dataset (class) | features | tylertoo | tippecanoe e2e | speedup | speedup, thinning off | peak RSS (tylertoo / tippecanoe) |
|---|---|---|---|---|---|---|
| NYC places (point) † | 597,782 | 1.39 s | 30.03 s | **21.6×** | 2.4×* | 600 / 519 MB |
| Portland roads (line) | 301,609 | 1.40 s | 28.30 s | **20.2×** | 16.3× | 1,611 / 652 MB |
| Portland buildings (polygon) | 813,456 | 2.69 s | 57.22 s | **21.3×** | 16.3× | 999 / 967 MB |
| Moldova parcels (polygon, dense) | 631,910 | 13.87 s | 119.28 s | **8.6×** | 5.9× | 1,848 / 926 MB |

The honest footnotes, which are the point of this table:

- † *The point row is not a parity measurement at all.* tippecanoe's recorded
  flags had it building z15–z16 — 9,639 of its 10,486 tiles and 68% of its
  output bytes — while tylertoo stopped at the requested z14. Both of that
  row's ratios include that extra work.
- *The point row's "thinning off" 2.4× is the weakest number this harness has
  produced, and it carries a second mismatch on top of †:* `--verbatim` makes
  tylertoo carry all 598k points at every zoom (1.39 s → 12.21 s) at 2110 MB
  peak RSS, 4.07× tippecanoe's. Both tools drop points by design at
  production settings, so neither point ratio describes how anyone would
  actually run them. Read the line and polygon rows for parity numbers.
- tylertoo's peak memory is **higher on three of four datasets** — up to 2.5×
  on lines. The speed is parallelism (6–7 cores busy vs tippecanoe's ~2) plus
  the native GeoParquet read, paid for in working set.
- Archive sizes are smaller or equal except Portland buildings (1.11× at
  defaults, 1.20× thinning off) — and except points, where the published
  0.59× compares tylertoo's z0–14 archive against tippecanoe's z0–16. At
  z0–14 on both sides tylertoo's points archive is 1.87× bigger.
- Converting GeoParquet for tippecanoe is only 1–2% of its end-to-end wall at
  this scale — the gap is tiler vs tiler, not a format-conversion tax.
- On a small contiguous admin coverage (the 17k-polygon fixture set), the
  parity-matched ratio is **≈7.5×**, not 16–20×.

Every number above is from a committed record
([`benchmarks/e2e/corpus_results.json`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/corpus_results.json),
[`results.json`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/results.json))
with full provenance, per-zoom tile counts, and the asymmetry list in
[`benchmarks/e2e/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/RESULTS.md).
Reproduce with `./benchmarks/e2e/run_corpus.sh`.

## Flag mapping

| tippecanoe | tylertoo | Note |
|---|---|---|
| `-z` / `-Z` maximum/minimum zoom | `--max-zoom` / `--min-zoom` | Same zoom range |
| `-zg` guess the maximum zoom | `--max-zoom auto` | Uses a bounded sample of feature extents and spacing at 256 px per tile, capped at z16. Inspired by `-zg`; the algorithm differs. Both reject input with nothing to measure |
| `-l` layer name | `--layer-name` | Set at export |
| `-L` one layer per input | `pyramid --band LO-HI:INPUT:LAYER …` with bands sharing a zoom range | Each band has its own ladder; tiles at shared zooms contain every layer |
| `-n` name / `-N` description / `-A` attribution | `--name` / `--description` / `--attribution` | An unset field is left out of the archive. tippecanoe names the tileset after the output file and copies the name into `description`. `merge` and `pyramid` keep every inherited credit; see [attribution](attribution.md) |
| `-b` buffer (default 5) | `--tile-buffer` (default 8) | Tile-pixel seam buffer |
| `-y` / `-x` / `-X` property selection | `--include-property` / `--exclude-property` / `--exclude-all-properties` | Applied during scans for `overview` / `tiles`, during export for `export-pmtiles`. In both tools, an include list overrides exclusions |
| `-r` drop rate (default 2.5) | `--drop-rate` (default 1.65) | Same geometric ladder. tylertoo anchors on the full canonical count, so the default differs. Thins points only by default, as in tippecanoe |
| `--drop-lines` / `--drop-polygons` | `--drop-lines` / `--drop-polygons` | Apply the drop rate to lines and polygons too |
| gamma dot-dropping | `--drop-gamma` | Applied per super-cell; per-level totals stay unchanged |
| `-S` simplification | `--simplify-factor` | Ramer-Douglas-Peucker (RDP), cascading by default |
| `-pn` / `--no-simplification-of-shared-nodes` | none yet (#450) | Each polygon is simplified on its own, so thin slivers open along shared borders: on dense parcels about 2% of the area at z12 at the default factor. `--simplify-factor 0.25` reaches tippecanoe's coverage; `0` closes the slivers. See [coverage and simplification](../OVERVIEW_TUNING.md#coverage-and-simplification) |
| tiny-polygon reduction | placeholder squares (the default; `--no-collapse` to drop) | Accumulates area per 32×GSD patch of the level, independent of tiles, with per-feature dithering for write-time collapses. The square is one pixel regardless of `--simplify-factor`. In dense blocks of sub-pixel polygons the squares overlap and leave gaps (#750) |
| `-M` maximum tile bytes (default 500K) | `--max-compressed-tile-size` (default 500K), with `--max-tile-size` / `--tile-size-limit` (default 2M) | Both cap the gzip-compressed tile at 500K. tylertoo also caps the uncompressed MVT at 2M, which tippecanoe does not. 0 disables either |
| `--coalesce-densest-as-needed` | `--size-valve merge` (the default) | Merges the densest lines and polygons into the feature kept before them, measuring again until the tile fits. The merged feature keeps its own attributes. Each tile uses its own threshold; tippecanoe raises one threshold for the whole zoom |
| `--drop-densest-as-needed`, `--drop-fraction-as-needed` | `--size-valve drop` | Drops features until the tile fits, keeping the largest polygons and lines and a uniform stride of points |
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
