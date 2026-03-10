//! Integration test for Issue #83: Precision mismatch between filtering and MVT encoding
//!
//! This test verifies that polygons which pass the should_drop_tiny_polygon filter
//! produce valid (non-degenerate) MVT output. Before the fix, small polygons could
//! pass the f64-based filter but collapse to all-zero deltas when encoded to MVT
//! using i32 coordinates.
//!
//! Run: cargo test -p gpq-tiles-core --test issue_83_precision_mismatch -- --nocapture

use geo::{Coord, LineString, Polygon};
use gpq_tiles_core::feature_drop::{
    polygon_area_in_tile_coords, should_drop_tiny_polygon, DEFAULT_TINY_POLYGON_THRESHOLD,
};
use gpq_tiles_core::mvt::encode_polygon;
use gpq_tiles_core::tile::TileBounds;

/// Helper to decode MVT geometry commands and extract coordinate deltas
fn extract_coordinate_deltas(commands: &[u32]) -> Vec<(i32, i32)> {
    let mut deltas = Vec::new();
    let mut i = 0;

    while i < commands.len() {
        let cmd = commands[i];
        let cmd_id = cmd & 0x7;
        let count = cmd >> 3;

        match cmd_id {
            1 | 2 => {
                // MoveTo (1) or LineTo (2)
                for _ in 0..count {
                    if i + 2 < commands.len() {
                        let dx = zigzag_decode(commands[i + 1]);
                        let dy = zigzag_decode(commands[i + 2]);
                        deltas.push((dx, dy));
                        i += 2;
                    }
                }
                i += 1;
            }
            7 => {
                // ClosePath
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    deltas
}

fn zigzag_decode(n: u32) -> i32 {
    ((n >> 1) as i32) ^ (-((n & 1) as i32))
}

/// Count how many deltas are (0, 0) after the first point
fn count_zero_deltas(deltas: &[(i32, i32)]) -> usize {
    deltas
        .iter()
        .skip(1)
        .filter(|(dx, dy)| *dx == 0 && *dy == 0)
        .count()
}

#[test]
fn test_issue_83_small_polygon_at_zoom_0_produces_valid_mvt() {
    // Zoom 0 tile bounds (full world in Web Mercator)
    let tile_bounds = TileBounds::new(-180.0, -85.05112878, 180.0, 85.05112878);
    let extent = 4096u32;

    // Test polygons of increasing size
    // At zoom 0, 1 pixel ≈ 0.088° longitude × 0.042° latitude
    let test_sizes = [
        (0.01, "0.01° - sub-pixel"),
        (0.05, "0.05° - ~0.5 pixel"),
        (0.1, "0.1° - ~1 pixel"),
        (0.5, "0.5° - ~5 pixels"),
        (1.0, "1.0° - ~10 pixels"),
        (5.0, "5.0° - ~50 pixels"),
        (10.0, "10.0° - ~100 pixels"),
    ];

    println!("\n=== Issue #83 Integration Test: Small Polygon MVT Encoding ===\n");
    println!("Zoom 0 tile: 360° × 170° (full world)");
    println!("Pixel size: ~0.088° × 0.042°");
    println!("Threshold: {} sq pixels\n", DEFAULT_TINY_POLYGON_THRESHOLD);

    let mut violations = Vec::new();

    for (size, description) in test_sizes {
        let polygon = Polygon::new(
            LineString::new(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: size, y: 0.0 },
                Coord { x: size, y: size },
                Coord { x: 0.0, y: size },
                Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        );

        let area = polygon_area_in_tile_coords(&polygon, &tile_bounds, extent);
        let should_drop = should_drop_tiny_polygon(
            &polygon,
            &tile_bounds,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );

        let commands = encode_polygon(&polygon, &tile_bounds, extent);
        let deltas = extract_coordinate_deltas(&commands);
        let zero_delta_count = count_zero_deltas(&deltas);
        let total_deltas = deltas.len().saturating_sub(1); // Exclude first point

        let all_zeros = total_deltas > 0 && zero_delta_count == total_deltas;

        println!(
            "{}: area={:.4} sq px, drop={}, deltas={}, zeros={}{}",
            description,
            area,
            should_drop,
            total_deltas,
            zero_delta_count,
            if all_zeros { " ← DEGENERATE!" } else { "" }
        );

        // THE KEY INVARIANTS for Issue #83:
        // 1. If we decide to KEEP a polygon, its MVT encoding must NOT be degenerate
        // 2. If we decide to KEEP a polygon, it must have meaningful area (>= 1 sq pixel)
        //    to avoid near-degenerate output
        if !should_drop {
            if all_zeros {
                violations.push(format!(
                    "Polygon {} was KEPT (area={:.4} sq px) but MVT has ALL ZERO deltas!",
                    description, area
                ));
            }
            if area < 1.0 {
                violations.push(format!(
                    "Polygon {} was KEPT but has only {:.4} sq px area (need >= 1.0 for valid MVT)",
                    description, area
                ));
            }
        }
    }

    println!();

    if !violations.is_empty() {
        println!("=== VIOLATIONS FOUND (Issue #83) ===\n");
        for v in &violations {
            println!("  ✗ {}", v);
        }
        println!();
        panic!(
            "Issue #83: {} polygon(s) passed filter but produced degenerate MVT output",
            violations.len()
        );
    } else {
        println!("✓ All kept polygons produce valid MVT output");
    }
}

#[test]
fn test_issue_83_filtering_and_encoding_use_same_precision() {
    // This test directly compares the coordinate calculations used by
    // feature_drop (filtering) and mvt (encoding)

    let tile_bounds = TileBounds::new(-180.0, -85.05112878, 180.0, 85.05112878);
    let extent = 4096u32;

    // A polygon that's exactly at the boundary of "should keep" vs "should drop"
    // This is where precision mismatches cause the most problems
    let borderline_sizes = [0.04, 0.05, 0.06, 0.07, 0.08, 0.09, 0.1];

    println!("\n=== Borderline Polygon Precision Test ===\n");

    for size in borderline_sizes {
        let polygon = Polygon::new(
            LineString::new(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: size, y: 0.0 },
                Coord { x: size, y: size },
                Coord { x: 0.0, y: size },
                Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        );

        let filter_area = polygon_area_in_tile_coords(&polygon, &tile_bounds, extent);
        let should_drop = should_drop_tiny_polygon(
            &polygon,
            &tile_bounds,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );

        // Encode and check for degeneracy
        let commands = encode_polygon(&polygon, &tile_bounds, extent);
        let deltas = extract_coordinate_deltas(&commands);
        let zero_count = count_zero_deltas(&deltas);
        let total = deltas.len().saturating_sub(1);
        let is_degenerate = total > 0 && zero_count == total;

        println!(
            "size={:.2}°: filter_area={:.4} sq px, drop={}, degenerate={}",
            size, filter_area, should_drop, is_degenerate
        );

        // Invariant: kept polygons must not be degenerate
        if !should_drop {
            assert!(
                !is_degenerate,
                "Issue #83: Polygon of size {}° passed filter (area={:.4}) but is degenerate in MVT",
                size,
                filter_area
            );
        }
    }
}
