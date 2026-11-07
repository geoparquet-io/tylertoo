# e2e GeoParquet → PMTiles: tylertoo vs pinned tippecanoe — measured

Method, settings-parity mapping and the full asymmetry list are in
[README.md](README.md). **Read that before quoting anything here.**

Every timing, memory, size and tile-count table below sits in a
generated region (marked `bench:NAME` in an HTML comment) that `scripts/gen_bench_docs.py` rewrites from
[`../results/e2e-fixtures.json`](../results/e2e-fixtures.json) (fixtures) or
[`../results/e2e-corpus.json`](../results/e2e-corpus.json) (the corpus tier,
§1b); each ends with its commit, versions, host and date. CI fails if a region
differs from what the generator writes. The per-zoom *feature* counts in §2
come from `render_parity.py` (PMTiles decoding, which `tylertoo stats` does
not do). Figures from an earlier run that is **not** in the results files
appear only in §4 and §7, and are labelled there as such. Prose around the
tables quotes numbers by hand; they match the tables as of the recorded run.

To update: rerun the harness, copy its JSON into `benchmarks/results/` with a
`provenance` block, then run `uv run python scripts/gen_bench_docs.py`.
`benchmarks/e2e/format_results.py` still prints the same tables for an ad-hoc
results file.

Issue: [#447](https://github.com/geoparquet-io/tylertoo/issues/447).

## Provenance

<!-- bench:e2e-fixtures/provenance -->
- **Machine** — Apple M3 Pro, 12 cores, 18 GB RAM, macOS-15.6-arm64-arm-64bit
- **tylertoo** — tylertoo 0.11.0 @ `044360d` (release build)
- **tippecanoe** — tippecanoe v2.79.0, tag `2.79.0`, commit `68ab8dcc229f95b8b25877697d5e8d66783af503`, built from source by `setup_tippecanoe.sh`
- **Settings** — z0–z14, tile buffer 8, per-tile cap 500,000 bytes
- **Method** — median of 3 timed runs after one discarded warm-up; warm page cache
- **Recorded** — 2026-09-26T20:41:10Z → `e2e-fixtures.json`
<!-- /bench -->

`044360d` includes #559 (export speed-up) and predates #564 (parallel level
assignment); later tylertoo builds are not reflected here.

A laptop is not a cluster. These numbers are a floor for reproducibility, not
a claim about server-class hardware; `--dataset ID=PATH` runs the identical
harness on a bigger file when someone has one.

## What to quote

