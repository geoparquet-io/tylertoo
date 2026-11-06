//! Property: `TilesOptions::validate` and the facade agree, and a refusal
//! names the option (#677; replaces mutation testing for the options owner).
//!
//! The input is a corrupt file, so the facade never gets past the options:
//! an accepted configuration fails later with a non-config error, a refused
//! one fails first with a config error whose message names the field.
use proptest::prelude::*;
use tylertoo_core::overview::tiles::{convert_to_pmtiles, TilesInput, TilesOptions};

/// Each knob: how to set it and the flag its refusal must name.
const KNOBS: [&str; 5] = [
    "read-batch-size",
    "gsd-base",
    "simplify-factor",
    "drop-gamma",
    "extent",
];

fn apply(o: &mut TilesOptions, knob: usize, v: f64) {
    match knob {
        0 => o.convert.read_batch_size = v as usize,
        1 => o.convert.gsd_base = v,
        2 => o.convert.simplify.factor = v,
        3 => o.convert.density.gamma = v,
        _ => o.export.extent = v as u32,
    }
}

fn value() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(0.0),
        Just(-1.0),
        Just(1_048_577.0),
        -10.0..5000.0f64,
        0.0..2_000_000.0f64,
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn accepted_never_config_error_and_refused_names_the_field(knob in 0usize..5, v in value()) {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("corrupt.parquet");
        std::fs::write(&input, b"corrupt").unwrap();
        let mut o = TilesOptions::default();
        apply(&mut o, knob, v);
        let validated = o.validate().map(|_| ());
        let ran = convert_to_pmtiles(
            &TilesInput::Path(input),
            &dir.path().join("out.pmtiles"),
            &o,
        )
        .unwrap_err();
        match validated {
            Ok(()) => prop_assert!(!ran.is_config(), "accepted {knob}={v} but facade refused: {ran}"),
            Err(e) => {
                prop_assert!(ran.is_config(), "refused {knob}={v} but facade did not: {ran}");
                let msg = e.to_string();
                prop_assert!(msg.contains(KNOBS[knob]), "refusal of {knob}={v} does not name {}: {msg}", KNOBS[knob]);
            }
        }
    }
}
