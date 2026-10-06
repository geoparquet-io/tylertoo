# Changelog

Notable changes to this project are recorded here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and releases follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## v0.8.0 (2026-10-06)

Sharded builds, much faster conversion and export, and a long list of
robustness fixes. Some commands now refuse to overwrite existing files; see
**Changed** before upgrading.

During development, `main` carried versions 0.8.0 to 0.11.0. None of those
were released. This is the first release after v0.7.1.

### Added

- **Sharded builds:** `tiles --shard I/N` builds one disjoint slice of a
  large job, `--tile-range` limits a build to part of the tile space, and
  `tylertoo merge` joins the shards into one archive. The merged result
  matches a single-machine build (#498, #510, #537, #541, #555).
- **Saved convert plans:** `--save-plan` writes the expensive first pass to a
  file, `--plan` reuses it, and `tiles --plan-only` writes the plan without
  exporting tiles (#505, #521, #560, #574, #600).
- **`--max-zoom auto`** picks the finest zoom from the data, similar to
  tippecanoe's `-zg`. In Python, pass `max_zoom="auto"` (#444, #573).
- **`--feature-id <COLUMN>`** sets stable MVT feature ids from a column, so
  MapLibre `setFeatureState` works across tiles and zooms (#443, #568).
- **`tylertoo stats`** reports tile sizes per zoom (count, total, mean, p50,
  p99, max), with `--largest N` and `--json` (#551, #557).
- **`pyramid --feature-order`**, and `--band` now accepts remote inputs and
  paths that contain colons (#374, #491, #598).
- **Nested properties:** struct, list and map columns reach the tiles as JSON
  strings instead of being dropped. Columns that are still skipped produce a
  warning (#434, #599).
- **`--bbox` on GeoParquet 2.0 files** skips row groups using the file's
  native geometry statistics (#497, #500).
- **Profiling:** `TYLERTOO_PROFILE_JSON` writes per-phase timers and memory
  use, including peak memory within each phase (#502, #550, #571, #627).

### Changed

- **Existing outputs are no longer overwritten by default.** `tiles`, the
  bare `tylertoo IN OUT` form, `overview`, `export-pmtiles` and `decode`
  refuse to write over an existing file unless you pass `-f`/`--force`. A
  directory at the output path is always refused (#427, #552, #595).
- **Zoom limit:** zooms above 30 are rejected up front. Before, they were
  accepted and the run hung (#371, #488).
- **Out-of-range input fails loudly:** when every feature falls outside the
  valid coordinate range (for example, meters labelled as degrees), the
  conversion fails instead of writing an empty archive. Partial drops are
  counted and reported, and warnings from the core library now reach the
  CLI (#429, #493, #553, #563).
- **Clustered PMTiles:** tiles are written in tile-id order, and the header
  min and max zoom match the tiles actually stored, so `go-pmtiles verify`
  accepts the archives (#501, #506, #516, #529, #554).
- **Pyramid bands** are checked against the tiles they really contain, not
  their declared zoom range (#495, #503, #514, #527).
- **Rust API:** `tylertoo-core` has breaking changes. The level-assignment
  and cluster functions take `&FeatureTable` instead of `&[AssignFeature]`;
  see `crates/core/api/tylertoo-core.txt` for the full surface (#543, #630).

### Fixed

- Features that cross the antimeridian keep the part past ±180°, as in
  tippecanoe (#342, #642).
- At zoom 15 and above, the top rows of tiles were lost because latitude was
  clamped at ±85.05 instead of the exact Web Mercator limit (#416, #484).
- On the musl Linux binary, large runs no longer abort from memory
  fragmentation (#480, #483).
- `--profile auto` and `--partition-wave auto` respect container memory
  limits (cgroup v1 and v2, as used by Docker, Kubernetes and Slurm) (#481,
  #485).
- Killed runs no longer leave a truncated `overview` or `decode` output in
  place of the previous file (#427, #595).
- GeometryCollections are encoded as one feature per geometry type instead of
  being dropped (#431, #592).
- Hostile or corrupt input is rejected with an error instead of a crash or
  wrong output: malformed WKB, truncated or hostile remote Parquet footers,
  hostile PMTiles archives, out-of-tile coordinates, NaN values, and
  invalid `--extent` or `--tile-buffer` values (#399, #406, #417, #428, #430,
  #432, #433, #623, #632).
- Simplifying very long lines no longer overflows the stack (#575, #578).
- Output is byte-identical across thread counts (#423, #487, #609, #610,
  #611).
- The first pass fails early with a clear message when its feature table
  cannot fit in memory, and pass-1 line memory stays within its limit (#449,
  #543, #549, #569).
- Writing more than 32,767 row groups now works (#507, #508, #509).

### Performance

- Export is about 1.75× faster (#535, #559), and checkpoints cost the same
  no matter how large the archive is (#459, #528).
- The first pass and the level assignment run in parallel (#460, #504, #534,
  #564).
- Pass-2 reads run in parallel, and spilling to disk no longer blocks (#494,
  #533, #536).
- Memory use is lower: the pass-1 feature table shrank to 42 bytes per row,
  and the density table and line-joining step use less memory (#543, #565,
  #570, #579, #581, #626).
- Zoom levels that share a simplification step compute it once (#499, #525).

### Internal

- Docs rebuilt around two tested tutorials (#639). Release builds check for
  integer overflow (#432, #589). Line clipping now uses i_overlay 9 only
  (#205, #435, #597). New regression guards, benchmarks against tippecanoe,
  sharded slow tests, dependency updates and CI work (#447, #448, #524, #558,
  #562, #566, #567, #612, #619).

## v0.7.1 (2026-09-23)

Packaging and documentation updates for v0.7.0, with easier installation and
a new quickstart. The tiling engine is unchanged.

### Added

- **Python wheels:** `pip install tylertoo` uses prebuilt wheels on Linux
  x86_64 and aarch64 (manylinux and musllinux, including Alpine), macOS Intel
  and Apple Silicon, and Windows x86_64. Each stable-ABI wheel (`abi3-py39`)
  supports CPython 3.9 and newer without source builds (#410, #473).
- **`cargo binstall tylertoo`** downloads a prebuilt release binary without
  requiring Rust or `protoc` (#411, #469).
- **60-second quickstart** in the README and at the start of the
  getting-started guide, using a downloadable 28 MB sample file. Every
  command was tested (#413, #472).
- **Community files:** `SECURITY.md` with private vulnerability reporting, a
  code of conduct, issue forms, a PR template, and issues labelled
  good-first-issue (#415, #467).

### Changed

- **Reproducible builds:** `Cargo.lock` is committed, and release binaries,
  wheels and crates.io publishes use `--locked` to build from reviewed
  dependencies. v0.7.0 was built without a lockfile (#412, #468).
- **Clearer README intro and a curated v0.7.0 changelog.** The Brazil tutorial
  now links to the 52-file remote manifest; the previously named single file
  never existed. The gpio commands no longer use a flag absent from gpio
  1.5.0 (#414, #472).

### Fixed

- **Release pipeline:** natively built wheels are smoke-tested (installed and
  imported) before publication. Manual workflow re-runs cannot upload wheels
  under an already released version (#473).

### Internal

- uv.lock version-sync hook and CI check (#466); geometry-test-data submodule
  setup documented (#464, #474); gpq-tiles tombstone runbook (#470);
  rust-toolchain action re-pin (#475); dependency and action bumps (#477,
  #478).

## v0.7.0 (2026-09-19)

Replaces the original per-tile pipeline with `geo:overviews`: build a
multi-resolution overview GeoParquet, then export tiles from it. The overview
file can be validated, queried with SQL, and re-exported.

### Added

- **`geo:overviews` GeoParquet overviews** — `tylertoo overview` embeds
  COG-style multi-resolution levels in a single valid GeoParquet file;
  `tylertoo validate` checks one against the draft spec (#168, #184, #190).
- **`tylertoo export-pmtiles`** — PMTiles v3 export from an overview file,
  with per-level progress logging and incremental checkpoints (#169, #229).
- **`tylertoo tiles`, and the bare form** — one-shot GeoParquet → PMTiles via
  overview → export, with `--keep-overview`, a configurable spill directory,
  a free-space preflight, and the same tuning options as the two-step commands
  (#251, #276, #318, #319).
- **`tylertoo decode`** — PMTiles v3 → GeoParquet with tippecanoe-decode
  semantics and `zoom` / `layer` / `mvt_id` provenance columns (#112, #206).
- **`tylertoo pyramid`** — merge several inputs with disjoint zoom ranges into
  one PMTiles archive. Bands may be GeoParquet (tiled during conversion) or
  existing PMTiles. Bands in different layers may share a zoom range (#348,
  #385, #392).
- **Remote input** — read `s3://`, `https://` and `gs://` GeoParquet over
  byte-range requests, including remote prefix listing (#210, #216, #281).
- **Multi-file input** — a local directory, a glob, an `s3://` / `gs://`
  prefix, a `--files-from` manifest (ordered, mixed local and remote), or a
  Python `list[str]` is read as one dataset, with cross-partition
  schema and CRS validation and a deterministic row order (#277, #281, #282).
- **`--spill-dir` and a free-space guard** — choose where to store the
  remote-input spill file. Before pass 1, the converter estimates its size
  and warns about potentially insufficient space or tmpfs storage (#272,
  #273).
  Conversions that would stage roughly the whole remote object also show
  equivalent download-then-convert commands up front (#267).
- **`--bbox` regional extracts** — spatial pushdown skips row groups outside
  the requested region, avoiding unnecessary reads and remote fetches
  (#102, #207).
- **`--filter` / `--where`** — SQL-WHERE-style attribute predicates with
  parquet row-group statistics pushdown, timestamp columns included
  (#315, #321).
- **Property selection** — `--include-property`, `--exclude-property` and
  `--exclude-all-properties` (tippecanoe `-y` / `-x` / `-X`), applied at scan
  time on `overview` and at encode time on `export-pmtiles` (#386, #391).
- **`--feature-order`** — set within-tile draw order to input order or a
  property, ascending or descending (#361, #366).
- **Magnitude ladder** — `--magnitude-ladder` and `--entry-zoom` set each
  feature's first zoom from an attribute (#364, #375).
- **`--verbatim`** — tile the input as given, disabling generalization at
  every level (#345, #360, #367).
- **Zoom-band representation** — `--representation "0-7:point"` draws a band as
  representative points and the rest as polygons in one archive, and
  `--collapse-square` replaces dropped tiny polygons with area-dithered
  placeholder squares (#279, #317, #322).
- **Tiny-polygon accumulator** — preserve the area of dropped polygons at
  coarse levels (#384, #394).
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
- Warn during conversion about features suspected of crossing the antimeridian
  (#188, #199).

### Changed

- **Breaking: the legacy per-tile pipeline is gone.** Removed `pipeline.rs`,
  `Converter`, `TilerConfig` and their library re-exports. All conversion now
  runs overview → export (#177, #189). Removed the old pipeline flags:
  `--streaming-mode`, `--include` / `--exclude` / `--exclude-all` property
  filtering, output-compression selection, and `--deterministic`. Use the
  `overview` / `export-pmtiles` tuning flags, also accepted by `tiles` (#249).
- **Breaking: Python `convert()` is deprecated.** It no longer runs the removed
  legacy pipeline; it chains `overview()` + `export_pmtiles()`.
  Use the two-step API for all options. Its legacy keyword
  arguments `drop_density`, `compression`, `include`, `exclude`,
  `exclude_all`, `deterministic`, `drop_smallest_as_needed`,
  `drop_smallest_threshold` and `progress_callback` were removed; passing one
  raises `TypeError`.
- `--polygon-visibility` retuned **4.0 → 2.0** (#259): the rendered sweep
  showed gates above 2.0 remove too many features at coarse zooms without
  reducing file size. Gates below ~2.0 mostly admit candidates dropped by
  write-time collapse (`corpus/SWEEPS.md`, Decision 6).
- Per-tile size caps at 500K by default, matching tippecanoe (#280).
- The simple-clip fast path is on by default; `--no-simple-clip-fastpath` opts
  out (#239, #256).
- Line thinning retuned 2.0 → 1.0, point thinning defaults to 16.0 under
  clustering, and junction continuation now defaults off, based on the same
  sweep methodology.
- Overview footers declare spec version 0.2.0 (#184, #190).
- The declared MSRV is now 1.95, corrected from 1.75 and verified in CI (#358).

### Fixed

- **PMTiles directories**: the contiguous-offset rule now applies to leaf
  entries, oversized directories split into leaves, and `leaf_dirs_offset` is
  never written as 0 (#356, #377, #378).
- **MVT polygons are cleaned in tile space after quantization**, preventing
  self-intersecting or degenerate rings (#383, #393). Ring winding follows
  the spec.
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

- Remote-input network traffic is bounded to about 1× the object's bytes,
  down from ~3×, regardless of zoom range or pass count. Selected row groups
  are staged up front for coalesced, parallel fetches; column chunks are
  cached to the largest row group's working set (#261); and an on-disk spill
  file serves passes 2 and later (#219, #286, #287). If the spill volume fills,
  conversion continues by re-fetching from the network.
- Export uses a single-read fan-out pass to avoid cross-level prefix and
  per-partition re-reads, with parallel clipping, simplification, MVT encoding
  and gzip (#227, #228, #233, #235).
- Large polygons split top-down recursively instead of being clipped whole
  (#226), and three O(V²) hot spots in clipping and validity checking were
  replaced or capped (#237, #241, #242).
- Memory budgeting uses winner-grid level waves, partition waves sized from
  the densest partition and available cores, and an auto profile that selects
  RAM or spill storage from measured geometry (#293, #294, #303,
  #305, #306, #311).
- Parquet row-group encoding and GeoParquet WKB encoding run in parallel
  (#296, #304).
- `--profile` presets over a single-read pipelined pass-2 engine (#212, #213).

### Internal

For Clippy, rustfmt, dependency, CI and benchmark-harness changes, see the full
465-commit record: `git log v0.6.0..v0.7.0`.

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
