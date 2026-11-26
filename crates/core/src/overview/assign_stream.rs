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

use foldhash::HashMap;
use rayon::prelude::*;
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{RTree, AABB};

use super::accumulate::{log_floored_zooms, CarrierRow, LevelAccumulator};
use super::assign::{
    gsd_to_coord_units, place_feature, AssignConfig, CellKey, FeatureKind, FeatureRow,
    FeatureTable, Placement, Priority, SortDirection,
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

/// A cell or patch scheduled to close after a row group: 24 bytes, one
/// per live cell or patch.
#[derive(Clone, Copy)]
struct Due {
    level: u8,
    /// The cell key's kind byte, or [`Due::PATCH`] for a carrier patch.
    kind: u8,
    ix: i64,
    iy: i64,
}

impl Due {
    const PATCH: u8 = u8::MAX;

    fn cell(level: u8, (kind, ix, iy): CellKey) -> Self {
        Self {
            level,
            kind,
            ix,
            iy,
        }
    }

    fn patch(level: u8, (ix, iy): (i64, i64)) -> Self {
        Self {
            level,
            kind: Self::PATCH,
            ix,
            iy,
        }
    }
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
    /// Entries the due lists have capacity for, now and at their peak.
    live_cap: usize,
    peak_cap: usize,
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
            live_cap: 0,
            peak_cap: 0,
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

    /// Whether `center` lies inside the footer bounds of `row`'s group.
    fn contains_row(&self, row: usize, (x, y): (f64, f64)) -> bool {
        let group = self.ends.partition_point(|&end| end <= row);
        match self.bounds.get(group) {
            Some(Some([x0, y0, x1, y1])) => x >= *x0 && x <= *x1 && y >= *y0 && y <= *y1,
            _ => true,
        }
    }

    /// The last group that can still reach `rect`: the one to close it after.
    fn expiry(&self, [x0, y0, x1, y1]: [f64; 4]) -> usize {
        let current = self.current.min(self.ends.len() - 1);
        let envelope = AABB::from_corners([x0, y0], [x1, y1]);
        self.tree
            .locate_in_envelope_intersecting(&envelope)
            .map(|g| g.data)
            .chain(self.last_unbounded[current])
            .max()
            .unwrap_or(current)
            .max(current)
    }

    /// Schedule `due` to close after group `last` (its [`Self::expiry`]).
    fn schedule(&mut self, due: Due, last: usize) {
        let list = &mut self.due[last];
        let before = list.capacity();
        list.push(due);
        self.live_cap += list.capacity() - before;
        self.peak_cap = self.peak_cap.max(self.live_cap);
    }

    fn take_due(&mut self, group: usize) -> Vec<Due> {
        let due = std::mem::take(&mut self.due[group]);
        self.live_cap -= due.capacity();
        due
    }

    /// Peak schedule bytes: the due lists at their peak capacity, plus the
    /// per-group index (ends, bounds, the unbounded suffix, empty list
    /// heads, and the R-tree's leaves).
    fn peak_bytes(&self) -> u64 {
        let per_group = 8 + 40 + 16 + 24;
        let index = self.ends.len() * per_group + self.tree.size() * 48;
        (self.peak_cap * std::mem::size_of::<Due>() + index) as u64
    }
}

/// What one grid or accumulator opened in a segment, with each entry's
/// expiry group.
type Opened = Vec<(Due, usize)>;

/// Line grids per level: the contests of one level fan out this wide.
const GRID_SHARDS: usize = 8;

/// The shard of `key`'s grid within its level.
fn grid_shard((_, ix, iy): CellKey) -> usize {
    let h = (ix as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (iy as u64);
    (h.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 61) as usize % GRID_SHARDS
}

/// One scan chunk handed to [`StreamAssigner::push_batch`]: its table,
/// areas, first row and row count.
pub(super) type ChunkRef<'a> = (&'a FeatureTable, &'a [f32], usize, usize);

/// A placed row: the row, its area, and the coarsest level it wins.
type Placed = (FeatureRow, f32, u8);

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
    /// Per coarse level, [`GRID_SHARDS`] grids split by cell key: a cell's
    /// contests stay in one grid, in row order, so sharding changes nothing.
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
    /// `[profile]` split: placement, level contests, scheduling.
    timers: [std::time::Duration; 3],
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
            grids: (0..finest * GRID_SHARDS)
                .map(|_| HashMap::default())
                .collect(),
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
            timers: Default::default(),
        }
    }

    /// Whether the eviction is on (the stream's row groups are known).
    pub(super) fn evicts(&self) -> bool {
        self.evict.is_some()
    }

    /// Close everything due after the row groups `row` has moved past, each
    /// level's cells and patches in parallel.
    fn advance(&mut self, row: usize) {
        let Some(evict) = &mut self.evict else {
            return;
        };
        let groups = evict.advance(row);
        if groups.is_empty() {
            return;
        }
        let mut cells: Vec<Vec<Due>> = vec![Vec::new(); self.grids.len()];
        let mut patches: Vec<Vec<Due>> = vec![Vec::new(); self.gsd_units.len()];
        for group in groups {
            for due in evict.take_due(group) {
                let li = usize::from(due.level);
                if due.kind == Due::PATCH {
                    patches[li].push(due);
                } else {
                    let key = (due.kind, due.ix, due.iy);
                    cells[li * GRID_SHARDS + grid_shard(key)].push(due);
                }
            }
        }
        let (won, closed): (Vec<Vec<usize>>, usize) = rayon::join(
            || {
                self.grids
                    .par_iter_mut()
                    .zip(cells)
                    .map(|(grid, dues)| {
                        dues.iter()
                            .map(|d| {
                                grid.remove(&(d.kind, d.ix, d.iy))
                                    .expect("a scheduled cell is live")
                                    .index
                            })
                            .collect()
                    })
                    .collect()
            },
            || {
                self.accs
                    .par_iter_mut()
                    .zip(patches)
                    .map(|(acc, dues)| {
                        for d in &dues {
                            acc.close_patch((d.ix, d.iy));
                        }
                        dues.len()
                    })
                    .sum()
            },
        );
        self.live_patches -= closed;
        for (gi, won) in won.into_iter().enumerate() {
            self.live_cells -= won.len();
            // `fold_wave_winners`: the coarsest level a row wins.
            let level = (gi / GRID_SHARDS) as u8;
            for index in won {
                let slot = &mut self.min_levels[index];
                *slot = (*slot).min(level);
            }
        }
    }

    /// Features assigned so far.
    pub(super) fn features(&self) -> usize {
        self.features
    }

    /// Assign consecutive scan chunks `(chunk, areas, base, rows)`: each
    /// `chunk` holds chunk-local indices, `areas` is parallel to it (empty
    /// unless the accumulator is on), and it covers rows `base..base + rows`. The per-row placement fans out
    /// over rows; the order-dependent contests and patch sums fan out over
    /// levels, each level still reading its rows in ascending order, so
    /// tie-breaks and the dither are those of a serial pass.
    ///
    /// # Errors
    ///
    /// A row of a kind the footers did not declare, or outside its row
    /// group's bounds: the streamed path cannot fall back once rows are
    /// folded. The first such row in row order is the one reported.
    pub(super) fn push_batch(&mut self, chunks: &[ChunkRef<'_>]) -> Result<(), ConvertError> {
        let Some(&(_, _, last_base, last_rows)) = chunks.last() else {
            return Ok(());
        };
        let end = last_base + last_rows;
        self.min_levels.resize(end, super::stream::UNASSIGNED_LEVEL);
        if let Some(k) = &mut self.kinds {
            k.resize(end, FeatureKind::Point);
        }
        let t = std::time::Instant::now();
        // Each chunk writes its own slice of the row tables, in parallel.
        let first = chunks[0].2;
        let mut min_levels = std::mem::take(&mut self.min_levels);
        let mut kinds = self.kinds.take();
        let mut ml_rest = &mut min_levels[first..end];
        let mut kind_rest = kinds.as_mut().map(|k| &mut k[first..end]);
        let mut slices = Vec::with_capacity(chunks.len());
        for &(_, _, _, rows) in chunks {
            let (ml, rest) = std::mem::take(&mut ml_rest).split_at_mut(rows);
            ml_rest = rest;
            let kind = kind_rest.take().map(|k| {
                let (kind, rest) = k.split_at_mut(rows);
                kind_rest = Some(rest);
                kind
            });
            slices.push((ml, kind));
        }
        let placed: Vec<_> = chunks
            .par_iter()
            .zip(slices)
            .map(
                |(&(chunk, areas, base, _), (ml, mut kind))| -> Result<_, ConvertError> {
                    let mut out = Vec::with_capacity(chunk.len());
                    let mut counts = [0usize; 3];
                    for pos in 0..chunk.len() {
                        let mut row = chunk.row(pos);
                        let local = row.index;
                        row.index += base;
                        let level = self.place(row)?;
                        ml[local] = level;
                        if let Some(k) = kind.as_deref_mut() {
                            k[local] = row.kind;
                        }
                        counts[row.kind as usize] += 1;
                        let area = areas.get(pos).copied().unwrap_or(0.0);
                        out.push((row, area, level));
                    }
                    Ok((out, counts))
                },
            )
            .collect();
        self.min_levels = min_levels;
        self.kinds = kinds;
        let mut rows = Vec::with_capacity(placed.iter().flatten().map(|(o, _)| o.len()).sum());
        for p in placed {
            // In chunk order, so the first bad row in row order is reported.
            let (out, [points, lines, polygons]) = p?;
            self.kind_counts.0 += points;
            self.kind_counts.1 += lines;
            self.kind_counts.2 += polygons;
            rows.extend(out);
        }
        self.features += rows.len();
        self.timers[0] += t.elapsed();
        // One segment per row group, so closes still happen at group ends.
        let mut a = 0;
        while a < rows.len() {
            let t = std::time::Instant::now();
            self.advance(rows[a].0.index);
            self.timers[2] += t.elapsed();
            let group_end = self
                .evict
                .as_ref()
                .and_then(|e| e.ends.get(e.current).copied())
                .unwrap_or(usize::MAX);
            let b = a + rows[a..].partition_point(|(r, _, _)| r.index < group_end);
            self.assign_segment(&rows[a..b]);
            a = b;
        }
        Ok(())
    }

    /// The pure per-row part: the row's checks and the coarsest level it
    /// wins outright.
    fn place(&self, row: FeatureRow) -> Result<u8, ConvertError> {
        if !self.allowed.has(row.kind) {
            return Err(undeclared(row.index, row.kind));
        }
        if self
            .evict
            .as_ref()
            .is_some_and(|e| !e.contains_row(row.index, row.center))
        {
            return Err(outside_bounds(row.index));
        }
        debug_assert!(!row.sort_key.is_finite() && row.entry_level.is_none());
        let mut ml = self.finest;
        for li in 0..self.gsd_units.len() {
            match place_feature(row, &self.config, self.gsd_units[li], self.reprs[li]) {
                Placement::Skip => {}
                Placement::Wins => ml = ml.min(li as u8),
                // A polygon's level must be final before the accumulator
                // reads it; the gate rules this out.
                Placement::Cell(_) if row.kind == FeatureKind::Polygon => {
                    return Err(undeclared(row.index, row.kind));
                }
                Placement::Cell(_) => {}
            }
        }
        Ok(ml)
    }

    /// Run the cell contests and patch sums of `rows` (ascending, one row
    /// group) on every level at once, then schedule what they opened.
    fn assign_segment(&mut self, rows: &[Placed]) {
        let config = &self.config;
        let dir = config.sort_direction;
        let any_acc = self.any_acc;
        let t = std::time::Instant::now();
        let evict = self.evict.as_ref();
        // The R-tree lookups run in the tasks; only the due-list pushes stay
        // serial.
        let expiry = |rect| evict.map_or(0, |e: &Eviction| e.expiry(rect));
        let (gsd_units, reprs) = (&self.gsd_units, &self.reprs);
        let lines = self.allowed.lines;
        let (cells, patches): (Vec<Opened>, Vec<Opened>) = rayon::join(
            || {
                if !lines {
                    return Vec::new();
                }
                self.grids
                    .par_iter_mut()
                    .enumerate()
                    .map(|(gi, grid)| {
                        let (li, shard) = (gi / GRID_SHARDS, gi % GRID_SHARDS);
                        let level = li as u8;
                        let cs = gsd_units[li] * config.line_thinning;
                        let mut opened = Vec::new();
                        for &(row, _, _) in rows {
                            if row.kind != FeatureKind::Line {
                                continue;
                            }
                            let Placement::Cell(key) =
                                place_feature(row, config, gsd_units[li], reprs[li])
                            else {
                                continue;
                            };
                            if grid_shard(key) != shard {
                                continue;
                            }
                            let challenger = Incumbent {
                                diag_sq: row.diag_sq,
                                index: row.index,
                            };
                            match grid.entry(key) {
                                Entry::Occupied(mut slot) => {
                                    if challenger.priority(dir).beats(&slot.get().priority(dir)) {
                                        *slot.get_mut() = challenger;
                                    }
                                }
                                Entry::Vacant(slot) => {
                                    slot.insert(challenger);
                                    let (_, ix, iy) = key;
                                    let d = Due::cell(level, key);
                                    opened.push((d, expiry(grown_cell(ix, iy, cs))));
                                }
                            }
                        }
                        opened
                    })
                    .collect()
            },
            || {
                if !any_acc {
                    return Vec::new();
                }
                self.accs
                    .par_iter_mut()
                    .take(gsd_units.len())
                    .enumerate()
                    .map(|(li, acc)| {
                        let level = li as u8;
                        let mut opened = Vec::new();
                        let Some(w) = acc.patch_width() else {
                            return opened;
                        };
                        for &(row, area, ml) in rows {
                            if row.kind != FeatureKind::Polygon || usize::from(ml) <= li {
                                continue;
                            }
                            // The gate keeps every row index within a `CarrierRow`.
                            if let Some((ix, iy)) =
                                acc.add(row.index as CarrierRow, row.center, area)
                            {
                                let d = Due::patch(level, (ix, iy));
                                opened.push((d, expiry(grown_cell(ix, iy, w))));
                            }
                        }
                        opened
                    })
                    .collect()
            },
        );
        self.timers[1] += t.elapsed();
        let t = std::time::Instant::now();
        self.live_cells += cells.iter().map(Vec::len).sum::<usize>();
        self.live_patches += patches.iter().map(Vec::len).sum::<usize>();
        if let Some(evict) = &mut self.evict {
            for (d, last) in cells.into_iter().chain(patches).flatten() {
                evict.schedule(d, last);
            }
        }
        self.timers[2] += t.elapsed();
        // Nothing closes inside a segment, so its end is its peak.
        self.peak_cells = self.peak_cells.max(self.live_cells);
        self.peak_patches = self.peak_patches.max(self.live_patches);
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
            timers,
            ..
        } = self;
        log::debug!(
            "[profile] streamed assignment: place {:.2}s, levels {:.2}s, schedule {:.2}s",
            timers[0].as_secs_f64(),
            timers[1].as_secs_f64(),
            timers[2].as_secs_f64()
        );
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
        for (gi, grid) in grids.into_iter().enumerate() {
            let level = (gi / GRID_SHARDS) as u8;
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
            a.push_batch(&[(&[line(x)].iter().collect(), &[], 0, 1)])
        };
        assert!(push(1.0).is_ok());
        let err = push(500.0).unwrap_err().to_string();
        assert!(err.contains("outside its row group"), "{err}");
    }

    #[test]
    fn a_due_entry_is_24_bytes() {
        assert_eq!(std::mem::size_of::<Due>(), 24);
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
