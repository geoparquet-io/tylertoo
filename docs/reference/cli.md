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
* [`tylertoo pyramid`↴](#tylertoo-pyramid)
* [`tylertoo merge`↴](#tylertoo-merge)

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
* `pyramid` — Build a multi-band pyramid: several inputs, each owning a zoom range, one archive (issue #345)
* `merge` — Concatenate disjoint PMTiles archives into one (issue #498)



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
* `--max-tile-size <SIZE>` — Maximum tile size (e.g., "500K", "1M", or raw bytes). When a tile exceeds this limit, the export sheds features in a single non-iterative pass (largest-first for polygons/lines; a uniform spatial stride for point tiles). Defaults to 500K (tippecanoe parity, #280); pass 0 to disable the cap. Aliased as --tile-size-limit for parity with `export-pmtiles`. With --verbatim and no explicit value, the cap is disabled: a valve that sheds features to fit a byte budget is not verbatim either
* `--no-simple-clip-fastpath` — Disable the simple-clip fast path (issue #239), forcing the i_overlay boundary-bridge fallback on every polygon clip. The fast path is on by default (render-equivalent on simple rings); pass this only when you need byte-stable tile output, since the fast path rotates simple rings to a different start vertex
* `--tile-buffer <TILE_BUFFER>` — Per-tile edge buffer, in tile pixels, carried across tile seams so features don't clip at boundaries

  Default value: `8`
* `--partition-wave <N|auto>` — Partitions processed per band read during the export phase (the export concurrency knob). `auto` (the default) preflights a memory budget: the machine's core count, capped by how many estimated per-partition transients fit in a fraction of available RAM (container-aware: cgroup v2/v1 limits are respected; floor 6; fixed cap 16 only when RAM cannot be probed; override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Pass an explicit integer to override. Wider waves keep more cores busy at proportionally more peak memory (one wave of partitions resident). The chosen width and the preflight inputs are logged at export start. Output is byte-identical for every value

  Default value: `auto`
* `--feature-order <input|COLUMN[:asc|:desc]>` — Within-tile feature order (#361): `input` (default) or a property name, optionally `:asc` / `:desc`.

   MVT does not define draw order, but renderers paint features in the order the tile lists them, so this is the paint order for any style that does not override it. `input` emits source row order. Naming a column sorts within each tile by that property — `--feature-order level` puts high `level` on top, which is what a nested choropleth usually wants — with ties kept in input order so output stays deterministic.

  Default value: `input`
* `--report <PATH>` — Write a JSON report to this path: a combined object with a `convert` section (the overview build, matching `overview --report`) and an `export` section (the PMTiles export, matching `export-pmtiles --report`), so the one-step run captures both halves the two-step chain would
* `--keep-overview <PATH>` — Write the intermediate overview GeoParquet to PATH and RETAIN it, instead of a temp file removed after the export — one run then yields both artifacts: the reusable multi-resolution overview (queryable, re-exportable, see `tylertoo overview`) and the PMTiles. The PMTiles output is identical either way. Without this flag the intermediate is written to --spill-dir if given, else $TMPDIR if set, else the output directory, and deleted once the export finishes (see the note on the materialized intermediate under --spill-dir)
* `-v`, `--verbose` — Enable verbose output (per-level and per-zoom breakdowns)
* `--verbatim` — Tile the input EXACTLY AS GIVEN: switch the whole generalization ladder off at every level (#345 / #360).

   The ladder derives coarse levels from the fine input by thinning and simplifying. That is right for a road network and wrong for a pre-aggregated grid: an H3 r6 cell is not a simplified r7 cell, it is their parent, and its count is their sum. Run an aggregate through the gates and a coarse level shows SOME cells and silently omits the rest, instead of showing what they sum to.

   Equivalent to --no-density-drop --no-coalesce-lines --simplify-factor 0 with every thinning factor and visibility gate at 0 — a flag set that was not even reachable before, since a thinning factor of 0 used to be rejected. Reach for it when the input is already the right resolution for the zooms you are asking for: DGGS/cell aggregates, pre-levelled input, or one band of a pyramid.

   Requires --mode duplicating: partitioning writes each feature at one level, so with thinning off everything lands in the coarsest level.

   On `tiles` it also disables the per-tile size cap (an unbounded --max-tile-size), since a valve that sheds features to fit a byte budget is not verbatim either; pass --max-tile-size explicitly to put a cap back. The two-step form does NOT inherit that — pass --tile-size-limit 0 to export-pmtiles.

   Supplies DEFAULTS rather than overriding: any knob you set explicitly wins, so --verbatim --simplify-factor 0.5 is NEARLY verbatim.
* `--sort-key <COL>` — Column name used as the cell-winner priority (sort) key. Mutually exclusive with --class-rank
* `--magnitude-ladder <COL>` — Magnitude ladder: let COL decide each feature's ENTRY ZOOM (#364).

   Thinning ranks on geometry, which is backwards whenever a dataset's most important features are its physically smallest — a population density layer, say, where dense urban tracts are tiny next to sparse rural ones. Coarse levels then keep the big low-value polygons and drop the small high-value ones. --sort-key cannot fix that: it chooses between features competing for a cell, and the visibility gate has already dropped the small ones on size.

   A ladder ranks COL's DISTINCT values descending and gives each rank an entry zoom one --ladder-step apart, starting at --min-zoom. A feature appears from its entry zoom inward and not before, exempt from the visibility gate and from thinning throughout. Nothing is deleted: the finest level still carries every feature.

   Ranking DISTINCT values (SQL DENSE_RANK) rather than the values themselves keeps the ladder scale-free — mapping a raw value onto the zoom range strands everything in the upper zooms whenever the values occupy a narrow part of their nominal scale.

   Implies --collapse unless you pass --collapse-square, so a promoted feature that simplifies below its level's tolerance survives as a representative point rather than being dropped again.

   Mutually exclusive with --entry-zoom.
* `--ladder-step <N>` — Zooms between consecutive --magnitude-ladder rungs (default 1)

  Default value: `1`
* `--entry-zoom <SPEC>` — Explicit entry zooms, for full control over the rungs: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...` — e.g. `--entry-zoom "density:5000=4,1000=6,200=8"`.

   Same semantics as --magnitude-ladder but the rungs are placed by hand rather than derived. Values the spec does not list get no entry zoom and take the ordinary gate. Mutually exclusive with --magnitude-ladder.
* `--class-rank <SPEC>` — Categorical class ranking (higher priority wins a cell). Format: `COLUMN:VALUE=RANK,VALUE=RANK,...` — e.g. `--class-rank road_class:motorway=5,primary=4,residential=2`. Present-but-unlisted values rank below every listed value (but above nulls). Mutually exclusive with --sort-key
* `--no-auto-rank` — Disable auto-detection of well-known schemas (Overture roads `class`/ `road_class`, Overture places `confidence`)
* `--filter <EXPR>` — Attribute filter: only convert features matching this SQL-WHERE-style predicate over the input's property columns, e.g. "confidence > 0.8", "crop_type IN ('soy', 'corn')", "note IS NOT NULL AND (class = 'a' OR class = 'b')". Supports =, !=, <, <=, >, >=, IN (...), IS [NOT] NULL, AND/OR/NOT, parentheses, 'string' and numeric literals, and "quoted column" names; timestamp columns compare against 'YYYY-MM-DD' / 'YYYY-MM-DD HH:MM:SS' / RFC 3339 datetime strings (read as UTC); nulls follow SQL three-valued logic (a row is kept only when the predicate is TRUE). Evaluated during the pass-1 scan, so it composes with --bbox; input row groups whose parquet column statistics preclude any match are skipped at the footer level (on remote input those byte ranges are never fetched). Aliased as --where. See docs/OVERVIEW_TUNING.md
* `--include-property <COL>` — Keep ONLY these property columns (repeatable; tippecanoe -y). Every other property is dropped at scan time: the excluded columns are not decoded and the overview file only carries what was asked for. The geometry column is always kept. A column another knob reads (--sort-key, --filter, --accumulate-attribute, --magnitude-ladder, --class-rank) must stay included; naming a column the input does not have is an error
* `--exclude-property <COL>` — Drop these property columns (repeatable; tippecanoe -x). Ignored when --include-property is given, as in tippecanoe. Naming a column the input does not have only warns
* `--exclude-all-properties` — Drop every property column (tippecanoe -X): geometry-only output. Ignored when --include-property is given, as in tippecanoe
* `--gsd-base <F>` — GSD tile-band base for the zoom→GSD mapping: gsd(z) = 40075016.69 / base / 2^z (spec §5.2, cogp-rs default 1024).

   This is the master detail knob for a zoom-range plan. A LARGER base makes every level's GSD SMALLER, so less is thinned and simplified at a given zoom (denser, more detailed, larger coarse levels). A SMALLER base makes GSDs LARGER (sparser, cruder, cheaper coarse levels). It scales the whole ladder at once, whereas --simplify-factor and the --*-thinning knobs act relative to each level's GSD. No effect when --gsd is given (those GSDs are already absolute meters).

   Cheat sheet: coarse levels too sparse → RAISE --gsd-base (or lower the thinning factors); too crude → lower --simplify-factor. See docs/OVERVIEW_TUNING.md.

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance factor: RDP tolerance = factor * gsd (meters), duplicating mode only (default 1.0).

   Controls how much per-feature vertex detail each coarse level sheds. LOWER = smoother/less aggressive = more vertices kept = crisper but heavier levels; HIGHER = cruder = fewer vertices = lighter levels. The canonical (finest) level is always verbatim regardless. A line/polygon whose bbox diagonal is below the tolerance is dropped entirely, so a very high factor also thins features, not just vertices.

   Cheat sheet: coarse levels look too crude/blocky → LOWER --simplify-factor. See docs/OVERVIEW_TUNING.md.
* `--collapse` — Collapse below-visibility polygons to a representative point instead of dropping them (spec Q4 opt-in). Changes the geometry type at coarse levels (fill-styled renderers silently ignore points — add a circle layer, or use --collapse-square to stay type-preserving)
* `--collapse-square` — Stand in for the polygons a coarse level drops with ~1xGSD placeholder SQUARES, so the level still shows where the area is (tippecanoe tiny-polygon reduction; opt-in).

   Two mechanisms, one threshold T = (simplify-factor * gsd)^2 (#384): every polygon the level does NOT carry — failed the visibility gate, lost its thinning cell, cut by the density budget — adds its area to an accumulator for its 32xGSD patch, and each time a patch's total crosses T the polygon that crossed it is emitted as a T-area square with its own attributes; a polygon the level DOES carry but that collapses below T at write time survives as a square with probability A/T. Either way aggregate area stays truthful: a country of 25 m fields reads as farmland at z0 instead of vanishing, and dense blocks read denser than isolated barns. Type-preserving (the output stays Polygon), so plain fill styles keep working, unlike --collapse. Deterministic (same input -> same output, engine- and thread-independent). Duplicating mode only. See docs/OVERVIEW_TUNING.md.
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
* `--polygon-thinning <POLYGON_THINNING>` — Polygon thinning factor: grid cell size = factor * gsd (default 1.0).

   BIGGER = SPARSER, SMALLER = denser. Polygons thin least by default (1.0) since they tile space rather than cluster.
* `--line-visibility <LINE_VISIBILITY>` — Line visibility gate in GSD multiples: a line is eligible at a level only if its bbox diagonal >= factor * gsd (default 2.0).

   This is a hard drop, not a thin: BIGGER = more small lines dropped at coarse levels (sparser); SMALLER = more small lines kept. The gate is multiplied by the level GSD, so --gsd-base moves it too.
* `--polygon-visibility <POLYGON_VISIBILITY>` — Polygon visibility gate in GSD multiples: a polygon is eligible only if its bbox diagonal >= factor * gsd (default 2.0).

   BIGGER = more small polygons dropped at coarse levels (sparser); SMALLER = more kept. See --line-visibility. Retuned 4.0 -> 2.0 in the #259 coarse-zoom sweep (corpus/SWEEPS.md Decision 6): write-time RDP already drops polygons that simplify below the level tolerance, so gates above 2.0 starve coarse zooms without making files smaller, and gates below ~2.0 mostly admit candidates that RDP drops anyway (use --collapse to keep those as representative points).
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

   By default, at each non-canonical level touching same-class line segments are chained into single "stroke" LineStrings BEFORE the visibility gate and thinning run, so a chain of individually sub-visibility fragments survives as one long, connected artery — road/river networks read as continuous lines at coarse zooms instead of scattered dashes. Chains never merge across class values (when a class ranking is active); junctions continue only within --coalesce-junction-angle of straight. The merged feature keeps the attributes of its highest-priority member, and the output gains a `coalesced_count` INT32 NOT NULL column (source segments merged per row; 1 for unmerged rows and everywhere at the canonical level; withheld from tiles when it is 1 everywhere). Points and polygons are unaffected. In partitioning mode coalescing is inert (a merged chain cannot satisfy the feature-once/verbatim contract). See docs/OVERVIEW_TUNING.md.
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

   This is a request, not a guarantee: it may be raised automatically to fit parquet's row-group ceiling of 32,768 groups per file (a warning says so, and the conversion report records the cap actually used).

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

   LARGER batches amortize per-batch overhead (slightly faster) at the cost of proportionally more peak memory; SMALLER batches bound memory tighter. The default (8192) keeps per-batch transients in the tens of MB even for vertex-heavy polygon data. Capped at 1048576 rows. No effect with --no-streaming.

  Default value: `8192`
* `--profile <PROFILE>` — Memory/throughput profile for the single-read pass-2 engine (#213/#212).

   `speed` buffers each output level's rows in RAM (fastest; peak RAM grows with buffered output). `bounded` spills them to temporary Arrow IPC files (memory-capped; slight temp-I/O cost). `auto` (default) is workload-based: it estimates buffered output from feature and level counts and spills when that exceeds a fraction of available RAM (container-aware: cgroup v2/v1 limits are respected), so large duplicating runs prefer bounded instead of risking OOM (override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Output is byte-identical across profiles. No effect with --no-streaming.

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches allowed in flight through the streaming pipeline at once (read/compute-overlap knob; bounded-channel depth) — pass 1's scan and pass 2's per-level fan-out both use it.

   `auto` (the default) sizes this to the machine's available cores (clamped to 4..=16); pass an explicit integer to override. Higher improves core utilization on long-pole geometries at proportionally more peak memory (in-flight-batches × read-batch-size rows resident, PER PASS — passes 1 and 2 never run concurrently, so this does not double). This is no longer the only resident-batch term: pass 2's readers hold their own read-ahead on top (--read-workers × its queue depth), and under --profile bounded each level's spill writer holds up to 3 more. The chosen depth and detected core count are logged at the start of each pass. No effect with --no-streaming.

  Default value: `auto`
* `--read-workers <N|auto>` — Reader threads pass 2 splits the input across (issue #494).

   Parquet row groups are independently readable, so pass 2 can read the input with several threads at once and merge their batches back into read order. `auto` (the default) takes a quarter of the machine's cores, capped at 4 — readers decompress and decode, so they compete with the pool doing the simplification they are feeding. `1` is the single sequential reader. An explicit value is honoured up to a ceiling of 2× this machine's cores (at least 4); above that it is rejected.

   Output is byte-identical for every value: workers own disjoint runs of row groups and the merge reproduces exactly the batch sequence one reader would have produced.

   Remote inputs always read sequentially (concurrent readers over one remote source evict each other's fetched chunks) — including a remote input the run has staged to local disk, since staging is per-part and the source stays remote. The read-ahead is sized against a modelled slice of the same memory budget the pass-2 sink uses (and each worker's queue is capped shallower under --profile bounded), so a small box, or a wide input, quietly gets fewer workers. Helps most when the input's row groups are small relative to that budget — `gpio` writes well-sized ones.

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for the remote-input spill file (issues #219/#272).

   A remote convert stages every fetched column chunk in an anonymous temp file — growing to ≈1× the touched input bytes (the whole object for a full-file convert; only the covering row groups with --bbox) — so later passes re-read from local disk instead of the network. By default it lives under the process temp dir ($TMPDIR); point this at a volume with enough room (a free-space preflight warns about a projected shortfall). The directory must exist. Local inputs never spill.

   On `tiles` this directory also hosts the removed-after-export intermediate overview (#314) — at least input-sized, with its own free-space preflight — unless --keep-overview is given (then the intermediate goes to that path instead). Location precedence for the intermediate: --spill-dir, $TMPDIR, the output directory.
* `--save-plan <PATH>` — Write the convert plan artifact to PATH, then carry on converting.

   The plan is the complete result of pass 1 and the level assignment: which level every input row enters at, the per-level counts, the clustering / coalescing / carrier side tables, the resolved ranking provenance and the dataset-wide tallies — plus a fingerprint of the inputs and of every thinning-relevant flag (see --plan for exactly what the fingerprint pins). Re-running with --plan then skips pass 1 and the assignment entirely.

   The assignment is dataset-global: the density budget water-fills a super-cell budget over every candidate of a level, the level walk carries a running kept count coarse to fine, and --magnitude-ladder dense-ranks the whole column. A sharded build must therefore consume ONE plan rather than recompute the assignment per shard, or the shards' pyramids disagree.

   PATH's parent directory must exist and be writable; that is checked up front, before anything is scanned. An existing plan at PATH is overwritten, with a log line.
* `--plan <PATH>` — Reuse the convert plan artifact at PATH instead of running pass 1 and the level assignment.

   The plan's fingerprint must match this run — the tylertoo version, every thinning-relevant flag, and each input's identity. A mismatch is a hard error naming the offending field, so a stale plan never silently produces a different pyramid. The write-side flags (--profile, --row-group-size, --in-flight-batches, --spill-dir) are deliberately NOT fingerprinted, so one plan can be replayed across them.

   What "input identity" pins: for every part, local or remote, the path/URL, the byte size, the row count and the row-group layout from the parquet footer, plus the row groups --bbox/--filter pruned to. A local part additionally pins its mtime. A remote object has no mtime and no ETag here, so an object rewritten in place with the same size, row count and row-group layout is NOT detected; tylertoo warns when any part is remote.

   PATH must be a readable convert plan; that is checked up front, before the input is opened.



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
* `--verbatim` — Tile the input EXACTLY AS GIVEN: switch the whole generalization ladder off at every level (#345 / #360).

   The ladder derives coarse levels from the fine input by thinning and simplifying. That is right for a road network and wrong for a pre-aggregated grid: an H3 r6 cell is not a simplified r7 cell, it is their parent, and its count is their sum. Run an aggregate through the gates and a coarse level shows SOME cells and silently omits the rest, instead of showing what they sum to.

   Equivalent to --no-density-drop --no-coalesce-lines --simplify-factor 0 with every thinning factor and visibility gate at 0 — a flag set that was not even reachable before, since a thinning factor of 0 used to be rejected. Reach for it when the input is already the right resolution for the zooms you are asking for: DGGS/cell aggregates, pre-levelled input, or one band of a pyramid.

   Requires --mode duplicating: partitioning writes each feature at one level, so with thinning off everything lands in the coarsest level.

   On `tiles` it also disables the per-tile size cap (an unbounded --max-tile-size), since a valve that sheds features to fit a byte budget is not verbatim either; pass --max-tile-size explicitly to put a cap back. The two-step form does NOT inherit that — pass --tile-size-limit 0 to export-pmtiles.

   Supplies DEFAULTS rather than overriding: any knob you set explicitly wins, so --verbatim --simplify-factor 0.5 is NEARLY verbatim.
* `--sort-key <COL>` — Column name used as the cell-winner priority (sort) key. Mutually exclusive with --class-rank
* `--magnitude-ladder <COL>` — Magnitude ladder: let COL decide each feature's ENTRY ZOOM (#364).

   Thinning ranks on geometry, which is backwards whenever a dataset's most important features are its physically smallest — a population density layer, say, where dense urban tracts are tiny next to sparse rural ones. Coarse levels then keep the big low-value polygons and drop the small high-value ones. --sort-key cannot fix that: it chooses between features competing for a cell, and the visibility gate has already dropped the small ones on size.

   A ladder ranks COL's DISTINCT values descending and gives each rank an entry zoom one --ladder-step apart, starting at --min-zoom. A feature appears from its entry zoom inward and not before, exempt from the visibility gate and from thinning throughout. Nothing is deleted: the finest level still carries every feature.

   Ranking DISTINCT values (SQL DENSE_RANK) rather than the values themselves keeps the ladder scale-free — mapping a raw value onto the zoom range strands everything in the upper zooms whenever the values occupy a narrow part of their nominal scale.

   Implies --collapse unless you pass --collapse-square, so a promoted feature that simplifies below its level's tolerance survives as a representative point rather than being dropped again.

   Mutually exclusive with --entry-zoom.
* `--ladder-step <N>` — Zooms between consecutive --magnitude-ladder rungs (default 1)

  Default value: `1`
* `--entry-zoom <SPEC>` — Explicit entry zooms, for full control over the rungs: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...` — e.g. `--entry-zoom "density:5000=4,1000=6,200=8"`.

   Same semantics as --magnitude-ladder but the rungs are placed by hand rather than derived. Values the spec does not list get no entry zoom and take the ordinary gate. Mutually exclusive with --magnitude-ladder.
* `--class-rank <SPEC>` — Categorical class ranking (higher priority wins a cell). Format: `COLUMN:VALUE=RANK,VALUE=RANK,...` — e.g. `--class-rank road_class:motorway=5,primary=4,residential=2`. Present-but-unlisted values rank below every listed value (but above nulls). Mutually exclusive with --sort-key
* `--no-auto-rank` — Disable auto-detection of well-known schemas (Overture roads `class`/ `road_class`, Overture places `confidence`)
* `--filter <EXPR>` — Attribute filter: only convert features matching this SQL-WHERE-style predicate over the input's property columns, e.g. "confidence > 0.8", "crop_type IN ('soy', 'corn')", "note IS NOT NULL AND (class = 'a' OR class = 'b')". Supports =, !=, <, <=, >, >=, IN (...), IS [NOT] NULL, AND/OR/NOT, parentheses, 'string' and numeric literals, and "quoted column" names; timestamp columns compare against 'YYYY-MM-DD' / 'YYYY-MM-DD HH:MM:SS' / RFC 3339 datetime strings (read as UTC); nulls follow SQL three-valued logic (a row is kept only when the predicate is TRUE). Evaluated during the pass-1 scan, so it composes with --bbox; input row groups whose parquet column statistics preclude any match are skipped at the footer level (on remote input those byte ranges are never fetched). Aliased as --where. See docs/OVERVIEW_TUNING.md
* `--include-property <COL>` — Keep ONLY these property columns (repeatable; tippecanoe -y). Every other property is dropped at scan time: the excluded columns are not decoded and the overview file only carries what was asked for. The geometry column is always kept. A column another knob reads (--sort-key, --filter, --accumulate-attribute, --magnitude-ladder, --class-rank) must stay included; naming a column the input does not have is an error
* `--exclude-property <COL>` — Drop these property columns (repeatable; tippecanoe -x). Ignored when --include-property is given, as in tippecanoe. Naming a column the input does not have only warns
* `--exclude-all-properties` — Drop every property column (tippecanoe -X): geometry-only output. Ignored when --include-property is given, as in tippecanoe
* `--gsd-base <F>` — GSD tile-band base for the zoom→GSD mapping: gsd(z) = 40075016.69 / base / 2^z (spec §5.2, cogp-rs default 1024).

   This is the master detail knob for a zoom-range plan. A LARGER base makes every level's GSD SMALLER, so less is thinned and simplified at a given zoom (denser, more detailed, larger coarse levels). A SMALLER base makes GSDs LARGER (sparser, cruder, cheaper coarse levels). It scales the whole ladder at once, whereas --simplify-factor and the --*-thinning knobs act relative to each level's GSD. No effect when --gsd is given (those GSDs are already absolute meters).

   Cheat sheet: coarse levels too sparse → RAISE --gsd-base (or lower the thinning factors); too crude → lower --simplify-factor. See docs/OVERVIEW_TUNING.md.

  Default value: `1024.0`
* `--simplify-factor <SIMPLIFY_FACTOR>` — Simplification tolerance factor: RDP tolerance = factor * gsd (meters), duplicating mode only (default 1.0).

   Controls how much per-feature vertex detail each coarse level sheds. LOWER = smoother/less aggressive = more vertices kept = crisper but heavier levels; HIGHER = cruder = fewer vertices = lighter levels. The canonical (finest) level is always verbatim regardless. A line/polygon whose bbox diagonal is below the tolerance is dropped entirely, so a very high factor also thins features, not just vertices.

   Cheat sheet: coarse levels look too crude/blocky → LOWER --simplify-factor. See docs/OVERVIEW_TUNING.md.
* `--collapse` — Collapse below-visibility polygons to a representative point instead of dropping them (spec Q4 opt-in). Changes the geometry type at coarse levels (fill-styled renderers silently ignore points — add a circle layer, or use --collapse-square to stay type-preserving)
* `--collapse-square` — Stand in for the polygons a coarse level drops with ~1xGSD placeholder SQUARES, so the level still shows where the area is (tippecanoe tiny-polygon reduction; opt-in).

   Two mechanisms, one threshold T = (simplify-factor * gsd)^2 (#384): every polygon the level does NOT carry — failed the visibility gate, lost its thinning cell, cut by the density budget — adds its area to an accumulator for its 32xGSD patch, and each time a patch's total crosses T the polygon that crossed it is emitted as a T-area square with its own attributes; a polygon the level DOES carry but that collapses below T at write time survives as a square with probability A/T. Either way aggregate area stays truthful: a country of 25 m fields reads as farmland at z0 instead of vanishing, and dense blocks read denser than isolated barns. Type-preserving (the output stays Polygon), so plain fill styles keep working, unlike --collapse. Deterministic (same input -> same output, engine- and thread-independent). Duplicating mode only. See docs/OVERVIEW_TUNING.md.
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
* `--polygon-thinning <POLYGON_THINNING>` — Polygon thinning factor: grid cell size = factor * gsd (default 1.0).

   BIGGER = SPARSER, SMALLER = denser. Polygons thin least by default (1.0) since they tile space rather than cluster.
* `--line-visibility <LINE_VISIBILITY>` — Line visibility gate in GSD multiples: a line is eligible at a level only if its bbox diagonal >= factor * gsd (default 2.0).

   This is a hard drop, not a thin: BIGGER = more small lines dropped at coarse levels (sparser); SMALLER = more small lines kept. The gate is multiplied by the level GSD, so --gsd-base moves it too.
* `--polygon-visibility <POLYGON_VISIBILITY>` — Polygon visibility gate in GSD multiples: a polygon is eligible only if its bbox diagonal >= factor * gsd (default 2.0).

   BIGGER = more small polygons dropped at coarse levels (sparser); SMALLER = more kept. See --line-visibility. Retuned 4.0 -> 2.0 in the #259 coarse-zoom sweep (corpus/SWEEPS.md Decision 6): write-time RDP already drops polygons that simplify below the level tolerance, so gates above 2.0 starve coarse zooms without making files smaller, and gates below ~2.0 mostly admit candidates that RDP drops anyway (use --collapse to keep those as representative points).
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

   By default, at each non-canonical level touching same-class line segments are chained into single "stroke" LineStrings BEFORE the visibility gate and thinning run, so a chain of individually sub-visibility fragments survives as one long, connected artery — road/river networks read as continuous lines at coarse zooms instead of scattered dashes. Chains never merge across class values (when a class ranking is active); junctions continue only within --coalesce-junction-angle of straight. The merged feature keeps the attributes of its highest-priority member, and the output gains a `coalesced_count` INT32 NOT NULL column (source segments merged per row; 1 for unmerged rows and everywhere at the canonical level; withheld from tiles when it is 1 everywhere). Points and polygons are unaffected. In partitioning mode coalescing is inert (a merged chain cannot satisfy the feature-once/verbatim contract). See docs/OVERVIEW_TUNING.md.
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

   This is a request, not a guarantee: it may be raised automatically to fit parquet's row-group ceiling of 32,768 groups per file (a warning says so, and the conversion report records the cap actually used).

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

   LARGER batches amortize per-batch overhead (slightly faster) at the cost of proportionally more peak memory; SMALLER batches bound memory tighter. The default (8192) keeps per-batch transients in the tens of MB even for vertex-heavy polygon data. Capped at 1048576 rows. No effect with --no-streaming.

  Default value: `8192`
* `--profile <PROFILE>` — Memory/throughput profile for the single-read pass-2 engine (#213/#212).

   `speed` buffers each output level's rows in RAM (fastest; peak RAM grows with buffered output). `bounded` spills them to temporary Arrow IPC files (memory-capped; slight temp-I/O cost). `auto` (default) is workload-based: it estimates buffered output from feature and level counts and spills when that exceeds a fraction of available RAM (container-aware: cgroup v2/v1 limits are respected), so large duplicating runs prefer bounded instead of risking OOM (override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Output is byte-identical across profiles. No effect with --no-streaming.

  Default value: `auto`

  Possible values: `auto`, `speed`, `bounded`

* `--in-flight-batches <N|auto>` — Read batches allowed in flight through the streaming pipeline at once (read/compute-overlap knob; bounded-channel depth) — pass 1's scan and pass 2's per-level fan-out both use it.

   `auto` (the default) sizes this to the machine's available cores (clamped to 4..=16); pass an explicit integer to override. Higher improves core utilization on long-pole geometries at proportionally more peak memory (in-flight-batches × read-batch-size rows resident, PER PASS — passes 1 and 2 never run concurrently, so this does not double). This is no longer the only resident-batch term: pass 2's readers hold their own read-ahead on top (--read-workers × its queue depth), and under --profile bounded each level's spill writer holds up to 3 more. The chosen depth and detected core count are logged at the start of each pass. No effect with --no-streaming.

  Default value: `auto`
* `--read-workers <N|auto>` — Reader threads pass 2 splits the input across (issue #494).

   Parquet row groups are independently readable, so pass 2 can read the input with several threads at once and merge their batches back into read order. `auto` (the default) takes a quarter of the machine's cores, capped at 4 — readers decompress and decode, so they compete with the pool doing the simplification they are feeding. `1` is the single sequential reader. An explicit value is honoured up to a ceiling of 2× this machine's cores (at least 4); above that it is rejected.

   Output is byte-identical for every value: workers own disjoint runs of row groups and the merge reproduces exactly the batch sequence one reader would have produced.

   Remote inputs always read sequentially (concurrent readers over one remote source evict each other's fetched chunks) — including a remote input the run has staged to local disk, since staging is per-part and the source stays remote. The read-ahead is sized against a modelled slice of the same memory budget the pass-2 sink uses (and each worker's queue is capped shallower under --profile bounded), so a small box, or a wide input, quietly gets fewer workers. Helps most when the input's row groups are small relative to that budget — `gpio` writes well-sized ones.

  Default value: `auto`
* `--spill-dir <PATH>` — Directory for the remote-input spill file (issues #219/#272).

   A remote convert stages every fetched column chunk in an anonymous temp file — growing to ≈1× the touched input bytes (the whole object for a full-file convert; only the covering row groups with --bbox) — so later passes re-read from local disk instead of the network. By default it lives under the process temp dir ($TMPDIR); point this at a volume with enough room (a free-space preflight warns about a projected shortfall). The directory must exist. Local inputs never spill.

   On `tiles` this directory also hosts the removed-after-export intermediate overview (#314) — at least input-sized, with its own free-space preflight — unless --keep-overview is given (then the intermediate goes to that path instead). Location precedence for the intermediate: --spill-dir, $TMPDIR, the output directory.
* `--save-plan <PATH>` — Write the convert plan artifact to PATH, then carry on converting.

   The plan is the complete result of pass 1 and the level assignment: which level every input row enters at, the per-level counts, the clustering / coalescing / carrier side tables, the resolved ranking provenance and the dataset-wide tallies — plus a fingerprint of the inputs and of every thinning-relevant flag (see --plan for exactly what the fingerprint pins). Re-running with --plan then skips pass 1 and the assignment entirely.

   The assignment is dataset-global: the density budget water-fills a super-cell budget over every candidate of a level, the level walk carries a running kept count coarse to fine, and --magnitude-ladder dense-ranks the whole column. A sharded build must therefore consume ONE plan rather than recompute the assignment per shard, or the shards' pyramids disagree.

   PATH's parent directory must exist and be writable; that is checked up front, before anything is scanned. An existing plan at PATH is overwritten, with a log line.
* `--plan <PATH>` — Reuse the convert plan artifact at PATH instead of running pass 1 and the level assignment.

   The plan's fingerprint must match this run — the tylertoo version, every thinning-relevant flag, and each input's identity. A mismatch is a hard error naming the offending field, so a stale plan never silently produces a different pyramid. The write-side flags (--profile, --row-group-size, --in-flight-batches, --spill-dir) are deliberately NOT fingerprinted, so one plan can be replayed across them.

   What "input identity" pins: for every part, local or remote, the path/URL, the byte size, the row count and the row-group layout from the parquet footer, plus the row groups --bbox/--filter pruned to. A local part additionally pins its mtime. A remote object has no mtime and no ETag here, so an object rewritten in place with the same size, row count and row-group layout is NOT detected; tylertoo warns when any part is remote.

   PATH must be a readable convert plan; that is checked up front, before the input is opened.



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
* `--min-zoom <ZOOM>` — Minimum zoom the archive declares, even if the overview file's coarsest levels are missing (#380). `overview` omits a level that generalizes to nothing, so a file built for z0..z13 can start at z2; without this the header then says z2 and a client set up for the requested range never asks for the zoomed-out view. The empty zooms hold no tiles (an empty tile, in PMTiles terms). Must not be finer than the coarsest level present. Default: the coarsest level's zoom
* `--include-property <NAME>` — Keep ONLY these properties in the tiles (repeatable; tippecanoe -y), matched on the names the tiles publish. The overview file is untouched. Naming a property the file does not export is an error; the --feature-order column must stay included
* `--exclude-property <NAME>` — Drop these properties from the tiles (repeatable; tippecanoe -x). Ignored when --include-property is given, as in tippecanoe
* `--exclude-all-properties` — Drop every property (tippecanoe -X): geometry-only tiles. Ignored when --include-property is given, as in tippecanoe
* `--tile-buffer <TILE_BUFFER>` — Per-tile edge buffer, in tile pixels (feature seam continuity)

  Default value: `8`
* `--tile-size-limit <SIZE>` — Per-tile MVT size cap (e.g., "500K", "1M", or raw bytes). When a tile exceeds it, a single non-iterative drop pass sheds features for that tile only (largest-first for polygons/lines; a uniform spatial stride for point tiles). Defaults to 500K (tippecanoe parity, #280); pass 0 to disable the cap. Aliased as --max-tile-size for parity with the `tiles` command

  Default value: `500K`
* `--report <PATH>` — Write the JSON export report to this path
* `--no-simple-clip-fastpath` — Disable the simple-clip fast path (issue #239), forcing the i_overlay boundary-bridge fallback on every polygon clip. The fast path is on by default (render-equivalent on simple rings); pass this only when you need byte-stable tile output, since the fast path rotates simple rings to a different start vertex
* `--partition-wave <N|auto>` — Partitions processed per band read during export (the export concurrency knob). `auto` (the default) preflights a memory budget: the machine's core count, capped by how many estimated per-partition transients fit in a fraction of available RAM (container-aware: cgroup v2/v1 limits are respected; floor 6; fixed cap 16 only when RAM cannot be probed; override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Pass an explicit integer to override. Wider waves keep more cores busy at proportionally more peak memory (one wave of partitions resident). The chosen width and the preflight inputs are logged at export start. Output is byte-identical for every value

  Default value: `auto`
* `--feature-order <input|COLUMN[:asc|:desc]>` — Within-tile feature order (#361): `input` (default) or a property name, optionally `:asc` / `:desc`.

   MVT does not define draw order, but renderers paint features in the order the tile lists them, so this is the paint order for any style that does not override it. `input` emits source row order. Naming a column sorts within each tile by that property — `--feature-order level` puts high `level` on top, which is what a nested choropleth usually wants — with ties kept in input order so output stays deterministic.

  Default value: `input`



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



## `tylertoo pyramid`

Build a multi-band pyramid: several inputs, each owning a zoom range, one archive (issue #345)

**Usage:** `tylertoo pyramid [OPTIONS] --band <LO-HI:INPUT[:LAYER]> <OUTPUT>`

###### **Arguments:**

* `<OUTPUT>` — Output PMTiles archive

###### **Options:**

* `--band <LO-HI:INPUT[:LAYER]>` — A band: `LO-HI:INPUT[:LAYER]`, repeatable.

   INPUT is either a GeoParquet source — tiled here, restricted to this band's zoom range — or a PMTiles archive already covering that range, which is merged as-is. For a pre-tiled archive the declared LO-HI does not have to equal the archive's own zoom range exactly: an archive that holds MORE zooms than LO-HI is a subrange — the documented way to split one pre-tiled archive across several bands (e.g. two `--band` entries pointing at the same z0-z13 archive, one declaring z0-5 and the other z6-13) — and is accepted quietly. An archive that holds FEWER zooms than LO-HI is an error by default: those missing zooms would render silently empty, almost always because LO-HI disagrees with what the archive was actually tiled with; pass `--allow-missing-zooms` for a deliberately sparse pyramid. A LO-HI that shares no zoom at all with the archive is always an error, regardless of that flag. Which kind of INPUT this is is detected from the file, not the extension. A source may be remote (`https://`, `s3://`, `gs://`), read with byte-range requests like every other subcommand's input; a band ARCHIVE must be local, since the merge reads it by offset.

   LAYER defaults to the file stem, and several bands may share one layer name (the usual case: a coarse and a fine aggregate that are the same layer to a client). Two bands in the SAME layer must not share a zoom -- they would write the same tile ids. Bands in DIFFERENT layers may (tippecanoe's -L): at the zooms they share, each tile carries every band's layer, so `--band 0-13:a.parquet:2024 --band 0-13:b.parquet:2025 --generalize` is one archive with two independently generalized layers. The per-tile size cap applies to each band's tiles before they are combined, not to the combined tile. For a pre-tiled archive LAYER is a label: when bands share zooms it must match the layer name inside the archive, or the merge is refused rather than write two layers of one name into a tile.

   Bands are emitted coarsest-first in the merged archive regardless of listing order.

   Colons in INPUT: the LAYER is only split off the LAST `:` when what follows it has no `/`, `\` or `:`, so a URL, a Windows drive and a `2024:06/` directory stay whole. A drive-relative path with no `\` after the drive, e.g. `C:data.parquet`, also stays whole: a single ASCII letter before the last `:` is treated as a drive letter, not a path, even though `data.parquet` alone would otherwise look like a bare layer name. For the inputs that rule cannot express — one ENDING in a bare colon segment, e.g. a Hive directory `admin:country_code=BR` — spell the band `LO-HI=INPUT[=LAYER]` instead: the range is split at the first `=` and the LAYER at the last, again only when the segment after it has no `/`, `\` or `:`. An INPUT that itself ends in `=VALUE` needs an explicit `=LAYER`.
* `--generalize` — Generalize each GeoParquet band with the normal ladder instead of tiling it verbatim.

   Off by default: a band's premise is that its input is already the right resolution for the zooms it owns, so thinning and simplifying it would re-introduce exactly what banding avoids. Pass this when a band is raw features spanning several zooms and you do want the ladder inside it.
* `--max-tile-size <SIZE>` — Per-tile MVT size cap for bands tiled here (e.g. "500K"). Unset means no cap, matching the verbatim default: a valve that sheds features to fit a byte budget would drop cells the band exists to draw
* `--work-dir <DIR>` — Directory for the per-band intermediates (removed on the way out). Defaults to the system temp directory
* `--allow-missing-zooms` — Allow a pre-tiled band's declared zoom range to overshoot what its archive actually holds.

   Off by default: an overshooting band declares zooms its archive does not have, and those zooms would render silently empty — almost always a `--band` range that disagrees with what the archive was actually tiled with, so it is a hard error unless this is set. Has no effect on a band whose declared range shares no zoom at all with its archive (that is always an error), nor on a band that declares a subrange of its archive (that is always fine, with or without this flag). Pass this only for a deliberately sparse pyramid.
* `-f`, `--force` — Overwrite the output if it exists



## `tylertoo merge`

Concatenate disjoint PMTiles archives into one (issue #498)

**Usage:** `tylertoo merge [OPTIONS] <OUTPUT> <INPUT>...`

###### **Arguments:**

* `<OUTPUT>` — Output PMTiles archive
* `<INPUT>` — Input PMTiles archives, two or more. They must hold disjoint tile ids and agree on tile type and tile compression; the merged archive's bounds are the union of theirs, its zoom range the union of the ranges they declare, and its `vector_layers` the union of theirs (layers sharing an id collapse into one entry spanning their combined zooms, with their fields unioned)

###### **Options:**

* `--work-dir <DIR>` — Directory for the merge's spool file, which holds the merged tile data until the archive is assembled. Defaults to the system temp directory — worth setting when the output is large and `/tmp` is a small tmpfs
* `--report <PATH>` — Write the JSON merge report to this path.

   Includes per-zoom tile counts, which are what a sharded build is checked against: merging N shards must yield the same tiles per zoom as tiling the whole input in one pass.
* `-f`, `--force` — Overwrite the output if it exists



