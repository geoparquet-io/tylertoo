# Drop Smallest As Needed Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement `--drop-smallest-as-needed` flag to drop features below a pixel-area threshold, matching tippecanoe's behavior.

**Architecture:** Add pixel area calculation for all geometry types (polygons use Shoelace formula, lines use circle-area heuristic, points get area=1). Filter features post-clip in the tile encoding pipeline. Start with fixed threshold (4 sq px), optionally add tippecanoe's iterative threshold adjustment later.

**Tech Stack:** Rust (geo-types, WorldCoord arithmetic), TDD with cargo test

---

## Phase 1: Core Infrastructure (Pixel Area Calculation)

### Task 1: Add WorldCoord polygon pixel area calculation

**Files:**
- Modify: `crates/core/src/feature_drop.rs` (add new function after `world_ring_area`)
- Test: `crates/core/src/feature_drop.rs` (in existing `#[cfg(test)]` section)

**Step 1: Write the failing test**

Add to the test module in `feature_drop.rs` (around line 1140):

```rust
#[test]
fn test_polygon_pixel_area_world() {
    // 1x1 degree square at equator, z0, 256px extent
    // Should be ~256px wide (1/360 of world width)
    let tile = TileCoord { x: 0, y: 0, z: 0 };
    let extent = 256;
    
    // Square from 0,0 to 1,1 degrees
    let exterior = vec![
        WorldCoord::from_lng_lat(0.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 1.0),
        WorldCoord::from_lng_lat(0.0, 1.0),
        WorldCoord::from_lng_lat(0.0, 0.0),
    ];
    
    let pixel_area = polygon_pixel_area_world(&exterior, &[], &tile, extent);
    
    // 1 degree at z0 = 1/360 of 2^32 world coords
    // Projected to 256px extent = (256/360)^2 ≈ 0.506 sq px
    assert!((pixel_area - 0.506).abs() < 0.1, "Expected ~0.506, got {}", pixel_area);
}

#[test]
fn test_polygon_pixel_area_world_with_hole() {
    let tile = TileCoord { x: 0, y: 0, z: 0 };
    let extent = 256;
    
    // Outer ring: 2x2 degrees
    let exterior = vec![
        WorldCoord::from_lng_lat(0.0, 0.0),
        WorldCoord::from_lng_lat(2.0, 0.0),
        WorldCoord::from_lng_lat(2.0, 2.0),
        WorldCoord::from_lng_lat(0.0, 2.0),
        WorldCoord::from_lng_lat(0.0, 0.0),
    ];
    
    // Inner ring (hole): 1x1 degrees centered
    let hole = vec![
        WorldCoord::from_lng_lat(0.5, 0.5),
        WorldCoord::from_lng_lat(1.5, 0.5),
        WorldCoord::from_lng_lat(1.5, 1.5),
        WorldCoord::from_lng_lat(0.5, 1.5),
        WorldCoord::from_lng_lat(0.5, 0.5),
    ];
    
    let area_without_hole = polygon_pixel_area_world(&exterior, &[], &tile, extent);
    let area_with_hole = polygon_pixel_area_world(&exterior, &[hole], &tile, extent);
    
    // Hole should reduce area by ~25% (1/4 of outer ring)
    assert!(area_with_hole < area_without_hole);
    assert!((area_without_hole - area_with_hole) / area_without_hole > 0.2);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --package gpq-tiles-core test_polygon_pixel_area_world -- --nocapture`

Expected: FAIL with "cannot find function `polygon_pixel_area_world`"

**Step 3: Write minimal implementation**

Add after `world_ring_area` function (around line 690):

```rust
/// Calculate polygon pixel area in WorldCoord space.
///
/// Converts world-coordinate area to square pixels for the given tile and extent.
/// Uses Shoelace formula via `world_ring_area`, subtracts holes.
///
/// # Arguments
/// * `exterior` - Outer ring in WorldCoord
/// * `interiors` - Hole rings in WorldCoord
/// * `tile` - Tile coordinate for scale calculation
/// * `extent` - Tile extent in pixels (typically 4096)
///
/// # Returns
/// Area in square pixels (can be fractional)
pub fn polygon_pixel_area_world(
    exterior: &[WorldCoord],
    interiors: &[Vec<WorldCoord>],
    tile: &TileCoord,
    extent: u32,
) -> f64 {
    // Calculate outer ring area (signed, doubled)
    let outer_area = world_ring_area(exterior).unsigned_abs();
    
    // Subtract holes
    let holes_area: u64 = interiors
        .iter()
        .map(|ring| world_ring_area(ring).unsigned_abs())
        .sum();
    
    let net_area = outer_area.saturating_sub(holes_area);
    
    // Convert from world coords to square pixels
    // World coord range is 0..2^32, tile size is extent px
    // At zoom z, tile covers (2^32 / 2^z) world units
    // So 1 world unit = (extent / (2^32 / 2^z)) = extent * 2^z / 2^32 pixels
    // Area scaling: (pixels/world_unit)^2 = (extent * 2^z / 2^32)^2
    
    let world_units_per_tile = (1u64 << 32) >> tile.z;
    let pixels_per_world_unit = (extent as f64) / (world_units_per_tile as f64);
    
    // net_area is 2x actual area from Shoelace, so divide by 2
    let world_area = (net_area as f64) / 2.0;
    
    world_area * pixels_per_world_unit * pixels_per_world_unit
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test --package gpq-tiles-core test_polygon_pixel_area_world -- --nocapture`

