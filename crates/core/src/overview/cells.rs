//! Cell aggregation for `pyramid` cell bands (#332).
//!
//! A cell band reads its input once, drops every feature into the cell of a
//! discrete grid that holds its representative point, and writes one table
//! of cell polygons per zoom: one row per occupied cell, carrying
//! `point_count` and the `--accumulate-attribute` / `--cluster-categories`
//! rollups of the features inside it. The pyramid then tiles each table
//! verbatim at its own zoom.
//!
//! Memory is bounded by occupied cells, not input rows: the input streams
//! through [`read_in_order`] once, each batch folds into a per-cell map at
//! the band's finest resolution, and every coarser zoom is built from the
//! next finer zoom's cells by their parent id (a parent cell is a group-by
//! on its key). The map is charged to the run's [`MemoryBudget`].
//!
//! Parents are taken by id, so an H3 or A5 parent sums the cells its system
//! lists as its children. Grid and S2 children tile their parent exactly;
//! H3 and A5 children only approximate it, so a point near a parent's edge
//! can count in the neighbouring coarse cell.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::Float64Type;
use arrow_array::{Array, ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geo::{Centroid, Coord, Geometry, LineString, MultiPolygon, Polygon};
use geoarrow::array::{from_arrow_array, GeometryBuilder};
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use rayon::prelude::*;

use super::cluster::{rank_categories, AccumulateSpec, AggState, CategoryCounts};
use super::cluster::{MAX_CLUSTER_CATEGORIES, OTHER_CATEGORY, POINT_COUNT_COLUMN};
use super::convert::{ConvertError, ConvertOptions, GroupInterner};
use super::level::Crs;
use super::memory::{MemoryBudget, MemoryPhase};
use super::read::{read_in_order, ReadFlow, ReadPass, ReadTuning};
use crate::input_set::ConvertSource;

/// A discrete global grid a cell band aggregates into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CellSystem {
    /// TODO(docs)
    Grid,
    /// TODO(docs)
    H3,
    /// TODO(docs)
    S2,
    /// TODO(docs)
    A5,
}

/// Name of the cell id column every cell table carries.
pub(crate) const CELL_COLUMN: &str = "cell";

// Zoom → resolution, one entry per zoom 0..=30. A cell is sized to about a
// 4×4 pixel block of a 256-pixel tile: 1/64 of the tile's width, so a tile
// holds at most ~4,096 cells. Grid cells are exactly that (zoom + 6, capped
// at the tile math's z30). For H3, S2 and A5 each entry is the resolution
// whose average cell area is nearest, in log ratio, to that block's area at
// the equator (`resolution_tables_follow_the_rule` checks it). Web Mercator
// cells shrink away from the equator and the equal-area systems do not, so
// at 60° an H3 cell covers about four grid cells.
const GRID_RES: [u8; 31] = [
    6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30,
    30, 30, 30, 30, 30, 30,
];
const H3_RES: [u8; 31] = [
    1, 2, 3, 3, 4, 5, 5, 6, 7, 8, 8, 9, 10, 10, 11, 12, 13, 13, 14, 15, 15, 15, 15, 15, 15, 15, 15,
    15, 15, 15, 15,
];
const S2_RES: [u8; 31] = [
    4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28,
    29, 30, 30, 30, 30, 30,
];
// A5's resolution 30 covers only part of the globe, so it stops at 29.
const A5_RES: [u8; 31] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 29, 29, 29, 29,
];

impl CellSystem {
    /// TODO(docs)
    pub const ALL: [Self; 4] = [Self::Grid, Self::H3, Self::S2, Self::A5];

