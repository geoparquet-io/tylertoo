//! The merge step of the tile size valve (#745): when a tile is over its size
//! limit, merge the densest lines and polygons into the feature kept just
//! before them instead of dropping them, so a polygon coverage keeps drawing
//! its whole area.
//!
//! This is tippecanoe's `--coalesce-densest-as-needed` (`tile.cpp`,
//! `write_tile` and `find_feature_to_accumulate_onto`):
//!
//! - Each feature has a **gap**: the squared world distance from the feature
//!   before it in spatial order. Small gaps mark dense areas.
//! - A feature whose gap is below the threshold `mingap` is merged into the
//!   most recent kept feature of the same geometry type. Its geometry is
//!   appended to that feature's; the kept feature's id and attributes win.
//! - If the tile is still too big, the threshold rises to a quantile of the
//!   tile's gaps ([`next_mingap`]) and the tile is tried again.
//!
//! Merged polygons need no union here: the MVT encoder already unions the
//! parts of a multipolygon that overlap or share an edge, in tile
//! coordinates (`mvt.rs`, `resolve_part_overlaps`), which is where tippecanoe
//! unions too.
//!
//! DIVERGENCE FROM TIPPECANOE:
//! - The gap is measured to the corners of the feature's bbox, not to every
//!   vertex. Squared distance is convex, so the bbox gap is never smaller
//!   than tippecanoe's and the bias is the same everywhere; it also lets the
//!   export scan, which reads only bboxes, compute it.
//! - The feature before is the previous row **in the same row group** of the
//!   overview file, and the first row of each row group has no gap
//!   ([`GAP_NONE`]). Export reads whole row groups, but which row groups a
//!   partition reads depends on the partition, so a predecessor across a
//!   row-group boundary would make the output depend on partitioning.
//!   Tippecanoe measures against the previous feature in its global index.
//! - Members merge only within their own level band. In partitioning mode a
//!   tile holds several bands, each in its own spatial order, so the row
//!   before a band's first row belongs to another band and is spatially
//!   unrelated.
//! - The threshold is one above the quantile gap ([`next_mingap`]), so the
//!   quantile gap itself merges. Tippecanoe uses the quantile itself; on a
//!   regular grid, where every gap is equal, that never merges anything,
//!   because the test is `gap < mingap`.

use geo::{
    BoundingRect, CoordsIter, Geometry, LineString, MultiLineString, MultiPolygon, Polygon, Rect,
};
use rayon::prelude::*;

use super::{Member, SharedProps};

/// The gap of a row with no predecessor in its row group, or with no bbox.
/// Never below any threshold, so such a member is never merged.
pub(super) const GAP_NONE: u64 = u64::MAX;

/// Web Mercator world size the gap is measured in: tippecanoe's 32-bit world.
const WORLD: f64 = 4_294_967_296.0;

/// Web Mercator's latitude limit.
const MAX_LAT: f64 = 85.051_128_779_806_59;

/// A lon/lat position in Web Mercator world units, `0..2^32` on both axes,
/// `y` growing southward.
pub(super) fn world_xy(lng: f64, lat: f64) -> (f64, f64) {
    let x = (lng + 180.0) / 360.0 * WORLD;
    let phi = lat.clamp(-MAX_LAT, MAX_LAT).to_radians();
    let y = (1.0 - (phi.tan() + 1.0 / phi.cos()).ln() / std::f64::consts::PI) / 2.0 * WORLD;
    (x, y)
}

/// The gap from the point `from` (world units) to a bbox (lon/lat): the
/// largest squared distance to one of its corners, saturating below
/// [`GAP_NONE`].
pub(super) fn gap_to_rect(from: (f64, f64), rect: &Rect<f64>) -> u64 {
    #[cfg(test)]
    if rayon::current_thread_index().is_none() {
        SERIAL_GAPS.with(|n| n.set(n.get() + 1));
    }
    let (min, max) = (rect.min(), rect.max());
    let mut d2_max = 0.0_f64;
    for (lng, lat) in [
        (min.x, min.y),
        (min.x, max.y),
        (max.x, min.y),
        (max.x, max.y),
    ] {
        let (x, y) = world_xy(lng, lat);
        let (dx, dy) = (x - from.0, y - from.1);
        d2_max = d2_max.max(dx * dx + dy * dy);
    }
    // `as` saturates at `u64::MAX`; keep one below so a real gap never reads
    // as GAP_NONE.
    (d2_max as u64).min(GAP_NONE - 1)
}

