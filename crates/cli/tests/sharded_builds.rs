//! CLI surface of a sharded build (#498): the guard rails, and the flags
//! reaching core.
//!
//! The *correctness* of a sharded build is proven in
//! `crates/core/tests/shard_merge_parity.rs`, which compares a merged fleet
//! against a monolithic run tile body by tile body. What is left for here is
//! the part a library test cannot see: that a misuse of the flags fails fast,
//! with a message naming what to do instead, before any tiling happens.

use std::path::{Path, PathBuf};
use std::process::Command;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
}

/// The world-spanning grid with covering statistics — the one fixture whose
/// row groups actually prune (see the parity oracle's module docs).
fn grid() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/streaming/sharding-grid.parquet")
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(tylertoo_bin())
        .args(args)
        .output()
        .expect("run tylertoo");
    let merged = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), merged)
}

/// `tylertoo shard-plan` cuts a plan whose ranges partition the pivot zoom
/// and whose estimated rows are balanced — on a fixture whose density is
/// uniform, "balanced" is checkable without fixing exact ids.
#[test]
fn shard_plan_cuts_a_balanced_partition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = dir.path().join("shards.json");
    let (ok, out) = run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "4",
        "--pivot",
        "3",
        "-o",
        plan.to_str().unwrap(),
    ]);
    assert!(ok, "shard-plan failed: {out}");

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).expect("plan is JSON");
    assert_eq!(json["format"], "tylertoo-shard-plan");
    assert_eq!(json["pivot_zoom"], 3);
    let ranges = json["ranges"].as_array().expect("ranges");
    assert_eq!(ranges.len(), 4);

    // z3's ids are 21..=84. The ranges must tile that exactly.
    let mut expect = 21u64;
    let mut rows: Vec<u64> = Vec::new();
    for r in ranges {
        assert_eq!(
            r["lo"].as_u64().unwrap(),
            expect,
            "gap or overlap in {json}"
        );
        expect = r["hi"].as_u64().unwrap() + 1;
        rows.push(r["estimated_rows"].as_u64().unwrap());
    }
    assert_eq!(expect, 85, "the ranges must reach the end of z3");

    // Uniform density in, roughly even shards out.
    let total: u64 = rows.iter().sum();
    let mean = total as f64 / 4.0;
    for r in &rows {
        assert!(
            (*r as f64) > mean * 0.5 && (*r as f64) < mean * 1.6,
            "shard rows {rows:?} are not balanced around {mean}"
        );
    }

    // Refusing to clobber is the default: every job of a fleet must be given
    // the SAME plan, so silently re-cutting one mid-build is the bug.
    let (ok, out) = run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "4",
        "--pivot",
        "3",
        "-o",
        plan.to_str().unwrap(),
    ]);
    assert!(!ok, "a second shard-plan must not clobber: {out}");
    assert!(out.contains("--force"), "{out}");
}

/// The load-bearing guard: a data shard without a convert plan is refused,
/// naming why (the assignment is dataset-global) and the fix (run the coarse
/// job with --save-plan first).
#[test]
fn shard_without_a_convert_plan_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = dir.path().join("shards.json");
    let (ok, _) = run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "3",
        "-o",
        plan.to_str().unwrap(),
    ]);
    assert!(ok);

    let out_path = dir.path().join("shard.pmtiles");
    let (ok, out) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        out_path.to_str().unwrap(),
        "--max-zoom",
        "4",
        "--shard",
        "0/2",
        "--shard-plan",
        plan.to_str().unwrap(),
    ]);
    assert!(!ok, "--shard without --plan must be refused");
    assert!(
        out.contains("--shard requires --plan") && out.contains("dataset-global"),
        "the error must say why and how to fix it: {out}"
    );
    assert!(!out_path.exists(), "nothing must be written");
}

/// A shard would write a plan covering only its own subset, which is useless
/// to the rest of the fleet — so the pairing is refused rather than producing
/// a plan that looks usable.
#[test]
fn shard_with_save_plan_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = dir.path().join("shards.json");
    run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "3",
        "-o",
        plan.to_str().unwrap(),
    ]);
    let (ok, out) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        dir.path().join("out.pmtiles").to_str().unwrap(),
        "--max-zoom",
        "4",
        "--shard",
        "0/2",
        "--shard-plan",
        plan.to_str().unwrap(),
        "--save-plan",
        dir.path().join("convert.plan").to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(
        out.contains("--shard and --save-plan are mutually exclusive"),
        "{out}"
    );
}

