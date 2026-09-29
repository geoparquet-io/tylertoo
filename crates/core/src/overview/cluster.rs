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
//!   cell size and [`Priority`](super::assign::Priority) order the cell-winner
//!   stage used).
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
//! point feature nearest (Euclidean) to the orphan cell's center, over every
//! present point of the level, found by one scan over the present cells
//! (ties broken by [`Priority`](super::assign::Priority), then input
//! position). This keeps the invariant *Σ point_count over a level's point
//! rows = total source point count* whenever the level has at least one point
//! row.
//!
//! # DIVERGENCE FROM SUPERCLUSTER
//!
//! The winner keeps its **own geometry** (and its own values for every
//! non-accumulated column). Supercluster re-centers a cluster at the weighted
//! centroid of its members; we deliberately do not — keeping the winner's
//! geometry is deterministic, preserves a real feature location, and requires
//! no geometry rewrite at coarse levels.

use std::collections::{HashMap, HashSet};

use super::assign::{AssignConfig, FeatureKind, FeatureTable, SortDirection};
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
///
/// The [`AssignFeature::index`] of every POINT feature must be unique. The
/// writer applies an entry to every row that carries its index, so a shared
/// index would stamp one cluster's `point_count` on several rows and break
/// the spec §12.1 sum invariant. If a caller passes duplicate point indices
/// anyway, [`build_cluster_tables`] still returns the same table on every
/// call (the clusters of all present rows sharing an index are merged into
/// that index's entry, #609), and [`verify_sum_invariant`] rejects it.
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
    features: &FeatureTable,
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
    let point_pos: Vec<usize> = (0..features.len())
        .filter(|&p| features.kind(p) == FeatureKind::Point)
        .collect();
    if point_pos.is_empty() {
        return tables;
    }

    // Priorities are DERIVED where a comparison needs one, not tabulated
    // (#565). `Priority` is 32 B, so a `Vec<Priority>` here was another
    // dataset-wide table alongside the pass-1 feature table — and it bought
    // almost nothing: the two comparisons below run once per *present* point
    // per level (the cell-winner fold) and once per orphan-cell candidate,
    // not inside an O(n log n) sort, so recomputing is in the noise.
    let prio = |pos: usize| features.priority(pos, config.sort_direction);

    // Thinning off (`--verbatim`, or `--point-thinning 0`) makes every cell
    // guard below fail, so no cluster table is built and every `point_count`
    // defaults to 1. That answer is CORRECT — with no thinning every point
    // survives at every level, so every cluster genuinely is a singleton — but
    // the run still appends the column and records `clustering.enabled: true`
    // in the footer, which overstates what happened. Say so rather than
    // leaving the caller to infer it from a column of 1s.
    if config.point_thinning == 0.0 {
        log::warn!(
            "--cluster with point thinning off: every point survives at every              level, so every cluster is a singleton and every point_count is 1.              The point_count column and the clustering provenance are still              written. Set --point-thinning above 0 to actually cluster."
        );
    }

    // Non-canonical levels only: at the finest level every point is present,
    // every cluster is a singleton, and rows pass through verbatim.
    for level in 0..finest {
        let cell_size = crs.meters_to_units(level_gsds[level]) * config.point_thinning;
        if cell_size <= 0.0 || cell_size.is_nan() {
            continue;
        }
        let cell = |pos: usize| -> (i64, i64) {
            let (cx, cy) = features.center(pos);
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
                    if prio(pos).beats(&prio(*best)) {
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
        // feature by one scan over the present cells (see `nearest_present`),
        // deterministic.
        if !orphan_cells.is_empty() {
            let mut orphan_keys: Vec<(i64, i64)> = orphan_cells.keys().copied().collect();
            orphan_keys.sort_unstable();
            let mut resolved: HashMap<(i64, i64), usize> = HashMap::new();
            for key in orphan_keys {
                let w = nearest_present(key, &present, features, config.sort_direction, cell_size);
                resolved.insert(key, w);
            }
            for (i, &pos) in point_pos.iter().enumerate() {
                if rep[i] == usize::MAX {
                    rep[i] = resolved[&cell(pos)];
                }
            }
        }

        // Accumulate counts + aggregates per representative's `index` (#609).
        //
        // The table is keyed by `AssignFeature::index`. Point indices must be
        // unique (see [`ClusterTables`]), and with unique indices this is the
        // per-row table. Keying the accumulator by POSITION and then inserting
        // by index let two present rows that share an index overwrite each
        // other in `HashMap` iteration order, so a caller that broke the
        // uniqueness rule got a different table on each call. Accumulating by
        // index merges such rows into one entry instead, deterministically:
        // values are added in `point_pos` order. The merged table is still not
        // a valid output (the writer would stamp the merged count on every row
        // sharing the index), and `verify_sum_invariant` rejects it.
        let mut acc: HashMap<usize, (i64, Vec<AggState>)> = HashMap::new();
        for (i, &pos) in point_pos.iter().enumerate() {
            let w = rep[i];
            let entry = acc
                .entry(features.indices()[w])
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
        for (index, (count, states)) in acc {
            if count <= 1 {
                continue;
            }
            table.insert(
                index,
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

/// Verify the strict §12.1 accounting / sum invariant over freshly built
/// cluster tables: at every level, every source point feature is counted in
/// exactly one point row of that level, so `Σ point_count` over a level's
/// point rows equals the total source point count exactly — under any drop
/// mechanism (cell-winner thinning, density budget, their interactions).
/// Two present point rows sharing an [`AssignFeature::index`] at a level are
/// a violation too: the writer applies an entry to every row carrying its
/// index, so the rows would repeat one cluster's `point_count`.
///
/// Also asserts the derived producer obligation: a clustered level MUST NOT
/// thin its points to zero while the source contains points — there must be
/// a surviving point row to absorb the re-assignments.
///
/// Inputs are the exact arguments/results of [`build_cluster_tables`]
/// (`min_levels` parallel to `features`). Returns a human-readable violation
/// description; callers surface it as a conversion error.
pub fn verify_sum_invariant(
    features: &FeatureTable,
    min_levels: &[u8],
    tables: &ClusterTables,
) -> Result<(), String> {
    debug_assert_eq!(features.len(), min_levels.len());
    let total: i64 = (0..features.len())
        .filter(|&p| features.kind(p) == FeatureKind::Point)
        .count() as i64;
    if total == 0 {
        return Ok(()); // no source points: nothing to account for
    }
    for (level, table) in tables.iter().enumerate() {
        let mut sum = 0i64;
        let mut point_rows = 0usize;
        // Indices of the table entries seen at this level, to reject a
        // duplicate present point index (#609). Only table keys are tracked
        // (memory O(clusters), never O(rows)). That catches every duplicate
        // `build_cluster_tables` can leave at a clustered level: rows sharing
        // an index are merged into one entry of point_count >= 2, so the
        // index is always in the table. At the canonical level the table is
        // empty and a shared index goes undetected, but every row there
        // writes point_count 1, so the per-row sum is still exact.
        let mut seen: HashSet<usize> = HashSet::new();
        for (pos, &ml) in min_levels.iter().enumerate() {
            if features.kind(pos) != FeatureKind::Point || ml as usize > level {
                continue;
            }
            let f_index = features.indices()[pos];
            point_rows += 1;
            match table.get(&f_index) {
                Some(e) => {
                    if !seen.insert(f_index) {
                        return Err(format!(
                            "clustered level {level}: two present point rows \
                             share feature index {}; cluster tables are keyed \
                             by index, so each row would carry the same \
                             point_count (point feature indices must be unique)",
                            f_index
                        ));
                    }
                    sum += e.point_count;
                }
                None => sum += 1,
            }
        }
        if point_rows == 0 {
            return Err(format!(
                "clustered level {level} has no surviving point row to absorb \
                 {total} source points (spec §12.1: a clustered level cannot \
                 thin points to zero while the source contains points)"
            ));
        }
        if sum != total {
            return Err(format!(
                "clustered level {level}: sum(point_count) over point rows = \
                 {sum}, expected the source point count {total} (spec §12.1 \
                 sum invariant)"
            ));
        }
    }
    Ok(())
}
/// Deterministic nearest present point feature to the center of `cell_key`.
///
/// The rule: over EVERY present cell winner, the smallest squared Euclidean
/// distance from the feature's center to the orphan cell's center wins;
/// exact ties fall back to the cell-winner
/// [`Priority`](super::assign::Priority) order, and a full `Priority` tie
/// (only possible when two features share an `index`) to the smaller
/// position. A NaN distance (a NaN center, reachable only through the
/// public API) counts as infinitely far. The comparison is therefore a strict
/// total order, and the answer does not depend on `HashMap` iteration order.
///
/// # One scan instead of rings (#610)
///
/// The original expanding-ring search visited every cell of rings
/// `1..=r + 1` (about `4r²` hash lookups and one `Vec` per ring), so an orphan
/// 64,000 cells from the nearest present cell took minutes. It also paid a
/// full pass over `present` up front for its radius bound. This function does
/// only that one pass, so each orphan costs `O(|present|)` whatever the gap.
/// A cell at Chebyshev distance `r` (in cells) holds points no nearer than
/// `(r - 0.5)` cells to the orphan center, so a cell whose lower bound already
/// exceeds the best distance so far is skipped without computing a distance
/// (only for keys within ±2^40, where `f64` still resolves single cells).
///
/// The ring search stopped after rings `r_min` and `r_min + 1`. That is not
/// the nearest point: a point in the far corner of a ring-`r` cell is up to
/// `sqrt(2)·(r + 0.5)` cells away, while one in a ring-`r + 2` cell can be as
/// near as `r + 1.5`, which is smaller for every `r >= 2`. The ring limit only
/// bounded the walk; the scan reads every present cell anyway, so it returns
/// the true nearest, as the spec's §12.1 note ("tylertoo: the nearest") says.
///
/// # Saturated keys (#611)
///
/// A finite but huge coordinate (e.g. `1e300`) gives a grid key saturated to
/// `i64::MAX`/`i64::MIN`, and the difference of two keys then overflows
/// `i64` (a panic: release builds keep `overflow-checks`). Keys are now
/// subtracted only for the pruning bound, and only when both lie within
/// ±2^40; other cells are measured in `f64` directly. No `cell_key + offset`
/// sums remain. Every point more than about `9.2e18`
/// cells from the origin shares the saturated cell, and an orphan in that
/// cell is measured from the saturated cell's center, not from its points,
/// so the representative chosen for it has no spatial meaning; it only keeps
/// the point counted (§12.1).
fn nearest_present(
    cell_key: (i64, i64),
    present: &HashMap<(i64, i64), usize>,
    features: &FeatureTable,
    dir: SortDirection,
    cell_size: f64,
) -> usize {
    let center = (
        (cell_key.0 as f64 + 0.5) * cell_size,
        (cell_key.1 as f64 + 0.5) * cell_size,
    );
    let dist_sq = |pos: usize| -> f64 {
        let (x, y) = features.center(pos);
        let dx = x - center.0;
        let dy = y - center.1;
        let d = dx * dx + dy * dy;
        if d.is_nan() {
            f64::INFINITY
        } else {
            d
        }
    };
    // Does `a` (at squared distance `da`) beat `b` (at `db`)?
    let beats = |a: usize, da: f64, b: usize, db: f64| -> bool {
        if da != db {
            return da < db;
        }
        let (pa, pb) = (features.priority(a, dir), features.priority(b, dir));
        if pa.beats(&pb) {
            true
        } else if pb.beats(&pa) {
            false
        } else {
            a < b
        }
    };
    // Lower bound on the squared distance from the orphan center to any
    // point in the cell at `key`, or 0 (no pruning) where it is not safe.
    // The exact bound is `r - 0.5` cells; `r - 1` leaves half a cell of slack
    // for float rounding in `floor(x / cell_size)` and in the center. That
    // slack only holds while cell indices are small enough for `f64` to
    // resolve single cells: beyond about 2^52 neighbouring cells collapse to
    // the same coordinate (and saturated keys say nothing about where their
    // points are), so keys outside ±2^40 are never pruned, just measured.
    const PRUNE_KEY_LIMIT: i64 = 1 << 40;
    let key_ok = |(x, y): (i64, i64)| {
        x.unsigned_abs() < PRUNE_KEY_LIMIT as u64 && y.unsigned_abs() < PRUNE_KEY_LIMIT as u64
    };
    let orphan_key_ok = key_ok(cell_key);
    let lower_bound_sq = |key: (i64, i64)| -> f64 {
        if !(orphan_key_ok && key_ok(key)) {
            return 0.0;
        }
        // Both keys are within ±2^40, so the difference cannot overflow and
        // `r - 1 < 2^41` converts to `f64` exactly.
        let r = (key.0 - cell_key.0).abs().max((key.1 - cell_key.1).abs());
        let lb = (r - 1) as f64 * cell_size;
        lb * lb
    };

    let mut best: Option<(usize, f64)> = None;
    for (&key, &w) in present {
        if let Some((_, bd)) = best {
            // Strictly greater: a cell that could tie still competes on
            // `Priority`. A NaN bound (infinite cell size) never skips.
            if lower_bound_sq(key) > bd {
                continue;
            }
        }
        let d = dist_sq(w);
        match best {
            Some((b, bd)) if !beats(w, d, b, bd) => {}
            _ => best = Some((w, d)),
        }
    }
    best.expect("present is non-empty").0
}

#[cfg(test)]
mod tests {
    use super::super::assign::AssignFeature;

    /// Array-of-structs fixture → the column-major table the engine takes
    /// (#543). Tests build small `Vec<AssignFeature>`/array fixtures; the
    /// pipeline fills a [`FeatureTable`] directly from the scan, so this
    /// conversion exists only here.
    fn table(feats: &[AssignFeature]) -> FeatureTable {
        feats.iter().collect()
    }
    use super::*;
    use crate::overview::assign::{assign_levels, Priority, SortDirection};

    fn gsd(z: u32) -> f64 {
        40_075_016.69 / 1024.0 / 2f64.powi(z as i32)
    }

    fn point(index: usize, x: f64, y: f64) -> AssignFeature {
        AssignFeature {
            index,
            bbox: [x, y, x, y],
            kind: FeatureKind::Point,
            sort_key: None,
            entry_level: None,
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
        let assignment = assign_levels(&table(&feats), &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
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
        let assignment = assign_levels(&table(&feats), &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let vals = vec![vec![Some(10.0), Some(30.0), None, Some(20.0)]; 4];
        let ops = [
            AccumulateOp::Sum,
            AccumulateOp::Max,
            AccumulateOp::Min,
            AccumulateOp::Mean,
        ];
        let tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &vals,
            &ops,
        );

        assert_eq!(tables[0].len(), 1, "one cluster at level 0");
        let entry = tables[0].values().next().unwrap();
        assert_eq!(entry.point_count, 4);
        assert_eq!(
            entry.aggregates,
            vec![Some(60.0), Some(30.0), Some(10.0), Some(20.0)]
        );
    }

    /// #428: what the accumulate path does with the two kinds of "not an
    /// ordinary number". A NaN is skipped — it would poison every aggregate
    /// it touched and it is nodata far more often than a value — so it does
    /// not even count as a contributor. An infinity is a real summand: the
    /// canonical level copies the source value verbatim, so dropping it at
    /// the coarse levels would make the pyramid contradict itself (`max` over
    /// `{1.0, +inf}` reporting `1.0` up top and `inf` at the bottom).
    ///
    /// The NaN is filed under `None` upstream, by
    /// `convert::extract_numeric_values`; this pins what the aggregation then
    /// makes of `{None, 1.0, +inf}`.
    #[test]
    fn nan_is_skipped_but_infinity_aggregates() {
        let feats: Vec<AssignFeature> = (0..3).map(|i| point(i, i as f64 * 100.0, 0.0)).collect();
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig::default();
        let assignment = assign_levels(&table(&feats), &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        // Specs 0..3 read the source column {NaN, 1.0, +inf} as the extractor
        // hands it over; spec 4 is an all-finite {NaN, 1.0, 3.0} whose mean
        // pins the contributor COUNT, which `inf / n` cannot distinguish.
        let mut vals = vec![vec![None, Some(1.0), Some(f64::INFINITY)]; 4];
        vals.push(vec![None, Some(1.0), Some(3.0)]);
        let ops = [
            AccumulateOp::Sum,
            AccumulateOp::Max,
            AccumulateOp::Min,
            AccumulateOp::Mean,
            AccumulateOp::Mean,
        ];
        let tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &vals,
            &ops,
        );

        assert_eq!(tables[0].len(), 1, "one cluster at level 0");
        let entry = tables[0].values().next().unwrap();
        assert_eq!(
            entry.point_count, 3,
            "point_count counts ROWS, NaN value or not"
        );
        assert_eq!(
            entry.aggregates,
            vec![
                Some(f64::INFINITY), // sum: 1.0 + inf, the NaN contributes nothing
                Some(f64::INFINITY), // max
                Some(1.0),           // min: inf loses to the real value
                Some(f64::INFINITY), // mean
                Some(2.0),           // mean over 2 contributors, not 4/3 over 3
            ],
            "the NaN is not a contributor (count = 2), the infinity is"
        );
    }

    #[test]
    fn all_null_values_yield_none_aggregate() {
        let feats: Vec<AssignFeature> = (0..3).map(|i| point(i, i as f64 * 100.0, 0.0)).collect();
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig::default();
        let assignment = assign_levels(&table(&feats), &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();
        let vals = vec![vec![None, None, None]];
        let tables = build_cluster_tables(
            &table(&feats),
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
        let assignment = assign_levels(&table(&feats), &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let vals = vec![vec![Some(0.0), Some(0.0), Some(0.0), Some(8.0)]];
        let tables = build_cluster_tables(
            &table(&feats),
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
        let assignment = assign_levels(&table(&feats), &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );

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

        let tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );

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
                entry_level: None,
            },
            AssignFeature {
                index: 3,
                bbox: [0.0, 0.0, 60_000.0, 60_000.0],
                kind: FeatureKind::Line,
                sort_key: None,
                entry_level: None,
            },
        ];
        feats[0].sort_key = Some(1.0);
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig::default();
        let assignment = assign_levels(&table(&feats), &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();

        let tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
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
        let assignment = assign_levels(&table(&feats), &gsds, &cfg, Crs::Epsg3857);
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();
        assert_eq!(min_levels[3], 0, "sort-key holder wins the coarse cell");

        let tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
        assert_eq!(tables[0].get(&3).map(|e| e.point_count), Some(5));
    }

    #[test]
    fn empty_inputs_and_no_points_are_noops() {
        let gsds = [gsd(2), gsd(6)];
        let cfg = AssignConfig::default();
        let t = build_cluster_tables(&table(&[]), &[], &gsds, &cfg, Crs::Epsg3857, &[], &[]);
        assert!(t.iter().all(|m| m.is_empty()));

        let poly = AssignFeature {
            index: 0,
            bbox: [0.0, 0.0, 50_000.0, 50_000.0],
            kind: FeatureKind::Polygon,
            sort_key: None,
            entry_level: None,
        };
        let t = build_cluster_tables(&table(&[poly]), &[0], &gsds, &cfg, Crs::Epsg3857, &[], &[]);
        assert!(t.iter().all(|m| m.is_empty()));
    }

    /// §12.1 verifier: a healthy table set (including a simulated
    /// density-budget orphan) passes; the pathological zero-survivor level
    /// (every point deferred past a level) is rejected with the derived
    /// producer obligation, and a doctored table is rejected by the sum rule.
    #[test]
    fn verify_sum_invariant_pass_orphan_and_zero_survivor() {
        let cell = 4.0 * gsd(2);
        let feats = vec![
            point(0, 0.5 * cell, 0.0),
            point(1, 1.5 * cell, 0.0), // orphan cell (deferred winner)
            point(2, 4.5 * cell, 0.0),
        ];
        let gsds = [gsd(2), gsd(10)];
        let cfg = AssignConfig::default();

        // Orphan absorbed by nearest survivor: invariant holds.
        let min_levels = vec![0u8, 1, 0];
        let tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
        verify_sum_invariant(&table(&feats), &min_levels, &tables).unwrap();

        // Zero survivors at level 0 (every point deferred): build silently
        // skips the level, the verifier MUST reject it (spec §12.1).
        let all_deferred = vec![1u8, 1, 1];
        let tables = build_cluster_tables(
            &table(&feats),
            &all_deferred,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
        let err = verify_sum_invariant(&table(&feats), &all_deferred, &tables).unwrap_err();
        assert!(
            err.contains("no surviving point row"),
            "unexpected message: {err}"
        );

        // Doctored table (a lost absorption): sum rule rejects.
        let min_levels = vec![0u8, 1, 0];
        let mut tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
        tables[0].clear(); // drop the 2-point cluster entry → sum 2, not 3
        let err = verify_sum_invariant(&table(&feats), &min_levels, &tables).unwrap_err();
        assert!(err.contains("sum invariant"), "unexpected message: {err}");
    }

    /// The verifier is a no-op for point-free inputs (lines/polygons only).
    #[test]
    fn verify_sum_invariant_no_points_is_ok() {
        let poly = AssignFeature {
            index: 0,
            bbox: [0.0, 0.0, 50_000.0, 50_000.0],
            kind: FeatureKind::Polygon,
            sort_key: None,
            entry_level: None,
        };
        let gsds = [gsd(2), gsd(6)];
        let cfg = AssignConfig::default();
        let feats = vec![poly];
        let min_levels = vec![0u8];
        let tables = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
        verify_sum_invariant(&table(&feats), &min_levels, &tables).unwrap();
    }

    /// #609: two present points sharing an `AssignFeature::index` give the
    /// same table on every call (their clusters are merged under the shared
    /// key, not overwritten in `HashMap` iteration order), and the verifier
    /// rejects the table, because the writer would stamp the merged
    /// `point_count` on both rows.
    #[test]
    fn duplicate_present_index_is_deterministic_and_rejected() {
        // Level 0 cell = 100 m (point_thinning 1). Level 1 is canonical.
        let gsds = [100.0, 1.0];
        let cfg = AssignConfig {
            point_thinning: 1.0,
            ..AssignConfig::default()
        };
        // Cell A: present point at x=10 plus two absent points.
        // Cell B: present point at x=1010 plus one absent point.
        // Both present points carry index 7.
        let feats = vec![
            point(7, 10.0, 10.0),
            point(1, 20.0, 20.0),
            point(2, 30.0, 30.0),
            point(7, 1010.0, 10.0),
            point(3, 1020.0, 20.0),
        ];
        let min_levels = [0u8, 1, 1, 0, 1];
        let vals: Vec<Option<f64>> = vec![
            Some(1.0),
            Some(10.0),
            Some(100.0),
            Some(1000.0),
            Some(10000.0),
        ];
        let build = || {
            build_cluster_tables(
                &table(&feats),
                &min_levels,
                &gsds,
                &cfg,
                Crs::Epsg3857,
                std::slice::from_ref(&vals),
                &[AccumulateOp::Sum],
            )
        };
        let first = build();
        for _ in 0..200 {
            assert_eq!(
                build(),
                first,
                "cluster tables differ between identical calls"
            );
        }
        let err = verify_sum_invariant(&table(&feats), &min_levels, &first).unwrap_err();
        assert!(err.contains("share feature index 7"), "{err}");
    }

    /// #609: two present SINGLETON rows sharing an index would each be
    /// written with the merged `point_count` of 2 (a per-row sum of 4 for 2
    /// source points). The verifier rejects it.
    #[test]
    fn duplicate_present_singletons_are_rejected_by_verifier() {
        let gsds = [100.0, 1.0];
        let cfg = AssignConfig {
            point_thinning: 1.0,
            ..AssignConfig::default()
        };
        let feats = vec![point(4, 10.0, 10.0), point(4, 1010.0, 10.0)];
        let min_levels = [0u8, 0];
        let t = build_cluster_tables(
            &table(&feats),
            &min_levels,
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
        let err = verify_sum_invariant(&table(&feats), &min_levels, &t).unwrap_err();
        assert!(err.contains("share feature index 4"), "{err}");
    }

    /// #611: an orphan point with a huge finite coordinate saturates its
    /// grid key to `i64::MAX` / `i64::MIN`; resolving it must not overflow
    /// `i64`, and it joins the only present point.
    #[test]
    fn huge_coordinate_orphan_does_not_overflow() {
        let cfg = AssignConfig {
            point_thinning: 1.0,
            ..AssignConfig::default()
        };
        let gsds = [1.0, 0.5];
        for (x_present, x_orphan) in [(-10.0, 1e300), (10.0, -1e300), (-1e300, 1e300)] {
            let feats = vec![point(0, x_present, 0.0), point(1, x_orphan, 0.0)];
            let t = build_cluster_tables(
                &table(&feats),
                &[0, 1],
                &gsds,
                &cfg,
                Crs::Epsg3857,
                &[],
                &[],
            );
            assert_eq!(t[0].len(), 1, "x_present={x_present} x_orphan={x_orphan}");
            assert_eq!(t[0].get(&0).map(|e| e.point_count), Some(2));
        }
        // Saturated keys on both axes.
        let feats = vec![point(0, -5.0, -5.0), point(1, 1e300, -1e300)];
        let t = build_cluster_tables(
            &table(&feats),
            &[0, 1],
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
        assert_eq!(t[0].get(&0).map(|e| e.point_count), Some(2));
    }

    /// #610: resolving an orphan `d` cells from the nearest present cell
    /// must not cost O(d^2). At d = 8000 the old ring search did ~2.6e8
    /// hash lookups (75 s in a debug build); the scan is O(present cells),
    /// here one.
    #[test]
    fn far_orphan_resolves_without_quadratic_ring_search() {
        let cfg = AssignConfig {
            point_thinning: 1.0,
            ..AssignConfig::default()
        };
        let gsds = [1.0, 0.5];
        let d = 8_000.0;
        let feats = vec![point(0, 0.5, 0.5), point(1, d + 0.5, 0.5)];
        let t0 = std::time::Instant::now();
        let t = build_cluster_tables(
            &table(&feats),
            &[0, 1],
            &gsds,
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
        let elapsed = t0.elapsed();
        assert_eq!(t[0].get(&0).map(|e| e.point_count), Some(2));
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "orphan {d} cells away took {elapsed:?}"
        );
    }

    /// The nearest point can lie two or more Chebyshev rings beyond the
    /// nearest present cell: a point in the far corner of a ring-2 cell is
    /// farther than one on the axis of a ring-4 cell. The orphan (#12, at
    /// (0.5, 0.5)) must join #11 (distance 3.5), not #10 (distance 3.521).
    /// The ring search, and the first version of the scan that kept its
    /// "rings r and r + 1" candidate set, picked #10.
    #[test]
    fn orphan_joins_true_nearest_beyond_next_ring() {
        let cfg = AssignConfig {
            point_thinning: 1.0,
            ..AssignConfig::default()
        };
        let feats = vec![
            point(10, 2.99, 2.99), // cell (2, 2), ring 2
            point(11, 4.0, 0.5),   // cell (4, 0), ring 4
            point(12, 0.5, 0.5),   // orphan cell (0, 0)
        ];
        let t = build_cluster_tables(
            &table(&feats),
            &[0, 0, 1],
            &[1.0, 0.5],
            &cfg,
            Crs::Epsg3857,
            &[],
            &[],
        );
        assert_eq!(t[0].len(), 1, "{:?}", t[0]);
        assert_eq!(t[0].get(&11).map(|e| e.point_count), Some(2));
    }

    /// Of two outer candidates that both beat the nearest ring's best, the
    /// nearer wins whatever its ring order. The pre-#610 ring search
    /// compared ring-`r + 1` cells against the ring-`r` best only, so here it
    /// picked the LAST beater, cell (2, 0), over the nearer cell (2, -1).
    #[test]
    fn nearest_present_picks_nearest_of_outer_ring_candidates() {
        let features = vec![
            point(0, 1.99, 1.99), // cell (1, 1), ring 1, d² ≈ 4.44
            point(1, 2.0, -0.01), // cell (2, -1), ring 2, d² ≈ 2.51
            point(2, 2.2, 0.5),   // cell (2, 0), ring 2, d² = 2.89
        ];
        let present: HashMap<(i64, i64), usize> = [((1, 1), 0), ((2, -1), 1), ((2, 0), 2)]
            .into_iter()
            .collect();
        let got = nearest_present(
            (0, 0),
            &present,
            &table(&features),
            SortDirection::Desc,
            1.0,
        );
        assert_eq!(got, 1);
    }

    /// A present feature with a NaN center (reachable through the public API
    /// only; `scan_feature` rejects non-finite coordinates) is treated as
    /// infinitely far, so the comparison stays a strict total order and the
    /// answer does not depend on `HashMap` iteration order. Each map built
    /// below gets a fresh `RandomState`, so the 200 runs see many orders.
    #[test]
    fn nan_center_does_not_make_answer_order_dependent() {
        let features = vec![
            point(1, f64::NAN, 0.5), // cell (1, 1) by key, NaN center
            point(2, 1.9, 0.5),      // cell (1, 0), d² = 1.96
            point(3, -0.2, 0.5),     // cell (-1, 0), d² = 0.49
        ];
        let entries = [((1i64, 1i64), 0usize), ((1, 0), 1), ((-1, 0), 2)];
        for _ in 0..200 {
            let present: HashMap<(i64, i64), usize> = entries.iter().copied().collect();
            let got = nearest_present(
                (0, 0),
                &present,
                &table(&features),
                SortDirection::Desc,
                1.0,
            );
            assert_eq!(got, 2);
        }
    }

    /// Deterministic generator for the randomized comparisons below
    /// (splitmix64 over an LCG state).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, m: u64) -> u64 {
            self.next() % m
        }

        fn unit(&mut self) -> f64 {
            (self.next() >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Exhaustive reference: the minimum over EVERY present cell winner of
    /// (squared distance to the orphan cell center, `Priority`, position).
    fn nearest_present_exhaustive(
        cell_key: (i64, i64),
        present: &HashMap<(i64, i64), usize>,
        features: &FeatureTable,
        dir: SortDirection,
        cell_size: f64,
    ) -> usize {
        let center = (
            (cell_key.0 as f64 + 0.5) * cell_size,
            (cell_key.1 as f64 + 0.5) * cell_size,
        );
        let d = |p: usize| {
            let (x, y) = features.center(p);
            let (dx, dy) = (x - center.0, y - center.1);
            let d = dx * dx + dy * dy;
            if d.is_nan() {
                f64::INFINITY
            } else {
                d
            }
        };
        let better = |a: usize, b: usize| {
            let (da, db) = (d(a), d(b));
            if da != db {
                return da < db;
            }
            let (pa, pb) = (
                Priority::new(features.row(a), dir),
                Priority::new(features.row(b), dir),
            );
            if pa.beats(&pb) {
                true
            } else if pb.beats(&pa) {
                false
            } else {
                a < b
            }
        };
        let mut all: Vec<usize> = present.values().copied().collect();
        all.sort_unstable();
        all.into_iter()
            .reduce(|a, b| if better(b, a) { b } else { a })
            .expect("present is non-empty")
    }

    /// `nearest_present` agrees with the exhaustive scan on random layouts:
    /// points anywhere inside their cells (and, every third layout, on cell
    /// centers, for exact distance ties), spans up to 41 cells with up to 20
    /// present cells, grid bases near 0, -1e6 and ±2^40, sort keys from
    /// {None, 0, 1}, both sort directions, and (every fifth layout) indices
    /// from {0, 1, 2} so `Priority` ties fully and the position decides.
    /// The mismatch count must be 0.
    #[test]
    fn nearest_present_matches_exhaustive_scan() {
        let mut rng = Rng(12_345);
        let (mut checked, mut mismatches) = (0usize, 0usize);
        let mut first: Option<String> = None;
        for trial in 0..20_000u32 {
            let lattice = trial % 3 == 0;
            let dup = trial % 5 == 0;
            let base: (i64, i64) = match trial % 4 {
                0 => (0, 0),
                1 => (-1_000_000, 777),
                2 => (-37, -91),
                _ => (1 << 40, -(1 << 40)),
            };
            let dir = if trial % 2 == 0 {
                SortDirection::Desc
            } else {
                SortDirection::Asc
            };
            let n = 1 + rng.below(20) as usize;
            let span = 2 + rng.below(40) as i64;
            let mut features = Vec::new();
            let mut present: HashMap<(i64, i64), usize> = HashMap::new();
            for i in 0..n {
                let cx = base.0 + rng.below(span as u64) as i64 - span / 2;
                let cy = base.1 + rng.below(span as u64) as i64 - span / 2;
                if present.contains_key(&(cx, cy)) {
                    continue;
                }
                let (ox, oy) = if lattice {
                    (0.5, 0.5)
                } else {
                    (rng.unit(), rng.unit())
                };
                let index = if dup { rng.below(3) as usize } else { 1000 - i };
                let mut f = point(index, cx as f64 + ox, cy as f64 + oy);
                f.sort_key = match rng.below(3) {
                    0 => None,
                    1 => Some(0.0),
                    _ => Some(1.0),
                };
                present.insert((cx, cy), features.len());
                features.push(f);
            }
            for _ in 0..4 {
                let key = (
                    base.0 + rng.below(span as u64 + 20) as i64 - span / 2 - 10,
                    base.1 + rng.below(span as u64 + 20) as i64 - span / 2 - 10,
                );
                if present.contains_key(&key) {
                    continue;
                }
                checked += 1;
                let want = nearest_present_exhaustive(key, &present, &table(&features), dir, 1.0);
                let got = nearest_present(key, &present, &table(&features), dir, 1.0);
                if got != want {
                    mismatches += 1;
                    first.get_or_insert_with(|| {
                        format!("orphan {key:?}: got {got}, want {want}, present {present:?}")
                    });
                }
            }
        }
        assert!(checked > 50_000, "only {checked} orphans checked");
        assert_eq!(
            mismatches, 0,
            "{mismatches}/{checked} mismatches; first: {first:?}"
        );
    }

    /// Keys at or near `i64::MAX` / `i64::MIN` on either axis (the #611
    /// saturation case): no overflow, and the same answer as the exhaustive
    /// scan. Distances there are huge but finite in `f64`.
    #[test]
    fn nearest_present_saturated_keys_match_exhaustive_scan() {
        let mut rng = Rng(999);
        let mut checked = 0usize;
        let pick = |rng: &mut Rng| -> i64 {
            match rng.below(6) {
                0 => i64::MAX,
                1 => i64::MIN,
                2 => i64::MAX - rng.below(3) as i64,
                3 => i64::MIN + rng.below(3) as i64,
                4 => rng.below(7) as i64 - 3,
                _ => -(rng.below(7) as i64),
            }
        };
        for _ in 0..20_000 {
            let mut features = Vec::new();
            let mut present: HashMap<(i64, i64), usize> = HashMap::new();
            for i in 0..=rng.below(6) {
                let k = (pick(&mut rng), pick(&mut rng));
                if present.contains_key(&k) {
                    continue;
                }
                let mut f = point(50 - i as usize, k.0 as f64 + 0.5, k.1 as f64 + 0.5);
                f.sort_key = Some(rng.below(2) as f64);
                present.insert(k, features.len());
                features.push(f);
            }
            let key = (pick(&mut rng), pick(&mut rng));
            if present.contains_key(&key) {
                continue;
            }
            let want = nearest_present_exhaustive(
                key,
                &present,
                &table(&features),
                SortDirection::Desc,
                1.0,
            );
            let got = nearest_present(key, &present, &table(&features), SortDirection::Desc, 1.0);
            assert_eq!(got, want, "orphan {key:?} present {present:?}");
            checked += 1;
        }
        assert!(checked > 10_000, "only {checked} orphans checked");
    }
}
