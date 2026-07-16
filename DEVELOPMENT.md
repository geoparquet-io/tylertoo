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
```

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

CI runs the full matrix (`cargo test --all-features -- --skip
large_polygon_regression` on ubuntu/macos × stable/beta) — let it.

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
# cognitive-complexity threshold in clippy.toml)
cargo clippy --all-targets --all-features -- -D warnings

# Format
cargo fmt --all --check

# Unused dependencies
cargo install cargo-machete   # once
cargo machete

# Supply chain (policy in deny.toml)
cargo install cargo-audit cargo-deny   # once
cargo audit
cargo deny check

# Profiling feature still compiles
cargo build --features dhat-heap

# Coverage (informational; CI uploads to codecov)
cargo install cargo-tarpaulin   # once (CI uses a prebuilt binary)
cargo tarpaulin --out xml --all-features --workspace \
  --exclude tylertoo-python
```

Some thresholds are **ratchets** set at current-code level and marked
`RATCHET` in-source (`clippy.toml` cognitive-complexity 30, xenon
max-absolute C). Lower them as code improves; never raise them.

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
version), `crates/python/pyproject.toml`, and `.cz.toml` must agree.
The pre-commit hook and a CI job both enforce it; `uv run cz bump`
from the repo root is the only supported way to move versions (see
CONTRIBUTING.md).

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

Dependabot (weekly) covers cargo, pip (uv lockfile), and GitHub
Actions; patch/minor updates auto-merge once all gates pass, majors
wait for a human. A weekly security job (cargo-audit + cargo-deny +
pip-audit) opens/updates a pinned `security-audit` issue on failure.

## Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Criterion.rs Docs](https://bheisler.github.io/criterion.rs/book/)
- [pyo3 Guide](https://pyo3.rs/)
- [MVT Spec](https://github.com/mapbox/vector-tile-spec)
- [PMTiles Spec](https://github.com/protomaps/PMTiles)
- [GeoParquet Spec](https://geoparquet.org/)
