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
    //
    // #535 step 1: a one-shot `tiles` run now writes TWO JSONL lines to
    // `TYLERTOO_PROFILE_JSON` — convert's (unchanged schema, emitted the
    // instant `convert_to_overviews` finishes) followed by export's own
    // (added here). See `write_export_profile_json`'s doc and
    // `docs/PROFILING.md`'s "Two JSONL lines for one `tiles` run" section for
    // why this is two lines rather than one merged object: convert's line is
    // already on disk by the time export starts, and merging would mean
    // threading convert's report through the export call chain purely to
    // serve profiling.
    let contents = std::fs::read_to_string(&profile_json)
        .unwrap_or_else(|e| panic!("read TYLERTOO_PROFILE_JSON file {profile_json:?}: {e}"));
    let mut lines = contents.lines();
    let line = lines
        .next()
        .expect("one JSON line must be written for convert");
    let export_line = lines
        .next()
        .expect("a second JSON line must be written for export (#535 step 1)");
    assert!(
        lines.next().is_none(),
        "exactly two JSON objects (convert, export) for one `tiles` conversion, got: {contents:?}"
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
    // PRECONDITION for the guard below. The assertions that follow are sharp
    // ONLY while this fixture/zoom-range plans exactly ONE level: with two or
    // more levels the buffered engine also runs and contributes non-zero
    // stage seconds of its own, so `> 0.0` would hold even with the finest
    // level's timers dropped again (probed: all four assertions pass against
    // the pre-fix code on a 6-level run). If this trips, re-pick the fixture
    // or the zoom range to restore a single-level plan — do not relax the
    // assertions. See the fixture comment above.
    assert_eq!(
        levels.len(),
        1,
        "the #517 S1 guard below is only sharp on a ONE-level plan (see the \
         comment above): this run planned {} levels, so re-pick the fixture / \
         zoom range rather than weakening the assertions: {value}",
        levels.len()
    );

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

    // --- #533 regression guard: the phase walls must partition the run. ---
    //
    // Every entry of `phase_walls` is a DISJOINT wall-clock window of the same
    // conversion, so their sum can never exceed `total`. Before #533 the
    // pass-1 wall was an `Instant` carried all the way to the end of the run
    // and elapsed there, so `phase_walls.pass1` silently swallowed the level
    // assignment, all of pass 2 and `writer.finish()` — it was ≈ `total`, the
    // sum was ≈ 2× `total`, `pass1.rows_per_sec` was wrong by the same factor,
    // and the assignment (the single largest stage on a planet-scale run) had
    // no entry at all. The tolerance is a hair over 1.0 only to absorb the
    // handful of microseconds of bookkeeping between the phase boundaries.
    let wall = |name: &str| -> f64 {
        value["phase_walls"][name]
            .as_f64()
            .unwrap_or_else(|| panic!("phase_walls.{name} must be a number: {value}"))
    };
    let (pass1_wall, assign_wall, writer_finish_wall, total_wall) = (
        wall("pass1"),
        wall("assign"),
        wall("writer_finish"),
        wall("total"),
    );
    for (name, secs) in [
        ("pass1", pass1_wall),
        ("assign", assign_wall),
        ("pass2", pass2_wall),
        ("writer_finish", writer_finish_wall),
        ("total", total_wall),
    ] {
        assert!(
            secs >= 0.0 && secs.is_finite(),
            "phase_walls.{name} must be a finite, non-negative duration: {value}"
        );
    }
    let walls_sum = pass1_wall + assign_wall + pass2_wall + writer_finish_wall;
    assert!(
        walls_sum <= total_wall * 1.01,
        "phase_walls must be disjoint windows of the run: \
         pass1 ({pass1_wall:.6}s) + assign ({assign_wall:.6}s) + \
         pass2 ({pass2_wall:.6}s) + writer_finish ({writer_finish_wall:.6}s) \
         = {walls_sum:.6}s exceeds total ({total_wall:.6}s) — a phase wall is \
         being elapsed after its phase ended (#533): {value}"
    );
    assert!(
        pass1_wall < total_wall,
        "phase_walls.pass1 ({pass1_wall:.6}s) must be strictly less than total \
         ({total_wall:.6}s): a pass-1 wall that equals the whole run is the \
         #533 signature: {value}"
    );

    // --- #535 step 1: the export profile line. ---
    //
    // Independent source of truth: the SAME `--report`'s `export.zooms`,
    // `export.total_tiles` and `export.total_tile_features` this test already
    // parsed `report` from above.
    let export_value: serde_json::Value =
        serde_json::from_str(export_line).expect("export profile line must be valid JSON");
    let export = &export_value["export"];
    assert!(
        export.is_object(),
        "the second profile line must have an `export` object: {export_value}"
    );

    let stage = &export["stage_secs"];
    let export_stage_field = |name: &str| -> f64 {
        stage[name]
            .as_f64()
            .unwrap_or_else(|| panic!("export.stage_secs.{name} must be a number: {export_value}"))
    };
    let (band_read, clip, encode, spool_write) = (
        export_stage_field("band_read"),
        export_stage_field("clip"),
        export_stage_field("encode"),
        export_stage_field("spool_write"),
    );
    // `checkpoint` is legitimately 0.0 on a short run that never crosses
    // `CHECKPOINT_INTERVAL` (this fixture's z0..z6 export is far too fast to),
    // so it is checked for presence/type only, not included in the sum-must-
    // be-positive guard below.
    let checkpoint = export_stage_field("checkpoint");
    assert!(
        checkpoint >= 0.0,
        "export.stage_secs.checkpoint must be a non-negative number: {export_value}"
    );
    let export_stage_sum = band_read + clip + encode + spool_write;
    assert!(
        export_stage_sum > 0.0,
        "export.stage_secs (band_read+clip+encode+spool_write) must be > 0 for \
         a run that wrote {report_level_count} level(s) of tiles: {export_value}"
    );

    let report_export_zooms = report["export"]["zooms"]
        .as_array()
        .expect("report export.zooms must be an array");
    let per_zoom = export["per_zoom"]
        .as_array()
        .unwrap_or_else(|| panic!("export.per_zoom must be an array: {export_value}"));
    assert_eq!(
        per_zoom.len(),
        report_export_zooms.len(),
        "export.per_zoom length must match --report's export.zooms length: {export_value}"
    );

    let mut per_zoom_tiles_sum: u64 = 0;
    let mut per_zoom_features_sum: u64 = 0;
    for z in per_zoom {
        assert!(
            z["zoom"].is_u64(),
            "export.per_zoom[].zoom must be a number: {export_value}"
        );
        let wall_secs = z["wall_secs"].as_f64().unwrap_or_else(|| {
            panic!("export.per_zoom[].wall_secs must be a number: {export_value}")
        });
        assert!(
            wall_secs >= 0.0 && wall_secs.is_finite(),
            "export.per_zoom[].wall_secs must be finite and non-negative: {export_value}"
        );
        per_zoom_tiles_sum += z["tiles"]
            .as_u64()
            .unwrap_or_else(|| panic!("export.per_zoom[].tiles must be a number: {export_value}"));
        per_zoom_features_sum += z["features"].as_u64().unwrap_or_else(|| {
            panic!("export.per_zoom[].features must be a number: {export_value}")
        });
        assert!(
            z["bytes"].is_u64(),
            "export.per_zoom[].bytes must be a number: {export_value}"
        );
    }

    // The report's own totals are the independent source of truth these two
    // sums must agree with — same tile/feature counts, computed by the
    // profile dump's own per-level bookkeeping in `export_level` rather than
    // by `ZoomReport`'s.
    let report_total_tiles = report["export"]["total_tiles"]
        .as_u64()
        .expect("report export.total_tiles must be a number");
    let report_total_tile_features = report["export"]["total_tile_features"]
        .as_u64()
        .expect("report export.total_tile_features must be a number");
    assert_eq!(
        per_zoom_tiles_sum, report_total_tiles,
        "sum of export.per_zoom[].tiles must match --report's export.total_tiles: {export_value}"
    );
    assert_eq!(
        per_zoom_features_sum, report_total_tile_features,
        "sum of export.per_zoom[].features must match --report's \
         export.total_tile_features: {export_value}"
    );

    assert!(
        export["waves_total"].as_u64().is_some_and(|w| w >= 1),
        "export.waves_total must be a number >= 1 for a run that wrote tiles: {export_value}"
    );
    assert!(
        export["partition_wave_width"]
            .as_u64()
            .is_some_and(|w| w >= 1),
        "export.partition_wave_width must be a positive number: {export_value}"
    );
    assert!(
        export["checkpoints"].is_u64(),
        "export.checkpoints must be a number: {export_value}"
    );

    // The startup preflight probes writability with a uniquely named SIBLING
    // file and removes it again; nothing of its own may survive the run.
    let strays: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read tempdir")
        .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.starts_with(".tylertoo-profile-json-probe."))
        .collect();
    assert!(
        strays.is_empty(),
        "the TYLERTOO_PROFILE_JSON preflight must leave no probe file behind, \
         found: {strays:?}"
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

/// #517 S4 / cross-review: the startup preflight must OBSERVE the dump path,
/// never create it. The first version opened the target itself with
/// `create(true).append(true)`, so any run that then failed — or any pipeline
/// that never reaches `write_profile_json` — left a stray ZERO-BYTE
/// `profile.jsonl` behind, which reads as "a dump was written and it is
/// empty" rather than "no dump was written". Probe-and-remove (the shape
/// `--save-plan` uses, #513) fixes that.
#[test]
fn failed_run_leaves_no_zero_byte_profile_json() {
    let Some(fixture) = fixture::realdata("open-buildings.parquet") else {
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let profile_json = dir.path().join("profile.jsonl");
    // A writable profile path, but an overview output under a directory that
    // does not exist: option validation passes (so the preflight runs), the
    // conversion then fails long before the dump would be appended.
    let out = dir.path().join("no-such-dir").join("out.parquet");

    let output = Command::new(tylertoo_bin())
        .args([
            "overview",
            fixture.to_str().unwrap(),
            out.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "6",
        ])
        .env("TYLERTOO_PROFILE_JSON", &profile_json)
        .output()
        .expect("run tylertoo overview");
    assert!(
        !output.status.success(),
        "this scenario must FAIL the conversion (unwritable overview output), \
         otherwise it does not exercise the stray-file path: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        !profile_json.exists(),
        "a preflight must not create the dump file: {profile_json:?} exists \
         ({} bytes) after a failed run that never wrote a profile line",
        std::fs::metadata(&profile_json)
            .map(|m| m.len())
            .unwrap_or(0)
    );
}

/// #499: `pass2.identical_steps` — the cascade fold's Arc-sharing counters.
///
/// This needs its own run because it needs a MULTI-level ladder. The
/// `--max-zoom 6` run in `profile_json_written_and_parses` emits a single
/// level for this fixture, so `run_pass2_buffered` (and with it
/// `process_batch_cascade`, the only thing that moves these counters) never
/// executes and the dump legitimately reports `0/0`. `--max-zoom 12` emits
/// four levels, so the fold actually runs.
#[test]
fn profile_json_reports_cascade_identical_steps() {
    let Some(fixture) = fixture::realdata("open-buildings.parquet") else {
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("out.pmtiles");
    let profile_json = dir.path().join("profile.jsonl");

    let output = Command::new(tylertoo_bin())
        .args([
            "tiles",
            fixture.to_str().unwrap(),
            out.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "12",
        ])
        .env("TYLERTOO_PROFILE_JSON", &profile_json)
        .output()
        .expect("run tylertoo tiles");
    assert!(
        output.status.success(),
        "tiles exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let contents = std::fs::read_to_string(&profile_json)
        .unwrap_or_else(|e| panic!("read TYLERTOO_PROFILE_JSON file {profile_json:?}: {e}"));
    let line = contents
        .lines()
        .next()
        .expect("one JSON line must be written");
    let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");

    let level_count = value["levels"]
        .as_array()
        .expect("levels must be an array")
        .len();
    assert!(
        level_count > 1,
        "this test needs a multi-level ladder for the cascade fold to run, \
         got {level_count} level(s): {value}"
    );

    let identical = &value["pass2"]["identical_steps"];
    let shared = identical["shared"]
        .as_u64()
        .unwrap_or_else(|| panic!("pass2.identical_steps.shared must be an integer: {value}"));
    let total = identical["total"]
        .as_u64()
        .unwrap_or_else(|| panic!("pass2.identical_steps.total must be an integer: {value}"));
    let ratio = identical["ratio"]
        .as_f64()
        .unwrap_or_else(|| panic!("pass2.identical_steps.ratio must be a number: {value}"));

    assert!(
        total > 0,
        "pass2.identical_steps.total must be > 0 once the cascade fold runs: {value}"
    );
    assert!(
        shared <= total,
        "pass2.identical_steps.shared ({shared}) must not exceed total ({total}): {value}"
    );
    assert!(
        ratio.is_finite() && (0.0..=1.0).contains(&ratio),
        "pass2.identical_steps.ratio ({ratio}) must be finite in [0, 1]: {value}"
    );
    // `ratio` is the derived view of the two counters; it must agree with them
    // rather than drift as an independently-computed number.
    let expected = shared as f64 / total as f64;
    assert!(
        (ratio - expected).abs() < 1e-9,
        "pass2.identical_steps.ratio ({ratio}) must equal shared/total ({expected}): {value}"
    );
}
