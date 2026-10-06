//! World-space, GSD-driven geometry simplification for overview levels.
//!
//! # Why this module exists (vs. `crate::simplify`)
//!
//! The crate-level `crate::simplify` module simplifies in **tile-local
//! pixel space**: every entry point transforms geometry into a
//! `TileCoord` + `extent` (0–4096) pixel frame, runs Ramer–Douglas–Peucker
//! (RDP), and transforms back. That is correct for MVT tile generation but
//! meaningless for overview *levels*, which have no tile seams, no pixel
//! extent, and no per-tile context.
//!
//! Overview simplification runs RDP **directly on the source-CRS geometry**
//! with a world-space tolerance derived from the level's GSD (ground sample
//! distance, meters). This module therefore *extracts and adapts* the
//! algorithms from `crate::simplify` (the RDP call itself — originally
//! `geo`'s [`geo::Simplify`], now the output-identical iterative
//! `rdp_coords`, #575 — plus the ring-validity/degenerate guards) but
//! couples to none of its tile-space entry points.
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
use std::sync::Arc;

use geo::{
    Area, BoundingRect, Centroid, Distance, Geometry, LineString, MultiLineString, MultiPolygon,
    Point, Polygon, Rect, Validation,
};

use super::level::{zoom_for_gsd, WEBMERC_CIRCUMFERENCE_M};
pub use super::level::{Crs, METERS_PER_DEGREE};
use crate::mvt::{DEFAULT_EXTENT, MIN_SURVIVING_SQUARE_SIDE};

/// Process-wide count of polygons that exhausted every epsilon-backoff retry
/// (see [`simplify_polygon_impl_checked`]) and were kept at full resolution.
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
    level_tolerance(gsd_meters, crs, opts.factor)
}

/// `factor × gsd`, in the geometry's coordinate units: the one tolerance
/// formula behind [`world_tolerance`] and [`placeholder_side`] (#407).
///
/// `0.0` when `factor × gsd_meters <= 0` (the canonical/identity case).
pub(crate) fn level_tolerance(gsd_meters: f64, crs: Crs, factor: f64) -> f64 {
    let meters = factor * gsd_meters;
    if meters <= 0.0 {
        return 0.0;
    }
    crs.meters_to_units(meters)
}

/// The Web Mercator zoom the exporter renders a level at: the level's
/// recorded `zoom` when it has one (a `--min-zoom/--max-zoom` plan), else the
/// §5.2 inverse of its GSD rounded to the nearest zoom and floored at 0 (an
/// explicit `--gsd` plan). The same rule as `export::zoom_for_level`.
pub(crate) fn export_zoom(gsd_meters: f64, zoom: Option<u8>) -> f64 {
    match zoom {
        Some(z) => f64::from(z),
        None => zoom_for_gsd(gsd_meters).round().max(0.0),
    }
}

/// Length in meters of one MVT tile unit at the zoom a level renders at
/// ([`export_zoom`]), for the default extent [`DEFAULT_EXTENT`]:
/// `circumference / 2^z / 4096`. With a zoom-range plan that is
/// `gsd × gsd_base / 4096`, so a quarter of the GSD at the default
/// `--gsd-base` 1024.
///
/// The overview is written before any export extent is chosen, so the unit
/// assumes the default, which is the only extent the CLI exports at. An
/// export at a larger extent (the Python `export(extent=…)`) draws a floored
/// placeholder over more than one unit, which is still visible; one at a
/// smaller extent draws it under one unit, where the cleaner keeps it with
/// probability `side²` as it did before the floor.
pub(crate) fn tile_unit_meters(gsd_meters: f64, zoom: Option<u8>) -> f64 {
    WEBMERC_CIRCUMFERENCE_M / export_zoom(gsd_meters, zoom).exp2() / f64::from(DEFAULT_EXTENT)
}

/// Side of a level's placeholder square in coordinate units: the level
/// tolerance `factor × gsd`, floored at [`MIN_SURVIVING_SQUARE_SIDE`] tile
/// units at the level's export zoom ([`tile_unit_meters`]). One value serves
/// the `--collapse-square` dither ([`squarify_polygon`]), the accumulator
/// threshold (`side²`) and [`carrier_square`], so the three stay
/// interchangeable (#407).
///
/// `0.0` when the tolerance is `0` (`--simplify-factor 0`): there is no
/// placeholder then, and no floor is applied.
///
/// DIVERGENCE FROM TIPPECANOE: tippecanoe's placeholder is a fixed
/// `tiny_polygon_size` in tile units (clip.cpp, `reduce_tiny_poly`); ours is
/// `factor × gsd` with this one-unit floor, so it follows the simplify knob
/// down to the smallest square a tile can draw and no further. Like
/// tippecanoe (which skips the reduction at size 0), a zero factor means no
/// placeholder at all.
pub(crate) fn placeholder_side(gsd_meters: f64, zoom: Option<u8>, crs: Crs, factor: f64) -> f64 {
    let tol = level_tolerance(gsd_meters, crs, factor);
    if tol <= 0.0 {
        return 0.0;
    }
    let floor = crs.meters_to_units(MIN_SURVIVING_SQUARE_SIDE * tile_unit_meters(gsd_meters, zoom));
    tol.max(floor)
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
    simplify_for_level_checked(geom, gsd_meters, None, crs, opts).0
}

