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
//! it is credited where it was lost and goes to a placeholder accumulator
//! ([`squares_for_collapsed`]) whose squares sit on a grid, so they never
//! overlap. Each isolated group of credit loses less than one square.
//!
//! The export then draws each tile's fill under the tile's own features, one
//! fill feature per feature the fill joins, with that feature's id and tags
//! ([`assign_hosts`]). A dissolved piece can span many features and carries
//! one of them's attributes. The size valve fits the tile's own features
//! first, exactly as without a fill; the fill then takes only the room they
//! leave, at most about a quarter of the size limit, shedding its smallest
//! pieces first ([`within_budget`]; `export.rs`, `with_fill`). It never makes
//! the valve merge or drop a real feature.
//!
//! Memory: seed and parent runs spill under the shared fill-workspace
//! grant. Each active tile preserves its f64 geometry and credit order.
//! Snapped payloads spill per zoom; only bounded indices remain resident.
//! A level group owns its zoom's run and drops it after writing. Each tile
//! lookup reads its own frame by position, without sharing a seek cursor.
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

// Resident buffer pricing, including unused Vec slots. The test probes
// exclude allocator metadata and overlay internals; dhat measures those.
fn shape_payload_bytes(shape: &Shape) -> usize {
    shape.capacity() * std::mem::size_of::<Ring>()
        + shape
            .iter()
            .map(|r| r.capacity() * std::mem::size_of::<[f64; 2]>())
            .sum::<usize>()
}

fn shape_capacity_bytes(shapes: &Vec<Shape>) -> usize {
    shapes.capacity() * std::mem::size_of::<Shape>()
        + shapes.iter().map(shape_payload_bytes).sum::<usize>()
}

fn polygon_capacity_bytes(polys: &Vec<FillPolygon>) -> usize {
    polys.capacity() * std::mem::size_of::<FillPolygon>()
        + polys
            .iter()
            .map(|p| {
                p.capacity() * std::mem::size_of::<TileRing>()
                    + p.iter()
                        .map(|r| r.capacity() * std::mem::size_of::<(i32, i32)>())
                        .sum::<usize>()
            })
            .sum::<usize>()
}

type SeedBuckets = std::collections::BTreeMap<u64, (Vec<Shape>, Vec<Collapsed>)>;

#[cfg(test)]
fn note_capacity(
    _stage: &str,
    buckets: &SeedBuckets,
    fills: &[(u64, Vec<Shape>)],
    map_bytes: usize,
) {
    let buckets_bytes: usize = buckets
        .values()
        .map(|(s, c)| shape_capacity_bytes(s) + c.capacity() * std::mem::size_of::<Collapsed>())
        .sum();
    let fills_bytes: usize = fills.iter().map(|(_, s)| shape_capacity_bytes(s)).sum();
    let live_bytes = buckets_bytes + fills_bytes + map_bytes;

    #[cfg(test)]
    cost_probe::update(|c| {
        c.bucket_peak = c.bucket_peak.max(buckets_bytes);
        c.map_peak = c.map_peak.max(map_bytes);
        c.live_peak = c.live_peak.max(live_bytes);
    });
}

#[cfg(test)]
pub(super) mod cost_probe {
    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct Counts {
        pub(crate) bucket_peak: usize,
        pub(crate) seed_windows: usize,
        pub(crate) pyramid_wave_tiles: usize,
        pub(crate) map_peak: usize,
        pub(crate) live_peak: usize,
        pub(crate) box_comparisons: usize,
        pub(crate) host_entries: usize,
        pub(crate) host_occupied: usize,
        pub(crate) large_host_visits: usize,
        pub(crate) holding_checks: usize,
        pub(crate) fill_polygons_encoded: usize,
        pub(crate) fill_passes: usize,
        pub(crate) fill_gzip_calls: usize,
        pub(crate) cached_gzip_rechecks: usize,
    }
    thread_local! {
        static COUNTS: std::cell::Cell<Counts> = std::cell::Cell::new(Counts::default());
    }
    pub(crate) fn update(f: impl FnOnce(&mut Counts)) {
        COUNTS.with(|cell| {
            let mut c = cell.get();
            f(&mut c);
            cell.set(c);
        });
    }
    pub(crate) fn take() -> Counts {
        COUNTS.with(|cell| cell.replace(Counts::default()))
    }
}

struct FillSeed<'a> {
    geometry: Option<&'a str>,
    crs: super::Crs,
    seed_zoom: u8,
    opts: &'a super::ExportOptions,
    timers: &'a super::ExportTimers,
    diagnostics: &'a crate::diagnostics::Diagnostics,
}

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
            // Distance to the segment, not its infinite line, so a vertex far
            // past either end is kept (tippecanoe's `square_distance_from_line`).
            let t = if len2 == 0.0 {
                0.0
            } else {
                ((rx * dx + ry * dy) / len2).clamp(0.0, 1.0)
            };
            let (ex, ey) = (rx - t * dx, ry - t * dy);
            let d2 = ex * ex + ey * ey;
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

/// Which vertices of the closed ring `r` survive Douglas–Peucker at `tol2`
/// (squared pixels), and the index of the vertex farthest from the first
/// (the ring's long axis runs from vertex 0 to it). `None` under 3 points.
fn simplify_mask(r: &[[f64; 2]], tol2: f64) -> Option<(Vec<bool>, usize)> {
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
    keep.truncate(r.len());
    Some((keep, far))
}

/// The kept vertices of `r`, or `None` once they no longer enclose any area.
fn kept_ring(r: &[[f64; 2]], keep: &[bool]) -> Option<Ring> {
    let out: Ring = r
        .iter()
        .zip(keep)
        .filter(|(_, k)| **k)
        .map(|(p, _)| *p)
        .collect();
    (out.len() >= 3 && ring_area2(&out) != 0.0).then_some(out)
}

/// One closed ring simplified at `tol2` (squared pixels); `None` once it no
/// longer encloses any area.
#[cfg(test)]
fn simplify_ring(r: &[[f64; 2]], tol2: f64) -> Option<Ring> {
    let (keep, _) = simplify_mask(r, tol2)?;
    kept_ring(r, &keep)
}

/// Credit `area` along the segment `a`→`b`, in equal shares one `step`
/// apart (at least one share), so the squares it buys land where the area
/// was rather than at one point.
fn credit_along(a: [f64; 2], b: [f64; 2], area: f64, step: f64, out: &mut Vec<Collapsed>) {
    let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
    let n = ((len / step).ceil() as usize).max(1);
    for i in 0..n {
        let t = (i as f64 + 0.5) / n as f64;
        out.push(Collapsed {
            area: area / n as f64,
            at: [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t],
        });
    }
}

/// The area Douglas–Peucker took from (positive) or added to (negative) the
/// shape at each run of vertices it removed from `r`: the signed area
/// between the run and the chord that replaced it, credited along the run.
/// `sign` is `1.0` for an exterior and `-1.0` for a hole, whose lost area
/// the shape gains. The credits sum to the change in the shape's area.
fn credit_shaved(r: &[[f64; 2]], keep: &[bool], sign: f64, step: f64, out: &mut Vec<Collapsed>) {
    let n = r.len();
    let orient = ring_area2(r).signum();
    let kept: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
    if kept.len() < 2 {
        return;
    }
    for (k, &a) in kept.iter().enumerate() {
        let b = kept[(k + 1) % kept.len()];
        let removed: Vec<usize> = if b > a {
            (a + 1..b).collect()
        } else {
            (a + 1..n).chain(0..b).collect()
        };
        if removed.is_empty() {
            continue;
        }
        let mut run = Vec::with_capacity(removed.len() + 2);
        run.push(r[a]);
        run.extend(removed.iter().map(|&i| r[i]));
        run.push(r[b]);
        let area = sign * orient * ring_area2(&run) / 2.0;
        if area != 0.0 {
            credit_run(&run, area, step, out);
        }
    }
}

/// Credit `area`, the area between a removed `run` and its chord (the run's
/// first and last points), along the run: each segment gets the trapezoid
/// between it and its projection on the chord, so the pieces sum to the
/// run's area and land where it was. Every removed vertex lies within the
/// simplification tolerance of the chord, so each share stays small: a
/// lone vertex's area spreads along both its edges, not as a block of
/// squares at the vertex.
fn credit_run(run: &[[f64; 2]], area: f64, step: f64, out: &mut Vec<Collapsed>) {
    let (a, b) = (run[0], run[run.len() - 1]);
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = (dx * dx + dy * dy).sqrt();
    // In the chord's frame: distance along it, and height off it.
    let frame = |p: [f64; 2]| {
        let (px, py) = (p[0] - a[0], p[1] - a[1]);
        ((px * dx + py * dy) / len, (dx * py - dy * px) / len)
    };
    let pieces: Vec<f64> = run
        .windows(2)
        .map(|w| {
            let ((x0, h0), (x1, h1)) = (frame(w[0]), frame(w[1]));
            (x1 - x0) * (h0 + h1) / 2.0
        })
        .collect();
    let total: f64 = pieces.iter().sum();
    if len == 0.0 || total.abs() < 1e-12 {
        // A closed or degenerate run: along the run's whole span instead.
        credit_along(run[1], run[run.len() - 2], area, step, out);
        return;
    }
    for (w, piece) in run.windows(2).zip(&pieces) {
        if *piece != 0.0 {
            credit_along(w[0], w[1], area * piece / total, step, out);
        }
    }
}

