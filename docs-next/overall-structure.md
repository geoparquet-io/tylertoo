# tylertoo docs — overall structure (Phase 1)

Planning artifact for the scaffold-docs overhaul. This is the durable handoff
for later phases. Review and edit before any prose is written.

Staging directory: `docs-next/` (fresh; existing `docs/` is untouched and
treated as reference-only — the intent is that this set replaces it once
approved).

## Audiences

**Primary:** Geospatial data engineers building tile pipelines
People with large GeoParquet datasets (city → planet scale) who need web-map
tiles (PMTiles) or a queryable multi-resolution overview. Many arrive from
tippecanoe and want Arrow-native GeoParquet integration. They work at the CLI
and Python levels and care about throughput, memory bounds, remote reads, and
output quality.

**Secondary:**
- GeoParquet format adopters — interested in `geo:overviews` as a new
  GeoParquet capability (COG-style levels inside a valid, SQL-queryable file).
  Care about the format, its metadata model, and interoperability. Tie-breaker
  when a topic's *why* touches the format itself.
- Rust integrators — embedding `tylertoo-core` into their own services
  (`convert_to_overviews()`, `export_pmtiles()`, `ConvertOptions`,
  `ExportOptions`, the streaming writer). Served mainly by Reference; a Rust
  angle in Diving Deeper is optional.

## Primary use case (Getting Started tutorial)

**Take one country's field/building polygons (a GeoParquet file) and produce a
tuned PMTiles web map**, via the two-step overview → export workflow.

Chosen because it is concrete, matches the flagship Brazil 2025 demo, and
touches the most important components in one narrative: the input contract,
overview construction, spec validation, PMTiles export, and a first taste of
the quality ladder.

## Getting Started: concrete input (command-verified 2026-07-20)

The tutorial uses the **real Brazil 2025 fields** source file. Local copy one
directory up from the repo:

`../demo-artifacts/raw-brazil-2025-fields.parquet`

(Canonical remote source is the FTW `results/` collection on Source Cooperative
— see [[reference-ftw-results-collection]] — for readers without the local
copy.)

Verified facts (`gpio inspect` / `gpio check`):

- **43,903,999 features**, 4.46 GB, 355 row groups, ZSTD, GeoParquet 1.1.0.
- **CRS: OGC:CRS84 (WGS84)** — already correct, *no reprojection needed*.
- Geometry types: MultiPolygon, Point, Polygon. Columns: `geometry`, `time`,
  `label`, `bbox`. Bbox ≈ Brazil.
- **Not gpio-optimized yet** — this makes the "prepare input" step real:
  - Spatial order: ⚠️ poor (overlap ratio 1.00) → `gpio sort hilbert`.
  - Row groups: 12.79 MB avg, below the 64–256 MB target → resize during sort.
- Downstream artifacts already on disk for the "view the result" step:
  `../demo-artifacts/brazil-2025-fields-ov-z14-v2.parquet` (14 GB overview) and
  `../demo-artifacts/brazil-2025-fields-v2.pmtiles` (exported tiles).

Tutorial implication: this is **country-scale** (44M features / 4.5 GB), so
Getting Started must set honest timing/memory expectations and may suggest a
first run at a lower `--max-zoom` or a `--bbox` subset before the full pyramid.
(The README's ~45 s figure is for a 632k-polygon file; Brazil is ~70× larger —
minutes, not seconds.)

## Getting Started outline

One narrative, front to back. Each step lists the components/technologies it
introduces. (Step titles are provisional; finalized at Phase 2 Pass A.)

1. **Install tylertoo** — CLI (cargo/binary) and Python (pip). Which surface
   for which reader.
2. **Prepare a GeoParquet input** — WGS84 requirement; gpio sorting + row-group
   sizing and why they matter for speed and memory. One `gpio` command.
3. **Build an overview file** — `tylertoo overview in.parquet ov.parquet
   --min-zoom 0 --max-zoom 14`. What the command produces; the overview file as
   the interesting artifact.
4. **Check the overview against the spec** — `tylertoo validate`. What
   conformance means; why you'd trust the file.
5. **Export a PMTiles archive** — `tylertoo export-pmtiles ov.parquet
   out.pmtiles`. Layer name, the archive as render target.
6. **View the tiles** — drop into a PMTiles viewer; what good output looks like
   across zoom.
7. **Aside: the one-shot form** — `tylertoo in.parquet out.pmtiles` as a facade
   over the two steps, and when to prefer each.

## Diving Deeper topics

The user picks which of these get written (not all must be). Recommended set
first; optional set after. Titles are provisional (finalized at Phase 3 Pass A).

### Recommended

- **Preparing input for tiling** — the gpio-optimized GeoParquet contract:
  WGS84, Hilbert sorting, row-group sizing. *Why* each matters (streaming
  memory model depends on it) and what degrades without it.
  Tags: DD (mental-model, customization).
- **Working with the overview file** — the `geo:overviews` format: level bands,
  metadata, that the file stays valid/SQL-queryable GeoParquet, and why one
  artifact beats separate tile outputs. Inspect and query it with DuckDB.
  Tags: DD (mental-model). Serves format-adopter secondary audience.
- **Tuning what appears at each zoom** — the quality ladder as one mental model:
  class ranking (Overture auto-detect), visibility gates, density budget /
  dropping, point clustering, line coalescing, world-space simplification.
  Organized by intent, not by flag. Links to Reference for full flag list.
  Tags: DD (customization, mental-model).
