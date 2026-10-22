//! Coverage fill (#750, experimental): draw, under each coarse tile's own
//! features, the area its four child tiles cover.
//!
//! Dense polygons too small for a level leave holes at coarse zooms: the
//! overview replaces them with placeholder squares, which overlap one another
//! and leave gaps between them. Tippecanoe's reference output has no such
//! holes because `--coalesce-densest-as-needed` merges dense features
//! (`tile.cpp`, before `reduce_tiny_poly`) and unions them, so a dense area
//! becomes one dissolved shape.
//!
//! This builds the same dissolved shape after the overview, at export time,
//! before the export writes its first tile ([`build_fill_map`]):
//!
//! 1. The finest level's polygons are clipped into the tiles one zoom below
//!    the finest, projected to tile pixels and simplified there.
//! 2. At each zoom, coarsest last, a tile's shapes are unioned where they
//!    touch, clipped to the tile and simplified at [`FILL_SIMPLIFY_PX`]
//!    ([`fill_for_tile`]). That is the tile's fill; halved into its parent's
//!    pixels ([`lift_into_parent`]), it is the parent's input.
//!
//! Area that the simplification loses — rings that collapse, and the convex
//! corners Douglas–Peucker shaves off the rings it keeps — is not dropped:
//! it goes to a placeholder accumulator ([`squares_for_collapsed`]) whose
//! squares sit on a grid, so they never overlap. The fill keeps its area to
//! within a square per tile at every zoom.
//!
//! The export then draws each tile's fill under the tile's own features, one
//! fill feature per feature the fill joins, with that feature's id and tags
//! ([`assign_hosts`]), so an attribute-styled map colours it like its
//! neighbours. The size valve fits the tile's own features into what the
//! fill leaves of the limit (`export.rs`, `FillBytes`).
//!
//! DIVERGENCE FROM TIPPECANOE: tippecanoe coalesces the raw geometry of the
//! dropped features themselves, in every tile that is over its size limit
//! (`tile.cpp`, `coalesce_geometry`). Here the fill comes from the finest
//! level, carried up the zooms already simplified, in every tile whether or
//! not it is over the limit, so the overview is read once and never at
//! coarse zooms.

/// A ring in tile pixels (extent 4096), not closed.
pub(super) type Ring = Vec<[f64; 2]>;
/// An exterior ring followed by its holes.
pub(super) type Shape = Vec<Ring>;

/// Tile extent the fill is built in.
pub(super) const FILL_EXTENT: f64 = 4096.0;

/// Douglas–Peucker tolerance for the fill, in tile pixels: one GSD at the
/// default `--gsd-base`, the tolerance the level geometry is drawn at.
pub(super) const FILL_SIMPLIFY_PX: f64 = 4.0;

/// A ring the simplification collapsed: its area (pixels²) and where it was.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Collapsed {
    pub area: f64,
    pub at: [f64; 2],
}

/// Halve a child tile's shapes into its parent's pixels, child `(dx, dy)` of
/// the parent's four, dropping repeated points. Coordinates stay fractional;
/// the MVT encoder snaps them.
pub(super) fn lift_into_parent(child: &[Shape], dx: u32, dy: u32, out: &mut Vec<Shape>) {
    let half = FILL_EXTENT / 2.0;
    let (ox, oy) = (f64::from(dx) * half, f64::from(dy) * half);
    for shape in child {
        let mut lifted: Shape = Vec::with_capacity(shape.len());
        for (i, ring) in shape.iter().enumerate() {
            let mut r: Ring = Vec::with_capacity(ring.len());
            for p in ring {
                // Exact halves: snapping a thin shape here would round its
                // area away before the fill could count it (#750).
                let q = [p[0] / 2.0 + ox, p[1] / 2.0 + oy];
                if r.last() != Some(&q) {
                    r.push(q);
                }
            }
            if r.len() > 1 && r.first() == r.last() {
                r.pop();
            }
            if r.len() >= 3 {
                lifted.push(r);
            } else if i == 0 {
                break; // the exterior vanished: so does the shape
            }
        }
        if !lifted.is_empty() {
            out.push(lifted);
        }
    }
}

/// Twice the signed area of a ring (shoelace).
fn ring_area2(r: &[[f64; 2]]) -> f64 {
    let mut a = 0.0;
    for k in 0..r.len() {
        let (p, q) = (r[k], r[(k + 1) % r.len()]);
        a += p[0] * q[1] - q[0] * p[1];
    }
    a
}

