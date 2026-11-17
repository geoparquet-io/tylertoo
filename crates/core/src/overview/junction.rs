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
//! - a **junction**: two of its distinct unordered neighbour pairs
//!   `{prev, next}` across all polygon rings have a neighbour in common,
//!   so a run of edges two rings share starts or ends there (where a shared
//!   border meets a third polygon, ends, or branches), or
//! - a ring's start, its farthest vertex from the start, or the vertex
//!   farthest from that chord. These keep every ring at three vertices or
//!   more, and, because the set is global, a neighbour sharing the vertex
//!   keeps it too: a hole filled by another polygon has no junction at all,
//!   but both rings then cut at the same pins.
//!
//! This is tippecanoe's `-pn` node set (`serial.cpp`, `main.cpp`), keyed on
//! exact `f64` coordinates instead of 32-bit world coordinates, with
//! `-ab`'s shared-edge rule for which vertex is a junction ([`is_pin`]).
//!
//! DIVERGENCE FROM TIPPECANOE: tippecanoe external-sorts the vertex triples
//! and probes a bloom filter at encode time. This keeps each vertex as a
//! hashed `(vertex, neighbour pair)` record, sorts in memory, and stores the
//! pins as a sorted `u64` array probed by binary search. A vertex-key
//! collision can only add a pin (two vertices look like one whose pairs share
//! a neighbour); missing one takes two pairs whose 32-bit neighbour tags
//! both collide.

use geo::Coord;
use geo_traits::{
    CoordTrait, GeometryCollectionTrait, GeometryTrait, GeometryType, LineStringTrait,
    MultiPolygonTrait, PolygonTrait,
};
use rayon::prelude::*;

/// One vertex occurrence: `(vertex key, neighbour-pair key)`.
pub(crate) type Occurrence = (u64, u64);

/// The neighbour-pair key of a forced pin (ring start and far points). Any
/// second record for the same vertex then makes it a pin.
const FORCED: u64 = u64::MAX;

/// The neighbour-pair key of a vertex already known to be a pin.
const PIN: u64 = u64::MAX - 1;

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

/// The unordered neighbour pair as two 32-bit neighbour tags, smaller one
/// high, so [`shares_neighbour`] can see a common neighbour. A tag
/// collision can only show a shared neighbour that isn't, adding a pin.
#[inline]
fn pair_key(a: u64, b: u64) -> u64 {
    let (a, b) = (a >> 32, b >> 32);
    (a.min(b) << 32) | a.max(b)
}

/// Whether two different neighbour pairs of one vertex have a neighbour in
/// common: one ring's edge to it is shared and its other edge is not.
#[inline]
fn shares_neighbour(p: u64, q: u64) -> bool {
    let (p0, p1, q0, q1) = (p >> 32, p as u32 as u64, q >> 32, q as u32 as u64);
    p != q && (p0 == q0 || p0 == q1 || p1 == q0 || p1 == q1)
}

/// The dataset's pins, sorted and unique.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct JunctionNodes {
    keys: Vec<u64>,
    /// `keys[dir[b]..dir[b + 1]]` are the keys whose top bits are `b`, about
    /// four a bucket: a lookup reads two cache lines, not a binary search's
    /// twenty-odd (the keys are hashes, so the buckets stay even).
    dir: Vec<u32>,
    shift: u32,
}

impl JunctionNodes {
    /// Reduce the occurrences of every polygon ring to the pin set.
    pub(crate) fn from_occurrences(mut occ: Vec<Occurrence>) -> Self {
        occ.par_sort_unstable();
        let mut keys: Vec<u64> = occ
            .par_chunk_by(|a, b| a.0 == b.0)
            .filter(|g| is_pin(g))
            .map(|g| g[0].0)
            .collect();
        keys.shrink_to_fit(); // resident through pass 2, priced by length
        log::debug!(
            "[profile] junction pins: {} occurrence(s) -> {} pin(s)",
            occ.len(),
            keys.len()
        );
        Self::indexed(keys)
    }

    /// Rebuild from a saved plan's keys; `None` unless strictly ascending.
    pub(crate) fn from_sorted(keys: Vec<u64>) -> Option<Self> {
        keys.windows(2)
            .all(|w| w[0] < w[1])
            .then(|| Self::indexed(keys))
    }

