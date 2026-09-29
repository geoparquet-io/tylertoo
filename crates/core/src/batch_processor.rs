//! Arrow-native geometry batch decoding.
//!
//! Decodes GeoArrow geometry arrays into `geo::Geometry` values within the
//! Arrow `RecordBatch` lifetime, plus parquet file/directory resolution.
//! The overview pipeline ([`crate::overview`]) drives these helpers from its
//! own streaming readers; DO NOT accumulate whole files into
//! `Vec<Geometry>` — decode per batch and process immediately.

use std::path::{Path, PathBuf};

use arrow_array::cast::AsArray;
use geo::Geometry;
use geo_traits::to_geo::ToGeoGeometry;
use geoarrow::datatypes::GeoArrowType;
use geoarrow_array::cast::AsGeoArrowArray;
use geoarrow_array::{GeoArrowArray, GeoArrowArrayAccessor};

use crate::wkb_column::check_wkb_value;
use crate::{Error, Result};

/// Resolve a path to a list of parquet files.
///
/// If the path is a file, returns it as a single-element vector.
/// If the path is a directory, recursively collects all .parquet files
/// (sorted; `.`/`_`-prefixed basenames such as `_SUCCESS` are skipped —
/// the collection lives in [`crate::input_set::list_parquet_files`], shared
/// with the multi-partition [`crate::input_set::ConvertSource`]).
pub fn resolve_parquet_files(path: &Path) -> Result<Vec<PathBuf>> {
    if path.is_file() {
        return Ok(vec![path.to_path_buf()]);
    }

    if path.is_dir() {
        let files = crate::input_set::list_parquet_files(path).map_err(|e| {
            Error::GeoParquetRead(format!(
                "Failed to read directory {}: {}",
                path.display(),
                e
            ))
        })?;
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
    visit_geoarrow_array(array, ExtractGeometries { output })
}

/// A computation over one typed GeoArrow accessor, run by
/// [`visit_geoarrow_array`] after it has resolved the array's concrete type.
///
/// The dispatch over [`GeoArrowType`] lives in exactly one place; a caller
/// that wants something other than a full `geo::Geometry` conversion (e.g.
/// the `--max-zoom auto` estimator, which reads a bbox for a sample of rows
/// through `geo_traits` without converting anything) implements this trait
/// instead of re-matching every geometry type.
pub(crate) trait GeoArrowVisitor {
    /// What the visit produces.
    type Output;

    /// Run over the typed accessor.
    fn visit<'a, A>(self, accessor: &'a A) -> Result<Self::Output>
    where
        A: GeoArrowArrayAccessor<'a>,
        A::Item: ToGeoGeometry<f64>;
}

/// Resolve `array`'s concrete GeoArrow type and hand its typed accessor to
/// `visitor`. Unsupported types are a [`Error::GeoParquetRead`].
pub(crate) fn visit_geoarrow_array<V: GeoArrowVisitor>(
    array: &dyn GeoArrowArray,
    visitor: V,
) -> Result<V::Output> {
    match array.data_type() {
        GeoArrowType::Point(_) => visitor.visit(array.as_point()),
        GeoArrowType::LineString(_) => visitor.visit(array.as_line_string()),
        GeoArrowType::Polygon(_) => visitor.visit(array.as_polygon()),
        GeoArrowType::MultiPoint(_) => visitor.visit(array.as_multi_point()),
        GeoArrowType::MultiLineString(_) => visitor.visit(array.as_multi_line_string()),
        GeoArrowType::MultiPolygon(_) => visitor.visit(array.as_multi_polygon()),
        GeoArrowType::Geometry(_) => visitor.visit(array.as_geometry()),
        GeoArrowType::GeometryCollection(_) => visitor.visit(array.as_geometry_collection()),
        GeoArrowType::Wkb(_) => {
            let wkb = array.as_wkb::<i32>();
            check_wkb_values(wkb.inner().iter())?;
            visitor.visit(wkb)
        }
        GeoArrowType::LargeWkb(_) => {
            let wkb = array.as_wkb::<i64>();
            check_wkb_values(wkb.inner().iter())?;
            visitor.visit(wkb)
        }
        GeoArrowType::WkbView(_) => {
            let wkb = array.as_wkb_view();
            check_wkb_values(wkb.to_array_ref().as_binary_view().iter())?;
            visitor.visit(wkb)
        }
        GeoArrowType::Wkt(_) => visitor.visit(array.as_wkt::<i32>()),
        GeoArrowType::LargeWkt(_) => visitor.visit(array.as_wkt::<i64>()),
        GeoArrowType::WktView(_) => visitor.visit(array.as_wkt_view()),
        _ => Err(Error::GeoParquetRead(format!(
            "Unsupported geometry type: {:?}",
            array.data_type()
        ))),
    }
}

/// Run [`check_wkb_value`] over every non-null value of a WKB column before
/// the `wkb` crate's reader sees any of them: the reader allocates from
/// unchecked counts and recurses without a depth limit, so a single crafted
/// value would otherwise abort the process (see [`crate::wkb_column`]).
fn check_wkb_values<'a>(values: impl Iterator<Item = Option<&'a [u8]>>) -> Result<()> {
    for (i, value) in values.enumerate() {
        if let Some(bytes) = value {
            check_wkb_value(bytes).map_err(|e| {
                Error::GeoParquetRead(format!("Invalid geometry at index {i}: {e}"))
            })?;
        }
    }
    Ok(())
}

