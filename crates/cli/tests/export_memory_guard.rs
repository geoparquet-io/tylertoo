//! #665: an export partition that cannot fit in available RAM on its own is
//! refused after the scan, with an error that says what to change, instead
//! of being collected until the kernel OOM-kills the run.
//!
//! `--verbatim` turns thinning off, so the z0 tile holds every point and the
//! z0 level is one partition no wave width can shrink. Under a small
//! `TYLERTOO_AUTO_MEM_LIMIT_BYTES` export must refuse it; with the probe
//! left alone the same file exports. A subprocess test because the RAM
//! probe is a process-wide cache.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use arrow_array::{ArrayRef, BinaryArray, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

const ROWS: usize = 20_000;

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
        .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new(
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
fn export_refuses_a_partition_that_cannot_fit_in_ram() {
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

    // 20k members at several hundred resident bytes each is several MB,
    // past a 1 MB probe even serially.
    let refused = dir.path().join("refused.pmtiles");
    let (ok, stderr) = export(&overview, &refused, Some("1000000"));
    assert!(!ok, "a partition past available RAM must be refused");
    for needle in [
        "overview level 0 (z0) cannot fit in memory",
        "z0/0/0",
        "--verbatim",
        "--point-thinning 4",
        "TYLERTOO_AUTO_MEM_LIMIT_BYTES",
    ] {
        assert!(
            stderr.contains(needle),
            "stderr must name {needle}:\n{stderr}"
        );
    }
    assert!(!refused.exists(), "a refused export writes no archive");

    // The same file exports when the probe is left to the machine.
    let exported = dir.path().join("exported.pmtiles");
    let (ok, stderr) = export(&overview, &exported, None);
    assert!(ok, "export must succeed with real RAM:\n{stderr}");
    assert!(exported.exists());
}
