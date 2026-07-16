# H3(c) — Wall-time profile of the overview pipeline (Moldova)

Where the ~12 min of `overview` convert and ~5 min of `export-pmtiles` actually
go, measured on `corpus/data/gpio/polygons-ftw-moldova-large.parquet`
(631,910 polygons), duplicating mode, z0–z14 (12 emitted levels), 16-core
machine, release build.

Method: `std::time::Instant` phase accumulators in
`crates/core/src/overview/stream.rs` and `export.rs`, logged at
`RUST_LOG=tylertoo_core::overview=debug` (instrumentation retained behind that
log level), plus `/usr/bin/time -v` and live `ps`/`/proc/<pid>/task` sampling.
`perf` was unavailable (`perf_event_paranoid=4`, no sudo); the simplify
decomposition below came from a throwaway micro-benchmark on real Moldova
geometries (not committed; described in §4).

## 1. CPU utilization: strictly serial

Both commands run **one thread at ~100% of one core** for their entire life
(`/proc/<pid>/task` count = 1; `ps %cpu` = 99.9; `time` reports 99–100% CPU on
a 16-core box). Neither pipeline has any parallelism today.

## 2. Convert phase breakdown (total wall 726.8 s)

| Phase | Wall (s) | Share |
|---|---:|---:|
| Pass 1 (stream + feature scan) | 1.3 | 0.2% |
| Assignment + density budget | 0.5 | 0.1% |
| Pass 2 — parquet read+decode (12 re-reads) | 9.4 | 1.3% |
| Pass 2 — winner select + geometry decode | 2.2 | 0.3% |
| **Pass 2 — simplification** | **704.6** | **96.9%** |
| Pass 2 — output batch assembly | 1.2 | 0.2% |
| Pass 2 — writer (Arrow→Parquet + ZSTD-3) | 7.6 | 1.0% |
| Writer finish (footer) | 0.0 | 0.0% |

Per level (pass 2; `rows` = rows written; duplicating mode simplifies the
*cumulative* winner set at each non-canonical level from full resolution):

| Level | GSD (m) | Rows | Total (s) | Read | Decode | Simplify | Build | Write |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | 4892.0 | 1 | 0.8 | 0.76 | 0.00 | 0.00 | 0.00 | 0.00 |
| 1 | 2446.0 | 41 | 6.6 | 0.79 | 0.00 | 5.78 | 0.01 | 0.01 |
| 2 | 1223.0 | 994 | 26.2 | 0.77 | 0.02 | 25.32 | 0.02 | 0.06 |
| 3 | 611.5 | 7,804 | 56.6 | 0.80 | 0.09 | 55.51 | 0.04 | 0.19 |
| 4 | 305.7 | 18,633 | 80.7 | 0.83 | 0.14 | 79.34 | 0.05 | 0.31 |
| 5 | 152.9 | 31,149 | 95.0 | 0.79 | 0.17 | 93.55 | 0.07 | 0.42 |
| 6 | 76.4 | 51,559 | 107.4 | 0.79 | 0.22 | 105.74 | 0.09 | 0.56 |
| 7 | 38.2 | 85,209 | 113.1 | 0.78 | 0.24 | 111.33 | 0.10 | 0.64 |
| 8 | 19.1 | 140,670 | 92.9 | 0.78 | 0.26 | 91.05 | 0.11 | 0.65 |
| 9 | 9.6 | 232,107 | 24.4 | 0.75 | 0.30 | 22.46 | 0.12 | 0.78 |
| 10 | 4.8 | 382,976 | 117.7 | 0.77 | 0.36 | 114.51 | 0.28 | 1.77 |
| 11 (verbatim) | 2.4 | 631,910 | 3.7 | 0.77 | 0.42 | 0.01 | 0.29 | 2.17 |

Observations:

- **Simplification is everything.** The canonical level (631,910 rows,
  verbatim) takes 3.7 s end-to-end — reading, decoding, and ZSTD-writing the
  whole dataset costs almost nothing. Every non-canonical level costs 24–118 s,
  >98% of it inside `simplify_for_level`.
