# Coalesce benchmark: full profile

tylertoo `67a221e` · Neoverse-V1, 64 cores, 123.5 GiB RAM · 2026-10-22 14:21 UTC

**Noise floor.** Coverage differences under 3 percentage points (missing or added share) and timing differences under 10% are not regressions. At full detail, small fields show about 2.5% missing and 2.5% added area from tile-grid snapping alone. Timings depend on the host; compare ratios across hosts, not seconds.

## Performance

Median of the repeats per stage. End to end is convert plus export.

### Madagascar adm4 (17,465 features)

| Setting | Convert | Convert RSS | Export | Export RSS | End to end | vs Before #745 | Overview | PMTiles | Merged | Oversized |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Before #745 | 0.67 s | 0.30 GiB | 0.51 s | 0.25 GiB | 1.18 s | 1.00× | 30 MB | 19 MB | 0 | 0 |
| Defaults | 0.87 s | 0.42 GiB | 1.94 s | 0.41 GiB | 2.81 s | 2.38× | 48 MB | 25 MB | 47,441 | 12 |
| --simplify-factor 0.25 | 1.02 s | 0.46 GiB | 2.15 s | 0.43 GiB | 3.17 s | 2.69× | 61 MB | 28 MB | 51,381 | 13 |
| --simplify-factor 0 | 1.70 s | 0.78 GiB | 2.42 s | 0.79 GiB | 4.12 s | 3.49× | 187 MB | 31 MB | 61,933 | 17 |

### FTW Moldova (631,910 features)

