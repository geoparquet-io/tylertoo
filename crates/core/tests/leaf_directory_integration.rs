//! Integration tests for leaf directory support (Issue #88)
//!
//! These tests verify that PMTiles files with many tiles are correctly
//! structured with leaf directories to fit in the initial 16KB HTTP range request.

use std::fs;
use std::path::Path;
use std::process::Command;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

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

/// A real-data conversion must leave a root directory small enough for the
/// initial 16 KB range request, or `pmtiles-js` cannot open the archive.
///
/// Runs the convert -> export chain **in process**. It used to shell out to
/// `cargo run --release`, which was never exercised (#369: the fixture path
/// resolved under `crates/core/`, so the test always skipped). Once it began
/// running, that nested release build timed tarpaulin out — and it could
/// never have contributed coverage anyway, since tarpaulin does not
/// instrument a subprocess. In-process is faster, measurable, and tests the
/// same invariant: the CLI was only ever a way to reach this chain, as the
/// sibling property test below already assumes.
#[test]
fn test_conversion_produces_valid_leaf_directories() {
    use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

    let Some(fixture_path) = fixture::realdata("fieldmaps-madagascar-adm4.parquet") else {
        return;
    };

    let overview = tempfile::Builder::new()
        .suffix(".parquet")
        .tempfile()
        .expect("overview tempfile");
    let output = tempfile::Builder::new()
        .suffix(".pmtiles")
        .tempfile()
        .expect("output tempfile");

    convert_to_overviews(
        &fixture_path,
        overview.path(),
        &ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 10,
            },
            ..Default::default()
        },
    )
    .expect("convert");
    export_pmtiles(overview.path(), output.path(), &ExportOptions::default()).expect("export");

    let (root_dir_length, leaf_dirs_offset, leaf_dirs_length) = read_pmtiles_header(output.path());
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

    assert!(
        verify_with_pmtiles_cli(output.path()),
        "pmtiles verify failed"
    );
}

#[test]
fn test_root_directory_always_fits_in_16kb() {
    // Property test: generate archives of various sizes and verify root always fits
    use tylertoo_core::compression::Compression;
    use tylertoo_core::pmtiles_writer::StreamingPmtilesWriter;
    use tylertoo_core::tile::TileBounds;

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

        let (root_dir_length, _leaf_dirs_offset, leaf_dirs_length) =
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
