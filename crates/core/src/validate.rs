//! Geometry validation for post-simplification degenerate geometry detection.
//!
//! After simplification, geometries can become degenerate (invalid for MVT encoding):
//! - Polygons with fewer than 4 points (need 3 unique + closing point)
//! - LineStrings with fewer than 2 points
//! - Zero-area polygons (all points collinear or coincident)
//! - Empty geometries
//!
//! # Tippecanoe Behavior
//!
//! Tippecanoe silently drops degenerate geometries rather than attempting repair.
//! This is the approach we follow - simple filtering is preferred over complex repair logic.
//!
//! # Usage
//!
//! ```
//! use gpq_tiles_core::validate::is_valid_geometry;
//! use geo::{Geometry, LineString, Coord};
//!
//! let line = LineString::new(vec![
//!     Coord { x: 0.0, y: 0.0 },
//!     Coord { x: 1.0, y: 1.0 },
//! ]);
//! assert!(is_valid_geometry(&Geometry::LineString(line)));
//! ```

use crate::tile::TileCoord;
use crate::world_coord::WorldCoord;
use geo::{Area, Geometry, LineString, MultiLineString, MultiPolygon, Polygon};

/// Minimum number of points for a valid polygon ring (3 unique + closing = 4)
pub const MIN_POLYGON_RING_POINTS: usize = 4;

/// Minimum number of points for a valid linestring
pub const MIN_LINESTRING_POINTS: usize = 2;

/// Minimum area threshold for polygons (in tile coordinates squared)
/// Polygons with area smaller than this are considered degenerate.
/// This is separate from the "tiny polygon" dropping which uses diffuse probability.
pub const MIN_POLYGON_AREA: f64 = 1e-10;

/// Result of geometry validation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// Geometry is valid and can be encoded
    Valid,
    /// Geometry is invalid and should be dropped
    Invalid(InvalidReason),
}

/// Reason why a geometry is invalid
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvalidReason {
    /// Polygon ring has fewer than 4 points
    PolygonTooFewPoints {
        ring_index: usize,
        point_count: usize,
    },
    /// LineString has fewer than 2 points
    LineStringTooFewPoints { point_count: usize },
    /// Polygon has zero or near-zero area (degenerate)
    ZeroAreaPolygon,
    /// Geometry is empty (no coordinates)
    EmptyGeometry,
    /// MultiPolygon has no valid polygons after filtering
    NoValidPolygons,
    /// MultiLineString has no valid linestrings after filtering
    NoValidLineStrings,
}

impl ValidationResult {
    /// Returns true if the geometry is valid
    pub fn is_valid(&self) -> bool {
        matches!(self, ValidationResult::Valid)
    }

    /// Returns true if the geometry is invalid
    pub fn is_invalid(&self) -> bool {
        matches!(self, ValidationResult::Invalid(_))
    }
}

/// Check if a geometry is valid for MVT encoding.
///
/// Returns `true` if the geometry is valid, `false` if it should be dropped.
///
/// # Arguments
/// * `geom` - The geometry to validate
///
/// # Returns
/// `true` if valid, `false` if degenerate/invalid
pub fn is_valid_geometry(geom: &Geometry<f64>) -> bool {
    validate_geometry(geom).is_valid()
}

/// Validate a geometry and return detailed validation result.
///
/// # Arguments
/// * `geom` - The geometry to validate
///
/// # Returns
/// `ValidationResult::Valid` if the geometry can be encoded,
/// `ValidationResult::Invalid(reason)` if it should be dropped
pub fn validate_geometry(geom: &Geometry<f64>) -> ValidationResult {
    match geom {
        Geometry::Point(_) => ValidationResult::Valid, // Points are always valid
        Geometry::MultiPoint(mp) => {
            if mp.0.is_empty() {
                ValidationResult::Invalid(InvalidReason::EmptyGeometry)
            } else {
                ValidationResult::Valid
            }
        }
        Geometry::LineString(ls) => validate_linestring(ls),
        Geometry::MultiLineString(mls) => validate_multi_linestring(mls),
        Geometry::Polygon(poly) => validate_polygon(poly),
        Geometry::MultiPolygon(mp) => validate_multi_polygon(mp),
        // GeometryCollection and other types - pass through as valid
        // (they may contain valid sub-geometries)
        _ => ValidationResult::Valid,
    }
}

/// Validate a LineString geometry.
pub fn validate_linestring(ls: &LineString<f64>) -> ValidationResult {
    let point_count = ls.0.len();
    if point_count < MIN_LINESTRING_POINTS {
        ValidationResult::Invalid(InvalidReason::LineStringTooFewPoints { point_count })
    } else {
        ValidationResult::Valid
    }
}

/// Validate a MultiLineString geometry.
pub fn validate_multi_linestring(mls: &MultiLineString<f64>) -> ValidationResult {
    if mls.0.is_empty() {
        return ValidationResult::Invalid(InvalidReason::EmptyGeometry);
    }

    // Check if at least one linestring is valid
    let has_valid = mls.0.iter().any(|ls| validate_linestring(ls).is_valid());

    if has_valid {
        ValidationResult::Valid
    } else {
        ValidationResult::Invalid(InvalidReason::NoValidLineStrings)
    }
}

