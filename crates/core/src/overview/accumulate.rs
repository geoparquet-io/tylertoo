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

use super::assign::{gsd_to_coord_units, FeatureKind, FeatureTable};
use super::level::Crs;
use super::simplify::{
    dither_u01, export_zoom, level_tolerance, placeholder_side, CollapseMode, Representation,
};

/// Accumulation patch width in GSD multiples: 1/32 of a 1024-pixel tile.
pub const ACCUMULATE_CELL_GSD: f64 = 32.0;

/// Per-level parameters the accumulator needs.
///
/// `#[non_exhaustive]` since #407 added [`zoom`](Self::zoom): build one with
/// [`AccumulateLevel::new`].
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct AccumulateLevel {
    /// Ground sample distance in meters.
    pub gsd_meters: f64,
    /// The level's recorded Web Mercator zoom, `None` for an explicit `--gsd`
    /// plan. Fixes the zoom the level is exported at, and so the tile unit
    /// its placeholder side is floored at (#407).
    pub zoom: Option<u8>,
    /// Whether this level accumulates at all ([`level_accumulates`]). The
    /// canonical level never does — every feature is already present there.
    pub enabled: bool,
}

impl AccumulateLevel {
    /// A level of `gsd_meters` at the recorded `zoom` (if any), accumulating
    /// when `enabled`.
    pub fn new(gsd_meters: f64, zoom: Option<u8>, enabled: bool) -> Self {
        Self {
            gsd_meters,
            zoom,
            enabled,
        }
    }
}

/// Whether the accumulator has a placeholder to emit at this simplify
/// factor. `--simplify-factor 0` gives the square no size, so the run skips
/// the accumulator (and pass 1's area collection) and says so once — the
/// same as tippecanoe, which skips its tiny-polygon reduction at
/// `tiny_polygon_size` 0. Call it once per conversion, where the decision
/// is made, not per level.
pub(crate) fn placeholder_has_size(simplify_factor: f64) -> bool {
    if simplify_factor > 0.0 {
        return true;
    }
    log::info!(
        "[convert] tiny-polygon accumulator off: --simplify-factor {simplify_factor} gives \
         placeholder squares no size, so dropped polygons are not stood in for"
    );
    false
}

/// Whether a level's effective disposition is the placeholder square.
///
/// That is, whether the accumulator runs there: a `square` representation band, or
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

