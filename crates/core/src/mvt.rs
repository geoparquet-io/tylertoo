//! MVT (Mapbox Vector Tile) encoding module.
//!
//! This module implements the MVT specification for encoding geometries
//! into vector tiles. Key components:
//!
//! - **Zigzag encoding**: Efficiently encode signed integers as unsigned
//! - **Delta encoding**: Store coordinates as differences from previous position
//! - **Command encoding**: Pack geometry commands (MoveTo, LineTo, ClosePath)
//! - **Feature encoding**: Convert geo::Geometry to MVT Feature
//! - **Layer encoding**: Group features with deduplicated keys/values
//!
//! Reference: <https://github.com/mapbox/vector-tile-spec>

use crate::tile::TileBounds;
use crate::vector_tile::tile::{Feature, GeomType, Layer, Value};
use crate::vector_tile::Tile;
use geo::orient::{Direction, Orient};
use geo::{Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
use std::collections::HashMap;

/// Default tile extent (4096 as per MVT spec)
pub const DEFAULT_EXTENT: u32 = 4096;

/// MVT command IDs
const CMD_MOVE_TO: u32 = 1;
const CMD_LINE_TO: u32 = 2;
const CMD_CLOSE_PATH: u32 = 7;

// ============================================================================
// Zigzag Encoding
// ============================================================================

/// Encode a signed integer using zigzag encoding.
///
/// Zigzag encoding maps signed integers to unsigned integers so that
/// small negative numbers have small encoded values:
/// - 0 → 0
/// - -1 → 1
/// - 1 → 2
/// - -2 → 3
/// - 2 → 4
/// - etc.
///
/// This is efficient for protobuf varint encoding since small values
/// use fewer bytes.
#[inline]
pub fn zigzag_encode(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

/// Decode a zigzag-encoded unsigned integer back to signed.
#[inline]
pub fn zigzag_decode(n: u32) -> i32 {
    ((n >> 1) as i32) ^ -((n & 1) as i32)
}

// ============================================================================
// Command Encoding
// ============================================================================

/// Pack a command with a repeat count.
///
/// MVT commands are packed as: `(command_id | (count << 3))`
/// - command_id: 1=MoveTo, 2=LineTo, 7=ClosePath
/// - count: number of times to repeat the command
#[inline]
pub fn command_encode(command_id: u32, count: u32) -> u32 {
    (command_id & 0x7) | (count << 3)
}

/// Unpack a command into (command_id, count).
#[inline]
pub fn command_decode(command: u32) -> (u32, u32) {
    (command & 0x7, command >> 3)
}

// ============================================================================
// Winding Order Correction
// ============================================================================

/// Orient a polygon for MVT encoding.
///
/// MVT spec 4.3.3.3 defines ring roles by the sign of the surveyor's-formula
/// (shoelace) area computed on the stored tile coordinates:
/// - Exterior rings: POSITIVE area (appears clockwise with Y pointing down)
/// - Interior rings: NEGATIVE area (appears counter-clockwise with Y down)
///
/// Our coordinate transform flips Y (geographic latitude up → tile Y down),
/// and a Y-flip NEGATES the shoelace sign. So to end up positive in tile
/// coordinates, exterior rings must be NEGATIVE (clockwise) in geographic
/// coordinates — geo's `Direction::Reversed` convention:
/// - Exterior rings: clockwise in geo coords (positive area after Y-flip)
/// - Interior rings: counter-clockwise in geo coords (negative after Y-flip)
///
/// (An earlier version used `Direction::Default`, reasoning visually that
/// "geographic CCW appears CW after the Y-flip" — true on screen, but the
/// spec's definition is the algebraic sign on the stored coordinates, which
/// the flip negates. That emitted spec-inverted windings; fixed as part of
/// issue #112, whose decoder follows the spec sign.)
///
/// # Arguments
/// * `polygon` - The polygon to orient
///
/// # Returns
/// A new polygon with correctly oriented rings for MVT encoding
pub fn orient_polygon_for_mvt(polygon: &Polygon) -> Polygon {
    polygon.orient(Direction::Reversed)
}

/// Orient a multi-polygon for MVT encoding.
///
/// Applies `orient_polygon_for_mvt` to each constituent polygon.
///
/// # Arguments
/// * `multi` - The multi-polygon to orient
///
/// # Returns
/// A new multi-polygon with correctly oriented rings for MVT encoding
pub fn orient_multi_polygon_for_mvt(multi: &MultiPolygon) -> MultiPolygon {
    multi.orient(Direction::Reversed)
}

// ============================================================================
// Coordinate Transformation
// ============================================================================

/// Transform geographic coordinates (lng/lat) to tile-local coordinates.
///
/// Tile coordinates range from 0 to extent (typically 4096).
/// The tile bounds define the geographic extent being mapped.
///
/// # Arguments
/// * `lng` - Longitude in degrees
/// * `lat` - Latitude in degrees
/// * `bounds` - The geographic bounds of the tile
/// * `extent` - The tile extent (default 4096)
///
/// # Returns
/// (x, y) in tile-local coordinates, where (0,0) is top-left
pub fn geo_to_tile_coords(lng: f64, lat: f64, bounds: &TileBounds, extent: u32) -> (i32, i32) {
    let (x, y) = geo_to_tile_coords_unrounded(lng, lat, bounds, extent);
    (x.round() as i32, y.round() as i32)
}

/// Web Mercator Y fraction of a latitude: 0.0 at the top of the Mercator
/// world (+85.0511°), 0.5 at the equator, 1.0 at the bottom (−85.0511°).
///
/// Latitude is clamped to ±89.9° only to keep `tan` finite; callers passing
/// buffered coordinates slightly outside the Mercator range still get
/// monotonic (out-of-range) fractions rather than infinities.
#[inline]
pub(crate) fn mercator_y_fraction(lat: f64) -> f64 {
    let lat = lat.clamp(-89.9, 89.9);
    (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0
}

/// f64 (unrounded) core of [`geo_to_tile_coords`], shared with the
/// simplification and feature-drop paths so filtering/simplification sees the
/// exact coordinates MVT encoding will produce.
///
/// Longitude is linear in Web Mercator X, so X interpolates linearly between
/// the tile's degree bounds. Latitude is NOT linear in Web Mercator Y: tile
/// bounds are Mercator-derived (see `TileCoord::bounds`), so Y must
/// interpolate in Mercator fraction space. Linear latitude interpolation
/// displaces features toward the poles — ~470/4096 units at z0 for 40.7°N —
/// shrinking below one unit only around z12.
#[inline]
pub(crate) fn geo_to_tile_coords_unrounded(
    lng: f64,
    lat: f64,
    bounds: &TileBounds,
    extent: u32,
) -> (f64, f64) {
    let extent_f = extent as f64;

    // X: linear in longitude.
    let x_ratio = (lng - bounds.lng_min) / (bounds.lng_max - bounds.lng_min);

    // Y: linear in Mercator fraction, top-down (tile Y increases downward).
    let merc_top = mercator_y_fraction(bounds.lat_max);
    let merc_bottom = mercator_y_fraction(bounds.lat_min);
    let y_ratio = (mercator_y_fraction(lat) - merc_top) / (merc_bottom - merc_top);

    (x_ratio * extent_f, y_ratio * extent_f)
}

// ============================================================================
// Geometry Encoding
// ============================================================================

/// Encode a Point geometry to MVT geometry commands.
pub fn encode_point(point: &Point, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    let (x, y) = geo_to_tile_coords(point.x(), point.y(), bounds, extent);

    vec![
        command_encode(CMD_MOVE_TO, 1),
        zigzag_encode(x),
        zigzag_encode(y),
    ]
}

/// Encode a MultiPoint geometry to MVT geometry commands.
pub fn encode_multi_point(points: &MultiPoint, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    if points.0.is_empty() {
        return vec![];
    }

    let mut geometry = Vec::with_capacity(1 + points.0.len() * 2);
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;

    // All points use MoveTo with count = number of points
    geometry.push(command_encode(CMD_MOVE_TO, points.0.len() as u32));

    for point in &points.0 {
        let (x, y) = geo_to_tile_coords(point.x(), point.y(), bounds, extent);
        let dx = x - cursor_x;
        let dy = y - cursor_y;
        geometry.push(zigzag_encode(dx));
        geometry.push(zigzag_encode(dy));
        cursor_x = x;
        cursor_y = y;
    }

    geometry
}

/// Encode a LineString geometry to MVT geometry commands.
pub fn encode_linestring(line: &LineString, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    if line.0.len() < 2 {
        return vec![];
    }

    let mut geometry = Vec::with_capacity(3 + (line.0.len() - 1) * 2);
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;

    // First point: MoveTo
    let first = &line.0[0];
    let (x, y) = geo_to_tile_coords(first.x, first.y, bounds, extent);
    let dx = x - cursor_x;
    let dy = y - cursor_y;
    geometry.push(command_encode(CMD_MOVE_TO, 1));
    geometry.push(zigzag_encode(dx));
    geometry.push(zigzag_encode(dy));
    cursor_x = x;
    cursor_y = y;

    // Remaining points: LineTo
    if line.0.len() > 1 {
        geometry.push(command_encode(CMD_LINE_TO, (line.0.len() - 1) as u32));
        for coord in line.0.iter().skip(1) {
            let (x, y) = geo_to_tile_coords(coord.x, coord.y, bounds, extent);
            let dx = x - cursor_x;
            let dy = y - cursor_y;
            geometry.push(zigzag_encode(dx));
            geometry.push(zigzag_encode(dy));
            cursor_x = x;
            cursor_y = y;
        }
    }

    geometry
}

/// Encode a MultiLineString geometry to MVT geometry commands.
pub fn encode_multi_linestring(
    lines: &MultiLineString,
    bounds: &TileBounds,
    extent: u32,
) -> Vec<u32> {
    let mut geometry = Vec::new();
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;

    for line in &lines.0 {
        if line.0.len() < 2 {
            continue;
        }

        // First point: MoveTo
        let first = &line.0[0];
        let (x, y) = geo_to_tile_coords(first.x, first.y, bounds, extent);
        let dx = x - cursor_x;
        let dy = y - cursor_y;
        geometry.push(command_encode(CMD_MOVE_TO, 1));
        geometry.push(zigzag_encode(dx));
        geometry.push(zigzag_encode(dy));
        cursor_x = x;
        cursor_y = y;

        // Remaining points: LineTo
        if line.0.len() > 1 {
            geometry.push(command_encode(CMD_LINE_TO, (line.0.len() - 1) as u32));
            for coord in line.0.iter().skip(1) {
                let (x, y) = geo_to_tile_coords(coord.x, coord.y, bounds, extent);
                let dx = x - cursor_x;
                let dy = y - cursor_y;
                geometry.push(zigzag_encode(dx));
                geometry.push(zigzag_encode(dy));
                cursor_x = x;
                cursor_y = y;
            }
        }
    }

    geometry
}

// ============================================================================
// Polygon quantization cleanup (#383)
// ============================================================================

/// A closed ring in integer tile coordinates (first == last).
type TileRing = Vec<(i32, i32)>;

/// One edge of a ring, as its two endpoints.
type TileEdge = ((i32, i32), (i32, i32));

/// Snap a ring to tile units, dropping consecutive duplicates and closing it.
/// Returns `None` when fewer than three distinct vertices remain — the ring
/// has no area at this zoom and would only encode as a degenerate polygon.
fn quantize_ring(ring: &LineString, bounds: &TileBounds, extent: u32) -> Option<TileRing> {
    let mut out: TileRing = Vec::with_capacity(ring.0.len());
    for c in &ring.0 {
        let p = geo_to_tile_coords(c.x, c.y, bounds, extent);
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    // Close (the source ring may or may not repeat its first vertex, and the
    // first and last may have snapped together).
    if out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    if out.len() < 3 {
        return None;
    }
    let first = out[0];
    out.push(first);
    Some(out)
}

/// Twice the shoelace area of a closed integer ring. The sign is the
/// orientation on the stored coordinates — which is what the MVT spec keys
/// on: exterior rings positive, interior rings negative.
fn ring_area2(ring: &[(i32, i32)]) -> i64 {
    ring.windows(2)
        .map(|w| i64::from(w[0].0) * i64::from(w[1].1) - i64::from(w[1].0) * i64::from(w[0].1))
        .sum()
}

fn tile_rings_to_polygon(rings: &[TileRing]) -> Polygon<f64> {
    let to_ls = |r: &TileRing| {
        LineString::from(
            r.iter()
                .map(|&(x, y)| (f64::from(x), f64::from(y)))
                .collect::<Vec<_>>(),
        )
    };
    Polygon::new(to_ls(&rings[0]), rings[1..].iter().map(to_ls).collect())
}

/// Round a repaired (f64, integer-valued except at new crossing points)
/// ring back to tile units, with the same dedup/closure rules as
/// [`quantize_ring`].
fn requantize_ring(ring: &LineString<f64>) -> Option<TileRing> {
    let mut out: TileRing = Vec::with_capacity(ring.0.len());
    for c in &ring.0 {
        let p = (c.x.round() as i32, c.y.round() as i32);
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    if out.len() > 1 && out.first() == out.last() {
        out.pop();
    }
    if out.len() < 3 {
        return None;
    }
    let first = out[0];
    out.push(first);
    Some(out)
}

/// Split a closed ring at every vertex it visits twice (a "pinch": a neck
/// that snapped shut, or a spike that folded back). Each loop between two
/// visits of the same vertex becomes its own closed ring; zero-area loops
/// (spikes) are dropped. The returned rings visit no vertex twice.
fn split_pinches(mut ring: TileRing) -> Vec<TileRing> {
    ring.pop(); // work on the open ring
    let mut out = Vec::new();
    let mut stack = vec![ring];
    while let Some(mut cur) = stack.pop() {
        // Find the first repeated vertex: sort (index, vertex) by vertex and
        // look for an equal neighbour. No hashing, and n is small.
        let mut order: Vec<usize> = (0..cur.len()).collect();
        order.sort_unstable_by_key(|&i| cur[i]);
        let mut pinch: Option<(usize, usize)> = None;
        for w in order.windows(2) {
            if cur[w[0]] == cur[w[1]] {
                let (i, j) = (w[0].min(w[1]), w[0].max(w[1]));
                // Take the earliest second visit so loops nest predictably.
                if pinch.is_none_or(|(_, pj)| j < pj) {
                    pinch = Some((i, j));
                }
            }
        }
        match pinch {
            None => {
                if cur.len() >= 3 {
                    let first = cur[0];
                    cur.push(first);
                    if ring_area2(&cur) != 0 {
                        out.push(cur);
                    }
                }
            }
            Some((i, j)) => {
                // `cur[i..j]` is the loop entered at the pinch vertex; removing
                // it leaves the vertex once, at index i, in the remainder.
                let sub: Vec<(i32, i32)> = cur.drain(i..j).collect();
                stack.push(cur);
                stack.push(sub);
            }
        }
    }
    out
}

/// Insert every vertex that lies strictly inside a non-adjacent edge into
/// that edge, so a ring that touches itself vertex-on-edge (a "T-touch")
/// becomes one that visits the vertex twice, which [`split_pinches`] then
/// resolves. Exact integer arithmetic; only exact coincidences qualify,
/// which after snapping to the grid they frequently do.
fn node_ring(ring: TileRing) -> TileRing {
    let n = ring.len() - 1;
    if n > NODE_MAX_EDGES {
        return ring;
    }
    // (edge index, position along the edge, vertex) for every insertion.
    let mut inserts: Vec<(usize, i64, (i32, i32))> = Vec::new();
    for (k, &v) in ring[..n].iter().enumerate() {
        for i in 0..n {
            // Edges that end or start at v contain it trivially.
            if i == k || (i + 1) % n == k {
                continue;
            }
            let (a, b) = (ring[i], ring[i + 1]);
            let bx = edge_box(a, b);
            if v.0 < bx.0 || v.0 > bx.2 || v.1 < bx.1 || v.1 > bx.3 || v == a || v == b {
                continue;
            }
            let cross = (i64::from(b.0) - i64::from(a.0)) * (i64::from(v.1) - i64::from(a.1))
                - (i64::from(b.1) - i64::from(a.1)) * (i64::from(v.0) - i64::from(a.0));
            if cross == 0 {
                let along = (i64::from(v.0) - i64::from(a.0)).abs()
                    + (i64::from(v.1) - i64::from(a.1)).abs();
                inserts.push((i, along, v));
            }
        }
    }
    if inserts.is_empty() {
        return ring;
    }
    inserts.sort_unstable();
    let mut out: TileRing = Vec::with_capacity(ring.len() + inserts.len());
    let mut next = 0;
    for (i, &v0) in ring[..n].iter().enumerate() {
        out.push(v0);
        while next < inserts.len() && inserts[next].0 == i {
            let v = inserts[next].2;
            if out.last() != Some(&v) {
                out.push(v);
            }
            next += 1;
        }
    }
    let first = out[0];
    out.push(first);
    out
}

/// Exact integer test: do closed segments `a` and `b` share any point
/// (crossing, touching, or collinear overlap)?
fn segments_meet(a: ((i32, i32), (i32, i32)), b: ((i32, i32), (i32, i32))) -> bool {
    let cross = |o: (i32, i32), p: (i32, i32), q: (i32, i32)| -> i64 {
        (i64::from(p.0) - i64::from(o.0)) * (i64::from(q.1) - i64::from(o.1))
            - (i64::from(p.1) - i64::from(o.1)) * (i64::from(q.0) - i64::from(o.0))
    };
    let on_segment = |p: (i32, i32), q: (i32, i32), r: (i32, i32)| -> bool {
        r.0 >= p.0.min(q.0) && r.0 <= p.0.max(q.0) && r.1 >= p.1.min(q.1) && r.1 <= p.1.max(q.1)
    };
    let (p1, p2) = a;
    let (p3, p4) = b;
    let d1 = cross(p3, p4, p1);
    let d2 = cross(p3, p4, p2);
    let d3 = cross(p1, p2, p3);
    let d4 = cross(p1, p2, p4);
    if ((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) && ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0)) {
        return true;
    }
    (d1 == 0 && on_segment(p3, p4, p1))
        || (d2 == 0 && on_segment(p3, p4, p2))
        || (d3 == 0 && on_segment(p1, p2, p3))
        || (d4 == 0 && on_segment(p1, p2, p4))
}

/// Bounding box of an edge, for the cheap pair reject.
#[inline]
fn edge_box(p: (i32, i32), q: (i32, i32)) -> (i32, i32, i32, i32) {
    (p.0.min(q.0), p.1.min(q.1), p.0.max(q.0), p.1.max(q.1))
}

#[inline]
fn boxes_meet(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> bool {
    a.0 <= b.2 && b.0 <= a.2 && a.1 <= b.3 && b.1 <= a.3
}

/// Rings with more edges than this skip noding ([`node_ring`], which is
/// quadratic in the vertex count). The simplicity and ring-meeting checks
/// are plane sweeps ([`any_box_pair`]) and have no cap: a smooth 10k-edge
/// ring (coastline-like, short edges) checks in ~0.4 ms release, a 4k-edge
/// one in ~0.1 ms; only a saw of long edges that all overlap in x costs
/// milliseconds. A ring over the cap that fails the checks still goes
/// through pinch-splitting and the overlay; it only misses the vertex-on-
/// edge noding, so a T-touch on such a ring reaches the overlay instead of
/// the cheap split — the result is the same, it just costs more.
const NODE_MAX_EDGES: usize = 4096;

/// Visit every pair of boxes that meet, once, and stop at the first pair
/// `hit` accepts. A plane sweep over x: boxes are sorted by their min x and
/// each is paired with the following ones until their min x passes its max
/// x, so the work is O(n log n + k) for k box-meeting pairs rather than
/// O(n²). Which of a pair comes first in `hit(i, j)` follows the sort, not
/// the input order.
fn any_box_pair(boxes: &[(i32, i32, i32, i32)], mut hit: impl FnMut(usize, usize) -> bool) -> bool {
    let mut order: Vec<u32> = (0..boxes.len() as u32).collect();
    order.sort_unstable_by_key(|&i| boxes[i as usize].0);
    for (p, &i) in order.iter().enumerate() {
        let bi = boxes[i as usize];
        for &j in &order[p + 1..] {
            let bj = boxes[j as usize];
            if bj.0 > bi.2 {
                break;
            }
            if bj.1 <= bi.3 && bi.1 <= bj.3 && hit(i as usize, j as usize) {
                return true;
            }
        }
    }
    false
}

/// Is a closed ring (no repeated vertices — see [`split_pinches`]) simple:
/// no two non-adjacent edges meet, and no adjacent pair folds back onto
/// itself?
fn ring_is_simple(ring: &TileRing) -> bool {
    let n = ring.len() - 1; // edges

    // Adjacent edges share a vertex by construction; they may not overlap
    // beyond it (a 180° fold), which is: collinear at the shared vertex and
    // the second edge running back along the first. Tested on the actual
    // consecutive triple (prev, v, next) for every vertex, including
    // ring[0] — a collinear-through vertex there (RDP never tests a ring's
    // first vertex, so snapping leaves them constantly) is not a fold.
    for i in 0..n {
        let (p, v, q) = (ring[(i + n - 1) % n], ring[i], ring[i + 1]);
        let cross = (i64::from(v.0) - i64::from(p.0)) * (i64::from(q.1) - i64::from(v.1))
            - (i64::from(v.1) - i64::from(p.1)) * (i64::from(q.0) - i64::from(v.0));
        let dot = (i64::from(v.0) - i64::from(p.0)) * (i64::from(q.0) - i64::from(v.0))
            + (i64::from(v.1) - i64::from(p.1)) * (i64::from(q.1) - i64::from(v.1));
        if cross == 0 && dot < 0 {
            return false;
        }
    }
    let boxes: Vec<_> = (0..n).map(|i| edge_box(ring[i], ring[i + 1])).collect();
    !any_box_pair(&boxes, |i, j| {
        let adjacent = (i + 1) % n == j || (j + 1) % n == i;
        !adjacent && segments_meet((ring[i], ring[i + 1]), (ring[j], ring[j + 1]))
    })
}

/// Exact integer test: do closed segments `a` and `b` cross properly or
/// overlap along a stretch? Touching at a single point — an endpoint on the
/// other segment, or a shared endpoint — is NOT counted: two rings of one
/// polygon may touch at a point (a hole on the exterior, two holes meeting)
/// and stay valid.
fn segments_cross_or_overlap(a: ((i32, i32), (i32, i32)), b: ((i32, i32), (i32, i32))) -> bool {
    let cross = |o: (i32, i32), p: (i32, i32), q: (i32, i32)| -> i64 {
        (i64::from(p.0) - i64::from(o.0)) * (i64::from(q.1) - i64::from(o.1))
            - (i64::from(p.1) - i64::from(o.1)) * (i64::from(q.0) - i64::from(o.0))
    };
    let (p1, p2) = a;
    let (p3, p4) = b;
    let d1 = cross(p3, p4, p1);
    let d2 = cross(p3, p4, p2);
    let d3 = cross(p1, p2, p3);
    let d4 = cross(p1, p2, p4);
    if ((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) && ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0)) {
        return true; // proper crossing
    }
    if d1 == 0 && d2 == 0 {
        // Collinear: overlap along more than a point?
        let lo = |p: (i32, i32), q: (i32, i32)| (p.0.min(q.0), p.1.min(q.1));
        let hi = |p: (i32, i32), q: (i32, i32)| (p.0.max(q.0), p.1.max(q.1));
        let (alo, ahi, blo, bhi) = (lo(p1, p2), hi(p1, p2), lo(p3, p4), hi(p3, p4));
        let ox = alo.0.max(blo.0)..=ahi.0.min(bhi.0);
        let oy = alo.1.max(blo.1)..=ahi.1.min(bhi.1);
        return !ox.is_empty()
            && !oy.is_empty()
            && (ox.start() != ox.end() || oy.start() != oy.end());
    }
    false
}

/// Do the edges of any two *different* rings among `rings` cross or overlap
/// (touching at points is fine)? One sweep over every edge of every ring.
fn any_rings_meet(rings: &[&TileRing]) -> bool {
    // Every edge, tagged with the ring it belongs to.
    let mut edges: Vec<(u32, TileEdge)> =
        Vec::with_capacity(rings.iter().map(|r| r.len() - 1).sum());
    for (k, r) in rings.iter().enumerate() {
        edges.extend(r.windows(2).map(|w| (k as u32, (w[0], w[1]))));
    }
    let boxes: Vec<_> = edges.iter().map(|&(_, (a, b))| edge_box(a, b)).collect();
    any_box_pair(&boxes, |i, j| {
        let (ki, ei) = edges[i];
        let (kj, ej) = edges[j];
        ki != kj && segments_cross_or_overlap(ei, ej)
    })
}

/// Do the edges of two rings cross or overlap (touching at points is fine)?
fn rings_meet(a: &TileRing, b: &TileRing) -> bool {
    boxes_meet(ring_box(a), ring_box(b)) && any_rings_meet(&[a, b])
}

/// Is a polygon (exterior first, then holes) clean in tile space: every ring
/// simple and no two rings crossing or overlapping? (Rings touching at a
/// point are left alone; a hole touching the exterior twice — a
/// disconnected interior — slips through, and is rare enough to accept.)
fn polygon_is_clean(rings: &[TileRing]) -> bool {
    if !rings.iter().all(ring_is_simple) {
        return false;
    }
    if rings.len() < 2 {
        return true;
    }
    // Rings whose boxes are disjoint cannot meet; sweep the rest together.
    let boxes: Vec<_> = rings.iter().map(ring_box).collect();
    let mut keep = vec![false; rings.len()];
    any_box_pair(&boxes, |i, j| {
        keep[i] = true;
        keep[j] = true;
        false
    });
    let candidates: Vec<&TileRing> = rings
        .iter()
        .zip(&keep)
        .filter(|(_, &k)| k)
        .map(|(r, _)| r)
        .collect();
    candidates.len() < 2 || !any_rings_meet(&candidates)
}

/// Is `p` inside a closed integer ring? Exact crossing-number test; a point
/// on the boundary (on an edge or at a vertex) counts as inside.
fn point_in_ring(p: (i32, i32), ring: &TileRing) -> bool {
    let (px, py) = (i64::from(p.0), i64::from(p.1));
    let mut inside = false;
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (ax, ay, bx, by) = (
            i64::from(a.0),
            i64::from(a.1),
            i64::from(b.0),
            i64::from(b.1),
        );
        let eb = edge_box(a, b);
        if p.0 >= eb.0
            && p.0 <= eb.2
            && p.1 >= eb.1
            && p.1 <= eb.3
            && (bx - ax) * (py - ay) - (by - ay) * (px - ax) == 0
        {
            return true;
        }
        // Half-open in y so a ray through a vertex counts once; the x of the
        // edge at y = py is compared without dividing.
        if (ay > py) != (by > py) {
            let num = (py - ay) * (bx - ax);
            let den = by - ay;
            let lhs = (px - ax) * den;
            if (den > 0 && lhs < num) || (den < 0 && lhs > num) {
                inside = !inside;
            }
        }
    }
    inside
}

fn ring_box(ring: &TileRing) -> (i32, i32, i32, i32) {
    ring.iter()
        .fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |b, &(x, y)| {
            (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y))
        })
}

/// Split every ring at its pinches and regroup the pieces into polygons.
///
/// Orientation is judged against each ring's *own* original sense, not a
/// global convention, because source rings carry no guaranteed winding: a
/// piece of the exterior that keeps the exterior's sense is another
/// exterior (an island the neck used to join); one that reverses it is a
/// hole touching the boundary. For a hole ring the roles swap. A ring whose
/// lobes cancel exactly (a bowtie with equal lobes, net area zero) has no
/// sense of its own; its largest piece stands in for it. Holes are then
/// attached to the smallest exterior that geometrically contains them
/// (falling back to the first whose box does).
fn regroup_pinched(rings: Vec<TileRing>) -> Vec<Vec<TileRing>> {
    let mut exteriors: Vec<TileRing> = Vec::new();
    let mut holes: Vec<TileRing> = Vec::new();
    for (k, ring) in rings.into_iter().enumerate() {
        let mut own_sign = ring_area2(&ring).signum();
        let pieces = split_pinches(node_ring(ring));
        if own_sign == 0 {
            own_sign = pieces
                .iter()
                .max_by_key(|p| ring_area2(p).abs())
                .map_or(0, |p| ring_area2(p).signum());
        }
        for piece in pieces {
            let same_sense = ring_area2(&piece).signum() == own_sign;
            let is_exterior = (k == 0) == same_sense;
            if is_exterior {
                exteriors.push(piece);
            } else {
                holes.push(piece);
            }
        }
    }
    let boxes: Vec<_> = exteriors.iter().map(ring_box).collect();
    let areas: Vec<i64> = exteriors.iter().map(|e| ring_area2(e).abs()).collect();
    let mut polys: Vec<Vec<TileRing>> = exteriors.into_iter().map(|e| vec![e]).collect();
    for hole in holes {
        let hb = ring_box(&hole);
        // Box containment is only a filter: after a pinch split an L-shaped
        // lobe's box often contains an island in its concavity, and the
        // island's hole must land on the island.
        let mut by_box = boxes
            .iter()
            .enumerate()
            .filter(|(_, b)| b.0 <= hb.0 && b.1 <= hb.1 && b.2 >= hb.2 && b.3 >= hb.3)
            .map(|(i, _)| i);
        let Some(first) = by_box.next() else {
            continue;
        };
        let containing = std::iter::once(first)
            .chain(by_box)
            .filter(|&i| point_in_ring(hole[0], &polys[i][0]))
            .min_by_key(|&i| areas[i]);
        polys[containing.unwrap_or(first)].push(hole);
    }
    polys
}

/// Make a quantized polygon valid in tile space.
///
/// Snapping to the tile grid can fold a ring onto itself: two source
/// vertices that were a fraction of a unit apart land on the same column,
/// and the ring now runs back down an edge it already ran up (raster-derived
/// field boundaries with near-collinear vertices do this constantly), or a
/// narrow neck closes and the ring touches itself. The source was valid; the
/// tile geometry is not, and a fill renderer can show slivers or holes where
/// the ring overlaps. tippecanoe cleans every polygon after snapping; this
/// does the same, but only as much as each polygon needs:
///
/// 1. the polygon is checked with exact integer tests (no ring
///    self-intersects, no two rings meet) — the usual outcome, returned
///    untouched;
/// 2. one that fails has its rings noded (a vertex lying on another edge is
///    inserted into it) and split at pinch points (a vertex visited twice),
///    and the pieces regrouped — this settles the common neck-snapped-shut
///    and vertex-on-edge cases without an overlay;
/// 3. a piece that still fails is re-resolved through the even-odd overlay
///    that already repairs RDP bowties, snapped again, and pinch-split
///    again (bounded rounds).
///
/// The result may be several polygons (a neck that closed splits the
/// shape) — that is the correct tile geometry. Rings that collapse to
/// nothing are dropped; an empty result means the polygon has no area at
/// this zoom.
fn clean_tile_polygon(rings: Vec<TileRing>) -> Vec<Vec<TileRing>> {
    if rings.is_empty() {
        return Vec::new();
    }
    // The usual case: nothing touches, nothing crosses. One pass of the
    // integer checks and out; noding and pinch-splitting only run on the
    // polygons that fail it (a touch of any kind fails it).
    if polygon_is_clean(&rings) {
        return vec![rings];
    }
    let mut out = Vec::new();
    for poly in regroup_pinched(rings) {
        clean_into(poly, 0, &mut out);
    }
    out
}

/// Overlay rounds a polygon gets before it is emitted as-is. The overlay's
/// new crossing points are fractional; snapping them can (rarely) cross
/// again, and one more round settles that. Beyond that, emit what we have —
/// a fill renderer copes, and unbounded loops are worse than a rare sliver.
const MAX_REPAIR_ROUNDS: u32 = 2;

/// Push `poly` onto `out` once it is clean, repairing through the overlay
/// (and re-snapping) up to [`MAX_REPAIR_ROUNDS`] times.
fn clean_into(poly: Vec<TileRing>, round: u32, out: &mut Vec<Vec<TileRing>>) {
    if round >= MAX_REPAIR_ROUNDS || polygon_is_clean(&poly) {
        out.push(poly);
        return;
    }
    let Some(repaired) =
        crate::ioverlay_clip::repair_polygon_ioverlay(&tile_rings_to_polygon(&poly))
    else {
        // The overlay has nothing to say (it only returns `None` for input
        // it cannot trace); same policy as the round bound — emit what we
        // have rather than drop a piece that has area.
        out.push(poly);
        return;
    };
    for rs in requantized_parts(repaired) {
        for piece in regroup_pinched(rs) {
            clean_into(piece, round + 1, out);
        }
    }
}

/// Snap an overlay result back to tile rings, one `Vec<TileRing>` per part.
fn requantized_parts(g: Geometry<f64>) -> Vec<Vec<TileRing>> {
    let parts: Vec<Polygon<f64>> = match g {
        Geometry::Polygon(p) => vec![p],
        Geometry::MultiPolygon(mp) => mp.0,
        _ => Vec::new(),
    };
    parts
        .iter()
        .filter_map(|p| {
            let exterior = requantize_ring(p.exterior())?;
            let mut rs = vec![exterior];
            rs.extend(p.interiors().iter().filter_map(requantize_ring));
            Some(rs)
        })
        .collect()
}

/// Orient a polygon's rings in tile space: exterior positive, holes negative.
fn orient_tile_polygon(rings: &mut [TileRing]) {
    for (i, ring) in rings.iter_mut().enumerate() {
        let positive = ring_area2(ring) > 0;
        if positive != (i == 0) {
            ring.reverse();
        }
    }
}

/// Do two clean polygons (exterior first) interact — exteriors crossing or
/// overlapping, or one exterior's first vertex inside the other's fill
/// (inside the exterior and not inside one of its holes)? Touching at a
/// point is not an interaction, and an island inside a hole is a valid
/// multipolygon, not an overlap.
fn parts_interact(a: &[TileRing], b: &[TileRing]) -> bool {
    let inside_fill = |p: (i32, i32), poly: &[TileRing]| {
        point_in_ring(p, &poly[0]) && !poly[1..].iter().any(|h| point_in_ring(p, h))
    };
    rings_meet(&a[0], &b[0]) || inside_fill(b[0][0], a) || inside_fill(a[0][0], b)
}

/// Resolve overlaps *between* the parts of a multipolygon: each part is
/// clean on its own by now, but two parts may overlap or nest, and a fill
/// renderer under even-odd would show the overlap as a hole. Parts that
/// actually interact (see [`parts_interact`]) are grouped into connected
/// components; each component of two or more is oriented consistently,
/// unioned under NonZero, then snapped and cleaned again. Every other part
/// — including a mainland-and-island pair whose boxes nest but whose rings
/// never touch — passes through untouched, in its original order.
fn resolve_part_overlaps(parts: Vec<Vec<TileRing>>) -> Vec<Vec<TileRing>> {
    if parts.len() < 2 {
        return parts;
    }
    // Union-find over the parts; box-meeting pairs are the only candidates.
    let mut root: Vec<usize> = (0..parts.len()).collect();
    fn find(root: &mut [usize], mut i: usize) -> usize {
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    let boxes: Vec<_> = parts.iter().map(|p| ring_box(&p[0])).collect();
    let mut joined = false;
    any_box_pair(&boxes, |i, j| {
        if parts_interact(&parts[i], &parts[j]) {
            let (ri, rj) = (find(&mut root, i), find(&mut root, j));
            root[ri] = rj;
            joined = true;
        }
        false
    });
    if !joined {
        return parts;
    }
    let mut groups: Vec<Vec<usize>> = vec![Vec::new(); parts.len()];
    for i in 0..parts.len() {
        let r = find(&mut root, i);
        groups[r].push(i);
    }
    let mut out = Vec::with_capacity(parts.len());
    let mut done = vec![false; parts.len()];
    for i in 0..parts.len() {
        if done[i] {
            continue;
        }
        let group = &groups[find(&mut root, i)];
        if group.len() < 2 {
            out.push(parts[i].clone());
            done[i] = true;
            continue;
        }
        let polys: Vec<Polygon<f64>> = group
            .iter()
            .map(|&k| {
                let mut p = parts[k].clone();
                orient_tile_polygon(&mut p);
                tile_rings_to_polygon(&p)
            })
            .collect();
        match crate::ioverlay_clip::union_polygons_ioverlay(&polys) {
            Some(unioned) => {
                for rs in requantized_parts(unioned) {
                    for piece in regroup_pinched(rs) {
                        clean_into(piece, 1, &mut out);
                    }
                }
            }
            // The overlay could not trace the group: emit the parts as they
            // are rather than drop them.
            None => out.extend(group.iter().map(|&k| parts[k].clone())),
        }
        for &k in group {
            done[k] = true;
        }
    }
    out
}

/// Quantize every ring of `polygon`, clean the result, and return the
/// polygons to emit (exterior first in each).
fn quantized_polygons(polygon: &Polygon, bounds: &TileBounds, extent: u32) -> Vec<Vec<TileRing>> {
    let Some(exterior) = quantize_ring(polygon.exterior(), bounds, extent) else {
        return Vec::new();
    };
    let mut rings = vec![exterior];
    rings.extend(
        polygon
            .interiors()
            .iter()
            .filter_map(|r| quantize_ring(r, bounds, extent)),
    );
    clean_tile_polygon(rings)
}

/// Encode one closed integer ring with the orientation the spec requires
/// (`exterior`: positive area; hole: negative), updating the cursor.
fn encode_tile_ring(
    ring: &[(i32, i32)],
    exterior: bool,
    cursor: &mut (i32, i32),
    out: &mut Vec<u32>,
) {
    // Without the closing vertex: ClosePath returns to the first point.
    let open = &ring[..ring.len() - 1];
    let positive = ring_area2(ring) > 0;
    let mut emit = |x: i32, y: i32, first: bool| {
        if first {
            out.push(command_encode(CMD_MOVE_TO, 1));
        }
        out.push(zigzag_encode(x - cursor.0));
        out.push(zigzag_encode(y - cursor.1));
        *cursor = (x, y);
        if first {
            out.push(command_encode(CMD_LINE_TO, (open.len() - 1) as u32));
        }
    };
    if positive == exterior {
        for (k, &(x, y)) in open.iter().enumerate() {
            emit(x, y, k == 0);
        }
    } else {
        // Reversed traversal, same start vertex, no copy.
        emit(open[0].0, open[0].1, true);
        for &(x, y) in open[1..].iter().rev() {
            emit(x, y, false);
        }
    }
    out.push(command_encode(CMD_CLOSE_PATH, 1));
}

fn encode_tile_polygons(polys: &[Vec<TileRing>]) -> Vec<u32> {
    let total: usize = polys.iter().flatten().map(|r| 4 + r.len() * 2).sum();
    let mut out = Vec::with_capacity(total);
    let mut cursor = (0i32, 0i32);
    for rings in polys {
        for (i, ring) in rings.iter().enumerate() {
            encode_tile_ring(ring, i == 0, &mut cursor, &mut out);
        }
    }
    out
}

/// Encode a Polygon geometry to MVT geometry commands.
///
/// This function automatically corrects polygon winding order to comply with
/// the MVT specification before encoding:
/// - Exterior rings: clockwise in tile coordinates
/// - Interior rings: counter-clockwise in tile coordinates
pub fn encode_polygon(polygon: &Polygon, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    // Quantize first, clean in tile space (#383), then orient on the stored
    // integer coordinates — the sign the spec is defined on.
    encode_tile_polygons(&quantized_polygons(polygon, bounds, extent))
}

/// Encode a MultiPolygon geometry to MVT geometry commands.
///
/// This function automatically corrects polygon winding order to comply with
/// the MVT specification before encoding:
/// - Exterior rings: clockwise in tile coordinates
/// - Interior rings: counter-clockwise in tile coordinates
pub fn encode_multi_polygon(polygons: &MultiPolygon, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    let polys: Vec<Vec<TileRing>> = polygons
        .0
        .iter()
        .flat_map(|p| quantized_polygons(p, bounds, extent))
        .collect();
    encode_tile_polygons(&resolve_part_overlaps(polys))
}

/// Encode any geo::Geometry to MVT geometry commands and return the geometry type.
pub fn encode_geometry(geom: &Geometry, bounds: &TileBounds, extent: u32) -> (Vec<u32>, GeomType) {
    match geom {
        Geometry::Point(p) => (encode_point(p, bounds, extent), GeomType::Point),
        Geometry::MultiPoint(mp) => (encode_multi_point(mp, bounds, extent), GeomType::Point),
        Geometry::LineString(ls) => (encode_linestring(ls, bounds, extent), GeomType::Linestring),
        Geometry::MultiLineString(mls) => (
            encode_multi_linestring(mls, bounds, extent),
            GeomType::Linestring,
        ),
        Geometry::Polygon(p) => (encode_polygon(p, bounds, extent), GeomType::Polygon),
        Geometry::MultiPolygon(mp) => (encode_multi_polygon(mp, bounds, extent), GeomType::Polygon),
        // For geometry collections, we'd need to handle each part separately
        // For now, return empty geometry with unknown type
        _ => (vec![], GeomType::Unknown),
    }
}

// ============================================================================
// Feature Encoding
// ============================================================================

/// A property value that can be encoded in MVT.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    String(String),
    Float(f32),
    Double(f64),
    Int(i64),
    UInt(u64),
    Bool(bool),
}

impl PropertyValue {
    /// Convert to MVT Value type.
    pub fn to_mvt_value(&self) -> Value {
        match self {
            PropertyValue::String(s) => Value {
                string_value: Some(s.clone()),
                ..Default::default()
            },
            PropertyValue::Float(f) => Value {
                float_value: Some(*f),
                ..Default::default()
            },
            PropertyValue::Double(d) => Value {
                double_value: Some(*d),
                ..Default::default()
            },
            PropertyValue::Int(i) => Value {
                int_value: Some(*i),
                ..Default::default()
            },
            PropertyValue::UInt(u) => Value {
                uint_value: Some(*u),
                ..Default::default()
            },
            PropertyValue::Bool(b) => Value {
                bool_value: Some(*b),
                ..Default::default()
            },
        }
    }
}

/// Builder for encoding features into an MVT layer.
pub struct LayerBuilder {
    name: String,
    extent: u32,
    features: Vec<Feature>,
    keys: Vec<String>,
    key_index: HashMap<String, u32>,
    values: Vec<Value>,
    value_index: HashMap<String, u32>, // Serialize value for deduplication lookup
}

impl LayerBuilder {
    /// Create a new layer builder with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            extent: DEFAULT_EXTENT,
            features: Vec::new(),
            keys: Vec::new(),
            key_index: HashMap::new(),
            values: Vec::new(),
            value_index: HashMap::new(),
        }
    }

    /// Set the layer extent.
    pub fn with_extent(mut self, extent: u32) -> Self {
        self.extent = extent;
        self
    }

    /// Get or insert a key, returning its index.
    fn get_or_insert_key(&mut self, key: &str) -> u32 {
        if let Some(&idx) = self.key_index.get(key) {
            idx
        } else {
            let idx = self.keys.len() as u32;
            self.keys.push(key.to_string());
            self.key_index.insert(key.to_string(), idx);
            idx
        }
    }

    /// Get or insert a value, returning its index.
    fn get_or_insert_value(&mut self, value: &PropertyValue) -> u32 {
        // Create a string key for deduplication
        let value_key = format!("{:?}", value);

        if let Some(&idx) = self.value_index.get(&value_key) {
            idx
        } else {
            let idx = self.values.len() as u32;
            self.values.push(value.to_mvt_value());
            self.value_index.insert(value_key, idx);
            idx
        }
    }

    /// Add a feature to the layer.
    ///
    /// # Arguments
    /// * `id` - Optional feature ID
    /// * `geometry` - The geometry to encode
    /// * `properties` - Feature properties as key-value pairs
    /// * `bounds` - The tile bounds for coordinate transformation
    pub fn add_feature(
        &mut self,
        id: Option<u64>,
        geometry: &Geometry,
        properties: &[(String, PropertyValue)],
        bounds: &TileBounds,
    ) {
        let (geom_commands, geom_type) = encode_geometry(geometry, bounds, self.extent);

        // Skip empty geometries: unsupported types, and polygons that
        // quantize to nothing at this zoom (MVT 2.1 §4.2 requires a
        // geometry).
        if geom_commands.is_empty() {
            return;
        }

        // Encode tags as [key_idx, value_idx, key_idx, value_idx, ...]
        let mut tags = Vec::with_capacity(properties.len() * 2);
        for (key, value) in properties {
            let key_idx = self.get_or_insert_key(key);
            let value_idx = self.get_or_insert_value(value);
            tags.push(key_idx);
            tags.push(value_idx);
        }

        let feature = Feature {
            id,
            tags,
            r#type: Some(geom_type as i32),
            geometry: geom_commands,
        };

        self.features.push(feature);
    }

    /// Build the MVT Layer.
    pub fn build(self) -> Layer {
        Layer {
            version: 2,
            name: self.name,
            features: self.features,
            keys: self.keys,
            values: self.values,
            extent: Some(self.extent),
        }
    }
}