#[cfg(test)]
thread_local! {
    /// Gaps this thread measured outside the Rayon pool (test counter, #817).
    static SERIAL_GAPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A rect's centre in world units: the point the next row's gap is measured
/// from (tippecanoe's index point).
fn rect_center(rect: &Rect<f64>) -> (f64, f64) {
    let c = rect.center();
    world_xy(c.x, c.y)
}

/// Assigns each row read from an overview file its gap and band, in read
/// order.
///
/// Built from the row groups being read, in read order, as `(band, rows)`.
/// Fed one geometry per row, in order, across batches; batches may straddle
/// row groups.
pub(super) struct RowGapCursor {
    groups: Vec<(u16, u64)>,
    next_group: usize,
    left: u64,
    band: u16,
    prev: Option<(f64, f64)>,
    /// Whether [`Self::batch`] measures gaps. Off, every row gets
    /// [`GAP_NONE`] and no bbox is computed.
    measure: bool,
}

impl RowGapCursor {
    pub(super) fn new(groups: Vec<(u16, u64)>) -> Self {
        Self {
            groups,
            next_group: 0,
            left: 0,
            band: 0,
            prev: None,
            measure: true,
        }
    }

    /// Whether [`Self::batch`] measures gaps (default on). Off when nothing
    /// will merge (`--size-valve drop`, or no size limit), so a read pays no
    /// bbox per row. Bands are reported either way.
    pub(super) fn measuring(mut self, measure: bool) -> Self {
        self.measure = measure;
        self
    }

    /// The next row's `(gap, band)`, given its bbox (`None` for an empty
    /// geometry, which gets [`GAP_NONE`] and leaves the predecessor as is).
    pub(super) fn next_row(&mut self, rect: Option<Rect<f64>>) -> (u64, u16) {
        self.enter_row();
        let Some(rect) = rect else {
            return (GAP_NONE, self.band);
        };
        let gap = match self.prev {
            Some(p) => gap_to_rect(p, &rect),
            None => GAP_NONE,
        };
        self.prev = Some(rect_center(&rect));
        (gap, self.band)
    }

    /// Step to the next row: into the next non-empty row group when this
    /// one is done, which forgets the predecessor. `true` when it did.
    fn enter_row(&mut self) -> bool {
        let entered = self.left == 0;
        while self.left == 0 {
            let Some(&(band, rows)) = self.groups.get(self.next_group) else {
                // More rows than the footer promised: cannot happen for a
                // reader built from that footer. Treat the rest as one more
                // row group rather than panic.
                debug_assert!(false, "RowGapCursor fed more rows than its row groups hold");
                self.left = u64::MAX;
                self.prev = None;
                break;
            };
            self.next_group += 1;
            self.left = rows;
            self.band = band;
            self.prev = None;
        }
        self.left -= 1;
        entered
    }

    /// [`Self::next_row`] over a whole batch of decoded geometries.
    ///
    /// PERF (#817): only the row-group walk is sequential. It finds each
    /// row's predecessor; the bboxes, centres and gaps, five Web Mercator
    /// projections a row, are computed in the pool. Each gap is the same
    /// `gap_to_rect` of the same predecessor centre, so the gaps are those
    /// of the row-by-row walk. Measured on the calling thread, they held the
    /// export's ordered collect stage, and with it the pool, to one core.
    pub(super) fn batch(&mut self, geoms: &[Geometry<f64>]) -> Vec<(u64, u16)> {
        if !self.measure {
            return geoms.iter().map(|_| self.next_row(None)).collect();
        }
        #[cfg(test)]
        crate::valve_counter::note(|c| c.gap_rows += geoms.len());
        let rects: Vec<Option<Rect<f64>>> = geoms.par_iter().map(|g| g.bounding_rect()).collect();
        let centres: Vec<Option<(f64, f64)>> = rects
            .par_iter()
            .map(|r| r.as_ref().map(rect_center))
            .collect();
        // Each row's band and predecessor centre, in read order.
        let mut last = self.prev;
        let walk: Vec<(Option<(f64, f64)>, u16)> = centres
            .iter()
            .map(|centre| {
                if self.enter_row() {
                    // A new row group: no predecessor.
                    last = None;
                }
                let pred = last;
                if centre.is_some() {
                    last = *centre;
                }
                (pred, self.band)
            })
            .collect();
        self.prev = last;
        rects
            .par_iter()
            .zip(walk)
            .map(|(rect, (pred, band))| match (rect, pred) {
                (Some(rect), Some(p)) => (gap_to_rect(p, rect), band),
                _ => (GAP_NONE, band),
            })
            .collect()
    }
}

/// Which members can merge into which: only lines with lines and polygons
/// with polygons. Points and geometry collections never merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeKind {
    Line,
    Polygon,
}

