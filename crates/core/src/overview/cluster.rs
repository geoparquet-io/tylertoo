//! Point clustering + attribute aggregation for overview levels (plan Q4/Q6).
//!
//! This is a **pure** stage layered on top of the final level assignment
//! (cell-winner + density budget). It answers, per level: *which present
//! point row represents each source point feature, how many features does
//! each present row represent (`point_count`), and what are the aggregated
//! attribute values of the features it absorbed?*
//!
//! # Model (duplicating mode only)
//!
//! At each overview level `L` the cell-winner assignment keeps one winner
//! point per occupied grid cell; with clustering enabled the winner **absorbs**
//! the losers in its cell at that level instead of them simply vanishing:
//!
//! - Every source point feature is assigned exactly one representative among
//!   the rows *present* at `L` (`min_level <= L`): itself if present, else the
//!   best-priority present point feature in its level-`L` grid cell (the same
//!   cell size and [`Priority`] order the cell-winner stage used).
//! - `point_count` of a present row at level `L` = the number of source
//!   features it represents at that level (itself + absorbed). At the
//!   canonical (finest) level every cluster is a singleton (`point_count = 1`).
//! - Absorption is **per level, from source values**: a feature absorbed at
//!   level `L` may itself be a winner at finer level `L+1`, and each level's
//!   aggregates are computed over the full set of *source* features in the
//!   winner's cell at that level's grid — never from already-aggregated
//!   values, so `mean` is numerically exact at every level.
//!
//! Lines and polygons are unaffected (their rows carry `point_count = 1`).
//!
//! # Orphan cells (density-budget interaction)
//!
//! Without the Q2 density budget, every occupied point cell's winner is
//! present at the level it won, so every source point finds a representative
//! in its own cell. The budget, however, can *defer* a cell winner to a finer
//! level, leaving the cell with no present row ("orphan cell"). Orphan cells
//! are resolved deterministically: the cell's features attach to the present
//! point feature nearest to the orphan cell's center, found by an expanding
//! ring search over the level grid (ties broken by [`Priority`]). This keeps
//! the invariant *Σ point_count over a level's point rows = total source
//! point count* whenever the level has at least one point row.
//!
//! # DIVERGENCE FROM SUPERCLUSTER
//!
//! The winner keeps its **own geometry** (and its own values for every
//! non-accumulated column). Supercluster re-centers a cluster at the weighted
//! centroid of its members; we deliberately do not — keeping the winner's
//! geometry is deterministic, preserves a real feature location, and requires
//! no geometry rewrite at coarse levels.

use std::collections::HashMap;

use super::assign::{AssignConfig, AssignFeature, FeatureKind, Priority};
use super::level::Crs;

/// Name of the mandatory cluster-size column written when clustering is
/// enabled (tippecanoe / supercluster convention).
pub const POINT_COUNT_COLUMN: &str = "point_count";

/// Numeric aggregation operators for `--accumulate-attribute` (Q6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccumulateOp {
    /// Sum of the non-null values.
    Sum,
    /// Maximum of the non-null values.
    Max,
    /// Minimum of the non-null values.
    Min,
    /// Arithmetic mean of the non-null values (sum + count accumulated
    /// internally; exact per level, never a mean of means).
    Mean,
}

impl AccumulateOp {
    /// Parse an operator name (case-insensitive): `sum`, `max`, `min`, `mean`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "sum" => Some(Self::Sum),
            "max" => Some(Self::Max),
            "min" => Some(Self::Min),
            "mean" => Some(Self::Mean),
            _ => None,
        }
    }

    /// Canonical lower-case name (footer provenance / display).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Max => "max",
            Self::Min => "min",
            Self::Mean => "mean",
        }
    }
}

/// One `--accumulate-attribute col:op` request.
#[derive(Debug, Clone, PartialEq)]
pub struct AccumulateSpec {
    /// Numeric source column whose values are aggregated across a cluster.
    pub column: String,
    /// Aggregation operator.
    pub op: AccumulateOp,
}