    /// TODO(docs)
    pub fn name(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::H3 => "h3",
            Self::S2 => "s2",
            Self::A5 => "a5",
        }
    }

    /// TODO(docs)
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The resolution this system aggregates at for Web Mercator zoom `z`.
    pub(crate) fn resolution(self, z: u8) -> u8 {
        let z = usize::from(z.min(30));
        match self {
            Self::Grid => GRID_RES[z],
            Self::H3 => H3_RES[z],
            Self::S2 => S2_RES[z],
            Self::A5 => A5_RES[z],
        }
    }

    /// The id of the cell at `res` holding `(lon, lat)` degrees, or `None`
    /// for a point the system cannot place.
    fn cell(self, lon: f64, lat: f64, res: u8) -> Option<u64> {
        if !lon.is_finite() || !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
            return None;
        }
        // Past ±180 (a reprojection hair, or wrapped data): wrap back in.
        let lon = if (-180.0..=180.0).contains(&lon) {
            lon
        } else {
            (lon + 180.0).rem_euclid(360.0) - 180.0
        };
        match self {
            Self::Grid => {
                let t = crate::tile::lng_lat_to_tile(lon, lat, res);
                Some(u64::from(t.x) << 32 | u64::from(t.y))
            }
            Self::H3 => {
                let res = h3o::Resolution::try_from(res).ok()?;
                Some(u64::from(h3o::LatLng::new(lat, lon).ok()?.to_cell(res)))
            }
            Self::S2 => {
                let leaf = s2::cellid::CellID::from(s2::latlng::LatLng::from_degrees(lat, lon));
                Some(leaf.parent(u64::from(res)).0)
            }
            Self::A5 => a5::lonlat_to_cell(a5::LonLat::new(lon, lat), i32::from(res)).ok(),
        }
    }

    /// The parent at `to` of cell `id` at `from` (`to <= from`).
    fn parent(self, id: u64, from: u8, to: u8) -> u64 {
        match self {
            Self::Grid => {
                let k = from - to;
                (id >> 32 >> k) << 32 | (id & 0xFFFF_FFFF) >> k
            }
            Self::H3 => h3o::CellIndex::try_from(id)
                .ok()
                .and_then(|c| c.parent(h3o::Resolution::try_from(to).ok()?))
                .map_or(id, u64::from),
            Self::S2 => s2::cellid::CellID(id).parent(u64::from(to)).0,
            Self::A5 => {
                a5::core::serialization::cell_to_parent(id, Some(i32::from(to))).unwrap_or(id)
            }
        }
    }

    /// The cell id as its system writes it: hex for H3 and A5, the S2
    /// token, and `res/x/y` for a grid cell.
    fn token(self, id: u64, res: u8) -> String {
        match self {
            Self::Grid => format!("{res}/{}/{}", id >> 32, id & 0xFFFF_FFFF),
            Self::H3 => format!("{id:x}"),
            Self::S2 => s2::cellid::CellID(id).to_token(),
            Self::A5 => a5::u64_to_hex(id),
        }
    }

    /// The cell's outline as a lon/lat polygon, split at the antimeridian
    /// and closed over a pole it encircles.
    fn polygon(self, id: u64, res: u8) -> MultiPolygon<f64> {
        let ring: Vec<(f64, f64)> = match self {
            Self::Grid => {
                let b = crate::tile::tile_bounds((id >> 32) as u32, id as u32, res);
                return MultiPolygon(vec![Polygon::new(
                    LineString::from(vec![
                        (b.lng_min, b.lat_min),
                        (b.lng_max, b.lat_min),
                        (b.lng_max, b.lat_max),
                        (b.lng_min, b.lat_max),
                        (b.lng_min, b.lat_min),
                    ]),
                    vec![],
                )]);
            }
            Self::H3 => h3o::CellIndex::try_from(id)
                .map(|c| c.boundary().iter().map(|ll| (ll.lng(), ll.lat())).collect())
                .unwrap_or_default(),
            Self::S2 => {
                let cell = s2::cell::Cell::from(s2::cellid::CellID(id));
                (0..4)
                    .map(|k| {
                        let ll = s2::latlng::LatLng::from(cell.vertex(k));
                        (ll.lng.deg(), ll.lat.deg())
                    })
                    .collect()
            }
            Self::A5 => a5::cell_to_boundary(id, None)
                .map(|b| b.iter().map(|p| (p.longitude(), p.latitude())).collect())
                .unwrap_or_default(),
        };
        lonlat_cell(&ring)
    }
}

