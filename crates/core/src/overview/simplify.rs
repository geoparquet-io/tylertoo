//! World-space, GSD-driven geometry simplification for overview levels.
//!
//! # Why this module exists (vs. `crate::simplify`)
//!
//! The crate-level [`crate::simplify`] module simplifies in **tile-local
//! pixel space**: every entry point transforms geometry into a
//! `TileCoord` + `extent` (0–4096) pixel frame, runs Ramer–Douglas–Peucker
//! (RDP), and transforms back. That is correct for MVT tile generation but
//! meaningless for overview *levels*, which have no tile seams, no pixel
//! extent, and no per-tile context.
//!
//! Overview simplification runs RDP **directly on the source-CRS geometry**
//! with a world-space tolerance derived from the level's GSD (ground sample
//! distance, meters). This module therefore *extracts and adapts* the
//! algorithms from `crate::simplify` (the RDP call itself — `geo`'s
//! [`geo::Simplify`] — plus the ring-validity/degenerate guards) but couples
//! to none of its tile-space entry points.
//!
//! # Tolerance model
//!
//! Simplification tolerance is a **world-space distance** derived from the
//! level GSD:
//!
//! ```text
//! tolerance_world = to_world_units(factor * gsd_meters)
//! ```
//!
//! - `gsd_meters` is the level's GSD in meters (spec §5.2 GSD table; always
//!   meters regardless of file CRS).
//! - `factor` is a multiplier (default [`DEFAULT_SIMPLIFY_FACTOR`] = `1.0`):
//!   one GSD is the smallest ground distance independently meaningful at a
//!   level, so sub-GSD vertex wobble is exactly the detail an overview should
//!   shed. `factor = 1.0` matches that "collapse anything finer than one
//!   ground sample" intent; callers may set it lower to preserve more detail
//!   or higher to thin harder.
//! - CRS conversion (spec Q3, §7.1): for **EPSG:3857** world units are meters
//!   so the tolerance is used verbatim; for **EPSG:4326** coordinates are
//!   degrees, so meters are divided by [`METERS_PER_DEGREE`] (`111_320`, the
//!   equatorial degree length).
//!
//! # Visibility gate
//!
//! Beyond vertex reduction, a line or polygon whose **bounding-box diagonal**
//! is smaller than the world-space tolerance is not independently meaningful
//! at this level and is dropped (spec §3.5 `visibility_gate_m`, "min
//! bbox-diagonal kept"; P1 "bbox-diagonal visibility gates"). The caller
//! receives [`Simplified::Dropped`] and decides how to handle the hole.
//!
//! # Canonical level (identity path)
//!
//! The canonical (finest) level reproduces the source geometry
//! value-for-value (spec §2.4, Q1). Passing `gsd_meters == 0.0` (or any
//! `factor * gsd_meters <= 0`) yields a **bit-identical** clone via
//! [`simplify_for_level`] with no simplification, gating, or dropping — an
//! explicit, tested identity path rather than an emergent one.

use geo::{
    Area, BoundingRect, Centroid, Geometry, LineString, MultiLineString, MultiPolygon, Point,
    Polygon, Rect, Simplify, Validation,
};

pub use super::level::{Crs, METERS_PER_DEGREE};

/// Default simplification factor: `tolerance = factor * gsd`.
///
/// `1.0` means "simplify away detail finer than one ground sample". One GSD is
/// the smallest ground distance that is independently meaningful at a level
/// (spec §1.2), so this is the natural default; see the module docs.
pub const DEFAULT_SIMPLIFY_FACTOR: f64 = 1.0;

/// Minimum number of coordinates for a closed polygon ring to be valid
/// (3 distinct vertices + the closing vertex). Mirrors
/// `crate::validate::MIN_POLYGON_RING_POINTS`; duplicated here to keep this
/// module free of tile-space dependencies.
const MIN_POLYGON_RING_POINTS: usize = 4;

/// Minimum number of coordinates for a non-degenerate line.
const MIN_LINESTRING_POINTS: usize = 2;

