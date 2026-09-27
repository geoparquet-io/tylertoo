//! Output-overwrite guards (#427, following #551 for `tiles`): every
//! subcommand that writes a file refuses an existing output unless `--force`
//! is given, and it refuses *before* any work is done — so these tests need
//! no real input. With `--force` the guard is skipped and the run proceeds
//! to its input, which is what the "wrong" error proves.

use std::process::Command;

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
}

const FORCE_HINT: &str = "exists (use --force to overwrite)";

/// Run `tylertoo <args>` and return (success, stderr).
fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(tylertoo_bin())
        .args(args)
        .output()
        .expect("run tylertoo");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `sub` with `INPUT OUTPUT` positionals: an existing OUTPUT is refused
/// without `--force`, and `--force` gets past the guard (the run then fails
/// on the bogus input instead, with a different message).
fn assert_guarded(sub: &str, existing_output_name: &str) {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join(existing_output_name);
    std::fs::write(&output, b"previous good output").unwrap();
    let input = dir.path().join("missing-input");

    let (ok, stderr) = run(&[sub, input.to_str().unwrap(), output.to_str().unwrap()]);
    assert!(!ok, "{sub}: an existing output must be refused");
    assert!(
        stderr.contains(FORCE_HINT),
        "{sub}: error must name --force, got:\n{stderr}"
    );
    assert_eq!(
        std::fs::read(&output).unwrap(),
        b"previous good output",
        "{sub}: the refused output must be untouched"
    );

    let (ok, stderr) = run(&[
        sub,
        input.to_str().unwrap(),
        output.to_str().unwrap(),
        "--force",
    ]);
    assert!(!ok, "{sub}: the bogus input must still fail");
    assert!(
        !stderr.contains(FORCE_HINT),
        "{sub}: --force must skip the guard, got:\n{stderr}"
    );
}

#[test]
fn export_pmtiles_refuses_existing_output_without_force() {
    assert_guarded("export-pmtiles", "out.pmtiles");
}

#[test]
fn decode_refuses_existing_output_without_force() {
    assert_guarded("decode", "out.parquet");
}

#[test]
fn overview_refuses_existing_output_without_force() {
    assert_guarded("overview", "out.parquet");
}

/// `tiles` already had the guard (#551); it stays.
#[test]
fn tiles_refuses_existing_output_without_force() {
    assert_guarded("tiles", "out.pmtiles");
}

/// A directory at the output path can never be replaced by the final rename,
/// `--force` or not, so it is refused up front for every writer.
#[test]
fn directory_output_is_refused_even_with_force() {
    for (sub, name, kind) in [
        ("export-pmtiles", "out.pmtiles", "PMTiles"),
        ("decode", "out.parquet", "GeoParquet"),
        ("overview", "out.parquet", "GeoParquet"),
        ("tiles", "out.pmtiles", "PMTiles"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join(name);
        std::fs::create_dir(&output).unwrap();
        let input = dir.path().join("missing-input");
        let (ok, stderr) = run(&[
            sub,
            input.to_str().unwrap(),
            output.to_str().unwrap(),
            "--force",
        ]);
        assert!(!ok, "{sub}: a directory output must be refused");
        assert!(
            stderr.contains(&format!(
                "is a directory; the output must be a {kind} file path"
            )),
            "{sub}: error must say why and name the output kind, got:\n{stderr}"
        );
    }
}

/// `--spill-dir` on `export-pmtiles` (#427) is validated before any work,
/// with the same wording as the convert side.
#[test]
fn export_pmtiles_rejects_missing_spill_dir_up_front() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("missing-input.parquet");
    let output = dir.path().join("out.pmtiles");
    let spill = dir.path().join("no-such-dir");
    let (ok, stderr) = run(&[
        "export-pmtiles",
        input.to_str().unwrap(),
        output.to_str().unwrap(),
        "--spill-dir",
        spill.to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(
        stderr.contains("spill-dir") && stderr.contains("not an existing directory"),
        "got:\n{stderr}"
    );
}
