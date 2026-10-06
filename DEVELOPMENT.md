# Development guide

How to build tylertoo, run its tests, and run every CI gate on your
machine. For the contribution process, see
[CONTRIBUTING.md](CONTRIBUTING.md).

## Initial setup

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf \
  https://sh.rustup.rs | sh

# protoc 3.x or later
brew install protobuf                    # macOS
sudo apt-get install protobuf-compiler   # Ubuntu/Debian
protoc --version

# The repo's git hooks
git config core.hooksPath .githooks

# The geometry-test-data submodule
git submodule update --init

# The real-data test fixtures (a fresh clone holds
# git-lfs pointers, not the files)
gh release download fixtures-v1 \
  --dir tests/fixtures/realdata/ --clobber
```

When the real-data fixtures are absent, the tests that need them skip
**locally** with a message that names the download command. **On CI they
fail**: CI downloads the fixtures as a build step, so a missing fixture
there means that step or its cache broke. See
`tests/fixtures/realdata/README.md` for what each fixture holds.

The submodule has no such guard. Without it, the tests in
`ioverlay_real_fixtures_test.rs` print "Fixture not found, skipping test"
and pass, and `invalid_geometry_clipping.rs` panics on a file read. CI
checks the submodule out, so only local clones hit this. Run
`git submodule update --init` after cloning and again whenever the
submodule pointer moves.

The pre-commit hook runs:

- `cargo fmt --check` and `cargo clippy` with warnings denied
- the prose lint on staged docs (see [Docs prose](#docs-prose))
- a version-consistency check
- a refresh of `Cargo.lock` and `crates/python/uv.lock` when a version
  bump left them stale
- a copy of the root `README.md` and `NOTICE` into each `crates/*/`

Never bypass it with `--no-verify`.

## Day-to-day workflow

```bash
cargo check             # fast compile check
cargo build             # debug build
cargo build --release   # release build
cargo fmt --all         # format before you commit
```

### Tests

Tier runs use [cargo-nextest](https://nexte.st):

```bash
cargo install cargo-nextest --locked
```

`.config/nextest.toml` splits the suite into two profiles. Work in
this order:

1. **Inner loop: targeted tests only.** Run one test, one module, or one
   integration binary with `cargo test` or a nextest filter:

   ```bash
   # One test
   cargo test --package tylertoo-core \
     overview::assign::tests::some_test -- --nocapture

   # One module
   cargo test --package tylertoo-core \
     overview::cluster:: -- --nocapture
   cargo nextest run -E 'test(overview::cluster::)'

   # One integration-test binary
   cargo test --package tylertoo-core \
     --test overview_hostile
   cargo test --package tylertoo --test tiles_facade
   ```

2. **Before you commit or push: the `quick` tier.** It holds the unit
   tests and every integration binary except the known-slow set. Plain
   `cargo nextest run` runs the same set. Its ~1400 tests take minutes,
   most of it compiling the integration binaries, so run it before a
   commit, not after every edit:

   ```bash
   cargo nextest run --profile quick
   ```

3. **`full`, when the change warrants it.** It adds the slow end-to-end
   set to `quick`: tests that drive the CLI or the whole pipeline over
   real-data fixtures, at 20 seconds to 7 minutes each. Run it before you
   land a change to pipeline internals, sharding, determinism, or other
   platform-sensitive code. Otherwise CI's Slow Tests job covers it:

   ```bash
   cargo nextest run --profile full
   ```

Plain `cargo test` ignores `nextest.toml`. It runs every test in the
target you name, slow ones included, with no timeout. Use it for
targeted runs, and use `cargo nextest run` for the tiers.

CI mirrors the tiers. The Test job runs `quick` with stable and beta
Rust, on both Ubuntu and macOS. The Slow Tests job runs the set that
`quick` excludes, with stable Rust on both systems, in three CPU shards
per OS. All six
`Slow Tests (<os>, shard N/3)` checks must pass. To reproduce one shard:

```bash
cargo nextest run --ignore-default-filter \
  -E 'not default()' --partition count:2/3
```

When a new test takes more than ~20 seconds, add it to the slow set in
`.config/nextest.toml` in two places. One is the `default-filter`
exclusion in `[profile.default]`, which `quick` inherits. The other is
the matching `[[profile.default.overrides]]` filter, which gives slow
tests a longer timeout. `full` picks the test up with no edit.

### Benchmarks

`crates/core/benches/` holds Criterion benchmarks of the hot paths. The
real-data benches read `tests/fixtures/realdata/`. A bench whose fixture
is missing prints a message and skips. With
`TYLERTOO_BENCH_REQUIRE_FIXTURES=1`, which the regression gate sets, it
panics instead, so a gated run can't compare nothing.

| Bench | Covers |
|-------|--------|
| `pass1_decode` | Arrow columnar decode: `from_arrow_array` and `extract_geometries_from_array` |
| `assign` | `assign_levels_bounded` and `apply_density_budget` at a fixed, CI-sized feature count |
| `simplify_cascade` | `simplify_cascade` over real polygons through a 10-level chain with a zoom-band `Point` tail |
| `mvt_encode` | `encode_polygon`, its over-cap early exit (`antarctica_316k_over_cap`), and `LayerBuilder::add_feature`/`build` |
| `clipping` | Sutherland-Hodgman against `i_overlay`, plus `export_clip_mix`, the point, line, and polygon mix that export clips per tile |
| `bbox_containment` | Bounding-box containment fast paths |
| `tile_compress_dedup` | Per-tile gzip and the XXH3 tile-content dedup cache |

Run one:

```bash
cargo bench --package tylertoo-core --bench clipping
open target/criterion/report/index.html
```

`scripts/criterion_compare.sh --list` prints the gated set. Run the
whole set, as the Benchmark workflow does on each push to `main`:

```bash
for b in $(scripts/criterion_compare.sh --list); do
  cargo bench --package tylertoo-core --bench "$b"
done
```

`assign_scaling` measures thread-count scaling at dataset scale. It is a
plain wall-clock harness, outside Criterion and outside the regression
gate, because the run is too large and too noisy for repeated sampling:

```bash
cargo bench --package tylertoo-core \
  --bench assign_scaling
```

`benchmarks/overview/` scripts the corpus-scale storage, access, and
conversion benchmarks. `context/PROFILING.md` covers profiling.

#### Criterion regression gate

`.github/workflows/bench-regression.yml` runs the benches twice on one
runner, base revision first and head second, and gates the difference.
A baseline from another VM would mostly measure the VM. Two runs on one
machine, minutes apart, give a comparison that shared runners support.

The gate runs in two cases, never on a schedule:

- On a PR with the **`benchmark` label**. Adding the label starts it, and
  later pushes rerun it while the label stays. The base is the
  merge-base with the PR's base branch.
- On `workflow_dispatch`, with an optional `base` input. Without one, a
  run on `main` compares against the newest `v*` release tag, and a run
  on another branch against its merge-base with `origin/main`.

`scripts/criterion_compare.sh <base-rev>` does the two runs, the base
in a temporary git worktree. It writes to `target/criterion-compare/`,
so your `target/criterion/` history stays out of it.
`scripts/criterion_gate.py` then reads the relative change of each
bench's mean, with its 95% confidence interval, and applies these rules:

- **Fail** when the interval's lower bound is +25% or more. The
  regression is at least that large even at the optimistic end, so noise
  alone is an unlikely cause.
- **Warn** when the point estimate is +10% or more.
- Report a bench with no base result as "new (no baseline)", and one with
  no head result as "not run at head". Neither counts toward the gate.
- **Fail** when nothing was comparable. A comparison of nothing means
  the run broke.

To run the same check locally:

```bash
scripts/criterion_compare.sh origin/main
uv run python scripts/criterion_gate.py --check \
  --criterion-dir target/criterion-compare \
  --baseline base --warn 10 --fail 25
```

## CI gates — and how to run them locally

Branch protection requires every gate below on every PR, and each one
runs locally.

### Rust

```bash
# Lint: a curated pedantic subset from
# [workspace.lints.clippy], doc gates included.
# Thresholds and doc-valid-idents: clippy.toml
cargo clippy --all-targets --all-features \
  -- -D warnings

# Doctests (CI: the Test job)
cargo test --all-features --doc

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

# Public API surface against the baseline in
# crates/core/api/tylertoo-core.txt. The Public API
# job in ci.yml pins the nightly, because rustdoc
# output drifts between nightlies.
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

# MSRV: <msrv> is rust-version in Cargo.toml
rustup toolchain install <msrv>     # once
cargo +<msrv> check --workspace --all-targets \
  --all-features

# Musl target and the profiling feature still compile
rustup target add x86_64-unknown-linux-musl
cargo check --target x86_64-unknown-linux-musl \
  -p tylertoo
cargo build --release --features dhat-heap

# Convert regression guards
# 1. Structural signature of overview -> export-pmtiles
#    over fixtures-v1 (see "Archive e2e" below)
cargo build --release --package tylertoo
python3 benchmarks/overview/ci_guard.py --check
# 2. Golden tile digests of a full convert -> export
#    build (a slow-set test, so the Slow Tests checks)
cargo test --release -p tylertoo-core \
  --test convert_guard_golden

# Coverage (informational; CI uploads to Codecov)
cargo install cargo-llvm-cov    # once
rustup component add llvm-tools-preview
cargo llvm-cov nextest --all-features --workspace \
  --exclude tylertoo-python \
  --lcov --output-path lcov.info
```

The breaking-change check (`cargo semver-checks`) stays off until the
`tylertoo-core` API freeze. The Public API job still surfaces each API
change in review.

Some thresholds are **ratchets**, set at the current code's level and
marked `RATCHET` in the source. They are cognitive complexity 30 and
too-many-lines 200 in `clippy.toml`, the similarity-rs threshold 0.95,
and xenon's max-absolute C. Lower them as the code improves, and never
raise them.

`cargo shear` also inspects `[workspace.dependencies]`. To keep a
dependency that no code references, such as a transitive version pin,
list it under `[package.metadata.cargo-shear]` in the crate that owns it.

similarity-rs has no baseline file and no per-pair suppression, so you
must refactor away any new finding at or above 0.95. When the API gate
fires on an intended change, regenerate the baseline in the same PR, as
`crates/core/api/README.md` describes.

The golden tile guard works the same way. When a change moves tile
output on purpose, regenerate the golden in the same PR, and say in the
PR body why the output moved:

```bash
TYLERTOO_UPDATE_GOLDEN=1 cargo test -p tylertoo-core \
  --test convert_guard_golden
```

The command rewrites `tests/fixtures/guard/br-clip-divergence.golden.txt`
and then fails, so a regeneration never passes for a green run. A golden
diff on a **dependency bump** is not a cue to regenerate. See
[Dependency updates](#dependency-updates).

### Python (`crates/python`)

Run everything through **uv**, never bare `python` or `pip`:

```bash
cd crates/python
uv sync --group dev
uv run maturin develop     # build the extension module

uv run ruff check .        # strict, docstrings included
uv run ruff format --check .
uv run mypy                # strict typing
uv run python -m mypy.stubtest tylertoo \
  --allowlist stubtest-allowlist.txt
uv run ty check python     # package and extension stub
uv run vulture             # dead code
uv run xenon --max-absolute C --max-modules A \
  --max-average A tests
uv run pytest tests/ -v
uv run pytest --doctest-modules python

# Supply chain
uv export --no-emit-project \
  --format requirements-txt -o /tmp/requirements.txt
uv run pip-audit -r /tmp/requirements.txt --disable-pip

# A release wheel, in target/wheels/
uv run maturin build --release
```

The package uses maturin's mixed layout. `python/tylertoo/__init__.py`
holds the public API, with its types and docstrings. It passes every
argument by keyword to the compiled extension `tylertoo._tylertoo`
(`src/lib.rs`).

When you change a `#[pyo3(signature = ...)]`, update the extension stub
`python/tylertoo/_tylertoo.pyi` and the matching function in
`__init__.py`. stubtest fails on a stale stub. ruff checks the
docstrings with pydocstyle (Google convention), pydoclint, and `W505` at
72 columns. The docstring examples run as doctests against a small
fixture from `python/conftest.py`.

### Archive e2e

`.github/workflows/e2e.yml` builds the release binary and runs the
structural convert guard over the fixtures-v1 inputs. It then opens each
archive it produced with two readers that share no code with the
tylertoo PMTiles writer:

- go-pmtiles `pmtiles verify` checks the header, directories,
  clustering, and zoom bounds.
- `scripts/verify_archive.py` walks the archive with the Python
  `pmtiles` reader and decodes sampled tiles at every zoom with
  `mapbox-vector-tile`.

Locally:

```bash
cargo build --release --package tylertoo

# 1. The guard, keeping the archives
python3 benchmarks/overview/ci_guard.py --check \
  --keep-archives /tmp/e2e-archives

# 2. go-pmtiles at the pinned release, checksummed,
#    installed into .tools/ (gitignored)
scripts/setup_go_pmtiles.sh
eval "$(scripts/setup_go_pmtiles.sh --print-path)"
for f in /tmp/e2e-archives/*.pmtiles; do
  "$PMTILES_BIN" verify "$f"
done

# 3. pmtiles and mapbox-vector-tile (inline deps
#    that uv resolves); the layer is the file stem
for f in /tmp/e2e-archives/*.pmtiles; do
  uv run scripts/verify_archive.py "$f" \
    --layer "$(basename "$f" .pmtiles)" --per-zoom 5
done
```

After an intended output change, regenerate the baseline with a release
build and `python3 benchmarks/overview/ci_guard.py --update`, then commit
`benchmarks/overview/ci_baseline.json`. The guard compares archive bytes
within 2% (`--tolerance`) and everything else exactly. Only tylertoo's
own archives go through `pmtiles verify`. The tippecanoe-made
`tests/fixtures/golden/*.pmtiles` files are inputs, not outputs.

### tippecanoe parity gate

`.github/workflows/tippecanoe-compare.yml` runs on every PR, on each
push to `main`, and weekly. It tiles the fixtures-v1 inputs with tylertoo
and with tippecanoe 2.79.0, built from source at a pinned tag and commit.
It decodes both archives with independent readers and gates the per-zoom
ratios of tiles, features, distinct IDs, vertices, and bytes against
`benchmarks/e2e/tippecanoe_tolerances.toml`.

The ratio table lands in the job summary. A failed scheduled run opens
an issue labelled `tippecanoe-parity`. `benchmarks/e2e/README.md`,
"Parity gate", covers the method and the flag mapping. Locally:

```bash
cargo build --release --package tylertoo

# Pinned tippecanoe, built into .tools/ (gitignored).
# Needs a C++ toolchain and the sqlite3 and zlib headers
benchmarks/e2e/setup_tippecanoe.sh

# The gate: all three fixtures, ~75 s on 16 cores.
# Exits 1 on a breach and names the cell
uv run benchmarks/e2e/compare_tippecanoe.py

# One fixture, keeping the archives and the JSON record
uv run benchmarks/e2e/compare_tippecanoe.py \
  --only open-buildings --work /tmp/cmp --keep \
  --json /tmp/cmp/parity.json
```

The bands are a ratchet, like the convert guard baseline. When a change
moves a ratio on purpose, edit the band in the same PR and record the new
measured value in its comment.

The Test and Coverage jobs build the same pinned tippecanoe, so that
`decode_golden_against_tippecanoe_decode` in `decode_roundtrip.rs` runs.
On CI that test fails when `tippecanoe-decode` is missing, and locally it
skips. To run it locally:

```bash
eval "$(benchmarks/e2e/setup_tippecanoe.sh \
  --print-path)"
cargo test -p tylertoo-core --test decode_roundtrip \
  decode_golden -- --nocapture
```

### Docs

The `Docs` job in `ci.yml` covers the site, under one rule: if a page
shows it, a test runs it. Each tutorial lives as numbered scripts under
`examples/*/`. Its `README.md` shows every script verbatim, followed by
output that the run reproduces. `docs/tutorials/*.md` include those
READMEs, and `docs/index.md` includes the root `README.md`.
`crates/python/tests/docs/conftest.py` states the full policy.

```bash
# Reference pages: regenerate, then commit any diff
cargo run -p tylertoo --features gen-docs -- \
  gen-reference-docs > docs/reference/cli.md
cd crates/python
uv run --no-sync --with docstring-parser==0.18.0 \
  python scripts/gen_reference.py \
  > ../../docs/reference/python.md
cd ../..

# rustdoc, as docs.rs builds it
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps \
  -p tylertoo-core --features remote

# Tutorials, README quickstart, command flags,
# and the strict site build
cargo build --release -p tylertoo
cd crates/python
uv sync --group docs
uv run pytest -m docs tests/docs -v
cd ../..

# Preview at http://localhost:8000/tylertoo/ from
# the repo root. Needs only the docs group, and
# rebuilds when you save a page
uv run --project crates/python --only-group docs \
  zensical serve
```

To change a tutorial, edit its script, run the docs tests, and paste the
new script and output into the README. A failing test names the step and
the lines that differ.

### Docs prose

The `Docs Prose` job lints the handwritten docs with
[Vale](https://vale.sh/) and
[proselint](https://github.com/amperser/proselint), and the pre-commit
hook runs the same script on staged files. `scripts/lint-prose.sh` holds
the file list:

- the top-level `README.md`, `CONTRIBUTING.md`, and `DEVELOPMENT.md`
- the pages under `docs/`
- `examples/*/README.md`

The list includes the generated CLI and Python reference pages, so the
lint reads the clap help and the Python docstrings as they render. After
you edit either, regenerate the page (see [Docs](#docs)) and commit it
with the change. `CHANGELOG.md` is out of scope. Neither tool reads code
blocks or inline code.

A Vale error or any proselint finding fails the check. Vale's warnings
and suggestions are advice: CI prints them after the gate, and they
never fail it.

`.vale.ini` holds Vale's settings, `.vale/styles/Tylertoo-*` the house
rules, and `.vale/tests/` their test cases. `vale sync` downloads the
pinned Google and Microsoft packages into `.vale/styles/`,
which git ignores. Add a real project term to
`.vale/styles/config/vocabularies/Tylertoo/accept.txt` rather than
turning a rule off. `proselint.json` selects proselint's checks.

Install Vale 3.24.0 or later
([install guide](https://vale.sh/docs/install)). uv fetches proselint
0.16.0 itself.

```bash
# Fetch the pinned Vale packages (once)
vale sync

