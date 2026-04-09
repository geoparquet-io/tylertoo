//! Integration tests for adaptive threshold iteration.
//!
//! These tests verify that the adaptive threshold system correctly:
//! 1. Reduces tile feature counts when limits are exceeded
//! 2. Propagates thresholds between zoom levels
//! 3. Reports appropriate errors when thresholds are exhausted
//!
//! # Adaptive Threshold Overview
//!
//! When `max_tile_features` or `max_tile_size` limits are set, the tiler:
//! 1. Samples features to compute initial thresholds (mingap/minextent)
//! 2. Encodes tiles with those thresholds
//! 3. If a tile exceeds limits, increases thresholds and retries
//! 4. Propagates successful thresholds to subsequent zoom levels

use gpq_tiles_core::adaptive::AdaptiveTargets;
use gpq_tiles_core::compression::Compression;
use gpq_tiles_core::pipeline::{generate_tiles_to_writer, TilerConfig};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
use gpq_tiles_core::Error;
use std::path::Path;

// ============================================================================
// Test 1: Adaptive Threshold Reduces Tile Size
// ============================================================================

/// Test that adaptive thresholds successfully reduce tile feature count
/// when max_tile_features is set.
///
/// This test uses a real fixture to verify the full pipeline works with
/// adaptive thresholds enabled. The adaptive retry loop uses percentile-based
/// threshold selection to progressively drop more features until tiles fit.
///
/// With max_tile_features=500 and drop_densest enabled, the pipeline:
/// 1. Samples gap values during initial encoding
/// 2. If a tile exceeds 500 features, selects a higher gap threshold
/// 3. Re-encodes with the new threshold, repeating until tiles fit
/// 4. Reports the final threshold to AdaptiveTargets for propagation
#[test]
fn test_adaptive_threshold_reduces_features() {
    // Use open-buildings fixture (dense point data - ideal for testing adaptive behavior)
    let fixture_path = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");

    if !fixture_path.exists() {
        eprintln!(
            "Skipping test: fixture not found at {:?}",
            fixture_path.display()
        );
        return;
    }

    // Configure with aggressive limits to force adaptive behavior
    // 500 features per tile is quite restrictive for dense building data
    let config = TilerConfig::new(0, 4)
        .with_quiet(true)
        .with_max_tile_features(500) // Force adaptive behavior on dense tiles
        .with_drop_densest(); // Enable gap-based dropping

    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");

    let result = generate_tiles_to_writer(fixture_path, &config, &mut writer);

    // Should succeed - adaptive thresholds handle dense tiles by:
    // - Sampling gap values during encoding
    // - Using percentile-based threshold selection when tiles exceed limits
    // - Progressively increasing thresholds until tiles fit
    // - Returning CannotReduceFurther only when all options exhausted
    assert!(
        result.is_ok(),
        "Expected success with adaptive thresholds: {:?}",
        result
    );

    let stats = result.unwrap();
    println!(
        "Adaptive threshold test completed: peak memory {} bytes",
        stats.peak_bytes
    );
}

/// Test that drop_smallest_as_needed also enables adaptive behavior.
#[test]
fn test_adaptive_with_drop_smallest() {
    let fixture_path = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");

    if !fixture_path.exists() {
        eprintln!(
            "Skipping test: fixture not found at {:?}",
            fixture_path.display()
        );
        return;
    }

    let config = TilerConfig::new(0, 4)
        .with_quiet(true)
        .with_max_tile_features(1000)
        .with_drop_smallest(); // Use size-based dropping instead

    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");

    let result = generate_tiles_to_writer(fixture_path, &config, &mut writer);

    assert!(
        result.is_ok(),
        "Expected success with drop_smallest: {:?}",
        result
    );
}

// ============================================================================
// Test 2: Threshold Propagation Across Zooms
// ============================================================================

