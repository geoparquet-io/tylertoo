//! i_overlay-based polygon and line clipping for robust tile boundary clipping.
//!
//! This is the ONE boolean-ops engine tylertoo clips with (#435): the polygon
//! fallback, the line clipper and the #383 polygon repair all run on the
//! crate's direct `i_overlay` dependency. `geo` 0.33 vendors its own, older
//! `i_overlay` for `BooleanOps`; nothing here or in `clip.rs` calls it.
//!
//! This module provides clipping functions using `i_overlay`'s boolean operations
//! for robust polygon clipping. Unlike wagyu which operates in integer coordinates,
//! `i_overlay` works directly with f64 coordinates, eliminating coordinate conversion
//! overhead.
//!
//! # Design
//!
//! The workflow is:
//! 1. Convert `geo::Polygon<f64>` to `i_overlay`'s shape format (Vec<Vec<[f64; 2]>>)
//! 2. Create a clip box from `TileBounds`
//! 3. Perform Intersect operation with `FillRule::EvenOdd`
//! 4. Convert the result back to `geo::Geometry<f64>`
//!
//! # Why `i_overlay`?
//!
//! `i_overlay`'s boolean operations correctly handle:
//! - Self-intersecting polygons (resolved via fill rule)
//! - U-shaped polygons that split into multiple parts
//! - Polygons with holes that intersect the exterior ring
//! - Complex nested holes
//!
//! The `FillRule::EvenOdd` interprets overlapping regions correctly, producing
//! valid, non-self-intersecting output from invalid input.
//!
//! # Performance
//!
//! `i_overlay` uses a sweep-line algorithm with O(n log n) complexity, similar to
//! wagyu's Vatti algorithm. However, by operating in f64 directly, we avoid
//! the overhead of coordinate conversion that wagyu requires.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::tile::TileBounds;
use geo::{BoundingRect, Coord, Geometry, LineString, MultiLineString, MultiPolygon, Polygon};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::clip::FloatClip;
use i_overlay::float::overlay::FloatOverlay;
use i_overlay::string::clip::ClipRule;

// ============================================================================
// Type Aliases
// ============================================================================

/// A point in `i_overlay` format
type IOverlayPoint = [f64; 2];

/// A contour (ring) in `i_overlay` format
type IOverlayContour = Vec<IOverlayPoint>;

/// A shape in `i_overlay` format (first contour is exterior, rest are holes)
type IOverlayShape = Vec<IOverlayContour>;

/// Multiple shapes
type IOverlayShapes = Vec<IOverlayShape>;

// ============================================================================
// Conversion: geo -> i_overlay
// ============================================================================

/// Convert a `geo::Polygon` to `i_overlay` shape format.
///
/// `i_overlay` expects shapes as Vec<Vec<[f64; 2]>> where:
/// - First contour is the exterior ring
/// - Subsequent contours are holes
///
/// Note: `i_overlay` handles both closed (first=last) and open rings.
#[inline]
fn polygon_to_ioverlay(poly: &Polygon<f64>) -> IOverlayShape {
    let mut shape = Vec::with_capacity(1 + poly.interiors().len());

    // Exterior ring
    let exterior: IOverlayContour = poly.exterior().coords().map(|c| [c.x, c.y]).collect();
    shape.push(exterior);

    // Holes
    for hole in poly.interiors() {
        let hole_contour: IOverlayContour = hole.coords().map(|c| [c.x, c.y]).collect();
        shape.push(hole_contour);
    }

    shape
}

/// Create a clip box from `TileBounds` in `i_overlay` format.
///
/// Returns a single shape (rectangle) that can be used as the clip subject.
#[inline]
fn bounds_to_clip_box(bounds: &TileBounds) -> IOverlayShape {
    vec![vec![
        [bounds.lng_min, bounds.lat_min],
        [bounds.lng_max, bounds.lat_min],
        [bounds.lng_max, bounds.lat_max],
        [bounds.lng_min, bounds.lat_max],
        [bounds.lng_min, bounds.lat_min], // Close the ring
    ]]
}

// ============================================================================
// Conversion: i_overlay -> geo
// ============================================================================

/// Convert `i_overlay` shapes to `geo::Geometry`.
///
/// Returns:
/// - None if no shapes (empty result)
/// - `Geometry::Polygon` if single shape
/// - `Geometry::MultiPolygon` if multiple shapes
fn ioverlay_to_geometry(shapes: IOverlayShapes) -> Option<Geometry<f64>> {
    // Filter out empty shapes
    let valid_shapes: Vec<_> = shapes
        .into_iter()
        .filter(|shape| !shape.is_empty() && !shape[0].is_empty())
        .collect();

    if valid_shapes.is_empty() {
        return None;
    }

    let polygons: Vec<Polygon<f64>> = valid_shapes
        .into_iter()
        .filter_map(ioverlay_shape_to_polygon)
        .collect();

    match polygons.len() {
        0 => None,
        1 => Some(Geometry::Polygon(polygons.into_iter().next().unwrap())),
        _ => Some(Geometry::MultiPolygon(MultiPolygon::new(polygons))),
    }
}

