//! GeoParquet file quality assessment for streaming optimization.
//!
//! Detects whether a GeoParquet file is well-optimized for streaming processing:
//! - Has geo metadata extension
//! - Has row group bounding boxes
//! - Is spatially sorted (Hilbert)
//! - Uses WGS84 (EPSG:4326) coordinate reference system
//!
//! Emits warnings when files could benefit from optimization with geoparquet-io tools.

use std::path::Path;

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::reader::FileReader;
use parquet::file::serialized_reader::SerializedFileReader;
use serde_json::Value;

use crate::batch_processor::resolve_parquet_files;
use crate::{Error, Result};

/// CRS information extracted from GeoParquet metadata.
#[derive(Debug, Clone)]
pub struct CrsInfo {
    /// The raw CRS identifier (e.g., "EPSG:4326", "OGC:CRS84", or PROJJSON)
    pub identifier: Option<String>,
    /// Whether the CRS is WGS84-compatible (EPSG:4326 or OGC:CRS84)
    pub is_wgs84: bool,
    /// Human-readable CRS name if available
    pub name: Option<String>,
}

impl CrsInfo {
    /// Create CrsInfo indicating WGS84
    fn wgs84() -> Self {
        Self {
            identifier: Some("EPSG:4326".to_string()),
            is_wgs84: true,
            name: Some("WGS 84".to_string()),
        }
    }

    /// Create CrsInfo from a CRS identifier string
    fn from_identifier(id: &str) -> Self {
        let is_wgs84 = is_wgs84_identifier(id);
        Self {
            identifier: Some(id.to_string()),
            is_wgs84,
            name: None,
        }
    }

    /// Create CrsInfo indicating unknown/missing CRS
    fn unknown() -> Self {
        Self {
            identifier: None,
            is_wgs84: false,
            name: None,
        }
    }
}

/// Check if a CRS identifier represents WGS84 or compatible CRS.
fn is_wgs84_identifier(id: &str) -> bool {
    let id_upper = id.to_uppercase();
    // Common WGS84 identifiers
    id_upper == "EPSG:4326"
        || id_upper == "OGC:CRS84"
        || id_upper == "CRS84"
        || id_upper == "URN:OGC:DEF:CRS:OGC::CRS84"
        || id_upper == "URN:OGC:DEF:CRS:EPSG::4326"
        || id_upper.contains("WGS 84")
        || id_upper.contains("WGS84")
}

/// Check if PROJJSON represents WGS84.
fn is_wgs84_projjson(projjson: &Value) -> bool {
    // Check for EPSG code
    if let Some(id) = projjson.get("id") {
        let authority = id.get("authority").and_then(Value::as_str);
        let code_i64 = id.get("code").and_then(Value::as_i64);
        let code_str = id.get("code").and_then(Value::as_str);

        if authority == Some("EPSG") && code_i64 == Some(4326) {
            return true;
        }
        if authority == Some("OGC") && code_str == Some("CRS84") {
            return true;
        }
    }

    // Check the name as fallback
    if let Some(name) = projjson.get("name").and_then(Value::as_str) {
        if name.to_uppercase().contains("WGS 84") || name.to_uppercase().contains("WGS84") {
            return true;
        }
    }

    false
}