/// `--shard I/N` must agree with the plan's N, and `I` must be a valid index.
/// Both are caught before the input is opened.
#[test]
fn shard_selector_is_checked_against_the_plan() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = dir.path().join("shards.json");
    run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "4",
        "--pivot",
        "3",
        "-o",
        plan.to_str().unwrap(),
    ]);
    let tiles = |shard: &str| {
        run(&[
            "tiles",
            grid().to_str().unwrap(),
            dir.path().join("out.pmtiles").to_str().unwrap(),
            "--shard",
            shard,
            "--shard-plan",
            plan.to_str().unwrap(),
        ])
    };

    let (ok, out) = tiles("0/8");
    assert!(!ok);
    assert!(out.contains("cuts 4 shards"), "{out}");

    let (ok, out) = tiles("4/4");
    assert!(!ok);
    assert!(out.contains("less than the shard count"), "{out}");

    let (ok, out) = tiles("nonsense");
    assert!(!ok);
    assert!(out.contains("is not a shard selector"), "{out}");
}

/// A shard plan cut for a different file is refused, so a fleet cannot half
/// tile one input and half another.
#[test]
fn a_shard_plan_from_another_input_is_refused() {
    let Some(other) = fixture::realdata("open-buildings.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = dir.path().join("shards.json");
    run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "3",
        "-o",
        plan.to_str().unwrap(),
    ]);
    let (ok, out) = run(&[
        "tiles",
        other.to_str().unwrap(),
        dir.path().join("out.pmtiles").to_str().unwrap(),
        "--shard",
        "0/2",
        "--shard-plan",
        plan.to_str().unwrap(),
    ]);
    assert!(!ok, "a plan cut for another input must be refused");
    assert!(
        out.contains("does not match this run") && out.contains("path"),
        "{out}"
    );
}

/// The pivot must leave the shards something to build.
#[test]
fn a_pivot_past_the_finest_zoom_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = dir.path().join("shards.json");
    run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "6",
        "-o",
        plan.to_str().unwrap(),
    ]);
    let (ok, out) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        dir.path().join("out.pmtiles").to_str().unwrap(),
        "--max-zoom",
        "4",
        "--shard",
        "coarse",
        "--shard-plan",
        plan.to_str().unwrap(),
    ]);
    assert!(!ok);
    assert!(
        out.contains("pivot z6") && out.contains("--max-zoom 4"),
        "{out}"
    );
}

