//! CRITICAL TEST: Can i_overlay's boolean operations handle real-world
//! self-intersecting polygons from our test fixtures?
//!
//! This tests the hypothesis that i_overlay's Intersect operation with
//! FillRule can clip self-intersecting polygons correctly, even though
//! simplify_shape() cannot REPAIR them.
//!
//! If these tests pass, wagyu may not have been necessary.

use geo::{Coord, LineString, Polygon};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::overlay::FloatOverlay;
use std::fs;
use std::path::PathBuf;

// ============================================================================
// Fixture loading (same as invalid_geometry_clipping.rs)
// ============================================================================

fn fixtures_base() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests/fixtures/geometry-test-data/examples")
}

fn load_geojson_polygon(fixture_path: &str) -> Option<Polygon<f64>> {
    let full_path = fixtures_base().join(fixture_path);
    let content = fs::read_to_string(&full_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    // Extract the first feature's geometry coordinates
    let coords = &json["features"][0]["geometry"]["coordinates"];
    Some(parse_polygon_coords(coords))
}

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

// ============================================================================
// i_overlay conversion utilities
// ============================================================================

/// Convert geo::Polygon to i_overlay format
fn polygon_to_ioverlay(poly: &Polygon<f64>) -> Vec<Vec<[f64; 2]>> {
    let mut shape = Vec::with_capacity(1 + poly.interiors().len());

    // Exterior ring
    let exterior: Vec<[f64; 2]> = poly.exterior().coords().map(|c| [c.x, c.y]).collect();
    shape.push(exterior);

    // Holes
    for hole in poly.interiors() {
        let hole_coords: Vec<[f64; 2]> = hole.coords().map(|c| [c.x, c.y]).collect();
        shape.push(hole_coords);
    }

    shape
}

/// Create a clip box in i_overlay format
fn create_clip_box(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Vec<Vec<[f64; 2]>> {
    vec![vec![
        [min_x, min_y],
        [max_x, min_y],
        [max_x, max_y],
        [min_x, max_y],
        [min_x, min_y],
    ]]
}

// ============================================================================
// Validation
// ============================================================================

/// Check if i_overlay result is valid (non-empty shapes with valid contours)
fn is_valid_ioverlay_result(shapes: &[Vec<Vec<[f64; 2]>>]) -> bool {
    if shapes.is_empty() {
        return true; // Empty result is valid (nothing inside clip bounds)
    }

    for shape in shapes {
        if shape.is_empty() {
            continue;
        }
        for contour in shape {
            // i_overlay returns open contours (no closing point repeat)
            // Minimum valid: 3 points for a triangle
            if contour.len() < 3 {
                return false;
            }
        }
    }
    true
}

/// Check if result has any self-intersections (simple O(n^2) check)
fn has_self_intersection(contour: &[[f64; 2]]) -> bool {
    if contour.len() < 4 {
        return false;
    }

    let n = contour.len();
    for i in 0..n {
        let i_next = (i + 1) % n;
        for j in (i + 2)..n {
            let j_next = (j + 1) % n;
            // Skip adjacent edges
            if j_next == i {
                continue;
            }
            if edges_intersect(contour[i], contour[i_next], contour[j], contour[j_next]) {
                return true;
            }
        }
    }
    false
}

fn edges_intersect(a1: [f64; 2], a2: [f64; 2], b1: [f64; 2], b2: [f64; 2]) -> bool {
    let d1 = cross_product_sign(b1, b2, a1);
    let d2 = cross_product_sign(b1, b2, a2);
    let d3 = cross_product_sign(a1, a2, b1);
    let d4 = cross_product_sign(a1, a2, b2);

    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

fn cross_product_sign(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// Calculate area using shoelace formula
fn contour_area(contour: &[[f64; 2]]) -> f64 {
    if contour.len() < 3 {
        return 0.0;
    }
    let mut area = 0.0;
    let n = contour.len();
    for i in 0..n {
        let j = (i + 1) % n;
        area += contour[i][0] * contour[j][1];
        area -= contour[j][0] * contour[i][1];
    }
    area.abs() / 2.0
}

fn total_area(shapes: &[Vec<Vec<[f64; 2]>>]) -> f64 {
    shapes
        .iter()
        .flat_map(|shape| shape.iter())
        .map(|contour| contour_area(contour))
        .sum()
}

// ============================================================================
// THE CRITICAL TESTS
// ============================================================================

#[test]
fn test_self_intersecting_large_with_ioverlay_intersect() {
    let poly = match load_geojson_polygon(
        "problematic_geometries/problematic_self_intersection_large.geojson",
    ) {
        Some(p) => p,
        None => {
            println!("Fixture not found, skipping test");
            return;
        }
    };

    println!("=== Self-Intersecting Large Polygon ===");
    println!("Exterior ring points: {}", poly.exterior().0.len());
    println!("Holes: {}", poly.interiors().len());

    // Convert to i_overlay format
    let subj = polygon_to_ioverlay(&poly);

    // Get bounding box of polygon and create clip box that covers it
    let xs: Vec<f64> = poly.exterior().coords().map(|c| c.x).collect();
    let ys: Vec<f64> = poly.exterior().coords().map(|c| c.y).collect();
    let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    println!(
        "Polygon bounds: ({}, {}) to ({}, {})",
        min_x, min_y, max_x, max_y
    );

    // Clip box that encompasses the whole polygon
    let clip = create_clip_box(min_x - 0.001, min_y - 0.001, max_x + 0.001, max_y + 0.001);

    // Use i_overlay's Intersect operation
    let mut overlay = FloatOverlay::with_subj_and_clip(&subj[..], &clip[..]);
    let result_evenodd: Vec<Vec<Vec<[f64; 2]>>> =
        overlay.overlay(OverlayRule::Intersect, FillRule::EvenOdd);

    let mut overlay2 = FloatOverlay::with_subj_and_clip(&subj[..], &clip[..]);
    let result_nonzero: Vec<Vec<Vec<[f64; 2]>>> =
        overlay2.overlay(OverlayRule::Intersect, FillRule::NonZero);

    println!("\nResult with EvenOdd:");
    println!("  Shapes: {}", result_evenodd.len());
    println!(
        "  Valid structure: {}",
        is_valid_ioverlay_result(&result_evenodd)
    );
    println!("  Total area: {:.6}", total_area(&result_evenodd));

    // Check each shape for self-intersections
    let mut evenodd_self_intersects = false;
    for (i, shape) in result_evenodd.iter().enumerate() {
        for (j, contour) in shape.iter().enumerate() {
            if has_self_intersection(contour) {
                println!("  Shape {} contour {} HAS SELF-INTERSECTION!", i, j);
                evenodd_self_intersects = true;
            }
        }
    }
    println!(
        "  Output has self-intersections: {}",
        evenodd_self_intersects
    );

    println!("\nResult with NonZero:");
    println!("  Shapes: {}", result_nonzero.len());
    println!(
        "  Valid structure: {}",
        is_valid_ioverlay_result(&result_nonzero)
    );
    println!("  Total area: {:.6}", total_area(&result_nonzero));

    let mut nonzero_self_intersects = false;
    for (i, shape) in result_nonzero.iter().enumerate() {
        for (j, contour) in shape.iter().enumerate() {
            if has_self_intersection(contour) {
                println!("  Shape {} contour {} HAS SELF-INTERSECTION!", i, j);
                nonzero_self_intersects = true;
            }
        }
    }
    println!(
        "  Output has self-intersections: {}",
        nonzero_self_intersects
    );

    // THE KEY ASSERTION: At least one fill rule should produce valid output
    let evenodd_valid = is_valid_ioverlay_result(&result_evenodd) && !evenodd_self_intersects;
    let nonzero_valid = is_valid_ioverlay_result(&result_nonzero) && !nonzero_self_intersects;

    assert!(
        evenodd_valid || nonzero_valid,
        "i_overlay Intersect should produce valid (non-self-intersecting) output from self-intersecting input"
    );
}

#[test]
fn test_inner_exterior_ring_intersect_with_ioverlay() {
    let poly = match load_geojson_polygon(
        "invalid_geometries/invalid_inner_and_exterior_ring_intersect.geojson",
    ) {
        Some(p) => p,
        None => {
            println!("Fixture not found, skipping test");
            return;
        }
    };

    println!("=== Inner/Exterior Ring Intersect Polygon ===");
    println!("Exterior ring points: {}", poly.exterior().0.len());
    println!("Holes: {}", poly.interiors().len());

    let subj = polygon_to_ioverlay(&poly);

    // Get bounding box
    let xs: Vec<f64> = poly.exterior().coords().map(|c| c.x).collect();
    let ys: Vec<f64> = poly.exterior().coords().map(|c| c.y).collect();
    let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    let clip = create_clip_box(min_x - 0.001, min_y - 0.001, max_x + 0.001, max_y + 0.001);

    let mut overlay = FloatOverlay::with_subj_and_clip(&subj[..], &clip[..]);
    let result_evenodd: Vec<Vec<Vec<[f64; 2]>>> =
        overlay.overlay(OverlayRule::Intersect, FillRule::EvenOdd);

    let mut overlay2 = FloatOverlay::with_subj_and_clip(&subj[..], &clip[..]);
    let result_nonzero: Vec<Vec<Vec<[f64; 2]>>> =
        overlay2.overlay(OverlayRule::Intersect, FillRule::NonZero);

    println!("\nResult with EvenOdd:");
    println!("  Shapes: {}", result_evenodd.len());
    println!("  Valid: {}", is_valid_ioverlay_result(&result_evenodd));
    println!("  Area: {:.6}", total_area(&result_evenodd));

    println!("\nResult with NonZero:");
    println!("  Shapes: {}", result_nonzero.len());
    println!("  Valid: {}", is_valid_ioverlay_result(&result_nonzero));
    println!("  Area: {:.6}", total_area(&result_nonzero));

    assert!(
        is_valid_ioverlay_result(&result_evenodd) || is_valid_ioverlay_result(&result_nonzero),
        "i_overlay should handle inner/exterior ring intersections"
    );
}

#[test]
fn test_holes_problematic_with_ioverlay() {
    let poly = match load_geojson_polygon("problematic_geometries/problematic_holes.geojson") {
        Some(p) => p,
        None => {
            println!("Fixture not found, skipping test");
            return;
        }
    };

    println!("=== Problematic Holes Polygon ===");
    println!("Exterior ring points: {}", poly.exterior().0.len());
    println!("Holes: {}", poly.interiors().len());

    let subj = polygon_to_ioverlay(&poly);

    let xs: Vec<f64> = poly.exterior().coords().map(|c| c.x).collect();
    let ys: Vec<f64> = poly.exterior().coords().map(|c| c.y).collect();
    let min_x = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_x = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    let max_y = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    let clip = create_clip_box(min_x - 0.001, min_y - 0.001, max_x + 0.001, max_y + 0.001);

    let mut overlay = FloatOverlay::with_subj_and_clip(&subj[..], &clip[..]);
    let result: Vec<Vec<Vec<[f64; 2]>>> =
        overlay.overlay(OverlayRule::Intersect, FillRule::EvenOdd);

    println!("\nResult with EvenOdd:");
    println!("  Shapes: {}", result.len());
    println!("  Valid: {}", is_valid_ioverlay_result(&result));
    println!("  Area: {:.6}", total_area(&result));

    assert!(
        is_valid_ioverlay_result(&result),
        "i_overlay should handle problematic holes"
    );
}

/// Test the U-shape case - this is critical for tile clipping
#[test]
fn test_u_shape_split_with_ioverlay() {
    // Create a U-shaped polygon
    let u_shape: Vec<Vec<[f64; 2]>> = vec![vec![
        [0.0, 0.0], // bottom-left
        [0.0, 2.0], // top-left outer
        [0.3, 2.0], // top-left inner corner
        [0.3, 0.5], // inside left arm
        [0.7, 0.5], // inside bottom
        [0.7, 2.0], // top-right inner corner
        [1.0, 2.0], // top-right outer
        [1.0, 0.0], // bottom-right
        [0.0, 0.0], // close
    ]];

    // Clip box that cuts through the U opening (y > 1.0)
    let clip = create_clip_box(-0.1, 1.0, 1.1, 2.5);

    println!("=== U-Shape Split Test ===");
    println!("Clipping U-shape with bounds that cut through the opening");

    let mut overlay = FloatOverlay::with_subj_and_clip(&u_shape[..], &clip[..]);
    let result: Vec<Vec<Vec<[f64; 2]>>> =
        overlay.overlay(OverlayRule::Intersect, FillRule::EvenOdd);

    println!("\nResult:");
    println!("  Shapes: {}", result.len());
    for (i, shape) in result.iter().enumerate() {
        println!("  Shape {}: {} contours", i, shape.len());
        for (j, contour) in shape.iter().enumerate() {
            println!(
                "    Contour {}: {} points, area: {:.4}",
                j,
                contour.len(),
                contour_area(contour)
            );
        }
    }

    // The correct result is 2 separate shapes (the two arms of the U)
    assert_eq!(
        result.len(),
        2,
        "U-shape clipped across opening should produce 2 separate shapes, got {}",
        result.len()
    );

    // Each shape should be valid
    for (i, shape) in result.iter().enumerate() {
        assert!(!shape.is_empty(), "Shape {} should not be empty", i);
        for contour in shape {
            assert!(contour.len() >= 3, "Contour should have at least 3 points");
            assert!(
                !has_self_intersection(contour),
                "Shape {} should not self-intersect",
                i
            );
        }
    }

    println!("\n✓ U-shape correctly split into 2 separate polygons!");
}
