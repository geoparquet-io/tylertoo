//! GeoParquet covering metadata extraction for row group filtering.
//!
//! This module extracts bounding box information from GeoParquet files to enable
//! spatial filtering of row groups. When processing bounded extracts (e.g., a city
//! from a country file), we can skip entire row groups whose bboxes don't intersect
//! the area of interest.
//!
//! # GeoParquet Covering Metadata
//!
//! GeoParquet 1.1.0+ supports a `covering` field that specifies which columns contain
//! bounding box information:
//!
//! ```json
//! {
//!   "columns": {
//!     "geometry": {
//!       "covering": {
//!         "bbox": {
//!           "xmin": ["geometry_bbox", "xmin"],
//!           "ymin": ["geometry_bbox", "ymin"],
//!           "xmax": ["geometry_bbox", "xmax"],
//!           "ymax": ["geometry_bbox", "ymax"]
//!         }
//!       }
//!     }
//!   }
//! }
//! ```
//!
//! # Row Group Bbox Extraction
//!
//! For each row group, we extract the bounding box from column statistics:
//! - `xmin` column: MIN statistic (smallest x in row group)
//! - `ymin` column: MIN statistic (smallest y in row group)
//! - `xmax` column: MAX statistic (largest x in row group)
//! - `ymax` column: MAX statistic (largest y in row group)
//!
//! This approach has ~4% overhead compared to baseline metadata reads, and is
//! 370x faster than scanning column values.

use crate::overview::level::Crs;
use crate::tile::{lng_lat_to_tile, TileBounds, TileCoord};
use crate::Error;
use parquet::basic::LogicalType;
use parquet::file::metadata::ParquetMetaData;
use parquet::file::reader::{FileReader, SerializedFileReader};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

/// Specification for bbox covering columns parsed from GeoParquet metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct CoveringSpec {
    /// Column path for xmin values (e.g., "geometry_bbox.xmin")
    pub xmin_path: Vec<String>,
    /// Column path for ymin values
    pub ymin_path: Vec<String>,
    /// Column path for xmax values
    pub xmax_path: Vec<String>,
    /// Column path for ymax values
    pub ymax_path: Vec<String>,
}

impl CoveringSpec {
    /// Create a new CoveringSpec from column paths.
    pub fn new(
        xmin_path: Vec<String>,
        ymin_path: Vec<String>,
        xmax_path: Vec<String>,
        ymax_path: Vec<String>,
    ) -> Self {
        Self {
            xmin_path,
            ymin_path,
            xmax_path,
            ymax_path,
        }
    }

    /// Convert a column path to a dotted string for matching against Parquet column paths.
    fn path_to_string(path: &[String]) -> String {
        path.join(".")
    }

    /// Get the dotted xmin column path.
    pub fn xmin_column(&self) -> String {
        Self::path_to_string(&self.xmin_path)
    }

    /// Get the dotted ymin column path.
    pub fn ymin_column(&self) -> String {
        Self::path_to_string(&self.ymin_path)
    }

    /// Get the dotted xmax column path.
    pub fn xmax_column(&self) -> String {
        Self::path_to_string(&self.xmax_path)
    }

    /// Get the dotted ymax column path.
    pub fn ymax_column(&self) -> String {
        Self::path_to_string(&self.ymax_path)
    }
}

/// Bounding box for a single row group.
#[derive(Debug, Clone, PartialEq)]
pub struct RowGroupBounds {
    /// Row group index (0-based)
    pub row_group_idx: usize,
    /// Minimum longitude
    pub xmin: f64,
    /// Minimum latitude
    pub ymin: f64,
    /// Maximum longitude
    pub xmax: f64,
    /// Maximum latitude
    pub ymax: f64,
    /// Number of rows in this row group (for density estimation)
    pub num_rows: usize,
}

impl RowGroupBounds {
    /// True when a corner of the stats bbox is a NaN, i.e. when the bbox
    /// cannot bound anything at all. ±inf is *not* NaN: it is an ordered
    /// value, and the classic empty-envelope sentinel (`xmin = +inf`,
    /// `xmax = -inf`) bounds an empty set correctly under the plain AABB
    /// test. Deliberately not public: it is an implementation detail of
    /// [`Self::intersects`], and the public surface is gated by a baseline
    /// (`crates/core/api/`).
    fn has_nan(&self) -> bool {
        self.xmin.is_nan() || self.ymin.is_nan() || self.xmax.is_nan() || self.ymax.is_nan()
    }

    /// Check if this row group's bounds intersect with the given filter bounds.
    ///
    /// A bbox holding a NaN is treated as *no* bbox and always intersects
    /// (#428). Every comparison against a NaN is false, so without this guard
    /// a single NaN statistic — how a foreign writer spells nodata, and what
    /// pre-1.0 writers emitted for an all-NaN column — would prune the row
    /// group and silently drop every feature in it. Unusable statistics mean
    /// "read it", never "skip it"; [`extract_row_group_bounds_from_metadata`]
    /// already drops such a bbox on the way in, and this keeps the invariant
    /// for a `RowGroupBounds` built any other way.
    ///
    /// Infinities are left to the AABB test, which handles them exactly: an
    /// `xmin = +inf / xmax = -inf` envelope (the conventional "empty" bbox)
    /// intersects nothing and is pruned, which is what it asks for.
    ///
    /// **Antimeridian wraparound (#497):** per the Parquet Geospatial spec, a
    /// bounding box's X range may have `xmin > xmax`, meaning the row
    /// group's X coverage is *everything outside* the open interval
    /// `(xmax, xmin)` — i.e. two lobes, `x <= xmax` OR `x >= xmin` — rather
    /// than the usual single closed interval `[xmin, xmax]`. This shows up
    /// both in GeoParquet 1.1 covering-column stats and GeoParquet 2.0
    /// native geo statistics for a row group holding antimeridian-crossing
    /// geometry. The filter bbox itself is assumed non-wrapping (never
    /// produced by `--bbox` parsing). The non-wrapping path below is
    /// byte-for-byte the prior test, so ordinary row groups pay no extra
    /// cost.
    pub fn intersects(&self, filter: &TileBounds) -> bool {
        if self.has_nan() {
            return true;
        }
        let y_overlap = self.ymin <= filter.lat_max && self.ymax >= filter.lat_min;
        if !y_overlap {
            return false;
        }
        if self.xmin <= self.xmax {
            // Standard AABB intersection test (unchanged fast path).
            self.xmin <= filter.lng_max && self.xmax >= filter.lng_min
        } else {
            // Wraparound: the row group's X domain is x <= xmax OR x >= xmin.
            // The filter (a single interval) misses it only when the whole
            // filter interval falls inside the excluded gap (xmax, xmin).
            filter.lng_min <= self.xmax || filter.lng_max >= self.xmin
        }
    }

    /// Convert to TileBounds for compatibility with existing code.
    ///
    /// No NaN guard here on purpose: this is a plain field-for-field view of
    /// the same numbers, and the only way in (`extract_stat_value`) already
    /// refuses a NaN statistic, so a `RowGroupBounds` this crate builds never
    /// holds one. A caller that constructs the struct itself and
    /// hands a NaN corner to something that compares it gets the usual
    /// IEEE-754 answer (every comparison false); the guard that matters for
    /// pruning lives in [`Self::intersects`].
    pub fn to_tile_bounds(&self) -> TileBounds {
        TileBounds {
            lng_min: self.xmin,
            lat_min: self.ymin,
            lng_max: self.xmax,
            lat_max: self.ymax,
        }
    }
}

// ============================================================================
// GeoParquet Metadata Parsing
// ============================================================================

