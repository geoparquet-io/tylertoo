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
//! Per level, the only state is a hashmap `cell -> winner-position-so-far`,
//! `O(occupied cells)` — never `O(features)` retained across levels. The coarse
//! levels are built concurrently (#264, see below), so the transient peak is
//! the sum of the *concurrently live* grids. On large simple-geometry layers
//! that sum IS the whole-convert peak RSS (#295/#300: 5.9 GiB on
//! germany-segments, ~24 GiB at Brazil scale), so it is **bounded** (#306):
//! grid footprints are estimated up front and levels are scheduled in
//! memory-budgeted waves ([`assign_levels_bounded`]), trading cross-wave
//! concurrency for a capped peak when the budget binds. Grid entries store
//! only the winner position (priorities are recomputed on contest), roughly
//! halving per-entry cost.
//!
//! # Parallelism & determinism
//!
//! Each level's cell-winner pass is independent, so the coarse levels run
//! concurrently across rayon threads (#264 — this is the hot serial stage on
//! large-feature / simple-geometry layers), within the memory-bounded waves
//! described above (#306). Results are unaffected: the
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
pub use super::simplify::Representation;

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
/// polygon 2; sort descending. Point thinning matches the cogp-rs
/// reference; line thinning was retuned 2.0 → 1.0 after the 2026-07-02
/// Portland-roads parameter sweep (corpus/SWEEPS.md): lt=1 keeps
/// road networks visibly more continuous at coarse zooms, chosen by
/// maintainer review of the true-scale sweep renders. Polygon
/// visibility was retuned 4.0 → 2.0 in the 2026-07-15 coarse-zoom
/// sweep (#259, corpus/SWEEPS.md Decision 6): write-time RDP already
/// drops polygons whose simplified exterior falls below the level
/// tolerance (an effective ~2×GSD area gate on real-world shapes), so
/// the old 4×GSD eligibility gate was strictly stricter than what
/// survives writing and starved coarse zooms for no benefit — 4→2
/// gives 3–5× more coarse-level polygons on the Moldova corpus at
/// +0.1% file size, while values below 2 only admit candidates that
/// RDP kills anyway (wasted density-budget slots).
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
            polygon_visibility: 2.0,
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
    repr: Representation,
) -> Vec<usize> {
    // cell -> position of the winning feature in `features` (#306: the grid is
    // the dominant pass-1 allocation, so the entry stores ONLY the winner
    // position — the incumbent's Priority is recomputed on each contest instead
    // of cached. Priority::new is a handful of arithmetic ops + a murmur
    // finalizer, so the recompute is noise next to the hash-map probe, while
    // dropping the cached 40-byte Priority shrinks each entry from 72 to 32
    // bytes (~55% off the grid, the structure the level count multiplies).
    let mut grid: HashMap<CellKey, usize> = HashMap::new();

    for (pos, feat) in features.iter().enumerate() {
        // Zoom-band representation (#317 / #279): at a point-band level a
        // polygon is *rendered* as a representative point, so it gates and
        // thins like one — no visibility gate (a dot is always visible) and
        // the point grid factor. It keeps its own (Polygon) grid, so
        // polygons-as-points never compete with genuine point features for
        // cells. At a square-band level a polygon stays a polygon (its
        // below-tolerance disposition is a dithered placeholder square), so
        // it keeps the polygon thinning grid, but the visibility gate is
        // bypassed too — the tiny polygons are exactly the ones the dither
        // must see, and area lost to the gate would silently deflate the
        // aggregate-area invariant.
        let effective_kind = match repr {
            Representation::Point if feat.kind == FeatureKind::Polygon => FeatureKind::Point,
            _ => feat.kind,
        };

        // Visibility gate (points always pass).
        let vis = if repr == Representation::Square && feat.kind == FeatureKind::Polygon {
            0.0
        } else {
            config.visibility_factor(effective_kind)
        };
        if vis > 0.0 {
            let gate = vis * gsd_units;
            if feat.diag_sq() < gate * gate {
                continue; // ineligible at this level
            }
        }

        let cell_size = gsd_units * config.thinning_factor(effective_kind);
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

        grid.entry(key)
            .and_modify(|slot| {
                let challenger = Priority::new(feat, config.sort_direction);
                let incumbent = Priority::new(&features[*slot], config.sort_direction);
                if challenger.beats(&incumbent) {
                    *slot = pos;
                }
            })
            .or_insert(pos);
    }

    grid.into_values().collect()
}