/// A non-singleton cluster at one level: the winner's `point_count` and its
/// finalized per-spec aggregates.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusterEntry {
    /// Number of source features this row represents at the level (>= 2;
    /// singleton winners are omitted from the table and default to 1).
    pub point_count: i64,
    /// Finalized aggregate per [`AccumulateSpec`], parallel to the spec list.
    /// `None` = no non-null contributor; the winner's own (null) value stands.
    pub aggregates: Vec<Option<f64>>,
}

/// Per-level cluster tables, parallel to `level_gsds`. Keyed by the winner's
/// [`AssignFeature::index`]. Only non-singleton clusters are stored (memory is
/// `O(actual clusters)`), and the canonical (finest) level's table is always
/// empty: every cluster there is a singleton and rows pass through verbatim
/// (spec §2.4 value-identity).
pub type ClusterTables = Vec<HashMap<usize, ClusterEntry>>;

/// Per-cluster running aggregate state for one [`AccumulateSpec`].
#[derive(Debug, Clone, Copy)]
struct AggState {
    sum: f64,
    min: f64,
    max: f64,
    /// Non-null contributors.
    count: u64,
}

impl AggState {
    fn new() -> Self {
        Self {
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            count: 0,
        }
    }

    fn add(&mut self, v: f64) {
        self.sum += v;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.count += 1;
    }

    fn finalize(&self, op: AccumulateOp) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        Some(match op {
            AccumulateOp::Sum => self.sum,
            AccumulateOp::Max => self.max,
            AccumulateOp::Min => self.min,
            AccumulateOp::Mean => self.sum / self.count as f64,
        })
    }
}