/// Extract CRS information from GeoParquet file metadata.
///
/// Reads the `geo` key-value metadata and extracts CRS from the primary geometry column.
///
/// # Arguments
///
/// * `path` - Path to the GeoParquet file
///
/// # Returns
///
/// CRS information, or an error if the file cannot be read.
pub fn extract_crs(path: &Path) -> Result<CrsInfo> {
    // For directories, use the first file (all files should have the same CRS)
    let files = resolve_parquet_files(path)?;
    let first_file = files
        .first()
        .ok_or_else(|| Error::GeoParquetRead("No parquet files found".to_string()))?;

    let file = std::fs::File::open(first_file)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to open file: {}", e)))?;

    let reader = SerializedFileReader::new(file)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to create parquet reader: {}", e)))?;

    let metadata = reader.metadata();
    let file_metadata = metadata.file_metadata();

    // Look for the "geo" key in key-value metadata
    let Some(kv_metadata) = file_metadata.key_value_metadata() else {
        // No metadata at all - assume WGS84 with warning
        tracing::warn!("GeoParquet file has no key-value metadata; assuming WGS84");
        return Ok(CrsInfo::wgs84());
    };

    let geo_value = kv_metadata
        .iter()
        .find(|kv| kv.key.to_lowercase() == "geo")
        .and_then(|kv| kv.value.as_ref());

    let Some(geo_json_str) = geo_value else {
        // No geo metadata - assume WGS84 with warning
        tracing::warn!("GeoParquet file has no 'geo' metadata; assuming WGS84");
        return Ok(CrsInfo::wgs84());
    };

    // Parse the geo metadata JSON
    let geo_json: Value = serde_json::from_str(geo_json_str)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to parse geo metadata JSON: {}", e)))?;

    // Get the primary geometry column name (default is "geometry")
    let primary_column = geo_json
        .get("primary_column")
        .and_then(Value::as_str)
        .unwrap_or("geometry");

    // Get the columns object
    let Some(columns) = geo_json.get("columns").and_then(Value::as_object) else {
        tracing::warn!("GeoParquet 'geo' metadata has no 'columns'; assuming WGS84");
        return Ok(CrsInfo::wgs84());
    };

    // Get the primary column's metadata
    let Some(column_meta) = columns.get(primary_column) else {
        tracing::warn!(
            "GeoParquet 'geo' metadata missing column '{}'; assuming WGS84",
            primary_column
        );
        return Ok(CrsInfo::wgs84());
    };

    // Extract CRS - can be a string identifier or PROJJSON object
    let crs = column_meta.get("crs");

    match crs {
        None => {
            // No CRS specified - GeoParquet spec says this means WGS84
            Ok(CrsInfo::wgs84())
        }
        Some(Value::Null) => {
            // Explicitly null CRS means "no CRS" (engineering coordinates)
            // We'll treat this as unknown and let the caller decide
            Ok(CrsInfo::unknown())
        }
        Some(Value::String(crs_str)) => {
            // Simple CRS identifier (e.g., "EPSG:4326")
            Ok(CrsInfo::from_identifier(crs_str))
        }
        Some(crs_obj @ Value::Object(_)) => {
            // PROJJSON object
            let is_wgs84 = is_wgs84_projjson(crs_obj);
            let name = crs_obj.get("name").and_then(Value::as_str);
            let id_str = crs_obj.get("id").map(|id| {
                let authority = id.get("authority").and_then(Value::as_str).unwrap_or("");
                let code = id
                    .get("code")
                    .map(|c| {
                        c.as_str()
                            .map(|s| s.to_string())
                            .or_else(|| c.as_i64().map(|n| n.to_string()))
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                format!("{}:{}", authority, code)
            });

            Ok(CrsInfo {
                identifier: id_str.or_else(|| name.map(|n| n.to_string())),
                is_wgs84,
                name: name.map(|s| s.to_string()),
            })
        }
        Some(other) => {
            tracing::warn!("Unexpected CRS format in GeoParquet metadata: {:?}", other);
            Ok(CrsInfo::unknown())
        }
    }
}

/// Validate that a GeoParquet file uses WGS84 (EPSG:4326) coordinates.
///
/// Returns an error with a helpful message if the file uses a different CRS.
///
/// # Arguments
///
/// * `path` - Path to the GeoParquet file
///
/// # Returns
///
/// Ok(()) if the file uses WGS84, or an error with reprojection instructions.
pub fn validate_wgs84(path: &Path) -> Result<()> {
    let crs_info = extract_crs(path)?;

    if crs_info.is_wgs84 {
        return Ok(());
    }

    // Build a helpful error message
    let crs_desc = match (&crs_info.identifier, &crs_info.name) {
        (Some(id), Some(name)) => format!("'{}' ({})", id, name),
        (Some(id), None) => format!("'{}'", id),
        (None, Some(name)) => format!("'{}'", name),
        (None, None) => "an unknown CRS".to_string(),
    };

    let filename = path.file_name().unwrap_or_default().to_string_lossy();

    Err(Error::GeoParquetRead(format!(
        "Input file uses CRS {}.\n\
         gpq-tiles requires WGS84 (EPSG:4326) coordinates.\n\n\
         Reproject with geoparquet-io:\n  \
         gpio convert reproject {} reprojected.parquet -d EPSG:4326",
        crs_desc, filename
    )))
}

/// Quality assessment of a GeoParquet file for streaming processing.
#[derive(Debug, Clone)]
pub struct GeoParquetQuality {
    /// Whether the file has GeoParquet `geo` metadata extension
    pub has_geo_metadata: bool,
    /// Whether row groups have bounding box metadata
    pub has_row_group_bboxes: bool,
    /// Number of row groups in the file
    pub row_group_count: usize,
    /// Total number of rows in the file
    pub total_rows: usize,
    /// Average rows per row group
    pub avg_rows_per_group: usize,
    /// File size in bytes
    pub file_size_bytes: u64,
    /// Percentage of row groups with overlapping bboxes (None if not checked)
    pub row_group_overlap_pct: Option<f32>,
    /// Whether features appear to be Hilbert-sorted (None if not checked for small files)
    pub is_hilbert_sorted: Option<bool>,
}

/// Minimum recommended rows per row group for efficient processing
pub const MIN_RECOMMENDED_ROWS_PER_GROUP: usize = 100;

impl GeoParquetQuality {
    /// Returns true if the file is well-optimized for streaming
    pub fn is_optimized(&self) -> bool {
        self.has_geo_metadata
            && (self.row_group_count <= 1 || self.has_row_group_bboxes)
            && self.is_hilbert_sorted.unwrap_or(true)
            && self.avg_rows_per_group >= MIN_RECOMMENDED_ROWS_PER_GROUP
    }

    /// Returns a list of optimization suggestions
    pub fn suggestions(&self) -> Vec<String> {
        let mut suggestions = Vec::new();

        if !self.has_geo_metadata {
            suggestions.push("File missing GeoParquet metadata extension".to_string());
        }

        if self.row_group_count > 1 && !self.has_row_group_bboxes {
            suggestions
                .push("Row groups lack bounding box metadata - cannot skip spatially".to_string());
        }

        // Check for pathologically small row groups (major performance issue!)
        // Recommended: 50,000-150,000 rows per group, minimum 100 for reasonable perf
        if self.row_group_count > 1 && self.avg_rows_per_group < MIN_RECOMMENDED_ROWS_PER_GROUP {
            suggestions.push(format!(
                "Row groups too small: {} rows/group (need {}+). This causes ~{}x slowdown!",
                self.avg_rows_per_group,
                MIN_RECOMMENDED_ROWS_PER_GROUP,
                MIN_RECOMMENDED_ROWS_PER_GROUP / self.avg_rows_per_group.max(1)
            ));
        }

        // Large file with few row groups
        let size_mb = self.file_size_bytes / (1024 * 1024);
        if size_mb > 500 && self.row_group_count < 5 {
            suggestions.push(format!(
                "Large file ({}MB) with only {} row groups limits streaming efficiency",
                size_mb, self.row_group_count
            ));
        }

        if let Some(overlap) = self.row_group_overlap_pct {
            if overlap > 20.0 {
                suggestions.push(format!(
                    "Row group bboxes overlap significantly ({:.1}%)",
                    overlap
                ));
            }
        }

        if let Some(false) = self.is_hilbert_sorted {
            suggestions.push("File does not appear to be spatially sorted".to_string());
        }

        suggestions
    }
}

/// Assess the quality of a GeoParquet file or directory for streaming processing.
///
/// For directories, aggregates metrics across all files and checks the first file
/// for metadata quality (all files should have consistent metadata).
///
/// Performs cheap checks (O(1) metadata reads) first, then more expensive
/// checks (sampling) only for large files.
pub fn assess_quality(path: &Path) -> Result<GeoParquetQuality> {
    let files = resolve_parquet_files(path)?;
    let first_file = files
        .first()
        .ok_or_else(|| Error::GeoParquetRead("No parquet files found".to_string()))?;

    // Aggregate metrics across all files
    let mut total_file_size_bytes = 0u64;
    let mut total_row_groups = 0usize;
    let mut total_rows = 0usize;

    for file_path in &files {
        let file = std::fs::File::open(file_path)
            .map_err(|e| Error::GeoParquetRead(format!("Failed to open file: {}", e)))?;

        total_file_size_bytes += file
            .metadata()
            .map_err(|e| Error::GeoParquetRead(format!("Failed to get file metadata: {}", e)))?
            .len();

        let reader = SerializedFileReader::new(file).map_err(|e| {
            Error::GeoParquetRead(format!("Failed to create parquet reader: {}", e))
        })?;

        let parquet_metadata = reader.metadata();
        total_row_groups += parquet_metadata.num_row_groups();
        total_rows += parquet_metadata.file_metadata().num_rows() as usize;
    }

    // Use the first file for metadata quality checks
    let first_file_handle = std::fs::File::open(first_file)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to open file: {}", e)))?;

    let reader = SerializedFileReader::new(first_file_handle)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to create parquet reader: {}", e)))?;

    let parquet_metadata = reader.metadata();
    let file_metadata = parquet_metadata.file_metadata();

    // Check for geo metadata in key-value pairs
    let has_geo_metadata = file_metadata
        .key_value_metadata()
        .map(|kv| {
            kv.iter()
                .any(|pair| pair.key.to_lowercase().contains("geo"))
        })
        .unwrap_or(false);

    let avg_rows_per_group = if total_row_groups > 0 {
        total_rows / total_row_groups
    } else {
        0
    };

    // Check for row group bboxes (would be in column statistics or custom metadata)
    // For now, we check if row groups have statistics on geometry-like columns
    let has_row_group_bboxes = check_row_group_bboxes(&reader);

    // For large files (>1GB), sample to check Hilbert sorting
    let is_hilbert_sorted = if total_file_size_bytes > 1024 * 1024 * 1024 {
        Some(check_hilbert_sorted(first_file)?)
    } else {
        None // Don't check for small files
    };

    Ok(GeoParquetQuality {
        has_geo_metadata,
        has_row_group_bboxes,
        row_group_count: total_row_groups,
        total_rows,
        avg_rows_per_group,
        file_size_bytes: total_file_size_bytes,
        row_group_overlap_pct: None, // TODO: implement bbox overlap check
        is_hilbert_sorted,
    })
}

/// Check if row groups have bounding box information.
fn check_row_group_bboxes(reader: &SerializedFileReader<std::fs::File>) -> bool {
    let metadata = reader.metadata();

    // Check if any row group has statistics that could serve as bbox
    // In well-formed GeoParquet, the geo metadata should contain bbox per row group
    // For now, just check if there's more than one row group with statistics
    if metadata.num_row_groups() <= 1 {
        return true; // Single row group doesn't need bbox filtering
    }

    // Check file-level geo metadata for covering information
    if let Some(kv) = metadata.file_metadata().key_value_metadata() {
        for pair in kv {
            if pair.key.to_lowercase() == "geo" {
                if let Some(value) = &pair.value {
                    // Check if geo metadata contains "covering" which indicates bbox support
                    if value.contains("covering") || value.contains("bbox") {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Sample the first N features to check if they're Hilbert-sorted.
fn check_hilbert_sorted(path: &Path) -> Result<bool> {
    use crate::spatial_index::{encode_hilbert, lng_lat_to_world_coords};
    use geo::{BoundingRect, Centroid};

    let file = std::fs::File::open(path)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to open file: {}", e)))?;

    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to create reader: {}", e)))?;

    // Only read first batch for sampling
    let reader = builder
        .with_batch_size(1000)
        .build()
        .map_err(|e| Error::GeoParquetRead(format!("Failed to build reader: {}", e)))?;

    let mut hilbert_indices: Vec<u64> = Vec::new();

    // Process first batch only
    if let Some(batch_result) = reader.into_iter().next() {
        let batch = batch_result
            .map_err(|e| Error::GeoParquetRead(format!("Failed to read batch: {}", e)))?;

        // Use our existing batch processor to extract geometries
        let schema = batch.schema();
        let geom_idx = schema
            .fields()
            .iter()
            .position(|f| f.name() == "geometry" || f.name().contains("geom"));

        if let Some(idx) = geom_idx {
            let geom_col = batch.column(idx);
            let geom_field = schema.field(idx);

            // Convert to GeoArrow and extract centroids
            let geom_array = geoarrow::array::from_arrow_array(geom_col.as_ref(), geom_field)
                .map_err(|e| {
                    Error::GeoParquetRead(format!("Failed to parse geometry array: {}", e))
                })?;

            // Get centroids and compute Hilbert indices
            use geoarrow::datatypes::GeoArrowType;
            use geoarrow_array::cast::AsGeoArrowArray;
            use geoarrow_array::GeoArrowArrayAccessor;

            match geom_array.data_type() {
                GeoArrowType::Polygon(_) => {
                    let arr = geom_array.as_polygon();
                    for item in arr.iter().take(1000) {
                        if let Some(Ok(poly)) = item {
                            use geo_traits::to_geo::ToGeoGeometry;
                            if let Some(geom) = poly.try_to_geometry() {
                                if let Some(centroid) = geom.centroid() {
                                    let (wx, wy) =
                                        lng_lat_to_world_coords(centroid.x(), centroid.y());
                                    hilbert_indices.push(encode_hilbert(wx, wy));
                                }
                            }
                        }
                    }
                }
                GeoArrowType::Point(_) => {
                    let arr = geom_array.as_point();
                    for item in arr.iter().take(1000) {
                        if let Some(Ok(pt)) = item {
                            use geo_traits::to_geo::ToGeoGeometry;
                            if let Some(geom) = pt.try_to_geometry() {
                                if let Some(rect) = geom.bounding_rect() {
                                    let center = rect.center();
                                    let (wx, wy) = lng_lat_to_world_coords(center.x, center.y);
                                    hilbert_indices.push(encode_hilbert(wx, wy));
                                }
                            }
                        }
                    }
                }
                _ => {
                    // For other geometry types, skip the check
                    return Ok(true);
                }
            }
        }
    }

    // Check if indices are mostly sorted (allow 5% out of order)
    if hilbert_indices.len() < 10 {
        return Ok(true); // Too few samples to determine
    }

    let mut inversions = 0;
    for i in 1..hilbert_indices.len() {
        if hilbert_indices[i] < hilbert_indices[i - 1] {
            inversions += 1;
        }
    }

    let inversion_rate = inversions as f64 / hilbert_indices.len() as f64;
    Ok(inversion_rate < 0.05) // Less than 5% inversions = considered sorted
}

/// Emit quality warnings to stderr.
///
/// If `quiet` is true, no warnings are emitted.
pub fn emit_quality_warnings(quality: &GeoParquetQuality, quiet: bool) {
    if quiet {
        return;
    }

    let suggestions = quality.suggestions();
    if suggestions.is_empty() {
        return;
    }

    // Check if this is a severe row group issue (major performance impact)
    let has_row_group_issue =
        quality.row_group_count > 1 && quality.avg_rows_per_group < MIN_RECOMMENDED_ROWS_PER_GROUP;

    eprintln!("\n⚠ Input file not optimized for streaming:");
    for suggestion in &suggestions {
        eprintln!("  • {}", suggestion);
    }
    eprintln!();

    if has_row_group_issue {
        eprintln!("  Row group sizing is critical for performance!");
        eprintln!("  See: https://geoparquet.io/concepts/best-practices/#row-group-sizing");
        eprintln!();
        eprintln!("  Fix with gpio:");
        eprintln!("    gpio convert input.parquet output.parquet --row-group-size 100000");
    } else {
        eprintln!("  For best performance, optimize with geoparquet-io:");
        eprintln!(
            "    gpio convert input.parquet output.parquet --hilbert --row-group-size 100000"
        );
    }
    eprintln!();
    eprintln!("  Proceeding anyway (may be slow)...\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_assess_quality_with_geo_metadata() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let quality = assess_quality(fixture).expect("Should assess quality");
        assert!(quality.has_geo_metadata, "Should have geo metadata");
        assert!(
            quality.row_group_count >= 1,
            "Should have at least 1 row group"
        );
    }

    #[test]
    fn test_detects_missing_geo_metadata() {
        let fixture = Path::new("../../tests/fixtures/streaming/no-geo-metadata.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let quality = assess_quality(fixture).expect("Should assess quality");
        assert!(
            !quality.has_geo_metadata,
            "Should detect missing geo metadata"
        );

        let suggestions = quality.suggestions();
        assert!(
            suggestions
                .iter()
                .any(|s| s.contains("missing GeoParquet metadata")),
            "Should suggest adding geo metadata"
        );
    }

    #[test]
    fn test_detects_multiple_row_groups() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let quality = assess_quality(fixture).expect("Should assess quality");
        assert!(
            quality.row_group_count > 1,
            "Should have multiple row groups, got {}",
            quality.row_group_count
        );
    }

    #[test]
    fn test_suggestions_empty_for_good_file() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let quality = assess_quality(fixture).expect("Should assess quality");
        // Good files should have few or no suggestions
        // (single row group files don't need bbox filtering)
        let suggestions = quality.suggestions();
        assert!(
            suggestions.len() <= 1,
            "Good file should have minimal suggestions, got: {:?}",
            suggestions
        );
    }

    // -------------------------------------------------------------------------
    // CRS Validation Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_is_wgs84_identifier() {
        // Should recognize common WGS84 identifiers
        assert!(is_wgs84_identifier("EPSG:4326"));
        assert!(is_wgs84_identifier("epsg:4326"));
        assert!(is_wgs84_identifier("OGC:CRS84"));
        assert!(is_wgs84_identifier("CRS84"));
        assert!(is_wgs84_identifier("urn:ogc:def:crs:EPSG::4326"));
        assert!(is_wgs84_identifier("urn:ogc:def:crs:OGC::CRS84"));

        // Should reject non-WGS84 identifiers
        assert!(!is_wgs84_identifier("EPSG:27700")); // British National Grid
        assert!(!is_wgs84_identifier("EPSG:3857")); // Web Mercator
        assert!(!is_wgs84_identifier("EPSG:32610")); // UTM Zone 10N
    }

    #[test]
    fn test_is_wgs84_projjson() {
        // Test PROJJSON with EPSG:4326 id
        let projjson_4326: Value = serde_json::json!({
            "type": "GeographicCRS",
            "name": "WGS 84",
            "id": {
                "authority": "EPSG",
                "code": 4326
            }
        });
        assert!(is_wgs84_projjson(&projjson_4326));

        // Test PROJJSON with OGC:CRS84 id
        let projjson_crs84: Value = serde_json::json!({
            "type": "GeographicCRS",
            "name": "WGS 84 (CRS84)",
            "id": {
                "authority": "OGC",
                "code": "CRS84"
            }
        });
        assert!(is_wgs84_projjson(&projjson_crs84));

        // Test PROJJSON with non-WGS84 CRS
        let projjson_27700: Value = serde_json::json!({
            "type": "ProjectedCRS",
            "name": "OSGB36 / British National Grid",
            "id": {
                "authority": "EPSG",
                "code": 27700
            }
        });
        assert!(!is_wgs84_projjson(&projjson_27700));
    }

    #[test]
    fn test_extract_crs_wgs84_file() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let crs_info = extract_crs(fixture).expect("Should extract CRS");
        assert!(
            crs_info.is_wgs84,
            "open-buildings fixture should be in WGS84, got: {:?}",
            crs_info
        );
    }

    #[test]
    fn test_validate_wgs84_passes_for_wgs84_file() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let result = validate_wgs84(fixture);
        assert!(
            result.is_ok(),
            "WGS84 file should pass validation: {:?}",
            result
        );
    }
}
