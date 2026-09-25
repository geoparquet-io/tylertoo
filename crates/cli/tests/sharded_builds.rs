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

    // Coalescing on (the default), so the plan carries chains. Written by a
    // plain run rather than the coarse job: this test is about what a SHARD
    // does with such a plan, and a coarse job's own zoom range is beside the
    // point.
    let (ok, out) = run(&[
        "tiles",
        lines.to_str().unwrap(),
        dir.path().join("whole.pmtiles").to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "6",
        "--save-plan",
        convert_plan.to_str().unwrap(),
    ]);
    assert!(ok, "the plan-writing run must succeed: {out}");

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

/// `export-pmtiles --tile-range LO..HI` is the manual form of a shard's
/// restriction: two tile ids at one zoom, which then own every descendant.
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

    // z2's ids are 5..=20. Take the first half and the second half; together
    // they must account for every tile the unrestricted export emits at
    // z2..z4, and neither alone may hold a z0 or z1 tile.
    let whole = dir.path().join("whole-export.pmtiles");
    let (ok, out) = run(&[
        "export-pmtiles",
        overview.to_str().unwrap(),
        whole.to_str().unwrap(),
    ]);
    assert!(ok, "{out}");
    let whole_zooms = per_zoom(&out);
    let whole_total = count_tiles(&out);
    let above_pivot: usize = whole_zooms
        .iter()
        .filter(|(z, _)| *z < 2)
        .map(|(_, n)| n)
        .sum();
    assert!(
        above_pivot > 0,
        "the control must hold tiles above the pivot"
    );

    let mut halves = 0usize;
    for (lo, hi) in [(5u64, 12u64), (13, 20)] {
        let part = dir.path().join(format!("part-{lo}.pmtiles"));
        let (ok, out) = run(&[
            "export-pmtiles",
            overview.to_str().unwrap(),
            part.to_str().unwrap(),
            "--tile-range",
            &format!("{lo}..{hi}"),
        ]);
        assert!(ok, "{out}");
        for (z, n) in per_zoom(&out) {
            assert!(
                z >= 2 || n == 0,
                "a range whose pivot is z2 must emit nothing at z{z}, got {n}: {out}"
            );
        }
        halves += count_tiles(&out);
    }
    // The two halves partition z2 and everything below it; what is left over
    // is exactly the zooms above the pivot, which no z2 range owns.
    assert_eq!(
        halves + above_pivot,
        whole_total,
        "the two halves plus the zooms above the pivot must account for every tile"
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

/// Total tiles from an `export-pmtiles` run's summary line.
fn count_tiles(stdout: &str) -> usize {
    stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("✓ "))
        .and_then(|l| l.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no summary line in: {stdout}"))
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
