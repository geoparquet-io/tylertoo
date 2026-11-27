# Benchmarks

These measurements compare whole builds from GeoParquet to PMTiles.

<!-- bench:e2e-corpus/stamp -->
Corpus: commit ae0363f, 2026-11-06. Africa: commit afb6b15, 2026-11-07. Release cells: commit afb6b15, 2026-11-07. tippecanoe v2.79.0. gpio 1.7.0.
<!-- /bench -->

## Against tippecanoe

<!-- bench:e2e-corpus/speedup -->
tylertoo is **20×** faster than tippecanoe end to end, the geometric mean over 3 datasets.

Included: Portland buildings (polygon), Oregon roads (line), Moldova parcels (polygon, dense). Excluded: New York City places (point): the archives reach different max zooms (tylertoo z14, tippecanoe z16).

GeoParquet to FlatGeobuf conversion as a share of tippecanoe's total wall time: Portland buildings (polygon): 2.8%, Oregon roads (line): 2.1%, Moldova parcels (polygon, dense): 2.2%. For tiling alone, the geometric mean is 20×. With thinning off, the geometric mean is 16× over 3 datasets.

_Run: commit ae0363f, tippecanoe v2.79.0, gpio 1.7.0, allocator: mimalloc, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

<!-- bench:e2e-corpus/speedup-bars -->
<figure class="tt-chart"><figcaption>End-to-end wall time, shorter is faster</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">tylertoo</span> <span class="tt-key tt-b">tippecanoe</span></p>
<div class="tt-row"><span class="tt-label">New York City places (point)</span>
<span class="tt-bar tt-a" style="--w:0.9%" aria-hidden="true"></span><span class="tt-val">1.6 s, 27× faster (max zoom differs)</span>
<span class="tt-bar tt-b" style="--w:23.6%" aria-hidden="true"></span><span class="tt-val">42.7 s</span>
</div>
<div class="tt-row"><span class="tt-label">Portland buildings (polygon)</span>
<span class="tt-bar tt-a" style="--w:2.0%" aria-hidden="true"></span><span class="tt-val">3.7 s, 23× faster</span>
<span class="tt-bar tt-b" style="--w:46.9%" aria-hidden="true"></span><span class="tt-val">84.8 s</span>
</div>
<div class="tt-row"><span class="tt-label">Oregon roads (line)</span>
<span class="tt-bar tt-a" style="--w:5.9%" aria-hidden="true"></span><span class="tt-val">10.7 s, 17× faster</span>
<span class="tt-bar tt-b" style="--w:100.0%" aria-hidden="true"></span><span class="tt-val">180.7 s</span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels (polygon, dense)</span>
<span class="tt-bar tt-a" style="--w:4.3%" aria-hidden="true"></span><span class="tt-val">7.8 s, 21× faster</span>
<span class="tt-bar tt-b" style="--w:92.6%" aria-hidden="true"></span><span class="tt-val">167.3 s</span>
</div>
</figure>
<details markdown><summary>The figures as a table</summary>

| Dataset | Features | tylertoo | tippecanoe end to end | tippecanoe tiling only | Speedup | Speedup with thinning off | tylertoo archive | tippecanoe archive |
|---|---|---|---|---|---|---|---|---|
| New York City places (point) | 630,065 | 1.6 s | 42.7 s | 40.9 s | 27× faster (max zoom differs) | 2.3× | 61.7 MB | 95.0 MB |
| Portland buildings (polygon) | 813,456 | 3.7 s | 84.8 s | 82.5 s | 23× faster | 14× | 103.9 MB | 73.7 MB |
| Oregon roads (line) | 1,203,997 | 10.7 s | 180.7 s | 177.0 s | 17× faster | 17× | 326.7 MB | 292.2 MB |
| Moldova parcels (polygon, dense) | 631,910 | 7.8 s | 167.3 s | 163.6 s | 21× faster | 18× | 182.7 MB | 183.3 MB |

</details>

_Run: commit ae0363f, tippecanoe v2.79.0, gpio 1.7.0, allocator: mimalloc, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

See [Coming from tippecanoe](guides/tippecanoe.md) for the map comparison.

<!-- vale Tylertoo-Mechanics.Headings = NO -->
## Streaming Overture from S3
<!-- vale Tylertoo-Mechanics.Headings = YES -->

<!-- bench:overture-buildings-africa/hero -->
<div class="tt-hero" markdown>

526,532,679 features. 54 min wall time. 107.1 GB peak memory. 35.6 GB fetched.

</div>

`--profile bounded`: 50 min wall time, 41.1 GB peak memory, same archive.

Overture release: 2026-10-21.0.

