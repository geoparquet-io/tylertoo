# Coming from tippecanoe

[tippecanoe](https://github.com/felt/tippecanoe) is tylertoo's tiling reference.
tylertoo applies the same concepts to stored overview levels in world space
before encoding tiles. `context/ARCHITECTURE.md` describes the deliberate
differences.

Like tippecanoe, tylertoo thins points by default and keeps lines and
polygons. This preserves polygon coverage at coarse and middle zooms.
Polygons too small for a zoom become placeholder squares. For oversized
tiles, tylertoo merges dense polygons into their neighbors, as
`--coalesce-densest-as-needed` does. To adjust this behavior, see
[complete tiles or small tiles](../OVERVIEW_TUNING.md#complete-tiles-or-small-tiles).

For speed, memory, and archive size against tippecanoe, with methods and
caveats, see [benchmarks](../benchmarks.md).

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
| `-S` simplification | `--simplify-factor` | Ramer-Douglas-Peucker (RDP), cascading by default. `-S N` is N tile units; `--simplify-factor N/4` is about the same at z0 to max zoom − 1. tylertoo does not simplify the max zoom; tippecanoe does |
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

- **Reads GeoParquet natively.** There is no conversion step and no
  intermediate file: tylertoo decodes only the columns it needs, straight
  from Arrow. tippecanoe needs the data converted to FlatGeobuf, GeoJSON, or
  CSV first.
- **Streams remote data in place.** Pass an `s3://`, `gs://`, `https://`, or
  `http://` URL where a local path would go. tylertoo reads each file's
  footer, then fetches only the column chunks of the row groups it needs, by
  byte range, without downloading the dataset. See
  [remote reads](remote-reads.md).
- **Reads a multi-file dataset as one input.** Pass a directory, a glob, an
  `s3://` or `gs://` prefix ending in `/`, or a `--files-from` manifest that
  mixes local paths and URLs. Hive layouts such as
  `year=2024/part-00000.parquet` work; tylertoo walks them recursively and
  reads the parts in a fixed order, so the output is reproducible.
- **Skips data it does not need.** `--bbox` uses GeoParquet 1.1 bbox
  covering statistics to skip row groups outside the query before reading
  them, and `--filter` (a SQL `WHERE` predicate) skips row groups whose
  statistics cannot match. See [what gets skipped](remote-reads.md#what-gets-skipped).
- **Bounds its memory.** Conversion streams the input in batches, so peak
  memory follows the largest row group and the feature count, not the file
  size. `--profile bounded` spills buffered levels to disk for the smallest
  footprint. See [one machine, bounded memory](scaling.md#one-machine-bounded-memory).
- **Uses every core.** Reading, generalization, and tile encoding run in
  parallel; on the benchmark host tylertoo kept 19–29 of 64 cores busy, to
  tippecanoe's 2–3. See [benchmarks](../benchmarks.md).
- **Shards a build across machines.** `shard-plan` cuts the job, each machine
  builds its shard, and `merge` joins the archives by copying tiles without
  re-encoding them. Tile bodies match a single-machine build byte for byte.
  See [sharded builds](scaling.md#sharded-builds).
- **Stores overview levels in GeoParquet.** `overview` writes world-space
  generalized levels in the `geo:overviews` format. Query them with DuckDB,
  export them to PMTiles as often as you like with `export-pmtiles`, or check
  them with `validate`.

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
