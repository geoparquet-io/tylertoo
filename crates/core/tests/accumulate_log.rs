//! #407: the tiny-polygon accumulator's `log::info` lines, asserted.
//!
//! Its own test binary, so this is the only code installing a logger here:
//! `log::set_logger` is process-global and succeeds once.

use std::sync::Mutex;

use geo::{Geometry, LineString, Polygon};
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::simplify::{CollapseMode, SimplifyOptions};

#[allow(dead_code)]
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

/// A block of 40 m fields near (10°E, 45°N), all far below the coarse
/// levels' visibility gate.
fn fields() -> Vec<Option<Geometry<f64>>> {
    let side = 40.0 / 111_320.0;
    let pitch = side * 1.5;
    let mut out = Vec::new();
    for r in 0..40 {
        for c in 0..40 {
            let (x, y) = (10.0 + c as f64 * pitch, 45.0 + r as f64 * pitch);
            out.push(Some(Geometry::Polygon(Polygon::new(
                LineString::from(vec![
                    (x, y),
                    (x + side, y),
                    (x + side, y + side),
                    (x, y + side),
                    (x, y),
                ]),
                vec![],
            ))));
        }
    }
    out
}

fn convert_at(factor: f64, streaming: bool) {
    let tin = tempfile::NamedTempFile::new().unwrap();
    testutil::write_input(tin.path(), &fields(), true, None);
    let tout = tempfile::NamedTempFile::new().unwrap();
    let o = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 8,
            max_zoom: 12,
        },
        simplify: SimplifyOptions {
            factor,
            collapse: CollapseMode::Square,
            ..SimplifyOptions::default()
        },
        streaming,
        ..Default::default()
    };
    // Factor 0 leaves the coarse levels empty, which is not an error.
    let _ = convert_to_overviews(tin.path(), tout.path(), &o).unwrap();
}

/// Factor 0: one line saying the accumulator is off, per engine. A
/// sub-unit factor: one line naming the zooms whose side was floored.
#[test]
fn accumulator_logs_the_zero_factor_skip_and_the_floored_zooms() {
    log::set_logger(&Capture).expect("the only logger in this binary");
    log::set_max_level(log::LevelFilter::Info);

    for streaming in [true, false] {
        LINES.lock().unwrap().clear();
        convert_at(0.0, streaming);
        let lines = LINES.lock().unwrap().clone();
        let off: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("tiny-polygon accumulator off: --simplify-factor 0"))
            .collect();
        assert_eq!(off.len(), 1, "streaming={streaming}: {lines:#?}");

        LINES.lock().unwrap().clear();
        convert_at(0.1, streaming);
        let lines = LINES.lock().unwrap().clone();
        let floored: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("narrower than one tile unit"))
            .collect();
        assert_eq!(floored.len(), 1, "streaming={streaming}: {lines:#?}");
        // Levels z8..z11 accumulate (z12 is canonical); all are floored at
        // factor 0.1 < 0.25, and the line names them by zoom.
        assert!(
            floored[0].contains("z8, z9, z10, z11"),
            "streaming={streaming}: {}",
            floored[0]
        );
    }
}
