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
    if boxes.len() < 2 * STRIP_BOXES {
        return any_box_pair(boxes, hit);
    }
    // About the boxes the x sweep pairs each box with, from every 16th box.
    // Where that is few (a ring's edges: about two), the strips' sort and
    // lists cost more than they save. shortcut: a uniform-x estimate, so
    // boxes bunched in x with an outlier far off keep the x sweep and its
    // cost; bucket min x if a profile shows that.
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
        return any_box_pair(boxes, hit);
    }
    let (y0, y1) = boxes.iter().fold((i32::MAX, i32::MIN), |(lo, hi), b| {
        (lo.min(b.1), hi.max(b.3))
    });
    let span = i64::from(y1) - i64::from(y0) + 1;
    // Strips no lower than the mean box, so a box reaches about two.
    let heights: i64 = boxes.iter().map(|b| i64::from(b.3) - i64::from(b.1)).sum();
    let mean_height = (heights / boxes.len() as i64).max(1);
    let strips = (boxes.len() / STRIP_BOXES)
        .min(MAX_STRIPS)
        .min(usize::try_from(span / mean_height).unwrap_or(usize::MAX));
    if strips < 2 {
        return any_box_pair(boxes, hit);
    }
    let strip_of = |y: i32| ((i64::from(y) - i64::from(y0)) * strips as i64 / span) as usize;
    // `any_box_pair`'s order, so each strip is a subsequence of it.
    let mut order: Vec<u32> = (0..boxes.len() as u32).collect();
    order.sort_unstable_by_key(|&i| boxes[i as usize].0);
    // Two boxes are compared in every strip they share, so a box reaching
    // more than two strips is tall: its pairs are found once, in x order.
    let mut members: Vec<Vec<u32>> = vec![Vec::new(); strips];
    let mut tall: Vec<usize> = Vec::new();
    for (p, &i) in order.iter().enumerate() {
        let b = boxes[i as usize];
        let (lo, hi) = (strip_of(b.1), strip_of(b.3));
        if hi - lo > 1 {
            tall.push(p);
            continue;
        }
        for list in &mut members[lo..=hi] {
            list.push(i);
        }
    }
    // A tall box meets every box after it in the window; a short one only
    // the tall boxes after it.
    let mut next = 0;
    for (p, &i) in order.iter().enumerate() {
        let bi = boxes[i as usize];
        let after: &mut dyn Iterator<Item = u32> = if tall.get(next) == Some(&p) {
            next += 1;
            &mut order[p + 1..].iter().copied()
        } else {
            &mut tall[next..].iter().map(|&q| order[q])
        };
        for j in after {
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
    }
    for (s, list) in members.iter().enumerate() {
        for (p, &i) in list.iter().enumerate() {
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
                    && strip_of(bi.1.max(bj.1)) == s
                    && hit(i as usize, j as usize)
                {
                    return true;
                }
            }
        }
    }
    false
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