- Cost is *not* proportional to row count: level 1 spends 5.8 s on **41
  features** (141 ms/feature). Coarse-level winners are the largest
  geometries, and per-feature cost is superlinear in vertex count (§4).
- Level 9 is an anomaly (22.5 s for 232k rows vs 91 s for 141k at level 8);
  its winner increment is dominated by small, few-vertex features. Level 10
  jumps back up (114.5 s).

## 3. Export phase breakdown (total wall 271.8 s)

| Phase | Wall (s) | Share |
|---|---:|---:|
| Read + decode level bands | 3.4 | 1.3% |
| **Clip + tile grouping (`clip_geometry` per feature×tile)** | **255.5** | **94.0%** |
| MVT encode | 7.4 | 2.7% |
| Writer add_tile (gzip) | 4.6 | 1.7% |
| PMTiles finalize (directories) | 0.1 | 0.0% |

Per zoom:

| Zoom (level) | Feats | Tiles | Read | Clip+group | MVT | Write |
|---|---:|---:|---:|---:|---:|---:|
| z3 (0) | 1 | 1 | 0.00 | 0.00 | 0.00 | 0.00 |
| z4 (1) | 41 | 1 | 0.01 | 0.34 | 0.00 | 0.00 |
| z5 (2) | 994 | 1 | 0.04 | 1.68 | 0.03 | 0.02 |
| z6 (3) | 7,804 | 2 | 0.08 | 3.51 | 0.09 | 0.07 |
| z7 (4) | 18,633 | 4 | 0.12 | 4.96 | 0.16 | 0.15 |
| z8 (5) | 31,149 | 11 | 0.17 | 6.67 | 0.24 | 0.25 |
| z9 (6) | 51,559 | 27 | 0.23 | 8.21 | 0.30 | 0.35 |
| z10 (7) | 85,209 | 76 | 0.25 | 10.01 | 0.39 | 0.40 |
| z11 (8) | 140,670 | 252 | 0.27 | 11.06 | 0.49 | 0.37 |
| z12 (9) | 232,107 | 901 | 0.34 | 2.80 | 0.73 | 0.39 |
| z13 (10) | 382,976 | 3,363 | 0.82 | 38.01 | 1.77 | 1.01 |
| z14 (11) | 631,910 | 12,733 | 1.08 | 168.20 | 3.18 | 1.54 |

z13+z14 clip alone = 206 s = 76% of the export. At z14 the 631,910 features
produce only 763,308 clipped copies (~21% seam duplication) — i.e. **~80% of
features fall entirely inside one tile yet still pay a full BooleanOps
intersection**. A bbox-containment fast path (feature bbox ⊂ buffered tile
bounds ⇒ keep geometry unclipped) would skip most of that work. The same
level-9/z12 anomaly appears here (2.8 s vs 11.1 s at z11).

## 4. Inside "simplify": validation, not RDP

Micro-benchmark on the first 20,000 Moldova polygons (1.10 M exterior
vertices), timing the three components of `overview::simplify`'s polygon path
separately at real level GSDs (EPSG:4326 tolerances):

| GSD (m) | RDP `simplify()` | `is_valid()` on candidate | `is_valid()` on original (fallback path) | invalid candidates |
|---:|---:|---:|---:|---:|
| 152.87 | 0.04 s | 0.08 s | 3.98 s | 1,859 |
| 76.44 | 0.05 s | 0.06 s | 4.03 s | 1,766 |
| 38.22 | 0.05 s | 0.07 s | 3.83 s | 1,267 |
| 19.11 | 0.06 s | 0.12 s | 2.97 s | 652 |
| 9.55 | 0.10 s | 0.78 s | 0.00 s | 3 |
| 4.78 | 0.15 s | 4.01 s | 0.00 s | 0 |

- **RDP itself is 1–4% of simplify cost.** The rest is `geo::Validation`
  (self-intersection checking, superlinear in ring vertices): at fine GSD the
  candidate check dominates (many vertices survive), at coarse GSD 3–9% of
  candidates come out self-intersecting and trigger a *full-resolution*
  `is_valid()` on the original — the dominant term.