_Run: commit afb6b15, allocator: mimalloc, aarch64, CPU 0xd40, 64 CPUs, 2026-11-07._
<!-- /bench -->

<!-- bench:overture-buildings-africa/history -->
54 min at commit afb6b15.

<figure class="tt-chart"><figcaption>Africa wall time, shorter is faster</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">tylertoo</span></p>
<div class="tt-row"><span class="tt-label">commit afb6b15</span>
<span class="tt-bar tt-a" style="--w:100.0%" aria-hidden="true"></span><span class="tt-val">54.1 min</span>
</div>
</figure>
<details markdown><summary>The figures as a table</summary>

| Dataset | tylertoo |
|---|---|
| commit afb6b15 | 54.1 min |

</details>

_Run: commit afb6b15, allocator: mimalloc, aarch64, CPU 0xd40, 64 CPUs, 2026-11-07._
<!-- /bench -->

<!-- bench:overture-buildings-africa/recipe -->
```bash
AWS_REGION=us-west-2 AWS_SKIP_SIGNATURE=true tylertoo tiles s3://overturemaps-us-west-2/release/2026-10-21.0/theme=buildings/type=building/ africa.pmtiles --min-zoom 0 --max-zoom 14 --layer-name buildings --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --include-property class --include-property subtype --include-property height --bbox=-18,-35,52,37.5
```

_Run: commit afb6b15, allocator: mimalloc, aarch64, CPU 0xd40, 64 CPUs, 2026-11-07._
<!-- /bench -->

The box is a rectangle, so it also covers parts of the Middle East and southern Europe.

## Input types

<!-- bench:release-cells/encodings -->
_Not measured yet. `benchmarks/release.sh` fills this chart._
<!-- /bench -->

<!-- bench:release-cells/sources -->
_Not measured yet. `benchmarks/release.sh` fills this chart._
<!-- /bench -->

## Memory

<!-- bench:e2e-corpus/memory-bars -->
<figure class="tt-chart"><figcaption>Peak memory, shorter is less</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">tylertoo</span> <span class="tt-key tt-b">tippecanoe</span></p>
<div class="tt-row"><span class="tt-label">New York City places (point)</span>
<span class="tt-bar tt-a" style="--w:24.4%" aria-hidden="true"></span><span class="tt-val">2.3 GB</span>
<span class="tt-bar tt-b" style="--w:14.9%" aria-hidden="true"></span><span class="tt-val">1.4 GB</span>
</div>
<div class="tt-row"><span class="tt-label">Portland buildings (polygon)</span>
<span class="tt-bar tt-a" style="--w:34.6%" aria-hidden="true"></span><span class="tt-val">3.3 GB</span>
<span class="tt-bar tt-b" style="--w:23.5%" aria-hidden="true"></span><span class="tt-val">2.2 GB</span>
</div>
<div class="tt-row"><span class="tt-label">Oregon roads (line)</span>
<span class="tt-bar tt-a" style="--w:100.0%" aria-hidden="true"></span><span class="tt-val">9.5 GB</span>
<span class="tt-bar tt-b" style="--w:26.9%" aria-hidden="true"></span><span class="tt-val">2.6 GB</span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels (polygon, dense)</span>
<span class="tt-bar tt-a" style="--w:49.9%" aria-hidden="true"></span><span class="tt-val">4.7 GB</span>
<span class="tt-bar tt-b" style="--w:26.9%" aria-hidden="true"></span><span class="tt-val">2.5 GB</span>
</div>
</figure>
<details markdown><summary>The figures as a table</summary>

| Dataset | tylertoo | tippecanoe |
|---|---|---|
| New York City places (point) | 2.3 GB | 1.4 GB |
| Portland buildings (polygon) | 3.3 GB | 2.2 GB |
| Oregon roads (line) | 9.5 GB | 2.6 GB |
| Moldova parcels (polygon, dense) | 4.7 GB | 2.5 GB |

</details>

_Run: commit ae0363f, tippecanoe v2.79.0, gpio 1.7.0, allocator: mimalloc, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

<!-- bench:release-cells/profiles -->
<figure class="tt-chart"><figcaption>Memory profiles, wall time</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">auto</span> <span class="tt-key tt-b">bounded</span></p>
<div class="tt-row"><span class="tt-label">Madagascar admin boundaries</span>
<span class="tt-bar tt-a" style="--w:1.5%" aria-hidden="true"></span><span class="tt-val">1.1 s</span>
<span class="tt-bar tt-b" style="--w:1.6%" aria-hidden="true"></span><span class="tt-val">1.3 s</span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels</span>
<span class="tt-bar tt-a" style="--w:5.5%" aria-hidden="true"></span><span class="tt-val">4.3 s</span>
<span class="tt-bar tt-b" style="--w:7.1%" aria-hidden="true"></span><span class="tt-val">5.6 s</span>
</div>
<div class="tt-row"><span class="tt-label">New Zealand parcels</span>
<span class="tt-bar tt-a" style="--w:73.7%" aria-hidden="true"></span><span class="tt-val">58.2 s</span>
<span class="tt-bar tt-b" style="--w:100.0%" aria-hidden="true"></span><span class="tt-val">79.0 s</span>
</div>
</figure>
<details markdown><summary>The figures as a table</summary>