/// Options controlling per-level simplification.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimplifyOptions {
    /// Tolerance multiplier: `tolerance = factor * gsd`. See
    /// [`DEFAULT_SIMPLIFY_FACTOR`].
    pub factor: f64,
    /// Opt-in geometry-type collapse (spec Q4, default **off**). When `true`,
    /// a polygon / multipolygon that collapses below the visibility gate is
    /// replaced by a representative [`Point`] instead of being dropped.
    pub collapse: bool,
}

impl Default for SimplifyOptions {
    fn default() -> Self {
        Self {
            factor: DEFAULT_SIMPLIFY_FACTOR,
            collapse: false,
        }
    }
}

/// Result of simplifying one feature's geometry for a level.
///
/// The caller decides what a [`Dropped`](Simplified::Dropped) feature means
/// (skip the row, aggregate its attributes into a neighbor, etc.); this module
/// never makes that policy choice.
#[derive(Debug, Clone, PartialEq)]
pub enum Simplified {
    /// Geometry survives at this level (possibly simplified or collapsed).
    Keep(Geometry<f64>),
    /// Geometry is not meaningful at this level and should be omitted.
    Dropped,
}

/// The world-space RDP tolerance for a level, in the geometry's coordinate
/// units.
///
/// Returns `0.0` for the canonical/identity case (`gsd_meters <= 0` or
/// `factor <= 0`), which callers and [`simplify_for_level`] treat as "no
/// simplification".
pub fn world_tolerance(gsd_meters: f64, crs: Crs, opts: &SimplifyOptions) -> f64 {
    let meters = opts.factor * gsd_meters;
    if meters <= 0.0 {
        return 0.0;
    }
    crs.meters_to_units(meters)
}

/// Simplify one feature's geometry for a level of the given GSD.
///
/// See the module documentation for the full tolerance / gate / identity
/// model. Summary of per-kind rules (spec §2.1, §7.5):
///
/// - **Point / MultiPoint**: passed through untouched.
/// - **LineString**: simplified; dropped if degenerate (`< 2` distinct
///   points) or below the visibility gate.
/// - **Polygon**: rings simplified preserving validity (exterior stays a valid
///   `>= 4`-point ring; interior rings that collapse are dropped). If the
///   polygon collapses: dropped by default, or collapsed to a representative
///   point when `opts.collapse` is set.
/// - **MultiLineString / MultiPolygon**: simplified per part; empty/collapsed
///   parts dropped; the whole feature dropped (or, for polygons with
///   `opts.collapse`, collapsed to a point) if no part survives.
/// - **GeometryCollection / other**: passed through untouched.
///
/// Canonical/identity path: when the derived tolerance is `0` the input is
/// returned as a bit-identical clone.
pub fn simplify_for_level(
    geom: &Geometry<f64>,
    gsd_meters: f64,
    crs: Crs,
    opts: &SimplifyOptions,
) -> Simplified {
    let tol = world_tolerance(gsd_meters, crs, opts);

    // Canonical / identity path (spec §2.4, Q1): a zero tolerance means "this
    // is the canonical level" — return the geometry bit-identical, with no
    // simplification, gating, or dropping.
    if tol <= 0.0 {
        return Simplified::Keep(geom.clone());
    }

    match geom {
        // Points carry no reducible vertices and are never gated (spec §2.1).
        Geometry::Point(_) | Geometry::MultiPoint(_) => Simplified::Keep(geom.clone()),

        Geometry::LineString(ls) => match simplify_linestring_impl(ls, tol) {
            Some(out) => Simplified::Keep(Geometry::LineString(out)),
            None => Simplified::Dropped,
        },

        Geometry::MultiLineString(mls) => {
            let kept: Vec<LineString<f64>> = mls
                .0
                .iter()
                .filter_map(|ls| simplify_linestring_impl(ls, tol))
                .collect();
            if kept.is_empty() {
                Simplified::Dropped
            } else {
                Simplified::Keep(Geometry::MultiLineString(MultiLineString::new(kept)))
            }
        }

        Geometry::Polygon(poly) => simplify_polygon_impl(poly, tol, opts.collapse),

        Geometry::MultiPolygon(mp) => {
            // Simplify each part with collapse disabled: a collapsed *part* is
            // dropped, never turned into a Point (a MultiPolygon cannot hold
            // one). Whole-feature collapse is decided after.
            let kept: Vec<Polygon<f64>> =
                mp.0.iter()
                    .filter_map(|p| match simplify_polygon_impl(p, tol, false) {
                        Simplified::Keep(Geometry::Polygon(poly)) => Some(poly),
                        _ => None,
                    })
                    .collect();
            if !kept.is_empty() {
                Simplified::Keep(Geometry::MultiPolygon(MultiPolygon::new(kept)))
            } else if opts.collapse {
                match mp.centroid() {
                    Some(pt) => Simplified::Keep(Geometry::Point(pt)),
                    None => Simplified::Dropped,
                }
            } else {
                Simplified::Dropped
            }
        }

        // GeometryCollection / Line / Rect / Triangle: out of scope for v0.1;
        // pass through untouched.
        other => Simplified::Keep(other.clone()),
    }
}

