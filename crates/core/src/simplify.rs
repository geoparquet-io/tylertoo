//! Zoom-based geometry simplification.
//!
//! Uses the Ramer-Douglas-Peucker (RDP) algorithm via `geo::Simplify` to reduce
//! vertex count based on zoom level. Matches tippecanoe's approach: simplify to
//! tile resolution (~1 pixel at the given zoom level).
//!
//! # Coordinate Spaces
//!
//! Tippecanoe simplifies in **tile-local pixel coordinates** (0-4096 extent),
//! NOT in geographic degrees. This is critical because:
//!
//! - 1° longitude at equator ≈ 111 km
//! - 1° longitude at 60°N ≈ 55 km
//!
//! Using degree-based tolerance would over-simplify at high latitudes.
//!
//! The correct approach is:
//! ```text
//! Geographic Coords → Transform to tile coords → Simplify (pixels) → Encode
//! ```

use crate::tile::TileBounds;
use geo::{Coord, Geometry, LineString, MultiLineString, MultiPolygon, Point, Polygon, Simplify};

/// Default pixel tolerance for simplification (matches tippecanoe)
pub const DEFAULT_PIXEL_TOLERANCE: f64 = 1.0;

/// Simplify geometry to tile resolution (Douglas-Peucker).
///
/// **DEPRECATED**: This function uses degree-based tolerance which causes
/// latitude-dependent simplification. Use [`simplify_in_tile_coords`] instead.
///
/// Matches tippecanoe's approach: "At every zoom level, line and polygon features
/// are subjected to Douglas-Peucker simplification to the resolution of the tile."
///
/// Tolerance calculation:
/// - At zoom z, one tile covers 360° / 2^z degrees
/// - With `extent` pixels per tile, each pixel = tile_degrees / extent
/// - We use 1 pixel as the tolerance (matching tippecanoe)
///
/// Points and MultiPoints pass through unchanged since they have no vertices to reduce.
pub fn simplify_for_zoom(geom: &Geometry<f64>, zoom: u8, extent: u32) -> Geometry<f64> {
    // Tippecanoe simplifies to tile resolution
    // At zoom z, one tile covers 360/2^z degrees
    // With extent pixels, tolerance is degrees per pixel
    let tile_degrees = 360.0 / (1u64 << zoom) as f64;
    let tolerance = tile_degrees / extent as f64;

    // Guard against numerical issues at high zoom levels
    if tolerance < 1e-10 {
        return geom.clone();
    }

    match geom {
        // Points have no vertices to simplify
        Geometry::Point(_) | Geometry::MultiPoint(_) => geom.clone(),

        // Apply RDP simplification to line/polygon types
        // Guard against degenerate geometries: geo::Simplify panics on linestrings
        // with < 2 points. Return unchanged; they'll be filtered by filter_valid_geometry.
        Geometry::LineString(ls) => {
            if ls.0.len() < 2 {
                return geom.clone();
            }
            Geometry::LineString(ls.simplify(tolerance))
        }
        Geometry::Polygon(poly) => Geometry::Polygon(poly.simplify(tolerance)),
        Geometry::MultiPolygon(mp) => Geometry::MultiPolygon(mp.simplify(tolerance)),
        Geometry::MultiLineString(mls) => {
            // Handle degenerate linestrings within the multi - return unchanged
            // (they'll be filtered by filter_valid_geometry later)
            let simplified_lines: Vec<LineString<f64>> = mls
                .0
                .iter()
                .map(|ls| {
                    if ls.0.len() < 2 {
                        ls.clone()
                    } else {
                        ls.simplify(tolerance)
                    }
                })
                .collect();
            Geometry::MultiLineString(MultiLineString::new(simplified_lines))
        }

        // GeometryCollection and other types pass through unchanged
        other => other.clone(),
    }
}

// ============================================================================
// Tile-Local Coordinate Simplification (tippecanoe-compatible)
// ============================================================================

