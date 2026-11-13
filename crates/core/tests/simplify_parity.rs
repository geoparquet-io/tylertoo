//! Under the cascade, placeholder squares must thin toward coarse levels as
//! they do without it (#749): quality-matched runs (`--verbatim
//! --simplify-factor 1`, polygon gate 0) once kept every square alive at
//! every coarser level.
use std::collections::BTreeMap;

use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

/// Rows per zoom of a quality-matched conversion.
fn rows_per_zoom(input: &std::path::Path, max_zoom: u8, cascade: bool) -> BTreeMap<u8, usize> {
    let dir = tempfile::tempdir().unwrap();
    let mut options = (ConvertOptions::default().verbatim()).with_levels(LevelPlan::ZoomRange {
        min_zoom: 0,
        max_zoom,
    });
    options.simplify.factor = 1.0;
    options.simplify.cascade = cascade;
    let report =
        convert_to_overviews(input, dir.path().join("overview.parquet"), &options).unwrap();
    report
        .levels
        .iter()
        .map(|level| (level.zoom.unwrap(), level.feature_count))
        .collect()
}

/// At every zoom of the cascade run, it keeps about as many rows as
/// simplifying each level from the source does (a level the source run does
/// not emit holds no rows there).
fn assert_cascade_matches_source(fixture: &str, max_zoom: u8) {
    let Some(input) = fixture::realdata(fixture) else {
        return;
    };
    let cascade = rows_per_zoom(&input, max_zoom, true);
    let source = rows_per_zoom(&input, max_zoom, false);
    for (zoom, &cascaded) in &cascade {
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
    assert_cascade_matches_source("open-buildings.parquet", 14);
}

#[test]
fn madagascar_squares_decay_under_the_cascade() {
    // On main before the fix: 1,150 placeholder squares at z0 against 379.
    assert_cascade_matches_source("fieldmaps-madagascar-adm4.parquet", 2);
}
