//! Sanity checks on the wagyu-rs calls the runner makes, so a lopsided
//! scorecard can be told apart from a mis-driven engine: a square clipped by
//! an overlapping box must give the overlap, under either fill rule, in both
//! coordinate types, whichever way the rings wind.

use wagyu_rs::{FillType, Operation, Point, PolygonType, Wagyu};

fn area_i64(mp: &geo_types::MultiPolygon<i64>) -> f64 {
    mp.0.iter()
        .map(|p| {
            let r = &p.exterior().0;
            let mut s = 0.0;
            for w in r.windows(2) {
                s += (w[0].x * w[1].y - w[1].x * w[0].y) as f64;
            }
            s.abs() / 2.0
        })
        .sum()
}

fn area_f64(mp: &geo_types::MultiPolygon<f64>) -> f64 {
    mp.0.iter()
        .map(|p| {
            let r = &p.exterior().0;
            let mut s = 0.0;
            for w in r.windows(2) {
                s += w[0].x * w[1].y - w[1].x * w[0].y;
            }
            s.abs() / 2.0
        })
        .sum()
}

fn square_i64(min: i64, max: i64, ccw: bool) -> Vec<Point<i64>> {
    let mut v = vec![
        Point::new(min, min),
        Point::new(max, min),
        Point::new(max, max),
        Point::new(min, max),
    ];
    if !ccw {
        v.reverse();
    }
    v
}

fn square_f64(min: f64, max: f64, ccw: bool) -> Vec<Point<f64>> {
    let mut v = vec![
        Point::new(min, min),
        Point::new(max, min),
        Point::new(max, max),
        Point::new(min, max),
    ];
    if !ccw {
        v.reverse();
    }
    v
}

#[test]
fn i64_square_clipped_by_overlapping_box_gives_the_overlap() {
    for fill in [FillType::EvenOdd, FillType::NonZero] {
        for subj_ccw in [true, false] {
            for clip_ccw in [true, false] {
                let mut w: Wagyu<i64> = Wagyu::new();
                w.add_ring(&square_i64(0, 10, subj_ccw), PolygonType::Subject);
                w.add_ring(&square_i64(5, 15, clip_ccw), PolygonType::Clip);
                let mp = w.execute(Operation::Intersection, fill, fill).unwrap();
                assert_eq!(
                    area_i64(&mp),
                    25.0,
                    "i64 {fill:?} subj_ccw={subj_ccw} clip_ccw={clip_ccw}: {mp:?}"
                );
            }
        }
    }
}

#[test]
fn f64_square_clipped_by_overlapping_box_gives_the_overlap() {
    for fill in [FillType::EvenOdd, FillType::NonZero] {
        for subj_ccw in [true, false] {
            for clip_ccw in [true, false] {
                let mut w: Wagyu<f64> = Wagyu::new();
                w.add_ring(&square_f64(0.0, 10.0, subj_ccw), PolygonType::Subject);
                w.add_ring(&square_f64(5.0, 15.0, clip_ccw), PolygonType::Clip);
                let mp = w.execute(Operation::Intersection, fill, fill).unwrap();
                assert!(
                    (area_f64(&mp) - 25.0).abs() < 1e-9,
                    "f64 {fill:?} subj_ccw={subj_ccw} clip_ccw={clip_ccw}: {mp:?}"
                );
            }
        }
    }
}

/// Pins why the runner has no degrees-as-is column: `Wagyu<f64>` snap-rounds
/// coordinates to integers internally (hot-pixel snap rounding, `wround`), so
/// a sub-integer square collapses and the intersection comes back EMPTY
/// instead of 0.0625. wagyu is an integer engine whatever `T` says; the only
/// meaningful way to feed it degrees is to scale them onto an integer grid
/// first, which is what the runner's two i64 spaces do.
#[test]
fn f64_fractional_coordinates_are_snap_rounded_to_integers() {
    let mut w: Wagyu<f64> = Wagyu::new();
    w.add_ring(&square_f64(0.25, 0.75, true), PolygonType::Subject);
    w.add_ring(&square_f64(0.5, 1.0, true), PolygonType::Clip);
    let mp = w
        .execute(
            Operation::Intersection,
            FillType::EvenOdd,
            FillType::EvenOdd,
        )
        .unwrap();
    assert_eq!(
        area_f64(&mp),
        0.0,
        "wagyu-rs 0.2.1 snap-rounds f64 input; if this now yields 0.0625 the \
         degrees-as-is space is worth adding back to the runner: {mp:?}"
    );
}
