<!-- GENERATED FILE — do not edit by hand.
     Regenerate: cargo run -p tylertoo --features gen-docs -- gen-reference-docs > docs/reference/cli.md
     CI fails if this file drifts from the clap definitions. -->

# CLI reference

This document contains the help content for the `tylertoo` command-line program.

**Command Overview:**

* [`tylertoo`↴](#tylertoo)
* [`tylertoo tiles`↴](#tylertoo-tiles)
* [`tylertoo overview`↴](#tylertoo-overview)
* [`tylertoo validate`↴](#tylertoo-validate)
* [`tylertoo export-pmtiles`↴](#tylertoo-export-pmtiles)
* [`tylertoo decode`↴](#tylertoo-decode)
* [`tylertoo stats`↴](#tylertoo-stats)
* [`tylertoo pyramid`↴](#tylertoo-pyramid)
* [`tylertoo merge`↴](#tylertoo-merge)
* [`tylertoo shard-plan`↴](#tylertoo-shard-plan)

## `tylertoo`

Convert GeoParquet to PMTiles vector tiles and multi-resolution overviews.

`tylertoo INPUT OUTPUT` with no subcommand runs `tiles`.

**Usage:** `tylertoo <COMMAND>`

###### **Subcommands:**

* `tiles` — Generate PMTiles vector tiles from GeoParquet (the default command)
* `overview` — Build a multi-resolution overview GeoParquet file
* `validate` — Check a GeoParquet overview file against the overviews spec
* `export-pmtiles` — Export a PMTiles archive from an overview GeoParquet file
* `decode` — Decode a PMTiles vector-tile archive back to GeoParquet
* `stats` — Report per-zoom tile sizes for a PMTiles archive, from its directory alone
* `pyramid` — Build one archive from several inputs, each owning a zoom range
* `merge` — Combine PMTiles archives that hold disjoint tiles into one
* `shard-plan` — Cut a dataset's tile space into N disjoint shards for a sharded build



## `tylertoo tiles`

Generate PMTiles vector tiles from GeoParquet (the default command)

**Usage:** `tylertoo tiles [OPTIONS] [INPUT] [OUTPUT]`

###### **Arguments:**

* `<INPUT>` — Input GeoParquet in EPSG:4326 or EPSG:3857: a file, a directory or glob of partitions, a remote URL, or an `s3://` or `gs://` prefix. Omit it when `--files-from` is given
* `<OUTPUT>` — Output PMTiles file. Omitted under --plan-only, which writes no archive

###### **Options:**

* `--files-from <PATH>` — Convert the files listed in this manifest instead of INPUT, one path or URL per line in dataset row order. Each line names one `.parquet` file; `#` lines and blank lines are skipped
* `--min-zoom <MIN_ZOOM>` — Minimum (coarsest) Web Mercator zoom

  Default value: `0`
* `--max-zoom <MAX_ZOOM>` — Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a sample of the input. Ignored with `--gsd`

  Default value: `14`
* `--gsd <GSDS>` — Explicit comma-separated GSD list in meters, strictly decreasing. Overrides `--min-zoom` and `--max-zoom`
* `--bbox <XMIN,YMIN,XMAX,YMAX>` — Convert only features whose bbox intersects this lon/lat box. Row groups outside it are skipped without reading their data
* `--layer-name <LAYER_NAME>` — Layer name for the tiles; the input's file stem if unset
* `--max-tile-size <SIZE>` [alias: `tile-size-limit`] — Per-tile MVT size cap, such as `500K` or `1M` (default 500K, or none with `--verbatim`); 0 disables it. A tile over the cap drops features in one pass until it fits
* `--no-simple-clip-fastpath` — Clip every polygon with the full overlay instead of the fast path for simple rings. Use it for byte-stable tiles: the fast path renders the same but can start a ring at a different vertex
* `--tile-buffer <TILE_BUFFER>` — Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width)

  Default value: `8`
* `--partition-wave <N|auto>` — Partitions processed at once during export; `auto` sizes it to the cores and available memory. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `auto`
* `--feature-order <input|COLUMN[:asc|:desc]>` — Within-tile feature order: `input` keeps source row order, and a property name, optionally with `:asc` or `:desc`, sorts each tile by it. A renderer paints in this order unless a style overrides it

  Default value: `input`
* `--feature-id <COLUMN>` — Write this integer property column as the MVT feature id on every tile, so `setFeatureState` keys work across tiles and zooms. Every row must hold a value from 0 to 2^64-1; unset keeps tile-local ids
* `--report <PATH>` — Write a JSON report with `convert` and `export` sections, matching the reports of `overview` and `export-pmtiles`
* `--keep-overview <PATH>` — Keep the intermediate overview GeoParquet at PATH instead of deleting it after the export. The PMTiles output is the same
* `--shard <I/N|coarse>` — Build one job of a sharded fleet: `I/N` for data shard I of N, or `coarse` for the zooms below the pivot. Requires `--shard-plan`, and data shards also need `--plan`; see <https://geoparquet-io.github.io/tylertoo/guides/sharded-builds/>
* `--shard-plan <PATH>` — The shard plan from `tylertoo shard-plan` that every job shares
* `--tile-range <LO..HI>` — Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at one zoom, plus their descendants. Prefer `--shard`, which also skips input the range cannot reach
* `--plan-only` — Write the convert plan (`--save-plan`) and stop, with no export and no OUTPUT. Use it for a coarse job whose tiles the fleet discards
* `-v`, `--verbose` — Enable verbose output (per-level and per-zoom breakdowns)
* `-f`, `--force` — Overwrite the output if it exists
* `--verbatim` — Tile the input exactly as given, with every thinning, simplification, and density step off. Use it for pre-aggregated or pre-levelled input; knobs you set explicitly still win
* `--sort-key <COL>` — Column name used as the cell-winner priority (sort) key. Mutually exclusive with --class-rank
* `--magnitude-ladder <COL>` — Rank the distinct values of COL from high to low and let them set each feature's entry zoom, one `--ladder-step` apart from `--min-zoom`. Features appear from their entry zoom inward, exempt from thinning
* `--ladder-step <N>` — Zooms between consecutive `--magnitude-ladder` rungs

  Default value: `1`
* `--entry-zoom <SPEC>` — Place entry zooms by hand: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...`, for example `density:5000=4,1000=6`. Unlisted values take the ordinary visibility gate
* `--class-rank <SPEC>` — Rank categorical classes for cell winners: `COLUMN:VALUE=RANK,...`, where higher wins. Unlisted values rank below listed ones and above nulls
* `--no-auto-rank` — Disable automatic detection of well-known schemas (Overture roads `class`/`road_class`, Overture places `confidence`)
* `--filter <EXPR>` [alias: `where`] — Convert only features matching this SQL-WHERE predicate over the property columns, such as `confidence > 0.8`. Row groups that cannot match are skipped; the grammar is in the tuning guide: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--include-property <COL>` — Keep only these property columns (repeatable); the rest are never decoded. Columns that other flags read must stay included
* `--exclude-property <COL>` — Drop these property columns (repeatable). Ignored when `--include-property` is given
* `--exclude-all-properties` — Drop every property column, writing geometry only. Ignored when `--include-property` is given
* `--gsd-base <F>` — Base of the zoom-to-GSD mapping, `gsd(z) = 40075016.69 / base / 2^z`: a larger base keeps more detail at every level. No effect with `--gsd`

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance as a multiple of each level's GSD (default 1.0); lower keeps more vertices. Duplicating mode only
* `--collapse` — Collapse polygons too small for a level to a representative point instead of dropping them. Fill styles ignore points, so add a circle layer or use `--collapse-square`
* `--collapse-square` — Replace the polygons a coarse level drops with small placeholder squares, so the level still shows where the area is. The output stays Polygon; duplicating mode only
* `--representation <SPEC>` — Zoom bands that change how polygons render: comma-separated `LO-HI:KIND` with KIND `geom`, `point`, or `square`, for example `0-7:point,8-14:geom`. The band rules are in the tuning guide: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--no-cascade` — Simplify each level from the source instead of from the next finer level. Slower, and reproduces the pre-cascade output byte for byte
* `--point-thinning <POINT_THINNING>` — Point thinning grid cell, as a multiple of the level's GSD (default 4.0, or 16.0 with `--cluster`). Larger cells keep fewer points
* `--line-thinning <LINE_THINNING>` — Line thinning grid cell, as a multiple of the level's GSD (default 1.0). Larger cells keep fewer lines
* `--polygon-thinning <POLYGON_THINNING>` — Polygon thinning grid cell, as a multiple of the level's GSD (default 1.0). Larger cells keep fewer polygons
* `--line-visibility <LINE_VISIBILITY>` — Drop lines whose bbox diagonal is shorter than this many GSDs at a level (default 2.0)
* `--polygon-visibility <POLYGON_VISIBILITY>` — Drop polygons whose bbox diagonal is shorter than this many GSDs at a level (default 2.0)
* `--drop-rate <F>` — Density budget decay: each coarser level keeps 1/rate of the next finer level's feature budget. Larger values thin mid zooms harder

  Default value: `1.65`
* `--drop-gamma <F>` — How strongly the density budget protects sparse areas: 1 cuts every neighborhood equally, and larger values protect sparse ones more

  Default value: `1.5`
* `--no-density-drop` — Turn off the per-level density budget, leaving cell-winner thinning only
* `--cluster` — Merge each thinning cell's points into its surviving point, which gains a `point_count` column. Duplicating mode only
* `--accumulate-attribute <COL:OP>` — Aggregate a numeric column over each cluster as `COL:OP`, where OP is `sum`, `max`, `min`, or `mean` (repeatable). Requires `--cluster`
* `--no-coalesce-lines` — Turn off line coalescing, which joins touching same-class line segments into longer strokes at coarse levels
* `--coalesce-junction-angle <DEG>` — Continue a line through a junction when the straightest pair deviates by at most this many degrees; 0 stops chains at junctions

  Default value: `0.0`
* `--coalesce-snap <F>` — Join line ends within this many GSDs of each other when coalescing; 0 requires exact endpoint matches

  Default value: `1.0`
* `--coalesce-max-level-rows <ROWS>` — Skip coalescing on a level with more candidate lines than this, to bound memory. Long lines hit a matching geometry-size limit first

  Default value: `2000000`
* `--row-group-size <ROW_GROUP_SIZE>` — Maximum rows per output row group, applied per level. It is raised automatically if a file would pass Parquet's row-group limit

  Default value: `10000`
* `--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` — How the row-group cap varies by level: `constant`, or `zoom-scaled`, which doubles it per zoom step coarser than the finest level

  Default value: `constant`

  Possible values: `constant`, `zoom-scaled`

* `--full-column-stats` — Keep Parquet min/max statistics on every column, including large string and geometry columns. Use it when remote clients filter on property columns
* `--no-streaming` — Load the whole dataset into memory instead of streaming it in two passes. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>
* `--read-batch-size <ROWS>` — Rows per Arrow read batch. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `8192`
* `--profile <PROFILE>` — Memory profile for writing levels: `speed` buffers in RAM, `bounded` spills to disk, and `auto` picks per run. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches in flight at once; `auto` sizes it to the machine's cores. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `auto`
* `--read-workers <N|auto>` — Reader threads for the second pass; `auto` uses a quarter of the cores, up to 4. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for spill files: staged remote input, and on `tiles` the intermediate overview. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>
* `--save-plan <PATH>` — Write the convert plan to PATH and keep converting, so a later run can reuse it with `--plan`. A sharded build shares one plan
* `--plan <PATH>` — Reuse the convert plan at PATH and skip the first pass and level assignment. The plan's fingerprint must match this run's version, flags, and inputs



## `tylertoo overview`

Build a multi-resolution overview GeoParquet file

**Usage:** `tylertoo overview [OPTIONS] [INPUT] [OUTPUT]`

###### **Arguments:**

* `<INPUT>` — Input GeoParquet in EPSG:4326 or EPSG:3857: a file, a directory or glob of partitions, a remote URL, or an `s3://` or `gs://` prefix. Omit it when `--files-from` is given
* `<OUTPUT>` — Output overview GeoParquet file

###### **Options:**

* `--files-from <PATH>` — Convert the files listed in this manifest instead of INPUT, one path or URL per line in dataset row order. Each line names one `.parquet` file; `#` lines and blank lines are skipped
* `--mode <MODE>` — Level materialization mode: `duplicating` writes each level in full, and `partitioning` writes each feature once, at its coarsest level

  Default value: `duplicating`

  Possible values: `duplicating`, `partitioning`

* `--min-zoom <MIN_ZOOM>` — Minimum (coarsest) Web Mercator zoom

  Default value: `0`
* `--max-zoom <MAX_ZOOM>` — Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a sample of the input. Ignored with `--gsd`

  Default value: `6`
* `--gsd <GSDS>` — Explicit comma-separated GSD list in meters, strictly decreasing. Overrides `--min-zoom` and `--max-zoom`
* `--bbox <XMIN,YMIN,XMAX,YMAX>` — Convert only features whose bbox intersects this lon/lat box. Row groups outside it are skipped without reading their data
* `--cogp-compat` — Emit the optional COGP compatibility footer key (partitioning mode)
* `--report <PATH>` — Write the JSON conversion report to this path
* `-f`, `--force` — Overwrite the output if it exists
* `--verbatim` — Tile the input exactly as given, with every thinning, simplification, and density step off. Use it for pre-aggregated or pre-levelled input; knobs you set explicitly still win
* `--sort-key <COL>` — Column name used as the cell-winner priority (sort) key. Mutually exclusive with --class-rank
* `--magnitude-ladder <COL>` — Rank the distinct values of COL from high to low and let them set each feature's entry zoom, one `--ladder-step` apart from `--min-zoom`. Features appear from their entry zoom inward, exempt from thinning
* `--ladder-step <N>` — Zooms between consecutive `--magnitude-ladder` rungs

  Default value: `1`
* `--entry-zoom <SPEC>` — Place entry zooms by hand: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...`, for example `density:5000=4,1000=6`. Unlisted values take the ordinary visibility gate
* `--class-rank <SPEC>` — Rank categorical classes for cell winners: `COLUMN:VALUE=RANK,...`, where higher wins. Unlisted values rank below listed ones and above nulls
* `--no-auto-rank` — Disable automatic detection of well-known schemas (Overture roads `class`/`road_class`, Overture places `confidence`)
* `--filter <EXPR>` [alias: `where`] — Convert only features matching this SQL-WHERE predicate over the property columns, such as `confidence > 0.8`. Row groups that cannot match are skipped; the grammar is in the tuning guide: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--include-property <COL>` — Keep only these property columns (repeatable); the rest are never decoded. Columns that other flags read must stay included
* `--exclude-property <COL>` — Drop these property columns (repeatable). Ignored when `--include-property` is given
* `--exclude-all-properties` — Drop every property column, writing geometry only. Ignored when `--include-property` is given
* `--gsd-base <F>` — Base of the zoom-to-GSD mapping, `gsd(z) = 40075016.69 / base / 2^z`: a larger base keeps more detail at every level. No effect with `--gsd`

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance as a multiple of each level's GSD (default 1.0); lower keeps more vertices. Duplicating mode only
* `--collapse` — Collapse polygons too small for a level to a representative point instead of dropping them. Fill styles ignore points, so add a circle layer or use `--collapse-square`
* `--collapse-square` — Replace the polygons a coarse level drops with small placeholder squares, so the level still shows where the area is. The output stays Polygon; duplicating mode only
* `--representation <SPEC>` — Zoom bands that change how polygons render: comma-separated `LO-HI:KIND` with KIND `geom`, `point`, or `square`, for example `0-7:point,8-14:geom`. The band rules are in the tuning guide: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--no-cascade` — Simplify each level from the source instead of from the next finer level. Slower, and reproduces the pre-cascade output byte for byte
* `--point-thinning <POINT_THINNING>` — Point thinning grid cell, as a multiple of the level's GSD (default 4.0, or 16.0 with `--cluster`). Larger cells keep fewer points
* `--line-thinning <LINE_THINNING>` — Line thinning grid cell, as a multiple of the level's GSD (default 1.0). Larger cells keep fewer lines
* `--polygon-thinning <POLYGON_THINNING>` — Polygon thinning grid cell, as a multiple of the level's GSD (default 1.0). Larger cells keep fewer polygons
* `--line-visibility <LINE_VISIBILITY>` — Drop lines whose bbox diagonal is shorter than this many GSDs at a level (default 2.0)
* `--polygon-visibility <POLYGON_VISIBILITY>` — Drop polygons whose bbox diagonal is shorter than this many GSDs at a level (default 2.0)
* `--drop-rate <F>` — Density budget decay: each coarser level keeps 1/rate of the next finer level's feature budget. Larger values thin mid zooms harder

  Default value: `1.65`
* `--drop-gamma <F>` — How strongly the density budget protects sparse areas: 1 cuts every neighborhood equally, and larger values protect sparse ones more

  Default value: `1.5`
* `--no-density-drop` — Turn off the per-level density budget, leaving cell-winner thinning only
* `--cluster` — Merge each thinning cell's points into its surviving point, which gains a `point_count` column. Duplicating mode only
* `--accumulate-attribute <COL:OP>` — Aggregate a numeric column over each cluster as `COL:OP`, where OP is `sum`, `max`, `min`, or `mean` (repeatable). Requires `--cluster`
* `--no-coalesce-lines` — Turn off line coalescing, which joins touching same-class line segments into longer strokes at coarse levels
* `--coalesce-junction-angle <DEG>` — Continue a line through a junction when the straightest pair deviates by at most this many degrees; 0 stops chains at junctions

  Default value: `0.0`
* `--coalesce-snap <F>` — Join line ends within this many GSDs of each other when coalescing; 0 requires exact endpoint matches

  Default value: `1.0`
* `--coalesce-max-level-rows <ROWS>` — Skip coalescing on a level with more candidate lines than this, to bound memory. Long lines hit a matching geometry-size limit first

  Default value: `2000000`
* `--row-group-size <ROW_GROUP_SIZE>` — Maximum rows per output row group, applied per level. It is raised automatically if a file would pass Parquet's row-group limit

  Default value: `10000`
* `--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` — How the row-group cap varies by level: `constant`, or `zoom-scaled`, which doubles it per zoom step coarser than the finest level

  Default value: `constant`

  Possible values: `constant`, `zoom-scaled`

* `--full-column-stats` — Keep Parquet min/max statistics on every column, including large string and geometry columns. Use it when remote clients filter on property columns
* `--no-streaming` — Load the whole dataset into memory instead of streaming it in two passes. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>
* `--read-batch-size <ROWS>` — Rows per Arrow read batch. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `8192`
* `--profile <PROFILE>` — Memory profile for writing levels: `speed` buffers in RAM, `bounded` spills to disk, and `auto` picks per run. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches in flight at once; `auto` sizes it to the machine's cores. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `auto`
* `--read-workers <N|auto>` — Reader threads for the second pass; `auto` uses a quarter of the cores, up to 4. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for spill files: staged remote input, and on `tiles` the intermediate overview. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>
* `--save-plan <PATH>` — Write the convert plan to PATH and keep converting, so a later run can reuse it with `--plan`. A sharded build shares one plan
* `--plan <PATH>` — Reuse the convert plan at PATH and skip the first pass and level assignment. The plan's fingerprint must match this run's version, flags, and inputs



## `tylertoo validate`

Check a GeoParquet overview file against the overviews spec

**Usage:** `tylertoo validate <FILE>`

###### **Arguments:**

* `<FILE>` — GeoParquet overview file to validate



## `tylertoo export-pmtiles`

Export a PMTiles archive from an overview GeoParquet file

**Usage:** `tylertoo export-pmtiles [OPTIONS] <INPUT> <OUTPUT>`

###### **Arguments:**

* `<INPUT>` — Input overview GeoParquet file (produced by `tylertoo overview`)
* `<OUTPUT>` — Output PMTiles archive

###### **Options:**

* `--layer-name <LAYER_NAME>` — MVT layer name written into every tile

  Default value: `overview`
* `--min-zoom <ZOOM>` — Minimum zoom to declare in the archive metadata, even when the overview file's coarsest levels are empty. Unset uses the coarsest level's zoom
* `--include-property <NAME>` — Keep only these properties in the tiles (repeatable). The overview file is unchanged, and the `--feature-order` column must stay
* `--exclude-property <NAME>` — Drop these properties from the tiles (repeatable). Ignored when `--include-property` is given
* `--exclude-all-properties` — Drop every property, writing geometry-only tiles. Ignored when `--include-property` is given
* `--tile-buffer <TILE_BUFFER>` — Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width)

  Default value: `8`
* `--tile-size-limit <SIZE>` [alias: `max-tile-size`] — Per-tile MVT size cap, such as `500K`, `1M`, or a byte count; 0 disables it. A tile over the cap drops features in one pass until it fits

  Default value: `500K`
* `--report <PATH>` — Write the JSON export report, with per-zoom tile and feature counts, to this path
* `--no-simple-clip-fastpath` — Clip every polygon with the full overlay instead of the fast path for simple rings. Use it for byte-stable tiles: the fast path renders the same but can start a ring at a different vertex
* `--partition-wave <N|auto>` — Partitions processed at once during export; `auto` sizes it to the cores and available memory. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>

  Default value: `auto`
* `--feature-order <input|COLUMN[:asc|:desc]>` — Within-tile feature order: `input` keeps source row order, and a property name, optionally with `:asc` or `:desc`, sorts each tile by it. A renderer paints in this order unless a style overrides it

  Default value: `input`
* `--feature-id <COLUMN>` — Write this integer property column as the MVT feature id on every tile, so `setFeatureState` keys work across tiles and zooms. Every row must hold a value from 0 to 2^64-1; unset keeps tile-local ids
* `--tile-range <LO..HI>` — Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at one zoom, plus all their descendants. Archives cut by ranges that partition a zoom are disjoint, so `tylertoo merge` can join them
* `--zoom-ceiling <ZOOM>` — Emit only the tiles at or below this zoom, the coarse half of a sharded build. A partial overview kept from `tiles --shard coarse` needs a ceiling at or below its own
* `--spill-dir <PATH>` — Directory for the export's spill file, used when buffered tiles exceed the memory budget. See <https://geoparquet-io.github.io/tylertoo/guides/bounded-memory/>
* `-f`, `--force` — Overwrite the output if it exists



## `tylertoo decode`

Decode a PMTiles vector-tile archive back to GeoParquet

**Usage:** `tylertoo decode [OPTIONS] <INPUT> <OUTPUT>`

The output is the tiled representation, not the original source:
  - simplified: vertices were removed during tiling at lower zooms
    (extract the max zoom for best detail)
  - clipped: features are cut at (buffered) tile boundaries
  - duplicated: a feature appears once per neighboring tile and per
    zoom level; nothing is deduplicated (matches tippecanoe-decode) -
    filter with --zoom or the output's `zoom` column
  - lost properties: attributes dropped during tiling cannot be
    recovered
There is no round-trip guarantee: `A.parquet` -> `B.pmtiles` -> `C.parquet`
does not reproduce A.

Output columns, in order:
  - `zoom` (UInt8), `layer` (Utf8), `mvt_id` (UInt64, null when the
    encoder set no id): where each row came from
  - every property seen in any tile, alphabetical, and null for a
    feature that lacks it.
    Integers become Int64 and floats Float64. A key that mixes the two
    becomes Float64; any other mix becomes Utf8.
  - geometry: WKB in EPSG:4326, with a bbox covering
A source property named `zoom`, `layer`, `mvt_id`, or `geometry` is an error.

###### **Arguments:**

* `<INPUT>` — Input PMTiles archive (vector tiles)
* `<OUTPUT>` — Output GeoParquet file

###### **Options:**

* `--zoom <ZOOM>` — Decode a single zoom level (recommended for most uses)
* `--min-zoom <MIN_ZOOM>` — Minimum zoom level to decode
* `--max-zoom <MAX_ZOOM>` — Maximum zoom level to decode
* `--layer <NAME>` — Only decode features from this MVT layer
* `--report <PATH>` — Write the JSON decode report to this path
* `-f`, `--force` — Overwrite the output if it exists



## `tylertoo stats`

Report per-zoom tile sizes for a PMTiles archive, from its directory alone

**Usage:** `tylertoo stats [OPTIONS] <ARCHIVE>`

###### **Arguments:**

* `<ARCHIVE>` — PMTiles archive to report on

###### **Options:**

* `--largest <N>` — How many of the largest tiles (by stored size) to list

  Default value: `10`
* `--json` — Print the report as JSON instead of a human-readable table



## `tylertoo pyramid`

Build one archive from several inputs, each owning a zoom range

**Usage:** `tylertoo pyramid [OPTIONS] --band <LO-HI:INPUT[:LAYER]> <OUTPUT>`

###### **Arguments:**

* `<OUTPUT>` — Output PMTiles archive

###### **Options:**

* `--band <LO-HI:INPUT[:LAYER]>` — A band, `LO-HI:INPUT[:LAYER]` (repeatable): INPUT is a GeoParquet source or a PMTiles archive for zooms LO to HI, and LAYER defaults to the file stem. See the tuning guide for the colon rules and the `LO-HI=INPUT[=LAYER]` form: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--generalize` — Run the generalization ladder on each GeoParquet band instead of tiling it verbatim. Use it when a band holds raw features spanning several zooms
* `--max-tile-size <SIZE>` — Per-tile MVT size cap for bands tiled here, such as `500K`. Unset means no cap, so a band keeps every cell it exists to draw
* `--feature-order <input|COLUMN[:asc|:desc]>` — Within-tile feature order for GeoParquet bands: `input`, or a property name with optional `:asc` or `:desc`. A band without that column keeps input order with a warning

  Default value: `input`
* `--work-dir <DIR>` — Directory for the per-band intermediates, which are removed afterwards; the system temp directory if unset
* `--allow-missing-zooms` — Accept a pre-tiled band whose archive holds fewer zooms than the band declares. Those zooms render empty, so use it only for a deliberately sparse pyramid
* `-f`, `--force` — Overwrite the output if it exists



## `tylertoo merge`

Combine PMTiles archives that hold disjoint tiles into one

**Usage:** `tylertoo merge [OPTIONS] <OUTPUT> <INPUT>...`

###### **Arguments:**

* `<OUTPUT>` — Output PMTiles archive
* `<INPUT>` — Input PMTiles archives, two or more, with disjoint tile ids and the same tile type and compression. The output's bounds, zoom range, and `vector_layers` are the union of the inputs'

###### **Options:**

* `--work-dir <DIR>` — Directory for the spool file that holds merged tile data until the archive is assembled; the system temp directory if unset
* `--report <PATH>` — Write the JSON merge report, with per-zoom tile counts, to this path
* `-f`, `--force` — Overwrite the output if it exists



## `tylertoo shard-plan`

Cut a dataset's tile space into N disjoint shards for a sharded build

**Usage:** `tylertoo shard-plan [OPTIONS] --output <PATH> --shards <N> [INPUT]`

###### **Arguments:**

* `<INPUT>` — Input GeoParquet that every job of the fleet tiles, resolved as `tiles` resolves it. Omit it when `--files-from` is given

###### **Options:**

* `--files-from <PATH>` — Plan for the inputs listed in this manifest instead of INPUT, one path or URL per line in dataset row order. Give every job of the fleet the same manifest
* `-o`, `--output <PATH>` — Where to write the shard plan
* `--shards <N>` — How many data shards to cut. One `tiles --shard i/N` job per shard, plus one `--shard coarse` job; all N+1 archives merge in one step
* `--pivot <ZOOM>` — Zoom to cut at: shards own zooms from PIVOT to `--max-zoom`, and the coarse job owns the zooms below. Choose it so each shard holds a few tiles of data; z4 to z8 suits most fleets

  Default value: `6`
* `-f`, `--force` — Overwrite an existing plan at --output



