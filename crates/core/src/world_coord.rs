//! Integer world coordinate system for tippecanoe parity.
//!
//! This module provides 32-bit integer world coordinates that match tippecanoe's
//! internal coordinate system. Using integers throughout the pipeline eliminates
//! floating-point precision errors and improves performance.
//!
//! # Coordinate System
//!
//! At zoom 0, the world spans [0, 2^32) in both x and y:
//! - Origin (0, 0): Northwest corner (lng=-180, lat=~85.05)
//! - X increases eastward
//! - Y increases southward (Web Mercator convention)
//!
//! At zoom z, divide by 2^(32-z) to get tile coordinates.
//!
//! # Precision
//!
//! - At zoom 32: 1 unit ≈ 0.009 meters at equator
//! - At zoom 20: 1 unit ≈ 0.149 meters at equator
//!
//! # Reference
//!
//! Based on tippecanoe's coordinate system:
//! - geometry.hpp: `struct draw { long long x : 40; long long y : 40; }`
//! - projection.cpp: `*y = std::round(((1LL << 32) - 1) - ...)`

use std::f64::consts::PI;

use crate::tile::{TileBounds, TileCoord};

/// World scale: 2^32 units cover the entire world at zoom 0.
pub const WORLD_SCALE: u64 = 1_u64 << 32;

/// Half the world scale (2^31) - useful for center calculations.
pub const WORLD_HALF: u32 = 1_u32 << 31;

/// Maximum valid latitude for Web Mercator projection.
pub const MAX_LATITUDE: f64 = 85.05112878;

/// 32-bit world coordinate, matching tippecanoe's internal representation.
///
/// Uses unsigned integers because world coordinates span [0, 2^32).
///
/// # Examples
///
/// ```
/// use tylertoo_core::world_coord::{WorldCoord, lng_lat_to_world, WORLD_HALF};
///
/// // Null Island (0, 0) in geographic coordinates
/// let coord = lng_lat_to_world(0.0, 0.0);
/// assert_eq!(coord.x, WORLD_HALF); // Middle of world
/// assert_eq!(coord.y, WORLD_HALF); // Middle of world
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorldCoord {
    /// X coordinate in world space [0, 2^32)
    pub x: u32,
    /// Y coordinate in world space [0, 2^32)
    pub y: u32,
}

impl WorldCoord {
    /// Create a new world coordinate.
    #[inline]
    pub const fn new(x: u32, y: u32) -> Self {
        Self { x, y }
    }

    /// Create a WorldCoord from longitude/latitude.
    ///
    /// Convenience wrapper around [`lng_lat_to_world`].
    #[inline]
    pub fn from_lng_lat(lng: f64, lat: f64) -> Self {
        lng_lat_to_world(lng, lat)
    }

    /// Get the tile coordinate containing this world coordinate at the given zoom.
    ///
    /// # Arguments
    /// * `zoom` - Zoom level (0-30)
    ///
    /// # Returns
    /// TileCoord containing this point
    #[inline]
    pub fn to_tile(&self, zoom: u8) -> TileCoord {
        // At zoom 0, shift by 32 would overflow, but the result should be (0, 0)
        if zoom == 0 {
            return TileCoord::new(0, 0, 0);
        }
        let shift = 32 - zoom as u32;
        let x = self.x >> shift;
        let y = self.y >> shift;
        TileCoord::new(x, y, zoom)
    }

    /// Convert to tile-local coordinates within the given tile.
    ///
    /// # Arguments
    /// * `tile` - The tile to convert into
    /// * `extent` - Tile extent (typically 4096 for MVT)
    ///
    /// # Returns
    /// (x, y) in tile-local coordinates [0, extent]
    ///
    /// # Note
    /// Coordinates outside the tile will be outside [0, extent].
    /// This is intentional for buffer regions.
    #[inline]
    pub fn to_tile_local(&self, tile: &TileCoord, extent: u32) -> (i32, i32) {
        // Handle zoom 0 specially
        if tile.z == 0 {
            // At zoom 0, the entire world is one tile
            // Scale from [0, 2^32) to [0, extent)
            let local_x = ((self.x as u64) * extent as u64 / WORLD_SCALE) as i32;
            let local_y = ((self.y as u64) * extent as u64 / WORLD_SCALE) as i32;
            return (local_x, local_y);
        }

        let shift = 32 - tile.z as u32;
        let tile_size = 1_u64 << shift;

        // World position of tile's top-left corner
        let tile_x = (tile.x as u64) << shift;
        let tile_y = (tile.y as u64) << shift;

        // Position within tile, scaled to extent
        // Use i64 for intermediate calculations to handle negative buffer regions
        let local_x = ((self.x as i64 - tile_x as i64) * extent as i64 / tile_size as i64) as i32;
        let local_y = ((self.y as i64 - tile_y as i64) * extent as i64 / tile_size as i64) as i32;

        (local_x, local_y)
    }
}