/// Build the per-level cluster tables from the **final** level assignment.
///
/// - `features`: the exact inputs given to the assignment engine (any kinds;
///   only [`FeatureKind::Point`] features participate in clustering).
/// - `min_levels`: final per-feature coarsest level, parallel to `features`
///   (after [`assign_levels`](super::assign::assign_levels) and any
///   [`apply_density_budget`](super::assign::apply_density_budget)).
/// - `level_gsds`: level GSDs in meters, coarse→fine, as used for assignment.
/// - `config` / `crs`: the assignment configuration (point thinning factor
///   drives the grid; sort direction drives the priority order).
/// - `values`: per [`AccumulateSpec`], the per-feature source values
///   (parallel to `features`; `None` = null).
/// - `ops`: the operators, parallel to `values`.
///
/// Duplicating-mode semantics: a feature is *present* at level `L` iff
/// `min_levels <= L`. Partitioning mode is not supported (see
/// `ConvertError::ClusterPartitioningUnsupported`).
pub fn build_cluster_tables(
    features: &[AssignFeature],
    min_levels: &[u8],
    level_gsds: &[f64],
    config: &AssignConfig,
    crs: Crs,
    values: &[Vec<Option<f64>>],
    ops: &[AccumulateOp],
) -> ClusterTables {
    debug_assert_eq!(features.len(), min_levels.len());
    debug_assert_eq!(values.len(), ops.len());

    let num_levels = level_gsds.len();
    let mut tables: ClusterTables = vec![HashMap::new(); num_levels];
    if num_levels == 0 || features.is_empty() {
        return tables;
    }
    let finest = num_levels - 1;

    // Positions of the point features (the only clustering participants).
    let point_pos: Vec<usize> = features
        .iter()
        .enumerate()
        .filter(|(_, f)| f.kind == FeatureKind::Point)
        .map(|(p, _)| p)
        .collect();
    if point_pos.is_empty() {
        return tables;
    }

    let prio: Vec<Priority> = features
        .iter()
        .map(|f| Priority::new(f, config.sort_direction))
        .collect();

    // Non-canonical levels only: at the finest level every point is present,
    // every cluster is a singleton, and rows pass through verbatim.
    for level in 0..finest {
        let cell_size = crs.meters_to_units(level_gsds[level]) * config.point_thinning;
        if cell_size <= 0.0 || cell_size.is_nan() {
            continue;
        }
        let cell = |pos: usize| -> (i64, i64) {
            let (cx, cy) = features[pos].center();
            (
                (cx / cell_size).floor() as i64,
                (cy / cell_size).floor() as i64,
            )
        };

        // Best-priority PRESENT point feature per occupied grid cell.
        let mut present: HashMap<(i64, i64), usize> = HashMap::new();
        for &pos in &point_pos {
            if min_levels[pos] as usize > level {
                continue;
            }
            let key = cell(pos);
            present
                .entry(key)
                .and_modify(|best| {
                    if prio[pos].beats(&prio[*best]) {
                        *best = pos;
                    }
                })
                .or_insert(pos);
        }
        if present.is_empty() {
            // No point row at this level at all (pathological: e.g. every
            // point deferred in a mixed dataset): nothing to attach counts to.
            continue;
        }

        // Representative per source point feature: itself if present, else the
        // best present feature in its cell, else (orphan cell) resolved below.
        let mut rep: Vec<usize> = Vec::with_capacity(point_pos.len());
        let mut orphan_cells: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for &pos in &point_pos {
            if min_levels[pos] as usize <= level {
                rep.push(pos); // a present row always represents itself
                continue;
            }
            let key = cell(pos);
            match present.get(&key) {
                Some(&w) => rep.push(w),
                None => {
                    orphan_cells.entry(key).or_default().push(pos);
                    rep.push(usize::MAX); // patched after orphan resolution
                }
            }
        }

        // Resolve orphan cells (density-budget deferrals): nearest present
        // feature by expanding ring search over the level grid, deterministic.
        if !orphan_cells.is_empty() {
            let mut orphan_keys: Vec<(i64, i64)> = orphan_cells.keys().copied().collect();
            orphan_keys.sort_unstable();
            let mut resolved: HashMap<(i64, i64), usize> = HashMap::new();
            for key in orphan_keys {
                let w = nearest_present(key, &present, features, &prio, cell_size);
                resolved.insert(key, w);
            }
            for (i, &pos) in point_pos.iter().enumerate() {
                if rep[i] == usize::MAX {
                    rep[i] = resolved[&cell(pos)];
                }
            }
        }

        // Accumulate counts + aggregates per representative.
        let mut acc: HashMap<usize, (i64, Vec<AggState>)> = HashMap::new();
        for (i, &pos) in point_pos.iter().enumerate() {
            let w = rep[i];
            let entry = acc
                .entry(w)
                .or_insert_with(|| (0, vec![AggState::new(); ops.len()]));
            entry.0 += 1;
            for (s, vals) in values.iter().enumerate() {
                if let Some(v) = vals[pos] {
                    entry.1[s].add(v);
                }
            }
        }

        // Keep only non-singleton clusters (singletons pass through verbatim).
        let table = &mut tables[level];
        for (w, (count, states)) in acc {
            if count <= 1 {
                continue;
            }
            table.insert(
                features[w].index,
                ClusterEntry {
                    point_count: count,
                    aggregates: states
                        .iter()
                        .zip(ops)
                        .map(|(st, &op)| st.finalize(op))
                        .collect(),
                },
            );
        }
    }

    tables
}

