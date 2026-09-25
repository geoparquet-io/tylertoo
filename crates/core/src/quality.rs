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

/// What a CRS declaration names, as far as the two CRSs tylertoo supports are
/// concerned ([`crate::overview::level::Crs`]).
///
/// The single classifier for every CRS string in the codebase: the GeoParquet
/// `geo` JSON `crs` field (string or PROJJSON), and the Parquet
/// `LogicalType::Geometry(crs)` / `Geography(crs)` annotation that
/// [`crate::covering`] prunes on. Both used to run their own substring
/// matches, and both got EPSG:3857 wrong (#518).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrsKind {
    /// Lon/lat WGS84 — EPSG:4326 / OGC:CRS84, including the spec's "unset
    /// CRS means OGC:CRS84" default.
    Wgs84,
    /// Web Mercator meters — EPSG:3857 (or its legacy alias EPSG:900913).
    WebMercator,
    /// Anything else, plus everything that cannot be classified without
    /// guessing. Callers must never prune, reproject or assume on this.
    Unknown,
}

/// Classify a CRS identifier *string*.
///
/// Order matters (#518): EPSG:3857's own name is "WGS 84 / Pseudo-Mercator"
/// and every UTM zone is "WGS 84 / UTM zone …", so a `contains("WGS 84")`
/// test run first classifies half the projected world as lon/lat — which
/// downstream means degrees-vs-meters silent data loss. Web Mercator markers
/// are tested first, and a WGS-84 *substring* only counts when the string is
/// not a projected-CRS name (those always carry a `/`).
///
/// A PROJJSON-shaped string is never substring-matched: it is parsed and
/// classified structurally ([`classify_crs_projjson`]), or, if it will not
/// parse, read as [`CrsKind::Unknown`].
pub(crate) fn classify_crs_identifier(id: &str) -> CrsKind {
    let id = id.trim();

    // An unset CRS means OGC:CRS84 per the GeoParquet/Parquet specs. arrow-rs
    // writes the unset Parquet `Geometry` CRS as the literal "srid:0" (#518),
    // which is by far the most common shape of a GeoParquet 2.0 file in the
    // wild — reading it as "unclassifiable" would disable pruning there.
    if id.is_empty() || id.eq_ignore_ascii_case("srid:0") {
        return CrsKind::Wgs84;
    }

    // PROJJSON inline in a string field: structure, never substrings.
    if id.starts_with('{') || id.contains("\"type\"") {
        return match serde_json::from_str::<Value>(id) {
            Ok(v) => classify_crs_projjson(&v),
            Err(_) => CrsKind::Unknown,
        };
    }

    let up = id.to_uppercase();

    // 1. Web Mercator, BEFORE any WGS-84 test.
    if is_web_mercator_marker(&up) {
        return CrsKind::WebMercator;
    }
    // 2. Explicit lon/lat identifiers, including the URN/URL spellings
    //    ("urn:ogc:def:crs:EPSG::4326", "http://.../EPSG/0/4326").
    if up.contains("4326") || up.contains("CRS84") || up == "SRID:4326" {
        return CrsKind::Wgs84;
    }
    // 3. Bare WGS-84 *names* ("WGS 84", "WGS_1984"). A projected CRS name is
    //    always "<base> / <projection>", so a `/` disqualifies this branch —
    //    "WGS 84 / UTM zone 33N" is meters, not degrees.
    if is_bare_wgs84_name(&up) {
        return CrsKind::Wgs84;
    }
    CrsKind::Unknown
}

/// Every spelling of Web Mercator we accept, tested against an
/// already-uppercased string.
///
/// The ESRI aliases (102100 / 102113) and the deprecated EPSG:3785 are the
/// same projection under different authorities, and ArcGIS-exported data
/// labels itself with them routinely. Refusing them made a plainly Web
/// Mercator file "unsupported" (#519 S4).
fn is_web_mercator_marker(up: &str) -> bool {
    up.contains("3857")
        || up.contains("900913")
        || up.contains("102100")
        || up.contains("102113")
        || up.contains("3785")
        || up.contains("PSEUDO-MERCATOR")
        || up.contains("WEB MERCATOR")
}

