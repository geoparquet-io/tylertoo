# Parameter sweeps — the shipped-default decisions

Single home for the corpus sweeps that set the engine's default knob
values. Each section states the decision, the evidence, and where the
renders/artifacts live. The knobs themselves are documented in
`docs/OVERVIEW_TUNING.md`; the code constants live in
`crates/core/src/overview/assign.rs` and the CLI arg defaults in
`crates/cli/src/main.rs`.

Generated artifacts referenced here are **not committed**: rebuild the
corpus with `corpus/fetch.sh` + `corpus/optimize.sh`, goldens with
`corpus/goldens.sh`, per-level metrics (`corpus/V2_METRICS.md`) with
`corpus/render.py`, and a fresh correctness report
(`corpus/V1_RESULTS.md`) with `corpus/verify.sh`. Metric definitions:
`corpus/METRICS.md`. Historical point-in-time snapshots:
`context/archive/corpus/`.

---

## Decision 1: `--line-thinning 1.0` (retuned from 2.0)

**Sweep:** `--line-thinning` ∈ {1, 2, 4} × `--simplify-factor` ∈
{0.5, 1.0, 2.0} on `lines-portland-medium` (295,881 Overture road
segments), duplicating mode, z0–14, auto-ranking on. Rendered at
levels 2/4/6/8 (z4/z6/z8/z10) in magnified and true-scale modes
(contact sheet: `corpus/data/renders/sweep.html`, regenerable via
`corpus/render.sh`).

**Decision (maintainer render review, 2026-07-02):** ship `lt=1.0`.
At 1.0, road networks stay visibly more continuous at coarse zooms;
the true-scale strips showed the extra density costs little
legibility. Feature counts depend on `lt` only (`sf` changes
vertices/bytes, not survival):

| level | zoom | lt=1 | lt=2 | lt=4 |
|--:|--:|--:|--:|--:|
| 2 | 4 | 55 | 45 | 32 |
| 4 | 6 | 1035 | 853 | 512 |
| 6 | 8 | 17614 | 14032 | 7458 |
| 8 | 10 | 137854 | 120294 | 70313 |
| 10 | 12 | 250559 | 242545 | 218005 |
| 12 | 14 | 295881 | 295881 | 295881 |

At coarse zooms (z2–z7) our counts sit far below tippecanoe's
(tippecanoe keeps a near-constant ~15–18k segments per zoom,
tile-budget driven); from z8 our counts cross tippecanoe's and peak
~3× at z10 — the mid-zoom plateau that motivated Decision 2.

Full tables: `context/archive/corpus/SWEEP_NOTES.md`.

## Decision 2: `--drop-rate 1.65` (Q2 density budget)

**Sweep:** `--drop-rate` ∈ {off, 1.55, 1.60, 1.65, 1.80, 2.50} on
`lines-portland-medium` (duplicating, lt=1, auto-rank, z0–14);
ratio = ours / tippecanoe distinct-feature count at matching zoom:

| rate | z8 | z9 | z10 | z11 | z12 |
|--:|--:|--:|--:|--:|--:|
| off (cell-winner only) | 1.20 | 2.89 | 3.57 | 2.10 | 0.98 |
| **1.65 (shipped)** | **1.00** | **1.21** | **1.03** | **0.67** | **0.42** |
| 2.50 (tippecanoe's nominal) | 0.08 | 0.15 | 0.20 | 0.19 | 0.18 |

**Decision (2026-07-02):** ship `1.65`. It lands the worst offenders
(z9 at 1.21×, z10 at 1.03×) in band and moves z8 to 1.00, accepting
z11 at 0.67×. Tippecanoe's nominal 2.5 over-thins catastrophically
here because our budget anchors on the full canonical count `N`
(every feature appears at the finest level), not a per-tile basezoom
count. Coarse zooms (z2–z7) are cell-winner-limited and byte-identical
with or without the budget. File-size effect: Portland lines
duplicating 110.07 → 71.91 MB (−34.7 %).

Points barely feel the budget (already thinned hard by
`--point-thinning`); NYC point over-retention was assigned to
clustering (Decision 3) instead.

Full calibration tables: `context/archive/corpus/SWEEP_NOTES.md` (Q2
section).

## Decision 3: `--point-thinning 16.0` when `--cluster` is on

**Sweep:** NYC POIs (`points-nyc-medium`, 458k) with `--cluster` at
`pt` ∈ {4, 16, 48}; each 4× step in the factor shifts the whole
density ladder about two zooms.

**Decision (maintainer render review, 2026-07-03, PR #172 line):**
with clustering, absorbed points are summarized into `point_count`
rather than discarded, so a sparser grid is pure win; `16 × GSD`
(≈ one dot per ~16 display pixels, near supercluster's ~40 px default
radius) gives the familiar graduated-cluster look. Without
`--cluster` the default stays `4.0` (a coarse grid would *discard*
data). Pass `--point-thinning` explicitly to override in either mode.

## Decision 4: coalescing junction hardness — `--coalesce-junction-angle 0` (junction continuation OFF)

**Sweep:** Portland roads rendered with junction continuation at 0°
vs 30° (`corpus/data/bench/q3/portland-roads-junction{00,30}.pmtiles`,
regenerable).

**Decision (maintainer render review, 2026-07-03, commit c0b4e3a):**
ship `0` — junctions terminate chains (strict degree-2 chaining),
preserving network topology. Continuation (30°) produced giant
arterial strokes at z0–z1 but over-merges through genuine turns and
smears attributes across crossings. Line coalescing itself remains
**ON by default** (same review: defaults should look right); opt out
with `--no-coalesce-lines`.

## Decision 5: `--row-group-size-policy zoom-scaled` (issue #202)

**Sweep:** `--row-group-size` ∈ {10k, 25k, 50k, 100k} and the new
`--row-group-size-policy zoom-scaled` on all four benchmark datasets
(points-nyc, lines-portland, polygons-portland, polygons-ftw-moldova),
duplicating mode, z0–14, default knobs. Measured against real S3
(us-east-2) with the #200 harness: DuckDB cold requests/bytes/wall
per viewport.

**Request counts (cold, median of 3):**

| dataset | viewport | z | rg10k | rg25k | rg50k | rg100k | zoom-scaled |
|---|---|---|---|---|---|---|---|
| points-nyc-medium | regional | 11 | 38 | 22 | 20 | 14 | **14** |
| points-nyc-medium | street | 14 | 46 | 32 | 22 | 16 | **46** |
| lines-portland-medium | regional | 11 | 32 | 20 | 14 | 8 | **8** |
| lines-portland-medium | street | 14 | 27 | 21 | 15 | 15 | **27** |
| polygons-portland-medium | street | 14 | 33 | 33 | 33 | 21 | **33** |
| polygons-ftw-moldova-large | regional | 9 | **52** | 32 | 22 | 12 | **12** |
| polygons-ftw-moldova-large | street | 14 | 25 | 24 | 23 | 13 | **25** |

**Bytes fetched (cold, median of 3):**

| dataset | viewport | z | rg10k | rg25k | rg50k | rg100k | zoom-scaled |
|---|---|---|---|---|---|---|---|
| points-nyc-medium | street | 14 | **5.1 MB** | 8.0 MB | 11.0 MB | 15.9 MB | **5.1 MB** |
| lines-portland-medium | street | 14 | **4.5 MB** | 9.2 MB | 13.2 MB | 27.0 MB | **4.5 MB** |
| polygons-portland-medium | street | 14 | **6.7 MB** | 16.7 MB | 34.2 MB | 39.3 MB | **6.7 MB** |
| polygons-ftw-moldova-large | regional | 9 | 7.1 MB | 8.5 MB | 8.5 MB | 8.5 MB | 8.5 MB |
| polygons-ftw-moldova-large | street | 14 | **2.0 MB** | 4.3 MB | 12.3 MB | 12.6 MB | **2.0 MB** |

World viewports: all policies identical (row count < any cap).

**Storage (local, file / footer / RGs):**

| dataset | rg10k | rg100k | zoom-scaled |
|---|---|---|---|
| points-nyc-medium | 78.95 MB / 121 KB / 116 | 73.92 MB / 28 KB / 23 | 76.98 MB / 82 KB / 77 |
| lines-portland-medium | 73.21 MB / 81 KB / 81 | 71.28 MB / 21 KB / 17 | 72.39 MB / 54 KB / 52 |
| polygons-portland-medium | 186.68 MB / 156 KB / 148 | 180.75 MB / 24 KB / 20 | 184.78 MB / 121 KB / 114 |
| polygons-ftw-moldova-large | 293.65 MB / 248 KB / 167 | 292.23 MB / 40 KB / 24 | 292.52 MB / 151 KB / 100 |

File size: negligible (2–6% variation). Footer: linear in RG count but
small either way (<250 KB on the 294 MB Moldova file).

**Decision (2026-07-04, #202):** ship `zoom-scaled` as **opt-in**; keep
`constant 10k` as the default for now. `zoom-scaled` doubles the
row-group cap per zoom step below the finest level, so coarse bands —
read mostly whole by wide viewports anyway — become fewer, larger row
groups, while the finest level keeps tight bbox pruning:

- Coarse/regional: matches rg100k request counts (Moldova regional
  52 → 12 requests, 77% reduction).
- Fine/street: maintains rg10k byte counts (Portland polygons 6.7 MB
  vs rg100k's 39.3 MB, 6× less).
- Best of both: the lowest wall times across the sweep on street
  viewports (fewer bytes trumps fewer requests at the canonical level).

The constant policies are a tradeoff: rg100k cuts requests but pulls
3–6× more bytes at fine zoom (wider row groups mean coarser pruning);
rg10k keeps bytes tight but pays 2–4× more round trips on coarse/mid
viewports. `zoom-scaled` gets both — request efficiency at coarse zoom,
byte efficiency at fine zoom.

**Why opt-in (for now):** the sweep confirms the win, but the policy is
new; shipping it as the default would be a behavior change to all users
without a bake-in period. The option is there, documented, and proven;
it can be promoted to the default in a subsequent release once real-world
usage confirms no edge-case surprises.

Raw data: `corpus/data/bench/sweep202/{rg10k,rg25k,rg50k,rg100k,zoom-scaled}/`
(local), `s3://tylertoo-bench/sweep202/` (remote, can be deleted after
this decision is final).

## Decision 6: `--polygon-visibility 2.0` (retuned from 4.0) + the coarse-zoom dot-fill recipe (issue #259)

**Sweep (2026-07-15):** `--polygon-visibility` ∈ {4 (old default), 2, 1,
0+`--collapse`} on `polygons-ftw-moldova-large` (631,910 field polygons)
and `overture-germany-buildings` (59,032,924 footprints, the #250 demo
fixture), duplicating mode, z0–14, all other knobs default. Reports +
uncapped PMTiles exports + level renders.

**Root cause first.** The #259 hypothesis was that `--drop-rate` ×
`--drop-gamma` needed retuning at the coarsest levels. The sweep
disproves that: on the default Germany run **every level is
visibility-gate-limited far below its density budget** (z10: 145k written
vs 7.96M budget; only z13 approaches it), so the budget knobs have no
coarse-zoom effect at all. Two mechanisms empty coarse zooms for
small-polygon layers: the assign-time gate (`diag >= pv × GSD`) and the
write-time RDP collapse (simplified geometry below the 1 × GSD tolerance
is dropped — an effective ~2 × GSD survival bar on real shapes). The
#250 demo's `--drop-rate 1.3` merely inflated budget-bound mid zooms
(+13 % rows on Moldova) without changing coarse fill.

**pv sweep, Moldova (written features; no collapse unless noted):**

| zoom | pv=4 (old) | pv=2 | pv=1 | pv=0 + collapse |
|--:|--:|--:|--:|--:|
| 2 | — | — | — | 614 |
| 3 | — | 3 | 3 | 2,266 |
| 4 | 36 | 188 | 188 | 4,225 |
| 5 | 978 | 3,138 | 3,133 | 6,971 |
| 6 | 8,005 | 10,947 | 10,885 | 11,502 |
| 7 | 18,859 | 18,819 | 18,758 | 18,979 |
| 12 | 232,107 | 232,107 | 231,359 | 232,107 |

File size: 267.4 → 267.7 MB (pv 4→2, +0.1 %). **pv=1 ≡ pv=2 in output**
(188 vs 188 at z4): everything the lower gate admits below ~2 × GSD is
RDP-collapsed at write time anyway — pv=1 only *wastes* density-budget
slots on doomed candidates (z12: 231,359 vs 232,107 written). So 2.0 is
the natural floor while collapse is off.

**Germany buildings, pv 4→2:** z8 917 → 4,257 (4.6×), z9 15k → 48k
(3.2×), z10 145k → 424k (2.9×), z11 1.14M → 3.05M (2.7×), z12 6.9M →
18.7M (2.7×); z13/z14 ~unchanged (budget-capped / canonical). Pyramid
starts z6 instead of z7. Cost: +14.5 % rows, +12.8 % overview bytes
(11.91 → 13.43 GB), +6.6 % convert wall (242 → 258 s).

**Decision:** ship `--polygon-visibility 2.0`. The old 4.0 gate was
strictly stricter than what write-time simplification can ever emit —
it starved z8–z12 for zero benefit. 2.0 aligns the eligibility gate with
the write-time survival bar (and with the line gate).

**The country view needs the recipe, not a gate.** No gate value can put
a 20 m building on a z4 map as a polygon. The documented recipe
(`docs/OVERVIEW_TUNING.md` → "Country-scale dot fill") is
`--polygon-visibility 0 --collapse` (+ `--max-tile-size 500K` on export,
+ a circle layer in the style): every Germany level populates (z0 = 581
dots, z4 = 128,886, z6 = 1.07M), z6–z13 land exactly on the budget
ladder `N/1.65^(14−z)` with visible gamma-fair density structure
(Ruhr/Berlin denser, rural protected), at +31 % overview bytes / +40 %
convert wall on Germany and **+0.3 % bytes** on Moldova. Uncapped coarse
dot tiles reach 12 MB (Germany z6) — hence the 500K cap in the recipe,
now the default (#280), applied as a spatially even stride of the dots.
`--collapse` stays opt-in per spec Q4 (§7.5: type collapse MUST be
producer-opt-in); the demo viewer gained the circle layer, and the
empty-level WARN now points at the recipe. Tippecanoe fills the same gap
by default with tiny-polygon *placeholder squares* (type-preserving) —
recorded as future work in `context/ARCHITECTURE.md` (refs #85/#177,
#246).

Raw artifacts: `corpus/data/bigbench/sweep_scratch/` (not committed).
