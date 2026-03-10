//! Wagyu-based polygon clipping for robust tile boundary clipping.
//!
//! This module provides coordinate conversion utilities and clipping functions
//! that use wagyu-rs (a Rust port of Mapbox's Vatti clipper) for robust polygon
//! clipping. Wagyu operates in integer coordinates (like tippecanoe), providing
//! numerically robust results compared to floating-point clipping.
//!
//! # Design
//!
//! The workflow is:
//! 1. Convert geo::Polygon<f64> (geographic coords) to wagyu's integer format
//! 2. Perform clipping using wagyu's Vatti algorithm
//! 3. Convert the result back to geo::Geometry<f64>
//!
//! # Coordinate Systems
//!
//! - **Geographic**: f64 lng/lat in degrees
//! - **Tile-local**: i64 coordinates scaled to tile extent (0 to extent, typically 4096)
//!   - Origin (0, 0) is top-left of tile
//!   - X increases rightward (east)
//!   - Y increases downward (south) - note the flip from geographic coords
//!
//! # Reference
//!
//! This matches tippecanoe's approach of using integer coordinates internally
//! for geometric operations to avoid floating-point precision issues.

use crate::tile::TileBounds;
use geo::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use wagyu_rs::{FillType, Operation, Point as WagyuPoint, PolygonType, Wagyu};

/// Default tile extent (matches MVT spec)
pub const DEFAULT_EXTENT: u32 = 4096;

// ============================================================================
// Coordinate Conversion: Geographic <-> Tile-local Integer
// ============================================================================

/// Convert a geographic coordinate (lng, lat) to tile-local integer coordinates.
///
/// The conversion:
/// 1. Normalizes lng/lat to 0-1 within the tile bounds
/// 2. Scales to the tile extent (0 to extent)
/// 3. Flips Y axis (tile coords have Y increasing downward)
///
/// # Arguments
///
/// * `lng` - Longitude in degrees
/// * `lat` - Latitude in degrees
/// * `bounds` - The geographic bounds of the tile
/// * `extent` - The tile extent (typically 4096)
///
/// # Returns
///
/// (x, y) as i64 in tile-local coordinates
#[inline]
fn geo_to_wagyu_coord(lng: f64, lat: f64, bounds: &TileBounds, extent: u32) -> WagyuPoint<i64> {
    let extent_f = extent as f64;

    // Normalize to 0-1 within tile bounds
    let x_ratio = (lng - bounds.lng_min) / (bounds.lng_max - bounds.lng_min);
    let y_ratio = (lat - bounds.lat_min) / (bounds.lat_max - bounds.lat_min);

    // Scale to extent and flip Y (tile coords have Y increasing downward)
    let x = (x_ratio * extent_f).round() as i64;
    let y = ((1.0 - y_ratio) * extent_f).round() as i64;

    WagyuPoint::new(x, y)
}

/// Convert a tile-local integer coordinate back to geographic (lng, lat).
///
/// Inverse of `geo_to_wagyu_coord`.
///
/// # Arguments
///
/// * `x` - X coordinate in tile-local space (0 to extent)
/// * `y` - Y coordinate in tile-local space (0 to extent, Y-down)
/// * `bounds` - The geographic bounds of the tile
/// * `extent` - The tile extent (typically 4096)
///
/// # Returns
///
/// (lng, lat) as f64 in geographic coordinates
#[inline]
fn wagyu_coord_to_geo(x: i64, y: i64, bounds: &TileBounds, extent: u32) -> Coord<f64> {
    let extent_f = extent as f64;

    // Normalize from extent to 0-1
    let x_ratio = x as f64 / extent_f;
    let y_ratio = y as f64 / extent_f;

    // Convert to geographic coords (flip Y back)
    let lng = bounds.lng_min + x_ratio * (bounds.lng_max - bounds.lng_min);
    let lat = bounds.lat_min + (1.0 - y_ratio) * (bounds.lat_max - bounds.lat_min);

    Coord { x: lng, y: lat }
}

// ============================================================================
// Polygon Conversion: geo -> wagyu
// ============================================================================

/// Convert a geo::Polygon<f64> to wagyu's ring format (Vec<Vec<WagyuPoint<i64>>>).
///
/// Returns all rings: exterior ring first, then interior rings (holes).
/// Each ring is a Vec<WagyuPoint<i64>> in wagyu's expected format.
///
/// # Arguments
///
/// * `poly` - The polygon to convert
/// * `bounds` - The tile bounds for coordinate transformation
/// * `extent` - The tile extent (typically 4096)
///
/// # Returns
///
/// A vector of rings, where each ring is a vector of wagyu points.
/// The first ring is the exterior, subsequent rings are holes.
pub fn polygon_to_wagyu_rings(
    poly: &Polygon<f64>,
    bounds: &TileBounds,
    extent: u32,
) -> Vec<Vec<WagyuPoint<i64>>> {
    let mut rings = Vec::with_capacity(1 + poly.interiors().len());

    // Convert exterior ring
    let exterior_ring: Vec<WagyuPoint<i64>> = poly
        .exterior()
        .coords()
        .map(|c| geo_to_wagyu_coord(c.x, c.y, bounds, extent))
        .collect();
    rings.push(exterior_ring);

    // Convert interior rings (holes)
    for interior in poly.interiors() {
        let interior_ring: Vec<WagyuPoint<i64>> = interior
            .coords()
            .map(|c| geo_to_wagyu_coord(c.x, c.y, bounds, extent))
            .collect();
        rings.push(interior_ring);
    }

    rings
}

