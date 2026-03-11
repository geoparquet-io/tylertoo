//! Integration tests for time profiling CLI flags.
//!
//! These tests verify that:
//! - `--profile` produces console timing summary
//! - `--trace-output` produces valid Chrome trace JSON
//! - Both flags can be used together

use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

/// Get path to the test fixture
fn fixture_path() -> &'static str {
    "../../tests/fixtures/streaming/multi-rowgroup-small.parquet"
}

/// Get path to the built CLI binary
fn cli_binary() -> String {
    // In tests, the binary is in target/debug/
    env!("CARGO_BIN_EXE_gpq-tiles").to_string()
}

/// Test that --profile flag produces timing summary output
#[test]
fn test_profile_flag_produces_timing_summary() {
    let fixture = fixture_path();
    if !Path::new(fixture).exists() {
        eprintln!("Skipping test: fixture not found at {}", fixture);
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_file = temp_dir.path().join("output.pmtiles");

    let output = Command::new(cli_binary())
        .args([
            fixture,
            output_file.to_str().unwrap(),
            "--profile",
            "--min-zoom",
            "0",
            "--max-zoom",
            "2",
        ])
        .output()
        .expect("Failed to execute CLI");

    // Check that command succeeded
    assert!(
        output.status.success(),
        "CLI failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Check that stderr contains profiling summary
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Profiling summary:") || stderr.contains("pipeline"),
        "Expected profiling summary in stderr, got: {}",
        stderr
    );
}

/// Test that --trace-output flag produces valid JSON file
#[test]
fn test_trace_output_produces_valid_json() {
    let fixture = fixture_path();
    if !Path::new(fixture).exists() {
        eprintln!("Skipping test: fixture not found at {}", fixture);
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_file = temp_dir.path().join("output.pmtiles");
    let trace_file = temp_dir.path().join("trace.json");

    let output = Command::new(cli_binary())
        .args([
            fixture,
            output_file.to_str().unwrap(),
            "--trace-output",
            trace_file.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "2",
        ])
        .output()
        .expect("Failed to execute CLI");

    // Check that command succeeded
    assert!(
        output.status.success(),
        "CLI failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Check that trace file was created
    assert!(
        trace_file.exists(),
        "Trace file should be created at {}",
        trace_file.display()
    );

    // Check that trace file contains valid JSON
    let trace_content = fs::read_to_string(&trace_file).expect("Failed to read trace file");
    assert!(!trace_content.is_empty(), "Trace file should not be empty");

    // Chrome trace format should be JSON array or object
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&trace_content);
    assert!(
        parsed.is_ok(),
        "Trace file should contain valid JSON, got error: {:?}",
        parsed.err()
    );

    // Chrome trace format has "traceEvents" array
    let json = parsed.unwrap();
    let has_trace_events = json.get("traceEvents").is_some() || json.is_array();
    assert!(
        has_trace_events,
        "Chrome trace should have traceEvents array or be an array"
    );
}

/// Test that both --profile and --trace-output can be used together
#[test]
fn test_combined_profile_and_trace_output() {
    let fixture = fixture_path();
    if !Path::new(fixture).exists() {
        eprintln!("Skipping test: fixture not found at {}", fixture);
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_file = temp_dir.path().join("output.pmtiles");
    let trace_file = temp_dir.path().join("trace.json");

    let output = Command::new(cli_binary())
        .args([
            fixture,
            output_file.to_str().unwrap(),
            "--profile",
            "--trace-output",
            trace_file.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "2",
        ])
        .output()
        .expect("Failed to execute CLI");

    // Check that command succeeded
    assert!(
        output.status.success(),
        "CLI failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Check that stderr contains profiling summary
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Profiling summary:") || stderr.contains("pipeline"),
        "Expected profiling summary in stderr when using --profile"
    );

    // Check that trace file was created
    assert!(
        trace_file.exists(),
        "Trace file should be created when using --trace-output"
    );

    // Check that trace file contains valid JSON
    let trace_content = fs::read_to_string(&trace_file).expect("Failed to read trace file");
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&trace_content);
    assert!(parsed.is_ok(), "Trace file should contain valid JSON");
}

/// Test that trace file contains expected span names
#[test]
fn test_trace_contains_expected_spans() {
    let fixture = fixture_path();
    if !Path::new(fixture).exists() {
        eprintln!("Skipping test: fixture not found at {}", fixture);
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_file = temp_dir.path().join("output.pmtiles");
    let trace_file = temp_dir.path().join("trace.json");

    let output = Command::new(cli_binary())
        .args([
            fixture,
            output_file.to_str().unwrap(),
            "--trace-output",
            trace_file.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "2",
        ])
        .output()
        .expect("Failed to execute CLI");

    assert!(
        output.status.success(),
        "CLI failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Read and parse trace file
    let trace_content = fs::read_to_string(&trace_file).expect("Failed to read trace file");
    let json: serde_json::Value =
        serde_json::from_str(&trace_content).expect("Failed to parse trace JSON");

    // Get trace events array
    let events = json
        .get("traceEvents")
        .and_then(|v| v.as_array())
        .or_else(|| json.as_array())
        .expect("Trace should have events array");

    // Collect all span names
    let span_names: Vec<&str> = events
        .iter()
        .filter_map(|event| event.get("name").and_then(|n| n.as_str()))
        .collect();

    // Check for expected spans (at least some of them should be present)
    let expected_spans = ["pipeline", "read_parquet"];
    for expected in expected_spans {
        assert!(
            span_names.iter().any(|name| name.contains(expected)),
            "Expected to find span '{}' in trace, found spans: {:?}",
            expected,
            span_names
        );
    }
}

/// Test that CLI runs without profiling flags (baseline)
#[test]
fn test_cli_runs_without_profiling() {
    let fixture = fixture_path();
    if !Path::new(fixture).exists() {
        eprintln!("Skipping test: fixture not found at {}", fixture);
        return;
    }

    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let output_file = temp_dir.path().join("output.pmtiles");

    let output = Command::new(cli_binary())
        .args([
            fixture,
            output_file.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "2",
        ])
        .output()
        .expect("Failed to execute CLI");

    // Check that command succeeded
    assert!(
        output.status.success(),
        "CLI failed with stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Check that output file was created
    assert!(
        output_file.exists(),
        "Output PMTiles file should be created"
    );

    // stderr should NOT contain profiling summary when --profile is not used
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("Profiling summary:"),
        "Should not show profiling summary when --profile is not used"
    );
}