/// A convert plan carrying coalesced line chains is refused under `--shard`:
/// a chain is a new geometry spanning every row it merged, which no single
/// row group's bbox bounds, so one shard would emit tiles past its range
/// while its neighbour emitted none — a gap at the seam.
#[test]
fn shard_with_coalesced_lines_in_the_plan_is_refused() {
    let Some(lines) = fixture::realdata("road-detections.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let shard_plan = dir.path().join("shards.json");
    let convert_plan = dir.path().join("convert.plan");
    // Pivot z6: this fixture's features are only visible from z5, so a
    // coarser pivot would leave the coarse job with no level to export and
    // the test would fail for a reason that has nothing to do with
    // coalescing.
    let (ok, out) = run(&[
        "shard-plan",
        lines.to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "6",
        "-o",
        shard_plan.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");

    // Coalescing on (the default), so the plan carries chains. Written by the
    // COARSE JOB of this fleet, which is the only run whose plan a shard will
    // accept: the shard plan's cut digest is part of the convert plan's
    // fingerprint (#498), so a plan saved by an unsharded run is refused
    // before the coalescing rule is ever reached.
    let (ok, out) = run(&[
        "tiles",
        lines.to_str().unwrap(),
        dir.path().join("coarse.pmtiles").to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "6",
        "--shard",
        "coarse",
        "--shard-plan",
        shard_plan.to_str().unwrap(),
        "--save-plan",
        convert_plan.to_str().unwrap(),
    ]);
    assert!(ok, "the plan-writing coarse job must succeed: {out}");

    let (ok, out) = run(&[
        "tiles",
        lines.to_str().unwrap(),
        dir.path().join("shard.pmtiles").to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "6",
        "--shard",
        "0/2",
        "--shard-plan",
        shard_plan.to_str().unwrap(),
        "--plan",
        convert_plan.to_str().unwrap(),
    ]);
    assert!(!ok, "a shard over a coalesced plan must be refused: {out}");
    assert!(
        out.contains("not supported with line coalescing") && out.contains("--no-coalesce-lines"),
        "the error must name the flag that fixes it: {out}"
    );
}

/// #541: `--shard coarse` caps the CONVERT at the pivot, not just the export.
///
/// The parity oracle proves the tiles are unchanged; what only a CLI test can
/// say is that the flag reaches core at all — that the coarse job's pass 2
/// really does build fewer levels than the same command without `--shard`,
/// and that the plan it writes is still one a data shard accepts.
#[test]
fn a_coarse_job_builds_only_the_levels_below_the_pivot() {
    /// "... → N rows across L levels in ..." from `tiles --verbose`.
    fn levels_built(out: &str) -> usize {
        out.split_whitespace()
            .zip(out.split_whitespace().skip(1))
            .find(|(_, w)| *w == "levels")
            .and_then(|(n, _)| n.parse().ok())
            .unwrap_or_else(|| panic!("no level count in: {out}"))
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let shard_plan = dir.path().join("shards.json");
    let convert_plan = dir.path().join("convert.plan");
    let (ok, out) = run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "3",
        "-o",
        shard_plan.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");

    // The control: the same pyramid, no shard role.
    let (ok, whole) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        dir.path().join("whole.pmtiles").to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "5",
        "--verbose",
    ]);
    assert!(ok, "{whole}");

    let (ok, coarse) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        dir.path().join("coarse.pmtiles").to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "5",
        "--shard",
        "coarse",
        "--shard-plan",
        shard_plan.to_str().unwrap(),
        "--save-plan",
        convert_plan.to_str().unwrap(),
        "--verbose",
    ]);
    assert!(ok, "the coarse job must succeed: {coarse}");

    let (built, control) = (levels_built(&coarse), levels_built(&whole));
    assert!(
        built < control,
        "a coarse job at pivot z3 must build fewer than the {control} levels a whole \
         run builds, got {built}:\n{coarse}"
    );
    // It owns z0..z2, and cannot build more levels than that.
    assert!(built <= 3, "pivot z3 leaves at most 3 levels, got {built}");

    // And the artifact is still the fleet's: a data shard takes it.
    let (ok, out) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        dir.path().join("shard0.pmtiles").to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "5",
        "--shard",
        "0/2",
        "--shard-plan",
        shard_plan.to_str().unwrap(),
        "--plan",
        convert_plan.to_str().unwrap(),
    ]);
    assert!(
        ok,
        "a shard must accept the plan a level-capped coarse job wrote: {out}"
    );
}

