# Portland Roads Parameter Sweep — notes

Sweep of `--line-thinning` ∈ {1, 2, 4} × `--simplify-factor` ∈ {0.5, 1.0, 2.0} on `lines-portland-medium` (295,881 Overture road segments), duplicating mode, zoom range 0–14, auto-ranking on (Overture `road_class` detected). Rendered at level indices 2, 4, 6, 8 (= z4, z6, z8, z10) in magnified and true-scale modes.

Reproduce the conversions with the release binary:

```bash
for lt in 1 2 4; do for sf in 0.5 1.0 2.0; do
  tylertoo overview \
    corpus/data/gpio/lines-portland-medium.parquet \
    corpus/data/sweep/lt${lt}_sf${sf}.parquet \
    --mode duplicating --min-zoom 0 --max-zoom 14 \
    --line-thinning $lt --simplify-factor $sf \
    --report corpus/data/sweep/lt${lt}_sf${sf}.report.json
done; done
```

## Feature counts by level (index → zoom)

Feature counts depend on `--line-thinning` only (`--simplify-factor` changes vertices/bytes, not which features survive). Per line-thinning value:

| level | zoom | lt=1 | lt=2 | lt=4 |
|--:|--:|--:|--:|--:|
| 0 | 2 | 5 | 4 | 4 |
| 1 | 3 | 15 | 14 | 10 |
| 2 | 4 | 55 | 45 | 32 |
| 3 | 5 | 229 | 191 | 124 |
| 4 | 6 | 1035 | 853 | 512 |
| 5 | 7 | 4509 | 3636 | 2056 |
| 6 | 8 | 17614 | 14032 | 7458 |
| 7 | 9 | 57850 | 46600 | 24522 |
| 8 | 10 | 137854 | 120294 | 70313 |
| 9 | 11 | 207441 | 193691 | 151563 |
| 10 | 12 | 250559 | 242545 | 218005 |
| 11 | 13 | 278377 | 275143 | 262603 |
| 12 | 14 | 295881 | 295881 | 295881 |

## Ours vs tippecanoe feature-count ratio (default combo lt2_sf1.0)

Our per-level feature count vs tippecanoe's distinct-feature count at the matching zoom (golden `lines-portland-medium.pmtiles`; counts from `corpus/V2_METRICS.md`, no re-decode). Ratio = ours / tippecanoe. <1 = we keep fewer features than tippecanoe (sparser); >1 = more.

| zoom | ours | tippecanoe | ratio |
|--:|--:|--:|--:|
| 2 | 4 | 18266 | 0.00 |
| 3 | 14 | 18194 | 0.00 |
| 4 | 45 | 16844 | 0.00 |
| 5 | 191 | 16757 | 0.01 |
| 6 | 853 | 15469 | 0.06 |
| 7 | 3636 | 16620 | 0.22 |
| 8 | 14032 | 14675 | 0.96 |
| 9 | 46600 | 19986 | 2.33 |
| 10 | 120294 | 38660 | 3.11 |
| 11 | 193691 | 98726 | 1.96 |
| 12 | 242545 | 256520 | 0.95 |
| 13 | 275143 | 295810 | 0.93 |
| 14 | 295881 | 295876 | 1.00 |

## Reading the ratio

