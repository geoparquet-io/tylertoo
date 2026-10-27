//! Export must tile the overview's primary geometry column whatever its name.
//!
//! `convert_to_overviews` resolves the input's geometry column from the `geo`
//! metadata's `primary_column`, so a `SHAPE` column (the Esri file
//! geodatabase name) converts. Export looked the column up by name only
//! (`geometry`, else the first name containing `geom`), so the same overview
//! failed with "overview file has no geometry column", or picked an attribute
//! such as `geom_type` when one came first (#675).
//!
//! An input can also carry a second geometry column (a precomputed
//! `centroid`) ahead of the primary one. The overview writer used to leave
//! `primary_column` to the geoparquet crate, which picks the alphabetically
//! first geometry column unless one is named `geometry`; the overview then
//! declared `centroid` primary and export tiled the ungeneralized points.
//! Every round trip here decodes the archive and checks the features are the
//! input's polygons, not merely that some features came out.
#![allow(clippy::disallowed_methods)] // #779: test code; reads and writes its own trusted fixtures

use std::path::Path;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::Int64Type;
use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geo::{BoundingRect, Centroid, Coord, Geometry, LineString, Polygon};
use geoarrow::array::{from_arrow_array, GeometryBuilder};
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use tylertoo_io::open_parquet;
use tylertoo_io::test_support::ArrowWriter;

use tylertoo_core::batch_processor::extract_geometries_from_array;

const LON: f64 = 10.0;
const LAT: f64 = 50.0;
const HALF: f64 = 0.02;
const MAX_ZOOM: u8 = 8;
/// Coarser than one z4 MVT unit (22.5° / 4096 ≈ 0.0055°).
const TOLERANCE: f64 = 0.01;

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

fn squares() -> Vec<Geometry<f64>> {
    (0..8)
        .map(|i| square(LON + i as f64 * 0.05, LAT + i as f64 * 0.05, HALF))
        .collect()
}

fn geometry_column(name: &str, geoms: &[Geometry<f64>]) -> (Field, ArrayRef) {
    let mut b =
        GeometryBuilder::new(GeometryType::new(Default::default())).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(Some));
    let arr = b.finish();
    (arr.data_type().to_field(name, true), arr.to_array_ref())
}

/// A lon/lat GeoParquet file whose primary geometry column is `geom_name`,
/// with a string column `geom_type` placed first. With `centroid`, a second
/// geometry column of that name holding each square's centre point comes
/// right before the primary one.
fn write_input(path: &Path, geom_name: &str, centroid: Option<&str>, geoms: &[Geometry<f64>]) {
    let n = geoms.len();
    let mut fields = vec![
        Field::new("id", DataType::Int64, false),
        Field::new("geom_type", DataType::Utf8, false),
    ];
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from((0..n as i64).collect::<Vec<_>>())),
        Arc::new(StringArray::from(vec!["Polygon"; n])),
    ];
    if let Some(name) = centroid {
        let points: Vec<Geometry<f64>> = geoms
            .iter()
            .map(|g| Geometry::Point(g.centroid().unwrap()))
            .collect();
        let (field, column) = geometry_column(name, &points);
        fields.push(field);
        columns.push(column);
    }
    let (field, column) = geometry_column(geom_name, geoms);
    fields.push(field);
    columns.push(column);
    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

    let options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_primary_column(geom_name.to_string())
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

/// Every decoded `(zoom, id, geometry)` row.
fn read_decoded(path: &Path) -> Vec<(u8, i64, Geometry<f64>)> {
    let file = std::fs::File::open(path).unwrap();
    let mut rows = Vec::new();
    for batch in open_parquet(file).unwrap().build().unwrap() {
        let batch = batch.unwrap();
        let schema = batch.schema();
        let zoom = batch
            .column(schema.index_of("zoom").unwrap())
            .as_primitive::<arrow_array::types::UInt8Type>()
            .clone();
        let id = batch
            .column(schema.index_of("id").unwrap())
            .as_primitive::<Int64Type>()
            .clone();
        let gidx = schema.index_of("geometry").unwrap();
        let garr = from_arrow_array(batch.column(gidx).as_ref(), schema.field(gidx)).unwrap();
        let mut geoms = Vec::new();
        extract_geometries_from_array(garr.as_ref(), &mut geoms).unwrap();
        for (i, g) in geoms.into_iter().enumerate() {
            rows.push((zoom.value(i), id.value(i), g));
        }
    }
    rows
}

fn round_trip(geom_name: &str, centroid: Option<&str>) {
    use tylertoo_core::decode::{decode_pmtiles, DecodeOptions};
    use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

    let case = format!("geometry column {geom_name:?}, centroid column {centroid:?}");
    let geoms = squares();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.parquet");
    let overview = dir.path().join("ov.parquet");
    let archive = dir.path().join("out.pmtiles");
    write_input(&input, geom_name, centroid, &geoms);

    convert_to_overviews(
        &input,
        &overview,
        &ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: MAX_ZOOM,
            },
            ..Default::default()
        },
    )
    .unwrap_or_else(|e| panic!("convert with {case}: {e}"));

    export_pmtiles(&overview, &archive, &ExportOptions::default())
        .unwrap_or_else(|e| panic!("export with {case}: {e}"));

    let decoded = dir.path().join("decoded.parquet");
    decode_pmtiles(&archive, &decoded, &DecodeOptions::default()).expect("decode");
    let rows = read_decoded(&decoded);

    for (zoom, id, g) in &rows {
        assert!(
            matches!(g, Geometry::Polygon(_) | Geometry::MultiPolygon(_)),
            "{case}: z{zoom} feature {id} decoded to {g:?}, not the input's polygon"
        );
        let want = geoms[usize::try_from(*id).unwrap()]
            .bounding_rect()
            .unwrap();
        let got = g.bounding_rect().unwrap();
        for (w, v) in [
            (want.min().x, got.min().x),
            (want.min().y, got.min().y),
            (want.max().x, got.max().x),
            (want.max().y, got.max().y),
        ] {
            assert!(
                (w - v).abs() <= TOLERANCE,
                "{case}: z{zoom} feature {id} has bbox {got:?}, input square is {want:?}"
            );
        }
    }
    let mut ids: Vec<i64> = rows
        .iter()
        .filter(|(z, _, _)| *z == MAX_ZOOM)
        .map(|(_, id, _)| *id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids,
        (0..geoms.len() as i64).collect::<Vec<_>>(),
        "{case}: every input square is present at z{MAX_ZOOM}"
    );
}

#[test]
fn export_finds_a_shape_geometry_column() {
    round_trip("SHAPE", None);
}

#[test]
fn export_finds_a_geometry_column_with_a_custom_name() {
    round_trip("footprint", None);
}

#[test]
fn export_still_finds_the_conventional_name() {
    round_trip("geometry", None);
}

/// `centroid` sorts before `footprint`, so an unpinned `primary_column`
/// names it, and a scan for the first `geoarrow.*` field finds it too.
#[test]
fn export_tiles_the_primary_column_not_an_earlier_geometry_column() {
    round_trip("footprint", Some("centroid"));
}

/// The same with the primary column named `geometry`, which the geoparquet
/// crate's own inference already prefers.
#[test]
fn export_tiles_geometry_after_an_earlier_geometry_column() {
    round_trip("geometry", Some("centroid"));
}

/// `SHAPE` sorts before `centroid` byte-wise (upper case first), so this one
/// passes on name order alone; it pins that the pin does not break it.
#[test]
fn export_tiles_shape_after_an_earlier_geometry_column() {
    round_trip("SHAPE", Some("centroid"));
}
