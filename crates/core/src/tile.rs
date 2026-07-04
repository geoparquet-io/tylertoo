//! Tile coordinate math and utilities
//!
//! This module provides functions for converting between geographic coordinates (lat/lng)
//! and tile coordinates (x/y/z) using Web Mercator projection.

use std::f64::consts::PI;

/// Tile coordinates: x, y, and zoom level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    pub x: u32,
    pub y: u32,
    pub z: u8,
}

impl TileCoord {
    /// Create a new tile coordinate
    pub fn new(x: u32, y: u32, z: u8) -> Self {
        Self { x, y, z }
    }

    /// Get the bounding box of this tile in geographic coordinates (lng/lat)
    pub fn bounds(&self) -> TileBounds {
        let n = 2_f64.powi(self.z as i32);
        let lng_min = (self.x as f64) / n * 360.0 - 180.0;
        let lng_max = (self.x as f64 + 1.0) / n * 360.0 - 180.0;

        let lat_rad = |y: f64| {
            let y_rad = PI * (1.0 - 2.0 * y / n);
            y_rad.sinh().atan().to_degrees()
        };

        let lat_max = lat_rad(self.y as f64);
        let lat_min = lat_rad(self.y as f64 + 1.0);

        TileBounds {
            lng_min,
            lat_min,
            lng_max,
            lat_max,
        }
    }

    /// Get the parent tile at zoom level z-1.
    ///
    /// Each tile at zoom z has exactly one parent at zoom z-1.
    /// The parent contains this tile and its 3 siblings (2x2 grid).
    /// Returns `None` at zoom 0 (no parent).
    pub fn parent(&self) -> Option<TileCoord> {
        if self.z == 0 {
            return None;
        }
        Some(TileCoord::new(self.x / 2, self.y / 2, self.z - 1))
    }

    /// Get the four child tiles at zoom level z+1.
    ///
    /// Each tile at zoom z has exactly four children at zoom z+1,
    /// forming a 2x2 grid that exactly covers the parent tile.
    /// Returns `None` at zoom 30 (maximum supported zoom).
    pub fn children(&self) -> Option<[TileCoord; 4]> {
        if self.z >= 30 {
            return None;
        }
        let child_z = self.z + 1;
        let cx = self.x * 2;
        let cy = self.y * 2;
        Some([
            TileCoord::new(cx, cy, child_z),         // top-left
            TileCoord::new(cx + 1, cy, child_z),     // top-right
            TileCoord::new(cx, cy + 1, child_z),     // bottom-left
            TileCoord::new(cx + 1, cy + 1, child_z), // bottom-right
        ])
    }
}

/// Geographic bounding box
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileBounds {
    pub lng_min: f64,
    pub lat_min: f64,
    pub lng_max: f64,
    pub lat_max: f64,
}

impl TileBounds {
    /// Create a new bounding box
    pub fn new(lng_min: f64, lat_min: f64, lng_max: f64, lat_max: f64) -> Self {
        Self {
            lng_min,
            lat_min,
            lng_max,
            lat_max,
        }
    }

    /// Create an empty/invalid bounding box
    pub fn empty() -> Self {
        Self {
            lng_min: f64::INFINITY,
            lat_min: f64::INFINITY,
            lng_max: f64::NEG_INFINITY,
            lat_max: f64::NEG_INFINITY,
        }
    }

    /// Check if this is a valid bounding box
    pub fn is_valid(&self) -> bool {
        self.lng_min <= self.lng_max && self.lat_min <= self.lat_max
    }

    /// Expand this bounding box to include another
    pub fn expand(&mut self, other: &Self) {
        self.lng_min = self.lng_min.min(other.lng_min);
        self.lat_min = self.lat_min.min(other.lat_min);
        self.lng_max = self.lng_max.max(other.lng_max);
        self.lat_max = self.lat_max.max(other.lat_max);
    }

    /// Get the width in degrees
    pub fn width(&self) -> f64 {
        self.lng_max - self.lng_min
    }

    /// Get the height in degrees
    pub fn height(&self) -> f64 {
        self.lat_max - self.lat_min
    }
}

