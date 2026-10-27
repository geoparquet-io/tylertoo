use arrow_array::{Array, ArrayRef, Int32Array, RecordBatch, StringArray, StructArray};
use arrow_schema::{DataType, Field};
use parquet::arrow::{ArrowWriter, ProjectionMask};
use std::sync::Arc;
use tylertoo_core::untrusted::open_parquet;

#[test]
fn byte_array_widening_preserves_partial_nested_projection() {
    let child = Arc::new(StructArray::from(vec![
        (
            Arc::new(Field::new("name", DataType::Utf8, false)),
            Arc::new(StringArray::from(vec!["hello"])) as ArrayRef,
        ),
        (
            Arc::new(Field::new("number", DataType::Int32, false)),
            Arc::new(Int32Array::from(vec![7])) as ArrayRef,
        ),
    ]));
    let batch = RecordBatch::try_from_iter(vec![("child", child as ArrayRef)]).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut writer = ArrowWriter::try_new(file.reopen().unwrap(), batch.schema(), None).unwrap();
    writer.write(&batch).unwrap();
    writer.close().unwrap();
    let source = open_parquet(file.reopen().unwrap()).unwrap();
    let mask = ProjectionMask::leaves(source.parquet_schema(), [0]);
    let batch = source
        .with_projection(mask)
        .build()
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let child = batch
        .column(0)
        .as_any()
        .downcast_ref::<StructArray>()
        .unwrap();
    assert_eq!(child.num_columns(), 1);
    assert_eq!(child.column(0).data_type(), &DataType::Utf8);
    assert_eq!(child.len(), 1);
}
