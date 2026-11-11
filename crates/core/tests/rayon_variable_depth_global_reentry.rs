//! Variable-depth export must help probe work queued on its only global worker.

use std::sync::mpsc;
use std::time::Duration;

mod rayon_support;

#[test]
fn variable_depth_export_finishes_on_the_only_global_worker() {
    rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global()
        .unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = rayon_support::overview(dir.path());
    let expected = rayon_support::export(&overview, &dir.path().join("expected.pmtiles"));
    let output = dir.path().join("worker.pmtiles");
    let (tx, rx) = mpsc::channel();
    rayon::spawn(move || {
        let _ = tx.send(rayon_support::export(&overview, &output));
    });
    let actual = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("variable-depth export deadlocked on the only global worker");
    assert_eq!(actual, expected, "tile bytes changed");
}