/// Run the accumulator.
///
/// Returns, per level, the **sorted** row indices
/// (`AssignFeature::index`) of that level's carriers: polygons that are not
/// members of the level (`min_level > level`) but must be emitted there as a
/// placeholder square.
///
/// `features`, `min_levels` and `areas` are parallel; `areas` holds each
/// feature's unsigned area in CRS units² (anything for non-polygons).
/// `simplify_factor` is the simplify knob the placeholder side derives from.
///
/// The threshold is `side²`, `side` being the level's `placeholder_side`:
/// `factor × gsd`, floored at one tile unit at the level's export zoom
/// (#407). On a floored level each patch's residual is dithered into one
/// more carrier with probability `residual / T` (see below). The floor
/// bounds the carrier count by the dropped area over one unit², where a
/// small factor used to make every dropped polygon a carrier, and makes
/// every carrier square wide enough for the tile encoder to draw. One
/// `log::info` names the zooms where the floor applies. A zero factor has
/// no placeholder: nothing accumulates (`placeholder_has_size`).
///
/// DIVERGENCE FROM TIPPECANOE: tippecanoe's placeholder side is a fixed
/// `tiny_polygon_size` in tile units (clip.cpp `reduce_tiny_poly`), whatever
/// the simplification. Ours follows `factor × gsd` down to that one-unit
/// floor, so the accumulator and the `--collapse-square` dither share one
/// square.
pub fn tiny_polygon_carriers(
    features: &FeatureTable,
    min_levels: &[u8],
    areas: &[f32],
    levels: &[AccumulateLevel],
    crs: Crs,
    simplify_factor: f64,
) -> Vec<Vec<usize>> {
    debug_assert_eq!(features.len(), min_levels.len());
    debug_assert_eq!(features.len(), areas.len());
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); levels.len()];
    let mut floored_zooms: Vec<f64> = Vec::new();
    for (li, level) in levels.iter().enumerate() {
        if !level.enabled {
            continue;
        }
        let gsd_units = gsd_to_coord_units(level.gsd_meters, crs);
        let side = placeholder_side(level.gsd_meters, level.zoom, crs, simplify_factor);
        let floored = side > level_tolerance(level.gsd_meters, crs, simplify_factor);
        if floored {
            floored_zooms.push(export_zoom(level.gsd_meters, level.zoom));
        }
        let threshold = side * side;
        let cell = ACCUMULATE_CELL_GSD * gsd_units;
        // NaN / zero guards (a NaN compares false everywhere).
        if threshold.is_nan() || threshold <= 0.0 || cell.is_nan() || cell <= 0.0 {
            continue;
        }
        let mut acc: HashMap<(i64, i64), Patch> = HashMap::new();
        for (pos, (&ml, &area)) in min_levels.iter().zip(areas).enumerate() {
            if features.kind(pos) != FeatureKind::Polygon || usize::from(ml) <= li {
                continue; // not a polygon, or already present at this level
            }
            if features.entry_level(pos).is_some() {
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
            // The patch key comes from the bbox centre. A polygon whose bbox
            // spans the antimeridian (lng_min near -180, lng_max near 180)
            // therefore lands in a patch near lng 0, far from where it
            // sits. The rest of the pipeline has no wrap handling either
            // (#342); this stays consistent with it until that lands.
            let (cx, cy) = features.center(pos);
            let key = ((cx / cell).floor() as i64, (cy / cell).floor() as i64);
            let patch = acc.entry(key).or_default();
            patch.total += area;
            if patch.total >= threshold {
                out[li].push(features.indices()[pos]);
                patch.total -= threshold;
            } else {
                patch.last_free = Some((features.indices()[pos], cx, cy));
            }
        }
        // A patch keeps less than one `T` unemitted. At a floored level `T`
        // is up to several times what the factor asked for, so that
        // truncation would thin sparse patches noticeably (at `--gsd-base
        // 8192`, ~12% of the z3 area in the #407 probe). There, dither the
        // residual instead: the patch's last non-carrier contributor also
        // becomes a carrier with probability `residual / T`, decided by the
        // same anchor hash the `--collapse-square` dither uses, so expected
        // placeholder area equals the clamped dropped area. Unfloored levels
        // keep the plain truncation, byte-identical to before #407.
        if floored {
            for patch in acc.values() {
                if let Some((index, cx, cy)) = patch.last_free {
                    if dither_u01(cx, cy) < patch.total / threshold {
                        out[li].push(index);
                    }
                }
            }
        }
        // `features` is in input order and `index` is monotone in it, but
        // sort anyway: the lookup is a binary search.
        out[li].sort_unstable();
    }
    if !floored_zooms.is_empty() {
        let zooms: Vec<String> = floored_zooms.iter().map(|z| format!("z{z}")).collect();
        log::info!(
            "[convert] tiny-polygon accumulator: at {} a --simplify-factor {simplify_factor} \
             placeholder is narrower than one tile unit, so its side is raised to one unit \
             (fewer, visible squares)",
            zooms.join(", ")
        );
    }
    out
}

/// One accumulation patch's running state.
#[derive(Default)]
struct Patch {
    /// Clamped dropped area not yet emitted (`< T` between features).
    total: f64,
    /// The latest contributor that did not become a carrier, with its bbox
    /// centre: the residual dither's candidate on a floored level.
    last_free: Option<(usize, f64, f64)>,
}

/// Whether row `g` is a carrier at a level, given that level's sorted list.
#[inline]
pub fn is_carrier(carriers: &[usize], g: usize) -> bool {
    carriers.binary_search(&g).is_ok()
}

#[cfg(test)]
mod tests {
    use super::super::assign::AssignFeature;