/// #541 review (S2-2): a coarse job whose levels below the pivot all come out
/// empty — every feature first appears at or past the pivot — succeeds with
/// an empty archive, exactly like an empty data shard, and still writes the
/// convert plan the shards need.
///
/// Before the fix its convert-side ceiling left nothing to build and the job
/// failed with a misleading "empty input or all features dropped", after
/// `--save-plan` had already written a perfectly good plan.
#[test]
fn a_coarse_job_with_nothing_below_the_pivot_writes_an_empty_archive() {
    // The road detections are only visible from z5, so a pivot of z3 leaves
    // the coarse job's z0..z2 with nothing in them.
    let Some(lines) = fixture::realdata("road-detections.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let shard_plan = dir.path().join("shards.json");
    let convert_plan = dir.path().join("convert.plan");
    let coarse = dir.path().join("coarse.pmtiles");
    let (ok, out) = run(&[
        "shard-plan",
        lines.to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "3",
        "-o",
        shard_plan.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");

    let (ok, out) = run(&[
        "tiles",
        lines.to_str().unwrap(),
        coarse.to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "6",
        "--shard",
        "coarse",
        "--shard-plan",
        shard_plan.to_str().unwrap(),
        "--save-plan",
        convert_plan.to_str().unwrap(),
    ]);
    assert!(ok, "an empty coarse job must succeed: {out}");
    assert!(out.contains("empty archive"), "{out}");
    assert!(
        convert_plan.exists(),
        "the plan must still be written: {out}"
    );
    let idx = tylertoo_core::archive_index::ArchiveIndex::open(&coarse).expect("valid archive");
    assert_eq!(
        idx.tiles().count(),
        0,
        "the coarse archive must hold no tile"
    );
}

/// #541 review (S3b): `--shard coarse --no-streaming` is not an error about
/// a "convert-side zoom ceiling" the user never asked for. The in-memory
/// path cannot cap its levels, so the coarse job falls back to building all
/// of them — same tiles, more work.
#[test]
fn a_coarse_job_without_streaming_falls_back_to_the_uncapped_convert() {
    let dir = tempfile::tempdir().expect("tempdir");
    let shard_plan = dir.path().join("shards.json");
    let (ok, out) = run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "3",
        "-o",
        shard_plan.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");
    let (ok, out) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        dir.path().join("coarse.pmtiles").to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "5",
        "--shard",
        "coarse",
        "--shard-plan",
        shard_plan.to_str().unwrap(),
        "--no-streaming",
    ]);
    assert!(ok, "--shard coarse --no-streaming must succeed: {out}");
    assert!(!out.contains("zoom ceiling"), "{out}");
}

/// `export-pmtiles --tile-range LO..HI` is the manual form of a shard's
/// restriction: two tile ids at one zoom, which then own every descendant.
///
/// Deliberately NOT re-deriving the tile arithmetic from the summary lines:
/// `shard_merge_parity.rs` already proves the halves partition the whole,
/// tile body by tile body, against a monolithic control. Parsing stdout to
/// re-prove it here bought a second, weaker copy of that oracle and a
/// dependence on the exact shape of a human-readable line. What is left is
/// what only a CLI test can say: the flag reaches core, the restriction
/// actually restricts, and a malformed range is refused.
#[test]
fn export_tile_range_restricts_and_rejects_mixed_zooms() {
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = dir.path().join("ov.parquet");
    let (ok, out) = run(&[
        "overview",
        grid().to_str().unwrap(),
        overview.to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "4",
    ]);
    assert!(ok, "{out}");

    let whole = dir.path().join("whole-export.pmtiles");
    let (ok, out) = run(&[
        "export-pmtiles",
        overview.to_str().unwrap(),
        whole.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");

    // z2's ids are 5..=20; take the first half. It must emit nothing above
    // its pivot, and strictly fewer tiles than the unrestricted control.
    let part = dir.path().join("part.pmtiles");
    let (ok, out) = run(&[
        "export-pmtiles",
        overview.to_str().unwrap(),
        part.to_str().unwrap(),
        "--tile-range",
        "5..12",
    ]);
    assert!(ok, "{out}");
    for (z, n) in per_zoom(&out) {
        assert!(
            z >= 2 || n == 0,
            "a range whose pivot is z2 must emit nothing at z{z}, got {n}: {out}"
        );
    }
    assert!(
        part.metadata().unwrap().len() < whole.metadata().unwrap().len(),
        "a restricted export must be smaller than the whole"
    );

    // Two ids at different zooms is not a range.
    let (ok, out) = run(&[
        "export-pmtiles",
        overview.to_str().unwrap(),
        dir.path().join("bad.pmtiles").to_str().unwrap(),
        "--tile-range",
        "5..21",
    ]);
    assert!(!ok);
    assert!(out.contains("same zoom"), "{out}");
}

/// `--zoom-ceiling` is the coarse half's complement, and is named a ceiling
/// because — unlike `--min-zoom`, which only widens what `vector_layers`
/// declares — it decides which zooms are actually emitted.
#[test]
fn export_zoom_ceiling_emits_only_the_coarse_half() {
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = dir.path().join("ov.parquet");
    let (ok, out) = run(&[
        "overview",
        grid().to_str().unwrap(),
        overview.to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "4",
    ]);
    assert!(ok, "{out}");

    let (ok, out) = run(&[
        "export-pmtiles",
        overview.to_str().unwrap(),
        dir.path().join("coarse.pmtiles").to_str().unwrap(),
        "--zoom-ceiling",
        "1",
    ]);
    assert!(ok, "{out}");
    for (z, n) in per_zoom(&out) {
        assert!(
            z <= 1 || n == 0,
            "z{z} is past the ceiling but holds {n}: {out}"
        );
    }

    // A ceiling below the file's coarsest level leaves nothing to emit, and
    // says so by name rather than writing an archive with no tiles in it.
    let (ok, out) = run(&[
        "export-pmtiles",
        overview.to_str().unwrap(),
        dir.path().join("none.pmtiles").to_str().unwrap(),
        "--tile-range",
        "5..12",
        "--zoom-ceiling",
        "1",
    ]);
    assert!(!ok, "a z2 range under a z1 ceiling emits nothing: {out}");
    assert!(out.contains("would emit no zoom at all"), "{out}");
}

/// A `--tile-buffer` wider than a shard's read-pruning margin is refused.
///
/// A shard prunes its input to the row groups within two pivot tiles of its
/// range; a buffer wider than that could pull geometry into one of its tiles
/// from a row group it never read, and the tile would come out missing
/// geometry the monolithic run has. Silently wrong is the failure mode this
/// prevents, so it is an error, not a warning.
#[test]
fn a_tile_buffer_wider_than_the_shard_margin_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = dir.path().join("ov.parquet");
    let (ok, out) = run(&[
        "overview",
        grid().to_str().unwrap(),
        overview.to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "4",
    ]);
    assert!(ok, "{out}");

    let (ok, out) = run(&[
        "export-pmtiles",
        overview.to_str().unwrap(),
        dir.path().join("wide.pmtiles").to_str().unwrap(),
        "--tile-range",
        "5..12",
        "--tile-buffer",
        "513",
    ]);
    assert!(!ok, "a 513px buffer must be refused under --tile-range");
    assert!(
        out.contains("too wide for a sharded build") && out.contains("512"),
        "{out}"
    );

    // Exactly at the bound is fine, and so is any buffer without a range.
    let (ok, out) = run(&[
        "export-pmtiles",
        overview.to_str().unwrap(),
        dir.path().join("at-bound.pmtiles").to_str().unwrap(),
        "--tile-range",
        "5..12",
        "--tile-buffer",
        "512",
    ]);
    assert!(ok, "512 is the bound, not past it: {out}");
    let (ok, out) = run(&[
        "export-pmtiles",
        overview.to_str().unwrap(),
        dir.path().join("unsharded.pmtiles").to_str().unwrap(),
        "--tile-buffer",
        "600",
    ]);
    assert!(ok, "the bound only applies to a sharded export: {out}");
}

/// Per-zoom `(zoom, tile_count)` from an `export-pmtiles` summary.
fn per_zoom(stdout: &str) -> Vec<(u8, usize)> {
    stdout
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix('z')?;
            let (z, rest) = rest.split_once(' ')?;
            let z: u8 = z.trim().parse().ok()?;
            let n: usize = rest
                .split(':')
                .nth(1)?
                .trim()
                .split(' ')
                .next()?
                .parse()
                .ok()?;
            Some((z, n))
        })
        .collect()
}

