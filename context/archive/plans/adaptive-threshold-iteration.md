# Adaptive Threshold Iteration

**Status:** Ready for Implementation  
**Issues:** #28 (gap-based), #132 (extent-based)  
**Branch:** `feat/adaptive-threshold-iteration`

## Overview

Implement tippecanoe's iterative percentile-based threshold adjustment for automatic tile size control. When tiles exceed size/feature limits, sample the relevant metric (gap or extent), compute a percentile-based threshold, and retry encoding until tiles fit within limits.

This PR implements **both** issues together because they share 80% of infrastructure.

## Problem Statement

Currently, tylertoo uses fixed thresholds for feature dropping:
- `--drop-smallest-as-needed` uses a static pixel area threshold (default 4.0 sq px)
- `--drop-densest-as-needed` uses gamma-based exponential dropping

When tiles exceed size limits, there's no automatic adjustment. Users must manually tune thresholds through trial and error.

**Goal:** Automatic, adaptive threshold selection that guarantees tiles fit within size/feature limits while preserving maximum features.

## Design Decisions

All decisions are FINAL. Do not deviate without explicit approval.

| Decision | Choice | Rationale |
|----------|--------|-----------|
| **Sampling** | Incremental halving, 100K cap | Match tippecanoe exactly |
| **Percentile** | `ix = (size-1) * (1-f)` with ratcheting | Tippecanoe algorithm |
| **Multipliers** | mingap=0.80, minextent=0.75 | Tippecanoe values |
| **Cross-tile** | Per-zoom propagation + zoom retry | Feature-complete requirement |
| **Prediction** | Extend `CoalesceTargets` pattern | Leverage GeoParquet metadata |
| **Failure** | `Error(CannotReduceFurther)` | Match tippecanoe |
| **Types** | gaps=`u64`, extents=`i64`, fractions=`f64` | Match existing codebase |
| **Defaults** | 500KB, 200K features, 100K samples | Tippecanoe defaults |

## Architecture

### High-Level Flow

```
Phase 0: Metadata Scan (extends CoalesceTargets pattern)
├── Read row group metadata (no decompression)
├── Estimate tile density at each zoom
├── Sample features for gap/extent distributions
└── Compute initial per-zoom thresholds

Phase 1-3: Streaming Pipeline (existing, modified)
├── Get initial threshold from AdaptiveTargets
├── Encode tile with threshold
├── If exceeds limits: local retry with increased threshold
└── Report final threshold to AdaptiveTargets

Phase 4: Zoom-Level Propagation (new)
├── After encoding all tiles at zoom Z
├── If any tile reported higher threshold than initial
│   └── Re-encode entire zoom Z with new threshold
└── Propagate threshold to zoom Z+1
```

### Key Data Structures

```rust
/// Bounded sampler using incremental halving (tippecanoe algorithm)
/// Generic over T: Ord + Copy (instantiated as u64 for gaps, i64 for extents)
pub struct BoundedSampler<T> {
    samples: Vec<T>,
    increment: usize,      // Sample every Nth item
    seq: usize,            // Current sequence number
    max_samples: usize,    // Default: 100_000
}

impl<T: Ord + Copy> BoundedSampler<T> {
    pub fn new(max_samples: usize) -> Self;
    
    /// Record a value (may be skipped based on increment)
    pub fn record(&mut self, value: T);
    
    /// Select threshold at given fraction using tippecanoe algorithm:
    /// 1. Sort samples
    /// 2. ix = (len - 1) * (1 - fraction)
    /// 3. Ratchet: increment ix while samples[ix] <= existing
    pub fn select_threshold(&self, fraction: f64, existing: T) -> Option<T>;
    
    /// Clear samples for reuse
    pub fn clear(&mut self);
}

// Type aliases for clarity
pub type GapSampler = BoundedSampler<u64>;
pub type ExtentSampler = BoundedSampler<i64>;
```

```rust
/// Per-zoom adaptive threshold state
/// Extends the CoalesceTargets pattern
pub struct AdaptiveTargets {
    /// Initial thresholds computed from metadata scan
    initial_mingap: HashMap<u8, u64>,
    initial_minextent: HashMap<u8, i64>,
    
    /// Maximum observed thresholds during encoding (thread-safe)
    observed_mingap: DashMap<u8, u64>,
    observed_minextent: DashMap<u8, i64>,
    
    /// Whether any tile at each zoom increased its threshold
    zoom_needs_retry: DashMap<u8, bool>,
}

impl AdaptiveTargets {
    /// Get initial threshold for a zoom level
    pub fn get_mingap(&self, zoom: u8) -> u64;
    pub fn get_minextent(&self, zoom: u8) -> i64;
    
    /// Report a tile's final threshold (called after encoding)
    /// Updates observed max and sets retry flag if threshold increased
    pub fn report_mingap(&self, zoom: u8, threshold: u64);
    pub fn report_minextent(&self, zoom: u8, threshold: i64);
    
    /// Check if zoom level needs re-encoding
    pub fn needs_retry(&self, zoom: u8) -> bool;
    
    /// Propagate thresholds to next zoom level
    pub fn propagate_to_next_zoom(&self, from_zoom: u8);
}
```

