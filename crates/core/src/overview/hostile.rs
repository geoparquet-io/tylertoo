//! Hostile-input hardening tests for the overview pipeline (issue H4).
//!
//! Every input class from the H4 checklist gets a synthetic fixture and a
//! test asserting the pipeline either produces correct output or fails fast
//! with a typed, actionable error — never a panic, never silent wrong output.
//!
//! Fixtures are generated programmatically (no binary fixtures checked in).

use std::path::Path;
use std::sync::Arc;

use arrow_array::{BinaryArray, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use geo::{Geometry, GeometryCollection, LineString, Point, Polygon};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use parquet::file::metadata::KeyValue;

use super::check::validate_file;
use super::convert::{convert_to_overviews, ConvertError, ConvertOptions, LevelPlan};
use super::export::{export_pmtiles, ExportError, ExportOptions};
use super::level::Mode;
use super::reader::{OverviewReader, ReaderError};
use super::simplify::{CollapseMode, SimplifyOptions};
use super::testutil::write_input;

// ============================================================================
// Fixture builders
// ============================================================================

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

/// Negative, NaN and infinite thinning factors are meaningless and rejected.
///
/// `0` is NOT in this list any more: it is the documented off switch
/// (#345/#360, see [`zero_thinning_is_the_off_switch_not_an_error`]). It used
/// to be rejected because a zero cell size made the grid pass *skip* every
/// feature — the opposite of what a caller means by "no thinning" — which
/// forced a `1e-9` workaround. The grid pass now treats a zero factor as "every
/// feature is its own cell", so the value means what it reads like.
#[test]
fn thinning_negative_nan_rejected() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, None);
    for bad in [-4.0, f64::NAN, f64::INFINITY] {
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

    // Every kind's factor accepts 0 individually, not just via --verbatim.
    for knob in 0..3 {
        let mut o = opts(true);
        match knob {
            0 => o.assign.point_thinning = 0.0,
            1 => o.assign.line_thinning = 0.0,
            _ => o.assign.polygon_thinning = 0.0,
        }
        convert_to_overviews(tin.path(), tout.path(), &o)
            .unwrap_or_else(|e| panic!("thinning knob {knob}=0 must be accepted: {e}"));
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

/// Field names of an overview output file, in schema order.
fn output_column_names(path: &Path) -> Vec<String> {
    let file = std::fs::File::open(path).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    builder
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect()
}

#[test]
fn reserved_columns_auto_renamed_case_insensitive() {
    // #288: a source property colliding (case-insensitively) with a reserved
    // overview column is auto-renamed (suffix `_`), not rejected, so real-world
    // data — Overture buildings' `level`, admin `LEVEL` — converts out of the
    // box. The reserved output column stays authoritative. Verified for all
    // three reserved columns, across both pipelines.

    // `LEVEL` (any casing) is always reserved.
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &spread_points(3), true, Some("LEVEL"));
        convert_to_overviews(tin.path(), tout.path(), &opts(streaming))
            .unwrap_or_else(|e| panic!("streaming={streaming}: expected auto-rename, got {e}"));
        let names = output_column_names(tout.path());
        assert_eq!(
            names
                .iter()
                .filter(|n| n.eq_ignore_ascii_case("level"))
                .count(),
            1,
            "streaming={streaming}: one authoritative `level`, names={names:?}"
        );
        assert!(
            names.iter().any(|n| n == "LEVEL_"),
            "streaming={streaming}: renamed source column present, names={names:?}"
        );
    }

    // `Point_Count` is renamed when clustering is enabled.
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, Some("Point_Count"));
    let o = ConvertOptions {
        cluster: true,
        ..opts(true)
    };
    convert_to_overviews(tin.path(), tout.path(), &o).expect("Point_Count auto-renamed");
    let names = output_column_names(tout.path());
    assert!(
        names.iter().any(|n| n == "Point_Count_"),
        "renamed source column present, names={names:?}"
    );
    assert_eq!(
        names
            .iter()
            .filter(|n| n.eq_ignore_ascii_case("point_count"))
            .count(),
        1,
        "one authoritative `point_count`, names={names:?}"
    );

    // `COALESCED_COUNT` is renamed when coalescing is enabled (the default).
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &spread_points(3), true, Some("COALESCED_COUNT"));
    convert_to_overviews(tin.path(), tout.path(), &opts(true))
        .expect("COALESCED_COUNT auto-renamed");
    let names = output_column_names(tout.path());
    assert!(
        names.iter().any(|n| n == "COALESCED_COUNT_"),
        "renamed source column present, names={names:?}"
    );
}

