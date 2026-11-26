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

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{RTree, AABB};

use super::accumulate::{log_floored_zooms, CarrierRow, LevelAccumulator};
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

/// Bytes of the resident assignment state, measured from capacities. The
/// maps never shrink, so their capacity at the end is their peak.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct StreamMemory {
    pub(super) min_levels: u64,
    pub(super) kinds: u64,
    pub(super) carriers: u64,
    /// Peak live line cells, and the cell maps' bytes.
    pub(super) cells: usize,
    pub(super) cell_maps: u64,
    /// Peak live carrier patches, and the patch maps' bytes.
    pub(super) patches: usize,
    pub(super) patch_maps: u64,
    /// Peak bytes of the eviction schedule, plus its row-group index.
    pub(super) eviction: u64,
}

/// One row group of the read stream: its row count and footer bounds
/// (`[xmin, ymin, xmax, ymax]` in the coordinates the scan sees), `None`
/// when the footer has none the eviction can trust.
#[derive(Debug, Clone, Copy)]
pub(super) struct GroupSpan {
    pub(super) rows: usize,
    pub(super) bounds: Option<[f64; 4]>,
}

/// A cell or patch scheduled to close after a row group.
#[derive(Clone, Copy)]
enum Due {
    Cell(u8, CellKey),
    Patch(u8, (i64, i64)),
}

/// The conservative eviction contract (#928 design step 4). A cell or patch
/// closes once every row group that could hold a row landing in it has been
/// read: the last group whose footer bounds intersect it, grown by one cell
/// on every side, or any later group without bounds. Each row's centre is
/// checked against its group's bounds, so a footer that lies stops the run
/// instead of moving a winner. Nothing here depends on grid nesting, on the
/// row order, or on enumerating the cells a bbox covers.
struct Eviction {
    /// Cumulative row end (exclusive) of each group, in read order.
    ends: Vec<usize>,
    /// Each group's bounds, grown by the float tolerance.
    bounds: Vec<Option<[f64; 4]>>,
    tree: RTree<GeomWithData<Rectangle<[f64; 2]>, usize>>,
    /// `last_unbounded[g]`: the last group at or after `g` without bounds.
    last_unbounded: Vec<Option<usize>>,
    /// The group the stream is in.
    current: usize,
    due: Vec<Vec<Due>>,
    live: usize,
    peak_live: usize,
}

impl Eviction {
    /// `None` when the spans do not add up to the stream's rows, or no group
    /// has bounds (the eviction then stays off and every cell closes at the
    /// end).
    fn new(spans: &[GroupSpan], rows: usize) -> Option<Self> {
        let mut ends = Vec::with_capacity(spans.len());
        let mut end = 0usize;
        for s in spans {
            end += s.rows;
            ends.push(end);
        }
        // With no bounds anywhere nothing could close before the end, and
        // the schedule would be pure cost.
        if end != rows || spans.iter().all(|s| s.bounds.is_none()) {
            return None;
        }
        // Covering bboxes are often f32, rounded either way: grow by a
        // relative 1e-6 so a correct footer never trips the check.
        let grow = |v: f64| 1e-6 * v.abs().max(1.0);
        let bounds: Vec<Option<[f64; 4]>> = spans
            .iter()
            .map(|s| {
                s.bounds
                    .filter(|b| b.iter().all(|v| v.is_finite()) && b[0] <= b[2] && b[1] <= b[3])
                    .map(|[x0, y0, x1, y1]| {
                        [x0 - grow(x0), y0 - grow(y0), x1 + grow(x1), y1 + grow(y1)]
                    })
            })
            .collect();
        let tree = RTree::bulk_load(
            bounds
                .iter()
                .enumerate()
                .filter_map(|(g, b)| {
                    b.map(|[x0, y0, x1, y1]| {
                        GeomWithData::new(Rectangle::from_corners([x0, y0], [x1, y1]), g)
                    })
                })
                .collect(),
        );
        let mut last_unbounded = vec![None; bounds.len()];
        let mut last = None;
        for g in (0..bounds.len()).rev() {
            if bounds[g].is_none() && last.is_none() {
                last = Some(g);
            }
            last_unbounded[g] = last;
        }
        Some(Self {
            ends,
            due: vec![Vec::new(); bounds.len()],
            bounds,
            tree,
            last_unbounded,
            current: 0,
            live: 0,
            peak_live: 0,
        })
    }