/// Deterministic nearest present point feature to the center of `cell_key`,
/// searched over expanding Chebyshev rings of the level grid. Among the
/// candidates of the first non-empty ring, the one with the smallest squared
/// distance to the orphan cell's center wins; exact ties fall back to the
/// cell-winner [`Priority`] order. `present` is non-empty (checked by caller),
/// so the search terminates within the present cells' key bounds.
fn nearest_present(
    cell_key: (i64, i64),
    present: &HashMap<(i64, i64), usize>,
    features: &[AssignFeature],
    prio: &[Priority],
    cell_size: f64,
) -> usize {
    let center = (
        (cell_key.0 as f64 + 0.5) * cell_size,
        (cell_key.1 as f64 + 0.5) * cell_size,
    );
    let dist_sq = |pos: usize| -> f64 {
        let (x, y) = features[pos].center();
        let dx = x - center.0;
        let dy = y - center.1;
        dx * dx + dy * dy
    };
    let better = |a: usize, b: usize| -> bool {
        let (da, db) = (dist_sq(a), dist_sq(b));
        if da != db {
            da < db
        } else {
            prio[a].beats(&prio[b])
        }
    };

    // Maximum useful ring radius: the farthest present cell (Chebyshev).
    let max_r = present
        .keys()
        .map(|&(x, y)| (x - cell_key.0).abs().max((y - cell_key.1).abs()))
        .max()
        .expect("present is non-empty");

    let mut best: Option<usize> = None;
    for r in 1..=max_r {
        for (dx, dy) in ring_offsets(r) {
            if let Some(&w) = present.get(&(cell_key.0 + dx, cell_key.1 + dy)) {
                if best.map_or(true, |b| better(w, b)) {
                    best = Some(w);
                }
            }
        }
        if let Some(b) = best {
            // A feature in ring r can be nearer than one in ring r+1's cells,
            // but never farther than ring r+2's; one extra ring guarantees the
            // true nearest. Scan ring r+1 then stop.
            let rr = r + 1;
            if rr <= max_r {
                for (dx, dy) in ring_offsets(rr) {
                    if let Some(&w) = present.get(&(cell_key.0 + dx, cell_key.1 + dy)) {
                        if better(w, b) {
                            best = Some(w);
                        }
                    }
                }
            }
            return best.expect("best set");
        }
    }
    // Unreachable when present is non-empty and max_r bounds the search, but
    // fall back to a global scan for absolute safety.
    let mut all: Vec<usize> = present.values().copied().collect();
    all.sort_unstable();
    all.into_iter()
        .reduce(|a, b| if better(b, a) { b } else { a })
        .expect("present is non-empty")
}

