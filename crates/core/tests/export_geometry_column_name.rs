//! Export must find the overview's geometry column whatever its name.
//!
//! `convert_to_overviews` resolves the input's geometry column from the `geo`
//! metadata, so a `SHAPE` column (the Esri file geodatabase name) converts. Export looked the
//! column up by name only (`geometry`, else the first name containing `geom`),
//! so the same overview failed with "overview file has no geometry column",
//! or picked an attribute such as `geom_type` when one came first.

use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geo::{Coord, Geometry, LineString, Polygon};
use geoarrow::array::GeometryBuilder;
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use parquet::arrow::ArrowWriter;

const LON: f64 = 10.0;
const LAT: f64 = 50.0;

fn square(lon: f64, lat: f64, half: f64) -> Geometry<f64> {
    let ring: Vec<Coord<f64>> = [
        (lon - half, lat - half),
        (lon + half, lat - half),
        (lon + half, lat + half),
        (lon - half, lat + half),
        (lon - half, lat - half),
    ]
    .iter()
    .map(|&(x, y)| Coord { x, y })
    .collect();
    Geometry::Polygon(Polygon::new(LineString::from(ring), vec![]))
}

/// A lon/lat GeoParquet file with a string column `geom_type` placed before
/// a geometry column named `geom_name`.
fn write_input(path: &Path, geom_name: &str, geoms: &[Geometry<f64>]) {
    let mut b =
        GeometryBuilder::new(GeometryType::new(Default::default())).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(Some));
    let geom_arr = b.finish();
    let geom_field = geom_arr.data_type().to_field(geom_name, true);

    let n = geoms.len();
    let schema = Arc::new(Schema::new(vec![
        Arc::new(Field::new("id", DataType::Int64, false)),
        Arc::new(Field::new("geom_type", DataType::Utf8, false)),
        Arc::new(geom_field),
    ]));
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from((0..n as i64).collect::<Vec<_>>())),
        Arc::new(StringArray::from(vec!["Polygon"; n])),
        geom_arr.to_array_ref(),
    ];
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

    let options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &options).unwrap();
    let file = std::fs::File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, encoder.target_schema(), None).unwrap();
    writer
        .write(&encoder.encode_record_batch(&batch).unwrap())
        .unwrap();
    writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
    writer.close().unwrap();
}

fn round_trip(geom_name: &str) {
    use tylertoo_core::decode::{decode_pmtiles, DecodeOptions};
    use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

    let geoms: Vec<Geometry<f64>> = (0..8)
        .map(|i| square(LON + i as f64 * 0.05, LAT + i as f64 * 0.05, 0.02))
        .collect();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.parquet");
    let overview = dir.path().join("ov.parquet");
    let archive = dir.path().join("out.pmtiles");
    write_input(&input, geom_name, &geoms);

    convert_to_overviews(
        &input,
        &overview,
        &ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 8,
            },
            ..Default::default()
        },
    )
    .unwrap_or_else(|e| panic!("convert with geometry column {geom_name:?}: {e}"));

    export_pmtiles(&overview, &archive, &ExportOptions::default())
        .unwrap_or_else(|e| panic!("export with geometry column {geom_name:?}: {e}"));

    let decoded = dir.path().join("decoded.parquet");
    let report = decode_pmtiles(&archive, &decoded, &DecodeOptions::default()).expect("decode");
    assert!(
        report.features_written > 0,
        "geometry column {geom_name:?}: archive decoded to zero features"
    );
}

#[test]
fn export_finds_a_shape_geometry_column() {
    round_trip("SHAPE");
}

#[test]
fn export_finds_a_geometry_column_with_a_custom_name() {
    round_trip("footprint");
}

#[test]
fn export_still_finds_the_conventional_name() {
    round_trip("geometry");
}
