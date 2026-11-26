//! #876: a library caller gets its profile's worker pool, as the CLI and the
//! Python bindings do. Its own binary, so no other test builds the pool first.

use tylertoo_core::{convert_to_overviews, ConvertOptions, MemoryProfile};

#[test]
fn a_library_conversion_sizes_the_pool_for_its_profile() {
    if std::env::var_os("RAYON_NUM_THREADS").is_some() {
        eprintln!("RAYON_NUM_THREADS sizes the pool; nothing to check");
        return;
    }
    let mut options = ConvertOptions::default();
    options.profile = MemoryProfile::Bounded;
    let dir = tempfile::tempdir().expect("tempdir");
    // The input is missing, so the run fails, but only after it validated.
    let run = convert_to_overviews(
        dir.path().join("missing.parquet"),
        dir.path().join("out.parquet"),
        &options,
    );
    assert!(run.is_err());
    assert!(
        rayon::ThreadPoolBuilder::new().build_global().is_err(),
        "the conversion must build the global pool"
    );
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    assert_eq!(
        rayon::current_num_threads(),
        cores.min(tylertoo_core::internal::overview::convert::WORKER_THREADS_MAX)
    );
}
