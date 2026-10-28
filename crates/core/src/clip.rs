//! Geometry clipping to tile bounds.
//!
//! Clips geometries to tile boundaries with a configurable buffer zone to prevent
//! visual seams when rendering adjacent tiles.
//!
//! # Tippecanoe Alignment
//!
//! This module matches tippecanoe's clipping behavior:
//! - **Buffer**: Default 8 pixels (configurable via `--buffer` in tippecanoe)
//!   Buffer is measured in "screen pixels" where 1 pixel = 1/256th of tile width
//! - **Clipping method**: Features are clipped to tile boundary + buffer zone
//! - **Duplication**: Features may appear in multiple tiles if they span boundaries
//! - **Algorithm**: Uses Sutherland-Hodgman for polygon clipping against axis-aligned
//!   tile boundaries (same approach as tippecanoe's clip.cpp). This is O(n) and
//!   specialized for rectangle clipping. For edge cases where S-H produces invalid
//!   output (self-intersecting polygons, U-shapes that split), we fall back to
//!   `i_overlay`'s robust boolean operations. Lines are clipped by that same
//!   `i_overlay` (`ioverlay_clip::clip_multilinestring_ioverlay`, #435), so one
//!   engine decides where the tile edge is for every geometry type.
//!
//! # Edge Case Handling (Issue #94)
//!
//! Sutherland-Hodgman cannot handle:
//! - Self-intersecting input polygons
//! - U-shaped polygons clipped across the opening (should produce MultiPolygon)
//! - Polygons with holes that intersect the exterior ring
//!
//! When S-H produces output with structural issues (detected via cheap O(n) checks),
//! we fall back to `i_overlay` which handles these cases correctly.
//!
//! See: <https://github.com/felt/tippecanoe> (clipping documentation)

use geo::algorithm::sweep::Intersections;
use geo::{
    BoundingRect, Coord, Geometry, Line, LineString, MultiLineString, MultiPolygon, Point, Polygon,
    Rect,
};

use crate::ioverlay_clip;
use crate::sutherland_hodgman;
use crate::tile::TileBounds;

/// Default buffer in pixels (matches tippecanoe's common usage)
/// Tippecanoe default is 5, but CLAUDE.md specifies 8 for this project
pub const DEFAULT_BUFFER_PIXELS: u32 = 8;

/// Default tile extent in pixels
pub const DEFAULT_EXTENT: u32 = 4096;

// ============================================================================
// Structural Validity Checks (Issue #94)
// ============================================================================

/// Check if a polygon has structural issues that indicate clipping failure.
///
/// This performs cheap O(n) checks for problems that Sutherland-Hodgman
/// can produce when clipping invalid or complex geometries:
///
/// - **Degenerate rings**: Less than 4 vertices (minimum for valid polygon)
/// - **Duplicate consecutive vertices**: Self-touching at a point
/// - **Self-intersecting edges**: Edges that cross each other
///
/// # `assume_simple` (issue #237, RC3)
///
/// The self-intersecting-edges test (an O((n + m) log n) sweep, issue #241) runs
/// on every clip. When the caller has already established that the source
/// feature's rings are simple (no self-intersections) — computed **once per
/// feature** via [`geometry_is_simple`] — pass `assume_simple = true` to skip
/// that check here. A continental admin polygon covering thousands of tiles thus
/// pays the sweep once instead of once per tile. This is byte-identical, not
/// merely an approximation: `assume_simple` is only ever `true` when
/// [`has_self_intersecting_edges`] would have returned `false`, so the branch it
/// skips could not have changed the result. The cheap O(n) degenerate/duplicate
/// checks always run.
///
/// # Performance
///
/// - O(n) for the vertex checks (always run)
/// - O((n + m) log n) sweep for edge intersection (skipped when `assume_simple`)
fn has_structural_issues(poly: &Polygon<f64>, assume_simple: bool) -> bool {
    let ring = poly.exterior();

    // Check for degenerate ring (need at least 4 vertices for valid closed polygon)
    if ring.0.len() < 4 {
        return true;
    }

    // Check for duplicate consecutive vertices (self-touching)
    // Skip checking the closing vertex which legitimately matches the first
    for i in 0..ring.0.len() - 1 {
        let curr = ring.0[i];
        let next = ring.0[i + 1];
        // Use epsilon comparison for floating point
        if (curr.x - next.x).abs() < 1e-10 && (curr.y - next.y).abs() < 1e-10 {
            // This is only okay for the closing vertex
            if i != ring.0.len() - 2 {
                return true;
            }
        }
    }

    // Check for self-intersecting edges (O((n + m) log n) sweepline). Skipped
    // when the feature was pre-validated as simple — see the `assume_simple`
    // note above.
    if !assume_simple && has_self_intersecting_edges(&ring.0) {
        return true;
    }

    false
}

/// Check if a ring has self-intersecting edges (any two non-adjacent edges that
/// *properly* cross).
///
/// # Sweepline (issue #241)
///
/// This runs a Bentley–Ottmann sweep (`geo::algorithm::sweep::Intersections`) to
/// enumerate candidate intersecting edge pairs in **O((n + m) log n)** — where
/// `m` is the number of x-overlapping pairs — instead of the previous
/// **O(n²)** pairwise scan. The sweep replaced a capped scan (issue #237 skipped
/// rings above 2048 vertices to keep the export from stalling on continental
/// admin rings); with the sweep the cap is gone, so even a genuinely
/// self-intersecting giant ring is now detected and routed to `i_overlay` repair.
///
/// ## Byte-identical decision
///
/// The sweep only decides which pairs to *test*; the accept/reject decision is
/// the unchanged [`edges_intersect_properly`] predicate applied to each
/// candidate. A proper crossing meets at a point interior to both edges, so its
/// two edges always overlap in x and are therefore always surfaced by the sweep
/// — no proper crossing is missed. Adjacent edges (sharing a vertex) and
/// collinear overlaps yield a zero cross-product and are rejected by the
/// predicate exactly as before, so the boolean result matches the old scan on
/// every real (simple) ring. Degenerate zero-length edges are dropped up front,
/// matching the old scan's `continue`.
fn has_self_intersecting_edges(coords: &[Coord<f64>]) -> bool {
    let n = coords.len();
    if n < 4 {
        return false;
    }

    // Ring edges as sweep-line segments, dropping degenerate (zero-length)
    // edges just as the previous pairwise scan skipped them.
    let segments: Vec<Line<f64>> = coords
        .windows(2)
        .filter_map(|w| {
            let (a, b) = (w[0], w[1]);
            if (a.x - b.x).abs() < 1e-10 && (a.y - b.y).abs() < 1e-10 {
                None
            } else {
                Some(Line::new(a, b))
            }
        })
        .collect();

    // The sweep yields every edge pair that overlaps in x and meets; re-apply
    // the exact proper-crossing predicate to keep the result byte-identical to
    // the old O(n²) scan (see the note above).
    Intersections::from_iter(segments)
        .any(|(a, b, _)| edges_intersect_properly(a.start, a.end, b.start, b.end))
}

