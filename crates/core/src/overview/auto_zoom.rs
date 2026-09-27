//! `--max-zoom auto` (#444): tippecanoe `-zg` parity.
//!
//! tippecanoe's `-zg` picks a maximum zoom by sorting point locations along a
//! 32-bit quadkey index, measuring the (log-space) gap between consecutive
//! distinct locations, and choosing the zoom at which a slightly-denser-than-
//! typical gap resolves to roughly one output pixel (`main.cpp`, the
//! `guess_maxzoom` block — see `docs/diving-deeper/tippecanoe.md`). For
//! polygons/lines it additionally measures the average distance between
//! consecutive vertices *within* a feature and raises the zoom further if
//! that demands more detail.
//!
//! tylertoo adapts the same intent — pick the finest zoom that resolves
//! "typical" feature detail, then go one zoom finer for safety — to data it
//! can measure cheaply, with two deliberate divergences from tippecanoe's
//! own method, both empirically justified below.
//!
//! DIVERGENCE FROM TIPPECANOE 1: footer statistics (row count, whole-file
//! bbox) are NOT used as the estimator's primary signal, even though they
//! are the cheapest thing available before any scan and the issue's design
//! notes floated them as the preferred route. Measured on the
//! `open-buildings` golden fixture (1,000 building polygons over a ~15km ×
//! 20km extract): footer-only density (`sqrt(bbox_area / count)`) implies a
//! typical spacing of ~566m, while the *actual* median building extent is
//! ~28m and the true median nearest-neighbor distance (computed with a
//! KD-tree for this comparison) is ~27m — a >20x error, entirely an artifact
//! of how populated a bbox happens to be (this fixture is a 1,000-row
//! extract, not a full download). A 20x spacing error is roughly 4-5 zoom
//! levels, i.e. exactly the "-z8 for buildings" failure mode the issue warns
//! against. So the estimator instead reads each feature's own bounding box —
//! cheap (project just the geometry column, no attribute decode) but not
//! free, and NOT deferred past pass 1 (see divergence 2).
//!
//! DIVERGENCE FROM TIPPECANOE 2: tylertoo could in principle defer max-zoom
//! resolution until after pass 1's own per-feature bbox scan
//! (`super::stream::run_pass1`), which already computes exactly the data
//! this estimator wants, for free. That was investigated and rejected for
//! v1: `LevelPlan::resolve` — and by extension `ConvertOptions.levels` — is
//! matched exhaustively at several call sites across `convert.rs`,
//! `stream.rs`, the CLI and the Python bindings, and `validate_options`
//! calls `LevelPlan::check_zoom_ceiling` before any I/O happens specifically
//! so a bad `--max-zoom` fails in milliseconds (#371) — a contract "auto"
//! cannot satisfy without a value in hand yet. Threading auto-resolution
//! through that seam would touch a large, heavily-tested surface for a
//! marginal I/O saving. Instead, this module runs its own lightweight
//! geometry-bbox-only read (via a caller-supplied [`ConvertSource`], never
//! the one the real conversion will use) before `ConvertOptions` is built,
//! so `LevelPlan`, `check_zoom_ceiling` and `resolve` are untouched and the
//! numeric `--max-zoom N` path is byte-identical to before this change. The
//! cost is a real one worth stating plainly: on a very large input, this
//! reads the geometry column once before the real conversion reads it again.
//! A follow-up could special-case `LevelPlan::Auto` and resolve it from
//! pass 1's own output instead, saving that second read.
//!
//! Given per-feature bboxes, the estimator combines two signals, matching
//! tippecanoe's own two-signal shape (inter-feature spacing, then a second
//! term that can only raise the zoom):
//!
//! - **typical extent**: the median bbox diagonal across sampled features.
//!   Meaningful for lines/polygons, always `0` for points.
//! - **nearby spacing**: sampled feature centers are Morton-sorted (the same
//!   technique as tippecanoe's quadkey sort — real coordinates here, not a
//!   32-bit quadkey delta) and the 10th percentile of consecutive-order gaps
//!   is taken, mirroring tippecanoe's `exp(mean - 1.5*stddev)` "denser than
//!   typical" measure. Meaningful for points, and a secondary signal for
//!   packed lines/polygons.
//!
//! `resolvable = min(typical extent, nearby spacing)` (only over whichever
//! signal is available) — the *finer* requirement wins, exactly as
//! tippecanoe's own `if (mz > maxzoom) maxzoom = mz` lets its intra-feature
//! term only raise the zoom. `chosen_zoom` is then the finest zoom whose
//! resolution — ground distance per standard 256px tile pixel — is no
//! larger than half `resolvable` ("go one zoom beyond what is strictly
//! necessary", tippecanoe's own comment), clamped to `[min_zoom, ceiling]`.
//!
//! Feature reads are bounded at [`AUTO_ZOOM_SAMPLE_CAP`] via a deterministic
//! systematic sample (every Nth feature in file order, N chosen from the
//! footer row count) rather than reading every feature of an arbitrarily
//! large input. A systematic/random subsample of a 2D point process thins
//! the *apparent* nearest-neighbor gap by `1/sqrt(sample_fraction)`
//! (standard Poisson-thinning behavior), so the measured "nearby spacing" is
//! corrected by `sqrt(sample_fraction)` before use. The median extent needs
//! no such correction — a subsample's median converges to the population
//! median regardless of sample fraction. Being systematic rather than random
//! also makes the result deterministic: the same input always chooses the
//! same zoom.