/// Convert a single `i_overlay` shape to `geo::Polygon`.
///
/// `i_overlay` returns open contours (no repeated closing point), so we need
/// to ensure the LineString is properly closed for geo.
fn ioverlay_shape_to_polygon(shape: IOverlayShape) -> Option<Polygon<f64>> {
    if shape.is_empty() {
        return None;
    }

    // Convert exterior ring
    let exterior = contour_to_linestring(&shape[0])?;

    // Convert holes
    let holes: Vec<LineString<f64>> = shape[1..]
        .iter()
        .filter_map(contour_to_linestring)
        .collect();

    Some(Polygon::new(exterior, holes))
}

/// Convert an `i_overlay` contour to `geo::LineString`.
///
/// Ensures the ring is closed (first point == last point) as required by geo.
fn contour_to_linestring(contour: &IOverlayContour) -> Option<LineString<f64>> {
    if contour.len() < 3 {
        return None;
    }

    let mut coords: Vec<Coord<f64>> = contour.iter().map(|p| Coord { x: p[0], y: p[1] }).collect();

    // Ensure closed ring (i_overlay returns open contours)
    if coords.first() != coords.last() {
        if let Some(first) = coords.first().cloned() {
            coords.push(first);
        }
    }

    // Need at least 4 points for a valid closed ring (triangle + closing point)
    if coords.len() < 4 {
        return None;
    }

    Some(LineString::new(coords))
}

// ============================================================================
// Public Clipping API
// ============================================================================

/// Clip a polygon to tile bounds using `i_overlay`'s boolean intersection.
///
/// This function:
/// 1. Converts the polygon to `i_overlay` format
/// 2. Creates a clip box from the bounds
/// 3. Performs an Intersect operation with `FillRule::EvenOdd`
/// 4. Converts the result back to `geo::Geometry`
///
/// The `EvenOdd` fill rule correctly handles self-intersecting polygons by
/// interpreting overlapping regions as "outside", effectively resolving
/// self-intersections in the output.
///
/// # Arguments
///
/// * `poly` - The polygon to clip
/// * `bounds` - The tile bounds to clip to
///
/// # Returns
///
/// - `Some(Geometry::Polygon)` if result is a single polygon
/// - `Some(Geometry::MultiPolygon)` if result splits into multiple polygons
/// - `None` if the polygon doesn't intersect the bounds
///
/// # Example
///
/// ```ignore
/// use tylertoo_core::ioverlay_clip::clip_polygon_ioverlay;
/// use tylertoo_core::tile::TileBounds;
/// use geo::Polygon;
///
/// let poly = create_polygon();
/// let bounds = TileBounds::new(-180.0, -90.0, 180.0, 90.0);
/// let result = clip_polygon_ioverlay(&poly, &bounds);
/// ```
pub fn clip_polygon_ioverlay(poly: &Polygon<f64>, bounds: &TileBounds) -> Option<Geometry<f64>> {
    // Input guard (#692, mirroring the #435 line-path guard): i_float 5's
    // adapter panics ("Invalid adapter bounds") on a non-finite extent or a
    // coordinate magnitude past 2^500. A hostile vertex means there is
    // nothing we can clip — return the same `None` as "doesn't intersect".
    if !polygon_representable(poly) {
        count_guard_drop();
        return None;
    }

    // Convert polygon to i_overlay format
    let subj = polygon_to_ioverlay(poly);

    // Create clip box
    let clip = bounds_to_clip_box(bounds);

    // Perform intersection using EvenOdd fill rule
    // EvenOdd correctly handles self-intersecting polygons
    // Default options with the EvenOdd fill rule give valid (OGC-style) output;
    // `OverlayOptions::ogc()` is not needed for a rectangle clip.
    let mut overlay = FloatOverlay::with_subj_and_clip_custom(
        &[subj],
        &[clip],
        Default::default(),
        Default::default(),
    );
    let result: IOverlayShapes = overlay.overlay(OverlayRule::Intersect, FillRule::EvenOdd);

    ioverlay_to_geometry(result)
}

