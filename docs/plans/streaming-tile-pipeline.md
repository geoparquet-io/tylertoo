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

### freestiler's Approach (Kyle Walkerke)
- Uses DuckDB for sorting (SQL ORDER BY handles grouping)
- **TileSpool pattern**: Write encoded tiles to temp file, track index in memory
- Sort only the index (tiny) at the end, not the tile data
- Temp disk = **~1x output size**, not input size

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
│    → Parallel encoding of batch when possible               │
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

    /// Track spatial progress for completion detection
    hilbert_high_water_mark: u64,

    /// Configuration
    max_active_tiles: usize,
    tile_format: TileFormat,

    /// Statistics
    tiles_flushed: u64,
    features_processed: u64,
}

struct TileAccumulator {
    tile_id: u64,
    coord: TileCoord,
    features: Vec<TileFeature>,
    last_update_hilbert: u64,  // For LRU eviction
}

impl StreamingTileBuffer {
    /// Add a clipped feature to its tile
    pub fn add_feature(&mut self, tile_id: u64, coord: TileCoord, feature: TileFeature,
                       source_hilbert: u64) -> io::Result<()>;

    /// Check if tiles should be flushed based on spatial progress
    fn maybe_flush_completed(&mut self) -> io::Result<()>;

    /// Force eviction when memory pressure detected
    fn evict_oldest(&mut self) -> io::Result<()>;

    /// Flush all remaining tiles at end of input
    pub fn finish(self) -> io::Result<TileSpool>;
}
```

#### TileSpool (inspired by freestiler)

```rust
pub struct TileSpool {
    /// Temp file for tile data
    file: BufWriter<File>,
    path: PathBuf,

    /// In-memory index: (tile_id, offset, length)
    entries: Vec<SpoolEntry>,

    /// Current write position
    offset: u64,
}

pub struct SpoolEntry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
}

impl TileSpool {
    pub fn new() -> io::Result<Self>;

    /// Write an encoded tile, return its entry
    pub fn write_tile(&mut self, tile_id: u64, data: &[u8]) -> io::Result<()>;

    /// Get sorted entries for PMTiles directory
    pub fn into_sorted_entries(self) -> (PathBuf, Vec<SpoolEntry>);
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
        TileFormat::Mlt  // Default to MLT for better compression
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

### Tile Completion Heuristic

For well-sorted input, we detect "completed" tiles based on spatial progress:

```rust
fn should_flush_tile(&self, tile: &TileAccumulator) -> bool {
    // If we've moved significantly in Hilbert space since this tile's last update,
    // it's unlikely to receive more features
    let hilbert_distance = self.hilbert_high_water_mark - tile.last_update_hilbert;

    // Threshold based on zoom level - higher zoom = smaller tiles = flush sooner
    let threshold = completion_threshold_for_zoom(tile.coord.z);

    hilbert_distance > threshold
}

fn completion_threshold_for_zoom(z: u8) -> u64 {
    // At z0, tiles are huge - need to see more progress before flushing
    // At z14, tiles are small - can flush quickly
    // These values tuned empirically for typical GeoParquet Hilbert sorting
    match z {
        0..=4 => 1_000_000,
        5..=8 => 100_000,
        9..=12 => 10_000,
        _ => 1_000,
    }
}
```

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
    }
    Ok(())
}
```

### PMTiles Writing

```rust
pub fn write_pmtiles_from_spool(
    output_path: &str,
    spool_path: &Path,
    mut entries: Vec<SpoolEntry>,
    format: TileFormat,
    layers: &[LayerMeta],
    bounds: TileBounds,
) -> Result<(), Error> {
    // 1. Sort entries by tile_id (in memory - just the index)
    entries.sort_by_key(|e| e.tile_id);

    // 2. Write PMTiles structure
    let mut output = File::create(output_path)?;

    // Reserve space for header
    output.seek(SeekFrom::Start(HEADER_SIZE))?;

    // Write directory (references spool offsets)
    let directory_offset = output.stream_position()?;
    write_directory(&mut output, &entries)?;

    // Write metadata
    let metadata_offset = output.stream_position()?;
    write_metadata(&mut output, format, layers, bounds)?;

    // Copy tile data from spool (no reordering needed!)
    let tile_data_offset = output.stream_position()?;
    let mut spool = File::open(spool_path)?;
    io::copy(&mut spool, &mut output)?;

    // Write header with all offsets
    output.seek(SeekFrom::Start(0))?;
    write_header(&mut output, format, directory_offset, metadata_offset, tile_data_offset)?;

    // Patch tile_type byte for MLT if needed
    if format == TileFormat::Mlt {
        patch_tile_type_for_mlt(&mut output)?;
    }

    Ok(())
}
```

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

- Port MLT encoding from freestiler
- Add `TileFormat` enum with `Mvt` and `Mlt` variants
- Update `TilerConfig` with `tile_format` option

**Estimated effort:** 3-4 hours

### Step 2: Implement TileSpool
**New file:** `crates/core/src/tile_spool.rs`

- `TileSpool` struct with append-only file + in-memory index
- Gzip compression for tiles
- `into_sorted_entries()` for PMTiles writing

**Estimated effort:** 2-3 hours

### Step 3: Implement StreamingTileBuffer
**New file:** `crates/core/src/streaming_buffer.rs`

- `StreamingTileBuffer` with HashMap-based accumulation
- Hilbert-based completion detection
- Memory pressure eviction
- Integration with TileSpool

**Estimated effort:** 4-5 hours

### Step 4: Update PMTiles writer
**Modified file:** `crates/core/src/pmtiles_writer.rs`

- Add `write_pmtiles_from_spool()` function
- Support both MVT and MLT tile types
- Handle directory building from spool entries

**Estimated effort:** 2-3 hours

### Step 5: Integrate into pipeline
**Modified file:** `crates/core/src/pipeline.rs`

- Replace external sort with StreamingTileBuffer
- Wire up tile format selection
- Update progress reporting

**Estimated effort:** 3-4 hours

### Step 6: Update CLI
**Modified file:** `crates/cli/src/main.rs`

- Add `--tile-format [mvt|mlt]` flag (default: mlt)
- Document MLT benefits and compatibility considerations

**Estimated effort:** 1 hour

### Step 7: Deprecate external sort
**Modified file:** `crates/core/src/external_sort.rs`

- Add `#[deprecated]` attribute
- Keep for potential fallback use
- Remove in future release