use std::sync::Arc;

use geo::Geometry;
use geoarrow::array::from_arrow_array;
use geoarrow_array::GeoArrowArray;
use parquet::arrow::ProjectionMask;

use crate::batch_processor::extract_geometries_opt_from_array;
use crate::input_set::ConvertSource;

use super::convert::{detect_crs_from_kv, find_geometry_column, geometry_bbox, ConvertError};
use super::level::{Crs, METERS_PER_DEGREE, WEBMERC_CIRCUMFERENCE_M};

/// Default ceiling for `--max-zoom auto` (issue #444): "never above a
/// documented ceiling". tippecanoe's own default cap is effectively
/// `32 - full_detail` (z20 at defaults) but its typical real-world outputs
/// for web-mapping-scale data land well under 16; picking 16 keeps an
/// auto-guessed archive from silently becoming enormous while still
/// covering building- and address-level detail.
pub const AUTO_MAX_ZOOM_CEILING: u8 = 16;

/// Upper bound on how many features the estimator reads per input,
/// regardless of total feature count — bounded memory (`O(sample)`, not
/// `O(dataset)`), matching the rest of tylertoo's streaming-first design.
pub const AUTO_ZOOM_SAMPLE_CAP: usize = 200_000;

/// Reference tile pixel width for the "one pixel resolves this many meters"
/// target. This is the standard web-map tile pixel width (256), deliberately
/// NOT [`super::level::GSD_TILE_BASE`] (1024) — that constant calibrates
/// tylertoo's own RDP simplification tolerance per zoom (a related but
/// distinct question: "how much can this vertex move before it matters?"),
/// while `--max-zoom auto` answers "at what zoom does a feature become
/// visually resolvable at all?", the same question tippecanoe's own
/// (`extent`-denominated) formula answers.
const AUTO_ZOOM_TILE_PIXELS: f64 = 256.0;

/// The chosen zoom plus the measurements that produced it, so a user can
/// second-guess the pick without re-deriving it (issue #444: "log the chosen
/// zoom and its evidence").
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AutoZoomEvidence {
    /// Total features in the (bbox-filtered, if any) input, from the footer.
    pub feature_count: u64,
    /// Features actually read to build the estimate (`<=` [`AUTO_ZOOM_SAMPLE_CAP`]).
    pub sampled_count: usize,
    /// Median per-feature bbox diagonal, in meters. `0.0` for point data or
    /// when no sample was available.
    pub typical_extent_m: f64,
    /// 10th-percentile Morton-order consecutive-gap between feature centers,
    /// in meters, corrected for the sample fraction. `f64::INFINITY` when
    /// fewer than two samples were available (no spacing signal).
    pub nearby_spacing_m: f64,
    /// `min(typical_extent_m, nearby_spacing_m)` over whichever signal(s)
    /// were available; `f64::NAN` when neither was.
    pub resolvable_m: f64,
    /// The zoom this evidence resolved to.
    pub chosen_zoom: u8,
    /// The floor `chosen_zoom` was clamped to.
    pub min_zoom: u8,
    /// The ceiling `chosen_zoom` was clamped to.
    pub ceiling: u8,
}