/// Like [`simplify_for_level`], but also reports whether the returned
/// geometry is value-identical to `geom` (#499, the compute side of the
/// ladder amplification issue): `unchanged == true` means the output is a
/// value-identical clone of the input — no vertex was dropped, no ring
/// collapsed, no disposition changed. Cheap: every case below is decided
/// from a `Vec::len()` comparison the simplification call already computed
/// ([`polygon_unchanged`], [`simplify_linestring_checked`]), never a
/// coordinate-by-coordinate diff.
///
/// Callers that fold a geometry through several levels (the cascade fold,
/// [`super::stream::process_batch_cascade`]) use `unchanged == true` to skip
/// retaining a fresh allocation and instead share (`Arc::clone`) the input
/// they fed in — for FTW-like small polygons at minimum vertex count,
/// adjacent ladder levels often produce identical output, so this turns an
/// O(levels) chain of deep clones into O(1) allocations plus refcount bumps.
pub(super) fn simplify_for_level_checked(
    geom: &Geometry<f64>,
    gsd_meters: f64,
    zoom: Option<u8>,
    crs: Crs,
    opts: &SimplifyOptions,
) -> (Simplified, bool) {
    let tol = world_tolerance(gsd_meters, crs, opts);
    // The placeholder side only matters to the square disposition; skip the
    // zoom arithmetic otherwise.
    let side = if opts.collapse == CollapseMode::Square {
        placeholder_side(gsd_meters, zoom, crs, opts.factor)
    } else {
        tol
    };

    // Canonical / identity path (spec §2.4, Q1): a zero tolerance means "this
    // is the canonical level" — return the geometry bit-identical, with no
    // simplification, gating, or dropping.
    if tol <= 0.0 {
        return (Simplified::Keep(geom.clone()), true);
    }

    match geom {
        // Points carry no reducible vertices and are never gated (spec §2.1).
        Geometry::Point(_) | Geometry::MultiPoint(_) => (Simplified::Keep(geom.clone()), true),

        Geometry::LineString(ls) => match simplify_linestring_checked(ls, tol) {
            Some((out, unchanged)) => (Simplified::Keep(Geometry::LineString(out)), unchanged),
            None => (Simplified::Dropped, false),
        },

        Geometry::MultiLineString(mls) => {
            let mut kept: Vec<LineString<f64>> = Vec::with_capacity(mls.0.len());
            let mut unchanged = true;
            for ls in &mls.0 {
                match simplify_linestring_checked(ls, tol) {
                    Some((out, part_unchanged)) => {
                        unchanged &= part_unchanged;
                        kept.push(out);
                    }
                    None => unchanged = false,
                }
            }
            if kept.is_empty() {
                (Simplified::Dropped, false)
            } else {
                // defensive: already implied by the arms above
                unchanged &= kept.len() == mls.0.len();
                (
                    Simplified::Keep(Geometry::MultiLineString(MultiLineString::new(kept))),
                    unchanged,
                )
            }
        }

        Geometry::Polygon(poly) => {
            simplify_polygon_impl_checked(poly, tol, side, opts.collapse, opts.cascade)
        }

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
            let mut kept: Vec<Polygon<f64>> = Vec::with_capacity(mp.0.len());
            let mut unchanged = true;
            for p in &mp.0 {
                match simplify_polygon_impl_checked(p, tol, side, part_mode, opts.cascade) {
                    (Simplified::Keep(Geometry::Polygon(poly)), part_unchanged) => {
                        unchanged &= part_unchanged;
                        kept.push(poly);
                    }
                    (Simplified::Keep(Geometry::MultiPolygon(parts)), _) => {
                        // A repaired part expanding into several — always a
                        // structural change even if some sub-parts, taken in
                        // isolation, would compare unchanged.
                        unchanged = false;
                        kept.extend(parts.0);
                    }
                    (Simplified::Keep(_) | Simplified::Dropped, _) => unchanged = false,
                }
            }
            if !kept.is_empty() {
                // defensive: already implied by the arms above
                unchanged &= kept.len() == mp.0.len();
                (
                    Simplified::Keep(Geometry::MultiPolygon(MultiPolygon::new(kept))),
                    unchanged,
                )
            } else {
                match opts.collapse {
                    // Every part had its own dither under Square; nothing more.
                    CollapseMode::Drop | CollapseMode::Square => (Simplified::Dropped, false),
                    CollapseMode::Point => match mp.centroid() {
                        Some(pt) => (Simplified::Keep(Geometry::Point(pt)), false),
                        None => (Simplified::Dropped, false),
                    },
                }
            }
        }

        // GeometryCollection / Line / Rect / Triangle: out of scope for v0.1;
        // pass through untouched.
        other => (Simplified::Keep(other.clone()), true),
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
}

impl Representation {
    /// The spec / CLI keyword for this representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Representation::Geometry => "geom",
            Representation::Point => "point",
            Representation::Square => "square",
        }
    }
}

/// One step of a cascading fine→coarse simplification chain (#218, #317).
///
/// A step is a level's GSD plus its [`Representation`]: a
/// [`Representation::Point`] step marks a zoom-band point level (#317) at
/// which polygonal features are replaced by their representative point
/// instead of being simplified.
///
/// `#[non_exhaustive]` since #407 added [`zoom`](Self::zoom): build steps
/// with [`geom`](Self::geom) / [`point`](Self::point) /
/// [`square`](Self::square) and [`with_zoom`](Self::with_zoom).
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct CascadeStep {
    /// The step level's GSD in meters.
    pub gsd_meters: f64,
    /// The step level's feature representation (#317).
    pub repr: Representation,
    /// The step level's recorded Web Mercator zoom, `None` when the plan has
    /// none (an explicit `--gsd` plan). Sets the tile unit the placeholder
    /// square is floored at (#407); `None` derives the zoom from the GSD the
    /// way the exporter does.
    pub zoom: Option<u8>,
}

impl CascadeStep {
    /// A normal (full-geometry) cascade step.
    pub fn geom(gsd_meters: f64) -> Self {
        Self {
            gsd_meters,
            repr: Representation::Geometry,
            zoom: None,
        }
    }

    /// A zoom-band point-representation step (#317).
    pub fn point(gsd_meters: f64) -> Self {
        Self {
            gsd_meters,
            repr: Representation::Point,
            zoom: None,
        }
    }

    /// A zoom-band placeholder-square step (#279).
    pub fn square(gsd_meters: f64) -> Self {
        Self {
            gsd_meters,
            repr: Representation::Square,
            zoom: None,
        }
    }

