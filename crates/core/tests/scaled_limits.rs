//! The scaled-limit lane (#779, G3): the hard ceilings that only bite on
//! cluster-scale inputs, scaled down to a small real fixture, through every
//! reader path.
//!
//! - arrow's i32 byte-array ceiling (2 GiB per decoded batch, #563):
//!   `TYLERTOO_BYTE_ARRAY_LIMIT_BYTES` scales it. The ceiling has one owner,
//!   `untrusted::byte_array_limit`: every path that sizes a batch reads the
//!   scaled value from it, and the owner's reader (plus the in-memory
//!   engine's concatenated table) fails a batch past it the way arrow fails
//!   past the real one (`untrusted::check_byte_array_ceiling`). The fix for #563 reached
//!   two of four reader paths (#606 `--no-streaming`, #607 `validate`) because
//!   only a >2 GiB file could show the other two. This replaces the
//!   `#[ignore]`d test that wrote such a file (`input_set.rs`,
//!   `open_stream_reads_a_real_oversized_row_group`).
//! - the pass-1 memory floor (#543), the pass-2 sink budgets (#294) and the
//!   export's partition-wave budget (#303): `TYLERTOO_AUTO_MEM_LIMIT_BYTES`,
//!   the existing override, set to 1 MiB, so `auto` takes the bounded/spill
//!   paths and the floor warns.
//!
//! The output must not move: a batch size or a memory budget is scheduling,
//! never content. Where a ceiling cannot be met, the run must fail with the
//! typed error that names it, before reading data, never with arrow's
//! overflow.
//!
//! One `#[test]` on purpose: the overrides are process environment, and the
//! pass-2 memory probe caches its first answer for the process, so the
//! scenarios run in a fixed order in one test binary of their own.

use std::path::Path;

use tylertoo_core::overview::check::validate_file;
use tylertoo_core::overview::convert::{
    convert_to_overviews, ConvertError, ConvertOptions, ConvertReport, LevelPlan,
};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions, ExportReport};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

const BYTE_ARRAY_ENV: &str = "TYLERTOO_BYTE_ARRAY_LIMIT_BYTES";
const MEMORY_ENV: &str = "TYLERTOO_AUTO_MEM_LIMIT_BYTES";
/// The fixture's geometry column is ~130 KB over one row group: a 16 KiB
/// ceiling caps every batch to a few dozen rows and puts the whole column
/// 8x over what one batch may hold.
const SCALED_LIMIT: u64 = 16 * 1024;
/// Below one row's bytes: no batch size can decode the column.
const BELOW_ONE_ROW: u64 = 64;

fn options(streaming: bool) -> ConvertOptions {
    ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 14,
        },
        streaming,
        ..Default::default()
    }
}

type Signature = (Vec<(usize, usize)>, Vec<(u8, usize, usize)>);

/// Per level (features, vertices) and per zoom (tiles, features): the
/// output, independent of how it was scheduled.
fn signature(conv: &ConvertReport, exp: &ExportReport) -> Signature {
    (
        conv.levels
            .iter()
            .map(|l| (l.feature_count, l.vertex_count))
            .collect(),
        exp.zooms
            .iter()
            .map(|z| (z.zoom, z.tile_count, z.tile_feature_count))
            .collect(),
    )
}

/// Streaming convert, export, and `validate` of the overview.
fn run(input: &Path, dir: &Path, tag: &str) -> Signature {
    let overview = dir.join(format!("{tag}.parquet"));
    let archive = dir.join(format!("{tag}.pmtiles"));
    let conv = convert_to_overviews(input, &overview, &options(true))
        .unwrap_or_else(|e| panic!("{tag}: streaming convert: {e}"));
    let exp = export_pmtiles(&overview, &archive, &ExportOptions::default())
        .unwrap_or_else(|e| panic!("{tag}: export: {e}"));
    let report = validate_file(&overview).unwrap_or_else(|e| panic!("{tag}: validate: {e}"));
    let failed: Vec<String> = report
        .failures()
        .map(|c| format!("{}: {}", c.name, c.message))
        .collect();
    assert!(failed.is_empty(), "{tag}: validate failed: {failed:?}");
    signature(&conv, &exp)
}

#[test]
fn every_reader_path_honours_the_scaled_ceilings() {
    let Some(input) = fixture::realdata("open-buildings.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    // Process-wide for the whole test: the first pass-2 probe caches it.
    std::env::set_var(MEMORY_ENV, (1u64 << 20).to_string());
    std::env::remove_var(BYTE_ARRAY_ENV);

    // A: the real byte-array ceiling (only the memory budget scaled).
    let reference = run(&input, dir.path(), "reference");

    // B: the scaled ceiling. Streaming pass 1 and pass 2 (`SourceStream`),
    // the export's overview reader and decode, and `validate`'s readers all
    // cap their batches; any one that did not would fail its decode check.
    std::env::set_var(BYTE_ARRAY_ENV, SCALED_LIMIT.to_string());
    let scaled = run(&input, dir.path(), "scaled");
    assert_eq!(
        scaled, reference,
        "a {SCALED_LIMIT}-byte byte-array ceiling changed the output"
    );

    // C: `--no-streaming` holds the whole column in one batch, which the
    // scaled ceiling cannot: the typed #606 error, from footer statistics.
    let err = convert_to_overviews(&input, dir.path().join("mem.parquet"), &options(false))
        .expect_err("--no-streaming cannot hold the column in one batch");
    match err {
        ConvertError::InMemoryByteArrayTooLarge { limit_bytes, .. } => {
            assert_eq!(limit_bytes, SCALED_LIMIT);
        }
        other => panic!("--no-streaming: expected InMemoryByteArrayTooLarge, got {other}"),
    }

    // D: a ceiling below one row: the streaming preflight refuses with the
    // typed #563 error, naming the column, before reading data.
    std::env::set_var(BYTE_ARRAY_ENV, BELOW_ONE_ROW.to_string());
    let err = convert_to_overviews(&input, dir.path().join("row.parquet"), &options(true))
        .expect_err("no batch can hold one row");
    match err {
        ConvertError::ByteArrayRowTooLarge { limit_bytes, .. } => {
            assert_eq!(limit_bytes, BELOW_ONE_ROW);
        }
        other => panic!("streaming: expected ByteArrayRowTooLarge, got {other}"),
    }

    std::env::remove_var(BYTE_ARRAY_ENV);
    std::env::remove_var(MEMORY_ENV);
}
