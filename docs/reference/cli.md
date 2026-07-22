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

## `tylertoo`

Top-level CLI: a default (bare) tile pipeline plus subcommands.

`tylertoo input.parquet output.pmtiles` still works (bare tile pipeline); `tylertoo tiles ...` is the explicit form, and `overview` / `validate` are the GeoParquet-overview subcommands.

**Usage:** `tylertoo <COMMAND>`

###### **Subcommands:**

* `tiles` — Generate PMTiles vector tiles (the default pipeline)
* `overview` — Build a multi-resolution overview GeoParquet file
* `validate` — Validate a GeoParquet overview file against the spec (§6.2)
* `export-pmtiles` — Export a PMTiles archive from an overview GeoParquet file (Plan E0)
* `decode` — Decode a PMTiles vector-tile archive back to GeoParquet



## `tylertoo tiles`

Generate PMTiles vector tiles (the default pipeline)

**Usage:** `tylertoo tiles [OPTIONS] [INPUT] [OUTPUT]`

###### **Arguments:**

* `<INPUT>` — Input GeoParquet (EPSG:4326 or EPSG:3857): a local file, a directory or glob of partitions, or a remote URL (s3://, https://, gs://; trailing- slash prefixes are listed to their .parquet objects). Omit when --files-from is given
* `<OUTPUT>` — Output PMTiles file

###### **Options:**

* `--files-from <PATH>` — Convert the inputs listed in this manifest instead of a positional INPUT: one .parquet path or remote URL per line (no directories, globs, or prefixes); `#` comments and blank lines are skipped; line order is preserved verbatim as the dataset row order. Usage: --files-from PATH OUTPUT
* `--min-zoom <MIN_ZOOM>` — Minimum (coarsest) Web Mercator zoom level

  Default value: `0`
* `--max-zoom <MAX_ZOOM>` — Maximum (finest) Web Mercator zoom level

  Default value: `14`
* `--gsd <GSDS>` — Explicit comma-separated GSD list (meters, strictly decreasing). Overrides --min-zoom/--max-zoom when set (same as `overview --gsd`)
* `--bbox <XMIN,YMIN,XMAX,YMAX>` — Regional extract: only convert features whose bbox intersects this bounding box (lon/lat degrees: xmin,ymin,xmax,ymax). See --bbox in `tylertoo overview --help` for details
* `--layer-name <LAYER_NAME>` — Layer name for the output tiles (default: derived from input filename)
* `--max-tile-size <SIZE>` — Maximum tile size (e.g., "500K", "1M", or raw bytes; default 500K). When a tile exceeds it, the export sheds features in a single pass (largest- first for polygons/lines; a uniform spatial stride for point tiles). Pass 0 to disable. Aliased as --tile-size-limit

  Default value: `500K`
* `--no-simple-clip-fastpath` — Force the i_overlay boundary-bridge fallback on every polygon clip, disabling the default simple-clip fast path. Use only when you need byte-stable tile output (the fast path rotates simple rings to a different start vertex)
* `--tile-buffer <TILE_BUFFER>` — Per-tile edge buffer, in tile pixels, carried across tile seams so features don't clip at boundaries

  Default value: `8`
* `--partition-wave <N|auto>` — Partitions processed per band read during the export phase (export concurrency). `auto` (default) sizes a memory budget from core count and available RAM (override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES); pass an integer to override. Wider waves use more cores and more peak memory. Output is byte-identical for every value

  Default value: `auto`
* `--report <PATH>` — Write a JSON report to this path: a combined object with a `convert` section (matching `overview --report`) and an `export` section (matching `export-pmtiles --report`)
* `--keep-overview <PATH>` — Write the intermediate overview GeoParquet to PATH and RETAIN it, instead of a temp file removed after the export — one run then yields both the reusable overview (see `tylertoo overview`) and the PMTiles. The PMTiles output is identical either way
* `-v`, `--verbose` — Enable verbose output (per-level and per-zoom breakdowns)
* `--sort-key <COL>` — Column name used as the cell-winner priority (sort) key. Mutually exclusive with --class-rank
* `--class-rank <SPEC>` — Categorical class ranking (higher priority wins a cell). Format: `COLUMN:VALUE=RANK,VALUE=RANK,...` — e.g. `--class-rank road_class:motorway=5,primary=4,residential=2`. Present-but-unlisted values rank below every listed value (but above nulls). Mutually exclusive with --sort-key
* `--no-auto-rank` — Disable auto-detection of well-known schemas (Overture roads `class`/ `road_class`, Overture places `confidence`)
* `--filter <EXPR>` — Attribute filter: only convert features matching this SQL-WHERE-style predicate over the input's property columns, e.g. "confidence > 0.8" or "crop_type IN ('soy', 'corn')". Supports =, !=, <, <=, >, >=, IN (...), IS [NOT] NULL, AND/OR/NOT, parentheses, string/ numeric literals, "quoted columns", and 'YYYY-MM-DD'/RFC 3339 timestamps (UTC). Nulls follow SQL three-valued logic. Aliased as --where
* `--gsd-base <F>` — GSD tile-band base for the zoom→GSD mapping (default 1024): gsd(z) = 40075016.69 / base / 2^z. Master detail knob for a zoom-range plan — a LARGER base means smaller GSDs (denser, more detailed levels), a SMALLER base means larger GSDs (sparser, cheaper levels). No effect when --gsd is given

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance factor: RDP tolerance = factor * gsd (meters), duplicating mode only (default 1.0). LOWER keeps more vertices (crisper, heavier coarse levels); HIGHER sheds more (cruder, lighter). The finest level is always verbatim. Features whose bbox diagonal falls below the tolerance are dropped entirely

  Default value: `1.0`
* `--collapse` — Collapse below-visibility polygons to a representative point instead of dropping them (opt-in). Changes geometry type at coarse levels, so fill styles ignore the points — add a circle layer, or use --collapse-square to stay type-preserving
* `--collapse-square` — Collapse below-visibility polygons to a ~1xGSD placeholder SQUARE at the representative point instead of dropping them (opt-in). Squares are area-dithered (a polygon of area A below threshold T survives as a T-area square with probability A/T) so aggregate area stays truthful. Type- preserving (output stays Polygon), unlike --collapse
* `--representation <SPEC>` — Zoom-band representation selector: comma-separated LO-HI:KIND bands, e.g. "0-7:point,8-14:geom" or "0-5:square". KIND is geom (default), point (polygons become representative-point centroids), or square (below-tolerance polygons emit area-dithered placeholder squares). Bands must not overlap; non-geom bands must end before --max-zoom; point bands must be contiguous from the coarsest zoom. Requires a zoom-range plan (not --gsd) and duplicating mode
* `--no-cascade` — Disable cascading simplification and reproduce the pre-cascade output byte-for-byte. By default each coarser level is simplified from the next-finer level's already-simplified output (faster on duplicating mode; coarse coordinates differ by up to ~2x the level tolerance)
* `--point-thinning <POINT_THINNING>` — Point thinning factor: grid cell size = factor * gsd (default 4.0, or 16.0 with --cluster). One feature survives per grid cell per level, so BIGGER = sparser, SMALLER = denser
* `--line-thinning <LINE_THINNING>` — Line thinning factor: grid cell size = factor * gsd (default 1.0). BIGGER = sparser (fewer lines survive per level), SMALLER = denser. The roads/line counterpart to --point-thinning

  Default value: `1.0`
* `--polygon-thinning <POLYGON_THINNING>` — Polygon thinning factor: grid cell size = factor * gsd (default 1.0). BIGGER = sparser, SMALLER = denser

  Default value: `1.0`
* `--line-visibility <LINE_VISIBILITY>` — Line visibility gate in GSD multiples: a line is eligible at a level only if its bbox diagonal >= factor * gsd (default 2.0). A hard drop, not a thin: BIGGER drops more small lines (sparser), SMALLER keeps more

  Default value: `2.0`
* `--polygon-visibility <POLYGON_VISIBILITY>` — Polygon visibility gate in GSD multiples: a polygon is eligible only if its bbox diagonal >= factor * gsd (default 2.0). BIGGER drops more small polygons (sparser), SMALLER keeps more. Use --collapse to keep dropped polygons as representative points

  Default value: `2.0`
* `--drop-rate <F>` — Per-level density drop rate: each coarser level keeps 1/rate of the next finer level's feature budget (default 1.65). After cell-winner thinning, each level is capped at budget(L) = N / rate^(finest-L) (N = input feature count) and the lowest-priority survivors are dropped to meet it. BIGGER = sparser mid-zooms and smaller files, SMALLER = gentler. The finest level is never dropped

  Default value: `1.65`
* `--drop-gamma <F>` — Spatial-fairness strength for the density budget (default 1.5). The budget is shared across coarse super-cells so a global cut cannot empty sparse rural areas: each super-cell keeps its top features up to an allocation proportional to population^(1/gamma). BIGGER protects sparse areas more. Does not change per-level totals. No effect with --no-density-drop

  Default value: `1.5`
* `--no-density-drop` — Disable the per-level density budget entirely, reverting to pure cell-winner thinning
* `--cluster` — Enable point clustering (duplicating mode only; opt-in). At each level the surviving point in each thinning cell absorbs the others and the output gains a `point_count` INT64 column recording how many source features each row represents. Lines and polygons carry point_count = 1. Use for graduated-dot rendering of dense point data
* `--accumulate-attribute <COL:OP>` — Aggregate a numeric column across clustered points: COL:OP where OP is sum, max, min, or mean. Repeatable. Requires --cluster. The winner's COL becomes the aggregate over itself and the points it absorbed at each level (mean is exact). Example: --accumulate-attribute population:sum
* `--no-coalesce-lines` — Disable line network coalescing (ON by default; duplicating mode). By default, touching same-class line segments are chained into single "stroke" LineStrings before the visibility gate and thinning, so road/ river networks read as continuous lines at coarse zooms. The output gains a `coalesced_count` INT32 column. Inert in partitioning mode
* `--coalesce-junction-angle <DEG>` — Junction continuation angle for line coalescing, in degrees (default 0 = OFF: junctions terminate chains, preserving network topology). When > 0, the pair of lines at a junction that best continue each other merge if their deviation from straight is at most this angle. BIGGER bends chains further through junctions (longer strokes; risk of merging real turns)

  Default value: `0.0`
* `--coalesce-snap <F>` — Endpoint snap tolerance for line coalescing, in GSD multiples (default 1.0). Exactly-touching endpoints always chain; this additionally joins chain ends within factor * gsd of each other. BIGGER bridges larger digitization gaps; 0 = exact endpoint matching only

  Default value: `1.0`
* `--coalesce-max-level-rows <ROWS>` — Per-level candidate-line ceiling for line coalescing (memory guard, default 2000000). Chaining holds a level's candidate line geometries in memory; levels with more lines than this skip coalescing with a warning instead of breaking the streaming memory bound

  Default value: `2000000`
* `--row-group-size <ROW_GROUP_SIZE>` — Maximum output row-group size in rows (default 10000). Interpreted per level: a level at or below this size is one row group; a larger level is split into roughly uniform row groups of at most this size

  Default value: `10000`
* `--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` — Per-level row-group sizing policy. `constant` (default): every level uses --row-group-size as its cap. `zoom-scaled`: the cap doubles per zoom step below the finest level, so coarse bands become fewer/larger row groups (fewer remote requests) while the finest level keeps tight bbox pruning

  Default value: `constant`

  Possible values: `constant`, `zoom-scaled`

* `--full-column-stats` — Keep full Parquet statistics on every column, including high-cardinality string/binary properties and the WKB geometry column. By default those stats are suppressed to keep the footer small; the bbox covering and `level` column always keep pruning stats. Enable this if remote clients push predicates on property columns
* `--no-streaming` — Disable the two-pass bounded-memory streaming pipeline. By default the converter streams the input twice so peak memory is O(read batch + winner tables) instead of O(dataset); output is equivalent either way. This flag reverts to the in-memory pipeline, which may be marginally faster on small inputs that fit in RAM
* `--read-batch-size <ROWS>` — Rows per Arrow read batch in the streaming pipeline (default 8192). LARGER batches are slightly faster at more peak memory; SMALLER bound memory tighter. No effect with --no-streaming

  Default value: `8192`
* `--profile <PROFILE>` — Memory/throughput profile for the pass-2 engine. `speed` buffers each output level's rows in RAM (fastest); `bounded` spills them to temporary Arrow IPC files (memory-capped); `auto` (default) spills when estimated output exceeds a fraction of available RAM (override with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Byte-identical across profiles. No effect with --no-streaming

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches allowed in flight through the pass-2 pipeline at once (read/compute overlap). `auto` (default) sizes this to available cores (clamped to 4..=16); pass an integer to override. Higher improves core utilization at more peak memory. No effect with --no-streaming

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for the remote-input spill file. A remote convert stages every fetched column chunk in a temp file (growing to ≈1x the touched input bytes) so later passes re-read from local disk instead of the network; it defaults to $TMPDIR. The directory must exist; local inputs never spill. On `tiles` this directory also hosts the removed-after-export intermediate overview unless --keep-overview is given



## `tylertoo overview`

Build a multi-resolution overview GeoParquet file

**Usage:** `tylertoo overview [OPTIONS] [INPUT] [OUTPUT]`

###### **Arguments:**

* `<INPUT>` — Input GeoParquet (EPSG:4326 or EPSG:3857): a local file, a directory or glob of partitions, or a remote URL (s3://, https://, gs://; trailing- slash prefixes are listed to their .parquet objects). Remote inputs are read with byte-range requests. Omit when --files-from is given
* `<OUTPUT>` — Output overview GeoParquet file

###### **Options:**

* `--files-from <PATH>` — Convert the inputs listed in this manifest instead of a positional INPUT: one .parquet path or remote URL per line (no directories, globs, or prefixes); `#` comments and blank lines are skipped; line order is preserved verbatim as the dataset row order. Usage: --files-from PATH OUTPUT
* `--mode <MODE>` — Level materialization mode

  Default value: `duplicating`

  Possible values: `duplicating`, `partitioning`

* `--min-zoom <MIN_ZOOM>` — Minimum (coarsest) Web Mercator zoom for the level range

  Default value: `0`
* `--max-zoom <MAX_ZOOM>` — Maximum (finest / canonical) Web Mercator zoom for the level range

  Default value: `6`
* `--gsd <GSDS>` — Explicit comma-separated GSD list (meters, strictly decreasing). Overrides --min-zoom/--max-zoom when set
* `--bbox <XMIN,YMIN,XMAX,YMAX>` — Regional extract: only convert features whose bbox intersects this bounding box (lon/lat degrees: xmin,ymin,xmax,ymax). Row groups are pruned via GeoParquet 1.1 covering statistics when present
* `--cogp-compat` — Emit the optional COGP compatibility footer key (partitioning mode)
* `--report <PATH>` — Write the JSON conversion report to this path
* `--sort-key <COL>` — Column name used as the cell-winner priority (sort) key. Mutually exclusive with --class-rank
* `--class-rank <SPEC>` — Categorical class ranking (higher priority wins a cell). Format: `COLUMN:VALUE=RANK,VALUE=RANK,...` — e.g. `--class-rank road_class:motorway=5,primary=4,residential=2`. Present-but-unlisted values rank below every listed value (but above nulls). Mutually exclusive with --sort-key
* `--no-auto-rank` — Disable auto-detection of well-known schemas (Overture roads `class`/ `road_class`, Overture places `confidence`)
* `--filter <EXPR>` — Attribute filter: only convert features matching this SQL-WHERE-style predicate over the input's property columns, e.g. "confidence > 0.8" or "crop_type IN ('soy', 'corn')". Supports =, !=, <, <=, >, >=, IN (...), IS [NOT] NULL, AND/OR/NOT, parentheses, string/ numeric literals, "quoted columns", and 'YYYY-MM-DD'/RFC 3339 timestamps (UTC). Nulls follow SQL three-valued logic. Aliased as --where
* `--gsd-base <F>` — GSD tile-band base for the zoom→GSD mapping (default 1024): gsd(z) = 40075016.69 / base / 2^z. Master detail knob for a zoom-range plan — a LARGER base means smaller GSDs (denser, more detailed levels), a SMALLER base means larger GSDs (sparser, cheaper levels). No effect when --gsd is given

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance factor: RDP tolerance = factor * gsd (meters), duplicating mode only (default 1.0). LOWER keeps more vertices (crisper, heavier coarse levels); HIGHER sheds more (cruder, lighter). The finest level is always verbatim. Features whose bbox diagonal falls below the tolerance are dropped entirely

  Default value: `1.0`
* `--collapse` — Collapse below-visibility polygons to a representative point instead of dropping them (opt-in). Changes geometry type at coarse levels, so fill styles ignore the points — add a circle layer, or use --collapse-square to stay type-preserving
* `--collapse-square` — Collapse below-visibility polygons to a ~1xGSD placeholder SQUARE at the representative point instead of dropping them (opt-in). Squares are area-dithered (a polygon of area A below threshold T survives as a T-area square with probability A/T) so aggregate area stays truthful. Type- preserving (output stays Polygon), unlike --collapse
* `--representation <SPEC>` — Zoom-band representation selector: comma-separated LO-HI:KIND bands, e.g. "0-7:point,8-14:geom" or "0-5:square". KIND is geom (default), point (polygons become representative-point centroids), or square (below-tolerance polygons emit area-dithered placeholder squares). Bands must not overlap; non-geom bands must end before --max-zoom; point bands must be contiguous from the coarsest zoom. Requires a zoom-range plan (not --gsd) and duplicating mode
* `--no-cascade` — Disable cascading simplification and reproduce the pre-cascade output byte-for-byte. By default each coarser level is simplified from the next-finer level's already-simplified output (faster on duplicating mode; coarse coordinates differ by up to ~2x the level tolerance)
* `--point-thinning <POINT_THINNING>` — Point thinning factor: grid cell size = factor * gsd (default 4.0, or 16.0 with --cluster). One feature survives per grid cell per level, so BIGGER = sparser, SMALLER = denser
* `--line-thinning <LINE_THINNING>` — Line thinning factor: grid cell size = factor * gsd (default 1.0). BIGGER = sparser (fewer lines survive per level), SMALLER = denser. The roads/line counterpart to --point-thinning

  Default value: `1.0`
* `--polygon-thinning <POLYGON_THINNING>` — Polygon thinning factor: grid cell size = factor * gsd (default 1.0). BIGGER = sparser, SMALLER = denser

  Default value: `1.0`
* `--line-visibility <LINE_VISIBILITY>` — Line visibility gate in GSD multiples: a line is eligible at a level only if its bbox diagonal >= factor * gsd (default 2.0). A hard drop, not a thin: BIGGER drops more small lines (sparser), SMALLER keeps more

  Default value: `2.0`
* `--polygon-visibility <POLYGON_VISIBILITY>` — Polygon visibility gate in GSD multiples: a polygon is eligible only if its bbox diagonal >= factor * gsd (default 2.0). BIGGER drops more small polygons (sparser), SMALLER keeps more. Use --collapse to keep dropped polygons as representative points

  Default value: `2.0`
* `--drop-rate <F>` — Per-level density drop rate: each coarser level keeps 1/rate of the next finer level's feature budget (default 1.65). After cell-winner thinning, each level is capped at budget(L) = N / rate^(finest-L) (N = input feature count) and the lowest-priority survivors are dropped to meet it. BIGGER = sparser mid-zooms and smaller files, SMALLER = gentler. The finest level is never dropped

  Default value: `1.65`
* `--drop-gamma <F>` — Spatial-fairness strength for the density budget (default 1.5). The budget is shared across coarse super-cells so a global cut cannot empty sparse rural areas: each super-cell keeps its top features up to an allocation proportional to population^(1/gamma). BIGGER protects sparse areas more. Does not change per-level totals. No effect with --no-density-drop

  Default value: `1.5`
* `--no-density-drop` — Disable the per-level density budget entirely, reverting to pure cell-winner thinning
* `--cluster` — Enable point clustering (duplicating mode only; opt-in). At each level the surviving point in each thinning cell absorbs the others and the output gains a `point_count` INT64 column recording how many source features each row represents. Lines and polygons carry point_count = 1. Use for graduated-dot rendering of dense point data
* `--accumulate-attribute <COL:OP>` — Aggregate a numeric column across clustered points: COL:OP where OP is sum, max, min, or mean. Repeatable. Requires --cluster. The winner's COL becomes the aggregate over itself and the points it absorbed at each level (mean is exact). Example: --accumulate-attribute population:sum
* `--no-coalesce-lines` — Disable line network coalescing (ON by default; duplicating mode). By default, touching same-class line segments are chained into single "stroke" LineStrings before the visibility gate and thinning, so road/ river networks read as continuous lines at coarse zooms. The output gains a `coalesced_count` INT32 column. Inert in partitioning mode
* `--coalesce-junction-angle <DEG>` — Junction continuation angle for line coalescing, in degrees (default 0 = OFF: junctions terminate chains, preserving network topology). When > 0, the pair of lines at a junction that best continue each other merge if their deviation from straight is at most this angle. BIGGER bends chains further through junctions (longer strokes; risk of merging real turns)

  Default value: `0.0`
* `--coalesce-snap <F>` — Endpoint snap tolerance for line coalescing, in GSD multiples (default 1.0). Exactly-touching endpoints always chain; this additionally joins chain ends within factor * gsd of each other. BIGGER bridges larger digitization gaps; 0 = exact endpoint matching only

  Default value: `1.0`
* `--coalesce-max-level-rows <ROWS>` — Per-level candidate-line ceiling for line coalescing (memory guard, default 2000000). Chaining holds a level's candidate line geometries in memory; levels with more lines than this skip coalescing with a warning instead of breaking the streaming memory bound

  Default value: `2000000`
* `--row-group-size <ROW_GROUP_SIZE>` — Maximum output row-group size in rows (default 10000). Interpreted per level: a level at or below this size is one row group; a larger level is split into roughly uniform row groups of at most this size

  Default value: `10000`
* `--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` — Per-level row-group sizing policy. `constant` (default): every level uses --row-group-size as its cap. `zoom-scaled`: the cap doubles per zoom step below the finest level, so coarse bands become fewer/larger row groups (fewer remote requests) while the finest level keeps tight bbox pruning

  Default value: `constant`

  Possible values: `constant`, `zoom-scaled`

* `--full-column-stats` — Keep full Parquet statistics on every column, including high-cardinality string/binary properties and the WKB geometry column. By default those stats are suppressed to keep the footer small; the bbox covering and `level` column always keep pruning stats. Enable this if remote clients push predicates on property columns
* `--no-streaming` — Disable the two-pass bounded-memory streaming pipeline. By default the converter streams the input twice so peak memory is O(read batch + winner tables) instead of O(dataset); output is equivalent either way. This flag reverts to the in-memory pipeline, which may be marginally faster on small inputs that fit in RAM
* `--read-batch-size <ROWS>` — Rows per Arrow read batch in the streaming pipeline (default 8192). LARGER batches are slightly faster at more peak memory; SMALLER bound memory tighter. No effect with --no-streaming

  Default value: `8192`
* `--profile <PROFILE>` — Memory/throughput profile for the pass-2 engine. `speed` buffers each output level's rows in RAM (fastest); `bounded` spills them to temporary Arrow IPC files (memory-capped); `auto` (default) spills when estimated output exceeds a fraction of available RAM (override with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Byte-identical across profiles. No effect with --no-streaming

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches allowed in flight through the pass-2 pipeline at once (read/compute overlap). `auto` (default) sizes this to available cores (clamped to 4..=16); pass an integer to override. Higher improves core utilization at more peak memory. No effect with --no-streaming

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for the remote-input spill file. A remote convert stages every fetched column chunk in a temp file (growing to ≈1x the touched input bytes) so later passes re-read from local disk instead of the network; it defaults to $TMPDIR. The directory must exist; local inputs never spill. On `tiles` this directory also hosts the removed-after-export intermediate overview unless --keep-overview is given



## `tylertoo validate`

Validate a GeoParquet overview file against the spec (§6.2)

**Usage:** `tylertoo validate <FILE>`

###### **Arguments:**

* `<FILE>` — GeoParquet overview file to validate



## `tylertoo export-pmtiles`

Export a PMTiles archive from an overview GeoParquet file (Plan E0)

**Usage:** `tylertoo export-pmtiles [OPTIONS] <INPUT> <OUTPUT>`

###### **Arguments:**

* `<INPUT>` — Input overview GeoParquet file (produced by `tylertoo overview`)
* `<OUTPUT>` — Output PMTiles archive

###### **Options:**

* `--layer-name <LAYER_NAME>` — MVT layer name written into every tile

  Default value: `overview`
* `--tile-buffer <TILE_BUFFER>` — Per-tile edge buffer, in tile pixels (feature seam continuity)

  Default value: `8`
* `--tile-size-limit <SIZE>` — Per-tile MVT size cap (e.g., "500K", "1M", or raw bytes; default 500K). When a tile exceeds it, a single drop pass sheds features (largest-first for polygons/lines; a uniform spatial stride for point tiles). Pass 0 to disable. Aliased as --max-tile-size

  Default value: `500K`
* `--report <PATH>` — Write the JSON export report to this path
* `--no-simple-clip-fastpath` — Force the i_overlay boundary-bridge fallback on every polygon clip, disabling the default simple-clip fast path. Use only when you need byte-stable tile output (the fast path rotates simple rings to a different start vertex)
* `--partition-wave <N|auto>` — Partitions processed per band read during export (export concurrency). `auto` (default) sizes a memory budget from core count and available RAM (override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES); pass an integer to override. Wider waves use more cores and more peak memory. Output is byte-identical for every value

  Default value: `auto`



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
  - lossy properties: attributes dropped during tiling cannot be
    recovered
There is no round-trip guarantee: A.parquet -> B.pmtiles -> C.parquet
does not reproduce A. See docs/diving-deeper/decoding.md for details.

###### **Arguments:**

* `<INPUT>` — Input PMTiles archive (vector tiles)
* `<OUTPUT>` — Output GeoParquet file

###### **Options:**

* `--zoom <ZOOM>` — Decode a single zoom level (recommended for most uses)
* `--min-zoom <MIN_ZOOM>` — Minimum zoom level to decode
* `--max-zoom <MAX_ZOOM>` — Maximum zoom level to decode
* `--layer <NAME>` — Only decode features from this MVT layer
* `--report <PATH>` — Write the JSON decode report to this path



