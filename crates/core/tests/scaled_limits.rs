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
//! typed error that names it, never with arrow's overflow. Oversized
//! rows are identified from their decoded values rather than chunk averages.
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
    let regression_dir = tempfile::tempdir().unwrap();
    reader_regressions(regression_dir.path());
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
    eprintln!("reference/scaled tile signature: {reference:?} / {scaled:?}");
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

    // D: a ceiling below one row: the owner refuses with the typed #563
    // input error, naming the actual oversized row.
    std::env::set_var(BYTE_ARRAY_ENV, BELOW_ONE_ROW.to_string());
    let err = convert_to_overviews(&input, dir.path().join("row.parquet"), &options(true))
        .expect_err("no batch can hold one row");
    assert!(err.to_string().contains("one row"), "{err}");

    std::env::remove_var(BYTE_ARRAY_ENV);
    std::env::remove_var(MEMORY_ENV);
}

#[expect(
    clippy::disallowed_methods,
    reason = "#779: writes a trusted temporary regression fixture"
)]
fn reader_regressions(dir: &Path) {
    use arrow_array::builder::{ListBuilder, StringBuilder};
    use arrow_array::{ArrayRef, RecordBatch, StringArray};
    use std::sync::Arc;
    use tylertoo_io::open_parquet;
    use tylertoo_io::test_support::ArrowWriter;
    use tylertoo_io::test_support::WriterProperties;

    std::env::set_var(BYTE_ARRAY_ENV, "1000");
    let read = |name: &str, array: ArrayRef| {
        let batch = RecordBatch::try_from_iter(vec![("name", array)]).unwrap();
        let path = dir.join(name);
        let mut writer = ArrowWriter::try_new(
            std::fs::File::create(&path).unwrap(),
            batch.schema(),
            Some(
                WriterProperties::builder()
                    .set_dictionary_enabled(false)
                    .build(),
            ),
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        open_parquet(std::fs::File::open(path).unwrap())
            .unwrap()
            .with_batch_rows(100)
            .build()
            .map_err(arrow_schema::ArrowError::from)?
            .collect::<Result<Vec<_>, _>>()
    };
    let mut values = vec![String::new(); 100];
    values[0] = "x".repeat(600);
    values[1] = "y".repeat(600);
    let batches = read("skew.parquet", Arc::new(StringArray::from(values))).unwrap();
    assert_eq!(
        batches.iter().map(RecordBatch::num_rows).sum::<usize>(),
        100
    );
    assert!(batches.iter().all(|b| {
        {
            let a = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
            *a.value_offsets().last().unwrap() <= 1000
        }
    }));
    let mut list = ListBuilder::new(StringBuilder::new());
    list.values().append_value("x".repeat(600));
    list.values().append_value("y".repeat(600));
    list.append(true);
    let error = read("nested.parquet", Arc::new(list.finish())).unwrap_err();
    assert!(
        matches!(
            tylertoo_io::input_error(&error),
            Some(tylertoo_io::Error::ByteArrayRowTooLarge { .. })
        ),
        "{error}"
    );
    let error = read(
        "single.parquet",
        Arc::new(StringArray::from(vec!["x".repeat(1200)])),
    )
    .unwrap_err();
    assert!(
        matches!(
            tylertoo_io::input_error(&error),
            Some(tylertoo_io::Error::ByteArrayRowTooLarge { .. })
        ),
        "{error}"
    );
    // Nested children must be split by addressed offsets, including slices.
    let mut list = ListBuilder::new(StringBuilder::new());
    for value in ["unused", &"a".repeat(600), &"b".repeat(600)] {
        list.values().append_value(value);
        list.append(true);
    }
    let sliced = list.finish().slice(1, 2);
    let batches = read("list-slice.parquet", Arc::new(sliced)).unwrap();
    assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 2);

    use arrow_array::types::Int8Type;
    use arrow_array::{DictionaryArray, Int8Array, LargeStringArray, StructArray};
    use arrow_schema::{DataType, Field};
    let dict = |bytes| {
        Arc::new(
            DictionaryArray::<Int8Type>::try_new(
                Int8Array::from(vec![0, 0]),
                Arc::new(StringArray::from(vec!["x".repeat(bytes)])),
            )
            .unwrap(),
        ) as ArrayRef
    };
    let batches = read("dictionary.parquet", dict(600)).unwrap();
    assert_eq!(batches.iter().map(RecordBatch::num_rows).sum::<usize>(), 2);
    // No optional decoded-size statistics: repeated payloads must remain
    // shared through the first decode, not expand before the splitter runs.
    let path = dir.join("dictionary-no-size-stats.parquet");
    let batch = RecordBatch::try_from_iter(vec![(
        "name",
        Arc::new(StringArray::from(vec!["x".repeat(600); 100])) as ArrayRef,
    )])
    .unwrap();
    let mut writer =
        ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), batch.schema(), None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    let source = open_parquet(std::fs::File::open(&path).unwrap()).unwrap();
    let group = source.metadata().row_group(0).clone();
    let columns = group
        .columns()
        .iter()
        .cloned()
        .map(|c| {
            c.into_builder()
                .set_unencoded_byte_array_data_bytes(None)
                .build()
                .unwrap()
        })
        .collect();
    let group = group
        .into_builder()
        .set_column_metadata(columns)
        .build()
        .unwrap();
    let metadata = source
        .metadata()
        .as_ref()
        .clone()
        .into_builder()
        .set_row_groups(vec![group])
        .build();
    let before = memory_stats::memory_stats().map(|s| s.physical_mem);
    let reader = tylertoo_io::open_parquet_with_metadata(
        std::fs::File::open(&path).unwrap(),
        tylertoo_io::reader_metadata(Arc::new(metadata)).unwrap(),
    )
    .with_batch_rows(100)
    .build()
    .unwrap();
    let mut rows = 0;
    let mut peak = before;
    for batch in reader {
        let batch = batch.unwrap();
        tylertoo_io::check_byte_array_ceiling(&batch).unwrap();
        rows += batch.num_rows();
        if let Some(stats) = memory_stats::memory_stats() {
            peak = Some(peak.unwrap_or(0).max(stats.physical_mem));
        }
    }
    assert_eq!(rows, 100);
    eprintln!("dictionary without size stats: RSS before={before:?}, sampled peak={peak:?}");
    let error = read("dictionary-too-wide.parquet", dict(1200)).unwrap_err();
    assert!(
        matches!(
            tylertoo_io::input_error(&error),
            Some(tylertoo_io::Error::ByteArrayRowTooLarge { .. })
        ),
        "{error}"
    );
    let child = dict(1200);
    let field = Arc::new(Field::new("child", child.data_type().clone(), false));
    let nested = StructArray::from(vec![(field, child)]);
    assert!(read("struct-dictionary.parquet", Arc::new(nested)).is_err());
    let child: ArrayRef = Arc::new(LargeStringArray::from(vec!["x".repeat(1200)]));
    let nested = StructArray::from(vec![(
        Arc::new(Field::new("wide", DataType::LargeUtf8, false)),
        child,
    )]);
    assert!(read("large-offsets.parquet", Arc::new(nested)).is_ok());
    use arrow_array::builder::{FixedSizeListBuilder, MapBuilder};
    let mut fixed = FixedSizeListBuilder::new(StringBuilder::new(), 2);
    fixed.values().append_value("x".repeat(600));
    fixed.values().append_value("y".repeat(600));
    fixed.append(true);
    assert!(read("fixed-list.parquet", Arc::new(fixed.finish())).is_err());
    let mut map = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
    for key in ["a", "b"] {
        map.keys().append_value(key);
        map.values().append_value("x".repeat(600));
    }
    map.append(true).unwrap();
    assert!(read("map.parquet", Arc::new(map.finish())).is_err());
    std::env::remove_var(BYTE_ARRAY_ENV);
}
