//! Synthetic variable-depth input and tile-byte checks shared by reentry tests.

use std::path::{Path, PathBuf};

use tylertoo_core::internal::archive_index::ArchiveIndex;
use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::internal::overview::export::{export_pmtiles, ExportOptions};

#[expect(dead_code, reason = "only the synthetic input writer is needed here")]
#[path = "../../src/overview/testutil.rs"]
mod testutil;

pub(crate) fn overview(dir: &Path) -> PathBuf {
    let input = dir.join("input.parquet");
    let points = (0..64)
        .map(|i| {
            Some(geo::Geometry::Point(geo::Point::new(
                10.0 + f64::from(i % 16) * 0.01,
                45.0 + f64::from(i / 16) * 0.01,
            )))
        })
        .collect::<Vec<_>>();
    testutil::write_input(&input, &points, true, None);
    let overview = dir.join("overview.parquet");
    let mut options = ConvertOptions::default();
    options.density.enabled = false;
    options.assign.point_thinning = 0.0;
    convert_to_overviews(
        &input,
        &overview,
        &options.with_levels(LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 6,
        }),
    )
    .expect("convert");
    overview
}

/// Return tile coordinates and compressed bodies in archive order.
pub(crate) fn export(overview: &Path, output: &Path) -> Vec<((u8, u32, u32), Vec<u8>)> {
    let report = export_pmtiles(
        overview,
        output,
        &ExportOptions::default().with_variable_depth(true),
    )
    .expect("variable-depth export");
    let vd = report.variable_depth.expect("variable-depth report");
    assert!(vd.probed_tiles > 0, "must exercise probe_wave");
    assert!(vd.stopped_tiles > 0, "must produce terminal tiles");
    let index = ArchiveIndex::open(output).expect("archive");
    index
        .tiles()
        .map(|tile| {
            let tile = tile.expect("tile");
            (
                (tile.z, tile.x, tile.y),
                index.read_range(tile.range).expect("tile bytes"),
            )
        })
        .collect()
}
