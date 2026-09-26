//! Hostile-input hardening tests for the overview pipeline (issue H4) --
//! white-box remainder.
//!
//! The bulk of the original H4 suite drove the pipeline only through its
//! public API (`convert_to_overviews`, `export_pmtiles`, `OverviewReader`,
//! ...) with real parquet I/O, each case run through both the in-memory and
//! streaming engines. That bulk moved to
//! `crates/core/tests/overview_hostile.rs` (#457) so it stops running on
//! every `cargo test --lib`. It is still fast, so nextest's `quick` profile
//! (and therefore `full`, and plain `cargo test`) covers it.
//!
//! What is left here calls `pub(super)` helpers in [`super::convert`]
//! (bbox/CRS range classification, the all-lost gate) that are not part of
//! the crate's public API and so cannot be reached from an integration test
//! binary. They are small, fast, allocation-free unit tests of pure
//! functions -- exactly what `--lib` is for.

use geo::{Geometry, LineString, Polygon};

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
fn coordinates_exactly_on_the_domain_edge_are_in_range() {
    use super::convert::bbox_out_of_crs_range;
    use super::level::Crs;

    // The whole world, the poles, and an antimeridian vertex are legitimate.
    for bbox in [
        [-180.0, -90.0, 180.0, 90.0],
        [180.0, 90.0, 180.0, 90.0],
        [-180.0, -90.0, -180.0, -90.0],
        [0.0, 0.0, 0.0, 0.0],
    ] {
        assert!(
            !bbox_out_of_crs_range(&bbox, Crs::Epsg4326),
            "bbox {bbox:?} sits on (not beyond) the domain edge"
        );
    }
    // One ULP beyond any edge is out.
    for bbox in [
        [-180.000_001, 0.0, 0.0, 0.0],
        [0.0, -90.000_001, 0.0, 0.0],
        [0.0, 0.0, 180.000_001, 0.0],
        [0.0, 0.0, 0.0, 90.000_001],
    ] {
        assert!(
            bbox_out_of_crs_range(&bbox, Crs::Epsg4326),
            "bbox {bbox:?} reaches beyond the domain"
        );
    }
    // A 3857 input is measured against the Web Mercator world extent.
    assert!(!bbox_out_of_crs_range(
        &[-20_037_508.0, -20_037_508.0, 20_037_508.0, 20_037_508.0],
        Crs::Epsg3857
    ));
    assert!(bbox_out_of_crs_range(
        &[0.0, 0.0, 30_000_000.0, 0.0],
        Crs::Epsg3857
    ));
}

/// Only a bbox ENTIRELY beyond the Mercator limit is untileable; one that
/// straddles it still puts geometry in tiles and is merely clipped.
#[test]
fn only_wholly_polar_bboxes_count_as_unprojectable() {
    use super::convert::bbox_unprojectable;
    use super::level::Crs;

    for bbox in [
        [0.0, 86.0, 10.0, 88.0],   // wholly Arctic
        [0.0, -89.0, 10.0, -86.0], // wholly Antarctic
    ] {
        assert!(
            bbox_unprojectable(&bbox, Crs::Epsg4326),
            "bbox {bbox:?} never reaches a tile"
        );
    }
    for bbox in [
        [0.0, 84.0, 10.0, 88.0],  // straddles the limit: clipped, not lost
        [0.0, -85.0, 10.0, 85.0], // ordinary
        [0.0, 85.05, 10.0, 85.05],
    ] {
        assert!(
            !bbox_unprojectable(&bbox, Crs::Epsg4326),
            "bbox {bbox:?} still has geometry inside the tiling domain"
        );
    }
    // A 3857 input is already in the tiling domain's own units.
    assert!(!bbox_unprojectable(
        &[0.0, 19_000_000.0, 10.0, 20_000_000.0],
        Crs::Epsg3857
    ));
}