| Setting | Convert | Convert RSS | Export | Export RSS | End to end | vs Before #745 | Overview | PMTiles | Merged | Oversized |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Before #745 | 1.65 s | 2.36 GiB | 4.57 s | 1.57 GiB | 6.22 s | 1.00× | 173 MB | 78 MB | 0 | 49 |
| Defaults | 1.96 s | 2.50 GiB | 7.08 s | 1.82 GiB | 9.04 s | 1.45× | 228 MB | 85 MB | 901,431 | 83 |
| --simplify-factor 0.25 | 3.34 s | 3.20 GiB | 17.40 s | 2.36 GiB | 20.74 s | 3.33× | 315 MB | 95 MB | 1,348,169 | 160 |
| --simplify-factor 0 | 3.45 s | 5.05 GiB | 131.09 s | 7.71 GiB | 134.54 s | 21.63× | 630 MB | 89 MB | 1,594,540 | 217 |
| tippecanoe | failed: exit 100 after 2038 s (Can't increase feature gap threshold further) |  |  |  |  |  |  |  |  |  |

tippecanoe runs as one stage: its wall time is under End to end and its peak RSS under Export RSS.

### NZ Parcels (4,343,547 features)

| Setting | Convert | Convert RSS | Export | Export RSS | End to end | vs Before #745 | Overview | PMTiles | Merged | Oversized |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Before #745 | 7.92 s | 6.10 GiB | 44.58 s | 3.74 GiB | 52.49 s | 1.00× | 2,017 MB | 992 MB | 0 | 0 |
| Defaults | 11.27 s | 9.28 GiB | 97.09 s | 6.45 GiB | 108.36 s | 2.06× | 2,903 MB | 1,425 MB | 2,487,951 | 55 |
| --simplify-factor 0.25 | 13.29 s | 10.57 GiB | 102.24 s | 6.73 GiB | 115.53 s | 2.20× | 3,411 MB | 1,524 MB | 3,121,286 | 68 |
| --simplify-factor 0 | 17.20 s | 21.96 GiB | 122.03 s | 8.72 GiB | 139.23 s | 2.65× | 8,480 MB | 1,729 MB | 3,936,650 | 94 |
| tippecanoe |  |  |  | 6.26 GiB | 1900.86 s | 36.21× |  | 884 MB |  |  |

tippecanoe runs as one stage: its wall time is under End to end and its peak RSS under Export RSS.

## Rendered coverage (MapLibre)

Share of the source area missing in each window, drawn by MapLibre GL JS in headless Chromium; added area in parentheses when at least 0.5%.

| Window | Zoom | Before #745 | Defaults | --simplify-factor 0.25 | --simplify-factor 0 | tippecanoe |
| --- | --- | --- | --- | --- | --- | --- |
| Auckland city, 2.4 km | 8 | 100.0% | 28.5% | 28.1% | 27.0% | 5.7% |
|  | 10 | 91.6% | 28.2% | 17.0% | 16.1% | 0.0% |
|  | 12 | 57.7% | 3.2% | 0.9% | 0.0% | 0.6% |
| Upper North Island, z5 tile 31/19 | 3 | 56.4% | 13.3% | 10.4% | 9.9% | 12.1% |
|  | 5 | 34.9% | 10.2% | 5.9% | 4.7% | 0.4% |
|  | 7 | 16.1% | 5.1% | 1.6% | 0.7% | 0.2% |
| Moldova, whole country | 3 | 99.8% | 15.8% (+51%) | 15.6% (+51%) | 15.4% (+51%) |  |
|  | 5 | 71.6% (+3%) | 22.3% (+25%) | 13.5% (+24%) | 12.3% (+24%) |  |
|  | 7 | 29.8% (+4%) | 12.4% (+9%) | 6.0% (+8%) | 5.4% (+8%) |  |
| Moldova fields, z10 tile 592/359 | 8 | 82.2% (+2%) | 31.2% (+32%) | 22.1% (+30%) | 21.2% (+30%) |  |
|  | 10 | 31.5% (+4%) | 16.9% (+10%) | 7.6% (+9%) | 6.9% (+7%) |  |
|  | 12 | 2.5% (+3%) | 2.5% (+3%) | 2.5% (+3%) | 2.5% (+3%) |  |
| Madagascar adm4, z4 tile 10/8 | 3 | 82.7% | 23.5% | 8.0% | 5.2% |  |
|  | 4 | 79.2% | 14.4% | 2.8% | 0.8% |  |
|  | 6 | 65.3% | 2.1% | 0.3% | 0.0% |  |

## Tile coverage (render_coverage.py)

| Check | Setting | Missing | Added | Tile features |
| --- | --- | --- | --- | --- |
| Madagascar, z8, 3×3 tiles around the densest | Before #745 | 48.59% | 0.000% | 1,028 |
|  | Defaults | 0.23% | 0.002% | 7,782 |
|  | --simplify-factor 0.25 | 0.03% | 0.000% | 7,783 |
|  | --simplify-factor 0 | 0.00% | 0.000% | 6,518 |
| Madagascar, z6, 3×3 tiles (the whole island) | Before #745 | 65.14% | 0.019% | 1,045 |
|  | Defaults | 1.94% | 0.080% | 8,828 |
|  | --simplify-factor 0.25 | 0.28% | 0.030% | 7,966 |
|  | --simplify-factor 0 | 0.04% | 0.043% | 5,495 |

## Drawn share per tile (tile_metrics.py)

Per zoom: median drawn share of the sampled tiles, worst in parentheses. Edge holes: share of the source along tile seams that neither tile draws. Seam excess: colour changes across seams minus those across a control line.

### FTW Moldova

| Setting | z6 | z8 | z10 | z12 | Edge holes | Seam excess |
| --- | --- | --- | --- | --- | --- | --- |
| Before #745 | 56.2% (53.9%) | 89.6% (70.0%) | 97.2% (69.6%) | 99.5% (92.4%) | z6 0.43 / z8 0.18 / z10 0.03 / z12 0.01 | z8 0.00 / z10 0.01 / z12 0.01 |
| Defaults | 78.9% (78.6%) | 93.6% (92.0%) | 97.7% (83.2%) | 99.5% (96.3%) | z6 0.20 / z8 0.07 / z10 0.03 / z12 0.01 | z8 0.39 / z10 -0.00 / z12 -0.02 |
| --simplify-factor 0.25 | 89.6% (89.4%) | 97.0% (96.0%) | 98.9% (92.2%) | 99.5% (96.3%) | z6 0.15 / z8 0.03 / z10 0.01 / z12 0.01 | z8 0.71 / z10 0.13 / z12 -0.02 |
| --simplify-factor 0 | 91.8% (91.3%) | 96.5% (95.8%) | 98.6% (91.0%) | 99.5% (96.3%) | z6 0.10 / z8 0.04 / z10 0.02 / z12 0.01 | z8 0.84 / z10 0.91 / z12 -0.02 |

### NZ Parcels

| Setting | z10 | z12 | z14 | 12/4035/2500 | 12/4036/2499 | 12/4036/2500 | Edge holes | Seam excess |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Before #745 | 98.6% (75.4%) | 99.5% (66.2%) | 100.0% (100.0%) | 67.2% | 76.4% | 68.7% | z10 0.06 / z12 0.09 / z14 0.00 | z10 -0.04 / z12 -0.01 / z14 0.01 |
| Defaults | 99.5% (93.8%) | 99.9% (97.1%) | 100.0% (100.0%) | 97.3% | 97.9% | 97.6% | z10 0.02 / z12 0.01 / z14 0.00 | z10 -0.02 / z12 -0.03 / z14 0.01 |
| --simplify-factor 0.25 | 99.9% (97.3%) | 100.0% (99.4%) | 100.0% (100.0%) | 99.4% | 99.6% | 99.5% | z10 0.01 / z12 0.00 / z14 0.00 | z10 -0.02 / z12 -0.03 / z14 0.01 |
| --simplify-factor 0 | 100.0% (97.9%) | 100.0% (100.0%) | 100.0% (100.0%) | 100.0% | 100.0% | 100.0% | z10 0.01 / z12 0.00 / z14 0.00 | z10 -0.02 / z12 -0.03 / z14 0.01 |
| tippecanoe | 100.0% (99.6%) | 100.0% (99.5%) | 100.0% (99.8%) | 99.5% | 99.6% | 99.6% | z10 0.00 / z12 0.00 / z14 0.00 | z10 0.50 / z12 0.06 / z14 -0.01 |
