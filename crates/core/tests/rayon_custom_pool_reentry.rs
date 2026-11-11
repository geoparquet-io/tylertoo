//! A conversion in a custom pool must not need an idle global-pool worker,
//! including when the writer fails. Own binary: the global pool is sized
//! once per process, independently of the global-worker regression test.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

type Finished = Result<Result<(), String>, mpsc::RecvTimeoutError>;

#[test]
fn convert_in_a_custom_pool_finishes_with_every_global_worker_busy() {
    let Some(input) = fixture::realdata("ftw-moldova-z10.parquet") else {
        return;
    };
    rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build_global()
        .expect("size the global pool before anything uses it");
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
    let results = pools.each_ref().map(|custom| {
        let converted = finishes(&input, &output, |job| custom.spawn(job));
        let failed = cfg!(target_os = "linux")
            .then(|| finishes(&input, Path::new("/dev/full"), |job| custom.spawn(job)));
        (converted, failed)
    });
    // Release even on timeout, before asserting or dropping the custom pool.
    drop(release);

    for (converted, failed) in results {
        converted
            .expect("deadlocked in a custom pool with every global worker busy")
            .expect("convert");
        if let Some(failed) = failed {
            failed
                .expect("deadlocked in a custom pool after a write error")
                .expect_err("writes to /dev/full fail");
        }
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
