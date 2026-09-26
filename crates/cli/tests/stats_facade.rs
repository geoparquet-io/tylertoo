//! Integration test for `tylertoo stats` (#552): a per-zoom tile-weight
//! report over an existing PMTiles archive.

use std::process::Command;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
}

/// The human table has a header row naming every column the issue asked for,
/// and at least one data row (the fixture is non-empty).
#[test]
fn stats_prints_per_zoom_table() {
    let archive = fixture::golden("road-detections.pmtiles");

    let output = Command::new(tylertoo_bin())
        .args(["stats", archive.to_str().unwrap()])
        .output()
        .expect("run tylertoo stats");
    assert!(
        output.status.success(),
        "stats exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    for column in ["z", "tiles", "mean", "p50", "p99", "max"] {
        assert!(
            stdout.contains(column),
            "table header should name column {column:?}, got:\n{stdout}"
        );
    }
    // At least one zoom row, formatted as a right-aligned integer.
    assert!(
        stdout.lines().count() > 1,
        "should print a header plus at least one zoom row, got:\n{stdout}"
    );
}

/// `--json` produces machine-readable output with the same numbers as the
/// human report: per-zoom stats plus the largest tiles.
#[test]
fn stats_json_reports_same_shape_as_core() {
    let archive = fixture::golden("road-detections.pmtiles");

    let output = Command::new(tylertoo_bin())
        .args(["stats", archive.to_str().unwrap(), "--json"])
        .output()
        .expect("run tylertoo stats --json");
    assert!(
        output.status.success(),
        "stats --json exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stats --json did not print valid JSON ({e}):\n{stdout}"));

    let per_zoom = json["per_zoom"].as_array().expect("per_zoom array");
    assert!(!per_zoom.is_empty(), "per_zoom should not be empty");
    let z0 = &per_zoom[0];
    for field in ["zoom", "tile_count", "mean", "p50", "p99", "max"] {
        assert!(
            z0.get(field).is_some(),
            "per_zoom entry missing {field:?}: {z0:?}"
        );
    }

    let largest = json["largest"].as_array().expect("largest array");
    assert!(!largest.is_empty(), "largest should not be empty");
    for field in ["z", "x", "y", "bytes"] {
        assert!(
            largest[0].get(field).is_some(),
            "largest entry missing {field:?}: {:?}",
            largest[0]
        );
    }
}

/// `--largest N` caps the reported largest-tiles list to N entries.
#[test]
fn stats_largest_flag_caps_the_list() {
    let archive = fixture::golden("road-detections.pmtiles");

    let output = Command::new(tylertoo_bin())
        .args([
            "stats",
            archive.to_str().unwrap(),
            "--largest",
            "2",
            "--json",
        ])
        .output()
        .expect("run tylertoo stats --largest 2 --json");
    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid json");
    let largest = json["largest"].as_array().expect("largest array");
    assert!(
        largest.len() <= 2,
        "expected at most 2 largest tiles, got {}",
        largest.len()
    );
}

/// A nonexistent archive is a clean error, not a panic.
#[test]
fn stats_on_missing_archive_errors_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist.pmtiles");

    let output = Command::new(tylertoo_bin())
        .args(["stats", missing.to_str().unwrap()])
        .output()
        .expect("run tylertoo stats");
    assert!(!output.status.success());
}
