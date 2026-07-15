//! Level-assignment engine (task P1): grid cell-winner thinning.
//!
//! This is a **pure** algorithm with no parquet/arrow/geo dependencies and no
//! dependency on other `overview` submodules. It answers a single question:
//! *at which levels does each feature survive?*
//!
//! # Model
//!
//! Given a coarse→fine list of level GSDs (ground sample distances, in meters,
//! per spec §5.2) and a bounding box + geometry kind per feature, the engine
//! assigns every feature a `min_level`: the **coarsest** level at which it
//! appears. Consumers then expand that per mode (spec §2.2 / §2.3):
//!
//! - **`duplicating` mode**: a feature appears at *every* level `>= min_level`
//!   (see [`Assignment::duplicating_at_level`]).
//! - **`partitioning` mode**: a feature appears at *exactly* `min_level`
//!   (see [`Assignment::partitioning_at_level`]).
//!
//! # Algorithm (spec §5, cogp-rs reference)
//!
//! For each level, coarse→fine:
//! 1. Derive a grid cell size from the level GSD scaled by a per-kind
//!    **thinning factor** (points thin most, polygons least).
//! 2. Take each feature's representative point (bbox center — see divergence
//!    note below) and drop it into its grid cell.
//! 3. Keep exactly **one winner per occupied cell**, chosen by a strict,
//!    deterministic lexicographic priority:
//!    `(sort_key, bbox-diagonal², stable hash of index, index)`.
//! 4. A **visibility gate** makes a line/polygon *ineligible* at a level unless
//!    its bbox diagonal is at least `visibility_factor × gsd(level)`. Points are
//!    always eligible.
//!
//! A feature's `min_level` is the first (coarsest) level at which it wins a
//! cell. A feature that never wins a cell (e.g. a small line gated out until
//! the finest level, or one always out-competed) is assigned the **finest**
//! level, guaranteeing every feature is present at the canonical level (spec
//! §2.4).
//!
//! # Memory
//!
//! Per level, the only state is a hashmap `cell -> best-priority-so-far`,
//! `O(occupied cells)` — never `O(features)` retained across levels. The coarse
//! levels are built concurrently (#264, see below), so the transient peak is
//! the sum of the *concurrently live* grids rather than a single one; this
//! phase runs before the pass-2 geometry buffers that set overall peak RSS, so
//! it does not move the high-water mark in practice.
//!
//! # Parallelism & determinism
//!
//! Each level's cell-winner pass is independent, so the coarse levels run
//! concurrently across rayon threads (#264 — this is the hot serial stage on
//! large-feature / simple-geometry layers). Results are unaffected: the
//! per-cell winner is the maximum under a *strict total order* (ties are
//! ultimately broken by the feature `index`), so they never depend on hashmap
//! iteration order or thread scheduling, and a feature takes the *coarsest*
//! level at which it wins regardless of the order levels finish. Output is
//! identical across runs and to a fully serial build.
//!
//! # DIVERGENCE FROM TIPPECANOE / cogp-rs
//!
//! We use the **bbox center** as a feature's representative point (v1). A
//! centroid or a first-vertex would be closer to cartographic convention for
//! irregular polygons/lines; bbox center is cheap, needs no geometry decode,
//! and is adequate for thinning at overview scales. Revisit if quality gating
//! (plan V2) shows artifacts.

use std::collections::HashMap;

use rayon::prelude::*;

pub use super::level::{Crs, METERS_PER_DEGREE};

/// Geometry kind, for per-kind thinning and visibility gating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureKind {
    Point,
    Line,
    Polygon,
}

impl FeatureKind {
    /// Stable discriminant so per-kind grids never collide on cell integers.
    #[inline]
    fn discriminant(self) -> u8 {
        match self {
            FeatureKind::Point => 0,
            FeatureKind::Line => 1,
            FeatureKind::Polygon => 2,
        }
    }
}

/// Direction in which a larger vs smaller `sort_key` is preferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    /// Larger `sort_key` wins (e.g. population, importance). Default.
    Desc,
    /// Smaller `sort_key` wins (e.g. rank where 1 is best).
    Asc,
}

/// A single input feature for level assignment.
///
/// Deliberately geometry-free: the caller supplies only the bbox, the kind, and
/// an optional sort key. `index` is an opaque caller-owned identifier echoed
/// back in the output (typically the row's position in the source file).
#[derive(Debug, Clone, Copy)]
pub struct AssignFeature {
    /// Opaque, caller-owned feature identifier (echoed in output).
    pub index: usize,
    /// `[xmin, ymin, xmax, ymax]` in the file CRS coordinate units.
    pub bbox: [f64; 4],
    /// Geometry kind for thinning / visibility.
    pub kind: FeatureKind,
    /// Optional priority key; features *with* a key always out-rank features
    /// without one (nulls lose).
    pub sort_key: Option<f64>,
}

impl AssignFeature {
    /// Bbox center — the representative point used for grid placement.
    #[inline]
    pub(super) fn center(&self) -> (f64, f64) {
        let [xmin, ymin, xmax, ymax] = self.bbox;
        ((xmin + xmax) * 0.5, (ymin + ymax) * 0.5)
    }

    /// Squared bbox diagonal length, in (coordinate-unit)². Used both as a
    /// priority component and, against a squared gate, for visibility.
    #[inline]
    fn diag_sq(&self) -> f64 {
        let [xmin, ymin, xmax, ymax] = self.bbox;
        let dx = xmax - xmin;
        let dy = ymax - ymin;
        dx * dx + dy * dy
    }
}

/// Thinning / visibility / sort configuration.
///
/// Defaults: point thinning 4, line 1, polygon 1; line visibility 2,
/// polygon 4; sort descending. Point/polygon match the cogp-rs
/// reference; line thinning was retuned 2.0 → 1.0 after the 2026-07-02
/// Portland-roads parameter sweep (corpus/SWEEPS.md): lt=1 keeps
/// road networks visibly more continuous at coarse zooms, chosen by
/// maintainer review of the true-scale sweep renders.
#[derive(Debug, Clone, Copy)]
pub struct AssignConfig {
    /// Grid-cell multiplier for points (coarser grid ⇒ more aggressive thin).
    pub point_thinning: f64,
    /// Grid-cell multiplier for lines.
    pub line_thinning: f64,
    /// Grid-cell multiplier for polygons.
    pub polygon_thinning: f64,
    /// A line is eligible at level `z` only if `diag >= line_visibility·gsd(z)`.
    pub line_visibility: f64,
    /// A polygon is eligible at level `z` only if
    /// `diag >= polygon_visibility·gsd(z)`.
    pub polygon_visibility: f64,
    /// Whether larger or smaller `sort_key` wins.
    pub sort_direction: SortDirection,
}