/// A bare WGS-84 *name*, tested against an already-uppercased string: the
/// `WGS 84` family with no projection suffix.
///
/// One rule, used by BOTH the string path and the PROJJSON no-id name
/// fallback. They used to disagree — the PROJJSON side matched a closed list
/// of eight literals, so a realistic `"name": "WGS 84 (G1762)"` (a WGS-84
/// realization, plain lon/lat) was WGS84 to `detect_crs_from_kv` — which
/// re-classifies the name through the lenient string path — and NOT WGS84 to
/// `is_wgs84` / `validate_wgs84` on the very same file (#519).
///
/// The `/` guard is what keeps it honest: a projected CRS name is always
/// "<base> / <projection>", so "WGS 84 / UTM zone 33N" is refused here and
/// falls through to [`CrsKind::Unknown`].
fn is_bare_wgs84_name(up: &str) -> bool {
    (up.contains("WGS 84") || up.contains("WGS84") || up.contains("WGS_1984")) && !up.contains('/')
}

/// Classify a PROJJSON object structurally.
///
/// A present, structured authority id is AUTHORITATIVE (#518): an EPSG code
/// that is not 4326 means the file is not lon/lat, full stop, whatever the
/// `name` says. The name is consulted only when the object carries no id at
/// all, and then only for non-projected types — EPSG:3857's PROJJSON is
/// literally `{"type":"ProjectedCRS","name":"WGS 84 / Pseudo-Mercator",…}`,
/// and the old name fallback declared exactly that file WGS84.
pub(crate) fn classify_crs_projjson(projjson: &Value) -> CrsKind {
    let name = projjson.get("name").and_then(Value::as_str);
    let is_projected = projjson
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|t| t.eq_ignore_ascii_case("ProjectedCRS"));

    if let Some(id) = projjson.get("id") {
        let authority = id.get("authority").and_then(Value::as_str);
        let code_i64 = id.get("code").and_then(Value::as_i64);
        let code_str = id.get("code").and_then(Value::as_str);
        // A string code ("4326") is as authoritative as a numeric one.
        let code = code_i64.or_else(|| code_str.and_then(|s| s.parse::<i64>().ok()));

        if authority.is_some_and(|a| a.eq_ignore_ascii_case("EPSG")) {
            return match code {
                Some(4326) => CrsKind::Wgs84,
                // 900913 (Google) and 3785 (deprecated) are the same
                // projection as 3857 under other codes.
                Some(3857 | 900913 | 3785) => CrsKind::WebMercator,
                // Present and not one of ours: authoritative NO. Never fall
                // through to the name.
                Some(_) => CrsKind::Unknown,
                None => CrsKind::Unknown,
            };
        }
        if authority.is_some_and(|a| a.eq_ignore_ascii_case("OGC")) {
            return match code_str {
                Some(c) if c.eq_ignore_ascii_case("CRS84") => CrsKind::Wgs84,
                _ => CrsKind::Unknown,
            };
        }
        // ESRI's own codes for Web Mercator, which ArcGIS exports carry.
        if authority.is_some_and(|a| a.eq_ignore_ascii_case("ESRI")) {
            return match code {
                Some(102100 | 102113) => CrsKind::WebMercator,
                _ => CrsKind::Unknown,
            };
        }
        // Some other authority with a real id: not classifiable, and the id
        // still outranks the name.
        if authority.is_some() {
            return CrsKind::Unknown;
        }
    }

    // No usable id at all — fall back to the well-known names.
    let Some(name) = name else {
        return CrsKind::Unknown;
    };
    let up = name.trim().to_uppercase();
    if is_web_mercator_marker(&up) {
        return CrsKind::WebMercator;
    }
    if is_projected {
        return CrsKind::Unknown;
    }
    // Same rule as the string path ([`is_bare_wgs84_name`]), NOT a closed
    // list of literals: the list refused realistic realization names like
    // "WGS 84 (G1762)" that `detect_crs_from_kv` accepted through the string
    // path, so one file could be WGS84 to the converter and not-WGS84 to
    // `validate_wgs84` (#519). The mercator and `is_projected` guards above
    // are what make the looser test safe here.
    // (Every literal the old list held — including ESRI's "GCS_WGS_1984" and
    // "WGS 84 (GEOGRAPHIC 3D)" — contains one of the three substrings and no
    // `/`, so nothing that used to be accepted is lost.)
    if is_bare_wgs84_name(&up) {
        return CrsKind::Wgs84;
    }
    CrsKind::Unknown
}

