# Coalesce benchmark

This benchmark measures what a generalization setting costs (wall time and
peak memory for each stage) and what it draws (how much of the source area
each zoom covers, as a web map renders it). It started with #745's
investigation (`RESULTS.md`, the decision record). #755 turned it into a
command anyone can rerun, so changes to generalization (#750 first) can be
compared with fixed numbers.

```bash
uv run benchmarks/coalesce/run.py --profile small
uv run benchmarks/coalesce/run.py --profile small --compare benchmarks/coalesce/baseline/small.json
```

## What it does

`run.py` reads `bench.toml` and runs six steps. Each step's output is cached
under `--work` (default `~/bench-data/coalesce`), so a rerun skips any finished
work whose inputs haven't changed:

| Step | What | Cached by |
|---|---|---|
| `fetch` | The profile's inputs from the [`bench-coalesce-v1`](https://github.com/geoparquet-io/tylertoo/releases/tag/bench-coalesce-v1) release, plus MapLibre GL JS 4.7.1 and pmtiles.js 3.2.1, each checked against its sha256 in `bench.toml` | file and sha256 |
| `build` | `cargo build --release --bin tylertoo`; skipped with `--bin PATH` | cargo |
| `tile` | `tylertoo overview` then `export-pmtiles` for each dataset and setting, `repeats` times each, under GNU time with a wall and RSS cap (`run_matrix.run_cell`) | binary sha256, input, flags, repeats |
| `tippecanoe` | The pinned tippecanoe (`benchmarks/e2e/setup_tippecanoe.sh`, 2.79.0) on FlatGeobuf input, with #745's arguments | tippecanoe commit, input, arguments |
| `score` | `render_maplibre.py` for each window, `render_coverage.py` and `tile_metrics.py` for each check | archive sha256s and the entry's config |
| `report` | `report-<profile>.json` and `.md` in `<work>/runs/<binary>/`; the latest run's also as `<work>/report.json` and `report.md` | — |

`--only tile,report` runs only the named steps. Any step you leave out must
already be cached.

Each run waits until the 1-minute load average drops below a tenth of the
core count, and at least 3 (`--load-max`; 6.4 on 64 cores),
and records the load before and after. Every cell records whether its repeats
wrote identical bytes (`repeats_identical` in `report.json`).

## Profiles

| Profile | Datasets | Settings | Time | Disk |
|---|---|---|---|---|
| `small` | Madagascar adm4 (17k polygons), FTW Moldova (632k) | before #745, defaults, `--simplify-factor 0.25`, `--simplify-factor 0` | about 20 min: 8.5 min tiling, of which 6 min is the `sf0` export ([#758](https://github.com/geoparquet-io/tylertoo/issues/758)), and about 9 min scoring. The first run adds about 2 min of environment setup | about 3 GB |
| `full` | Adds NZ Parcels (4.3M) | Adds tippecanoe on NZ | about 1 h 30 min on the first run, of which tippecanoe takes about 30 min; reruns use the cached tippecanoe archive | about 30 GB |

The `small` profile needs no keys and no network access beyond the release
and the CDN.

## Setup

- [uv](https://docs.astral.sh/uv/), Rust (for the `build` step), `curl`, and
  GNU time at `/usr/bin/time` (`apt install time`). Timing and RSS come from
  `/proc` and GNU time, so the benchmark runs on Linux only.
- Headless Chromium for the MapLibre windows, installed once:

  ```bash
  uvx --from playwright==1.64.0 playwright install --with-deps chromium-headless-shell
  ```

  `--with-deps` installs Chromium's system libraries with apt (it asks for
  sudo). On Ubuntu without sudo, `uvx --from playwright==1.64.0 playwright
  install-deps --dry-run chromium-headless-shell` simulates the apt install
  and exits nonzero if packages are missing, so an administrator can see
  which ones to install. Rendering uses SwiftShader (software
  WebGL), so the machine needs no GPU.
- For `full`: the libraries tippecanoe builds against
  (`apt install build-essential libsqlite3-dev zlib1g-dev`).

## Reading the report

**Performance**: the median of the repeats for each stage, for convert
(`tylertoo overview`) and export (`export-pmtiles`). "End to end" is their sum,
and the ratio compares it with the before-#745 settings on the same host.
Seconds depend on the host. Compare ratios across hosts, and seconds only
within one host. A local `cargo build` uses glibc's allocator. The released
musl binaries use mimalloc and export faster in absolute terms, so compare
ratios against them too.

**Rendered coverage (MapLibre)**: each archive is drawn by MapLibre GL JS in
headless Chromium over a fixed window, then compared pixel by pixel with the
source, which is rasterized by the same pixel-center rule. Each cell is the
share of the source area the map leaves empty. The share the map fills where
the source has nothing is shown in parentheses when it's 0.5% or more.
Pictures are in `<work>/runs/<binary>/score/windows/<window>/`: magenta
marks missing area, orange marks added area.

**Tile coverage and drawn share**: per-tile checks that rasterize decoded
tiles at 256 px (`render_coverage.py`) or 128 px (`tile_metrics.py`) per tile.
Both resolutions are coarse for small parcels, so use these numbers to compare
settings, not as absolute values.

**Noise floor**: at full detail, small fields show about 2.5% missing and 2.5%
added area from tile-grid snapping alone. With `--compare`, each value shows
its change from the baseline. The report marks a change with ▲ or ▼ only when
it is beyond 3 percentage points (coverage) or 10% (median time), set in
`bench.toml` under `[noise]`. Smaller changes are not regressions.

## Baselines

`baseline/` holds a report for each profile, with the host it was made on:
c7g.16xlarge, 64 Arm cores, a glibc build. Each timed run started with the
load average below 6.4 (a tenth of the cores). To compare a change against it:

```bash
uv run benchmarks/coalesce/run.py --profile small --compare benchmarks/coalesce/baseline/small.json
```

When a PR changes generalization on purpose, replace the baseline in the same
PR: copy `<work>/runs/<binary>/report-<profile>.json` and `.md` to
`baseline/<profile>.*`.

## Findings (`baseline/`, `fc64fe1`)

- **`--simplify-factor 0` makes the merge valve very slow on dense fields
  ([#758](https://github.com/geoparquet-io/tylertoo/issues/758)).** The
  Moldova export takes 117.6 s and 7.7 GiB, against 6.7 s and 1.8 GiB with the
  defaults. Nearly all of it is MVT encoding at z6–z11:
  - tiles there need 13–24 merge passes;
  - each pass re-encodes full-resolution geometry;
  - encode CPU totals 3,111 s, against 60 s with the defaults.

  NZ Parcels did not show this in #745, because it was measured with a 2 MB
  gzip cap instead of the 500K raw cap.
- **Cost of the shipped defaults** relative to before #745, end to end:
  1.51× on Moldova and 2.42× on Madagascar. The Madagascar times are only a
  few seconds, so its ratio carries more noise. `--simplify-factor 0.25`
  costs 3.15× on Moldova.
- **Coverage gained, Moldova whole country, missing area at z5:**
  - before #745: 71.6%;
  - defaults: 22.3%;
  - `--simplify-factor 0.25`: 13.5%;
  - `--simplify-factor 0`: 12.3%.

  The added area (25% at z5, 51% at z3) is the placeholder squares spreading
  into gaps between fields
  ([#750](https://github.com/geoparquet-io/tylertoo/issues/750)).
- **Before #745, coverage depended on the maximum zoom.** Madagascar's
  before-#745 archive misses 65.3% of the z4 window at z6 when built to z12,
  and 43.6% when built to z10, which is the `RESULTS.md` figure. With the
  defaults, the maximum zoom makes no difference to coverage at these zooms.
- **Repeats wrote identical bytes in every cell.** Re-scoring the same
  archives reproduced every MapLibre and tile-check value exactly.

## Relation to `RESULTS.md`

`RESULTS.md` records #745 as it was investigated. Its numbers come from builds
made during that work, some of which predate later changes on the branch: the
merge threshold (`f323c9f`) and the one-GSD placeholder square. `run.py`
measures the current binary on the same inputs, windows and settings. The
small profile reproduces the Madagascar and Moldova rows within the noise
floor:

- MapLibre Moldova window: every value matches within 0.6 percentage points.
- MapLibre Madagascar window: the defaults and both simplify factors match
  within 0.2 points.
- The Madagascar z8 and z6 tile checks match exactly: 48.59% → 0.23%, and
  1,028 → 7,782 features.
- Moldova drawn share (z6/z8/z10 median, worst z10):
  - before #745: 0.56/0.90/0.97 (0.70);
  - defaults: 0.79/0.94/0.98 (0.83);
  - the z8 seam excess is 0.39, as in the merge-threshold section.

The one exception is Madagascar's before-#745 MapLibre column. `RESULTS.md`
built that archive to `--max-zoom 10`; this benchmark builds every Madagascar
archive to z12 (see Findings above). Other differences from the #745 runs:

- Each dataset's archives are all built from its Hilbert-sorted input with
  one tool chain. #745 built some Madagascar archives from the unsorted
  fixture with `tylertoo tiles`, and its Auckland-city archives from a subset
  of NZ.
- The `tippecanoe` column covers NZ only, as in #745.
- Not covered here: the Mato Grosso scaling ladder, the row-order runs and the
  simplify sweep. Run those by hand with `run_matrix.py --stage L|G2` and
  `sweep_simplify.py`.

## Inputs

The inputs are assets on the `bench-coalesce-v1` release. See its
[`ATTRIBUTION.md`](ATTRIBUTION.md) for sources and licenses:

- FTW Moldova and NZ Parcels: CC BY 4.0.
- Madagascar: ODbL 1.0, from FieldMaps over OCHA COD-AB.

The benchmark reads the `*-hilbert.parquet` files. The raw files are there
for provenance.

To regenerate or add an input:

```bash
# NZ Parcels from LINZ (needs a free API key in ~/.linz_key, never committed)
uv run benchmarks/coalesce/fetch_nz_parcels.py raw/nz-parcels.parquet
# Sort into every row order with the pinned gpio
uv run benchmarks/coalesce/prepare_data.py --out <dir> --dataset nz-parcels=raw/nz-parcels.parquet
```

Then upload the files under a **new** tag (`bench-coalesce-v2`). Never replace
an asset under an existing tag. Update `release` and the sha256 values in
`bench.toml`, and regenerate the baselines.

## Tests

```bash
uv run --no-project --with pytest pytest benchmarks/coalesce/test_run.py
```
