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

use std::sync::atomic::{AtomicU64, Ordering};

use geo::{
    Area, BoundingRect, Centroid, Geometry, LineString, MultiLineString, MultiPolygon, Point,
    Polygon, Rect, Simplify, Validation,
};

pub use super::level::{Crs, METERS_PER_DEGREE};

/// Process-wide count of polygons that exhausted every epsilon-backoff retry
/// (see [`simplify_polygon_impl`]) and were kept at full resolution.
///
/// Callers (e.g. the streaming convert loop) log deltas of
/// [`full_resolution_fallback_count`] at debug level to expose how often the
/// last-resort path fires.
static FULL_RES_FALLBACKS: AtomicU64 = AtomicU64::new(0);

/// Snapshot of the process-wide full-resolution fallback counter.
pub fn full_resolution_fallback_count() -> u64 {
    FULL_RES_FALLBACKS.load(Ordering::Relaxed)
}

/// Process-wide count of RDP candidates that skipped the validity check
/// because they exceeded `MAX_VALIDATION_VERTS` (#242). Logged as a delta
/// alongside [`full_resolution_fallback_count`] by the streaming convert
/// loop.
static VALIDATION_SKIPS: AtomicU64 = AtomicU64::new(0);

/// Snapshot of the process-wide capped-validation skip counter.
pub fn validation_skip_count() -> u64 {
    VALIDATION_SKIPS.load(Ordering::Relaxed)
}

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
    /// Cascading simplification (#218, default **on**). When `true`:
    ///
    /// - conversion paths derive each coarser level from the next-finer
    ///   level's already-simplified output via [`simplify_cascade`] instead
    ///   of re-simplifying canonical geometry per level, and
    /// - a polygon whose RDP candidate self-intersects is repaired into its
    ///   valid even-odd interpretation
    ///   ([`crate::ioverlay_clip::repair_polygon_ioverlay`]) instead of
    ///   being epsilon-retried and ultimately kept at full resolution — a
    ///   full-resolution fallback would poison every coarser cascade step
    ///   for that feature.
    ///
    /// Output-changing: coarse-level geometry differs from the non-cascaded
    /// pipeline. `false` reproduces the pre-#218 output byte-for-byte.
    pub cascade: bool,
}