- Side observation (correctness/size, not perf): those invalid candidates take
  the boundary-preserving fallback, i.e. coarse levels quietly carry ~5–9% of
  features at **full resolution**. This is likely why coarse-level
  vertex/byte counts are higher than expected.
- The absolute numbers here (≈4 s/20k) are much smaller than pass 2's per-level
  cost because level winners at coarse levels are the *largest* features, not
  a file-order sample; the split (validation ≫ RDP) is the robust finding.

## 5. Redundant per-level decode + simplify (cascade potential)

Duplicating mode re-reads the file and re-processes the cumulative winner set
at every level: 1,583,053 rows processed for 631,910 inputs (**2.5× row
redundancy**; winner counts per level in §2).

- Redundant **decode** is worthless to eliminate: all 12 re-reads plus decode
  total 11.6 s = 1.6% of convert wall.
- Redundant **simplify** is the real cost: every non-canonical level
  re-simplifies its winners from full resolution, so the big coarse-level
  features are re-RDP'd and re-validated up to 11 times (levels 1–10 sum to
  704.6 s). A cascade (simplify level k from level k+1's already-simplified
  output) shrinks both the RDP input and — more importantly — the vertex count
  seen by validation. Estimated from the per-level output vertex table
  (level outputs sum to ~113 M vertices vs ~240 M full-res vertex re-reads):
  roughly **1.5–3× on the simplify share**, more where validation's
  superlinearity bites. Caveat: cascading changes output geometry slightly
  (RDP is not idempotent across tolerances) — a documented divergence would be
  needed.

## 6. Ranked recommendation

Amdahl ceilings assume 16 cores and the measured serial shares.

1. **Rayon parallelism in pass 2 (lever 1) — do first.** Simplify is 96.9% of
   convert wall and embarrassingly parallel per feature/batch. Ceiling
   ≈ 1/(0.031 + 0.969/16) ≈ **10.6× convert** (12 min → ~70 s). Mechanical,
   no output change.
2. **Export per-tile / per-feature parallelism (lever 3) — same PR family.**
   Clip is 94.0% of export wall, parallel per feature (grouping) or per tile
   (encode). Ceiling ≈ 1/(0.06 + 0.94/16) ≈ **8.4× export** (4:32 → ~32 s).
3. **Cut validation cost in `overview::simplify` (new lever, found here).**
   RDP is 1–4% of simplify; `is_valid()` (candidate + full-res fallback) is
   the rest. Options: bail out of validation early, use a topology-preserving
   simplifier, or validate only the touched rings. Potential is another
   **~5–10× on the simplify share**, multiplicative with lever 1 — together
   they could put convert near the 15–20 s I/O+write floor.
4. **Export clip bbox fast path (new lever).** ~80% of z14 features are
   fully interior to one tile; skipping BooleanOps for bbox-contained features
   plausibly cuts clip 2–5×, multiplicative with lever 2.
5. **Cascade simplification (lever 2).** Est. 1.5–3× on simplify only, with a
   behavioral divergence to document. Worth doing *after* 1+3; redundant
   decode elimination alone is worthless (1.6% of wall).
6. **Compression level (lever 4) — skip.** ZSTD-3 writing is 1.0% of convert;
   gzip tiles are 1.7% of export. No headroom.

## Reproduction

```bash
cargo build --release
RUST_LOG=tylertoo_core::overview=debug \
  /usr/bin/time -v target/release/tylertoo overview \
  corpus/data/gpio/polygons-ftw-moldova-large.parquet \
  /tmp/moldova.dup.parquet \
  --mode duplicating --min-zoom 0 --max-zoom 14
RUST_LOG=tylertoo_core::overview=debug \
  /usr/bin/time -v target/release/tylertoo export-pmtiles \
  /tmp/moldova.dup.parquet /tmp/moldova.pmtiles \
  --layer-name fields
```