| Dataset | auto | bounded |
|---|---|---|
| Madagascar admin boundaries | 1.1 s | 1.3 s |
| Moldova parcels | 4.3 s | 5.6 s |
| New Zealand parcels | 58.2 s | 79.0 s |

</details>

<figure class="tt-chart"><figcaption>Memory profiles, peak memory</figcaption>
<p class="tt-legend"><span class="tt-key tt-a">auto</span> <span class="tt-key tt-b">bounded</span></p>
<div class="tt-row"><span class="tt-label">Madagascar admin boundaries</span>
<span class="tt-bar tt-a" style="--w:14.2%" aria-hidden="true"></span><span class="tt-val">1.4 GiB</span>
<span class="tt-bar tt-b" style="--w:12.6%" aria-hidden="true"></span><span class="tt-val">1.2 GiB</span>
</div>
<div class="tt-row"><span class="tt-label">Moldova parcels</span>
<span class="tt-bar tt-a" style="--w:37.4%" aria-hidden="true"></span><span class="tt-val">3.7 GiB</span>
<span class="tt-bar tt-b" style="--w:27.9%" aria-hidden="true"></span><span class="tt-val">2.7 GiB</span>
</div>
<div class="tt-row"><span class="tt-label">New Zealand parcels</span>
<span class="tt-bar tt-a" style="--w:100.0%" aria-hidden="true"></span><span class="tt-val">9.8 GiB</span>
<span class="tt-bar tt-b" style="--w:48.0%" aria-hidden="true"></span><span class="tt-val">4.7 GiB</span>
</div>
</figure>
<details markdown><summary>The figures as a table</summary>

| Dataset | auto | bounded |
|---|---|---|
| Madagascar admin boundaries | 1.4 GiB | 1.2 GiB |
| Moldova parcels | 3.7 GiB | 2.7 GiB |
| New Zealand parcels | 9.8 GiB | 4.7 GiB |

</details>

_Run: commit afb6b15, allocator: mimalloc, aarch64, CPU 0xd40, 64 CPUs, 2026-11-07._
<!-- /bench -->

`bounded` trades wall time for a lower peak. Use it when the input is larger than memory.

## Measurement method

<!-- bench:e2e-corpus/method -->
- Host: Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39.
- Rounds: 3.
- Wall time: median of recorded rounds.
- Warm cache: yes.
- Allocator: mimalloc.
- Inputs: pinned by sha256 in `e2e-corpus.json`.

<details markdown><summary>Exact commands</summary>

- New York City places (point):
    - tylertoo: `'<repo>/target/release/tylertoo' tiles '<repo>/corpus/data/gpio/points-nyc-medium.parquet' '<work>/points-nyc-medium.tylertoo.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name points-nyc-medium --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/points-nyc-medium.tylertoo.report.json' --force`.
    - tippecanoe tiling only: `'<repo>/.tools/tippecanoe-2.79.0/bin/tippecanoe' -o '<work>/points-nyc-medium.tippecanoe-fgb.pmtiles' -f -Z 0 -z 14 -l points-nyc-medium -b 8 --maximum-tile-bytes 500000 --drop-fraction-as-needed --quiet --drop-densest-as-needed --extend-zooms-if-still-dropping '<work>/points-nyc-medium.tippecanoe-input.fgb'`.
    - GeoParquet to FlatGeobuf: `gpio convert flatgeobuf '<repo>/corpus/data/gpio/points-nyc-medium.parquet' '<work>/points-nyc-medium.gpio.fgb' --overwrite`.
    - tylertoo with thinning off: `'<repo>/target/release/tylertoo' tiles '<repo>/corpus/data/gpio/points-nyc-medium.parquet' '<work>/points-nyc-medium.tylertoo-qm.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name points-nyc-medium --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/points-nyc-medium.tylertoo-qm.report.json' --force --verbatim --simplify-factor 1.0`.
