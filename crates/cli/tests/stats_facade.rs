//! Integration test for `tylertoo stats` (#552): a per-zoom tile-weight
//! report over an existing PMTiles archive.
//!
//! Numbers are pinned against the committed golden fixture
//! `tests/fixtures/golden/road-detections.pmtiles` (z0-z10, one tile per zoom
//! through z6, two per zoom from z7). If that fixture is regenerated, these
//! expectations must be regenerated with it.

use std::process::{Command, Output};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
}

fn run_stats(extra: &[&str]) -> Output {
    let archive = fixture::golden("road-detections.pmtiles");
    let output = Command::new(tylertoo_bin())
        .arg("stats")
        .arg(&archive)
        .args(extra)
        .output()
        .expect("run tylertoo stats");
    assert!(
        output.status.success(),
        "stats {extra:?} exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_stats_json(extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["--json"];
    args.extend_from_slice(extra);
    let output = run_stats(&args);
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stats --json did not print valid JSON ({e}):\n{stdout}"))
}

/// Per-zoom `(z, tile_count, max_bytes)` of the golden fixture.
const GOLDEN_PER_ZOOM: [(u64, u64, u64); 11] = [
    (0, 1, 506),
    (1, 1, 685),
    (2, 1, 1163),
    (3, 1, 2163),
    (4, 1, 3463),
    (5, 1, 5045),
    (6, 1, 7081),
    (7, 2, 9176),
    (8, 2, 11272),
    (9, 2, 12678),
    (10, 2, 17032),
];

/// The human table: exact header, one row per zoom, and the z7 row's numbers
/// (a two-tile zoom, so p50 and max differ).
#[test]
fn stats_prints_per_zoom_table() {
    let output = run_stats(&[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    let header: Vec<&str> = lines[0].split_whitespace().collect();
    assert_eq!(
        header,
        ["z", "tiles", "total", "mean", "p50", "p99", "max"],
        "got:\n{stdout}"
    );
    let rows: Vec<Vec<&str>> = lines[1..=GOLDEN_PER_ZOOM.len()]
        .iter()
        .map(|l| l.split_whitespace().collect())
        .collect();
    for (row, &(z, count, max)) in rows.iter().zip(GOLDEN_PER_ZOOM.iter()) {
        assert_eq!(row[0], z.to_string(), "row {row:?}");
        assert_eq!(row[1], count.to_string(), "row {row:?}");
        assert_eq!(row[6].replace(',', ""), max.to_string(), "row {row:?}");
    }
    assert_eq!(
        rows[7],
        ["7", "2", "10,566", "5,283", "1,390", "9,176", "9,176"]
    );
    assert!(
        stdout.contains("Largest 10 tile(s):"),
        "default --largest is 10, got:\n{stdout}"
    );
}

/// `--json`: unit-bearing field names, exact per-zoom numbers from the
/// golden fixture, and the largest tile pinned to z/x/y/bytes.
#[test]
fn stats_json_pins_golden_numbers() {
    let json = run_stats_json(&[]);

    let per_zoom = json["per_zoom"].as_array().expect("per_zoom array");
    assert_eq!(per_zoom.len(), GOLDEN_PER_ZOOM.len());
    for (row, &(z, count, max)) in per_zoom.iter().zip(GOLDEN_PER_ZOOM.iter()) {
        assert_eq!(row["z"], z, "{row}");
        assert_eq!(row["tile_count"], count, "{row}");
        assert_eq!(row["max_bytes"], max, "{row}");
        for field in ["total_bytes", "mean_bytes", "p50_bytes", "p99_bytes"] {
            assert!(row[field].is_u64(), "missing {field:?}: {row}");
        }
    }
    assert_eq!(
        per_zoom[7],
        serde_json::json!({
            "z": 7, "tile_count": 2, "total_bytes": 10566, "mean_bytes": 5283,
            "p50_bytes": 1390, "p99_bytes": 9176, "max_bytes": 9176
        })
    );

    let largest = json["largest"].as_array().expect("largest array");
    assert_eq!(largest.len(), 10);
    assert_eq!(
        largest[0],
        serde_json::json!({"z": 10, "x": 338, "y": 472, "bytes": 17032})
    );
}

/// The human table and the JSON report agree on every per-zoom row.
#[test]
fn stats_table_and_json_agree() {
    let json = run_stats_json(&[]);
    let table = run_stats(&[]);
    let stdout = String::from_utf8_lossy(&table.stdout);
    let per_zoom = json["per_zoom"].as_array().unwrap();
    for (line, row) in stdout.lines().skip(1).zip(per_zoom) {
        let cells: Vec<u64> = line
            .split_whitespace()
            .map(|c| c.replace(',', "").parse().unwrap())
            .collect();
        let from_json: Vec<u64> = [
            "z",
            "tile_count",
            "total_bytes",
            "mean_bytes",
            "p50_bytes",
            "p99_bytes",
            "max_bytes",
        ]
        .iter()
        .map(|f| row[*f].as_u64().unwrap())
        .collect();
        assert_eq!(cells, from_json, "table line {line:?} vs json {row}");
    }
}

/// `--largest N` caps the reported largest-tiles list to exactly N entries
/// (the fixture has 15 tiles, so 2 are always available).
#[test]
fn stats_largest_flag_caps_the_list() {
    let json = run_stats_json(&["--largest", "2"]);
    let largest = json["largest"].as_array().expect("largest array");
    assert_eq!(largest.len(), 2);
    assert_eq!(largest[0]["bytes"], 17032);
    assert_eq!(largest[1]["bytes"], 12678);
}

/// A nonexistent archive is a clean error naming the path, not a panic.
#[test]
fn stats_on_missing_archive_errors_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist.pmtiles");

    let output = Command::new(tylertoo_bin())
        .args(["stats", missing.to_str().unwrap()])
        .output()
        .expect("run tylertoo stats");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not open") && stderr.contains("does-not-exist.pmtiles"),
        "got: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "got: {stderr}");
}

/// A file that is not a PMTiles archive (here, a Parquet file's magic) says
/// so, rather than surfacing only the reader's generic error.
#[test]
fn stats_on_non_pmtiles_input_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let not_pmtiles = dir.path().join("input.parquet");
    std::fs::write(&not_pmtiles, b"PAR1 not an archive at all PAR1").unwrap();

    let output = Command::new(tylertoo_bin())
        .args(["stats", not_pmtiles.to_str().unwrap()])
        .output()
        .expect("run tylertoo stats");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("is not a PMTiles v3 archive"),
        "got: {stderr}"
    );
}
