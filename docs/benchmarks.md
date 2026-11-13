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
- **Disk.** A network disk (AWS gp3). The corpus, Brazil, and memory-profile
  runs used 125 MB/s, much slower than a laptop's NVMe drive; the Africa run
  started during the upgrade to 1,000 MB/s. Spilling runs are sensitive to
  it.
- **Inputs are pinned.** The Overture corpus extracts are pinned to release
  `2026-10-21.0`, with each extract's ids hashed in `corpus/manifest.json`.
  The Moldova parcels file is a stable direct download. Each Overture
  buildings part in the at-scale run is pinned by ETag.

## Streaming at scale: Africa, straight from S3

One `tylertoo tiles` command reads Overture's buildings release
`2026-10-21.0` in place on `s3://`, as the partitioned GeoParquet files
Overture publishes, and writes one PMTiles archive. `--bbox` selects Africa;
tylertoo reads only the row groups whose bbox statistics intersect it, and
only the columns it tiles. Nothing is downloaded first, and no format
conversion runs. tippecanoe can't do this at all: it reads neither
GeoParquet nor S3, so this run has no tippecanoe row. A single run.

<!-- bench:overture-buildings-africa/stream-at-scale -->
Overture buildings 2026-10-21.0, Africa (bbox `-18,-35,52,37.5`): 526,532,679 features kept by `--bbox`. The bbox touches 118 of the release's partitioned GeoParquet files and 17,372 of its 84,224 row groups; those row groups hold 58.7 GB of the release's 276.9 GB.

| run | wall | peak RSS | effective cores | fetched | peak local disk | archive |
|---|---|---|---|---|---|---|
| tylertoo, s3:// in place | 54 min (3,243 s) | 107.1 GB | 11.9 | 35.6 GB | 145.8 GB | 12.5 GB |
| tylertoo, s3:// in place, `--profile bounded` | 50 min (3,018 s) | 41.1 GB | 13.6 | 35.6 GB | 194.9 GB | 12.5 GB |

_Run: commit afb6b15, tylertoo 0.8.0, aarch64, CPU 0xd40, 64 CPUs, 2026-11-07._
<!-- /bench -->

- **Both profiles write the same archive,** byte for byte. `--profile
  bounded` peaks at 41 GB, against the default's 107 GB on this 123 GB host,
  so bounded is the one to use on a smaller machine.
- **Disk:** the default run overlapped an upgrade of the host's disk from
  125 to 1,000 MB/s; the bounded run had the fast disk throughout, which is
  part of why it is faster here. On Brazil, on the same fast disk, bounded
  is about a fifth slower than the default.
- **Cores:** about 12–14 of 64 are busy on average. This run predates
  the core-use work in #818, #878 and #881.
- The bbox is a rectangle, so it also takes in the Middle East west of 52°E
  and southern Europe below 37.5°N.

## Against tippecanoe at scale: Brazil

The Brazil bbox of the same release, 148 million buildings. tylertoo runs
three ways: straight from `s3://` (median of 3), on a local GeoParquet
extract of the bbox, and on that extract with `--profile bounded`.
tippecanoe can't read GeoParquet or S3, so its time includes extracting the
same bbox with gpio and converting the extract to FlatGeobuf; the extract
alone took 10.5 minutes and every row but the `s3://` one pays it.

<!-- bench:overture-buildings-brazil/vida-head-to-head -->
Overture buildings 2026-10-21.0, Brazil (bbox `-74,-33.8,-34.7,5.3`): 148,088,547 features, 10.3 GB.

| tool | end-to-end wall | peak RSS | effective cores | archive | note |
|---|---|---|---|---|---|
| tylertoo, s3:// in place | 855 s | 50.3 GB | 13.6 | 4.2 GB | reads the parts in place; no extract |
| tylertoo, local extract | 1,356 s | 48.3 GB | 15.8 | 4.2 GB | end to end includes the gpio extract |
| tylertoo, local extract, `--profile bounded` | 1,341 s | 12.3 GB | 18.9 | 4.2 GB | end to end includes the gpio extract |
| tippecanoe v2.79.0 | 5,905 s | 23.6 GB | 4.1 | 2.2 GB | extract + FlatGeobuf + tile |

