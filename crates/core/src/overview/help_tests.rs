//! Numeric advice is checked against the owners of the decisions.
use crate::overview::{assign, cluster, convert, export, level};
use clap::Args;

fn numbers(text: &str) -> Vec<f64> {
    text.split(|c: char| !c.is_ascii_digit() && c != '.')
        .filter_map(|word| word.trim_end_matches('.').parse().ok())
        .collect()
}

#[test]
fn help_numbers_match_their_owners() {
    let commands = [
        export::ExportOptions::augment_args(clap::Command::new("export")),
        convert::ConvertOptions::augment_args(clap::Command::new("convert")),
    ];
    let cases: &[(&str, &[f64])] = &[
        ("point_thinning", &[assign::CLUSTER_POINT_THINNING_DEFAULT]),
        ("workers", &[convert::WORKER_THREADS_MAX as f64]),
        ("read_workers", &[convert::READ_WORKERS_MAX as f64]),
        ("tile_buffer", &[export::MAX_TILE_BUFFER_PX as f64]),
        ("size_valve", &[export::valve::MIN_VALVE_EXTENT as f64]),
        (
            "valve_max_step",
            &[
                export::valve_predict::MAX_RINGS as f64,
                (export::valve_predict::MAX_RINGS + 1) as f64,
            ],
        ),
        (
            "max_zoom_simplify_factor",
            &[export::DEFAULT_LINE_MAX_ZOOM_SIMPLIFY_FACTOR],
        ),
        (
            "cluster_categories",
            &[cluster::MAX_CLUSTER_CATEGORIES as f64],
        ),
        ("gsd_base", &[level::WEBMERC_CIRCUMFERENCE_M]),
        (
            "mlt_profile",
            &[
                u32::BITS as f64,
                crate::tile_format::MAX_EXACT_DOUBLE_INTEGER as f64,
                u64::BITS as f64,
            ],
        ),
    ];
    for (id, expected) in cases {
        let args: Vec<_> = commands
            .iter()
            .flat_map(|cmd| cmd.get_arguments())
            .filter(|arg| arg.get_id() == id)
            .collect();
        assert!(!args.is_empty(), "missing {id}");
        for arg in args {
            assert_eq!(
                numbers(&arg.get_help().unwrap().to_string()),
                *expected,
                "{id}"
            );
        }
    }
    for (id, bytes) in [
        ("tile_size_limit", export::DEFAULT_TILE_SIZE_LIMIT),
        (
            "max_compressed_tile_size",
            export::DEFAULT_COMPRESSED_TILE_SIZE_LIMIT,
        ),
    ] {
        let arg = commands[0]
            .get_arguments()
            .find(|arg| arg.get_id() == id)
            .unwrap_or_else(|| panic!("missing {id}"));
        let help = arg.get_help().unwrap().to_string();
        let size = convert::human_bytes(bytes as u64);
        assert!(help.contains(&size), "{id} help must state {size}: {help}");
    }
}
