//! i_overlay-based polygon clipping for robust tile boundary clipping.
//!
//! This module provides clipping functions using i_overlay's boolean operations
//! for robust polygon clipping. Unlike wagyu which operates in integer coordinates,
//! i_overlay works directly with f64 coordinates, eliminating coordinate conversion
//! overhead.
//!
//! # Design
//!
//! The workflow is:
//! 1. Convert geo::Polygon<f64> to i_overlay's shape format (Vec<Vec<[f64; 2]>>)
//! 2. Create a clip box from TileBounds
//! 3. Perform Intersect operation with FillRule::EvenOdd
//! 4. Convert the result back to geo::Geometry<f64>
//!
//! # Why i_overlay?
//!
//! i_overlay's boolean operations correctly handle:
//! - Self-intersecting polygons (resolved via fill rule)
//! - U-shaped polygons that split into multiple parts
//! - Polygons with holes that intersect the exterior ring
//! - Complex nested holes
//!
//! The FillRule::EvenOdd interprets overlapping regions correctly, producing
//! valid, non-self-intersecting output from invalid input.
//!
//! # Performance
//!
//! i_overlay uses a sweep-line algorithm with O(n log n) complexity, similar to
//! wagyu's Vatti algorithm. However, by operating in f64 directly, we avoid
//! the overhead of coordinate conversion that wagyu requires.

use crate::tile::TileBounds;
use geo::{Coord, Geometry, LineString, MultiPolygon, Polygon};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::overlay::FloatOverlay;

// ============================================================================
// Type Aliases
// ============================================================================

/// A point in i_overlay format
type IOverlayPoint = [f64; 2];

/// A contour (ring) in i_overlay format
type IOverlayContour = Vec<IOverlayPoint>;

/// A shape in i_overlay format (first contour is exterior, rest are holes)
type IOverlayShape = Vec<IOverlayContour>;

/// Multiple shapes
type IOverlayShapes = Vec<IOverlayShape>;

// ============================================================================
// Conversion: geo -> i_overlay
// ============================================================================

/// Convert a geo::Polygon to i_overlay shape format.
///
/// i_overlay expects shapes as Vec<Vec<[f64; 2]>> where:
/// - First contour is the exterior ring
/// - Subsequent contours are holes
///
/// Note: i_overlay handles both closed (first=last) and open rings.
#[inline]
fn polygon_to_ioverlay(poly: &Polygon<f64>) -> IOverlayShape {
    let mut shape = Vec::with_capacity(1 + poly.interiors().len());

    // Exterior ring
    let exterior: IOverlayContour = poly.exterior().coords().map(|c| [c.x, c.y]).collect();
    shape.push(exterior);

    // Holes
    for hole in poly.interiors() {
        let hole_contour: IOverlayContour = hole.coords().map(|c| [c.x, c.y]).collect();
        shape.push(hole_contour);
    }

    shape
}

/// Create a clip box from TileBounds in i_overlay format.
///
/// Returns a single shape (rectangle) that can be used as the clip subject.
#[inline]
fn bounds_to_clip_box(bounds: &TileBounds) -> IOverlayShape {
    vec![vec![
        [bounds.lng_min, bounds.lat_min],
        [bounds.lng_max, bounds.lat_min],
        [bounds.lng_max, bounds.lat_max],
        [bounds.lng_min, bounds.lat_max],
        [bounds.lng_min, bounds.lat_min], // Close the ring
    ]]
}

// ============================================================================
// Conversion: i_overlay -> geo
// ============================================================================

/// Convert i_overlay shapes to geo::Geometry.
///
/// Returns:
/// - None if no shapes (empty result)
/// - Geometry::Polygon if single shape
/// - Geometry::MultiPolygon if multiple shapes
fn ioverlay_to_geometry(shapes: IOverlayShapes) -> Option<Geometry<f64>> {
    // Filter out empty shapes
    let valid_shapes: Vec<_> = shapes
        .into_iter()
        .filter(|shape| !shape.is_empty() && !shape[0].is_empty())
        .collect();

    if valid_shapes.is_empty() {
        return None;
    }

    let polygons: Vec<Polygon<f64>> = valid_shapes
        .into_iter()
        .filter_map(ioverlay_shape_to_polygon)
        .collect();

    match polygons.len() {
        0 => None,
        1 => Some(Geometry::Polygon(polygons.into_iter().next().unwrap())),
        _ => Some(Geometry::MultiPolygon(MultiPolygon::new(polygons))),
    }
}

