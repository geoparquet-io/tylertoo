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

/// Check if a rectangle is fully contained within the given bounds
fn is_fully_inside(rect: &Rect<f64>, bounds: &TileBounds) -> bool {
    rect.min().x >= bounds.lng_min
        && rect.max().x <= bounds.lng_max
        && rect.min().y >= bounds.lat_min
        && rect.max().y <= bounds.lat_max
}

// ============================================================================
// Geometry Clipping Functions
// ============================================================================

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
    // Quick rejection test using bounding box
    let poly_rect = poly.bounding_rect()?;
    if !intersects_bounds(&poly_rect, bounds) {
        return None;
    }

    // FAST PATH: If polygon is fully inside bounds, return as-is (no clipping needed)
    if is_fully_inside(&poly_rect, bounds) {
        return Some(Geometry::Polygon(poly.clone()));
    }

    // Use Sutherland-Hodgman for O(n) rectangle clipping
    sutherland_hodgman::clip_polygon_sh(poly, bounds)
}

/// Clip a multipolygon to bounds using Sutherland-Hodgman algorithm.
///
/// Applies a two-level bounding box filter:
/// 1. Overall MultiPolygon bbox check (fast rejection for the whole geometry)
/// 2. Per-polygon bbox check (skips sub-polygons that don't intersect the tile)
///
/// For MultiPolygons like Antarctica (7453 sub-polygons spanning the globe),
/// the per-polygon filter eliminates the vast majority of sub-polygons before
/// any clipping work is done.
fn clip_multipolygon(mp: &MultiPolygon<f64>, bounds: &TileBounds) -> Option<MultiPolygon<f64>> {
    // Level 1: Quick rejection using overall MultiPolygon bbox
    let mp_rect = mp.bounding_rect()?;
    if !intersects_bounds(&mp_rect, bounds) {
        return None;
    }

    // FAST PATH: If entire multipolygon is fully inside bounds, return as-is
    if is_fully_inside(&mp_rect, bounds) {
        return Some(mp.clone());
    }

    // Level 2: Per-polygon bbox filter + clip
    // Each polygon is individually tested with its own bounding box before
    // any clipping is attempted. This avoids expensive operations for
    // sub-polygons that are far from the tile.
    let mut clipped_polys = Vec::new();
    for poly in &mp.0 {
        // Per-polygon bbox filter: compute each polygon's bbox and check
        // intersection before calling into the clip pipeline
        let poly_rect = match poly.bounding_rect() {
            Some(r) => r,
            None => continue, // Degenerate polygon, skip
        };

        if !intersects_bounds(&poly_rect, bounds) {
            // This polygon's bbox doesn't intersect the tile -- skip entirely.
            // This is the key optimization: for a MultiPolygon with 7453 polygons
            // where only ~100 intersect the tile, we skip 7353 polygons here
            // without any clipping work.
            continue;
        }

        // FAST PATH: If this polygon is fully inside bounds, add as-is
        if is_fully_inside(&poly_rect, bounds) {
            clipped_polys.push(poly.clone());
            continue;
        }

        // Polygon intersects but isn't fully inside -- needs clipping with SH
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

    // ========== Bounding Box Pre-filter Tests ==========

    #[test]
    fn test_multipolygon_bbox_prefilter_skips_distant_polygons() {
        // Simulates an "Antarctica-like" MultiPolygon: many sub-polygons spread
        // across a wide geographic area, clipped to a small tile that only
        // intersects a handful of them.
        //
        // This verifies that per-polygon bbox filtering correctly:
        // 1. Produces output only for the intersecting polygons
        // 2. Returns None for the non-intersecting ones
        //
        // The tile covers a 10x10 degree area at (0,0)-(10,10).
        // We create 1000 polygons:
        //   - 990 are outside the tile (spread from x=20..200)
        //   - 10 are inside the tile (at x=1..9, y=1..9)
        let tile_bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let mut polygons = Vec::with_capacity(1000);

        // 10 polygons inside the tile
        for i in 0..10 {
            let x = 1.0 + (i as f64) * 0.8;
            let y = 1.0 + (i as f64) * 0.8;
            polygons.push(Polygon::new(
                LineString::from(vec![
                    Coord { x, y },
                    Coord { x: x + 0.5, y },
                    Coord {
                        x: x + 0.5,
                        y: y + 0.5,
                    },
                    Coord { x, y: y + 0.5 },
                    Coord { x, y },
                ]),
                vec![],
            ));
        }

        // 990 polygons outside the tile (far away, scattered in x=20..200)
        for i in 0..990 {
            let x = 20.0 + (i as f64) * 0.18;
            let y = -80.0 + (i as f64) * 0.16;
            polygons.push(Polygon::new(
                LineString::from(vec![
                    Coord { x, y },
                    Coord { x: x + 0.1, y },
                    Coord {
                        x: x + 0.1,
                        y: y + 0.1,
                    },
                    Coord { x, y: y + 0.1 },
                    Coord { x, y },
                ]),
                vec![],
            ));
        }

        let mp = MultiPolygon::new(polygons);

        // Clip to the tile
        let result = clip_multipolygon(&mp, &tile_bounds);

        // Should produce output (the 10 inside polygons)
        assert!(
            result.is_some(),
            "Should produce output for the intersecting polygons"
        );

        let clipped_mp = result.unwrap();
        // Should have approximately 10 polygons (the ones inside the tile)
        // Exact count may vary slightly due to clipping artifacts
        assert!(
            clipped_mp.0.len() >= 8 && clipped_mp.0.len() <= 12,
            "Expected ~10 output polygons, got {}",
            clipped_mp.0.len()
        );

        // All output coordinates should be within tile bounds
        for poly in &clipped_mp.0 {
            let bbox = poly.bounding_rect().unwrap();
            assert!(
                bbox.min().x >= 0.0 - 0.01 && bbox.max().x <= 10.0 + 0.01,
                "Output polygon x outside tile bounds: {:?}",
                bbox
            );
            assert!(
                bbox.min().y >= 0.0 - 0.01 && bbox.max().y <= 10.0 + 0.01,
                "Output polygon y outside tile bounds: {:?}",
                bbox
            );
        }
    }

    #[test]
    fn test_multipolygon_bbox_prefilter_all_outside() {
        // All polygons are outside the tile -- should return None quickly
        let tile_bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let polygons: Vec<Polygon<f64>> = (0..500)
            .map(|i| {
                let x = 50.0 + (i as f64) * 0.2;
                let y = 50.0 + (i as f64) * 0.1;
                Polygon::new(
                    LineString::from(vec![
                        Coord { x, y },
                        Coord { x: x + 0.1, y },
                        Coord {
                            x: x + 0.1,
                            y: y + 0.1,
                        },
                        Coord { x, y: y + 0.1 },
                        Coord { x, y },
                    ]),
                    vec![],
                )
            })
            .collect();

        let mp = MultiPolygon::new(polygons);
        let result = clip_multipolygon(&mp, &tile_bounds);
        assert!(
            result.is_none(),
            "All-outside multipolygon should return None"
        );
    }

    #[test]
    fn test_bbox_prefilter_large_polygon_preclip() {
        // A single large polygon spanning a huge area (-180 to +180 longitude)
        // is clipped to a small 10-degree tile. The pre-clip optimization should
        // reduce the coordinate count before sending to the expensive wagyu clipper.
        let tile_bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        // Build a large polygon with many coordinates spanning the entire globe.
        // This simulates a complex coastline polygon.
        let mut coords: Vec<Coord<f64>> = Vec::new();
        // Bottom edge: many points from -180 to +180
        for i in 0..360 {
            let x = -180.0 + i as f64;
            let y = -60.0 + (i as f64 * 0.1).sin() * 2.0; // Wavy bottom edge
            coords.push(Coord { x, y });
        }
        // Top edge: many points from +180 back to -180
        for i in (0..360).rev() {
            let x = -180.0 + i as f64;
            let y = 60.0 + (i as f64 * 0.1).cos() * 2.0; // Wavy top edge
            coords.push(Coord { x, y });
        }
        // Close the polygon
        coords.push(coords[0]);

        let large_poly = Polygon::new(LineString::from(coords.clone()), vec![]);

        // Total input coordinates
        let total_input_coords = coords.len();
        assert!(
            total_input_coords > 700,
            "Test polygon should have many coordinates, got {}",
            total_input_coords
        );

        // Clip to small tile
        let result = clip_polygon(&large_poly, &tile_bounds);
        assert!(result.is_some(), "Large polygon should intersect the tile");

        // Verify the clipped result is reasonable
        match result.unwrap() {
            Geometry::Polygon(p) => {
                let output_coords = p.exterior().coords().count();
                // The clipped polygon should have far fewer coordinates than input
                assert!(
                    output_coords < total_input_coords / 2,
                    "Clipped polygon should have fewer coords than input: {} vs {}",
                    output_coords,
                    total_input_coords
                );
            }
            Geometry::MultiPolygon(mp) => {
                let total_output: usize = mp.0.iter().map(|p| p.exterior().coords().count()).sum();
                assert!(
                    total_output < total_input_coords / 2,
                    "Clipped multipolygon should have fewer coords than input: {} vs {}",
                    total_output,
                    total_input_coords
                );
            }
            other => panic!("Expected Polygon or MultiPolygon, got {:?}", other),
        }
    }

    // ========== Sutherland-Hodgman Clipping Unit Tests ==========

    #[test]
    fn test_sutherland_hodgman_fully_inside() {
        // Polygon fully inside clip bounds -- should be unchanged
        use crate::sutherland_hodgman::clip_polygon_sh;

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

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some(), "Fully inside polygon should be preserved");
        match result.unwrap() {
            Geometry::Polygon(p) => {
                // Should preserve all 4 vertices + closing
                assert_eq!(
                    p.exterior().0.len(),
                    5,
                    "Should have 5 coords (4 vertices + close)"
                );
            }
            other => panic!("Expected Polygon, got {:?}", other),
        }
    }

    #[test]
    fn test_sutherland_hodgman_fully_outside() {
        // Polygon fully outside clip bounds -- should return None
        use crate::sutherland_hodgman::clip_polygon_sh;

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

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_none(), "Fully outside polygon should be empty");
    }

    #[test]
    fn test_sutherland_hodgman_partial_clip() {
        // Polygon overlapping the right edge of the bounds
        use crate::sutherland_hodgman::clip_polygon_sh;

        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: 5.0, y: 2.0 },
                Coord { x: 15.0, y: 2.0 },
                Coord { x: 15.0, y: 8.0 },
                Coord { x: 5.0, y: 8.0 },
                Coord { x: 5.0, y: 2.0 },
            ]),
            vec![],
        );

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(
            result.is_some(),
            "Partially overlapping polygon should produce output"
        );
        // Verify all result coords are within bounds
        match result.unwrap() {
            Geometry::Polygon(p) => {
                for coord in p.exterior().coords() {
                    assert!(
                        coord.x >= 0.0 - 0.001 && coord.x <= 10.0 + 0.001,
                        "x out of bounds: {}",
                        coord.x
                    );
                    assert!(
                        coord.y >= 0.0 - 0.001 && coord.y <= 10.0 + 0.001,
                        "y out of bounds: {}",
                        coord.y
                    );
                }
            }
            other => panic!("Expected Polygon, got {:?}", other),
        }
    }

    #[test]
    fn test_sutherland_hodgman_large_polygon_reduction() {
        // A polygon with many coordinates spanning a large area, clipped to a
        // small box. Tests that Sutherland-Hodgman reduces coordinate count.
        use crate::sutherland_hodgman::clip_polygon_sh;

        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        // Create a polygon with 720 coords spanning -180 to +180
        let mut coords = Vec::new();
        for i in 0..360 {
            coords.push(Coord {
                x: -180.0 + i as f64,
                y: -50.0,
            });
        }
        for i in (0..360).rev() {
            coords.push(Coord {
                x: -180.0 + i as f64,
                y: 50.0,
            });
        }
        coords.push(coords[0]); // close

        let input_count = coords.len();
        let poly = Polygon::new(LineString::from(coords), vec![]);

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some(), "Clipped polygon should not be empty");

        match result.unwrap() {
            Geometry::Polygon(p) => {
                let output_count = p.exterior().0.len();
                assert!(
                    output_count < input_count / 10,
                    "Sutherland-Hodgman should dramatically reduce coordinates: {} -> {}",
                    input_count,
                    output_count
                );
            }
            other => panic!("Expected Polygon, got {:?}", other),
        }
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