- Portland buildings (polygon):
    - tylertoo: `'<repo>/target/release/tylertoo' tiles '<repo>/corpus/data/gpio/polygons-portland-medium.parquet' '<work>/polygons-portland-medium.tylertoo.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name polygons-portland-medium --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/polygons-portland-medium.tylertoo.report.json' --force`.
    - tippecanoe tiling only: `'<repo>/.tools/tippecanoe-2.79.0/bin/tippecanoe' -o '<work>/polygons-portland-medium.tippecanoe-fgb.pmtiles' -f -Z 0 -z 14 -l polygons-portland-medium -b 8 --maximum-tile-bytes 500000 --drop-fraction-as-needed --quiet --simplification=10 --drop-densest-as-needed --coalesce-densest-as-needed '<work>/polygons-portland-medium.tippecanoe-input.fgb'`.
    - GeoParquet to FlatGeobuf: `gpio convert flatgeobuf '<repo>/corpus/data/gpio/polygons-portland-medium.parquet' '<work>/polygons-portland-medium.gpio.fgb' --overwrite`.
    - tylertoo with thinning off: `'<repo>/target/release/tylertoo' tiles '<repo>/corpus/data/gpio/polygons-portland-medium.parquet' '<work>/polygons-portland-medium.tylertoo-qm.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name polygons-portland-medium --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/polygons-portland-medium.tylertoo-qm.report.json' --force --verbatim --simplify-factor 1.0`.
- Oregon roads (line):
    - tylertoo: `'<repo>/target/release/tylertoo' tiles '<repo>/corpus/data/gpio/lines-oregon-large.parquet' '<work>/lines-oregon-large.tylertoo.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name lines-oregon-large --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/lines-oregon-large.tylertoo.report.json' --force`.
    - tippecanoe tiling only: `'<repo>/.tools/tippecanoe-2.79.0/bin/tippecanoe' -o '<work>/lines-oregon-large.tippecanoe-fgb.pmtiles' -f -Z 0 -z 14 -l lines-oregon-large -b 8 --maximum-tile-bytes 500000 --drop-fraction-as-needed --quiet --simplification=10 --drop-densest-as-needed '<work>/lines-oregon-large.tippecanoe-input.fgb'`.
    - GeoParquet to FlatGeobuf: `gpio convert flatgeobuf '<repo>/corpus/data/gpio/lines-oregon-large.parquet' '<work>/lines-oregon-large.gpio.fgb' --overwrite`.
    - tylertoo with thinning off: `'<repo>/target/release/tylertoo' tiles '<repo>/corpus/data/gpio/lines-oregon-large.parquet' '<work>/lines-oregon-large.tylertoo-qm.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name lines-oregon-large --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/lines-oregon-large.tylertoo-qm.report.json' --force --verbatim --simplify-factor 1.0`.
- Moldova parcels (polygon, dense):
    - tylertoo: `'<repo>/target/release/tylertoo' tiles '<repo>/corpus/data/gpio/polygons-ftw-moldova-large.parquet' '<work>/polygons-ftw-moldova-large.tylertoo.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name polygons-ftw-moldova-large --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/polygons-ftw-moldova-large.tylertoo.report.json' --force`.
    - tippecanoe tiling only: `'<repo>/.tools/tippecanoe-2.79.0/bin/tippecanoe' -o '<work>/polygons-ftw-moldova-large.tippecanoe-fgb.pmtiles' -f -Z 0 -z 14 -l polygons-ftw-moldova-large -b 8 --maximum-tile-bytes 500000 --drop-fraction-as-needed --quiet --simplification=10 --drop-densest-as-needed --coalesce-densest-as-needed '<work>/polygons-ftw-moldova-large.tippecanoe-input.fgb'`.
    - GeoParquet to FlatGeobuf: `gpio convert flatgeobuf '<repo>/corpus/data/gpio/polygons-ftw-moldova-large.parquet' '<work>/polygons-ftw-moldova-large.gpio.fgb' --overwrite`.
    - tylertoo with thinning off: `'<repo>/target/release/tylertoo' tiles '<repo>/corpus/data/gpio/polygons-ftw-moldova-large.parquet' '<work>/polygons-ftw-moldova-large.tylertoo-qm.pmtiles' --min-zoom 0 --max-zoom 14 --layer-name polygons-ftw-moldova-large --tile-buffer 8 --max-compressed-tile-size 500000 --max-tile-size 0 --report '<work>/polygons-ftw-moldova-large.tylertoo-qm.report.json' --force --verbatim --simplify-factor 1.0`.

</details>

_Run: commit ae0363f, tippecanoe v2.79.0, gpio 1.7.0, allocator: mimalloc, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

See the [benchmark method](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/README.md) and [recorded results](https://github.com/geoparquet-io/tylertoo/tree/main/benchmarks/results).

Reproduce: `benchmarks/release.sh`

For sharded builds, follow the [Brazil tutorial's sharding step](tutorials/brazil.md#2-split-the-same-build-into-shard-jobs).