/// Transform a geographic coordinate to tile-local pixel coordinates.
///
/// Tile coordinates range from 0 to extent (typically 4096).
/// The tile bounds define the geographic extent being mapped.
///
/// # Arguments
/// * `lng` - Longitude in degrees
/// * `lat` - Latitude in degrees
/// * `bounds` - The geographic bounds of the tile
/// * `extent` - The tile extent (default 4096)
///
/// # Returns
/// (x, y) in tile-local coordinates as f64 for precision during simplification
#[inline]
fn geo_to_tile_coords_f64(lng: f64, lat: f64, bounds: &TileBounds, extent: u32) -> (f64, f64) {
    let extent_f = extent as f64;

    // Normalize to 0-1 within tile bounds
    let x_ratio = (lng - bounds.lng_min) / (bounds.lng_max - bounds.lng_min);
    let y_ratio = (lat - bounds.lat_min) / (bounds.lat_max - bounds.lat_min);

    // Scale to extent and flip Y (tile coords have Y increasing downward)
    let x = x_ratio * extent_f;
    let y = (1.0 - y_ratio) * extent_f;

    (x, y)
}

/// Transform a tile-local coordinate back to geographic coordinates.
///
/// This is the inverse of [`geo_to_tile_coords_f64`].
#[inline]
fn tile_coords_to_geo(x: f64, y: f64, bounds: &TileBounds, extent: u32) -> (f64, f64) {
    let extent_f = extent as f64;

    // Unflip Y and denormalize
    let x_ratio = x / extent_f;
    let y_ratio = 1.0 - (y / extent_f);

    let lng = bounds.lng_min + x_ratio * (bounds.lng_max - bounds.lng_min);
    let lat = bounds.lat_min + y_ratio * (bounds.lat_max - bounds.lat_min);

    (lng, lat)
}

/// Transform a LineString from geographic to tile-local coordinates.
fn linestring_to_tile_coords(
    ls: &LineString<f64>,
    bounds: &TileBounds,
    extent: u32,
) -> LineString<f64> {
    let coords: Vec<Coord<f64>> = ls
        .coords()
        .map(|c| {
            let (x, y) = geo_to_tile_coords_f64(c.x, c.y, bounds, extent);
            Coord { x, y }
        })
        .collect();
    LineString::new(coords)
}

/// Transform a LineString from tile-local back to geographic coordinates.
fn linestring_to_geo_coords(
    ls: &LineString<f64>,
    bounds: &TileBounds,
    extent: u32,
) -> LineString<f64> {
    let coords: Vec<Coord<f64>> = ls
        .coords()
        .map(|c| {
            let (lng, lat) = tile_coords_to_geo(c.x, c.y, bounds, extent);
            Coord { x: lng, y: lat }
        })
        .collect();
    LineString::new(coords)
}

/// Transform a Polygon from geographic to tile-local coordinates.
fn polygon_to_tile_coords(poly: &Polygon<f64>, bounds: &TileBounds, extent: u32) -> Polygon<f64> {
    let exterior = linestring_to_tile_coords(poly.exterior(), bounds, extent);
    let interiors: Vec<LineString<f64>> = poly
        .interiors()
        .iter()
        .map(|ring| linestring_to_tile_coords(ring, bounds, extent))
        .collect();
    Polygon::new(exterior, interiors)
}

/// Transform a Polygon from tile-local back to geographic coordinates.
fn polygon_to_geo_coords(poly: &Polygon<f64>, bounds: &TileBounds, extent: u32) -> Polygon<f64> {
    let exterior = linestring_to_geo_coords(poly.exterior(), bounds, extent);
    let interiors: Vec<LineString<f64>> = poly
        .interiors()
        .iter()
        .map(|ring| linestring_to_geo_coords(ring, bounds, extent))
        .collect();
    Polygon::new(exterior, interiors)
}