/// Clip a MultiPolygon to tile bounds using `i_overlay`.
///
/// Each polygon in the MultiPolygon is added to the overlay as a separate
/// subject shape, then clipped against the bounds.
///
/// # Arguments
///
/// * `multi` - The MultiPolygon to clip
/// * `bounds` - The tile bounds to clip to
///
/// # Returns
///
/// - `Some(Geometry::Polygon)` if result is a single polygon
/// - `Some(Geometry::MultiPolygon)` if result has multiple polygons
/// - `None` if no polygons intersect the bounds
pub fn clip_multipolygon_ioverlay(
    multi: &MultiPolygon<f64>,
    bounds: &TileBounds,
) -> Option<Geometry<f64>> {
    // Input guard (#692): one hostile vertex anywhere poisons the whole
    // input — empty result, no partial output, as the #435 line guard does.
    if !multi.0.iter().all(polygon_representable) {
        count_guard_drop();
        return None;
    }

    // Convert all polygons to i_overlay format
    let subj_shapes: Vec<IOverlayShape> = multi.0.iter().map(polygon_to_ioverlay).collect();

    if subj_shapes.is_empty() {
        return None;
    }

    // Create clip box
    let clip = bounds_to_clip_box(bounds);

    // Perform intersection using default options
    // Default options with the EvenOdd fill rule give valid (OGC-style) output;
    // `OverlayOptions::ogc()` is not needed for a rectangle clip.
    let mut overlay = FloatOverlay::with_subj_and_clip_custom(
        &subj_shapes,
        &[clip],
        Default::default(),
        Default::default(),
    );
    let result: IOverlayShapes = overlay.overlay(OverlayRule::Intersect, FillRule::EvenOdd);

    ioverlay_to_geometry(result)
}

// F2: becomes Diagnostic::ClipGuardDropped
/// Process-wide count of geometries the clip input guard refused (#707): a
/// NaN, infinite or over-[`IOVERLAY_MAX_ABS_COORD`] vertex that `i_float`'s
/// adapter would panic on, or that `clip.rs`'s fast path would have emitted
/// unclipped.
///
/// What the refusal means depends on the entry point: the clippers
/// ([`clip_polygon_ioverlay`], [`clip_multipolygon_ioverlay`],
/// [`clip_multilinestring_ioverlay`], and `clip.rs`'s polygon paths) drop the
/// feature from that tile; [`union_polygons_ioverlay`] and
/// [`repair_polygon_ioverlay`] hand their input back unmerged or unrepaired.
/// Either way the input never reached the engine, and before #707 nothing
/// said so: the feature vanished from every tile at every zoom with exit 0.
/// The first refusal in a process warns; [`clip_guard_drop_count`] has the
/// total, following simplify's `FULL_RES_FALLBACKS` precedent.
static CLIP_GUARD_DROPS: AtomicU64 = AtomicU64::new(0);

/// Snapshot of the process-wide clip-guard refusal counter (#707).
pub fn clip_guard_drop_count() -> u64 {
    CLIP_GUARD_DROPS.load(Ordering::Relaxed)
}

/// Count one clip-guard refusal, warning on the first in the process.
pub(crate) fn count_guard_drop() {
    if CLIP_GUARD_DROPS.fetch_add(1, Ordering::Relaxed) == 0 {
        log::warn!(
            "a geometry with a NaN, infinite or absurdly large coordinate reached the \
             clipper and was left out of the tile (#707); later ones are counted, not \
             logged. The input holds coordinates no valid GeoParquet or overview file does."
        );
    }
}

/// Largest coordinate magnitude handed to `i_overlay`'s float adapter. `i_float` 5
/// documents 2^500 (about 3.3e150) as the f64 limit of
/// `FloatPointAdapter::new` and panics beyond it; the margin keeps the
/// extent's centre/radius arithmetic finite too.
///
/// The margin must specifically leave a factor of two: `repair_polygon_ioverlay`
/// and `union_polygons_ioverlay` pad the bounding rect by half its longest
/// extent, so a shape spanning the full +/- this limit produces clip-box corners
/// at +/- twice it -- and those corners are themselves adapter input points,
/// validated against the same 2^500. Raising this constant past 2^499 would
/// reintroduce the panic by way of the padding alone; the test
/// `ioverlay_max_abs_coord_is_under_the_adapter_limit` pins that headroom.
const IOVERLAY_MAX_ABS_COORD: f64 = 1e150;

/// Can `v` be handed to `i_float` 5's adapter without tripping its
/// "Invalid adapter bounds" panic (finite and below
/// [`IOVERLAY_MAX_ABS_COORD`])? The #435 line-path guard, shared with the
/// polygon entry points since #692.
#[inline]
fn representable(v: f64) -> bool {
    v.is_finite() && v.abs() < IOVERLAY_MAX_ABS_COORD
}

/// Every coordinate of `poly` — exterior and holes — is
/// [`representable`] (#692).
fn polygon_representable(poly: &Polygon<f64>) -> bool {
    std::iter::once(poly.exterior())
        .chain(poly.interiors())
        .flat_map(|ring| ring.0.iter())
        .all(|c| representable(c.x) && representable(c.y))
}