# The gate: every in-scope file, or the files you name
scripts/lint-prose.sh
scripts/lint-prose.sh docs/guides/tippecanoe.md

# Advisory audit: warnings and suggestions too
scripts/lint-prose.sh --list \
  | xargs vale --minAlertLevel=suggestion

# The house rules' own test cases
vale test --coverage .vale/tests \
  .vale/styles/Tylertoo-Docs \
  .vale/styles/Tylertoo-Mechanics \
  .vale/styles/Tylertoo-Terms \
  .vale/styles/Tylertoo-Voice
```

To keep one heading or phrase that a rule misreads, turn that rule off
around it rather than for the whole file:

```markdown
<!-- vale Tylertoo-Mechanics.Headings = NO -->
## A heading that is an external Name
<!-- vale Tylertoo-Mechanics.Headings = YES -->
```

### Workflows

```bash
# CI config lint (action refs must stay SHA-pinned)
uvx zizmor --min-severity low .github/workflows
```

### Version consistency

Five version fields must agree:

- the workspace version in `Cargo.toml`
- the `tylertoo-core` dependency version in `Cargo.toml`
- `crates/python/pyproject.toml`
- `.cz.toml`
- the `tylertoo` entry in `crates/python/uv.lock`

The Version Consistency job checks all five. Move versions only with
`uv run cz bump` from the repo root, as
[CONTRIBUTING.md](CONTRIBUTING.md#releasing-maintainers) describes.

## Scheduled and advisory jobs

These jobs never block a merge.

### Mutation testing

Every PR gets a diff-scoped run (`.github/workflows/mutation-diff.yml`,
check "Mutation score (diff-scoped)"). It runs
`cargo mutants --package tylertoo-core --in-diff` on the PR's diff
against `origin/main`, so it tests only the mutants inside changed core
lines. A typical PR yields a few dozen mutants and takes ten to twenty
minutes, most of it the cold baseline build.

The score and the missed mutants (`file:line`) go to the job summary. A
same-repo PR with missed mutants also gets one sticky comment, updated
on every push. A PR that touches no Rust under `crates/core/` skips the
run, and a diff with no mutation site reports "no mutants in this diff".

`scripts/mutants-diff.sh` is the local twin. It also covers uncommitted
changes and new `.rs` files, and it exits 1 on a missed mutant, so you
can use it as a gate:

```bash
scripts/mutants-diff.sh              # vs origin/main
scripts/mutants-diff.sh main -- -j 2 # base, extra args
```

The weekly job (`.github/workflows/mutation-tests.yml`, Sundays) runs
`cargo mutants` over the whole `tylertoo-core` crate, in shards.
`.cargo/mutants.toml` points it at the `mutants` nextest profile, which
inherits `quick`'s filter, so marking a test slow also drops it from
every mutant's test run. Missed mutants and timeouts show up as the
score in the job summary, not as a red run. If a failed baseline or a
timed-out shard leaves part of the run unmeasured, the job opens or
updates a pinned `mutation-tests` issue. To test a workflow fix
without a full sweep, dispatch a subset of shards:

```bash
gh workflow run mutation-tests.yml -f shard_list=0,1
```

`.cargo/mutants.toml` also lists the test binaries to build. After you
add an integration test binary under `crates/core/tests/` or mark one
slow, run `scripts/mutants-test-binaries.sh` and paste its output there.
Its `--check` flag reports whether you need to, and the Test job runs
that check.

To reproduce locally, with the submodule and fixtures in place:

```bash
cargo install cargo-mutants cargo-nextest --locked
# What would mutate
cargo mutants --package tylertoo-core --list
# One file, in minutes
cargo mutants --package tylertoo-core \
  -f crates/core/src/overview/simplify.rs
