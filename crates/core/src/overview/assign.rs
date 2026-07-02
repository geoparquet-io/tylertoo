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
//! # Memory (streaming-critical property)
//!
//! Per level, the only state is a hashmap `cell -> best-priority-so-far`. This
//! is the property the streaming refactor (plan V4) depends on: peak memory is
//! `O(occupied cells)` per level, not `O(features)` retained across levels.
//!
//! # Determinism
//!
//! The per-cell winner is the maximum under a *strict total order* (ties are
//! ultimately broken by the feature `index`), so results never depend on
//! hashmap iteration order and are identical across runs.
//!
//! # DIVERGENCE FROM TIPPECANOE / cogp-rs
//!
//! We use the **bbox center** as a feature's representative point (v1). A
//! centroid or a first-vertex would be closer to cartographic convention for
//! irregular polygons/lines; bbox center is cheap, needs no geometry decode,
//! and is adequate for thinning at overview scales. Revisit if quality gating
//! (plan V2) shows artifacts.

use std::collections::HashMap;

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
    fn center(&self) -> (f64, f64) {
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
/// Defaults match the spec/cogp-rs reference: point thinning 4, line 2,
/// polygon 1; line visibility 2, polygon 4; sort descending.
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

impl Default for AssignConfig {
    fn default() -> Self {
        Self {
            point_thinning: 4.0,
            line_thinning: 2.0,
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
#[derive(Debug, Clone, Copy)]
struct Priority {
    /// Encoded sort key: `Some` sorts above `None`; direction already applied
    /// so that "larger `sort_bits` wins".
    sort_rank: Option<f64>,
    diag_sq: f64,
    hash: u64,
    /// Smaller index wins ties; stored negated-in-comparison.
    index: usize,
}

impl Priority {
    fn new(feat: &AssignFeature, dir: SortDirection) -> Self {
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
    fn beats(&self, other: &Priority) -> bool {
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
    for (level_idx, &gsd_m) in level_gsds.iter().enumerate() {
        let level = level_idx as u8;
        let gsd_units = gsd_to_coord_units(gsd_m, crs);

        // cell -> (best priority, position of winning feature in `features`)
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
            // Guard against a zero/negative cell size (bad GSD input).
            if !(cell_size > 0.0) {
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

        // Winners at this level take it as their min_level, unless a coarser
        // level already claimed them (we only lower, never raise).
        for (_prio, pos) in grid.values() {
            if level < min_levels[*pos] {
                min_levels[*pos] = level;
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
}
