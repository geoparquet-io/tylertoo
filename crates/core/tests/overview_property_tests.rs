//! Property-based invariant tests for the overview pipeline stages
//! (issue #187, follow-up to the H4 hostile-input hardening in PR #186).
//!
//! The deterministic H4 tests pin *known* hostile classes; these properties
//! search for the classes we didn't think of by throwing arbitrary valid
//! geometries × arbitrary valid configurations at the pure stages:
//!
//! - **assign** ([`assign_levels`]): structural invariants (parallel output,
//!   bounded levels, exact partitioning across levels, canonical
//!   completeness), determinism, and input-order independence.
//! - **visibility gate**: a stricter gate never yields MORE rows at the
//!   coarsest level (monotonicity).
//! - **density budget** ([`apply_density_budget`]): never promotes a feature
//!   to a coarser level than cell-winner allowed, respects the per-level
//!   budget ceiling, keeps the canonical level complete, and a stricter
//!   `drop_rate` never yields MORE rows at any level. Disabled = identity.
//! - **clustering** ([`build_cluster_tables`]): the spec §12.1 strict sum
//!   invariant ([`verify_sum_invariant`]) holds under random inputs, plus
//!   table structure (finest level empty, `point_count >= 2`, keys are
//!   present point features).
//! - **coalescing** ([`coalesce_level_lines`]): chains partition the input
//!   (`Σ count == n` when nothing is gated/thinned; `Σ count <= n` always),
//!   `count >= 1`, unique ascending reps drawn from the input, determinism.
//!
//! # Why there is no raw grid-size ("thinning factor") monotonicity property
//!
//! "Stricter thinning factor ⇒ never more rows" is NOT a true invariant of
//! floor-quantized grids: coarsening a non-nested grid can SPLIT points that
//! shared a fine cell (e.g. x = 2.9 and 3.1 share cell ⌊x/2⌋ = 1 but land in
//! cells ⌊x/3⌋ = 0 and 1), so an occupied-cell count can locally increase
//! with the cell size. The monotone knobs are the visibility gate (removing
//! features from a FIXED grid can only vacate cells) and the density-budget
//! `drop_rate` (an explicit per-level ceiling); both are tested below.
//!
//! # Runtime budget
//!
//! Case counts default to 64 per property (well under a second per property;
//! the whole file adds only a few seconds). Crank locally with:
//!
//! ```text
//! PROPTEST_CASES=4096 cargo test --package tylertoo-core \
//!     --test overview_property_tests
//! ```

use std::collections::HashMap;

use geo::{Geometry, LineString, MultiLineString, Point};
use proptest::prelude::*;

use tylertoo_core::overview::assign::{
    apply_density_budget, assign_levels, AssignConfig, AssignFeature, Assignment, Crs,
    DensityBudgetConfig, FeatureKind, SortDirection, MIN_DENSITY_LEVEL_FEATURES,
};
use tylertoo_core::overview::cluster::{build_cluster_tables, verify_sum_invariant, AccumulateOp};
use tylertoo_core::overview::coalesce::{coalesce_level_lines, CoalesceInput, CoalesceParams};

/// Default proptest cases per property, overridable via `PROPTEST_CASES`.
const DEFAULT_CASES: u32 = 64;

fn cases() -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CASES);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

/// Spec §5.2 GSD (meters) for a zoom level.
fn gsd(z: u32) -> f64 {
    40_075_016.69 / 1024.0 / 2f64.powi(z as i32)
}

// ============================================================================
// Generators
// ============================================================================

/// A finite coordinate: world-scale (3857-ish), degree-scale (4326-ish),
/// plus antimeridian-adjacent edges and exact zero.
fn coord() -> impl Strategy<Value = f64> {
    prop_oneof![
        4 => -2.0e7..2.0e7f64,
        2 => -180.0..180.0f64,
        1 => Just(20_037_508.34),
        1 => Just(-20_037_508.34),
        1 => Just(179.999_999),
        1 => Just(-179.999_999),
        1 => Just(0.0),
    ]
}

/// A bbox extent: zero (degenerate point-like), tiny, or normal-sized.
fn extent() -> impl Strategy<Value = f64> {
    prop_oneof![
        2 => Just(0.0),
        2 => 1e-12..1e-6f64,
        4 => 1e-6..1e6f64,
    ]
}

fn kind() -> impl Strategy<Value = FeatureKind> {
    prop_oneof![
        Just(FeatureKind::Point),
        Just(FeatureKind::Line),
        Just(FeatureKind::Polygon),
    ]
}

