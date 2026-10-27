//! `--max-zoom auto` (#444): pick the finest zoom from the data, inspired by
//! tippecanoe's `-zg`.
//!
//! Like `-zg`, the estimate combines two signals and lets the finer one win:
//!
//! - **typical extent**: the median bbox diagonal over sampled features with a
//!   positive, finite extent (lines/polygons; points carry none and are left
//!   out of the median rather than dragging it to zero).
//! - **nearby spacing**: sampled feature centers are Morton-sorted and the
//!   10th percentile of consecutive distinct-center gaps is taken — a
//!   "denser than typical" spacing — corrected for the subsample's thinning.
//!
//! `resolvable = min(extent, spacing)` over whichever signals exist, and the
//! chosen zoom is the finest whose standard 256 px tile pixel is no larger
//! than `resolvable / 2` ("one zoom beyond what is strictly necessary",
//! tippecanoe's own phrase), clamped to `[min_zoom, AUTO_MAX_ZOOM_CEILING]`.
//! Distances are Web Mercator meters (EPSG:4326 latitude deltas are scaled by
//! `1 / cos(latitude)`), the same space tile pixels are measured in.
//!
//! With neither signal (a single location, or only empty geometries) the
//! estimate fails with [`ConvertError::AutoZoomNoSignal`], as `-zg` does.
//!
//! The estimate honors `--bbox` and `--filter` exactly as the conversion does
//! (footer-statistics row-group pruning, then the per-feature tests), reads
//! only ONE bbox-bearing column (plus the filter's columns), and computes a
//! bbox for at most 200,000 systematically sampled rows
//! (`AUTO_ZOOM_SAMPLE_CAP`) — non-sampled rows are never converted.
//!
//! Which column that is depends on the input (#693). When the file declares a
//! GeoParquet 1.1 bbox covering whose four fields are all `f64`
//! (`find_f64_covering`), the sample projects ONLY the covering struct and
//! takes each sampled row's bbox from it — no geometry bytes are read at all,
//! which on gpio-optimized input (the documented input contract) turns the
//! pre-conversion read from the file's dominant column into its smallest.
//! Otherwise the geometry column is projected and the bbox is computed
//! straight from the GeoArrow scalars. Both signals are pure functions of the
//! f64 bboxes, so the two paths resolve the IDENTICAL zoom for the same rows;
//! an `f32` covering (spec-legal; Overture writes one) is rounded relative to
//! the geometry-derived f64 bboxes and is therefore NOT used. A null covering
//! value skips the row, exactly as a null geometry does — the spec ties the
//! two ("a row MUST contain a bounding box value if and only if the row
//! contains a geometry value").
//!
//! Either way that bounds memory and geometry work, not I/O: the selected
//! row groups' sampled column is still read once.
//!
//! How this differs from tippecanoe's method (4096-unit resolution, vertex
//! spacing, quadkey deltas) and why the whole-file footer statistics are not
//! used is recorded in the divergence table of `context/ARCHITECTURE.md`.

use std::fmt;
use std::str::FromStr;

use arrow_array::{Array, ArrayRef, Float64Array, StructArray};
use geo_traits::to_geo::ToGeoGeometry;
use geo_traits::{
    CoordTrait, GeometryCollectionTrait, GeometryTrait, GeometryType, LineStringTrait, LineTrait,
    MultiLineStringTrait, MultiPointTrait, MultiPolygonTrait, PointTrait, PolygonTrait, RectTrait,
    TriangleTrait,
};
use geoarrow::array::from_arrow_array;
use geoarrow_array::GeoArrowArrayAccessor;

use crate::batch_processor::{visit_geoarrow_array, GeoArrowVisitor};
use crate::covering::find_f64_covering;
use crate::input_set::{ConvertSource, ReadPlan};
use crate::tile::MAX_LATITUDE;

use super::convert::{
    detect_crs_from_kv, find_geometry_column, validate_options, BboxQuery, ConvertError,
    ConvertOptions, LevelPlan,
};
use super::filter::{parse_filter, BoundFilter};
use super::level::{Crs, WEBMERC_CIRCUMFERENCE_M};

/// Ceiling for `--max-zoom auto` (issue #444).
///
/// The estimate is "never above a documented ceiling". tippecanoe's own `-zg` cap is `32 - full_detail` (z20 at
/// defaults); 16 keeps an auto-guessed archive from silently becoming
/// enormous while still covering building- and address-level detail.
pub const AUTO_MAX_ZOOM_CEILING: u8 = 16;

/// Upper bound on how many rows the estimator converts to a bbox, whatever
/// the input size — `O(sample)` memory and geometry work.
pub(crate) const AUTO_ZOOM_SAMPLE_CAP: usize = 200_000;

/// Tile width, in pixels, the resolvable distance is compared against: the
/// standard 256 px web-map tile. tippecanoe instead resolves at its full
/// 4096-unit tile extent (`ceil(log2(360 / want_deg) - full_detail)`,
/// `full_detail = 12`), which would put the same distance four zooms finer;
/// see the ARCHITECTURE.md divergence row. Deliberately not
/// [`super::level::GSD_TILE_BASE`] (1024), which calibrates simplification
/// tolerance, a different question.
const AUTO_ZOOM_TILE_PIXELS: f64 = 256.0;

/// Rows per record batch while sampling (one bbox-bearing column only: the
/// covering struct, or the geometry).
const AUTO_ZOOM_BATCH_ROWS: usize = 8192;

/// A `--max-zoom` value: an explicit zoom, or `auto` (#444).
///
/// Parse with [`FromStr`] (`"14"`, `"auto"` in any case), build the level plan
/// with [`MaxZoom::plan_zoom`], and turn `auto` into a zoom with
/// [`MaxZoom::resolve`] once the cheap validation has run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxZoom {
    /// An explicit zoom, used verbatim.
    Fixed(u8),
    /// Estimate the zoom from the input (module docs).
    Auto,
}

impl From<u8> for MaxZoom {
    fn from(z: u8) -> Self {
        MaxZoom::Fixed(z)
    }
}

impl FromStr for MaxZoom {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.trim().eq_ignore_ascii_case("auto") {
            return Ok(MaxZoom::Auto);
        }
        s.trim()
            .parse::<u8>()
            .map(MaxZoom::Fixed)
            .map_err(|_| format!("invalid max zoom {s:?}: expected a zoom number or \"auto\""))
    }
}

impl fmt::Display for MaxZoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MaxZoom::Fixed(z) => write!(f, "{z}"),
            MaxZoom::Auto => f.write_str("auto"),
        }
    }
}

impl MaxZoom {
    /// Whether this is `auto`.
    pub fn is_auto(self) -> bool {
        matches!(self, MaxZoom::Auto)
    }

    /// The zoom to build the level plan (and run cheap validation) with
    /// before `auto` is resolved: the fixed value, or
    /// [`AUTO_MAX_ZOOM_CEILING`] for `auto` — the most permissive value it can
    /// resolve to, so the placeholder never rejects anything the real value
    /// would accept.
    pub fn plan_zoom(self) -> u8 {
        match self {
            MaxZoom::Fixed(z) => z,
            MaxZoom::Auto => AUTO_MAX_ZOOM_CEILING,
        }
    }

    /// Resolve to a concrete zoom for a conversion of `source` under
    /// `options`, whose level plan was built with [`MaxZoom::plan_zoom`].
    ///
    /// - `Fixed(z)` returns `z`: no I/O, `options` untouched.
    /// - `Auto` with an explicit GSD plan returns the placeholder without
    ///   estimating (the GSD ladder sets the levels, so the value is unused).
    /// - `Auto` with a zoom range fails fast on a `min_zoom` above the
    ///   ceiling, then runs the same up-front validation the conversion runs
    ///   (so a bad option still fails in milliseconds, #371), then checks
    ///   that `output` (when given) can be written (#452), then estimates
    ///   over `source` honoring `options.bbox` / `options.filter`, logs the
    ///   pick and its evidence, and writes the chosen zoom into
    ///   `options.levels`.
    ///
    /// `source` is only read (its column projection is left alone), so it may
    /// be the one the conversion then reads. `output` is the conversion's
    /// output, which the conversion checks again; pass `None` when there is
    /// none (a plan-only run) or the caller has already checked it.
    ///
    /// # Errors
    ///
    /// Returns a [`ConvertError`] if `min_zoom` is above the ceiling, if
    /// `options` fail the conversion's up-front validation, if `output`
    /// cannot be written, or if [`estimate_max_zoom`] fails.
    pub fn resolve(
        self,
        source: &ConvertSource,
        options: &mut ConvertOptions,
        output: Option<&std::path::Path>,
    ) -> Result<u8, ConvertError> {
        let MaxZoom::Auto = self else {
            return Ok(self.plan_zoom());
        };
        let LevelPlan::ZoomRange { min_zoom, .. } = options.levels else {
            log::info!("--max-zoom auto: ignored, the explicit GSD list sets the levels");
            return Ok(self.plan_zoom());
        };
        check_auto_min_zoom(min_zoom, AUTO_MAX_ZOOM_CEILING)?;
        validate_options(options)?;
        if let Some(output) = output {
            crate::atomic_output::check_writable(output)?;
        }
        let evidence = estimate_max_zoom(source, options)?;
        evidence.log();
        options.levels = LevelPlan::ZoomRange {
            min_zoom,
            max_zoom: evidence.chosen_zoom,
        };
        Ok(evidence.chosen_zoom)
    }
}

