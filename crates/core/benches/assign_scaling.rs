//! Scaling harness for the level-assignment phase (#534).
//!
//! Carried into #448's bench inventory verbatim from #564 (open at the time
//! of writing), which introduces it against the parallelized assign path —
//! it depends only on `overview::assign` APIs already on `main`, so it
//! compiles and runs unchanged here. No functional edits.
//!
//! Times the two stages `resolve_winner_tables` runs — the per-level
//! cell-winner passes ([`assign_levels_bounded`]) and the density budget
//! ([`apply_density_budget`]) — over a synthetic, spatially clustered feature
//! table, at a range of rayon pool sizes. Prints one row per pool size so the
//! scaling curve (and the serial fraction) is visible directly, and asserts
//! that every pool size produced the byte-identical assignment.
//!
//! ```text
//! cargo bench --package tylertoo-core --bench assign_scaling
//! ASSIGN_BENCH_ROWS=20000000 ASSIGN_BENCH_THREADS=1,4,8,12 \
//!     ASSIGN_BENCH_REPEATS=5 \
//!     cargo bench --package tylertoo-core --bench assign_scaling
//! ```
//!
//! Knobs: `ASSIGN_BENCH_ROWS`, `ASSIGN_BENCH_THREADS` (comma-separated pool
//! sizes), `ASSIGN_BENCH_REPEATS` (best-of, default 3) and
//! `ASSIGN_BENCH_GRID_MIB` (the #306 grid budget; `0` = unbounded).
//!
//! Not a criterion benchmark: the interesting quantity is a wall-clock curve
//! against thread count on one large input, not a distribution over many small
//! runs, and a 20M-row table is far too big to sample repeatedly.

use std::time::Instant;

use tylertoo_core::overview::assign::{
    apply_density_budget, assign_levels_bounded, AssignConfig, AssignFeature, Crs,
    DensityBudgetConfig, FeatureKind,
};

/// Web-Mercator GSD (meters per pixel at a 256-px tile) for a zoom level.
fn gsd(z: u32) -> f64 {
    40_075_016.685_578_5 / (256.0 * f64::from(1u32 << z))
}