/// Estimated retained bytes per occupied winner-grid cell (#306).
///
/// A compacted grid entry is a `(CellKey, usize)` bucket (32 bytes payload);
/// hashbrown adds control bytes and up to 2× capacity slack from power-of-two
/// growth, and each winner also occupies one `usize` slot in the per-level
/// winner vec. 96 bytes covers all of that with margin — biased HIGH (the #294
/// convention: an overestimate merely serializes a wave earlier; an
/// underestimate defeats the memory bound). Steers scheduling only, never
/// output.
const GRID_ENTRY_EST_BYTES: u64 = 96;

/// Extent of the feature centers (grid keys are derived from centers) and
/// per-kind feature counts — the two inputs of the per-level grid estimate.
/// Empty input yields a zero extent and zero counts.
fn center_extent_and_kind_counts(features: &[AssignFeature]) -> ((f64, f64), [usize; 3]) {
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    let mut counts = [0usize; 3];
    for feat in features {
        let (cx, cy) = feat.center();
        min_x = min_x.min(cx);
        min_y = min_y.min(cy);
        max_x = max_x.max(cx);
        max_y = max_y.max(cy);
        counts[feat.kind.discriminant() as usize] += 1;
    }
    if features.is_empty() {
        return ((0.0, 0.0), counts);
    }
    ((max_x - min_x, max_y - min_y), counts)
}

/// Estimated peak bytes of one level's winner grid (#306): per kind present,
/// occupied cells ≤ min(kind count, cells the center extent spans at that
/// kind's cell size), times [`GRID_ENTRY_EST_BYTES`]. The visibility gate can
/// only shrink the eligible set, so ignoring it keeps the estimate biased high.
fn estimate_level_grid_bytes(
    config: &AssignConfig,
    gsd_units: f64,
    extent: (f64, f64),
    kind_counts: &[usize; 3],
) -> u64 {
    let kinds = [FeatureKind::Point, FeatureKind::Line, FeatureKind::Polygon];
    let mut entries: u64 = 0;
    for kind in kinds {
        let count = kind_counts[kind.discriminant() as usize];
        if count == 0 {
            continue;
        }
        let cell = gsd_units * config.thinning_factor(kind);
        if cell <= 0.0 || cell.is_nan() {
            continue; // such features are skipped by the grid pass too
        }
        // +1.0: an extent spanning k full cells touches k+1 cell columns.
        let cells = ((extent.0 / cell).floor() + 1.0) * ((extent.1 / cell).floor() + 1.0);
        let capped = if cells.is_finite() && cells < count as f64 {
            cells as u64
        } else {
            count as u64
        };
        entries = entries.saturating_add(capped);
    }
    entries.saturating_mul(GRID_ENTRY_EST_BYTES)
}

