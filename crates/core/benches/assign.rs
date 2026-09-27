//! Level-assignment benchmark (#448).
//!
//! Criterion coverage of the cell-winner + density-budget phase
//! ([`assign_levels_bounded`] / [`apply_density_budget`]), the hot path
//! `assign_scaling.rs` (#534/#564) profiles at dataset scale with a manual
//! wall-clock harness. This bench is the small, CI-budget-friendly sibling:
//! one fixed-size synthetic feature set, run under criterion's own
//! statistics so it slots into the critcmp/`--baseline` regression workflow
//! (see DEVELOPMENT.md). It is not a substitute for `assign_scaling`'s
//! thread-count curve — that harness stays a manual `cargo bench --bench
//! assign_scaling` run, not a criterion target.
//!
//! Run with: cargo bench --package tylertoo-core --bench assign

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tylertoo_core::overview::assign::{
    apply_density_budget, assign_levels_bounded, AssignConfig, AssignFeature, Crs,
    DensityBudgetConfig, FeatureKind,
};

/// Web-Mercator GSD (meters per pixel at a 256-px tile) for a zoom level.
fn gsd(z: u32) -> f64 {
    40_075_016.685_578_5 / (256.0 * f64::from(1u32 << z))
}

/// A spatially clustered mix of points, lines and polygons, matching the
/// shape `assign_scaling.rs`'s fixture uses (real inputs pile many features
/// into the same coarse cell) but at a size criterion can sample repeatedly
/// within a modest CI budget.
fn fixture(n: usize) -> Vec<AssignFeature> {
    const CLUSTERS: u64 = 256;
    const SPAN: f64 = 4_000_000.0;

    let mut feats = Vec::with_capacity(n);
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for i in 0..n {
        let r = next();
        let (cx, cy) = if r % 4 == 0 {
            let u = (next() >> 11) as f64 / (1u64 << 53) as f64;
            let v = (next() >> 11) as f64 / (1u64 << 53) as f64;
            (u * SPAN - SPAN / 2.0, v * SPAN - SPAN / 2.0)
        } else {
            let c = next() % CLUSTERS;
            let ang = (c as f64) * 2.399_963_23;
            let rad = SPAN / 2.0 * ((c % 97) as f64 / 97.0).sqrt();
            let jx = ((next() >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 4_000.0;
            let jy = ((next() >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 4_000.0;
            (rad * ang.cos() + jx, rad * ang.sin() + jy)
        };
        let size = 10.0_f64.powf(1.0 + (next() % 5000) as f64 / 1000.0);
        let kind = match next() % 10 {
            0..=4 => FeatureKind::Point,
            5..=7 => FeatureKind::Line,
            _ => FeatureKind::Polygon,
        };
        let sort_key = if next() % 8 == 0 {
            None
        } else {
            Some((next() % 1_000_000) as f64)
        };
        feats.push(AssignFeature {
            index: i,
            bbox: [cx, cy, cx + size, cy + size],
            kind,
            sort_key,
            entry_level: None,
        });
    }
    feats
}

fn bench_assign(c: &mut Criterion) {
    let mut group = c.benchmark_group("assign");
    group.measurement_time(Duration::from_secs(6));
    group.sample_size(20);

    // Coarse->fine, 10 levels: enough to exercise the #306 wave scheduling
    // without the multi-minute fixture generation a dataset-scale run needs.
    let gsds: Vec<f64> = (1u32..=10).map(gsd).collect();
    let config = AssignConfig::default();
    let density = DensityBudgetConfig::default();
    let grid_budget = 256 * 1024 * 1024; // 256 MiB, binds waves at this scale

    for n in [50_000usize, 200_000] {
        let feats = fixture(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &feats, |b, feats| {
            b.iter(|| {
                let cw = assign_levels_bounded(
                    black_box(feats),
                    black_box(&gsds),
                    black_box(&config),
                    Crs::Epsg3857,
                    grid_budget,
                    &[],
                );
                let out = apply_density_budget(&cw, feats, &gsds, &config, &density, Crs::Epsg3857);
                black_box(out.assignments.len())
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_assign);
criterion_main!(benches);
