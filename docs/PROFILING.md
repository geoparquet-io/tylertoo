# Profiling gpq-tiles

This document describes how to profile gpq-tiles for performance analysis.

## Memory Profiling with dhat

gpq-tiles supports heap profiling using [dhat](https://docs.rs/dhat/), a heap profiling
library for Rust. This helps identify:

- Total bytes allocated during a run
- Peak heap usage
- Allocation sites ranked by bytes
- Call stacks for the largest allocators

### Building with Memory Profiling

Memory profiling is feature-gated to avoid any runtime overhead in normal builds:

```bash
# Build release binary with heap profiling enabled
cargo build --release --features dhat-heap

# Or run directly
cargo run --release --features dhat-heap -- input.parquet output.pmtiles
```

### Running a Profile

When built with `dhat-heap`, the binary automatically writes a profile on exit:

```bash
# Run your workload
./target/release/gpq-tiles input.parquet output.pmtiles

# dhat-heap.json is created in the current directory
ls dhat-heap.json
```

### Analyzing Results

Open the dhat web viewer and load your profile:

1. Navigate to: <https://nnethercote.github.io/dh_view/dh_view.html>
2. Click "Load..." and select your `dhat-heap.json` file
3. Explore the allocation breakdown

#### Key Metrics

- **Total bytes**: Total heap allocation across the entire run
- **Peak bytes**: Maximum heap usage at any point (high-water mark)
- **At end bytes**: Memory still allocated at program exit (potential leaks)
- **Allocation sites**: Where allocations occurred, sorted by total bytes

#### Tips for Analysis

1. **Sort by "Total bytes"** to find the biggest allocators
2. **Expand call stacks** to trace allocations back to your code
3. **Look for unexpected allocations** in hot paths (clipping, encoding)
4. **Compare profiles** before/after optimizations

### Example Workflow

```bash
# Profile a typical workload
cargo build --release --features dhat-heap
./target/release/gpq-tiles large-file.parquet output.pmtiles

# View the profile
# Open https://nnethercote.github.io/dh_view/dh_view.html
# Load dhat-heap.json

# After making changes, profile again and compare
mv dhat-heap.json dhat-heap-before.json
./target/release/gpq-tiles large-file.parquet output.pmtiles
# Compare dhat-heap.json with dhat-heap-before.json
```

### Limitations

- **Release builds only**: Debug builds are too slow for meaningful profiling
- **Single-threaded profiling**: dhat captures allocations across all threads but
  call stacks may be harder to interpret in parallel code
- **Overhead**: ~2-5% runtime overhead when profiling is enabled
- **Feature-gated**: Must rebuild with `--features dhat-heap`; normal builds have
  zero overhead

### Related Issues

- [#32](https://github.com/portolan/gpq-tiles/issues/32) - Memory usage investigation
