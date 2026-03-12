# Plan: Streaming Tile Pipeline with Spool-Based PMTiles Writing

## Context

**Problem:** The current external sort implementation creates too many temp files during the merge phase. With 100M+ records, this exceeds the default `ulimit -n` (1024), causing "Too many open files (os error 24)".

**Current workaround:** Buffer size increased to 2M records (~1-2GB RAM) to reduce temp file count. This trades memory for fewer files and requires ~2x input size in temp disk space.

**Issue:** https://github.com/geoparquet-io/gpq-tiles/issues/114

## Research Findings

### tippecanoe's Approach
- Uses fixed shard count bounded by system limits
- Features written directly to shard files during read phase
- Still requires significant temp disk (~2x input size)

### freestiler's Approach (Kyle Walker)
- Uses DuckDB for sorting (SQL ORDER BY handles grouping)
- **TileSpool pattern**: Write encoded tiles to temp file, track index in memory
- Sort only the index (tiny) at the end, not the tile data
- Temp disk = **~1x output size**, not input size
- **Native Rust MLT encoder**: ~700 lines, MIT licensed, uses `integer-encoding` for varints + `rayon` for parallel encoding

### Key Insight
For well-sorted GeoParquet input (via `gpio optimize`), features arrive in spatial Hilbert order. This means:
- Features for nearby tiles arrive together
- Only a small "active tiles" set needs to be in memory at any time
- We can detect when tiles are "complete" and flush them immediately

**We don't need DuckDB** - the pre-sorted input gives us the same benefit.

## Design

### Architecture

```
Current:
  Read → Clip → External Sort (many temp files) → Encode → PMTiles

Proposed:
  Read → Clip → Streaming Tile Buffer → TileSpool → PMTiles
                     ↓                      ↓
              O(active_tiles)        ~1x output size
```

### Data Flow

```
┌─────────────────────────────────────────────────────────────┐
│ 1. Read GeoParquet (Hilbert-sorted via gpio)                │
│    → Features arrive in spatial order                       │
│    → Row groups streamed, not loaded entirely               │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│ 2. Clip & Assign to Tiles                                   │
│    → Hierarchical clipping (unchanged from current)         │
│    → Each feature → one or more (tile_id, clipped_geom)     │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│ 3. Streaming Tile Buffer                                    │
│    → HashMap<TileId, Vec<Feature>> for active tiles         │
│    → Track spatial progress via Hilbert index               │
│    → Flush "completed" tiles to spool as we progress        │
│    → Memory pressure triggers early eviction                │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│ 4. Tile Encoding (MVT or MLT)                               │
│    → Encode tile when flushing from buffer                  │
│    → Support both MVT (compatibility) and MLT (compression) │
│    → Parallel encoding via channel + rayon (see below)      │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│ 5. TileSpool                                                │
│    → Single temp file, append-only writes                   │
│    → Track Vec<Entry> index in memory: (tile_id, offset, len)│
│    → Tiles written in arrival order (not tile_id order)     │
└─────────────────────────────────────────────────────────────┘
                              ↓
┌─────────────────────────────────────────────────────────────┐
│ 6. Write PMTiles                                            │
│    → Sort index entries by tile_id (in memory, small)       │
│    → Write PMTiles header + directory                       │
│    → Copy tile data from spool (no reordering needed)       │
└─────────────────────────────────────────────────────────────┘
```

### Core Components

#### StreamingTileBuffer