/// Create a clip box (tile bounds as wagyu ring) for clipping operations.
///
/// Returns a rectangular ring representing the tile extent in integer coordinates.
/// The box is CCW oriented (standard for wagyu subject/clip polygons).
///
/// # Arguments
///
/// * `extent` - The tile extent (typically 4096)
///
/// # Returns
///
/// A vector of wagyu points forming a rectangular clip box
pub fn create_clip_box(extent: u32) -> Vec<WagyuPoint<i64>> {
    let e = extent as i64;
    // CCW rectangle: bottom-left -> bottom-right -> top-right -> top-left
    // In tile coords (Y-down), this is:
    // (0, extent) -> (extent, extent) -> (extent, 0) -> (0, 0)
    vec![
        WagyuPoint::new(0, e), // bottom-left
        WagyuPoint::new(e, e), // bottom-right
        WagyuPoint::new(e, 0), // top-right
        WagyuPoint::new(0, 0), // top-left
    ]
}

/// Create a clip box with buffer for clipping operations.
///
/// The buffer extends the clip box beyond the tile extent on all sides.
///
/// # Arguments
///
/// * `extent` - The tile extent (typically 4096)
/// * `buffer` - Buffer size in tile units (e.g., 8 pixels)
///
/// # Returns
///
/// A vector of wagyu points forming a rectangular clip box with buffer
pub fn create_clip_box_with_buffer(extent: u32, buffer: i64) -> Vec<WagyuPoint<i64>> {
    let e = extent as i64;
    // Extend bounds by buffer on all sides
    vec![
        WagyuPoint::new(-buffer, e + buffer),    // bottom-left
        WagyuPoint::new(e + buffer, e + buffer), // bottom-right
        WagyuPoint::new(e + buffer, -buffer),    // top-right
        WagyuPoint::new(-buffer, -buffer),       // top-left
    ]
}

// ============================================================================
// Polygon Conversion: wagyu -> geo
// ============================================================================

/// Convert wagyu's MultiPolygon<i64> output back to geo::MultiPolygon<f64>.
///
/// # Arguments
///
/// * `mp` - The wagyu output multi-polygon in integer coordinates
/// * `bounds` - The tile bounds for coordinate transformation
/// * `extent` - The tile extent (typically 4096)
///
/// # Returns
///
/// A geo::MultiPolygon<f64> in geographic coordinates
pub fn multipolygon_from_wagyu(
    mp: &geo::MultiPolygon<i64>,
    bounds: &TileBounds,
    extent: u32,
) -> MultiPolygon<f64> {
    let polygons: Vec<Polygon<f64>> =
        mp.0.iter()
            .map(|poly| polygon_from_wagyu(poly, bounds, extent))
            .collect();
    MultiPolygon::new(polygons)
}

/// Convert a single wagyu polygon to geo::Polygon<f64>.
fn polygon_from_wagyu(poly: &geo::Polygon<i64>, bounds: &TileBounds, extent: u32) -> Polygon<f64> {
    // Convert exterior ring
    let exterior_coords: Vec<Coord<f64>> = poly
        .exterior()
        .coords()
        .map(|c| wagyu_coord_to_geo(c.x, c.y, bounds, extent))
        .collect();
    let exterior = LineString::new(exterior_coords);

    // Convert interior rings
    let interiors: Vec<LineString<f64>> = poly
        .interiors()
        .iter()
        .map(|ring| {
            let coords: Vec<Coord<f64>> = ring
                .coords()
                .map(|c| wagyu_coord_to_geo(c.x, c.y, bounds, extent))
                .collect();
            LineString::new(coords)
        })
        .collect();

    Polygon::new(exterior, interiors)
}

// ============================================================================
// Main Clipping Function
// ============================================================================

/// Clip a polygon to tile bounds using wagyu's Vatti algorithm.
///
/// This function:
/// 1. Converts the input polygon to integer tile coordinates
/// 2. Creates a clip box at the tile extent
/// 3. Performs intersection using wagyu's robust Vatti clipper
/// 4. Converts the result back to geographic coordinates
///
/// # Arguments
///
/// * `poly` - The polygon to clip (in geographic coordinates)
/// * `bounds` - The tile bounds for coordinate transformation
/// * `extent` - The tile extent (typically 4096)
///
/// # Returns
///
/// - `Some(Geometry::Polygon)` if clipping results in a single polygon
/// - `Some(Geometry::MultiPolygon)` if clipping results in multiple polygons
/// - `None` if the polygon doesn't intersect the tile bounds
///
/// # Example
///
/// ```ignore
/// use gpq_tiles_core::wagyu_clip::{clip_polygon_wagyu, DEFAULT_EXTENT};
/// use gpq_tiles_core::tile::TileBounds;
/// use geo::Polygon;
///
/// let poly = Polygon::new(/* ... */);
/// let bounds = TileBounds::new(-180.0, -85.0, 180.0, 85.0);
///
/// if let Some(clipped) = clip_polygon_wagyu(&poly, &bounds, DEFAULT_EXTENT) {
///     // Use the clipped geometry
/// }
/// ```
pub fn clip_polygon_wagyu(
    poly: &Polygon<f64>,
    bounds: &TileBounds,
    extent: u32,
) -> Option<Geometry<f64>> {
    clip_polygon_wagyu_with_buffer(poly, bounds, extent, 0)
}