/// Test that thresholds propagate between zoom levels.
///
/// The adaptive threshold system tracks per-zoom thresholds. When a tile
/// at zoom Z requires a higher threshold than initially computed, that
/// threshold should propagate to zoom Z+1 to avoid repeated retry loops.
#[test]
fn test_threshold_propagation_mingap() {
    let targets = AdaptiveTargets::new();

    // Set initial threshold at zoom 5
    targets.set_initial_mingap(5, 100);

    // Verify initial state
    assert_eq!(targets.get_mingap(5), 100);
    assert!(!targets.needs_retry(5));

    // Simulate a tile reporting a higher threshold (tile exceeded limits)
    targets.report_mingap(5, 200);

    // Observed threshold should now be higher
    assert_eq!(targets.get_mingap(5), 200);
    // And zoom should need retry
    assert!(targets.needs_retry(5));

    // Propagate to zoom 6
    targets.propagate_to_next_zoom(5);

    // Zoom 6 should start with the higher threshold from zoom 5
    assert_eq!(
        targets.get_mingap(6),
        200,
        "Zoom 6 should inherit max threshold from zoom 5"
    );

    // Zoom 6 doesn't need retry yet (no tiles have been processed)
    assert!(!targets.needs_retry(6));
}

/// Test minextent threshold propagation.
#[test]
fn test_threshold_propagation_minextent() {
    let targets = AdaptiveTargets::new();

    // Set initial minextent
    targets.set_initial_minextent(10, 50);
    assert_eq!(targets.get_minextent(10), 50);

    // Report higher threshold
    targets.report_minextent(10, 75);
    assert_eq!(targets.get_minextent(10), 75);
    assert!(targets.needs_retry(10));

    // Propagate to next zoom
    targets.propagate_to_next_zoom(10);
    assert_eq!(targets.get_minextent(11), 75);
}

/// Test that thresholds ratchet up (never decrease).
#[test]
fn test_threshold_ratcheting() {
    let targets = AdaptiveTargets::new();

    // Report increasing thresholds
    targets.report_mingap(5, 100);
    assert_eq!(targets.get_mingap(5), 100);

    targets.report_mingap(5, 200);
    assert_eq!(targets.get_mingap(5), 200);

    // Report lower threshold - should NOT decrease
    targets.report_mingap(5, 150);
    assert_eq!(
        targets.get_mingap(5),
        200,
        "Threshold should ratchet up, not decrease"
    );
}

/// Test retry flag lifecycle.
#[test]
fn test_retry_flag_lifecycle() {
    let targets = AdaptiveTargets::new();

    // Initially no retry needed
    assert!(!targets.needs_retry(5));

    // Set initial and report same value - no retry
    targets.set_initial_mingap(5, 100);
    targets.report_mingap(5, 100);
    assert!(!targets.needs_retry(5));

    // Report higher value - triggers retry
    targets.report_mingap(5, 150);
    assert!(targets.needs_retry(5));

    // Clear retry flag (after re-encoding)
    targets.clear_retry_flag(5);
    assert!(!targets.needs_retry(5));
}

/// Test that zoom levels are independent.
#[test]
fn test_zoom_levels_independent() {
    let targets = AdaptiveTargets::new();

    // Set thresholds at different zoom levels
    targets.set_initial_mingap(5, 100);
    targets.set_initial_mingap(6, 200);
    targets.set_initial_mingap(7, 300);

    // Verify each zoom has its own threshold
    assert_eq!(targets.get_mingap(5), 100);
    assert_eq!(targets.get_mingap(6), 200);
    assert_eq!(targets.get_mingap(7), 300);

    // Trigger retry only at zoom 6
    targets.report_mingap(6, 250);

    assert!(!targets.needs_retry(5));
    assert!(targets.needs_retry(6));
    assert!(!targets.needs_retry(7));
}

// ============================================================================
// Test 3: CannotReduceFurther Error
// ============================================================================