```rust
pub struct StreamingTileBuffer {
    /// Active tiles being accumulated
    active_tiles: HashMap<u64, TileAccumulator>,

    /// Output spool for completed tiles
    spool: TileSpool,

    /// Track which tiles have been flushed (for late arrival detection)
    flushed_tiles: HashSet<u64>,

    /// Track spatial progress for completion detection
    hilbert_high_water_mark: u64,

    /// Configuration
    config: StreamingConfig,

    /// Statistics
    stats: StreamingStats,
}

pub struct StreamingConfig {
    pub max_active_tiles: usize,
    pub tile_format: TileFormat,
    /// Warn if late arrival rate exceeds this threshold (0.0-1.0)
    pub late_arrival_warn_threshold: f64,  // default: 0.05 (5%)
    /// Fallback to external sort if late arrival rate exceeds this
    pub late_arrival_fallback_threshold: f64,  // default: 0.20 (20%)
}

struct StreamingStats {
    tiles_flushed: u64,
    features_processed: u64,
    late_arrivals: u64,      // Features arriving for already-flushed tiles
    evictions: u64,          // Memory pressure evictions
}

struct TileAccumulator {
    tile_id: u64,
    coord: TileCoord,
    features: Vec<TileFeature>,
    last_update_hilbert: u64,  // For LRU eviction
}

impl StreamingTileBuffer {
    /// Add a clipped feature to its tile
    ///
    /// If the tile was already flushed (late arrival), we create a new accumulator.
    /// The sparse spool handles deduplication - only the last entry per tile_id is kept.
    pub fn add_feature(&mut self, tile_id: u64, coord: TileCoord, feature: TileFeature,
                       source_hilbert: u64) -> io::Result<()> {
        // Detect late arrivals
        if self.flushed_tiles.contains(&tile_id) {
            self.stats.late_arrivals += 1;
            // Don't error - sparse spool handles this. Just recreate the accumulator.
        }

        self.active_tiles
            .entry(tile_id)
            .or_insert_with(|| TileAccumulator::new(tile_id, coord))
            .add_feature(feature, source_hilbert);

        self.stats.features_processed += 1;
        self.hilbert_high_water_mark = self.hilbert_high_water_mark.max(source_hilbert);

        self.maybe_flush_completed()?;
        self.maybe_warn_late_arrivals();
        Ok(())
    }

    /// Check if tiles should be flushed based on spatial progress
    fn maybe_flush_completed(&mut self) -> io::Result<()>;

    /// Force eviction when memory pressure detected
    fn evict_oldest(&mut self) -> io::Result<()>;

    /// Warn if late arrival rate is high (indicates poorly-sorted input)
    fn maybe_warn_late_arrivals(&self) {
        let rate = self.stats.late_arrivals as f64 / self.stats.features_processed as f64;
        if rate > self.config.late_arrival_warn_threshold && self.stats.features_processed % 100_000 == 0 {
            eprintln!(
                "Warning: {:.1}% late arrivals detected. Input may be poorly sorted. \
                Consider running `gpio optimize` for better performance.",
                rate * 100.0
            );
        }
    }

    /// Flush all remaining tiles at end of input
    pub fn finish(self) -> io::Result<(TileSpool, StreamingStats)>;
}
```

#### TileSpool (inspired by freestiler)

```rust
pub struct TileSpool {
    /// Temp file for tile data
    file: BufWriter<File>,
    path: PathBuf,

    /// In-memory index: (tile_id, offset, length)
    /// May contain multiple entries per tile_id (sparse spool pattern)
    entries: Vec<SpoolEntry>,

    /// Current write position
    offset: u64,
}

pub struct SpoolEntry {
    pub tile_id: u64,
    /// Offset within the spool file (NOT the final PMTiles offset)
    pub spool_offset: u64,
    pub length: u32,
}

impl TileSpool {
    pub fn new() -> io::Result<Self>;

    /// Write an encoded tile, return its entry.
    /// Multiple writes for the same tile_id are allowed (late arrivals).
    pub fn write_tile(&mut self, tile_id: u64, data: &[u8]) -> io::Result<()>;

    /// Get deduplicated, sorted entries for PMTiles directory.
    /// Only keeps the LAST entry for each tile_id (contains all features).
    pub fn into_sorted_entries(mut self) -> (PathBuf, Vec<SpoolEntry>) {
        // Sort by (tile_id, spool_offset) - offset gives arrival order
        self.entries.sort_by_key(|e| (e.tile_id, e.spool_offset));

        // Deduplicate: keep only the last entry per tile_id
        // (the last entry has all features, earlier ones are incomplete)
        let mut deduped = Vec::with_capacity(self.entries.len());
        for entry in self.entries {
            if deduped.last().map_or(true, |last: &SpoolEntry| last.tile_id != entry.tile_id) {
                deduped.push(entry);
            } else {
                // Same tile_id - replace with newer entry
                *deduped.last_mut().unwrap() = entry;
            }
        }

        (self.path, deduped)
    }
}
```