/// #359: the rename that #288 applies is a property of the intermediate
/// overview file, not of the data. The overview GeoParquet must keep it (both
/// `level` columns coexist there, and the reserved one is authoritative), but
/// the PMTiles export drops tylertoo's `level` from MVT entirely — so by
/// tile-writing time the source name is free and must be given back.
///
/// Publishing `level_` instead is silent: a style keyed on `level` finds no
/// such property, paints every feature at the ramp's base, and reads as a
/// rendering bug rather than a tiler one.
#[test]
fn renamed_source_column_is_restored_in_the_exported_archive() {
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tov = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &spread_points(6), true, Some("level"));
        convert_to_overviews(tin.path(), tov.path(), &opts(streaming)).unwrap();

        // The intermediate file keeps the rename — the collision is real here.
        let names = output_column_names(tov.path());
        assert!(
            names.iter().any(|n| n == "level_") && names.iter().any(|n| n == "level"),
            "streaming={streaming}: both columns must coexist in the overview \
             file, names={names:?}"
        );

        // ...and records why, so a later standalone `export-pmtiles` on this
        // file can restore the name without the converting process's state.
        let renames = OverviewReader::open(tov.path())
            .unwrap()
            .meta()
            .generalization
            .as_ref()
            .and_then(|g| g.renamed_columns.clone())
            .unwrap_or_else(|| panic!("streaming={streaming}: no rename provenance"));
        assert_eq!(
            renames.get("level_").map(String::as_str),
            Some("level"),
            "streaming={streaming}: provenance must map output -> source"
        );

        // The archive advertises the source name, not the internal one.
        let tout = tempfile::NamedTempFile::new().unwrap();
        export_pmtiles(tov.path(), tout.path(), &ExportOptions::default()).unwrap();
        let fields = archive_layer_fields(tout.path());
        assert!(
            fields.contains(&"level".to_string()),
            "streaming={streaming}: the source name must be published, got {fields:?}"
        );
        assert!(
            !fields.contains(&"level_".to_string()),
            "streaming={streaming}: the internal name must not leak, got {fields:?}"
        );
    }
}

