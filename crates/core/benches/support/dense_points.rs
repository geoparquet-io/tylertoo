//! Deterministic #711 fixture: four properties, all points inside z14/8192/8192.
//! `tile_compress_dedup --generate-dense .bench-out/dense.parquet 500000`

use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, Float64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geo::{Geometry, Point};
use geoarrow::array::GeometryBuilder;
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use tylertoo_io::test_support::ArrowWriter;

#[expect(
    clippy::disallowed_methods,
    reason = "benchmark fixture output, not a published archive"
)]
pub(super) fn write(path: &Path, rows: usize) {
    let mut builder = GeometryBuilder::new(GeometryType::new(Default::default()));
    let geoms: Vec<_> = (0..rows)
        .map(|i| {
            Geometry::Point(Point::new(
                0.004 + (i % 1000) as f64 * 0.00001,
                -0.004 - ((i / 1000) % 1000) as f64 * 0.00001,
            ))
        })
        .collect();
    builder.extend_from_iter(geoms.iter().map(Some));
    let geometry = builder.finish();
    let fields = vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("confidence", DataType::Float64, false),
        Field::new("basic_category", DataType::Utf8, false),
        Field::new("operating_status", DataType::Utf8, false),
        geometry.data_type().to_field("geometry", false),
    ];
    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(
            (0..rows).map(|i| format!("place-{i:016x}")),
        )),
        Arc::new(Float64Array::from_iter_values(
            (0..rows).map(|i| (i % 100) as f64 / 100.0),
        )),
        Arc::new(StringArray::from_iter_values(
            (0..rows).map(|i| ["shop", "office", "cafe", "school"][i % 4]),
        )),
        Arc::new(StringArray::from(vec!["open"; rows])),
        geometry.to_array_ref(),
    ];
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(Arc::clone(&schema), columns).unwrap();
    let options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(true)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &options).unwrap();
    let mut writer = ArrowWriter::try_new(
        std::fs::File::create(path).unwrap(),
        encoder.target_schema(),
        None,
    )
    .unwrap();
    writer
        .write(&encoder.encode_record_batch(&batch).unwrap())
        .unwrap();
    writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
    writer.close().unwrap();
}
