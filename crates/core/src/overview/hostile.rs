//! Hostile-input hardening tests for the overview pipeline (issue H4).
//!
//! Every input class from the H4 checklist gets a synthetic fixture and a
//! test asserting the pipeline either produces correct output or fails fast
//! with a typed, actionable error — never a panic, never silent wrong output.
//!
//! Fixtures are generated programmatically (no binary fixtures checked in).

use std::path::Path;
use std::sync::Arc;

use arrow_array::{Array, BinaryArray, Int32Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geo::{Geometry, GeometryCollection, LineString, Point, Polygon};
use geoarrow::array::GeometryBuilder;
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::file::metadata::KeyValue;

use super::check::validate_file;
use super::convert::{convert_to_overviews, ConvertError, ConvertOptions, LevelPlan};
use super::export::{export_pmtiles, ExportError, ExportOptions};
use super::level::Mode;
use super::reader::{OverviewReader, ReaderError};

// ============================================================================
// Fixture builders
// ============================================================================

/// Write a GeoParquet file with `id` (Int64), `name` (Utf8) property columns
/// and the given (possibly null) geometries. `covering` toggles bbox covering
/// generation; `extra_col` injects an additional Int32 column with the given
/// name (reserved-column rejection tests).
fn write_input(
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

/// Spread-out points that survive as distinct cell winners.
fn spread_points(n: usize) -> Vec<Option<Geometry<f64>>> {
    (0..n)
        .map(|i| {
            Some(Geometry::Point(Point::new(
                -60.0 + i as f64 * 20.0,
                -30.0 + i as f64 * 12.0,
            )))
        })
        .collect()
}

/// Default duplicating conversion options over a modest zoom range, with the
/// given streaming flag.
fn opts(streaming: bool) -> ConvertOptions {
    ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 2,
            max_zoom: 6,
        },
        streaming,
        ..Default::default()
    }
}

/// Read `(id, geometry)` pairs for a level of an overview file, in row order.
fn read_level_ids_geoms(path: &Path, level: usize) -> Vec<(i64, Geometry<f64>)> {
    use crate::batch_processor::extract_geometries_from_array;
    use arrow_array::cast::AsArray;
    use geoarrow::array::from_arrow_array;

    let reader = OverviewReader::open(path).unwrap();
    let rdr = reader.read_level(level, None).unwrap();
    let mut out = Vec::new();
    for batch in rdr {
        let batch = batch.unwrap();
        let schema = batch.schema();
        let ids = batch
            .column(schema.index_of("id").unwrap())
            .as_primitive::<arrow_array::types::Int64Type>()
            .clone();
        let gidx = schema.index_of("geometry").unwrap();
        let garr = from_arrow_array(batch.column(gidx).as_ref(), schema.field(gidx)).unwrap();
        let mut geoms = Vec::new();
        extract_geometries_from_array(garr.as_ref(), &mut geoms).unwrap();
        assert_eq!(
            geoms.len(),
            batch.num_rows(),
            "output level {level} contains null/undecodable geometry rows"
        );
        for (i, g) in geoms.into_iter().enumerate() {
            out.push((ids.value(i), g));
        }
    }
    out
}

/// Rewrite an overview file with a tampered `geo:overviews` footer value.
/// Batches and the `geo` key are copied verbatim; the file is written as a
/// single row group (which is itself a footer/data mismatch for multi-level
/// footers).
fn rewrite_with_tampered_footer(src: &Path, dst: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let file = std::fs::File::open(src).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let schema = builder.schema().clone();
    let kvs = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .unwrap()
        .clone();
    let batches: Vec<RecordBatch> = builder.build().unwrap().map(|b| b.unwrap()).collect();

    let out = std::fs::File::create(dst).unwrap();
    let mut writer = ArrowWriter::try_new(out, schema, None).unwrap();
    for b in &batches {
        writer.write(b).unwrap();
    }
    let mut edit = Some(edit);
    for kv in kvs {
        match kv.key.as_str() {
            "geo:overviews" => {
                let mut v: serde_json::Value = serde_json::from_str(&kv.value.unwrap()).unwrap();
                (edit.take().expect("single geo:overviews key"))(&mut v);
                writer.append_key_value_metadata(KeyValue::new(
                    "geo:overviews".to_string(),
                    serde_json::to_string(&v).unwrap(),
                ));
            }
            "geo" => writer.append_key_value_metadata(kv),
            _ => {}
        }
    }
    writer.close().unwrap();
}

