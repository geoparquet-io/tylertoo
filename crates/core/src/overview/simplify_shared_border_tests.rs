//! #802: neighbours that share a border simplify it identically under the
//! pass-1 junction pins.

use super::*;
use crate::overview::junction::push_ring;
use geo::{Coord, CoordsIter};
use std::collections::BTreeSet;

/// A jagged border from (1000, 0) to (1000, 1000), deterministic.
fn border(n: usize) -> Vec<Coord<f64>> {
    (0..=n)
        .map(|i| Coord {
            x: 1000.0 + jag(i, n, 7.0),
            y: i as f64 * 1000.0 / n as f64,
        })
        .collect()
}

/// A ±30 offset, zero at both ends.
fn jag(i: usize, n: usize, seed: f64) -> f64 {
    if i == 0 || i == n {
        0.0
    } else {
        (dither_u01(i as f64, seed) - 0.5) * 60.0
    }
}

/// Two polygons left and right of `b`, both wound the same way, so they
/// walk the border in opposite directions.
fn neighbours(b: &[Coord<f64>]) -> (Polygon<f64>, Polygon<f64>) {
    let mut left = vec![Coord { x: 0.0, y: 0.0 }];
    left.extend_from_slice(b);
    left.push(Coord { x: 0.0, y: 1000.0 });
    let mut right = vec![
        b[0],
        Coord { x: 2000.0, y: 0.0 },
        Coord {
            x: 2000.0,
            y: 1000.0,
        },
    ];
    right.extend(b.iter().rev().skip(1));
    (
        Polygon::new(LineString::new(left), vec![]),
        Polygon::new(LineString::new(right), vec![]),
    )
}

/// The pins pass 1 would build over `polys`.
fn pins_of(polys: &[&Polygon<f64>]) -> JunctionNodes {
    let mut occ = Vec::new();
    for p in polys {
        for r in std::iter::once(p.exterior()).chain(p.interiors()) {
            push_ring(&r.0, &mut occ);
        }
    }
    JunctionNodes::from_occurrences(occ)
}

/// The vertices of `on` that `g` kept.
fn kept_on(g: &Simplified, on: &[Coord<f64>]) -> BTreeSet<(u64, u64)> {
    let Simplified::Keep(g) = g else {
        panic!("dropped")
    };
    let key = |c: Coord<f64>| (c.x.to_bits(), c.y.to_bits());
    let on: BTreeSet<_> = on.iter().map(|&c| key(c)).collect();
    g.coords_iter()
        .map(key)
        .filter(|k| on.contains(k))
        .collect()
}

fn run(p: &Polygon<f64>, opts: &SimplifyOptions, pins: Option<&JunctionNodes>) -> Simplified {
    simplify_cascade_pinned(
        &Geometry::Polygon(p.clone()),
        &[5.0, 10.0, 20.0, 40.0].map(CascadeStep::geom),
        Crs::Epsg3857,
        opts,
        &crate::diagnostics::Diagnostics::default(),
        pins,
    )
}

#[test]
fn canonical_span_is_direction_independent() {
    let b = border(200);
    let rev: Vec<_> = b.iter().rev().copied().collect();
    for eps in [1.0, 5.0, 12.5, 40.0] {
        let mut back = rdp_span_canonical(&rev, eps);
        back.reverse();
        assert_eq!(rdp_span_canonical(&b, eps), back, "eps {eps}");
    }
    // Equal endpoints: the interior decides the direction.
    let mut closed = b.clone();
    closed.push(b[0]);
    let rev: Vec<_> = closed.iter().rev().copied().collect();
    let mut back = rdp_span_canonical(&rev, 12.5);
    back.reverse();
    assert_eq!(rdp_span_canonical(&closed, 12.5), back);
}

#[test]
fn jagged_shared_border_simplifies_identically() {
    let b = border(200);
    let (left, right) = neighbours(&b);
    let pins = pins_of(&[&left, &right]);
    let opts = SimplifyOptions::default();
    // Without pins each side keeps its own vertices: the #802 gap.
    assert_ne!(
        kept_on(&run(&left, &opts, None), &b),
        kept_on(&run(&right, &opts, None), &b)
    );
    let kept = kept_on(&run(&left, &opts, Some(&pins)), &b);
    assert_eq!(kept, kept_on(&run(&right, &opts, Some(&pins)), &b));
    assert!(kept.len() < b.len() / 4, "still simplifies: {}", kept.len());
}