/// Check if two line segments intersect properly (crossing, not touching at endpoints).
///
/// Uses the cross-product orientation test.
fn edges_intersect_properly(
    a1: Coord<f64>,
    a2: Coord<f64>,
    b1: Coord<f64>,
    b2: Coord<f64>,
) -> bool {
    let d1 = cross_product_sign(b1, b2, a1);
    let d2 = cross_product_sign(b1, b2, a2);
    let d3 = cross_product_sign(a1, a2, b1);
    let d4 = cross_product_sign(a1, a2, b2);

    // Segments cross if endpoints are on opposite sides of each other's lines
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
        && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

/// Compute the cross product sign for orientation test.
fn cross_product_sign(a: Coord<f64>, b: Coord<f64>, c: Coord<f64>) -> f64 {
    (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x)
}

/// Check if a geometry (Polygon or MultiPolygon) has structural issues.
/// See [`has_structural_issues`] for the meaning of `assume_simple`.
fn geometry_has_structural_issues(geom: &Geometry<f64>, assume_simple: bool) -> bool {
    match geom {
        Geometry::Polygon(p) => has_structural_issues(p, assume_simple),
        Geometry::MultiPolygon(mp) => mp.0.iter().any(|p| has_structural_issues(p, assume_simple)),
        _ => false,
    }
}

/// Whether a geometry's polygon exterior rings are **simple** (free of
/// self-intersecting edges).
///
/// This runs the same self-intersection sweep as `has_structural_issues` (an
/// O((n + m) log n) Bentley–Ottmann pass, issue #241), but is meant to be called
/// **once per feature** rather than once per clip. Threading the result into the
/// clip pipeline as `assume_simple` lets the hot path skip that check on every
/// tile the feature touches (issue #237, RC3): a continental admin polygon
/// covering thousands of tiles pays the cost once instead of thousands of times.
///
/// Only exterior rings are inspected, matching what `has_structural_issues`
/// (and hence the S-H validity gate) actually checks. Non-polygon geometries
/// are trivially simple for the purposes of rectangle clipping.
pub fn geometry_is_simple(geom: &Geometry<f64>) -> bool {
    match geom {
        Geometry::Polygon(p) => !has_self_intersecting_edges(&p.exterior().0),
        Geometry::MultiPolygon(mp) => {
            mp.0.iter()
                .all(|p| !has_self_intersecting_edges(&p.exterior().0))
        }
        _ => true,
    }
}

/// Check if a polygon has edges that run along the clip boundary.
///
/// This detects the case where S-H incorrectly connects disconnected regions
/// by tracing along the clip boundary. For example, a U-shaped polygon clipped
/// across its opening will have an edge running along the bottom of the clip
/// region, connecting the two arms.
///
/// Returns true if any edge lies entirely on a boundary (same x or y coordinate
/// for both endpoints, matching a boundary value).
fn has_boundary_connecting_edges(poly: &Polygon<f64>, bounds: &TileBounds) -> bool {
    let ring = poly.exterior();
    let eps = 1e-10;

    for window in ring.0.windows(2) {
        let p1 = window[0];
        let p2 = window[1];

        // Skip if this is a degenerate (zero-length) edge
        if (p1.x - p2.x).abs() < eps && (p1.y - p2.y).abs() < eps {
            continue;
        }

        // Check if edge lies on left boundary (both x == lng_min)
        if (p1.x - bounds.lng_min).abs() < eps && (p2.x - bounds.lng_min).abs() < eps {
            // Edge runs along left boundary - this is connecting
            return true;
        }

        // Check if edge lies on right boundary (both x == lng_max)
        if (p1.x - bounds.lng_max).abs() < eps && (p2.x - bounds.lng_max).abs() < eps {
            return true;
        }

        // Check if edge lies on bottom boundary (both y == lat_min)
        if (p1.y - bounds.lat_min).abs() < eps && (p2.y - bounds.lat_min).abs() < eps {
            return true;
        }

        // Check if edge lies on top boundary (both y == lat_max)
        if (p1.y - bounds.lat_max).abs() < eps && (p2.y - bounds.lat_max).abs() < eps {
            return true;
        }
    }

    false
}

// ============================================================================
// Public Clipping API
// ============================================================================

pub fn clip_geometry(
    geom: &Geometry<f64>,
    bounds: &TileBounds,
    buffer: f64,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Option<Geometry<f64>> {
    // Backward-compatible entry point: validate simplicity per clip (the
    // pre-#237 behavior). Hot export paths should call `clip_geometry_simple`
    // with a per-feature `assume_simple` flag instead.
    clip_geometry_simple(geom, bounds, buffer, false, false, diagnostics)
}

/// Clip a geometry to buffered tile bounds, with caller-supplied hints that skip
/// redundant validity work on features already proven simple.
///
/// # `assume_simple` (issue #237, RC3)
///
/// Pass `true` **only** when the source feature's rings have already been proven
/// simple via [`geometry_is_simple`]. It skips the O((n + m) log n)
/// self-intersection sweep per clip (`has_self_intersecting_edges`); under
/// its precondition that sweep would always have returned "no issue", so the
/// result is byte-identical to [`clip_geometry`].
///
/// # `skip_boundary_fallback` (issue #239)
///
/// Consulted **only when `assume_simple` is also true**. It additionally skips
/// the O(V) `has_boundary_connecting_edges` gate, so a simple polygon whose S-H
/// clip runs an edge along the tile boundary keeps the (cheap) S-H result
/// instead of routing to `i_overlay`. For simple rings that S-H "bridge" is a
/// self-touching ring that is area- and fill-equivalent to `i_overlay`'s split
/// under nonzero winding (measured — see `fastpath_u_render_equivalent`), so this
/// removes the ~94% fine-zoom `i_overlay` fallback without changing rendered
/// output. On non-simple inputs the gate always runs, preserving the #94 fix.
pub fn clip_geometry_simple(
    geom: &Geometry<f64>,
    bounds: &TileBounds,
    buffer: f64,
    assume_simple: bool,
    skip_boundary_fallback: bool,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Option<Geometry<f64>> {
    let buffered = TileBounds::new(
        bounds.lng_min - buffer,
        bounds.lat_min - buffer,
        bounds.lng_max + buffer,
        bounds.lat_max + buffer,
    );

    match geom {
        Geometry::Point(p) => clip_point(p, &buffered).map(Geometry::Point),
        Geometry::LineString(ls) => clip_linestring(ls, &buffered, diagnostics),
        Geometry::Polygon(poly) => clip_polygon(
            poly,
            &buffered,
            assume_simple,
            skip_boundary_fallback,
            diagnostics,
        ),
        Geometry::MultiPolygon(mp) => clip_multipolygon(
            mp,
            &buffered,
            assume_simple,
            skip_boundary_fallback,
            diagnostics,
        )
        .map(Geometry::MultiPolygon),
        Geometry::MultiLineString(mls) => clip_multilinestring(mls, &buffered, diagnostics),
        other => {
            // For other geometry types, use bounding box check
            if let Some(rect) = other.bounding_rect() {
                if intersects_bounds(&rect, &buffered) {
                    return Some(other.clone());
                }
            }
            None
        }
    }
}

/// Convert buffer from pixels to degrees based on tile bounds.
///
/// # Arguments
///
/// * `buffer_pixels` - Buffer size in pixels (e.g., 8)
/// * `tile_bounds` - The tile bounds to calculate pixel size from
/// * `extent` - Tile extent in pixels (e.g., 4096)
///
/// # Returns
///
/// Buffer size in degrees (same units as tile bounds)
pub fn buffer_pixels_to_degrees(buffer_pixels: u32, tile_bounds: &TileBounds, extent: u32) -> f64 {
    // Buffer is specified in "screen pixels" where the tile is extent pixels wide
    // Convert to the same units as bounds (degrees)
    tile_bounds.width() * buffer_pixels as f64 / extent as f64
}

/// Check if a rectangle intersects the given bounds
fn intersects_bounds(rect: &Rect<f64>, bounds: &TileBounds) -> bool {
    rect.max().x >= bounds.lng_min
        && rect.min().x <= bounds.lng_max
        && rect.max().y >= bounds.lat_min
        && rect.min().y <= bounds.lat_max
}

/// A polygon's bounding rect over every ring, and whether every coordinate
/// is finite.
///
/// Over every ring, not just the exterior: `geo`'s `Polygon::bounding_rect`
/// covers the exterior only, which is right for valid input (holes lie
/// inside the exterior), but the convert pass carries invalid input verbatim
/// (#188), and a hole outside or across its exterior would otherwise ride
/// the fully-inside fast path below into the tile unclipped — thousands of
/// MVT units past the buffer (found by the #205 hostile-geometry harness).
/// For valid polygons the rect is the exterior's, so the fast path is
/// byte-identical there.
///
/// And with a finiteness flag, from the same pass: `geo`'s min/max skip a
/// NaN that is not a ring's first coordinate, so a NaN-bearing polygon could
/// take the fully-inside fast path and never reach the `i_overlay` guard
/// (#707). The callers refuse a non-finite polygon, counted, instead.
fn polygon_rect_all_rings(poly: &Polygon<f64>) -> Option<(Rect<f64>, bool)> {
    let mut coords = poly
        .exterior()
        .0
        .iter()
        .chain(poly.interiors().iter().flat_map(|ring| ring.0.iter()));
    let first = coords.next()?;
    let (mut min, mut max) = (*first, *first);
    let mut finite = first.x.is_finite() && first.y.is_finite();
    for c in coords {
        finite &= c.x.is_finite() && c.y.is_finite();
        min = Coord {
            x: min.x.min(c.x),
            y: min.y.min(c.y),
        };
        max = Coord {
            x: max.x.max(c.x),
            y: max.y.max(c.y),
        };
    }
    Some((Rect::new(min, max), finite))
}

/// [`polygon_rect_all_rings`] over every part of a MultiPolygon.
fn multipolygon_rect_all_rings(mp: &MultiPolygon<f64>) -> Option<(Rect<f64>, bool)> {
    let mut rects = mp.0.iter().filter_map(polygon_rect_all_rings);
    let first = rects.next()?;
    Some(rects.fold(first, |(acc, acc_finite), (r, finite)| {
        (
            Rect::new(
                Coord {
                    x: acc.min().x.min(r.min().x),
                    y: acc.min().y.min(r.min().y),
                },
                Coord {
                    x: acc.max().x.max(r.max().x),
                    y: acc.max().y.max(r.max().y),
                },
            ),
            acc_finite && finite,
        )
    }))
}

/// Check if a rectangle is fully contained within the given bounds
fn is_fully_inside(rect: &Rect<f64>, bounds: &TileBounds) -> bool {
    rect.min().x >= bounds.lng_min
        && rect.max().x <= bounds.lng_max
        && rect.min().y >= bounds.lat_min
        && rect.max().y <= bounds.lat_max
}

// ============================================================================
// Geometry Clipping Functions
// ============================================================================

/// Clip a point to bounds (simple containment check)
fn clip_point(point: &Point<f64>, bounds: &TileBounds) -> Option<Point<f64>> {
    if point.x() >= bounds.lng_min
        && point.x() <= bounds.lng_max
        && point.y() >= bounds.lat_min
        && point.y() <= bounds.lat_max
    {
        Some(*point)
    } else {
        None
    }
}

/// Clip a linestring to bounds on `i_overlay` (#435).
///
/// Same engine as the polygon fallback (`ioverlay_clip`), so lines and
/// polygons never disagree about where the tile edge is. Was
/// `geo::BooleanOps::clip`, the `i_overlay` 4.x copy `geo` vendors.
fn clip_linestring(
    ls: &LineString<f64>,
    bounds: &TileBounds,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Option<Geometry<f64>> {
    // Quick rejection test
    if let Some(rect) = ls.bounding_rect() {
        if !intersects_bounds(&rect, bounds) {
            return None;
        }
    }

    // Keep the parts INSIDE the bounds (not inverted).
    let mls = MultiLineString::new(vec![ls.clone()]);
    let clipped = ioverlay_clip::clip_multilinestring_ioverlay(&mls, bounds, diagnostics);

    if clipped.0.is_empty() {
        None
    } else if clipped.0.len() == 1 {
        Some(Geometry::LineString(clipped.0.into_iter().next().unwrap()))
    } else {
        Some(Geometry::MultiLineString(clipped))
    }
}

/// Clip a multilinestring to bounds
fn clip_multilinestring(
    mls: &MultiLineString<f64>,
    bounds: &TileBounds,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Option<Geometry<f64>> {
    // Quick rejection test
    if let Some(rect) = mls.bounding_rect() {
        if !intersects_bounds(&rect, bounds) {
            return None;
        }
    }

    let clipped = ioverlay_clip::clip_multilinestring_ioverlay(mls, bounds, diagnostics);

    if clipped.0.is_empty() {
        None
    } else {
        Some(Geometry::MultiLineString(clipped))
    }
}

fn clip_polygon(
    poly: &Polygon<f64>,
    bounds: &TileBounds,
    assume_simple: bool,
    skip_boundary_fallback: bool,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Option<Geometry<f64>> {
    // Quick rejection test using bounding box. Over every ring, not just the
    // exterior, so an interior ring outside its exterior cannot take the
    // fully-inside fast path below unclipped (#205).
    let (poly_rect, finite) = polygon_rect_all_rings(poly)?;
    if !finite {
        // A NaN or infinite vertex: nothing to clip, and the fast path below
        // would emit it unclipped. Dropped, and counted (#707).
        ioverlay_clip::count_guard_drop(diagnostics);
        return None;
    }
    if !intersects_bounds(&poly_rect, bounds) {
        return None;
    }

    // Check if input polygon has structural issues (self-intersecting, etc.)
    // If so, we MUST use i_overlay even for "fully inside" polygons because
    // i_overlay will repair the geometry while S-H cannot. When the feature was
    // pre-validated as simple (issue #237), the self-intersection sweep is
    // skipped here — see `has_structural_issues`.
    let input_has_issues = has_structural_issues(poly, assume_simple);

    // FAST PATH: If polygon is fully inside bounds AND valid, return as-is
    if is_fully_inside(&poly_rect, bounds) && !input_has_issues {
        return Some(Geometry::Polygon(poly.clone()));
    }

    // If input has structural issues, go directly to i_overlay (skip S-H)
    // i_overlay handles self-intersecting polygons by splitting them into valid parts
    if input_has_issues {
        return ioverlay_clip::clip_polygon_ioverlay(poly, bounds, diagnostics);
    }

    // Primary path: Use Sutherland-Hodgman for O(n) rectangle clipping
    let sh_result = sutherland_hodgman::clip_polygon_sh(poly, bounds);

    // Validate S-H output and fall back to i_overlay on structural issues or
    // boundary-connecting bridges. `assume_simple` (issue #237) only suppresses
    // the self-intersection re-sweep inside `geometry_has_structural_issues`.
    //
    // `skip_boundary_fallback` (issue #239) additionally suppresses the O(V)
    // `has_boundary_connecting_edges` gate, but ONLY when `assume_simple` — for a
    // ring proven simple, S-H's boundary-following "bridge" is a self-touching
    // ring that is area- and fill-identical to the i_overlay result under nonzero
    // winding (measured: `fastpath_u_render_equivalent`), so the fallback is pure
    // wasted work. On non-simple inputs the gate always runs, preserving #94.
    let skip_boundary = assume_simple && skip_boundary_fallback;
    match &sh_result {
        Some(Geometry::Polygon(p)) => {
            // Check for structural issues OR boundary-connecting edges
            // (the latter indicates S-H connected disconnected regions)
            if geometry_has_structural_issues(sh_result.as_ref().unwrap(), assume_simple)
                || (!skip_boundary && has_boundary_connecting_edges(p, bounds))
            {
                ioverlay_clip::clip_polygon_ioverlay(poly, bounds, diagnostics)
            } else {
                sh_result
            }
        }
        Some(Geometry::MultiPolygon(mp)) => {
            // Check each polygon for issues
            let has_issues = mp.0.iter().any(|p| {
                has_structural_issues(p, assume_simple)
                    || (!skip_boundary && has_boundary_connecting_edges(p, bounds))
            });
            if has_issues {
                ioverlay_clip::clip_polygon_ioverlay(poly, bounds, diagnostics)
            } else {
                sh_result
            }
        }
        Some(_) => {
            // Other geometry type - shouldn't happen for polygon clipping
            sh_result
        }
        None => {
            // S-H returned None - polygon doesn't intersect bounds
            // (This shouldn't happen given the bbox check above, but handle it)
            None
        }
    }
}

fn clip_multipolygon(
    mp: &MultiPolygon<f64>,
    bounds: &TileBounds,
    assume_simple: bool,
    skip_boundary_fallback: bool,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Option<MultiPolygon<f64>> {
    // Level 1: Quick rejection using overall MultiPolygon bbox (every ring,
    // see `polygon_rect_all_rings`).
    let (mp_rect, finite) = multipolygon_rect_all_rings(mp)?;
    if !finite {
        // One hostile vertex poisons the whole input, as in the i_overlay
        // guard: dropped, and counted (#707).
        ioverlay_clip::count_guard_drop(diagnostics);
        return None;
    }
    if !intersects_bounds(&mp_rect, bounds) {
        return None;
    }

    // FAST PATH: If entire multipolygon is fully inside bounds, return as-is
    if is_fully_inside(&mp_rect, bounds) {
        return Some(mp.clone());
    }

    // Level 2: Per-polygon bbox filter + clip
    // Each polygon is individually tested with its own bounding box before
    // any clipping is attempted. This avoids expensive operations for
    // sub-polygons that are far from the tile.
    let mut clipped_polys = Vec::new();
    for poly in &mp.0 {
        // Per-polygon bbox filter: compute each polygon's bbox and check
        // intersection before calling into the clip pipeline
        // Every part is finite: the whole MultiPolygon was checked above.
        let poly_rect = match polygon_rect_all_rings(poly) {
            Some((r, _)) => r,
            None => continue, // Degenerate polygon, skip
        };

        if !intersects_bounds(&poly_rect, bounds) {
            // This polygon's bbox doesn't intersect the tile -- skip entirely.
            // This is the key optimization: for a MultiPolygon with 7453 polygons
            // where only ~100 intersect the tile, we skip 7353 polygons here
            // without any clipping work.
            continue;
        }

        // FAST PATH: If this polygon is fully inside bounds, add as-is
        if is_fully_inside(&poly_rect, bounds) {
            clipped_polys.push(poly.clone());
            continue;
        }

        // Polygon intersects but isn't fully inside -- needs clipping. The
        // clip may split one part into several disjoint pieces (a tile edge
        // crossing a wiggly boundary more than once, or the i_overlay
        // fallback resolving an S-H bridge into its true parts) — keep every
        // piece. Discarding the MultiPolygon case here silently deleted
        // boundary-straddling parts from individual tiles (#244).
        match clip_polygon(
            poly,
            bounds,
            assume_simple,
            skip_boundary_fallback,
            diagnostics,
        ) {
            Some(Geometry::Polygon(clipped)) => clipped_polys.push(clipped),
            Some(Geometry::MultiPolygon(pieces)) => clipped_polys.extend(pieces.0),
            Some(other) => debug_assert!(false, "polygon clip returned {other:?}"),
            None => {}
        }
    }

    if clipped_polys.is_empty() {
        None
    } else {
        Some(MultiPolygon::new(clipped_polys))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::point;

    // ========== Non-finite coordinates (issue #707) ==========

    /// A simple square fully inside the tile with a NaN at its third vertex:
    /// `geo`'s bounding rect skips a NaN that is not a ring's first
    /// coordinate, so this rode the fully-inside fast path and came back
    /// unclipped, never reaching the `i_overlay` guard.
    #[test]
    fn a_nan_vertex_cannot_take_the_fully_inside_fast_path() {
        let diagnostics = crate::diagnostics::Diagnostics::default();

        let poly = Polygon::new(
            LineString::from(vec![
                (1.0, 1.0),
                (2.0, 1.0),
                (f64::NAN, 2.0),
                (1.0, 2.0),
                (1.0, 1.0),
            ]),
            vec![],
        );
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        for geom in [
            Geometry::Polygon(poly.clone()),
            Geometry::MultiPolygon(MultiPolygon::new(vec![poly])),
        ] {
            let before =
                diagnostics.count(&crate::diagnostics::Diagnostic::ClipGuardDropped { count: 0 });
            assert_eq!(
                clip_geometry_simple(&geom, &bounds, 0.0, true, true, &diagnostics),
                None
            );
            assert!(
                diagnostics.count(&crate::diagnostics::Diagnostic::ClipGuardDropped { count: 0 })
                    > before,
                "the drop is counted"
            );
        }
    }

    // ========== Interior rings outside their exterior (issue #205) ==========

    fn ring_square(min: f64, max: f64) -> LineString<f64> {
        LineString::from(vec![
            (min, min),
            (max, min),
            (max, max),
            (min, max),
            (min, min),
        ])
    }

    fn assert_within(g: &Geometry<f64>, b: &TileBounds, what: &str) {
        use geo::CoordsIter;
        for c in g.coords_iter() {
            assert!(
                c.x >= b.lng_min && c.x <= b.lng_max && c.y >= b.lat_min && c.y <= b.lat_max,
                "{what}: vertex ({}, {}) lies outside the clip window {b:?}",
                c.x,
                c.y
            );
        }
    }

    /// The hostile-geometry harness (#205) caught this: `geo`'s
    /// `Polygon::bounding_rect` covers the EXTERIOR only, so a polygon whose
    /// exterior sits inside the tile took the fully-inside fast path and
    /// returned its interior rings verbatim — even a hole lying wholly
    /// outside the exterior (and the tile), thousands of MVT units past the
    /// buffer. Invalid input, but the convert pass carries it verbatim
    /// (#188) and the tile must still stay inside its window.
    #[test]
    fn hole_outside_its_exterior_is_clipped_not_fast_pathed() {
        let poly = Polygon::new(ring_square(0.0, 10.0), vec![ring_square(20.0, 25.0)]);
        let window = TileBounds::new(-1.0, -1.0, 11.0, 11.0);
        for (assume_simple, fast) in [(false, false), (true, false), (true, true)] {
            let out = clip_geometry_simple(
                &Geometry::Polygon(poly.clone()),
                &window,
                0.0,
                assume_simple,
                fast,
                &crate::diagnostics::Diagnostics::default(),
            )
            .expect("exterior overlaps the window");
            assert_within(&out, &window, "hole outside exterior");
            // Through the MultiPolygon path too (its own per-part fast path).
            let out = clip_geometry_simple(
                &Geometry::MultiPolygon(MultiPolygon::new(vec![poly.clone()])),
                &window,
                0.0,
                assume_simple,
                fast,
                &crate::diagnostics::Diagnostics::default(),
            )
            .expect("exterior overlaps the window");
            assert_within(&out, &window, "hole outside exterior (multipolygon)");
        }
    }

    /// Same leak with a hole that crosses its exterior: the part of the hole
    /// past the window must be cut, not carried.
    #[test]
    fn hole_crossing_its_exterior_is_clipped_to_the_window() {
        let poly = Polygon::new(ring_square(0.0, 10.0), vec![ring_square(5.0, 15.0)]);
        let window = TileBounds::new(-1.0, -1.0, 11.0, 11.0);
        let out = clip_geometry_simple(
            &Geometry::Polygon(poly),
            &window,
            0.0,
            true,
            true,
            &crate::diagnostics::Diagnostics::default(),
        )
        .expect("exterior overlaps the window");
        assert_within(&out, &window, "hole crossing exterior");
    }

    /// A valid polygon (holes inside the exterior) fully inside the window
    /// still takes the fast path and comes back untouched — the fix widens
    /// the bbox by the interiors, which changes nothing for valid input.
    #[test]
    fn valid_polygon_with_holes_still_takes_the_fast_path_verbatim() {
        let poly = Polygon::new(ring_square(0.0, 10.0), vec![ring_square(2.0, 4.0)]);
        let window = TileBounds::new(-1.0, -1.0, 11.0, 11.0);
        let out = clip_geometry_simple(
            &Geometry::Polygon(poly.clone()),
            &window,
            0.0,
            true,
            true,
            &crate::diagnostics::Diagnostics::default(),
        )
        .expect("inside the window");
        assert_eq!(out, Geometry::Polygon(poly));
    }

    // ========== MultiPolygon part-splitting clips (issue #244) ==========

    /// A one-part MultiPolygon whose part is a U: two vertical prongs
    /// (x ∈ [0,1] and x ∈ [3,4], rising to y=5) joined by a base (y ∈ [0,1]).
    #[cfg(test)]
    fn u_multipolygon() -> MultiPolygon<f64> {
        MultiPolygon::new(vec![Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (4.0, 0.0),
                (4.0, 5.0),
                (3.0, 5.0),
                (3.0, 1.0),
                (1.0, 1.0),
                (1.0, 5.0),
                (0.0, 5.0),
                (0.0, 0.0),
            ]),
            vec![],
        )])
    }

    #[test]
    fn multipolygon_part_splitting_into_pieces_is_kept() {
        // Clip window covering only the prong tips: the single U part clips
        // into TWO disjoint pieces. `clip_polygon` correctly reports that as a
        // MultiPolygon; the part collector must keep both pieces instead of
        // silently dropping the result (#244 — the field failure dropped
        // boundary-straddling admin polygons from individual tiles, cutting
        // them off along razor-straight tile-boundary lines).
        let mp = u_multipolygon();
        let window = TileBounds::new(-0.5, 3.0, 4.5, 6.0);
        let clipped = clip_multipolygon(
            &mp,
            &window,
            false,
            false,
            &crate::diagnostics::Diagnostics::default(),
        )
        .expect("clip must not drop a genuinely-overlapping feature");
        assert_eq!(clipped.0.len(), 2, "both prong pieces must survive");
        // Same through the public geometry entry point, simple-path flavor.
        let via_public = clip_geometry_simple(
            &Geometry::MultiPolygon(mp),
            &window,
            0.0,
            true,
            false,
            &crate::diagnostics::Diagnostics::default(),
        )
        .expect("public entry point must keep the feature");
        match via_public {
            Geometry::MultiPolygon(m) => assert_eq!(m.0.len(), 2),
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    #[test]
    fn tielt_winge_boundary_sliver_regression() {
        // Real-data regression for #244: Tielt-Winge (fieldmaps Belgium adm4,
        // 401 verts, valid, simple) overlaps six z12 tiles; its eastern sliver
        // in tile 12/2104/1372 clips into two disjoint pieces and was silently
        // dropped, cutting the municipality off along lon 4.921875. Verifies
        // both the direct leaf clip and the export cascade's ancestor chain.
        use geo::Contains;
        use geozero::ToGeo;
        let path = std::path::Path::new("../../tests/fixtures/realdata/tielt-winge-adm4.wkb");
        if !path.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }
        let geom: Geometry<f64> = geozero::wkb::Wkb(std::fs::read(path).unwrap())
            .to_geo()
            .unwrap();
        let simple = geometry_is_simple(&geom);
        let buf = |b: &TileBounds| b.width() * 8.0 / 4096.0;

        // The user-reported hole: this point is inside the municipality and
        // inside tile 12/2104/1372.
        let hole = point!(x: 4.9265, y: 50.9468);
        let contains_hole = |g: &Geometry<f64>| match g {
            Geometry::MultiPolygon(m) => m.contains(&hole),
            Geometry::Polygon(p) => p.contains(&hole),
            other => panic!("unexpected clip output {other:?}"),
        };
        let leaf = crate::tile::tile_bounds(2104, 1372, 12);

        let direct = clip_geometry_simple(
            &geom,
            &leaf,
            buf(&leaf),
            simple,
            false,
            &crate::diagnostics::Diagnostics::default(),
        )
        .expect("direct leaf clip must keep the eastern sliver");
        assert!(
            contains_hole(&direct),
            "clipped sliver must cover the reported hole point"
        );

        // Export cascade replay: ancestors of (2104,1372) below the z8
        // covering tile. Every ancestor clip must retain the sliver region.
        let mut cur = geom;
        for (x, y, z) in [(263u32, 171u32, 9u8), (526, 343, 10), (1052, 686, 11)] {
            let nb = crate::tile::tile_bounds(x, y, z);
            cur = clip_geometry_simple(
                &cur,
                &nb,
                buf(&nb),
                simple,
                false,
                &crate::diagnostics::Diagnostics::default(),
            )
            .unwrap_or_else(|| panic!("cascade lost the feature at z{z} ({x},{y})"));
        }
        let casc = clip_geometry_simple(
            &cur,
            &leaf,
            buf(&leaf),
            simple,
            false,
            &crate::diagnostics::Diagnostics::default(),
        )
        .expect("cascade leaf clip must keep the eastern sliver");
        assert!(
            contains_hole(&casc),
            "cascade-clipped sliver must cover the reported hole point"
        );
    }

    // ========== Simplicity fast-path (issue #237, RC3) ==========

    fn square(minx: f64, miny: f64, maxx: f64, maxy: f64) -> Polygon<f64> {
        Polygon::new(
            LineString::from(vec![
                (minx, miny),
                (maxx, miny),
                (maxx, maxy),
                (minx, maxy),
                (minx, miny),
            ]),
            vec![],
        )
    }

    #[test]
    fn geometry_is_simple_true_for_simple_polygon() {
        let g = Geometry::Polygon(square(0.0, 0.0, 4.0, 4.0));
        assert!(geometry_is_simple(&g));
    }

    #[test]
    fn geometry_is_simple_false_for_bowtie() {
        // Figure-eight: edges (0,0)->(2,2) and (2,0)->(0,2) cross.
        let bowtie = Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (2.0, 2.0),
                (2.0, 0.0),
                (0.0, 2.0),
                (0.0, 0.0),
            ]),
            vec![],
        );
        assert!(!geometry_is_simple(&Geometry::Polygon(bowtie)));
    }

    #[test]
    fn geometry_is_simple_multipolygon_all_or_nothing() {
        let good = square(0.0, 0.0, 1.0, 1.0);
        let bowtie = Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (2.0, 2.0),
                (2.0, 0.0),
                (0.0, 2.0),
                (0.0, 0.0),
            ]),
            vec![],
        );
        assert!(geometry_is_simple(&Geometry::MultiPolygon(
            MultiPolygon::new(vec![good.clone(), square(5.0, 5.0, 6.0, 6.0)])
        )));
        assert!(!geometry_is_simple(&Geometry::MultiPolygon(
            MultiPolygon::new(vec![good, bowtie])
        )));
    }

    /// A crossing ("bowtie") ring of `n` vertices: two dense diagonals that
    /// intersect. Used to exercise the sweepline self-intersection test at a
    /// range of ring sizes.
    fn crossing_ring(n: usize) -> Vec<Coord<f64>> {
        let half = n / 2;
        let mut v: Vec<Coord<f64>> = Vec::with_capacity(n + 1);
        // Diagonal A: (0,0) -> (10,10), the line y = x.
        for i in 0..half {
            let t = i as f64 / half as f64;
            v.push(Coord {
                x: 10.0 * t,
                y: 10.0 * t,
            });
        }
        // Diagonal B: (10,0.7) -> (0,10.7), the line y = 10.7 - x. A and B cross
        // transversally at (~5.35, ~5.35), which the 0.7 offset keeps off every
        // vertex so it is a *proper* self-intersection.
        for i in 0..half {
            let t = i as f64 / half as f64;
            v.push(Coord {
                x: 10.0 - 10.0 * t,
                y: 0.7 + 10.0 * t,
            });
        }
        v.push(v[0]); // close
        v
    }

    #[test]
    fn self_intersection_detected_small_ring() {
        let ring = crossing_ring(64);
        assert!(has_self_intersecting_edges(&ring));
    }

    #[test]
    fn large_self_intersecting_ring_detected() {
        // Issue #241: a genuinely self-intersecting ring far larger than the old
        // O(n²) cap (2048) must still be reported as self-intersecting. The
        // sweepline test carries no vertex cap, so the crossing is found
        // regardless of ring size — closing the latent correctness gap left by
        // the capped scan.
        let ring = crossing_ring(6_000);
        assert!(ring.len() > 2_048);
        assert!(has_self_intersecting_edges(&ring));
    }

    #[test]
    fn large_simple_ring_not_self_intersecting() {
        // A large convex ring (regular polygon approximating a circle), far
        // above the old cap, must be reported simple. This guards the sweepline
        // against false positives at scale and confirms it stays fast where the
        // old O(n²) scan would have stalled (issue #241).
        let n = 20_000usize;
        let mut ring: Vec<Coord<f64>> = (0..n)
            .map(|i| {
                let theta = std::f64::consts::TAU * (i as f64) / (n as f64);
                Coord {
                    x: theta.cos(),
                    y: theta.sin(),
                }
            })
            .collect();
        ring.push(ring[0]); // close
        assert!(ring.len() > 2_048);
        assert!(!has_self_intersecting_edges(&ring));
    }

    #[test]
    fn geometry_is_simple_true_for_non_polygon() {
        let g = Geometry::Point(point!(x: 1.0, y: 1.0));
        assert!(geometry_is_simple(&g));
    }

    #[test]
    fn clip_geometry_simple_byte_identical_on_simple_input() {
        // A simple polygon that straddles the clip bounds (so it is actually
        // clipped, exercising the S-H path + output validity gate). With a
        // simple input, `assume_simple = true` must produce output identical to
        // the default full-validation path — the RC3 guarantee.
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let buffer = 0.5;
        let cases = vec![
            Geometry::Polygon(square(-3.0, -3.0, 5.0, 5.0)),
            Geometry::Polygon(square(2.0, 2.0, 20.0, 8.0)),
            Geometry::MultiPolygon(MultiPolygon::new(vec![
                square(-2.0, -2.0, 4.0, 4.0),
                square(6.0, 6.0, 13.0, 13.0),
            ])),
        ];
        for g in cases {
            assert!(geometry_is_simple(&g));
            let default = clip_geometry(
                &g,
                &bounds,
                buffer,
                &crate::diagnostics::Diagnostics::default(),
            );
            let fast = clip_geometry_simple(
                &g,
                &bounds,
                buffer,
                true,
                false,
                &crate::diagnostics::Diagnostics::default(),
            );
            assert_eq!(default, fast, "assume_simple output diverged for {g:?}");
        }
    }

    // ========== Point Clipping Tests ==========

    #[test]
    fn test_clip_point_inside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let point = point!(x: 5.0, y: 5.0);
        assert!(clip_point(&point, &bounds).is_some());
    }

    #[test]
    fn test_clip_point_outside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let point = point!(x: 15.0, y: 5.0);
        assert!(clip_point(&point, &bounds).is_none());
    }

    #[test]
    fn test_clip_point_on_boundary() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let point = point!(x: 10.0, y: 5.0);
        assert!(clip_point(&point, &bounds).is_some());
    }

    // ========== Polygon Clipping Tests ==========

    #[test]
    fn test_clip_polygon_partial() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: -5.0, y: -5.0 },
                Coord { x: 5.0, y: -5.0 },
                Coord { x: 5.0, y: 5.0 },
                Coord { x: -5.0, y: 5.0 },
                Coord { x: -5.0, y: -5.0 },
            ]),
            vec![],
        );

        let result = clip_polygon(
            &poly,
            &bounds,
            false,
            false,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(result.is_some());

        // Extract the polygon (should be single polygon for this simple case)
        let clipped = match result.unwrap() {
            Geometry::Polygon(p) => p,
            Geometry::MultiPolygon(mp) => mp.0.into_iter().next().unwrap(),
            _ => panic!("Expected polygon geometry"),
        };
        // Verify clipped polygon is within bounds
        for coord in clipped.exterior().coords() {
            assert!(
                coord.x >= 0.0 && coord.x <= 10.0,
                "x={} out of bounds",
                coord.x
            );
            assert!(
                coord.y >= 0.0 && coord.y <= 10.0,
                "y={} out of bounds",
                coord.y
            );
        }
    }

    #[test]
    fn test_clip_polygon_outside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: 20.0, y: 20.0 },
                Coord { x: 30.0, y: 20.0 },
                Coord { x: 30.0, y: 30.0 },
                Coord { x: 20.0, y: 30.0 },
                Coord { x: 20.0, y: 20.0 },
            ]),
            vec![],
        );
        assert!(clip_polygon(
            &poly,
            &bounds,
            false,
            false,
            &crate::diagnostics::Diagnostics::default()
        )
        .is_none());
    }

    #[test]
    fn test_clip_polygon_fully_inside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: 2.0, y: 2.0 },
                Coord { x: 8.0, y: 2.0 },
                Coord { x: 8.0, y: 8.0 },
                Coord { x: 2.0, y: 8.0 },
                Coord { x: 2.0, y: 2.0 },
            ]),
            vec![],
        );

        let result = clip_polygon(
            &poly,
            &bounds,
            false,
            false,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(result.is_some());
    }

    // ========== LineString Clipping Tests ==========

    #[test]
    fn test_clip_linestring_crossing() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let ls = LineString::from(vec![Coord { x: -5.0, y: 5.0 }, Coord { x: 15.0, y: 5.0 }]);

        let result = clip_linestring(&ls, &bounds, &crate::diagnostics::Diagnostics::default());
        assert!(result.is_some());
    }

    #[test]
    fn test_clip_linestring_outside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let ls = LineString::from(vec![Coord { x: 20.0, y: 20.0 }, Coord { x: 30.0, y: 30.0 }]);

        let result = clip_linestring(&ls, &bounds, &crate::diagnostics::Diagnostics::default());
        assert!(result.is_none());
    }

    #[test]
    fn test_clip_linestring_fully_inside() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let ls = LineString::from(vec![Coord { x: 2.0, y: 2.0 }, Coord { x: 8.0, y: 8.0 }]);

        let result = clip_linestring(&ls, &bounds, &crate::diagnostics::Diagnostics::default());
        assert!(result.is_some());
    }

    // ========== Buffer Calculation Tests ==========

    #[test]
    fn test_buffer_pixels_to_degrees() {
        let bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let buffer = buffer_pixels_to_degrees(8, &bounds, 4096);

        // 8 pixels / 4096 extent * 1.0 degree width = 0.001953125
        let expected = 8.0 / 4096.0;
        assert!(
            (buffer - expected).abs() < 1e-10,
            "buffer={} expected={}",
            buffer,
            expected
        );
    }

    #[test]
    fn test_buffer_affects_clipping() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let buffer = 2.0; // 2 degree buffer

        // Point just outside bounds but within buffer
        let point = point!(x: 11.0, y: 5.0);

        // Without buffer: should be outside
        assert!(clip_point(&point, &bounds).is_none());

        // With buffer via clip_geometry: should be inside
        let result = clip_geometry(
            &Geometry::Point(point),
            &bounds,
            buffer,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(result.is_some());
    }

    // ========== clip_geometry Integration Tests ==========

    #[test]
    fn test_clip_geometry_point() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let point = Geometry::Point(point!(x: 5.0, y: 5.0));

        let result = clip_geometry(
            &point,
            &bounds,
            0.0,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(result.is_some());
        assert!(matches!(result.unwrap(), Geometry::Point(_)));
    }

    #[test]
    fn test_clip_geometry_polygon() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                Coord { x: 5.0, y: 5.0 },
                Coord { x: 15.0, y: 5.0 },
                Coord { x: 15.0, y: 15.0 },
                Coord { x: 5.0, y: 15.0 },
                Coord { x: 5.0, y: 5.0 },
            ]),
            vec![],
        ));

        let result = clip_geometry(
            &poly,
            &bounds,
            0.0,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(result.is_some());
    }

    #[test]
    fn test_clip_geometry_with_buffer() {
        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let buffer = 1.0;

        // Polygon just outside bounds but overlapping with buffer
        let poly = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                Coord { x: 10.5, y: 5.0 },
                Coord { x: 12.0, y: 5.0 },
                Coord { x: 12.0, y: 8.0 },
                Coord { x: 10.5, y: 8.0 },
                Coord { x: 10.5, y: 5.0 },
            ]),
            vec![],
        ));

        // Without buffer: should be outside
        let result_no_buffer = clip_geometry(
            &poly,
            &bounds,
            0.0,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(result_no_buffer.is_none());

        // With buffer: should clip to buffered bounds
        let result_with_buffer = clip_geometry(
            &poly,
            &bounds,
            buffer,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(result_with_buffer.is_some());
    }

    // ========== Bounding Box Pre-filter Tests ==========

    #[test]
    fn test_multipolygon_bbox_prefilter_skips_distant_polygons() {
        // Simulates an "Antarctica-like" MultiPolygon: many sub-polygons spread
        // across a wide geographic area, clipped to a small tile that only
        // intersects a handful of them.
        //
        // This verifies that per-polygon bbox filtering correctly:
        // 1. Produces output only for the intersecting polygons
        // 2. Returns None for the non-intersecting ones
        //
        // The tile covers a 10x10 degree area at (0,0)-(10,10).
        // We create 1000 polygons:
        //   - 990 are outside the tile (spread from x=20..200)
        //   - 10 are inside the tile (at x=1..9, y=1..9)
        let tile_bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let mut polygons = Vec::with_capacity(1000);

        // 10 polygons inside the tile
        for i in 0..10 {
            let x = 1.0 + (i as f64) * 0.8;
            let y = 1.0 + (i as f64) * 0.8;
            polygons.push(Polygon::new(
                LineString::from(vec![
                    Coord { x, y },
                    Coord { x: x + 0.5, y },
                    Coord {
                        x: x + 0.5,
                        y: y + 0.5,
                    },
                    Coord { x, y: y + 0.5 },
                    Coord { x, y },
                ]),
                vec![],
            ));
        }

        // 990 polygons outside the tile (far away, scattered in x=20..200)
        for i in 0..990 {
            let x = 20.0 + (i as f64) * 0.18;
            let y = -80.0 + (i as f64) * 0.16;
            polygons.push(Polygon::new(
                LineString::from(vec![
                    Coord { x, y },
                    Coord { x: x + 0.1, y },
                    Coord {
                        x: x + 0.1,
                        y: y + 0.1,
                    },
                    Coord { x, y: y + 0.1 },
                    Coord { x, y },
                ]),
                vec![],
            ));
        }

        let mp = MultiPolygon::new(polygons);

        // Clip to the tile
        let result = clip_multipolygon(
            &mp,
            &tile_bounds,
            false,
            false,
            &crate::diagnostics::Diagnostics::default(),
        );

        // Should produce output (the 10 inside polygons)
        assert!(
            result.is_some(),
            "Should produce output for the intersecting polygons"
        );

        let clipped_mp = result.unwrap();
        // Should have approximately 10 polygons (the ones inside the tile)
        // Exact count may vary slightly due to clipping artifacts
        assert!(
            clipped_mp.0.len() >= 8 && clipped_mp.0.len() <= 12,
            "Expected ~10 output polygons, got {}",
            clipped_mp.0.len()
        );

        // All output coordinates should be within tile bounds
        for poly in &clipped_mp.0 {
            let bbox = poly.bounding_rect().unwrap();
            assert!(
                bbox.min().x >= 0.0 - 0.01 && bbox.max().x <= 10.0 + 0.01,
                "Output polygon x outside tile bounds: {:?}",
                bbox
            );
            assert!(
                bbox.min().y >= 0.0 - 0.01 && bbox.max().y <= 10.0 + 0.01,
                "Output polygon y outside tile bounds: {:?}",
                bbox
            );
        }
    }

    #[test]
    fn test_multipolygon_bbox_prefilter_all_outside() {
        // All polygons are outside the tile -- should return None quickly
        let tile_bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        let polygons: Vec<Polygon<f64>> = (0..500)
            .map(|i| {
                let x = 50.0 + (i as f64) * 0.2;
                let y = 50.0 + (i as f64) * 0.1;
                Polygon::new(
                    LineString::from(vec![
                        Coord { x, y },
                        Coord { x: x + 0.1, y },
                        Coord {
                            x: x + 0.1,
                            y: y + 0.1,
                        },
                        Coord { x, y: y + 0.1 },
                        Coord { x, y },
                    ]),
                    vec![],
                )
            })
            .collect();

        let mp = MultiPolygon::new(polygons);
        let result = clip_multipolygon(
            &mp,
            &tile_bounds,
            false,
            false,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(
            result.is_none(),
            "All-outside multipolygon should return None"
        );
    }

    #[test]
    fn test_bbox_prefilter_large_polygon_preclip() {
        // A single large polygon spanning a huge area (-180 to +180 longitude)
        // is clipped to a small 10-degree tile. The pre-clip optimization should
        // reduce the coordinate count before sending to the expensive i_overlay clipper.
        let tile_bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        // Build a large polygon with many coordinates spanning the entire globe.
        // This simulates a complex coastline polygon.
        let mut coords: Vec<Coord<f64>> = Vec::new();
        // Bottom edge: many points from -180 to +180
        for i in 0..360 {
            let x = -180.0 + i as f64;
            let y = -60.0 + (i as f64 * 0.1).sin() * 2.0; // Wavy bottom edge
            coords.push(Coord { x, y });
        }
        // Top edge: many points from +180 back to -180
        for i in (0..360).rev() {
            let x = -180.0 + i as f64;
            let y = 60.0 + (i as f64 * 0.1).cos() * 2.0; // Wavy top edge
            coords.push(Coord { x, y });
        }
        // Close the polygon
        coords.push(coords[0]);

        let large_poly = Polygon::new(LineString::from(coords.clone()), vec![]);

        // Total input coordinates
        let total_input_coords = coords.len();
        assert!(
            total_input_coords > 700,
            "Test polygon should have many coordinates, got {}",
            total_input_coords
        );

        // Clip to small tile
        let result = clip_polygon(
            &large_poly,
            &tile_bounds,
            false,
            false,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(result.is_some(), "Large polygon should intersect the tile");

        // Verify the clipped result is reasonable
        match result.unwrap() {
            Geometry::Polygon(p) => {
                let output_coords = p.exterior().coords().count();
                // The clipped polygon should have far fewer coordinates than input
                assert!(
                    output_coords < total_input_coords / 2,
                    "Clipped polygon should have fewer coords than input: {} vs {}",
                    output_coords,
                    total_input_coords
                );
            }
            Geometry::MultiPolygon(mp) => {
                let total_output: usize = mp.0.iter().map(|p| p.exterior().coords().count()).sum();
                assert!(
                    total_output < total_input_coords / 2,
                    "Clipped multipolygon should have fewer coords than input: {} vs {}",
                    total_output,
                    total_input_coords
                );
            }
            other => panic!("Expected Polygon or MultiPolygon, got {:?}", other),
        }
    }

    // ========== Sutherland-Hodgman Clipping Unit Tests ==========

    // ---- antimeridian-crossing geometries (issue #188 behavior pins) --------
    //
    // Geometries are stored verbatim, so a polygon whose vertices sit at
    // lng ±179.9 is, in coordinate space, a near-world-wide rectangle passing
    // through lng 0 — NOT two slivers at the antimeridian. These tests PIN
    // what export-time clipping does with such a geometry: every tile in the
    // world row intersects it ("smearing"). Documenting current behavior,
    // not desired behavior. See `context/ANTIMERIDIAN.md`.

    /// A polygon with vertices at lng ±179.9 — intended by the data author as
    /// a 0.2°-wide feature crossing the antimeridian, but stored (verbatim)
    /// as a 359.8°-wide rectangle.
    #[cfg(test)]
    fn antimeridian_polygon() -> Geometry<f64> {
        Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                Coord { x: -179.9, y: -0.1 },
                Coord { x: 179.9, y: -0.1 },
                Coord { x: 179.9, y: 0.1 },
                Coord { x: -179.9, y: 0.1 },
                Coord { x: -179.9, y: -0.1 },
            ]),
            vec![],
        ))
    }

    #[test]
    fn antimeridian_polygon_smears_into_prime_meridian_tile() {
        // A tile at lng ≈ 0 is ~180° from either "true" half of the feature,
        // yet clipping yields content there because the stored rectangle
        // passes straight through it.
        let tile = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
        let clipped = clip_geometry(
            &antimeridian_polygon(),
            &tile,
            0.0,
            &crate::diagnostics::Diagnostics::default(),
        );
        let clipped = clipped.expect(
            "PIN: prime-meridian tile receives geometry from an \
             antimeridian-crossing polygon (smearing)",
        );
        // The smear fills the tile's full x-range.
        let rect = clipped.bounding_rect().unwrap();
        assert!(
            (rect.min().x - (-1.0)).abs() < 1e-9 && (rect.max().x - 1.0).abs() < 1e-9,
            "PIN: smear spans the entire tile width, got {rect:?}"
        );
    }

    #[test]
    fn antimeridian_polygon_clips_at_edge_tile() {
        // A tile adjacent to +180° also intersects — the geometry is present
        // where the author intended it, in addition to the world-row smear.
        let tile = TileBounds::new(178.0, -1.0, 180.0, 1.0);
        let clipped = clip_geometry(
            &antimeridian_polygon(),
            &tile,
            0.0,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(
            clipped.is_some(),
            "tile at the +180° edge intersects the stored rectangle"
        );
    }

    #[test]
    fn test_sutherland_hodgman_fully_inside() {
        // Polygon fully inside clip bounds -- should be unchanged
        use crate::sutherland_hodgman::clip_polygon_sh;

        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: 2.0, y: 2.0 },
                Coord { x: 8.0, y: 2.0 },
                Coord { x: 8.0, y: 8.0 },
                Coord { x: 2.0, y: 8.0 },
                Coord { x: 2.0, y: 2.0 },
            ]),
            vec![],
        );

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some(), "Fully inside polygon should be preserved");
        match result.unwrap() {
            Geometry::Polygon(p) => {
                // Should preserve all 4 vertices + closing
                assert_eq!(
                    p.exterior().0.len(),
                    5,
                    "Should have 5 coords (4 vertices + close)"
                );
            }
            other => panic!("Expected Polygon, got {:?}", other),
        }
    }

    #[test]
    fn test_sutherland_hodgman_fully_outside() {
        // Polygon fully outside clip bounds -- should return None
        use crate::sutherland_hodgman::clip_polygon_sh;

        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: 20.0, y: 20.0 },
                Coord { x: 30.0, y: 20.0 },
                Coord { x: 30.0, y: 30.0 },
                Coord { x: 20.0, y: 30.0 },
                Coord { x: 20.0, y: 20.0 },
            ]),
            vec![],
        );

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_none(), "Fully outside polygon should be empty");
    }

    #[test]
    fn test_sutherland_hodgman_partial_clip() {
        // Polygon overlapping the right edge of the bounds
        use crate::sutherland_hodgman::clip_polygon_sh;

        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let poly = Polygon::new(
            LineString::from(vec![
                Coord { x: 5.0, y: 2.0 },
                Coord { x: 15.0, y: 2.0 },
                Coord { x: 15.0, y: 8.0 },
                Coord { x: 5.0, y: 8.0 },
                Coord { x: 5.0, y: 2.0 },
            ]),
            vec![],
        );

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(
            result.is_some(),
            "Partially overlapping polygon should produce output"
        );
        // Verify all result coords are within bounds
        match result.unwrap() {
            Geometry::Polygon(p) => {
                for coord in p.exterior().coords() {
                    assert!(
                        coord.x >= 0.0 - 0.001 && coord.x <= 10.0 + 0.001,
                        "x out of bounds: {}",
                        coord.x
                    );
                    assert!(
                        coord.y >= 0.0 - 0.001 && coord.y <= 10.0 + 0.001,
                        "y out of bounds: {}",
                        coord.y
                    );
                }
            }
            other => panic!("Expected Polygon, got {:?}", other),
        }
    }

    #[test]
    fn test_sutherland_hodgman_large_polygon_reduction() {
        // A polygon with many coordinates spanning a large area, clipped to a
        // small box. Tests that Sutherland-Hodgman reduces coordinate count.
        use crate::sutherland_hodgman::clip_polygon_sh;

        let bounds = TileBounds::new(0.0, 0.0, 10.0, 10.0);

        // Create a polygon with 720 coords spanning -180 to +180
        let mut coords = Vec::new();
        for i in 0..360 {
            coords.push(Coord {
                x: -180.0 + i as f64,
                y: -50.0,
            });
        }
        for i in (0..360).rev() {
            coords.push(Coord {
                x: -180.0 + i as f64,
                y: 50.0,
            });
        }
        coords.push(coords[0]); // close

        let input_count = coords.len();
        let poly = Polygon::new(LineString::from(coords), vec![]);

        let result = clip_polygon_sh(&poly, &bounds);
        assert!(result.is_some(), "Clipped polygon should not be empty");

        match result.unwrap() {
            Geometry::Polygon(p) => {
                let output_count = p.exterior().0.len();
                assert!(
                    output_count < input_count / 10,
                    "Sutherland-Hodgman should dramatically reduce coordinates: {} -> {}",
                    input_count,
                    output_count
                );
            }
            other => panic!("Expected Polygon, got {:?}", other),
        }
    }

    #[test]
    fn test_clip_polygon_u_shape() {
        // U-shaped polygon clipped by a horizontal band.
        //
        // With i_overlay, clipping correctly produces two separate polygons
        // (the two arms of the U) as a MultiPolygon. This is the geometrically
        // correct result and avoids the self-touching polygon that S-H produces.
        let bounds = TileBounds::new(0.0, 4.0, 10.0, 6.0); // Horizontal band

        // U-shape: two vertical bars connected at the bottom
        let u_shape = Polygon::new(
            LineString::from(vec![
                Coord { x: 1.0, y: 0.0 },
                Coord { x: 2.0, y: 0.0 },
                Coord { x: 2.0, y: 10.0 },
                Coord { x: 1.0, y: 10.0 },
                Coord { x: 1.0, y: 2.0 },
                Coord { x: 8.0, y: 2.0 },
                Coord { x: 8.0, y: 10.0 },
                Coord { x: 9.0, y: 10.0 },
                Coord { x: 9.0, y: 0.0 },
                Coord { x: 1.0, y: 0.0 },
            ]),
            vec![],
        );

        let result = clip_polygon(
            &u_shape,
            &bounds,
            false,
            false,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(result.is_some(), "U-shape should intersect the band");

        // i_overlay correctly produces a MultiPolygon with 2 separate polygons
        match result.unwrap() {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(
                    mp.0.len(),
                    2,
                    "U-shape clipped should produce 2 separate polygons"
                );
                // Verify all coords within bounds for each polygon
                for p in mp.0.iter() {
                    for coord in p.exterior().coords() {
                        assert!(
                            coord.x >= 0.0 && coord.x <= 10.0,
                            "x={} out of bounds",
                            coord.x
                        );
                        assert!(
                            coord.y >= 4.0 - 1e-10 && coord.y <= 6.0 + 1e-10,
                            "y={} out of bounds",
                            coord.y
                        );
                    }
                }
            }
            Geometry::Polygon(p) => {
                // Also acceptable if i_overlay produces single valid polygon
                for coord in p.exterior().coords() {
                    assert!(
                        coord.x >= 0.0 && coord.x <= 10.0,
                        "x={} out of bounds",
                        coord.x
                    );
                    assert!(
                        coord.y >= 4.0 - 1e-10 && coord.y <= 6.0 + 1e-10,
                        "y={} out of bounds",
                        coord.y
                    );
                }
            }
            other => panic!("Expected Polygon or MultiPolygon, got {:?}", other),
        }
    }

    // ========== Simple-clip fast path (issue #239) ==========
    //
    // The `simple_clip_fastpath` flag skips the i_overlay boundary-bridge
    // fallback for features whose rings are already simple. These tests pin the
    // behavior and the equivalence it relies on: on a simple concave polygon,
    // S-H's self-touching output is area- and fill-identical to i_overlay's
    // split under nonzero winding, so skipping the fallback changes nothing that
    // renders. The concave "U" cut across its mouth is the case #94's fallback
    // was built for; the flag deliberately keeps S-H there.

    /// A genuinely SIMPLE concave U (opening upward), non-self-intersecting.
    ///
    /// NOTE: the existing `test_clip_polygon_u_shape` fixture is *self-
    /// intersecting* (its inner edge at y=2 passes through the arm verticals at
    /// (2,2)/(8,2)), so `geometry_is_simple` is false for it and the self-
    /// intersection check — not the boundary gate — routes it to `i_overlay`. To
    /// isolate the boundary gate we need a clean simple U: solid base y∈[0,3]
    /// across x∈[0,10], arms x∈[0,3] and x∈[7,10] up to y=10, notch x∈[3,7].
    #[cfg(test)]
    fn u_shape() -> Polygon<f64> {
        Polygon::new(
            LineString::from(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: 10.0, y: 0.0 },
                Coord { x: 10.0, y: 10.0 },
                Coord { x: 7.0, y: 10.0 },
                Coord { x: 7.0, y: 3.0 },
                Coord { x: 3.0, y: 3.0 },
                Coord { x: 3.0, y: 10.0 },
                Coord { x: 0.0, y: 10.0 },
                Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        )
    }

    /// The flag's behavioral contract on a simple concave ring: OFF splits into
    /// clean parts (the #94 fallback), ON keeps S-H's single self-touching ring.
    /// Both are correct; the equivalence is proven by the two tests below.
    #[test]
    fn simple_u_fastpath_keeps_single_ring() {
        let u = u_shape();
        let bounds = TileBounds::new(0.0, 4.0, 10.0, 6.0);

        // The U-shape passes the simplicity check → `assume_simple` is true for
        // it in the real export path, so the flag applies to it.
        assert!(
            geometry_is_simple(&Geometry::Polygon(u.clone())),
            "U-shape is a simple polygon; assume_simple is true in prod"
        );

        // Flag OFF (default): the boundary gate fires even at assume_simple=true,
        // so the fallback yields the clean 2-part split.
        match clip_polygon(
            &u,
            &bounds,
            true,
            false,
            &crate::diagnostics::Diagnostics::default(),
        )
        .unwrap()
        {
            Geometry::MultiPolygon(mp) => assert_eq!(mp.0.len(), 2),
            other => panic!("flag off should split; got {other:?}"),
        }

        // Flag ON (#239): the boundary gate is skipped for this simple ring, so
        // S-H's single self-touching polygon is kept (no i_overlay).
        match clip_polygon(
            &u,
            &bounds,
            true,
            true,
            &crate::diagnostics::Diagnostics::default(),
        )
        .unwrap()
        {
            Geometry::Polygon(_) => {}
            other => panic!("flag on should keep the S-H single polygon; got {other:?}"),
        }
    }

    /// The equivalence the flag relies on: on a simple U, S-H's kept ring has the
    /// same enclosed AREA as `i_overlay`'s split and leaves the mouth EMPTY under
    /// nonzero winding — so it renders identically despite being self-touching.
    #[test]
    fn fastpath_u_render_equivalent() {
        use geo::{Area, Contains};
        let u = u_shape();
        let bounds = TileBounds::new(0.0, 4.0, 10.0, 6.0);

        let sh = sutherland_hodgman::clip_polygon_sh(&u, &bounds).unwrap();
        let io = ioverlay_clip::clip_polygon_ioverlay(
            &u,
            &bounds,
            &crate::diagnostics::Diagnostics::default(),
        )
        .unwrap();

        // Notch center: inside the band, inside the mouth → NOT part of the U.
        let notch = Point::new(5.0, 5.0);
        assert!(!io.contains(&notch), "i_overlay: mouth empty");
        assert!(
            !sh.contains(&notch),
            "S-H: mouth also empty (winding cancels)"
        );
        assert!(
            (sh.unsigned_area() - io.unsigned_area()).abs() < 1e-9,
            "areas must match: sh={} io={}",
            sh.unsigned_area(),
            io.unsigned_area()
        );
    }

    /// Equivalence generalizes to a MULTI-mouth shape: a 3-tooth comb has two
    /// notches, and if S-H's bridge windings failed to cancel, area or fill would
    /// diverge here. They don't.
    #[test]
    fn fastpath_comb_render_equivalent() {
        use geo::{Area, Contains};
        // Base y∈[0,3] across x∈[0,15]; arms at x∈[0,3],[6,9],[12,15] up to y=10;
        // notches at x∈[3,6] and x∈[9,12].
        let comb = Polygon::new(
            LineString::from(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: 15.0, y: 0.0 },
                Coord { x: 15.0, y: 10.0 },
                Coord { x: 12.0, y: 10.0 },
                Coord { x: 12.0, y: 3.0 },
                Coord { x: 9.0, y: 3.0 },
                Coord { x: 9.0, y: 10.0 },
                Coord { x: 6.0, y: 10.0 },
                Coord { x: 6.0, y: 3.0 },
                Coord { x: 3.0, y: 3.0 },
                Coord { x: 3.0, y: 10.0 },
                Coord { x: 0.0, y: 10.0 },
                Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        );
        let bounds = TileBounds::new(0.0, 4.0, 15.0, 6.0);
        assert!(
            geometry_is_simple(&Geometry::Polygon(comb.clone())),
            "comb must be simple to be an assume_simple case"
        );

        let sh = sutherland_hodgman::clip_polygon_sh(&comb, &bounds).unwrap();
        let io = ioverlay_clip::clip_polygon_ioverlay(
            &comb,
            &bounds,
            &crate::diagnostics::Diagnostics::default(),
        )
        .unwrap();
        let notch1 = Point::new(4.5, 5.0);
        let notch2 = Point::new(10.5, 5.0);
        assert!(
            (sh.unsigned_area() - io.unsigned_area()).abs() < 1e-9,
            "areas must match across multiple mouths"
        );
        assert!(
            !sh.contains(&notch1) && !sh.contains(&notch2),
            "both mouths stay empty under S-H"
        );
    }
}
