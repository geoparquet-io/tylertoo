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
//! only the geometry column (plus the filter's columns), and computes a bbox
//! for at most 200,000 systematically sampled rows (`AUTO_ZOOM_SAMPLE_CAP`)
//! straight from the GeoArrow scalars — non-sampled rows are never converted.
//! That bounds memory and geometry work, not I/O: the selected row groups'
//! geometry column is still read once.
//!
//! How this differs from tippecanoe's method (4096-unit resolution, vertex
//! spacing, quadkey deltas) and why the whole-file footer statistics are not
//! used is recorded in the divergence table of `context/ARCHITECTURE.md`.

use std::fmt;
use std::str::FromStr;

use geo_traits::to_geo::ToGeoGeometry;
use geo_traits::{
    CoordTrait, GeometryCollectionTrait, GeometryTrait, GeometryType, LineStringTrait, LineTrait,
    MultiLineStringTrait, MultiPointTrait, MultiPolygonTrait, PointTrait, PolygonTrait, RectTrait,
    TriangleTrait,
};
use geoarrow::array::from_arrow_array;
use geoarrow_array::GeoArrowArrayAccessor;

use crate::batch_processor::{visit_geoarrow_array, GeoArrowVisitor};
use crate::input_set::{ConvertSource, ReadPlan};
use crate::world_coord::MAX_LATITUDE;

use super::convert::{
    bbox_to_crs_units, bboxes_intersect, detect_crs_from_kv, find_geometry_column,
    validate_options, ConvertError, ConvertOptions, LevelPlan,
};
use super::filter::{parse_filter, BoundFilter};
use super::level::{Crs, WEBMERC_CIRCUMFERENCE_M};

/// Ceiling for `--max-zoom auto` (issue #444): "never above a documented
/// ceiling". tippecanoe's own `-zg` cap is `32 - full_detail` (z20 at
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

/// Rows per record batch while sampling (the geometry column only).
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
    ///   (so a bad option still fails in milliseconds, #371), then estimates
    ///   over `source` honoring `options.bbox` / `options.filter`, logs the
    ///   pick and its evidence, and writes the chosen zoom into
    ///   `options.levels`.
    ///
    /// `source` is only read (its column projection is left alone), so it may
    /// be the one the conversion then reads.
    pub fn resolve(
        self,
        source: &ConvertSource,
        options: &mut ConvertOptions,
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
}

impl AutoZoomEvidence {
    /// Log the pick (issue #444) so a user can see — and question — the
    /// inputs that drove it.
    pub(crate) fn log(&self) {
        log::info!(
            "--max-zoom auto: chose z{} (clamped to [{}, {}]) from {} sampled features of {} \
             rows read; typical feature extent ~{:.1}m, nearby spacing ~{:.1}m, resolvable \
             distance ~{:.1}m (Web Mercator meters)",
            self.chosen_zoom,
            self.min_zoom,
            self.ceiling,
            self.sampled_count,
            self.rows_read,
            self.typical_extent_m,
            self.nearby_spacing_m,
            self.resolvable_m,
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

/// Nearest-rank percentile (`p` in `[0, 1]`) of a non-empty `values`, sorted
/// in place. `total_cmp` keeps a stray NaN from panicking the sort (callers
/// only pass finite values anyway).
pub(crate) fn percentile(values: &mut [f64], p: f64) -> f64 {
    debug_assert!(!values.is_empty());
    values.sort_by(f64::total_cmp);
    let idx = (((values.len() - 1) as f64) * p).round() as usize;
    values[idx]
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

/// Estimate `--max-zoom auto` for a conversion of `source` under `options`
/// (module docs for the method): the minimum zoom comes from
/// `options.levels` (which must be a zoom range), and `options.bbox` /
/// `options.filter` scope the sample exactly as they scope the conversion.
/// [`MaxZoom::resolve`] is the usual entry point; this one returns the
/// evidence without logging it or touching `options`.
///
/// `source` is only read (its column projection is left alone), so it may be
/// the one the conversion then reads.
pub fn estimate_max_zoom(
    source: &ConvertSource,
    options: &ConvertOptions,
) -> Result<AutoZoomEvidence, ConvertError> {
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
    let crs = detect_crs_from_kv(source.key_value_metadata()?.as_ref())?;
    let geom_idx = find_geometry_column(&schema).ok_or(ConvertError::NoGeometryColumn)?;
    let geom_field = schema.field(geom_idx).clone();
    // Bound against the file's own column names: a filter naming a column
    // the conversion renames (#288) addresses it by that same input name.
    let bound_filter = filter
        .map(|src| Ok::<_, ConvertError>(BoundFilter::bind(&parse_filter(src)?, &schema, &[])?))
        .transpose()?;
    let bbox_units = bbox.map(|b| bbox_to_crs_units(b, crs));

    // The conversion's footer-statistics pruning (#102 bbox, #315 filter).
    let bbox_sel = bbox_units
        .as_ref()
        .map(|bb| source.select_row_groups(bb))
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

    let mut cols = vec![geom_idx];
    if let Some(f) = &bound_filter {
        cols.extend_from_slice(f.columns());
    }
    cols.sort_unstable();
    cols.dedup();
    let geom_pos = cols
        .binary_search(&geom_idx)
        .expect("the geometry column is projected");
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
        let garr = from_arrow_array(batch.column(geom_pos).as_ref(), &geom_field).map_err(|e| {
            crate::Error::GeoParquetRead(format!("auto max-zoom geometry decode: {e}"))
        })?;
        let bboxes = visit_geoarrow_array(garr.as_ref(), SampledBboxes { rows: &picks })?;
        for bb in bboxes.into_iter().flatten() {
            if bbox_units
                .as_ref()
                .is_some_and(|u| !bboxes_intersect(&bb, u))
            {
                continue;
            }
            let (cx, cy) = ((bb[0] + bb[2]) * 0.5, (bb[1] + bb[3]) * 0.5);
            diags_m.push(mercator_delta_m(bb[2] - bb[0], bb[3] - bb[1], cy, crs));
            centers.push((cx, cy));
        }
    }

    let sampled_count = centers.len();
    centers.sort_by_key(|&(cx, cy)| morton_key(cx, cy, crs));
    let gaps_m: Vec<f64> = centers
        .windows(2)
        .map(|w| {
            let mid_y = (w[0].1 + w[1].1) * 0.5;
            mercator_delta_m(w[1].0 - w[0].0, w[1].1 - w[0].1, mid_y, crs)
        })
        .collect();

    choose_auto_max_zoom(
        rows_read,
        sampled_count,
        diags_m,
        gaps_m,
        1.0 / stride as f64,
        min_zoom,
        ceiling,
    )
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
        let err = MaxZoom::Auto.resolve(&source, &mut options).unwrap_err();
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
        assert_eq!(MaxZoom::Fixed(9).resolve(&source, &mut options).unwrap(), 9);
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
            MaxZoom::Auto.resolve(&source, &mut bad).unwrap_err(),
            ConvertError::Filter(_)
        ));

        let mut options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: MaxZoom::Auto.plan_zoom(),
            },
            ..ConvertOptions::default()
        };
        let z = MaxZoom::Auto.resolve(&source, &mut options).unwrap();
        assert_eq!(z, 12);
        assert!(matches!(
            options.levels,
            LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 12
            }
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
            MaxZoom::Auto.resolve(&source, &mut options).unwrap(),
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