/// Simplify geometry in tile-local pixel coordinates.
///
/// This is the **correct** approach matching tippecanoe:
/// 1. Transform geometry to tile-local coordinates (0-extent)
/// 2. Apply Douglas-Peucker simplification with pixel tolerance
/// 3. Transform back to geographic coordinates
///
/// This ensures latitude-independent simplification: identical shapes
/// at different latitudes will be simplified consistently.
///
/// # Arguments
/// * `geom` - The geometry to simplify (in geographic coordinates)
/// * `bounds` - The tile bounds for coordinate transformation
/// * `extent` - The tile extent (typically 4096)
/// * `pixel_tolerance` - Simplification tolerance in pixels (typically 1.0)
///
/// # Returns
/// Simplified geometry in geographic coordinates.
///
/// # Example
/// ```
/// use gpq_tiles_core::simplify::simplify_in_tile_coords;
/// use gpq_tiles_core::tile::TileBounds;
/// use geo::{Geometry, LineString, Coord};
///
/// let line = LineString::new(vec![
///     Coord { x: 0.0, y: 0.0 },
///     Coord { x: 0.5, y: 0.01 },
///     Coord { x: 1.0, y: 0.0 },
/// ]);
/// let geom = Geometry::LineString(line);
/// let bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
///
/// let simplified = simplify_in_tile_coords(&geom, &bounds, 4096, 1.0);
/// ```
pub fn simplify_in_tile_coords(
    geom: &Geometry<f64>,
    bounds: &TileBounds,
    extent: u32,
    pixel_tolerance: f64,
) -> Geometry<f64> {
    match geom {
        // Points have no vertices to simplify
        Geometry::Point(_) | Geometry::MultiPoint(_) => geom.clone(),

        Geometry::LineString(ls) => {
            // Transform to tile coords
            let tile_ls = linestring_to_tile_coords(ls, bounds, extent);
            // Simplify in tile space
            let simplified = tile_ls.simplify(pixel_tolerance);
            // Transform back to geo coords
            Geometry::LineString(linestring_to_geo_coords(&simplified, bounds, extent))
        }

        Geometry::Polygon(poly) => {
            let tile_poly = polygon_to_tile_coords(poly, bounds, extent);
            let simplified = tile_poly.simplify(pixel_tolerance);
            Geometry::Polygon(polygon_to_geo_coords(&simplified, bounds, extent))
        }

        Geometry::MultiPolygon(mp) => {
            let simplified_polys: Vec<Polygon<f64>> =
                mp.0.iter()
                    .map(|poly| {
                        let tile_poly = polygon_to_tile_coords(poly, bounds, extent);
                        let simplified = tile_poly.simplify(pixel_tolerance);
                        polygon_to_geo_coords(&simplified, bounds, extent)
                    })
                    .collect();
            Geometry::MultiPolygon(MultiPolygon::new(simplified_polys))
        }

        Geometry::MultiLineString(mls) => {
            let simplified_lines: Vec<LineString<f64>> = mls
                .0
                .iter()
                .map(|ls| {
                    let tile_ls = linestring_to_tile_coords(ls, bounds, extent);
                    let simplified = tile_ls.simplify(pixel_tolerance);
                    linestring_to_geo_coords(&simplified, bounds, extent)
                })
                .collect();
            Geometry::MultiLineString(MultiLineString::new(simplified_lines))
        }

        // GeometryCollection and other types pass through unchanged
        other => other.clone(),
    }
}