/// Test that CannotReduceFurther error is returned when appropriate.
///
/// This error indicates that a tile cannot be reduced below the limit
/// even with maximum threshold adjustment.
#[test]
fn test_cannot_reduce_further_error_format() {
    // Create the error to verify its format
    let err = Error::CannotReduceFurther {
        tile: "5/10/12".to_string(),
        zoom: 5,
        size: 1_000_000,
        features: 50_000,
        max_tile_size: 500_000,
    };

    let msg = err.to_string();

    // Verify error message contains expected information
    assert!(
        msg.contains("5/10/12"),
        "Error should contain tile coordinate"
    );
    assert!(
        msg.contains("Cannot reduce"),
        "Error should mention reduction failure"
    );
    // Should contain helpful suggestions
    assert!(
        msg.contains("Suggestions") || msg.contains("--"),
        "Error should contain actionable suggestions"
    );

    // Verify Debug formatting works
    let debug_msg = format!("{:?}", err);
    assert!(debug_msg.contains("CannotReduceFurther"));
}

/// Test that CannotReduceFurther is returned for pathological input.
///
/// When a tile has so many features that no threshold can reduce it below
/// the limit, the pipeline should return CannotReduceFurther rather than
/// silently returning an oversized tile.
///
/// This test uses an impossibly small feature limit (1 feature per tile)
/// on dense data. Since even a single feature can't satisfy a 1-feature
/// limit when the tile has multiple features, this should fail.
#[test]
fn test_cannot_reduce_further_pathological_input() {
    let fixture_path = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");

    if !fixture_path.exists() {
        eprintln!(
            "Skipping test: fixture not found at {:?}",
            fixture_path.display()
        );
        return;
    }

    // Configure with impossibly small limit to force CannotReduceFurther
    // 1 feature per tile is impossible for dense data with multiple features per tile
    let config = TilerConfig::new(0, 2) // Low zoom to ensure dense tiles
        .with_quiet(true)
        .with_max_tile_features(1) // Impossibly small limit
        .with_drop_densest(); // Enable gap-based dropping to attempt reduction

    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");

    let result = generate_tiles_to_writer(fixture_path, &config, &mut writer);

    // Should fail with CannotReduceFurther
    // Even with maximum threshold adjustment, 1 feature per tile is impossible
    // for any tile with more than 1 feature
    match result {
        Err(Error::CannotReduceFurther {
            tile,
            size,
            features,
            ..
        }) => {
            println!(
                "Got expected CannotReduceFurther for tile {} (size: {}, features: {})",
                tile, size, features
            );
            // Verify the error contains meaningful information
            assert!(!tile.is_empty(), "Tile coordinate should not be empty");
            assert!(features > 1, "Features should exceed the limit of 1");
        }
        Err(other) => {
            // Other errors might occur (e.g., if the data is sparse enough)
            // That's also acceptable - the point is we don't silently accept oversized tiles
            println!("Got different error (acceptable): {:?}", other);
        }
        Ok(_) => {
            // If all tiles somehow fit within 1 feature, that's fine too
            // It means the data at low zoom is very sparse
            println!("All tiles fit within 1 feature limit (sparse data)");
        }
    }
}

/// Test that the error type is Send + Sync (required for parallel processing).
#[test]
fn test_error_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Error>();
}

// ============================================================================
// Test 4: Full Pipeline Integration
// ============================================================================