/// The `vector_layers[].fields` keys of a PMTiles archive's JSON metadata.
fn archive_layer_fields(path: &Path) -> Vec<String> {
    use std::io::Read;

    let mut buf = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_end(&mut buf)
        .unwrap();
    // PMTiles v3 header: the JSON metadata offset/length live at bytes 24..40.
    let off = u64::from_le_bytes(buf[24..32].try_into().unwrap()) as usize;
    let len = u64::from_le_bytes(buf[32..40].try_into().unwrap()) as usize;
    let json =
        crate::compression::decompress(&buf[off..off + len], crate::compression::Compression::Gzip)
            .expect("archive metadata is gzip");
    let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let mut out: Vec<String> = v["vector_layers"]
        .as_array()
        .expect("vector_layers")
        .iter()
        .flat_map(|l| {
            l["fields"]
                .as_object()
                .map(|o| o.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .collect();
    out.sort();
    out
}

// ============================================================================
// Verbatim disposition: tile the input exactly as given (#345 / #360)
// ============================================================================

/// A grid of small, uniform, evenly spaced cells — the shape of a DGGS
/// aggregate (H3/A5 rollup, gpio `process aggregate`). Every cell carries its
/// own count and every one must be drawn; none is a "simplified" version of
/// another.
fn cell_aggregate(n: usize) -> Vec<Option<Geometry<f64>>> {
    let side = (n as f64).sqrt().ceil() as usize;
    (0..n)
        .map(|i| {
            let (x, y) = (
                -10.0 + (i % side) as f64 * 0.05,
                20.0 + (i / side) as f64 * 0.05,
            );
            Some(Geometry::Polygon(Polygon::new(
                LineString::from(vec![
                    (x, y),
                    (x + 0.02, y),
                    (x + 0.02, y + 0.02),
                    (x, y + 0.02),
                    (x, y),
                ]),
                vec![],
            )))
        })
        .collect()
}

/// The reported failure (#360): running a cell aggregate through the
/// generalizing ladder answers the wrong question. The gates ask "is this
/// feature big enough to see"; for an aggregate the question is "what do these
/// cells sum to", so a coarse level shows some children and silently omits the
/// rest. The reporter measured z0 falling from 3,399 cells to 278 — 92% of a
/// choropleth in which every cell must be drawn.
///
/// Verbatim must keep every feature at every level.
#[test]
fn verbatim_keeps_every_feature_at_every_level() {
    const N: usize = 400;
    let cells = cell_aggregate(N);

    for streaming in [true, false] {
        // Baseline: the default ladder thins the aggregate away at coarse
        // levels. This is correct for roads and wrong here — it is the
        // behaviour the flag exists to switch off.
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &cells, true, None);
        let report = convert_to_overviews(tin.path(), tout.path(), &opts(streaming)).unwrap();
        let laddered: Vec<usize> = report.levels.iter().map(|l| l.feature_count).collect();
        assert!(
            laddered.iter().any(|&c| c < N),
            "streaming={streaming}: the default ladder should thin this \
             aggregate (that is the bug being fixed), got {laddered:?}"
        );

        // Verbatim: every level is the whole input.
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &cells, true, None);
        let report =
            convert_to_overviews(tin.path(), tout.path(), &opts(streaming).verbatim()).unwrap();
        let counts: Vec<usize> = report.levels.iter().map(|l| l.feature_count).collect();
        assert_eq!(
            counts,
            vec![N; counts.len()],
            "streaming={streaming}: every level must carry every cell"
        );
        assert!(
            counts.len() >= 2,
            "streaming={streaming}: no level may be omitted as empty, got {counts:?}"
        );
        validate_file(tout.path()).unwrap();
    }
}

/// Verbatim must also leave geometry alone: a level that kept every feature
/// but simplified their rings is still not the input.
#[test]
fn verbatim_preserves_vertex_counts_at_every_level() {
    let cells = cell_aggregate(200);
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &cells, true, None);
    let report = convert_to_overviews(tin.path(), tout.path(), &opts(true).verbatim()).unwrap();

    let vertices: Vec<usize> = report.levels.iter().map(|l| l.vertex_count).collect();
    assert!(
        vertices.windows(2).all(|w| w[0] == w[1]),
        "every level must carry identical geometry, got {vertices:?}"
    );
}

/// The issue's parenthetical: `--polygon-thinning 0` was rejected ("must be a
/// finite value > 0"), forcing a `1e-9` workaround. Zero is now the documented
/// off switch and means what a caller expects — keep everything — rather than
/// the old degenerate reading, which dropped everything.
#[test]
fn zero_thinning_is_the_off_switch_not_an_error() {
    let cells = cell_aggregate(120);
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    write_input(tin.path(), &cells, true, None);

    let mut o = opts(true);
    o.assign.polygon_thinning = 0.0;
    o.assign.polygon_visibility = 0.0;
    o.density.enabled = false;
    let report = convert_to_overviews(tin.path(), tout.path(), &o)
        .expect("0 must be accepted as the off switch");
    assert!(
        report.levels.iter().all(|l| l.feature_count == 120),
        "0 must keep every feature, got {:?}",
        report
            .levels
            .iter()
            .map(|l| l.feature_count)
            .collect::<Vec<_>>()
    );

    // Still nonsense, still rejected.
    let mut bad = opts(true);
    bad.assign.polygon_thinning = -1.0;
    assert!(convert_to_overviews(tin.path(), tout.path(), &bad).is_err());
    let mut nan = opts(true);
    nan.assign.polygon_thinning = f64::NAN;
    assert!(convert_to_overviews(tin.path(), tout.path(), &nan).is_err());
}