#### Tile Encoding (MVT and MLT)

```rust
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TileFormat {
    /// Mapbox Vector Tiles - maximum compatibility
    Mvt,
    /// MapLibre Tiles - better compression (up to 6x on large tiles)
    Mlt,
}

impl Default for TileFormat {
    fn default() -> Self {
        TileFormat::Mvt  // Default to MVT for maximum compatibility
    }
}

/// Encode a tile's features into the specified format
pub fn encode_tile(
    coord: &TileCoord,
    layer_name: &str,
    features: &[TileFeature],
    format: TileFormat,
) -> Vec<u8> {
    match format {
        TileFormat::Mvt => encode_tile_mvt(coord, layer_name, features),
        TileFormat::Mlt => encode_tile_mlt(coord, layer_name, features),
    }
}
```

### Parallel Tile Encoding Architecture

Tile encoding (especially MLT) is CPU-intensive. We use a channel-based architecture
with rayon for parallel encoding while maintaining single-threaded spool writes:

```rust
use crossbeam_channel::{bounded, Sender, Receiver};
use rayon::prelude::*;

/// Tiles ready for encoding
struct PendingTile {
    tile_id: u64,
    coord: TileCoord,
    features: Vec<TileFeature>,
}

/// Encoded tile ready for spool
struct EncodedTile {
    tile_id: u64,
    data: Vec<u8>,
}

impl StreamingTileBuffer {
    /// Flush multiple tiles in parallel
    fn flush_batch(&mut self, tiles: Vec<TileAccumulator>) -> io::Result<()> {
        let format = self.config.tile_format;
        let layer_name = &self.layer_name;

        // Parallel encode using rayon
        let encoded: Vec<EncodedTile> = tiles
            .into_par_iter()
            .map(|acc| {
                let data = encode_tile(&acc.coord, layer_name, &acc.features, format);
                EncodedTile { tile_id: acc.tile_id, data }
            })
            .collect();

        // Sequential write to spool (single file handle)
        for tile in encoded {
            self.spool.write_tile(tile.tile_id, &tile.data)?;
            self.flushed_tiles.insert(tile.tile_id);
        }

        self.stats.tiles_flushed += tiles.len() as u64;
        Ok(())
    }

    /// Batch tiles for parallel encoding when we have enough
    fn maybe_flush_completed(&mut self) -> io::Result<()> {
        let mut to_flush = Vec::new();

        for (tile_id, acc) in &self.active_tiles {
            if self.should_flush_tile(acc) {
                to_flush.push(*tile_id);
            }
        }

        // Batch if we have multiple tiles to flush
        if to_flush.len() >= self.config.parallel_batch_size {
            let tiles: Vec<_> = to_flush.iter()
                .filter_map(|id| self.active_tiles.remove(id))
                .collect();
            self.flush_batch(tiles)?;
        } else if !to_flush.is_empty() {
            // Small batch - encode sequentially to avoid rayon overhead
            for tile_id in to_flush {
                if let Some(acc) = self.active_tiles.remove(&tile_id) {
                    let data = encode_tile(&acc.coord, &self.layer_name, &acc.features, self.config.tile_format);
                    self.spool.write_tile(acc.tile_id, &data)?;
                    self.flushed_tiles.insert(acc.tile_id);
                    self.stats.tiles_flushed += 1;
                }
            }
        }

        Ok(())
    }
}
```

