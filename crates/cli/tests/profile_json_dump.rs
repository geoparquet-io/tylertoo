//! `TYLERTOO_PROFILE_JSON` dump: the measurement base the perf series
//! (pass-1 parallelization, pass-2 throughput, checkpoint work) is gated on.
//!
//! This MUST be a subprocess test, not an in-process one. An earlier version
//! lived in `overview::convert::tests` and set the env var in-process with
//! `std::env::set_var`/`remove_var`. `TYLERTOO_PROFILE_JSON` is read by
//! `crate::overview::stream::write_profile_json` — but an env var is
//! process-global state, so setting it in-process is visible to every OTHER
//! `overview::convert` test running concurrently in the same `cargo test`
//! process: those tests' conversions also appended lines to the temp file (or
//! raced the `remove_var`), which is why the test passed in isolation but
//! flaked in the full parallel suite (see #502, and the pattern this test
//! follows from `thread_count_determinism.rs`, #487). Spawning the CLI as a
//! child process and setting the var only in that child's environment
//! (`Command::env`) touches no shared state at all.

use std::process::Command;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
}

#[test]
fn profile_json_written_and_parses() {
    let Some(fixture) = fixture::realdata("open-buildings.parquet") else {
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("out.pmtiles");
    let profile_json = dir.path().join("profile.jsonl");
    // `--report` writes the combined convert+export JSON report, whose
    // `convert.levels` length is the independent source of truth this test
    // checks the profile dump's `levels` array against.
    let report_json = dir.path().join("report.json");

    let output = Command::new(tylertoo_bin())
        .args([
            "tiles",
            fixture.to_str().unwrap(),
            out.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "6",
            "--report",
            report_json.to_str().unwrap(),
        ])
        // Set only in the child's environment: no in-process global state is
        // touched, so this test cannot race any other test in the suite.
        .env("TYLERTOO_PROFILE_JSON", &profile_json)
        .output()
        .expect("run tylertoo tiles");
    assert!(
        output.status.success(),
        "tiles exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // --- The independent source of truth: the --report's convert.levels. ---
    let report_contents = std::fs::read_to_string(&report_json).expect("read --report JSON output");
    let report: serde_json::Value =
        serde_json::from_str(&report_contents).expect("valid --report JSON");
    let report_level_count = report["convert"]["levels"]
        .as_array()
        .expect("report convert.levels must be an array")
        .len();

    // --- The profile dump. ---
    let contents = std::fs::read_to_string(&profile_json)
        .unwrap_or_else(|e| panic!("read TYLERTOO_PROFILE_JSON file {profile_json:?}: {e}"));
    let mut lines = contents.lines();
    let line = lines.next().expect("one JSON line must be written");
    assert!(
        lines.next().is_none(),
        "exactly one JSON object for one conversion, got: {contents:?}"
    );
    let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");

    let pass1_rows_per_sec = value["pass1"]["rows_per_sec"]
        .as_f64()
        .unwrap_or_else(|| panic!("pass1.rows_per_sec must be a number: {value}"));
    assert!(
        pass1_rows_per_sec > 0.0,
        "pass1 rows/s must be positive: {value}"
    );
    let pass2_rows_per_sec = value["pass2"]["rows_per_sec"]
        .as_f64()
        .unwrap_or_else(|| panic!("pass2.rows_per_sec must be a number: {value}"));
    assert!(
        pass2_rows_per_sec > 0.0,
        "pass2 rows/s must be positive: {value}"
    );

    let levels = value["levels"]
        .as_array()
        .unwrap_or_else(|| panic!("levels must be an array: {value}"));
    assert_eq!(
        levels.len(),
        report_level_count,
        "profile dump's per-level array length must match --report's convert.levels: {value}"
    );
    for level in levels {
        assert!(
            level["rows"].is_u64(),
            "level.rows must be a number: {value}"
        );
        assert!(
            level["spill_bytes"].is_u64(),
            "level.spill_bytes must be a number: {value}"
        );
    }
}
