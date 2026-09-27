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

### Tests: targeted first, quick/full tiers before commit

Tier runs use [cargo-nextest](https://nexte.st):

```bash
cargo install cargo-nextest --locked
```

The suite is split into two nextest profiles defined in
`.config/nextest.toml` (#457). Work in this order:

1. **Inner loop (TDD red/green): targeted tests only** — a specific test,
   module, or integration binary, via `cargo test` or nextest's filter
   syntax:

   ```bash
   # A specific test
   cargo test --package tylertoo-core \
     overview::assign::tests::some_test -- --nocapture

   # A module
   cargo test --package tylertoo-core overview::cluster:: -- --nocapture
   cargo nextest run -E 'test(overview::cluster::)'

   # One integration-test binary
   cargo test --package tylertoo-core --test overview_hostile
   cargo test --package tylertoo --test tiles_facade
   ```

2. **Before commit/push: the `quick` tier** — unit tests plus every
   integration-test binary except the known-slow set. It is also what plain
   `cargo nextest run` (no `--profile`) runs. It is ~1400 tests and takes
   minutes including compiling every integration binary, so it is a
   pre-commit gate, not something to run after every edit:

   ```bash
   cargo nextest run --profile quick
   ```

3. **`full`, only when it matters** — `quick` plus the slow end-to-end set:
   tests that drive the CLI or the full pipeline over real-data fixtures
   with real parquet I/O and nested parallelism (20s–7min each). Run it
   before landing a change to pipeline internals, sharding, determinism, or
   anything platform-sensitive (shard-merge parity); otherwise let CI's
   Slow Tests job prove it:

   ```bash
   cargo nextest run --profile full
   ```

Plain `cargo test` ignores `nextest.toml`: it runs everything in whatever
target it's pointed at, slow tests included, with no timeout. Use it for
targeted runs and `cargo nextest run` for the tiers. The slowest end-to-end
suites (e.g. `thread_count_determinism`, `shard_merge_parity`) live in
`crates/*/tests/*.rs` as separate binaries, so `cargo test --lib` stays
reasonably fast — but `--lib` still includes some real-parquet I/O tests
(e.g. in `overview::convert`).

CI mirrors the tiers: the Test matrix (ubuntu/macos × stable/beta) runs
`quick`; the Slow Tests job runs exactly the set `quick` excludes, on
ubuntu and macOS stable. Together they cover `full`.

The weekly mutation-testing job (`.github/workflows/mutation-tests.yml`,
Sundays, or `gh workflow run mutation-tests.yml`) runs `cargo mutants` on
`tylertoo-core` under the `quick` tier too: `.cargo/mutants.toml` sets
`test_tool = "nextest"`, so marking a test slow in `nextest.toml` also
drops it from every mutant's test run. The ~8200 mutants are split over
a `--shard k/n` matrix (`SHARD_COUNT` in the workflow) so each shard
fits GitHub's 6-hour job limit; a `report` job sums the shards'
`outcomes.json` into one score. The job is advisory — missed mutants and
timeouts show up as the mutation score in the job summary, not as a red
run — but a run that could not measure everything (a baseline failure,
which needs the `geometry-test-data` submodule and the realdata
fixtures, or a shard hitting its time limit) opens or updates a pinned
`mutation-tests` issue. To reproduce locally, with the submodule and
fixtures in place:

```bash
cargo install cargo-mutants cargo-nextest --locked
cargo mutants --package tylertoo-core --list   # what would be mutated
cargo mutants --package tylertoo-core \
  -F 'simplify'                                # one module, minutes
cargo mutants --package tylertoo-core          # the full sweep, hours
```

The nightly fuzz job (`.github/workflows/fuzz.yml`, 3 AM UTC, or
`gh workflow run fuzz.yml`) runs every `cargo-fuzz` target in `fuzz/`
for 300 s each, one matrix job per target, seeded from
`fuzz/corpus/<target>/seed-*`. The targets cover the parsers that read
third-party bytes: the PMTiles header and directory, an MVT tile body, a
whole `--band` archive, the footer JSON, WKB, and the `--filter` grammar.
A crash uploads the reproducer as the `fuzz-artifacts-<target>` artifact
and opens or updates a pinned `fuzz` issue. `fuzz/README.md` has the
target table, how to run one locally, and how to minimize a crash into
a regression test. The fuzz crate is its own workspace and never affects
the per-PR build; two of its targets reach private code through
`#[doc(hidden)]` hooks behind core's `fuzzing` feature, which nothing
else enables.

```bash
cargo install cargo-fuzz --locked
cd fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run pmtiles_directory -- \
  -max_total_time=60
```

When a new test takes more than ~20s, add it to the slow set in
`.config/nextest.toml`: the `default-filter` exclusion in
`[profile.default]` (which `quick` inherits) and the matching
`[[profile.default.overrides]]` filter that gives slow tests a longer
timeout. `full` needs no edit — its `default-filter = "all()"` picks the
new test up automatically.

### Benchmarks

Criterion coverage of the measured hot paths lives in `crates/core/benches/`.
The real-data benches read `tests/fixtures/realdata/` (fetch it with the
`gh release download fixtures-v1 ...` step above). Locally, a bench whose
fixture is missing prints a message and skips; with
`TYLERTOO_BENCH_REQUIRE_FIXTURES=1` (set in CI's regression gate) it panics
instead, so a gated run can't silently compare nothing.

| Bench | Covers |
|-------|--------|
| `pass1_decode` | Arrow columnar decode: `from_arrow_array` + `extract_geometries_from_array` (the pass-1 scan's per-chunk decode) |
| `assign` | `assign_levels_bounded` + `apply_density_budget` at a fixed, CI-sized feature count (the thread-*scaling* curve at dataset scale is `assign_scaling.rs`, a separate manual harness — see its own doc comment) |
| `simplify_cascade` | `simplify_cascade` over real polygons through a 10-level fine→coarse chain with a zoom-band `Point` tail (#218, #317) |
| `mvt_encode` | `encode_polygon` (the #383/#461 quantize → noding-sweep → clean → orient path, on rings under the 4096-edge noding cap; `antarctica_316k_over_cap` times the over-cap early-out) and `LayerBuilder::add_feature`/`build` (the #559 alloc-free value-dedup path) |
| `clipping` | Sutherland-Hodgman vs `i_overlay`, plus an `export_clip_mix` group matching the point/line/polygon distribution `overview::export` actually clips per tile |
| `bbox_containment` | Bbox containment fast-path checks |
| `tile_compress_dedup` | Per-tile gzip compression and the XXH3 tile-content dedup cache |

Run one:

```bash
cargo bench --package tylertoo-core --bench clipping
cargo bench --package tylertoo-core --bench mvt_encode
open target/criterion/report/index.html
```

The gated set is defined once, in `scripts/criterion_compare.sh`
(`scripts/criterion_compare.sh --list` prints it). Run all of them (what CI's
`bench` job runs):

```bash
for b in $(scripts/criterion_compare.sh --list); do
  cargo bench --package tylertoo-core --bench "$b"
done
```

`assign_scaling` (`cargo bench --package tylertoo-core --bench assign_scaling`)
is a separate, non-criterion wall-clock harness for the thread-count scaling
curve at dataset scale (#534) — too large and too variance-sensitive for
criterion's repeated-sampling model. It isn't part of the regression gate
below.

The corpus-scale benchmarks (storage/access/conversion) are scripted in
`benchmarks/overview/`; profiling is documented in `docs/PROFILING.md`.

#### Criterion regression gate (#448)

`.github/workflows/bench-regression.yml` runs the benches twice on one
runner, base revision first and head second, and gates the difference. A
baseline saved by an earlier run on another VM would mostly measure the VM;
two runs on the same machine minutes apart are the comparison shared runners
can support. (The #393 lesson the issue is named for: an 18% encode
regression sailed through the old bench.yml, which only uploaded reports.)

It is **not** run on every PR. It runs:

- on a PR carrying the **`benchmark` label** (adding the label starts it;
  later pushes re-run it while the label stays). Base = the merge-base with
  the PR's base branch;
- on `workflow_dispatch`, with an optional `base` input. Without it, a run on
  `main` compares against the latest `v*` release tag, and a run on any other
  branch against its merge-base with `origin/main`.

There is no schedule: main against itself on a timer only measures noise.

`scripts/criterion_compare.sh <base-rev>` does the two runs. It checks the
base out into a temporary git worktree, runs the benches there with
`--save-baseline base`, deletes every `new/` and `change/` directory, then
runs the current checkout with `--baseline-lenient base`. `--baseline-lenient`
matters: plain `--baseline` aborts the whole run on a bench that has no base
result yet. Output goes to `target/criterion-compare/` (`CRITERION_HOME`),
wiped at the start, so your normal `target/criterion/` history isn't touched
and can't leak in.

`scripts/criterion_gate.py` then reads criterion's own `change/estimates.json`
for each bench (the relative change of the mean with its 95% bootstrap
confidence interval):

- **fail** when the CI *lower bound* of the change is >= +25%, meaning the
  regression is at least that large even at the optimistic end of the
  interval, so noise alone is an unlikely cause;
- **warn** when the point estimate is >= +10%;
- a bench with no base result (new in this change) is reported as
  "new (no baseline)" and not gated; a bench with a base result but no head
  result is reported as "not run at head";
- **fail** when nothing at all was comparable: a comparison that compared
  nothing is a broken run, not a pass.

To run the same check locally:

```bash
scripts/criterion_compare.sh origin/main      # or any base revision
uv run python scripts/criterion_gate.py --check \
  --criterion-dir target/criterion-compare --baseline base --warn 10 --fail 25
```

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

# Convert regression guards (#558, #425)
# 1. structural signature of `overview` -> `export-pmtiles`
#    over fixtures-v1: per-level counts, per-zoom tile and
#    feature counts (written + `decode`d back), archive
#    bytes +-2% (e2e.yml Archive e2e; see "Archive e2e" below)
cargo build --release --package tylertoo
python3 benchmarks/overview/ci_guard.py --check
# 2. golden tile digests of a full convert -> export build
#    (nextest slow set -> required Slow Tests checks in ci.yml)
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

### Archive e2e (independent readers, #421 / #425)

`.github/workflows/e2e.yml` builds the release binary, runs the
convert+export guard above over the fixtures-v1 inputs, and then opens
every archive it produced with readers that share no code with our
PMTiles writer: go-pmtiles `pmtiles verify` (structure: header,
directories, clustering, zoom bounds) and a uv script that walks the
archive with the Python `pmtiles` reader and decodes sampled tiles at
every zoom with `mapbox-vector-tile`. Locally:

```bash
cargo build --release --package tylertoo

# 1. guard, keeping the archives for the reader checks
python3 benchmarks/overview/ci_guard.py --check \
  --keep-archives /tmp/e2e-archives

# 2. go-pmtiles at the pinned release, sha256-checked,
#    installed into .tools/ (gitignored)
scripts/setup_go_pmtiles.sh
eval "$(scripts/setup_go_pmtiles.sh --print-path)"
for f in /tmp/e2e-archives/*.pmtiles; do
  "$PMTILES_BIN" verify "$f"
done

# 3. pmtiles + mapbox-vector-tile (PEP 723 inline deps,
#    resolved by uv); the layer is the archive's stem
for f in /tmp/e2e-archives/*.pmtiles; do
  uv run scripts/verify_archive.py "$f" \
    --layer "$(basename "$f" .pmtiles)" --per-zoom 5
done
```

After an intended output change, regenerate the baseline with
`python3 benchmarks/overview/ci_guard.py --update` (release build) and
commit `benchmarks/overview/ci_baseline.json`. Archive bytes are compared
with a 2% tolerance (`--tolerance`); everything else exactly. Only our
own archives go through `pmtiles verify` — the tippecanoe-made
`tests/fixtures/golden/*.pmtiles` are inputs, not outputs.

### tippecanoe parity gate (#420)

`.github/workflows/tippecanoe-compare.yml` (every PR, push to main,
weekly) tiles the fixtures-v1 inputs with tylertoo and with tippecanoe
2.79.0 built from source at a pinned tag + commit, decodes both
archives with independent readers, and gates the per-zoom tile,
feature, distinct-id, vertex and byte ratios against
`benchmarks/e2e/tippecanoe_tolerances.toml`. The table lands in the
job summary; a scheduled failure opens an issue labelled
`tippecanoe-parity`. Method and flag mapping:
`benchmarks/e2e/README.md`, "Parity gate". Locally:

```bash
cargo build --release --package tylertoo

# pinned tippecanoe, built into .tools/ (gitignored);
# needs a C++ toolchain, sqlite3 and zlib headers
benchmarks/e2e/setup_tippecanoe.sh

# the gate: all three fixtures, ~75 s on a 16-core
# laptop; exit 1 on a breach with the cell named
uv run benchmarks/e2e/compare_tippecanoe.py

# one fixture, keep the archives and the JSON record
uv run benchmarks/e2e/compare_tippecanoe.py \
  --only open-buildings --work /tmp/cmp --keep \
  --json /tmp/cmp/parity.json
```

The bands are a ratchet like the convert guard baseline: when a
change moves a ratio on purpose, edit the band in the same PR and put
the new measured value in its comment. The Test and Coverage jobs
build the same pinned tippecanoe so that
`decode_golden_against_tippecanoe_decode` (decode_roundtrip.rs) runs;
on CI a missing `tippecanoe-decode` fails that test instead of
skipping it. To run it locally:

```bash
eval "$(benchmarks/e2e/setup_tippecanoe.sh --print-path)"
cargo test -p tylertoo-core --test decode_roundtrip \
  decode_golden -- --nocapture
```

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
output change and runs in the required `Slow Tests` checks — so such a bump reaches a human as a red check rather
than as a merge. See `context/ARCHITECTURE.md`, "Decision Record:
Geometry-Engine Dependency Bumps (#558)".

## Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Criterion.rs Docs](https://bheisler.github.io/criterion.rs/book/)
- [pyo3 Guide](https://pyo3.rs/)
- [MVT Spec](https://github.com/mapbox/vector-tile-spec)
- [PMTiles Spec](https://github.com/protomaps/PMTiles)
- [GeoParquet Spec](https://geoparquet.org/)
