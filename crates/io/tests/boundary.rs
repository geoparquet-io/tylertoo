use std::sync::Arc;

use arrow_array::{BinaryArray, Int32Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use tylertoo_io::{open_parquet, overview_writer, OverviewWriteOptions};

#[test]
fn overview_writer_preserves_pruning_statistics_and_manual_row_groups() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("shape", DataType::Binary, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("level", DataType::Int32, false),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(BinaryArray::from(vec![b"wkb".as_slice(); 3])),
            Arc::new(StringArray::from(vec!["a", "b", "c"])),
            Arc::new(Int32Array::from(vec![0, 0, 1])),
        ],
    )
    .unwrap();
    for full_column_stats in [false, true] {
        let options = OverviewWriteOptions {
            zstd_level: 3,
            full_column_stats,
            geometry_columns: vec!["shape".into()],
            covering_columns: vec!["shape_bbox".into()],
        };
        let mut file = tempfile::tempfile().unwrap();
        let mut writer = overview_writer(&mut file, schema.clone(), &schema, &options).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let source = open_parquet(file).unwrap();
        let metadata = source.metadata();
        assert_eq!(metadata.num_row_groups(), 1);
        let group = metadata.row_group(0);
        assert_eq!(group.column(0).statistics().is_some(), full_column_stats);
        assert_eq!(group.column(1).statistics().is_some(), full_column_stats);
        assert!(group.column(2).statistics().is_some());
        assert_eq!(group.num_rows(), 3);
        assert_eq!(source.build().unwrap().next().unwrap().unwrap(), batch);
    }
}

#[test]
fn overview_writer_rejects_invalid_compression_before_writing() {
    let schema = Arc::new(Schema::empty());
    let mut bytes = Vec::new();
    let options = OverviewWriteOptions {
        zstd_level: i32::MAX,
        full_column_stats: false,
        geometry_columns: vec![],
        covering_columns: vec![],
    };
    assert!(overview_writer(&mut bytes, schema.clone(), &schema, &options).is_err());
    assert!(bytes.is_empty());
}

#[test]
fn row_limit_is_applied_before_batch_decoding() {
    let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int32, false)]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int32Array::from(vec![1, 2, 3]))],
    )
    .unwrap();
    let mut file = tempfile::tempfile().unwrap();
    let mut writer = tylertoo_io::batch_writer(&mut file, schema).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    let mut reader = open_parquet(file).unwrap().with_limit(2).build().unwrap();
    assert_eq!(reader.next().unwrap().unwrap(), batch.slice(0, 2));
    assert!(reader.next().is_none());
}