fn merge_kind(g: &Geometry<f64>) -> Option<MergeKind> {
    match g {
        Geometry::Line(_) | Geometry::LineString(_) | Geometry::MultiLineString(_) => {
            Some(MergeKind::Line)
        }
        Geometry::Polygon(_)
        | Geometry::MultiPolygon(_)
        | Geometry::Rect(_)
        | Geometry::Triangle(_) => Some(MergeKind::Polygon),
        Geometry::Point(_) | Geometry::MultiPoint(_) | Geometry::GeometryCollection(_) => None,
    }
}

/// A tile feature after merging: a kept member and the geometry it now
/// carries (its own, plus the parts of every member merged into it).
pub(super) struct Merged<'a> {
    pub(super) target: &'a Member,
    ordered: &'a [&'a Member],
    /// Cached polygon encoding uses only member indices. Materialize the
    /// concatenated geometry only for plain encoding (lines or fallback).
    #[expect(
        clippy::disallowed_types,
        reason = "per-feature lazy geometry shared by parallel encoders; no diagnostic globals"
    )]
    geom: std::sync::OnceLock<Geometry<f64>>,
    /// How many members were merged into this one.
    pub(super) absorbed: usize,
    /// The members `geom` is made of, as indices into the `ordered` slice
    /// given to [`coalesce_members`]: the kept member, then each merged one
    /// in the order its parts are appended (#758).
    pub(super) members: Vec<usize>,
}

impl Merged<'_> {
    pub(super) fn geometry(&self) -> &Geometry<f64> {
        if self.absorbed == 0 {
            &self.target.geom
        } else {
            self.geom.get_or_init(|| {
                concat_geometry(
                    &self.target.geom,
                    self.members[1..].iter().map(|&i| self.ordered[i]),
                )
            })
        }
    }

    /// Same count as the concatenated geometry, without deep copies just
    /// to decide whether encoding is large enough to parallelize.
    pub(super) fn coords_count(&self) -> usize {
        if self.absorbed == 0 {
            return self.target.geom.coords_count();
        }
        self.members
            .iter()
            .map(|&i| match &self.ordered[i].geom {
                Geometry::Rect(_) => 5,
                Geometry::Triangle(_) => 4,
                g => g.coords_count(),
            })
            .sum()
    }

    pub(super) fn props(&self) -> &SharedProps {
        &self.target.props
    }
}

