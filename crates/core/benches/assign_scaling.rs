//! Scaling harness for the level-assignment phase (#534).
//!
//! Times the two stages `resolve_winner_tables` runs — the per-level
//! cell-winner passes ([`assign_levels_bounded`]) and the density budget
//! ([`apply_density_budget`]) — over a synthetic, spatially clustered feature
//! table, at a range of rayon pool sizes. Prints one row per pool size so the
//! scaling curve (and the serial fraction) is visible directly, and asserts
//! that every pool size produced the byte-identical assignment.
//!
//! It also reports each phase's **transient bytes per feature** (#565): the
//! high-water mark of live heap the phase adds on top of the feature table it
//! is given. That is the quantity the #543 preflight's
//! `PASS1_RECOMMENDED_FACTOR` has to cover, and the one a memory change to
//! either phase has to move. It is measured by a counting global allocator
//! rather than by sampling RSS: the allocator sees every byte exactly, needs
//! no platform probe, and — unlike `ru_maxrss`, a monotone per-process
//! high-water — can be re-armed between phases, which is the only way to
//! separate the density budget's footprint from the winner grids' (#306) RAM
//! budget sitting right before it. Process peak RSS is printed alongside as an
//! independent sanity check.
//!
//! Allocation tracking is **off by default** and costs an order of magnitude in
//! wall time when on, so a timing run and a memory run are two invocations:
//!
//! ```text
//! cargo bench --package tylertoo-core --bench assign_scaling
//! ASSIGN_BENCH_ROWS=20000000 ASSIGN_BENCH_THREADS=1,4,8,12 \
//!     ASSIGN_BENCH_REPEATS=5 \
//!     cargo bench --package tylertoo-core --bench assign_scaling
//! ASSIGN_BENCH_TRACK_MEM=1 ASSIGN_BENCH_THREADS=8 ASSIGN_BENCH_REPEATS=1 \
//!     cargo bench --package tylertoo-core --bench assign_scaling
//! ```
//!
//! Knobs: `ASSIGN_BENCH_ROWS`, `ASSIGN_BENCH_THREADS` (comma-separated pool
//! sizes), `ASSIGN_BENCH_REPEATS` (best-of, default 3),
//! `ASSIGN_BENCH_GRID_MIB` (the #306 grid budget; `0` = unbounded) and
//! `ASSIGN_BENCH_TRACK_MEM` (the per-phase `B/ft` columns).
//!
//! Not a criterion benchmark: the interesting quantity is a wall-clock curve
//! against thread count on one large input, not a distribution over many small
//! runs, and a 20M-row table is far too big to sample repeatedly.

use std::time::Instant;

/// Live-heap counter + re-armable high-water mark, behind a counting global
/// allocator.
///
/// **Tracking is off unless `ASSIGN_BENCH_TRACK_MEM` is set**, and the B/ft
/// columns and the times must therefore come from two runs. It is not free: the
/// per-level pending-placement blocks and per-run sort scratch of the parallel
/// build (#534) allocate often enough that two atomic read-modify-writes per
/// allocation — on ONE cache line, shared by every worker — dominate the phase.
/// So the gate is a plain `Relaxed` load of a never-written flag (the line stays
/// Shared in every core's cache, no coherence traffic), the default is off, and
/// a timing run and a memory run are separate invocations. The header line says
/// which mode produced the output.
///
/// Compiled out under `--features dhat-heap`, where `tylertoo-core` installs
/// dhat's `#[global_allocator]` and a second one in this crate is a hard
/// compile error. The B/ft columns then read `-`; use dhat for that build.
#[cfg(not(feature = "dhat-heap"))]
mod track {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
    use std::sync::OnceLock;

    struct Counting;

    // Signed, because tracking is armed inside `main`: an allocation made
    // before that (std's own startup) and freed after it decrements a counter
    // that never saw its `alloc`. On an unsigned counter that wraps to ~2^64
    // and every later `fetch_max` latches the garbage; signed, it is a small
    // negative that the next allocations absorb and that `added_since`
    // saturates away.
    static LIVE: AtomicIsize = AtomicIsize::new(0);
    static PEAK: AtomicIsize = AtomicIsize::new(0);
    static TRACKING: AtomicBool = AtomicBool::new(false);