Expected: PASS

**Step 5: Commit**

```bash
git add crates/core/src/feature_drop.rs
git commit -m "feat(core): add polygon_pixel_area_world for area-based filtering"
```

---

### Task 2: Add WorldCoord linestring pixel area calculation

**Files:**
- Modify: `crates/core/src/feature_drop.rs` (add after `polygon_pixel_area_world`)
- Test: `crates/core/src/feature_drop.rs`

**Step 1: Write the failing test**

Add to test module:

```rust
#[test]
fn test_linestring_pixel_area_world() {
    // Horizontal line 1 degree long at equator, z0, 256px extent
    let tile = TileCoord { x: 0, y: 0, z: 0 };
    let extent = 256;
    
    let coords = vec![
        WorldCoord::from_lng_lat(0.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 0.0),
    ];
    
    let pixel_area = linestring_pixel_area_world(&coords, &tile, extent);
    
    // 1 degree at z0 ≈ 0.711 pixels
    // Tippecanoe: area = pi * (length/2)^2 = pi * (0.711/2)^2 ≈ 0.397
    assert!((pixel_area - 0.397).abs() < 0.1, "Expected ~0.397, got {}", pixel_area);
}

#[test]
fn test_linestring_pixel_area_world_multipart() {
    let tile = TileCoord { x: 0, y: 0, z: 0 };
    let extent = 256;
    
    // L-shaped line: 2 segments
    let coords = vec![
        WorldCoord::from_lng_lat(0.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 1.0),
    ];
    
    let pixel_area = linestring_pixel_area_world(&coords, &tile, extent);
    
    // Total length ≈ 1.414 degrees ≈ 1.006 pixels
    // Area = pi * (1.006/2)^2 ≈ 0.794
    assert!((pixel_area - 0.794).abs() < 0.2, "Expected ~0.794, got {}", pixel_area);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --package gpq-tiles-core test_linestring_pixel_area_world -- --nocapture`

Expected: FAIL with "cannot find function `linestring_pixel_area_world`"

**Step 3: Write minimal implementation**

Add after `polygon_pixel_area_world`:

```rust
/// Calculate linestring "pixel area" using tippecanoe's circle heuristic.
///
/// Tippecanoe treats lines as having area = π × (length/2)² (area of a circle
/// whose diameter equals the line length). This makes lines comparable to polygons.
///
/// # Arguments
/// * `coords` - Linestring vertices in WorldCoord
/// * `tile` - Tile coordinate for scale calculation
/// * `extent` - Tile extent in pixels
///
/// # Returns
/// Pseudo-area in square pixels
pub fn linestring_pixel_area_world(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
) -> f64 {
    if coords.len() < 2 {
        return 0.0;
    }
    
    // Calculate total length in world coords
    let world_length = world_linestring_length(coords);
    
    // Convert to pixels (same scaling as polygon area)
    let world_units_per_tile = (1u64 << 32) >> tile.z;
    let pixels_per_world_unit = (extent as f64) / (world_units_per_tile as f64);
    let pixel_length = (world_length as f64) * pixels_per_world_unit;
    
    // Tippecanoe: area = π × (length/2)²
    std::f64::consts::PI * (pixel_length / 2.0).powi(2)
}
```

**Step 4: Run test to verify it passes**

Run: `cargo test --package gpq-tiles-core test_linestring_pixel_area_world -- --nocapture`

Expected: PASS

**Step 5: Commit**

```bash
git add crates/core/src/feature_drop.rs
git commit -m "feat(core): add linestring_pixel_area_world using circle heuristic"
```

---

### Task 3: Add dispatcher for all geometry types

**Files:**
- Modify: `crates/core/src/feature_drop.rs` (add after linestring function)
- Modify: `crates/core/src/geometry.rs` (check WorldClippedGeometry enum)
- Test: `crates/core/src/feature_drop.rs`

**Step 1: Write the failing test**

Add to test module:

```rust
#[test]
fn test_geometry_pixel_area_world_all_types() {
    use crate::geometry::WorldClippedGeometry;
    
    let tile = TileCoord { x: 0, y: 0, z: 0 };
    let extent = 256;
    
    // Point: should return 1.0
    let point = WorldClippedGeometry::Point(WorldCoord::from_lng_lat(0.0, 0.0));
    assert_eq!(geometry_pixel_area_world(&point, &tile, extent), 1.0);
    
    // MultiPoint: should return 1.0 per point
    let multipoint = WorldClippedGeometry::MultiPoint(vec![
        WorldCoord::from_lng_lat(0.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 1.0),
    ]);
    assert_eq!(geometry_pixel_area_world(&multipoint, &tile, extent), 2.0);
    
    // LineString: should use circle heuristic
    let linestring = WorldClippedGeometry::LineString(vec![
        WorldCoord::from_lng_lat(0.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 0.0),
    ]);
    let line_area = geometry_pixel_area_world(&linestring, &tile, extent);
    assert!(line_area > 0.0 && line_area < 1.0);
    
    // Polygon: should use Shoelace
    let exterior = vec![
        WorldCoord::from_lng_lat(0.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 1.0),
        WorldCoord::from_lng_lat(0.0, 1.0),
        WorldCoord::from_lng_lat(0.0, 0.0),
    ];
    let polygon = WorldClippedGeometry::Polygon { exterior, interiors: vec![] };
    let poly_area = geometry_pixel_area_world(&polygon, &tile, extent);
    assert!(poly_area > 0.0);
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --package gpq-tiles-core test_geometry_pixel_area_world_all_types -- --nocapture`

Expected: FAIL with "cannot find function `geometry_pixel_area_world`"

**Step 3: Check WorldClippedGeometry enum structure**

Run: `cargo doc --package gpq-tiles-core --open` and search for `WorldClippedGeometry`, or:

```rust
// Check crates/core/src/geometry.rs for the enum definition
// Expected variants: Point, MultiPoint, LineString, MultiLineString, Polygon, MultiPolygon
```

**Step 4: Write minimal implementation**

Add after `linestring_pixel_area_world`:

```rust
/// Calculate pixel area for any geometry type in WorldCoord space.
///
/// Dispatches to type-specific functions:
/// - Points/MultiPoints: 1.0 per point
/// - LineStrings: circle heuristic (π × (length/2)²)
/// - Polygons: Shoelace formula minus holes
///
/// # Arguments
/// * `geom` - Clipped geometry in WorldCoord
/// * `tile` - Tile coordinate
/// * `extent` - Tile extent in pixels
///
/// # Returns
/// Area in square pixels
pub fn geometry_pixel_area_world(
    geom: &WorldClippedGeometry,
    tile: &TileCoord,
    extent: u32,
) -> f64 {
    use crate::geometry::WorldClippedGeometry;
    
    match geom {
        WorldClippedGeometry::Point(_) => 1.0,
        
        WorldClippedGeometry::MultiPoint(points) => points.len() as f64,
        
        WorldClippedGeometry::LineString(coords) => {
            linestring_pixel_area_world(coords, tile, extent)
        }
        
        WorldClippedGeometry::MultiLineString(lines) => {
            lines.iter()
                .map(|line| linestring_pixel_area_world(line, tile, extent))
                .sum()
        }
        
        WorldClippedGeometry::Polygon { exterior, interiors } => {
            polygon_pixel_area_world(exterior, interiors, tile, extent)
        }
        
        WorldClippedGeometry::MultiPolygon(polygons) => {
            polygons.iter()
                .map(|(ext, ints)| polygon_pixel_area_world(ext, ints, tile, extent))
                .sum()
        }
    }
}
```

**Step 5: Run test to verify it passes**

Run: `cargo test --package gpq-tiles-core test_geometry_pixel_area_world_all_types -- --nocapture`

Expected: PASS

**Step 6: Commit**

```bash
git add crates/core/src/feature_drop.rs
git commit -m "feat(core): add geometry_pixel_area_world dispatcher for all types"
```

---

## Phase 2: Configuration and CLI

### Task 4: Add TilerConfig field and builder

**Files:**
- Modify: `crates/core/src/pipeline.rs` (TilerConfig struct and impl)

**Step 1: Find TilerConfig struct**

Run: `cargo doc --package gpq-tiles-core --open` or read `crates/core/src/pipeline.rs` around line 195

**Step 2: Add field to TilerConfig**

In `pipeline.rs`, add field to `TilerConfig` struct (after `gamma` field, around line 207):

```rust
/// Enable size-based feature dropping (tippecanoe --drop-smallest-as-needed).
///
/// When enabled, features with pixel area below a threshold are dropped first
/// when a tile has too many features. Threshold is auto-computed per tile.
pub drop_smallest_as_needed: bool,

/// Minimum pixel area threshold for --drop-smallest-as-needed.
///
/// Features smaller than this are candidates for dropping. Default: 4.0 square pixels.
/// Only used when drop_smallest_as_needed = true.
pub drop_smallest_threshold: f64,
```

**Step 3: Update Default impl**

In the `Default` impl for `TilerConfig` (around line 230), add:

```rust
drop_smallest_as_needed: false,
drop_smallest_threshold: 4.0,
```

**Step 4: Add builder methods**

After the `with_drop_densest_as_needed()` method (around line 310), add:

```rust
/// Enable dropping of smallest features when tiles are dense.
///
/// Equivalent to tippecanoe's --drop-smallest-as-needed.
pub fn with_drop_smallest_as_needed(mut self) -> Self {
    self.drop_smallest_as_needed = true;
    self
}

/// Set the minimum pixel area threshold for smallest-feature dropping.
///
/// Only used when drop_smallest_as_needed = true.
pub fn with_drop_smallest_threshold(mut self, threshold: f64) -> Self {
    self.drop_smallest_threshold = threshold;
    self
}
```

**Step 5: Verify it compiles**

Run: `cargo build --package gpq-tiles-core`

Expected: SUCCESS

**Step 6: Commit**

```bash
git add crates/core/src/pipeline.rs
git commit -m "feat(core): add drop_smallest_as_needed to TilerConfig"
```

---

### Task 5: Add CLI arguments

**Files:**
- Modify: `crates/cli/src/main.rs` (Args struct and config wiring)

**Step 1: Add fields to Args struct**

In `main.rs`, add after `drop_densest_as_needed` field (around line 65):

```rust
/// Enable size-based feature dropping (tippecanoe parity).
///
/// When a tile has more features than can be rendered clearly,
/// drops the smallest features (by pixel area) first.
/// Equivalent to tippecanoe's --drop-smallest-as-needed.
#[arg(long)]
drop_smallest_as_needed: bool,

/// Minimum pixel area for --drop-smallest-as-needed (default: 4.0).
///
/// Features with pixel area below this threshold are candidates for dropping.
#[arg(long, default_value = "4.0")]
drop_smallest_threshold: f64,
```

**Step 2: Wire to TilerConfig**

In the config building section (around line 265, after gamma wiring), add:

```rust
if args.drop_smallest_as_needed {
    tiler_config = tiler_config
        .with_drop_smallest_as_needed()
        .with_drop_smallest_threshold(args.drop_smallest_threshold);
}
```

**Step 3: Test CLI parsing**

Run: `cargo run --package gpq-tiles -- --help | grep -A 3 "drop-smallest"`

Expected: Help text for both flags displayed

**Step 4: Commit**

```bash
git add crates/cli/src/main.rs
git commit -m "feat(cli): add --drop-smallest-as-needed flags"
```

---

### Task 6: Add Python bindings

**Files:**
- Modify: `crates/python/src/lib.rs` (convert function signature and config building)

**Step 1: Add parameters to convert function**

In `lib.rs`, add to `convert()` function signature (around line 135, after `gamma` parameter):

```rust
/// Enable size-based feature dropping (tippecanoe --drop-smallest-as-needed).
///
/// Features smaller than `drop_smallest_threshold` square pixels are dropped
/// when tiles are dense. Default: False.
#[pyo3(signature = (..., drop_smallest_as_needed = false, ...))]
drop_smallest_as_needed: bool,

/// Minimum pixel area threshold for smallest-feature dropping.
///
/// Only used when drop_smallest_as_needed = True. Default: 4.0.
#[pyo3(signature = (..., drop_smallest_threshold = 4.0, ...))]
drop_smallest_threshold: f64,
```

**Step 2: Wire to config builder**

In the config building section (after gamma wiring, around line 180):

```rust
if drop_smallest_as_needed {
    config = config
        .with_drop_smallest_as_needed()
        .with_drop_smallest_threshold(drop_smallest_threshold);
}
```

**Step 3: Verify Python module builds**

Run: `cd crates/python && uv sync && uv run maturin develop`

Expected: SUCCESS

**Step 4: Test Python help**

Run: `cd crates/python && uv run python -c "import gpq_tiles; help(gpq_tiles.convert)" | grep drop_smallest`

Expected: Parameter documentation displayed

**Step 5: Commit**

```bash
git add crates/python/src/lib.rs
git commit -m "feat(python): add drop_smallest_as_needed parameters"
```

---

## Phase 3: Pipeline Integration (Simplified Fixed Threshold)

### Task 7: Implement filtering in encode_tile_from_raw (production path)

**Files:**
- Modify: `crates/core/src/pipeline.rs` (encode_tile_from_raw function)
- Test: `crates/core/src/pipeline.rs` (test module)

**Step 1: Write the failing integration test**

Add to test module at end of `pipeline.rs` (around line 3200):

