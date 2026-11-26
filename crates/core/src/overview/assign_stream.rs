//! Streaming level assignment (#928 prototype).
//!
//! Today's assignment holds the pass-1 [`FeatureTable`] (33–42 B/row) for the
//! whole input and runs the cell-winner passes over it afterwards. For a run
//! where every row's level is local — polygons that never contest a cell, and
//! lines that contest cells only against each other — the same answer can be
//! computed while pass 1 reads, keeping only the per-row byte (`min_levels`),
//! the per-cell incumbents of the line grids, and the accumulator's patches.
//!
//! Byte identity with [`super::assign::assign_levels_bounded`] rests on:
//! - the per-level placement is the same [`place_feature`];
//! - a cell's contests run in ascending row order, as the table's do, and the
//!   contest is the same strict [`Priority::beats`];
//! - the fold takes the coarsest level a row wins, as `fold_wave_winners`;
//! - the accumulator is the same [`LevelAccumulator`], fed in row order.
//!
//! The path is chosen before the scan ([`stream_path`]) and never switched:
//! rows already folded into `min_levels` cannot be recovered.

use std::collections::HashMap;

use super::accumulate::{log_floored_zooms, LevelAccumulator};
use super::assign::{
    gsd_to_coord_units, place_feature, AssignConfig, CellKey, FeatureKind, FeatureTable, Placement,
    Priority, SortDirection,
};
use super::convert::{ConvertError, ConvertOptions};
use super::level::Crs;
use super::simplify::Representation;

/// The hidden prototype switch: `TYLERTOO_STREAM_ASSIGN=1`.
pub(super) const STREAM_ASSIGN_ENV: &str = "TYLERTOO_STREAM_ASSIGN";

#[cfg(test)]
thread_local! {
    /// Test seam: force the switch on this thread (`None` = read the env).
    static FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    /// Test seam: the path the last pass 1 on this thread took.
    static LAST_PATH: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Run `f` with the switch forced to `on` on this thread.
#[cfg(test)]
pub(super) fn with_stream_assign<R>(on: bool, f: impl FnOnce() -> R) -> R {
    struct Reset(Option<bool>);
    impl Drop for Reset {
        fn drop(&mut self) {
            FORCE.set(self.0);
        }
    }
    let _reset = Reset(FORCE.replace(Some(on)));
    f()
}

/// Whether the last pass 1 on this thread streamed its assignment.
#[cfg(test)]
pub(super) fn last_path_streamed() -> Option<bool> {
    LAST_PATH.get()
}

#[cfg(test)]
pub(super) fn record_path(streamed: bool) {
    LAST_PATH.set(Some(streamed));
}
#[cfg(not(test))]
pub(super) fn record_path(_streamed: bool) {}

/// Whether the switch is on.
pub(super) fn requested() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCE.get() {
        return on;
    }
    std::env::var(STREAM_ASSIGN_ENV).is_ok_and(|v| v == "1")
}

/// The geometry kinds the footers declare, when they are polygons and/or
/// lines only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct KindSet {
    pub(super) polygons: bool,
    pub(super) lines: bool,
}

impl KindSet {
    fn has(self, kind: FeatureKind) -> bool {
        match kind {
            FeatureKind::Polygon => self.polygons,
            FeatureKind::Line => self.lines,
            FeatureKind::Point => false,
        }
    }
}

/// Fold every part's declared `geometry_types` into one [`KindSet`]. `None`
/// when a part declares nothing, or declares a point or a collection.
pub(super) fn declared_kinds<I>(parts: I) -> Option<KindSet>
where
    I: IntoIterator<Item = Option<Vec<String>>>,
{
    let mut set = KindSet::default();
    for types in parts {
        let types = types?;
        if types.is_empty() {
            return None;
        }
        for t in &types {
            let base = t
                .trim_end_matches(" ZM")
                .trim_end_matches(" Z")
                .trim_end_matches(" M");
            match base {
                "Polygon" | "MultiPolygon" => set.polygons = true,
                "LineString" | "MultiLineString" => set.lines = true,
                _ => return None,
            }
        }
    }
    (set.polygons || set.lines).then_some(set)
}

/// What [`stream_path`] reads besides the options.
pub(super) struct StreamGate<'a> {
    pub(super) options: &'a ConvertOptions,
    pub(super) declared: Option<KindSet>,
    /// Whether the ranking plan is the size fallback (no sort key, no
    /// auto-rank candidate column).
    pub(super) size_fallback_ranking: bool,
    pub(super) accumulate_columns: bool,
    /// Whether pass 1 collects shared-node pins (#802).
    pub(super) shared_nodes: bool,
    pub(super) level_reprs: &'a [Representation],
}