/// Merge every line and polygon whose gap is below `mingap` into the last
/// kept member of the same kind and band.
///
/// `ordered` must be in `seq` order (the file's spatial order); the result
/// keeps that order. A member with nothing of its kind and band before it is
/// kept. With `mingap == 0` nothing merges.
pub(super) fn coalesce_members<'a>(ordered: &'a [&'a Member], mingap: u64) -> Vec<Merged<'a>> {
    #[cfg(test)]
    crate::valve_counter::note(|c| c.coalesce_calls += 1);
    // (kept member, members merged into it), as indices into `ordered`
    let mut kept: Vec<(usize, Vec<usize>)> = Vec::with_capacity(ordered.len());
    // The index in `kept` of the last kept member per (band, kind). Bands
    // are few, so a linear scan beats a map.
    let mut last: Vec<(u16, MergeKind, usize)> = Vec::new();
    for (i, &m) in ordered.iter().enumerate() {
        let kind = merge_kind(&m.geom);
        if let Some(kind) = kind {
            if m.gap < mingap {
                if let Some(&(_, _, t)) = last.iter().find(|(b, k, _)| *b == m.band && *k == kind) {
                    kept[t].1.push(i);
                    continue;
                }
            }
        }
        kept.push((i, Vec::new()));
        if let Some(kind) = kind {
            let idx = kept.len() - 1;
            match last.iter_mut().find(|(b, k, _)| *b == m.band && *k == kind) {
                Some(slot) => slot.2 = idx,
                None => last.push((m.band, kind, idx)),
            }
        }
    }
    kept.into_iter()
        .map(|(t, absorbed)| {
            let target = ordered[t];
            let n = absorbed.len();
            let mut members = Vec::with_capacity(n + 1);
            members.push(t);
            members.extend(absorbed);
            Merged {
                target,
                ordered,
                geom: Default::default(),
                absorbed: n,
                members,
            }
        })
        .collect()
}

/// The target's geometry with every absorbed member's parts appended, as one
/// multi-geometry of the target's kind.
fn concat_geometry<'a>(
    target: &Geometry<f64>,
    absorbed: impl Iterator<Item = &'a Member>,
) -> Geometry<f64> {
    match merge_kind(target) {
        Some(MergeKind::Polygon) => {
            let mut polys: Vec<Polygon<f64>> = Vec::new();
            push_polygons(target, &mut polys);
            for m in absorbed {
                push_polygons(&m.geom, &mut polys);
            }
            Geometry::MultiPolygon(MultiPolygon(polys))
        }
        Some(MergeKind::Line) => {
            let mut lines: Vec<LineString<f64>> = Vec::new();
            push_lines(target, &mut lines);
            for m in absorbed {
                push_lines(&m.geom, &mut lines);
            }
            Geometry::MultiLineString(MultiLineString(lines))
        }
        // Points and collections are never merge targets.
        None => target.clone(),
    }
}

fn push_polygons(g: &Geometry<f64>, out: &mut Vec<Polygon<f64>>) {
    #[cfg(test)]
    crate::valve_counter::clone_polygons(g);
    match g {
        Geometry::Polygon(p) => out.push(p.clone()),
        Geometry::MultiPolygon(mp) => out.extend(mp.0.iter().cloned()),
        Geometry::Rect(r) => out.push(r.to_polygon()),
        Geometry::Triangle(t) => out.push(t.to_polygon()),
        _ => {}
    }
}

fn push_lines(g: &Geometry<f64>, out: &mut Vec<LineString<f64>>) {
    match g {
        Geometry::LineString(l) => out.push(l.clone()),
        Geometry::MultiLineString(ml) => out.extend(ml.0.iter().cloned()),
        Geometry::Line(l) => out.push(LineString::from(vec![l.start, l.end])),
        _ => {}
    }
}

/// The gaps the threshold is chosen from: those of the members that can
/// merge (lines and polygons with a predecessor).
pub(super) fn mergeable_gaps(ordered: &[&Member]) -> Vec<u64> {
    ordered
        .iter()
        .filter(|m| m.gap != GAP_NONE && merge_kind(&m.geom).is_some())
        .map(|m| m.gap)
        .collect()
}

