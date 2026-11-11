//! A conversion or a variable-depth export in a custom pool must not need an
//! idle global-pool worker, including when the writer fails. Own binary: the
//! global pool is sized once per process, independently of the global-worker
//! regression test.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};

mod rayon_support;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

type Finished = Result<Result<(), String>, mpsc::RecvTimeoutError>;

#[expect(
    clippy::disallowed_types,
    reason = "initialize the process-wide test pool once"
)]
fn global_pool() -> std::sync::MutexGuard<'static, ()> {
    // Plain cargo test shares a process; two tests must not each occupy
    // half the global pool while waiting for its other half to become busy.
    static TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());
    // A test that panicked holding the lock must not fail the next one.
    let held = TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(2)
            .build_global()
            .expect("size the global pool before anything uses it");
    });
    held
}

#[test]
fn convert_in_a_custom_pool_finishes_with_every_global_worker_busy() {
    let Some(input) = fixture::realdata("ftw-moldova-z10.parquet") else {
        return;
    };
    let _global = global_pool();
    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("overview.parquet");
    let (release, held) = crossbeam_channel::unbounded::<()>();
    for _ in 0..2 {
        let (busy_tx, busy) = mpsc::channel();
        let held = held.clone();
        rayon::spawn(move || {
            busy_tx.send(()).expect("signal busy");
            let _ = held.recv_timeout(Duration::from_secs(600));
        });
        busy.recv_timeout(Duration::from_secs(10))
            .expect("a global worker is busy");
    }
    let pools = [1, 2].map(|threads| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("custom pool")
    });
    for custom in &pools {
        let converted = finishes(&input, &output, |job| custom.spawn(job));
        if converted.is_err() {
            drop(release);
            panic!("deadlocked in a custom pool with every global worker busy: {converted:?}");
        }
        converted.unwrap().expect("convert");
        let failed = cfg!(target_os = "linux")
            .then(|| finishes(&input, Path::new("/dev/full"), |job| custom.spawn(job)));
        if let Some(failed) = failed {
            if failed.is_err() {
                drop(release);
                panic!("deadlocked in a custom pool after a write error: {failed:?}");
            }
            failed.unwrap().expect_err("writes to /dev/full fail");
        }
    }
}

#[test]
fn variable_depth_export_finishes_with_every_global_worker_busy() {
    let _global = global_pool();
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = rayon_support::overview(dir.path());
    let expected = rayon_support::export(&overview, &dir.path().join("expected.pmtiles"));
    let (release, held) = crossbeam_channel::unbounded::<()>();
    for _ in 0..2 {
        let (busy_tx, busy) = mpsc::channel();
        let held = held.clone();
        rayon::spawn(move || {
            busy_tx.send(()).expect("signal busy");
            let _ = held.recv_timeout(Duration::from_secs(300));
        });
        busy.recv_timeout(Duration::from_secs(10))
            .expect("global worker busy");
    }
    for threads in [1, 2] {
        let custom = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let output = dir.path().join(format!("custom-{threads}.pmtiles"));
        let (tx, rx) = mpsc::channel();
        let overview = overview.clone();
        custom.spawn(move || {
            let _ = tx.send(rayon_support::export(&overview, &output));
        });
        let result = rx.recv_timeout(Duration::from_secs(30));
        if result.is_err() {
            // Release blockers before the assertion, and stop at the first timeout.
            drop(release);
            panic!("variable-depth export deadlocked in custom pool: {result:?}");
        }
        assert_eq!(result.unwrap(), expected, "tile bytes changed");
    }
}

/// Convert in a job handed to `spawn`; `Err` when it does not finish.
fn finishes(input: &Path, output: &Path, spawn: impl FnOnce(Box<dyn FnOnce() + Send>)) -> Finished {
    let (input, output) = (input.to_owned(), output.to_owned());
    let (tx, rx) = mpsc::channel();
    spawn(Box::new(move || {
        let result = convert_to_overviews(
            &input,
            &output,
            &ConvertOptions::default()
                // Several batches per window: a one-batch window skips
                // the parallel iterator and can hide the helper's pool.
                .with_read_batch_size(256)
                .with_in_flight_batches(16)
                .with_levels(LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 12,
                }),
        );
        let _ = tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
    }));
    rx.recv_timeout(Duration::from_secs(120))
}