/// Convert a single i_overlay shape to geo::Polygon.
///
/// i_overlay returns open contours (no repeated closing point), so we need
/// to ensure the LineString is properly closed for geo.
fn ioverlay_shape_to_polygon(shape: IOverlayShape) -> Option<Polygon<f64>> {
    if shape.is_empty() {
        return None;
    }

    // Convert exterior ring
    let exterior = contour_to_linestring(&shape[0])?;

    // Convert holes
    let holes: Vec<LineString<f64>> = shape[1..]
        .iter()
        .filter_map(contour_to_linestring)
        .collect();

    Some(Polygon::new(exterior, holes))
}

/// Convert an i_overlay contour to geo::LineString.
///
/// Ensures the ring is closed (first point == last point) as required by geo.
fn contour_to_linestring(contour: &IOverlayContour) -> Option<LineString<f64>> {
    if contour.len() < 3 {
        return None;
    }

    let mut coords: Vec<Coord<f64>> = contour.iter().map(|p| Coord { x: p[0], y: p[1] }).collect();

    // Ensure closed ring (i_overlay returns open contours)
    if coords.first() != coords.last() {
        if let Some(first) = coords.first().cloned() {
            coords.push(first);
        }
    }

    // Need at least 4 points for a valid closed ring (triangle + closing point)
    if coords.len() < 4 {
        return None;
    }

    Some(LineString::new(coords))
}

// ============================================================================
// Public Clipping API
// ============================================================================

/// Clip a polygon to tile bounds using i_overlay's boolean intersection.
///
/// This function:
/// 1. Converts the polygon to i_overlay format
/// 2. Creates a clip box from the bounds
/// 3. Performs an Intersect operation with FillRule::EvenOdd
/// 4. Converts the result back to geo::Geometry
///
/// The EvenOdd fill rule correctly handles self-intersecting polygons by
/// interpreting overlapping regions as "outside", effectively resolving
/// self-intersections in the output.
///
/// # Arguments
///
/// * `poly` - The polygon to clip
/// * `bounds` - The tile bounds to clip to
///
/// # Returns
///
/// - `Some(Geometry::Polygon)` if result is a single polygon
/// - `Some(Geometry::MultiPolygon)` if result splits into multiple polygons
/// - `None` if the polygon doesn't intersect the bounds
///
/// # Example
///
/// ```ignore
/// use gpq_tiles_core::ioverlay_clip::clip_polygon_ioverlay;
/// use gpq_tiles_core::tile::TileBounds;
/// use geo::Polygon;
///
/// let poly = create_polygon();
/// let bounds = TileBounds::new(-180.0, -90.0, 180.0, 90.0);
/// let result = clip_polygon_ioverlay(&poly, &bounds);
/// ```
pub fn clip_polygon_ioverlay(poly: &Polygon<f64>, bounds: &TileBounds) -> Option<Geometry<f64>> {
    // Convert polygon to i_overlay format
    let subj = polygon_to_ioverlay(poly);

    // Create clip box
    let clip = bounds_to_clip_box(bounds);

    // Perform intersection using EvenOdd fill rule
    // EvenOdd correctly handles self-intersecting polygons
    // Note: geo 0.32 pins i_overlay to 4.0.x which lacks OverlayOptions::ogc(),
    // but Default options with EvenOdd fill rule achieve the same valid output.
    let mut overlay = FloatOverlay::with_subj_and_clip_custom(
        &[subj],
        &[clip],
        Default::default(),
        Default::default(),
    );
    let result: IOverlayShapes = overlay.overlay(OverlayRule::Intersect, FillRule::EvenOdd);

    ioverlay_to_geometry(result)
}