/// Validate a Polygon geometry.
pub fn validate_polygon(poly: &Polygon<f64>) -> ValidationResult {
    // Check exterior ring has enough points
    let exterior_count = poly.exterior().0.len();
    if exterior_count < MIN_POLYGON_RING_POINTS {
        return ValidationResult::Invalid(InvalidReason::PolygonTooFewPoints {
            ring_index: 0,
            point_count: exterior_count,
        });
    }

    // Check interior rings have enough points
    for (idx, interior) in poly.interiors().iter().enumerate() {
        let interior_count = interior.0.len();
        if interior_count < MIN_POLYGON_RING_POINTS {
            return ValidationResult::Invalid(InvalidReason::PolygonTooFewPoints {
                ring_index: idx + 1, // +1 because exterior is ring 0
                point_count: interior_count,
            });
        }
    }

    // Check for zero-area polygon
    let area = poly.unsigned_area();
    if area < MIN_POLYGON_AREA {
        return ValidationResult::Invalid(InvalidReason::ZeroAreaPolygon);
    }

    ValidationResult::Valid
}

/// Validate a MultiPolygon geometry.
pub fn validate_multi_polygon(mp: &MultiPolygon<f64>) -> ValidationResult {
    if mp.0.is_empty() {
        return ValidationResult::Invalid(InvalidReason::EmptyGeometry);
    }

    // Check if at least one polygon is valid
    let has_valid = mp.0.iter().any(|poly| validate_polygon(poly).is_valid());

    if has_valid {
        ValidationResult::Valid
    } else {
        ValidationResult::Invalid(InvalidReason::NoValidPolygons)
    }
}

/// Filter a geometry, returning `Some(geometry)` if valid, `None` if invalid.
///
/// For multi-geometries, this filters out invalid components and returns
/// a new geometry with only the valid parts.
///
/// # Arguments
/// * `geom` - The geometry to filter
///
/// # Returns
/// `Some(geometry)` if the geometry (or part of it) is valid, `None` if completely invalid
pub fn filter_valid_geometry(geom: &Geometry<f64>) -> Option<Geometry<f64>> {
    match geom {
        Geometry::Point(_) => Some(geom.clone()),
        Geometry::MultiPoint(mp) => {
            if mp.0.is_empty() {
                None
            } else {
                Some(geom.clone())
            }
        }
        Geometry::LineString(ls) => {
            if validate_linestring(ls).is_valid() {
                Some(geom.clone())
            } else {
                None
            }
        }
        Geometry::MultiLineString(mls) => filter_multi_linestring(mls),
        Geometry::Polygon(poly) => {
            if validate_polygon(poly).is_valid() {
                Some(geom.clone())
            } else {
                None
            }
        }
        Geometry::MultiPolygon(mp) => filter_multi_polygon(mp),
        // Other types pass through unchanged
        other => Some(other.clone()),
    }
}

/// Filter a MultiLineString, keeping only valid linestrings.
fn filter_multi_linestring(mls: &MultiLineString<f64>) -> Option<Geometry<f64>> {
    let valid_lines: Vec<LineString<f64>> = mls
        .0
        .iter()
        .filter(|ls| validate_linestring(ls).is_valid())
        .cloned()
        .collect();

    if valid_lines.is_empty() {
        None
    } else if valid_lines.len() == 1 {
        // Downgrade to single LineString if only one remains
        Some(Geometry::LineString(
            valid_lines.into_iter().next().unwrap(),
        ))
    } else {
        Some(Geometry::MultiLineString(MultiLineString::new(valid_lines)))
    }
}

/// Filter a MultiPolygon, keeping only valid polygons.
fn filter_multi_polygon(mp: &MultiPolygon<f64>) -> Option<Geometry<f64>> {
    let valid_polygons: Vec<Polygon<f64>> =
        mp.0.iter()
            .filter(|poly| validate_polygon(poly).is_valid())
            .cloned()
            .collect();

    if valid_polygons.is_empty() {
        None
    } else if valid_polygons.len() == 1 {
        // Downgrade to single Polygon if only one remains
        Some(Geometry::Polygon(
            valid_polygons.into_iter().next().unwrap(),
        ))
    } else {
        Some(Geometry::MultiPolygon(MultiPolygon::new(valid_polygons)))
    }
}

// =========================================================================
// WorldCoord Validation
// =========================================================================
//
// These functions validate geometries represented as WorldCoord sequences,
// which is the integer-coordinate representation used in the tiling pipeline.
// This allows validation to happen before MVT encoding, using the same
// coordinate system that will be used for encoding.

/// Result of WorldCoord validation
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldCoordValidation {
    /// Coordinates are valid
    Valid,
    /// Coordinates are invalid
    Invalid(WorldCoordInvalidReason),
}