_Run: commit 395a55f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio 1.7.0, aarch64, CPU 0xd40, 64 CPUs, 2026-11-07._
<!-- /bench -->

- **On this disk, `--profile bounded` is not slower,** and it peaks at a
  quarter of the default's memory, below tippecanoe's. At this size the default profile
  spills its intermediate overview to disk anyway (35 GB), so the memory it
  holds buys no speed. On inputs that fit in memory, bounded is slower; see
  [memory profiles](#memory-profiles).
- **Disk speed shows at this size.** These runs used a 125 MB/s network
  disk, far slower than a laptop's NVMe drive. Convert's second pass is
  bound by writing that spill: 275 s of the local run's 725 s. The same
  tylertoo runs after the disk was upgraded to 1,000 MB/s (tippecanoe was
  not re-run):

<!-- bench:overture-buildings-brazil/fast-disk -->
| tylertoo run | wall | peak RSS | effective cores |
|---|---|---|---|
| s3:// in place | 653 s | 50.2 GB | 17.6 |
| s3:// in place, `--profile bounded` | 793 s | 15.1 GB | 17.1 |
| local extract | 530 s | 48.1 GB | 21.2 |
| local extract, `--profile bounded` | 665 s | 12.3 GB | 20.2 |

_Run: commit 395a55f, tylertoo 0.8.0, tippecanoe v2.79.0, gpio 1.7.0, aarch64, CPU 0xd40, 64 CPUs, 2026-11-07._
<!-- /bench -->

  On the faster disk `bounded` costs about a fifth more wall time than the
  default, in a quarter to a third of its memory.
- **tylertoo's archive is 1.9× tippecanoe's because it keeps more
  buildings.** tippecanoe's `--drop-densest-as-needed` thins a whole zoom
  at once: at z13 it kept 27.9 million features to tylertoo's 152 million,
  at z12 10 million to 100 million, and at z14 it dropped 40.8 million
  buildings, about a quarter, from its densest tiles. tylertoo cuts only
  the tiles over the size limit, by merging features, and drops none. z12
  and z13 account for three quarters of the 2 GB gap. Per feature the two
  are close: the same three properties, extent and gzip level. tylertoo
  also does not simplify z14, where tippecanoe's `-S 10` simplifies every
  zoom.
- **The `s3://` and local archives differ by a few KB** of 4.2 GB. tylertoo's
  `--bbox` and gpio's extract keep slightly different features on the bbox
  edge (82 of 434 million overview rows); each path on its own is
  byte-identical across runs.

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

## Memory profiles

`--profile auto`, the default, holds what fits in memory; `--profile
bounded` caps every buffer and spills the rest. Three alternating rounds
per input, same commit; the archives are byte-identical:

<!-- bench:memory-profiles/memory-profiles -->
| input | `auto` wall | `bounded` wall | bounded costs | `auto` peak | `bounded` peak | same archive |
|---|---|---|---|---|---|---|
| Madagascar admin level 4, z0–12 | 1.15 s | 1.27 s | +10% | 1.4 GiB | 1.2 GiB | yes |
| Moldova parcels, z0–14 | 4.33 s | 5.63 s | +30% | 3.7 GiB | 2.7 GiB | yes |
| NZ parcels, z0–14 | 58.24 s | 79.03 s | +36% | 9.8 GiB | 4.7 GiB | yes |

_Run: commit afb6b15, tylertoo 0.8.0, aarch64, CPU 0xd40, 64 CPUs, 2026-11-07._
<!-- /bench -->

On inputs that fit in memory, bounded costs <!-- bench:memory-profiles/bounded-cost -->10–36%<!-- /bench -->
of the wall for its lower peak. On Brazil, where the default spills anyway,
the two tie on the slow disk; on the fast one bounded is about a fifth
slower, in a quarter to a third of the memory. So `auto` stays the
default, and `bounded` is the choice for inputs larger than the machine's
memory or for jobs with a hard memory cap.

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
  with a load gate. It records each output's sha256: equal hashes mean both
  builds wrote the same bytes.
- **The tables on this page:** copy a new record into `benchmarks/results/`,
  then run `uv run python scripts/gen_bench_docs.py`. `--check` exits 1 if
  any generated region is stale.
