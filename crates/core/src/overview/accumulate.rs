//! Tiny-polygon accumulation for coarse levels (#384).
//!
//! At a coarse level most small polygons fail the visibility gate or lose
//! their thinning cell and vanish. Each one is invisible on its own, but
//! together they are the map: a country of 25 m fields is 90% farmland at
//! z0, and a level that shows none of it is wrong, not merely sparse.
//! tippecanoe answers this with its tiny-polygon reduction — every dropped
//! polygon's area goes into a running total, and each time the total crosses
//! one placeholder's worth a placeholder square is emitted where the
//! polygon that crossed it sits. Dense regions read as dense, and the
//! placeholder area tracks the dropped area: a polygon contributes at most
//! one placeholder of area, and a patch keeps less than one `T` unemitted.
//!
//! This is the same accumulator, run once per level after assignment, over
//! every polygon that is *not* a member of that level, bucketed into
//! [`ACCUMULATE_CELL_GSD`]-wide patches — 1/32 of a level's 1024-pixel
//! tile, so a patch holds ~1,000 placeholders' worth of area and a region
//! that is 40% fields yields ~400 squares per patch rather than none (an
//! accumulation scope the size of one placeholder would need 100% coverage
//! to emit anything). The polygon that pushes a patch's total across the threshold
//! becomes that level's **carrier**: it is added to the level and rendered
//! as a threshold-area square at its representative point, carrying its own
//! attributes — exactly as tippecanoe's placeholder does. The threshold is
//! the level's simplification tolerance squared, the same `T` the
//! per-feature `--collapse-square` dither uses, so the two agree on what a
//! placeholder is worth; the difference is that the dither only ever saw
//! polygons that survived assignment, while this sees all of them.
//!
//! Deterministic: input order and integer cell keys, no randomness — and
//! computed once on the pass-1 feature table, so every engine reads the
//! same carrier set (the reason the per-feature dither exists is engine
//! independence; this keeps it).
//!
//! Divergences from tippecanoe's `reduce_tiny_poly` (clip.cpp): (a) it only
//! accumulates rings with area ≤ `tiny_polygon_size²` and keeps larger
//! rings as geometry, whereas we clamp each polygon's contribution to one
//! placeholder instead of skipping large ones — they were already dropped
//! by the gate or thinning here, so there is no geometry to keep; (b) it
//! places the placeholder at the ring's first vertex with side
//! `tiny_polygon_size` (default 2 px), whereas ours sits at the polygon's
//! representative point with side `factor × gsd`. Features placed by an
//! entry-zoom ladder (#364) are never accumulated: the ladder decides where
//! they first appear.

use std::collections::HashMap;

use super::assign::{gsd_to_coord_units, AssignFeature, FeatureKind};
use super::level::Crs;
use super::simplify::{CollapseMode, Representation};

/// Accumulation patch width in GSD multiples: 1/32 of a 1024-pixel tile.
pub const ACCUMULATE_CELL_GSD: f64 = 32.0;

/// Per-level parameters the accumulator needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AccumulateLevel {
    /// Ground sample distance in meters.
    pub gsd_meters: f64,
    /// Whether this level accumulates at all ([`level_accumulates`]). The
    /// canonical level never does — every feature is already present there.
    pub enabled: bool,
}

/// Whether a level's effective disposition is the placeholder square, i.e.
/// whether the accumulator runs there: a `square` representation band, or
/// the global `--collapse-square` at a plain-geometry level. A `point` band
/// is points only — its polygons thin on the point grid, and their losers
/// must not come back as squares. Shared by every engine so the carrier
/// sets cannot drift.
pub fn level_accumulates(collapse: CollapseMode, repr: Representation) -> bool {
    match repr {
        Representation::Square => true,
        Representation::Geometry => collapse == CollapseMode::Square,
        Representation::Point => false,
    }
}