    /// Array-of-structs fixture → the column-major table the engine takes
    /// (#543). Tests build small `Vec<AssignFeature>`/array fixtures; the
    /// pipeline fills a [`FeatureTable`] directly from the scan, so this
    /// conversion exists only here.
    fn table(feats: &[AssignFeature]) -> FeatureTable {
        feats.iter().collect()
    }
    use super::*;
    use crate::mvt::MIN_SURVIVING_SQUARE_SIDE;

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
            AccumulateLevel::new(1000.0, None, true),
            AccumulateLevel::new(10.0, None, false),
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
        let out = tiny_polygon_carriers(
            &table(&feats),
            &min_levels,
            &areas,
            &level(),
            Crs::Epsg3857,
            1.0,
        );
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
        let out = tiny_polygon_carriers(
            &table(&feats),
            &min_levels,
            &areas,
            &level(),
            Crs::Epsg3857,
            1.0,
        );
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
        let out = tiny_polygon_carriers(
            &table(&feats),
            &min_levels,
            &areas,
            &level(),
            Crs::Epsg3857,
            1.0,
        );
        assert!(out[0].is_empty(), "{:?}", out[0]);
        // Bigger fields: 5 × 300k = 1.5e6 per cell ⇒ one carrier per cell.
        let areas = vec![300_000.0f32; 10];
        let out = tiny_polygon_carriers(
            &table(&feats),
            &min_levels,
            &areas,
            &level(),
            Crs::Epsg3857,
            1.0,
        );
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
        let out = tiny_polygon_carriers(
            &table(&feats),
            &min_levels,
            &areas,
            &level(),
            Crs::Epsg3857,
            1.0,
        );
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
        let out = tiny_polygon_carriers(
            &table(&feats),
            &min_levels,
            &areas,
            &level(),
            Crs::Epsg3857,
            1.0,
        );
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
        let out = tiny_polygon_carriers(
            &table(&feats),
            &min_levels,
            &areas,
            &level(),
            Crs::Epsg3857,
            1.0,
        );
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

    // ---- #407: the placeholder side is floored at one tile unit ---------

    /// Carriers at level 0 of [`level`] for the ten-field fixture of
    /// [`one_carrier_per_threshold_of_dropped_area`] under `factor`.
    fn ten_fields_at(factor: f64) -> Vec<usize> {
        let feats: Vec<AssignFeature> = (0..10)
            .map(|i| square(i, 10.0 + i as f64 * 5.0, 10.0, 400.0))
            .collect();
        let min_levels = vec![1u8; 10];
        let areas = vec![160_000.0f32; 10];
        tiny_polygon_carriers(
            &table(&feats),
            &min_levels,
            &areas,
            &level(),
            Crs::Epsg3857,
            factor,
        )
        .swap_remove(0)
    }

    /// The factor at which level 0's `factor × gsd` is exactly one tile unit.
    fn one_unit_factor() -> f64 {
        use super::super::simplify::tile_unit_meters;
        let lvl = level()[0];
        MIN_SURVIVING_SQUARE_SIDE * tile_unit_meters(lvl.gsd_meters, lvl.zoom) / lvl.gsd_meters
    }

    /// `--simplify-factor 0`: the placeholder has no side at all, so the
    /// level must not accumulate (every field would cross `T = 0`).
    #[test]
    fn zero_factor_accumulates_nothing() {
        assert!(ten_fields_at(0.0).is_empty());
        assert!(!placeholder_has_size(0.0));
        assert!(placeholder_has_size(1e-9));
    }

    /// A factor so small that `factor × gsd` is a millimetre: the side is
    /// floored at one tile unit, so the level carries exactly what it carries
    /// at the one-unit factor — not one carrier per dropped polygon (the
    /// #407 blow-up) and not nothing.
    #[test]
    fn sub_unit_factor_is_floored_at_one_tile_unit() {
        let at_unit = ten_fields_at(one_unit_factor());
        assert!(!at_unit.is_empty());
        assert_eq!(ten_fields_at(1e-6), at_unit, "1e-6 is floored to one unit");
        assert_eq!(ten_fields_at(one_unit_factor() * 0.5), at_unit);
        // Carriers are bounded by the dropped area over one unit²:
        // each 160,000 m² field is clamped to T = unit² (305.7 m)², so all
        // ten cross — and no more than ten can.
        assert!(at_unit.len() <= 10);
        // Above the floor the factor is used as is.
        assert_ne!(ten_fields_at(one_unit_factor() * 2.0), at_unit);
    }