/// Convert `spread_points` input to a valid overview file at `out`.
fn make_valid_overview(out: &Path) {
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(6), true, None);
    convert_to_overviews(tin.path(), out, &opts(true)).unwrap();
}

// ============================================================================
// Class 1: empty file (0 rows) / all-null geometry column
// ============================================================================

#[test]
fn empty_input_zero_rows_errors_nodata() {
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &[], true, None);
        let err = convert_to_overviews(tin.path(), tout.path(), &opts(streaming)).unwrap_err();
        assert!(
            matches!(err, ConvertError::NoData),
            "streaming={streaming}: expected NoData, got: {err}"
        );
    }
}

#[test]
fn all_null_geometry_errors_nodata() {
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &[None, None, None], true, None);
        let err = convert_to_overviews(tin.path(), tout.path(), &opts(streaming)).unwrap_err();
        assert!(
            matches!(err, ConvertError::NoData),
            "streaming={streaming}: expected NoData, got: {err}"
        );
    }
}

#[test]
fn partial_null_geometry_rows_skipped_with_aligned_attributes() {
    // Null geometry rows interleaved with valid ones must be skipped WITHOUT
    // shifting the attribute<->geometry pairing of the surviving rows.
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        let mut geoms = spread_points(5);
        geoms.insert(1, None); // id 1 null
        geoms.insert(4, None); // id 4 null
        write_input(tin.path(), &geoms, true, None);

        let report = convert_to_overviews(tin.path(), tout.path(), &opts(streaming))
            .unwrap_or_else(|e| panic!("streaming={streaming}: conversion failed: {e}"));
        assert_eq!(report.input_features, 5, "streaming={streaming}");

        let vr = validate_file(tout.path()).unwrap();
        assert!(vr.is_valid(), "streaming={streaming}");

        // Canonical level: exactly the 5 non-null rows, each id paired with
        // ITS OWN geometry (regression: misalignment pairs id with the next
        // non-null row's geometry).
        let reader = OverviewReader::open(tout.path()).unwrap();
        let canonical = reader.num_levels() - 1;
        let rows = read_level_ids_geoms(tout.path(), canonical);
        let expected_ids: Vec<i64> = vec![0, 2, 3, 5, 6];
        assert_eq!(
            rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            expected_ids,
            "streaming={streaming}"
        );
        for (id, g) in &rows {
            let Geometry::Point(p) = g else {
                panic!("expected point");
            };
            // Original spread_points index for this id (nulls at 1 and 4).
            let orig = match id {
                0 => 0,
                2 => 1,
                3 => 2,
                5 => 3,
                6 => 4,
                _ => unreachable!(),
            };
            let expected = Point::new(-60.0 + orig as f64 * 20.0, -30.0 + orig as f64 * 12.0);
            assert_eq!(p, &expected, "streaming={streaming}: id {id}");
        }
    }
}

// ============================================================================
// Class 2: invalid / degenerate source geometries
// ============================================================================