/// Run the accumulator. Returns, per level, the **sorted** row indices
/// (`AssignFeature::index`) of that level's carriers: polygons that are not
/// members of the level (`min_level > level`) but must be emitted there as a
/// placeholder square.
///
/// `features`, `min_levels` and `areas` are parallel; `areas` holds each
/// feature's unsigned area in CRS units² (anything for non-polygons).
/// `simplify_factor` is the simplify knob the placeholder threshold
/// (`(factor × gsd)²`) derives from.
pub fn tiny_polygon_carriers(
    features: &[AssignFeature],
    min_levels: &[u8],
    areas: &[f32],
    levels: &[AccumulateLevel],
    crs: Crs,
    simplify_factor: f64,
) -> Vec<Vec<usize>> {
    debug_assert_eq!(features.len(), min_levels.len());
    debug_assert_eq!(features.len(), areas.len());
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); levels.len()];
    for (li, level) in levels.iter().enumerate() {
        if !level.enabled {
            continue;
        }
        let gsd_units = gsd_to_coord_units(level.gsd_meters, crs);
        let tol = simplify_factor * gsd_units;
        let threshold = tol * tol;
        let cell = ACCUMULATE_CELL_GSD * gsd_units;
        // NaN / zero guards (a NaN compares false everywhere).
        if threshold.is_nan() || threshold <= 0.0 || cell.is_nan() || cell <= 0.0 {
            continue;
        }
        let mut acc: HashMap<(i64, i64), f64> = HashMap::new();
        for ((f, &ml), &area) in features.iter().zip(min_levels).zip(areas) {
            if f.kind != FeatureKind::Polygon || usize::from(ml) <= li {
                continue; // not a polygon, or already present at this level
            }
            if f.entry_level.is_some() {
                continue; // #364: the ladder decides where it first appears
            }
            let area = f64::from(area);
            if area.is_nan() || area <= 0.0 {
                continue;
            }
            // DIVERGENCE FROM TIPPECANOE: tippecanoe only accumulates rings
            // with area <= pixel² and keeps larger rings as geometry, so its
            // residual never exceeds one placeholder. Here a non-member is
            // gone whatever its size (gate-failed polygons routinely sit in
            // (T, 2T); thinning and budget losers are unbounded), so clamp
            // the contribution instead: a polygon is worth at most one
            // placeholder, and the residual invariant (< T per patch) holds.
            let area = area.min(threshold);
            let (cx, cy) = f.center();
            let key = ((cx / cell).floor() as i64, (cy / cell).floor() as i64);
            let total = acc.entry(key).or_insert(0.0);
            *total += area;
            if *total >= threshold {
                out[li].push(f.index);
                *total -= threshold;
            }
        }
        // `features` is in input order and `index` is monotone in it, but
        // sort anyway: the lookup is a binary search.
        out[li].sort_unstable();
    }
    out
}

