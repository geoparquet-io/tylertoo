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
--simplify-factor 1.0` so it cannot win by emitting fewer features. Median
of 3 warm runs per tool, alternating the tools within each round, on an idle
64-core arm64 host (the run line under the table names it).

<!-- bench:e2e-corpus/tippecanoe-table -->
| dataset (class) | features | tylertoo | tippecanoe e2e | speedup | speedup, thinning off | peak RSS (tylertoo / tippecanoe) |
|---|---|---|---|---|---|---|
| NYC places (point) † | 630,065 | 1.57 s | 42.66 s | **27.2×** | 2.2×* | 2,210 / 1,352 MB |
| Portland buildings (polygon) | 813,456 | 3.68 s | 84.81 s | **23.0×** | 13.4× | 3,132 / 2,127 MB |
| Oregon roads (line) | 1,203,997 | 10.65 s | 180.69 s | **17.0×** | 16.9× | 9,051 / 2,435 MB |
| Moldova parcels (polygon, dense) | 631,910 | 7.78 s | 167.27 s | **21.5×** | 17.5× | 4,520 / 2,431 MB |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

The honest footnotes, which are the point of this table:

- † *The point row is not a parity measurement at all.* tippecanoe's recorded
  flags (`--extend-zooms-if-still-dropping`) had it building z15–z16 — 9,694
  of its 10,543 tiles and 69% of its output bytes — while tylertoo stopped at
  the requested z14. Both of that row's ratios include that extra work.
- *The point row's "thinning off" 2.2× is the weakest number this harness has
  produced, and it carries a second mismatch on top of †:* `--verbatim` makes
  tylertoo carry all 630k points at every zoom (1.57 s → 18.38 s). The two
  tools also thin points by different rules, so they keep different points
  at each zoom ([#666](https://github.com/geoparquet-io/tylertoo/issues/666)
  measures it on this dataset). Read the line and polygon rows for parity
  numbers.
- tylertoo's peak memory is **higher on all four datasets** — up to <!-- bench:e2e-corpus/max-memory-ratio -->3.7×<!-- /bench -->
  on lines. The speed is parallelism (19–29 of 64 cores busy vs tippecanoe's
  2–3) plus the native GeoParquet read, paid for in working set.
- Archives are equal on Moldova (1.00×) and larger elsewhere: Oregon roads
  1.12× at defaults (1.23× thinning off), Portland buildings 1.41× (1.47×).
  For points, the 0.65× compares tylertoo's z0–14 archive against
  tippecanoe's z0–16; at z0–14 on both sides tylertoo's is 2.1× bigger.
- Converting GeoParquet for tippecanoe is only 2–4% of its end-to-end wall at
  this scale — the gap is tiler vs tiler, not a format-conversion tax.
- On a small contiguous admin coverage (the 17k-polygon fixture set), the
  parity-matched ratio is **≈7.5×**, not 16–20×.

## At scale: 21.6M Overture buildings

The Netherlands bbox of Overture buildings release `2026-10-21.0`, every
part pinned by ETag. tylertoo reads the GeoParquet parts straight from
`s3://` with `--bbox`, fetching only the row groups the bbox touches.
tippecanoe can't read GeoParquet or S3, so its time includes extracting the
same bbox to local GeoParquet with gpio and converting it to FlatGeobuf.

<!-- bench:overture-buildings-netherlands/vida-head-to-head -->
Overture buildings 2026-10-21.0, netherlands (bbox 3.31,50.75,7.23,53.56): 21,557,082 features, 1.9 GB.

| tool | end-to-end wall | peak RSS | effective cores | archive | note |
|---|---|---|---|---|---|
| tylertoo, s3:// in place | 107 s | 15.3 GB | 16.0 | 0.9 GB | reads the parts in place; no extract |
| tylertoo, local extract | 321 s | 13.1 GB | 22.7 | 0.9 GB | end to end includes the gpio extract |
| tippecanoe v2.79.0 | 1,417 s | 6.1 GB | 3.1 | 0.8 GB | extract + FlatGeobuf + tile |

_Run: commit 875b1bdc9478e323668d48c366888c4ecb36d6f4, tylertoo 0.8.0, tippecanoe v2.79.0, gpio 1.7.0, aarch64, 0xd40, 2026-11-06._
<!-- /bench -->