    /// Move to `row`'s group, returning the groups finished on the way.
    fn advance(&mut self, row: usize) -> std::ops::Range<usize> {
        let from = self.current;
        while self.current < self.ends.len() && row >= self.ends[self.current] {
            self.current += 1;
        }
        from..self.current
    }

    /// Whether `center` lies inside the current group's footer bounds.
    fn contains(&self, (x, y): (f64, f64)) -> bool {
        match self.bounds.get(self.current) {
            Some(Some([x0, y0, x1, y1])) => x >= *x0 && x <= *x1 && y >= *y0 && y <= *y1,
            _ => true,
        }
    }

    /// Schedule `due`, covering `rect`, to close after the last group that
    /// can still reach it.
    fn schedule(&mut self, due: Due, [x0, y0, x1, y1]: [f64; 4]) {
        let current = self.current.min(self.ends.len() - 1);
        let envelope = AABB::from_corners([x0, y0], [x1, y1]);
        let last = self
            .tree
            .locate_in_envelope_intersecting(&envelope)
            .map(|g| g.data)
            .chain(self.last_unbounded[current])
            .max()
            .unwrap_or(current)
            .max(current);
        self.due[last].push(due);
        self.live += 1;
        self.peak_live = self.peak_live.max(self.live);
    }

    fn take_due(&mut self, group: usize) -> Vec<Due> {
        let due = std::mem::take(&mut self.due[group]);
        self.live -= due.len();
        due
    }

    fn peak_bytes(&self) -> u64 {
        let index = self.ends.len() * (8 + 8 + 33 + 8 + 24) + self.tree.size() * 48;
        (self.peak_live * std::mem::size_of::<Due>() + index) as u64
    }
}

/// The streaming assignment's result, row-indexed like `WinnerTables`.
pub(super) struct StreamAssigned {
    pub(super) min_levels: Vec<u8>,
    pub(super) kinds: Option<Vec<FeatureKind>>,
    pub(super) carriers: Vec<Vec<CarrierRow>>,
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
    evict: Option<Eviction>,
    peak_cells: usize,
    live_cells: usize,
    peak_patches: usize,
    live_patches: usize,
}

