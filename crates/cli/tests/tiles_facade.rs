//! Integration test for the `tiles` facade: one-shot GeoParquet → PMTiles
//! via overview convert → export-pmtiles through a temporary file.

use std::path::Path;
use std::process::Command;

/// PMTiles v3 archives start with the 7-byte magic "PMTiles" followed by the
/// spec version byte 3.
const PMTILES_MAGIC: &[u8] = b"PMTiles\x03";

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
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

    let status = Command::new(tylertoo_bin())
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
        .expect("run tylertoo tiles");
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
    let status = Command::new(tylertoo_bin())
        .args([
            fixture.to_str().unwrap(),
            output.to_str().unwrap(),
            "--max-zoom",
            "5",
        ])
        .status()
        .expect("run tylertoo (bare)");
    assert!(status.success(), "bare invocation exited with {status}");

    let bytes = std::fs::read(&output).expect("read output pmtiles");
    assert_eq!(&bytes[..PMTILES_MAGIC.len()], PMTILES_MAGIC);
}

/// #272: `--spill-dir` parses on both convert subcommands and reaches
/// `ConvertOptions` — a nonexistent directory is rejected by core option
/// validation (which runs before any input I/O, so no fixture is needed).
#[test]
fn spill_dir_flag_reaches_convert_options() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.parquet");
    std::fs::write(&input, b"not really parquet").unwrap();

    for subcommand in ["overview", "tiles"] {
        let out = dir.path().join(format!("{subcommand}-out"));
        let output = Command::new(tylertoo_bin())
            .args([
                subcommand,
                input.to_str().unwrap(),
                out.to_str().unwrap(),
                "--spill-dir",
                "/nonexistent/tylertoo-spill-dir-272",
            ])
            .output()
            .expect("run tylertoo");
        assert!(
            !output.status.success(),
            "{subcommand}: nonexistent --spill-dir must be rejected"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("spill-dir") && stderr.contains("/nonexistent/tylertoo-spill-dir-272"),
            "{subcommand}: error should name the option and path, got: {stderr}"
        );
    }
}