/// The next merge threshold, tippecanoe's `choose_mingap`: keep about
/// `keep_fraction` of the features by choosing the gap at that quantile from
/// the top, forced above `current`. The threshold is that gap plus one, so
/// it merges every gap `<=` the quantile, ties included.
///
/// It used to snap up to the next power of two, which overshot the quantile
/// by up to 2×: on a tile whose gaps all sat within one octave, the first
/// step merged every polygon into one feature. Merging only what the
/// quantile asks for takes more encodes on a dense tile (up to 30 against
/// 13 on 4.58M Mato Grosso fields). A quarter-octave rung, tried to win
/// them back, took as many (25 against 27 at 2M): the shrinking
/// `keep_fraction`, not the rounding, sets how many steps a tile needs.
///
/// `None` when no gap lies above `current`: the threshold cannot rise, and
/// the tile has merged everything it can.
pub(super) fn next_mingap(gaps: &mut [u64], keep_fraction: f64, current: u64) -> Option<u64> {
    if gaps.is_empty() {
        return None;
    }
    gaps.sort_unstable();
    let f = keep_fraction.clamp(0.0, 1.0);
    let mut ix = ((gaps.len() - 1) as f64 * (1.0 - f)) as usize;
    // Every gap below `current` already merges; step to the first that
    // does not.
    while ix + 1 < gaps.len() && gaps[ix] < current {
        ix += 1;
    }
    if gaps[ix] < current {
        return None;
    }
    // `gaps` never holds GAP_NONE, so this stays a real threshold, and it
    // is above `current` because `gaps[ix] >= current`.
    Some(gaps[ix].saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{coord, line_string, point, polygon};
    use std::sync::Arc;

    fn member(seq: u64, geom: Geometry<f64>, gap: u64, band: u16) -> Member {
        Member {
            key: 0,
            seq,
            geom,
            props: Arc::new(vec![(Arc::from("n"), crate::mvt::PropertyValue::UInt(seq))]),
            id: Some(seq * 10),
            gap,
            band,
        }
    }

    fn square(x: f64) -> Geometry<f64> {
        Geometry::Polygon(polygon![
            (x: x, y: 0.0), (x: x + 1.0, y: 0.0), (x: x + 1.0, y: 1.0), (x: x, y: 1.0), (x: x, y: 0.0)
        ])
    }

    fn line(x: f64) -> Geometry<f64> {
        Geometry::LineString(line_string![(x: x, y: 0.0), (x: x + 1.0, y: 1.0)])
    }

    fn polygon_count(g: &Geometry<f64>) -> usize {
        match g {
            Geometry::Polygon(_) => 1,
            Geometry::MultiPolygon(mp) => mp.0.len(),
            _ => 0,
        }
    }

    #[test]
    fn mutation_regression_merged_coords_count() {
        // An unabsorbed Rect has four coordinates; once concatenated it is
        // represented by a closed five-coordinate polygon ring.
        let ms = [
            member(
                0,
                Geometry::Rect(geo::Rect::new((0., 0.), (1., 1.))),
                GAP_NONE,
                0,
            ),
            member(
                1,
                Geometry::Triangle(geo::Triangle::new(
                    coord! { x: 2., y: 0. },
                    coord! { x: 3., y: 0. },
                    coord! { x: 2., y: 1. },
                )),
                1,
                0,
            ),
            member(2, square(4.), 1, 0),
        ];
        let ordered: Vec<_> = ms.iter().collect();
        let separate = coalesce_members(&ordered, 0);
        assert_eq!(
            separate
                .iter()
                .map(Merged::coords_count)
                .collect::<Vec<_>>(),
            [4, 3, 5]
        );
        let merged = coalesce_members(&ordered, 2);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].absorbed, 2);
        assert_eq!(merged[0].coords_count(), 14);
        assert_eq!(merged[0].geometry().coords_count(), 14);
    }

    #[test]
    fn mutation_regression_tile_feature_coords_count_forwards() {
        use super::super::TileFeature;

        fn count<T: TileFeature>(feature: T) -> usize {
            feature.coords_count()
        }

        let ms = [member(0, line(0.), GAP_NONE, 0), member(1, line(2.), 1, 0)];
        let ordered: Vec<_> = ms.iter().collect();
        for (threshold, expected) in [(0, 2), (2, 4)] {
            let merged = coalesce_members(&ordered, threshold);
            assert_eq!(TileFeature::coords_count(&merged[0]), expected);
            assert_eq!(count(&merged[0]), expected);
        }
    }

    #[test]
    fn world_xy_spans_the_32_bit_world() {
        let (x0, y0) = world_xy(-180.0, MAX_LAT);
        let (x1, y1) = world_xy(180.0, -MAX_LAT);
        assert!(x0.abs() < 1.0 && y0.abs() < 1.0, "{x0} {y0}");
        assert!(
            (x1 - WORLD).abs() < 1.0 && (y1 - WORLD).abs() < 1.0,
            "{x1} {y1}"
        );
        let (xc, yc) = world_xy(0.0, 0.0);
        assert!((xc - WORLD / 2.0).abs() < 1.0 && (yc - WORLD / 2.0).abs() < 1.0);
    }

    #[test]
    fn gap_is_the_farthest_corner_squared() {
        let from = world_xy(0.0, 0.0);
        let near = Rect::new(coord! { x: 0.0, y: 0.0 }, coord! { x: 0.001, y: 0.001 });
        let far = Rect::new(coord! { x: 0.0, y: 0.0 }, coord! { x: 1.0, y: 1.0 });
        let g_near = gap_to_rect(from, &near);
        let g_far = gap_to_rect(from, &far);
        assert!(g_near > 0 && g_near < g_far);
        // 1° of longitude at the equator is 2^32/360 world units.
        let unit = WORLD / 360.0;
        let expect = 2.0 * unit * unit;
        assert!(
            (g_far as f64 / expect - 1.0).abs() < 1e-3,
            "{g_far} vs {expect}"
        );
    }

    #[test]
    fn gap_saturates_below_gap_none() {
        let from = world_xy(-180.0, MAX_LAT);
        let world = Rect::new(coord! { x: -180.0, y: -90.0 }, coord! { x: 180.0, y: 90.0 });
        // dx² + dy² is 2^65 here, past u64.
        assert_eq!(gap_to_rect(from, &world), GAP_NONE - 1);
    }

    #[test]
    fn cursor_resets_at_each_row_group_and_reports_bands() {
        let mut c = RowGapCursor::new(vec![(0, 2), (0, 0), (1, 2)]);
        let r = |x: f64| {
            Some(Rect::new(
                coord! { x: x, y: 0.0 },
                coord! { x: x + 0.1, y: 0.1 },
            ))
        };
        let (g0, b0) = c.next_row(r(0.0));
        let (g1, b1) = c.next_row(r(1.0));
        let (g2, b2) = c.next_row(r(2.0));
        let (g3, b3) = c.next_row(r(3.0));
        assert_eq!(
            (g0, b0),
            (GAP_NONE, 0),
            "first row of a row group has no gap"
        );
        assert!(g1 < GAP_NONE && b1 == 0);
        assert_eq!((g2, b2), (GAP_NONE, 1), "the empty row group is skipped");
        assert!(g3 < GAP_NONE && b3 == 1);
    }

    #[test]
    fn cursor_skips_empty_geometry_without_moving_the_predecessor() {
        let mut c = RowGapCursor::new(vec![(0, 3)]);
        let r = |x: f64| {
            Some(Rect::new(
                coord! { x: x, y: 0.0 },
                coord! { x: x + 0.1, y: 0.1 },
            ))
        };
        c.next_row(r(0.0));
        assert_eq!(c.next_row(None).0, GAP_NONE);
        let mut d = RowGapCursor::new(vec![(0, 2)]);
        d.next_row(r(0.0));
        assert_eq!(c.next_row(r(1.0)).0, d.next_row(r(1.0)).0);
    }

    /// #817: `batch` measures its gaps in the pool, and gives exactly the
    /// gaps `next_row` gives row by row, across row groups, empty rows and
    /// batch boundaries.
    #[test]
    fn batch_gaps_are_measured_in_the_pool_and_match_the_row_walk() {
        let geoms: Vec<Geometry<f64>> = (0..3000)
            .map(|i| {
                let x = f64::from(i % 97) * 0.37 - 170.0;
                let y = f64::from(i % 89) * 0.41 - 40.0;
                if i % 11 == 0 {
                    Geometry::GeometryCollection(geo::GeometryCollection::default())
                } else {
                    Geometry::Polygon(polygon![
                        (x: x, y: y),
                        (x: x + 0.2, y: y),
                        (x: x, y: y + 0.3),
                    ])
                }
            })
            .collect();
        let groups = vec![(0, 700), (0, 0), (1, 1300), (2, 1000)];
        let mut serial = RowGapCursor::new(groups.clone());
        let expected: Vec<(u64, u16)> = geoms
            .iter()
            .map(|g| serial.next_row(g.bounding_rect()))
            .collect();
        let mut cursor = RowGapCursor::new(groups);
        SERIAL_GAPS.with(|n| n.set(0));
        let got: Vec<(u64, u16)> = [&geoms[..1], &geoms[1..1500], &geoms[1500..]]
            .into_iter()
            .flat_map(|chunk| cursor.batch(chunk))
            .collect();
        assert_eq!(got, expected);
        assert_eq!(
            SERIAL_GAPS.with(std::cell::Cell::get),
            0,
            "gaps measured on the calling thread"
        );
    }

    #[test]
    fn a_cursor_not_measuring_reports_bands_and_no_gaps() {
        let g =
            || Geometry::Polygon(polygon![(x: 0.0, y: 0.0), (x: 1.0, y: 0.0), (x: 0.0, y: 1.0)]);
        let geoms: Vec<Geometry<f64>> = (0..4).map(|_| g()).collect();
        let groups = vec![(0, 2), (1, 2)];
        let off = RowGapCursor::new(groups.clone())
            .measuring(false)
            .batch(&geoms);
        assert_eq!(
            off,
            [(GAP_NONE, 0), (GAP_NONE, 0), (GAP_NONE, 1), (GAP_NONE, 1)]
        );
        let on = RowGapCursor::new(groups).batch(&geoms);
        let bands: Vec<u16> = on.iter().map(|&(_, b)| b).collect();
        assert_eq!(bands, [0, 0, 1, 1]);
        assert!(on[1].0 < GAP_NONE, "measured by default");
    }

    #[test]
    fn dense_polygons_merge_into_the_previous_kept_polygon() {
        let ms = [
            member(0, square(0.0), GAP_NONE, 0),
            member(1, square(1.0), 5, 0),
            member(2, square(2.0), 50, 0),
            member(3, square(3.0), 5, 0),
        ];
        let ordered: Vec<&Member> = ms.iter().collect();
        let out = coalesce_members(&ordered, 10);
        let seqs: Vec<u64> = out.iter().map(|m| m.target.seq).collect();
        assert_eq!(
            seqs,
            [0, 2],
            "gaps below 10 merge into the kept polygon before them"
        );
        assert_eq!(out[0].absorbed, 1);
        assert_eq!(polygon_count(out[0].geometry()), 2);
        assert_eq!(out[1].absorbed, 1);
        // The kept member's id and attributes win.
        assert_eq!(out[0].target.id, Some(0));
        assert_eq!(out[0].props()[0].1, crate::mvt::PropertyValue::UInt(0));
    }

    #[test]
    fn zero_threshold_merges_nothing_and_borrows_geometry() {
        let ms = [
            member(0, square(0.0), GAP_NONE, 0),
            member(1, square(1.0), 0, 0),
        ];
        let ordered: Vec<&Member> = ms.iter().collect();
        let out = coalesce_members(&ordered, 0);
        assert_eq!(out.len(), 2);
        assert!(out
            .iter()
            .all(|m| std::ptr::eq(m.geometry(), &m.target.geom)));
    }

    #[test]
    fn points_and_collections_never_merge() {
        let ms = [
            member(0, Geometry::Point(point!(x: 0.0, y: 0.0)), GAP_NONE, 0),
            member(1, Geometry::Point(point!(x: 0.1, y: 0.0)), 1, 0),
            member(
                2,
                Geometry::GeometryCollection(geo::GeometryCollection(vec![square(0.0)])),
                1,
                0,
            ),
        ];
        let ordered: Vec<&Member> = ms.iter().collect();
        assert_eq!(coalesce_members(&ordered, u64::MAX).len(), 3);
    }

    #[test]
    fn merges_stay_within_kind_and_band() {
        let ms = [
            member(0, square(0.0), GAP_NONE, 0),
            member(1, line(1.0), GAP_NONE, 0),
            member(2, square(2.0), 1, 1), // band 1 has no polygon yet: kept
            member(3, square(3.0), 1, 0), // into seq 0, not the band-1 polygon
            member(4, line(4.0), 1, 0),   // into the line, not a polygon
        ];
        let ordered: Vec<&Member> = ms.iter().collect();
        let out = coalesce_members(&ordered, 10);
        let seqs: Vec<(u64, usize)> = out.iter().map(|m| (m.target.seq, m.absorbed)).collect();
        assert_eq!(seqs, [(0, 1), (1, 1), (2, 0)]);
        assert!(matches!(out[1].geometry(), Geometry::MultiLineString(ml) if ml.0.len() == 2));
        // Each feature names its members by their index in `ordered`, kept
        // member first, in the order their parts are appended (#758).
        let members: Vec<&[usize]> = out.iter().map(|m| m.members.as_slice()).collect();
        assert_eq!(members, [&[0, 3][..], &[1, 4], &[2]]);
    }

    #[test]
    fn coalesce_is_deterministic() {
        let ms: Vec<Member> = (0..200)
            .map(|i| member(i, square(i as f64), (i * 7919) % 97, (i % 3) as u16))
            .collect();
        let ordered: Vec<&Member> = ms.iter().collect();
        let a: Vec<(u64, usize)> = coalesce_members(&ordered, 40)
            .iter()
            .map(|m| (m.target.seq, m.absorbed))
            .collect();
        let b: Vec<(u64, usize)> = coalesce_members(&ordered, 40)
            .iter()
            .map(|m| (m.target.seq, m.absorbed))
            .collect();
        assert_eq!(a, b);
    }

    /// Gaps all within one octave, as for a near-regular grid of similar
    /// parcels, on a tile just over its limit: the threshold merges about
    /// the 20% the quantile asks for, not the whole tile (the power-of-two
    /// snap merged 1000 of 1000 here).
    #[test]
    fn the_threshold_does_not_overshoot_the_quantile() {
        for (lo, hi) in [(130u64, 250u64), (100, 199), (2000, 3900)] {
            let mut gaps: Vec<u64> = (0..1000).map(|i| lo + (hi - lo) * i / 999).collect();
            let m = next_mingap(&mut gaps, 0.8, 0).unwrap();
            let merged = gaps.iter().filter(|&&g| g < m).count();
            assert!(
                (195..=210).contains(&merged),
                "{lo}..{hi}: {merged} of 1000"
            );
        }
        // The largest real gap still yields a threshold that merges it.
        let mut top = [GAP_NONE - 1];
        assert_eq!(next_mingap(&mut top, 0.5, 0), Some(GAP_NONE));
    }

    #[test]
    fn tied_gaps_still_merge() {
        // A regular grid: every gap equal. The quantile is that gap, and the
        // threshold must sit above it or `gap < mingap` never fires.
        let mut gaps = vec![100u64; 50];
        let m = next_mingap(&mut gaps, 0.5, 0).expect("threshold must rise");
        assert!(m > 100);
        let ms: Vec<Member> = (0..50)
            .map(|i| member(i, square(i as f64), 100, 0))
            .collect();
        let ordered: Vec<&Member> = ms.iter().collect();
        assert!(coalesce_members(&ordered, m).len() < 50);
    }

    #[test]
    fn next_mingap_rises_and_stops() {
        let mut gaps: Vec<u64> = (1..=100).collect();
        let m1 = next_mingap(&mut gaps, 0.8, 0).unwrap();
        let m2 = next_mingap(&mut gaps, 0.5, m1).unwrap();
        assert!(m2 > m1);
        // Everything already merges: nothing left to raise to.
        assert_eq!(next_mingap(&mut gaps, 0.5, 1 << 20), None);
        assert_eq!(next_mingap(&mut [], 0.5, 0), None);
    }

    #[test]
    fn mergeable_gaps_skip_points_and_gap_none() {
        let ms = [
            member(0, square(0.0), GAP_NONE, 0),
            member(1, square(1.0), 3, 0),
            member(2, Geometry::Point(point!(x: 0.0, y: 0.0)), 4, 0),
        ];
        let ordered: Vec<&Member> = ms.iter().collect();
        assert_eq!(mergeable_gaps(&ordered), [3]);
    }
}
