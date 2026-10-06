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

/// Largest coordinate magnitude handed to `i_overlay`'s float adapter. `i_float` 5
/// documents 2^500 (about 3.3e150) as the f64 limit of
/// `FloatPointAdapter::new` and panics beyond it; the margin keeps the
/// extent's centre/radius arithmetic finite too.
const IOVERLAY_MAX_ABS_COORD: f64 = 1e150;

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
    let representable = |v: f64| v.is_finite() && v.abs() < IOVERLAY_MAX_ABS_COORD;
    if mls.0.is_empty()
        || !mls
            .0
            .iter()
            .flat_map(|ls| ls.0.iter())
            .all(|c| representable(c.x) && representable(c.y))
    {
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
    }
}
