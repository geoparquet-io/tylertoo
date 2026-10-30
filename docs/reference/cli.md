<!-- GENERATED FILE — do not edit by hand.
     Regenerate: cargo run -p tylertoo --features gen-docs -- gen-reference-docs > docs/reference/cli.md
     CI fails if this file drifts from the clap definitions. -->

# CLI reference

## `tylertoo`

Convert GeoParquet to PMTiles vector tiles and multi-resolution overviews.

`tylertoo INPUT OUTPUT` with no subcommand runs `tiles`.

```text
tylertoo [OPTIONS] <COMMAND>
```

Exit status:

- 0: success.
- 1: the run failed, with the error on stderr. `validate` also exits 1 when a check fails.
- 2: the command line was rejected: while it was parsed, or by an option or output-path rule checked before any input is read.
- 101: a panic, from an internal bug or an output pipe closed early.

### Subcommands

| Command | Purpose |
| --- | --- |
| [`tiles`](#tylertoo-tiles) | Generate PMTiles vector tiles from GeoParquet (the default command) |
| [`overview`](#tylertoo-overview) | Build a multi-resolution overview GeoParquet file |
| [`validate`](#tylertoo-validate) | Check a GeoParquet overview file against the overviews spec |
| [`export-pmtiles`](#tylertoo-export-pmtiles) | Export a PMTiles archive from an overview GeoParquet file |
| [`decode`](#tylertoo-decode) | Decode a PMTiles vector-tile archive back to GeoParquet |
| [`stats`](#tylertoo-stats) | Report per-zoom tile sizes for a PMTiles archive, from its directory alone |
| [`pyramid`](#tylertoo-pyramid) | Build one archive from several inputs, each owning a zoom range |
| [`merge`](#tylertoo-merge) | Combine PMTiles archives that hold disjoint tiles into one |
| [`shard-plan`](#tylertoo-shard-plan) | Cut a dataset's tile space into N disjoint shards for a sharded build |

### Global options

| Option | Description |
| --- | --- |
| `-q`, `--quiet` | Print only warnings and errors. The output of `stats` and `validate` still prints. |

These go before or after the subcommand.

## `tylertoo tiles`

Generate PMTiles vector tiles from GeoParquet (the default command)

```text
tylertoo tiles [OPTIONS] [INPUT] [OUTPUT]
```

### Arguments

| Argument | Description |
| --- | --- |
| `<INPUT>` | Input GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator (`EPSG:3857`). Pass a file, directory, glob, URL, or `s3://` or `gs://` prefix, or omit it with `--files-from`. |
| `<OUTPUT>` | Output PMTiles file. Omit it with `--plan-only`, which writes no archive. |

### Options

| Option | Description |
| --- | --- |
| `--files-from <PATH>` | Read `.parquet` files from this manifest instead of `INPUT`: one path or URL per line, in dataset row order. Ignores blank lines and lines starting with `#`. |
| `--min-zoom <MIN_ZOOM>` | Minimum (coarsest) Web Mercator zoom. Default: `0`. |
| `--max-zoom <MAX_ZOOM>` | Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a sample of the input. Ignored with `--gsd`. Default: `14`. |
| `--gsd <GSDS>` | Comma-separated ground sample distances (GSDs) in meters, each smaller than the last. Overrides `--min-zoom` and `--max-zoom`. |
| `--name <TEXT>` | Human-readable name stored in the archive metadata. |
| `--description <TEXT>` | Description stored in the archive metadata. |
| `--attribution <TEXT>` | Attribution text or HTML stored in the archive metadata. |
| `--tile-buffer <TILE_BUFFER>` | Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width). Default: `8`. |
| `--extent <EXTENT>` | MVT tile extent: the tile-local coordinate resolution. A power of two is recommended. Default: `4096`. |
| `--max-tile-size <SIZE\|auto>` [alias: `tile-size-limit`] | Per-tile cap on the raw MVT size, such as `2M`, or 0 for no cap: a backstop behind `--max-compressed-tile-size`. `auto` is 2M, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile over a cap is merged or thinned until it fits; see `--size-valve`. Default: `auto`. |
| `--max-compressed-tile-size <SIZE\|auto>` | Per-tile cap on the gzip-compressed tile size, like tippecanoe's `-M`, such as `500K`, or 0 for no cap. `auto` is 500K, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile must fit both caps. Default: `auto`. |
| `--size-valve <merge\|drop>` | How to reduce a tile that exceeds its cap: `merge` merges the densest lines and polygons into a neighbor, which keeps its own attributes; `drop` removes features, leaving holes while preserving each retained feature's attributes. Point tiles use feature dropping in both modes. Default: `merge`. |
| `--coverage-fill` | Fill the holes that dense small polygons (parcels, fields, small admin units) leave at coarse zooms: under each tile's features, also draw the area its child tiles cover, with the attributes of one feature it sits under, in the room the tile's own features leave. Adds 10–17% to build time on dense coverages, about 1.5x on scattered sub-pixel fields (which it may also over-draw), and holds the finest level's geometry in memory. A sharded build's coarse job draws it; data shards (`--tile-range`) do not. |
| `--no-simple-clip-fastpath` | Clip all polygons with the full overlay for byte-stable tiles. The simple-ring fast path renders identically but can change a ring's starting vertex. |
| `--partition-wave <N\|auto>` | Partitions to export at once, or `auto` to size the wave to the cores and free memory. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>. Default: `auto`. |
| `--feature-order <input\|COLUMN[:asc\|:desc]>` | Feature order within each tile: `input` preserves source row order; `COLUMN[:asc\|:desc]` sorts by a property. Most renderers paint in this order unless the style overrides it. A `pyramid` band without that column keeps input order with a warning. Default: `input`. |
| `--feature-id <COLUMN>` | Use this integer column as the MVT feature id for `setFeatureState` across tiles and zooms. Every row must contain 0 to 2^64-1. Defaults to tile-local ids. |
| `--report <PATH>` | Write a JSON report with `convert` and `export` sections, matching the reports of `overview` and `export-pmtiles`. |
| `-v`, `--verbose` | Print per-level and per-zoom breakdowns. |
| `-f`, `--force` | Overwrite the output if it exists. |
| `--layer-name <LAYER_NAME>` | Layer name for the tiles. Defaults to the input's file stem. |
| `--keep-overview <PATH>` | Save the intermediate overview GeoParquet to PATH after export. Does not change the PMTiles output. |

### Sharded builds

| Option | Description |
| --- | --- |
| `--shard <I/N\|coarse>` | Build one shard: `I/N` for data shard I of N, or `coarse` for the zooms below the pivot. Requires `--shard-plan`, and data shards also need `--plan`. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#sharded-builds>. |
| `--shard-plan <PATH>` | The shard plan from `tylertoo shard-plan` that every job shares. |
| `--tile-range <LO..HI>` | Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at one zoom, plus their descendants. Prefer `--shard`, which also skips input the range cannot reach. |
| `--plan-only` | Write the convert plan (`--save-plan`) and stop, with no export and no OUTPUT. Use it for a coarse job whose tiles the fleet discards. |

Also accepts all [shared conversion options](#shared-conversion-options).

## `tylertoo overview`

Build a multi-resolution overview GeoParquet file

```text
tylertoo overview [OPTIONS] [INPUT] [OUTPUT]
```

### Arguments

| Argument | Description |
| --- | --- |
| `<INPUT>` | Input GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator (`EPSG:3857`). Pass a file, directory, glob, URL, or `s3://` or `gs://` prefix, or omit it with `--files-from`. |
| `<OUTPUT>` | Output overview GeoParquet file. |

### Options

| Option | Description |
| --- | --- |
| `--files-from <PATH>` | Read `.parquet` files from this manifest instead of `INPUT`: one path or URL per line, in dataset row order. Ignores blank lines and lines starting with `#`. |
| `--min-zoom <MIN_ZOOM>` | Minimum (coarsest) Web Mercator zoom. Default: `0`. |
| `--max-zoom <MAX_ZOOM>` | Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a sample of the input. Ignored with `--gsd`. Default: `6`. |
| `--gsd <GSDS>` | Comma-separated ground sample distances (GSDs) in meters, each smaller than the last. Overrides `--min-zoom` and `--max-zoom`. |
| `--report <PATH>` | Write the JSON conversion report to this path. |
| `-f`, `--force` | Overwrite the output if it exists. |
| `--mode <MODE>` | Level layout: `duplicating` writes each level in full, and `partitioning` writes each feature once, at its coarsest level. Default: `duplicating`. Possible values: `duplicating`, `partitioning`. |
| `--cogp-compat` | Write the third-party `cogp` footer key for readers of that overview format. Partitioning mode only. |

Also accepts all [shared conversion options](#shared-conversion-options).

## `tylertoo validate`

Check a GeoParquet overview file against the overviews spec

```text
tylertoo validate <FILE>
```

### Arguments

| Argument | Description |
| --- | --- |
| `<FILE>` | GeoParquet overview file to validate. |

## `tylertoo export-pmtiles`

Export a PMTiles archive from an overview GeoParquet file

```text
tylertoo export-pmtiles [OPTIONS] <INPUT> <OUTPUT>
```

### Arguments

| Argument | Description |
| --- | --- |
| `<INPUT>` | Input overview GeoParquet file from `tylertoo overview`. |
| `<OUTPUT>` | Output PMTiles archive. |

### Options

| Option | Description |
| --- | --- |
| `--name <TEXT>` | Human-readable name stored in the archive metadata. |
| `--description <TEXT>` | Description stored in the archive metadata. |
| `--attribution <TEXT>` | Attribution text or HTML stored in the archive metadata. |
| `--include-property <NAME>` | Keep only these properties in the tiles (repeatable). The input keeps every column, and the `--feature-order` column must stay. |
| `--exclude-property <NAME>` | Drop these properties from the tiles (repeatable). `--include-property` overrides it. |
| `--exclude-all-properties` | Drop every property, writing geometry-only tiles. `--include-property` overrides it. |
| `--layer-name <LAYER_NAME>` | MVT layer name written into every tile. Default: `overview`. |
| `--tile-buffer <TILE_BUFFER>` | Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width). Default: `8`. |
| `--extent <EXTENT>` | MVT tile extent: the tile-local coordinate resolution. A power of two is recommended. Default: `4096`. |
| `--max-tile-size <SIZE\|auto>` [alias: `tile-size-limit`] | Per-tile cap on the raw MVT size, such as `2M`, or 0 for no cap: a backstop behind `--max-compressed-tile-size`. `auto` is 2M, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile over a cap is merged or thinned until it fits; see `--size-valve`. Default: `auto`. |
| `--max-compressed-tile-size <SIZE\|auto>` | Per-tile cap on the gzip-compressed tile size, like tippecanoe's `-M`, such as `500K`, or 0 for no cap. `auto` is 500K, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile must fit both caps. Default: `auto`. |
| `--size-valve <merge\|drop>` | How to reduce a tile that exceeds its cap: `merge` merges the densest lines and polygons into a neighbor, which keeps its own attributes; `drop` removes features, leaving holes while preserving each retained feature's attributes. Point tiles use feature dropping in both modes. Default: `merge`. |
| `--coverage-fill` | Fill the holes that dense small polygons (parcels, fields, small admin units) leave at coarse zooms: under each tile's features, also draw the area its child tiles cover, with the attributes of one feature it sits under, in the room the tile's own features leave. Adds 10–17% to build time on dense coverages, about 1.5x on scattered sub-pixel fields (which it may also over-draw), and holds the finest level's geometry in memory. A sharded build's coarse job draws it; data shards (`--tile-range`) do not. |
| `--no-simple-clip-fastpath` | Clip all polygons with the full overlay for byte-stable tiles. The simple-ring fast path renders identically but can change a ring's starting vertex. |
| `--partition-wave <N\|auto>` | Partitions to export at once, or `auto` to size the wave to the cores and free memory. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>. Default: `auto`. |
| `--feature-order <input\|COLUMN[:asc\|:desc]>` | Feature order within each tile: `input` preserves source row order; `COLUMN[:asc\|:desc]` sorts by a property. Most renderers paint in this order unless the style overrides it. A `pyramid` band without that column keeps input order with a warning. Default: `input`. |
| `--min-zoom <ZOOM>` | Minimum zoom in archive metadata, including empty coarse overview levels. Defaults to the coarsest level's zoom. |
| `--tile-range <LO..HI>` | Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at one zoom, plus their descendants. Ranges that partition a zoom give disjoint archives, which `tylertoo merge` can join. |
| `--zoom-ceiling <ZOOM>` | Emit only the tiles at or below this zoom, the coarse half of a sharded build. A partial overview kept from `tiles --shard coarse` needs a ceiling at or below its own. |
| `--feature-id <COLUMN>` | Use this integer column as the MVT feature id for `setFeatureState` across tiles and zooms. Every row must contain 0 to 2^64-1. Defaults to tile-local ids. |
| `--report <PATH>` | Write the JSON export report, with per-zoom tile and feature counts, to this path. |
| `-f`, `--force` | Overwrite the output if it exists. |

### Memory & performance

| Option | Description |
| --- | --- |
| `--spill-dir <PATH>` | Directory for the export's spill file, used when buffered tiles exceed the memory budget. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>. |
| `--profile <PROFILE>` | Memory profile: `speed` holds everything in RAM, `bounded` spills to disk and holds the least, and `auto` sizes every buffer to the machine and spills when it must. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles>. Default: `auto`. Possible values: `auto`, `speed`, `bounded`. |

## `tylertoo decode`

Decode a PMTiles vector-tile archive back to GeoParquet

```text
tylertoo decode [OPTIONS] <INPUT> <OUTPUT>
```

Decoding reconstructs tile features. A round trip from `A.parquet` to
`B.pmtiles` to `C.parquet` changes the data:

- Tiling simplifies vertices at lower zooms. Decode the maximum zoom
  for the most detail.
- Features are clipped at buffered tile edges.
- Features repeat for each tile they touch and each zoom. Filter with
  `--zoom` or the `zoom` column.
- Properties dropped during tiling cannot be recovered.

Output columns, in order:

1. `zoom` (UInt8), `layer` (Utf8), and `mvt_id` (UInt64; null if the
   encoder set no id) identify each row's origin.
2. All properties found in any tile, alphabetically ordered and null
   where absent. Integers become Int64; floats become Float64. Mixed
   integers and floats become Float64; other mixed types become Utf8.
3. `geometry`: Well-Known Binary (WKB) in lon/lat (`EPSG:4326`), with a
   bbox covering.

A source property named `zoom`, `layer`, `mvt_id`, or `geometry` is an error.

### Arguments

| Argument | Description |
| --- | --- |
| `<INPUT>` | Input PMTiles archive (vector tiles). |
| `<OUTPUT>` | Output GeoParquet file. |

### Options

| Option | Description |
| --- | --- |
| `--zoom <ZOOM>` | Decode one zoom level, which suits most uses. |
| `--min-zoom <MIN_ZOOM>` | Minimum zoom level to decode. |
| `--max-zoom <MAX_ZOOM>` | Maximum zoom level to decode. |
| `--layer <NAME>` | Only decode features from this MVT layer. |
| `--report <PATH>` | Write the JSON decode report to this path. |
| `-f`, `--force` | Overwrite the output if it exists. |

## `tylertoo stats`

Report per-zoom tile sizes for a PMTiles archive, from its directory alone

```text
tylertoo stats [OPTIONS] <ARCHIVE>
```

### Arguments

| Argument | Description |
| --- | --- |
| `<ARCHIVE>` | PMTiles archive to report on. |

### Options

| Option | Description |
| --- | --- |
| `--largest <N>` | Number of largest tiles to list, ranked by stored size. Default: `10`. |
| `--json` | Print the report as JSON instead of a human-readable table. |

## `tylertoo pyramid`

Build one archive from several inputs, each owning a zoom range

```text
tylertoo pyramid [OPTIONS] --band <LO-HI:INPUT[:LAYER]> <OUTPUT>
```

### Arguments

| Argument | Description |
| --- | --- |
| `<OUTPUT>` | Output PMTiles archive. |

### Options

| Option | Description |
| --- | --- |
| `--band <LO-HI:INPUT[:LAYER]>` | One zoom band as `LO-HI:INPUT[:LAYER]` (repeatable). `INPUT` is a GeoParquet source or PMTiles archive for zooms `LO` to `HI`, and `LAYER` defaults to its file stem. The tuning guide covers the colon rules and the `LO-HI=INPUT[=LAYER]` form: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>. |
| `--name <TEXT>` | Human-readable name stored in the archive metadata. |
| `--description <TEXT>` | Description stored in the archive metadata. |
| `--attribution <TEXT>` | Attribution text or HTML stored in the archive metadata. |
| `--generalize` | Generalize each GeoParquet band instead of tiling it verbatim. Use for raw features spanning several zooms. |
| `--tile-buffer <TILE_BUFFER>` | Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width). Default: `8`. |
| `--extent <EXTENT>` | MVT tile extent: the tile-local coordinate resolution. A power of two is recommended. Default: `4096`. |
| `--max-tile-size <SIZE\|auto>` [alias: `tile-size-limit`] | Per-tile cap on the raw MVT size, such as `2M`, or 0 for no cap: a backstop behind `--max-compressed-tile-size`. `auto` is 2M, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile over a cap is merged or thinned until it fits; see `--size-valve`. Default: `auto`. |
| `--max-compressed-tile-size <SIZE\|auto>` | Per-tile cap on the gzip-compressed tile size, like tippecanoe's `-M`, such as `500K`, or 0 for no cap. `auto` is 500K, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile must fit both caps. Default: `auto`. |
| `--size-valve <merge\|drop>` | How to reduce a tile that exceeds its cap: `merge` merges the densest lines and polygons into a neighbor, which keeps its own attributes; `drop` removes features, leaving holes while preserving each retained feature's attributes. Point tiles use feature dropping in both modes. Default: `merge`. |
| `--coverage-fill` | Fill the holes that dense small polygons (parcels, fields, small admin units) leave at coarse zooms: under each tile's features, also draw the area its child tiles cover, with the attributes of one feature it sits under, in the room the tile's own features leave. Adds 10–17% to build time on dense coverages, about 1.5x on scattered sub-pixel fields (which it may also over-draw), and holds the finest level's geometry in memory. A sharded build's coarse job draws it; data shards (`--tile-range`) do not. |
| `--no-simple-clip-fastpath` | Clip all polygons with the full overlay for byte-stable tiles. The simple-ring fast path renders identically but can change a ring's starting vertex. |
| `--partition-wave <N\|auto>` | Partitions to export at once, or `auto` to size the wave to the cores and free memory. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>. Default: `auto`. |
| `--feature-order <input\|COLUMN[:asc\|:desc]>` | Feature order within each tile: `input` preserves source row order; `COLUMN[:asc\|:desc]` sorts by a property. Most renderers paint in this order unless the style overrides it. A `pyramid` band without that column keeps input order with a warning. Default: `input`. |
| `--feature-id <COLUMN>` | Use this integer column as the MVT feature id for `setFeatureState` across tiles and zooms. Every row must contain 0 to 2^64-1. Defaults to tile-local ids. |
| `--include-property <NAME>` | Keep only these properties in the tiles (repeatable). The input keeps every column, and the `--feature-order` column must stay. |
| `--exclude-property <NAME>` | Drop these properties from the tiles (repeatable). `--include-property` overrides it. |
| `--exclude-all-properties` | Drop every property, writing geometry-only tiles. `--include-property` overrides it. |
| `--report <PATH>` | Write the JSON pyramid report, with per-band tile counts, to this path. |
| `-f`, `--force` | Overwrite the output if it exists. |
| `--work-dir <DIR>` | Directory for temporary per-band files, deleted after the build. Defaults to the system temp directory. |
| `--allow-missing-zooms` | Accept a pre-tiled band with fewer zooms than declared. Missing zooms render empty; use only for a sparse pyramid. |

## `tylertoo merge`

Combine PMTiles archives that hold disjoint tiles into one

```text
tylertoo merge [OPTIONS] <OUTPUT> <INPUT>...
```

### Arguments

| Argument | Description |
| --- | --- |
| `<OUTPUT>` | Output PMTiles archive. |
| `<INPUT>` | Two or more input PMTiles archives with disjoint tile ids and one tile type and compression. The output's bounds, zoom range, and `vector_layers` are the union of the inputs'. |

### Options

| Option | Description |
| --- | --- |
| `--name <TEXT>` | Human-readable name stored in the archive metadata. |
| `--description <TEXT>` | Description stored in the archive metadata. |
| `--attribution <TEXT>` | Attribution text or HTML stored in the archive metadata. |
| `--work-dir <DIR>` | Directory for the spool file that holds tile data until the archive is complete. Defaults to the system temp directory. |
| `--report <PATH>` | Write the JSON merge report, with per-zoom tile counts, to this path. |
| `-f`, `--force` | Overwrite the output if it exists. |

## `tylertoo shard-plan`

Cut a dataset's tile space into N disjoint shards for a sharded build

```text
tylertoo shard-plan [OPTIONS] --output <PATH> --shards <N> [INPUT]
```

### Arguments

| Argument | Description |
| --- | --- |
| `<INPUT>` | Input GeoParquet that every job in the fleet tiles, in any form `tiles` accepts. Omit it with `--files-from`. |

### Options

| Option | Description |
| --- | --- |
| `--files-from <PATH>` | Plan for the inputs this manifest lists, one path or URL per line in dataset row order, instead of `INPUT`. Give every job in the fleet the same manifest. |
| `-o`, `--output <PATH>` | Where to write the shard plan. |
| `--shards <N>` | Number of data shards. Run one `tiles --shard i/N` job per shard plus one `--shard coarse` job, then merge all N+1 archives. |
| `--pivot <ZOOM>` | Pivot zoom: data shards cover this zoom through `--max-zoom`; the coarse job covers lower zooms. Choose a zoom with a few populated tiles per shard, usually z4 to z8. Default: `6`. |
| `-f`, `--force` | Overwrite an existing plan at `--output`. |

## Shared conversion options

These options apply to both `tylertoo tiles` and `tylertoo overview`.

### Thinning & visibility

| Option | Description |
| --- | --- |
| `--point-thinning <POINT_THINNING>` | Point thinning grid cell, as a multiple of the level's GSD (default 4.0, or 16.0 with `--cluster`). Larger cells keep fewer points. |
| `--line-thinning <LINE_THINNING>` | Line thinning grid cell, as a multiple of the level's GSD (default 1.0). Larger cells keep fewer lines. |
| `--polygon-thinning <POLYGON_THINNING>` | Polygon thinning grid cell, as a multiple of the level's GSD (default 0, off). Larger cells keep fewer polygons and can remove a polygon whose center falls in a larger neighbor's cell. |
| `--line-visibility <LINE_VISIBILITY>` | Drop lines whose bbox diagonal is shorter than this many GSDs at a level (default 2.0). |
| `--polygon-visibility <POLYGON_VISIBILITY>` | Drop polygons whose bbox diagonal is shorter than this many GSDs at a level (default 2.0). |
| `--verbatim` | Disable thinning, simplification, and density reduction for pre-aggregated or pre-levelled input. Explicit tuning flags override these defaults. |

### Ranking

| Option | Description |
| --- | --- |
| `--sort-direction <SORT_DIRECTION>` | Whether the larger (`desc`) or the smaller (`asc`) `--sort-key` value wins a cell. Default: `desc`. Possible values: `desc`, `asc`. |
| `--sort-key <COL>` | Numeric column used to choose each thinning cell's winning feature. Conflicts with `--class-rank`. |
| `--no-auto-rank` | Turn off automatic ranking for known schemas (Overture roads `class`/`road_class`, Overture places `confidence`). |
| `--magnitude-ladder <COL>` | Rank `COL` values from highest to lowest, assigning entry zooms from `--min-zoom` in `--ladder-step` increments. Features appear at their entry zoom and finer zooms, exempt from thinning. |
| `--ladder-step <N>` | Zooms between consecutive `--magnitude-ladder` rungs. Default: `1`. |
| `--entry-zoom <SPEC>` | Assign entry zooms: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...`, for example `density:5000=4,1000=6`. Unlisted values use the normal visibility gate. |
| `--class-rank <SPEC>` | Rank categorical classes for cell winners: `COLUMN:VALUE=RANK,...`, where higher wins. Unlisted values rank below listed ones and above nulls. |

### Generalization

| Option | Description |
| --- | --- |
| `--simplify-factor <SIMPLIFY_FACTOR>` | Simplification tolerance in multiples of each level's GSD (default 1.0, duplicating mode only). Lower values keep more vertices. |
| `--no-cascade` | Simplify each level from the source instead of from the next finer level. Slower, but each level stays within its own tolerance of the source. |
| `--gsd-base <F>` | Base of the zoom-to-GSD mapping, `gsd(z) = 40075016.69 / base / 2^z`: a larger base keeps more detail at every level. No effect with `--gsd`. Default: `1024`. |
| `--collapse` | Replace polygons too small for a level with a point instead of the default placeholder square. Fill styles ignore points; add a circle layer. |
| `--collapse-square` | Replace polygons too small for a level, and those thinned out at coarse levels, with small placeholder squares marking their area. This is the default in duplicating mode; output remains Polygon. |
| `--no-collapse` | Drop polygons too small for a level instead of replacing them with placeholder squares: smaller files, but a polygon coverage shows holes at coarse zooms (the default before #745). |
| `--representation <SPEC>` | Polygon representation by zoom band: comma-separated `LO-HI:KIND`, where `KIND` is `geom`, `point`, or `square`, such as `0-7:point,8-14:geom`. Band rules: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>. |

### Density budget

| Option | Description |
| --- | --- |
| `--no-density-drop` | Turn off the per-level density budget, leaving cell-winner thinning only. |
| `--drop-rate <F>` | Density budget decay: each coarser level keeps 1/rate of the next finer level's feature budget. Larger values thin mid zooms more. Default: `1.65`. |
| `--drop-gamma <F>` | Sparse-area protection: 1 reduces every neighborhood equally; larger values protect sparse areas more. Default: `1.5`. |
| `--drop-lines` | Apply the density budget to lines as well as points: smaller, faster tiles that drop line features at coarse zooms (tippecanoe's `--drop-lines`). |
| `--drop-polygons` | Apply the density budget to polygons as well as points. Removing a polygon leaves a hole in a coverage such as admin areas or parcels, so the budget excludes polygons by default (tippecanoe's `--drop-polygons`). |

### Output layout

| Option | Description |
| --- | --- |
| `--row-group-size <ROW_GROUP_SIZE>` | Maximum rows per output row group at each level. The cap increases if a level would exceed Parquet's row-group limit. Default: `10000`. |
| `--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` | How the row-group cap varies by level: `constant`, or `zoom-scaled`, which doubles it per zoom step coarser than the finest level. Default: `constant`. Possible values: `constant`, `zoom-scaled`. |
| `--full-column-stats` | Keep Parquet min/max stats on every column, even large string and geometry columns. Use it when remote clients filter on property columns. |

### Memory & performance

| Option | Description |
| --- | --- |
| `--no-streaming` | Load the whole dataset into memory instead of streaming it in two passes. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>. |
| `--read-batch-size <ROWS>` | Rows per Arrow read batch. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>. Default: `8192`. |
| `--profile <PROFILE>` | Memory profile: `speed` holds everything in RAM, `bounded` spills to disk and holds the least, and `auto` sizes every buffer to the machine and spills when it must. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles>. Default: `auto`. Possible values: `auto`, `speed`, `bounded`. |
| `--in-flight-batches <N\|auto>` | Read batches in flight at once, or `auto` to size it to the cores. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>. Default: `auto`. |
| `--read-workers <N\|auto>` | Reader threads for both passes, or `auto` for a quarter of the cores, up to 4. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>. Default: `auto`. |
| `--spill-dir <PATH>` | Directory for spill files: staged remote input, and on `tiles` the intermediate overview. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>. |
| `--save-plan <PATH>` | Save the convert plan to PATH and continue converting. Reuse it with `--plan`; sharded builds share one plan. |
| `--plan <PATH>` | Reuse a convert plan, skipping the first pass and level assignment. Its fingerprint must match this run's version, flags, and inputs. |

### Clustering

| Option | Description |
| --- | --- |
| `--cluster` | Merge each thinning cell's points into its surviving point, which gains a `point_count` column. Duplicating mode only. |
| `--accumulate-attribute <COL:OP>` | Aggregate a numeric column over each cluster as `COL:OP`, where `OP` is `sum`, `max`, `min`, or `mean` (repeatable). Requires `--cluster`. |

### Line coalescing

| Option | Description |
| --- | --- |
| `--no-coalesce-lines` | Turn off line coalescing, which joins touching same-class line segments into longer strokes at coarse levels. |
| `--coalesce-snap <F>` | Join line ends within this many GSDs of each other when coalescing. Set 0 to join only ends that match exactly. Default: `1`. |
| `--coalesce-max-level-rows <ROWS>` | Skip coalescing above this candidate-line count to bound memory. Long lines reach a corresponding geometry-size limit first. Default: `2000000`. |
| `--coalesce-junction-angle <DEG>` | Continue a line through a junction when the straightest pair turns by at most this many degrees. Set 0 to stop chains at every junction. Default: `0`. |

### Options

| Option | Description |
| --- | --- |
| `--bbox <XMIN,YMIN,XMAX,YMAX>` | Convert features whose bbox intersects this lon/lat box, or would one ±360° wrap away (0..360 data). Skips row groups outside the box. |

### Filtering

| Option | Description |
| --- | --- |
| `--filter <EXPR>` [alias: `where`] | Filter property columns with a SQL `WHERE` predicate, such as `confidence > 0.8`. Skips row groups that cannot match. Grammar: <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>. |

### Properties

| Option | Description |
| --- | --- |
| `--include-property <COL>` | Keep only these property columns (repeatable) and skip decoding the rest. Columns that other flags read must stay in the list. |
| `--exclude-property <COL>` | Drop these property columns (repeatable). `--include-property` overrides it. |
| `--exclude-all-properties` | Drop every property column, writing geometry only. `--include-property` overrides it. |