#[test]
fn nonfinite_coordinate_rows_skipped() {
    // NaN / infinite coordinates cannot be placed on any grid: those rows are
    // skipped like nulls instead of silently landing in cell (0, 0).
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        let mut geoms = spread_points(4);
        geoms.push(Some(Geometry::Point(Point::new(f64::NAN, 1.0))));
        geoms.push(Some(Geometry::Point(Point::new(2.0, f64::INFINITY))));
        // Covering generation over NaN bboxes is itself hostile; skip it.
        write_input(tin.path(), &geoms, false, None);

        let report = convert_to_overviews(tin.path(), tout.path(), &opts(streaming))
            .unwrap_or_else(|e| panic!("streaming={streaming}: conversion failed: {e}"));
        assert_eq!(report.input_features, 4, "streaming={streaming}");

        let reader = OverviewReader::open(tout.path()).unwrap();
        let canonical = reader.num_levels() - 1;
        let rows = read_level_ids_geoms(tout.path(), canonical);
        assert_eq!(
            rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![0, 1, 2, 3],
            "streaming={streaming}"
        );
        for (_, g) in &rows {
            let Geometry::Point(p) = g else {
                panic!("expected point")
            };
            assert!(
                p.x().is_finite() && p.y().is_finite(),
                "streaming={streaming}: non-finite geometry leaked into output"
            );
        }
    }
}

#[test]
fn empty_coordinate_geometries_skipped() {
    // A LineString with zero coordinates has no spatial content; it is
    // skipped like a null rather than parked at a fabricated [0,0,0,0] bbox.
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        let mut geoms = spread_points(3);
        geoms.push(Some(Geometry::LineString(LineString::new(vec![]))));
        write_input(tin.path(), &geoms, false, None);

        let report = convert_to_overviews(tin.path(), tout.path(), &opts(streaming))
            .unwrap_or_else(|e| panic!("streaming={streaming}: conversion failed: {e}"));
        assert_eq!(report.input_features, 3, "streaming={streaming}");
        let reader = OverviewReader::open(tout.path()).unwrap();
        let canonical = reader.num_levels() - 1;
        let rows = read_level_ids_geoms(tout.path(), canonical);
        assert_eq!(
            rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "streaming={streaming}"
        );
    }
}

#[test]
fn empty_wkb_value_errors_typed() {
    // A zero-length WKB value is undecodable: the conversion must surface a
    // typed error (never a panic).
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();

    // Hand-built GeoParquet: Binary geometry column with one empty value.
    let mut md = std::collections::HashMap::new();
    md.insert(
        "ARROW:extension:name".to_string(),
        "geoarrow.wkb".to_string(),
    );
    let geom_field = Field::new("geometry", DataType::Binary, true).with_metadata(md);
    let schema = Arc::new(Schema::new(vec![
        Arc::new(Field::new("id", DataType::Int64, false)),
        Arc::new(geom_field),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(vec![0i64])),
            Arc::new(BinaryArray::from_vec(vec![b"" as &[u8]])),
        ],
    )
    .unwrap();
    let file = std::fs::File::create(tin.path()).unwrap();
    let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
    writer.write(&batch).unwrap();
    writer.append_key_value_metadata(KeyValue::new(
        "geo".to_string(),
        r#"{"version":"1.1.0","primary_column":"geometry","columns":{"geometry":{"encoding":"WKB","geometry_types":[]}}}"#
            .to_string(),
    ));
    writer.close().unwrap();

    for streaming in [true, false] {
        let result = convert_to_overviews(tin.path(), tout.path(), &opts(streaming));
        assert!(
            result.is_err(),
            "streaming={streaming}: empty WKB must error"
        );
    }
}

#[test]
fn self_intersecting_polygon_converts() {
    // A bowtie (self-intersecting ring) is structurally valid WKB; the
    // pipeline carries it through rather than crashing on it.
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    let bowtie = Polygon::new(
        LineString::from(vec![
            (0.0, 0.0),
            (20.0, 20.0),
            (20.0, 0.0),
            (0.0, 20.0),
            (0.0, 0.0),
        ]),
        vec![],
    );
    let mut geoms = spread_points(3);
    geoms.push(Some(Geometry::Polygon(bowtie)));
    write_input(tin.path(), &geoms, true, None);

    let report = convert_to_overviews(tin.path(), tout.path(), &opts(true)).unwrap();
    assert_eq!(report.input_features, 4);
    let vr = validate_file(tout.path()).unwrap();
    assert!(vr.is_valid());
}