    fn indexed(keys: Vec<u64>) -> Self {
        if u32::try_from(keys.len()).is_err() {
            // Past u32 offsets: no directory, `contains` binary-searches.
            return Self {
                keys,
                dir: Vec::new(),
                shift: 0,
            };
        }
        let bits = (keys.len() / 4).max(2).next_power_of_two().trailing_zeros();
        let shift = 64 - bits;
        let mut dir = vec![0u32; (1 << bits) + 1];
        for &k in &keys {
            dir[(k >> shift) as usize + 1] += 1;
        }
        for b in 1..dir.len() {
            dir[b] += dir[b - 1];
        }
        Self { keys, dir, shift }
    }

    /// The bytes the pins hold resident.
    pub(crate) fn resident_bytes(&self) -> u64 {
        (size_of_val(self.keys.as_slice()) + size_of_val(self.dir.as_slice())) as u64
    }

    /// The sorted keys (the plan artifact's section).
    pub(crate) fn keys(&self) -> &[u64] {
        &self.keys
    }

    /// Whether `c` is a pin.
    #[inline]
    pub(crate) fn contains(&self, c: Coord<f64>) -> bool {
        let k = vertex_key(c.x, c.y);
        if self.dir.is_empty() {
            return self.keys.binary_search(&k).is_ok();
        }
        let b = (k >> self.shift) as usize;
        self.keys[self.dir[b] as usize..self.dir[b + 1] as usize].contains(&k)
    }
}

/// Whether one vertex's records `g` show it is a pin: a [`PIN`] or
/// [`FORCED`] record, or two neighbour pairs with a neighbour in common,
/// where a run of edges two rings share starts or ends (tippecanoe `-ab`'s
/// rule). Pairs with nothing in common are rings that only pass through
/// the vertex: no span both hold ends there, so pinning it buys nothing.
///
/// DIVERGENCE FROM TIPPECANOE: `-pn` pins any vertex with two different
/// pairs. On field coverages whose borders almost coincide that pins every
/// stray common vertex (Moldova: 3.76M pins against 2.07M under this rule)
/// and keeps them at every level.
fn is_pin(g: &[Occurrence]) -> bool {
    g.iter().any(|o| o.1 >= PIN)
        || g.iter()
            .enumerate()
            .any(|(i, p)| g[i + 1..].iter().any(|q| shares_neighbour(p.1, q.1)))
}

/// Sort and shrink one chunk's occurrences: duplicates go (both sides of a
/// border in the chunk), and a vertex the chunk already shows is a pin keeps
/// one [`PIN`] record.
pub(crate) fn compact(occ: &mut Vec<Occurrence>) {
    occ.sort_unstable();
    occ.dedup();
    // In place: a pin keeps one record, any other vertex all of its pairs.
    let (mut w, mut i) = (0, 0);
    while i < occ.len() {
        let n = occ[i..].partition_point(|o| o.0 == occ[i].0);
        if is_pin(&occ[i..i + n]) {
            occ[w] = (occ[i].0, PIN);
            w += 1;
        } else {
            occ.copy_within(i..i + n, w);
            w += n;
        }
        i += n;
    }
    occ.truncate(w);
}