/// Clip a MultiPolygon to tile bounds using i_overlay.
///
/// Each polygon in the MultiPolygon is added to the overlay as a separate
/// subject shape, then clipped against the bounds.
///
/// # Arguments
///
/// * `multi` - The MultiPolygon to clip
/// * `bounds` - The tile bounds to clip to
///
/// # Returns
///
/// - `Some(Geometry::Polygon)` if result is a single polygon
/// - `Some(Geometry::MultiPolygon)` if result has multiple polygons
/// - `None` if no polygons intersect the bounds
pub fn clip_multipolygon_ioverlay(
    multi: &MultiPolygon<f64>,
    bounds: &TileBounds,
) -> Option<Geometry<f64>> {
    // Convert all polygons to i_overlay format
    let subj_shapes: Vec<IOverlayShape> = multi.0.iter().map(polygon_to_ioverlay).collect();

    if subj_shapes.is_empty() {
        return None;
    }

    // Create clip box
    let clip = bounds_to_clip_box(bounds);

    // Perform intersection using default options
    // Note: geo 0.32 pins i_overlay to 4.0.x which lacks OverlayOptions::ogc(),
    // but Default options with EvenOdd fill rule achieve the same valid output.
    let mut overlay = FloatOverlay::with_subj_and_clip_custom(
        &subj_shapes,
        &[clip],
        Default::default(),
        Default::default(),
    );
    let result: IOverlayShapes = overlay.overlay(OverlayRule::Intersect, FillRule::EvenOdd);

    ioverlay_to_geometry(result)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Coord;

    fn make_square(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Polygon<f64> {
        Polygon::new(
            LineString::new(vec![
                Coord { x: min_x, y: min_y },
                Coord { x: max_x, y: min_y },
                Coord { x: max_x, y: max_y },
                Coord { x: min_x, y: max_y },
                Coord { x: min_x, y: min_y },
            ]),
            vec![],
        )
    }

    #[test]
    fn test_clip_fully_inside() {
        let poly = make_square(1.0, 1.0, 2.0, 2.0);
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let result = clip_polygon_ioverlay(&poly, &bounds);

        assert!(result.is_some());
        match result.unwrap() {
            Geometry::Polygon(p) => {
                assert!(p.exterior().0.len() >= 4);
            }
            _ => panic!("Expected Polygon"),
        }
    }

    #[test]
    fn test_clip_fully_outside() {
        let poly = make_square(100.0, 100.0, 200.0, 200.0);
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let result = clip_polygon_ioverlay(&poly, &bounds);

        assert!(result.is_none());
    }

    #[test]
    fn test_clip_partial() {
        let poly = make_square(-5.0, -5.0, 5.0, 5.0);
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let result = clip_polygon_ioverlay(&poly, &bounds);

        assert!(result.is_some());
        match result.unwrap() {
            Geometry::Polygon(p) => {
                // Should be clipped to the corner
                assert!(p.exterior().0.len() >= 4);
            }
            _ => panic!("Expected Polygon"),
        }
    }

    #[test]
    fn test_clip_self_intersecting_bowtie() {
        // Create a self-intersecting bowtie polygon
        let bowtie = Polygon::new(
            LineString::new(vec![
                Coord { x: -1.0, y: -1.0 },
                Coord { x: 1.0, y: 1.0 },
                Coord { x: -1.0, y: 1.0 },
                Coord { x: 1.0, y: -1.0 },
                Coord { x: -1.0, y: -1.0 },
            ]),
            vec![],
        );

        let bounds = TileBounds::new(-2.0, -2.0, 2.0, 2.0);

        let result = clip_polygon_ioverlay(&bowtie, &bounds);

        // Should produce valid output (MultiPolygon with 2 triangles)
        assert!(result.is_some());
        match result.unwrap() {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2, "Bowtie should split into 2 triangles");
            }
            Geometry::Polygon(_) => {
                // Also acceptable if it merges them somehow
            }
            other => panic!("Expected Polygon or MultiPolygon, got {:?}", other),
        }
    }

    #[test]
    fn test_clip_u_shape_splits() {
        // Create a U-shaped polygon
        let u_shape = Polygon::new(
            LineString::new(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: 0.0, y: 2.0 },
                Coord { x: 0.3, y: 2.0 },
                Coord { x: 0.3, y: 0.5 },
                Coord { x: 0.7, y: 0.5 },
                Coord { x: 0.7, y: 2.0 },
                Coord { x: 1.0, y: 2.0 },
                Coord { x: 1.0, y: 0.0 },
                Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        );

        // Clip box that cuts through the U opening
        let bounds = TileBounds::new(-0.1, 1.0, 1.1, 2.5);

        let result = clip_polygon_ioverlay(&u_shape, &bounds);

        assert!(result.is_some());
        match result.unwrap() {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(
                    mp.0.len(),
                    2,
                    "U-shape clipped across opening should produce 2 polygons"
                );
            }
            other => panic!("Expected MultiPolygon with 2 polygons, got {:?}", other),
        }
    }

    #[test]
    fn test_clip_multipolygon() {
        let poly1 = make_square(1.0, 1.0, 2.0, 2.0);
        let poly2 = make_square(5.0, 5.0, 6.0, 6.0);
        let multi = MultiPolygon::new(vec![poly1, poly2]);

        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let result = clip_multipolygon_ioverlay(&multi, &bounds);

        assert!(result.is_some());
        match result.unwrap() {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2, "Both polygons should be preserved");
            }
            _ => panic!("Expected MultiPolygon"),
        }
    }
}