/// Clip a polygon to tile bounds with a buffer using wagyu's Vatti algorithm.
///
/// Same as `clip_polygon_wagyu` but allows specifying a buffer in tile units.
///
/// # Arguments
///
/// * `poly` - The polygon to clip (in geographic coordinates)
/// * `bounds` - The tile bounds for coordinate transformation
/// * `extent` - The tile extent (typically 4096)
/// * `buffer` - Buffer size in tile units (e.g., 8 for 8 pixels at extent 4096)
///
/// # Returns
///
/// - `Some(Geometry::Polygon)` if clipping results in a single polygon
/// - `Some(Geometry::MultiPolygon)` if clipping results in multiple polygons
/// - `None` if the polygon doesn't intersect the buffered tile bounds
pub fn clip_polygon_wagyu_with_buffer(
    poly: &Polygon<f64>,
    bounds: &TileBounds,
    extent: u32,
    buffer: i64,
) -> Option<Geometry<f64>> {
    // Convert polygon to wagyu format
    let subject_rings = polygon_to_wagyu_rings(poly, bounds, extent);

    // Create clip box (with optional buffer)
    let clip_ring = if buffer > 0 {
        create_clip_box_with_buffer(extent, buffer)
    } else {
        create_clip_box(extent)
    };

    // Set up wagyu clipper
    let mut clipper: Wagyu<i64> = Wagyu::new();

    // Add subject polygon (all rings - exterior + holes)
    for ring in &subject_rings {
        if ring.len() >= 3 {
            clipper.add_ring(ring, PolygonType::Subject);
        }
    }

    // Add clip box
    clipper.add_ring(&clip_ring, PolygonType::Clip);

    // Execute intersection
    let result = clipper
        .execute(
            Operation::Intersection,
            FillType::EvenOdd, // Subject fill type
            FillType::EvenOdd, // Clip fill type
        )
        .ok()?;

    // Check if we got any output
    if result.0.is_empty() {
        return None;
    }

    // Convert back to geo format
    let geo_result = multipolygon_from_wagyu(&result, bounds, extent);

    // Return appropriate geometry type
    if geo_result.0.len() == 1 {
        Some(Geometry::Polygon(geo_result.0.into_iter().next().unwrap()))
    } else {
        Some(Geometry::MultiPolygon(geo_result))
    }
}

/// Clip a MultiPolygon to tile bounds using wagyu's Vatti algorithm.
///
/// # Arguments
///
/// * `mp` - The multi-polygon to clip (in geographic coordinates)
/// * `bounds` - The tile bounds for coordinate transformation
/// * `extent` - The tile extent (typically 4096)
///
/// # Returns
///
/// - `Some(Geometry::Polygon)` if clipping results in a single polygon
/// - `Some(Geometry::MultiPolygon)` if clipping results in multiple polygons
/// - `None` if the multi-polygon doesn't intersect the tile bounds
pub fn clip_multipolygon_wagyu(
    mp: &MultiPolygon<f64>,
    bounds: &TileBounds,
    extent: u32,
) -> Option<Geometry<f64>> {
    clip_multipolygon_wagyu_with_buffer(mp, bounds, extent, 0)
}

/// Clip a MultiPolygon to tile bounds with a buffer using wagyu's Vatti algorithm.
pub fn clip_multipolygon_wagyu_with_buffer(
    mp: &MultiPolygon<f64>,
    bounds: &TileBounds,
    extent: u32,
    buffer: i64,
) -> Option<Geometry<f64>> {
    // Set up wagyu clipper
    let mut clipper: Wagyu<i64> = Wagyu::new();

    // Add all polygons as subject
    for poly in &mp.0 {
        let subject_rings = polygon_to_wagyu_rings(poly, bounds, extent);
        for ring in &subject_rings {
            if ring.len() >= 3 {
                clipper.add_ring(ring, PolygonType::Subject);
            }
        }
    }

    // Create clip box (with optional buffer)
    let clip_ring = if buffer > 0 {
        create_clip_box_with_buffer(extent, buffer)
    } else {
        create_clip_box(extent)
    };

    // Add clip box
    clipper.add_ring(&clip_ring, PolygonType::Clip);

    // Execute intersection
    let result = clipper
        .execute(
            Operation::Intersection,
            FillType::EvenOdd,
            FillType::EvenOdd,
        )
        .ok()?;

    // Check if we got any output
    if result.0.is_empty() {
        return None;
    }

    // Convert back to geo format
    let geo_result = multipolygon_from_wagyu(&result, bounds, extent);

    // Return appropriate geometry type
    if geo_result.0.len() == 1 {
        Some(Geometry::Polygon(geo_result.0.into_iter().next().unwrap()))
    } else {
        Some(Geometry::MultiPolygon(geo_result))
    }
}