    /// Whether `ASSIGN_BENCH_TRACK_MEM` asked for allocation tracking. Read
    /// once and cached — `std::env::var` allocates, and doing that inside the
    /// allocator would recurse.
    pub fn requested() -> bool {
        static REQUESTED: OnceLock<bool> = OnceLock::new();
        *REQUESTED.get_or_init(|| {
            std::env::var("ASSIGN_BENCH_TRACK_MEM")
                .map(|v| !matches!(v.trim(), "" | "0" | "false" | "no" | "off"))
                .unwrap_or(false)
        })
    }

    /// Turn tracking on or off. Called once, before the fixture is built, so
    /// `LIVE` is accurate for everything the run allocates.
    pub fn enable(on: bool) {
        TRACKING.store(on, Ordering::Relaxed);
    }

    /// Re-arm the high-water mark at the current live total and return it.
    pub fn arm() -> isize {
        let live = LIVE.load(Ordering::Relaxed);
        PEAK.store(live, Ordering::Relaxed);
        live
    }

    /// Bytes the phase added at its high-water, over the `base` [`arm`]
    /// returned. `Relaxed` throughout: this is read only after the measured
    /// phase has joined (a rayon `install` is a full barrier), so nothing
    /// beyond atomicity is needed.
    pub fn added_since(base: isize) -> usize {
        PEAK.load(Ordering::Relaxed)
            .saturating_sub(base)
            .max(0)
            .unsigned_abs()
    }

    #[inline]
    fn record_alloc(size: usize) {
        let size = size as isize;
        let live = LIVE.fetch_add(size, Ordering::Relaxed) + size;
        // `fetch_max` rather than load/compare/store: a racing pair of the
        // latter can drop the higher sample, which would silently understate
        // the peak.
        PEAK.fetch_max(live, Ordering::Relaxed);
    }

    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(layout) };
            if !p.is_null() && TRACKING.load(Ordering::Relaxed) {
                record_alloc(layout.size());
            }
            p
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if TRACKING.load(Ordering::Relaxed) {
                LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
            }
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let p = unsafe { System.realloc(ptr, layout, new_size) };
            if !p.is_null() && TRACKING.load(Ordering::Relaxed) {
                LIVE.fetch_sub(layout.size() as isize, Ordering::Relaxed);
                record_alloc(new_size);
            }
            p
        }
    }

    #[global_allocator]
    static ALLOC: Counting = Counting;
}

/// Stand-in for [`track`] under `--features dhat-heap`, where dhat owns the
/// process's `#[global_allocator]`: tracking reports as unavailable and the
/// B/ft columns print `-`.
#[cfg(feature = "dhat-heap")]
mod track {
    pub fn requested() -> bool {
        false
    }
    pub fn enable(_on: bool) {}
    pub fn arm() -> isize {
        0
    }
    pub fn added_since(_base: isize) -> usize {
        0
    }
}

use track::{added_since, arm};

