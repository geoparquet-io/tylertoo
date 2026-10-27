//! Core's option rules fire through the CLI (#779).
//!
//! Each rule below used to be a `bail!` or a preflight in `main.rs` (and
//! often a second copy in the Python bindings). Core owns them now
//! (`overview::config`, `TilesOptions::validate`, `PyramidOptions::validate`)
//! and the CLI only surfaces the error. Every input here is missing on
//! purpose: a rule must fire before anything is read. The Python side is
//! `crates/python/tests/test_config.py`.
//!
//! A refused option is a usage error: exit 2, as clap's own are, and the
//! message alone, with no "... failed:" prefix naming a step that never ran.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Run `tylertoo <args>` and return (exit code, stderr).
fn run_code(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tylertoo"))
        .args(args)
        .output()
        .expect("run tylertoo");
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Run `tylertoo <args>` and return (success, stderr).
fn run(args: &[&str]) -> (bool, String) {
    let (code, stderr) = run_code(args);
    (code == Some(0), stderr)
}

/// A core refusal: exit 2 and `needle`, with no step-failed prefix.
fn assert_refused(code: Option<i32>, stderr: &str, needle: &str, what: &str) {
    assert_eq!(code, Some(2), "{what}: a refused option exits 2:\n{stderr}");
    assert!(
        stderr.contains(needle),
        "{what}: expected {needle:?} in:\n{stderr}"
    );
    assert!(
        !stderr.contains("failed:"),
        "{what}: a refusal names no failed step:\n{stderr}"
    );
}

/// `tiles` with a missing input, `--max-zoom auto` (which reads the input
/// to pick a zoom) and `flags`: it must fail naming `needle` before reading
/// the input, whose absence would be the error otherwise.
fn tiles_refuses(flags: &[&str], needle: &str) {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("never-read.parquet");
    let output = dir.path().join("out.pmtiles");
    let mut argv = vec![
        "tiles",
        input.to_str().unwrap(),
        output.to_str().unwrap(),
        "--max-zoom",
        "auto",
    ];
    argv.extend_from_slice(flags);
    let (code, stderr) = run_code(&argv);
    assert_refused(code, &stderr, needle, &format!("{flags:?}"));
    assert!(
        !stderr.contains("never-read.parquet"),
        "{flags:?}: refused only after looking at the input:\n{stderr}"
    );
    assert!(!output.exists());
}

fn grid() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/streaming/sharding-grid.parquet")
}

#[test]
fn tiles_surfaces_every_core_rule() {
    for (flags, needle) in [
        // #433 (1e37cbe): the export knobs that need no file.
        (&["--tile-buffer", "257"][..], "--tile-buffer 257"),
        (&["--extent", "0"][..], "extent 0"),
        // #427 (492502f): the spill directory.
        (
            &["--spill-dir", "/nonexistent/tylertoo-779"][..],
            "not an existing directory",
        ),
        // #428 (b8fa58a): a NaN class rank.
        (&["--class-rank", "c:a=nan"][..], "finite"),
        // Q1: formerly a `bail!` "(also enforced in core)".
        (
            &["--class-rank", "c:a=1", "--sort-key", "pop"][..],
            "mutually exclusive",
        ),
        // Q4: formerly a `bail!` "(also enforced in core)".
        (&["--accumulate-attribute", "pop:sum"][..], "--cluster"),
        // #386/#443: the feature id excluded by the property selection.
        (
            &["--exclude-property", "id", "--feature-id", "id"][..],
            "--feature-id",
        ),
        // #433: the kept overview's directory.
        (
            &["--keep-overview", "/nonexistent/tylertoo-779/o.parquet"][..],
            "does not exist",
        ),
    ] {
        tiles_refuses(flags, needle);
    }
}