/// Cut a shard plan for the grid fixture, returning its path.
fn cut_plan(dir: &Path, shards: &str, pivot: &str) -> PathBuf {
    let plan = dir.join(format!("shards-{shards}-{pivot}.json"));
    let (ok, out) = run(&[
        "shard-plan",
        grid().to_str().unwrap(),
        "--shards",
        shards,
        "--pivot",
        pivot,
        "-o",
        plan.to_str().unwrap(),
    ]);
    assert!(ok, "shard-plan failed: {out}");
    plan
}

/// The coarse job owns `[--min-zoom, pivot - 1]`. A pivot at or below the
/// requested minimum leaves it nothing to build — and without a fail-fast the
/// whole convert (hours, on the inputs this feature exists for) runs before
/// the export refuses an empty restriction.
#[test]
fn a_coarse_job_with_no_zoom_to_build_fails_before_converting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = cut_plan(dir.path(), "2", "3");
    let out_path = dir.path().join("coarse.pmtiles");
    let (ok, out) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        out_path.to_str().unwrap(),
        "--min-zoom",
        "3",
        "--max-zoom",
        "5",
        "--shard",
        "coarse",
        "--shard-plan",
        plan.to_str().unwrap(),
        "--save-plan",
        dir.path().join("convert.plan").to_str().unwrap(),
    ]);
    assert!(!ok, "a coarse job owning no zoom must be refused: {out}");
    assert!(
        out.contains("has no zoom to build") && out.contains("pivot is z3"),
        "{out}"
    );
    assert!(!out_path.exists(), "nothing must be written");
    assert!(
        !dir.path().join("convert.plan").exists(),
        "and no convert plan either"
    );
}

