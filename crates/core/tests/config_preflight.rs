use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions};

#[test]
fn plan_sources_validates_before_opening_input() {
    use tylertoo_core::input_set::ConvertSource;
    use tylertoo_core::overview::convert::write_convert_plan_sources;
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("corrupt.parquet");
    std::fs::write(&input, b"corrupt").unwrap();
    let source = ConvertSource::resolve_path(&input).unwrap();
    let opts = ConvertOptions {
        levels: tylertoo_core::overview::convert::LevelPlan::Gsds(vec![]),
        properties: tylertoo_core::overview::properties::PropertySelection {
            exclude_all: true,
            ..Default::default()
        },
        save_plan: Some(dir.path().join("plan.arrow")),
        ..Default::default()
    };
    let err = write_convert_plan_sources(&source, &opts).unwrap_err();
    assert!(err.is_config(), "expected config error before input: {err}");
    assert!(!opts.save_plan.unwrap().exists());
}

#[test]
fn path_validates_before_resolving_partitions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.parquet"), b"corrupt").unwrap();
    std::fs::write(dir.path().join("b.parquet"), b"corrupt").unwrap();
    let opts = ConvertOptions {
        gsd_base: 0.0,
        ..Default::default()
    };
    let err = convert_to_overviews(dir.path(), dir.path().join("out.parquet"), &opts).unwrap_err();
    assert!(
        err.is_config(),
        "expected config error before opening input: {err}"
    );
}

#[test]
fn core_owns_read_bounds() {
    for size in [0, 1_048_577] {
        let opts = ConvertOptions {
            read_batch_size: size,
            ..Default::default()
        };
        assert!(opts.validate().is_err(), "accepted read_batch_size={size}");
    }
    for size in [1, 1_048_576] {
        ConvertOptions {
            read_batch_size: size,
            ..Default::default()
        }
        .validate()
        .unwrap();
    }
    let opts = ConvertOptions {
        read_workers: tylertoo_core::overview::convert::read_workers_ceiling() + 1,
        ..Default::default()
    };
    assert!(opts.validate().is_err(), "accepted excess read_workers");
}

#[test]
fn shard_plan_existing_output_explains_fleet_identity() {
    use tylertoo_core::overview::config::{check_output, OutputKind};
    let file = tempfile::NamedTempFile::new().unwrap();
    let err = check_output(file.path(), OutputKind::ShardPlan, false).unwrap_err();
    let message = err.to_string();
    assert!(message.contains("SAME shard plan"), "{message}");
    assert!(message.contains("silently re-cuts"), "{message}");
}

#[cfg(not(feature = "testing"))]
#[test]
fn production_ignores_scaled_limit_environment() {
    std::env::set_var("TYLERTOO_BYTE_ARRAY_LIMIT_BYTES", "1");
    assert_eq!(tylertoo_io::byte_array_limit(), i32::MAX as u64);
    std::env::remove_var("TYLERTOO_BYTE_ARRAY_LIMIT_BYTES");
}
