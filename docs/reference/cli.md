<!-- GENERATED FILE — do not edit by hand.
     Regenerate: cargo run -p tylertoo --features gen-docs -- gen-reference-docs > docs/reference/cli.md
     CI fails if this file drifts from the clap definitions. -->

# CLI reference

In option tables, the Python column names the keyword that sets the same option on the matching Python function, such as `tylertoo.convert()` for `tiles`.

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
| [`stats`](#tylertoo-stats) | Report per-zoom tile sizes and optional per-layer stats for a PMTiles archive |
| [`pyramid`](#tylertoo-pyramid) | Build one archive from several inputs, each owning a zoom range |
| [`merge`](#tylertoo-merge) | Combine PMTiles archives that hold disjoint tiles into one |
| [`shard-plan`](#tylertoo-shard-plan) | Cut a dataset's tile space into N disjoint shards for a sharded build |
| [`serve`](#tylertoo-serve) | Serve PMTiles archives over HTTP with a map viewer |

### Global options

| Option | Description |
| --- | --- |
| <a id="option-quiet"></a>`-q`, `--quiet` | Print only warnings and errors. The output of `stats` and `validate` still prints. |

These go before or after the subcommand.

## `tylertoo tiles`

Generate PMTiles vector tiles from GeoParquet (the default command)

```text
tylertoo tiles [OPTIONS] [INPUT] [OUTPUT]
```

### Arguments

| Argument | Description |
| --- | --- |
| `<INPUT>` | Input GeoParquet. Pass a file, directory, glob, URL, or `s3://` or `gs://` prefix, or omit it with `--files-from`. Quote a presigned or SAS URL; logs and saved plans redact its query. Directory and prefix walks skip names starting with `.` or `_`; `key=value` partition keys do not become columns. EPSG:4326 and EPSG:3857 input is read directly. Other CRSs are reprojected to EPSG:4326 as they are read when they name an EPSG code (PROJJSON `id`, `EPSG:n` string, or Parquet `srid:n`) with a usable proj4 definition. A directory, list, or `--files-from` input can mix these CRSs. EPSG:3857 parts are reprojected to EPSG:4326 when mixed with other CRSs. An all-EPSG:3857 input is read directly. Unsupported CRSs are refused; the error points to `gpio convert reproject` to reproject the input first. Reprojection uses no grid shifts. A datum change uses the definition's proj4 `towgs84` Helmert parameters, so results can differ from PROJ/pyproj by metres (around 100 m at the edge of EPSG:27700's area). Same-datum projections, such as UTM and ETRS89 LAEA (EPSG:3035), match to under a centimetre. |
| `<OUTPUT>` | Output PMTiles file. Omit it with `--plan-only`, which writes no archive. |

### Options

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-files-from"></a>`--files-from <PATH>` | CLI only | Read `.parquet` files from this manifest instead of `INPUT`: one path or URL per line, in dataset row order. Ignores blank lines and lines starting with `#`. |
| <a id="option-min-zoom"></a>`--min-zoom <MIN_ZOOM>` | `min_zoom` | Minimum (coarsest) Web Mercator zoom. Default: `0`. |
| <a id="option-max-zoom"></a>`--max-zoom <MAX_ZOOM>` | `max_zoom` | Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a sample of the input; `auto:14` caps the estimate at z14. Ignored with `--gsd`. Default: `14`. |
| <a id="option-gsd"></a>`--gsd <GSDS>` | `gsds` | Comma-separated ground sample distances (GSDs) in meters, each smaller than the last. Overrides `--min-zoom` and `--max-zoom`. |
| <a id="option-band"></a>`--band <LO-HI:KIND>` | `bands` | Draw features or cells by zoom range. Repeat with LO-HI:KIND or Z:KIND (inclusive). Use geom, point or square for features; grid, h3, s2 or a5 for cells in the same layer. Requires contiguous feature bands. No overlaps. Excludes `--min-zoom`, `--max-zoom`, `--gsd` and `--representation`. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#several-layers>. |
| <a id="option-name"></a>`--name <TEXT>` | `name` | Human-readable name stored in the archive metadata. |
| <a id="option-description"></a>`--description <TEXT>` | `description` | Description stored in the archive metadata. |
| <a id="option-attribution"></a>`--attribution <TEXT>` | `attribution` | Attribution text or HTML stored in the archive metadata. |
| <a id="option-tile-buffer"></a>`--tile-buffer <TILE_BUFFER>` | `tile_buffer` | Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width). Default: `5`. |
| <a id="option-extent"></a>`--extent <EXTENT>` | `extent` | MVT tile extent: the tile-local coordinate resolution. A power of two is recommended. Default: `4096`. |
| <a id="option-max-tile-size"></a>`--max-tile-size <SIZE\|auto>` [alias: `tile-size-limit`] | `tile_size_limit` | Per-tile cap on the raw MVT size, or zero for no cap: a backstop behind `--max-compressed-tile-size`. `auto` is 2.00 MiB, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile over a cap goes through `--size-valve`. With several layers the caps apply to the joined tile. Default: `auto`. |
| <a id="option-max-compressed-tile-size"></a>`--max-compressed-tile-size <SIZE\|auto>` | `max_compressed_tile_size` | Per-tile cap on the gzip-compressed tile size, like tippecanoe's `-M`, or zero for no cap. `auto` is 500 KiB, or no cap with `tiles --verbatim` and for `pyramid` bands. Both caps apply, to the joined tile when there are several layers. Default: `auto`. |
| <a id="option-size-valve"></a>`--size-valve <merge\|drop>` | `size_valve` | How to reduce a tile over its cap: `merge` merges dense lines and polygons, drops singles, halves the extent down to 512, then drops whole features if needed. Merges keep the target's attributes and are never split into their sources. In a tile of lines or polygons, only a lone feature still over the cap at the extent floor is written over it, with a warning. `drop` removes features and keeps each survivor's attributes. Point tiles use dropping in both modes. Default: `merge`. |
| <a id="option-valve-scope"></a>`--valve-scope <smooth\|tile\|zoom>` | `valve_scope` | How far the size valve's thinning reaches past the tile that needs it: `smooth` predicts each tile's threshold from the export scan and fades it over the tiles around, so neighboring tiles differ by at most `--valve-max-step` (in point tiles, in the share of points they keep) and the density steps down over several tiles instead of jumping at a tile edge; tiles near a dense one are thinned too. `tile` thins each tile only as much as it needs on its own: the most detail, with visible steps at tile edges. `zoom` starts every tile from the largest threshold predicted at its zoom, like tippecanoe's one threshold per zoom: no edges, the least detail. On `tiles` and `export-pmtiles`, `--preset tippecanoe` sets it; not with `--tile-range`. Default: `smooth`. |
| <a id="option-valve-max-step"></a>`--valve-max-step <RATIO>` | `valve_max_step` | With `--valve-scope smooth`, the largest density step allowed between neighboring tiles: a tile keeps at most RATIO times the points its neighbor keeps, or merges at most RATIO times less hard, so the thinning around a dense tile fades over the tiles beside it. The fade reaches at most 8 tiles out: a tile thinned more than RATIO^9 fades by the larger even step that ends there. Smaller values spread the thinning further; larger values come closer to `--valve-scope tile`. Must be above one. Default: `2`. |
| <a id="option-tile-format"></a>`--tile-format <TILE_FORMAT>` | `tile_format` | Tile format: `mvt` (default) or `mlt` (MapLibre GL JS 5.12 or later, or MapLibre Native); MLT stores each attribute of a layer as one typed column, following mlt-core's MVT conversion: integers mixed with floats become f64 (doubles), other mixes become strings, and attribute order within a feature becomes column order; conversion precedes gzip and both size caps, so the caps measure MLT bytes; merge refuses mixed formats and pyramid bands must all have the given format. Default: `mvt`. Possible values: `mvt`, `mlt`. |
| <a id="option-mlt-profile"></a>`--mlt-profile <MLT_PROFILE>` | `mlt_profile` | MLT profile, non-default profiles need `--tile-format mlt`: `web` (default) supports web renderers, omits FastPFOR, FSST, and shared dictionaries, and stores integer columns wider than 32 bits as doubles when every value is exact (absolute value at most 9007199254740992), since GL JS reads 64-bit integers as BigInt and style expressions do not compare BigInt with numbers; mlt-core profiles: `native` for MapLibre Native with every codec and stored polygon triangles, `min-size` for smallest tiles, `diffable` for small binary diffs between releases, `fast-encoding` for quickest encoding. Default: `web`. Possible values: `web`, `native`, `min-size`, `diffable`, `fast-encoding`. |
| <a id="option-coverage-fill"></a>`--coverage-fill` | `coverage_fill` | Fill the holes that dense small polygons (parcels, fields, small admin units) leave at coarse zooms: under each tile's features, also draw the area its child tiles cover, with the attributes of one feature it sits under, in the room the tile's own features leave. Can overdraw scattered fields and holds the finest level's geometry in memory. A sharded `tiles` build's coarse job draws it; data shards (`--tile-range`) do not. |
| <a id="option-generate-variable-depth-tile-pyramid"></a>`--generate-variable-depth-tile-pyramid` | `variable_depth` | Stop each branch of the tile pyramid at the coarsest zoom whose tile holds all of that area's finest-level features within both size caps, like tippecanoe's flag of the same name. Fewer, larger tiles; the server must serve the nearest ancestor for a missing tile. Off by default. |
| <a id="option-no-simple-clip-fastpath"></a>`--no-simple-clip-fastpath` | `simple_clip_fastpath=False` | Clip all polygons with the full overlay for byte-stable tiles. The simple-ring fast path renders identically but can change a ring's starting vertex. |
| <a id="option-partition-wave"></a>`--partition-wave <N\|auto>` | `partition_wave` | Partitions to export at once, or `auto` to size the wave to the cores and free memory. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. Default: `auto`. |
| <a id="option-feature-order"></a>`--feature-order <input\|COLUMN[:asc\|:desc]>` | `feature_order` | Feature order within each tile: `input` preserves source row order; `COLUMN[:asc\|:desc]` sorts by a property. Most renderers paint in this order unless the style overrides it. A `pyramid` band without that column keeps input order with a warning. Default: `input`. |
| <a id="option-feature-id"></a>`--feature-id <COLUMN>` | `feature_id` | Use this integer column as the MVT feature id for `setFeatureState` across tiles and zooms. Every row must contain 0 to 2^64-1. By default features have no id. |
| <a id="option-report"></a>`--report <PATH>` | CLI only | Write a JSON report with `convert` and `export` sections, matching the reports of `overview` and `export-pmtiles`. |
| <a id="option-verbose"></a>`-v`, `--verbose` | CLI only | Print per-level and per-zoom breakdowns. |
| <a id="option-force"></a>`-f`, `--force` | CLI only | Overwrite the output if it exists. |
| <a id="option-layer-name"></a>`--layer-name <LAYER_NAME>` | `layer_name` | Layer name for the tiles. Defaults to the input's file stem. |
| <a id="option-layer-column"></a>`--layer-column <COLUMN>` | `layer_column` | Put each feature in the layer named by its value in COLUMN, and null or blank values in `_unknown`. The column is not written as a property. Each layer has its own overview. The size caps apply to the joined tile, with one size valve across source layers. Tiles still over a cap are kept with a warning and counted in the report. |
| <a id="option-keep-overview"></a>`--keep-overview <PATH>` | `keep_overview` | Save the intermediate overview GeoParquet to PATH after export. Does not change the PMTiles output. |
| <a id="option-temp-compression"></a>`--temp-compression <CODEC>` | `temp_compression` | Compression of the temporary intermediate overview: `lz4` uses less CPU, and `zstd` writes a smaller file, for a host short of temp disk or on a slow disk. No effect with --keep-overview, whose file is always ZSTD. Does not change the PMTiles output. Default: `lz4`. Possible values: `lz4`, `zstd`. |

### Generalization

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-max-zoom-simplify-factor"></a>`--max-zoom-simplify-factor <FACTOR>` | `max_zoom_simplify_factor` | Simplify each finest-level (canonical) line and polygon once before clipping, with tolerance factor × GSD at that level's zoom, like `overview --simplify-factor` for coarser levels. Only vertices are removed: features, parts and rings smaller than the tolerance keep their own geometry. Leaves coarser levels unchanged, except that variable-depth stop tiles carry the simplified finest-level features. Leaves the overview file unchanged. Unset, lines get 0.25 and polygons are not simplified, since each polygon is simplified on its own and shared borders would part. Zero simplifies nothing, as `tiles --verbatim` and `pyramid` bands do. |

### Sharded builds

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-shard"></a>`--shard <I/N\|coarse>` | CLI only | Build one shard: `I/N` for data shard I of N, or `coarse` for the zooms below the pivot. Requires `--shard-plan`, and data shards also need `--plan`. See <https://geoparquet-io.github.io/tylertoo/tutorials/brazil/#2-split-the-build-across-workers>. |
| <a id="option-shard-plan"></a>`--shard-plan <PATH>` | CLI only | The shard plan from `tylertoo shard-plan` that every job shares. Give every job the same inputs and settings. If a saved convert plan no longer matches the inputs or selection settings, rebuild it with `--plan-only` and `--save-plan` before restarting the data jobs. |
| <a id="option-tile-range"></a>`--tile-range <LO..HI>` | `tile_range` | Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at one zoom, plus their descendants. Prefer `--shard`, which also skips input the range cannot reach. |
| <a id="option-plan-only"></a>`--plan-only` | CLI only | Write the convert plan (`--save-plan`) and stop, with no export and no OUTPUT. Requires `--save-plan`. Use it when only the data jobs need tiles. The plan still covers the whole input and its selection settings; it does not create the separate `--shard-plan`. |

Also accepts all [shared conversion options](#shared-conversion-options).

## `tylertoo overview`

Build a multi-resolution overview GeoParquet file

```text
tylertoo overview [OPTIONS] [INPUT] [OUTPUT]
```

### Arguments

| Argument | Description |
| --- | --- |
| `<INPUT>` | Input GeoParquet. Pass a file, directory, glob, URL, or `s3://` or `gs://` prefix, or omit it with `--files-from`. Quote a presigned or SAS URL; logs and saved plans redact its query. Directory and prefix walks skip names starting with `.` or `_`; `key=value` partition keys do not become columns. EPSG:4326 and EPSG:3857 input is read directly. Other CRSs are reprojected to EPSG:4326 as they are read when they name an EPSG code (PROJJSON `id`, `EPSG:n` string, or Parquet `srid:n`) with a usable proj4 definition. A directory, list, or `--files-from` input can mix these CRSs. EPSG:3857 parts are reprojected to EPSG:4326 when mixed with other CRSs. An all-EPSG:3857 input is read directly. Unsupported CRSs are refused; the error points to `gpio convert reproject` to reproject the input first. Reprojection uses no grid shifts. A datum change uses the definition's proj4 `towgs84` Helmert parameters, so results can differ from PROJ/pyproj by metres (around 100 m at the edge of EPSG:27700's area). Same-datum projections, such as UTM and ETRS89 LAEA (EPSG:3035), match to under a centimetre. |
| `<OUTPUT>` | Output overview GeoParquet file. |

### Options

| Option | Python | Description |
| --- | --- | --- |
| `--files-from <PATH>` | CLI only | Read `.parquet` files from this manifest instead of `INPUT`: one path or URL per line, in dataset row order. Ignores blank lines and lines starting with `#`. |
| `--min-zoom <MIN_ZOOM>` | `min_zoom` | Minimum (coarsest) Web Mercator zoom. Default: `0`. |
| `--max-zoom <MAX_ZOOM>` | `max_zoom` | Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a sample of the input; `auto:14` caps the estimate at z14. Ignored with `--gsd`. Default: `6`. |
| `--gsd <GSDS>` | `gsds` | Comma-separated ground sample distances (GSDs) in meters, each smaller than the last. Overrides `--min-zoom` and `--max-zoom`. |
| `--report <PATH>` | CLI only | Write the JSON conversion report to this path. |
| `-f`, `--force` | CLI only | Overwrite the output if it exists. |
| <a id="option-mode"></a>`--mode <MODE>` | `mode` | Level layout: `duplicating` writes each level in full, and `partitioning` writes each feature once, at its coarsest level. Default: `duplicating`. Possible values: `duplicating`, `partitioning`. |
| <a id="option-cogp-compat"></a>`--cogp-compat` | `cogp_compat` | Write the third-party `cogp` footer key for readers of that overview format. Partitioning mode only. |

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

| Option | Python | Description |
| --- | --- | --- |
| `--name <TEXT>` | `name` | Human-readable name stored in the archive metadata. |
| `--description <TEXT>` | `description` | Description stored in the archive metadata. |
| `--attribution <TEXT>` | `attribution` | Attribution text or HTML stored in the archive metadata. |
| <a id="option-include-property"></a>`--include-property <NAME>` | `include_properties` | Keep only these properties in the tiles (repeatable). The input keeps every column, and the `--feature-order` column must stay. |
| <a id="option-exclude-property"></a>`--exclude-property <NAME>` | `exclude_properties` | Drop these properties from the tiles (repeatable). `--include-property` overrides it. |
| <a id="option-exclude-all-properties"></a>`--exclude-all-properties` | `exclude_all_properties` | Drop every property, writing geometry-only tiles. `--include-property` overrides it. |
| `--layer-name <LAYER_NAME>` | `layer_name` | MVT layer name written into every tile. Default: `overview`. |
| `--tile-buffer <TILE_BUFFER>` | `tile_buffer` | Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width). Default: `5`. |
| `--extent <EXTENT>` | `extent` | MVT tile extent: the tile-local coordinate resolution. A power of two is recommended. Default: `4096`. |
| `--max-tile-size <SIZE\|auto>` [alias: `tile-size-limit`] | `tile_size_limit` | Per-tile cap on the raw MVT size, or zero for no cap: a backstop behind `--max-compressed-tile-size`. `auto` is 2.00 MiB, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile over a cap goes through `--size-valve`. With several layers the caps apply to the joined tile. Default: `auto`. |
| `--max-compressed-tile-size <SIZE\|auto>` | `max_compressed_tile_size` | Per-tile cap on the gzip-compressed tile size, like tippecanoe's `-M`, or zero for no cap. `auto` is 500 KiB, or no cap with `tiles --verbatim` and for `pyramid` bands. Both caps apply, to the joined tile when there are several layers. Default: `auto`. |
| `--size-valve <merge\|drop>` | `size_valve` | How to reduce a tile over its cap: `merge` merges dense lines and polygons, drops singles, halves the extent down to 512, then drops whole features if needed. Merges keep the target's attributes and are never split into their sources. In a tile of lines or polygons, only a lone feature still over the cap at the extent floor is written over it, with a warning. `drop` removes features and keeps each survivor's attributes. Point tiles use dropping in both modes. Default: `merge`. |
| `--valve-scope <smooth\|tile\|zoom>` | `valve_scope` | How far the size valve's thinning reaches past the tile that needs it: `smooth` predicts each tile's threshold from the export scan and fades it over the tiles around, so neighboring tiles differ by at most `--valve-max-step` (in point tiles, in the share of points they keep) and the density steps down over several tiles instead of jumping at a tile edge; tiles near a dense one are thinned too. `tile` thins each tile only as much as it needs on its own: the most detail, with visible steps at tile edges. `zoom` starts every tile from the largest threshold predicted at its zoom, like tippecanoe's one threshold per zoom: no edges, the least detail. On `tiles` and `export-pmtiles`, `--preset tippecanoe` sets it; not with `--tile-range`. Default: `smooth`. |
| `--valve-max-step <RATIO>` | `valve_max_step` | With `--valve-scope smooth`, the largest density step allowed between neighboring tiles: a tile keeps at most RATIO times the points its neighbor keeps, or merges at most RATIO times less hard, so the thinning around a dense tile fades over the tiles beside it. The fade reaches at most 8 tiles out: a tile thinned more than RATIO^9 fades by the larger even step that ends there. Smaller values spread the thinning further; larger values come closer to `--valve-scope tile`. Must be above one. Default: `2`. |
| `--tile-format <TILE_FORMAT>` | `tile_format` | Tile format: `mvt` (default) or `mlt` (MapLibre GL JS 5.12 or later, or MapLibre Native); MLT stores each attribute of a layer as one typed column, following mlt-core's MVT conversion: integers mixed with floats become f64 (doubles), other mixes become strings, and attribute order within a feature becomes column order; conversion precedes gzip and both size caps, so the caps measure MLT bytes; merge refuses mixed formats and pyramid bands must all have the given format. Default: `mvt`. Possible values: `mvt`, `mlt`. |
| `--mlt-profile <MLT_PROFILE>` | `mlt_profile` | MLT profile, non-default profiles need `--tile-format mlt`: `web` (default) supports web renderers, omits FastPFOR, FSST, and shared dictionaries, and stores integer columns wider than 32 bits as doubles when every value is exact (absolute value at most 9007199254740992), since GL JS reads 64-bit integers as BigInt and style expressions do not compare BigInt with numbers; mlt-core profiles: `native` for MapLibre Native with every codec and stored polygon triangles, `min-size` for smallest tiles, `diffable` for small binary diffs between releases, `fast-encoding` for quickest encoding. Default: `web`. Possible values: `web`, `native`, `min-size`, `diffable`, `fast-encoding`. |
| `--coverage-fill` | `coverage_fill` | Fill the holes that dense small polygons (parcels, fields, small admin units) leave at coarse zooms: under each tile's features, also draw the area its child tiles cover, with the attributes of one feature it sits under, in the room the tile's own features leave. Can overdraw scattered fields and holds the finest level's geometry in memory. A sharded `tiles` build's coarse job draws it; data shards (`--tile-range`) do not. |
| `--generate-variable-depth-tile-pyramid` | `variable_depth` | Stop each branch of the tile pyramid at the coarsest zoom whose tile holds all of that area's finest-level features within both size caps, like tippecanoe's flag of the same name. Fewer, larger tiles; the server must serve the nearest ancestor for a missing tile. Off by default. |
| `--no-simple-clip-fastpath` | `simple_clip_fastpath=False` | Clip all polygons with the full overlay for byte-stable tiles. The simple-ring fast path renders identically but can change a ring's starting vertex. |
| `--partition-wave <N\|auto>` | `partition_wave` | Partitions to export at once, or `auto` to size the wave to the cores and free memory. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. Default: `auto`. |
| `--feature-order <input\|COLUMN[:asc\|:desc]>` | `feature_order` | Feature order within each tile: `input` preserves source row order; `COLUMN[:asc\|:desc]` sorts by a property. Most renderers paint in this order unless the style overrides it. A `pyramid` band without that column keeps input order with a warning. Default: `input`. |
| `--min-zoom <ZOOM>` | `min_zoom` | Minimum zoom in archive metadata, including empty coarse overview levels. Defaults to the coarsest level's zoom. |
| `--tile-range <LO..HI>` | `tile_range` | Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at one zoom, plus their descendants. Ranges that partition a zoom give disjoint archives, which `tylertoo merge` can join. |
| <a id="option-zoom-ceiling"></a>`--zoom-ceiling <ZOOM>` | `zoom_ceiling` | Emit only the tiles at or below this zoom, the coarse half of a sharded build. A partial overview kept from `tiles --shard coarse` needs a ceiling at or below its own. |
| `--feature-id <COLUMN>` | `feature_id` | Use this integer column as the MVT feature id for `setFeatureState` across tiles and zooms. Every row must contain 0 to 2^64-1. By default features have no id. |
| <a id="option-preset"></a>`--preset <tippecanoe>` | `preset` | Start from another tool's export defaults instead of tylertoo's; options you pass still override them. `tippecanoe` sets its tile buffer, size caps, size valve, one valve threshold per zoom (`--valve-scope zoom`) and max-zoom simplification: <https://geoparquet-io.github.io/tylertoo/guides/tippecanoe/#the-tippecanoe-preset>. Possible values: `tippecanoe`. |
| `--report <PATH>` | CLI only | Write the JSON export report, with per-zoom tile and feature counts, to this path. |
| `-f`, `--force` | CLI only | Overwrite the output if it exists. |

### Generalization

| Option | Python | Description |
| --- | --- | --- |
| `--max-zoom-simplify-factor <FACTOR>` | `max_zoom_simplify_factor` | Simplify each finest-level (canonical) line and polygon once before clipping, with tolerance factor × GSD at that level's zoom, like `overview --simplify-factor` for coarser levels. Only vertices are removed: features, parts and rings smaller than the tolerance keep their own geometry. Leaves coarser levels unchanged, except that variable-depth stop tiles carry the simplified finest-level features. Leaves the overview file unchanged. Unset, lines get 0.25 and polygons are not simplified, since each polygon is simplified on its own and shared borders would part. Zero simplifies nothing, as `tiles --verbatim` and `pyramid` bands do. |
| <a id="option-no-simplification-of-shared-nodes"></a>`--no-simplification-of-shared-nodes` | `no_simplification_of_shared_nodes` | With --max-zoom-simplify-factor, keep the vertices where shared polygon borders meet, end, or branch at the finest level, and simplify every shared stretch between them the same way in each polygon. Like tippecanoe's -pn. |

### Memory & performance

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-spill-dir"></a>`--spill-dir <PATH>` | `spill_dir` | Directory for the export's spill file, used when buffered tiles exceed the memory budget. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. |
| <a id="option-profile"></a>`--profile <PROFILE>` | `profile` | Memory profile: `speed` holds everything in RAM, `bounded` spills to disk and holds the least, and `auto` sizes every buffer to the machine and spills when it must. The default mimalloc build also purges freed pages immediately under `bounded` (at a speed cost); a build without mimalloc uses the system allocator. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. Default: `auto`. Possible values: `auto`, `speed`, `bounded`. |
| <a id="option-workers"></a>`--workers <N>` | `workers` | Worker threads for the whole run. Default: every core under `auto` and `speed`, at most 40 under `bounded`; `RAYON_NUM_THREADS` is used when this is unset. The pool is process-wide, so in Python the first call sets it. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. |
| <a id="option-memory-limit"></a>`--memory-limit <SIZE>` | `memory_limit` | Memory the run may use, in place of the detected available memory (cgroup limit or `MemAvailable`); overrides `TYLERTOO_AUTO_MEM_LIMIT_BYTES`. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. |

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

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-zoom"></a>`--zoom <ZOOM>` | CLI only | Decode one zoom level, which suits most uses. |
| `--min-zoom <MIN_ZOOM>` | `min_zoom` | Minimum zoom level to decode. |
| `--max-zoom <MAX_ZOOM>` | `max_zoom` | Maximum zoom level to decode. |
| <a id="option-layer"></a>`--layer <NAME>` | `layer` | Only decode features from this MVT layer. |
| `--report <PATH>` | CLI only | Write the JSON decode report to this path. |
| `-f`, `--force` | CLI only | Overwrite the output if it exists. |

## `tylertoo stats`

Report per-zoom tile sizes and optional per-layer stats for a PMTiles archive

```text
tylertoo stats [OPTIONS] <ARCHIVE>
```

### Arguments

| Argument | Description |
| --- | --- |
| `<ARCHIVE>` | PMTiles archive to report on. |

### Options

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-largest"></a>`--largest <N>` | `largest` | Number of largest tiles to list, ranked by stored size. Default: `10`. |
| <a id="option-layers"></a>`--layers` | `layers` | Add per-zoom, per-layer feature counts, uncompressed bytes, and geometry types. Decodes tile bodies; requires a vector (MVT) archive. |
| <a id="option-json"></a>`--json` | CLI only | Print the report as JSON instead of a human-readable table. |

## `tylertoo pyramid`

Build one archive from several inputs, each owning a zoom range

```text
tylertoo pyramid [OPTIONS] --band <LO-HI:[SYSTEM:]INPUT[:LAYER]> <OUTPUT>
```

### Arguments

| Argument | Description |
| --- | --- |
| `<OUTPUT>` | Output PMTiles archive. |

### Options

| Option | Python | Description |
| --- | --- | --- |
| `--band <LO-HI:[SYSTEM:]INPUT[:LAYER]>` | `bands` | One zoom band as `LO-HI:[SYSTEM:]INPUT[:LAYER]` or `LO-HI=[SYSTEM=]INPUT[=LAYER]` (repeatable; layer defaults to file stem). Omit SYSTEM to tile GeoParquet or reuse PMTiles; grid/h3/s2/a5 aggregate GeoParquet centroids into cells: <https://geoparquet-io.github.io/tylertoo/reference/tuning/#several-layers>. |
| `--name <TEXT>` | `name` | Human-readable name stored in the archive metadata. |
| `--description <TEXT>` | `description` | Description stored in the archive metadata. |
| `--attribution <TEXT>` | `attribution` | Attribution text or HTML stored in the archive metadata. |
| <a id="option-generalize"></a>`--generalize` | `generalize` | Generalize each GeoParquet band instead of tiling it verbatim. Use for raw features spanning several zooms. |
| `--tile-buffer <TILE_BUFFER>` | `tile_buffer` | Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width). Default: `5`. |
| `--extent <EXTENT>` | `extent` | MVT tile extent: the tile-local coordinate resolution. A power of two is recommended. Default: `4096`. |
| `--max-tile-size <SIZE\|auto>` [alias: `tile-size-limit`] | `tile_size_limit` | Per-tile cap on the raw MVT size, or zero for no cap: a backstop behind `--max-compressed-tile-size`. `auto` is 2.00 MiB, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile over a cap goes through `--size-valve`. With several layers the caps apply to the joined tile. Default: `auto`. |
| `--max-compressed-tile-size <SIZE\|auto>` | `max_compressed_tile_size` | Per-tile cap on the gzip-compressed tile size, like tippecanoe's `-M`, or zero for no cap. `auto` is 500 KiB, or no cap with `tiles --verbatim` and for `pyramid` bands. Both caps apply, to the joined tile when there are several layers. Default: `auto`. |
| `--size-valve <merge\|drop>` | `size_valve` | How to reduce a tile over its cap: `merge` merges dense lines and polygons, drops singles, halves the extent down to 512, then drops whole features if needed. Merges keep the target's attributes and are never split into their sources. In a tile of lines or polygons, only a lone feature still over the cap at the extent floor is written over it, with a warning. `drop` removes features and keeps each survivor's attributes. Point tiles use dropping in both modes. Default: `merge`. |
| `--valve-scope <smooth\|tile\|zoom>` | `valve_scope` | How far the size valve's thinning reaches past the tile that needs it: `smooth` predicts each tile's threshold from the export scan and fades it over the tiles around, so neighboring tiles differ by at most `--valve-max-step` (in point tiles, in the share of points they keep) and the density steps down over several tiles instead of jumping at a tile edge; tiles near a dense one are thinned too. `tile` thins each tile only as much as it needs on its own: the most detail, with visible steps at tile edges. `zoom` starts every tile from the largest threshold predicted at its zoom, like tippecanoe's one threshold per zoom: no edges, the least detail. On `tiles` and `export-pmtiles`, `--preset tippecanoe` sets it; not with `--tile-range`. Default: `smooth`. |
| `--valve-max-step <RATIO>` | `valve_max_step` | With `--valve-scope smooth`, the largest density step allowed between neighboring tiles: a tile keeps at most RATIO times the points its neighbor keeps, or merges at most RATIO times less hard, so the thinning around a dense tile fades over the tiles beside it. The fade reaches at most 8 tiles out: a tile thinned more than RATIO^9 fades by the larger even step that ends there. Smaller values spread the thinning further; larger values come closer to `--valve-scope tile`. Must be above one. Default: `2`. |
| `--tile-format <TILE_FORMAT>` | `tile_format` | Tile format: `mvt` (default) or `mlt` (MapLibre GL JS 5.12 or later, or MapLibre Native); MLT stores each attribute of a layer as one typed column, following mlt-core's MVT conversion: integers mixed with floats become f64 (doubles), other mixes become strings, and attribute order within a feature becomes column order; conversion precedes gzip and both size caps, so the caps measure MLT bytes; merge refuses mixed formats and pyramid bands must all have the given format. Default: `mvt`. Possible values: `mvt`, `mlt`. |
| `--mlt-profile <MLT_PROFILE>` | `mlt_profile` | MLT profile, non-default profiles need `--tile-format mlt`: `web` (default) supports web renderers, omits FastPFOR, FSST, and shared dictionaries, and stores integer columns wider than 32 bits as doubles when every value is exact (absolute value at most 9007199254740992), since GL JS reads 64-bit integers as BigInt and style expressions do not compare BigInt with numbers; mlt-core profiles: `native` for MapLibre Native with every codec and stored polygon triangles, `min-size` for smallest tiles, `diffable` for small binary diffs between releases, `fast-encoding` for quickest encoding. Default: `web`. Possible values: `web`, `native`, `min-size`, `diffable`, `fast-encoding`. |
| `--coverage-fill` | `coverage_fill` | Fill the holes that dense small polygons (parcels, fields, small admin units) leave at coarse zooms: under each tile's features, also draw the area its child tiles cover, with the attributes of one feature it sits under, in the room the tile's own features leave. Can overdraw scattered fields and holds the finest level's geometry in memory. A sharded `tiles` build's coarse job draws it; data shards (`--tile-range`) do not. |
| `--no-simple-clip-fastpath` | `simple_clip_fastpath=False` | Clip all polygons with the full overlay for byte-stable tiles. The simple-ring fast path renders identically but can change a ring's starting vertex. |
| `--partition-wave <N\|auto>` | `partition_wave` | Partitions to export at once, or `auto` to size the wave to the cores and free memory. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. Default: `auto`. |
| `--feature-order <input\|COLUMN[:asc\|:desc]>` | `feature_order` | Feature order within each tile: `input` preserves source row order; `COLUMN[:asc\|:desc]` sorts by a property. Most renderers paint in this order unless the style overrides it. A `pyramid` band without that column keeps input order with a warning. Default: `input`. |
| `--feature-id <COLUMN>` | `feature_id` | Use this integer column as the MVT feature id for `setFeatureState` across tiles and zooms. Every row must contain 0 to 2^64-1. By default features have no id. |
| `--include-property <NAME>` | `include_properties` | Keep only these properties in the tiles (repeatable). The input keeps every column, and the `--feature-order` column must stay. |
| `--exclude-property <NAME>` | `exclude_properties` | Drop these properties from the tiles (repeatable). `--include-property` overrides it. |
| `--exclude-all-properties` | `exclude_all_properties` | Drop every property, writing geometry-only tiles. `--include-property` overrides it. |
| `--report <PATH>` | CLI only | Write the JSON pyramid report, with per-band tile counts, to this path. |
| `-f`, `--force` | CLI only | Overwrite the output if it exists. |
| <a id="option-work-dir"></a>`--work-dir <DIR>` | `work_dir` | Directory for temporary per-band files, deleted after the build. Defaults to the system temp directory. |
| <a id="option-allow-missing-zooms"></a>`--allow-missing-zooms` | `allow_missing_zooms` | Accept a pre-tiled band with fewer zooms than declared. Missing zooms render empty; use only for a sparse pyramid. |
| <a id="option-accumulate-attribute"></a>`--accumulate-attribute <COL:OP>` | `accumulate_attributes` | Aggregate a numeric column in every cell band as COL:OP, where OP is sum, min, max, or mean (repeatable). Requires a cell band. |
| <a id="option-cluster-categories"></a>`--cluster-categories <COL>` | `cluster_categories` | Count a string column's values in every cell band as COL:label columns (up to 100 values plus 'other'). Requires a cell band; ranks categories as in tiles. |

### Generalization

| Option | Python | Description |
| --- | --- | --- |
| `--max-zoom-simplify-factor <FACTOR>` | `max_zoom_simplify_factor` | Simplify each finest-level (canonical) line and polygon once before clipping, with tolerance factor × GSD at that level's zoom, like `overview --simplify-factor` for coarser levels. Only vertices are removed: features, parts and rings smaller than the tolerance keep their own geometry. Leaves coarser levels unchanged, except that variable-depth stop tiles carry the simplified finest-level features. Leaves the overview file unchanged. Unset, lines get 0.25 and polygons are not simplified, since each polygon is simplified on its own and shared borders would part. Zero simplifies nothing, as `tiles --verbatim` and `pyramid` bands do. |

### Memory & performance

| Option | Python | Description |
| --- | --- | --- |
| `--profile <PROFILE>` | `profile` | Memory profile, as for tiles, for every GeoParquet band's convert and export; changes memory use, not output bytes. Default: `auto`. Possible values: `auto`, `speed`, `bounded`. |
| `--workers <N>` | `workers` | Worker threads, as for tiles. |
| `--memory-limit <SIZE>` | `memory_limit` | Memory the run may use, such as `24G`, as for tiles. |

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

Cut a dataset's tile space into N disjoint shards for a sharded build.

If row groups lack usable bounding-box statistics, or cover too much of the pivot zoom, their rows are spread evenly instead of balanced. Prepare tighter row groups with `gpio sort hilbert --add-bbox`. Empty ranges are legal and produce empty archives that `merge` skips. If many ranges are empty, reduce `--shards` or use a finer `--pivot`. Choose a pivot with a few populated tiles per shard, within the build's finest zoom. See <https://geoparquet-io.github.io/tylertoo/tutorials/brazil/#2-split-the-build-across-workers>.

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
| <a id="option-output"></a>`-o`, `--output <PATH>` | Where to write the shard plan. |
| <a id="option-shards"></a>`--shards <N>` | Number of data shards. Run one `tiles --shard i/N` job per shard plus one `--shard coarse` job, then merge the data archives and the coarse archive. |
| <a id="option-pivot"></a>`--pivot <ZOOM>` | Pivot zoom: data shards cover this zoom through `tiles --max-zoom`; the coarse job covers lower zooms. Choose a zoom with a few populated tiles per shard. Default: `6`. |
| `-f`, `--force` | Overwrite an existing plan at `--output`. |

## `tylertoo serve`

Serve PMTiles archives over HTTP with a map viewer.

Run until Ctrl-C. The viewer at `/` handles MVT and MLT archives.
It loads MapLibre GL JS from unpkg.com and needs internet access.

`/index.json` lists archive names. Each `{name}` is the archive's file
stem. `/{name}.json` returns TileJSON; `/{name}/{z}/{x}/{y}` returns
tiles, with HTTP 204 for absent tiles. `/{name}/metadata.json` returns
archive metadata; `/{name}/stats.json` returns tile-size statistics.
`/{name}.pmtiles` serves the archive with HTTP Range support.
It honors `If-Match` and `If-Range` against its `ETag`.
If a Range read detects the file being rewritten in place during the
read, it returns HTTP 412 for requests with `If-Match` or `If-Range`,
or HTTP 503 otherwise. A whole-file download caught by a rewrite is
cut off before its `Content-Length`. Renaming another file over the
path does not affect a download in progress.

At most 64 responses can be in flight at once. Further requests get
HTTP 503 with a `Retry-After` header.

`serve` returns HTTP 204 for missing tiles without scaling up a stored
ancestor, so variable-depth archives (`tiles --generate-variable-depth-tile-pyramid`)
show holes below stopped tiles; `serve` warns when it opens one.

An archive rewritten on disk is reopened on the next request for it;
if it does not reopen, requests for it return HTTP 503 until it does.

```text
tylertoo serve [OPTIONS] <ARCHIVE>...
```

### Arguments

| Argument | Description |
| --- | --- |
| `<ARCHIVE>` | PMTiles archives to serve, each named by its file stem. |

### Options

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-host"></a>`--host <ADDR>` | `host` | Address to bind. The default allows local access only. Use `--host 0.0.0.0` or another address to expose it to the network. Default: `127.0.0.1`. |
| <a id="option-port"></a>`--port <N>` | `port` | Port to listen on. Use `--port 0` to pick a free port. Default: `8080`. |
| <a id="option-cors"></a>`--cors <ORIGIN>` | `cors` | Allow pages from ORIGIN to read responses. Repeat `--cors ORIGIN` for multiple origins. Each value is an exact origin such as `https://pmtiles.io`, or `*` for any origin. With no `--cors`, no Access-Control headers are sent, so pages on other origins cannot read the archives. The built-in viewer is same-origin and needs none. |

## Shared conversion options

These options apply to both `tylertoo tiles` and `tylertoo overview`.

### Thinning & visibility

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-point-thinning"></a>`--point-thinning <POINT_THINNING>` | `point_thinning` | Point thinning grid cell, as a multiple of the level's GSD. With `--cluster`, the default is 16. Larger cells keep fewer points. Default: `4`. |
| <a id="option-line-thinning"></a>`--line-thinning <LINE_THINNING>` | `line_thinning` | Line thinning grid cell, as a multiple of the level's GSD. Larger cells keep fewer lines. Default: `1`. |
| <a id="option-polygon-thinning"></a>`--polygon-thinning <POLYGON_THINNING>` | `polygon_thinning` | Polygon thinning grid cell, as a multiple of the level's GSD. Larger cells keep fewer polygons and can remove a polygon whose center falls in a larger neighbor's cell. Default: `0`. |
| <a id="option-line-visibility"></a>`--line-visibility <LINE_VISIBILITY>` | `line_visibility` | Drop lines whose bbox diagonal is shorter than this many GSDs at a level. Default: `2`. |
| <a id="option-polygon-visibility"></a>`--polygon-visibility <POLYGON_VISIBILITY>` | `polygon_visibility` | Gate polygons whose bbox diagonal is shorter than this many GSDs at a level. In duplicating mode, rejected polygons contribute area to collapse squares unless a point or drop disposition applies; lowering this gate keeps more source geometry, not necessarily more visible area. Default: `2`. |
| <a id="option-verbatim"></a>`--verbatim` | `verbatim` | Disable thinning, simplification, and density reduction for pre-aggregated or pre-levelled input. Explicit tuning flags override these defaults. |

### Ranking

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-sort-direction"></a>`--sort-direction <SORT_DIRECTION>` | `sort_direction` | Whether the larger (`desc`) or the smaller (`asc`) `--sort-key` value wins a cell. Default: `desc`. Possible values: `desc`, `asc`. |
| <a id="option-sort-key"></a>`--sort-key <COL>` | `sort_key` | Numeric column used to choose each thinning cell's winning feature. Conflicts with `--class-rank`. |
| <a id="option-no-auto-rank"></a>`--no-auto-rank` | `no_auto_rank` | Turn off automatic ranking for known schemas (Overture roads `class`/`road_class`, Overture places `confidence`). |
| <a id="option-magnitude-ladder"></a>`--magnitude-ladder <COL>` | `magnitude_ladder` | Rank `COL` values from highest to lowest, assigning entry zooms from `--min-zoom` in `--ladder-step` increments. Features appear at their entry zoom and finer zooms, exempt from thinning. |
| <a id="option-ladder-step"></a>`--ladder-step <N>` | `ladder_step` | Zooms between consecutive `--magnitude-ladder` rungs. Default: `1`. |
| <a id="option-entry-zoom"></a>`--entry-zoom <SPEC>` | `entry_zoom` | Assign entry zooms: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...`, for example `density:5000=4,1000=6`. Unlisted values use the normal visibility gate. |
| <a id="option-class-rank"></a>`--class-rank <SPEC>` | `class_rank_column`, `class_ranks`, `class_rank_unknown` | Rank categorical classes for cell winners: `COLUMN:VALUE=RANK,...`, where higher wins. Unlisted values rank below listed ones and above nulls. |

### Generalization

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-simplify-factor"></a>`--simplify-factor <SIMPLIFY_FACTOR>` | `simplify_factor` | Simplification tolerance in multiples of each level's GSD (duplicating mode only). Lower values keep more vertices. Default: `1`. |
| <a id="option-no-cascade"></a>`--no-cascade` | `cascade=False` | Simplify each level from the source instead of from the next finer level. Slower, but each level stays within its own tolerance of the source. |
| `--no-simplification-of-shared-nodes` | `no_simplification_of_shared_nodes` | Keep the vertices where shared polygon borders meet, end, or branch, and simplify every shared stretch between them the same way in each polygon, so neighbours share their simplified borders instead of opening slivers. Costs time, memory and archive bytes on dense coverages. Like tippecanoe's -pn. |
| <a id="option-gsd-base"></a>`--gsd-base <F>` | `gsd_base` | Base of the zoom-to-GSD mapping: a larger base keeps more detail at every level. Uses an equatorial circumference of 40075016.69 meters. No effect with `--gsd`. Default: `1024`. |
| <a id="option-collapse"></a>`--collapse` | `collapse` | Replace polygons too small for a level with a point instead of the default placeholder square. Fill styles ignore points; add a circle layer. |
| <a id="option-collapse-square"></a>`--collapse-square` | `collapse_square` | Replace polygons too small for a level, and those thinned out at coarse levels, with small placeholder squares marking their area. This is the default in duplicating mode; output remains Polygon. |
| <a id="option-no-collapse"></a>`--no-collapse` | `no_collapse` | Drop polygons too small for a level instead of replacing them with placeholder squares: smaller files, but a polygon coverage shows holes at coarse zooms (the default before #745). |
| <a id="option-representation"></a>`--representation <SPEC>` | `representation` | Polygon representation by zoom band: comma-separated `LO-HI:KIND`, where `KIND` is `geom`, `point`, or `square`. Ranges are inclusive. Band rules: <https://geoparquet-io.github.io/tylertoo/reference/tuning/#collapse-and-representation>. |

### Density budget

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-no-density-drop"></a>`--no-density-drop` | `density_drop=False` | Turn off the per-level density budget, leaving cell-winner thinning only. |
| <a id="option-drop-rate"></a>`--drop-rate <F>` | `drop_rate` | Density budget decay: each coarser level keeps the next finer level's budget divided by this rate. Larger values thin mid zooms more. A rate at or below one silently disables the budget. Default: `2.5`. |
| <a id="option-drop-gamma"></a>`--drop-gamma <F>` | `drop_gamma` | Sparse-area protection, at least 1: 1 reduces every neighborhood equally; larger values protect sparse areas more. Smaller values are rejected. Default: `1.5`. |
| <a id="option-drop-lines"></a>`--drop-lines` | `drop_lines` | Apply the density budget to lines as well as points: smaller, faster tiles that drop line features at coarse zooms (tippecanoe's `--drop-lines`). |
| <a id="option-drop-polygons"></a>`--drop-polygons` | `drop_polygons` | Apply the density budget to polygons as well as points. Removing a polygon leaves a hole in a coverage such as admin areas or parcels, so the budget excludes polygons by default (tippecanoe's `--drop-polygons`). Polygons too small to see (outside `square` representation bands), or removed by `--polygon-thinning`, still become placeholder squares, so coarse zooms can hold more features than the budget; `--no-collapse` removes them outside `square` bands. |

### Output layout

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-row-group-size"></a>`--row-group-size <ROW_GROUP_SIZE>` | `row_group_size` | Maximum rows per output row group at each level. The cap increases if a level would exceed Parquet's row-group limit. Default: `10000`. |
| <a id="option-row-group-size-policy"></a>`--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` | `row_group_size_policy` | How the row-group cap varies by level: `constant`, or `zoom-scaled`, which doubles it per zoom step coarser than the finest level. Default: `constant`. Possible values: `constant`, `zoom-scaled`. |
| <a id="option-full-column-stats"></a>`--full-column-stats` | `full_column_stats` | Keep Parquet min/max stats on every column, even large string and geometry columns. Use it when remote clients filter on property columns. |

### Memory & performance

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-read-batch-size"></a>`--read-batch-size <ROWS>` | `read_batch_size` | Rows per Arrow read batch, in the range 1..=1048576. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. Default: `8192`. |
| `--profile <PROFILE>` | `profile` | Memory profile: `speed` holds everything in RAM, `bounded` spills to disk and holds the least, and `auto` sizes every buffer to the machine and spills when it must. The default mimalloc build also purges freed pages immediately under `bounded` (at a speed cost); a build without mimalloc uses the system allocator. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. Default: `auto`. Possible values: `auto`, `speed`, `bounded`. |
| `--workers <N>` | `workers` | Worker threads for the whole run. Default: every core under `auto` and `speed`, at most 40 under `bounded`; `RAYON_NUM_THREADS` is used when this is unset. The pool is process-wide, so in Python the first call sets it. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. |
| `--memory-limit <SIZE>` | `memory_limit` | Memory the run may use, in place of the detected available memory (cgroup limit or `MemAvailable`); overrides `TYLERTOO_AUTO_MEM_LIMIT_BYTES`. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. |
| <a id="option-in-flight-batches"></a>`--in-flight-batches <N\|auto>` | `in_flight_batches` | Read batches in flight at once, or `auto` to size it to the cores. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. Default: `auto`. |
| <a id="option-read-workers"></a>`--read-workers <N\|auto>` | `read_workers` | Reader threads for both passes, or `auto` to size each pass to the core count, up to 4. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. Default: `auto`. |
| `--spill-dir <PATH>` | `spill_dir` | Directory for spill files: staged remote input, and on `tiles` the intermediate overview. See <https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory>. |
| <a id="option-save-plan"></a>`--save-plan <PATH>` | `save_plan` | Save the convert plan to PATH and continue converting. Reuse it with `--plan`; sharded builds share one plan. |
| <a id="option-plan"></a>`--plan <PATH>` | `plan` | Reuse a convert plan, skipping the first pass and level assignment. Its fingerprint must match this run's version, flags, and inputs. |

### Clustering

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-cluster"></a>`--cluster` | `cluster` | Merge each thinning cell's points into its surviving point, which gains a `point_count` column. Duplicating mode only. Reserves `level`, `input_row`, and `point_count`; input columns with these names are renamed by appending underscores until unique. |
| `--accumulate-attribute <COL:OP>` | `accumulate_attributes` | Aggregate a numeric column over each cluster as `COL:OP`, where `OP` is `sum`, `max`, `min`, or `mean` (repeatable). Requires `--cluster`. |
| `--cluster-categories <COL>` | `cluster_categories` | Count a string column's values per cluster in `COL:value` columns (up to 100 values plus `other`, requires `--cluster`). |

### Line coalescing

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-no-coalesce-lines"></a>`--no-coalesce-lines` | `coalesce_lines=False` | Turn off line coalescing, which joins touching same-class line segments into longer strokes at coarse levels. Coalescing reserves `coalesced_count`, in addition to `level` and `input_row`; input columns with reserved names are renamed by appending underscores until unique. |
| <a id="option-coalesce-snap"></a>`--coalesce-snap <F>` | `coalesce_snap` | Join line ends within this many GSDs of each other when coalescing. Set 0 to join only ends that match exactly. Default: `1`. |
| <a id="option-coalesce-max-level-rows"></a>`--coalesce-max-level-rows <ROWS>` | `coalesce_max_level_rows` | Skip coalescing above this candidate-line count to bound memory. Long lines reach a corresponding geometry-size limit first. Default: `2000000`. |
| <a id="option-coalesce-junction-angle"></a>`--coalesce-junction-angle <DEG>` | `coalesce_junction_angle` | Continue a line through a junction when the straightest pair turns by at most this many degrees. Set 0 to stop chains at every junction. Default: `0`. |

### Options

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-bbox"></a>`--bbox <XMIN,YMIN,XMAX,YMAX>` | `bbox` | Convert features whose bbox intersects this lon/lat box, or would one ±360° wrap away (0..360 data). Skips row groups outside the box. |
| `--preset <tippecanoe>` | `preset` | Start from another tool's defaults instead of tylertoo's; options you pass still override them. `tippecanoe` sets its drop rate, gamma and simplification, and with `tiles` its tile buffer, size caps, size valve, one valve threshold per zoom and max-zoom simplification: <https://geoparquet-io.github.io/tylertoo/guides/tippecanoe/#the-tippecanoe-preset>. Possible values: `tippecanoe`. |

### Filtering

| Option | Python | Description |
| --- | --- | --- |
| <a id="option-mask"></a>`--mask <PATH>` | `mask` | Keep features whose geometry intersects polygons or multipolygons in a GeoJSON (.geojson/.json, RFC 7946 lon/lat) or GeoParquet file; touching and copies one ±360° wrap away count. Skips row groups by the mask's bbox, combines with --bbox and --filter by AND. A GeoParquet mask uses its declared CRS and the same reprojection rules as input. EPSG:4326 masks are converted to EPSG:3857 for 3857 input and vice versa, vertex by vertex. WKB/shapely masks have no CRS: use EPSG:3857 for EPSG:3857 input, otherwise lon/lat EPSG:4326, including for reprojected input. |
| <a id="option-filter"></a>`--filter <EXPR>` [alias: `where`] | `filter` | Filter property columns with a SQL `WHERE` predicate, such as `class = 'primary'`. Skips row groups that cannot match. Grammar: <https://geoparquet-io.github.io/tylertoo/reference/tuning/#expression-syntax>. |

### Properties

| Option | Python | Description |
| --- | --- | --- |
| `--include-property <COL>` | `include_properties` | Keep only these property columns (repeatable) and skip decoding the rest. Columns that other flags read must stay in the list. |
| `--exclude-property <COL>` | `exclude_properties` | Drop these property columns (repeatable). `--include-property` overrides it. |
| `--exclude-all-properties` | `exclude_all_properties` | Drop every property column, writing geometry only. `--include-property` overrides it. |

## Environment variables

| Variable | Meaning |
| --- | --- |
| `RAYON_NUM_THREADS` | Worker threads used when the workers option is unset. |
| `TYLERTOO_AUTO_MEM_LIMIT_BYTES` | Available-memory override in bytes; the memory-limit option takes precedence. |
| `TYLERTOO_SKIP_MEMORY_PREFLIGHT` | Skip the memory preflight refusal; allocation can still fail. |
| `TYLERTOO_HTTP_<KEY>` | Remote HTTP client option named by KEY, converted to lowercase; unsupported or invalid options produce a warning. |
| `TYLERTOO_STAGING_CONCURRENCY` | Experimental, not stable API: maximum concurrent staging requests. |
| `TYLERTOO_STAGING_MEM_BUDGET_BYTES` | Experimental, not stable API: staging memory budget in bytes. |
| `TYLERTOO_STAGING_COALESCE_BYTES` | Experimental, not stable API: staging request coalescing window in bytes. |
| `TYLERTOO_PROFILE_JSON` | Path for structured pipeline profiling output. |
| `TYLERTOO_TILE_TRACE` | Path for the per-tile export trace. |