/// Douglas–Peucker on an open chain, both ends kept; marks survivors in `keep`.
fn douglas_peucker(points: &[[f64; 2]], tol2: f64, keep: &mut [bool]) {
    let mut stack = vec![(0usize, points.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (p, q) = (points[a], points[b]);
        let (dx, dy) = (q[0] - p[0], q[1] - p[1]);
        let len2 = dx * dx + dy * dy;
        let (mut best, mut at) = (-1.0, a);
        for (i, r) in points.iter().enumerate().take(b).skip(a + 1) {
            let (rx, ry) = (r[0] - p[0], r[1] - p[1]);
            let d2 = if len2 == 0.0 {
                rx * rx + ry * ry
            } else {
                let c = rx * dy - ry * dx;
                c * c / len2
            };
            if d2 > best {
                best = d2;
                at = i;
            }
        }
        if best > tol2 {
            keep[at] = true;
            stack.push((a, at));
            stack.push((at, b));
        }
    }
}

/// One closed ring simplified at `tol2` (squared pixels); `None` once it no
/// longer encloses any area.
fn simplify_ring(r: &[[f64; 2]], tol2: f64) -> Option<Ring> {
    if r.len() < 3 {
        return None;
    }
    // Split at the first vertex and the vertex farthest from it, so both
    // halves are open chains with fixed ends.
    let p0 = r[0];
    let d2 = |q: [f64; 2]| (q[0] - p0[0]).powi(2) + (q[1] - p0[1]).powi(2);
    let far = (1..r.len()).max_by(|&i, &j| d2(r[i]).total_cmp(&d2(r[j])))?;
    let closed: Vec<[f64; 2]> = r.iter().copied().chain(std::iter::once(p0)).collect();
    let mut keep = vec![false; closed.len()];
    keep[0] = true;
    keep[far] = true;
    douglas_peucker(&closed[..=far], tol2, &mut keep[..=far]);
    douglas_peucker(&closed[far..], tol2, &mut keep[far..]);
    let out: Ring = r
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(p, _)| *p)
        .collect();
    (out.len() >= 3 && ring_area2(&out) != 0.0).then_some(out)
}

/// Simplify every ring at `tol` pixels. Returns the simplified shapes and the
/// rings that collapsed (an exterior collapsing takes its holes with it).
pub(super) fn simplify_coverage(shapes: Vec<Shape>, tol: f64) -> (Vec<Shape>, Vec<Collapsed>) {
    let tol2 = tol * tol;
    let mut out = Vec::with_capacity(shapes.len());
    let mut collapsed = Vec::new();
    for shape in shapes {
        let Some(exterior) = shape.first().and_then(|e| simplify_ring(e, tol2)) else {
            if let Some(e) = shape.first().filter(|e| !e.is_empty()) {
                let holes: f64 = shape[1..].iter().map(|h| ring_area2(h).abs()).sum();
                let area = (ring_area2(e).abs() - holes).max(0.0) / 2.0;
                if area > 0.0 {
                    collapsed.push(Collapsed { area, at: e[0] });
                }
            }
            continue;
        };
        // One winding for every exterior and the other for every hole: the
        // union's `NonZero` rule adds overlapping exteriors only when they
        // wind the same way, and source rings come in either.
        let orient = |mut r: Ring, exterior: bool| {
            if (ring_area2(&r) > 0.0) != exterior {
                r.reverse();
            }
            r
        };
        // A hole that collapses is filled in: a few pixels over-drawn.
        let mut kept = vec![orient(exterior, true)];
        kept.extend(
            shape[1..]
                .iter()
                .filter_map(|h| simplify_ring(h, tol2))
                .map(|h| orient(h, false)),
        );
        // Douglas–Peucker shaves convex corners: on a blob a few tolerances
        // across that is a third of its area, at every zoom. Credit the net
        // change (negative where it filled a concavity) to the squares, so
        // the fill keeps its area to within a square per tile.
        let shaved = net_area(&shape) - net_area(&kept);
        if shaved != 0.0 {
            collapsed.push(Collapsed {
                area: shaved,
                at: kept[0][0],
            });
        }
        out.push(kept);
    }
    (out, collapsed)
}

/// A shape's exterior area minus its holes, pixels².
fn net_area(shape: &[Ring]) -> f64 {
    let Some((exterior, holes)) = shape.split_first() else {
        return 0.0;
    };
    (ring_area2(exterior).abs() - holes.iter().map(|h| ring_area2(h).abs()).sum::<f64>()) / 2.0
}

/// Exterior-ring bounding box `[min_x, min_y, max_x, max_y]`.
fn bbox(shape: &Shape) -> [f64; 4] {
    let mut b = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for p in &shape[0] {
        b[0] = b[0].min(p[0]);
        b[1] = b[1].min(p[1]);
        b[2] = b[2].max(p[0]);
        b[3] = b[3].max(p[1]);
    }
    b
}

/// The union of `shapes` clipped to the tile, in one `i_overlay` pass:
/// `NonZero` makes overlapping parts add, as in `mvt.rs`.
fn union_clip(shapes: &[Shape]) -> Vec<Shape> {
    use i_overlay::core::fill_rule::FillRule;
    use i_overlay::core::overlay_rule::OverlayRule;
    use i_overlay::float::overlay::FloatOverlay;

    let e = FILL_EXTENT;
    let tile: Shape = vec![vec![[0.0, 0.0], [e, 0.0], [e, e], [0.0, e]]];
    FloatOverlay::with_subj_and_clip_custom(shapes, &[tile], Default::default(), Default::default())
        .overlay(OverlayRule::Intersect, FillRule::NonZero)
}