fn sort_key() -> impl Strategy<Value = Option<f64>> {
    prop::option::of(-1e9..1e9f64)
}

/// A vector of valid features with unique indices `0..n` (matching how the
/// pipeline feeds row positions in). Includes identical/coincident features
/// with some probability via the shared-coordinate weights in [`coord`].
fn features(max: usize) -> impl Strategy<Value = Vec<AssignFeature>> {
    prop::collection::vec(
        (coord(), coord(), extent(), extent(), kind(), sort_key()),
        0..max,
    )
    .prop_map(|parts| {
        parts
            .into_iter()
            .enumerate()
            .map(|(i, (x, y, w, h, kind, sort_key))| AssignFeature {
                index: i,
                bbox: [x, y, x + w, y + h],
                kind,
                sort_key,
            })
            .collect()
    })
}

/// An [`AssignConfig`] that passes `convert::validate_options`: finite
/// positive thinning factors, finite non-negative visibility gates.
fn assign_config() -> impl Strategy<Value = AssignConfig> {
    (
        0.1..64.0f64,
        0.1..64.0f64,
        0.1..64.0f64,
        0.0..16.0f64,
        0.0..16.0f64,
        prop_oneof![Just(SortDirection::Desc), Just(SortDirection::Asc)],
    )
        .prop_map(|(pt, lt, at, lv, av, sort_direction)| AssignConfig {
            point_thinning: pt,
            line_thinning: lt,
            polygon_thinning: at,
            line_visibility: lv,
            polygon_visibility: av,
            sort_direction,
        })
}

/// Level GSD lists: 1..=5 levels, coarse→fine, strictly decreasing (spec
/// §5.2 table starting at an arbitrary coarsest zoom).
fn level_gsds() -> impl Strategy<Value = Vec<f64>> {
    (0u32..12, 1usize..6).prop_map(|(z0, n)| (0..n).map(|i| gsd(z0 + i as u32)).collect())
}

fn crs() -> impl Strategy<Value = Crs> {
    prop_oneof![Just(Crs::Epsg3857), Just(Crs::Epsg4326)]
}

/// A valid, enabled-or-not density budget within sensible knob bounds.
fn budget_config() -> impl Strategy<Value = DensityBudgetConfig> {
    (any::<bool>(), 1.05..4.0f64, 1.0..3.0f64).prop_map(|(enabled, drop_rate, gamma)| {
        DensityBudgetConfig {
            enabled,
            drop_rate,
            gamma,
        }
    })
}

/// The per-level budget ceiling `apply_density_budget` enforces (mirrors its
/// internal `effective_budget`).
fn budget_ceiling(n: usize, num_levels: usize, level: usize, drop_rate: f64) -> usize {
    let finest = num_levels - 1;
    if level >= finest {
        return n;
    }
    let raw = n as f64 * (1.0 / drop_rate).powi((finest - level) as i32);
    (raw.round() as usize).max(MIN_DENSITY_LEVEL_FEATURES)
}

fn min_level_map(a: &Assignment) -> HashMap<usize, u8> {
    a.assignments
        .iter()
        .map(|f| (f.index, f.min_level))
        .collect()
}

// ============================================================================
// Assign stage
// ============================================================================

