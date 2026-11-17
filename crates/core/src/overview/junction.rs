//! Global junction pins for shared-border simplification (#802).
//!
//! Per-feature simplification opens slivers along the borders polygons
//! share: each neighbour runs RDP over the border with its own chord, so the
//! two keep different vertices. Pass 1 builds one dataset-wide set of
//! **pins**: vertices every polygon keeps at every level. The simplifier then
//! cuts each ring at its pins and simplifies every span in a canonical
//! direction ([`super::simplify`]), so a span two rings share comes out the
//! same in both.
//!
//! A vertex is a pin when it is
//! - a **junction**: it occurs with two or more distinct unordered neighbour
//!   pairs `{prev, next}` across all polygon rings (where a shared border
//!   meets a third polygon, ends, or branches), or
//! - a ring's start, its farthest vertex from the start, or the vertex
//!   farthest from that chord. These keep every ring at three vertices or
//!   more, and, because the set is global, a neighbour sharing the vertex
//!   keeps it too: a hole filled by another polygon has no junction at all,
//!   but both rings then cut at the same pins.
//!
//! This is tippecanoe's `-pn` node set (`serial.cpp`, `main.cpp`), keyed on
//! exact `f64` coordinates instead of 32-bit world coordinates.
//!
//! DIVERGENCE FROM TIPPECANOE: tippecanoe external-sorts the vertex triples
//! and probes a bloom filter at encode time. This keeps each vertex as a
//! hashed `(vertex, neighbour pair)` record, sorts in memory, and stores the
//! pins as a sorted `u64` array probed by binary search. A hash collision can
//! only add a pin (the two vertices look like one vertex with two neighbour
//! pairs), which keeps a vertex and never opens a gap.

use geo::{Coord, Distance, Euclidean, Line};
use geo_traits::{
    CoordTrait, GeometryCollectionTrait, GeometryTrait, GeometryType, LineStringTrait,
    MultiPolygonTrait, PolygonTrait,
};
use rayon::slice::ParallelSliceMut;

/// One vertex occurrence: `(vertex key, neighbour-pair key)`.
pub(crate) type Occurrence = (u64, u64);

/// The neighbour-pair key of a forced pin (ring start and far points). Any
/// second record for the same vertex then makes it a pin.
const FORCED: u64 = u64::MAX;

/// splitmix64's finalizer.
#[inline]
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Exact-coordinate key. `+ 0.0` folds `-0.0` into `0.0`, which compare
/// equal and land on the same tile pixel.
#[inline]
pub(crate) fn vertex_key(x: f64, y: f64) -> u64 {
    mix((x + 0.0).to_bits() ^ mix((y + 0.0).to_bits()))
}

#[inline]
fn pair_key(a: u64, b: u64) -> u64 {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    mix(lo ^ mix(hi).rotate_left(17))
}

/// The dataset's pins, sorted and unique.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct JunctionNodes {
    keys: Vec<u64>,
}

impl JunctionNodes {
    /// Reduce the occurrences of every polygon ring to the pin set.
    pub(crate) fn from_occurrences(mut occ: Vec<Occurrence>) -> Self {
        occ.par_sort_unstable();
        occ.dedup();
        // After dedup, two records for one vertex are two distinct pairs.
        let mut keys: Vec<u64> = occ
            .windows(2)
            .filter(|w| w[0].0 == w[1].0)
            .map(|w| w[0].0)
            .collect();
        keys.dedup();
        keys.shrink_to_fit(); // resident through pass 2, priced by length
        log::debug!(
            "[profile] junction pins: {} occurrence(s) -> {} pin(s)",
            occ.len(),
            keys.len()
        );
        Self { keys }
    }

    /// Rebuild from a saved plan's keys; `None` unless strictly ascending.
    pub(crate) fn from_sorted(keys: Vec<u64>) -> Option<Self> {
        keys.windows(2)
            .all(|w| w[0] < w[1])
            .then_some(Self { keys })
    }

    /// The sorted keys (the plan artifact's section).
    pub(crate) fn keys(&self) -> &[u64] {
        &self.keys
    }

