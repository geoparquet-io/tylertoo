//! #644: the one aggregate `log::info` line naming features wrapped across
//! the antimeridian, asserted.
//!
//! Its own test binary, so this is the only code installing a logger here:
//! `log::set_logger` is process-global and succeeds once.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use geo::{Geometry, Point};
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};

#[expect(
    dead_code,
    reason = "#779: this test binary uses a subset of the shared fixture writers"
)]
#[path = "../src/overview/testutil.rs"]
mod testutil;

static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

struct Capture;
impl log::Log for Capture {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Info
    }
    fn log(&self, record: &log::Record) {
        if record.level() == log::Level::Info {
            LINES.lock().unwrap().push(record.args().to_string());
        }
    }
    fn flush(&self) {}
}

/// The info lines a conversion of `input` logs about wrapped features.
fn wrapped_lines(input: &Path, streaming: bool) -> Vec<String> {
    LINES.lock().unwrap().clear();
    let tout = tempfile::NamedTempFile::new().unwrap();
    let o = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 2,
            max_zoom: 6,
        },
        streaming,
        ..Default::default()
    };
    convert_to_overviews(input, tout.path(), &o)
        .unwrap_or_else(|e| panic!("streaming={streaming}: {e}"));
    LINES
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.contains("wrapped across the antimeridian"))
        .cloned()
        .collect()
}

#[test]
fn wrapped_features_get_one_info_line_naming_the_row() {
    log::set_logger(&Capture).expect("the only logger in this binary");
    log::set_max_level(log::LevelFilter::Info);

    // Rows 0-6 and 8-9 sit near lon -115; row 7 is at lon 190.25.
    let dateline: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/streaming/out-of-range-dateline.parquet");
    let in_range = tempfile::NamedTempFile::new().unwrap();
    let points: Vec<Option<Geometry<f64>>> = (0..10)
        .map(|i| Some(Geometry::Point(Point::new(-120.0 + i as f64, 40.0))))
        .collect();
    testutil::write_input(in_range.path(), &points, true, None);

    for streaming in [true, false] {
        let lines = wrapped_lines(&dateline, streaming);
        assert_eq!(lines.len(), 1, "streaming={streaming}: {lines:#?}");
        assert!(
            lines[0].contains("1 of 10 feature(s)") && lines[0].contains("lon 190.250 (row 7)"),
            "streaming={streaming}: {}",
            lines[0]
        );

        let lines = wrapped_lines(in_range.path(), streaming);
        assert!(lines.is_empty(), "streaming={streaming}: {lines:#?}");
    }
}
