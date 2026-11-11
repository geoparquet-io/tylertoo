//! Convert must finish on a worker of rayon's *global* pool while every
//! other worker of that pool is busy (another conversion, a caller's job).
//!
//! The RAM drain rebuilds shared level batches (#852) with a parallel
//! iterator on a helper thread, which queues that work on the global pool.
//! A writer that blocked in a plain `recv` for those batches parked the one
//! worker that could build them. Its own binary, because the global pool
//! can be sized once per process.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

#[test]
fn convert_finishes_on_a_global_pool_worker_while_the_other_is_busy() {
    let Some(input) = fixture::realdata("ftw-moldova-z10.parquet") else {
        return;
    };
    // Two workers, so the drain rebuilds more than one batch at a time.
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build_global()
        .expect("size the global pool before anything uses it");
    let (release, held) = mpsc::channel::<()>();
    let (busy_tx, busy) = mpsc::channel();
    rayon::spawn(move || {
        busy_tx.send(()).expect("signal busy");
        let _ = held.recv_timeout(Duration::from_secs(120));
    });
    busy.recv().expect("one worker is busy");

    let dir = tempfile::tempdir().expect("tempdir");
    let outcome = convert_on_the_pool(&input, &dir.path().join("overview.parquet"));
    // A writer that fails mid-drain must report its error, not wait on a
    // rebuild only this worker could run. Linux's `/dev/full` fails writes.
    let failed =
        cfg!(target_os = "linux").then(|| convert_on_the_pool(&input, Path::new("/dev/full")));
    let _ = release.send(());
    outcome
        .expect("deadlocked on a global pool worker")
        .expect("convert");
    if let Some(failed) = failed {
        failed
            .expect("deadlocked on a global pool worker after a write error")
            .expect_err("writes to /dev/full fail");
    }
}

/// Convert on a global pool worker; `Err` when it does not finish.
fn convert_on_the_pool(
    input: &Path,
    output: &Path,
) -> Result<Result<(), String>, mpsc::RecvTimeoutError> {
    let (input, output) = (input.to_owned(), output.to_owned());
    let (tx, rx) = mpsc::channel();
    rayon::spawn(move || {
        let result = convert_to_overviews(
            &input,
            &output,
            &ConvertOptions::default().with_levels(LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 12,
            }),
        );
        let _ = tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
    });
    rx.recv_timeout(Duration::from_secs(60))
}
