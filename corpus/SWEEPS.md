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
