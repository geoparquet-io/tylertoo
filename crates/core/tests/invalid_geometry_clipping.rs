//! Tests for clipping invalid/problematic geometries.
//!
//! These tests verify that gpq-tiles handles edge-case geometries correctly.
//! The test fixtures come from chrieke/geojson-invalid-geometry.
//!
//! # TDD Baseline
//!
//! When first written, these tests FAIL because Sutherland-Hodgman cannot
//! handle self-intersecting polygons correctly. After implementing the
//! i_overlay fallback (issue #94), they should PASS.

use geo::{Coord, Geometry, LineString, Polygon};
use gpq_tiles_core::clip::clip_geometry;
use gpq_tiles_core::tile::TileBounds;
use std::fs;
use std::path::PathBuf;

/// Helper to load a GeoJSON polygon from the test fixtures
fn load_geojson_polygon(fixture_path: &str) -> Polygon<f64> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let full_path = manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests/fixtures/geometry-test-data/examples")
        .join(fixture_path);

    let content = fs::read_to_string(&full_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {}", full_path.display(), e));

    let json: serde_json::Value = serde_json::from_str(&content).expect("Invalid JSON");

    // Extract the first feature's geometry coordinates
    let coords = &json["features"][0]["geometry"]["coordinates"];
    parse_polygon_coords(coords)
}

/// Parse GeoJSON polygon coordinates into geo::Polygon
fn parse_polygon_coords(coords: &serde_json::Value) -> Polygon<f64> {
    let rings: Vec<Vec<Coord<f64>>> = coords
        .as_array()
        .expect("Expected array of rings")
        .iter()
        .map(|ring| {
            ring.as_array()
                .expect("Expected array of coordinates")
                .iter()
                .map(|coord| {
                    let arr = coord.as_array().expect("Expected coordinate array");
                    Coord {
                        x: arr[0].as_f64().expect("Expected x coordinate"),
                        y: arr[1].as_f64().expect("Expected y coordinate"),
                    }
                })
                .collect()
        })
        .collect();

    let exterior = LineString::new(rings[0].clone());
    let interiors: Vec<LineString<f64>> = rings[1..]
        .iter()
        .map(|r| LineString::new(r.clone()))
        .collect();

    Polygon::new(exterior, interiors)
}

/// Check if a polygon has structural issues that indicate clipping failure.
///
/// This checks for:
/// - Self-touching vertices (same vertex appears consecutively)
/// - Degenerate rings (< 4 vertices)
/// - Self-intersecting edges
fn has_structural_issues(poly: &Polygon<f64>) -> bool {
    let ring = poly.exterior();

    // Check for degenerate ring
    if ring.0.len() < 4 {
        return true;
    }

    // Check for duplicate consecutive vertices (self-touching)
    for window in ring.0.windows(2) {
        if (window[0].x - window[1].x).abs() < 1e-10 && (window[0].y - window[1].y).abs() < 1e-10 {
            // Skip the closing vertex which is expected to match the first
            if window[1] != ring.0[0] || window[0] != ring.0[ring.0.len() - 2] {
                return true;
            }
        }
    }

    // Check for self-intersection using a simple O(n^2) edge intersection test
    // (Good enough for small test polygons)
    let edges: Vec<_> = ring.0.windows(2).collect();
    for (i, e1) in edges.iter().enumerate() {
        for (j, e2) in edges.iter().enumerate() {
            // Skip adjacent edges (they share a vertex)
            if i == j || (i + 1) % edges.len() == j || (j + 1) % edges.len() == i {
                continue;
            }
            if edges_intersect(e1[0], e1[1], e2[0], e2[1]) {
                return true;
            }
        }
    }

    false
}