**Recommendation:** Yes, parallel encoding is worthwhile:
- MLT encoding is ~5-10x more CPU-intensive than MVT
- Typical speedup: 2-4x on 4+ core machines
- Rayon handles work-stealing efficiently
- Sequential spool writes avoid contention

**Config default:** `parallel_batch_size = 16` (tune based on benchmarks)

### Tile Completion Heuristic

For well-sorted input, we detect "completed" tiles based on spatial progress.

**Key insight**: The Hilbert curve maps 2D space to 1D. If input is Hilbert-sorted,
features for a tile arrive in a contiguous "window" of Hilbert indices. Once we've
moved past that window, no more features will arrive for that tile.

#### Adaptive Threshold Calibration

Instead of hardcoded magic numbers, we **calibrate thresholds dynamically** during
the first N features:

```rust
pub struct HilbertCalibrator {
    /// Sample: (tile_id, hilbert_index) for first N features
    samples: Vec<(u64, u64)>,
    /// Number of samples to collect before calibrating
    calibration_size: usize,  // default: 100_000
    /// Calibrated thresholds per zoom level
    thresholds: Option<[u64; 15]>,
}

impl HilbertCalibrator {
    pub fn add_sample(&mut self, tile_id: u64, hilbert: u64) {
        if self.samples.len() < self.calibration_size {
            self.samples.push((tile_id, hilbert));
        }
        if self.samples.len() == self.calibration_size && self.thresholds.is_none() {
            self.calibrate();
        }
    }

    fn calibrate(&mut self) {
        // Group samples by tile_id, compute Hilbert span for each tile
        let mut tile_spans: HashMap<u64, (u64, u64)> = HashMap::new();
        for &(tile_id, hilbert) in &self.samples {
            tile_spans.entry(tile_id)
                .and_modify(|(min, max)| {
                    *min = (*min).min(hilbert);
                    *max = (*max).max(hilbert);
                })
                .or_insert((hilbert, hilbert));
        }

        // Compute p95 span per zoom level
        let mut spans_by_zoom: [Vec<u64>; 15] = Default::default();
        for (tile_id, (min, max)) in tile_spans {
            let z = zoom_from_tile_id(tile_id);
            spans_by_zoom[z as usize].push(max - min);
        }

        let mut thresholds = [0u64; 15];
        for z in 0..15 {
            let spans = &mut spans_by_zoom[z];
            if spans.is_empty() {
                // Fallback for zoom levels with no samples
                thresholds[z] = 10u64.pow(6 - z.min(5) as u32);
            } else {
                spans.sort_unstable();
                // Use p95 span * 2 as threshold (safe margin)
                let p95_idx = (spans.len() * 95) / 100;
                thresholds[z] = spans[p95_idx] * 2;
            }
        }

        self.thresholds = Some(thresholds);
        log::info!("Calibrated Hilbert thresholds: {:?}", thresholds);
    }

    pub fn threshold_for_zoom(&self, z: u8) -> u64 {
        self.thresholds
            .map(|t| t[z.min(14) as usize])
            .unwrap_or(100_000)  // Conservative default before calibration
    }
}
```

#### Flush Decision

```rust
fn should_flush_tile(&self, tile: &TileAccumulator) -> bool {
    let hilbert_distance = self.hilbert_high_water_mark
        .saturating_sub(tile.last_update_hilbert);

    let threshold = self.calibrator.threshold_for_zoom(tile.coord.z);

    hilbert_distance > threshold
}
```

#### Validation Strategy

Before merging, validate thresholds on 3+ real datasets:

1. **OpenStreetMap buildings** (dense polygons, global)
2. **US Census blocks** (polygons, USA-only)
3. **OpenAddresses points** (points, sparse/dense mix)

Acceptance criteria:
- Late arrival rate < 5% on all datasets
- Memory usage < 500MB for 100M feature dataset
- No tile data corruption (compare with tippecanoe output)

### Memory Pressure Handling

When active tiles exceed threshold:

```rust
fn evict_oldest(&mut self) -> io::Result<()> {
    // Find tile with oldest last_update_hilbert (furthest behind spatially)
    let oldest_tile_id = self.active_tiles.iter()
        .min_by_key(|(_, acc)| acc.last_update_hilbert)
        .map(|(id, _)| *id);

    if let Some(tile_id) = oldest_tile_id {
        let accumulator = self.active_tiles.remove(&tile_id).unwrap();
        self.flush_tile(accumulator)?;
        self.stats.evictions += 1;
    }
    Ok(())
}
```

### Automatic Fallback for Unsorted Input

If the input is poorly sorted, streaming is inefficient (many late arrivals, excessive evictions).
We detect this and offer/trigger a fallback to external sort:

```rust
pub enum SortingStrategy {
    /// Streaming with spool (default for sorted input)
    Streaming,
    /// External sort (fallback for unsorted input)
    ExternalSort,
    /// Let the system decide based on observed metrics
    Auto,
}

impl StreamingTileBuffer {
    /// Check if we should recommend falling back to external sort
    fn should_fallback(&self) -> Option<FallbackReason> {
        let late_rate = self.stats.late_arrivals as f64 / self.stats.features_processed as f64;
        let eviction_rate = self.stats.evictions as f64 / self.stats.tiles_flushed as f64;

        if late_rate > self.config.late_arrival_fallback_threshold {
            return Some(FallbackReason::HighLateArrivalRate(late_rate));
        }

        // If we're evicting more than 50% of tiles due to memory pressure,
        // external sort would be more efficient
        if eviction_rate > 0.5 && self.stats.tiles_flushed > 1000 {
            return Some(FallbackReason::HighEvictionRate(eviction_rate));
        }

        None
    }
}

pub enum FallbackReason {
    HighLateArrivalRate(f64),
    HighEvictionRate(f64),
}

impl FallbackReason {
    pub fn message(&self) -> String {
        match self {
            Self::HighLateArrivalRate(rate) => format!(
                "Input appears unsorted ({:.1}% late arrivals). \
                Consider: (1) run `gpio optimize input.parquet` first, or \
                (2) use `--sorting-strategy external` to force external sort.",
                rate * 100.0
            ),
            Self::HighEvictionRate(rate) => format!(
                "High memory pressure ({:.1}% eviction rate). \
                Consider: (1) increase --max-active-tiles, or \
                (2) use `--sorting-strategy external` for unsorted input.",
                rate * 100.0
            ),
        }
    }
}
```

#### CLI Flags

```bash
# Auto-detect (default): use streaming, warn if input is poorly sorted
gpq-tiles input.parquet output.pmtiles

# Force streaming (fail if input is very poorly sorted)
gpq-tiles input.parquet output.pmtiles --sorting-strategy streaming

# Force external sort (works with any input, but uses more disk/memory)
gpq-tiles input.parquet output.pmtiles --sorting-strategy external
```

### PMTiles Writing

**CRITICAL**: PMTiles directory entries store offsets relative to the tile data section start.
The spool stores tiles in arrival order (not tile_id order), so we must:
1. Copy tiles from spool in **tile_id order** (not bulk copy)
2. Calculate the final offset as we write each tile