/// A great-circle ring as a lon/lat polygon.
///
/// Edges longer than a degree are densified along the great circle (S2 and
/// H3 hand back only corners), longitudes are unwrapped into one continuous
/// run, a ring that winds once around a pole is closed over it, and the
/// result is cut at ±180 into pieces inside the lon/lat extent.
fn lonlat_cell(ring: &[(f64, f64)]) -> MultiPolygon<f64> {
    let mut pts: Vec<(f64, f64)> = ring.to_vec();
    if pts.first() == pts.last() {
        pts.pop();
    }
    if pts.len() < 3 {
        return MultiPolygon(vec![]);
    }
    let mut dense: Vec<(f64, f64)> = Vec::with_capacity(pts.len() * 2);
    for (i, &a) in pts.iter().enumerate() {
        let b = pts[(i + 1) % pts.len()];
        dense.extend(great_circle(a, b));
    }
    // A pole has no longitude: a corner on one becomes the pole at the
    // longitudes of the edges either side of it.
    let m = dense.len();
    let dense: Vec<(f64, f64)> = (0..m)
        .flat_map(|i| {
            let (lon, lat) = dense[i];
            if lat.abs() < 90.0 - 1e-9 {
                return vec![(lon, lat)];
            }
            vec![(dense[(i + m - 1) % m].0, lat), (dense[(i + 1) % m].0, lat)]
        })
        .collect();
    // Unwrap: each step takes the short way round.
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(dense.len() + 4);
    for (lon, lat) in dense {
        let lon = match out.last() {
            Some(&(prev, _)) => prev + (lon - prev + 540.0).rem_euclid(360.0) - 180.0,
            None => lon,
        };
        out.push((lon, lat));
    }
    let (first, last) = (out[0], out[out.len() - 1]);
    let winding = last.0 + (first.0 - last.0 + 540.0).rem_euclid(360.0) - 180.0 - first.0;
    if winding.abs() > 180.0 {
        // Encircles a pole: run along it from the end back to the start.
        let pole = if out.iter().map(|p| p.1).sum::<f64>() > 0.0 {
            90.0
        } else {
            -90.0
        };
        let end = first.0 + winding;
        out.push((end, last.1));
        out.push((end, pole));
        out.push((first.0, pole));
    }
    out.push(out[0]);
    let poly = Polygon::new(LineString::from(out), vec![]);
    split_antimeridian(&poly)
}

/// Points from `a` to just before `b` along the great circle, one at least
/// every degree.
fn great_circle(a: (f64, f64), b: (f64, f64)) -> Vec<(f64, f64)> {
    let to_xyz = |(lon, lat): (f64, f64)| {
        let (lon, lat) = (lon.to_radians(), lat.to_radians());
        [lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin()]
    };
    let (p, q) = (to_xyz(a), to_xyz(b));
    let dot = (p[0] * q[0] + p[1] * q[1] + p[2] * q[2]).clamp(-1.0, 1.0);
    let steps = dot.acos().to_degrees().ceil().max(1.0) as usize;
    let mut out = vec![a];
    for s in 1..steps {
        let t = s as f64 / steps as f64;
        let v: Vec<f64> = (0..3).map(|i| p[i] * (1.0 - t) + q[i] * t).collect();
        let lat = v[2].atan2(v[0].hypot(v[1])).to_degrees();
        out.push((v[1].atan2(v[0]).to_degrees(), lat));
    }
    out
}

/// Cut a polygon whose longitudes run past ±180 into pieces inside
/// [-180, 180], shifting the overhang back by 360°.
fn split_antimeridian(poly: &Polygon<f64>) -> MultiPolygon<f64> {
    use geo::{BooleanOps, BoundingRect, MapCoords, Translate};
    let Some(r) = poly.bounding_rect() else {
        return MultiPolygon(vec![]);
    };
    if r.min().x >= -180.0 && r.max().x <= 180.0 {
        return MultiPolygon(vec![poly.clone()]);
    }
    let window = |lo: f64| {
        geo::Rect::new(
            Coord { x: lo, y: -90.0 },
            Coord {
                x: lo + 360.0,
                y: 90.0,
            },
        )
        .to_polygon()
    };
    let mut parts = Vec::new();
    for shift in [-360.0, 0.0, 360.0] {
        let piece = poly.intersection(&window(-180.0 - shift));
        parts.extend(piece.translate(shift, 0.0).0);
    }
    // The shift can leave a cut edge a rounding error past ±180.
    MultiPolygon(parts).map_coords(|c| Coord {
        x: c.x.clamp(-180.0, 180.0),
        y: c.y,
    })
}