/// Clip a MultiLineString to tile bounds using i_overlay's string clipping
/// (#435).
///
/// This is the line counterpart of [`clip_polygon_ioverlay`], and the drop-in
/// replacement for `geo::BooleanOps::clip(&mls, false)`: the same operation
/// (`FillRule::EvenOdd`, boundary included, not inverted, open clip contour)
/// on our direct `i_overlay` instead of the 4.x copy `geo` 0.33 vendors, so
/// lines and polygons are clipped by ONE engine. Each input line is one
/// subject path; the bounds rectangle is the clip contour.
///
/// Output parts are whatever the engine emits: pieces of the input lines
/// that lie inside or on the bounds, in the engine's own order, with
/// zero-length pieces and consecutive duplicate vertices dropped. A part
/// can be split at an interior vertex where the engine nodes it (see the
/// pinned cases in `tests/line_clip_pinned.rs`). An empty result means
/// nothing of the input lies within the bounds.
// `pub` only for the hostile-geometry harness (`tests/hostile_geometry_eval.rs`),
// which scores the raw line clipper as its own engine column (#205); not
// part of the supported API, hence hidden from the docs.
#[doc(hidden)]
pub fn clip_multilinestring_ioverlay(
    mls: &MultiLineString<f64>,
    bounds: &TileBounds,
) -> MultiLineString<f64> {
    // i_float 5's adapter panics ("Invalid adapter bounds") when the
    // combined extent is non-finite or a coordinate magnitude exceeds 2^500
    // (its documented f64 limit). The geo path returned nothing for such
    // input; keep that contract. `bounds` are lon/lat, so only the subject
    // can trip it.
    if mls.0.is_empty() {
        return MultiLineString::new(Vec::new());
    }
    if !mls
        .0
        .iter()
        .flat_map(|ls| ls.0.iter())
        .all(|c| representable(c.x) && representable(c.y))
    {
        count_guard_drop();
        return MultiLineString::new(Vec::new());
    }

    let subject: Vec<IOverlayContour> = mls
        .0
        .iter()
        .map(|ls| ls.0.iter().map(|c| [c.x, c.y]).collect())
        .collect();

    // Open contour: i_overlay closes it implicitly, as geo's
    // `ring_to_shape_path` does when it strips a ring's closing vertex.
    let clip: IOverlayContour = vec![
        [bounds.lng_min, bounds.lat_min],
        [bounds.lng_max, bounds.lat_min],
        [bounds.lng_max, bounds.lat_max],
        [bounds.lng_min, bounds.lat_max],
    ];

    let clip_rule = ClipRule {
        invert: false,
        boundary_included: true,
    };
    let paths = subject.clip_by(&clip, FillRule::EvenOdd, clip_rule);

    MultiLineString::new(
        paths
            .into_iter()
            .map(|path| LineString::new(path.into_iter().map(|[x, y]| Coord { x, y }).collect()))
            .collect(),
    )
}

/// Union several polygons into non-overlapping polygons.
///
/// The exteriors must all wind the same way and the holes the other. The
/// union intersects the set with a strict-superset box under
/// `FillRule::NonZero` (#383).
///
/// `NonZero` is the rule that makes overlapping parts *add*: `EvenOdd` would
/// cut the overlap out as a hole. It relies on consistent winding, which is
/// why the caller orients the parts first.
pub fn union_polygons_ioverlay(polys: &[Polygon<f64>]) -> Option<Geometry<f64>> {
    // Input guard (#692): a non-finite or over-limit vertex would panic the
    // adapter (and an infinity poisons the padded-box arithmetic below).
    // `None` is the "could not trace" result the caller (mvt.rs merge)
    // already handles by emitting the parts as they are.
    if !polys.iter().all(polygon_representable) {
        count_guard_drop();
        return None;
    }

    let mut rect: Option<geo::Rect<f64>> = None;
    for p in polys {
        let r = p.bounding_rect()?;
        rect = Some(match rect {
            None => r,
            Some(acc) => geo::Rect::new(
                geo::coord! { x: acc.min().x.min(r.min().x), y: acc.min().y.min(r.min().y) },
                geo::coord! { x: acc.max().x.max(r.max().x), y: acc.max().y.max(r.max().y) },
            ),
        });
    }
    let rect = rect?;
    let pad = rect.width().max(rect.height()).max(f64::MIN_POSITIVE) * 0.5;
    let bounds = TileBounds::new(
        rect.min().x - pad,
        rect.min().y - pad,
        rect.max().x + pad,
        rect.max().y + pad,
    );
    let subj_shapes: Vec<IOverlayShape> = polys.iter().map(polygon_to_ioverlay).collect();
    let clip = bounds_to_clip_box(&bounds);
    let mut overlay = FloatOverlay::with_subj_and_clip_custom(
        &subj_shapes,
        &[clip],
        Default::default(),
        Default::default(),
    );
    let result: IOverlayShapes = overlay.overlay(OverlayRule::Intersect, FillRule::NonZero);
    ioverlay_to_geometry(result)
}