// ============================================================================
// Class 3: mixed geometry types / GeometryCollections
// ============================================================================

#[test]
fn geometry_collection_passes_through() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    let gc = GeometryCollection::from(vec![
        Geometry::Point(Point::new(10.0, 10.0)),
        Geometry::LineString(LineString::from(vec![(11.0, 10.0), (12.0, 11.0)])),
    ]);
    let mut geoms = spread_points(3);
    geoms.push(Some(Geometry::GeometryCollection(gc)));
    write_input(tin.path(), &geoms, true, None);

    for streaming in [true, false] {
        let report = convert_to_overviews(tin.path(), tout.path(), &opts(streaming))
            .unwrap_or_else(|e| panic!("streaming={streaming}: conversion failed: {e}"));
        assert_eq!(report.input_features, 4, "streaming={streaming}");
        let reader = OverviewReader::open(tout.path()).unwrap();
        let canonical = reader.num_levels() - 1;
        let rows = read_level_ids_geoms(tout.path(), canonical);
        assert!(
            rows.iter()
                .any(|(_, g)| matches!(g, Geometry::GeometryCollection(_))),
            "streaming={streaming}: GeometryCollection lost from canonical level"
        );
    }
}

// ============================================================================
// Class 4: antimeridian-crossing / pole-adjacent geometries
// ============================================================================

#[test]
fn antimeridian_and_pole_features_convert() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    let geoms: Vec<Option<Geometry<f64>>> = vec![
        Some(Geometry::Point(Point::new(179.95, 0.0))),
        Some(Geometry::Point(Point::new(-179.95, 5.0))),
        Some(Geometry::Point(Point::new(0.0, 89.9))),
        Some(Geometry::Point(Point::new(0.0, -89.9))),
        // Raw antimeridian-crossing linestring (as stored: a long east-west line).
        Some(Geometry::LineString(LineString::from(vec![
            (179.5, 10.0),
            (-179.5, 10.5),
        ]))),
    ];
    write_input(tin.path(), &geoms, true, None);

    let report = convert_to_overviews(tin.path(), tout.path(), &opts(true)).unwrap();
    assert_eq!(report.input_features, 5);
    let vr = validate_file(tout.path()).unwrap();
    assert!(
        vr.is_valid(),
        "failures: {:?}",
        vr.failures().collect::<Vec<_>>()
    );
    let reader = OverviewReader::open(tout.path()).unwrap();
    let canonical = reader.num_levels() - 1;
    assert_eq!(read_level_ids_geoms(tout.path(), canonical).len(), 5);
}

// ============================================================================
// Class 5: degenerate extents
// ============================================================================

#[test]
fn single_point_dataset_converts() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(1), true, None);

    for streaming in [true, false] {
        let report = convert_to_overviews(tin.path(), tout.path(), &opts(streaming))
            .unwrap_or_else(|e| panic!("streaming={streaming}: conversion failed: {e}"));
        assert_eq!(report.input_features, 1, "streaming={streaming}");
        let vr = validate_file(tout.path()).unwrap();
        assert!(vr.is_valid(), "streaming={streaming}");
        // The single point survives at every emitted level.
        let reader = OverviewReader::open(tout.path()).unwrap();
        for l in 0..reader.num_levels() {
            assert_eq!(
                read_level_ids_geoms(tout.path(), l).len(),
                1,
                "streaming={streaming} level={l}"
            );
        }
    }
}