// ============================================================================
// Internal helpers (world-space, tile-free).
//
// These adapt the algorithms from `crate::simplify` — the RDP call itself
// (`geo`'s `Simplify`), the ring-closure/degenerate guards, and the multi
// dispatch — but run directly on source-CRS coordinates with a world-space
// tolerance instead of transforming into tile-local pixel space.
// ============================================================================

/// Bounding-box diagonal of a `Rect` in coordinate units.
#[inline]
fn rect_diag(r: Rect<f64>) -> f64 {
    r.width().hypot(r.height())
}

/// Bounding-box diagonal of a LineString (`0.0` if empty / a single point).
#[inline]
fn linestring_diag(ls: &LineString<f64>) -> f64 {
    ls.bounding_rect().map(rect_diag).unwrap_or(0.0)
}

/// Bounding-box diagonal of a Polygon (`0.0` if degenerate).
#[inline]
fn polygon_diag(poly: &Polygon<f64>) -> f64 {
    poly.bounding_rect().map(rect_diag).unwrap_or(0.0)
}

/// Simplify a single LineString, returning `None` when it should be dropped:
/// degenerate (`< 2` points or all coincident) or below the visibility gate.
///
/// Adapted from `crate::simplify`'s degenerate-linestring guard (which returns
/// the input unchanged for `< 2` points to avoid a `geo::Simplify` panic);
/// here the level path instead *drops* sub-visible lines and reports it.
fn simplify_linestring_impl(ls: &LineString<f64>, tol: f64) -> Option<LineString<f64>> {
    if ls.0.len() < MIN_LINESTRING_POINTS {
        return None;
    }
    let diag = linestring_diag(ls);
    // All points coincide (diag == 0 ⇒ < 2 distinct points) or the whole
    // feature is finer than the level tolerance.
    if diag < tol {
        return None;
    }
    let simplified = ls.simplify(tol);
    if simplified.0.len() < MIN_LINESTRING_POINTS || linestring_diag(&simplified) <= 0.0 {
        return None;
    }
    Some(simplified)
}

