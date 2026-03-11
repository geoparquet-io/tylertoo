# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-02-25

### Added

**Python API Enhancements** (closes [#45](https://github.com/geoparquet-io/gpq-tiles/issues/45))

- Property filtering: `include`, `exclude`, `exclude_all` parameters
- Layer name override: `layer_name` parameter
- Streaming mode: `streaming_mode` parameter (`"fast"` or `"low-memory"`)
- Parallel controls: `parallel_tiles`, `parallel_geoms` parameters
- Progress callbacks: `progress_callback` parameter with detailed phase events

Python API now has full feature parity with CLI.

### Changed

- Refactored Python bindings from `Converter::convert()` to `generate_tiles_to_writer()` API
- Progress callback automatically uses `ExternalSort` mode (only mode with progress events)

## [0.2.0] - 2026-02-25

### Fixed

- Release workflow setup and version synchronization

## [0.1.0] - 2026-02-24

### Added

**Core Functionality**
- GeoParquet to PMTiles conversion with MVT encoding
- Property filtering with `--include`, `--exclude`, `--exclude-all` flags (tippecanoe `-y/-x/-X` compatibility)
- Compression options: gzip, brotli, zstd (default), none
- Streaming modes: fast (row-group based) and low-memory (external sort)
- Tile deduplication via XXH3 hashing and run-length encoding
- Progress bars with `--verbose` flag showing phase progress
- Quality detection and warnings for unoptimized GeoParquet files

**Parallelization**
- Parallel tile processing for large geometries spanning many tiles
- Parallel geometry processing within row groups
- CLI flags `--no-parallel` and `--no-parallel-geoms` for debugging

**Metadata & Schema**
- Auto-extraction of field metadata from GeoParquet schema
- Tilestats generation in PMTiles metadata
- Vector layers metadata with field types
- Layer name derivation from input filename or `--layer-name` flag

**APIs**
- **CLI**: Full-featured command-line interface (`gpq-tiles`)
- **Rust**: Library API with `Converter` (high-level) and `generate_tiles` (low-level)
- **Python**: Basic bindings via pyo3 (`gpq_tiles.convert()`)

**Testing & Benchmarks**
- 329 tests (Rust unit, integration, golden tests)
- Streaming benchmarks measuring memory usage
- External sort integration tests
- Golden tests against tippecanoe v2.49.0 output

**Documentation**
- Comprehensive API reference (CLI, Python, Rust)
- Advanced usage guide (performance tuning, troubleshooting, CI/CD integration)
- Getting started guide with examples
- Architecture documentation with tippecanoe alignment notes
- Streaming design specification

**CI/CD**
- Release workflow for crates.io publishing
- Python wheel builds (Linux, macOS, Windows)
- Documentation deployment to GitHub Pages
- Codecov integration for coverage reporting
- Dependabot for dependency updates
- CodeRabbit for PR reviews

### Fixed
- Degenerate linestring handling in simplification (<2 points)
- Flaky streaming memory tracking test

### Changed
- Default compression changed from gzip to zstd (5s faster encoding on 3.3GB test file)
- Documentation organization: `docs/` for mkdocs site, `context/` for architecture
- All documentation verified against actual benchmarks (removed speculative claims)

### Performance
- Zstd compression: 2:59 encoding time, 254MB output (3.3GB input, zoom 0-8)
- Gzip compression: 3:04 encoding time, 175MB output (same test)
- Row-group streaming: memory bounded by largest row group (~100-200MB typical)

### Notes
- Tested against tippecanoe v2.49.0 for MVT output compliance

[0.3.0]: https://github.com/geoparquet-io/gpq-tiles/releases/tag/v0.3.0
[0.2.0]: https://github.com/geoparquet-io/gpq-tiles/releases/tag/v0.2.0
[0.1.0]: https://github.com/geoparquet-io/gpq-tiles/releases/tag/v0.1.0

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
