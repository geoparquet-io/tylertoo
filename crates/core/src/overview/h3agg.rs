//! H3 aggregate overview levels (#332).
//!
//! At coarse zooms an overview can render **H3 cell aggregates** instead of the
//! real features: every feature is binned into the H3 cell covering its
//! representative point, and each occupied cell becomes one output row carrying
//! the cell's feature `count` and its boundary polygon. Zooming in, the geom
//! band takes over and the real geometry returns — one PMTiles archive, no
//! two-archive merge. This mirrors what `gpio process aggregate h3` does in
//! geoparquet-io, and slots in as a row-collapsing sibling of the point/square
//! representation bands (see [`super::simplify::Representation`]).
//!
//! # DIVERGENCE FROM geoparquet-io
//!
//! gpio bins on the true `ST_Centroid`. We bin on the **bbox center** that
//! [`super::assign::AssignFeature`] already computes for level assignment (see
//! `assign.rs` §"representative point"), so H3 keying costs nothing extra. For
//! a count aggregation the difference is immaterial — both place a feature in
//! one cell — and it keeps the hot path allocation-free.
//!
//! This module is deliberately **CRS-free and geometry-free on input**: the
//! caller supplies representative points already in lon/lat degrees (the convert
//! wiring reprojects EPSG:3857 rep points with
//! [`super::export::webmerc_to_lnglat`] before calling in). That keeps the
//! aggregator a pure, exhaustively testable function.

use std::collections::HashMap;

use geo::{Geometry, MultiPolygon};
use h3o::{CellIndex, LatLng, Resolution};

use super::assign::AssignFeature;
use super::export::webmerc_to_lnglat;
use super::level::Crs;

/// The representative point (lon/lat degrees) each feature is binned by: the
/// bbox center [`AssignFeature`] already carries. EPSG:3857 centers are
/// reprojected to lon/lat; EPSG:4326 centers already are lon/lat.
///
/// See the module-level DIVERGENCE note on bbox-center vs true centroid.
pub(super) fn rep_points_lonlat(features: &[AssignFeature], crs: Crs) -> Vec<(f64, f64)> {
    features
        .iter()
        .map(|f| {
            let (x, y) = f.center();
            match crs {
                Crs::Epsg4326 => (x, y),
                Crs::Epsg3857 => webmerc_to_lnglat(x, y),
            }
        })
        .collect()
}

/// One aggregated H3 cell: its id, the number of source features binned into
/// it, and its boundary geometry in lon/lat degrees.
#[derive(Debug, Clone, PartialEq)]
pub struct H3CellRow {
    /// The H3 cell index as a raw `u64` (written to the `h3_cell` column).
    pub cell: u64,
    /// Number of source features whose representative point fell in this cell.
    pub count: i64,
    /// The cell boundary as a `geo` geometry (a `Polygon`, or a `MultiPolygon`
    /// for cells that h3o splits at the antimeridian).
    pub geometry: Geometry<f64>,
}

/// Aggregate representative points into H3 cells at `resolution`.
///
/// Each `(lon, lat)` in degrees is binned into the covering H3 cell; the result
/// is one [`H3CellRow`] per occupied cell, **ordered by cell id** so the output
/// is deterministic regardless of input order. Points that are not valid
/// lat/lng (non-finite, out of range) are skipped, so
/// `Σ row.count <= points.len()` with equality when every point is valid.
pub fn aggregate_h3_cells<I>(points_lonlat: I, resolution: Resolution) -> Vec<H3CellRow>
where
    I: IntoIterator<Item = (f64, f64)>,
{
    let mut counts: HashMap<CellIndex, i64> = HashMap::new();
    for (lon, lat) in points_lonlat {
        let Ok(ll) = LatLng::new(lat, lng_wrap(lon)) else {
            continue;
        };
        *counts.entry(ll.to_cell(resolution)).or_insert(0) += 1;
    }
    let mut rows: Vec<H3CellRow> = counts
        .into_iter()
        .map(|(cell, count)| H3CellRow {
            cell: u64::from(cell),
            count,
            geometry: cell_geometry(cell),
        })
        .collect();
    rows.sort_unstable_by_key(|r| r.cell);
    rows
}