/// Simplify every ring at `tol` pixels. Returns the simplified shapes and
/// the area the simplification lost or gained, as credits placed where it
/// happened: a collapsed shape's area along its long axis (an exterior
/// collapsing takes its holes with it), and each kept ring's shaved runs
/// along the run.
pub(super) fn simplify_coverage(shapes: Vec<Shape>, tol: f64) -> (Vec<Shape>, Vec<Collapsed>) {
    let tol2 = tol * tol;
    let mut out = Vec::with_capacity(shapes.len());
    let mut collapsed = Vec::new();
    for shape in shapes {
        let Some(e) = shape.first() else {
            continue;
        };
        let Some((keep, far)) = simplify_mask(e, tol2) else {
            continue; // under 3 points: no area
        };
        let Some(exterior) = kept_ring(e, &keep) else {
            let area = net_area(&shape).max(0.0);
            if area > 0.0 {
                credit_along(e[0], e[far], area, tol, &mut collapsed);
            }
            continue;
        };
        credit_shaved(e, &keep, 1.0, tol, &mut collapsed);
        // One winding for every exterior and the other for every hole: the
        // union's `NonZero` rule adds overlapping exteriors only when they
        // wind the same way, and source rings come in either.
        let orient = |mut r: Ring, exterior: bool| {
            if (ring_area2(&r) > 0.0) != exterior {
                r.reverse();
            }
            r
        };
        let mut kept = vec![orient(exterior, true)];
        for h in &shape[1..] {
            let Some((hkeep, hfar)) = simplify_mask(h, tol2) else {
                continue;
            };
            match kept_ring(h, &hkeep) {
                Some(hole) => {
                    credit_shaved(h, &hkeep, -1.0, tol, &mut collapsed);
                    kept.push(orient(hole, false));
                }
                // A hole that collapses is filled in: the shape gains its area.
                None => {
                    let area = ring_area2(h).abs() / 2.0;
                    if area > 0.0 {
                        credit_along(h[0], h[hfar], -area, tol, &mut collapsed);
                    }
                }
            }
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

/// The union of `shapes` clipped to the tile, in one `i_overlay` pass.
/// Every input is oriented (exteriors positive, holes negative), so the
/// `Positive` rule keeps what any exterior covers less its holes. Unlike
/// `NonZero` (as in `mvt.rs`), it does not fill the part of a hole that
/// simplification pushed outside its exterior.
fn union_clip(shapes: &[Shape]) -> Vec<Shape> {
    use i_overlay::core::fill_rule::FillRule;
    use i_overlay::core::overlay_rule::OverlayRule;
    use i_overlay::float::overlay::FloatOverlay;

    let e = FILL_EXTENT;
    let tile: Shape = vec![vec![[0.0, 0.0], [e, 0.0], [e, e], [0.0, e]]];
    FloatOverlay::with_subj_and_clip_custom(shapes, &[tile], Default::default(), Default::default())
        .overlay(OverlayRule::Intersect, FillRule::Positive)
}

/// Union shapes whose bounding boxes meet and clip them to the tile; a lone
/// shape inside the tile passes through untouched.
#[cfg(test)]
fn union_touching(shapes: Vec<Shape>) -> Vec<Shape> {
    union_within_workspace(shapes, usize::MAX).expect("unbounded test union")
}

/// Price retained geometry and the sweep index, then reserve overlay scratch
/// only for the component being overlaid. Components are consumed in the
/// original order; a single oversized component returns a typed budget error.
fn union_within_workspace(
    shapes: Vec<Shape>,
    budget: usize,
) -> Result<Vec<Shape>, super::ExportError> {
    let n = shapes.len();
    // Bounds Vec capacities for boxes, order, roots, active set, groups,
    // slots and component members, including unused slots during growth.
    let fixed = shapes
        .capacity()
        .saturating_mul(std::mem::size_of::<Shape>())
        .saturating_add(n.saturating_mul(256));
    let mut remaining = shapes.iter().map(shape_payload_bytes).sum::<usize>();
    require_fill_bytes(fixed.saturating_add(remaining), budget)?;
    let mut boxes: Vec<[f64; 4]> = shapes.iter().map(bbox).collect();
    // If all x intervals overlap, the x sweep compares every pair. A y
    // sweep can only reduce that work. Otherwise keep the original axis;
    // an average-span heuristic increased comparisons on Moldova.
    // Roots and component input order remain the original shape order.
    let shared_x = boxes
        .iter()
        .fold([f64::NEG_INFINITY, f64::INFINITY], |a, b| {
            [a[0].max(b[0]), a[1].min(b[2])]
        });
    if shared_x[0] <= shared_x[1] {
        // Transpose only the broad-phase boxes. Keep the hot sweep's fixed
        // indexes instead of paying dynamic axis lookup for every pair.
        // The square tile's inside test is symmetric in x and y too.
        for b in &mut boxes {
            b.swap(0, 1);
            b.swap(2, 3);
        }
    }
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
    // Sweep on the chosen axis; touching boxes join one group.
    let mut active: Vec<usize> = Vec::new();
    #[cfg(test)]
    let mut box_comparisons = 0usize;
    for &i in &order {
        let b = boxes[i];
        active.retain(|&j| boxes[j][2] >= b[0]);
        #[cfg(test)]
        {
            box_comparisons += active.len();
        }
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

    #[cfg(test)]
    cost_probe::update(|c| c.box_comparisons += box_comparisons);
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
    let mut output_bytes = 0usize;
    for group in groups.into_iter().filter(|g| !g.is_empty()) {
        if let [only] = group[..] {
            if inside(boxes[only]) {
                let shape = slots[only].take().expect("one component member");
                let bytes = shape_payload_bytes(&shape);
                remaining -= bytes;
                output_bytes += bytes;
                out.push(shape);
                continue;
            }
        }
        let members: Vec<Shape> = group.iter().filter_map(|&i| slots[i].take()).collect();
        let vertices = members.iter().flatten().map(Vec::len).sum::<usize>();
        require_fill_bytes(
            fixed
                .saturating_add(remaining)
                .saturating_add(output_bytes)
                .saturating_add(vertices.saturating_mul(128)),
            budget,
        )?;
        let united = union_clip(&members);
        remaining =
            remaining.saturating_sub(members.iter().map(shape_payload_bytes).sum::<usize>());
        drop(members);
        output_bytes += united.iter().map(shape_payload_bytes).sum::<usize>();
        require_fill_bytes(
            fixed
                .saturating_add(remaining)
                .saturating_add(output_bytes)
                .saturating_add(out.capacity() * std::mem::size_of::<Shape>()),
            budget,
        )?;
        out.extend(united);
    }
    require_fill_bytes(shape_capacity_bytes(&out), budget)?;
    Ok(out)
}

/// How far, in cells, a remainder under one square carries to the next cell
/// that has credit; past that it is dropped.
const MAX_CARRY_CELLS: i64 = 8;

/// How far, in cells, a cell's excess looks for free cells to draw in.
const MAX_SPILL_CELLS: i64 = 16;

/// Placeholder squares for collapsed and shaved area, by error diffusion on
/// a `side` grid. Each cell sums the credits that fall in it. Cells are
/// visited row by row, alternating direction (rows with no credit are
/// skipped), with a running total: a cell draws a `side`-pixel square each
/// time the total reaches `side²`, in the cell itself if free, else in the
/// nearest free cell. A remainder under one square carries to the next cell
/// only within [`MAX_CARRY_CELLS`]; a deficit (filled concavities) is never
/// carried, so it cannot cancel squares elsewhere. Each isolated group of
/// credit loses less than one square of area.
///
/// DIVERGENCE FROM TIPPECANOE: `reduce_tiny_poly` (clip.cpp) emits a square
/// at the first vertex of the ring whose area crosses the running total, in
/// feature order, so squares can overlap; here they sit on a grid and never
/// do (#750).
#[cfg(test)]
pub(super) fn squares_for_collapsed(collapsed: &[Collapsed], side: f64) -> Vec<Shape> {
    let mut cells = CreditCells::with_side(side);
    for c in collapsed {
        add_credit(&mut cells, *c, side);
    }
    squares_for_cells(cells, side)
}

/// Sparse 16x16 pages on the existing cell grid. A page preserves each
/// cell's running f64 sum, and keys record zero-sum cells too. The maximum
/// payload at the production 4px scale is about 8 MiB for one active tile.
#[cfg_attr(test, derive(Debug))]
struct CreditPage {
    values: [f64; 256],
    present: [u64; 4],
}

#[cfg_attr(test, derive(Debug))]
struct CreditCells {
    width: usize,
    page_columns: usize,
    pages: Vec<Option<Box<CreditPage>>>,
    allocated_pages: usize,
    keys: Vec<usize>,
}

impl CreditCells {
    fn new() -> Self {
        Self::with_side(FILL_SIMPLIFY_PX)
    }
    fn with_side(side: f64) -> Self {
        let width = (FILL_EXTENT / side).ceil() as usize;
        Self {
            width,
            page_columns: width.div_ceil(16),
            pages: Vec::new(),
            allocated_pages: 0,
            keys: Vec::new(),
        }
    }
    fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
    fn capacity_bytes(&self) -> usize {
        self.pages.capacity() * std::mem::size_of::<Option<Box<CreditPage>>>()
            + self.allocated_pages * std::mem::size_of::<CreditPage>()
            + self.keys.capacity() * std::mem::size_of::<usize>()
    }
    /// Reserve a possible page, index allocation and key-vector growth
    /// before reading another credit. Existing cells need less than this.
    fn next_growth_bound(&self) -> usize {
        usize::from(self.pages.is_empty())
            * self.page_columns
            * self.page_columns
            * std::mem::size_of::<Option<Box<CreditPage>>>()
            + std::mem::size_of::<CreditPage>()
            + if self.keys.len() == self.keys.capacity() {
                self.keys.capacity().max(4) * std::mem::size_of::<usize>()
            } else {
                0
            }
    }
    fn add(&mut self, x: usize, y: usize, area: f64) {
        if self.pages.is_empty() {
            self.pages
                .resize_with(self.page_columns * self.page_columns, || None);
        }
        let page = (y / 16) * self.page_columns + x / 16;
        let page = self.pages[page].get_or_insert_with(|| {
            self.allocated_pages += 1;
            Box::new(CreditPage {
                values: [0.0; 256],
                present: [0; 4],
            })
        });
        let slot = (y % 16) * 16 + x % 16;
        let mask = 1u64 << (slot % 64);
        if page.present[slot / 64] & mask == 0 {
            page.present[slot / 64] |= mask;
            self.keys.push(y * self.width + x);
        }
        page.values[slot] += area;
    }
    fn into_sorted(mut self) -> Vec<((i64, i64), f64)> {
        self.keys.sort_unstable();
        self.keys
            .into_iter()
            .map(|key| {
                let (x, y) = (key % self.width, key / self.width);
                let page = self.pages[(y / 16) * self.page_columns + x / 16]
                    .as_ref()
                    .expect("credited page");
                (
                    ((y as i64), (x as i64)),
                    page.values[(y % 16) * 16 + x % 16],
                )
            })
            .collect()
    }
}

fn add_credit(cells: &mut CreditCells, c: Collapsed, side: f64) {
    let last = (FILL_EXTENT / side).ceil() as i64 - 1;
    let x = ((c.at[0] / side).floor() as i64).clamp(0, last);
    let y = ((c.at[1] / side).floor() as i64).clamp(0, last);
    cells.add(x as usize, y as usize, c.area);
}

fn squares_for_cells(cells: CreditCells, side: f64) -> Vec<Shape> {
    let threshold = side * side;
    let last = (FILL_EXTENT / side).ceil() as i64 - 1;
    // Each cell received the original addition sequence. Restore the exact
    // (row, column) traversal before the unchanged error diffusion.
    let cells = cells.into_sorted();
    // Alternate the direction of each row that has credit, so the carry
    // moves on to a nearby cell at the end of a row too.
    let mut order: Vec<((i64, i64), f64)> = Vec::with_capacity(cells.len());
    let mut row: Vec<((i64, i64), f64)> = Vec::new();
    let mut rows = 0usize;
    let mut flush = |row: &mut Vec<((i64, i64), f64)>, rows: &mut usize| {
        if *rows % 2 == 1 {
            row.reverse();
        }
        order.append(row);
        *rows += 1;
    };
    for ((y, x), area) in cells {
        if row.last().is_some_and(|&((_, ry), _)| ry != y) {
            flush(&mut row, &mut rows);
        }
        row.push(((x, y), area));
    }
    flush(&mut row, &mut rows);

    let mut occupied: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut total = 0.0;
    let mut prev: Option<(i64, i64)> = None;
    for ((x, y), area) in order {
        if prev.is_some_and(|(px, py)| (x - px).abs().max((y - py).abs()) > MAX_CARRY_CELLS) {
            total = 0.0;
        }
        prev = Some((x, y));
        total += area;
        if total <= 0.0 {
            total = 0.0; // a deficit never travels
            continue;
        }
        // A hair under the threshold still counts: sums of exact areas can
        // land a rounding error short.
        while total >= threshold * (1.0 - 1e-9) {
            let Some((sx, sy)) = nearest_free((x, y), &occupied, last) else {
                total = 0.0; // nothing free nearby: the excess is dropped
                break;
            };
            occupied.insert((sx, sy));
            let (px, py) = (sx as f64 * side, sy as f64 * side);
            out.push(vec![vec![
                [px, py],
                [px + side, py],
                [px + side, py + side],
                [px, py + side],
            ]]);
            total -= threshold;
        }
    }
    out
}

/// The free cell nearest `c` within [`MAX_SPILL_CELLS`] and the tile's
/// cells `0..=last`: `c` itself, then ring by ring, each ring in row order.
fn nearest_free(
    c: (i64, i64),
    occupied: &std::collections::HashSet<(i64, i64)>,
    last: i64,
) -> Option<(i64, i64)> {
    for r in 0..=MAX_SPILL_CELLS.min(last) {
        for dy in -r..=r {
            for dx in -r..=r {
                if dx.abs().max(dy.abs()) != r {
                    continue;
                }
                let (x, y) = (c.0 + dx, c.1 + dy);
                if (0..=last).contains(&x) && (0..=last).contains(&y) && !occupied.contains(&(x, y))
                {
                    return Some((x, y));
                }
            }
        }
    }
    None
}

/// One tile's fill from the shapes lifted into it: simplify, union where
/// shapes touch, simplify the union, and return collapsed area as squares.
#[cfg(test)]
pub(super) fn fill_for_tile(lifted: Vec<Shape>) -> Vec<Shape> {
    fill_for_tile_timed(lifted, &super::ExportTimers::default(), usize::MAX)
        .expect("unlimited test workspace")
}

/// [`fill_for_tile`], charging its stages to the export's fill timers.
fn fill_for_tile_timed(
    lifted: Vec<Shape>,
    timers: &super::ExportTimers,
    budget: usize,
) -> Result<Vec<Shape>, super::ExportError> {
    let (shapes, collapsed) = super::ExportTimers::time(&timers.fill_simplify, || {
        simplify_coverage(lifted, FILL_SIMPLIFY_PX)
    });
    fill_from_timed(shapes, collapsed, timers, budget)
}

/// The tail of [`fill_for_tile`] for shapes already simplified at this
/// tile's scale, with the area that collapsed (or was shaved) on the way.
#[cfg(test)]
fn fill_from(shapes: Vec<Shape>, collapsed: Vec<Collapsed>) -> Vec<Shape> {
    fill_from_timed(
        shapes,
        collapsed,
        &super::ExportTimers::default(),
        usize::MAX,
    )
    .expect("unlimited test workspace")
}

/// [`fill_from`], charging its stages to the export's fill timers.
fn fill_from_timed(
    shapes: Vec<Shape>,
    mut collapsed: Vec<Collapsed>,
    timers: &super::ExportTimers,
    budget: usize,
) -> Result<Vec<Shape>, super::ExportError> {
    let mut cells = CreditCells::new();
    for c in collapsed.drain(..) {
        add_credit(&mut cells, c, FILL_SIMPLIFY_PX);
    }
    fill_from_cells_timed(shapes, cells, timers, budget)
}

fn fill_from_cells_timed(
    shapes: Vec<Shape>,
    mut cells: CreditCells,
    timers: &super::ExportTimers,
    budget: usize,
) -> Result<Vec<Shape>, super::ExportError> {
    use super::ExportTimers as T;
    let united = T::time(&timers.fill_union, || {
        union_within_workspace(shapes, budget.saturating_sub(cells.capacity_bytes()))
    })?;
    let (mut fill, more) = T::time(&timers.fill_simplify, || {
        simplify_coverage(united, FILL_SIMPLIFY_PX)
    });
    // Add each new credit to the original per-cell running sum. Combining
    // partial sums here would change f64 rounding and therefore coverage.
    let fixed = shape_capacity_bytes(&fill)
        .saturating_add(more.capacity() * std::mem::size_of::<Collapsed>());
    for c in more {
        require_fill_bytes(
            fixed
                .saturating_add(cells.capacity_bytes())
                .saturating_add(cells.next_growth_bound()),
            budget,
        )?;
        add_credit(&mut cells, c, FILL_SIMPLIFY_PX);
    }
    require_fill_bytes(
        shape_capacity_bytes(&fill)
            .saturating_add(cells.capacity_bytes())
            .saturating_add(cells.keys.len().saturating_mul(48)),
        budget,
    )?;
    fill.extend(T::time(&timers.fill_squares, || {
        squares_for_cells(cells, FILL_SIMPLIFY_PX)
    }));
    // Squares sit on a grid, so a block of them touch: dissolve them with
    // each other and with the shapes they touch. A block of squares is then
    // one shape, not a 4-vertex ring per cell, which cuts the vertices the
    // fill carries on scattered sub-pixel fields by a third (FTW Moldova).
    // Rectangles from the grid alone are cheaper to build but leave the
    // squares beside the shapes, and the larger fill sent the size valve to
    // 15 passes per tile.
    T::time(&timers.fill_union, || union_within_workspace(fill, budget))
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

/// The MVT command bytes a fill polygon encodes to: per ring a `MoveTo`, a
/// `LineTo` with its vertex count and a `ClosePath` command, and each
/// vertex's zigzag-encoded delta from the one before as varints. The first
/// point's delta runs from wherever the last ring ended, unknown here, so it
/// is counted at the most a tile coordinate takes. Dissolved shapes have long
/// edges, so counting vertices alone would let them overrun the budget. The
/// feature's tags are not counted: they are its host's, which the tile
/// already carries.
fn est_bytes(poly: &FillPolygon) -> usize {
    let varint = |d: i32| -> usize {
        let z = ((d << 1) ^ (d >> 31)) as u32;
        match z {
            0..=0x7f => 1,
            0x80..=0x3fff => 2,
            0x4000..=0x1f_ffff => 3,
            _ => 4,
        }
    };
    poly.iter()
        .map(|r| {
            let mut prev = r.first().copied().unwrap_or((0, 0));
            // MoveTo, LineTo and its count, ClosePath; the first point.
            let count = if r.len() > 15 { 2 } else { 1 };
            let mut bytes = 3 + count + 2 * 3;
            for &p in r.iter().skip(1) {
                bytes += varint(p.0 - prev.0) + varint(p.1 - prev.1);
                prev = p;
            }
            bytes
        })
        .sum()
}

/// A fill polygon's area in tile units², exterior minus holes.
pub(super) fn poly_area(poly: &FillPolygon) -> f64 {
    let ring = |r: &TileRing| {
        let mut a = 0i64;
        for k in 0..r.len() {
            let (p, q) = (r[k], r[(k + 1) % r.len()]);
            a += i64::from(p.0) * i64::from(q.1) - i64::from(q.0) * i64::from(p.1);
        }
        (a as f64 / 2.0).abs()
    };
    poly.first().map_or(0.0, ring) - poly.iter().skip(1).map(ring).sum::<f64>()
}

/// The fill polygons to draw within about `budget` bytes, largest first,
/// returned in their original order, and how many were shed. The fill must
/// not crowd out the tile's own features: past its budget the smallest
/// pieces (placeholder squares first) go before the valve has to merge or
/// drop real data.
pub(super) fn within_budget(polys: &[FillPolygon], budget: usize) -> (Vec<usize>, usize) {
    let total: usize = polys.iter().map(est_bytes).sum();
    if total <= budget {
        return ((0..polys.len()).collect(), 0);
    }
    let areas: Vec<f64> = polys.iter().map(poly_area).collect();
    let mut by_size: Vec<usize> = (0..polys.len()).collect();
    by_size.sort_by(|&a, &b| areas[b].total_cmp(&areas[a]).then(a.cmp(&b)));
    let mut used = 0;
    let mut kept = Vec::new();
    for i in by_size {
        let b = est_bytes(&polys[i]);
        if used + b <= budget {
            used += b;
            kept.push(i);
        }
    }
    kept.sort_unstable();
    let shed = polys.len() - kept.len();
    (kept, shed)
}

/// For each fill polygon, the index of the feature box it joins: the
/// smallest of `hosts` (tile-unit `[min_x, min_y, max_x, max_y]`) holding the
/// polygon's first vertex, else the nearest. Tippecanoe likewise gives a
/// coalesced feature the attributes of the feature it joins (`tile.cpp`,
/// `find_feature_to_accumulate_onto`); its choice is the previous feature in
/// spatial order, ours is spatial containment. `hosts` must not be empty.
pub(super) fn assign_hosts<'a>(
    polys: impl IntoIterator<Item = &'a FillPolygon, IntoIter: ExactSizeIterator>,
    hosts: &[[f64; 4]],
) -> Vec<usize> {
    let polys = polys.into_iter();
    const CELLS: usize = 64;
    let size = FILL_EXTENT / CELLS as f64;
    let cell_of = |v: f64| ((v / size).floor().max(0.0) as usize).min(CELLS - 1);
    // Keep the ordinary small-box grid. A host covering more than one
    // row's worth of cells is stored once in `large`, with its cell bounds.
    // Queries still visit the original cell/ring candidate sets, so host
    // ties and early-exit behavior are unchanged.
    let cells: Vec<[u8; 4]> = hosts.iter().map(|b| b.map(|v| cell_of(v) as u8)).collect();
    let mut grid: Vec<Vec<usize>> = Vec::new();
    let mut large = Vec::new();
    for (h, bounds) in cells.iter().enumerate() {
        let [x0, y0, x1, y1] = bounds.map(usize::from);
        if x0 > x1 || y0 > y1 {
            continue;
        }
        if (x1 - x0 + 1) * (y1 - y0 + 1) > CELLS {
            large.push(h);
            continue;
        }
        if grid.is_empty() {
            grid.resize_with(CELLS * CELLS, Vec::new);
        }
        for cy in y0..=y1 {
            for cx in x0..=x1 {
                grid[cy * CELLS + cx].push(h);
            }
        }
    }
    #[cfg(test)]
    {
        let entries = grid.iter().map(Vec::len).sum::<usize>() + large.len();
        let mut used = [false; CELLS * CELLS];
        for (cell, hosts) in used.iter_mut().zip(&grid) {
            *cell = !hosts.is_empty();
        }
        for &h in &large {
            let [x0, y0, x1, y1] = cells[h].map(usize::from);
            for cy in y0..=y1 {
                used[cy * CELLS + x0..=cy * CELLS + x1].fill(true);
            }
        }
        let occupied = used.iter().filter(|&&c| c).count();

        #[cfg(test)]
        cost_probe::update(|c| {
            c.host_entries += entries;
            c.host_occupied += occupied;
        });
    }
    let dist2 = |b: &[f64; 4], x: f64, y: f64| {
        let dx = (b[0] - x).max(0.0).max(x - b[2]);
        let dy = (b[1] - y).max(0.0).max(y - b[3]);
        dx * dx + dy * dy
    };
    let area = |b: &[f64; 4]| (b[2] - b[0]) * (b[3] - b[1]);
    polys
        .map(|poly| {
            let (x, y) = (f64::from(poly[0][0].0), f64::from(poly[0][0].1));
            let (cx, cy) = (cell_of(x), cell_of(y));
            // Containment first, smallest box winning, ties to the lower index.
            let holding = grid
                .get(cy * CELLS + cx)
                .into_iter()
                .flatten()
                .copied()
                .chain(large.iter().copied().filter(|&h| {
                    let [x0, y0, x1, y1] = cells[h].map(usize::from);
                    x0 <= cx && cx <= x1 && y0 <= cy && cy <= y1
                }))
                .inspect(|_| {
                    #[cfg(test)]
                    cost_probe::update(|c| c.holding_checks += 1);
                })
                .filter(|&h| dist2(&hosts[h], x, y) == 0.0)
                .min_by(|&a, &b| area(&hosts[a]).total_cmp(&area(&hosts[b])).then(a.cmp(&b)));
            if let Some(h) = holding {
                return h;
            }
            // Else the nearest box, over growing rings of cells. Any box
            // found within ring `r` is at most `r + 1` cells away, so one
            // more ring settles it.
            let mut best: Option<(f64, usize)> = None;
            let consider = |best: &mut Option<(f64, usize)>, h: usize| {
                let d = dist2(&hosts[h], x, y);
                if best.is_none_or(|(bd, bh)| d < bd || (d == bd && h < bh)) {
                    *best = Some((d, h));
                }
            };
            for r in 0..CELLS {
                let (x0, x1) = (cx.saturating_sub(r), (cx + r).min(CELLS - 1));
                let (y0, y1) = (cy.saturating_sub(r), (cy + r).min(CELLS - 1));
                for gy in y0..=y1 {
                    for gx in x0..=x1 {
                        if gy != y0 && gy != y1 && gx != x0 && gx != x1 {
                            continue; // inside: seen at a smaller ring
                        }
                        for &h in grid.get(gy * CELLS + gx).into_iter().flatten() {
                            consider(&mut best, h);
                        }
                    }
                }
                for &h in &large {
                    let [hx0, hy0, hx1, hy1] = cells[h].map(usize::from);
                    let intersects = hx0 <= x1 && x0 <= hx1 && hy0 <= y1 && y0 <= hy1;
                    let boundary = (hx0 <= x0 && x0 <= hx1)
                        || (hx0 <= x1 && x1 <= hx1)
                        || (hy0 <= y0 && y0 <= hy1)
                        || (hy0 <= y1 && y1 <= hy1);
                    if intersects && boundary {
                        #[cfg(test)]
                        cost_probe::update(|c| c.large_host_visits += 1);
                        consider(&mut best, h);
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

/// The fill builder's workspace grant (#788): what
/// [`build_fill_map`] asks of the export's memory budget under
/// [`MemoryPhase::Fill`](crate::overview::memory::MemoryPhase), and the
/// least it can run on.
// Room for concurrent tile jobs and the snapping/lifting reserved during
// ordered commit. MemoryBudget::fit may grant less; bounded uses the floor.
pub(super) const WORKSPACE_WANT_BYTES: usize = 32 * WORKSPACE_FLOOR_BYTES;
/// See [`WORKSPACE_WANT_BYTES`].
pub(super) const WORKSPACE_FLOOR_BYTES: usize = 32 << 20;

/// Every zoom owns its index and spill run. A group moves its zoom out and
/// drops both when written; the map never caches snapped polygon payloads.
pub(super) struct FillMap {
    zooms: std::collections::BTreeMap<u8, ZoomFill>,
    counts: std::sync::Arc<FillCounts>,
    built: usize,
}

#[derive(Default)]
#[expect(
    clippy::disallowed_types,
    reason = "#779: per-export fill accounting shared by level groups"
)]
struct FillCounts {
    used: std::sync::atomic::AtomicUsize,
    shed: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    reads: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    resident: [std::sync::atomic::AtomicUsize; 32],
}

/// Fixed charge covers the file, path, vector and map node. Payloads live
/// on disk. The active writer additionally owns its 8 KiB buffered output.
const ZOOM_INDEX_FIXED_BYTES: usize = 1024;
const ZOOM_WRITER_FIXED_BYTES: usize = ZOOM_INDEX_FIXED_BYTES + 8192;

struct FillEntry {
    key: u64,
    offset: usize,
    len: usize,
}

struct ZoomFill {
    data: crate::archive_index::FileBytes,
    _path: tempfile::TempPath,
    index: Vec<FillEntry>,
    #[cfg(test)]
    zoom: u8,
    #[cfg(test)]
    counts: std::sync::Arc<FillCounts>,
}

impl ZoomFill {
    fn resident_bytes(&self) -> usize {
        ZOOM_INDEX_FIXED_BYTES + self.index.capacity() * std::mem::size_of::<FillEntry>()
    }
}

#[cfg(test)]
impl Drop for ZoomFill {
    fn drop(&mut self) {
        self.counts.resident[usize::from(self.zoom)]
            .fetch_sub(self.resident_bytes(), std::sync::atomic::Ordering::Relaxed);
    }
}

struct ZoomBuilder {
    writer: std::io::BufWriter<tempfile::NamedTempFile>,
    index: Vec<FillEntry>,
    offset: usize,
}

impl ZoomBuilder {
    fn new(opts: &super::ExportOptions) -> Result<Self, super::ExportError> {
        Ok(Self {
            writer: std::io::BufWriter::new(super::super::pipeline::spill_temp_file(
                opts.spill_dir.as_deref(),
            )?),
            index: Vec::new(),
            offset: 0,
        })
    }
    fn resident_bytes(&self) -> usize {
        ZOOM_WRITER_FIXED_BYTES + self.index.capacity() * std::mem::size_of::<FillEntry>()
    }
    fn append(
        &mut self,
        key: u64,
        polys: &Vec<FillPolygon>,
        reserved: usize,
        budget: usize,
    ) -> Result<(), super::ExportError> {
        use std::io::Write;
        let cap = if self.index.len() == self.index.capacity() {
            self.index.capacity().max(2).saturating_mul(2)
        } else {
            self.index.capacity()
        };
        require_fill_bytes(
            reserved
                .saturating_add(ZOOM_WRITER_FIXED_BYTES)
                .saturating_add(cap.saturating_mul(std::mem::size_of::<FillEntry>()))
                .saturating_add(polygon_capacity_bytes(polys)),
            budget,
        )?;
        debug_assert!(self.index.last().is_none_or(|e| e.key < key));
        let len = 8 + polys
            .iter()
            .map(|p| 8 + p.iter().map(|r| 8 + r.len() * 8).sum::<usize>())
            .sum::<usize>();
        self.writer.write_all(&(polys.len() as u64).to_le_bytes())?;
        let mut buffer = [0u8; 8192];
        for poly in polys {
            self.writer.write_all(&(poly.len() as u64).to_le_bytes())?;
            for ring in poly {
                self.writer.write_all(&(ring.len() as u64).to_le_bytes())?;
                for chunk in ring.chunks(1024) {
                    for (&(x, y), bytes) in chunk.iter().zip(buffer.as_chunks_mut::<8>().0) {
                        bytes[..4].copy_from_slice(&x.to_le_bytes());
                        bytes[4..].copy_from_slice(&y.to_le_bytes());
                    }
                    self.writer.write_all(&buffer[..chunk.len() * 8])?;
                }
            }
        }
        self.index.push(FillEntry {
            key,
            offset: self.offset,
            len,
        });
        self.offset += len;
        Ok(())
    }
    fn finish(
        self,
        zoom: u8,
        counts: &std::sync::Arc<FillCounts>,
    ) -> Result<Option<ZoomFill>, super::ExportError> {
        if self.index.is_empty() {
            return Ok(None);
        }
        let path = self
            .writer
            .into_inner()
            .map_err(|e| e.into_error())?
            .into_temp_path();
        let data = crate::archive_index::FileBytes::open(&path)?;
        let run = ZoomFill {
            data,
            _path: path,
            index: self.index,
            #[cfg(test)]
            zoom,
            #[cfg(test)]
            counts: std::sync::Arc::clone(counts),
        };
        #[cfg(test)]
        counts.resident[usize::from(zoom)]
            .fetch_add(run.resident_bytes(), std::sync::atomic::Ordering::Relaxed);
        #[cfg(not(test))]
        let _ = (zoom, counts);
        Ok(Some(run))
    }
}

/// Decode one complete positional-read frame; framing errors are never a
/// missing tile. Coordinates and ring/polygon order are copied verbatim.
fn decode_fill(mut data: &[u8]) -> Result<Vec<FillPolygon>, std::io::Error> {
    fn invalid() -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid snapped fill spill frame",
        )
    }
    fn count(data: &mut &[u8]) -> Result<usize, std::io::Error> {
        let head = data.get(..8).ok_or_else(invalid)?;
        let n = usize::try_from(u64::from_le_bytes(
            head.try_into().expect("eight-byte count"),
        ))
        .map_err(|_| invalid())?;
        *data = &data[8..];
        if n > data.len() / 8 {
            return Err(invalid());
        }
        Ok(n)
    }
    let n = count(&mut data)?;
    let mut polys = Vec::with_capacity(n);
    for _ in 0..n {
        let n = count(&mut data)?;
        let mut poly = Vec::with_capacity(n);
        for _ in 0..n {
            let n = count(&mut data)?;
            let (points, rest) = data.split_at(n * 8);
            let ring = points
                .as_chunks::<8>()
                .0
                .iter()
                .map(|p| {
                    (
                        i32::from_le_bytes(p[..4].try_into().expect("four-byte x")),
                        i32::from_le_bytes(p[4..].try_into().expect("four-byte y")),
                    )
                })
                .collect();
            poly.push(ring);
            data = rest;
        }
        polys.push(poly);
    }
    if !data.is_empty() {
        return Err(invalid());
    }
    Ok(polys)
}

impl FillMap {
    fn resident_bytes(&self) -> usize {
        self.zooms.values().map(ZoomFill::resident_bytes).sum()
    }
    fn tile_count(&self) -> usize {
        self.zooms.values().map(|r| r.index.len()).sum()
    }
    /// Move exactly one zoom's index and spill run to its level group.
    pub(super) fn take_zoom(&mut self, zoom: u8) -> Option<FillMap> {
        let run = self.zooms.remove(&zoom)?;
        Some(Self {
            zooms: std::collections::BTreeMap::from([(zoom, run)]),
            counts: std::sync::Arc::clone(&self.counts),
            built: 0,
        })
    }
    /// One positional read for a present tile, using the shared file reader
    /// owner. No seek cursor or decoded payload is shared across workers.
    pub(super) fn get(&self, key: u64) -> Result<Option<Vec<FillPolygon>>, super::ExportError> {
        use crate::pmtiles_writer::ArchiveBytes;
        let Some((run, entry)) = self.zooms.values().find_map(|r| {
            r.index
                .binary_search_by_key(&key, |e| e.key)
                .ok()
                .map(|i| (r, &r.index[i]))
        }) else {
            return Ok(None);
        };
        #[cfg(any(unix, windows))]
        let bytes = run.data.read_range(entry.offset, entry.len)?;
        // FileBytes' portable fallback uses a cursor. Open an independent
        // reader on those targets so workers never share that cursor.
        #[cfg(not(any(unix, windows)))]
        let reader = crate::archive_index::FileBytes::open(&run._path)?;
        #[cfg(not(any(unix, windows)))]
        let bytes = reader.read_range(entry.offset, entry.len)?;
        #[cfg(test)]
        self.counts
            .reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let polys = decode_fill(&bytes)?;
        self.counts
            .used
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(Some(polys))
    }
    pub(super) fn note_shed(&self, n: usize) {
        self.counts
            .shed
            .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for FillMap {
    fn drop(&mut self) {
        if self.built == 0 {
            return;
        }
        let used = self.counts.used.load(std::sync::atomic::Ordering::Relaxed);
        let shed = self.counts.shed.load(std::sync::atomic::Ordering::Relaxed);
        log::info!("[export] coverage fill: {} fill tiles built, {used} drawn into tiles with features, {shed} fill polygons shed to fit tile size limits",self.built);
    }
}

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

fn require_fill_bytes(needed: usize, available: usize) -> Result<(), super::ExportError> {
    if needed > available {
        return Err(super::ExportError::BudgetExceeded {
            phase: "coverage fill",
            needed: needed as u64,
            available: available as u64,
        });
    }
    Ok(())
}

/// Stable per-tile runs. Each frame appends shapes and raw credit triples in
/// input order. Only one tile is materialized during pyramid construction;
/// credits are folded directly into their cell's running sum while reading.
/// The codec stores f64 bits, with no coordinate or area quantization.
struct SpillBuckets {
    tiles: std::collections::BTreeMap<u64, tempfile::TempPath>,
    spill_dir: Option<std::path::PathBuf>,
    budget: usize,
}

impl SpillBuckets {
    fn new(opts: &super::ExportOptions, budget: usize) -> Self {
        Self {
            tiles: Default::default(),
            spill_dir: opts.spill_dir.clone(),
            budget,
        }
    }

    fn append(
        &mut self,
        key: u64,
        shapes: &[Shape],
        credits: &[Collapsed],
        reserved: usize,
    ) -> Result<(), super::ExportError> {
        use std::io::Write;
        // Include a conservative allowance for each tree node, path and file.
        require_fill_bytes(
            (self.tiles.len() + 1)
                .saturating_mul(512)
                .saturating_add(reserved),
            self.budget,
        )?;
        if !self.tiles.contains_key(&key) {
            self.tiles.insert(
                key,
                super::super::pipeline::spill_temp_file(self.spill_dir.as_deref())?
                    .into_temp_path(),
            );
        }
        let path = self.tiles.get(&key).expect("inserted spill tile");
        let file = std::fs::OpenOptions::new().append(true).open(path)?;
        let mut w = std::io::BufWriter::new(file);
        w.write_all(&(shapes.len() as u64).to_le_bytes())?;
        for shape in shapes {
            w.write_all(&(shape.len() as u64).to_le_bytes())?;
            for ring in shape {
                w.write_all(&(ring.len() as u64).to_le_bytes())?;
                let mut buffer = [0u8; 8192];
                for points in ring.chunks(512) {
                    for (point, bytes) in
                        points.iter().zip(buffer.as_chunks_mut::<16>().0.iter_mut())
                    {
                        bytes[..8].copy_from_slice(&point[0].to_bits().to_le_bytes());
                        bytes[8..].copy_from_slice(&point[1].to_bits().to_le_bytes());
                    }
                    w.write_all(&buffer[..points.len() * 16])?;
                }
            }
        }
        w.write_all(&(credits.len() as u64).to_le_bytes())?;
        let mut buffer = [0u8; 8184];
        for chunk in credits.chunks(341) {
            for (credit, bytes) in chunk.iter().zip(buffer.as_chunks_mut::<24>().0.iter_mut()) {
                for (value, word) in [credit.area, credit.at[0], credit.at[1]]
                    .iter()
                    .zip(bytes.as_chunks_mut::<8>().0.iter_mut())
                {
                    word.copy_from_slice(&value.to_bits().to_le_bytes());
                }
            }
            w.write_all(&buffer[..chunk.len() * 24])?;
        }
        w.flush()?;
        Ok(())
    }

    #[cfg(test)]
    fn pop_first(
        &mut self,
        reserved: usize,
    ) -> Result<Option<(u64, Vec<Shape>, CreditCells)>, super::ExportError> {
        let Some((key, path)) = self.tiles.pop_first() else {
            return Ok(None);
        };
        let (shapes, cells) = Self::read_run(
            &path,
            reserved.saturating_add(self.tiles.len().saturating_mul(512)),
            self.budget,
        )?;
        Ok(Some((key, shapes, cells)))
    }

    fn read_run(
        path: &std::path::Path,
        reserved: usize,
        budget: usize,
    ) -> Result<(Vec<Shape>, CreditCells), super::ExportError> {
        use std::io::Read;
        fn word(r: &mut impl Read) -> std::io::Result<u64> {
            let mut b = [0; 8];
            r.read_exact(&mut b)?;
            Ok(u64::from_le_bytes(b))
        }
        let mut r = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut shapes = Vec::new();
        let mut cells = CreditCells::new();
        let mut price = reserved;
        loop {
            // Read the first byte separately: only a frame boundary can be EOF.
            let mut first = [0; 1];
            if r.read(&mut first)? == 0 {
                break;
            }
            let mut rest = [0; 7];
            r.read_exact(&mut rest)?;
            let mut count = [0; 8];
            count[0] = first[0];
            count[1..].copy_from_slice(&rest);
            for _ in 0..u64::from_le_bytes(count) {
                let rings = word(&mut r)? as usize;
                price = price.saturating_add(48 + rings.saturating_mul(24));
                require_fill_bytes(price.saturating_add(cells.capacity_bytes()), budget)?;
                let mut shape = Vec::with_capacity(rings);
                for _ in 0..rings {
                    let points = word(&mut r)? as usize;
                    // Retain this tile's input; union_within_workspace grants
                    // overlay scratch one component at a time, not per tile.
                    price = price.saturating_add(points.saturating_mul(16));
                    require_fill_bytes(price.saturating_add(cells.capacity_bytes()), budget)?;
                    let mut ring = Vec::with_capacity(points);
                    let mut buffer = [0u8; 8192];
                    let mut remaining = points;
                    while remaining > 0 {
                        let count = remaining.min(512);
                        r.read_exact(&mut buffer[..count * 16])?;
                        ring.extend(
                            buffer[..count * 16]
                                .as_chunks::<16>()
                                .0
                                .iter()
                                .map(|bytes| {
                                    [
                                        f64::from_le_bytes(
                                            bytes[..8].try_into().expect("eight-byte word"),
                                        ),
                                        f64::from_le_bytes(
                                            bytes[8..].try_into().expect("eight-byte word"),
                                        ),
                                    ]
                                }),
                        );
                        remaining -= count;
                    }
                    shape.push(ring);
                }
                shapes.push(shape);
            }
            let mut remaining = word(&mut r)?;
            let mut buffer = [0u8; 8184];
            while remaining > 0 {
                let count = remaining.min(341) as usize;
                r.read_exact(&mut buffer[..count * 24])?;
                for bytes in buffer[..count * 24].as_chunks::<24>().0 {
                    let number = |bytes: &[u8]| {
                        f64::from_le_bytes(bytes.try_into().expect("eight-byte word"))
                    };
                    let credit = Collapsed {
                        area: number(&bytes[..8]),
                        at: [number(&bytes[8..16]), number(&bytes[16..])],
                    };
                    require_fill_bytes(
                        price
                            .saturating_add(cells.capacity_bytes())
                            .saturating_add(cells.next_growth_bound()),
                        budget,
                    )?;
                    add_credit(&mut cells, credit, FILL_SIMPLIFY_PX);
                }
                remaining -= count as u64;
            }
        }
        Ok((shapes, cells))
    }
}

/// One independent pyramid tile. Its workspace is a share of the original
/// Fill grant; retained indexes and other wave outputs are charged separately.
fn pyramid_tile(
    path: &std::path::Path,
    seed: bool,
    timers: &super::ExportTimers,
    budget: usize,
    reserved: usize,
) -> Result<Vec<Shape>, super::ExportError> {
    let (shapes, cells) = SpillBuckets::read_run(path, 0, budget)?;
    #[cfg(test)]
    {
        let input_bytes = shape_capacity_bytes(&shapes) + cells.capacity_bytes();
        cost_probe::update(|c| {
            c.bucket_peak = c.bucket_peak.max(input_bytes);
            c.live_peak = c.live_peak.max(input_bytes + reserved);
        });
    }
    #[cfg(not(test))]
    let _ = reserved;
    if seed {
        fill_from_cells_timed(shapes, cells, timers, budget)
    } else {
        debug_assert!(cells.is_empty());
        fill_for_tile_timed(shapes, timers, budget)
    }
}

fn pyramid_lanes(available: usize) -> usize {
    (available / WORKSPACE_FLOOR_BYTES)
        .saturating_sub(2)
        .max(1)
        .min(rayon::current_num_threads())
}

fn pyramid_wave(
    jobs: &[(u64, tempfile::TempPath)],
    seed: bool,
    timers: &super::ExportTimers,
    available: usize,
    reserved: usize,
) -> Result<Option<Vec<Vec<Shape>>>, super::ExportError> {
    use rayon::prelude::*;
    let parallel = if jobs.len() > 1 {
        let share = available / (jobs.len() + 2);
        let result: Result<Vec<_>, super::ExportError> = jobs
            .par_iter()
            .map(|(_, path)| pyramid_tile(path, seed, timers, share, reserved))
            .collect();
        match result {
            Ok(fills) => Some(fills),
            // A tile may fit the whole grant but not a parallel
            // share. Drop every wave output and retry serially from
            // the unchanged runs; a true oversized tile still errors.
            Err(super::ExportError::BudgetExceeded { .. }) => None,
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    #[cfg(test)]
    cost_probe::update(|c| {
        c.pyramid_wave_tiles = c
            .pyramid_wave_tiles
            .max(parallel.as_ref().map_or(1, Vec::len));
    });
    Ok(parallel)
}

/// One seed batch: every polygon clipped into its tiles at `seed_zoom`,
/// projected to tile pixels and simplified, appended to its tile's bucket in
/// file order.
fn seed_batch(
    batch: &super::RecordBatch,
    inputs: FillSeed<'_>,
    buckets: &mut SpillBuckets,
    features: &mut usize,
) -> Result<(), super::ExportError> {
    let FillSeed {
        geometry,
        crs,
        seed_zoom,
        opts,
        timers,
        diagnostics,
    } = inputs;
    use rayon::prelude::*;
    type Bucket = (Vec<Shape>, Vec<Collapsed>);
    // The whole batch is the fill's seed time; its decode is not the
    // export's own `decode` stage.
    let _seed = super::ExportTimers::scope(&timers.fill_seed);
    #[cfg(test)]
    cost_probe::update(|c| c.seed_windows += 1);
    let schema = batch.schema();
    let geom_idx = super::batch_geometry_index(batch, geometry)?;
    let geoms = super::decode_batch_geometries(
        batch,
        &schema,
        geom_idx,
        crs,
        &super::ExportTimers::default(),
        diagnostics,
    )?;
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
            super::feature_tile_members(g, seed_zoom, opts, 0, u64::MAX, diagnostics)
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
    let live_bytes: usize = pieces
        .iter()
        .flatten()
        .map(|(_, (shapes, credits))| {
            shape_capacity_bytes(shapes) + credits.capacity() * std::mem::size_of::<Collapsed>()
        })
        .sum();
    require_fill_bytes(
        live_bytes.saturating_add(buckets.tiles.len() * 512),
        buckets.budget,
    )?;
    #[cfg(test)]
    cost_probe::update(|c| {
        c.bucket_peak = c.bucket_peak.max(live_bytes);
        c.live_peak = c.live_peak.max(live_bytes);
    });
    // One frame per tile/window, rather than per feature. Both shape and
    // credit sequences remain in feature order; write order between tiles
    // is immaterial. This avoids reopening and buffering the same run for
    // every feature in a Hilbert-local batch.
    let mut pending = SeedBuckets::new();
    for (key, (shapes, credits)) in pieces.into_iter().flatten() {
        let bucket = pending.entry(key).or_default();
        bucket.0.extend(shapes);
        bucket.1.extend(credits);
    }
    let pending_bytes: usize = pending
        .values()
        .map(|(shapes, credits)| {
            shape_capacity_bytes(shapes) + credits.capacity() * std::mem::size_of::<Collapsed>()
        })
        .sum();
    require_fill_bytes(
        pending_bytes.saturating_add(buckets.tiles.len() * 512),
        buckets.budget,
    )?;
    #[cfg(test)]
    cost_probe::update(|c| {
        c.bucket_peak = c.bucket_peak.max(pending_bytes);
        c.live_peak = c.live_peak.max(pending_bytes);
    });
    for (key, (shapes, credits)) in pending {
        buckets.append(key, &shapes, &credits, 0)?;
    }
    Ok(())
}

/// Build the [`FillMap`] for zooms `min_zoom..finest_zoom` from the level
/// at `level_idx`, the one exported at `finest_zoom`.
///
/// Each of its polygons is clipped into the tiles one zoom below the finest
/// (the export's own clip), projected to tile pixels and simplified there;
/// each zoom's fills are then built and lifted into the next, coarsest last.
pub(super) fn build_fill_map(
    level: &super::ExportLevelCtx<'_>,
    min_zoom: u8,
    workspace_bytes: u64,
    timers: &super::ExportTimers,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<FillMap, super::ExportError> {
    let reader = level.reader;
    let level_idx = level.level_idx;
    let crs = level.crs;
    let geometry = level.published.geometry.as_deref();
    let finest_zoom = level.plan.zoom;
    let opts = level.options;
    let mut map = FillMap {
        zooms: Default::default(),
        counts: Default::default(),
        built: 0,
    };
    // #788: the workspace the export's budget granted; F's spilling
    // builder holds its seed and pyramid within it.
    log::debug!(
        "[profile] coverage fill workspace grant: {} MiB",
        workspace_bytes >> 20
    );
    if finest_zoom == 0 || min_zoom >= finest_zoom {
        return Ok(map);
    }
    let seed_zoom = finest_zoom - 1;
    let t_seed = std::time::Instant::now();

    // Seed: per tile at `seed_zoom`, its polygons' shapes and collapsed area,
    // in the order the level stores them.
    let workspace = usize::try_from(workspace_bytes).unwrap_or(usize::MAX);
    let mut buckets = SpillBuckets::new(opts, workspace);
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
            // Only the geometry is needed: project the read to it, over the
            // level's whole render set (its prefix in partitioning mode).
            let batches = match reader.resolved_schema().geometry_index() {
                Some(root) => reader.read_row_groups_projected(
                    reader.selected_row_groups(level_idx, None)?,
                    super::EXPORT_BATCH_SIZE,
                    &[root],
                )?,
                None => reader.read_level_batched(level_idx, None, super::EXPORT_BATCH_SIZE)?,
            };
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
                // Bound the parallel clip/simplify results as well as the
                // retained runs. Slicing preserves Arrow and feature order.
                // The 32 MiB floor keeps the original 128-row window. A
                // larger grant amortizes Rayon dispatch and run opens; the
                // same byte guards still check the materialized capacities.
                let rows = 128usize
                    .saturating_mul((workspace / WORKSPACE_FLOOR_BYTES).max(1))
                    .min(super::EXPORT_BATCH_SIZE);
                for offset in (0..batch.num_rows()).step_by(rows) {
                    seed_batch(
                        &batch.slice(offset, (batch.num_rows() - offset).min(rows)),
                        FillSeed {
                            geometry,
                            crs,
                            seed_zoom,
                            opts: &seed_opts,
                            timers,
                            diagnostics,
                        },
                        buckets_ref,
                        features_ref,
                    )?;
                }
            }
            Ok(())
        },
    )?;
    #[cfg(test)]
    note_capacity("seed", &SeedBuckets::new(), &[], map.resident_bytes());
    let seed_secs = t_seed.elapsed().as_secs_f64();

    // Pyramid: each zoom's fills, then lifted into the zoom above.
    let t_pyramid = std::time::Instant::now();
    let scale = f64::from(opts.extent) / FILL_EXTENT;
    let mut vertices = 0usize;
    let mut zoom = seed_zoom;
    loop {
        let t_zoom = std::time::Instant::now();
        let mut parents = SpillBuckets::new(opts, workspace);
        let mut snapped = ZoomBuilder::new(opts)?;
        let mut tile_count = 0usize;
        let mut area = 0.0;
        while !buckets.tiles.is_empty() {
            let fixed = map.resident_bytes()
                + snapped.resident_bytes()
                + (buckets.tiles.len() + parents.tiles.len()) * 512;
            let available = workspace.saturating_sub(fixed);
            // Each concurrent tile gets at least the floor. Reserve two
            // additional shares for snapping/lifting during ordered commit,
            // while the other wave outputs are still resident. Bounded's
            // floor-sized grant keeps its original serial heap envelope.
            let lanes = pyramid_lanes(available);
            let jobs: Vec<_> = (0..lanes)
                .filter_map(|_| buckets.tiles.pop_first())
                .collect();
            let parallel = pyramid_wave(&jobs, zoom == seed_zoom, timers, available, fixed)?;
            let mut held = parallel.as_ref().map_or(0, |fills| {
                fills.iter().map(shape_capacity_bytes).sum::<usize>()
            });
            let mut parallel = parallel.map(Vec::into_iter);
            let job_count = jobs.len();
            // Indexed collection and ordered commit preserve every tile,
            // shape and parent-run frame order, including f64 credit sums.
            for (job, (key, path)) in jobs.into_iter().enumerate() {
                let path_bytes = (job_count - job) * 512;
                let reserved = map.resident_bytes()
                    + snapped.resident_bytes()
                    + (buckets.tiles.len() + parents.tiles.len()) * 512
                    + path_bytes;
                let fill = if let Some(fills) = &mut parallel {
                    let fill = fills.next().expect("one result per pyramid job");
                    held -= shape_capacity_bytes(&fill);
                    fill
                } else {
                    pyramid_tile(
                        &path,
                        zoom == seed_zoom,
                        timers,
                        workspace.saturating_sub(reserved),
                        reserved,
                    )?
                };
                let fills = [(key, fill)];
                let fill = &fills[0].1;
                #[cfg(test)]
                note_capacity(
                    "unioned",
                    &SeedBuckets::new(),
                    &fills,
                    map.resident_bytes() + snapped.resident_bytes() + held + path_bytes,
                );
                if !fill.is_empty() {
                    let polys = to_tile_polygons(fill, scale);
                    if !polys.is_empty() {
                        snapped.append(
                            key,
                            &polys,
                            map.resident_bytes()
                                + (buckets.tiles.len() + parents.tiles.len()) * 512
                                + shape_capacity_bytes(fill)
                                + held
                                + path_bytes,
                            workspace,
                        )?;
                    }
                }
                #[cfg(test)]
                note_capacity(
                    "snapped",
                    &SeedBuckets::new(),
                    &fills,
                    map.resident_bytes() + snapped.resident_bytes() + held + path_bytes,
                );
                tile_count += 1;
                area += fill.iter().map(|s| net_area(s)).sum::<f64>();
                if zoom > min_zoom {
                    vertices += fill.iter().flatten().map(Vec::len).sum::<usize>();
                    let (x, y) = super::key_to_xy(key, zoom);
                    let parent = crate::pmtiles_writer::tile_id(zoom - 1, x / 2, y / 2);
                    let mut lifted = Vec::new();
                    lift_into_parent(fill, x & 1, y & 1, &mut lifted);
                    parents.append(
                        parent,
                        &lifted,
                        &[],
                        buckets
                            .tiles
                            .len()
                            .saturating_mul(512)
                            .saturating_add(shape_capacity_bytes(&fills[0].1))
                            .saturating_add(shape_capacity_bytes(&lifted))
                            .saturating_add(map.resident_bytes())
                            .saturating_add(snapped.resident_bytes())
                            .saturating_add(held)
                            .saturating_add(path_bytes),
                    )?;
                }
            }
        }
        log::debug!(
            "[export] coverage fill z{zoom}: {tile_count} tiles, area {:.0} seed px², {:.3}s",
            area * 4f64.powi(i32::from(seed_zoom - zoom)),
            t_zoom.elapsed().as_secs_f64()
        );
        if let Some(run) = snapped.finish(zoom, &map.counts)? {
            map.zooms.insert(zoom, run);
        }
        if zoom == min_zoom {
            break;
        }
        buckets = parents;
        zoom -= 1;
    }
    log::info!(
        "[export] coverage fill (#750, experimental): seed z{seed_zoom} from {features} features \
         in {seed_secs:.2}s; z{min_zoom}..=z{seed_zoom} built in {:.2}s; {} fill tiles, \
         {vertices} vertices lifted",
        t_pyramid.elapsed().as_secs_f64(),
        map.tile_count(),
    );

    map.built = map.tile_count();

    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_fill_map(tiles: impl IntoIterator<Item = (u8, u64, Vec<FillPolygon>)>) -> FillMap {
        let opts = super::super::ExportOptions::default();
        let mut map = FillMap {
            zooms: Default::default(),
            counts: Default::default(),
            built: 0,
        };
        let mut active = None;
        for (zoom, key, polys) in tiles {
            if active.as_ref().is_some_and(|(z, _)| *z != zoom) {
                let (z, writer): (u8, ZoomBuilder) = active.take().unwrap();
                map.zooms
                    .insert(z, writer.finish(z, &map.counts).unwrap().unwrap());
            }
            let (_, writer) =
                active.get_or_insert_with(|| (zoom, ZoomBuilder::new(&opts).unwrap()));
            writer
                .append(key, &polys, map.resident_bytes(), WORKSPACE_FLOOR_BYTES)
                .unwrap();
        }
        if let Some((z, writer)) = active {
            map.zooms
                .insert(z, writer.finish(z, &map.counts).unwrap().unwrap());
        }
        map
    }

    #[test]
    fn phase3_retained_map_fits_workspace_floor() {
        let first = crate::pmtiles_writer::tile_id(5, 0, 0);
        let map = test_fill_map((0..80).map(|i| {
            (
                5,
                first + i,
                vec![vec![(0..65536).map(|n| (n, -n)).collect()]],
            )
        }));
        assert!(
            map.resident_bytes() <= WORKSPACE_FLOOR_BYTES,
            "retained map uses {} bytes",
            map.resident_bytes()
        );
    }

    #[test]
    fn phase3_one_positioned_read_per_present_tile() {
        use rayon::prelude::*;
        let first = crate::pmtiles_writer::tile_id(5, 0, 0);
        let map = test_fill_map((0..80).map(|i| {
            (
                5,
                first + i,
                vec![vec![vec![(i as i32, i32::MIN), (i32::MAX, 1), (0, 2)]]],
            )
        }));
        (0..80u64).into_par_iter().for_each(|i| {
            assert_eq!(
                map.get(first + i).unwrap(),
                Some(vec![vec![vec![
                    (i as i32, i32::MIN),
                    (i32::MAX, 1),
                    (0, 2)
                ]]])
            )
        });
        assert!(map.get(first + 80).unwrap().is_none());
        assert_eq!(
            map.counts.reads.load(std::sync::atomic::Ordering::Relaxed),
            80
        );
    }

    /// #788: the fixed fixture fits within the 32 MiB workspace floor.
    /// The production grant may be larger; 32 MiB is not its cap.
    /// These capacity observations are a lower bound on peak live heap:
    /// exceeding them proves the budget is exceeded without an RSS threshold.
    fn fixture_fill_counts(profile: crate::overview::level::MemoryProfile) -> cost_probe::Counts {
        use crate::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
        let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/realdata/ftw-moldova-z10.parquet");
        let dir = tempfile::tempdir().expect("fixture workspace");
        let overview = dir.path().join("overview.parquet");
        convert_to_overviews(
            input,
            &overview,
            &ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 12,
                },
                ..Default::default()
            },
        )
        .expect("convert the fixed Moldova fixture");
        cost_probe::take();
        super::super::export_pmtiles_run(
            overview,
            dir.path().join("out.pmtiles"),
            &super::super::ExportOptions {
                coverage_fill: true,
                profile,
                ..Default::default()
            },
            super::super::DEFAULT_PARTITION_TARGET,
            &super::super::Pass2Knobs {
                force_legacy_pass2: false,
                backing_override: None,
                concurrent_levels: true,
                checkpoint_interval: super::super::CHECKPOINT_INTERVAL,
                fail_level: None,
                memory_limit: None,
                // Pin the grant; this work counter must not depend on the
                // host's available RAM or another test's memory pressure.
                budget_available: Some(4 << 30),
            },
        )
        .expect("export with fill");
        cost_probe::take()
    }

    #[test]
    fn phase1_fill_prepass_respects_live_byte_budget() {
        let counts = fixture_fill_counts(crate::overview::level::MemoryProfile::Bounded);
        eprintln!("prepass retained capacity: {counts:?}");
        assert!(counts.bucket_peak > 0 && counts.map_peak > 0);
        assert!(
            counts.live_peak <= 32 * 1024 * 1024,
            "32 MiB workspace exceeded (map nodes and union scratch excluded): {counts:?}"
        );
    }

    /// A large Fill grant must buy tile concurrency and amortize seed
    /// dispatch, rather than retain the floor's serial/small-window shape.
    #[test]
    fn phase5_auto_fill_uses_granted_parallelism() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        pool.install(|| {
            let counts = fixture_fill_counts(crate::overview::level::MemoryProfile::Auto);
            assert!(counts.pyramid_wave_tiles >= 2, "serial pyramid: {counts:?}");
            assert!(counts.seed_windows <= 4, "tiny seed dispatches: {counts:?}");
        });
    }

    #[test]
    fn pyramid_wave_share_overflow_keeps_runs_for_serial_retry() {
        let opts = super::super::ExportOptions::default();
        let mut runs = SpillBuckets::new(&opts, WORKSPACE_FLOOR_BYTES);
        for key in 1..=2 {
            runs.append(key, &[square(10.0, 10.0, 100.0)], &[], 0)
                .unwrap();
        }
        let jobs: Vec<_> = runs.tiles.into_iter().collect();
        let timers = super::super::ExportTimers::default();
        // A deliberately tiny share fails before consuming either run.
        assert!(pyramid_wave(&jobs, true, &timers, 512, 0)
            .unwrap()
            .is_none());
        for (_, path) in jobs {
            assert_eq!(
                pyramid_tile(&path, true, &timers, WORKSPACE_FLOOR_BYTES, 0).unwrap(),
                fill_from(vec![square(10.0, 10.0, 100.0)], Vec::new()),
            );
        }
    }

    /// #767: east-west strips share x but have disjoint y ranges. Pin the
    /// work, rather than time, so a broad-phase index can replace the sweep
    /// while preserving every polygon and its order.
    #[test]
    fn phase1_wide_strip_union_comparisons_stay_linear() {
        let n = 1024usize;
        let strips: Vec<Shape> = (0..n)
            .map(|i| {
                let y = (i * 3) as f64;
                vec![vec![
                    [0.0, y],
                    [FILL_EXTENT, y],
                    [FILL_EXTENT, y + 1.0],
                    [0.0, y + 1.0],
                ]]
            })
            .collect();
        cost_probe::take();
        let result = union_touching(strips.clone());
        let counts = cost_probe::take();
        assert_eq!(result, strips, "disjoint strips retain their exact order");
        assert!(
            counts.box_comparisons <= n * 32,
            "wide-strip broad phase exceeded 32 comparisons per shape: {counts:?}"
        );
    }

    #[test]
    fn phase1_tile_spanning_hosts_have_bounded_grid_entries() {
        let hosts = vec![[0.0, 0.0, FILL_EXTENT, FILL_EXTENT]; 128];
        let polys = vec![vec![vec![(50, 50), (60, 50), (60, 60)]]];
        cost_probe::take();
        assert_eq!(assign_hosts(&polys, &hosts), vec![0]);
        let counts = cost_probe::take();
        assert_eq!(counts.host_occupied, 4096);
        assert!(
            counts.host_entries <= hosts.len() * 64,
            "spanning hosts must not replicate across all 4096 cells: {counts:?}"
        );
    }

    #[test]
    fn spill_preserves_coordinate_bits_and_per_cell_credit_addition_order() {
        let dir = tempfile::tempdir().unwrap();
        let opts = super::super::ExportOptions {
            spill_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let workspace = WORKSPACE_FLOOR_BYTES;
        let mut runs = SpillBuckets::new(&opts, workspace);
        let shapes = vec![square(-0.0, 0.125, 11.25)];
        for area in [1e16, -1e16, 1.0] {
            runs.append(
                7,
                &shapes,
                &[Collapsed {
                    area,
                    at: [3.0, 2.0],
                }],
                0,
            )
            .unwrap();
        }
        let (key, got, cells) = runs.pop_first(0).unwrap().unwrap();
        assert_eq!(key, 7);
        assert_eq!(got.len(), 3);
        for shape in got {
            for (p, q) in shape.iter().flatten().zip(shapes[0].iter().flatten()) {
                assert_eq!(p.map(f64::to_bits), q.map(f64::to_bits));
            }
        }
        assert_eq!(cells.into_sorted()[0].1.to_bits(), 1.0f64.to_bits());
        assert!(runs.pop_first(0).unwrap().is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn fill_workspace_budget_error_is_not_io() {
        let error = require_fill_bytes(33, 32).unwrap_err();
        assert_eq!(error.to_string(), "io error: coverage fill needs 33 bytes but only 32 bytes are available; use a bigger machine or reduce the input with --bbox/--filter.");
        assert!(matches!(
            error,
            super::super::ExportError::BudgetExceeded {
                phase: "coverage fill",
                needed: 33,
                available: 32,
            }
        ));
        assert!(std::error::Error::source(&error).is_none());
    }

    #[test]
    fn oversized_union_component_returns_a_budget_error() {
        let error = union_within_workspace(vec![square(0.0, 0.0, 10.0)], 32).unwrap_err();
        assert!(matches!(
            error,
            super::super::ExportError::BudgetExceeded {
                phase: "coverage fill",
                available: 32,
                ..
            }
        ));
    }

    #[test]
    fn oversized_fill_run_returns_the_typed_budget_error() {
        let dir = tempfile::tempdir().unwrap();
        let opts = super::super::ExportOptions {
            spill_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let workspace = WORKSPACE_FLOOR_BYTES;
        let mut runs = SpillBuckets::new(&opts, workspace);
        runs.append(9, &[square(0.0, 0.0, 10.0)], &[], 0).unwrap();
        runs.budget = 32;
        assert!(matches!(
            runs.pop_first(0).unwrap_err(),
            super::super::ExportError::BudgetExceeded {
                phase: "coverage fill",
                available: 32,
                ..
            }
        ));
    }

    /// #788: a level takes only its own zoom's fill out of the map, the
    /// rest stays for the finer levels, and a lookup hands the caller its
    /// own copy.
    #[test]
    fn take_zoom_moves_only_that_zooms_tiles() {
        let poly = |v: i32| -> FillPolygon { vec![vec![(v, 0), (v, 1), (v + 1, 1)]] };
        let key = |z: u8, x: u32| crate::pmtiles_writer::tile_id(z, x, 0);
        let mut tiles = vec![
            (2, key(2, 1), vec![poly(1)]),
            (2, key(2, 3), vec![poly(2)]),
            (3, key(3, 5), vec![poly(3)]),
        ];
        tiles.sort_by_key(|(_, key, _)| *key);
        let mut map = test_fill_map(tiles);
        let counts = std::sync::Arc::clone(&map.counts);
        assert!(counts.resident[2].load(std::sync::atomic::Ordering::Relaxed) > 0);
        assert!(map.take_zoom(1).is_none(), "no z1 fill");
        let z2 = map.take_zoom(2).expect("z2 fill");
        assert_eq!(z2.tile_count(), 2);
        assert_eq!(z2.get(key(2, 3)).unwrap(), Some(vec![poly(2)]));
        assert_eq!(z2.get(key(3, 5)).unwrap(), None, "z3 stays in the map");
        assert_eq!(map.tile_count(), 1);
        assert!(map.take_zoom(2).is_none(), "taken once");
        let paths: Vec<_> = z2
            .zooms
            .values()
            .map(|run| run._path.to_path_buf())
            .collect();
        assert!(
            !map.zooms.contains_key(&2),
            "ownership moved, with no retained zoom index"
        );
        drop(z2); // E1 drops the group when its level has been written.
        assert_eq!(
            counts.resident[2].load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(counts.resident[3].load(std::sync::atomic::Ordering::Relaxed) > 0);
        assert!(
            paths.iter().all(|path| !path.exists()),
            "completed zoom runs removed"
        );
        let z3 = map.take_zoom(3).expect("z3 fill");
        assert_eq!(z3.get(key(3, 5)).unwrap(), Some(vec![poly(3)]));
        assert!(map.zooms.is_empty());
        assert_eq!(
            map.counts.used.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "the zooms count into the map's totals"
        );
    }

    #[test]
    fn phase3_truncated_spill_is_an_error_not_missing_coverage() {
        let key = crate::pmtiles_writer::tile_id(2, 0, 0);
        let map = test_fill_map([(2, key, vec![vec![vec![(0, 0), (1, 0), (0, 1)]]])]);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&map.zooms[&2]._path)
            .unwrap()
            .set_len(0)
            .unwrap();
        assert!(map.get(key).is_err());
        for bytes in [vec![], vec![255; 8], vec![0; 9]] {
            assert!(decode_fill(&bytes).is_err());
        }
    }

    #[test]
    fn phase3_disjoint_components_do_not_reserve_combined_overlay_scratch() {
        let shapes: Vec<_> = (0..1024)
            .map(|i| square((i % 32) as f64 * 16.0, (i / 32) as f64 * 16.0, 8.0))
            .collect();
        let expected = shapes.clone();
        assert_eq!(
            union_within_workspace(shapes, 512 << 10).expect("individual components fit"),
            expected
        );
    }

    #[test]
    fn phase3_fill_grant_uses_the_shared_profile_policy() {
        use crate::overview::{
            level::MemoryProfile,
            memory::{MemoryBudget, MemoryPhase, Price, PriceBasis},
        };
        let mut budget = MemoryBudget::with_available(Some(4 << 30));
        for (profile, expected) in [
            (MemoryProfile::Bounded, WORKSPACE_FLOOR_BYTES),
            (MemoryProfile::Auto, WORKSPACE_WANT_BYTES),
            (MemoryProfile::Speed, WORKSPACE_WANT_BYTES),
        ] {
            let grant = budget.fit(
                profile,
                MemoryPhase::Fill,
                Price::new(1, PriceBasis::Measured),
                WORKSPACE_WANT_BYTES,
                WORKSPACE_FLOOR_BYTES,
            );
            assert_eq!(grant.bytes(), expected as u64);
            budget.release(grant);
        }
    }

    #[test]
    fn phase3_snapped_workspace_overflow_is_typed() {
        let opts = super::super::ExportOptions::default();
        let mut writer = ZoomBuilder::new(&opts).unwrap();
        let error = writer
            .append(1, &vec![vec![vec![(0, 0), (1, 0), (0, 1)]]], 0, 32)
            .unwrap_err();
        assert!(matches!(
            error,
            super::super::ExportError::BudgetExceeded {
                phase: "coverage fill",
                available: 32,
                ..
            }
        ));
        assert!(writer.index.is_empty());
    }

    #[test]
    fn shape_bytes_price_every_vector_slot() {
        let shapes = vec![square(0.0, 0.0, 4.0)];
        assert_eq!(
            shape_capacity_bytes(&shapes),
            std::mem::size_of::<Shape>()
                + std::mem::size_of::<Ring>()
                + 4 * std::mem::size_of::<[f64; 2]>()
        );
    }

    #[test]
    fn credit_cells_price_their_index_pages_and_keys() {
        let mut cells = CreditCells::with_side(4.0);
        cells.add(0, 0, 1.0);
        // 1024 cells a side: a 64 × 64 page index, one page, four key slots.
        assert_eq!(
            cells.capacity_bytes(),
            64 * 64 * std::mem::size_of::<Option<Box<CreditPage>>>()
                + std::mem::size_of::<CreditPage>()
                + 4 * std::mem::size_of::<usize>()
        );
    }

    #[test]
    fn a_corner_cells_spill_stays_inside_the_tile() {
        // Three squares' worth in the top-right cell: the spill takes the
        // free cells beside it inside the tile, never the column past it.
        let out = squares_for_collapsed(
            &[Collapsed {
                area: 48.0,
                at: [FILL_EXTENT, 0.0],
            }],
            4.0,
        );
        assert_eq!(
            out,
            vec![
                square(4092.0, 0.0, 4.0),
                square(4088.0, 0.0, 4.0),
                square(4088.0, 4.0, 4.0)
            ]
        );
    }

    #[test]
    fn a_fill_polygon_outside_every_box_reaches_a_large_box_to_its_left() {
        let polys = vec![vec![vec![(2000, 2000), (2010, 2000), (2010, 2010)]]];
        let hosts = [
            [3900.0, 2000.0, 4000.0, 2100.0],
            [0.0, 0.0, 640.0, FILL_EXTENT],
        ];
        assert_eq!(assign_hosts(&polys, &hosts), vec![1]);
    }

    /// The nearest-box search visits a large box only once its rings reach
    /// it: boxes beyond the ring that settles the search cost nothing.
    #[test]
    fn phase1_far_large_hosts_are_not_visited() {
        let polys = vec![vec![vec![(2000, 2000), (2010, 2000), (2010, 2010)]]];
        let mut hosts = vec![[0.0, 0.0, 1700.0, FILL_EXTENT]];
        hosts.extend([[3500.0, 0.0, 3560.0, FILL_EXTENT]; 8]);
        cost_probe::take();
        assert_eq!(assign_hosts(&polys, &hosts), vec![0]);
        assert_eq!(cost_probe::take().large_host_visits, 1);
    }

    #[test]
    fn of_several_large_boxes_holding_a_polygon_the_smallest_wins() {
        let polys = vec![vec![vec![(2000, 2000), (2010, 2000), (2010, 2010)]]];
        let hosts = [
            [0.0, 0.0, FILL_EXTENT, FILL_EXTENT],
            [1000.0, 1000.0, 3000.0, 3000.0],
        ];
        assert_eq!(assign_hosts(&polys, &hosts), vec![1]);
    }

    /// Containment tests only the large boxes whose cells hold the
    /// polygon's cell: a strip beside it is not measured.
    #[test]
    fn phase1_containment_measures_only_boxes_over_the_cell() {
        let polys = vec![vec![vec![(2000, 2000), (2010, 2000), (2010, 2010)]]];
        let hosts = [
            [0.0, 0.0, FILL_EXTENT, FILL_EXTENT],
            [0.0, 0.0, 640.0, FILL_EXTENT],
        ];
        cost_probe::take();
        assert_eq!(assign_hosts(&polys, &hosts), vec![0]);
        assert_eq!(cost_probe::take().holding_checks, 1);
    }

    #[test]
    fn a_nearer_box_keeps_a_polygon_from_a_lower_numbered_farther_one() {
        // The small box 1 is 14 px away, the large box 0 is 15 px away and
        // seen in the same ring after it.
        let polys = vec![vec![vec![(2000, 2000), (2010, 2000), (2010, 2010)]]];
        let hosts = [
            [0.0, 0.0, 1985.0, FILL_EXTENT],
            [2010.0, 2010.0, 2020.0, 2020.0],
        ];
        assert_eq!(assign_hosts(&polys, &hosts), vec![1]);
    }

    /// The ring search visits a large box only from the ring that first
    /// reaches it while one of the ring's edges crosses it.
    #[test]
    fn phase1_large_box_visits_follow_the_ring_edges() {
        let polys = vec![vec![vec![(2000, 2000), (2010, 2000), (2010, 2010)]]];
        // Bars 4 cells thick, left, right, below and above the polygon's
        // cell (31, 31), each 6 to 25 cells away, and one corner block.
        let hosts = [
            [320.0, 1920.0, 1663.0, 2175.0],
            [2432.0, 1920.0, 3775.0, 2175.0],
            [1920.0, 320.0, 2175.0, 1663.0],
            [1920.0, 2432.0, 2175.0, 3775.0],
            [2700.0, 2700.0, 3500.0, 3500.0],
        ];
        cost_probe::take();
        // The left and lower bars tie at 337 px, both first reached at
        // ring 5; the lower index wins and the search stops there.
        assert_eq!(assign_hosts(&polys, &hosts), vec![0]);
        assert_eq!(cost_probe::take().large_host_visits, 2);
    }

    /// The ring search stops visiting a large box once the ring has grown
    /// past it on every side: only rings whose edges cross it see it.
    #[test]
    fn phase1_a_box_the_ring_has_passed_is_not_revisited() {
        // The polygon sits in cell (2, 2); the box covers cells 27..=35 by
        // 27..=34, diagonal from it, so the search reaches it at ring 25,
        // the ring's right edge leaves it after ring 33 and its top after
        // ring 32, and the search runs on to ring 35 (2218 px away).
        let polys = vec![vec![vec![(160, 160), (170, 160), (170, 170)]]];
        let hosts = [[1728.0, 1728.0, 2303.0, 2239.0]];
        cost_probe::take();
        assert_eq!(assign_hosts(&polys, &hosts), vec![0]);
        assert_eq!(cost_probe::take().large_host_visits, 9);
    }

    #[test]
    fn a_zoom_writer_reserves_its_index_growth_before_appending() {
        let opts = super::super::ExportOptions::default();
        let polys = vec![vec![vec![(0, 0), (1, 0), (0, 1)]]];
        // The first append grows the empty index to four entries.
        let needed = ZOOM_WRITER_FIXED_BYTES
            + 4 * std::mem::size_of::<FillEntry>()
            + polygon_capacity_bytes(&polys);
        let mut writer = ZoomBuilder::new(&opts).unwrap();
        assert!(matches!(
            writer.append(1, &polys, 0, needed - 1),
            Err(super::super::ExportError::BudgetExceeded { .. })
        ));
        writer.append(1, &polys, 0, needed).unwrap();
        assert_eq!(writer.index.len(), 1);
    }

    #[test]
    fn fill_workspace_fits_exactly_at_its_budget() {
        require_fill_bytes(32, 32).unwrap();
    }

    /// One geometry-column batch of `geoms`, as the level read hands it.
    fn geometry_batch(geoms: &[geo::Geometry<f64>]) -> super::super::RecordBatch {
        use geoarrow::array::GeometryBuilder;
        use geoarrow::datatypes::GeometryType;
        use geoarrow_array::GeoArrowArray;
        let mut builder =
            GeometryBuilder::new(GeometryType::new(Default::default())).with_prefer_multi(false);
        builder.extend_from_iter(geoms.iter().map(Some));
        let array = builder.finish();
        let field = array.data_type().to_field("geometry", true);
        super::super::RecordBatch::try_new(
            std::sync::Arc::new(arrow_schema::Schema::new(vec![field])),
            vec![array.to_array_ref()],
        )
        .unwrap()
    }

    /// Seed `batch` at z4 into `buckets`; `Err(needed)` on a budget error.
    fn seed_z4(
        batch: &super::super::RecordBatch,
        buckets: &mut SpillBuckets,
        features: &mut usize,
    ) -> Result<(), u64> {
        let opts = super::super::ExportOptions {
            tile_buffer: 0,
            ..Default::default()
        };
        match seed_batch(
            batch,
            FillSeed {
                geometry: Some("geometry"),
                crs: super::super::Crs::Epsg4326,
                seed_zoom: 4,
                opts: &opts,
                timers: &super::super::ExportTimers::default(),
                diagnostics: &crate::diagnostics::Diagnostics::default(),
            },
            buckets,
            features,
        ) {
            Ok(()) => Ok(()),
            Err(super::super::ExportError::BudgetExceeded { needed, .. }) => Err(needed),
            Err(error) => panic!("expected a budget error, got {error:?}"),
        }
    }

    /// A 1° square in the z4 tile at (8, 7), far above the tolerance.
    fn degree_square(lon: f64, lat: f64) -> geo::Geometry<f64> {
        geo::Geometry::Polygon(
            geo::Rect::new(
                geo::coord! { x: lon, y: lat },
                geo::coord! { x: lon + 1.0, y: lat + 1.0 },
            )
            .to_polygon(),
        )
    }

    /// The seed charges a feature's collapsed credits as well as its shapes:
    /// a 400 px sliver that collapses whole is a hundred credits.
    #[test]
    fn the_seed_charges_a_collapsed_slivers_credits() {
        let sliver = geo::Geometry::Polygon(geo::Polygon::new(
            geo::LineString::from(vec![(1.0, 1.0), (3.2, 1.0), (3.2, 1.005), (1.0, 1.005)]),
            vec![],
        ));
        let opts = super::super::ExportOptions::default();
        let mut buckets = SpillBuckets::new(&opts, 0);
        let mut features = 0;
        let needed = seed_z4(&geometry_batch(&[sliver]), &mut buckets, &mut features)
            .expect_err("nothing fits a zero budget");
        assert!(
            needed >= 100 * std::mem::size_of::<Collapsed>() as u64,
            "needed {needed}"
        );
        assert_eq!(features, 1);
    }

    /// Each tile run the seed already holds is charged 512 bytes.
    #[test]
    fn the_seed_charges_each_held_tile_run() {
        let dir = tempfile::tempdir().unwrap();
        let opts = super::super::ExportOptions {
            spill_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        let batch = geometry_batch(&[degree_square(1.0, 1.0)]);
        let mut features = 0;
        let alone = seed_z4(&batch, &mut SpillBuckets::new(&opts, 0), &mut features)
            .expect_err("nothing fits a zero budget");
        let mut buckets = SpillBuckets::new(&opts, WORKSPACE_FLOOR_BYTES);
        seed_z4(&batch, &mut buckets, &mut features).unwrap();
        assert_eq!(buckets.tiles.len(), 1);
        buckets.budget = 0;
        let held = seed_z4(&batch, &mut buckets, &mut features).expect_err("zero budget");
        assert_eq!(held, alone + 512);
    }

    /// Two features of one tile are merged into one pending bucket before
    /// they are written, and that bucket is charged too: its shape vector
    /// grows to four slots where each feature held one.
    #[test]
    fn the_seed_charges_the_merged_tile_bucket() {
        let opts = super::super::ExportOptions::default();
        let batch = geometry_batch(&[degree_square(1.0, 1.0), degree_square(5.0, 5.0)]);
        let mut features = 0;
        let pieces = seed_z4(&batch, &mut SpillBuckets::new(&opts, 0), &mut features)
            .expect_err("nothing fits a zero budget");
        let pending = seed_z4(
            &batch,
            &mut SpillBuckets::new(&opts, usize::try_from(pieces).unwrap()),
            &mut features,
        )
        .expect_err("the merged bucket outgrows its pieces");
        assert_eq!(pending, pieces + 2 * std::mem::size_of::<Shape>() as u64);
    }

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
        assert!((credited + 50.0).abs() < 1e-9, "{credited}");
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
        // Credited along the ring's long axis, (10, 20)–(12, 22): one share
        // at its middle.
        assert_eq!(collapsed[0].at, [11.0, 21.0]);
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

    /// Simplification can push a hole near the boundary partly outside its
    /// exterior. Under `NonZero` that part (winding -1) was filled, drawing
    /// fill outside the coverage; a hole only ever removes area.
    #[test]
    fn union_does_not_fill_a_hole_pushed_outside_its_exterior() {
        let mut frame = square(100.0, 100.0, 100.0);
        // Exterior positive, hole negative: the hole pokes 20 px out of the
        // bottom edge.
        frame.push(vec![
            [140.0, 150.0],
            [140.0, 220.0],
            [160.0, 220.0],
            [160.0, 150.0],
        ]);
        assert!(ring_area2(&frame[0]) > 0.0 && ring_area2(&frame[1]) < 0.0);
        let neighbour = square(500.0, 500.0, 10.0);
        let out = union_clip(&[frame, neighbour.clone(), square(195.0, 100.0, 10.0)]);
        let area: f64 = out.iter().map(|s| net_area(s)).sum();
        // 100×100, less the hole's 20×50 inside it, plus the overlapping
        // 10×10 square's 5×10 outside, plus the neighbour.
        assert!(
            (area - (10_000.0 - 1_000.0 + 50.0 + 100.0)).abs() < 1e-6,
            "{area}"
        );
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
    fn a_cells_excess_spills_into_nearby_cells() {
        // Ten squares' worth credited at one point inside the tile: all ten
        // are drawn, in that cell (25, 25) and its nearest free neighbours.
        let out = squares_for_collapsed(
            &[Collapsed {
                area: 160.0,
                at: [101.0, 101.0],
            }],
            4.0,
        );
        assert_eq!(area(&out), 160.0);
        for s in &out {
            let (cx, cy) = (s[0][0][0] / 4.0, s[0][0][1] / 4.0);
            assert!(
                (cx - 25.0).abs() <= 2.0 && (cy - 25.0).abs() <= 2.0,
                "square at cell ({cx}, {cy}) is not nearby"
            );
        }
    }

    #[test]
    fn a_negative_credit_does_not_suppress_a_distant_square() {
        // A dent filled at (0, 0) must not cancel a collapsed parcel 200 px
        // away: that cancellation is exactly a #750 hole.
        let out = squares_for_collapsed(
            &[
                Collapsed {
                    area: -50.0,
                    at: [0.0, 0.0],
                },
                Collapsed {
                    area: 16.0,
                    at: [201.0, 1.0],
                },
            ],
            4.0,
        );
        assert_eq!(out, vec![square(200.0, 0.0, 4.0)]);
    }

    #[test]
    fn a_collapsed_strip_is_credited_along_its_length() {
        let strip: Shape = vec![vec![[1.0, 1.0], [129.0, 1.0], [129.0, 2.0], [1.0, 2.0]]];
        let (out, credit) = simplify_coverage(vec![strip], 4.0);
        assert!(out.is_empty());
        assert!((credit.iter().map(|c| c.area).sum::<f64>() - 128.0).abs() < 1e-9);
        let xs: Vec<f64> = credit.iter().map(|c| c.at[0]).collect();
        let (lo, hi) = xs
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
        assert!(
            lo < 10.0 && hi > 120.0,
            "credit spans {lo}..{hi}, not the strip's length"
        );
    }

    #[test]
    fn a_shaved_bump_is_credited_where_it_was() {
        // A 1000 px square with a 2 px bump at the middle of its bottom edge:
        // the bump's area is credited next to the bump, not at a corner.
        let s: Shape = vec![vec![
            [0.0, 0.0],
            [1000.0, 0.0],
            [1000.0, 1000.0],
            [510.0, 1000.0],
            [500.0, 1002.0],
            [490.0, 1000.0],
            [0.0, 1000.0],
        ]];
        let (_, credit) = simplify_coverage(vec![s], 4.0);
        assert!(!credit.is_empty());
        for c in &credit {
            assert!(
                (c.at[0] - 500.0).abs() <= 20.0 && (c.at[1] - 1000.0).abs() <= 4.0,
                "{c:?}"
            );
        }
        assert!((credit.iter().map(|c| c.area).sum::<f64>() - 20.0).abs() < 1e-9);
    }

    #[test]
    fn a_single_shaved_vertex_is_credited_along_its_edges() {
        // One vertex 3 px out of a 1000 px edge: 1500 px² shaved off. It is
        // credited in small shares along the edges it sat on, not as one
        // lump at the vertex (which drew a 40 px block of squares).
        let s: Shape = vec![vec![
            [0.0, 0.0],
            [1000.0, 0.0],
            [1000.0, 1000.0],
            [500.0, 1003.0],
            [0.0, 1000.0],
        ]];
        let (_, credit) = simplify_coverage(vec![s], 4.0);
        assert!((credit.iter().map(|c| c.area).sum::<f64>() - 1500.0).abs() < 1e-6);
        assert!(credit.len() > 100, "{} shares", credit.len());
        for c in &credit {
            assert!(c.area.abs() <= 2.0 * 16.0, "{c:?}");
            assert!((c.at[1] - 1000.0).abs() <= 4.0, "{c:?}");
        }
    }

    #[test]
    fn douglas_peucker_measures_distance_to_the_segment() {
        // (-60, -5) lies almost on the extension of the chord (100, 10)–(0, 0)
        // but 60 px from the chord itself, so it must be kept.
        let ring = vec![[0.0, 0.0], [100.0, 0.0], [100.0, 10.0], [-60.0, -5.0]];
        let out = simplify_ring(&ring, 16.0).expect("kept");
        assert!(out.contains(&[-60.0, -5.0]), "{out:?}");
    }

    #[test]
    fn a_fill_over_its_budget_sheds_its_smallest_pieces_first() {
        // One large polygon and a hundred placeholder squares, with room for
        // about ten pieces: the large one stays, the squares go first.
        let big: FillPolygon = vec![vec![(0, 0), (1000, 0), (1000, 1000), (0, 1000)]];
        let sq = |i: i32| -> FillPolygon {
            vec![vec![
                (i * 8, 2000),
                (i * 8 + 4, 2000),
                (i * 8 + 4, 2004),
                (i * 8, 2004),
            ]]
        };
        let mut polys: Vec<FillPolygon> = (0..100).map(sq).collect();
        polys.insert(50, big);
        // Room for the large polygon and nine squares.
        let budget = est_bytes(&polys[50]) + 9 * est_bytes(&polys[0]);
        let (kept, shed) = within_budget(&polys, budget);
        assert_eq!(kept.len(), 10);
        assert_eq!(shed, 91);
        assert!(kept.contains(&50), "the large polygon is kept");
        assert!(kept.windows(2).all(|w| w[0] < w[1]), "original order kept");
        assert_eq!(within_budget(&polys, usize::MAX), ((0..101).collect(), 0));
    }

    #[test]
    fn adjacent_squares_are_dissolved_into_one_shape() {
        // Three squares' worth credited in a row of cells: drawn as one
        // 12×4 rectangle, not three 4-vertex rings.
        let collapsed: Vec<Collapsed> = [1.0, 5.0, 9.0]
            .iter()
            .map(|&x| Collapsed {
                area: 16.0,
                at: [x, 1.0],
            })
            .collect();
        let fill = fill_from(Vec::new(), collapsed);
        assert_eq!(fill.len(), 1, "{fill:?}");
        assert_eq!(fill[0][0].len(), 4, "{fill:?}");
        assert_eq!(area(&fill), 48.0);
    }

    #[test]
    fn the_byte_estimate_charges_long_edges_more() {
        // Same vertex count; the large square's deltas take more varint bytes.
        let small: FillPolygon = vec![vec![(0, 0), (4, 0), (4, 4), (0, 4)]];
        let large: FillPolygon = vec![vec![(0, 0), (4000, 0), (4000, 4000), (0, 4000)]];
        assert!(est_bytes(&large) > est_bytes(&small));
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