/// Check if two line segments intersect (excluding endpoints)
fn edges_intersect(a1: Coord<f64>, a2: Coord<f64>, b1: Coord<f64>, b2: Coord<f64>) -> bool {
    let d1 = cross_product_sign(b1, b2, a1);
    let d2 = cross_product_sign(b1, b2, a2);
    let d3 = cross_product_sign(a1, a2, b1);
    let d4 = cross_product_sign(a1, a2, b2);

    // Segments intersect if endpoints are on opposite sides of each other's lines
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

fn cross_product_sign(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

// ============================================================================
// BASELINE TESTS: These demonstrate that S-H currently produces bad output
// ============================================================================

/// Test: Self-intersecting polygon (bowtie/figure-8 shape)
///
/// When clipped, S-H produces a self-touching polygon where the two "lobes"
/// meet at a single point. This is geometrically invalid for vector tiles.
///
/// Expected behavior after fix: i_overlay fallback produces a valid MultiPolygon
/// with the two lobes as separate polygons.
#[test]
fn test_self_intersecting_polygon_produces_valid_output() {
    let poly =
        load_geojson_polygon("problematic_geometries/problematic_self_intersection_large.geojson");

    // Create bounds that clip through the self-intersection
    // The polygon is in Berlin (lng ~13.38, lat ~52.507)
    let bounds = TileBounds::new(13.379, 52.506, 13.382, 52.508);

    let result = clip_geometry(&Geometry::Polygon(poly), &bounds, 0.0);

    // The result should exist (polygon intersects bounds)
    assert!(result.is_some(), "Clipping should produce a result");

    let clipped = result.unwrap();

    // After the fix, this should be either:
    // - A valid Polygon (no structural issues)
    // - A MultiPolygon with valid sub-polygons
    match &clipped {
        Geometry::Polygon(p) => {
            assert!(
                !has_structural_issues(p),
                "Clipped polygon should not have structural issues (self-touching, self-intersecting)"
            );
        }
        Geometry::MultiPolygon(mp) => {
            for p in mp.0.iter() {
                assert!(
                    !has_structural_issues(p),
                    "Each polygon in MultiPolygon should be structurally valid"
                );
            }
        }
        other => panic!("Expected Polygon or MultiPolygon, got {:?}", other),
    }
}

/// Test: Polygon with hole that intersects exterior ring
///
/// This is topologically invalid - the hole extends outside the exterior.
/// S-H doesn't handle this case; i_overlay should produce a valid result.
#[test]
fn test_hole_intersecting_exterior_produces_valid_output() {
    let poly = load_geojson_polygon(
        "invalid_geometries/invalid_inner_and_exterior_ring_intersect.geojson",
    );

    // Create bounds that encompass the whole polygon
    // The polygon is in Berlin (lng ~13.382-13.384, lat ~52.514-52.516)
    let bounds = TileBounds::new(13.381, 52.514, 13.385, 52.517);

    let result = clip_geometry(&Geometry::Polygon(poly), &bounds, 0.0);

    assert!(result.is_some(), "Clipping should produce a result");

    let clipped = result.unwrap();

    match &clipped {
        Geometry::Polygon(p) => {
            assert!(
                !has_structural_issues(p),
                "Clipped polygon should not have structural issues"
            );
            // Additionally check that hole doesn't extend outside exterior
            // (This is a more sophisticated validity check)
        }
        Geometry::MultiPolygon(mp) => {
            for p in mp.0.iter() {
                assert!(
                    !has_structural_issues(p),
                    "Each polygon in MultiPolygon should be structurally valid"
                );
            }
        }
        other => panic!("Expected Polygon or MultiPolygon, got {:?}", other),
    }
}

/// Test: U-shaped polygon clipped across the opening
///
/// When a U-shape is clipped by a horizontal line through the opening,
/// the result should be two separate polygons (the two arms of the U).
///
/// S-H produces a single polygon that traces along the clip boundary.
///
/// Fixed in i_overlay-rs v0.2.1 - the clip operation now correctly produces
/// two separate polygons for U-shapes that are split by the clip boundary.
#[test]
fn test_u_shape_split_produces_multipolygon() {
    // Create a U-shaped polygon manually
    let u_shape = Polygon::new(
        LineString::new(vec![
            Coord { x: 0.0, y: 0.0 }, // bottom-left
            Coord { x: 0.0, y: 2.0 }, // top-left outer
            Coord { x: 0.3, y: 2.0 }, // top-left inner corner
            Coord { x: 0.3, y: 0.5 }, // inside left arm
            Coord { x: 0.7, y: 0.5 }, // inside bottom
            Coord { x: 0.7, y: 2.0 }, // top-right inner corner
            Coord { x: 1.0, y: 2.0 }, // top-right outer
            Coord { x: 1.0, y: 0.0 }, // bottom-right
            Coord { x: 0.0, y: 0.0 }, // close
        ]),
        vec![],
    );

    // Clip bounds that cut through the U opening (y > 1.0)
    // This should produce two separate polygons (the two arms)
    let bounds = TileBounds::new(-0.1, 1.0, 1.1, 2.5);

    let result = clip_geometry(&Geometry::Polygon(u_shape), &bounds, 0.0);

    assert!(result.is_some(), "Clipping should produce a result");

    let clipped = result.unwrap();

    // The correct result is a MultiPolygon with 2 polygons (the two arms)
    // S-H incorrectly produces a single self-touching polygon
    match &clipped {
        Geometry::MultiPolygon(mp) => {
            assert_eq!(
                mp.0.len(),
                2,
                "U-shape clipped across opening should produce 2 separate polygons"
            );
            for p in mp.0.iter() {
                assert!(
                    !has_structural_issues(p),
                    "Each arm polygon should be structurally valid"
                );
            }
        }
        Geometry::Polygon(p) => {
            // If it's a single polygon, it MUST not have structural issues
            assert!(
                !has_structural_issues(p),
                "If single polygon, it should not have structural issues (self-touching at the clip line)"
            );
            // Additionally, a single polygon result is wrong for this case
            panic!(
                "U-shape clipped across opening should produce MultiPolygon, not single Polygon"
            );
        }
        other => panic!("Expected Polygon or MultiPolygon, got {:?}", other),
    }
}

// ============================================================================
// REGRESSION TESTS: Ensure valid polygons still work after adding fallback
// ============================================================================

/// Test: Valid simple polygon should still clip correctly
#[test]
fn test_valid_polygon_clips_correctly() {
    // Simple square polygon
    let square = Polygon::new(
        LineString::new(vec![
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 1.0, y: 0.0 },
            Coord { x: 1.0, y: 1.0 },
            Coord { x: 0.0, y: 1.0 },
            Coord { x: 0.0, y: 0.0 },
        ]),
        vec![],
    );

    // Clip to right half
    let bounds = TileBounds::new(0.5, -0.1, 1.1, 1.1);

    let result = clip_geometry(&Geometry::Polygon(square), &bounds, 0.0);

    assert!(result.is_some(), "Clipping should produce a result");

    let clipped = result.unwrap();

    match &clipped {
        Geometry::Polygon(p) => {
            assert!(
                !has_structural_issues(p),
                "Clipped valid polygon should remain valid"
            );
            // Should be a rectangle from x=0.5 to x=1.0
            let exterior = p.exterior();
            assert!(
                exterior.0.len() >= 4,
                "Clipped polygon should have at least 4 vertices"
            );
        }
        other => panic!("Expected Polygon, got {:?}", other),
    }
}

/// Test: Valid polygon with hole should clip correctly
#[test]
fn test_valid_polygon_with_hole_clips_correctly() {
    // Square with square hole
    let poly_with_hole = Polygon::new(
        LineString::new(vec![
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 2.0, y: 0.0 },
            Coord { x: 2.0, y: 2.0 },
            Coord { x: 0.0, y: 2.0 },
            Coord { x: 0.0, y: 0.0 },
        ]),
        vec![LineString::new(vec![
            Coord { x: 0.5, y: 0.5 },
            Coord { x: 1.5, y: 0.5 },
            Coord { x: 1.5, y: 1.5 },
            Coord { x: 0.5, y: 1.5 },
            Coord { x: 0.5, y: 0.5 },
        ])],
    );

    // Clip to encompass everything
    let bounds = TileBounds::new(-0.1, -0.1, 2.1, 2.1);

    let result = clip_geometry(&Geometry::Polygon(poly_with_hole), &bounds, 0.0);

    assert!(result.is_some(), "Clipping should produce a result");

    let clipped = result.unwrap();

    match &clipped {
        Geometry::Polygon(p) => {
            assert!(
                !has_structural_issues(p),
                "Polygon with hole should remain valid after clipping"
            );
            // Hole should be preserved when fully inside bounds
        }
        other => panic!("Expected Polygon, got {:?}", other),
    }
}