/// The v1 scope: the declared kinds when every row's level can be decided
/// in row order, otherwise the reason the table path runs.
pub(super) fn stream_path(gate: &StreamGate<'_>) -> Result<KindSet, &'static str> {
    let o = gate.options;
    let kinds = gate
        .declared
        .ok_or("the footers do not declare polygon/line-only geometry types")?;
    if o.cluster || gate.accumulate_columns || o.cluster_categories.is_some() {
        return Err("clustering is on");
    }
    if o.coalesce_lines && kinds.lines {
        return Err("line coalescing is on");
    }
    if gate.shared_nodes {
        return Err("shared-node collection is on");
    }
    if o.entry_zoom.is_some() {
        return Err("an entry-zoom ladder is set");
    }
    if kinds.polygons && o.assign.polygon_thinning != 0.0 {
        return Err("polygon thinning is on");
    }
    if kinds.polygons && gate.level_reprs.contains(&Representation::Point) {
        return Err("a point representation band is set");
    }
    if kinds.lines && !gate.size_fallback_ranking {
        return Err("a sort key or auto-ranking candidate may rank lines");
    }
    let budget = &o.density;
    if budget.enabled
        && ((budget.kinds.polygons && kinds.polygons) || (budget.kinds.lines && kinds.lines))
    {
        return Err("the density budget applies to polygons or lines");
    }
    Ok(kinds)
}

/// A line grid's incumbent. Its sort key is always missing: the gate
/// admits lines only under the size fallback, and keys resolve after the
/// scan anyway.
#[derive(Clone, Copy)]
struct Incumbent {
    diag_sq: f64,
    index: usize,
}

impl Incumbent {
    fn priority(self, dir: SortDirection) -> Priority {
        Priority::from_parts(f64::NAN, self.diag_sq, self.index, dir)
    }
}

/// Bytes of the resident assignment state, measured from capacities.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct StreamMemory {
    pub(super) min_levels: u64,
    pub(super) kinds: u64,
    pub(super) carriers: u64,
    pub(super) cells: usize,
    pub(super) cell_maps: u64,
    pub(super) patches: usize,
    pub(super) patch_maps: u64,
}

/// The streaming assignment's result, row-indexed like `WinnerTables`.
pub(super) struct StreamAssigned {
    pub(super) min_levels: Vec<u8>,
    pub(super) kinds: Option<Vec<FeatureKind>>,
    pub(super) carriers: Vec<Vec<usize>>,
    pub(super) features: usize,
    /// `(points, lines, polygons)`.
    pub(super) kind_counts: (usize, usize, usize),
    pub(super) memory: StreamMemory,
}

/// Level assignment fed one scan chunk at a time, in ascending row order.
pub(super) struct StreamAssigner {
    allowed: KindSet,
    config: AssignConfig,
    finest: u8,
    /// Per coarse level (all but the finest): GSD in coordinate units.
    gsd_units: Vec<f64>,
    reprs: Vec<Representation>,
    grids: Vec<HashMap<CellKey, Incumbent>>,
    /// Per planned level.
    accs: Vec<LevelAccumulator>,
    any_acc: bool,
    min_levels: Vec<u8>,
    kinds: Option<Vec<FeatureKind>>,
    features: usize,
    kind_counts: (usize, usize, usize),
}

impl StreamAssigner {
    /// The assigner for `options`' resolved plan; `rows` pre-sizes the
    /// row-indexed tables.
    pub(super) fn new(
        allowed: KindSet,
        options: &ConvertOptions,
        level_specs: &[(f64, Option<u8>)],
        level_reprs: &[Representation],
        crs: Crs,
        rows: usize,
    ) -> Self {
        let level_gsds: Vec<f64> = level_specs.iter().map(|(g, _)| *g).collect();
        let finest = level_gsds.len().saturating_sub(1);
        let accs: Vec<LevelAccumulator> =
            super::stream::accumulate_levels(options, level_specs, level_reprs)
                .iter()
                .map(|l| LevelAccumulator::new(l, crs))
                .collect();
        let want_kinds = options.coalesce_lines || options.save_plan.is_some();
        let mut min_levels = Vec::new();
        let _ = min_levels.try_reserve_exact(rows);
        let kinds = want_kinds.then(|| {
            let mut k = Vec::new();
            let _ = k.try_reserve_exact(rows);
            k
        });
        Self {
            allowed,
            config: options.assign,
            finest: finest.min(usize::from(u8::MAX)) as u8,
            gsd_units: level_gsds[..finest]
                .iter()
                .map(|&g| gsd_to_coord_units(g, crs))
                .collect(),
            reprs: (0..finest)
                .map(|l| level_reprs.get(l).copied().unwrap_or_default())
                .collect(),
            grids: (0..finest).map(|_| HashMap::new()).collect(),
            any_acc: accs.iter().any(LevelAccumulator::is_active),
            accs,
            min_levels,
            kinds,
            features: 0,
            kind_counts: (0, 0, 0),
        }
    }

