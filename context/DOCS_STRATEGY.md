# Docs strategy for the pre-release overhaul

Status: Phase 1 output, awaiting maintainer review. Nothing in Phase 2
(writing) starts until this file is approved. Delete this file, or move it to
`context/archive/`, when the overhaul merges.

Each section was written by a separate research agent against `origin/main`
at f52847c1, checking claims against code, not against the existing docs. The
sections are self-contained: keep, cut, auto-generate, CI test, how it stays
accurate, and an ordered work list for the writer.

## 0. Cross-section rules and decisions

Where a section below disagrees with this section, this section wins.

### Settled

- **Benchmark tiers.** No self-hosted runner and no cloud setup. Nightly runs
  the small cells on GitHub-hosted runners as a regression gate. The release
  tier is one manual command on the 64-core development machine
  (`benchmarks/release.sh`) and is the only writer of
  `benchmarks/results/*.json`, the comparison archives, and
  `docs/benchmarks.md`.
- **Comparison archives.** Uploaded to
  `s3://nlebovits/tylertoo-benchmarks/<version>/` with the local `source-coop`
  AWS profile. Write access and public read were verified on 2026-11-27.
- **Comparison manifest path.** `benchmarks/results/comparison-pmtiles.json`,
  with the schema from section 2. Section 3's generator reads that file.
- **Tuning page ownership.** Section 5 owns the page: its path, its anchors,
  and its rule that the page states no defaults and no measured numbers.
  Where section 3 repoints a link to `OVERVIEW_TUNING/#...`, the target is the
  matching anchor on the section 5 page. `MEMORY_SIZING_DOC` points at that
  page's memory anchor.
- **Sharded builds.** After `docs/guides/scaling.md` is deleted, sharding is
  taught by the Brazil tutorial and described by the `--shard`, `shard-plan`,
  and `merge` help strings. Sections 1, 3, and 5 agree on this.
- **First comparison manifest.** Phase 2 ends with one full release-tier run
  on this branch, so the manifest and archives exist before the PR merges.
- **Brazil stays** as the second tutorial. The Africa block on the benchmarks
  page prints its generated command as a recipe.
- **Writing order in Phase 2.** Code-side prerequisites, tutorials, guides and
  deletions, tuning page, then benchmarks. One PR, one commit per section.

### Decided by the maintainer on 2026-11-27

Rows 1, 2, 4, 6, 7, and 8 below are approved as recommended. For row 8, the
memory preflight message prints the calculation for the user's data from the
constants, and the tuning page explains the remedies in words only. The other
rows follow their recommendation unless the maintainer says otherwise.

| # | Section | Decision | Recommendation |
|---|---|---|---|
| 1 | Tutorials | The new prepared Madagascar fixture must be attached to the `fixtures-v1` GitHub release (or a new fixtures release) before the PR's CI can pass. | Attach to `fixtures-v1`. |
| 2 | Tutorials | Switch the README Quickstart to the prepared fixture. Today it feeds the raw file and prints two WGS84 warnings. | Switch. |
| 3 | Benchmarks | Create a `bench-inputs-v2` GitHub release holding the Moldova encoding variants so the nightly runner can download them. | Create it. |
| 4 | Benchmarks | Chart WKT and GEOGRAPHY input in the input-type matrix. gpio writes neither, so each needs a hand-built input. | Leave both out; chart WKB, native GeoArrow, and Parquet GEOMETRY. |
| 5 | Benchmarks | Confirm that `source-coop creds` refreshes unattended on this machine. The agent couldn't read the credential cache. | Check before the first release-tier run. |
| 6 | Guides | `preset.rs` cites tippecanoe v2.82.0 but CI pins 2.79.0. | Re-pin CI to 2.82.0. |
| 7 | Tuning | New page path `docs/reference/tuning.md`, and whether the old `OVERVIEW_TUNING/` URL keeps a stub. | New path; no stub, because the link test catches every inbound link. |
| 8 | Tuning | Memory-sizing facts from `scaling.md` are numeric (bytes per row, multipliers). The tuning page bans numbers. | Put the numbers in the preflight error message and help strings, generated from the constants; the page explains the choice in words. |
| 9 | Tuning | Ban measured numbers in help strings too. | Yes, except numbers formatted from a constant. |
| 10 | Tuning | Coverage-fill cost: help says 10–17%, the page says 2.4–2.8×. | Measure once in the release-tier run, or state neither. |
| 11 | Tuning | Remove the staging environment variables from user docs. | Remove; list them in a generated `ENVIRONMENT` table in the CLI reference. |

### Found along the way

- The nightly Brazil tutorial job has failed every night since at least
  2026-11-20 because the expected output drifted. Nothing alerted anyone.
  Section 1 fixes the output and adds a failure notification.
- The self-hosted legs of `bench-regression.yml` and `valve-guard.yml` never
  run: no runner is registered. Section 2 removes them.
- The current tuning page has 19 bad claims: 8 contradicted by code, 11 with
  no source. Section 5 lists them.

## 1. Tutorials

Evidence for this section, gathered 2026-11-27 at `f52847c1`:

- **Madagascar.** Steps 02–05 on today's `prepared.parquet` (release build,
  gpio 1.7.0, duckdb-cli 1.5.6) match every `text` line. On the proposed
  fixture, tiles, stats, and decode are identical; only the overview `bytes`
  column in steps 2 and 3 changes.
- **Brazil.** The nightly has failed every night since at least 2026-11-20
  (latest: run 45979409335) because its output drifted. Work item 9 quotes
  the real numbers.
- **The input check.** tylertoo has no check subcommand, only read-time
  warnings (`UnsortedInput`, `CrsAssumed`, `CrsMetadata`). "Clearly passes"
  means `gpio check all --strict` exits 0 and tylertoo warns about nothing in
  the input. Today's `prepared.parquet` fails `--strict` (3 row groups of
  ~5.8k rows). The raw fixture is "Not a GeoParquet file" to gpio, and
  tylertoo warns twice that it assumes WGS84.
- **CRS.** `reproject.rs` reprojects any CRS with an EPSG code to 4326 as it
  reads; no EPSG code → `UnsupportedCrs`. An EPSG:32738 copy of the fixture
  with the lon/lat `--bbox` gave the same 943 features.

### Keep

- Both tutorials, their structure, Madagascar steps 2–5, and both Brazil
  scripts. The only changes to those steps are the input file name and the
  refreshed numbers. `test_doc_commands.py` checks the Brazil flags, and I
  checked them against `--help`.
- The current mechanics: scripts as the source, the verbatim and
  output-match tests, the PR `docs` job (`ci.yml`, no path filter, so
  Madagascar runs on every PR), and the nightly Brazil run.
- The raw fixture `tests/fixtures/realdata/fieldmaps-madagascar-adm4.parquet`.
  About 20 Rust tests and benchmarks read it. Only the docs tests switch.
- Facts from the deleted guides that Brazil already covers and must keep: an
  `s3://` prefix ending in `/` is one dataset; `AWS_ENDPOINT_URL` and the
  anonymous fallback; quoted columns in `--filter`; row-group pruning (875 of
  879); temporary disk and `TMPDIR`; shard-plan → coarse `--save-plan` →
  shards `--plan` → `merge`; one shared `OPTIONS` set (the plan fingerprint
  refuses a mismatch); a cell band ends below the pivot; #854.
- Add two facts, one sentence each. In Brazil step 2: "A shard with no rows
  writes an empty archive and exits 0; `merge` skips empty archives and
  refuses archives whose tiles overlap." In "Before you begin": `--spill-dir`
  as an alternative to `TMPDIR`.
- The rest of both guides (signed URLs, `https://`/`gs://`, `--files-from`,
  Hive keys, filter grammar, `--plan-only`, plan mismatches, Slurm, memory and
  thread settings, export waves) is reference material, not tutorial material.

### Cut

- `examples/madagascar-boundaries/01-prepare.sh` (curl, DuckDB WKB round
  trip, gpio repack). The work moves into the fixture recipe.
- Madagascar README:25–83 ("1. Prepare the input"), replaced by a check step.
- The `duckdb` tools row (README:20). After the cut, no docs test runs
  DuckDB, so drop `duckdb-cli` and `duckdb` from the `docs` group in
  `crates/python/pyproject.toml` and run `uv lock`. First grep
  `crates/python/tests/docs` to confirm nothing imports `duckdb`.
- The Brazil "Next steps" links to `guides/remote-reads/` and
  `guides/scaling/` (README:231–236).

### Auto-generate

Nothing new. The scripts are the single source, and the output blocks are
curated subsets, so a generator would have to guess which lines to keep. A
failing assertion prints the full real output to copy from.

### CI test

1. **Include-aware verbatim check.** Add `rendered_source(example)` to
   `doc_blocks.py`. It finds the `docs/tutorials/*.md` page that includes
   `examples/<name>/README.md`. For each `--8<-- "…README.md[:section]"` line,
   it takes either the whole README or the text between
   `<!-- --8<-- [start:section] -->` and `[end:section]`.
   `test_tutorial_shows_step_verbatim` then checks `fence_for(step)` against
   that text, not the raw README. Today it passes. It fails if a script moves
   outside an included section.
2. **Step 1 is the check.** `01-check.sh` runs `gpio check all --strict`, so a
   fixture that stops passing fails the PR job.
3. **Loud nightly.** Add a final step to `tutorials.yml`, with
   `if: failure()` and `permissions: issues: write`. It opens the issue
   "Nightly Brazil tutorial failed", labeled `bug` and `P2`, whose body starts
   with an `Impact:` line and links the run. If that issue is already open,
   the step comments on it instead (find it with
   `gh issue list --state open --search 'in:title "Nightly Brazil tutorial failed"'`).

### How it stays accurate

- Madagascar runs on every PR, so its drift is fixed in the PR that causes
  it. Brazil runs nightly; with CI item 3 a drift opens an issue next morning.
- Every number in tutorial prose either appears in the following `text`
  block or names the machine it was measured on. A `text` line may be any
  substring of an output line, so log numbers can be tested as fragments
  (`attribute filter: reading 875/879 input row groups`).
- The sha pin plus `scripts/ci/verify_fixtures.sh` guard the fixture. Both
  READMEs pass `scripts/lint-prose.sh` today; `docs/tutorials/brazil.md` is
  excluded from it but holds prose (work item 12).

### Work list for the writer

1. **Make the fixture**, in a scratch directory holding the raw fixture:
   ```bash
   uvx --from duckdb-cli==1.5.6 duckdb -c "LOAD spatial; COPY (
     SELECT * EXCLUDE (geometry_bbox) REPLACE (ST_GeomFromWKB(geometry) AS geometry)
     FROM 'fieldmaps-madagascar-adm4.parquet') TO 'raw.parquet' (FORMAT parquet);"
   uvx --from geoparquet-io==1.7.0 gpio convert geoparquet raw.parquet fieldmaps-madagascar-adm4-hilbert.parquet
   uvx --from geoparquet-io==1.7.0 gpio check all fieldmaps-madagascar-adm4-hilbert.parquet --strict
   ```
   gpio's default row-group size gives one group, which passes `--strict`.
   `EXCLUDE (geometry_bbox)` drops the export's FLOAT bbox struct, which
   would sit beside gpio's `bbox`. The output is deterministic: two runs gave
   18,129,353 bytes, sha256
   `48e412554a84312fc17f702c3a330b9de0f037d58a9852c093970925d525d064`. The
   name follows tylertoo's `<stem>-hilbert.parquet` hint (`diagnostics.rs:943`).
2. **Publish (maintainer)** with `gh release upload fixtures-v1 fieldmaps-madagascar-adm4-hilbert.parquet`,
   before the PR's CI runs. Commit the same file at `tests/fixtures/realdata/`.
   `.gitattributes` already sends it to LFS, and `NOTICE:78`
   (`fieldmaps-*.parquet`) already covers its license.
3. **Pin it.** Add the sha line to `benchmarks/fixtures.sha256` in
   alphabetical order. Add the name to `RELEASE_ASSETS`
   (`scripts/ci/tests/test_verify_fixtures.py:13`). Add a table row and the
   recipe from item 1 to `tests/fixtures/realdata/README.md`, in the style of
   the FTW Moldova recipe.
4. **Docs tests.** In `crates/python/tests/docs/doc_blocks.py:16`, set
   `MADAGASCAR = "fieldmaps-madagascar-adm4-hilbert.parquet"`. `conftest.py`,
   `stage_input`, and `test_readme.py` follow from that constant. In
   `.github/workflows/tutorials.yml`, update `--pattern` at :76 and the duckdb
   comment at :83–84.
5. **Replace step 1.** `git rm examples/madagascar-boundaries/01-prepare.sh`,
   then add `examples/madagascar-boundaries/01-check.sh`:
   ```bash
   #!/usr/bin/env bash
   set -euo pipefail

   # Download the input once: 17,465 admin-4 boundary polygons (17 MB),
   # already GeoParquet, Hilbert-sorted, and ZSTD-compressed.
   SRC=fieldmaps-madagascar-adm4-hilbert.parquet
   if [ ! -f "$SRC" ]; then
     curl -fsSLO "https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/$SRC"
   fi

   # Check compression, bbox covering, row groups, and spatial order.
   # --strict fails on warnings too.
   gpio check all "$SRC" --strict
   ```
6. **Write README section "## 1. Check the input"** in place of :25–83. It
   holds this prose, then the `01-check.sh` fence, then the `text` block:
   > tylertoo reads GeoParquet in any coordinate reference system (CRS) that
   > has an EPSG code, and reprojects it to WGS84 as it reads. It keeps the
   > input's row order, so the rows must be sorted by location; tylertoo warns
   > when they are not. This tutorial's input is already prepared. To prepare
   > your own file, run `gpio convert geoparquet input.parquet output.parquet`,
   > which sorts rows in Hilbert order, adds a bbox column, and compresses with
   > ZSTD in one pass.
   ```text
   Number of row groups: 1
   ...
   ✓ Found bbox column 'bbox' with proper metadata covering
   ...
   ✓ ZSTD compression on geometry column 'geometry'
   ...
   ✓ Data appears to be spatially ordered
   ...
     ✓ 30 checks passed
   ```
7. **Fix the rest of `examples/madagascar-boundaries/README.md`:**
   - :1 "from a raw export to PMTiles" → "from GeoParquet to PMTiles".
     :3 "(28 MB)" → "(17 MB)".
   - :19–20: gpio's "Used for" becomes "Checking the input". Delete the
     duckdb row.
   - :87–89: one row group means nothing is skipped here. Write "`--bbox`
     keeps features whose bounding boxes intersect the box, and skips row
     groups outside it." Add "Write `--bbox=` with an equals sign, so a
     negative first coordinate is not read as a flag."
   - `prepared.parquet` → `fieldmaps-madagascar-adm4-hilbert.parquet`: in
     `02-preview.sh`, in `03-overview.sh`, and in README:99, :150, :282. The
     README fences must match the scripts byte for byte.
   - :105–120 and :157–169: paste the new header line and `bytes` column from
     a rerun. Counts stay the same (verified).
   - :180–182: Brazil does not query with DuckDB. Write "Any Parquet reader,
     such as DuckDB, can open the file." and drop the link.
   - :284–286: Brazil uses no `--files-from`, `--bbox`, or `pyramid`. Write
     "The Brazil tutorial tiles a whole country from a cloud bucket with
     `--filter`, draws density cells with `--band`, and splits the build into
     shard jobs."
8. **Run the docs tests** from `crates/python`:
   `uv sync --group docs && TYLERTOO_BIN=$PWD/../../target/release/tylertoo uv run --no-sync pytest -m docs tests/docs -v`.