# One function's mutants
cargo mutants --package tylertoo-core -F 'fn_name'
# Only code this branch touched
cargo mutants --package tylertoo-core \
  --in-diff <(git diff main...HEAD)
# The full sweep, about one CPU-day
cargo mutants --package tylertoo-core
```

cargo-mutants copies the tree into a scratch directory without
`target/`, so each run above starts with a clean build of the
dependencies at `opt-level = 3`. For a short local run, add
`--in-place`. cargo-mutants then mutates your working tree, reuses its
`target/`, and restores each file at the end, so leave the tree alone
while it runs.

cargo-mutants can't narrow the tests to the mutated file. Narrowing them
by hand loses catches. In a 30-mutant sample, 11 of 28 catches came from
a test in *another* module or an integration binary. Run the whole
`mutants` set, and narrow the *mutants* with `-f`, `-F`, or `--in-diff`
instead.

### Fuzzing

The nightly fuzz job (`.github/workflows/fuzz.yml`, 3 AM UTC) runs each
`cargo-fuzz` target in `fuzz/` for 300 seconds, seeded from
`fuzz/corpus/<target>/seed-*`. The targets cover the parsers that read
third-party bytes:

- the PMTiles header and directory
- an MVT tile body and a whole `--band` archive
- the footer JSON
- Well-Known Binary (WKB) geometry
- the `--filter` grammar

A crash uploads the reproducer as the `fuzz-artifacts-<target>` artifact
and opens or updates a pinned `fuzz` issue. `fuzz/README.md` lists the
targets and shows how to run one and how to turn a crash into a
regression test. The fuzz crate is its own workspace and stays out of
the per-PR build.

```bash
cargo install cargo-fuzz --locked
cd fuzz
cargo +nightly fuzz list
cargo +nightly fuzz run pmtiles_directory -- \
  -max_total_time=60
