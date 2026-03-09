//! Geometry clipping to tile bounds.
//!
//! Clips geometries to tile boundaries with a configurable buffer zone to prevent
//! visual seams when rendering adjacent tiles.
//!
//! # Tippecanoe Alignment
//!
//! This module matches tippecanoe's clipping behavior:
//! - **Buffer**: Default 8 pixels (configurable via `--buffer` in tippecanoe)
//!   Buffer is measured in "screen pixels" where 1 pixel = 1/256th of tile width
//! - **Clipping method**: Features are clipped to tile boundary + buffer zone
//! - **Duplication**: Features may appear in multiple tiles if they span boundaries
//! - **Algorithm**: Uses Sutherland-Hodgman for polygon clipping against axis-aligned
//!   tile boundaries (same approach as tippecanoe's clip.cpp). This is O(n) and
//!   specialized for rectangle clipping, replacing the general-purpose Vatti/wagyu
//!   algorithm which was O(n log n) and pathologically slow on complex polygons.
//!
//! See: https://github.com/felt/tippecanoe (clipping documentation)

use geo::{
    BooleanOps, BoundingRect, Coord, Geometry, LineString, MultiLineString, MultiPolygon, Point,
    Polygon, Rect,
};

use crate::sutherland_hodgman;
use crate::tile::TileBounds;

/// Default buffer in pixels (matches tippecanoe's common usage)
/// Tippecanoe default is 5, but CLAUDE.md specifies 8 for this project
pub const DEFAULT_BUFFER_PIXELS: u32 = 8;

/// Default tile extent in pixels
pub const DEFAULT_EXTENT: u32 = 4096;

/// Clip a geometry to tile bounds with a buffer.
///
/// # Arguments
///
/// * `geom` - The geometry to clip
/// * `bounds` - The tile bounds (without buffer)
/// * `buffer` - Buffer size in the same units as bounds (typically degrees)
///
/// # Returns
///
/// The clipped geometry, or `None` if the geometry doesn't intersect the buffered bounds
///
/// # Tippecanoe Behavior
///
/// Tippecanoe clips features to tile boundaries plus a buffer zone. The buffer
/// prevents visual seams when tiles are rendered side-by-side. Features that
/// span tile boundaries are duplicated into adjacent tiles.
pub fn clip_geometry(
    geom: &Geometry<f64>,
    bounds: &TileBounds,
    buffer: f64,
) -> Option<Geometry<f64>> {
    let buffered = TileBounds::new(
        bounds.lng_min - buffer,
        bounds.lat_min - buffer,
        bounds.lng_max + buffer,
        bounds.lat_max + buffer,
    );

    match geom {
        Geometry::Point(p) => clip_point(p, &buffered).map(Geometry::Point),
        Geometry::LineString(ls) => clip_linestring(ls, &buffered),
        Geometry::Polygon(poly) => clip_polygon(poly, &buffered),
        Geometry::MultiPolygon(mp) => clip_multipolygon(mp, &buffered).map(Geometry::MultiPolygon),
        Geometry::MultiLineString(mls) => clip_multilinestring(mls, &buffered),
        other => {
            // For other geometry types, use bounding box check
            if let Some(rect) = other.bounding_rect() {
                if intersects_bounds(&rect, &buffered) {
                    return Some(other.clone());
                }
            }
            None
        }
    }
}

/// Convert buffer from pixels to degrees based on tile bounds.
///
/// # Arguments
///
/// * `buffer_pixels` - Buffer size in pixels (e.g., 8)
/// * `tile_bounds` - The tile bounds to calculate pixel size from
/// * `extent` - Tile extent in pixels (e.g., 4096)
///
/// # Returns
///
/// Buffer size in degrees (same units as tile bounds)
pub fn buffer_pixels_to_degrees(buffer_pixels: u32, tile_bounds: &TileBounds, extent: u32) -> f64 {
    // Buffer is specified in "screen pixels" where the tile is extent pixels wide
    // Convert to the same units as bounds (degrees)
    tile_bounds.width() * buffer_pixels as f64 / extent as f64
}

/// Check if a rectangle intersects the given bounds
fn intersects_bounds(rect: &Rect<f64>, bounds: &TileBounds) -> bool {
    rect.max().x >= bounds.lng_min
        && rect.min().x <= bounds.lng_max
        && rect.max().y >= bounds.lat_min
        && rect.min().y <= bounds.lat_max
}

