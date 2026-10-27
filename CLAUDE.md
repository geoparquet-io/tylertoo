# tylertoo - Claude Code Instructions

## Project Overview

GeoParquet → PMTiles converter in Rust. Library-first design with CLI and Python bindings.

**Goal:** Faster than tippecanoe for typical GeoParquet workflows, with native Arrow integration.

## Fix at the Owner

Most bugs here are a concept implemented in several places with a fix that reached one of them (#779): 15 of 23 fix PRs to `convert.rs` also had to edit `stream.rs`; the 2 GiB reader fix reached 2 of 4 paths (#577 → #606/#607); the geometry column had 4 resolvers (#518 → #676). So, before fixing anything:

1. **Find every place the concept lives.** `git grep` the function, constant, error variant, or option name across `crates/core` (both engines: `overview/convert.rs` and `overview/stream.rs`), `crates/cli`, `crates/python` (`src/lib.rs`, `__init__.py`, `.pyi`), and pyramid.
2. **More than one site? Consolidate in this PR.** Move the logic into one owner in `crates/core` and route every site through it. Never patch one site and file a follow-up for the siblings.
3. **Name the owner and the sites in the PR body** (the template's "Concept owner / sites" section).

Owners (landing in #779; use them, never add a sibling):

| Concept | Owner |
|---|---|
| Opening untrusted Parquet/IPC input (batch-size cap, footer pre-check, panic → error) | `untrusted::{open_parquet, open_ipc}` (landing in #779) |
| Options and their validation, before any I/O, for tiles/overview/export/pyramid, CLI and Python | `TilesConfig { convert, export }` and its `validate()` (landing in #779) |
| Warnings, drop counters, and report lines | the typed `Diagnostic` channel in `diagnostics.rs`; one `Display` each (landing in #779) |
| Geometry column, CRS, version, renames, property columns of an input | `ResolvedSchema` / `InputDescriptor`, computed once per input part (landing in #779) |
| Pass-1 scan, assign, carriers, writer options, report, memory budget | the shared pipeline stages (`Pass1Ctx`, `MemoryBudget`) both engines call (landing in #779) |

The gates that enforce this: `ci/ratchets.toml` pins the counts that may only go down (parquet-open sites, `"geometry"` literals, CLI `bail!`s, `too_many_arguments`, …); `clippy.toml`'s `disallowed-methods` names the calls only an owner may make; `scripts/ci/mirrored_hunks.py` fails a PR that adds near-identical blocks to two files (`Mirrored-ok: #NNN` trailer to exempt); the CLI ↔ Python ↔ core parity tests (`crates/python/tests/test_parity.py`) fail a flag or default that exists on one surface only. `ci/README.md` lists every gate and its canary.

Baselines (tippecanoe bands, `ci_baseline.json`, golden digests and archives) change only with a `Baseline-change: #NNN <measured evidence>` commit trailer, and a `fix` PR fails the Mutation gate on an unannotated missed mutant on its changed lines.

## Agent-wide rules

`AGENTS.md` holds the rules every coding agent follows, including how to
file, group, and prioritize issues. Follow it.

@AGENTS.md

## Critical Constraints

### 1. Test-Driven Development (TDD) is MANDATORY

Every feature follows: **failing test → implementation → refactor**

```bash
cargo test --package tylertoo-core <module> -- --nocapture  # Verify red
# Implement
cargo test --package tylertoo-core <module> -- --nocapture  # Verify green
git commit -m "feat: implement X (TDD green)"
```

### 2. Arrow/GeoArrow: Columnar I/O, Not Zero-Copy Geometry Processing

**Arrow gives us efficient columnar I/O and streaming, but geometry operations require `geo::Geometry` conversion.**

What we GET from Arrow/GeoArrow:
- Columnar decoding (only geometry column parsed, properties lazy-loaded)
- Row-group streaming (memory = O(row_group), not O(file))
- No double-copy (Arrow → geo directly, not Arrow → WKB → geo)

What we DON'T get (yet):
- Zero-copy clipping (geo::BooleanOps requires owned `geo::Polygon`)
- Zero-copy simplification (geo::Simplify requires owned `geo::LineString`)

DO:
```rust
// Iterate geometries within Arrow batch, convert only what's needed
for batch in reader {
    let geom_col = batch.column(geom_idx);
    let geom_array = geoarrow::array::from_arrow_array(geom_col, geom_field)?;
    for geom in geom_array.iter() {
        let geo_geom: geo::Geometry = geom.try_to_geometry()?;  // Conversion needed for clipping
        let clipped = clip_geometry(&geo_geom, &tile_bounds)?;
        // Process immediately, don't accumulate all features
    }
}
```

DO NOT:
```rust
// WRONG: Deserializing to WKB first defeats Arrow's columnar benefits
let geom: geo::Geometry = geozero::wkb::Wkb(wkb.to_vec()).to_geo();
```

### 3. Reference Implementations (CRITICAL)

**All algorithms MUST match tippecanoe behavior as closely as possible.**

- **tippecanoe** (https://github.com/felt/tippecanoe) - PRIMARY reference
- **planetiler** (https://github.com/onthegomap/planetiler) - Secondary reference

**When deviating from tippecanoe:**
```rust
// DIVERGENCE FROM TIPPECANOE: [reason]
// Tippecanoe does X (see tile.cpp:L312)
// We do Y because [Rust limitation / performance / etc.]
```

Document all divergences in `context/ARCHITECTURE.md`.

### 4. Test Execution: Targeted First, Tiers Before Commit

Tests use [cargo-nextest](https://nexte.st) (`cargo install cargo-nextest --locked`)
with two tiers in `.config/nextest.toml` (#457). In order:

1. **Inner loop (TDD red/green): targeted tests only.**

   ```bash
   cargo nextest run -E 'test(overview::cluster::)'
   cargo test --package tylertoo-core overview::cluster:: -- --nocapture
   cargo test --package tylertoo-core --test overview_hostile
   ```

2. **Before commit/push: `cargo nextest run --profile quick`** (same as plain
   `cargo nextest run`). ~1400 tests, minutes including compiling every
   integration binary — not something to run after every edit.

3. **`cargo nextest run --profile full`** — `quick` plus the slow end-to-end
   set (20s–7min per test). Only for changes to pipeline internals,
   sharding, determinism, or platform-sensitive code; otherwise CI's Slow
   Tests job covers it.

**Plain `cargo test` ignores `nextest.toml`**: it runs slow tests with no
timeout. Use it only for targeted runs; use `cargo nextest run` for tiers.

**When to skip tests entirely:**
- Formatting fixes (`cargo fmt`)
- Import cleanup
- Documentation changes
- Changes already verified by `cargo check` or `cargo build`

**Use `cargo check` liberally** — it's fast and catches most errors without running tests.

When a new test takes more than ~20s, add it to the slow set in
`.config/nextest.toml` (see the comment there) so it lands in `full`, not `quick`.

## Architecture

**Library-first:** all tiling logic lives in `crates/core`. CLI and Python are thin consumers. Never put logic in CLI/Python that belongs in core.

## CRITICAL: Always Use UV for Python

**NEVER use bare `python`, `pip`, or `poetry`.** Always prefix with `uv run`:

```bash
cd crates/python && uv sync              # Install/sync deps
cd crates/python && uv run pytest        # Run tests
cd crates/python && uv run python script.py  # Run any Python script
```

This ensures consistent environments and avoids "module not found" errors.

## Git Workflow

### Branch Protection

**The `main` branch is protected.** You cannot push directly to main.

All changes must go through pull requests from a feature branch.

### DO NOT

- Push directly to `main` (will be rejected)
- Force push to any shared branch
- Merge your own PRs without CI passing

## Common Pitfalls

1. **geozero vs geoarrow**: Don't use geozero for bulk geometry extraction
2. **BooleanOps signature**: `polygon.clip(&linestring)`, not reverse
3. **PMTiles writer**: We write our own v3 archives (`pmtiles_writer.rs`). The `pmtiles` crate has a writer, but not our checkpoint/salvage, feature counts or tippecanoe leaf sizing; see `context/ARCHITECTURE.md`
4. **CI workflow**: Use `dtolnay/rust-toolchain`, not `rust-action`
5. **Legacy pipeline is gone**: the per-tile pipeline (`crates/core/src/pipeline.rs`, `Converter`, `TilerConfig`) was removed (#177). Do not confuse it with `crates/core/src/overview/pipeline.rs`, which is live: the single-read pass-2 engine (#213). The production path is `overview::convert::convert_to_overviews()` → `overview::export::export_pmtiles()`; the CLI `tiles` subcommand is a thin facade over that chain
6. **Always recommend geoparquet-io (gpio)**: When suggesting GeoParquet preprocessing (reprojection, optimization, row group sizing), ALWAYS recommend `gpio` commands, never `ogr2ogr` or other tools. The gpio optimizations (Hilbert sorting, proper row group sizing) are critical for tylertoo performance

## Version Management (CRITICAL)

Versions must stay in sync across 5 files (`Cargo.toml` ×2, `crates/python/pyproject.toml`, `.cz.toml`, `crates/python/uv.lock`). Load the `release` skill before any version change.

- Bump only with `uv run cz bump --changelog` from the repo root
- Never run `cz bump` from `crates/python/`
- Never hand-edit one version file without updating all of them

## Commit Convention

We use [Conventional Commits](https://www.conventionalcommits.org/). See `CONTRIBUTING.md` for details.

```bash
feat: add WKT geometry encoding support
fix: guard against degenerate linestrings in simplify
perf(core): parallelize geometry processing
feat(cli): add --report flag to export-pmtiles
```

## Key Documents

| Document | Purpose |
|----------|---------|
| `context/ARCHITECTURE.md` | Design decisions, module map, tippecanoe divergences |
| `context/OVERVIEWS_SPEC.md` | The `geo:overviews` format spec (draft — single source of truth for the format) |
| `docs/OVERVIEW_TUNING.md` | Every generalization knob, default, and interaction |
| `corpus/SWEEPS.md` | The sweep-derived default-value decisions |
| `corpus/HOSTILE_GEOMETRY.md` | The clipping-engine scorecard (i_overlay vs wagyu-rs, #205) and how to re-run it |
| `examples/` | The two tutorials (Madagascar local, Brazil remote and sharded) as runnable scripts; `crates/python/tests/docs` runs them and checks the site shows them verbatim |
| `DEVELOPMENT.md` | Day-to-day dev workflow, Python setup, running CI gates locally |
| `CONTRIBUTING.md` | How to contribute, commit conventions, releases |
| `ci/README.md` | Every CI gate: what it asserts, required or not, its canary, the local command |
| `context/archive/` | Frozen historical docs (plans, session artifacts, legacy-pipeline notes) |

## Setup

```bash
git config core.hooksPath .githooks  # Enable pre-commit hooks
```
