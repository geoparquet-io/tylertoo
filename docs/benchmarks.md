# Benchmarks

tylertoo against [tippecanoe](https://github.com/felt/tippecanoe), end to
end: GeoParquet in, z0–z14 PMTiles out. Every number on this page is
generated from a committed results file by `scripts/gen_bench_docs.py`, and
CI fails if a number here differs from its record. Each table ends with a
run line that names the commit, tool versions, host, and date.

## Hardware, versions, and pinning

- **Host.** The corpus and Overture runs used a 64-core arm64 (Neoverse-V1)
  host with 124 GB of RAM and no other jobs running.
- **tippecanoe** is v2.79.0, built from source at a verified pinned commit
  by `benchmarks/e2e/setup_tippecanoe.sh`.
- **tylertoo** is a release build of the commit in each run line.
- **Inputs are pinned.** The Overture corpus extracts are pinned to release
  `2026-10-21.0`, with each extract's ids hashed in `corpus/manifest.json`.
  The Moldova parcels file is a stable direct download. Each Overture
  buildings part in the at-scale run is pinned by ETag.

## Against tippecanoe on the corpus

Both tools run a production configuration, not a rigged one. tippecanoe
runs its recorded flags for each dataset (`--drop-densest-as-needed`,
`--simplification=10`, …); tylertoo runs its defaults. The "thinning off"
column re-runs tylertoo with `--verbatim --simplify-factor 1.0`, so it
cannot win by emitting fewer features. Each time is the median of 3 warm
runs per tool, alternating the tools within each round. Both speedup
columns are end to end.

<!-- bench:e2e-corpus/tippecanoe-table -->
| dataset (class) | features | tylertoo | tippecanoe e2e | speedup | speedup, thinning off | peak RSS (tylertoo / tippecanoe) |
|---|---|---|---|---|---|---|
| NYC places (point) † | 630,065 | 1.57 s | 42.66 s | **27.2×** | 2.3×* | 2,210 / 1,352 MB |
| Portland buildings (polygon) | 813,456 | 3.68 s | 84.81 s | **23.0×** | 13.7× | 3,132 / 2,127 MB |
| Oregon roads (line) | 1,203,997 | 10.65 s | 180.69 s | **17.0×** | 17.3× | 9,051 / 2,435 MB |
| Moldova parcels (polygon, dense) | 631,910 | 7.78 s | 167.27 s | **21.5×** | 17.9× | 4,520 / 2,431 MB |

_Run: commit ae0363f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio geoparquet-io, version 1.7.0, Neoverse-V1, 124 GB RAM, Linux-7.0.0-1014-aws-aarch64-with-glibc2.39, 2026-11-06._
<!-- /bench -->

The honest footnotes, which are the point of this table:

- † *The point row is not a parity measurement at all.* tippecanoe's recorded
  flags (`--extend-zooms-if-still-dropping`) had it building z15–z16 — 9,694
  of its 10,543 tiles and 69% of its output bytes — while tylertoo stopped at
  the requested z14. Both of that row's ratios include that extra work.
- *The point row's "thinning off" 2.3× is the weakest number this harness has
  produced, and it carries a second mismatch on top of †:* `--verbatim` makes
  tylertoo carry all 630k points at every zoom (1.57 s → 18.38 s). The two
  tools also thin points by different rules, so they keep different points
  at each zoom ([#666](https://github.com/geoparquet-io/tylertoo/issues/666)
  measures it on this dataset). Read the line and polygon rows for parity
  numbers.
- Archives are equal on Moldova (1.00×) and larger elsewhere: Oregon roads
  1.12× at defaults (1.23× thinning off), Portland buildings 1.41× (1.47×).
  For points, the 0.65× compares tylertoo's z0–14 archive against
  tippecanoe's z0–16; at z0–14 on both sides tylertoo's is 2.1× bigger.
- Converting GeoParquet for tippecanoe is only 2–4% of its end-to-end wall at
  this scale — the gap is tiler vs tiler, not a format-conversion tax.
- On a small contiguous admin coverage (the 17k-polygon fixture set, an
  older 12-core laptop run), the parity-matched ratio was **≈7.5×**, well
  below the corpus rows.

## Memory

tylertoo's peak memory is **higher on all four datasets** — up to <!-- bench:e2e-corpus/max-memory-ratio -->3.7×<!-- /bench -->
on lines. The speed is parallelism (19–29 of 64 cores busy vs tippecanoe's
2–3) plus the native GeoParquet read, paid for in working set. Turning the
thinning off costs memory as well as time. The export's concurrency,
`--partition-wave auto`, sizes itself against available RAM, so peak memory
moves with the machine; pin `--partition-wave N` before you compare memory
across hosts.

## At scale: 21.6 million buildings

The Netherlands bbox of Overture buildings release `2026-10-21.0`, every
part pinned by ETag. tylertoo reads the GeoParquet parts straight from
`s3://` with `--bbox`, fetching only the row groups the bbox touches.
tippecanoe can't read GeoParquet or S3, so its time includes extracting the
same bbox to local GeoParquet with gpio and converting it to FlatGeobuf.
The `s3://` row is a single run (the local rows are medians of 3). Its
archive differs from the local extract's by 3 KB of 912 MB; that
difference has not been traced.

<!-- bench:overture-buildings-netherlands/vida-head-to-head -->
Overture buildings 2026-10-21.0, Netherlands (bbox 3.31,50.75,7.23,53.56): 21,557,082 features, 1.9 GB.

| tool | end-to-end wall | peak RSS | effective cores | archive | note |
|---|---|---|---|---|---|
| tylertoo, s3:// in place | 107 s | 15.4 GB | 16.0 | 0.9 GB | reads the parts in place; no extract |
| tylertoo, local extract | 321 s | 13.3 GB | 22.7 | 0.9 GB | end to end includes the gpio extract |
| tippecanoe v2.79.0 | 1,417 s | 6.1 GB | 3.1 | 0.8 GB | extract + FlatGeobuf + tile |

_Run: commit 875b1bd, tylertoo 0.8.0, tippecanoe v2.79.0, gpio 1.7.0, aarch64 0xd40 x64, 2026-11-06._
<!-- /bench -->

## What the numbers don't show

The corpus table times one local file per dataset, and only the at-scale
`s3://` row shows a remote read. Neither table measures what tylertoo does
that tippecanoe can't do at all:

- reading GeoParquet with no conversion step;
- streaming `s3://`, `gs://`, or `https://` data in place, by byte range;
- reading a directory, glob, prefix, or Hive layout as one dataset;
- skipping row groups with `--bbox` and `--filter`;
- sharding one build across machines and merging without re-encoding.

See [what only tylertoo does](guides/tippecanoe.md#what-only-tylertoo-does).

## Methods and caveats

- **Warm cache.** Each tool gets one discarded warm-up run, then 3 timed
  runs; the page cache is warm for both tools. Cold-cache reads are not
  measured.
- **Quiet host, checked by hand.** No other job ran during the corpus
  recording, but the corpus harness has no load gate and records no load
  figure. Spread across the 3 repeats is at most 5.3% on every measurement
  except two tylertoo runs: Oregon roads at defaults (16.8%) and Moldova
  with thinning off (13.7%).
- **The fastest converter.** tippecanoe's input conversion uses whichever
  of `gpio` and `ogr2ogr` is faster, so the baseline never pays for a tool
  choice. On the fixture set, FlatGeobuf was tippecanoe's fastest input:
  line-delimited GeoJSON was slower to write and slower to read.
- **Different thinning models.** Exact tile parity is neither expected nor
  claimed. The per-zoom tile counts in the full record agree within a few
  tiles per zoom on lines and polygons.
- **Not swept.** Both tools get every core; neither is pinned to fewer
  cores or swept across core counts.

The full record, with per-zoom tile counts, the input-conversion cost, both
tools' phase split, run-to-run variance, and the asymmetry list, is
[`benchmarks/e2e/RESULTS.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/RESULTS.md).
The method and the settings-parity mapping are in
[`benchmarks/e2e/README.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/e2e/README.md).
For a sharded build across machines, see [scaling](guides/scaling.md).

## Reproduce

- **The corpus table:** `./benchmarks/e2e/run_corpus.sh`, which writes
  [`benchmarks/results/e2e-corpus.json`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/results/e2e-corpus.json).
- **The at-scale run:** `benchmarks/showcase/showcase.py`; its
  [README](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/showcase/README.md)
  walks through picking a region, extracting it, and collecting the record.
- **One build against another:** `benchmarks/run_manifest.py` times the
  workloads in `benchmarks/manifest.toml` for two binaries, interleaved,
  with a load gate, and records each output's sha256 so you can confirm
  both builds wrote the same bytes.
- **The tables on this page:** copy a new record into `benchmarks/results/`,
  then run `uv run python scripts/gen_bench_docs.py`. `--check` exits 1 if
  any generated region is stale.
