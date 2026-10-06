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

* `<INPUT>` — Input GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator (`EPSG:3857`). Pass a file, directory, glob, URL, or `s3://` or `gs://` prefix, or omit it with `--files-from`
* `<OUTPUT>` — Output PMTiles file. Omit it with `--plan-only`, which writes no archive

###### **Options:**

* `--files-from <PATH>` — Convert the `.parquet` files this manifest lists instead of `INPUT`. Give one path or URL per line, in dataset row order. Blank lines and `#` lines do nothing
* `--min-zoom <MIN_ZOOM>` — Minimum (coarsest) Web Mercator zoom

  Default value: `0`
* `--max-zoom <MAX_ZOOM>` — Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a sample of the input. Ignored with `--gsd`

  Default value: `14`
* `--gsd <GSDS>` — Comma-separated ground sample distances (GSDs) in meters, each smaller than the last. Overrides `--min-zoom` and `--max-zoom`
* `--bbox <XMIN,YMIN,XMAX,YMAX>` — Convert only features whose bbox intersects this lon/lat box. Row groups outside the box go unread
* `--layer-name <LAYER_NAME>` — Layer name for the tiles. Defaults to the input's file stem
* `--max-tile-size <SIZE>` [alias: `tile-size-limit`] — Per-tile MVT size cap, such as `500K` or `1M`, or 0 for no cap (default 500K, or no cap with `--verbatim`). A tile over the cap drops features until it fits
* `--no-simple-clip-fastpath` — Clip every polygon with the full overlay instead of the fast path for simple rings. Use it for byte-stable tiles: the fast path renders the same but can start a ring at a different vertex
* `--tile-buffer <TILE_BUFFER>` — Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width)

  Default value: `8`
* `--partition-wave <N|auto>` — Partitions to export at once, or `auto` to size the wave to the cores and free memory. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>

  Default value: `auto`
* `--feature-order <input|COLUMN[:asc|:desc]>` — Within-tile feature order: `input` keeps source row order, and a property name, optionally with `:asc` or `:desc`, sorts each tile by it. A renderer paints in this order unless a style overrides it

  Default value: `input`
* `--feature-id <COLUMN>` — Write this integer column as each feature's MVT id, so `setFeatureState` keys work across tiles and zooms. Every row must hold a value from 0 to 2^64-1. Without it, tiles keep tile-local ids
* `--report <PATH>` — Write a JSON report with `convert` and `export` sections, matching the reports of `overview` and `export-pmtiles`
* `--keep-overview <PATH>` — Keep the intermediate overview GeoParquet at PATH instead of deleting it after the export. The PMTiles output is the same
* `--shard <I/N|coarse>` — Build one job of a sharded fleet: `I/N` for data shard I of N, or `coarse` for the zooms below the pivot. Requires `--shard-plan`, and data shards also need `--plan`. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#sharded-builds>
* `--shard-plan <PATH>` — The shard plan from `tylertoo shard-plan` that every job shares
* `--tile-range <LO..HI>` — Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at one zoom, plus their descendants. Prefer `--shard`, which also skips input the range cannot reach
* `--plan-only` — Write the convert plan (`--save-plan`) and stop, with no export and no OUTPUT. Use it for a coarse job whose tiles the fleet discards
* `-v`, `--verbose` — Print per-level and per-zoom breakdowns
* `-f`, `--force` — Overwrite the output if it exists
* `--verbatim` — Tile the input exactly as given, with every thinning, simplification, and density step off. Use it for pre-aggregated or pre-levelled input. Knobs you set yourself still win
* `--sort-key <COL>` — Numeric column that decides which feature wins each thinning cell. Conflicts with `--class-rank`
* `--magnitude-ladder <COL>` — Rank the values of `COL` from high to low to set each feature's entry zoom, one `--ladder-step` apart from `--min-zoom`. Features appear from their entry zoom inward, exempt from thinning
* `--ladder-step <N>` — Zooms between consecutive `--magnitude-ladder` rungs

  Default value: `1`