/// A spatially clustered mix of points, lines and polygons over a
/// continental-scale Web-Mercator extent.
///
/// Clustered on purpose: a uniform scatter would give every feature its own
/// grid cell at every level, which is the *easy* case for a parallel winner
/// pass (no contention). Real inputs pile many features into the same coarse
/// cell, so the fixture does too — `CLUSTERS` hot spots with a tight jitter,
/// plus a diffuse background.
fn fixture(n: usize) -> Vec<AssignFeature> {
    const CLUSTERS: u64 = 4_096;
    // Continental extent in Web-Mercator meters.
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
            // Diffuse background: anywhere in the extent.
            let u = (next() >> 11) as f64 / (1u64 << 53) as f64;
            let v = (next() >> 11) as f64 / (1u64 << 53) as f64;
            (u * SPAN - SPAN / 2.0, v * SPAN - SPAN / 2.0)
        } else {
            // Clustered: pick a hot spot, jitter within ~2 km of it.
            let c = next() % CLUSTERS;
            let ang = (c as f64) * 2.399_963_23; // golden-angle scatter
            let rad = SPAN / 2.0 * ((c % 97) as f64 / 97.0).sqrt();
            let jx = ((next() >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 4_000.0;
            let jy = ((next() >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 4_000.0;
            (rad * ang.cos() + jx, rad * ang.sin() + jy)
        };
        // Sizes span five orders of magnitude so the visibility gate bites at
        // different levels for different features.
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

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let rows = env_usize("ASSIGN_BENCH_ROWS", 8_000_000);
    let threads: Vec<usize> = match std::env::var("ASSIGN_BENCH_THREADS") {
        Ok(v) => v.split(',').filter_map(|t| t.trim().parse().ok()).collect(),
        Err(_) => {
            let max = std::thread::available_parallelism().map_or(8, |n| n.get());
            let mut t = vec![1usize];
            let mut k = 2;
            while k < max {
                t.push(k);
                k *= 2;
            }
            t.push(max);
            t
        }
    };
    // The #306 winner-grid RAM budget. The production `auto`/`bounded`
    // profiles derive it from available RAM, and at dataset scale it BINDS —
    // the levels are packed into several waves, which is what leaves the
    // per-level pass with nothing to overlap. Defaulting it to a bound (not
    // `u64::MAX`) keeps the harness in the regime #534 is about; set
    // `ASSIGN_BENCH_GRID_MIB=0` for the unbounded single-wave shape.
    let grid_mib = env_usize("ASSIGN_BENCH_GRID_MIB", 1024);
    let grid_budget = if grid_mib == 0 {
        u64::MAX
    } else {
        grid_mib as u64 * 1024 * 1024
    };
    // Coarse→fine, the shape a `tiles` run plans: 14 levels ending at z14.
    let gsds: Vec<f64> = (1u32..=14).map(gsd).collect();
    let config = AssignConfig::default();
    let density = DensityBudgetConfig::default();

    let t0 = Instant::now();
    let feats = fixture(rows);
    println!(
        "fixture: {} features in {:.1}s ({} levels, grid budget {})\n",
        feats.len(),
        t0.elapsed().as_secs_f64(),
        gsds.len(),
        if grid_mib == 0 {
            "unbounded".to_string()
        } else {
            format!("{grid_mib} MiB")
        }
    );
    println!(
        "{:>8}  {:>10}  {:>10}  {:>10}  {:>8}",
        "threads", "assign_s", "budget_s", "total_s", "speedup"
    );

    // Repeats are reported by their MINIMUM, not their mean. Contention from
    // anything else on the box can only ever ADD wall time to a run, so under
    // a shared machine the minimum is the estimator that converges on the
    // uncontended figure while the mean wanders with the neighbours. Repeats
    // are interleaved across thread counts (outer loop) so a slow patch hits
    // every point on the curve rather than whichever one it landed on.
    let repeats = env_usize("ASSIGN_BENCH_REPEATS", 3).max(1);
    let mut best: Vec<(f64, f64)> = vec![(f64::INFINITY, f64::INFINITY); threads.len()];
    let mut expected: Option<Vec<u8>> = None;
    for _ in 0..repeats {
        for (slot, &t) in best.iter_mut().zip(&threads) {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(t)
                .build()
                .expect("rayon pool");
            let (assign_s, budget_s, levels) = pool.install(|| {
                let ta = Instant::now();
                let cw =
                    assign_levels_bounded(&feats, &gsds, &config, Crs::Epsg3857, grid_budget, &[]);
                let assign_s = ta.elapsed().as_secs_f64();
                let tb = Instant::now();
                let out =
                    apply_density_budget(&cw, &feats, &gsds, &config, &density, Crs::Epsg3857);
                let budget_s = tb.elapsed().as_secs_f64();
                let levels: Vec<u8> = out.assignments.iter().map(|a| a.min_level).collect();
                (assign_s, budget_s, levels)
            });
            if assign_s + budget_s < slot.0 + slot.1 {
                *slot = (assign_s, budget_s);
            }
            match &expected {
                None => expected = Some(levels),
                Some(want) => assert!(
                    *want == levels,
                    "assignment differs at {t} thread(s) — the parallel build must be \
                     identical to the serial one"
                ),
            }
        }
    }

    let baseline = best[0].0 + best[0].1;
    for (&(assign_s, budget_s), &t) in best.iter().zip(&threads) {
        let total = assign_s + budget_s;
        let speedup = baseline / total;
        println!("{t:>8}  {assign_s:>10.2}  {budget_s:>10.2}  {total:>10.2}  {speedup:>7.2}x");
    }
    println!(
        "\nbest of {repeats} repeat(s); assignment identical across every pool size and repeat."
    );
}
