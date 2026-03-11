//! Sutherland-Hodgman polygon clipping for axis-aligned rectangles.
//!
//! This module implements the Sutherland-Hodgman algorithm for clipping polygons
//! against axis-aligned rectangular bounds. Unlike i_overlay's Vatti algorithm which
//! handles general polygon boolean operations, Sutherland-Hodgman is specialized
//! for rectangle clipping and runs in O(n) time per edge (O(4n) = O(n) total).
//!
//! # Tippecanoe Alignment
//!
//! This matches tippecanoe's approach in `clip.cpp` which uses Sutherland-Hodgman
//! for tile boundary clipping. The algorithm clips against each of the four
//! rectangle edges sequentially: left, right, bottom, top.
//!
//! # DIVERGENCE FROM TIPPECANOE: coordinate space
//! Tippecanoe operates in integer tile coordinates (0-4096).
//! We operate in f64 geographic coordinates to avoid coordinate conversion overhead.
//! The algorithm is identical; only the coordinate space differs.
//!
//! # Performance
//!
//! - O(n) per polygon ring (vs O(n log n) for Vatti)
//! - No heap allocations beyond output vectors
//! - Handles degenerate cases (empty results, collinear points)

use geo::{Coord, Geometry, LineString, MultiPolygon, Polygon};

use crate::tile::TileBounds;

/// The four edges of an axis-aligned rectangle.
#[derive(Debug, Clone, Copy)]
enum Edge {
    Left,
    Right,
    Bottom,
    Top,
}

/// Check if a point is inside (or on) the given edge of the clip rectangle.
///
/// "Inside" means the side of the edge that contains the interior of the rectangle.
#[inline]
fn is_inside(coord: &Coord<f64>, edge: Edge, bounds: &TileBounds) -> bool {
    match edge {
        Edge::Left => coord.x >= bounds.lng_min,
        Edge::Right => coord.x <= bounds.lng_max,
        Edge::Bottom => coord.y >= bounds.lat_min,
        Edge::Top => coord.y <= bounds.lat_max,
    }
}

/// Compute the intersection of line segment (a -> b) with the given edge.
///
/// Assumes the segment actually crosses the edge (one point inside, one outside).
/// Uses parametric line intersection for numerical stability.
#[inline]
fn intersect(a: &Coord<f64>, b: &Coord<f64>, edge: Edge, bounds: &TileBounds) -> Coord<f64> {
    match edge {
        Edge::Left => {
            let t = (bounds.lng_min - a.x) / (b.x - a.x);
            Coord {
                x: bounds.lng_min,
                y: a.y + t * (b.y - a.y),
            }
        }
        Edge::Right => {
            let t = (bounds.lng_max - a.x) / (b.x - a.x);
            Coord {
                x: bounds.lng_max,
                y: a.y + t * (b.y - a.y),
            }
        }
        Edge::Bottom => {
            let t = (bounds.lat_min - a.y) / (b.y - a.y);
            Coord {
                x: a.x + t * (b.x - a.x),
                y: bounds.lat_min,
            }
        }
        Edge::Top => {
            let t = (bounds.lat_max - a.y) / (b.y - a.y);
            Coord {
                x: a.x + t * (b.x - a.x),
                y: bounds.lat_max,
            }
        }
    }
}

/// Clip a ring of coordinates against a single edge of the clip rectangle.
///
/// This is one pass of the Sutherland-Hodgman algorithm. The full algorithm
/// applies this four times (once per edge).
///
/// The ring is treated as closed: the last vertex connects back to the first.
fn clip_ring_against_edge(ring: &[Coord<f64>], edge: Edge, bounds: &TileBounds) -> Vec<Coord<f64>> {
    if ring.is_empty() {
        return Vec::new();
    }

    // Pre-allocate with a reasonable estimate
    let mut output = Vec::with_capacity(ring.len());

    // Process each edge of the polygon ring.
    // For a closed ring [p0, p1, ..., pn, p0], we iterate edges:
    //   (pn, p0), (p0, p1), ..., (pn-1, pn)
    // We skip the closing vertex if ring is explicitly closed (first == last).
    let n = if ring.len() >= 2 && ring[0] == ring[ring.len() - 1] {
        ring.len() - 1
    } else {
        ring.len()
    };

    if n == 0 {
        return Vec::new();
    }

    let mut s = &ring[n - 1]; // Start with the last vertex

    for e in ring.iter().take(n) {
        let e_inside = is_inside(e, edge, bounds);
        let s_inside = is_inside(s, edge, bounds);

        match (s_inside, e_inside) {
            (true, true) => {
                // Both inside: output E
                output.push(*e);
            }
            (true, false) => {
                // S inside, E outside: output intersection
                output.push(intersect(s, e, edge, bounds));
            }
            (false, true) => {
                // S outside, E inside: output intersection, then E
                output.push(intersect(s, e, edge, bounds));
                output.push(*e);
            }
            (false, false) => {
                // Both outside: output nothing
            }
        }

        s = e;
    }

    output
}

/// Clip a single ring against all four edges of the rectangle.
///
/// Returns the clipped ring, or an empty vec if the ring is entirely outside.
fn clip_ring(ring: &[Coord<f64>], bounds: &TileBounds) -> Vec<Coord<f64>> {
    // Apply Sutherland-Hodgman sequentially against each edge
    let edges = [Edge::Left, Edge::Right, Edge::Bottom, Edge::Top];

    let mut current = ring.to_vec();

    for &edge in &edges {
        if current.is_empty() {
            return Vec::new();
        }
        current = clip_ring_against_edge(&current, edge, bounds);
    }

    // Close the ring if it has enough points
    if current.len() >= 3 {
        if current[0] != current[current.len() - 1] {
            current.push(current[0]);
        }
        current
    } else {
        Vec::new()
    }
}

