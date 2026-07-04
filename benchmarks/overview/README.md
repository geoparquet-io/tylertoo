# benchmarks/overview

V3 access + storage benchmark for **overview GeoParquet** vs the status-quo
web-map deployment (**gpio GeoParquet + tippecanoe PMTiles**) vs **COGP**
(`cogp-rs`). The publishable numbers and full method notes live in
[`RESULTS.md`](./RESULTS.md); pipeline performance methodology + history
live in [`PROFILE.md`](./PROFILE.md); this README is the operator's guide.

## What it measures

1. **Storage** — on-disk size of each artifact per dataset, plus overhead
   percentages and the single-file-vs-two-artifact comparison.
2. **Bytes-per-viewport (headline)** — total bytes, HTTP request count, and
   wall time to satisfy three canonical viewports (world / regional / street)
   for the overview read protocol (DuckDB httpfs, `WHERE level = k AND bbox
   overlap`) vs the PMTiles tile-range protocol, over a byte-range HTTP
   server that logs every response's byte count.
3. **Conversion cost** — wall time + peak RSS of `gpq-tiles overview` vs the
   `gpio convert geojson | tippecanoe` pipeline on the same input.

## Datasets

`points-nyc-medium`, `lines-portland-medium`, `polygons-portland-medium`,
`polygons-ftw-moldova-large` — the medium point/line/polygon tiers plus the
large dense-polygon stress case. Inputs are the gpio-optimized corpus in
`corpus/data/gpio/` (rebuild with `corpus/fetch.sh` + `corpus/optimize.sh`).
tippecanoe goldens come from `corpus/goldens.sh`.

## Files

| file | role |
|---|---|
| `run_all.sh` | orchestrator: regenerate overview files, then storage + conversion + access |
| `make_viewports.py` | derive the 3 viewport rectangles per dataset from the inputs → `viewports.json` |
| `logging_server.py` | byte-range HTTP server with per-request byte/request logging; `/__reset` + `/__stats` control endpoints |
| `bench_storage.py` | storage table → `storage_results.json` + markdown |
| `bench_access.py` | the headline access benchmark → `access_results.json` + markdown |
| `format_access.py` | render `access_results.json` into the RESULTS.md tables |
| `bench_access_remote.py` | the same access benchmark over real S3 (issue #176) → `remote_access_results.json` |
| `format_remote.py` | render `remote_access_results.json` into the RESULTS.md §2b tables + `remote_access_chart.svg` |
| `run_conversion.sh` | conversion-cost table (overview vs tippecanoe, `/usr/bin/time -v`) |

Raw/large outputs (regenerated overview parquet, per-run logs, timing
captures) are written under `corpus/data/bench/` which is **gitignored**.
Only the scripts, `viewports.json`, `RESULTS.md`, and the small
`*_results.json` are intended to be committed.

## Run

```bash
# release binary (once)
cargo build --release --package gpq-tiles

# everything (regenerates overview files first; Moldova dup ~11 min)
benchmarks/overview/run_all.sh
```

Or piecewise (overview files must exist under `corpus/data/bench/overviews/`
— `run_all.sh` step 1 creates them):

```bash
cd benchmarks/overview
python3 make_viewports.py

# storage
python3 bench_storage.py

# access: start the logging server, then run the benchmark
python3 logging_server.py ../../corpus/data 8899 &
uv run --with pmtiles python3 bench_access.py
python3 format_access.py            # -> the RESULTS.md markdown

# conversion cost
./run_conversion.sh
```

### Remote (S3) leg

Needs the artifacts uploaded once (any bucket; names under
`overviews/` and `pmtiles/` must match the local layout) and AWS
credentials for the profile:

```bash
aws s3 cp ../../corpus/data/bench/overviews/ \
  s3://$BUCKET/overviews/ --recursive \
  --exclude "*" --include "*.dup.parquet" \
  --include "*.dup.report.json"
aws s3 cp ../../corpus/data/goldens/tippecanoe/ \
  s3://$BUCKET/pmtiles/ --recursive \
  --exclude "*" --include "<the four datasets>.pmtiles"

BENCH_BUCKET=$BUCKET BENCH_REGION=us-east-2 \
BENCH_AWS_PROFILE=<profile> \
  uv run --with pmtiles --with requests \
  python3 bench_access_remote.py
python3 format_remote.py   # -> RESULTS.md §2b tables + chart svg
```

## Fairness rules (enforced by the harness)

- Same viewport rectangle + zoom for the overview and PMTiles paths
  (`viewports.json`, echoed in RESULTS.md).
- Cold cache per run: a fresh DuckDB process / fresh pmtiles reader each run;
  DuckDB http metadata cache disabled. N=3 runs, median wall time; bytes and
  requests are deterministic.
- The overview query materializes all columns (`SELECT *`) — the realistic
  client fetch, not a projected count.
- tippecanoe uses the exact recorded golden flags verbatim.

See the **Caveats** section of `RESULTS.md` — the access comparison is
deliberately *not* apples-to-apples (overview returns exact geometry + all
attributes; MVT is lossy and property-pruned) and the honest reading of the
numbers is spelled out there.
