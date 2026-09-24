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

    // --- #517 S1 regression guard: pass2.stage_secs must not silently omit
    // the finest level. ---
    //
    // With this exact fixture (`open-buildings.parquet`, z0..z6) the pass-2
    // plan resolves to exactly ONE overview level, so the "buffered"
    // levels-0..n-1 engine never runs (`run_pass2_levels`'s Pipelined branch
    // takes its `n == 1` arm) and every measured stage second comes from
    // `write_level_streaming` alone. That makes this fixture/zoom-range combo
    // the sharpest possible witness for the #517 bug: before the fix,
    // `write_level_streaming` built its own *local* `Pass2Timers` that never
    // reached the accumulator returned to `emit_profile_json`, so
    // `pass2.stage_secs.{read,decode,simplify,build}` were not just small but
    // *exactly* 0.0 — confirmed by re-running this exact scenario against the
    // pre-fix code. `pass2.rows` and `phase_walls.pass2`, computed
    // independently, were unaffected, which is exactly the inconsistency the
    // issue named. A plain positivity check is deterministic (no timing
    // threshold to tune, so it can't flake on a slow/loaded CI runner) and
    // would have failed 100% of the time pre-fix.
    let stage = &value["pass2"]["stage_secs"];
    let stage_field = |name: &str| -> f64 {
        stage[name]
            .as_f64()
            .unwrap_or_else(|| panic!("pass2.stage_secs.{name} must be a number: {value}"))
    };
    let (read, decode, simplify, build) = (
        stage_field("read"),
        stage_field("decode"),
        stage_field("simplify"),
        stage_field("build"),
    );
    let stage_sum = read + decode + simplify + build;
    assert!(
        stage_sum > 0.0,
        "pass2.stage_secs (read+decode+simplify+build) must be > 0 for a run \
         that processed {} row(s) — 0.0 is the exact pre-#517-fix value when \
         the (here, only) level streamed through write_level_streaming never \
         folds its timers into the dump: {value}",
        value["pass2"]["rows"]
    );
    // `decode` (Arrow take + geometry decode) and `build` (output batch
    // assembly) run unconditionally for every row this level writes,
    // verbatim or simplified — named individually because the issue's own
    // probe showed exactly these two fields pinned near-zero.
    assert!(
        decode > 0.0,
        "pass2.stage_secs.decode must be > 0 (#517 regression guard): {value}"
    );
    assert!(
        build > 0.0,
        "pass2.stage_secs.build must be > 0 (#517 regression guard): {value}"
    );

    // Loosely relate the stage split back to the independently-measured
    // pass-2 wall time (`phase_walls.pass2`): the threshold is deliberately
    // generous (1%) so it never flakes on a slow CI runner — its only job is
    // to catch a stage split that is *present* but implausibly tiny next to
    // the wall clock, the failure mode this consistency check exists for.
    let pass2_wall = value["phase_walls"]["pass2"]
        .as_f64()
        .unwrap_or_else(|| panic!("phase_walls.pass2 must be a number: {value}"));
    assert!(
        stage_sum > pass2_wall * 0.01,
        "pass2.stage_secs sum ({stage_sum:.6}s) is implausibly small next to \
         phase_walls.pass2 ({pass2_wall:.6}s) — looks like a level's timers \
         are missing from the dump: {value}"
    );
}

/// #517 S2: an unwritable `TYLERTOO_PROFILE_JSON` path must be reported
/// LOUDLY at conversion *start*, not only via the pre-existing single
/// `log::warn` at the very end of the run (verified in the issue: exit 0,
/// profiling data silently gone). A typo'd path in a multi-hour benchmark
/// sweep must not lose its data quietly.
///
/// This does not — and must not — fail the conversion: a diagnostics-only
/// knob can never gate production output, so the run still exits 0 and
/// produces the pmtiles output.
#[test]
fn unwritable_profile_json_path_warns_loudly_at_startup() {
    let Some(fixture) = fixture::realdata("open-buildings.parquet") else {
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("out.pmtiles");
    // A path whose parent directory does not exist: `OpenOptions::open` with
    // `create(true)` cannot create missing parent directories, so this is
    // unwritable the same way a typo'd path segment would be.
    let bad_profile_json = dir.path().join("does-not-exist").join("profile.jsonl");

    let output = Command::new(tylertoo_bin())
        .args([
            "tiles",
            fixture.to_str().unwrap(),
            out.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "6",
        ])
        .env("TYLERTOO_PROFILE_JSON", &bad_profile_json)
        .output()
        .expect("run tylertoo tiles");

    // The knob must never gate production output.
    assert!(
        output.status.success(),
        "an unwritable TYLERTOO_PROFILE_JSON must not fail the conversion, \
         got {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        out.exists(),
        "the pmtiles output must still be produced despite the profiling \
         knob being broken"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().collect();

    // The loud, immediate notice (S2's fix): unmistakable text, distinct from
    // the pre-existing end-of-run warning below.
    let startup_warning_line = lines
        .iter()
        .position(|l| l.contains("NOT WRITABLE"))
        .unwrap_or_else(|| {
            panic!("expected a loud 'NOT WRITABLE' warning in stderr, got:\n{stderr}")
        });

    // The pre-existing end-of-run warning (`write_profile_json`'s own
    // `open` failure) must still fire too — this test asserts the startup
    // notice is now ADDITIONAL, not a replacement.
    let end_of_run_warning_line = lines
        .iter()
        .position(|l| l.contains("TYLERTOO_PROFILE_JSON open") && l.contains("failed"))
        .unwrap_or_else(|| {
            panic!(
                "expected the pre-existing end-of-run \
                 'TYLERTOO_PROFILE_JSON open ... failed' warning in stderr \
                 too, got:\n{stderr}"
            )
        });

    // "Immediate": the startup notice must land at (or very near) the start
    // of the run, strictly before the end-of-run one — not just be present
    // somewhere in the log.
    assert!(
        startup_warning_line < end_of_run_warning_line,
        "the loud startup warning (line {startup_warning_line}) must appear \
         before the pre-existing end-of-run warning (line \
         {end_of_run_warning_line}): {stderr}"
    );
    // Loose bound on "at the start": before pass 2 begins (a mid-run stage
    // that logs its own line), not merely somewhere before the final
    // end-of-run warning.
    let pass2_start_line = lines.iter().position(|l| l.contains("[convert] pass 2:"));
    if let Some(pass2_start_line) = pass2_start_line {
        assert!(
            startup_warning_line < pass2_start_line,
            "the startup warning (line {startup_warning_line}) must precede \
             pass 2 starting (line {pass2_start_line}) to be a true startup \
             preflight, not a late-run notice: {stderr}"
        );
    }
}