/// `overview --mode partitioning` with the flags no partitioning run can
/// honour: formerly three `bail!`s in `build_convert_options`.
#[test]
fn overview_surfaces_the_mode_rules() {
    for (flags, needle) in [
        (&["--verbatim"][..], "verbatim requires duplicating mode"),
        (&["--cluster"][..], "--cluster requires duplicating mode"),
        (&["--coalesce-lines"][..], "--coalesce-lines requires"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("never-read.parquet");
        let output = dir.path().join("o.parquet");
        let mut argv = vec![
            "overview",
            input.to_str().unwrap(),
            output.to_str().unwrap(),
            "--mode",
            "partitioning",
        ];
        argv.extend_from_slice(flags);
        let (ok, stderr) = run(&argv);
        assert!(!ok, "{flags:?} must be refused");
        assert!(stderr.contains(needle), "{flags:?}: {stderr}");
    }
}

/// `export-pmtiles` and `pyramid` surface the same export rules, from the
/// one `ExportArgs` they share with `tiles`.
#[test]
fn export_and_pyramid_surface_the_export_rules() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("never-read.parquet");
    let output = dir.path().join("out.pmtiles");
    let (ok, stderr) = run(&[
        "export-pmtiles",
        input.to_str().unwrap(),
        output.to_str().unwrap(),
        "--spill-dir",
        "/nonexistent/tylertoo-779",
    ]);
    assert!(!ok);
    assert!(stderr.contains("not an existing directory"), "{stderr}");

    let band = dir.path().join("band.parquet");
    std::fs::write(&band, b"never read").unwrap();
    let (ok, stderr) = run(&[
        "pyramid",
        output.to_str().unwrap(),
        "--band",
        &format!("0-2:{}", band.display()),
        "--tile-buffer",
        "999",
    ]);
    assert!(!ok);
    assert!(stderr.contains("--tile-buffer 999"), "{stderr}");
    assert!(!output.exists());
}

/// The output preflight is one core rule for every command (#427, #551).
#[test]
fn every_writer_refuses_a_directory_output() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("out");
    std::fs::create_dir(&out_dir).unwrap();
    let band = dir.path().join("band.parquet");
    std::fs::write(&band, b"never read").unwrap();
    let out = out_dir.to_str().unwrap();
    let band_spec = format!("0-2:{}", band.display());
    for argv in [
        vec!["tiles", "in.parquet", out],
        vec!["export-pmtiles", "in.parquet", out],
        vec!["pyramid", out, "--band", &band_spec],
        vec!["merge", out, "a.pmtiles", "b.pmtiles"],
    ] {
        let (ok, stderr) = run(&argv);
        assert!(!ok, "{argv:?}");
        assert!(
            stderr.contains("is a directory; the output must be a PMTiles file path"),
            "{argv:?}: {stderr}"
        );
    }
}

/// The exclusions clap used to own (`conflicts_with` / `requires`) are
/// core's now, so the CLI and Python refuse them with one message (#779).
#[test]
fn the_former_clap_exclusions_are_core_rules() {
    let dir = tempfile::tempdir().unwrap();
    let saved = dir.path().join("saved.plan");
    let reused = dir.path().join("reused.plan");
    for (flags, needle) in [
        (
            &["--magnitude-ladder", "pop", "--entry-zoom", "pop:1=2"][..],
            "--magnitude-ladder and --entry-zoom are mutually exclusive",
        ),
        (
            &["--collapse", "--no-collapse"][..],
            "--collapse, --collapse-square and --no-collapse are mutually exclusive",
        ),
        (
            &["--collapse", "--collapse-square"][..],
            "--collapse, --collapse-square and --no-collapse are mutually exclusive",
        ),
        (
            &["--collapse-square", "--no-collapse"][..],
            "--collapse, --collapse-square and --no-collapse are mutually exclusive",
        ),
        (
            &[
                "--save-plan",
                saved.to_str().unwrap(),
                "--plan",
                reused.to_str().unwrap(),
            ][..],
            "--save-plan and --plan are mutually exclusive",
        ),
    ] {
        tiles_refuses(flags, needle);
    }

    // --plan-only without --save-plan (it takes no OUTPUT).
    let (code, stderr) = run_code(&[
        "tiles",
        dir.path().join("never-read.parquet").to_str().unwrap(),
        "--plan-only",
    ]);
    assert_refused(
        code,
        &stderr,
        "--plan-only requires --save-plan",
        "--plan-only",
    );
}

/// `--tile-range` with any shard job: core's rule, which needs the shard
/// plan to know the job, so this one reads the (small) plan file.
#[test]
fn tile_range_with_a_shard_is_a_core_rule() {
    let dir = tempfile::tempdir().unwrap();
    let plan = dir.path().join("shards.json");
    let (ok, stderr) = run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "3",
        "-o",
        plan.to_str().unwrap(),
    ]);
    assert!(ok, "shard-plan failed: {stderr}");
    let output = dir.path().join("out.pmtiles");
    for role in ["0/2", "coarse"] {
        let (code, stderr) = run_code(&[
            "tiles",
            grid().to_str().unwrap(),
            output.to_str().unwrap(),
            "--shard",
            role,
            "--shard-plan",
            plan.to_str().unwrap(),
            "--tile-range",
            "5..12",
        ]);
        assert_refused(
            code,
            &stderr,
            "--tile-range cannot be combined with --shard",
            role,
        );
        assert!(!output.exists());
    }
}
