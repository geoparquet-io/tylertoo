# Performance profile — methodology + history

Companion to [`RESULTS.md`](./RESULTS.md) (storage/access numbers).
This page records **how** the pipeline's performance was measured and
the sequence of changes that produced the current numbers. The full
point-in-time engineering notes are archived in
[`context/archive/bench/`](../../context/archive/bench/) —
`H3_NOTES.md` (streaming convert + export restructure),
`H3C_PROFILE.md` (wall-time profile + optimization levers),
`EXPORT_NOTES.md` (E0 export design/validation).

## Current headline (Moldova, the large stress case)

`polygons-ftw-moldova-large` — 631,910 polygons, 38 M canonical
vertices, duplicating mode, z0–14 (12 emitted levels), release build,
16-core machine:

| Command | Wall | Peak RSS |
|---|---:|---:|
| `gpq-tiles overview` (convert) | **~55 s** | **~320 MB** |
| `gpq-tiles export-pmtiles` | **~59 s** | **~2.4 GB** |

Full pipeline (GeoParquet → overview file → PMTiles) < 2 min.

## Methodology

- **Wall/RSS**: `/usr/bin/time -v` on the release binary; identical
  commands to `run_conversion.sh`.
- **Phase breakdown**: `std::time::Instant` accumulators in
  `crates/core/src/overview/stream.rs` and `export.rs`, logged at
  `RUST_LOG=gpq_tiles_core::overview=debug` (instrumentation is
  retained behind that log level).
- **Heap**: `cargo build --release --features dhat-heap` (see
  `docs/PROFILING.md`).
- **Output equivalence**: every perf change was gated on footer/row
  equivalence and all `gpq-tiles validate` checks passing.

## History (all 2026-07-03, chronological)

1. **H1 — writer footer pathology fixed.** Per-row-group min/max stats
   suppressed on geometry/string columns + per-level row-group sizing.
   Moldova duplicating footer 8.84 MB → 0.24 MB (−97 %), file
   −12.5 %, remote viewport bytes −42…−81 %. Details: the H1 revision
   note in `RESULTS.md`.
2. **H3(a) — two-pass streaming convert (now the default).** Pass 1
   builds per-feature winner tables; pass 2 re-reads per level and
   writes batch-by-batch. Peak RSS 5.30 GB → 305.7 MB (−94 %), wall
   unchanged; output equivalent (footer byte-identical). Archived:
   `H3_NOTES.md`.
3. **H3(b) — bounded-memory export.** Per-zoom whole-band
   materialization + serial BTreeMap merge replaced with a partitioned
   streaming pipeline into the PMTiles writer. Archived: `H3_NOTES.md`
   (second half).