/// `shard-plan` resolves its input exactly as `tiles` does, `--files-from`
/// manifests included — otherwise a fleet reading a manifest could not be
/// planned at all, and the plan's per-part input binding would have nothing
/// to bind to.
#[test]
fn shard_plan_accepts_a_files_from_manifest_and_binds_to_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let manifest = dir.path().join("parts.txt");
    std::fs::write(
        &manifest,
        format!("# one part\n{}\n", grid().to_str().unwrap()),
    )
    .unwrap();

    let plan = dir.path().join("shards.json");
    let (ok, out) = run(&[
        "shard-plan",
        "--files-from",
        manifest.to_str().unwrap(),
        "--shards",
        "2",
        "--pivot",
        "3",
        "-o",
        plan.to_str().unwrap(),
    ]);
    assert!(ok, "a manifest-driven shard-plan must work: {out}");
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&plan).unwrap()).unwrap();
    assert_eq!(json["inputs"].as_array().unwrap().len(), 1, "{json}");

    // And the plan the manifest produced is accepted by a `tiles --shard`
    // run reading the same manifest: the round trip is what makes a
    // multi-part fleet possible at all.
    let (ok, out) = run(&[
        "tiles",
        "--files-from",
        manifest.to_str().unwrap(),
        dir.path().join("out.pmtiles").to_str().unwrap(),
        "--max-zoom",
        "4",
        "--shard",
        "0/2",
        "--shard-plan",
        plan.to_str().unwrap(),
    ]);
    // It gets past every plan check and stops at the one remaining
    // requirement, which is what "the plan was accepted" looks like here.
    assert!(!ok);
    assert!(
        out.contains("--shard requires --plan"),
        "the manifest plan must be accepted, leaving only the --plan rule: {out}"
    );
}

/// A fleet is bound to ONE cut. The coarse job stamps the shard plan's cut
/// digest into the convert plan's fingerprint, so a shard handed a
/// differently-cut `shards.json` is refused by name instead of quietly
/// building tiles that overlap its siblings' and leave holes elsewhere.
#[test]
fn a_shard_given_a_different_cut_than_the_plan_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let two = cut_plan(dir.path(), "2", "3");
    let four = cut_plan(dir.path(), "4", "3");
    let convert_plan = dir.path().join("convert.plan");

    // The coarse job, run against the two-way cut.
    let (ok, out) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        dir.path().join("coarse.pmtiles").to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "4",
        "--no-coalesce-lines",
        "--shard",
        "coarse",
        "--shard-plan",
        two.to_str().unwrap(),
        "--save-plan",
        convert_plan.to_str().unwrap(),
    ]);
    assert!(ok, "the coarse job must succeed: {out}");

    // A shard of the FOUR-way cut, handed that plan: same input, same
    // options, different cut.
    let (ok, out) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        dir.path().join("wrong.pmtiles").to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "4",
        "--no-coalesce-lines",
        "--shard",
        "0/4",
        "--shard-plan",
        four.to_str().unwrap(),
        "--plan",
        convert_plan.to_str().unwrap(),
    ]);
    assert!(!ok, "a re-cut shard plan must be refused: {out}");
    assert!(
        out.contains("not the one the convert plan was saved with"),
        "the error must name the mismatch: {out}"
    );

    // The matching cut is accepted, and the shard builds.
    let shard0 = dir.path().join("shard-0.pmtiles");
    let (ok, out) = run(&[
        "tiles",
        grid().to_str().unwrap(),
        shard0.to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "4",
        "--no-coalesce-lines",
        "--shard",
        "0/2",
        "--shard-plan",
        two.to_str().unwrap(),
        "--plan",
        convert_plan.to_str().unwrap(),
    ]);
    assert!(ok, "the matching cut must build: {out}");
    assert!(shard0.exists());
}

