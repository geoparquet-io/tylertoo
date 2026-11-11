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
use geoarrow::array::{GeometryBuilder, PolygonBuilder};
use geoarrow::datatypes::{Dimension, GeometryType, PolygonType};
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
    let groups: Vec<Vec<Option<Geometry>>> = rects
        .iter()
        .map(|r| {
            vec![Some(Geometry::Polygon(
                Rect::new((r[0], r[1]), (r[2], r[3])).to_polygon(),
            ))]
        })
        .collect();
    write_groups(path, epsg, name, GeoParquetWriterEncoding::WKB, &groups);
}

/// Write each of `groups` as one row group, in `encoding`, with ids
/// `<stem>-<row>` counted across the file.
fn write_groups(
    path: &Path,
    epsg: u32,
    name: &str,
    encoding: GeoParquetWriterEncoding,
    groups: &[Vec<Option<Geometry>>],
) {
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
        .set_encoding(encoding)
        .set_generate_covering(true)
        .build();
    let mut encoder = None;
    let mut writer = None;
    let mut row = 0;
    for geoms in groups {
        let arr: Arc<dyn GeoArrowArray> = if matches!(encoding, GeoParquetWriterEncoding::WKB) {
            let mut b = GeometryBuilder::new(typ.clone()).with_prefer_multi(false);
            b.extend_from_iter(geoms.iter().map(Option::as_ref));
            Arc::new(b.finish())
        } else {
            // Native encoding needs one geometry type: polygons.
            let polys: Vec<Option<geo::Polygon>> = geoms
                .iter()
                .map(|g| g.clone().map(|g| g.try_into().unwrap()))
                .collect();
            let ptyp = PolygonType::new(Dimension::XY, typ.metadata().clone());
            Arc::new(PolygonBuilder::from_nullable_polygons(&polys, ptyp).finish())
        };
        let schema = Arc::new(Schema::new(vec![
            Arc::new(Field::new("id", DataType::Utf8, false)),
            Arc::new(arr.data_type().to_field("geometry", true)),
        ]));
        let stem = path.file_stem().unwrap().to_string_lossy();
        let ids: Vec<String> = (row..row + geoms.len())
            .map(|i| format!("{stem}-{i}"))
            .collect();
        row += geoms.len();
        let columns: Vec<ArrayRef> = vec![Arc::new(StringArray::from(ids)), arr.to_array_ref()];
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

/// The reader threads reproject on rayon. A caller whose conversion runs on
/// the only worker of the global pool must still finish: that worker waits
/// on the readers, so their work cannot be queued behind it (#868).
#[test]
fn projected_input_converts_on_a_one_thread_global_pool() {
    // nextest runs every test in its own process, so this is the global pool.
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("nl.parquet");
    let nl = SQUARES[0].3;
    // More rows than one rayon run, so a batch's rewrite is split.
    let rects: Vec<[f64; 4]> = (0..1200)
        .map(|i| {
            let dx = 10.0 * f64::from(i);
            [nl[0] + dx, nl[1], nl[2] + dx, nl[3]]
        })
        .collect();
    write_input(&input, 28992, SQUARES[0].2, &rects);
    let output = dir.path().join("ov.parquet");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let opts = options().with_read_workers(2);
        let report = rayon::scope(|_| convert_to_overviews(&input, &output, &opts));
        let _ = tx.send(report.map(|r| r.input_features).map_err(|e| e.to_string()));
    });
    let features = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("deadlocked on the one-thread global pool");
    assert_eq!(features, Ok(1200));
}

