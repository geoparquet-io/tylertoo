//! Quality-matched simplification must retain the main-branch parity floor.
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

#[test]
fn madagascar_verbatim_simplification_retains_level_zero() {
    let Some(input) = fixture::realdata("fieldmaps-madagascar-adm4.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut options = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 2,
        },
        ..ConvertOptions::default().verbatim()
    };
    options.simplify.factor = 1.0;
    let report =
        convert_to_overviews(&input, dir.path().join("overview.parquet"), &options).unwrap();
    let level = report
        .levels
        .iter()
        .find(|level| level.zoom == Some(0))
        .unwrap();
    assert!(
        level.feature_count >= 1150,
        "main retains 1150 level-0 features; got {}",
        level.feature_count
    );
}
