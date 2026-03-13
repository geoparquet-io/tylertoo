# Lazy Clipping Implementation Plan

**Author:** Claude (Session abb81969-5794-4212-9bf2-d1ea36fc93e0)  
**Date:** 2026-03-13  
**Status:** Ready for implementation  
**Related Issues:** #123 (lazy clipping), #117 (containment optimization)  
**POC PR:** #129

## Overview

This document provides a complete implementation plan for integrating lazy clipping into the production pipeline. The proof-of-concept in PR #129 validated the architecture. This plan covers the full pipeline refactor needed to realize the memory and CPU savings.

**Expected Benefits:**
- **Memory:** 120GB → 20GB for 10M features × 30 tiles (6x reduction)
- **CPU:** ~80% of building footprints skip clipping (containment optimization)
- **Architecture:** Store geometry once, read N times (vs. N copies)

**Infrastructure Already Implemented:**
- ✅ `GeometryStore`: Disk-backed storage with append/read API (PR #127)
- ✅ `TileRef`: 48-byte lightweight reference (PR #127)
- ✅ `TileBounds::contains()`: Containment check for geo::Rect (PR #129)
- ✅ `WorldClippedGeometry::world_bounds()`: Compute WorldBounds bbox (PR #129)
- ✅ POC tests: 14x memory savings + containment optimization verified (PR #129)

## Phase 1: Refactor Read Phase (pipeline.rs)

**Current behavior (lines 920-1290):**
```rust
// Phase 1: Read → Clip hierarchically → Store N copies
let features = read_geoparquet(&input_path)?;
let tile_features = hierarchical_clip(features, min_zoom, max_zoom)?;
// tile_features contains full TileFeatureRecord (680 bytes each)
```

**New behavior:**
```rust
// Phase 1: Read → Store once → Create TileRefs
let mut geometry_store = GeometryStore::new()?;
let tile_refs = read_and_store(
    &input_path,
    &mut geometry_store,
    min_zoom,
    max_zoom
)?;
// tile_refs contains TileRef (48 bytes each)
```

### Implementation Steps

1. **Add GeometryStore to TilerConfig** (lines ~100-150):
   ```rust
   pub struct TilerConfig {
       // ... existing fields
       pub geometry_store: GeometryStore,
   }
   ```

2. **Replace hierarchical_clip() call** (line ~950):
   ```rust
   // OLD:
   let tile_features = hierarchical_clip(
       &features,
       config.min_zoom,
       config.max_zoom,
       config.buffer,
   )?;

   // NEW:
   let tile_refs = read_and_store_features(
       &features,
       &mut config.geometry_store,
       config.min_zoom,
       config.max_zoom,
   )?;
   ```

3. **Implement read_and_store_features()** (new function, ~200 lines):
   ```rust
   fn read_and_store_features(
       features: &[Feature],
       store: &mut GeometryStore,
       min_zoom: u8,
       max_zoom: u8,
   ) -> Result<Vec<TileRef>> {
       let mut tile_refs = Vec::new();
       
       for (feature_id, feature) in features.iter().enumerate() {
           // 1. Convert to WorldClippedGeometry
           let world_geom = convert_to_world_coords(&feature.geometry)?;
           
           // 2. Serialize to bytes
           let geom_bytes = world_geom.to_bytes()?;
           let props_bytes = serialize_properties(&feature.properties)?;
           
           // 3. Store once
           let handle = store.append(&geom_bytes, &props_bytes)?;
           
           // 4. Determine which tiles this geometry intersects
           let tiles = compute_intersecting_tiles(&feature.bbox, min_zoom, max_zoom)?;
           
           // 5. Create TileRef for each tile
           for tile in tiles {
               let tile_id = hilbert_encode(tile.z, tile.x, tile.y);
               tile_refs.push(TileRef::new(
                   tile_id,
                   tile.z,
                   tile.x,
                   tile.y,
                   feature_id as u64,
                   handle,
               ));
           }
       }
       
       store.flush()?;
       Ok(tile_refs)
   }
   ```

4. **Add compute_intersecting_tiles() helper** (~100 lines):
   ```rust
   fn compute_intersecting_tiles(
       bbox: &geo::Rect,
       min_zoom: u8,
       max_zoom: u8,
   ) -> Result<Vec<TileCoord>> {
       let mut tiles = Vec::new();
       
       for z in min_zoom..=max_zoom {
           // Convert bbox to tile coordinates at this zoom
           let min_tile = lng_lat_to_tile(bbox.min().x, bbox.min().y, z);
           let max_tile = lng_lat_to_tile(bbox.max().x, bbox.max().y, z);
           
           // Add all tiles that intersect the bbox
           for x in min_tile.x..=max_tile.x {
               for y in min_tile.y..=max_tile.y {
                   tiles.push(TileCoord { x, y, z });
               }
           }
       }
       
       Ok(tiles)
   }
   ```

### Files Changed
- `crates/core/src/pipeline.rs`: ~500 lines modified
- `crates/core/src/tile.rs`: Add `lng_lat_to_tile()` helper (~50 lines)

## Phase 2: Adapt External Sorter (pipeline.rs)

**Current behavior (lines ~1300-1400):**
```rust
let sorter = ExternalSorter::new();
for tile_feature in tile_features {
    sorter.push(tile_feature)?;  // TileFeatureRecord
}
let sorted = sorter.sort()?;
```

**New behavior:**
```rust
let sorter = ExternalSorter::new();
for tile_ref in tile_refs {
    sorter.push(tile_ref)?;  // TileRef (already implements Sortable)
}
let sorted = sorter.sort()?;
```

### Implementation Steps

1. **Change type from TileFeatureRecord to TileRef** (line ~1300):
   ```rust
   // OLD:
   let sorted_features: Vec<TileFeatureRecord> = sorter.sort()?;

   // NEW:
   let sorted_refs: Vec<TileRef> = sorter.sort()?;
   ```

2. **Update loop iteration** (line ~1350):
   ```rust
   // OLD:
   for tile_feature in sorted_features {
       // ...
   }

   // NEW:
   for tile_ref in sorted_refs {
       // ...
   }
   ```

### Files Changed
- `crates/core/src/pipeline.rs`: ~50 lines modified

## Phase 3: Refactor Encode Phase (pipeline.rs)

**Current behavior (lines ~1830-1870):**
```rust
// Phase 3: Decode → Encode MVT
for tile_feature in tile_features_for_this_tile {
    let geometry = decode_geometry(&tile_feature.geometry_bytes)?;
    // geometry is already clipped
    encode_mvt_feature(&mut tile, geometry, tile_feature.properties)?;
}
```

**New behavior:**
```rust
// Phase 3: Lookup → Check containment → Conditionally clip → Encode MVT
for tile_ref in tile_refs_for_this_tile {
    // 1. Lookup geometry from store
    let (geom_bytes, props_bytes) = geometry_store.read(tile_ref.geometry_handle)?;
    let world_geom = WorldClippedGeometry::from_bytes(&geom_bytes)?;
    
    // 2. Compute bounds
    let tile_bounds = WorldBounds::from_tile_with_buffer(
        &TileCoord { x: tile_ref.x, y: tile_ref.y, z: tile_ref.z },
        config.buffer,
        4096,
    );
    let geom_bounds = world_geom.world_bounds();
    
    // 3. Check containment (Issue #117 optimization)
    let final_geom = if tile_bounds.contains_bounds(&geom_bounds) {
        // Fully contained → skip clipping
        world_geom
    } else {
        // Must clip
        clip_geometry_to_tile(world_geom, &tile_bounds)?
    };
    
    // 4. Encode MVT
    let properties = deserialize_properties(&props_bytes)?;
    encode_mvt_feature(&mut tile, final_geom, properties)?;
}
```

### Implementation Steps

1. **Add geometry_store parameter to encode functions** (lines ~1800-1850):
   ```rust
   fn encode_tile(
       tile_refs: &[TileRef],
       geometry_store: &GeometryStore,
       config: &TilerConfig,
   ) -> Result<Vec<u8>> {
       // ...
   }
   ```

2. **Implement lazy clipping with containment check** (~150 lines):
   ```rust
   for tile_ref in tile_refs {
       // 1. Lookup
       let (geom_bytes, props_bytes) = geometry_store.read(tile_ref.geometry_handle)?;
       let world_geom = WorldClippedGeometry::from_bytes(&geom_bytes)?;
       
       // 2. Compute bounds
       let tile_coord = TileCoord {
           x: tile_ref.x,
           y: tile_ref.y,
           z: tile_ref.z,
       };
       let tile_bounds = WorldBounds::from_tile_with_buffer(
           &tile_coord,
           config.buffer,
           4096,
       )?;
       let geom_bounds = world_geom.world_bounds();
       
       // 3. Containment check
       let final_geom = if tile_bounds.contains_bounds(&geom_bounds) {
           world_geom
       } else {
           clip_geometry_to_tile(world_geom, &tile_bounds)?
       };
       
       // 4. Encode
       let properties = deserialize_properties(&props_bytes)?;
       encode_mvt_feature(&mut mvt_tile, final_geom, properties)?;
   }
   ```

3. **Add clip_geometry_to_tile() helper** (~100 lines):
   ```rust
   fn clip_geometry_to_tile(
       geom: WorldClippedGeometry,
       tile_bounds: &WorldBounds,
   ) -> Result<WorldClippedGeometry> {
       // Convert WorldBounds to clipping polygon
       let clip_poly = tile_bounds.to_polygon();
       
       // Clip based on geometry type
       match geom {
           WorldClippedGeometry::Point(p) => {
               // Points don't need clipping (already checked containment)
               Ok(WorldClippedGeometry::Point(p))
           }
           WorldClippedGeometry::LineString(coords) => {
               // Use wagyu to clip linestring
               let clipped = clip_linestring(&coords, &clip_poly)?;
               Ok(WorldClippedGeometry::LineString(clipped))
           }
           WorldClippedGeometry::Polygon(rings) => {
               // Use wagyu to clip polygon
               let clipped = clip_polygon(&rings, &clip_poly)?;
               Ok(WorldClippedGeometry::Polygon(clipped))
           }
           // ... other geometry types
       }
   }
   ```

### Files Changed
- `crates/core/src/pipeline.rs`: ~300 lines modified
- `crates/core/src/world_coord.rs`: Add `WorldBounds::to_polygon()` (~50 lines)

## Phase 4: Parallel Access Pattern

**Problem:** Phase 3 encodes tiles in parallel (rayon), but GeometryStore currently uses a single file handle.

**Solution:** Add `new_reader()` method to create independent readers for concurrent access.

### Implementation Steps

1. **Add new_reader() to GeometryStore** (geometry_store.rs, ~50 lines):
   ```rust
   impl GeometryStore {
       pub fn new_reader(&self) -> Result<GeometryStoreReader> {
           let file = File::open(&self.path)?;
           Ok(GeometryStoreReader {
               file: BufReader::new(file),
           })
       }
   }

   pub struct GeometryStoreReader {
       file: BufReader<File>,
   }

   impl GeometryStoreReader {
       pub fn read(&mut self, handle: GeometryHandle) -> Result<(Vec<u8>, Vec<u8>)> {
           // Same logic as GeometryStore::read() but on independent file handle
           self.file.seek(SeekFrom::Start(handle.offset))?;
           // ... read wkb_bytes and props_bytes
           Ok((wkb_bytes, props_bytes))
       }
   }
   ```

2. **Update Phase 3 parallel encoding** (pipeline.rs, ~20 lines):
   ```rust
   // Create readers for parallel access
   let readers: Vec<_> = (0..rayon::current_num_threads())
       .map(|_| geometry_store.new_reader())
       .collect::<Result<_>>()?;
   
   // Parallel encode with thread-local readers
   tiles.par_iter().enumerate().try_for_each(|(idx, tile_refs)| {
       let reader = &mut readers[idx % readers.len()];
       encode_tile(tile_refs, reader, config)
   })?;
   ```

### Files Changed
- `crates/core/src/geometry_store.rs`: ~70 lines added
- `crates/core/src/pipeline.rs`: ~20 lines modified

## Integration Tests

Add comprehensive tests to verify identical output between old and new pipeline.

### Test Structure (~200 lines)

```rust
#[test]
fn test_lazy_clipping_produces_identical_tiles() {
    // 1. Run old pipeline (hierarchical clipping)
    let old_output = run_old_pipeline(&test_input)?;
    
    // 2. Run new pipeline (lazy clipping)
    let new_output = run_new_pipeline(&test_input)?;
    
    // 3. Compare tile-by-tile
    assert_eq!(old_output.tiles.len(), new_output.tiles.len());
    for (tile_id, old_tile) in &old_output.tiles {
        let new_tile = &new_output.tiles[tile_id];
        
        // Compare MVT bytes (should be identical)
        assert_eq!(old_tile.mvt_bytes, new_tile.mvt_bytes);
        
        // Compare feature counts
        assert_eq!(old_tile.feature_count, new_tile.feature_count);
    }
}

#[test]
fn test_containment_optimization_skips_clipping() {
    // Create geometry that's fully contained in tile
    let contained_geom = create_contained_polygon();
    
    // Track whether clipping was invoked
    let clip_count = Arc::new(AtomicUsize::new(0));
    
    // Run pipeline with instrumented clipping
    let output = run_pipeline_with_clip_tracking(&contained_geom, clip_count.clone())?;
    
    // Verify clipping was skipped
    assert_eq!(clip_count.load(Ordering::SeqCst), 0);
    
    // Verify tile still encoded correctly
    assert_eq!(output.tiles.len(), 1);
}

#[test]
fn test_memory_usage_reduction() {
    // Test with 10K features × 30 tiles = 300K records
    let input = create_test_dataset(10_000, 30);
    
    // Measure old pipeline memory
    let old_memory = measure_memory(|| {
        run_old_pipeline(&input)
    })?;
    
    // Measure new pipeline memory
    let new_memory = measure_memory(|| {
        run_new_pipeline(&input)
    })?;
    
    // Verify 5-6x reduction
    assert!(new_memory < old_memory / 5);
}
```

### Files Changed
- `crates/core/tests/lazy_clipping_integration.rs`: ~200 lines added

## Benchmarks

Add benchmarks to measure improvements in memory and CPU.

### Benchmark Structure (~100 lines)

```rust
fn bench_hierarchical_vs_lazy(c: &mut Criterion) {
    let input = create_test_dataset(1000, 10);
    
    c.bench_function("hierarchical_clipping", |b| {
        b.iter(|| run_old_pipeline(&input))
    });
    
    c.bench_function("lazy_clipping", |b| {
        b.iter(|| run_new_pipeline(&input))
    });
}

fn bench_containment_optimization(c: &mut Criterion) {
    // 80% contained, 20% need clipping (typical building footprints)
    let input = create_mixed_dataset(1000, 0.8);
    
    c.bench_function("with_containment_check", |b| {
        b.iter(|| run_new_pipeline(&input))
    });
    
    c.bench_function("without_containment_check", |b| {
        b.iter(|| run_new_pipeline_no_optimization(&input))
    });
}
```

### Files Changed
- `crates/core/benches/lazy_clipping.rs`: ~100 lines added

## Verification Checklist

Before merging:

- [ ] All existing tests pass
- [ ] New integration tests verify identical tile output
- [ ] Benchmarks show expected memory reduction (5-6x)
- [ ] Benchmarks show CPU savings from containment optimization (~80% skip clipping)
- [ ] Parallel encoding works correctly with GeometryStoreReader
- [ ] GeometryStore properly cleans up temporary files
- [ ] PMTiles output is byte-identical to old pipeline (for non-optimized case)
- [ ] Clippy passes with no new warnings
- [ ] rustfmt passes
- [ ] Documentation updated in ARCHITECTURE.md

## Estimated File Changes

| File | Lines Added | Lines Modified | Lines Removed |
|------|-------------|----------------|---------------|
| `crates/core/src/pipeline.rs` | 500 | 300 | 200 |
| `crates/core/src/geometry_store.rs` | 70 | 0 | 0 |
| `crates/core/src/world_coord.rs` | 50 | 0 | 0 |
| `crates/core/src/tile.rs` | 50 | 0 | 0 |
| `crates/core/tests/lazy_clipping_integration.rs` | 200 | 0 | 0 |
| `crates/core/benches/lazy_clipping.rs` | 100 | 0 | 0 |
| **Total** | **970** | **300** | **200** |

## Implementation Order

1. Phase 1: Read phase refactor (~500 lines, most complex)
2. Phase 2: Sorter adaptation (~50 lines, straightforward)
3. Phase 3: Encode phase refactor (~300 lines, moderate complexity)
4. Phase 4: Parallel access (~70 lines, low risk)
5. Integration tests (~200 lines, verify correctness)
6. Benchmarks (~100 lines, measure improvements)

**Total estimated effort:** 6-8 hours for experienced Rust developer familiar with codebase.

## Notes for Next Agent

- The POC in PR #129 proves the architecture works
- All infrastructure is ready (GeometryStore, TileRef, contains(), world_bounds())
- The main challenge is the Phase 1 refactor (replacing hierarchical_clip)
- Use TDD: Write integration tests first, then implement
- Verify tile output is byte-identical to old pipeline before optimizing
- The containment optimization (#117) is the "easy win" after lazy clipping works

Good luck! 🚀