/// Internal structures for parsing GeoParquet JSON metadata.
#[derive(Debug, Deserialize)]
struct GeoMetadata {
    columns: HashMap<String, ColumnMetadata>,
    #[serde(default)]
    primary_column: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ColumnMetadata {
    #[serde(default)]
    covering: Option<CoveringMetadata>,
}

#[derive(Debug, Deserialize)]
struct CoveringMetadata {
    bbox: Option<BboxCovering>,
}

#[derive(Debug, Deserialize)]
struct BboxCovering {
    xmin: Vec<String>,
    ymin: Vec<String>,
    xmax: Vec<String>,
    ymax: Vec<String>,
}

/// Parse the covering specification from GeoParquet geo metadata JSON, for
/// ONE named geometry column.
///
/// # Arguments
///
/// * `geo_json` - The JSON string from the "geo" key-value metadata
/// * `geom_column` - the column whose covering is wanted: for pruning, the
///   column the rest of the pipeline will actually read
///   ([`resolve_geometry_column_name`]). `None` means the caller has no
///   column in hand, and then the spec-REQUIRED `primary_column` is the only
///   thing consulted.
///
/// There is deliberately **no** "first column that happens to declare a
/// covering" fallback (#519). A file with a `centroid` covering beside a
/// `geom_shape` the pipeline reads got pruned on the centroid's envelope —
/// silent data loss — and, with two covered columns, the fallback scanned a
/// `HashMap`'s values, so *which* wrong column won varied run to run. A
/// column with no covering of its own simply has no tier-1 bounds; the row
/// group is then read, which is always correct.
///
/// # Returns
///
/// `Ok(Some(CoveringSpec))` if that column declares a valid covering,
/// `Ok(None)` if it does not (or is absent from the metadata),
/// `Err` if the JSON is malformed.
pub fn parse_covering_metadata(
    geo_json: &str,
    geom_column: Option<&str>,
) -> Result<Option<CoveringSpec>, Error> {
    let metadata: GeoMetadata = serde_json::from_str(geo_json)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to parse geo metadata JSON: {}", e)))?;

    let name = geom_column.or(metadata.primary_column.as_deref());
    let Some(column) = name.and_then(|n| metadata.columns.get(n)) else {
        return Ok(None);
    };

    let Some(covering) = &column.covering else {
        return Ok(None);
    };

    let Some(bbox) = &covering.bbox else {
        return Ok(None);
    };

    Ok(Some(CoveringSpec::new(
        bbox.xmin.clone(),
        bbox.ymin.clone(),
        bbox.xmax.clone(),
        bbox.ymax.clone(),
    )))
}

// ============================================================================
// Row Group Bbox Extraction
// ============================================================================

/// Column indices for bbox fields within a Parquet file.
#[derive(Debug, Clone, Copy)]
pub struct BboxColumnIndices {
    pub xmin: usize,
    pub ymin: usize,
    pub xmax: usize,
    pub ymax: usize,
}

/// Find the column indices for bbox fields in the Parquet schema.
pub fn find_bbox_column_indices(
    metadata: &ParquetMetaData,
    covering: &CoveringSpec,
) -> Option<BboxColumnIndices> {
    let schema = metadata.file_metadata().schema_descr();
    let num_columns = schema.num_columns();

    let mut xmin_idx = None;
    let mut ymin_idx = None;
    let mut xmax_idx = None;
    let mut ymax_idx = None;

    let xmin_col = covering.xmin_column().to_lowercase();
    let ymin_col = covering.ymin_column().to_lowercase();
    let xmax_col = covering.xmax_column().to_lowercase();
    let ymax_col = covering.ymax_column().to_lowercase();

    for col_idx in 0..num_columns {
        let col_path = schema.column(col_idx).path().string().to_lowercase();

        if col_path == xmin_col || col_path.ends_with(&format!(".{}", xmin_col)) {
            xmin_idx = Some(col_idx);
        }
        if col_path == ymin_col || col_path.ends_with(&format!(".{}", ymin_col)) {
            ymin_idx = Some(col_idx);
        }
        if col_path == xmax_col || col_path.ends_with(&format!(".{}", xmax_col)) {
            xmax_idx = Some(col_idx);
        }
        if col_path == ymax_col || col_path.ends_with(&format!(".{}", ymax_col)) {
            ymax_idx = Some(col_idx);
        }
    }

    match (xmin_idx, ymin_idx, xmax_idx, ymax_idx) {
        (Some(xmin), Some(ymin), Some(xmax), Some(ymax)) => Some(BboxColumnIndices {
            xmin,
            ymin,
            xmax,
            ymax,
        }),
        _ => None,
    }
}

/// Extract a statistic value from column metadata.
///
/// Handles both f32 and f64 physical types, converting to f64. A **NaN**
/// statistic reads as *missing* (`None`, #428): it cannot bound anything, and
/// a caller that treated one as a bound would prune row groups it must read
/// (every comparison against a NaN is false). arrow-rs leaves NaN out of the
/// statistics it computes, but a file written elsewhere can carry one, so the
/// guard belongs on the read side.
///
/// ±inf is kept, matching the `--filter` path's
/// [`num_bounds`](crate::overview::filter): an infinity is an ordered value
/// and bounds perfectly well. In particular the conventional empty-envelope
/// sentinel (`xmin = +inf`, `xmax = -inf`) stays a real, empty bbox and goes
/// on being pruned.
fn extract_stat_value(
    row_group: &parquet::file::metadata::RowGroupMetaData,
    col_idx: usize,
    use_min: bool,
) -> Option<f64> {
    let col_meta = row_group.column(col_idx);
    let stats = col_meta.statistics()?;

    let bytes = if use_min {
        stats.min_bytes_opt()?
    } else {
        stats.max_bytes_opt()?
    };

    let value = match bytes.len() {
        4 => {
            // FLOAT (f32)
            let arr: [u8; 4] = bytes.try_into().ok()?;
            f32::from_le_bytes(arr) as f64
        }
        8 => {
            // DOUBLE (f64)
            let arr: [u8; 8] = bytes.try_into().ok()?;
            f64::from_le_bytes(arr)
        }
        _ => return None,
    };
    (!value.is_nan()).then_some(value)
}

/// Extract bounding boxes for all row groups from a Parquet file.
///
/// Returns `None` for row groups where statistics are unavailable.
///
/// # Arguments
///
/// * `path` - Path to the GeoParquet file
///
/// # Returns
///
/// A vector of `Option<RowGroupBounds>`, one per row group.
/// Returns `Err` if the file cannot be read or has no covering metadata.
pub fn extract_row_group_bounds(path: &Path) -> Result<Vec<Option<RowGroupBounds>>, Error> {
    let file = File::open(path)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to open {}: {}", path.display(), e)))?;

    let reader = SerializedFileReader::new(file)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to read {}: {}", path.display(), e)))?;

    extract_row_group_bounds_from_reader(&reader)
}

/// Extract bounding boxes from an already-opened Parquet reader.
///
/// This is useful when you already have a reader open and don't want to re-open the file.
pub fn extract_row_group_bounds_from_reader(
    reader: &SerializedFileReader<File>,
) -> Result<Vec<Option<RowGroupBounds>>, Error> {
    extract_row_group_bounds_from_metadata(reader.metadata())
}

/// Extract per-row-group bounding boxes directly from parsed Parquet metadata.
///
/// This is the metadata-only variant of [`extract_row_group_bounds_from_reader`]:
/// callers that already hold a [`ParquetMetaData`] (e.g. from a
/// `ParquetRecordBatchReaderBuilder`) can prune without re-opening the file.
/// Returns `None` for row groups whose covering statistics are unavailable —
/// including a bbox with a NaN corner, which cannot bound anything (#428) —
/// and `vec![None; n]` when the file lacks geo/covering metadata entirely.
pub fn extract_row_group_bounds_from_metadata(
    metadata: &ParquetMetaData,
) -> Result<Vec<Option<RowGroupBounds>>, Error> {
    let num_row_groups = metadata.num_row_groups();

    // Parse geo metadata to get covering spec
    let geo_json = get_geo_metadata(metadata)?;
    let Some(geo_json) = geo_json else {
        // No geo metadata - return all None
        return Ok(vec![None; num_row_groups]);
    };

    // Tier 1 must prune on the column the reader reads, not on whichever
    // column happens to declare a covering first (#519).
    let geom_column = resolve_geometry_column_name(metadata);
    let covering = parse_covering_metadata(&geo_json, geom_column.as_deref())?;
    let Some(covering) = covering else {
        // No covering metadata - return all None
        return Ok(vec![None; num_row_groups]);
    };

    // Find column indices
    let Some(indices) = find_bbox_column_indices(metadata, &covering) else {
        // Couldn't find bbox columns - return all None
        return Ok(vec![None; num_row_groups]);
    };

    // Extract bounds for each row group
    let mut bounds = Vec::with_capacity(num_row_groups);

    for rg_idx in 0..num_row_groups {
        let rg = metadata.row_group(rg_idx);

        // For row group bounds:
        // - xmin: MIN of xmin column (smallest x in this row group)
        // - ymin: MIN of ymin column (smallest y in this row group)
        // - xmax: MAX of xmax column (largest x in this row group)
        // - ymax: MAX of ymax column (largest y in this row group)
        let xmin = extract_stat_value(rg, indices.xmin, true);
        let ymin = extract_stat_value(rg, indices.ymin, true);
        let xmax = extract_stat_value(rg, indices.xmax, false);
        let ymax = extract_stat_value(rg, indices.ymax, false);

        match (xmin, ymin, xmax, ymax) {
            (Some(xmin), Some(ymin), Some(xmax), Some(ymax)) => {
                bounds.push(Some(RowGroupBounds {
                    row_group_idx: rg_idx,
                    xmin,
                    ymin,
                    xmax,
                    ymax,
                    num_rows: rg.num_rows() as usize,
                }));
            }
            _ => bounds.push(None),
        }
    }

    Ok(bounds)
}

/// Get the "geo" metadata JSON string from Parquet file metadata.
pub fn get_geo_metadata(metadata: &ParquetMetaData) -> Result<Option<String>, Error> {
    let kv = metadata.file_metadata().key_value_metadata();
    let Some(kv) = kv else {
        return Ok(None);
    };

    for pair in kv {
        if pair.key.to_lowercase() == "geo" {
            return Ok(pair.value.clone());
        }
    }

    Ok(None)
}

// ============================================================================
// GeoParquet 2.0 Native Geo Statistics (tier 2, #497)
// ============================================================================
//
// GeoParquet 1.1's covering columns (above) require the legacy "geo" JSON
// key-value metadata. A pure GeoParquet 2.0 file may carry no "geo" JSON at
// all — its geometry column is a native Parquet `Geometry`/`Geography`
// logical type instead, and *that* schema annotation is where its CRS and
// per-row-group bounding box live (`ColumnChunkMetaData::geo_statistics`,
// parsed unconditionally by the vendored `parquet` crate regardless of any
// cargo feature). [`extract_row_group_bounds_tiered`] adds this as a second
// tier, tried per row group only where covering-column stats (tier 1) are
// unavailable; [`crate::overview::convert::select_input_row_groups`] is the
// production entry point that calls it.

/// The geometry column the rest of the conversion will actually read,
/// resolved ONCE from the footer so that pruning can never operate on a
/// different column than the reader (#518).
///
/// Priority, matching [`crate::overview::convert::find_geometry_column`]
/// exactly (which consults the same "geo" metadata through the Arrow
/// schema):
///
/// 1. the "geo" JSON `primary_column`, when it names a real root column —
///    the GeoParquet spec makes this field REQUIRED, so it is the
///    authoritative answer whenever a "geo" key exists at all (and tier 1
///    already prunes on exactly this column's covering);
/// 2. otherwise a root column literally named "geometry";
/// 3. otherwise the first root column whose name contains "geom".
///
/// `None` means "cannot tell" — which is also the case in which the
/// pipeline itself fails with `NoGeometryColumn`, so refusing to prune
/// costs nothing.
pub(crate) fn resolve_geometry_column_name(metadata: &ParquetMetaData) -> Option<String> {
    let root = metadata.file_metadata().schema_descr().root_schema();
    let names: Vec<&str> = root.get_fields().iter().map(|f| f.name()).collect();

    if let Ok(Some(geo_json)) = get_geo_metadata(metadata) {
        if let Some(primary) = primary_geometry_column(&geo_json) {
            if names.contains(&primary.as_str()) {
                return Some(primary);
            }
        }
    }
    geometry_column_by_name(&names).map(str::to_string)
}

/// The "geo" JSON `primary_column`, if the metadata parses and declares one.
fn primary_geometry_column(geo_json: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(geo_json)
        .ok()?
        .get("primary_column")?
        .as_str()
        .map(str::to_string)
}

/// The name-only half of [`crate::overview::convert::find_geometry_column`],
/// shared so the two can never drift: exact "geometry", else the first name
/// containing "geom".
pub(crate) fn geometry_column_by_name<'a>(names: &[&'a str]) -> Option<&'a str> {
    names
        .iter()
        .find(|n| **n == "geometry")
        .or_else(|| names.iter().find(|n| n.contains("geom")))
        .copied()
}

/// Parquet leaf-column index of `geom_column`'s GeoParquet 2.0 geometry
/// annotation (`LogicalType::Geometry` / `Geography`) — no "geo" JSON
/// required, which is exactly the shape of a pure GeoParquet 2.0 file.
///
/// The leaf MUST belong to `geom_column` (matched on the schema path's root
/// component): a file with several annotated columns — say a `centroid`
/// alongside the real `geom_shape` — otherwise prunes on whichever envelope
/// happens to come first in schema order while the pipeline reads a
/// different column, silently dropping every matching feature (#518). No
/// match means no tier-2 pruning, never a guess.
pub(crate) fn find_native_geometry_leaf_column(
    metadata: &ParquetMetaData,
    geom_column: &str,
) -> Option<(usize, LogicalType)> {
    let schema = metadata.file_metadata().schema_descr();

    (0..schema.num_columns()).find_map(|col_idx| {
        let col = schema.column(col_idx);
        let lt = col.logical_type_ref()?;
        if !matches!(lt, LogicalType::Geometry(_) | LogicalType::Geography(_)) {
            return None;
        }
        // `parts()[0]` is the root field: the leaf itself for a top-level
        // primitive column, the enclosing field for a nested one.
        if col.path().parts().first().map(String::as_str) != Some(geom_column) {
            return None;
        }
        Some((col_idx, lt.clone()))
    })
}

/// CRS classification for `LogicalType::Geometry(crs)` / `Geography(crs)`
/// annotations, mapped onto the two CRSs [`crate::overview::level::Crs`]
/// supports. `None` means "unresolvable" and
/// [`native_geo_crs_matches`] turns that into "never guess" — tier-2
/// pruning is skipped entirely.
///
/// Delegates to [`crate::quality::classify_crs_identifier`], the single
/// classifier shared with the legacy "geo" JSON path. It used to be a
/// verbatim copy of an older substring match that tested `contains("WGS 84")`
/// BEFORE `contains("3857")` — and EPSG:3857's PROJJSON (what arrow-rs
/// inlines here) is named "WGS 84 / Pseudo-Mercator", so a Web Mercator file
/// classified as lon/lat and its meter envelopes were filtered against a
/// degree bbox: every row group pruned, a successful EMPTY archive (#518).
fn crs_identifier_kind(id: &str) -> Option<Crs> {
    match crate::quality::classify_crs_identifier(id) {
        crate::quality::CrsKind::Wgs84 => Some(Crs::Epsg4326),
        crate::quality::CrsKind::WebMercator => Some(Crs::Epsg3857),
        crate::quality::CrsKind::Unknown => None,
    }
}

