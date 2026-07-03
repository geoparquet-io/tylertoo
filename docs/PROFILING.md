# Profiling gpq-tiles

How to measure where time and memory go. The recorded performance
history of the pipeline (what was measured, what was fixed) lives in
[`benchmarks/overview/PROFILE.md`](https://github.com/geoparquet-io/gpq-tiles/blob/main/benchmarks/overview/PROFILE.md).

## Phase Timing (built in)

The overview pipeline keeps phase accumulators (pass 1 scan, assignment,
per-level read/decode, simplification, write; export clip/encode/write)
behind the `log` debug level:

```bash
RUST_LOG=gpq_tiles_core::overview=debug \
  gpq-tiles overview input.parquet output.parquet

RUST_LOG=gpq_tiles_core::overview=debug \
  gpq-tiles export-pmtiles output.parquet tiles.pmtiles
```

For end-to-end wall time and peak RSS, wrap the release binary in GNU
time:

```bash
cargo build --release
/usr/bin/time -v ./target/release/gpq-tiles overview \
  input.parquet output.parquet --min-zoom 0 --max-zoom 14
```

`Maximum resident set size` is the peak-RSS number quoted throughout
the benchmark docs.

## Wall-Time Profiling with cargo-flamegraph

```bash
cargo install flamegraph

# Profile a conversion (requires perf; may need
# kernel.perf_event_paranoid <= 1)
cargo flamegraph --release --package gpq-tiles -- \
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
./target/release/gpq-tiles overview input.parquet output.parquet
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
./target/release/gpq-tiles overview input.parquet output.parquet
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
cargo bench --package gpq-tiles-core --bench clipping
open target/criterion/report/index.html
```

## Reproducing the Published Numbers

The corpus-based storage/access/conversion benchmarks are scripted in
`benchmarks/overview/` (see its README for the run order); the corpus
itself is rebuilt from `corpus/fetch.sh` + `corpus/optimize.sh`.