/// Fuzzing hook: decode one WKB value the way a GeoParquet WKB column is
/// decoded ([`visit_geoarrow_array`] over a one-row `geoarrow.wkb` array).
///
/// It also checks [`check_wkb_value`] against the `wkb` crate's reader it
/// guards. When the walk accepts a value, the reader must accept it too and
/// stop at the same byte; when the walk finds it malformed, the reader must
/// reject it too. Either mismatch panics, so the fuzzer reports it. The
/// reader is not called on the values the walk rejects for a count, the
/// nesting cap or a MultiPoint member overrun: those are the inputs that
/// abort it.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_geoparquet_wkb(bytes: &[u8]) -> Result<Option<Geometry<f64>>> {
    use crate::wkb_column::WkbCheckError;
    use arrow_array::BinaryArray;
    use geoarrow::array::WkbArray;
    use geoarrow::datatypes::WkbType;

    let array = WkbArray::from((BinaryArray::from_vec(vec![bytes]), WkbType::default()));
    match check_wkb_value(bytes) {
        Ok(size) => {
            let wkb = array
                .value(0)
                .expect("the walk accepted a value the wkb reader rejects");
            assert_eq!(
                wkb.buf().len() as u64,
                size,
                "the walk and the wkb reader disagree on where the geometry ends"
            );
        }
        Err(WkbCheckError::Malformed(_)) => assert!(
            array.value(0).is_err(),
            "the walk rejected a value the wkb reader accepts"
        ),
        Err(_) => {}
    }
    let mut out = Vec::with_capacity(1);
    extract_geometries_opt_from_array(&array, &mut out)?;
    Ok(out.pop().flatten())
}

/// [`extract_geometries_opt_from_array`]'s visitor: one row-aligned
/// `Option<Geometry>` per slot.
struct ExtractGeometries<'o> {
    output: &'o mut Vec<Option<Geometry<f64>>>,
}

impl GeoArrowVisitor for ExtractGeometries<'_> {
    type Output = ();

    fn visit<'a, A>(self, accessor: &'a A) -> Result<()>
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
                    self.output.push(geom_trait.try_to_geometry());
                }
                None => self.output.push(None),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid point, a null, and #632's 9-byte MultiPolygon that claims
    /// 4,294,967,295 polygons.
    fn wkb_rows() -> Vec<Option<Vec<u8>>> {
        let mut point = vec![1u8, 1, 0, 0, 0];
        point.extend_from_slice(&1.0f64.to_le_bytes());
        point.extend_from_slice(&2.0f64.to_le_bytes());
        vec![
            Some(point),
            None,
            Some(vec![1, 6, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]),
        ]
    }

    fn assert_rejects_row_2(array: &dyn GeoArrowArray) {
        let mut out = Vec::new();
        let err = extract_geometries_opt_from_array(array, &mut out)
            .expect_err("the hostile value must be an error, not an abort");
        assert_eq!(
            err.to_string(),
            "Failed to read GeoParquet file: Invalid geometry at index 2: WKB declares \
             4294967295 polygons at byte 5 but only 0 bytes remain (each needs at least 9)"
        );
    }

    /// #632: every WKB column layout is checked before the `wkb` crate's
    /// reader sees a value, and the error names the row.
    #[test]
    fn hostile_wkb_value_is_an_error_in_every_wkb_layout() {
        use arrow_array::{BinaryArray, BinaryViewArray, LargeBinaryArray};
        use geoarrow::array::{LargeWkbArray, WkbArray, WkbViewArray};
        use geoarrow::datatypes::WkbType;

        let rows = wkb_rows();
        let refs: Vec<Option<&[u8]>> = rows.iter().map(|r| r.as_deref()).collect();

        let wkb = WkbArray::from((BinaryArray::from(refs.clone()), WkbType::default()));
        assert_rejects_row_2(&wkb);
        let large = LargeWkbArray::from((LargeBinaryArray::from(refs.clone()), WkbType::default()));
        assert_rejects_row_2(&large);
        let view = WkbViewArray::from((BinaryViewArray::from(refs), WkbType::default()));
        assert_rejects_row_2(&view);
    }

    /// Without the hostile row, the same column decodes as before: one
    /// geometry per row, `None` for the null.
    #[test]
    fn valid_wkb_column_decodes_row_aligned() {
        use arrow_array::BinaryArray;
        use geoarrow::array::WkbArray;
        use geoarrow::datatypes::WkbType;

        let rows = wkb_rows();
        let refs: Vec<Option<&[u8]>> = rows[..2].iter().map(|r| r.as_deref()).collect();
        let wkb = WkbArray::from((BinaryArray::from(refs), WkbType::default()));
        let mut out = Vec::new();
        extract_geometries_opt_from_array(&wkb, &mut out).unwrap();
        assert_eq!(
            out,
            vec![Some(Geometry::Point(geo::point!(x: 1.0, y: 2.0))), None]
        );
    }

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