/// The chosen zoom plus the measurements that produced it, so a user can
/// second-guess the pick (issue #444: "log the chosen zoom and its evidence").
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct AutoZoomEvidence {
    /// Input rows in the row groups the estimate read: after `--bbox` /
    /// `--filter` footer-statistics pruning, before the per-feature bbox and
    /// filter tests — so an upper bound on the features the conversion keeps.
    pub rows_read: u64,
    /// Features that were sampled and passed the per-feature bbox / filter
    /// tests with a usable (non-empty, finite) geometry.
    pub sampled_count: usize,
    /// Median bbox diagonal over sampled features with a positive extent, in
    /// Web Mercator meters. `0.0` when no feature has an extent (points).
    pub typical_extent_m: f64,
    /// 10th-percentile Morton-order gap between distinct sampled centers, in
    /// Web Mercator meters, corrected for the sample fraction.
    /// `f64::INFINITY` when fewer than two distinct locations were sampled.
    pub nearby_spacing_m: f64,
    /// `min(typical_extent_m, nearby_spacing_m)` over the available signals.
    pub resolvable_m: f64,
    /// The zoom this evidence resolved to.
    pub chosen_zoom: u8,
    /// The floor `chosen_zoom` was clamped to.
    pub min_zoom: u8,
    /// The ceiling `chosen_zoom` was clamped to.
    pub ceiling: u8,
    /// Whether the sampled bboxes were read from the file's f64 bbox covering
    /// struct instead of the geometry scalars (#693). Diagnostic only — the
    /// two paths resolve the identical zoom for the same rows.
    pub(crate) sampled_from_covering: bool,
}

impl AutoZoomEvidence {
    /// Log the pick (issue #444) so a user can see — and question — the
    /// inputs that drove it.
    pub(crate) fn log(&self) {
        log::info!(
            "--max-zoom auto: chose z{} (clamped to [{}, {}]) from {} sampled features of {} \
             rows read; typical feature extent ~{:.1}m, nearby spacing ~{:.1}m, resolvable \
             distance ~{:.1}m (Web Mercator meters){}",
            self.chosen_zoom,
            self.min_zoom,
            self.ceiling,
            self.sampled_count,
            self.rows_read,
            self.typical_extent_m,
            self.nearby_spacing_m,
            self.resolvable_m,
            if self.sampled_from_covering {
                "; bboxes read from the covering column (#693)"
            } else {
                ""
            },
        );
    }
}

/// `min_zoom` must leave `auto` a zoom to pick.
fn check_auto_min_zoom(min_zoom: u8, ceiling: u8) -> Result<(), ConvertError> {
    if min_zoom > ceiling {
        return Err(ConvertError::AutoZoomMinAboveCeiling { min_zoom, ceiling });
    }
    Ok(())
}

/// Nearest-rank percentile (`p` in `[0, 1]`) of a non-empty `values`, which
/// are reordered in place (a selection, not a full sort). `total_cmp` keeps a
/// stray NaN from panicking it (callers only pass finite values anyway).
pub(crate) fn percentile(values: &mut [f64], p: f64) -> f64 {
    debug_assert!(!values.is_empty());
    let idx = (((values.len() - 1) as f64) * p).round() as usize;
    *values.select_nth_unstable_by(idx, f64::total_cmp).1
}

/// Length in Web Mercator meters of a `(dx, dy)` delta in the file's native
/// CRS units, taken at latitude `lat_deg` (ignored for EPSG:3857, whose
/// units already are Web Mercator meters). For EPSG:4326 a degree of
/// longitude is a fixed `circumference / 360` Mercator meters, while a degree
/// of latitude stretches by `1 / cos(latitude)` — the local Mercator scale.
pub(crate) fn mercator_delta_m(dx: f64, dy: f64, lat_deg: f64, crs: Crs) -> f64 {
    match crs {
        Crs::Epsg3857 => dx.hypot(dy),
        Crs::Epsg4326 => {
            let per_deg = WEBMERC_CIRCUMFERENCE_M / 360.0;
            let cos = lat_deg
                .clamp(-MAX_LATITUDE, MAX_LATITUDE)
                .to_radians()
                .cos();
            (dx * per_deg).hypot(dy * per_deg / cos)
        }
    }
}

