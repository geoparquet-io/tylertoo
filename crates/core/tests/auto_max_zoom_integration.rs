//! Integration test for per-feature max zoom optimization (--auto-max-zoom).
//!
//! Verifies that large features stop at appropriate zoom levels instead of
//! generating millions of tiles at high zooms.

use geo::{polygon, Geometry};
use gpq_tiles_core::hierarchical_clip::max_zoom_for_bbox;
use gpq_tiles_core::pipeline::TilerConfig;
use gpq_tiles_core::tile::TileBounds;

#[test]
fn test_auto_max_zoom_reduces_tile_count() {
    // Create synthetic features with different sizes
    let large_country = Geometry::Polygon(polygon![
        (x: -10.0, y: -10.0),
        (x: 10.0, y: -10.0),
        (x: 10.0, y: 10.0),
        (x: -10.0, y: 10.0),
        (x: -10.0, y: -10.0),
    ]);
    let large_bbox = TileBounds::new(-10.0, -10.0, 10.0, 10.0);

    let small_building = Geometry::Polygon(polygon![
        (x: -0.001, y: -0.001),
        (x: 0.001, y: -0.001),
        (x: 0.001, y: 0.001),
        (x: -0.001, y: 0.001),
        (x: -0.001, y: -0.001),
    ]);
    let small_bbox = TileBounds::new(-0.001, -0.001, 0.001, 0.001);

    // Test max_zoom_for_bbox calculation
    let large_max_z = max_zoom_for_bbox(&large_bbox, 400);
    let small_max_z = max_zoom_for_bbox(&small_bbox, 400);

    // Large feature should stop at low zoom
    assert!(
        large_max_z <= 10,
        "Large feature should stop by z10, got z{}",
        large_max_z
    );

    // Small feature should go to max zoom
    assert_eq!(
        small_max_z, 14,
        "Small feature should reach z14, got z{}",
        small_max_z
    );

    // Verify that auto_max_zoom actually uses these calculations
    use gpq_tiles_core::hierarchical_clip::clip_geometry_hierarchical_world;

    // Clip large feature WITHOUT auto_max_zoom
    let (results_without, _) = clip_geometry_hierarchical_world(
        &large_country,
        &large_bbox,
        0,
        14,
        8,
        4096,
        false, // auto_max_zoom = false
        400,
    );

    // Clip large feature WITH auto_max_zoom
    let (results_with, _) = clip_geometry_hierarchical_world(
        &large_country,
        &large_bbox,
        0,
        14,
        8,
        4096,
        true, // auto_max_zoom = true
        400,
    );

    // With auto_max_zoom, should have MUCH fewer tiles
    let reduction_ratio = results_with.len() as f64 / results_without.len().max(1) as f64;

    println!("Tiles without auto_max_zoom: {}", results_without.len());
    println!("Tiles with auto_max_zoom: {}", results_with.len());
    println!("Reduction: {:.1}%", (1.0 - reduction_ratio) * 100.0);

    assert!(
        reduction_ratio < 0.1,
        "auto_max_zoom should reduce tiles by >90%, got {:.1}%",
        (1.0 - reduction_ratio) * 100.0
    );

    // Small feature should NOT be affected by auto_max_zoom
    let (small_results_without, _) =
        clip_geometry_hierarchical_world(&small_building, &small_bbox, 0, 14, 8, 4096, false, 400);

    let (small_results_with, _) =
        clip_geometry_hierarchical_world(&small_building, &small_bbox, 0, 14, 8, 4096, true, 400);

    // Small features should have same tile count with/without auto_max_zoom
    assert_eq!(
        small_results_without.len(),
        small_results_with.len(),
        "Small features should not be affected by auto_max_zoom"
    );
}

#[test]
fn test_auto_max_zoom_config_integration() {
    // Test that TilerConfig properly stores and uses auto_max_zoom settings

    let mut config = TilerConfig::default();
    assert!(!config.auto_max_zoom, "Should be disabled by default");
    assert_eq!(config.min_tile_threshold, 400, "Default threshold");

    config.auto_max_zoom = true;
    config.min_tile_threshold = 1000;

    assert!(config.auto_max_zoom);
    assert_eq!(config.min_tile_threshold, 1000);
}

#[test]
fn test_different_thresholds() {
    let bbox = TileBounds::new(-1.0, -1.0, 1.0, 1.0); // 2° x 2° feature

    // Conservative threshold (stop earlier)
    let max_z_conservative = max_zoom_for_bbox(&bbox, 100);

    // Aggressive threshold (go deeper)
    let max_z_aggressive = max_zoom_for_bbox(&bbox, 1000);

    println!("Conservative (100 tiles): z{}", max_z_conservative);
    println!("Aggressive (1000 tiles): z{}", max_z_aggressive);

    // Conservative should stop at lower or equal zoom
    assert!(
        max_z_conservative <= max_z_aggressive,
        "Lower threshold should result in lower max zoom"
    );
}