/// Convert longitude/latitude to world coordinates.
///
/// Uses Web Mercator projection (EPSG:3857) with 32-bit precision.
///
/// # Arguments
/// * `lng` - Longitude in degrees [-180, 180]
/// * `lat` - Latitude in degrees [-85.05, 85.05] (Web Mercator bounds)
///
/// # Returns
/// WorldCoord with x, y in [0, 2^32) space
///
/// # Examples
///
/// ```
/// use tylertoo_core::world_coord::{lng_lat_to_world, WORLD_HALF};
///
/// // Null Island
/// let coord = lng_lat_to_world(0.0, 0.0);
/// assert_eq!(coord.x, WORLD_HALF);
/// assert_eq!(coord.y, WORLD_HALF);
///
/// // Northwest corner of world
/// let nw = lng_lat_to_world(-180.0, 85.05);
/// assert_eq!(nw.x, 0);
/// assert!(nw.y < 100_000_000); // Near 0 relative to 2^32
/// ```
pub fn lng_lat_to_world(lng: f64, lat: f64) -> WorldCoord {
    let scale = WORLD_SCALE as f64;

    // Longitude → x: simple linear mapping [-180, 180] → [0, 2^32)
    // Clamp to prevent overflow at exactly 180°
    let lng_normalized = ((lng + 180.0) / 360.0).clamp(0.0, 0.9999999999);
    let x = (lng_normalized * scale) as u32;

    // Latitude → y: Web Mercator projection
    // Clamp to valid Web Mercator range to prevent infinity
    let lat_clamped = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE);
    let lat_rad = lat_clamped.to_radians();

    // Web Mercator formula: y = (1 - ln(tan(lat) + sec(lat)) / pi) / 2
    // sec(lat) = 1 / cos(lat)
    let mercator_y = (lat_rad.tan() + 1.0 / lat_rad.cos()).ln();
    let y_normalized = ((1.0 - mercator_y / PI) / 2.0).clamp(0.0, 0.9999999999);
    let y = (y_normalized * scale) as u32;

    WorldCoord::new(x, y)
}

/// Convert world coordinates back to longitude/latitude.
///
/// # Arguments
/// * `coord` - World coordinate
///
/// # Returns
/// (longitude, latitude) in degrees
///
/// # Examples
///
/// ```
/// use tylertoo_core::world_coord::{lng_lat_to_world, world_to_lng_lat};
///
/// let coord = lng_lat_to_world(-73.985428, 40.748817); // NYC
/// let (lng, lat) = world_to_lng_lat(coord);
/// assert!((lng - (-73.985428)).abs() < 0.0001);
/// assert!((lat - 40.748817).abs() < 0.0001);
/// ```
pub fn world_to_lng_lat(coord: WorldCoord) -> (f64, f64) {
    let scale = WORLD_SCALE as f64;

    // x → longitude: simple linear mapping [0, 2^32) → [-180, 180]
    let lng = (coord.x as f64) / scale * 360.0 - 180.0;

    // y → latitude: inverse Web Mercator projection
    let mercator_y = (1.0 - 2.0 * (coord.y as f64) / scale) * PI;
    let lat = (mercator_y.sinh()).atan().to_degrees();

    (lng, lat)
}