4. **H3(c) — wall-time profile + four levers.** The profile showed
   both commands strictly serial with simplification 96.9 % of convert
   wall — and *inside* simplify, `geo` ring-validation (not RDP)
   dominating, with invalid candidates triggering full-resolution
   re-validation. Levers shipped: (1) rayon in convert pass 2,
   (2) rayon per-feature clip / per-tile encode in export, (3)
   validation cuts + eps-halving fallback fix in `overview::simplify`,
   (4) export bbox fast path skipping the clip for fully-interior
   features. Result: convert 706 s → 55.3 s (**12.8×**), export
   288 s → 58.8 s (**4.9×**); the fallback fix also cut the Moldova
   overview file −18.6 % (coarse levels no longer quietly carry ~5–9 %
   of features at full resolution). Remaining known lever: cascaded
   simplification (simplify level k from level k+1's output).
   Archived: `H3C_PROFILE.md`.

## Known behavior notes (still true)

- Streaming vs in-memory divergence: the streaming path decides level
  omission from the winner table (pre-simplification) and fails loudly
  with `EmptyLevel` if every winner of a level degenerates — impossible
  under default knobs. (`stream.rs` module docs.)
- Export reintroduces MVT border duplication: a feature spanning a
  tile seam appears in every tile it touches (0 % while a level fits
  one tile, ~7 % at z14 on Portland roads). Accounting table:
  archived `EXPORT_NOTES.md`.
- `--tile-size-limit` is a single non-iterative drop pass per
  oversized tile — a backstop, not the sizing mechanism.

## Big-file tier (2026-07-04)

Pre-release validation on real multi-GB inputs (maintainer request;
complements the #179 release-readiness pass). Binary: release build at
`319d147`; 16-core AMD Ryzen 7040 laptop, 54 GiB RAM. Data lives in
`corpus/data/bigbench/` (gitignored; provenance in
`bigbench-manifest.json` there, raw + optimized copies alongside).

### Datasets + gpio optimization

Optimized with the local dev geoparquet-io checkout (gpio 1.3.0 @
`9b37138`): `gpio sort hilbert <raw> <opt> --add-bbox
--geoparquet-version 1.1 --compression zstd --overwrite`, wrapped in
`/usr/bin/time -v`. Overture extracts via DuckDB v1.4.1
(httpfs+spatial, `COPY ... (FORMAT PARQUET, COMPRESSION ZSTD)`,
`preserve_insertion_order=false`), release `2026-06-17.0`, Germany
bbox `[5.87, 47.27, 15.04, 55.06]`.

| dataset | source | rows | raw | gpio | sort wall | sort RSS |
|---|---|---:|---:|---:|---:|---:|
| fieldmaps-adm4 | data.fieldmaps.io edge-matched humanitarian ADM0–4 (MultiPolygon) | 363,783 | 3.22 GiB | 2.70 GiB | 47.2 s | 8.6 GiB |
| overture-germany-buildings | Overture buildings/building (polygons) | 59,032,924 | 5.09 GiB | 6.51 GiB | 3:22.6 | 18.1 GiB |
| overture-germany-segments | Overture transportation/segment (lines) | 19,243,535 | 1.96 GiB | 2.38 GiB | 1:21.6 | 5.8 GiB |

### Conversion results (`gpq-tiles overview`, default knobs, z0–14)

DNFs are results, not gaps — see findings.

| dataset / mode | wall | peak RSS | CPU | output |
|---|---:|---:|---:|---:|
| fieldmaps-adm4, duplicating | **DNF** (>45 min, killed) | — | ~184 % | 1.21 GiB partial |
| fieldmaps-adm4, partitioning (pre-#221, 15× re-read) | **2:57.3** | 1.55 GiB | 99 % | 2.92 GiB, 363,783 rows / 15 levels |
| fieldmaps-adm4, partitioning (#221 single-read) | **0:55.6** | 5.51 GiB | 109 % | identical: 363,783 rows / 15 levels |
| fieldmaps-adm4 partitioning → `export-pmtiles` | **DNF** (killed at 3 h 13 m wall / 7 h 29 m CPU, 231 %) | — | — | nothing written |
| fieldmaps-adm4, `gpio pmtiles create` (tippecanoe, keep-everything defaults) | **DNF** (killed at 1:26:08) | 6.0 GiB | 347 % | 15.07 GB partial from a 2.9 GB input |
| fieldmaps-adm4, `gpio pmtiles create --tile-size-limit` (tippecanoe native triage) | not measured (launched, stopped at ~2 min by decision; rerunnable) | — | — | — |
| germany-segments, duplicating (run 1) | 3:09.1 | 8.64 GiB | 129 % | 4.80 GiB, 48.4 M rows / 15 levels |
| germany-segments, duplicating (run 2) | **3:08.1** | 8.64 GiB | 130 % | (same) |
| germany-buildings, duplicating | **FAILED** at 1:16.5 | 10.5 GiB | 97 % | `level 0 is empty` → #211 |

### Findings

1. **The success bar is parity with tippecanoe minus the GeoJSON
   detour — and on vertex-heavy global polygons, *every* tool DNF'd
   in lossless mode.** fieldmaps adm4 carries 261 M vertices in 364 k
   features (~7× the Moldova stress case). Our duplicating convert
   exceeded 45 min; our export ran 3 h+ without emitting a tile;
   tippecanoe with keep-everything flags was killed at 1 h 26 m with
   a 15 GB partial archive from a 2.9 GB input. Lossless tiling of
   this class of data is ill-posed for every tool tested. The
   conclusion (tracked in #212): triage must happen once per level at
   convert time, not per tile at export time.
2. **The overview GeoParquet artifact is the product story.**
   Partitioning-mode convert was the only thing any pipeline produced
   quickly on fieldmaps: 2.92 GiB, 15 levels, in **2:57 at 1.55 GiB
   RSS** — a queryable multi-resolution artifact while every
   tile-materializing path DNF'd. **Update (2026-07-08, #221 merged):**
   the single-read engine cut this to **0:55.6 (3.2×)**, landing on the
   ~50 s target #220 was opened to chase. #220 (per-level row-group
   winner indexes) is therefore **closed as obviated** — CPU held at
   109 % (≈1.1 cores), confirming partitioning is now I/O-/single-thread
   bound, not row-filter bound, so an index would chase a ~5 s residual.
   The speed came at RAM cost (1.55 → 5.51 GiB) from the `auto` profile
   buffering full-resolution output; `--profile bounded` caps it.
3. **Germany segments is the clean win: 19.2 M lines (2.38 GiB) →
   48.4 M rows / 15 levels (4.80 GiB) in 3:08 at 8.6 GiB RSS**,
   ~13 MB/s of input, runs 1 and 2 within 1 s of each other. Level
   duplication amplified rows 2.5× and bytes ~2× — the expected
   duplicating-mode cost, nothing Moldova-pathological.
4. **Convert is effectively serial at this scale: 97–130 % CPU on 16
   cores** across every cell (only tippecanoe exceeded 3 cores).
   Decode/simplify/write take turns; pipeline parallelism is filed as
   #213 and is the single biggest wall-clock lever this tier exposed.
5. **Germany buildings (59 M polygons) fails outright**: every
   feature drops out of the z0 thinning grid and convert aborts with
   `level 0 is empty` after 1:16 (#211, release blocker). A big-file
   tier exists precisely to catch this class of bug before release.
6. Memory peaked at 18.1 GiB in gpio sort (DuckDB) and 10.5 GiB in
   the buildings convert — the streaming convert path held fieldmaps
   partitioning to 1.55 GiB.
7. The antimeridian warning (#199) fired on real data — 7
   >180°-wide features in fieldmaps — exactly as designed.

### Methodology notes

- Same harness as above: `/usr/bin/time -v`, release binary, default
  knobs, `--mode duplicating --min-zoom 0 --max-zoom 14` (plus one
  `--mode partitioning` cell), `--report` JSON captured per run;
  45-min timeout per cell.
- Long benchmark runs must be **detached from agent-harness task
  groups** (`setsid nohup ... > driver.log`): a harness task-group
  kill produced a false "converter died" signal (exit 144, no timing
  file) that cost one fieldmaps run.