9. **Refresh Brazil** (about 60 runner-minutes a try, so batch the fixes). Trigger the nightly (`gh workflow run tutorials.yml
   --ref <branch>`), or reuse run 45979409335 if no core change has landed
   since. Paste its real lines into the `text` blocks. That run printed:
   - step 1: `850,534 tiles across z0..z14`; no "still over the size limit"
     line; `2 tile(s) at z8 written at reduced extent; smallest extent 2048`;
     a new stats table.
   - step 2: coarse `1,975`; shards `334,182`, `271,771`, `109,790`,
     `132,816`; merge `850,534 tiles (850,109 unique)`; z10 line
     `10    4,704  234,519,671`. The shard-plan lines are unchanged.

   Then fix the prose in `examples/brazil-fields/README.md`:
   - :114 and :211: 857,315 → the new count.
   - :116: the intermediate file was 12.33 GiB, not 7.3. Show it as the
     fragment `(12.33 GiB, removed after export`.
   - :121–124: #873 is closed. No tile stays over the cap; two z8 tiles are
     written at extent 2048. Delete the #873 sentence.
   - :92 shows `174.16s` but :115 says 4 min 19 s; the runner took 1051.79 s.
     State durations only with the machine named.
   - :26–29: the nightly runner is the 4-core, 16 GB machine, and step 1 took
     17.5 min there, not about 20. Re-measure the 12 GB peak, the 21 GB of
     disk, the 3.3 GB archive, and "byte-identical to the 64-core run" with
     `/usr/bin/time -v` and `ls -l` in one manual run, or delete them.
   - :24 and :41–42 "downloads all 11.6 GB once": the run staged
     11,598,571,638 B plus 1,241,731,353 B of pass-1 columns (~12.8 GB). Each
     shard downloads its own subset again (5.2–11.6 GB). Correct both
     passages, and show `attribute filter: reading 875/879 input row groups`
     in step 1's block.
   - :9–14 and :118–119: check each number once (45 files, 55,500,503 rows,
     4,173,713 fields, 103.3 Mha, 185,261 cells, 129,558 at z7). Pass 2
     logged `55500495 input rows`, 8 fewer than the prose. Count rows over the
     prefix with DuckDB, then sum `point_count` and `metrics:area` in
     `tylertoo decode brazil.pmtiles z7.parquet --zoom 7`.
   - :206–216: the 64-core timings and "all but 4 byte-identical" need one
     64-core rerun with a per-zoom `tylertoo decode` comparison. Otherwise
     write only "a few z10 tiles at the cap differ (#854)".
   - :231–242: replace the guide links with `reference/cli/` and keep the
     tuning and Python bullets.
10. **Add the two facts** from Keep (empty shards, `--spill-dir`).
11. **Implement CI test items 1 and 3**, with a unit test for `rendered_source`.
12. **Vale.** Drop `':(exclude,glob)docs/tutorials/*.md'` from
    `scripts/lint-prose.sh` and lint both tutorial pages. If raw HTML trips
    Vale, keep the exclusion and move "The result" prose into a `result`
    snippet section of the Brazil README. In `docs/tutorials/brazil.md`,
    "Zoom past z8" → "Zoom to z8 or closer", because fields start at z8.
13. **Finish:** lint every changed file, rerun item 8, and run `test_verify_fixtures.py`.

### Open risks

- **Maintainer decisions:** upload to `fixtures-v1` or cut a new release?
  Should the README Quickstart (maintainer voice, Vale-excluded) switch to the
  new file? It feeds the raw file today, which prints two WGS84-assumption
  warnings, and `test_readme.py` shares the `MADAGASCAR` constant. I
  recommend switching, then copying the README to `crates/{cli,core,python}/`.
- With one row group, the Madagascar preview shows no row-group skipping
  (Brazil does). Smaller groups would show it but fail `--strict`.
- The Brazil map loads a hand-uploaded Source Cooperative `brazil.pmtiles` of
  unknown version. Re-upload it from the refreshed run.
- Readers with AWS credentials sign their requests to Source Cooperative; the
  error then hints `AWS_SKIP_SIGNATURE=true` (`input.rs:2147`). Untested.
- Deleting `scaling.md` breaks 13 anchor links in Rust help and docstrings
  (`#spill-files`, `#memory-profiles`, `#export-waves`, `#worker-threads`,
  `#memory-limit`, `#read-concurrency`, `#sharded-builds`,
  `#how-streaming-bounds-memory`), which the generated references render.
  `test_absolute_site_links_resolve` catches them; the reference section must
  re-home them.
## 2. Benchmarks

One manifest-driven suite runs in two tiers. The **nightly** tier runs the small cells on GitHub-hosted `ubuntu-latest`. It is a regression gate only and never feeds the page. The **release** tier is one command, `benchmarks/release.sh`, that the maintainer runs by hand on the 64-core dev box for each release. It is the only writer of `benchmarks/results/*.json`, the comparison PMTiles and `docs/benchmarks.md`, and its output is committed in the release PR. The PR gate stays `scripts/gen_bench_docs.py --check`. There is no self-hosted runner and no AWS setup.

Facts checked on `origin/main` (f52847c1):

- `Cargo.toml` is 0.8.0 and the newest tag is `v0.8.0`, so every result labelled "0.8.0" ran on post-release `main`.
- `run_e2e.py` has no load gate.
- `release.yml` ships no aarch64 Linux binary, so the box benchmarks `cargo build --locked --release -p tylertoo` (glibc; the footer prints the allocator).
- Moldova's 13 parts and the Overture cell inputs have no `url`: they exist only on the box.

### Keep

- `benchmarks/run_manifest.py` + `manifest.toml`: the one harness. It already has interleaved `--bin` rounds, the load gate, per-run resume, caps, `host_class()` and `report --json-out`.
- `benchmarks/e2e/`: `run_e2e.py` (corpus tier against tippecanoe, plus the `--invariants` mode `e2e.yml` uses), `setup_tippecanoe.sh`, `compare_tippecanoe.py` + `tippecanoe_tolerances.toml` (PR parity), and `README.md` (method and settings parity, trimmed).
- `benchmarks/showcase/`: `showcase.py`, both manifests, `tippecanoe_shim.sh`, the parts TSV, `test_showcase.py`.
- `valve_guard.py` (github leg, moved) and `baseline/valve_guard.json` (for manual box runs, documented in `context/PROFILING.md`).
- From `benchmarks/overview/`, only `ci_guard.py`, `ci_baseline.json` (`e2e.yml`) and `logging_server.py` (`serve` cells).
- Workflows unchanged: `codspeed.yml`, `tippecanoe-compare.yml`, `e2e.yml`, and the labelled-PR plus dispatch legs of `bench-regression.yml`.
- `gen_bench_docs.py`: the marker contract, `footer()`, `readme-claim`, `stream-claim`, `--check`.
- Re-measured each release: `e2e-corpus.json`, `overture-buildings-africa.json`, `overture-buildings-brazil.json`.
- The Africa material: both profiles, the byte-identical archives, the bbox caveat. All of it is now generated.

### Cut

- Pinned legs, dead code with no runner: `criterion-regression-pinned` and its 01:00 schedule in `bench-regression.yml`, the `pinned` leg of `valve-guard.yml`, and every `vars.TYLERTOO_BENCH_RUNNER` reference. Also the Monday schedule of `bench-regression.yml`, which the nightly tier supersedes.
- `benchmarks/e2e/run_corpus.sh`, which becomes a stage of `release.sh`.
- `benchmarks/e2e/format_results.py`, a shim to deleted renderers.
- `benchmarks/e2e/RESULTS.md`. Its 22 regions render the fixtures run and per-zoom tables; the JSON keeps the full record.
- Results files:
  - `e2e-fixtures.json`, labelled 0.11.0 and used only by RESULTS.md.
  - `results-table.json` and `tickets-772-732.json`, rendered nowhere, plus `benchmarks/tickets/remeasure_772_732.sh`.
  - `memory-profiles.json`, folded into `release-cells.json`.
  - `brazil-ftw-2025.json`, whose only consumer, the scaling guide, is being deleted.
- Renderers left without a region: `headline`, `quality-matched`, `memory`, `input`, `phases`, `flags`, `provenance`, `per-zoom`, `variance`, `overture-scaling`, `brazil-table`, `fast-disk`, `memory-profiles`, `bounded-cost`, `max-memory-ratio`, `stream-at-scale`, `vida-head-to-head`, and `SECTIONS`.
- The rest of `benchmarks/overview/`. Move `benchmarks/overview/PROFILE.md`, `coalesce/RESULTS.md` and `demo/RESULTS.md` with `git mv` into `context/archive/bench/` as frozen history, and fix their links (`context/PROFILING.md` L5, `DEVELOPMENT.md` L279, `demo/README.md`).
- `showcase/README.md`: the runbook (steps 1–5) and the hard-coded `/home/ubuntu/tylertoo-track-666/...` path go. Keep the file table and one line, "run `benchmarks/release.sh brazil africa`". The default `TIPPECANOE` in `showcase.py` becomes `.tools/tippecanoe-2.79.0/bin/tippecanoe`.
- From `docs/benchmarks.md`: every hand-typed number, the fast-disk table, "What the numbers don't show" (the tippecanoe guide owns it), and the Methods prose (now generated).

### Auto-generate

Every number, version, date, host, size or command on the page comes from a `<!-- bench:... -->` region. `release.sh` writes these files, each with the existing `provenance` block:

| file | writer | regions |
|---|---|---|
| `e2e-corpus.json` | `run_e2e.py --corpus --quality-matched --keep --repeat 3` | `stamp`, `speedup`, `speedup-bars`, `memory-bars`, `method`, `readme-claim` |
| `release-cells.json` | `run_manifest.py report --json-out`, set `release` | `encodings`, `sources`, `profiles` |
| `overture-buildings-brazil.json` | `showcase.py collect` | `bars` |
| `overture-buildings-africa.json` | `showcase.py collect-stream --history-from <old file>` | `hero`, `history`, `recipe`, `stream-claim` |
| `comparison-pmtiles.json` | `release.sh upload` | the tippecanoe guide (other strategy) |

Renderer contents. Each chart is bars, then a `<details>` Markdown table, then `footer()`:

- `stamp`: version, commit, date, host, and the tippecanoe and gpio versions. It also names any page file at another `tylertoo_version` ("Africa: measured at 0.8.0"), so the renderer receives all results.
- `speedup`: "tylertoo is **N×** faster than tippecanoe end to end, the geometric mean over K datasets", plus the generated footnotes.
- `speedup-bars`: per dataset, tylertoo against tippecanoe end to end. The tylertoo label reads "3.7 s, 23× faster". Table columns: features, tippecanoe tile only, speedup with thinning off, archive sizes.
- `hero`: large text for features, wall, peak memory, and bytes fetched of the release's bytes. A `--profile bounded` line, and "same archive" from the sha256s.
- `history`: one bar per Africa run, labelled with version or `git describe`. The lead line reads "54 min at main afb6b15 → X min at 0.9.0, N× faster". An entry on a different Overture release is flagged.
- `recipe`: a `bash` block with the `env` and argv recorded in the `ovt-s3` row, with `{out}` set to `africa.pmtiles`. It shows what ran.
- Brazil `bars`: tylertoo from `s3://`; tylertoo on a local extract, which includes the extract; tippecanoe as extract, FlatGeobuf and tile. The speedup is against the `s3://` row.
- `encodings`, `sources`, `profiles`: see the matrix below. `profiles` is auto against bounded on Madagascar, Moldova and NZ, wall and peak.
- `memory-bars`: peak memory per corpus dataset, both tools.
- `method`: a bullet list of host, warm-ups, rounds, median, alternation, warm cache, each tool's flags, input pinning and allocator.

**Bars.** One `bars(title, series, rows, unit)` helper, about 40 lines, emits raw HTML. `md_in_html` and `attr_list` are already on in `mkdocs.yml`:

```html
<figure class="tt-chart"><figcaption>Wall time end to end, shorter is faster</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">tylertoo</span> <span class="tt-key tt-b">tippecanoe</span></p>
<div class="tt-row"><span class="tt-label">Portland buildings, polygons</span>
<span class="tt-bar tt-a" style="--w:4.3%" aria-hidden="true"></span><span class="tt-val">3.7 s, 23× faster</span>
<span class="tt-bar tt-b" style="--w:100%" aria-hidden="true"></span><span class="tt-val">84.8 s</span></div></figure>
<details markdown><summary>The figures as a table</summary>

| dataset | tylertoo | tippecanoe | speedup |
</details>
```

- The scale is linear from zero, and the chart maximum is 100%.
- tylertoo comes first in the accent color (`--md-accent-fg-color`). The other bar uses `--md-default-fg-color--lighter`, which suits both schemes. The legend and the fixed order carry identity, so color never does alone.
- Values are visible text, bars are `aria-hidden`, and the table repeats everything in Markdown.
- About 30 lines of CSS go in the existing `docs/stylesheets/extra.css`: a `.tt-row` grid that drops to one column below 600 px, with width `calc((100% - 9rem) * var(--w))`, plus a `.tt-hero` class.

**The headline number.** Per corpus dataset, `s_d = median(tippecanoe_e2e) / median(tylertoo default)` of whole-run wall. `tippecanoe_e2e` is the GeoParquet to FlatGeobuf conversion (the faster of gpio and ogr2ogr) plus tippecanoe's tile run. Both tools run:

- on the same host, in one session, in alternating order
- with one discarded warm-up, then three rounds

`N = geomean(s_d)`, an integer when N ≥ 10 and one decimal below that.

Only datasets whose two archives reach the same max zoom count, read from the per-zoom tile counts. This drops NYC points today, where tippecanoe's recorded `--extend-zooms-if-still-dropping` builds z15–16.

The generated footnote states:

- the included and excluded datasets, and why
- the conversion's share of tippecanoe's wall
- the tile-only geomean, against `tippecanoe_fgb`
- the thinning-off geomean

`readme-claim` calls the same `speedup_geomean()`, so the README and the page cannot disagree. Brazil shows its own N× and stays out of the geomean.

**Input-type matrix.** FTW Moldova (632k polygons), `tiles` defaults, z0–12. The variants come once from gpio 1.7.0 and are pinned by sha256:

| cell | input | produced with |
|---|---|---|
| `polygons` (exists) | GeoParquet 1.1, WKB | `moldova-hilbert.parquet` as published |
| `in-geoarrow` | GeoParquet 1.1, native GeoArrow | `gpio convert geoparquet moldova-hilbert.parquet out.parquet --geoparquet-version 1.1-geoarrow` |
| `in-geo2` | GeoParquet 2.0, Parquet GEOMETRY | same, `--geoparquet-version 2.0` |
| `in-geo-only` | Parquet GEOMETRY, no `geo` metadata | same, `--geoparquet-version parquet-geo-only` |
| `in-parts13` | 13 local files as one dataset | `--files-from {files_from}` over `moldova-part-000`…`012` |
| `in-http13` | the same 13 files over HTTP | `tiles`, `serve = { ttfb_ms = 100 }` |

`encodings` charts the first four. `sources` charts `polygons`, `in-parts13` and `in-http13`, and points to Africa for `s3://`. gpio writes neither WKT (`geoarrow.wkt`) nor GEOGRAPHY, so those stay uncharted (maintainer decision).

**Hand-written prose cap:** 40 non-blank lines outside regions, code and comments.

### CI test

These run in the existing `ci.yml` "Gate scripts" job and its pytest step:

1. `gen_bench_docs.py --check` keeps failing on any stale region, and gains these rules:
   - Every `DOC_FILES` entry must exist and hold a marker; today a missing file is skipped silently. The list becomes `README.md`, `crates/{core,cli,python}/README.md`, `docs/benchmarks.md`, plus what the guides strategy adds.
   - Every `benchmarks/results/*.json` must be rendered by some region. The only named exception is `comparison-pmtiles.json`, when no marker uses it.
   - On `docs/benchmarks.md`, strip regions, fenced and inline code, HTML comments, link targets `](...)` and the token `S3`. No ASCII digit may remain, and at most 40 non-blank lines.
   - Warnings only, exit 0, as `::warning::`: a page file whose `tylertoo_version` differs from `Cargo.toml` ("measured at X; this branch is Y; run benchmarks/release.sh"), or mixed versions on the page.
2. `scripts/ci/tests/test_gen_bench_docs.py` updates the L24 assertion and covers bar widths, the geomean and max-zoom eligibility, the history line, the recipe from the recorded argv, a planted digit, an orphan JSON, and the line cap.
3. `benchmarks/test_run_manifest.py` checks:
   - the `nightly` and `release` sets resolve
   - every `nightly` input has a `url`
   - `gate` passes and fails on the ratio, and applies the baseline only on a matching host class
4. The `prose` job already lints `docs/benchmarks.md`, generated text included. A renderer string that breaks Google style fails the PR that regenerates the page. The `<details>` tables hold all generated text as plain Markdown, in case Vale skips raw HTML.
5. The strict Zensical build in `ci.yml`'s docs job renders the page.

### How it stays accurate

- **One writer.** `release.sh` writes `benchmarks/results/` only for a release candidate:
  - the tree is clean, except results, `docs/` and the READMEs
  - and either no `v$VERSION` tag exists yet (the release branch after `cz bump` and `git tag -d`) or `HEAD` is at that tag

  Any other run writes only to `$DATA/release/<git describe>/`, and the `upload` and `docs` stages refuse to run. That ends the "0.8.0 from later main" labels.