/// Classify an already-extracted [`CrsInfo`] — the one answer both the
/// convert side ([`crate::overview::convert::detect_crs_from_kv`]) and the
/// export side (`overview::export::detect_crs`) ask for, so the two can never
/// disagree about one file (#519: export kept a fourth hand-rolled
/// `contains("3857")` matcher that was blind to PROJJSON).
///
/// `is_wgs84` is already classifier-derived upstream — structurally for
/// PROJJSON, via [`classify_crs_identifier`] for a string — so it is
/// honoured first; otherwise the identifier is re-classified. Callers decide
/// for themselves what an [`CrsKind::Unknown`] with nothing declared at all
/// should mean.
pub(crate) fn classify_crs_info(info: &CrsInfo) -> CrsKind {
    if info.is_wgs84 {
        return CrsKind::Wgs84;
    }
    info.identifier
        .as_deref()
        .map(classify_crs_identifier)
        .unwrap_or(CrsKind::Unknown)
}

/// Check if a CRS identifier represents WGS84 or compatible CRS.
fn is_wgs84_identifier(id: &str) -> bool {
    classify_crs_identifier(id) == CrsKind::Wgs84
}

/// Check if PROJJSON represents WGS84.
fn is_wgs84_projjson(projjson: &Value) -> bool {
    classify_crs_projjson(projjson) == CrsKind::Wgs84
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

/// One-shot gates for the "assuming WGS84 / unknown CRS" warnings below.
///
/// [`crs_info_from_kv_metadata`] runs once PER PART of a multi-file input
/// (`MultiSource::from_sources` detects the CRS of every part, plus a raw
/// descriptor read for the mismatch message), so an 800-part glob whose parts
/// all share one metadata quirk used to print 800 identical `warn!` lines
/// (#429 review). Each distinct warning fires once per process instead —
/// which for a single-part input is exactly the old behavior.
mod warn_once {
    use std::sync::Once;

    /// Fire `f` the first time this gate is reached.
    pub(super) fn gate(once: &'static Once, f: impl FnOnce()) {
        once.call_once(f);
    }

    pub(super) static NO_KV_METADATA: Once = Once::new();
    pub(super) static NO_GEO_METADATA: Once = Once::new();
    pub(super) static NO_COLUMNS: Once = Once::new();
    pub(super) static MISSING_COLUMN: Once = Once::new();
    pub(super) static NULL_CRS_DEGREES: Once = Once::new();
    pub(super) static NULL_CRS_UNKNOWN: Once = Once::new();
    pub(super) static UNEXPECTED_CRS_FORMAT: Once = Once::new();
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
        warn_once::gate(&warn_once::NO_KV_METADATA, || {
            log::warn!("GeoParquet file has no key-value metadata; assuming WGS84")
        });
        return Ok(CrsInfo::wgs84());
    };

    let geo_value = kv_metadata
        .iter()
        .find(|kv| kv.key.to_lowercase() == "geo")
        .and_then(|kv| kv.value.as_ref());

    let Some(geo_json_str) = geo_value else {
        // No geo metadata - assume WGS84 with warning
        warn_once::gate(&warn_once::NO_GEO_METADATA, || {
            log::warn!("GeoParquet file has no 'geo' metadata; assuming WGS84")
        });
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
        warn_once::gate(&warn_once::NO_COLUMNS, || {
            log::warn!("GeoParquet 'geo' metadata has no 'columns'; assuming WGS84")
        });
        return Ok(CrsInfo::wgs84());
    };

    // Get the primary column's metadata
    let Some(column_meta) = columns.get(primary_column) else {
        warn_once::gate(&warn_once::MISSING_COLUMN, || {
            log::warn!(
                "GeoParquet 'geo' metadata missing column '{}'; assuming WGS84",
                primary_column
            )
        });
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
            // Explicit null means "no CRS assigned" (spec-distinct from an
            // omitted key = OGC:CRS84), but real-world writers (Spark/Sedona,
            // e.g. the FTW predictions collection) emit it on lon/lat data.
            // Assume CRS84 unless the declared bbox proves the coordinates
            // can't be degrees.
            let bbox_plausible_degrees = column_meta
                .get("bbox")
                .and_then(Value::as_array)
                .filter(|b| b.len() >= 4)
                .map(|b| {
                    let v: Vec<f64> = b.iter().filter_map(Value::as_f64).collect();
                    v.len() >= 4 && v[0] >= -180.0 && v[2] <= 180.0 && v[1] >= -90.0 && v[3] <= 90.0
                })
                // No (or malformed) bbox: nothing to check against — match
                // the missing-geo-metadata precedent and assume.
                .unwrap_or(true);
            if bbox_plausible_degrees {
                warn_once::gate(&warn_once::NULL_CRS_DEGREES, || {
                    log::warn!(
                        "GeoParquet 'crs' is explicitly null (no CRS assigned); \
                         assuming OGC:CRS84 (lon/lat WGS84)"
                    )
                });
                Ok(CrsInfo::wgs84())
            } else {
                warn_once::gate(&warn_once::NULL_CRS_UNKNOWN, || {
                    log::warn!(
                        "GeoParquet 'crs' is explicitly null and the declared bbox \
                         is outside lon/lat degree ranges; treating CRS as unknown"
                    )
                });
                Ok(CrsInfo::unknown())
            }
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
            warn_once::gate(&warn_once::UNEXPECTED_CRS_FORMAT, || {
                log::warn!("Unexpected CRS format in GeoParquet metadata: {:?}", other)
            });
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
         tylertoo requires WGS84 (EPSG:4326) coordinates.\n\n\
         Reproject with geoparquet-io:\n  \
         gpio convert reproject {} reprojected.parquet -d EPSG:4326",
        crs_desc, filename
    )))
}
#[cfg(test)]
mod tests {
    use super::*;

