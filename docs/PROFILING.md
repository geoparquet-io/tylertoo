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
run through the **streaming pipeline** (the default) appends one JSON object
(one line, JSONL) to that file — the measurement base the perf series
(pass-1 parallelization, pass-2 throughput, checkpoint work) is gated on:

```bash
TYLERTOO_PROFILE_JSON=/tmp/profile.jsonl \
  tylertoo tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 10
cat /tmp/profile.jsonl | python3 -m json.tool
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
