//! Regression coverage for the one schema owner and its persisted descriptor.
use std::path::{Path, PathBuf};
use tylertoo_core::input_set::ConvertSource;
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};
use tylertoo_core::overview::reader::OverviewReader;
use tylertoo_core::resolved_schema::{ResolvedSchema, RESOLVED_SCHEMA_KEY};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/invariants")
        .join(name)
}

#[test]
fn shape_descriptor_survives_convert_check_export_and_auto_zoom() {
    let input = fixture("shape-column.parquet");
    let source = ConvertSource::resolve(input.to_str().unwrap()).unwrap();
    let descriptor = source.resolved_schema().unwrap();
    assert_eq!(descriptor.geometry_name(), Some("SHAPE"));
    assert_eq!(descriptor.primary_column(), Some("SHAPE"));
    let options = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 14,
            max_zoom: 14,
        },
        ..Default::default()
    };
    let evidence =
        tylertoo_core::overview::auto_zoom::estimate_max_zoom(&source, &options).unwrap();
    assert!(evidence.chosen_zoom >= 14);
    let temp = tempfile::tempdir().unwrap();
    let overview = temp.path().join("overview.parquet");
    convert_to_overviews(&input, &overview, &options).unwrap();
    let reader = OverviewReader::open(&overview).unwrap();
    let out = reader.resolved_schema();
    assert_eq!(out.geometry_name(), Some("SHAPE"));
    assert!(out.covering().is_some());
    assert!(out.bounds().iter().all(Option::is_some));
    assert!(!out.properties().is_empty());
    assert!(out
        .properties()
        .iter()
        .all(|&i| Some(i) != out.geometry_index() && out.schema().field(i).name() != "level"));
    let metadata =
        tylertoo_core::untrusted::read_parquet_metadata(&std::fs::File::open(&overview).unwrap())
            .unwrap();
    assert!(metadata
        .file_metadata()
        .key_value_metadata()
        .unwrap()
        .iter()
        .any(|kv| kv.key == RESOLVED_SCHEMA_KEY));
    let overview_source = ConvertSource::resolve_path(&overview).unwrap();
    assert_eq!(
        overview_source.resolved_schema().unwrap().geometry_name(),
        Some("SHAPE")
    );
    tylertoo_core::overview::auto_zoom::estimate_max_zoom(&overview_source, &options).unwrap();
    assert!(tylertoo_core::overview::check::validate_file(&overview)
        .unwrap()
        .is_valid());
    let archive = temp.path().join("tiles.pmtiles");
    let report = export_pmtiles(&overview, &archive, &ExportOptions::default()).unwrap();
    assert!(report.total_tile_features > 0);
}

#[test]
fn native_geoparquet2_descriptor_is_cached_per_part() {
    let input = fixture("geoparquet2-native.parquet");
    let source = ConvertSource::resolve(input.to_str().unwrap()).unwrap();
    let descriptor = source.resolved_schema().unwrap();
    assert_eq!(descriptor.version(), Some("2.0.0"));
    assert!(!descriptor.crs_info().assumed_wgs84);
    assert!(std::ptr::eq(descriptor, source.resolved_schema().unwrap()));
    let metadata =
        tylertoo_core::untrusted::read_parquet_metadata(&std::fs::File::open(input).unwrap())
            .unwrap();
    assert_eq!(
        ResolvedSchema::from_metadata(&metadata)
            .unwrap()
            .geometry_name(),
        descriptor.geometry_name()
    );
}

#[test]
fn native_geoparquet2_multipart_reports_declared_crs() {
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("parts");
    std::fs::create_dir(&input).unwrap();
    for name in ["a.parquet", "b.parquet"] {
        std::fs::copy(fixture("geoparquet2-native.parquet"), input.join(name)).unwrap();
    }
    let source = ConvertSource::resolve_path(&input).unwrap();
    let output = temp.path().join("overview.parquet");
    let options = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 14,
            max_zoom: 14,
        },
        ..Default::default()
    };
    let report =
        tylertoo_core::overview::convert::convert_to_overviews_sources(&source, &output, &options)
            .unwrap();
    assert_eq!(report.input_features, 2000);
    assert!(
        !report.crs_assumed_wgs84,
        "#708: every part declares its CRS through the logical type"
    );
}
