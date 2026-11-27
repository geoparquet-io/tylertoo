# Benchmarks

<!-- bench:e2e-corpus/headline -->
<p class="tt-headline"><strong>43× faster</strong> than tippecanoe</p>
<p class="tt-sub">Whole builds from GeoParquet to PMTiles, the geometric mean over 3 datasets.</p>
<!-- /bench -->

<!-- bench:e2e-corpus/stamp -->
<p class="tt-stamp" markdown>Measured at an unreleased commit f0e7d29e after v0.8.0 on 2026-11-27, against tippecanoe v2.82.0 and gpio 1.7.0. See the [measurement method](#measurement-method).</p>
<!-- /bench -->

<!-- bench:e2e-corpus/speedup-bars -->
<figure class="tt-chart"><figcaption>End-to-end wall time, shorter is faster</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">tylertoo</span> <span class="tt-key tt-b">tippecanoe</span></p>
<div class="tt-row tt-excluded"><span class="tt-label">New York City places (point) <span class="tt-note">not in the mean</span></span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.005" aria-hidden="true"></span><span class="tt-val">0.9 s, 45× faster</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.238" aria-hidden="true"></span><span class="tt-val">42.2 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">Portland buildings (polygon)</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.011" aria-hidden="true"></span><span class="tt-val">2.0 s, 42× faster</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.476" aria-hidden="true"></span><span class="tt-val">84.4 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">Oregon roads (line)</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.023" aria-hidden="true"></span><span class="tt-val">4.1 s, 43× faster</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:1.000" aria-hidden="true"></span><span class="tt-val">177.2 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels (polygon, dense)</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.022" aria-hidden="true"></span><span class="tt-val">3.9 s, 43× faster</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.942" aria-hidden="true"></span><span class="tt-val">167.0 s</span></span>
</div>
</figure>
<details markdown><summary>The figures as a table</summary>

| Dataset | Features | tylertoo | tippecanoe end to end | tippecanoe tiling only | Speedup | Speedup with thinning off | tylertoo archive | tippecanoe archive |
|---|---|---|---|---|---|---|---|---|
| New York City places (point) | 630,065 | 0.9 s | 42.2 s | 40.4 s | 45× faster (max zoom differs) | 13× | 50.0 MB | 95.0 MB |
| Portland buildings (polygon) | 813,456 | 2.0 s | 84.4 s | 82.0 s | 42× faster | 43× | 99.4 MB | 73.7 MB |
| Oregon roads (line) | 1,203,997 | 4.1 s | 177.2 s | 173.5 s | 43× faster | 45× | 306.4 MB | 292.2 MB |
| Moldova parcels (polygon, dense) | 631,910 | 3.9 s | 167.0 s | 163.2 s | 43× faster | 43× | 177.5 MB | 166.4 MB |

</details>
<!-- /bench -->

See [Coming from tippecanoe](guides/tippecanoe.md) for the map comparison.

<!-- vale Tylertoo-Mechanics.Headings = NO -->
## Streaming Overture from S3
<!-- vale Tylertoo-Mechanics.Headings = YES -->

<!-- bench:overture-buildings-africa/hero -->
<div class="tt-stats">
<p class="tt-stat"><span class="tt-stat-value">20 min</span> <span class="tt-stat-label">wall time</span></p>
<p class="tt-stat"><span class="tt-stat-value">527 million</span> <span class="tt-stat-label">features</span></p>
<p class="tt-stat"><span class="tt-stat-value">35.6 GB</span> <span class="tt-stat-label">fetched</span></p>
<p class="tt-stat"><span class="tt-stat-value">152.0 GB</span> <span class="tt-stat-label">peak memory</span></p>
</div>

526,532,679 features from Overture release 2026-10-21.0.
<!-- /bench -->

<!-- bench:overture-buildings-africa/recipe -->
```bash
AWS_REGION=us-west-2 AWS_SKIP_SIGNATURE=true tylertoo tiles s3://overturemaps-us-west-2/release/2026-10-21.0/theme=buildings/type=building/ africa.pmtiles --min-zoom 0 --max-zoom 14 --layer-name buildings --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --include-property class --include-property subtype --include-property height --bbox=-18,-35,52,37.5
```
<!-- /bench -->

The box is a rectangle, so it also covers parts of the Middle East and southern Europe.

<!-- bench:overture-buildings-africa/history -->
<details markdown><summary>Earlier runs on hardware that differs or was not recorded</summary>

| Run | Host | Overture release | Wall time |
|---|---|---|---|
| an unreleased commit afb6b150 after v0.8.0 | aarch64, CPU 0xd40, 64 CPUs, memory not recorded | 2026-10-21.0 | 54 min |

</details>
<!-- /bench -->

## Input types

<!-- bench:release-cells/encodings -->
<figure class="tt-chart"><figcaption>Input encodings, wall time</figcaption>
<div class="tt-row"><span class="tt-label">GeoParquet WKB</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.557" aria-hidden="true"></span><span class="tt-val">2.6 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">native GeoArrow</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:1.000" aria-hidden="true"></span><span class="tt-val">4.7 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">GeoParquet GEOMETRY</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.555" aria-hidden="true"></span><span class="tt-val">2.6 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">GEOMETRY without GeoParquet metadata</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.546" aria-hidden="true"></span><span class="tt-val">2.5 s</span></span>
</div>
</figure>
<details markdown><summary>The figures as a table</summary>

| Dataset | tylertoo |
|---|---|
| GeoParquet WKB | 2.6 s |
| native GeoArrow | 4.7 s |
| GeoParquet GEOMETRY | 2.6 s |
| GEOMETRY without GeoParquet metadata | 2.5 s |

</details>
<!-- /bench -->

<!-- bench:release-cells/sources -->
<figure class="tt-chart"><figcaption>Input sources, wall time</figcaption>
<div class="tt-row"><span class="tt-label">Local file</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.749" aria-hidden="true"></span><span class="tt-val">2.6 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">Multi-file list</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.749" aria-hidden="true"></span><span class="tt-val">2.6 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">Partitioned directory</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.743" aria-hidden="true"></span><span class="tt-val">2.6 s</span></span>
</div>
<div class="tt-row"><span class="tt-label">HTTPS</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:1.000" aria-hidden="true"></span><span class="tt-val">3.5 s</span></span>
</div>
</figure>
<details markdown><summary>The figures as a table</summary>

| Dataset | tylertoo |
|---|---|
| Local file | 2.6 s |
| Multi-file list | 2.6 s |
| Partitioned directory | 2.6 s |
| HTTPS | 3.5 s |

</details>

For `s3://`, see [Streaming Overture from S3](#streaming-overture-from-s3).
<!-- /bench -->

## Memory

<!-- bench:e2e-corpus/memory-bars -->
<figure class="tt-chart"><figcaption>Peak memory, shorter is less</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">tylertoo</span> <span class="tt-key tt-b">tippecanoe</span></p>
<div class="tt-row"><span class="tt-label">New York City places (point)</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.474" aria-hidden="true"></span><span class="tt-val">2.0 GB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.330" aria-hidden="true"></span><span class="tt-val">1.4 GB</span></span>
</div>
<div class="tt-row"><span class="tt-label">Portland buildings (polygon)</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.656" aria-hidden="true"></span><span class="tt-val">2.8 GB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.508" aria-hidden="true"></span><span class="tt-val">2.2 GB</span></span>
</div>
<div class="tt-row"><span class="tt-label">Oregon roads (line)</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:1.000" aria-hidden="true"></span><span class="tt-val">4.2 GB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.607" aria-hidden="true"></span><span class="tt-val">2.6 GB</span></span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels (polygon, dense)</span>
<span class="tt-line"><span class="tt-bar tt-a" style="--f:0.997" aria-hidden="true"></span><span class="tt-val">4.2 GB</span></span>
<span class="tt-line"><span class="tt-bar tt-b" style="--f:0.575" aria-hidden="true"></span><span class="tt-val">2.4 GB</span></span>
</div>
</figure>
<details markdown><summary>The figures as a table</summary>

| Dataset | tylertoo | tippecanoe |
|---|---|---|
| New York City places (point) | 2.0 GB | 1.4 GB |
| Portland buildings (polygon) | 2.8 GB | 2.2 GB |
| Oregon roads (line) | 4.2 GB | 2.6 GB |
| Moldova parcels (polygon, dense) | 4.2 GB | 2.4 GB |

</details>
<!-- /bench -->

<!-- bench:release-cells/profiles -->
<figure class="tt-chart"><figcaption>Memory profiles, wall time</figcaption>
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
<details markdown><summary>The figures as a table</summary>

| Dataset | auto | bounded |
|---|---|---|
| Madagascar admin boundaries | 0.7 s | 1.4 s |
| Moldova parcels | 2.6 s | 6.3 s |
| New Zealand parcels | 22.9 s | 59.2 s |

</details>

<figure class="tt-chart"><figcaption>Memory profiles, peak memory</figcaption>
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
<details markdown><summary>The figures as a table</summary>

| Dataset | auto | bounded |
|---|---|---|
| Madagascar admin boundaries | 1.0 GiB | 0.5 GiB |
| Moldova parcels | 3.2 GiB | 1.2 GiB |
| New Zealand parcels | 6.8 GiB | 2.2 GiB |

</details>
<!-- /bench -->

`bounded` trades wall time for a lower peak. Use it when the input is larger than memory.

## Measurement method

### The mean

<!-- bench:e2e-corpus/speedup -->
- In the mean: Portland buildings (polygon), Oregon roads (line), Moldova parcels (polygon, dense).
- Not in the mean: New York City places (point): the archives reach different max zooms (tylertoo z14, tippecanoe z16).
- tippecanoe's end-to-end time includes converting the GeoParquet input to FlatGeobuf with gpio. That conversion is this share of it: Portland buildings (polygon): 2.8%, Oregon roads (line): 2.1%, Moldova parcels (polygon, dense): 2.3%.
- For tiling alone, the geometric mean is 42×.
- With thinning off, the geometric mean is 44× over 3 datasets.
<!-- /bench -->

### Settings

<!-- bench:e2e-corpus/method -->
- Host: aarch64, CPU 0xd40, 64 CPUs, 247 GiB RAM.
- Rounds: 3.
- Wall time: median of recorded rounds.
- Warm cache: yes.
- Allocator: mimalloc.
- Inputs: pinned by sha256 in `e2e-corpus.json`.

<details markdown><summary>Exact commands</summary>

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

### Runs

<!-- bench:e2e-corpus/runs -->
- Corpus: an unreleased commit f0e7d29e after v0.8.0, tippecanoe v2.82.0, gpio 1.7.0, allocator: mimalloc, aarch64, CPU 0xd40, 64 CPUs, 247 GiB RAM, 2026-11-27.
- Africa: an unreleased commit f0e7d29e after v0.8.0, allocator: mimalloc, aarch64, CPU 0xd40, 64 CPUs, 247 GiB RAM, 2026-11-27.
- Release cells: an unreleased commit f0e7d29e after v0.8.0, allocator: mimalloc, aarch64, CPU 0xd40, 64 CPUs, 247 GiB RAM, 2026-11-27.
<!-- /bench -->

See the [benchmark method](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/README.md) and [recorded results](https://github.com/geoparquet-io/tylertoo/tree/main/benchmarks/results).

Reproduce: `benchmarks/release.sh`

For sharded builds, follow the [Brazil tutorial's sharding step](tutorials/brazil.md#2-split-the-build-across-workers).