    /// Features assigned so far.
    pub(super) fn features(&self) -> usize {
        self.features
    }

    /// Assign one scan chunk: `chunk` holds chunk-local indices, `areas` is
    /// parallel to it (empty unless the accumulator is on), and the chunk
    /// covers rows `base..base + rows`.
    ///
    /// # Errors
    ///
    /// A row of a kind the footers did not declare: the streamed path cannot
    /// fall back once rows are folded.
    pub(super) fn push_chunk(
        &mut self,
        chunk: &FeatureTable,
        areas: &[f32],
        base: usize,
        rows: usize,
    ) -> Result<(), ConvertError> {
        let end = base + rows;
        self.min_levels.resize(end, super::stream::UNASSIGNED_LEVEL);
        if let Some(k) = &mut self.kinds {
            k.resize(end, FeatureKind::Point);
        }
        let dir = self.config.sort_direction;
        for pos in 0..chunk.len() {
            let mut row = chunk.row(pos);
            row.index += base;
            if !self.allowed.has(row.kind) {
                return Err(undeclared(row.index, row.kind));
            }
            debug_assert!(!row.sort_key.is_finite() && row.entry_level.is_none());
            match row.kind {
                FeatureKind::Point => self.kind_counts.0 += 1,
                FeatureKind::Line => self.kind_counts.1 += 1,
                FeatureKind::Polygon => self.kind_counts.2 += 1,
            }
            self.features += 1;
            let mut ml = self.finest;
            for li in 0..self.gsd_units.len() {
                match place_feature(row, &self.config, self.gsd_units[li], self.reprs[li]) {
                    Placement::Skip => {}
                    Placement::Wins => ml = ml.min(li as u8),
                    Placement::Cell(key) => {
                        // A polygon's level must be final before the
                        // accumulator reads it; the gate rules this out.
                        if row.kind == FeatureKind::Polygon {
                            return Err(undeclared(row.index, row.kind));
                        }
                        let challenger = Incumbent {
                            diag_sq: row.diag_sq,
                            index: row.index,
                        };
                        self.grids[li]
                            .entry(key)
                            .and_modify(|slot| {
                                if challenger.priority(dir).beats(&slot.priority(dir)) {
                                    *slot = challenger;
                                }
                            })
                            .or_insert(challenger);
                    }
                }
            }
            self.min_levels[row.index] = ml;
            if let Some(k) = &mut self.kinds {
                k[row.index] = row.kind;
            }
            if self.any_acc && row.kind == FeatureKind::Polygon {
                let area = areas.get(pos).copied().unwrap_or(0.0);
                for (li, acc) in self.accs.iter_mut().enumerate() {
                    if usize::from(ml) > li {
                        acc.add(row.index, row.center, area);
                    }
                }
            }
        }
        Ok(())
    }

    /// Fold the line grids' winners and close the accumulator.
    pub(super) fn finish(self, num_rows: usize) -> StreamAssigned {
        let Self {
            grids,
            accs,
            mut min_levels,
            kinds,
            features,
            kind_counts,
            ..
        } = self;
        let mut memory = StreamMemory::default();
        let slot = (std::mem::size_of::<(CellKey, Incumbent)>() + 1) as u64;
        for grid in &grids {
            memory.cells += grid.len();
            memory.cell_maps += grid.capacity() as u64 * slot;
        }
        for acc in &accs {
            let (n, bytes) = acc.patch_map_bytes();
            memory.patches += n;
            memory.patch_maps += bytes;
        }
        min_levels.resize(num_rows, super::stream::UNASSIGNED_LEVEL);
        // `fold_wave_winners`: a row takes the coarsest level it wins.
        for (li, grid) in grids.into_iter().enumerate() {
            let level = li as u8;
            for inc in grid.into_values() {
                let slot = &mut min_levels[inc.index];
                if level < *slot {
                    *slot = level;
                }
            }
        }
        let kinds = kinds.map(|mut k| {
            k.resize(num_rows, FeatureKind::Point);
            k
        });
        let per_level: Vec<(Vec<usize>, Option<f64>)> =
            accs.into_iter().map(LevelAccumulator::finish).collect();
        let floored: Vec<f64> = per_level.iter().filter_map(|(_, z)| *z).collect();
        log_floored_zooms(&floored);
        let carriers: Vec<Vec<usize>> = per_level.into_iter().map(|(c, _)| c).collect();
        memory.min_levels = min_levels.capacity() as u64;
        memory.kinds = kinds.as_ref().map_or(0, |k| k.capacity() as u64);
        memory.carriers = carriers
            .iter()
            .map(|c| (c.capacity() * std::mem::size_of::<usize>()) as u64)
            .sum();
        StreamAssigned {
            min_levels,
            kinds,
            carriers,
            features,
            kind_counts,
            memory,
        }
    }
}