proptest! {
    #![proptest_config(cases())]

    /// Structural invariants of `assign_levels` for any valid input:
    /// - output is parallel to the input (same length, index echoed);
    /// - every `min_level` is within `[0, num_levels)`;
    /// - every feature is assigned to EXACTLY one level: the per-level
    ///   partitioning sets are disjoint and their sizes sum to the input
    ///   row count (nothing dropped, nothing duplicated);
    /// - duplicating counts are monotone non-decreasing with level and the
    ///   finest (canonical) level contains every feature (spec §2.4).
    #[test]
    fn assign_structural_invariants(
        feats in features(48),
        gsds in level_gsds(),
        config in assign_config(),
        crs in crs(),
    ) {
        let out = assign_levels(&feats, &gsds, &config, crs);
        let num_levels = gsds.len() as u8;

        prop_assert_eq!(out.num_levels, num_levels);
        prop_assert_eq!(out.assignments.len(), feats.len());
        for (f, a) in feats.iter().zip(&out.assignments) {
            prop_assert_eq!(a.index, f.index, "index echo broken");
            prop_assert!(a.min_level < num_levels, "min_level out of range");
        }

        // Exact partition across levels.
        let mut total = 0usize;
        let mut prev_dup = 0usize;
        for level in 0..num_levels {
            let part = out.partitioning_at_level(level);
            total += part.len();
            let dup = out.duplicating_at_level(level).len();
            prop_assert!(dup >= prev_dup, "duplicating counts must be monotone");
            prop_assert_eq!(
                dup, total,
                "duplicating(L) must equal the union of partitions 0..=L"
            );
            prev_dup = dup;
        }
        prop_assert_eq!(total, feats.len(), "partition must cover every row exactly once");
        prop_assert_eq!(
            out.duplicating_at_level(num_levels - 1).len(),
            feats.len(),
            "canonical level must contain every feature"
        );
    }

    /// `assign_levels` is deterministic and independent of input order: the
    /// same rows presented in a different order yield the same
    /// `index -> min_level` mapping. (The row-alignment bug class from
    /// PR #186 is exactly a violation of this property.)
    #[test]
    fn assign_deterministic_and_order_independent(
        (feats, shuffled) in features(48)
            .prop_flat_map(|f| {
                let orig = f.clone();
                (Just(orig), Just(f).prop_shuffle())
            }),
        gsds in level_gsds(),
        config in assign_config(),
        crs in crs(),
    ) {
        let a = assign_levels(&feats, &gsds, &config, crs);
        let b = assign_levels(&feats, &gsds, &config, crs);
        prop_assert_eq!(a.assignments.clone(), b.assignments.clone(), "same input must reproduce byte-identically");

        let c = assign_levels(&shuffled, &gsds, &config, crs);
        prop_assert_eq!(
            min_level_map(&a),
            min_level_map(&c),
            "assignment must not depend on input row order"
        );
    }

    /// Visibility-gate monotonicity: raising the line/polygon visibility
    /// gates only removes features from a FIXED grid, so the number of
    /// coarsest-level (level 0) rows can never increase.
    #[test]
    fn assign_visibility_gate_monotone(
        feats in features(48),
        gsds in level_gsds(),
        config in assign_config(),
        crs in crs(),
        bump in 0.0..16.0f64,
    ) {
        let stricter = AssignConfig {
            line_visibility: config.line_visibility + bump,
            polygon_visibility: config.polygon_visibility + bump,
            ..config
        };
        let base = assign_levels(&feats, &gsds, &config, crs);
        let gated = assign_levels(&feats, &gsds, &stricter, crs);
        prop_assert!(
            gated.duplicating_at_level(0).len() <= base.duplicating_at_level(0).len(),
            "a stricter visibility gate must never yield MORE coarsest-level rows"
        );
    }
}

// ============================================================================
// Density budget stage
// ============================================================================

proptest! {
    #![proptest_config(cases())]

    /// Density-budget invariants on top of any cell-winner assignment:
    /// - the output stays parallel (index echo, level bounds, partition);
    /// - a feature is never PROMOTED to a coarser level than cell-winner
    ///   assigned (the budget only defers);
    /// - every non-canonical level respects its budget ceiling;
    /// - the canonical level keeps every feature (spec §2.4);
    /// - `enabled = false` is a byte-for-byte identity;
    /// - the whole stage is deterministic.
    #[test]
    fn density_budget_invariants(
        feats in features(48),
        gsds in level_gsds(),
        config in assign_config(),
        budget in budget_config(),
        crs in crs(),
    ) {
        let cw = assign_levels(&feats, &gsds, &config, crs);
        let out = apply_density_budget(&cw, &feats, &gsds, &config, &budget, crs);
        let out2 = apply_density_budget(&cw, &feats, &gsds, &config, &budget, crs);
        prop_assert_eq!(out.assignments.clone(), out2.assignments.clone(), "budget must be deterministic");

        prop_assert_eq!(out.assignments.len(), feats.len());
        prop_assert_eq!(out.num_levels, cw.num_levels);
        let num_levels = gsds.len();
        for (a, b) in cw.assignments.iter().zip(&out.assignments) {
            prop_assert_eq!(a.index, b.index, "index echo broken");
            prop_assert!(b.min_level < num_levels as u8);
            prop_assert!(
                b.min_level >= a.min_level,
                "budget must never promote a feature to a coarser level \
                 (cw = {}, budgeted = {})", a.min_level, b.min_level
            );
        }

        prop_assert_eq!(
            out.duplicating_at_level(num_levels as u8 - 1).len(),
            feats.len(),
            "canonical level must keep every feature"
        );

        if budget.enabled && budget.drop_rate > 1.0 && !feats.is_empty() && num_levels >= 2 {
            for level in 0..num_levels - 1 {
                let count = out.duplicating_at_level(level as u8).len();
                let ceiling = budget_ceiling(feats.len(), num_levels, level, budget.drop_rate);
                prop_assert!(
                    count <= ceiling,
                    "level {level} has {count} rows over its budget ceiling {ceiling}"
                );
            }
        } else {
            prop_assert_eq!(
                cw.assignments.clone(), out.assignments,
                "disabled/degenerate budget must be an identity"
            );
        }
    }

    /// "Stricter thinning never yields MORE rows", stated on the knob where
    /// it is actually an invariant: a larger `drop_rate` imposes a smaller
    /// per-level ceiling, so every level's duplicating count is monotone
    /// non-increasing in the drop rate.
    #[test]
    fn density_budget_drop_rate_monotone(
        feats in features(48),
        gsds in level_gsds(),
        config in assign_config(),
        rate in 1.05..4.0f64,
        factor in 1.01..2.0f64,
        gamma in 1.0..3.0f64,
        crs in crs(),
    ) {
        let cw = assign_levels(&feats, &gsds, &config, crs);
        let mk = |drop_rate: f64| DensityBudgetConfig { enabled: true, drop_rate, gamma };
        let loose = apply_density_budget(&cw, &feats, &gsds, &config, &mk(rate), crs);
        let strict = apply_density_budget(&cw, &feats, &gsds, &config, &mk(rate * factor), crs);
        for level in 0..gsds.len() as u8 {
            prop_assert!(
                strict.duplicating_at_level(level).len() <= loose.duplicating_at_level(level).len(),
                "a stricter drop_rate must never yield MORE rows at level {level}"
            );
        }
    }
}