/// Default `point_thinning` when point clustering is enabled.
///
/// With clustering, absorbed points are *summarized* (`point_count` +
/// accumulated attributes) rather than dropped, so a much coarser grid is
/// pure win: one dot per ~16 display pixels approaches the supercluster
/// look (radius 40 px) while the non-clustered default (4.0) preserves
/// density for thin-only output. Chosen by maintainer review of the
/// 2026-07-03 NYC pt={4,16,48} sweep (corpus/data/bench/q4/).
///
/// Callers that expose a user-facing `point_thinning` knob should apply
/// this default only when the user did not set the knob explicitly.
pub const CLUSTER_POINT_THINNING_DEFAULT: f64 = 16.0;

impl Default for AssignConfig {
    fn default() -> Self {
        Self {
            point_thinning: 4.0,
            line_thinning: 1.0,
            polygon_thinning: 1.0,
            line_visibility: 2.0,
            polygon_visibility: 4.0,
            sort_direction: SortDirection::Desc,
        }
    }
}

impl AssignConfig {
    #[inline]
    fn thinning_factor(&self, kind: FeatureKind) -> f64 {
        match kind {
            FeatureKind::Point => self.point_thinning,
            FeatureKind::Line => self.line_thinning,
            FeatureKind::Polygon => self.polygon_thinning,
        }
    }

    /// Visibility factor per kind. Points return `0.0` (always eligible).
    #[inline]
    fn visibility_factor(&self, kind: FeatureKind) -> f64 {
        match kind {
            FeatureKind::Point => 0.0,
            FeatureKind::Line => self.line_visibility,
            FeatureKind::Polygon => self.polygon_visibility,
        }
    }
}

/// One feature's assignment result, parallel to the input slice order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeatureAssignment {
    /// Echo of [`AssignFeature::index`].
    pub index: usize,
    /// Coarsest level at which the feature appears (`0` = coarsest level).
    pub min_level: u8,
}

/// Output of [`assign_levels`].
///
/// `assignments` is parallel to the input `features` slice. Helper methods
/// expand it into the per-level feature sets required by each spec mode.
#[derive(Debug, Clone)]
pub struct Assignment {
    /// Per-feature results, in input order.
    pub assignments: Vec<FeatureAssignment>,
    /// Total number of levels (`= level_gsds.len()`, capped at 255).
    pub num_levels: u8,
}

impl Assignment {
    /// Feature indices present at `level` in **`duplicating` mode**: every
    /// feature whose `min_level <= level` (spec §2.2). Returned in input order.
    pub fn duplicating_at_level(&self, level: u8) -> Vec<usize> {
        self.assignments
            .iter()
            .filter(|a| a.min_level <= level)
            .map(|a| a.index)
            .collect()
    }

    /// Feature indices placed at `level` in **`partitioning` mode**: every
    /// feature whose `min_level == level` (spec §2.3). Returned in input order.
    pub fn partitioning_at_level(&self, level: u8) -> Vec<usize> {
        self.assignments
            .iter()
            .filter(|a| a.min_level == level)
            .map(|a| a.index)
            .collect()
    }
}

/// Convert a meter-denominated GSD into input-coordinate units for the CRS.
#[inline]
pub fn gsd_to_coord_units(gsd_meters: f64, crs: Crs) -> f64 {
    crs.meters_to_units(gsd_meters)
}

/// Murmur3 finalizer — a deterministic mix used as a priority tiebreaker.
/// (Matches the pattern in `feature_drop::point_deterministic_hash`.)
#[inline]
fn stable_hash(index: usize) -> u64 {
    let mut x = index as u64;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^= x >> 33;
    x
}

/// Integer grid cell key. Includes the kind discriminant so per-kind grids
/// (which have different cell sizes) never share a bucket.
type CellKey = (u8, i64, i64);

/// Priority of a feature within a cell. Higher is better. Compared
/// lexicographically to yield a strict total order (see [`Priority::cmp`]).
/// `pub(super)` so the clustering stage (`super::cluster`) can rank present
/// features with the exact order the cell-winner stage used.
#[derive(Debug, Clone, Copy)]
pub(super) struct Priority {
    /// Encoded sort key: `Some` sorts above `None`; direction already applied
    /// so that "larger `sort_bits` wins".
    sort_rank: Option<f64>,
    diag_sq: f64,
    hash: u64,
    /// Smaller index wins ties; stored negated-in-comparison.
    index: usize,
}

impl Priority {
    pub(super) fn new(feat: &AssignFeature, dir: SortDirection) -> Self {
        // Apply direction so a plain "larger wins" comparison is correct.
        let sort_rank = feat.sort_key.map(|k| match dir {
            SortDirection::Desc => k,
            SortDirection::Asc => -k,
        });
        Priority {
            sort_rank,
            diag_sq: feat.diag_sq(),
            hash: stable_hash(feat.index),
            index: feat.index,
        }
    }

    /// Returns `true` if `self` is strictly better (should win) than `other`.
    pub(super) fn beats(&self, other: &Priority) -> bool {
        // 1. sort_rank: Some beats None; both Some compares larger-wins.
        match (self.sort_rank, other.sort_rank) {
            (Some(a), Some(b)) => {
                if a != b {
                    return a > b;
                }
            }
            (Some(_), None) => return true,
            (None, Some(_)) => return false,
            (None, None) => {}
        }
        // 2. larger bbox diagonal wins.
        if self.diag_sq != other.diag_sq {
            return self.diag_sq > other.diag_sq;
        }
        // 3. larger stable hash wins.
        if self.hash != other.hash {
            return self.hash > other.hash;
        }
        // 4. smaller index wins (guarantees a strict total order).
        self.index < other.index
    }
}

/// Build one overview level's cell-winner grid and return the winning feature
/// positions (one per occupied cell).
///
/// Serial single-map build — no cross-thread merge. Parallelism in
/// [`assign_levels`] is *across* levels (each level's grid is independent), so
/// this per-level work stays a single tight HashMap pass with no merge tax.
fn level_winner_positions(
    features: &[AssignFeature],
    config: &AssignConfig,
    gsd_units: f64,
) -> Vec<usize> {
    // cell -> (best priority, position of winning feature in `features`).
    let mut grid: HashMap<CellKey, (Priority, usize)> = HashMap::new();

    for (pos, feat) in features.iter().enumerate() {
        // Visibility gate (points always pass).
        let vis = config.visibility_factor(feat.kind);
        if vis > 0.0 {
            let gate = vis * gsd_units;
            if feat.diag_sq() < gate * gate {
                continue; // ineligible at this level
            }
        }

        let cell_size = gsd_units * config.thinning_factor(feat.kind);
        // Guard against a zero/negative cell size (bad GSD input); also
        // reject NaN.
        if cell_size <= 0.0 || cell_size.is_nan() {
            continue;
        }
        let (cx, cy) = feat.center();
        let key: CellKey = (
            feat.kind.discriminant(),
            (cx / cell_size).floor() as i64,
            (cy / cell_size).floor() as i64,
        );

        let prio = Priority::new(feat, config.sort_direction);
        grid.entry(key)
            .and_modify(|slot| {
                if prio.beats(&slot.0) {
                    *slot = (prio, pos);
                }
            })
            .or_insert((prio, pos));
    }

    grid.into_values().map(|(_prio, pos)| pos).collect()
}