    fn kv_with_geo(geo: &Value) -> Vec<parquet::file::metadata::KeyValue> {
        vec![parquet::file::metadata::KeyValue::new(
            "geo".to_string(),
            geo.to_string(),
        )]
    }

    #[test]
    fn test_crs_null_with_lonlat_bbox_assumes_crs84() {
        // Spark/Sedona writers emit "crs": null on lon/lat data (e.g. the FTW
        // predictions collection). Explicit null means "no CRS assigned"
        // (unlike an omitted key = OGC:CRS84), so assume CRS84 only when the
        // declared bbox is plausible in degrees.
        let geo = serde_json::json!({
            "version": "1.1.0",
            "primary_column": "geometry",
            "columns": {"geometry": {
                "encoding": "WKB",
                "geometry_types": ["Polygon"],
                "bbox": [-179.99, -56.93, -61.52, 0.004],
                "crs": null
            }}
        });
        let info = crs_info_from_kv_metadata(Some(&kv_with_geo(&geo))).unwrap();
        assert!(info.is_wgs84, "null CRS with degree-range bbox → CRS84");
    }

    #[test]
    fn test_crs_null_with_projected_bbox_stays_unknown() {
        let geo = serde_json::json!({
            "version": "1.1.0",
            "primary_column": "geometry",
            "columns": {"geometry": {
                "encoding": "WKB",
                "bbox": [366882.0, 5237430.0, 973200.0, 6100100.0],
                "crs": null
            }}
        });
        let info = crs_info_from_kv_metadata(Some(&kv_with_geo(&geo))).unwrap();
        assert!(
            !info.is_wgs84,
            "null CRS with out-of-degree-range bbox must stay unknown"
        );
    }

    #[test]
    fn test_crs_null_without_bbox_assumes_crs84() {
        // No bbox to check against — match the file-without-geo-metadata
        // precedent (assume WGS84 with a warning).
        let geo = serde_json::json!({
            "version": "1.1.0",
            "primary_column": "geometry",
            "columns": {"geometry": {"encoding": "WKB", "crs": null}}
        });
        let info = crs_info_from_kv_metadata(Some(&kv_with_geo(&geo))).unwrap();
        assert!(info.is_wgs84);
    }

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

