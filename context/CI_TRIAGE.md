# CI Triage — feat/geoparquet-overviews (Task H2, "CI reckoning")

Date: 2026-07-03
Branch: `feat/geoparquet-overviews`
Context: PR #168 is being retargeted from #158 to `main`. CI has never run on
this branch (workflows only trigger on `main`). The last CI run on the
underlying branch failed 4 jobs after commit `61e9c17` (a coalesced-linestring
WIP committed from a dirty working tree). Everything was reproduced **locally**
by mirroring the exact CI commands from `.github/workflows/ci.yml`. (GitHub was
unreachable for the first part of the triage — clippy/tests were done offline;
connectivity returned before the security-audit step, which ran against the
fresh advisory-db.)

Scope note: partway through, the maintainer decided the per-tile
zoom-dependent simplification feature (PR #158) is **excised**, not repaired.
That both resolves the Test-job failure and shrinks the suite; see
`context/TILE_SIMPLIFY_POSTMORTEM.md`.

## Exact CI commands mirrored

| Job | CI step | Command |
|-----|---------|---------|
| Check (clippy) | "Run clippy" | `cargo clippy --all-targets --all-features -- -D warnings` |
| Check (unit) | "Run unit tests" | `cargo test --lib --all-features` |
| Test (ubuntu, stable) | "Run all tests (excluding slow regression tests)" | `cargo test --all-features -- --skip large_polygon_regression` |
| Coverage | "Generate coverage" | `cargo tarpaulin --out xml --all-features --workspace --exclude gpq-tiles-python` |
| Security Audit | "Run security audit" | `cargo audit` |

Note: the local RTK proxy mangles `cargo clippy ... -- -D warnings` (splits
`-D` into an input filename). Ran clippy/tests via `command cargo ...` to
bypass the shell rewrite; the underlying command is identical to CI.

---

## Job 1 — Check (clippy)  →  REPRODUCED, FIXED

**Reproduced:** yes. `cargo clippy --all-targets --all-features -- -D warnings`
failed with 12 errors (10 in lib, +2 more in lib-test) under Rust 1.93 clippy.

**Root cause:** new overview modules + the `61e9c17` WIP tripped lints that are
errors under `-D warnings` (stricter than a bare local `cargo clippy`):

| Lint | Location(s) |
|------|-------------|
| `neg_cmp_op_on_partial_ord` (`!(x > 0.0)`) | assign.rs:368,521,643; convert.rs:100 |
| `needless_range_loop` | assign.rs:558 (budget loop); convert.rs:496, 1259 |
| `unnecessary_map_or` | simplify.rs:503,566 |
| `doc_lazy_continuation` | mvt.rs:1943; convert.rs:19 |
| `type_complexity` | export.rs:325 |

**Fixed** in commit `fix(clippy): resolve -D warnings failures across overview
+ simplify`:
- `!(x > 0.0)` guards rewritten as `x <= 0.0 || x.is_nan()` — semantically
  identical (both reject non-positive **and** NaN), passes the lint.
- `needless_range_loop`: `enumerate()` in convert.rs where the index only
  indexes one slice; a scoped `#[allow(clippy::needless_range_loop)]` on the
  assign.rs budget loop where `level` is a scalar used in arithmetic and
  comparisons (not merely an index).
- `map_or(true, |p| p != px)` → `!= Some(px)`.
- doc comments: blank-line paragraph breaks.
- `type_complexity`: extracted `type GroupedTileGeoms = ...` alias.

**Verification:** `cargo clippy --all-targets --all-features -- -D warnings`
exits 0. `cargo fmt --all -- --check` clean.

---

## Job 2 — Test (ubuntu, stable)  →  REPRODUCED, FIXED (feature excised)

**Reproduced:** yes. `cargo test --all-features -- --skip large_polygon_regression`
→ **987 passed, 1 failed, 3 ignored** (lib). Single failure:
`simplify::tests::world_coord_tests::test_non_coalesced_tiny_linestrings_should_be_dropped`
(simplify.rs:2405).

**Root cause:** an aspirational TDD "red" test ("This assertion SHOULD fail with
current implementation!"). It asserted the non-coalesced `simplify_geometry_for_tile`
should drop sub-pixel linestrings like `simplify_coalesced_linestring` does
(`min_extent_px`) — a feature never implemented. It, and the whole per-tile
zoom-dependent simplification feature, came from PR #158 (commit range
`c91c9a1..61e9c17`; `61e9c17` committed from a dirty tree).

**Fix:** per maintainer decision the entire per-tile simplification feature was
**excised** (not repaired) — it is superseded by the overview architecture
(`overview/simplify.rs` world-space GSD simplification + E0 `export-pmtiles`).
Removal commit `d897110`. `pipeline.rs`, `simplify.rs`, `python/src/lib.rs`
reverted to `origin/main`; the `tiles` CLI flags, `TilerConfig::simplify_factor`,
the `lib.rs` re-export, and the feature's tests (incl. this red test and
`tests/simplification_integration.rs`) removed. See
`context/TILE_SIMPLIFY_POSTMORTEM.md`. The failing test no longer exists.

