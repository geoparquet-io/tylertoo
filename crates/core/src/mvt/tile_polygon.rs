//! The one owner of "a polygon that leaves the encoder is valid in tile
//! integer space" (#748).
//!
//! Every polygonal MVT feature is encoded by [`super::encode_tile_polygons`],
//! and that function hands its parts to [`finalize_polygons`] first, whatever
//! produced them: a plain encode, the merge valve's cached unions (#758), the
//! coverage fill (#750), a placeholder square or a `--verbatim` polygon. The
//! cleanup stages in front of it (pinch splitting and the first overlay
//! rounds of #393, the part unions of #383) are fast paths: they settle the
//! common snapping defects cheaply, and whatever they leave behind is caught
//! here.
//!
//! [`finalize_polygons`] checks the feature as one OGC MultiPolygon with
//! exact integer tests ([`check_polygons`]), the same rules GEOS's
//! `IsValidOp` (and so `shapely.is_valid`) applies to the decoded tile. The
//! usual outcome is "valid", and the parts go out untouched. A feature that
//! fails is rebuilt by one `i_overlay` union in the tile's own integer grid
//! ([`repair_polygons`]), whose OGC extraction cannot produce the defects the
//! check looks for; the result is checked again, and one that still fails is
//! counted ([`invalid_after_encode_count`]) and logged, never passed over in
//! silence.
//!
//! DIVERGENCE FROM TIPPECANOE: tippecanoe runs every polygon of every tile
//! through wagyu (`clean_or_clip_poly` in `clip.cpp`: a `clip_type_union` with
//! `fill_type_positive` over the snapped rings) unconditionally. We union
//! with the same fill rule (`FillRule::Positive`: outer rings count +1, holes
//! -1, a point is filled when its count is above zero), but only for
//! features the exact check rejects, so a valid polygon keeps its vertices
//! exactly as quantized instead of being re-traced. The engine is
//! `i_overlay`'s integer overlay rather than wagyu (`corpus/HOSTILE_GEOMETRY.md`
//! has the engine comparison).

use super::{
    any_box_pair, edge_box, ring_area2, ring_box, segments_cross_or_overlap, segments_meet,
    TileRing,
};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay::{IntOverlayOptions, Overlay, ShapeType};
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::core::solver::Solver;
use i_overlay::i_float::int::point::IntPoint;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Why a feature failed [`check_polygons`]: the OGC rule it breaks, named
/// after the snapping mechanism that usually causes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Defect {
    /// A ring with fewer than three distinct vertices, or no area: a sliver
    /// that snapped flat. GEOS: "Too few points"; the MVT decoder drops it.
    CollapsedRing,
    /// A ring touches or crosses itself: a neck that snapped shut, or a spike
    /// that folded back. GEOS: "Ring Self-intersection" / "Self-intersection".
    RingSelfTouch,
    /// Two rings of one polygon cross or run along each other: a hole that
    /// snapped onto its exterior. GEOS: "Self-intersection".
    RingsCross,
    /// A hole is not inside its exterior. GEOS: "Hole lies outside shell".
    HoleOutsideShell,
    /// A hole inside another hole. GEOS: "Holes are nested".
    NestedHoles,
    /// Holes touch the exterior and each other in a cycle, cutting the
    /// polygon in two. GEOS: "Interior is disconnected".
    DisconnectedInterior,
    /// Two parts of a MultiPolygon cross or share an edge stretch: two
    /// neighbours that snapped onto each other. GEOS: "Self-intersection".
    PartsOverlap,
    /// One part lies inside another's fill. GEOS: "Nested shells".
    NestedShells,
}

impl Defect {
    const ALL: [Defect; 8] = [
        Defect::CollapsedRing,
        Defect::RingSelfTouch,
        Defect::RingsCross,
        Defect::HoleOutsideShell,
        Defect::NestedHoles,
        Defect::DisconnectedInterior,
        Defect::PartsOverlap,
        Defect::NestedShells,
    ];

    /// A stable name for reports and logs.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Defect::CollapsedRing => "collapsed_ring",
            Defect::RingSelfTouch => "ring_self_touch",
            Defect::RingsCross => "rings_cross",
            Defect::HoleOutsideShell => "hole_outside_shell",
            Defect::NestedHoles => "nested_holes",
            Defect::DisconnectedInterior => "disconnected_interior",
            Defect::PartsOverlap => "parts_overlap",
            Defect::NestedShells => "nested_shells",
        }
    }

    fn index(self) -> usize {
        Defect::ALL
            .iter()
            .position(|&d| d == self)
            .expect("every defect is in ALL")
    }
}

/// Features [`finalize_polygons`] repaired, per [`Defect`] found on the way
/// in. Process-wide, like simplify's `VALIDATION_SKIPS`.
static REPAIRED: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

// F2: becomes Diagnostic::InvalidAfterEncode
/// Features that left the encoder still failing [`check_polygons`] after
/// [`repair_polygons`] (#748). The first one in a process warns; the count is
/// [`invalid_after_encode_count`]. Zero on every fixture of the strict
/// invariant matrix (`run_e2e.py --invariants`).
static INVALID_AFTER_ENCODE: AtomicU64 = AtomicU64::new(0);

/// Snapshot of the process-wide count of polygonal features emitted invalid
/// because the tile-space repair could not fix them (#748).
pub fn invalid_after_encode_count() -> u64 {
    INVALID_AFTER_ENCODE.load(Ordering::Relaxed)
}