/// Convert tile-local coordinates back to world coordinates.
///
/// # Arguments
/// * `tile` - The tile the coordinates are relative to
/// * `local_x` - X coordinate in tile space [0, extent]
/// * `local_y` - Y coordinate in tile space [0, extent]
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// WorldCoord in global space
pub fn tile_local_to_world(
    tile: &TileCoord,
    local_x: i32,
    local_y: i32,
    extent: u32,
) -> WorldCoord {
    // Handle zoom 0 specially
    if tile.z == 0 {
        let world_x = ((local_x as i64) * WORLD_SCALE as i64 / extent as i64) as u32;
        let world_y = ((local_y as i64) * WORLD_SCALE as i64 / extent as i64) as u32;
        return WorldCoord::new(world_x, world_y);
    }

    let shift = 32 - tile.z as u32;
    let tile_size = 1_u64 << shift;

    // World position of tile's top-left corner
    let tile_world_x = (tile.x as u64) << shift;
    let tile_world_y = (tile.y as u64) << shift;

    // Convert local to world
    let world_x =
        (tile_world_x as i64 + (local_x as i64) * tile_size as i64 / extent as i64) as u32;
    let world_y =
        (tile_world_y as i64 + (local_y as i64) * tile_size as i64 / extent as i64) as u32;

    WorldCoord::new(world_x, world_y)
}

// ============================================================================
// WorldBounds: Axis-aligned bounding box in world coordinate space
// ============================================================================

/// Axis-aligned bounding box in 32-bit world coordinate space.
///
/// Represents a rectangular region for tile clipping and intersection tests.
/// Uses `u32` coordinates matching the `WorldCoord` system.
///
/// # Coordinate System
///
/// - `x_min` / `x_max`: Horizontal bounds [0, 2^32) where X increases eastward
/// - `y_min` / `y_max`: Vertical bounds [0, 2^32) where Y increases southward
///
/// # Relationship to Tiles
///
/// At zoom level `z`, a tile's world bounds span exactly `2^(32-z)` units in
/// each dimension. The buffer for clipping can be computed in world units:
///
/// ```text
/// buffer_world = tile_size * buffer_pixels / extent
///              = 2^(32-z) * 8 / 4096
/// ```
///
/// This replaces the imprecise `buffer_pixels_to_degrees` calculation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorldBounds {
    pub x_min: u32,
    pub y_min: u32,
    pub x_max: u32,
    pub y_max: u32,
}

impl WorldBounds {
    /// Create a new world bounds from min/max coordinates.
    #[inline]
    pub const fn new(x_min: u32, y_min: u32, x_max: u32, y_max: u32) -> Self {
        Self {
            x_min,
            y_min,
            x_max,
            y_max,
        }
    }

    /// Create world bounds for a tile at the given coordinate.
    ///
    /// At zoom `z`, each tile spans `2^(32-z)` world units in each dimension.
    ///
    /// # Examples
    ///
    /// ```
    /// use tylertoo_core::world_coord::WorldBounds;
    /// use tylertoo_core::tile::TileCoord;
    ///
    /// // Zoom 0: single tile covers entire world
    /// let bounds = WorldBounds::from_tile(&TileCoord::new(0, 0, 0));
    /// assert_eq!(bounds.x_min, 0);
    /// assert_eq!(bounds.y_min, 0);
    /// assert_eq!(bounds.x_max, u32::MAX);
    /// assert_eq!(bounds.y_max, u32::MAX);
    /// ```
    pub fn from_tile(tile: &TileCoord) -> Self {
        if tile.z == 0 {
            return Self::new(0, 0, u32::MAX, u32::MAX);
        }

        let shift = 32 - tile.z as u32;
        let tile_size = 1_u64 << shift;

        let x_min = ((tile.x as u64) << shift) as u32;
        let y_min = ((tile.y as u64) << shift) as u32;

        // Saturate to u32::MAX to avoid overflow at tile boundaries
        let x_max = ((tile.x as u64 + 1) * tile_size - 1).min(u32::MAX as u64) as u32;
        let y_max = ((tile.y as u64 + 1) * tile_size - 1).min(u32::MAX as u64) as u32;

        Self::new(x_min, y_min, x_max, y_max)
    }

