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

use crate::tile::{TileBounds, TileCoord};
use crate::world_coord::{WorldCoord, WORLD_SCALE};
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
    // Delegates to the shared MVT transform (linear X, Web Mercator Y) so
    // simplification tolerances are measured in the same tile pixels that
    // MVT encoding will produce.
    crate::mvt::geo_to_tile_coords_unrounded(lng, lat, bounds, extent)
}

/// Transform a tile-local coordinate back to geographic coordinates.
///
/// This is the inverse of [`geo_to_tile_coords_f64`].
#[inline]
fn tile_coords_to_geo(x: f64, y: f64, bounds: &TileBounds, extent: u32) -> (f64, f64) {
    crate::mvt::tile_coords_to_geo_f64(x, y, bounds, extent)
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

// ============================================================================
// WorldCoord-based Simplification (Phase 1: integer coordinate migration)
// ============================================================================
//
// These functions provide simplification through the WorldCoord integer
// coordinate system, matching tippecanoe's internal representation.
//
// The pipeline is:
//   WorldCoord → tile-local f64 → RDP simplify → tile-local f64 → WorldCoord
//
// This is preferred over the geographic coordinate path because:
// 1. Web Mercator projection is applied once (in lng_lat_to_world)
// 2. Tile-local conversion from WorldCoord is a simple linear operation
// 3. Simplification is automatically latitude-independent (projection is baked in)
// ============================================================================

/// Convert a WorldCoord to tile-local f64 coordinates for RDP simplification.
///
/// This is the WorldCoord equivalent of [`geo_to_tile_coords_f64`]. The key
/// difference is that WorldCoord already has Web Mercator projection baked in,
/// so the conversion is a simple linear transformation.
///
/// # Arguments
/// * `coord` - World coordinate
/// * `tile` - The tile to project into
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// (x, y) in tile-local coordinates as f64 for precision during simplification
#[inline]
fn world_to_tile_local_f64(coord: WorldCoord, tile: &TileCoord, extent: u32) -> (f64, f64) {
    let extent_f = extent as f64;

    if tile.z == 0 {
        // At zoom 0, the entire world is one tile
        let x = (coord.x as f64) / (WORLD_SCALE as f64) * extent_f;
        let y = (coord.y as f64) / (WORLD_SCALE as f64) * extent_f;
        return (x, y);
    }

    let shift = 32 - tile.z as u32;
    let tile_size = 1_u64 << shift;

    // World position of tile's top-left corner
    let tile_x = (tile.x as u64) << shift;
    let tile_y = (tile.y as u64) << shift;

    // Position within tile, scaled to extent (f64 for RDP precision)
    let x = (coord.x as f64 - tile_x as f64) / tile_size as f64 * extent_f;
    let y = (coord.y as f64 - tile_y as f64) / tile_size as f64 * extent_f;

    (x, y)
}

/// Convert tile-local f64 coordinates back to WorldCoord.
///
/// This is the inverse of [`world_to_tile_local_f64`].
///
/// # Arguments
/// * `x` - X coordinate in tile-local space
/// * `y` - Y coordinate in tile-local space
/// * `tile` - The tile the coordinates are relative to
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// WorldCoord in global space
#[inline]
fn tile_local_f64_to_world(x: f64, y: f64, tile: &TileCoord, extent: u32) -> WorldCoord {
    let extent_f = extent as f64;

    if tile.z == 0 {
        let world_x = (x / extent_f * WORLD_SCALE as f64) as u32;
        let world_y = (y / extent_f * WORLD_SCALE as f64) as u32;
        return WorldCoord::new(world_x, world_y);
    }

    let shift = 32 - tile.z as u32;
    let tile_size = 1_u64 << shift;

    // World position of tile's top-left corner
    let tile_world_x = (tile.x as u64) << shift;
    let tile_world_y = (tile.y as u64) << shift;

    // Convert local f64 to world coordinates
    let world_x = (tile_world_x as f64 + x / extent_f * tile_size as f64) as u32;
    let world_y = (tile_world_y as f64 + y / extent_f * tile_size as f64) as u32;

    WorldCoord::new(world_x, world_y)
}

/// Transform a slice of WorldCoords to a tile-local LineString<f64> for simplification.
fn world_coords_to_tile_linestring(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
) -> LineString<f64> {
    let points: Vec<Coord<f64>> = coords
        .iter()
        .map(|wc| {
            let (x, y) = world_to_tile_local_f64(*wc, tile, extent);
            Coord { x, y }
        })
        .collect();
    LineString::new(points)
}

/// Transform a tile-local LineString<f64> back to WorldCoords.
fn tile_linestring_to_world_coords(
    ls: &LineString<f64>,
    tile: &TileCoord,
    extent: u32,
) -> Vec<WorldCoord> {
    ls.coords()
        .map(|c| tile_local_f64_to_world(c.x, c.y, tile, extent))
        .collect()
}

/// Simplify a polyline given as WorldCoords using Douglas-Peucker in tile-local space.
///
/// This is the primary WorldCoord simplification function. It:
/// 1. Converts WorldCoords to tile-local f64 coordinates
/// 2. Applies RDP simplification with the given pixel tolerance
/// 3. Converts back to WorldCoords
///
/// # Arguments
/// * `coords` - Polyline vertices in world coordinates
/// * `tile` - The tile context for coordinate transformation
/// * `extent` - Tile extent (typically 4096)
/// * `pixel_tolerance` - Simplification tolerance in pixels (typically 1.0)
///
/// # Returns
/// Simplified polyline as a Vec of WorldCoords.
/// Returns the input unchanged if fewer than 2 points.
pub fn simplify_world_linestring(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
    pixel_tolerance: f64,
) -> Vec<WorldCoord> {
    if coords.len() < 2 {
        return coords.to_vec();
    }

    // Transform to tile-local f64
    let tile_ls = world_coords_to_tile_linestring(coords, tile, extent);

    // Apply RDP simplification in tile pixel space
    let simplified = tile_ls.simplify(pixel_tolerance);

    // Transform back to WorldCoords
    tile_linestring_to_world_coords(&simplified, tile, extent)
}

/// Simplify a polygon ring (exterior or interior) given as WorldCoords.
///
/// Same as [`simplify_world_linestring`] but ensures the ring is closed
/// (first == last point) after simplification.
pub fn simplify_world_ring(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
    pixel_tolerance: f64,
) -> Vec<WorldCoord> {
    if coords.len() < 4 {
        // A valid ring needs at least 4 points (3 unique + closing)
        return coords.to_vec();
    }

    let tile_ls = world_coords_to_tile_linestring(coords, tile, extent);
    let simplified = tile_ls.simplify(pixel_tolerance);
    let mut result = tile_linestring_to_world_coords(&simplified, tile, extent);

    // Ensure the ring is closed
    if result.len() >= 2 && result.first() != result.last() {
        result.push(result[0]);
    }

    result
}

/// Get the simplified vertex count for a WorldCoord polyline without
/// materializing the result. Useful for feature dropping decisions.
///
/// # Arguments
/// * `coords` - Polyline vertices in world coordinates
/// * `tile` - The tile context for coordinate transformation
/// * `extent` - Tile extent (typically 4096)
/// * `pixel_tolerance` - Simplification tolerance in pixels
///
/// # Returns
/// Number of vertices after simplification
pub fn world_simplified_vertex_count(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
    pixel_tolerance: f64,
) -> usize {
    if coords.len() < 2 {
        return coords.len();
    }

    let tile_ls = world_coords_to_tile_linestring(coords, tile, extent);
    let simplified = tile_ls.simplify(pixel_tolerance);
    simplified.coords().count()
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

    // ========================================================================
    // WorldCoord-based Simplification Tests (Phase 1)
    // ========================================================================

    mod world_coord_tests {
        use super::*;
        use crate::tile::TileCoord;
        use crate::world_coord::{lng_lat_to_world, WorldCoord, WORLD_HALF};

        // ---- Coordinate conversion round-trip tests ----

        #[test]
        fn test_world_to_tile_local_f64_round_trip() {
            // A point at the center of a tile should round-trip through f64 conversion
            let tile = TileCoord::new(100, 200, 10);
            let extent = 4096u32;

            // Create a world coordinate near the center of this tile
            let shift = 32 - 10u32;
            let tile_origin_x = (100u64) << shift;
            let tile_origin_y = (200u64) << shift;
            let tile_size = 1u64 << shift;
            let center_x = (tile_origin_x + tile_size / 2) as u32;
            let center_y = (tile_origin_y + tile_size / 2) as u32;
            let original = WorldCoord::new(center_x, center_y);

            // Convert to tile-local f64
            let (local_x, local_y) = world_to_tile_local_f64(original, &tile, extent);

            // Should be near center of tile (extent/2)
            assert!(
                (local_x - 2048.0).abs() < 1.0,
                "Expected local_x near 2048, got {}",
                local_x
            );
            assert!(
                (local_y - 2048.0).abs() < 1.0,
                "Expected local_y near 2048, got {}",
                local_y
            );

            // Convert back to WorldCoord
            let recovered = tile_local_f64_to_world(local_x, local_y, &tile, extent);

            // Should be very close to original (within rounding)
            let dx = (original.x as i64 - recovered.x as i64).unsigned_abs();
            let dy = (original.y as i64 - recovered.y as i64).unsigned_abs();
            // Allow tolerance of tile_size/extent (~1 world unit per pixel at this zoom)
            let max_error = (tile_size / extent as u64) + 1;
            assert!(
                dx <= max_error,
                "X round-trip error too large: {} (max {})",
                dx,
                max_error
            );
            assert!(
                dy <= max_error,
                "Y round-trip error too large: {} (max {})",
                dy,
                max_error
            );
        }

        #[test]
        fn test_world_to_tile_local_f64_zoom_0() {
            let tile = TileCoord::new(0, 0, 0);
            let extent = 4096u32;

            // Null Island (center of world) at zoom 0
            let center = WorldCoord::new(WORLD_HALF, WORLD_HALF);
            let (lx, ly) = world_to_tile_local_f64(center, &tile, extent);

            assert!(
                (lx - 2048.0).abs() < 1.0,
                "Center of world at z0 should be at extent/2, got {}",
                lx
            );
            assert!(
                (ly - 2048.0).abs() < 1.0,
                "Center of world at z0 should be at extent/2, got {}",
                ly
            );

            // Northwest corner of world
            let nw = WorldCoord::new(0, 0);
            let (lx, ly) = world_to_tile_local_f64(nw, &tile, extent);
            assert!(
                lx.abs() < 1.0,
                "NW corner at z0 should be near x=0, got {}",
                lx
            );
            assert!(
                ly.abs() < 1.0,
                "NW corner at z0 should be near y=0, got {}",
                ly
            );
        }

        #[test]
        fn test_tile_local_f64_to_world_zoom_0() {
            let tile = TileCoord::new(0, 0, 0);
            let extent = 4096u32;

            // Center of zoom 0 tile
            let wc = tile_local_f64_to_world(2048.0, 2048.0, &tile, extent);
            // Should be near WORLD_HALF for both x and y
            let half = WORLD_HALF as i64;
            assert!(
                (wc.x as i64 - half).unsigned_abs() < 1_000_000,
                "x should be near WORLD_HALF, got {}",
                wc.x
            );
            assert!(
                (wc.y as i64 - half).unsigned_abs() < 1_000_000,
                "y should be near WORLD_HALF, got {}",
                wc.y
            );
        }

        // ---- Simplification tests ----

        #[test]
        fn test_simplify_world_linestring_reduces_vertices() {
            // Create a line with 100 points that has small oscillations.
            // The points trace a roughly straight path from left to right across
            // the tile, with tiny vertical deviations (< 1 pixel) that RDP should
            // collapse at 1px tolerance.
            let tile = TileCoord::new(16, 16, 5);
            let extent = 4096u32;
            let shift = 32 - 5u32;
            let tile_origin_x = (16u64) << shift;
            let tile_origin_y = (16u64) << shift;
            let tile_size = 1u64 << shift;

            // World units per pixel at this zoom+extent
            let world_per_pixel = tile_size / extent as u64;

            // Create 100 points with sub-pixel oscillation (0.3 pixels)
            let coords: Vec<WorldCoord> = (0..100)
                .map(|i| {
                    let x = tile_origin_x + (i as u64 * tile_size / 99);
                    let y_base = tile_origin_y + tile_size / 2;
                    // Oscillation of 0.3 pixels - below the 1px tolerance
                    let osc = ((i as f64 * 0.1).sin() * 0.3 * world_per_pixel as f64) as i64;
                    WorldCoord::new(x as u32, (y_base as i64 + osc) as u32)
                })
                .collect();

            let simplified = simplify_world_linestring(&coords, &tile, extent, 1.0);

            assert!(
                simplified.len() < coords.len(),
                "Expected fewer vertices after simplification: got {}, original {}",
                simplified.len(),
                coords.len()
            );
            assert!(
                simplified.len() >= 2,
                "Simplified line should have at least 2 points"
            );
        }

        #[test]
        fn test_simplify_world_linestring_preserves_short() {
            // A line with fewer than 2 points should be returned unchanged
            let tile = TileCoord::new(0, 0, 0);
            let single = vec![WorldCoord::new(100, 200)];
            let result = simplify_world_linestring(&single, &tile, 4096, 1.0);
            assert_eq!(result.len(), 1);
            assert_eq!(result[0], single[0]);

            let empty: Vec<WorldCoord> = vec![];
            let result = simplify_world_linestring(&empty, &tile, 4096, 1.0);
            assert_eq!(result.len(), 0);
        }

        #[test]
        fn test_simplify_world_linestring_straight_line_collapses() {
            // A perfectly straight line should simplify to just 2 points
            let tile = TileCoord::new(16, 16, 5);
            let extent = 4096u32;
            let shift = 32 - 5u32;
            let tile_origin_x = (16u64) << shift;
            let tile_origin_y = (16u64) << shift;
            let tile_size = 1u64 << shift;

            // 20 collinear points
            let coords: Vec<WorldCoord> = (0..20)
                .map(|i| {
                    let x = tile_origin_x + (i as u64 * tile_size / 19);
                    let y = tile_origin_y + tile_size / 2;
                    WorldCoord::new(x as u32, y as u32)
                })
                .collect();

            let simplified = simplify_world_linestring(&coords, &tile, extent, 1.0);
            assert_eq!(
                simplified.len(),
                2,
                "Straight line should simplify to 2 endpoints, got {}",
                simplified.len()
            );
        }

        #[test]
        fn test_simplify_world_ring_stays_closed() {
            // Create a simple ring (triangle) in world coordinates
            let tile = TileCoord::new(16, 16, 5);
            let extent = 4096u32;
            let shift = 32 - 5u32;
            let tile_origin_x = (16u64) << shift;
            let tile_origin_y = (16u64) << shift;
            let tile_size = 1u64 << shift;

            // Triangle ring with many intermediate points
            let mut coords: Vec<WorldCoord> = Vec::new();
            let cx = tile_origin_x + tile_size / 2;
            let cy = tile_origin_y + tile_size / 2;
            let radius = tile_size / 4;

            // Create a circle approximation with 36 points
            for i in 0..=36 {
                let angle = (i as f64) * 2.0 * std::f64::consts::PI / 36.0;
                let x = cx as f64 + radius as f64 * angle.cos();
                let y = cy as f64 + radius as f64 * angle.sin();
                coords.push(WorldCoord::new(x as u32, y as u32));
            }

            let simplified = simplify_world_ring(&coords, &tile, extent, 1.0);

            // Ring should still be closed
            assert!(
                simplified.len() >= 4,
                "Simplified ring should have at least 4 points (3 + closing), got {}",
                simplified.len()
            );
            assert_eq!(
                simplified.first(),
                simplified.last(),
                "Simplified ring should be closed (first == last)"
            );
        }

        #[test]
        fn test_world_simplified_vertex_count() {
            // Verify vertex count matches actual simplification result
            let tile = TileCoord::new(16, 16, 5);
            let extent = 4096u32;
            let shift = 32 - 5u32;
            let tile_origin_x = (16u64) << shift;
            let tile_origin_y = (16u64) << shift;
            let tile_size = 1u64 << shift;

            // Create zigzag
            let coords: Vec<WorldCoord> = (0..30)
                .map(|i| {
                    let x = tile_origin_x + (i as u64 * tile_size / 29);
                    let y_base = tile_origin_y + tile_size / 2;
                    let oscillation = if i % 2 == 0 { tile_size / 20 } else { 0 };
                    WorldCoord::new(x as u32, (y_base + oscillation) as u32)
                })
                .collect();

            let count = world_simplified_vertex_count(&coords, &tile, extent, 1.0);
            let simplified = simplify_world_linestring(&coords, &tile, extent, 1.0);

            assert_eq!(
                count,
                simplified.len(),
                "Vertex count should match actual simplification result"
            );
        }

        // ---- Consistency with f64 path tests ----

        #[test]
        fn test_world_path_matches_f64_path_for_equator() {
            // For points near the equator, the WorldCoord path and the
            // TileBounds-based f64 path should produce similar results.
            //
            // At the equator, the linear degree-to-pixel mapping and the
            // Mercator-projected WorldCoord mapping are closest, so results
            // should be nearly identical.
            let tile = TileCoord::new(16, 16, 5); // Near equator
            let bounds = tile.bounds();
            let extent = 4096u32;

            // Create a zigzag in geographic coordinates within the tile
            let num_points = 30;
            let width = bounds.lng_max - bounds.lng_min;
            let height = bounds.lat_max - bounds.lat_min;
            let center_lat = (bounds.lat_min + bounds.lat_max) / 2.0;
            let start_lng = bounds.lng_min + width * 0.1;

            let geo_coords: Vec<Coord<f64>> = (0..num_points)
                .map(|i| {
                    let x = start_lng + (i as f64 / (num_points - 1) as f64) * width * 0.8;
                    let y = center_lat + (if i % 2 == 0 { 1.0 } else { -1.0 }) * height * 0.05;
                    Coord { x, y }
                })
                .collect();

            // Convert to WorldCoords
            let world_coords: Vec<WorldCoord> = geo_coords
                .iter()
                .map(|c| lng_lat_to_world(c.x, c.y))
                .collect();

            // Simplify via f64 path (existing TileBounds-based)
            let line = LineString::new(geo_coords);
            let geom = Geometry::LineString(line);
            let f64_simplified = simplify_to_tile_coords(&geom, &bounds, extent, 1.0);
            let f64_count = if let Geometry::LineString(ls) = &f64_simplified {
                ls.coords().count()
            } else {
                panic!("Expected LineString");
            };

            // Simplify via WorldCoord path
            let world_simplified = simplify_world_linestring(&world_coords, &tile, extent, 1.0);

            // At the equator, vertex counts should be very close
            // (they may not be exactly equal due to slightly different projections
            // in the TileBounds path vs WorldCoord Mercator path)
            let diff = (f64_count as i32 - world_simplified.len() as i32).unsigned_abs();
            assert!(
                diff <= 3,
                "WorldCoord and f64 paths should produce similar results near equator.\n\
                 f64 path: {} vertices, WorldCoord path: {} vertices, diff: {}",
                f64_count,
                world_simplified.len(),
                diff
            );
        }

        #[test]
        fn test_world_simplification_is_latitude_independent() {
            // The key advantage of WorldCoord-based simplification:
            // identical tile-relative patterns at different latitudes should
            // simplify to the same vertex count because Web Mercator projection
            // is already baked into WorldCoord.
            let extent = 4096u32;
            let zoom = 5u8;

            // Get tiles at equator and at high latitude
            let tile_equator = TileCoord::new(16, 16, zoom); // Near equator
            let tile_arctic = TileCoord::new(16, 8, zoom); // Near 60N

            // Create identical patterns in tile-local space, then convert to WorldCoord
            fn make_zigzag_world(
                tile: &TileCoord,
                _extent: u32,
                num_points: usize,
            ) -> Vec<WorldCoord> {
                let shift = 32 - tile.z as u32;
                let tile_origin_x = (tile.x as u64) << shift;
                let tile_origin_y = (tile.y as u64) << shift;
                let tile_size = 1u64 << shift;

                (0..num_points)
                    .map(|i| {
                        // Span 50% of tile width
                        let x = tile_origin_x
                            + tile_size / 4
                            + (i as u64 * tile_size / 2 / (num_points as u64 - 1));
                        // Center + 5% oscillation
                        let y_base = tile_origin_y + tile_size / 2;
                        let oscillation = if i % 2 == 0 {
                            (tile_size / 20) as i64
                        } else {
                            -((tile_size / 20) as i64)
                        };
                        WorldCoord::new(x as u32, (y_base as i64 + oscillation) as u32)
                    })
                    .collect()
            }

            let coords_equator = make_zigzag_world(&tile_equator, extent, 50);
            let coords_arctic = make_zigzag_world(&tile_arctic, extent, 50);

            let simplified_equator =
                simplify_world_linestring(&coords_equator, &tile_equator, extent, 1.0);
            let simplified_arctic =
                simplify_world_linestring(&coords_arctic, &tile_arctic, extent, 1.0);

            // Both should produce the same vertex count because the patterns
            // are identical in tile-local pixel space
            assert_eq!(
                simplified_equator.len(),
                simplified_arctic.len(),
                "WorldCoord simplification should be latitude-independent.\n\
                 Equator: {} vertices, Arctic: {} vertices",
                simplified_equator.len(),
                simplified_arctic.len()
            );
        }

        #[test]
        fn test_world_tolerance_scales_with_zoom() {
            // At higher zoom levels, tolerance is smaller in world coordinate space,
            // which means more vertices should be preserved.
            let extent = 4096u32;

            // Create a line that spans a tile at zoom 5 with known oscillation
            fn make_line_at_zoom(zoom: u8) -> (Vec<WorldCoord>, TileCoord) {
                let tile = TileCoord::new(16, 16, zoom);
                let shift = 32 - zoom as u32;
                let tile_origin_x = (16u64) << shift;
                let tile_origin_y = (16u64) << shift;
                let tile_size = 1u64 << shift;

                let coords: Vec<WorldCoord> = (0..50)
                    .map(|i| {
                        let x = tile_origin_x + (i as u64 * tile_size / 49);
                        let y_base = tile_origin_y + tile_size / 2;
                        // Oscillation of 2% of tile height (about 82 pixels)
                        let oscillation = if i % 2 == 0 { tile_size / 50 } else { 0 };
                        WorldCoord::new(x as u32, (y_base + oscillation) as u32)
                    })
                    .collect();
                (coords, tile)
            }

            let (coords_z5, tile_z5) = make_line_at_zoom(5);
            let (coords_z10, tile_z10) = make_line_at_zoom(10);

            let count_z5 = world_simplified_vertex_count(&coords_z5, &tile_z5, extent, 1.0);
            let count_z10 = world_simplified_vertex_count(&coords_z10, &tile_z10, extent, 1.0);

            // Both should produce the same count because the oscillation
            // is the same fraction of the tile (2%), so in pixel space
            // the pattern is identical (about 82 pixels oscillation >> 1 pixel tolerance)
            assert_eq!(
                count_z5, count_z10,
                "Same tile-relative pattern should simplify identically at any zoom.\n\
                 z5: {} vertices, z10: {} vertices",
                count_z5, count_z10
            );
        }

        #[test]
        fn test_world_to_tile_local_f64_consistency_with_integer_method() {
            // The f64 tile-local conversion should be consistent with
            // WorldCoord::to_tile_local() (the integer version)
            let tile = TileCoord::new(1234, 5678, 14);
            let extent = 4096u32;

            // Test several points within the tile
            let shift = 32 - 14u32;
            let tile_origin_x = (1234u64) << shift;
            let tile_origin_y = (5678u64) << shift;
            let tile_size = 1u64 << shift;

            for frac in &[0.0, 0.25, 0.5, 0.75, 1.0] {
                let x = (tile_origin_x as f64 + frac * tile_size as f64) as u32;
                let y = (tile_origin_y as f64 + frac * tile_size as f64) as u32;
                let wc = WorldCoord::new(x, y);

                let (f64_x, f64_y) = world_to_tile_local_f64(wc, &tile, extent);
                let (i32_x, i32_y) = wc.to_tile_local(&tile, extent);

                // The f64 version and i32 version should agree to within 1 pixel
                assert!(
                    (f64_x - i32_x as f64).abs() < 1.5,
                    "f64 and i32 x disagree at frac={}: f64={}, i32={}",
                    frac,
                    f64_x,
                    i32_x
                );
                assert!(
                    (f64_y - i32_y as f64).abs() < 1.5,
                    "f64 and i32 y disagree at frac={}: f64={}, i32={}",
                    frac,
                    f64_y,
                    i32_y
                );
            }
        }
    }
}
