# Advanced Usage

## Performance Tuning

### Input File Optimization

**Always optimize GeoParquet files before tiling:**

```bash
# Install geoparquet-io
cargo install geoparquet-io

# Check and fix GeoParquet formatting
gpio check all --fix input.parquet
```

gpq-tiles will warn if input files lack optimization metadata:

```
⚠ WARNING: File lacks spatial metadata (row group bboxes).
  For optimal performance, use: gpio check all --fix
```

**Why this matters:**

- **Spatial sorting** — Groups spatially-close features, reducing tile processing overhead
- **Row group bboxes** — Enables row-group skipping for faster queries
- **Streaming efficiency** — Well-formatted row groups enable bounded-memory processing

### Compression Selection

Choose compression based on your priorities:

```bash
# Fast encoding (default)
gpq-tiles input.parquet output.pmtiles --compression zstd

# Smallest files
gpq-tiles input.parquet output.pmtiles --compression gzip

# Best compression ratio (slowest)
gpq-tiles input.parquet output.pmtiles --compression brotli
```

**Benchmark data** (3.3GB GeoParquet, zoom 0-8):

| Algorithm | Encoding Time | Output Size |
|-----------|---------------|-------------|
| zstd | 2:59 | 254MB |
| gzip | 3:04 | 175MB |
| brotli | (not benchmarked) | (smallest) |

Zstd is the default because encoding speed is prioritized. Use gzip for smaller tiles if encoding time is not a concern.

### Streaming Modes

For files larger than available memory:

```bash
# Fast mode (default): Row-group streaming
gpq-tiles large.parquet output.pmtiles --streaming-mode fast

# Low-memory mode: External sort (guaranteed bounded memory)
gpq-tiles large.parquet output.pmtiles --streaming-mode low-memory
```

**Fast mode:**
- Processes one row group at a time
- Memory bounded by largest row group (typically 100-200MB)
- Requires well-formatted input (`gpio check all --fix`)

**Low-memory mode:**
- Sorts features to disk
- Slower (multiple file passes)
- Works with any GeoParquet file

### Parallelization

By default, gpq-tiles uses parallel processing:

- **Tile-level parallelism** — Large geometries spanning many tiles processed in parallel
- **Geometry-level parallelism** — Within each row group, geometries processed in parallel

Disable for debugging or deterministic output:

```bash
# Disable tile parallelism
gpq-tiles input.parquet output.pmtiles --no-parallel

# Disable geometry parallelism
gpq-tiles input.parquet output.pmtiles --no-parallel-geoms

# Disable both
gpq-tiles input.parquet output.pmtiles --no-parallel --no-parallel-geoms
```

**Note:** Parallelization requires `--streaming-mode low-memory`. Fast mode processes row groups sequentially by design.

---

## Property Filtering Strategies

### Include Only What You Need (Whitelist)

```bash
gpq-tiles input.parquet output.pmtiles \
  --include name \
  --include population \
  --include admin_level
```

Best for: Datasets with many attributes where only a few are needed for visualization.

### Exclude Unwanted Fields (Blacklist)

```bash
gpq-tiles input.parquet output.pmtiles \
  --exclude internal_id \
  --exclude processing_timestamp \
  --exclude debug_info
```

Best for: Datasets where most attributes are useful, but a few should be removed.

### Geometry Only

```bash
gpq-tiles input.parquet output.pmtiles --exclude-all
```

Best for: Base layers, heatmaps, or cases where attributes aren't needed.

---

## Geometry Simplification

Simplification reduces vertex counts in complex geometries, dramatically shrinking tile sizes—especially for linear features like roads, rivers, and boundaries.

### Basic Usage

```bash
# Enable simplification with default tolerance (1 pixel)
gpq-tiles input.parquet output.pmtiles --simplify

# More aggressive simplification (2 pixel tolerance)
gpq-tiles input.parquet output.pmtiles --simplify --simplify-factor 2.0

# Less aggressive (preserve more detail)
gpq-tiles input.parquet output.pmtiles --simplify --simplify-factor 0.5
```

### How It Works

gpq-tiles uses the **Douglas-Peucker algorithm** with zoom-dependent tolerance:

```
tolerance = simplify_factor / (extent × 2^zoom)
```

This means:
- **Low zoom** (zoomed out): Aggressive simplification—roads become smooth arcs
- **High zoom** (zoomed in): Gentle simplification—full detail preserved

The default `simplify_factor` of `1.0` means "1 pixel tolerance"—vertices closer than 1 pixel are collapsed.

### Tile Boundary Preservation