/// Clip a point to bounds (simple containment check)
fn clip_point(point: &Point<f64>, bounds: &TileBounds) -> Option<Point<f64>> {
    if point.x() >= bounds.lng_min
        && point.x() <= bounds.lng_max
        && point.y() >= bounds.lat_min
        && point.y() <= bounds.lat_max
    {
        Some(*point)
    } else {
        None
    }
}

/// Clip a linestring to bounds using BooleanOps.
///
/// IMPORTANT: Uses correct signature - `polygon.clip(&linestring, invert)`
/// NOT `linestring.clip(&polygon)` which doesn't exist.
fn clip_linestring(ls: &LineString<f64>, bounds: &TileBounds) -> Option<Geometry<f64>> {
    // Quick rejection test
    if let Some(rect) = ls.bounding_rect() {
        if !intersects_bounds(&rect, bounds) {
            return None;
        }
    }

    let clip_rect = Rect::new(
        Coord {
            x: bounds.lng_min,
            y: bounds.lat_min,
        },
        Coord {
            x: bounds.lng_max,
            y: bounds.lat_max,
        },
    );
    let clip_poly = clip_rect.to_polygon();

    // Correct usage: polygon.clip(&multilinestring, invert)
    // invert=false means keep the parts INSIDE the polygon
    let mls = MultiLineString::new(vec![ls.clone()]);
    let clipped = clip_poly.clip(&mls, false);

    if clipped.0.is_empty() {
        None
    } else if clipped.0.len() == 1 {
        Some(Geometry::LineString(clipped.0.into_iter().next().unwrap()))
    } else {
        Some(Geometry::MultiLineString(clipped))
    }
}

/// Clip a multilinestring to bounds
fn clip_multilinestring(mls: &MultiLineString<f64>, bounds: &TileBounds) -> Option<Geometry<f64>> {
    // Quick rejection test
    if let Some(rect) = mls.bounding_rect() {
        if !intersects_bounds(&rect, bounds) {
            return None;
        }
    }

    let clip_rect = Rect::new(
        Coord {
            x: bounds.lng_min,
            y: bounds.lat_min,
        },
        Coord {
            x: bounds.lng_max,
            y: bounds.lat_max,
        },
    );
    let clip_poly = clip_rect.to_polygon();

    // Correct usage: polygon.clip(&multilinestring, invert)
    let clipped = clip_poly.clip(mls, false);

    if clipped.0.is_empty() {
        None
    } else {
        Some(Geometry::MultiLineString(clipped))
    }
}

/// Clip a polygon to bounds using Sutherland-Hodgman algorithm.
///
/// Uses Sutherland-Hodgman for O(n) clipping against axis-aligned tile boundaries.
/// This matches tippecanoe's approach (clip.cpp) and is significantly faster than
/// general-purpose polygon clipping (wagyu/Vatti) for rectangle clipping.
///
/// # DIVERGENCE FROM TIPPECANOE: coordinate space
/// Tippecanoe operates in integer tile coordinates (0-4096).
/// We operate in f64 geographic coordinates to avoid coordinate conversion overhead.
/// The algorithm is identical; only the coordinate space differs.
///
/// Returns `Geometry::Polygon` with the clipped result, or `None` if the polygon
/// doesn't intersect the bounds.
fn clip_polygon(poly: &Polygon<f64>, bounds: &TileBounds) -> Option<Geometry<f64>> {
    // Quick rejection test
    let poly_rect = poly.bounding_rect()?;
    if !intersects_bounds(&poly_rect, bounds) {
        return None;
    }

    // FAST PATH: If polygon is fully inside bounds, return as-is (no clipping needed)
    if poly_rect.min().x >= bounds.lng_min
        && poly_rect.max().x <= bounds.lng_max
        && poly_rect.min().y >= bounds.lat_min
        && poly_rect.max().y <= bounds.lat_max
    {
        return Some(Geometry::Polygon(poly.clone()));
    }

    // Use Sutherland-Hodgman for O(n) rectangle clipping
    sutherland_hodgman::clip_polygon_sh(poly, bounds)
}

