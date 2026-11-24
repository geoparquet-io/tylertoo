//! The one search for pairs of boxes that meet, shared by every validity
//! and crossing check that needs edge or part pairs: the integer tile
//! checks (`mvt`, `mvt::tile_polygon`) and the f64 ring checks (`clip`,
//! `overview::simplify` through `clip::any_edge_pair`), and the coverage
//! fill's grouping of touching shapes.

use std::cmp::Ordering;

/// A box coordinate the sweep can sort: a total order, so an f64 NaN sorts
/// (last) instead of breaking the sort.
pub(crate) trait SweepCoord: Copy + PartialOrd {
    fn sweep_cmp(&self, other: &Self) -> Ordering;
}

impl SweepCoord for i32 {
    fn sweep_cmp(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

impl SweepCoord for f64 {
    fn sweep_cmp(&self, other: &Self) -> Ordering {
        self.total_cmp(other)
    }
}

#[cfg(test)]
thread_local! {
    /// Box pairs a sweep compared, on this thread.
    pub(crate) static SWEEP_CANDIDATES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Visit every pair of boxes that meet, once, and stop at the first pair
/// `hit` accepts. A plane sweep over x: boxes are sorted by their min x and
/// each is paired with the following ones until their min x passes its max
/// x, so the work is O(n log n + k) for k box-meeting pairs rather than
/// O(n²). Which of a pair comes first in `hit(i, j)` follows the sort, not
/// the input order.
pub(crate) fn any_box_pair<T: SweepCoord>(
    boxes: &[(T, T, T, T)],
    mut hit: impl FnMut(usize, usize) -> bool,
) -> bool {
    let mut order: Vec<u32> = (0..boxes.len() as u32).collect();
    order.sort_unstable_by(|&a, &b| boxes[a as usize].0.sweep_cmp(&boxes[b as usize].0));
    for (p, &i) in order.iter().enumerate() {
        let bi = boxes[i as usize];
        for &j in &order[p + 1..] {
            let bj = boxes[j as usize];
            if bj.0 > bi.2 {
                break;
            }
            #[cfg(test)]
            SWEEP_CANDIDATES.with(|c| c.set(c.get() + 1));
            // Written as "not apart" so a NaN coordinate still reaches `hit`.
            if !(bi.3 < bj.1 || bj.3 < bi.1) && hit(i as usize, j as usize) {
                return true;
            }
        }
    }
    false
}

/// Boxes per horizontal strip in [`strip_box_pairs`].
const STRIP_BOXES: usize = 256;

/// Most strips [`strip_box_pairs`] cuts the boxes' y range into.
const MAX_STRIPS: usize = 1024;

/// Mean boxes sharing a box's x range below which [`strip_box_pairs`] keeps
/// the x sweep.
const X_SWEEP_DENSITY: i64 = 8;

/// [`any_box_pair`] in horizontal strips: the same pairs, each with the same
/// one first in `hit(i, j)`, but not in the same order. Each strip runs the
/// x sweep over the boxes that reach it, and a pair is visited only in the
/// strip holding the bottom of the two boxes' overlap; a box reaching more
/// than two strips is paired by the x sweep instead. An x-only sweep pairs
/// every box with all the boxes that share its x range, however far apart
/// in y: on a merged feature spanning the tile (#772) that is about 170
/// candidates per pair that meets.
pub(crate) fn strip_box_pairs(
    boxes: &[(i32, i32, i32, i32)],
    mut hit: impl FnMut(usize, usize) -> bool,
) -> bool {
    let Some(s) = Strips::cut(boxes) else {
        return any_box_pair(boxes, hit);
    };
    (0..boxes.len()).any(|p| s.tall_run(boxes, p, &mut hit))
        || (0..s.members.len())
            .any(|k| (0..s.members[k].len()).any(|p| s.strip_run(boxes, k, p, &mut hit)))
}

/// A sweep's callback: true stops the sweep.
type PairHit<'a> = dyn FnMut(usize, usize) -> bool + 'a;

/// [`strip_box_pairs`]'s pairs, each given to `found` from rayon's workers
/// in no fixed order; what it returns for them is collected (#908: one
/// merged feature can hold a million parts).
pub(crate) fn par_strip_box_pairs<T: Send>(
    boxes: &[(i32, i32, i32, i32)],
    found: impl Fn(usize, usize) -> Option<T> + Sync,
) -> Vec<T> {
    use rayon::prelude::*;
    // One run's pairs, collected by the worker that sweeps it.
    let collect = |run: &dyn Fn(&mut PairHit<'_>) -> bool| {
        let mut out = Vec::new();
        run(&mut |i, j| {
            out.extend(found(i, j));
            false
        });
        out
    };
    if boxes.len() < 2 * STRIP_BOXES {
        return collect(&|hit| any_box_pair(boxes, hit));
    }
    match Strips::cut(boxes) {
        Some(s) => (0..boxes.len())
            .into_par_iter()
            .flat_map_iter(|p| collect(&|hit| s.tall_run(boxes, p, &mut |i, j| hit(i, j))))
            .chain(s.members.par_iter().enumerate().flat_map_iter(|(k, list)| {
                (0..list.len())
                    .flat_map(|p| collect(&|hit| s.strip_run(boxes, k, p, &mut |i, j| hit(i, j))))
                    .collect::<Vec<_>>()
            }))
            .collect(),
        None => {
            let order = x_order(boxes);
            (0..order.len())
                .into_par_iter()
                .flat_map_iter(|p| collect(&|hit| x_run(boxes, &order[p..], &mut |i, j| hit(i, j))))
                .collect()
        }
    }
}

/// Indices of `boxes` by min x: [`any_box_pair`]'s order.
fn x_order(boxes: &[(i32, i32, i32, i32)]) -> Vec<u32> {
    let mut order: Vec<u32> = (0..boxes.len() as u32).collect();
    order.sort_unstable_by_key(|&i| boxes[i as usize].0);
    order
}

/// The x sweep's pairs of `run[0]` with the boxes after it in `run`.
fn x_run(
    boxes: &[(i32, i32, i32, i32)],
    run: &[u32],
    hit: &mut impl FnMut(usize, usize) -> bool,
) -> bool {
    let i = run[0];
    let bi = boxes[i as usize];
    for &j in &run[1..] {
        let bj = boxes[j as usize];
        if bj.0 > bi.2 {
            break;
        }
        #[cfg(test)]
        SWEEP_CANDIDATES.with(|c| c.set(c.get() + 1));
        if bj.1 <= bi.3 && bi.1 <= bj.3 && hit(i as usize, j as usize) {
            return true;
        }
    }
    false
}

/// [`strip_box_pairs`]'s cut of the boxes into horizontal strips.
struct Strips {
    /// All boxes by min x.
    order: Vec<u32>,
    /// Positions in `order` of the boxes reaching more than two strips.
    tall: Vec<usize>,
    /// Per strip, the other boxes reaching it, by min x.
    members: Vec<Vec<u32>>,
    y0: i32,
    span: i64,
}

impl Strips {
    /// The strips for `boxes`, or `None` where the x sweep is cheaper.
    fn cut(boxes: &[(i32, i32, i32, i32)]) -> Option<Self> {
        if boxes.len() < 2 * STRIP_BOXES {
            return None;
        }
        // About the boxes the x sweep pairs each box with, from every 16th
        // box. Where that is few (a ring's edges: about two), the strips'
        // sort and lists cost more than they save. shortcut: a uniform-x
        // estimate, so boxes bunched in x with an outlier far off keep the
        // x sweep and its cost; bucket min x if a profile shows that.
        let (x0, x1, widths) =
            boxes
                .iter()
                .step_by(16)
                .fold((i32::MAX, i32::MIN, 0_i64), |(lo, hi, w), b| {
                    (
                        lo.min(b.0),
                        hi.max(b.2),
                        // Inclusive: a vertical edge still shares its x.
                        w + i64::from(b.2) - i64::from(b.0) + 1,
                    )
                });
        if 16 * widths < X_SWEEP_DENSITY * (i64::from(x1) - i64::from(x0) + 1) {
            return None;
        }
        let (y0, y1) = boxes.iter().fold((i32::MAX, i32::MIN), |(lo, hi), b| {
            (lo.min(b.1), hi.max(b.3))
        });
        let span = i64::from(y1) - i64::from(y0) + 1;
        // Strips no lower than the mean box, so a box reaches about two.
        let heights: i64 = boxes.iter().map(|b| i64::from(b.3) - i64::from(b.1)).sum();
        let mean_height = (heights / boxes.len() as i64).max(1);
        let count = (boxes.len() / STRIP_BOXES)
            .min(MAX_STRIPS)
            .min(usize::try_from(span / mean_height).unwrap_or(usize::MAX));
        if count < 2 {
            return None;
        }
        let mut s = Self {
            // `any_box_pair`'s order, so each strip is a subsequence of it.
            order: x_order(boxes),
            tall: Vec::new(),
            members: vec![Vec::new(); count],
            y0,
            span,
        };
        // Two boxes are compared in every strip they share, so a box
        // reaching more than two strips is tall: its pairs are found once,
        // in x order.
        for p in 0..s.order.len() {
            let i = s.order[p];
            let b = boxes[i as usize];
            let (lo, hi) = (s.strip_of(b.1), s.strip_of(b.3));
            if hi - lo > 1 {
                s.tall.push(p);
                continue;
            }
            for list in &mut s.members[lo..=hi] {
                list.push(i);
            }
        }
        Some(s)
    }

    fn strip_of(&self, y: i32) -> usize {
        ((i64::from(y) - i64::from(self.y0)) * self.members.len() as i64 / self.span) as usize
    }

    /// The x-order pairs of the box at `p` in `order` that involve a tall
    /// box: a tall box meets every box after it in the window, a short one
    /// only the tall boxes after it.
    fn tall_run(
        &self,
        boxes: &[(i32, i32, i32, i32)],
        p: usize,
        hit: &mut impl FnMut(usize, usize) -> bool,
    ) -> bool {
        let next = self.tall.partition_point(|&q| q < p);
        if self.tall.get(next) == Some(&p) {
            return x_run(boxes, &self.order[p..], hit);
        }
        let i = self.order[p];
        let bi = boxes[i as usize];
        for &q in &self.tall[next..] {
            let j = self.order[q];
            let bj = boxes[j as usize];
            if bj.0 > bi.2 {
                break;
            }
            #[cfg(test)]
            SWEEP_CANDIDATES.with(|c| c.set(c.get() + 1));
            if bj.1 <= bi.3 && bi.1 <= bj.3 && hit(i as usize, j as usize) {
                return true;
            }
        }
        false
    }

    /// The pairs of the `p`th box of strip `k` with the boxes after it
    /// there, each pair only in the strip holding the bottom of its overlap.
    fn strip_run(
        &self,
        boxes: &[(i32, i32, i32, i32)],
        k: usize,
        p: usize,
        hit: &mut impl FnMut(usize, usize) -> bool,
    ) -> bool {
        let list = &self.members[k];
        let i = list[p];
        let bi = boxes[i as usize];
        for &j in &list[p + 1..] {
            let bj = boxes[j as usize];
            if bj.0 > bi.2 {
                break;
            }
            #[cfg(test)]
            SWEEP_CANDIDATES.with(|c| c.set(c.get() + 1));
            if bj.1 <= bi.3
                && bi.1 <= bj.3
                && self.strip_of(bi.1.max(bj.1)) == k
                && hit(i as usize, j as usize)
            {
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The f64 ring checks (`clip::any_edge_pair`) pair their edges through
    /// this sweep, not a copy of it.
    #[test]
    fn f64_ring_edges_go_through_the_one_sweep() {
        let mut ring: Vec<geo::Coord<f64>> = (0..64)
            .map(|k| {
                let t = f64::from(k) / 64.0 * std::f64::consts::TAU;
                geo::coord! { x: t.cos(), y: t.sin() }
            })
            .collect();
        ring.push(ring[0]);
        SWEEP_CANDIDATES.with(|c| c.set(0));
        assert!(!crate::clip::any_edge_pair(&ring, |_, _| false));
        assert!(SWEEP_CANDIDATES.with(|c| c.get()) > 0);
    }

    /// #908: the parallel sweep finds the serial sweep's pairs, on boxes cut
    /// into strips (with tall boxes) and on boxes that keep the x sweep.
    #[test]
    fn parallel_sweep_finds_the_serial_pairs() {
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut random = |n: i32| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as i32
        };
        let striped: Vec<_> = (0..20_000)
            .map(|k| {
                let (x, y) = (random(400), random(40_000));
                let h = if k % 97 == 0 { 9_000 } else { random(30) };
                (x, y, x + random(20), y + h)
            })
            .collect();
        let sparse: Vec<_> = (0..3_000)
            .map(|k| (k * 10, random(100), k * 10 + 15, random(100) + 50))
            .collect();
        assert!(Strips::cut(&striped).is_some() && Strips::cut(&sparse).is_none());
        for boxes in [&striped, &sparse] {
            let mut serial = Vec::new();
            strip_box_pairs(boxes, |i, j| {
                serial.push((i.min(j), i.max(j)));
                false
            });
            let mut parallel = par_strip_box_pairs(boxes, |i, j| Some((i.min(j), i.max(j))));
            serial.sort_unstable();
            parallel.sort_unstable();
            assert!(!serial.is_empty());
            assert_eq!(serial, parallel);
        }
    }

    /// The same boxes as i32 and as f64 meet in the same pairs, met in the
    /// same order.
    #[test]
    fn integer_and_float_boxes_pair_alike() {
        let ints: Vec<(i32, i32, i32, i32)> = (0..200)
            .map(|k: i32| {
                let (x, y) = ((k * 37) % 101, (k * 53) % 97);
                (x, y, x + k % 7, y + k % 5)
            })
            .collect();
        let floats: Vec<(f64, f64, f64, f64)> = ints
            .iter()
            .map(|&(a, b, c, d)| (a.into(), b.into(), c.into(), d.into()))
            .collect();
        fn pairs<T: SweepCoord>(boxes: &[(T, T, T, T)]) -> Vec<(usize, usize)> {
            let mut out = Vec::new();
            any_box_pair(boxes, |i, j| {
                out.push((i, j));
                false
            });
            out
        }
        let (a, b) = (pairs(&ints), pairs(&floats));
        assert!(!a.is_empty());
        assert_eq!(a, b);
    }
}
