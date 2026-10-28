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

use crate::wkb_column::check_wkb_column;
use crate::{Error, Result};

/// Resolve a path to a list of parquet files.
///
/// If the path is a file, returns it as a single-element vector.
/// If the path is a directory, recursively collects all .parquet files
/// (sorted; `.`/`_`-prefixed basenames such as `_SUCCESS` are skipped —
/// the collection lives in `crate::input_set::list_parquet_files`, shared
/// with the multi-partition [`crate::input_set::ConvertSource`]).
///
/// # Errors
///
/// Returns [`Error::GeoParquetRead`] if `path` does not exist, if a directory
/// cannot be listed, or if a directory holds no `.parquet` files.
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
///
/// # Errors
///
/// Returns [`Error::GeoParquetRead`] if a geometry payload is structurally
/// invalid.
pub fn extract_geometries_from_array(
    array: &dyn GeoArrowArray,
    output: &mut Vec<Geometry<f64>>,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<()> {
    let mut opts: Vec<Option<Geometry<f64>>> = Vec::with_capacity(array.len());
    extract_geometries_opt_from_array(array, &mut opts, diagnostics)?;
    output.extend(opts.into_iter().flatten());
    Ok(())
}

/// Extract geometries from a GeoArrow array into a **row-aligned** Vec of `Option`s.
///
/// The output holds exactly one entry per array slot: `None` for null slots
/// (and for the rare slot that decodes but has no `geo::Geometry` conversion).
///
/// Structurally invalid geometry payloads (e.g. an empty or corrupt WKB
/// value) still return a hard [`Error::GeoParquetRead`]; only *absent*
/// geometry maps to `None`.
///
/// # Errors
///
/// Returns [`Error::GeoParquetRead`] if a geometry payload is structurally
/// invalid, such as an empty or corrupt WKB value.
pub fn extract_geometries_opt_from_array(
    array: &dyn GeoArrowArray,
    output: &mut Vec<Option<Geometry<f64>>>,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<()> {
    visit_geoarrow_array(
        array,
        ExtractGeometries {
            output,
            diagnostics,
        },
    )
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
            check_wkb_array(wkb)?;
            visitor.visit(wkb)
        }
        GeoArrowType::LargeWkb(_) => {
            let wkb = array.as_wkb::<i64>();
            check_wkb_array(wkb)?;
            visitor.visit(wkb)
        }
        GeoArrowType::WkbView(_) => {
            let wkb = array.as_wkb_view();
            check_wkb_array(wkb)?;
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

/// Check every non-null value of a WKB array before the `wkb` crate's reader
/// sees any of them: the reader allocates from unchecked counts and recurses
/// without a depth limit, so a single crafted value would otherwise abort the
/// process (see [`crate::wkb_column`]).
fn check_wkb_array(wkb: &dyn GeoArrowArray) -> Result<()> {
    check_wkb_column(wkb.to_array_ref().as_ref())
        .map_err(|(i, e)| Error::GeoParquetRead(format!("Invalid geometry at index {i}: {e}")))
}

/// Fuzzing hook: decode one WKB value the way a GeoParquet WKB column is
/// decoded ([`visit_geoarrow_array`] over a one-row `geoarrow.wkb` array).
///
/// It also checks [`crate::wkb_column::check_wkb_value`] against the `wkb`
/// crate's reader it guards. When the walk accepts a value, the reader must
/// accept it too and stop at the same byte; when the walk finds it malformed,
/// the reader must reject it too. Either mismatch panics, so the fuzzer
/// reports it. The reader is not called on the values the walk rejects for a
/// count, the nesting cap or a MultiPoint member overrun: those are the
/// inputs that abort it.
///
/// On a value the reader decodes, it also checks pass 1's in-place scan
/// ([`crate::overview::convert::scan_feature_traits`], #650) against
/// decode-then-scan, to the bit, and panics if they differ.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_geoparquet_wkb(bytes: &[u8]) -> Result<Option<Geometry<f64>>> {
    let diagnostics = &crate::diagnostics::Diagnostics::default();

    use crate::wkb_column::{check_wkb_value, WkbCheckError};
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
    extract_geometries_opt_from_array(&array, &mut out, diagnostics)?;
    let decoded = out.pop().flatten();
    // #650: pass 1 scans the value in place instead of decoding it. The two
    // must agree, to the bit, on every value the decoder accepts.
    let wkb = array.value(0).expect("the value just decoded");
    let in_place = crate::overview::convert::scan_feature_traits(&wkb);
    let via_decode = decoded
        .as_ref()
        .and_then(crate::overview::convert::scan_feature);
    assert_eq!(
        in_place.map(|(k, b)| (k, b.map(f64::to_bits))),
        via_decode.map(|(k, b)| (k, b.map(f64::to_bits))),
        "the in-place pass-1 scan disagrees with decode-then-scan"
    );
    Ok(decoded)
}

/// `geometry`, or an empty geometry collection when any coordinate is NaN or
/// infinite (#707): counted, and warned about once per run.
///
/// This is the ingest boundary every decoded geometry crosses (the
/// in-memory convert, pass 2, the export's reads of an overview file), so a
/// hostile coordinate is caught once per feature here instead of once per
/// feature per tile per zoom at the clipper, and cannot take a fast path
/// that skips the clipper's guard. Empty rather than dropped, so the output
/// stays row-aligned with the batch's other columns: an empty geometry has
/// no bounding box, so it lands in no tile, and the convert's usable-geometry
/// filter skips it exactly as it skipped the non-finite original.
fn finite_or_empty(
    geometry: Geometry<f64>,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Geometry<f64> {
    if geometry_is_finite(&geometry) {
        return geometry;
    }
    diagnostics.record(crate::diagnostics::Diagnostic::NonFiniteGeometry { count: 1 });
    Geometry::GeometryCollection(geo::GeometryCollection::default())
}

fn coord_is_finite(coord: geo::Coord<f64>) -> bool {
    // Shifting discards the sign. Finite IEEE-754 magnitudes sort before
    // infinity; every NaN payload sorts after it. Integer comparisons avoid
    // the larger floating-point class expansion on targets such as ARM.
    const INFINITY: u64 = f64::INFINITY.to_bits() << 1;
    (coord.x.to_bits().wrapping_shl(1) < INFINITY) & (coord.y.to_bits().wrapping_shl(1) < INFINITY)
}

/// Resolve the geometry once instead of dispatching through an enum for
/// every coordinate. Typed ring folds allow vectorization of the #707
/// predicate; collections recurse without allocating boxed iterators.
fn geometry_is_finite(geometry: &Geometry<f64>) -> bool {
    fn finite(geometry: &impl geo::CoordsIter<Scalar = f64>) -> bool {
        geometry
            .coords_iter()
            .fold(true, |finite, c| finite & coord_is_finite(c))
    }

    fn polygon_is_finite(polygon: &geo::Polygon<f64>) -> bool {
        finite(polygon.exterior()) && polygon.interiors().iter().all(finite)
    }

    match geometry {
        Geometry::Point(g) => finite(g),
        Geometry::Line(g) => finite(g),
        Geometry::LineString(g) => finite(g),
        Geometry::Polygon(g) => polygon_is_finite(g),
        Geometry::MultiPoint(g) => finite(g),
        Geometry::MultiLineString(g) => g.0.iter().all(finite),
        Geometry::MultiPolygon(g) => g.0.iter().all(polygon_is_finite),
        Geometry::GeometryCollection(g) => g.0.iter().all(geometry_is_finite),
        Geometry::Rect(g) => finite(g),
        Geometry::Triangle(g) => finite(g),
    }
}

/// [`extract_geometries_opt_from_array`]'s visitor: one row-aligned
/// `Option<Geometry>` per slot.
struct ExtractGeometries<'o> {
    output: &'o mut Vec<Option<Geometry<f64>>>,
    diagnostics: &'o crate::diagnostics::Diagnostics,
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
                    self.output.push(
                        geom_trait
                            .try_to_geometry()
                            .map(|g| finite_or_empty(g, self.diagnostics)),
                    );
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

    #[test]
    fn coordinate_finiteness_matches_every_ieee_exponent() {
        for sign in [0, 1u64 << 63] {
            for exponent in 0..2048u64 {
                for fraction in [0, 1, 1u64 << 51, (1u64 << 52) - 1] {
                    let value = f64::from_bits(sign | (exponent << 52) | fraction);
                    for coord in [
                        geo::Coord { x: value, y: 0. },
                        geo::Coord { x: 0., y: value },
                        geo::Coord { x: value, y: value },
                    ] {
                        assert_eq!(coord_is_finite(coord), value.is_finite(), "{coord:?}");
                    }
                }
            }
        }
    }

    /// Dispatching outside the coordinate loop must preserve the #707
    /// predicate, including late coordinates, holes and nested members.
    #[test]
    fn typed_finiteness_matches_every_geometry_variant() {
        use geo::{CoordsIter, MapCoords};
        use std::cell::Cell;

        let line = geo::LineString::from(vec![(0., 0.), (3., 1.), (2., 4.), (0., 0.)]);
        let polygon = geo::Polygon::new(line.clone(), vec![line.clone()]);
        let variants: Vec<Geometry<f64>> = vec![
            geo::Point::new(1., 2.).into(),
            geo::Line::new((0., 0.), (1., 2.)).into(),
            line.clone().into(),
            geo::LineString::from((0..65).map(|i| (f64::from(i), 0.)).collect::<Vec<_>>()).into(),
            polygon.clone().into(),
            geo::MultiPoint::from(vec![(0., 0.), (1., 2.)]).into(),
            geo::MultiLineString::new(vec![line.clone(), line]).into(),
            geo::MultiPolygon::new(vec![polygon.clone(), polygon.clone()]).into(),
            Geometry::GeometryCollection(geo::GeometryCollection::new_from(vec![
                polygon.into(),
                Geometry::GeometryCollection(geo::GeometryCollection::new_from(vec![
                    geo::Point::new(1., 2.).into(),
                ])),
            ])),
            geo::Rect::new((0., 0.), (1., 2.)).into(),
            geo::Triangle::new((0., 0.).into(), (1., 2.).into(), (3., 0.).into()).into(),
            geo::LineString::new(vec![]).into(),
            geo::Polygon::empty().into(),
            Geometry::GeometryCollection(geo::GeometryCollection::default()),
        ];
        for original in variants {
            assert!(geometry_is_finite(&original));
            for index in 0..original.coords_count() {
                for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MAX, -0.] {
                    for x_axis in [false, true] {
                        let current = Cell::new(0);
                        let changed = original.map_coords(|mut coord| {
                            let i = current.get();
                            current.set(i + 1);
                            if i == index {
                                if x_axis {
                                    coord.x = value;
                                } else {
                                    coord.y = value;
                                }
                            }
                            coord
                        });
                        let expected = changed
                            .coords_iter()
                            .all(|c| c.x.is_finite() && c.y.is_finite());
                        assert_eq!(geometry_is_finite(&changed), expected, "{changed:?}");
                    }
                }
            }
        }
    }

    /// #707: a decoded geometry with a NaN or infinite coordinate becomes an
    /// empty geometry at the ingest boundary, counted, and the output stays
    /// row-aligned.
    #[test]
    fn a_non_finite_geometry_is_emptied_and_counted_at_ingest() {
        let diagnostics = crate::diagnostics::Diagnostics::default();

        use arrow_array::BinaryArray;
        use geoarrow::array::WkbArray;
        use geoarrow::datatypes::WkbType;

        let point = |x: f64, y: f64| {
            let mut wkb = vec![1u8, 1, 0, 0, 0];
            wkb.extend_from_slice(&x.to_le_bytes());
            wkb.extend_from_slice(&y.to_le_bytes());
            wkb
        };
        let mut line = vec![1u8, 2, 0, 0, 0, 2, 0, 0, 0];
        for v in [0.0, 0.0, 1.0, f64::INFINITY] {
            line.extend_from_slice(&f64::to_le_bytes(v));
        }
        let rows = [point(1.0, 2.0), line, point(3.0, 4.0)];
        let array = WkbArray::from((
            BinaryArray::from_vec(rows.iter().map(Vec::as_slice).collect()),
            WkbType::default(),
        ));
        let before =
            diagnostics.count(&crate::diagnostics::Diagnostic::NonFiniteGeometry { count: 0 });
        let mut out = Vec::new();
        extract_geometries_opt_from_array(&array, &mut out, &diagnostics).unwrap();
        assert_eq!(out.len(), 3, "row-aligned");
        assert_eq!(
            out[1],
            Some(Geometry::GeometryCollection(
                geo::GeometryCollection::default()
            ))
        );
        assert!(matches!(out[0], Some(Geometry::Point(_))));
        assert!(matches!(out[2], Some(Geometry::Point(_))));
        assert!(
            diagnostics.count(&crate::diagnostics::Diagnostic::NonFiniteGeometry { count: 0 })
                > before,
            "counted"
        );
    }

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
        let err = extract_geometries_opt_from_array(
            array,
            &mut out,
            &crate::diagnostics::Diagnostics::default(),
        )
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
        extract_geometries_opt_from_array(
            &wkb,
            &mut out,
            &crate::diagnostics::Diagnostics::default(),
        )
        .unwrap();
        assert_eq!(
            out,
            vec![Some(Geometry::Point(geo::point!(x: 1.0, y: 2.0))), None]
        );
    }

    /// Test that `resolve_parquet_files` handles single files correctly.
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

    /// Test that `resolve_parquet_files` returns error for non-existent path.
    #[test]
    fn test_resolve_parquet_files_nonexistent() {
        let nonexistent = Path::new("/nonexistent/path.parquet");
        let result = resolve_parquet_files(nonexistent);
        assert!(result.is_err(), "Should error on non-existent path");
    }

    /// Test that `resolve_parquet_files` returns error for empty directory.
    #[test]
    fn test_resolve_parquet_files_empty_dir() {
        let temp_dir = tempfile::tempdir().expect("Should create temp dir");

        let result = resolve_parquet_files(temp_dir.path());
        assert!(
            result.is_err(),
            "Should error on directory with no parquet files"
        );
    }

    /// Test that `resolve_parquet_files` recurses into subdirectories and
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
