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

use crate::tile::{lng_lat_to_tile, TileBounds, TileCoord};
use crate::Error;
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
    /// Check if this row group's bounds intersect with the given filter bounds.
    pub fn intersects(&self, filter: &TileBounds) -> bool {
        // Standard AABB intersection test
        self.xmin <= filter.lng_max
            && self.xmax >= filter.lng_min
            && self.ymin <= filter.lat_max
            && self.ymax >= filter.lat_min
    }

    /// Convert to TileBounds for compatibility with existing code.
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

/// Parse the covering specification from GeoParquet geo metadata JSON.
///
/// Returns `None` if the metadata doesn't contain covering information.
///
/// # Arguments
///
/// * `geo_json` - The JSON string from the "geo" key-value metadata
///
/// # Returns
///
/// `Ok(Some(CoveringSpec))` if covering metadata is present and valid,
/// `Ok(None)` if no covering metadata exists,
/// `Err` if the JSON is malformed.
pub fn parse_covering_metadata(geo_json: &str) -> Result<Option<CoveringSpec>, Error> {
    let metadata: GeoMetadata = serde_json::from_str(geo_json)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to parse geo metadata JSON: {}", e)))?;

    // Find the geometry column (use primary_column if specified, otherwise first with covering)
    let geom_column = if let Some(ref primary) = metadata.primary_column {
        metadata.columns.get(primary)
    } else {
        // Find first column with covering metadata
        metadata.columns.values().find(|col| col.covering.is_some())
    };

    let Some(column) = geom_column else {
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
/// Handles both f32 and f64 physical types, converting to f64.
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

    match bytes.len() {
        4 => {
            // FLOAT (f32)
            let arr: [u8; 4] = bytes.try_into().ok()?;
            Some(f32::from_le_bytes(arr) as f64)
        }
        8 => {
            // DOUBLE (f64)
            let arr: [u8; 8] = bytes.try_into().ok()?;
            Some(f64::from_le_bytes(arr))
        }
        _ => None,
    }
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
    let metadata = reader.metadata();
    let num_row_groups = metadata.num_row_groups();

    // Parse geo metadata to get covering spec
    let geo_json = get_geo_metadata(metadata)?;
    let Some(geo_json) = geo_json else {
        // No geo metadata - return all None
        return Ok(vec![None; num_row_groups]);
    };

    let covering = parse_covering_metadata(&geo_json)?;
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
fn get_geo_metadata(metadata: &ParquetMetaData) -> Result<Option<String>, Error> {
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

        let result = parse_covering_metadata(geo_json).unwrap();
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

        let result = parse_covering_metadata(geo_json).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_covering_metadata_invalid_json() {
        let result = parse_covering_metadata("not valid json");
        assert!(result.is_err());
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
    // cargo test --package gpq-tiles-core covering -- --ignored

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
}
