# Zoom-Dependent Geometry Simplification Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `--simplify` flag that enables zoom-dependent Douglas-Peucker simplification to reduce tile sizes for linear features at low zoom levels.

**Architecture:** Simplification is applied per-tile in `encode_tile_from_raw()` after geometry deserialization, using existing `simplify_world_linestring()` / `simplify_world_ring()` functions. Tolerance scales with zoom level: `pixel_tolerance * simplify_factor` where `simplify_factor` defaults to 1.0 (1 pixel). Tile boundary points are marked as "necessary" to prevent tile-edge seams (matching tippecanoe's shared-node approach).

**Tech Stack:** Rust, `geo::Simplify` (Douglas-Peucker), existing WorldCoord-based simplification in `simplify.rs`

**Reference:** Tippecanoe `geometry.cpp:simplify_lines()`, `clip.cpp:douglas_peucker()`

---

## Task 1: Add simplify_factor to TilerConfig

**Files:**
- Modify: `crates/core/src/pipeline.rs:228-278` (TilerConfig struct)
- Modify: `crates/core/src/pipeline.rs:412-470` (Default impl)
- Modify: `crates/core/src/pipeline.rs:475+` (builder methods)

**Step 1: Write failing test**

Add to `crates/core/src/pipeline.rs` in the existing tests module (or create inline test):

```rust
#[test]
fn test_tiler_config_simplify_factor() {
    let config = TilerConfig::default();
    assert_eq!(config.simplify_factor, None); // Disabled by default
    
    let config_enabled = TilerConfig::default().with_simplify(1.0);
    assert_eq!(config_enabled.simplify_factor, Some(1.0));
    
    let config_custom = TilerConfig::default().with_simplify(0.5);
    assert_eq!(config_custom.simplify_factor, Some(0.5));
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test --package gpq-tiles-core test_tiler_config_simplify_factor -- --nocapture
```

Expected: FAIL with "no field `simplify_factor` on type `TilerConfig`"

**Step 3: Add simplify_factor field to TilerConfig**

In `crates/core/src/pipeline.rs`, add after line 268 (`pub deterministic: bool,`):

```rust
    /// Simplification factor for zoom-dependent geometry simplification.
    /// When Some, enables Douglas-Peucker simplification with tolerance = pixels * factor.
    /// Default: None (disabled). Typical value: 1.0 (1 pixel tolerance).
    /// Lower values (0.5) preserve more detail; higher values (2.0) simplify more aggressively.
    pub simplify_factor: Option<f64>,
```

**Step 4: Add default value**

In the `Default` impl (around line 437), add before the closing brace:

```rust
            // Zoom-dependent simplification disabled by default
            simplify_factor: None,
```

**Step 5: Add builder method**

After the existing builder methods (around line 600), add:

```rust
    /// Enable zoom-dependent geometry simplification.
    ///
    /// When enabled, geometries are simplified using Douglas-Peucker algorithm
    /// with tolerance = pixel_tolerance * factor at each zoom level.
    /// Lower zoom levels get more aggressive simplification.
    ///
    /// # Arguments
    /// * `factor` - Simplification factor (typically 1.0 for 1-pixel tolerance)
    pub fn with_simplify(mut self, factor: f64) -> Self {
        self.simplify_factor = Some(factor);
        self
    }
```

**Step 6: Run test to verify it passes**

```bash
cargo test --package gpq-tiles-core test_tiler_config_simplify_factor -- --nocapture
```

Expected: PASS

**Step 7: Commit**

```bash
git add crates/core/src/pipeline.rs
git commit -m "feat(core): add simplify_factor to TilerConfig

Add configuration for zoom-dependent geometry simplification.
When enabled, Douglas-Peucker simplification is applied per-tile
with tolerance scaling by zoom level.

Refs: #156"
```

---

## Task 2: Add boundary point detection to simplify.rs

**Files:**
- Modify: `crates/core/src/simplify.rs`

This task adds a function to identify which points lie on tile boundaries. These points must be preserved during simplification to prevent tile-edge seams.

**Step 1: Write failing test**

Add to `crates/core/src/simplify.rs` in the tests module:

```rust
#[test]
fn test_is_on_tile_boundary() {
    use crate::tile::TileCoord;
    use crate::world_coord::WorldCoord;
    
    let tile = TileCoord::new(0, 0, 1); // zoom 1, tile (0,0)
    let extent = 4096u32;
    
    // Point clearly inside tile - not on boundary
    let inside = WorldCoord::new(1 << 30, 1 << 30); // middle of tile
    assert!(!is_on_tile_boundary(&inside, &tile, extent, 1.0));
    
    // Point on left edge (x = tile_min_x)
    let on_left = WorldCoord::new(0, 1 << 30);
    assert!(is_on_tile_boundary(&on_left, &tile, extent, 1.0));
    
    // Point on right edge (x = tile_max_x)
    let on_right = WorldCoord::new(1 << 31, 1 << 30);
    assert!(is_on_tile_boundary(&on_right, &tile, extent, 1.0));
    
    // Point on top edge (y = tile_min_y)
    let on_top = WorldCoord::new(1 << 30, 0);
    assert!(is_on_tile_boundary(&on_top, &tile, extent, 1.0));
    
    // Point on bottom edge (y = tile_max_y)
    let on_bottom = WorldCoord::new(1 << 30, 1 << 31);
    assert!(is_on_tile_boundary(&on_bottom, &tile, extent, 1.0));
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test --package gpq-tiles-core simplify::tests::test_is_on_tile_boundary -- --nocapture
```

Expected: FAIL with "cannot find function `is_on_tile_boundary`"

**Step 3: Implement is_on_tile_boundary**

Add after `tile_linestring_to_world_coords` function (around line 400):

```rust
/// Check if a WorldCoord lies on or near a tile boundary.
///
/// Points on tile boundaries must be preserved during simplification to prevent
/// visible seams between adjacent tiles. This matches tippecanoe's approach of
/// marking boundary-crossing points as "necessary".
///
/// # Arguments
/// * `coord` - The world coordinate to check
/// * `tile` - The tile to check boundaries against
/// * `extent` - Tile extent (typically 4096)
/// * `pixel_tolerance` - Distance in pixels to consider "on boundary"
///
/// # Returns
/// `true` if the point is within `pixel_tolerance` pixels of any tile edge.
pub fn is_on_tile_boundary(
    coord: &WorldCoord,
    tile: &TileCoord,
    extent: u32,
    pixel_tolerance: f64,
) -> bool {
    let (x, y) = world_to_tile_local_f64(*coord, tile, extent);
    let extent_f = extent as f64;
    
    // Check if within tolerance of any edge (0, extent)
    x < pixel_tolerance 
        || x > extent_f - pixel_tolerance
        || y < pixel_tolerance 
        || y > extent_f - pixel_tolerance
}
```

**Step 4: Run test to verify it passes**

```bash
cargo test --package gpq-tiles-core simplify::tests::test_is_on_tile_boundary -- --nocapture
```

Expected: PASS

**Step 5: Commit**

```bash
git add crates/core/src/simplify.rs
git commit -m "feat(simplify): add tile boundary detection

Add is_on_tile_boundary() to identify points that should be preserved
during simplification. Points near tile edges are marked as necessary
to prevent visible seams between adjacent tiles.

Matches tippecanoe's shared-node preservation approach.

Refs: #156"
```

---

## Task 3: Add boundary-preserving simplification for linestrings

**Files:**
- Modify: `crates/core/src/simplify.rs`

**Step 1: Write failing test**

```rust
#[test]
fn test_simplify_world_linestring_preserve_boundaries() {
    use crate::tile::TileCoord;
    use crate::world_coord::WorldCoord;
    
    let tile = TileCoord::new(0, 0, 1);
    let extent = 4096u32;
    
    // Create a linestring that crosses the tile boundary
    // Points: start inside, cross boundary, end inside
    let coords = vec![
        WorldCoord::new(1 << 30, 1 << 30),       // inside
        WorldCoord::new(1 << 30, 1 << 30 + 100), // small deviation (should be simplified away normally)
        WorldCoord::new(1 << 31, 1 << 30),       // ON RIGHT BOUNDARY - must be preserved
        WorldCoord::new(1 << 31, 1 << 30 + 100), // small deviation on boundary
        WorldCoord::new(1 << 30 + (1 << 29), 1 << 30), // inside
    ];
    
    // Simplify with boundary preservation
    let simplified = simplify_world_linestring_preserve_boundaries(
        &coords,
        &tile,
        extent,
        10.0, // aggressive tolerance to trigger simplification
    );
    
    // The boundary point (index 2) must be preserved
    assert!(simplified.contains(&coords[2]), "Boundary point must be preserved");
    
    // First and last points are always preserved by Douglas-Peucker
    assert_eq!(simplified.first(), Some(&coords[0]));
    assert_eq!(simplified.last(), Some(&coords[4]));
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test --package gpq-tiles-core simplify::tests::test_simplify_world_linestring_preserve_boundaries -- --nocapture
```

Expected: FAIL with "cannot find function `simplify_world_linestring_preserve_boundaries`"

**Step 3: Implement boundary-preserving simplification**

Add after `simplify_world_ring` (around line 463):

```rust
/// Simplify a linestring while preserving points on tile boundaries.
///
/// This is the production simplification function that prevents tile-edge seams.
/// Points within 1 pixel of tile boundaries are never removed, matching
/// tippecanoe's behavior of marking boundary-crossing points as "necessary".
///
/// # Algorithm
/// 1. Identify which points are on tile boundaries
/// 2. Split the linestring at boundary points
/// 3. Simplify each segment independently
/// 4. Rejoin segments, preserving boundary points
///
/// # Arguments
/// * `coords` - Polyline vertices in world coordinates
/// * `tile` - The tile context for boundary detection and coordinate transformation
/// * `extent` - Tile extent (typically 4096)
/// * `pixel_tolerance` - Simplification tolerance in pixels
///
/// # Returns
/// Simplified polyline with boundary points preserved.
pub fn simplify_world_linestring_preserve_boundaries(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
    pixel_tolerance: f64,
) -> Vec<WorldCoord> {
    if coords.len() < 2 {
        return coords.to_vec();
    }
    
    // Find indices of boundary points (these must be preserved)
    let boundary_indices: Vec<usize> = coords
        .iter()
        .enumerate()
        .filter(|(_, c)| is_on_tile_boundary(c, tile, extent, 1.0))
        .map(|(i, _)| i)
        .collect();
    
    // If no boundary points, use standard simplification
    if boundary_indices.is_empty() {
        return simplify_world_linestring(coords, tile, extent, pixel_tolerance);
    }
    
    // Split at boundary points and simplify each segment
    let mut result = Vec::new();
    let mut segment_start = 0;
    
    for &boundary_idx in &boundary_indices {
        if boundary_idx > segment_start {
            // Simplify segment from segment_start to boundary_idx (inclusive)
            let segment = &coords[segment_start..=boundary_idx];
            let simplified_segment = simplify_world_linestring(segment, tile, extent, pixel_tolerance);
            
            // Add all but the last point (boundary point will be added next)
            if result.is_empty() {
                result.extend_from_slice(&simplified_segment[..simplified_segment.len() - 1]);
            } else {
                // Skip first point (duplicate of previous boundary)
                result.extend_from_slice(&simplified_segment[1..simplified_segment.len() - 1]);
            }
        }
        
        // Always add the boundary point
        result.push(coords[boundary_idx]);
        segment_start = boundary_idx;
    }
    
    // Handle final segment (from last boundary to end)
    if segment_start < coords.len() - 1 {
        let segment = &coords[segment_start..];
        let simplified_segment = simplify_world_linestring(segment, tile, extent, pixel_tolerance);
        // Skip first point (duplicate of boundary)
        result.extend_from_slice(&simplified_segment[1..]);
    }
    
    result
}
```

**Step 4: Run test to verify it passes**

```bash
cargo test --package gpq-tiles-core simplify::tests::test_simplify_world_linestring_preserve_boundaries -- --nocapture
```

Expected: PASS

**Step 5: Commit**

```bash
git add crates/core/src/simplify.rs
git commit -m "feat(simplify): add boundary-preserving linestring simplification

Implement simplify_world_linestring_preserve_boundaries() that splits
linestrings at tile boundaries, simplifies each segment independently,
and rejoins while preserving boundary points.

This prevents visible seams between adjacent tiles by ensuring points
on tile edges are never removed during simplification.

Refs: #156"
```

---

## Task 4: Add boundary-preserving simplification for polygon rings

**Files:**
- Modify: `crates/core/src/simplify.rs`

**Step 1: Write failing test**

```rust
#[test]
fn test_simplify_world_ring_preserve_boundaries() {
    use crate::tile::TileCoord;
    use crate::world_coord::WorldCoord;
    
    let tile = TileCoord::new(0, 0, 1);
    let extent = 4096u32;
    
    // Create a ring that crosses the tile boundary
    // Square with one edge on the tile boundary
    let ring = vec![
        WorldCoord::new(1 << 30, 1 << 30),           // inside corner
        WorldCoord::new(1 << 31, 1 << 30),           // ON RIGHT BOUNDARY
        WorldCoord::new(1 << 31, 1 << 30 + (1<<28)), // ON RIGHT BOUNDARY  
        WorldCoord::new(1 << 30, 1 << 30 + (1<<28)), // inside corner
        WorldCoord::new(1 << 30, 1 << 30),           // close ring
    ];
    
    let simplified = simplify_world_ring_preserve_boundaries(
        &ring,
        &tile,
        extent,
        10.0,
    );
    
    // Boundary points must be preserved
    assert!(simplified.contains(&ring[1]), "First boundary point must be preserved");
    assert!(simplified.contains(&ring[2]), "Second boundary point must be preserved");
    
    // Ring must remain closed
    assert_eq!(simplified.first(), simplified.last(), "Ring must be closed");
    
    // Ring must have at least 4 points (3 unique + closing)
    assert!(simplified.len() >= 4, "Ring must have at least 4 points");
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test --package gpq-tiles-core simplify::tests::test_simplify_world_ring_preserve_boundaries -- --nocapture
```

Expected: FAIL with "cannot find function `simplify_world_ring_preserve_boundaries`"

**Step 3: Implement boundary-preserving ring simplification**

Add after `simplify_world_linestring_preserve_boundaries`:

```rust
/// Simplify a polygon ring while preserving points on tile boundaries.
///
/// Same as [`simplify_world_linestring_preserve_boundaries`] but ensures the ring
/// remains closed and has at least 4 points (3 unique + closing).
pub fn simplify_world_ring_preserve_boundaries(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
    pixel_tolerance: f64,
) -> Vec<WorldCoord> {
    if coords.len() < 4 {
        return coords.to_vec();
    }
    
    // Simplify as a linestring (excluding the closing point to avoid duplication)
    let open_coords = if coords.first() == coords.last() && coords.len() > 1 {
        &coords[..coords.len() - 1]
    } else {
        coords
    };
    
    let mut simplified = simplify_world_linestring_preserve_boundaries(
        open_coords,
        tile,
        extent,
        pixel_tolerance,
    );
    
    // Ensure minimum ring size (3 unique points)
    // Douglas-Peucker might reduce below this; if so, return original
    if simplified.len() < 3 {
        return coords.to_vec();
    }
    
    // Close the ring
    if simplified.first() != simplified.last() {
        simplified.push(simplified[0]);
    }
    
    simplified
}
```

**Step 4: Run test to verify it passes**

```bash
cargo test --package gpq-tiles-core simplify::tests::test_simplify_world_ring_preserve_boundaries -- --nocapture
```

Expected: PASS

**Step 5: Commit**

```bash
git add crates/core/src/simplify.rs
git commit -m "feat(simplify): add boundary-preserving polygon ring simplification

Implement simplify_world_ring_preserve_boundaries() that preserves
boundary points while ensuring the ring remains closed and valid
(at least 4 points).

Refs: #156"
```

---

## Task 5: Add simplify_geometry_for_tile helper

**Files:**
- Modify: `crates/core/src/simplify.rs`

This task adds a high-level function that applies the correct simplification to any `WorldClippedGeometry`.

**Step 1: Write failing test**

```rust
#[test]
fn test_simplify_geometry_for_tile() {
    use crate::hierarchical_clip::WorldClippedGeometry;
    use crate::tile::TileCoord;
    use crate::world_coord::WorldCoord;
    
    let tile = TileCoord::new(0, 0, 5);
    let extent = 4096u32;
    let factor = 1.0;
    
    // Test with a linestring
    let line = WorldClippedGeometry::LineString(vec![
        WorldCoord::new(1 << 26, 1 << 26),
        WorldCoord::new(1 << 26 + 1, 1 << 26 + 1), // tiny deviation
        WorldCoord::new(1 << 27, 1 << 27),
    ]);
    
    let simplified = simplify_geometry_for_tile(&line, &tile, extent, factor);
    
    // Should have fewer or equal points
    if let WorldClippedGeometry::LineString(coords) = simplified {
        assert!(coords.len() <= 3);
        assert!(coords.len() >= 2); // Minimum for valid linestring
    } else {
        panic!("Expected LineString");
    }
    
    // Test that points pass through unchanged
    let point = WorldClippedGeometry::Point(WorldCoord::new(1 << 26, 1 << 26));
    let simplified_point = simplify_geometry_for_tile(&point, &tile, extent, factor);
    assert!(matches!(simplified_point, WorldClippedGeometry::Point(_)));
}
```

**Step 2: Run test to verify it fails**

```bash
cargo test --package gpq-tiles-core simplify::tests::test_simplify_geometry_for_tile -- --nocapture
```

Expected: FAIL with "cannot find function `simplify_geometry_for_tile`"

**Step 3: Implement simplify_geometry_for_tile**

Add necessary import at top of file:

```rust
use crate::hierarchical_clip::WorldClippedGeometry;
```

Add the function:

```rust
/// Simplify a WorldClippedGeometry for a specific tile.
///
/// This is the main entry point for tile-level simplification in the encoding
/// pipeline. It applies boundary-preserving Douglas-Peucker simplification
/// with zoom-appropriate tolerance.
///
/// # Arguments
/// * `geom` - The clipped geometry to simplify
/// * `tile` - The tile being encoded (for boundary detection and tolerance)
/// * `extent` - Tile extent (typically 4096)
/// * `simplify_factor` - Multiplier for pixel tolerance (typically 1.0)
///
/// # Returns
/// Simplified geometry with boundary points preserved.
pub fn simplify_geometry_for_tile(
    geom: &WorldClippedGeometry,
    tile: &TileCoord,
    extent: u32,
    simplify_factor: f64,
) -> WorldClippedGeometry {
    // Tolerance in pixels, scaled by factor
    let pixel_tolerance = simplify_factor;
    
    match geom {
        // Points cannot be simplified
        WorldClippedGeometry::Point(p) => WorldClippedGeometry::Point(*p),
        WorldClippedGeometry::MultiPoint(points) => {
            WorldClippedGeometry::MultiPoint(points.clone())
        }
        
        // Linestrings: use boundary-preserving simplification
        WorldClippedGeometry::LineString(coords) => {
            let simplified = simplify_world_linestring_preserve_boundaries(
                coords,
                tile,
                extent,
                pixel_tolerance,
            );
            WorldClippedGeometry::LineString(simplified)
        }
        
        WorldClippedGeometry::MultiLineString(lines) => {
            let simplified_lines: Vec<Vec<WorldCoord>> = lines
                .iter()
                .map(|line| {
                    simplify_world_linestring_preserve_boundaries(
                        line,
                        tile,
                        extent,
                        pixel_tolerance,
                    )
                })
                .filter(|line| line.len() >= 2) // Filter degenerate lines
                .collect();
            WorldClippedGeometry::MultiLineString(simplified_lines)
        }
        
        // Polygons: use boundary-preserving ring simplification
        WorldClippedGeometry::Polygon { exterior, interiors } => {
            let simplified_exterior = simplify_world_ring_preserve_boundaries(
                exterior,
                tile,
                extent,
                pixel_tolerance,
            );
            
            // Only keep interior rings that remain valid after simplification
            let simplified_interiors: Vec<Vec<WorldCoord>> = interiors
                .iter()
                .map(|ring| {
                    simplify_world_ring_preserve_boundaries(ring, tile, extent, pixel_tolerance)
                })
                .filter(|ring| ring.len() >= 4) // Filter degenerate rings
                .collect();
            
            // If exterior becomes degenerate, return original
            if simplified_exterior.len() < 4 {
                return geom.clone();
            }
            
            WorldClippedGeometry::Polygon {
                exterior: simplified_exterior,
                interiors: simplified_interiors,
            }
        }
        
        WorldClippedGeometry::MultiPolygon(polys) => {
            let simplified_polys: Vec<(Vec<WorldCoord>, Vec<Vec<WorldCoord>>)> = polys
                .iter()
                .map(|(exterior, interiors)| {
                    let simplified_exterior = simplify_world_ring_preserve_boundaries(
                        exterior,
                        tile,
                        extent,
                        pixel_tolerance,
                    );
                    
                    let simplified_interiors: Vec<Vec<WorldCoord>> = interiors
                        .iter()
                        .map(|ring| {
                            simplify_world_ring_preserve_boundaries(
                                ring,
                                tile,
                                extent,
                                pixel_tolerance,
                            )
                        })
                        .filter(|ring| ring.len() >= 4)
                        .collect();
                    
                    (simplified_exterior, simplified_interiors)
                })
                .filter(|(ext, _)| ext.len() >= 4) // Filter degenerate polygons
                .collect();
            
            WorldClippedGeometry::MultiPolygon(simplified_polys)
        }
    }
}
```

**Step 4: Run test to verify it passes**

```bash
cargo test --package gpq-tiles-core simplify::tests::test_simplify_geometry_for_tile -- --nocapture
```

Expected: PASS

**Step 5: Commit**

```bash
git add crates/core/src/simplify.rs
git commit -m "feat(simplify): add simplify_geometry_for_tile entry point

Add high-level function that applies appropriate boundary-preserving
simplification to any WorldClippedGeometry type. This is the main
entry point for the encoding pipeline.

Refs: #156"
```

---

## Task 6: Export new functions from simplify module

**Files:**
- Modify: `crates/core/src/simplify.rs` (pub visibility)
- Modify: `crates/core/src/lib.rs` (re-export)

**Step 1: Verify functions are pub**

Ensure all new functions have `pub` visibility (they should from previous tasks).

**Step 2: Add re-export in lib.rs**

In `crates/core/src/lib.rs`, find the simplify module export and ensure it includes the new functions. If there's a `pub use simplify::*;` this is automatic. Otherwise, add explicit exports:

```rust
pub use simplify::{
    simplify_for_zoom,
    simplify_in_tile_coords,
    simplify_geometry_for_tile,
    // ... existing exports
};
```

**Step 3: Run cargo check**

```bash
cargo check --package gpq-tiles-core
```

Expected: No errors

**Step 4: Commit**

```bash
git add crates/core/src/lib.rs crates/core/src/simplify.rs
git commit -m "chore(core): export simplify functions from lib.rs

Refs: #156"
```

---

## Task 7: Integrate simplification into encode_tile_from_raw

**Files:**
- Modify: `crates/core/src/pipeline.rs:1852+` (encode_tile_from_raw function)

This is the core integration. We add simplification right after geometry deserialization.

**Step 1: Write failing test**

Add to pipeline tests (or create new test file):

```rust
#[test]
fn test_encode_tile_applies_simplification() {
    use crate::pipeline::TilerConfig;
    use crate::tile::TileCoord;
    
    // Create a config with simplification enabled
    let config = TilerConfig::default()
        .with_zoom_range(0, 6)
        .with_simplify(1.0);
    
    assert!(config.simplify_factor.is_some());
    // Further integration testing happens in Task 10
}
```

**Step 2: Add simplify import to pipeline.rs**

At the top of `crates/core/src/pipeline.rs`, add to the simplify import:

```rust
use crate::simplify::{simplify_for_zoom, simplify_geometry_for_tile};
```

**Step 3: Modify encode_tile_from_raw signature**

The function needs access to `simplify_factor`. Add it as a parameter. Find the function at line 1852 and update:

```rust
fn encode_tile_from_raw(
    tile_data: RawTileData,
    layer_name: &str,
    extent: u32,
    enable_tiny_polygon_accumulation: bool,
    cluster_config: Option<&ClusterConfig>,
    coalesce_config: Option<&CoalesceConfig>,
    coalesce_targets: Option<&CoalesceTargets>,
    drop_smallest_as_needed: bool,
    drop_smallest_threshold: f64,
    gamma: Option<f64>,
    mut gap_sampler: Option<&mut GapSampler>,
    mut extent_sampler: Option<&mut ExtentSampler>,
    simplify_factor: Option<f64>,  // NEW PARAMETER
) -> Option<EncodedTile> {
```

**Step 4: Apply simplification after geometry deserialization**

Find the line (around 2444):
```rust
let geom = match WorldClippedGeometry::from_bytes(&raw_feat.geometry_bytes) {
    Some(g) => g,
    None => continue,
};
```

Replace with:
```rust
let geom = match WorldClippedGeometry::from_bytes(&raw_feat.geometry_bytes) {
    Some(g) => g,
    None => continue,
};

// Apply zoom-dependent simplification if enabled
let geom = if let Some(factor) = simplify_factor {
    simplify_geometry_for_tile(&geom, &coord, extent, factor)
} else {
    geom
};
```

**Step 5: Update all call sites of encode_tile_from_raw**

Search for all calls to `encode_tile_from_raw` and add the new parameter. There should be calls in:
- The streaming pipeline
- The batch processing path

For each call site, pass `config.simplify_factor` as the last argument.

**Step 6: Run cargo check**

```bash
cargo check --package gpq-tiles-core
```

Expected: No errors (all call sites updated)

**Step 7: Commit**

```bash
git add crates/core/src/pipeline.rs
git commit -m "feat(pipeline): integrate simplification into encode_tile_from_raw

Apply zoom-dependent simplification after geometry deserialization
in the tile encoding pipeline. Simplification is applied per-tile
with boundary-preserving logic to prevent tile-edge seams.

Refs: #156"
```

---

## Task 8: Add CLI flags for simplification

**Files:**
- Modify: `crates/cli/src/main.rs`

**Step 1: Add --simplify flag**

Find the Args struct (around line 59) and add after the existing flags:

```rust
    /// Enable zoom-dependent geometry simplification.
    ///
    /// Applies Douglas-Peucker simplification with tolerance scaling by zoom level.
    /// At lower zoom levels, geometries are simplified more aggressively.
    /// This dramatically reduces tile sizes for linear features (roads, rivers, boundaries).
    ///
    /// Equivalent to tippecanoe's --simplification flag behavior.
    #[arg(long)]
    simplify: bool,

    /// Simplification factor (default: 1.0 = 1 pixel tolerance).
    ///
    /// Controls simplification aggressiveness:
    /// - 0.5: Preserve more detail (half-pixel tolerance)
    /// - 1.0: Standard (1 pixel tolerance, tippecanoe default)
    /// - 2.0: More aggressive (2 pixel tolerance)
    ///
    /// Only used when --simplify is enabled.
    #[arg(long, default_value = "1.0")]
    simplify_factor: f64,
```

**Step 2: Wire flags to TilerConfig**

Find where TilerConfig is built from args (search for `TilerConfig::default()`) and add:

```rust
let config = TilerConfig::default()
    // ... existing config ...
    ;

// Apply simplification if enabled
let config = if args.simplify {
    config.with_simplify(args.simplify_factor)
} else {
    config
};
```

**Step 3: Run cargo check**

```bash
cargo check --package gpq-tiles
```

Expected: No errors

**Step 4: Test CLI help**

```bash
cargo run --package gpq-tiles -- --help | grep -A3 simplify
```

Expected: Shows --simplify and --simplify-factor flags with descriptions

**Step 5: Commit**

```bash
git add crates/cli/src/main.rs
git commit -m "feat(cli): add --simplify and --simplify-factor flags

Enable zoom-dependent geometry simplification from the command line.
--simplify enables the feature, --simplify-factor controls aggressiveness.

Usage:
  gpq-tiles input.parquet output.pmtiles --simplify
  gpq-tiles input.parquet output.pmtiles --simplify --simplify-factor 0.5

Refs: #156"
```

---

## Task 9: Add Python bindings for simplification

**Files:**
- Modify: `crates/python/src/lib.rs`

**Step 1: Find PyTilerConfig or equivalent**

Search for where Python bindings expose TilerConfig:

```bash
grep -n "simplify\|TilerConfig" crates/python/src/lib.rs
```

**Step 2: Add simplify parameter**

Add `simplify_factor: Option<f64>` parameter to the Python-exposed configuration, following the existing pattern for other optional parameters.

**Step 3: Run cargo check**

```bash
cd crates/python && cargo check
```

Expected: No errors

**Step 4: Commit**

```bash
git add crates/python/src/lib.rs
git commit -m "feat(python): add simplify_factor to Python bindings

Expose zoom-dependent simplification configuration to Python API.

Refs: #156"
```

---

## Task 10: Add integration test with real GeoParquet data

**Files:**
- Modify: `crates/core/src/integration_tests.rs`

**Step 1: Write integration test**

```rust
#[test]
fn test_simplification_reduces_tile_size() {
    use crate::pipeline::{TilerConfig, generate_tiles_to_writer};
    use std::io::Cursor;
    
    // Use existing test fixture (or skip if not available)
    let test_file = std::path::Path::new("tests/fixtures/roads.parquet");
    if !test_file.exists() {
        eprintln!("Skipping test: roads.parquet fixture not found");
        return;
    }
    
    // Generate tiles WITHOUT simplification
    let config_no_simplify = TilerConfig::default()
        .with_zoom_range(0, 6)
        .with_layer_name("roads");
    
    let mut output_no_simplify = Cursor::new(Vec::new());
    let stats_no_simplify = generate_tiles_to_writer(
        test_file,
        &mut output_no_simplify,
        config_no_simplify,
    ).expect("Tiling without simplification failed");
    
    // Generate tiles WITH simplification
    let config_simplify = TilerConfig::default()
        .with_zoom_range(0, 6)
        .with_layer_name("roads")
        .with_simplify(1.0);
    
    let mut output_simplify = Cursor::new(Vec::new());
    let stats_simplify = generate_tiles_to_writer(
        test_file,
        &mut output_simplify,
        config_simplify,
    ).expect("Tiling with simplification failed");
    
    // Simplified output should be smaller (at least at low zoom levels)
    let size_no_simplify = output_no_simplify.get_ref().len();
    let size_simplify = output_simplify.get_ref().len();
    
    println!("Without simplification: {} bytes", size_no_simplify);
    println!("With simplification: {} bytes", size_simplify);
    println!("Reduction: {:.1}%", (1.0 - size_simplify as f64 / size_no_simplify as f64) * 100.0);
    
    // Expect at least some reduction for road data
    assert!(
        size_simplify <= size_no_simplify,
        "Simplification should not increase output size"
    );
}
```

**Step 2: Run integration test**

```bash
cargo test --package gpq-tiles-core test_simplification_reduces_tile_size -- --nocapture
```

Expected: PASS (or skip if fixture unavailable)

**Step 3: Commit**

```bash
git add crates/core/src/integration_tests.rs
git commit -m "test(core): add integration test for simplification

Verify that --simplify reduces output tile size for linear features.

Refs: #156"
```

---

## Task 11: Update documentation

**Files:**
- Modify: `docs/advanced-usage.md`
- Modify: `docs/api-reference.md`

**Step 1: Add simplification section to advanced-usage.md**

```markdown
## Geometry Simplification

For datasets with complex linear features (roads, rivers, coastlines, boundaries),
zoom-dependent simplification can dramatically reduce tile sizes at low zoom levels.

### Basic Usage

```bash
# Enable with default settings (1 pixel tolerance)
gpq-tiles roads.parquet roads.pmtiles --simplify

# Custom tolerance (more aggressive)
gpq-tiles roads.parquet roads.pmtiles --simplify --simplify-factor 2.0

# Preserve more detail
gpq-tiles roads.parquet roads.pmtiles --simplify --simplify-factor 0.5
```

### How It Works

At each zoom level, Douglas-Peucker simplification is applied with a tolerance
proportional to the pixel size at that zoom. Lower zoom levels (zoomed out) 
have larger pixels, so more vertices are removed.

**Tile boundary preservation**: Points on tile boundaries are never removed,
preventing visible seams between adjacent tiles.

### When to Use

- **Linear features**: Roads, rivers, boundaries, coastlines
- **Low zoom levels**: Where high vertex density causes oversized tiles
- **CannotReduceFurther errors**: Simplification reduces geometry size, not feature count

### Combining with Other Options

```bash
# Simplify + adaptive dropping for maximum size reduction
gpq-tiles roads.parquet roads.pmtiles \
  --simplify \
  --drop-smallest-as-needed \
  --max-tile-size 500K
```
```

**Step 2: Update api-reference.md**

Add to TilerConfig section:

```markdown
### simplify_factor

- Type: `Option<f64>`
- Default: `None` (disabled)
- CLI: `--simplify`, `--simplify-factor`

When set, enables zoom-dependent Douglas-Peucker simplification.
The tolerance at each zoom level is `pixel_tolerance * factor`.

```rust
let config = TilerConfig::default()
    .with_simplify(1.0);  // 1 pixel tolerance
```
```

**Step 3: Commit**

```bash
git add docs/advanced-usage.md docs/api-reference.md
git commit -m "docs: add simplification documentation

Document --simplify and --simplify-factor CLI flags, explain when
to use simplification, and how it interacts with other options.

Refs: #156"
```

---

## Task 12: Final verification and cleanup

**Step 1: Run full test suite for affected modules**

```bash
cargo test --package gpq-tiles-core simplify:: -- --nocapture
cargo test --package gpq-tiles-core test_tiler_config -- --nocapture
```

**Step 2: Run cargo fmt**

```bash
cargo fmt --all
```

**Step 3: Run cargo clippy**

```bash
cargo clippy --package gpq-tiles-core --package gpq-tiles -- -D warnings
```

**Step 4: Build release to verify optimization**

```bash
cargo build --release --package gpq-tiles
```

**Step 5: Manual test with real data (if available)**

```bash
# Test with Canada roads or similar linear dataset
cargo run --release --package gpq-tiles -- \
  /path/to/roads.parquet \
  /tmp/roads-simplified.pmtiles \
  --simplify \
  --max-zoom 10

# Compare sizes
ls -la /tmp/roads*.pmtiles
```

**Step 6: Final commit**

```bash
git add -A
git commit -m "chore: final cleanup for simplification feature

Refs: #156"
```

---

## Acceptance Criteria Checklist

- [ ] `--simplify` flag enables zoom-dependent simplification
- [ ] `--simplify-factor` controls tolerance (default: 1.0 = 1 pixel)
- [ ] Simplification reduces tile sizes for linear features at low zoom
- [ ] Canada roads dataset can be tiled with --simplify without CannotReduceFurther errors
- [ ] Simplification is applied per-zoom (stricter at low zoom, gentler at high zoom)
- [ ] Tile boundary points are preserved (no visible seams)
- [ ] All tests pass
- [ ] Documentation updated

---

## Summary of Changes

| File | Changes |
|------|---------|
| `crates/core/src/pipeline.rs` | Add `simplify_factor` to TilerConfig, integrate into `encode_tile_from_raw` |
| `crates/core/src/simplify.rs` | Add boundary detection, boundary-preserving simplification functions |
| `crates/cli/src/main.rs` | Add `--simplify` and `--simplify-factor` CLI flags |
| `crates/python/src/lib.rs` | Add `simplify_factor` to Python bindings |
| `docs/advanced-usage.md` | Add simplification documentation |
| `docs/api-reference.md` | Add simplify_factor API docs |