**Estimated effort:** 30 minutes

## Files Summary

| File | Change |
|------|--------|
| `crates/core/src/lib.rs` | Add new modules |
| `crates/core/src/mlt.rs` | **New:** MLT encoding |
| `crates/core/src/tile_spool.rs` | **New:** TileSpool implementation |
| `crates/core/src/streaming_buffer.rs` | **New:** StreamingTileBuffer |
| `crates/core/src/pmtiles_writer.rs` | Add spool-based writing |
| `crates/core/src/pipeline.rs` | Replace external sort |
| `crates/core/src/external_sort.rs` | Deprecate |
| `crates/cli/src/main.rs` | Add tile-format flag |

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
# Default: MLT encoding (best compression)
gpq-tiles input.parquet output.pmtiles

# Explicit MVT for compatibility with older viewers
gpq-tiles input.parquet output.pmtiles --tile-format mvt

# Explicit MLT
gpq-tiles input.parquet output.pmtiles --tile-format mlt
```

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Poorly-sorted input causes many evictions | Document requirement for `gpio optimize`. Add warning if eviction rate is high. |
| Late arrivals after tile flushed | Track "reopened" tiles metric. If excessive, warn user about input sorting. |
| MLT compatibility with older viewers | Default to MLT but provide `--tile-format mvt` escape hatch. Document in CLI help. |
| Memory spikes from large tiles | Configurable `max_active_tiles`. Monitor and auto-reduce if needed. |
| Spool file corruption | Validate spool entries before PMTiles write. Checksum option for paranoid mode. |

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
| Step 1: MLT encoding | 3-4 hours |
| Step 2: TileSpool | 2-3 hours |
| Step 3: StreamingTileBuffer | 4-5 hours |
| Step 4: PMTiles writer | 2-3 hours |
| Step 5: Pipeline integration | 3-4 hours |
| Step 6: CLI update | 1 hour |
| Step 7: Deprecation | 30 min |
| Testing & debugging | 4-5 hours |
| **Total** | **~20-25 hours (3-4 days)** |

## References

- [freestiler](https://github.com/walkerke/freestiler) - Kyle Walker's streaming tile implementation
- [MapLibre Tile Spec](https://maplibre.org/maplibre-tile-spec/) - MLT format specification
- [PMTiles Spec](https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md) - PMTiles v3 format
- [tippecanoe](https://github.com/felt/tippecanoe) - Reference tiling implementation