> On a 17,465-feature MultiPolygon admin dataset, at near-parity output
> (tile counts within 2 per zoom; features within 0.4% at z8 and ~5% at z4;
> 10 tiles hit tylertoo's size valve), tylertoo built a z0–z14 PMTiles
> archive **≈7.5× faster** than tippecanoe 2.79.0 reading FlatGeobuf
> (2.67 s vs 20.05 s), in an archive 22% smaller, at roughly 2× tippecanoe's
> peak RSS (835 MB vs 413 MB). Counting the GeoParquet→FlatGeobuf conversion
> a tippecanoe user has to run, and with tylertoo at its own defaults (which
> emit a thinned map — see below), the end-to-end figure in this run is
> **≈12.5×**.

That is one recorded run of three warm repeats on a laptop. Treat the ratios
as approximate, not as two-significant-figure facts; §7 has the per-repeat
spread.

At engine scale (the corpus tier, §1b: 631k–1.2M features per dataset,
both tools in their production configuration, recorded on an idle 64-core
arm64 host), the measured end-to-end ratios are **17.0×–23.0×** across the
three datasets where both tools built the same zoom range, including
**21.5× on the 632k-polygon Moldova parcels set that the pre-H3 pipeline
lost 3.6× on**. The points row measured 27.2×, but tippecanoe built two zoom
levels there that tylertoo did not, so it is quoted separately rather than
as the top of the range (caveat 4); the rest of the caveats are in §1b.

## 1. Headline (both tools at their own defaults)

<!-- bench:e2e-fixtures/headline -->
| dataset | features | tylertoo (parquet→pmtiles) | tippecanoe e2e (convert+tile) | tippecanoe tile-only (fgb in) | e2e | tile-only |
|---|---|---|---|---|---|---|
| `madagascar-adm4` | 17,465 | 1.61 s | 20.24 s | 20.05 s | **12.5×** | **12.4×** |
| `open-buildings` | 1,000 | 0.03 s | 0.28 s | 0.22 s | **9.3×** | **7.2×** |
| `road-detections` | 1,000 | 0.04 s | 0.28 s | 0.21 s | **6.8×** | **5.3×** |

_Run: commit 044360d, tylertoo 0.11.0, tippecanoe v2.79.0, Apple M3 Pro, 18 GB RAM, macOS-15.6-arm64-arm-64bit, 2026-09-26._
<!-- /bench -->

**`madagascar-adm4` is the only row that measures tiling.** The other two are
1,000-feature fixtures where both tools spend most of their wall time on
process start-up and pyramid scaffolding; they are the geometry-class smoke
test (Polygon, LineString) and nothing more. Do not average these rows.

**This table is not an output-parity comparison.** See §2.

## 1b. The corpus tier — engine-scale datasets (#447)

Everything above and below this section is the committed fixture run
(`../results/e2e-fixtures.json`). This section is the committed **corpus** run
(`../results/e2e-corpus.json`): the `corpus/manifest.json` datasets at 631k–1.2M
features, where tiling throughput, not process start-up, is what is measured.
Tables are generated from `e2e-corpus.json`. Reproduce with one command: `./benchmarks/e2e/run_corpus.sh`.

### Provenance and caveats — read these before quoting

<!-- bench:e2e-corpus/provenance -->
- **Machine** — Neoverse-V1, 64 cores, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39
- **tylertoo** — tylertoo 0.8.0 @ `ae0363f` (release build)
- **tippecanoe** — tippecanoe v2.79.0, tag `2.79.0`, commit `68ab8dcc229f95b8b25877697d5e8d66783af503`, built from source by `setup_tippecanoe.sh`
- **Settings** — z0–z14, tile buffer 8, per-tile cap 500,000 bytes
- **Method** — median of 3 timed runs after one discarded warm-up; warm page cache
- **Recorded** — 2026-11-06T19:06:34Z → `e2e-corpus.json`
<!-- /bench -->

1. **Per-repeat spread.** No other job ran during the recording (checked
   by hand before it started; `run_e2e.py` has no load gate and records no
   load figure). Spread (max − min over min, from
   `wall_all_s` in `../results/e2e-corpus.json`) is ≤5.3% on every
   measurement except two tylertoo runs: Oregon roads at defaults (16.8%)
   and Moldova quality-matched (13.7%). tippecanoe and the FlatGeobuf
   conversions stay under 1%.
2. **The Overture inputs are pinned** to release `2026-10-21.0`, with each
   extract's ids hashed in `corpus/manifest.json` (#696). The Moldova
   parcels file is a stable direct download.
3. **tippecanoe runs the manifest's recorded flags** (the `flags` table
   below — density dropping, `--simplification=10`) on top of the parity
   set, and tylertoo runs its own defaults: both tools in their production
   configuration for this data, not both at bare defaults. See README.md,
   "Datasets". Exact tile parity is neither expected nor claimed; the
   per-zoom counts below are the record of the difference.
4. **The points row's tippecanoe archive extends past z14**: its recorded
   `--extend-zooms-if-still-dropping` added z15–z16 (9,694 of its 10,543
   tiles). Its wall time includes that extra work; tylertoo stops at the
   requested z14. Read that row accordingly.

### Headline (tylertoo defaults vs tippecanoe with its recorded flags)

<!-- bench:e2e-corpus/headline -->
| dataset | features | tylertoo (parquet→pmtiles) | tippecanoe e2e (convert+tile) | tippecanoe tile-only (fgb in) | e2e | tile-only |
|---|---|---|---|---|---|---|
| `points-nyc-medium` | 630,065 | 1.57 s | 42.66 s | 40.91 s | **27.2×** | **26.1×** |
| `polygons-portland-medium` | 813,456 | 3.68 s | 84.81 s | 82.47 s | **23.0×** | **22.4×** |
| `lines-oregon-large` | 1,203,997 | 10.65 s | 180.69 s | 176.98 s | **17.0×** | **16.6×** |
| `polygons-ftw-moldova-large` | 631,910 | 7.78 s | 167.27 s | 163.57 s | **21.5×** | **21.0×** |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

