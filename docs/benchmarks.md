# Benchmarks

<!-- bench:e2e-corpus/headline -->
tylertoo was **43× faster than tippecanoe** across three GeoParquet-to-PMTiles builds. tippecanoe used less peak memory on each dataset.
<!-- /bench -->

## tylertoo vs. tippecanoe

The timings cover the full build from GeoParquet to PMTiles. For tippecanoe, that includes converting the input to FlatGeobuf with `gpio`.

<!-- bench:e2e-corpus/speedup-bars -->
<figure class="tt-chart"><figcaption>Build time</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">tylertoo</span> <span class="tt-key tt-b">tippecanoe</span></p>
<div class="tt-row"><span class="tt-label">Portland buildings</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.011" aria-hidden="true"></span><span class="tt-val">2.0 s, 42× faster</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.476" aria-hidden="true"></span><span class="tt-val">84.4 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">Oregon roads</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.023" aria-hidden="true"></span><span class="tt-val">4.1 s, 43× faster</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:1.000" aria-hidden="true"></span><span class="tt-val">177.2 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.022" aria-hidden="true"></span><span class="tt-val">3.9 s, 43× faster</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.942" aria-hidden="true"></span><span class="tt-val">167.0 s</span></span>
</div>
<div class="tt-row tt-excluded"><span class="tt-label">New York City places <span class="tt-note">not in the mean</span></span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.005" aria-hidden="true"></span><span class="tt-val">0.9 s, 45× faster</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.238" aria-hidden="true"></span><span class="tt-val">42.2 s</span></span>
</div>
</figure>
<!-- /bench -->

<!-- bench:e2e-corpus/memory-bars -->
<figure class="tt-chart"><figcaption>Peak memory</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">tylertoo</span> <span class="tt-key tt-b">tippecanoe</span></p>
<div class="tt-row"><span class="tt-label">Portland buildings</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.656" aria-hidden="true"></span><span class="tt-val">2.8 GB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.508" aria-hidden="true"></span><span class="tt-val">2.2 GB</span></span>
</div>
<div class="tt-row"><span class="tt-label">Oregon roads</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:1.000" aria-hidden="true"></span><span class="tt-val">4.2 GB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.607" aria-hidden="true"></span><span class="tt-val">2.6 GB</span></span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.997" aria-hidden="true"></span><span class="tt-val">4.2 GB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.575" aria-hidden="true"></span><span class="tt-val">2.4 GB</span></span>
</div>
<div class="tt-row"><span class="tt-label">New York City places</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.474" aria-hidden="true"></span><span class="tt-val">2.0 GB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.330" aria-hidden="true"></span><span class="tt-val">1.4 GB</span></span>
</div>
</figure>
<!-- /bench -->

<!-- bench:e2e-corpus/speedup -->
The 43× figure is the geometric mean across Portland, Oregon, and Moldova. The New York City runs are shown separately because the archives end at different zoom levels, z14 for tylertoo and z16 for tippecanoe.
<!-- /bench -->

## tylertoo memory profiles: `auto` vs. `bounded`

<!-- bench:release-cells/profiles -->
The `bounded` profile uses less peak memory and takes longer to build. On New Zealand parcels, it reduced peak memory from 6.8 GiB to 2.2 GiB and increased build time from 22.9 s to 59.2 s.

<figure class="tt-chart"><figcaption>Build time</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">auto</span> <span class="tt-key tt-b">bounded</span></p>
<div class="tt-row"><span class="tt-label">Madagascar admin boundaries</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.012" aria-hidden="true"></span><span class="tt-val">0.7 s</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.024" aria-hidden="true"></span><span class="tt-val">1.4 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.044" aria-hidden="true"></span><span class="tt-val">2.6 s</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.106" aria-hidden="true"></span><span class="tt-val">6.3 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">New Zealand parcels</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.388" aria-hidden="true"></span><span class="tt-val">22.9 s</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:1.000" aria-hidden="true"></span><span class="tt-val">59.2 s</span></span>
</div>
</figure>

<figure class="tt-chart"><figcaption>Peak memory</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">auto</span> <span class="tt-key tt-b">bounded</span></p>
<div class="tt-row"><span class="tt-label">Madagascar admin boundaries</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.140" aria-hidden="true"></span><span class="tt-val">1.0 GiB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.073" aria-hidden="true"></span><span class="tt-val">0.5 GiB</span></span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.470" aria-hidden="true"></span><span class="tt-val">3.2 GiB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.182" aria-hidden="true"></span><span class="tt-val">1.2 GiB</span></span>
</div>
<div class="tt-row"><span class="tt-label">New Zealand parcels</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:1.000" aria-hidden="true"></span><span class="tt-val">6.8 GiB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.320" aria-hidden="true"></span><span class="tt-val">2.2 GiB</span></span>
</div>
</figure>
<!-- /bench -->

<!-- vale Tylertoo-Mechanics.Headings = NO -->
## Streaming Overture buildings from S3 {#streaming-overture-from-s3}
<!-- vale Tylertoo-Mechanics.Headings = YES -->

