# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## v0.7.0 (Unreleased)

The converter was rebuilt around **GeoParquet overviews**: instead of
tiling features tile-by-tile, `gpq-tiles` now embeds COG-style
multi-resolution levels in a GeoParquet file (`gpq-tiles overview`) and
exports PMTiles from that file (`gpq-tiles export-pmtiles`). The one-shot
GeoParquet → PMTiles path still exists (`gpq-tiles tiles`, or the bare
form) and is a thin facade over the same two-step chain. The overview
format is specified in
[`context/OVERVIEWS_SPEC.md`](https://github.com/geoparquet-io/gpq-tiles/blob/main/context/OVERVIEWS_SPEC.md)
(draft v0.2.0).

### Breaking Changes

- The legacy per-tile pipeline was removed (#177). `tiles` (and the bare
  form) now runs overview convert into a temporary file, then export.
  Flags tied to the old pipeline — `--streaming-mode`, property filtering
  (`--include`/`--exclude`/`--exclude-all`), output-compression selection,
  `--deterministic` — are gone. The tuning surface is the
  `overview`/`export-pmtiles` flag set, all of which is also accepted by
  the one-shot `tiles` command (#249).
- Python `convert()` is deprecated: it no longer runs the removed legacy
  pipeline and is now a facade chaining `overview()` + `export_pmtiles()`.
  Use the two-step API for the full option surface.

### Features

- **GeoParquet overviews** (`gpq-tiles overview`): embed multi-resolution
  levels alongside your exact source data. The output stays valid
  GeoParquet 1.1 — SQL-queryable, readable over HTTP range requests, the
  finest level is the source data verbatim. `duplicating` (default) and
  `partitioning` layout modes; `gpq-tiles validate` checks a file against
  the spec.
- **Quality ladder** calibrated on rendered corpus sweeps: class-aware
  cell-winner ranking with Overture schema auto-detection, per-level
  density budget (`--drop-rate`, `--drop-gamma`), visibility gates,
  world-space GSD-driven simplification, and a drop-to-fit per-tile byte
  cap on export (`--max-tile-size`).
- **Point clustering** (`--cluster`): cluster winners carry a
  `point_count` column, with numeric aggregation via
  `--accumulate-attribute col:sum` (spec §12).
- **Line coalescing** (on by default): thinned road/river networks are
  merged into continuous strokes at coarse levels instead of fragmenting
  (`--no-coalesce-lines` to opt out; spec §13).
- **Multi-partition input** (#277, #281, #282): `overview`, `tiles`, and
  Python `overview()` accept a local directory, a glob, an `s3://`/`gs://`
  prefix (trailing slash), a `--files-from` manifest (ordered, mixed
  local/remote), or a Python `list[str]` — read as one logical dataset,
  with cross-partition schema/CRS validation, Hive sidecar filtering, and
  a deterministic row-order guarantee. See
  [Multi-Partition Input](https://geoparquet-io.github.io/gpq-tiles/multi-partition/).
- **Remote input** (#210): convert directly from `s3://`, `https://`, and
  `gs://` URLs via HTTP byte-range requests — the input is never
  downloaded whole up front. Uses the standard AWS credential chain with
  unsigned fallback for public buckets; honors `AWS_ENDPOINT_URL` /
  `AWS_SKIP_SIGNATURE` for S3-compatible stores, with targeted hints on
  signature failures.
- **Regional extracts** (`--bbox`, #102): row groups whose bbox statistics
  miss the region are skipped entirely — and, on remote inputs, never
  downloaded.
- **Configurable spill directory** (`--spill-dir`, Python `spill_dir=`,
  #272): place the remote-input spill file on a volume of your choosing;
  the converter projects the spill footprint before pass 1 and warns if
  the volume's free space may not fit it.
- A full-file remote conversion that would stage roughly the whole object
  warns up front, with the equivalent download-then-convert commands, when
  that route is likely faster (#267).
- **PMTiles decoding** (`gpq-tiles decode`, #112): PMTiles → GeoParquet
  with tippecanoe-decode semantics, for any PMTiles v3 MVT archive (not
  just gpq-tiles output).
- **Prebuilt CLI binaries** (#275): GitHub Releases now attach binaries
  for Linux x86_64 (gnu and musl), macOS (Intel and Apple Silicon), and
  Windows x86_64, plus generated release notes.

### Performance

- Remote-input network traffic is bounded to ≈1× the object's bytes,
  regardless of zoom range or pass count: fetched column chunks are held
  in an in-memory cache sized to the largest row group's working set
  (#261) and staged in an on-disk spill file so later passes re-read from
  local disk instead of the network (#219). The spill is best-effort — if
  the volume fills, conversion continues with network re-fetch.
- The convert stage's dominant serial sections were parallelized —
  level assignment across levels, and finest-level write overlapped with
  production (#264).
- **Simple-clip fast path, now on by default** (#239, #255, #256): tile
  clips whose result is a simple ring skip the expensive robust-boolean
  fallback. Opt out with `--no-simple-clip-fastpath` (Python:
  `simple_clip_fastpath=False`).
- Three O(V²) hot spots on large geometries removed: export clip stall on
  large-extent polygons (#237), uncapped validity checking of oversized
  simplification candidates (#242), and the self-intersection scan, now a
  sweepline (#241).
- Export parallelized with Rayon: pass-2 simplification, and per-tile
  clip + MVT encode; bbox-containment clips skip clipping entirely;
  finished tile partitions stream to the PMTiles writer instead of
  buffering.

### Changed Defaults

- `--polygon-visibility` retuned **4.0 → 2.0** (#259): the rendered sweep
  on the #250 fixtures showed gates above 2.0 starve coarse zooms without
  making files smaller, and gates below ~2.0 mostly admit candidates the
  write-time collapse drops anyway (see `corpus/SWEEPS.md`, Decision 6).
- `--line-thinning` retuned 2.0 → 1.0 from the same sweep methodology.

### Fixes

- MultiPolygon parts straddling a tile boundary are no longer silently
  dropped during export clipping (#244).
- Coarse-level export used geographic latitude instead of Web Mercator Y
  in the tile-local transform, distorting high-latitude tiles.
- Inputs with a case-insensitive `level` column collision are rejected up
  front instead of producing an ambiguous overview file.
- Remote URL classification, cross-partition CRS/metadata diagnostics, and
  footer caching hardened during multi-partition review (#277).

### Docs

- Live **Germany buildings demo** (59M features) on the docs site, with a
  hosted PMTiles viewer and a measured head-to-head against the
  gpio → tippecanoe pipeline (#250).
- New [Multi-Partition Input](https://geoparquet-io.github.io/gpq-tiles/multi-partition/)
  and [Remote Reads](https://geoparquet-io.github.io/gpq-tiles/remote-reads/)
  pages — the latter includes the evidence-based DuckDB recipe for
  querying overview files in place (#203).
- [Overview Tuning](https://geoparquet-io.github.io/gpq-tiles/OVERVIEW_TUNING/)
  documents every generalization knob, default, and interaction; docs
  consolidated to one source of truth per topic (#180).

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
