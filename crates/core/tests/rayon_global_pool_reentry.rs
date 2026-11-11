//! Convert must finish on a rayon worker whose pool's other workers are
//! busy (another conversion, a caller's job), and inside a custom pool
//! while every global worker is busy.
//!
//! The RAM drain rebuilds shared level batches (#852) with a parallel
//! iterator. Run on a helper thread, that work queued on the global pool,
//! and the writer's worker waited for it without helping: with the other
//! workers busy, or the conversion in a custom pool, nothing built it. Its
//! own binary, because the global pool can be sized once per process.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

type Finished = Result<Result<(), String>, mpsc::RecvTimeoutError>;

#[test]
fn convert_finishes_while_the_other_pool_workers_are_busy() {
    let Some(input) = fixture::realdata("ftw-moldova-z10.parquet") else {
        return;
    };
    // Two workers, so the drain rebuilds more than one batch at a time.
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build_global()
        .expect("size the global pool before anything uses it");
    let dir = tempfile::tempdir().expect("tempdir");
    let output = dir.path().join("overview.parquet");
    let (release, held) = crossbeam_channel::unbounded::<()>();
    let occupy_a_global_worker = || {
        let (busy_tx, busy) = mpsc::channel();
        let held = held.clone();
        rayon::spawn(move || {
            busy_tx.send(()).expect("signal busy");
            let _ = held.recv_timeout(Duration::from_secs(300));
        });
        busy.recv().expect("a global worker is busy");
    };

    occupy_a_global_worker();
    let on_global = finishes(&input, &output, rayon::spawn);
    // A writer that fails mid-drain must report its error, not wait on a
    // rebuild only its worker could run. Linux's `/dev/full` fails writes.
    let failed_on_global =
        cfg!(target_os = "linux").then(|| finishes(&input, Path::new("/dev/full"), rayon::spawn));
    occupy_a_global_worker();
    let custom = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .expect("custom pool");
    let in_custom = finishes(&input, &output, |job| custom.spawn(job));
    drop(release);

    on_global
        .expect("deadlocked on a global pool worker")
        .expect("convert");
    if let Some(failed) = failed_on_global {
        failed
            .expect("deadlocked on a global pool worker after a write error")
            .expect_err("writes to /dev/full fail");
    }
    in_custom
        .expect("deadlocked in a custom pool with the global pool busy")
        .expect("convert");
}

/// Convert in a job handed to `spawn`; `Err` when it does not finish.
fn finishes(input: &Path, output: &Path, spawn: impl FnOnce(Box<dyn FnOnce() + Send>)) -> Finished {
    let (input, output) = (input.to_owned(), output.to_owned());
    let (tx, rx) = mpsc::channel();
    spawn(Box::new(move || {
        let result = convert_to_overviews(
            &input,
            &output,
            &ConvertOptions::default().with_levels(LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 12,
            }),
        );
        let _ = tx.send(result.map(|_| ()).map_err(|e| e.to_string()));
    }));
    rx.recv_timeout(Duration::from_secs(60))
}