- **Provenance.** The `commit` (short SHA), `tylertoo_version` (the binary's `--version`), `git_describe`, `date`, `host` and `host_class`. The stamp and every footer print them.
- **No silent staleness.** All stages use one binary. A file left at an older version is named in the stamp and warned about by `--check`.
- **A reminder that does not block.** The version warning fires on the release PR when Cargo.toml is bumped but the results are not. The `release.sh` step goes into CONTRIBUTING.md and `.claude/skills/release/SKILL.md`.
- **Before → after.** `gen_bench_docs.py --diff <ref>` prints whole-run wall before → after for every charted row against `git show <ref>:benchmarks/results/<f>`. It ranks rows by wall change, skips rows on another `host_class`, and has no phase or CPU columns. `release.sh docs` prints it against `origin/main` for the PR body.
- **Overture pinning.** The manifests keep the release id. A new `showcase.py check-pins` HEADs the first and last TSV parts anonymously and compares their ETags. When a release is gone, the `brazil` or `africa` stage stops with "run `showcase.py pin <release>`". That new subcommand uses the `regions` footer reader to rewrite the TSV, the manifests' prefix and sha256, and `RELEASE`, which is read from the manifest instead of a constant. The page prints the Overture release id. Bump only when forced. Corpus extracts stay cached in `corpus/data/`, and `GPQ_OVERTURE_RELEASE` exists for a re-fetch.
- **Brazil tippecanoe reuse.** About 90 minutes of tippecanoe does not change with tylertoo, so `release.sh` copies the previous row when the tippecanoe version, input sha256 and `host_class` match. It stamps `reused_from: {commit, date}`, which the footnote prints. `FORCE_TIPPECANOE=1` re-runs it. The corpus always re-runs both tools, interleaved.

### Suite and workflow design

**Sets** (append to `[sets]` in `benchmarks/manifest.toml`):

```toml
nightly = ["tiles-madagascar", "verbatim", "polygons", "coverage-fill", "resume",
           "export-moldova", "in-geoarrow", "in-geo2", "in-parts13", "in-http13"]
release = ["tiles-madagascar", "polygons", "tiles-nz", "tiles-madagascar-bounded",
           "polygons-bounded", "tiles-nz-bounded", "in-geoarrow", "in-geo2",
           "in-geo-only", "in-parts13", "in-http13"]
```

- `*-bounded` cells are their twins plus `flags = ["--profile", "bounded"]`.
- Nightly inputs need a `url`. Upload the Moldova variants and parts as assets of a new GitHub release, `bench-inputs-v2`.
- Other sets stay for ticket A/B work.

**Nightly tier.** `git mv .github/workflows/valve-guard.yml bench-nightly.yml`, named "Bench nightly".

- Triggers: `schedule: '30 5 * * *'` and `workflow_dispatch` with a boolean input `record_baseline`.
- Concurrency: `{group: bench-nightly, cancel-in-progress: false}`.
- Job `valve-guard`: the existing steps on the github host only, with no matrix.
- Job `suite`: `ubuntu-latest`, `timeout-minutes: 120`, `contents: read` and `issues: write`. Steps:
  1. Checkout with `fetch-depth: 0`. Set `BASE=$(git describe --tags --abbrev=0 --match 'v*')`, and skip with a notice when `HEAD` is that tag.
  2. Restore the base binary from the cache key `nightly-base-${BASE}-${runner.arch}`. On a miss, run `git worktree add ../base $BASE`, build there with `--target-dir ../base-target`, and save the binary to the cache.
  3. Build the head binary.
  4. Restore the inputs cache, keyed on the nightly inputs' sha256 values, then run `run_manifest.py fetch`.
  5. Run `run_manifest.py run --set nightly --name nightly --pairs 3 --bin base=../base-target/release/tylertoo --bin head=target/release/tylertoo`.
  6. Run `run_manifest.py gate --name nightly --wall-max 1.25 --rss-max 1.15 --baseline benchmarks/baseline/nightly.json`.
  7. Write the gate table to `$GITHUB_STEP_SUMMARY`. When the gate fails, open or update the issue "Nightly benchmark regression" with valve-guard's issue step. With `record_baseline`, upload the result of `gate --record-baseline` as an artifact for the maintainer to commit.
- New `run_manifest.py gate`, about 60 lines:
  - It takes the head and base median wall and peak RSS for each cell.
  - It fails when head/base is over `--wall-max` or `--rss-max`.
  - When the runner's host class (`host_class_key`, moved here from `valve_guard.py`) has a baseline entry, it also fails at more than 1.40× the baseline wall or 1.20× the baseline RSS.
  - It ranks its before → after output by wall ratio.
  - `nightly.json` starts as `{"hosts": {}}`, so the baseline check stays off until a baseline is recorded. It runs only on nights whose GitHub-hosted CPU model matches.
- **Catches:** a regression of 25% or more in wall, or 15% or more in peak memory, on 4 x86 cores, against the last release. It covers the single-file, multi-file, HTTP, encoding, export and resume paths.
- **Misses:** regressions that show only at 64 cores (column-split decode, core use), at large inputs (spill, Brazil, Africa), on `s3://`, or on aarch64. Those surface only in `release.sh`'s `--diff` table.
- **Wall time:** 40 to 55 minutes a night. That is about 15 minutes to build the head, plus about 15 minutes to build the base once after each release, then about 25 minutes for 10 cells of 7 runs at 20–40 s each.

**Release tier.** `benchmarks/release.sh` is about 150 lines of bash. It replaces `run_corpus.sh` and the showcase runbook.

```
benchmarks/release.sh [--data ~/bench-data] [--force] [STAGE ...]
#   stages, in order: build tippecanoe corpus cells brazil africa upload docs
SKIP="africa brazil" benchmarks/release.sh   # a pre-tag dry run, about 1 h
```

- `flock $DATA/release.lock` gives the box one run queue.
- Preflight checks:
  - the release-candidate check
  - tippecanoe is v2.79.0 and gpio is 1.7.0
  - at least 250 GB of free disk when `africa` is staged; bounded Africa peaked at 195 GB
  - `showcase.py check-pins`
  - when `upload` is staged, `aws s3 ls s3://nlebovits/tylertoo-benchmarks/ --profile source-coop`. The profile's `credential_process` (`source-coop creds`) refreshes expired cached credentials when it can. Otherwise preflight stops with "run `source-coop login`" before any timed run. The maintainer is at the box, so the interactive login is fine.
- Resume: each stage writes `$DATA/release/$VERSION/<stage>.done` and is skipped when that file exists, unless `--force`. Inside a stage, `run_manifest.py` resumes per run, so a failed Africa run re-runs only Africa.
- Load gate: every timed run waits on `run_manifest`'s load gate. Add an `rm.Gate` wait to `run_e2e.py`.

| stage | runs | box wall |
|---|---|---|
| build | `cargo build --locked --release -p tylertoo` | 3 min |
| tippecanoe | `benchmarks/e2e/setup_tippecanoe.sh` | 0–10 min |
| corpus | `corpus/fetch.sh && corpus/optimize.sh` (cached), then `run_e2e.py` on 4 datasets | ~40 min |
| cells | `run_manifest.py run --set release` + `report --json-out` | ~20 min |
| brazil | showcase `ovt-s3` + `ovt-local` once each, tippecanoe via the shim unless reused, `collect` | ~2 h first, ~35 min reused |
| africa | `manifest-africa.toml` `ovt-s3` + `ovt-s3-bounded`, `collect-stream --history-from` | ~1.5 h (54 + 50 min at afb6b15) |
| upload | comparison PMTiles + manifest | ~5 min |
| docs | copy JSONs into `benchmarks/results/`, `gen_bench_docs.py`, `--diff origin/main` | 1 min |

The whole run takes about 4.5 h the first time and about 3 h after that.

**Comparison PMTiles.** The four corpus datasets, both tools, from the corpus stage's `--keep` archives:

```
s3://nlebovits/tylertoo-benchmarks/<version>/<dataset-id>/{tylertoo,tippecanoe}.pmtiles
s3://nlebovits/tylertoo-benchmarks/<version>/comparison-pmtiles.json
public: https://data.source.coop/nlebovits/tylertoo-benchmarks/<version>/...
```

The stage uploads with `aws s3 cp --profile source-coop` and refuses an existing `<version>/` prefix without `--force`. It verifies each object with `curl -sfI`: status 200 and a `content-length` equal to the bytes. The repo copy is **`benchmarks/results/comparison-pmtiles.json`**, the fixed path the guides strategy reads:

```json
{"provenance": {"...": "the standard block"}, "version": "0.9.0",
 "base_url": "https://data.source.coop/nlebovits/tylertoo-benchmarks/0.9.0/",
 "datasets": [{"id": "polygons-portland-medium", "label": "Portland buildings",
   "geometry": "polygon", "features": 813456,
   "tylertoo":   {"url": "...", "bytes": 0, "sha256": "...", "min_zoom": 0, "max_zoom": 14, "argv": []},
   "tippecanoe": {"url": "...", "bytes": 0, "sha256": "...", "min_zoom": 0, "max_zoom": 14, "argv": []}}]}
```

**The release procedure.** CONTRIBUTING.md's "Release workflow" gets a step after "Check the build":

1. Run `benchmarks/release.sh` on the box.
2. Commit `benchmarks/results/`, `docs/benchmarks.md` and the READMEs (the pre-commit hook copies README).
3. Paste the `--diff` table into the PR.

CI only warns, so nothing blocks.

### Page layout

`docs/benchmarks.md`, in order:

1. `# Benchmarks`: one sentence of intro, then `bench:e2e-corpus/stamp`.
2. `## Against tippecanoe`: `speedup`, then `speedup-bars`. One sentence links to [Coming from tippecanoe](guides/tippecanoe.md) for the map comparison.
3. `## Streaming Overture from S3`: the Africa `hero`, `history` and `recipe`, displayed prominently right below the headline. One sentence: "The box is a rectangle, so it also covers parts of the Middle East and southern Europe."
4. `## At scale: Brazil`: `overture-buildings-brazil/bars`.
5. `## Input types`: `release-cells/encodings`, then `release-cells/sources`.
6. `## Memory`: `memory-bars`, then `release-cells/profiles`. One sentence: "`bounded` trades wall time for a lower peak. Use it when the input is larger than memory."
7. `## How these were measured`: `method`, links to `benchmarks/e2e/README.md` and the results JSONs, and the line "Reproduce: `benchmarks/release.sh`".

### Work list for the writer

**[box]** marks a step that needs the 64-core dev box.

1. `scripts/gen_bench_docs.py`:
   - Add `bars()`, the new renderers, `speedup_geomean()` (shared with `readme-claim`), `--diff`, and the `--check` rules.
   - Delete the cut renderers and `SECTIONS`, and set `DOC_FILES`.
   - Tests go in `scripts/ci/tests/test_gen_bench_docs.py`, with fixtures in `scripts/ci/tests/fixtures/bench/`.
2. `docs/stylesheets/extra.css`: the `.tt-*` rules, using theme variables only.
3. Rewrite `docs/benchmarks.md` to the layout, with 40 hand-written lines or fewer. Render it from the current JSONs as a stopgap, with the stamp naming their versions. Run `scripts/lint-prose.sh docs/benchmarks.md` and the strict build.
4. `benchmarks/run_manifest.py`: move `host_class_key` in from `valve_guard.py`, add `gate` (`--baseline`, `--record-baseline`), and add tests. Create `benchmarks/baseline/nightly.json` as `{"hosts": {}}`.
5. `benchmarks/manifest.toml`: add the `in-*` and `*-bounded` cells and both sets. Fix `test_run_manifest.py` L55, which uses the `results-table` set.
6. **[box]** Make the Moldova variants with gpio 1.7.0 and check that tylertoo reads each one. Upload them and the 13 parts to `bench-inputs-v2` with `gh release upload`, then pin each `url` and `sha256`. Ask the maintainer to approve the new release first.
7. `benchmarks/e2e/run_e2e.py`: add a load-gate wait before each timed run, and keep both archives under a known directory with `--keep`.
8. `benchmarks/showcase/showcase.py`:
   - Add `check-pins` and `pin`, and read `RELEASE` from the manifest.
   - Add `collect-stream --history-from`. Seed the afb6b15 entry with `git describe afb6b15`.
   - Record `env` and argv per row.
   - Set the default tippecanoe path.
   - Add tests.
9. Write `benchmarks/release.sh` and delete `benchmarks/e2e/run_corpus.sh`.
10. Workflows:
    - `git mv valve-guard.yml bench-nightly.yml`, drop its pinned leg, and add the `suite` job.
    - In `bench-regression.yml`, drop the pinned job and both schedules, and fix the header comment.
    - Grep `.github/` and confirm no `TYLERTOO_BENCH_RUNNER`, `tylertoo-bench` or `self-hosted` is left.
11. Do the deletions and moves under **Cut**. Fix every link `grep -rn` finds, including `OVERVIEW_TUNING.md` L2311 and the `scaling/#memory-profiles` links that the docstrings generate in `docs/reference/python.md`.
12. Add the release step to CONTRIBUTING.md and `.claude/skills/release/SKILL.md`.
13. **[box]** First real run: `benchmarks/release.sh` on the next release branch. Commit the results in the release PR.

### What the maintainer must do once

- Approve the `bench-inputs-v2` GitHub release (step 6).
- Before the first run, check that `source-coop creds` still refreshes, or run `source-coop login`. My access to the credential cache was denied, so how long a login lasts is unverified.
- Close the "Weekly benchmark comparison" and "Nightly pipeline benchmark (64-core host)" issues. `TYLERTOO_BENCH_RUNNER` is already unset.

### Open risks

- **Overture expiry.** Overture drops old releases from the bucket. Brazil and Africa then need `showcase.py pin`, and the history chart marks a data change. The corpus extracts survive only in the box's cache.
- **Late catches.** The nightly tier sees 4 x86 cores and small inputs. A regression that shows only at 64 cores, at scale, or on `s3://` stays on `main` until the next release run.
- **Disk speed.** The disk is not part of `host_class`. It went from 125 to 1,000 MB/s mid-way through the last runs, so the first new Africa history point mixes disks. Add the disk to the recorded `host` string.
- **Unverified rendering.** It is unverified whether Vale lints raw HTML, and whether Zensical keeps inline `style`. The `<details>` tables and the strict build cover both.
- **Run length.** A release run takes 3–4.5 h on the shared box, and load gate waits can stretch it.
- **Missing encodings.** WKT and GEOGRAPHY inputs stay uncharted until someone builds them without gpio.
## 3. Guides

Two guides remain, each running a real script in CI. Checked against
`origin/main` `f52847c1` (2026-11-27); line numbers are from that commit.

### Keep

| Page | Kept content | Hand-written prose budget |
|---|---|---|
| `docs/guides/tippecanoe.md` | Short intro, flag mapping (generated), preset table (generated), preset example (script), side-by-side output maps (generated) | **25 lines**, not counting includes, generated regions or map HTML |
| `docs/guides/attribution.md` | The three flags, what gets stored, merge and pyramid inheritance, showing the credit in MapLibre, the example (script) | **40 lines**, not counting the included example |

Keep lowercase "tippecanoe" (Vale's `Headings.yml:38` and every page use it). Keep two
headings word for word because code links to them: "The tippecanoe preset"
(`config/cli.rs:566`, `keywords.rs:558`, both docstring `.txt` files) and
"Merged and pyramid archives" (`OVERVIEW_TUNING.md:1990`).

### Cut

**Delete `docs/guides/scaling.md` (682 lines) and `docs/guides/remote-reads.md` (108 lines).**
The tutorials (Brazil: remote read, shard, merge; Madagascar: gpio prep,
`--bbox` pruning), the CLI reference or the tuning page already hold most of
their facts. The rest move or are dropped as below. "T" is the tuning page:
another agent owns it, so these rows brief that agent.

| Fact (source lines) | Destination |
|---|---|
| Brazil FTW bench table, `bench:brazil-ftw-2025/brazil-table` (scaling 7-14) | Dropped: `benchmarks.md` owns the numbers. Remove the `brazil-table` renderer and `brazil_table()` from `scripts/gen_bench_docs.py`, and `"brazil-ftw-2025"` from `scripts/ci/tests/test_gen_bench_docs.py:24`. Keep the JSON unless the benchmarks section drops it |
| Coarse-job budget: rows x 42 B x 3.3, warn vs fail, `TYLERTOO_SKIP_MEMORY_PREFLIGHT`, off-Linux, `--plan` skips it (scaling 499-544) | **T, new `### Memory sizing` under "Performance profiles"**, anchor `#memory-sizing`, the new `MEMORY_SIZING_DOC` target. Fold in `OVERVIEW_TUNING.md:2108-2121` |
| Wide schemas (`pass2.sink.bytes_per_row`, `--plan-only`); line-coalescing extra (`--coalesce-max-level-rows` x 512 B, chain at 16-29x); size from `true_peak_mib`, not `MaxRSS` (scaling 546-580) | T `#memory-sizing`, 5 lines |
| "Reading a job's MaxRSS" in full (scaling 582-682) | `context/PROFILING.md`, new `## Reading a job's MaxRSS`, moved as is. PROFILING already points to it 3 times |
| tmpfs `/tmp` is charged to the cgroup, so use real disk for `--spill-dir`/`TMPDIR`, plus the slow-volume advice from #923 (scaling 125-127, 238-247); `--memory-limit` replaces the cgroup check, so export only warns about an oversized tile (137-141); one tile too big for memory, the error and its three remedies (284-307) | T `#memory-sizing`, 9 lines |
| Interrupted exports: `.partial` checkpoint, `.partial.prev` (scaling 309-318) | T `### Paths and output replacement`, 3 lines |
| `--workers` caps the core count used for sizing (scaling 150-153) | T profiles section, 1 line. **Drop** the timing and RSS tables (155-187): one-off measurements no generator owns |
| Profiles table, mimalloc purge, cgroup probe, read concurrency, partition-wave `auto`, wave overlap (scaling 51-128, 189-216, 265-283) | Already in T ("Performance profiles", "Batch concurrency", "Read workers") or the CLI help. Drop the duplicate figures |
| Shard-plan remedies (no bbox stats: `gpio sort hilbert --add-bbox`; empty ranges; pivot z4-z8), `--plan-only`, plan-mismatch list, merge rejects duplicate ids (scaling 345-423) | T `### Shared plans for sharded builds`, at most 6 lines |
| Slurm script, "one machine is not enough" estimates, per-shard coalescing numbers (scaling 320-335, 442-469, 473-486) | Dropped: CI can't run them, and the Brazil tutorial shows the fleet pattern |
| Give every shard the same `--name`/`--attribution` (scaling 425-428) | Attribution guide, merge section, 1 line |
| Presigned S3 or SAS URLs: quoting, query redacted, TLS always verified (remote 16-27) | Clap help on `<INPUT>` (`convert.rs` `input` arg): "A presigned or SAS URL works; logs and saved plans redact its query." T already covers redaction (`OVERVIEW_TUNING.md:2584`). Drop the referrer and band-layer details |
| Walks skip `.`/`_` names; `key=value` partition keys aren't columns (remote 92-108) | Clap help on `<INPUT>`, 1 sentence. Drop the DuckDB recipe |
| Covering stats, Hilbert sort, `--bbox`/`--filter` syntax, `--bbox=` with negatives, `--files-from` (remote 38-90) | Already in Madagascar steps 1-2, T `--bbox`/`--filter`/"Expression syntax", and the CLI `--files-from` help |

**`docs/guides/tippecanoe.md` (243 lines):**
- Delete 21-67 ("Why middle zooms"), 106-110 (Python block), 137-173
  (preset prose and "Differences the preset does not remove"), and 174-243
  (both "What only ..." sections).
- Replace the tables at 68-94 and 125-135 with generated includes.

**`docs/guides/attribution.md` (143 lines):**
- Delete 59-64 and 66-68 (16 MiB, `--plan-only`; the CLI reports both),
  plus 42-47, 88-93 and 105-124 (NUL/U+2028 and other edge cases).
- Fix 31-33: Python `convert()` takes `name`, `description` and
  `attribution` (`python.md:687-689`), and nothing marks it deprecated.

### Auto-generate

**1. Tippecanoe tables, from Rust.** Add a mode to the hidden
`gen-reference-docs` command (`crates/cli/src/lib.rs:463`, `gen-docs`
feature). `gen-reference-docs tippecanoe` prints
`crates/cli/docs/tippecanoe-tables.md`, with snippet sections `flags` and
`preset`. The file sits outside `docs/`, so it never renders as a page.

- **Flag data** is `const TIPPECANOE_FLAGS: &[FlagRow]` in a new
  `crates/cli/src/tippecanoe_doc.rs`, gated on `gen-docs`. A row looks like
  `{tippecanoe: &["-z","--maximum-zoom"], command: "tiles", tylertoo: &["--max-zoom"], note: ""}`.
  The renderer panics when a tylertoo flag doesn't resolve in
  `Cli::command().find_subcommand(command)` (long name or alias), when a
  note runs over 90 characters or contains a digit (so no defaults), or when
  a row's tippecanoe side holds no `-flag` or program name. That drops
  concept-only rows, such as `tippecanoe.layer`.
- **Starting rows** (the writer checks each):
  `-z/-Z` -> `--max-zoom/--min-zoom`, `-zg` -> `--max-zoom auto`,
  `-l` -> `--layer-name`, `-L` -> `pyramid --band`,
  `-n/-N/-A` -> `--name/--description/--attribution`, `-b` -> `--tile-buffer`,
  `-y/-x/-X` -> `--include-property/--exclude-property/--exclude-all-properties`,
  `-r` -> `--drop-rate`, `-g` -> `--drop-gamma`.
  `--drop-lines`, `--drop-polygons` and `-pn` keep their names.
  `-S` -> `--simplify-factor` (note: "tile units in tippecanoe, ground sample
  distance in tylertoo"), `--no-tiny-polygon-reduction` -> `--no-collapse`,
  `-M` -> `--max-compressed-tile-size`.
  `--coalesce-densest-as-needed` -> `--size-valve merge`;
  `--drop-densest-as-needed` and `--drop-fraction-as-needed` -> `--size-valve drop`.
  `-K` -> `--cluster`; `--coalesce` -> `--no-coalesce-lines` (note:
  "tylertoo coalesces by default"); `--use-attribute-for-id` -> `--feature-id`;
  `tile-join` -> `merge`, `pyramid`; `tippecanoe-decode` -> `decode`.
- **Preset table.** The values stay in `Preset::convert` and `Preset::export`
  (`preset.rs:26-66`). Next to them, add
  `pub const TIPPECANOE_SOURCES: &[(serde_key, cli_flag, source)]` (text moved
  from the existing comments) and `pub const TIPPECANOE_VERSION`.
  - The renderer diffs `keywords::defaults(X::default())` against
    `keywords::defaults(Preset::x(Some(Tippecanoe), X::default()))` for
    `ConvertOptions` and `ExportOptions`.
  - Each changed key gives one row: flag | tylertoo default | with
    `--preset tippecanoe` | tippecanoe source. `null` shows as "no cap" or
    "unset", and integers get separators.
  - It panics unless the changed keys equal the `TIPPECANOE_SOURCES` keys.
    Unchanged values, such as drop rate 2.5, drop out.
  - The header names `TIPPECANOE_VERSION`. One unit test calls the renderer.

**2. Side-by-side maps, from `scripts/gen_bench_docs.py`.** These follow the
same release flow as every benchmark number. The release benchmark job
commits `benchmarks/results/comparison-pmtiles.json`:

```json
{"provenance": {"commit": "...", "tylertoo_version": "tylertoo 0.9.0", "host": "...", "nproc": 64, "kernel": "...", "allocator": "mimalloc",
                "tool_versions": {"tippecanoe": "v2.79.0"}, "inputs": [{"name": "...", "sha256": "..."}], "date": "..."},
 "datasets": [{"id": "points-nyc-medium", "geometry": "point", "layer": "points-nyc-medium",
   "tylertoo":   {"url": "https://data.source.coop/nlebovits/tylertoo-benchmarks/0.9.0/points-nyc-medium.tylertoo.pmtiles", "argv": ["..."]},
   "tippecanoe": {"url": "https://data.source.coop/nlebovits/tylertoo-benchmarks/0.9.0/points-nyc-medium.tippecanoe.pmtiles", "argv": ["..."]}}]}
```

- `provenance` holds every `PROVENANCE_KEYS` entry (`load_results` enforces
  this).
- `id` is an e2e-corpus id: `points-nyc-medium`, `polygons-portland-medium`,
  `lines-oregon-large` or `polygons-ftw-moldova-large`.
- `geometry` is `point`, `line` or `polygon`, and `layer` is the MVT layer.

**Renderer.** `"compare-maps": (lambda r, a: compare_maps(r), False, True)`.

- It emits a `<label>` and `<select id="compare-dataset">`. Each option
  carries the `DISPLAY` name plus `data-tylertoo`, `data-tippecanoe`,
  `data-layer` and `data-geometry`.
- It also emits a Markdown table (Dataset | tylertoo archive | tippecanoe
  archive) with plain links. That table is the fallback for readers without
  JavaScript or with a screen reader.
- It raises unless each URL equals
  `https://data.source.coop/nlebovits/tylertoo-benchmarks/{version}/{id}.{tool}.pmtiles`.

**Page HTML**, written by hand after the region:
`<div class="compare-maps">` holds `#compare-tylertoo` and
`#compare-tippecanoe`, both `class="tutorial-map" role="region"`. Then come
Brazil's basemap caption, its CDN tags (maplibre-gl@4.7.1, pmtiles@3.2.1),
and `<script src="../assets/guides/compare-map.js">` at Brazil's relative
depth.

**JS.** `docs/assets/guides/compare-map.js`, under 80 lines, copies
`brazil-map.js`:

- It returns early if anything is missing and registers one `pmtiles.Protocol`
  for two positron maps.
- On a select change, it swaps both sources and styles them by
  `data-geometry` in one color.
- It jumps to the tylertoo header's center and syncs the cameras on `move`,
  with a guard.
- It sets each `aria-label` to "<tool> output: <dataset>" and builds popups
  with `textContent` only.

**CSS.** A `.compare-maps` flex row in `extra.css` stacks below 700 px. Left out: swipe (plugin) and a `--preset` archive.

### CI test

| What | Test | Runs |
|---|---|---|
| Attribution example runs, is shown verbatim, output matches | `examples/attribution/01-credit.sh`, auto-discovered by `crates/python/tests/docs/test_examples.py` | ci.yml `docs` job, every PR |
| Preset example, same checks | `examples/tippecanoe/01-preset.sh`, same test | same |
| Every `tylertoo` command and flag on both pages exists | existing `test_doc_commands.py` | same |
| Tables are fresh; tylertoo flags resolve; preset sources match the diff | `gen-reference-docs tippecanoe` regeneration plus `git diff --exit-code` in ci.yml's "Fail if reference docs drifted" step (~882-899) | same |
| Every tippecanoe flag exists in 2.79.0 | new `benchmarks/e2e/check_tippecanoe_flags.py BIN TABLES`: runs `BIN --help`, matches each backticked `-flag` in the flags table's first column as a whole token, checks that `tile-join` and `tippecanoe-decode` sit next to `BIN` | `tippecanoe-compare.yml`, new step after setup, every PR (it already builds the pin) |
| Comparison region is current, URLs are well formed, version matches the release results | `gen_bench_docs.py --check` (ci.yml:1379) plus new tests in `scripts/ci/tests/test_gen_bench_docs.py`: a synthetic manifest renders; manifest `tylertoo_version` == `e2e-corpus`'s; ids are a subset of `e2e-corpus` `datasets` | every PR |
| Archives are reachable | `tutorials.yml` nightly step: for each manifest URL, `curl -fsS -r 0-126 -o /dev/null "$url"` (the PMTiles header); skip when the manifest is absent | nightly |
| Pages render; script resolves | `test_site_build.py`: add `("guides/tippecanoe/index.html", "compare-dataset")` and `("guides/attribution/index.html", "01-credit.sh")`; `test_local_scripts_resolve` already covers the JS | every PR |
| No link to a deleted page survives, even in code | extend `test_absolute_site_links_resolve` to scan `docs/reference/{cli,python}.md`, `crates/*/src/**/*.rs` and `crates/python/src/docs/*.txt`. Today it skips generated pages, so the 25 dangling `guides/scaling/#...` links there would pass | every PR |
| Prose | `scripts/lint-prose.sh`: add `crates/cli/docs/tippecanoe-tables.md` to `in_scope`; guides and `examples/*/README.md` are already in scope | every PR |

**Example scripts.** Both read `open-buildings.parquet` and start with
Madagascar's download-if-missing `curl` guard. The file holds 1,000 Andorra
buildings (143 KB, ODbL), ships in `fixtures-v1`, and is pinned in
`fixtures.sha256`.

- `examples/attribution/01-credit.sh` tiles z0-10 and z11-14 as two jobs,
  like a coarse job and a shard. Both jobs get
  `--name "Andorra buildings" --attribution '<a href="https://source.coop/vida/google-microsoft-osm-open-buildings">VIDA</a>, © OpenStreetMap contributors'`.
  Then `tylertoo merge andorra.pmtiles low.pmtiles high.pmtiles` prints what
  it wrote (`print_publication`, `lib.rs:147`).
- `examples/tippecanoe/01-preset.sh` runs `tylertoo tiles "$SRC" andorra.pmtiles --preset tippecanoe --layer-name buildings`,
  then `tylertoo stats`.

Each `README.md` holds only the file label, the code fence and the `text`
output. The guide includes it with `--8<--`.

### How it stays accurate

No check depends on review:
- Tables render from `Preset` and clap; the tippecanoe knowledge is one const checked against the pinned binary.
- Maps render from the release manifest, version-checked against the benchmark results.
- Examples run on every PR, and code links resolve against the built site.
- Hand prose has a line budget and states no defaults.

### References to repoint

Re-grep `guides/scaling\|guides/remote-reads\|remote-reads.md\|what-only`
before editing. "T#x" means
`https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/#x` in code and
`OVERVIEW_TUNING.md#x` in docs. "Profiles" is
T#performance-profiles-profile-in-flight-batches-read-workers.

| file:line | New target |
|---|---|
| `crates/core/src/overview/convert.rs:592` (`--read-batch-size`); `crates/python/src/docs/overview.txt:119` | T#read-batch-size |
| `convert.rs:605`, `:622`; `export.rs:767`, `:785` (`--profile`, `--workers`); `export_pmtiles.txt:36`, `:46`; `overview.txt:135`, `:144` | Profiles |
| `convert.rs:635`, `:853`; `export.rs:615`, `:744`, `:799` (`--memory-limit`, `--spill-dir`, `--partition-wave`); `export_pmtiles.txt:19`, `:34`, `:52`; `overview.txt:150`, `:160` | T#memory-sizing |
| `convert.rs:657`, `overview.txt:156` / `convert.rs:675`, `overview.txt:158` | T#batch-concurrency / T#read-workers |
| `convert.rs:5539` `MEMORY_SIZING_DOC`; tests `:8807`, `:8846` | `"https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/#memory-sizing"`; tests assert the constant, not a copy |
| comments `convert.rs:5486`, `:8578`, `assign.rs:2883`; `context/PROFILING.md:536` | "`docs/OVERVIEW_TUNING.md` (Memory sizing)" |
| `config/cli.rs:720` (`--shard`) | `https://geoparquet-io.github.io/tylertoo/tutorials/brazil/#2-split-the-same-build-into-shard-jobs` |
| `crates/core/src/input.rs:41`; `benchmarks/overview/README.md:134` (both name a nonexistent `docs/remote-reads.md`) | "`docs/OVERVIEW_TUNING.md` (Remote staging)" |
| `crates/core/tests/shard_merge_parity.rs:699` | "`docs/OVERVIEW_TUNING.md` (Shared plans for sharded builds)" |
| `crates/cli/tests/thread_count_determinism.rs:34` | Drop `guides/scaling.md` from the list |
| `config/preset.rs:4` | "`gen-reference-docs tippecanoe` renders them into `crates/cli/docs/tippecanoe-tables.md`" |
| `docs/reference/cli.md`, `docs/reference/python.md` (25 links) | Regenerate; never hand-edit |
| `docs/benchmarks.md:208` (`#what-only-tylertoo-does`) | Delete the sentence |
| `docs/benchmarks.md:235` | `[Brazil tutorial](tutorials/brazil.md#2-split-the-same-build-into-shard-jobs)` |
| `docs/OVERVIEW_TUNING.md:1655`, `:1970` | `#shared-plans-for-sharded-builds` |
| `OVERVIEW_TUNING.md:2700` (inside that section), `:2216`, `:2311-2313`, `:2375` parenthetical | Delete the "See ..." text |
| `OVERVIEW_TUNING.md:2088`, `:2115`, `:2367`, `:2372`, `:2767` | `#memory-sizing` |
| `OVERVIEW_TUNING.md:2128` | `tutorials/madagascar.md#2-preview-one-region` |
| `examples/brazil-fields/README.md:231-233` | "To tile a smaller area, add `--bbox`, as the [Madagascar tutorial](https://geoparquet-io.github.io/tylertoo/tutorials/madagascar/#2-preview-one-region) does. [Expression syntax](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/#expression-syntax) covers `--filter`." |
| `examples/brazil-fields/README.md:235-236` | Delete the bullet |
| `CONTRIBUTING.md:66-67` | Link the [tuning reference](https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/) |
| `demo/README.md:13-14` / `demo/RESULTS.md:171-172` | `https://geoparquet-io.github.io/tylertoo/benchmarks/` / delete the sentence |
| `context/PROFILING.md:306`, `:672` | In-file `#reading-a-jobs-maxrss` |
| `scripts/gen_bench_docs.py:69`, `:505` and `brazil_table()`; `test_gen_bench_docs.py:24`; `mkdocs.yml:78-79` | Remove (drop `"brazil-ftw-2025"` from the test's set) |

### Work list for the writer

1. **Moves first.** Add `## Reading a job's MaxRSS` to `context/PROFILING.md`
   (scaling 582-682 verbatim). Hand the Cut table's T rows to the tuning-page
   agent, or apply them if its work has landed. `### Memory sizing` must
   produce the anchor `memory-sizing`.
2. **Delete the two guides.** Remove `docs/guides/scaling.md` and
   `remote-reads.md`. The nav becomes
   `Guides: [Coming from tippecanoe: guides/tippecanoe.md, Attribution: guides/attribution.md]`.
   Two pages still earn a group: tutorials are workflows, and these answer
   "I'm migrating" and "I must credit my data".
3. **Repoint links.** Apply **References to repoint**, Rust and Python sources
   first. Regenerate `docs/reference/cli.md` and `python.md` with the commands
   at ci.yml:882-890. Then run `cargo test -p tylertoo-core pass1_memory`.
4. **Extend the link test.** Extend `test_absolute_site_links_resolve`. It
   must fail before step 3 and pass after.
5. **Stage the fixture.** `stage_input` (`doc_blocks.py`) also copies
   `open-buildings.parquet`. Add `--pattern open-buildings.parquet` at
   `tutorials.yml:75`, because the module fixture runs every example, even
   under `-k brazil`.
6. **Write the examples.** Create `examples/attribution/` and
   `examples/tippecanoe/`, each with a script and a `README.md`. Paste the
   real output from the release binary, with `...` for noise, as Brazil does.
7. **Rewrite `docs/guides/attribution.md`** (at most 40 prose lines).
   - `# Name, description, and attribution` (3 lines): the flags; `tiles`,
     `export-pmtiles`, `pyramid` and `merge`; the Python keywords on
     `convert()`, `export_pmtiles()` and `pyramid()`; `--layer-name` differs.
     Then the example include.
   - `## What gets stored` (5 lines): byte for byte, unescaped, so trust your
     HTML; an unset flag omits the key (tippecanoe fills in a name and
     description); `''` stays empty; nothing is inferred from the input.
   - `## Merged and pyramid archives`: an explicit flag wins per field; the
     4-row table from today's 76-83; 3 lines: credits are never dropped,
     GeoParquet bands carry none, give every shard the same flags.
   - `## Showing the attribution in MapLibre` (3 lines):
     `new pmtiles.Protocol({ metadata: true })`, leave the source's
     `attribution` unset, and link the Protomaps page.
8. **Build the generators.**
   - Write the preset consts, `tippecanoe_doc.rs` with its mode and unit test,
     and the generated tables file.
   - Add the regeneration and diff to ci.yml's drift step and to
     `PULL_REQUEST_TEMPLATE.md:27`, and add the tables file to `lint-prose.sh`.
   - Write `check_tippecanoe_flags.py` and its workflow step. Run it once
     against a local 2.79.0 (`setup_tippecanoe.sh`), and fix any rejected row.
   - Write the `compare-maps` renderer and its tests (synthetic manifest in
     `tmp_path`), `compare-map.js`, the CSS, and the nightly `curl` step.
9. **Rewrite `docs/guides/tippecanoe.md`** (at most 25 prose lines).
   - `# Coming from tippecanoe` (2 lines): tylertoo reads GeoParquet; convert
     other formats with `gpio` first, linking
     [preparing input](../tutorials/madagascar.md#1-prepare-the-input).
   - `## Flag mapping`: a 1-line lead-in, then the `:flags` include.
   - `## The tippecanoe preset` (5 lines): `--preset tippecanoe` and
     `preset="tippecanoe"` swap in the defaults below, and flags you pass
     still win; `tiles`/`convert()` take all of it, `overview` the convert
     rows, `export-pmtiles` the export rows, `pyramid` none; no `--verbatim`;
     the algorithms still differ. Then the `:preset` and example includes.
   - `## Output side by side` (2 lines): each tool's benchmark settings on
     the four corpus datasets, from the release run. Then the region and the
     map HTML. Add this section only once the manifest is committed;
     otherwise ship without it and say so in the PR.
10. **Verify.** Add the two `test_page_renders` rows. Run `scripts/lint-prose.sh`,
    `uv run python scripts/gen_bench_docs.py --check`,
    `uv run pytest -m docs tests/docs` (in `crates/python`, with `TYLERTOO_BIN`)
    and `cargo test -p tylertoo --features gen-docs`.

### Open risks

- **Cross-agent anchors:** T anchors depend on the tuning agent; step 4's test makes a mismatch fail CI.
- **No manifest yet.** **Maintainer:** release with the new job first, or ship one release without maps.
- **Version split:** `preset.rs` cites tippecanoe v2.82.0 (`4f26211`); CI pins 2.79.0
  (`setup_tippecanoe.sh:47`). **Maintainer:** pick one (re-pin to 2.82.0 is consistent).
- **Unverified:** the `tippecanoe --help` token format (no binary here; step 8 checks it), and that
  `tiles --min-zoom 11` writes no z0-10 tiles (else use `pyramid` with two PMTiles bands).
- **Stale neighbors:** `benchmarks.md:200-206` repeats "what tippecanoe can't do";
  `benchmarks/e2e/README.md:132-138` says tile buffer 8 (CLI default is 5). Benchmarks section owns both.
# Docs strategy, sections 4 and 5: API reference and tuning knobs

Checked against `/home/ubuntu/wt/docs-overhaul` at `f52847c1` (origin/main); line numbers are from that commit.
Path shorthand: `ov/` = `crates/core/src/overview/`, `cli.rs` = `ov/config/cli.rs`, `kw.rs` = `ov/config/keywords.rs`,
`lib.rs` = `crates/cli/src/lib.rs`.

## 4. API reference

### Keep

All three references are generated already. I confirmed each gate in the worktree. **CLI:** `docs/reference/cli.md`
comes from clap via the hidden `gen-reference-docs` command (feature `gen-docs`, `lib.rs:528`, layout in
`crates/cli/src/reference_docs.rs`). The unit test `generated_reference_matches_committed_documentation`
(`lib.rs:2124`) and the `docs` job's regenerate-and-`git diff --exit-code` step (`ci.yml:880-899`) guard it.
**Python:** `docs/reference/python.md` comes from `crates/python/scripts/gen_reference.py`, with signatures from
`command_defaults` (via `crates/python/build.rs`) and text from `crates/python/src/docs/*.txt`; the same step diffs it.
**Rust:** docs.rs, with `cargo doc -D warnings` in CI (`ci.yml:901-904`). Vale lints both generated pages: 0 errors
today, and 55 (`cli.md`) and 26 (`python.md`) advisory warnings.

### Cut

- `reference_docs.rs:13-17`, the hidden-default branch of `command_for_markdown`, which exists only for six flags.
- Hand-typed numbers in help strings and docstrings that a constant already holds (see Auto-generate).
- Links to the guides being deleted; the guide owner picks new targets. In help strings: `guides/scaling/#…` at
  `ov/convert.rs:592, 605, 622, 635, 657, 675, 853`, `ov/export.rs:615, 744, 767, 785, 799`, `cli.rs:720`, and
  `OVERVIEW_TUNING/` at `ov/convert.rs:829`, `kw.rs:622`, `cli.rs:484, 696`. In docstrings: `src/docs/overview.txt:119,
  135, 144, 150, 156, 158, 160`, `export_pmtiles.txt:19, 34, 36, 46, 52, 124, 130`, `pyramid.txt:13`,
  `crates/python/python/tylertoo/__init__.py:5`. In a runtime error: `MEMORY_SIZING_DOC` (`ov/convert.rs:5539`, printed
  at `:5674` and `:5700`) holds a repo path; make it a site URL (section 5).
- Stale text: `ci.yml:808-809` says the docstrings live in `__init__.py`, but they live in `src/docs/*.txt`.
  `ov/simplify.rs:89` calls `Drop` the default collapse mode; the default is `Auto`, which acts as `Square`
  (`:107-125`). This rustdoc shows on docs.rs.

### Auto-generate

1. **Unhide the six defaults.** `--point-thinning`, `--line-thinning`, `--polygon-thinning`, `--line-visibility`,
   `--polygon-visibility` (`ov/assign.rs:802-828`) and `--simplify-factor` (`ov/simplify.rs:145-146`) set
   `hide_default_value` and restate the default by hand. Remove the attribute so clap prints `Default: …`, and delete the
   "(default N)" text. Keep only the exceptions, worded like "With `--cluster`, {CLUSTER_POINT_THINNING_DEFAULT}" and
   "0 with `--verbatim`".
2. **Fill constants with `format!`.** Write `help = format!(…, CONST)` for each help string that quotes a constant.
   clap's derive takes an expression for `help`; the writer confirms this with `cargo check`. If it does not compile, the
   fallback is one unit test per constant that pins the help string to it.

   | Help string | Hand-typed | Constant |
   |---|---|---|
   | `--point-thinning` | 16.0 with `--cluster` | `CLUSTER_POINT_THINNING_DEFAULT` `ov/assign.rs:865` |
   | `--workers` (convert, export) | at most 40 | `WORKER_THREADS_MAX` `ov/convert.rs:1128` |
   | `--read-workers` | up to 4 | `READ_WORKERS_MAX` `ov/convert.rs:1271` |
   | `--max-tile-size` / `--max-compressed-tile-size` | 2M / 500K | `DEFAULT_{,COMPRESSED_}TILE_SIZE_LIMIT` `ov/export.rs:287, 295` |
   | `--tile-buffer` | at most 256 | `MAX_TILE_BUFFER_PX` `ov/export.rs:256` |
   | `--size-valve` | down to 512 | `MIN_VALVE_EXTENT` `ov/export/valve.rs:241` |
   | `--valve-max-step` | 8 tiles, RATIO^9 | `MAX_RINGS` `ov/export/valve_predict.rs:402` |
   | `--max-zoom-simplify-factor` | lines get 0.25 | `DEFAULT_LINE_MAX_ZOOM_SIMPLIFY_FACTOR` `ov/export.rs:237` |
   | `--cluster-categories` | up to 100 values | `MAX_CLUSTER_CATEGORIES` `ov/cluster.rs:196` |

3. **Python docstring numbers from `build.rs`**, which already calls `command_defaults`. Add `{default:KEY}`
   placeholders (from `keywords::defaults(ConvertOptions::default())` and `…(ExportOptions::default())`) and
   `{const:NAME}` placeholders (a short `match` over the constants above); `build.rs` panics on an unknown one. Sites:
   `overview.txt:21, 67, 69, 71, 74, 76, 79, 92, 98`, the `simplify_factor` entry, `export_pmtiles.txt:7, 16, 96, 106,
   110, 120`. Generated numbers cannot disagree with core, so no test is needed.
4. **A Python keyword on each `cli.md` row** (`reference_docs.rs` `write_table`). Clap arg ids already equal the serde
   names, which are the Python keywords, for nearly every option. An alias map covers the rest: `shared_nodes`,
   `gsd` → `gsds`, `band` → `bands`, `class_rank` → `class_rank_column`/`class_ranks`/`class_rank_unknown`, and the
   inverted `--no-*` switches. `cli.md` then becomes the living knob inventory.
5. **Environment variables:** one `pub const ENVIRONMENT: &[(&str, &str)]` table in core (name, one-line meaning),
   rendered by the generator as an "Environment variables" section of `cli.md`.

### CI test

These run in the existing jobs.

- `lib.rs` test `no_option_hides_its_default`: walks `Cli::command()` and fails on `is_hide_default_value_set()`.
- `lib.rs` test `every_tuning_flag_has_a_python_keyword_or_is_cli_only`: each visible arg of `tiles`, `overview`,
  `export-pmtiles`, and `pyramid` maps through the alias map to a key of `command_defaults(cmd)`, or is in
  `CLI_ONLY` (`shard`, `shard_plan`, `plan_only`, `verbose`, `files_from`, `report`, `force`, `quiet`).
- Core test `every_env_var_is_documented`: scans `crates/*/src/**/*.rs` for `"TYLERTOO_…"` literals. Each one must be
  in `ENVIRONMENT` or in `DEV_ONLY` (`TYLERTOO_DHAT`, `TYLERTOO_BYTE_ARRAY_LIMIT_BYTES`, the `*_TEST_*` names).
- A site-anchor test, added to `crates/python/tests/docs/test_site_build.py`: after `zensical build --strict`, every
  `https://geoparquet-io.github.io/tylertoo/<path>/#<id>` in `docs/reference/*.md`, `crates/core/src/**/*.rs`,
  `crates/python/src/docs/*.txt`, and `crates/python/python/**/*.py` must resolve to a built page that holds that
  `id`. This covers `MEMORY_SIZING_DOC` and every help-string link.

### How it stays accurate

A default, a constant, or a Python keyword reaches the reference only through code: clap defaults, `format!`,
`build.rs` placeholders, and `command_defaults`. Behavior text still lives in hand-written help strings and docstrings,
but each knob has one string per surface, sitting next to its field. The regenerate-and-diff step plus the four tests
fail when a flag, default, keyword, environment variable, or link target changes without its docs.

### Work list for the writer

1. Add `no_option_hides_its_default` (fails on six args). Remove `hide_default_value` at `ov/assign.rs:802, 808, 814,
   820, 827` and `ov/simplify.rs:145`, rewrite those help strings, delete `reference_docs.rs:13-17`, regenerate.
2. Convert the table's help strings to `format!`: refactors, guarded by the existing diff test.
3. Add a Rust test in `crates/python/src/lib.rs` that fails on a digit after "`None` for" or "default" in
   `src/docs/*.txt` (fails today). Add the `build.rs` placeholders and regenerate `python.md`.
4. Add the Python-keyword test, then the column. Then the env test, then `ENVIRONMENT`. Regenerate after each.
5. Add the site-anchor test, repoint the Cut links once section 5's page exists, and fix the stale text.
6. Run `scripts/lint-prose.sh`: 0 Vale errors, and no growth in warnings on `cli.md` (55) or `python.md` (26).

## 5. Tuning knobs

### Knob inventory

I built this inventory from the option structs, their `Default` impls, clap
attributes, `command_defaults`, and the generated `cli.md` and `python.md`.
I did not use the tuning page. Python defaults come from the generated
signatures, which `command_defaults` produces; I did not run
`tylertoo._defaults()` because the worktree has no venv.

- **Commands:** T = `tiles`, O = `overview`, E = `export-pmtiles`,
  P = `pyramid`, S = `shard-plan`. Python: ov = `overview()`,
  ex = `export_pmtiles()`, cv = `convert()` (the one-call twin of `tiles`),
  py = `pyramid()`.
- **Page §:** the start line of the section on the current
  `docs/OVERVIEW_TUNING.md` that covers the knob.
- **Match:** ✓ means the page's stated default and flag agree with the code.
  I checked every default claim on the page with a pattern search
  (`default N`, `(default …)`, and table rows that start with a flag).
  I checked behavior claims only in the sections listed under the audit.

**95 options in 12 stages, 11 environment variables, and 18 fixed constants.**
The 95 count leaves out metadata (`--name`, `--description`,
`--attribution`), I/O plumbing (`--report`, `-f`, `-v`, `-q`), and the
`decode`, `stats`, `merge`, and `serve` flags. It counts each spelling once,
so `--collapse`, `--collapse-square`, and `--no-collapse` are three options.

#### A. Input selection

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning (from code) | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| A1 | input manifest | `OverviewArgs.files_from` cli.rs:614, `TilesArgs` cli.rs:675, `ShardPlanArgs` lib.rs:298 | none | `--files-from` | (input takes a list) | T O S | Read paths from a manifest, in row order | absent | n/a |
| A2 | bbox | `ConvertOptions.bbox` ov/convert.rs:791 | `None` convert.rs:1801 | `--bbox` | `bbox` | T O / ov cv | Keep features whose bbox meets a lon/lat box (also ±360°); prunes row groups | 2124 | ✓ |
| A3 | mask | `.mask` convert.rs:808 | `None` :1802 | `--mask` | `mask` (also a shapely geometry) | T O / ov cv | Keep features that meet polygons in GeoJSON or GeoParquet | 476 (mention only) | no section |
| A4 | filter | `.filter` convert.rs:830 | `None` :1803 | `--filter` (alias `--where`) | `filter` | T O / ov cv | SQL `WHERE` predicate on properties; prunes row groups | 2150 | not audited |
| A5 | include properties | `PropertySelection.include` ov/properties.rs:41 (convert help overridden at cli.rs:268; export side `ExportOptions.properties` export.rs:652) | `None` (all) convert.rs:1804, export.rs:863 | `--include-property` | `include_properties` | T O E P / all | Keep only these columns (convert: skip decoding the rest) | 2229 | ✓ |
| A6 | exclude properties | properties.rs:57 | `None` | `--exclude-property` | `exclude_properties` | T O E P / all | Drop these columns; include wins | 2229 | ✓ |
| A7 | exclude all properties | properties.rs:66 | `false` | `--exclude-all-properties` | `exclude_all_properties` | T O E P / all | Geometry only; include wins | 2229 | ✓ |

#### B. Level plan

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| B1 | min zoom | cli.rs:618 (O), :679 (T) → `ConvertOptions.levels` convert.rs:452 | 0 convert.rs:1771 | `--min-zoom` | `min_zoom` | T O / ov cv | Coarsest zoom | 308 | ✓ |
| B2 | max zoom | cli.rs:623 (O), :684 (T) | 6 convert.rs:1772; 14 on `tiles` ov/tiles.rs:45 | `--max-zoom N\|auto\|auto:N` | `max_zoom` | T O / ov cv | Finest zoom, or estimate it (ceiling `AUTO_MAX_ZOOM_CEILING`=16 ov/auto_zoom.rs:79) | 308, 340 | ✓ |
| B3 | explicit GSDs | cli.rs:628, :689 → `LevelPlan::Gsds` | none | `--gsd` | `gsds` | T O / ov cv | Per-level GSDs; override the zooms | 308 | ✓ |
| B4 | GSD base | `ConvertOptions.gsd_base` convert.rs:545 | 1024 `GSD_TILE_BASE` ov/level.rs:40 | `--gsd-base` | `gsd_base` | T O / ov cv | `gsd(z)=40075016.69/base/2^z`; larger keeps more detail | 308 | ✓ |
| B5 | mode | `.mode` convert.rs:449 | duplicating :1769 | `--mode` | `mode` | O / ov | Duplicating (each level complete) or partitioning (each feature once) | mentions only | n/a |

#### C. Which features survive (thinning, visibility, ranking)

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| C1 | point thinning | `AssignConfig.point_thinning` ov/assign.rs:804 | 4.0 assign.rs:870; 16 with cluster :865; 0 verbatim convert.rs:1431 | `--point-thinning` (default hidden) | `point_thinning` (`None`) | T O / ov cv | Grid cell × GSD; larger keeps fewer points | 658 | ✓ |
| C2 | line thinning | assign.rs:810 | 1.0 :871 | `--line-thinning` (hidden) | `line_thinning` | T O / ov cv | Same, for lines | 658 | ✓ |
| C3 | polygon thinning | assign.rs:816 | 0 (off) :878 | `--polygon-thinning` (hidden) | `polygon_thinning` | T O / ov cv | Same, for polygons; can remove nested polygons | 658 | ✓ |
| C4 | line visibility | assign.rs:822 | 2.0 :879 | `--line-visibility` (hidden) | `line_visibility` | T O / ov cv | Drop lines whose bbox diagonal is under N GSD | 692 | ✓ |
| C5 | polygon visibility | assign.rs:829 | 2.0 :880 | `--polygon-visibility` (hidden) | `polygon_visibility` | T O / ov cv | Same, for polygons | 692 | ✓ (evidence: page cites Germany, code comment assign.rs:785-793 cites Moldova; both in SWEEPS D6) |
| C6 | sort key | `ConvertOptions.sort_key` convert.rs:497 | `None` :1777 | `--sort-key` | `sort_key` | T O / ov cv | Numeric column picks each cell's winner | 1105 | ✓ |
| C7 | sort direction | `AssignConfig.sort_direction` assign.rs:842 | desc :881 | `--sort-direction` | `sort_direction` | T O / ov cv | Larger or smaller key wins | absent | missing |
| C8 | class rank (string) | `ConvertSyntax.class_rank` kw.rs:602 → `ConvertOptions.class_ranking` convert.rs:501 | `None` kw.rs:651 | `--class-rank` | (structured, C9-C11) | T O | `COL:VAL=RANK,…`, higher wins | 1105 | ✓ |
| C9 | class rank column | `ClassRankSyntax.class_rank_column` kw.rs:30 | `None` kw.rs:345 | none | `class_rank_column` | ov cv | Column that `class_ranks` ranks | absent | missing |
| C10 | class ranks | kw.rs:31 | `None` kw.rs:346 | none | `class_ranks` | ov cv | Priority per class | absent | missing |
| C11 | class rank unknown | kw.rs:32 | `None` kw.rs:347 | none | `class_rank_unknown` | ov cv | Priority of unlisted classes | absent | missing |
| C12 | no auto rank | `.no_auto_rank` convert.rs:506 | `false` :1779 | `--no-auto-rank` | `no_auto_rank` | T O / ov cv | Skip built-in Overture ranking | 1105 | ✓ |
| C13 | magnitude ladder | `ConvertSyntax.magnitude_ladder` kw.rs:576 → `ConvertOptions.entry_zoom` convert.rs:484 | `None` kw.rs:648 | `--magnitude-ladder` | `magnitude_ladder` | T O / ov cv | Rank a column; each value enters one zoom later | 1033 | not audited |
| C14 | ladder step | kw.rs:585 | 1 `DEFAULT_LADDER_STEP` ov/config.rs:41 | `--ladder-step` | `ladder_step` | T O / ov cv | Zooms between rungs | 1033 | ✓ |
| C15 | entry zoom | kw.rs:594 | `None` kw.rs:650 | `--entry-zoom` | `entry_zoom` | T O / ov cv | `COL:VAL=ZOOM,…` entry zooms | 1033 | not audited |

#### D. Density budget

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| D1 | density budget on/off | `DensityBudgetConfig.enabled` assign.rs:2041 | `true` :2182; off under verbatim convert.rs:1437 | `--no-density-drop` | `density_drop` (`None`) | T O / ov cv | Per-level budget after thinning | 1134 | ✓ |
| D2 | drop rate | assign.rs:2052 | 2.5 :2189 | `--drop-rate` | `drop_rate` | T O / ov cv | Each coarser level keeps 1/rate of the finer budget; ≤1 silently disables (convert.rs:7148), which neither help nor page says | 1134 | ✓ default |
| D3 | drop gamma | assign.rs:2063 | 1.5 :2190 | `--drop-gamma` | `drop_gamma` | T O / ov cv | Sparse-area protection; <1 rejected (convert.rs:2852) | 1208 | ✓ |
| D4 | drop lines | `BudgetKinds.lines` assign.rs:2102 | `false` (`POINTS` :2130) | `--drop-lines` | `drop_lines` | T O / ov cv | Budget lines too | 1134 | ✓ |
| D5 | drop polygons | assign.rs:2113 | `false` | `--drop-polygons` | `drop_polygons` | T O / ov cv | Budget polygons too | 1134 | ✓ |

#### E. Geometry: simplification and representation

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| E1 | simplify factor | `SimplifyOptions.factor` ov/simplify.rs:148 | 1.0 simplify.rs:75 | `--simplify-factor` (hidden) | `simplify_factor` (`None`) | T O / ov cv | RDP tolerance × GSD; duplicating only | 757 | ✓ |
| E2 | cascade | simplify.rs:170 | `true` :201 | `--no-cascade` | `cascade` | T O / ov cv | Simplify each level from the finer one | 785 | ✓ |
| E3 | shared nodes (convert) | `SimplifyOptions.shared_nodes` simplify.rs:186 | `false` :202 | `--no-simplification-of-shared-nodes` | `no_simplification_of_shared_nodes` | T O / ov cv | Pin shared-border junctions; same simplification each side | 113 | not audited |
| E4 | collapse to point | `ConvertSyntax.collapse` kw.rs:608 → `SimplifyOptions.collapse` simplify.rs:153 | mode `Auto` simplify.rs:200 (= Square; Point under a ladder, :107-125) | `--collapse` | `collapse` | T O / ov cv | Small polygons become points | 757 | ✓ |
| E5 | collapse to square | kw.rs:613 | (see E4) | `--collapse-square` | `collapse_square` | T O / ov cv | Small polygons become placeholder squares (the default) | 757, 931 | ✗ line 770 says "off" |
| E6 | no collapse | kw.rs:618 | (see E4) | `--no-collapse` | `no_collapse` | T O / ov cv | Drop small polygons | 931 | ✓ |
| E7 | representation bands | `ConvertSyntax.representation` kw.rs:627 → convert.rs:528 | none (all `geom`) | `--representation` | `representation` | T O / ov cv | `LO-HI:geom\|point\|square` | 882 | ✓ |
| E8 | tiles bands | `TilesArgs.band` cli.rs:704 | none | `--band` | `bands` | T / cv | Feature or cell kind per zoom range | 476 | not audited |
| E9 | verbatim | `ConvertSyntax.verbatim` kw.rs:568 → `ConvertOptions::verbatim` convert.rs:1430, `ExportOptions::verbatim` export.rs:888 | `false` | `--verbatim` | `verbatim` | T O / ov cv | Defaults of the 8 ladder knobs to off; on `tiles` also caps off and max-zoom simplify 0 | 590 | ✓ |
| E10 | preset | `ConvertSyntax.preset` kw.rs:563, `ExportPmtilesArgs.preset` cli.rs:568, values in `ov/config/preset.rs` | `None` | `--preset tippecanoe` | `preset` | T O E / ov ex cv | Start from tippecanoe's defaults | mentions | owned by guides/tippecanoe.md |

#### F. Clustering and line coalescing

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| F1 | cluster | `ConvertOptions.cluster` convert.rs:685 | `false` :1794 | `--cluster` | `cluster` | T O / ov cv | Merge a cell's points; add `point_count` | 1250 | not audited |
| F2 | accumulate | convert.rs:693; `PyramidOptions.accumulate` ov/../pyramid.rs:1993 | empty | `--accumulate-attribute` | `accumulate_attributes` (dict) | T O P / ov cv py | `COL:sum\|min\|max\|mean` per cluster or cell | 1250 | not audited |
| F3 | cluster categories | convert.rs:705; pyramid.rs:1999 | `None` | `--cluster-categories` | `cluster_categories` | T O P / ov cv py | Count values (≤`MAX_CLUSTER_CATEGORIES`=100) | mention 476 | no section |
| F4 | coalesce lines | convert.rs:724 (hidden `--coalesce-lines` kw.rs:640) | `true` :1797; off under verbatim | `--no-coalesce-lines` | `coalesce_lines` (`None`) | T O / ov cv | Join touching same-class lines at coarse levels | 1314 | ✓ |
| F5 | coalesce snap | convert.rs:736 | 1.0 ov/coalesce.rs:87 | `--coalesce-snap` | `coalesce_snap` | T O / ov cv | Join ends within N GSD | 1366 | ✓ |
| F6 | coalesce max rows | convert.rs:759 | 2,000,000 coalesce.rs:106 | `--coalesce-max-level-rows` | `coalesce_max_level_rows` | T O / ov cv | Skip coalescing above this count (memory) | 1373 | ✓ |
| F7 | junction angle | convert.rs:774 | 0 coalesce.rs:98 | `--coalesce-junction-angle` | `coalesce_junction_angle` | T O / ov cv | Continue through a junction within N degrees | 1357 | ✓ |

#### G. Overview file and intermediate

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| G1 | row group size | `ConvertOptions.max_row_group_size` convert.rs:560 | 10,000 ov/writer.rs:131 | `--row-group-size` | `row_group_size` | T O / ov cv | Max rows per row group | 2022 | ✓ |
| G2 | row group policy | convert.rs:573 | constant :1786 | `--row-group-size-policy` | `row_group_size_policy` | T O / ov cv | Constant or doubles per coarser zoom | 2050 | ✓ |
| G3 | full column stats | convert.rs:581 | `false` :1787 | `--full-column-stats` | `full_column_stats` | T O / ov cv | Keep min/max stats on big columns | 2066 | ✓ |
| G4 | cogp footer | `.cogp_compat_key` convert.rs:551 | `false` :1784 | `--cogp-compat` | `cogp_compat` | O / ov | Third-party footer key, partitioning only | mention | n/a |
| G5 | temp compression | `TilesOptions.temp_compression` ov/tiles.rs:329 | lz4 tiles.rs:372 | `--temp-compression` | `temp_compression` | T / cv | Codec of the temporary overview | 2305 | ✓ default; numbers hand-measured |
| G6 | keep overview | `TilesOptions.keep_overview` tiles.rs:321 | `None` | `--keep-overview` | `keep_overview` | T / cv | Save the intermediate overview | 425 | not audited |

#### H. Tile encoding (export)

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| H1 | layer name | `ExportOptions.layer_name` ov/export.rs:428; `TilesOptions.layer_name` tiles.rs:305 | "overview" export.rs:845; file stem on `tiles` (tiles.rs:369) | `--layer-name` | `layer_name` | T E / ex cv | MVT layer name | 425 | not audited |
| H2 | layer column | `TilesOptions.layer_column` tiles.rs:316 | `None` | `--layer-column` | `layer_column` | T / cv | One layer per column value | 425 | not audited |
| H3 | tile buffer | export.rs:436 | 5 export.rs:232 (max 256 :256) | `--tile-buffer` | `tile_buffer` | T E P / ex cv py | Edge buffer in tile pixels | 1644 | ✓ |
| H4 | extent | export.rs:443 | 4096 export.rs:227 | `--extent` | `extent` | T E P / ex cv py | MVT coordinate resolution | 1691 | ✗ lines 1011, 1694 say no CLI flag |
| H5 | tile format | export.rs:519 | mvt :853 | `--tile-format` | `tile_format` | T E P / ex cv py | MVT or MLT | 1704 | not audited |
| H6 | MLT profile | export.rs:531 | web :854 | `--mlt-profile` | `mlt_profile` | T E P / ex cv py | MLT codec profile | 1704 | not audited |
| H7 | feature order | export.rs:626 | input :861 | `--feature-order` | `feature_order` | T E P / ex cv py | Paint order in a tile | 1518 | not audited |
| H8 | feature id | export.rs:729 | `None` :866 | `--feature-id` | `feature_id` | T E P / ex cv py | Integer column as MVT id | 1579 | not audited |
| H9 | max-zoom simplify | export.rs:585 | `None` :858 → lines 0.25 (export.rs:237), polygons off; 0 under verbatim (:892) | `--max-zoom-simplify-factor` | `max_zoom_simplify_factor` | T E P / ex cv py | Simplify the finest level once before clipping | 1657 | ✓ default; ✗ cites "Decision 11", which is Decision 12 (SWEEPS.md:540) |
| H10 | shared nodes (export) | `ExportOptions.shared_nodes` export.rs:601 | `false` :859 | `--no-simplification-of-shared-nodes` | `no_simplification_of_shared_nodes` | E / ex | Same as E3, at the finest zoom | 113, 1657 | not audited |
| H11 | simple clip fast path | export.rs:563 | `true` :857 | `--no-simple-clip-fastpath` | `simple_clip_fastpath` | T E P / ex cv py | Faster clip; ring start may rotate | 1919 | ✓ |

#### I. Tile size and the size valve

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| I1 | raw cap | `ExportOptions.tile_size_limit` export.rs:459 | 2 MiB export.rs:287 (`auto`; none on `tiles --verbatim` and pyramid bands, :889) | `--max-tile-size` (alias `--tile-size-limit`) | `tile_size_limit` | T E P / ex cv py | Raw MVT cap, backstop | 1761 | ✓ |
| I2 | compressed cap | export.rs:472 | 500 KiB export.rs:295 | `--max-compressed-tile-size` | `max_compressed_tile_size` | T E P / ex cv py | Gzip cap, like `-M` | 1761 | ✓ |
| I3 | size valve | export.rs:482 | merge :850 | `--size-valve` | `size_valve` | T E P / ex cv py | Merge or drop to fit the cap (extent floor `MIN_VALVE_EXTENT`=512) | 1761 | ✓ |
| I4 | valve scope | export.rs:492 | smooth :851 | `--valve-scope` | `valve_scope` | T E P / ex cv py | How far thinning fades past a dense tile | 1802 | ✓ |
| I5 | valve max step | export.rs:507 | 2.0 :852 | `--valve-max-step` | `valve_max_step` | T E P / ex cv py | Max density ratio between neighbours | 1802 | ✓ |
| I6 | coverage fill | export.rs:540 | `false` :855 | `--coverage-fill` | `coverage_fill` | T E P / ex cv py | Draw child-tile area under coarse tiles | 210 | ✗ cost: page lines 45 and 256 say 2.4-2.8×, help (export.rs:539) says 10-17% |
| I7 | variable depth | export.rs:550 | `false` :856 | `--generate-variable-depth-tile-pyramid` | `variable_depth` | T E / ex cv | Stop branches whose tile holds all finest features | 262 | ✓; ✗ line 1341 says `--variable-depth` |

#### J. Memory and performance

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| J1 | profile | `ConvertOptions.profile` convert.rs:606; `ExportOptions.profile` export.rs:769; `PyramidArgs` cli.rs:513 | auto convert.rs:1789, export.rs:868 | `--profile` | `profile` | all | speed, bounded, or auto | 2305 | ✓ |
| J2 | workers | convert.rs:623; export.rs:787; cli.rs:517 | `None`: all cores, ≤`WORKER_THREADS_MAX`=40 under bounded (convert.rs:1128), `RAYON_NUM_THREADS` (:1153) | `--workers` | `workers` | all | Process-wide thread pool | 2305 | ✓ |
| J3 | memory limit | convert.rs:636; export.rs:801; cli.rs:526 | `None`: detected; budget share `ALLOWANCE_FRACTION`=0.6 (ov/memory.rs:37) | `--memory-limit` | `memory_limit` (bytes) | all | Replace detected memory | 2305 | ✓ |
| J4 | read batch size | convert.rs:593 | 8192 convert.rs:1056 (1..=1,048,576 at :2815) | `--read-batch-size` | `read_batch_size` | T O / ov cv | Rows per Arrow batch | 2085 | ✓ |
| J5 | in-flight batches | convert.rs:658 | auto (0, :1063) = clamp(cores, 4, 16) (:1067, :1081, :1110) | `--in-flight-batches` | `in_flight_batches` (0 = auto) | T O / ov cv | Batches in flight | 2305 | ✓ |
| J6 | read workers | convert.rs:676 | auto (0, :1262): scan `max(cores/2, min(cores,2))`, build `cores/4`, ≤4 (:1336-1337); explicit ≤ max(4, 2×cores) (:1282) | `--read-workers` | `read_workers` (0 = auto) | T O / ov cv | Parallel row-group readers | 2305, 2487 | ≈ page says "cores/2" and leaves out the floor of 2 |
| J7 | spill dir | convert.rs:854; export.rs:746 | `None` (system temp, `TMPDIR` tiles.rs:1847) | `--spill-dir` | `spill_dir` | T O E / ov ex cv | Spill and staging directory | 2305 | links to scaling |
| J8 | partition wave | export.rs:616 | auto (0, ov/export/plan.rs:143) | `--partition-wave` | `partition_wave` (0 = auto) | T E P / ex cv py | Partitions exported at once | absent | missing |
| J9 | work dir | `PyramidOptions.work_dir` pyramid.rs:1960; `MergeArgs` lib.rs:208 | `None` (temp) | `--work-dir` | `work_dir` | P (and merge) / py | Temporary per-band files | 569 | not audited |

#### K. Plans and sharding

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| K1 | save plan | convert.rs:876 | `None` | `--save-plan` | `save_plan` | T O / ov cv | Save the pass-1 plan | 2596 | not audited |
| K2 | reuse plan | convert.rs:894 | `None` | `--plan` | `plan` | T O / ov cv | Skip pass 1 with a saved plan | 2596 | not audited |
| K3 | shard | `TilesArgs.shard` cli.rs:727 | `None` | `--shard I/N\|coarse` | none | T | Build one shard | 2686 | via scaling |
| K4 | shard plan file | cli.rs:731 | `None` | `--shard-plan` | none | T | Shared plan for the fleet | absent | n/a |
| K5 | tile range | `ExportOptions.tile_range` export.rs:681; cli.rs:738 | `None` | `--tile-range` | `tile_range` | T E / ex cv | Emit one tile-id range | 1961 | not audited |
| K6 | plan only | cli.rs:744 | `false` | `--plan-only` | none | T | Write the plan and stop | mention | n/a |
| K7 | zoom ceiling | export.rs:695 | `None` | `--zoom-ceiling` | `zoom_ceiling` | E / ex | Emit tiles at or below a zoom | 1961 | not audited |
| K8 | declared min zoom | `ExportOptions.min_zoom` export.rs:645 | `None` (coarsest level) | `--min-zoom` (E) | `min_zoom` (ex) | E / ex | Metadata min zoom | 1927 | not audited |
| K9 | shard count | lib.rs:307 | required | `--shards` | none | S | Number of data shards | absent | n/a |
| K10 | pivot | lib.rs:313 | 6 (literal `"6"`) | `--pivot` | none | S | Zoom where data shards start | absent | n/a |

#### L. Pyramid only

| # | Knob | Owner field | Default | CLI flag | Python | Cmds | Meaning | Page § | Match |
|---|---|---|---|---|---|---|---|---|---|
| L1 | pyramid band | `PyramidArgs.bands` cli.rs:490 | required | `--band LO-HI:[SYSTEM:]INPUT[:LAYER]` | `bands` (positional) | P / py | One input per zoom band | 393, 519 | not audited |
| L2 | generalize | cli.rs:500 | `false` | `--generalize` | `generalize` | P / py | Generalize bands instead of verbatim | 393 | not audited |
| L3 | allow missing zooms | `PyramidOptions.allow_missing_zooms` pyramid.rs:1971 | `false` :2021 | `--allow-missing-zooms` | `allow_missing_zooms` | P / py | Accept sparse pre-tiled bands | 557 | not audited |

#### M. Environment variables (not reachable from clap or serde)

| Variable | Read at | Default | Kind | On page |
|---|---|---|---|---|
| `TYLERTOO_AUTO_MEM_LIMIT_BYTES` | ov/pipeline.rs:316 | unset (detected) | user | 2305 |
| `RAYON_NUM_THREADS` | ov/convert.rs:1153 | unset | user | via help |
| `TYLERTOO_SKIP_MEMORY_PREFLIGHT` | ov/convert.rs:5529 | unset | user (escape hatch named in the error) | 210 |
| `TYLERTOO_HTTP_<KEY>` | crates/core/src/input.rs:2261 | unset | user (remote client options) | 2524 |
| `TYLERTOO_STAGING_CONCURRENCY` | input.rs:2470 | derived, base `STAGE_MAX_CONCURRENCY`=8 (:2425), ≤128 | experimental ("not stable API", input.rs:2456) | 2524 |
| `TYLERTOO_STAGING_MEM_BUDGET_BYTES` | input.rs:2473 | 256 MiB (:2416, :2433) | experimental | 2524 |
| `TYLERTOO_STAGING_COALESCE_BYTES` | input.rs:2475 | 0 (:2441) | experimental | 2524 |
| `TYLERTOO_PROFILE_JSON` | ov/convert.rs:3442, ov/stream.rs:4641 | unset | diagnostic | 2305 |
| `TYLERTOO_TILE_TRACE` | crates/core/src/tile_trace.rs:39 | unset | diagnostic | absent |
| `TYLERTOO_DHAT` | crates/cli/src/main.rs:40 | unset | dev | absent |
| `TYLERTOO_BYTE_ARRAY_LIMIT_BYTES` | crates/io/src/untrusted.rs:1301 | `testing` feature only | test | absent |

#### N. Fixed constants that shape output (no flag, no keyword)

`CLUSTER_POINT_THINNING_DEFAULT` 16 (assign.rs:865).
`DEFAULT_LINE_MAX_ZOOM_SIMPLIFY_FACTOR` 0.25 (export.rs:237).
`MAX_TILE_BUFFER_PX` 256 (export.rs:256).
`MIN_DENSITY_LEVEL_FEATURES` 256 (assign.rs:2028).
`SUPERCELL_GSD_FACTOR` 128 (assign.rs:2021).
`MAX_VALIDATION_VERTS` 2,048 (simplify.rs:1332).
`AUTO_MAX_ZOOM_CEILING` 16 (auto_zoom.rs:79).
`AUTO_ZOOM_SAMPLE_CAP` 200,000 (auto_zoom.rs:83).
`MAX_ZOOM` 30 (crates/core/src/tile.rs:49).
`MAX_CLUSTER_CATEGORIES` 100 (cluster.rs:196).
`IN_FLIGHT_BATCHES_MIN` and `_MAX`, 4 and 16 (convert.rs:1067, :1081).
`READ_WORKERS_MAX` 4 (convert.rs:1271).
`WORKER_THREADS_MAX` 40 (convert.rs:1128).
`ALLOWANCE_FRACTION` 0.6 (memory.rs:37).
`MIN_VALVE_EXTENT` 512 (valve.rs:241).
Read-batch maximum 1,048,576 (convert.rs:2815, a literal).
`MAX_RINGS` 8 (valve_predict.rs:402).
`STAGE_MAX_CONCURRENCY` 8 (input.rs:2425).

The page states 12 of these by hand. All 12 match the code today.

### Audit of the current page

The current page is `docs/OVERVIEW_TUNING.md` (2,781 lines). **Fully audited:** 1-60, 210-392 (coverage fill, variable
depth, GSD ladder), 590-815 (verbatim, thinning, visibility, simplification), 877-960, 1000-1032 (squares), 1105-1249
(ranking, budget), 1635-1703 (tile geometry), 1759-1918 (size cap, valve), 2305-2595 (profiles, concurrency, staging),
2721-2781 (scenarios). **Not audited** except by the page-wide default search: 61-209, 393-589, 816-876, 961-999,
1033-1104, 1250-1634, 1704-1758, 1919-2304, 2596-2720.

**(i) Defaults and flags that code can supply.** The page states about 35 defaults: tables at 319-321, 670-672,
706-707, 768-772, 1157-1161, 1641-1642, 2094, 2257-2259, and 2323-2326; headings at 785, 1357, 1366, 1373, 2022, 2050,
and 2066; inline at 1065, 1190, 1236, 1646, 1693, and 1814. All but line 770 match the code, and each one is a second
copy of `cli.md`.

**(ii) Behavior with a source in code or tests** (a sample): verbatim sets the 8 knobs listed
(`ConvertOptions::verbatim`, convert.rs:1430); `0` turns a factor off and negatives are errors (convert.rs:2842-2850);
`--drop-gamma` ≥ 1 (convert.rs:2852); the budget floor of 256 and 128-GSD super-cells (assign.rs:2021-2028); the 60%
allowance; the in-flight clamp of 4-16; staging limits (input.rs:2416-2490); identical output across thread counts
(`crates/cli/tests/thread_count_determinism.rs`); bit-reversed point rank (valve.rs:809); the `valve_guard.py`
thresholds (`benchmarks/valve_guard.py:83-84`); the zoom ceiling of 30 (tile.rs:49).

**(iii) Claims that code or another owner contradicts, or that have no source** (19):

| Line | Quote | Finding |
|---|---|---|
| 45, 256 | "2.4–2.8× the build time" | Contradicts the `--coverage-fill` help: "Adds 10–17% to build time" |
| 770 | "`--collapse-square` \| off" | Wrong: squares are the default (simplify.rs:107-125) |
| 1011-1012 | "the only one the CLI exports at … `export(extent=…)`" | Wrong: `--extent` exists, and the function is `export_pmtiles` |
| 1694 | "there is no CLI flag" | Wrong: `--extent` (export.rs:443) |
| 1341 | "in `--variable-depth` stopped tiles" | Not a flag |
| 1689 | "`corpus/SWEEPS.md`, Decision 11" | It is Decision 12 (SWEEPS.md:540) |
| 1239-1240 | "Lines and polygons use thinning factor 1, so the budget usually affects them more" | Wrong: polygon thinning is 0, and the budget is points-only |
| 1138 | "On Portland roads, this is 2–3× tippecanoe's count" | Describes the line budget before #745 (SWEEPS D2, superseded) |
| 1917 | "retains points at uniform spatial intervals" | Conflicts with the bit-reversed rank the same section describes |
| 2334-2335 | "Each phase is capped at or below main's" | "main" is a git branch; the sentence tells a user nothing |
| 796 | "cumulative deviation at about 2× the target level's tolerance" | No source found |
| 719-722 | "Values below 2.0 change little … simplification collapses nearly every extra feature" | Reasoning from before #745: collapsed polygons now become squares. Unverified |
| 781 | "High factors also remove features whose bbox diagonal falls below" | Same: by default these become squares, not removals. Unverified |
| 735-745 | The empty-level warning for 59M buildings, with "--collapse" as the remedy | Unverified under default squares; `crates/core/src/diagnostics.rs:519` gives the same advice |
| 813-814 | "logs a count of skipped checks at `info` when conversion ends" | The code logs each candidate (simplify.rs:1350); I found no end-of-run count |
| 2326 | "pass 1 cores/2" | Leaves out the floor of 2 (convert.rs:1336) |
| 2738-2743 | "use `--collapse` for points" / "`--collapse-square` for polygons" | Squares are already the default |
| 2746-2747 | "Too many mid-zoom features … raise `--drop-rate`" | By default the budget does not touch lines or polygons |
| 2767 | "keep streaming enabled" | No such setting exists |

The page also has 129 lines of hand-typed measurements that no generator owns, and 40 lines that cite issue numbers as
their evidence.

**(iv) Content that duplicates another owner:**

| Owner | Duplicated content |
|---|---|
| The CLI reference | Every table in (i), and paraphrases of help strings (for example, lines 1761-1800 repeat `--size-valve` and `--valve-scope`) |
| The spec | Footer provenance keys (331-333, 723, 785-797, 925-929, 1123, 1246-1248), §7.3 level omission (727-756), §2.4, §3.5, §5.2 |
| `corpus/SWEEPS.md` | The measurements behind defaults (683-690, 714-719, 1187-1206, 1686-1689) |
| `context/ARCHITECTURE.md` and `guides/tippecanoe.md` | Tippecanoe divergences (985-1032, 1098-1104) |
| `context/PROFILING.md` | Engine internals (2305-2523) |
| The Madagascar tutorial | The `--bbox` example (2132) |

### Mechanism options

| Option | Cost | Catches | Misses |
|---|---|---|---|
| (a) Generated tables in marker regions on the page, via a sibling of `gen-reference-docs`, checked by the same diff step | About 150 lines of generator code, plus markers through a 2,700-line page | Flag, default, and keyword drift inside the tables | All prose. It also renders the same facts a second time beside `cli.md` |
| (b) A Rust test that parses the page: every flag exists, every stated default equals clap's, every flag is covered | About 100 lines; reading defaults out of English is fragile | Renamed flags, table defaults, a new flag with no docs | Defaults written as prose, behavior, measurements |
| (c) Collapse: per-knob facts move into help strings and docstrings, the generated references become the knob reference, and a short "how to choose" page remains | Writing time; help strings get longer | Nothing on its own; it removes most of what can drift | Behavior prose on the short page |

### Recommendation

**Use (c) with a light form of (b).** Take one part of (a), the Python keyword column, and put it in the generated
`cli.md`; add no marker regions. With defaults unhidden, constants filled by `format!`, and the Python column, `cli.md`
becomes the knob reference and the living copy of the inventory above. The hand-written page becomes a "how to choose"
guide that states no defaults and no measurements. A test in `crates/cli` enforces that, and it fails when the CLI gains
a flag that neither the page nor an explicit allow-list mentions.

Behavior prose stays honest three ways. Each per-knob fact lives only in its help string, next to the field, where it
renders in `--help` and `cli.md` and Vale lints it. The page's example commands run through the real CLI parser and the
pre-I/O `validate()`, which checks `--band`, `--representation`, `--entry-zoom`, and `--class-rank` syntax; the filter
parser checks the filter examples. Measurements appear only in `<!-- bench:… -->` regions, which
`scripts/gen_bench_docs.py` owns, and a line cap keeps the rest small enough to review. The plan adds no config file or
schema, parses no defaults out of English, and uses no LLM audit.

### Keep

The new page is at most 350 lines and aims for about 250. Proposed path: `docs/reference/tuning.md`, nav "Reference →
Tuning knobs". Each flag mention links to its `cli.md` row.

| # | Section | Lines | Content |
|---|---|---|---|
| 1 | How a level is built | 20 | The GSD ladder, then selection, geometry, and export in order |
| 2 | Complete tiles or small tiles | 30 | The trade-off table from lines 40-49, without numbers |
| 3 | More or fewer features at coarse zooms | 35 | Thinning, visibility, `--gsd-base`, the points-only budget, ranking (with `--sort-direction` and the Python `class_rank_*` keywords), entry zooms |
| 4 | Geometry detail | 25 | `--simplify-factor`, max-zoom simplification, shared nodes, collapse modes, `--representation`, `--verbatim` |
| 5 | Tile size | 25 | Caps, valve, scope and step, variable depth |
| 6 | Points and lines | 20 | Clustering, reserved columns, coalescing |
| 7 | Several layers | 20 | `--layer-column`, `tiles --band`, `pyramid` |
| 8 | Memory and speed (anchor `#memory`) | 35 | When to pick `auto`, `speed`, or `bounded`; `--memory-limit`; `--spill-dir` on local disk; leave the `auto` concurrency knobs alone unless one is measured as the bottleneck; `--temp-compression`; `--plan` reuse; what to do when the pass-1 preflight refuses |
| 9 | Filtering and selection | 25 | `--bbox`, `--mask`, the `--filter` grammar (parser-checked), property selection |
| 10 | Output layout | 10 | Row groups, column stats |

Section 8 is the only thing the page absorbs from the deleted guides, and it absorbs choices, not internals. Sharded
builds, remote staging, and plans shared across a fleet go to the Brazil tutorial, plus the `--shard`, `shard-plan`, and
`TYLERTOO_HTTP_*` rows of the generated reference.

`corpus/SWEEPS.md` stays where it is as contributor history, because code comments cite its path; the user page links it
once at most. Fix its line 7, which puts the CLI defaults in `crates/cli/src/main.rs` (they live on the core structs).
Mark superseded headings in the heading text: "Decision 2 (superseded by 11)", and Decision 5 reworded so `zoom-scaled`
is opt-in and `constant` is the default.

### Cut

- Every default table and default-stating heading; the 129 measurement lines outside bench markers; the 40 issue-number
  citations; every (iii) claim; and spec restatements (link the spec section instead).
- Engine internals (2305-2523, 785-815, 1373-1442, 1946-1960); git keeps them. The staging table (2524-2595) moves to
  `context/PROFILING.md`, since the code calls it sweep knobs, not stable API (input.rs:2456); repoint input.rs:2260, 2457.
- Tippecanoe divergences (985-1032, 1098-1104): add rows `context/ARCHITECTURE.md` lacks there (CLAUDE.md requires it);
  preset details belong to `guides/tippecanoe.md`, which the guides agent owns.
- Worked scenarios (2721-2781): fold the valid rows into sections 2-8 as "if X, try Y" lines; drop 2738-2747, 2767,
  and 2775-2778.

### Auto-generate

Do everything in section 4's Auto-generate list. Constants that clap cannot reach get to users only two ways: through
help strings (`format!`) and through `ENVIRONMENT`. The guide never restates them.

### CI test

Add `lib.rs` test `tuning_page_follows_the_cli`, which reads `include_str!("../../../docs/reference/tuning.md")`. It
checks eight rules:

1. Every `--flag` token is a long flag or alias in the `Cli::command()` tree, or is in `FOREIGN_FLAGS` (tippecanoe and
   cargo spellings: `--buffer`, `--coalesce-densest-as-needed`, `--layer-by-column`, `--use-attribute-for-id`,
   `--no-default-features`).
2. Every visible long flag of `tiles`, `overview`, `export-pmtiles`, and `pyramid` appears on the page or in
   `UNTUNED_FLAGS` (metadata, `--report`, `--force`, `--files-from`, sharding). A new flag fails with "mention `--x` in
   docs/reference/tuning.md or add it to UNTUNED_FLAGS".
3. No table header cell `Default`, and no `(default …<digit>` or `default (is |of )?` followed by a digit.
4. Outside bench regions and code fences, no digit directly followed by `s`, `ms`, `MB`, `GB`, `MiB`, `GiB`, `%`, `×`,
   or `x`.
5. No `#NNN` issue references.
6. Every backticked Python keyword (`` `kw=` `` or "Python `kw`") is a key of some `command_defaults(…)`.
7. Every `tylertoo …` line in a bash block passes `Cli::try_parse_from` and the CLI's pre-I/O `validate()`.
8. The page has at most 350 lines.

Also: a core test parses each line of the page's `<!-- check:filter -->` block with the filter parser.
`test_doc_commands.py`, `gen_bench_docs.py --check` (update its list at `scripts/gen_bench_docs.py:70`), and
`scripts/lint-prose.sh` keep running unchanged, and the section 4 site-anchor test guards links into the page.

### How it stays accurate

Flags, defaults, constants, and keywords live only in generated `cli.md` and `python.md` (rule 3 stops restatements).
A renamed flag breaks rules 1 and 7; a new flag breaks rule 2; a moved anchor breaks the site-anchor test; numbers come
from `benchmarks/results/*.json`. Unchecked: about 250 lines of advice pointing at the help strings that own the facts.

### Work list for the writer

Do section 4's work list first, because the guide links to the regenerated `cli.md`.

1. Write `tuning_page_follows_the_cli` in `crates/cli/src/lib.rs`. Create an empty `docs/reference/tuning.md` and check
   that the test fails on rule 2.
2. Write the page from the outline, using only help strings, the inventory, and code. Work until the test passes; add
   flags that the page should not explain to `UNTUNED_FLAGS`.
3. Move per-knob facts the help lacks into help strings: `--drop-rate` ≤ 1 disables the budget, the
   `--read-batch-size` range, how `--polygon-visibility` relates to squares, and reserved columns on `--cluster` and
   `--no-coalesce-lines`. Regenerate `cli.md` and keep Vale errors at 0.
4. Add the `<!-- check:filter -->` block and its core parser test. Write the test first.
5. Delete `docs/OVERVIEW_TUNING.md` and update `mkdocs.yml:86` and every inbound link: the URLs in section 4's Cut list;
   `docs/guides/tippecanoe.md`, `examples/*/README.md`, `CONTRIBUTING.md`, and
   `context/{ARCHITECTURE,PROFILING,GEO_OVERVIEWS_ONEPAGER}.md`; `crates/core/src/diagnostics.rs:519`; the comments at
   `ov/level.rs:79`, `ov/stream.rs:5625`, `ov/filter.rs:1920`, `ov/coalesce.rs:102, 134`, `ov/convert.rs:1901`,
   `crates/core/src/shard.rs:118`, `crates/core/tests/overview_hostile.rs:969`,
   `crates/cli/tests/thread_count_determinism.rs:33`, and `crates/python/tests/test_properties.py:4`; and
   `crates/core/tests/properties_tiles_options.rs:5, 37`, where the oracle should point at `validate()`.
6. Set `MEMORY_SIZING_DOC` (`ov/convert.rs:5539`) to `https://geoparquet-io.github.io/tylertoo/reference/tuning/#memory`.
7. Fix stale code text: `ov/simplify.rs:89` (the default is `Auto`, not `Drop`) and `shard.rs:127` ("a default of 8"
   is now 5). Change the `diagnostics.rs:519` advice if a run shows `--collapse` is no longer the remedy.
8. Move the staging table to `context/PROFILING.md`, add any missing divergences to `context/ARCHITECTURE.md`, and fix
   the `corpus/SWEEPS.md` header and headings.
9. Run `scripts/lint-prose.sh`, `uv run python scripts/gen_bench_docs.py --check`, `cargo nextest run --profile quick`,
   and `uv run pytest -m docs tests/docs`.

### Open risks

- The guide's advice is still hand-written. The cap, the regex bans (rules 3 and 4, which rephrasing can dodge), and
  parsed commands limit how wrong it can get; they do not prove it right.
- Python parameter text (`src/docs/*.txt`) and clap help are two hand-written descriptions of each knob. This plan fixes
  their numbers, not their wording; I left out generating docstring prose from clap help.
- `help = format!(…)` must compile inside `cfg_attr`; if not, use one pin test per constant. Rule 7 needs shell-style
  splitting, and some `validate()` paths open files (`--mask`).
- Longer help strings add Vale warnings to `cli.md` and lengthen `--help`. External bookmarks to `/OVERVIEW_TUNING/`
  break unless the site keeps a stub or a redirect.
- **The maintainer needs to decide five things:**
  1. The page path, and whether to keep a stub at the old URL.
  2. Whether to ban measured numbers in help strings. I recommend extending rule 4 to `cli.md`. `--coverage-fill` and
     `--temp-compression` carry numbers today.
  3. Which coverage-fill cost is current: "10–17%" in the help or "2.4–2.8×" on the page.
  4. Whether the staging variables leave the user docs. I recommend they do.
  5. Where sharded builds and shared plans live once `scaling.md` is gone. I recommend the Brazil tutorial plus help
     strings, not this page.
