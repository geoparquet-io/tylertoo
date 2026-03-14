//! Integration test for zoom-by-area optimization (--zoom-by-area).
//!
//! Verifies that:
//! - Large features stop at appropriate zoom levels (prevent tile explosion)
//! - Small features start at appropriate zoom levels (prevent visual clutter)

use geo::{polygon, Geometry};
use gpq_tiles_core::hierarchical_clip::{
    max_zoom_for_bbox, min_zoom_for_bbox, zoom_range_for_bbox,
};
use gpq_tiles_core::pipeline::TilerConfig;
use gpq_tiles_core::tile::TileBounds;

#[test]
fn test_zoom_by_area_reduces_tile_count() {
    // Create synthetic features with different sizes
    let large_country = Geometry::Polygon(polygon![
        (x: -10.0, y: -10.0),
        (x: 10.0, y: -10.0),
        (x: 10.0, y: 10.0),
        (x: -10.0, y: 10.0),
        (x: -10.0, y: -10.0),
    ]);
    let large_bbox = TileBounds::new(-10.0, -10.0, 10.0, 10.0);

    let _small_building = Geometry::Polygon(polygon![
        (x: -0.001, y: -0.001),
        (x: 0.001, y: -0.001),
        (x: 0.001, y: 0.001),
        (x: -0.001, y: 0.001),
        (x: -0.001, y: -0.001),
    ]);
    let small_bbox = TileBounds::new(-0.001, -0.001, 0.001, 0.001);

    // Test zoom_range_for_bbox calculation
    let (large_min_z, large_max_z) = zoom_range_for_bbox(&large_bbox, 4.0, 400);
    let (small_min_z, small_max_z) = zoom_range_for_bbox(&small_bbox, 4.0, 400);

    println!("Large feature: z{} to z{}", large_min_z, large_max_z);
    println!("Small feature: z{} to z{}", small_min_z, small_max_z);

    // Large feature should appear immediately but stop early
    assert_eq!(large_min_z, 0, "Large feature should appear at z0");
    assert!(
        large_max_z <= 10,
        "Large feature should stop by z10, got z{}",
        large_max_z
    );

    // Small feature should appear late but go to max zoom
    assert!(
        small_min_z >= 7,
        "Small feature (0.001° ~ 100m) should not appear until z7+, got z{}",
        small_min_z
    );
    assert_eq!(
        small_max_z, 14,
        "Small feature should reach z14, got z{}",
        small_max_z
    );

    // Verify that zoom_by_area reduces tiles for large features
    use gpq_tiles_core::hierarchical_clip::clip_geometry_hierarchical_world;

    // Clip large feature WITHOUT zoom_by_area (full zoom range)
    let (results_without, _) = clip_geometry_hierarchical_world(
        &large_country,
        &large_bbox,
        0,
        14,
        8,
        4096,
        false, // zoom_by_area logic not applied
        0,
    );

    // Clip large feature WITH zoom_by_area (limited to calculated max zoom)
    let (results_with, _) = clip_geometry_hierarchical_world(
        &large_country,
        &large_bbox,
        large_min_z,
        large_max_z,
        8,
        4096,
        false, // zoom_by_area logic already applied via zoom range
        0,
    );

    // With zoom_by_area, should have MUCH fewer tiles
    let reduction_ratio = results_with.len() as f64 / results_without.len().max(1) as f64;

    println!("Tiles without zoom_by_area: {}", results_without.len());
    println!("Tiles with zoom_by_area: {}", results_with.len());
    println!("Reduction: {:.1}%", (1.0 - reduction_ratio) * 100.0);

    assert!(
        reduction_ratio < 0.1,
        "zoom_by_area should reduce tiles by >90%, got {:.1}%",
        (1.0 - reduction_ratio) * 100.0
    );
}

#[test]
fn test_zoom_by_area_delays_small_features() {
    // Test that small features don't appear at low zoom (prevents clutter)
    let tiny_feature = Geometry::Polygon(polygon![
        (x: -0.0005, y: -0.0005),
        (x: 0.0005, y: -0.0005),
        (x: 0.0005, y: 0.0005),
        (x: -0.0005, y: 0.0005),
        (x: -0.0005, y: -0.0005),
    ]);
    let tiny_bbox = TileBounds::new(-0.0005, -0.0005, 0.0005, 0.0005);

    use gpq_tiles_core::hierarchical_clip::clip_geometry_hierarchical_world;

    // Without zoom_by_area: feature appears at all zooms
    let (results_all_zooms, _) =
        clip_geometry_hierarchical_world(&tiny_feature, &tiny_bbox, 0, 14, 8, 4096, false, 0);

    // With zoom_by_area: feature only appears when visible
    let (min_z, max_z) = zoom_range_for_bbox(&tiny_bbox, 4.0, 400);
    let (results_limited, _) = clip_geometry_hierarchical_world(
        &tiny_feature,
        &tiny_bbox,
        min_z,
        max_z,
        8,
        4096,
        false,
        0,
    );

    println!("Tiny feature appears at z{}", min_z);
    println!("Tiles z0-z14: {}", results_all_zooms.len());
    println!("Tiles z{}-z{}: {}", min_z, max_z, results_limited.len());

    // Should have fewer tiles because it doesn't appear at low zooms
    assert!(
        results_limited.len() < results_all_zooms.len(),
        "zoom_by_area should reduce tiles for tiny features"
    );
}