```rust
#[test]
fn test_drop_smallest_filters_tiny_features() {
    use crate::test_utils::create_test_parquet;
    use std::io::Cursor;
    
    // Create a test file with mixed-size polygons at different locations
    let features = vec![
        // Large polygon (should be kept)
        r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0.0,0.0],[0.1,0.0],[0.1,0.1],[0.0,0.1],[0.0,0.0]]]},"properties":{"name":"large"}}"#,
        // Tiny polygon (should be dropped)
        r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0.2,0.2],[0.201,0.2],[0.201,0.201],[0.2,0.201],[0.2,0.2]]]},"properties":{"name":"tiny"}}"#,
        // Medium polygon (should be kept)
        r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[0.3,0.3],[0.35,0.3],[0.35,0.35],[0.3,0.35],[0.3,0.3]]]},"properties":{"name":"medium"}}"#,
    ];
    
    let parquet_data = create_test_parquet(&features);
    let mut output = Cursor::new(Vec::new());
    
    let config = TilerConfig::default()
        .with_min_zoom(8)
        .with_max_zoom(8)
        .with_drop_smallest_as_needed()
        .with_drop_smallest_threshold(4.0);
    
    crate::convert(Cursor::new(parquet_data), &mut output, config).unwrap();
    
    // Verify tiny feature was dropped
    let pmtiles_data = output.into_inner();
    let reader = pmtiles::MmapPMTilesReader::new(pmtiles_data);
    
    // Check z8 tile containing the features
    let tile_data = reader.get_tile(TileCoord { z: 8, x: 128, y: 128 }).unwrap();
    let tile = crate::mvt::decode_tile(&tile_data).unwrap();
    
    // Should have 2 features (large + medium), tiny should be dropped
    assert_eq!(tile.layers[0].features.len(), 2);
    
    // Verify the dropped feature is the tiny one
    let names: Vec<_> = tile.layers[0].features.iter()
        .filter_map(|f| f.properties.get("name"))
        .collect();
    assert!(names.contains(&"large"));
    assert!(names.contains(&"medium"));
    assert!(!names.contains(&"tiny"));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --package gpq-tiles-core test_drop_smallest_filters_tiny_features -- --nocapture`

Expected: FAIL with assertion error (all 3 features present)

**Step 3: Implement filtering in encode_tile_from_raw**

Find the per-feature loop in `encode_tile_from_raw` (around line 1400). Add filtering logic after the degenerate check:

```rust
// Check for drop-smallest filtering
if config.drop_smallest_as_needed {
    let pixel_area = geometry_pixel_area_world(&validated, &tile, config.extent);
    if pixel_area < config.drop_smallest_threshold {
        // Drop this feature - too small to render clearly
        continue;
    }
}
```

**Step 4: Add import at top of file**

```rust
use crate::feature_drop::geometry_pixel_area_world;
```

**Step 5: Run test to verify it passes**

Run: `cargo test --package gpq-tiles-core test_drop_smallest_filters_tiny_features -- --nocapture`

Expected: PASS

**Step 6: Commit**

```bash
git add crates/core/src/pipeline.rs
git commit -m "feat(core): implement drop-smallest filtering in encode_tile_from_raw"
```

---

### Task 8: Implement filtering in streaming path

**Files:**
- Modify: `crates/core/src/pipeline.rs` (generate_tiles_streaming_with_stats function)

**Step 1: Find the streaming feature loop**

In `generate_tiles_streaming_with_stats`, locate the per-tile feature processing loop (around line 1990).

**Step 2: Add filtering logic**

After the existing density/gap dropping checks, add:

```rust
// Drop smallest features if enabled
if config.drop_smallest_as_needed {
    let pixel_area = geometry_pixel_area_world(&validated, &tile, config.extent);
    if pixel_area < config.drop_smallest_threshold {
        continue;
    }
}
```

**Step 3: Verify it compiles**

Run: `cargo build --package gpq-tiles-core`

Expected: SUCCESS

**Step 4: Commit**

```bash
git add crates/core/src/pipeline.rs
git commit -m "feat(core): add drop-smallest filtering to streaming path"
```

---

### Task 9: Implement filtering in legacy TileIterator path

**Files:**
- Modify: `crates/core/src/pipeline.rs` (process_tile_static function)

**Step 1: Find the TileIterator feature loop**

In `process_tile_static`, locate the per-tile loop (around line 2130).

**Step 2: Add filtering logic**

After the existing density/gap dropping checks, add:

```rust
// Drop smallest features if enabled
if config.drop_smallest_as_needed {
    let pixel_area = geometry_pixel_area_world(&validated, &tile, config.extent);
    if pixel_area < config.drop_smallest_threshold {
        continue;
    }
}
```

**Step 3: Verify all tests pass**

Run: `cargo test --package gpq-tiles-core`

Expected: All tests PASS

**Step 4: Commit**

```bash
git add crates/core/src/pipeline.rs
git commit -m "feat(core): add drop-smallest filtering to legacy TileIterator path"
```

---

## Phase 4: Testing and Documentation

### Task 10: Add comprehensive unit tests

**Files:**
- Modify: `crates/core/src/feature_drop.rs` (test module)

**Step 1: Add edge case tests**