/// Pack coarse levels (order preserved, coarse→fine) into contiguous waves
/// whose summed grid estimates fit `budget_bytes` (#306). Greedy: a wave always
/// holds at least one level (a level's grid cannot be split), so an oversized
/// level gets a wave of its own — fully serialized, the tightest bound this
/// scheme can give. With a large budget the result is a single wave, i.e. the
/// pre-#306 all-levels-concurrent build.
fn plan_level_waves(estimates: &[u64], budget_bytes: u64) -> Vec<std::ops::Range<usize>> {
    let mut waves = Vec::new();
    let mut start = 0usize;
    let mut sum = 0u64;
    for (i, &est) in estimates.iter().enumerate() {
        if i > start && sum.saturating_add(est) > budget_bytes {
            waves.push(start..i);
            start = i;
            sum = 0;
        }
        sum = sum.saturating_add(est);
    }
    if start < estimates.len() {
        waves.push(start..estimates.len());
    }
    waves
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
///
/// This variant places no bound on concurrent grid memory (single wave —
/// the pre-#306 behavior). Production paths pass a RAM budget through
/// [`assign_levels_bounded`]; the two produce identical assignments.
pub fn assign_levels(
    features: &[AssignFeature],
    level_gsds: &[f64],
    config: &AssignConfig,
    crs: Crs,
) -> Assignment {
    assign_levels_bounded(features, level_gsds, config, crs, u64::MAX, &[])
}

/// [`assign_levels`] with a zoom-band representation selector (#317).
///
/// `level_reprs` is parallel to `level_gsds` (missing tail entries read as
/// [`Representation::Geometry`]): [`Representation::Point`] marks a
/// point-band level, where polygon features gate and thin as points — the
/// polygon visibility gate is bypassed (their point representation is always
/// visible) and the point-thinning grid factor applies;
/// [`Representation::Square`] bypasses the gate but keeps the polygon grid.
/// Lines and genuine points are unaffected at every level.
///
/// Boundary-zoom note: winning a point-band cell lowers a polygon's
/// `min_level` like any other win, so in duplicating mode a small polygon
/// admitted to the band is also a *member* of the finer polygon levels down
/// to canonical. Write-time simplification still applies its 1×GSD
/// visibility gate there, so sub-visible polygons drop from those finer
/// levels' output (or collapse per the disposition); per the #259 sweep
/// (corpus/SWEEPS.md Decision 6), polygons between the 1×GSD write gate and
/// the 2×GSD assign gate are mostly killed by RDP anyway.
pub fn assign_levels_banded(
    features: &[AssignFeature],
    level_gsds: &[f64],
    config: &AssignConfig,
    crs: Crs,
    level_reprs: &[Representation],
) -> Assignment {
    assign_levels_bounded(features, level_gsds, config, crs, u64::MAX, level_reprs)
}

/// [`assign_levels`] with a cap on the transient winner-grid memory (#306)
/// and a zoom-band representation selector (#317; see
/// [`assign_levels_banded`] for the `level_reprs` semantics).
///
/// The #264 design builds every coarse level's cell-winner grid concurrently,
/// so the transient peak scales with `level_count × grid size` — the pass-1
/// peak the `[rss]` instrumentation pinned in #300 (5.9 GiB on
/// germany-segments; the ~24 GiB Brazil-scale peak of #295). This variant
/// schedules the levels in **memory-bounded waves**: per-level grid footprints
/// are estimated up front ([`estimate_level_grid_bytes`], biased high), levels
/// are greedily packed coarse→fine into waves whose summed estimate fits
/// `grid_budget_bytes` ([`plan_level_waves`]), and each wave's winners are
/// folded into `min_levels` and freed before the next wave starts. Peak grid
/// memory is `O(max wave)` instead of `O(sum of all levels)`.
///
/// Tradeoff (documented in #306): when the budget binds, levels in different
/// waves no longer run concurrently — on a roomy box the plan is a single wave
/// and #264's parallelism (and wall time) is untouched; on a constrained box
/// waves shrink toward one-level-per-wave, degrading gracefully to the serial
/// pre-#264 build rather than an OOM. The wave schedule is pure scheduling:
/// the assignment is **identical** for every budget (a feature takes the
/// coarsest level at which it wins, a fold that is order-independent).
pub fn assign_levels_bounded(
    features: &[AssignFeature],
    level_gsds: &[f64],
    config: &AssignConfig,
    crs: Crs,
    grid_budget_bytes: u64,
    level_reprs: &[Representation],
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
    // Parallelism (#264) + memory bound (#306): each coarse level's grid is
    // fully independent, so the per-level cell-winner passes run concurrently
    // across rayon threads WITHIN a wave (no merge — each level owns one
    // HashMap). The wave plan caps how many grids are live at once; each
    // wave's winner vecs are folded and dropped before the next wave
    // allocates. The fold is deterministic regardless of completion order: a
    // feature takes the *coarsest* (smallest) level at which it wins.
    let gsd_units: Vec<f64> = level_gsds[..finest as usize]
        .iter()
        .map(|&g| gsd_to_coord_units(g, crs))
        .collect();
    let (extent, kind_counts) = center_extent_and_kind_counts(features);
    let estimates: Vec<u64> = gsd_units
        .iter()
        .map(|&g| estimate_level_grid_bytes(config, g, extent, &kind_counts))
        .collect();
    let waves = plan_level_waves(&estimates, grid_budget_bytes);
    if waves.len() > 1 {
        let total_mib = estimates.iter().copied().sum::<u64>() / (1024 * 1024);
        let budget_mib = grid_budget_bytes / (1024 * 1024);
        log::info!(
            "[assign] winner grids est {total_mib} MiB exceed the {budget_mib} MiB \
             budget: building {} coarse level(s) in {} memory-bounded wave(s) (#306)",
            estimates.len(),
            waves.len()
        );
    }

    for wave in waves {
        let winners_per_level: Vec<Vec<usize>> = wave
            .clone()
            .into_par_iter()
            .map(|level_idx| {
                let repr = level_reprs.get(level_idx).copied().unwrap_or_default();
                level_winner_positions(features, config, gsd_units[level_idx], repr)
            })
            .collect();

        for (offset, winners) in winners_per_level.iter().enumerate() {
            let level = (wave.start + offset) as u8;
            for &pos in winners {
                if level < min_levels[pos] {
                    min_levels[pos] = level;
                }
            }
        }
        // `winners_per_level` (and the wave's grids, already dropped inside
        // `level_winner_positions`) are freed here, before the next wave.
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
        // Level 0 = gsd(2) ~ 9784 m; gate = 2*gsd (default). Level 2 = gsd(6)
        // ~ 611 m.
        let g6 = gsd(6);
        // diagonal ~ 4.5 * gsd(6) in meters → eligible at level 2 (gate
        // 2*gsd6 ≈ 2*611) but not at coarser levels (level 1 gate 2*gsd4 ≈
        // 4892 > 4.5*gsd6 ≈ 2752).
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
    fn per_kind_visibility_gates_are_independent() {
        // Same-size line vs polygon with explicitly different per-kind gates
        // (line 2, polygon 4 — the pre-#259 polygon default): a geometry
        // sized between the two gates is eligible as a line but not as a
        // polygon at a given level. The default config now sets both gates
        // to 2.0 (see `default_visibility_gates`), so this pins explicit
        // values to keep exercising the per-kind independence.
        let g4 = gsd(4);
        let side = 3.0 * g4 / std::f64::consts::SQRT_2; // diag = 3*gsd4
        let mut line = poly(0, 0.0, 0.0, side, side);
        line.kind = FeatureKind::Line;
        let polygon = poly(1, 1e7, 1e7, 1e7 + side, 1e7 + side); // far away cell
        let gsds = [gsd(4), gsd(8)];
        let cfg = AssignConfig {
            polygon_visibility: 4.0,
            ..AssignConfig::default()
        };
        let out = assign_levels(&[line, polygon], &gsds, &cfg, Crs::Epsg3857);
        // line: diag 3*gsd4 >= 2*gsd4 → eligible at level 0.
        assert_eq!(out.assignments[0].min_level, 0, "line visible at level 0");
        // polygon: diag 3*gsd4 < 4*gsd4 → gated at level 0, appears finer.
        assert!(out.assignments[1].min_level > 0, "polygon gated at level 0");
    }

    #[test]
    fn default_visibility_gates() {
        // #259: the default polygon gate is 2.0 — the write-time RDP
        // collapse already imposes an effective ~2×GSD survival bar, so a
        // 4×GSD eligibility gate only starved coarse levels (see
        // corpus/SWEEPS.md Decision 6). Line gate stays 2.0 (Portland
        // sweep), points are never gated.
        let cfg = AssignConfig::default();
        assert_eq!(cfg.polygon_visibility, 2.0);
        assert_eq!(cfg.line_visibility, 2.0);
        assert_eq!(cfg.visibility_factor(FeatureKind::Point), 0.0);
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
        // A polygon whose true extent is 0.1° × 0.1° straddling the
        // antimeridian. Our bbox math hands assignment the inflated bbox
        // [-179.95, -0.05, 179.95, 0.05] (diag ≈ 359.9°), so the polygon
        // clears every visibility gate and wins the coarsest level.
        let inflated = poly(0, -179.95, -0.05, 179.95, 0.05);
        // The same feature's *true* extent, expressed unwrapped past 180°
        // (diag ≈ 0.14°): gated out of level 0 (gate = 2·gsd(2) ≈ 0.18°),
        // eligible only at the finest level.
        let true_extent = poly(1, 179.95, -0.05, 180.05, 0.05);
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

    // ---- #306: memory-bounded winner-grid waves -----------------------------

    #[test]
    fn plan_waves_single_when_under_budget() {
        // Everything fits in one wave → identical to the pre-#306 all-at-once
        // parallel build.
        assert_eq!(plan_level_waves(&[100, 200, 300], 1_000), vec![0..3]);
        // Exactly at budget still fits.
        assert_eq!(plan_level_waves(&[100, 200, 300], 600), vec![0..3]);
        // No levels → no waves.
        assert!(plan_level_waves(&[], 1).is_empty());
    }

    #[test]
    fn plan_waves_splits_preserving_order_with_min_one_level() {
        // Budget smaller than any single level: one wave per level (a level
        // never splits — its grid must be materialized whole).
        assert_eq!(
            plan_level_waves(&[500, 500, 500], 100),
            vec![0..1, 1..2, 2..3]
        );
        // Mixed sizes: greedy coarse→fine packing, order preserved.
        assert_eq!(
            plan_level_waves(&[100, 100, 300, 50], 250),
            vec![0..2, 2..3, 3..4]
        );
        // Saturating: absurd estimates never panic; a finite budget splits them.
        assert_eq!(
            plan_level_waves(&[u64::MAX, u64::MAX], u64::MAX - 1),
            vec![0..1, 1..2]
        );
    }

    #[test]
    fn grid_estimate_caps_entries_at_feature_count() {
        // A tiny cell over a huge extent would imply astronomically many cells,
        // but a grid can never hold more entries than there are features.
        let cfg = AssignConfig::default();
        let est = estimate_level_grid_bytes(&cfg, 1.0, (1e12, 1e12), &[1_000, 0, 0]);
        assert_eq!(est, 1_000 * GRID_ENTRY_EST_BYTES);
        // No features → no grid.
        assert_eq!(
            estimate_level_grid_bytes(&cfg, 1.0, (1e12, 1e12), &[0, 0, 0]),
            0
        );
    }

    #[test]
    fn grid_estimate_extent_bound_binds_at_coarse_levels() {
        // A coarse cell over a small extent bounds the grid by cell count, not
        // feature count: 1M points in a 10×10-cell extent ≈ ~121 cells.
        let cfg = AssignConfig {
            point_thinning: 1.0,
            ..AssignConfig::default()
        };
        let est = estimate_level_grid_bytes(&cfg, 100.0, (1_000.0, 1_000.0), &[1_000_000, 0, 0]);
        assert_eq!(est, 121 * GRID_ENTRY_EST_BYTES);
        let fine = estimate_level_grid_bytes(&cfg, 10.0, (1_000.0, 1_000.0), &[1_000_000, 0, 0]);
        assert!(fine > est, "finer GSD ⇒ more cells ⇒ larger estimate");
    }

    #[test]
    fn bounded_waves_match_unbounded_assignment() {
        // A scattered mix of kinds and sizes across several levels. A 1-byte
        // grid budget forces one level per wave (fully serialized build); the
        // resulting assignment must be identical to the unbounded single-wave
        // build — the wave schedule is pure scheduling, never policy.
        let mut feats: Vec<AssignFeature> = Vec::new();
        let mut idx = 0usize;
        for c in 0..40 {
            let base = c as f64 * 7_000.0;
            for k in 0..25 {
                let span = 150.0 + (k as f64) * 400.0;
                feats.push(poly(idx, base, base, base + span, base + span));
                idx += 1;
                feats.push(point(idx, base + k as f64 * 31.0, base + k as f64 * 17.0));
                idx += 1;
            }
        }
        let gsds = [gsd(2), gsd(5), gsd(8), gsd(11), gsd(14)];
        let cfg = AssignConfig::default();

        let unbounded = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
        let bounded = assign_levels_bounded(&feats, &gsds, &cfg, Crs::Epsg3857, 1, &[]);
        assert_eq!(unbounded.assignments, bounded.assignments);
        assert_eq!(unbounded.num_levels, bounded.num_levels);

        // An intermediate budget (some multi-level waves) must also match.
        let mid_budget = 200 * GRID_ENTRY_EST_BYTES;
        let mid = assign_levels_bounded(&feats, &gsds, &cfg, Crs::Epsg3857, mid_budget, &[]);
        assert_eq!(unbounded.assignments, mid.assignments);
    }

    /// #306 × #317: the wave schedule is pure scheduling under banding too —
    /// a 1-byte budget (one level per wave, serialized) must produce the
    /// identical banded assignment as the unbounded single-wave build.
    #[test]
    fn bounded_waves_match_unbounded_banded_assignment() {
        let mut feats: Vec<AssignFeature> = Vec::new();
        for i in 0..200 {
            let base = (i % 20) as f64 * 0.01;
            feats.push(poly(
                i,
                base,
                base,
                base + 0.0005 + (i as f64) * 1e-6,
                base + 0.0005,
            ));
        }
        let gsds = [gsd(2), gsd(5), gsd(8), gsd(12)];
        let cfg = AssignConfig::default();
        let reprs = [
            Representation::Point,
            Representation::Point,
            Representation::Square,
            Representation::Geometry,
        ];
        let unbounded = assign_levels_banded(&feats, &gsds, &cfg, Crs::Epsg4326, &reprs);
        let bounded = assign_levels_bounded(&feats, &gsds, &cfg, Crs::Epsg4326, 1, &reprs);
        assert_eq!(unbounded.assignments, bounded.assignments);
        assert!(
            unbounded.assignments.iter().any(|a| a.min_level == 0),
            "point band must admit coarse winners"
        );
    }

    // ---- zoom-band point representation (#317) ------------------------------

    #[test]
    fn banded_point_levels_bypass_polygon_visibility_gate() {
        // A polygon far below the coarse level's visibility gate: normally
        // stuck at the finest level, but eligible at a point-band level
        // (where it is rendered as a dot).
        let small = poly(0, 0.0, 0.0, 0.001, 0.001);
        let gsds = [gsd(2), gsd(12)];
        let cfg = AssignConfig::default();

        let plain = assign_levels(&[small], &gsds, &cfg, Crs::Epsg4326);
        assert_eq!(
            plain.assignments[0].min_level, 1,
            "precondition: gated out of the coarse level without a band"
        );

        let banded = assign_levels_banded(
            &[small],
            &gsds,
            &cfg,
            Crs::Epsg4326,
            &[Representation::Point, Representation::Geometry],
        );
        assert_eq!(
            banded.assignments[0].min_level, 0,
            "point-band level must admit the sub-gate polygon"
        );
    }

    #[test]
    fn banded_empty_flags_match_plain_assign() {
        let feats = [
            poly(0, 0.0, 0.0, 5.0, 5.0),
            poly(1, 10.0, 10.0, 10.001, 10.001),
        ];
        let gsds = [gsd(2), gsd(6), gsd(12)];
        let cfg = AssignConfig::default();
        assert_eq!(
            assign_levels(&feats, &gsds, &cfg, Crs::Epsg4326).assignments,
            assign_levels_banded(&feats, &gsds, &cfg, Crs::Epsg4326, &[]).assignments
        );
        assert_eq!(
            assign_levels(&feats, &gsds, &cfg, Crs::Epsg4326).assignments,
            assign_levels_banded(
                &feats,
                &gsds,
                &cfg,
                Crs::Epsg4326,
                &[Representation::Geometry; 3]
            )
            .assignments
        );
    }

    #[test]
    fn banded_point_levels_thin_polygons_on_point_grid() {
        // Two small polygons in the same point-thinning cell at the coarse
        // level: with the band, only ONE wins the cell (the other stays
        // finer) — proof the point grid (point_thinning factor) applies
        // rather than every polygon passing the bypassed gate.
        let a = poly(0, 0.0, 0.0, 0.002, 0.002);
        let b = poly(1, 0.003, 0.003, 0.004, 0.004);
        let gsds = [gsd(2), gsd(12)];
        let cfg = AssignConfig::default();
        let out = assign_levels_banded(
            &[a, b],
            &gsds,
            &cfg,
            Crs::Epsg4326,
            &[Representation::Point, Representation::Geometry],
        );
        let coarse = out.assignments.iter().filter(|x| x.min_level == 0).count();
        assert_eq!(
            coarse, 1,
            "one winner per point-grid cell at the band level"
        );
        // The larger polygon (bigger diag) wins the cell.
        assert_eq!(out.assignments[0].min_level, 0);
        assert_eq!(out.assignments[1].min_level, 1);
    }

    #[test]
    fn banded_square_levels_bypass_gate_on_polygon_grid() {
        // Two sub-gate polygons ~0.15 deg apart: different polygon-thinning
        // cells (1×gsd(2) ≈ 0.088 deg) but the SAME point-thinning cell
        // (4×gsd(2) ≈ 0.35 deg). A square band must admit BOTH — gate
        // bypassed, but thinning on the polygon grid, not the point grid.
        let a = poly(0, 0.0, 0.0, 0.001, 0.001);
        let b = poly(1, 0.15, 0.0, 0.151, 0.001);
        let gsds = [gsd(2), gsd(12)];
        let cfg = AssignConfig::default();

        let plain = assign_levels(&[a, b], &gsds, &cfg, Crs::Epsg4326);
        assert!(
            plain.assignments.iter().all(|x| x.min_level == 1),
            "precondition: both gated out of the coarse level without a band"
        );

        let square_band = assign_levels_banded(
            &[a, b],
            &gsds,
            &cfg,
            Crs::Epsg4326,
            &[Representation::Square, Representation::Geometry],
        );
        assert!(
            square_band.assignments.iter().all(|x| x.min_level == 0),
            "square band must admit both tiny polygons (polygon grid cells \
             differ): {:?}",
            square_band.assignments
        );

        let point_band = assign_levels_banded(
            &[a, b],
            &gsds,
            &cfg,
            Crs::Epsg4326,
            &[Representation::Point, Representation::Geometry],
        );
        assert_eq!(
            point_band
                .assignments
                .iter()
                .filter(|x| x.min_level == 0)
                .count(),
            1,
            "point band thins the same pair on the coarser point grid"
        );
    }

    #[test]
    fn banded_lines_and_points_unaffected() {
        // A sub-gate line stays gated at a point-band level: the band selects
        // the POLYGON representation only.
        let line = AssignFeature {
            index: 0,
            bbox: [0.0, 0.0, 0.001, 0.001],
            kind: FeatureKind::Line,
            sort_key: None,
        };
        let gsds = [gsd(2), gsd(12)];
        let cfg = AssignConfig::default();
        let plain = assign_levels(&[line], &gsds, &cfg, Crs::Epsg4326);
        let banded = assign_levels_banded(
            &[line],
            &gsds,
            &cfg,
            Crs::Epsg4326,
            &[Representation::Point, Representation::Geometry],
        );
        assert_eq!(plain.assignments, banded.assignments);
    }
}