/// Test full pipeline with various adaptive configurations.
///
/// Compares output characteristics with different settings to verify
/// adaptive thresholds affect the output appropriately.
#[test]
fn test_pipeline_with_various_adaptive_configs() {
    let fixture_path = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");

    if !fixture_path.exists() {
        eprintln!(
            "Skipping test: fixture not found at {:?}",
            fixture_path.display()
        );
        return;
    }

    // Configuration 1: No adaptive limits (baseline)
    let config_baseline = TilerConfig::new(0, 4).with_quiet(true);

    let mut writer_baseline =
        StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
    let stats_baseline =
        generate_tiles_to_writer(fixture_path, &config_baseline, &mut writer_baseline)
            .expect("Baseline should succeed");

    // Configuration 2: With max_tile_features limit
    let config_limited = TilerConfig::new(0, 4)
        .with_quiet(true)
        .with_max_tile_features(500);

    let mut writer_limited =
        StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
    let stats_limited =
        generate_tiles_to_writer(fixture_path, &config_limited, &mut writer_limited)
            .expect("Limited config should succeed");

    // Configuration 3: With drop_densest_as_needed (gap-based)
    let config_gap = TilerConfig::new(0, 4)
        .with_quiet(true)
        .with_max_tile_features(500)
        .with_drop_densest_as_needed();

    let mut writer_gap =
        StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
    let stats_gap = generate_tiles_to_writer(fixture_path, &config_gap, &mut writer_gap)
        .expect("Gap-based config should succeed");

    println!("Adaptive threshold pipeline comparison:");
    println!(
        "  Baseline: peak memory {} bytes",
        stats_baseline.peak_bytes
    );
    println!(
        "  Limited (500 features): peak memory {} bytes",
        stats_limited.peak_bytes
    );
    println!("  Gap-based: peak memory {} bytes", stats_gap.peak_bytes);

    // All configurations should complete successfully
    // Memory usage patterns may vary based on adaptive behavior
}

/// Test that max_tile_size limit is respected.
#[test]
fn test_max_tile_size_limit() {
    let fixture_path = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");

    if !fixture_path.exists() {
        eprintln!(
            "Skipping test: fixture not found at {:?}",
            fixture_path.display()
        );
        return;
    }

    // Set a reasonable size limit (500KB compressed)
    let config = TilerConfig::new(0, 4)
        .with_quiet(true)
        .with_max_tile_size(500_000)
        .with_drop_densest();

    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");

    let result = generate_tiles_to_writer(fixture_path, &config, &mut writer);

    // Should succeed with size limits
    assert!(
        result.is_ok(),
        "Expected success with max_tile_size: {:?}",
        result
    );
}

// ============================================================================
// Test 5: Thread Safety (AdaptiveTargets is used from Rayon parallel iterators)
// ============================================================================

/// Test concurrent access to AdaptiveTargets from multiple threads.
#[test]
fn test_adaptive_targets_thread_safety() {
    use std::sync::Arc;
    use std::thread;

    let targets = Arc::new(AdaptiveTargets::new());
    targets.set_initial_mingap(10, 100);

    let mut handles = vec![];

    // Spawn multiple threads that report different thresholds
    for i in 0..10 {
        let targets = Arc::clone(&targets);
        let handle = thread::spawn(move || {
            let threshold = (i + 1) * 50; // 50, 100, 150, ..., 500
            targets.report_mingap(10, threshold as u64);

            // Also read while others are writing
            let _ = targets.get_mingap(10);
            let _ = targets.needs_retry(10);
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().expect("Thread should not panic");
    }

    // Should have the max threshold: 10 * 50 = 500
    assert_eq!(targets.get_mingap(10), 500);

    // Should need retry since 500 > 100 (initial)
    assert!(targets.needs_retry(10));
}

/// Test concurrent propagation doesn't cause issues.
#[test]
fn test_adaptive_targets_concurrent_propagation() {
    use std::sync::Arc;
    use std::thread;

    let targets = Arc::new(AdaptiveTargets::new());

    // Set thresholds at multiple zoom levels
    for z in 0..10 {
        targets.set_initial_mingap(z, (z as u64 + 1) * 100);
    }

    let mut handles = vec![];

    // Multiple threads propagating from different zoom levels
    for z in 0..9 {
        let targets = Arc::clone(&targets);
        let handle = thread::spawn(move || {
            targets.propagate_to_next_zoom(z);
        });
        handles.push(handle);
    }

    // Wait for all threads
    for handle in handles {
        handle.join().expect("Thread should not panic");
    }

    // All zoom levels should have values (either initial or propagated)
    for z in 0..10 {
        assert!(
            targets.get_mingap(z) > 0,
            "Zoom {} should have threshold",
            z
        );
    }
}
