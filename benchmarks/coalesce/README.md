# Coalesce benchmark

Use this benchmark to measure wall time and peak memory for each stage of a
generalization setting. It also measures how much source area a web map
covers at each zoom. The benchmark comes from the #745 investigation,
recorded in `RESULTS.md`. The command added for #755 lets you rerun those
measurements and compare generalization changes (#750 first) with fixed
numbers.

```bash
uv run benchmarks/coalesce/run.py --profile small
uv run benchmarks/coalesce/run.py --profile smoke
uv run benchmarks/coalesce/run.py --profile small --compare benchmarks/coalesce/baseline/small.json
```

## Benchmark steps

`run.py` reads `bench.toml` and runs six steps. It caches each step's output
under `--work` (default `~/bench-data/coalesce`). When you rerun the benchmark,
it skips completed work if the inputs haven't changed:

| Step | What | Cached by |
|---|---|---|
| `fetch` | The profile's inputs from the [`bench-coalesce-v1`](https://github.com/geoparquet-io/tylertoo/releases/tag/bench-coalesce-v1) release, plus MapLibre GL JS 4.7.1 and pmtiles.js 3.2.1, each checked against its sha256 in `bench.toml` | file and sha256 |
| `build` | `cargo build --release --bin tylertoo`; skipped with `--bin PATH` | cargo |
| `tile` | `tylertoo overview` then `export-pmtiles` for each dataset and setting, `repeats` times each, under GNU time with a wall and RSS cap (`run_matrix.run_cell`) | binary sha256, `run_matrix.py`, input, flags, repeats |
| `tippecanoe` | The pinned tippecanoe (`benchmarks/e2e/setup_tippecanoe.sh`, 2.79.0) on FlatGeobuf input, with #745's arguments | tippecanoe commit, input, arguments, gpio version |
| `score` | `render_maplibre.py` for each window, `render_coverage.py` and `tile_metrics.py` for each check | archive sha256s, input, scorer script, and the entry's config |
| `report` | `report-<profile>.json` and `.md` in `<work>/runs/<binary>/`; the latest run's also as `<work>/report.json` and `report.md` | — |

A failed run is a result: the benchmark caches it and shows it in the
report, for tylertoo cells and tippecanoe alike. To retry a failed
tippecanoe run, delete `<work>/tippecanoe/<dataset>-<key>/`. A run that a
wall-time or memory cap stopped (`TIMEOUT`, `RSS_CAP`) isn't cached, so the
next run tries it again.

tippecanoe's `-M` caps the compressed tile, as tylertoo's default does since
[#761](https://github.com/geoparquet-io/tylertoo/issues/761) (500,000 bytes
against tylertoo's 512,000). tylertoo also caps the raw tile at 2M;
tippecanoe has no raw cap. tippecanoe writes its temporary files under
`--work`, not `/tmp`.

Use `--only tile,report` to run only the named steps. The cache must already
contain the output of any step you omit. Without the `fetch` step, the
benchmark still checks each input against its sha256.

The scorer scripts pin their Python dependencies with uv's `exclude-newer`,
so a clean checkout resolves the same versions later.

Each run waits until the 1-minute load average drops below the load limit,
then records the load before and after. The limit is a tenth of the core
count, with a minimum of 3 (`--load-max`; 6.4 on 64 cores). Every cell records
whether its repeats write identical bytes (`repeats_identical` in `report.json`).

## Profiles

| Profile | Datasets | Settings | Time | Disk |
|---|---|---|---|---|
| `smoke` | Madagascar adm4 (17k polygons), FTW Moldova (632k) | defaults, `--coverage-fill` | about 2 min of tiling; nothing is scored (`score = false`). The report's Work table sums each export's counters (valve passes, fill polygons and vertices), which are exact: compare them before the timings | about 1 GB |
| `small` | Madagascar adm4 (17k polygons), FTW Moldova (632k) | before #745, 500K raw cap (before #761), defaults, `--simplify-factor 0.25`, `--simplify-factor 0`, `--coverage-fill` | about 13 min: 3½ min tiling and about 9 min scoring, and the `--coverage-fill` cells add about 1 min of tiling and their scoring. The first run adds about 2 min of environment setup | about 3 GB |
| `full` | Adds NZ Parcels (4.3M) | Adds tippecanoe on NZ and Moldova | about 50 min of tylertoo tiling and scoring. The first run adds tippecanoe: about 32 min on NZ and 34 min on Moldova, where it fails. Both results are cached | about 20 GB at peak |

You can run the `small` profile without keys. It needs network access to
the release, the CDN, PyPI (through uv) and, for the `build` step,
crates.io. The `full` profile also clones tippecanoe from GitHub.

## Setup

- [uv](https://docs.astral.sh/uv/), Rust and `protoc` (for the `build` step;
  `apt install protobuf-compiler`), `curl`, and GNU time at `/usr/bin/time`
  (`apt install time`). Timing and RSS come from
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

## Read the report

**Performance**: the report shows the median of the repeats for conversion
(`tylertoo overview`) and export (`export-pmtiles`). "End to end" is the sum
of these medians. The ratio compares this sum with the before-#745 settings
on the same host.
Seconds depend on the host. Compare ratios across hosts, and seconds only
within one host. A `cargo build` with default features uses mimalloc, like
the released musl binaries (#739). A `--no-default-features` build uses
glibc's allocator and exports slower in absolute terms, so compare ratios
against it.

**Rendered coverage (MapLibre)**: MapLibre GL JS draws each archive in
headless Chromium over a fixed window. The benchmark compares the rendering
pixel by pixel with the source, rasterized by the same pixel-center rule.
Each cell shows the share of source area the map leaves empty. Parentheses
show the share the map fills outside the source when it is 0.5% or more.
You can find the images in `<work>/runs/<binary>/score/windows/<window>-<key>/`:
magenta marks missing area, orange marks added area.

**Tile coverage and drawn share**: these checks rasterize decoded tiles at
256 px (`render_coverage.py`) or 128 px (`tile_metrics.py`) per tile.
Both resolutions are coarse for small parcels, so use these numbers to compare
settings, not as absolute values.

**Noise floor**: at full detail, small fields show about 2.5% missing and 2.5%
added area from tile-grid snapping alone. With `--compare`, each value shows
its change from the baseline. The report marks a change with ▲ or ▼ only when
it is beyond the noise floor in `bench.toml` under `[noise]`: 3 percentage
points for coverage, and 10% for times. For times, the floor widens to the
spread of the repeats when that is larger. For example, Moldova `sf0` exports
vary by about 20% from run to run, so a 15% change there isn't marked.
Smaller changes are not regressions.

When the baseline comes from a different host, the report shows time changes
but doesn't mark them. It marks changes to the ratio column instead, since
the ratio carries across hosts.

Two marks flag cells to treat with care:

- † The load average was at or above the gate when a run of the cell started.
  The gate waits at most 30 minutes, then runs anyway, so the times may be
  high.
- ‡ The cell's repeats wrote different bytes.

## Baselines

`baseline/` contains a report for each profile and identifies the host:
c7g.16xlarge, 64 Arm cores. The `small` report is from a release build at
`fb9ed97` (#788) with default features, so it uses mimalloc (#739). The
`full` report is still from a glibc build at `635322c` (PR #770: the merge
valve's caches and stall rule, #758, and the default caps, #761). Compare
seconds only with the report from the same build. Every run started with the
load average below the gate, so no cell carries a † mark. The NZ tippecanoe
time is seeded from the #745 run.
To compare your change with the baseline, run:

```bash
uv run benchmarks/coalesce/run.py --profile small --compare benchmarks/coalesce/baseline/small.json
```

When your PR intentionally changes generalization, replace the baseline in
the same PR. Copy `<work>/runs/<binary>/report-<profile>.json` and `.md` to
`baseline/<profile>.*`.

## Findings (`baseline/`)

- **`--simplify-factor 0` no longer stalls the merge valve on dense fields
  ([#758](https://github.com/geoparquet-io/tylertoo/issues/758), fixed in
  #770).** The Moldova export took 131 s and 7.7 GiB, against 7.1 s and 1.8
  GiB with the defaults: each merge pass re-encoded full-resolution geometry
  from scratch, 13–24 passes per tile at z6–z11. With each member quantized
  once per tile, unions reused across passes, and a tile that stops shrinking
  merged in one step, it takes 15.2 s and 5.5 GiB, against 6.7 s and 1.9
  GiB. `--simplify-factor 0.25` went from 17.4 s to 7.8 s. NZ, with its
  2 MB gzip cap, has few tiles over and did not move (124 s against 99 s
  for the defaults).
- **tippecanoe can't tile FTW Moldova.** With `-z12 -M 500000
  --coalesce-densest-as-needed`, it stops after 34 min with `Can't increase
  feature gap threshold further` (exit 100). tylertoo finishes the same file
  in 9 s with the defaults, and in 19 s at factor 0. On NZ, tippecanoe
  takes 31 min 41 s, against 110 s for the tylertoo defaults (17.3×).
- **tylertoo's default cap was stricter than tippecanoe's
  ([#761](https://github.com/geoparquet-io/tylertoo/issues/761), fixed in
  #770).** tippecanoe's `-M` caps the gzip-compressed tile; tylertoo's 500K
  default capped the raw tile, about 1.75× stricter on Moldova. The defaults
  are now 500K compressed and 2M raw; the "500K raw (before #761)" column
  keeps the old cap.
- **End-to-end cost of the shipped defaults** compared with before #745:

  | Dataset | Defaults | `--simplify-factor 0.25` |
  |---|---|---|
  | Moldova | 1.38× | 1.77× |
  | NZ | 2.07× | 2.22× |
  | Madagascar | 2.43× | |

  Madagascar's times are around a second, so its ratio varies with noise.
- **Coverage improvement** (missing area, before #745 → defaults → `sf0.25` →
  `sf0`):
  - Moldova whole country at z5: 71.6% → 22.3% → 13.5% → 12.3%.
  - Auckland city at z10: 91.6% → 28.2% → 17.0% → 16.1%; tippecanoe is at
    0.0%.

  The added area on Moldova (25% at z5) comes from placeholder squares
  spreading into the gaps between fields
  ([#750](https://github.com/geoparquet-io/tylertoo/issues/750)).
- **Before #745, coverage depended on the maximum zoom.** Madagascar's
  before-#745 archive misses 65.3% of the z4 window at z6 when built to z12,
  but 43.6% when built to z10 (the `RESULTS.md` figure). With the defaults,
  the maximum zoom makes no difference to coverage at these zooms.
- **Determinism.** Every cell's repeats wrote identical bytes. Re-scoring the
  same archives reproduced every MapLibre and tile-check value exactly.

## Compare with `RESULTS.md`

`RESULTS.md` records the #745 investigation. Its numbers come from builds
made during that work. Some builds predate the merge threshold (`f323c9f`)
and the one-GSD placeholder square changes on the branch. `run.py` measures
the current binary on the same inputs, windows, and settings. The
`small` profile reproduces the Madagascar and Moldova rows within the noise
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

The `full` profile reproduces the NZ rows:

- MapLibre, Auckland city and Upper North Island: every value within 0.6
  points, and the before-#745 and tippecanoe columns are identical.
- palmerj's three z12 tiles:
  - before #745: 67.2/76.4/68.7%;
  - defaults: 97.3/97.9/97.6%, the "thinning off" row;
  - tippecanoe: 99.5/99.6/99.6%.
- Timings: before #745 takes 52 s (57 s in `RESULTS.md`) and the defaults
  take 108 s (106 s). `--simplify-factor 0.25` is faster than in `RESULTS.md`
  (116 s against 156 s), and factor 0 is slower (139 s against 120 s).

The one exception is Madagascar's before-#745 MapLibre column. The
`RESULTS.md` measurements use an archive built to `--max-zoom 10`; this
benchmark builds every Madagascar archive to z12 (see
[Findings](#findings-baseline)). Other differences from the #745 runs:

- Each dataset's archives are all built from its Hilbert-sorted input with
  one tool chain. #745 built some Madagascar archives from the unsorted
  fixture with `tylertoo tiles`, and its Auckland-city archives from a subset
  of NZ.
- The `tippecanoe` column covers NZ only, as in #745.
- This benchmark does not cover the Mato Grosso scaling ladder, the row-order
  runs, or the simplify sweep. Run those manually with
  `run_matrix.py --stage L|G2` and `sweep_simplify.py`.

## Inputs

The `bench-coalesce-v1` release contains the input assets. See
[input sources and licenses in `ATTRIBUTION.md`](ATTRIBUTION.md) for
attribution details:

- FTW Moldova and NZ Parcels: CC BY 4.0.
- Madagascar: ODbL 1.0, from FieldMaps over OCHA COD-AB.

The benchmark reads the `*-hilbert.parquet` files. The release retains the
raw files for provenance.

To regenerate or add an input:

```bash
# NZ Parcels from LINZ (needs a free API key in ~/.linz_key, never committed)
uv run benchmarks/coalesce/fetch_nz_parcels.py raw/nz-parcels.parquet
# Sort into every row order with the pinned gpio
uv run benchmarks/coalesce/prepare_data.py --out <dir> --dataset nz-parcels=raw/nz-parcels.parquet
```

After regenerating or adding an input, upload the files under a new tag
(`bench-coalesce-v2`). Never replace an asset under an existing tag. Update `release` and the sha256 values in
`bench.toml`, and regenerate the baselines.

## Tests

```bash
uv run --no-project --with pytest pytest benchmarks/coalesce/test_run.py
```
