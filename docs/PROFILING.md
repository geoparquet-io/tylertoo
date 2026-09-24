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

Set `TYLERTOO_PROFILE_JSON=<path>` and every `overview`/`tiles` conversion
appends one JSON object (one line, JSONL) to that file — the measurement
base the perf series (pass-1 parallelization, pass-2 throughput, checkpoint
work) is gated on:

```bash
TYLERTOO_PROFILE_JSON=/tmp/profile.jsonl \
  tylertoo tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 10
cat /tmp/profile.jsonl | python3 -m json.tool
```

It is an env var rather than a CLI flag so this diagnostics-only knob costs
no CLI-doc churn, and it is best-effort: an unset/blank var is a no-op, and
a write failure only logs — it never fails the conversion (see
**Fail-fast preflight** below).

### Schema

```jsonc
{
  "timestamp": 1790248637.27,       // UNIX epoch seconds
  "phase_walls": {                  // wall-clock time per top-level phase
    "pass1": 0.0297,
    "pass2": 0.0228,
    "writer_finish": 0.0005,
    "total": 0.0310
  },
  "pass1": {
    "rows": 1000,                   // input rows streamed
    "rows_per_sec": 33682.7,
    "stage_secs": {                 // pass-1 stage breakdown, in seconds
      "read": 0.00199,
      "decode": 0.00265,
      "scan": 0.00022,
      "keys": 0.00005,
      "assemble": 0.00006
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

`pass1.stage_secs` and `pass2.stage_secs` are independent stage splits —
they do not need to sum to their phase's `rows_per_sec` denominator or to
`phase_walls`, since stages overlap across a producer/consumer pipeline
(read/decode/simplify/build run on a reader thread while the writer thread
drains finished batches). A stage split that sums to (near) zero while
`rows` and `phase_walls` are non-zero is a bug, not a fast run — that
inconsistency is exactly what shipped as #517 (the finest level's own
stage timers were dropped) and is now covered by
`crates/cli/tests/profile_json_dump.rs::profile_json_written_and_parses`.

### Fail-fast preflight

An unwritable path (typo, missing directory, read-only mount) is validated
**at conversion start**, not only when the dump is finally appended at the
very end of the run: a bad path logs an unmistakable `log::warn` (`NOT
WRITABLE`) immediately, so a multi-hour batch/sweep run doesn't silently
lose its profiling data while still exiting `0`. This preflight — like the
dump itself — never fails the conversion; it only makes the failure mode
loud and early instead of quiet and late (#517).

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