/// Normalize a longitude into `[-180, 180]`. Reprojected 3857 rep points can
/// land a hair outside the range at the antimeridian; h3o rejects those, so
/// wrap first rather than silently drop the feature.
#[inline]
fn lng_wrap(lon: f64) -> f64 {
    if !lon.is_finite() {
        return lon; // let LatLng::new reject it
    }
    let mut l = (lon + 180.0) % 360.0;
    if l < 0.0 {
        l += 360.0;
    }
    l - 180.0
}

/// The cell boundary as a `geo::Geometry` in lon/lat degrees. h3o's `geo`
/// feature yields a `MultiPolygon` already converted to degrees; a normal cell
/// is a single ring, which we unwrap to a `Polygon` for a leaner MVT feature,
/// keeping the `MultiPolygon` only for antimeridian-split cells.
fn cell_geometry(cell: CellIndex) -> Geometry<f64> {
    let mut mp = MultiPolygon::<f64>::from(cell);
    if mp.0.len() == 1 {
        Geometry::Polygon(mp.0.pop().unwrap())
    } else {
        Geometry::MultiPolygon(mp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::CoordsIter;

    fn res(r: u8) -> Resolution {
        Resolution::try_from(r).unwrap()
    }

    #[test]
    fn counts_are_exact_and_order_independent() {
        // Three points: two coincide (same cell), one elsewhere. Feeding them
        // in two different orders must produce the identical row set, and the
        // counts must sum to the input size.
        let a = (2.349014, 48.864716); // Paris
        let b = (2.349001, 48.864700); // ~2 m away -> same cell at res 6
        let c = (-73.985, 40.748); // NYC
        let forward = aggregate_h3_cells([a, b, c], res(6));
        let reversed = aggregate_h3_cells([c, b, a], res(6));
        assert_eq!(forward, reversed, "output must be order-independent");

        let total: i64 = forward.iter().map(|r| r.count).sum();
        assert_eq!(total, 3, "Σcount must equal the valid input point count");
        assert_eq!(forward.len(), 2, "Paris pair collapses, NYC stands alone");
        let paris = forward.iter().find(|r| r.count == 2).unwrap();
        assert_eq!(paris.count, 2);
    }

    #[test]
    fn finer_resolution_splits_nearby_points() {
        // Two points ~2 m apart share a coarse cell but separate at a fine one.
        let a = (2.349014, 48.864716);
        let b = (2.349200, 48.864716);
        assert_eq!(aggregate_h3_cells([a, b], res(6)).len(), 1);
        assert_eq!(aggregate_h3_cells([a, b], res(13)).len(), 2);
    }

    #[test]
    fn non_finite_points_are_skipped_not_panicked() {
        // h3o normalizes finite-but-out-of-range coordinates internally, so the
        // only inputs we drop are the non-finite ones (NaN / ±Inf in either
        // component). Garbage in must never panic.
        let good = (10.0, 50.0);
        let rows = aggregate_h3_cells(
            [
                good,
                (f64::NAN, 0.0),
                (0.0, f64::NAN),
                (f64::INFINITY, 0.0),
                (0.0, f64::NEG_INFINITY),
            ],
            res(5),
        );
        let total: i64 = rows.iter().map(|r| r.count).sum();
        assert_eq!(total, 1, "only the one finite point should be counted");
    }

    #[test]
    fn cell_geometry_is_a_closed_nonempty_ring() {
        let rows = aggregate_h3_cells([(0.0, 0.0)], res(4));
        let geom = &rows[0].geometry;
        let poly = match geom {
            Geometry::Polygon(p) => p,
            other => panic!("expected a Polygon near the equator, got {other:?}"),
        };
        let ext = poly.exterior();
        // A hexagon ring is 7 coords (6 verts + closing point); pentagons 6.
        assert!(ext.coords_count() >= 4, "ring too small");
        let first = ext.coords().next().unwrap();
        let last = ext.coords().last().unwrap();
        assert_eq!(first, last, "exterior ring must be closed");
    }

    #[test]
    fn longitudes_just_past_the_antimeridian_wrap_in() {
        // 180.0000001 must not be dropped; it wraps to ~-179.9999999.
        let rows = aggregate_h3_cells([(180.000_000_1, 0.0)], res(3));
        assert_eq!(rows.iter().map(|r| r.count).sum::<i64>(), 1);
    }
}