    /// #518 (3): a structured EPSG id that is present and ≠ 4326 is
    /// authoritative. The old code fell through to `name.contains("WGS 84")`,
    /// which is true for EPSG:3857 ("WGS 84 / Pseudo-Mercator") and for every
    /// UTM zone ("WGS 84 / UTM zone 33N") — so a correctly declared 3857 file
    /// was detected as EPSG:4326 and its meters were read as degrees.
    #[test]
    fn projjson_structured_id_outranks_a_wgs84_name() {
        let pseudo_mercator: Value = serde_json::json!({
            "type": "ProjectedCRS",
            "name": "WGS 84 / Pseudo-Mercator",
            "id": { "authority": "EPSG", "code": 3857 }
        });
        assert!(
            !is_wgs84_projjson(&pseudo_mercator),
            "EPSG:3857 is not WGS84 however its name reads"
        );
        assert_eq!(
            classify_crs_projjson(&pseudo_mercator),
            CrsKind::WebMercator
        );

        let utm: Value = serde_json::json!({
            "type": "ProjectedCRS",
            "name": "WGS 84 / UTM zone 33N",
            "id": { "authority": "EPSG", "code": 32633 }
        });
        assert!(!is_wgs84_projjson(&utm));
        assert_eq!(classify_crs_projjson(&utm), CrsKind::Unknown);

        // String-typed codes are just as authoritative as numeric ones.
        let utm_str_code: Value = serde_json::json!({
            "type": "ProjectedCRS",
            "name": "WGS 84 / UTM zone 33N",
            "id": { "authority": "EPSG", "code": "32633" }
        });
        assert!(!is_wgs84_projjson(&utm_str_code));
    }

    /// The name fallback is PRESERVED where it is the only evidence: a
    /// PROJJSON object with no `id` at all.
    #[test]
    fn projjson_name_fallback_survives_when_there_is_no_id() {
        let no_id: Value = serde_json::json!({
            "type": "GeographicCRS",
            "name": "WGS 84"
        });
        assert!(is_wgs84_projjson(&no_id));
        assert_eq!(classify_crs_projjson(&no_id), CrsKind::Wgs84);

        // …but a projected name is still not lon/lat.
        let no_id_projected: Value = serde_json::json!({
            "type": "ProjectedCRS",
            "name": "WGS 84 / Pseudo-Mercator"
        });
        assert!(!is_wgs84_projjson(&no_id_projected));
        assert_eq!(
            classify_crs_projjson(&no_id_projected),
            CrsKind::WebMercator
        );
    }

    /// #518 (2): Web Mercator markers are tested BEFORE the WGS-84
    /// substrings, and PROJJSON-shaped strings are parsed rather than
    /// substring-matched.
    #[test]
    fn classify_identifier_orders_webmercator_before_wgs84() {
        assert_eq!(classify_crs_identifier("EPSG:3857"), CrsKind::WebMercator);
        assert_eq!(classify_crs_identifier("EPSG:900913"), CrsKind::WebMercator);
        assert_eq!(
            classify_crs_identifier("WGS 84 / Pseudo-Mercator"),
            CrsKind::WebMercator
        );
        assert_eq!(
            classify_crs_identifier("WGS 84 / UTM zone 33N"),
            CrsKind::Unknown,
            "a projected WGS84-based name is not lon/lat"
        );

        // Inline PROJJSON (what arrow-rs and GeoPandas put in the Parquet
        // `LogicalType::Geometry(crs)` annotation).
        let projjson_3857 = r#"{"type":"ProjectedCRS","name":"WGS 84 / Pseudo-Mercator",
            "base_crs":{"name":"WGS 84"},"id":{"authority":"EPSG","code":3857}}"#;
        assert_eq!(
            classify_crs_identifier(projjson_3857),
            CrsKind::WebMercator,
            "3857 PROJJSON must never classify as lon/lat"
        );
        let projjson_4326 =
            r#"{"type":"GeographicCRS","name":"WGS 84","id":{"authority":"EPSG","code":4326}}"#;
        assert_eq!(classify_crs_identifier(projjson_4326), CrsKind::Wgs84);

        // Unparsable PROJJSON-shaped input: refuse, never guess.
        assert_eq!(
            classify_crs_identifier(r#"{"type":"GeographicCRS","name":"WGS 84""#),
            CrsKind::Unknown
        );
    }

