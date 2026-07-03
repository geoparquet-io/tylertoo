//! Integration test for the `tiles` facade: one-shot GeoParquet → PMTiles
//! via overview convert → export-pmtiles through a temporary file.

use std::path::Path;
use std::process::Command;

/// PMTiles v3 archives start with the 7-byte magic "PMTiles" followed by the
/// spec version byte 3.
const PMTILES_MAGIC: &[u8] = b"PMTiles\x03";

fn gpq_tiles_bin() -> &'static str {
    env!("CARGO_BIN_EXE_gpq-tiles")
}

#[test]
fn tiles_facade_produces_valid_pmtiles() {
    let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: fixture not found");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("out.pmtiles");

    let status = Command::new(gpq_tiles_bin())
        .args([
            "tiles",
            fixture.to_str().unwrap(),
            output.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "6",
        ])
        .status()
        .expect("run gpq-tiles tiles");
    assert!(status.success(), "tiles facade exited with {status}");

    let bytes = std::fs::read(&output).expect("read output pmtiles");
    assert!(
        bytes.len() > PMTILES_MAGIC.len(),
        "output PMTiles is suspiciously small ({} bytes)",
        bytes.len()
    );
    assert_eq!(
        &bytes[..PMTILES_MAGIC.len()],
        PMTILES_MAGIC,
        "output does not start with the PMTiles v3 magic"
    );

    // The intermediate overview file must not be left behind next to the
    // output (NamedTempFile drop guard).
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("overview"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary overview file leaked: {leftovers:?}"
    );
}

#[test]
fn bare_invocation_rewrites_to_tiles() {
    let fixture = Path::new("../../tests/fixtures/realdata/road-detections.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: fixture not found");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("bare.pmtiles");

    // No `tiles` subcommand: the backward-compatible bare form.
    let status = Command::new(gpq_tiles_bin())
        .args([
            fixture.to_str().unwrap(),
            output.to_str().unwrap(),
            "--max-zoom",
            "5",
        ])
        .status()
        .expect("run gpq-tiles (bare)");
    assert!(status.success(), "bare invocation exited with {status}");

    let bytes = std::fs::read(&output).expect("read output pmtiles");
    assert_eq!(&bytes[..PMTILES_MAGIC.len()], PMTILES_MAGIC);
}
