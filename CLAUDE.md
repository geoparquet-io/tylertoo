# gpq-tiles - Claude Code Instructions

## Project Overview

GeoParquet → PMTiles converter in Rust. Library-first design with CLI and Python bindings.

**Goal:** Faster than tippecanoe for typical GeoParquet workflows, with native Arrow integration.

## Critical Constraints

### 1. Test-Driven Development (TDD) is MANDATORY

Every feature follows: **failing test → implementation → refactor**

```bash
cargo test --package gpq-tiles-core <module> -- --nocapture  # Verify red
# Implement
cargo test --package gpq-tiles-core <module> -- --nocapture  # Verify green
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

## Architecture

```
crates/
├── core/     # ALL tiling logic lives here
├── cli/      # Thin wrapper: args → core::convert()
└── python/   # pyo3 bindings → core
```

**Library-first:** CLI and Python are thin consumers. Never put logic in CLI/Python that belongs in core.

## Key Types

```rust
TileCoord { x: u32, y: u32, z: u8 }           // Tile coordinates (Web Mercator)
TileBounds { lng_min, lat_min, lng_max, lat_max }  // Geographic bounds
TilerConfig { min_zoom, max_zoom, ... }       // Pipeline configuration
```

## Commands

```bash
cargo build                   # Build
cargo test                    # Run all tests
cargo bench                   # Run all benchmarks
cargo bench -- "single_tile"  # Run specific benchmark group
cargo fmt --all               # Format (required before commit)

# Run the CLI
cargo run --package gpq-tiles -- input.parquet output.pmtiles
```

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

All changes must go through pull requests:
```bash
# Create a feature branch
git checkout -b fix/my-fix
git push -u origin fix/my-fix

# Create PR
gh pr create --title "fix: description" --body "..."
```

### DO NOT

- Push directly to `main` (will be rejected)
- Force push to any shared branch
- Merge your own PRs without CI passing

## Common Pitfalls

1. **geozero vs geoarrow**: Don't use geozero for bulk geometry extraction
2. **BooleanOps signature**: `polygon.clip(&linestring)`, not reverse
3. **PMTiles crate**: Read-only — we implement our own v3 writer
4. **CI workflow**: Use `dtolnay/rust-toolchain`, not `rust-action`
5. **rstar**: Listed in deps but we use space-filling curves for spatial indexing instead
6. **Python tooling**: Always use `uv` for Python work (not pip/poetry). See `DEVELOPMENT.md` for setup
7. **Streaming vs non-streaming**: Use `generate_tiles_to_writer()` for the production pipeline. `TileIterator` is deprecated
8. **Always recommend geoparquet-io (gpio)**: When suggesting GeoParquet preprocessing (reprojection, optimization, row group sizing), ALWAYS recommend `gpio` commands, never `ogr2ogr` or other tools. The gpio optimizations (Hilbert sorting, proper row group sizing) are critical for gpq-tiles performance

## Version Management (CRITICAL)

**All versions MUST stay synchronized across these files:**

| File | Field |
|------|-------|
| `Cargo.toml` | `[workspace.package] version` |
| `Cargo.toml` | `gpq-tiles-core = { ..., version = "X.Y.Z" }` in `[workspace.dependencies]` |
| `crates/python/pyproject.toml` | `[project] version` |
| `.cz.toml` | `version` |

### How to bump versions correctly

```bash
# ALWAYS bump from repo root, NEVER from crates/python
uv run cz bump --changelog

# This updates ALL version files listed in .cz.toml
git push --tags  # Triggers release workflow
```

### Safeguards in place

1. **Pre-commit hook** (`.githooks/pre-commit`): Validates version consistency before commit
2. **CI version-check job**: Fails PRs with mismatched versions
3. **Commitizen config** (`.cz.toml`): Single source of truth for version_files

### DO NOT

- Run `cz bump` from `crates/python/` (it has no commitizen config)
- Manually edit versions without updating ALL files
- Merge PRs that fail the version-check CI job

## Commit Convention

We use [Conventional Commits](https://www.conventionalcommits.org/). See `CONTRIBUTING.md` for details.

```bash
feat: add WKT geometry encoding support
fix: guard against degenerate linestrings in simplify
perf(core): parallelize geometry processing
feat(cli): add --streaming-mode flag
```

## Key Documents

| Document | Purpose |
|----------|---------|
| `context/ARCHITECTURE.md` | Design decisions, tippecanoe divergences |
| `DEVELOPMENT.md` | Day-to-day dev workflow, Python setup |
| `CONTRIBUTING.md` | How to contribute, commit conventions, releases |

## Setup

```bash
git config core.hooksPath .githooks  # Enable pre-commit hooks
```
