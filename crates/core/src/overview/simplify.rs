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

/// Disposition of a polygon / multipolygon that collapses below the level
/// tolerance (visibility gate, exterior collapse, or sub-tolerance area).
///
/// Three dispositions (#279):
/// - [`Drop`](CollapseMode::Drop) (default): the feature is omitted.
/// - [`Point`](CollapseMode::Point) (`--collapse`, spec Q4 opt-in): replaced
///   by a representative [`Point`]. Changes the geometry type.
/// - [`Square`](CollapseMode::Square) (`--collapse-square`): replaced by a
///   ~1×tolerance placeholder **square** anchored at the representative
///   point, area-dithered so aggregate area stays truthful (tippecanoe's
///   tiny-polygon reduction). Type-preserving — `geometry_types` stays
///   `["Polygon"]`, so fill-styled renderers need no style changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CollapseMode {
    /// Omit the collapsed feature (default).
    #[default]
    Drop,
    /// Replace with a representative point (centroid; spec Q4 opt-in).
    Point,
    /// Replace with an area-dithered ~1×tolerance placeholder square
    /// (tippecanoe tiny-polygon reduction, #279).
    Square,
}

/// Options controlling per-level simplification.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimplifyOptions {
    /// Tolerance multiplier: `tolerance = factor * gsd`. See
    /// [`DEFAULT_SIMPLIFY_FACTOR`].
    pub factor: f64,
    /// Disposition of polygons that collapse below the visibility gate
    /// (default [`CollapseMode::Drop`]; see [`CollapseMode`]).
    pub collapse: CollapseMode,
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
            collapse: CollapseMode::Drop,
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
            // Per-part disposition: a collapsed *part* is dropped (never
            // turned into a Point — a MultiPolygon cannot hold one; whole-
            // feature Point collapse is decided after), EXCEPT under
            // `CollapseMode::Square` (#279), where each collapsed part is
            // area-dithered into its own placeholder square — matching
            // tippecanoe, which reduces tiny polygons ring-by-ring, so a
            // dense multipolygon block keeps per-part density instead of
            // collapsing to a single square. A repaired part (cascade path)
            // may itself be a MultiPolygon; its parts are flattened in.
            let part_mode = match opts.collapse {
                CollapseMode::Square => CollapseMode::Square,
                _ => CollapseMode::Drop,
            };
            let kept: Vec<Polygon<f64>> =
                mp.0.iter()
                    .flat_map(
                        |p| match simplify_polygon_impl(p, tol, part_mode, opts.cascade) {
                            Simplified::Keep(Geometry::Polygon(poly)) => vec![poly],
                            Simplified::Keep(Geometry::MultiPolygon(parts)) => parts.0,
                            _ => Vec::new(),
                        },
                    )
                    .collect();
            if !kept.is_empty() {
                Simplified::Keep(Geometry::MultiPolygon(MultiPolygon::new(kept)))
            } else {
                match opts.collapse {
                    // Every part had its own dither under Square; nothing more.
                    CollapseMode::Drop | CollapseMode::Square => Simplified::Dropped,
                    CollapseMode::Point => match mp.centroid() {
                        Some(pt) => Simplified::Keep(Geometry::Point(pt)),
                        None => Simplified::Dropped,
                    },
                }
            }
        }

        // GeometryCollection / Line / Rect / Triangle: out of scope for v0.1;
        // pass through untouched.
        other => Simplified::Keep(other.clone()),
    }
}

