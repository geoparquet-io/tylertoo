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

* `<INPUT>` — Input GeoParquet (EPSG:4326 or EPSG:3857): a local file, a directory or glob of partitions, or a remote URL (s3://, https://, gs://). s3://.../ and gs://.../ prefixes (trailing slash) are listed to their .parquet objects; remote inputs are read with byte-range requests. Omit when --files-from is given (then the one positional is OUTPUT)
* `<OUTPUT>` — Output PMTiles file

###### **Options:**

* `--files-from <PATH>` — Convert the inputs listed in this manifest instead of a positional INPUT: one local path or remote URL per line; `#` comment lines and blank lines are skipped; line order is preserved VERBATIM (it defines the dataset row order). Each line must be a single .parquet file/object — no directories, globs, or prefixes. Local and remote entries may be mixed. Usage: --files-from <PATH> OUTPUT
* `--min-zoom <MIN_ZOOM>` — Minimum (coarsest) Web Mercator zoom level

  Default value: `0`
* `--max-zoom <MAX_ZOOM>` — Maximum (finest) Web Mercator zoom level

  Default value: `14`
* `--gsd <GSDS>` — Explicit comma-separated GSD list (meters, strictly decreasing). Overrides --min-zoom/--max-zoom when set — the same semantics as `tylertoo overview --gsd`, so the absolute-GSD ladder is reachable in one step
* `--bbox <XMIN,YMIN,XMAX,YMAX>` — Regional extract: only convert features whose bbox intersects this bounding box (lon/lat degrees: xmin,ymin,xmax,ymax). See --bbox in `tylertoo overview --help` for details
* `--layer-name <LAYER_NAME>` — Layer name for the output tiles (default: derived from input filename)
* `--max-tile-size <SIZE>` — Maximum tile size (e.g., "500K", "1M", or raw bytes). When a tile exceeds this limit, the export sheds features in a single non-iterative pass (largest-first for polygons/lines; a uniform spatial stride for point tiles). Defaults to 500K (tippecanoe parity, #280); pass 0 to disable the cap. Aliased as --tile-size-limit for parity with `export-pmtiles`

  Default value: `500K`
* `--no-simple-clip-fastpath` — Disable the simple-clip fast path (issue #239), forcing the i_overlay boundary-bridge fallback on every polygon clip. The fast path is on by default (render-equivalent on simple rings); pass this only when you need byte-stable tile output, since the fast path rotates simple rings to a different start vertex
* `--tile-buffer <TILE_BUFFER>` — Per-tile edge buffer, in tile pixels, carried across tile seams so features don't clip at boundaries

  Default value: `8`
* `--partition-wave <N|auto>` — Partitions processed per band read during the export phase (the export concurrency knob). `auto` (the default) preflights a memory budget: the machine's core count, capped by how many estimated per-partition transients fit in a fraction of available RAM (floor 6; fixed cap 16 only when RAM cannot be probed; override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Pass an explicit integer to override. Wider waves keep more cores busy at proportionally more peak memory (one wave of partitions resident). The chosen width and the preflight inputs are logged at export start. Output is byte-identical for every value

  Default value: `auto`
* `--report <PATH>` — Write a JSON report to this path: a combined object with a `convert` section (the overview build, matching `overview --report`) and an `export` section (the PMTiles export, matching `export-pmtiles --report`), so the one-step run captures both halves the two-step chain would
* `--keep-overview <PATH>` — Write the intermediate overview GeoParquet to PATH and RETAIN it, instead of a temp file removed after the export — one run then yields both artifacts: the reusable multi-resolution overview (queryable, re-exportable, see `tylertoo overview`) and the PMTiles. The PMTiles output is identical either way. Without this flag the intermediate is written to --spill-dir if given, else $TMPDIR if set, else the output directory, and deleted once the export finishes (see the note on the materialized intermediate under --spill-dir)
* `-v`, `--verbose` — Enable verbose output (per-level and per-zoom breakdowns)
* `--sort-key <COL>` — Column name used as the cell-winner priority (sort) key. Mutually exclusive with --class-rank
* `--class-rank <SPEC>` — Categorical class ranking (higher priority wins a cell). Format: `COLUMN:VALUE=RANK,VALUE=RANK,...` — e.g. `--class-rank road_class:motorway=5,primary=4,residential=2`. Present-but-unlisted values rank below every listed value (but above nulls). Mutually exclusive with --sort-key
* `--no-auto-rank` — Disable auto-detection of well-known schemas (Overture roads `class`/ `road_class`, Overture places `confidence`)
* `--filter <EXPR>` — Attribute filter: only convert features matching this SQL-WHERE-style predicate over the input's property columns, e.g. "confidence > 0.8", "crop_type IN ('soy', 'corn')", "note IS NOT NULL AND (class = 'a' OR class = 'b')". Supports =, !=, <, <=, >, >=, IN (...), IS [NOT] NULL, AND/OR/NOT, parentheses, 'string' and numeric literals, and "quoted column" names; timestamp columns compare against 'YYYY-MM-DD' / 'YYYY-MM-DD HH:MM:SS' / RFC 3339 datetime strings (read as UTC); nulls follow SQL three-valued logic (a row is kept only when the predicate is TRUE). Evaluated during the pass-1 scan, so it composes with --bbox; input row groups whose parquet column statistics preclude any match are skipped at the footer level (on remote input those byte ranges are never fetched). Aliased as --where. See docs/OVERVIEW_TUNING.md
* `--gsd-base <F>` — GSD tile-band base for the zoom→GSD mapping: gsd(z) = 40075016.69 / base / 2^z (spec §5.2, cogp-rs default 1024).

   This is the master detail knob for a zoom-range plan. A LARGER base makes every level's GSD SMALLER, so less is thinned and simplified at a given zoom (denser, more detailed, larger coarse levels). A SMALLER base makes GSDs LARGER (sparser, cruder, cheaper coarse levels). It scales the whole ladder at once, whereas --simplify-factor and the --*-thinning knobs act relative to each level's GSD. No effect when --gsd is given (those GSDs are already absolute meters).

   Cheat sheet: coarse levels too sparse → RAISE --gsd-base (or lower the thinning factors); too crude → lower --simplify-factor. See docs/OVERVIEW_TUNING.md.

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance factor: RDP tolerance = factor * gsd (meters), duplicating mode only (default 1.0).

   Controls how much per-feature vertex detail each coarse level sheds. LOWER = smoother/less aggressive = more vertices kept = crisper but heavier levels; HIGHER = cruder = fewer vertices = lighter levels. The canonical (finest) level is always verbatim regardless. A line/polygon whose bbox diagonal is below the tolerance is dropped entirely, so a very high factor also thins features, not just vertices.

   Cheat sheet: coarse levels look too crude/blocky → LOWER --simplify-factor. See docs/OVERVIEW_TUNING.md.

  Default value: `1.0`
* `--collapse` — Collapse below-visibility polygons to a representative point instead of dropping them (spec Q4 opt-in). Changes the geometry type at coarse levels (fill-styled renderers silently ignore points — add a circle layer, or use --collapse-square to stay type-preserving)
* `--collapse-square` — Collapse below-visibility polygons to a ~1xGSD placeholder SQUARE at the representative point instead of dropping them (tippecanoe tiny-polygon reduction; opt-in).

   Squares are area-dithered: a polygon of area A below the level threshold T = (simplify-factor * gsd)^2 survives as a T-area square with probability A/T, so aggregate area stays truthful — dense city blocks read denser than isolated barns. Type-preserving (the output stays Polygon), so plain fill styles keep working, unlike --collapse. Deterministic per feature (same input -> same output, engine- and thread-independent). See docs/OVERVIEW_TUNING.md.
* `--representation <SPEC>` — Zoom-band representation selector: comma-separated LO-HI:KIND bands, e.g. "0-7:point,8-14:geom" or "0-5:square". KIND is geom, point, or square.

   point: ALL polygonal features in the band become representative points (centroid) — "dots zoomed out, polygons zoomed in" in ONE archive, no two-archive merge. In-band polygons bypass the visibility gate (a dot is always visible) and thin on the point grid. square: below-tolerance polygons in the band emit area-dithered ~1xGSD placeholder squares (see --collapse-square) instead of dropping; visible polygons are untouched. geom: normal (the default for unlisted zooms). Bands must not overlap, non-geom bands must end before --max-zoom (the canonical level is always verbatim), and point bands must be contiguous from the coarsest zoom. Requires a zoom-range plan (not --gsd) and duplicating mode. Lines and native points are unaffected by every band kind. See docs/OVERVIEW_TUNING.md.
* `--no-cascade` — Disable cascading simplification (#218) and reproduce the pre-cascade output byte-for-byte.

   By default each coarser level is simplified from the next-finer level's already-simplified output (tippecanoe-style) and invalid RDP candidates are repaired via a boolean overlay instead of epsilon- retried — much faster on duplicating mode, at the cost of coarse-level coordinates differing slightly from the non-cascaded pipeline (bounded by ~2x the level tolerance). See docs/OVERVIEW_TUNING.md.
* `--point-thinning <POINT_THINNING>` — Point thinning factor: grid cell size = factor * gsd.

   Default 4.0, or 16.0 when --cluster is enabled (absorbed points are summarized via point_count rather than dropped, so a coarser grid gives the familiar graduated-cluster look; chosen from the NYC pt={4,16,48} sweep).

   One feature survives per grid cell per level, so BIGGER factor = BIGGER cells = FEWER survivors = SPARSER map; SMALLER = denser. This multiplies the GSD cell size, so it interacts with --gsd-base (which sets the GSD).

   Cheat sheet: coarse levels too sparse → LOWER the thinning factors.
* `--line-thinning <LINE_THINNING>` — Line thinning factor: grid cell size = factor * gsd (default 1.0).

   BIGGER = SPARSER (fewer lines survive per level), SMALLER = denser. See --point-thinning; this is the roads/line knob. Default retuned 2.0 -> 1.0 after the Portland sweep (corpus/SWEEPS.md): 1.0 keeps road networks visibly more continuous at coarse zooms.

  Default value: `1.0`
* `--polygon-thinning <POLYGON_THINNING>` — Polygon thinning factor: grid cell size = factor * gsd (default 1.0).

   BIGGER = SPARSER, SMALLER = denser. Polygons thin least by default (1.0) since they tile space rather than cluster.

  Default value: `1.0`
* `--line-visibility <LINE_VISIBILITY>` — Line visibility gate in GSD multiples: a line is eligible at a level only if its bbox diagonal >= factor * gsd (default 2.0).

   This is a hard drop, not a thin: BIGGER = more small lines dropped at coarse levels (sparser); SMALLER = more small lines kept. The gate is multiplied by the level GSD, so --gsd-base moves it too.

  Default value: `2.0`
* `--polygon-visibility <POLYGON_VISIBILITY>` — Polygon visibility gate in GSD multiples: a polygon is eligible only if its bbox diagonal >= factor * gsd (default 2.0).

   BIGGER = more small polygons dropped at coarse levels (sparser); SMALLER = more kept. See --line-visibility. Retuned 4.0 -> 2.0 in the #259 coarse-zoom sweep (corpus/SWEEPS.md Decision 6): write-time RDP already drops polygons that simplify below the level tolerance, so gates above 2.0 starve coarse zooms without making files smaller, and gates below ~2.0 mostly admit candidates that RDP drops anyway (use --collapse to keep those as representative points).

  Default value: `2.0`
* `--drop-rate <F>` — Per-level density drop rate: each coarser level keeps 1/rate of the next finer level's feature budget (default 1.65).

   This is the Q2 knob that stops mid-zoom counts plateauing at ~everything. Cell-winner thinning stops binding once its grid cell is smaller than the typical feature spacing, so from ~z9 up every feature survives and coarse levels over-retain (Portland roads: ours/tippecanoe ≈ 2–3x at z9–z11). After cell-winner thinning, each level is capped at a budget that decays geometrically toward coarse zooms — budget(L) = N / rate^(finest−L), where N is the input feature count — and the lowest-priority survivors (same class-rank → size → hash order as the cell-winner, spec Q1) are dropped until the level meets its budget. Levels already sparser than their budget (the coarse zooms) are untouched, so this only bites the mid-zoom plateau. BIGGER rate = coarser levels shed harder (sparser mid zooms, smaller files); SMALLER = gentler. The default 1.65 is smaller than tippecanoe's nominal 2.5 because our budget anchors on the full canonical count N (every feature appears at the finest level), not a per-tile basezoom count. The canonical (finest) level is never dropped. See docs/OVERVIEW_TUNING.md and corpus/SWEEPS.md.

  Default value: `1.65`
* `--drop-gamma <F>` — Spatial-fairness strength for the density budget (default 1.5).

   The budget is shared across coarse super-cells (neighborhoods) so a global rank-ordered cut cannot empty sparse rural areas to keep dense cities under budget. Each super-cell keeps its top-priority features up to an allocation proportional to population^(1/gamma): gamma=1 is a proportional cut (every neighborhood keeps the same fraction); gamma>1 is SUBLINEAR — dense neighborhoods keep proportionally fewer, sparse ones proportionally more (they are protected). This is tippecanoe's gamma dot-dropping ("reduce dots to the 1/gamma power in dense areas") applied per super-cell. BIGGER = more protection for sparse areas / harder relative thinning of dense areas. Does not change per-level totals (it only redistributes which features survive spatially), so it is independent of --drop-rate. No effect when --no-density-drop is set.

  Default value: `1.5`
* `--no-density-drop` — Disable the Q2 per-level density budget entirely (off switch).

   Reverts to pure cell-winner thinning — the pre-Q2 behavior — and emits a byte-identical footer (no density_drop provenance). Use this to compare before/after, or when the cell-winner thinning already meets your needs.
* `--cluster` — Enable point clustering (duplicating mode only; opt-in).

   At each overview level, the surviving point in each thinning grid cell ABSORBS the other points in its cell instead of them simply vanishing: the output gains a `point_count` INT64 NOT NULL column recording how many source features each row represents at its level (tippecanoe / supercluster convention; always 1 at the canonical level). The winner keeps its own geometry and attribute values. Lines and polygons are unaffected (their rows carry point_count = 1). Use for graduated-dot rendering of dense point data. See docs/OVERVIEW_TUNING.md.
* `--accumulate-attribute <COL:OP>` — Aggregate a numeric column across clustered points: COL:OP where OP is sum, max, min, or mean. Repeatable. Requires --cluster.

   At each level the winner's value of COL becomes the aggregate over itself + the points it absorbed at that level (computed per level from SOURCE values — mean is exact, never a mean of means). All other columns keep the winner's own values. Example: --accumulate-attribute population:sum --accumulate-attribute confidence:mean
* `--no-coalesce-lines` — Disable line network coalescing (ON by default; duplicating mode).

   By default, at each non-canonical level touching same-class line segments are chained into single "stroke" LineStrings BEFORE the visibility gate and thinning run, so a chain of individually sub-visibility fragments survives as one long, connected artery — road/river networks read as continuous lines at coarse zooms instead of scattered dashes. Chains never merge across class values (when a class ranking is active); junctions continue only within --coalesce-junction-angle of straight. The merged feature keeps the attributes of its highest-priority member, and the output gains a `coalesced_count` INT32 NOT NULL column (source segments merged per row; 1 for unmerged rows and everywhere at the canonical level). Points and polygons are unaffected. In partitioning mode coalescing is inert (a merged chain cannot satisfy the feature-once/verbatim contract). See docs/OVERVIEW_TUNING.md.
* `--coalesce-junction-angle <DEG>` — Junction continuation angle for line coalescing, in degrees (default 0 = OFF: junctions terminate chains, preserving network topology — chosen from the Portland junction-angle sweep in corpus/data/bench/q3/, where strict degree-2 chaining rendered better).

   When > 0: at a junction (3+ same-class segment endpoints meeting), the pair of lines that best continue each other merge when their deviation from a straight continuation is at most this angle — best pair first, so a 4-way crossing continues BOTH through-streets. BIGGER = chains bend further through junctions (longer, fewer strokes; risk of merging through genuine turns).

  Default value: `0.0`
* `--coalesce-snap <F>` — Endpoint snap tolerance for line coalescing, in GSD multiples (default 1.0).

   Exactly-touching endpoints always chain; this knob additionally joins chain ends within factor * gsd of each other (two endpoints closer than one ground sample are indistinguishable at that level). BIGGER = bridges larger digitization gaps (risk: rungs of nearby parallel lines fusing); 0 = exact endpoint matching only.

  Default value: `1.0`
* `--coalesce-max-level-rows <ROWS>` — Per-level candidate-line ceiling for line coalescing (memory guard).

   Chaining holds the level's candidate line geometries in memory at once (every line is a candidate at every non-canonical level, since sub-visibility fragments must be reclaimable). Datasets with more lines than this skip coalescing with a warning instead of breaking the streaming pipeline's memory bound; near-canonical levels that large need coalescing least (segments are individually visible).

  Default value: `2000000`
* `--row-group-size <ROW_GROUP_SIZE>` — Maximum output row-group size in rows.

   Interpreted per level: a level with at most this many rows is written as a single row group; a larger level is split into roughly uniform row groups of at most this size. Coarse bands (few features) therefore become one broad row group; fine bands keep tight per-row-group bbox statistics.

  Default value: `10000`
* `--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` — Per-level row-group sizing policy (#202).

   `constant`: every level uses --row-group-size as its cap (default). `zoom-scaled`: the cap doubles per zoom step below the finest level (cap = row_group_size << (max_zoom - level_zoom)) — coarse bands, which wide viewports read mostly whole anyway, become fewer/larger row groups (fewer remote requests) while the finest level keeps tight bbox pruning.

  Default value: `constant`

  Possible values: `constant`, `zoom-scaled`

* `--full-column-stats` — Keep full Parquet statistics on every column, including high-cardinality string/binary property columns and the WKB geometry column.

   By default those columns' per-row-group min/max stats are suppressed to keep the footer small (a 26-char ULID `id` over hundreds of row groups otherwise bloats the footer to megabytes, paid on every remote query). The bbox covering and `level` column always keep their pruning stats. Enable this if remote clients push predicates on property columns and want row-group skipping on them.
* `--no-streaming` — Disable the two-pass bounded-memory streaming pipeline (H3).

   By default the converter streams the input twice: pass 1 builds the per-feature winner tables (level assignment + density budget) holding only bboxes/kinds/sort-keys; pass 2 re-reads the input per level and simplifies + writes batch-by-batch. Peak memory is O(read batch + winner tables) instead of O(dataset) — e.g. Moldova (632k polygons) drops from ~5.4 GB to well under 1 GB peak RSS. Output is equivalent (same level assignments, rows, and footer). This flag reverts to the original in-memory pipeline, which decodes the whole dataset once and may be marginally faster on small inputs that comfortably fit in RAM.
* `--read-batch-size <ROWS>` — Rows per Arrow read batch in the streaming pipeline (both passes).

   LARGER batches amortize per-batch overhead (slightly faster) at the cost of proportionally more peak memory; SMALLER batches bound memory tighter. The default (8192) keeps per-batch transients in the tens of MB even for vertex-heavy polygon data. No effect with --no-streaming.

  Default value: `8192`
* `--profile <PROFILE>` — Memory/throughput profile for the single-read pass-2 engine (#213/#212).

   `speed` buffers each output level's rows in RAM (fastest; peak RAM grows with buffered output). `bounded` spills them to temporary Arrow IPC files (memory-capped; slight temp-I/O cost). `auto` (default) is workload-based: it estimates buffered output from feature and level counts and spills when that exceeds a fraction of available RAM, so large duplicating runs prefer bounded instead of risking OOM (override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Output is byte-identical across profiles. No effect with --no-streaming.

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches allowed in flight through the pass-2 pipeline at once (read/compute-overlap knob; bounded-channel depth).

   `auto` (the default) sizes this to the machine's available cores (clamped to 4..=16); pass an explicit integer to override. Higher improves core utilization on long-pole geometries at proportionally more peak memory (in-flight-batches × read-batch-size rows resident). The chosen depth and detected core count are logged at pass-2 start. No effect with --no-streaming.

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for the remote-input spill file (issues #219/#272).

   A remote convert stages every fetched column chunk in an anonymous temp file — growing to ≈1× the touched input bytes (the whole object for a full-file convert; only the covering row groups with --bbox) — so later passes re-read from local disk instead of the network. By default it lives under the process temp dir ($TMPDIR); point this at a volume with enough room (a free-space preflight warns about a projected shortfall). The directory must exist. Local inputs never spill.

   On `tiles` this directory also hosts the removed-after-export intermediate overview (#314) — at least input-sized, with its own free-space preflight — unless --keep-overview is given (then the intermediate goes to that path instead). Location precedence for the intermediate: --spill-dir, $TMPDIR, the output directory.



## `tylertoo overview`

Build a multi-resolution overview GeoParquet file

**Usage:** `tylertoo overview [OPTIONS] [INPUT] [OUTPUT]`

###### **Arguments:**

* `<INPUT>` — Input GeoParquet (EPSG:4326 or EPSG:3857): a local file, a directory or glob of partitions, or a remote URL (s3://, https://, gs://). s3://.../ and gs://.../ prefixes (trailing slash) are listed to their .parquet objects. Remote inputs are read with byte-range requests; with --bbox, only the matching row groups are ever downloaded. Omit when --files-from is given (then the one positional is OUTPUT)
* `<OUTPUT>` — Output overview GeoParquet file

###### **Options:**

* `--files-from <PATH>` — Convert the inputs listed in this manifest instead of a positional INPUT: one local path or remote URL per line; `#` comment lines and blank lines are skipped; line order is preserved VERBATIM (it defines the dataset row order). Each line must be a single .parquet file/object — no directories, globs, or prefixes. Local and remote entries may be mixed. Usage: --files-from <PATH> OUTPUT
* `--mode <MODE>` — Level materialization mode

  Default value: `duplicating`

  Possible values: `duplicating`, `partitioning`

* `--min-zoom <MIN_ZOOM>` — Minimum (coarsest) Web Mercator zoom for the level range

  Default value: `0`
* `--max-zoom <MAX_ZOOM>` — Maximum (finest / canonical) Web Mercator zoom for the level range

  Default value: `6`
* `--gsd <GSDS>` — Explicit comma-separated GSD list (meters, strictly decreasing). Overrides --min-zoom/--max-zoom when set
* `--bbox <XMIN,YMIN,XMAX,YMAX>` — Regional extract: only convert features whose bbox intersects this bounding box (lon/lat degrees: xmin,ymin,xmax,ymax). Row groups whose GeoParquet 1.1 covering statistics don't intersect are skipped at the parquet footer level (no data pages read); inputs without covering stats degrade gracefully (all row groups read, exact per-feature filter still applies)
* `--cogp-compat` — Emit the optional COGP compatibility footer key (partitioning mode)
* `--report <PATH>` — Write the JSON conversion report to this path
* `--sort-key <COL>` — Column name used as the cell-winner priority (sort) key. Mutually exclusive with --class-rank
* `--class-rank <SPEC>` — Categorical class ranking (higher priority wins a cell). Format: `COLUMN:VALUE=RANK,VALUE=RANK,...` — e.g. `--class-rank road_class:motorway=5,primary=4,residential=2`. Present-but-unlisted values rank below every listed value (but above nulls). Mutually exclusive with --sort-key
* `--no-auto-rank` — Disable auto-detection of well-known schemas (Overture roads `class`/ `road_class`, Overture places `confidence`)
* `--filter <EXPR>` — Attribute filter: only convert features matching this SQL-WHERE-style predicate over the input's property columns, e.g. "confidence > 0.8", "crop_type IN ('soy', 'corn')", "note IS NOT NULL AND (class = 'a' OR class = 'b')". Supports =, !=, <, <=, >, >=, IN (...), IS [NOT] NULL, AND/OR/NOT, parentheses, 'string' and numeric literals, and "quoted column" names; timestamp columns compare against 'YYYY-MM-DD' / 'YYYY-MM-DD HH:MM:SS' / RFC 3339 datetime strings (read as UTC); nulls follow SQL three-valued logic (a row is kept only when the predicate is TRUE). Evaluated during the pass-1 scan, so it composes with --bbox; input row groups whose parquet column statistics preclude any match are skipped at the footer level (on remote input those byte ranges are never fetched). Aliased as --where. See docs/OVERVIEW_TUNING.md
* `--gsd-base <F>` — GSD tile-band base for the zoom→GSD mapping: gsd(z) = 40075016.69 / base / 2^z (spec §5.2, cogp-rs default 1024).

   This is the master detail knob for a zoom-range plan. A LARGER base makes every level's GSD SMALLER, so less is thinned and simplified at a given zoom (denser, more detailed, larger coarse levels). A SMALLER base makes GSDs LARGER (sparser, cruder, cheaper coarse levels). It scales the whole ladder at once, whereas --simplify-factor and the --*-thinning knobs act relative to each level's GSD. No effect when --gsd is given (those GSDs are already absolute meters).

   Cheat sheet: coarse levels too sparse → RAISE --gsd-base (or lower the thinning factors); too crude → lower --simplify-factor. See docs/OVERVIEW_TUNING.md.

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance factor: RDP tolerance = factor * gsd (meters), duplicating mode only (default 1.0).

   Controls how much per-feature vertex detail each coarse level sheds. LOWER = smoother/less aggressive = more vertices kept = crisper but heavier levels; HIGHER = cruder = fewer vertices = lighter levels. The canonical (finest) level is always verbatim regardless. A line/polygon whose bbox diagonal is below the tolerance is dropped entirely, so a very high factor also thins features, not just vertices.

   Cheat sheet: coarse levels look too crude/blocky → LOWER --simplify-factor. See docs/OVERVIEW_TUNING.md.

  Default value: `1.0`
* `--collapse` — Collapse below-visibility polygons to a representative point instead of dropping them (spec Q4 opt-in). Changes the geometry type at coarse levels (fill-styled renderers silently ignore points — add a circle layer, or use --collapse-square to stay type-preserving)
* `--collapse-square` — Collapse below-visibility polygons to a ~1xGSD placeholder SQUARE at the representative point instead of dropping them (tippecanoe tiny-polygon reduction; opt-in).

   Squares are area-dithered: a polygon of area A below the level threshold T = (simplify-factor * gsd)^2 survives as a T-area square with probability A/T, so aggregate area stays truthful — dense city blocks read denser than isolated barns. Type-preserving (the output stays Polygon), so plain fill styles keep working, unlike --collapse. Deterministic per feature (same input -> same output, engine- and thread-independent). See docs/OVERVIEW_TUNING.md.
* `--representation <SPEC>` — Zoom-band representation selector: comma-separated LO-HI:KIND bands, e.g. "0-7:point,8-14:geom" or "0-5:square". KIND is geom, point, or square.

   point: ALL polygonal features in the band become representative points (centroid) — "dots zoomed out, polygons zoomed in" in ONE archive, no two-archive merge. In-band polygons bypass the visibility gate (a dot is always visible) and thin on the point grid. square: below-tolerance polygons in the band emit area-dithered ~1xGSD placeholder squares (see --collapse-square) instead of dropping; visible polygons are untouched. geom: normal (the default for unlisted zooms). Bands must not overlap, non-geom bands must end before --max-zoom (the canonical level is always verbatim), and point bands must be contiguous from the coarsest zoom. Requires a zoom-range plan (not --gsd) and duplicating mode. Lines and native points are unaffected by every band kind. See docs/OVERVIEW_TUNING.md.
* `--no-cascade` — Disable cascading simplification (#218) and reproduce the pre-cascade output byte-for-byte.

   By default each coarser level is simplified from the next-finer level's already-simplified output (tippecanoe-style) and invalid RDP candidates are repaired via a boolean overlay instead of epsilon- retried — much faster on duplicating mode, at the cost of coarse-level coordinates differing slightly from the non-cascaded pipeline (bounded by ~2x the level tolerance). See docs/OVERVIEW_TUNING.md.
* `--point-thinning <POINT_THINNING>` — Point thinning factor: grid cell size = factor * gsd.

   Default 4.0, or 16.0 when --cluster is enabled (absorbed points are summarized via point_count rather than dropped, so a coarser grid gives the familiar graduated-cluster look; chosen from the NYC pt={4,16,48} sweep).

   One feature survives per grid cell per level, so BIGGER factor = BIGGER cells = FEWER survivors = SPARSER map; SMALLER = denser. This multiplies the GSD cell size, so it interacts with --gsd-base (which sets the GSD).

   Cheat sheet: coarse levels too sparse → LOWER the thinning factors.
* `--line-thinning <LINE_THINNING>` — Line thinning factor: grid cell size = factor * gsd (default 1.0).

   BIGGER = SPARSER (fewer lines survive per level), SMALLER = denser. See --point-thinning; this is the roads/line knob. Default retuned 2.0 -> 1.0 after the Portland sweep (corpus/SWEEPS.md): 1.0 keeps road networks visibly more continuous at coarse zooms.

  Default value: `1.0`
* `--polygon-thinning <POLYGON_THINNING>` — Polygon thinning factor: grid cell size = factor * gsd (default 1.0).

   BIGGER = SPARSER, SMALLER = denser. Polygons thin least by default (1.0) since they tile space rather than cluster.

  Default value: `1.0`
* `--line-visibility <LINE_VISIBILITY>` — Line visibility gate in GSD multiples: a line is eligible at a level only if its bbox diagonal >= factor * gsd (default 2.0).

   This is a hard drop, not a thin: BIGGER = more small lines dropped at coarse levels (sparser); SMALLER = more small lines kept. The gate is multiplied by the level GSD, so --gsd-base moves it too.

  Default value: `2.0`
* `--polygon-visibility <POLYGON_VISIBILITY>` — Polygon visibility gate in GSD multiples: a polygon is eligible only if its bbox diagonal >= factor * gsd (default 2.0).

   BIGGER = more small polygons dropped at coarse levels (sparser); SMALLER = more kept. See --line-visibility. Retuned 4.0 -> 2.0 in the #259 coarse-zoom sweep (corpus/SWEEPS.md Decision 6): write-time RDP already drops polygons that simplify below the level tolerance, so gates above 2.0 starve coarse zooms without making files smaller, and gates below ~2.0 mostly admit candidates that RDP drops anyway (use --collapse to keep those as representative points).

  Default value: `2.0`
* `--drop-rate <F>` — Per-level density drop rate: each coarser level keeps 1/rate of the next finer level's feature budget (default 1.65).

   This is the Q2 knob that stops mid-zoom counts plateauing at ~everything. Cell-winner thinning stops binding once its grid cell is smaller than the typical feature spacing, so from ~z9 up every feature survives and coarse levels over-retain (Portland roads: ours/tippecanoe ≈ 2–3x at z9–z11). After cell-winner thinning, each level is capped at a budget that decays geometrically toward coarse zooms — budget(L) = N / rate^(finest−L), where N is the input feature count — and the lowest-priority survivors (same class-rank → size → hash order as the cell-winner, spec Q1) are dropped until the level meets its budget. Levels already sparser than their budget (the coarse zooms) are untouched, so this only bites the mid-zoom plateau. BIGGER rate = coarser levels shed harder (sparser mid zooms, smaller files); SMALLER = gentler. The default 1.65 is smaller than tippecanoe's nominal 2.5 because our budget anchors on the full canonical count N (every feature appears at the finest level), not a per-tile basezoom count. The canonical (finest) level is never dropped. See docs/OVERVIEW_TUNING.md and corpus/SWEEPS.md.

  Default value: `1.65`
* `--drop-gamma <F>` — Spatial-fairness strength for the density budget (default 1.5).

   The budget is shared across coarse super-cells (neighborhoods) so a global rank-ordered cut cannot empty sparse rural areas to keep dense cities under budget. Each super-cell keeps its top-priority features up to an allocation proportional to population^(1/gamma): gamma=1 is a proportional cut (every neighborhood keeps the same fraction); gamma>1 is SUBLINEAR — dense neighborhoods keep proportionally fewer, sparse ones proportionally more (they are protected). This is tippecanoe's gamma dot-dropping ("reduce dots to the 1/gamma power in dense areas") applied per super-cell. BIGGER = more protection for sparse areas / harder relative thinning of dense areas. Does not change per-level totals (it only redistributes which features survive spatially), so it is independent of --drop-rate. No effect when --no-density-drop is set.

  Default value: `1.5`
* `--no-density-drop` — Disable the Q2 per-level density budget entirely (off switch).

   Reverts to pure cell-winner thinning — the pre-Q2 behavior — and emits a byte-identical footer (no density_drop provenance). Use this to compare before/after, or when the cell-winner thinning already meets your needs.
* `--cluster` — Enable point clustering (duplicating mode only; opt-in).

   At each overview level, the surviving point in each thinning grid cell ABSORBS the other points in its cell instead of them simply vanishing: the output gains a `point_count` INT64 NOT NULL column recording how many source features each row represents at its level (tippecanoe / supercluster convention; always 1 at the canonical level). The winner keeps its own geometry and attribute values. Lines and polygons are unaffected (their rows carry point_count = 1). Use for graduated-dot rendering of dense point data. See docs/OVERVIEW_TUNING.md.
* `--accumulate-attribute <COL:OP>` — Aggregate a numeric column across clustered points: COL:OP where OP is sum, max, min, or mean. Repeatable. Requires --cluster.

   At each level the winner's value of COL becomes the aggregate over itself + the points it absorbed at that level (computed per level from SOURCE values — mean is exact, never a mean of means). All other columns keep the winner's own values. Example: --accumulate-attribute population:sum --accumulate-attribute confidence:mean
* `--no-coalesce-lines` — Disable line network coalescing (ON by default; duplicating mode).

   By default, at each non-canonical level touching same-class line segments are chained into single "stroke" LineStrings BEFORE the visibility gate and thinning run, so a chain of individually sub-visibility fragments survives as one long, connected artery — road/river networks read as continuous lines at coarse zooms instead of scattered dashes. Chains never merge across class values (when a class ranking is active); junctions continue only within --coalesce-junction-angle of straight. The merged feature keeps the attributes of its highest-priority member, and the output gains a `coalesced_count` INT32 NOT NULL column (source segments merged per row; 1 for unmerged rows and everywhere at the canonical level). Points and polygons are unaffected. In partitioning mode coalescing is inert (a merged chain cannot satisfy the feature-once/verbatim contract). See docs/OVERVIEW_TUNING.md.
* `--coalesce-junction-angle <DEG>` — Junction continuation angle for line coalescing, in degrees (default 0 = OFF: junctions terminate chains, preserving network topology — chosen from the Portland junction-angle sweep in corpus/data/bench/q3/, where strict degree-2 chaining rendered better).

   When > 0: at a junction (3+ same-class segment endpoints meeting), the pair of lines that best continue each other merge when their deviation from a straight continuation is at most this angle — best pair first, so a 4-way crossing continues BOTH through-streets. BIGGER = chains bend further through junctions (longer, fewer strokes; risk of merging through genuine turns).

  Default value: `0.0`
* `--coalesce-snap <F>` — Endpoint snap tolerance for line coalescing, in GSD multiples (default 1.0).

   Exactly-touching endpoints always chain; this knob additionally joins chain ends within factor * gsd of each other (two endpoints closer than one ground sample are indistinguishable at that level). BIGGER = bridges larger digitization gaps (risk: rungs of nearby parallel lines fusing); 0 = exact endpoint matching only.

  Default value: `1.0`
* `--coalesce-max-level-rows <ROWS>` — Per-level candidate-line ceiling for line coalescing (memory guard).

   Chaining holds the level's candidate line geometries in memory at once (every line is a candidate at every non-canonical level, since sub-visibility fragments must be reclaimable). Datasets with more lines than this skip coalescing with a warning instead of breaking the streaming pipeline's memory bound; near-canonical levels that large need coalescing least (segments are individually visible).

  Default value: `2000000`
* `--row-group-size <ROW_GROUP_SIZE>` — Maximum output row-group size in rows.

   Interpreted per level: a level with at most this many rows is written as a single row group; a larger level is split into roughly uniform row groups of at most this size. Coarse bands (few features) therefore become one broad row group; fine bands keep tight per-row-group bbox statistics.

  Default value: `10000`
* `--row-group-size-policy <ROW_GROUP_SIZE_POLICY>` — Per-level row-group sizing policy (#202).

   `constant`: every level uses --row-group-size as its cap (default). `zoom-scaled`: the cap doubles per zoom step below the finest level (cap = row_group_size << (max_zoom - level_zoom)) — coarse bands, which wide viewports read mostly whole anyway, become fewer/larger row groups (fewer remote requests) while the finest level keeps tight bbox pruning.

  Default value: `constant`

  Possible values: `constant`, `zoom-scaled`

* `--full-column-stats` — Keep full Parquet statistics on every column, including high-cardinality string/binary property columns and the WKB geometry column.

   By default those columns' per-row-group min/max stats are suppressed to keep the footer small (a 26-char ULID `id` over hundreds of row groups otherwise bloats the footer to megabytes, paid on every remote query). The bbox covering and `level` column always keep their pruning stats. Enable this if remote clients push predicates on property columns and want row-group skipping on them.
* `--no-streaming` — Disable the two-pass bounded-memory streaming pipeline (H3).

   By default the converter streams the input twice: pass 1 builds the per-feature winner tables (level assignment + density budget) holding only bboxes/kinds/sort-keys; pass 2 re-reads the input per level and simplifies + writes batch-by-batch. Peak memory is O(read batch + winner tables) instead of O(dataset) — e.g. Moldova (632k polygons) drops from ~5.4 GB to well under 1 GB peak RSS. Output is equivalent (same level assignments, rows, and footer). This flag reverts to the original in-memory pipeline, which decodes the whole dataset once and may be marginally faster on small inputs that comfortably fit in RAM.
* `--read-batch-size <ROWS>` — Rows per Arrow read batch in the streaming pipeline (both passes).

   LARGER batches amortize per-batch overhead (slightly faster) at the cost of proportionally more peak memory; SMALLER batches bound memory tighter. The default (8192) keeps per-batch transients in the tens of MB even for vertex-heavy polygon data. No effect with --no-streaming.

  Default value: `8192`
* `--profile <PROFILE>` — Memory/throughput profile for the single-read pass-2 engine (#213/#212).

   `speed` buffers each output level's rows in RAM (fastest; peak RAM grows with buffered output). `bounded` spills them to temporary Arrow IPC files (memory-capped; slight temp-I/O cost). `auto` (default) is workload-based: it estimates buffered output from feature and level counts and spills when that exceeds a fraction of available RAM, so large duplicating runs prefer bounded instead of risking OOM (override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Output is byte-identical across profiles. No effect with --no-streaming.

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches allowed in flight through the pass-2 pipeline at once (read/compute-overlap knob; bounded-channel depth).

   `auto` (the default) sizes this to the machine's available cores (clamped to 4..=16); pass an explicit integer to override. Higher improves core utilization on long-pole geometries at proportionally more peak memory (in-flight-batches × read-batch-size rows resident). The chosen depth and detected core count are logged at pass-2 start. No effect with --no-streaming.

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for the remote-input spill file (issues #219/#272).

   A remote convert stages every fetched column chunk in an anonymous temp file — growing to ≈1× the touched input bytes (the whole object for a full-file convert; only the covering row groups with --bbox) — so later passes re-read from local disk instead of the network. By default it lives under the process temp dir ($TMPDIR); point this at a volume with enough room (a free-space preflight warns about a projected shortfall). The directory must exist. Local inputs never spill.

   On `tiles` this directory also hosts the removed-after-export intermediate overview (#314) — at least input-sized, with its own free-space preflight — unless --keep-overview is given (then the intermediate goes to that path instead). Location precedence for the intermediate: --spill-dir, $TMPDIR, the output directory.



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
* `--tile-size-limit <SIZE>` — Per-tile MVT size cap (e.g., "500K", "1M", or raw bytes). When a tile exceeds it, a single non-iterative drop pass sheds features for that tile only (largest-first for polygons/lines; a uniform spatial stride for point tiles). Defaults to 500K (tippecanoe parity, #280); pass 0 to disable the cap. Aliased as --max-tile-size for parity with the `tiles` command

  Default value: `500K`
* `--report <PATH>` — Write the JSON export report to this path
* `--no-simple-clip-fastpath` — Disable the simple-clip fast path (issue #239), forcing the i_overlay boundary-bridge fallback on every polygon clip. The fast path is on by default (render-equivalent on simple rings); pass this only when you need byte-stable tile output, since the fast path rotates simple rings to a different start vertex
* `--partition-wave <N|auto>` — Partitions processed per band read during export (the export concurrency knob). `auto` (the default) preflights a memory budget: the machine's core count, capped by how many estimated per-partition transients fit in a fraction of available RAM (floor 6; fixed cap 16 only when RAM cannot be probed; override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Pass an explicit integer to override. Wider waves keep more cores busy at proportionally more peak memory (one wave of partitions resident). The chosen width and the preflight inputs are logged at export start. Output is byte-identical for every value

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
does not reproduce A. See docs/decode.md for details.

###### **Arguments:**

* `<INPUT>` — Input PMTiles archive (vector tiles)
* `<OUTPUT>` — Output GeoParquet file

###### **Options:**

* `--zoom <ZOOM>` — Decode a single zoom level (recommended for most uses)
* `--min-zoom <MIN_ZOOM>` — Minimum zoom level to decode
* `--max-zoom <MAX_ZOOM>` — Maximum zoom level to decode
* `--layer <NAME>` — Only decode features from this MVT layer
* `--report <PATH>` — Write the JSON decode report to this path