    /// #519: the PROJJSON no-id name fallback and the string path must apply
    /// the SAME rule. The fallback used to be a closed list of eight
    /// literals, so a realistic WGS-84 realization name was lon/lat to
    /// `detect_crs_from_kv` (which re-classifies the name through the string
    /// path) and NOT lon/lat to `is_wgs84` / `validate_wgs84` — one file,
    /// two answers.
    #[test]
    fn projjson_name_fallback_matches_the_string_path() {
        for name in [
            "WGS 84 (G1762)",
            "WGS 84 (G2139)",
            "WGS 84 (Transit)",
            "WGS 84",
            "WGS84",
            "WGS_1984",
            "GCS_WGS_1984",
            "WGS 84 (CRS84)",
            "WGS 84 (Geographic 3D)",
        ] {
            let no_id: Value = serde_json::json!({ "type": "GeographicCRS", "name": name });
            assert_eq!(
                classify_crs_projjson(&no_id),
                CrsKind::Wgs84,
                "PROJJSON name {name:?} must classify as lon/lat"
            );
            assert!(
                is_wgs84_projjson(&no_id),
                "is_wgs84 must agree for {name:?}"
            );
            assert_eq!(
                classify_crs_identifier(name),
                CrsKind::Wgs84,
                "the string path must give the same answer for {name:?}"
            );
        }

        // …and a projected WGS-84-based name is still refused by BOTH.
        for name in ["WGS 84 / UTM zone 33N", "WGS 84 / World Mercator"] {
            let no_id: Value = serde_json::json!({ "type": "GeographicCRS", "name": name });
            assert_eq!(classify_crs_projjson(&no_id), CrsKind::Unknown, "{name:?}");
            assert!(!is_wgs84_projjson(&no_id), "{name:?}");
            assert_eq!(classify_crs_identifier(name), CrsKind::Unknown, "{name:?}");
        }
    }

    /// #519 (S4): ESRI's Web Mercator codes and the deprecated EPSG:3785 are
    /// the same projection as EPSG:3857. Refusing them made plainly Web
    /// Mercator ArcGIS exports "unsupported".
    #[test]
    fn web_mercator_aliases_are_recognized() {
        for id in [
            "ESRI:102100",
            "ESRI:102113",
            "EPSG:3785",
            "epsg:3785",
            "urn:ogc:def:crs:EPSG::3785",
        ] {
            assert_eq!(
                classify_crs_identifier(id),
                CrsKind::WebMercator,
                "{id:?} is Web Mercator"
            );
        }
        for (authority, code) in [("ESRI", 102100), ("ESRI", 102113), ("EPSG", 3785)] {
            let v: Value = serde_json::json!({
                "type": "ProjectedCRS",
                "name": "WGS 84 / Pseudo-Mercator",
                "id": { "authority": authority, "code": code }
            });
            assert_eq!(
                classify_crs_projjson(&v),
                CrsKind::WebMercator,
                "{authority}:{code}"
            );
        }
        // An unrelated ESRI code is still not classifiable.
        let other: Value = serde_json::json!({
            "type": "ProjectedCRS",
            "name": "NAD 1983 StatePlane",
            "id": { "authority": "ESRI", "code": 102645 }
        });
        assert_eq!(classify_crs_projjson(&other), CrsKind::Unknown);
    }

    /// #518 (4): arrow-rs writes an unset Parquet `Geometry` CRS as the
    /// literal "srid:0"; per the GeoParquet/Parquet specs an unset CRS means
    /// OGC:CRS84.
    #[test]
    fn classify_identifier_reads_unset_crs_as_crs84() {
        assert_eq!(classify_crs_identifier("srid:0"), CrsKind::Wgs84);
        assert_eq!(classify_crs_identifier("SRID:0"), CrsKind::Wgs84);
        assert_eq!(classify_crs_identifier(""), CrsKind::Wgs84);
        assert_eq!(classify_crs_identifier("   "), CrsKind::Wgs84);
        // A real SRID is not the unset marker.
        assert_eq!(classify_crs_identifier("srid:3857"), CrsKind::WebMercator);
        assert_eq!(classify_crs_identifier("srid:4326"), CrsKind::Wgs84);
        assert_eq!(classify_crs_identifier("srid:27700"), CrsKind::Unknown);
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