// ============================================================================
// Clustering stage (spec §12.1 sum invariant)
// ============================================================================

fn accumulate_op() -> impl Strategy<Value = AccumulateOp> {
    prop_oneof![
        Just(AccumulateOp::Sum),
        Just(AccumulateOp::Max),
        Just(AccumulateOp::Min),
        Just(AccumulateOp::Mean),
    ]
}

proptest! {
    #![proptest_config(cases())]

    /// The strict §12.1 accounting invariant holds for cluster tables built
    /// from ANY genuine assignment (cell-winner alone or composed with the
    /// density budget): at every level each source point is counted in
    /// exactly one point row, so `Σ point_count == total source points`.
    /// Also checks table structure: the canonical level's table is empty,
    /// stored entries always summarize `>= 2` features, keys refer to point
    /// features present at that level, and aggregates stay parallel to the
    /// accumulate specs.
    #[test]
    fn cluster_sum_invariant_under_random_inputs(
        feats in features(48),
        gsds in level_gsds(),
        config in assign_config(),
        budget in budget_config(),
        crs in crs(),
        ops in prop::collection::vec(accumulate_op(), 0..3),
        seed_values in prop::collection::vec(prop::option::of(-1e6..1e6f64), 0..(3 * 48)),
    ) {
        let cw = assign_levels(&feats, &gsds, &config, crs);
        let out = apply_density_budget(&cw, &feats, &gsds, &config, &budget, crs);
        let min_levels: Vec<u8> = out.assignments.iter().map(|a| a.min_level).collect();

        // Per-spec value columns, parallel to features (cycled from the pool).
        let values: Vec<Vec<Option<f64>>> = (0..ops.len())
            .map(|s| {
                (0..feats.len())
                    .map(|i| seed_values.get((s * feats.len() + i) % seed_values.len().max(1)).copied().flatten())
                    .collect()
            })
            .collect();

        let tables = build_cluster_tables(&feats, &min_levels, &gsds, &config, crs, &values, &ops);
        prop_assert_eq!(tables.len(), gsds.len());

        // The §12.1 invariant must hold — a failure here is a producer bug.
        if let Err(violation) = verify_sum_invariant(&feats, &min_levels, &tables) {
            return Err(TestCaseError::fail(format!(
                "spec §12.1 sum invariant violated: {violation}"
            )));
        }

        // Structure: canonical table empty; entries are >= 2-member summaries
        // keyed by present point features; aggregates parallel to specs.
        if let Some(finest_table) = tables.last() {
            prop_assert!(finest_table.is_empty(), "canonical level table must be empty");
        }
        for (level, table) in tables.iter().enumerate() {
            for (&idx, entry) in table {
                let f = &feats[idx]; // index == position by construction
                prop_assert_eq!(f.kind, FeatureKind::Point, "only points cluster");
                prop_assert!(
                    (min_levels[idx] as usize) <= level,
                    "cluster winner must be present at its level"
                );
                prop_assert!(entry.point_count >= 2, "stored clusters summarize >= 2 features");
                prop_assert_eq!(entry.aggregates.len(), ops.len());
            }
        }
    }
}

