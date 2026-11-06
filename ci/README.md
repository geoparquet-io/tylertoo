# CI tools

`CI passed` aggregates the jobs in `.github/workflows/ci.yml`. A failed or
cancelled dependency fails that status. Python-only PRs run Check, Lint,
Python Quality and Python Tests; they may skip the Rust integration matrix.

| Tool | Scope and configuration | Local command |
|---|---|---|
| rustfmt | Rust formatting | `cargo fmt --all --check` |
| clippy | Workspace lints and owner-specific call restrictions in `clippy.toml` | `cargo clippy --workspace --all-targets --all-features -- -D warnings` |
| cargo-deny | Advisories, licenses, dependency versions and sources in `deny.toml` | `cargo deny check` |
| cargo-audit | Dependency advisories | `cargo audit` |
| cargo-shear | Unused dependencies | `cargo shear` |
| jscpd 4.0.5 | Rust/Python token clones across and within files; `.jscpd.json` | `npx --yes jscpd@4.0.5 --config .jscpd.json` |
| similarity-rs 0.5.0 | Rust functions in all production source directories; native within-file comparison | `similarity-rs --threshold 0.938 --min-lines 10 --skip-test --fail-on-duplicates crates/io/src crates/core/src crates/cli/src crates/python/src` |
| Codecov | Native project targets: workspace 94%, core 94%, CLI 81.9%; `codecov.yml` | `cargo llvm-cov nextest --all-features --workspace --exclude tylertoo-python --lcov --output-path lcov.info` |
| cargo-public-api | Exported core API against `crates/core/api/tylertoo-core.txt`; `crates/core/tests/public_api_gate.rs` fails if that baseline is missing or truncated, names a path outside its allowlist of roots (`tylertoo_core`, `core`, `alloc`, `std`, `serde_core`, `serde_json`), or names a non-stable core item | `cd crates/core && cargo +nightly-2026-09-15 public-api --simplified \| diff -u api/tylertoo-core.txt -` |
| vulture | Package and tests, confidence 60; `[tool.vulture]` in `crates/python/pyproject.toml` | `cd crates/python && uv run vulture` |
| stubtest | Python extension and type stub agreement | `cd crates/python && uv run python -m mypy.stubtest tylertoo` |
| nextest | Quick/full tiers and timeouts in `.config/nextest.toml` | `cargo nextest run --profile quick` |

Workspace Rust lints deny unused and unreachable items, unexplained ignores,
`#[allow]`, and functions with more than seven arguments. Scoped exceptions
use `#[expect(..., reason = "#issue: why")]`; an obsolete expectation is an
error. `clippy.toml` also rejects raw reader construction outside its checked
I/O owner and synchronization types that could hide process-wide warnings.
Runtime/client caches, output-name counters, per-run worker synchronization,
profiling and test allocators have scoped expectations beside their owners.
Diagnostic counts belong to the per-operation `Diagnostics` channel.

Similarity-rs runs at 0.938. Its strongest remaining match is the distinct
export and pass-2 timer snapshots at 93.70%; 0.937 rejects that pair. At 0.85
it also matches unrelated defaults, typed adapters and independent geometry
algorithms. Version 0.5.0 recognizes a helper's own `#[cfg(test)]` attribute
with `--skip-test`, but does not inherit the enclosing module's attribute;
distinct fixture generators carry it explicitly. No files are excluded.
Jscpd supplies cross-file token comparison and includes the I/O crate.

Check runs clippy, jscpd and similarity-rs on Python-only code changes.
Lint runs cargo-deny independently of Rust build classification. Python Quality
runs vulture. The Gate scripts job runs the `scripts/ci/tests` pytest step
(the independent archive verifier, the bench list, the exemptions count, the
docs generator, and a check that `CI passed` needs every job in `ci.yml`), the
benchmark harness tests (`benchmarks/test_run_manifest.py`,
`benchmarks/test_valve_guard.py`, `benchmarks/coalesce/test_run.py`,
`benchmarks/e2e/test_run_e2e.py`), and `scripts/gen_bench_docs.py --check`,
which fails when a published benchmark number differs from
`benchmarks/results/`.

Every job that fetches the `fixtures-v1` release, from the cache or the
download, then runs `scripts/ci/verify_fixtures.sh`, which fails on any file
that does not match `benchmarks/fixtures.sha256`.

## Benchmarks and exemptions

`crates/core/benches/benches.toml` is the one bench list.
`scripts/bench_list.py` reads it for the weekly wall-clock gate
(`bench-regression.yml` through `scripts/criterion_compare.sh`), for
CodSpeed's per-PR selection (`codspeed.yml`), and for the generated table in
`DEVELOPMENT.md`. CodSpeed runs only the CPU-bound benches; it runs on every
push to `main` so that each PR has a baseline at its base commit.

`ci/exemptions.toml` holds every exemption from every check, each with its
reason: benches CodSpeed or the weekly gate skip, and the noisy globs the
weekly gate fails at +50% instead of +10%. `scripts/ci/tests/test_exemptions.py`
pins the entry count, so a new exemption also changes that test.

| Check | Local command |
|---|---|
| Bench list, exemptions, `DEVELOPMENT.md` table | `scripts/bench_list.py check` (`scripts/bench_list.py docs` rewrites the table) |
| Gate scripts tests | `uv run --no-project --with pytest==8.4.2 --with pmtiles==3.8.1 --with mapbox-vector-tile==2.2.0 --with shapely==2.2.0 --with pyarrow==26.0.0 --with pyyaml==6.0.3 pytest -q scripts/ci/tests` |
| Harness tests | `uv run --no-project --python 3.12 --with pytest==8.4.2 pytest -q benchmarks/test_run_manifest.py benchmarks/test_valve_guard.py benchmarks/coalesce/test_run.py benchmarks/e2e/test_run_e2e.py` (needs GNU `time`) |
| Generated docs | `uv run --no-project python scripts/gen_bench_docs.py --check` |
| Fixtures | `scripts/ci/verify_fixtures.sh tests/fixtures/realdata` |

`valve-guard.yml` runs nightly on `ubuntu-latest`, where only its merge:drop
ratio half applies. When the repository variable `TYLERTOO_BENCH_RUNNER` is
`true`, a second leg runs on the maintainer's self-hosted runner (labels
`self-hosted`, `tylertoo-bench`), where the baseline half applies too. The
workflow triggers only on `schedule` and `workflow_dispatch`, never on pull
requests.

The archive e2e workflow still runs its strict invariant matrix and independent
readers (`pmtiles verify` and `scripts/verify_archive.py`). Feature accounting
requires input count = written distinct source IDs + diagnosed losses. The
existing #707 exemptions remain until reports contain exact per-source losses;
pair counters can overlap. Exemptions validate issue references, reject duplicates
and fail on XPASS. Their open-issue check remains in the e2e workflow.

The tippecanoe comparison, golden archives and tile digests remain behavior
tests. Weekly security, fuzzing and benchmark workflows remain.

Mutation testing is gone; targeted property suites replace it: proptest in
`crates/core/tests/properties_*.rs`, Hypothesis in
`crates/python/tests/test_properties.py`, and the `fuzz/` targets.

`.github/CODEOWNERS` assigns architecture policy and exemption files to
`@nlebovits`. Code-owner review and the native Codecov statuses must be enabled
in repository branch protection to make those external controls required.
