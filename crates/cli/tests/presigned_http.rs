//! #820: a presigned-style `http(s)` input (a signature in the query
//! string) tiles end to end, and the signature never leaves the request: not
//! in stdout/stderr (at debug logging), the `--report` JSON, a saved convert
//! plan, or a shard plan.

use std::path::Path;
use std::process::Command;

use tylertoo_io::test_support::serve_bytes_over_http;

const SIGNATURE: &str = "SIG820SECRET";

/// The sharding grid, served by a host that 403s any request whose query is
/// not `X-Amz-Signature=…&X-Amz-Expires=60`; returns the presigned URL.
fn presigned_grid() -> String {
    let grid = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/streaming/sharding-grid.parquet");
    let query = format!("X-Amz-Signature={SIGNATURE}&X-Amz-Expires=60");
    let base = serve_bytes_over_http(std::fs::read(grid).unwrap(), Some(&query));
    format!("{base}/grid.parquet?{query}")
}

/// Run tylertoo at debug logging; `(success, stdout + stderr)`.
fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tylertoo"))
        .args(args)
        .env("RUST_LOG", "debug")
        .output()
        .expect("run tylertoo");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn assert_no_signature(what: &str, text: &str) {
    assert!(
        !text.contains(SIGNATURE),
        "{what} leaks the signature:\n{text}"
    );
}

#[test]
fn tiles_reads_a_presigned_url_without_leaking_it() {
    let url = presigned_grid();
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();

    let (ok, out) = run(&[
        "tiles",
        &url,
        &path("out.pmtiles"),
        "--max-zoom",
        "4",
        "--report",
        &path("report.json"),
        "--save-plan",
        &path("convert.plan"),
    ]);
    assert!(ok, "tiles failed: {out}");
    assert_no_signature("tiles output", &out);
    for file in ["report.json", "convert.plan"] {
        // The plan is binary; its input paths are plain strings inside it.
        let text =
            String::from_utf8_lossy(&std::fs::read(dir.path().join(file)).unwrap()).into_owned();
        assert_no_signature(file, &text);
        assert!(
            text.contains("grid.parquet"),
            "{file} names the input: {text}"
        );
    }
}

#[test]
fn shard_plan_reads_a_presigned_url_without_leaking_it() {
    let url = presigned_grid();
    let dir = tempfile::tempdir().unwrap();
    let plan = dir.path().join("shards.json");

    let (ok, out) = run(&[
        "shard-plan",
        &url,
        "--shards",
        "2",
        "--pivot",
        "2",
        "-o",
        plan.to_str().unwrap(),
    ]);
    assert!(ok, "shard-plan failed: {out}");
    assert_no_signature("shard-plan output", &out);
    let text = std::fs::read_to_string(&plan).unwrap();
    assert_no_signature("shards.json", &text);
    assert!(text.contains("grid.parquet?<redacted>"), "{text}");
}

/// A request the host refuses fails loudly, and the error does not quote
/// the query it was sent with.
#[test]
fn a_refused_presigned_url_fails_without_echoing_it() {
    let url = presigned_grid().replace(SIGNATURE, "WRONG820SECRET");
    let dir = tempfile::tempdir().unwrap();
    let out_path = dir.path().join("out.pmtiles");

    let (ok, out) = run(&["tiles", &url, out_path.to_str().unwrap()]);
    assert!(!ok, "a refused URL must fail: {out}");
    assert!(out.contains("403"), "names the refusal: {out}");
    assert!(!out.contains("WRONG820SECRET"), "leaks the query: {out}");
}