/// What each cell rolls up besides its `point_count`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CellRollup<'a> {
    /// `--accumulate-attribute` specs.
    pub accumulate: &'a [AccumulateSpec],
    /// `--cluster-categories` column.
    pub categories: Option<&'a str>,
}

/// One cell's partial aggregate. Merges in any order to the same totals.
#[derive(Debug, Clone)]
struct CellAcc {
    count: i64,
    aggs: Vec<AggState>,
    cats: CategoryCounts,
}

impl CellAcc {
    fn new(specs: usize) -> Self {
        Self {
            count: 0,
            aggs: vec![AggState::new(); specs],
            cats: CategoryCounts::default(),
        }
    }

    fn merge(&mut self, other: &Self) {
        self.count += other.count;
        for (a, b) in self.aggs.iter_mut().zip(&other.aggs) {
            a.merge(b);
        }
        self.cats.merge(&other.cats);
    }
}

type CellMap = HashMap<u64, CellAcc>;

/// Rows per parallel task within a batch.
const CHUNK_ROWS: usize = 8192;

/// Bytes one occupied cell holds, besides its category counts.
fn cell_bytes(specs: usize) -> u64 {
    // The map slot and control byte, the key, the struct, and its specs.
    (std::mem::size_of::<(u64, CellAcc)>() + 8 + specs * std::mem::size_of::<AggState>()) as u64
}