<!-- bench:overture-buildings-africa/hero -->
<div class="tt-stats">
<p class="tt-stat"><span class="tt-stat-value">20 min</span> <span class="tt-stat-label">build time</span></p>
<p class="tt-stat"><span class="tt-stat-value">527 million</span> <span class="tt-stat-label">buildings</span></p>
<p class="tt-stat"><span class="tt-stat-value">35.6 GB</span> <span class="tt-stat-label">fetched from S3</span></p>
<p class="tt-stat"><span class="tt-stat-value">152.0 GB</span> <span class="tt-stat-label">peak memory</span></p>
</div>
<!-- /bench -->

<details markdown><summary>Command and input</summary>

<!-- bench:overture-buildings-africa/recipe -->
The run used Overture release `2026-10-21.0` and processed 526,532,679 features.

```bash
AWS_REGION=us-west-2 AWS_SKIP_SIGNATURE=true \
tylertoo tiles \
  s3://overturemaps-us-west-2/release/2026-10-21.0/theme=buildings/type=building/ \
  africa.pmtiles \
  --min-zoom 0 \
  --max-zoom 14 \
  --layer-name buildings \
  --tile-buffer 8 \
  --max-compressed-tile-size 500000 \
  --max-tile-size 0 \
  --include-property class \
  --include-property subtype \
  --include-property height \
  --bbox=-18,-35,52,37.5
```
<!-- /bench -->

</details>

## Reproduce these numbers {#reproduce}

`benchmarks/release.sh` rebuilds every number on this page. Numbers from another host are not comparable.

<!-- bench:e2e-corpus/runs -->
Every run used tylertoo v0.9.0, commit `f0e7d29e`, built with `mimalloc`, on aarch64, CPU 0xd40, 64 CPUs, 247 GiB RAM, on 2026-11-27.

| Section | Compared with | Recorded results |
|---|---|---|
| tylertoo vs. tippecanoe | tippecanoe v2.82.0, gpio 1.7.0 | [`e2e-corpus.json`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/results/e2e-corpus.json) |
| Overture buildings from S3 | none | [`overture-buildings-africa.json`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/results/overture-buildings-africa.json) |
| Memory profiles | none | [`release-cells.json`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/results/release-cells.json) |
<!-- /bench -->

<!-- bench:e2e-corpus/method -->
- Each time is the median of three runs with a warm cache.
- Input checksums are pinned in `e2e-corpus.json`.
- tippecanoe's time includes converting GeoParquet to FlatGeobuf with `gpio`. That step is this share of it: Portland 2.8%, Oregon 2.1%, and Moldova 2.3%.
- For tiling alone, without the conversion, the geometric mean is 42×.
- With thinning off (`--verbatim --simplify-factor 1.0`), the geometric mean is 44×.
- New York City is not in the mean: the archives end at different zoom levels, z14 for tylertoo and z16 for tippecanoe.

<details markdown><summary>Exact commands for tylertoo vs. tippecanoe</summary>

- New York City places (point):
    - tylertoo: `target/release/tylertoo tiles '<repo>/corpus/data/gpio/points-nyc-medium.parquet' '<work>/points-nyc-medium/tylertoo.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name points-nyc-medium --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/points-nyc-medium.tylertoo.report.json' --force`.
    - tippecanoe tiling only: `/scratch/tools/tippecanoe-2.82.0/bin/tippecanoe -o '<work>/points-nyc-medium/tippecanoe.pmtiles' -f -Z 0 -z 14 -l points-nyc-medium -b 8 --maximum-tile-bytes 500000 --drop-fraction-as-needed --quiet --drop-densest-as-needed --extend-zooms-if-still-dropping '<work>/points-nyc-medium.tippecanoe-input.fgb'`.
    - GeoParquet to FlatGeobuf: `gpio convert flatgeobuf '<repo>/corpus/data/gpio/points-nyc-medium.parquet' '<work>/points-nyc-medium.gpio.fgb' --overwrite`.
    - tylertoo with thinning off: `target/release/tylertoo tiles '<repo>/corpus/data/gpio/points-nyc-medium.parquet' '<work>/points-nyc-medium.tylertoo-qm.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name points-nyc-medium --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/points-nyc-medium.tylertoo-qm.report.json' --force --verbatim --simplify-factor 1.0`.
- Portland buildings (polygon):
    - tylertoo: `target/release/tylertoo tiles '<repo>/corpus/data/gpio/polygons-portland-medium.parquet' '<work>/polygons-portland-medium/tylertoo.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name polygons-portland-medium --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/polygons-portland-medium.tylertoo.report.json' --force`.
    - tippecanoe tiling only: `/scratch/tools/tippecanoe-2.82.0/bin/tippecanoe -o '<work>/polygons-portland-medium/tippecanoe.pmtiles' -f -Z 0 -z 14 -l polygons-portland-medium -b 8 --maximum-tile-bytes 500000 --drop-fraction-as-needed --quiet --simplification=10 --drop-densest-as-needed --coalesce-densest-as-needed '<work>/polygons-portland-medium.tippecanoe-input.fgb'`.
    - GeoParquet to FlatGeobuf: `gpio convert flatgeobuf '<repo>/corpus/data/gpio/polygons-portland-medium.parquet' '<work>/polygons-portland-medium.gpio.fgb' --overwrite`.
    - tylertoo with thinning off: `target/release/tylertoo tiles '<repo>/corpus/data/gpio/polygons-portland-medium.parquet' '<work>/polygons-portland-medium.tylertoo-qm.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name polygons-portland-medium --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/polygons-portland-medium.tylertoo-qm.report.json' --force --verbatim --simplify-factor 1.0`.