/// Per-level feature representation (zoom-band representation selector,
/// #317 / #279).
///
/// Kept an enum (never a boolean) in options, contexts, and cascade steps so
/// further dispositions slot in without another plumbing change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Representation {
    /// Full (simplified) geometry — the normal path. Below-tolerance
    /// polygons follow the global [`CollapseMode`].
    #[default]
    Geometry,
    /// Polygonal features are replaced by their representative point
    /// (centroid; see [`simplify_step`]) — unconditionally, whatever their
    /// size. Lines and points are unaffected.
    Point,
    /// Normal simplification, but below-tolerance polygons emit an
    /// area-dithered ~1×GSD placeholder square instead of dropping
    /// ([`CollapseMode::Square`], tippecanoe tiny-polygon reduction, #279).
    /// Type-preserving; above-tolerance polygons are unaffected.
    Square,
    /// **Row-collapsing** H3 aggregate (#332): at this level every feature is
    /// binned into the H3 cell covering its representative point and each
    /// occupied cell becomes one row (cell boundary polygon + feature `count`).
    /// Unlike the other variants this changes row cardinality, so H3 levels are
    /// produced by [`super::h3agg`] rather than the per-feature simplify path —
    /// [`simplify_step`] never runs on an H3 level. The band's H3 resolution is
    /// auto-derived from the level GSD ([`super::level::h3_res_for_gsd`]); all
    /// geometry kinds participate.
    H3,
}

impl Representation {
    /// The spec / CLI keyword for this representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Representation::Geometry => "geom",
            Representation::Point => "point",
            Representation::Square => "square",
            Representation::H3 => "h3",
        }
    }
}

/// One step of a cascading fine→coarse simplification chain (#218, #317).
///
/// A step is a level's GSD plus its [`Representation`]: a
/// [`Representation::Point`] step marks a zoom-band point level (#317) at
/// which polygonal features are replaced by their representative point
/// instead of being simplified.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CascadeStep {
    /// The step level's GSD in meters.
    pub gsd_meters: f64,
    /// The step level's feature representation (#317).
    pub repr: Representation,
}

impl CascadeStep {
    /// A normal (full-geometry) cascade step.
    pub fn geom(gsd_meters: f64) -> Self {
        Self {
            gsd_meters,
            repr: Representation::Geometry,
        }
    }

    /// A zoom-band point-representation step (#317).
    pub fn point(gsd_meters: f64) -> Self {
        Self {
            gsd_meters,
            repr: Representation::Point,
        }
    }

    /// A zoom-band placeholder-square step (#279).
    pub fn square(gsd_meters: f64) -> Self {
        Self {
            gsd_meters,
            repr: Representation::Square,
        }
    }
}

/// Representative point for a *polygonal* geometry (zoom-band point
/// representation, #317). Returns `None` for non-polygonal input — points
/// pass through and lines keep their normal simplification path, so the
/// caller falls back to [`simplify_for_level`].
///
/// Point flavor: **centroid**, falling back to the bbox center and then the
/// first vertex for degenerate (zero-area) rings — the same flavor as the
/// `--collapse` path ([`collapse_polygon`]; `assign.rs` notes centroid is the
/// closest to cartographic convention). Unlike `--collapse` the fallback
/// chain is applied to MultiPolygons too: a point-band level must never
/// silently lose a feature to a degenerate centroid. `Dropped` only when the
/// geometry has no coordinates at all.
///
/// DIVERGENCE FROM TIPPECANOE: tippecanoe's
/// `--convert-polygons-to-label-points` emits one label point *per
/// intersecting tile* and applies at every zoom; per-zoom representation
/// switching there requires building two tilesets and merging with
/// `tile-join`. Overview levels are tile-free (a level is a parquet row
/// band), so a per-tile point is not representable here — we emit one
/// deterministic per-feature centroid, and the zoom band replaces the
/// two-archive merge. Planetiler exposes centroid / point-on-surface /
/// innermost-point per layer; centroid is its cheapest default and matches
/// our existing collapse flavor.
fn polygonal_representative_point(geom: &Geometry<f64>) -> Option<Simplified> {
    match geom {
        Geometry::Polygon(poly) => Some(collapse_polygon(poly, CollapseMode::Point, 0.0)),
        Geometry::MultiPolygon(mp) => {
            let pt = mp
                .centroid()
                .or_else(|| mp.bounding_rect().map(|r| r.center().into()))
                .or_else(|| {
                    mp.0.first()
                        .and_then(|p| p.exterior().0.first())
                        .map(|c| Point::new(c.x, c.y))
                });
            Some(match pt {
                Some(p) => Simplified::Keep(Geometry::Point(p)),
                None => Simplified::Dropped,
            })
        }
        _ => None,
    }
}

