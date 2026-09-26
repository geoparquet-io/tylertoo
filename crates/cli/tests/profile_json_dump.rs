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

/// Run `tiles` on `fixture` with `--report` and `TYLERTOO_PROFILE_JSON` set,
/// and return the tempdir (kept alive so its files survive for the caller's
/// own follow-up checks), the parsed `--report` JSON, and the two parsed
/// profile JSONL lines (convert, export).
///
/// [`profile_json_written_and_parses`] runs it once and checks both lines
/// from that single subprocess run (see the module doc: this MUST stay a
/// subprocess test).
///
/// #535 step 1: a one-shot `tiles` run now writes TWO JSONL lines to
/// `TYLERTOO_PROFILE_JSON` — convert's (unchanged schema, emitted the instant
/// `convert_to_overviews` finishes) followed by export's own. See
/// `write_export_profile_json`'s doc and `docs/PROFILING.md`'s "Two JSONL
/// lines for one `tiles` run" section for why this is two lines rather than
/// one merged object: convert's line is already on disk by the time export
/// starts, and merging would mean threading convert's report through the
/// export call chain purely to serve profiling.
fn run_tiles_with_profile(
    fixture: &std::path::Path,
    min_zoom: &str,
    max_zoom: &str,
) -> (
    tempfile::TempDir,
    serde_json::Value,
    serde_json::Value,
    serde_json::Value,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("out.pmtiles");
    let profile_json = dir.path().join("profile.jsonl");
    // `--report` writes the combined convert+export JSON report, the
    // independent source of truth both callers check the profile dump
    // against.
    let report_json = dir.path().join("report.json");

    let output = Command::new(tylertoo_bin())
        .args([
            "tiles",
            fixture.to_str().unwrap(),
            out.to_str().unwrap(),
            "--min-zoom",
            min_zoom,
            "--max-zoom",
            max_zoom,
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

    let report_contents = std::fs::read_to_string(&report_json).expect("read --report JSON output");
    let report: serde_json::Value =
        serde_json::from_str(&report_contents).expect("valid --report JSON");

    let contents = std::fs::read_to_string(&profile_json)
        .unwrap_or_else(|e| panic!("read TYLERTOO_PROFILE_JSON file {profile_json:?}: {e}"));
    let mut lines = contents.lines();
    let convert_line = lines
        .next()
        .expect("one JSON line must be written for convert");
    let export_line = lines
        .next()
        .expect("a second JSON line must be written for export (#535 step 1)");
    assert!(
        lines.next().is_none(),
        "exactly two JSON objects (convert, export) for one `tiles` conversion, got: {contents:?}"
    );
    let convert: serde_json::Value = serde_json::from_str(convert_line).expect("valid JSON");
    let export: serde_json::Value =
        serde_json::from_str(export_line).expect("export profile line must be valid JSON");

    (dir, report, convert, export)
}

/// One `tiles` run, both of its profile lines: convert's
/// ([`check_convert_line`]), export's ([`check_export_line`]), and the fields
/// that pair them (#535 review).
#[test]
fn profile_json_written_and_parses() {
    let Some(fixture) = fixture::realdata("open-buildings.parquet") else {
        return;
    };

    let (dir, report, convert, export) = run_tiles_with_profile(&fixture, "0", "6");

    // --- Pairing: both lines of one `tiles` process share a `run_id` and
    // say which phase they are. ---
    assert_eq!(
        convert["phase"], "convert",
        "first line must be phase=convert: {convert}"
    );
    assert_eq!(
        export["phase"], "export",
        "second line must be phase=export: {export}"
    );
    let run_id = convert["run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("convert line must carry a string run_id: {convert}"));
    assert!(!run_id.is_empty(), "run_id must be non-empty: {convert}");
    assert_eq!(
        export["run_id"].as_str(),
        Some(run_id),
        "the convert and export lines of ONE `tiles` run must share a run_id: \
         convert={convert} export={export}"
    );
    assert!(
        export["output"]
            .as_str()
            .is_some_and(|o| o.ends_with("out.pmtiles")),
        "export line must name its output archive: {export}"
    );

    check_convert_line(dir.path(), &report, &convert);
    check_export_line(&report, &export);
}

/// The convert line of [`profile_json_written_and_parses`]'s `tiles` run.
fn check_convert_line(
    dir: &std::path::Path,
    report: &serde_json::Value,
    value: &serde_json::Value,
) {
    // --- The independent source of truth: the --report's convert.levels. ---
    let report_level_count = report["convert"]["levels"]
        .as_array()
        .expect("report convert.levels must be an array")
        .len();

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

    // The startup preflight probes writability with a uniquely named SIBLING
    // file and removes it again; nothing of its own may survive the run.
    let strays: Vec<String> = std::fs::read_dir(dir)
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

/// #535: the export profile line of [`profile_json_written_and_parses`]'s
/// `tiles` run (the second of the two JSONL lines — see
/// [`run_tiles_with_profile`]), checked against the SAME `--report`.
fn check_export_line(report: &serde_json::Value, export_value: &serde_json::Value) {
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
    // `tiles` exports a duplicating-mode overview, so every stage of the
    // per-wave path runs; the partitioning-only ones (`spill_*`) are present
    // but 0 here — see `profile_json_export_line_partitioning_mode`.
    for name in ["band_read", "decode", "clip", "encode", "spool_write"] {
        assert!(
            export_stage_field(name) > 0.0,
            "export.stage_secs.{name} must be > 0 for a run that wrote tiles: {export_value}"
        );
    }
    for name in ["spill_write", "spill_read"] {
        assert!(
            export_stage_field(name) >= 0.0,
            "export.stage_secs.{name} must be a non-negative number: {export_value}"
        );
    }
    assert_eq!(export["mode"], "duplicating", "{export_value}");
    check_export_phase_walls(export_value);
    // `checkpoint` is legitimately 0.0 on a short run that never crosses
    // `CHECKPOINT_INTERVAL` (this fixture's z0..z6 export is far too fast to),
    // so it is checked for presence/type only, not included in the sum-must-
    // be-positive guard below.
    let checkpoint = export_stage_field("checkpoint");
    assert!(
        checkpoint >= 0.0,
        "export.stage_secs.checkpoint must be a non-negative number: {export_value}"
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

    // NOT an independent recount: `per_zoom` and the report's `ZoomReport`s
    // are built from the same per-level counters in `export_level`. What this
    // pins is the plumbing — every exported zoom has an entry, and the
    // entries carry (and serialize) those counters rather than something
    // else.
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
    assert!(
        export["threads"].as_u64().is_some_and(|t| t >= 1),
        "export.threads must be a positive number: {export_value}"
    );
    assert!(
        export["peak_rss_mib"].is_null()
            || export["peak_rss_mib"].as_f64().is_some_and(|m| m > 0.0),
        "export.peak_rss_mib must be a positive number or null: {export_value}"
    );
}

/// `export.phase_walls` are disjoint wall windows of the export, so they
/// must sum to no more than `total` (and `total` must be positive). Returns
/// the walls object for mode-specific checks by the caller.
fn check_export_phase_walls(export_value: &serde_json::Value) -> &serde_json::Value {
    let walls = &export_value["export"]["phase_walls"];
    let wall = |name: &str| -> f64 {
        let secs = walls[name].as_f64().unwrap_or_else(|| {
            panic!("export.phase_walls.{name} must be a number: {export_value}")
        });
        assert!(
            secs >= 0.0 && secs.is_finite(),
            "export.phase_walls.{name} must be finite and non-negative: {export_value}"
        );
        secs
    };
    let total = wall("total");
    assert!(
        total > 0.0,
        "export.phase_walls.total must be > 0: {export_value}"
    );
    let sum = wall("scan") + wall("fill") + wall("levels") + wall("finalize");
    assert!(
        sum <= total * 1.01,
        "export.phase_walls must be disjoint windows: scan+fill+levels+finalize \
         ({sum:.6}s) exceeds total ({total:.6}s): {export_value}"
    );
    walls
}

/// #535 review: the partitioning-mode export path — the single-read fill
/// (`fill_member_store` / `fanout_batch_members`) and the per-wave drain from
/// the member store (`encode_wave_from_store`) — which `tiles` never takes
/// (it always converts in duplicating mode). Two-step by hand: `overview
/// --mode partitioning`, then a separate `export-pmtiles` process, both
/// appending to the same `TYLERTOO_PROFILE_JSON`.
#[test]
fn profile_json_export_line_partitioning_mode() {
    let Some(fixture) = fixture::realdata("open-buildings.parquet") else {
        return;
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let overview = dir.path().join("overview.parquet");
    let out = dir.path().join("out.pmtiles");
    let profile_json = dir.path().join("profile.jsonl");
    for args in [
        vec![
            "overview",
            fixture.to_str().unwrap(),
            overview.to_str().unwrap(),
            "--mode",
            "partitioning",
            "--min-zoom",
            "0",
            "--max-zoom",
            "12",
        ],
        vec![
            "export-pmtiles",
            overview.to_str().unwrap(),
            out.to_str().unwrap(),
        ],
    ] {
        let output = Command::new(tylertoo_bin())
            .args(&args)
            .env("TYLERTOO_PROFILE_JSON", &profile_json)
            .output()
            .expect("run tylertoo");
        assert!(
            output.status.success(),
            "{} exited with {}: {}",
            args[0],
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let contents = std::fs::read_to_string(&profile_json).expect("read TYLERTOO_PROFILE_JSON");
    let lines: Vec<serde_json::Value> = contents
        .lines()
        .map(|l| serde_json::from_str(l).expect("every profile line must be valid JSON"))
        .collect();
    let of_phase = |phase: &str| -> Vec<&serde_json::Value> {
        lines.iter().filter(|v| v["phase"] == phase).collect()
    };
    let (converts, exports) = (of_phase("convert"), of_phase("export"));
    assert_eq!(
        (lines.len(), converts.len(), exports.len()),
        (2, 1, 1),
        "overview + export-pmtiles must write exactly one convert and one export line: {contents}"
    );
    let export_value = exports[0];
    // Two processes, two runs: `run_id` must tell them apart.
    assert_ne!(
        converts[0]["run_id"], export_value["run_id"],
        "separate processes must get distinct run_ids: {contents}"
    );
    assert!(
        export_value["run_id"]
            .as_str()
            .is_some_and(|r| !r.is_empty()),
        "export line must carry a run_id: {export_value}"
    );

    let export = &export_value["export"];
    assert_eq!(export["mode"], "partitioning", "{export_value}");
    let stage = |name: &str| -> f64 {
        export["stage_secs"][name]
            .as_f64()
            .unwrap_or_else(|| panic!("export.stage_secs.{name} must be a number: {export_value}"))
    };
    for name in ["band_read", "decode", "clip", "encode", "spool_write"] {
        assert!(
            stage(name) > 0.0,
            "export.stage_secs.{name} must be > 0 on the partitioning path: {export_value}"
        );
    }
    assert!(stage("spill_read") >= 0.0 && stage("spill_write") >= 0.0);
    let walls = check_export_phase_walls(export_value);
    assert!(
        walls["fill"].as_f64().is_some_and(|f| f > 0.0),
        "phase_walls.fill must be > 0: the single-read fill must have run: {export_value}"
    );
    assert!(
        export["per_zoom"].as_array().is_some_and(|z| z.len() > 1),
        "this test needs a multi-level export: {export_value}"
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
