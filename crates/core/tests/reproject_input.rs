//! Input in a projected CRS is reprojected to EPSG:4326 as it is read (#670).
//!
//! The fixture is the issue's: two adjacent 1 km squares at the NL–BE border,
//! one in EPSG:28992 and one in EPSG:31370. Read as one dataset, they must
//! give what their EPSG:4326 twins give: two features within
//! [4.7927, 51.4410, 4.8072, 51.4590].
#![expect(
    clippy::disallowed_methods,
    reason = "#779: trusted fixture construction"
)]

use std::path::Path;
use std::sync::Arc;

use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geo::{BoundingRect, Geometry, Rect};
use geoarrow::array::GeometryBuilder;
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_io::test_support::ArrowWriter;

/// The issue's acceptance bbox for the two squares, in degrees.
const ACCEPT: [f64; 4] = [4.7927, 51.4410, 4.8072, 51.4590];

/// The generator's squares: (file stem, EPSG code, CRS name, native bounds).
const SQUARES: [(&str, u32, &str, [f64; 4]); 2] = [
    (
        "nl-28992",
        28992,
        "Amersfoort / RD New",
        [
            113_682.264_567,
            384_710.303_691,
            114_682.264_567,
            385_710.303_691,
        ],
    ),
    (
        "be-31370",
        31370,
        "BD72 / Belgian Lambert 72",
        [
            179_480.369_423,
            236_905.310_413,
            180_480.369_423,
            237_905.310_413,
        ],
    ),
];

/// Write `rects` (one row group each) as WKB GeoParquet with a bbox covering
/// and a PROJJSON CRS carrying `epsg` as its id.
fn write_input(path: &Path, epsg: u32, name: &str, rects: &[[f64; 4]]) {
    let projjson = serde_json::json!({
        "type": "ProjectedCRS",
        "name": name,
        "id": { "authority": "EPSG", "code": epsg }
    });
    let md =
        geoarrow::datatypes::Metadata::new(geoarrow::datatypes::Crs::from_projjson(projjson), None);
    let typ = GeometryType::new(Arc::new(md));
    let file = std::fs::File::create(path).unwrap();
    let options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(true)
        .build();
    let mut encoder = None;
    let mut writer = None;
    for (i, r) in rects.iter().enumerate() {
        let geom = Geometry::Polygon(Rect::new((r[0], r[1]), (r[2], r[3])).to_polygon());
        let mut b = GeometryBuilder::new(typ.clone()).with_prefer_multi(false);
        b.extend_from_iter(std::iter::once(Some(&geom)));
        let arr = b.finish();
        let schema = Arc::new(Schema::new(vec![
            Arc::new(Field::new("id", DataType::Utf8, false)),
            Arc::new(arr.data_type().to_field("geometry", true)),
        ]));
        let id = format!("{}-{i}", path.file_stem().unwrap().to_string_lossy());
        let columns: Vec<ArrayRef> =
            vec![Arc::new(StringArray::from(vec![id])), arr.to_array_ref()];
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        let enc = encoder.get_or_insert_with(|| {
            GeoParquetRecordBatchEncoder::try_new(&schema, &options).unwrap()
        });
        let w = writer.get_or_insert_with(|| {
            ArrowWriter::try_new(file.try_clone().unwrap(), enc.target_schema(), None).unwrap()
        });
        w.write(&enc.encode_record_batch(&batch).unwrap()).unwrap();
        w.flush().unwrap();
    }
    let mut w = writer.unwrap();
    w.append_key_value_metadata(encoder.unwrap().into_keyvalue().unwrap());
    w.close().unwrap();
}

/// Bounds of every geometry in an overview file, and its feature ids.
fn overview_bounds(path: &Path) -> ([f64; 4], Vec<String>) {
    use geoarrow::array::from_arrow_array;
    use tylertoo_core::internal::batch_processor::extract_geometries_from_array;

    let builder = tylertoo_io::open_parquet(std::fs::File::open(path).unwrap()).unwrap();
    let schema = builder.schema().clone();
    let (gidx, idx) = (
        schema.index_of("geometry").unwrap(),
        schema.index_of("id").unwrap(),
    );
    let gfield = schema.field(gidx).clone();
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    let mut ids = Vec::new();
    for batch in builder.build().unwrap() {
        let batch = batch.unwrap();
        let arr = from_arrow_array(batch.column(gidx).as_ref(), &gfield).unwrap();
        let mut geoms: Vec<Geometry<f64>> = Vec::new();
        extract_geometries_from_array(
            arr.as_ref(),
            &mut geoms,
            &tylertoo_core::internal::diagnostics::Diagnostics::default(),
        )
        .unwrap();
        for r in geoms.iter().filter_map(BoundingRect::bounding_rect) {
            b = [
                b[0].min(r.min().x),
                b[1].min(r.min().y),
                b[2].max(r.max().x),
                b[3].max(r.max().y),
            ];
        }
        let col = batch.column(idx);
        let col = col.as_any().downcast_ref::<StringArray>().unwrap();
        ids.extend(col.iter().flatten().map(str::to_owned));
    }
    ids.sort();
    ids.dedup();
    (b, ids)
}

