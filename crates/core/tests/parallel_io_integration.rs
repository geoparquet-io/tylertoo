//! Integration tests for parallel row group I/O.
//!
//! These tests verify that the parallel reader produces correct results
//! when used in the full tiling pipeline.

use std::path::Path;

use gpq_tiles_core::batch_processor::{process_geometries_parallel, DEFAULT_PARALLEL_READERS};
use gpq_tiles_core::compression::Compression;
use gpq_tiles_core::pipeline::{generate_tiles_to_writer, TilerConfig};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;

/// Test that parallel pipeline produces tiles for multi-row-group files.
#[test]
fn test_parallel_pipeline_multi_row_group() {
    let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: fixture not found");
        return;
    }

    let config = TilerConfig::new(0, 6).with_quiet(true);

    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");

    let stats = generate_tiles_to_writer(fixture, &config, &mut writer);
    assert!(stats.is_ok(), "Pipeline should succeed: {:?}", stats.err());

    let stats = stats.unwrap();
    assert!(stats.peak_bytes > 0, "Should track memory usage");

    // Finalize to temp file to verify output is valid
    let output_path = Path::new("/tmp/test-parallel-pipeline.pmtiles");
    let _ = std::fs::remove_file(output_path);
    let write_stats = writer.finalize(output_path).expect("Should finalize");
    assert!(write_stats.total_tiles > 0, "Should produce tiles");

    // Cleanup
    let _ = std::fs::remove_file(output_path);
}

/// Test that parallel reader processes all geometries from a file.
#[test]
fn test_parallel_reader_complete_coverage() {
    let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: fixture not found");
        return;
    }

    let total_geoms = std::sync::atomic::AtomicUsize::new(0);
    let row_groups_seen = std::sync::Mutex::new(Vec::new());

    let result = process_geometries_parallel(fixture, DEFAULT_PARALLEL_READERS, |info, geoms| {
        total_geoms.fetch_add(geoms.len(), std::sync::atomic::Ordering::Relaxed);
        row_groups_seen.lock().unwrap().push(info.index);
        Ok(())
    });

    assert!(result.is_ok(), "Parallel reader should succeed");

    let total = total_geoms.load(std::sync::atomic::Ordering::Relaxed);
    assert!(total > 100, "Should process many geometries, got {}", total);

    let mut seen = row_groups_seen.into_inner().unwrap();
    seen.sort();

    // All row groups should be processed exactly once
    let expected: Vec<usize> = (0..seen.len()).collect();
    assert_eq!(seen, expected, "All row groups should be processed once");
}

/// Test that parallel reader handles files with single row group.
#[test]
fn test_parallel_reader_single_row_group_file() {
    let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: fixture not found");
        return;
    }

    let mut total = 0;
    let result = process_geometries_parallel(fixture, DEFAULT_PARALLEL_READERS, |_info, geoms| {
        total += geoms.len();
        Ok(())
    });

    assert!(result.is_ok(), "Should handle single row group file");
    assert!(total > 0, "Should process geometries");
}

/// Test that parallel reader works with different concurrency levels.
#[test]
fn test_parallel_reader_various_concurrency() {
    let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: fixture not found");
        return;
    }

    // Test with different concurrency levels
    for num_readers in [1, 2, 4, 8] {
        let mut count = 0;
        let result = process_geometries_parallel(fixture, num_readers, |_info, geoms| {
            count += geoms.len();
            Ok(())
        });

        assert!(
            result.is_ok(),
            "Should work with {} readers: {:?}",
            num_readers,
            result.err()
        );
        assert!(
            count > 0,
            "Should process geometries with {} readers",
            num_readers
        );
    }
}

/// Test that parallel pipeline produces deterministic tile count.
#[test]
fn test_parallel_pipeline_deterministic_count() {
    let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: fixture not found");
        return;
    }

    let config = TilerConfig::new(0, 4)
        .with_quiet(true)
        .with_deterministic(true); // Use deterministic mode for reproducibility

    // Run twice and compare tile counts
    let mut tile_counts = Vec::new();
    for i in 0..2 {
        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");

        let _stats = generate_tiles_to_writer(fixture, &config, &mut writer)
            .expect("Pipeline should succeed");

        let output_str = format!("/tmp/test-parallel-deterministic-{}.pmtiles", i);
        let output_path = Path::new(&output_str);
        let _ = std::fs::remove_file(output_path);
        let write_stats = writer.finalize(output_path).expect("Should finalize");
        tile_counts.push(write_stats.unique_tiles);

        // Cleanup
        let _ = std::fs::remove_file(output_path);
    }

    assert_eq!(
        tile_counts[0], tile_counts[1],
        "Tile count should be deterministic"
    );
}