    /// This step with the level's recorded zoom (#407).
    #[must_use]
    pub fn with_zoom(self, zoom: Option<u8>) -> Self {
        Self { zoom, ..self }
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
/// chain is applied to `MultiPolygons` too: a point-band level must never
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
/// (`polygonal_representative_point`) — unconditionally, with no
/// visibility gating (a dot is always visible) — while points pass through
/// and lines keep the normal simplification path. On a
/// [`Representation::Square`] level (#279), simplification is normal but
/// below-tolerance polygons emit area-dithered placeholder squares
/// (`squarify_polygon`) instead of following the global [`CollapseMode`].
pub fn simplify_step(
    geom: &Geometry<f64>,
    gsd_meters: f64,
    crs: Crs,
    opts: &SimplifyOptions,
    repr: Representation,
) -> Simplified {
    simplify_step_checked(geom, gsd_meters, None, crs, opts, repr).0
}

/// [`simplify_step`] for a level whose recorded zoom is known: the engines'
/// entry point, so the placeholder square is floored at the tile unit of
/// the zoom the level is exported at (#407).
pub(crate) fn simplify_step_at(
    geom: &Geometry<f64>,
    gsd_meters: f64,
    zoom: Option<u8>,
    crs: Crs,
    opts: &SimplifyOptions,
    repr: Representation,
) -> Simplified {
    simplify_step_checked(geom, gsd_meters, zoom, crs, opts, repr).0
}

/// Like [`simplify_step`], but also reports whether the output is
/// value-identical to `geom` (`unchanged == true`) — see
/// [`simplify_for_level_checked`] (#499). A [`Representation::Point`] step's
/// polygon-to-point conversion always counts as changed (the geometry type
/// differs from the input); a *point* revisited by a later `Point` step
/// correctly reports `unchanged == true`, since
/// [`polygonal_representative_point`] only matches polygonal input and
/// points fall through to [`simplify_for_level_checked`], which passes them
/// through untouched — matching the "coarser steps pass the point through
/// untouched" cascade semantics documented on [`simplify_cascade`].
pub(super) fn simplify_step_checked(
    geom: &Geometry<f64>,
    gsd_meters: f64,
    zoom: Option<u8>,
    crs: Crs,
    opts: &SimplifyOptions,
    repr: Representation,
) -> (Simplified, bool) {
    match repr {
        Representation::Geometry => simplify_for_level_checked(geom, gsd_meters, zoom, crs, opts),
        Representation::Point => {
            if let Some(out) = polygonal_representative_point(geom) {
                return (out, false);
            }
            simplify_for_level_checked(geom, gsd_meters, zoom, crs, opts)
        }
        // Square (#279): normal simplification with the below-tolerance
        // disposition forced to area-dithered placeholder squares at this
        // level — above-tolerance polygons are unaffected (type-preserving).
        Representation::Square => {
            let opts = SimplifyOptions {
                collapse: CollapseMode::Square,
                ..*opts
            };
            simplify_for_level_checked(geom, gsd_meters, zoom, crs, &opts)
        }
    }
}

/// Cascading simplification (#218): fold a geometry through a fine→coarse chain of level steps.
///
/// The fold feeds each coarser level the previous level's
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
///
/// The step rules live in `CascadeFold`, which the pipelined engine's
/// incremental fold (`overview::stream::process_batch_cascade`) drives too;
/// equivalence is enforced by
/// `overview::convert::tests::pipelined_matches_serial`.
pub fn simplify_cascade(
    geom: &Geometry<f64>,
    steps_fine_to_coarse: &[CascadeStep],
    crs: Crs,
    opts: &SimplifyOptions,
) -> Simplified {
    if steps_fine_to_coarse.is_empty() {
        return Simplified::Keep(geom.clone());
    }
    let canonical = Arc::new(geom.clone());
    let mut fold = CascadeFold::new();
    let mut last = FoldStep::Dropped;
    for step in steps_fine_to_coarse {
        last = fold.step(&canonical, step, crs, opts);
    }
    // Release every other handle first, so the common case (the result is a
    // fresh step output, uniquely owned) unwraps without a copy.
    drop(fold);
    drop(canonical);
    match last {
        FoldStep::Keep { geom, .. } => {
            Simplified::Keep(Arc::try_unwrap(geom).unwrap_or_else(|shared| (*shared).clone()))
        }
        FoldStep::Dropped => Simplified::Dropped,
    }
}

/// The cascade's per-feature state machine (#218), shared by every fold in
/// the crate (#541 review): [`simplify_cascade`] (the Serial and in-memory
/// engines), the pipelined engine's incremental fold, and its #541 prefix
/// over unmaterialized fine levels. Keeping ONE copy of the step rules is
/// what keeps those paths byte-identical; they used to be three hand-synced
/// loops.
///
/// Rules, per [`step`](Self::step):
/// - a [`Representation::Geometry`] step after a drop stays dropped (dropping
///   is monotone along geometry steps);
/// - otherwise the step folds from the working geometry while alive, or
///   **revives from canonical** geometry at a `Point` / `Square` step;
/// - a kept result that the step left value-identical to its input
///   (`unchanged`, #499) shares the input's allocation instead of the fresh
///   copy — same value, one fewer geometry resident.
pub(crate) struct CascadeFold {
    current: Option<Arc<Geometry<f64>>>,
    alive: bool,
}

/// One [`CascadeFold::step`]'s result.
pub(crate) enum FoldStep {
    /// Kept. `shared` is `true` when `geom` is the step's input allocation,
    /// reused because the step changed nothing (#499's profile counter).
    Keep {
        /// The level's geometry.
        geom: Arc<Geometry<f64>>,
        /// Whether `geom` is shared with the step's input.
        shared: bool,
    },
    /// Not meaningful at this level.
    Dropped,
}

impl CascadeFold {
    /// A fresh fold: alive, working geometry = canonical.
    pub(crate) fn new() -> Self {
        Self {
            current: None,
            alive: true,
        }
    }

    /// Whether the working geometry survived the last step.
    pub(crate) fn is_alive(&self) -> bool {
        self.alive
    }

    /// Apply one fine→coarse step. `canonical` is both the starting geometry
    /// and the revival source; it must be the same value on every call.
    pub(crate) fn step(
        &mut self,
        canonical: &Arc<Geometry<f64>>,
        step: &CascadeStep,
        crs: Crs,
        opts: &SimplifyOptions,
    ) -> FoldStep {
        if !self.alive && step.repr == Representation::Geometry {
            // Monotone along geometry steps: once dropped, stays dropped.
            return FoldStep::Dropped;
        }
        // Alive: cascade the previous step's output. Not alive (Point /
        // Square step): revive from canonical geometry.
        let base: &Arc<Geometry<f64>> = if self.alive {
            self.current.as_ref().unwrap_or(canonical)
        } else {
            canonical
        };
        let (out, unchanged) = simplify_step_checked(
            base.as_ref(),
            step.gsd_meters,
            step.zoom,
            crs,
            opts,
            step.repr,
        );
        match out {
            Simplified::Keep(s) => {
                let geom = if unchanged {
                    Arc::clone(base)
                } else {
                    Arc::new(s)
                };
                self.current = Some(Arc::clone(&geom));
                self.alive = true;
                FoldStep::Keep {
                    geom,
                    shared: unchanged,
                }
            }
            Simplified::Dropped => {
                self.alive = false;
                FoldStep::Dropped
            }
        }
    }
}

// ============================================================================
// Internal helpers (world-space, tile-free).
//
// These adapt the algorithms from `crate::simplify` — the RDP call itself
// (`rdp_coords`, output-identical to `geo`'s `Simplify` but iterative,
// #575), the ring-closure/degenerate guards, and the multi
// dispatch — but run directly on source-CRS coordinates with a world-space
// tolerance instead of transforming into tile-local pixel space.
// ============================================================================

/// Bounding-box diagonal of a `Rect` in coordinate units.
#[inline]
fn rect_diag(r: Rect<f64>) -> f64 {
    r.width().hypot(r.height())
}

/// Minimum retained vertices RDP will not cull below, for an open LineString
/// (`geo`'s `LINE_STRING_INITIAL_MIN`).
const RDP_MIN_LINE: usize = 2;

/// Minimum retained vertices RDP will not cull below, for a Polygon ring
/// (`geo`'s `POLYGON_INITIAL_MIN`).
const RDP_MIN_RING: usize = 4;

/// Ramer–Douglas–Peucker, with the recursion on the heap (#575).
///
/// # Why this is not `geo::Simplify`
///
/// `geo`'s `compute_rdp` recurses, and RDP's split depth is **O(n)** in the
/// worst case, not O(log n): when the farthest vertex is always next to a
/// subproblem's end, every frame peels off one vertex. A regular zigzag whose
/// amplitude is just above the epsilon does exactly that — coalescing a road
/// row into one 12,000-vertex stroke and simplifying it blew a rayon worker's
/// stack (`fatal runtime error: stack overflow`, #575). Coalescing makes long
/// chains routine, so the depth has to come off the stack. The explicit stack
/// grows on the heap and is bounded by the same O(n), which is now fine.
///
/// It also removes geo's per-frame `Vec<RdpIndex<T>>` (allocated, popped and
/// `extend_from_slice`d at every level, i.e. O(n²) *copying* on top of the
/// O(n²) distance scans in the pathological case): this keeps one `keep` mask
/// and writes the survivors out once.
///
/// # Output identity
///
/// Bit-for-bit `geo`'s result, deliberately:
///
/// - the farthest-vertex scan runs over the same half-open interior range and
///   keeps the LAST maximum (`>=`), seeded at distance `0.0`;
/// - distances come from **`geo`'s own** point-to-segment
///   `Euclidean.distance`, so no comparison can land differently through a
///   re-derived formula;
/// - the `> epsilon` split test is strict, as there;
/// - `initial_min` reproduces geo's `INITIAL_MIN` floor, including that it is
///   threaded through a single running length in depth-first, left-to-right
///   leaf order — hence `stack.push(right)` before `stack.push(left)` below,
///   which makes the pop order a pre-order DFS and visits leaves left to
///   right, exactly as the recursion did.
///
/// A differential test (`rdp_matches_geo_*`) pins all of this against
/// `geo::Simplify` over random and adversarial shapes.
//
// DIVERGENCE FROM TIPPECANOE: tippecanoe's `douglas_peucker` (clip.cpp) is
// itself iterative (an explicit `std::stack`), so running on the heap
// matches it. Its split rules differ, and this keeps `geo`'s because it must
// reproduce the pre-#575 output byte for byte. Tippecanoe measures distance
// on integer tile coordinates rounded to 1/16 (`distance_from_line`), breaks
// distance ties toward the lexicographically smallest vertex, scanning from
// the smaller endpoint so the result does not depend on winding, and keeps a
// minimum through its `retain` count rather than `geo`'s `INITIAL_MIN` floor.
// We keep the last maximum in index order and the `INITIAL_MIN` floor. Both
// use a strict `> epsilon` split test. Recorded in context/ARCHITECTURE.md
// (Known Divergences, Simplification row).
fn rdp_coords(
    coords: &[geo::Coord<f64>],
    epsilon: f64,
    initial_min: usize,
) -> Vec<geo::Coord<f64>> {
    // `geo` returns the input untouched for a non-positive epsilon, and its
    // base cases (0, 1 or 2 coordinates) retain everything.
    if epsilon <= 0.0 || coords.len() < 3 {
        return coords.to_vec();
    }

    let mut keep = vec![true; coords.len()];
    let mut retained = coords.len();
    let mut stack: Vec<(usize, usize)> = vec![(0, coords.len() - 1)];
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue; // nothing between the endpoints: geo's 2-element base case
        }
        let chord = geo::Line::new(coords[a], coords[b]);
        // Seeded exactly as geo's fold: index `a` ("local 0"), distance 0.
        let mut farthest = a;
        let mut farthest_dist = 0.0_f64;
        // `take(b).skip(a + 1)`: geo's own interior range, ascending.
        for (i, c) in coords.iter().enumerate().take(b).skip(a + 1) {
            let d = geo::Euclidean.distance(*c, &chord);
            if d >= farthest_dist {
                farthest = i;
                farthest_dist = d;
            }
        }
        debug_assert_ne!(farthest, a, "an interior vertex is always the farthest");
        if farthest_dist > epsilon {
            // Right first so the left subproblem pops (and culls) first.
            stack.push((farthest, b));
            stack.push((a, farthest));
            continue;
        }
        // Cull the interior — unless that would take the whole geometry below
        // the floor, in which case geo keeps this subproblem's input as-is.
        let culled = b - a - 1;
        if retained - culled < initial_min {
            continue;
        }
        retained -= culled;
        for k in &mut keep[(a + 1)..b] {
            *k = false;
        }
    }
    debug_assert_eq!(
        keep.iter().filter(|k| **k).count(),
        retained,
        "the keep mask and geo's running retained length must agree"
    );