/// Clip a polygon (with optional holes) to the given bounds using Sutherland-Hodgman.
///
/// Handles:
/// - Simple polygons (exterior ring only)
/// - Polygons with holes (interior rings)
///
/// Returns `None` if the polygon doesn't intersect the bounds.
/// Returns `Geometry::Polygon` for a single polygon result.
///
/// Note: Unlike Vatti/i_overlay, Sutherland-Hodgman does NOT split a polygon into
/// multiple disconnected parts. A U-shape clipped across its opening will produce
/// a single (possibly self-intersecting) polygon, not two separate polygons.
/// For tile rendering purposes, this is acceptable and matches tippecanoe's behavior.
pub fn clip_polygon_sh(poly: &Polygon<f64>, bounds: &TileBounds) -> Option<Geometry<f64>> {
    // Clip exterior ring
    let clipped_exterior = clip_ring(poly.exterior().0.as_slice(), bounds);

    if clipped_exterior.is_empty() {
        return None;
    }

    // Clip interior rings (holes)
    let clipped_interiors: Vec<LineString<f64>> = poly
        .interiors()
        .iter()
        .filter_map(|interior| {
            let clipped = clip_ring(interior.0.as_slice(), bounds);
            if clipped.len() >= 4 {
                // Need at least 3 coords + closing point
                Some(LineString::new(clipped))
            } else {
                None
            }
        })
        .collect();

    Some(Geometry::Polygon(Polygon::new(
        LineString::new(clipped_exterior),
        clipped_interiors,
    )))
}

/// Clip a MultiPolygon to bounds using Sutherland-Hodgman.
///
/// Clips each polygon individually and collects the results.
pub fn clip_multipolygon_sh(
    mp: &MultiPolygon<f64>,
    bounds: &TileBounds,
) -> Option<MultiPolygon<f64>> {
    let mut clipped_polys = Vec::new();

    for poly in &mp.0 {
        if let Some(Geometry::Polygon(clipped)) = clip_polygon_sh(poly, bounds) {
            clipped_polys.push(clipped);
        }
    }

    if clipped_polys.is_empty() {
        None
    } else {
        Some(MultiPolygon::new(clipped_polys))
    }
}

// ============================================================================
// WorldCoord-based Sutherland-Hodgman (integer coordinates)
// ============================================================================
//
// These functions implement the same Sutherland-Hodgman algorithm using
// WorldCoord integer coordinates instead of f64 geographic coordinates.
// This matches tippecanoe's approach more closely and eliminates
// floating-point precision issues in clipping.
//
// PHASE 1: These are additive -- the f64 versions above remain unchanged.
// Phase 2 will migrate the pipeline to call these instead.

use crate::world_coord::{WorldBounds, WorldCoord};

/// A coordinate in i64 space for intermediate clipping calculations.
///
/// We use i64 (not u32) because:
/// 1. Intersection calculations can produce intermediate values outside [0, 2^32)
/// 2. Buffer regions extend beyond tile boundaries (negative offsets)
/// 3. The Sutherland-Hodgman intersection formula requires signed arithmetic
///
/// After clipping, results are converted back to WorldCoord (u32).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipCoord {
    pub x: i64,
    pub y: i64,
}

impl ClipCoord {
    #[inline]
    pub const fn new(x: i64, y: i64) -> Self {
        Self { x, y }
    }

    /// Convert from WorldCoord (u32) to ClipCoord (i64).
    #[inline]
    pub fn from_world(coord: WorldCoord) -> Self {
        Self {
            x: coord.x as i64,
            y: coord.y as i64,
        }
    }

    /// Convert to WorldCoord (u32), clamping to valid range.
    #[inline]
    pub fn to_world(&self) -> WorldCoord {
        WorldCoord::new(
            self.x.clamp(0, u32::MAX as i64) as u32,
            self.y.clamp(0, u32::MAX as i64) as u32,
        )
    }
}

/// Bounds in i64 space for clipping (allows buffer regions outside [0, 2^32)).
#[derive(Debug, Clone, Copy)]
struct ClipBounds {
    x_min: i64,
    y_min: i64,
    x_max: i64,
    y_max: i64,
}

impl ClipBounds {
    fn from_world_bounds(bounds: &WorldBounds) -> Self {
        Self {
            x_min: bounds.x_min as i64,
            y_min: bounds.y_min as i64,
            x_max: bounds.x_max as i64,
            y_max: bounds.y_max as i64,
        }
    }
}

/// Check if a point is inside the given edge in integer space.
///
/// In world coordinate space:
/// - Left edge: x >= x_min
/// - Right edge: x <= x_max
/// - Top edge: y >= y_min (y increases southward, so "top" = min y)
/// - Bottom edge: y <= y_max
#[inline]
fn is_inside_world(coord: &ClipCoord, edge: Edge, bounds: &ClipBounds) -> bool {
    match edge {
        Edge::Left => coord.x >= bounds.x_min,
        Edge::Right => coord.x <= bounds.x_max,
        // In world coords, Y increases southward:
        // "Top" = northern edge = y_min, "Bottom" = southern edge = y_max
        Edge::Top => coord.y >= bounds.y_min,
        Edge::Bottom => coord.y <= bounds.y_max,
    }
}

/// Compute intersection of segment (a -> b) with an edge in integer space.
///
/// Uses integer arithmetic with rounding to maintain precision.
/// The parametric form: P = A + t * (B - A) where t = (edge - A.axis) / (B.axis - A.axis)
///
/// For integer coordinates, we compute the cross-axis value as:
///   cross = A.cross + (edge - A.axis) * (B.cross - A.cross) / (B.axis - A.axis)
///
/// This uses i64 arithmetic throughout to avoid overflow for u32 coordinate differences.
#[inline]
fn intersect_world(a: &ClipCoord, b: &ClipCoord, edge: Edge, bounds: &ClipBounds) -> ClipCoord {
    match edge {
        Edge::Left => {
            let dx = b.x - a.x;
            if dx == 0 {
                return ClipCoord::new(bounds.x_min, a.y);
            }
            let t_num = bounds.x_min - a.x;
            let dy = b.y - a.y;
            // Use i128 for the multiplication to avoid overflow
            let y = a.y + ((t_num as i128 * dy as i128) / dx as i128) as i64;
            ClipCoord::new(bounds.x_min, y)
        }
        Edge::Right => {
            let dx = b.x - a.x;
            if dx == 0 {
                return ClipCoord::new(bounds.x_max, a.y);
            }
            let t_num = bounds.x_max - a.x;
            let dy = b.y - a.y;
            let y = a.y + ((t_num as i128 * dy as i128) / dx as i128) as i64;
            ClipCoord::new(bounds.x_max, y)
        }
        Edge::Top => {
            let dy = b.y - a.y;
            if dy == 0 {
                return ClipCoord::new(a.x, bounds.y_min);
            }
            let t_num = bounds.y_min - a.y;
            let dx = b.x - a.x;
            let x = a.x + ((t_num as i128 * dx as i128) / dy as i128) as i64;
            ClipCoord::new(x, bounds.y_min)
        }
        Edge::Bottom => {
            let dy = b.y - a.y;
            if dy == 0 {
                return ClipCoord::new(a.x, bounds.y_max);
            }
            let t_num = bounds.y_max - a.y;
            let dx = b.x - a.x;
            let x = a.x + ((t_num as i128 * dx as i128) / dy as i128) as i64;
            ClipCoord::new(x, bounds.y_max)
        }
    }
}