```rust
pub fn write_pmtiles_from_spool(
    output_path: &str,
    spool_path: &Path,
    mut entries: Vec<SpoolEntry>,  // Already deduplicated
    format: TileFormat,
    layers: &[LayerMeta],
    bounds: TileBounds,
) -> Result<(), Error> {
    // 1. Sort entries by tile_id for PMTiles directory order
    entries.sort_by_key(|e| e.tile_id);

    // 2. Create output file, reserve space for header
    let mut output = File::create(output_path)?;
    output.seek(SeekFrom::Start(HEADER_SIZE as u64))?;

    // 3. Write metadata first (we know its content)
    let metadata_offset = output.stream_position()?;
    let metadata_bytes = build_metadata(format, layers, bounds)?;
    output.write_all(&metadata_bytes)?;

    // 4. Write tile data, building directory entries with correct offsets
    let tile_data_offset = output.stream_position()?;
    let mut spool_reader = BufReader::new(File::open(spool_path)?);
    let mut directory_entries = Vec::with_capacity(entries.len());
    let mut current_offset: u64 = 0;

    for entry in &entries {
        // Seek to tile data in spool
        spool_reader.seek(SeekFrom::Start(entry.spool_offset))?;

        // Read tile data
        let mut tile_data = vec![0u8; entry.length as usize];
        spool_reader.read_exact(&mut tile_data)?;

        // Write to output
        output.write_all(&tile_data)?;

        // Record directory entry with FINAL offset (relative to tile_data_offset)
        directory_entries.push(DirectoryEntry {
            tile_id: entry.tile_id,
            offset: current_offset,
            length: entry.length,
            run_length: 1,  // No run-length encoding for now
        });

        current_offset += entry.length as u64;
    }

    // 5. Write directory (after tile data, so we know all offsets)
    let directory_offset = output.stream_position()?;
    write_directory(&mut output, &directory_entries)?;

    // 6. Write header with all section offsets
    output.seek(SeekFrom::Start(0))?;
    write_header(
        &mut output,
        format,
        metadata_offset,
        metadata_bytes.len() as u64,
        tile_data_offset,
        current_offset,  // total tile data size
        directory_offset,
    )?;

    Ok(())
}
```

**Note:** This is slightly slower than bulk copy (we seek per tile), but correctness requires
writing tiles in tile_id order. For a 1M tile dataset, this adds ~10-20 seconds overhead.

## Resource Analysis

### Memory Usage

| Component | Size | Notes |
|-----------|------|-------|
| Active tiles buffer | O(max_active × avg_tile_size) | Default: ~500 tiles × ~100KB = ~50MB |
| Spool index | O(num_tiles × 20 bytes) | 1M tiles = 20MB |
| Row group buffer | O(row_group_size) | Unchanged from current |
| **Total** | **~100-500MB** | Bounded, configurable |

### Disk Usage

| Approach | Temp Disk | Notes |
|----------|-----------|-------|
| Current (external sort) | ~2× input size | 100GB input → ~200GB temp |
| **Proposed (spool)** | **~1× output size** | 100GB input → ~3GB temp |

### File Handles

| Phase | Files Open |
|-------|-----------|
| Read GeoParquet | 1 (input file) |
| Write to spool | 1 (temp file) |
| Write PMTiles | 2 (spool read + output write) |
| **Maximum** | **3 files** |

## Implementation Plan

### Step 1: Add MLT encoding support
**New file:** `crates/core/src/mlt.rs`

**Approach:** Vendor freestiler's MIT-licensed `mlt.rs` directly, then adapt to our types.

```bash
# Vendor the file
curl -o crates/core/src/mlt.rs \
  https://raw.githubusercontent.com/walkerke/freestiler/main/src/rust/freestiler-core/src/mlt.rs
```

**Adaptations needed:**
1. Replace freestiler's `Feature` type with our `TileFeature`
2. Replace freestiler's `Geometry` enum with our `geo::Geometry`
3. Replace freestiler's `TileCoord` with ours (should be compatible)
4. Add `TileFormat` enum with `Mvt` and `Mlt` variants
5. Update `TilerConfig` with `tile_format` option

**New dependency:** `integer-encoding = "4"` (for varints)

**Attribution:** Add to `NOTICE.md`:
```
MLT encoding adapted from freestiler (https://github.com/walkerke/freestiler)
Copyright (c) 2026 Kyle Walker, MIT License
```

**Estimated effort:** 4-6 hours (vendoring + type adaptation + testing)

### Step 2: Implement TileSpool
**New file:** `crates/core/src/tile_spool.rs`

- `TileSpool` struct with append-only file + in-memory index
- Gzip compression for tiles
- Sparse spool pattern: allow multiple entries per tile_id
- `into_sorted_entries()` with deduplication for PMTiles writing

