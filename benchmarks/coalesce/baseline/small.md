# Coalesce benchmark: small profile

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

## Rendered coverage (MapLibre)

Share of the source area missing in each window, drawn by MapLibre GL JS in headless Chromium; added area in parentheses when at least 0.5%.

| Window | Zoom | Before #745 | Defaults | --simplify-factor 0.25 | --simplify-factor 0 |
| --- | --- | --- | --- | --- | --- |
| Moldova, whole country | 3 | 99.8% | 15.8% (+51%) | 15.6% (+51%) | 15.4% (+51%) |
|  | 5 | 71.6% (+3%) | 22.3% (+25%) | 13.5% (+24%) | 12.3% (+24%) |
|  | 7 | 29.8% (+4%) | 12.4% (+9%) | 6.0% (+8%) | 5.4% (+8%) |
| Moldova fields, z10 tile 592/359 | 8 | 82.2% (+2%) | 31.2% (+32%) | 22.1% (+30%) | 21.2% (+30%) |
|  | 10 | 31.5% (+4%) | 16.9% (+10%) | 7.6% (+9%) | 6.9% (+7%) |
|  | 12 | 2.5% (+3%) | 2.5% (+3%) | 2.5% (+3%) | 2.5% (+3%) |
| Madagascar adm4, z4 tile 10/8 | 3 | 82.7% | 23.5% | 8.0% | 5.2% |
|  | 4 | 79.2% | 14.4% | 2.8% | 0.8% |
|  | 6 | 65.3% | 2.1% | 0.3% | 0.0% |

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