/// Snapshot of the process-wide count of polygonal features the tile-space
/// owner repaired, per defect name (#748). Defects with no repairs are
/// included with 0, in a fixed order.
pub fn tile_polygon_repair_counts() -> Vec<(&'static str, u64)> {
    Defect::ALL
        .iter()
        .map(|&d| (d.name(), REPAIRED[d.index()].load(Ordering::Relaxed)))
        .collect()
}

fn count_invalid_after_encode(defect: Defect) {
    if INVALID_AFTER_ENCODE.fetch_add(1, Ordering::Relaxed) == 0 {
        log::warn!(
            "a polygon is still invalid in tile coordinates after repair ({}); it is \
             written as-is (#748). Later ones are counted, not logged.",
            defect.name()
        );
    }
}

/// Make a feature's polygons valid in tile integer space: the single gate
/// every polygonal MVT geometry passes (see the module docs).
///
/// `parts` are the feature's polygons, each an exterior followed by its
/// holes, every ring closed (first == last). Orientation does not matter:
/// [`super::encode_tile_ring`] orients each ring by its role. Returns the
/// parts unchanged when they already form a valid OGC MultiPolygon, else the
/// repaired parts (possibly empty: a feature whose rings all collapsed has
/// no area at this zoom).
pub(crate) fn finalize_polygons(parts: Vec<Vec<TileRing>>) -> Vec<Vec<TileRing>> {
    if parts.is_empty() {
        return parts;
    }
    let Err(defect) = check_polygons(&parts) else {
        return parts;
    };
    REPAIRED[defect.index()].fetch_add(1, Ordering::Relaxed);
    let repaired = repair_polygons(&parts);
    if let Err(left) = check_polygons(&repaired) {
        count_invalid_after_encode(left);
    }
    repaired
}

/// Rebuild `parts` as a valid MultiPolygon on the same integer grid: one
/// `i_overlay` union of every ring, exteriors counting +1 and holes -1 under
/// `FillRule::Positive` (tippecanoe's `fill_type_positive`), extracted with
/// the engine's OGC option so rings touch only where OGC allows.
///
/// So overlapping parts merge, a hole that snapped outside its exterior
/// (count -1) is dropped instead of becoming fill, a ring that folded over
/// itself keeps the area it encloses, and collapsed rings vanish. Coordinates
/// stay within the input's bounding box, which [`super::TILE_COORD_CLAMP`]
/// keeps well inside the i32 engine's ±2^30 range.
pub(crate) fn repair_polygons<P: AsRef<[TileRing]>>(parts: &[P]) -> Vec<Vec<TileRing>> {
    let edges: usize = parts.iter().flat_map(|p| p.as_ref()).map(|r| r.len()).sum();
    let mut overlay: Overlay<i32> =
        Overlay::new_custom(edges, IntOverlayOptions::ogc(), Solver::default());
    let mut contour: Vec<IntPoint<i32>> = Vec::new();
    for rings in parts.iter().map(AsRef::as_ref) {
        for (i, ring) in rings.iter().enumerate() {
            if ring.len() < 4 {
                continue;
            }
            // Exterior counts +1 (positive shoelace area), a hole -1.
            let want_positive = i == 0;
            let open = &ring[..ring.len() - 1];
            contour.clear();
            if (ring_area2(ring) > 0) == want_positive {
                contour.extend(open.iter().map(|&(x, y)| IntPoint::new(x, y)));
            } else {
                contour.extend(open.iter().rev().map(|&(x, y)| IntPoint::new(x, y)));
            }
            overlay.add_contour(&contour, ShapeType::Subject);
        }
    }
    let close = |c: Vec<IntPoint<i32>>| -> Option<TileRing> {
        let mut r: TileRing = c.into_iter().map(|p| (p.x, p.y)).collect();
        if r.len() < 3 {
            return None;
        }
        r.push(r[0]);
        Some(r)
    };
    let mut shells: Vec<Vec<TileRing>> = Vec::new();
    let mut holes: Vec<TileRing> = Vec::new();
    for shape in overlay.overlay(OverlayRule::Subject, FillRule::Positive) {
        let mut contours = shape.into_iter();
        if let Some(shell) = contours.next().and_then(close) {
            shells.push(vec![shell]);
            holes.extend(contours.filter_map(close));
        }
    }
    bind_holes(&mut shells, holes);
    shells
}

/// Attach each hole to the smallest exterior that contains it.
///
/// `i_overlay`'s OGC extraction traces every ring right but can attach a
/// hole to the wrong exterior (9.1: a coverage-fill union on ftw-moldova,
/// `repair_binds_each_hole_to_the_exterior_that_contains_it`), so the
/// repair does the grouping itself. The overlay's rings do not cross, so
/// one point of a hole settles which exteriors contain it; a hole no
/// exterior contains has no fill around it and is dropped.
fn bind_holes(shells: &mut [Vec<TileRing>], holes: Vec<TileRing>) {
    let boxes: Vec<_> = shells.iter().map(|s| ring_box(&s[0])).collect();
    let areas: Vec<i64> = shells.iter().map(|s| ring_area2(&s[0]).abs()).collect();
    for hole in holes {
        let hb = ring_box(&hole);
        let target = (0..shells.len())
            .filter(|&i| {
                box_within(hb, boxes[i]) && locate_ring(&hole, &shells[i][0]) == Loc::Inside
            })
            .min_by_key(|&i| areas[i]);
        if let Some(i) = target {
            shells[i].push(hole);
        }
    }
}