impl StreamAssigner {
    /// The assigner for `options`' resolved plan; `rows` pre-sizes the
    /// row-indexed tables. `spans` are the stream's row groups, for the
    /// eviction; empty keeps every cell and patch to the end.
    pub(super) fn new(
        allowed: KindSet,
        options: &ConvertOptions,
        level_specs: &[(f64, Option<u8>)],
        level_reprs: &[Representation],
        crs: Crs,
        (rows, spans): (usize, &[GroupSpan]),
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
            evict: Eviction::new(spans, rows),
            peak_cells: 0,
            live_cells: 0,
            peak_patches: 0,
            live_patches: 0,
        }
    }

    /// Whether the eviction is on (the stream's row groups are known).
    pub(super) fn evicts(&self) -> bool {
        self.evict.is_some()
    }

    /// Close everything due after the row groups `row` has moved past.
    fn advance(&mut self, row: usize) {
        let Some(evict) = &mut self.evict else {
            return;
        };
        for group in evict.advance(row) {
            for due in evict.take_due(group) {
                match due {
                    Due::Cell(li, key) => {
                        let inc = self.grids[usize::from(li)]
                            .remove(&key)
                            .expect("a scheduled cell is live");
                        // `fold_wave_winners`: the coarsest level a row wins.
                        let slot = &mut self.min_levels[inc.index];
                        *slot = (*slot).min(li);
                        self.live_cells -= 1;
                    }
                    Due::Patch(li, key) => {
                        self.accs[usize::from(li)].close_patch(key);
                        self.live_patches -= 1;
                    }
                }
            }
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
            self.advance(row.index);
            if self.evict.as_ref().is_some_and(|e| !e.contains(row.center)) {
                return Err(outside_bounds(row.index));
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
                        match self.grids[li].entry(key) {
                            Entry::Occupied(mut slot) => {
                                if challenger.priority(dir).beats(&slot.get().priority(dir)) {
                                    *slot.get_mut() = challenger;
                                }
                            }
                            Entry::Vacant(slot) => {
                                slot.insert(challenger);
                                self.live_cells += 1;
                                self.peak_cells = self.peak_cells.max(self.live_cells);
                                if let Some(evict) = &mut self.evict {
                                    let cs = self.gsd_units[li] * self.config.line_thinning;
                                    let (_, ix, iy) = key;
                                    let rect = grown_cell(ix, iy, cs);
                                    evict.schedule(Due::Cell(li as u8, key), rect);
                                }
                            }
                        }
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
                        // The gate keeps every row index within a `CarrierRow`.
                        let index = row.index as CarrierRow;
                        if let Some((ix, iy)) = acc.add(index, row.center, area) {
                            self.live_patches += 1;
                            self.peak_patches = self.peak_patches.max(self.live_patches);
                            if let (Some(evict), Some(w)) = (&mut self.evict, acc.patch_width()) {
                                evict.schedule(
                                    Due::Patch(li as u8, (ix, iy)),
                                    grown_cell(ix, iy, w),
                                );
                            }
                        }
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
            evict,
            peak_cells,
            peak_patches,
            ..
        } = self;
        let mut memory = StreamMemory {
            cells: peak_cells,
            patches: peak_patches,
            eviction: evict.as_ref().map_or(0, Eviction::peak_bytes),
            ..StreamMemory::default()
        };
        let slot = (std::mem::size_of::<(CellKey, Incumbent)>() + 1) as u64;
        for grid in &grids {
            memory.cell_maps += grid.capacity() as u64 * slot;
        }
        for acc in &accs {
            memory.patch_maps += acc.patch_map_bytes().1;
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
        let per_level: Vec<(Vec<CarrierRow>, Option<f64>)> =
            accs.into_iter().map(LevelAccumulator::finish).collect();
        let floored: Vec<f64> = per_level.iter().filter_map(|(_, z)| *z).collect();
        log_floored_zooms(&floored);
        let carriers: Vec<Vec<CarrierRow>> = per_level.into_iter().map(|(c, _)| c).collect();
        memory.min_levels = min_levels.capacity() as u64;
        memory.kinds = kinds.as_ref().map_or(0, |k| k.capacity() as u64);
        memory.carriers = carriers
            .iter()
            .map(|c| (c.capacity() * std::mem::size_of::<CarrierRow>()) as u64)
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

/// Cell `(ix, iy)` of width `w`, grown by one cell on every side so float
/// rounding in `floor(c / w)` can never place a row outside it.
fn grown_cell(ix: i64, iy: i64, w: f64) -> [f64; 4] {
    let (x, y) = (ix as f64 * w, iy as f64 * w);
    [x - w, y - w, x + 2.0 * w, y + 2.0 * w]
}

fn outside_bounds(row: usize) -> ConvertError {
    ConvertError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "{STREAM_ASSIGN_ENV}: row {row}'s bbox centre lies outside its row group's \
             footer bounds, so streamed assignment cannot evict safely; rerun without \
             {STREAM_ASSIGN_ENV}"
        ),
    ))
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

    /// A footer whose bounds do not contain a row's centre stops the run:
    /// evicting on it could close a cell a later row still contests.
    #[test]
    fn rows_outside_their_footer_bounds_stop_the_run() {
        use super::super::assign::AssignFeature;
        let options = eligible();
        let specs = options.levels.resolve(options.gsd_base).unwrap();
        let reprs = super::super::convert::level_representations(&specs, &options.representation);
        let line = |x: f64| AssignFeature {
            index: 0,
            bbox: [x, x, x + 10.0, x + 10.0],
            kind: FeatureKind::Line,
            sort_key: None,
            entry_level: None,
        };
        let spans = [GroupSpan {
            rows: 1,
            bounds: Some([0.0, 0.0, 20.0, 20.0]),
        }];
        let push = |x: f64| {
            let mut a =
                StreamAssigner::new(BOTH, &options, &specs, &reprs, Crs::Epsg3857, (1, &spans));
            assert!(a.evicts());
            a.push_chunk(&[line(x)].iter().collect(), &[], 0, 1)
        };
        assert!(push(1.0).is_ok());
        let err = push(500.0).unwrap_err().to_string();
        assert!(err.contains("outside its row group"), "{err}");
    }

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