/// Union shapes whose bounding boxes meet and clip them to the tile; a lone
/// shape inside the tile passes through untouched.
pub(super) fn union_touching(shapes: Vec<Shape>) -> Vec<Shape> {
    let n = shapes.len();
    let boxes: Vec<[f64; 4]> = shapes.iter().map(bbox).collect();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_unstable_by(|&a, &b| boxes[a][0].total_cmp(&boxes[b][0]).then(a.cmp(&b)));
    let mut root: Vec<usize> = (0..n).collect();
    fn find(root: &mut [usize], mut i: usize) -> usize {
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    // Sweep on min x; boxes that meet (touching counts) join one group.
    let mut active: Vec<usize> = Vec::new();
    for &i in &order {
        let b = boxes[i];
        active.retain(|&j| boxes[j][2] >= b[0]);
        for &j in &active {
            let c = boxes[j];
            if c[1] <= b[3] && b[1] <= c[3] {
                let (ri, rj) = (find(&mut root, i), find(&mut root, j));
                if ri != rj {
                    root[ri.max(rj)] = ri.min(rj);
                }
            }
        }
        active.push(i);
    }
    // Groups in order of their lowest member, so the output is deterministic.
    let mut groups: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in 0..n {
        let r = find(&mut root, i);
        groups[r].push(i);
    }
    let inside =
        |b: [f64; 4]| b[0] >= 0.0 && b[1] >= 0.0 && b[2] <= FILL_EXTENT && b[3] <= FILL_EXTENT;
    let mut slots: Vec<Option<Shape>> = shapes.into_iter().map(Some).collect();
    let mut out = Vec::new();
    for group in groups.into_iter().filter(|g| !g.is_empty()) {
        if let [only] = group[..] {
            if inside(boxes[only]) {
                out.extend(slots[only].take());
                continue;
            }
        }
        let members: Vec<Shape> = group.iter().filter_map(|&i| slots[i].take()).collect();
        out.extend(union_clip(&members));
    }
    out
}

/// Placeholder squares for collapsed rings, by error diffusion on a `side`
/// grid: each cell sums the area of the rings that collapsed in it; cells are
/// visited in serpentine row order with a running total, and a cell draws a
/// `side`-pixel square whenever the total reaches `side²`. The excess carries
/// to the next cell, so a tile loses less than one square of area, however
/// the rings are ordered.
///
/// DIVERGENCE FROM TIPPECANOE: `reduce_tiny_poly` (clip.cpp) emits a square
/// at the first vertex of the ring whose area crosses the running total, in
/// feature order, so squares can overlap; here they sit on a grid and never
/// do (#750).
pub(super) fn squares_for_collapsed(collapsed: &[Collapsed], side: f64) -> Vec<Shape> {
    let threshold = side * side;
    let mut cells: std::collections::HashMap<(i64, i64), f64> = std::collections::HashMap::new();
    // A ring on the tile's right or bottom edge falls in the last cell, so
    // no square is drawn past the edge.
    let last = (FILL_EXTENT / side).ceil() as i64 - 1;
    for c in collapsed {
        let cell = (
            ((c.at[0] / side).floor() as i64).clamp(0, last),
            ((c.at[1] / side).floor() as i64).clamp(0, last),
        );
        *cells.entry(cell).or_default() += c.area;
    }
    let mut order: Vec<((i64, i64), f64)> = cells.into_iter().collect();
    // Serpentine: rows top to bottom, alternate rows right to left, so the
    // carry always moves to a neighbouring cell within a row.
    order.sort_unstable_by_key(|&((x, y), _)| (y, if y % 2 == 0 { x } else { -x }));
    let mut total = 0.0;
    let mut out = Vec::new();
    for ((cx, cy), area) in order {
        total += area;
        // A hair under the threshold still counts: sums of exact areas can
        // land a rounding error short.
        if total >= threshold * (1.0 - 1e-9) {
            let (x, y) = (cx as f64 * side, cy as f64 * side);
            out.push(vec![vec![
                [x, y],
                [x + side, y],
                [x + side, y + side],
                [x, y + side],
            ]]);
            total -= threshold;
        }
    }
    out
}

/// One tile's fill from the shapes lifted into it: simplify, union where
/// shapes touch, simplify the union, and return collapsed area as squares.
pub(super) fn fill_for_tile(lifted: Vec<Shape>) -> Vec<Shape> {
    let (shapes, collapsed) = simplify_coverage(lifted, FILL_SIMPLIFY_PX);
    fill_from(shapes, collapsed)
}

/// The tail of [`fill_for_tile`] for shapes already simplified at this
/// tile's scale, with the area that collapsed (or was shaved) on the way.
fn fill_from(shapes: Vec<Shape>, mut collapsed: Vec<Collapsed>) -> Vec<Shape> {
    let (mut fill, more) = simplify_coverage(union_touching(shapes), FILL_SIMPLIFY_PX);
    collapsed.extend(more);
    fill.extend(squares_for_collapsed(&collapsed, FILL_SIMPLIFY_PX));
    fill
}

/// An open ring in integer tile units at the export extent.
pub(super) type TileRing = Vec<(i32, i32)>;
/// An exterior ring followed by its holes, in tile units.
pub(super) type FillPolygon = Vec<TileRing>;

/// A fill snapped to integer tile units at `scale` × [`FILL_EXTENT`]: rings
/// that snap to fewer than three distinct points are dropped (an exterior
/// with its holes).
pub(super) fn to_tile_polygons(fill: &[Shape], scale: f64) -> Vec<FillPolygon> {
    let ring = |r: &Ring| -> Option<TileRing> {
        let mut out: TileRing = Vec::with_capacity(r.len());
        for p in r {
            let q = ((p[0] * scale).round() as i32, (p[1] * scale).round() as i32);
            if out.last() != Some(&q) {
                out.push(q);
            }
        }
        if out.len() > 1 && out.first() == out.last() {
            out.pop();
        }
        (out.len() >= 3).then_some(out)
    };
    fill.iter()
        .filter_map(|s| {
            let mut poly = vec![ring(s.first()?)?];
            poly.extend(s[1..].iter().filter_map(ring));
            Some(poly)
        })
        .collect()
}

/// For each fill polygon, the index of the feature box it joins: the
/// smallest of `hosts` (tile-unit `[min_x, min_y, max_x, max_y]`) holding the
/// polygon's first vertex, else the nearest. Tippecanoe likewise gives a
/// coalesced feature the attributes of the feature it joins (`tile.cpp`,
/// `find_feature_to_accumulate_onto`); its choice is the previous feature in
/// spatial order, ours is spatial containment. `hosts` must not be empty.
pub(super) fn assign_hosts(polys: &[FillPolygon], hosts: &[[f64; 4]]) -> Vec<usize> {
    const CELLS: usize = 64;
    let size = FILL_EXTENT / CELLS as f64;
    let cell_of = |v: f64| ((v / size).floor().max(0.0) as usize).min(CELLS - 1);
    // Every host box is listed in each grid cell it overlaps.
    let mut grid: Vec<Vec<usize>> = vec![Vec::new(); CELLS * CELLS];
    for (h, b) in hosts.iter().enumerate() {
        for cy in cell_of(b[1])..=cell_of(b[3]) {
            for cx in cell_of(b[0])..=cell_of(b[2]) {
                grid[cy * CELLS + cx].push(h);
            }
        }
    }
    let dist2 = |b: &[f64; 4], x: f64, y: f64| {
        let dx = (b[0] - x).max(0.0).max(x - b[2]);
        let dy = (b[1] - y).max(0.0).max(y - b[3]);
        dx * dx + dy * dy
    };
    let area = |b: &[f64; 4]| (b[2] - b[0]) * (b[3] - b[1]);
    polys
        .iter()
        .map(|poly| {
            let (x, y) = (f64::from(poly[0][0].0), f64::from(poly[0][0].1));
            let (cx, cy) = (cell_of(x), cell_of(y));
            // Containment first, smallest box winning, ties to the lower index.
            let holding = grid[cy * CELLS + cx]
                .iter()
                .copied()
                .filter(|&h| dist2(&hosts[h], x, y) == 0.0)
                .min_by(|&a, &b| area(&hosts[a]).total_cmp(&area(&hosts[b])).then(a.cmp(&b)));
            if let Some(h) = holding {
                return h;
            }
            // Else the nearest box, over growing rings of cells. Any box
            // found within ring `r` is at most `r + 1` cells away, so one
            // more ring settles it.
            let mut best: Option<(f64, usize)> = None;
            for r in 0..CELLS {
                let (x0, x1) = (cx.saturating_sub(r), (cx + r).min(CELLS - 1));
                let (y0, y1) = (cy.saturating_sub(r), (cy + r).min(CELLS - 1));
                for gy in y0..=y1 {
                    for gx in x0..=x1 {
                        if gy != y0 && gy != y1 && gx != x0 && gx != x1 {
                            continue; // inside: seen at a smaller ring
                        }
                        for &h in &grid[gy * CELLS + gx] {
                            let d = dist2(&hosts[h], x, y);
                            if best.is_none_or(|(bd, bh)| d < bd || (d == bd && h < bh)) {
                                best = Some((d, h));
                            }
                        }
                    }
                }
                if let Some((d, _)) = best {
                    if d.sqrt() <= r as f64 * size {
                        break;
                    }
                }
            }
            best.map_or(0, |(_, h)| h)
        })
        .collect()
}

/// Every coarse tile's fill, in integer tile units keyed by PMTiles tile id.
/// Built before the export writes anything, so the export keeps its
/// coarse→fine order.
pub(super) struct FillMap {
    tiles: std::collections::HashMap<u64, Vec<FillPolygon>>,
    used: std::sync::atomic::AtomicUsize,
}

impl FillMap {
    /// The fill for tile `key`, if it has one.
    pub(super) fn get(&self, key: u64) -> Option<&[FillPolygon]> {
        let polys = self.tiles.get(&key)?;
        self.used.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Some(polys)
    }
}

impl Drop for FillMap {
    /// Fills built, and how many the export drew. A fill is drawn only into
    /// a tile that has features of its own; the rest are not emitted yet.
    fn drop(&mut self) {
        if self.tiles.is_empty() {
            return;
        }
        // Encode retries (the size valve) count a tile more than once.
        let used = self.used.load(std::sync::atomic::Ordering::Relaxed);
        log::info!(
            "[export] coverage fill: {} fill tiles built, {used} draws into tiles with features",
            self.tiles.len(),
        );
    }
}

/// A geo polygon in lon/lat as a [`Shape`] in tile pixels, closing vertex
/// dropped.
fn project_polygon(p: &geo::Polygon<f64>, proj: &crate::mvt::TileProjector) -> Shape {
    let ring = |ls: &geo::LineString<f64>| -> Ring {
        let n = ls.0.len().saturating_sub(usize::from(ls.is_closed()));
        ls.0[..n]
            .iter()
            .map(|c| {
                let (x, y) = proj.project(c.x, c.y);
                [x, y]
            })
            .collect()
    };
    std::iter::once(p.exterior())
        .chain(p.interiors())
        .map(ring)
        .collect()
}

/// One seed batch: every polygon clipped into its tiles at `seed_zoom`,
/// projected to tile pixels and simplified, appended to its tile's bucket in
/// file order.
#[allow(clippy::too_many_arguments)]
fn seed_batch(
    batch: &super::RecordBatch,
    geometry: Option<&str>,
    crs: super::Crs,
    seed_zoom: u8,
    opts: &super::ExportOptions,
    timers: &super::ExportTimers,
    buckets: &mut std::collections::BTreeMap<u64, (Vec<Shape>, Vec<Collapsed>)>,
    features: &mut usize,
) -> Result<(), super::ExportError> {
    use rayon::prelude::*;
    type Bucket = (Vec<Shape>, Vec<Collapsed>);
    let schema = batch.schema();
    let geom_idx = super::batch_geometry_index(batch, geometry)?;
    let geoms = super::decode_batch_geometries(batch, &schema, geom_idx, crs, timers)?;
    *features += geoms.len();
    let pieces: Vec<Vec<(u64, Bucket)>> = geoms
        .par_iter()
        .map(|g| {
            if !matches!(
                g,
                geo::Geometry::Polygon(_) | geo::Geometry::MultiPolygon(_)
            ) {
                return Vec::new();
            }
            super::feature_tile_members(g, seed_zoom, opts, 0, u64::MAX)
                .into_iter()
                .map(|(key, clipped)| {
                    let (x, y) = super::key_to_xy(key, seed_zoom);
                    let tb = super::TileCoord::new(x, y, seed_zoom).bounds();
                    let proj = crate::mvt::TileProjector::new(&tb, FILL_EXTENT as u32);
                    let shapes: Vec<Shape> = match &clipped {
                        geo::Geometry::Polygon(p) => vec![project_polygon(p, &proj)],
                        geo::Geometry::MultiPolygon(mp) => {
                            mp.0.iter().map(|p| project_polygon(p, &proj)).collect()
                        }
                        _ => Vec::new(),
                    };
                    (key, simplify_coverage(shapes, FILL_SIMPLIFY_PX))
                })
                .collect()
        })
        .collect();
    for (key, (shapes, collapsed)) in pieces.into_iter().flatten() {
        let b = buckets.entry(key).or_default();
        b.0.extend(shapes);
        b.1.extend(collapsed);
    }
    Ok(())
}

/// Build the [`FillMap`] for zooms `min_zoom..finest_zoom` from the level
/// at `level_idx`, the one exported at `finest_zoom`.
///
/// Each of its polygons is clipped into the tiles one zoom below the finest
/// (the export's own clip), projected to tile pixels and simplified there;
/// each zoom's fills are then built and lifted into the next, coarsest last.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_fill_map(
    reader: &super::OverviewReader,
    level_idx: usize,
    crs: super::Crs,
    geometry: Option<&str>,
    finest_zoom: u8,
    min_zoom: u8,
    opts: &super::ExportOptions,
    timers: &super::ExportTimers,
) -> Result<FillMap, super::ExportError> {
    use rayon::prelude::*;
    use std::collections::BTreeMap;

    let mut map = FillMap {
        tiles: Default::default(),
        used: Default::default(),
    };
    if finest_zoom == 0 || min_zoom >= finest_zoom {
        return Ok(map);
    }
    let seed_zoom = finest_zoom - 1;
    let t_seed = std::time::Instant::now();

    // Seed: per tile at `seed_zoom`, its polygons' shapes and collapsed area,
    // in the order the level stores them.
    type Bucket = (Vec<Shape>, Vec<Collapsed>);
    let mut buckets: BTreeMap<u64, Bucket> = BTreeMap::new();
    let mut features = 0usize;
    // Clip to the exact tile, with no buffer: a polygon in the buffer band
    // would land in both neighbours, and its collapsed area would be
    // credited twice.
    let seed_opts = super::ExportOptions {
        tile_buffer: 0,
        ..opts.clone()
    };
    // The read runs on its own thread, one batch ahead of the parallel
    // per-feature work, as the export's own band reads do (#535).
    let buckets_ref = &mut buckets;
    let features_ref = &mut features;
    super::scoped_pipe(
        super::WAVE_READ_IN_FLIGHT,
        |tx: &super::Sender<super::RecordBatch>| -> Result<(), super::ExportError> {
            let batches =
                reader.read_level_with_batch_size(level_idx, None, super::EXPORT_BATCH_SIZE)?;
            for batch in batches {
                let batch = batch
                    .map_err(|e| crate::Error::GeoParquetRead(format!("fill seed read: {e}")))?;
                if tx.send(batch).is_err() {
                    return Ok(()); // the consumer stopped (error path)
                }
            }
            Ok(())
        },
        |rx: super::Receiver<super::RecordBatch>| -> Result<(), super::ExportError> {
            for batch in rx.iter() {
                seed_batch(
                    &batch,
                    geometry,
                    crs,
                    seed_zoom,
                    &seed_opts,
                    timers,
                    buckets_ref,
                    features_ref,
                )?;
            }
            Ok(())
        },
    )?;
    let seed_secs = t_seed.elapsed().as_secs_f64();

    // Pyramid: each zoom's fills, then lifted into the zoom above.
    let t_pyramid = std::time::Instant::now();
    let scale = f64::from(opts.extent) / FILL_EXTENT;
    let mut vertices = 0usize;
    let mut zoom = seed_zoom;
    loop {
        let fills: Vec<(u64, Vec<Shape>)> = std::mem::take(&mut buckets)
            .into_iter()
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|(key, (shapes, collapsed))| {
                let fill = if zoom == seed_zoom {
                    fill_from(shapes, collapsed)
                } else {
                    debug_assert!(collapsed.is_empty());
                    fill_for_tile(shapes)
                };
                (key, fill)
            })
            .collect();
        let snapped: Vec<(u64, Vec<FillPolygon>)> = fills
            .par_iter()
            .filter(|(_, fill)| !fill.is_empty())
            .map(|(key, fill)| (*key, to_tile_polygons(fill, scale)))
            .collect();
        for (key, polys) in snapped {
            if !polys.is_empty() {
                map.tiles.insert(key, polys);
            }
        }
        if log::log_enabled!(log::Level::Debug) {
            // Fill area in seed-zoom pixels, so zooms compare: it should hold
            // steady up the zooms.
            let k = 4f64.powi(i32::from(seed_zoom - zoom));
            let area: f64 = fills
                .iter()
                .flat_map(|(_, f)| f.iter())
                .map(|s| net_area(s))
                .sum::<f64>()
                * k;
            log::debug!(
                "[export] coverage fill z{zoom}: {} tiles, area {area:.0} seed px²",
                fills.len()
            );
        }
        if zoom == min_zoom {
            break;
        }
        for (key, fill) in &fills {
            vertices += fill.iter().flatten().map(Vec::len).sum::<usize>();
            let (x, y) = super::key_to_xy(*key, zoom);
            let parent = crate::pmtiles_writer::tile_id(zoom - 1, x / 2, y / 2);
            let b = buckets.entry(parent).or_default();
            lift_into_parent(fill, x & 1, y & 1, &mut b.0);
        }
        zoom -= 1;
    }
    log::info!(
        "[export] coverage fill (#750, experimental): seed z{seed_zoom} from {features} features \
         in {seed_secs:.2}s; z{min_zoom}..=z{seed_zoom} built in {:.2}s; {} fill tiles, \
         {vertices} vertices lifted",
        t_pyramid.elapsed().as_secs_f64(),
        map.tiles.len(),
    );
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, side: f64) -> Shape {
        vec![vec![
            [x, y],
            [x + side, y],
            [x + side, y + side],
            [x, y + side],
        ]]
    }

    fn ring_area(r: &Ring) -> f64 {
        let mut a = 0.0;
        for k in 0..r.len() {
            let (p, q) = (r[k], r[(k + 1) % r.len()]);
            a += p[0] * q[1] - q[0] * p[1];
        }
        a / 2.0
    }

    fn area(shapes: &[Shape]) -> f64 {
        shapes
            .iter()
            .map(|s| {
                let ext = ring_area(&s[0]).abs();
                ext - s[1..].iter().map(|h| ring_area(h).abs()).sum::<f64>()
            })
            .sum()
    }

    #[test]
    fn a_fill_snaps_to_integer_tile_rings_at_the_export_extent() {
        let fill = vec![square(10.4, 20.6, 100.0)];
        let polys = to_tile_polygons(&fill, 0.5);
        assert_eq!(
            polys,
            vec![vec![vec![(5, 10), (55, 10), (55, 60), (5, 60)]]]
        );
        // A ring that snaps to a line is dropped.
        assert!(to_tile_polygons(&[square(0.0, 0.0, 0.2)], 1.0).is_empty());
    }

    #[test]
    fn a_fill_polygon_goes_to_the_smallest_box_holding_it() {
        let polys = vec![vec![vec![(50, 50), (60, 50), (60, 60)]]];
        let hosts = [
            [0.0, 0.0, 4096.0, 4096.0],
            [40.0, 40.0, 80.0, 80.0],
            [500.0, 500.0, 600.0, 600.0],
        ];
        assert_eq!(assign_hosts(&polys, &hosts), vec![1]);
    }

    #[test]
    fn a_fill_polygon_outside_every_box_goes_to_the_nearest() {
        let polys = vec![
            vec![vec![(1000, 1000), (1010, 1000), (1010, 1010)]],
            vec![vec![(3000, 10), (3010, 10), (3010, 20)]],
        ];
        let hosts = [
            [0.0, 0.0, 100.0, 100.0],
            [900.0, 1100.0, 950.0, 1200.0],
            [3500.0, 0.0, 3600.0, 50.0],
        ];
        assert_eq!(assign_hosts(&polys, &hosts), vec![1, 2]);
    }

    #[test]
    fn lift_halves_a_child_into_its_quadrant() {
        let mut out = Vec::new();
        lift_into_parent(&[square(0.0, 0.0, 4096.0)], 1, 0, &mut out);
        assert_eq!(out, vec![square(2048.0, 0.0, 2048.0)]);
    }

    #[test]
    fn lift_halves_exactly_without_snapping() {
        // Snapping is the encoder's job; halving keeps fractions.
        let child = vec![vec![[1.0, 1.0], [3.0, 1.0], [100.0, 1.0], [100.0, 100.0]]];
        let mut out = Vec::new();
        lift_into_parent(&[child], 0, 0, &mut out);
        assert_eq!(
            out,
            vec![vec![vec![
                [0.5, 0.5],
                [1.5, 0.5],
                [50.0, 0.5],
                [50.0, 50.0]
            ]]]
        );
    }

    #[test]
    fn lifting_a_thin_field_keeps_its_area() {
        // A 1 px wide field halves to half a pixel wide: its area must come
        // through the parent's fill (as a square), not round away to nothing.
        let thin: Shape = vec![vec![[1.0, 1.0], [65.0, 1.0], [65.0, 2.0], [1.0, 2.0]]];
        let mut lifted = Vec::new();
        lift_into_parent(&[thin], 0, 0, &mut lifted);
        let fill = fill_for_tile(lifted);
        assert_eq!(area(&fill), 16.0, "64 px² / 4 = one 4×4 square");
    }

    #[test]
    fn simplify_drops_a_sub_tolerance_wiggle() {
        let s = vec![vec![
            [0.0, 0.0],
            [50.0, 1.0],
            [100.0, 0.0],
            [100.0, 100.0],
            [0.0, 100.0],
        ]];
        let (out, credit) = simplify_coverage(vec![s], 4.0);
        assert_eq!(out[0][0].len(), 4, "the 1 px bump goes: {:?}", out[0][0]);
        // The bump dents the shape (y grows downward); filling the dent
        // adds 50 px², which comes back as a negative credit.
        let credited: f64 = credit.iter().map(|c| c.area).sum();
        assert_eq!(credited, -50.0);
    }

    #[test]
    fn simplify_credits_the_area_it_shaves_off_a_ring_it_keeps() {
        // A 12-px octagon at 4 px: Douglas–Peucker keeps a smaller polygon.
        // What it shaves comes back as credit, so kept + credit = original.
        let r = 6.0;
        let octagon: Shape = vec![(0..8)
            .map(|i| {
                let t = std::f64::consts::FRAC_PI_4 * f64::from(i);
                [100.0 + r * t.cos(), 100.0 + r * t.sin()]
            })
            .collect()];
        let before = area(std::slice::from_ref(&octagon));
        let (out, credit) = simplify_coverage(vec![octagon], 4.0);
        assert_eq!(out.len(), 1, "the octagon is kept, smaller");
        let kept = area(&out);
        assert!(kept < before);
        let credited: f64 = credit.iter().map(|c| c.area).sum();
        assert!(
            (kept + credited - before).abs() < 1e-9,
            "{kept} + {credited} != {before}"
        );
    }

    #[test]
    fn simplify_reports_a_collapsed_ring_with_its_area() {
        let (out, collapsed) = simplify_coverage(vec![square(10.0, 20.0, 2.0)], 4.0);
        assert!(out.is_empty());
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].area, 4.0);
        assert_eq!(collapsed[0].at, [10.0, 20.0]);
    }

    #[test]
    fn union_merges_abutting_shapes_and_keeps_area() {
        let out = union_touching(vec![square(0.0, 0.0, 10.0), square(10.0, 0.0, 10.0)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0][0].len(), 4, "one 20×10 rectangle: {:?}", out[0][0]);
        assert_eq!(area(&out), 200.0);
    }

    #[test]
    fn opposite_windings_still_add_in_the_fill() {
        // Source rings come in either winding. Two overlapping squares, one
        // of each, must union to their combined area, not cancel where they
        // overlap.
        let a = square(0.0, 0.0, 40.0);
        let mut b = square(20.0, 0.0, 40.0);
        b[0].reverse();
        let fill = fill_for_tile(vec![a, b]);
        assert_eq!(area(&fill), 60.0 * 40.0);
    }

    #[test]
    fn union_passes_a_lone_interior_shape_through() {
        let lone = square(100.0, 100.0, 10.0);
        assert_eq!(union_touching(vec![lone.clone()]), vec![lone]);
    }

    #[test]
    fn union_clips_a_lone_shape_crossing_the_tile_edge() {
        let out = union_touching(vec![square(4090.0, 0.0, 20.0)]);
        assert_eq!(area(&out), 6.0 * 20.0);
    }

    #[test]
    fn collapsed_area_comes_back_as_non_overlapping_grid_squares() {
        // Sixteen rings of 8 px², two per 4 px cell across eight cells:
        // 128 px² is eight 4×4 squares, one per cell.
        let collapsed: Vec<Collapsed> = (0..16)
            .map(|i| Collapsed {
                area: 8.0,
                at: [f64::from(i / 2) * 4.0 + 1.0, 1.0],
            })
            .collect();
        let out = squares_for_collapsed(&collapsed, 4.0);
        assert_eq!(out.len(), 8);
        let mut cells: Vec<[f64; 2]> = out.iter().map(|s| s[0][0]).collect();
        cells.sort_by(|a, b| a[0].total_cmp(&b[0]));
        cells.dedup();
        assert_eq!(cells.len(), 8, "every square in its own cell");
        assert_eq!(area(&out), 128.0);
    }

    #[test]
    fn a_cells_excess_carries_into_the_next_cell() {
        // Two squares' worth in one cell, then a little in the next cell:
        // the first cell holds one square and the excess draws the second.
        let collapsed = vec![
            Collapsed {
                area: 16.0,
                at: [1.0, 1.0],
            },
            Collapsed {
                area: 16.0,
                at: [2.0, 1.0],
            },
            Collapsed {
                area: 1.0,
                at: [5.0, 1.0],
            },
        ];
        let out = squares_for_collapsed(&collapsed, 4.0);
        assert_eq!(out, vec![square(0.0, 0.0, 4.0), square(4.0, 0.0, 4.0)]);
    }

    #[test]
    fn squares_stay_inside_the_tile() {
        // A ring that collapsed on the tile's right or bottom edge must not
        // draw its square past the edge, where the next tile draws too.
        let collapsed = vec![
            Collapsed {
                area: 16.0,
                at: [FILL_EXTENT, 100.0],
            },
            Collapsed {
                area: 16.0,
                at: [100.0, FILL_EXTENT],
            },
        ];
        for s in squares_for_collapsed(&collapsed, 4.0) {
            for p in &s[0] {
                assert!(
                    (0.0..=FILL_EXTENT).contains(&p[0]) && (0.0..=FILL_EXTENT).contains(&p[1]),
                    "{s:?}"
                );
            }
        }
    }

    #[test]
    fn collapsed_area_is_kept_whatever_the_ring_order() {
        // A 10×10-cell block fully covered by 1 px² rings, given in a
        // scrambled order: every cell gets its square, none is lost.
        let mut collapsed = Vec::new();
        for k in 0..1600u32 {
            let i = (k * 7919) % 1600; // a permutation of 0..1600
            let (px, py) = (f64::from(i % 40), f64::from(i / 40));
            collapsed.push(Collapsed {
                area: 1.0,
                at: [px + 0.5, py + 0.5],
            });
        }
        let out = squares_for_collapsed(&collapsed, 4.0);
        assert_eq!(out.len(), 100);
        assert_eq!(area(&out), 1600.0);
    }

    #[test]
    fn squares_survive_being_lifted_and_collapsed_again() {
        // A parent's fill from four children that each hold a block of
        // placeholder squares keeps their area: lifted, each square is
        // 2×2 px and collapses, and four of them make one square again.
        let child: Vec<Shape> = (0..64)
            .map(|i| square(f64::from(i % 8) * 8.0, f64::from(i / 8) * 8.0, 4.0))
            .collect();
        let mut lifted = Vec::new();
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            lift_into_parent(&child, dx, dy, &mut lifted);
        }
        let fill = fill_for_tile(lifted);
        assert_eq!(area(&fill), 4.0 * 64.0 * 4.0);
    }

    #[test]
    fn four_full_children_fill_their_parent() {
        let mut lifted = Vec::new();
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            lift_into_parent(&[square(0.0, 0.0, 4096.0)], dx, dy, &mut lifted);
        }
        let fill = fill_for_tile(lifted);
        assert_eq!(fill.len(), 1);
        assert_eq!(fill[0][0].len(), 4);
        assert_eq!(area(&fill), 4096.0 * 4096.0);
    }

    #[test]
    fn sub_pixel_children_come_back_as_squares() {
        // 64 separate 1×1 px fields scattered 8 px apart: each collapses at
        // 4 px, and their 64 px² returns as four 4×4 squares.
        let child: Vec<Shape> = (0..64)
            .map(|i| square(f64::from(i % 8) * 16.0, f64::from(i / 8) * 16.0, 2.0))
            .collect();
        let mut lifted = Vec::new();
        lift_into_parent(&child, 0, 0, &mut lifted);
        let fill = fill_for_tile(lifted);
        assert_eq!(area(&fill), 64.0);
        assert_eq!(fill.len(), 4);
    }
}
