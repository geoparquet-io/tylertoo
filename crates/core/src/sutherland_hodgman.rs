//! Sutherland-Hodgman polygon clipping for axis-aligned rectangles.
//!
//! This module implements the Sutherland-Hodgman algorithm for clipping polygons
//! against axis-aligned rectangular bounds. Unlike wagyu's Vatti algorithm which
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
/// Note: Unlike Vatti/wagyu, Sutherland-Hodgman does NOT split a polygon into
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
            "Clipping {} vertices took {:.3}s (should be <0.5s, wagyu took ~10s)",
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
}