/// Whether a native geometry column's declared CRS is consistent with
/// `session_crs` — the CRS the rest of the conversion has already committed
/// to via [`crate::overview::convert::detect_crs_from_kv`]. Tier-2 pruning
/// is only safe when this holds; the caller must fall through to tier 3
/// (read everything) rather than guess (#497).
///
/// DIVERGENCE FROM TIPPECANOE (N/A — no tippecanoe equivalent; this is a
/// GeoParquet 2.0 concern): a `Geography` column is excluded
/// unconditionally. Its edges interpolate along the sphere (non-planar), so
/// a corner-only AABB test does not necessarily bound the true shape the
/// way it does for a planar `Geometry` column — a great-circle arc can bulge
/// outside the box its endpoints describe. Conservative: never prune it.
pub(crate) fn native_geo_crs_matches(logical_type: &LogicalType, session_crs: Crs) -> bool {
    let LogicalType::Geometry(g) = logical_type else {
        return false;
    };
    let resolved = match &g.crs {
        None => Some(Crs::Epsg4326), // unset ⇒ OGC:CRS84 per the Parquet spec
        Some(s) => crs_identifier_kind(s),
    };
    resolved == Some(session_crs)
}

/// One row group's tier-2 bounds from `ColumnChunkMetaData::geo_statistics`,
/// or `None` if unusable: no geo statistics at all, no bounding box within
/// them (the spec allows a writer to emit `geospatial_types` without a
/// `bbox` or vice versa), or a NaN corner (#428 precedent: unusable
/// statistics must read as missing, never as an impossible/empty box that
/// would prune the row group).
fn row_group_native_bounds(
    rg: &parquet::file::metadata::RowGroupMetaData,
    rg_idx: usize,
    geom_leaf_idx: usize,
) -> Option<RowGroupBounds> {
    let stats = rg.column(geom_leaf_idx).geo_statistics()?;
    let bbox = stats.bounding_box()?;
    let (xmin, xmax, ymin, ymax) = (
        bbox.get_xmin(),
        bbox.get_xmax(),
        bbox.get_ymin(),
        bbox.get_ymax(),
    );
    if xmin.is_nan() || xmax.is_nan() || ymin.is_nan() || ymax.is_nan() {
        return None;
    }
    Some(RowGroupBounds {
        row_group_idx: rg_idx,
        xmin,
        ymin,
        xmax,
        ymax,
        num_rows: rg.num_rows() as usize,
    })
}

/// Per-row-group bounds tier 2: GeoParquet 2.0 native geo statistics on the
/// geometry column at `geom_leaf_idx`. `None` per row group where that
/// chunk's statistics are absent or unusable.
pub(crate) fn geo_statistics_bounds(
    metadata: &ParquetMetaData,
    geom_leaf_idx: usize,
) -> Vec<Option<RowGroupBounds>> {
    (0..metadata.num_row_groups())
        .map(|rg_idx| row_group_native_bounds(metadata.row_group(rg_idx), rg_idx, geom_leaf_idx))
        .collect()
}

/// Tiered per-row-group bbox extraction (#497): (1) GeoParquet 1.1 covering
/// columns ([`extract_row_group_bounds_from_metadata`], unchanged) → (2)
/// GeoParquet 2.0 native geo statistics ([`geo_statistics_bounds`]) → (3)
/// `None` (the row group is read; the exact per-feature filter downstream
/// is the correctness backstop either way, so over-pruning is the only
/// danger and this order never increases it). Tiered per row group, not per
/// file: a file with partial covering-column coverage still gets tier 2 for
/// whichever row groups tier 1 missed.
///
/// Tier 2 fires only when (a) the annotated leaf belongs to `geom_column` —
/// the column the pipeline actually reads, resolved once by
/// [`resolve_geometry_column_name`] — and (b) its declared CRS is consistent
/// with `session_crs` ([`native_geo_crs_matches`]). A different column, an
/// unresolvable CRS or a mismatched one all skip tier 2 file-wide rather
/// than guess (#518). `geom_column: None` ("couldn't resolve") likewise
/// skips it.
///
/// Tier 1 applies the same rule from inside
/// [`extract_row_group_bounds_from_metadata`], which resolves the column
/// itself and asks [`parse_covering_metadata`] for THAT column's covering
/// (#519 — it used to accept any column's, `primary_column` or not).
pub(crate) fn extract_row_group_bounds_tiered(
    metadata: &ParquetMetaData,
    session_crs: Crs,
    geom_column: Option<&str>,
) -> Vec<Option<RowGroupBounds>> {
    let tier1 = extract_row_group_bounds_from_metadata(metadata)
        .unwrap_or_else(|_| vec![None; metadata.num_row_groups()]);
    if tier1.iter().all(Option::is_some) {
        return tier1; // fully covered — tier 2 would be wasted work
    }

    let tier2 = geom_column
        .and_then(|name| find_native_geometry_leaf_column(metadata, name))
        .filter(|(_, lt)| native_geo_crs_matches(lt, session_crs))
        .map(|(geom_idx, _)| geo_statistics_bounds(metadata, geom_idx));

    match tier2 {
        None => tier1,
        Some(tier2) => tier1.into_iter().zip(tier2).map(|(a, b)| a.or(b)).collect(),
    }
}

// ============================================================================
// Tile Coordinate Parsing (for --bounds flag)
// ============================================================================

/// Parse bounds from either tile coordinates (z/x/y) or bbox (xmin,ymin,xmax,ymax).
///
/// Tile coordinates are converted to WGS84 bounds using Web Mercator projection.
pub fn parse_bounds(input: &str) -> Result<TileBounds, Error> {
    // Try z/x/y format first
    if let Some(bounds) = parse_tile_coords(input) {
        return Ok(bounds);
    }

    // Fall back to xmin,ymin,xmax,ymax format
    parse_bbox(input)
}

/// Parse tile coordinates in z/x/y format and convert to WGS84 bounds.
fn parse_tile_coords(input: &str) -> Option<TileBounds> {
    let parts: Vec<&str> = input.split('/').collect();
    if parts.len() != 3 {
        return None;
    }

    let z: u8 = parts[0].parse().ok()?;
    let x: u32 = parts[1].parse().ok()?;
    let y: u32 = parts[2].parse().ok()?;

    Some(tile_to_bounds(z, x, y))
}

/// Convert tile coordinates to WGS84 bounds using Web Mercator projection.
pub fn tile_to_bounds(z: u8, x: u32, y: u32) -> TileBounds {
    use std::f64::consts::PI;

    let n = 2f64.powi(z as i32);

    let lng_min = x as f64 / n * 360.0 - 180.0;
    let lng_max = (x + 1) as f64 / n * 360.0 - 180.0;

    // Note: y=0 is at the top (north), so lat_max uses y, lat_min uses y+1
    let lat_max = (PI * (1.0 - 2.0 * y as f64 / n)).sinh().atan().to_degrees();
    let lat_min = (PI * (1.0 - 2.0 * (y + 1) as f64 / n))
        .sinh()
        .atan()
        .to_degrees();

    TileBounds {
        lng_min,
        lat_min,
        lng_max,
        lat_max,
    }
}

/// Iterator over tiles covering a geographic bounding box at a given zoom level.
///
/// Used for coalescing density estimation: count tiles to estimate features-per-tile.
///
/// # Arguments
///
/// * `bounds` - Geographic bounding box (lng/lat, WGS84)
/// * `zoom` - Target zoom level
///
/// # Returns
///
/// Iterator yielding `TileCoord` for each tile that intersects the bounding box.
pub fn covering_tiles(bounds: &TileBounds, zoom: u8) -> impl Iterator<Item = TileCoord> {
    // Convert corner coordinates to tile coordinates
    let min_tile = lng_lat_to_tile(bounds.lng_min, bounds.lat_max, zoom); // NW corner
    let max_tile = lng_lat_to_tile(bounds.lng_max, bounds.lat_min, zoom); // SE corner

    // Generate all tiles in the bounding rectangle
    (min_tile.x..=max_tile.x)
        .flat_map(move |x| (min_tile.y..=max_tile.y).map(move |y| TileCoord::new(x, y, zoom)))
}