/// Spread `x`'s low 32 bits so each occupies every other bit of a `u64` —
/// the bit-interleave building block of a 2D Morton (Z-order) code.
fn spread_bits(x: u32) -> u64 {
    let mut x = x as u64;
    x = (x | (x << 16)) & 0x0000_FFFF_0000_FFFF;
    x = (x | (x << 8)) & 0x00FF_00FF_00FF_00FF;
    x = (x | (x << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
    x = (x | (x << 2)) & 0x3333_3333_3333_3333;
    x = (x | (x << 1)) & 0x5555_5555_5555_5555;
    x
}

/// Morton (Z-order) code of `(cx, cy)`, quantized against `crs`'s fixed
/// global coordinate range (not the data's), so the code is a pure function
/// of one point.
pub(crate) fn morton_key(cx: f64, cy: f64, crs: Crs) -> u64 {
    let half = WEBMERC_CIRCUMFERENCE_M / 2.0;
    let (lo_x, hi_x, lo_y, hi_y) = match crs {
        Crs::Epsg4326 => (-180.0, 180.0, -90.0, 90.0),
        Crs::Epsg3857 => (-half, half, -half, half),
    };
    let qx = (((cx - lo_x) / (hi_x - lo_x)).clamp(0.0, 1.0) * u32::MAX as f64) as u32;
    let qy = (((cy - lo_y) / (hi_y - lo_y)).clamp(0.0, 1.0) * u32::MAX as f64) as u32;
    spread_bits(qx) | (spread_bits(qy) << 1)
}

/// Core estimator (pure — no I/O), given sampled bbox diagonals and
/// Morton-order consecutive-center gaps, both in Web Mercator meters.
///
/// Only positive, finite diagonals and gaps count: a point's zero diagonal
/// or a duplicate location's zero gap is "no signal", not "zero meters".
/// `sample_fraction` (the chance any row was sampled) corrects the spacing
/// percentile for subsample thinning: a systematic subsample of a 2D point
/// process stretches the apparent gap by `1 / sqrt(fraction)`.
pub(crate) fn choose_auto_max_zoom(
    rows_read: u64,
    sampled_count: usize,
    diags_m: Vec<f64>,
    gaps_m: Vec<f64>,
    sample_fraction: f64,
    min_zoom: u8,
    ceiling: u8,
) -> Result<AutoZoomEvidence, ConvertError> {
    check_auto_min_zoom(min_zoom, ceiling)?;
    let positive = |v: Vec<f64>| -> Vec<f64> {
        v.into_iter()
            .filter(|d| d.is_finite() && *d > 0.0)
            .collect()
    };
    let mut diags_m = positive(diags_m);
    let mut gaps_m = positive(gaps_m);

    let extent_signal = (!diags_m.is_empty()).then(|| percentile(&mut diags_m, 0.5));
    let spacing_signal = (!gaps_m.is_empty())
        .then(|| percentile(&mut gaps_m, 0.10) * sample_fraction.clamp(0.0, 1.0).sqrt())
        .filter(|s| *s > 0.0);

    let resolvable_m = match (extent_signal, spacing_signal) {
        (Some(e), Some(s)) => e.min(s),
        (Some(e), None) => e,
        (None, Some(s)) => s,
        (None, None) => {
            return Err(ConvertError::AutoZoomNoSignal {
                rows: rows_read,
                sampled: sampled_count,
            })
        }
    };

    let want = resolvable_m / 2.0;
    let z = (WEBMERC_CIRCUMFERENCE_M / AUTO_ZOOM_TILE_PIXELS / want)
        .log2()
        .ceil();
    // A subnormal `want` gives `+inf`; `as i64` would saturate anyway, but
    // say what is meant.
    let z = if z.is_finite() { z } else { f64::from(ceiling) };
    // `min_zoom <= ceiling` was checked above, so the clamp cannot panic.
    let chosen_zoom = (z as i64).clamp(i64::from(min_zoom), i64::from(ceiling)) as u8;

    Ok(AutoZoomEvidence {
        rows_read,
        sampled_count,
        typical_extent_m: extent_signal.unwrap_or(0.0),
        nearby_spacing_m: spacing_signal.unwrap_or(f64::INFINITY),
        resolvable_m,
        chosen_zoom,
        min_zoom,
        ceiling,
        sampled_from_covering: false,
    })
}

/// Running `[xmin, ymin, xmax, ymax]`; `None` once a non-finite coordinate is
/// seen (the feature is unusable, exactly as pass 1 treats it).
struct BboxAcc {
    b: [f64; 4],
    any: bool,
    finite: bool,
}

impl BboxAcc {
    fn new() -> Self {
        BboxAcc {
            b: [
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ],
            any: false,
            finite: true,
        }
    }

    fn add(&mut self, c: impl CoordTrait<T = f64>) {
        let (x, y) = (c.x(), c.y());
        if !x.is_finite() || !y.is_finite() {
            self.finite = false;
            return;
        }
        self.any = true;
        self.b = [
            self.b[0].min(x),
            self.b[1].min(y),
            self.b[2].max(x),
            self.b[3].max(y),
        ];
    }

    fn add_line(&mut self, ls: &impl LineStringTrait<T = f64>) {
        ls.coords().for_each(|c| self.add(c));
    }

    fn add_polygon(&mut self, p: &impl PolygonTrait<T = f64>) {
        // Interior rings lie inside the exterior: it alone bounds the polygon.
        if let Some(ring) = p.exterior() {
            self.add_line(&ring);
        }
    }

    fn add_geometry(&mut self, g: &impl GeometryTrait<T = f64>) {
        match g.as_type() {
            GeometryType::Point(p) => {
                if let Some(c) = p.coord() {
                    self.add(c);
                }
            }
            GeometryType::LineString(ls) => self.add_line(ls),
            GeometryType::Polygon(p) => self.add_polygon(p),
            GeometryType::MultiPoint(mp) => {
                for p in mp.points() {
                    if let Some(c) = p.coord() {
                        self.add(c);
                    }
                }
            }
            GeometryType::MultiLineString(ml) => ml.line_strings().for_each(|l| self.add_line(&l)),
            GeometryType::MultiPolygon(mp) => mp.polygons().for_each(|p| self.add_polygon(&p)),
            GeometryType::GeometryCollection(gc) => {
                gc.geometries().for_each(|g| self.add_geometry(&g))
            }
            GeometryType::Rect(r) => {
                self.add(r.min());
                self.add(r.max());
            }
            GeometryType::Triangle(t) => {
                self.add(t.first());
                self.add(t.second());
                self.add(t.third());
            }
            GeometryType::Line(l) => {
                self.add(l.start());
                self.add(l.end());
            }
        }
    }

    fn finish(self) -> Option<[f64; 4]> {
        (self.any && self.finite).then_some(self.b)
    }
}

/// The bbox of each row in `rows` (batch-local indices), read through the
/// typed GeoArrow scalar without converting it to a `geo::Geometry`. `None`
/// for a null, empty or non-finite geometry.
struct SampledBboxes<'r> {
    rows: &'r [usize],
}

impl GeoArrowVisitor for SampledBboxes<'_> {
    type Output = Vec<Option<[f64; 4]>>;

    fn visit<'a, A>(self, accessor: &'a A) -> crate::Result<Self::Output>
    where
        A: GeoArrowArrayAccessor<'a>,
        A::Item: ToGeoGeometry<f64>,
    {
        self.rows
            .iter()
            .map(|&i| {
                let item = accessor.get(i).map_err(|e| {
                    crate::Error::GeoParquetRead(format!("Invalid geometry at index {i}: {e}"))
                })?;
                Ok(item.and_then(|g| {
                    let mut acc = BboxAcc::new();
                    acc.add_geometry(&g);
                    acc.finish()
                }))
            })
            .collect()
    }
}

/// The bbox of each row in `rows` (batch-local indices), read from the f64
/// covering struct `col` with child fields `names` (`xmin, ymin, xmax, ymax`
/// order). `None` — the row is skipped, exactly as the geometry path skips a
/// null/empty/non-finite geometry — for a null struct entry or child value
/// (the spec ties bbox nulls to geometry nulls) or a non-finite bound.
///
/// An inverted envelope is **swapped, not skipped**. `xmin > xmax` is the
/// GeoParquet wraparound form a spec-aware producer writes for an
/// antimeridian-crossing feature, and the geometry path has no inverted
/// case at all: [`BboxAcc`] is a min/max over the coordinates, so the same
/// feature yields a valid, ~358-degree-wide planar envelope there and is
/// counted. Swapping reproduces exactly that envelope (179, -179 becomes
/// -179, 179), which keeps the two paths measuring the same population —
/// skipping would drop the row from one path only, and a Fiji- or
/// Chukotka-only extract where every feature crosses would sample nothing
/// and fail with `AutoZoomNoSignal` on a file the geometry path reads
/// fine. An inverted `ymin > ymax` is not a legal wraparound, just
/// malformed, and gets the same charitable planar reading.
fn covering_bboxes(
    col: &ArrayRef,
    names: &[String; 4],
    rows: &[usize],
) -> crate::Result<Vec<Option<[f64; 4]>>> {
    let strukt = col.as_any().downcast_ref::<StructArray>().ok_or_else(|| {
        crate::Error::GeoParquetRead("bbox covering column is not a struct".into())
    })?;
    let field = |name: &String| -> crate::Result<&Float64Array> {
        strukt
            .column_by_name(name)
            .and_then(|c| c.as_any().downcast_ref::<Float64Array>())
            .ok_or_else(|| {
                crate::Error::GeoParquetRead(format!("bbox covering field {name} is not Float64"))
            })
    };
    let [xmin, ymin, xmax, ymax] = [
        field(&names[0])?,
        field(&names[1])?,
        field(&names[2])?,
        field(&names[3])?,
    ];
    Ok(rows
        .iter()
        .map(|&i| {
            if strukt.is_null(i)
                || xmin.is_null(i)
                || ymin.is_null(i)
                || xmax.is_null(i)
                || ymax.is_null(i)
            {
                return None;
            }
            let (x0, y0, x1, y1) = (xmin.value(i), ymin.value(i), xmax.value(i), ymax.value(i));
            if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
                return None;
            }
            // Swap rather than skip: see the note above.
            Some([x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)])
        })
        .collect())
}

/// One sampling pass's signals: a mercator diagonal and a center per
/// sampled feature, in sample order (the caller sorts the centers).
struct SampledSignals {
    diags_m: Vec<f64>,
    centers: Vec<(f64, f64)>,
}

/// Estimate `--max-zoom auto` for a conversion of `source` under `options`.
///
/// The module docs describe the method. The minimum zoom comes from
/// `options.levels` (which must be a zoom range), and `options.bbox` /
/// `options.filter` scope the sample exactly as they scope the conversion.
/// [`MaxZoom::resolve`] is the usual entry point; this one returns the
/// evidence without logging it or touching `options`.
///
/// `source` is only read (its column projection is left alone), so it may be
/// the one the conversion then reads.
///
/// # Errors
///
/// Returns a [`ConvertError`] if `options.levels` is not a zoom range, if
/// `min_zoom` is above the ceiling, if the input has no geometry column, an
/// unsupported CRS, or a filter that does not parse or bind, if reading the
/// sample fails, or if the sample holds nothing to measure.
pub fn estimate_max_zoom(
    source: &ConvertSource,
    options: &ConvertOptions,
) -> Result<AutoZoomEvidence, ConvertError> {
    options.validate()?;
    let LevelPlan::ZoomRange { min_zoom, .. } = options.levels else {
        return Err(ConvertError::InvalidConfig(
            "--max-zoom auto needs a zoom-range level plan, not an explicit GSD list".to_string(),
        ));
    };
    estimate_with(
        source,
        min_zoom,
        AUTO_MAX_ZOOM_CEILING,
        options.bbox.as_ref(),
        options.filter.as_deref(),
    )
}