/// `--layer-column` stages projected input in lon/lat (#670): `--bbox`
/// gates in degrees, and no layer reads the source-CRS covering.
#[test]
fn layer_column_stages_projected_input_in_lon_lat() {
    use tylertoo_core::internal::archive_index::ArchiveIndex;
    use tylertoo_core::internal::overview::tiles::{
        convert_to_pmtiles, TilesInput, TilesOptions, TilesOutcome,
    };

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("nl.parquet");
    let nl = SQUARES[0].3;
    let far = [nl[0] + 50_000.0, nl[1], nl[2] + 50_000.0, nl[3]];
    write_input(&input, 28992, SQUARES[0].2, &[nl, far]);
    let out = dir.path().join("o.pmtiles");
    let mut tiles = TilesOptions::default();
    tiles.layer_column = Some("id".into());
    tiles.convert = options();
    tiles.convert.bbox = Some([4.79, 51.449, 4.81, 51.46]);
    let outcome = convert_to_pmtiles(&TilesInput::Path(input), &out, &tiles).unwrap();
    let TilesOutcome::Layered(report) = outcome else {
        panic!("expected a layered outcome, got {outcome:?}");
    };
    assert_eq!(report.filtered_rows, 1);
    let layers: Vec<&str> = report.layers.iter().map(|l| l.layer.as_str()).collect();
    assert_eq!(layers, ["nl-0"]);
    let diagnostics = format!("{:?}", report.layers[0].report.convert.diagnostics);
    assert!(!diagnostics.contains("Covering"), "{diagnostics}");
    let meta = ArchiveIndex::open(&out).unwrap().metadata_json().unwrap();
    assert!(
        !meta.to_string().contains("xmin"),
        "covering exported: {meta}"
    );
}

/// Native GeoArrow input takes the buffer rewrite, not the WKB one: a
/// polygon with a hole (nested ring offsets), a null row, and batches the
/// reader slices out of a multi-row row group must all come out in degrees.
#[test]
fn native_geoarrow_input_is_reprojected_end_to_end() {
    use geo::{Contains, Polygon};

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("nl.parquet");
    let nl = SQUARES[0].3;
    let ring = |r: [f64; 4]| {
        Rect::new((r[0], r[1]), (r[2], r[3]))
            .to_polygon()
            .exterior()
            .clone()
    };
    let hole = [nl[0] + 250.0, nl[1] + 250.0, nl[2] - 250.0, nl[3] - 250.0];
    let holed = Geometry::Polygon(Polygon::new(ring(nl), vec![ring(hole)]));
    let east = [nl[0] + 2_000.0, nl[1], nl[2] + 2_000.0, nl[3]];
    let plain = Geometry::Polygon(Rect::new((east[0], east[1]), (east[2], east[3])).to_polygon());
    let groups = [vec![Some(holed), None, Some(plain)], vec![None]];
    let wkb = dir.path().join("wkb");
    std::fs::create_dir(&wkb).unwrap();
    let wkb_input = wkb.join("nl.parquet");
    for (path, encoding) in [
        (&input, GeoParquetWriterEncoding::GeoArrow),
        (&wkb_input, GeoParquetWriterEncoding::WKB),
    ] {
        write_groups(path, 28992, SQUARES[0].2, encoding, &groups);
    }
    let builder = tylertoo_io::open_parquet(std::fs::File::open(&input).unwrap()).unwrap();
    let gtype = builder
        .schema()
        .field_with_name("geometry")
        .unwrap()
        .data_type()
        .clone();
    assert!(
        !matches!(
            gtype,
            DataType::Binary | DataType::LargeBinary | DataType::BinaryView
        ),
        "fixture must be native GeoArrow, got {gtype:?}"
    );

    // Batches of one row make the reader slice the three-row row group.
    let opts = options().with_read_batch_size(1);
    let overview = dir.path().join("ov.parquet");
    let report = convert_to_overviews(&input, &overview, &opts).unwrap();
    assert_eq!(report.input_features, 2);
    let (b, ids) = overview_bounds(&overview);
    assert_eq!(ids, ["nl-0", "nl-2"]);
    assert!(b[2] < 6.0 && b[3] < 52.0, "lon/lat output, got {b:?}");
    // The WKB rewrite of the same rows is the oracle.
    let wkb_overview = dir.path().join("wkb.parquet");
    convert_to_overviews(&wkb_input, &wkb_overview, &opts).unwrap();
    let (want, _) = overview_bounds(&wkb_overview);
    for (got, want) in b.iter().zip(want) {
        assert!((got - want).abs() < 1e-9, "native {b:?}, WKB {want:?}");
    }

    // The hole survives: its centre, in degrees, is inside no output polygon.
    // The NL square spans about 0.0144° of longitude and 0.009° of latitude
    // from its south-west corner, `b[0..2]`; the hole is its middle half.
    let centre = geo::Point::new(b[0] + 0.0072, b[1] + 0.0045);
    let rim = geo::Point::new(b[0] + 0.001, b[1] + 0.001);
    let builder = tylertoo_io::open_parquet(std::fs::File::open(&overview).unwrap()).unwrap();
    let gidx = builder.schema().index_of("geometry").unwrap();
    let gfield = builder.schema().field(gidx).clone();
    let mut geoms = Vec::new();
    for batch in builder.build().unwrap() {
        let arr = geoarrow::array::from_arrow_array(batch.unwrap().column(gidx).as_ref(), &gfield)
            .unwrap();
        tylertoo_core::internal::batch_processor::extract_geometries_from_array(
            arr.as_ref(),
            &mut geoms,
            &tylertoo_core::internal::diagnostics::Diagnostics::default(),
        )
        .unwrap();
    }
    assert!(!geoms.is_empty());
    assert!(geoms.iter().any(|g| g.contains(&rim)), "square missing");
    assert!(geoms.iter().all(|g| !g.contains(&centre)), "hole filled");
}