// ============================================================================
// WorldCoord-based Wagyu Clipping (Phase 1)
// ============================================================================
//
// These functions provide WorldCoord-to-wagyu conversion that bypasses
// the f64 TileBounds normalization step. Since WorldCoord is already in
// a global integer coordinate system, we can convert directly to wagyu's
// tile-local integer coordinates with a simple shift and scale operation.
//
// PHASE 1: Additive -- the f64 versions above remain unchanged.

use crate::world_coord::{WorldBounds, WorldCoord};

/// Convert a WorldCoord to wagyu tile-local integer coordinates.
///
/// This replaces the two-step process of:
/// 1. f64 geographic -> f64 normalized (geo_to_wagyu_coord)
///
/// Instead, it directly converts from world space to tile-local space:
/// `local = (world - tile_origin) * extent / tile_size`
///
/// # Arguments
/// * `coord` - World coordinate to convert
/// * `tile_bounds` - The tile's world bounds
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// WagyuPoint in tile-local integer coordinates
pub fn world_to_wagyu_coord(
    coord: &WorldCoord,
    tile_bounds: &WorldBounds,
    extent: u32,
) -> WagyuPoint<i64> {
    let tile_width = tile_bounds.x_max as i64 - tile_bounds.x_min as i64 + 1;
    let tile_height = tile_bounds.y_max as i64 - tile_bounds.y_min as i64 + 1;
    let extent_i = extent as i64;

    // Convert from world space to tile-local space
    let x = (coord.x as i64 - tile_bounds.x_min as i64) * extent_i / tile_width;
    let y = (coord.y as i64 - tile_bounds.y_min as i64) * extent_i / tile_height;

    WagyuPoint::new(x, y)
}

/// Convert wagyu tile-local integer coordinates back to WorldCoord.
///
/// Inverse of `world_to_wagyu_coord`.
///
/// # Arguments
/// * `x` - X coordinate in tile-local space
/// * `y` - Y coordinate in tile-local space
/// * `tile_bounds` - The tile's world bounds
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// WorldCoord in global space
pub fn wagyu_to_world_coord(x: i64, y: i64, tile_bounds: &WorldBounds, extent: u32) -> WorldCoord {
    let tile_width = tile_bounds.x_max as i64 - tile_bounds.x_min as i64 + 1;
    let tile_height = tile_bounds.y_max as i64 - tile_bounds.y_min as i64 + 1;
    let extent_i = extent as i64;

    let world_x = tile_bounds.x_min as i64 + x * tile_width / extent_i;
    let world_y = tile_bounds.y_min as i64 + y * tile_height / extent_i;

    WorldCoord::new(
        world_x.clamp(0, u32::MAX as i64) as u32,
        world_y.clamp(0, u32::MAX as i64) as u32,
    )
}

/// Convert WorldCoord polygon rings to wagyu format.
///
/// This is the WorldCoord equivalent of `polygon_to_wagyu_rings`.
pub fn world_polygon_to_wagyu_rings(
    exterior: &[WorldCoord],
    interiors: &[Vec<WorldCoord>],
    tile_bounds: &WorldBounds,
    extent: u32,
) -> Vec<Vec<WagyuPoint<i64>>> {
    let mut rings = Vec::with_capacity(1 + interiors.len());

    let exterior_ring: Vec<WagyuPoint<i64>> = exterior
        .iter()
        .map(|c| world_to_wagyu_coord(c, tile_bounds, extent))
        .collect();
    rings.push(exterior_ring);

    for interior in interiors {
        let interior_ring: Vec<WagyuPoint<i64>> = interior
            .iter()
            .map(|c| world_to_wagyu_coord(c, tile_bounds, extent))
            .collect();
        rings.push(interior_ring);
    }

    rings
}

/// Clip a polygon in WorldCoord space using wagyu's Vatti algorithm.
///
/// This is the WorldCoord equivalent of `clip_polygon_wagyu`.
/// It converts WorldCoord points to wagyu tile-local coordinates,
/// performs the clipping, then converts back.
///
/// # Arguments
/// * `exterior` - Exterior ring as WorldCoord points
/// * `interiors` - Interior rings (holes) as WorldCoord points
/// * `tile_bounds` - The tile's world bounds
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// Clipped exterior and interior rings in WorldCoord space, or None
pub fn clip_polygon_wagyu_world(
    exterior: &[WorldCoord],
    interiors: &[Vec<WorldCoord>],
    tile_bounds: &WorldBounds,
    extent: u32,
) -> Option<(Vec<WorldCoord>, Vec<Vec<WorldCoord>>)> {
    clip_polygon_wagyu_world_with_buffer(exterior, interiors, tile_bounds, extent, 0)
}

