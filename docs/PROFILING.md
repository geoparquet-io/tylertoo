# Profiling tylertoo

How to measure where time and memory go. The recorded performance
history of the pipeline (what was measured, what was fixed) lives in
[`benchmarks/overview/PROFILE.md`](https://github.com/geoparquet-io/tylertoo/blob/main/benchmarks/overview/PROFILE.md).

## Phase Timing (built in)

The overview pipeline keeps phase accumulators (pass 1 scan, assignment,
per-level read/decode, simplification, write; export clip/encode/write)
behind the `log` debug level:

```bash
RUST_LOG=tylertoo_core::overview=debug \
  tylertoo overview input.parquet output.parquet

RUST_LOG=tylertoo_core::overview=debug \
  tylertoo export-pmtiles output.parquet tiles.pmtiles
```

For end-to-end wall time and peak RSS, wrap the release binary in GNU
time:

```bash
cargo build --release
/usr/bin/time -v ./target/release/tylertoo overview \
  input.parquet output.parquet --min-zoom 0 --max-zoom 14
```

`Maximum resident set size` is the peak-RSS number quoted throughout
the benchmark docs.

## Structured JSON Profile Dump (`TYLERTOO_PROFILE_JSON`)

Set `TYLERTOO_PROFILE_JSON=<path>` and each successful run appends one JSON
object per phase (one line each, JSONL) to that file: a convert line from
`overview` run through the **streaming pipeline** (the default), and an
export line from `export-pmtiles`. A one-shot `tiles` run does both, so it
appends **two** lines — convert's, then export's — paired by a shared
`run_id` (see "Two JSONL lines for one `tiles` run" below). This is the
measurement base the perf series (pass-1 parallelization, pass-2 throughput,
checkpoint work and, since #535, export throughput) is gated on:

```bash
TYLERTOO_PROFILE_JSON=/tmp/profile.jsonl \
  tylertoo tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 10
# JSONL: one object per line, so pretty-print line by line (or `jq -s .`
# to load the whole file as an array of the two objects `tiles` writes).
while read -r line; do echo "$line" | python3 -m json.tool; done < /tmp/profile.jsonl
```

It is an env var rather than a CLI flag so this diagnostics-only knob costs
no CLI-doc churn, and it is best-effort: an unset/blank var is a no-op, and
a write failure only logs — it never fails the conversion (see
**Early warning** below).

`--no-streaming` (the in-memory reference path) writes **no dump at all**:
it has none of the per-stage timers or phase walls the schema below is made
of. Setting the var with `--no-streaming` logs a warning saying so and
leaves the file untouched — so a missing line there means "wrong pipeline",
not "the run was not profiled".

### Schema

```jsonc
{
  "timestamp": 1790248637.27,       // UNIX epoch seconds
  "run_id": "48213-1790248630112345678", // "<pid>-<unix nanos>", one per process
  "phase": "convert",
  "phase_walls": {                  // DISJOINT wall-clock windows, run order
    "pass1": 0.0068,                // the scan alone (stops at scan end)
    "assign": 0.0021,               // resolve_winner_tables: winners, the
                                    // density budget, carriers, clusters
    "pass2": 0.0228,                // the level build (stops before finish)
    "writer_finish": 0.0005,        // writer.finish() alone
    "total": 0.0330                 // the whole conversion
  },
  "pass1": {
    "rows": 1000,                   // input rows streamed
    "rows_per_sec": 33682.7,
    "stage_secs": {                 // pass-1 stage breakdown, in seconds —
      "read": 0.00199,              // summed core-seconds; may exceed the
      "decode": 0.00265,            // phase wall (phase_walls.pass1) under
      "scan": 0.00022,              // parallelism (true once #504's parallel
      "keys": 0.00005,              // pass-1 scan lands; until then pass 1
      "assemble": 0.00006           // is serial and they agree)
    }
  },
  "pass2": {
    "rows": 1000,                   // OUTPUT rows written across every level
    "rows_per_sec": 43789.5,
    "stage_secs": {                 // pass-2 stage breakdown, in seconds —
      "read": 0.00237,              // folded across every level, including
      "decode": 0.00251,            // the finest (#517): a fixed set of
      "simplify": 0.00003,          // stages run in parallel across levels
      "build": 0.00177,             // under the pipelined engine, so these
      "drain": 0.0,                 // are core-seconds that can exceed
      "spill_write": 0.0            // phase_walls.pass2's wall time
    },
    "identical_steps": {            // cascade-fold steps that reused the
      "shared": 1632,               // previous level's geometry (#499);
      "total": 26909,               // 0/0 when the fold never ran: a Serial
                                    // run, or a single-level ladder
      "ratio": 0.0606
    }
  },
  "levels": [                       // per WRITTEN level, writer order
    { "rows": 1000, "spill_bytes": 0 }
  ],
  "peak_rss_mib": 29.28,            // null if RSS sampling is unavailable
  "threads": 12,                    // rayon::current_num_threads()
  "in_flight": 12,                  // read batches in flight (pass 2)
  "memory_profile": "auto"          // the resolved MemoryProfile
}
```

The four `phase_walls` phases are **disjoint** windows of one conversion, so
`pass1 + assign + pass2 + writer_finish <= total` always holds; the
remainder of `total` is the preflight, the writer setup and the closing
report sums. Until #533 each phase carried its *start* instant to the end of
the run and elapsed it there, so `pass1` silently covered the assignment,
all of pass 2 and `writer.finish()` (≈ `total`), the four over-counted
`total` by ~2×, `pass1.rows_per_sec` was wrong by the same factor, and the
assignment — the single largest phase on a planet-scale run — had no entry
at all. `crates/cli/tests/profile_json_dump.rs::profile_json_written_and_parses`
asserts the partition.

> **`pass2.stage_secs.read` changed meaning in #536.** It is now the *summed
> core-seconds of the reader worker threads* (`--read-workers`, #494), not the
> time one consumer thread observed itself waiting for batches. With several
> workers decoding at once the number goes UP while the pass gets faster, so
> comparing a dump from before #536 against one from after will read a
> ~+35% `read` as a regression when it is the opposite. Compare
> `phase_walls.pass2` across such a pair instead, and treat `stage_secs.read`
> as a core-seconds figure only. Two costs sit outside that window on purpose:
> the in-order merge's splice at a worker seam, and opening each segment's
> reader — the latter is now a bare `File::open` over already-parsed footer
> metadata (#536 stopped re-parsing the footer per segment, ~5.5 s of CPU on
> the Brazil fixture), so it is negligible rather than merely uncounted.
> `pass2.stage_secs.spill_write` likewise moved off the consumer thread onto
> each level's spill writer and is folded in when that thread is joined.

`pass1.stage_secs` and `pass2.stage_secs` are independent stage splits —
they do not need to sum to their phase's `rows_per_sec` denominator or to
`phase_walls`, since stages overlap across a producer/consumer pipeline
(read/decode/simplify/build run on a reader thread while the writer thread
drains finished batches). A stage split that sums to (near) zero while
`rows` and `phase_walls` are non-zero is a bug, not a fast run — that
inconsistency is exactly what shipped as #517 (the finest level's own
stage timers were dropped) and is now covered by
`crates/cli/tests/profile_json_dump.rs::profile_json_written_and_parses`.

### Early warning

An unwritable path (typo, missing directory, read-only mount) is probed
**at conversion start** — with the other convert path preflights, in
`validate_options` — not only when the dump is finally appended at the very
end of the run: a bad path logs an unmistakable `log::warn` (`NOT WRITABLE`)
immediately, so a multi-hour batch/sweep run doesn't silently lose its
profiling data while still exiting `0`. It is deliberately **not**
fail-fast: like the dump itself it never fails the conversion, it only makes
the failure mode loud and early instead of quiet and late (#517). The probe
is read-only as far as the target is concerned — an existing file is opened
for append, a missing one is checked through a uniquely named sibling that
is removed again — so a preflight never leaves a zero-byte dump behind for
a run that wrote no line.

### What the dump does not contain

The `[profile]` debug log carries a few numbers the JSON does not. Most
notably, the **writer-thread busy time** of the finest (streamed) level —
`total - recv_wait` from `write_level_streaming` — is logged per level but
has no field in the dump, whose `pass2.stage_secs` covers producer-side
stages only. Where the log and the dump disagree in shape like this, the log
is the finer-grained view and the JSON is the stable, parseable one; only
the JSON is covered by tests.

### Export phase (#535)

`export::export_pmtiles` (the `export-pmtiles` CLI subcommand, the second
half of `tiles`, and the Python bindings) appends its own line to the same
`TYLERTOO_PROFILE_JSON` file, with the same best-effort contract: an
unset/blank var is a no-op, an open/write error is only logged, and it can
never fail an export or change its output bytes. Only a **successful**
export writes a line. An export that errors out appends nothing, so a
missing export line after a failed run is expected, not a profiling bug.

```jsonc
{
  "timestamp": 1790403985.86,
  "run_id": "48213-1790248630112345678", // same as this process's convert line
  "phase": "export",
  "output": "out.pmtiles",         // the archive written
  "layer": "features",             // the MVT layer name
  "export": {
    "mode": "partitioning",        // the overview file's mode, or "duplicating"
    "phase_walls": {               // DISJOINT WALL-clock windows, run order
      "scan": 0.041,               // level scan + range restriction + planning
      "fill": 1.92,                // partitioning single-read fill; 0.0 in
                                   // duplicating mode (see below)
      "levels": 2.87,              // every level's wave loop + checkpoints
      "finalize": 0.08,            // writer.finalize()
      "total": 4.95                // the whole export
    },
    "stage_secs": {                // CORE-SECONDS (see the callout below)
      "band_read": 0.62,
      "decode": 0.91,
      "clip": 1.55,
      "encode": 2.23,
      "spill_write": 0.0,          // partitioning mode only; 0.0 otherwise
      "spill_read": 0.0,           // partitioning mode only; 0.0 otherwise
      "spool_write": 0.0028,
      "checkpoint": 0.0
    },
    "per_zoom": [                  // one entry per exported zoom, coarse -> fine
      { "zoom": 1, "wall_secs": 0.012, "tiles": 1, "features": 40, "bytes": 4372 },
      { "zoom": 10, "wall_secs": 2.41, "tiles": 524, "features": 26839, "bytes": 4049886 }
      // ...
    ],
    "waves_total": 10,             // sum of every level's wave-loop iterations
    "partition_wave_width": 12,    // the resolved partition-wave CEILING
    "checkpoints": 0,              // writer.checkpoint() calls, NOT finalize
    "peak_rss_mib": 812.4,         // sampled peak (see below); null if unavailable
    "threads": 12                  // rayon::current_num_threads()
  }
}
```

**Phase walls.** The four `export.phase_walls` phases are disjoint wall-clock
windows, so `scan + fill + levels + finalize <= total` always holds. The
remainder of `total` is the open/validation/writer setup before the scan.
`fill` is the partitioning-mode single-read fill (`fill_member_store`, #235),
which reads, decodes and clips every band up front. In duplicating mode
there is no fill (`fill` is exactly 0.0) and the same work happens per wave
inside `levels`. Use `export.mode` to tell which case you are looking at.

**Stages** (`crates/core/src/overview/export.rs`). Each timer's `Instant`
window is taken inside the innermost per-item closure (one geometry, one
tile), never around a whole parallel section:

- `band_read`: reading overview rows. It covers opening the Parquet
  reader (footer/page-index setup, once per band in partitioning mode and
  once per wave in duplicating mode) plus every
  `ParquetRecordBatchReader::next()` call (read + Arrow decode). It is
  charged on the reading thread: the wave's own thread in `process_wave`
  (duplicating), or the fill's producer thread in `fill_member_store`
  (partitioning). The producer's blocking `tx.send` is excluded.
- `decode`: everything per batch around the clip that is not the clip. That
  means the geoarrow → `geo::Geometry` decode, the EPSG:3857 → 4326
  reprojection (timed per geometry), property extraction, and per-row member
  materialization and routing into partition buckets or the `MemberStore`.
  A spill flush triggered while routing is subtracted out and charged to
  `spill_write`.
- `clip`: `feature_tile_members` (the recursive quadtree clip cascade),
  timed per geometry inside its `par_iter`.
- `encode`: per-tile MVT encode (including the oversized-tile valve),
  content hash and gzip, timed per tile inside `encode_members`. The
  per-tile member sort before it is not counted.
- `spill_write`: partitioning mode only. Serializing and writing
  `MemberStore` spill segments during the fill and its final flush.
- `spill_read`: partitioning mode only. `MemberStore::take_wave`, which
  reads and decodes a wave's spilled segments back (~0 under RAM backing).
- `spool_write`: the serial `writer.add_tile_precompressed` calls in
  `export_level`'s per-wave write loop.
- `checkpoint`: `writer.checkpoint(...)` calls (the throttled #229/#459
  salvage snapshots). The final `writer.finalize(...)` is not included; it
  is `phase_walls.finalize`. This stage is **legitimately 0.0** on any export
  that finishes before `CHECKPOINT_INTERVAL` elapses, and `checkpoints` (the
  call count) says the same thing as an integer.

> **`export.stage_secs` is CORE-SECONDS, the same convention as
> `pass2.stage_secs`.** Each stage is the sum of per-item windows across all
> threads, so `decode`, `clip` and `encode` routinely EXCEED the wall time
> they ran in under parallelism. That is what the parallelism buys, not a
> bug. Because windows are per item, nested `par_iter`s and work stealing
> never double-count. Time outside every window (the member sort, channel
> waits, rayon scheduling) is uncounted rather than mis-attributed, so the
> stages are not expected to sum to any `phase_walls` entry.
>
> **Where stages overlap in wall time:** in the partitioning-mode fill, the
> producer thread's `band_read` runs concurrently with the consumer's
> `decode`/`clip`. The consumer's time parked in `rx.iter()` waiting for the
> next batch is uncounted. So in that mode `band_read + decode + clip` can
> exceed `phase_walls.fill` even before parallelism is counted, and a small
> `band_read` next to a large `fill` means the fill was CPU-bound, not
> I/O-bound. In duplicating mode, each wave's read and its decode/clip run
> one after the other, not concurrently.

`export.per_zoom[].wall_secs` is the wall time of that zoom's wave loop in
`export_level`. It **excludes the partitioning-mode fill** (which does the
band reads, decode and clip for every level up front, in
`phase_walls.fill`) and the checkpoint after the level. In duplicating mode
it includes the per-wave reads and clip.

`export.per_zoom[].bytes` is the **compressed** (gzipped) tile bytes the zoom
produced, summed over every tile handed to the writer and counted **before
the writer's content-hash dedup**. A duplicated tile (e.g. an all-ocean tile)
counts every time it is produced but is stored once, so the per-zoom sums
can exceed the archive's size. It is not the raw pre-gzip MVT size.

`export.peak_rss_mib` is the peak of RSS samples taken at the export's phase
boundaries (after the scan, the fill, each level, and finalize). It is a
sampled peak, not a true high-water mark, and is `null` where the platform
cannot report RSS.

`export.partition_wave_width` is the resolved partition-wave **ceiling**
(`resolve_and_log_partition_wave`'s return, `auto` or an explicit
`--partition-wave`), a single scalar for the whole export.
`memory_safe_level_wave` (#311) can narrow a level's actual wave below it
based on that level's density. The dump does not carry that per-level detail
yet.

`crates/cli/tests/profile_json_dump.rs` covers this section:

- `profile_json_written_and_parses` runs `tiles` (duplicating mode). It
  asserts that both lines share one `run_id` and have the right `phase`, and
  that `per_zoom` matches `--report`'s zooms, tile totals and feature totals.
  It also checks that `band_read`, `decode`, `clip`, `encode` and
  `spool_write` are each positive and that the phase walls fit inside
  `total`.
- `profile_json_export_line_partitioning_mode` runs `overview --mode
  partitioning` followed by `export-pmtiles` as separate processes. It
  asserts distinct `run_id`s, `mode: "partitioning"`, a positive
  `phase_walls.fill`, and positive stages.

#### Two JSONL lines for one `tiles` run

A one-shot `tiles` conversion appends **two** JSON lines to
`TYLERTOO_PROFILE_JSON`: convert's (`"phase": "convert"`, the object
documented above) followed by export's (`"phase": "export"`). Both carry the
same `run_id` (`"<pid>-<unix nanos>"`, fixed once per process), which is how
a consumer pairs them. Group lines by `run_id` and split them by `phase`.
Separate processes get separate `run_id`s, so lines from concurrent runs
appending to one file can still be told apart when their timestamps
interleave. Each line is written with a single append-mode `write_all`, so
concurrent writers do not tear each other's lines.

Two lines are a deliberate choice. Convert's line is written the moment
`convert_streaming_strategy` returns, before `tiles` even calls
`export_pmtiles`, because convert has no idea export is coming next.
Merging the two into one object would mean either:

- (a) delaying convert's write until export finishes, which means threading
  export's completion back into `convert_to_overviews`. That is a layering
  violation.
- (b) having the `tiles` CLI facade hold both halves and write once. That
  moves the write out of the library and into the CLI, which is backwards
  for a library-first design, since `export-pmtiles` and the Python bindings
  call `export_pmtiles` directly.

A standalone `export-pmtiles` run writes exactly one `"phase": "export"`
line, the same shape as the second line of a `tiles` run.

## Wall-Time Profiling with cargo-flamegraph

```bash
cargo install flamegraph

# Profile a conversion (requires perf; may need
# kernel.perf_event_paranoid <= 1)
cargo flamegraph --release --package tylertoo -- \
  overview input.parquet output.parquet
```

Expect simplification (RDP + ring validation) to dominate convert on
polygon-heavy data, and clipping/encoding to dominate export — both are
rayon-parallel, so look at per-thread flame widths.

## Memory Profiling with dhat

Heap profiling is feature-gated (zero overhead in normal builds):

```bash
# Build with heap profiling enabled
cargo build --release --features dhat-heap

# Run your workload; dhat-heap.json is written on exit
./target/release/tylertoo overview input.parquet output.parquet
ls dhat-heap.json
```

(CI's "Profiling Features" job keeps this build working.)

### Analyzing Results

1. Open <https://nnethercote.github.io/dh_view/dh_view.html>
2. Load `dhat-heap.json`

Key metrics:

- **Total bytes** — total heap allocation across the run
- **Peak bytes** — high-water mark (compare against `time -v` RSS)
- **At end bytes** — still allocated at exit (potential leaks)
- **Allocation sites** — sorted by total bytes; expand call stacks

Compare before/after a change:

```bash
mv dhat-heap.json dhat-heap-before.json
./target/release/tylertoo overview input.parquet output.parquet
# diff dhat-heap-before.json vs dhat-heap.json in the viewer
```

### Limitations

- **Release builds only** — debug builds are too slow to be meaningful
- **~2–5% runtime overhead** while profiling
- **Feature-gated** — rebuild with `--features dhat-heap`

## Criterion Benchmarks

Micro-benchmarks for the clipping hot path live in
`crates/core/benches/` (`clipping`, `bbox_containment`):

```bash
cargo bench --package tylertoo-core --bench clipping
open target/criterion/report/index.html
```

## Reproducing the Published Numbers

The corpus-based storage/access/conversion benchmarks are scripted in
`benchmarks/overview/` (see its README for the run order); the corpus
itself is rebuilt from `corpus/fetch.sh` + `corpus/optimize.sh`.