#[test]
fn all_identical_geometries_convert() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    let geoms: Vec<Option<Geometry<f64>>> = (0..10)
        .map(|_| Some(Geometry::Point(Point::new(7.5, 45.0))))
        .collect();
    write_input(tin.path(), &geoms, true, None);

    let report = convert_to_overviews(tin.path(), tout.path(), &opts(true)).unwrap();
    assert_eq!(report.input_features, 10);
    let vr = validate_file(tout.path()).unwrap();
    assert!(vr.is_valid());
    // Coarse levels keep exactly one cell winner; canonical keeps all 10.
    let reader = OverviewReader::open(tout.path()).unwrap();
    let canonical = reader.num_levels() - 1;
    assert_eq!(read_level_ids_geoms(tout.path(), 0).len(), 1);
    assert_eq!(read_level_ids_geoms(tout.path(), canonical).len(), 10);
}

#[test]
fn extent_smaller_than_finest_gsd_converts() {
    // All features within ~100 m of each other, converted over a coarse zoom
    // range whose finest GSD is ~2.4 km: everything lands in one cell per
    // level, so each coarse level has one winner and canonical has all rows.
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    let geoms: Vec<Option<Geometry<f64>>> = (0..5)
        .map(|i| {
            Some(Geometry::Point(Point::new(
                10.0 + i as f64 * 0.0002,
                50.0 + i as f64 * 0.0002,
            )))
        })
        .collect();
    write_input(tin.path(), &geoms, true, None);

    let o = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 4,
        },
        ..Default::default()
    };
    let report = convert_to_overviews(tin.path(), tout.path(), &o).unwrap();
    assert_eq!(report.input_features, 5);
    let vr = validate_file(tout.path()).unwrap();
    assert!(vr.is_valid());
    let reader = OverviewReader::open(tout.path()).unwrap();
    let canonical = reader.num_levels() - 1;
    assert_eq!(read_level_ids_geoms(tout.path(), 0).len(), 1);
    assert_eq!(read_level_ids_geoms(tout.path(), canonical).len(), 5);
}

// ============================================================================
// Class 6: absurd knob combos
// ============================================================================

#[test]
fn min_zoom_greater_than_max_zoom_rejected() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, None);
    let o = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 8,
            max_zoom: 2,
        },
        ..Default::default()
    };
    let err = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_err();
    assert!(matches!(err, ConvertError::InvalidLevels(_)), "got: {err}");
}

#[test]
fn forty_plus_zoom_levels_convert() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(4), true, None);
    let o = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 45,
        },
        ..Default::default()
    };
    let report = convert_to_overviews(tin.path(), tout.path(), &o).unwrap();
    assert_eq!(report.input_features, 4);
    let vr = validate_file(tout.path()).unwrap();
    assert!(vr.is_valid());
}

#[test]
fn more_than_255_levels_rejected() {
    // The per-feature level table is u8-indexed; plans beyond 255 levels are
    // rejected up front instead of silently wrapping.
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, None);
    let gsds: Vec<f64> = (0..300).map(|i| 1.0e6 * 0.99f64.powi(i)).collect();
    let o = ConvertOptions {
        levels: LevelPlan::Gsds(gsds),
        ..Default::default()
    };
    let err = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_err();
    assert!(matches!(err, ConvertError::InvalidLevels(_)), "got: {err}");
}

#[test]
fn gsd_base_extremes_rejected() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, None);
    for bad in [0.0, -1024.0, f64::NAN, f64::INFINITY] {
        let o = ConvertOptions {
            gsd_base: bad,
            ..opts(true)
        };
        let err = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_err();
        assert!(
            matches!(err, ConvertError::InvalidConfig(_)),
            "gsd_base={bad}: got: {err}"
        );
    }
}

#[test]
fn thinning_zero_negative_nan_rejected() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, None);
    for bad in [0.0, -4.0, f64::NAN, f64::INFINITY] {
        for knob in 0..3 {
            let mut o = opts(true);
            match knob {
                0 => o.assign.point_thinning = bad,
                1 => o.assign.line_thinning = bad,
                _ => o.assign.polygon_thinning = bad,
            }
            let err = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_err();
            assert!(
                matches!(err, ConvertError::InvalidConfig(_)),
                "thinning knob {knob}={bad}: got: {err}"
            );
        }
    }
}

