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
//! through [`read_in_order`] once and each batch folds into a per-cell map.
//! The map is charged to the run's [`MemoryBudget`].
//!
//! Grid and S2 children tile their parent exactly, so the map holds the
//! band's finest resolution and every coarser zoom is a group-by of the
//! next finer zoom's cells on their parent id. H3 and A5 parents do not
//! contain their children, so there each feature counts directly in the
//! cell holding its point at every resolution the band draws; their ids
//! carry their resolution, so one map holds them all.

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
use super::read::{footer_price, grant_read_ahead, read_in_order, ReadFlow, ReadPass, ReadTuning};
use crate::input_set::ConvertSource;

/// A discrete global grid a cell band aggregates into.
///
/// Each feature counts in the cell holding its centroid, at every zoom.
/// Cells cover about a 4×4-pixel block of a 256-pixel tile at the equator;
/// resolutions follow the tables in this module and stop at each system's
/// resolution limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CellSystem {
    /// Web Mercator quadkey grid at zoom + 6, capped at 30.
    Grid,
    /// H3 hexagonal cells, with resolution chosen by equatorial cell area.
    H3,
    /// S2 quadrilateral cells, with resolution chosen by equatorial cell area.
    S2,
    /// A5 pentagonal cells, with resolution chosen by equatorial cell area.
    A5,
}

/// A `tiles` zoom range drawn as cells (#332): `--band LO-HI:SYSTEM`.
///
/// Each zoom holds one polygon per occupied cell of `system`, carrying
/// `point_count` and the `--accumulate-attribute` and
/// `--cluster-categories` rollups, in the tiles layer. See
/// [`TilesOptions::cell_bands`](crate::TilesOptions::cell_bands).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct CellBand {
    /// Coarsest zoom (inclusive).
    pub min_zoom: u8,
    /// Finest zoom (inclusive).
    pub max_zoom: u8,
    /// The grid the features are counted in.
    pub system: CellSystem,
}

impl CellBand {
    /// `system` cells at zooms `min_zoom..=max_zoom`.
    pub fn new(min_zoom: u8, max_zoom: u8, system: CellSystem) -> Self {
        Self {
            min_zoom,
            max_zoom,
            system,
        }
    }
}

/// The band as `--band` spells it: `LO-HI:SYSTEM`.
impl std::fmt::Display for CellBand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}-{}:{}",
            self.min_zoom,
            self.max_zoom,
            self.system.name()
        )
    }
}

/// Name of the cell id column every cell table carries.
pub(crate) const CELL_COLUMN: &str = "cell";

