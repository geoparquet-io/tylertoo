//! Integration tests for leaf directory support (Issue #88)
//!
//! These tests verify that PMTiles files with many tiles are correctly
//! structured with leaf directories to fit in the initial 16KB HTTP range request.

use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::Command;

/// PMTiles initial HTTP range request size (16KB)
const INITIAL_FETCH_SIZE: usize = 16384;
/// PMTiles header size
const HEADER_SIZE: usize = 127;
/// Maximum root directory size that fits in initial fetch
const MAX_ROOT_DIR_SIZE: usize = INITIAL_FETCH_SIZE - HEADER_SIZE;

/// Read PMTiles header fields from a file
fn read_pmtiles_header(path: &Path) -> (usize, u64, u64) {
    let data = fs::read(path).expect("Failed to read PMTiles file");

    // Verify magic number
    assert_eq!(&data[0..7], b"PMTiles", "Invalid PMTiles magic number");
    assert_eq!(data[7], 3, "Expected PMTiles v3");

    let root_dir_length = u64::from_le_bytes(data[16..24].try_into().unwrap()) as usize;
    let leaf_dirs_offset = u64::from_le_bytes(data[40..48].try_into().unwrap());
    let leaf_dirs_length = u64::from_le_bytes(data[48..56].try_into().unwrap());

    (root_dir_length, leaf_dirs_offset, leaf_dirs_length)
}

/// Verify that pmtiles CLI can read the file (if available)
fn verify_with_pmtiles_cli(path: &Path) -> bool {
    // Try to run pmtiles verify
    let result = Command::new("pmtiles")
        .args(["verify", path.to_str().unwrap()])
        .output();

    match result {
        Ok(output) => {
            if output.status.success() {
                true
            } else {
                eprintln!(
                    "pmtiles verify failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                false
            }
        }
        Err(_) => {
            eprintln!("pmtiles CLI not available, skipping verify step");
            true // Don't fail if CLI not installed
        }
    }
}

#[test]
fn test_cli_produces_valid_leaf_directories() {
    // This test runs the CLI on a real fixture and verifies the output
    let fixture_path = Path::new("tests/fixtures/realdata/fieldmaps-madagascar-adm4.parquet");

    if !fixture_path.exists() {
        eprintln!("Skipping test: fixture not found at {:?}", fixture_path);
        return;
    }

    let output_path = Path::new("/tmp/test-leaf-integration.pmtiles");
    let _ = fs::remove_file(output_path);

    // Run the CLI to generate PMTiles
    let cli_result = Command::new("cargo")
        .args([
            "run",
            "--release",
            "--package",
            "gpq-tiles",
            "--",
            fixture_path.to_str().unwrap(),
            output_path.to_str().unwrap(),
            "--max-zoom",
            "10",
        ])
        .output();

    match cli_result {
        Ok(output) => {
            if !output.status.success() {
                eprintln!("CLI stderr: {}", String::from_utf8_lossy(&output.stderr));
                panic!("CLI failed to produce PMTiles file");
            }
        }
        Err(e) => {
            panic!("Failed to run CLI: {}", e);
        }
    }

    assert!(output_path.exists(), "PMTiles file should be created");

    // Verify header fields
    let (root_dir_length, leaf_dirs_offset, leaf_dirs_length) = read_pmtiles_header(output_path);

    eprintln!(
        "Integration test: root_dir_length={}, leaf_dirs_offset={}, leaf_dirs_length={}",
        root_dir_length, leaf_dirs_offset, leaf_dirs_length
    );

    // CRITICAL: Root directory must fit in initial fetch
    assert!(
        root_dir_length <= MAX_ROOT_DIR_SIZE,
        "Root directory ({} bytes) exceeds maximum ({} bytes) - pmtiles-js will fail!",
        root_dir_length,
        MAX_ROOT_DIR_SIZE
    );

    // Verify with pmtiles CLI if available
    assert!(
        verify_with_pmtiles_cli(output_path),
        "pmtiles verify failed"
    );

    // Clean up
    let _ = fs::remove_file(output_path);
}

#[test]
fn test_root_directory_always_fits_in_16kb() {
    // Property test: generate archives of various sizes and verify root always fits
    use gpq_tiles_core::compression::Compression;
    use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
    use gpq_tiles_core::tile::TileBounds;

    let test_cases = [
        ("tiny", 10),
        ("small", 100),
        ("medium", 1000),
        ("large", 5000),
        ("very_large", 15000),
    ];

    for (name, num_tiles) in test_cases {
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        for i in 0..num_tiles {
            let x = i % 4096;
            let y = i / 4096;
            let data = vec![0x1a, (i & 0xff) as u8, ((i >> 8) & 0xff) as u8];
            writer.add_tile(12, x as u32, y as u32, &data).unwrap();
        }

        let output_path = Path::new("/tmp/test-root-size.pmtiles");
        let _ = fs::remove_file(output_path);
        writer.finalize(output_path).unwrap();

        let (root_dir_length, leaf_dirs_offset, leaf_dirs_length) =
            read_pmtiles_header(output_path);

        eprintln!(
            "{} ({} tiles): root={} bytes, leaves={} bytes",
            name, num_tiles, root_dir_length, leaf_dirs_length
        );

        assert!(
            root_dir_length <= MAX_ROOT_DIR_SIZE,
            "{}: Root directory ({} bytes) exceeds {} bytes",
            name,
            root_dir_length,
            MAX_ROOT_DIR_SIZE
        );

        let _ = fs::remove_file(output_path);
    }
}