To prevent visible seams between adjacent tiles, gpq-tiles marks vertices on tile boundaries as "necessary"—they're never removed during simplification.

This matches tippecanoe's shared-node approach: clipping happens first, then simplification, ensuring adjacent tiles share identical boundary vertices.

### When to Use Simplification

**Best for:**
- **Linear features**: Roads, rivers, rail lines, coastlines, boundaries
- **Low zoom levels**: Where vertex-dense features create oversized tiles
- **CannotReduceFurther errors**: When dropping features isn't enough

**Less useful for:**
- **Point features**: Already minimal geometry
- **Polygons with few vertices**: Not much to simplify
- **High zoom only**: Detail is already appropriate

### Combining with Other Options

Simplification works alongside feature dropping for maximum tile size reduction:

```bash
# Simplify geometry AND drop small features
gpq-tiles roads.parquet roads.pmtiles \
  --simplify \
  --drop-smallest-as-needed \
  --max-tile-size 500K

# Simplify + density dropping (typical for dense road networks)
gpq-tiles roads.parquet roads.pmtiles \
  --simplify \
  --drop-density high
```

**How they differ:**
- `--simplify` reduces **vertex count** within each feature
- `--drop-smallest-as-needed` removes **entire features** (smallest first)
- `--drop-density` limits **features per grid cell**

Use all three together for datasets with many complex linear features.

---

## Feature Dropping

Control how aggressively features are dropped at low zoom levels:

```bash
# Keep more features (less aggressive)
gpq-tiles input.parquet output.pmtiles --drop-density low

# Balanced (default)
gpq-tiles input.parquet output.pmtiles --drop-density medium

# Keep fewer features (more aggressive)
gpq-tiles input.parquet output.pmtiles --drop-density high
```

**How it works:**

- Features are binned into a 32×32 pixel grid
- At low zooms, only N features per grid cell are kept
- `low` = 10 features/cell, `medium` = 3, `high` = 1

This prevents tile overload at low zoom levels while preserving detail at high zooms.

---

## Zoom Level Selection

Choose zoom range based on your data density and use case:

```bash
# Global overview datasets (countries, admin boundaries)
gpq-tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 8

# Regional datasets (cities, road networks)
gpq-tiles input.parquet output.pmtiles --min-zoom 6 --max-zoom 14

# High-detail datasets (buildings, parcels)
gpq-tiles input.parquet output.pmtiles --min-zoom 10 --max-zoom 18
```

**Guidelines:**

| Dataset Type | Min Zoom | Max Zoom | Rationale |
|--------------|----------|----------|-----------|
| Global admin boundaries | 0 | 8 | Visible at world scale, low detail needed |
| Country/state features | 4 | 12 | Regional visibility |
| City features | 8 | 14 | Local visibility |
| Building footprints | 12 | 18 | High detail required |
| Parcel boundaries | 14 | 20 | Very high detail |

Higher max zoom = larger output files. Test interactively to find the right balance.

---

## Progress Monitoring

For long-running conversions, enable verbose output:

```bash
gpq-tiles large.parquet output.pmtiles --verbose
```

Shows:
- Row group processing progress
- Tile encoding progress
- Peak memory usage
- Total duration

Example output:
```
Phase 1: Reading row groups ████████████████████ 100% (45/45)
Phase 2: Sorting tiles
Phase 3: Encoding tiles  ████████████████████ 100% (12,453 tiles)
✓ Completed in 2m 34s (peak memory: 347MB)
```

---

## Debugging

### Verify Output Quality

```bash
# Check tile count and file size
pmtiles show output.pmtiles

# Extract a tile for inspection
pmtiles extract output.pmtiles --tile 10/512/384 > tile.mvt
```

### Identify Performance Bottlenecks

```bash
# Run with verbose output to see timing
gpq-tiles input.parquet output.pmtiles --verbose

# Disable parallelism to isolate issues
gpq-tiles input.parquet output.pmtiles --no-parallel --no-parallel-geoms --verbose
```

### Test with Smaller Datasets

```bash
# Generate tiles for limited zoom range
gpq-tiles input.parquet test.pmtiles --min-zoom 0 --max-zoom 8

# Use subset of data (requires GeoParquet tools)
gpq filter input.parquet subset.parquet --limit 10000
gpq-tiles subset.parquet test.pmtiles
```

---

## Rust Library Integration

### Custom Pipeline

For full control, use the low-level API:

```rust
use gpq_tiles_core::pipeline::{generate_tiles_to_writer, TilerConfig};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
use gpq_tiles_core::{Compression, PropertyFilter};

let config = TilerConfig::new(0, 14)
    .with_compression(Compression::Zstd)
    .with_property_filter(PropertyFilter::Include(vec!["name".into()]))
    .with_density_drop(true);

let mut writer = StreamingPmtilesWriter::new();

generate_tiles_to_writer(
    Path::new("input.parquet"),
    &config,
    &mut writer
)?;

let stats = writer.write_to_file("output.pmtiles")?;

println!("Generated {} tiles ({} unique, {} duplicates eliminated)",
         stats.tiles_written,
         stats.unique_tiles,
         stats.duplicates_eliminated);
```

### Progress Callbacks

```rust
use gpq_tiles_core::pipeline::{generate_tiles_to_writer_with_progress, ProgressEvent};
use std::sync::Arc;

let progress = Arc::new(|event: ProgressEvent| {
    match event {
        ProgressEvent::Phase1Progress { row_group, total_row_groups, .. } => {
            eprintln!("Processing row group {}/{}", row_group, total_row_groups);
        }
        ProgressEvent::Complete { total_tiles, duration_secs, .. } => {
            eprintln!("Done: {} tiles in {:.1}s", total_tiles, duration_secs);
        }
        _ => {}
    }
});

generate_tiles_to_writer_with_progress(
    Path::new("input.parquet"),
    &config,
    &mut writer,
    progress
)?;
```

### Error Handling

```rust
use gpq_tiles_core::Error;

match generate_tiles_to_writer(path, &config, &mut writer) {
    Ok(stats) => println!("Success: {} tiles", stats.peak_memory_bytes),
    Err(Error::GeoParquetRead(msg)) => {
        eprintln!("Failed to read GeoParquet: {}", msg);
        // Handle: check file exists, check permissions, verify format
    }
    Err(Error::InvalidGeometry { feature_id, reason }) => {
        eprintln!("Invalid geometry at feature {}: {}", feature_id, reason);
        // Handle: log and skip, or fail depending on requirements
    }
    Err(e) => eprintln!("Conversion failed: {}", e),
}
```

---

## Troubleshooting

### "File lacks spatial metadata" Warning

**Cause:** Input GeoParquet missing row group bboxes or not spatially sorted.

**Solution:**
```bash
gpio check all --fix input.parquet
gpq-tiles input.parquet output.pmtiles
```

### High Memory Usage

**Symptoms:** Process killed by OOM, slow performance

**Solutions:**
1. Use low-memory streaming mode:
   ```bash
   gpq-tiles input.parquet output.pmtiles --streaming-mode low-memory
   ```

2. Optimize input file (improves row-group locality):
   ```bash
   gpio check all --fix input.parquet
   ```

3. Reduce zoom range:
   ```bash
   gpq-tiles input.parquet output.pmtiles --max-zoom 12
   ```

### Slow Performance

**Common causes:**

1. **Unoptimized input** — Use `gpio check all --fix`
2. **Too many properties** — Filter with `--include` or `--exclude`
3. **Excessive zoom levels** — Reduce `--max-zoom`
4. **Slow compression** — Use `--compression zstd` instead of brotli

### Empty Tiles

**Cause:** Features don't intersect tile bounds at given zoom levels.

**Debug:**
```bash
# Check input with geoparquet-io
gpio inspect input.parquet

# Try wider zoom range
gpq-tiles input.parquet output.pmtiles --min-zoom 0 --max-zoom 18
```

### Invalid Geometry Errors

**Cause:** Input GeoParquet contains malformed geometries.

**Solution:**
```bash
# Check and fix with geoparquet-io
gpio check all --fix input.parquet
gpq-tiles input.parquet output.pmtiles
```

---

## CI/CD Integration

### GitHub Actions

```yaml
name: Generate Tiles

on:
  push:
    paths:
      - 'data/*.parquet'

jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable

      - name: Install gpq-tiles
        run: cargo install gpq-tiles

      - name: Generate tiles
        run: |
          gpq-tiles data/input.parquet output/tiles.pmtiles \
            --compression zstd \
            --verbose

      - name: Upload artifacts
        uses: actions/upload-artifact@v4
        with:
          name: tiles
          path: output/tiles.pmtiles
```

### Docker

```dockerfile
FROM rust:1.75 AS builder

RUN apt-get update && apt-get install -y protobuf-compiler

RUN cargo install gpq-tiles

FROM debian:bookworm-slim

COPY --from=builder /usr/local/cargo/bin/gpq-tiles /usr/local/bin/

ENTRYPOINT ["gpq-tiles"]
```

Usage:
```bash
docker build -t gpq-tiles .
docker run -v $(pwd)/data:/data gpq-tiles \
  /data/input.parquet /data/output.pmtiles --verbose
```