/// Simplify a Polygon in world space with ring-validity guards.
///
/// - Below the visibility gate ⇒ collapse (drop, or representative point when
///   `collapse` is set).
/// - Rings are simplified via `geo::Simplify` (which keeps each ring at
///   `>= 4` points, matching `MIN_POLYGON_RING_POINTS`); interior rings that
///   fall below the gate are dropped.
/// - If the exterior collapses (too few points or sub-tolerance area) ⇒
///   collapse.
/// - If RDP introduces an invalid (self-intersecting) polygon, fall back to
///   the original geometry (boundary-preserving: never emit an invalid ring).
fn simplify_polygon_impl(poly: &Polygon<f64>, tol: f64, collapse: bool) -> Simplified {
    if polygon_diag(poly) < tol {
        return collapse_polygon(poly, collapse);
    }

    let simplified = poly.simplify(tol);

    // Drop interior rings that collapsed below the gate.
    let interiors: Vec<LineString<f64>> = simplified
        .interiors()
        .iter()
        .filter(|ring| linestring_diag(ring) >= tol)
        .cloned()
        .collect();

    let exterior = simplified.exterior().clone();
    let candidate = Polygon::new(exterior, interiors);

    // Exterior collapse: too few points, or area smaller than a
    // tolerance-sized cell (catches slivers / zero-area rings).
    let min_area = tol * tol;
    if candidate.exterior().0.len() < MIN_POLYGON_RING_POINTS
        || candidate.unsigned_area() < min_area
    {
        return collapse_polygon(poly, collapse);
    }

    if candidate.is_valid() {
        Simplified::Keep(Geometry::Polygon(candidate))
    } else if poly.is_valid() {
        // Boundary-preserving fallback: RDP made it self-intersect; keep the
        // original valid geometry rather than emit an invalid ring.
        Simplified::Keep(Geometry::Polygon(poly.clone()))
    } else {
        collapse_polygon(poly, collapse)
    }
}

