//! #626: `--profile auto`'s mid-pass Ram→Spill downgrade, end to end.
//!
//! The input is built so the up-front estimate keeps the pass-2 sinks in RAM
//! and only the rows that arrive late make them too big: point geometry
//! (which is all the up-front estimate can see) with an empty `name` on the
//! first rows and a 4 KiB `name` on the rest. Under a small
//! `TYLERTOO_AUTO_MEM_LIMIT_BYTES` the run must start in RAM, notice the
//! heavy tail while it is being buffered, and downgrade to spill — and the
//! output must still be byte-identical to a `--profile bounded` run, because
//! the backing choice never touches output bytes.
//!
//! A subprocess test for the reason `profile_json_dump.rs` gives: both
//! `TYLERTOO_AUTO_MEM_LIMIT_BYTES` and `TYLERTOO_PROFILE_JSON` are process
//! globals, so they are set only in the child's environment.

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use arrow_array::{ArrayRef, BinaryArray, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

/// Input rows. Small enough to run in a few seconds, and read in
/// [`READ_BATCH_ROWS`]-row batches so the light head alone spans far more
/// than the 64 output batches an earlier fixed-size sample window covered.
const ROWS: usize = 40_000;
/// Leading rows with an empty `name`: light enough that the rate measured
/// over them projects the whole buffered set under the budget (an all-light
/// input measures ~50 MB against the 108 MB budget).
const LIGHT_HEAD_ROWS: usize = 16_000;
/// Bytes of `name` on every row after the head: the whole skewed pass
/// buffers ~195 MB, well past the budget.
const HEAVY_NAME_BYTES: usize = 4_096;
/// Pass-2 read batch size, in rows.
const READ_BATCH_ROWS: &str = "256";
/// `TYLERTOO_AUTO_MEM_LIMIT_BYTES` for the `auto` run; `auto` budgets 60% of
/// it for the sinks.
const MEM_LIMIT_BYTES: &str = "180000000";

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
}

/// A deterministic spread of points over the world, as WKB with GeoParquet
/// 1.1 `geo` metadata: an `id`, and a `name` that is empty on the first
/// `light_rows` rows and heavy on the rest.
fn write_points(path: &Path, light_rows: usize) {
    let wkb = |x: f64, y: f64| -> Vec<u8> {
        let mut out = Vec::with_capacity(21);
        out.push(1u8); // little endian
        out.extend_from_slice(&1u32.to_le_bytes()); // Point
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
        out
    };
    // A fixed LCG, so the fixture is identical on every run and platform.
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
    let heavy = "x".repeat(HEAVY_NAME_BYTES);
    let names: Vec<&str> = (0..ROWS)
        .map(|i| if i < light_rows { "" } else { heavy.as_str() })
        .collect();

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("geometry", DataType::Binary, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from_iter_values(0..ROWS as i64)) as ArrayRef,
            Arc::new(StringArray::from(names)),
            Arc::new(BinaryArray::from(
                geoms.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            )),
        ],
    )
    .expect("skewed batch");
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
        .set_max_row_group_row_count(Some(4_096))
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

/// Run `overview` on `input` under `profile`, returning the output bytes and
/// the parsed `TYLERTOO_PROFILE_JSON` line.
fn run_overview(
    dir: &Path,
    input: &Path,
    profile: &str,
    mem_limit: Option<&str>,
) -> (Vec<u8>, serde_json::Value) {
    let stem = input.file_stem().unwrap().to_string_lossy();
    let out = dir.join(format!("out-{stem}-{profile}.parquet"));
    let profile_json = dir.join(format!("profile-{stem}-{profile}.jsonl"));
    let mut cmd = Command::new(tylertoo_bin());
    cmd.args([
        "overview",
        input.to_str().unwrap(),
        out.to_str().unwrap(),
        "--min-zoom",
        "0",
        "--max-zoom",
        "7",
        "--profile",
        profile,
        "--read-batch-size",
        READ_BATCH_ROWS,
    ])
    .env("TYLERTOO_PROFILE_JSON", &profile_json)
    .env_remove("TYLERTOO_AUTO_MEM_LIMIT_BYTES");
    if let Some(limit) = mem_limit {
        cmd.env("TYLERTOO_AUTO_MEM_LIMIT_BYTES", limit);
    }
    let output = cmd.output().expect("run tylertoo overview");
    assert!(
        output.status.success(),
        "overview --profile {profile} exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let line = std::fs::read_to_string(&profile_json).expect("read profile JSON");
    let value = serde_json::from_str(line.lines().next().expect("one profile line"))
        .expect("valid profile JSON");
    (std::fs::read(&out).expect("read output"), value)
}

/// A light head over a heavy tail: `auto` starts in RAM, the whole-pass
/// sink check sees the tail as it is buffered and downgrades to spill, and
/// the output matches `bounded` byte for byte. The same limit on an input
/// that is light throughout stays in RAM, so it is the tail that forces the
/// downgrade — not the head, and not the up-front estimate.
#[test]
fn auto_downgrades_to_spill_on_a_heavy_tail_and_matches_bounded() {
    let dir = tempfile::tempdir().expect("tempdir");

    let light = dir.path().join("light.parquet");
    write_points(&light, ROWS);
    let (_, light_auto) = run_overview(dir.path(), &light, "auto", Some(MEM_LIMIT_BYTES));
    assert_eq!(
        light_auto["pass2"]["sink"]["downgraded_to_spill"].as_bool(),
        Some(false),
        "a light input under the same limit must stay in RAM, or the skewed run below \
         proves nothing about the tail: {light_auto}"
    );

    let input = dir.path().join("skewed.parquet");
    write_points(&input, LIGHT_HEAD_ROWS);

    let (auto_bytes, auto) = run_overview(dir.path(), &input, "auto", Some(MEM_LIMIT_BYTES));
    let sink = &auto["pass2"]["sink"];
    let budget = sink["budget_bytes"].as_u64().expect("budget_bytes");
    assert_eq!(
        budget,
        (MEM_LIMIT_BYTES.parse::<f64>().unwrap() * 0.6) as u64,
        "the budget must come from TYLERTOO_AUTO_MEM_LIMIT_BYTES: {auto}"
    );
    assert_eq!(
        sink["downgraded_to_spill"].as_bool(),
        Some(true),
        "the heavy tail must force a mid-pass Ram→Spill downgrade: {auto}"
    );
    // The downgrade was earned by the tail, not handed out up front: the
    // bytes buffered for the whole pass really are past the budget.
    let projected = sink["projected_bytes"].as_u64().expect("projected_bytes");
    assert!(
        projected > budget,
        "the whole pass buffered {projected} B, not past the {budget} B budget — the \
         fixture no longer exercises the downgrade: {auto}"
    );

    let (bounded_bytes, bounded) = run_overview(dir.path(), &input, "bounded", None);
    assert_eq!(
        bounded["pass2"]["sink"]["downgraded_to_spill"].as_bool(),
        Some(false),
        "an explicit bounded profile is never downgraded: {bounded}"
    );
    assert!(
        auto_bytes == bounded_bytes,
        "a mid-pass downgrade must not change output bytes ({} B auto vs {} B bounded)",
        auto_bytes.len(),
        bounded_bytes.len(),
    );
}
