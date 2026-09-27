//! Shared test fixtures for the crate's in-tree (`#[cfg(test)]`) unit tests.
//!
//! Compiled under `cfg(test)` only. Lives here rather than being re-created
//! per test module: several suites (`overview::convert`, `overview::plan_state`,
//! `pyramid`) need the same "write a small GeoParquet" primitive, and
//! several copies of a fixture writer drift.
//!
//! This module is `pub(crate)`, so it is invisible to the integration-test
//! binaries under `crates/core/tests/` (they link only against the crate's
//! public API). `crates/core/tests/overview_hostile.rs` compiles this same
//! file in via `#[path]` instead (#457), so keep it free of `crate::` /
//! `super::` imports: std and external crates only.

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

/// Write a GeoParquet file carrying one **f64 attribute column** alongside the
/// geometry, for tests that rank on a real per-row value (#364's magnitude
/// ladder).
///
/// A sibling of [`write_input`] rather than a parameter on it: that one's
/// `extra_col` injects a constant Int32, which cannot carry a ladder's rungs.
pub(crate) fn write_input_with_f64(
    path: &Path,
    geoms: &[Option<Geometry<f64>>],
    column: &str,
    values: &[f64],
) {
    use arrow_array::Float64Array;

    assert_eq!(geoms.len(), values.len(), "one value per geometry");
    let id = Int64Array::from((0..geoms.len() as i64).collect::<Vec<_>>());
    let attr = Float64Array::from(values.to_vec());

    let typ = GeometryType::new(Default::default());
    let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(|g| g.as_ref()));
    let geom_arr = b.finish();
    let geom_field = geom_arr.data_type().to_field("geometry", true);

    let schema = Arc::new(Schema::new(vec![
        Arc::new(Field::new("id", DataType::Int64, false)),
        Arc::new(Field::new(column, DataType::Float64, true)),
        Arc::new(geom_field),
    ]));
    let columns: Vec<Arc<dyn Array>> = vec![Arc::new(id), Arc::new(attr), geom_arr.to_array_ref()];
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

    let gpq_options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(true)
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

/// Write a multi-row-group GeoParquet file with covering column stats so
/// row-group pruning can actually bite (#102). Each row group contains one
/// point at `(x, y)` with `id` = row-group index.
///
/// Shared by `overview::convert`'s bbox-pruning tests and
/// `overview::stream`'s pruned-groups-are-never-read test (#422).
pub(crate) fn write_multi_rg_input(path: &Path, coords: &[(f64, f64)], with_covering: bool) {
    write_multi_rg_input_with_crs(path, coords, with_covering, None)
}

/// [`write_multi_rg_input`] with an explicit geometry CRS (#518: the
/// EPSG:3857 probe needs a file that declares Pseudo-Mercator PROJJSON).
pub(crate) fn write_multi_rg_input_with_crs(
    path: &Path,
    coords: &[(f64, f64)],
    with_covering: bool,
    crs_metadata: Option<geoarrow::datatypes::Metadata>,
) {
    use geo::Point;
    use parquet::file::properties::WriterProperties;

    let geoms: Vec<Geometry<f64>> = coords
        .iter()
        .map(|&(x, y)| Geometry::Point(Point::new(x, y)))
        .collect();
    let n = geoms.len();
    let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
    let typ = match crs_metadata {
        Some(md) => GeometryType::new(Arc::new(md)),
        None => GeometryType::new(Default::default()),
    };
    let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(Some));
    let geom_arr = b.finish();
    let geom_field = geom_arr.data_type().to_field("geometry", true);
    let fields = vec![
        Arc::new(Field::new("id", DataType::Int64, false)),
        Arc::new(geom_field),
    ];
    let columns: Vec<Arc<dyn Array>> = vec![Arc::new(id), geom_arr.to_array_ref()];
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

    let gpq_options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(with_covering)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
    let target_schema = encoder.target_schema();
    // Row-group size = 1 to force n row groups.
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(1))
        .build();
    let file = std::fs::File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, target_schema, Some(props)).unwrap();
    let encoded = encoder.encode_record_batch(&batch).unwrap();
    writer.write(&encoded).unwrap();
    writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
    writer.close().unwrap();
}

/// One [`write_multi_rg_attr_input`] row: `((x, y), confidence, crop)`.
pub(crate) type AttrRow = ((f64, f64), Option<f64>, Option<&'static str>);

/// Multi-row-group input with attribute columns: one row per row group at
/// `(x, y)` with `id` = row-group index, plus a nullable `confidence`
/// Float64 and a nullable `crop` Utf8 column. One row per row group makes
/// per-row-group column statistics exact, so pushdown pruning (#315) can
/// bite.
pub(crate) fn write_multi_rg_attr_input(path: &Path, rows: &[AttrRow]) {
    use arrow_array::Float64Array;
    use geo::Point;
    use parquet::file::properties::WriterProperties;

    let geoms: Vec<Geometry<f64>> = rows
        .iter()
        .map(|&((x, y), _, _)| Geometry::Point(Point::new(x, y)))
        .collect();
    let n = geoms.len();
    let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
    let confidence = Float64Array::from(rows.iter().map(|(_, c, _)| *c).collect::<Vec<_>>());
    let crop = StringArray::from(rows.iter().map(|(_, _, s)| *s).collect::<Vec<_>>());
    let typ = GeometryType::new(Default::default());
    let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(Some));
    let geom_arr = b.finish();
    let geom_field = geom_arr.data_type().to_field("geometry", true);
    let fields = vec![
        Arc::new(Field::new("id", DataType::Int64, false)),
        Arc::new(Field::new("confidence", DataType::Float64, true)),
        Arc::new(Field::new("crop", DataType::Utf8, true)),
        Arc::new(geom_field),
    ];
    let columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(id),
        Arc::new(confidence),
        Arc::new(crop),
        geom_arr.to_array_ref(),
    ];
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

    let gpq_options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(true)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
    let target_schema = encoder.target_schema();
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(1))
        .build();
    let file = std::fs::File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, target_schema, Some(props)).unwrap();
    let encoded = encoder.encode_record_batch(&batch).unwrap();
    writer.write(&encoded).unwrap();
    writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
    writer.close().unwrap();
}