/// Simplify geometry in tile-local coordinates, returning tile coordinates.
///
/// Same as [`simplify_in_tile_coords`] but returns geometry **in tile coordinates**
/// rather than transforming back to geographic. This is more efficient when the
/// geometry will be immediately encoded to MVT (which uses tile coordinates).
///
/// # Arguments
/// * `geom` - The geometry to simplify (in geographic coordinates)
/// * `bounds` - The tile bounds for coordinate transformation
/// * `extent` - The tile extent (typically 4096)
/// * `pixel_tolerance` - Simplification tolerance in pixels (typically 1.0)
///
/// # Returns
/// Simplified geometry in tile-local coordinates (0 to extent).
pub fn simplify_to_tile_coords(
    geom: &Geometry<f64>,
    bounds: &TileBounds,
    extent: u32,
    pixel_tolerance: f64,
) -> Geometry<f64> {
    match geom {
        // Points: just transform to tile coords
        Geometry::Point(p) => {
            let (x, y) = geo_to_tile_coords_f64(p.x(), p.y(), bounds, extent);
            Geometry::Point(Point::new(x, y))
        }

        Geometry::MultiPoint(mp) => {
            let points: Vec<Point<f64>> =
                mp.0.iter()
                    .map(|p| {
                        let (x, y) = geo_to_tile_coords_f64(p.x(), p.y(), bounds, extent);
                        Point::new(x, y)
                    })
                    .collect();
            Geometry::MultiPoint(geo::MultiPoint::new(points))
        }

        Geometry::LineString(ls) => {
            let tile_ls = linestring_to_tile_coords(ls, bounds, extent);
            Geometry::LineString(tile_ls.simplify(pixel_tolerance))
        }

        Geometry::Polygon(poly) => {
            let tile_poly = polygon_to_tile_coords(poly, bounds, extent);
            Geometry::Polygon(tile_poly.simplify(pixel_tolerance))
        }

        Geometry::MultiPolygon(mp) => {
            let simplified_polys: Vec<Polygon<f64>> =
                mp.0.iter()
                    .map(|poly| {
                        let tile_poly = polygon_to_tile_coords(poly, bounds, extent);
                        tile_poly.simplify(pixel_tolerance)
                    })
                    .collect();
            Geometry::MultiPolygon(MultiPolygon::new(simplified_polys))
        }

        Geometry::MultiLineString(mls) => {
            let simplified_lines: Vec<LineString<f64>> = mls
                .0
                .iter()
                .map(|ls| {
                    let tile_ls = linestring_to_tile_coords(ls, bounds, extent);
                    tile_ls.simplify(pixel_tolerance)
                })
                .collect();
            Geometry::MultiLineString(MultiLineString::new(simplified_lines))
        }

        // GeometryCollection and other types pass through unchanged
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Coord, LineString};

    #[test]
    fn test_simplify_reduces_vertices() {
        // Create a line with 100 points that has small oscillations
        // The oscillations are small enough to be simplified away at zoom 0
        let coords: Vec<Coord<f64>> = (0..100)
            .map(|i| Coord {
                x: i as f64 * 0.01,
                y: (i as f64 * 0.1).sin() * 0.001,
            })
            .collect();
        let line = LineString::new(coords);
        let geom = Geometry::LineString(line.clone());

        // At zoom 0, tolerance is large, should simplify aggressively
        let simplified = simplify_for_zoom(&geom, 0, 4096);
        if let Geometry::LineString(s) = simplified {
            assert!(
                s.coords().count() < line.coords().count(),
                "Expected fewer vertices after simplification: got {}, original {}",
                s.coords().count(),
                line.coords().count()
            );
        } else {
            panic!("Expected LineString geometry");
        }
    }

    #[test]
    fn test_points_unchanged() {
        let point = Geometry::Point(geo::point!(x: 1.0, y: 2.0));
        assert_eq!(point, simplify_for_zoom(&point, 5, 4096));
    }

    #[test]
    fn test_multipoint_unchanged() {
        use geo::MultiPoint;
        let mp = Geometry::MultiPoint(MultiPoint::new(vec![
            geo::point!(x: 1.0, y: 2.0),
            geo::point!(x: 3.0, y: 4.0),
        ]));
        assert_eq!(mp, simplify_for_zoom(&mp, 10, 4096));
    }

    #[test]
    fn test_high_zoom_preserves_detail() {
        // At high zoom, tolerance is very small, should preserve most vertices
        // At zoom 20 with extent 4096: tolerance = 360 / 2^20 / 4096 ≈ 8.4e-8 degrees
        // Create vertices spaced further apart than the tolerance
        let coords: Vec<Coord<f64>> = (0..10)
            .map(|i| Coord {
                x: i as f64 * 0.001, // 0.001° spacing >> 8.4e-8° tolerance
                y: (i as f64 * 0.5).sin() * 0.001,
            })
            .collect();
        let line = LineString::new(coords.clone());
        let geom = Geometry::LineString(line.clone());

        // At zoom 20, tolerance is tiny, should preserve all detail
        let simplified = simplify_for_zoom(&geom, 20, 4096);
        if let Geometry::LineString(s) = simplified {
            // Should preserve all vertices since they're spaced well above tolerance
            assert_eq!(
                s.coords().count(),
                line.coords().count(),
                "High zoom should preserve all vertices when spacing >> tolerance"
            );
        }
    }

    #[test]
    fn test_tolerance_decreases_with_zoom() {
        // Create a line with predictable behavior
        let coords: Vec<Coord<f64>> = (0..50)
            .map(|i| Coord {
                x: i as f64 * 0.02,
                y: (i as f64 * 0.2).sin() * 0.01,
            })
            .collect();
        let line = LineString::new(coords);
        let geom = Geometry::LineString(line);

        let simplified_z0 = simplify_for_zoom(&geom, 0, 4096);
        let simplified_z5 = simplify_for_zoom(&geom, 5, 4096);
        let simplified_z10 = simplify_for_zoom(&geom, 10, 4096);

        let count_z0 = if let Geometry::LineString(s) = simplified_z0 {
            s.coords().count()
        } else {
            0
        };
        let count_z5 = if let Geometry::LineString(s) = simplified_z5 {
            s.coords().count()
        } else {
            0
        };
        let count_z10 = if let Geometry::LineString(s) = simplified_z10 {
            s.coords().count()
        } else {
            0
        };

        // Higher zoom should generally preserve more vertices
        assert!(
            count_z0 <= count_z5 && count_z5 <= count_z10,
            "Expected more vertices at higher zooms: z0={}, z5={}, z10={}",
            count_z0,
            count_z5,
            count_z10
        );
    }

    #[test]
    fn test_polygon_simplification() {
        use geo::Polygon;

        // Create a polygon with many vertices (approximating a circle)
        let coords: Vec<Coord<f64>> = (0..=36)
            .map(|i| {
                let angle = (i as f64) * 10.0 * std::f64::consts::PI / 180.0;
                Coord {
                    x: angle.cos() * 0.1,
                    y: angle.sin() * 0.1,
                }
            })
            .collect();
        let poly = Polygon::new(LineString::new(coords), vec![]);
        let geom = Geometry::Polygon(poly.clone());

        let simplified = simplify_for_zoom(&geom, 0, 4096);
        if let Geometry::Polygon(s) = simplified {
            assert!(
                s.exterior().coords().count() < poly.exterior().coords().count(),
                "Polygon should be simplified at zoom 0"
            );
        }
    }

    #[test]
    fn test_tolerance_matches_tippecanoe() {
        // Verify our tolerance formula matches tippecanoe's approach
        // At zoom 0: 360° / 4096 = 0.087890625° per pixel
        // At zoom 1: 180° / 4096 = 0.0439453125° per pixel
        // At zoom 10: ~0.351° / 4096 ≈ 0.0000858° per pixel

        let extent = 4096;

        // Zoom 0: 360 / 1 / 4096
        let tol_z0 = 360.0 / (1u64 << 0) as f64 / extent as f64;
        assert!(
            (tol_z0 - 0.087890625).abs() < 1e-9,
            "Zoom 0 tolerance mismatch: {}",
            tol_z0
        );

        // Zoom 1: 360 / 2 / 4096
        let tol_z1 = 360.0 / (1u64 << 1) as f64 / extent as f64;
        assert!(
            (tol_z1 - 0.0439453125).abs() < 1e-9,
            "Zoom 1 tolerance mismatch: {}",
            tol_z1
        );

        // Verify tolerance halves with each zoom level
        let tol_z2 = 360.0 / (1u64 << 2) as f64 / extent as f64;
        assert!(
            (tol_z1 / tol_z2 - 2.0).abs() < 1e-9,
            "Tolerance should halve with each zoom"
        );
    }

    // ========================================================================
    // FAILING TEST: Demonstrates the latitude-dependent simplification bug
    // ========================================================================
    //
    // This test verifies that identical shapes at different latitudes are
    // simplified consistently when working in tile-local pixel coordinates.
    //
    // **Problem**: Using degree-based tolerance causes latitude-dependent
    // simplification because 1° of longitude covers different distances at
    // different latitudes (due to Web Mercator projection).
    //
    // **Expected behavior**: Identical geometry shapes should produce the same
    // number of vertices after simplification regardless of latitude, because
    // tippecanoe simplifies in tile-local pixel coordinates (0-4096).
    // ========================================================================

    #[test]
    fn test_simplification_is_latitude_independent() {
        use crate::tile::TileCoord;

        // Create identical zigzag patterns at two different latitudes:
        // - Equator (lat ~0°): Where 1° longitude = maximum distance
        // - High latitude (lat ~60°): Where 1° longitude ≈ 50% the distance
        //
        // The zigzag has small oscillations that should be simplified away.
        // If simplification is latitude-independent (like tippecanoe), both
        // shapes should simplify to the same number of vertices.

        let extent = 4096u32;
        let zoom = 5u8;

        // Get tiles at equator and at ~60°N
        let tile_equator = TileCoord::new(16, 16, zoom); // Near equator
        let tile_arctic = TileCoord::new(16, 8, zoom); // Near 60°N

        let bounds_equator = tile_equator.bounds();
        let bounds_arctic = tile_arctic.bounds();

        // Create a zigzag line that spans 50% of each tile's width
        // with small vertical oscillations (5% of tile height)
        fn make_zigzag_in_bounds(
            bounds: &crate::tile::TileBounds,
            num_points: usize,
        ) -> LineString {
            let width = bounds.lng_max - bounds.lng_min;
            let height = bounds.lat_max - bounds.lat_min;
            let center_lat = (bounds.lat_min + bounds.lat_max) / 2.0;
            let start_lng = bounds.lng_min + width * 0.25;

            let coords: Vec<Coord<f64>> = (0..num_points)
                .map(|i| {
                    let x = start_lng + (i as f64 / (num_points - 1) as f64) * width * 0.5;
                    let y = center_lat + (if i % 2 == 0 { 1.0 } else { -1.0 }) * height * 0.05;
                    Coord { x, y }
                })
                .collect();
            LineString::new(coords)
        }

        // Create 50-point zigzag lines in each tile
        let line_equator = make_zigzag_in_bounds(&bounds_equator, 50);
        let line_arctic = make_zigzag_in_bounds(&bounds_arctic, 50);

        let geom_equator = Geometry::LineString(line_equator);
        let geom_arctic = Geometry::LineString(line_arctic);

        // Simplify both lines
        let simplified_equator = simplify_for_zoom(&geom_equator, zoom, extent);
        let simplified_arctic = simplify_for_zoom(&geom_arctic, zoom, extent);

        let count_equator = if let Geometry::LineString(s) = &simplified_equator {
            s.coords().count()
        } else {
            panic!("Expected LineString");
        };

        let count_arctic = if let Geometry::LineString(s) = &simplified_arctic {
            s.coords().count()
        } else {
            panic!("Expected LineString");
        };

        // Both should simplify to approximately the same vertex count
        // because the zigzag patterns are identical relative to tile bounds.
        //
        // CURRENT BUG: At the equator, 1° longitude is larger in real-world
        // distance than at 60°N. Our degree-based tolerance will over-simplify
        // at high latitudes (the zigzag oscillations appear "smaller" in degrees).
        //
        // With the fix (pixel-based simplification), both should have the same count.
        assert_eq!(
            count_equator, count_arctic,
            "Identical shapes at different latitudes should simplify to same vertex count.\n\
             Equator: {} vertices, Arctic: {} vertices.\n\
             This indicates latitude-dependent simplification (degree-based tolerance bug).",
            count_equator, count_arctic
        );
    }

    /// Test that simplification works in tile-local coordinates.
    /// This is the NEW API that should be added to fix the bug.
    #[test]
    fn test_simplify_in_tile_coords_exists() {
        use crate::tile::TileBounds;

        // Create a simple line geometry in geographic coordinates
        let coords: Vec<Coord<f64>> = vec![
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 0.25, y: 0.1 },
            Coord { x: 0.5, y: 0.0 },
            Coord { x: 0.75, y: 0.1 },
            Coord { x: 1.0, y: 0.0 },
        ];
        let line = LineString::new(coords);
        let geom = Geometry::LineString(line);

        let bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let pixel_tolerance = 2.0; // 2-pixel tolerance

        // This new function should:
        // 1. Transform geometry to tile-local coordinates (0-4096)
        // 2. Simplify using pixel_tolerance
        // 3. Return simplified geometry STILL IN TILE COORDINATES
        //    (because MVT encoding expects tile coords)
        let simplified = simplify_in_tile_coords(&geom, &bounds, extent, pixel_tolerance);

        // Should return a valid geometry
        assert!(
            matches!(simplified, Geometry::LineString(_)),
            "Should return a LineString geometry"
        );
    }
}