/// Process peak resident set size in bytes, or `None` where it is unavailable.
///
/// `ru_maxrss` is in bytes on Darwin and in kibibytes on Linux.
#[cfg(unix)]
fn peak_rss_bytes() -> Option<u64> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return None;
    }
    let raw = usage.ru_maxrss as u64;
    Some(if cfg!(target_os = "macos") {
        raw
    } else {
        raw * 1024
    })
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> Option<u64> {
    None
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

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
    // Armed before the fixture so `LIVE` is accurate for everything the run
    // allocates (see `Counting`).
    let track_mem = track::requested();
    track::enable(track_mem);

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
    let table_bytes = std::mem::size_of::<AssignFeature>() * feats.len();
    println!(
        "feature table: {:.0} MiB ({} B/feature){}\n",
        mib(table_bytes as u64),
        std::mem::size_of::<AssignFeature>(),
        if track_mem {
            " — ASSIGN_BENCH_TRACK_MEM: B/ft columns are exact, TIMES ARE NOT (see `Counting`)"
        } else {
            " — timing mode; set ASSIGN_BENCH_TRACK_MEM=1 for the B/ft columns"
        },
    );
    println!(
        "{:>8}  {:>10}  {:>10}  {:>10}  {:>8}  {:>12}  {:>12}",
        "threads", "assign_s", "budget_s", "total_s", "speedup", "assign_B/ft", "budget_B/ft"
    );

    // Repeats are reported by their MINIMUM, not their mean. Contention from
    // anything else on the box can only ever ADD wall time to a run, so under
    // a shared machine the minimum is the estimator that converges on the
    // uncontended figure while the mean wanders with the neighbours. Repeats
    // are interleaved across thread counts (outer loop) so a slow patch hits
    // every point on the curve rather than whichever one it landed on.
    let repeats = env_usize("ASSIGN_BENCH_REPEATS", 3).max(1);
    let mut best: Vec<(f64, f64)> = vec![(f64::INFINITY, f64::INFINITY); threads.len()];
    // Per-phase transient bytes, kept as the MAXIMUM over repeats (unlike the
    // times, where the minimum is the honest estimator: an allocation
    // high-water is exact, not noisy, so the only spread across repeats is a
    // genuinely larger peak).
    let mut mem: Vec<(usize, usize)> = vec![(0, 0); threads.len()];
    let mut expected: Option<Vec<u8>> = None;
    for _ in 0..repeats {
        for ((slot, mslot), &t) in best.iter_mut().zip(mem.iter_mut()).zip(&threads) {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(t)
                .build()
                .expect("rayon pool");
            let (assign_s, budget_s, assign_mem, budget_mem, levels) = pool.install(|| {
                // The cell-winner phase's own transient: measured with the
                // winner tables NOT yet built, so it includes the #306 grid
                // budget. `cw` stays live across the budget phase (the budget
                // reads it), so it is deliberately counted into the assign
                // figure and excluded from the budget one, which is where the
                // real call chain puts it too.
                let assign_base = arm();
                let ta = Instant::now();
                let cw =
                    assign_levels_bounded(&feats, &gsds, &config, Crs::Epsg3857, grid_budget, &[]);
                let assign_s = ta.elapsed().as_secs_f64();
                let assign_mem = added_since(assign_base);

                let budget_base = arm();
                let tb = Instant::now();
                let out =
                    apply_density_budget(&cw, &feats, &gsds, &config, &density, Crs::Epsg3857);
                let budget_s = tb.elapsed().as_secs_f64();
                let budget_mem = added_since(budget_base);

                let levels: Vec<u8> = out.assignments.iter().map(|a| a.min_level).collect();
                (assign_s, budget_s, assign_mem, budget_mem, levels)
            });
            if assign_s + budget_s < slot.0 + slot.1 {
                *slot = (assign_s, budget_s);
            }
            mslot.0 = mslot.0.max(assign_mem);
            mslot.1 = mslot.1.max(budget_mem);
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
    let per_feature = |bytes: usize| bytes as f64 / rows as f64;
    for ((&(assign_s, budget_s), &(assign_mem, budget_mem)), &t) in
        best.iter().zip(&mem).zip(&threads)
    {
        let total = assign_s + budget_s;
        let speedup = baseline / total;
        let (a_mem, b_mem) = if track_mem {
            (
                format!("{:.1}", per_feature(assign_mem)),
                format!("{:.1}", per_feature(budget_mem)),
            )
        } else {
            ("-".to_string(), "-".to_string())
        };
        println!(
            "{t:>8}  {assign_s:>10.2}  {budget_s:>10.2}  {total:>10.2}  {speedup:>7.2}x  \
             {a_mem:>12}  {b_mem:>12}"
        );
    }
    println!(
        "\nbest of {repeats} repeat(s); assignment identical across every pool size and repeat."
    );
    if let Some(peak) = peak_rss_bytes() {
        println!(
            "process peak RSS: {:.0} MiB ({:.1} B/feature, feature table {:.0} MiB of it)",
            mib(peak),
            peak as f64 / rows as f64,
            mib(table_bytes as u64),
        );
    }
}