/// Whether row `g` is a carrier at a level, given that level's sorted list.
#[inline]
pub fn is_carrier(carriers: &[usize], g: usize) -> bool {
    carriers.binary_search(&g).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(index: usize, x: f64, y: f64, side: f64) -> AssignFeature {
        AssignFeature {
            index,
            bbox: [x, y, x + side, y + side],
            kind: FeatureKind::Polygon,
            sort_key: None,
            entry_level: None,
        }
    }

    /// Level 0 at 1000 m (EPSG:3857 so units are meters), tolerance 1×gsd
    /// ⇒ threshold 1e6 m²; accumulation patch = 32×gsd = 32 km.
    fn level() -> Vec<AccumulateLevel> {
        vec![
            AccumulateLevel {
                gsd_meters: 1000.0,
                enabled: true,
            },
            AccumulateLevel {
                gsd_meters: 10.0,
                enabled: false,
            },
        ]
    }

    #[test]
    fn one_carrier_per_threshold_of_dropped_area() {
        // Ten 400×400 m fields (160,000 m² each) in one patch, none
        // present at level 0: 1.6e6 m² ⇒ one square (the 7th field crosses
        // 1e6), with 0.6e6 carried over.
        let feats: Vec<AssignFeature> = (0..10)
            .map(|i| square(i, 10.0 + i as f64 * 5.0, 10.0, 400.0))
            .collect();
        let min_levels = vec![1u8; 10];
        let areas = vec![160_000.0f32; 10];
        let out = tiny_polygon_carriers(&feats, &min_levels, &areas, &level(), Crs::Epsg3857, 1.0);
        assert_eq!(out[0], vec![6]);
        assert!(out[1].is_empty(), "disabled level accumulates nothing");
    }

    #[test]
    fn present_polygons_and_other_kinds_do_not_accumulate() {
        let mut feats: Vec<AssignFeature> = (0..10).map(|i| square(i, 10.0, 10.0, 400.0)).collect();
        feats[3].kind = FeatureKind::Point;
        let mut min_levels = vec![1u8; 10];
        min_levels[0] = 0; // already a member of level 0
        let areas = vec![160_000.0f32; 10];
        let out = tiny_polygon_carriers(&feats, &min_levels, &areas, &level(), Crs::Epsg3857, 1.0);
        // 8 polygons accumulate (10 minus the member minus the point):
        // 1.28e6 ⇒ one carrier, and it is the 7th accumulated (index 8).
        assert_eq!(out[0], vec![8]);
    }

    #[test]
    fn cells_accumulate_independently() {
        // Two clusters 100 km apart (different patches), each just under
        // one threshold: no carrier from either alone; together they would
        // have made one.
        let mut feats = Vec::new();
        for i in 0..5 {
            feats.push(square(i, 10.0, 10.0, 400.0));
            feats.push(square(5 + i, 100_000.0, 10.0, 400.0));
        }
        let min_levels = vec![1u8; 10];
        let areas = vec![160_000.0f32; 10]; // 5 × 160k = 800k < 1e6 per cell
        let out = tiny_polygon_carriers(&feats, &min_levels, &areas, &level(), Crs::Epsg3857, 1.0);
        assert!(out[0].is_empty(), "{:?}", out[0]);
        // Bigger fields: 5 × 300k = 1.5e6 per cell ⇒ one carrier per cell.
        let areas = vec![300_000.0f32; 10];
        let out = tiny_polygon_carriers(&feats, &min_levels, &areas, &level(), Crs::Epsg3857, 1.0);
        assert_eq!(out[0].len(), 2);
        assert_eq!(out[0], vec![3, 8], "the 4th field of each cell crosses");
    }

    /// A polygon bigger than one placeholder contributes at most one
    /// placeholder's worth (tippecanoe's accounting: it never accumulates
    /// a ring above `pixel²`). Ten fields of 1.4 T must leave NO residual
    /// — the 0.5 T field after them cannot cross on its own.
    #[test]
    fn a_polygon_larger_than_the_threshold_contributes_one_placeholder() {
        let mut feats: Vec<AssignFeature> = (0..10)
            .map(|i| square(i, 10.0 + i as f64 * 5.0, 10.0, 1183.0))
            .collect();
        feats.push(square(10, 100.0, 10.0, 707.0));
        let min_levels = vec![1u8; 11];
        let mut areas = vec![1_400_000.0f32; 10];
        areas.push(500_000.0);
        let out = tiny_polygon_carriers(&feats, &min_levels, &areas, &level(), Crs::Epsg3857, 1.0);
        assert_eq!(
            out[0],
            (0..10).collect::<Vec<_>>(),
            "one carrier per over-threshold field and none for the 0.5 T tail"
        );
    }

    /// Mixed over-threshold (T..8T) and tiny (0.1 T) fields in one patch:
    /// the emitted area (carriers × T) equals Σ min(area, T) to within one
    /// T, i.e. the residual stays bounded however large the big ones are.
    #[test]
    fn over_threshold_areas_are_clamped_before_accumulating() {
        let t = 1_000_000.0f64;
        let big = [1.5, 3.0, 7.9, 1.01, 5.0, 2.0, 6.5, 1.2, 4.4, 7.0];
        let mut feats = Vec::new();
        let mut areas = Vec::new();
        for (i, &b) in big.iter().enumerate() {
            feats.push(square(2 * i, 10.0 + i as f64 * 5.0, 10.0, 100.0));
            areas.push((b * t) as f32);
            feats.push(square(2 * i + 1, 20.0 + i as f64 * 5.0, 10.0, 100.0));
            areas.push((0.1 * t) as f32);
        }
        let min_levels = vec![1u8; feats.len()];
        let out = tiny_polygon_carriers(&feats, &min_levels, &areas, &level(), Crs::Epsg3857, 1.0);
        let expected: f64 = areas.iter().map(|&a| f64::from(a).min(t)).sum();
        let emitted = out[0].len() as f64 * t;
        assert!(
            emitted <= expected + 1.0 && emitted > expected - t,
            "{} carriers emit {emitted:e} for {expected:e} of clamped area",
            out[0].len()
        );
        // Every big field is its own carrier; the tinies together make
        // exactly one more (10 × 0.1 T).
        assert_eq!(out[0].len(), big.len() + 1, "{:?}", out[0]);
    }

    /// #364: a feature the entry-zoom ladder placed at level 3 is held out
    /// of levels 0..3 on purpose; its area must not turn into carrier
    /// squares there, however large it is.
    #[test]
    fn laddered_features_are_never_accumulated() {
        let mut feats: Vec<AssignFeature> = (0..3).map(|i| square(i, 10.0, 10.0, 5000.0)).collect();
        for f in &mut feats {
            f.entry_level = Some(3);
        }
        let min_levels = vec![3u8; 3];
        let areas = vec![25_000_000.0f32; 3]; // 25 T each
        let out = tiny_polygon_carriers(&feats, &min_levels, &areas, &level(), Crs::Epsg3857, 1.0);
        assert!(
            out[0].is_empty(),
            "laddered features became carriers: {:?}",
            out[0]
        );
    }

    /// The per-level switch both engines share: a `point` band never
    /// accumulates, whatever the global disposition.
    #[test]
    fn point_bands_never_accumulate() {
        use CollapseMode as C;
        use Representation as R;
        assert!(level_accumulates(C::Square, R::Geometry));
        assert!(level_accumulates(C::Square, R::Square));
        assert!(level_accumulates(C::Drop, R::Square));
        assert!(!level_accumulates(C::Square, R::Point));
        assert!(!level_accumulates(C::Drop, R::Geometry));
        assert!(!level_accumulates(C::Point, R::Geometry));
        assert!(!level_accumulates(C::Point, R::Point));
    }

    #[test]
    fn area_is_conserved_over_many_cells() {
        // 1,000 fields of 90,000 m² (300 m) spread over a 128 km strip:
        // total 9e7 m² ⇒ 90 thresholds; the carriers must number 90 ± one
        // per patch of carry-over (each patch keeps < 1 threshold unemitted).
        let feats: Vec<AssignFeature> = (0..1000)
            .map(|i| square(i, (i as f64) * 128.0, 0.0, 300.0))
            .collect();
        let min_levels = vec![1u8; 1000];
        let areas = vec![90_000.0f32; 1000];
        let out = tiny_polygon_carriers(&feats, &min_levels, &areas, &level(), Crs::Epsg3857, 1.0);
        let cells = 4; // 128 km / 32 km patches
        let n = out[0].len();
        assert!((90 - cells..=90).contains(&n), "{n} carriers");
        assert!(out[0].windows(2).all(|w| w[0] < w[1]), "sorted");
    }
}