/// Repair a self-intersecting polygon by re-tracing it through a boolean
/// intersection with its own (padded) bounding box.
///
/// RDP simplification can fold a ring across itself (bowtie / spike
/// crossings). Intersecting with a strict-superset box under
/// `FillRule::EvenOdd` re-resolves the crossings into one or more valid
/// simple polygons — the same interpretation `clip_polygon_ioverlay` applies
/// to self-intersecting input (see `test_clip_self_intersecting_bowtie`).
///
/// Returns `None` when nothing remains (degenerate rings or a fully
/// self-canceling shape).
pub fn repair_polygon_ioverlay(poly: &Polygon<f64>) -> Option<Geometry<f64>> {
    // Input guard (#692): a hostile vertex cannot be repaired, only panic
    // the adapter (directly, or through the padded box turning non-finite).
    // `None` is the non-panicking result both call sites already handle:
    // mvt.rs `clean_into` emits the input unrepaired, and
    // overview/simplify's `repair_candidate` falls back to collapse /
    // full-resolution — the pass-through-unrepaired semantics of #435.
    if !polygon_representable(poly) {
        count_guard_drop();
        return None;
    }

    let rect = poly.bounding_rect()?;
    // Pad so no vertex lies exactly on the clip-box edge; any positive
    // fraction of the extent works, the box just has to strictly contain
    // the shape.
    let pad = rect.width().max(rect.height()).max(f64::MIN_POSITIVE) * 0.5;
    let bounds = TileBounds::new(
        rect.min().x - pad,
        rect.min().y - pad,
        rect.max().x + pad,
        rect.max().y + pad,
    );
    clip_polygon_ioverlay(poly, &bounds)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Coord;

    /// #707: the guard's edge, just under and just over
    /// [`IOVERLAY_MAX_ABS_COORD`]: under reaches the engine (which must not
    /// panic on it, whatever its precision makes of a unit tile), over is
    /// refused and counted.
    #[test]
    fn the_guard_edge_sits_at_the_max_abs_coord() {
        let bounds = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
        let with_far_vertex = |v: f64| {
            let mut p = make_square(0.0, 0.0, 0.5, 0.5);
            p.exterior_mut(|ring| ring.0[2] = Coord { x: v, y: v });
            p
        };
        let under = with_far_vertex(IOVERLAY_MAX_ABS_COORD * 0.99);
        assert!(polygon_representable(&under));
        let _ = clip_polygon_ioverlay(&under, &bounds);
        let _ = repair_polygon_ioverlay(&under);
        assert!(!polygon_representable(&with_far_vertex(
            IOVERLAY_MAX_ABS_COORD * 1.01
        )));
        let before = clip_guard_drop_count();
        assert!(
            clip_polygon_ioverlay(&with_far_vertex(IOVERLAY_MAX_ABS_COORD * 1.01), &bounds)
                .is_none()
        );
        assert!(clip_guard_drop_count() > before);
    }

    /// #707: every guarded entry point counts its refusal instead of
    /// returning a silent `None`.
    #[test]
    fn every_guard_refusal_is_counted() {
        let mut hostile = make_square(1.0, 1.0, 2.0, 2.0);
        hostile.exterior_mut(|ring| ring.0[2].x = f64::INFINITY);
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let counted = |f: &dyn Fn() -> bool| {
            let before = clip_guard_drop_count();
            assert!(f(), "the guard refused");
            assert!(clip_guard_drop_count() > before, "and counted it");
        };
        counted(&|| clip_polygon_ioverlay(&hostile, &bounds).is_none());
        counted(&|| {
            clip_multipolygon_ioverlay(&MultiPolygon::new(vec![hostile.clone()]), &bounds).is_none()
        });
        counted(&|| union_polygons_ioverlay(std::slice::from_ref(&hostile)).is_none());
        counted(&|| repair_polygon_ioverlay(&hostile).is_none());
        counted(&|| {
            let line =
                MultiLineString::new(vec![LineString::from(vec![(1.0, 1.0), (f64::NAN, 2.0)])]);
            clip_multilinestring_ioverlay(&line, &bounds).0.is_empty()
        });
    }

    fn make_square(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> Polygon<f64> {
        Polygon::new(
            LineString::new(vec![
                Coord { x: min_x, y: min_y },
                Coord { x: max_x, y: min_y },
                Coord { x: max_x, y: max_y },
                Coord { x: min_x, y: max_y },
                Coord { x: min_x, y: min_y },
            ]),
            vec![],
        )
    }

    #[test]
    fn test_clip_fully_inside() {
        let poly = make_square(1.0, 1.0, 2.0, 2.0);
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let result = clip_polygon_ioverlay(&poly, &bounds);

        assert!(result.is_some());
        match result.unwrap() {
            Geometry::Polygon(p) => {
                assert!(p.exterior().0.len() >= 4);
            }
            _ => panic!("Expected Polygon"),
        }
    }

    #[test]
    fn test_clip_fully_outside() {
        let poly = make_square(100.0, 100.0, 200.0, 200.0);
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let result = clip_polygon_ioverlay(&poly, &bounds);

        assert!(result.is_none());
    }

    #[test]
    fn test_clip_partial() {
        let poly = make_square(-5.0, -5.0, 5.0, 5.0);
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let result = clip_polygon_ioverlay(&poly, &bounds);

        assert!(result.is_some());
        match result.unwrap() {
            Geometry::Polygon(p) => {
                // Should be clipped to the corner
                assert!(p.exterior().0.len() >= 4);
            }
            _ => panic!("Expected Polygon"),
        }
    }

    #[test]
    fn test_clip_self_intersecting_bowtie() {
        // Create a self-intersecting bowtie polygon
        let bowtie = Polygon::new(
            LineString::new(vec![
                Coord { x: -1.0, y: -1.0 },
                Coord { x: 1.0, y: 1.0 },
                Coord { x: -1.0, y: 1.0 },
                Coord { x: 1.0, y: -1.0 },
                Coord { x: -1.0, y: -1.0 },
            ]),
            vec![],
        );

        let bounds = TileBounds::new(-2.0, -2.0, 2.0, 2.0);

        let result = clip_polygon_ioverlay(&bowtie, &bounds);

        // Should produce valid output (MultiPolygon with 2 triangles)
        assert!(result.is_some());
        match result.unwrap() {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2, "Bowtie should split into 2 triangles");
            }
            Geometry::Polygon(_) => {
                // Also acceptable if it merges them somehow
            }
            other => panic!("Expected Polygon or MultiPolygon, got {:?}", other),
        }
    }

    #[test]
    fn test_repair_self_intersecting_bowtie() {
        use geo::Validation;
        let bowtie = Polygon::new(
            LineString::new(vec![
                Coord { x: -1.0, y: -1.0 },
                Coord { x: 1.0, y: 1.0 },
                Coord { x: -1.0, y: 1.0 },
                Coord { x: 1.0, y: -1.0 },
                Coord { x: -1.0, y: -1.0 },
            ]),
            vec![],
        );
        assert!(!bowtie.is_valid(), "fixture must self-intersect");

        let repaired = repair_polygon_ioverlay(&bowtie).expect("bowtie repairs to non-empty");
        match repaired {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2, "bowtie should split into 2 triangles");
                for p in &mp.0 {
                    assert!(p.is_valid(), "repaired part must be valid");
                }
            }
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    #[test]
    fn test_repair_valid_polygon_stays_equivalent() {
        use geo::{Area, Validation};
        let square = make_square(0.0, 0.0, 10.0, 10.0);
        let repaired = repair_polygon_ioverlay(&square).expect("valid input survives repair");
        match repaired {
            Geometry::Polygon(p) => {
                assert!(p.is_valid());
                assert!((p.unsigned_area() - 100.0).abs() < 1e-9);
            }
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn test_clip_u_shape_splits() {
        // Create a U-shaped polygon
        let u_shape = Polygon::new(
            LineString::new(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: 0.0, y: 2.0 },
                Coord { x: 0.3, y: 2.0 },
                Coord { x: 0.3, y: 0.5 },
                Coord { x: 0.7, y: 0.5 },
                Coord { x: 0.7, y: 2.0 },
                Coord { x: 1.0, y: 2.0 },
                Coord { x: 1.0, y: 0.0 },
                Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        );

        // Clip box that cuts through the U opening
        let bounds = TileBounds::new(-0.1, 1.0, 1.1, 2.5);

        let result = clip_polygon_ioverlay(&u_shape, &bounds);

        assert!(result.is_some());
        match result.unwrap() {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(
                    mp.0.len(),
                    2,
                    "U-shape clipped across opening should produce 2 polygons"
                );
            }
            other => panic!("Expected MultiPolygon with 2 polygons, got {:?}", other),
        }
    }

    #[test]
    fn test_clip_multipolygon() {
        let poly1 = make_square(1.0, 1.0, 2.0, 2.0);
        let poly2 = make_square(5.0, 5.0, 6.0, 6.0);
        let multi = MultiPolygon::new(vec![poly1, poly2]);

        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let result = clip_multipolygon_ioverlay(&multi, &bounds);

        assert!(result.is_some());
        match result.unwrap() {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2, "Both polygons should be preserved");
            }
            _ => panic!("Expected MultiPolygon"),
        }
    }

    // ========== Line clipping (#435) ==========

    fn line_parts(m: &MultiLineString<f64>) -> Vec<Vec<(f64, f64)>> {
        m.0.iter()
            .map(|l| l.0.iter().map(|c| (c.x, c.y)).collect())
            .collect()
    }

    fn lines(parts: &[&[(f64, f64)]]) -> MultiLineString<f64> {
        MultiLineString::new(parts.iter().map(|p| LineString::from(p.to_vec())).collect())
    }

    #[test]
    fn test_clip_lines_crossing_is_trimmed() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let out = clip_multilinestring_ioverlay(&lines(&[&[(-5.0, 5.0), (15.0, 5.0)]]), &bounds);
        assert_eq!(line_parts(&out), vec![vec![(0.0, 5.0), (10.0, 5.0)]]);
    }

    #[test]
    fn test_clip_lines_outside_is_empty() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let out = clip_multilinestring_ioverlay(&lines(&[&[(20.0, 20.0), (30.0, 30.0)]]), &bounds);
        assert!(out.0.is_empty(), "{out:?}");
    }

    #[test]
    fn test_clip_lines_collinear_with_edge_is_kept() {
        // boundary_included: a run along the bottom edge survives.
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let out = clip_multilinestring_ioverlay(&lines(&[&[(-5.0, 0.0), (15.0, 0.0)]]), &bounds);
        assert_eq!(line_parts(&out), vec![vec![(0.0, 0.0), (10.0, 0.0)]]);
    }

    #[test]
    fn test_clip_lines_corner_graze_is_dropped() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let out = clip_multilinestring_ioverlay(&lines(&[&[(-5.0, 5.0), (5.0, -5.0)]]), &bounds);
        assert!(out.0.is_empty(), "{out:?}");
    }

    #[test]
    fn test_clip_lines_re_entering_matches_pinned_baseline() {
        // Same case as `line_clip_pinned::line_exiting_and_re_entering_is_split_at_the_bounds`:
        // the geo/i_overlay-4 baseline broke the re-entering run at (7,5).
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let out = clip_multilinestring_ioverlay(
            &lines(&[&[
                (-5.0, 5.0),
                (3.0, 5.0),
                (3.0, 15.0),
                (7.0, 15.0),
                (7.0, 5.0),
                (15.0, 5.0),
            ]]),
            &bounds,
        );
        assert_eq!(
            line_parts(&out),
            vec![
                vec![(0.0, 5.0), (3.0, 5.0), (3.0, 10.0)],
                vec![(7.0, 10.0), (7.0, 5.0)],
                vec![(7.0, 5.0), (10.0, 5.0)],
            ]
        );
    }

    #[test]
    fn test_clip_lines_zero_length_is_dropped() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let out = clip_multilinestring_ioverlay(&lines(&[&[(5.0, 5.0), (5.0, 5.0)]]), &bounds);
        assert!(out.0.is_empty(), "{out:?}");
    }

    #[test]
    fn test_clip_lines_empty_input_is_empty() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let out = clip_multilinestring_ioverlay(&MultiLineString::new(vec![]), &bounds);
        assert!(out.0.is_empty());
    }

    #[test]
    fn test_clip_lines_non_finite_coordinate_yields_empty() {
        // i_float 5's adapter panics ("Invalid adapter bounds") on a
        // non-finite extent; the geo path returned nothing. Keep returning
        // nothing.
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        for bad in [f64::INFINITY, f64::NEG_INFINITY, f64::NAN] {
            let out = clip_multilinestring_ioverlay(
                &lines(&[&[(5.0, 5.0), (bad, 5.0)], &[(2.0, 2.0), (8.0, 8.0)]]),
                &bounds,
            );
            assert!(out.0.is_empty(), "{bad}: {out:?}");
        }
    }

    #[test]
    fn test_clip_lines_astronomical_coordinate_yields_empty() {
        // Finite but beyond the adapter's 2^500 limit: same panic, same
        // guard.
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        for bad in [1e300, -1e300, f64::MAX] {
            let out = clip_multilinestring_ioverlay(&lines(&[&[(5.0, 5.0), (5.0, bad)]]), &bounds);
            assert!(out.0.is_empty(), "{bad}: {out:?}");
        }
    }

    #[test]
    fn test_clip_lines_far_but_sane_coordinate_still_clips() {
        // Well inside the guard: the engine must still run, not be
        // short-circuited.
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let out = clip_multilinestring_ioverlay(&lines(&[&[(-1e6, 5.0), (1e6, 5.0)]]), &bounds);
        assert_eq!(line_parts(&out), vec![vec![(0.0, 5.0), (10.0, 5.0)]]);
    }

    #[test]
    fn ioverlay_max_abs_coord_is_under_the_adapter_limit() {
        assert!(IOVERLAY_MAX_ABS_COORD < 2f64.powi(500));
        assert!(IOVERLAY_MAX_ABS_COORD > 2f64.powi(498));
        // `repair_polygon_ioverlay` / `union_polygons_ioverlay` pad the
        // bounding rect by half its longest extent, so a shape spanning
        // +/-IOVERLAY_MAX_ABS_COORD puts the clip box's corners at twice
        // that -- and the adapter validates those corners too. Without this
        // bound a larger-but-still-under-2^500 threshold would pass every
        // other test here and panic again on a padded repair.
        assert!(2.0 * IOVERLAY_MAX_ABS_COORD <= 2f64.powi(500));
    }

    // ========== Polygon-path input guard (#692) ==========
    //
    // Same hostile inputs as the line-path guard above (#435): a NaN/inf/huge
    // vertex must yield the empty/None result the callers already handle, not
    // an i_float adapter panic ("Invalid adapter bounds").

    /// A square with one vertex replaced by `bad` in x.
    fn poisoned_square(bad: f64) -> Polygon<f64> {
        Polygon::new(
            LineString::new(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: 4.0, y: 0.0 },
                Coord { x: bad, y: 4.0 },
                Coord { x: 0.0, y: 4.0 },
                Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        )
    }

    const HOSTILE_COORDS: [f64; 6] = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        1e300,
        -1e300,
        f64::MAX,
    ];

    #[test]
    fn test_clip_polygon_hostile_coordinate_yields_none() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        for bad in HOSTILE_COORDS {
            let result = clip_polygon_ioverlay(&poisoned_square(bad), &bounds);
            assert!(result.is_none(), "{bad}: {result:?}");
        }
    }

    #[test]
    fn test_clip_polygon_hostile_hole_coordinate_yields_none() {
        // The guard must look at interior rings too, not just the exterior.
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        for bad in [f64::NAN, f64::INFINITY, 1e300] {
            let poly = Polygon::new(
                make_square(0.0, 0.0, 4.0, 4.0).exterior().clone(),
                vec![LineString::new(vec![
                    Coord { x: 1.0, y: 1.0 },
                    Coord { x: 2.0, y: 1.0 },
                    Coord { x: bad, y: 2.0 },
                    Coord { x: 1.0, y: 1.0 },
                ])],
            );
            let result = clip_polygon_ioverlay(&poly, &bounds);
            assert!(result.is_none(), "{bad}: {result:?}");
        }
    }

    #[test]
    fn test_clip_polygon_far_but_sane_coordinate_still_clips() {
        // Well inside the guard: the engine must still run, not be
        // short-circuited.
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let result = clip_polygon_ioverlay(&make_square(-1e6, -1e6, 1e6, 1e6), &bounds);
        match result {
            Some(Geometry::Polygon(p)) => assert!(p.exterior().0.len() >= 4),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn test_clip_multipolygon_hostile_coordinate_yields_none() {
        // One bad part poisons the whole input, mirroring the line guard
        // (#435): empty result, no partial output.
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        for bad in HOSTILE_COORDS {
            let multi =
                MultiPolygon::new(vec![make_square(1.0, 1.0, 2.0, 2.0), poisoned_square(bad)]);
            let result = clip_multipolygon_ioverlay(&multi, &bounds);
            assert!(result.is_none(), "{bad}: {result:?}");
        }
    }

    #[test]
    fn test_clip_multipolygon_far_but_sane_coordinate_still_clips() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let multi = MultiPolygon::new(vec![
            make_square(1.0, 1.0, 2.0, 2.0),
            make_square(-1e6, -1e6, 1e6, 1e6),
        ]);
        let result = clip_multipolygon_ioverlay(&multi, &bounds);
        assert!(result.is_some(), "{result:?}");
    }

    #[test]
    fn test_union_polygons_hostile_coordinate_yields_none() {
        for bad in HOSTILE_COORDS {
            let polys = [make_square(0.0, 0.0, 2.0, 2.0), poisoned_square(bad)];
            let result = union_polygons_ioverlay(&polys);
            assert!(result.is_none(), "{bad}: {result:?}");
        }
    }

    #[test]
    fn test_union_polygons_valid_input_still_unions() {
        let polys = [
            make_square(0.0, 0.0, 2.0, 2.0),
            make_square(1.0, 1.0, 3.0, 3.0),
        ];
        match union_polygons_ioverlay(&polys) {
            Some(Geometry::Polygon(p)) => assert!(p.exterior().0.len() >= 4),
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn test_repair_polygon_hostile_coordinate_yields_none() {
        for bad in HOSTILE_COORDS {
            let result = repair_polygon_ioverlay(&poisoned_square(bad));
            assert!(result.is_none(), "{bad}: {result:?}");
        }
    }
}
