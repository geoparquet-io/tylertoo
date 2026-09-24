//! The `clustered` header flag must tell the truth about the bytes on disk.
//!
//! PMTiles v3's `clustered` byte promises a reader that tile data can be
//! streamed in directory order (ascending tile id) without seeking backward
//! past unread bytes — go-pmtiles `verify` checks exactly this. The writer
//! used to stamp every archive `clustered: true` unconditionally, but a real
//! export adds tiles zoom-by-zoom in row-major `(x, y)` order, not tile-id
//! (Hilbert) order, so the directory's offsets are not actually monotonic
//! once sorted by tile id. On this fixture that is not a corner case: of the
//! archive's directory entries, thousands are non-monotonic in tile-id order
//! (measured at 5,007 of 10,075 while diagnosing this bug).
//!
//! This test does not assert the flag is `true` — today it is honestly
//! `false` for an export, and a follow-up PR that writes tiles in tile-id
//! order will flip it. What it asserts is that the header's claim matches
//! reality: `header.clustered` must equal an independent, file-side
//! re-derivation of the same predicate ([`verify_clustered`]), so the writer
//! and a go-pmtiles-style verifier can never disagree about one archive.

use tylertoo_core::pmtiles_writer::verify_clustered;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

#[test]
fn header_clustered_flag_matches_the_actual_entry_layout() {
    use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};
    use tylertoo_core::pmtiles_writer::Header;

    let Some(fixture_path) = fixture::realdata("fieldmaps-madagascar-adm4.parquet") else {
        return;
    };

    let overview = tempfile::Builder::new()
        .suffix(".parquet")
        .tempfile()
        .expect("overview tempfile");
    let output = tempfile::Builder::new()
        .suffix(".pmtiles")
        .tempfile()
        .expect("output tempfile");

    convert_to_overviews(
        &fixture_path,
        overview.path(),
        &ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 10,
            },
            ..Default::default()
        },
    )
    .expect("convert");
    export_pmtiles(overview.path(), output.path(), &ExportOptions::default()).expect("export");

    let bytes = std::fs::read(output.path()).expect("read exported archive");
    let header = Header::from_bytes(&bytes).expect("parse header");
    let actually_clustered =
        verify_clustered(output.path()).expect("verify_clustered on the exported archive");

    assert_eq!(
        header.clustered, actually_clustered,
        "the header's `clustered` claim must match what the directory's own \
         offsets deliver — a writer bug that hardcodes the flag would slip \
         past a check that only inspects one side of this equation"
    );

    eprintln!(
        "export header.clustered={}, independently verified={}",
        header.clustered, actually_clustered
    );
}