* `--entry-zoom <SPEC>` — Place entry zooms by hand: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...`, for example `density:5000=4,1000=6`. Unlisted values take the ordinary visibility gate
* `--class-rank <SPEC>` — Rank categorical classes for cell winners: `COLUMN:VALUE=RANK,...`, where higher wins. Unlisted values rank below listed ones and above nulls
* `--no-auto-rank` — Turn off automatic ranking for known schemas (Overture roads `class`/`road_class`, Overture places `confidence`)
* `--filter <EXPR>` [alias: `where`] — Convert only features matching this SQL `WHERE` predicate over the property columns, such as `confidence > 0.8`. Row groups that cannot match go unread, and the tuning guide has the grammar: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--include-property <COL>` — Keep only these property columns (repeatable) and skip decoding the rest. Columns that other flags read must stay in the list
* `--exclude-property <COL>` — Drop these property columns (repeatable). `--include-property` overrides it
* `--exclude-all-properties` — Drop every property column, writing geometry only. `--include-property` overrides it
* `--gsd-base <F>` — Base of the zoom-to-GSD mapping, `gsd(z) = 40075016.69 / base / 2^z`: a larger base keeps more detail at every level. No effect with `--gsd`

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance in multiples of each level's GSD (default 1.0, duplicating mode only). Lower values keep more vertices
* `--collapse` — Collapse polygons too small for a level to a single point instead of dropping them. Fill styles ignore points, so add a circle layer or use `--collapse-square`
* `--collapse-square` — Replace the polygons a coarse level drops with small placeholder squares, so the level still shows where the area is. Duplicating mode only, and the output stays Polygon
* `--representation <SPEC>` — Zoom bands that change how polygons render: comma-separated `LO-HI:KIND`, where `KIND` is `geom`, `point`, or `square`, such as `0-7:point,8-14:geom`. The band rules are in the tuning guide: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--no-cascade` — Simplify each level from the source instead of from the next finer level. Slower, but each level stays within its own tolerance of the source
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
* `--accumulate-attribute <COL:OP>` — Aggregate a numeric column over each cluster as `COL:OP`, where `OP` is `sum`, `max`, `min`, or `mean` (repeatable). Requires `--cluster`
* `--no-coalesce-lines` — Turn off line coalescing, which joins touching same-class line segments into longer strokes at coarse levels
* `--coalesce-junction-angle <DEG>` — Continue a line through a junction when the straightest pair turns by at most this many degrees. Set 0 to stop chains at every junction

  Default value: `0.0`
* `--coalesce-snap <F>` — Join line ends within this many GSDs of each other when coalescing. Set 0 to join only ends that match exactly

  Default value: `1.0`
* `--coalesce-max-level-rows <ROWS>` — Skip coalescing on a level with more candidate lines than this, to bound memory. Long lines hit a matching geometry-size limit first

  Default value: `2000000`
* `--row-group-size <ROW_GROUP_SIZE>` — Maximum rows per output row group, per level. A level that would pass Parquet's row-group limit gets a larger cap

  Default value: `10000`
* `--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` — How the row-group cap varies by level: `constant`, or `zoom-scaled`, which doubles it per zoom step coarser than the finest level

  Default value: `constant`

  Possible values: `constant`, `zoom-scaled`

* `--full-column-stats` — Keep Parquet min/max stats on every column, even large string and geometry columns. Use it when remote clients filter on property columns
* `--no-streaming` — Load the whole dataset into memory instead of streaming it in two passes. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>
* `--read-batch-size <ROWS>` — Rows per Arrow read batch. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>

  Default value: `8192`
* `--profile <PROFILE>` — Memory profile for writing levels: `speed` buffers in RAM, `bounded` spills to disk, and `auto` picks per run. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles>

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches in flight at once, or `auto` to size it to the cores. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>

  Default value: `auto`
* `--read-workers <N|auto>` — Reader threads for the second pass, or `auto` for a quarter of the cores, up to 4. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for spill files: staged remote input, and on `tiles` the intermediate overview. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>
* `--save-plan <PATH>` — Write the convert plan to PATH and keep converting, so a later run can reuse it with `--plan`. A sharded build shares one plan
* `--plan <PATH>` — Reuse the convert plan at PATH and skip the first pass and level assignment. The plan's fingerprint must match this run's version, flags, and inputs



## `tylertoo overview`

Build a multi-resolution overview GeoParquet file

**Usage:** `tylertoo overview [OPTIONS] [INPUT] [OUTPUT]`

###### **Arguments:**

* `<INPUT>` — Input GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator (`EPSG:3857`). Pass a file, directory, glob, URL, or `s3://` or `gs://` prefix, or omit it with `--files-from`
* `<OUTPUT>` — Output overview GeoParquet file