/// Simplify one feature's geometry for a level, honoring the level's
/// [`Representation`] (#317).
///
/// [`Representation::Geometry`] is exactly [`simplify_for_level`]. On a
/// [`Representation::Point`] level (a zoom-band point level), polygonal
/// geometry is replaced by its representative point
/// ([`polygonal_representative_point`]) — unconditionally, with no
/// visibility gating (a dot is always visible) — while points pass through
/// and lines keep the normal simplification path. On a
/// [`Representation::Square`] level (#279), simplification is normal but
/// below-tolerance polygons emit area-dithered placeholder squares
/// ([`squarify_polygon`]) instead of following the global [`CollapseMode`].
pub fn simplify_step(
    geom: &Geometry<f64>,
    gsd_meters: f64,
    crs: Crs,
    opts: &SimplifyOptions,
    repr: Representation,
) -> Simplified {
    match repr {
        Representation::Geometry => simplify_for_level(geom, gsd_meters, crs, opts),
        Representation::Point => {
            if let Some(out) = polygonal_representative_point(geom) {
                return out;
            }
            simplify_for_level(geom, gsd_meters, crs, opts)
        }
        // Square (#279): normal simplification with the below-tolerance
        // disposition forced to area-dithered placeholder squares at this
        // level — above-tolerance polygons are unaffected (type-preserving).
        Representation::Square => {
            let opts = SimplifyOptions {
                collapse: CollapseMode::Square,
                ..*opts
            };
            simplify_for_level(geom, gsd_meters, crs, &opts)
        }
        // H3 (#332) levels are row-collapsing: the convert driver discards the
        // per-feature geometry and replaces the whole level with H3 cell
        // aggregates (`h3agg`). The per-level geometry pass still runs over
        // every level's features generically before that replacement, so this
        // arm is reached with output that is thrown away — simplify plainly and
        // cheaply; the result is never written.
        Representation::H3 => simplify_for_level(geom, gsd_meters, crs, opts),
    }
}

