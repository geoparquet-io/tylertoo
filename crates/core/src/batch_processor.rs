//! Arrow-native geometry batch decoding.
//!
//! Decodes GeoArrow geometry arrays into `geo::Geometry` values within the
//! Arrow `RecordBatch` lifetime, plus parquet file/directory resolution.
//! The overview pipeline ([`crate::overview`]) drives these helpers from its
//! own streaming readers; DO NOT accumulate whole files into
//! `Vec<Geometry>` — decode per batch and process immediately.

use std::path::{Path, PathBuf};

use geo::Geometry;
use geo_traits::to_geo::ToGeoGeometry;
use geoarrow::datatypes::GeoArrowType;
use geoarrow_array::cast::AsGeoArrowArray;
use geoarrow_array::{GeoArrowArray, GeoArrowArrayAccessor};

use crate::{Error, Result};

/// Resolve a path to a list of parquet files.
///
/// If the path is a file, returns it as a single-element vector.
/// If the path is a directory, recursively collects all .parquet files.
pub fn resolve_parquet_files(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }

    if path.is_dir() {
        let mut files = Vec::new();
        collect_parquet_files(path, &mut files)?;
        files.sort(); // Deterministic order
        if files.is_empty() {
            return Err(Error::GeoParquetRead(format!(
                "No .parquet files found in directory: {}",
                path.display()
            )));
        }
        return Ok(files);
    }

    Err(Error::GeoParquetRead(format!(
        "Path does not exist: {}",
        path.display()
    )))
}

/// Recursively collect .parquet files from a directory.
fn collect_parquet_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        Error::GeoParquetRead(format!("Failed to read directory {}: {}", dir.display(), e))
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| Error::GeoParquetRead(e.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_parquet_files(&path, files)?;
        } else if path.extension().is_some_and(|ext| ext == "parquet") {
            files.push(path);
        }
    }
    Ok(())
}

/// Extract geometries from a GeoArrow array into a Vec.
///
/// Null slots and slots that cannot convert to a `geo::Geometry` are
/// **silently skipped**, so `output` may end up shorter than the array.
/// Callers that must keep row indices aligned with other columns should use
/// [`extract_geometries_opt_from_array`] instead.
pub fn extract_geometries_from_array(
    array: &dyn GeoArrowArray,
    output: &mut Vec<Geometry<f64>>,
) -> Result<()> {
    let mut opts: Vec<Option<Geometry<f64>>> = Vec::with_capacity(array.len());
    extract_geometries_opt_from_array(array, &mut opts)?;
    output.extend(opts.into_iter().flatten());
    Ok(())
}

/// Extract geometries from a GeoArrow array into a **row-aligned** Vec of
/// `Option`s: exactly one entry per array slot, `None` for null slots (and
/// for the rare slot that decodes but has no `geo::Geometry` conversion).
///
/// Structurally invalid geometry payloads (e.g. an empty or corrupt WKB
/// value) still return a hard [`Error::GeoParquetRead`]; only *absent*
/// geometry maps to `None`.
pub fn extract_geometries_opt_from_array(
    array: &dyn GeoArrowArray,
    output: &mut Vec<Option<Geometry<f64>>>,
) -> Result<()> {
    match array.data_type() {
        GeoArrowType::Point(_) => {
            let arr = array.as_point();
            extract_typed_array(arr, output)
        }
        GeoArrowType::LineString(_) => {
            let arr = array.as_line_string();
            extract_typed_array(arr, output)
        }
        GeoArrowType::Polygon(_) => {
            let arr = array.as_polygon();
            extract_typed_array(arr, output)
        }
        GeoArrowType::MultiPoint(_) => {
            let arr = array.as_multi_point();
            extract_typed_array(arr, output)
        }
        GeoArrowType::MultiLineString(_) => {
            let arr = array.as_multi_line_string();
            extract_typed_array(arr, output)
        }
        GeoArrowType::MultiPolygon(_) => {
            let arr = array.as_multi_polygon();
            extract_typed_array(arr, output)
        }
        GeoArrowType::Geometry(_) => {
            let arr = array.as_geometry();
            extract_typed_array(arr, output)
        }
        GeoArrowType::GeometryCollection(_) => {
            let arr = array.as_geometry_collection();
            extract_typed_array(arr, output)
        }
        GeoArrowType::Wkb(_) => {
            let arr = array.as_wkb::<i32>();
            extract_typed_array(arr, output)
        }
        GeoArrowType::LargeWkb(_) => {
            let arr = array.as_wkb::<i64>();
            extract_typed_array(arr, output)
        }
        GeoArrowType::WkbView(_) => {
            let arr = array.as_wkb_view();
            extract_typed_array(arr, output)
        }
        GeoArrowType::Wkt(_) => {
            let arr = array.as_wkt::<i32>();
            extract_typed_array(arr, output)
        }
        GeoArrowType::LargeWkt(_) => {
            let arr = array.as_wkt::<i64>();
            extract_typed_array(arr, output)
        }
        GeoArrowType::WktView(_) => {
            let arr = array.as_wkt_view();
            extract_typed_array(arr, output)
        }
        _ => Err(Error::GeoParquetRead(format!(
            "Unsupported geometry type: {:?}",
            array.data_type()
        ))),
    }
}