**Estimated effort:** 3-4 hours

### Step 3: Implement Parallel Tile Encoding
**Modified file:** `crates/core/src/tile_encoder.rs` (new)

- Channel-based architecture with rayon for parallel encoding
- `flush_batch()` for parallel encoding of multiple tiles
- Sequential spool writes to avoid file handle contention
- Configurable `parallel_batch_size` (default: 16)

**New dependency:** `rayon = "1.10"` (already in freestiler)

```rust
// Core parallel encoding pattern
let encoded: Vec<EncodedTile> = tiles
    .into_par_iter()
    .map(|acc| EncodedTile {
        tile_id: acc.tile_id,
        data: encode_tile(&acc.coord, layer_name, &acc.features, format),
    })
    .collect();

// Sequential write to spool
for tile in encoded {
    self.spool.write_tile(tile.tile_id, &tile.data)?;
}
```

**Estimated effort:** 2-3 hours

### Step 4: Implement StreamingTileBuffer
**New file:** `crates/core/src/streaming_buffer.rs`

- `StreamingTileBuffer` with HashMap-based accumulation
- Late arrival detection with `flushed_tiles: HashSet<u64>`
- Hilbert-based completion detection with adaptive calibration
- Memory pressure eviction
- Integration with TileSpool and parallel encoder

**Estimated effort:** 5-6 hours

### Step 5: Update PMTiles writer
**Modified file:** `crates/core/src/pmtiles_writer.rs`

- Add `write_pmtiles_from_spool()` function
- Write tiles in tile_id order (not bulk copy) for correct offsets
- Support both MVT and MLT tile types
- Handle directory building from spool entries

**Estimated effort:** 3-4 hours

### Step 6: Integrate into pipeline
**Modified file:** `crates/core/src/pipeline.rs`

- Replace external sort with StreamingTileBuffer
- Wire up tile format selection
- Add `--sorting-strategy` flag with auto-fallback logic
- Update progress reporting

**Estimated effort:** 4-5 hours

### Step 7: Update CLI
**Modified file:** `crates/cli/src/main.rs`

- Add `--tile-format [mvt|mlt]` flag (default: mvt)
- Add `--sorting-strategy [auto|streaming|external]` flag
- Document MLT benefits and compatibility considerations

**Estimated effort:** 2 hours

### Step 8: Deprecate external sort
**Modified file:** `crates/core/src/external_sort.rs`

- Add `#[deprecated]` attribute
- Keep for `--sorting-strategy external` fallback
- Remove in future release

**Estimated effort:** 30 minutes

## Files Summary

| File | Change |
|------|--------|
| `crates/core/src/lib.rs` | Add new modules |
| `crates/core/src/mlt.rs` | **New:** MLT encoding (vendored from freestiler) |
| `crates/core/src/tile_spool.rs` | **New:** TileSpool with sparse dedup |
| `crates/core/src/tile_encoder.rs` | **New:** Parallel encoding with rayon |
| `crates/core/src/streaming_buffer.rs` | **New:** StreamingTileBuffer with late arrival handling |
| `crates/core/src/pmtiles_writer.rs` | Add spool-based writing with correct offsets |
| `crates/core/src/pipeline.rs` | Replace external sort, add fallback logic |
| `crates/core/src/external_sort.rs` | Deprecate (keep for fallback) |
| `crates/cli/src/main.rs` | Add `--tile-format` and `--sorting-strategy` flags |
| `NOTICE.md` | **New:** Attribution for freestiler MLT code |

## Testing Strategy

### Unit Tests

1. `test_tile_spool_round_trip` - write tiles, read back, verify
2. `test_streaming_buffer_flush_on_progress` - verify completion detection
3. `test_streaming_buffer_eviction` - verify memory pressure handling
4. `test_mlt_encoding` - verify MLT output is valid
5. `test_mvt_encoding` - verify MVT output unchanged

### Integration Tests