/// Assign every feature its coarsest surviving level.
///
/// - `features`: input features (any order; output is parallel to this slice).
/// - `level_gsds`: level GSDs in **meters**, ordered **coarse→fine** (strictly
///   decreasing is expected but not required by this function).
/// - `config`: thinning / visibility / sort configuration.
/// - `crs`: CRS of the input coordinates (governs meter→unit conversion).
///
/// Returns per-feature `min_level`. If `level_gsds` is empty, `num_levels` is 0
/// and every feature is assigned `min_level = 0` (degenerate; callers should
/// pass at least one level).
pub fn assign_levels(
    features: &[AssignFeature],
    level_gsds: &[f64],
    config: &AssignConfig,
    crs: Crs,
) -> Assignment {
    let num_levels_usize = level_gsds.len();
    let num_levels = num_levels_usize.min(u8::MAX as usize) as u8;

    // Degenerate: no levels defined.
    if num_levels_usize == 0 {
        return Assignment {
            assignments: features
                .iter()
                .map(|f| FeatureAssignment {
                    index: f.index,
                    min_level: 0,
                })
                .collect(),
            num_levels: 0,
        };
    }

    let finest = num_levels - 1;

    // Start every feature at the finest level: a feature that never wins a
    // coarser cell falls through to the canonical (finest) level (spec §2.4).
    let mut min_levels: Vec<u8> = vec![finest; features.len()];

    // Per level, coarse→fine, run one cell-winner pass. State is O(cells).
    //
    // The finest level is skipped: `min_levels` already starts at `finest`, so
    // its cell-winner pass can only re-assign winners to a level they already
    // hold — pure dead work, and (crucially) its grid is the single largest
    // one (at fine GSD nearly every feature wins its own cell), so skipping it
    // also removes the biggest allocation. Only coarse levels can *lower* a
    // feature's `min_level`.
    //
    // Parallelism (#264): each coarse level's grid is fully independent, so the
    // per-level cell-winner passes run concurrently across rayon threads (no
    // merge — each level owns one HashMap). This is the hot serial stage on
    // large-feature / simple-geometry layers (issue #264). The concurrent grids
    // live only during this phase, which precedes the pass-2 geometry buffers
    // that set peak RSS, so overall peak memory is unaffected. The fold below
    // is deterministic regardless of completion order: a feature takes the
    // *coarsest* (smallest) level at which it wins.
    let winners_per_level: Vec<Vec<usize>> = (0..finest as usize)
        .into_par_iter()
        .map(|level_idx| {
            let gsd_units = gsd_to_coord_units(level_gsds[level_idx], crs);
            level_winner_positions(features, config, gsd_units)
        })
        .collect();

    for (level_idx, winners) in winners_per_level.iter().enumerate() {
        let level = level_idx as u8;
        for &pos in winners {
            if level < min_levels[pos] {
                min_levels[pos] = level;
            }
        }
    }

    let assignments = features
        .iter()
        .zip(min_levels)
        .map(|(f, min_level)| FeatureAssignment {
            index: f.index,
            min_level,
        })
        .collect();

    Assignment {
        assignments,
        num_levels,
    }
}

// ============================================================================
// Density-based budgets (Q2)
// ============================================================================
//
// Cell-winner thinning (above) stops binding once the grid cell is smaller than
// the typical feature spacing: at mid zooms *every* feature wins its own cell,
// so counts plateau at ~the whole dataset (Portland roads: ours/tippecanoe ≈
// 2–3x at z9–z11; see `corpus/SWEEPS.md`). Tippecanoe instead applies a
// rank-ordered drop-rate per zoom. This stage layers that on top of the
// cell-winner assignment: it imposes a per-level feature **budget** that decays
// geometrically toward coarse zooms and drops the lowest-priority survivors
// (by the same Q1 [`Priority`] order) until each level meets its budget.
//
// # Budget shape (tippecanoe drop-rate analog)
//
// The finest (canonical) level keeps everything. Each coarser level keeps a
// `1/drop_rate` fraction of the finer one, so the budget at level `L` is
// `budget(L) = N · (1/drop_rate)^(finest − L)` where `N` is the input feature
// count. The cut is a *ceiling*: a level whose cell-winner survivor count is
// already below its budget is left untouched (coarse zooms are cell-winner
// limited, not density limited). A floor ([`MIN_DENSITY_LEVEL_FEATURES`]) leaves
// already-sparse levels alone entirely.
//
// # Spatial fairness (tippecanoe gamma analog)
//
// A global rank-ordered cut would empty sparse rural neighborhoods to keep dense
// cities under budget. Instead the per-level budget is shared across coarse
// **super-cells** ([`SUPERCELL_GSD_FACTOR`] × GSD): each super-cell keeps its
// top-priority features up to an allocation `∝ population^(1/gamma)`,
// water-filled so no cell is allocated more than it has and the surplus flows to
// cells still under their cap. With `gamma = 1` the allocation is proportional
// (every neighborhood keeps the same fraction); `gamma > 1` is **sublinear** —
// dense neighborhoods keep proportionally fewer, sparse ones proportionally
// more. This is exactly tippecanoe's `-g`/gamma dot-dropping ("reduces dots to
// the `1/gamma` power of the original count in dense areas"; see
// `gap_density.rs`), applied per super-cell instead of globally.
//
// # Monotonicity / correctness
//
// Admission is greedy coarse→fine and a feature, once admitted, is never
// dropped at a finer level (its `min_level` is the level it was admitted at).
// This preserves duplicating monotonicity and — because the finest level admits
// every remaining feature — the canonical level is never thinned (spec §2.4).

use std::cmp::Ordering;

/// Super-cell edge length for spatial-fairness budget allocation, as a multiple
/// of the level GSD (in coordinate units). A super-cell is the neighborhood over
/// which the per-level budget is shared; `128 × GSD` is roughly a tile-scale
/// patch at the level's nominal display scale — coarse enough to hold many
/// features (so the budget can bind) yet fine enough that a city spans many
/// cells (so rural areas are not starved to feed it).
pub const SUPERCELL_GSD_FACTOR: f64 = 128.0;