/// Clip a ring of WorldCoord-based coordinates against a single edge.
///
/// Same algorithm as `clip_ring_against_edge` but operates in integer space.
fn clip_ring_against_edge_world(
    ring: &[ClipCoord],
    edge: Edge,
    bounds: &ClipBounds,
) -> Vec<ClipCoord> {
    if ring.is_empty() {
        return Vec::new();
    }

    let mut output = Vec::with_capacity(ring.len());

    // Handle closed ring (first == last)
    let n = if ring.len() >= 2 && ring[0] == ring[ring.len() - 1] {
        ring.len() - 1
    } else {
        ring.len()
    };

    if n == 0 {
        return Vec::new();
    }

    let mut s = &ring[n - 1];

    for e in ring.iter().take(n) {
        let e_inside = is_inside_world(e, edge, bounds);
        let s_inside = is_inside_world(s, edge, bounds);

        match (s_inside, e_inside) {
            (true, true) => {
                output.push(*e);
            }
            (true, false) => {
                output.push(intersect_world(s, e, edge, bounds));
            }
            (false, true) => {
                output.push(intersect_world(s, e, edge, bounds));
                output.push(*e);
            }
            (false, false) => {}
        }

        s = e;
    }

    output
}

/// Clip a ring of WorldCoord points against all four edges of a WorldBounds rectangle.
///
/// Returns the clipped ring as ClipCoord points, or empty if fully outside.
fn clip_ring_world(coords: &[ClipCoord], bounds: &WorldBounds) -> Vec<ClipCoord> {
    let clip_bounds = ClipBounds::from_world_bounds(bounds);

    // In world coordinate space:
    // - Left/Right clip on X axis
    // - Top/Bottom clip on Y axis (top = y_min, bottom = y_max)
    let edges = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    let mut current = coords.to_vec();

    for &edge in &edges {
        if current.is_empty() {
            return Vec::new();
        }
        current = clip_ring_against_edge_world(&current, edge, &clip_bounds);
    }

    // Close the ring if it has enough points
    if current.len() >= 3 {
        if current[0] != current[current.len() - 1] {
            current.push(current[0]);
        }
        current
    } else {
        Vec::new()
    }
}

