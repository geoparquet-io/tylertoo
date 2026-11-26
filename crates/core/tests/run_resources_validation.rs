//! #876: a run's process-wide resources (the worker pool, the memory limit)
//! are applied only once its whole configuration passed, from one owner per
//! run. Its own binary: the global pool is built at most once per process,
//! so these tests must see it unbuilt, and none of them may build it.

use tylertoo_core::{ConvertOptions, ExportOptions, PyramidOptions, TilesOptions};

/// A configuration whose export rules pass and whose convert rules fail
/// (`read_batch_size` 0) is refused without building the pool its
/// `workers` names: nested validation used to apply the export's first.
#[test]
fn a_rejected_configuration_builds_no_pool() {
    let bad_convert = ConvertOptions::default().with_read_batch_size(0);

    let mut tiles = TilesOptions::default();
    tiles.convert = bad_convert.clone().with_workers(Some(3));
    assert!(tiles.validate().is_err());

    let mut pyramid = PyramidOptions::default();
    pyramid.convert = bad_convert;
    pyramid.export = ExportOptions::default().with_workers(Some(3));
    assert!(pyramid.validate().is_err());

    assert!(
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build_global()
            .is_ok(),
        "a rejected configuration built the global pool"
    );
}

/// A pyramid's resources are its export's: a band's convert that names its
/// own would change the run's limit between phases, so it is refused.
#[test]
fn a_pyramid_refuses_convert_resources() {
    for convert in [
        ConvertOptions::default().with_workers(Some(2)),
        ConvertOptions::default().with_memory_limit(Some(1 << 30)),
    ] {
        let mut pyramid = PyramidOptions::default();
        pyramid.convert = convert;
        let Err(err) = pyramid.validate() else {
            panic!("convert resources are refused");
        };
        let err = err.to_string();
        assert!(err.contains("PyramidOptions::export's workers"), "{err}");
    }
}

/// The valve-floor oracle validates its options before it opens the
/// overview, as the export it predicts does.
#[test]
fn the_valve_floor_oracle_validates_its_options_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let options = ExportOptions::default().with_workers(Some(0));
    let Err(err) = tylertoo_core::internal::overview::export::predicted_valve_floors(
        &dir.path().join("missing.parquet"),
        &options,
    ) else {
        panic!("workers 0 is refused");
    };
    let err = err.to_string();
    assert!(
        err.contains("--workers / workers must be at least 1"),
        "{err}"
    );
}
