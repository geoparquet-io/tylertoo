//! EPSG:3857 input must survive the whole `convert -> export` chain (#519).
//!
//! #518 taught `detect_crs_from_kv` to read a GeoPandas-shaped
//! `{"type":"ProjectedCRS","name":"WGS 84 / Pseudo-Mercator","id":{"EPSG":3857}}`
//! PROJJSON as Web Mercator instead of lon/lat, so a 3857 file now *converts*.
//! But the overview writer built its output geometry field from
//! `GeometryType::new(Default::default())` — no geoarrow extension metadata —
//! so the overview's own `geo` JSON carried no `crs` key at all. Export then
//! read the overview as the GeoParquet default (EPSG:4326), fed metre
//! coordinates to the lon/lat tile grid, and every feature fell outside the
//! world: a successful, EMPTY archive. That is strictly worse than the loud
//! pre-#518 error.
//!
//! This test is the round trip nobody had: metres in, tiles out, features
//! where the map says they should be.

use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, Float64Array, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use geo::{Coord, Geometry, LineString, Polygon};
use geoarrow::array::GeometryBuilder;
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use parquet::arrow::ArrowWriter;

/// Half the Web Mercator world extent, in metres.
const WEBMERC_HALF_M: f64 = 20_037_508.342_789_244;

/// The lon/lat the fixture's features really sit at.
const LON: f64 = 10.0;
const LAT: f64 = 50.0;

/// lon/lat degrees -> EPSG:3857 metres (the inverse of what export must do).
fn lnglat_to_webmerc(lng: f64, lat: f64) -> (f64, f64) {
    use std::f64::consts::{FRAC_PI_4, PI};
    let x = lng / 180.0 * WEBMERC_HALF_M;
    let y = (FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln() / PI * WEBMERC_HALF_M;
    (x, y)
}

/// A square polygon of `half_deg` degrees around `(lon, lat)`, expressed in
/// EPSG:3857 metres.
fn webmerc_square(lon: f64, lat: f64, half_deg: f64) -> Geometry<f64> {
    let corners = [
        (lon - half_deg, lat - half_deg),
        (lon + half_deg, lat - half_deg),
        (lon + half_deg, lat + half_deg),
        (lon - half_deg, lat + half_deg),
        (lon - half_deg, lat - half_deg),
    ];
    let ring: Vec<Coord<f64>> = corners
        .iter()
        .map(|&(a, b)| {
            let (x, y) = lnglat_to_webmerc(a, b);
            Coord { x, y }
        })
        .collect();
    Geometry::Polygon(Polygon::new(LineString::from(ring), vec![]))
}

/// Write a GeoParquet file whose geometry column declares EPSG:3857 through
/// PROJJSON named "WGS 84 / Pseudo-Mercator" — the exact shape GeoPandas
/// writes, and the shape #518 was about.
fn write_pseudo_mercator_input(path: &Path, geoms: &[Geometry<f64>]) {
    let projjson = serde_json::json!({
        "type": "ProjectedCRS",
        "name": "WGS 84 / Pseudo-Mercator",
        "id": { "authority": "EPSG", "code": 3857 }
    });
    let md =
        geoarrow::datatypes::Metadata::new(geoarrow::datatypes::Crs::from_projjson(projjson), None);
    let typ = GeometryType::new(Arc::new(md));
    let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(Some));
    let geom_arr = b.finish();
    let geom_field = geom_arr.data_type().to_field("geometry", true);

    let n = geoms.len();
    let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
    let rank = Float64Array::from((0..n).map(|i| (n - i) as f64).collect::<Vec<_>>());
    let schema = Arc::new(Schema::new(vec![
        Arc::new(Field::new("id", DataType::Int64, false)),
        Arc::new(Field::new("rank", DataType::Float64, false)),
        Arc::new(geom_field),
    ]));
    let columns: Vec<ArrayRef> = vec![Arc::new(id), Arc::new(rank), geom_arr.to_array_ref()];
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

    let gpq_options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(true)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
    let target_schema = encoder.target_schema();
    let file = std::fs::File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, target_schema, None).unwrap();
    writer
        .write(&encoder.encode_record_batch(&batch).unwrap())
        .unwrap();
    writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
    writer.close().unwrap();
}