/// Convert longitude/latitude to tile coordinates at a given zoom level
///
/// Uses Web Mercator projection (EPSG:3857)
///
/// # Arguments
///
/// * `lng` - Longitude in degrees (-180 to 180)
/// * `lat` - Latitude in degrees (-85.0511 to 85.0511, Web Mercator bounds)
/// * `zoom` - Zoom level (0-30)
///
/// # Returns
///
/// TileCoord with x, y, and zoom
pub fn lng_lat_to_tile(lng: f64, lat: f64, zoom: u8) -> TileCoord {
    let n = 2_f64.powi(zoom as i32);

    // Maximum valid tile coordinate at this zoom level
    let max_coord = 2_u32.pow(zoom as u32).saturating_sub(1);

    // Convert longitude to tile x
    // Clamp to valid range to handle lng=180° edge case (which would produce x=2^z)
    let x = ((lng + 180.0) / 360.0 * n).floor() as u32;
    let x = x.min(max_coord);

    // Clamp latitude to Web Mercator bounds to prevent tile coordinate overflow.
    // Web Mercator is defined for ~±85.0511° but we use ±85.05° for safety margin.
    // Without this clamp, lat=-90° produces y values 6-20x larger than valid bounds.
    let lat = lat.clamp(-85.05, 85.05);

    // Convert latitude to tile y (Web Mercator)
    // Clamp to valid range for the same edge case reasons
    let lat_rad = lat.to_radians();
    let y = ((1.0 - lat_rad.tan().asinh() / PI) / 2.0 * n).floor() as u32;
    let y = y.min(max_coord);

    TileCoord::new(x, y, zoom)
}

/// Get the geographic bounds of a tile
///
/// Convenience function that wraps `TileCoord::bounds()`
pub fn tile_bounds(x: u32, y: u32, z: u8) -> TileBounds {
    TileCoord::new(x, y, z).bounds()
}