Add to test module:

```rust
#[test]
fn test_polygon_pixel_area_world_zero_area() {
    // Degenerate polygon (collapsed to line)
    let tile = TileCoord { x: 0, y: 0, z: 0 };
    let extent = 256;
    
    let exterior = vec![
        WorldCoord::from_lng_lat(0.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 0.0),
        WorldCoord::from_lng_lat(1.0, 0.0),  // duplicate
        WorldCoord::from_lng_lat(0.0, 0.0),
    ];
    
    let pixel_area = polygon_pixel_area_world(&exterior, &[], &tile, extent);
    assert_eq!(pixel_area, 0.0);
}

#[test]
fn test_linestring_pixel_area_world_single_point() {
    // Degenerate line (single point)
    let tile = TileCoord { x: 0, y: 0, z: 0 };
    let extent = 256;
    
    let coords = vec![WorldCoord::from_lng_lat(0.0, 0.0)];
    let pixel_area = linestring_pixel_area_world(&coords, &tile, extent);
    assert_eq!(pixel_area, 0.0);
}

#[test]
fn test_geometry_pixel_area_world_zoom_scaling() {
    use crate::geometry::WorldClippedGeometry;
    
    // Same polygon at different zooms should have different pixel areas
    let extent = 256;
    let exterior = vec![
        WorldCoord::from_lng_lat(0.0, 0.0),
        WorldCoord::from_lng_lat(0.1, 0.0),
        WorldCoord::from_lng_lat(0.1, 0.1),
        WorldCoord::from_lng_lat(0.0, 0.1),
        WorldCoord::from_lng_lat(0.0, 0.0),
    ];
    let polygon = WorldClippedGeometry::Polygon { exterior, interiors: vec![] };
    
    let area_z0 = geometry_pixel_area_world(&polygon, &TileCoord { x: 0, y: 0, z: 0 }, extent);
    let area_z8 = geometry_pixel_area_world(&polygon, &TileCoord { x: 0, y: 0, z: 8 }, extent);
    
    // At z8, pixel area should be 2^16 times larger (2^8 per dimension)
    let expected_ratio = (1u64 << 16) as f64;
    let actual_ratio = area_z8 / area_z0;
    assert!((actual_ratio - expected_ratio).abs() / expected_ratio < 0.01);
}
```

**Step 2: Run tests**

Run: `cargo test --package gpq-tiles-core feature_drop -- --nocapture`

Expected: All tests PASS

**Step 3: Commit**

```bash
git add crates/core/src/feature_drop.rs
git commit -m "test(core): add edge case tests for pixel area calculation"
```

---

### Task 11: Add golden test for visual comparison

**Files:**
- Modify: `crates/core/src/golden.rs`

**Step 1: Write failing golden test**

Add after `test_density_dropping_reduces_z8_feature_count` (around line 450):

```rust
#[test]
fn test_drop_smallest_visual_comparison() {
    use crate::test_utils::create_test_parquet;
    use std::io::Cursor;
    
    // Create test data with features of various sizes
    let features = create_mixed_size_features();
    let parquet_data = create_test_parquet(&features);
    
    // Convert WITHOUT drop-smallest
    let mut output_baseline = Cursor::new(Vec::new());
    let config_baseline = TilerConfig::default()
        .with_min_zoom(10)
        .with_max_zoom(10);
    crate::convert(Cursor::new(parquet_data.clone()), &mut output_baseline, config_baseline).unwrap();
    
    // Convert WITH drop-smallest
    let mut output_filtered = Cursor::new(Vec::new());
    let config_filtered = TilerConfig::default()
        .with_min_zoom(10)
        .with_max_zoom(10)
        .with_drop_smallest_as_needed()
        .with_drop_smallest_threshold(4.0);
    crate::convert(Cursor::new(parquet_data), &mut output_filtered, config_filtered).unwrap();
    
    // Compare feature counts
    let baseline_tiles = extract_tile_stats(&output_baseline.into_inner());
    let filtered_tiles = extract_tile_stats(&output_filtered.into_inner());
    
    // Filtered should have fewer features
    assert!(filtered_tiles.total_features < baseline_tiles.total_features);
    
    // Document the reduction
    println!("Baseline: {} features", baseline_tiles.total_features);
    println!("Filtered: {} features", filtered_tiles.total_features);
    println!("Reduction: {:.1}%", 
        100.0 * (1.0 - filtered_tiles.total_features as f64 / baseline_tiles.total_features as f64));
}

fn create_mixed_size_features() -> Vec<String> {
    // Generate 100 features: 20 large, 30 medium, 50 tiny
    let mut features = Vec::new();
    
    // Large polygons (0.01 degree squares)
    for i in 0..20 {
        let x = (i % 5) as f64 * 0.02;
        let y = (i / 5) as f64 * 0.02;
        features.push(format!(
            r#"{{"type":"Feature","geometry":{{"type":"Polygon","coordinates":[[[{},{x}],[{},{}],[{},{}],[{},{}],[{},{}]]]}},"properties":{{"size":"large"}}}}"#,
            x, y, x + 0.01, y, x + 0.01, y + 0.01, x, y + 0.01, x, y
        ));
    }
    
    // Medium polygons (0.003 degree squares)
    for i in 0..30 {
        let x = (i % 6) as f64 * 0.01 + 0.1;
        let y = (i / 6) as f64 * 0.01;
        features.push(format!(
            r#"{{"type":"Feature","geometry":{{"type":"Polygon","coordinates":[[[{},{x}],[{},{}],[{},{}],[{},{}],[{},{}]]]}},"properties":{{"size":"medium"}}}}"#,
            x, y, x + 0.003, y, x + 0.003, y + 0.003, x, y + 0.003, x, y
        ));
    }
    
    // Tiny polygons (0.0005 degree squares - should be dropped)
    for i in 0..50 {
        let x = (i % 10) as f64 * 0.005 + 0.2;
        let y = (i / 10) as f64 * 0.005;
        features.push(format!(
            r#"{{"type":"Feature","geometry":{{"type":"Polygon","coordinates":[[[{},{x}],[{},{}],[{},{}],[{},{}],[{},{}]]]}},"properties":{{"size":"tiny"}}}}"#,
            x, y, x + 0.0005, y, x + 0.0005, y + 0.0005, x, y + 0.0005, x, y
        ));
    }
    
    features
}
```

