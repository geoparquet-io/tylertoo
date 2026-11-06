//! Property: `TilesOptions::validate` agrees with the documented range of
//! each option, and a refusal names the option (#677; replaces mutation
//! testing for the options owner).
//!
//! The oracle is the documented range (`docs/OVERVIEW_TUNING.md`, the CLI
//! help, `core_owns_read_bounds`), not core's code, so loosening a bound
//! fails. `None` means the range is undocumented and only the name is checked.
use proptest::prelude::*;
use tylertoo_core::internal::overview::tiles::TilesOptions;

/// The flag each refusal must name.
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

/// Whether the documented range accepts `v` (`None`: not documented).
fn documented(knob: usize, v: f64) -> Option<bool> {
    let non_finite = !v.is_finite();
    match knob {
        0 => Some((1.0..=1_048_576.0).contains(&v.trunc()) && v >= 0.0),
        1 => non_finite.then_some(false),
        2 => Some(!non_finite && v >= 0.0),
        // "exponent (>=1)" in OVERVIEW_TUNING.md.
        3 => Some(v.is_finite() && v >= 1.0),
        _ => (v >= 0.0).then_some(v.trunc() >= 1.0),
    }
}

fn check(knob: usize, v: f64) -> Result<(), TestCaseError> {
    let mut o = TilesOptions::default();
    apply(&mut o, knob, v);
    let verdict = o.validate().map(|_| ());
    if let Some(expected) = documented(knob, v) {
        prop_assert_eq!(
            verdict.is_ok(),
            expected,
            "{}={}: {:?}",
            KNOBS[knob],
            v,
            verdict
        );
    }
    if let Err(e) = verdict {
        prop_assert!(
            e.to_string().contains(KNOBS[knob]),
            "{}={v}: {e}",
            KNOBS[knob]
        );
    }
    Ok(())
}

fn value() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        Just(0.0),
        Just(1.0),
        Just(1_048_576.0),
        Just(1_048_577.0),
        -10.0..5000.0f64,
        0.0..2_000_000.0f64,
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn documented_ranges(knob in 0usize..5, v in value()) {
        check(knob, v)?;
    }
}

#[test]
fn documented_boundaries() {
    for v in [0.0, 1.0, 2.0, 1_048_575.0, 1_048_576.0, 1_048_577.0] {
        check(0, v).unwrap();
    }
    for knob in [1, 2, 3] {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            check(knob, v).unwrap();
        }
    }
    for v in [0.0, 1.0, 4096.0] {
        check(4, v).unwrap();
        check(2, v).unwrap();
    }
    check(3, 1.0).unwrap();
    // A negative simplify factor is refused (it used to behave like 0).
    check(2, -1.0).unwrap();
    check(2, -0.0).unwrap();
}

/// `--drop-gamma` is an exponent >= 1; below that is refused, naming the flag.
#[test]
fn drop_gamma_below_one_is_refused() {
    for v in [0.0, 0.5, -1.0, f64::NAN] {
        let mut o = TilesOptions::default();
        o.convert.density.gamma = v;
        let e = o.validate().expect_err("gamma below 1 must be refused");
        assert!(e.to_string().contains("drop-gamma"), "{e}");
        assert!(
            v.is_nan() || e.to_string().contains("must be at least 1"),
            "{e}"
        );
    }
}

#[test]
fn drop_gamma_is_not_checked_when_the_density_budget_is_off() {
    let mut o = TilesOptions::default();
    o.convert.density.enabled = false;
    o.convert.density.gamma = 0.5;
    o.validate()
        .expect("--no-density-drop makes --drop-gamma unused, so it must not be refused");
}