/// Parse bbox in xmin,ymin,xmax,ymax format.
fn parse_bbox(input: &str) -> Result<TileBounds, Error> {
    let parts: Vec<&str> = input.split(',').collect();
    if parts.len() != 4 {
        return Err(Error::InvalidConfig(format!(
            "Invalid bounds format '{}'. Expected 'z/x/y' or 'xmin,ymin,xmax,ymax'",
            input
        )));
    }

    let xmin: f64 = parts[0].trim().parse().map_err(|_| {
        Error::InvalidConfig(format!("Invalid xmin value '{}' in bounds", parts[0]))
    })?;
    let ymin: f64 = parts[1].trim().parse().map_err(|_| {
        Error::InvalidConfig(format!("Invalid ymin value '{}' in bounds", parts[1]))
    })?;
    let xmax: f64 = parts[2].trim().parse().map_err(|_| {
        Error::InvalidConfig(format!("Invalid xmax value '{}' in bounds", parts[2]))
    })?;
    let ymax: f64 = parts[3].trim().parse().map_err(|_| {
        Error::InvalidConfig(format!("Invalid ymax value '{}' in bounds", parts[3]))
    })?;

    // Validate bounds
    if xmin >= xmax {
        return Err(Error::InvalidConfig(format!(
            "xmin ({}) must be less than xmax ({})",
            xmin, xmax
        )));
    }
    if ymin >= ymax {
        return Err(Error::InvalidConfig(format!(
            "ymin ({}) must be less than ymax ({})",
            ymin, ymax
        )));
    }

    Ok(TileBounds {
        lng_min: xmin,
        lat_min: ymin,
        lng_max: xmax,
        lat_max: ymax,
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::TileCoord;

    // -------------------------------------------------------------------------
    // Covering Metadata Parsing Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_parse_covering_metadata_standard_format() {
        let geo_json = r#"{
            "columns": {
                "geometry": {
                    "covering": {
                        "bbox": {
                            "xmin": ["geometry_bbox", "xmin"],
                            "ymin": ["geometry_bbox", "ymin"],
                            "xmax": ["geometry_bbox", "xmax"],
                            "ymax": ["geometry_bbox", "ymax"]
                        }
                    }
                }
            },
            "primary_column": "geometry",
            "version": "1.1.0"
        }"#;

        let result = parse_covering_metadata(geo_json, Some("geometry")).unwrap();
        assert!(result.is_some());

        let spec = result.unwrap();
        assert_eq!(spec.xmin_path, vec!["geometry_bbox", "xmin"]);
        assert_eq!(spec.ymin_path, vec!["geometry_bbox", "ymin"]);
        assert_eq!(spec.xmax_path, vec!["geometry_bbox", "xmax"]);
        assert_eq!(spec.ymax_path, vec!["geometry_bbox", "ymax"]);
    }

    #[test]
    fn test_parse_covering_metadata_no_covering() {
        let geo_json = r#"{
            "columns": {
                "geometry": {
                    "bbox": [-180, -90, 180, 90]
                }
            },
            "primary_column": "geometry"
        }"#;

        let result = parse_covering_metadata(geo_json, Some("geometry")).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_covering_metadata_invalid_json() {
        let result = parse_covering_metadata("not valid json", Some("geometry"));
        assert!(result.is_err());
    }

    /// #519: a covering belonging to ANOTHER column is not this column's
    /// covering. The old "first column with a covering" fallback handed the
    /// `centroid` covering to a pipeline reading `geom_shape`, and pruned
    /// against an envelope of points that are not the data.
    #[test]
    fn parse_covering_metadata_refuses_another_columns_covering() {
        let geo_json = r#"{
            "version": "1.1.0",
            "columns": {
                "centroid": {
                    "encoding": "WKB",
                    "covering": { "bbox": {
                        "xmin": ["centroid_bbox", "xmin"],
                        "ymin": ["centroid_bbox", "ymin"],
                        "xmax": ["centroid_bbox", "xmax"],
                        "ymax": ["centroid_bbox", "ymax"]
                    }}
                },
                "geom_shape": { "encoding": "WKB" }
            }
        }"#;
        assert_eq!(
            parse_covering_metadata(geo_json, Some("geom_shape")).unwrap(),
            None,
            "geom_shape declares no covering of its own"
        );
        assert!(parse_covering_metadata(geo_json, Some("centroid"))
            .unwrap()
            .is_some());
        // No primary_column and no column in hand ⇒ nothing to key off.
        assert_eq!(parse_covering_metadata(geo_json, None).unwrap(), None);
    }

    /// The same shape, but with TWO covered columns: the old `HashMap`
    /// `values()` scan picked a different one run to run (iteration order is
    /// randomized per process), so a bbox extract's output depended on the
    /// hash seed. The resolved-column lookup is a function of the input.
    #[test]
    fn parse_covering_metadata_is_deterministic_with_two_covered_columns() {
        let geo_json = r#"{
            "version": "1.1.0",
            "columns": {
                "centroid": { "covering": { "bbox": {
                    "xmin": ["c", "xmin"], "ymin": ["c", "ymin"],
                    "xmax": ["c", "xmax"], "ymax": ["c", "ymax"]
                }}},
                "geom_shape": { "covering": { "bbox": {
                    "xmin": ["g", "xmin"], "ymin": ["g", "ymin"],
                    "xmax": ["g", "xmax"], "ymax": ["g", "ymax"]
                }}}
            }
        }"#;
        for _ in 0..16 {
            let spec = parse_covering_metadata(geo_json, Some("geom_shape"))
                .unwrap()
                .expect("geom_shape declares a covering");
            assert_eq!(spec.xmin_path, vec!["g", "xmin"]);
        }
    }

    #[test]
    fn test_covering_spec_column_paths() {
        let spec = CoveringSpec::new(
            vec!["bbox".to_string(), "xmin".to_string()],
            vec!["bbox".to_string(), "ymin".to_string()],
            vec!["bbox".to_string(), "xmax".to_string()],
            vec!["bbox".to_string(), "ymax".to_string()],
        );

        assert_eq!(spec.xmin_column(), "bbox.xmin");
        assert_eq!(spec.ymin_column(), "bbox.ymin");
        assert_eq!(spec.xmax_column(), "bbox.xmax");
        assert_eq!(spec.ymax_column(), "bbox.ymax");
    }

    // -------------------------------------------------------------------------
    // Row Group Bounds Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_row_group_bounds_intersects() {
        let bounds = RowGroupBounds {
            row_group_idx: 0,
            xmin: -10.0,
            ymin: -10.0,
            xmax: 10.0,
            ymax: 10.0,
            num_rows: 1000,
        };

        // Overlapping filter
        let filter = TileBounds {
            lng_min: -5.0,
            lat_min: -5.0,
            lng_max: 5.0,
            lat_max: 5.0,
        };
        assert!(bounds.intersects(&filter));

        // Partial overlap
        let filter = TileBounds {
            lng_min: 5.0,
            lat_min: 5.0,
            lng_max: 15.0,
            lat_max: 15.0,
        };
        assert!(bounds.intersects(&filter));

        // No overlap (completely outside)
        let filter = TileBounds {
            lng_min: 20.0,
            lat_min: 20.0,
            lng_max: 30.0,
            lat_max: 30.0,
        };
        assert!(!bounds.intersects(&filter));

        // Edge touch (should intersect)
        let filter = TileBounds {
            lng_min: 10.0,
            lat_min: 10.0,
            lng_max: 20.0,
            lat_max: 20.0,
        };
        assert!(bounds.intersects(&filter));
    }

    #[test]
    fn test_row_group_bounds_to_tile_bounds() {
        let rg_bounds = RowGroupBounds {
            row_group_idx: 5,
            xmin: -122.5,
            ymin: 37.7,
            xmax: -122.3,
            ymax: 37.9,
            num_rows: 500,
        };

        let tile_bounds = rg_bounds.to_tile_bounds();
        assert_eq!(tile_bounds.lng_min, -122.5);
        assert_eq!(tile_bounds.lat_min, 37.7);
        assert_eq!(tile_bounds.lng_max, -122.3);
        assert_eq!(tile_bounds.lat_max, 37.9);
    }

    // -------------------------------------------------------------------------
    // Tile Coordinate Parsing Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_tile_to_bounds_z0() {
        let bounds = tile_to_bounds(0, 0, 0);
        assert!((bounds.lng_min - (-180.0)).abs() < 0.001);
        assert!((bounds.lng_max - 180.0).abs() < 0.001);
        // Web Mercator bounds at z0 are approximately [-85.05, 85.05]
        assert!(bounds.lat_min > -86.0 && bounds.lat_min < -85.0);
        assert!(bounds.lat_max > 85.0 && bounds.lat_max < 86.0);
    }

    #[test]
    fn test_tile_to_bounds_sf() {
        // Tile containing San Francisco at z10
        let bounds = tile_to_bounds(10, 163, 395);

        // SF is roughly at -122.4, 37.8
        assert!(bounds.lng_min < -122.0 && bounds.lng_min > -123.0);
        assert!(bounds.lng_max < -122.0 && bounds.lng_max > -123.0);
        assert!(bounds.lat_min > 37.0 && bounds.lat_min < 38.0);
        assert!(bounds.lat_max > 37.0 && bounds.lat_max < 38.0);
    }

    #[test]
    fn test_parse_bounds_tile_coords() {
        let bounds = parse_bounds("10/163/395").unwrap();
        assert!(bounds.lng_min < -122.0);
        assert!(bounds.lat_min > 37.0);
    }

    #[test]
    fn test_parse_bounds_bbox() {
        let bounds = parse_bounds("-122.5,37.7,-122.3,37.9").unwrap();
        assert_eq!(bounds.lng_min, -122.5);
        assert_eq!(bounds.lat_min, 37.7);
        assert_eq!(bounds.lng_max, -122.3);
        assert_eq!(bounds.lat_max, 37.9);
    }

    #[test]
    fn test_parse_bounds_bbox_with_spaces() {
        let bounds = parse_bounds("-122.5, 37.7, -122.3, 37.9").unwrap();
        assert_eq!(bounds.lng_min, -122.5);
        assert_eq!(bounds.lat_min, 37.7);
    }

    #[test]
    fn test_parse_bounds_invalid_tile_coords() {
        // Invalid format falls through to bbox parsing, which also fails
        let result = parse_bounds("10/abc/395");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_bounds_invalid_bbox() {
        // Not enough values
        let result = parse_bounds("-122.5,37.7,-122.3");
        assert!(result.is_err());

        // xmin >= xmax
        let result = parse_bounds("10,37.7,-122.3,37.9");
        assert!(result.is_err());

        // ymin >= ymax
        let result = parse_bounds("-122.5,40.0,-122.3,37.9");
        assert!(result.is_err());
    }

    // -------------------------------------------------------------------------
    // Integration Tests (require test fixtures)
    // -------------------------------------------------------------------------

    // These tests require actual GeoParquet files with covering metadata.
    // They are marked as #[ignore] and can be run with:
    // cargo test --package tylertoo-core covering -- --ignored

    #[test]
    #[ignore]
    fn test_extract_row_group_bounds_from_file() {
        // This test requires ~/Downloads/adm4_polygons.parquet
        let path = std::path::PathBuf::from(
            std::env::var("HOME").unwrap() + "/Downloads/adm4_polygons.parquet",
        );

        if !path.exists() {
            eprintln!("Skipping test: {} not found", path.display());
            return;
        }

        let bounds = extract_row_group_bounds(&path).unwrap();

        // adm4_polygons.parquet has 364 row groups
        assert!(!bounds.is_empty());

        // At least some row groups should have bounds
        let with_bounds = bounds.iter().filter(|b| b.is_some()).count();
        assert!(with_bounds > 0, "Expected some row groups to have bounds");

        // Check that bounds are valid WGS84 coordinates
        for bound in bounds.iter().flatten() {
            assert!(
                bound.xmin >= -180.0 && bound.xmin <= 180.0,
                "Invalid xmin: {}",
                bound.xmin
            );
            assert!(
                bound.xmax >= -180.0 && bound.xmax <= 180.0,
                "Invalid xmax: {}",
                bound.xmax
            );
            assert!(
                bound.ymin >= -90.0 && bound.ymin <= 90.0,
                "Invalid ymin: {}",
                bound.ymin
            );
            assert!(
                bound.ymax >= -90.0 && bound.ymax <= 90.0,
                "Invalid ymax: {}",
                bound.ymax
            );
            assert!(bound.xmin <= bound.xmax, "xmin > xmax");
            assert!(bound.ymin <= bound.ymax, "ymin > ymax");
        }

        println!(
            "Extracted bounds for {}/{} row groups",
            with_bounds,
            bounds.len()
        );
    }

    // -------------------------------------------------------------------------
    // Tile Coverage Tests (for coalescing density estimation)
    // -------------------------------------------------------------------------

    #[test]
    fn test_covering_tiles_single_tile() {
        // Bounds that fit entirely within a single z10 tile
        // San Francisco downtown area
        let bounds = TileBounds::new(-122.42, 37.78, -122.40, 37.80);
        let tiles: Vec<_> = covering_tiles(&bounds, 10).collect();

        assert_eq!(tiles.len(), 1, "Expected single tile coverage");
        assert_eq!(tiles[0].z, 10);
    }

    #[test]
    fn test_covering_tiles_multiple_tiles() {
        // Bounds spanning multiple tiles at z10
        // Larger SF Bay area
        let bounds = TileBounds::new(-122.5, 37.7, -122.3, 37.9);
        let tiles: Vec<_> = covering_tiles(&bounds, 10).collect();

        assert!(tiles.len() > 1, "Expected multiple tiles");
        // All tiles should be at z10
        assert!(tiles.iter().all(|t| t.z == 10));
    }

    #[test]
    fn test_covering_tiles_world_at_z0() {
        // World bounds at z0 = exactly one tile
        let bounds = TileBounds::new(-180.0, -85.0, 180.0, 85.0);
        let tiles: Vec<_> = covering_tiles(&bounds, 0).collect();

        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0], TileCoord::new(0, 0, 0));
    }

    #[test]
    fn test_covering_tiles_world_at_z1() {
        // World bounds at z1 = four tiles (2x2)
        let bounds = TileBounds::new(-180.0, -85.0, 180.0, 85.0);
        let tiles: Vec<_> = covering_tiles(&bounds, 1).collect();

        assert_eq!(tiles.len(), 4);
    }

    #[test]
    fn test_covering_tiles_count_matches_expected() {
        // A bbox covering roughly 2x3 tiles at z12
        let bounds = TileBounds::new(-122.5, 37.7, -122.35, 37.85);
        let tiles: Vec<_> = covering_tiles(&bounds, 12).collect();

        // Calculate expected: tiles spanning this bbox
        // We don't need exact count, just verify reasonable range
        assert!(
            tiles.len() >= 4 && tiles.len() <= 12,
            "Expected 4-12 tiles, got {}",
            tiles.len()
        );
    }

    // -------------------------------------------------------------------------
    // NaN statistics (#428)
    // -------------------------------------------------------------------------

    /// Synthetic footer metadata for a file whose covering bbox lives in four
    /// flat DOUBLE columns, with the given `(min, max)` statistics per column
    /// in `xmin, ymin, xmax, ymax` order.
    ///
    /// Hand-built rather than written through `ArrowWriter`: arrow-rs skips
    /// NaN when it computes float statistics, so a NaN min/max can only reach
    /// us from *another* writer — which is exactly the case under test.
    fn metadata_with_bbox_stats(stats: [(f64, f64); 4], num_rows: i64) -> ParquetMetaData {
        use parquet::basic::Type as PhysicalType;
        use parquet::file::metadata::{
            ColumnChunkMetaData, FileMetaData, KeyValue, RowGroupMetaData,
        };
        use parquet::file::statistics::Statistics;
        use parquet::schema::types::{SchemaDescriptor, Type};
        use std::sync::Arc;

        let names = ["xmin", "ymin", "xmax", "ymax"];
        let fields = names
            .iter()
            .map(|n| {
                Arc::new(
                    Type::primitive_type_builder(n, PhysicalType::DOUBLE)
                        .build()
                        .unwrap(),
                )
            })
            .collect();
        let schema = Type::group_type_builder("schema")
            .with_fields(fields)
            .build()
            .unwrap();
        let descr = Arc::new(SchemaDescriptor::new(Arc::new(schema)));

        let columns = (0..4)
            .map(|i| {
                let (min, max) = stats[i];
                ColumnChunkMetaData::builder(descr.column(i))
                    .set_statistics(Statistics::double(
                        Some(min),
                        Some(max),
                        None,
                        Some(0),
                        false,
                    ))
                    .build()
                    .unwrap()
            })
            .collect();
        let rg = RowGroupMetaData::builder(descr.clone())
            .set_num_rows(num_rows)
            .set_column_metadata(columns)
            .build()
            .unwrap();

        let geo = r#"{"version":"1.1.0","primary_column":"geometry","columns":{"geometry":{"covering":{"bbox":{"xmin":["xmin"],"ymin":["ymin"],"xmax":["xmax"],"ymax":["ymax"]}}}}}"#;
        let file_meta = FileMetaData::new(
            2,
            num_rows,
            None,
            Some(vec![KeyValue::new("geo".to_string(), geo.to_string())]),
            descr,
            None,
        );
        ParquetMetaData::new(file_meta, vec![rg])
    }

    /// A NaN in the stats bbox makes every AABB comparison false, which would
    /// PRUNE the row group — silent data loss. Unusable statistics mean
    /// "read it".
    #[test]
    fn nan_bounds_never_prune() {
        let filter = TileBounds {
            lng_min: -5.0,
            lat_min: -5.0,
            lng_max: 5.0,
            lat_max: 5.0,
        };
        for (label, bounds) in [
            (
                "NaN xmin",
                RowGroupBounds {
                    row_group_idx: 0,
                    xmin: f64::NAN,
                    ymin: -10.0,
                    xmax: 10.0,
                    ymax: 10.0,
                    num_rows: 100,
                },
            ),
            (
                "NaN ymax",
                RowGroupBounds {
                    row_group_idx: 0,
                    xmin: -10.0,
                    ymin: -10.0,
                    xmax: 10.0,
                    ymax: f64::NAN,
                    num_rows: 100,
                },
            ),
            (
                "all NaN",
                RowGroupBounds {
                    row_group_idx: 0,
                    xmin: f64::NAN,
                    ymin: f64::NAN,
                    xmax: f64::NAN,
                    ymax: f64::NAN,
                    num_rows: 100,
                },
            ),
        ] {
            assert!(
                bounds.intersects(&filter),
                "{label}: a NaN statistic must keep the row group, not prune it"
            );
        }
    }

    /// ±inf is NOT NaN: it is an ordered value that bounds perfectly well, so
    /// the guard must not swallow it. In particular the conventional empty
    /// envelope (`xmin = +inf`, `xmax = -inf`) says "this row group holds
    /// nothing" and must keep being pruned — treating it as unusable would
    /// un-prune a row group that older code correctly skipped.
    #[test]
    fn infinite_bounds_still_prune() {
        let filter = TileBounds {
            lng_min: -5.0,
            lat_min: -5.0,
            lng_max: 5.0,
            lat_max: 5.0,
        };

        let empty_sentinel = RowGroupBounds {
            row_group_idx: 0,
            xmin: f64::INFINITY,
            ymin: f64::INFINITY,
            xmax: f64::NEG_INFINITY,
            ymax: f64::NEG_INFINITY,
            num_rows: 100,
        };
        assert!(
            !empty_sentinel.intersects(&filter),
            "the empty-envelope sentinel bounds an empty set and must prune"
        );

        let whole_world = RowGroupBounds {
            row_group_idx: 0,
            xmin: f64::NEG_INFINITY,
            ymin: f64::NEG_INFINITY,
            xmax: f64::INFINITY,
            ymax: f64::INFINITY,
            num_rows: 100,
        };
        assert!(
            whole_world.intersects(&filter),
            "an unbounded bbox contains everything and must be read"
        );

        // An infinity on the far side of the filter still prunes normally.
        let east_of_everything = RowGroupBounds {
            row_group_idx: 0,
            xmin: 10.0,
            ymin: -10.0,
            xmax: f64::INFINITY,
            ymax: 10.0,
            num_rows: 100,
        };
        assert!(!east_of_everything.intersects(&filter));
    }

    /// The same thing one layer down: a NaN statistic is read as *missing*
    /// statistics, so every caller's "no stats -> read it" branch does the
    /// right thing without knowing about NaN — while ±inf reads as the real
    /// bound it is.
    #[test]
    fn nan_statistics_read_as_missing() {
        let finite = metadata_with_bbox_stats(
            [(-10.0, -1.0), (-10.0, -1.0), (1.0, 10.0), (1.0, 10.0)],
            100,
        );
        let bounds = extract_row_group_bounds_from_metadata(&finite).unwrap();
        assert_eq!(
            bounds[0].as_ref().map(|b| (b.xmin, b.ymax)),
            Some((-10.0, 10.0)),
            "control: finite statistics are still read"
        );

        let nan = metadata_with_bbox_stats(
            [
                (f64::NAN, f64::NAN),
                (-10.0, -1.0),
                (1.0, 10.0),
                (1.0, 10.0),
            ],
            100,
        );
        let bounds = extract_row_group_bounds_from_metadata(&nan).unwrap();
        assert_eq!(
            bounds,
            vec![None],
            "a NaN covering statistic must read as missing statistics"
        );

        // ±inf survives the read: it is a bound, not a missing statistic.
        let sentinel = metadata_with_bbox_stats(
            [
                (f64::INFINITY, f64::INFINITY),
                (f64::INFINITY, f64::INFINITY),
                (f64::NEG_INFINITY, f64::NEG_INFINITY),
                (f64::NEG_INFINITY, f64::NEG_INFINITY),
            ],
            100,
        );
        let bounds = extract_row_group_bounds_from_metadata(&sentinel).unwrap();
        assert_eq!(
            bounds[0].as_ref().map(|b| (b.xmin, b.xmax)),
            Some((f64::INFINITY, f64::NEG_INFINITY)),
            "an infinite statistic is a real bound and must be read as one"
        );
    }

    /// End of the line: the row-group selector that the bbox extract path
    /// actually calls must KEEP a NaN-statistic row group — and must still
    /// DROP an empty-envelope one.
    #[test]
    fn nan_statistics_keep_the_row_group_selected() {
        let nan = metadata_with_bbox_stats(
            [
                (f64::NAN, f64::NAN),
                (-10.0, -1.0),
                (1.0, 10.0),
                (1.0, 10.0),
            ],
            100,
        );
        // A bbox on the far side of the world from any plausible reading of
        // those statistics: only the NaN guard can keep this row group.
        let selected =
            crate::overview::convert::select_input_row_groups(&nan, &[100.0, 60.0, 110.0, 70.0]);
        assert_eq!(
            selected,
            vec![0],
            "a row group whose covering statistics are unusable must be read, not pruned"
        );

        let sentinel = metadata_with_bbox_stats(
            [
                (f64::INFINITY, f64::INFINITY),
                (f64::INFINITY, f64::INFINITY),
                (f64::NEG_INFINITY, f64::NEG_INFINITY),
                (f64::NEG_INFINITY, f64::NEG_INFINITY),
            ],
            100,
        );
        let selected = crate::overview::convert::select_input_row_groups(
            &sentinel,
            &[100.0, 60.0, 110.0, 70.0],
        );
        assert!(
            selected.is_empty(),
            "an empty-envelope row group bounds nothing and must stay pruned (got {selected:?})"
        );
    }

    // -------------------------------------------------------------------------
    // GeoParquet 2.0 native geo statistics (tier 2, #497)
    // -------------------------------------------------------------------------

    /// One row group's synthetic tier-1/tier-2 stats for
    /// [`metadata_with_tiered_stats`]. `covering`/`native` are independently
    /// present or absent so a single helper builds every tier-ordering
    /// scenario.
    #[derive(Clone, Copy)]
    struct TieredRowGroupSpec {
        /// Tier-1 covering-column bbox, `(xmin, ymin, xmax, ymax)`.
        covering: Option<(f64, f64, f64, f64)>,
        /// Tier-2 native `geo_statistics` bbox, `(xmin, xmax, ymin, ymax)`
        /// — [`parquet::geospatial::bounding_box::BoundingBox::new`]'s own
        /// (unusual) parameter order.
        native: Option<(f64, f64, f64, f64)>,
        num_rows: i64,
    }

    /// Build synthetic footer metadata with schema
    /// `[xmin, ymin, xmax, ymax, geometry]`: the first four DOUBLE columns
    /// are the GeoParquet 1.1 tier-1 covering columns; `geometry` is a
    /// BYTE_ARRAY column annotated `LogicalType::Geometry(crs)` for tier 2.
    /// `with_geo_json` attaches (or omits) the "geo" key-value metadata
    /// that points a covering spec at the four DOUBLE columns — `false`
    /// models a pure GeoParquet 2.0 file (no legacy "geo" JSON at all),
    /// making tier 1 unavailable file-wide regardless of what any
    /// individual row group's DOUBLE-column stats say.
    ///
    /// Hand-built rather than written through `ArrowWriter`: this build has
    /// the vendored `parquet` crate's `geospatial` cargo feature off (the
    /// write-side `GeospatialStatistics` accumulator is feature-gated,
    /// unlike the read side used in production — see `extension.rs`'s
    /// `logical_type_for_binary`), so `ArrowWriter` cannot emit
    /// `LogicalType::Geometry` at all in this workspace. This mirrors
    /// [`metadata_with_bbox_stats`]'s existing precedent for the same
    /// reason (NaN statistics there).
    fn metadata_with_tiered_stats(
        crs: Option<&str>,
        with_geo_json: bool,
        row_groups: &[TieredRowGroupSpec],
    ) -> ParquetMetaData {
        use parquet::basic::Type as PhysicalType;
        use parquet::file::metadata::{
            ColumnChunkMetaData, FileMetaData, KeyValue, RowGroupMetaData,
        };
        use parquet::file::statistics::Statistics;
        use parquet::geospatial::bounding_box::BoundingBox;
        use parquet::geospatial::statistics::GeospatialStatistics;
        use parquet::schema::types::{SchemaDescriptor, Type};
        use std::sync::Arc;

        let mut fields: Vec<Arc<Type>> = ["xmin", "ymin", "xmax", "ymax"]
            .iter()
            .map(|n| {
                Arc::new(
                    Type::primitive_type_builder(n, PhysicalType::DOUBLE)
                        .build()
                        .unwrap(),
                )
            })
            .collect();
        fields.push(Arc::new(
            Type::primitive_type_builder("geometry", PhysicalType::BYTE_ARRAY)
                .with_logical_type(Some(LogicalType::geometry(crs.map(str::to_string))))
                .build()
                .unwrap(),
        ));
        let schema = Type::group_type_builder("schema")
            .with_fields(fields)
            .build()
            .unwrap();
        let descr = Arc::new(SchemaDescriptor::new(Arc::new(schema)));

        let row_group_metas: Vec<RowGroupMetaData> = row_groups
            .iter()
            .map(|spec| {
                let covering_vals: [Option<f64>; 4] = match spec.covering {
                    Some((xmin, ymin, xmax, ymax)) => {
                        [Some(xmin), Some(ymin), Some(xmax), Some(ymax)]
                    }
                    None => [None, None, None, None],
                };
                let mut columns: Vec<ColumnChunkMetaData> = covering_vals
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let mut builder = ColumnChunkMetaData::builder(descr.column(i));
                        if let Some(v) = v {
                            builder = builder.set_statistics(Statistics::double(
                                Some(v),
                                Some(v),
                                None,
                                Some(0),
                                false,
                            ));
                        }
                        builder.build().unwrap()
                    })
                    .collect();

                let mut geom_builder = ColumnChunkMetaData::builder(descr.column(4));
                if let Some((xmin, xmax, ymin, ymax)) = spec.native {
                    geom_builder =
                        geom_builder.set_geo_statistics(Box::new(GeospatialStatistics::new(
                            Some(BoundingBox::new(xmin, xmax, ymin, ymax)),
                            None,
                        )));
                }
                columns.push(geom_builder.build().unwrap());

                RowGroupMetaData::builder(descr.clone())
                    .set_num_rows(spec.num_rows)
                    .set_column_metadata(columns)
                    .build()
                    .unwrap()
            })
            .collect();

        let total_rows: i64 = row_groups.iter().map(|s| s.num_rows).sum();
        let geo_json = r#"{"version":"1.1.0","primary_column":"geometry","columns":{"geometry":{"covering":{"bbox":{"xmin":["xmin"],"ymin":["ymin"],"xmax":["xmax"],"ymax":["ymax"]}}}}}"#;
        let kv =
            with_geo_json.then(|| vec![KeyValue::new("geo".to_string(), geo_json.to_string())]);
        let file_meta = FileMetaData::new(2, total_rows, None, kv, descr, None);
        ParquetMetaData::new(file_meta, row_group_metas)
    }

    /// Tier 2 fires when tier 1 has nothing at all — the pure GeoParquet 2.0
    /// shape: no "geo" JSON, geometry column carries `LogicalType::Geometry`
    /// with native `geo_statistics` instead.
    #[test]
    fn native_geo_stats_prune_without_covering_column() {
        let specs = [
            TieredRowGroupSpec {
                covering: None,
                native: Some((-100.0, -90.0, -10.0, -1.0)), // (xmin, xmax, ymin, ymax)
                num_rows: 10,
            },
            TieredRowGroupSpec {
                covering: None,
                native: Some((0.0, 10.0, 0.0, 10.0)),
                num_rows: 10,
            },
            TieredRowGroupSpec {
                covering: None,
                native: Some((50.0, 60.0, 50.0, 60.0)),
                num_rows: 10,
            },
        ];
        let metadata = metadata_with_tiered_stats(None, false, &specs);

        let bounds = extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg4326,
            resolve_geometry_column_name(&metadata).as_deref(),
        );
        assert_eq!(
            bounds,
            vec![
                Some(RowGroupBounds {
                    row_group_idx: 0,
                    xmin: -100.0,
                    ymin: -10.0,
                    xmax: -90.0,
                    ymax: -1.0,
                    num_rows: 10,
                }),
                Some(RowGroupBounds {
                    row_group_idx: 1,
                    xmin: 0.0,
                    ymin: 0.0,
                    xmax: 10.0,
                    ymax: 10.0,
                    num_rows: 10,
                }),
                Some(RowGroupBounds {
                    row_group_idx: 2,
                    xmin: 50.0,
                    ymin: 50.0,
                    xmax: 60.0,
                    ymax: 60.0,
                    num_rows: 10,
                }),
            ],
            "native geo_statistics must be read when no covering column exists"
        );

        // End-to-end through the production entry point: a bbox around row
        // group 1 only must prune 0 and 2.
        let selected =
            crate::overview::convert::select_input_row_groups(&metadata, &[-1.0, -1.0, 11.0, 11.0]);
        assert_eq!(selected, vec![1], "native stats did not prune 0 and 2");
    }

    /// Tier order: when BOTH tiers are present for a row group, tier 1
    /// (covering columns) wins — even when tier 2's numbers disagree.
    #[test]
    fn covering_column_wins_over_native_stats() {
        let specs = [TieredRowGroupSpec {
            covering: Some((0.0, 0.0, 10.0, 10.0)), // the "true" bbox
            // Deliberately wrong/wider — must be ignored in favor of tier 1.
            native: Some((-1000.0, 1000.0, -1000.0, 1000.0)),
            num_rows: 5,
        }];
        let metadata = metadata_with_tiered_stats(None, true, &specs);

        let bounds = extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg4326,
            resolve_geometry_column_name(&metadata).as_deref(),
        );
        assert_eq!(
            bounds,
            vec![Some(RowGroupBounds {
                row_group_idx: 0,
                xmin: 0.0,
                ymin: 0.0,
                xmax: 10.0,
                ymax: 10.0,
                num_rows: 5,
            })],
            "covering-column stats (tier 1) must win over native stats (tier 2)"
        );

        // A bbox that only the (wrong) native stats would keep must still
        // be pruned, proving tier 1's numbers — not tier 2's — decided it.
        let selected = crate::overview::convert::select_input_row_groups(
            &metadata,
            &[500.0, 500.0, 600.0, 600.0],
        );
        assert!(
            selected.is_empty(),
            "tier 1 must have decided pruning, not the wider tier-2 bbox"
        );
    }

    /// Tiering is per row group, not per file: a row group tier 1 missed
    /// still gets tier 2.
    #[test]
    fn native_stats_fill_in_where_covering_column_is_missing() {
        let specs = [
            TieredRowGroupSpec {
                covering: Some((0.0, 0.0, 10.0, 10.0)),
                native: None,
                num_rows: 5,
            },
            TieredRowGroupSpec {
                // This row group's covering-column stats are unset (as if
                // that writer skipped them for just this chunk); only its
                // native stats are usable.
                covering: None,
                native: Some((100.0, 110.0, 100.0, 110.0)),
                num_rows: 5,
            },
        ];
        let metadata = metadata_with_tiered_stats(None, true, &specs);

        let bounds = extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg4326,
            resolve_geometry_column_name(&metadata).as_deref(),
        );
        assert_eq!(
            bounds[0].as_ref().map(|b| (b.xmin, b.ymax)),
            Some((0.0, 10.0)),
            "row group 0: tier 1"
        );
        assert_eq!(
            bounds[1].as_ref().map(|b| (b.xmin, b.ymax)),
            Some((100.0, 110.0)),
            "row group 1: tier 2 fills in where tier 1 was missing"
        );
    }

    /// The Parquet Geospatial spec allows `xmin > xmax` for a bbox that
    /// wraps the antimeridian: the true X coverage is two lobes (`x <=
    /// xmax` OR `x >= xmin`), not the empty interval a naive AABB test
    /// would read it as.
    #[test]
    fn native_stats_antimeridian_bbox_keeps_both_lobes() {
        // Row group's X range wraps: covers x <= -170 OR x >= 170.
        let wrapping = RowGroupBounds {
            row_group_idx: 0,
            xmin: 170.0,
            ymin: -10.0,
            xmax: -170.0,
            ymax: 10.0,
            num_rows: 100,
        };

        let east_lobe = TileBounds {
            lng_min: 175.0,
            lat_min: -5.0,
            lng_max: 179.0,
            lat_max: 5.0,
        };
        assert!(
            wrapping.intersects(&east_lobe),
            "east lobe (near +180) must intersect"
        );

        let west_lobe = TileBounds {
            lng_min: -179.0,
            lat_min: -5.0,
            lng_max: -178.0,
            lat_max: 5.0,
        };
        assert!(
            wrapping.intersects(&west_lobe),
            "west lobe (near -180) must intersect"
        );

        let the_gap = TileBounds {
            lng_min: -50.0,
            lat_min: -5.0,
            lng_max: 50.0,
            lat_max: 5.0,
        };
        assert!(
            !wrapping.intersects(&the_gap),
            "a query entirely in the excluded gap must NOT intersect"
        );

        // Same thing through the extraction path: geo_statistics preserves
        // the xmin > xmax inversion verbatim (no clamping/rejecting it).
        let specs = [TieredRowGroupSpec {
            covering: None,
            native: Some((170.0, -170.0, -10.0, 10.0)), // (xmin, xmax, ymin, ymax)
            num_rows: 100,
        }];
        let metadata = metadata_with_tiered_stats(None, false, &specs);
        let bounds = extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg4326,
            resolve_geometry_column_name(&metadata).as_deref(),
        );
        let b = bounds[0].as_ref().expect("native stats must be read");
        assert_eq!((b.xmin, b.xmax), (170.0, -170.0), "inversion preserved");

        let selected = crate::overview::convert::select_input_row_groups(
            &metadata,
            &[175.0, -5.0, 179.0, 5.0],
        );
        assert_eq!(selected, vec![0], "east lobe must select the row group");

        let selected =
            crate::overview::convert::select_input_row_groups(&metadata, &[-50.0, -5.0, 50.0, 5.0]);
        assert!(selected.is_empty(), "the gap must prune the row group");
    }

    /// Never guess: a native geometry column whose declared CRS doesn't
    /// match the session CRS must skip tier 2 entirely (keep, don't prune).
    #[test]
    fn native_stats_crs_mismatch_never_prunes() {
        let specs = [TieredRowGroupSpec {
            covering: None,
            // Values that look like plausible EPSG:3857 meters, not lon/lat.
            native: Some((-1.0e7, -1.0e7 + 10.0, -1.0e7, -1.0e7 + 10.0)),
            num_rows: 10,
        }];
        // Column declares EPSG:3857, but the session resolved to EPSG:4326
        // (e.g. because there is no "geo" JSON at all to say otherwise).
        let metadata = metadata_with_tiered_stats(Some("EPSG:3857"), false, &specs);

        let bounds = extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg4326,
            resolve_geometry_column_name(&metadata).as_deref(),
        );
        assert_eq!(
            bounds,
            vec![None],
            "CRS mismatch between the column and the session must skip tier 2"
        );

        // Consistent CRS: tier 2 fires normally.
        let metadata_ok = metadata_with_tiered_stats(Some("EPSG:3857"), false, &specs);
        let bounds_ok = extract_row_group_bounds_tiered(
            &metadata_ok,
            Crs::Epsg3857,
            resolve_geometry_column_name(&metadata_ok).as_deref(),
        );
        assert!(
            bounds_ok[0].is_some(),
            "a matching CRS must let tier 2 through"
        );
    }

    /// Never guess: an unresolvable CRS string (not one of the two CRSs
    /// this pipeline supports) must skip tier 2, whatever the session CRS.
    #[test]
    fn native_stats_unresolvable_crs_never_prunes() {
        let specs = [TieredRowGroupSpec {
            covering: None,
            native: Some((0.0, 10.0, 0.0, 10.0)),
            num_rows: 10,
        }];
        // A CRS this crate cannot classify (e.g. a state-plane code).
        let metadata = metadata_with_tiered_stats(Some("EPSG:2154"), false, &specs);
        for crs in [Crs::Epsg4326, Crs::Epsg3857] {
            let bounds = extract_row_group_bounds_tiered(
                &metadata,
                crs,
                resolve_geometry_column_name(&metadata).as_deref(),
            );
            assert_eq!(bounds, vec![None], "unresolvable CRS must skip tier 2");
        }
    }

    /// DIVERGENCE (documented on `native_geo_crs_matches`): `Geography`
    /// columns are never pruned via tier 2, even with a WGS84 CRS —
    /// spherical edge interpolation isn't bounded by a planar AABB corner
    /// test the way a `Geometry` column's straight edges are.
    #[test]
    fn native_stats_geography_never_prunes() {
        use parquet::basic::Type as PhysicalType;
        use parquet::file::metadata::{ColumnChunkMetaData, FileMetaData, RowGroupMetaData};
        use parquet::geospatial::bounding_box::BoundingBox;
        use parquet::geospatial::statistics::GeospatialStatistics;
        use parquet::schema::types::{SchemaDescriptor, Type};
        use std::sync::Arc;

        let geometry_field = Arc::new(
            Type::primitive_type_builder("geometry", PhysicalType::BYTE_ARRAY)
                .with_logical_type(Some(LogicalType::geography(None, None)))
                .build()
                .unwrap(),
        );
        let schema = Type::group_type_builder("schema")
            .with_fields(vec![geometry_field])
            .build()
            .unwrap();
        let descr = Arc::new(SchemaDescriptor::new(Arc::new(schema)));
        let geo_stats =
            GeospatialStatistics::new(Some(BoundingBox::new(0.0, 10.0, 0.0, 10.0)), None);
        let column = ColumnChunkMetaData::builder(descr.column(0))
            .set_geo_statistics(Box::new(geo_stats))
            .build()
            .unwrap();
        let rg = RowGroupMetaData::builder(descr.clone())
            .set_num_rows(10)
            .set_column_metadata(vec![column])
            .build()
            .unwrap();
        let file_meta = FileMetaData::new(2, 10, None, None, descr, None);
        let metadata = ParquetMetaData::new(file_meta, vec![rg]);

        let bounds = extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg4326,
            resolve_geometry_column_name(&metadata).as_deref(),
        );
        assert_eq!(bounds, vec![None], "Geography columns must never prune");
    }

    /// Extends the stats-free graceful-degradation contract to a pure
    /// GeoParquet 2.0 file: `LogicalType::Geometry` present, no "geo" JSON,
    /// AND no `geo_statistics` on any row group either (a writer that
    /// annotated the schema but skipped stats). Every row group must still
    /// be read — the same conservative behavior as the GP1.1
    /// stats-free case covered by `bbox_filter_stats_free_degradation`
    /// (`overview::convert` tests), exercised here at the unit level
    /// because writing a real round-tripped GeoParquet 2.0 fixture needs
    /// the vendored `parquet` crate's `geospatial` cargo feature, which
    /// this workspace does not enable (see `metadata_with_tiered_stats`).
    #[test]
    fn select_input_row_groups_degrades_gracefully_for_stats_free_gp2() {
        let specs = [
            TieredRowGroupSpec {
                covering: None,
                native: None,
                num_rows: 10,
            },
            TieredRowGroupSpec {
                covering: None,
                native: None,
                num_rows: 10,
            },
        ];
        let metadata = metadata_with_tiered_stats(None, false, &specs);

        let bounds = extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg4326,
            resolve_geometry_column_name(&metadata).as_deref(),
        );
        assert_eq!(bounds, vec![None, None]);

        // A tiny, far-away bbox would prune everything if stats existed;
        // with none at all, both row groups must still be read.
        let selected = crate::overview::convert::select_input_row_groups(
            &metadata,
            &[500.0, 500.0, 501.0, 501.0],
        );
        assert_eq!(
            selected,
            vec![0, 1],
            "stats-free GeoParquet 2.0 must degrade to reading everything"
        );
    }

    // -------------------------------------------------------------------------
    // Tier 1 must prune on the column the pipeline READS (#519)
    // -------------------------------------------------------------------------

    /// Synthetic GeoParquet 1.1 footer whose schema is
    /// `[c_xmin, c_ymin, c_xmax, c_ymax (DOUBLE), centroid, geom_shape]`, with
    /// covering-column statistics for the `centroid` column only. The "geo"
    /// JSON declares `centroid`'s covering and — deliberately — **no**
    /// `primary_column`, the shape that made tier 1 fall back to "whichever
    /// column declares a covering".
    fn metadata_with_centroid_covering(bbox: (f64, f64, f64, f64)) -> ParquetMetaData {
        use parquet::basic::Type as PhysicalType;
        use parquet::file::metadata::{
            ColumnChunkMetaData, FileMetaData, KeyValue, RowGroupMetaData,
        };
        use parquet::file::statistics::Statistics;
        use parquet::schema::types::{SchemaDescriptor, Type};
        use std::sync::Arc;

        let mut fields: Vec<Arc<Type>> = ["c_xmin", "c_ymin", "c_xmax", "c_ymax"]
            .iter()
            .map(|n| {
                Arc::new(
                    Type::primitive_type_builder(n, PhysicalType::DOUBLE)
                        .build()
                        .unwrap(),
                )
            })
            .collect();
        for n in ["centroid", "geom_shape"] {
            fields.push(Arc::new(
                Type::primitive_type_builder(n, PhysicalType::BYTE_ARRAY)
                    .build()
                    .unwrap(),
            ));
        }
        let schema = Type::group_type_builder("schema")
            .with_fields(fields)
            .build()
            .unwrap();
        let descr = Arc::new(SchemaDescriptor::new(Arc::new(schema)));

        let (xmin, ymin, xmax, ymax) = bbox;
        let mut columns: Vec<ColumnChunkMetaData> = [xmin, ymin, xmax, ymax]
            .into_iter()
            .enumerate()
            .map(|(i, v)| {
                ColumnChunkMetaData::builder(descr.column(i))
                    .set_statistics(Statistics::double(Some(v), Some(v), None, Some(0), false))
                    .build()
                    .unwrap()
            })
            .collect();
        for i in 4..6 {
            columns.push(
                ColumnChunkMetaData::builder(descr.column(i))
                    .build()
                    .unwrap(),
            );
        }
        let rg = RowGroupMetaData::builder(descr.clone())
            .set_num_rows(10)
            .set_column_metadata(columns)
            .build()
            .unwrap();

        let geo_json = r#"{"version":"1.1.0","columns":{
            "centroid":{"encoding":"WKB","covering":{"bbox":{
                "xmin":["c_xmin"],"ymin":["c_ymin"],
                "xmax":["c_xmax"],"ymax":["c_ymax"]}}},
            "geom_shape":{"encoding":"WKB"}}}"#;
        let kv = Some(vec![KeyValue::new("geo".to_string(), geo_json.to_string())]);
        let file_meta = FileMetaData::new(2, 10, None, kv, descr, None);
        ParquetMetaData::new(file_meta, vec![rg])
    }

    /// #519: the centroids sit at (100..110, 60..70); the shapes they
    /// summarize span the whole world for all pruning knows, because
    /// `geom_shape` declares no covering. A bbox over the shapes must KEEP
    /// the row group. Tier 1 used to take the `centroid` covering (first
    /// column with one, `primary_column` absent) and prune the row group
    /// away — the reader would have found the features, pruning never let it
    /// look.
    #[test]
    fn tier1_ignores_another_columns_covering() {
        let metadata = metadata_with_centroid_covering((100.0, 60.0, 110.0, 70.0));
        assert_eq!(
            resolve_geometry_column_name(&metadata).as_deref(),
            Some("geom_shape"),
            "the pipeline reads geom_shape (the first `geom*` name)"
        );
        assert_eq!(
            extract_row_group_bounds_from_metadata(&metadata).unwrap(),
            vec![None],
            "geom_shape declares no covering ⇒ no tier-1 bounds"
        );
        assert_eq!(
            crate::overview::convert::select_input_row_groups(&metadata, &[0.0, 0.0, 1.0, 1.0]),
            vec![0],
            "pruning on the centroid covering is silent data loss"
        );
    }

    // -------------------------------------------------------------------------
    // Tier 2 must prune on the column the pipeline READS (#518)
    // -------------------------------------------------------------------------

    /// Synthetic GeoParquet 2.0 footer with TWO annotated geometry columns —
    /// `centroid` at leaf 0 and `geom_shape` at leaf 1 — each carrying its
    /// own native `geo_statistics` bbox, in
    /// [`parquet::geospatial::bounding_box::BoundingBox::new`]'s
    /// `(xmin, xmax, ymin, ymax)` order. `geo_json` optionally attaches a
    /// "geo" key declaring a `primary_column`.
    fn metadata_with_two_geometry_columns(
        centroid: Option<(f64, f64, f64, f64)>,
        shape: Option<(f64, f64, f64, f64)>,
        geo_json: Option<&str>,
    ) -> ParquetMetaData {
        use parquet::basic::Type as PhysicalType;
        use parquet::file::metadata::{
            ColumnChunkMetaData, FileMetaData, KeyValue, RowGroupMetaData,
        };
        use parquet::geospatial::bounding_box::BoundingBox;
        use parquet::geospatial::statistics::GeospatialStatistics;
        use parquet::schema::types::{SchemaDescriptor, Type};
        use std::sync::Arc;

        let fields: Vec<Arc<Type>> = ["centroid", "geom_shape"]
            .iter()
            .map(|n| {
                Arc::new(
                    Type::primitive_type_builder(n, PhysicalType::BYTE_ARRAY)
                        .with_logical_type(Some(LogicalType::geometry(None)))
                        .build()
                        .unwrap(),
                )
            })
            .collect();
        let schema = Type::group_type_builder("schema")
            .with_fields(fields)
            .build()
            .unwrap();
        let descr = Arc::new(SchemaDescriptor::new(Arc::new(schema)));

        let columns: Vec<ColumnChunkMetaData> = [centroid, shape]
            .into_iter()
            .enumerate()
            .map(|(i, bbox)| {
                let mut b = ColumnChunkMetaData::builder(descr.column(i));
                if let Some((xmin, xmax, ymin, ymax)) = bbox {
                    b = b.set_geo_statistics(Box::new(GeospatialStatistics::new(
                        Some(BoundingBox::new(xmin, xmax, ymin, ymax)),
                        None,
                    )));
                }
                b.build().unwrap()
            })
            .collect();
        let rg = RowGroupMetaData::builder(descr.clone())
            .set_num_rows(10)
            .set_column_metadata(columns)
            .build()
            .unwrap();
        let kv = geo_json.map(|g| vec![KeyValue::new("geo".to_string(), g.to_string())]);
        let file_meta = FileMetaData::new(2, 10, None, kv, descr, None);
        ParquetMetaData::new(file_meta, vec![rg])
    }

    /// #518 (1) — the motivating probe. A file with `centroid` (leaf 0) and
    /// `geom_shape` (leaf 1) both annotated: the pipeline reads `geom_shape`
    /// (the first name containing "geom"), so pruning must use THAT
    /// envelope. The old "first annotated leaf in schema order" rule pruned
    /// on the far-away centroid envelope instead, and every matching feature
    /// vanished from the output.
    #[test]
    fn tier2_prunes_on_the_column_the_pipeline_reads() {
        // Centroids parked far away; the shapes span [0,10]².
        let metadata = metadata_with_two_geometry_columns(
            Some((100.0, 110.0, 60.0, 70.0)),
            Some((0.0, 10.0, 0.0, 10.0)),
            None,
        );
        assert_eq!(
            resolve_geometry_column_name(&metadata).as_deref(),
            Some("geom_shape"),
            "no geo JSON ⇒ the `geom*` heuristic, matching find_geometry_column"
        );

        let bounds = extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg4326,
            resolve_geometry_column_name(&metadata).as_deref(),
        );
        assert_eq!(
            bounds[0].as_ref().map(|b| (b.xmin, b.ymin, b.xmax, b.ymax)),
            Some((0.0, 0.0, 10.0, 10.0)),
            "tier 2 must read geom_shape's envelope, not the centroid's"
        );

        let selected =
            crate::overview::convert::select_input_row_groups(&metadata, &[0.0, 0.0, 1.0, 1.0]);
        assert_eq!(
            selected,
            vec![0],
            "a bbox over the shapes must KEEP the row group (silent data loss otherwise)"
        );
    }

    /// The same file with a "geo" JSON naming `geom_shape` as the primary
    /// column: the declaration is authoritative and reaches the same answer.
    #[test]
    fn tier2_follows_the_geo_json_primary_column() {
        let geo = r#"{"version":"1.1.0","primary_column":"geom_shape","columns":{"geom_shape":{"encoding":"WKB"}}}"#;
        let metadata = metadata_with_two_geometry_columns(
            Some((100.0, 110.0, 60.0, 70.0)),
            Some((0.0, 10.0, 0.0, 10.0)),
            Some(geo),
        );
        assert_eq!(
            resolve_geometry_column_name(&metadata).as_deref(),
            Some("geom_shape")
        );
        let selected =
            crate::overview::convert::select_input_row_groups(&metadata, &[0.0, 0.0, 1.0, 1.0]);
        assert_eq!(selected, vec![0]);

        // A primary_column naming the centroid makes the centroid the column
        // the pipeline reads too (find_geometry_column honors it), so
        // pruning on the centroid envelope is then the CORRECT answer.
        let geo_centroid = r#"{"version":"1.1.0","primary_column":"centroid","columns":{"centroid":{"encoding":"WKB"}}}"#;
        let metadata = metadata_with_two_geometry_columns(
            Some((100.0, 110.0, 60.0, 70.0)),
            Some((0.0, 10.0, 0.0, 10.0)),
            Some(geo_centroid),
        );
        assert_eq!(
            resolve_geometry_column_name(&metadata).as_deref(),
            Some("centroid")
        );
        assert_eq!(
            crate::overview::convert::select_input_row_groups(
                &metadata,
                &[105.0, 65.0, 106.0, 66.0]
            ),
            vec![0],
        );
    }

    /// Only the WRONG column is annotated: tier 2 must stand down entirely
    /// (no bounds ⇒ the row group is read), never fall back to whatever
    /// annotated leaf it can find.
    #[test]
    fn tier2_skipped_when_only_another_column_is_annotated() {
        let metadata =
            metadata_with_two_geometry_columns(Some((100.0, 110.0, 60.0, 70.0)), None, None);
        let bounds = extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg4326,
            resolve_geometry_column_name(&metadata).as_deref(),
        );
        assert_eq!(
            bounds,
            vec![None],
            "geom_shape has no geo_statistics ⇒ no tier-2 bounds at all"
        );
        assert_eq!(
            crate::overview::convert::select_input_row_groups(&metadata, &[0.0, 0.0, 1.0, 1.0]),
            vec![0],
            "must degrade to reading the row group"
        );

        // And an unresolvable geometry column skips tier 2 outright.
        assert_eq!(
            extract_row_group_bounds_tiered(&metadata, Crs::Epsg4326, None),
            vec![None]
        );
    }

    /// #518 (4): arrow-rs writes an unset Parquet `Geometry` CRS as the
    /// literal "srid:0", which the spec defines as OGC:CRS84. Reading it as
    /// "unclassifiable" disabled tier 2 on the single most common
    /// GeoParquet 2.0 shape there is.
    #[test]
    fn srid_zero_is_the_lonlat_default() {
        let specs = [
            TieredRowGroupSpec {
                covering: None,
                native: Some((0.0, 10.0, 0.0, 10.0)),
                num_rows: 10,
            },
            TieredRowGroupSpec {
                covering: None,
                native: Some((50.0, 60.0, 50.0, 60.0)),
                num_rows: 10,
            },
        ];
        for crs in ["srid:0", ""] {
            let metadata = metadata_with_tiered_stats(Some(crs), false, &specs);
            let bounds = extract_row_group_bounds_tiered(
                &metadata,
                Crs::Epsg4326,
                resolve_geometry_column_name(&metadata).as_deref(),
            );
            assert!(
                bounds.iter().all(Option::is_some),
                "{crs:?} means unset ⇒ OGC:CRS84, so tier 2 must fire"
            );
            assert_eq!(
                crate::overview::convert::select_input_row_groups(&metadata, &[0.0, 0.0, 1.0, 1.0]),
                vec![0],
                "{crs:?}: pruning must be active"
            );
        }
    }

    /// The tier-2 CRS gate now runs through
    /// `quality::classify_crs_identifier`: a Web Mercator column must not
    /// be mistaken for lon/lat because its PROJJSON says "WGS 84 /
    /// Pseudo-Mercator" (#518 (2)).
    #[test]
    fn tier2_classifies_pseudo_mercator_projjson_as_3857() {
        let specs = [TieredRowGroupSpec {
            covering: None,
            native: Some((-1.0e7, -1.0e7 + 10.0, -1.0e7, -1.0e7 + 10.0)),
            num_rows: 10,
        }];
        let projjson = r#"{"type":"ProjectedCRS","name":"WGS 84 / Pseudo-Mercator","id":{"authority":"EPSG","code":3857}}"#;
        let metadata = metadata_with_tiered_stats(Some(projjson), false, &specs);

        // Session says degrees: the column is meters, so tier 2 stands down
        // rather than filtering meters against a degree bbox (which pruned
        // every row group into a silently EMPTY archive).
        assert_eq!(
            extract_row_group_bounds_tiered(
                &metadata,
                Crs::Epsg4326,
                resolve_geometry_column_name(&metadata).as_deref()
            ),
            vec![None],
        );
        // Session says meters: consistent, so tier 2 fires.
        assert!(extract_row_group_bounds_tiered(
            &metadata,
            Crs::Epsg3857,
            resolve_geometry_column_name(&metadata).as_deref()
        )[0]
        .is_some());
    }

    // -------------------------------------------------------------------------
    // Property test (#497): tier-2 bounds never prune a row group that
    // truly contains a matching point.
    // -------------------------------------------------------------------------

    mod native_stats_proptest {
        use super::*;
        use proptest::prelude::*;

        fn point_in_bbox(p: (f64, f64), b: &TileBounds) -> bool {
            p.0 >= b.lng_min && p.0 <= b.lng_max && p.1 >= b.lat_min && p.1 <= b.lat_max
        }

        proptest! {
            #[test]
            fn selection_never_drops_a_true_match(
                groups in proptest::collection::vec(
                    proptest::collection::vec((-100.0f64..100.0, -100.0f64..100.0), 1..6),
                    1..6,
                ),
                qx0 in -100.0f64..100.0, qx1 in -100.0f64..100.0,
                qy0 in -100.0f64..100.0, qy1 in -100.0f64..100.0,
            ) {
                let filter = TileBounds {
                    lng_min: qx0.min(qx1),
                    lat_min: qy0.min(qy1),
                    lng_max: qx0.max(qx1),
                    lat_max: qy0.max(qy1),
                };
                // Scope (#519): this case exercises the *intersection
                // predicate* over tight per-row-group envelopes — the
                // arithmetic core, including the antimeridian wraparound and
                // NaN rules. It deliberately does NOT go through
                // `extract_row_group_bounds_tiered`; the sibling property
                // below does that over real `ParquetMetaData`.
                let bounds: Vec<RowGroupBounds> = groups
                    .iter()
                    .enumerate()
                    .map(|(i, pts)| {
                        let xmin = pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
                        let xmax = pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max);
                        let ymin = pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
                        let ymax = pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
                        RowGroupBounds {
                            row_group_idx: i,
                            xmin,
                            ymin,
                            xmax,
                            ymax,
                            num_rows: pts.len(),
                        }
                    })
                    .collect();

                let selected: std::collections::HashSet<usize> = (0..groups.len())
                    .filter(|&i| bounds[i].intersects(&filter))
                    .collect();
                let truth: std::collections::HashSet<usize> = (0..groups.len())
                    .filter(|&i| groups[i].iter().any(|&p| point_in_bbox(p, &filter)))
                    .collect();
                prop_assert!(
                    truth.is_subset(&selected),
                    "pruned a row group with a truly matching point: truth={truth:?} selected={selected:?}"
                );
            }
        }

        // The same invariant, but driven through the REAL tiered path
        // (#519): synthetic-but-genuine `ParquetMetaData` carrying native
        // `geo_statistics` on two geometry columns, fed to
        // `select_input_row_groups` — CRS detection, column resolution, tier
        // selection and all.
        //
        // Both columns get an envelope, and the "geo" JSON's
        // `primary_column` varies over {absent, centroid, geom_shape}, so the
        // property sees every combination of "annotated column" and "column
        // the pipeline reads". Truth is the points of the column the READER
        // would read — which is what pruning must never drop. A prune keyed
        // off the other column's envelope violates it, which is exactly the
        // #518/#519 bug class.
        proptest! {
            #[test]
            fn tiered_selection_never_drops_a_true_match(
                centroid_pts in proptest::collection::vec((-100.0f64..100.0, -100.0f64..100.0), 1..5),
                shape_pts in proptest::collection::vec((-100.0f64..100.0, -100.0f64..100.0), 1..5),
                primary_idx in 0usize..3,
                qx0 in -100.0f64..100.0, qx1 in -100.0f64..100.0,
                qy0 in -100.0f64..100.0, qy1 in -100.0f64..100.0,
            ) {
                /// `(xmin, xmax, ymin, ymax)` — `BoundingBox::new`'s order.
                fn envelope(pts: &[(f64, f64)]) -> (f64, f64, f64, f64) {
                    (
                        pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
                        pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
                        pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
                        pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
                    )
                }

                let primary = [None, Some("centroid"), Some("geom_shape")][primary_idx];
                let geo_json = primary.map(|p| {
                    format!(
                        r#"{{"version":"1.1.0","primary_column":"{p}","columns":{{"{p}":{{"encoding":"WKB"}}}}}}"#
                    )
                });
                let metadata = metadata_with_two_geometry_columns(
                    Some(envelope(&centroid_pts)),
                    Some(envelope(&shape_pts)),
                    geo_json.as_deref(),
                );

                // The column the READER reads: `primary_column` when
                // declared, else the `geom*` heuristic.
                let read_column = primary.unwrap_or("geom_shape");
                let resolved = resolve_geometry_column_name(&metadata);
                prop_assert_eq!(
                    resolved.as_deref(),
                    Some(read_column),
                    "pruning and the reader must agree on the column"
                );
                let read_pts = if read_column == "centroid" {
                    &centroid_pts
                } else {
                    &shape_pts
                };

                let filter = TileBounds {
                    lng_min: qx0.min(qx1),
                    lat_min: qy0.min(qy1),
                    lng_max: qx0.max(qx1),
                    lat_max: qy0.max(qy1),
                };
                let selected = crate::overview::convert::select_input_row_groups(
                    &metadata,
                    &[filter.lng_min, filter.lat_min, filter.lng_max, filter.lat_max],
                );
                let truly_matches = read_pts.iter().any(|&p| point_in_bbox(p, &filter));
                prop_assert!(
                    !truly_matches || selected == vec![0],
                    "pruned the only row group although {read_column} has a matching point: \
                     read_pts={read_pts:?} filter={filter:?} selected={selected:?}"
                );
            }
        }
    }
}