/// The columns a cell table writes besides its rollups.
pub(crate) const RESERVED_COLUMNS: [&str; 3] = [CELL_COLUMN, POINT_COUNT_COLUMN, "geometry"];

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
    /// Every supported cell system, in grid, H3, S2, A5 order.
    pub const ALL: [Self; 4] = [Self::Grid, Self::H3, Self::S2, Self::A5];

    /// The lowercase system name used in a band spec.
    pub fn name(self) -> &'static str {
        match self {
            Self::Grid => "grid",
            Self::H3 => "h3",
            Self::S2 => "s2",
            Self::A5 => "a5",
        }
    }

    /// Parse a lowercase system name, or return `None` for an unknown name.
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

    /// Whether every cell tiles its parent exactly, so a coarse cell is the
    /// sum of its children.
    fn nests(self) -> bool {
        matches!(self, Self::Grid | Self::S2)
    }

    /// The resolution an H3 or A5 id encodes; `None` for the others.
    fn id_resolution(self, id: u64) -> Option<u8> {
        match self {
            Self::H3 => h3o::CellIndex::try_from(id)
                .ok()
                .map(|c| u8::from(c.resolution())),
            Self::A5 => u8::try_from(a5::core::serialization::get_resolution(id)).ok(),
            Self::Grid | Self::S2 => None,
        }
    }

    /// The parent at `to` of cell `id` at `from` (`to <= from`), for a
    /// system that [`nests`](Self::nests).
    fn parent(self, id: u64, from: u8, to: u8) -> u64 {
        match self {
            Self::Grid => {
                let k = from - to;
                (id >> 32 >> k) << 32 | (id & 0xFFFF_FFFF) >> k
            }
            Self::S2 => s2::cellid::CellID(id).parent(u64::from(to)).0,
            Self::H3 | Self::A5 => unreachable!("only nesting systems roll up"),
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

/// Points from `a` to just before `b` along the great circle, at most a
/// degree apart.
fn great_circle(a: (f64, f64), b: (f64, f64)) -> Vec<(f64, f64)> {
    use geo::{Haversine, InterpolatePoint};
    let degree = Haversine.radius().to_radians();
    let (a, b) = (geo::Point::new(a.0, a.1), geo::Point::new(b.0, b.1));
    let mut out = vec![a.x_y()];
    out.extend(
        Haversine
            .points_along_line(a, b, degree, false)
            .map(|p| p.x_y()),
    );
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

/// Refuse rollups that would name two columns of a cell table (#332): a
/// table writes one column per spec, named after its source, beside its
/// own cell id, count, geometry and category columns.
///
/// # Errors
///
/// [`ConfigError::InvalidSpec`](super::config::ConfigError::InvalidSpec)
/// naming the column.
pub(crate) fn check_rollup_columns(
    rollup: CellRollup<'_>,
) -> Result<(), super::config::ConfigError> {
    let mut seen = std::collections::HashSet::new();
    let category_prefix = rollup.categories.map(|c| format!("{c}:"));
    for spec in rollup.accumulate {
        let counted = category_prefix
            .as_ref()
            .is_some_and(|p| spec.column.starts_with(p.as_str()));
        if !seen.insert(spec.column.as_str())
            || RESERVED_COLUMNS.contains(&spec.column.as_str())
            || counted
        {
            return Err(super::config::ConfigError::invalid_spec(
                "--accumulate-attribute",
                format!(
                    "column {:?} would name two columns of a cell band: aggregate \
                     each column once, and none named {} or {}LABEL",
                    spec.column,
                    RESERVED_COLUMNS.join(", "),
                    category_prefix.as_deref().unwrap_or("CATEGORY:")
                ),
            ));
        }
    }
    Ok(())
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

/// Rows per parallel task within a batch. Fixed, so float rollups add up
/// the same way on any number of cores.
const CHUNK_ROWS: usize = 1024;

/// Rows per read batch: enough chunks to keep every core busy.
const READ_ROWS: usize = 64 * CHUNK_ROWS;

/// Shards of the cell map, so batches and rollups merge on every core.
const SHARDS: usize = 64;

/// Which shard holds cell `id`: a multiplicative hash, since cell ids of
/// one area share their high bits.
fn shard_of(id: u64) -> usize {
    (id.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize
}

/// The cell map, split into [`SHARDS`] maps by [`shard_of`].
struct Cells(Vec<CellMap>);

impl Cells {
    fn new() -> Self {
        Self((0..SHARDS).map(|_| CellMap::new()).collect())
    }

    fn len(&self) -> usize {
        self.0.iter().map(HashMap::len).sum()
    }

    /// The cells a table at `res` writes: all of them for a nesting
    /// system, else those whose id is at `res`.
    fn at(&self, system: CellSystem, res: u8) -> impl Iterator<Item = (u64, &CellAcc)> {
        self.0
            .iter()
            .flat_map(HashMap::iter)
            .filter_map(move |(&id, acc)| {
                system
                    .id_resolution(id)
                    .is_none_or(|r| r == res)
                    .then_some((id, acc))
            })
    }

    fn len_at(&self, system: CellSystem, res: u8) -> usize {
        self.at(system, res).count()
    }

    /// Drop the cells finer than `res`, once their table is written.
    fn retain_at_most(&mut self, system: CellSystem, res: u8) {
        self.0.par_iter_mut().for_each(|m| {
            m.retain(|&id, _| system.id_resolution(id).is_none_or(|r| r <= res));
        });
    }

    /// Merge partial maps, each split by shard, into these cells: shard by
    /// shard in parallel, the parts in order, so the result is the same on
    /// every run. Returns the category heap bytes added.
    fn absorb(&mut self, parts: Vec<Vec<CellMap>>) -> u64 {
        let mut by_shard: Vec<Vec<CellMap>> = (0..SHARDS).map(|_| Vec::new()).collect();
        for part in parts {
            for (s, map) in part.into_iter().enumerate() {
                by_shard[s].push(map);
            }
        }
        self.0
            .par_iter_mut()
            .zip(by_shard)
            .map(|(shard, maps)| {
                let mut bytes = 0u64;
                for map in maps {
                    for (id, acc) in map {
                        match shard.get_mut(&id) {
                            Some(c) => {
                                let before = c.cats.heap_bytes();
                                c.merge(&acc);
                                bytes += c.cats.heap_bytes() - before;
                            }
                            None => {
                                bytes += acc.cats.heap_bytes();
                                shard.insert(id, acc);
                            }
                        }
                    }
                }
                bytes
            })
            .sum()
    }

    /// The cells at resolution `to`, each the merge of its children at
    /// `from` in child id order (so a float sum adds up the same way every
    /// run). Consumes the children shard by shard.
    fn roll_up(self, system: CellSystem, from: u8, to: u8) -> Self {
        let parts: Vec<Vec<CellMap>> = self
            .0
            .into_par_iter()
            .map(|shard| {
                let mut out: Vec<CellMap> = (0..SHARDS).map(|_| CellMap::new()).collect();
                let mut children: Vec<(u64, CellAcc)> = shard.into_iter().collect();
                children.sort_unstable_by_key(|c| c.0);
                for (id, acc) in children {
                    let parent = system.parent(id, from, to);
                    out[shard_of(parent)]
                        .entry(parent)
                        .and_modify(|p| p.merge(&acc))
                        .or_insert(acc);
                }
                out
            })
            .collect();
        let mut parents = Self::new();
        parents.absorb(parts);
        parents
    }

    /// Heap bytes of every cell's category counts, by allocated capacity.
    fn category_bytes(&self) -> u64 {
        self.0
            .par_iter()
            .map(|m| m.values().map(|c| c.cats.heap_bytes()).sum::<u64>())
            .sum()
    }

    /// What holding these cells costs (#332): every shard's slots and
    /// `category_bytes` of counts, with room for `incoming[s]` more cells in
    /// shard `s`, each with its first counts. A shard that must grow to
    /// take them holds its old and its doubled table at once.
    fn bytes(&self, specs: usize, category_bytes: u64, incoming: &[usize]) -> u64 {
        let incoming = incoming.iter().copied().chain(std::iter::repeat(0));
        let (slots, new) =
            self.0
                .iter()
                .zip(incoming)
                .fold((0u64, 0u64), |(slots, new), (m, k)| {
                    let (cap, need) = (m.capacity() as u64, (m.len() + k) as u64);
                    let held = if need > cap {
                        cap + 2 * cap.max(need)
                    } else {
                        cap
                    };
                    (slots + held, new + k as u64)
                });
        RSS_PER_PRICED_BYTE
            * (slots * cell_bytes(specs) + category_bytes + new * FIRST_COUNTS_BYTES)
    }
}

/// Resident bytes per byte the cells are priced at. Measured on 10.1M points
/// in 927k grid cells with 275 categories (#332): 694 MB resident after the
/// scan against 308 MB priced plus 37 MB of read-ahead. The gap is the
/// allocator holding the batches' freed partial maps and the map's
/// pre-growth tables, and each cell's own small allocations.
const RSS_PER_PRICED_BYTE: u64 = 2;

/// Resident bytes each pool thread's allocator heap holds while it builds a
/// batch's partial maps. Measured on 10.1M points (#332): 454 MB resident at
/// the first batch under 64 threads, 168 MB under 8.
const THREAD_HEAP_BYTES: u64 = 5 << 20;

/// What the pool's allocator heaps hold while `threads` aggregate batches
/// of `batch_rows` rows: one [`THREAD_HEAP_BYTES`] per thread with a chunk.
fn thread_heap_bytes(batch_rows: usize, threads: usize) -> u64 {
    batch_rows.div_ceil(CHUNK_ROWS).min(threads) as u64 * THREAD_HEAP_BYTES
}

/// Heap bytes of a cell's first category count: a `Vec` grows from empty to
/// four `(u32, i64)` slots.
const FIRST_COUNTS_BYTES: u64 = 4 * std::mem::size_of::<(u32, i64)>() as u64;

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
/// centroid). With `gates` (#332), only the rows `--filter`, `--bbox` and
/// `--mask` keep are counted, each kept or dropped by its own geometry as
/// conversion keeps it, and only the row groups they select are read.
/// A feature with no usable geometry is skipped and counted on
/// [`Diagnostic::UnusableGeometry`](crate::Diagnostic::UnusableGeometry).
///
/// # Errors
///
/// The input cannot be read, its CRS is unsupported, a rollup column is
/// missing or of the wrong type, or the cell map outgrows the memory budget.
#[expect(
    clippy::too_many_arguments,
    reason = "#332: a cell band's input, gates, grid, zooms, rollups and outputs"
)]
pub(crate) fn write_cell_tables(
    source: &ConvertSource,
    gates: Option<&super::layer_column::Gates>,
    system: CellSystem,
    (min_zoom, max_zoom): (u8, u8),
    rollup: CellRollup<'_>,
    tables: &[&Path],
    read: &ConvertOptions,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(), ConvertError> {
    debug_assert_eq!(tables.len(), usize::from(max_zoom - min_zoom) + 1);
    let started = std::time::Instant::now();
    // A nesting system aggregates at the finest resolution and rolls up;
    // the others count each feature at every resolution the band draws.
    let mut resolutions: Vec<u8> = if system.nests() {
        vec![system.resolution(max_zoom)]
    } else {
        (min_zoom..=max_zoom)
            .map(|z| system.resolution(z))
            .collect()
    };
    resolutions.dedup();
    let (mut cells, labels, budget, mut cat_bytes) = aggregate(
        source,
        gates,
        system,
        &resolutions,
        rollup,
        read,
        diagnostics,
    )?;
    let specs = rollup.accumulate.len();
    log::info!(
        "[cells] {} aggregate: {} cell(s) up to z{max_zoom} in {:.1}s, priced {} B",
        system.name(),
        cells.len(),
        started.elapsed().as_secs_f64(),
        cells.bytes(specs, cat_bytes, &[])
    );
    let mut res = system.resolution(max_zoom);
    for z in (min_zoom..=max_zoom).rev() {
        let to = system.resolution(z);
        if to < res && !system.nests() {
            // The finer cells are written; the coarser ones were counted.
            cells.retain_at_most(system, to);
            cat_bytes = cells.category_bytes();
            res = to;
        } else if to < res {
            // A rollup holds children and their parents at once; a parent
            // holds no more cells or category pairs than its children.
            budget.require_allowance(
                read.profile,
                MemoryPhase::Scan,
                2 * cells.bytes(specs, cat_bytes, &[]),
            )?;
            cells = cells.roll_up(system, res, to);
            cat_bytes = cells.category_bytes();
            res = to;
        }
        // Writing adds an id-ordered index of the cells and one batch's
        // columns: counts (8 bytes plus null bits), rollups, id and outline.
        let batch_bytes = cells.len().min(WRITE_ROWS) as u64
            * (17 * labels.len() as u64 + 9 * specs as u64 + 512);
        let priced = cells.bytes(specs, cat_bytes, &[]) + cells.len() as u64 * 16 + batch_bytes;
        budget.require_allowance(read.profile, MemoryPhase::Scan, priced)?;
        write_table(
            tables[usize::from(z - min_zoom)],
            system,
            res,
            &cells,
            rollup,
            &labels,
        )?;
        log::info!(
            "[cells] z{z}: {} cell(s), {} label(s), total {:.1}s, priced {priced} B",
            cells.len_at(system, res),
            labels.len(),
            started.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

/// Read `source` once into the cell map at `max_zoom`'s resolution, the
/// rows `gates` keep only. Returns the map, its category counts keyed by
/// category slot, the slot labels, the budget it was charged to, and its
/// category heap bytes.
fn aggregate(
    source: &ConvertSource,
    gates: Option<&super::layer_column::Gates>,
    system: CellSystem,
    resolutions: &[u8],
    rollup: CellRollup<'_>,
    read: &ConvertOptions,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(Cells, Vec<String>, MemoryBudget, u64), ConvertError> {
    source.set_spill_dir(read.spill_dir.as_deref());
    let descriptor = source.resolved_schema()?;
    let (crs, assumed) =
        super::convert::crs_from_resolved(descriptor, &source.display_name(), diagnostics)?;
    if assumed {
        super::convert::warn_assumed_wgs84(source, diagnostics);
    }
    let filter = gates.and_then(|g| g.filter.as_ref());
    let (bbox, mask) = (
        gates.and_then(|g| g.bbox.as_ref()),
        gates.and_then(|g| g.mask.as_ref()),
    );
    let row_groups = gates.and_then(|g| g.row_groups.as_ref());
    let geom_idx = descriptor
        .geometry_index()
        .ok_or_else(|| ConvertError::NoGeometryColumn {
            input: source.display_name(),
        })?;
    // The rollup columns are checked exactly as `--cluster` checks them.
    let check = ConvertOptions {
        accumulate: rollup.accumulate.to_vec(),
        cluster_categories: rollup.categories.map(str::to_string),
        ..ConvertOptions::default()
    };
    // A cell table drops the input's other columns: they cannot collide.
    let acc_idx = super::convert::validate_rollup_columns(descriptor.schema(), &check, false)?;
    let cat_idx = super::convert::cluster_category_column(descriptor.schema(), &check);
    let mut projection: Vec<usize> = std::iter::once(geom_idx)
        .chain(acc_idx.iter().copied())
        .chain(cat_idx)
        .chain(filter.map_or(&[][..], |f| f.columns()).iter().copied())
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

    let specs = rollup.accumulate.len();
    let mut budget = MemoryBudget::probe();
    // A small input is priced as the batches it can fill, not as full ones.
    let total_rows = source.selected_row_count(row_groups)?;
    let tuning = ReadTuning {
        budget,
        batch_size: READ_ROWS.min(usize::try_from(total_rows).unwrap_or(READ_ROWS).max(1)),
        workers: super::convert::resolve_pass_read_workers(ReadPass::Scan, read.read_workers),
        avg_geom_bytes: None,
        footer: footer_price(source, row_groups, projection.iter().copied())?,
        profile: read.profile,
    };
    // Every reader's queue, plus the batch being aggregated.
    let read_ahead = grant_read_ahead(&mut budget, tuning, 1);
    // Checked before the readers start, so a budget too small for the pool
    // fails here. Only as many threads work as a batch has chunks.
    let threads = thread_heap_bytes(tuning.batch_size, rayon::current_num_threads());
    budget.require_allowance(read.profile, MemoryPhase::Scan, threads)?;
    let mut cells = Cells::new();
    let mut interner = GroupInterner::default();
    let (mut rows, mut skipped, mut cat_bytes) = (0usize, 0usize, 0u64);
    if gates.is_some() {
        super::stream::stage_for_read(
            source,
            &crate::input_set::ReadPlan {
                batch_size: tuning.batch_size,
                projection: Some(&projection),
                row_groups,
            },
            read.profile,
            "cells",
        );
    }
    read_in_order(
        source,
        row_groups,
        Some(&projection),
        tuning,
        |batch, _, _| {
            let n = batch.num_rows();
            rows += n;
            // Checked before the batch lands, so a cell map that would outgrow
            // the budget fails here, not by OOM kill: first for the batch's
            // partial maps at worst (a new cell per row, in a table that may be
            // mid-doubling: three slots a row), then, once they are built, for
            // them as they are and the shards they grow.
            let per_row = resolutions.len() as u64 * (3 * cell_bytes(specs) + FIRST_COUNTS_BYTES);
            budget.require_allowance(
                read.profile,
                MemoryPhase::Scan,
                threads
                    + cells.bytes(specs, cat_bytes, &[])
                    + interner.heap_bytes()
                    + n as u64 * per_row,
            )?;
            let bschema = batch.schema();
            let col = |name: &str| bschema.index_of(name).map(|i| batch.column(i).clone());
            let geom_i = bschema.index_of(&geom_name)?;
            let geom_field = bschema.field(geom_i).clone();
            let geom_col = batch.column(geom_i).clone();
            // The batch holds the projected columns, in projection order. SQL
            // semantics: a row the filter cannot decide is dropped.
            let keep = filter.map(|f| {
                f.eval_mask(&batch, &|i| {
                    projection
                        .binary_search(&i)
                        .expect("the filter's columns are projected")
                })
            });
            let outside = match (bbox, mask) {
                (None, None) => Vec::new(),
                _ => super::layer_column::outside_gates(&batch, geom_i, bbox, mask)?,
            };
            let gated = |row: usize| {
                keep.as_ref().is_some_and(|k| k[row] != Some(true))
                    || outside.get(row) == Some(&true)
            };
            let values: Vec<ArrayRef> = acc_names
                .iter()
                .map(|name| Ok(arrow_cast::cast(&col(name)?, &DataType::Float64)?))
                .collect::<Result<_, ConvertError>>()?;
            let mut ids: Vec<u32> = Vec::new();
            if let Some(name) = &cat_name {
                interner.extend(col(name)?.as_ref(), &mut ids);
            }
            let chunks: Vec<(Vec<CellMap>, usize)> = (0..n)
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
                    let mut local: Vec<CellMap> = (0..SHARDS).map(|_| CellMap::new()).collect();
                    let mut bad = 0usize;
                    for (r, g) in geoms.iter().enumerate() {
                        if gated(off + r) {
                            continue;
                        }
                        let Some(cells) = g
                            .as_ref()
                            .and_then(|g| cells_of(system, crs, g, resolutions))
                        else {
                            bad += 1;
                            continue;
                        };
                        let row = off + r;
                        for id in cells {
                            let acc = local[shard_of(id)]
                                .entry(id)
                                .or_insert_with(|| CellAcc::new(specs));
                            acc.count += 1;
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
                    }
                    Ok((local, bad))
                })
                .collect::<Result<_, ConvertError>>()?;
            skipped += chunks.iter().map(|c| c.1).sum::<usize>();
            let mut incoming = vec![0usize; SHARDS];
            let mut partial = 0u64;
            for (maps, _) in &chunks {
                for (s, m) in maps.iter().enumerate() {
                    incoming[s] += m.len();
                    partial += m.capacity() as u64 * cell_bytes(specs)
                        + m.values().map(|c| c.cats.heap_bytes()).sum::<u64>();
                }
            }
            let need = threads
                + cells.bytes(specs, cat_bytes, &incoming)
                + interner.heap_bytes()
                + partial;
            budget.require_allowance(read.profile, MemoryPhase::Scan, need)?;
            cat_bytes += cells.absorb(chunks.into_iter().map(|c| c.0).collect());
            Ok(ReadFlow::Continue)
        },
    )?;
    budget.release(read_ahead);
    if skipped > 0 {
        diagnostics.once(crate::diagnostics::Diagnostic::UnusableGeometry {
            count: skipped,
            total: rows,
        });
    }
    let labels = if cat_name.is_some() {
        let labels = relabel_categories(&mut cells, &interner.into_values());
        cat_bytes = cells.category_bytes();
        labels
    } else {
        Vec::new()
    };
    Ok((cells, labels, budget, cat_bytes))
}

/// The cells at each of `resolutions` that hold `g`'s representative
/// point, `None` when the system cannot place it.
fn cells_of(
    system: CellSystem,
    crs: Crs,
    g: &Geometry<f64>,
    resolutions: &[u8],
) -> Option<Vec<u64>> {
    let (x, y) = representative_point(g)?;
    let (lon, lat) = match crs {
        Crs::Epsg3857 => super::export::webmerc_to_lnglat(x, y),
        _ => (x, y),
    };
    resolutions
        .iter()
        .map(|&res| system.cell(lon, lat, res))
        .collect()
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
fn relabel_categories(cells: &mut Cells, values: &[String]) -> Vec<String> {
    let null = GroupInterner::NULL_GROUP;
    let mut freq = vec![0u64; values.len()];
    for acc in cells.0.iter().flat_map(HashMap::values) {
        for &(id, n) in acc.cats.as_slice() {
            if id != null {
                freq[id as usize] += n as u64;
            }
        }
    }
    let (slot_of, mut labels) = rank_categories(&freq, values, MAX_CLUSTER_CATEGORIES);
    let other = labels.len() as u32;
    let uses_other = cells
        .0
        .par_iter_mut()
        .map(|shard| {
            let mut uses_other = false;
            for acc in shard.values_mut() {
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
            uses_other
        })
        .reduce(|| false, |a, b| a || b);
    if uses_other {
        labels.push(OTHER_CATEGORY.to_string());
    }
    labels
}

/// Rows per written batch: bounds the output buffers to a slice of cells.
const WRITE_ROWS: usize = 65_536;

/// Write one zoom's cells as a GeoParquet table, rows in cell id order.
fn write_table(
    path: &Path,
    system: CellSystem,
    res: u8,
    cells: &Cells,
    rollup: CellRollup<'_>,
    labels: &[String],
) -> Result<(), ConvertError> {
    use super::writer::WriterError;
    let gpq = |e: geoarrow::error::GeoArrowError| WriterError::GeoParquet(e.to_string());
    let geom_type = GeometryType::new(Default::default());
    let mut fields = vec![
        Field::new(CELL_COLUMN, DataType::Utf8, false),
        Field::new(POINT_COUNT_COLUMN, DataType::Int64, false),
    ];
    for spec in rollup.accumulate {
        fields.push(Field::new(&spec.column, DataType::Float64, true));
    }
    if let Some(col) = rollup.categories {
        for label in labels {
            fields.push(Field::new(format!("{col}:{label}"), DataType::Int64, true));
        }
    }
    let geom_field = GeometryBuilder::new(geom_type.clone())
        .finish()
        .data_type()
        .to_field("geometry", true);
    fields.push(geom_field);
    let schema = Arc::new(Schema::new(fields));

    let options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &options).map_err(gpq)?;
    let (file, pending) = crate::atomic_output::create(path)?;
    let mut writer = tylertoo_io::batch_writer(file, encoder.target_schema())?;

    let mut all: Vec<(u64, &CellAcc)> = cells.at(system, res).collect();
    all.par_sort_unstable_by_key(|c| c.0);
    for chunk in all.chunks(WRITE_ROWS) {
        let ids: Vec<u64> = chunk.iter().map(|c| c.0).collect();
        let rows: Vec<&CellAcc> = chunk.iter().map(|c| c.1).collect();
        let tokens: Vec<String> = ids.par_iter().map(|&id| system.token(id, res)).collect();
        let mut columns: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(tokens)),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|c| c.count))),
        ];
        for (i, spec) in rollup.accumulate.iter().enumerate() {
            columns.push(Arc::new(arrow_array::Float64Array::from_iter(
                rows.iter().map(|c| c.aggs[i].finalize(spec.op)),
            )));
        }
        if rollup.categories.is_some() {
            // One pass over the cells' counts, scattered into a column per
            // slot; a zero count is null, as `--cluster-categories` writes it.
            let n = rows.len();
            let mut counts = vec![0i64; labels.len() * n];
            for (r, c) in rows.iter().enumerate() {
                for &(slot, k) in c.cats.as_slice() {
                    counts[slot as usize * n + r] = k;
                }
            }
            let arrays: Vec<ArrayRef> = counts
                .par_chunks(n.max(1))
                .take(labels.len())
                .map(|col| {
                    Arc::new(Int64Array::from_iter(
                        col.iter().map(|&k| (k != 0).then_some(k)),
                    )) as ArrayRef
                })
                .collect();
            columns.extend(arrays);
        }
        let geoms: Vec<Geometry<f64>> = ids
            .par_iter()
            .map(|&id| Geometry::MultiPolygon(system.polygon(id, res)))
            .collect();
        let mut builder = GeometryBuilder::new(geom_type.clone()).with_prefer_multi(false);
        builder.extend_from_iter(geoms.iter().map(Some));
        columns.push(builder.finish().to_array_ref());
        let batch = RecordBatch::try_new(schema.clone(), columns)?;
        writer.write(&encoder.encode_record_batch(&batch).map_err(gpq)?)?;
        // One row group per batch, so the writer buffers one batch at most.
        writer.flush()?;
    }
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

    /// A nesting system's parent by id holds the point.
    #[test]
    fn parents_contain_their_children() {
        let pts = [
            (-73.98, 40.75),
            (2.35, 48.86),
            (179.99, -16.5),
            (-0.0001, 0.0001),
        ];
        for system in CellSystem::ALL.into_iter().filter(|s| s.nests()) {
            for &(lon, lat) in &pts {
                let fine = system.resolution(14);
                let coarse = system.resolution(3);
                let id = system.cell(lon, lat, fine).unwrap();
                let parent = system.parent(id, fine, coarse);
                assert_eq!(parent, system.cell(lon, lat, coarse).unwrap(), "{system:?}");
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

    /// At every zoom of a band, each feature counts in the cell that holds
    /// its point, for every system: H3 and A5 parents do not contain their
    /// children, so a coarse cell cannot be a rollup of fine ones there.
    #[test]
    fn every_zoom_counts_each_point_in_the_cell_that_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("pts.parquet");
        // A 60x60 lattice over 6 degrees: plenty of points near coarse edges.
        let pts: Vec<(f64, f64)> = (0..3600)
            .map(|i| (-56.0 + (i % 60) as f64 * 0.1, -13.0 + (i / 60) as f64 * 0.1))
            .collect();
        let geoms: Vec<Option<Geometry<f64>>> = pts
            .iter()
            .map(|&(x, y)| Some(Geometry::Point(geo::Point::new(x, y))))
            .collect();
        crate::overview::testutil::write_input(&src, &geoms, false, None);
        let rollup = CellRollup {
            accumulate: &[],
            categories: None,
        };
        let (lo, hi) = (0u8, 7u8);
        for system in CellSystem::ALL {
            let tables: Vec<_> = (lo..=hi)
                .map(|z| dir.path().join(format!("{}-{z}.parquet", system.name())))
                .collect();
            let paths: Vec<&Path> = tables.iter().map(|t| t.as_path()).collect();
            write_cell_tables(
                &ConvertSource::resolve_path(&src).unwrap(),
                None,
                system,
                (lo, hi),
                rollup,
                &paths,
                &ConvertOptions::default(),
                &crate::diagnostics::Diagnostics::default(),
            )
            .unwrap();
            for (z, table) in (lo..=hi).zip(&tables) {
                let res = system.resolution(z);
                let mut want: HashMap<String, i64> = HashMap::new();
                for &(lon, lat) in &pts {
                    let id = system.cell(lon, lat, res).unwrap();
                    *want.entry(system.token(id, res)).or_default() += 1;
                }
                let source = ConvertSource::resolve_path(table).unwrap();
                let plan = crate::input_set::ReadPlan {
                    batch_size: 1 << 20,
                    projection: None,
                    row_groups: None,
                };
                let mut got: HashMap<String, i64> = HashMap::new();
                for batch in source.open_stream(&plan).unwrap() {
                    let batch = batch.unwrap();
                    let col =
                        |name: &str| batch.column(batch.schema().index_of(name).unwrap()).clone();
                    let counts = col(POINT_COUNT_COLUMN);
                    let counts = counts.as_primitive::<arrow_array::types::Int64Type>();
                    for (t, n) in col(CELL_COLUMN).as_string::<i32>().iter().zip(counts) {
                        got.insert(t.unwrap().to_string(), n.unwrap());
                    }
                }
                assert_eq!(got, want, "{system:?} z{z}");
            }
        }
    }

    /// A table longer than one written batch keeps every cell, once, in
    /// cell id order.
    #[test]
    fn tables_span_several_batches() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("pts.parquet");
        let n = WRITE_ROWS + 4_464;
        let geoms: Vec<Option<Geometry<f64>>> = (0..n)
            .map(|i| {
                let (x, y) = ((i % 300) as f64 * 0.01, (i / 300) as f64 * 0.01);
                Some(Geometry::Point(geo::Point::new(x, y)))
            })
            .collect();
        crate::overview::testutil::write_input(&src, &geoms, false, None);
        let table = dir.path().join("z12.parquet");
        let rollup = CellRollup {
            accumulate: &[],
            categories: None,
        };
        let diagnostics = crate::diagnostics::Diagnostics::default();
        write_cell_tables(
            &ConvertSource::resolve_path(&src).unwrap(),
            None,
            CellSystem::Grid,
            (12, 12),
            rollup,
            &[&table],
            &ConvertOptions::default(),
            &diagnostics,
        )
        .unwrap();
        let source = ConvertSource::resolve_path(&table).unwrap();
        let plan = crate::input_set::ReadPlan {
            batch_size: 1 << 20,
            projection: None,
            row_groups: None,
        };
        let mut cells: Vec<(u64, u64)> = Vec::new();
        let mut total = 0i64;
        for batch in source.open_stream(&plan).unwrap() {
            let batch = batch.unwrap();
            let col = |name: &str| batch.column(batch.schema().index_of(name).unwrap()).clone();
            let tokens = col(CELL_COLUMN);
            for t in tokens.as_string::<i32>().iter().flatten() {
                let mut parts = t.split('/').skip(1).map(|v| v.parse::<u64>().unwrap());
                cells.push((parts.next().unwrap(), parts.next().unwrap()));
            }
            total += col(POINT_COUNT_COLUMN)
                .as_primitive::<arrow_array::types::Int64Type>()
                .iter()
                .flatten()
                .sum::<i64>();
        }
        assert_eq!(total, n as i64);
        assert_eq!(
            cells.len(),
            n,
            "0.01° apart, every point has its own z12 cell"
        );
        assert!(
            cells.windows(2).all(|w| w[0] < w[1]),
            "rows in cell id order"
        );
        let groups = source.num_row_groups_total().unwrap();
        assert_eq!(groups, 2, "one row group per written batch");
    }

    /// A float sum adds up the same way on any number of cores, even where
    /// the order of additions decides the result.
    #[test]
    fn sums_do_not_depend_on_core_count() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("pts.parquet");
        let n = 3 * READ_ROWS;
        let geoms = vec![Some(Geometry::Point(geo::Point::new(1.0, 1.0))); n];
        let mut values = vec![0.0; n];
        // Chunked by core count, 1 core sums these to 1 and 16 cores to 0.
        for (row, v) in [
            (0, 1e16),
            (READ_ROWS / 64, -1e16),
            (READ_ROWS / 64 + 1, 1.0),
        ] {
            values[row] = v;
        }
        crate::overview::testutil::write_input_with_f64(&src, &geoms, "v", &values);
        let spec = [AccumulateSpec::new(
            "v",
            super::super::cluster::AccumulateOp::Sum,
        )];
        let rollup = CellRollup {
            accumulate: &spec,
            categories: None,
        };
        let sum_on = |threads: usize| {
            let table = dir.path().join(format!("t{threads}.parquet"));
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let diagnostics = crate::diagnostics::Diagnostics::default();
            pool.install(|| {
                write_cell_tables(
                    &ConvertSource::resolve_path(&src).unwrap(),
                    None,
                    CellSystem::S2,
                    (3, 3),
                    rollup,
                    &[&table],
                    &ConvertOptions::default(),
                    &diagnostics,
                )
            })
            .unwrap();
            std::fs::read(&table).unwrap()
        };
        assert_eq!(sum_on(1), sum_on(16));
    }

    /// A cell table drops the input's other columns, so one named like a
    /// category count column (`name:legacy` beside `name`) is no collision.
    #[test]
    fn unrelated_category_named_columns_are_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("pts.parquet");
        let geoms = vec![Some(Geometry::Point(geo::Point::new(1.0, 1.0))); 3];
        crate::overview::testutil::write_input(&src, &geoms, false, Some("name:legacy"));
        let table = dir.path().join("t.parquet");
        let rollup = CellRollup {
            accumulate: &[],
            categories: Some("name"),
        };
        let diagnostics = crate::diagnostics::Diagnostics::default();
        let read = ConvertOptions::default();
        write_cell_tables(
            &ConvertSource::resolve_path(&src).unwrap(),
            None,
            CellSystem::S2,
            (3, 3),
            rollup,
            &[&table],
            &read,
            &diagnostics,
        )
        .unwrap();
    }

    /// Category counts are priced by what they allocate: a first count
    /// takes four 16-byte `(u32, i64)` slots, and the tracked total matches
    /// a recount after batches merge.
    #[test]
    fn category_bytes_are_priced_by_capacity() {
        let mut one = CategoryCounts::default();
        one.add(7);
        assert_eq!(one.heap_bytes(), FIRST_COUNTS_BYTES);
        assert_eq!(FIRST_COUNTS_BYTES, 64);
        for slot in 0..5 {
            one.add(slot);
        }
        assert_eq!(
            one.heap_bytes(),
            8 * 16,
            "six pairs grow the Vec to eight slots"
        );

        let part = |ids: &[u64], slot: u32| {
            let mut maps: Vec<CellMap> = (0..SHARDS).map(|_| CellMap::new()).collect();
            for &id in ids {
                let acc = maps[shard_of(id)]
                    .entry(id)
                    .or_insert_with(|| CellAcc::new(0));
                acc.count += 1;
                acc.cats.add(slot);
            }
            maps
        };
        let mut cells = Cells::new();
        let tracked = cells.absorb(vec![part(&[1, 2, 3], 0), part(&[2, 3, 4], 1)]);
        assert_eq!(tracked, cells.category_bytes());
        assert_eq!(tracked, 4 * FIRST_COUNTS_BYTES);

        // Ten new cells in an empty shard: it grows to hold them, old and
        // doubled table at once, and each brings its first counts; all of
        // it resident at the measured allocator overhead.
        let empty = Cells::new();
        assert_eq!(
            empty.bytes(0, 0, &[10]),
            RSS_PER_PRICED_BYTE * (20 * cell_bytes(0) + 10 * FIRST_COUNTS_BYTES)
        );

        // A full shard grows even when the others have room to spare.
        let mut uneven = Cells::new();
        uneven.0[1] = CellMap::with_capacity(4096);
        let mut id = 0u64;
        while uneven.0[0].len() < uneven.0[0].capacity().max(1) {
            uneven.0[0].insert(id, CellAcc::new(0));
            id += 1;
        }
        let cap0 = uneven.0[0].capacity() as u64;
        let still = uneven.bytes(0, 0, &[]);
        let grown = uneven.bytes(0, 0, &[1]);
        assert!(
            grown - still >= 2 * cap0 * cell_bytes(0),
            "{still} -> {grown}"
        );
    }

    /// A small input is priced for the threads it can keep busy, not the
    /// whole pool.
    #[test]
    fn thread_heaps_follow_the_chunks() {
        assert_eq!(thread_heap_bytes(1, 64), THREAD_HEAP_BYTES);
        assert_eq!(thread_heap_bytes(3 * CHUNK_ROWS, 64), 3 * THREAD_HEAP_BYTES);
        assert_eq!(thread_heap_bytes(READ_ROWS, 8), 8 * THREAD_HEAP_BYTES);
    }
}