/// Reason why WorldCoord geometry is invalid
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldCoordInvalidReason {
    /// Too few coordinates for the geometry type
    TooFewCoords { required: usize, actual: usize },
    /// All tile-local coordinates collapse to the same pixel (degenerate)
    DegenerateInTile,
    /// Ring has zero area in tile-local coordinates (collinear or coincident)
    ZeroAreaRing,
    /// Empty coordinate sequence
    Empty,
}

impl WorldCoordValidation {
    /// Returns true if valid
    pub fn is_valid(&self) -> bool {
        matches!(self, WorldCoordValidation::Valid)
    }
}

/// Check if a WorldCoord falls within a specific tile.
///
/// This is useful for verifying that a coordinate actually belongs to the
/// tile it is being encoded into.
///
/// # Arguments
/// * `coord` - World coordinate to check
/// * `tile` - The tile to check containment in
///
/// # Returns
/// `true` if the coordinate falls within the tile bounds
pub fn world_coord_in_tile(coord: &WorldCoord, tile: &TileCoord) -> bool {
    if tile.z == 0 {
        // At zoom 0, the entire world is one tile
        return true;
    }

    let shift = 32 - tile.z as u32;

    // Tile bounds in world coordinates
    let tile_min_x = (tile.x as u64) << shift;
    let tile_max_x = ((tile.x + 1) as u64) << shift;
    let tile_min_y = (tile.y as u64) << shift;
    let tile_max_y = ((tile.y + 1) as u64) << shift;

    let cx = coord.x as u64;
    let cy = coord.y as u64;

    cx >= tile_min_x && cx < tile_max_x && cy >= tile_min_y && cy < tile_max_y
}

/// Check if a WorldCoord falls within a tile's buffered region.
///
/// Buffer extends the tile bounds by the given number of extent units.
/// This is useful for including features that extend slightly beyond
/// tile boundaries for seamless rendering.
///
/// # Arguments
/// * `coord` - World coordinate to check
/// * `tile` - The tile
/// * `extent` - Tile extent (typically 4096)
/// * `buffer` - Buffer in extent units (e.g., 64 for ~1.5% buffer)
///
/// # Returns
/// `true` if the coordinate falls within the buffered tile bounds
pub fn world_coord_in_tile_buffer(
    coord: &WorldCoord,
    tile: &TileCoord,
    extent: u32,
    buffer: u32,
) -> bool {
    let (local_x, local_y) = coord.to_tile_local(tile, extent);
    let buf = buffer as i32;
    let ext = extent as i32;

    local_x >= -buf && local_x <= ext + buf && local_y >= -buf && local_y <= ext + buf
}

/// Validate a ring of WorldCoords for MVT encoding.
///
/// Checks:
/// - Minimum point count (4 for a valid ring: 3 unique + closing)
/// - Non-zero area in tile-local coordinates (detects degenerate rings)
///
/// # Arguments
/// * `coords` - Ring coordinates
/// * `tile` - Target tile for area calculation
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// Validation result
pub fn validate_world_ring(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
) -> WorldCoordValidation {
    if coords.is_empty() {
        return WorldCoordValidation::Invalid(WorldCoordInvalidReason::Empty);
    }

    if coords.len() < MIN_POLYGON_RING_POINTS {
        return WorldCoordValidation::Invalid(WorldCoordInvalidReason::TooFewCoords {
            required: MIN_POLYGON_RING_POINTS,
            actual: coords.len(),
        });
    }

    // Calculate signed area using the shoelace formula in tile-local coordinates.
    // Using i64 to avoid overflow with i32 coordinates.
    let mut area2: i64 = 0;
    let locals: Vec<(i32, i32)> = coords
        .iter()
        .map(|c| c.to_tile_local(tile, extent))
        .collect();

    for i in 0..locals.len() - 1 {
        let (x1, y1) = locals[i];
        let (x2, y2) = locals[i + 1];
        area2 += (x1 as i64) * (y2 as i64) - (x2 as i64) * (y1 as i64);
    }

    // area2 is 2 * signed area. If zero, the ring is degenerate.
    if area2 == 0 {
        return WorldCoordValidation::Invalid(WorldCoordInvalidReason::ZeroAreaRing);
    }

    WorldCoordValidation::Valid
}

/// Validate a linestring of WorldCoords for MVT encoding.
///
/// Checks minimum point count (2 for a valid linestring).
///
/// # Arguments
/// * `coords` - LineString coordinates
///
/// # Returns
/// Validation result
pub fn validate_world_linestring(coords: &[WorldCoord]) -> WorldCoordValidation {
    if coords.is_empty() {
        return WorldCoordValidation::Invalid(WorldCoordInvalidReason::Empty);
    }

    if coords.len() < MIN_LINESTRING_POINTS {
        return WorldCoordValidation::Invalid(WorldCoordInvalidReason::TooFewCoords {
            required: MIN_LINESTRING_POINTS,
            actual: coords.len(),
        });
    }

    WorldCoordValidation::Valid
}

