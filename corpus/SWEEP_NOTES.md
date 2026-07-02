# Portland Roads Parameter Sweep — notes

Sweep of `--line-thinning` ∈ {1, 2, 4} × `--simplify-factor` ∈ {0.5, 1.0, 2.0} on `lines-portland-medium` (295,881 Overture road segments), duplicating mode, zoom range 0–14, auto-ranking on (Overture `road_class` detected). Rendered at level indices 2, 4, 6, 8 (= z4, z6, z8, z10) in magnified and true-scale modes.

Reproduce the conversions with the release binary:

```bash
for lt in 1 2 4; do for sf in 0.5 1.0 2.0; do
  gpq-tiles overview \
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
