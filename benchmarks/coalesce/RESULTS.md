# #745 benchmark results: complete tiles at coarse and middle zooms

What the new defaults (density budget on points only, placeholder squares,
merging oversized tiles) cost and buy, against the defaults before #745.
The decision these tables back is
[`corpus/SWEEPS.md` Decision 8](../../corpus/SWEEPS.md).

## Setup

- Host: AWS c7g.16xlarge (64 Arm cores, 123 GB RAM), shared with other
  sessions. Every run waited for a 1-minute load average below 3 and logs
  the load before and after (`results*.jsonl`); runs where the load rose
  during the run are flagged below.
- tylertoo release build of this branch, glibc allocator (the shipped musl
  build uses mimalloc and is faster in absolute terms; ratios are what to
  read). Inputs ordered with gpio 1.7.0 (`prepare_data.py`).
- Settings, spelled for the new defaults (`run_matrix.py` `CONVERTS`):
  - `C0`: `--drop-polygons --drop-lines --polygon-thinning 1
    --no-collapse`, the defaults before #745.
  - `C1`: `--polygon-thinning 1`, the new default as first measured, with
    polygon thinning still on.
  - `C1t`: no flags, the shipped default (`C1` with polygon thinning off;
    see § Polygon thinning off).
  - `C2`: `--polygon-visibility 0 --polygon-thinning 0`.
  - Valves: `--size-valve drop` (re-measuring) or `merge` (the new
    default). The G0 and G1 runs also measured the removed `zoom` and
    `hybrid` valves.
- Drawn-area share (`tile_metrics.py`): per tile, the source polygons and
  the decoded tile are rasterized at 128 px per tile; the share is the
  source-covered pixels the tile also covers. Coarse for parcels about 20 m
  wide, so read it comparatively.
- Today's export path is unchanged: `C0` + `drop` on this branch gives the
  same bytes as `main` (checked on Moldova against a `main` build).

## Madagascar admin-4 (17,465 polygons, a gap-free coverage)

The #572 case. No tile is over the 500K cap at z8, so the size valve never
runs there: the holes come from convert.

| z8, 3×3 tiles around the densest | missing area | added area | tile features |
|---|---|---|---|
| Today (`C0` + drop) | 48.6% | 0.000% | 1,028 |
| `C1` + merge | 0.23% | 0.002% | 7,782 |
| `C2` + merge | 0.23% | 0.002% | 7,782 |

