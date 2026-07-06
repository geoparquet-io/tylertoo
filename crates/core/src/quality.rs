//! GeoParquet CRS extraction and WGS84 validation.
//!
//! Reads the GeoParquet `geo` metadata to determine the input file's
//! coordinate reference system and rejects non-WGS84 inputs with a
//! `gpio`-based reprojection hint.

use std::path::Path;

use parquet::file::reader::FileReader;
use parquet::file::serialized_reader::SerializedFileReader;
use serde_json::Value;

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
    use crate::batch_processor::resolve_parquet_files;

    // Resolve to first file if path is a directory
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
    crs_info_from_kv_metadata(file_metadata.key_value_metadata())
}

/// Extract CRS information from already-parsed parquet key-value metadata.
///
/// The metadata-only core of [`extract_crs`], shared with input paths that
/// have a parsed footer in hand already (e.g. remote inputs, #210, where the
/// footer was range-fetched once and re-opening the file would cost another
/// round trip).
pub fn crs_info_from_kv_metadata(
    kv_metadata: Option<&Vec<parquet::file::metadata::KeyValue>>,
) -> Result<CrsInfo> {
    // Look for the "geo" key in key-value metadata
    let Some(kv_metadata) = kv_metadata else {
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
#[cfg(test)]
mod tests {
    use super::*;

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
