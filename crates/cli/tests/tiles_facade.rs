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

/// #314: `tiles --keep-overview PATH` retains the intermediate overview at
/// PATH, logs its path + size, and produces a PMTiles archive byte-identical
/// to running the two-step `overview` → `export-pmtiles` chain by hand.
#[test]
fn tiles_keep_overview_retains_and_matches_two_step() {
    let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
    if !fixture.exists() {
        eprintln!("Skipping: fixture not found");
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let one_step_out = dir.path().join("one-step.pmtiles");
    let kept_overview = dir.path().join("kept-overview.parquet");

    // One-shot with --keep-overview (fixed layer name so both runs match).
    let output = Command::new(tylertoo_bin())
        .args([
            "tiles",
            fixture.to_str().unwrap(),
            one_step_out.to_str().unwrap(),
            "--max-zoom",
            "5",
            "--layer-name",
            "parity",
            "--keep-overview",
            kept_overview.to_str().unwrap(),
        ])
        .output()
        .expect("run tylertoo tiles --keep-overview");
    assert!(
        output.status.success(),
        "tiles --keep-overview exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // The intermediate is retained and is a parquet file.
    let overview_bytes = std::fs::read(&kept_overview).expect("kept overview must exist");
    assert_eq!(&overview_bytes[..4], b"PAR1", "kept overview is parquet");

    // The disk cost is not silent: path + size are logged.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("intermediate overview:")
            && stdout.contains("kept-overview.parquet")
            && stdout.contains("retained"),
        "stdout should name the intermediate's path, size, and fate: {stdout}"
    );

    // Two-step by hand: overview (duplicating, same zooms) → export-pmtiles.
    let two_step_overview = dir.path().join("two-step-overview.parquet");
    let two_step_out = dir.path().join("two-step.pmtiles");
    let status = Command::new(tylertoo_bin())
        .args([
            "overview",
            fixture.to_str().unwrap(),
            two_step_overview.to_str().unwrap(),
            "--max-zoom",
            "5",
        ])
        .status()
        .expect("run tylertoo overview");
    assert!(status.success(), "overview exited with {status}");
    let status = Command::new(tylertoo_bin())
        .args([
            "export-pmtiles",
            two_step_overview.to_str().unwrap(),
            two_step_out.to_str().unwrap(),
            "--layer-name",
            "parity",
        ])
        .status()
        .expect("run tylertoo export-pmtiles");
    assert!(status.success(), "export-pmtiles exited with {status}");

    // PMTiles export is byte-deterministic, so the acceptance bar is exact
    // byte identity between the one-step and two-step outputs.
    let one_step = std::fs::read(&one_step_out).expect("read one-step pmtiles");
    let two_step = std::fs::read(&two_step_out).expect("read two-step pmtiles");
    assert_eq!(&one_step[..8], PMTILES_MAGIC);
    assert_eq!(
        one_step, two_step,
        "one-step --keep-overview PMTiles must be byte-identical to the two-step chain"
    );
}

/// #314: a nonexistent --keep-overview directory fails fast with an error
/// naming the flag, before any conversion work.
#[test]
fn tiles_keep_overview_rejects_missing_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("in.parquet");
    std::fs::write(&input, b"not really parquet").unwrap();

    let output = Command::new(tylertoo_bin())
        .args([
            "tiles",
            input.to_str().unwrap(),
            dir.path().join("out.pmtiles").to_str().unwrap(),
            "--keep-overview",
            "/nonexistent/tylertoo-314/ov.parquet",
        ])
        .output()
        .expect("run tylertoo tiles");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("keep-overview") && stderr.contains("/nonexistent/tylertoo-314"),
        "error should name the flag and path, got: {stderr}"
    );
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