#[test]
fn visibility_negative_nan_rejected() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, None);
    for bad in [-2.0, f64::NAN, f64::INFINITY] {
        for knob in 0..2 {
            let mut o = opts(true);
            match knob {
                0 => o.assign.line_visibility = bad,
                _ => o.assign.polygon_visibility = bad,
            }
            let err = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_err();
            assert!(
                matches!(err, ConvertError::InvalidConfig(_)),
                "visibility knob {knob}={bad}: got: {err}"
            );
        }
    }
}

#[test]
fn coalesce_nan_knobs_rejected() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, None);
    let o = ConvertOptions {
        coalesce_snap: f64::NAN,
        ..opts(true)
    };
    let err = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_err();
    assert!(matches!(err, ConvertError::InvalidConfig(_)), "got: {err}");

    let o = ConvertOptions {
        coalesce_junction_angle: f64::NAN,
        ..opts(true)
    };
    let err = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_err();
    assert!(matches!(err, ConvertError::InvalidConfig(_)), "got: {err}");
}

// ============================================================================
// Class 7: pre-existing reserved columns (verify-only; case-insensitive)
// ============================================================================

#[test]
fn reserved_columns_rejected_case_insensitive() {
    // `LEVEL` (any casing) is always rejected.
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, Some("LEVEL"));
    for streaming in [true, false] {
        let err = convert_to_overviews(tin.path(), tout.path(), &opts(streaming)).unwrap_err();
        assert!(
            matches!(err, ConvertError::LevelColumnPresent),
            "streaming={streaming}: got: {err}"
        );
    }

    // `Point_Count` is rejected when clustering is enabled.
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, Some("Point_Count"));
    let o = ConvertOptions {
        cluster: true,
        ..opts(true)
    };
    let err = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_err();
    assert!(
        matches!(err, ConvertError::PointCountColumnPresent),
        "got: {err}"
    );

    // `COALESCED_COUNT` is rejected when coalescing is enabled (the default).
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, Some("COALESCED_COUNT"));
    let err = convert_to_overviews(tin.path(), tout.path(), &opts(true)).unwrap_err();
    assert!(
        matches!(err, ConvertError::CoalescedCountColumnPresent),
        "got: {err}"
    );
}

// ============================================================================
// Class 8: zero-row levels after thinning (empty-level omission, all routes)
// ============================================================================

#[test]
fn empty_coarse_levels_omitted_across_pipelines() {
    // Tiny lines fail the visibility gate at every coarse level: with
    // coalescing off those levels are empty and must be omitted (not written,
    // not EmptyLevel-crashed), leaving a valid file with fewer levels.
    let tiny_lines: Vec<Option<Geometry<f64>>> = (0..4)
        .map(|i| {
            let x = 10.0 + i as f64 * 5.0;
            Some(Geometry::LineString(LineString::from(vec![
                (x, 20.0),
                (x + 0.00005, 20.00005),
            ])))
        })
        .collect();

    for streaming in [true, false] {
        for coalesce in [false, true] {
            let tin = tempfile::NamedTempFile::new().unwrap();
            let tout = tempfile::NamedTempFile::new().unwrap();
            write_input(tin.path(), &tiny_lines, true, None);
            let o = ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 10,
                },
                coalesce_lines: coalesce,
                streaming,
                ..Default::default()
            };
            let report = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_or_else(|e| {
                panic!("streaming={streaming} coalesce={coalesce}: failed: {e}")
            });
            assert!(
                !report.levels.is_empty() && report.levels.len() <= 11,
                "streaming={streaming} coalesce={coalesce}"
            );
            let vr = validate_file(tout.path()).unwrap();
            assert!(
                vr.is_valid(),
                "streaming={streaming} coalesce={coalesce}: {:?}",
                vr.failures().collect::<Vec<_>>()
            );
        }
    }
}