**Step 2: Run test to verify behavior**

Run: `cargo test --package gpq-tiles-core test_drop_smallest_visual_comparison -- --nocapture`

Expected: PASS with reduction percentage printed

**Step 3: Commit**

```bash
git add crates/core/src/golden.rs
git commit -m "test(core): add golden test for drop-smallest visual comparison"
```

---

### Task 12: Update ARCHITECTURE.md documentation

**Files:**
- Modify: `context/ARCHITECTURE.md`

**Step 1: Add section after density dropping docs**

Find the "Density-Based Feature Dropping" section (around line 40) and add after it:

```markdown
### Size-Based Feature Dropping (`--drop-smallest-as-needed`)

**Flag:** `--drop-smallest-as-needed`

**Reference:** Tippecanoe's `--drop-smallest-as-needed`

**Algorithm:** Drop features with pixel area below a threshold when tiles are dense.

#### Pixel Area Calculation

We implement tippecanoe's area calculation methods for all geometry types:

| Geometry Type | Area Calculation | Implementation |
|---------------|------------------|----------------|
| Polygon | Shoelace formula (sum exterior - sum holes) | `polygon_pixel_area_world()` |
| LineString | π × (length/2)² (circle with line as diameter) | `linestring_pixel_area_world()` |
| Point/MultiPoint | 1.0 per point (constant) | Direct return |

All areas are converted from world coordinates to square pixels using:
```
pixels_per_world_unit = extent / (2^32 / 2^z)
pixel_area = world_area × pixels_per_world_unit²
```

#### Filtering Logic

**Phase:** Post-clip (features are clipped to tile bounds BEFORE area filtering)

**Location in pipeline:**
- `encode_tile_from_raw()` - Production path (external sort)
- `generate_tiles_streaming_with_stats()` - Streaming path
- `process_tile_static()` - Legacy TileIterator path

**Default threshold:** 4.0 square pixels

Features with `pixel_area < threshold` are dropped from the tile.

#### Divergences from Tippecanoe

1. **Fixed threshold (v1):** We start with a fixed threshold per zoom level. Tippecanoe uses iterative threshold adjustment (percentile-based) when tiles exceed size limits.

2. **Point area:** Tippecanoe calculates point area from Hilbert curve gaps (spatial distribution). We use constant area=1.0 per point for simplicity.

3. **Pre-computed areas:** Tippecanoe pre-computes polygon/line areas at serialization time (on unclipped geometry). We compute on-demand during tile encoding (on clipped geometry). This may give slightly different results for features that span tile boundaries.

**Future work:** Implement tippecanoe's iterative threshold adjustment for better tile size control.

#### Testing

- `test_polygon_pixel_area_world` - Shoelace formula correctness
- `test_linestring_pixel_area_world` - Circle heuristic correctness
- `test_geometry_pixel_area_world_all_types` - Dispatcher correctness
- `test_drop_smallest_filters_tiny_features` - Integration test
- `test_drop_smallest_visual_comparison` - Golden test with reduction metrics
```

**Step 2: Verify markdown renders correctly**

Run: `cargo doc --package gpq-tiles-core --open` and check if ARCHITECTURE.md is linked

**Step 3: Commit**

```bash
git add context/ARCHITECTURE.md
git commit -m "docs: document drop-smallest-as-needed algorithm and divergences"
```

---

### Task 13: Add CLI example to README