/// `verbatim()` and `is_verbatim()` must agree, and the flag must not reach
/// beyond the ladder into unrelated configuration.
#[test]
fn verbatim_is_recognizable_and_leaves_other_options_alone() {
    let base = ConvertOptions {
        mode: Mode::Duplicating,
        max_row_group_size: 1234,
        cluster: true,
        ..opts(true)
    };
    assert!(!base.clone().is_verbatim());

    let v = base.clone().verbatim();
    assert!(v.is_verbatim());
    assert_eq!(v.max_row_group_size, 1234, "layout knobs are untouched");
    assert!(v.cluster, "an explicit --cluster is the caller's call");
    assert_eq!(v.mode, base.mode);
    assert_eq!(v.levels, base.levels);
}

// ============================================================================
// Entry-zoom ladder: attribute-driven entry, overriding the gate (#364)
// ============================================================================

/// Concentric bands: the strongest magnitude is the physically *smallest*
/// ring, which is exactly the case geometry-ranked thinning gets backwards.
/// Returns `(geometries, magnitudes)` in row order.
fn nested_bands(sites: usize) -> (Vec<Option<Geometry<f64>>>, Vec<f64>) {
    let mags = [0.2f64, 0.275, 0.35, 0.425, 0.5];
    let mut geoms = Vec::new();
    let mut values = Vec::new();
    for s in 0..sites {
        let (cx, cy) = (-40.0 + (s % 8) as f64 * 9.0, 10.0 + (s / 8) as f64 * 9.0);
        for (k, m) in mags.iter().enumerate() {
            // Outermost ring (weakest) is ~4 degrees; innermost (strongest) is
            // tiny enough that every visibility gate removes it.
            let r = 4.0 * (0.06f64).powi(k as i32);
            geoms.push(Some(Geometry::Polygon(Polygon::new(
                LineString::from(vec![
                    (cx - r, cy - r),
                    (cx + r, cy - r),
                    (cx + r, cy + r),
                    (cx - r, cy + r),
                    (cx - r, cy - r),
                ]),
                vec![],
            ))));
            values.push(*m);
        }
    }
    (geoms, values)
}

/// Write nested-band input with the magnitudes in a `magnitude` column.
fn write_banded_input(path: &Path, sites: usize) -> Vec<f64> {
    let (geoms, values) = nested_bands(sites);
    super::testutil::write_input_with_f64(path, &geoms, "magnitude", &values);
    values
}

/// Count how many rows at each level carry a magnitude at or above `min_mag`.
fn level_strong_counts(path: &Path, min_mag: f64) -> Vec<usize> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::Float64Type;
    use arrow_array::Array;

    let reader = OverviewReader::open(path).unwrap();
    let n = reader.meta().levels.len();
    (0..n)
        .map(|level| {
            let mut strong = 0usize;
            for batch in reader.read_level(level, None).unwrap() {
                let batch = batch.unwrap();
                let idx = batch.schema().index_of("magnitude").unwrap();
                let col = batch.column(idx).as_primitive::<Float64Type>();
                for i in 0..col.len() {
                    if !col.is_null(i) && col.value(i) >= min_mag {
                        strong += 1;
                    }
                }
            }
            strong
        })
        .collect()
}