Phase lines are logged as `[profile] …` at debug level. This run: convert
726.8 s / 335 MB peak RSS; export 271.8 s / 2.06 GB peak RSS (matches the
uninstrumented H3 baselines within noise).

## 7. Results after levers 1–4 (2026-07-03)

Same dataset, same commands, same machine (16 cores), release build, after
implementing the four ranked levers (§6 items 1–4; the cascade, §6 item 5,
remains open):

- **Lever 3 — validation cuts in `overview::simplify`**: skip candidate
  `is_valid()` when RDP removed no vertices; never re-validate the original
  on the fallback path; **fallback bug fixed** — an invalid RDP candidate now
  retries at `eps/2, eps/4, eps/8` and only keeps the original geometry when
  every retry self-intersects (counted, logged at debug level).
- **Lever 1 — rayon in convert pass 2**: per-feature simplify parallelized
  within each read batch (order-preserving collect; writer single-threaded).
- **Lever 4 — export bbox fast path**: features whose bbox lies inside the
  buffered tile bounds skip the BooleanOps clip entirely.
- **Lever 2 — rayon in export**: per-feature clip and per-tile MVT encode
  parallelized (order-preserving; grouping merge and write loop serial).

### Wall / memory (Moldova, duplicating z0–14)

Baselines are the H3 artifacts in `corpus/data/bench/h3/` (same commands).

| Command | Wall before | Wall after | Speedup | RSS before | RSS after |
|---|---:|---:|---:|---:|---:|
| `overview` (convert) | 706.1 s | **55.3 s** | **12.8×** | 305.7 MB | 320.2 MB |
| `export-pmtiles` | 288.5 s | **58.8 s** | **4.9×** | 2.16 GB | 2.38 GB |

CPU utilization: convert 557 %, export 828 % (of 1600 %). Output validation:
all 9 `tylertoo validate` checks pass; export reports **0 oversized tiles**
(17,372 tiles in both runs).

### Output size drop (the fallback fix, intended)

Coarse/mid levels no longer quietly carry 3–9 % of features at full
resolution. Features kept at full resolution (all epsilon retries
self-intersecting): **3,664 total** — level 1: 7, level 2: 120, level 3: 633,
level 4: 1,401, level 5: 1,502, level 6: 1, levels 7–10: 0 — versus the old
behaviour where *every* invalid first candidate (e.g. ~9 % of a coarse-level
sample, §4) fell back.

| | Before | After | Δ |
|---|---:|---:|---:|
| Overview parquet | 360.7 MB | 293.6 MB | **−18.6 %** |
| Total vertices | 118.3 M | 87.7 M | −25.9 % |
| Level 6 vertices | 8.45 M | 1.35 M | −84 % |
| Level 7 vertices | 9.76 M | 2.18 M | −78 % |
| PMTiles archive | 130.7 MB | 123.3 MB | −5.7 % |

Row-count deltas are tiny: 1,583,053 → 1,583,041 (−12 rows; retry candidates
whose shoelace area collapsed below the sliver gate are now dropped instead
of being kept at full resolution) and 1,807,912 → 1,807,901 exported tile
features (−11, downstream of the same rows).

### Where the remaining time goes (vs the Amdahl ceilings)

- Convert (ceiling ~10.6× from lever 1 alone; achieved 12.8× because lever 3
  multiplies): level 10 is now the bottleneck (26.1 s of the 53.6 s pass 2 —
  candidate `is_valid()` on 383 k nearly-full-resolution polygons, imperfectly
  load-balanced across the per-batch parallel sections). Read+decode+write
  floor is ~12 s serial.
- Export (ceiling ~8.4×; achieved 4.9×): z14 clip+group fell 168.2 s → 42.2 s
  (4×, not the naive 16×·5×): the bbox fast path replaces most clips with a
  geometry clone + regroup that is allocation-bound, and the per-zoom merge
  into the `BTreeMap` stays serial. Export RSS rose ~10 % (per-feature clip
  results are staged before the serial merge).
- The §5 cascade (simplify level k from level k+1) remains the next lever for
  convert level 10.