- Oregon roads (line):
    - tylertoo: `target/release/tylertoo tiles '<repo>/corpus/data/gpio/lines-oregon-large.parquet' '<work>/lines-oregon-large/tylertoo.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name lines-oregon-large --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/lines-oregon-large.tylertoo.report.json' --force`.
    - tippecanoe tiling only: `/scratch/tools/tippecanoe-2.82.0/bin/tippecanoe -o '<work>/lines-oregon-large/tippecanoe.pmtiles' -f -Z 0 -z 14 -l lines-oregon-large -b 8 --maximum-tile-bytes 500000 --drop-fraction-as-needed --quiet --simplification=10 --drop-densest-as-needed '<work>/lines-oregon-large.tippecanoe-input.fgb'`.
    - GeoParquet to FlatGeobuf: `gpio convert flatgeobuf '<repo>/corpus/data/gpio/lines-oregon-large.parquet' '<work>/lines-oregon-large.gpio.fgb' --overwrite`.
    - tylertoo with thinning off: `target/release/tylertoo tiles '<repo>/corpus/data/gpio/lines-oregon-large.parquet' '<work>/lines-oregon-large.tylertoo-qm.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name lines-oregon-large --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/lines-oregon-large.tylertoo-qm.report.json' --force --verbatim --simplify-factor 1.0`.
- Moldova parcels (polygon, dense):
    - tylertoo: `target/release/tylertoo tiles '<repo>/corpus/data/gpio/polygons-ftw-moldova-large.parquet' '<work>/polygons-ftw-moldova-large/tylertoo.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name polygons-ftw-moldova-large --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/polygons-ftw-moldova-large.tylertoo.report.json' --force`.
    - tippecanoe tiling only: `/scratch/tools/tippecanoe-2.82.0/bin/tippecanoe -o '<work>/polygons-ftw-moldova-large/tippecanoe.pmtiles' -f -Z 0 -z 14 -l polygons-ftw-moldova-large -b 8 --maximum-tile-bytes 500000 --drop-fraction-as-needed --quiet --simplification=10 --drop-densest-as-needed --coalesce-densest-as-needed '<work>/polygons-ftw-moldova-large.tippecanoe-input.fgb'`.
    - GeoParquet to FlatGeobuf: `gpio convert flatgeobuf '<repo>/corpus/data/gpio/polygons-ftw-moldova-large.parquet' '<work>/polygons-ftw-moldova-large.gpio.fgb' --overwrite`.
    - tylertoo with thinning off: `target/release/tylertoo tiles '<repo>/corpus/data/gpio/polygons-ftw-moldova-large.parquet' '<work>/polygons-ftw-moldova-large.tylertoo-qm.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name polygons-ftw-moldova-large --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/polygons-ftw-moldova-large.tylertoo-qm.report.json' --force --verbatim --simplify-factor 1.0`.

</details>
<!-- /bench -->

<details markdown><summary>Every recorded figure for tylertoo vs. tippecanoe</summary>

<!-- bench:e2e-corpus/figures -->
| Dataset | Features | tylertoo | tippecanoe end to end | tippecanoe tiling only | gpio conversion | Speedup | Speedup, thinning off | tylertoo archive | tippecanoe archive |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| Portland buildings (polygon) | 813,456 | 2.0 s | 84.4 s | 82.0 s | 2.4 s | 42× | 43× | 99.4 MB | 73.7 MB |
| Oregon roads (line) | 1,203,997 | 4.1 s | 177.2 s | 173.5 s | 3.7 s | 43× | 45× | 306.4 MB | 292.2 MB |
| Moldova parcels (polygon, dense) | 631,910 | 3.9 s | 167.0 s | 163.2 s | 3.8 s | 43× | 43× | 177.5 MB | 166.4 MB |
| New York City places (point), not in the mean | 630,065 | 0.9 s | 42.2 s | 40.4 s | 1.8 s | 45× | 13× | 50.0 MB | 95.0 MB |
<!-- /bench -->

</details>

<!-- bench:overture-buildings-africa/history -->
<details markdown><summary>Earlier Overture runs on other or unrecorded hardware</summary>

| Run | Host | Overture release | Wall time |
|---|---|---|---|
| an unreleased commit afb6b150 after v0.8.0 | aarch64, CPU 0xd40, 64 CPUs, memory not recorded | 2026-10-21.0 | 54 min |

</details>
<!-- /bench -->

The [benchmark method](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/README.md) describes the harness in full.