    /// Create world bounds for a tile with a buffer in pixels.
    ///
    /// The buffer is converted to world units based on the tile size and extent:
    /// `buffer_world = tile_size_world * buffer_pixels / extent`
    ///
    /// This is the integer-precision equivalent of `buffer_pixels_to_degrees`.
    ///
    /// # Arguments
    /// * `tile` - The tile coordinate
    /// * `buffer_pixels` - Buffer in pixels (e.g., 8)
    /// * `extent` - Tile extent (e.g., 4096)
    pub fn from_tile_with_buffer(tile: &TileCoord, buffer_pixels: u32, extent: u32) -> Self {
        let base = Self::from_tile(tile);

        if buffer_pixels == 0 {
            return base;
        }

        // Tile size in world units at this zoom level
        let tile_size_world: u64 = if tile.z == 0 {
            WORLD_SCALE
        } else {
            1_u64 << (32 - tile.z as u32)
        };

        // Buffer in world units: tile_size * buffer_pixels / extent
        // Use u64 to avoid overflow
        let buffer_world = (tile_size_world * buffer_pixels as u64 / extent as u64) as u32;

        Self::new(
            base.x_min.saturating_sub(buffer_world),
            base.y_min.saturating_sub(buffer_world),
            base.x_max.saturating_add(buffer_world),
            base.y_max.saturating_add(buffer_world),
        )
    }

    /// Check if a point is inside (or on the boundary of) these bounds.
    #[inline]
    pub fn contains(&self, coord: &WorldCoord) -> bool {
        coord.x >= self.x_min
            && coord.x <= self.x_max
            && coord.y >= self.y_min
            && coord.y <= self.y_max
    }

    /// Check if these bounds intersect with another bounds.
    #[inline]
    pub fn intersects(&self, other: &WorldBounds) -> bool {
        self.x_max >= other.x_min
            && self.x_min <= other.x_max
            && self.y_max >= other.y_min
            && self.y_min <= other.y_max
    }

    /// Check if another bounds is fully contained within this bounds.
    #[inline]
    pub fn contains_bounds(&self, other: &WorldBounds) -> bool {
        other.x_min >= self.x_min
            && other.x_max <= self.x_max
            && other.y_min >= self.y_min
            && other.y_max <= self.y_max
    }

    /// Width of the bounds in world units.
    #[inline]
    pub fn width(&self) -> u32 {
        self.x_max - self.x_min
    }

    /// Height of the bounds in world units.
    #[inline]
    pub fn height(&self) -> u32 {
        self.y_max - self.y_min
    }

    /// Convert to f64-based TileBounds (geographic coordinates).
    ///
    /// This is a convenience method for interoperability with the existing
    /// f64-based pipeline during Phase 1 migration.
    pub fn to_tile_bounds(&self) -> TileBounds {
        let (lng_min, lat_max) = world_to_lng_lat(WorldCoord::new(self.x_min, self.y_min));
        let (lng_max, lat_min) = world_to_lng_lat(WorldCoord::new(self.x_max, self.y_max));
        TileBounds::new(lng_min, lat_min, lng_max, lat_max)
    }