/// Get all tiles that intersect a geographic bounding box at a given zoom level
///
/// Handles antimeridian crossing: when `lng_min > lng_max`, the bbox crosses
/// the antimeridian (180° longitude) and is split into two ranges:
/// `[lng_min, 180°]` and `[-180°, lng_max]`.
///
/// # Arguments
///
/// * `bbox` - Geographic bounding box
/// * `zoom` - Zoom level
///
/// # Returns
///
/// Iterator of TileCoord that intersect the bbox
pub fn tiles_for_bbox(bbox: &TileBounds, zoom: u8) -> impl Iterator<Item = TileCoord> {
    let crosses_antimeridian = bbox.lng_min > bbox.lng_max;

    // Get y-tile range (latitude doesn't wrap)
    let min_y_tile = lng_lat_to_tile(bbox.lng_min, bbox.lat_max, zoom).y; // lat_max -> min_y
    let max_y_tile = lng_lat_to_tile(bbox.lng_min, bbox.lat_min, zoom).y; // lat_min -> max_y

    let max_tile_x = 2_u32.pow(zoom as u32) - 1;

    // Calculate x-tile ranges
    let (x_ranges, second_range): ((u32, u32), Option<(u32, u32)>) = if crosses_antimeridian {
        // Split into two ranges: [lng_min, 180°] and [-180°, lng_max]
        let west_x = lng_lat_to_tile(bbox.lng_min, 0.0, zoom).x; // From lng_min to 180°
        let east_x = lng_lat_to_tile(bbox.lng_max, 0.0, zoom).x; // From -180° to lng_max

        // First range: lng_min to 180° (west_x to max_tile_x)
        // Second range: -180° to lng_max (0 to east_x)
        ((west_x, max_tile_x), Some((0, east_x)))
    } else {
        // Normal case: single range
        let min_x = lng_lat_to_tile(bbox.lng_min, 0.0, zoom).x;
        let max_x = lng_lat_to_tile(bbox.lng_max, 0.0, zoom).x;
        ((min_x, max_x), None)
    };

    // Generate tiles for the first x-range
    let first_tiles = (min_y_tile..=max_y_tile)
        .flat_map(move |y| (x_ranges.0..=x_ranges.1).map(move |x| TileCoord::new(x, y, zoom)));

    // Generate tiles for the second x-range (if crossing antimeridian)
    let second_tiles = second_range.into_iter().flat_map(move |(x_min, x_max)| {
        (min_y_tile..=max_y_tile)
            .flat_map(move |y| (x_min..=x_max).map(move |x| TileCoord::new(x, y, zoom)))
    });

    first_tiles.chain(second_tiles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lng_lat_to_tile_origin() {
        // Origin (null island: 0, 0) at zoom 0
        let tile = lng_lat_to_tile(0.0, 0.0, 0);
        assert_eq!(tile, TileCoord::new(0, 0, 0));
    }

    #[test]
    fn test_lng_lat_to_tile_zoom_1() {
        // Test various points at zoom 1
        let tile = lng_lat_to_tile(0.0, 0.0, 1);
        assert_eq!(tile.x, 1);
        assert_eq!(tile.y, 1);
        assert_eq!(tile.z, 1);

        // Top-left quadrant
        let tile = lng_lat_to_tile(-90.0, 45.0, 1);
        assert_eq!(tile.x, 0);

        // Top-right quadrant
        let tile = lng_lat_to_tile(90.0, 45.0, 1);
        assert_eq!(tile.x, 1);
    }

    #[test]
    fn test_tile_bounds() {
        // Tile 0,0,0 should cover the whole world
        let tile = TileCoord::new(0, 0, 0);
        let bounds = tile.bounds();

        assert!((bounds.lng_min - (-180.0)).abs() < 0.0001);
        assert!((bounds.lng_max - 180.0).abs() < 0.0001);
        // Lat bounds are Web Mercator limits (~85.05 degrees)
        assert!(bounds.lat_min < -85.0);
        assert!(bounds.lat_max > 85.0);
    }

    #[test]
    fn test_tiles_for_bbox_single_tile() {
        // Small bbox that fits in one tile
        let bbox = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 10).collect();

        // Should be at least 1 tile
        assert!(!tiles.is_empty());

        // All tiles should be at zoom 10
        for tile in &tiles {
            assert_eq!(tile.z, 10);
        }
    }

    #[test]
    fn test_tiles_for_bbox_multiple_tiles() {
        // Larger bbox spanning multiple tiles at zoom 5
        let bbox = TileBounds::new(-10.0, -10.0, 10.0, 10.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 5).collect();

        // Should cover multiple tiles
        assert!(tiles.len() > 1);

        // Check bounds are reasonable
        let first = tiles.first().unwrap();
        let last = tiles.last().unwrap();
        assert!(first.x <= last.x);
        assert!(first.y <= last.y);
    }

    #[test]
    fn test_bbox_expand() {
        let mut bbox1 = TileBounds::new(-10.0, -10.0, 10.0, 10.0);
        let bbox2 = TileBounds::new(-20.0, -5.0, 5.0, 15.0);

        bbox1.expand(&bbox2);

        assert_eq!(bbox1.lng_min, -20.0);
        assert_eq!(bbox1.lat_min, -10.0);
        assert_eq!(bbox1.lng_max, 10.0);
        assert_eq!(bbox1.lat_max, 15.0);
    }

    #[test]
    fn test_bbox_empty() {
        let bbox = TileBounds::empty();
        assert!(!bbox.is_valid());

        let mut bbox = TileBounds::empty();
        bbox.expand(&TileBounds::new(-10.0, -10.0, 10.0, 10.0));
        assert!(bbox.is_valid());
        assert_eq!(bbox.lng_min, -10.0);
    }

    #[test]
    fn test_tile_coord_round_trip() {
        // For various zooms, check that a tile's center converts back to the same tile
        for zoom in 0..=14 {
            // Use valid tile coordinates for each zoom (max tile = 2^zoom - 1)
            let max_coord = 2_u32.pow(zoom as u32) - 1;
            let x = max_coord.min(100);
            let y = max_coord.min(200);

            let tile = TileCoord::new(x, y, zoom);
            let bounds = tile.bounds();

            let center_lng = (bounds.lng_min + bounds.lng_max) / 2.0;
            let center_lat = (bounds.lat_min + bounds.lat_max) / 2.0;

            let tile_back = lng_lat_to_tile(center_lng, center_lat, zoom);

            assert_eq!(tile, tile_back, "Round-trip failed at zoom {}", zoom);
        }
    }

    #[test]
    fn test_tiles_for_bbox_antimeridian_crossing() {
        // Fiji area: bbox from 170°E to 170°W (which is -170°)
        // This crosses the antimeridian at 180°
        let bbox = TileBounds::new(170.0, -20.0, -170.0, -10.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 4).collect();

        // Should NOT be empty - this is the bug we're fixing
        assert!(
            !tiles.is_empty(),
            "Antimeridian crossing bbox should produce tiles"
        );

        // Collect all unique x coordinates
        let x_coords: std::collections::HashSet<_> = tiles.iter().map(|t| t.x).collect();

        // At zoom 4, the world is 16 tiles wide (0-15)
        // 170° is around x=15, -170° is around x=0
        // We should have tiles on BOTH sides of the antimeridian
        let has_high_x = x_coords.iter().any(|&x| x >= 15); // Near 180° (east side)
        let has_low_x = x_coords.iter().any(|&x| x <= 1); // Near -180° (west side)

        assert!(
            has_high_x && has_low_x,
            "Should have tiles on both sides of antimeridian. Got x coords: {:?}",
            x_coords
        );
    }

    #[test]
    fn antimeridian_inflated_bbox_covers_full_world_row() {
        // Issue #188 behavior pin. `tiles_for_bbox` supports wrapped bboxes
        // (lng_min > lng_max, tested above), but the overview pipeline never
        // produces one: bboxes come from `geo::bounding_rect` (plain min/max),
        // so an antimeridian-crossing feature arrives as the INFLATED bbox
        // [-179.9, .., 179.9]. That bbox enumerates every x column at the
        // zoom — the full world row — not two columns at ±180°.
        // See `context/ANTIMERIDIAN.md`.
        let bbox = TileBounds::new(-179.9, -0.1, 179.9, 0.1);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 4).collect();
        let x_coords: std::collections::HashSet<_> = tiles.iter().map(|t| t.x).collect();
        assert_eq!(
            x_coords.len(),
            16,
            "PIN: inflated antimeridian bbox spans all 16 x columns at z4"
        );
    }

    #[test]
    fn test_tiles_for_bbox_normal_still_works() {
        // Normal case: Europe (doesn't cross antimeridian)
        let bbox = TileBounds::new(-10.0, 40.0, 10.0, 50.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 4).collect();

        assert!(!tiles.is_empty(), "Normal bbox should produce tiles");

        // All tiles should be in the expected range
        for tile in &tiles {
            assert_eq!(tile.z, 4);
        }
    }

    // ========== Parent/Children Tests ==========

    #[test]
    fn test_tile_parent_at_zoom_0() {
        let tile = TileCoord::new(0, 0, 0);
        assert_eq!(tile.parent(), None, "Zoom 0 tile has no parent");
    }

    #[test]
    fn test_tile_parent_at_zoom_1() {
        // All four z=1 tiles should have z=0/0/0 as parent
        for x in 0..2 {
            for y in 0..2 {
                let tile = TileCoord::new(x, y, 1);
                let parent = tile.parent().expect("z=1 tile should have parent");
                assert_eq!(parent, TileCoord::new(0, 0, 0));
            }
        }
    }

    #[test]
    fn test_tile_parent_at_higher_zoom() {
        let tile = TileCoord::new(5, 7, 4);
        let parent = tile.parent().expect("Should have parent");
        assert_eq!(parent, TileCoord::new(2, 3, 3));

        let grandparent = parent.parent().expect("Should have grandparent");
        assert_eq!(grandparent, TileCoord::new(1, 1, 2));
    }

    #[test]
    fn test_tile_children() {
        let tile = TileCoord::new(1, 2, 3);
        let children = tile.children().expect("Should have children");

        assert_eq!(children[0], TileCoord::new(2, 4, 4)); // top-left
        assert_eq!(children[1], TileCoord::new(3, 4, 4)); // top-right
        assert_eq!(children[2], TileCoord::new(2, 5, 4)); // bottom-left
        assert_eq!(children[3], TileCoord::new(3, 5, 4)); // bottom-right
    }

    #[test]
    fn test_tile_children_at_max_zoom() {
        let tile = TileCoord::new(0, 0, 30);
        assert_eq!(tile.children(), None, "Zoom 30 tile has no children");
    }

    #[test]
    fn test_parent_child_round_trip() {
        // A tile's parent's children should include the original tile
        let tile = TileCoord::new(5, 7, 4);
        let parent = tile.parent().unwrap();
        let siblings = parent.children().unwrap();
        assert!(
            siblings.contains(&tile),
            "Parent's children should include original tile"
        );
    }

    #[test]
    fn test_child_parent_round_trip() {
        // Each child's parent should be the original tile
        let tile = TileCoord::new(3, 2, 5);
        let children = tile.children().unwrap();
        for child in &children {
            assert_eq!(
                child.parent().unwrap(),
                tile,
                "Each child's parent should be the original tile"
            );
        }
    }

    #[test]
    fn test_children_cover_parent_bounds() {
        // The four children should collectively cover the parent's bounds
        let parent = TileCoord::new(1, 1, 2);
        let parent_bounds = parent.bounds();
        let children = parent.children().unwrap();

        // Find the bounding box of all children
        let mut min_lng = f64::INFINITY;
        let mut max_lng = f64::NEG_INFINITY;
        let mut min_lat = f64::INFINITY;
        let mut max_lat = f64::NEG_INFINITY;

        for child in &children {
            let b = child.bounds();
            min_lng = min_lng.min(b.lng_min);
            max_lng = max_lng.max(b.lng_max);
            min_lat = min_lat.min(b.lat_min);
            max_lat = max_lat.max(b.lat_max);
        }

        assert!(
            (min_lng - parent_bounds.lng_min).abs() < 1e-10,
            "Children lng_min should match parent"
        );
        assert!(
            (max_lng - parent_bounds.lng_max).abs() < 1e-10,
            "Children lng_max should match parent"
        );
        assert!(
            (min_lat - parent_bounds.lat_min).abs() < 1e-10,
            "Children lat_min should match parent"
        );
        assert!(
            (max_lat - parent_bounds.lat_max).abs() < 1e-10,
            "Children lat_max should match parent"
        );
    }

    #[test]
    fn test_tiles_for_bbox_antimeridian_tile_count() {
        // At zoom 2, tiles are ~90° wide
        // A bbox from 170° to -170° spans about 20° (across the antimeridian)
        // Should produce a reasonable number of tiles, not wrap around the whole world
        let bbox = TileBounds::new(170.0, -20.0, -170.0, -10.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 2).collect();

        // At zoom 2 (4x4 grid), this should be ~1-2 tiles in x direction
        // The bbox is small, just crossing the antimeridian
        let x_coords: std::collections::HashSet<_> = tiles.iter().map(|t| t.x).collect();

        // Should have tiles from x=3 (170°-180°) and x=0 (-180° to -170°)
        assert!(
            x_coords.len() <= 3,
            "Antimeridian bbox should produce tiles only near the crossing, not wrap around. Got {} unique x coords: {:?}",
            x_coords.len(),
            x_coords
        );
    }

    #[test]
    fn test_lng_lat_to_tile_boundary_clamping() {
        // Test that lng=180 and lat=-85.05 don't produce out-of-bounds tile coordinates
        // This was a bug: lng=180 at zoom 0 produced x=1, but only x=0 is valid at zoom 0

        // At zoom 0, only tile (0,0,0) exists
        let tile = lng_lat_to_tile(180.0, 0.0, 0);
        assert_eq!(tile.x, 0, "lng=180 at zoom 0 should clamp to x=0");
        assert_eq!(tile.y, 0, "lat=0 at zoom 0 should be y=0");

        let tile = lng_lat_to_tile(180.0, -85.05, 0);
        assert_eq!(tile.x, 0, "lng=180 at zoom 0 should clamp to x=0");
        assert_eq!(tile.y, 0, "lat=-85.05 at zoom 0 should clamp to y=0");

        // At zoom 1, only x in [0,1] and y in [0,1] are valid
        let tile = lng_lat_to_tile(180.0, 0.0, 1);
        assert!(tile.x <= 1, "lng=180 at zoom 1 should have x <= 1");

        // Test various edge cases
        for zoom in 0..=10 {
            let max_valid = 2_u32.pow(zoom as u32) - 1;

            let tile_pos180 = lng_lat_to_tile(180.0, 0.0, zoom);
            assert!(
                tile_pos180.x <= max_valid,
                "lng=180 at zoom {} should have x <= {}, got {}",
                zoom,
                max_valid,
                tile_pos180.x
            );

            let tile_neg180 = lng_lat_to_tile(-180.0, 0.0, zoom);
            assert_eq!(
                tile_neg180.x, 0,
                "lng=-180 at zoom {} should have x = 0",
                zoom
            );

            let tile_north_pole = lng_lat_to_tile(0.0, 85.05, zoom);
            assert!(
                tile_north_pole.y <= max_valid,
                "lat=85.05 at zoom {} should have y <= {}",
                zoom,
                max_valid
            );

            let tile_south_pole = lng_lat_to_tile(0.0, -85.05, zoom);
            assert!(
                tile_south_pole.y <= max_valid,
                "lat=-85.05 at zoom {} should have y <= {}",
                zoom,
                max_valid
            );
        }
    }
}