1. `test_small_dataset_streaming` - end-to-end with small input
2. `test_well_sorted_input` - verify minimal memory usage with sorted GeoParquet
3. `test_poorly_sorted_input` - verify correctness (more evictions, but still works)
4. `test_output_matches_current` - compare PMTiles output with old implementation

### Performance Tests

```bash
# Benchmark with large dataset
cargo bench -- streaming_pipeline

# Manual verification
hyperfine \
  'cargo run --release -- input.parquet output_old.pmtiles --legacy-sort' \
  'cargo run --release -- input.parquet output_new.pmtiles'

# Verify resource usage
/usr/bin/time -v cargo run --release -- large.parquet output.pmtiles
```

## CLI Interface

```bash
# Default: MVT encoding (maximum compatibility)
gpq-tiles input.parquet output.pmtiles

# Explicit MLT for better compression (up to 6x on large tiles)
# Note: Requires MLT-compatible viewer (MapLibre GL JS with MLT plugin)
gpq-tiles input.parquet output.pmtiles --tile-format mlt

# Explicit MVT (same as default)
gpq-tiles input.parquet output.pmtiles --tile-format mvt
```

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Poorly-sorted input causes many evictions | Auto-detect via metrics, warn user, offer `--sorting-strategy external` fallback |
| Late arrivals after tile flushed | Sparse spool pattern: allow multiple entries, deduplicate at PMTiles write time |
| MLT compatibility with older viewers | Default to MVT; MLT opt-in via `--tile-format mlt` |
| Memory spikes from large tiles | Configurable `max_active_tiles`. Eviction based on Hilbert distance, not just count |
| Spool file corruption | Validate spool entries before PMTiles write. Checksum option for paranoid mode |
| PMTiles offset calculation | Write tiles in tile_id order to output (not bulk copy), calculate offsets on the fly |
| Hilbert threshold tuning | Adaptive calibration from first 100K features, validate on 3+ datasets before merge |

## Requirements for Large Files (100GB+)

1. **Memory:** ~100-500MB (bounded by active tiles buffer)
2. **Local disk:** ~1× output size for spool (e.g., 3GB for typical 100GB input)
3. **Input sorting:** GeoParquet should be Hilbert-sorted via `gpio optimize`
4. **Remote storage:** Must support streaming/range requests (S3, GCS, Azure Blob)

## Comparison with Previous Approach

| Aspect | Sharded Sort | Streaming + Spool |
|--------|--------------|-------------------|
| Temp disk | ~2× input | ~1× output |
| Memory | ~2GB | ~100-500MB |
| File handles | ~64-88 | 3 |
| Complexity | High | Medium |
| Depends on sorted input | No | Yes (for optimal perf) |
| MLT support | No | Yes |

## Estimated Total Effort

| Step | Effort |
|------|--------|
| Step 1: MLT encoding (vendor + adapt) | 4-6 hours |
| Step 2: TileSpool (with sparse dedup) | 3-4 hours |
| Step 3: Parallel tile encoding | 2-3 hours |
| Step 4: StreamingTileBuffer (with late arrival handling) | 5-6 hours |
| Step 5: PMTiles writer (correct offset calculation) | 3-4 hours |
| Step 6: Pipeline integration + fallback logic | 4-5 hours |
| Step 7: CLI update (tile-format + sorting-strategy) | 2 hours |
| Step 8: Deprecation | 30 min |
| Hilbert calibration + validation on 3 datasets | 6-8 hours |
| Testing & debugging | 6-8 hours |
| **Total** | **~36-47 hours (~1-1.5 weeks)** |

## References

- [freestiler](https://github.com/walkerke/freestiler) - Kyle Walker's streaming tile implementation
- [MapLibre Tile Spec](https://maplibre.org/maplibre-tile-spec/) - MLT format specification
- [PMTiles Spec](https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md) - PMTiles v3 format
- [tippecanoe](https://github.com/felt/tippecanoe) - Reference tiling implementation