/// Extract geometries from a typed GeoArrow array into a row-aligned Vec of
/// `Option`s (one entry per slot; see [`extract_geometries_opt_from_array`]).
fn extract_typed_array<'a, A>(
    accessor: &'a A,
    output: &mut Vec<Option<Geometry<f64>>>,
) -> Result<()>
where
    A: GeoArrowArrayAccessor<'a>,
    A::Item: ToGeoGeometry<f64>,
{
    for (i, item) in accessor.iter().enumerate() {
        match item {
            Some(geom_result) => {
                let geom_trait = geom_result.map_err(|e| {
                    Error::GeoParquetRead(format!("Invalid geometry at index {}: {}", i, e))
                })?;
                output.push(geom_trait.try_to_geometry());
            }
            None => output.push(None),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that resolve_parquet_files handles single files correctly.
    #[test]
    fn test_resolve_parquet_files_single_file() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let files = resolve_parquet_files(fixture).expect("Should resolve file");
        assert_eq!(files.len(), 1, "Should return single file");
        assert_eq!(files[0], fixture, "Should return the input file path");
    }

    /// Test that resolve_parquet_files returns error for non-existent path.
    #[test]
    fn test_resolve_parquet_files_nonexistent() {
        let nonexistent = Path::new("/nonexistent/path.parquet");
        let result = resolve_parquet_files(nonexistent);
        assert!(result.is_err(), "Should error on non-existent path");
    }

    /// Test that resolve_parquet_files returns error for empty directory.
    #[test]
    fn test_resolve_parquet_files_empty_dir() {
        let temp_dir = tempfile::tempdir().expect("Should create temp dir");

        let result = resolve_parquet_files(temp_dir.path());
        assert!(
            result.is_err(),
            "Should error on directory with no parquet files"
        );
    }

    /// Test that resolve_parquet_files recurses into subdirectories and
    /// returns a deterministic (sorted) order.
    #[test]
    fn test_resolve_parquet_files_directory_recursive() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let temp_dir = tempfile::tempdir().expect("Should create temp dir");
        let sub = temp_dir.path().join("sub");
        std::fs::create_dir(&sub).expect("Should create subdir");
        std::fs::copy(fixture, temp_dir.path().join("b.parquet")).unwrap();
        std::fs::copy(fixture, sub.join("a.parquet")).unwrap();
        // Non-parquet files are ignored.
        std::fs::write(temp_dir.path().join("readme.txt"), "hi").unwrap();

        let files = resolve_parquet_files(temp_dir.path()).expect("Should resolve dir");
        assert_eq!(files.len(), 2, "Should find both parquet files");
        assert!(files.windows(2).all(|w| w[0] <= w[1]), "Should be sorted");
    }
}