```

Start any of these jobs by hand with `gh workflow run <file>.yml`.

## Module layout

```text
crates/
├── core/     # all logic: overview/ and shared infrastructure
├── cli/      # argument parsing over core
└── python/   # typed Python API over pyo3 and core
```

`context/ARCHITECTURE.md` holds the full module map and the design
rationale.

## Debugging

```bash
# Pipeline phase timing and diagnostics
RUST_LOG=tylertoo_core::overview=debug \
  cargo run --package tylertoo -- \
  overview in.parquet out.parquet

# Backtrace on a failing test
RUST_BACKTRACE=1 cargo test \
  --package tylertoo-core <test-name>
```

| Problem | Fix |
|---------|-----|
| `protoc` not found during the build | Install `protoc` (see [Initial setup](#initial-setup)) |
| Linker errors on macOS | `xcode-select --install` |
| A test can't find a fixture | Tests run from the workspace root, so use paths like `tests/fixtures/...` |
| stubtest fails after a binding change | Update `_tylertoo.pyi` and the wrapper in `__init__.py` to match the new `#[pyo3(signature)]` |
| Python Quality fails on `uv sync --locked` | Run `cd crates/python && uv lock` and commit `uv.lock` |

## Dependency updates

The repo commits `Cargo.lock`, so every build resolves to the same
versions, from a laptop to the release artifacts. To update it, run
`cargo update -p <crate>` or edit a manifest, and commit the new lock
with the change. The Security Audit job fails when the lock falls out
of step with the manifests.

Dependabot opens weekly updates for every dependency ecosystem in the
repo. Patch and minor updates merge on their own once all gates
pass, and majors wait for a human. The weekly security job runs
cargo-audit, cargo-deny, and pip-audit. It first re-resolves
`Cargo.lock` from scratch, as early warning of what the next bump pulls
in. On failure it opens or updates a pinned `security-audit`
issue.

**Geometry-engine bumps are the exception.** `geo`, `geo-types`,
`i_overlay`, `i_float`, `i_shape`, and `earcut` can change rendered tile
geometry at any semver level, so green CI alone does not approve one. The
golden tile guard fails on any output change, and it runs in the required
Slow Tests checks. Such a bump therefore reaches a human as a red check,
not as a merge. See "Decision Record: Geometry-Engine Dependency Bumps"
in `context/ARCHITECTURE.md`.

## Resources

- [Criterion.rs](https://bheisler.github.io/criterion.rs/book/)
- [PyO3](https://pyo3.rs/)
- [MVT specification](https://github.com/mapbox/vector-tile-spec)
- [PMTiles specification](https://github.com/protomaps/PMTiles)
- [GeoParquet specification](https://geoparquet.org/)