At z6 (the whole island): today 65.1% missing, `C1` and `C2` 1.94%. The
remainder is simplification fringe along borders (#450). `C1` and `C2`
match because these polygons pass the gate and thinning.

## FTW Moldova (631,910 field polygons), z0–z12

End to end, median of 3, Hilbert-sorted input:

| Convert | Valve | Export | Peak RSS (export) | End to end | vs today |
|---|---|---|---|---|---|
| C0 | drop | 4.73 s | 1.63 GiB | 6.47 s | 1.00× |
| C1 | merge | 6.58 s | 1.81 GiB | 8.61 s | 1.33× |
| C1 | drop | 6.51 s | 1.73 GiB | 8.54 s | 1.32× |
| C2 | merge | 11.08 s | 2.33 GiB | 13.07 s | 2.02× |
| C2 | drop | 9.05 s | 1.82 GiB | 11.04 s | 1.71× |
| C2 | zoom (removed) | 30.10 s | 2.47 GiB | 32.09 s | 4.96× |

Convert: 1.74 s / 2.26 GiB (C0), 2.03 s / 2.39 GiB (C1), 1.99 s /
2.62 GiB (C2). Overview file 173 MB (C0), 233 MB (C1), 313 MB (C2).

Drawn-area share, median per sampled tile (the densest tiles included):

| | z6 | z8 | z10 (worst tile) | edge holes z6 / z8 |
|---|---|---|---|---|
| Today | 0.56 | 0.90 | 0.97 (0.70) | 0.43 / 0.17 |
| C1 + drop | 0.41 | 0.81 | 0.97 (0.59) | 0.59 / 0.19 |
| C1 + merge (measured with the removed hybrid valve) | 0.79 | 0.93 | 0.97 (0.82) | 0.22 / 0.08 |
| C2 + merge (per-tile, as shipped) | 0.82 | 0.94 | 0.98 (0.83) | 0.14 / 0.08 |

The convert change alone, with drop, is worse than today at z6: more
polygons reach export and the drop valve throws more away. Merging also
fixes tiles the one-pass drop valve left over the cap: 45 tiles today, 0
with C1 + merge.

Seam colour excess at z8 (colour changes across tile edges minus those
across a control line inside the tile): 0.36 for C1, 0.69 for C2, 0.74 for
the removed per-zoom valve. Merged features keep the attributes of the
feature they merged into, as in tippecanoe, so merged areas can change
colour at tile edges; C1 merges less and shows it less.

### Row order (Moldova, one run each)

| Order | Today total (with sort) | New default total (with sort) | merged |
|---|---|---|---|
| hilbert | 6.75 s (14.4 s) | 8.65 s (16.3 s) | 945,666 |
| str | 6.79 s (16.4 s) | 8.84 s (18.5 s) | 933,382 |
| unsorted (shuffled) | 8.01 s (10.7 s) | 9.72 s (12.4 s) | 1,012,614 |
| as delivered | 6.98 s | 9.05 s | 941,911 |

Quality, new default: drawn share is the same for hilbert and shuffled
input, but tile-edge colour changes appear even at z10 on shuffled input
(seam excess 0.24, against 0.00 for hilbert), because each feature merges
into an arbitrary one. Convert therefore warns on unsorted input.

## Scaling: FTW Mato Grosso prefixes (Hilbert order), z0–z12

| Rows | Input | Today total | New default total | Ratio | Peak RSS today → new |
|---|---|---|---|---|---|
| 250,000 | 29 MB | 3.11 s | 3.30 s | 1.06× | 0.77 → 0.89 GiB |
| 500,000 | 41 MB | 4.36 s | 5.09 s | 1.17× | 1.01 → 1.17 GiB |
| 1,000,000 | 70 MB | 6.79 s | 9.60 s | 1.41× | 1.58 → 1.90 GiB |
| 2,000,000 | 219 MB | 16.81 s | 24.98 s | 1.49× | 3.36 → 3.70 GiB |
| 4,583,000 | 678 MB | 51.49 s | 67.39 s | 1.31× | 6.04 → 7.45 GiB |

Time is linear in input size for both; the overhead does not grow with N.
Peak RSS is set by convert. One run each.

### The merge threshold (review follow-up)

The rows above were taken with the threshold snapped up to a power of two,
which could overshoot the quantile by 2× and, on a tile whose gaps sat
within one octave, merge every polygon in one step. It is now the quantile
gap plus one. Same session, convert once, the two exports alternating
(medians; Moldova 3 runs each, 4.58M 2, the rest 1):

| Input | End to end, snap → +1 | Change | Merged tile members | Most encodes per tile |
|---|---|---|---|---|
| Moldova (632k) | 8.74 → 8.78 s | +0.5% | 948k → 901k | 9 → 28 |
| Mato Grosso 1M | 9.32 → 9.47 s | +1.6% | 1.59M → 1.47M | 13 → 27 |
| Mato Grosso 2M | 24.85 → 27.80 s | +11.9% | 2.92M → 2.73M | 13 → 27 |
| Mato Grosso 4.58M | 70.41 → 80.29 s | +14.0% | 5.59M → 5.27M | 13 → 30 |
| NZ Parcels, 2 MB gzip cap | 109.2 → 109.7 s | +0.5% | 2.65M → 2.49M | 3 → 3 |

Drawn area is unchanged everywhere (Moldova z6–z12, NZ z10/z12 and the
three Auckland tiles to the fourth digit); Moldova's z8 seam excess falls
from 0.43 to 0.39. The cost lands where many tiles are over the cap and
need many steps. Against the table above that puts the 2M and 4.58M rows
at about 1.66× and 1.49× of today (estimated: the two tables are from
different sessions). A quarter-octave rung took about as many encodes as
+1 (25 against 27 at 2M) and was no faster.

## NZ Parcels (4,343,547 polygons, LINZ layer 51571), z0–z14

| Setting | Convert | Export | Total | Peak RSS (convert / export) | Merged | Oversized tiles |
|---|---|---|---|---|---|---|
| Today, 3800K raw cap | 8.1 s | 48.8 s | 57 s | 6.1 / 3.7 GiB | 0 | 0 |
| C1 + merge, 3800K raw cap | 11.6 s | 96.1 s | 108 s | 9.3 / 6.4 GiB | 3.29M | 80 |
| C2 + merge, 3800K raw cap | 11.8 s | 180.6 s | 192 s | 11.7 / 7.5 GiB | 12.9M | 106 |
| Today, 2 MB gzip cap | 8.1 s | 48.3 s | 56 s | 6.0 / 3.8 GiB | 0 | 0 |
| C1 + merge, 2 MB gzip cap | 11.6 s | 95.6 s | 107 s | 9.2 / 6.4 GiB | 2.65M | 54 |

Re-timed on a quiet host, 2 MB gzip cap, two repeats each (they agreed
within 1%): today 8.2 s + 48.7 s = 57 s; C1 + merge 11.7 s + 98.7 s = 110 s,
**1.94×**. (The load averages logged after these exports include the export
itself, which uses every core.) Parcels are denser than fields, so more
tiles reach the cap and merge than on the Mato Grosso ladder (1.31×).
Against tippecanoe's 31 min 41 s that is about 17× faster.

Drawn-area share on palmerj's three Auckland z12 tiles (#246):

| | 12/4035/2500 | 12/4036/2499 | 12/4036/2500 | z12 median (worst sampled) |
|---|---|---|---|---|
| Today | 67.2% | 76.4% | 68.7% | 99.46% (66.2%) |
| C1 + merge | 97.1% | 97.7% | 97.5% | 99.88% (94.0%) |
| C2 + merge | 97.3% | 97.9% | 97.6% | 99.89% (97.1%) |

No tile exceeds either cap with C1 or C2.

### Against tippecanoe

tippecanoe 2.79.0 (the pinned tag, built from source against a local
SQLite), `-Z0 -z14 -b 8 -M 2000000 --coalesce-densest-as-needed`, on the
same NZ Parcels input converted to FlatGeobuf with gpio. 31 min 41 s, peak
RSS 6.3 GiB, 884 MB archive. Scored with the same `tile_metrics.py`:

| | 12/4035/2500 | 12/4036/2499 | 12/4036/2500 | z12 median (worst) | z10 worst |
|---|---|---|---|---|---|
| tippecanoe | 99.5% | 99.6% | 99.6% | 99.98% (99.5%) | 99.6% |
| C1 + merge | 97.1% | 97.7% | 97.5% | 99.88% (94.0%) | 93.8% |

The gap is not thinning (C2 barely moves it) but slivers along shared
borders: each parcel is simplified on its own at 1 GSD (about 9.5 m at z12),
roughly 4× tippecanoe's 1-tile-unit tolerance. On an Auckland subset
(`--bbox 174.5,-37.1,175.0,-36.6`, same caps):

| Settings | 12/4035/2500 | 12/4036/2499 | 12/4036/2500 | archive |
|---|---|---|---|---|
| C1, `--simplify-factor 1.0` (default) | 97.1% | 97.7% | 97.5% | 144 MB |
| C1, `--simplify-factor 0.5` | 98.5% | 99.0% | 98.8% | 150 MB |
| C1, `--simplify-factor 0.25` | 99.2% | 99.4% | 99.4% | 156 MB |
| C2, `--simplify-factor 0.25` | 99.4% | 99.6% | 99.5% | 161 MB |

### Coverage against `--simplify-factor` (`sweep_simplify.py`)

Auckland subset (about 400k parcels, `--bbox 174.5,-37.1,175.0,-36.6`),
defaults plus the 2 MB gzip cap, z0–z14, release build on a quiet host;
area drawn is the mean of the
three named z12 tiles, scored against the subset's own parcels:

| factor | area drawn | z12 median | build | peak RSS | archive |
|---|---|---|---|---|---|
| 2 | 87.40% | 99.50% | 10.8 s | 2.6 GiB | 132 MB |
| 1 (default) | 97.60% | 99.84% | 13.8 s | 3.2 GiB | 145 MB |
| 0.75 | 98.39% | 99.91% | 15.6 s | 3.5 GiB | 147 MB |
| 0.5 | 98.92% | 99.93% | 17.3 s | 4.0 GiB | 150 MB |
| 0.35 | 99.28% | 99.95% | 20.0 s | 4.0 GiB | 154 MB |
| 0.25 | 99.48% | 99.96% | 22.4 s | 4.5 GiB | 155 MB |
| 0.15 | 99.60% | 99.97% | 21.5 s | 4.5 GiB | 157 MB |
| 0.1 | 99.63% | 99.97% | 21.9 s | 4.5 GiB | 157 MB |
| 0.05 | 99.65% | 99.96% | 22.4 s | 4.6 GiB | 157 MB |
| 0 | 99.99% | 100.00% | 17.1 s | 3.2 GiB | 155 MB |

With the shipped defaults (polygon thinning off). Asymptotic above zero,
at about 99.65%, level with tippecanoe's 99.5–99.6%; the knee is near
0.25. With polygon thinning on, every row sat 0.1–0.2 points lower and
`0` reached only 99.85%: the rest was one nested parcel (below).
At 0 the only vertex reduction is the tile grid, which snaps both sides of
a shared border to the same points. On all of NZ (2 MB gzip cap):

| factor | convert | convert peak RSS | overview | export | PMTiles | total |
|---|---|---|---|---|---|---|
| 1 (default) | 11.7 s | 9.3 GiB | 2.9 GB | 98.7 s | 1,409 MB | 110 s |
| 0.25 | 14.8 s | 12.7 GiB | 4.1 GB | 141.1 s | 1,568 MB | 156 s |
| 0 | 15.4 s | 19.0 GiB | 7.9 GB | 104.1 s | 1,610 MB | 120 s |
| 0, `--profile bounded` | 38.2 s | 4.9 GiB | 7.9 GB | | | |

The 19 GiB is the default memory profile choosing RAM on a 123 GB host;
the bounded profile spills to disk and needs less than today's defaults.
The cost that remains is the overview file: every level stores full
resolution.

### Polygon thinning off (`--polygon-thinning 0`, now the default)

A z12 hole in Auckland that every tylertoo setting showed and tippecanoe
did not was parcel 5197556 (about 340 m across), inside a larger ring-
shaped parcel: both bbox centres fall in one thinning cell, the larger one
wins, and the smaller one survives only as a placeholder square about one
GSD wide. With polygon thinning off:

| | total | convert peak RSS | PMTiles | z12 worst sampled tile | named tiles |
|---|---|---|---|---|---|
| NZ, C1 | 110.4 s | 9.3 GiB | 1,409 MB | 94.0% | 97.1–97.7% |
| NZ, C1 + thinning off | 106.3 s | 9.3 GiB | 1,418 MB | 97.05% | 97.3–97.9% |
| Moldova, C1 | 8.6 s | 2.45 GiB | 83 MB | | |
| Moldova, C1 + thinning off | 8.8 s | 2.41 GiB | 84 MB | | |

### A placeholder bug found on the way (#749)

With the polygon visibility gate at 0 (`C2`, or `--verbatim` with a
nonzero `--simplify-factor`), placeholder squares survive every coarse level
under the cascade: on `open-buildings` (1,000 buildings) 229–263 squares
about 35 km across at z0–z9, drawing 19× the real area at z8. Pre-existing
(`main` with `--collapse-square`); the default gate of 2 avoids it, and the
docs no longer recommend a zero gate.

## Rendered coverage at every zoom (MapLibre)

`render_maplibre.py` draws each archive with MapLibre GL JS 4.7.1 in
headless Chromium and compares it pixel by pixel with the source,
rasterized by the same pixel-centre rule. Missing share of the source area
in each window (orange, area drawn where the source has nothing, in
brackets when at least 0.5%). Differences under about 3% are tile-grid
noise: at full detail small fields show about 2.5% missing and 2.5% added.

| Window | Zoom | Before #745 | #745 defaults | 0.25 | 0 | tippecanoe |
|---|---|---|---|---|---|---|
| Auckland city, 2.4 km | 8 | 100% | 28.5% | 28.1% | 26.4% | 5.7% |
| | 10 | 91.6% | 28.3% | 17.0% | 16.1% | 0.0% |
| | 12 | 57.7% | 3.2% | 0.8% | 0.0% | 0.6% |
| Upper North Island, z5 tile 31/19 | 3 | 56.4% | 13.3% | 10.4% | 9.9% | 12.1% |
| | 5 | 34.9% | 10.2% | 5.9% | 4.7% | 0.4% |
| | 7 | 16.1% | 5.1% | 1.6% | 0.7% | 0.2% |
| Moldova, whole country | 3 | 99.8% | 15.8% (+51%) | 15.5% (+52%) | 15.3% (+52%) | |
| | 5 | 71.6% | 22.3% (+25%) | 13.4% (+24%) | 12.3% (+24%) | |
| | 7 | 29.2% | 12.4% (+9%) | 6.0% (+8%) | 5.3% (+8%) | |
| Madagascar adm4, z4 tile 10/8 | 3 | 74.9% | 23.6% | 8.1% | 5.4% | |
| | 4 | 64.5% | 14.4% | 2.9% | 0.8% | |
| | 6 | 43.6% | 2.1% | 0.3% | 0.0% | |

0.25 and 0 are with the fixed one-GSD placeholder square. Before that fix
the square shrank with the factor (and factor 0 had none): Auckland city at
z10 missed 50.4% at 0.25 and 57.3% at 0, Moldova at z5 31.4% and 41.4%.

What remains at the defaults, dense sub-pixel polygons drawn as overlapping
squares (and squares spilling over the gaps between fields), is #750.