/// Aggregate `input` into cells for every zoom `min_zoom..=max_zoom` and
/// write zoom `z`'s table to `tables[z - min_zoom]` as GeoParquet in
/// EPSG:4326, rows ordered by cell id.
///
/// Each feature lands in the cell holding its centroid (a point is its own
/// centroid). A feature with no usable geometry is skipped and counted on
/// [`Diagnostic::UnusableGeometry`](crate::Diagnostic::UnusableGeometry).
///
/// # Errors
///
/// The input cannot be read, its CRS is unsupported, a rollup column is
/// missing or of the wrong type, or the cell map outgrows the memory budget.
pub(crate) fn write_cell_tables(
    input: &Path,
    system: CellSystem,
    (min_zoom, max_zoom): (u8, u8),
    rollup: CellRollup<'_>,
    tables: &[&Path],
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(), ConvertError> {
    debug_assert_eq!(tables.len(), usize::from(max_zoom - min_zoom) + 1);
    let (mut cells, labels) = aggregate(input, system, max_zoom, rollup, diagnostics)?;
    let mut res = system.resolution(max_zoom);
    for z in (min_zoom..=max_zoom).rev() {
        let to = system.resolution(z);
        if to < res {
            let mut parents = CellMap::with_capacity(cells.len() / 2);
            for (id, acc) in cells.drain() {
                parents
                    .entry(system.parent(id, res, to))
                    .and_modify(|p| p.merge(&acc))
                    .or_insert(acc);
            }
            cells = parents;
            res = to;
        }
        write_table(
            tables[usize::from(z - min_zoom)],
            system,
            res,
            &cells,
            rollup,
            &labels,
        )?;
    }
    Ok(())
}

/// Read `input` once into the cell map at `max_zoom`'s resolution. Returns
/// the map, its category counts keyed by category slot, and the slot labels.
fn aggregate(
    input: &Path,
    system: CellSystem,
    max_zoom: u8,
    rollup: CellRollup<'_>,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(CellMap, Vec<String>), ConvertError> {
    let source = ConvertSource::resolve_path(input)?;
    let descriptor = source.resolved_schema()?;
    let (crs, _) =
        super::convert::crs_from_resolved(descriptor, &source.display_name(), diagnostics)?;
    let geom_idx = descriptor
        .geometry_index()
        .ok_or_else(|| ConvertError::NoGeometryColumn {
            input: source.display_name(),
        })?;
    // The rollup columns are checked exactly as `--cluster` checks them.
    let check = ConvertOptions {
        cluster: true,
        accumulate: rollup.accumulate.to_vec(),
        cluster_categories: rollup.categories.map(str::to_string),
        ..ConvertOptions::default()
    };
    let acc_idx = super::convert::validate_cluster_schema(descriptor, &check)?;
    let cat_idx = super::convert::cluster_category_column(descriptor.schema(), &check);
    let mut projection: Vec<usize> = std::iter::once(geom_idx)
        .chain(acc_idx.iter().copied())
        .chain(cat_idx)
        .collect();
    projection.sort_unstable();
    projection.dedup();
    let schema = descriptor.schema();
    let geom_name = schema.field(geom_idx).name().clone();
    let acc_names: Vec<String> = acc_idx
        .iter()
        .map(|&i| schema.field(i).name().clone())
        .collect();
    let cat_name = cat_idx.map(|i| schema.field(i).name().clone());

    let res = system.resolution(max_zoom);
    let specs = rollup.accumulate.len();
    let defaults = ConvertOptions::default();
    let budget = MemoryBudget::probe();
    let tuning = ReadTuning {
        budget,
        batch_size: defaults.read_batch_size,
        workers: super::convert::resolve_pass_read_workers(ReadPass::Scan, defaults.read_workers),
        avg_geom_bytes: None,
        footer: None,
        profile: defaults.profile,
    };
    let mut cells = CellMap::new();
    let mut interner = GroupInterner::default();
    let (mut rows, mut skipped, mut cat_pairs) = (0usize, 0usize, 0u64);
    read_in_order(&source, None, Some(&projection), tuning, |batch, _, _| {
        let n = batch.num_rows();
        rows += n;
        let bschema = batch.schema();
        let col = |name: &str| bschema.index_of(name).map(|i| batch.column(i).clone());
        let geom_i = bschema.index_of(&geom_name)?;
        let geom_field = bschema.field(geom_i).clone();
        let geom_col = batch.column(geom_i).clone();
        let values: Vec<ArrayRef> = acc_names
            .iter()
            .map(|name| Ok(arrow_cast::cast(&col(name)?, &DataType::Float64)?))
            .collect::<Result<_, ConvertError>>()?;
        let mut ids: Vec<u32> = Vec::new();
        if let Some(name) = &cat_name {
            interner.extend(col(name)?.as_ref(), &mut ids);
        }
        let chunks: Vec<(CellMap, usize)> = (0..n)
            .step_by(CHUNK_ROWS)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|off| {
                let len = CHUNK_ROWS.min(n - off);
                let garr = from_arrow_array(geom_col.slice(off, len).as_ref(), &geom_field)
                    .map_err(|e| crate::GeoParquetReadError::decode("geometry decode", e))?;
                let mut geoms = Vec::with_capacity(len);
                crate::batch_processor::extract_geometries_opt_from_array(
                    garr.as_ref(),
                    &mut geoms,
                    diagnostics,
                )?;
                let mut local = CellMap::new();
                let mut bad = 0usize;
                for (r, g) in geoms.iter().enumerate() {
                    let Some(id) = g
                        .as_ref()
                        .and_then(representative_point)
                        .and_then(|(x, y)| {
                            let (lon, lat) = match crs {
                                Crs::Epsg3857 => super::export::webmerc_to_lnglat(x, y),
                                _ => (x, y),
                            };
                            system.cell(lon, lat, res)
                        })
                    else {
                        bad += 1;
                        continue;
                    };
                    let acc = local.entry(id).or_insert_with(|| CellAcc::new(specs));
                    acc.count += 1;
                    let row = off + r;
                    for (state, v) in acc.aggs.iter_mut().zip(&values) {
                        let v = v.as_primitive::<Float64Type>();
                        // NaN is skipped, as `--cluster` skips it.
                        if v.is_valid(row) && !v.value(row).is_nan() {
                            state.add(v.value(row));
                        }
                    }
                    if let Some(&id) = ids.get(row) {
                        acc.cats.add(id);
                    }
                }
                Ok((local, bad))
            })
            .collect::<Result<_, ConvertError>>()?;
        for (local, bad) in chunks {
            skipped += bad;
            for (id, acc) in local {
                match cells.get_mut(&id) {
                    Some(c) => {
                        let before = c.cats.as_slice().len();
                        c.merge(&acc);
                        cat_pairs += (c.cats.as_slice().len() - before) as u64;
                    }
                    None => {
                        cat_pairs += acc.cats.as_slice().len() as u64;
                        cells.insert(id, acc);
                    }
                }
            }
        }
        // A cell map that outgrows the budget fails here, not by OOM kill.
        budget.require(
            MemoryPhase::Scan,
            cells.capacity() as u64 * cell_bytes(specs) + cat_pairs * 12 + interner.heap_bytes(),
        )?;
        Ok(ReadFlow::Continue)
    })?;
    if skipped > 0 {
        diagnostics.once(crate::diagnostics::Diagnostic::UnusableGeometry {
            count: skipped,
            total: rows,
        });
    }
    let labels = if cat_name.is_some() {
        relabel_categories(&mut cells, &interner.into_values())
    } else {
        Vec::new()
    };
    Ok((cells, labels))
}