**Files:**
- Modify: `README.md`

**Step 1: Find the CLI usage section**

Read `README.md` to find the example commands section.

**Step 2: Add example command**

Add after the density dropping example:

```markdown
### Size-Based Feature Dropping

Drop the smallest features first when tiles are dense (tippecanoe parity):

```bash
gpq-tiles input.parquet output.pmtiles \
  --drop-smallest-as-needed \
  --drop-smallest-threshold 4.0  # square pixels (default)
```

Useful for:
- Building footprints (drop tiny sheds/outbuildings at high zoom)
- Dense point data (drop smallest markers)
- Polygon layers (drop single-pixel features)
```

**Step 3: Commit**

```bash
git add README.md
git commit -m "docs: add drop-smallest-as-needed CLI example"
```

---

### Task 14: Run full test suite and verify

**Files:**
- N/A (verification step)

**Step 1: Run all core tests**

Run: `cargo test --package gpq-tiles-core`

Expected: All tests PASS

**Step 2: Run CLI tests**

Run: `cargo test --package gpq-tiles`

Expected: All tests PASS

**Step 3: Test Python bindings**

Run: `cd crates/python && uv run pytest`

Expected: All tests PASS (or existing failures only)

**Step 4: Run formatting check**

Run: `cargo fmt --all --check`

Expected: No formatting issues

**Step 5: Build release binary**

Run: `cargo build --release`

Expected: SUCCESS

**Step 6: Test CLI end-to-end**

Create a test file and run:

```bash
# Assuming you have a test.parquet file
cargo run --release --package gpq-tiles -- test.parquet output.pmtiles \
  --min-zoom 8 --max-zoom 12 \
  --drop-smallest-as-needed \
  --drop-smallest-threshold 4.0
```

Expected: PMTiles file generated without errors

**Step 7: Final commit**

```bash
git add -A
git commit -m "test: verify all tests pass for drop-smallest-as-needed"
```

---

## Phase 5: Future Enhancements (Optional)

### Task 15: Implement tippecanoe's iterative threshold adjustment (OPTIONAL)

**Status:** DEFERRED - The fixed threshold implementation provides most of the value. Iterative adjustment is complex and should be implemented only if needed for better tile size control.

**Reference:** See tippecanoe agent research output for the full iterative algorithm (percentile selection, cross-tile propagation, retry logic).

**Estimated scope:** 500+ additional lines (threshold selector, tile retry loop, cross-tile state management)

---

## Completion Checklist

Before creating the PR:

- [ ] All tests pass (`cargo test`)
- [ ] Code is formatted (`cargo fmt --all`)
- [ ] Clippy is happy (`cargo clippy --all-targets --all-features`)
- [ ] Python bindings build (`cd crates/python && uv run maturin develop`)
- [ ] ARCHITECTURE.md documents algorithm and divergences
- [ ] README.md has CLI example
- [ ] At least one golden test shows reduction metrics
- [ ] Git log follows conventional commits (all commits start with `feat:`, `test:`, or `docs:`)

## PR Description Template

```markdown
## Summary

Implements `--drop-smallest-as-needed` flag for size-based feature dropping (tippecanoe parity).

Closes #119

## Implementation

- **Pixel area calculation:** Shoelace for polygons, circle heuristic (π × (length/2)²) for lines, constant for points
- **Filtering phase:** Post-clip (in all 3 pipeline paths)
- **Default threshold:** 4.0 square pixels
- **Configuration:** CLI flag, Python parameter, TilerConfig builder

## Testing

- Unit tests for all geometry types
- Integration test verifies tiny features are dropped
- Golden test documents reduction percentage

## Divergences from Tippecanoe

1. **Fixed threshold:** We use a fixed per-zoom threshold. Tippecanoe uses iterative percentile-based adjustment.
2. **Point area:** We use constant area=1.0. Tippecanoe uses Hilbert curve gaps.
3. **Area computation timing:** We compute on clipped geometry. Tippecanoe pre-computes on unclipped.

See `context/ARCHITECTURE.md` for details.

## Example Usage

```bash
gpq-tiles buildings.parquet tiles.pmtiles \
  --drop-smallest-as-needed \
  --drop-smallest-threshold 4.0
```

## Future Work

- Implement tippecanoe's iterative threshold adjustment for better tile size control
- Add Hilbert-based point area calculation for spatial distribution awareness
```

---

## Execution Notes

**Estimated total time:** 3-4 hours (assuming TDD workflow with small commits)

**Key dependencies:**
- WorldCoord arithmetic (existing)
- world_ring_area, world_linestring_length (existing)
- WorldClippedGeometry enum (existing)
- TilerConfig builder pattern (existing)

**Risk areas:**
- Pixel area scaling math (verify with unit tests at different zooms)
- Integration with existing density dropping (make sure they don't conflict)
- Python bindings signature (test with `uv run maturin develop`)