/// One edge of the feature, with the ring it belongs to.
#[derive(Clone, Copy)]
struct Edge {
    a: (i32, i32),
    b: (i32, i32),
    /// Index into the flat ring list of [`check_polygons`].
    ring: u32,
    /// Position of the edge in its ring.
    pos: u32,
}

/// Where a point lies relative to a ring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Loc {
    Inside,
    Outside,
    Boundary,
}

/// Is `parts` a valid OGC MultiPolygon in tile integer coordinates, as GEOS
/// judges the decoded tile? Exact integer arithmetic throughout; the first
/// defect found is returned.
///
/// One plane sweep over every edge of every ring finds self-touching rings,
/// rings or parts that cross or share an edge stretch, and the points where
/// rings of one polygon touch (a cycle of those touches disconnects the
/// interior). Once nothing crosses, one point per ring settles the nesting
/// rules: every hole inside its exterior, no hole in another hole, no part
/// inside another part's fill.
pub(crate) fn check_polygons<P: AsRef<[TileRing]>>(parts: &[P]) -> Result<(), Defect> {
    // Flat ring list, and the part of each ring.
    let mut ring_part: Vec<u32> = Vec::new();
    let mut rings: Vec<&TileRing> = Vec::new();
    for (p, poly) in parts.iter().map(AsRef::as_ref).enumerate() {
        if poly.is_empty() {
            return Err(Defect::CollapsedRing);
        }
        for ring in poly {
            if ring.len() < 4 || ring.first() != ring.last() || ring_area2(ring) == 0 {
                return Err(Defect::CollapsedRing);
            }
            ring_part.push(p as u32);
            rings.push(ring);
        }
    }
    for ring in &rings {
        if folds_back(ring) {
            return Err(Defect::RingSelfTouch);
        }
    }

    let mut edges: Vec<Edge> = Vec::with_capacity(rings.iter().map(|r| r.len() - 1).sum());
    for (k, ring) in rings.iter().enumerate() {
        edges.extend(ring.windows(2).enumerate().map(|(pos, w)| Edge {
            a: w[0],
            b: w[1],
            ring: k as u32,
            pos: pos as u32,
        }));
    }
    let boxes: Vec<_> = edges.iter().map(|e| edge_box(e.a, e.b)).collect();
    let mut defect = None;
    // Points where two different rings touch, with the edge of each that
    // the touch was found on (lower ring first).
    let mut touches: Vec<Touch> = Vec::new();
    any_box_pair(&boxes, |i, j| {
        let (ei, ej) = (edges[i], edges[j]);
        if ei.ring == ej.ring {
            let n = rings[ei.ring as usize].len() as u32 - 1;
            let adjacent = (ei.pos + 1) % n == ej.pos || (ej.pos + 1) % n == ei.pos;
            if !adjacent && segments_meet((ei.a, ei.b), (ej.a, ej.b)) {
                defect = Some(Defect::RingSelfTouch);
            }
        } else if segments_cross_or_overlap((ei.a, ei.b), (ej.a, ej.b)) {
            defect = Some(crossing_defect(&ring_part, ei.ring, ej.ring));
        } else if let Some(p) = touch_point((ei.a, ei.b), (ej.a, ej.b)) {
            let (lo, hi) = if ei.ring < ej.ring {
                (ei, ej)
            } else {
                (ej, ei)
            };
            touches.push(Touch { p, lo, hi });
        }
        defect.is_some()
    });
    if let Some(d) = defect {
        return Err(d);
    }
    // Each (ring, ring, point) once. Two rings that meet only at points can
    // still overlap: at a shared vertex one may pass from outside the other
    // to inside it (#748, Madagascar feature 7844 at 4/10/8: two parts that
    // share two vertices and overlap between them).
    touches.sort_unstable_by_key(|t| (t.lo.ring, t.hi.ring, t.p));
    touches.dedup_by_key(|t| (t.lo.ring, t.hi.ring, t.p));
    for t in &touches {
        let a = directions_at(rings[t.lo.ring as usize], t.lo, t.p);
        let b = directions_at(rings[t.hi.ring as usize], t.hi, t.p);
        if separates(a, b) {
            return Err(crossing_defect(&ring_part, t.lo.ring, t.hi.ring));
        }
    }
    let same_part: Vec<(u32, u32, (i32, i32))> = touches
        .iter()
        .filter(|t| ring_part[t.lo.ring as usize] == ring_part[t.hi.ring as usize])
        .map(|t| (t.lo.ring, t.hi.ring, t.p))
        .collect();
    if interior_disconnected(same_part, rings.len()) {
        return Err(Defect::DisconnectedInterior);
    }
    check_nesting(parts)
}

/// Two rings that touch at `p`, and the edge of each the touch was found on.
#[derive(Clone, Copy)]
struct Touch {
    p: (i32, i32),
    lo: Edge,
    hi: Edge,
}

fn crossing_defect(ring_part: &[u32], r: u32, s: u32) -> Defect {
    if ring_part[r as usize] == ring_part[s as usize] {
        Defect::RingsCross
    } else {
        Defect::PartsOverlap
    }
}