/// The reported failure (#364): `--sort-key` chooses between features
/// *competing for a cell*, but the visibility gate has already dropped the
/// small strong cores on size, so coarse levels show the big weak rings.
///
/// An entry-zoom ladder must admit them regardless of size.
#[test]
fn entry_zoom_ladder_admits_strong_features_the_gate_drops() {
    use crate::overview::ladder::{EntryZoomKind, EntryZoomSpec};

    for streaming in [true, false] {
        // Control: ranking only. The gate still removes the tiny cores.
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_banded_input(tin.path(), 8);
        let control = ConvertOptions {
            sort_key: Some("magnitude".to_string()),
            ..opts(streaming)
        };
        convert_to_overviews(tin.path(), tout.path(), &control).unwrap();
        let before = level_strong_counts(tout.path(), 0.5);

        // Ladder: strongest magnitude enters at the coarsest level.
        let tin2 = tempfile::NamedTempFile::new().unwrap();
        let tout2 = tempfile::NamedTempFile::new().unwrap();
        write_banded_input(tin2.path(), 8);
        let laddered = ConvertOptions {
            entry_zoom: Some(EntryZoomSpec {
                column: "magnitude".to_string(),
                kind: EntryZoomKind::DenseRank { step: 1 },
            }),
            // A ladder admits the feature; the collapse disposition decides
            // what its geometry becomes there. Without one, a 6-metre core
            // admitted to a ~10 km-tolerance level is simplified away again —
            // which is why the CLI turns this on alongside a ladder.
            simplify: SimplifyOptions {
                collapse: CollapseMode::Point,
                ..Default::default()
            },
            ..opts(streaming)
        };
        convert_to_overviews(tin2.path(), tout2.path(), &laddered).unwrap();
        let after = level_strong_counts(tout2.path(), 0.5);

        assert_eq!(
            before[0], 0,
            "streaming={streaming}: precondition — the gate drops every 0.5 \
             core at the coarsest level without a ladder (got {before:?})"
        );
        assert!(
            after[0] > 0,
            "streaming={streaming}: the ladder must admit the strongest \
             magnitude at the coarsest level (before={before:?} after={after:?})"
        );
        validate_file(tout2.path()).unwrap();
    }
}

/// Both engines must place a laddered feature identically — the ladder is
/// resolved from the same column and the same level plan in each.
#[test]
fn entry_zoom_ladder_is_identical_across_pipelines() {
    use crate::overview::ladder::{EntryZoomKind, EntryZoomSpec};

    let mut per_engine = Vec::new();
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_banded_input(tin.path(), 6);
        let o = ConvertOptions {
            entry_zoom: Some(EntryZoomSpec {
                column: "magnitude".to_string(),
                kind: EntryZoomKind::DenseRank { step: 1 },
            }),
            ..opts(streaming)
        };
        convert_to_overviews(tin.path(), tout.path(), &o).unwrap();
        per_engine.push((
            level_strong_counts(tout.path(), 0.5),
            level_strong_counts(tout.path(), 0.2),
        ));
    }
    assert_eq!(
        per_engine[0], per_engine[1],
        "streamed and buffered engines must agree on ladder placement"
    );
}

/// A ladder column that does not exist is a configuration error, reported
/// before any conversion work rather than silently ignored.
#[test]
fn entry_zoom_missing_column_is_rejected() {
    use crate::overview::ladder::{EntryZoomKind, EntryZoomSpec};

    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_banded_input(tin.path(), 2);
        let o = ConvertOptions {
            entry_zoom: Some(EntryZoomSpec {
                column: "nope".to_string(),
                kind: EntryZoomKind::DenseRank { step: 1 },
            }),
            ..opts(streaming)
        };
        let err = convert_to_overviews(tin.path(), tout.path(), &o).unwrap_err();
        assert!(
            matches!(err, ConvertError::InvalidConfig(ref m) if m.contains("nope")),
            "streaming={streaming}: got {err}"
        );
    }
}

// ============================================================================
// Class 8: zero-row levels after thinning (empty-level omission, all routes)
// ============================================================================