/// A shard whose range owns no input rows **succeeds**, writing a valid empty
/// archive.
///
/// `shard-plan` cuts N ranges whatever the data looks like — an empty RANGE
/// is legal, a gap is not — so a `--bbox`-narrowed (or simply concentrated)
/// dataset routinely leaves some shards with nothing. Failing them would mean
/// an array job whose red squares mean "correct", and a merge input list the
/// operator has to hand-edit. The whole fleet, empty shard included, must
/// merge and verify.
#[test]
fn a_shard_that_owns_no_rows_writes_an_empty_archive_and_the_fleet_still_merges() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plan = cut_plan(dir.path(), "4", "3");
    let convert_plan = dir.path().join("convert.plan");
    // A bbox confining the build to one corner of the world, so most of the
    // four ranges own nothing at all.
    // `--bbox=` rather than `--bbox -175,...`: a leading minus is a flag to
    // clap otherwise.
    let bbox = "-175,-70,-120,-20";

    let coarse = dir.path().join("coarse.pmtiles");
    let common = |extra: &[&str], out: &Path| -> Vec<String> {
        let mut v: Vec<String> = vec![
            "tiles".into(),
            grid().to_str().unwrap().into(),
            out.to_str().unwrap().into(),
            "--min-zoom".into(),
            "0".into(),
            "--max-zoom".into(),
            "4".into(),
            format!("--bbox={bbox}"),
            "--no-coalesce-lines".into(),
            "--shard-plan".into(),
            plan.to_str().unwrap().into(),
        ];
        v.extend(extra.iter().map(|s| s.to_string()));
        v
    };
    let argv = common(
        &[
            "--shard",
            "coarse",
            "--save-plan",
            convert_plan.to_str().unwrap(),
        ],
        &coarse,
    );
    let (ok, out) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(ok, "coarse job: {out}");

    let mut archives = vec![coarse];
    let mut empties = 0usize;
    for i in 0..4 {
        let shard = dir.path().join(format!("shard-{i}.pmtiles"));
        let argv = common(
            &[
                "--shard",
                &format!("{i}/4"),
                "--plan",
                convert_plan.to_str().unwrap(),
            ],
            &shard,
        );
        let (ok, out) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(
            ok,
            "shard {i} must exit 0 even with nothing to build: {out}"
        );
        assert!(shard.exists(), "shard {i} must write an archive: {out}");
        if out.contains("owns no input rows") {
            empties += 1;
            assert!(
                out.contains("wrote an empty archive"),
                "an empty shard must say so plainly: {out}"
            );
        }
        archives.push(shard);
    }
    assert!(
        empties > 0,
        "this fixture/bbox is meant to leave at least one shard empty; if the cut changed, \
         pick a narrower bbox"
    );

    let merged = dir.path().join("merged.pmtiles");
    let mut argv: Vec<String> = vec!["merge".into(), merged.to_str().unwrap().into()];
    argv.extend(archives.iter().map(|p| p.to_str().unwrap().to_string()));
    let (ok, out) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(ok, "the fleet must merge with an empty shard in it: {out}");

    // "Valid empty archive" is not a figure of speech: the merge above opened
    // and validated every input's header and directories, and the bytes below
    // are the PMTiles v3 magic. An empty shard is a real archive that happens
    // to hold nothing, not a zero-byte placeholder.
    for a in &archives {
        let head = std::fs::read(a).expect("read archive");
        assert!(
            head.len() > 127,
            "{}: too small to be an archive",
            a.display()
        );
        assert_eq!(
            &head[..7],
            b"PMTiles",
            "{}: not a PMTiles archive",
            a.display()
        );
        assert_eq!(head[7], 3, "{}: not PMTiles v3", a.display());
    }
}