Every number above is from a committed record
([`benchmarks/results/e2e-corpus.json`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/results/e2e-corpus.json),
[`e2e-fixtures.json`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/results/e2e-fixtures.json))
with full provenance, per-zoom tile counts, and the asymmetry list in
[`benchmarks/e2e/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/RESULTS.md).
Reproduce with `./benchmarks/e2e/run_corpus.sh`.

## Flag mapping

| tippecanoe | tylertoo | Note |
|---|---|---|
| `-z` / `-Z` maximum/minimum zoom | `--max-zoom` / `--min-zoom` | Same zoom range |
| `-zg` guess the maximum zoom | `--max-zoom auto` | Uses a bounded sample of feature extents and spacing at 256 px per tile, capped at z16 (`auto:N` lowers the cap to zN). Inspired by `-zg`; the algorithm differs. Both reject input with nothing to measure |
| `-l` layer name | `--layer-name` | Set at export |
| `-L` one layer per input | `pyramid --band LO-HI:INPUT:LAYER …` with bands sharing a zoom range | Each band has its own ladder; tiles at shared zooms contain every layer |
| `-n` name / `-N` description / `-A` attribution | `--name` / `--description` / `--attribution` | An unset field is left out of the archive. tippecanoe names the tileset after the output file and copies the name into `description`. `merge` and `pyramid` keep every inherited credit; see [attribution](attribution.md) |
| `-b` buffer (default 5) | `--tile-buffer` (default 8) | Tile-pixel seam buffer |
| `-y` / `-x` / `-X` property selection | `--include-property` / `--exclude-property` / `--exclude-all-properties` | Applied during scans for `overview` / `tiles`, during export for `export-pmtiles`. In both tools, an include list overrides exclusions |
| `-r` drop rate (default 2.5) | `--drop-rate` (default 1.65) | Same geometric ladder. tylertoo anchors on the full canonical count, so the default differs. Thins points only by default, as in tippecanoe |
| `--drop-lines` / `--drop-polygons` | `--drop-lines` / `--drop-polygons` | Apply the drop rate to lines and polygons too |
| gamma dot-dropping | `--drop-gamma` | Applied per super-cell; per-level totals stay unchanged |
| `-S` simplification | `--simplify-factor` | Ramer-Douglas-Peucker (RDP), cascading by default |
| `-pn` / `--no-simplification-of-shared-nodes` | none yet (#450) | Each polygon is simplified on its own, so thin slivers open along shared borders: on dense parcels about 2% of the area at z12 at the default factor. `--simplify-factor 0.25` reaches tippecanoe's coverage; `0` closes the slivers. On Madagascar admin-4 at z4–z8 residual missing coverage with `--coverage-fill` is 0.03–0.05%. See [coverage and simplification](../OVERVIEW_TUNING.md#coverage-and-simplification) |
| tiny-polygon reduction | placeholder squares (the default; `--no-collapse` to drop) | Accumulates area per 32×GSD patch of the level, independent of tiles, with per-feature dithering for write-time collapses. The square is one pixel regardless of `--simplify-factor`. In dense blocks of sub-pixel polygons the squares overlap and leave gaps (#750) |
| `-M` maximum tile bytes (default 500K) | `--max-compressed-tile-size` (default 500K), with `--max-tile-size` / `--tile-size-limit` (default 2M) | Both cap the gzip-compressed tile at 500K. tylertoo also caps the uncompressed MVT at 2M, which tippecanoe does not. 0 disables either |
| `--coalesce-densest-as-needed` | `--size-valve merge` (the default) | Merges the densest lines and polygons into the feature kept before them, measuring again until the tile fits. The merged feature keeps its own attributes. Each tile uses its own threshold; tippecanoe raises one threshold for the whole zoom |
| `--drop-densest-as-needed`, `--drop-fraction-as-needed` | `--size-valve drop` | Drops features until the tile fits, keeping the largest polygons and lines and a uniform stride of points |
| cluster centroid | `--cluster` | Winner keeps its own geometry and absorbs losers into `point_count` |
| `--coalesce` family | coalescing (on by default) | Chains same-class segments before gates and thinning |
| `--use-attribute-for-id` | `--feature-id` | Accepts integer or `DECIMAL(p,0)` columns. Rejects string, float, null, and negative ids. Without this flag, features carry no id, as in tippecanoe. See [stable feature ids](../OVERVIEW_TUNING.md#stable-feature-ids-feature-id) |
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