#[test]
fn out_of_range_warning_diagnoses_a_projected_crs_only_when_the_values_are_big() {
    use super::convert::out_of_range_warning;
    use super::level::Crs;

    assert!(out_of_range_warning(0, 10, Crs::Epsg4326, 0.0).is_none());
    assert!(out_of_range_warning(3, 0, Crs::Epsg4326, 0.0).is_none());

    // Meter-scale values: the file is almost certainly in another CRS.
    let msg = out_of_range_warning(1, 4, Crs::Epsg4326, 6_883_000.0).expect("a warning is due");
    assert!(msg.contains("1 of 4 feature(s) (25.0%)"), "{msg}");
    assert!(
        msg.contains("OGC:CRS84 / EPSG:4326 coordinate range"),
        "{msg}"
    );
    assert!(msg.contains("EPSG:3857 meters"), "{msg}");
    assert!(
        msg.contains("gpio convert reproject <input> reprojected.parquet -d EPSG:4326"),
        "the gpio hint must match quality.rs's wording: {msg}"
    );

    // One stray Pacific point at lng 180.001 (0–360° convention data) is not
    // evidence of a projected CRS, and must not be diagnosed as one (S2-2).
    let stray = out_of_range_warning(1, 1_000, Crs::Epsg4326, 180.001).expect("a warning is due");
    assert!(stray.contains("1 of 1000 feature(s) (0.1%)"), "{stray}");
    assert!(
        stray.contains("reach beyond the OGC:CRS84 / EPSG:4326 coordinate range")
            && stray.contains("dropped or clipped"),
        "neutral wording for a stray coordinate: {stray}"
    );
    assert!(
        !stray.contains("gpio convert reproject") && !stray.contains("projected CRS"),
        "one stray coordinate must not accuse the whole file: {stray}"
    );

    // A file that already declares EPSG:3857 gets 3857-shaped wording, never
    // a message that contradicts its own metadata (S2-3).
    let m3857 = out_of_range_warning(2, 2, Crs::Epsg3857, 30_000_000.0).expect("a warning is due");
    assert!(
        m3857.contains("EPSG:3857 coordinate range (±20037508.34 m)")
            && m3857.contains("metadata says EPSG:3857")
            && !m3857.contains("CRS84"),
        "a 3857 input must not be told its metadata says CRS84: {m3857}"
    );
}

#[test]
fn unprojectable_warning_names_the_mercator_domain_and_offers_no_reprojection() {
    use super::convert::unprojectable_warning;

    assert!(unprojectable_warning(0, 10).is_none());
    assert!(unprojectable_warning(3, 0).is_none());

    let msg = unprojectable_warning(2, 8).expect("a warning is due");
    assert!(msg.contains("2 of 8 feature(s) (25.0%)"), "{msg}");
    assert!(
        msg.contains(
            "valid lon/lat but lie outside the Web Mercator tiling domain \
                      (|lat| > 85.05°); these features cannot be tiled"
        ),
        "{msg}"
    );
    assert!(
        !msg.contains("gpio convert reproject"),
        "reprojecting to 4326 fixes nothing here: {msg}"
    );
}

/// The all-lost gate is a SHARE, not exactly 100%: a million-row wrong-CRS
/// file with a dozen `POINT(0 0)` placeholder rows must still fail (S2-1).
#[test]
fn the_all_lost_gate_fires_at_99_percent_not_only_at_100() {
    use super::convert::{all_lost_error, BboxTallies};
    use super::level::Crs;

    let lost = |out_of_range, unprojectable| BboxTallies {
        out_of_range,
        unprojectable,
        max_abs_out_of_range: 6_883_000.0,
        ..Default::default()
    };

    // 999,988 of 1,000,000 out of range (99.9988%) — a dozen placeholders no
    // longer buy a "successful" empty archive.
    let err = all_lost_error(&lost(999_988, 0), 1_000_000, Crs::Epsg4326)
        .expect("99.9% lost is a failed conversion");
    let msg = err.to_string();
    assert!(
        msg.contains("999988 of 1000000 feature(s) (100.0%) cannot be tiled"),
        "the message states the real count and share: {msg}"
    );

    // Exactly at the threshold: 99 of 100.
    assert!(all_lost_error(&lost(99, 0), 100, Crs::Epsg4326).is_some());
    // Mixed causes count together.
    assert!(all_lost_error(&lost(50, 49), 100, Crs::Epsg4326).is_some());
    // Below it the conversion is a warning, not a failure.
    assert!(all_lost_error(&lost(98, 0), 100, Crs::Epsg4326).is_none());
    assert!(all_lost_error(&lost(0, 0), 100, Crs::Epsg4326).is_none());
    assert!(all_lost_error(&lost(0, 0), 0, Crs::Epsg4326).is_none());
}