/// The point a feature is counted at: its centroid, `None` when it has
/// none or it is not finite.
fn representative_point(g: &Geometry<f64>) -> Option<(f64, f64)> {
    let p = match g {
        Geometry::Point(p) => *p,
        other => other.centroid()?,
    };
    (p.x().is_finite() && p.y().is_finite()).then_some((p.x(), p.y()))
}

/// Re-key every cell's category counts from interned value ids to the
/// ranked category slots of `--cluster-categories`, and return the slot
/// labels.
fn relabel_categories(cells: &mut CellMap, values: &[String]) -> Vec<String> {
    let null = GroupInterner::NULL_GROUP;
    let mut freq = vec![0u64; values.len()];
    for acc in cells.values() {
        for &(id, n) in acc.cats.as_slice() {
            if id != null {
                freq[id as usize] += n as u64;
            }
        }
    }
    let (slot_of, mut labels) = rank_categories(&freq, values, MAX_CLUSTER_CATEGORIES);
    let other = labels.len() as u32;
    let mut uses_other = false;
    for acc in cells.values_mut() {
        let mut slots = CategoryCounts::default();
        for &(id, n) in acc.cats.as_slice() {
            let slot = if id == null {
                other
            } else {
                slot_of[id as usize]
            };
            uses_other |= slot == other;
            slots.add_n(slot, n);
        }
        acc.cats = slots;
    }
    if uses_other {
        labels.push(OTHER_CATEGORY.to_string());
    }
    labels
}