/// Builder for encoding multiple layers into an MVT tile.
pub struct TileBuilder {
    layers: Vec<Layer>,
}

impl TileBuilder {
    /// Create a new tile builder.
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Add a layer to the tile.
    pub fn add_layer(&mut self, layer: Layer) {
        self.layers.push(layer);
    }

    /// Build the MVT Tile.
    pub fn build(self) -> Tile {
        Tile {
            layers: self.layers,
        }
    }
}

impl Default for TileBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::TileCoord;
    use geo::{line_string, point, polygon};

    // ------------------------------------------------------------------------
    // Zigzag Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_zigzag_encode_zero() {
        assert_eq!(zigzag_encode(0), 0);
    }

    #[test]
    fn test_zigzag_encode_negative_one() {
        assert_eq!(zigzag_encode(-1), 1);
    }

    #[test]
    fn test_zigzag_encode_positive_one() {
        assert_eq!(zigzag_encode(1), 2);
    }

    #[test]
    fn test_zigzag_encode_negative_two() {
        assert_eq!(zigzag_encode(-2), 3);
    }

    #[test]
    fn test_zigzag_encode_positive_two() {
        assert_eq!(zigzag_encode(2), 4);
    }

    #[test]
    fn test_zigzag_encode_large_positive() {
        // 100 → 200
        assert_eq!(zigzag_encode(100), 200);
    }

    #[test]
    fn test_zigzag_encode_large_negative() {
        // -100 → 199
        assert_eq!(zigzag_encode(-100), 199);
    }

    #[test]
    fn test_zigzag_roundtrip() {
        for n in -1000..=1000 {
            let encoded = zigzag_encode(n);
            let decoded = zigzag_decode(encoded);
            assert_eq!(decoded, n, "Roundtrip failed for {}", n);
        }
    }

    #[test]
    fn test_zigzag_decode() {
        assert_eq!(zigzag_decode(0), 0);
        assert_eq!(zigzag_decode(1), -1);
        assert_eq!(zigzag_decode(2), 1);
        assert_eq!(zigzag_decode(3), -2);
        assert_eq!(zigzag_decode(4), 2);
    }

    // ------------------------------------------------------------------------
    // Command Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_command_encode_moveto_1() {
        // MoveTo with count=1: (1 | (1 << 3)) = 9
        assert_eq!(command_encode(CMD_MOVE_TO, 1), 9);
    }

    #[test]
    fn test_command_encode_lineto_3() {
        // LineTo with count=3: (2 | (3 << 3)) = 26
        assert_eq!(command_encode(CMD_LINE_TO, 3), 26);
    }

    #[test]
    fn test_command_encode_closepath() {
        // ClosePath with count=1: (7 | (1 << 3)) = 15
        assert_eq!(command_encode(CMD_CLOSE_PATH, 1), 15);
    }

    #[test]
    fn test_command_decode() {
        let cmd = command_encode(CMD_LINE_TO, 5);
        let (id, count) = command_decode(cmd);
        assert_eq!(id, CMD_LINE_TO);
        assert_eq!(count, 5);
    }

    #[test]
    fn test_command_roundtrip() {
        for cmd_id in [CMD_MOVE_TO, CMD_LINE_TO, CMD_CLOSE_PATH] {
            for count in 1..=100 {
                let encoded = command_encode(cmd_id, count);
                let (decoded_id, decoded_count) = command_decode(encoded);
                assert_eq!(decoded_id, cmd_id);
                assert_eq!(decoded_count, count);
            }
        }
    }

    // ------------------------------------------------------------------------
    // Coordinate Transformation Tests
    // ------------------------------------------------------------------------

    fn test_bounds() -> TileBounds {
        TileBounds {
            lng_min: 0.0,
            lat_min: 0.0,
            lng_max: 1.0,
            lat_max: 1.0,
        }
    }

    #[test]
    fn test_geo_to_tile_coords_center() {
        let bounds = test_bounds();
        let (x, y) = geo_to_tile_coords(0.5, 0.5, &bounds, 4096);
        // Center should be (2048, 2048)
        assert_eq!(x, 2048);
        assert_eq!(y, 2048);
    }

    #[test]
    fn test_geo_to_tile_coords_origin() {
        let bounds = test_bounds();
        // Bottom-left corner (lng_min, lat_min) → (0, extent) since Y is flipped
        let (x, y) = geo_to_tile_coords(0.0, 0.0, &bounds, 4096);
        assert_eq!(x, 0);
        assert_eq!(y, 4096);
    }

    #[test]
    fn test_geo_to_tile_coords_top_right() {
        let bounds = test_bounds();
        // Top-right corner (lng_max, lat_max) → (extent, 0)
        let (x, y) = geo_to_tile_coords(1.0, 1.0, &bounds, 4096);
        assert_eq!(x, 4096);
        assert_eq!(y, 0);
    }

    #[test]
    fn test_geo_to_tile_coords_top_left() {
        let bounds = test_bounds();
        // Top-left corner (lng_min, lat_max) → (0, 0)
        let (x, y) = geo_to_tile_coords(0.0, 1.0, &bounds, 4096);
        assert_eq!(x, 0);
        assert_eq!(y, 0);
    }

    /// Web-Mercator-correct expected tile-local Y for a latitude at a zoom,
    /// computed independently of production code:
    /// merc fraction y = (1 - ln(tan(φ) + 1/cos(φ)) / π) / 2,
    /// tile-local = (y * 2^z - tile_y) * extent.
    fn expected_mercator_tile_y(lat: f64, zoom: u8, tile_y: u32, extent: u32) -> i32 {
        let phi = lat.to_radians();
        let merc = (1.0 - (phi.tan() + 1.0 / phi.cos()).ln() / std::f64::consts::PI) / 2.0;
        ((merc * (1u32 << zoom) as f64 - tile_y as f64) * extent as f64).round() as i32
    }

    #[test]
    fn test_geo_to_tile_coords_mercator_y_z0() {
        // NYC (40.7128°N, -74.0060°W) in the single z0 tile. Latitude must be
        // placed with Web Mercator Y, not linear interpolation between the
        // tile's degree bounds. Linear interpolation puts this ~472 units too
        // far north (y≈1068 instead of 1540), i.e. renders NYC at ~65°N.
        let (lng, lat) = (-74.0060, 40.7128);
        let tile = TileCoord::new(0, 0, 0);
        let bounds = tile.bounds();
        let extent = 4096;

        let expected_y = expected_mercator_tile_y(lat, 0, 0, extent);
        let expected_x = (((lng + 180.0) / 360.0) * extent as f64).round() as i32;

        let (x, y) = geo_to_tile_coords(lng, lat, &bounds, extent);
        assert!(
            (x - expected_x).abs() <= 1,
            "z0 X: got {x}, expected {expected_x}"
        );
        assert!(
            (y - expected_y).abs() <= 1,
            "z0 Y must be Web Mercator: got {y}, expected {expected_y}"
        );
    }

    #[test]
    fn test_geo_to_tile_coords_mercator_y_z2() {
        // Same point in its z2 tile (x=1, y=1). Linear interpolation still
        // misplaces latitude by tens of units at z2.
        let (lng, lat) = (-74.0060, 40.7128);
        let tile = TileCoord::new(1, 1, 2);
        let bounds = tile.bounds();
        let extent = 4096;

        let expected_y = expected_mercator_tile_y(lat, 2, 1, extent);
        let (_, y) = geo_to_tile_coords(lng, lat, &bounds, extent);
        assert!(
            (y - expected_y).abs() <= 1,
            "z2 Y must be Web Mercator: got {y}, expected {expected_y}"
        );
    }

    #[test]
    fn test_geo_to_tile_coords_mercator_y_z12_regression() {
        // Fine-zoom regression pin: at z12 a tile spans so few degrees that
        // mercator and linear agree to sub-unit precision — fine zooms were
        // visually correct before the mercator fix and must not shift.
        let (lng, lat) = (-74.0060_f64, 40.7128_f64);
        let zoom = 12u8;
        let n = 1u32 << zoom;
        let tx = (((lng + 180.0) / 360.0) * n as f64).floor() as u32;
        let phi = lat.to_radians();
        let merc = (1.0 - (phi.tan() + 1.0 / phi.cos()).ln() / std::f64::consts::PI) / 2.0;
        let ty = (merc * n as f64).floor() as u32;
        let tile = TileCoord::new(tx, ty, zoom);
        let bounds = tile.bounds();
        let extent = 4096;

        let expected_y = expected_mercator_tile_y(lat, zoom, ty, extent);
        // The pre-fix linear-interpolation value, pinned so the fix provably
        // does not move fine-zoom output by more than 1 unit.
        let linear_y = (((bounds.lat_max - lat) / (bounds.lat_max - bounds.lat_min))
            * extent as f64)
            .round() as i32;
        assert!(
            (expected_y - linear_y).abs() <= 1,
            "test premise: mercator and linear must agree at z12 \
             (mercator {expected_y} vs linear {linear_y})"
        );

        let (_, y) = geo_to_tile_coords(lng, lat, &bounds, extent);
        assert!(
            (y - expected_y).abs() <= 1,
            "z12 Y: got {y}, expected {expected_y} (pre-fix {linear_y})"
        );
    }

    // ------------------------------------------------------------------------
    // Point Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_encode_point_at_center() {
        let bounds = test_bounds();
        let point = point!(x: 0.5, y: 0.5);
        let commands = encode_point(&point, &bounds, 4096);

        // Should be: [MoveTo(1), zigzag(2048), zigzag(2048)]
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], command_encode(CMD_MOVE_TO, 1)); // 9
        assert_eq!(commands[1], zigzag_encode(2048)); // x
        assert_eq!(commands[2], zigzag_encode(2048)); // y
    }

    #[test]
    fn test_encode_point_at_origin() {
        let bounds = test_bounds();
        let point = point!(x: 0.0, y: 0.0); // Bottom-left
        let commands = encode_point(&point, &bounds, 4096);

        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], command_encode(CMD_MOVE_TO, 1));
        assert_eq!(commands[1], zigzag_encode(0)); // x = 0
        assert_eq!(commands[2], zigzag_encode(4096)); // y = 4096 (flipped)
    }

    // ------------------------------------------------------------------------
    // LineString Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_encode_linestring_simple() {
        let bounds = test_bounds();
        let line = line_string![
            (x: 0.0, y: 0.0),
            (x: 0.5, y: 0.5),
            (x: 1.0, y: 1.0),
        ];
        let commands = encode_linestring(&line, &bounds, 4096);

        // Should be: MoveTo(1), x, y, LineTo(2), dx1, dy1, dx2, dy2
        // That's 1 + 2 + 1 + 4 = 8 elements
        assert_eq!(commands.len(), 8);

        // MoveTo command
        assert_eq!(commands[0], command_encode(CMD_MOVE_TO, 1));

        // First point (0, 4096) - bottom left in tile coords
        assert_eq!(commands[1], zigzag_encode(0)); // x
        assert_eq!(commands[2], zigzag_encode(4096)); // y (flipped from lat)

        // LineTo command with count=2
        assert_eq!(commands[3], command_encode(CMD_LINE_TO, 2));

        // Delta to (2048, 2048) from (0, 4096) = (2048, -2048)
        assert_eq!(commands[4], zigzag_encode(2048));
        assert_eq!(commands[5], zigzag_encode(-2048));

        // Delta to (4096, 0) from (2048, 2048) = (2048, -2048)
        assert_eq!(commands[6], zigzag_encode(2048));
        assert_eq!(commands[7], zigzag_encode(-2048));
    }

    #[test]
    fn test_encode_linestring_too_short() {
        let bounds = test_bounds();
        let line = line_string![(x: 0.0, y: 0.0)]; // Only one point
        let commands = encode_linestring(&line, &bounds, 4096);

        // Should return empty - linestrings need at least 2 points
        assert!(commands.is_empty());
    }

    // ------------------------------------------------------------------------
    // Polygon Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_encode_polygon_simple() {
        let bounds = test_bounds();
        let poly = polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0), // Closing point
        ];
        let commands = encode_polygon(&poly, &bounds, 4096);

        // Should have: MoveTo(1), x, y, LineTo(3), dx1, dy1, dx2, dy2, dx3, dy3, ClosePath(1)
        // MoveTo + 2 coords + LineTo + 6 coords + ClosePath = 10 elements
        assert!(!commands.is_empty());

        // First command should be MoveTo
        assert_eq!(command_decode(commands[0]).0, CMD_MOVE_TO);

        // Last command should be ClosePath
        let last_cmd = *commands.last().unwrap();
        assert_eq!(command_decode(last_cmd).0, CMD_CLOSE_PATH);
    }

    // ------------------------------------------------------------------------
    // Layer Builder Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_layer_builder_basic() {
        let bounds = test_bounds();
        let mut builder = LayerBuilder::new("test_layer");

        let point = Geometry::Point(point!(x: 0.5, y: 0.5));
        let properties = vec![
            (
                "name".to_string(),
                PropertyValue::String("test".to_string()),
            ),
            ("value".to_string(), PropertyValue::Int(42)),
        ];

        builder.add_feature(Some(1), &point, &properties, &bounds);

        let layer = builder.build();

        assert_eq!(layer.name, "test_layer");
        assert_eq!(layer.version, 2);
        assert_eq!(layer.features.len(), 1);
        assert_eq!(layer.keys.len(), 2);
        assert_eq!(layer.values.len(), 2);
        assert_eq!(layer.extent, Some(4096));
    }

    #[test]
    fn test_layer_builder_key_deduplication() {
        let bounds = test_bounds();
        let mut builder = LayerBuilder::new("test_layer");

        let point1 = Geometry::Point(point!(x: 0.25, y: 0.25));
        let point2 = Geometry::Point(point!(x: 0.75, y: 0.75));

        // Both features have "name" key - should be deduplicated
        let props1 = vec![("name".to_string(), PropertyValue::String("a".to_string()))];
        let props2 = vec![("name".to_string(), PropertyValue::String("b".to_string()))];

        builder.add_feature(Some(1), &point1, &props1, &bounds);
        builder.add_feature(Some(2), &point2, &props2, &bounds);

        let layer = builder.build();

        assert_eq!(layer.features.len(), 2);
        assert_eq!(layer.keys.len(), 1); // Only one unique key "name"
        assert_eq!(layer.values.len(), 2); // Two different values "a" and "b"
    }

    #[test]
    fn test_layer_builder_value_deduplication() {
        let bounds = test_bounds();
        let mut builder = LayerBuilder::new("test_layer");

        let point1 = Geometry::Point(point!(x: 0.25, y: 0.25));
        let point2 = Geometry::Point(point!(x: 0.75, y: 0.75));

        // Both features have same value - should be deduplicated
        let props1 = vec![(
            "type".to_string(),
            PropertyValue::String("building".to_string()),
        )];
        let props2 = vec![(
            "type".to_string(),
            PropertyValue::String("building".to_string()),
        )];

        builder.add_feature(Some(1), &point1, &props1, &bounds);
        builder.add_feature(Some(2), &point2, &props2, &bounds);

        let layer = builder.build();

        assert_eq!(layer.features.len(), 2);
        assert_eq!(layer.keys.len(), 1); // One key "type"
        assert_eq!(layer.values.len(), 1); // One value "building" (deduplicated)
    }

    // ------------------------------------------------------------------------
    // Tile Builder Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tile_builder() {
        let bounds = test_bounds();

        let mut layer1 = LayerBuilder::new("points");
        layer1.add_feature(
            Some(1),
            &Geometry::Point(point!(x: 0.5, y: 0.5)),
            &[],
            &bounds,
        );

        let mut layer2 = LayerBuilder::new("lines");
        let line = Geometry::LineString(line_string![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 1.0),
        ]);
        layer2.add_feature(Some(2), &line, &[], &bounds);

        let mut tile_builder = TileBuilder::new();
        tile_builder.add_layer(layer1.build());
        tile_builder.add_layer(layer2.build());

        let tile = tile_builder.build();

        assert_eq!(tile.layers.len(), 2);
        assert_eq!(tile.layers[0].name, "points");
        assert_eq!(tile.layers[1].name, "lines");
    }

    // ------------------------------------------------------------------------
    // GeomType Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_encode_geometry_returns_correct_type() {
        let bounds = test_bounds();

        let (_, geom_type) =
            encode_geometry(&Geometry::Point(point!(x: 0.5, y: 0.5)), &bounds, 4096);
        assert_eq!(geom_type, GeomType::Point);

        let line = Geometry::LineString(line_string![(x: 0.0, y: 0.0), (x: 1.0, y: 1.0)]);
        let (_, geom_type) = encode_geometry(&line, &bounds, 4096);
        assert_eq!(geom_type, GeomType::Linestring);

        let poly = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let (_, geom_type) = encode_geometry(&poly, &bounds, 4096);
        assert_eq!(geom_type, GeomType::Polygon);
    }

    // ------------------------------------------------------------------------
    // Winding Order Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_polygon_correct_winding_unchanged() {
        // A polygon with correct MVT winding (CW exterior in geographic
        // coords: the Y-flip negates the shoelace sign, yielding the
        // POSITIVE tile-space area the spec requires for exterior rings)
        // should pass through unchanged.

        // CW polygon in geographic coords (correct for MVT after Y-flip)
        let poly = polygon![
            (x: 0.0, y: 0.0),
            (x: 0.0, y: 1.0),
            (x: 1.0, y: 1.0),
            (x: 1.0, y: 0.0),
            (x: 0.0, y: 0.0),
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // Should be unchanged since it's already correctly oriented
        assert_eq!(poly.exterior().0, oriented.exterior().0);
    }

    #[test]
    fn test_polygon_incorrect_winding_gets_corrected() {
        // A polygon with incorrect winding (CCW exterior in geographic
        // coords, which would flip to NEGATIVE tile-space area) should be
        // corrected to CW exterior.

        // CCW polygon in geographic coords (incorrect - needs correction)
        let poly = polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // Should now be CW (reversed from input)
        // The first and last points stay the same, but the middle points should be reversed
        assert_ne!(poly.exterior().0[1], oriented.exterior().0[1]);
    }

    #[test]
    fn test_encoded_exterior_ring_has_positive_tile_area() {
        // MVT spec 4.3.3.3: exterior rings must have POSITIVE area by the
        // surveyor's formula on tile coordinates; interior rings NEGATIVE.
        // This pins the winding fix (issue #112) at the command-stream level.
        let bounds = test_bounds();
        let poly = polygon![
            exterior: [
                (x: 0.1, y: 0.1),
                (x: 0.9, y: 0.1),
                (x: 0.9, y: 0.9),
                (x: 0.1, y: 0.9),
                (x: 0.1, y: 0.1),
            ],
            interiors: [
                [
                    (x: 0.4, y: 0.4),
                    (x: 0.6, y: 0.4),
                    (x: 0.6, y: 0.6),
                    (x: 0.4, y: 0.6),
                    (x: 0.4, y: 0.4),
                ],
            ],
        ];
        let commands = encode_polygon(&poly, &bounds, 4096);

        // Decode the command stream into rings of absolute tile coords.
        let mut rings: Vec<Vec<(i64, i64)>> = Vec::new();
        let mut cur: Vec<(i64, i64)> = Vec::new();
        let (mut cx, mut cy) = (0i64, 0i64);
        let mut i = 0;
        while i < commands.len() {
            let (cmd, count) = command_decode(commands[i]);
            i += 1;
            match cmd {
                CMD_MOVE_TO | CMD_LINE_TO => {
                    for _ in 0..count {
                        cx += i64::from(zigzag_decode(commands[i]));
                        cy += i64::from(zigzag_decode(commands[i + 1]));
                        i += 2;
                        cur.push((cx, cy));
                    }
                }
                CMD_CLOSE_PATH => rings.push(std::mem::take(&mut cur)),
                _ => panic!("unexpected command"),
            }
        }
        assert_eq!(rings.len(), 2, "exterior + hole");

        let area2 = |ring: &[(i64, i64)]| -> i64 {
            let n = ring.len();
            (0..n)
                .map(|j| {
                    let (x0, y0) = ring[j];
                    let (x1, y1) = ring[(j + 1) % n];
                    x0 * y1 - x1 * y0
                })
                .sum()
        };
        assert!(
            area2(&rings[0]) > 0,
            "exterior ring must have positive tile-space area, got {}",
            area2(&rings[0])
        );
        assert!(
            area2(&rings[1]) < 0,
            "interior ring must have negative tile-space area, got {}",
            area2(&rings[1])
        );
    }

    #[test]
    fn test_polygon_with_hole_correct_winding() {
        // A polygon with a hole should have:
        // - CW exterior in geographic coords (positive tile-space area)
        // - CCW interior in geographic coords (negative tile-space area)

        // Exterior: CW in geo coords (correct)
        // Interior: CCW in geo coords (correct for a hole)
        let poly = polygon![
            exterior: [
                (x: 0.0, y: 0.0),
                (x: 0.0, y: 10.0),
                (x: 10.0, y: 10.0),
                (x: 10.0, y: 0.0),
                (x: 0.0, y: 0.0),
            ],
            interiors: [
                [
                    (x: 2.0, y: 2.0),
                    (x: 8.0, y: 2.0),
                    (x: 8.0, y: 8.0),
                    (x: 2.0, y: 8.0),
                    (x: 2.0, y: 2.0),
                ],
            ],
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // After orientation, exterior stays CW and interior stays CCW
        // (in geographic coordinates: positive/negative tile-space area).
        assert_eq!(oriented.interiors().len(), 1);
        assert_eq!(poly.exterior().0, oriented.exterior().0);
        assert_eq!(poly.interiors()[0].0, oriented.interiors()[0].0);
    }

    #[test]
    fn test_polygon_with_hole_incorrect_winding_gets_corrected() {
        // A polygon where both exterior and interior have wrong winding

        // Exterior: CCW in geo coords (wrong)
        // Interior: CW in geo coords (wrong for a hole)
        let poly = polygon![
            exterior: [
                (x: 0.0, y: 0.0),
                (x: 10.0, y: 0.0),
                (x: 10.0, y: 10.0),
                (x: 0.0, y: 10.0),
                (x: 0.0, y: 0.0),
            ],
            interiors: [
                [
                    (x: 2.0, y: 2.0),
                    (x: 2.0, y: 8.0),
                    (x: 8.0, y: 8.0),
                    (x: 8.0, y: 2.0),
                    (x: 2.0, y: 2.0),
                ],
            ],
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // Both should be corrected
        assert_ne!(poly.exterior().0[1], oriented.exterior().0[1]);
        assert_ne!(poly.interiors()[0].0[1], oriented.interiors()[0].0[1]);
    }

    #[test]
    fn test_multipolygon_winding_correction() {
        // MultiPolygon should have all constituent polygons corrected

        // First polygon: wrong winding
        // Second polygon: correct winding
        let multi = geo::MultiPolygon::new(vec![
            polygon![
                (x: 0.0, y: 0.0),
                (x: 0.0, y: 1.0),
                (x: 1.0, y: 1.0),
                (x: 1.0, y: 0.0),
                (x: 0.0, y: 0.0),
            ],
            polygon![
                (x: 2.0, y: 0.0),
                (x: 3.0, y: 0.0),
                (x: 3.0, y: 1.0),
                (x: 2.0, y: 1.0),
                (x: 2.0, y: 0.0),
            ],
        ]);

        let oriented = orient_multi_polygon_for_mvt(&multi);

        assert_eq!(oriented.0.len(), 2);
        // First polygon should be corrected (was CW, now CCW)
        // Second polygon should remain unchanged (was already CCW)
    }

    #[test]
    fn test_encode_polygon_applies_winding_correction() {
        // The main encode_polygon function should apply winding correction
        let bounds = test_bounds();

        // CW polygon (wrong winding in geographic coords)
        let poly_cw = polygon![
            (x: 0.0, y: 0.0),
            (x: 0.0, y: 1.0),
            (x: 1.0, y: 1.0),
            (x: 1.0, y: 0.0),
            (x: 0.0, y: 0.0),
        ];

        // CCW polygon (correct winding in geographic coords)
        let poly_ccw = polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ];

        // Both should produce equivalent MVT output (same geometry, just different input winding)
        let commands_cw = encode_polygon(&poly_cw, &bounds, 4096);
        let commands_ccw = encode_polygon(&poly_ccw, &bounds, 4096);

        // The encoded geometry should be the same for both inputs
        // because winding correction normalizes them
        assert_eq!(commands_cw, commands_ccw);
    }

    /// Test to measure MVT encoding overhead for MultiLineString vs single LineString.
    ///
    /// Hypothesis: A MultiLineString with many short linestrings is MUCH larger than
    /// a single LineString with the same total points due to MoveTo command overhead.
    ///
    /// Each linestring in a MultiLineString requires:
    /// - MoveTo(1) command: 1 u32
    /// - First point coordinates: 2 u32 (zigzag encoded dx, dy)
    /// - LineTo(n-1) command: 1 u32
    /// - Remaining points: 2*(n-1) u32
    ///
    /// For a 2-point linestring, that's: 1 + 2 + 1 + 2 = 6 u32 per linestring
    /// For 100 2-point linestrings: 600 u32
    ///
    /// A single 200-point linestring:
    /// - MoveTo(1) command: 1 u32
    /// - First point: 2 u32
    /// - LineTo(199): 1 u32
    /// - Remaining 199 points: 398 u32
    ///
    /// Total: 402 u32
    ///
    /// Expected overhead ratio: ~1.5x for geometry commands alone,
    /// but protobuf varint encoding may amplify this.
    #[test]
    fn test_mvt_encoding_overhead_multilinestring_vs_linestring() {
        use geo::coord;
        use prost::Message;

        let bounds = TileBounds {
            lng_min: -180.0,
            lat_min: -85.0,
            lng_max: 180.0,
            lat_max: 85.0,
        };
        let extent = DEFAULT_EXTENT;

        // Create 100 separate 2-point linestrings (200 total points)
        let mut lines: Vec<LineString> = Vec::with_capacity(100);
        for i in 0..100 {
            // Each linestring spans a small portion of the tile
            let x1 = -180.0 + (i as f64 * 3.6); // spread across longitude
            let x2 = x1 + 1.0;
            let y = (i as f64) * 0.8 - 40.0; // spread across latitude
            let line = LineString::new(vec![coord! { x: x1, y: y }, coord! { x: x2, y: y }]);
            lines.push(line);
        }
        let multi_linestring = MultiLineString::new(lines);

        // Create a single linestring with 200 points
        let mut single_coords: Vec<geo::Coord<f64>> = Vec::with_capacity(200);
        for i in 0..200 {
            let x = -180.0 + (i as f64 * 1.8);
            let y = (i as f64) * 0.4 - 40.0;
            single_coords.push(coord! { x: x, y: y });
        }
        let single_linestring = LineString::new(single_coords);

        // Encode MultiLineString
        let multi_cmds = encode_multi_linestring(&multi_linestring, &bounds, extent);

        // Encode single LineString
        let single_cmds = encode_linestring(&single_linestring, &bounds, extent);

        println!("\n=== MVT Encoding Overhead Test ===");
        println!("MultiLineString: 100 linestrings x 2 points each = 200 total points");
        println!("Single LineString: 200 points");
        println!();
        println!("Geometry command counts:");
        println!("  MultiLineString: {} u32 values", multi_cmds.len());
        println!("  Single LineString: {} u32 values", single_cmds.len());
        println!(
            "  Command overhead ratio: {:.2}x",
            multi_cmds.len() as f64 / single_cmds.len() as f64
        );

        // Now encode to full MVT tiles and compare byte sizes
        let mut multi_layer = LayerBuilder::new("test");
        multi_layer.add_feature(
            Some(1),
            &Geometry::MultiLineString(multi_linestring.clone()),
            &[],
            &bounds,
        );
        let multi_tile = TileBuilder::new();
        let mut multi_builder = multi_tile;
        multi_builder.add_layer(multi_layer.build());
        let multi_tile_proto = multi_builder.build();
        let multi_bytes = multi_tile_proto.encode_to_vec();

        let mut single_layer = LayerBuilder::new("test");
        single_layer.add_feature(
            Some(1),
            &Geometry::LineString(single_linestring.clone()),
            &[],
            &bounds,
        );
        let single_tile = TileBuilder::new();
        let mut single_builder = single_tile;
        single_builder.add_layer(single_layer.build());
        let single_tile_proto = single_builder.build();
        let single_bytes = single_tile_proto.encode_to_vec();

        println!();
        println!("MVT protobuf byte sizes:");
        println!("  MultiLineString tile: {} bytes", multi_bytes.len());
        println!("  Single LineString tile: {} bytes", single_bytes.len());
        println!(
            "  Byte overhead ratio: {:.2}x",
            multi_bytes.len() as f64 / single_bytes.len() as f64
        );
        println!();
        println!(
            "Overhead per linestring: {} extra bytes",
            (multi_bytes.len() - single_bytes.len()) / 100
        );

        // Verify the hypothesis: MultiLineString should be significantly larger
        assert!(
            multi_cmds.len() > single_cmds.len(),
            "MultiLineString should have more command values than single LineString"
        );

        // The ratio should be around 1.5x for 2-point linestrings
        let cmd_ratio = multi_cmds.len() as f64 / single_cmds.len() as f64;
        assert!(
            cmd_ratio > 1.2,
            "Command overhead ratio should be >1.2x, got {:.2}x",
            cmd_ratio
        );
    }

    // ------------------------------------------------------------------------
    // Quantization cleanup (#383)
    // ------------------------------------------------------------------------

    /// Decode a polygon command stream back into closed integer rings.
    fn rings_of(commands: &[u32]) -> Vec<Vec<(i32, i32)>> {
        let mut rings = Vec::new();
        let mut cur: Vec<(i32, i32)> = Vec::new();
        let (mut cx, mut cy) = (0i32, 0i32);
        let mut i = 0;
        while i < commands.len() {
            let (cmd, count) = command_decode(commands[i]);
            i += 1;
            match cmd {
                CMD_MOVE_TO | CMD_LINE_TO => {
                    for _ in 0..count {
                        cx += zigzag_decode(commands[i]);
                        cy += zigzag_decode(commands[i + 1]);
                        i += 2;
                        cur.push((cx, cy));
                    }
                }
                CMD_CLOSE_PATH => {
                    let first = cur[0];
                    cur.push(first);
                    rings.push(std::mem::take(&mut cur));
                }
                _ => panic!("bad command"),
            }
        }
        rings
    }

    /// Group decoded rings into polygons by the MVT rule: a positive-area
    /// ring starts a polygon, negative-area rings are its holes.
    fn rings_to_geo(rings: &[Vec<(i32, i32)>]) -> Vec<Polygon<f64>> {
        let ls = |r: &Vec<(i32, i32)>| {
            LineString::from(
                r.iter()
                    .map(|&(x, y)| (x as f64, y as f64))
                    .collect::<Vec<_>>(),
            )
        };
        let mut polys: Vec<Polygon<f64>> = Vec::new();
        for r in rings {
            if area2(r) > 0 {
                polys.push(Polygon::new(ls(r), vec![]));
            } else {
                let last = polys.last_mut().expect("hole before any exterior");
                let mut holes = last.interiors().to_vec();
                holes.push(ls(r));
                *last = Polygon::new(last.exterior().clone(), holes);
            }
        }
        polys
    }

    /// Shoelace area * 2 of a closed integer ring (sign = orientation).
    fn area2(ring: &[(i32, i32)]) -> i64 {
        ring.windows(2)
            .map(|w| i64::from(w[0].0) * i64::from(w[1].1) - i64::from(w[1].0) * i64::from(w[0].1))
            .sum()
    }

    /// Tile bounds and a helper mapping tile units back to lng/lat so a
    /// fixture can be designed in tile space with sub-unit offsets.
    fn unit_bounds() -> TileBounds {
        TileBounds::new(0.0, 0.0, 1.0, 1.0)
    }
    fn lnglat(x: f64, y: f64) -> (f64, f64) {
        let b = unit_bounds();
        let lng = b.lng_min + x / 4096.0 * (b.lng_max - b.lng_min);
        let top = mercator_y_fraction(b.lat_max);
        let bottom = mercator_y_fraction(b.lat_min);
        let f = top + y / 4096.0 * (bottom - top);
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * f))
            .sinh()
            .atan()
            .to_degrees();
        (lng, lat)
    }
    fn poly_in_tile_units(exterior: &[(f64, f64)]) -> Polygon<f64> {
        Polygon::new(
            LineString::from(
                exterior
                    .iter()
                    .map(|&(x, y)| lnglat(x, y))
                    .collect::<Vec<_>>(),
            ),
            vec![],
        )
    }

    /// A valid source ring whose vertices at x=100 and x=99.7 snap to the
    /// same column, so the quantized ring runs down the column it already
    /// ran up (the shape from #383: raster-derived field boundaries with
    /// near-collinear vertices). The encoder must emit rings that are valid
    /// in tile space, and the fill must be preserved.
    #[test]
    fn quantization_fold_is_repaired_into_valid_rings() {
        use geo::Validation;
        let ext = [
            (100.0, 100.0),
            (100.0, 159.0),
            (94.0, 159.0),
            (94.0, 168.0),
            (69.0, 168.0),
            (69.0, 151.0),
            (78.0, 151.0),
            (78.0, 142.0),
            (99.7, 142.0),
            (99.7, 117.0),
            (94.0, 117.0),
            (94.0, 100.0),
            (100.0, 100.0),
        ];
        let poly = poly_in_tile_units(&ext);
        assert!(poly.is_valid(), "the source polygon is valid");

        let commands = encode_polygon(&poly, &unit_bounds(), 4096);
        let rings = rings_of(&commands);
        assert!(!rings.is_empty(), "the polygon must not vanish");
        // Every emitted polygon is valid in tile space (the closed neck
        // splits the shape in two, which is the correct tile geometry).
        let out = rings_to_geo(&rings);
        assert_eq!(out.len(), 2, "{rings:?}");
        for p in &out {
            assert!(p.is_valid(), "encoded rings must be valid: {rings:?}");
        }
        // Orientation per the MVT spec: exterior positive on tile coords.
        assert!(area2(&rings[0]) > 0, "exterior must have positive area");
        // Fill preserved: compare against the shoelace of the raw snapped
        // ring, whose overlapping edges cancel exactly.
        let snapped: Vec<(i32, i32)> = ext
            .iter()
            .map(|&(x, y)| (x.round() as i32, y.round() as i32))
            .collect();
        let expected = area2(&snapped).abs();
        let got: i64 = rings.iter().map(|r| area2(r)).sum::<i64>().abs();
        assert_eq!(got, expected, "fill must survive the repair");
    }

    /// A ring whose vertices all snap onto one line has no area and must be
    /// dropped rather than emitted as a degenerate polygon.
    #[test]
    fn ring_that_collapses_to_a_line_is_dropped() {
        let poly = poly_in_tile_units(&[
            (10.0, 10.0),
            (10.2, 20.0),
            (9.8, 30.0),
            (10.1, 20.0),
            (10.0, 10.0),
        ]);
        let commands = encode_polygon(&poly, &unit_bounds(), 4096);
        assert!(commands.is_empty(), "got {commands:?}");
    }

    /// Consecutive vertices that snap to the same point are emitted once.
    #[test]
    fn duplicate_snapped_vertices_are_removed() {
        let poly = poly_in_tile_units(&[
            (10.0, 10.0),
            (10.2, 10.1),
            (50.0, 10.0),
            (50.0, 50.0),
            (10.0, 50.0),
            (10.0, 10.0),
        ]);
        let rings = rings_of(&encode_polygon(&poly, &unit_bounds(), 4096));
        assert_eq!(rings.len(), 1);
        assert_eq!(
            rings[0].len(),
            5,
            "4 distinct vertices + close: {:?}",
            rings[0]
        );
    }

    /// A polygon that is already clean encodes exactly as before: same
    /// vertices, exterior positive, hole negative.
    #[test]
    fn clean_polygon_encodes_unchanged_with_spec_orientation() {
        let poly = Polygon::new(
            LineString::from(
                [
                    (10.0, 10.0),
                    (90.0, 10.0),
                    (90.0, 90.0),
                    (10.0, 90.0),
                    (10.0, 10.0),
                ]
                .iter()
                .map(|&(x, y)| lnglat(x, y))
                .collect::<Vec<_>>(),
            ),
            vec![LineString::from(
                [
                    (40.0, 40.0),
                    (60.0, 40.0),
                    (60.0, 60.0),
                    (40.0, 60.0),
                    (40.0, 40.0),
                ]
                .iter()
                .map(|&(x, y)| lnglat(x, y))
                .collect::<Vec<_>>(),
            )],
        );
        let rings = rings_of(&encode_polygon(&poly, &unit_bounds(), 4096));
        assert_eq!(rings.len(), 2);
        assert_eq!(rings[0].len(), 5);
        assert_eq!(rings[1].len(), 5);
        assert!(area2(&rings[0]) > 0);
        assert!(area2(&rings[1]) < 0);
        assert_eq!(area2(&rings[0]).abs(), 2 * 80 * 80);
        assert_eq!(area2(&rings[1]).abs(), 2 * 20 * 20);
    }

    /// A neck that closes to a single point leaves the overlay with one
    /// contour that touches itself at a vertex (a "pinch"). Fill renderers
    /// cope, but it is not a valid ring; it must come out as two polygons.
    #[test]
    fn pinched_ring_is_split_into_separate_polygons() {
        use geo::Validation;
        // Box A (10..50 × 10..50) joined to box B (50..80 × 20..40) by a neck
        // 0.4 units tall at x=50 that snaps to zero height, so the two
        // boxes meet at the single vertex (50, 30).
        let ext = [
            (10.0, 10.0),
            (50.0, 10.0),
            (50.0, 29.8),
            (80.0, 20.0),
            (80.0, 40.0),
            (50.0, 30.2),
            (50.0, 50.0),
            (10.0, 50.0),
            (10.0, 10.0),
        ];
        let poly = poly_in_tile_units(&ext);
        assert!(poly.is_valid());
        let rings = rings_of(&encode_polygon(&poly, &unit_bounds(), 4096));
        let out = rings_to_geo(&rings);
        assert_eq!(out.len(), 2, "pinch must split: {rings:?}");
        for p in &out {
            assert!(p.is_valid(), "{rings:?}");
        }
        let snapped: Vec<(i32, i32)> = ext
            .iter()
            .map(|&(x, y)| (x.round() as i32, y.round() as i32))
            .collect();
        let got: i64 = rings.iter().map(|r| area2(r)).sum::<i64>().abs();
        assert_eq!(got, area2(&snapped).abs());
    }

    /// A bite whose loop runs the other way is a hole touching the boundary,
    /// which is valid and must stay one polygon with one hole.
    #[test]
    fn pinched_reverse_loop_becomes_a_touching_hole() {
        use geo::Validation;
        // Square with an inner loop entered and left through the same
        // vertex (40, 40), traversed clockwise relative to the exterior.
        let ext = [
            (10.0, 10.0),
            (90.0, 10.0),
            (90.0, 90.0),
            (10.0, 90.0),
            (10.0, 40.0),
            (40.0, 40.0),
            (40.0, 60.0),
            (60.0, 60.0),
            (60.0, 40.2),
            (40.0, 39.8),
            (10.0, 39.8),
            (10.0, 10.0),
        ];
        let poly = poly_in_tile_units(&ext);
        assert!(poly.is_valid());
        let rings = rings_of(&encode_polygon(&poly, &unit_bounds(), 4096));
        let out = rings_to_geo(&rings);
        assert_eq!(out.len(), 1, "{rings:?}");
        assert_eq!(out[0].interiors().len(), 1, "{rings:?}");
        assert!(out[0].is_valid(), "{rings:?}");
    }

    /// A ring lifted from real output after the overlay repair (#383): it
    /// still visits (42, -1) twice — a notch whose 0.8-unit channel snapped
    /// shut. Whatever the overlay returns, the cleaner must not emit a ring
    /// that touches itself; here the loop runs against the exterior's sense
    /// inside the fill, so it is a hole touching the boundary at that point.
    #[test]
    fn real_pinched_ring_from_field_data_becomes_a_touching_hole() {
        use geo::Validation;
        let ring: Vec<(i32, i32)> = vec![
            (0, 0),
            (0, -10),
            (9, -10),
            (9, -35),
            (17, -35),
            (17, -52),
            (25, -52),
            (25, -86),
            (34, -86),
            (34, -103),
            (42, -103),
            (42, -94),
            (76, -94),
            (76, -44),
            (67, -44),
            (67, -10),
            (76, -10),
            (76, -1),
            (67, -1),
            (67, 0),
            (42, 0),
            (42, -1),
            (51, -1),
            (51, -10),
            (42, -10),
            (42, -1),
            (34, -1),
            (34, 0),
            (0, 0),
        ];
        let ring: TileRing = ring.into_iter().map(|(x, y)| (x + 200, y + 200)).collect();
        let before = ring_area2(&ring).abs();
        let polys = clean_tile_polygon(vec![ring]);
        let flat: Vec<TileRing> = polys.iter().flatten().cloned().collect();
        let out = rings_to_geo(&flat);
        assert_eq!(out.len(), 1, "{flat:?}");
        assert_eq!(out[0].interiors().len(), 1, "{flat:?}");
        assert!(out[0].is_valid(), "{flat:?}");
        let after: i64 = flat.iter().map(|r| ring_area2(r)).sum::<i64>().abs();
        assert_eq!(after, before, "fill must be preserved");
    }

    /// Two parts of a multipolygon that overlap each other (each valid on
    /// its own) must come out as one polygon covering their union — under
    /// even-odd a renderer would punch the overlap out as a hole.
    #[test]
    fn overlapping_multipolygon_parts_are_unioned() {
        use geo::Validation;
        let sq = |x0: f64, y0: f64, x1: f64, y1: f64| {
            Polygon::new(
                LineString::from(
                    [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]
                        .iter()
                        .map(|&(x, y)| lnglat(x, y))
                        .collect::<Vec<_>>(),
                ),
                vec![],
            )
        };
        let mp = MultiPolygon::new(vec![
            sq(10.0, 10.0, 50.0, 50.0),
            sq(30.0, 30.0, 70.0, 70.0),
            sq(200.0, 200.0, 210.0, 210.0),
        ]);
        let rings = rings_of(&encode_multi_polygon(&mp, &unit_bounds(), 4096));
        let out = rings_to_geo(&rings);
        assert_eq!(out.len(), 2, "{rings:?}");
        for p in &out {
            assert!(p.is_valid(), "{rings:?}");
        }
        let total: i64 = rings.iter().map(|r| area2(r)).sum();
        // 40*40 + 40*40 - 20*20 (overlap once) + 10*10, doubled.
        assert_eq!(total, 2 * (1600 + 1600 - 400 + 100));
    }

    /// A coarse-level ring (RDP output) that crosses itself, lifted from
    /// real z11 output that was still invalid after the first cut of #383.
    #[test]
    fn crossing_ring_from_field_data_is_repaired() {
        use geo::Validation;
        let ring: Vec<(i32, i32)> = vec![
            (0, 0),
            (0, -6),
            (31, -2),
            (31, 11),
            (46, 19),
            (44, 38),
            (33, 36),
            (37, 27),
            (25, 36),
            (0, 19),
            (4, 4),
            (14, 4),
            (16, 8),
            (21, 6),
            (0, 0),
        ];
        let ring: TileRing = ring.into_iter().map(|(x, y)| (x + 100, y + 100)).collect();
        assert!(!ring_is_simple(&ring), "fixture must self-intersect");
        let polys = clean_tile_polygon(vec![ring]);
        let flat: Vec<TileRing> = polys.iter().flatten().cloned().collect();
        assert!(!flat.is_empty());
        for p in rings_to_geo(&flat) {
            assert!(p.is_valid(), "{flat:?}");
        }
    }

    // ------------------------------------------------------------------------
    // Review fixes on #383: adjacency fold, empty features, sweeps, regrouping
    // ------------------------------------------------------------------------

    /// Close an open vertex list into a `TileRing`.
    fn closed(pts: &[(i32, i32)]) -> TileRing {
        let mut r: TileRing = pts.to_vec();
        r.push(pts[0]);
        r
    }

    /// A collinear vertex at ring[0] (RDP never tests the ring's first
    /// vertex, so snapping leaves these behind constantly) is not a fold:
    /// the ring must read as simple whichever vertex it starts at.
    #[test]
    fn collinear_first_vertex_is_not_a_fold() {
        let square = [(50, 0), (100, 0), (100, 100), (0, 100), (0, 0)];
        assert!(ring_is_simple(&closed(&square)), "collinear at ring[0]");
        let mut rotated = square.to_vec();
        rotated.rotate_left(1); // the collinear vertex is now ring[4]
        assert!(ring_is_simple(&closed(&rotated)), "collinear at ring[n-1]");
        // A genuine fold at ring[0]: ring[n-1] -> ring[0] -> ring[1] runs
        // (0,0) -> (100,0) -> (50,0), back along the same line.
        let fold = [(100, 0), (50, 0), (100, 100), (0, 100), (0, 0)];
        assert!(!ring_is_simple(&closed(&fold)), "fold at ring[0]");
        // ...and the same fold anywhere else.
        let mut fold2 = fold.to_vec();
        fold2.rotate_left(2);
        assert!(!ring_is_simple(&closed(&fold2)), "fold at ring[3]");
    }

    /// The collinear-first-vertex square is clean, so the encoder must emit
    /// it with its vertices untouched — not rotated or re-noded by a repair
    /// it does not need.
    #[test]
    fn collinear_first_vertex_square_encodes_untouched() {
        let poly = poly_in_tile_units(&[
            (50.0, 0.0),
            (100.0, 0.0),
            (100.0, 100.0),
            (0.0, 100.0),
            (0.0, 0.0),
            (50.0, 0.0),
        ]);
        let rings = rings_of(&encode_polygon(&poly, &unit_bounds(), 4096));
        assert_eq!(rings.len(), 1);
        assert_eq!(
            rings[0],
            vec![(50, 0), (100, 0), (100, 100), (0, 100), (0, 0), (50, 0)]
        );
    }

    /// A polygon that quantizes to nothing must not become a feature with
    /// an empty geometry (MVT 2.1 §4.2 requires one).
    #[test]
    fn layer_builder_skips_polygon_that_quantizes_to_nothing() {
        let poly = poly_in_tile_units(&[
            (0.001, 0.001),
            (0.00102, 0.001),
            (0.00102, 0.0011),
            (0.001, 0.0011),
            (0.001, 0.001),
        ]);
        let mut layer = LayerBuilder::new("l").with_extent(4096);
        layer.add_feature(
            Some(1),
            &Geometry::Polygon(poly),
            &[("k".to_string(), PropertyValue::Int(1))],
            &unit_bounds(),
        );
        let built = layer.build();
        assert_eq!(built.features.len(), 0, "{:?}", built.features);
    }

    /// Naive pairwise reference for [`ring_is_simple`]'s non-adjacent edge
    /// test, kept here so the sweep can be checked against it.
    fn ring_is_simple_pairwise(ring: &TileRing) -> bool {
        let n = ring.len() - 1;
        for i in 0..n {
            let (p, v, q) = (ring[(i + n - 1) % n], ring[i], ring[i + 1]);
            let cross = (i64::from(v.0) - i64::from(p.0)) * (i64::from(q.1) - i64::from(v.1))
                - (i64::from(v.1) - i64::from(p.1)) * (i64::from(q.0) - i64::from(v.0));
            let dot = (i64::from(v.0) - i64::from(p.0)) * (i64::from(q.0) - i64::from(v.0))
                + (i64::from(v.1) - i64::from(p.1)) * (i64::from(q.1) - i64::from(v.1));
            if cross == 0 && dot < 0 {
                return false;
            }
        }
        for i in 0..n {
            for j in (i + 1)..n {
                if j == i + 1 || (i == 0 && j == n - 1) {
                    continue;
                }
                if segments_meet((ring[i], ring[i + 1]), (ring[j], ring[j + 1])) {
                    return false;
                }
            }
        }
        true
    }

    fn rings_meet_pairwise(a: &TileRing, b: &TileRing) -> bool {
        (0..a.len() - 1).any(|i| {
            (0..b.len() - 1).any(|j| segments_cross_or_overlap((a[i], a[i + 1]), (b[j], b[j + 1])))
        })
    }

    /// A tiny deterministic LCG so the random-ring tests need no crate.
    fn lcg(seed: &mut u64) -> u32 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*seed >> 33) as u32
    }

    /// Random small-coordinate rings with distinct vertices, plenty of which
    /// self-intersect; the sweep must agree with the pairwise reference on
    /// every one.
    #[test]
    fn sweep_matches_pairwise_reference_on_random_rings() {
        let mut seed = 0x5eed_u64;
        let (mut simple, mut not) = (0, 0);
        for _ in 0..400 {
            let n = 3 + (lcg(&mut seed) % 12) as usize;
            let mut pts: Vec<(i32, i32)> = Vec::new();
            while pts.len() < n {
                let p = ((lcg(&mut seed) % 12) as i32, (lcg(&mut seed) % 12) as i32);
                if !pts.contains(&p) {
                    pts.push(p);
                }
            }
            let ring = closed(&pts);
            let expected = ring_is_simple_pairwise(&ring);
            assert_eq!(ring_is_simple(&ring), expected, "{ring:?}");
            if expected {
                simple += 1;
            } else {
                not += 1;
            }
        }
        assert!(simple > 20 && not > 20, "simple={simple} not={not}");

        // Two rings: the cross-ring sweep against its pairwise reference.
        let (mut meet, mut apart) = (0, 0);
        for _ in 0..400 {
            let gen = |off: i32, seed: &mut u64| {
                let n = 3 + (lcg(seed) % 6) as usize;
                let mut pts: Vec<(i32, i32)> = Vec::new();
                while pts.len() < n {
                    let p = (off + (lcg(seed) % 8) as i32, off + (lcg(seed) % 8) as i32);
                    if !pts.contains(&p) {
                        pts.push(p);
                    }
                }
                closed(&pts)
            };
            let a = gen(0, &mut seed);
            let off = (lcg(&mut seed) % 6) as i32;
            let b = gen(off, &mut seed);
            let expected = rings_meet_pairwise(&a, &b);
            assert_eq!(rings_meet(&a, &b), expected, "{a:?} {b:?}");
            if expected {
                meet += 1;
            } else {
                apart += 1;
            }
        }
        assert!(meet > 20 && apart > 20, "meet={meet} apart={apart}");
    }

    /// Two rings whose boxes are disjoint never meet, whatever their edges.
    #[test]
    fn rings_with_disjoint_boxes_do_not_meet() {
        let a = closed(&[(0, 0), (10, 0), (10, 10), (0, 10)]);
        let b = closed(&[(20, 0), (30, 0), (30, 10), (20, 10)]);
        assert!(!rings_meet(&a, &b));
        assert!(polygon_is_clean(&[a, b]));
    }

    /// An L-shaped lobe whose box contains an island in its concavity: a
    /// hole inside the island must attach to the island, not to the first
    /// exterior whose box contains it.
    #[test]
    fn pinched_hole_attaches_to_the_exterior_that_contains_it() {
        use geo::Validation;
        let ext = closed(&[
            (60, 30),
            (60, 80),
            (30, 80),
            (20, 20),
            (20, 100),
            (0, 100),
            (0, 0),
            (100, 0),
            (100, 20),
            (20, 20),
        ]);
        let hole = closed(&[(35, 45), (50, 45), (50, 60), (35, 60)]);
        let polys = regroup_pinched(vec![ext, hole]);
        assert_eq!(polys.len(), 2, "{polys:?}");
        let with_hole = polys
            .iter()
            .find(|p| p.len() == 2)
            .unwrap_or_else(|| panic!("hole dropped: {polys:?}"));
        assert!(
            with_hole[0].contains(&(60, 30)),
            "hole must land on the island: {polys:?}"
        );
        let flat: Vec<TileRing> = polys.iter().flatten().cloned().collect();
        for mut p in polys.clone() {
            orient_tile_polygon(&mut p);
            let g = tile_rings_to_polygon(&p);
            assert!(g.is_valid(), "{flat:?}");
        }
    }

    /// Point-in-ring: inside, outside, on an edge and on a vertex.
    #[test]
    fn point_in_ring_counts_boundary_as_inside() {
        let sq = closed(&[(0, 0), (10, 0), (10, 10), (0, 10)]);
        assert!(point_in_ring((5, 5), &sq));
        assert!(!point_in_ring((15, 5), &sq));
        assert!(!point_in_ring((5, -1), &sq));
        assert!(point_in_ring((10, 5), &sq), "on an edge");
        assert!(point_in_ring((0, 0), &sq), "on a vertex");
        // Concave: the notch of a C is outside.
        let c = closed(&[
            (0, 0),
            (30, 0),
            (30, 10),
            (10, 10),
            (10, 20),
            (30, 20),
            (30, 30),
            (0, 30),
        ]);
        assert!(!point_in_ring((20, 15), &c));
        assert!(point_in_ring((5, 15), &c));
        // A vertex-level ray (y equal to a vertex's y) is not double-counted.
        assert!(point_in_ring((5, 10), &c));
        assert!(point_in_ring((20, 10), &c), "on the notch edge");
    }

    /// A bowtie whose lobes cancel exactly has zero net area; that must not
    /// drop both lobes.
    #[test]
    fn zero_net_area_bowtie_keeps_a_lobe() {
        let ring = closed(&[(0, 0), (10, 5), (20, 10), (20, 0), (10, 5), (0, 10)]);
        assert_eq!(ring_area2(&ring), 0, "fixture must have zero net area");
        let polys = clean_tile_polygon(vec![ring]);
        assert!(!polys.is_empty(), "both lobes dropped");
        let area: i64 = polys.iter().flatten().map(|r| ring_area2(r).abs()).sum();
        assert!(area > 0);
    }

    /// A C-shaped mainland with an island in its concavity: the island's box
    /// is inside the mainland's, but nothing touches, so both parts must
    /// pass through untouched (no overlay, no ring rotation).
    #[test]
    fn nested_but_disjoint_multipolygon_parts_pass_through_untouched() {
        let mainland = vec![closed(&[
            (0, 0),
            (100, 0),
            (100, 20),
            (20, 20),
            (20, 80),
            (100, 80),
            (100, 100),
            (0, 100),
        ])];
        let island = vec![closed(&[(50, 40), (80, 40), (80, 60), (50, 60)])];
        let parts = vec![mainland.clone(), island.clone()];
        let out = resolve_part_overlaps(parts);
        assert_eq!(out, vec![mainland, island]);
    }

    /// An island inside a hole of the mainland is a valid multipolygon and
    /// must also pass through untouched.
    #[test]
    fn island_in_a_hole_passes_through_untouched() {
        let mainland = vec![
            closed(&[(0, 0), (100, 0), (100, 100), (0, 100)]),
            closed(&[(20, 20), (20, 80), (80, 80), (80, 20)]),
        ];
        let island = vec![closed(&[(40, 40), (60, 40), (60, 60), (40, 60)])];
        let out = resolve_part_overlaps(vec![mainland.clone(), island.clone()]);
        assert_eq!(out, vec![mainland, island]);
    }

    /// Two genuinely overlapping squares become one part with the union's
    /// area; a third, disjoint part rides through untouched.
    #[test]
    fn overlapping_parts_are_unioned_and_others_pass_through() {
        let a = vec![closed(&[(0, 0), (40, 0), (40, 40), (0, 40)])];
        let b = vec![closed(&[(20, 20), (60, 20), (60, 60), (20, 60)])];
        let c = vec![closed(&[(200, 200), (210, 200), (210, 210), (200, 210)])];
        let out = resolve_part_overlaps(vec![a, b, c.clone()]);
        assert_eq!(out.len(), 2, "{out:?}");
        let unioned = out.iter().find(|p| p[0].len() > 5).expect("union part");
        assert_eq!(unioned.len(), 1, "no holes: {unioned:?}");
        assert_eq!(ring_area2(&unioned[0]).abs(), 2 * (1600 + 1600 - 400));
        assert!(out.contains(&c), "{out:?}");
    }
}
