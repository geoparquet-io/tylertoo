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

/// #452: an output in a directory that does not exist used to be found only
/// when the output was created — after the whole convert (or every pyramid
/// band, or the decode's first pass). Every writer now checks first: the
/// input here does not exist either, and the output error must win.
#[test]
fn missing_output_directory_is_refused_before_the_input_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let missing_dir = dir.path().join("no-such-dir");
    let input = dir.path().join("missing-input.parquet");
    let archive = dir.path().join("missing-input.pmtiles");
    let band = format!("0-2:{}", input.display());
    let pmtiles = missing_dir.join("out.pmtiles");
    let parquet = missing_dir.join("out.parquet");
    let plan = missing_dir.join("plan.json");
    let (input, archive) = (input.to_str().unwrap(), archive.to_str().unwrap());
    let (pmtiles, parquet) = (pmtiles.to_str().unwrap(), parquet.to_str().unwrap());
    let cases: [&[&str]; 8] = [
        &["tiles", input, pmtiles],
        &[input, pmtiles], // the bare form is `tiles`
        &["overview", input, parquet],
        &["export-pmtiles", input, pmtiles],
        &["decode", archive, parquet],
        &["pyramid", pmtiles, "--band", &band],
        &["merge", pmtiles, archive, archive],
        &[
            "shard-plan",
            input,
            "-o",
            plan.to_str().unwrap(),
            "--shards",
            "2",
        ],
    ];
    for args in cases {
        let (ok, stderr) = run(args);
        assert!(!ok, "{args:?}: must fail");
        assert!(
            stderr.contains(&missing_dir.display().to_string())
                && stderr.contains("does not exist"),
            "{args:?}: must name the missing output directory, got:\n{stderr}"
        );
        assert!(
            !stderr.contains("missing-input"),
            "{args:?}: the input was looked at before the output, got:\n{stderr}"
        );
    }
    assert!(!missing_dir.exists(), "nothing may create the directory");
}

/// #452: `--report` is written when the run ends, so a report path in a
/// missing directory used to throw the finished run away. It is checked up
/// front on every subcommand that takes it.
#[test]
fn report_into_a_missing_directory_is_refused_up_front() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("missing-input.parquet");
    let archive = dir.path().join("missing-input.pmtiles");
    let report = dir.path().join("no-such-dir").join("report.json");
    let pmtiles = dir.path().join("out.pmtiles");
    let parquet = dir.path().join("out.parquet");
    let (input, archive) = (input.to_str().unwrap(), archive.to_str().unwrap());
    let (pmtiles, parquet) = (pmtiles.to_str().unwrap(), parquet.to_str().unwrap());
    let report = report.to_str().unwrap();
    let cases: [&[&str]; 5] = [
        &["tiles", input, pmtiles, "--report", report],
        &["overview", input, parquet, "--report", report],
        &["export-pmtiles", input, pmtiles, "--report", report],
        &["decode", archive, parquet, "--report", report],
        &["merge", pmtiles, archive, archive, "--report", report],
    ];
    for args in cases {
        let (ok, stderr) = run(args);
        assert!(!ok, "{args:?}: must fail");
        assert!(
            stderr.contains("--report") && stderr.contains("does not exist"),
            "{args:?}: must name --report and why, got:\n{stderr}"
        );
        assert!(
            !stderr.contains("missing-input"),
            "{args:?}: the input was looked at before --report, got:\n{stderr}"
        );
    }
}

/// #452 review: the preflight must not refuse what the writers handle. A
/// dangling symlink is a fine PMTiles output (the writer replaces the
/// link), and `--report /dev/stdout` writes through a `/proc/self/fd` link
/// that cannot be canonicalized when stdout is a pipe, as it is here. Both
/// get past the checks to the (missing) input.
#[cfg(target_os = "linux")]
#[test]
fn symlinked_outputs_and_stdout_reports_pass_the_preflight() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("missing-input.parquet");
    let link = dir.path().join("out.pmtiles");
    std::os::unix::fs::symlink(dir.path().join("gone").join("x"), &link).unwrap();
    let plain = dir.path().join("plain.pmtiles");
    let (input, link) = (input.to_str().unwrap(), link.to_str().unwrap());
    for args in [
        &["tiles", input, link, "--force"][..],
        &[
            "tiles",
            input,
            plain.to_str().unwrap(),
            "--report",
            "/dev/stdout",
        ][..],
    ] {
        let (ok, stderr) = run(args);
        assert!(!ok, "{args:?}: the missing input must still fail");
        assert!(
            stderr.contains("missing-input") && !stderr.contains("cannot write"),
            "{args:?}: must get past the output checks, got:\n{stderr}"
        );
    }
}