/// Append a batch's occurrences (one `Vec` per chunk) to `acc` if `fits`
/// the bytes `acc` would then hold, priced before it grows, plus the key
/// vector [`JunctionNodes::from_occurrences`] collects beside it. Otherwise drop
/// them all, record [`Diagnostic::SharedBordersUnpinned`] and return
/// `false`: the run simplifies without pins rather than run out of memory.
///
/// [`Diagnostic::SharedBordersUnpinned`]: crate::diagnostics::Diagnostic::SharedBordersUnpinned
pub(crate) fn absorb(
    acc: &mut Vec<Occurrence>,
    batch: &mut Vec<Vec<Occurrence>>,
    fits: impl FnOnce(u64) -> bool,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> bool {
    let need = acc.len() + batch.iter().map(Vec::len).sum::<usize>();
    if need == 0 {
        batch.clear();
        return true;
    }
    // The capacity amortized growth would allocate.
    let cap = if need > acc.capacity() {
        need.max(2 * acc.capacity())
    } else {
        acc.capacity()
    };
    let reduction = need * size_of::<u64>();
    if fits((cap * size_of::<Occurrence>() + reduction) as u64) {
        acc.reserve_exact(cap - acc.len());
        batch.drain(..).for_each(|chunk| acc.extend(chunk));
        return true;
    }
    diagnostics.record(crate::diagnostics::Diagnostic::SharedBordersUnpinned { count: 1 });
    *acc = Vec::new();
    batch.clear();
    false
}

/// The occurrences of `geoms`' polygons, one sorted, deduplicated `Vec` per
/// chunk of rows, built in parallel.
pub(crate) fn occurrences_of(geoms: &[geo::Geometry<f64>]) -> Vec<Vec<Occurrence>> {
    geoms
        .par_chunks(1024)
        .map(|chunk| {
            let (mut buf, mut out) = (Vec::new(), Vec::new());
            for g in chunk {
                push_geometry(g, &mut buf, &mut out);
            }
            compact(&mut out);
            out
        })
        .collect()
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
    out.reserve(n + 3);
    let key = |i: usize| vertex_key(c[i].x, c[i].y);
    let (mut prev, mut mid) = (key(n - 1), key(0));
    for i in 0..n {
        let next = key((i + 1) % n);
        out.push((mid, pair_key(prev, next)));
        (prev, mid) = (mid, next);
    }
    // The start and two far points, as tippecanoe's serial.cpp picks them.
    let o = c[0];
    let far = farthest(c, |p| (p.x - o.x).powi(2) + (p.y - o.y).powi(2));
    // Off the chord's line: twice the triangle's area, its length a constant.
    let (dx, dy) = (c[far].x - o.x, c[far].y - o.y);
    let far2 = farthest(c, |p| (dx * (p.y - o.y) - dy * (p.x - o.x)).abs());
    for i in [0, far, far2] {
        out.push((key(i), FORCED));
    }
}

/// The vertex of `c` after the first farthest by `dist`; ties go to the
/// lexicographically smaller vertex.
fn farthest(c: &[Coord<f64>], dist: impl Fn(Coord<f64>) -> f64) -> usize {
    let (mut best, mut far) = (0, 0.0);
    for (i, &p) in c.iter().enumerate().skip(1) {
        let d = dist(p);
        if d > far || (d == far && (p.x, p.y) < (c[best].x, c[best].y)) {
            (best, far) = (i, d);
        }
    }
    best
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

    /// Two rings that only pass through a vertex, sharing no edge there, do
    /// not make it a pin: no span both rings hold starts or ends at it
    /// (tippecanoe `-ab`'s rule, not `-pn`'s).
    #[test]
    fn only_a_shared_edge_ending_makes_a_pin() {
        let k = |x: f64| vertex_key(x, 0.0);
        let (v, w) = (k(0.0), k(9.0));
        let mut occ = vec![
            (v, pair_key(k(1.0), k(2.0))),
            (v, pair_key(k(3.0), k(4.0))),
            (w, pair_key(k(1.0), k(2.0))),
            (w, pair_key(k(2.0), k(5.0))),
        ];
        let nodes = JunctionNodes::from_occurrences(occ.clone());
        assert_eq!(nodes.keys(), &[w]);
        compact(&mut occ);
        assert_eq!(JunctionNodes::from_occurrences(occ), nodes);
    }

    /// The bucket directory finds exactly the keys a binary search would.
    #[test]
    fn directory_lookup_matches_the_sorted_keys() {
        let coords: Vec<Coord<f64>> = (0..5000)
            .map(|i| Coord {
                x: f64::from(i % 97),
                y: f64::from(i / 97),
            })
            .collect();
        let mut keys: Vec<u64> = coords
            .iter()
            .step_by(3)
            .map(|c| vertex_key(c.x, c.y))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        let nodes = JunctionNodes::from_sorted(keys.clone()).unwrap();
        for c in &coords {
            let k = vertex_key(c.x, c.y);
            assert_eq!(nodes.contains(*c), keys.binary_search(&k).is_ok());
        }
    }

    /// Compacting chunks first gives the same pins as reducing every record.
    #[test]
    fn compacted_chunks_keep_the_pins() {
        // A 6 x 6 grid of unit squares, each split in its own chunk or not.
        let mut rings = Vec::new();
        for x in 0..6 {
            for y in 0..6 {
                let (x, y) = (f64::from(x), f64::from(y));
                rings.push(ring(&[
                    (x, y),
                    (x + 1., y),
                    (x + 1., y + 1.),
                    (x, y + 1.),
                    (x, y),
                ]));
            }
        }
        let mut all = Vec::new();
        for r in &rings {
            push_ring(r, &mut all);
        }
        let whole = JunctionNodes::from_occurrences(all);
        for per_chunk in [1, 5, 36] {
            let mut occ = Vec::new();
            for chunk in rings.chunks(per_chunk) {
                let mut c = Vec::new();
                for r in chunk {
                    push_ring(r, &mut c);
                }
                compact(&mut c);
                occ.extend(c);
            }
            assert_eq!(
                JunctionNodes::from_occurrences(occ),
                whole,
                "{per_chunk} a chunk"
            );
        }
    }
}