/// Write one zoom's cells as a GeoParquet table, rows in cell id order.
fn write_table(
    path: &Path,
    system: CellSystem,
    res: u8,
    cells: &CellMap,
    rollup: CellRollup<'_>,
    labels: &[String],
) -> Result<(), ConvertError> {
    let mut ids: Vec<u64> = cells.keys().copied().collect();
    ids.par_sort_unstable();
    let rows: Vec<&CellAcc> = ids.iter().map(|id| &cells[id]).collect();
    let geoms: Vec<Option<Geometry<f64>>> = ids
        .par_iter()
        .map(|&id| Some(Geometry::MultiPolygon(system.polygon(id, res))))
        .collect();

    let mut fields = vec![
        Field::new(CELL_COLUMN, DataType::Utf8, false),
        Field::new(POINT_COUNT_COLUMN, DataType::Int64, false),
    ];
    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(
            ids.iter().map(|&id| system.token(id, res)),
        )),
        Arc::new(Int64Array::from_iter_values(rows.iter().map(|c| c.count))),
    ];
    for (i, spec) in rollup.accumulate.iter().enumerate() {
        fields.push(Field::new(&spec.column, DataType::Float64, true));
        columns.push(Arc::new(arrow_array::Float64Array::from_iter(
            rows.iter().map(|c| c.aggs[i].finalize(spec.op)),
        )));
    }
    if let Some(col) = rollup.categories {
        for (slot, label) in labels.iter().enumerate() {
            fields.push(Field::new(format!("{col}:{label}"), DataType::Int64, true));
            // A zero count is null, as `--cluster-categories` writes it.
            columns.push(Arc::new(Int64Array::from_iter(rows.iter().map(|c| {
                c.cats
                    .as_slice()
                    .iter()
                    .find(|e| e.0 == slot as u32)
                    .map(|e| e.1)
            }))));
        }
    }
    let mut builder =
        GeometryBuilder::new(GeometryType::new(Default::default())).with_prefer_multi(false);
    builder.extend_from_iter(geoms.iter().map(Option::as_ref));
    let geom = builder.finish();
    fields.push(geom.data_type().to_field("geometry", true));
    columns.push(geom.to_array_ref());

    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns)?;
    use super::writer::WriterError;
    let gpq = |e: geoarrow::error::GeoArrowError| WriterError::GeoParquet(e.to_string());
    let options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &options).map_err(gpq)?;
    let (file, pending) = crate::atomic_output::create(path)?;
    let mut writer = tylertoo_io::batch_writer(file, encoder.target_schema())?;
    writer.write(&encoder.encode_record_batch(&batch).map_err(gpq)?)?;
    writer.append_key_value_metadata(encoder.into_keyvalue().map_err(gpq)?);
    writer.close()?;
    pending.publish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table entry is the resolution whose average cell area is
    /// nearest, in log ratio, to a 1/64-tile block at the equator.
    #[test]
    fn resolution_tables_follow_the_rule() {
        let sphere = 4.0 * std::f64::consts::PI * 6_371_008.8_f64.powi(2);
        let nearest = |target: f64, areas: &[f64]| {
            (0..areas.len())
                .min_by(|&a, &b| {
                    let d = |i: usize| (areas[i] / target).ln().abs();
                    d(a).total_cmp(&d(b))
                })
                .unwrap() as u8
        };
        let h3: Vec<f64> = (0..=15)
            .map(|r| h3o::Resolution::try_from(r).unwrap().area_m2())
            .collect();
        let s2: Vec<f64> = (0..=30).map(|l| sphere / (6.0 * 4f64.powi(l))).collect();
        let a5: Vec<f64> = (0..=29).map(a5::cell_area).collect();
        for z in 0..=30u8 {
            let side = 40_075_016.686 / 2f64.powi(i32::from(z) + 6);
            let target = side * side;
            assert_eq!(CellSystem::Grid.resolution(z), (z + 6).min(30), "grid z{z}");
            assert_eq!(
                CellSystem::H3.resolution(z),
                nearest(target, &h3),
                "h3 z{z}"
            );
            assert_eq!(
                CellSystem::S2.resolution(z),
                nearest(target, &s2),
                "s2 z{z}"
            );
            assert_eq!(
                CellSystem::A5.resolution(z),
                nearest(target, &a5),
                "a5 z{z}"
            );
        }
    }

    /// A finer zoom never aggregates coarser, so every zoom's cells roll up
    /// from the next finer zoom's.
    #[test]
    fn resolutions_never_coarsen_with_zoom() {
        for system in CellSystem::ALL {
            for z in 1..=30u8 {
                assert!(
                    system.resolution(z) >= system.resolution(z - 1),
                    "{system:?} z{z}"
                );
            }
        }
    }

    #[test]
    fn names_round_trip() {
        for system in CellSystem::ALL {
            assert_eq!(CellSystem::from_name(system.name()), Some(system));
        }
        assert_eq!(CellSystem::from_name("H3"), None);
        assert_eq!(CellSystem::from_name("hex"), None);
    }

    /// The parent by id holds the point for the nesting systems. H3 and A5
    /// parents are id parents, which can differ from the coarse cell holding
    /// the point near an edge, so only their resolution is checked.
    #[test]
    fn parents_contain_their_children() {
        let pts = [
            (-73.98, 40.75),
            (2.35, 48.86),
            (179.99, -16.5),
            (-0.0001, 0.0001),
        ];
        for system in CellSystem::ALL {
            for &(lon, lat) in &pts {
                let fine = system.resolution(14);
                let coarse = system.resolution(3);
                let id = system.cell(lon, lat, fine).unwrap();
                let parent = system.parent(id, fine, coarse);
                if system == CellSystem::H3 {
                    let p = h3o::CellIndex::try_from(parent).unwrap();
                    assert_eq!(u8::from(p.resolution()), coarse);
                } else if system == CellSystem::A5 {
                    let r = a5::core::serialization::get_resolution(parent);
                    assert_eq!(r, i32::from(coarse));
                } else {
                    assert_eq!(parent, system.cell(lon, lat, coarse).unwrap(), "{system:?}");
                }
            }
        }
    }

    #[test]
    fn unplaceable_points_have_no_cell() {
        for system in CellSystem::ALL {
            for (lon, lat) in [(f64::NAN, 0.0), (0.0, f64::INFINITY), (0.0, 91.0)] {
                assert_eq!(system.cell(lon, lat, 5), None, "{system:?}");
            }
            // Just past the antimeridian wraps in rather than vanishing.
            assert!(system.cell(180.000_000_1, 10.0, 5).is_some(), "{system:?}");
        }
    }

    /// Cell outlines are closed, inside the lon/lat extent, and contain the
    /// point that made them, across the antimeridian and at the poles.
    #[test]
    fn polygons_contain_their_point_everywhere() {
        use geo::{Area, Contains, Point};
        let pts = [
            (-73.98, 40.75),
            (179.99, -16.5),
            (-179.99, 65.0),
            (10.0, 89.9),
            (-150.0, -89.9),
            (0.0, 0.0),
        ];
        for system in CellSystem::ALL {
            for z in [0u8, 2, 7, 12] {
                for &(lon, lat) in &pts {
                    let res = system.resolution(z);
                    let id = system.cell(lon, lat, res).unwrap();
                    let poly = system.polygon(id, res);
                    assert!(poly.unsigned_area() > 0.0, "{system:?} z{z} ({lon},{lat})");
                    for p in &poly.0 {
                        assert!(p.exterior().is_closed());
                        for c in p.exterior().coords() {
                            assert!((-180.0..=180.0).contains(&c.x), "{system:?} {c:?}");
                            assert!((-90.0..=90.0).contains(&c.y), "{system:?} {c:?}");
                        }
                    }
                    // The grid clamps latitude to Web Mercator's extent.
                    if system == CellSystem::Grid && lat.abs() > 85.0 {
                        continue;
                    }
                    // Straight lon/lat edges stand in for great circles, so
                    // test a point nudged toward the cell's center.
                    let part = poly
                        .0
                        .iter()
                        .find(|p| {
                            let r = geo::BoundingRect::bounding_rect(*p).unwrap();
                            r.min().x <= lon && lon <= r.max().x
                        })
                        .unwrap();
                    let c = part.centroid().unwrap();
                    let probe = Point::new(lon + (c.x() - lon) * 1e-3, lat + (c.y() - lat) * 1e-3);
                    assert!(
                        poly.contains(&probe),
                        "{system:?} z{z} ({lon},{lat}) {poly:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn partial_cells_merge_in_any_order() {
        let mut a = CellAcc::new(1);
        a.count = 2;
        a.aggs[0].add(1.0);
        a.aggs[0].add(5.0);
        a.cats.add(0);
        a.cats.add(1);
        let mut b = CellAcc::new(1);
        b.count = 1;
        b.aggs[0].add(3.0);
        b.cats.add(1);
        let mut ab = a.clone();
        ab.merge(&b);
        let mut ba = b.clone();
        ba.merge(&a);
        assert_eq!(ab.count, 3);
        assert_eq!(ab.cats, ba.cats);
        for op in [
            super::super::cluster::AccumulateOp::Sum,
            super::super::cluster::AccumulateOp::Mean,
            super::super::cluster::AccumulateOp::Min,
            super::super::cluster::AccumulateOp::Max,
        ] {
            assert_eq!(ab.aggs[0].finalize(op), ba.aggs[0].finalize(op));
        }
        assert_eq!(
            ab.aggs[0].finalize(super::super::cluster::AccumulateOp::Mean),
            Some(3.0)
        );
    }
}
