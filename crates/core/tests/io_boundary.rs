// Moved consumer-level coverage for the checked I/O boundary.
use std::path::{Path, PathBuf};

fn corpus(target: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target)
        .join(name)
}

/// Every row of every column of the input at `path`, read the way a
/// convert reads it.
fn read_all(path: &Path) -> Result<usize, String> {
    let source = tylertoo_core::input::InputSource::Local(path.to_path_buf());
    let reader = source
        .open()
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    let mut rows = 0;
    for batch in reader {
        rows += batch.map_err(|e| e.to_string())?.num_rows();
    }
    Ok(rows)
}

/// #683: a crafted `ARROW:schema` footer key panicked arrow-ipc's schema
/// conversion on every open.
#[test]
fn issue_683_crafted_arrow_schema_is_an_error_on_every_open() {
    let path = corpus("overview_file", "seed-issue-683-arrow-schema.parquet");
    let err = read_all(&path).unwrap_err();
    assert!(err.contains("panicked"), "input open: {err}");
    let err = tylertoo_core::overview::reader::OverviewReader::open(&path)
        .unwrap_err()
        .to_string();
    assert!(err.contains("panicked"), "overview reader: {err}");
    // `validate` must not pass a file export refuses: the schema decode
    // is one of its checks.
    let report = tylertoo_core::overview::check::validate_file(&path).unwrap();
    let failed = report
        .checks
        .iter()
        .find(|c| c.name == "arrow_schema_decodes" && !c.passed)
        .unwrap_or_else(|| panic!("validate passed #683's file: {:?}", report.checks));
    assert!(failed.message.contains("panicked"), "{}", failed.message);
}

/// #687: a crafted data page panicked parquet-rs's definition-level
/// decoder mid-read.
#[test]
fn issue_687_crafted_def_levels_are_an_error_not_a_panic() {
    let path = corpus("remote_footer", "seed-issue-687-def-levels.parquet");
    let err = read_all(&path).unwrap_err();
    assert!(err.contains("panicked"), "{err}");
}

/// #690: a footer whose schema element declares billions of children
/// made parquet-rs reserve ~12 GB before it read any of them. The
/// pre-check refuses it, naming the count, before parquet sees it.
#[test]
fn issue_690_huge_num_children_is_refused_before_the_decode() {
    let thrift = std::fs::read(corpus(
        "parquet_footer",
        "seed-issue-690-num-children.thrift",
    ))
    .unwrap();
    let mut file = b"PAR1".to_vec();
    file.extend_from_slice(&thrift);
    file.extend_from_slice(&(thrift.len() as u32).to_le_bytes());
    file.extend_from_slice(b"PAR1");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oom.parquet");
    std::fs::write(&path, &file).unwrap();
    let err = tylertoo_core::overview::check::validate_file(&path)
        .unwrap_err()
        .to_string();
    assert!(err.contains("children"), "{err}");
    let err = read_all(&path).unwrap_err();
    assert!(err.contains("children"), "{err}");
}