/// The Chebyshev ring of radius `r` around the origin (the 8r cells whose
/// max-coordinate distance is exactly `r`), in deterministic order.
fn ring_offsets(r: i64) -> Vec<(i64, i64)> {
    debug_assert!(r >= 1);
    let mut out = Vec::with_capacity((8 * r) as usize);
    for dx in -r..=r {
        out.push((dx, -r));
        out.push((dx, r));
    }
    for dy in (-r + 1)..r {
        out.push((-r, dy));
        out.push((r, dy));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::assign::{assign_levels, SortDirection};

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

    /// Sum of point_count over a level's PRESENT point rows (singletons = 1).
    fn level_count_sum(
        features: &[AssignFeature],
        min_levels: &[u8],
        table: &HashMap<usize, ClusterEntry>,
        level: u8,
    ) -> i64 {
        features
            .iter()
            .zip(min_levels)
            .filter(|(f, &ml)| f.kind == FeatureKind::Point && ml <= level)
            .map(|(f, _)| table.get(&f.index).map_or(1, |e| e.point_count))
            .sum()
    }

    #[test]
    fn accumulate_op_parse_and_names() {
        assert_eq!(AccumulateOp::parse("sum"), Some(AccumulateOp::Sum));
        assert_eq!(AccumulateOp::parse("MAX"), Some(AccumulateOp::Max));
        assert_eq!(AccumulateOp::parse("Min"), Some(AccumulateOp::Min));
        assert_eq!(AccumulateOp::parse("mean"), Some(AccumulateOp::Mean));
        assert_eq!(AccumulateOp::parse("median"), None);
        assert_eq!(AccumulateOp::Mean.as_str(), "mean");
    }

    /// 10 points in 2 far-apart clumps (6 + 4) with one coarse level whose
    /// point cell swallows each clump whole: the two winners get point_count
    /// 6 and 4, and the canonical table is empty (all singletons).
    #[test]
    fn ten_points_two_cells_counts_six_and_four() {
        // Level 0: gsd(2) ≈ 9784 m; point cell = 4·gsd ≈ 39 km. Clump A at
        // origin (spread 1 km), clump B at x = 10_000 km.
        let mut feats: Vec<AssignFeature> = Vec::new();
        for i in 0..6 {
            feats.push(point(i, i as f64 * 200.0, 0.0));
        }
        for j in 0..4 {
            feats.push(point(6 + j, 1.0e7 + j as f64 * 200.0, 0.0));
        }
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig::default();
        let assignment = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let tables =
            build_cluster_tables(&feats, &min_levels, &gsds, &cfg, Crs::Epsg3857, &[], &[]);
        assert_eq!(tables.len(), 2);

        // Exactly two present rows at level 0, with counts {6, 4}.
        let present0: Vec<usize> = (0..feats.len()).filter(|&i| min_levels[i] == 0).collect();
        assert_eq!(present0.len(), 2, "one winner per clump at level 0");
        let mut counts: Vec<i64> = present0
            .iter()
            .map(|&i| tables[0].get(&feats[i].index).map_or(1, |e| e.point_count))
            .collect();
        counts.sort_unstable();
        assert_eq!(counts, vec![4, 6]);

        // Sum over level-0 point rows == total source point count.
        assert_eq!(level_count_sum(&feats, &min_levels, &tables[0], 0), 10);
        // Canonical table empty; per-row counts default to 1; sum still 10.
        assert!(tables[1].is_empty(), "canonical level has no clusters");
        assert_eq!(level_count_sum(&feats, &min_levels, &tables[1], 1), 10);
    }

    #[test]
    fn aggregation_ops_including_nulls() {
        // One cluster of 4 points at level 0 (all within one coarse cell).
        // Values: 10, 30, null, 20 → sum 60, max 30, min 10, mean 20.
        let feats: Vec<AssignFeature> = (0..4).map(|i| point(i, i as f64 * 100.0, 0.0)).collect();
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig::default();
        let assignment = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let vals = vec![vec![Some(10.0), Some(30.0), None, Some(20.0)]; 4];
        let ops = [
            AccumulateOp::Sum,
            AccumulateOp::Max,
            AccumulateOp::Min,
            AccumulateOp::Mean,
        ];
        let tables =
            build_cluster_tables(&feats, &min_levels, &gsds, &cfg, Crs::Epsg3857, &vals, &ops);

        assert_eq!(tables[0].len(), 1, "one cluster at level 0");
        let entry = tables[0].values().next().unwrap();
        assert_eq!(entry.point_count, 4);
        assert_eq!(
            entry.aggregates,
            vec![Some(60.0), Some(30.0), Some(10.0), Some(20.0)]
        );
    }

    #[test]
    fn all_null_values_yield_none_aggregate() {
        let feats: Vec<AssignFeature> = (0..3).map(|i| point(i, i as f64 * 100.0, 0.0)).collect();
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig::default();
        let assignment = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();
        let vals = vec![vec![None, None, None]];
        let tables = build_cluster_tables(
            &feats,
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &vals,
            &[AccumulateOp::Sum],
        );
        let entry = tables[0].values().next().unwrap();
        assert_eq!(entry.point_count, 3);
        assert_eq!(entry.aggregates, vec![None]);
    }

    /// Mean is exact per level (computed from source values, not a mean of
    /// per-cluster means): two sub-clusters of unequal size merging at the
    /// coarse level must yield the true source mean, not the mean-of-means.
    #[test]
    fn mean_is_exact_across_levels_not_mean_of_means() {
        // Level 1 grid (gsd(6)·4 ≈ 2446 m cells): clump A = 3 points (values
        // 0,0,0) in one cell, clump B = 1 point (value 8) in a nearby cell.
        // Level 0 grid (gsd(2)·4 ≈ 39 km): both clumps in ONE cell.
        // True mean = 8/4 = 2. Mean of level-1 cluster means = (0+8)/2 = 4.
        let mut feats = vec![
            point(0, 0.0, 0.0),
            point(1, 100.0, 0.0),
            point(2, 200.0, 0.0),
            point(3, 5000.0, 0.0), // separate level-1 cell, same level-0 cell
        ];
        // Make feature 3 the level-0 winner-independent: give 0 high priority
        // via sort key so the level-0 winner is deterministic (id 0).
        feats[0].sort_key = Some(1.0);
        let gsds = [gsd(2), gsd(6), gsd(12)];
        let cfg = AssignConfig::default();
        let assignment = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let vals = vec![vec![Some(0.0), Some(0.0), Some(0.0), Some(8.0)]];
        let tables = build_cluster_tables(
            &feats,
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &vals,
            &[AccumulateOp::Mean],
        );

        // Level 0: a single 4-point cluster with the exact source mean 2.0.
        let e0 = tables[0].get(&0).expect("feature 0 wins level 0");
        assert_eq!(e0.point_count, 4);
        assert_eq!(e0.aggregates, vec![Some(2.0)]);

        // Level 1: clump A collapses to one 3-point cluster (mean 0); the
        // clump-B point is its own singleton (absent from the table).
        let sum1 = level_count_sum(&feats, &min_levels, &tables[1], 1);
        assert_eq!(sum1, 4, "level-1 counts partition the source set");
        let e1 = tables[1]
            .values()
            .find(|e| e.point_count == 3)
            .expect("3-point cluster at level 1");
        assert_eq!(e1.aggregates, vec![Some(0.0)]);
    }

    /// Per-level absorption: a feature absorbed at level 0 is a winner at
    /// level 1 with its own (smaller) cluster — counts reflect each level's
    /// grid independently and always partition the source set.
    #[test]
    fn per_level_absorption_partitions_at_every_level() {
        // 12 points in 3 clumps 5 km apart: one level-0 cell (39 km) holds
        // all; level-1 cells (2.4 km) separate the clumps.
        let mut feats = Vec::new();
        for c in 0..3 {
            for i in 0..4 {
                feats.push(point(c * 4 + i, c as f64 * 5000.0 + i as f64 * 50.0, 0.0));
            }
        }
        let gsds = [gsd(2), gsd(6), gsd(12)];
        let cfg = AssignConfig::default();
        let assignment = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let tables =
            build_cluster_tables(&feats, &min_levels, &gsds, &cfg, Crs::Epsg3857, &[], &[]);

        // Level 0: one winner holding all 12.
        let l0_winners: Vec<usize> = (0..feats.len()).filter(|&i| min_levels[i] == 0).collect();
        assert_eq!(l0_winners.len(), 1);
        assert_eq!(tables[0][&l0_winners[0]].point_count, 12);

        // Level 1: three present rows (one per clump), each holding 4 — the
        // level-0 winner's count SHRINKS to its own clump at the finer grid.
        assert_eq!(level_count_sum(&feats, &min_levels, &tables[1], 1), 12);
        let present1: Vec<usize> = (0..feats.len()).filter(|&i| min_levels[i] <= 1).collect();
        assert_eq!(present1.len(), 3, "one winner per clump at level 1");
        for &w in &present1 {
            assert_eq!(
                tables[1].get(&w).map_or(1, |e| e.point_count),
                4,
                "each level-1 winner holds its own clump"
            );
        }

        // Canonical: all singletons.
        assert!(tables[2].is_empty());
        assert_eq!(level_count_sum(&feats, &min_levels, &tables[2], 2), 12);
    }

    /// Orphan cells (budget-deferred winners) attach to the nearest present
    /// feature; the per-level sum invariant survives.
    #[test]
    fn orphan_cell_attaches_to_nearest_present_winner() {
        // Three points in three separate level-0 cells. Simulate a density
        // budget having deferred point 1's cell winner: min_levels says only
        // points 0 and 2 are present at level 0.
        let cell = 4.0 * gsd(2); // level-0 point cell size in meters (3857)
        let feats = vec![
            point(0, 0.5 * cell, 0.0),
            point(1, 1.5 * cell, 0.0), // orphan cell (deferred winner)
            point(2, 4.5 * cell, 0.0),
        ];
        let min_levels = vec![0u8, 1, 0];
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig::default();

        let tables =
            build_cluster_tables(&feats, &min_levels, &gsds, &cfg, Crs::Epsg3857, &[], &[]);

        // Point 1 attaches to point 0 (1 cell away) not point 2 (3 cells).
        assert_eq!(tables[0].get(&0).map(|e| e.point_count), Some(2));
        assert!(!tables[0].contains_key(&2), "far winner stays a singleton");
        assert_eq!(level_count_sum(&feats, &min_levels, &tables[0], 0), 3);
    }

    /// Lines/polygons never participate: no table entries, and point counts
    /// ignore them entirely.
    #[test]
    fn non_point_features_are_ignored() {
        let mut feats = vec![
            point(0, 0.0, 0.0),
            point(1, 100.0, 0.0),
            AssignFeature {
                index: 2,
                bbox: [0.0, 0.0, 50_000.0, 50_000.0],
                kind: FeatureKind::Polygon,
                sort_key: None,
            },
            AssignFeature {
                index: 3,
                bbox: [0.0, 0.0, 60_000.0, 60_000.0],
                kind: FeatureKind::Line,
                sort_key: None,
            },
        ];
        feats[0].sort_key = Some(1.0);
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig::default();
        let assignment = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let tables =
            build_cluster_tables(&feats, &min_levels, &gsds, &cfg, Crs::Epsg3857, &[], &[]);
        // Only the two points cluster (into one 2-point cluster on feature 0).
        assert_eq!(tables[0].len(), 1);
        assert_eq!(tables[0].get(&0).map(|e| e.point_count), Some(2));
        assert!(!tables[0].contains_key(&2));
        assert!(!tables[0].contains_key(&3));
    }

    /// Winner priority alignment: the clustering representative in a cell is
    /// the same feature the cell-winner stage picked (sort-key order).
    #[test]
    fn representative_matches_cell_winner_priority() {
        let mut feats: Vec<AssignFeature> =
            (0..5).map(|i| point(i, i as f64 * 10.0, 0.0)).collect();
        feats[3].sort_key = Some(99.0); // highest priority wins the cell
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig {
            sort_direction: SortDirection::Desc,
            ..Default::default()
        };
        let assignment = assign_levels(&feats, &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();
        assert_eq!(min_levels[3], 0, "sort-key holder wins the coarse cell");

        let tables =
            build_cluster_tables(&feats, &min_levels, &gsds, &cfg, Crs::Epsg3857, &[], &[]);
        assert_eq!(tables[0].get(&3).map(|e| e.point_count), Some(5));
    }

    #[test]
    fn empty_inputs_and_no_points_are_noops() {
        let gsds = [gsd(2), gsd(6)];
        let cfg = AssignConfig::default();
        let t = build_cluster_tables(&[], &[], &gsds, &cfg, Crs::Epsg3857, &[], &[]);
        assert!(t.iter().all(|m| m.is_empty()));

        let poly = AssignFeature {
            index: 0,
            bbox: [0.0, 0.0, 50_000.0, 50_000.0],
            kind: FeatureKind::Polygon,
            sort_key: None,
        };
        let t = build_cluster_tables(&[poly], &[0], &gsds, &cfg, Crs::Epsg3857, &[], &[]);
        assert!(t.iter().all(|m| m.is_empty()));
    }

    #[test]
    fn ring_offsets_cover_ring_exactly() {
        for r in 1..=3i64 {
            let ring = ring_offsets(r);
            assert_eq!(ring.len() as i64, 8 * r);
            assert!(ring.iter().all(|&(x, y)| x.abs().max(y.abs()) == r));
            let mut sorted = ring.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), ring.len(), "no duplicate cells");
        }
    }
}