/// The two directions a simple closed ring leaves `p` in, `p` being on
/// `edge`: towards its neighbours when `p` is a vertex, towards the edge's
/// ends when `p` is inside the edge.
fn directions_at(ring: &TileRing, edge: Edge, p: (i32, i32)) -> [(i64, i64); 2] {
    let n = ring.len() - 1;
    let (u, v) = if p == edge.a {
        let k = edge.pos as usize;
        (ring[(k + n - 1) % n], ring[k + 1])
    } else if p == edge.b {
        let k = (edge.pos as usize + 1) % n;
        (ring[(k + n - 1) % n], ring[k + 1])
    } else {
        (edge.a, edge.b)
    };
    let d = |q: (i32, i32)| {
        (
            i64::from(q.0) - i64::from(p.0),
            i64::from(q.1) - i64::from(p.1),
        )
    };
    [d(u), d(v)]
}

/// Do the directions `a` (one ring through a point) separate the directions
/// `b` (another ring through it)? Then the second ring passes from one side
/// of the first to the other there: the rings cross at the point. None of
/// the four directions coincide (that would be a shared edge stretch, found
/// earlier).
fn separates(a: [(i64, i64); 2], b: [(i64, i64); 2]) -> bool {
    // Angular order, exact: the upper half-plane (and the positive x axis)
    // first, then by cross product.
    let half = |v: (i64, i64)| v.1 < 0 || (v.1 == 0 && v.0 < 0);
    let mut all = [(a[0], 0u8), (a[1], 0), (b[0], 1), (b[1], 1)];
    all.sort_unstable_by(|&(u, _), &(v, _)| {
        half(u)
            .cmp(&half(v))
            .then_with(|| (v.0 * u.1).cmp(&(u.0 * v.1)))
    });
    // Alternating A, B, A, B around the point.
    all[0].1 == all[2].1
}

/// Does a closed ring run straight back along the edge it arrived on at
/// some vertex (a 180° fold, including at `ring[0]`)? The plane sweep cannot
/// see this: adjacent edges are allowed to share their common vertex.
fn folds_back(ring: &TileRing) -> bool {
    let n = ring.len() - 1;
    (0..n).any(|i| {
        let (p, v, q) = (ring[(i + n - 1) % n], ring[i], ring[i + 1]);
        let (dx0, dy0) = (
            i64::from(v.0) - i64::from(p.0),
            i64::from(v.1) - i64::from(p.1),
        );
        let (dx1, dy1) = (
            i64::from(q.0) - i64::from(v.0),
            i64::from(q.1) - i64::from(v.1),
        );
        // A zero-length edge (a repeated vertex) is a self-touch too.
        (dx0 == 0 && dy0 == 0) || (dx0 * dy1 - dy0 * dx1 == 0 && dx0 * dx1 + dy0 * dy1 < 0)
    })
}

/// Is `p` on the closed segment `a`–`b`? Exact.
fn on_segment(p: (i32, i32), a: (i32, i32), b: (i32, i32)) -> bool {
    let bx = edge_box(a, b);
    p.0 >= bx.0
        && p.0 <= bx.2
        && p.1 >= bx.1
        && p.1 <= bx.3
        && (i64::from(b.0) - i64::from(a.0)) * (i64::from(p.1) - i64::from(a.1))
            == (i64::from(b.1) - i64::from(a.1)) * (i64::from(p.0) - i64::from(a.0))
}

/// The point two segments share, when they meet at an endpoint of one of
/// them (a touch). `None` when they are disjoint or cross properly (a proper
/// crossing has no endpoint on the other segment).
fn touch_point(s: ((i32, i32), (i32, i32)), t: ((i32, i32), (i32, i32))) -> Option<(i32, i32)> {
    [(s.0, t), (s.1, t), (t.0, s), (t.1, s)]
        .into_iter()
        .find(|&(p, (a, b))| on_segment(p, a, b))
        .map(|(p, _)| p)
}

