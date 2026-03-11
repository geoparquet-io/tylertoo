# Profiling gpq-tiles

This guide explains how to profile gpq-tiles to identify performance bottlenecks.

## Time Profiling

gpq-tiles includes built-in time profiling to measure how long each phase of tile generation takes.

### Console Timing Summary

Use `--profile` to get a timing breakdown after conversion completes:

```bash
gpq-tiles input.parquet output.pmtiles --profile
```

Example output:
```
Profiling summary:
  pipeline          12.4s  100%
  ├─ read_parquet    2.8s   23%
  ├─ sort            1.1s    9%
  └─ encode          8.5s   68%
```

The phases are:
- **read_parquet**: Reading GeoParquet file and clipping geometries to tile bounds
- **sort**: External merge sort by tile ID (memory-bounded)
- **encode**: Encoding tiles to MVT format and writing to PMTiles

### Chrome Trace Output

For detailed visualization, use `--trace-output` to generate a Chrome trace file:

```bash
gpq-tiles input.parquet output.pmtiles --trace-output trace.json
```

View the trace in:
- **Chrome**: Navigate to `chrome://tracing` and load the file
- **Perfetto**: Open https://ui.perfetto.dev and load the file

Chrome traces show:
- Hierarchical span timing
- Concurrent execution visualization
- Detailed timing for each phase

### Combined Profiling

Use both flags together for console summary and trace file:

```bash
gpq-tiles input.parquet output.pmtiles --profile --trace-output trace.json
```

## Understanding the Pipeline Phases

### Phase 1: Read GeoParquet

This phase reads the input file, extracts geometries, and clips them to tile bounds:
- Row-by-row streaming to minimize memory usage
- Parallel geometry processing within each row group
- Hierarchical clipping (clips parent tile results for child tiles)
- Pre-simplification at max zoom tolerance

**Common bottlenecks:**
- Large files with many row groups
- Complex geometries (high vertex counts)
- Many zoom levels (wider min/max zoom range)

### Phase 2: External Sort

Records are sorted by tile ID using external merge sort:
- Memory-bounded (configurable buffer size)
- Disk-backed for large datasets
- Typically fast unless I/O bound

**Common bottlenecks:**
- Slow disk I/O
- Very large datasets (>100M features)

### Phase 3: Encode

Sorted records are grouped by tile and encoded to MVT:
- Features grouped by tile ID
- MVT encoding with property deduplication
- Compression (gzip, zstd, or brotli)
- Writing to PMTiles archive

**Common bottlenecks:**
- High feature density per tile
- Large property payloads
- Compression overhead (especially brotli)

## Performance Tips

### Optimize Input Data

Use `geoparquet-io` (gpio) to optimize your GeoParquet files:

```bash
# Hilbert-sort and set optimal row group size
gpio optimize input.parquet output.parquet --sort hilbert --row-group-size 100000
```

Optimal GeoParquet files have:
- Hilbert spatial sorting for locality
- 50K-200K rows per row group
- WGS84 (EPSG:4326) coordinates

### Tune Zoom Range

Reduce the zoom range to speed up processing:

```bash
# Only generate zooms 0-10 instead of 0-14
gpq-tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 10
```

Each additional zoom level approximately quadruples the number of tiles.

### Use Faster Compression

Zstd is faster than gzip for encoding:

```bash
gpq-tiles input.parquet output.pmtiles --compression zstd
```

Note: Some PMTiles viewers only support gzip compression.

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

## Architecture

### Tracing Implementation

gpq-tiles uses the `tracing` crate for profiling:
- Zero-cost when no subscriber is active
- Spans always compiled in
- Runtime opt-in via CLI flags

Key spans:
| Span Name | Level | Description |
|-----------|-------|-------------|
| `pipeline` | INFO | Overall tile generation |
| `read_parquet` | INFO | Phase 1: reading and clipping |
| `row_group` | INFO | Per-row-group processing |
| `sort` | INFO | Phase 2: external merge sort |
| `encode` | INFO | Phase 3: MVT encoding |

### Extending Profiling

To add custom spans in your application using gpq-tiles-core as a library:

```rust
use tracing::{info_span, instrument};

#[instrument(name = "my_custom_span")]
fn my_processing_function() {
    // Your code here
}
```

Initialize a subscriber to collect the spans:

```rust
use tracing_subscriber::prelude::*;
use tracing_chrome::ChromeLayerBuilder;

let (chrome_layer, guard) = ChromeLayerBuilder::new()
    .file("trace.json")
    .build();

tracing_subscriber::registry()
    .with(chrome_layer)
    .init();

// Run your code...

// guard drops here, flushing the trace file
```

## Related Issues

- [#32](https://github.com/portolan/gpq-tiles/issues/32) - Memory usage investigation
