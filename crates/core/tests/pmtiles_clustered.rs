//! The `clustered` header flag must tell the truth about the bytes on disk.
//!
//! PMTiles v3's `clustered` byte promises a reader that tile data can be
//! streamed in directory order (ascending tile id) without seeking backward
//! past unread bytes — go-pmtiles `verify` checks exactly this. The writer
//! used to stamp every archive `clustered: true` unconditionally; #501 made
//! the header derive the flag honestly from the directory's actual offset
//! layout instead, and at that point a real export was genuinely `false`: it
//! added tiles zoom-by-zoom in row-major `(x, y)` order, not tile-id
//! (Hilbert) order, so the directory's offsets were not monotonic once
//! sorted by tile id. On this fixture that was not a corner case: of the
//! archive's directory entries, thousands were non-monotonic in tile-id
//! order (measured at 5,007 of 10,075 while diagnosing that bug).
//!
//! #506 closes the gap on the other side: export now adds tiles in ascending
//! PMTiles tile-id order per zoom (levels export in ascending zoom, so this
//! holds globally too), so the archive is genuinely clustered and the header
//! flag comes out `true`. This test asserts both halves of that: the flag is
//! `true`, AND it still matches the independent, file-side re-derivation of
//! the same predicate ([`verify_clustered`]) — so the writer and a
//! go-pmtiles-style verifier can never disagree about one archive, in either
//! direction.

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

    assert!(
        header.clustered,
        "export now writes tiles in ascending PMTiles tile-id order (#506); \
         a real export archive must be genuinely clustered"
    );
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