At coarse zooms (z2–z7) our counts are far below tippecanoe's: tippecanoe keeps a near-constant ~15–18k road segments per zoom (tile-budget driven, dropping only to fit tiles), while our GSD-grid thinning collapses hard — dozens to a few thousand features. This is the coarse-zoom sparsity the sweep investigates. From z8 our counts cross tippecanoe's (ratio ~1 at z8, peaking ~3x at z10 where our grid is finer than tippecanoe's per-tile budget) and re-converge to 1.0 at z14 (canonical, verbatim). See `sweep.html` for the visual read; the true-scale strip shows that at true display size the coarse levels are far less alarming than the ~50x-magnified contact sheet suggests.

---

## Q2: density-based budgets (2026-07-02)

The mid-zoom plateau above (z9–z11 ≈ 2–3× tippecanoe) is where duplicating mode
sheds most of its storage overhead. **Q2** adds a per-level feature **budget** on
top of cell-winner thinning: `budget(L) = N / drop_rate^(finest − L)`, dropping
the lowest-priority survivors (Q1 rank order) per **super-cell** (spatial
fairness, gamma) until each level meets its budget. Canonical is never dropped.
See `docs/OVERVIEW_TUNING.md` (density-budget section) for the mechanism, its
tippecanoe analogs (drop-rate / gamma dot-dropping), and knobs.

### Calibration

`--drop-rate` sweep on `lines-portland-medium` (duplicating, defaults = `lt=1`,
auto-rank, z0–14), ratio = ours / tippecanoe (cached goldens from
`corpus/V2_METRICS.md`):

| rate | z8 | z9 | z10 | z11 | z12 |
|--:|--:|--:|--:|--:|--:|
| off (cell-winner only) | 1.20 | 2.89 | 3.57 | 2.10 | 0.98 |
| 1.55 | 1.20 | 1.65 | 1.33 | 0.80 | 0.48 |
| 1.60 | 1.20 | 1.41 | 1.17 | 0.73 | 0.45 |
| **1.65 (shipped)** | **1.00** | **1.21** | **1.03** | **0.67** | **0.42** |
| 1.80 | 0.59 | 0.78 | 0.73 | 0.51 | 0.36 |
| 2.50 | 0.08 | 0.15 | 0.20 | 0.19 | 0.18 |

The data's natural (cell-winner) curve is flat at both ends and steep in the
middle, so no single geometric rate lands all of z9–z11 inside [1.0, 1.3]. The
z8 non-binding threshold sits at rate ≈ 1.6 (`N / rate^6 ≈ natural(z8)`). We ship
**`--drop-rate 1.65`**: it puts z9 (1.21) and z10 (1.03) — the worst offenders —
squarely in band and moves z8 to 1.00 (≈ the ~0.96 target), accepting z11 at
0.67 (sparser than tippecanoe, but still 66k segments and a storage win).
Tippecanoe's nominal `2.5` over-thins catastrophically here because our budget
anchors on the full canonical count `N`, not a per-tile basezoom count.

### Before / after (shipped default, rate 1.65)

`lines-portland-medium`, duplicating, defaults, z0–14:

| zoom | before | before/tip | after | after/tip | tippe |
|--:|--:|--:|--:|--:|--:|
| 2 | 5 | 0.00 | 5 | 0.00 | 18266 |
| 3 | 15 | 0.00 | 15 | 0.00 | 18194 |
| 4 | 55 | 0.00 | 55 | 0.00 | 16844 |
| 5 | 229 | 0.01 | 229 | 0.01 | 16757 |
| 6 | 1035 | 0.07 | 1035 | 0.07 | 15469 |
| 7 | 4509 | 0.27 | 4509 | 0.27 | 16620 |
| 8 | 17614 | 1.20 | 14663 | 1.00 | 14675 |
| 9 | 57850 | 2.89 | 24193 | 1.21 | 19986 |
| 10 | 137854 | 3.57 | 39919 | 1.03 | 38660 |
| 11 | 207441 | 2.10 | 65867 | 0.67 | 98726 |
| 12 | 250559 | 0.98 | 108680 | 0.42 | 256520 |
| 13 | 278377 | 0.94 | 179322 | 0.61 | 295810 |
| 14 | 295881 | 1.00 | 295881 | 1.00 | 295876 |

Coarse zooms (z2–z7) are byte-identical before/after — they are cell-winner
limited, below their budgets, so the ceiling never bites. z8–z13 thin; z14
(canonical) is verbatim.

### File sizes (duplicating, on-disk .parquet)

| dataset | before (no budget) | after (rate 1.65) | Δ |
|---|--:|--:|--:|
| lines-portland-medium | 110.07 MB | 71.91 MB | **−34.7 %** |
| points-nyc-medium | 79.36 MB | 78.87 MB | −0.6 % |

### Points: NYC 458k POIs (Q2 gate)

The budget applies to points too, but `points-nyc-medium` barely moves (only z12
thins, 174626 → 168277). Points are already thinned hard by `--point-thinning 4`,
so their cell-winner counts sit *below* the `N`-anchored budget at every zoom
(NYC N = 458k makes budgets generous; its z8–z11 natural counts are 3–4×
tippecanoe in *ratio* but a small *fraction* of N). A rate aggressive enough to
bind NYC points (~2.5) would obliterate Portland lines. NYC point over-retention
is therefore left to **Q4 point clustering** (its plan gate: "NYC as graduated
dots"); Q2's job is the line/polygon plateau, where it delivers the −35 % above.