- **How tylertoo relates to tippecanoe** — a factual capability comparison, not
  a migration guide. tippecanoe reads GeoJSON; tylertoo reads GeoParquet
  directly and can embed overviews in the file. What each tool does, what only
  tylertoo does, and which quality-ladder concepts are shared. tippecanoe is the
  reference point the audience already knows.
  Tags: DD (mental-model). Neutral/factual framing per `headline-style.md`
  (no superiority claims).
- **Tiling remote and multi-file inputs** — `s3://`/`https://`/`gs://` byte-range
  reads, `--bbox` row-group pushdown (extract a city from a country file),
  `--files-from` multi-partition input, and `--filter`/`--where` attribute
  pushdown. Grouped because they share the "read less" theme.
  Tags: DD (alternative-path, auxiliary use case).
- **Keeping memory bounded** — the streaming model (memory ≈ O(row group)), the
  two-pass structure, `--spill-dir`/`$TMPDIR`, and realistic performance
  expectations. What to do when a file is too big for RAM.
  Tags: DD (mental-model).

### Optional / deferrable

- **Decoding PMTiles back to GeoParquet** — `tylertoo decode`, tippecanoe-decode
  semantics. Auxiliary use case.
- **Calling tylertoo from Python** — the Python API as an alternative entry
  point for pipeline authors (could instead live only in Reference).
- **Embedding the Rust core** — for the Rust-integrator secondary audience;
  convert/export functions and option structs from a library-embedding angle.

## Reference — auto-generated, single source of truth is the code

Reference is **generated from the codebase, never hand-written**, so it cannot
drift from the implementation. Prose comes from each surface's own doc comments.
A CI check regenerates and fails on any diff, keeping docs and code locked
together (the maintainability goal).

This **diverges from the scaffold-docs skill's Phase 4**, which hand-writes
Reference with intent commentary. We auto-generate instead; the intent and
mental-model commentary consolidates into Diving Deeper, where it belongs.

Per-surface generation plan (calibrated against current source coverage):

- **CLI** (`reference/cli.md`) — generated from the clap command tree. 581
  `///` help lines already present, so output is rich today. Tool candidate:
  `clap-markdown` via a small generator bin (or `clap_mangen` for man pages).
  Covers `overview`, `export-pmtiles`, `tiles`, `validate`, `decode`.
- **Rust core** — the canonical rustdoc already builds and publishes to
  docs.rs (`convert.rs`/`export.rs` carry 600–835 doc-comment lines each).
  Reference **links to docs.rs/tylertoo-core** rather than re-rendering rustdoc
  HTML into the mkdocs (markdown) site. Rich today, zero extra maintenance.
- **Python** (`reference/python.md`) — generated from `tylertoo.pyi` via
  `mkdocstrings-python` with `allow_inspection: false` (reads the stub
  statically; confirmed viable for compiled pyo3 modules). **Gap:** the stub
  has **0 docstrings** today, so generation currently yields signatures + types
  only. To reach parity-quality prose, add docstrings to the `#[pyfunction]`s in
  `crates/python/src/lib.rs` and regenerate the stub (pyo3 carries docstrings
  through to `.pyi`, and it improves IDE hovers too). This is **source work**,
  flagged for go-ahead — not assumed.

### On "100% parity across CLI + Python + Rust"

Achievable as *coverage* parity — every public option on every surface is
documented from source — but the three surfaces are not literally 1:1 (e.g. the
CLI exposes `decode`; the Python module does not). The generated reference
documents each surface completely; where a capability exists on one surface but
not another, that is stated, not hidden.

### Generation mode (decision needed)

- **Generate at build + CI no-diff guard** (recommended) — reference is produced
  during the docs build; a CI job regenerates and fails if the committed output
  differs. Matches the automated-quality-gate preference; nothing to hand-update.
- **Generate-and-commit** — run a script that writes `reference/*.md`, commit the
  output. Simpler, but relies on remembering to regenerate.

## Resolved in review (all locked)

- **Staging dir** — `docs-next/` approved.
- **Primary audience / use case** — geospatial data engineers; Getting Started
  follows country polygons → PMTiles via the two-step overview → export flow.
- **tippecanoe** — not a migration guide; reframed as a factual capability
  comparison ("How tylertoo relates to tippecanoe").
- **Reference generation mode** — generate at build + **CI no-diff guard**.
- **Python reference** — **add docstrings to the pyo3 `#[pyfunction]`s** in
  `crates/python/src/lib.rs` and regenerate `tylertoo.pyi`, so the Python
  reference renders real prose from a single source of truth. (Source work,
  scheduled as part of Phase 4.)
- **Rust reference** — **link to docs.rs/tylertoo-core**; do not render rustdoc
  into the mkdocs site.
- **Diving Deeper scope** — keep all six recommended topics.

## Phase 4 work items (Reference), carried forward

1. CLI: generator bin using `clap-markdown` (or `clap_mangen`) → `reference/cli.md`.
2. Python: add docstrings to `crates/python/src/lib.rs` `#[pyfunction]`s;
   regenerate `tylertoo.pyi`; wire `mkdocstrings-python` (`allow_inspection:
   false`) → `reference/python.md`.
3. Rust: reference page links to docs.rs/tylertoo-core.
4. CI: a job that regenerates all of the above and fails on any diff.