#[test]
fn empty_coarse_levels_omitted_with_clustering() {
    // Same omission contract on the clustering route: a single point dataset
    // over a wide zoom range keeps every level nonempty, while a clustered
    // conversion of points that all defer still validates.
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(5), true, None);
    let o = ConvertOptions {
        cluster: true,
        levels: LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 10,
        },
        ..Default::default()
    };
    let report = convert_to_overviews(tin.path(), tout.path(), &o).unwrap();
    assert_eq!(report.input_features, 5);
    let vr = validate_file(tout.path()).unwrap();
    assert!(
        vr.is_valid(),
        "failures: {:?}",
        vr.failures().collect::<Vec<_>>()
    );
}

// ============================================================================
// Class 9: export-pmtiles hostile inputs
// ============================================================================

#[test]
fn export_non_overview_parquet_errors() {
    // A plain GeoParquet file (no `geo:overviews` key) is rejected with the
    // typed reader error, not a panic.
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, None);
    let err = export_pmtiles(tin.path(), tout.path(), &ExportOptions::default()).unwrap_err();
    assert!(
        matches!(err, ExportError::Reader(ReaderError::MissingOverviewsKey)),
        "got: {err}"
    );
}

#[test]
fn export_truncated_file_errors() {
    let tovr = tempfile::NamedTempFile::new().unwrap();
    make_valid_overview(tovr.path());
    let len = std::fs::metadata(tovr.path()).unwrap().len();

    let ttrunc = tempfile::NamedTempFile::new().unwrap();
    let bytes = std::fs::read(tovr.path()).unwrap();
    std::fs::write(ttrunc.path(), &bytes[..(len as usize) / 2]).unwrap();

    let tout = tempfile::NamedTempFile::new().unwrap();
    let err = export_pmtiles(ttrunc.path(), tout.path(), &ExportOptions::default()).unwrap_err();
    assert!(matches!(err, ExportError::Reader(_)), "got: {err}");
}

#[test]
fn export_footer_data_mismatch_errors() {
    // Footer declares level bands that do not match the file's actual row
    // groups: the reader must reject the file on open instead of reading
    // wrong bands (or allocating from hostile row_group_end values).
    let tovr = tempfile::NamedTempFile::new().unwrap();
    make_valid_overview(tovr.path());

    // Out-of-range row_group_end.
    let tbad = tempfile::NamedTempFile::new().unwrap();
    rewrite_with_tampered_footer(tovr.path(), tbad.path(), |v| {
        let levels = v["levels"].as_array_mut().unwrap();
        let last = levels.len() - 1;
        levels[last]["row_group_end"] = serde_json::json!(999);
    });
    let tout = tempfile::NamedTempFile::new().unwrap();
    let err = export_pmtiles(tbad.path(), tout.path(), &ExportOptions::default()).unwrap_err();
    assert!(matches!(err, ExportError::Reader(_)), "got: {err}");
}

#[test]
fn reader_rejects_negative_row_group_end() {
    // A negative row_group_end would wrap through `as usize` into a huge band
    // range; the reader must reject it at open.
    let tovr = tempfile::NamedTempFile::new().unwrap();
    make_valid_overview(tovr.path());

    let tbad = tempfile::NamedTempFile::new().unwrap();
    rewrite_with_tampered_footer(tovr.path(), tbad.path(), |v| {
        v["levels"][0]["row_group_end"] = serde_json::json!(-1);
    });
    let err = OverviewReader::open(tbad.path()).unwrap_err();
    assert!(
        !matches!(err, ReaderError::MissingOverviewsKey),
        "wrong rejection: {err}"
    );
}

#[test]
fn export_partitioning_mode_file_works() {
    // Partitioning-mode overview files are a supported export source.
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tovr = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(6), true, None);
    let o = ConvertOptions {
        mode: Mode::Partitioning,
        ..opts(true)
    };
    convert_to_overviews(tin.path(), tovr.path(), &o).unwrap();
    let report = export_pmtiles(tovr.path(), tout.path(), &ExportOptions::default()).unwrap();
    assert!(report.total_tiles > 0);
}
