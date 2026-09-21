# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## v0.7.0 (2026-09-19)

The release that replaces the original per-tile pipeline with the
`geo:overviews` architecture. Tiling now runs as *build a multi-resolution
overview GeoParquet, then export tiles from it*, with the overview file as a
first-class artifact you can validate, query with SQL, and re-export.

### Added

- **`geo:overviews` GeoParquet overviews** — `tylertoo overview` embeds
  COG-style multi-resolution levels in a single valid GeoParquet file;
  `tylertoo validate` checks one against the draft spec (#168, #184, #190).
- **`tylertoo export-pmtiles`** — PMTiles v3 export from an overview file,
  with per-level progress logging and incremental checkpoints (#169, #229).
- **`tylertoo tiles`, and the bare form** — one-shot GeoParquet → PMTiles as a
  facade over overview → export, with `--keep-overview`, a controllable spill
  directory, a free-space preflight, and tuning parity with the two-step
  commands (#251, #276, #318, #319).
- **`tylertoo decode`** — PMTiles v3 → GeoParquet with tippecanoe-decode
  semantics and `zoom` / `layer` / `mvt_id` provenance columns (#112, #206).
- **`tylertoo pyramid`** — multi-band archives: several inputs, each owning a
  disjoint zoom range, merged into one PMTiles. Bands may be GeoParquet (tiled
  for you) or already-tiled PMTiles, and bands in different layers may share a
  zoom range (#348, #385, #392).
- **Remote input** — read `s3://`, `https://` and `gs://` GeoParquet over
  byte-range requests, including remote prefix listing (#210, #216, #281).
- **Multi-file input** — a local directory, a glob, an `s3://` / `gs://`
  prefix, a `--files-from` manifest (ordered, mixed local and remote), or a
  Python `list[str]` is read as one logical dataset, with cross-partition
  schema and CRS validation and a deterministic row order (#277, #281, #282).
- **`--spill-dir` and a free-space guard** — place the remote-input spill file
  where you want it; the converter projects the spill footprint before pass 1
  and warns when the volume may not fit it, or when it is on tmpfs (#272,
  #273). A full-file remote conversion that would stage roughly the whole
  object warns up front with the equivalent download-then-convert commands
  (#267).
- **`--bbox` regional extracts** — row-group-level spatial pushdown, so a city
  comes out of a country-scale file without reading — or, on remote input,
  fetching — the rest (#102, #207).
- **`--filter` / `--where`** — SQL-WHERE-style attribute predicates with
  parquet row-group statistics pushdown, timestamp columns included
  (#315, #321).
- **Property selection** — `--include-property`, `--exclude-property` and
  `--exclude-all-properties` (tippecanoe `-y` / `-x` / `-X`), applied at scan
  time on `overview` and at encode time on `export-pmtiles` (#386, #391).
- **`--feature-order`** — pin within-tile draw order to input order or to a
  property, ascending or descending (#361, #366).
- **Magnitude ladder** — `--magnitude-ladder` and `--entry-zoom` let an
  attribute decide the zoom at which each feature first appears (#364, #375).
- **`--verbatim`** — tile the input exactly as given, switching the whole
  generalization ladder off at every level (#345, #360, #367).
- **Zoom-band representation** — `--representation "0-7:point"` draws a band as
  representative points and the rest as polygons in one archive, and
  `--collapse-square` stands in for dropped tiny polygons with area-dithered
  placeholder squares (#279, #317, #322).
- **Tiny-polygon accumulator** — coarse levels keep the area they drop instead
  of losing it (#384, #394).
- **Per-level point clustering** — `--cluster` and `--accumulate-attribute`
  carry cluster winners with a `point_count` column and numeric attribute
  aggregation through the overview pipeline (#170).
- **Line coalescing** — on by default, with `--coalesce-snap`,
  `--coalesce-max-level-rows` and `--no-coalesce-lines` (#183).
- **Cascading simplification** — each level simplifies the one above it rather
  than the source; `--no-cascade` opts out (#218).
- **`--report`** — machine-readable JSON run reports from `overview`,
  `export-pmtiles` and `tiles` (#316, #318).
- **Python overview API** — `overview()`, `validate()` and `export_pmtiles()`
  bindings, with `convert()` kept as a facade over them (#185).
- **Prebuilt CLI binaries** attached to every GitHub Release — Linux x86_64
  (gnu and musl), macOS Intel and Apple Silicon, Windows x86_64 — plus
  generated release notes (#275).
- Antimeridian-suspect features are flagged with a warning at convert time
  (#188, #199).

### Changed

- **Breaking: the legacy per-tile pipeline is gone.** `pipeline.rs`,
  `Converter` and `TilerConfig` were removed along with their library
  re-exports, and every path now runs overview → export (#177, #189). The
  flags tied to the old pipeline — `--streaming-mode`, the old
  `--include` / `--exclude` / `--exclude-all` property filtering,
  output-compression selection, `--deterministic` — are gone; the tuning
  surface is the `overview` / `export-pmtiles` flag set, all of which the
  one-shot `tiles` command also accepts (#249).
- **Breaking: Python `convert()` is deprecated.** It no longer runs the removed
  legacy pipeline; it is a facade chaining `overview()` + `export_pmtiles()`.
  Use the two-step API for the full option surface.
- `--polygon-visibility` retuned **4.0 → 2.0** (#259): the rendered sweep
  showed gates above 2.0 starve coarse zooms without making files smaller, and
  gates below ~2.0 mostly admit candidates the write-time collapse drops
  anyway (`corpus/SWEEPS.md`, Decision 6).
- Per-tile size caps at 500K by default, matching tippecanoe (#280).
- The simple-clip fast path is on by default; `--no-simple-clip-fastpath` opts
  out (#239, #256).
- Line thinning retuned 2.0 → 1.0, point thinning defaults to 16.0 under
  clustering, and junction continuation now defaults off — all from the same
  sweep methodology.
- Overview footers declare spec version 0.2.0 (#184, #190).
- The declared MSRV is 1.95 — it had claimed 1.75 — and CI verifies it (#358).

### Fixed

- **PMTiles directories**: the contiguous-offset rule now applies to leaf
  entries, oversized directories split into leaves, and `leaf_dirs_offset` is
  never written as 0 (#356, #377, #378).
- **MVT polygons are cleaned in tile space after quantization**, so
  quantization can no longer leave self-intersecting or degenerate rings
  (#383, #393). Ring winding follows the spec.
- `export-pmtiles` declares the requested minimum zoom even when the coarse
  levels generalized to nothing (#380, #390).
- `coalesced_count` is withheld from tiles when it is 1 everywhere (#379,
  #389).
- Renamed source columns are published under their source name (#359, #365).
- `DATE`, `TIMESTAMP` and `DECIMAL` properties are encoded instead of dropped,
  with timestamps UTC-stamped.
- `--tile-buffer` is read as tile pixels, not MVT extent units, and tile
  membership derives from the buffer-expanded bbox.
- MultiPolygon parts that split into pieces at a tile boundary are kept (#244).
- Empty overview levels auto-clamp instead of failing the run (#211).
- Reserved-column collisions are auto-renamed rather than rejected (#288).
- An explicit null GeoParquet `crs` is assumed to be OGC:CRS84.
- Coarse-level export used geographic latitude instead of Web Mercator Y in the
  tile-local transform, distorting high-latitude tiles.
- An input with a case-insensitive `level` column collision is rejected up
  front instead of producing an ambiguous overview file.
- `decode` names the section or tile that failed to decompress, with its byte
  offset (#381, #388).
- Oversized remote column chunks are no longer re-fetched per page (#261), and
  `http://` inputs work (#262).

### Performance

- Remote-input network traffic is bounded to about 1× the object's bytes —
  down from ~3× — regardless of zoom range or pass count: the selected row
  groups are staged up front so the fetches coalesce and parallelize, fetched
  column chunks are cached to the largest row group's working set (#261), and
  an on-disk spill file serves passes 2 and later from local disk instead of
  the network (#219, #286, #287). The spill is best-effort; if the volume
  fills, conversion continues with network re-fetch.
- Export runs a single-read fan-out pass that kills cross-level prefix and
  per-partition re-reads, and parallelizes clip, simplify, MVT encode and gzip
  (#227, #228, #233, #235).
- Large polygons split top-down recursively instead of being clipped whole
  (#226), and three O(V²) hot spots in clipping and validity checking were
  replaced or capped (#237, #241, #242).
- Memory is budgeted rather than guessed: winner-grid level waves, partition
  waves sized from the densest partition and from available cores, and an auto
  profile that picks RAM-vs-spill from measured geometry (#293, #294, #303,
  #305, #306, #311).
- Parquet row-group encoding and GeoParquet WKB encoding run in parallel
  (#296, #304).
- `--profile` presets over a single-read pipelined pass-2 engine (#212, #213).

### Internal

Clippy, rustfmt, dependency, CI and benchmark-harness work is omitted from this
section; `git log v0.6.0..v0.7.0` holds the full 465-commit record.

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