fn options() -> ConvertOptions {
    ConvertOptions::default().with_levels(LevelPlan::ZoomRange {
        min_zoom: 10,
        max_zoom: 14,
    })
}

#[test]
fn parts_in_different_projected_crss_convert_as_their_4326_twins() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = dir.path().join("native");
    std::fs::create_dir(&inputs).unwrap();
    for (stem, epsg, name, rect) in SQUARES {
        write_input(&inputs.join(format!("{stem}.parquet")), epsg, name, &[rect]);
    }
    let overview = dir.path().join("ov.parquet");
    let report = convert_to_overviews(&inputs, &overview, &options())
        .expect("parts in EPSG:28992 and EPSG:31370 must convert");
    assert_eq!(report.input_features, 2);

    let (b, ids) = overview_bounds(&overview);
    assert_eq!(ids, ["be-31370-0", "nl-28992-0"]);
    for (got, want) in b.iter().zip(ACCEPT) {
        assert!((got - want).abs() < 1e-4, "bounds {b:?}, want {ACCEPT:?}");
    }
}

#[test]
fn bbox_prunes_projected_row_groups_in_degrees() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("nl.parquet");
    let nl = SQUARES[0].3;
    // A second square 50 km east, in its own row group.
    let far = [nl[0] + 50_000.0, nl[1], nl[2] + 50_000.0, nl[3]];
    write_input(&input, 28992, SQUARES[0].2, &[nl, far]);
    let overview = dir.path().join("ov.parquet");
    let mut opts = options();
    opts.bbox = Some([4.79, 51.449, 4.81, 51.46]);
    let report = convert_to_overviews(&input, &overview, &opts).unwrap();
    assert_eq!((report.row_groups_total, report.row_groups_read), (2, 1));
    assert_eq!(overview_bounds(&overview).1, ["nl-0"]);
}

#[test]
fn a_crs_without_an_epsg_code_is_refused_naming_gpio() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.parquet");
    write_input(&input, 999_999, "Somewhere / Custom", &[SQUARES[0].3]);
    let err = convert_to_overviews(&input, dir.path().join("ov.parquet"), &options())
        .unwrap_err()
        .to_string();
    assert!(err.contains("EPSG:999999") && err.contains("gpio"), "{err}");
}

#[test]
fn a_projected_geoparquet_mask_is_reprojected_too() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = dir.path().join("native");
    std::fs::create_dir(&inputs).unwrap();
    for (stem, epsg, name, rect) in SQUARES {
        write_input(&inputs.join(format!("{stem}.parquet")), epsg, name, &[rect]);
    }
    // A 200 m square inside the NL square, in EPSG:28992.
    let nl = SQUARES[0].3;
    let mask = dir.path().join("mask.parquet");
    let inner = [nl[0] + 400.0, nl[1] + 400.0, nl[0] + 600.0, nl[1] + 600.0];
    write_input(&mask, 28992, SQUARES[0].2, &[inner]);
    let overview = dir.path().join("ov.parquet");
    let mut opts = options();
    opts.mask = Some(tylertoo_core::MaskSource::Path(mask));
    let report = convert_to_overviews(&inputs, &overview, &opts).unwrap();
    assert_eq!(report.input_features, 1);
    assert_eq!(overview_bounds(&overview).1, ["nl-28992-0"]);
}

/// Web Mercator meters for a lon/lat point (the spherical EPSG:3857 formulas).
fn mercator(lon: f64, lat: f64) -> (f64, f64) {
    const R: f64 = 6_378_137.0;
    let y = (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
        .tan()
        .ln();
    (R * lon.to_radians(), R * y)
}

#[test]
fn epsg_3857_parts_mixed_with_other_crss_are_reprojected() {
    let dir = tempfile::tempdir().unwrap();
    let inputs = dir.path().join("mixed");
    std::fs::create_dir(&inputs).unwrap();
    let (stem, epsg, name, rect) = SQUARES[0];
    write_input(&inputs.join(format!("{stem}.parquet")), epsg, name, &[rect]);
    let want = [4.795, 51.445, 4.8, 51.45];
    let (x0, y0) = mercator(want[0], want[1]);
    let (x1, y1) = mercator(want[2], want[3]);
    let wm = inputs.join("wm-3857.parquet");
    write_input(&wm, 3857, "WGS 84 / Pseudo-Mercator", &[[x0, y0, x1, y1]]);

    let overview = dir.path().join("ov.parquet");
    let report = convert_to_overviews(&inputs, &overview, &options())
        .expect("an EPSG:3857 part must mix with an EPSG:28992 one");
    assert_eq!(report.input_features, 2);
    let (b, ids) = overview_bounds(&overview);
    assert_eq!(ids, ["nl-28992-0", "wm-3857-0"]);
    assert!(
        b.iter().all(|v| v.abs() < 90.0),
        "lon/lat output, got {b:?}"
    );

    // Alone, the EPSG:3857 part keeps its native path: output in meters.
    let alone = dir.path().join("alone.parquet");
    convert_to_overviews(&wm, &alone, &options()).unwrap();
    let (b, _) = overview_bounds(&alone);
    for (got, want) in b.iter().zip([x0, y0, x1, y1]) {
        assert!((got - want).abs() < 1.0, "bounds {b:?}");
    }
}