    let mut out = Vec::with_capacity(retained);
    out.extend(
        coords
            .iter()
            .zip(&keep)
            .filter(|(_, &k)| k)
            .map(|(c, _)| *c),
    );
    out
}

/// [`rdp_coords`] over a LineString — the drop-in for `geo`'s
/// `LineString::simplify`.
#[inline]
fn rdp_linestring(ls: &LineString<f64>, epsilon: f64) -> LineString<f64> {
    LineString::new(rdp_coords(&ls.0, epsilon, RDP_MIN_LINE))
}

/// [`rdp_coords`] over every ring — the drop-in for `geo`'s
/// `Polygon::simplify` (which uses the ring floor, not the line one).
fn rdp_polygon(poly: &Polygon<f64>, epsilon: f64) -> Polygon<f64> {
    Polygon::new(
        LineString::new(rdp_coords(&poly.exterior().0, epsilon, RDP_MIN_RING)),
        poly.interiors()
            .iter()
            .map(|r| LineString::new(rdp_coords(&r.0, epsilon, RDP_MIN_RING)))
            .collect(),
    )
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
/// Also reports whether RDP removed any vertices (`unchanged == false` when
/// it did) — cheap, since `geo`'s RDP output is always an ordered subsequence
/// of the input (endpoints included), so equal vertex counts imply an
/// identical linestring, a `Vec::len()` comparison rather than a
/// coordinate-by-coordinate diff (mirrors [`polygon_unchanged`]'s reasoning
/// for rings, #499).
///
/// Adapted from `crate::simplify`'s degenerate-linestring guard (which returns
/// the input unchanged for `< 2` points to avoid a `geo::Simplify` panic);
/// here the level path instead *drops* sub-visible lines and reports it.
fn simplify_linestring_checked(ls: &LineString<f64>, tol: f64) -> Option<(LineString<f64>, bool)> {
    if ls.0.len() < MIN_LINESTRING_POINTS {
        return None;
    }
    let diag = linestring_diag(ls);
    // All points coincide (diag == 0 ⇒ < 2 distinct points) or the whole
    // feature is finer than the level tolerance.
    if diag < tol {
        return None;
    }
    let simplified = rdp_linestring(ls, tol);
    if simplified.0.len() < MIN_LINESTRING_POINTS || linestring_diag(&simplified) <= 0.0 {
        return None;
    }
    let unchanged = simplified.0.len() == ls.0.len();
    debug_assert!(
        !unchanged || simplified == *ls,
        "RDP subsequence invariant broken: unchanged must mean value identity (#499)"
    );
    Some((simplified, unchanged))
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
/// (geometry validity is not a conformance requirement, `OVERVIEWS_SPEC` §
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
///
/// Since #499 this result also decides whether the cascade fold shares one
/// `Arc<Geometry<f64>>` across ladder levels, so it must never be `true`
/// unless the candidate is value-identical to `original`. That rests entirely
/// on the RDP ordered-subsequence premise above — a `geo` upgrade that ever
/// moved or replaced a retained vertex would break it, which is why both
/// checked paths carry a `debug_assert!` comparing the values outright.
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
/// - Rings are simplified via [`rdp_polygon`] (geo-identical RDP, which keeps each ring at
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
///
/// Only [`simplify_polygon_impl_checked`] is used outside tests now (#499);
/// this thin wrapper is kept for the tests below that don't care about the
/// `unchanged` flag.
#[cfg(test)]
fn simplify_polygon_impl(
    poly: &Polygon<f64>,
    tol: f64,
    mode: CollapseMode,
    repair: bool,
) -> Simplified {
    simplify_polygon_impl_checked(poly, tol, tol, mode, repair).0
}

/// Like `simplify_polygon_impl`, but also reports whether the kept
/// geometry is value-identical to `poly` (#499): `true` for the
/// RDP-removed-nothing candidate ([`polygon_unchanged`]) and the
/// full-resolution fallback (both literally the same rings as `poly`);
/// `false` for every collapse, repair, or vertex-removing candidate. Feeds
/// the cascade fold's Arc-sharing decision in
/// [`super::stream::process_batch_cascade`].
///
/// `side` is the placeholder side a collapse under
/// [`CollapseMode::Square`] emits ([`placeholder_side`]); the RDP epsilon and
/// the gates stay at `tol`.
fn simplify_polygon_impl_checked(
    poly: &Polygon<f64>,
    tol: f64,
    side: f64,
    mode: CollapseMode,
    repair: bool,
) -> (Simplified, bool) {
    if polygon_diag(poly) < tol {
        return (collapse_polygon(poly, mode, side), false);
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
        let simplified = rdp_polygon(poly, eps);

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
            return (collapse_polygon(poly, mode, side), false);
        }

        let unchanged = polygon_unchanged(&candidate, poly);
        debug_assert!(
            !unchanged || candidate == *poly,
            "RDP subsequence invariant broken: unchanged must mean value identity (#499)"
        );
        if unchanged || capped_is_valid(&candidate) {
            return (Simplified::Keep(Geometry::Polygon(candidate)), unchanged);
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
            return (Simplified::Keep(repaired), false);
        }
        // Repair left nothing above the gates (self-canceling sliver).
        return (collapse_polygon(poly, mode, side), false);
    }

    // Every retry self-intersected: keep the original geometry rather than
    // emit an invalid ring.
    FULL_RES_FALLBACKS.fetch_add(1, Ordering::Relaxed);
    log::trace!(
        "overview simplify: RDP candidate invalid after {} epsilon retries; \
         keeping full-resolution geometry",
        INVALID_RETRY_HALVINGS + 1
    );
    (Simplified::Keep(Geometry::Polygon(poly.clone())), true)
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
/// (Since #384 the write-time dither below is only half the story: polygons
/// a level does not carry at all go through a deterministic per-patch
/// accumulator in `overview::accumulate`, run once on the pass-1 feature
/// table. The dither remains for members that collapse under the tolerance
/// at write time, where the reasoning below still holds.)
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
pub(super) fn dither_u01(x: f64, y: f64) -> f64 {
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
/// survives as a `side × side` square at the polygon's representative point
/// with probability `min(1, area / side²)` (see [`dither_u01`]). `side` is
/// the level's [`placeholder_side`]: the tolerance, floored at one tile unit
/// (#407).
fn squarify_polygon(poly: &Polygon<f64>, side: f64) -> Simplified {
    let Some(anchor) = polygon_anchor(poly) else {
        return Simplified::Dropped;
    };
    let threshold = side * side;
    let p = if threshold > 0.0 {
        (poly.unsigned_area() / threshold).min(1.0)
    } else {
        0.0
    };
    if dither_u01(anchor.x(), anchor.y()) < p {
        Simplified::Keep(Geometry::Polygon(placeholder_square(anchor, side)))
    } else {
        Simplified::Dropped
    }
}

/// Placeholder square for a tiny-polygon accumulator carrier (#384): a
/// square of the level's [`placeholder_side`] at the polygon's
/// representative point — the same square the [`CollapseMode::Square`]
/// dither emits, so the two placeholders are interchangeable to a renderer.
/// `zoom` is the level's recorded zoom (#407). `None` for non-polygonal
/// input.
pub(super) fn carrier_square(
    geom: &Geometry<f64>,
    gsd_meters: f64,
    zoom: Option<u8>,
    crs: Crs,
    opts: &SimplifyOptions,
) -> Option<Geometry<f64>> {
    let anchor = match geom {
        Geometry::Polygon(p) => polygon_anchor(p),
        Geometry::MultiPolygon(mp) => {
            mp.0.iter()
                .max_by(|a, b| a.unsigned_area().total_cmp(&b.unsigned_area()))
                .and_then(polygon_anchor)
        }
        _ => None,
    }?;
    Some(Geometry::Polygon(placeholder_square(
        anchor,
        placeholder_side(gsd_meters, zoom, crs, opts.factor),
    )))
}

/// Resolve a collapsed polygon per the [`CollapseMode`] (#279): drop by
/// default, a representative [`Point`] at the polygon centroid (spec Q4,
/// `--collapse`), or an area-dithered placeholder square
/// (`--collapse-square`, [`squarify_polygon`] with the placeholder `side`).
fn collapse_polygon(poly: &Polygon<f64>, mode: CollapseMode, side: f64) -> Simplified {
    match mode {
        CollapseMode::Drop => Simplified::Dropped,
        CollapseMode::Point => match polygon_anchor(poly) {
            Some(p) => Simplified::Keep(Geometry::Point(p)),
            None => Simplified::Dropped,
        },
        CollapseMode::Square => squarify_polygon(poly, side),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Coord, MultiPoint, Simplify};

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

    // ---- #407: placeholder side floored at one tile unit -------------------

    /// The tile unit follows the zoom the exporter renders the level at: the
    /// recorded zoom when there is one, else the §5.2 inverse of the GSD
    /// rounded to the nearest zoom (`export::zoom_for_level`).
    #[test]
    fn tile_unit_follows_the_export_zoom() {
        use super::super::level::gsd;
        let extent = f64::from(DEFAULT_EXTENT);
        let want = WEBMERC_CIRCUMFERENCE_M / 128.0 / extent;
        assert!((tile_unit_meters(gsd(7), None) - want).abs() < 1e-9);
        // The recorded zoom wins over the GSD (a non-default `--gsd-base`).
        assert!((tile_unit_meters(123.0, Some(7)) - want).abs() < 1e-9);
        // Default base and extent: a unit is a quarter of a GSD.
        assert!((tile_unit_meters(gsd(9), Some(9)) - gsd(9) / 4.0).abs() < 1e-9);
        assert_eq!(export_zoom(1e9, None), 0.0, "floored at z0");
    }

    /// The `--collapse-square` dither uses the floored side: at factor 0.1
    /// on a z5 level (GSD 1000 m, unit 305.7 m) a collapsing 50 m field
    /// survives as a one-unit square, with probability 2,500 / unit² rather
    /// than 2,500 / 100², so the expected area is unchanged and every kept
    /// square is wide enough to draw.
    #[test]
    fn square_dither_uses_the_floored_side() {
        let opts = SimplifyOptions {
            factor: 0.1,
            collapse: CollapseMode::Square,
            ..SimplifyOptions::default()
        };
        let unit = tile_unit_meters(1000.0, None);
        let n = 4000;
        let mut kept = 0usize;
        for i in 0..n {
            let (cx, cy) = (i as f64 * 1_117.3, (i % 97) as f64 * 733.1);
            let poly = Geometry::Polygon(square(cx, cy, 25.0));
            if let Simplified::Keep(g) = simplify_step_at(
                &poly,
                1000.0,
                None,
                Crs::Epsg3857,
                &opts,
                Representation::Geometry,
            ) {
                let r = g.bounding_rect().unwrap();
                assert!(
                    (r.width() - unit).abs() < 1e-6,
                    "width {} vs unit {unit}",
                    r.width()
                );
                kept += 1;
            }
        }
        let p = 2_500.0 / (unit * unit);
        let expected = p * n as f64;
        let sd = (n as f64 * p * (1.0 - p)).sqrt();
        assert!(
            (kept as f64 - expected).abs() < 4.0 * sd,
            "kept {kept} of {n}, expected {expected:.1} ± {sd:.1}"
        );
        // The same GSD recorded at z2 (a `--gsd-base` near 10,000): one unit
        // there is ~2,446 m, so that is the square's side.
        let poly = Geometry::Polygon(square(0.0, 0.0, 25.0));
        let side = placeholder_side(1000.0, Some(2), Crs::Epsg3857, 0.1);
        assert!((side - WEBMERC_CIRCUMFERENCE_M / 4.0 / 4096.0).abs() < 1e-9);
        if let Simplified::Keep(g) = simplify_step_at(
            &poly,
            1000.0,
            Some(2),
            Crs::Epsg3857,
            &opts,
            Representation::Geometry,
        ) {
            assert!((g.bounding_rect().unwrap().width() - side).abs() < 1e-6);
        }
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

    /// `Representation::Square` in a cascade: a square kept at the band's
    /// finest level re-dithers deterministically at coarser steps (same
    /// anchor ⇒ same u), so survival is monotone and engine-independent.
    #[test]
    fn test_square_step_and_cascade_consistency() {
        let opts = SimplifyOptions::default();
        let poly = Geometry::Polygon(square(731.0, -1911.0, 800.0));
        // Direct step vs single-step cascade must agree.
        let direct = simplify_step(&poly, 5000.0, Crs::Epsg3857, &opts, Representation::Square);
        let steps = [CascadeStep::square(5000.0)];
        assert_eq!(
            direct,
            simplify_cascade(&poly, &steps, Crs::Epsg3857, &opts)
        );
        // Monotone: if dropped at the fine square step, a longer chain
        // through a coarser square step is dropped too.
        let chain = [CascadeStep::square(5000.0), CascadeStep::square(10_000.0)];
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

    // ---- `simplify_step_checked` (#499 compute-side: cascade Arc-sharing) --
    //
    // `unchanged == true` means the returned geometry is value-identical to
    // the input — the sharing decision the cascade fold makes in
    // `process_batch_cascade`.

    #[test]
    fn simplify_step_checked_reports_no_removal_for_minimal_geometry() {
        // A 4-point square (the minimum valid ring) can't lose any more
        // vertices to RDP without collapsing entirely, so a tolerance that
        // keeps it alive must report `unchanged == true`.
        let poly = Geometry::Polygon(square(0.0, 0.0, 50.0));
        let opts = SimplifyOptions::default();
        let (out, unchanged) = simplify_step_checked(
            &poly,
            10.0,
            None,
            Crs::Epsg3857,
            &opts,
            Representation::Geometry,
        );
        assert!(unchanged, "minimal ring should report no removal");
        match out {
            Simplified::Keep(Geometry::Polygon(p)) => {
                assert_eq!(p.exterior().0.len(), 5, "ring should be untouched");
            }
            other => panic!("expected Keep(Polygon), got {other:?}"),
        }
    }

    #[test]
    fn simplify_step_checked_reports_removal_when_rdp_drops_vertices() {
        let line = Geometry::LineString(wiggly_line(200, 50.0));
        let opts = SimplifyOptions::default();
        let (out, unchanged) = simplify_step_checked(
            &line,
            500.0,
            None,
            Crs::Epsg3857,
            &opts,
            Representation::Geometry,
        );
        assert!(!unchanged, "coarse GSD should remove vertices");
        assert!(line_len(&out) < 200);
    }

    #[test]
    fn simplify_step_checked_canonical_level_reports_no_removal() {
        // gsd == 0 is the canonical/identity path: always a bit-identical
        // clone, so always "no removal" regardless of geometry.
        let line = Geometry::LineString(wiggly_line(200, 50.0));
        let opts = SimplifyOptions::default();
        let (out, unchanged) = simplify_step_checked(
            &line,
            0.0,
            None,
            Crs::Epsg3857,
            &opts,
            Representation::Geometry,
        );
        assert!(unchanged);
        assert_eq!(line_len(&out), 200);
    }

    #[test]
    fn simplify_step_checked_point_revival_step_is_unchanged() {
        // A polygon on a `Point` step is a representation change (not
        // unchanged); but a *second* `Point` step over the resulting Point
        // (the "coarser steps pass the point through untouched" cascade
        // semantics) must report `unchanged == true`, since points are never
        // touched by `simplify_for_level`.
        let poly = Geometry::Polygon(square(0.0, 0.0, 5.0));
        let opts = SimplifyOptions::default();
        let (first, first_unchanged) = simplify_step_checked(
            &poly,
            100.0,
            None,
            Crs::Epsg3857,
            &opts,
            Representation::Point,
        );
        assert!(
            !first_unchanged,
            "polygon -> point is a representation change"
        );
        let Simplified::Keep(point_geom) = first else {
            panic!("expected the polygon to revive to a point");
        };
        let (second, second_unchanged) = simplify_step_checked(
            &point_geom,
            200.0,
            None,
            Crs::Epsg3857,
            &opts,
            Representation::Point,
        );
        assert!(
            second_unchanged,
            "a point passed through a coarser Point step is untouched"
        );
        assert_eq!(second, Simplified::Keep(point_geom));
    }
}

#[cfg(test)]
mod rdp_tests {
    use super::*;
    use geo::{Coord, Simplify};
    use std::time::Instant;

    /// The #575 shape: a regular zigzag of amplitude `amp`, the worst case for
    /// RDP's split depth (the farthest vertex is always next to an end, so
    /// every split peels off one vertex — O(n) deep, O(n²) work).
    fn zigzag(n: usize, amp: f64, step: f64) -> LineString<f64> {
        LineString::new(
            (0..n)
                .map(|i| Coord {
                    x: i as f64 * step,
                    y: if i % 2 == 1 { amp } else { 0.0 },
                })
                .collect(),
        )
    }

    /// Deterministic pseudo-random walk (no dev-dependency on a RNG).
    fn noisy_line(n: usize, seed: u64, scale: f64) -> LineString<f64> {
        let mut s = seed | 1;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s >> 11) as f64 / (1u64 << 53) as f64) - 0.5
        };
        let mut x = 0.0;
        let mut y = 0.0;
        LineString::new(
            (0..n)
                .map(|_| {
                    x += 1.0 + next() * 0.5;
                    y += next() * scale;
                    Coord { x, y }
                })
                .collect(),
        )
    }

    /// The contract [`rdp_coords`] rests on: identical output to `geo`'s
    /// recursive RDP, over shapes that exercise culling, the `INITIAL_MIN`
    /// floor, and the pathological split depth.
    #[test]
    fn rdp_matches_geo_on_lines() {
        let mut cases: Vec<LineString<f64>> = vec![
            LineString::new(vec![]),
            LineString::new(vec![Coord { x: 0.0, y: 0.0 }]),
            LineString::new(vec![Coord { x: 0.0, y: 0.0 }, Coord { x: 1.0, y: 0.0 }]),
            // Collinear: everything between the ends is culled.
            LineString::new(
                (0..9)
                    .map(|i| Coord {
                        x: i as f64,
                        y: 0.0,
                    })
                    .collect(),
            ),
            // All coincident (the floor is what keeps two of them).
            LineString::new((0..7).map(|_| Coord { x: 3.0, y: 4.0 }).collect()),
            // geo's own doc example.
            LineString::new(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: 5.0, y: 4.0 },
                Coord { x: 11.0, y: 5.5 },
                Coord { x: 17.3, y: 3.2 },
                Coord { x: 27.8, y: 0.1 },
            ]),
        ];
        for n in [3usize, 5, 17, 101, 1001] {
            cases.push(zigzag(n, 0.0002, 0.0004));
            cases.push(zigzag(n, 1.0, 1.0));
        }
        for seed in [1u64, 42, 12345] {
            for n in [4usize, 33, 257, 2049] {
                cases.push(noisy_line(n, seed, 3.0));
            }
        }

        for (c, ls) in cases.iter().enumerate() {
            for &tol in &[
                -1.0, 0.0, 1e-9, 0.00001, 0.0001, 0.00019, 0.0002, 0.00021, 0.5, 1.0, 2.0, 1e6,
            ] {
                assert_eq!(
                    rdp_linestring(ls, tol),
                    ls.simplify(tol),
                    "case {c} (n={}) diverged from geo at tol {tol}",
                    ls.0.len()
                );
            }
        }
    }

    /// Same contract for rings, which carry geo's higher `INITIAL_MIN` floor
    /// (4, not 2) — the floor is threaded through one running length in
    /// depth-first leaf order, so this is the case an iterative rewrite is
    /// most likely to get wrong.
    #[test]
    fn rdp_matches_geo_on_polygons() {
        let ring = |coords: Vec<Coord<f64>>| LineString::new(coords);
        let mut cases: Vec<Polygon<f64>> = vec![
            Polygon::new(
                ring(vec![
                    Coord { x: 0.0, y: 0.0 },
                    Coord { x: 10.0, y: 0.0 },
                    Coord { x: 10.0, y: 10.0 },
                    Coord { x: 0.0, y: 10.0 },
                    Coord { x: 0.0, y: 0.0 },
                ]),
                vec![],
            ),
            // A near-collinear ring: culling it would go under the floor.
            Polygon::new(
                ring(
                    (0..12)
                        .map(|i| Coord {
                            x: (i % 6) as f64,
                            y: 0.0,
                        })
                        .collect(),
                ),
                vec![],
            ),
        ];
        // A wobbly circle with a wobbly hole.
        for n in [8usize, 64, 512] {
            let circle = |r: f64, wob: f64| {
                ring(
                    (0..=n)
                        .map(|i| {
                            let t = i as f64 / n as f64 * std::f64::consts::TAU;
                            let rr = r + if i % 2 == 0 { wob } else { -wob };
                            Coord {
                                x: rr * t.cos(),
                                y: rr * t.sin(),
                            }
                        })
                        .collect(),
                )
            };
            cases.push(Polygon::new(circle(10.0, 0.05), vec![circle(3.0, 0.02)]));
        }

        for (c, poly) in cases.iter().enumerate() {
            for &tol in &[0.0, 1e-9, 0.01, 0.04, 0.05, 0.06, 0.5, 2.0, 100.0] {
                assert_eq!(
                    rdp_polygon(poly, tol),
                    poly.simplify(tol),
                    "polygon case {c} diverged from geo at tol {tol}"
                );
            }
        }
    }

    /// Stack budget for the #575 regression test, and the zigzag length run
    /// inside it. `geo`'s recursive RDP needs one frame per retained vertex,
    /// so this shape overflows it; the iterative one needs a constant frame
    /// and a heap `Vec`. Kept small on purpose: the shape is RDP's O(n²)
    /// worst case, so a CI-fast length matters more than a dramatic one (the
    /// real crash was a 12,001-vertex stroke on a rayon worker).
    const SMALL_STACK_BYTES: usize = 128 * 1024;
    const SMALL_STACK_ZIGZAG: usize = 4_001;

    /// #575: a coalesced stroke long enough to overflow a worker stack under
    /// the recursive RDP must simplify fine on a SMALL stack.
    ///
    /// A stack overflow aborts the process rather than panicking, so a
    /// regression here fails as a crashed test binary, not an assertion.
    #[test]
    fn long_zigzag_simplifies_on_a_small_stack() {
        let amp = 0.0002_f64;
        let ls = zigzag(SMALL_STACK_ZIGZAG, amp, 0.0004);
        let handle = std::thread::Builder::new()
            .stack_size(SMALL_STACK_BYTES)
            .spawn(move || {
                // Just below the amplitude: every vertex survives, which is
                // the deepest split tree the shape can produce.
                let out = rdp_linestring(&ls, amp * 0.99);
                (out.0.len(), ls.0.len())
            })
            .expect("spawn");
        let (kept, total) = handle.join().expect("no stack overflow, no panic");
        // Just under the amplitude, nearly every vertex is above the epsilon
        // and survives — i.e. the split tree really did go ~`total` deep,
        // which is what the recursive version could not do here.
        assert!(
            kept * 10 >= total * 9,
            "expected almost every vertex retained, kept {kept} of {total}"
        );
    }

    /// The same run through `geo`'s recursive RDP — proof that
    /// [`long_zigzag_simplifies_on_a_small_stack`] is a real regression test
    /// and not a tautology. `#[ignore]`d because it ABORTS the test process
    /// (that is the bug); run it deliberately:
    /// `cargo test --lib geo_rdp_overflows -- --ignored`.
    #[test]
    #[ignore]
    fn geo_rdp_overflows_the_same_small_stack() {
        let amp = 0.0002_f64;
        let ls = zigzag(SMALL_STACK_ZIGZAG, amp, 0.0004);
        let handle = std::thread::Builder::new()
            .stack_size(SMALL_STACK_BYTES)
            .spawn(move || ls.simplify(amp * 0.99).0.len())
            .expect("spawn");
        println!("geo kept {} — no overflow", handle.join().unwrap());
    }

    #[test]
    #[ignore] // measurement, not an assertion (#575)
    fn probe_rdp_scaling() {
        // `geo`'s recursive side needs one frame per retained vertex, so at
        // n = 12,000 it overflows the default 2 MiB test thread (release
        // build included) and aborts the probe. Give the whole measurement a
        // stack large enough for the reference to finish.
        std::thread::Builder::new()
            .stack_size(1 << 30)
            .spawn(probe_rdp_scaling_body)
            .expect("spawn")
            .join()
            .expect("probe");
    }

    fn probe_rdp_scaling_body() {
        let amp = 0.0002_f64;
        let step = 0.0004_f64;
        for n in [1500usize, 3000, 6000, 12000] {
            let ls = zigzag(n, amp, step);
            for f in [0.5, 0.99, 1.01, 2.0] {
                let tol = amp * f;
                let t = Instant::now();
                let ours = rdp_linestring(&ls, tol);
                let ms_ours = t.elapsed().as_secs_f64() * 1e3;
                let t = Instant::now();
                let theirs = ls.simplify(tol);
                let ms_geo = t.elapsed().as_secs_f64() * 1e3;
                assert_eq!(ours, theirs);
                println!(
                    "n={n} tol=amp*{f} kept={} iterative {ms_ours:.2} ms vs geo {ms_geo:.2} ms",
                    ours.0.len()
                );
            }
        }
    }

    // ---- hostile differential harness (adversarial review of #578) --------

    /// Bitwise identity of two coordinate lists (`==` on `f64` is false for
    /// NaN, which would hide or fake a divergence on NaN inputs).
    fn same_bits(a: &[Coord<f64>], b: &[Coord<f64>]) -> bool {
        a.len() == b.len()
            && a.iter()
                .zip(b)
                .all(|(p, q)| p.x.to_bits() == q.x.to_bits() && p.y.to_bits() == q.y.to_bits())
    }

    /// Run `f` and capture a panic as `None`. `geo` and [`rdp_coords`] both
    /// carry the same `debug_assert` on an all-NaN interior (no distance is
    /// `>= 0.0`), so in a debug build "both panic" is agreement too.
    fn outcome<F: FnOnce() -> Vec<Coord<f64>>>(f: F) -> Option<Vec<Coord<f64>>> {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
    }

    /// Differential check of both floors: the open-line floor against
    /// `LineString::simplify`, the ring floor against `Polygon::simplify`
    /// (whose exterior is the same coordinates, closed by `Polygon::new` on
    /// both sides).
    fn assert_matches_geo(coords: &[Coord<f64>], tol: f64) -> Result<(), String> {
        let ls = LineString::new(coords.to_vec());
        let ours = outcome(|| rdp_linestring(&ls, tol).0);
        let geo_out = outcome(|| ls.simplify(tol).0);
        match (&ours, &geo_out) {
            (Some(a), Some(b)) if same_bits(a, b) => {}
            (None, None) => {}
            _ => {
                return Err(format!(
                    "line floor diverged at tol {tol} on {coords:?}: ours {ours:?}, geo {geo_out:?}"
                ))
            }
        }
        let poly = Polygon::new(ls.clone(), vec![]);
        let ours = outcome(|| rdp_polygon(&poly, tol).exterior().0.clone());
        let geo_out = outcome(|| poly.simplify(tol).exterior().0.clone());
        match (&ours, &geo_out) {
            (Some(a), Some(b)) if same_bits(a, b) => Ok(()),
            (None, None) => Ok(()),
            _ => Err(format!(
                "ring floor diverged at tol {tol} on {coords:?}: ours {ours:?}, geo {geo_out:?}"
            )),
        }
    }

    fn hostile_tol() -> impl proptest::strategy::Strategy<Value = f64> {
        use proptest::prelude::*;
        prop_oneof![
            4 => 0.0f64..4.0,
            2 => (0u8..9).prop_map(|k| f64::from(k) * 0.5),
            1 => Just(0.0),
            1 => Just(-1.0),
            1 => Just(f64::NAN),
            1 => Just(f64::INFINITY),
            1 => Just(f64::MIN_POSITIVE),
        ]
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2048))]

        /// Small integer grids: dense with exact distance ties, coincident
        /// and duplicate-consecutive vertices, collinear runs and closed
        /// rings — where a changed tie-break or floor order would show.
        #[test]
        fn rdp_matches_geo_on_integer_grids(
            pts in proptest::collection::vec((-3i8..=3, -3i8..=3), 0..40),
            dup_every in 0usize..4,
            close in proptest::bool::ANY,
            tol in hostile_tol(),
        ) {
            let mut coords: Vec<Coord<f64>> = Vec::new();
            for (i, (x, y)) in pts.iter().enumerate() {
                let c = Coord { x: f64::from(*x), y: f64::from(*y) };
                coords.push(c);
                if dup_every > 0 && i % dup_every == 0 {
                    coords.push(c);
                }
            }
            if close {
                if let Some(first) = coords.first().copied() {
                    coords.push(first);
                }
            }
            if let Err(e) = assert_matches_geo(&coords, tol) {
                return Err(proptest::test_runner::TestCaseError::fail(e));
            }
        }

        /// Arbitrary finite and non-finite coordinates: NaN and ±inf
        /// vertices and endpoints, huge magnitudes, signed zeros.
        #[test]
        fn rdp_matches_geo_on_non_finite_coords(
            pts in proptest::collection::vec(
                (
                    proptest::prop_oneof![
                        6 => -1.0e6f64..1.0e6,
                        1 => proptest::prelude::Just(f64::NAN),
                        1 => proptest::prelude::Just(f64::INFINITY),
                        1 => proptest::prelude::Just(f64::NEG_INFINITY),
                        1 => proptest::prelude::Just(-0.0f64),
                        1 => proptest::prelude::Just(f64::MAX),
                    ],
                    -1.0e6f64..1.0e6,
                ),
                0..24,
            ),
            tol in hostile_tol(),
        ) {
            let coords: Vec<Coord<f64>> =
                pts.iter().map(|&(x, y)| Coord { x, y }).collect();
            if let Err(e) = assert_matches_geo(&coords, tol) {
                return Err(proptest::test_runner::TestCaseError::fail(e));
            }
        }
    }

    /// Deterministic edge shapes the property tests may not land on.
    #[test]
    fn rdp_matches_geo_on_edge_shapes() {
        let c = |x: f64, y: f64| Coord { x, y };
        let shapes: Vec<Vec<Coord<f64>>> = vec![
            vec![],
            vec![c(0.0, 0.0)],
            vec![c(0.0, 0.0), c(0.0, 0.0)],
            vec![c(0.0, 0.0), c(1.0, 1.0), c(0.0, 0.0)],
            vec![c(0.0, 0.0), c(1.0, 0.0), c(2.0, 0.0)],
            // Every interior vertex exactly equidistant: the last-maximum
            // tie-break decides the split.
            vec![
                c(0.0, 0.0),
                c(1.0, 1.0),
                c(2.0, 1.0),
                c(3.0, 1.0),
                c(4.0, 0.0),
            ],
            // Closed square, and a closed ring degenerate to one point.
            vec![
                c(0.0, 0.0),
                c(1.0, 0.0),
                c(1.0, 1.0),
                c(0.0, 1.0),
                c(0.0, 0.0),
            ],
            vec![c(5.0, 5.0); 6],
            // NaN endpoint (every chord distance is NaN) and NaN interior.
            vec![c(f64::NAN, 0.0), c(1.0, 1.0), c(2.0, 0.0)],
            vec![c(0.0, 0.0), c(f64::NAN, f64::NAN), c(2.0, 0.0), c(3.0, 5.0)],
            vec![c(0.0, 0.0), c(f64::INFINITY, 1.0), c(2.0, 0.0), c(3.0, 1.0)],
        ];
        for s in &shapes {
            for &tol in &[-1.0, 0.0, 0.5, 1.0, 1.5, f64::NAN, f64::INFINITY] {
                assert_matches_geo(s, tol).unwrap();
            }
        }
    }

    /// Million-vertex strokes, compared against `geo` on a 1 GiB stack (so
    /// the reference itself survives its recursion). Shapes whose split
    /// tree is tractable at this size: a random walk (shallow splits), a
    /// zigzag at a tolerance above its amplitude (one scan culls it all),
    /// and a jittered straight line (culled everywhere but the floor).
    #[test]
    fn rdp_matches_geo_on_million_vertex_strokes() {
        let big = std::thread::Builder::new()
            .stack_size(1 << 30)
            .spawn(|| {
                let n = 1_000_000;
                let walk = noisy_line(n, 7, 3.0);
                let zig = zigzag(n, 0.0002, 0.0004);
                let flat = noisy_line(n, 99, 1e-9);
                for (ls, tol) in [(&walk, 2.0), (&walk, 50.0), (&zig, 0.00021), (&flat, 1e-3)] {
                    let ours = rdp_linestring(ls, tol);
                    let theirs = ls.simplify(tol);
                    assert!(
                        same_bits(&ours.0, &theirs.0),
                        "diverged on an n={} stroke at tol {tol}",
                        ls.0.len()
                    );
                }
            })
            .expect("spawn");
        big.join().expect("no divergence");
    }

    /// #575 end to end: the production entry point (not just the RDP
    /// kernel) must simplify a stroke that is too deep for recursion on a
    /// small stack, so reverting either call site to `geo::Simplify` fails
    /// here. Covers the line path and the polygon-ring path.
    #[test]
    fn simplify_for_level_handles_deep_strokes_on_a_small_stack() {
        let amp = 0.0002_f64;
        let ls = zigzag(SMALL_STACK_ZIGZAG, amp, 0.0004);
        let mut ring = ls.0.clone();
        // Close the zigzag into a thin ring: down, back, and home.
        let last_x = ring.last().unwrap().x;
        ring.push(Coord { x: last_x, y: -1.0 });
        ring.push(Coord { x: 0.0, y: -1.0 });
        ring.push(ring[0]);
        let line = Geometry::LineString(ls);
        let poly = Geometry::Polygon(Polygon::new(LineString::new(ring), vec![]));
        let opts = SimplifyOptions {
            cascade: false,
            ..SimplifyOptions::default()
        };
        // 3857 meters verbatim: tolerance = factor * gsd, just under `amp`.
        let gsd = amp * 0.99;
        let handle = std::thread::Builder::new()
            .stack_size(SMALL_STACK_BYTES)
            .spawn(move || {
                let l = simplify_for_level(&line, gsd, Crs::Epsg3857, &opts);
                let p = simplify_for_level(&poly, gsd, Crs::Epsg3857, &opts);
                (l, p)
            })
            .expect("spawn");
        let (line_out, poly_out) = handle.join().expect("no stack overflow, no panic");
        match line_out {
            Simplified::Keep(Geometry::LineString(out)) => assert!(
                out.0.len() * 10 >= SMALL_STACK_ZIGZAG * 9,
                "expected almost every vertex retained, kept {}",
                out.0.len()
            ),
            other => panic!("line should survive as a LineString, got {other:?}"),
        }
        match poly_out {
            Simplified::Keep(Geometry::Polygon(p)) => assert!(
                p.exterior().0.len() * 10 >= SMALL_STACK_ZIGZAG * 9,
                "expected almost every ring vertex retained, kept {}",
                p.exterior().0.len()
            ),
            other => panic!("ring should survive as a Polygon, got {other:?}"),
        }
    }
}