impl Default for SimplifyOptions {
    fn default() -> Self {
        Self {
            factor: DEFAULT_SIMPLIFY_FACTOR,
            collapse: false,
            cascade: true,
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

        Geometry::Polygon(poly) => simplify_polygon_impl(poly, tol, opts.collapse, opts.cascade),

        Geometry::MultiPolygon(mp) => {
            // Simplify each part with collapse disabled: a collapsed *part* is
            // dropped, never turned into a Point (a MultiPolygon cannot hold
            // one). Whole-feature collapse is decided after. A repaired part
            // (cascade path) may itself be a MultiPolygon; its parts are
            // flattened in.
            let kept: Vec<Polygon<f64>> =
                mp.0.iter()
                    .flat_map(
                        |p| match simplify_polygon_impl(p, tol, false, opts.cascade) {
                            Simplified::Keep(Geometry::Polygon(poly)) => vec![poly],
                            Simplified::Keep(Geometry::MultiPolygon(parts)) => parts.0,
                            _ => Vec::new(),
                        },
                    )
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

/// Cascading simplification (#218): fold a geometry through a fine→coarse
/// chain of level GSDs, feeding each coarser level the previous level's
/// already-simplified output instead of re-simplifying canonical geometry.
///
/// `gsds_fine_to_coarse` lists the GSDs of every non-canonical level from
/// the finest (first) down to the target level (last). The fold is a pure
/// function of `(geom, chain, crs, opts)` — independent of engine, batch
/// boundaries, and neighboring features — so every conversion path
/// (in-memory, serial streaming, pipelined) computes identical results for
/// identical inputs.
///
/// Dropping is monotone along the chain: tolerances grow while the working
/// geometry's extent can only shrink (RDP keeps a vertex subset), so a
/// feature dropped at a fine step can never survive a coarser one — the fold
/// short-circuits on the first [`Simplified::Dropped`].
///
/// An empty chain is the identity (bit-identical clone), matching
/// [`simplify_for_level`]'s canonical path at zero tolerance.
pub fn simplify_cascade(
    geom: &Geometry<f64>,
    gsds_fine_to_coarse: &[f64],
    crs: Crs,
    opts: &SimplifyOptions,
) -> Simplified {
    let mut current: Option<Geometry<f64>> = None;
    for &gsd in gsds_fine_to_coarse {
        let input = current.as_ref().unwrap_or(geom);
        match simplify_for_level(input, gsd, crs, opts) {
            Simplified::Keep(g) => current = Some(g),
            Simplified::Dropped => return Simplified::Dropped,
        }
    }
    Simplified::Keep(current.unwrap_or_else(|| geom.clone()))
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

/// Number of epsilon halvings tried when RDP produces an invalid
/// (self-intersecting) candidate before giving up and keeping the original
/// geometry. Attempts run at `tol, tol/2, tol/4, tol/8`.
const INVALID_RETRY_HALVINGS: u32 = 3;

/// Vertex cap above which the RDP candidate skips `geo`'s validity check and
/// is assumed valid.
///
/// # Why (issue #242)
///
/// `geo::algorithm::validation`'s per-ring simplicity test is **O(V²)** in
/// ring vertex count. A fine-GSD cascade step over a continental admin
/// polygon hands it a candidate with hundreds of thousands of vertices —
/// gdb sampling showed a single rayon worker pinned inside
/// `linestring_has_self_intersection` for the entire "convert wall" (~450 s
/// per level per feature at 300 K vertices, × every fine level), which is
/// exactly the export-side pathology capped in `clip.rs`
/// (`MAX_SELF_INTERSECT_VERTS`, issue #237).
///
/// # Why assuming valid is the right default above the cap
///
/// A candidate only stays huge when the epsilon was small relative to the
/// ring's detail, i.e. RDP removed few, near-collinear vertices from input
/// we already assume valid — the *least* likely candidate to have acquired a
/// crossing. The expensive-to-check case and the low-risk case coincide.
/// The trade is the same as `clip.rs`: a giant candidate that *did* acquire
/// a crossing ships unrepaired, which the overviews spec explicitly permits
/// (geometry validity is not a conformance requirement, OVERVIEWS_SPEC §
/// "validity") and matches tippecanoe, which never validates simplification
/// output. Everything at or below the cap is validated exactly as before.
const MAX_VALIDATION_VERTS: usize = 2_048;

/// Total vertices across the polygon's exterior and interior rings.
fn polygon_vertex_count(poly: &Polygon<f64>) -> usize {
    poly.exterior().0.len() + poly.interiors().iter().map(|r| r.0.len()).sum::<usize>()
}

/// `is_valid`, capped: candidates above [`MAX_VALIDATION_VERTS`] total
/// vertices are assumed valid without running the O(V²) scan (#242).
fn capped_is_valid(candidate: &Polygon<f64>) -> bool {
    let verts = polygon_vertex_count(candidate);
    if verts > MAX_VALIDATION_VERTS {
        VALIDATION_SKIPS.fetch_add(1, Ordering::Relaxed);
        log::trace!(
            "overview simplify: skipping O(V²) validity check on {verts}-vertex \
             candidate (cap {MAX_VALIDATION_VERTS}); assuming valid"
        );
        return true;
    }
    candidate.is_valid()
}

/// `true` when RDP removed nothing: the candidate has the same ring count and
/// per-ring vertex counts as the original. RDP output vertices are always an
/// ordered subset of the input (endpoints included), so equal counts imply an
/// identical geometry — validation of the candidate is then redundant (the
/// input is assumed valid, and re-checking it is exactly the H3(c) profile's
/// dominant cost at fine GSDs).
fn polygon_unchanged(candidate: &Polygon<f64>, original: &Polygon<f64>) -> bool {
    candidate.exterior().0.len() == original.exterior().0.len()
        && candidate.interiors().len() == original.interiors().len()
        && candidate
            .interiors()
            .iter()
            .zip(original.interiors())
            .all(|(a, b)| a.0.len() == b.0.len())
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
/// - If RDP introduces an invalid (self-intersecting) polygon:
///   - `repair` **off** (pre-#218 behavior): retry with a progressively
///     halved epsilon ([`INVALID_RETRY_HALVINGS`] retries): a smaller
///     tolerance keeps more vertices and usually restores validity while
///     still shedding sub-tolerance detail. Only when every retry fails is
///     the original geometry kept verbatim (boundary-preserving last resort,
///     counted in [`full_resolution_fallback_count`]).
///   - `repair` **on** (cascade path, #218): no retries — the candidate's
///     self-crossings are resolved into their valid even-odd interpretation
///     ([`repair_polygon_ioverlay`]), with repaired parts re-gated. A
///     full-resolution fallback here would poison every coarser cascade step
///     for the feature, and the epsilon retries were the profiled waste
///     (4× RDP + `is_valid` on near-full-resolution rings).
///
/// Validation-cost notes (H3(c) profile, lever 3): the candidate skips
/// `is_valid()` entirely when RDP removed no vertices (identical to the
/// assumed-valid input), and the original is **never** re-validated — the old
/// code ran a full-resolution `is_valid()` on the fallback path, which was
/// ~96% of coarse-level simplification cost. A consequence: an *invalid
/// source* polygon whose candidates all fail validation is now kept verbatim
/// (like the canonical level does) instead of being collapsed/dropped.
fn simplify_polygon_impl(
    poly: &Polygon<f64>,
    tol: f64,
    collapse: bool,
    repair: bool,
) -> Simplified {
    if polygon_diag(poly) < tol {
        return collapse_polygon(poly, collapse);
    }

    // Gates are level properties, so they stay at `tol` even when the RDP
    // epsilon backs off below it.
    let min_area = tol * tol;

    let attempts = if repair {
        1
    } else {
        INVALID_RETRY_HALVINGS + 1
    };
    let mut eps = tol;
    let mut invalid_candidate: Option<Polygon<f64>> = None;
    for _ in 0..attempts {
        let simplified = poly.simplify(eps);

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
        if candidate.exterior().0.len() < MIN_POLYGON_RING_POINTS
            || candidate.unsigned_area() < min_area
        {
            return collapse_polygon(poly, collapse);
        }

        if polygon_unchanged(&candidate, poly) || capped_is_valid(&candidate) {
            return Simplified::Keep(Geometry::Polygon(candidate));
        }
        invalid_candidate = Some(candidate);
        eps *= 0.5;
    }

    if repair {
        let candidate = invalid_candidate.expect("loop ran at least once");
        if let Some(repaired) = repair_candidate(&candidate, tol, min_area) {
            log::trace!(
                "overview simplify: repaired self-intersecting RDP candidate \
                 instead of keeping full resolution"
            );
            return Simplified::Keep(repaired);
        }
        // Repair left nothing above the gates (self-canceling sliver).
        return collapse_polygon(poly, collapse);
    }

    // Every retry self-intersected: keep the original geometry rather than
    // emit an invalid ring.
    FULL_RES_FALLBACKS.fetch_add(1, Ordering::Relaxed);
    log::trace!(
        "overview simplify: RDP candidate invalid after {} epsilon retries; \
         keeping full-resolution geometry",
        INVALID_RETRY_HALVINGS + 1
    );
    Simplified::Keep(Geometry::Polygon(poly.clone()))
}

/// Resolve an invalid RDP candidate's self-crossings into their valid
/// even-odd interpretation, re-applying the level gates per repaired part
/// (a bowtie lobe can fall below the visibility gate its parent passed).
/// Returns `None` when no part survives.
fn repair_candidate(candidate: &Polygon<f64>, tol: f64, min_area: f64) -> Option<Geometry<f64>> {
    let kept: Vec<Polygon<f64>> = match crate::ioverlay_clip::repair_polygon_ioverlay(candidate)? {
        Geometry::Polygon(p) => vec![p],
        Geometry::MultiPolygon(mp) => mp.0,
        _ => return None,
    }
    .into_iter()
    .filter(|p| polygon_diag(p) >= tol && p.unsigned_area() >= min_area)
    .collect();
    match kept.len() {
        0 => None,
        1 => Some(Geometry::Polygon(kept.into_iter().next().expect("len 1"))),
        _ => Some(Geometry::MultiPolygon(MultiPolygon::new(kept))),
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

    // ---- invalid-candidate progressive retry -------------------------------

    /// A polygon whose RDP output at `tol = 4` self-intersects: a square with
    /// a shallow downward notch in the bottom edge and a thin finger from the
    /// top descending into the notch pocket. RDP at 4 removes the sub-tolerance
    /// notch, leaving the finger tip below the straightened bottom edge (the
    /// finger edges then cross it). At `tol = 2` the notch survives and the
    /// ring is valid again. The sub-tolerance wobble vertices on the right and
    /// left edges are removed at *both* tolerances, so the epsilon-backoff
    /// retry still yields a genuinely simplified (not full-resolution) result.
    fn notch_finger_polygon() -> Polygon<f64> {
        let c = |x: f64, y: f64| Coord { x, y };
        Polygon::new(
            LineString::new(vec![
                c(0.0, 0.0),
                c(45.0, 0.0),
                c(50.0, -3.0),
                c(55.0, 0.0),
                c(100.0, 0.0),
                c(100.1, 30.0), // sub-tolerance wobble (removed at tol >= ~0.1)
                c(100.0, 60.0),
                c(99.9, 80.0), // sub-tolerance wobble
                c(100.0, 100.0),
                c(52.0, 100.0),
                c(50.0, -1.0),
                c(48.0, 100.0),
                c(0.0, 100.0),
                c(0.1, 50.0), // sub-tolerance wobble
                c(0.0, 0.0),
            ]),
            vec![],
        )
    }

    #[test]
    fn test_invalid_rdp_candidate_retries_to_simplified_valid() {
        let poly = notch_finger_polygon();
        let orig_len = poly.exterior().0.len();
        assert!(poly.is_valid(), "fixture must start valid");
        assert!(
            !poly.simplify(4.0).is_valid(),
            "fixture must self-intersect at the full tolerance (precondition)"
        );

        // gsd 4 m in EPSG:3857 (meters) with factor 1.0 => tol = 4.0.
        // cascade off pins the pre-#218 epsilon-retry behavior.
        let opts = SimplifyOptions {
            cascade: false,
            ..SimplifyOptions::default()
        };
        match simplify_for_level(&Geometry::Polygon(poly), 4.0, Crs::Epsg3857, &opts) {
            Simplified::Keep(Geometry::Polygon(p)) => {
                assert!(
                    p.is_valid(),
                    "retry output must be valid, got {:?}",
                    p.exterior()
                );
                assert!(
                    p.exterior().0.len() < orig_len,
                    "retry output must be simplified, not the full-resolution \
                     fallback ({} !< {orig_len})",
                    p.exterior().0.len()
                );
            }
            other => panic!("expected Keep(Polygon), got {other:?}"),
        }
    }

    #[test]
    fn test_invalid_rdp_candidate_repaired_when_cascade() {
        let poly = notch_finger_polygon();
        let orig_len = poly.exterior().0.len();
        assert!(
            !poly.simplify(4.0).is_valid(),
            "fixture must self-intersect at the full tolerance (precondition)"
        );

        // cascade on (default): the invalid candidate is repaired in one
        // pass instead of epsilon-retried or kept at full resolution.
        let opts = SimplifyOptions::default();
        let fallbacks_before = full_resolution_fallback_count();
        match simplify_for_level(&Geometry::Polygon(poly), 4.0, Crs::Epsg3857, &opts) {
            Simplified::Keep(g) => {
                assert!(g.is_valid(), "repaired output must be valid, got {g:?}");
                let out_len: usize = match &g {
                    Geometry::Polygon(p) => p.exterior().0.len(),
                    Geometry::MultiPolygon(mp) => mp.0.iter().map(|p| p.exterior().0.len()).sum(),
                    other => panic!("expected (Multi)Polygon, got {other:?}"),
                };
                assert!(
                    out_len < orig_len,
                    "repaired output must be simplified, not the \
                     full-resolution fallback ({out_len} !< {orig_len})"
                );
            }
            other => panic!("expected Keep, got {other:?}"),
        }
        assert_eq!(
            full_resolution_fallback_count(),
            fallbacks_before,
            "repair path must never count a full-resolution fallback"
        );
    }

    // ---- cascading simplification (#218) -----------------------------------

    #[test]
    fn test_cascade_default_on() {
        assert!(SimplifyOptions::default().cascade);
    }

    #[test]
    fn test_cascade_empty_chain_is_identity() {
        let g = Geometry::LineString(wiggly_line(50, 10.0));
        assert_eq!(
            simplify_cascade(&g, &[], Crs::Epsg3857, &SimplifyOptions::default()),
            Simplified::Keep(g.clone())
        );
    }

    #[test]
    fn test_cascade_single_step_matches_direct() {
        let g = Geometry::LineString(wiggly_line(200, 30.0));
        let opts = SimplifyOptions::default();
        assert_eq!(
            simplify_cascade(&g, &[100.0], Crs::Epsg3857, &opts),
            simplify_for_level(&g, 100.0, Crs::Epsg3857, &opts)
        );
    }

    #[test]
    fn test_cascade_vertices_subset_of_canonical() {
        // RDP keeps a vertex subset; the fold composes subsets, so every
        // cascaded vertex must be a canonical vertex.
        let canonical = wiggly_line(400, 60.0);
        let canonical_set: Vec<Coord<f64>> = canonical.0.clone();
        let g = Geometry::LineString(canonical);
        let opts = SimplifyOptions::default();
        match simplify_cascade(&g, &[25.0, 50.0, 100.0], Crs::Epsg3857, &opts) {
            Simplified::Keep(Geometry::LineString(out)) => {
                assert!(out.0.len() < canonical_set.len(), "chain must simplify");
                for c in &out.0 {
                    assert!(
                        canonical_set.contains(c),
                        "cascaded vertex {c:?} not in canonical geometry"
                    );
                }
            }
            other => panic!("expected Keep(LineString), got {other:?}"),
        }
    }

    #[test]
    fn test_cascade_drop_short_circuits() {
        // A feature below the gate at the *finest* chain step is dropped for
        // the coarser target too (monotone drops).
        let tiny = Geometry::LineString(LineString::new(vec![
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 10.0, y: 0.0 },
        ]));
        let opts = SimplifyOptions::default();
        assert_eq!(
            simplify_cascade(&tiny, &[1000.0, 5000.0], Crs::Epsg3857, &opts),
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

    // ---- validation vertex cap (#242) --------------------------------------

    /// A bowtie (self-crossing) quad whose left edge is padded with a fine
    /// staircase (per period: four corners with a 0.05 x-excursion, which RDP
    /// at epsilon 0.01 always keeps, plus one filler vertex offset 0.001 from
    /// the middle of a straight run, which RDP always removes). The removed
    /// fillers defeat the `polygon_unchanged` short-circuit so the candidate
    /// reaches validation; the surviving corners keep it big. The staircase
    /// lives at x ∈ [0, 0.051], y ∈ [1, 9] — far from the diagonals'
    /// crossing at (5, 5) — so the candidate stays a genuine bowtie.
    fn padded_bowtie(periods: usize) -> Polygon<f64> {
        let mut v = vec![
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 10.0, y: 10.0 },
            Coord { x: 10.0, y: 0.0 },
            Coord { x: 0.0, y: 10.0 },
        ];
        // Descend the left edge from y=9 toward y=1.
        let h = 8.0 / periods as f64;
        for i in 0..periods {
            let y = 9.0 - i as f64 * h;
            v.push(Coord { x: 0.0, y });
            v.push(Coord { x: 0.05, y });
            // Filler on the vertical run at x=0.05: deviates only 0.001 from
            // the run's chord, so RDP (eps 0.01) removes it.
            v.push(Coord {
                x: 0.051,
                y: y - h / 2.0,
            });
            v.push(Coord { x: 0.05, y: y - h });
            v.push(Coord { x: 0.0, y: y - h });
        }
        v.push(v[0]);
        Polygon::new(LineString::new(v), vec![])
    }

    #[test]
    fn validation_skipped_above_vertex_cap_keeps_candidate() {
        // geo's recursive RDP degenerates to O(n) recursion depth on the
        // uniform-amplitude zigzag (every split is a tie), which overflows
        // the default test stack in debug builds — run on a roomy stack.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(validation_skipped_above_vertex_cap_impl)
            .unwrap()
            .join()
            .unwrap();
    }

    /// The bowtie's two diagonals cross at exactly (5, 5). Even-odd repair
    /// materializes that crossing as an explicit vertex; the raw RDP
    /// candidate has no vertex anywhere near it (corners sit on x ∈ {0, 10},
    /// padding on x ≈ 0). Presence of the vertex is therefore a race-free
    /// witness that repair ran.
    fn has_crossing_vertex(g: &Geometry<f64>) -> bool {
        use geo::coords_iter::CoordsIter;
        g.coords_iter()
            .any(|c| (c.x - 5.0).abs() < 1e-6 && (c.y - 5.0).abs() < 1e-6)
    }

    fn validation_skipped_above_vertex_cap_impl() {
        // Above MAX_VALIDATION_VERTS the O(V²) `is_valid` scan is skipped and
        // the RDP candidate is assumed valid (#242): the bowtie is kept
        // verbatim, crossing and all, instead of being even-odd repaired.
        // 1200 periods: RDP keeps at least the two x-extreme corners per
        // period (their 0.05 horizontal deviation is epsilon-independent),
        // so the candidate stays comfortably above the cap.
        let poly = padded_bowtie(1_200);
        assert!(poly.exterior().0.len() > MAX_VALIDATION_VERTS);
        match simplify_polygon_impl(&poly, 0.01, false, true) {
            Simplified::Keep(g @ Geometry::Polygon(_)) => {
                let Geometry::Polygon(ref out) = g else {
                    unreachable!()
                };
                assert!(
                    out.exterior().0.len() > MAX_VALIDATION_VERTS,
                    "RDP should keep the zigzag padding (got {} verts)",
                    out.exterior().0.len()
                );
                assert!(
                    out.exterior().0.len() < poly.exterior().0.len(),
                    "RDP should remove the sub-epsilon padding vertices"
                );
                assert!(
                    !has_crossing_vertex(&g),
                    "candidate must be kept verbatim, not repaired"
                );
            }
            other => panic!("expected Keep(Polygon) above the cap, got {other:?}"),
        }
    }

    #[test]
    fn validation_exact_below_vertex_cap_still_repairs() {
        // Below the cap nothing changes: the invalid candidate is detected
        // and even-odd repaired, materializing the (5,5) crossing vertex.
        let poly = padded_bowtie(40);
        assert!(poly.exterior().0.len() <= MAX_VALIDATION_VERTS);
        match simplify_polygon_impl(&poly, 0.01, false, true) {
            Simplified::Keep(g) => {
                assert!(
                    has_crossing_vertex(&g),
                    "below the cap the bowtie must be repaired, got {g:?}"
                );
            }
            other => panic!("expected Keep(repaired geometry), got {other:?}"),
        }
    }
}