```rust
/// Configuration additions to TilerConfig
impl TilerConfig {
    // New fields
    pub max_tile_size: Option<u32>,      // Default: None (disabled)
    pub max_tile_features: Option<u32>,  // Default: None (disabled)
    pub max_samples: usize,              // Default: 100_000
    
    // Multipliers (tippecanoe values)
    pub mingap_fraction: f64,            // Default: 0.80
    pub minextent_fraction: f64,         // Default: 0.75
    
    // Builder methods
    pub fn with_max_tile_size(self, bytes: u32) -> Self;
    pub fn with_max_tile_features(self, count: u32) -> Self;
}
```

### Retry Loop (in encode_tile_from_raw)

```rust
fn encode_tile_with_adaptive_retry(
    tile_data: &RawTileData,
    config: &TilerConfig,
    adaptive: &AdaptiveTargets,
    gap_sampler: &mut GapSampler,
    extent_sampler: &mut ExtentSampler,
) -> Result<EncodedTile, TileError> {
    let mut current_mingap = adaptive.get_mingap(tile_data.z);
    let mut current_minextent = adaptive.get_minextent(tile_data.z);
    
    loop {
        // Clear samplers for this iteration
        gap_sampler.clear();
        extent_sampler.clear();
        
        // Encode tile, collecting samples
        let (encoded, feature_count) = encode_tile_internal(
            tile_data,
            config,
            current_mingap,
            current_minextent,
            gap_sampler,
            extent_sampler,
        )?;
        
        // Check limits
        let size_ok = config.max_tile_size.map_or(true, |max| encoded.len() <= max as usize);
        let count_ok = config.max_tile_features.map_or(true, |max| feature_count <= max as usize);
        
        if size_ok && count_ok {
            // Success - report final thresholds
            adaptive.report_mingap(tile_data.z, current_mingap);
            adaptive.report_minextent(tile_data.z, current_minextent);
            return Ok(encoded);
        }
        
        // Calculate new fraction based on overage
        let target = config.max_tile_features.unwrap_or(u32::MAX) as f64;
        let actual = feature_count as f64;
        
        // Try increasing mingap threshold
        if config.drop_densest_as_needed {
            let new_fraction = config.mingap_fraction * (target / actual) * 0.80;
            let new_fraction = new_fraction.min(0.80);
            
            match gap_sampler.select_threshold(new_fraction, current_mingap) {
                Some(new_threshold) if new_threshold > current_mingap => {
                    current_mingap = new_threshold;
                    continue;
                }
                _ => {}
            }
        }
        
        // Try increasing minextent threshold
        if config.drop_smallest_as_needed {
            let new_fraction = config.minextent_fraction * (target / actual) * 0.75;
            let new_fraction = new_fraction.min(0.80);
            
            match extent_sampler.select_threshold(new_fraction, current_minextent) {
                Some(new_threshold) if new_threshold > current_minextent => {
                    current_minextent = new_threshold;
                    continue;
                }
                _ => {}
            }
        }
        
        // Cannot increase threshold further
        return Err(TileError::CannotReduceFurther {
            tile: TileCoord::new(tile_data.z, tile_data.x, tile_data.y),
            size: encoded.len(),
            features: feature_count,
        });
    }
}
```

### Zoom-Level Retry (in generate_tiles_to_writer_internal)

```rust
// After encoding all tiles at zoom Z
fn process_zoom_with_retry(
    zoom: u8,
    tiles: Vec<RawTileData>,
    config: &TilerConfig,
    adaptive: &AdaptiveTargets,
    writer: &mut StreamingPmtilesWriter,
) -> Result<()> {
    loop {
        // Encode all tiles at this zoom
        let encoded_tiles = encode_zoom_parallel(&tiles, config, adaptive)?;
        
        // Check if any tile increased threshold
        if !adaptive.needs_retry(zoom) {
            // Write tiles and continue
            for tile in encoded_tiles {
                writer.add_tile_with_count(...)?;
            }
            break;
        }
        
        // Reset retry flag, propagate new thresholds, retry
        adaptive.clear_retry_flag(zoom);
        tracing::info!(
            "Zoom {} threshold increased, retrying ({} tiles)",
            zoom, tiles.len()
        );
    }
    
    // Propagate thresholds to next zoom
    adaptive.propagate_to_next_zoom(zoom);
    Ok(())
}
```