/// Do the touches between rings of one polygon form a cycle? In the graph
/// with a node per ring and per touch point, linked when the ring passes
/// through the point, a cycle encloses part of the polygon's interior
/// between rings: a hole touching the exterior twice, or a chain of holes
/// from the exterior back to it. A tree of touches leaves it connected.
fn interior_disconnected(mut touches: Vec<(u32, u32, (i32, i32))>, ring_count: usize) -> bool {
    if touches.is_empty() {
        return false;
    }
    // Each (ring, point) incidence once.
    let mut incidences: Vec<(u32, (i32, i32))> = touches
        .drain(..)
        .flat_map(|(r, s, p)| [(r, p), (s, p)])
        .collect();
    incidences.sort_unstable();
    incidences.dedup();
    let mut point_node: HashMap<(i32, i32), usize> = HashMap::new();
    let mut root: Vec<usize> = (0..ring_count).collect();
    fn find(root: &mut [usize], mut i: usize) -> usize {
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    for (ring, p) in incidences {
        let node = *point_node.entry(p).or_insert_with(|| {
            root.push(root.len());
            root.len() - 1
        });
        let (a, b) = (find(&mut root, ring as usize), find(&mut root, node));
        if a == b {
            return true;
        }
        root[a] = b;
    }
    false
}

/// The nesting rules, for rings that are already known not to cross: each
/// hole inside its exterior, no hole inside another hole of its polygon, no
/// part inside another part's fill (an island in a lake is fine).
fn check_nesting<P: AsRef<[TileRing]>>(parts: &[P]) -> Result<(), Defect> {
    for poly in parts.iter().map(AsRef::as_ref) {
        let (shell, holes) = (&poly[0], &poly[1..]);
        let holes: Vec<&TileRing> = holes.iter().collect();
        if locate_rings(&holes, shell)
            .into_iter()
            .any(|l| l != Loc::Inside)
        {
            return Err(Defect::HoleOutsideShell);
        }
        if holes.len() > 1 {
            let boxes: Vec<_> = holes.iter().map(|h| ring_box(h)).collect();
            let nested = any_box_pair(&boxes, |i, j| {
                (box_within(boxes[i], boxes[j]) && locate_ring(holes[i], holes[j]) == Loc::Inside)
                    || (box_within(boxes[j], boxes[i])
                        && locate_ring(holes[j], holes[i]) == Loc::Inside)
            });
            if nested {
                return Err(Defect::NestedHoles);
            }
        }
    }
    if parts.len() < 2 {
        return Ok(());
    }
    let parts: Vec<&[TileRing]> = parts.iter().map(AsRef::as_ref).collect();
    let boxes: Vec<_> = parts.iter().map(|p| ring_box(&p[0])).collect();
    let in_fill = |inner: &[TileRing], outer: &[TileRing]| -> bool {
        locate_ring(&inner[0], &outer[0]) == Loc::Inside
            && !outer[1..]
                .iter()
                .any(|h| locate_ring(&inner[0], h) == Loc::Inside)
    };
    let nested = any_box_pair(&boxes, |i, j| {
        (box_within(boxes[i], boxes[j]) && in_fill(parts[i], parts[j]))
            || (box_within(boxes[j], boxes[i]) && in_fill(parts[j], parts[i]))
    });
    if nested {
        return Err(Defect::NestedShells);
    }
    Ok(())
}

fn box_within(inner: (i32, i32, i32, i32), outer: (i32, i32, i32, i32)) -> bool {
    inner.0 >= outer.0 && inner.1 >= outer.1 && inner.2 <= outer.2 && inner.3 <= outer.3
}

/// Where `ring` lies relative to `other`, for two rings that do not cross:
/// the location of its first vertex that is not on `other`, else of its
/// first edge midpoint that is not. `Boundary` only when every vertex and
/// every midpoint lies on `other`, which rings that do not overlap cannot
/// do.
fn locate_ring(ring: &TileRing, other: &TileRing) -> Loc {
    (0..2 * (ring.len() - 1))
        .map(|k| locate2(candidate_point(ring, k), other))
        .find(|&l| l != Loc::Boundary)
        .unwrap_or(Loc::Boundary)
}

/// The `k`-th point [`locate_ring`] tries on a closed ring, in doubled
/// coordinates: its vertices in order, then its edge midpoints.
fn candidate_point(ring: &TileRing, k: usize) -> (i64, i64) {
    let n = ring.len() - 1;
    if k < n {
        (2 * i64::from(ring[k].0), 2 * i64::from(ring[k].1))
    } else {
        let (a, b) = (ring[k - n], ring[k - n + 1]);
        (
            i64::from(a.0) + i64::from(b.0),
            i64::from(a.1) + i64::from(b.1),
        )
    }
}

/// [`locate_ring`] for many rings against one `other`, in one pass over
/// `other`'s edges per round rather than one per ring: a polygon with
/// thousands of holes checks them against its exterior in about the time of
/// one. Each ring is tried at its first vertex; the rare ring whose
/// candidate lies on `other` is retried at its next candidate point
/// (vertices, then edge midpoints), as [`locate_ring`] does.
fn locate_rings(rings: &[&TileRing], other: &TileRing) -> Vec<Loc> {
    let mut out = vec![Loc::Boundary; rings.len()];
    // Per ring, the index of its next candidate point: 0..n are vertices,
    // n..2n edge midpoints.
    let mut next = vec![0usize; rings.len()];
    let mut todo: Vec<usize> = (0..rings.len()).collect();
    while !todo.is_empty() {
        let points: Vec<(i64, i64)> = todo
            .iter()
            .map(|&k| candidate_point(rings[k], next[k]))
            .collect();
        let found = locate_points(&points, other);
        let mut still = Vec::new();
        for (slot, &k) in todo.iter().enumerate() {
            if found[slot] != Loc::Boundary {
                out[k] = found[slot];
                continue;
            }
            next[k] += 1;
            if next[k] < 2 * (rings[k].len() - 1) {
                still.push(k);
            }
        }
        todo = still;
    }
    out
}

/// Locate a point given in doubled coordinates (so edge midpoints are
/// exact) against a closed ring. Exact crossing-number test.
fn locate2(p: (i64, i64), ring: &TileRing) -> Loc {
    let mut inside = false;
    for w in ring.windows(2) {
        match edge_step(p, w[0], w[1]) {
            Step::Boundary => return Loc::Boundary,
            Step::Toggle => inside = !inside,
            Step::Pass => {}
        }
    }
    if inside {
        Loc::Inside
    } else {
        Loc::Outside
    }
}

/// [`locate2`] for many points at once: the points are sorted by y, and
/// each edge of `ring` visits only the points within its y range, so the
/// cost is one pass over the ring plus the (few) point-edge pairs that
/// share a y band, not points × edges.
fn locate_points(points: &[(i64, i64)], ring: &TileRing) -> Vec<Loc> {
    if points.len() < 4 {
        return points.iter().map(|&p| locate2(p, ring)).collect();
    }
    let mut order: Vec<u32> = (0..points.len() as u32).collect();
    order.sort_unstable_by_key(|&i| points[i as usize].1);
    let ys: Vec<i64> = order.iter().map(|&i| points[i as usize].1).collect();
    let mut inside = vec![false; points.len()];
    let mut boundary = vec![false; points.len()];
    for w in ring.windows(2) {
        let (ya, yb) = (2 * i64::from(w[0].1), 2 * i64::from(w[1].1));
        let lo = ys.partition_point(|&y| y < ya.min(yb));
        let hi = ys.partition_point(|&y| y <= ya.max(yb));
        for &i in &order[lo..hi] {
            let i = i as usize;
            match edge_step(points[i], w[0], w[1]) {
                Step::Boundary => boundary[i] = true,
                Step::Toggle => inside[i] = !inside[i],
                Step::Pass => {}
            }
        }
    }
    (0..points.len())
        .map(|i| match (boundary[i], inside[i]) {
            (true, _) => Loc::Boundary,
            (false, true) => Loc::Inside,
            (false, false) => Loc::Outside,
        })
        .collect()
}

/// What one ring edge `a`–`b` does to a crossing-number test at the doubled
/// point `p`.
enum Step {
    Boundary,
    Toggle,
    Pass,
}

#[inline]
fn edge_step(p: (i64, i64), a: (i32, i32), b: (i32, i32)) -> Step {
    let (px, py) = p;
    let (ax, ay) = (2 * i64::from(a.0), 2 * i64::from(a.1));
    let (bx, by) = (2 * i64::from(b.0), 2 * i64::from(b.1));
    if py < ay.min(by) || py > ay.max(by) {
        return Step::Pass;
    }
    if px >= ax.min(bx) && px <= ax.max(bx) && (bx - ax) * (py - ay) == (by - ay) * (px - ax) {
        return Step::Boundary;
    }
    // Half-open in y so a ray through a vertex counts once; the edge's x at
    // y = py is compared without dividing.
    if (ay > py) != (by > py) {
        let num = (py - ay) * (bx - ax);
        let den = by - ay;
        let lhs = (px - ax) * den;
        if (den > 0 && lhs < num) || (den < 0 && lhs > num) {
            return Step::Toggle;
        }
    }
    Step::Pass
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(pts: &[(i32, i32)]) -> TileRing {
        let mut r = pts.to_vec();
        r.push(pts[0]);
        r
    }

    fn square(x0: i32, y0: i32, x1: i32, y1: i32) -> TileRing {
        ring(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1)])
    }

    #[test]
    fn a_plain_square_is_valid_and_untouched() {
        let parts = vec![vec![square(0, 0, 10, 10)]];
        assert_eq!(check_polygons(&parts), Ok(()));
        assert_eq!(finalize_polygons(parts.clone()), parts);
    }

    #[test]
    fn a_square_with_a_hole_is_valid() {
        let parts = vec![vec![square(0, 0, 10, 10), square(2, 2, 8, 8)]];
        assert_eq!(check_polygons(&parts), Ok(()));
    }

    #[test]
    fn a_hole_touching_the_exterior_once_is_valid() {
        // The hole's corner sits on the exterior's left edge.
        let hole = ring(&[(0, 5), (5, 2), (5, 8)]);
        let parts = vec![vec![square(0, 0, 10, 10), hole]];
        assert_eq!(check_polygons(&parts), Ok(()));
    }

    #[test]
    fn a_hole_touching_the_exterior_twice_disconnects_the_interior() {
        // A diamond hole whose left and right corners touch the exterior.
        let hole = ring(&[(0, 5), (5, 2), (10, 5), (5, 8)]);
        let parts = vec![vec![square(0, 0, 10, 10), hole]];
        assert_eq!(check_polygons(&parts), Err(Defect::DisconnectedInterior));
    }

    #[test]
    fn a_chain_of_holes_across_the_polygon_disconnects_it() {
        let h1 = ring(&[(0, 5), (5, 3), (5, 7)]);
        let h2 = ring(&[(5, 3), (10, 5), (5, 7)]);
        let parts = vec![vec![square(0, 0, 10, 10), h1, h2]];
        // h1 and h2 share an edge: that is an overlap before it is a cycle.
        assert_eq!(check_polygons(&parts), Err(Defect::RingsCross));
        let h2 = ring(&[(5, 5), (10, 5), (8, 7)]);
        let h1 = ring(&[(0, 5), (5, 5), (3, 7)]);
        let parts = vec![vec![square(0, 0, 10, 10), h1, h2]];
        assert_eq!(check_polygons(&parts), Err(Defect::DisconnectedInterior));
    }

    #[test]
    fn a_hole_outside_its_exterior_is_caught() {
        let parts = vec![vec![square(0, 0, 10, 10), square(20, 20, 30, 30)]];
        assert_eq!(check_polygons(&parts), Err(Defect::HoleOutsideShell));
    }

    #[test]
    fn nested_holes_are_caught() {
        let parts = vec![vec![
            square(0, 0, 20, 20),
            square(2, 2, 18, 18),
            square(5, 5, 10, 10),
        ]];
        assert_eq!(check_polygons(&parts), Err(Defect::NestedHoles));
    }

    #[test]
    fn a_ring_touching_itself_is_caught() {
        // A figure-eight pinched at (5, 5).
        let r = ring(&[(0, 0), (5, 5), (10, 0), (10, 10), (5, 5), (0, 10)]);
        assert_eq!(
            check_polygons(&[vec![r]]),
            Err(Defect::RingSelfTouch),
            "a pinch"
        );
        // Vertex (5, 0) of the notch lies on the bottom edge.
        let r = ring(&[(0, 0), (10, 0), (10, 10), (5, 0), (0, 10)]);
        assert_eq!(check_polygons(&[vec![r]]), Err(Defect::RingSelfTouch));
    }

    #[test]
    fn a_fold_is_caught() {
        let r = ring(&[(0, 0), (10, 0), (10, 10), (10, 5), (10, 12), (0, 10)]);
        assert_eq!(check_polygons(&[vec![r]]), Err(Defect::RingSelfTouch));
    }

    #[test]
    fn collapsed_rings_are_caught() {
        let flat = ring(&[(0, 0), (5, 0), (10, 0)]);
        assert_eq!(check_polygons(&[vec![flat]]), Err(Defect::CollapsedRing));
    }

    #[test]
    fn overlapping_and_edge_sharing_parts_are_caught() {
        let a = vec![square(0, 0, 10, 10)];
        let b = vec![square(5, 5, 15, 15)];
        assert_eq!(check_polygons(&[a.clone(), b]), Err(Defect::PartsOverlap));
        let c = vec![square(10, 0, 20, 10)];
        assert_eq!(
            check_polygons(&[a.clone(), c]),
            Err(Defect::PartsOverlap),
            "a shared edge"
        );
        let d = vec![square(10, 10, 20, 20)];
        assert_eq!(check_polygons(&[a, d]), Ok(()), "corner touch");
    }

    #[test]
    fn nested_shells_are_caught_but_an_island_in_a_lake_is_not() {
        let outer = vec![square(0, 0, 20, 20)];
        let inner = vec![square(5, 5, 10, 10)];
        assert_eq!(
            check_polygons(&[outer, inner.clone()]),
            Err(Defect::NestedShells)
        );
        let lake = vec![square(0, 0, 20, 20), square(2, 2, 18, 18)];
        assert_eq!(check_polygons(&[lake, inner]), Ok(()));
    }

    #[test]
    fn a_part_in_a_lake_but_crossing_the_shore_is_an_overlap() {
        // The part's first vertex is in the lake; its far corner is on land.
        // A one-vertex test would call this an island (#748).
        let lake = vec![square(0, 0, 20, 20), square(2, 2, 18, 18)];
        let part = vec![square(5, 5, 19, 10)];
        assert_eq!(check_polygons(&[lake, part]), Err(Defect::PartsOverlap));
    }

    /// Rings as decoded from a tile (open), to closed tile rings.
    fn decoded(rings: &[&[(i32, i32)]]) -> Vec<TileRing> {
        rings.iter().map(|r| ring(r)).collect()
    }

    /// Madagascar adm4 feature 7844 at z4 4/10/8, as the default export
    /// wrote it before #748 was fixed: two parts that share the vertices
    /// (669, 3653) and (677, 3659) and overlap between them. No edge crosses
    /// another and neither part's first vertex is inside the other, so the
    /// old part test (`parts_interact`) let it through.
    fn madagascar_7844() -> Vec<Vec<TileRing>> {
        vec![
            decoded(&[&[(679, 3655), (677, 3659), (669, 3653), (677, 3648)]]),
            decoded(&[&[(668, 3656), (669, 3653), (673, 3654), (677, 3659)]]),
        ]
    }

    #[test]
    fn parts_crossing_at_shared_vertices_are_caught_madagascar_7844() {
        assert_eq!(
            check_polygons(&madagascar_7844()),
            Err(Defect::PartsOverlap)
        );
        let fixed = finalize_polygons(madagascar_7844());
        assert_eq!(check_polygons(&fixed), Ok(()));
        assert_eq!(fixed.len(), 1, "the overlapping parts merge");
    }

    #[test]
    fn a_part_inside_another_touching_its_boundary_is_nested_moldova_5749() {
        // ftw-moldova z6 6/37/22 feature 5749: the small part sits inside the
        // big one and touches it at (492, 1421), a vertex of both.
        let parts = vec![
            decoded(&[&[(492, 1421), (488, 1420), (489, 1420), (490, 1420)]]),
            decoded(&[&[
                (488, 1413),
                (494, 1414),
                (492, 1421),
                (490, 1427),
                (485, 1425),
            ]]),
        ];
        assert_eq!(check_polygons(&parts), Err(Defect::NestedShells));
        assert_eq!(check_polygons(&finalize_polygons(parts)), Ok(()));
    }

    #[test]
    fn holes_touching_the_exterior_twice_are_caught_madagascar_15595() {
        // Madagascar adm4 z4 4/10/8 feature 15595: both holes touch the
        // exterior at two vertices each.
        let poly = decoded(&[
            &[
                (-125, 3998),
                (-125, 3994),
                (-122, 3994),
                (-122, 3991),
                (-118, 3991),
                (-124, 3983),
                (-113, 3982),
                (-115, 3995),
                (-117, 3992),
                (-117, 4002),
                (-121, 3997),
                (-121, 3998),
            ],
            &[(-118, 3991), (-118, 3993), (-117, 3992)],
            &[(-121, 3997), (-120, 3995), (-121, 3995)],
        ]);
        assert_eq!(
            check_polygons(std::slice::from_ref(&poly)),
            Err(Defect::DisconnectedInterior)
        );
        assert_eq!(check_polygons(&finalize_polygons(vec![poly])), Ok(()));
    }

    #[test]
    fn a_hole_snapped_outside_is_caught_madagascar_10108() {
        // Madagascar adm4 --verbatim z1 1/1/1 feature 10108.
        let poly = decoded(&[
            &[
                (1045, 385),
                (1043, 386),
                (1042, 386),
                (1043, 385),
                (1043, 384),
                (1044, 384),
                (1044, 383),
                (1045, 383),
            ],
            &[(1043, 385), (1042, 384), (1042, 385)],
        ]);
        assert_eq!(
            check_polygons(std::slice::from_ref(&poly)),
            Err(Defect::HoleOutsideShell)
        );
        let fixed = finalize_polygons(vec![poly]);
        assert_eq!(check_polygons(&fixed), Ok(()));
        assert!(fixed.iter().all(|p| p.len() == 1), "the stray hole is gone");
    }

    #[test]
    fn rings_touching_at_a_vertex_without_crossing_are_fine() {
        // Two triangles meeting tip to tip, and a hole touching its
        // exterior at a vertex from inside.
        let a = vec![ring(&[(0, 0), (10, 0), (5, 5)])];
        let b = vec![ring(&[(5, 5), (10, 10), (0, 10)])];
        assert_eq!(check_polygons(&[a, b]), Ok(()));
        let shell = ring(&[(0, 0), (10, 0), (10, 10), (0, 10)]);
        let hole = ring(&[(10, 5), (5, 3), (5, 7)]);
        assert_eq!(check_polygons(&[vec![shell, hole]]), Ok(()));
    }

    /// A coverage-fill feature of ftw-moldova `--coverage-fill` (six parts,
    /// 509 vertices) whose union `i_overlay` 9.1's OGC extraction returned
    /// with a hole attached to the wrong exterior: every ring right, the
    /// grouping wrong ("Hole lies outside shell"). The repair binds holes
    /// itself.
    #[test]
    fn repair_binds_each_hole_to_the_exterior_that_contains_it() {
        let parts: Vec<Vec<TileRing>> =
            serde_json::from_str(include_str!("hole_binding_748.json")).expect("fixture");
        assert!(check_polygons(&parts).is_err());
        let fixed = repair_polygons(&parts);
        assert_eq!(check_polygons(&fixed), Ok(()));
        let area = |ps: &[Vec<TileRing>]| -> i64 {
            ps.iter()
                .flat_map(|p| p.iter().enumerate())
                .map(|(i, r)| if i == 0 { 1 } else { -1 } * ring_area2(r).abs())
                .sum()
        };
        assert!(area(&fixed) > 0);
    }

    /// Random rings on a small grid (most of them self-intersecting, holes
    /// anywhere, parts overlapping) always repair to a feature that passes
    /// the check, and the check passes valid input through untouched.
    #[test]
    fn repair_always_passes_the_check_on_random_input() {
        let mut seed = 0x748_u64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as i32
        };
        let (mut repaired, mut kept) = (0, 0);
        for _ in 0..3000 {
            let parts: Vec<Vec<TileRing>> = (0..=next() % 3)
                .map(|_| {
                    (0..=next() % 3)
                        .map(|_| {
                            let n = 3 + next() % 6;
                            let pts: Vec<_> = (0..n).map(|_| (next() % 10, next() % 10)).collect();
                            ring(&pts)
                        })
                        .collect()
                })
                .collect();
            let out = finalize_polygons(parts.clone());
            assert_eq!(check_polygons(&out), Ok(()), "{parts:?} -> {out:?}");
            if out == parts {
                kept += 1;
            } else {
                repaired += 1;
            }
        }
        assert!(
            repaired > 1000 && kept > 50,
            "repaired={repaired} kept={kept}"
        );
    }

    #[test]
    fn repair_unions_overlapping_parts_under_the_positive_rule() {
        let parts = vec![vec![square(0, 0, 10, 10)], vec![square(5, 5, 15, 15)]];
        let fixed = finalize_polygons(parts);
        assert_eq!(fixed.len(), 1);
        assert_eq!(check_polygons(&fixed), Ok(()));
        let area: i64 = fixed.iter().flatten().map(|r| ring_area2(r)).sum();
        // 100 + 100 - 25, twice, whatever the output orientation.
        assert_eq!(area.abs(), 350);
    }

    #[test]
    fn repair_drops_a_hole_outside_its_exterior() {
        let parts = vec![vec![square(0, 0, 10, 10), square(20, 20, 30, 30)]];
        let fixed = finalize_polygons(parts);
        assert_eq!(fixed, vec![vec![fixed[0][0].clone()]]);
        assert_eq!(ring_area2(&fixed[0][0]).abs(), 200);
    }

    #[test]
    fn repair_splits_a_pinched_ring_into_valid_parts() {
        let r = ring(&[(0, 0), (5, 5), (10, 0), (10, 10), (5, 5), (0, 10)]);
        let fixed = finalize_polygons(vec![vec![r]]);
        assert_eq!(check_polygons(&fixed), Ok(()));
        assert_eq!(fixed.len(), 2);
    }

    #[test]
    fn repair_keeps_a_hole_touching_twice_valid() {
        let hole = ring(&[(0, 5), (5, 2), (10, 5), (5, 8)]);
        let fixed = finalize_polygons(vec![vec![square(0, 0, 10, 10), hole]]);
        assert_eq!(check_polygons(&fixed), Ok(()));
        let area: i64 = fixed.iter().flatten().map(|r| ring_area2(r).abs()).sum();
        assert_eq!(area, 200 - 60, "the square less the diamond, in two halves");
    }
}
