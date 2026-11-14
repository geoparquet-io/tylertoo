//! Under the cascade, placeholder squares must thin toward coarse levels as
//! they do without it (#749): with the polygon gate at 0 they once survived
//! every coarser level.
use std::collections::BTreeMap;

use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

/// Rows per zoom of a z0–`max_zoom` conversion.
fn rows_per_zoom(
    input: &std::path::Path,
    options: &ConvertOptions,
    max_zoom: u8,
    cascade: bool,
) -> BTreeMap<u8, usize> {
    let dir = tempfile::tempdir().unwrap();
    let mut options = options.clone().with_levels(LevelPlan::ZoomRange {
        min_zoom: 0,
        max_zoom,
    });
    options.simplify.cascade = cascade;
    let report =
        convert_to_overviews(input, dir.path().join("overview.parquet"), &options).unwrap();
    report
        .levels
        .iter()
        .map(|level| (level.zoom.unwrap(), level.feature_count))
        .collect()
}

/// At every zoom of either run, the cascade keeps about as many rows as
/// simplifying each level from the source does (a level a run does not
/// emit holds no rows there).
fn assert_cascade_matches_source(fixture: &str, options: &ConvertOptions, max_zoom: u8) {
    let Some(input) = fixture::realdata(fixture) else {
        return;
    };
    let cascade = rows_per_zoom(&input, options, max_zoom, true);
    let source = rows_per_zoom(&input, options, max_zoom, false);
    eprintln!("{fixture}: cascade {cascade:?}\nsource {source:?}");
    for zoom in cascade.keys().chain(source.keys()) {
        let cascaded = cascade.get(zoom).copied().unwrap_or(0);
        let from_source = source.get(zoom).copied().unwrap_or(0);
        assert!(
            cascaded.abs_diff(from_source) <= from_source / 10 + 1,
            "{fixture} z{zoom}: cascade keeps {cascaded} rows, --no-cascade {from_source}\n\
             cascade {cascade:?}\nsource {source:?}"
        );
    }
}

#[test]
fn open_buildings_squares_decay_under_the_cascade() {
    // On main before the fix: 263 rows at z9 against 76, 229 at z0.
    assert_cascade_matches_source("open-buildings.parquet", &quality_matched(), 14);
}

#[test]
fn madagascar_squares_decay_under_the_cascade() {
    // On main before the fix: 1,150 placeholder squares at z0 against 379.
    assert_cascade_matches_source("fieldmaps-madagascar-adm4.parquet", &quality_matched(), 2);
}

/// `--verbatim --simplify-factor 1`: the quality-matched tippecanoe run.
fn quality_matched() -> ConvertOptions {
    let mut options = ConvertOptions::default().verbatim();
    options.simplify.factor = 1.0;
    options
}

#[test]
fn moldova_split_polygons_decay_under_the_cascade() {
    // Repairs split fields into MultiPolygons, whose squares kept 22 rows
    // at z0 against 1 with `--no-cascade` (#749).
    let mut options = ConvertOptions::default();
    options.assign.polygon_visibility = 0.0;
    assert_cascade_matches_source("ftw-moldova-z10.parquet", &options, 14);
}