/// The `crs` the overview file's own `geo` metadata declares, as a string
/// (PROJJSON re-serialized, or the authority code), or `None` when the key is
/// absent — which GeoParquet defines to mean OGC:CRS84 and is exactly the
/// silent lie this test exists to catch.
fn overview_declared_crs(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).unwrap();
    let reader = parquet::file::serialized_reader::SerializedFileReader::new(file).unwrap();
    let kv = parquet::file::reader::FileReader::metadata(&reader)
        .file_metadata()
        .key_value_metadata()?
        .iter()
        .find(|kv| kv.key.eq_ignore_ascii_case("geo"))?
        .value
        .clone()?;
    let v: serde_json::Value = serde_json::from_str(&kv).unwrap();
    let crs = v.get("columns")?.get("geometry")?.get("crs")?;
    if crs.is_null() {
        return None;
    }
    Some(crs.to_string())
}

#[test]
fn webmercator_input_round_trips_through_convert_and_export() {
    use tylertoo_core::decode::{decode_pmtiles, DecodeOptions};
    use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};
    use tylertoo_core::pmtiles_writer::Header;

    let geoms: Vec<Geometry<f64>> = (0..8)
        .map(|i| {
            webmerc_square(
                LON + i as f64 * 0.05,
                LAT + i as f64 * 0.05,
                0.02 + i as f64 * 0.005,
            )
        })
        .collect();

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.parquet");
    let overview = dir.path().join("ov.parquet");
    let archive = dir.path().join("out.pmtiles");
    write_pseudo_mercator_input(&input, &geoms);

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
    .expect("a Pseudo-Mercator input must convert");

    // (1) The overview must not forget the CRS on the way out. Without this
    //     the export below reads metres as degrees.
    let declared = overview_declared_crs(&overview);
    assert!(
        declared
            .as_deref()
            .is_some_and(|s| s.contains("3857") || s.to_uppercase().contains("PSEUDO-MERCATOR")),
        "the overview must carry the input's EPSG:3857 CRS in its `geo` metadata, \
         got {declared:?}"
    );

    export_pmtiles(&overview, &archive, &ExportOptions::default()).expect("export");

    // (2) Non-empty archive.
    let bytes = std::fs::read(&archive).unwrap();
    let header = Header::from_bytes(&bytes).unwrap();
    assert!(
        header.addressed_tiles_count > 0,
        "exporting a Web Mercator overview produced an EMPTY archive \
         (addressed_tiles_count = 0)"
    );

    // (3) Correct geographic placement: decode the archive back to lon/lat and
    //     check the features landed where the fixture put them.
    let decoded = dir.path().join("decoded.parquet");
    let report = decode_pmtiles(&archive, &decoded, &DecodeOptions::default()).expect("decode");
    assert!(
        report.features_written > 0,
        "archive decoded to zero features"
    );

    let (lon_min, lat_min, lon_max, lat_max) = decoded_bounds(&decoded);
    assert!(
        (lon_min - LON).abs() < 1.0 && (lon_max - LON).abs() < 1.5,
        "decoded longitudes {lon_min}..{lon_max} are not near {LON} — \
         metres were almost certainly read as degrees"
    );
    assert!(
        (lat_min - LAT).abs() < 1.0 && (lat_max - LAT).abs() < 1.5,
        "decoded latitudes {lat_min}..{lat_max} are not near {LAT}"
    );
}

/// lon/lat bounds of every vertex in a decoded GeoParquet file.
fn decoded_bounds(path: &Path) -> (f64, f64, f64, f64) {
    use geo::BoundingRect;
    use geoarrow::array::from_arrow_array;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use tylertoo_core::batch_processor::extract_geometries_from_array;

    let file = std::fs::File::open(path).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let schema = builder.schema().clone();
    let gidx = schema.index_of("geometry").unwrap();
    let gfield = schema.field(gidx).clone();
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for batch in builder.build().unwrap() {
        let batch = batch.unwrap();
        let arr = from_arrow_array(batch.column(gidx).as_ref(), &gfield).unwrap();
        let mut geoms: Vec<Geometry<f64>> = Vec::new();
        extract_geometries_from_array(arr.as_ref(), &mut geoms).unwrap();
        for g in &geoms {
            if let Some(r) = g.bounding_rect() {
                x0 = x0.min(r.min().x);
                y0 = y0.min(r.min().y);
                x1 = x1.max(r.max().x);
                y1 = y1.max(r.max().y);
            }
        }
    }
    (x0, y0, x1, y1)
}