/// Clip a multipolygon to bounds using Sutherland-Hodgman algorithm.
fn clip_multipolygon(mp: &MultiPolygon<f64>, bounds: &TileBounds) -> Option<MultiPolygon<f64>> {
    // Quick rejection test
    let mp_rect = mp.bounding_rect()?;
    if !intersects_bounds(&mp_rect, bounds) {
        return None;
    }

    // FAST PATH: If multipolygon is fully inside bounds, return as-is
    if mp_rect.min().x >= bounds.lng_min
        && mp_rect.max().x <= bounds.lng_max
        && mp_rect.min().y >= bounds.lat_min
        && mp_rect.max().y <= bounds.lat_max
    {
        return Some(mp.clone());
    }

    // Clip each polygon individually using Sutherland-Hodgman
    let mut clipped_polys = Vec::new();
    for poly in &mp.0 {
        if let Some(Geometry::Polygon(clipped)) = clip_polygon(poly, bounds) {
            clipped_polys.push(clipped);
        }
    }

    if clipped_polys.is_empty() {
        None
    } else {
        Some(MultiPolygon::new(clipped_polys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::point;

    // ========== Point Clipping Tests ==========

    #[test]
    fn test_clip_point_inside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let point = point!(x: 5.0, y: 5.0);
        assert!(clip_point(&point, &bounds).is_some());
    }

    #[test]
    fn test_clip_point_outside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let point = point!(x: 15.0, y: 5.0);
        assert!(clip_point(&point, &bounds).is_none());
    }

    #[test]
    fn test_clip_point_on_boundary() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let point = point!(x: 10.0, y: 5.0);
        assert!(clip_point(&point, &bounds).is_some());
    }

    // ========== Polygon Clipping Tests ==========

    #[test]
    fn test_clip_polygon_partial() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: -5.0, y: -5.0 },
                Coord { x: 5.0, y: -5.0 },
                Coord { x: 5.0, y: 5.0 },
                Coord { x: -5.0, y: 5.0 },
                Coord { x: -5.0, y: -5.0 },
            ]),
            vec![],
        );

        let result = clip_polygon(&poly, &bounds);
        assert!(result.is_some());

        // Extract the polygon (should be single polygon for this simple case)
        let clipped = match result.unwrap() {
            Geometry::Polygon(p) => p,
            Geometry::MultiPolygon(mp) => mp.0.into_iter().next().unwrap(),
            _ => panic!("Expected polygon geometry"),
        };
        // Verify clipped polygon is within bounds
        for coord in clipped.exterior().coords() {
            assert!(
                coord.x >= 0.0 && coord.x <= 10.0,
                "x={} out of bounds",
                coord.x
            );
            assert!(
                coord.y >= 0.0 && coord.y <= 10.0,
                "y={} out of bounds",
                coord.y
            );
        }
    }

    #[test]
    fn test_clip_polygon_outside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: 20.0, y: 20.0 },
                Coord { x: 30.0, y: 20.0 },
                Coord { x: 30.0, y: 30.0 },
                Coord { x: 20.0, y: 30.0 },
                Coord { x: 20.0, y: 20.0 },
            ]),
            vec![],
        );
        assert!(clip_polygon(&poly, &bounds).is_none());
    }

    #[test]
    fn test_clip_polygon_fully_inside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: 2.0, y: 2.0 },
                Coord { x: 8.0, y: 2.0 },
                Coord { x: 8.0, y: 8.0 },
                Coord { x: 2.0, y: 8.0 },
                Coord { x: 2.0, y: 2.0 },
            ]),
            vec![],
        );

        let result = clip_polygon(&poly, &bounds);
        assert!(result.is_some());
    }

    // ========== LineString Clipping Tests ==========

    #[test]
    fn test_clip_linestring_crossing() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let ls = LineString::from(vec![Coord { x: -5.0, y: 5.0 }, Coord { x: 15.0, y: 5.0 }]);

        let result = clip_linestring(&ls, &bounds);
        assert!(result.is_some());
    }

    #[test]
    fn test_clip_linestring_outside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let ls = LineString::from(vec![Coord { x: 20.0, y: 20.0 }, Coord { x: 30.0, y: 30.0 }]);

        let result = clip_linestring(&ls, &bounds);
        assert!(result.is_none());
    }

    #[test]
    fn test_clip_linestring_fully_inside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let ls = LineString::from(vec![Coord { x: 2.0, y: 2.0 }, Coord { x: 8.0, y: 8.0 }]);

        let result = clip_linestring(&ls, &bounds);
        assert!(result.is_some());
    }

    // ========== Buffer Calculation Tests ==========

    #[test]
    fn test_buffer_pixels_to_degrees() {
        let bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let buffer = buffer_pixels_to_degrees(8, &bounds, 4096);

        // 8 pixels / 4096 extent * 1.0 degree width = 0.001953125
        let expected = 8.0 / 4096.0;
        assert!(
            (buffer - expected).abs() < 1e-10,
            "buffer={} expected={}",
            buffer,
            expected
        );
    }

    #[test]
    fn test_buffer_affects_clipping() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let buffer = 2.0; // 2 degree buffer

        // Point just outside bounds but within buffer
        let point = point!(x: 11.0, y: 5.0);

        // Without buffer: should be outside
        assert!(clip_point(&point, &bounds).is_none());

        // With buffer via clip_geometry: should be inside
        let result = clip_geometry(&Geometry::Point(point), &bounds, buffer);
        assert!(result.is_some());
    }

    // ========== clip_geometry Integration Tests ==========

    #[test]
    fn test_clip_geometry_point() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let point = Geometry::Point(point!(x: 5.0, y: 5.0));

        let result = clip_geometry(&point, &bounds, 0.0);
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Geometry::Point(_)));
    }

    #[test]
    fn test_clip_geometry_polygon() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                Coord { x: 5.0, y: 5.0 },
                Coord { x: 15.0, y: 5.0 },
                Coord { x: 15.0, y: 15.0 },
                Coord { x: 5.0, y: 15.0 },
                Coord { x: 5.0, y: 5.0 },
            ]),
            vec![],
        ));

        let result = clip_geometry(&poly, &bounds, 0.0);
        assert!(result.is_some());
    }

    #[test]
    fn test_clip_geometry_with_buffer() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let buffer = 1.0;

        // Polygon just outside bounds but overlapping with buffer
        let poly = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                Coord { x: 10.5, y: 5.0 },
                Coord { x: 12.0, y: 5.0 },
                Coord { x: 12.0, y: 8.0 },
                Coord { x: 10.5, y: 8.0 },
                Coord { x: 10.5, y: 5.0 },
            ]),
            vec![],
        ));

        // Without buffer: should be outside
        let result_no_buffer = clip_geometry(&poly, &bounds, 0.0);
        assert!(result_no_buffer.is_none());

        // With buffer: should clip to buffered bounds
        let result_with_buffer = clip_geometry(&poly, &bounds, buffer);
        assert!(result_with_buffer.is_some());
    }

    #[test]
    fn test_clip_polygon_u_shape() {
        // U-shaped polygon clipped by a horizontal band.
        //
        // DIVERGENCE FROM WAGYU: Sutherland-Hodgman does not split disconnected
        // parts into separate polygons. It produces a single (possibly self-touching)
        // polygon. For tile rendering, this is acceptable and matches tippecanoe's
        // Sutherland-Hodgman behavior in clip.cpp.
        let bounds = TileBounds::new(0.0, 4.0, 10.0, 6.0); // Horizontal band

        // U-shape: two vertical bars connected at the bottom
        let u_shape = Polygon::new(
            LineString::from(vec![
                Coord { x: 1.0, y: 0.0 },
                Coord { x: 2.0, y: 0.0 },
                Coord { x: 2.0, y: 10.0 },
                Coord { x: 1.0, y: 10.0 },
                Coord { x: 1.0, y: 2.0 },
                Coord { x: 8.0, y: 2.0 },
                Coord { x: 8.0, y: 10.0 },
                Coord { x: 9.0, y: 10.0 },
                Coord { x: 9.0, y: 0.0 },
                Coord { x: 1.0, y: 0.0 },
            ]),
            vec![],
        );

        let result = clip_polygon(&u_shape, &bounds);
        assert!(result.is_some(), "U-shape should intersect the band");

        // Should produce a Polygon (SH produces a single polygon, not MultiPolygon)
        match result.unwrap() {
            Geometry::Polygon(p) => {
                // Verify all coords within bounds
                for coord in p.exterior().coords() {
                    assert!(
                        coord.x >= 0.0 && coord.x <= 10.0,
                        "x={} out of bounds",
                        coord.x
                    );
                    assert!(
                        coord.y >= 4.0 - 1e-10 && coord.y <= 6.0 + 1e-10,
                        "y={} out of bounds",
                        coord.y
                    );
                }
            }
            other => panic!("Expected Polygon, got {:?}", other),
        }
    }
}