/// Cascading simplification (#218): fold a geometry through a fine→coarse
/// chain of level steps, feeding each coarser level the previous level's
/// already-simplified output instead of re-simplifying canonical geometry.
///
/// `steps_fine_to_coarse` lists every non-canonical level from the finest
/// (first) down to the target level (last), each with its GSD and
/// representation ([`CascadeStep`]). The fold is a pure function of
/// `(geom, chain, crs, opts)` — independent of engine, batch boundaries, and
/// neighboring features — so every conversion path (in-memory, serial
/// streaming, pipelined) computes identical results for identical inputs.
///
/// Zoom-band representation (#317 / #279): at the first `Point` step (the
/// band's finest level) a polygonal feature collapses to its representative
/// point; coarser steps then pass the point through untouched, so every
/// point-band level shares the same point. `Square` steps dither the
/// below-tolerance survivors of the previous step into placeholder squares.
///
/// Along a pure-geometry chain, dropping is monotone: tolerances grow while
/// the working geometry's extent can only shrink (RDP keeps a vertex
/// subset), so a feature dropped at a fine [`Representation::Geometry`] step
/// stays dropped at every coarser geometry step. `Point` and `Square` steps,
/// however, **revive from canonical geometry**: a feature whose geometry
/// cascade died at a finer level is still a *member* of the coarser band
/// level, and the band's whole purpose is to represent exactly those
/// too-small-for-geometry features — so a `Point` step emits the canonical
/// geometry's representative point, and a `Square` step dithers the
/// canonical geometry's area. (Without this, cascading would silently empty
/// the band: a 20 m building drops at the first coarse geometry step long
/// before the fold reaches z0–7.) Revival stays deterministic and
/// engine-independent — it is a pure function of the canonical geometry and
/// the step.
///
/// An empty chain is the identity (bit-identical clone), matching
/// [`simplify_for_level`]'s canonical path at zero tolerance.
pub fn simplify_cascade(
    geom: &Geometry<f64>,
    steps_fine_to_coarse: &[CascadeStep],
    crs: Crs,
    opts: &SimplifyOptions,
) -> Simplified {
    let mut current: Option<Geometry<f64>> = None;
    let mut alive = true;
    let mut result = Simplified::Keep(geom.clone());
    for step in steps_fine_to_coarse {
        let out = if !alive && step.repr == Representation::Geometry {
            // Monotone along geometry steps: once dropped, stays dropped.
            Simplified::Dropped
        } else {
            // Alive: cascade the previous step's output. Not alive (Point /
            // Square step): revive from canonical geometry.
            let input = if alive {
                current.as_ref().unwrap_or(geom)
            } else {
                geom
            };
            simplify_step(input, step.gsd_meters, crs, opts, step.repr)
        };
        match &out {
            Simplified::Keep(g) => {
                current = Some(g.clone());
                alive = true;
            }
            Simplified::Dropped => alive = false,
        }
        result = out;
    }
    result
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
    mode: CollapseMode,
    repair: bool,
) -> Simplified {
    if polygon_diag(poly) < tol {
        return collapse_polygon(poly, mode, tol);
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
            return collapse_polygon(poly, mode, tol);
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
        return collapse_polygon(poly, mode, tol);
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

/// Representative point of a polygon: the centroid, falling back to the bbox
/// center, then the first vertex, for degenerate (zero-area) rings whose
/// centroid is undefined. `None` only when the ring has no coordinates.
fn polygon_anchor(poly: &Polygon<f64>) -> Option<Point<f64>> {
    poly.centroid()
        .or_else(|| poly.bounding_rect().map(|r| r.center().into()))
        .or_else(|| poly.exterior().0.first().map(|c| Point::new(c.x, c.y)))
}

/// Deterministic per-feature dither value, uniform in `[0, 1)`, keyed on the
/// anchor point's coordinate bit patterns (splitmix64 finalizer).
///
/// # Why a per-feature hash instead of tippecanoe's accumulator
///
/// DIVERGENCE FROM TIPPECANOE: tippecanoe's tiny-polygon reduction
/// (clip.cpp, `tiny_polygon` handling; the legacy per-tile pipeline's #85
/// port did the same) walks the features of a tile **serially**,
/// accumulating sub-threshold area and emitting a placeholder square each
/// time the accumulator crosses the threshold. That is exact but
/// order-dependent — unusable here, where three engines (in-memory, serial
/// streaming, pipelined) with different batch boundaries and rayon schedules
/// must produce byte-identical output, and levels have no tile scope to
/// accumulate over. We instead dither **per feature**: a polygon of area `a`
/// below the threshold `T = tol²` survives as a `tol × tol` square with
/// probability `a / T`, decided by this deterministic hash of its anchor
/// coordinates. Expected emitted area equals the true area (`(a/T)·T = a`),
/// so aggregate density stays truthful exactly like tippecanoe's
/// accumulator in expectation — dense blocks emit many squares, isolated
/// barns mostly none — while the decision is a pure function of the feature,
/// independent of engine, ordering, and parallelism. A kept square's anchor
/// is its own center, so re-dithering it at a coarser cascade step reuses
/// the same `u` against a smaller `a/T` — survival is monotone fine→coarse,
/// matching the cascade's drop monotonicity.
fn dither_u01(x: f64, y: f64) -> f64 {
    let mut z = x.to_bits() ^ y.to_bits().rotate_left(32);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Top 53 bits → exact f64 in [0, 1).
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// A `tol × tol` placeholder square (closed 5-coordinate ring, CCW) centered
/// at `anchor` — the #279 tiny-polygon placeholder.
fn placeholder_square(anchor: Point<f64>, tol: f64) -> Polygon<f64> {
    let h = tol * 0.5;
    let (cx, cy) = (anchor.x(), anchor.y());
    Polygon::new(
        LineString::new(vec![
            geo::Coord {
                x: cx - h,
                y: cy - h,
            },
            geo::Coord {
                x: cx + h,
                y: cy - h,
            },
            geo::Coord {
                x: cx + h,
                y: cy + h,
            },
            geo::Coord {
                x: cx - h,
                y: cy + h,
            },
            geo::Coord {
                x: cx - h,
                y: cy - h,
            },
        ]),
        vec![],
    )
}

/// Area-dithered placeholder square for a below-tolerance polygon (#279):
/// survives as a `tol × tol` square at the polygon's representative point
/// with probability `min(1, area / tol²)` (see [`dither_u01`]).
fn squarify_polygon(poly: &Polygon<f64>, tol: f64) -> Simplified {
    let Some(anchor) = polygon_anchor(poly) else {
        return Simplified::Dropped;
    };
    let threshold = tol * tol;
    let p = if threshold > 0.0 {
        (poly.unsigned_area() / threshold).min(1.0)
    } else {
        0.0
    };
    if dither_u01(anchor.x(), anchor.y()) < p {
        Simplified::Keep(Geometry::Polygon(placeholder_square(anchor, tol)))
    } else {
        Simplified::Dropped
    }
}

/// Resolve a collapsed polygon per the [`CollapseMode`] (#279): drop by
/// default, a representative [`Point`] at the polygon centroid (spec Q4,
/// `--collapse`), or an area-dithered placeholder square
/// (`--collapse-square`, [`squarify_polygon`]).
fn collapse_polygon(poly: &Polygon<f64>, mode: CollapseMode, tol: f64) -> Simplified {
    match mode {
        CollapseMode::Drop => Simplified::Dropped,
        CollapseMode::Point => match polygon_anchor(poly) {
            Some(p) => Simplified::Keep(Geometry::Point(p)),
            None => Simplified::Dropped,
        },
        CollapseMode::Square => squarify_polygon(poly, tol),
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
            collapse: CollapseMode::Point,
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

    /// Full-geometry cascade steps from plain GSDs (test shorthand).
    fn geom_steps(gsds: &[f64]) -> Vec<CascadeStep> {
        gsds.iter().map(|&g| CascadeStep::geom(g)).collect()
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
            simplify_cascade(&g, &geom_steps(&[100.0]), Crs::Epsg3857, &opts),
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
        match simplify_cascade(&g, &geom_steps(&[25.0, 50.0, 100.0]), Crs::Epsg3857, &opts) {
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
            simplify_cascade(&tiny, &geom_steps(&[1000.0, 5000.0]), Crs::Epsg3857, &opts),
            Simplified::Dropped
        );
    }

    // ---- zoom-band point representation (#317) ------------------------------

    #[test]
    fn test_simplify_step_point_repr_polygon_to_centroid() {
        let opts = SimplifyOptions::default();
        // A LARGE polygon (well above every gate) still becomes a point on a
        // point-representation step — the band is unconditional, not tied to
        // the visibility gate like --collapse.
        let poly = Geometry::Polygon(square(1000.0, 2000.0, 5000.0));
        match simplify_step(&poly, 100.0, Crs::Epsg3857, &opts, Representation::Point) {
            Simplified::Keep(Geometry::Point(pt)) => {
                assert!((pt.x() - 1000.0).abs() < 1e-9);
                assert!((pt.y() - 2000.0).abs() < 1e-9);
            }
            other => panic!("expected Keep(Point), got {other:?}"),
        }
        // point_repr = false is exactly simplify_for_level.
        assert_eq!(
            simplify_step(&poly, 100.0, Crs::Epsg3857, &opts, Representation::Geometry),
            simplify_for_level(&poly, 100.0, Crs::Epsg3857, &opts)
        );
    }

    #[test]
    fn test_simplify_step_point_repr_multipolygon_to_centroid() {
        let opts = SimplifyOptions::default();
        let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![
            square(0.0, 0.0, 1000.0),
            square(4000.0, 0.0, 1000.0),
        ]));
        match simplify_step(&mp, 100.0, Crs::Epsg3857, &opts, Representation::Point) {
            Simplified::Keep(Geometry::Point(pt)) => {
                assert!((pt.x() - 2000.0).abs() < 1e-9, "centroid x, got {}", pt.x());
                assert!(pt.y().abs() < 1e-9);
            }
            other => panic!("expected Keep(Point), got {other:?}"),
        }
    }

    #[test]
    fn test_simplify_step_point_repr_sub_gate_polygon_kept_as_point() {
        // A polygon far below the visibility gate is DROPPED by normal
        // simplification but KEPT as a point on a point step: a dot is
        // always visible.
        let opts = SimplifyOptions::default();
        let tiny = Geometry::Polygon(square(50.0, 50.0, 10.0));
        assert_eq!(
            simplify_for_level(&tiny, 5000.0, Crs::Epsg3857, &opts),
            Simplified::Dropped
        );
        assert!(matches!(
            simplify_step(&tiny, 5000.0, Crs::Epsg3857, &opts, Representation::Point),
            Simplified::Keep(Geometry::Point(_))
        ));
    }

    #[test]
    fn test_simplify_step_point_repr_lines_and_points_unaffected() {
        let opts = SimplifyOptions::default();
        // Lines keep the normal simplification path (the band selects the
        // POLYGON representation only).
        let line = Geometry::LineString(wiggly_line(200, 50.0));
        assert_eq!(
            simplify_step(&line, 100.0, Crs::Epsg3857, &opts, Representation::Point),
            simplify_for_level(&line, 100.0, Crs::Epsg3857, &opts)
        );
        // Points pass through untouched.
        let p = Geometry::Point(Point::new(3.0, 4.0));
        assert_eq!(
            simplify_step(&p, 5000.0, Crs::Epsg3857, &opts, Representation::Point),
            Simplified::Keep(p.clone())
        );
    }

    #[test]
    fn test_cascade_point_band_shares_one_point_across_coarser_steps() {
        let opts = SimplifyOptions::default();
        // Chain fine→coarse: two geometry steps, then the band boundary
        // (point step), then a coarser point step. The polygon collapses at
        // the boundary and the SAME point flows through the coarser step.
        let poly = Geometry::Polygon(square(500.0, -300.0, 5000.0));
        let steps = [
            CascadeStep::geom(50.0),
            CascadeStep::geom(100.0),
            CascadeStep::point(200.0),
            CascadeStep::point(400.0),
        ];
        let at_boundary = simplify_cascade(&poly, &steps[..3], Crs::Epsg3857, &opts);
        let at_coarser = simplify_cascade(&poly, &steps, Crs::Epsg3857, &opts);
        match (&at_boundary, &at_coarser) {
            (Simplified::Keep(Geometry::Point(a)), Simplified::Keep(Geometry::Point(b))) => {
                assert_eq!(a, b, "band levels must share the boundary point");
            }
            other => panic!("expected two Keep(Point), got {other:?}"),
        }
    }

    /// THE cascade-revival anchor (#317): a polygon too small to survive the
    /// fine geometry steps must still appear at the coarse point-band step,
    /// derived from canonical geometry — otherwise cascading silently
    /// empties the band (the open-buildings failure mode).
    #[test]
    fn test_cascade_point_step_revives_dropped_geometry() {
        let opts = SimplifyOptions::default();
        // 20 m square at (500, 700): dropped by the 5 km geometry step.
        let poly = Geometry::Polygon(square(500.0, 700.0, 10.0));
        let steps = [
            CascadeStep::geom(5_000.0),
            CascadeStep::point(10_000.0),
            CascadeStep::point(20_000.0),
        ];
        assert_eq!(
            simplify_cascade(&poly, &steps[..1], Crs::Epsg3857, &opts),
            Simplified::Dropped,
            "precondition: geometry step drops the tiny polygon"
        );
        for chain in [&steps[..2], &steps[..3]] {
            match simplify_cascade(&poly, chain, Crs::Epsg3857, &opts) {
                Simplified::Keep(Geometry::Point(pt)) => {
                    assert!((pt.x() - 500.0).abs() < 1e-9);
                    assert!((pt.y() - 700.0).abs() < 1e-9);
                }
                other => panic!("point step must revive from canonical, got {other:?}"),
            }
        }
    }

    /// Same revival for square steps: the dither sees the canonical
    /// geometry's area even when the geometry cascade died earlier.
    #[test]
    fn test_cascade_square_step_revives_dropped_geometry() {
        let opts = SimplifyOptions::default();
        // Scan anchors for one the dither keeps (deterministic per anchor).
        let mut revived = 0;
        for i in 0..300 {
            let (cx, cy) = (i as f64 * 7_919.0, i as f64 * 3_571.0);
            let poly = Geometry::Polygon(square(cx, cy, 2_000.0));
            let steps = [CascadeStep::geom(5_000.0), CascadeStep::square(8_000.0)];
            assert_eq!(
                simplify_cascade(&poly, &steps[..1], Crs::Epsg3857, &opts),
                Simplified::Dropped,
                "precondition: geometry step drops it"
            );
            match simplify_cascade(&poly, &steps, Crs::Epsg3857, &opts) {
                Simplified::Keep(Geometry::Polygon(sq)) => {
                    let r = sq.bounding_rect().unwrap();
                    assert!((r.width() - 8_000.0).abs() < 1e-6, "side = step tol");
                    revived += 1;
                }
                Simplified::Dropped => {}
                other => panic!("expected Keep(Polygon) or Dropped, got {other:?}"),
            }
        }
        assert!(revived > 0, "some anchors must dither through");
        assert!(revived < 300, "not all (p = 16e6/64e6 = 0.25 per anchor)");
    }

    // ---- tiny-polygon placeholder squares (#279) ---------------------------

    /// Survivors of a Square-mode collapse must be `tol × tol` squares
    /// centered at the source polygon's centroid.
    #[test]
    fn test_square_collapse_emits_gsd_square_at_anchor() {
        let tol = 5000.0; // gsd 5000 m, factor 1.0, EPSG:3857
        let opts = SimplifyOptions {
            collapse: CollapseMode::Square,
            ..Default::default()
        };
        // Scan candidate anchors until the dither keeps one (deterministic
        // per anchor, so this loop is stable run-to-run).
        let mut checked = 0;
        for i in 0..200 {
            let (cx, cy) = (1000.0 + i as f64 * 3137.0, -2000.0 + i as f64 * 911.0);
            // Area = (2·1000)² = 4e6; threshold = tol² = 2.5e7 → p ≈ 0.16.
            let poly = Geometry::Polygon(square(cx, cy, 1000.0));
            match simplify_for_level(&poly, tol, Crs::Epsg3857, &opts) {
                Simplified::Keep(Geometry::Polygon(sq)) => {
                    let ring = &sq.exterior().0;
                    assert_eq!(ring.len(), 5, "closed 5-coordinate square ring");
                    let r = sq.bounding_rect().unwrap();
                    assert!((r.width() - tol).abs() < 1e-6, "side = tol");
                    assert!((r.height() - tol).abs() < 1e-6, "side = tol");
                    let c = r.center();
                    assert!((c.x - cx).abs() < 1e-6 && (c.y - cy).abs() < 1e-6);
                    assert!((sq.unsigned_area() - tol * tol).abs() < 1e-3);
                    checked += 1;
                }
                Simplified::Dropped => {}
                other => panic!("expected Keep(Polygon) or Dropped, got {other:?}"),
            }
        }
        assert!(checked > 0, "at least one anchor must survive the dither");
        assert!(checked < 200, "not every anchor may survive (p ≈ 0.16)");
    }

    /// Deterministic: the same feature always gets the same dither decision.
    #[test]
    fn test_square_collapse_deterministic() {
        let opts = SimplifyOptions {
            collapse: CollapseMode::Square,
            ..Default::default()
        };
        for i in 0..50 {
            let poly = Geometry::Polygon(square(i as f64 * 731.0, i as f64 * 197.0, 500.0));
            let a = simplify_for_level(&poly, 5000.0, Crs::Epsg3857, &opts);
            let b = simplify_for_level(&poly, 5000.0, Crs::Epsg3857, &opts);
            assert_eq!(a, b);
        }
    }

    /// Expected emitted area equals true area: over many features of area
    /// `p·tol²`, about `p·N` survive, each contributing `tol²` — so total
    /// emitted area ≈ total true area (tippecanoe's accumulator invariant,
    /// in expectation).
    #[test]
    fn test_square_collapse_preserves_aggregate_area_statistically() {
        let tol = 5000.0;
        let opts = SimplifyOptions {
            collapse: CollapseMode::Square,
            ..Default::default()
        };
        let n = 4000;
        let half = 1250.0; // area (2·1250)² = 6.25e6, p = 0.25
        let mut true_area = 0.0;
        let mut emitted_area = 0.0;
        for i in 0..n {
            let (cx, cy) = (i as f64 * 17_077.0, (i % 613) as f64 * 12_923.0);
            let poly = square(cx, cy, half);
            true_area += poly.unsigned_area();
            if let Simplified::Keep(Geometry::Polygon(sq)) =
                simplify_for_level(&Geometry::Polygon(poly), tol, Crs::Epsg3857, &opts)
            {
                emitted_area += sq.unsigned_area();
            }
        }
        let ratio = emitted_area / true_area;
        assert!(
            (0.85..1.15).contains(&ratio),
            "aggregate area must be preserved in expectation, ratio = {ratio}"
        );
    }

    /// Above-tolerance polygons are untouched by Square mode.
    #[test]
    fn test_square_collapse_leaves_visible_polygons_alone() {
        let big = Geometry::Polygon(square(0.0, 0.0, 50_000.0));
        let drop_opts = SimplifyOptions::default();
        let square_opts = SimplifyOptions {
            collapse: CollapseMode::Square,
            ..Default::default()
        };
        assert_eq!(
            simplify_for_level(&big, 1000.0, Crs::Epsg3857, &square_opts),
            simplify_for_level(&big, 1000.0, Crs::Epsg3857, &drop_opts)
        );
    }

    /// Under Square mode, each collapsed MultiPolygon part dithers its own
    /// square (per-part density), and every emitted part is a Polygon —
    /// the geometry type never changes.
    #[test]
    fn test_square_collapse_multipolygon_per_part() {
        let tol = 5000.0;
        let opts = SimplifyOptions {
            collapse: CollapseMode::Square,
            ..Default::default()
        };
        // 40 tiny parts of p ≈ 0.64 each: expect several survivors.
        let parts: Vec<Polygon<f64>> = (0..40)
            .map(|i| square(i as f64 * 40_000.0, i as f64 * 23_000.0, 2000.0))
            .collect();
        let mp = Geometry::MultiPolygon(MultiPolygon::new(parts));
        match simplify_for_level(&mp, tol, Crs::Epsg3857, &opts) {
            Simplified::Keep(Geometry::MultiPolygon(out)) => {
                assert!(out.0.len() > 1, "several parts should dither through");
                assert!(out.0.len() < 40, "some parts should dither out");
                for p in &out.0 {
                    let r = p.bounding_rect().unwrap();
                    assert!((r.width() - tol).abs() < 1e-6);
                }
            }
            other => panic!("expected Keep(MultiPolygon), got {other:?}"),
        }
    }

    /// Representation::Square in a cascade: a square kept at the band's
    /// finest level re-dithers deterministically at coarser steps (same
    /// anchor ⇒ same u), so survival is monotone and engine-independent.
    #[test]
    fn test_square_step_and_cascade_consistency() {
        let opts = SimplifyOptions::default();
        let poly = Geometry::Polygon(square(731.0, -1911.0, 800.0));
        // Direct step vs single-step cascade must agree.
        let direct = simplify_step(&poly, 5000.0, Crs::Epsg3857, &opts, Representation::Square);
        let steps = [CascadeStep {
            gsd_meters: 5000.0,
            repr: Representation::Square,
        }];
        assert_eq!(
            direct,
            simplify_cascade(&poly, &steps, Crs::Epsg3857, &opts)
        );
        // Monotone: if dropped at the fine square step, a longer chain
        // through a coarser square step is dropped too.
        let chain = [
            CascadeStep {
                gsd_meters: 5000.0,
                repr: Representation::Square,
            },
            CascadeStep {
                gsd_meters: 10_000.0,
                repr: Representation::Square,
            },
        ];
        let coarser = simplify_cascade(&poly, &chain, Crs::Epsg3857, &opts);
        if matches!(direct, Simplified::Dropped) {
            assert_eq!(coarser, Simplified::Dropped, "drops are monotone");
        }
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
        match simplify_polygon_impl(&poly, 0.01, CollapseMode::Drop, true) {
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
        match simplify_polygon_impl(&poly, 0.01, CollapseMode::Drop, true) {
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