    /// Create WorldBounds from f64-based TileBounds (geographic coordinates).
    ///
    /// Converts the geographic bounds to world coordinate space.
    /// Note: This involves the Web Mercator projection, so lat/y mapping is non-linear.
    pub fn from_tile_bounds(bounds: &TileBounds) -> Self {
        // In world coord space, Y increases southward, so:
        // - lat_max (north) → smaller y value
        // - lat_min (south) → larger y value
        let nw = lng_lat_to_world(bounds.lng_min, bounds.lat_max);
        let se = lng_lat_to_world(bounds.lng_max, bounds.lat_min);
        Self::new(nw.x, nw.y, se.x, se.y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========== Basic Conversion Tests ==========

    #[test]
    fn test_null_island_conversion() {
        // Null Island (0, 0) should be in the middle of the world
        let coord = lng_lat_to_world(0.0, 0.0);

        // x should be exactly at 50% of world scale (2^31)
        assert_eq!(coord.x, WORLD_HALF, "x should be 2^31 for lng=0");

        // y should also be at 50% (equator in Web Mercator)
        assert_eq!(coord.y, WORLD_HALF, "y should be 2^31 for lat=0");
    }

    #[test]
    fn test_northwest_corner() {
        // Northwest corner: lng=-180, lat=85.05 (max Web Mercator lat)
        let coord = lng_lat_to_world(-180.0, MAX_LATITUDE);

        // x should be 0 (western edge)
        assert_eq!(coord.x, 0, "x should be 0 for lng=-180");

        // y should be near 0 (northern edge in Web Mercator)
        assert!(
            coord.y < 1000,
            "y should be near 0 for max latitude, got {}",
            coord.y
        );
    }

    #[test]
    fn test_southeast_corner() {
        // Test point near southeast corner (can't use exactly 180, -85.05 due to clamping)
        let coord = lng_lat_to_world(179.999, -MAX_LATITUDE);

        // x should be near max
        assert!(
            coord.x > u32::MAX - 100000,
            "x should be near max for lng=179.999, got {}",
            coord.x
        );

        // y should be near max for southern edge
        assert!(
            coord.y > u32::MAX - 100000,
            "y should be near max for lat=-85.05, got {}",
            coord.y
        );
    }

    #[test]
    fn test_round_trip_null_island() {
        let original = (0.0, 0.0);
        let coord = lng_lat_to_world(original.0, original.1);
        let (lng, lat) = world_to_lng_lat(coord);

        assert!(
            (lng - original.0).abs() < 0.0001,
            "Longitude round-trip failed: {} vs {}",
            lng,
            original.0
        );
        assert!(
            (lat - original.1).abs() < 0.0001,
            "Latitude round-trip failed: {} vs {}",
            lat,
            original.1
        );
    }

    #[test]
    fn test_round_trip_new_york() {
        // Empire State Building
        let original = (-73.985428, 40.748817);
        let coord = lng_lat_to_world(original.0, original.1);
        let (lng, lat) = world_to_lng_lat(coord);

        assert!(
            (lng - original.0).abs() < 0.0001,
            "Longitude round-trip failed: {} vs {}",
            lng,
            original.0
        );
        assert!(
            (lat - original.1).abs() < 0.0001,
            "Latitude round-trip failed: {} vs {}",
            lat,
            original.1
        );
    }

    #[test]
    fn test_round_trip_sydney() {
        // Sydney Opera House (southern hemisphere)
        let original = (151.215256, -33.856784);
        let coord = lng_lat_to_world(original.0, original.1);
        let (lng, lat) = world_to_lng_lat(coord);

        assert!(
            (lng - original.0).abs() < 0.0001,
            "Longitude round-trip failed: {} vs {}",
            lng,
            original.0
        );
        assert!(
            (lat - original.1).abs() < 0.0001,
            "Latitude round-trip failed: {} vs {}",
            lat,
            original.1
        );
    }

    // ========== Tile Conversion Tests ==========

    #[test]
    fn test_to_tile_zoom_0() {
        // At zoom 0, the entire world is one tile
        let coord = lng_lat_to_world(0.0, 0.0);
        let tile = coord.to_tile(0);

        assert_eq!(tile.x, 0);
        assert_eq!(tile.y, 0);
        assert_eq!(tile.z, 0);
    }

    #[test]
    fn test_to_tile_zoom_1() {
        // At zoom 1, world is divided into 2x2 tiles
        // Null Island (0, 0) is at world coord (2^31, 2^31)
        // At zoom 1, shift by 31, so tile = (1, 1)
        let coord = lng_lat_to_world(0.0, 0.0);
        let tile = coord.to_tile(1);

        assert_eq!(tile.x, 1, "Null Island should be in eastern half at z1");
        assert_eq!(tile.y, 1, "Null Island should be in southern half at z1");
        assert_eq!(tile.z, 1);
    }

    #[test]
    fn test_to_tile_northwest_quadrant() {
        // Northwest: lng=-90, lat=45 should be in tile (0, 0) at zoom 1
        let coord = lng_lat_to_world(-90.0, 45.0);
        let tile = coord.to_tile(1);

        assert_eq!(tile.x, 0, "Western hemisphere should have x=0 at z1");
        assert_eq!(tile.y, 0, "Northern hemisphere should have y=0 at z1");
    }

    #[test]
    fn test_to_tile_southeast_quadrant() {
        // Southeast: lng=90, lat=-45 should be in tile (1, 1) at zoom 1
        let coord = lng_lat_to_world(90.0, -45.0);
        let tile = coord.to_tile(1);

        assert_eq!(tile.x, 1, "Eastern hemisphere should have x=1 at z1");
        assert_eq!(tile.y, 1, "Southern hemisphere should have y=1 at z1");
    }

    // ========== Tile Local Conversion Tests ==========

    #[test]
    fn test_to_tile_local_center() {
        // Point at center of tile 0/0/0 should be at (extent/2, extent/2)
        let coord = lng_lat_to_world(0.0, 0.0);
        let tile = TileCoord::new(0, 0, 0);
        let extent = 4096;

        let (local_x, local_y) = coord.to_tile_local(&tile, extent);

        // Allow for rounding differences (2048 or 2047 are both acceptable)
        assert!(
            (local_x - 2048).abs() <= 1,
            "Center should be near extent/2, got {}",
            local_x
        );
        assert!(
            (local_y - 2048).abs() <= 1,
            "Center should be near extent/2, got {}",
            local_y
        );
    }

    #[test]
    fn test_to_tile_local_origin() {
        // Point at northwest corner of tile should be at (0, 0)
        let coord = lng_lat_to_world(-180.0, MAX_LATITUDE);
        let tile = TileCoord::new(0, 0, 0);
        let extent = 4096;

        let (local_x, local_y) = coord.to_tile_local(&tile, extent);

        assert!(
            (0..10).contains(&local_x),
            "Northwest should be near x=0, got {}",
            local_x
        );
        assert!(
            (0..10).contains(&local_y),
            "Northwest should be near y=0, got {}",
            local_y
        );
    }

    #[test]
    fn test_tile_local_round_trip() {
        let tile = TileCoord::new(1234, 5678, 14);
        let extent = 4096;
        let original_local = (1000, 2000);

        let world = tile_local_to_world(&tile, original_local.0, original_local.1, extent);
        let (local_x, local_y) = world.to_tile_local(&tile, extent);

        assert_eq!(
            local_x, original_local.0,
            "Local X round-trip failed: {} vs {}",
            local_x, original_local.0
        );
        assert_eq!(
            local_y, original_local.1,
            "Local Y round-trip failed: {} vs {}",
            local_y, original_local.1
        );
    }

    // ========== Edge Case Tests ==========

    #[test]
    fn test_latitude_clamping() {
        // Latitudes outside Web Mercator bounds should be clamped
        let coord_north = lng_lat_to_world(0.0, 90.0);
        let coord_max = lng_lat_to_world(0.0, MAX_LATITUDE);

        // Both should result in similar y coordinates (clamped)
        assert!(
            (coord_north.y as i64 - coord_max.y as i64).abs() < 100,
            "Latitude 90 should be clamped to max: {} vs {}",
            coord_north.y,
            coord_max.y
        );

        let coord_south = lng_lat_to_world(0.0, -90.0);
        let coord_min = lng_lat_to_world(0.0, -MAX_LATITUDE);

        assert!(
            (coord_south.y as i64 - coord_min.y as i64).abs() < 100,
            "Latitude -90 should be clamped to min: {} vs {}",
            coord_south.y,
            coord_min.y
        );
    }

    #[test]
    fn test_antimeridian_east() {
        // Points near 180° should work correctly
        let coord = lng_lat_to_world(179.9, 0.0);
        assert!(coord.x > WORLD_HALF, "x should be > 2^31 near 180°");

        let (lng, _) = world_to_lng_lat(coord);
        assert!(
            (lng - 179.9).abs() < 0.001,
            "Longitude near antimeridian: {} vs 179.9",
            lng
        );
    }

    #[test]
    fn test_antimeridian_west() {
        // Points near -180° should work correctly
        let coord = lng_lat_to_world(-179.9, 0.0);
        assert!(coord.x < WORLD_HALF, "x should be < 2^31 near -180°");

        let (lng, _) = world_to_lng_lat(coord);
        assert!(
            (lng - (-179.9)).abs() < 0.001,
            "Longitude near antimeridian: {} vs -179.9",
            lng
        );
    }

    // ========== Consistency with TileCoord Tests ==========

    #[test]
    fn test_consistency_with_tile_coord_bounds() {
        // WorldCoord.to_tile should give same result as lng_lat_to_tile for various points
        use crate::tile::lng_lat_to_tile;

        let test_points = [
            (0.0, 0.0),
            (-73.985428, 40.748817),  // NYC
            (151.215256, -33.856784), // Sydney
            (-122.4194, 37.7749),     // San Francisco
            (139.6917, 35.6895),      // Tokyo
        ];

        for (lng, lat) in test_points {
            for zoom in [0, 5, 10, 14, 18] {
                let world_coord = lng_lat_to_world(lng, lat);
                let tile_from_world = world_coord.to_tile(zoom);
                let tile_direct = lng_lat_to_tile(lng, lat, zoom);

                assert_eq!(
                    tile_from_world, tile_direct,
                    "Tile mismatch for ({}, {}) at z{}: {:?} vs {:?}",
                    lng, lat, zoom, tile_from_world, tile_direct
                );
            }
        }
    }

    // ========== Precision Tests ==========

    #[test]
    fn test_precision_at_high_zoom() {
        // At zoom 20, 1 unit should represent ~0.15 meters
        // Two points 1 meter apart should have different world coordinates
        let lng1 = -73.985428;
        let lat1 = 40.748817;

        // Move ~1 meter east (roughly 0.00001 degrees at this latitude)
        let lng2 = lng1 + 0.00001;

        let coord1 = lng_lat_to_world(lng1, lat1);
        let coord2 = lng_lat_to_world(lng2, lat1);

        // Should be different coordinates
        assert_ne!(
            coord1.x, coord2.x,
            "Points 1m apart should have different x coordinates"
        );

        // At zoom 20, tile extent 4096, this should be distinguishable
        let tile = coord1.to_tile(20);
        let (local1, _) = coord1.to_tile_local(&tile, 4096);
        let (local2, _) = coord2.to_tile_local(&tile, 4096);

        // The difference might be small but should exist
        // (depending on which tile the points fall in)
        // This test mainly verifies no overflow/precision loss
        // If we got here without panic, the conversion succeeded
        let _ = (local1, local2); // Use values to avoid unused warnings
    }

    // ========== World Coordinate Arithmetic Tests ==========

    #[test]
    fn test_world_coord_equality() {
        let c1 = WorldCoord::new(100, 200);
        let c2 = WorldCoord::new(100, 200);
        let c3 = WorldCoord::new(100, 201);

        assert_eq!(c1, c2);
        assert_ne!(c1, c3);
    }

    #[test]
    fn test_world_coord_hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(WorldCoord::new(100, 200));
        set.insert(WorldCoord::new(100, 200)); // duplicate
        set.insert(WorldCoord::new(100, 201));

        assert_eq!(set.len(), 2);
    }

    // ========== WorldBounds Tests ==========

    #[test]
    fn test_world_bounds_from_tile_zoom0() {
        let tile = TileCoord::new(0, 0, 0);
        let bounds = WorldBounds::from_tile(&tile);
        assert_eq!(bounds.x_min, 0);
        assert_eq!(bounds.y_min, 0);
        assert_eq!(bounds.x_max, u32::MAX);
        assert_eq!(bounds.y_max, u32::MAX);
    }

    #[test]
    fn test_world_bounds_from_tile_zoom1() {
        // At zoom 1, tile (0,0) covers the NW quadrant: [0, 2^31)
        let tile = TileCoord::new(0, 0, 1);
        let bounds = WorldBounds::from_tile(&tile);
        assert_eq!(bounds.x_min, 0);
        assert_eq!(bounds.y_min, 0);
        // tile_size = 2^31, so max = 2^31 - 1
        assert_eq!(bounds.x_max, (1u64 << 31) as u32 - 1);
        assert_eq!(bounds.y_max, (1u64 << 31) as u32 - 1);
    }

    #[test]
    fn test_world_bounds_from_tile_zoom1_se() {
        // At zoom 1, tile (1,1) covers the SE quadrant: [2^31, 2^32)
        let tile = TileCoord::new(1, 1, 1);
        let bounds = WorldBounds::from_tile(&tile);
        assert_eq!(bounds.x_min, 1u32 << 31);
        assert_eq!(bounds.y_min, 1u32 << 31);
        assert_eq!(bounds.x_max, u32::MAX);
        assert_eq!(bounds.y_max, u32::MAX);
    }

    #[test]
    fn test_world_bounds_contains_point() {
        let bounds = WorldBounds::new(100, 100, 200, 200);
        assert!(bounds.contains(&WorldCoord::new(150, 150)));
        assert!(bounds.contains(&WorldCoord::new(100, 100))); // on boundary
        assert!(bounds.contains(&WorldCoord::new(200, 200))); // on boundary
        assert!(!bounds.contains(&WorldCoord::new(99, 150)));
        assert!(!bounds.contains(&WorldCoord::new(201, 150)));
    }

    #[test]
    fn test_world_bounds_intersects() {
        let a = WorldBounds::new(0, 0, 100, 100);
        let b = WorldBounds::new(50, 50, 150, 150);
        let c = WorldBounds::new(200, 200, 300, 300);

        assert!(a.intersects(&b));
        assert!(b.intersects(&a));
        assert!(!a.intersects(&c));
        assert!(!c.intersects(&a));
    }

    #[test]
    fn test_world_bounds_contains_bounds() {
        let outer = WorldBounds::new(0, 0, 1000, 1000);
        let inner = WorldBounds::new(100, 100, 500, 500);
        let partial = WorldBounds::new(500, 500, 1500, 1500);

        assert!(outer.contains_bounds(&inner));
        assert!(!inner.contains_bounds(&outer));
        assert!(!outer.contains_bounds(&partial));
    }

    #[test]
    fn test_world_bounds_with_buffer() {
        // Use a tile that is NOT at the world edge, so buffer can expand in all directions
        let tile = TileCoord::new(1, 1, 2);
        let base = WorldBounds::from_tile(&tile);
        let buffered = WorldBounds::from_tile_with_buffer(&tile, 8, 4096);

        // Buffer should expand bounds in all directions
        assert!(
            buffered.x_min < base.x_min,
            "buffered x_min {} should be < base x_min {}",
            buffered.x_min,
            base.x_min
        );
        assert!(
            buffered.y_min < base.y_min,
            "buffered y_min {} should be < base y_min {}",
            buffered.y_min,
            base.y_min
        );
        assert!(
            buffered.x_max > base.x_max,
            "buffered x_max {} should be > base x_max {}",
            buffered.x_max,
            base.x_max
        );
        assert!(
            buffered.y_max > base.y_max,
            "buffered y_max {} should be > base y_max {}",
            buffered.y_max,
            base.y_max
        );

        // Buffer amount: tile_size = 2^30, buffer = 2^30 * 8 / 4096 = 2097152
        let expected_buffer = ((1u64 << 30) * 8 / 4096) as u32;
        assert_eq!(base.x_min - buffered.x_min, expected_buffer);
    }

    #[test]
    fn test_world_bounds_buffer_saturates_at_edges() {
        // Tile at (0,0) with buffer should not underflow below 0
        let tile = TileCoord::new(0, 0, 1);
        let buffered = WorldBounds::from_tile_with_buffer(&tile, 8, 4096);
        assert_eq!(buffered.x_min, 0); // saturated
        assert_eq!(buffered.y_min, 0); // saturated
    }

    #[test]
    fn test_world_bounds_round_trip_via_tile_bounds() {
        // Convert a tile to WorldBounds, then to TileBounds, then back
        let tile = TileCoord::new(4, 3, 3);
        let world_bounds = WorldBounds::from_tile(&tile);
        let tile_bounds = world_bounds.to_tile_bounds();
        let world_bounds_rt = WorldBounds::from_tile_bounds(&tile_bounds);

        // Should be approximately equal (small rounding from f64 conversion)
        let x_diff = (world_bounds.x_min as i64 - world_bounds_rt.x_min as i64).unsigned_abs();
        let y_diff = (world_bounds.y_min as i64 - world_bounds_rt.y_min as i64).unsigned_abs();

        // Allow tolerance of ~100 world units (sub-meter precision loss from f64 round-trip)
        assert!(
            x_diff < 1000,
            "x_min round-trip drift too large: {} vs {} (diff={})",
            world_bounds.x_min,
            world_bounds_rt.x_min,
            x_diff
        );
        assert!(
            y_diff < 1000,
            "y_min round-trip drift too large: {} vs {} (diff={})",
            world_bounds.y_min,
            world_bounds_rt.y_min,
            y_diff
        );
    }
}