/// Check if a ring of WorldCoords will produce a degenerate geometry in a specific tile.
///
/// A ring is degenerate in a tile if all its tile-local coordinates round to the
/// same pixel, meaning the ring has zero visual area in the rendered tile.
///
/// # Arguments
/// * `coords` - Ring coordinates
/// * `tile` - Target tile
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// `true` if the ring is degenerate (all points collapse to same pixel)
pub fn is_degenerate_in_tile(coords: &[WorldCoord], tile: &TileCoord, extent: u32) -> bool {
    if coords.len() < 2 {
        return true;
    }

    let (first_x, first_y) = coords[0].to_tile_local(tile, extent);

    coords[1..].iter().all(|c| {
        let (x, y) = c.to_tile_local(tile, extent);
        x == first_x && y == first_y
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Coord, MultiPoint, Point};

    // =========================================================================
    // HELPER FUNCTIONS
    // =========================================================================

    fn make_linestring(coords: &[(f64, f64)]) -> LineString<f64> {
        LineString::new(coords.iter().map(|&(x, y)| Coord { x, y }).collect())
    }

    fn make_polygon(exterior: &[(f64, f64)]) -> Polygon<f64> {
        Polygon::new(make_linestring(exterior), vec![])
    }

    fn make_polygon_with_hole(exterior: &[(f64, f64)], hole: &[(f64, f64)]) -> Polygon<f64> {
        Polygon::new(make_linestring(exterior), vec![make_linestring(hole)])
    }

    // =========================================================================
    // POINT TESTS
    // =========================================================================

    #[test]
    fn test_point_always_valid() {
        let point = Geometry::Point(Point::new(0.0, 0.0));
        assert!(is_valid_geometry(&point));
        assert_eq!(validate_geometry(&point), ValidationResult::Valid);
    }

    #[test]
    fn test_multipoint_valid() {
        let mp = Geometry::MultiPoint(geo::MultiPoint::new(vec![
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
        ]));
        assert!(is_valid_geometry(&mp));
    }

    #[test]
    fn test_multipoint_empty_invalid() {
        let mp = Geometry::MultiPoint(MultiPoint::new(vec![]));
        assert!(!is_valid_geometry(&mp));
        assert_eq!(
            validate_geometry(&mp),
            ValidationResult::Invalid(InvalidReason::EmptyGeometry)
        );
    }

    // =========================================================================
    // LINESTRING TESTS
    // =========================================================================

    #[test]
    fn test_linestring_valid_two_points() {
        let ls = Geometry::LineString(make_linestring(&[(0.0, 0.0), (1.0, 1.0)]));
        assert!(is_valid_geometry(&ls));
    }

    #[test]
    fn test_linestring_valid_many_points() {
        let ls = Geometry::LineString(make_linestring(&[
            (0.0, 0.0),
            (1.0, 0.0),
            (1.0, 1.0),
            (0.0, 1.0),
        ]));
        assert!(is_valid_geometry(&ls));
    }

    #[test]
    fn test_linestring_invalid_one_point() {
        let ls = Geometry::LineString(make_linestring(&[(0.0, 0.0)]));
        assert!(!is_valid_geometry(&ls));
        assert_eq!(
            validate_geometry(&ls),
            ValidationResult::Invalid(InvalidReason::LineStringTooFewPoints { point_count: 1 })
        );
    }

    #[test]
    fn test_linestring_invalid_empty() {
        let ls = Geometry::LineString(make_linestring(&[]));
        assert!(!is_valid_geometry(&ls));
        assert_eq!(
            validate_geometry(&ls),
            ValidationResult::Invalid(InvalidReason::LineStringTooFewPoints { point_count: 0 })
        );
    }

    // =========================================================================
    // MULTILINESTRING TESTS
    // =========================================================================

    #[test]
    fn test_multilinestring_valid() {
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![
            make_linestring(&[(0.0, 0.0), (1.0, 1.0)]),
            make_linestring(&[(2.0, 2.0), (3.0, 3.0)]),
        ]));
        assert!(is_valid_geometry(&mls));
    }

    #[test]
    fn test_multilinestring_empty_invalid() {
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![]));
        assert!(!is_valid_geometry(&mls));
        assert_eq!(
            validate_geometry(&mls),
            ValidationResult::Invalid(InvalidReason::EmptyGeometry)
        );
    }

    #[test]
    fn test_multilinestring_with_one_valid_line() {
        // One valid, one invalid - should be valid overall
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![
            make_linestring(&[(0.0, 0.0), (1.0, 1.0)]), // valid
            make_linestring(&[(2.0, 2.0)]),             // invalid (1 point)
        ]));
        assert!(is_valid_geometry(&mls));
    }

    #[test]
    fn test_multilinestring_all_invalid_lines() {
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![
            make_linestring(&[(0.0, 0.0)]), // invalid
            make_linestring(&[(1.0, 1.0)]), // invalid
        ]));
        assert!(!is_valid_geometry(&mls));
        assert_eq!(
            validate_geometry(&mls),
            ValidationResult::Invalid(InvalidReason::NoValidLineStrings)
        );
    }

    // =========================================================================
    // POLYGON TESTS
    // =========================================================================

    #[test]
    fn test_polygon_valid_triangle() {
        // Triangle: 3 unique points + closing = 4 points
        let poly = Geometry::Polygon(make_polygon(&[
            (0.0, 0.0),
            (1.0, 0.0),
            (0.5, 1.0),
            (0.0, 0.0), // closing point
        ]));
        assert!(is_valid_geometry(&poly));
    }

    #[test]
    fn test_polygon_valid_square() {
        let poly = Geometry::Polygon(make_polygon(&[
            (0.0, 0.0),
            (1.0, 0.0),
            (1.0, 1.0),
            (0.0, 1.0),
            (0.0, 0.0), // closing point
        ]));
        assert!(is_valid_geometry(&poly));
    }

    #[test]
    fn test_polygon_invalid_too_few_points() {
        // Only 3 points (2 unique + closing) - not enough for a polygon
        let poly = Geometry::Polygon(make_polygon(&[
            (0.0, 0.0),
            (1.0, 0.0),
            (0.0, 0.0), // closing point
        ]));
        assert!(!is_valid_geometry(&poly));
        assert_eq!(
            validate_geometry(&poly),
            ValidationResult::Invalid(InvalidReason::PolygonTooFewPoints {
                ring_index: 0,
                point_count: 3
            })
        );
    }

    #[test]
    fn test_polygon_invalid_two_points() {
        // Note: geo's LineString automatically closes the ring, so 2 input points
        // become 3 points (2 unique + closing). Still invalid for a polygon.
        let poly = Geometry::Polygon(make_polygon(&[(0.0, 0.0), (1.0, 0.0)]));
        assert!(!is_valid_geometry(&poly));
        // The ring will have 3 points after closing, but still invalid
        if let ValidationResult::Invalid(InvalidReason::PolygonTooFewPoints {
            ring_index,
            point_count,
        }) = validate_geometry(&poly)
        {
            assert_eq!(ring_index, 0);
            assert!(
                point_count < MIN_POLYGON_RING_POINTS,
                "Expected fewer than {} points, got {}",
                MIN_POLYGON_RING_POINTS,
                point_count
            );
        } else {
            panic!("Expected PolygonTooFewPoints error");
        }
    }

    #[test]
    fn test_polygon_invalid_empty() {
        let poly = Geometry::Polygon(make_polygon(&[]));
        assert!(!is_valid_geometry(&poly));
    }

    #[test]
    fn test_polygon_invalid_zero_area_collinear() {
        // All points on a line - zero area
        let poly = Geometry::Polygon(make_polygon(&[
            (0.0, 0.0),
            (1.0, 0.0),
            (2.0, 0.0),
            (3.0, 0.0),
            (0.0, 0.0),
        ]));
        assert!(!is_valid_geometry(&poly));
        assert_eq!(
            validate_geometry(&poly),
            ValidationResult::Invalid(InvalidReason::ZeroAreaPolygon)
        );
    }

    #[test]
    fn test_polygon_invalid_zero_area_coincident() {
        // All points at same location - zero area
        let poly = Geometry::Polygon(make_polygon(&[
            (0.5, 0.5),
            (0.5, 0.5),
            (0.5, 0.5),
            (0.5, 0.5),
        ]));
        assert!(!is_valid_geometry(&poly));
        assert_eq!(
            validate_geometry(&poly),
            ValidationResult::Invalid(InvalidReason::ZeroAreaPolygon)
        );
    }

    #[test]
    fn test_polygon_with_valid_hole() {
        let poly = Geometry::Polygon(make_polygon_with_hole(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            &[(2.0, 2.0), (8.0, 2.0), (8.0, 8.0), (2.0, 8.0), (2.0, 2.0)],
        ));
        assert!(is_valid_geometry(&poly));
    }

    #[test]
    fn test_polygon_with_invalid_hole() {
        // Exterior is valid, but hole has too few points
        let poly = Geometry::Polygon(make_polygon_with_hole(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            &[
                (2.0, 2.0),
                (8.0, 2.0),
                (2.0, 2.0), // Only 3 points
            ],
        ));
        assert!(!is_valid_geometry(&poly));
        assert_eq!(
            validate_geometry(&poly),
            ValidationResult::Invalid(InvalidReason::PolygonTooFewPoints {
                ring_index: 1, // Interior ring index
                point_count: 3
            })
        );
    }

    // =========================================================================
    // MULTIPOLYGON TESTS
    // =========================================================================

    #[test]
    fn test_multipolygon_valid() {
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![
            make_polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]),
            make_polygon(&[(2.0, 2.0), (3.0, 2.0), (3.0, 3.0), (2.0, 3.0), (2.0, 2.0)]),
        ]));
        assert!(is_valid_geometry(&mp));
    }

    #[test]
    fn test_multipolygon_empty_invalid() {
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![]));
        assert!(!is_valid_geometry(&mp));
        assert_eq!(
            validate_geometry(&mp),
            ValidationResult::Invalid(InvalidReason::EmptyGeometry)
        );
    }

    #[test]
    fn test_multipolygon_with_one_valid_polygon() {
        // One valid, one invalid
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![
            make_polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]), // valid
            make_polygon(&[(2.0, 2.0), (3.0, 2.0), (2.0, 2.0)]), // invalid (too few points)
        ]));
        assert!(is_valid_geometry(&mp));
    }

    #[test]
    fn test_multipolygon_all_invalid() {
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![
            make_polygon(&[(0.0, 0.0), (1.0, 0.0), (0.0, 0.0)]), // invalid
            make_polygon(&[(2.0, 2.0), (3.0, 2.0), (2.0, 2.0)]), // invalid
        ]));
        assert!(!is_valid_geometry(&mp));
        assert_eq!(
            validate_geometry(&mp),
            ValidationResult::Invalid(InvalidReason::NoValidPolygons)
        );
    }

    // =========================================================================
    // FILTER TESTS
    // =========================================================================

    #[test]
    fn test_filter_valid_geometry_passes_through() {
        let ls = Geometry::LineString(make_linestring(&[(0.0, 0.0), (1.0, 1.0)]));
        let filtered = filter_valid_geometry(&ls);
        assert!(filtered.is_some());
        assert_eq!(filtered.unwrap(), ls);
    }

    #[test]
    fn test_filter_invalid_geometry_returns_none() {
        let ls = Geometry::LineString(make_linestring(&[(0.0, 0.0)]));
        let filtered = filter_valid_geometry(&ls);
        assert!(filtered.is_none());
    }

    #[test]
    fn test_filter_multilinestring_removes_invalid() {
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![
            make_linestring(&[(0.0, 0.0), (1.0, 1.0)]), // valid
            make_linestring(&[(2.0, 2.0)]),             // invalid
            make_linestring(&[(3.0, 3.0), (4.0, 4.0)]), // valid
        ]));

        let filtered = filter_valid_geometry(&mls);
        assert!(filtered.is_some());

        let result = filtered.unwrap();
        if let Geometry::MultiLineString(result_mls) = result {
            assert_eq!(result_mls.0.len(), 2);
        } else {
            panic!("Expected MultiLineString");
        }
    }

    #[test]
    fn test_filter_multilinestring_downgrades_to_single() {
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![
            make_linestring(&[(0.0, 0.0), (1.0, 1.0)]), // valid
            make_linestring(&[(2.0, 2.0)]),             // invalid
        ]));

        let filtered = filter_valid_geometry(&mls);
        assert!(filtered.is_some());

        // Should downgrade to single LineString
        let result = filtered.unwrap();
        assert!(
            matches!(result, Geometry::LineString(_)),
            "Should downgrade to LineString when only one valid line remains"
        );
    }

    #[test]
    fn test_filter_multipolygon_removes_invalid() {
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![
            make_polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]), // valid
            make_polygon(&[(2.0, 2.0), (3.0, 2.0), (2.0, 2.0)]),                         // invalid
            make_polygon(&[(4.0, 4.0), (5.0, 4.0), (5.0, 5.0), (4.0, 5.0), (4.0, 4.0)]), // valid
        ]));

        let filtered = filter_valid_geometry(&mp);
        assert!(filtered.is_some());

        let result = filtered.unwrap();
        if let Geometry::MultiPolygon(result_mp) = result {
            assert_eq!(result_mp.0.len(), 2);
        } else {
            panic!("Expected MultiPolygon");
        }
    }

    #[test]
    fn test_filter_multipolygon_downgrades_to_single() {
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![
            make_polygon(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]), // valid
            make_polygon(&[(2.0, 2.0), (3.0, 2.0), (2.0, 2.0)]),                         // invalid
        ]));

        let filtered = filter_valid_geometry(&mp);
        assert!(filtered.is_some());

        // Should downgrade to single Polygon
        let result = filtered.unwrap();
        assert!(
            matches!(result, Geometry::Polygon(_)),
            "Should downgrade to Polygon when only one valid polygon remains"
        );
    }

    #[test]
    fn test_filter_all_invalid_returns_none() {
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![
            make_polygon(&[(0.0, 0.0), (1.0, 0.0), (0.0, 0.0)]), // invalid
            make_polygon(&[(2.0, 2.0), (3.0, 2.0), (2.0, 2.0)]), // invalid
        ]));

        let filtered = filter_valid_geometry(&mp);
        assert!(filtered.is_none());
    }

    // =========================================================================
    // EDGE CASE TESTS
    // =========================================================================

    #[test]
    fn test_polygon_near_zero_area_but_valid() {
        // Very small but valid triangle
        let poly = Geometry::Polygon(make_polygon(&[
            (0.0, 0.0),
            (0.001, 0.0),
            (0.0005, 0.001),
            (0.0, 0.0),
        ]));
        // This has non-zero area (0.0000005), should be valid
        assert!(is_valid_geometry(&poly));
    }

    #[test]
    fn test_validation_result_methods() {
        let valid = ValidationResult::Valid;
        let invalid = ValidationResult::Invalid(InvalidReason::EmptyGeometry);

        assert!(valid.is_valid());
        assert!(!valid.is_invalid());
        assert!(!invalid.is_valid());
        assert!(invalid.is_invalid());
    }

    // =========================================================================
    // WORLDCOORD VALIDATION TESTS
    // =========================================================================

    use crate::tile::TileCoord;
    use crate::world_coord::lng_lat_to_world;

    // ---- world_coord_in_tile tests ----

    #[test]
    fn test_world_coord_in_tile_zoom0_always_contained() {
        // At zoom 0, the entire world is one tile
        let tile = TileCoord::new(0, 0, 0);
        let coord = lng_lat_to_world(0.0, 0.0);
        assert!(world_coord_in_tile(&coord, &tile));

        let nw = lng_lat_to_world(-180.0, 85.0);
        assert!(world_coord_in_tile(&nw, &tile));

        let se = lng_lat_to_world(179.0, -85.0);
        assert!(world_coord_in_tile(&se, &tile));
    }

    #[test]
    fn test_world_coord_in_tile_zoom1_quadrants() {
        // At zoom 1, world is split into 4 tiles
        // Null Island (0, 0) should be in tile (1, 1) - eastern, southern hemisphere
        let coord = lng_lat_to_world(0.0, 0.0);

        assert!(
            !world_coord_in_tile(&coord, &TileCoord::new(0, 0, 1)),
            "Null Island should NOT be in NW tile"
        );
        assert!(
            world_coord_in_tile(&coord, &TileCoord::new(1, 1, 1)),
            "Null Island should be in SE tile"
        );
    }

    #[test]
    fn test_world_coord_in_tile_point_inside() {
        // NYC at zoom 14
        let coord = lng_lat_to_world(-73.985, 40.748);
        let tile = coord.to_tile(14);
        assert!(world_coord_in_tile(&coord, &tile));
    }

    #[test]
    fn test_world_coord_in_tile_point_outside() {
        // NYC coordinate, but check a completely different tile
        let coord = lng_lat_to_world(-73.985, 40.748);
        let wrong_tile = TileCoord::new(0, 0, 14); // Way off
        assert!(!world_coord_in_tile(&coord, &wrong_tile));
    }

    // ---- world_coord_in_tile_buffer tests ----

    #[test]
    fn test_world_coord_in_tile_buffer_inside() {
        let coord = lng_lat_to_world(0.0, 0.0);
        let tile = coord.to_tile(10);
        assert!(world_coord_in_tile_buffer(&coord, &tile, 4096, 64));
    }

    #[test]
    fn test_world_coord_in_tile_buffer_edge() {
        // A point just outside the tile should be within the buffer region
        let tile = TileCoord::new(512, 512, 10);
        let extent = 4096u32;
        let buffer = 256u32;

        // Get tile's edge in world coords and go slightly beyond
        let shift = 32 - 10u32;
        let tile_max_x = ((tile.x + 1) as u64) << shift;
        // Point just beyond tile's right edge
        let just_outside = WorldCoord::new(
            tile_max_x as u32 + 1,
            ((tile.y as u64) << shift) as u32 + 1000,
        );
        let (local_x, _) = just_outside.to_tile_local(&tile, extent);

        if local_x > extent as i32 && local_x <= extent as i32 + buffer as i32 {
            assert!(world_coord_in_tile_buffer(
                &just_outside,
                &tile,
                extent,
                buffer
            ));
        }
    }

    // ---- validate_world_linestring tests ----

    #[test]
    fn test_validate_world_linestring_valid() {
        let coords = vec![lng_lat_to_world(0.0, 0.0), lng_lat_to_world(1.0, 1.0)];
        assert!(validate_world_linestring(&coords).is_valid());
    }

    #[test]
    fn test_validate_world_linestring_too_short() {
        let coords = vec![lng_lat_to_world(0.0, 0.0)];
        let result = validate_world_linestring(&coords);
        assert_eq!(
            result,
            WorldCoordValidation::Invalid(WorldCoordInvalidReason::TooFewCoords {
                required: 2,
                actual: 1,
            })
        );
    }

    #[test]
    fn test_validate_world_linestring_empty() {
        let result = validate_world_linestring(&[]);
        assert_eq!(
            result,
            WorldCoordValidation::Invalid(WorldCoordInvalidReason::Empty)
        );
    }

    // ---- validate_world_ring tests ----

    #[test]
    fn test_validate_world_ring_valid_triangle() {
        let tile = TileCoord::new(0, 0, 0);
        let coords = vec![
            lng_lat_to_world(-90.0, 45.0),
            lng_lat_to_world(0.0, -45.0),
            lng_lat_to_world(90.0, 45.0),
            lng_lat_to_world(-90.0, 45.0), // closing
        ];
        assert!(validate_world_ring(&coords, &tile, 4096).is_valid());
    }

    #[test]
    fn test_validate_world_ring_too_few_points() {
        let tile = TileCoord::new(0, 0, 0);
        let coords = vec![
            lng_lat_to_world(0.0, 0.0),
            lng_lat_to_world(1.0, 0.0),
            lng_lat_to_world(0.0, 0.0), // only 3 points, need 4
        ];
        let result = validate_world_ring(&coords, &tile, 4096);
        assert_eq!(
            result,
            WorldCoordValidation::Invalid(WorldCoordInvalidReason::TooFewCoords {
                required: 4,
                actual: 3,
            })
        );
    }

    #[test]
    fn test_validate_world_ring_empty() {
        let tile = TileCoord::new(0, 0, 0);
        let result = validate_world_ring(&[], &tile, 4096);
        assert_eq!(
            result,
            WorldCoordValidation::Invalid(WorldCoordInvalidReason::Empty)
        );
    }

    #[test]
    fn test_validate_world_ring_collinear_zero_area() {
        // All points on the same line - zero area
        let tile = TileCoord::new(0, 0, 0);
        let coords = vec![
            lng_lat_to_world(-90.0, 0.0),
            lng_lat_to_world(0.0, 0.0),
            lng_lat_to_world(90.0, 0.0),
            lng_lat_to_world(-90.0, 0.0), // closing
        ];
        let result = validate_world_ring(&coords, &tile, 4096);
        assert_eq!(
            result,
            WorldCoordValidation::Invalid(WorldCoordInvalidReason::ZeroAreaRing)
        );
    }

    #[test]
    fn test_validate_world_ring_coincident_zero_area() {
        // All points at the same location - zero area
        let tile = TileCoord::new(0, 0, 0);
        let same = lng_lat_to_world(0.0, 0.0);
        let coords = vec![same, same, same, same];
        let result = validate_world_ring(&coords, &tile, 4096);
        assert_eq!(
            result,
            WorldCoordValidation::Invalid(WorldCoordInvalidReason::ZeroAreaRing)
        );
    }

    // ---- is_degenerate_in_tile tests ----

    #[test]
    fn test_is_degenerate_in_tile_large_polygon_not_degenerate() {
        let tile = TileCoord::new(0, 0, 0);
        let coords = vec![
            lng_lat_to_world(-90.0, 45.0),
            lng_lat_to_world(0.0, -45.0),
            lng_lat_to_world(90.0, 45.0),
            lng_lat_to_world(-90.0, 45.0),
        ];
        assert!(!is_degenerate_in_tile(&coords, &tile, 4096));
    }

    #[test]
    fn test_is_degenerate_in_tile_tiny_polygon_at_high_zoom() {
        // A very tiny polygon that collapses to a single pixel at zoom 0
        // with extent 4096
        let coord = lng_lat_to_world(0.0, 0.0);
        // All points are the same in world coords => definitely degenerate
        let coords = vec![coord, coord, coord, coord];
        let tile = TileCoord::new(0, 0, 0);
        assert!(is_degenerate_in_tile(&coords, &tile, 4096));
    }

    #[test]
    fn test_is_degenerate_in_tile_single_point() {
        let coord = lng_lat_to_world(0.0, 0.0);
        let tile = TileCoord::new(0, 0, 0);
        assert!(is_degenerate_in_tile(&[coord], &tile, 4096));
    }

    #[test]
    fn test_is_degenerate_in_tile_empty() {
        let tile = TileCoord::new(0, 0, 0);
        assert!(is_degenerate_in_tile(&[], &tile, 4096));
    }

    // ---- WorldCoordValidation API tests ----

    #[test]
    fn test_world_coord_validation_is_valid() {
        let valid = WorldCoordValidation::Valid;
        let invalid = WorldCoordValidation::Invalid(WorldCoordInvalidReason::Empty);

        assert!(valid.is_valid());
        assert!(!invalid.is_valid());
    }

    // ---- Consistency tests: WorldCoord validation agrees with f64 validation ----

    #[test]
    fn test_world_coord_ring_validation_agrees_with_f64_for_valid_polygon() {
        // A valid polygon should pass both f64 and WorldCoord validation
        let tile = TileCoord::new(0, 0, 0);
        let points = [
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ];

        // f64 validation
        let poly = make_polygon(&points);
        let f64_result = validate_polygon(&poly);
        assert!(f64_result.is_valid(), "f64 polygon should be valid");

        // WorldCoord validation
        let world_coords: Vec<WorldCoord> = points
            .iter()
            .map(|&(lng, lat)| lng_lat_to_world(lng, lat))
            .collect();
        let wc_result = validate_world_ring(&world_coords, &tile, 4096);
        assert!(wc_result.is_valid(), "WorldCoord ring should be valid");
    }

    #[test]
    fn test_world_coord_ring_validation_agrees_with_f64_for_degenerate() {
        // A degenerate polygon (collinear) should fail both validations
        let tile = TileCoord::new(0, 0, 0);
        let points = [
            (0.0, 0.0),
            (10.0, 0.0),
            (20.0, 0.0),
            (30.0, 0.0),
            (0.0, 0.0),
        ];

        // f64 validation
        let poly = make_polygon(&points);
        let f64_result = validate_polygon(&poly);
        assert!(
            !f64_result.is_valid(),
            "f64 polygon should be invalid (zero area)"
        );

        // WorldCoord validation
        let world_coords: Vec<WorldCoord> = points
            .iter()
            .map(|&(lng, lat)| lng_lat_to_world(lng, lat))
            .collect();
        let wc_result = validate_world_ring(&world_coords, &tile, 4096);
        assert!(
            !wc_result.is_valid(),
            "WorldCoord ring should be invalid (zero area)"
        );
    }
}