/// Clip a polygon using Sutherland-Hodgman in WorldCoord integer space.
///
/// This is the integer-coordinate equivalent of `clip_polygon_sh`.
/// It operates directly in world coordinate space, eliminating
/// floating-point precision issues.
///
/// # Arguments
/// * `exterior` - Exterior ring as WorldCoord points
/// * `interiors` - Interior rings (holes) as WorldCoord points
/// * `bounds` - Clipping bounds in world coordinate space
///
/// # Returns
/// * `Some((clipped_exterior, clipped_interiors))` - Clipped rings as WorldCoord vectors
/// * `None` - If the polygon doesn't intersect the bounds
pub fn clip_polygon_sh_world(
    exterior: &[WorldCoord],
    interiors: &[Vec<WorldCoord>],
    bounds: &WorldBounds,
) -> Option<(Vec<WorldCoord>, Vec<Vec<WorldCoord>>)> {
    // Convert exterior to ClipCoord for signed arithmetic
    let ext_clip: Vec<ClipCoord> = exterior.iter().map(|c| ClipCoord::from_world(*c)).collect();

    let clipped_ext = clip_ring_world(&ext_clip, bounds);
    if clipped_ext.is_empty() {
        return None;
    }

    // Convert back to WorldCoord
    let result_exterior: Vec<WorldCoord> = clipped_ext.iter().map(|c| c.to_world()).collect();

    // Clip interior rings
    let result_interiors: Vec<Vec<WorldCoord>> = interiors
        .iter()
        .filter_map(|interior| {
            let int_clip: Vec<ClipCoord> =
                interior.iter().map(|c| ClipCoord::from_world(*c)).collect();
            let clipped = clip_ring_world(&int_clip, bounds);
            if clipped.len() >= 4 {
                Some(clipped.iter().map(|c| c.to_world()).collect())
            } else {
                None
            }
        })
        .collect();

    Some((result_exterior, result_interiors))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use geo::polygon;

    fn test_bounds() -> TileBounds {
        TileBounds::new(0.0, 0.0, 10.0, 10.0)
    }

    fn unit_bounds() -> TileBounds {
        TileBounds::new(0.0, 0.0, 1.0, 1.0)
    }

    // ========================================================================
    // Edge helper tests
    // ========================================================================

    #[test]
    fn test_is_inside_left() {
        let bounds = test_bounds();
        assert!(is_inside(&Coord { x: 5.0, y: 5.0 }, Edge::Left, &bounds));
        assert!(is_inside(&Coord { x: 0.0, y: 5.0 }, Edge::Left, &bounds)); // on edge
        assert!(!is_inside(&Coord { x: -1.0, y: 5.0 }, Edge::Left, &bounds));
    }

    #[test]
    fn test_is_inside_right() {
        let bounds = test_bounds();
        assert!(is_inside(&Coord { x: 5.0, y: 5.0 }, Edge::Right, &bounds));
        assert!(is_inside(&Coord { x: 10.0, y: 5.0 }, Edge::Right, &bounds)); // on edge
        assert!(!is_inside(&Coord { x: 11.0, y: 5.0 }, Edge::Right, &bounds));
    }

    #[test]
    fn test_is_inside_bottom() {
        let bounds = test_bounds();
        assert!(is_inside(&Coord { x: 5.0, y: 5.0 }, Edge::Bottom, &bounds));
        assert!(is_inside(&Coord { x: 5.0, y: 0.0 }, Edge::Bottom, &bounds)); // on edge
        assert!(!is_inside(
            &Coord { x: 5.0, y: -1.0 },
            Edge::Bottom,
            &bounds
        ));
    }

    #[test]
    fn test_is_inside_top() {
        let bounds = test_bounds();
        assert!(is_inside(&Coord { x: 5.0, y: 5.0 }, Edge::Top, &bounds));
        assert!(is_inside(&Coord { x: 5.0, y: 10.0 }, Edge::Top, &bounds)); // on edge
        assert!(!is_inside(&Coord { x: 5.0, y: 11.0 }, Edge::Top, &bounds));
    }

    #[test]
    fn test_intersect_left() {
        let bounds = test_bounds();
        let a = Coord { x: -5.0, y: 5.0 };
        let b = Coord { x: 5.0, y: 5.0 };
        let result = intersect(&a, &b, Edge::Left, &bounds);
        assert!((result.x - 0.0).abs() < 1e-10);
        assert!((result.y - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_intersect_right() {
        let bounds = test_bounds();
        let a = Coord { x: 5.0, y: 5.0 };
        let b = Coord { x: 15.0, y: 5.0 };
        let result = intersect(&a, &b, Edge::Right, &bounds);
        assert!((result.x - 10.0).abs() < 1e-10);
        assert!((result.y - 5.0).abs() < 1e-10);
    }

    #[test]
    fn test_intersect_bottom() {
        let bounds = test_bounds();
        let a = Coord { x: 5.0, y: -5.0 };
        let b = Coord { x: 5.0, y: 5.0 };
        let result = intersect(&a, &b, Edge::Bottom, &bounds);
        assert!((result.x - 5.0).abs() < 1e-10);
        assert!((result.y - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_intersect_top() {
        let bounds = test_bounds();
        let a = Coord { x: 5.0, y: 5.0 };
        let b = Coord { x: 5.0, y: 15.0 };
        let result = intersect(&a, &b, Edge::Top, &bounds);
        assert!((result.x - 5.0).abs() < 1e-10);
        assert!((result.y - 10.0).abs() < 1e-10);
    }

    // ========================================================================
    // Ring clipping tests
    // ========================================================================

    #[test]
    fn test_clip_ring_fully_inside() {
        let bounds = test_bounds();
        let ring = vec![
            Coord { x: 2.0, y: 2.0 },
            Coord { x: 8.0, y: 2.0 },
            Coord { x: 8.0, y: 8.0 },
            Coord { x: 2.0, y: 8.0 },
            Coord { x: 2.0, y: 2.0 },
        ];

        let result = clip_ring(&ring, &bounds);
        assert!(!result.is_empty());
        // Should have same number of vertices (all inside)
        assert_eq!(result.len(), 5); // 4 corners + closing
    }

    #[test]
    fn test_clip_ring_fully_outside() {
        let bounds = test_bounds();
        let ring = vec![
            Coord { x: 20.0, y: 20.0 },
            Coord { x: 30.0, y: 20.0 },
            Coord { x: 30.0, y: 30.0 },
            Coord { x: 20.0, y: 30.0 },
            Coord { x: 20.0, y: 20.0 },
        ];

        let result = clip_ring(&ring, &bounds);
        assert!(result.is_empty());
    }

    #[test]
    fn test_clip_ring_partial_overlap() {
        let bounds = test_bounds();
        // Square from (-5,-5) to (5,5), overlapping with bounds (0,0)-(10,10)
        let ring = vec![
            Coord { x: -5.0, y: -5.0 },
            Coord { x: 5.0, y: -5.0 },
            Coord { x: 5.0, y: 5.0 },
            Coord { x: -5.0, y: 5.0 },
            Coord { x: -5.0, y: -5.0 },
        ];

        let result = clip_ring(&ring, &bounds);
        assert!(!result.is_empty());

        // All output vertices should be within bounds
        for coord in &result {
            assert!(
                coord.x >= -1e-10 && coord.x <= 10.0 + 1e-10,
                "x={} out of bounds",
                coord.x
            );
            assert!(
                coord.y >= -1e-10 && coord.y <= 10.0 + 1e-10,
                "y={} out of bounds",
                coord.y
            );
        }
    }

    #[test]
    fn test_clip_ring_crossing_one_edge() {
        let bounds = test_bounds();
        // Square from (5,-5) to (15,5) - crosses right edge
        let ring = vec![
            Coord { x: 5.0, y: -5.0 },
            Coord { x: 15.0, y: -5.0 },
            Coord { x: 15.0, y: 5.0 },
            Coord { x: 5.0, y: 5.0 },
            Coord { x: 5.0, y: -5.0 },
        ];

        let result = clip_ring(&ring, &bounds);
        assert!(!result.is_empty());

        // Verify all coords are within bounds
        for coord in &result {
            assert!(
                coord.x >= -1e-10 && coord.x <= 10.0 + 1e-10,
                "x={} out of bounds",
                coord.x
            );
            assert!(
                coord.y >= -1e-10 && coord.y <= 10.0 + 1e-10,
                "y={} out of bounds",
                coord.y
            );
        }
    }

    #[test]
    fn test_clip_ring_triangle() {
        let bounds = test_bounds();
        // Triangle that extends beyond top-right corner
        let ring = vec![
            Coord { x: 5.0, y: 5.0 },
            Coord { x: 15.0, y: 5.0 },
            Coord { x: 5.0, y: 15.0 },
            Coord { x: 5.0, y: 5.0 },
        ];

        let result = clip_ring(&ring, &bounds);
        assert!(!result.is_empty());

        // Verify all coords are within bounds
        for coord in &result {
            assert!(
                coord.x >= -1e-10 && coord.x <= 10.0 + 1e-10,
                "x={} out of bounds",
                coord.x
            );
            assert!(
                coord.y >= -1e-10 && coord.y <= 10.0 + 1e-10,
                "y={} out of bounds",
                coord.y
            );
        }
    }

    // ========================================================================
    // Polygon clipping tests
    // ========================================================================

    #[test]
    fn test_clip_polygon_fully_inside() {
        let bounds = test_bounds();
        let poly = polygon![
            (x: 2.0, y: 2.0),
            (x: 8.0, y: 2.0),
            (x: 8.0, y: 8.0),
            (x: 2.0, y: 8.0),
            (x: 2.0, y: 2.0),
        ];

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some());
        match result.unwrap() {
            Geometry::Polygon(p) => {
                assert!(p.exterior().coords().count() >= 4);
            }
            _ => panic!("Expected Polygon"),
        }
    }

    #[test]
    fn test_clip_polygon_fully_outside() {
        let bounds = test_bounds();
        let poly = polygon![
            (x: 20.0, y: 20.0),
            (x: 30.0, y: 20.0),
            (x: 30.0, y: 30.0),
            (x: 20.0, y: 30.0),
            (x: 20.0, y: 20.0),
        ];

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_none());
    }

    #[test]
    fn test_clip_polygon_partial_overlap() {
        let bounds = test_bounds();
        let poly = polygon![
            (x: -5.0, y: -5.0),
            (x: 5.0, y: -5.0),
            (x: 5.0, y: 5.0),
            (x: -5.0, y: 5.0),
            (x: -5.0, y: -5.0),
        ];

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some());

        match result.unwrap() {
            Geometry::Polygon(p) => {
                for coord in p.exterior().coords() {
                    assert!(
                        coord.x >= -1e-10 && coord.x <= 10.0 + 1e-10,
                        "x={} out of bounds",
                        coord.x
                    );
                    assert!(
                        coord.y >= -1e-10 && coord.y <= 10.0 + 1e-10,
                        "y={} out of bounds",
                        coord.y
                    );
                }
            }
            _ => panic!("Expected Polygon"),
        }
    }

    #[test]
    fn test_clip_polygon_with_hole() {
        let bounds = unit_bounds();
        // Exterior: large square covering the entire tile and beyond
        // Hole: small square fully inside the tile
        let poly = polygon![
            exterior: [
                (x: -0.5, y: -0.5),
                (x: 1.5, y: -0.5),
                (x: 1.5, y: 1.5),
                (x: -0.5, y: 1.5),
                (x: -0.5, y: -0.5),
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

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some());

        match result.unwrap() {
            Geometry::Polygon(p) => {
                // Exterior should be clipped
                assert!(p.exterior().coords().count() >= 4);
                // Hole should be preserved (it's fully inside)
                assert_eq!(p.interiors().len(), 1);
            }
            _ => panic!("Expected Polygon"),
        }
    }

    #[test]
    fn test_clip_polygon_hole_outside_bounds() {
        let bounds = unit_bounds();
        // Exterior covers the tile, hole is outside
        let poly = polygon![
            exterior: [
                (x: -0.5, y: -0.5),
                (x: 1.5, y: -0.5),
                (x: 1.5, y: 1.5),
                (x: -0.5, y: 1.5),
                (x: -0.5, y: -0.5),
            ],
            interiors: [
                [
                    (x: 2.0, y: 2.0),
                    (x: 3.0, y: 2.0),
                    (x: 3.0, y: 3.0),
                    (x: 2.0, y: 3.0),
                    (x: 2.0, y: 2.0),
                ],
            ],
        ];

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some());

        match result.unwrap() {
            Geometry::Polygon(p) => {
                // Hole should be removed (outside bounds)
                assert_eq!(p.interiors().len(), 0);
            }
            _ => panic!("Expected Polygon"),
        }
    }

    // ========================================================================
    // MultiPolygon clipping tests
    // ========================================================================

    #[test]
    fn test_clip_multipolygon() {
        let bounds = test_bounds();
        let mp = MultiPolygon::new(vec![
            polygon![
                (x: 2.0, y: 2.0),
                (x: 4.0, y: 2.0),
                (x: 4.0, y: 4.0),
                (x: 2.0, y: 4.0),
                (x: 2.0, y: 2.0),
            ],
            polygon![
                (x: 6.0, y: 6.0),
                (x: 8.0, y: 6.0),
                (x: 8.0, y: 8.0),
                (x: 6.0, y: 8.0),
                (x: 6.0, y: 6.0),
            ],
        ]);

        let result = clip_multipolygon_sh(&mp, &bounds);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.len(), 2);
    }

    #[test]
    fn test_clip_multipolygon_one_outside() {
        let bounds = test_bounds();
        let mp = MultiPolygon::new(vec![
            polygon![
                (x: 2.0, y: 2.0),
                (x: 4.0, y: 2.0),
                (x: 4.0, y: 4.0),
                (x: 2.0, y: 4.0),
                (x: 2.0, y: 2.0),
            ],
            polygon![
                (x: 20.0, y: 20.0),
                (x: 30.0, y: 20.0),
                (x: 30.0, y: 30.0),
                (x: 20.0, y: 30.0),
                (x: 20.0, y: 20.0),
            ],
        ]);

        let result = clip_multipolygon_sh(&mp, &bounds);
        assert!(result.is_some());
        assert_eq!(result.unwrap().0.len(), 1);
    }

    #[test]
    fn test_clip_multipolygon_all_outside() {
        let bounds = test_bounds();
        let mp = MultiPolygon::new(vec![polygon![
            (x: 20.0, y: 20.0),
            (x: 30.0, y: 20.0),
            (x: 30.0, y: 30.0),
            (x: 20.0, y: 30.0),
            (x: 20.0, y: 20.0),
        ]]);

        let result = clip_multipolygon_sh(&mp, &bounds);
        assert!(result.is_none());
    }

    // ========================================================================
    // Correctness tests: verify clipped area
    // ========================================================================

    #[test]
    fn test_clip_produces_correct_area() {
        use geo::Area;

        let bounds = test_bounds(); // (0,0)-(10,10)

        // Square from (5,5) to (15,15) - quarter overlap with bounds
        let poly = polygon![
            (x: 5.0, y: 5.0),
            (x: 15.0, y: 5.0),
            (x: 15.0, y: 15.0),
            (x: 5.0, y: 15.0),
            (x: 5.0, y: 5.0),
        ];

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some());

        match result.unwrap() {
            Geometry::Polygon(p) => {
                let area = p.unsigned_area();
                // Expected: 5x5 = 25.0
                assert!(
                    (area - 25.0).abs() < 0.01,
                    "Expected area ~25.0, got {}",
                    area
                );
            }
            _ => panic!("Expected Polygon"),
        }
    }

    #[test]
    fn test_clip_large_polygon_spanning_bounds() {
        use geo::Area;

        let bounds = test_bounds(); // (0,0)-(10,10)

        // Large square from (-10,-10) to (20,20) - should clip to bounds exactly
        let poly = polygon![
            (x: -10.0, y: -10.0),
            (x: 20.0, y: -10.0),
            (x: 20.0, y: 20.0),
            (x: -10.0, y: 20.0),
            (x: -10.0, y: -10.0),
        ];

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some());

        match result.unwrap() {
            Geometry::Polygon(p) => {
                let area = p.unsigned_area();
                // Expected: 10x10 = 100.0 (clipped to bounds)
                assert!(
                    (area - 100.0).abs() < 0.01,
                    "Expected area ~100.0, got {}",
                    area
                );
            }
            _ => panic!("Expected Polygon"),
        }
    }

    // ========================================================================
    // Performance regression test
    // ========================================================================

    #[test]
    fn test_clip_many_vertex_polygon() {
        // Create a polygon with many vertices (simulating a complex geometry)
        // This should complete in <10ms (SH is O(n))
        let bounds = test_bounds();
        let n = 10_000;
        let mut coords: Vec<Coord<f64>> = Vec::with_capacity(n + 1);

        // Create a circle-like polygon centered at (5,5) with radius 8
        // (extends beyond bounds in all directions)
        for i in 0..n {
            let angle = 2.0 * std::f64::consts::PI * (i as f64) / (n as f64);
            coords.push(Coord {
                x: 5.0 + 8.0 * angle.cos(),
                y: 5.0 + 8.0 * angle.sin(),
            });
        }
        coords.push(coords[0]); // Close the ring

        let poly = Polygon::new(LineString::new(coords), vec![]);

        let start = std::time::Instant::now();
        let result = clip_polygon_sh(&poly, &bounds);
        let elapsed = start.elapsed();

        assert!(result.is_some());
        assert!(
            elapsed.as_millis() < 100,
            "Clipping {} vertices took {}ms (should be <100ms)",
            n,
            elapsed.as_millis()
        );
    }

    #[test]
    fn test_clip_huge_vertex_polygon() {
        // Simulate the 316k-coordinate case from the issue
        // SH should handle this in <100ms
        let bounds = TileBounds::new(-67.50, -66.51, -56.25, -61.61);
        let n = 316_000;
        let mut coords: Vec<Coord<f64>> = Vec::with_capacity(n + 1);

        // Create a jagged polygon spanning the tile area
        for i in 0..n {
            let t = i as f64 / n as f64;
            let x = -80.0 + t * 40.0; // Spans -80 to -40 (crossing tile)
            let y = -70.0 + (i % 100) as f64 * 0.1; // Jagged between -70 and -60
            coords.push(Coord { x, y });
        }
        coords.push(coords[0]); // Close the ring

        let poly = Polygon::new(LineString::new(coords), vec![]);

        let start = std::time::Instant::now();
        let result = clip_polygon_sh(&poly, &bounds);
        let elapsed = start.elapsed();

        // The result may or may not produce output depending on geometry,
        // but it MUST complete quickly
        assert!(
            elapsed.as_secs_f64() < 0.5,
            "Clipping {} vertices took {:.3}s (should be <0.5s, i_overlay took ~10s)",
            n,
            elapsed.as_secs_f64()
        );

        // Log for visibility
        eprintln!(
            "SH clip of {}k vertices: {:.3}s (result: {})",
            n / 1000,
            elapsed.as_secs_f64(),
            if result.is_some() { "some" } else { "none" }
        );
    }

    // ========================================================================
    // Edge case tests
    // ========================================================================

    #[test]
    fn test_clip_polygon_on_boundary() {
        let bounds = test_bounds();
        // Polygon exactly on the boundary edge
        let poly = polygon![
            (x: 0.0, y: 0.0),
            (x: 10.0, y: 0.0),
            (x: 10.0, y: 10.0),
            (x: 0.0, y: 10.0),
            (x: 0.0, y: 0.0),
        ];

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some());
    }

    #[test]
    fn test_clip_polygon_touching_corner() {
        let bounds = test_bounds();
        // Triangle touching the corner of bounds
        let poly = polygon![
            (x: 10.0, y: 10.0),
            (x: 15.0, y: 10.0),
            (x: 10.0, y: 15.0),
            (x: 10.0, y: 10.0),
        ];

        // This is a degenerate case - the triangle touches bounds at a single point.
        // Result may be None (degenerate) or Some with a tiny polygon.
        // Either is acceptable.
        let _ = clip_polygon_sh(&poly, &bounds);
    }

    #[test]
    fn test_clip_empty_ring() {
        let bounds = test_bounds();
        let ring: Vec<Coord<f64>> = vec![];
        let result = clip_ring(&ring, &bounds);
        assert!(result.is_empty());
    }

    // ========================================================================
    // WorldCoord-based Sutherland-Hodgman tests
    // ========================================================================

    mod world_tests {
        use super::*;
        use crate::world_coord::{WorldBounds, WorldCoord};

        /// Helper: create a WorldBounds representing a box from (1000, 1000) to (5000, 5000)
        fn test_world_bounds() -> WorldBounds {
            WorldBounds::new(1000, 1000, 5000, 5000)
        }

        #[test]
        fn test_clip_coord_roundtrip() {
            let world = WorldCoord::new(12345, 67890);
            let clip = ClipCoord::from_world(world);
            let back = clip.to_world();
            assert_eq!(world, back);
        }

        #[test]
        fn test_clip_coord_clamping() {
            // Negative values clamp to 0
            let clip = ClipCoord::new(-100, -200);
            let world = clip.to_world();
            assert_eq!(world.x, 0);
            assert_eq!(world.y, 0);

            // Values above u32::MAX clamp
            let clip = ClipCoord::new(u32::MAX as i64 + 100, u32::MAX as i64 + 200);
            let world = clip.to_world();
            assert_eq!(world.x, u32::MAX);
            assert_eq!(world.y, u32::MAX);
        }

        #[test]
        fn test_world_sh_fully_inside() {
            let bounds = test_world_bounds();
            // Square fully inside: (2000, 2000) to (4000, 4000)
            let exterior = vec![
                WorldCoord::new(2000, 2000),
                WorldCoord::new(4000, 2000),
                WorldCoord::new(4000, 4000),
                WorldCoord::new(2000, 4000),
                WorldCoord::new(2000, 2000), // close
            ];

            let result = clip_polygon_sh_world(&exterior, &[], &bounds);
            assert!(result.is_some(), "Fully inside polygon should be preserved");

            let (ext, ints) = result.unwrap();
            assert!(ext.len() >= 4, "Should have at least 4 vertices");
            assert!(ints.is_empty(), "No holes expected");

            // All coords should be within bounds
            for coord in &ext {
                assert!(
                    coord.x >= bounds.x_min && coord.x <= bounds.x_max,
                    "x={} out of bounds [{}, {}]",
                    coord.x,
                    bounds.x_min,
                    bounds.x_max
                );
                assert!(
                    coord.y >= bounds.y_min && coord.y <= bounds.y_max,
                    "y={} out of bounds [{}, {}]",
                    coord.y,
                    bounds.y_min,
                    bounds.y_max
                );
            }
        }

        #[test]
        fn test_world_sh_fully_outside() {
            let bounds = test_world_bounds();
            // Square fully outside: (6000, 6000) to (8000, 8000)
            let exterior = vec![
                WorldCoord::new(6000, 6000),
                WorldCoord::new(8000, 6000),
                WorldCoord::new(8000, 8000),
                WorldCoord::new(6000, 8000),
                WorldCoord::new(6000, 6000),
            ];

            let result = clip_polygon_sh_world(&exterior, &[], &bounds);
            assert!(result.is_none(), "Fully outside polygon should return None");
        }

        #[test]
        fn test_world_sh_partial_clip_right_edge() {
            let bounds = test_world_bounds(); // (1000, 1000) to (5000, 5000)
                                              // Square straddling right edge: (3000, 2000) to (7000, 4000)
            let exterior = vec![
                WorldCoord::new(3000, 2000),
                WorldCoord::new(7000, 2000),
                WorldCoord::new(7000, 4000),
                WorldCoord::new(3000, 4000),
                WorldCoord::new(3000, 2000),
            ];

            let result = clip_polygon_sh_world(&exterior, &[], &bounds);
            assert!(
                result.is_some(),
                "Partially overlapping should produce output"
            );

            let (ext, _) = result.unwrap();
            // All output coords should be within bounds
            for coord in &ext {
                assert!(
                    coord.x >= bounds.x_min && coord.x <= bounds.x_max,
                    "x={} out of bounds",
                    coord.x
                );
                assert!(
                    coord.y >= bounds.y_min && coord.y <= bounds.y_max,
                    "y={} out of bounds",
                    coord.y
                );
            }
        }

        #[test]
        fn test_world_sh_partial_clip_corner() {
            let bounds = test_world_bounds(); // (1000, 1000) to (5000, 5000)
                                              // Square straddling top-left corner: (0, 0) to (3000, 3000)
            let exterior = vec![
                WorldCoord::new(0, 0),
                WorldCoord::new(3000, 0),
                WorldCoord::new(3000, 3000),
                WorldCoord::new(0, 3000),
                WorldCoord::new(0, 0),
            ];

            let result = clip_polygon_sh_world(&exterior, &[], &bounds);
            assert!(result.is_some(), "Corner-overlap should produce output");

            let (ext, _) = result.unwrap();
            for coord in &ext {
                assert!(
                    coord.x >= bounds.x_min && coord.x <= bounds.x_max,
                    "x={} out of bounds",
                    coord.x
                );
                assert!(
                    coord.y >= bounds.y_min && coord.y <= bounds.y_max,
                    "y={} out of bounds",
                    coord.y
                );
            }
        }

        #[test]
        fn test_world_sh_with_hole() {
            let bounds = WorldBounds::new(0, 0, 10000, 10000);
            // Exterior covers entire bounds and beyond
            let exterior = vec![
                WorldCoord::new(0, 0),
                WorldCoord::new(12000, 0),
                WorldCoord::new(12000, 12000),
                WorldCoord::new(0, 12000),
                WorldCoord::new(0, 0),
            ];
            // Hole fully inside bounds
            let hole = vec![
                WorldCoord::new(3000, 3000),
                WorldCoord::new(7000, 3000),
                WorldCoord::new(7000, 7000),
                WorldCoord::new(3000, 7000),
                WorldCoord::new(3000, 3000),
            ];

            let result = clip_polygon_sh_world(&exterior, &[hole], &bounds);
            assert!(result.is_some());

            let (_, ints) = result.unwrap();
            assert_eq!(ints.len(), 1, "Hole inside bounds should be preserved");
        }

        #[test]
        fn test_world_sh_hole_outside_bounds() {
            let bounds = WorldBounds::new(0, 0, 10000, 10000);
            let exterior = vec![
                WorldCoord::new(0, 0),
                WorldCoord::new(12000, 0),
                WorldCoord::new(12000, 12000),
                WorldCoord::new(0, 12000),
                WorldCoord::new(0, 0),
            ];
            // Hole completely outside bounds
            let hole = vec![
                WorldCoord::new(20000, 20000),
                WorldCoord::new(30000, 20000),
                WorldCoord::new(30000, 30000),
                WorldCoord::new(20000, 30000),
                WorldCoord::new(20000, 20000),
            ];

            let result = clip_polygon_sh_world(&exterior, &[hole], &bounds);
            assert!(result.is_some());

            let (_, ints) = result.unwrap();
            assert_eq!(ints.len(), 0, "Hole outside bounds should be removed");
        }

        #[test]
        fn test_world_sh_consistency_with_f64() {
            // Verify that WorldCoord-based SH produces equivalent results to f64-based SH
            // for a simple case that can be represented exactly in both coordinate systems.
            //
            // We use a tile at zoom 4 and a simple square polygon that partially
            // overlaps the tile.
            use crate::tile::TileCoord;
            use crate::world_coord::lng_lat_to_world;

            let tile = TileCoord::new(8, 5, 4);
            let tile_bounds_f64 = tile.bounds();
            let tile_bounds_world = WorldBounds::from_tile(&tile);

            // Create a polygon that spans from tile center to beyond the right edge
            let tile_center_lng = (tile_bounds_f64.lng_min + tile_bounds_f64.lng_max) / 2.0;
            let tile_center_lat = (tile_bounds_f64.lat_min + tile_bounds_f64.lat_max) / 2.0;

            // Simple square from tile center to beyond right/bottom edge
            let poly_f64 = Polygon::new(
                LineString::from(vec![
                    Coord {
                        x: tile_center_lng,
                        y: tile_center_lat,
                    },
                    Coord {
                        x: tile_bounds_f64.lng_max + 5.0,
                        y: tile_center_lat,
                    },
                    Coord {
                        x: tile_bounds_f64.lng_max + 5.0,
                        y: tile_bounds_f64.lat_min - 5.0,
                    },
                    Coord {
                        x: tile_center_lng,
                        y: tile_bounds_f64.lat_min - 5.0,
                    },
                    Coord {
                        x: tile_center_lng,
                        y: tile_center_lat,
                    },
                ]),
                vec![],
            );

            // f64 clip
            let f64_result = clip_polygon_sh(&poly_f64, &tile_bounds_f64);
            assert!(f64_result.is_some(), "f64 clip should produce output");

            // WorldCoord clip
            let world_exterior: Vec<WorldCoord> = poly_f64
                .exterior()
                .coords()
                .map(|c| lng_lat_to_world(c.x, c.y))
                .collect();

            let world_result = clip_polygon_sh_world(&world_exterior, &[], &tile_bounds_world);
            assert!(
                world_result.is_some(),
                "WorldCoord clip should produce output"
            );

            // Both should produce output with similar vertex count
            let f64_count = match f64_result.unwrap() {
                Geometry::Polygon(p) => p.exterior().coords().count(),
                _ => panic!("Expected Polygon from f64 clip"),
            };
            let (world_ext, _) = world_result.unwrap();
            let world_count = world_ext.len();

            // Vertex counts should be similar (exact match not required due to
            // coordinate system differences)
            assert!(
                (f64_count as i32 - world_count as i32).unsigned_abs() <= 2,
                "Vertex count mismatch: f64={}, world={}",
                f64_count,
                world_count
            );
        }

        #[test]
        fn test_world_sh_large_coordinate_values() {
            // Test with coordinates near u32::MAX to verify no overflow
            let bounds = WorldBounds::new(
                u32::MAX - 10000,
                u32::MAX - 10000,
                u32::MAX - 1000,
                u32::MAX - 1000,
            );

            let exterior = vec![
                WorldCoord::new(u32::MAX - 8000, u32::MAX - 8000),
                WorldCoord::new(u32::MAX - 2000, u32::MAX - 8000),
                WorldCoord::new(u32::MAX - 2000, u32::MAX - 2000),
                WorldCoord::new(u32::MAX - 8000, u32::MAX - 2000),
                WorldCoord::new(u32::MAX - 8000, u32::MAX - 8000),
            ];

            let result = clip_polygon_sh_world(&exterior, &[], &bounds);
            assert!(result.is_some(), "Should handle coordinates near u32::MAX");

            let (ext, _) = result.unwrap();
            for coord in &ext {
                assert!(
                    coord.x >= bounds.x_min && coord.x <= bounds.x_max,
                    "x={} out of bounds [{}, {}]",
                    coord.x,
                    bounds.x_min,
                    bounds.x_max
                );
            }
        }

        #[test]
        fn test_world_sh_performance_10k_vertices() {
            // Performance regression test for WorldCoord-based SH
            let bounds = WorldBounds::new(1000, 1000, 5000, 5000);
            let n = 10_000usize;
            let center_x = 3000i64;
            let center_y = 3000i64;
            let radius = 4000i64;

            let mut exterior = Vec::with_capacity(n + 1);
            for i in 0..n {
                let angle = 2.0 * std::f64::consts::PI * (i as f64) / (n as f64);
                let x = center_x + (radius as f64 * angle.cos()) as i64;
                let y = center_y + (radius as f64 * angle.sin()) as i64;
                exterior.push(WorldCoord::new(
                    x.clamp(0, u32::MAX as i64) as u32,
                    y.clamp(0, u32::MAX as i64) as u32,
                ));
            }
            exterior.push(exterior[0]); // close

            let start = std::time::Instant::now();
            let result = clip_polygon_sh_world(&exterior, &[], &bounds);
            let elapsed = start.elapsed();

            assert!(result.is_some());
            assert!(
                elapsed.as_millis() < 100,
                "WorldCoord SH clipping {} vertices took {}ms (should be <100ms)",
                n,
                elapsed.as_millis()
            );
        }

        #[test]
        fn test_world_sh_empty_input() {
            let bounds = test_world_bounds();
            let result = clip_polygon_sh_world(&[], &[], &bounds);
            assert!(result.is_none(), "Empty exterior should return None");
        }
    }
}