/// Cell bands count projected input in degrees: each row's cell is picked
/// from its reprojected centroid, so both NL squares land in the grid cell
/// that holds (4.80°, 51.45°), not in one picked from raw meters.
#[test]
fn cell_bands_count_projected_input_in_lon_lat() {
    use arrow_array::{cast::AsArray, Array};
    use tylertoo_core::internal::overview::tiles::{convert_to_pmtiles, TilesInput, TilesOptions};
    use tylertoo_core::{decode_pmtiles, CellBand, CellSystem, DecodeOptions};

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("nl.parquet");
    let nl = SQUARES[0].3;
    let near = [nl[0] + 2_000.0, nl[1], nl[2] + 2_000.0, nl[3]];
    write_input(&input, 28992, SQUARES[0].2, &[nl, near]);
    let out = dir.path().join("o.pmtiles");
    let mut tiles = TilesOptions::default();
    tiles.convert = ConvertOptions::default().with_levels(LevelPlan::ZoomRange {
        min_zoom: 8,
        max_zoom: 10,
    });
    tiles.cell_bands = vec![CellBand::new(0, 7, CellSystem::Grid)];
    convert_to_pmtiles(&TilesInput::Path(input), &out, &tiles).unwrap();

    let decoded = dir.path().join("decoded.parquet");
    decode_pmtiles(&out, &decoded, &DecodeOptions::default()).unwrap();
    let batch = {
        let reader = tylertoo_io::open_parquet(std::fs::File::open(&decoded).unwrap())
            .unwrap()
            .build()
            .unwrap();
        let batches: Vec<_> = reader.map(Result::unwrap).collect();
        arrow_select::concat::concat_batches(&batches[0].schema(), &batches).unwrap()
    };
    let col = |name: &str| batch.column(batch.schema().index_of(name).unwrap()).clone();
    let zoom = arrow_cast::cast(&col("zoom"), &DataType::Float64).unwrap();
    let zoom = zoom.as_primitive::<arrow_array::types::Float64Type>();
    let count = arrow_cast::cast(&col("point_count"), &DataType::Float64).unwrap();
    let count = count.as_primitive::<arrow_array::types::Float64Type>();
    let cell = col("cell");
    let cell = cell.as_string::<i32>();
    // A z0 grid cell is a web-mercator tile at zoom 6 (the band's zoom + 6).
    let (lon, lat) = (4.80_f64, 51.45_f64);
    let n = 64.0_f64;
    let x = ((lon + 180.0) / 360.0 * n).floor();
    let y = ((1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n).floor();
    let want = format!("6/{x}/{y}");
    let z0: Vec<(String, f64)> = (0..batch.num_rows())
        .filter(|&r| zoom.value(r) == 0.0 && cell.is_valid(r))
        .map(|r| (cell.value(r).to_string(), count.value(r)))
        .collect();
    assert_eq!(z0, [(want, 2.0)], "z0 cells");
}
