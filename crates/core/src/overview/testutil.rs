//! Shared test fixtures for the crate's tests.
//!
//! Compiled under `cfg(test)` only. Lives here rather than being re-created
//! per test module: several suites (`overview::hostile`, `pyramid`) need the
//! same "write a small GeoParquet" primitive, and three copies of a fixture
//! writer drift.

use std::path::Path;
use std::sync::Arc;

use arrow_array::{Array, Int32Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geo::Geometry;
use geoarrow::array::GeometryBuilder;
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use parquet::arrow::ArrowWriter;

/// Write a GeoParquet file with `id` (Int64), `name` (Utf8) property columns
/// and the given (possibly null) geometries. `covering` toggles bbox covering
/// generation; `extra_col` injects an additional Int32 column with the given
/// name (reserved-column auto-rename tests, #288).
pub(crate) fn write_input(
    path: &Path,
    geoms: &[Option<Geometry<f64>>],
    covering: bool,
    extra_col: Option<&str>,
) {
    let n = geoms.len();
    let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
    let name = StringArray::from((0..n).map(|i| format!("f{i}")).collect::<Vec<_>>());

    let typ = GeometryType::new(Default::default());
    let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(|g| g.as_ref()));
    let geom_arr = b.finish();
    let geom_field = geom_arr.data_type().to_field("geometry", true);

    let mut fields = vec![
        Arc::new(Field::new("id", DataType::Int64, false)),
        Arc::new(Field::new("name", DataType::Utf8, false)),
    ];
    let mut columns: Vec<Arc<dyn Array>> = vec![Arc::new(id), Arc::new(name)];
    if let Some(col) = extra_col {
        fields.push(Arc::new(Field::new(col, DataType::Int32, false)));
        columns.push(Arc::new(Int32Array::from(vec![0i32; n])));
    }
    fields.push(Arc::new(geom_field));
    columns.push(geom_arr.to_array_ref());

    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

    let gpq_options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(covering)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
    let target_schema = encoder.target_schema();

    let file = std::fs::File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, target_schema, None).unwrap();
    let encoded = encoder.encode_record_batch(&batch).unwrap();
    writer.write(&encoded).unwrap();
    writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
    writer.close().unwrap();
}
