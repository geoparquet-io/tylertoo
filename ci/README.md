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
| similarity-rs 0.5.0 | Rust functions in all production source directories; native within-file comparison | `similarity-rs --threshold 0.95 --min-lines 10 --skip-test --fail-on-duplicates crates/core/src crates/cli/src crates/python/src` |
| cargo-mutants | Direct exit status for mutants on PR diff lines in core and CLI; blocking through `CI passed` | `scripts/mutants-diff.sh origin/main` |
| Codecov | Native project targets: workspace 94%, core 94%, CLI 81.9%; `codecov.yml` | `cargo llvm-cov nextest --all-features --workspace --exclude tylertoo-python --lcov --output-path lcov.info` |
| cargo-public-api | Exported core API against `crates/core/api/tylertoo-core.txt` | `cd crates/core && cargo +nightly-2026-09-15 public-api --simplified \| diff -u api/tylertoo-core.txt -` |
| vulture | Package and tests, confidence 60; `[tool.vulture]` in `crates/python/pyproject.toml` | `cd crates/python && uv run vulture` |
| stubtest | Python extension and type stub agreement | `cd crates/python && uv run python -m mypy.stubtest tylertoo` |
| nextest | Quick/full tiers and timeouts in `.config/nextest.toml` | `cargo nextest run --profile quick` |

Check runs clippy, jscpd and similarity-rs on Python-only code changes.
Lint runs cargo-deny independently of Rust build classification. Python Quality
runs vulture. The Gate scripts job contains the single `scripts/ci` pytest
step, covering the independent archive verifier.

The archive e2e workflow still runs its strict invariant matrix and independent
readers (`pmtiles verify` and `scripts/verify_archive.py`). Feature accounting
requires input count = written distinct source IDs + diagnosed losses. The
existing #707 exemptions remain until reports contain exact per-source losses;
pair counters can overlap. Exemptions validate issue references, reject duplicates
and fail on XPASS. Their open-issue check remains in the e2e workflow.

The tippecanoe comparison, golden archives and tile digests remain behavior
tests. Weekly mutation, security, fuzzing and benchmark workflows remain.

`.github/CODEOWNERS` assigns architecture policy and exemption files to
`@nlebovits`. Code-owner review and the native Codecov statuses must be enabled
in repository branch protection to make those external controls required.