## Implementation Waves

### Wave 1: Infrastructure (parallel, ~2 hours)

**Agent A: BoundedSampler**
- File: `crates/core/src/sampling.rs` (NEW)
- Implement `BoundedSampler<T>` with incremental halving
- Implement `select_threshold()` with ratcheting
- Add unit tests for sampling behavior
- Add unit tests for percentile selection

**Agent B: TilerConfig Extensions**
- File: `crates/core/src/pipeline.rs`
- Add `max_tile_size`, `max_tile_features`, `max_samples` fields
- Add `mingap_fraction`, `minextent_fraction` fields
- Add builder methods
- Update `Default` impl

**Agent C: AdaptiveTargets Structure**
- File: `crates/core/src/adaptive.rs` (NEW)
- Implement `AdaptiveTargets` struct
- Implement thread-safe threshold tracking with `DashMap`
- Implement `needs_retry()` and `propagate_to_next_zoom()`
- Add unit tests

### Wave 2: Integration (parallel, ~2 hours)

**Agent D: CLI Flags**
- File: `crates/cli/src/main.rs`
- Add `--max-tile-size` flag (default: None)
- Add `--max-tile-features` flag (default: None)
- Wire flags to `TilerConfig`

**Agent E: Error Types**
- File: `crates/core/src/lib.rs`
- Add `TileError::CannotReduceFurther` variant
- Update error handling in pipeline

**Agent F: Sampling Integration**
- File: `crates/core/src/pipeline.rs`
- Modify `encode_tile_from_raw()` to accept samplers
- Add gap sampling during feature iteration
- Add extent sampling during feature iteration

### Wave 3: Core Logic (sequential, ~3 hours)

**Agent G: Retry Loop**
- File: `crates/core/src/pipeline.rs`
- Implement `encode_tile_with_adaptive_retry()`
- Wire into `flush_batch()` closure
- Add tracing for retry iterations

**Agent H: Zoom-Level Propagation**
- File: `crates/core/src/pipeline.rs`
- Implement `process_zoom_with_retry()`
- Modify pipeline to buffer tiles per zoom (when adaptive enabled)
- Implement zoom-level re-encoding

### Wave 4: Testing & Polish (parallel, ~2 hours)

**Agent I: Integration Tests**
- Add test with dense point cloud that exceeds limits
- Verify tiles fit within limits after adaptive iteration
- Verify threshold propagation across zooms

**Agent J: Documentation**
- Update `ARCHITECTURE.md` with adaptive threshold section
- Add CLI flag documentation
- Document divergences from tippecanoe (if any)

## File Changes Summary

| File | Change |
|------|--------|
| `crates/core/src/lib.rs` | Add `mod sampling; mod adaptive;`, update `Error` |
| `crates/core/src/sampling.rs` | NEW: `BoundedSampler<T>` |
| `crates/core/src/adaptive.rs` | NEW: `AdaptiveTargets` |
| `crates/core/src/pipeline.rs` | Modify `TilerConfig`, `encode_tile_from_raw()`, add retry logic |
| `crates/cli/src/main.rs` | Add CLI flags |
| `context/ARCHITECTURE.md` | Document adaptive threshold algorithm |

## Acceptance Criteria

- [ ] `BoundedSampler<T>` implements incremental halving with 100K cap
- [ ] `select_threshold()` matches tippecanoe's `choose_mingap()`/`choose_minextent()` exactly
- [ ] Thresholds only increase (ratcheting behavior)
- [ ] Per-zoom propagation: if ANY tile increases threshold, entire zoom is re-encoded
- [ ] Thresholds propagate from zoom Z to zoom Z+1
- [ ] When threshold cannot be increased, returns `TileError::CannotReduceFurther`
- [ ] All existing tests pass
- [ ] New integration test verifies adaptive behavior

## Tippecanoe Reference

Key code locations for verification:

| Function | File | Lines | Purpose |
|----------|------|-------|---------|
| `add_sample_to()` | tile.cpp | 1528-1547 | Incremental halving |
| `choose_mingap()` | tile.cpp | 762-778 | Gap percentile selection |
| `choose_minextent()` | tile.cpp | 780-796 | Extent percentile selection |
| Retry loop | tile.cpp | 2616-2860 | Threshold adjustment on overage |
| Zoom propagation | tile.cpp | 3178-3346 | Per-zoom retry and propagation |

## Non-Goals (Explicitly Out of Scope)

- `--drop-fraction-as-needed` / `mindrop_sequence` (different mechanism, separate PR)
- `--extend-zooms-if-still-dropping` (separate PR)
- Predictive threshold initialization from metadata (future enhancement)

## Open Questions

None. All design decisions are finalized.