#[test]
fn hole_filled_by_island_simplifies_identically() {
    // A closed jagged ring: the border, then back up a second jagged line.
    let b = border(120);
    let mut ring: Vec<Coord<f64>> = b.clone();
    ring.extend((1..120).rev().map(|i| Coord {
        x: 600.0 + jag(i, 120, 11.0),
        y: i as f64 * 1000.0 / 120.0,
    }));
    ring.push(b[0]);
    let outer = LineString::from(vec![
        (-1000.0, -500.0),
        (2000.0, -500.0),
        (2000.0, 1500.0),
        (-1000.0, 1500.0),
    ]);
    let donut = Polygon::new(outer, vec![LineString::new(ring.clone())]);
    // The island starts elsewhere on the same ring and walks it backwards.
    let mut island = ring[..ring.len() - 1].to_vec();
    island.rotate_left(37);
    island.reverse();
    let island = Polygon::new(LineString::new(island), vec![]);
    let pins = pins_of(&[&donut, &island]);
    let opts = SimplifyOptions::default();
    let hole = kept_on(&run(&donut, &opts, Some(&pins)), &ring);
    assert_eq!(hole, kept_on(&run(&island, &opts, Some(&pins)), &ring));
    assert!(
        hole.len() < ring.len() / 4,
        "still simplifies: {}",
        hole.len()
    );
}

#[test]
fn t_junction_of_three_polygons_simplifies_identically() {
    // left | right along the border; right split again by a jagged stem
    // from the border's midpoint to x = 2000.
    let mut b = border(200);
    let foot = 100;
    b[foot].y = 500.0;
    let stem: Vec<Coord<f64>> = (0..=100)
        .map(|i| Coord {
            x: b[foot].x + i as f64 * (2000.0 - b[foot].x) / 100.0,
            y: 500.0 + jag(i, 100, 3.0),
        })
        .collect();
    let (left, _) = neighbours(&b);
    let mut lower = vec![Coord { x: 2000.0, y: 0.0 }];
    lower.extend(stem.iter().rev());
    lower.extend(b[..foot].iter().rev());
    let lower = Polygon::new(LineString::new(lower), vec![]);
    let mut upper = b[foot..].to_vec();
    upper.push(Coord {
        x: 2000.0,
        y: 1000.0,
    });
    upper.extend(stem.iter().rev().take(100));
    let upper = Polygon::new(LineString::new(upper), vec![]);
    let pins = pins_of(&[&left, &lower, &upper]);
    assert!(pins.contains(b[foot]), "the T's foot is a junction");
    let opts = SimplifyOptions::default();
    let (l, lo, up) = (
        run(&left, &opts, Some(&pins)),
        run(&lower, &opts, Some(&pins)),
        run(&upper, &opts, Some(&pins)),
    );
    assert_eq!(kept_on(&l, &b[..=foot]), kept_on(&lo, &b[..=foot]));
    assert_eq!(kept_on(&l, &b[foot..]), kept_on(&up, &b[foot..]));
    assert_eq!(kept_on(&lo, &stem), kept_on(&up, &stem));
}

/// Under pins an invalid candidate is kept as is: the epsilon retry and the
/// even-odd repair would each move only this polygon's side of the border.
#[test]
fn invalid_candidate_keeps_the_shared_border() {
    let mut b = border(200);
    // A bump the coarse levels flatten, holding the tip of a spike that
    // reaches from the left's far side.
    for c in &mut b[99..=103] {
        c.x = 1035.0;
    }
    let (left, right) = neighbours(&b);
    let mut ext = left.exterior().0.clone();
    ext.pop();
    ext.extend([
        Coord { x: 0.0, y: 510.0 },
        Coord {
            x: 1025.0,
            y: 505.0,
        },
        Coord { x: 0.0, y: 500.0 },
    ]);
    let left = Polygon::new(LineString::new(ext), vec![]);
    let pins = pins_of(&[&left, &right]);
    let mut invalid = false;
    for cascade in [true, false] {
        let opts = SimplifyOptions::default().with_cascade(cascade);
        // Without pins the left's candidate goes invalid and is repaired or
        // retried, which is what this test guards against.
        let tol = world_tolerance(40.0, Crs::Epsg3857, &opts);
        let candidate = rdp_polygon(&left, tol, Some(&pins));
        invalid |= !polygon_is_valid(&candidate);
        assert_eq!(
            kept_on(&run(&left, &opts, Some(&pins)), &b),
            kept_on(&run(&right, &opts, Some(&pins)), &b),
            "cascade {cascade}"
        );
    }
    assert!(invalid, "the fixture must produce an invalid candidate");
}