/// Levels with fewer surviving features than this are exempt from the density
/// budget. Such levels are already grid-thinning-limited (sparse) rather than
/// density-limited, so a budget would only fight the cell-winner stage. This is
/// what keeps coarse zooms essentially unchanged under the default budget.
pub const MIN_DENSITY_LEVEL_FEATURES: usize = 256;

/// Configuration for the Q2 density budget (see the module section above).
#[derive(Debug, Clone, Copy)]
pub struct DensityBudgetConfig {
    /// Master switch. When `false`, [`apply_density_budget`] is an identity and
    /// the pipeline reproduces the pre-Q2 cell-winner behavior byte-for-byte.
    pub enabled: bool,
    /// Per-level drop rate: each coarser level keeps `1/drop_rate` of the next
    /// finer level. Must be `> 1` (values `<= 1` disable the budget). Larger ⇒
    /// coarser levels shed harder.
    pub drop_rate: f64,
    /// Spatial-fairness strength (`>= 1`). `1.0` = proportional cut; larger =
    /// sublinear, protecting sparse neighborhoods (dense areas kept to the
    /// `1/gamma` power of their population).
    pub gamma: f64,
}

impl Default for DensityBudgetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Calibrated on lines-portland-medium (duplicating, auto-rank): this
            // brings z9 (1.21x) and z10 (1.03x) into the 1.0–1.3x tippecanoe band
            // and z8 to ~1.0x, while the coarse zooms stay cell-winner-limited.
            // See `corpus/SWEEPS.md`. Smaller than tippecanoe's nominal 2.5
            // because our budget anchors on the full canonical count N (every
            // feature is present at the finest level), not a per-tile basezoom
            // count.
            drop_rate: 1.65,
            gamma: 1.5,
        }
    }
}

/// Apply the Q2 per-level density budget on top of a cell-winner [`Assignment`].
///
/// `features`, `level_gsds`, `config` (for the [`SortDirection`]) and `crs` must
/// be the exact inputs passed to [`assign_levels`] that produced `assignment`;
/// the returned assignment is parallel to `features` in the same way. When the
/// budget is disabled (or degenerate) the input assignment is returned unchanged.
pub fn apply_density_budget(
    assignment: &Assignment,
    features: &[AssignFeature],
    level_gsds: &[f64],
    config: &AssignConfig,
    budget: &DensityBudgetConfig,
    crs: Crs,
) -> Assignment {
    let num_levels = assignment.num_levels as usize;
    // Off-switch / degenerate: return the cell-winner assignment untouched.
    if !budget.enabled
        || budget.drop_rate <= 1.0
        || budget.drop_rate.is_nan()
        || features.is_empty()
        || num_levels < 2
    {
        return assignment.clone();
    }

    let n = features.len();
    let finest = num_levels - 1;

    // The coarsest level cell-winner permits each feature to appear at.
    let cw_min: Vec<usize> = assignment
        .assignments
        .iter()
        .map(|a| a.min_level as usize)
        .collect();

    // Q1 priority per feature — identical ordering to the cell-winner stage.
    let prio: Vec<Priority> = features
        .iter()
        .map(|f| Priority::new(f, config.sort_direction))
        .collect();

    let keep_frac = 1.0 / budget.drop_rate;
    let total = n as f64;
    let effective_budget = |level: usize| -> usize {
        if level >= finest {
            return n; // canonical keeps everything
        }
        let raw = total * keep_frac.powi((finest - level) as i32);
        (raw.round() as usize).max(MIN_DENSITY_LEVEL_FEATURES)
    };

    // Greedy admission, coarse→fine. `admitted_at[pos]` becomes the feature's
    // final (budgeted) min_level. Features not admitted at a level are deferred
    // to a finer one; the finest level admits all remaining (canonical fidelity).
    let mut admitted = vec![false; n];
    let mut admitted_at = vec![finest as u8; n];
    let mut kept_count = 0usize;

    // `level` is a scalar used in arithmetic/comparisons throughout the body
    // (not merely an index), so a range loop is the clearest form here.
    #[allow(clippy::needless_range_loop)]
    for level in 0..num_levels {
        let cands: Vec<usize> = (0..n)
            .filter(|&i| !admitted[i] && cw_min[i] <= level)
            .collect();
        if cands.is_empty() {
            continue;
        }
        let budget_l = effective_budget(level);

        if level == finest || kept_count + cands.len() <= budget_l {
            // Everything fits (or this is the canonical level): admit all.
            for &i in &cands {
                admitted[i] = true;
                admitted_at[i] = level as u8;
            }
            kept_count += cands.len();
            continue;
        }

        let available = budget_l.saturating_sub(kept_count);
        if available == 0 {
            // Grandfathered survivors already fill the budget; defer newcomers.
            continue;
        }

        // Budget binds: keep the top `available` candidates, fairly distributed
        // across super-cells, by Q1 priority within each cell.
        let chosen = select_budget_survivors(
            &cands,
            available,
            features,
            &prio,
            level_gsds[level],
            crs,
            budget.gamma,
        );
        for &i in &chosen {
            admitted[i] = true;
            admitted_at[i] = level as u8;
        }
        kept_count += chosen.len();
    }

    Assignment {
        assignments: features
            .iter()
            .zip(admitted_at)
            .map(|(f, min_level)| FeatureAssignment {
                index: f.index,
                min_level,
            })
            .collect(),
        num_levels: assignment.num_levels,
    }
}

