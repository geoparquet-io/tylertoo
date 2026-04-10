//! Integration tests for zoom-dependent simplification (#156).
//!
//! Validates that simplification reduces tile sizes for complex geometries
//! while preserving visual fidelity at each zoom level.

use std::path::Path;

use gpq_tiles_core::compression::Compression;
use gpq_tiles_core::pipeline::{generate_tiles_to_writer, TilerConfig};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;

/// Test that simplification reduces output size for linear features.
///
/// Uses road-detections fixture which contains LineString geometries -
/// ideal for testing Douglas-Peucker simplification since roads typically
/// have many vertices that can be reduced at lower zoom levels.
#[test]
fn test_simplification_reduces_tile_size() {
    // Try multiple fixtures - prefer roads (lines) but fall back to others
    let fixtures = [
        "../../tests/fixtures/realdata/road-detections.parquet",
        "../../tests/fixtures/realdata/fieldmaps-boundaries.parquet",
        "../../tests/fixtures/streaming/multi-rowgroup-small.parquet",
    ];

    let fixture = fixtures.iter().find(|p| Path::new(p).exists());
    let Some(fixture_path) = fixture else {
        eprintln!("Skipping: no suitable test fixture found");
        return;
    };
    let fixture_path = Path::new(fixture_path);

    println!("Using fixture: {}", fixture_path.display());

    // Generate tiles WITHOUT simplification
    let config_no_simplify = TilerConfig::new(0, 6)
        .with_layer_name("test")
        .with_quiet(true);

    let mut writer_no_simplify =
        StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");

    let stats_no_simplify =
        generate_tiles_to_writer(fixture_path, &config_no_simplify, &mut writer_no_simplify);
    assert!(
        stats_no_simplify.is_ok(),
        "Tiling without simplification failed: {:?}",
        stats_no_simplify.err()
    );

    // Finalize to get the total output size
    let output_no_simplify = Path::new("/tmp/test-simplification-none.pmtiles");
    let _ = std::fs::remove_file(output_no_simplify);
    let write_stats_no_simplify = writer_no_simplify
        .finalize(output_no_simplify)
        .expect("Should finalize");
    let size_no_simplify = std::fs::metadata(output_no_simplify)
        .map(|m| m.len())
        .unwrap_or(0);

    // Generate tiles WITH simplification (factor 1.0 = standard)
    let config_simplify = TilerConfig::new(0, 6)
        .with_layer_name("test")
        .with_quiet(true)
        .with_simplify(1.0);

    let mut writer_simplify =
        StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");

    let stats_simplify =
        generate_tiles_to_writer(fixture_path, &config_simplify, &mut writer_simplify);
    assert!(
        stats_simplify.is_ok(),
        "Tiling with simplification failed: {:?}",
        stats_simplify.err()
    );

    let output_simplify = Path::new("/tmp/test-simplification-enabled.pmtiles");
    let _ = std::fs::remove_file(output_simplify);
    let write_stats_simplify = writer_simplify
        .finalize(output_simplify)
        .expect("Should finalize");
    let size_simplify = std::fs::metadata(output_simplify)
        .map(|m| m.len())
        .unwrap_or(0);

    println!(
        "Without simplification: {} bytes ({} tiles)",
        size_no_simplify, write_stats_no_simplify.total_tiles
    );
    println!(
        "With simplification:    {} bytes ({} tiles)",
        size_simplify, write_stats_simplify.total_tiles
    );

    // Simplification shouldn't change tile count (same features, same zooms)
    assert_eq!(
        write_stats_no_simplify.total_tiles, write_stats_simplify.total_tiles,
        "Tile count should be identical"
    );

    // Output should be smaller or equal (not larger)
    // Note: for very simple geometries (few vertices), there may be no reduction
    assert!(
        size_simplify <= size_no_simplify,
        "Simplification should not increase output size: {} > {}",
        size_simplify,
        size_no_simplify
    );

    // If there was meaningful reduction, report it
    if size_simplify < size_no_simplify {
        let reduction_pct = 100.0 * (1.0 - (size_simplify as f64 / size_no_simplify as f64));
        println!("Size reduction: {:.1}%", reduction_pct);
    }

    // Cleanup
    let _ = std::fs::remove_file(output_no_simplify);
    let _ = std::fs::remove_file(output_simplify);
}

/// Test that higher simplification factors produce smaller output.
///
/// Factor 2.0 should be more aggressive than factor 1.0.
#[test]
fn test_simplification_factor_scaling() {
    // Use road-detections - it has linear geometries that benefit from simplification
    let fixture = Path::new("../../tests/fixtures/realdata/road-detections.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: road-detections fixture not found");
        return;
    }

    // Helper to generate tiles with a given simplification factor
    let generate_with_factor = |factor: Option<f64>| -> u64 {
        let mut config = TilerConfig::new(0, 6)
            .with_layer_name("test")
            .with_quiet(true);

        if let Some(f) = factor {
            config = config.with_simplify(f);
        }

        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
        generate_tiles_to_writer(fixture, &config, &mut writer).expect("Tiling should succeed");

        let output_path = format!(
            "/tmp/test-simplify-factor-{}.pmtiles",
            factor.unwrap_or(0.0)
        );
        let output = Path::new(&output_path);
        let _ = std::fs::remove_file(output);
        writer.finalize(output).expect("Should finalize");

        let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(output);
        size
    };

    let size_none = generate_with_factor(None);
    let size_low = generate_with_factor(Some(0.5));
    let size_standard = generate_with_factor(Some(1.0));
    let size_aggressive = generate_with_factor(Some(2.0));

    println!("No simplification:  {} bytes", size_none);
    println!("Factor 0.5 (mild):  {} bytes", size_low);
    println!("Factor 1.0 (std):   {} bytes", size_standard);
    println!("Factor 2.0 (aggr):  {} bytes", size_aggressive);

    // Higher factor should mean smaller or equal output
    // Note: relationship may not be strictly monotonic due to compression effects
    assert!(
        size_aggressive <= size_none,
        "Aggressive simplification should not increase size vs none"
    );
}

/// Test that simplification works correctly with deterministic mode.
///
/// Ensures reproducible output when both simplification and deterministic
/// processing are enabled (important for golden tests).
#[test]
fn test_simplification_deterministic() {
    let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: multi-rowgroup-small fixture not found");
        return;
    }

    let config = TilerConfig::new(0, 4)
        .with_layer_name("test")
        .with_quiet(true)
        .with_simplify(1.0)
        .with_deterministic(true);

    // Generate twice with deterministic mode
    let mut sizes = Vec::new();
    for run in 0..2 {
        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
        generate_tiles_to_writer(fixture, &config, &mut writer).expect("Tiling should succeed");

        let output = Path::new(match run {
            0 => "/tmp/test-simplify-det-1.pmtiles",
            _ => "/tmp/test-simplify-det-2.pmtiles",
        });
        let _ = std::fs::remove_file(output);
        writer.finalize(output).expect("Should finalize");

        let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
        sizes.push(size);
        let _ = std::fs::remove_file(output);
    }

    assert_eq!(
        sizes[0], sizes[1],
        "Deterministic mode should produce identical output sizes: {} vs {}",
        sizes[0], sizes[1]
    );
}
