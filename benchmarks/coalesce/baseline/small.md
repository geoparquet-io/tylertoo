# Coalesce benchmark: small profile

tylertoo `fb9ed97` · Neoverse-V1, 64 cores, 123.5 GiB RAM · 2026-10-30 19:34 UTC

**Noise floor.** Coverage differences under 3 percentage points (missing or added share) and timing differences under 10% are not regressions. At full detail, small fields show about 2.5% missing and 2.5% added area from tile-grid snapping alone. Timings depend on the host; compare ratios across hosts, not seconds.

## Performance

Median of the repeats per stage. End to end is convert plus export.

### Madagascar adm4 (17,465 features)

| Setting | Convert | Convert RSS | Export | Export RSS | End to end | vs Before #745 | Overview | PMTiles | Merged | Oversized |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Before #745 | 0.52 s | 0.60 GiB | 0.26 s | 0.90 GiB | 0.78 s | 1.00× | 30 MB | 19 MB | 0 | 0 |
| 500K raw cap (before #761) | 0.70 s | 0.89 GiB | 0.47 s | 1.23 GiB | 1.17 s | 1.50× | 48 MB | 25 MB | 48,326 | 12 |
| Defaults | 0.70 s | 0.92 GiB | 0.47 s | 1.19 GiB | 1.17 s | 1.50× | 48 MB | 27 MB | 6,302 | 2 |
| --simplify-factor 0.25 | 0.85 s | 0.96 GiB | 0.49 s | 1.16 GiB | 1.34 s | 1.72× | 61 MB | 29 MB | 9,379 | 3 |
| --simplify-factor 0 | 1.50 s | 1.20 GiB | 0.76 s | 1.61 GiB | 2.26 s | 2.90× | 187 MB | 34 MB | 19,374 | 4 |
| --coverage-fill | 0.70 s | 0.94 GiB | 1.98 s | 1.34 GiB | 2.68 s | 3.44× | 48 MB | 27 MB | 6,302 | 2 |

### FTW Moldova (631,910 features)

| Setting | Convert | Convert RSS | Export | Export RSS | End to end | vs Before #745 | Overview | PMTiles | Merged | Oversized |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Before #745 | 1.12 s | 3.43 GiB | 2.09 s | 2.57 GiB | 3.21 s | 1.00× | 173 MB | 77 MB | 0 | 49 |
| 500K raw cap (before #761) | 1.41 s | 3.69 GiB | 3.68 s | 2.63 GiB | 5.09 s | 1.59× | 231 MB | 85 MB | 927,675 | 84 |
| Defaults | 1.40 s | 3.72 GiB | 3.42 s | 2.63 GiB | 4.82 s | 1.50× | 231 MB | 96 MB | 510,899 | 27 |
| --simplify-factor 0.25 | 2.62 s | 4.03 GiB | 4.79 s | 2.63 GiB | 7.41 s | 2.31× | 315 MB | 116 MB | 713,194 | 43 |
| --simplify-factor 0 | 2.77 s | 6.01 GiB | 17.50 s | 5.80 GiB | 20.27 s | 6.31× | 630 MB | 119 MB | 890,931 | 67 |
| --coverage-fill | 1.42 s | 3.73 GiB | 28.21 s | 2.63 GiB | 29.63 s | 9.23× | 231 MB | 110 MB | 510,899 | 27 |

## Work

Summed over the zooms of the last export: exact for a binary and input, so compare these before timings. Valve passes count every encode, one per tile that fits at once; fill columns are the #750 coverage fill.

| Dataset | Setting | Tiles | Valve passes | Fill polygons | Fill vertices | Fill shed |
| --- | --- | --- | --- | --- | --- | --- |
| Madagascar adm4 | Before #745 | 10,114 | 10,114 | 0 | 0 | 0 |
| Madagascar adm4 | 500K raw cap (before #761) | 10,137 | 10,150 | 0 | 0 | 0 |
| Madagascar adm4 | Defaults | 10,137 | 10,139 | 0 | 0 | 0 |
| Madagascar adm4 | --simplify-factor 0.25 | 10,137 | 10,140 | 0 | 0 | 0 |
| Madagascar adm4 | --simplify-factor 0 | 10,137 | 10,143 | 0 | 0 | 0 |
| Madagascar adm4 | --coverage-fill | 10,137 | 12,815 | 17,676 | 352,897 | 0 |
| FTW Moldova | Before #745 | 1,286 | 1,510 | 0 | 0 | 0 |
| FTW Moldova | 500K raw cap (before #761) | 1,289 | 1,432 | 0 | 0 | 0 |
| FTW Moldova | Defaults | 1,289 | 1,334 | 0 | 0 | 0 |
| FTW Moldova | --simplify-factor 0.25 | 1,289 | 1,384 | 0 | 0 | 0 |
| FTW Moldova | --simplify-factor 0 | 1,289 | 1,499 | 0 | 0 | 0 |
| FTW Moldova | --coverage-fill | 1,289 | 1,746 | 449,140 | 7,496,160 | 252,861 |

## Rendered coverage (MapLibre)

Share of the source area missing in each window, drawn by MapLibre GL JS in headless Chromium; added area in parentheses when at least 0.5%.

| Window | Zoom | Before #745 | 500K raw cap (before #761) | Defaults | --simplify-factor 0.25 | --simplify-factor 0 | --coverage-fill |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Moldova, whole country | 3 | 99.8% | 15.6% (+51%) | 15.6% (+51%) | 15.6% (+51%) | 15.4% (+51%) | 7.0% (+60%) |
|  | 5 | 71.6% (+3%) | 21.3% (+25%) | 21.4% (+25%) | 13.4% (+24%) | 12.3% (+24%) | 21.4% (+25%) |
|  | 7 | 29.8% (+4%) | 12.3% (+9%) | 12.3% (+9%) | 5.9% (+8%) | 6.2% (+8%) | 10.0% (+11%) |
| Moldova fields, z10 tile 592/359 | 8 | 82.2% (+2%) | 30.7% (+32%) | 30.7% (+32%) | 22.1% (+30%) | 21.2% (+30%) | 30.7% (+32%) |
|  | 10 | 31.5% (+4%) | 16.5% (+10%) | 16.5% (+10%) | 7.5% (+9%) | 6.9% (+7%) | 15.5% (+11%) |
|  | 12 | 2.5% (+3%) | 2.5% (+3%) | 2.5% (+3%) | 2.5% (+3%) | 2.5% (+3%) | 2.5% (+3%) |
| Madagascar adm4, z4 tile 10/8 | 3 | 82.7% | 22.8% | 22.7% | 8.0% | 5.2% | 0.3% (+1%) |
|  | 4 | 79.2% | 14.2% | 14.2% | 2.8% | 0.9% | 0.2% |
|  | 6 | 65.3% | 2.1% | 2.1% | 0.3% | 0.0% | 0.0% |

## Tile coverage (render_coverage.py)

| Check | Setting | Missing | Added | Tile features |
| --- | --- | --- | --- | --- |
| Madagascar, z8, 3×3 tiles around the densest | Before #745 | 48.59% | 0.000% | 1,028 |
|  | 500K raw cap (before #761) | 0.23% | 0.002% | 7,782 |
|  | Defaults | 0.23% | 0.002% | 7,782 |
|  | --simplify-factor 0.25 | 0.03% | 0.000% | 7,783 |
|  | --simplify-factor 0 | 0.00% | 0.000% | 7,783 |
|  | --coverage-fill | 0.00% | 0.005% | 7,796 |
| Madagascar, z6, 3×3 tiles (the whole island) | Before #745 | 65.14% | 0.019% | 1,045 |
|  | 500K raw cap (before #761) | 1.95% | 0.071% | 8,840 |
|  | Defaults | 1.94% | 0.071% | 18,876 |
|  | --simplify-factor 0.25 | 0.29% | 0.030% | 16,934 |
|  | --simplify-factor 0 | 0.04% | 0.043% | 13,878 |
|  | --coverage-fill | 0.03% | 0.141% | 19,020 |

## Drawn share per tile (tile_metrics.py)

Per zoom: median drawn share of the sampled tiles, worst in parentheses. Edge holes: share of the source along tile seams that neither tile draws. Seam excess: colour changes across seams minus those across a control line.

### FTW Moldova

| Setting | z6 | z8 | z10 | z12 | Edge holes | Seam excess |
| --- | --- | --- | --- | --- | --- | --- |
| Before #745 | 56.2% (53.9%) | 89.6% (70.0%) | 97.2% (69.7%) | 99.5% (92.4%) | z6 0.43 / z8 0.18 / z10 0.03 / z12 0.01 | z8 0.01 / z10 0.01 / z12 0.01 |
| 500K raw cap (before #761) | 79.7% (79.6%) | 93.5% (92.2%) | 97.7% (83.5%) | 99.5% (96.3%) | z6 0.17 / z8 0.07 / z10 0.03 / z12 0.01 | z8 0.39 / z10 -0.01 / z12 -0.02 |
| Defaults | 79.3% (79.0%) | 93.5% (92.1%) | 97.7% (83.6%) | 99.5% (96.3%) | z6 0.17 / z8 0.07 / z10 0.03 / z12 0.01 | z8 -0.00 / z10 -0.01 / z12 -0.02 |
| --simplify-factor 0.25 | 89.7% (89.2%) | 97.0% (96.0%) | 98.9% (92.3%) | 99.5% (96.3%) | z6 0.15 / z8 0.03 / z10 0.01 / z12 0.01 | z8 0.18 / z10 -0.00 / z12 -0.02 |
| --simplify-factor 0 | 90.0% (89.6%) | 96.1% (95.5%) | 98.6% (90.9%) | 99.5% (96.3%) | z6 0.10 / z8 0.04 / z10 0.02 / z12 0.01 | z8 0.75 / z10 0.00 / z12 -0.02 |
| --coverage-fill | 85.1% (84.2%) | 96.1% (92.6%) | 98.9% (84.4%) | 99.5% (96.3%) | z6 0.13 / z8 0.05 / z10 0.01 / z12 0.01 | z8 0.00 / z10 -0.00 / z12 -0.02 |