impl AutoZoomEvidence {
    /// Log the pick prominently (issue #444) so a user can see — and
    /// question — the inputs that drove it.
    pub fn log(&self) {
        if self.resolvable_m.is_finite() {
            log::info!(
                "--max-zoom auto: chose z{} (clamped to [{}, {}]) from {} features \
                 ({} sampled); typical feature extent ~{:.1}m, nearby spacing ~{:.1}m, \
                 resolvable distance ~{:.1}m",
                self.chosen_zoom,
                self.min_zoom,
                self.ceiling,
                self.feature_count,
                self.sampled_count,
                self.typical_extent_m,
                self.nearby_spacing_m,
                self.resolvable_m,
            );
        } else {
            log::warn!(
                "--max-zoom auto: could not measure feature spacing or extent from {} features \
                 ({} sampled); defaulting to the minimum zoom z{}. Pick an explicit --max-zoom \
                 for this input.",
                self.feature_count,
                self.sampled_count,
                self.min_zoom,
            );
        }
    }
}

/// Nearest-rank percentile (`p` in `[0, 1]`) of `values`, sorted in place.
/// Not the textbook "average the two middle values" median definition —
/// nearest-rank is simpler and, for the sample sizes this module deals with,
/// indistinguishable in effect.
pub(crate) fn percentile(values: &mut [f64], p: f64) -> f64 {
    debug_assert!(!values.is_empty());
    values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a bbox diagonal or gap"));
    let idx = (((values.len() - 1) as f64) * p).round() as usize;
    values[idx]
}

/// Euclidean distance in meters between two points `stride`ed apart in the
/// file's native CRS units — degrees for [`Crs::Epsg4326`] (equatorial
/// approximation, matching [`super::level::Crs::meters_to_units`]'s own
/// tolerance, §7.1), meters already for [`Crs::Epsg3857`].
pub(crate) fn native_delta_to_meters(dx: f64, dy: f64, crs: Crs) -> f64 {
    let (dx_m, dy_m) = match crs {
        Crs::Epsg3857 => (dx, dy),
        Crs::Epsg4326 => (dx * METERS_PER_DEGREE, dy * METERS_PER_DEGREE),
    };
    dx_m.hypot(dy_m)
}

/// Spread `x`'s low 32 bits so each occupies every other bit of a `u64` —
/// the standard bit-interleave building block for a 2D Morton (Z-order)
/// code.
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
/// global coordinate range (not the data's own range, so the code is a pure
/// function of one point — no second pass needed to find data bounds).
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

/// Core estimator (pure — no I/O), given already-measured per-feature bbox
/// diagonals and Morton-order consecutive-center gaps, both in meters.
///
/// `sample_fraction` is `sampled_count / feature_count` and corrects
/// `gaps_m`'s percentile for subsample thinning (module docs, divergence 2).
pub(crate) fn choose_auto_max_zoom(
    feature_count: u64,
    sampled_count: usize,
    mut diags_m: Vec<f64>,
    mut gaps_m: Vec<f64>,
    sample_fraction: f64,
    min_zoom: u8,
    ceiling: u8,
) -> AutoZoomEvidence {
    let extent_signal = if diags_m.iter().any(|&d| d.is_finite() && d > 0.0) {
        Some(percentile(&mut diags_m, 0.5))
    } else {
        None
    };
    let spacing_signal = if gaps_m.len() >= 2 {
        Some(percentile(&mut gaps_m, 0.10) * sample_fraction.sqrt())
    } else {
        None
    };

    let resolvable_m = match (extent_signal, spacing_signal) {
        (Some(e), Some(s)) => e.min(s),
        (Some(e), None) => e,
        (None, Some(s)) => s,
        (None, None) => f64::NAN,
    };

    let chosen_zoom = if resolvable_m.is_finite() && resolvable_m > 0.0 {
        let want = resolvable_m / 2.0;
        let z = (WEBMERC_CIRCUMFERENCE_M / AUTO_ZOOM_TILE_PIXELS / want)
            .log2()
            .ceil();
        // Guard against a non-finite `z` (e.g. `want` denormal-tiny) before
        // the `as u8` cast, which saturates on out-of-range floats but not
        // on NaN/±inf.
        let z = if z.is_finite() { z } else { ceiling as f64 };
        (z as i64).clamp(min_zoom as i64, ceiling as i64) as u8
    } else {
        // tippecanoe's own precedent (main.cpp, `-zg` with no usable
        // signal): fall back to the floor, not the ceiling — better to
        // under-tile than to silently build a much larger archive than the
        // data can justify.
        min_zoom
    };

    AutoZoomEvidence {
        feature_count,
        sampled_count,
        typical_extent_m: extent_signal.unwrap_or(0.0),
        nearby_spacing_m: spacing_signal.unwrap_or(f64::INFINITY),
        resolvable_m,
        chosen_zoom,
        min_zoom,
        ceiling,
    }
}