/// Clip a polygon in WorldCoord space with buffer using wagyu.
pub fn clip_polygon_wagyu_world_with_buffer(
    exterior: &[WorldCoord],
    interiors: &[Vec<WorldCoord>],
    tile_bounds: &WorldBounds,
    extent: u32,
    buffer: i64,
) -> Option<(Vec<WorldCoord>, Vec<Vec<WorldCoord>>)> {
    let subject_rings = world_polygon_to_wagyu_rings(exterior, interiors, tile_bounds, extent);

    let clip_ring = if buffer > 0 {
        create_clip_box_with_buffer(extent, buffer)
    } else {
        create_clip_box(extent)
    };

    let mut clipper: Wagyu<i64> = Wagyu::new();

    for ring in &subject_rings {
        if ring.len() >= 3 {
            clipper.add_ring(ring, PolygonType::Subject);
        }
    }

    clipper.add_ring(&clip_ring, PolygonType::Clip);

    let result = clipper
        .execute(
            Operation::Intersection,
            FillType::EvenOdd,
            FillType::EvenOdd,
        )
        .ok()?;

    if result.0.is_empty() {
        return None;
    }

    // Convert back to WorldCoord
    let mut all_exteriors: Vec<WorldCoord> = Vec::new();
    let mut all_interiors: Vec<Vec<WorldCoord>> = Vec::new();

    for poly in &result.0 {
        let ext_coords: Vec<WorldCoord> = poly
            .exterior()
            .coords()
            .map(|c| wagyu_to_world_coord(c.x, c.y, tile_bounds, extent))
            .collect();

        if all_exteriors.is_empty() {
            // First polygon becomes the exterior
            all_exteriors = ext_coords;

            // Add its holes
            for interior in poly.interiors() {
                let int_coords: Vec<WorldCoord> = interior
                    .coords()
                    .map(|c| wagyu_to_world_coord(c.x, c.y, tile_bounds, extent))
                    .collect();
                all_interiors.push(int_coords);
            }
        } else {
            // Additional polygons are treated as separate results
            // For simplicity in Phase 1, we just return the first polygon
            // Phase 2 will handle multi-polygon results properly
        }
    }

    if all_exteriors.is_empty() {
        None
    } else {
        Some((all_exteriors, all_interiors))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use geo::polygon;

    fn test_bounds() -> TileBounds {
        TileBounds::new(0.0, 0.0, 1.0, 1.0)
    }

    // ------------------------------------------------------------------------
    // Coordinate Conversion Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_geo_to_wagyu_coord_center() {
        let bounds = test_bounds();
        let point = geo_to_wagyu_coord(0.5, 0.5, &bounds, 4096);
        assert_eq!(point.x, 2048);
        assert_eq!(point.y, 2048);
    }

    #[test]
    fn test_geo_to_wagyu_coord_origin() {
        let bounds = test_bounds();
        // Geographic origin (lng_min, lat_min) should map to (0, extent)
        // because Y is flipped (lat_min is bottom in geo, but bottom = extent in tile coords)
        let point = geo_to_wagyu_coord(0.0, 0.0, &bounds, 4096);
        assert_eq!(point.x, 0);
        assert_eq!(point.y, 4096);
    }

    #[test]
    fn test_geo_to_wagyu_coord_top_right() {
        let bounds = test_bounds();
        // Top-right in geo (lng_max, lat_max) should map to (extent, 0) in tile coords
        let point = geo_to_wagyu_coord(1.0, 1.0, &bounds, 4096);
        assert_eq!(point.x, 4096);
        assert_eq!(point.y, 0);
    }

    #[test]
    fn test_wagyu_coord_to_geo_roundtrip() {
        let bounds = test_bounds();
        let extent = 4096;

        // Test various points
        let test_cases = [(0.0, 0.0), (0.5, 0.5), (1.0, 1.0), (0.25, 0.75), (0.1, 0.9)];

        for (lng, lat) in test_cases {
            let wagyu_pt = geo_to_wagyu_coord(lng, lat, &bounds, extent);
            let geo_pt = wagyu_coord_to_geo(wagyu_pt.x, wagyu_pt.y, &bounds, extent);

            // Allow small floating-point tolerance due to rounding
            assert!(
                (geo_pt.x - lng).abs() < 0.001,
                "lng roundtrip failed: {} -> {} -> {}",
                lng,
                wagyu_pt.x,
                geo_pt.x
            );
            assert!(
                (geo_pt.y - lat).abs() < 0.001,
                "lat roundtrip failed: {} -> {} -> {}",
                lat,
                wagyu_pt.y,
                geo_pt.y
            );
        }
    }

    // ------------------------------------------------------------------------
    // Clip Box Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_create_clip_box() {
        let clip_box = create_clip_box(4096);
        assert_eq!(clip_box.len(), 4);

        // Check corners
        assert_eq!(clip_box[0], WagyuPoint::new(0, 4096)); // bottom-left
        assert_eq!(clip_box[1], WagyuPoint::new(4096, 4096)); // bottom-right
        assert_eq!(clip_box[2], WagyuPoint::new(4096, 0)); // top-right
        assert_eq!(clip_box[3], WagyuPoint::new(0, 0)); // top-left
    }

    #[test]
    fn test_create_clip_box_with_buffer() {
        let clip_box = create_clip_box_with_buffer(4096, 8);
        assert_eq!(clip_box.len(), 4);

        // Check corners (should be extended by buffer)
        assert_eq!(clip_box[0], WagyuPoint::new(-8, 4104)); // bottom-left
        assert_eq!(clip_box[1], WagyuPoint::new(4104, 4104)); // bottom-right
        assert_eq!(clip_box[2], WagyuPoint::new(4104, -8)); // top-right
        assert_eq!(clip_box[3], WagyuPoint::new(-8, -8)); // top-left
    }

    // ------------------------------------------------------------------------
    // Polygon Conversion Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_polygon_to_wagyu_rings_simple() {
        let bounds = test_bounds();
        let poly = polygon![
            (x: 0.25, y: 0.25),
            (x: 0.75, y: 0.25),
            (x: 0.75, y: 0.75),
            (x: 0.25, y: 0.75),
            (x: 0.25, y: 0.25),
        ];

        let rings = polygon_to_wagyu_rings(&poly, &bounds, 4096);
        assert_eq!(rings.len(), 1); // Just exterior, no holes

        let exterior = &rings[0];
        assert_eq!(exterior.len(), 5); // 4 corners + closing point

        // Check that coordinates are in expected range
        for pt in exterior {
            assert!(pt.x >= 0 && pt.x <= 4096, "x out of range: {}", pt.x);
            assert!(pt.y >= 0 && pt.y <= 4096, "y out of range: {}", pt.y);
        }
    }

    #[test]
    fn test_polygon_to_wagyu_rings_with_hole() {
        let bounds = test_bounds();
        let poly = polygon![
            exterior: [
                (x: 0.1, y: 0.1),
                (x: 0.9, y: 0.1),
                (x: 0.9, y: 0.9),
                (x: 0.1, y: 0.9),
                (x: 0.1, y: 0.1),
            ],
            interiors: [
                [
                    (x: 0.3, y: 0.3),
                    (x: 0.7, y: 0.3),
                    (x: 0.7, y: 0.7),
                    (x: 0.3, y: 0.7),
                    (x: 0.3, y: 0.3),
                ],
            ],
        ];

        let rings = polygon_to_wagyu_rings(&poly, &bounds, 4096);
        assert_eq!(rings.len(), 2); // Exterior + 1 hole
    }

    // ------------------------------------------------------------------------
    // Clipping Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_clip_polygon_fully_inside() {
        let bounds = test_bounds();
        let poly = polygon![
            (x: 0.25, y: 0.25),
            (x: 0.75, y: 0.25),
            (x: 0.75, y: 0.75),
            (x: 0.25, y: 0.75),
            (x: 0.25, y: 0.25),
        ];

        let result = clip_polygon_wagyu(&poly, &bounds, 4096);
        assert!(result.is_some(), "Polygon fully inside should be clipped");

        // Should return a polygon (not empty)
        match result.unwrap() {
            Geometry::Polygon(p) => {
                assert!(p.exterior().coords().count() >= 4);
            }
            Geometry::MultiPolygon(mp) => {
                assert!(!mp.0.is_empty());
            }
            _ => panic!("Expected Polygon or MultiPolygon"),
        }
    }

    #[test]
    fn test_clip_polygon_fully_outside() {
        let bounds = test_bounds();
        // Polygon completely outside the tile bounds
        let poly = polygon![
            (x: 2.0, y: 2.0),
            (x: 3.0, y: 2.0),
            (x: 3.0, y: 3.0),
            (x: 2.0, y: 3.0),
            (x: 2.0, y: 2.0),
        ];

        let result = clip_polygon_wagyu(&poly, &bounds, 4096);
        assert!(result.is_none(), "Polygon fully outside should return None");
    }

    #[test]
    fn test_clip_polygon_partial_overlap() {
        let bounds = test_bounds();
        // Polygon overlapping tile boundary
        let poly = polygon![
            (x: 0.5, y: 0.5),
            (x: 1.5, y: 0.5),
            (x: 1.5, y: 1.5),
            (x: 0.5, y: 1.5),
            (x: 0.5, y: 0.5),
        ];

        let result = clip_polygon_wagyu(&poly, &bounds, 4096);
        assert!(result.is_some(), "Partial overlap should produce result");

        // Verify the clipped polygon is within bounds (approximately)
        match result.unwrap() {
            Geometry::Polygon(p) => {
                for coord in p.exterior().coords() {
                    // Allow small tolerance for coordinate conversion
                    assert!(
                        coord.x >= -0.01 && coord.x <= 1.01,
                        "x out of bounds: {}",
                        coord.x
                    );
                    assert!(
                        coord.y >= -0.01 && coord.y <= 1.01,
                        "y out of bounds: {}",
                        coord.y
                    );
                }
            }
            Geometry::MultiPolygon(mp) => {
                for poly in &mp.0 {
                    for coord in poly.exterior().coords() {
                        assert!(
                            coord.x >= -0.01 && coord.x <= 1.01,
                            "x out of bounds: {}",
                            coord.x
                        );
                        assert!(
                            coord.y >= -0.01 && coord.y <= 1.01,
                            "y out of bounds: {}",
                            coord.y
                        );
                    }
                }
            }
            _ => panic!("Expected Polygon or MultiPolygon"),
        }
    }

    #[test]
    fn test_clip_polygon_with_buffer() {
        let bounds = test_bounds();
        // Polygon just outside tile bounds
        let poly = polygon![
            (x: 1.001, y: 0.5),
            (x: 1.1, y: 0.5),
            (x: 1.1, y: 0.6),
            (x: 1.001, y: 0.6),
            (x: 1.001, y: 0.5),
        ];

        // Without buffer, should return None
        let result_no_buffer = clip_polygon_wagyu(&poly, &bounds, 4096);
        assert!(
            result_no_buffer.is_none(),
            "Polygon outside without buffer should return None"
        );

        // With buffer of 8 pixels, should return clipped result
        // 8 pixels / 4096 extent = ~0.002 in normalized coords
        // Since our polygon starts at 1.001, a buffer of ~10 pixels should include it
        let result_with_buffer = clip_polygon_wagyu_with_buffer(&poly, &bounds, 4096, 10);
        assert!(
            result_with_buffer.is_some(),
            "Polygon within buffer zone should be included"
        );
    }

    #[test]
    fn test_clip_multipolygon() {
        let bounds = test_bounds();
        let mp = MultiPolygon::new(vec![
            polygon![
                (x: 0.1, y: 0.1),
                (x: 0.3, y: 0.1),
                (x: 0.3, y: 0.3),
                (x: 0.1, y: 0.3),
                (x: 0.1, y: 0.1),
            ],
            polygon![
                (x: 0.7, y: 0.7),
                (x: 0.9, y: 0.7),
                (x: 0.9, y: 0.9),
                (x: 0.7, y: 0.9),
                (x: 0.7, y: 0.7),
            ],
        ]);

        let result = clip_multipolygon_wagyu(&mp, &bounds, 4096);
        assert!(result.is_some(), "MultiPolygon should produce result");
    }

    #[test]
    fn test_clip_polygon_u_shape_creates_multipolygon() {
        // U-shaped polygon clipped by horizontal band may create multiple parts
        // (depending on where the clip intersects)
        let bounds = TileBounds::new(0.0, 0.4, 1.0, 0.6); // Horizontal band

        // U-shape with opening at top
        let poly = polygon![
            (x: 0.1, y: 0.0),
            (x: 0.2, y: 0.0),
            (x: 0.2, y: 1.0),
            (x: 0.1, y: 1.0),
            (x: 0.1, y: 0.2),
            (x: 0.8, y: 0.2),
            (x: 0.8, y: 1.0),
            (x: 0.9, y: 1.0),
            (x: 0.9, y: 0.0),
            (x: 0.1, y: 0.0),
        ];

        let result = clip_polygon_wagyu(&poly, &bounds, 4096);
        // Should produce some output (either Polygon or MultiPolygon)
        assert!(result.is_some(), "U-shape should intersect horizontal band");
    }

    // ========================================================================
    // WorldCoord-based wagyu conversion and clipping tests
    // ========================================================================

    mod world_tests {
        use super::*;
        use crate::tile::TileCoord;
        use crate::world_coord::{WorldBounds, WorldCoord};

        #[test]
        fn test_world_to_wagyu_coord_center() {
            // A point at the center of the tile should map to (extent/2, extent/2)
            let tile = TileCoord::new(1, 1, 2);
            let tile_bounds = WorldBounds::from_tile(&tile);

            let center_x = (tile_bounds.x_min as u64 + tile_bounds.x_max as u64) / 2;
            let center_y = (tile_bounds.y_min as u64 + tile_bounds.y_max as u64) / 2;
            let center = WorldCoord::new(center_x as u32, center_y as u32);

            let wagyu_pt = world_to_wagyu_coord(&center, &tile_bounds, 4096);

            // Should be approximately at (2048, 2048)
            assert!(
                (wagyu_pt.x - 2048).abs() <= 1,
                "Center x should be ~2048, got {}",
                wagyu_pt.x
            );
            assert!(
                (wagyu_pt.y - 2048).abs() <= 1,
                "Center y should be ~2048, got {}",
                wagyu_pt.y
            );
        }

        #[test]
        fn test_world_to_wagyu_coord_origin() {
            // A point at the tile's top-left corner should map to (0, 0)
            let tile = TileCoord::new(1, 1, 2);
            let tile_bounds = WorldBounds::from_tile(&tile);

            let origin = WorldCoord::new(tile_bounds.x_min, tile_bounds.y_min);
            let wagyu_pt = world_to_wagyu_coord(&origin, &tile_bounds, 4096);

            assert_eq!(wagyu_pt.x, 0, "Origin x should be 0");
            assert_eq!(wagyu_pt.y, 0, "Origin y should be 0");
        }

        #[test]
        fn test_world_wagyu_roundtrip() {
            let tile = TileCoord::new(5, 3, 4);
            let tile_bounds = WorldBounds::from_tile(&tile);
            let extent = 4096;

            // Test various points within the tile
            let test_points = [
                WorldCoord::new(
                    tile_bounds.x_min + (tile_bounds.width() / 4),
                    tile_bounds.y_min + (tile_bounds.height() / 4),
                ),
                WorldCoord::new(
                    tile_bounds.x_min + (tile_bounds.width() / 2),
                    tile_bounds.y_min + (tile_bounds.height() / 2),
                ),
                WorldCoord::new(
                    tile_bounds.x_min + (tile_bounds.width() * 3 / 4),
                    tile_bounds.y_min + (tile_bounds.height() * 3 / 4),
                ),
            ];

            for original in &test_points {
                let wagyu_pt = world_to_wagyu_coord(original, &tile_bounds, extent);
                let back = wagyu_to_world_coord(wagyu_pt.x, wagyu_pt.y, &tile_bounds, extent);

                // Allow small rounding error (1 world unit at most)
                let x_diff = (original.x as i64 - back.x as i64).unsigned_abs();
                let y_diff = (original.y as i64 - back.y as i64).unsigned_abs();

                // tile_size / extent = quantization step size
                let max_error = (tile_bounds.width() / extent) as u64 + 1;

                assert!(
                    x_diff <= max_error,
                    "x round-trip error too large: {} vs {} (diff={}, max_error={})",
                    original.x,
                    back.x,
                    x_diff,
                    max_error
                );
                assert!(
                    y_diff <= max_error,
                    "y round-trip error too large: {} vs {} (diff={}, max_error={})",
                    original.y,
                    back.y,
                    y_diff,
                    max_error
                );
            }
        }

        #[test]
        fn test_clip_polygon_wagyu_world_fully_inside() {
            let tile = TileCoord::new(2, 2, 3);
            let tile_bounds = WorldBounds::from_tile(&tile);

            // Polygon well inside the tile
            let quarter_w = tile_bounds.width() / 4;
            let quarter_h = tile_bounds.height() / 4;

            let exterior = vec![
                WorldCoord::new(tile_bounds.x_min + quarter_w, tile_bounds.y_min + quarter_h),
                WorldCoord::new(
                    tile_bounds.x_min + 3 * quarter_w,
                    tile_bounds.y_min + quarter_h,
                ),
                WorldCoord::new(
                    tile_bounds.x_min + 3 * quarter_w,
                    tile_bounds.y_min + 3 * quarter_h,
                ),
                WorldCoord::new(
                    tile_bounds.x_min + quarter_w,
                    tile_bounds.y_min + 3 * quarter_h,
                ),
                WorldCoord::new(tile_bounds.x_min + quarter_w, tile_bounds.y_min + quarter_h),
            ];

            let result = clip_polygon_wagyu_world(&exterior, &[], &tile_bounds, 4096);
            assert!(
                result.is_some(),
                "Fully inside polygon should be clipped (kept)"
            );
        }

        #[test]
        fn test_clip_polygon_wagyu_world_fully_outside() {
            let tile = TileCoord::new(2, 2, 3);
            let tile_bounds = WorldBounds::from_tile(&tile);

            // Polygon completely outside -- use a tile far away
            let far_tile = TileCoord::new(6, 6, 3);
            let far_bounds = WorldBounds::from_tile(&far_tile);

            let exterior = vec![
                WorldCoord::new(far_bounds.x_min + 100, far_bounds.y_min + 100),
                WorldCoord::new(far_bounds.x_max - 100, far_bounds.y_min + 100),
                WorldCoord::new(far_bounds.x_max - 100, far_bounds.y_max - 100),
                WorldCoord::new(far_bounds.x_min + 100, far_bounds.y_max - 100),
                WorldCoord::new(far_bounds.x_min + 100, far_bounds.y_min + 100),
            ];

            let result = clip_polygon_wagyu_world(&exterior, &[], &tile_bounds, 4096);
            assert!(result.is_none(), "Fully outside polygon should return None");
        }

        #[test]
        fn test_clip_polygon_wagyu_world_partial() {
            let tile = TileCoord::new(2, 2, 3);
            let tile_bounds = WorldBounds::from_tile(&tile);

            // Polygon spanning across the right edge
            let half_w = tile_bounds.width() / 2;
            let quarter_h = tile_bounds.height() / 4;

            let exterior = vec![
                WorldCoord::new(tile_bounds.x_min + half_w, tile_bounds.y_min + quarter_h),
                WorldCoord::new(
                    tile_bounds.x_max + half_w, // extends beyond
                    tile_bounds.y_min + quarter_h,
                ),
                WorldCoord::new(
                    tile_bounds.x_max + half_w,
                    tile_bounds.y_min + 3 * quarter_h,
                ),
                WorldCoord::new(
                    tile_bounds.x_min + half_w,
                    tile_bounds.y_min + 3 * quarter_h,
                ),
                WorldCoord::new(tile_bounds.x_min + half_w, tile_bounds.y_min + quarter_h),
            ];

            let result = clip_polygon_wagyu_world(&exterior, &[], &tile_bounds, 4096);
            assert!(result.is_some(), "Partial overlap should produce output");
        }
    }
}
