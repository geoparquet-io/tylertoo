//! #407, #745: the tiny-polygon accumulator's `log::info` lines, asserted.
//!
//! Its own test binary, so this is the only code installing a logger here:
//! `log::set_logger` is process-global and succeeds once.

use std::sync::Mutex;

use geo::{Geometry, LineString, Polygon};
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::simplify::{CollapseMode, SimplifyOptions};

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

fn convert_at(factor: f64, streaming: bool, gsd_base: f64) {
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
        gsd_base,
        ..Default::default()
    };
    let _ = convert_to_overviews(tin.path(), tout.path(), &o).unwrap();
}

/// #745: the placeholder is one GSD whatever the factor, so factor 0 no
/// longer switches the accumulator off and no line says so. The floor
/// binds only where a level's GSD is under one tile unit: at
/// `--gsd-base 8192` (one unit is two GSDs) one line names the floored
/// zooms; at the default base no line does.
#[test]
fn accumulator_logs_only_the_floored_zooms() {
    log::set_logger(&Capture).expect("the only logger in this binary");
    log::set_max_level(log::LevelFilter::Info);

    for streaming in [true, false] {
        for factor in [0.0, 0.1, 1.0] {
            LINES.lock().unwrap().clear();
            convert_at(factor, streaming, 512.0);
            let lines = LINES.lock().unwrap().clone();
            assert!(
                !lines
                    .iter()
                    .any(|l| l.contains("accumulator off")
                        || l.contains("narrower than one tile unit")),
                "streaming={streaming} factor={factor}: {lines:#?}"
            );
        }

        LINES.lock().unwrap().clear();
        convert_at(1.0, streaming, 8192.0);
        let lines = LINES.lock().unwrap().clone();
        let floored: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("narrower than one tile unit"))
            .collect();
        assert_eq!(floored.len(), 1, "streaming={streaming}: {lines:#?}");
        // Levels z8..z11 accumulate (z12 is canonical) and all are floored.
        assert!(
            floored[0].contains("z8, z9, z10, z11"),
            "streaming={streaming}: {}",
            floored[0]
        );
    }
}