// ============================================================================
// Coalescing stage
// ============================================================================

/// A small line-ish geometry on a coarse coordinate lattice (so endpoints
/// frequently coincide and chaining actually triggers), plus occasional
/// pass-through kinds (MultiLineString, Point) and degenerate lines.
fn coalesce_geometry() -> impl Strategy<Value = Geometry<f64>> {
    let lattice_pt = (-6i32..6, -6i32..6).prop_map(|(x, y)| (x as f64 * 1000.0, y as f64 * 1000.0));
    let line = prop::collection::vec(lattice_pt, 2..6)
        .prop_map(|pts| Geometry::LineString(LineString::from(pts)));
    let multi = (prop::collection::vec((-6i32..6, -6i32..6), 2..4)).prop_map(|pts| {
        let coords: Vec<(f64, f64)> = pts
            .into_iter()
            .map(|(x, y)| (x as f64 * 1000.0, y as f64 * 1000.0))
            .collect();
        Geometry::MultiLineString(MultiLineString::new(vec![LineString::from(coords)]))
    });
    let point = (-6i32..6, -6i32..6)
        .prop_map(|(x, y)| Geometry::Point(Point::new(x as f64 * 1000.0, y as f64 * 1000.0)));
    prop_oneof![
        6 => line,
        1 => multi,
        1 => point,
    ]
}

fn coalesce_params() -> impl Strategy<Value = CoalesceParams> {
    (
        -1.0..2.0f64,
        prop_oneof![Just(0.0), 1.0..90.0f64],
        prop::option::of((1usize..32, 1.0..3.0f64)),
    )
        .prop_map(
            |(snap_gsd_factor, junction_angle_deg, budget)| CoalesceParams {
                snap_gsd_factor,
                junction_angle_deg,
                budget,
            },
        )
}

proptest! {
    #![proptest_config(cases())]

    /// Coalescing invariants for any valid input:
    /// - no panic; every surviving chain has `count >= 1`;
    /// - reps are unique, drawn from the input indices, ascending
    ///   (documented output order);
    /// - `coalesced_count` sums are conserved: chains partition the input,
    ///   so `Σ count <= n` always (gating/thinning drop whole chains), and
    ///   `Σ count == n` exactly when nothing can be gated/thinned
    ///   (non-positive GSD, no chain budget);
    /// - the stage is deterministic.
    #[test]
    fn coalesce_count_conservation_and_determinism(
        geoms in prop::collection::vec((coalesce_geometry(), sort_key(), 0u32..3), 0..24),
        zoom in 4u32..14,
        degenerate_gsd in any::<bool>(),
        config in assign_config(),
        params in coalesce_params(),
        crs in crs(),
    ) {
        let gsd_m = if degenerate_gsd { 0.0 } else { gsd(zoom) };
        let params = if degenerate_gsd {
            // No gate/thinning/budget: chaining alone must conserve counts.
            CoalesceParams { budget: None, ..params }
        } else {
            params
        };
        let inputs: Vec<CoalesceInput<'_>> = geoms
            .iter()
            .enumerate()
            .map(|(i, (geom, sort_key, group))| CoalesceInput {
                index: i,
                geom,
                sort_key: *sort_key,
                group: *group,
            })
            .collect();

        let out = coalesce_level_lines(&inputs, gsd_m, crs, &config, &params);
        let out2 = coalesce_level_lines(&inputs, gsd_m, crs, &config, &params);
        prop_assert_eq!(&out, &out2, "coalescing must be deterministic");

        let n = inputs.len() as i64;
        let mut sum = 0i64;
        let mut prev_rep: Option<usize> = None;
        for chain in &out {
            prop_assert!(chain.count >= 1, "every chain summarizes >= 1 source row");
            prop_assert!(chain.rep < inputs.len(), "rep must be an input index");
            if let Some(prev) = prev_rep {
                prop_assert!(chain.rep > prev, "reps must be unique and ascending");
            }
            prev_rep = Some(chain.rep);
            sum += chain.count as i64;
        }
        prop_assert!(
            sum <= n,
            "chains partition the input: Σ coalesced_count ({sum}) cannot exceed rows ({n})"
        );
        if degenerate_gsd {
            prop_assert_eq!(
                sum, n,
                "with no gate/thinning/budget, Σ coalesced_count must equal the input rows"
            );
        }
    }
}