fn undeclared(row: usize, kind: FeatureKind) -> ConvertError {
    // shortcut: an I/O-classed error, not a ConvertError variant of its own;
    // a merge would add one with its ErrorKind.
    ConvertError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "{STREAM_ASSIGN_ENV}: row {row} is a {kind:?}, which the input's \
             geometry_types do not allow for streamed assignment; rerun without {STREAM_ASSIGN_ENV}"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eligible() -> ConvertOptions {
        ConvertOptions {
            coalesce_lines: false,
            ..Default::default()
        }
    }

    fn gate(options: &ConvertOptions, declared: Option<KindSet>) -> Result<KindSet, &'static str> {
        stream_path(&StreamGate {
            options,
            declared,
            size_fallback_ranking: true,
            accumulate_columns: false,
            shared_nodes: false,
            level_reprs: &[],
        })
    }

    const BOTH: KindSet = KindSet {
        polygons: true,
        lines: true,
    };
    const POLYS: KindSet = KindSet {
        polygons: true,
        lines: false,
    };

    #[test]
    fn declared_kinds_accept_polygons_and_lines_only() {
        let types = |t: &[&str]| Some(t.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
        assert_eq!(
            declared_kinds([
                types(&["MultiPolygon", "Polygon Z"]),
                types(&["LineString"])
            ]),
            Some(BOTH)
        );
        assert_eq!(declared_kinds([types(&["Polygon"])]), Some(POLYS));
        assert_eq!(declared_kinds([types(&["Polygon", "Point"])]), None);
        assert_eq!(declared_kinds([types(&["GeometryCollection"])]), None);
        assert_eq!(declared_kinds([types(&[])]), None, "unknown kinds");
        assert_eq!(
            declared_kinds([types(&["Polygon"]), None]),
            None,
            "a part declares none"
        );
    }

    #[test]
    fn the_eligible_config_streams() {
        assert_eq!(gate(&eligible(), Some(BOTH)), Ok(BOTH));
        // Coalescing only matters with lines.
        assert_eq!(gate(&ConvertOptions::default(), Some(POLYS)), Ok(POLYS));
    }

    #[test]
    fn each_gate_falls_back() {
        let base = eligible();
        let mut cases: Vec<(ConvertOptions, Option<KindSet>)> = vec![
            (base.clone(), None),
            (
                ConvertOptions {
                    cluster: true,
                    ..base.clone()
                },
                Some(BOTH),
            ),
            (
                ConvertOptions {
                    coalesce_lines: true,
                    ..base.clone()
                },
                Some(BOTH),
            ),
            (
                ConvertOptions {
                    entry_zoom: Some(super::super::ladder::EntryZoomSpec::new(
                        "rank",
                        super::super::ladder::EntryZoomKind::Explicit(vec![(1.0, 3)]),
                    )),
                    ..base.clone()
                },
                Some(POLYS),
            ),
        ];
        let mut thinned = base.clone();
        thinned.assign.polygon_thinning = 1.0;
        cases.push((thinned, Some(POLYS)));
        let mut budget_polys = base.clone();
        budget_polys.density.kinds.polygons = true;
        cases.push((budget_polys, Some(POLYS)));
        let mut budget_lines = base.clone();
        budget_lines.density.kinds.lines = true;
        cases.push((budget_lines, Some(BOTH)));
        for (i, (options, declared)) in cases.iter().enumerate() {
            assert!(gate(options, *declared).is_err(), "case {i} must fall back");
        }
        let with = |f: &dyn Fn(&mut StreamGate<'_>)| {
            let mut g = StreamGate {
                options: &base,
                declared: Some(BOTH),
                size_fallback_ranking: true,
                accumulate_columns: false,
                shared_nodes: false,
                level_reprs: &[],
            };
            f(&mut g);
            stream_path(&g)
        };
        assert!(
            with(&|g| g.size_fallback_ranking = false).is_err(),
            "ranked lines"
        );
        assert!(
            with(&|g| g.accumulate_columns = true).is_err(),
            "accumulate"
        );
        assert!(with(&|g| g.shared_nodes = true).is_err(), "shared nodes");
        assert!(
            with(&|g| g.level_reprs = &[Representation::Point]).is_err(),
            "point band"
        );
        // The ranking gate is about lines: polygons never contest a cell.
        assert!(with(&|g| {
            g.declared = Some(POLYS);
            g.size_fallback_ranking = false;
        })
        .is_ok());
    }
}