###### **Options:**

* `--files-from <PATH>` — Convert the `.parquet` files this manifest lists instead of `INPUT`. Give one path or URL per line, in dataset row order. Blank lines and `#` lines do nothing
* `--mode <MODE>` — Level layout: `duplicating` writes each level in full, and `partitioning` writes each feature once, at its coarsest level

  Default value: `duplicating`

  Possible values: `duplicating`, `partitioning`

* `--min-zoom <MIN_ZOOM>` — Minimum (coarsest) Web Mercator zoom

  Default value: `0`
* `--max-zoom <MAX_ZOOM>` — Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a sample of the input. Ignored with `--gsd`

  Default value: `6`
* `--gsd <GSDS>` — Comma-separated ground sample distances (GSDs) in meters, each smaller than the last. Overrides `--min-zoom` and `--max-zoom`
* `--bbox <XMIN,YMIN,XMAX,YMAX>` — Convert only features whose bbox intersects this lon/lat box. Row groups outside the box go unread
* `--cogp-compat` — Also write the third-party `cogp` footer key, for readers of that overview format. Partitioning mode only
* `--report <PATH>` — Write the JSON conversion report to this path
* `-f`, `--force` — Overwrite the output if it exists
* `--verbatim` — Tile the input exactly as given, with every thinning, simplification, and density step off. Use it for pre-aggregated or pre-levelled input. Knobs you set yourself still win
* `--sort-key <COL>` — Numeric column that decides which feature wins each thinning cell. Conflicts with `--class-rank`
* `--magnitude-ladder <COL>` — Rank the values of `COL` from high to low to set each feature's entry zoom, one `--ladder-step` apart from `--min-zoom`. Features appear from their entry zoom inward, exempt from thinning
* `--ladder-step <N>` — Zooms between consecutive `--magnitude-ladder` rungs

  Default value: `1`
