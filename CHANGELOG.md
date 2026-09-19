# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## v0.7.0 (2026-09-19)

### Feat

- **convert**: tiny-polygon accumulator — coarse levels keep the dropped area (#384) (#394)
- **pyramid**: bands in different layers may share a zoom range — multi-layer archives (#385) (#392)
- --include-property / --exclude-property / --exclude-all-properties (#386) (#391)
- **convert**: attribute-driven entry zoom — the magnitude ladder (#364) (#375)
- **pyramid**: tile GeoParquet bands directly, one shot (#345) (#368)
- **export**: add --feature-order to pin within-tile draw order (#361) (#366)
- **convert**: add --verbatim, tiling an input exactly as given (#345, #360) (#367)
- **pyramid**: merge per-band archives into one multi-band pyramid
- **export**: default per-tile size cap at 500K (tippecanoe parity)
- **filter**: timestamp column support in --filter
- **cli,python**: expose --representation and --collapse-square (#317, #279)
- **overview**: zoom-band representation selector + tiny-polygon placeholder squares (#317, #279)
- **convert**: attribute filter --filter/--where with stats pushdown (#315)
- **cli**: ergonomic one-step tiles — --keep-overview, controllable temp, visible disk cost (#314)
- **cli**: tiles tuning parity — expose --gsd and --report (#316)
- **core**: warn when remote-input spill dir is on tmpfs/ramfs (#273)
- **core**: AWS_SKIP_SIGNATURE hint on signature errors at custom endpoints (TDD green)
- **cli**: friendly single-file-only rejections on validate/decode/export-pmtiles (TDD green)
- **core**: derive default layer name from multi-partition inputs (TDD green)
- **python**: overview() accepts list[str] input (explicit ordered parts)
- **cli**: --files-from manifest on tiles and overview
- **core**: remote prefix listing, --files-from manifest core, store sharing
- **core**: retune coarse-zoom polygon defaults from the #259 sweep
- **core**: plumb ConvertSource through the streaming pipeline
- **core**: ConvertSource abstraction + local multi-partition resolution
- **core**: configurable input-spill directory + free-space guard (#272)
- **ci**: attach prebuilt CLI binaries and real release notes to GitHub Releases
- **core**: warn on large full-file remote convert (#267)
- **export**: default simple_clip_fastpath on; add --no-simple-clip-fastpath opt-out (#256)
- **cli**: expose overview tuning + memory flags on the tiles one-shot command
- **export**: incremental checkpoints + per-level progress logging (#229)
- **core**: cascade provenance, spec + tuning docs, Python surface (#218)
- **core**: cascade flag, simplify_cascade fold, i_overlay candidate repair (#218)
- remote input support — convert directly from s3://, https://, gs:// (#210)
- **core**: opt-in zoom-scaled per-level row-group sizing policy (TDD green)
- **cli**: add decode subcommand (PMTiles -> GeoParquet)
- **core**: PMTiles -> GeoParquet decode module (#112)
- **core**: add PMTiles read-side primitives
- warn on antimeridian-suspect features at convert time + user docs (#188)
- **bench**: remote-storage (S3 range-request) access benchmark
- **core**: enforce strict cluster sum invariant (spec §12.1)
- **core**: emit complete coalescing provenance and ranks as object map
- **core**: stamp overview footers with spec version 0.2.0
- **python**: re-point convert() at the overview facade
- **cli**: reimplement tiles as a facade over overview convert -> export
- **python**: expose the overview pipeline (E3)
- **core**: default junction continuation OFF per maintainer sweep review
- **core**: chain density budget, junction continuation, coalescing on by default
- **cli**: add --coalesce-lines, --coalesce-snap, --coalesce-max-level-rows
- **core**: wire line coalescing through both overview pipelines
- **core**: add line network coalescing engine (Q3)
- **cli**: default point-thinning 16.0 when clustering
- **core**: validate clustering metadata (point_count column + values)
- **cli**: --cluster and --accumulate-attribute flags on overview
- **core**: per-level point clustering with point_count + numeric aggregation
- **cli**: expose --no-streaming and --read-batch-size overview flags
- **core**: two-pass streaming overview convert (H3)
- **overview**: per-level row-group sizing + string/geometry stats suppression (H1)
- **overview**: add export-pmtiles (E0)
- **overview**: add Q2 density-based per-level budget
- **overview**: retune line-thinning default 2.0 -> 1.0
- **overview**: expose --gsd-base knob, document tuning, add true-scale renders
- **overview**: class-aware cell-winner ranking (Q1)
- **cli**: add overview and validate subcommands
- **core**: add overview convert pipeline (P5)
- **overview**: add overview reader and conformance validator (P4)
- **overview**: add level metadata model and GeoParquet overview writer (P3)
- **overview**: add world-space GSD-driven simplification (P2)
- **overview**: add pure level-assignment engine (P1)
- **simplify**: add coalesced-linestring simplification and noop removal
- **python**: add simplify_factor to Python bindings
- **cli**: add --simplify and --simplify-factor flags
- **pipeline**: integrate simplify_geometry_for_tile into encoding
- **simplify**: add unified simplify_geometry_for_tile helper
- **simplify**: add boundary-preserving polygon ring simplification
- **simplify**: add boundary-preserving linestring simplification
- **simplify**: add tile boundary detection
- **core**: add simplify_factor to TilerConfig
- **core**: Wave 3 - adaptive retry loop and zoom propagation
- **core,cli**: Wave 2 - adaptive threshold integration
- **core**: Wave 1 - adaptive threshold infrastructure
- **coalesce**: implement property passthrough for keep-first/strict modes
- **coalesce**: wire predictive architecture into pipeline
- **pipeline**: implement tile-level geometry coalescing
- **pipeline**: wire coalesce_config through tile encoding
- add CoalesceConfig and CLI flags for geometry coalescing
- **core**: add SpatialGrid and CoalesceTargets for coalescing
- **core**: implement coalesce_geometries for geometry merging
- **core**: add covering_tiles and percentile utilities
- **cli**: add --memory-budget flag to control sort segment size
- **core**: implement disk-spilling external sort for memory-bounded processing
- **core**: implement spatial bucketing for memory-bounded tile generation
- **core**: add row group filtering and memory-bounded processing infrastructure
- **core**: improve error messages for directory path failures
- support partitioned GeoParquet directories
- add zoom_range_for_bbox() unified min/max zoom function
- add min_zoom_for_bbox() for visibility-based zoom filtering
- add per-feature max zoom optimization (--auto-max-zoom)
- **core**: skip clipping for features fully contained within tile bounds
- **core**: add drop-smallest filtering to legacy TileIterator path
- **core**: add drop-smallest filtering to streaming path
- **core**: implement drop-smallest filtering in encode_tile_from_raw
- **config,cli,python**: add drop-smallest-as-needed configuration
- **core**: add geometry_pixel_area_world dispatcher for all types
- **core**: add Sortable trait to TileRef for external sorting
- **core**: add TileRef for lightweight sorting
- **core**: add disk-backed GeometryStore for bounded memory

### Fix

- **core**: raise the glob floor to 0.3.4 and convert with into
- **export**: declare the requested min zoom even when coarse levels generalized to nothing (#380) (#390)
- **mvt**: clean polygons in tile space after quantization (#383) (#393)
- **export**: withhold coalesced_count from tiles when it is 1 everywhere (#379) (#389)
- **pmtiles**: apply contiguous-offset rule to leaf directory entries (#377) (#378)
- **decode**: name the section or tile that failed to decompress, with byte offset (#381) (#388)
- **export**: publish renamed source columns under their source name (#359) (#365)
- **core**: release the producer before joining it in the streaming engines (#362) (#363)
- declare the real MSRV (1.95, was 1.75) and verify it in CI
- **input**: import ObjectStoreExt for object_store 0.14
- **pyramid**: rebuild the band merge around review findings
- **cli**: teach the bare-form rewrite about pyramid
- **pmtiles**: never write leaf_dirs_offset = 0
- **overview**: box LevelSink::Spill for arrow 59
- **pmtiles**: split oversized directories into leaves in PmtilesWriter
- **export**: encode UTC-stamped timestamps instead of advertising nulls
- **export**: encode DATE, TIMESTAMP and DECIMAL instead of dropping them
- **export**: stop out-of-domain bboxes claiming every tile column
- **export**: --tile-buffer is tile pixels, not MVT extent units
- **export**: derive tile membership from the buffer-expanded bbox
- **deps**: pin aws-smithy-types below 1.7.0, unbreaking Check
- **ci**: pin rust-toolchain at the v1 tag, unbreaking CI Lint
- **python**: bump pip past PYSEC-2026-3721, unbreaking Python Quality
- **input**: replace deprecated GlobError::into_error, unbreaking CI on main
- **python**: sync tylertoo.pyi tile_size_limit default to 512000
- **convert**: assume OGC:CRS84 for explicit null GeoParquet crs
- **python**: add filter kwarg to overview type stub
- **overview**: auto-rename reserved-column collisions instead of rejecting (#288)
- **core**: review fixes — remote URL classification, CRS/metadata diagnostics, cached single footer
- **python**: add spill_dir to type stub
- **core**: stop re-fetching oversized remote column chunks per page (#261)
- **core**: enable allow_http for http:// remote inputs
- **clip**: keep MultiPolygon parts that split into pieces at tile bounds (#244)
- **core**: address CI gates for #211 clamp (clippy complexity, must-use, xenon)
- **core**: auto-clamp empty overview levels instead of failing (#211)
- green the mypy and security-audit gates for remote input
- **python**: add bbox parameter to overview() type stub
- **core**: clippy/MSRV cleanups in bbox filter code
- **python**: wire bbox regional extract through the Python bindings
- **core**: emit spec-compliant MVT polygon ring winding
- **core**: migrate to arrow/parquet 58 APIs for geoarrow 0.8
- **bench**: migrate deprecated criterion::black_box to std::hint::black_box
- **core**: validate geo:overviews footer against actual row groups on open
- **core**: reject nonsensical overview conversion knobs up front
- **core**: row-align overview winner tables around skipped hostile geometries
- **core**: use Web Mercator Y for latitude in tile-local coordinate transform
- **clippy**: last 1.96 lint in debug_antarctica test
- **clippy**: appease Rust 1.96 lints (unnecessary_sort_by, manual_checked_ops)
- **clippy**: resolve -D warnings failures across overview + simplify
- **overview**: reject case-insensitive level column collisions (F2)
- **test**: increase memory budget to 500MB for tarpaulin overhead
- **test**: increase memory budget for CI environments
- **core**: fix clippy len_zero warning in test
- **core**: improve CannotReduceFurther error with actionable suggestions
- **core**: improve CannotReduceFurther error with actionable suggestions
- **core**: complete adaptive threshold algorithm integration
- **core**: track actual RSS instead of throughput for memory reporting
- **coalesce**: address adversarial review findings
- address clippy warnings
- **core**: add gap-based dropping to clustering path + tests
- **core**: sort records by original Hilbert index for gap-based dropping
- **core**: make temp file names unique with pid/tid for parallel safety
- **core**: remove unused segment_idx field in external sort
- **core**: prioritize exact "geometry" column match over partial
- **core**: apply PR #116's multi-file process_geometries_parallel
- use single pipeline for all files, not separate pipelines per file
- **core**: apply PR #116 directory support to get_row_group_count
- **core**: update extract_crs to handle directory paths
- update test calls after rebase on PR #133
- **test**: use array instead of vec for fixed-size test data
- **bench**: remove unnecessary mut and casts in tile_ref benchmark

### Refactor

- **core**: split the four conversion drivers into named phases
- **core**: fold away the near-identical writer and codec pairs
- **core**: sweep dead code and tighten the public surface
- **core**: remove legacy per-tile pipeline
- **tiles**: complete excision — drop CLI flags + lib re-export
- **tiles**: remove unproven zoom-dependent simplification (#158)
- **overview**: unify Crs and METERS_PER_DEGREE in level.rs
- **cli**: replace --auto-max-zoom with --zoom-by-area flags
- **core**: replace auto_max_zoom with zoom_by_area in TilerConfig

### Perf

- **export**: halve member-store fill budget to 128 MiB
- **export**: single-read fan-out pass 2 for partitioning mode (#235)
- **convert**: bound pass-1 winner-grid memory with budgeted level waves (#306)
- **export**: size partition wave from densest partition, not flat constant (#311)
- **writer**: parallelize GeoParquet WKB encode (#304)
- **export**: memory-budget preflight for auto partition wave (#303)
- **convert**: size auto RAM-vs-spill from measured geometry (#305)
- **export**: auto-scale partition wave to available cores (#293)
- **convert**: recalibrate auto buffered-output estimate to measured cost (#294)
- **convert**: add per-phase RSS instrumentation (#295)
- **convert**: make auto profile pick backing from workload, not mode (#294)
- **core**: parallelize parquet row-group encoding (#296)
- **core**: fuse pass-1 geometry traversals in streaming convert (#274)
- **convert**: --in-flight-batches auto — core-aware pass-2 concurrency
- **remote**: stage selected row groups up front to coalesce and parallelize fetches (#286, #287)
- **core**: parallelize explicit-list remote connects (TDD green)
- **core**: spill remote input to disk to bound re-reads to ~1× (#219)
- **core**: parallelize convert's dominant serial stages (#264)
- **bench**: refresh big-file profile, add sweep harness + convert guard + docs
- **export**: opt-in simple-clip fast path skipping i_overlay fallback (#239)
- **clip**: replace O(V²) self-intersection scan with sweepline (#241)
- **convert**: cap O(V²) validity check on oversized RDP candidates (#242)
- **export**: fix O(V²) clip stall on large-extent polygons (#237)
- **export**: single-read fan-out scan, kill cross-level prefix re-read (#233)
- **export**: wave-batched shared read, kill per-partition re-reads (#228)
- **export**: parallelize scan_level and gzip compression (#227)
- **export**: top-down recursive tile splitting for large polygons (#226)
- **core**: cascading simplification in all three pass-2 paths (#218)
- **core**: single-read pipelined pass-2 engine + --profile presets (#213, #212)
- **bench**: read AWS bucket/region/profile from env vars
- **bench**: four-way overview layout benchmark + column-per-zoom prototype
- **bench**: row-group sizing sweep against real S3 (#202)
- **core**: add --bbox row-group filtering for regional extracts (#102)
- **bench**: demonstrate the remote latency floor with a parallel range-request reader
- **core**: stream finished tile partitions to PMTiles writer (H3b)
- **core**: parallelize export clip and MVT encode with rayon (H3c lever 2)
- **core**: export bbox-containment clip fast path (H3c lever 4)
- **core**: parallelize pass-2 simplification with rayon (H3c lever 1)
- **core**: cut validation waste in overview simplify (H3c lever 3)
- **core**: H3(c) wall-time profile of overview pipeline
- **core**: add row-level predicate pushdown for spatial filtering
- **core**: add column projection for geometry-only reads
- **core**: revert to extsort crate to fix 7x performance regression

## v0.6.0 (2026-03-11)

### Feat

- implement point clustering with position averaging (#25)
- implement accumulator system for attribute aggregation (#23)
- implement gap-based density detection (#24)
- support WKT geometry encoding (#35)
- implement tiny polygon accumulation (#85)
- **profiling**: add fine-grained spans to read_parquet phase
- add time profiling with tracing
- add memory profiling with dhat

### Fix

- remove needless borrow in benchmark
- resolve clippy warnings and unused imports
- remove WKT fixture from repo, tests skip when missing
- gracefully skip WKT tests when fixture is missing
- skip profiling integration tests when dhat-heap feature enabled
- use tempfile crate for proper temp directory isolation in tests
- use cross-platform temp directories in integration tests

### Refactor

- replace wagyu-rs with i_overlay for polygon clipping

### Perf

- parallel row group I/O for ~24% speedup
- reuse file handle across row groups (#41)
- parallelize tile encoding in Phase 3 (#90)

## v0.5.0 (2026-03-10)

### Feat

- **core**: wire up pipeline to use WorldCoord throughout (Phase 2)
- **core**: add WorldCoord-based hierarchical clipping (Phase 2)
- **core**: add WorldCoord-based feature drop functions (Phase 2)
- **core**: add WorldCoord support to MVT encoding and validation (Phase 1)
- **core**: add WorldCoord support to clipping modules (Phase 1)
- **core**: add WorldCoord-based simplification functions (Phase 1)
- **core**: add WorldCoord type for 32-bit integer coordinates (Phase 0)
- **clip**: integrate wagyu-rs for robust polygon clipping
- add --deterministic flag and fix PR #63 review feedback

### Fix

- **clip**: enable U-shape split test with wagyu-rs v0.2.1
- **clip**: add wagyu fallback for edge case geometry handling (#94)
- change default compression to gzip and add CRS validation
- change default compression to gzip for compatibility
- implement leaf directory support for large PMTiles archives
- **core**: clamp tile coordinates and bounds to valid ranges
- **core**: fix issue #83 - geometry coordinates collapsing to zeros
- resolve clippy warnings in tests
- **core**: align feature_drop coordinate precision with MVT encoding
- **ci**: download fixtures from release instead of LFS
- **ci**: remove 1.8GB fixture from LFS to fix bandwidth quota
- Remove unused MIN_EXPECTED_TILES constant
- Remove #[ignore] from regression tests - fixture is in LFS
- use is_empty() instead of len() > 0 for Clippy
- Fix clippy warning and add clipping benchmarks
- **tile**: clamp latitude to Web Mercator bounds
- use wagyu-rs from crates.io instead of path dependency
- **ci**: update benchmark group names after consolidation

### Perf

- Add pre-clip bounding box filter for large geometries
- Implement hierarchical clipping across zoom levels
- Replace Wagyu with Sutherland-Hodgman for tile clipping

## v0.4.0 (2026-02-25)

### Feat

- **python**: add progress callback support
- **python**: add streaming mode and parallel control parameters
- **python**: add property filtering and layer name parameters

### Fix

- add version sync safeguards and fix pyproject.toml version mismatch
- add clippy allow for too_many_arguments and add clippy to pre-commit
- use workspace dependency for gpq-tiles-core version

## v0.2.0 (2026-02-25)

### Fix

- **release**: complete v0.2.0 release setup

## v0.1.0 (2026-02-24)

### Feat

- set up commitizen for automated versioning and releases
- **quality**: warn about pathologically small row groups
- default to zstd compression, expose parallel options in CLI
- add progress bars for cleaner output
- parallelize geometry processing within row groups (#37)
- parallelize tile processing for large geometries (#33)
- **cli**: add --streaming-mode flag with progress reporting
- **pipeline**: implement ExternalSort streaming mode
- **pipeline**: add StreamingMode::ExternalSort variant
- **core**: add external sort and WKB serialization modules
- **streaming**: add StreamingPmtilesWriter with LowMemory mode
- **streaming**: add memory budget configuration and tracking
- **streaming**: add row-group-based streaming tile generation
- **quality**: add GeoParquet file quality detection for streaming
- add tile deduplication with XXH3 hashing and run_length encoding
- add compression options (gzip, brotli, zstd, none) for PMTiles output
- add property filtering with --include/-y, --exclude/-x, --exclude-all/-X flags
- add 17K feature fixture for parallelization benchmarks
- add tilestats metadata to PMTiles output
- auto-extract field metadata from GeoParquet schema
- add field metadata support to PMTiles writer
- derive layer name from input filename, add --layer-name CLI flag
- complete Phase 5 Python bindings with uv/ruff tooling
- add benchmark suite with generate_tiles_from_geometries API
- **pipeline**: add Rayon parallel tile generation
- **pipeline**: wire spatial indexing into tile generation
- **spatial-index**: add space-filling curve sorting for efficient tile generation
- complete Phase 3 with density-based dropping
- integrate feature dropping into pipeline (Phase 3)
- add point thinning (1/2.5 drop rate per zoom)
- add line dropping (coordinate quantization algorithm)
- implement tiny polygon dropping with diffuse probability
- implement PMTiles v3 writer (Tasks 7-9)
- implement tiler pipeline wiring clip → simplify → MVT
- implement MVT encoding for vector tiles
- add golden comparison tests against tippecanoe output
- implement geometry clipping with correct BooleanOps
- implement zoom-based simplification
- implement Arrow-native geometry batch processing (TDD green)

### Fix

- **release**: add version to core dep, add READMEs, fix benchmark filter
- **release**: install protoc inside manylinux container
- **python**: copy README into crate for sdist builds
- **ci**: use tag triggers for release, skip slow benchmarks
- consolidate release workflows and fix version bump detection
- guard against degenerate linestrings in simplify and fix flaky test
- PMTiles now compatible with pmtiles.io and standard viewers
- **golden**: update stale Z8 test to use full pipeline
- resolve three medium/low priority issues
- simplify geometry in tile-local pixel coordinates
- handle antimeridian crossing in tiles_for_bbox
- **clip**: preserve all polygon parts when clipping produces MultiPolygon
- use real bbox calculation instead of world bounds
- resolve CI timeouts and coverage linker errors
- upgrade pyo3 0.24 → 0.28 for Python 3.14 support
- resolve CI failures for benchmark, check, test, and security audit

### Perf

- **streaming**: add memory benchmarks for streaming pipeline