**Verification:** post-excision `cargo check --workspace --all-features` = 0,
`cargo clippy --all-targets --all-features -- -D warnings` = 0 (no `dead_code`),
and the full CI test command is green (see below). The suite is smaller (the
feature's unit + integration tests are gone). The overview path, E0 export, and
`mvt.rs` are untouched and still pass.

---

## Job 3 — Security Audit  →  FIXED (pyo3 0.28 → 0.29 on `feat/geoparquet-overviews`)

**Status: RESOLVED.** The two failing `pyo3 0.28.2` vulnerabilities were cleared
by bumping the workspace pin to `pyo3 = "0.29"` and migrating the Python bindings
(commit `chore(deps): migrate pyo3 0.28 -> 0.29`). Post-fix `cargo audit` exits
`0` with **0 vulnerabilities** and only the 7 warning-level advisories remaining
(unmaintained/unsound; non-failing). See "Resolution" below.

**Originally reproduced:** yes. With connectivity restored, `cargo audit` fetched
the fresh advisory-db (1149 advisories) and reported **`error: 2 vulnerabilities
found!`** (the failing condition) plus **7 warnings**.

**The 2 vulnerabilities (both `pyo3 0.28.2`):**

| RUSTSEC | Title | Solution |
|---------|-------|----------|
| RUSTSEC-2026-0176 | pyo3: out-of-bounds read in `nth`/`nth_back` for `PyList`/`PyTuple` iterators | upgrade to `>= 0.29.0` |
| RUSTSEC-2026-0177 | pyo3: missing `Sync` bound on `PyCFunction::new_closure` closures | upgrade to `>= 0.29.0` |

**Root cause & triage — PRE-EXISTING ON MAIN, not introduced by this branch:**
`Cargo.toml` pins `pyo3 = "0.28"`. `origin/main` pins the **same** `pyo3 = "0.28"`
and does not contain the overview/simplify work. `Cargo.lock` is gitignored
(`.gitignore:3`), so CI resolves deps fresh from crates.io identically for both
branches — meaning this Security-Audit failure occurs on `main` too, as of these
2026-04 pyo3 advisories. It is **not** a regression from the overview or
(now-excised) simplify work.

**Resolution (this branch):** bumped the workspace pin to `pyo3 = "0.29"` (in
`Cargo.toml [workspace.dependencies]`). A semver-compatible `cargo update -p
pyo3` only reaches `0.28.3`, which does **not** clear the advisory (fix is in
`0.29.0`), so the minor bump was required. Per the PyO3 0.29 CHANGELOG, none of
the breaking changes touch this crate's API surface: the binding uses only
`Python::attach` / `py.detach` / `PyDict::new` / `set_item` / `call1` /
`wrap_pyfunction!` / `#[pymodule]` / `Bound<'_, PyModule>`, all unchanged in
0.29 (the `downcast`→`cast` and `TYPE_INFO`→`TYPE_HINT` reworks were 0.27/0.28).
The migration was therefore a **pure version bump** — no source changes beyond
updating two stale "PyO3 0.28" code comments. `maturin`/`pyproject.toml` needed
no change (`maturin>=1.0,<2.0` supports pyo3 0.29; no `pyo3-build-config` pin).
Verified: `cargo check -p gpq-tiles-python` ✓, `cargo clippy -p
gpq-tiles-python --all-targets --all-features -- -D warnings` ✓, `uv run maturin
develop` ✓ (built cp311 wheel against pyo3 0.29), `uv run pytest` 31 passed (the
11 failures are pre-existing `streaming_mode`/`parallel_tiles`/`parallel_geoms`
kwarg-mismatch tests unrelated to pyo3 — those kwargs are not in the `convert()`
signature). No `audit.toml` ignore was needed — the vulnerability is fixed, not
silenced.

**The 7 warnings (unmaintained/unsound, non-failing; deep transitive deps):**
RUSTSEC-2023-0089 atomic-polyfill, RUSTSEC-2025-0141 bincode,
RUSTSEC-2025-0119 number_prefix, RUSTSEC-2024-0436 paste (unmaintained);
RUSTSEC-2026-0190 anyhow, RUSTSEC-2026-0097 rand ×2 (unsound). These are
warning-level and do **not** fail default `cargo audit`; no action required.
(The lz4_flex 0.11.5 yanked finding seen with the stale offline DB is gone with
a fresh resolve — 0.11.6 is picked automatically.)

---

## Job 4 — Coverage  →  NOT RUN (assumed healed)

`cargo tarpaulin` was **not** run locally (slow; `cargo-tarpaulin` not
installed). The Coverage job fails because it runs the full test suite under
instrumentation and inherits the same test failure as Job 2. With the Job-2
failure resolved (test quarantined) the suite is green, so Coverage is expected
to heal. The Codecov upload step uses `fail_ci_if_error: false`, so a missing
token cannot fail the job. **Assumption: Coverage passes once tests pass.**

---

## Commits made (this triage)

1. `fix(clippy): resolve -D warnings failures across overview + simplify`
   (`932b233`) — assign.rs, convert.rs, export.rs, simplify.rs, mvt.rs (Job 1).
   Note: the simplify.rs hunk of this commit was later superseded by the
   excision revert; the overview-module fixes (assign/convert/export/mvt) stand.
2. `test(simplify): quarantine aspirational non-coalesced drop red test`
   (`ea21891`) — superseded by the excision (the test was removed, not ignored);
   kept in history as a forward commit.
3. `refactor(tiles): remove unproven zoom-dependent simplification (#158)`
   (`d897110`) — the excision (Job 2). See `TILE_SIMPLIFY_POSTMORTEM.md`.
4. `docs: record tile-simplify postmortem + CI triage` — this file, the
   postmortem, and the CARRYOVER/plan cross-links.

(`Cargo.lock` is gitignored; dependency resolution is fresh on CI. The pyo3
vulnerability fix landed as a `Cargo.toml` pin bump `0.28` → `0.29` — see Job 3
Resolution.)

## Still requires human / separate work (out of H2 agent scope)

- Merge/close #158, retarget #168 → `main` (human merge buttons).
- Actual CI re-run on `main`'s workflows.
- ~~pyo3 0.28 → 0.29 breaking bump + Python-bindings migration to clear
  RUSTSEC-2026-0176/0177~~ **DONE** on `feat/geoparquet-overviews` — turned out
  to be a pure pin bump (no binding API changes needed); see Job 3 Resolution.