The Moldova row is the one the stale table in
[`../overview/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/139aa99/benchmarks/overview/RESULTS.md) (in git history) recorded at **3.6×
slower** (pre-H3, in-memory pipeline, 5.4 GB peak RSS). The streaming
pipeline is 21.5× faster than tippecanoe end-to-end on the same file at
4.5 GB peak RSS (64-core host; peak RSS grows with core count).

### Quality-matched (thinning ladder off)

<!-- bench:e2e-corpus/quality-matched -->
| dataset | tylertoo default | tylertoo quality-matched | tippecanoe tile-only | tile-only speedup, matched | tylertoo tiles (default → matched) | tippecanoe tiles | archive, matched | archive ratio |
|---|---|---|---|---|---|---|---|---|
| `points-nyc-medium` | 1.57 s | 18.38 s | 40.91 s | **2.2×** | 851 → 851 | 10,543 | 79.9 MB | 0.88× |
| `polygons-portland-medium` | 3.68 s | 6.17 s | 82.47 s | **13.4×** | 1,214 → 1,216 | 1,213 | 103.2 MB | 1.47× |
| `lines-oregon-large` | 10.65 s | 10.47 s | 176.98 s | **16.9×** | 121,259 → 121,259 | 120,984 | 341.8 MB | 1.23× |
| `polygons-ftw-moldova-large` | 7.78 s | 9.35 s | 163.57 s | **17.5×** | 17,492 → 17,492 | 17,461 | 175.9 MB | 1.01× |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

Peak RSS on the matched runs, which the table above leaves out
(`tylertoo_quality_matched.peak_rss_bytes` vs `tippecanoe_fgb` in
`../results/e2e-corpus.json`): 4109 MB vs 2553 MB on lines (1.61×), 3775 MB
vs 2230 MB on Portland buildings (1.69×), 4928 MB vs 2549 MB on Moldova
(1.93×), and 3553 MB vs 1418 MB on points (2.51×, on the same
zoom-mismatched run as caveat 4). Turning the thinning ladder off costs
memory as well as time.

Two honest readings this table forces:

- **The points quality-matched row is 2.2×, the weakest ratio measured
  anywhere in this harness** — `--verbatim` makes tylertoo carry all 630k
  points at every zoom (1.57 s → 18.38 s), while tippecanoe's side is also
  doing non-comparable extra work (z15–z16, caveat 4). Treat it as a bound,
  not a parity measurement; for points the defaults row is the realistic one,
  since both tools' production configs drop points by design.
- **tylertoo's archives are bigger at a matched zoom range on lines and
  Portland buildings** (1.23× and 1.47× matched; 1.12× and 1.41× at
  defaults), and equal on Moldova (1.01×; 1.00×). The points row's 0.65× at
  defaults is **not** a like-for-like figure: it divides tylertoo's z0–14
  archive by tippecanoe's z0–16 one. Compared at z0–14 only — 28.0 MB of
  tippecanoe's 90.6 MB — tylertoo's points archive is **2.1× bigger**,
  the largest relative archive in the set.

Unlike the fixture set (contiguous admin coverage, where tippecanoe's
defaults keep nearly everything and tylertoo's thin visibly), here
tippecanoe is running its recorded density-dropping flags, so default
tylertoo vs flagged tippecanoe is already a thinned-vs-thinned comparison —
and the per-zoom tile counts below agree within a few tiles per zoom on
lines and polygons even at defaults.

### Memory, CPU, archive size

<!-- bench:e2e-corpus/memory -->
| dataset | tylertoo peak RSS | tippecanoe peak RSS | converter peak RSS | tylertoo CPU | tippecanoe CPU | tylertoo archive | tippecanoe archive | archive ratio |
|---|---|---|---|---|---|---|---|---|
| `points-nyc-medium` | 2210 MB | 1352 MB | 332 MB | 2522% | 240% | 58.8 MB | 90.6 MB | 0.65× † |
| `polygons-portland-medium` | 3132 MB | 2127 MB | 356 MB | 2639% | 219% | 99.1 MB | 70.2 MB | 1.41× |
| `lines-oregon-large` | 9051 MB | 2435 MB | 397 MB | 1861% | 242% | 311.5 MB | 278.7 MB | 1.12× |
| `polygons-ftw-moldova-large` | 4520 MB | 2431 MB | 353 MB | 2904% | 297% | 174.2 MB | 174.8 MB | 1.00× |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

† Zoom-mismatched: tippecanoe's archive covers z0–16, tylertoo's z0–14
(caveat 4). At z0–14 on both sides the ratio is 2.1×, not 0.65×.

**tylertoo loses the memory column on all four rows** — up to 3.7× on
lines (9.1 GB vs 2.4 GB) and 1.9× on Moldova (4.5 GB vs 2.4 GB). The CPU
column says where the speed comes from: tylertoo keeps 19–29 of the 64
cores busy, tippecanoe 2.2–3.0. The speed is
parallelism plus the native columnar read, bought with a larger working set
(`--partition-wave auto` sizes itself against available RAM; pin
`--partition-wave N` before drawing memory conclusions).

### Where tippecanoe's e2e time goes, and tylertoo's

GeoParquet→FlatGeobuf conversion is 2–4% of tippecanoe's end-to-end wall on
every corpus dataset (1.75–3.71 s; `gpio` was the fastest converter on all
four) — at this scale the ratio is
tiler vs tiler, not input format. tylertoo's own split:

<!-- bench:e2e-corpus/phases -->
| dataset | convert (parquet read + ladder) | export (MVT + archive) | row groups read/total | tiles | tile features | oversized tiles |
|---|---|---|---|---|---|---|
| `points-nyc-medium` | 0.40 s | 1.05 s | 13/13 | 851 | 1,256,277 | 25 |
| `polygons-portland-medium` | 0.88 s | 2.71 s | 17/17 | 1,214 | 2,456,008 | 44 |
| `lines-oregon-large` | 2.73 s | 8.91 s | 25/25 | 121,259 | 6,482,919 | 27 |
| `polygons-ftw-moldova-large` | 2.66 s | 4.98 s | 13/13 | 17,492 | 3,769,912 | 26 |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

(The defaults runs hit tylertoo's per-tile size valve on 25–44 tiles per
dataset — the `oversized tiles` column; tippecanoe's flagged runs handle the
same pressure with `--drop-densest-as-needed`.)

### The recorded tippecanoe flags

<!-- bench:e2e-corpus/flags -->
| dataset | zooms | tippecanoe flags beyond the parity set |
|---|---|---|
| `points-nyc-medium` | z0–z14 | `--drop-densest-as-needed --extend-zooms-if-still-dropping` |
| `polygons-portland-medium` | z0–z14 | `--simplification=10 --drop-densest-as-needed --coalesce-densest-as-needed` |
| `lines-oregon-large` | z0–z14 | `--simplification=10 --drop-densest-as-needed` |
| `polygons-ftw-moldova-large` | z0–z14 | `--simplification=10 --drop-densest-as-needed --coalesce-densest-as-needed` |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

### Per-zoom tile counts (defaults run; the tile-parity record)

Exact parity is not expected — different thinning models — and these counts
are the record of how far apart the two archives are. On lines and polygons
they agree within a few tiles per zoom; the systematic differences are
tylertoo's empty coarse zooms (it emits no tile where generalization leaves
nothing; tippecanoe writes a nearly-empty one) and tippecanoe's z15–z16
extension on points (caveat 4).

#### `points-nyc-medium`

<!-- bench:e2e-corpus/per-zoom/points-nyc-medium -->
| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z0 | 1 | 1 | 0.00 MB | 0.00 MB |
| z1 | 1 | 1 | 0.00 MB | 0.00 MB |
| z2 | 1 | 1 | 0.00 MB | 0.00 MB |
| z3 | 2 | 2 | 0.00 MB | 0.00 MB |
| z4 | 2 | 2 | 0.00 MB | 0.01 MB |
| z5 | 2 | 2 | 0.01 MB | 0.02 MB |
| z6 | 2 | 2 | 0.03 MB | 0.02 MB |
| z7 | 2 | 1 | 0.07 MB | 0.05 MB |
| z8 | 1 | 1 | 0.27 MB | 0.12 MB |
| z9 | 2 | 2 | 0.65 MB | 0.31 MB |
| z10 | 6 | 6 | 1.90 MB | 0.66 MB |
| z11 | 16 | 16 | 4.25 MB | 1.21 MB |
| z12 | 50 | 49 | 8.78 MB | 2.46 MB |
| z13 | 160 | 160 | 14.94 MB | 6.49 MB |
| z14 | 603 | 603 | 27.88 MB | 16.64 MB |
| z15 | 0 | 2,146 | — | 25.81 MB |
| z16 | 0 | 7,548 | — | 36.80 MB |
| **total** | **851** | **10,543** | | |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

#### `lines-oregon-large`

<!-- bench:e2e-corpus/per-zoom/lines-oregon-large -->
| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z0 | 1 | 1 | 0.00 MB | 0.42 MB |
| z1 | 1 | 1 | 0.01 MB | 0.41 MB |
| z2 | 1 | 1 | 0.03 MB | 0.45 MB |
| z3 | 2 | 2 | 0.12 MB | 0.38 MB |
| z4 | 1 | 1 | 0.40 MB | 0.38 MB |
| z5 | 2 | 2 | 0.57 MB | 0.52 MB |
| z6 | 6 | 6 | 1.67 MB | 0.77 MB |
| z7 | 12 | 12 | 3.46 MB | 2.16 MB |
| z8 | 42 | 42 | 7.59 MB | 5.54 MB |
| z9 | 124 | 124 | 14.11 MB | 10.20 MB |
| z10 | 450 | 450 | 23.18 MB | 17.13 MB |
| z11 | 1,684 | 1,683 | 35.10 MB | 27.38 MB |
| z12 | 6,413 | 6,407 | 48.88 MB | 48.46 MB |
| z13 | 24,533 | 24,507 | 64.69 MB | 70.06 MB |
| z14 | 87,987 | 87,745 | 111.51 MB | 94.22 MB |
| **total** | **121,259** | **120,984** | | |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

#### `polygons-portland-medium`

<!-- bench:e2e-corpus/per-zoom/polygons-portland-medium -->
| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z1 | 0 | 1 | — | 0.00 MB |
| z2 | 1 | 1 | 0.00 MB | 0.00 MB |
| z3 | 1 | 1 | 0.00 MB | 0.00 MB |
| z4 | 1 | 1 | 0.00 MB | 0.01 MB |
| z5 | 1 | 1 | 0.01 MB | 0.03 MB |
| z6 | 1 | 1 | 0.02 MB | 0.13 MB |
| z7 | 1 | 1 | 0.09 MB | 0.45 MB |
| z8 | 2 | 2 | 0.35 MB | 0.24 MB |
| z9 | 4 | 4 | 0.88 MB | 0.17 MB |
| z10 | 9 | 9 | 1.95 MB | 0.21 MB |
| z11 | 20 | 20 | 5.77 MB | 1.28 MB |
| z12 | 63 | 63 | 16.84 MB | 5.06 MB |
| z13 | 236 | 236 | 32.33 MB | 24.72 MB |
| z14 | 874 | 872 | 40.80 MB | 37.93 MB |
| **total** | **1,214** | **1,213** | | |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

#### `polygons-ftw-moldova-large`

<!-- bench:e2e-corpus/per-zoom/polygons-ftw-moldova-large -->
| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z0 | 1 | 1 | 0.00 MB | 0.01 MB |
| z1 | 1 | 1 | 0.00 MB | 0.03 MB |
| z2 | 1 | 1 | 0.02 MB | 0.10 MB |
| z3 | 1 | 1 | 0.06 MB | 0.34 MB |
| z4 | 1 | 1 | 0.22 MB | 0.40 MB |
| z5 | 1 | 1 | 0.46 MB | 0.41 MB |
| z6 | 2 | 2 | 0.87 MB | 0.60 MB |
| z7 | 4 | 4 | 1.40 MB | 1.02 MB |
| z8 | 11 | 11 | 2.59 MB | 1.77 MB |
| z9 | 28 | 28 | 6.21 MB | 4.88 MB |
| z10 | 78 | 76 | 13.13 MB | 9.90 MB |
| z11 | 255 | 254 | 22.35 MB | 21.37 MB |
| z12 | 905 | 905 | 33.19 MB | 31.92 MB |
| z13 | 3,385 | 3,380 | 43.36 MB | 43.02 MB |
| z14 | 12,818 | 12,795 | 50.27 MB | 59.02 MB |
| **total** | **17,492** | **17,461** | | |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

## 2. Output parity — the number to quote to a skeptic

At defaults the two tools do not draw the same map. tylertoo's density budget
thins features hard at coarse and mid zooms. tippecanoe's rate-based dropping
applies only to points at defaults; its tiny-polygon reduction (disabled by
`-pt`) still removes small polygons at low zooms — 6,102 of 17,465 survive at
z4 — but it reaches every feature by z8. Measured with `render_parity.py`
(distinct feature ids per zoom, so cross-tile duplication does not inflate
either side):

| zoom | tylertoo, defaults | tylertoo, quality-matched | tippecanoe |
|---|---|---|---|
| z4 | 254 | 5,786 | 6,102 |
| z8 | **865** | 17,406 | **17,465** |
| z11 | 3,888 | 17,465 | 17,465 |

_(from `render_parity.py --zooms 4,8,11`, not from `../results/e2e-fixtures.json` — counting
features means decoding tiles.)_

At z8 tylertoo's default archive carries 865 of 17,465 admin-4 polygons. That
is not a subtle difference — rendered side by side, the default output is a
scatter of disconnected polygons where tippecanoe's is a complete
choropleth. A speed ratio against that is a ratio against less work.

So the harness also runs tylertoo with the thinning ladder switched off
(`--verbatim --simplify-factor 1.0`, which keeps simplification because
tippecanoe simplifies too). That reaches near-parity, not identical output:
within 2 tiles of tippecanoe's count at every zoom, within 0.4% of its
feature count at z8 (17,406 vs 17,465) and about 5% at z4 (5,786 vs 6,102),
and 10 of its tiles hit tylertoo's per-tile size valve (`oversized_tiles` in
`../results/e2e-fixtures.json`):

<!-- bench:e2e-fixtures/quality-matched -->
| dataset | tylertoo default | tylertoo quality-matched | tippecanoe tile-only | tile-only speedup, matched | tylertoo tiles (default → matched) | tippecanoe tiles | archive, matched | archive ratio |
|---|---|---|---|---|---|---|---|---|
| `madagascar-adm4` | 1.61 s | 2.67 s | 20.05 s | **7.5×** | 146,924 → 153,052 | 153,047 | 118.0 MB | 0.78× |
| `open-buildings` | 0.03 s | 0.03 s | 0.22 s | **7.1×** | 82 → 82 | 90 | 0.1 MB | 0.70× |
| `road-detections` | 0.04 s | 0.04 s | 0.21 s | **5.6×** | 78 → 79 | 82 | 0.2 MB | 0.80× |

_Run: commit 044360d, tylertoo 0.11.0, tippecanoe v2.79.0, Apple M3 Pro, 18 GB RAM, macOS-15.6-arm64-arm-64bit, 2026-09-26._
<!-- /bench -->

**≈7.5× at output parity on the one dataset large enough to mean anything**,
with a 22% smaller archive. Turning the ladder off costs tylertoo 66% more
wall time (1.61 s → 2.67 s) and buys the missing 4.0% of tiles and ~16,600
missing mid-zoom features.

Regenerate the renders (they are not committed):

```bash
python3 run_e2e.py --only madagascar-adm4 --quality-matched \
    --work /tmp/e2e --keep
uv run --with pmtiles --with mapbox-vector-tile --with matplotlib \
    python3 render_parity.py /tmp/e2e/madagascar-adm4.tylertoo-qm.pmtiles \
        /tmp/e2e/madagascar-adm4.tippecanoe-fgb.pmtiles --zooms 4,8,11 \
        --out /tmp/parity-qm
```

Visual note, by eye at z4/z8/z11: the quality-matched output is
indistinguishable from tippecanoe's in coverage and boundary structure.
tylertoo's rings are slightly smoother at the same zoom — its RDP tolerance is
metres-of-GSD where tippecanoe's is tile units — which is consistent with the
smaller archive and is the remaining, unclosable simplification asymmetry.

## 3. Where tippecanoe's end-to-end time actually goes

<!-- bench:e2e-fixtures/input -->
| dataset | parquet in | fgb out | conversion (fastest tool) | tool | conversion share of tippecanoe e2e | ldGeoJSON out | ldGeoJSON convert | tippecanoe -P on ldGeoJSON |
|---|---|---|---|---|---|---|---|---|
| `madagascar-adm4` | 27.1 MB | 43.2 MB | 0.19 s | `ogr2ogr` | 1% | 75.0 MB | 2.07 s | 23.59 s |
| `open-buildings` | 0.1 MB | 0.3 MB | 0.06 s | `ogr2ogr` | 22% | 0.4 MB | 0.07 s | 0.21 s |
| `road-detections` | 0.1 MB | 0.2 MB | 0.06 s | `ogr2ogr` | 23% | 0.3 MB | 0.07 s | 0.21 s |

_Run: commit 044360d, tylertoo 0.11.0, tippecanoe v2.79.0, Apple M3 Pro, 18 GB RAM, macOS-15.6-arm64-arm-64bit, 2026-09-26._
<!-- /bench -->

**The input-format advantage is small on this dataset, and saying so matters.**
On `madagascar-adm4` the GeoParquet→FlatGeobuf conversion is 1% of
tippecanoe's end-to-end wall; the e2e and tile-only ratios are 12.5× and
12.4×. The native-parquet read is a real product advantage — no extra file,
no 43 MB of intermediate on disk, row-group and bbox pushdown that a
FlatGeobuf stream cannot offer — but on this fixture it is **not** where the
speed comes from. On the two tiny fixtures it is 22–23% of the e2e time,
which is mostly `ogr2ogr` start-up, not conversion work.

The ldGeoJSON leg is evidence for the claim that FlatGeobuf is tippecanoe's
best input here: text is 75 MB against 43 MB of FlatGeobuf, and even with
`-P` (parallel parse) tippecanoe is slower reading it (23.59 s vs 20.05 s)
on top of an 11× more expensive conversion (2.07 s vs 0.19 s). Benchmarking
tippecanoe from GeoJSON would have flattered tylertoo, so the headline does
not.

`gpio convert flatgeobuf` **failed** on `madagascar-adm4`
(`IO Error: ICreateFeature: NULL geometry not supported with spatial index`)
even though the parquet has no null, empty or invalid geometry — so `ogr2ogr`
supplied that input. On the small fixtures gpio succeeded but was slower than
`ogr2ogr` (Python interpreter start-up), and the harness always takes the
faster converter so the baseline is never penalised for our tool choice. gpio
remains the recommended GeoParquet preprocessor; this is a bug report, not a
recommendation change.

## 4. Memory and archive size

<!-- bench:e2e-fixtures/memory -->
| dataset | tylertoo peak RSS | tippecanoe peak RSS | converter peak RSS | tylertoo archive | tippecanoe archive | archive ratio |
|---|---|---|---|---|---|---|
| `madagascar-adm4` | 760 MB | 413 MB | 199 MB | 99.6 MB | 151.0 MB | 0.66× |
| `open-buildings` | 28 MB | 139 MB | 54 MB | 0.1 MB | 0.2 MB | 0.61× |
| `road-detections` | 32 MB | 141 MB | 53 MB | 0.1 MB | 0.2 MB | 0.57× |

_Run: commit 044360d, tylertoo 0.11.0, tippecanoe v2.79.0, Apple M3 Pro, 18 GB RAM, macOS-15.6-arm64-arm-64bit, 2026-09-26._
<!-- /bench -->

This table is the defaults run. **tylertoo loses the memory column on the
only dataset that matters** — 760 MB against tippecanoe's 413 MB on
`madagascar-adm4` at defaults (1.8×), and 835 MB against 413 MB for the
quality-matched run (2.0×; `tylertoo_quality_matched.peak_rss_bytes` in
`../results/e2e-fixtures.json`), which is the run the parity claim uses. tylertoo's export
concurrency is `--partition-wave auto`, which sizes itself against available
RAM, so this figure moves with machine state (from an earlier, uncommitted
run of the same harness on the same machine, not in `../results/e2e-fixtures.json`: 457 MB
at defaults). Pin it with an explicit `--partition-wave N` before drawing
conclusions. On the tiny
fixtures tylertoo wins the column, 28 MB to 139 MB, because tippecanoe
allocates its working set up front.

Archive sizes are tylertoo's, consistently: 0.57–0.66× tippecanoe at defaults,
0.70–0.80× quality-matched.

## 5. Where tylertoo's own time goes

<!-- bench:e2e-fixtures/phases -->
| dataset | convert (parquet read + ladder) | export (MVT + archive) | row groups read/total | tiles | tile features | oversized tiles |
|---|---|---|---|---|---|---|
| `madagascar-adm4` | 0.37 s | 1.23 s | 1/1 | 146,924 | 353,062 | 0 |
| `open-buildings` | 0.01 s | 0.02 s | 1/1 | 82 | 2,339 | 0 |
| `road-detections` | 0.01 s | 0.02 s | 1/1 | 78 | 3,787 | 0 |

_Run: commit 044360d, tylertoo 0.11.0, tippecanoe v2.79.0, Apple M3 Pro, 18 GB RAM, macOS-15.6-arm64-arm-64bit, 2026-09-26._
<!-- /bench -->

The parquet read and the whole generalization ladder are 23% of tylertoo's
wall on `madagascar-adm4`; MVT encoding and the archive write are the rest.
These fixtures are single-row-group, so row-group pushdown contributes nothing
here — one more reason the input-format advantage is understated by this
corpus rather than overstated.

## 6. Per-zoom tile counts

### `madagascar-adm4`

<!-- bench:e2e-fixtures/per-zoom/madagascar-adm4 -->
| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z0 | 0 | 1 | — | 0.14 MB |
| z1 | 1 | 1 | 0.00 MB | 0.36 MB |
| z2 | 1 | 1 | 0.02 MB | 0.38 MB |
| z3 | 2 | 2 | 0.03 MB | 0.52 MB |
| z4 | 4 | 4 | 0.03 MB | 0.66 MB |
| z5 | 4 | 4 | 0.03 MB | 0.61 MB |
| z6 | 6 | 7 | 0.04 MB | 0.82 MB |
| z7 | 15 | 16 | 0.09 MB | 2.08 MB |
| z8 | 42 | 44 | 0.17 MB | 2.44 MB |
| z9 | 138 | 149 | 0.36 MB | 3.19 MB |
| z10 | 487 | 524 | 0.82 MB | 4.41 MB |
| z11 | 1,705 | 1,943 | 2.09 MB | 6.91 MB |
| z12 | 6,037 | 7,440 | 6.27 MB | 13.68 MB |
| z13 | 24,489 | 28,920 | 22.10 MB | 35.11 MB |
| z14 | 113,993 | 113,991 | 93.19 MB | 110.20 MB |
| **total** | **146,924** | **153,047** | | |

_Run: commit 044360d, tylertoo 0.11.0, tippecanoe v2.79.0, Apple M3 Pro, 18 GB RAM, macOS-15.6-arm64-arm-64bit, 2026-09-26._
<!-- /bench -->

Read the byte columns, not just the tile columns: at z2–z8 tylertoo's stored
bytes are 14–25× smaller than tippecanoe's (z1: ~87×) for nearly the same
tile count.
That is the thinning of §2 showing up as size. At z14, where tylertoo emits
every feature, the two agree to two tiles and tylertoo's bytes are 15%
smaller — the simplification difference alone.

tylertoo emits no z0 tile: the visibility gate generalizes every feature away
at that GSD, and the archive declares the zoom rather than writing an empty
tile. tippecanoe writes one.

### `open-buildings`

<!-- bench:e2e-fixtures/per-zoom/open-buildings -->
| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z5 | 0 | 1 | — | 0.00 MB |
| z6 | 0 | 1 | — | 0.00 MB |
| z7 | 0 | 1 | — | 0.00 MB |
| z8 | 0 | 1 | — | 0.00 MB |
| z9 | 2 | 2 | 0.00 MB | 0.01 MB |
| z10 | 2 | 2 | 0.00 MB | 0.02 MB |
| z11 | 2 | 3 | 0.01 MB | 0.02 MB |
| z12 | 4 | 7 | 0.01 MB | 0.03 MB |
| z13 | 20 | 20 | 0.02 MB | 0.04 MB |
| z14 | 52 | 52 | 0.05 MB | 0.05 MB |
| **total** | **82** | **90** | | |

_Run: commit 044360d, tylertoo 0.11.0, tippecanoe v2.79.0, Apple M3 Pro, 18 GB RAM, macOS-15.6-arm64-arm-64bit, 2026-09-26._
<!-- /bench -->

### `road-detections`

<!-- bench:e2e-fixtures/per-zoom/road-detections -->
| zoom | tylertoo tiles | tippecanoe tiles | tylertoo bytes | tippecanoe bytes |
|---|---|---|---|---|
| z0 | 0 | 1 | — | 0.00 MB |
| z1 | 0 | 1 | — | 0.00 MB |
| z2 | 0 | 1 | — | 0.00 MB |
| z3 | 0 | 1 | — | 0.00 MB |
| z4 | 0 | 1 | — | 0.00 MB |
| z5 | 1 | 1 | 0.00 MB | 0.01 MB |
| z6 | 1 | 1 | 0.00 MB | 0.01 MB |
| z7 | 2 | 2 | 0.00 MB | 0.01 MB |
| z8 | 2 | 2 | 0.00 MB | 0.01 MB |
| z9 | 2 | 2 | 0.01 MB | 0.02 MB |
| z10 | 2 | 2 | 0.01 MB | 0.02 MB |
| z11 | 5 | 5 | 0.01 MB | 0.03 MB |
| z12 | 7 | 7 | 0.01 MB | 0.03 MB |
| z13 | 15 | 15 | 0.02 MB | 0.03 MB |
| z14 | 41 | 40 | 0.04 MB | 0.04 MB |
| **total** | **78** | **82** | | |

_Run: commit 044360d, tylertoo 0.11.0, tippecanoe v2.79.0, Apple M3 Pro, 18 GB RAM, macOS-15.6-arm64-arm-64bit, 2026-09-26._
<!-- /bench -->

## 7. Run-to-run variance

Within a single run (3 timed repeats after a warm-up), on `madagascar-adm4`:

<!-- bench:e2e-fixtures/variance/madagascar-adm4 -->
| measurement | median | min | max | spread, (max − min) / min |
|---|---|---|---|---|
| tylertoo, defaults | 1.61 s | 1.60 s | 1.80 s | 13% |
| tylertoo, quality-matched | 2.67 s | 2.56 s | 3.40 s | 33% |
| tippecanoe, fgb in | 20.05 s | 19.70 s | 22.81 s | 16% |
| tippecanoe -P, ldGeoJSON in | 23.59 s | 22.41 s | 31.73 s | 42% |
| ogr2ogr parquet→fgb | 0.19 s | 0.17 s | 0.19 s | 8% |

_Run: commit 044360d, tylertoo 0.11.0, tippecanoe v2.79.0, Apple M3 Pro, 18 GB RAM, macOS-15.6-arm64-arm-64bit, 2026-09-26._
<!-- /bench -->

Between runs, from an earlier, uncommitted run that is **not** in
`../results/e2e-fixtures.json` (so these figures cannot be regenerated from this record):
an hour before the recorded run, on the same idle laptop, tylertoo's
`madagascar-adm4` median was 2.34 s against the recorded 1.61 s, tippecanoe's
was 23.93 s against 20.05 s, and the defaults ratio was 10.4× against 12.5×.
Take that as a warning rather than a measurement: **a single-digit ratio
measured on a laptop can move by roughly 20% between runs**, which is why no
row here should be quoted to two significant figures.

## 8. What this does not measure

- **Anything above 1.2M features here.** §1b's corpus tier covers
  631k–1.2M features (and settles the Moldova question the archived table in
  [`../overview/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/139aa99/benchmarks/overview/RESULTS.md) (in git history) left open). The 21.6M-building
  Overture run is in [`docs/benchmarks.md`](../../docs/benchmarks.md)
  and `../results/overture-buildings-netherlands.json`.
- **Points at fixture scale.** The corpus tier measures a 630k-point dataset
  (§1b); the fixture set below has none, so §§1–7's numbers are
  polygon/line only.
- **Cold cache.** macOS cannot drop caches without root; everything is warm
  for both tools.
- **Remote input.** `--bbox`/`--filter` row-group pushdown over `s3://` is
  tylertoo's largest structural advantage over a GeoJSON or FlatGeobuf
  stream, and none of it is exercised: these fixtures are local and
  single-row-group.
- **Multi-core scaling.** Both tools were given all 12 cores; neither was
  pinned or swept.