    /// On a floored level the per-patch residual is dithered, not
    /// truncated: 400 patches each holding half a placeholder of dropped
    /// area yield about 200 carriers (expected area preserved), where an
    /// unfloored level keeps the plain truncation and yields none.
    #[test]
    fn floored_levels_dither_the_patch_residual() {
        use super::super::simplify::tile_unit_meters;
        let unit = tile_unit_meters(1000.0, None);
        let n = 400;
        let feats: Vec<AssignFeature> = (0..n)
            .map(|i| square(i, i as f64 * 40_000.0 + 7.3 * i as f64, 10.0, 100.0))
            .collect();
        let min_levels = vec![1u8; n];
        let run = |factor: f64, area: f64| {
            let areas = vec![area as f32; n];
            tiny_polygon_carriers(
                &table(&feats),
                &min_levels,
                &areas,
                &level(),
                Crs::Epsg3857,
                factor,
            )
            .swap_remove(0)
        };
        let floored = run(1e-6, 0.5 * unit * unit);
        let sd = (n as f64 * 0.25).sqrt();
        assert!(
            (floored.len() as f64 - n as f64 / 2.0).abs() < 4.0 * sd,
            "{} residual carriers of {n} half-full patches",
            floored.len()
        );
        // Unfloored (factor 1: T = 1e6 m²), half a T per patch stays unemitted.
        assert!(run(1.0, 500_000.0).is_empty());
    }

    /// One side serves the accumulator's threshold and [`carrier_square`]:
    /// `placeholder_side` is the tolerance when that is at least one tile
    /// unit (equal, within 4 ulp, to both pre-#407 formulas), the unit
    /// otherwise; the carrier square is exactly that wide.
    #[test]
    fn accumulator_and_carrier_square_share_one_side() {
        use super::super::simplify::{
            carrier_square, level_tolerance, placeholder_side, tile_unit_meters, SimplifyOptions,
        };
        use geo::{BoundingRect, Geometry};
        for crs in [Crs::Epsg3857, Crs::Epsg4326] {
            for gsd_m in [0.3, 9.55, 1000.0, 39_135.76] {
                for factor in [1e-6, 0.1, 0.25, 1.0, 3.7] {
                    let tol = level_tolerance(gsd_m, crs, factor);
                    let old_acc = factor * gsd_to_coord_units(gsd_m, crs);
                    let old_sq = crs.meters_to_units(factor * gsd_m);
                    let ulps = 4.0 * f64::EPSILON * tol;
                    assert!((tol - old_acc).abs() <= ulps, "{crs:?} {gsd_m} {factor}");
                    assert!((tol - old_sq).abs() <= ulps, "{crs:?} {gsd_m} {factor}");
                    let unit = crs.meters_to_units(tile_unit_meters(gsd_m, None));
                    let side = placeholder_side(gsd_m, None, crs, factor);
                    assert_eq!(side, tol.max(unit), "{crs:?} {gsd_m} {factor}");
                    let poly = geo::Rect::new((0.0, 0.0), (side * 0.1, side * 0.1)).to_polygon();
                    let opts = SimplifyOptions {
                        factor,
                        ..SimplifyOptions::default()
                    };
                    let sq = carrier_square(&Geometry::Polygon(poly), gsd_m, None, crs, &opts)
                        .and_then(|g| g.bounding_rect())
                        .expect("a polygon has a carrier square");
                    assert!(
                        (sq.width() - side).abs() <= 1e3 * f64::EPSILON * side,
                        "{crs:?} {gsd_m} {factor}"
                    );
                }
            }
        }
        assert_eq!(placeholder_side(1000.0, None, Crs::Epsg3857, 0.0), 0.0);
        assert_eq!(placeholder_side(0.0, None, Crs::Epsg3857, 1.0), 0.0);
    }

    /// The recorded zoom, not the GSD, fixes the floor: at `--gsd-base 8192`
    /// a z7 level has an eighth of the default GSD, and one unit is two of
    /// them, so the default factor 1.0 is floored there.
    #[test]
    fn the_floor_follows_the_recorded_zoom() {
        use super::super::level::gsd_with_base;
        use super::super::simplify::placeholder_side;
        let g = gsd_with_base(7, 8192.0);
        let side = placeholder_side(g, Some(7), Crs::Epsg3857, 1.0);
        assert!((side - 2.0 * g).abs() < 1e-9, "{side} vs 2 × {g}");
        // Without the zoom the GSD alone implies z10, where 1 × gsd is 4 units.
        assert_eq!(placeholder_side(g, None, Crs::Epsg3857, 1.0), g);
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
        let out = tiny_polygon_carriers(
            &table(&feats),
            &min_levels,
            &areas,
            &level(),
            Crs::Epsg3857,
            1.0,
        );
        let cells = 4; // 128 km / 32 km patches
        let n = out[0].len();
        assert!((90 - cells..=90).contains(&n), "{n} carriers");
        assert!(out[0].windows(2).all(|w| w[0] < w[1]), "sorted");
    }
}