/// Order two candidates best-first by Q1 [`Priority`] (a strict total order).
#[inline]
fn priority_order(prio: &[Priority], a: usize, b: usize) -> Ordering {
    if a == b {
        Ordering::Equal
    } else if prio[a].beats(&prio[b]) {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

/// Choose `available` survivors from `cands` for one level: partition into
/// super-cells, fair-allocate the budget across them ([`water_fill`]) and keep
/// each cell's top-priority members. Deterministic (cells iterated in sorted
/// key order; ties broken by [`Priority`]'s index tiebreak).
/// `pub(super)` so the coalescing stage (`super::coalesce`) can apply the
/// same per-level budget + spatial fairness to merged chains.
pub(super) fn select_budget_survivors(
    cands: &[usize],
    available: usize,
    features: &[AssignFeature],
    prio: &[Priority],
    gsd_m: f64,
    crs: Crs,
    gamma: f64,
) -> Vec<usize> {
    let gsd_units = gsd_to_coord_units(gsd_m, crs);
    let super_size = gsd_units * SUPERCELL_GSD_FACTOR;

    // Degenerate super-cell size (non-positive or NaN): fall back to a global
    // priority cut.
    if super_size <= 0.0 || super_size.is_nan() {
        let mut all = cands.to_vec();
        all.sort_by(|&a, &b| priority_order(prio, a, b));
        all.truncate(available);
        return all;
    }

    let mut cells: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for &i in cands {
        let (cx, cy) = features[i].center();
        let key = (
            (cx / super_size).floor() as i64,
            (cy / super_size).floor() as i64,
        );
        cells.entry(key).or_default().push(i);
    }
    for members in cells.values_mut() {
        members.sort_by(|&a, &b| priority_order(prio, a, b));
    }

    // Deterministic cell order for the water-fill + output.
    let mut keys: Vec<(i64, i64)> = cells.keys().copied().collect();
    keys.sort_unstable();
    let pops: Vec<usize> = keys.iter().map(|k| cells[k].len()).collect();

    let alpha = 1.0 / gamma.max(1.0);
    let allocs = water_fill(&pops, available, alpha);

    let mut chosen = Vec::with_capacity(available);
    for (k, a) in keys.iter().zip(allocs) {
        chosen.extend(cells[k].iter().take(a).copied());
    }
    chosen
}

/// Water-filling allocation of `budget` units across cells of the given
/// populations, weighting each cell by `population^alpha`, never exceeding a
/// cell's population, and redistributing any surplus to cells still under their
/// cap. Returns per-cell allocations summing to `min(budget, Σ pops)`.
fn water_fill(pops: &[usize], budget: usize, alpha: f64) -> Vec<usize> {
    let n = pops.len();
    let total_pop: usize = pops.iter().sum();
    if budget >= total_pop {
        return pops.to_vec();
    }
    let mut alloc = vec![0usize; n];
    let mut capped = vec![false; n];
    let mut remaining = budget;

    // Round 1..: cap any cell whose weighted share exceeds its population, then
    // redistribute the freed budget across the rest. Terminates because each
    // round with a cap removes at least one cell.
    loop {
        let sum_w: f64 = (0..n)
            .filter(|&i| !capped[i])
            .map(|i| (pops[i] as f64).powf(alpha))
            .sum();
        if sum_w <= 0.0 || remaining == 0 {
            break;
        }
        let mut newly_capped = false;
        for i in 0..n {
            if capped[i] {
                continue;
            }
            let raw = remaining as f64 * (pops[i] as f64).powf(alpha) / sum_w;
            if raw >= pops[i] as f64 {
                alloc[i] = pops[i];
                capped[i] = true;
                newly_capped = true;
            }
        }
        if newly_capped {
            let capped_sum: usize = (0..n).filter(|&i| capped[i]).map(|i| alloc[i]).sum();
            remaining = budget.saturating_sub(capped_sum);
            continue;
        }

        // No cell caps this round: hand out floors, then the leftover by largest
        // fractional part (a single pass suffices — leftover ≤ #fractional cells).
        let sum_w2 = sum_w;
        let mut leftover = remaining;
        let mut fracs: Vec<(usize, f64)> = Vec::new();
        for i in 0..n {
            if capped[i] {
                continue;
            }
            let raw = remaining as f64 * (pops[i] as f64).powf(alpha) / sum_w2;
            let floor = raw.floor();
            alloc[i] = floor as usize;
            leftover = leftover.saturating_sub(floor as usize);
            fracs.push((i, raw - floor));
        }
        fracs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
        for &(i, _) in &fracs {
            if leftover == 0 {
                break;
            }
            if alloc[i] < pops[i] {
                alloc[i] += 1;
                leftover -= 1;
            }
        }
        break;
    }
    alloc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GSD table from spec §5.2 for the zooms we test with.
    fn gsd(z: u32) -> f64 {
        40_075_016.69 / 1024.0 / 2f64.powi(z as i32)
    }

    fn point(index: usize, x: f64, y: f64) -> AssignFeature {
        AssignFeature {
            index,
            bbox: [x, y, x, y],
            kind: FeatureKind::Point,
            sort_key: None,
        }
    }

    fn poly(index: usize, xmin: f64, ymin: f64, xmax: f64, ymax: f64) -> AssignFeature {
        AssignFeature {
            index,
            bbox: [xmin, ymin, xmax, ymax],
            kind: FeatureKind::Polygon,
            sort_key: None,
        }
    }

    // ---- empty input --------------------------------------------------------

    #[test]
    fn empty_input_yields_empty_assignments() {
        let out = assign_levels(
            &[],
            &[gsd(4), gsd(6)],
            &AssignConfig::default(),
            Crs::Epsg3857,
        );
        assert!(out.assignments.is_empty());
        assert_eq!(out.num_levels, 2);
        assert!(out.duplicating_at_level(0).is_empty());
        assert!(out.partitioning_at_level(1).is_empty());
    }

    // ---- single-cell winner determinism -------------------------------------

    #[test]
    fn single_cell_one_winner_larger_polygon() {
        // Two polygons in the same coarse cell; the larger one wins level 0,
        // the smaller falls through to the finest level.
        let big = poly(0, 0.0, 0.0, 100_000.0, 100_000.0);
        let small = poly(1, 1000.0, 1000.0, 2000.0, 2000.0);
        let gsds = [gsd(2), gsd(6)];
        let out = assign_levels(
            &[big, small],
            &gsds,
            &AssignConfig::default(),
            Crs::Epsg3857,
        );
        assert_eq!(out.assignments[0].min_level, 0, "big polygon wins coarse");
        // small is gated out at level 0 anyway, but assert it is not at level 0.
        assert!(out.assignments[1].min_level > 0);
    }

    #[test]
    fn winner_is_deterministic_across_runs() {
        // Same-size, no sort key: winner decided by stable hash + index only,
        // so it must be identical every run regardless of input order.
        let a = poly(7, 0.0, 0.0, 50_000.0, 50_000.0);
        let b = poly(42, 100.0, 100.0, 50_100.0, 50_100.0);
        let gsds = [gsd(2), gsd(8)];
        let cfg = AssignConfig::default();
        let out1 = assign_levels(&[a, b], &gsds, &cfg, Crs::Epsg3857);
        let out2 = assign_levels(&[b, a], &gsds, &cfg, Crs::Epsg3857);
        // Find level-0 winner index in each ordering; must match.
        let w1: Vec<usize> = out1
            .assignments
            .iter()
            .filter(|x| x.min_level == 0)
            .map(|x| x.index)
            .collect();
        let w2: Vec<usize> = out2
            .assignments
            .iter()
            .filter(|x| x.min_level == 0)
            .map(|x| x.index)
            .collect();
        assert_eq!(w1, w2, "winner must not depend on input order");
        assert_eq!(w1.len(), 1, "exactly one winner in the shared cell");
    }

    #[test]
    fn all_features_one_cell_single_winner_at_coarsest() {
        // Many identical-size polygons in one coarse cell → exactly one wins
        // level 0; the rest fall through to the finest level.
        let feats: Vec<AssignFeature> = (0..20)
            .map(|i| poly(i, 0.0, 0.0, 40_000.0, 40_000.0))
            .collect();
        let gsds = [gsd(2), gsd(10)];
        let out = assign_levels(&feats, &gsds, &AssignConfig::default(), Crs::Epsg3857);
        let at0 = out.duplicating_at_level(0);
        assert_eq!(at0.len(), 1, "one winner at coarsest level");
    }

    #[test]
    fn parallel_grid_merge_is_deterministic_at_scale() {
        // Stress the sharded map-reduce grid build (#264): enough features,
        // spread across many cells with heavy same-cell contention, that rayon
        // splits the work into multiple shards whose partial grids must merge
        // to a single canonical result. The per-level `min_level` vector must
        // be byte-identical across repeated runs (no dependence on shard or
        // hashmap iteration order) and invariant to input order.
        let mut feats: Vec<AssignFeature> = Vec::new();
        // 50 coarse cells (5 km apart in EPSG:3857 meters), 400 polygons each
        // of varying size sharing that cell → 20k features, every cell heavily
        // contended so the winner selection actually exercises the merge.
        let mut idx = 0usize;
        for c in 0..50 {
            let base = c as f64 * 5_000.0;
            for k in 0..400 {
                let span = 200.0 + (k as f64) * 50.0;
                feats.push(poly(idx, base, base, base + span, base + span));
                idx += 1;
            }
        }
        let gsds = [gsd(4), gsd(8), gsd(12), gsd(14)];
        let cfg = AssignConfig::default();

        let baseline = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
        let levels_of = |a: &Assignment| -> Vec<(usize, u8)> {
            let mut v: Vec<(usize, u8)> = a
                .assignments
                .iter()
                .map(|x| (x.index, x.min_level))
                .collect();
            v.sort_unstable();
            v
        };
        let want = levels_of(&baseline);

        // Repeated runs must be identical (parallel merge determinism).
        for _ in 0..8 {
            let got = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
            assert_eq!(levels_of(&got), want, "assignment unstable across runs");
        }
        // Reversed input order must yield the same per-feature levels.
        let mut rev = feats.clone();
        rev.reverse();
        assert_eq!(
            levels_of(&assign_levels(&rev, &gsds, &cfg, Crs::Epsg3857)),
            want,
            "assignment must not depend on input order",
        );
    }

    // ---- visibility gating per kind -----------------------------------------

    #[test]
    fn points_always_eligible() {
        // A zero-size point must never be gated out; it wins its own cell at
        // level 0.
        let p = point(0, 0.0, 0.0);
        let gsds = [gsd(2), gsd(6)];
        let out = assign_levels(&[p], &gsds, &AssignConfig::default(), Crs::Epsg3857);
        assert_eq!(out.assignments[0].min_level, 0);
    }

    #[test]
    fn small_polygon_gated_until_fine_level() {
        // A polygon whose diagonal is smaller than the polygon visibility gate
        // at coarse levels but larger at the finest level. It should only
        // become eligible (and win) at the finest level.
        // Level 0 = gsd(2) ~ 9784 m; gate = 4*gsd. Level 2 = gsd(6) ~ 611 m.
        let g6 = gsd(6);
        // diagonal ~ 4.5 * gsd(6) in meters → eligible at level 2 (gate 4*gsd6)
        // but not at coarser levels (gate 4*gsd larger).
        let side = 4.5 * g6 / std::f64::consts::SQRT_2; // diag = side*sqrt2 = 4.5*gsd6
        let small = poly(0, 0.0, 0.0, side, side);
        let gsds = [gsd(2), gsd(4), gsd(6)];
        let out = assign_levels(&[small], &gsds, &AssignConfig::default(), Crs::Epsg3857);
        assert_eq!(
            out.assignments[0].min_level, 2,
            "polygon only visible at finest level"
        );
    }

    #[test]
    fn line_gate_less_strict_than_polygon() {
        // Same-size line vs polygon: with line_visibility 2 and polygon 4,
        // a geometry sized between the two gates is eligible as a line but not
        // as a polygon at a given level.
        let g4 = gsd(4);
        let side = 3.0 * g4 / std::f64::consts::SQRT_2; // diag = 3*gsd4
        let mut line = poly(0, 0.0, 0.0, side, side);
        line.kind = FeatureKind::Line;
        let polygon = poly(1, 1e7, 1e7, 1e7 + side, 1e7 + side); // far away cell
        let gsds = [gsd(4), gsd(8)];
        let out = assign_levels(
            &[line, polygon],
            &gsds,
            &AssignConfig::default(),
            Crs::Epsg3857,
        );
        // line: diag 3*gsd4 >= 2*gsd4 → eligible at level 0.
        assert_eq!(out.assignments[0].min_level, 0, "line visible at level 0");
        // polygon: diag 3*gsd4 < 4*gsd4 → gated at level 0, appears finer.
        assert!(out.assignments[1].min_level > 0, "polygon gated at level 0");
    }

    // ---- sort_key priority incl. null-loses ---------------------------------

    #[test]
    fn sort_key_beats_size_and_null_loses() {
        // Two polygons sharing a cell (same center 40000,40000), both large
        // enough to clear the level-0 visibility gate. The smaller one has a
        // sort_key; the larger has none. sort_key present must beat null →
        // smaller wins.
        let big_null = poly(0, 0.0, 0.0, 80_000.0, 80_000.0);
        let mut small_key = poly(1, 10_000.0, 10_000.0, 70_000.0, 70_000.0);
        small_key.sort_key = Some(5.0);
        let gsds = [gsd(2), gsd(8)];
        let out = assign_levels(
            &[big_null, small_key],
            &gsds,
            &AssignConfig::default(),
            Crs::Epsg3857,
        );
        assert!(
            out.assignments[0].min_level > 0,
            "null sort_key loses despite larger size"
        );
        assert_eq!(out.assignments[1].min_level, 0, "sort_key holder wins");
    }

    #[test]
    fn sort_key_direction_desc_vs_asc() {
        let a = {
            let mut f = poly(0, 0.0, 0.0, 40_000.0, 40_000.0);
            f.sort_key = Some(1.0);
            f
        };
        let b = {
            let mut f = poly(1, 100.0, 100.0, 40_100.0, 40_100.0);
            f.sort_key = Some(9.0);
            f
        };
        let gsds = [gsd(2), gsd(8)];

        let desc = AssignConfig {
            sort_direction: SortDirection::Desc,
            ..Default::default()
        };
        let out_desc = assign_levels(&[a, b], &gsds, &desc, Crs::Epsg3857);
        assert_eq!(
            out_desc.assignments[1].min_level, 0,
            "desc: larger key wins"
        );

        let asc = AssignConfig {
            sort_direction: SortDirection::Asc,
            ..Default::default()
        };
        let out_asc = assign_levels(&[a, b], &gsds, &asc, Crs::Epsg3857);
        assert_eq!(out_asc.assignments[0].min_level, 0, "asc: smaller key wins");
    }

    // ---- both mode expansions -----------------------------------------------

    #[test]
    fn duplicating_vs_partitioning_expansion() {
        // Construct a known assignment: three features at min_levels 0,1,2.
        let out = Assignment {
            assignments: vec![
                FeatureAssignment {
                    index: 10,
                    min_level: 0,
                },
                FeatureAssignment {
                    index: 11,
                    min_level: 1,
                },
                FeatureAssignment {
                    index: 12,
                    min_level: 2,
                },
            ],
            num_levels: 3,
        };
        // Duplicating: feature appears at every level >= min_level.
        assert_eq!(out.duplicating_at_level(0), vec![10]);
        assert_eq!(out.duplicating_at_level(1), vec![10, 11]);
        assert_eq!(out.duplicating_at_level(2), vec![10, 11, 12]);
        // Partitioning: feature appears at exactly min_level.
        assert_eq!(out.partitioning_at_level(0), vec![10]);
        assert_eq!(out.partitioning_at_level(1), vec![11]);
        assert_eq!(out.partitioning_at_level(2), vec![12]);
    }

    #[test]
    fn every_feature_present_at_finest_level_duplicating() {
        // Canonical fidelity (spec §2.4): all features appear at the finest
        // level in duplicating mode.
        let feats: Vec<AssignFeature> = (0..15)
            .map(|i| poly(i, 0.0, 0.0, 30_000.0, 30_000.0))
            .collect();
        let gsds = [gsd(2), gsd(4), gsd(6)];
        let out = assign_levels(&feats, &gsds, &AssignConfig::default(), Crs::Epsg3857);
        let finest = out.num_levels - 1;
        assert_eq!(
            out.duplicating_at_level(finest).len(),
            feats.len(),
            "all features present at finest level"
        );
    }

    // ---- 4326 vs 3857 grid sizing -------------------------------------------

    #[test]
    fn crs_affects_grid_sizing() {
        // Two points ~0.05° apart. In 4326 the grid cell (from gsd(6)≈611 m ⇒
        // ~0.0055° × thinning 4 ≈ 0.022°) is small enough to separate them, so
        // both win their own cell at level 0. Interpreted as 3857 (meters),
        // 0.05 "meters" is far smaller than the cell, so they collide and only
        // one wins.
        let p0 = point(0, 0.0, 0.0);
        let p1 = point(1, 0.05, 0.0);
        let gsds = [gsd(6), gsd(8)];
        let cfg = AssignConfig::default();

        let out_4326 = assign_levels(&[p0, p1], &gsds, &cfg, Crs::Epsg4326);
        assert_eq!(
            out_4326.duplicating_at_level(0).len(),
            2,
            "4326: points separated into two cells"
        );

        let out_3857 = assign_levels(&[p0, p1], &gsds, &cfg, Crs::Epsg3857);
        assert_eq!(
            out_3857.duplicating_at_level(0).len(),
            1,
            "3857: points collide into one cell"
        );
    }

    #[test]
    fn gsd_to_coord_units_conversion() {
        assert_eq!(gsd_to_coord_units(1000.0, Crs::Epsg3857), 1000.0);
        assert!((gsd_to_coord_units(111_320.0, Crs::Epsg4326) - 1.0).abs() < 1e-9);
    }

    // ---- Q2 density budget --------------------------------------------------

    /// Build an assignment with every feature at `min_level = 0` (cell-winner
    /// keeps all features at every level) so the *budget* alone drives counts.
    fn all_at_level_zero(n: usize, num_levels: u8) -> Assignment {
        Assignment {
            assignments: (0..n)
                .map(|i| FeatureAssignment {
                    index: i,
                    min_level: 0,
                })
                .collect(),
            num_levels,
        }
    }

    fn budget_cfg(enabled: bool, drop_rate: f64, gamma: f64) -> DensityBudgetConfig {
        DensityBudgetConfig {
            enabled,
            drop_rate,
            gamma,
        }
    }

    #[test]
    fn budget_geometric_decay_shape() {
        // With every feature present at every level (cell-winner non-binding),
        // per-level counts must follow the geometric budget N/rate^(finest-L).
        let n = 16_384usize;
        let num_levels = 6u8;
        let finest = (num_levels - 1) as usize;
        // Coords spread across several super-cells; counts are coord-independent.
        let feats: Vec<AssignFeature> = (0..n)
            .map(|i| {
                let x = (i % 128) as f64 * 5000.0;
                let y = (i / 128) as f64 * 5000.0;
                point(i, x, y)
            })
            .collect();
        let gsds: Vec<f64> = (0..num_levels).map(|z| gsd(z as u32)).collect();
        let base = all_at_level_zero(n, num_levels);

        let out = apply_density_budget(
            &base,
            &feats,
            &gsds,
            &AssignConfig::default(),
            &budget_cfg(true, 2.0, 1.0),
            Crs::Epsg3857,
        );

        let counts: Vec<usize> = (0..num_levels)
            .map(|l| out.duplicating_at_level(l).len())
            .collect();
        // Canonical keeps everything.
        assert_eq!(counts[finest], n, "canonical must keep all features");
        // Each finer level ~doubles the coarser one (rate = 2.0), within 6%.
        for w in counts.windows(2) {
            let ratio = w[1] as f64 / w[0] as f64;
            assert!(
                (ratio - 2.0).abs() < 0.12,
                "geometric decay violated: counts={counts:?} ratio={ratio}"
            );
        }
    }

    #[test]
    fn budget_keeps_high_priority_survivors() {
        // 1024 features in a single super-cell; the first 256 carry a high sort
        // key. The level-0 budget keeps 512, and every high-key feature (which
        // out-ranks the keyless ones) must survive the cut.
        let n = 1024usize;
        let high = 256usize;
        let feats: Vec<AssignFeature> = (0..n)
            .map(|i| {
                let mut f = point(i, i as f64 * 100.0, 0.0); // all within one super-cell
                if i < high {
                    f.sort_key = Some(100.0);
                }
                f
            })
            .collect();
        let gsds = [gsd(0), gsd(2)];
        let base = all_at_level_zero(n, 2);

        let out = apply_density_budget(
            &base,
            &feats,
            &gsds,
            &AssignConfig::default(),
            &budget_cfg(true, 2.0, 1.5),
            Crs::Epsg3857,
        );

        // Budget(0) = 1024/2 = 512 < 1024 → binds.
        assert_eq!(out.duplicating_at_level(0).len(), 512, "level-0 budget");
        // All high-key features present at the coarsest level.
        let high_at_0 = out
            .assignments
            .iter()
            .filter(|a| a.index < high && a.min_level == 0)
            .count();
        assert_eq!(
            high_at_0, high,
            "all high-priority features survive the cut"
        );
    }

    #[test]
    fn budget_spatial_fairness_protects_sparse() {
        // Two clusters far apart: A (sparse, 64 features) and B (dense, 576).
        // Under gamma>1 the sparse cluster keeps a larger share than a global
        // rank-ordered cut would give it.
        let a_n = 64usize;
        let b_n = 576usize;
        let n = a_n + b_n;
        let mut feats: Vec<AssignFeature> = Vec::with_capacity(n);
        for i in 0..a_n {
            feats.push(point(i, i as f64 * 1000.0, 0.0)); // near origin
        }
        for j in 0..b_n {
            feats.push(point(a_n + j, 10_000_000.0 + j as f64 * 1000.0, 0.0)); // far cluster
        }
        let gsds = [gsd(0), gsd(2)];
        let base = all_at_level_zero(n, 2);

        let out = apply_density_budget(
            &base,
            &feats,
            &gsds,
            &AssignConfig::default(),
            &budget_cfg(true, 2.0, 2.0),
            Crs::Epsg3857,
        );

        // Budget(0) = 640/2 = 320 → binds.
        let a_kept = out
            .assignments
            .iter()
            .filter(|a| a.index < a_n && a.min_level == 0)
            .count();

        // A global priority cut (top 320 across everything) keeps far fewer A.
        let mut order: Vec<usize> = (0..n).collect();
        let prio: Vec<Priority> = feats
            .iter()
            .map(|f| Priority::new(f, SortDirection::Desc))
            .collect();
        order.sort_by(|&a, &b| priority_order(&prio, a, b));
        let global_a = order.iter().take(320).filter(|&&i| i < a_n).count();

        assert_eq!(a_kept, a_n, "fairness keeps the entire sparse cluster");
        assert!(
            a_kept > global_a,
            "sparse cluster retains more than a global cut: fair={a_kept} global={global_a}"
        );
    }

    #[test]
    fn budget_canonical_never_dropped() {
        // Whatever the budget does at coarse levels, the finest level keeps every
        // feature (spec §2.4 canonical fidelity).
        let n = 4000usize;
        let num_levels = 5u8;
        let finest = num_levels - 1;
        let feats: Vec<AssignFeature> = (0..n)
            .map(|i| point(i, (i % 64) as f64 * 200.0, (i / 64) as f64 * 200.0))
            .collect();
        let gsds: Vec<f64> = (0..num_levels).map(|z| gsd(z as u32)).collect();
        let base = all_at_level_zero(n, num_levels);
        let out = apply_density_budget(
            &base,
            &feats,
            &gsds,
            &AssignConfig::default(),
            &DensityBudgetConfig::default(),
            Crs::Epsg3857,
        );
        assert_eq!(
            out.duplicating_at_level(finest).len(),
            n,
            "canonical level must contain every feature"
        );
    }

    #[test]
    fn budget_disabled_is_identity() {
        // The off switch reproduces the cell-winner assignment exactly.
        let feats: Vec<AssignFeature> = (0..500)
            .map(|i| point(i, (i % 20) as f64 * 100.0, (i / 20) as f64 * 100.0))
            .collect();
        let gsds = [gsd(2), gsd(4), gsd(6)];
        let cw = assign_levels(&feats, &gsds, &AssignConfig::default(), Crs::Epsg3857);
        let out = apply_density_budget(
            &cw,
            &feats,
            &gsds,
            &AssignConfig::default(),
            &budget_cfg(false, 2.0, 1.5),
            Crs::Epsg3857,
        );
        assert_eq!(
            cw.assignments, out.assignments,
            "disabled budget must be an identity"
        );
    }

    // ---- antimeridian-crossing bboxes (issue #188 behavior pins) -------------
    //
    // The convert pipeline stores geometries verbatim and computes bboxes with
    // `geo::bounding_rect` (plain min/max), so a feature straddling ±180° gets
    // an *inflated* bbox spanning nearly the whole world (lng_min ≈ -179.9,
    // lng_max ≈ +179.9) rather than a wrapped one (lng_min > lng_max never
    // arises). These tests PIN the downstream consequences for level
    // assignment — they document current behavior, not desired behavior. See
    // `context/ANTIMERIDIAN.md`.

    #[test]
    fn antimeridian_inflated_bbox_assigned_to_coarsest_level() {
        // A polygon whose true extent is 0.2° × 0.2° straddling the
        // antimeridian. Our bbox math hands assignment the inflated bbox
        // [-179.9, -0.1, 179.9, 0.1] (diag ≈ 359.8°), so the polygon clears
        // every visibility gate and wins the coarsest level.
        let inflated = poly(0, -179.9, -0.1, 179.9, 0.1);
        // The same feature's *true* extent, expressed unwrapped past 180°
        // (diag ≈ 0.28°): gated out of level 0 (gate = 4·gsd(2) ≈ 0.35°),
        // eligible only at the finest level.
        let true_extent = poly(1, 179.9, -0.1, 180.1, 0.1);
        let gsds = [gsd(2), gsd(6)];
        let out = assign_levels(
            &[inflated, true_extent],
            &gsds,
            &AssignConfig::default(),
            Crs::Epsg4326,
        );
        assert_eq!(
            out.assignments[0].min_level, 0,
            "PIN: inflated antimeridian bbox promotes a 0.2°-wide feature \
             to the coarsest level"
        );
        assert_eq!(
            out.assignments[1].min_level, 1,
            "the same feature at its true extent is visibility-gated to the \
             finest level"
        );
    }

    #[test]
    fn antimeridian_center_lands_on_prime_meridian_and_displaces_neighbor() {
        // The bbox center of the inflated bbox is lng ≈ 0 — the wrong
        // hemisphere. The feature therefore competes in a grid cell at the
        // prime meridian, and its huge bbox diagonal out-ranks any genuine
        // local feature sharing that cell.
        let antimeridian = poly(0, -179.9, -0.1, 179.9, 0.1); // center (0, 0)
        let local = poly(1, -0.5, -0.5, 0.5, 0.5); // genuinely at (0, 0)
        let gsds = [gsd(2), gsd(6)];
        let out = assign_levels(
            &[antimeridian, local],
            &gsds,
            &AssignConfig::default(),
            Crs::Epsg4326,
        );
        assert_eq!(
            out.assignments[0].min_level, 0,
            "PIN: antimeridian feature wins the prime-meridian cell"
        );
        assert_eq!(
            out.assignments[1].min_level, 1,
            "PIN: genuine prime-meridian feature is displaced to the finest \
             level by the antimeridian feature's inflated priority"
        );
    }
}
