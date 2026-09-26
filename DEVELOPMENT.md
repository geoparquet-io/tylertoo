# Development Guide

Quick reference for working on tylertoo.

## Initial Setup

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install protoc (choose your platform)
# macOS
brew install protobuf

# Ubuntu/Debian
sudo apt-get install protobuf-compiler

# Verify installation
protoc --version  # Should be 3.x or higher

# Enable the repo git hooks (fmt, clippy, version sync, README sync)
git config core.hooksPath .githooks

# Fetch the geometry-test-data submodule (the geometry fixture tests
# read from it; without it some skip silently and others panic —
# see below)
git submodule update --init

# Fetch the real-data test fixtures (a fresh clone holds git-lfs pointers,
# not the files; the integration tests that need them skip without these)
gh release download fixtures-v1 --dir tests/fixtures/realdata/ --clobber
```

Tests backed by those fixtures skip **locally**, with a message naming
the command above, when the fixtures are absent. **On CI they fail**:
CI downloads the fixtures as a build step, so an unusable one means that
step or its cache is broken rather than that you are working without
them — and a guard that stays silent there is how #369 went unnoticed
for six months. See `tests/fixtures/realdata/README.md` for what each
fixture is.

The `geometry-test-data` submodule has a related hazard. If it is not
initialized, the tests in `ioverlay_real_fixtures_test.rs` print
"Fixture not found, skipping test" and pass, while
`invalid_geometry_clipping.rs` fails with a file-read panic. CI checks
the submodule out via `submodules: recursive`, so this bites local
clones only. Run `git submodule update --init` once after cloning and
again whenever the submodule pointer moves upstream.

The pre-commit hook runs `cargo fmt --check`, `cargo clippy` (deny
warnings), a version-consistency check across the four version files,
and syncs the root `README.md` into `crates/*/README.md`. Never bypass
it with `--no-verify`.

## Day-to-Day Workflow

```bash
cargo check                    # Fast compile check — use liberally
cargo build                    # Debug build
cargo build --release          # Release build
cargo fmt --all                # Format (required before commit)
```

### Tests: targeted only

The full suite is slow (real parquet I/O, full pipeline runs, nested
parallelism). Run targeted tests:

```bash
# A specific test
cargo test --package tylertoo-core \
  overview::assign::tests::some_test -- --nocapture

# A module
cargo test --package tylertoo-core overview::cluster:: -- --nocapture

# The CLI facade integration test
cargo test --package tylertoo --test tiles_facade
```

CI runs the tests with [cargo-nextest](https://nexte.st), split in two.
The Test matrix (ubuntu/macos × stable/beta) runs everything except
`large_polygon_regression` and the slow end-to-end set listed in
`.config/nextest.toml`. The Slow Tests job runs that set on ubuntu and
macOS, stable only. Let CI run both. To run the slow set locally:

```bash
cargo nextest run --all-features \
  --ignore-default-filter -E 'not default()'
```

When a new test takes more than ~20s in CI, add it to the default filter
in `.config/nextest.toml`.

### Benchmarks

```bash
cargo bench --package tylertoo-core --bench clipping
cargo bench --package tylertoo-core --bench bbox_containment
open target/criterion/report/index.html
```

The corpus-scale benchmarks (storage/access/conversion) are scripted in
`benchmarks/overview/`; profiling is documented in `docs/PROFILING.md`.

## CI Gates — and How to Run Them Locally

Every PR must pass all gates (they are branch-protection required
checks). All of them are runnable locally.

### Rust

```bash
# Lint (curated pedantic subset via [workspace.lints.clippy];
# cognitive-complexity and too-many-lines thresholds in
# clippy.toml)
cargo clippy --all-targets --all-features -- -D warnings

# Format
cargo fmt --all --check

# Unused dependencies
cargo install cargo-shear   # once
cargo shear

# Duplicated code (hard gate: zero pairs at >=0.95)
cargo install similarity-rs   # once
similarity-rs --threshold 0.95 --min-lines 10 \
  --skip-test crates/core/src crates/cli/src \
  crates/python/src

# Public API surface (baseline lives in
# crates/core/api/tylertoo-core.txt). The nightly
# is pinned: rustdoc renders paths differently from
# one nightly to the next, so a floating toolchain
# reports drift that is not there. The pin lives in
# the public-api job in .github/workflows/ci.yml.
cargo install cargo-public-api        # once
rustup toolchain install \
  nightly-2026-09-15                  # once
cd crates/core
cargo +nightly-2026-09-15 public-api \
  --simplified | diff -u api/tylertoo-core.txt -
cd ../..

# Supply chain (policy in deny.toml)
cargo install cargo-audit cargo-deny   # once
cargo audit
cargo deny check

# Breaking changes vs. main (tylertoo-core is
# unpublished, so the baseline is a git revision)
cargo install cargo-semver-checks   # once
cargo semver-checks check-release \
  --package tylertoo-core \
  --baseline-rev origin/main

# Convert regression guard — two checks, both gating.
# 1. structural signature of `overview` over fixtures-v1
cargo build --release --package tylertoo
python3 benchmarks/overview/ci_guard.py --check
# 2. golden tile digests of a full convert -> export build (#558)
cargo test --release -p tylertoo-core \
  --test convert_guard_golden

# Profiling feature still compiles
cargo build --features dhat-heap

# Coverage (informational; CI uploads to codecov)
cargo install cargo-llvm-cov    # once (CI uses a prebuilt binary)
rustup component add llvm-tools-preview
cargo llvm-cov --all-features --workspace \
  --exclude tylertoo-python \
  --lcov --output-path lcov.info
```

Some thresholds are **ratchets** set at current-code level and marked
`RATCHET` in-source (`clippy.toml` cognitive-complexity 30 and
too-many-lines 200, the similarity-rs threshold 0.95, xenon
max-absolute C). Lower them as code improves; never raise them.

`cargo shear` replaced `cargo machete`: shear also inspects
`[workspace.dependencies]`, where machete reads only member crates.
A dependency that is deliberately declared without a code reference
(a transitive version pin, for instance) is listed under
`[package.metadata.cargo-shear]` in the crate that owns it.

similarity-rs has no baseline file and no per-pair suppression. A new
finding at or above 0.95 has to be refactored away. When the API
gate fires on an intended change, regenerate the baseline in the
same PR — see `crates/core/api/README.md`.

The golden tile guard is the same kind of gate: when a change moves
tile output **on purpose**, regenerate its golden in the same PR and
say in the PR body why the output moved.

```bash
TYLERTOO_UPDATE_GOLDEN=1 cargo test -p tylertoo-core \
  --test convert_guard_golden
```

It rewrites `tests/fixtures/guard/br-clip-divergence.golden.txt` and
then fails, so regenerating can never be mistaken for passing.
A golden diff on a **dependency bump** is not a regeneration cue — see
`context/ARCHITECTURE.md`, "Geometry-Engine Dependency Bumps (#558)".

### Python (`crates/python`)

Everything runs through **uv** (never bare `python`/`pip`):

```bash
cd crates/python
uv sync --group dev
uv run maturin develop          # build the extension module

uv run ruff check .             # strict 16-group ruleset
uv run ruff format --check .
uv run mypy                     # strict typing
uv run python -m mypy.stubtest tylertoo \
  --allowlist stubtest-allowlist.txt   # tylertoo.pyi matches the built module
uv run vulture                  # dead code
uv run xenon --max-absolute C --max-modules A --max-average A tests
uv run pytest tests/ -v

# Supply chain
uv export --no-emit-project --format requirements-txt \
  -o /tmp/requirements.txt
uv run pip-audit -r /tmp/requirements.txt --disable-pip
```

If you change a `#[pyo3(signature = ...)]` in
`crates/python/src/lib.rs`, update `crates/python/tylertoo.pyi` —
stubtest will fail otherwise.

### Workflows

```bash
# CI config lint (all action refs must stay SHA-pinned)
uvx zizmor --min-severity low .github/workflows
```

### Version consistency

`Cargo.toml` (workspace version + the `tylertoo-core` dependency
version), `crates/python/pyproject.toml`, `.cz.toml`, and the
`tylertoo` entry in `crates/python/uv.lock` must agree. The pre-commit
hook and a CI job both enforce it. `uv run cz bump` from the repo root
is the only supported way to move versions (see CONTRIBUTING.md).

`cz bump` does not touch `uv.lock`. The pre-commit hook regenerates it
and stages the result. If you commit with hooks disabled, run
`cd crates/python && uv lock` yourself, or the Python Quality job fails
on `uv sync --locked`.

## Module Layout

```
crates/
├── core/     # ALL logic: overview/ (the product) + shared infrastructure
├── cli/      # Thin argument parsing → core (tiles facade, overview,
│             # validate, export-pmtiles)
└── python/   # pyo3 bindings → core (+ tylertoo.pyi stubs)
```

The full module map and design rationale live in
`context/ARCHITECTURE.md`.

## Python Development

```bash
cd crates/python

uv sync --group dev             # create venv + install dev deps
uv run maturin develop          # build + install the extension in-place
uv run python -c "import tylertoo; print(tylertoo.__doc__)"
uv run pytest tests/ -v

# Build a release wheel (lands in target/wheels/)
uv run maturin build --release
```

## Debugging

```bash
# Pipeline phase timing / diagnostics
RUST_LOG=tylertoo_core::overview=debug \
  cargo run --package tylertoo -- overview in.parquet out.parquet

# Backtrace on a failing test
RUST_BACKTRACE=1 cargo test --package tylertoo-core <test-name>
```

### Common Issues

**Problem**: `protoc` not found during build
**Solution**: Install protobuf compiler (see Initial Setup)

**Problem**: Linker errors on macOS
**Solution**: `xcode-select --install`

**Problem**: Tests fail with file not found
**Solution**: Tests run from the workspace root; use relative paths like
`tests/fixtures/...`

**Problem**: stubtest fails after a binding change
**Solution**: Update `crates/python/tylertoo.pyi` to match the new
`#[pyo3(signature)]`

## Dependency Updates

`Cargo.lock` is committed, so every build — local, CI, and the release
artifacts — resolves to the same versions. Update it deliberately
(`cargo update -p <crate>`, or let the manifest edit rewrite it) and
commit the result alongside the change. CI's audit job fails if the lock
is stale relative to the manifests; the weekly security job deliberately
re-resolves from scratch as an early warning for what the next bump would
pull in.

Dependabot (weekly) covers cargo, pip (uv lockfile), and GitHub
Actions; patch/minor updates auto-merge once all gates pass, majors
wait for a human. A weekly security job (cargo-audit + cargo-deny +
pip-audit) opens/updates a pinned `security-audit` issue on failure.

**Geometry-engine bumps are the exception** (#558): `geo`, `geo-types`,
`i_overlay`, `i_float`, `i_shape` and `earcut` can change rendered tile geometry
at any semver level, so green CI alone is not consent to merge one.
The golden tile guard enforces this mechanically — it fails on any
output change — so such a bump reaches a human as a red check rather
than as a merge. See `context/ARCHITECTURE.md`, "Decision Record:
Geometry-Engine Dependency Bumps (#558)".

## Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Criterion.rs Docs](https://bheisler.github.io/criterion.rs/book/)
- [pyo3 Guide](https://pyo3.rs/)
- [MVT Spec](https://github.com/mapbox/vector-tile-spec)
- [PMTiles Spec](https://github.com/protomaps/PMTiles)
- [GeoParquet Spec](https://geoparquet.org/)
