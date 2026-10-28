//! #665: an export tile that cannot fit in memory on its own is caught after
//! the scan, with a message that says what to change, instead of being
//! collected until the kernel OOM-kills the run.
//!
//! `--verbatim` turns thinning off, so the z0 tile holds every point and no
//! wave width can shrink it. Against `MemAvailable` or a cgroup `memory.max`
//! export refuses it; core's `dense_tile_refusal_writes_nothing` covers that
//! with an injected figure. A `TYLERTOO_AUTO_MEM_LIMIT_BYTES` override is a
//! knob, not the machine's memory, so under one export warns with the same
//! message and carries on. That is what this subprocess test pins, since the
//! override is read from the environment.
#![expect(
    clippy::disallowed_methods,
    reason = "#779: trusted fixture construction"
)] // #779: test code; reads and writes its own trusted fixtures

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use arrow_array::{ArrayRef, BinaryArray, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use tylertoo_io::test_support::ArrowWriter;
use tylertoo_io::test_support::WriterProperties;

const ROWS: usize = 5_000;

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
}

/// A deterministic spread of points with an `id` and a `category`, as WKB
/// with GeoParquet 1.1 `geo` metadata.
fn write_points(path: &Path) {
    let wkb = |x: f64, y: f64| -> Vec<u8> {
        let mut out = Vec::with_capacity(21);
        out.push(1u8); // little endian
        out.extend_from_slice(&1u32.to_le_bytes()); // Point
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
        out
    };
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let geoms: Vec<Vec<u8>> = (0..ROWS)
        .map(|_| wkb(-179.0 + 358.0 * next(), -80.0 + 160.0 * next()))
        .collect();
    let ids: Vec<String> = (0..ROWS).map(|i| format!("place-{i:026}")).collect();
    let categories = ["restaurant", "park", "school", "hotel"];
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("category", DataType::Utf8, false),
        Field::new("geometry", DataType::Binary, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from_iter_values(&ids)) as ArrayRef,
            Arc::new(StringArray::from_iter_values(
                (0..ROWS).map(|i| categories[i % categories.len()]),
            )),
            Arc::new(BinaryArray::from(
                geoms.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            )),
        ],
    )
    .expect("points batch");
    let geo = serde_json::json!({
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {"geometry": {
            "encoding": "WKB",
            "geometry_types": ["Point"],
            "crs": serde_json::Value::Null,
        }},
    });
    let props = WriterProperties::builder()
        .set_key_value_metadata(Some(vec![tylertoo_io::KeyValue::new(
            "geo".to_string(),
            geo.to_string(),
        )]))
        .build();
    let file = std::fs::File::create(path).expect("create fixture");
    let mut w = ArrowWriter::try_new(file, schema, Some(props)).expect("arrow writer");
    w.write(&batch).expect("write fixture");
    w.close().expect("close fixture");
}

/// Run `export-pmtiles` on `overview`, with `mem_limit` as the RAM probe's
/// override when given. Returns (success, stderr).
fn export(overview: &Path, out: &Path, mem_limit: Option<&str>) -> (bool, String) {
    let mut cmd = Command::new(tylertoo_bin());
    cmd.args([
        "export-pmtiles",
        overview.to_str().unwrap(),
        out.to_str().unwrap(),
        "--max-tile-size",
        "500000",
    ])
    .env_remove("TYLERTOO_AUTO_MEM_LIMIT_BYTES");
    if let Some(limit) = mem_limit {
        cmd.env("TYLERTOO_AUTO_MEM_LIMIT_BYTES", limit);
    }
    let output = cmd.output().expect("run tylertoo export-pmtiles");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn export_warns_when_a_tile_exceeds_the_override() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("points.parquet");
    write_points(&input);
    let overview = dir.path().join("overview.parquet");
    let status = Command::new(tylertoo_bin())
        .args([
            "overview",
            input.to_str().unwrap(),
            overview.to_str().unwrap(),
            "--min-zoom",
            "0",
            "--max-zoom",
            "2",
            "--verbatim",
        ])
        .env_remove("TYLERTOO_AUTO_MEM_LIMIT_BYTES")
        .output()
        .expect("run tylertoo overview");
    assert!(
        status.status.success(),
        "overview failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    // 5k members at several hundred resident bytes each is a few MB,
    // past a 1 MB override even serially.
    let warned = dir.path().join("warned.pmtiles");
    let (ok, stderr) = export(&overview, &warned, Some("1000000"));
    assert!(ok, "the override only warns:\n{stderr}");
    for needle in [
        "continuing because the memory figure is the TYLERTOO_AUTO_MEM_LIMIT_BYTES override",
        "overview level 0 (z0) cannot fit in memory",
        "z0/0/0",
        "--verbatim",
        "--point-thinning 4",
        "TYLERTOO_SKIP_MEMORY_PREFLIGHT",
    ] {
        assert!(
            stderr.contains(needle),
            "stderr must name {needle}:\n{stderr}"
        );
    }
    assert!(warned.exists());

    // The same file exports when the probe is left to the machine.
    let exported = dir.path().join("exported.pmtiles");
    let (ok, stderr) = export(&overview, &exported, None);
    assert!(ok, "export must succeed with real RAM:\n{stderr}");
    assert!(
        !stderr.contains("cannot fit in memory"),
        "5k points fit in real RAM:\n{stderr}"
    );
    assert!(exported.exists());
}
