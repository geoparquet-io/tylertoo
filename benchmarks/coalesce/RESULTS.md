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
  - `C0`: `--drop-polygons --drop-lines --no-collapse`, the defaults before
    #745.
  - `C1`: no flags, the new default.
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
Peak RSS is set by convert. One run each; load rose during the 1M today
run and the 4.58M new-default run.

## NZ Parcels (4,343,547 polygons, LINZ layer 51571), z0–z14

| Setting | Convert | Export | Total | Peak RSS (convert / export) | Merged | Oversized tiles |
|---|---|---|---|---|---|---|
| Today, 3800K raw cap | 8.1 s | 48.8 s | 57 s | 6.1 / 3.7 GiB | 0 | 0 |
| C1 + merge, 3800K raw cap | 11.6 s | 96.1 s | 108 s | 9.3 / 6.4 GiB | 3.29M | 80 |
| C2 + merge, 3800K raw cap | 11.8 s | 180.6 s | 192 s | 11.7 / 7.5 GiB | 12.9M | 106 |
| Today, 2 MB gzip cap | 8.1 s | 48.3 s | 56 s | 6.0 / 3.8 GiB | 0 | 0 |
| C1 + merge, 2 MB gzip cap | 11.6 s | 95.6 s | 107 s | 9.2 / 6.4 GiB | 2.65M | 54 |

The load average rose to 10–18 during the C1 exports; see the quiet-host
re-timing below before quoting the NZ ratio.

Drawn-area share on palmerj's three Auckland z12 tiles (#246):

| | 12/4035/2500 | 12/4036/2499 | 12/4036/2500 | z12 median (worst sampled) |
|---|---|---|---|---|
| Today | 67.2% | 76.4% | 68.7% | 99.46% (66.2%) |
| C1 + merge | 97.1% | 97.7% | 97.5% | 99.88% (94.0%) |
| C2 + merge | 97.3% | 97.9% | 97.6% | 99.89% (97.1%) |

No tile exceeds either cap with C1 or C2.