/// Estimate `--max-zoom auto` for `source` (module docs for the method).
///
/// `source` must be a [`ConvertSource`] the caller does not intend to reuse
/// for the real conversion with a *different* column projection — this
/// function does not call [`ConvertSource::restrict_columns`] (it projects
/// per-part parquet readers directly), so it is safe to call on the same
/// `ConvertSource` the real conversion will also read, but callers that want
/// a `--properties` selection applied to the real read should resolve a
/// fresh `ConvertSource` for this estimate to avoid confusion.
pub fn estimate_max_zoom(
    source: &ConvertSource,
    min_zoom: u8,
    ceiling: u8,
) -> Result<AutoZoomEvidence, ConvertError> {
    let crs = detect_crs_from_kv(source.key_value_metadata()?.as_ref())?;
    let feature_count = source.selected_row_count(None)?.max(0) as u64;
    if feature_count == 0 {
        return Err(ConvertError::NoData);
    }
    let stride = ((feature_count as usize) / AUTO_ZOOM_SAMPLE_CAP).max(1) as u64;

    let mut diags_m: Vec<f64> = Vec::new();
    let mut centers: Vec<(f64, f64)> = Vec::new();
    let mut seen: u64 = 0;

    for part in source.parts() {
        let builder = part.open()?;
        let schema: Arc<arrow_schema::Schema> = builder.schema().clone();
        let Some(geom_idx) = find_geometry_column(&schema) else {
            continue;
        };
        let geom_field = schema.field(geom_idx).clone();
        let mask = ProjectionMask::roots(builder.parquet_schema(), [geom_idx]);
        let reader = builder.with_projection(mask).build()?;
        for batch in reader {
            let batch = batch?;
            if batch.num_columns() == 0 {
                continue;
            }
            let geom_array: Arc<dyn GeoArrowArray> =
                from_arrow_array(batch.column(0).as_ref(), &geom_field).map_err(|e| {
                    crate::Error::GeoParquetRead(format!("auto max-zoom geometry decode: {e}"))
                })?;
            let mut opts: Vec<Option<Geometry<f64>>> = Vec::with_capacity(batch.num_rows());
            extract_geometries_opt_from_array(geom_array.as_ref(), &mut opts)?;
            for g in opts.into_iter().flatten() {
                if seen.is_multiple_of(stride) {
                    let bbox = geometry_bbox(&g);
                    let dx = bbox[2] - bbox[0];
                    let dy = bbox[3] - bbox[1];
                    diags_m.push(native_delta_to_meters(dx, dy, crs));
                    centers.push(((bbox[0] + bbox[2]) * 0.5, (bbox[1] + bbox[3]) * 0.5));
                }
                seen += 1;
            }
        }
    }

    let sampled_count = centers.len();
    centers.sort_by_key(|&(cx, cy)| morton_key(cx, cy, crs));
    let mut gaps_m: Vec<f64> = Vec::with_capacity(sampled_count.saturating_sub(1));
    for w in centers.windows(2) {
        let g = native_delta_to_meters(w[1].0 - w[0].0, w[1].1 - w[0].1, crs);
        if g > 0.0 {
            gaps_m.push(g);
        }
    }
    let sample_fraction = if feature_count == 0 {
        1.0
    } else {
        (sampled_count as f64 / feature_count as f64).min(1.0)
    };

    let evidence = choose_auto_max_zoom(
        feature_count,
        sampled_count,
        diags_m,
        gaps_m,
        sample_fraction,
        min_zoom,
        ceiling,
    );
    evidence.log();
    Ok(evidence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_set::ConvertSource;
    use geo::{Geometry, Point, Polygon};
    use std::path::Path;

    /// An `n x n` grid of points, `step_deg` degrees apart (EPSG:4326),
    /// centered near the equator so the degree→meter approximation this
    /// module already uses is accurate.
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

    /// `n` square polygons, `size_deg` on a side, spaced `size_deg * 3` apart
    /// so they never overlap.
    fn polygon_row(n: usize, size_deg: f64) -> Vec<Option<Geometry<f64>>> {
        (0..n)
            .map(|i| {
                let x0 = i as f64 * size_deg * 3.0;
                Some(Geometry::Polygon(Polygon::new(
                    geo::LineString::from(vec![
                        (x0, 0.0),
                        (x0 + size_deg, 0.0),
                        (x0 + size_deg, size_deg),
                        (x0, size_deg),
                        (x0, 0.0),
                    ]),
                    vec![],
                )))
            })
            .collect()
    }

    #[test]
    fn measures_a_real_point_grid_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("points.parquet");
        let geoms = point_grid(20, 0.001); // ~111m apart at the equator
        super::super::testutil::write_input(&path, &geoms, false, None);

        let source = ConvertSource::resolve_path(&path).unwrap();
        let evidence = estimate_max_zoom(&source, 0, AUTO_MAX_ZOOM_CEILING).unwrap();

        assert_eq!(evidence.feature_count, 400);
        assert_eq!(evidence.sampled_count, 400); // well under the sample cap
        assert_eq!(evidence.typical_extent_m, 0.0); // points have no extent
        assert!(
            evidence.nearby_spacing_m.is_finite() && evidence.nearby_spacing_m > 0.0,
            "point data must produce a spacing signal, got {}",
            evidence.nearby_spacing_m
        );
        assert!(
            evidence.chosen_zoom > 0 && evidence.chosen_zoom <= AUTO_MAX_ZOOM_CEILING,
            "z{} out of range",
            evidence.chosen_zoom
        );
    }

    #[test]
    fn denser_points_choose_a_finer_zoom() {
        let dir = tempfile::tempdir().unwrap();
        let dense_path = dir.path().join("dense.parquet");
        let sparse_path = dir.path().join("sparse.parquet");
        super::super::testutil::write_input(&dense_path, &point_grid(20, 0.0001), false, None);
        super::super::testutil::write_input(&sparse_path, &point_grid(20, 0.1), false, None);

        let dense = estimate_max_zoom(
            &ConvertSource::resolve_path(&dense_path).unwrap(),
            0,
            AUTO_MAX_ZOOM_CEILING,
        )
        .unwrap();
        let sparse = estimate_max_zoom(
            &ConvertSource::resolve_path(&sparse_path).unwrap(),
            0,
            AUTO_MAX_ZOOM_CEILING,
        )
        .unwrap();

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
        let path = dir.path().join("polys.parquet");
        // ~1km squares.
        super::super::testutil::write_input(&path, &polygon_row(50, 0.009), false, None);

        let source = ConvertSource::resolve_path(&path).unwrap();
        let evidence = estimate_max_zoom(&source, 0, AUTO_MAX_ZOOM_CEILING).unwrap();

        assert_eq!(evidence.feature_count, 50);
        assert!(
            evidence.typical_extent_m > 500.0 && evidence.typical_extent_m < 2000.0,
            "expected ~1.3km diagonal squares, got {}",
            evidence.typical_extent_m
        );
    }

    /// Characterization test (issue #444's validation ask) over the repo's
    /// three real golden fixtures: coarse admin polygons, small dense
    /// building polygons, and detected road lines. Ranges are generous —
    /// this pins "sane ballpark", not an exact zoom, since the estimator is
    /// a heuristic and any future formula tweak should not have to hit a
    /// single magic number. The important property under test is the
    /// *ordering*: admin polygons (fokontany, km-scale) should land well
    /// below buildings (meter-scale) and below/around road segments
    /// (tens-to-hundreds-of-meters scale) -- a heuristic that inverted this
    /// order (e.g. z16 for admin polygons or z8 for buildings) would be
    /// exactly the failure mode #444 warns against.
    #[test]
    fn real_fixtures_choose_zooms_in_a_sane_order() {
        let fixtures: &[(&str, u8, u8)] = &[
            // (file, expected minimum z, expected maximum z)
            ("fieldmaps-madagascar-adm4.parquet", 2, 10),
            ("road-detections.parquet", 10, 16),
            ("open-buildings.parquet", 12, 16),
        ];
        let mut chosen = Vec::new();
        for (name, lo, hi) in fixtures {
            let path = Path::new("../../tests/fixtures/realdata").join(name);
            if !path.exists() {
                eprintln!("skipping {name}: fixture not present");
                continue;
            }
            let source = ConvertSource::resolve_path(&path).unwrap();
            let evidence = estimate_max_zoom(&source, 0, AUTO_MAX_ZOOM_CEILING).unwrap();
            eprintln!(
                "{name}: chosen_zoom=z{} typical_extent_m={:.1} nearby_spacing_m={:.1} \
                 feature_count={}",
                evidence.chosen_zoom,
                evidence.typical_extent_m,
                evidence.nearby_spacing_m,
                evidence.feature_count,
            );
            assert!(
                evidence.chosen_zoom >= *lo && evidence.chosen_zoom <= *hi,
                "{name}: chosen z{} outside expected [{lo}, {hi}]",
                evidence.chosen_zoom
            );
            chosen.push((*name, evidence.chosen_zoom));
        }
        if chosen.len() == fixtures.len() {
            let adm4 = chosen[0].1;
            let roads = chosen[1].1;
            let buildings = chosen[2].1;
            assert!(
                adm4 < roads && roads <= buildings,
                "expected adm4 (z{adm4}) < roads (z{roads}) <= buildings (z{buildings})"
            );
        }
    }

    #[test]
    fn min_zoom_is_respected_even_for_tiny_features() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.parquet");
        super::super::testutil::write_input(&path, &point_grid(5, 0.5), false, None);

        let source = ConvertSource::resolve_path(&path).unwrap();
        let evidence = estimate_max_zoom(&source, 12, AUTO_MAX_ZOOM_CEILING).unwrap();
        assert!(evidence.chosen_zoom >= 12);
    }

    #[test]
    fn percentile_median_of_odd_count() {
        let mut v = vec![30.0, 10.0, 20.0];
        assert_eq!(percentile(&mut v, 0.5), 20.0);
    }

    #[test]
    fn morton_key_is_translation_monotonic_in_x() {
        // Not a full 2D-locality proof, just a sanity check that increasing
        // one axis (holding the other fixed) increases the code -- a Morton
        // code that didn't do this would be broken outright.
        let crs = Crs::Epsg4326;
        assert!(morton_key(-10.0, 0.0, crs) < morton_key(10.0, 0.0, crs));
        assert!(morton_key(0.0, -10.0, crs) < morton_key(0.0, 10.0, crs));
    }

    #[test]
    fn native_delta_to_meters_matches_equatorial_approximation() {
        // 1 degree of longitude at the equator ~= METERS_PER_DEGREE meters.
        let m = native_delta_to_meters(1.0, 0.0, Crs::Epsg4326);
        assert!((m - METERS_PER_DEGREE).abs() < 1e-6);
        // EPSG:3857 units are already meters -- verbatim.
        let m3857 = native_delta_to_meters(3.0, 4.0, Crs::Epsg3857);
        assert!((m3857 - 5.0).abs() < 1e-9);
    }

    /// Synthetic extents -> expected zoom, no I/O (issue #444's TDD ask).
    ///
    /// `resolvable_m = 2 * gsd_at_pixel(z)` should choose exactly `z`: the
    /// estimator wants `resolvable / 2` to resolve to one pixel, i.e.
    /// `resolvable = 2 * (circumference / 256 / 2^z)`.
    fn gsd_at_pixel(z: u8) -> f64 {
        WEBMERC_CIRCUMFERENCE_M / AUTO_ZOOM_TILE_PIXELS / 2f64.powi(z as i32)
    }

    #[test]
    fn extent_only_chooses_the_zoom_it_was_built_for() {
        let target_z = 10u8;
        let resolvable = 2.0 * gsd_at_pixel(target_z);
        let ev = choose_auto_max_zoom(1000, 1000, vec![resolvable; 5], vec![], 1.0, 0, 16);
        assert_eq!(ev.chosen_zoom, target_z);
        assert_eq!(ev.typical_extent_m, resolvable);
        assert_eq!(ev.nearby_spacing_m, f64::INFINITY);
    }

    #[test]
    fn spacing_only_chooses_the_zoom_it_was_built_for() {
        let target_z = 13u8;
        let resolvable = 2.0 * gsd_at_pixel(target_z);
        // percentile(0.10) of ten equal gaps is that same value.
        let ev = choose_auto_max_zoom(1000, 1000, vec![], vec![resolvable; 10], 1.0, 0, 16);
        assert_eq!(ev.chosen_zoom, target_z);
        assert_eq!(ev.typical_extent_m, 0.0);
    }

    #[test]
    fn finer_signal_wins_between_extent_and_spacing() {
        // A large typical extent (coarse) but tight spacing (fine): the
        // finer (smaller ground-distance) signal must win, per tippecanoe's
        // own "only ever raise the zoom" combination rule.
        let coarse_extent = 2.0 * gsd_at_pixel(4);
        let fine_spacing = 2.0 * gsd_at_pixel(15);
        let ev = choose_auto_max_zoom(
            1000,
            1000,
            vec![coarse_extent; 5],
            vec![fine_spacing; 10],
            1.0,
            0,
            16,
        );
        assert_eq!(ev.chosen_zoom, 15);
    }

    #[test]
    fn clamps_to_the_ceiling() {
        let ev = choose_auto_max_zoom(1000, 1000, vec![0.001; 5], vec![], 1.0, 0, 16);
        assert_eq!(ev.chosen_zoom, 16);
    }

    #[test]
    fn clamps_to_min_zoom() {
        let ev = choose_auto_max_zoom(1000, 1000, vec![10_000_000.0; 5], vec![], 1.0, 6, 16);
        assert_eq!(ev.chosen_zoom, 6);
    }

    #[test]
    fn no_signal_falls_back_to_min_zoom_not_ceiling() {
        // All-zero diagonals (pure points with a degenerate bbox reader) and
        // no gaps: nothing to measure. tippecanoe's own precedent for "can't
        // guess" is the floor, not the ceiling.
        let ev = choose_auto_max_zoom(1000, 1000, vec![0.0; 5], vec![], 1.0, 4, 16);
        assert_eq!(ev.chosen_zoom, 4);
        assert!(ev.resolvable_m.is_nan());
    }

    #[test]
    fn subsample_correction_shrinks_apparent_spacing() {
        // A 10-degree-of-freedom apparent gap of 1000m measured from a 1%
        // subsample should correct down toward 100m (sqrt(0.01) = 0.1).
        let ev = choose_auto_max_zoom(1_000_000, 10_000, vec![], vec![1000.0; 10], 0.01, 0, 16);
        assert!(
            (ev.nearby_spacing_m - 100.0).abs() < 1e-6,
            "expected ~100m, got {}",
            ev.nearby_spacing_m
        );
    }

    #[test]
    fn min_zoom_never_exceeds_ceiling_when_both_signals_absent_and_min_above_ceiling_is_impossible()
    {
        // Guard against a pathological (min_zoom > ceiling) caller: the
        // fallback still returns min_zoom verbatim -- callers are
        // responsible for min_zoom <= ceiling (validated the same way the
        // existing --min-zoom/--max-zoom ordering is, at the CLI/core
        // boundary), this just documents the fallback does not itself clamp.
        let ev = choose_auto_max_zoom(1000, 1000, vec![], vec![], 1.0, 5, 16);
        assert_eq!(ev.chosen_zoom, 5);
    }
}
