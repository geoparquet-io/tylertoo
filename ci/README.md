# CI gates

This page lists every gate that can stop a change. For each gate it
gives what the gate asserts and whether branch protection requires it.
It also names the canary that proves the gate can still fail, and the
command that runs it locally.
`DEVELOPMENT.md` explains how to work with each gate day to day.

A gate is only worth its green check if it can go red. So every gate
has one of these:

- a canary in [`canaries/`](canaries/README.md), which the nightly
  `gate-canary.yml` applies to main, expecting the gate to fail;
- a test that is `xfail(strict)` or `#[ignore = "#NNN"]`, keyed to the
  issue it catches;
- staleness coverage: `gate-canary.yml` fails when the gate's scheduled
  run has not succeeded for 8 days, or when the issue it files has
  been open for more than 7.

A gate with none of the three is a gap. Add a canary.

## Required checks

Branch protection requires three contexts:

- `CI passed`, which fails when any job in `ci.yml` failed or was
  cancelled;
- `tippecanoe parity`;
- `Archive e2e (independent readers)`.

Required checks are not strict, and there is no merge queue. So each
push to main runs the full set, as the backstop. Pushes to main are
never cancelled: each commit gets its own concurrency group (#779).

## The gates

"Required" means the gate is part of a required context. Canary names
refer to `canaries/<name>.patch`.

### `ci.yml`: every PR and every push to main

| Gate (job, step) | Asserts | Required | Canary | Run locally |
|---|---|---|---|---|
| Check, formatting | `cargo fmt --check` is clean | yes | `g4-fmt` | `cargo fmt --all -- --check` |
| Check, clippy | no clippy warning, with `-D warnings` over the curated `[workspace.lints]` set | yes | `g4-clippy` | `cargo clippy --locked --all-targets --all-features -- -D warnings` |
| Check, musl, and shear | the musl target compiles with both allocator setups; no unused dependency | yes | none | see `DEVELOPMENT.md`, "Rust" |
| Check, duplicated code | no pair at or above 0.95 similarity (similarity-rs) | yes | G1 adds one | `similarity-rs --threshold 0.95 --min-lines 10 --skip-test crates/core/src crates/cli/src crates/python/src` |
| Check, structural gates | the `clippy.toml` `disallowed-*` rules, the counts in `ci/ratchets.toml`, and mirrored hunks (G1, #779) | yes | G1's `g1-*` | `DEVELOPMENT.md`, "Structural gates" |
| Check, unit tests | `cargo test --lib --all-features` | yes | none | same command |
| Test, quick tier | the nextest `quick` profile on ubuntu and macOS, plus beta on main | yes | `g4-cli-flag-parity` (the tiles/export flag parity test) | `cargo nextest run --profile quick --all-features` |
| Test, doctests | `cargo test --doc` | yes | none | `cargo test --all-features --doc` |
| Test, mutation binaries | `.cargo/mutants.toml` lists every test binary the mutation run must build | yes | none | `scripts/mutants-test-binaries.sh --check` |
| Slow Tests | the slow set from `.config/nextest.toml`: determinism, shard parity, the golden tile guard `convert_guard_golden`, `leaf_directory_integration` | yes | G3 adds one (pmtiles absent) | `cargo nextest run --profile full --all-features` |
| Python Quality | ruff, mypy strict, stubtest, ty, doctests, pytest, vulture, xenon, pip-audit | yes | none | `DEVELOPMENT.md`, "Python" |
| Python Tests | pytest on macOS and Windows | yes | none | `cd crates/python && uv run pytest tests/` |
| Docs | the committed references match regeneration; rustdoc `-D warnings`; tutorials; strict site build | yes | none | `DEVELOPMENT.md`, "Docs" |
| Coverage | line coverage over the quick tier is at least 94% for the workspace and for `crates/core`, and at least 81.9% for `crates/cli` (#779). Python is excluded. | yes | none | see below |
| Lint | versions in sync, zizmor, Vale and proselint, cargo-audit, cargo-deny, MSRV build, public API diff | yes | none | `DEVELOPMENT.md`, "Rust" |
| Gate scripts, pytest | the gate helpers in `scripts/ci/` behave, and every canary parses | yes | none | `uv run --no-project --with pytest==8.3.4 --with pyyaml==6.0.2 pytest scripts/ci/tests` |
| Gate scripts, baseline ratchet | the PR loosens no tippecanoe band and changes no `ci_baseline.json`, `tests/fixtures/guard/*.golden.txt` or `tests/fixtures/golden/*`, unless a commit carries `Baseline-change: #NNN <measured evidence>` | yes | `g4-band-widening` | `python3 scripts/ci/baseline_ratchet.py --base origin/main --head HEAD` |

Run the coverage gate locally:

```bash
cargo llvm-cov nextest --all-features --workspace --exclude tylertoo-python \
  --lcov --output-path lcov.info
python3 scripts/coverage_gate.py --check lcov.info --min 94 \
  --floor 'crates/core/**=94' --floor 'crates/cli/**=81.9'
```

### The other workflows

| Gate (workflow) | Asserts | Required | Canary | Run locally |
|---|---|---|---|---|
| Archive e2e (`e2e.yml`) | `ci_guard.py`'s structural fingerprint equals `ci_baseline.json`, with archive bytes within 2%; `pmtiles verify` passes; `verify_archive.py` decodes the archives | yes | `g4-e2e-baseline`, plus G3's | `DEVELOPMENT.md`, "Archive e2e" |
| tippecanoe parity (`tippecanoe-compare.yml`) | each tylertoo/tippecanoe ratio, per zoom, sits in its band in `benchmarks/e2e/tippecanoe_tolerances.toml` | yes | none for the comparison itself; `g4-band-widening` keeps the bands from widening | `cargo build --release -p tylertoo && benchmarks/e2e/setup_tippecanoe.sh && uv run benchmarks/e2e/compare_tippecanoe.py` |
| Mutation gate (`mutation-diff.yml`) | on a PR titled `fix…`, no mutant on a changed line of `crates/core` or `crates/cli` is missed unless it is annotated `// mutants: equivalent #NNN`. Advisory on other PRs. | no; recommended (#779) | none | `scripts/mutants-diff.sh origin/main` |
| CodSpeed (`codspeed.yml`) | instruction counts per bench against main | no | none | `DEVELOPMENT.md`, "CodSpeed" |
| Criterion regression (`bench-regression.yml`) | no gated bench is more than 10% slower (50% for the noisy set). Runs on the `benchmark` label and weekly. | no | staleness (weekly run) | `DEVELOPMENT.md`, "Criterion regression gate" |
| Weekly mutation sweep (`mutation-tests.yml`) | every shard of the full core sweep completes. It sets no score threshold. | no | staleness (weekly run, `mutation-tests` issue) | `cargo mutants --package tylertoo-core --shard 0/40` |
| Nightly fuzz (`fuzz.yml`) | no crash, timeout, or OOM in 5 minutes per target. The held-out targets are listed in `FUZZ_HOLDOUTS`. | no | staleness (nightly run, `fuzz` issue, hold-outs that link a closed issue) | `cd fuzz && cargo +nightly-2026-09-15 fuzz run <target>` |
| Fuzz seeds (`crates/core/tests/fuzz_seeds.rs`, in Test) | every committed seed, held-out targets included, replays without a panic | yes | none | `cargo nextest run --all-features -E 'binary(fuzz_seeds)'` |
| Security audit (`security.yml`) | a fresh dependency resolution has no advisory | no | staleness (weekly run, `security-audit` issue) | `cargo update && cargo audit && cargo deny check` |
| Gate canaries (`gate-canary.yml`) | every canary turns its gate red; nothing in the staleness list is stale | no | itself: a canary that stays green is the finding | `uv run scripts/ci/canary.py run ci/canaries/<name>.patch` (on a scratch branch); `GH_REPO=geoparquet-io/tylertoo uv run scripts/ci/staleness.py` |
| pre-commit hook (`.githooks/pre-commit`) | fmt, clippy, and prose lint on the staged files | local only | `g4-fmt` and `g4-clippy` cover the same checks in CI | `git config core.hooksPath .githooks` |

## Rules that come with the gates

- **Changing a baseline needs evidence.** A PR that loosens a band or
  re-cuts a baseline needs a commit trailer, such as
  `Baseline-change: #761 z3 features 0.910 (CI run 1234)`. Without one,
  the baseline ratchet fails. Narrowing a band needs no trailer.
- **The issue closes after a full scheduled run.** The issue a gate
  files closes only after a full green scheduled run, never after a
  dispatched subset. #635 closed after a check of 2 of 40 shards, and
  the next sweep failed again (#677).
- **Fix PRs pin their lines.** On a `fix` PR, a missed mutant on a
  changed line fails the Mutation gate. Add the test that kills it, or
  annotate a mutant that is equivalent.
- **Fix at the owner.** `CLAUDE.md`, "Fix at the owner", has the rule.
  The structural gates and the parity gates enforce it.