    /// Whether `c` is a pin.
    #[inline]
    pub(crate) fn contains(&self, c: Coord<f64>) -> bool {
        self.keys.binary_search(&vertex_key(c.x, c.y)).is_ok()
    }
}

/// Append the occurrence records of every polygon ring in `g`. `buf` is
/// scratch reused across rings.
pub(crate) fn push_geometry<G: GeometryTrait<T = f64>>(
    g: &G,
    buf: &mut Vec<Coord<f64>>,
    out: &mut Vec<Occurrence>,
) {
    match g.as_type() {
        GeometryType::Polygon(p) => push_polygon(p, buf, out),
        GeometryType::MultiPolygon(mp) => {
            for p in mp.polygons() {
                push_polygon(&p, buf, out);
            }
        }
        GeometryType::GeometryCollection(gc) => {
            for g in gc.geometries() {
                push_geometry(&g, buf, out);
            }
        }
        _ => {}
    }
}

fn push_polygon<P: PolygonTrait<T = f64>>(
    p: &P,
    buf: &mut Vec<Coord<f64>>,
    out: &mut Vec<Occurrence>,
) {
    for r in p.exterior().into_iter().chain(p.interiors()) {
        buf.clear();
        buf.extend(r.coords().map(|c| Coord { x: c.x(), y: c.y() }));
        push_ring(buf, out);
    }
}

/// The records of one ring, closed or open.
pub(crate) fn push_ring(coords: &[Coord<f64>], out: &mut Vec<Occurrence>) {
    let mut n = coords.len();
    if n > 1 && coords[0] == coords[n - 1] {
        n -= 1;
    }
    if n < 3 {
        return;
    }
    let c = &coords[..n];
    let key = |i: usize| vertex_key(c[i].x, c[i].y);
    let (mut prev, mut mid) = (key(n - 1), key(0));
    for i in 0..n {
        let next = key((i + 1) % n);
        out.push((mid, pair_key(prev, next)));
        (prev, mid) = (mid, next);
    }
    // The start and two far points, as tippecanoe's serial.cpp picks them;
    // distance ties go to the lexicographically smaller vertex.
    let smaller = |a: Coord<f64>, b: Coord<f64>| (a.x, a.y) < (b.x, b.y);
    let farthest = |dist: &dyn Fn(Coord<f64>) -> f64| {
        let (mut best, mut far) = (0, 0.0);
        for (i, &p) in c.iter().enumerate().skip(1) {
            let d = dist(p);
            if d > far || (d == far && smaller(p, c[best])) {
                (best, far) = (i, d);
            }
        }
        best
    };
    let far = farthest(&|p| (p.x - c[0].x).powi(2) + (p.y - c[0].y).powi(2));
    let chord = Line::new(c[0], c[far]);
    let far2 = farthest(&|p| Euclidean.distance(p, &chord));
    for i in [0, far, far2] {
        out.push((key(i), FORCED));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(pts: &[(f64, f64)]) -> Vec<Coord<f64>> {
        pts.iter().map(|&(x, y)| Coord { x, y }).collect()
    }

    #[test]
    fn junctions_are_vertices_with_two_neighbour_pairs() {
        // Two unit squares sharing the edge x = 1, with a midpoint on it.
        let a = ring(&[(0., 0.), (1., 0.), (1., 0.5), (1., 1.), (0., 1.), (0., 0.)]);
        let b = ring(&[(1., 0.), (2., 0.), (2., 1.), (1., 1.), (1., 0.5), (1., 0.)]);
        let mut occ = Vec::new();
        push_ring(&a, &mut occ);
        push_ring(&b, &mut occ);
        let nodes = JunctionNodes::from_occurrences(occ);
        let pin = |x, y| nodes.contains(Coord { x, y });
        // Where the shared edge ends, each vertex has two different pairs.
        assert!(pin(1., 0.) && pin(1., 1.));
        // The shared midpoint has the pair {(1,0),(1,1)} in both rings.
        assert!(!pin(1., 0.5));
        // Ring starts are forced.
        assert!(pin(0., 0.));
        assert!(pin(-0.0, 0.0), "-0.0 keys like 0.0");
        assert_eq!(
            JunctionNodes::from_sorted(nodes.keys().to_vec()),
            Some(nodes)
        );
    }
}