/// Resolve a collapsed polygon: drop by default, or (opt-in) a representative
/// [`Point`] at the polygon centroid (spec Q4, default off).
fn collapse_polygon(poly: &Polygon<f64>, collapse: bool) -> Simplified {
    if !collapse {
        return Simplified::Dropped;
    }
    // Prefer the true centroid; fall back to the bbox center, then the first
    // vertex, for degenerate (zero-area) rings whose centroid is undefined.
    let pt = poly
        .centroid()
        .or_else(|| poly.bounding_rect().map(|r| r.center().into()))
        .or_else(|| poly.exterior().0.first().map(|c| Point::new(c.x, c.y)));
    match pt {
        Some(p) => Simplified::Keep(Geometry::Point(p)),
        None => Simplified::Dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Coord, MultiPoint};

    // ---- helpers -----------------------------------------------------------

    /// A long, gently wiggling line in a projected (meter-like) frame.
    fn wiggly_line(n: usize, wobble: f64) -> LineString<f64> {
        LineString::new(
            (0..n)
                .map(|i| Coord {
                    x: i as f64 * 100.0,
                    y: (i as f64 * 0.7).sin() * wobble,
                })
                .collect(),
        )
    }

    fn line_len(g: &Simplified) -> usize {
        match g {
            Simplified::Keep(Geometry::LineString(ls)) => ls.0.len(),
            other => panic!("expected Keep(LineString), got {other:?}"),
        }
    }

    fn square(cx: f64, cy: f64, half: f64) -> Polygon<f64> {
        Polygon::new(
            LineString::new(vec![
                Coord {
                    x: cx - half,
                    y: cy - half,
                },
                Coord {
                    x: cx + half,
                    y: cy - half,
                },
                Coord {
                    x: cx + half,
                    y: cy + half,
                },
                Coord {
                    x: cx - half,
                    y: cy + half,
                },
                Coord {
                    x: cx - half,
                    y: cy - half,
                },
            ]),
            vec![],
        )
    }

    // ---- tolerance / CRS conversion ---------------------------------------

    #[test]
    fn test_tolerance_crs_conversion() {
        let opts = SimplifyOptions::default();
        // 3857: meters verbatim.
        assert_eq!(world_tolerance(1000.0, Crs::Epsg3857, &opts), 1000.0);
        // 4326: meters / 111320.
        let deg = world_tolerance(1000.0, Crs::Epsg4326, &opts);
        assert!((deg - 1000.0 / METERS_PER_DEGREE).abs() < 1e-12);
        // The two differ by the degree factor.
        assert!(deg < 1.0 && deg > 0.0);
    }

    #[test]
    fn test_tolerance_zero_is_canonical() {
        let opts = SimplifyOptions::default();
        assert_eq!(world_tolerance(0.0, Crs::Epsg3857, &opts), 0.0);
        let zero_factor = SimplifyOptions {
            factor: 0.0,
            ..opts
        };
        assert_eq!(world_tolerance(500.0, Crs::Epsg3857, &zero_factor), 0.0);
    }

    // ---- tolerance scaling / monotonicity ---------------------------------

    #[test]
    fn test_coarser_gsd_fewer_vertices_monotone() {
        let line = Geometry::LineString(wiggly_line(200, 50.0));
        let opts = SimplifyOptions::default();
        let gsds = [10.0, 50.0, 100.0, 500.0, 2000.0];
        let counts: Vec<usize> = gsds
            .iter()
            .map(|g| line_len(&simplify_for_level(&line, *g, Crs::Epsg3857, &opts)))
            .collect();
        // Monotonically non-increasing as GSD coarsens.
        for w in counts.windows(2) {
            assert!(
                w[0] >= w[1],
                "vertex count should not increase with coarser GSD: {counts:?}"
            );
        }
        // And it should actually reduce somewhere.
        assert!(
            counts.first() > counts.last(),
            "coarsest GSD should reduce vertices vs finest: {counts:?}"
        );
    }

    #[test]
    fn test_4326_and_3857_scale_comparably() {
        // Same geometric shape expressed at equator: a 3857 line in meters and
        // a 4326 line in the equivalent degrees should simplify to the same
        // vertex count for the same GSD, because tolerance is CRS-converted.
        let n = 120;
        let wobble_m = 40.0;
        let line_m = Geometry::LineString(LineString::new(
            (0..n)
                .map(|i| Coord {
                    x: i as f64 * 200.0,
                    y: (i as f64 * 0.6).sin() * wobble_m,
                })
                .collect(),
        ));
        let line_deg = Geometry::LineString(LineString::new(
            (0..n)
                .map(|i| Coord {
                    x: i as f64 * 200.0 / METERS_PER_DEGREE,
                    y: (i as f64 * 0.6).sin() * wobble_m / METERS_PER_DEGREE,
                })
                .collect(),
        ));
        let opts = SimplifyOptions::default();
        let gsd = 100.0;
        let c_m = line_len(&simplify_for_level(&line_m, gsd, Crs::Epsg3857, &opts));
        let c_deg = line_len(&simplify_for_level(&line_deg, gsd, Crs::Epsg4326, &opts));
        assert_eq!(
            c_m, c_deg,
            "CRS-converted tolerance should simplify identical shapes equally: {c_m} vs {c_deg}"
        );
    }

    // ---- points pass through ----------------------------------------------

    #[test]
    fn test_points_pass_through() {
        let opts = SimplifyOptions::default();
        let p = Geometry::Point(Point::new(3.0, 4.0));
        assert_eq!(
            simplify_for_level(&p, 5000.0, Crs::Epsg3857, &opts),
            Simplified::Keep(p.clone())
        );
        let mp = Geometry::MultiPoint(MultiPoint::new(vec![
            Point::new(1.0, 1.0),
            Point::new(2.0, 2.0),
        ]));
        assert_eq!(
            simplify_for_level(&mp, 5000.0, Crs::Epsg3857, &opts),
            Simplified::Keep(mp.clone())
        );
    }

    // ---- canonical identity (bit-equal) -----------------------------------

    #[test]
    fn test_canonical_identity_bit_equal() {
        let opts = SimplifyOptions::default();
        let line = Geometry::LineString(wiggly_line(50, 30.0));
        // gsd == 0 => canonical => bit-identical clone.
        match simplify_for_level(&line, 0.0, Crs::Epsg3857, &opts) {
            Simplified::Keep(g) => assert_eq!(g, line),
            Simplified::Dropped => panic!("canonical level must not drop"),
        }
        let poly = Geometry::Polygon(square(0.0, 0.0, 1000.0));
        match simplify_for_level(&poly, 0.0, Crs::Epsg3857, &opts) {
            Simplified::Keep(g) => assert_eq!(g, poly),
            Simplified::Dropped => panic!("canonical level must not drop"),
        }
    }

    // ---- line drop below visibility ---------------------------------------

    #[test]
    fn test_line_dropped_below_visibility() {
        let opts = SimplifyOptions::default();
        // A 10m-long line at a 1000m GSD is sub-visible => dropped.
        let tiny = Geometry::LineString(LineString::new(vec![
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 10.0, y: 0.0 },
        ]));
        assert_eq!(
            simplify_for_level(&tiny, 1000.0, Crs::Epsg3857, &opts),
            Simplified::Dropped
        );
        // The same line at a fine 1m GSD survives.
        assert!(matches!(
            simplify_for_level(&tiny, 1.0, Crs::Epsg3857, &opts),
            Simplified::Keep(_)
        ));
    }

    #[test]
    fn test_single_point_line_dropped() {
        let opts = SimplifyOptions::default();
        let degen = Geometry::LineString(LineString::new(vec![Coord { x: 5.0, y: 5.0 }]));
        assert_eq!(
            simplify_for_level(&degen, 1.0, Crs::Epsg3857, &opts),
            Simplified::Dropped
        );
        // Two identical points (< 2 distinct) also degenerate.
        let dup = Geometry::LineString(LineString::new(vec![
            Coord { x: 5.0, y: 5.0 },
            Coord { x: 5.0, y: 5.0 },
        ]));
        assert_eq!(
            simplify_for_level(&dup, 1.0, Crs::Epsg3857, &opts),
            Simplified::Dropped
        );
    }

    #[test]
    fn test_empty_line_dropped() {
        let opts = SimplifyOptions::default();
        let empty = Geometry::LineString(LineString::new(vec![]));
        assert_eq!(
            simplify_for_level(&empty, 1.0, Crs::Epsg3857, &opts),
            Simplified::Dropped
        );
    }

    // ---- polygon ring validity --------------------------------------------

    #[test]
    fn test_polygon_ring_valid_after_simplify() {
        let opts = SimplifyOptions::default();
        // A many-vertex circle-ish polygon, big enough to survive the gate.
        let coords: Vec<Coord<f64>> = (0..=64)
            .map(|i| {
                let a = i as f64 * std::f64::consts::TAU / 64.0;
                Coord {
                    x: a.cos() * 5000.0,
                    y: a.sin() * 5000.0,
                }
            })
            .collect();
        let poly = Geometry::Polygon(Polygon::new(LineString::new(coords), vec![]));
        match simplify_for_level(&poly, 500.0, Crs::Epsg3857, &opts) {
            Simplified::Keep(Geometry::Polygon(p)) => {
                assert!(p.exterior().0.len() >= MIN_POLYGON_RING_POINTS);
                assert_eq!(
                    p.exterior().0.first(),
                    p.exterior().0.last(),
                    "exterior ring must stay closed"
                );
                assert!(p.is_valid(), "simplified polygon must be valid");
                assert!(
                    p.exterior().0.len() < 65,
                    "polygon should actually be simplified"
                );
            }
            other => panic!("expected Keep(Polygon), got {other:?}"),
        }
    }

    #[test]
    fn test_interior_ring_collapse_dropped() {
        let opts = SimplifyOptions::default();
        // Large exterior, tiny hole. At a coarse GSD the hole collapses and
        // must be dropped while the exterior survives.
        let exterior = LineString::new(vec![
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 10000.0, y: 0.0 },
            Coord {
                x: 10000.0,
                y: 10000.0,
            },
            Coord { x: 0.0, y: 10000.0 },
            Coord { x: 0.0, y: 0.0 },
        ]);
        let tiny_hole = LineString::new(vec![
            Coord { x: 100.0, y: 100.0 },
            Coord { x: 105.0, y: 100.0 },
            Coord { x: 105.0, y: 105.0 },
            Coord { x: 100.0, y: 105.0 },
            Coord { x: 100.0, y: 100.0 },
        ]);
        let poly = Geometry::Polygon(Polygon::new(exterior, vec![tiny_hole]));
        match simplify_for_level(&poly, 1000.0, Crs::Epsg3857, &opts) {
            Simplified::Keep(Geometry::Polygon(p)) => {
                assert_eq!(p.interiors().len(), 0, "collapsed hole must be dropped");
                assert!(p.is_valid());
            }
            other => panic!("expected Keep(Polygon) with no interiors, got {other:?}"),
        }
    }

    #[test]
    fn test_polygon_collapse_default_drop_vs_optin_point() {
        // A 20m square at a 5000m GSD collapses.
        let poly = Geometry::Polygon(square(1000.0, 2000.0, 10.0));

        // Default: dropped.
        let drop_opts = SimplifyOptions::default();
        assert_eq!(
            simplify_for_level(&poly, 5000.0, Crs::Epsg3857, &drop_opts),
            Simplified::Dropped
        );

        // Opt-in collapse: representative point near the square's center.
        let collapse_opts = SimplifyOptions {
            collapse: true,
            ..Default::default()
        };
        match simplify_for_level(&poly, 5000.0, Crs::Epsg3857, &collapse_opts) {
            Simplified::Keep(Geometry::Point(pt)) => {
                assert!((pt.x() - 1000.0).abs() < 1.0);
                assert!((pt.y() - 2000.0).abs() < 1.0);
            }
            other => panic!("expected Keep(Point), got {other:?}"),
        }
    }

    #[test]
    fn test_sliver_polygon_collapses() {
        let opts = SimplifyOptions::default();
        // Degenerate zero-width sliver (all points collinear).
        let sliver = Polygon::new(
            LineString::new(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: 1000.0, y: 0.0 },
                Coord { x: 2000.0, y: 0.0 },
                Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        );
        assert_eq!(
            simplify_for_level(&Geometry::Polygon(sliver), 100.0, Crs::Epsg3857, &opts),
            Simplified::Dropped
        );
    }

    // ---- multi-geometry part dropping -------------------------------------

    #[test]
    fn test_multipolygon_part_dropping() {
        let opts = SimplifyOptions::default();
        // One large part survives, one tiny part collapses.
        let big = square(0.0, 0.0, 5000.0);
        let tiny = square(20000.0, 20000.0, 10.0);
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![big, tiny]));
        match simplify_for_level(&mp, 1000.0, Crs::Epsg3857, &opts) {
            Simplified::Keep(Geometry::MultiPolygon(m)) => {
                assert_eq!(m.0.len(), 1, "tiny part should be dropped");
                assert!(m.0[0].is_valid());
            }
            other => panic!("expected Keep(MultiPolygon) with 1 part, got {other:?}"),
        }
    }

    #[test]
    fn test_multipolygon_all_parts_gone_dropped() {
        let opts = SimplifyOptions::default();
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![
            square(0.0, 0.0, 10.0),
            square(100.0, 100.0, 8.0),
        ]));
        assert_eq!(
            simplify_for_level(&mp, 5000.0, Crs::Epsg3857, &opts),
            Simplified::Dropped
        );
    }

    #[test]
    fn test_multilinestring_part_dropping() {
        let opts = SimplifyOptions::default();
        let long = LineString::new(vec![Coord { x: 0.0, y: 0.0 }, Coord { x: 10000.0, y: 0.0 }]);
        let short = LineString::new(vec![Coord { x: 0.0, y: 0.0 }, Coord { x: 5.0, y: 0.0 }]);
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![long, short]));
        match simplify_for_level(&mls, 1000.0, Crs::Epsg3857, &opts) {
            Simplified::Keep(Geometry::MultiLineString(m)) => {
                assert_eq!(m.0.len(), 1, "short part should be dropped");
            }
            other => panic!("expected Keep(MultiLineString) with 1 part, got {other:?}"),
        }
    }

    #[test]
    fn test_multilinestring_all_gone_dropped() {
        let opts = SimplifyOptions::default();
        let mls = Geometry::MultiLineString(MultiLineString::new(vec![
            LineString::new(vec![Coord { x: 0.0, y: 0.0 }, Coord { x: 5.0, y: 0.0 }]),
            LineString::new(vec![Coord { x: 0.0, y: 0.0 }, Coord { x: 3.0, y: 0.0 }]),
        ]));
        assert_eq!(
            simplify_for_level(&mls, 1000.0, Crs::Epsg3857, &opts),
            Simplified::Dropped
        );
    }
}