* `--entry-zoom <SPEC>` — Place entry zooms by hand: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...`, for example `density:5000=4,1000=6`. Unlisted values take the ordinary visibility gate
* `--class-rank <SPEC>` — Rank categorical classes for cell winners: `COLUMN:VALUE=RANK,...`, where higher wins. Unlisted values rank below listed ones and above nulls
* `--no-auto-rank` — Turn off automatic ranking for known schemas (Overture roads `class`/`road_class`, Overture places `confidence`)
* `--filter <EXPR>` [alias: `where`] — Convert only features matching this SQL `WHERE` predicate over the property columns, such as `confidence > 0.8`. Row groups that cannot match go unread, and the tuning guide has the grammar: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--include-property <COL>` — Keep only these property columns (repeatable) and skip decoding the rest. Columns that other flags read must stay in the list
* `--exclude-property <COL>` — Drop these property columns (repeatable). `--include-property` overrides it
* `--exclude-all-properties` — Drop every property column, writing geometry only. `--include-property` overrides it
* `--gsd-base <F>` — Base of the zoom-to-GSD mapping, `gsd(z) = 40075016.69 / base / 2^z`: a larger base keeps more detail at every level. No effect with `--gsd`

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance in multiples of each level's GSD (default 1.0, duplicating mode only). Lower values keep more vertices
* `--collapse` — Collapse polygons too small for a level to a single point instead of dropping them. Fill styles ignore points, so add a circle layer or use `--collapse-square`
* `--collapse-square` — Replace the polygons a coarse level drops with small placeholder squares, so the level still shows where the area is. Duplicating mode only, and the output stays Polygon
* `--representation <SPEC>` — Zoom bands that change how polygons render: comma-separated `LO-HI:KIND`, where `KIND` is `geom`, `point`, or `square`, such as `0-7:point,8-14:geom`. The band rules are in the tuning guide: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--no-cascade` — Simplify each level from the source instead of from the next finer level. Slower, but each level stays within its own tolerance of the source
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
* `--accumulate-attribute <COL:OP>` — Aggregate a numeric column over each cluster as `COL:OP`, where `OP` is `sum`, `max`, `min`, or `mean` (repeatable). Requires `--cluster`
* `--no-coalesce-lines` — Turn off line coalescing, which joins touching same-class line segments into longer strokes at coarse levels
* `--coalesce-junction-angle <DEG>` — Continue a line through a junction when the straightest pair turns by at most this many degrees. Set 0 to stop chains at every junction

  Default value: `0.0`
* `--coalesce-snap <F>` — Join line ends within this many GSDs of each other when coalescing. Set 0 to join only ends that match exactly

  Default value: `1.0`
* `--coalesce-max-level-rows <ROWS>` — Skip coalescing on a level with more candidate lines than this, to bound memory. Long lines hit a matching geometry-size limit first

  Default value: `2000000`
* `--row-group-size <ROW_GROUP_SIZE>` — Maximum rows per output row group, per level. A level that would pass Parquet's row-group limit gets a larger cap

  Default value: `10000`
* `--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` — How the row-group cap varies by level: `constant`, or `zoom-scaled`, which doubles it per zoom step coarser than the finest level

  Default value: `constant`

  Possible values: `constant`, `zoom-scaled`

* `--full-column-stats` — Keep Parquet min/max stats on every column, even large string and geometry columns. Use it when remote clients filter on property columns
* `--no-streaming` — Load the whole dataset into memory instead of streaming it in two passes. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>
* `--read-batch-size <ROWS>` — Rows per Arrow read batch. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>

  Default value: `8192`
* `--profile <PROFILE>` — Memory profile for writing levels: `speed` buffers in RAM, `bounded` spills to disk, and `auto` picks per run. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles>

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches in flight at once, or `auto` to size it to the cores. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>

  Default value: `auto`
* `--read-workers <N|auto>` — Reader threads for the second pass, or `auto` for a quarter of the cores, up to 4. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for spill files: staged remote input, and on `tiles` the intermediate overview. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>
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

* `<INPUT>` — Input overview GeoParquet file from `tylertoo overview`
* `<OUTPUT>` — Output PMTiles archive

###### **Options:**

* `--layer-name <LAYER_NAME>` — MVT layer name written into every tile

  Default value: `overview`
* `--min-zoom <ZOOM>` — Minimum zoom to declare in the archive metadata, even when the overview file's coarsest levels are empty. Unset uses the coarsest level's zoom
* `--include-property <NAME>` — Keep only these properties in the tiles (repeatable). The overview file keeps every column, and the `--feature-order` column must stay
* `--exclude-property <NAME>` — Drop these properties from the tiles (repeatable). `--include-property` overrides it
* `--exclude-all-properties` — Drop every property, writing geometry-only tiles. `--include-property` overrides it
* `--tile-buffer <TILE_BUFFER>` — Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width)

  Default value: `8`
* `--tile-size-limit <SIZE>` [alias: `max-tile-size`] — Per-tile MVT size cap, such as `500K`, `1M`, or a byte count, or 0 for no cap. A tile over the cap drops features until it fits

  Default value: `500K`
* `--report <PATH>` — Write the JSON export report, with per-zoom tile and feature counts, to this path
* `--no-simple-clip-fastpath` — Clip every polygon with the full overlay instead of the fast path for simple rings. Use it for byte-stable tiles: the fast path renders the same but can start a ring at a different vertex
* `--partition-wave <N|auto>` — Partitions to export at once, or `auto` to size the wave to the cores and free memory. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>

  Default value: `auto`
* `--feature-order <input|COLUMN[:asc|:desc]>` — Within-tile feature order: `input` keeps source row order, and a property name, optionally with `:asc` or `:desc`, sorts each tile by it. A renderer paints in this order unless a style overrides it

  Default value: `input`
* `--feature-id <COLUMN>` — Write this integer column as each feature's MVT id, so `setFeatureState` keys work across tiles and zooms. Every row must hold a value from 0 to 2^64-1. Without it, tiles keep tile-local ids
* `--tile-range <LO..HI>` — Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at one zoom, plus their descendants. Ranges that partition a zoom give disjoint archives, which `tylertoo merge` can join
* `--zoom-ceiling <ZOOM>` — Emit only the tiles at or below this zoom, the coarse half of a sharded build. A partial overview kept from `tiles --shard coarse` needs a ceiling at or below its own
* `--spill-dir <PATH>` — Directory for the export's spill file, used when buffered tiles exceed the memory budget. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>
* `-f`, `--force` — Overwrite the output if it exists



## `tylertoo decode`

Decode a PMTiles vector-tile archive back to GeoParquet

**Usage:** `tylertoo decode [OPTIONS] <INPUT> <OUTPUT>`

The output holds the tiles' features, not the original source:
  - simplified: tiling drops vertices at lower zooms. Decode the max
    zoom for the most detail.
  - clipped: tiling cuts features at the buffered tile edges.
  - duplicated: a feature appears once per tile it touches and once per
    zoom. Filter with --zoom or the `zoom` column.
  - lost properties: attributes that tiling dropped do not come back.
Decoding does not restore the input: `A.parquet` -> `B.pmtiles` ->
`C.parquet` gives a C that differs from A.

Output columns, in order:
  - `zoom` (UInt8), `layer` (Utf8), `mvt_id` (UInt64, null when the
    encoder set no id): where each row came from.
  - every property seen in any tile, alphabetical, and null for a
    feature that lacks it. Integers become Int64 and floats Float64.
    A key that mixes the two becomes Float64, and any other mix Utf8.
  - geometry: Well-Known Binary (WKB) in lon/lat (`EPSG:4326`), with a
    bbox covering.
A source property named `zoom`, `layer`, `mvt_id`, or `geometry` is an error.

###### **Arguments:**

* `<INPUT>` — Input PMTiles archive (vector tiles)
* `<OUTPUT>` — Output GeoParquet file

###### **Options:**

* `--zoom <ZOOM>` — Decode one zoom level, which suits most uses
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

* `--band <LO-HI:INPUT[:LAYER]>` — One zoom band as `LO-HI:INPUT[:LAYER]` (repeatable). `INPUT` is a GeoParquet source or PMTiles archive for zooms `LO` to `HI`, and `LAYER` defaults to its file stem. The tuning guide covers the colon rules and the `LO-HI=INPUT[=LAYER]` form: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>
* `--generalize` — Run the generalization ladder on each GeoParquet band instead of tiling it verbatim. Use it when a band holds raw features spanning several zooms
* `--max-tile-size <SIZE>` — Per-tile MVT size cap for bands tiled here, such as `500K`. Unset means no cap, so a band keeps every cell it exists to draw
* `--feature-order <input|COLUMN[:asc|:desc]>` — Within-tile feature order for GeoParquet bands: `input`, or a property name with optional `:asc` or `:desc`. A band without that column keeps input order with a warning

  Default value: `input`
* `--work-dir <DIR>` — Directory for the per-band intermediate files, which tylertoo deletes afterwards. Defaults to the system temp directory
* `--allow-missing-zooms` — Accept a pre-tiled band whose archive holds fewer zooms than the band declares. Those zooms render empty, so use it only when you want a sparse pyramid
* `-f`, `--force` — Overwrite the output if it exists



## `tylertoo merge`

Combine PMTiles archives that hold disjoint tiles into one

**Usage:** `tylertoo merge [OPTIONS] <OUTPUT> <INPUT>...`

###### **Arguments:**

* `<OUTPUT>` — Output PMTiles archive
* `<INPUT>` — Two or more input PMTiles archives with disjoint tile ids and one tile type and compression. The output's bounds, zoom range, and `vector_layers` are the union of the inputs'

###### **Options:**

* `--work-dir <DIR>` — Directory for the spool file that holds tile data until the archive is complete. Defaults to the system temp directory
* `--report <PATH>` — Write the JSON merge report, with per-zoom tile counts, to this path
* `-f`, `--force` — Overwrite the output if it exists



## `tylertoo shard-plan`

Cut a dataset's tile space into N disjoint shards for a sharded build

**Usage:** `tylertoo shard-plan [OPTIONS] --output <PATH> --shards <N> [INPUT]`

###### **Arguments:**

* `<INPUT>` — Input GeoParquet that every job in the fleet tiles, in any form `tiles` accepts. Omit it with `--files-from`

###### **Options:**

* `--files-from <PATH>` — Plan for the inputs this manifest lists, one path or URL per line in dataset row order, instead of `INPUT`. Give every job in the fleet the same manifest
* `-o`, `--output <PATH>` — Where to write the shard plan
* `--shards <N>` — How many data shards to cut. Run one `tiles --shard i/N` job per shard plus one `--shard coarse` job, then merge all N+1 archives
* `--pivot <ZOOM>` — Zoom to cut at. Shards own the zooms from here to `--max-zoom`, and the coarse job owns the zooms below. Pick a zoom where each shard holds a few tiles of data, usually z4 to z8

  Default value: `6`
* `-f`, `--force` — Overwrite an existing plan at --output



