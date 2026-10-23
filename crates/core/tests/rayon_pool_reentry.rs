//! Convert and export must finish when called from inside a rayon pool.
//!
//! The overview writer hands its WKB and row-group encodes to rayon with
//! `rayon::spawn` and then blocks on the result. Called from a pool's own
//! worker thread (a library caller's `pool.install`, a rayon-parallel loop
//! over inputs), that worker sat in a blocking `recv` while the encode it
//! waited for was queued behind it; with one thread, nothing ever ran it.
//! The CLI never hit this: its main thread is not a pool worker. Found by
//! the pipeline bench (#759), which hung for 50 min in a one-thread pool.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

/// Run `f` on a fresh thread; fail instead of hanging the suite if it
/// doesn't return within `limit`. A deadlocked thread is leaked, which is
/// fine for a failing test process.
fn within<T: Send + 'static>(limit: Duration, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(limit).unwrap_or_else(|_| {
        panic!("did not finish within {limit:?}: deadlocked inside the rayon pool")
    })
}

fn one_thread_pool() -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .expect("one-thread rayon pool")
}

fn convert_in_pool(input: &Path, output: &Path) {
    let (input, output) = (input.to_owned(), output.to_owned());
    within(Duration::from_secs(60), move || {
        one_thread_pool().install(|| {
            convert_to_overviews(
                &input,
                &output,
                &ConvertOptions {
                    levels: LevelPlan::ZoomRange {
                        min_zoom: 0,
                        max_zoom: 12,
                    },
                    ..Default::default()
                },
            )
            .expect("convert");
        });
    });
}

#[test]
fn convert_finishes_inside_a_one_thread_pool() {
    let Some(input) = fixture::realdata("open-buildings.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    convert_in_pool(&input, &dir.path().join("overview.parquet"));
}

#[test]
fn export_finishes_inside_a_one_thread_pool() {
    let Some(input) = fixture::realdata("road-detections.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = dir.path().join("overview.parquet");
    // Convert outside any pool, so this test isolates export.
    convert_to_overviews(&input, &overview, &ConvertOptions::default()).expect("convert");
    let archive = dir.path().join("out.pmtiles");
    within(Duration::from_secs(60), move || {
        one_thread_pool().install(|| {
            export_pmtiles(&overview, &archive, &ExportOptions::default()).expect("export");
        });
    });
}
