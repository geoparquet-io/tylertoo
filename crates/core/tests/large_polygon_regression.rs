//! Regression test for large polygon conversion performance.
//!
//! This test ensures that conversion of files with large/complex polygons
//! completes in a reasonable time. It catches regressions like the wagyu
//! pathological slowdown that was fixed by switching to Sutherland-Hodgman.
//!
//! Run with: cargo test --release -p gpq-tiles-core --test large_polygon_regression -- --nocapture
//!
//! Expected: ~70 seconds for adm2_polygons.parquet (z0-z8)

use gpq_tiles_core::{Config, Converter};
use std::path::Path;
use std::time::{Duration, Instant};

/// Path to the ADM2 polygons fixture (1.7GB, ~472k records with complex polygons)
const ADM2_FIXTURE: &str = "../../tests/fixtures/realdata/adm2_polygons.parquet";

/// Maximum allowed time for conversion (3 minutes - generous buffer over expected ~70s)
const MAX_CONVERSION_TIME: Duration = Duration::from_secs(180);

#[test]
fn test_adm2_conversion_completes_in_reasonable_time() {
    let input_path = Path::new(ADM2_FIXTURE);
    if !input_path.exists() {
        eprintln!(
            "Skipping: {} not found. Download the fixture first.",
            ADM2_FIXTURE
        );
        return;
    }

    let output_path = std::env::temp_dir().join("adm2_regression_test.pmtiles");

    // Clean up any previous run
    let _ = std::fs::remove_file(&output_path);

    let config = Config {
        min_zoom: 0,
        max_zoom: 8,
        ..Default::default()
    };

    let converter = Converter::new(config);

    println!("=== ADM2 Large Polygon Regression Test ===");
    println!("Input: {}", input_path.display());
    println!("Output: {}", output_path.display());
    println!("Zoom levels: 0-8");
    println!("Max allowed time: {}s", MAX_CONVERSION_TIME.as_secs());
    println!();

    let start = Instant::now();
    let result = converter.convert(input_path, &output_path);
    let elapsed = start.elapsed();

    println!();
    println!("Conversion completed in {:.2}s", elapsed.as_secs_f64());

    // Check conversion succeeded
    assert!(result.is_ok(), "Conversion failed: {:?}", result.err());

    // Check it completed within time limit
    assert!(
        elapsed < MAX_CONVERSION_TIME,
        "Conversion took too long: {:.1}s (max: {}s). \
         This may indicate a regression in polygon clipping performance.",
        elapsed.as_secs_f64(),
        MAX_CONVERSION_TIME.as_secs()
    );

    // Check output file was created and has reasonable size
    let output_meta = std::fs::metadata(&output_path).expect("Output file should exist");
    assert!(
        output_meta.len() > 1_000_000,
        "Output file suspiciously small: {} bytes",
        output_meta.len()
    );

    println!(
        "✓ Conversion completed in {:.1}s (limit: {}s)",
        elapsed.as_secs_f64(),
        MAX_CONVERSION_TIME.as_secs()
    );
    println!(
        "✓ Output size: {:.1} MB",
        output_meta.len() as f64 / 1_000_000.0
    );

    // Clean up
    let _ = std::fs::remove_file(&output_path);
}

/// Quick smoke test with a smaller zoom range for CI
#[test]
fn test_adm2_smoke_test_z0_z5() {
    let input_path = Path::new(ADM2_FIXTURE);
    if !input_path.exists() {
        eprintln!("Skipping: {} not found", ADM2_FIXTURE);
        return;
    }

    let output_path = std::env::temp_dir().join("adm2_smoke_test.pmtiles");
    let _ = std::fs::remove_file(&output_path);

    let config = Config {
        min_zoom: 0,
        max_zoom: 5, // Smaller range for faster CI
        ..Default::default()
    };

    let converter = Converter::new(config);

    println!("=== ADM2 Smoke Test (z0-z5) ===");

    let start = Instant::now();
    let result = converter.convert(input_path, &output_path);
    let elapsed = start.elapsed();

    assert!(result.is_ok(), "Smoke test failed: {:?}", result.err());

    // Should complete in under 30 seconds for z0-z5
    assert!(
        elapsed < Duration::from_secs(60),
        "Smoke test too slow: {:.1}s",
        elapsed.as_secs_f64()
    );

    println!("✓ Smoke test completed in {:.1}s", elapsed.as_secs_f64());

    let _ = std::fs::remove_file(&output_path);
}