/// [`estimate_max_zoom`] with its inputs spelled out (tests pin the ceiling).
pub(crate) fn estimate_with(
    source: &ConvertSource,
    min_zoom: u8,
    ceiling: u8,
    bbox: Option<&[f64; 4]>,
    filter: Option<&str>,
) -> Result<AutoZoomEvidence, ConvertError> {
    check_auto_min_zoom(min_zoom, ceiling)?;
    let schema = source.schema()?;
    let crs = detect_crs_from_kv(
        source.key_value_metadata()?.as_ref(),
        &source.display_name(),
    )?;
    let geom_idx = find_geometry_column(&schema).ok_or_else(|| ConvertError::NoGeometryColumn {
        input: source.display_name(),
    })?;
    let geom_field = schema.field(geom_idx).clone();
    // Bound against the file's own column names: a filter naming a column
    // the conversion renames (#288) addresses it by that same input name.
    let bound_filter = filter
        .map(|src| Ok::<_, ConvertError>(BoundFilter::bind(&parse_filter(src)?, &schema, &[])?))
        .transpose()?;
    let bbox_query = bbox.map(|b| BboxQuery::new(b, crs));

    // The conversion's footer-statistics pruning (#102 bbox, #315 filter).
    let bbox_sel = bbox_query
        .as_ref()
        .map(|q| source.select_row_groups_any(q.boxes()))
        .transpose()?;
    let filter_sel = bound_filter
        .as_ref()
        .map(|f| source.select_row_groups_matching(f))
        .transpose()?;
    let selection = [bbox_sel, filter_sel]
        .into_iter()
        .flatten()
        .reduce(|a, b| a.intersect(&b));
    let rows_read = source.selected_row_count(selection.as_ref())?.max(0) as u64;
    if rows_read == 0 {
        return Err(ConvertError::NoData);
    }
    // Ceil, so the sample never exceeds the cap.
    let stride = rows_read.div_ceil(AUTO_ZOOM_SAMPLE_CAP as u64).max(1);

    // Sample bboxes from the f64 covering struct when the file declares one
    // (#693) — the geometry column is then never projected at all — else from
    // the geometry scalars.
    let declared = find_f64_covering(&schema, geom_idx);

    // One sampling pass over `source`, reading each row's bbox either from
    // the declared covering struct or from the geometry scalars.
    //
    // Factored out so the covering path can fall back to the geometry path
    // over the same source. A covering that the schema declares can still be
    // unusable in the data — every value null (a struct column created from
    // a schema template and never populated, a shape real pyarrow/ogr
    // rewrites produce), or a child whose physical type is not Float64 — and
    // committing to it from the schema alone turned a working
    // `--max-zoom auto` into a hard `AutoZoomNoSignal` whose message blames
    // "fewer than two distinct locations", naming neither the covering nor
    // the real cause, on a file whose geometries are perfectly fine.
    let sample = |covering: Option<&(usize, [String; 4])>| -> Result<SampledSignals, ConvertError> {
        let sample_idx = covering.map_or(geom_idx, |(idx, _)| *idx);
        let mut cols = vec![sample_idx];
        if let Some(f) = &bound_filter {
            cols.extend_from_slice(f.columns());
        }
        cols.sort_unstable();
        cols.dedup();
        let sample_pos = cols
            .binary_search(&sample_idx)
            .expect("the sampled bbox column is projected");
        let proj = |c: usize| cols.binary_search(&c).expect("filter column is projected");

        let stream = source.open_stream(&ReadPlan {
            batch_size: AUTO_ZOOM_BATCH_ROWS,
            projection: Some(&cols),
            row_groups: selection.as_ref(),
        })?;

        let mut diags_m: Vec<f64> = Vec::new();
        let mut centers: Vec<(f64, f64)> = Vec::new();
        let mut row: u64 = 0;
        for batch in stream {
            let batch = batch?;
            let n = batch.num_rows() as u64;
            // Global rows `row + i` with `(row + i) % stride == 0`.
            let first = (stride - row % stride) % stride;
            row += n;
            let mut picks: Vec<usize> = (first..n)
                .step_by(stride as usize)
                .map(|i| i as usize)
                .collect();
            if picks.is_empty() {
                continue;
            }
            if let Some(f) = &bound_filter {
                let mask = f.eval_mask(&batch, &proj);
                picks.retain(|&i| mask[i] == Some(true));
            }
            let bboxes = match covering {
                Some((_, names)) => covering_bboxes(batch.column(sample_pos), names, &picks)?,
                None => {
                    let garr = from_arrow_array(batch.column(sample_pos).as_ref(), &geom_field)
                        .map_err(|e| {
                            crate::Error::GeoParquetRead(format!(
                                "auto max-zoom geometry decode: {e}"
                            ))
                        })?;
                    visit_geoarrow_array(garr.as_ref(), SampledBboxes { rows: &picks })?
                }
            };
            for bb in bboxes.into_iter().flatten() {
                if bbox_query.as_ref().is_some_and(|q| !q.intersects(&bb)) {
                    continue;
                }
                let (cx, cy) = ((bb[0] + bb[2]) * 0.5, (bb[1] + bb[3]) * 0.5);
                diags_m.push(mercator_delta_m(bb[2] - bb[0], bb[3] - bb[1], cy, crs));
                centers.push((cx, cy));
            }
        }
        Ok(SampledSignals { diags_m, centers })
    };

    let (
        SampledSignals {
            diags_m,
            mut centers,
        },
        used_covering,
    ) = match declared.as_ref() {
        None => (sample(None)?, false),
        Some(cv) => match sample(Some(cv)) {
            Ok(got) if !got.centers.is_empty() => (got, true),
            // The sample is capped at AUTO_ZOOM_SAMPLE_CAP rows either way,
            // so the retry is bounded; and when the geometry path finds
            // nothing either, the resulting AutoZoomNoSignal is the honest
            // answer rather than an artifact of the sidecar.
            Ok(_) => {
                log::warn!(
                    "the declared bbox covering column yielded no usable bounds for                      any sampled row; re-sampling --max-zoom auto from the geometry                      column"
                );
                (sample(None)?, false)
            }
            Err(e) => {
                log::warn!(
                    "could not read the declared bbox covering column ({e});                      re-sampling --max-zoom auto from the geometry column"
                );
                (sample(None)?, false)
            }
        },
    };

    let sampled_count = centers.len();
    centers.sort_by_key(|&(cx, cy)| morton_key(cx, cy, crs));
    let gaps_m: Vec<f64> = centers
        .windows(2)
        .map(|w| {
            let mid_y = (w[0].1 + w[1].1) * 0.5;
            mercator_delta_m(w[1].0 - w[0].0, w[1].1 - w[0].1, mid_y, crs)
        })
        .collect();

    let mut evidence = choose_auto_max_zoom(
        rows_read,
        sampled_count,
        diags_m,
        gaps_m,
        1.0 / stride as f64,
        min_zoom,
        ceiling,
    )?;
    evidence.sampled_from_covering = used_covering;
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_set::ConvertSource;
    use geo::{Geometry, LineString, MultiPolygon, Point, Polygon};
    use std::path::Path;

    /// An `n x n` grid of points, `step_deg` degrees apart (EPSG:4326), from
    /// the origin, so latitudes stay near the equator.
    fn point_grid(n: usize, step_deg: f64) -> Vec<Option<Geometry<f64>>> {
        (0..n * n)
            .map(|i| {
                let (row, col) = (i / n, i % n);
                Some(Geometry::Point(Point::new(
                    col as f64 * step_deg,
                    row as f64 * step_deg,
                )))
            })
            .collect()
    }

    fn square(x0: f64, y0: f64, size_deg: f64) -> Geometry<f64> {
        Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (x0, y0),
                (x0 + size_deg, y0),
                (x0 + size_deg, y0 + size_deg),
                (x0, y0 + size_deg),
                (x0, y0),
            ]),
            vec![],
        ))
    }

    /// `n` square polygons, `size_deg` on a side, spaced `size_deg * 3` apart.
    fn polygon_row(n: usize, size_deg: f64) -> Vec<Option<Geometry<f64>>> {
        (0..n)
            .map(|i| Some(square(i as f64 * size_deg * 3.0, 0.0, size_deg)))
            .collect()
    }

    fn write(
        dir: &tempfile::TempDir,
        name: &str,
        geoms: &[Option<Geometry<f64>>],
    ) -> ConvertSource {
        let path = dir.path().join(name);
        super::super::testutil::write_input(&path, geoms, false, None);
        ConvertSource::resolve_path(&path).unwrap()
    }

    fn estimate(source: &ConvertSource, min_zoom: u8) -> Result<AutoZoomEvidence, ConvertError> {
        estimate_with(source, min_zoom, AUTO_MAX_ZOOM_CEILING, None, None)
    }

    /// Like [`write`], but with a bbox covering column generated and declared
    /// (the geoparquet writer's own f64 covering, what gpio writes).
    fn write_covered(
        dir: &tempfile::TempDir,
        name: &str,
        geoms: &[Option<Geometry<f64>>],
    ) -> ConvertSource {
        let path = dir.path().join(name);
        super::super::testutil::write_input(&path, geoms, true, None);
        ConvertSource::resolve_path(&path).unwrap()
    }

    /// Write a GeoParquet file whose bbox covering struct is built BY THE
    /// TEST: `f64_fields` toggles Float64 vs Float32 children, and
    /// `null_bbox_rows` leaves the covering value null at those rows whatever
    /// the geometry holds (a non-conformant writer; the spec ties bbox nulls
    /// to geometry nulls).
    fn write_manual_covering(
        dir: &tempfile::TempDir,
        name: &str,
        geoms: &[Option<Geometry<f64>>],
        f64_fields: bool,
        null_bbox_rows: &[usize],
    ) -> ConvertSource {
        use arrow_array::{ArrayRef, Float32Array, Float64Array, Int64Array, StructArray};
        use arrow_schema::{DataType, Field, Fields, Schema};
        use geo::BoundingRect;
        use geoarrow::array::GeometryBuilder;
        use geoarrow::datatypes::GeometryType as GaGeometryType;
        use geoarrow_array::GeoArrowArray;
        use geoparquet::writer::{
            GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
        };
        use std::sync::Arc;
        use tylertoo_io::test_support::ArrowWriter;
        use tylertoo_io::KeyValue;

        let n = geoms.len();
        let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
        let typ = GaGeometryType::new(Default::default());
        let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
        b.extend_from_iter(geoms.iter().map(|g| g.as_ref()));
        let geom_arr = b.finish();
        let geom_field = geom_arr.data_type().to_field("geometry", true);

        let rects: Vec<Option<geo::Rect<f64>>> = geoms
            .iter()
            .enumerate()
            .map(|(i, g)| {
                if null_bbox_rows.contains(&i) {
                    return None;
                }
                g.as_ref().and_then(|g| g.bounding_rect())
            })
            .collect();
        let field_type = if f64_fields {
            DataType::Float64
        } else {
            DataType::Float32
        };
        let mk = |pick: fn(&geo::Rect<f64>) -> f64| -> ArrayRef {
            if f64_fields {
                Arc::new(Float64Array::from(
                    rects
                        .iter()
                        .map(|r| r.as_ref().map(pick))
                        .collect::<Vec<_>>(),
                ))
            } else {
                Arc::new(Float32Array::from(
                    rects
                        .iter()
                        .map(|r| r.as_ref().map(|r| pick(r) as f32))
                        .collect::<Vec<_>>(),
                ))
            }
        };
        let children: Fields = ["xmin", "ymin", "xmax", "ymax"]
            .iter()
            .map(|n| Arc::new(Field::new(*n, field_type.clone(), true)))
            .collect();
        let arrays = vec![
            mk(|r| r.min().x),
            mk(|r| r.min().y),
            mk(|r| r.max().x),
            mk(|r| r.max().y),
        ];
        let validity = arrow_buffer::NullBuffer::from(
            rects.iter().map(Option::is_some).collect::<Vec<bool>>(),
        );
        let bbox_arr = StructArray::new(children.clone(), arrays, Some(validity));

        let schema = Arc::new(Schema::new(vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(Field::new("bbox", DataType::Struct(children), true)),
            Arc::new(geom_field),
        ]));
        let batch = arrow_array::RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(id), Arc::new(bbox_arr), geom_arr.to_array_ref()],
        )
        .unwrap();

        let gpq_options = GeoParquetWriterOptionsBuilder::default()
            .set_encoding(GeoParquetWriterEncoding::WKB)
            .build();
        let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
        let target_schema = encoder.target_schema();
        let path = dir.path().join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = ArrowWriter::try_new(file, target_schema, None).unwrap();
        writer
            .write(&encoder.encode_record_batch(&batch).unwrap())
            .unwrap();
        // Declare the covering by hand (the encoder was told not to generate
        // one, since the column is the test's own).
        let kv = encoder.into_keyvalue().unwrap();
        let mut geo: serde_json::Value =
            serde_json::from_str(kv.value.as_deref().unwrap()).unwrap();
        geo["columns"]["geometry"]["covering"] = serde_json::json!({
            "bbox": {
                "xmin": ["bbox", "xmin"], "ymin": ["bbox", "ymin"],
                "xmax": ["bbox", "xmax"], "ymax": ["bbox", "ymax"]
            }
        });
        writer.append_key_value_metadata(KeyValue::new(kv.key, geo.to_string()));
        writer.close().unwrap();
        ConvertSource::resolve_path(&path).unwrap()
    }

    /// #693 equivalence: the covering path and the geometry path are the same
    /// pure function of the same f64 bboxes, so on identical rows the whole
    /// evidence — not just the zoom — must match bit for bit.
    #[test]
    fn covering_path_matches_the_geometry_path_bit_for_bit() {
        let dir = tempfile::tempdir().unwrap();
        let mut geoms = polygon_row(40, 0.009);
        geoms.extend(point_grid(10, 0.001));
        let covered = estimate(&write_covered(&dir, "cov.parquet", &geoms), 0).unwrap();
        let plain = estimate(&write(&dir, "plain.parquet", &geoms), 0).unwrap();
        assert!(covered.sampled_from_covering, "covering path not taken");
        assert!(!plain.sampled_from_covering);
        let mut normalized = covered;
        normalized.sampled_from_covering = false;
        assert_eq!(normalized, plain);
    }

    /// Null geometries carry null covering values (the spec ties the two), so
    /// both paths skip the same rows.
    #[test]
    fn null_geometries_are_skipped_identically_on_the_covering_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut geoms = polygon_row(20, 0.009);
        geoms.insert(3, None);
        geoms.push(None);
        let covered = estimate(&write_covered(&dir, "cov.parquet", &geoms), 0).unwrap();
        let plain = estimate(&write(&dir, "plain.parquet", &geoms), 0).unwrap();
        assert!(covered.sampled_from_covering);
        assert_eq!(covered.sampled_count, 20);
        let mut normalized = covered;
        normalized.sampled_from_covering = false;
        assert_eq!(normalized, plain);
    }

    /// An f32 covering (spec-legal, Overture-style) is NOT bit-identical to
    /// the geometry-derived f64 bboxes, so it is not used: the geometry path
    /// keeps the resolved zoom identical to an uncovered read.
    #[test]
    fn f32_covering_falls_back_to_the_geometry_scalars() {
        let dir = tempfile::tempdir().unwrap();
        let geoms = polygon_row(30, 0.009);
        let f32_cov = write_manual_covering(&dir, "f32.parquet", &geoms, false, &[]);
        let evidence = estimate(&f32_cov, 0).unwrap();
        assert!(
            !evidence.sampled_from_covering,
            "an f32 covering must fall back to the geometry path"
        );
        let plain = estimate(&write(&dir, "plain.parquet", &geoms), 0).unwrap();
        let mut normalized = evidence;
        normalized.sampled_from_covering = false;
        assert_eq!(normalized, plain);

        // The same file with f64 fields takes the covering path.
        let f64_cov = write_manual_covering(&dir, "f64.parquet", &geoms, true, &[]);
        assert!(estimate(&f64_cov, 0).unwrap().sampled_from_covering);
    }

    /// A covering value that is null where the geometry is not (a
    /// non-conformant writer) skips that row, exactly as a null geometry
    /// would — never a panic, never a zero-sized bbox.
    #[test]
    fn covering_nulls_on_nonnull_geometries_skip_those_rows() {
        let dir = tempfile::tempdir().unwrap();
        let geoms = polygon_row(20, 0.009);
        let source = write_manual_covering(&dir, "nulls.parquet", &geoms, true, &[2, 7]);
        let evidence = estimate(&source, 0).unwrap();
        assert!(evidence.sampled_from_covering);
        assert_eq!(evidence.rows_read, 20);
        assert_eq!(evidence.sampled_count, 18);
    }

    /// `covering_bboxes` swaps an inverted envelope rather than skipping it,
    /// so the covering path measures the same population the geometry path
    /// does (`BboxAcc` is a min/max, which has no inverted case at all), and
    /// skips only what a null or non-finite bound makes unmeasurable.
    #[test]
    fn covering_bboxes_swaps_inverted_and_skips_nonfinite() {
        use arrow_array::{ArrayRef, Float64Array, StructArray};
        use arrow_schema::{DataType, Field, Fields};
        use std::sync::Arc;

        // 0: ordinary. 1: antimeridian wraparound (xmin > xmax). 2: inverted
        // y (not a legal wraparound, just malformed). 3: NaN. 4: +inf.
        // 5: null child.
        let xmin = Float64Array::from(vec![
            Some(1.0),
            Some(179.0),
            Some(1.0),
            Some(f64::NAN),
            Some(1.0),
            None,
        ]);
        let ymin = Float64Array::from(vec![
            Some(2.0),
            Some(-1.0),
            Some(5.0),
            Some(2.0),
            Some(2.0),
            Some(2.0),
        ]);
        let xmax = Float64Array::from(vec![
            Some(3.0),
            Some(-179.0),
            Some(3.0),
            Some(3.0),
            Some(f64::INFINITY),
            Some(3.0),
        ]);
        let ymax = Float64Array::from(vec![
            Some(4.0),
            Some(1.0),
            Some(2.0),
            Some(4.0),
            Some(4.0),
            Some(4.0),
        ]);

        let field_names = ["xmin", "ymin", "xmax", "ymax"];
        let children: Fields = field_names
            .iter()
            .map(|n| Arc::new(Field::new(*n, DataType::Float64, true)))
            .collect();
        let arrays: Vec<ArrayRef> = vec![
            Arc::new(xmin),
            Arc::new(ymin),
            Arc::new(xmax),
            Arc::new(ymax),
        ];
        let col: ArrayRef = Arc::new(StructArray::new(children, arrays, None));
        let names = field_names.map(str::to_string);

        let got = covering_bboxes(&col, &names, &[0, 1, 2, 3, 4, 5]).unwrap();
        assert_eq!(got[0], Some([1.0, 2.0, 3.0, 4.0]));
        assert_eq!(
            got[1],
            Some([-179.0, -1.0, 179.0, 1.0]),
            "a wraparound bbox must become the planar envelope BboxAcc's \
             min/max yields for the same feature, not a skipped row"
        );
        assert_eq!(got[2], Some([1.0, 2.0, 3.0, 5.0]), "inverted y swaps too");
        assert_eq!(got[3], None, "NaN is unmeasurable");
        assert_eq!(got[4], None, "+inf is unmeasurable");
        assert_eq!(got[5], None, "a null child skips the row");
    }

    /// A covering the schema declares but the data never populates must not
    /// turn a working `--max-zoom auto` into an error: the estimate warns,
    /// re-samples the geometry column, and lands exactly where an uncovered
    /// read of the same geometries does.
    #[test]
    fn an_all_null_covering_falls_back_to_the_geometry_path() {
        let dir = tempfile::tempdir().unwrap();
        let geoms = polygon_row(20, 0.009);
        let every_row: Vec<usize> = (0..geoms.len()).collect();
        let source = write_manual_covering(&dir, "allnull.parquet", &geoms, true, &every_row);

        let fell_back = estimate(&source, 0).unwrap();
        let plain = estimate(&write(&dir, "plain.parquet", &geoms), 0).unwrap();

        assert!(
            !fell_back.sampled_from_covering,
            "the evidence must report the path actually used"
        );
        assert_eq!(
            fell_back.sampled_count, 20,
            "every row measured after the retry, not zero"
        );
        assert_eq!(fell_back, plain);
    }

    /// `--bbox` and `--filter` scope the covering path exactly as they scope
    /// the geometry path (row-group pruning and the per-feature tests).
    #[test]
    fn bbox_and_filter_scope_the_covering_path_identically() {
        let dir = tempfile::tempdir().unwrap();
        let mut geoms = point_grid(20, 0.0001); // dense, near (0, 0)
        geoms.extend(point_grid(10, 0.1).into_iter().map(|g| {
            g.map(|g| match g {
                Geometry::Point(p) => Geometry::Point(Point::new(p.x() + 10.0, p.y() + 10.0)),
                other => other,
            })
        }));
        let covered = write_covered(&dir, "cov.parquet", &geoms);
        let plain = write(&dir, "plain.parquet", &geoms);

        let bbox = Some([9.5, 9.5, 12.0, 12.0]);
        let cov_bbox =
            estimate_with(&covered, 0, AUTO_MAX_ZOOM_CEILING, bbox.as_ref(), None).unwrap();
        let plain_bbox =
            estimate_with(&plain, 0, AUTO_MAX_ZOOM_CEILING, bbox.as_ref(), None).unwrap();
        assert!(cov_bbox.sampled_from_covering);
        assert_eq!(cov_bbox.sampled_count, 100);
        let mut normalized = cov_bbox;
        normalized.sampled_from_covering = false;
        assert_eq!(normalized, plain_bbox);

        let cov_filter =
            estimate_with(&covered, 0, AUTO_MAX_ZOOM_CEILING, None, Some("id >= 400")).unwrap();
        let plain_filter =
            estimate_with(&plain, 0, AUTO_MAX_ZOOM_CEILING, None, Some("id >= 400")).unwrap();
        assert!(cov_filter.sampled_from_covering);
        assert_eq!(cov_filter.sampled_count, 100);
        let mut normalized = cov_filter;
        normalized.sampled_from_covering = false;
        assert_eq!(normalized, plain_filter);
    }

    /// Real data: `open-buildings.parquet` already carries an f64 bbox struct
    /// column, just without the 1.1 covering declaration. Declaring it must
    /// not move the pinned zoom (z14), and the covering path must be taken.
    #[test]
    fn declared_covering_on_a_real_fixture_keeps_the_pinned_zoom() {
        use std::sync::Arc;
        use tylertoo_io::test_support::ArrowWriter;
        use tylertoo_io::KeyValue;

        let src = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !src.exists() {
            eprintln!("skipping: fixture not present");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let dst = dir.path().join("ob-covered.parquet");

        let file = std::fs::File::open(src).unwrap();
        let builder = tylertoo_io::open_parquet(file).unwrap();
        let kv = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .clone();
        // Strip the schema-embedded metadata: ArrowWriter would re-emit the
        // original (covering-less) "geo" beside the patched one below.
        let schema = Arc::new(arrow_schema::Schema::new(builder.schema().fields().clone()));
        let reader = builder.build().unwrap();
        let out = std::fs::File::create(&dst).unwrap();
        let mut writer = ArrowWriter::try_new(out, schema, None).unwrap();
        for batch in reader {
            writer.write(&batch.unwrap()).unwrap();
        }
        for pair in kv {
            if pair.key.eq_ignore_ascii_case("geo") {
                let mut geo: serde_json::Value =
                    serde_json::from_str(pair.value.as_deref().unwrap()).unwrap();
                let primary = geo["primary_column"].as_str().unwrap().to_string();
                geo["columns"][primary.as_str()]["covering"] = serde_json::json!({
                    "bbox": {
                        "xmin": ["bbox", "xmin"], "ymin": ["bbox", "ymin"],
                        "xmax": ["bbox", "xmax"], "ymax": ["bbox", "ymax"]
                    }
                });
                writer.append_key_value_metadata(KeyValue::new(pair.key, geo.to_string()));
            } else {
                writer.append_key_value_metadata(pair);
            }
        }
        writer.close().unwrap();

        let covered = ConvertSource::resolve_path(&dst).unwrap();
        let evidence = estimate(&covered, 0).unwrap();
        assert!(evidence.sampled_from_covering, "{evidence:?}");
        assert_eq!(evidence.chosen_zoom, BUILDINGS_Z, "{evidence:?}");
    }

    #[test]
    fn measures_a_real_point_grid_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let source = write(&dir, "points.parquet", &point_grid(20, 0.001));
        let evidence = estimate(&source, 0).unwrap();

        assert_eq!(evidence.rows_read, 400);
        assert_eq!(evidence.sampled_count, 400); // well under the sample cap
        assert_eq!(evidence.typical_extent_m, 0.0); // points have no extent
                                                    // ~111m grid -> resolvable 111m -> want 55.7m -> z ceil(log2(40075016/256/55.7)) = 12.
        assert_eq!(evidence.chosen_zoom, 12);
    }

    #[test]
    fn denser_points_choose_a_finer_zoom() {
        let dir = tempfile::tempdir().unwrap();
        let dense = estimate(&write(&dir, "d.parquet", &point_grid(20, 0.0001)), 0).unwrap();
        let sparse = estimate(&write(&dir, "s.parquet", &point_grid(20, 0.1)), 0).unwrap();
        assert!(
            dense.chosen_zoom > sparse.chosen_zoom,
            "dense z{} should exceed sparse z{}",
            dense.chosen_zoom,
            sparse.chosen_zoom
        );
    }

    #[test]
    fn measures_real_polygon_extents_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        // ~1km squares.
        let evidence = estimate(&write(&dir, "p.parquet", &polygon_row(50, 0.009)), 0).unwrap();
        assert_eq!(evidence.rows_read, 50);
        assert!(
            evidence.typical_extent_m > 500.0 && evidence.typical_extent_m < 2000.0,
            "expected ~1.4km diagonal squares, got {}",
            evidence.typical_extent_m
        );
    }

    /// S1-a (#573 review): points mixed into polygon data must not drag the
    /// extent median to zero (which used to fall back to `min_zoom`).
    #[test]
    fn mixed_points_and_polygons_keep_the_polygon_extent_signal() {
        let dir = tempfile::tempdir().unwrap();
        // 30 points far apart (1 degree) + 10 small (~100m) squares.
        let mut geoms: Vec<Option<Geometry<f64>>> = (0..30)
            .map(|i| Some(Geometry::Point(Point::new(i as f64, 5.0))))
            .collect();
        geoms.extend((0..10).map(|i| Some(square(i as f64, 0.0, 0.0009))));
        let evidence = estimate(&write(&dir, "mixed.parquet", &geoms), 0).unwrap();
        assert!(
            (100.0..200.0).contains(&evidence.typical_extent_m),
            "median over the squares only, got {}",
            evidence.typical_extent_m
        );
        assert!(evidence.chosen_zoom >= 12, "z{}", evidence.chosen_zoom);

        // The pure-estimator shape the review probed: [0, 0, 0, 50] + gaps 100.
        let ev = choose_auto_max_zoom(
            14,
            14,
            vec![0.0, 0.0, 0.0, 50.0],
            vec![100.0; 10],
            1.0,
            0,
            16,
        )
        .unwrap();
        assert_eq!(ev.typical_extent_m, 50.0);
        assert!(ev.chosen_zoom > 0);
    }

    /// Null and empty geometries are skipped, not measured as zero.
    #[test]
    fn null_and_empty_geometries_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut geoms = polygon_row(20, 0.009);
        geoms.push(None);
        geoms.push(Some(Geometry::MultiPolygon(MultiPolygon::<f64>(vec![]))));
        geoms.push(Some(Geometry::LineString(LineString::<f64>(vec![]))));
        let evidence = estimate(&write(&dir, "empties.parquet", &geoms), 0).unwrap();
        assert_eq!(evidence.rows_read, 23);
        assert_eq!(evidence.sampled_count, 20);
    }

    /// Only empty/null geometries: nothing to measure, so a clean error
    /// (tippecanoe `-zg`: "Can't guess maxzoom").
    #[test]
    fn only_empty_geometries_is_a_no_signal_error() {
        let dir = tempfile::tempdir().unwrap();
        let geoms = vec![None, Some(Geometry::LineString(LineString::<f64>(vec![])))];
        let err = estimate(&write(&dir, "allempty.parquet", &geoms), 0).unwrap_err();
        assert!(
            matches!(err, ConvertError::AutoZoomNoSignal { sampled: 0, .. }),
            "{err}"
        );
        assert!(err.to_string().contains("explicit --max-zoom"), "{err}");
    }

    /// One location repeated: no spacing, no extent -> the same error.
    #[test]
    fn a_single_location_is_a_no_signal_error() {
        let dir = tempfile::tempdir().unwrap();
        let geoms = vec![Some(Geometry::Point(Point::new(1.0, 1.0))); 5];
        let err = estimate(&write(&dir, "one.parquet", &geoms), 0).unwrap_err();
        assert!(
            matches!(err, ConvertError::AutoZoomNoSignal { .. }),
            "{err}"
        );
    }

    /// S1-c: a NaN coordinate must neither panic the percentile sort nor
    /// poison the estimate — the feature is skipped.
    #[test]
    fn nan_coordinates_are_skipped_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let mut geoms = point_grid(10, 0.001);
        geoms.push(Some(Geometry::Point(Point::new(f64::NAN, 1.0))));
        geoms.push(Some(square(f64::NAN, 0.0, 0.01)));
        let evidence = estimate(&write(&dir, "nan.parquet", &geoms), 0).unwrap();
        assert_eq!(evidence.sampled_count, 100);
        assert!(evidence.resolvable_m.is_finite());

        let mut v = vec![3.0, f64::NAN, 1.0];
        percentile(&mut v, 0.5); // must not panic
    }

    /// S1-d: `--bbox` scopes the estimate. Dense points in one corner and
    /// sparse points elsewhere: a bbox around the sparse part picks a coarser
    /// zoom than the whole file.
    #[test]
    fn bbox_scopes_the_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let mut geoms = point_grid(20, 0.0001); // dense, near (0, 0)
        geoms.extend(point_grid(10, 0.1).into_iter().map(|g| {
            g.map(|g| match g {
                Geometry::Point(p) => Geometry::Point(Point::new(p.x() + 10.0, p.y() + 10.0)),
                other => other,
            })
        }));
        let source = write(&dir, "bbox.parquet", &geoms);
        let whole = estimate(&source, 0).unwrap();
        let sparse = estimate_with(
            &source,
            0,
            AUTO_MAX_ZOOM_CEILING,
            Some(&[9.5, 9.5, 12.0, 12.0]),
            None,
        )
        .unwrap();
        assert_eq!(sparse.sampled_count, 100);
        assert!(
            sparse.chosen_zoom < whole.chosen_zoom,
            "bbox z{} should be coarser than whole-file z{}",
            sparse.chosen_zoom,
            whole.chosen_zoom
        );
    }

    /// #643: a western-hemisphere bbox scopes the estimate over a 0..360
    /// dataset, as it scopes the convert, instead of finding no data.
    #[test]
    fn bbox_scopes_the_estimate_across_the_wrap() {
        let dir = tempfile::tempdir().unwrap();
        let geoms: Vec<Option<Geometry<f64>>> = point_grid(10, 0.1)
            .into_iter()
            .map(|g| {
                g.map(|g| match g {
                    Geometry::Point(p) => Geometry::Point(Point::new(p.x() + 200.0, p.y())),
                    other => other,
                })
            })
            .collect();
        let source = write(&dir, "wrap.parquet", &geoms);
        let wrapped = estimate_with(
            &source,
            0,
            AUTO_MAX_ZOOM_CEILING,
            Some(&[-161.0, -1.0, -158.0, 2.0]),
            None,
        )
        .unwrap();
        assert_eq!(wrapped.sampled_count, 100);
    }

    /// `--filter` scopes the estimate the same way (on the `id` column the
    /// test writer adds: the first 400 rows are the dense grid).
    #[test]
    fn filter_scopes_the_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let mut geoms = point_grid(20, 0.0001);
        geoms.extend(point_grid(10, 0.1).into_iter().map(|g| {
            g.map(|g| match g {
                Geometry::Point(p) => Geometry::Point(Point::new(p.x() + 10.0, p.y() + 10.0)),
                other => other,
            })
        }));
        let source = write(&dir, "filter.parquet", &geoms);
        let sparse =
            estimate_with(&source, 0, AUTO_MAX_ZOOM_CEILING, None, Some("id >= 400")).unwrap();
        assert_eq!(sparse.sampled_count, 100);
        assert!(sparse.chosen_zoom < estimate(&source, 0).unwrap().chosen_zoom);
    }

    /// S1-b: `min_zoom` above the ceiling is a clean error, not a
    /// `clamp(min > max)` panic — from the estimator, the pure core, and the
    /// public resolver (before any I/O).
    #[test]
    fn min_zoom_above_the_auto_ceiling_is_an_error_not_a_panic() {
        let err = choose_auto_max_zoom(10, 10, vec![100.0; 3], vec![], 1.0, 17, 16).unwrap_err();
        assert!(
            matches!(
                err,
                ConvertError::AutoZoomMinAboveCeiling {
                    min_zoom: 17,
                    ceiling: 16
                }
            ),
            "{err}"
        );

        let dir = tempfile::tempdir().unwrap();
        let source = write(&dir, "p.parquet", &point_grid(5, 0.01));
        let mut options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 18,
                max_zoom: MaxZoom::Auto.plan_zoom(),
            },
            ..ConvertOptions::default()
        };
        let err = MaxZoom::Auto
            .resolve(&source, &mut options, None)
            .unwrap_err();
        assert!(
            matches!(
                err,
                ConvertError::AutoZoomMinAboveCeiling { min_zoom: 18, .. }
            ),
            "{err}"
        );
    }

    #[test]
    fn max_zoom_parses_numbers_and_auto_case_insensitively() {
        assert_eq!("6".parse::<MaxZoom>().unwrap(), MaxZoom::Fixed(6));
        assert_eq!("auto".parse::<MaxZoom>().unwrap(), MaxZoom::Auto);
        assert_eq!("AUTO".parse::<MaxZoom>().unwrap(), MaxZoom::Auto);
        assert!("banana".parse::<MaxZoom>().unwrap_err().contains("auto"));
        assert!("300".parse::<MaxZoom>().is_err());
        assert_eq!(MaxZoom::Auto.to_string(), "auto");
        assert_eq!(MaxZoom::Fixed(9).to_string(), "9");
    }

    /// `Fixed` resolves with no I/O at all: the source is never read (a
    /// directory with no parquet parts would fail to resolve, so the test
    /// uses a real source but a plan the estimate would reject).
    #[test]
    fn fixed_resolves_verbatim_and_leaves_options_alone() {
        let dir = tempfile::tempdir().unwrap();
        let source = write(&dir, "p.parquet", &point_grid(5, 0.01));
        let mut options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 9,
            },
            filter: Some("no_such_column = 1".to_string()),
            ..ConvertOptions::default()
        };
        assert_eq!(
            MaxZoom::Fixed(9)
                .resolve(&source, &mut options, None)
                .unwrap(),
            9
        );
        assert!(matches!(
            options.levels,
            LevelPlan::ZoomRange { max_zoom: 9, .. }
        ));
    }

    /// `Auto` validates the options before reading anything: a malformed
    /// filter fails as a filter error, not after the estimate.
    #[test]
    fn auto_validates_before_estimating_and_writes_the_zoom_back() {
        let dir = tempfile::tempdir().unwrap();
        let source = write(&dir, "p.parquet", &point_grid(20, 0.001));
        let mut bad = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: MaxZoom::Auto.plan_zoom(),
            },
            filter: Some("id >>> 3".to_string()),
            ..ConvertOptions::default()
        };
        assert!(matches!(
            MaxZoom::Auto.resolve(&source, &mut bad, None).unwrap_err(),
            ConvertError::Filter(_)
        ));

        let mut options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: MaxZoom::Auto.plan_zoom(),
            },
            ..ConvertOptions::default()
        };
        let z = MaxZoom::Auto.resolve(&source, &mut options, None).unwrap();
        assert_eq!(z, 12);
        assert!(matches!(
            options.levels,
            LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 12
            }
        ));
    }

    /// #452: the estimate reads the input, so an output that cannot be
    /// written is refused first — but after the option checks, so a bad
    /// option is still reported as one.
    #[test]
    fn auto_checks_the_output_before_estimating() {
        let dir = tempfile::tempdir().unwrap();
        let source = write(&dir, "p.parquet", &point_grid(20, 0.001));
        let missing = dir.path().join("no-such-dir").join("out.parquet");
        let auto = || ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: MaxZoom::Auto.plan_zoom(),
            },
            ..ConvertOptions::default()
        };
        let err = MaxZoom::Auto
            .resolve(&source, &mut auto(), Some(&missing))
            .unwrap_err();
        assert!(matches!(err, ConvertError::Output(_)), "{err:?}");

        let mut bad = ConvertOptions {
            filter: Some("id >>> 3".to_string()),
            ..auto()
        };
        assert!(matches!(
            MaxZoom::Auto
                .resolve(&source, &mut bad, Some(&missing))
                .unwrap_err(),
            ConvertError::Filter(_)
        ));
    }

    /// With an explicit GSD plan the value is unused, so nothing is read.
    #[test]
    fn auto_with_a_gsd_plan_skips_the_estimate() {
        let dir = tempfile::tempdir().unwrap();
        let geoms = vec![None]; // would be a no-signal error if estimated
        let source = write(&dir, "g.parquet", &geoms);
        let mut options = ConvertOptions {
            levels: LevelPlan::Gsds(vec![100.0, 10.0]),
            ..ConvertOptions::default()
        };
        assert_eq!(
            MaxZoom::Auto.resolve(&source, &mut options, None).unwrap(),
            AUTO_MAX_ZOOM_CEILING
        );
    }

    /// Characterization over the repo's real golden fixtures (issue #444's
    /// validation ask), pinned exactly so a formula change is a visible,
    /// reviewed diff: km-scale admin polygons < road segments < buildings.
    #[test]
    fn real_fixtures_choose_pinned_zooms() {
        let fixtures: &[(&str, u8)] = &[
            ("fieldmaps-madagascar-adm4.parquet", ADM4_Z),
            ("road-detections.parquet", ROADS_Z),
            ("open-buildings.parquet", BUILDINGS_Z),
        ];
        for (name, want) in fixtures {
            let path = Path::new("../../tests/fixtures/realdata").join(name);
            if !path.exists() {
                eprintln!("skipping {name}: fixture not present");
                continue;
            }
            let source = ConvertSource::resolve_path(&path).unwrap();
            let evidence = estimate(&source, 0).unwrap();
            eprintln!("{name}: {evidence:?}");
            assert_eq!(evidence.chosen_zoom, *want, "{name}: {evidence:?}");
        }
    }
    const ADM4_Z: u8 = 8;
    const ROADS_Z: u8 = 13;
    const BUILDINGS_Z: u8 = 14;

    #[test]
    fn percentile_median_of_odd_count() {
        let mut v = vec![30.0, 10.0, 20.0];
        assert_eq!(percentile(&mut v, 0.5), 20.0);
    }

    #[test]
    fn morton_key_is_translation_monotonic_in_each_axis() {
        let crs = Crs::Epsg4326;
        assert!(morton_key(-10.0, 0.0, crs) < morton_key(10.0, 0.0, crs));
        assert!(morton_key(0.0, -10.0, crs) < morton_key(0.0, 10.0, crs));
    }

    #[test]
    fn mercator_delta_is_latitude_aware() {
        let per_deg = WEBMERC_CIRCUMFERENCE_M / 360.0;
        // A degree of longitude is the same Mercator distance at any latitude.
        assert!((mercator_delta_m(1.0, 0.0, 60.0, Crs::Epsg4326) - per_deg).abs() < 1e-6);
        // A degree of latitude at 60 degrees stretches by 1/cos(60) = 2.
        let m = mercator_delta_m(0.0, 1.0, 60.0, Crs::Epsg4326);
        assert!((m - 2.0 * per_deg).abs() < 1e-6, "{m}");
        // At the equator, 1:1.
        assert!((mercator_delta_m(0.0, 1.0, 0.0, Crs::Epsg4326) - per_deg).abs() < 1e-6);
        // EPSG:3857 units are already Mercator meters.
        assert!((mercator_delta_m(3.0, 4.0, 70.0, Crs::Epsg3857) - 5.0).abs() < 1e-9);
    }

    /// `resolvable = 2 * pixel(z)` chooses exactly `z`.
    fn gsd_at_pixel(z: u8) -> f64 {
        WEBMERC_CIRCUMFERENCE_M / AUTO_ZOOM_TILE_PIXELS / 2f64.powi(z as i32)
    }

    #[test]
    fn extent_only_chooses_the_zoom_it_was_built_for() {
        let resolvable = 2.0 * gsd_at_pixel(10);
        let ev = choose_auto_max_zoom(1000, 1000, vec![resolvable; 5], vec![], 1.0, 0, 16).unwrap();
        assert_eq!(ev.chosen_zoom, 10);
        assert_eq!(ev.typical_extent_m, resolvable);
        assert_eq!(ev.nearby_spacing_m, f64::INFINITY);
    }

    #[test]
    fn spacing_only_chooses_the_zoom_it_was_built_for() {
        let resolvable = 2.0 * gsd_at_pixel(13);
        let ev =
            choose_auto_max_zoom(1000, 1000, vec![], vec![resolvable; 10], 1.0, 0, 16).unwrap();
        assert_eq!(ev.chosen_zoom, 13);
        assert_eq!(ev.typical_extent_m, 0.0);
    }

    #[test]
    fn finer_signal_wins_between_extent_and_spacing() {
        let ev = choose_auto_max_zoom(
            1000,
            1000,
            vec![2.0 * gsd_at_pixel(4); 5],
            vec![2.0 * gsd_at_pixel(15); 10],
            1.0,
            0,
            16,
        )
        .unwrap();
        assert_eq!(ev.chosen_zoom, 15);
    }

    #[test]
    fn clamps_to_the_ceiling_and_to_min_zoom() {
        let ev = choose_auto_max_zoom(1000, 1000, vec![0.001; 5], vec![], 1.0, 0, 16).unwrap();
        assert_eq!(ev.chosen_zoom, 16);
        let ev =
            choose_auto_max_zoom(1000, 1000, vec![10_000_000.0; 5], vec![], 1.0, 6, 16).unwrap();
        assert_eq!(ev.chosen_zoom, 6);
        // min_zoom == ceiling is legal and pins the pick.
        let ev =
            choose_auto_max_zoom(1000, 1000, vec![10_000_000.0; 5], vec![], 1.0, 16, 16).unwrap();
        assert_eq!(ev.chosen_zoom, 16);
    }

    #[test]
    fn no_signal_is_an_error_not_a_min_zoom_fallback() {
        let err =
            choose_auto_max_zoom(1000, 5, vec![0.0; 5], vec![0.0; 4], 1.0, 4, 16).unwrap_err();
        assert!(matches!(
            err,
            ConvertError::AutoZoomNoSignal {
                rows: 1000,
                sampled: 5
            }
        ));
    }

    #[test]
    fn subsample_correction_shrinks_apparent_spacing() {
        // A 1000m apparent gap from a 1% subsample corrects to 100m.
        let ev =
            choose_auto_max_zoom(1_000_000, 10_000, vec![], vec![1000.0; 10], 0.01, 0, 16).unwrap();
        assert!(
            (ev.nearby_spacing_m - 100.0).abs() < 1e-6,
            "{}",
            ev.nearby_spacing_m
        );
    }
}