#[test]
fn test_zoom_by_area_config_integration() {
    // Test that TilerConfig properly stores and uses zoom_by_area settings

    let mut config = TilerConfig::default();
    assert!(!config.zoom_by_area, "Should be disabled by default");
    assert_eq!(config.max_tile_threshold, 400, "Default max threshold");
    assert_eq!(config.min_pixel_area, 4.0, "Default min pixel area");

    config.zoom_by_area = true;
    config.max_tile_threshold = 1000;
    config.min_pixel_area = 8.0;

    assert!(config.zoom_by_area);
    assert_eq!(config.max_tile_threshold, 1000);
    assert_eq!(config.min_pixel_area, 8.0);
}

#[test]
fn test_min_zoom_calculation() {
    // Test min_zoom_for_bbox directly
    let tiny_bbox = TileBounds::new(-0.0005, -0.0005, 0.0005, 0.0005); // 100m
    let small_bbox = TileBounds::new(-0.005, -0.005, 0.005, 0.005); // 1km
    let large_bbox = TileBounds::new(-5.0, -5.0, 5.0, 5.0); // 1000km

    let tiny_min = min_zoom_for_bbox(&tiny_bbox, 4.0);
    let small_min = min_zoom_for_bbox(&small_bbox, 4.0);
    let large_min = min_zoom_for_bbox(&large_bbox, 4.0);

    println!("100m feature min zoom: z{}", tiny_min);
    println!("1km feature min zoom: z{}", small_min);
    println!("1000km feature min zoom: z{}", large_min);

    // Larger features should have lower min zoom
    assert!(large_min < small_min, "Large < small min zoom");
    assert!(small_min < tiny_min, "Small < tiny min zoom");
}

#[test]
fn test_max_zoom_calculation() {
    // Test max_zoom_for_bbox directly
    let tiny_bbox = TileBounds::new(-0.0005, -0.0005, 0.0005, 0.0005); // 100m
    let small_bbox = TileBounds::new(-0.005, -0.005, 0.005, 0.005); // 1km
    let large_bbox = TileBounds::new(-5.0, -5.0, 5.0, 5.0); // 1000km

    let tiny_max = max_zoom_for_bbox(&tiny_bbox, 400);
    let small_max = max_zoom_for_bbox(&small_bbox, 400);
    let large_max = max_zoom_for_bbox(&large_bbox, 400);

    println!("100m feature max zoom: z{}", tiny_max);
    println!("1km feature max zoom: z{}", small_max);
    println!("1000km feature max zoom: z{}", large_max);

    // Larger features should have lower max zoom
    assert!(large_max < small_max, "Large < small max zoom");
    assert!(small_max <= tiny_max, "Small <= tiny max zoom");
}

#[test]
fn test_different_thresholds() {
    let bbox = TileBounds::new(-1.0, -1.0, 1.0, 1.0); // 2° x 2° feature

    // Conservative max threshold (stop earlier)
    let max_z_conservative = max_zoom_for_bbox(&bbox, 100);

    // Aggressive max threshold (go deeper)
    let max_z_aggressive = max_zoom_for_bbox(&bbox, 1000);

    println!("Conservative (100 tiles): z{}", max_z_conservative);
    println!("Aggressive (1000 tiles): z{}", max_z_aggressive);

    // Conservative should stop at lower or equal zoom
    assert!(
        max_z_conservative <= max_z_aggressive,
        "Lower threshold should result in lower max zoom"
    );

    // Test min pixel area thresholds
    let min_z_strict = min_zoom_for_bbox(&bbox, 16.0); // 4x4 pixels
    let min_z_relaxed = min_zoom_for_bbox(&bbox, 4.0); // 2x2 pixels

    println!("Strict min (16 px²): z{}", min_z_strict);
    println!("Relaxed min (4 px²): z{}", min_z_relaxed);

    // Stricter threshold should have higher min zoom (appear later)
    assert!(
        min_z_strict >= min_z_relaxed,
        "Stricter threshold should result in higher min zoom"
    );
}