#[test]
fn empty_coarse_levels_omitted_across_pipelines() {
    // Tiny lines fail the visibility gate at every coarse level: with
    // coalescing off those levels are empty and must be omitted (not written,
    // not crashed on — #211 auto-clamp), leaving a valid file with fewer
    // levels.
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

// ============================================================================
// Issue #188: antimeridian downstream behavior pins
// ============================================================================
//
// PR #186 (class 4 above) pinned that antimeridian/polar inputs never crash.
// These tests pin what the verbatim contract does DOWNSTREAM: the bbox math
// never produces a wrapped bbox, and export smears an antimeridian-crossing
// polygon across the whole world row. They document current behavior, not
// desired behavior. See `context/ANTIMERIDIAN.md`.

#[test]
fn antimeridian_bbox_is_inflated_never_wrapped() {
    use super::convert::geometry_bbox;
    // A 0.2°-wide polygon straddling ±180°, stored verbatim.
    let poly = Geometry::Polygon(Polygon::new(
        LineString::from(vec![
            (-179.9, -0.1),
            (179.9, -0.1),
            (179.9, 0.1),
            (-179.9, 0.1),
            (-179.9, -0.1),
        ]),
        vec![],
    ));
    let [xmin, ymin, xmax, ymax] = geometry_bbox(&poly);
    // Plain min/max: never a wrapped bbox (xmin > xmax cannot arise), so the
    // wrapped-bbox branch in `tiles_for_bbox` is unreachable from this
    // pipeline's own bboxes.
    assert_eq!(
        [xmin, ymin, xmax, ymax],
        [-179.9, -0.1, 179.9, 0.1],
        "PIN: bounding_rect yields the inflated (359.8°-wide) bbox"
    );
    assert!(xmin < xmax, "PIN: wrapped bboxes never arise");
}

#[test]
fn antimeridian_suspect_features_warned_normal_inputs_clean() {
    // Convert-time detection (#188 follow-up): a feature whose bbox spans
    // more than 180° of longitude is counted as antimeridian-suspect and
    // surfaced via `ConvertReport::antimeridian_suspect_features` plus ONE
    // aggregate `log::warn!` at the end of convert. Detection only — the
    // geometry itself is stored verbatim, never mutated.
    let suspect = Geometry::Polygon(Polygon::new(
        LineString::from(vec![
            (-179.9, -0.1),
            (179.9, -0.1),
            (179.9, 0.1),
            (-179.9, 0.1),
            (-179.9, -0.1),
        ]),
        vec![],
    ));
    let geoms: Vec<Option<Geometry<f64>>> = vec![
        Some(suspect),
        Some(Geometry::Point(Point::new(10.0, 10.0))),
        Some(Geometry::Point(Point::new(-170.0, 5.0))),
    ];
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, true, None);
        let report = convert_to_overviews(tin.path(), tout.path(), &opts(streaming)).unwrap();
        assert_eq!(
            report.antimeridian_suspect_features, 1,
            "streaming={streaming}: exactly the wide polygon is flagged"
        );
    }

    // A normal (wide but < 180°) dataset triggers nothing.
    let normal: Vec<Option<Geometry<f64>>> = vec![
        Some(Geometry::LineString(LineString::from(vec![
            (-80.0, 0.0),
            (80.0, 10.0),
        ]))),
        Some(Geometry::Point(Point::new(0.0, 0.0))),
    ];
    for streaming in [true, false] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &normal, true, None);
        let report = convert_to_overviews(tin.path(), tout.path(), &opts(streaming)).unwrap();
        assert_eq!(
            report.antimeridian_suspect_features, 0,
            "streaming={streaming}: sub-180° extents are not flagged"
        );
    }
}

#[test]
fn antimeridian_polygon_export_smears_world_row() {
    // End-to-end: one 0.2°-wide polygon straddling ±180° at the equator.
    // A wrap-aware exporter would emit ~2 tile columns per zoom (one on each
    // side of ±180°); the verbatim rectangle instead intersects EVERY column,
    // so the finest zoom (z6, 64 columns × 2 equator rows) writes ~128 tiles.
    let tin = tempfile::NamedTempFile::new().unwrap();
    let tovr = tempfile::NamedTempFile::new().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    let geoms: Vec<Option<Geometry<f64>>> = vec![Some(Geometry::Polygon(Polygon::new(
        LineString::from(vec![
            (-179.9, -0.1),
            (179.9, -0.1),
            (179.9, 0.1),
            (-179.9, 0.1),
            (-179.9, -0.1),
        ]),
        vec![],
    )))];
    write_input(tin.path(), &geoms, true, None);
    convert_to_overviews(tin.path(), tovr.path(), &opts(true)).unwrap();
    let report = export_pmtiles(tovr.path(), tout.path(), &ExportOptions::default()).unwrap();

    for z in &report.zooms {
        eprintln!("z{}: {} tiles", z.zoom, z.tile_count);
    }
    let finest = report.zooms.last().unwrap();
    assert_eq!(finest.zoom, 6);
    assert!(
        finest.tile_count >= 64,
        "PIN: export smears the polygon across the full world row at z6 \
         (>= 64 tile columns), got {} tiles",
        finest.tile_count
    );
}
