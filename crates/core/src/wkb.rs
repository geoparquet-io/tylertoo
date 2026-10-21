//! WKB serialization utilities for temporary file storage.
//!
//! This module provides functions to serialize `geo::Geometry` to Well-Known Binary (WKB)
//! format and back. This is used for streaming pipelines that need to spill features
//! to disk when memory pressure is high.
//!
//! # Note on Usage
//!
//! This module is intended for **temp file storage only**, not for bulk geometry extraction
//! from GeoParquet files. For reading geometries from Parquet, use GeoArrow's columnar
//! decoding which provides better performance (see `batch_processor.rs`).
//!
//! # Examples
//!
//! ```
//! use geo::{Geometry, Point, point};
//! use tylertoo_core::wkb::{geometry_to_wkb, wkb_to_geometry};
//!
//! let point = Geometry::Point(point!(x: 1.5, y: 2.5));
//! let wkb_bytes = geometry_to_wkb(&point).unwrap();
//! let restored = wkb_to_geometry(&wkb_bytes).unwrap();
//!
//! // Round-trip preserves geometry
//! assert!(matches!(restored, Geometry::Point(_)));
//! ```

use geo::{Geometry, Point};
use geo_traits::to_geo::ToGeoGeometry;
use geo_traits::{GeometryTrait, GeometryType, PointTrait};
use wkb::writer::{write_geometry, WriteOptions};
use wkb::Endianness;

use crate::wkb_column::check_wkb_value;

/// Errors that can occur during WKB serialization/deserialization.
#[derive(Debug, thiserror::Error)]
pub enum WkbError {
    #[error("WKB encode error: {0}")]
    EncodeError(String),

    #[error("WKB decode error: {0}")]
    DecodeError(String),
}

pub type Result<T> = std::result::Result<T, WkbError>;

/// Serialize a geometry to WKB bytes.
///
/// Writes little-endian ISO WKB with XY coordinates (no Z or M dimensions)
/// through the `wkb` crate, the same reader and writer geoarrow uses for
/// GeoParquet columns.
///
/// # Arguments
/// * `geom` - The geometry to serialize
///
/// # Returns
/// WKB-encoded bytes on success, or an error if encoding fails.
///
/// # Example
/// ```
/// use geo::{Geometry, LineString, line_string};
/// use tylertoo_core::wkb::geometry_to_wkb;
///
/// let line = Geometry::LineString(line_string![(x: 0.0, y: 0.0), (x: 1.0, y: 1.0)]);
/// let wkb = geometry_to_wkb(&line).unwrap();
/// assert!(!wkb.is_empty());
/// ```
///
/// # Errors
///
/// Returns `WkbError::EncodeError` if the geometry cannot be encoded,
/// including a polygon with holes but an empty exterior: WKB has no way to
/// say "no exterior ring", and the `wkb` writer would emit a ring count of
/// zero followed by the holes, which no reader can parse back.
pub fn geometry_to_wkb(geom: &Geometry) -> Result<Vec<u8>> {
    if has_holes_without_exterior(geom) {
        return Err(WkbError::EncodeError(
            "a polygon with holes but an empty exterior ring has no WKB encoding".to_string(),
        ));
    }
    let mut out = Vec::new();
    write_geometry(
        &mut out,
        geom,
        &WriteOptions {
            endianness: Endianness::LittleEndian,
        },
    )
    .map_err(|e| WkbError::EncodeError(e.to_string()))?;
    Ok(out)
}

fn has_holes_without_exterior(geom: &Geometry) -> bool {
    let polygon = |p: &geo::Polygon| p.exterior().0.is_empty() && !p.interiors().is_empty();
    match geom {
        Geometry::Polygon(p) => polygon(p),
        Geometry::MultiPolygon(mp) => mp.0.iter().any(polygon),
        Geometry::GeometryCollection(gc) => gc.0.iter().any(has_holes_without_exterior),
        _ => false,
    }
}

/// Deserialize WKB bytes back to a geometry.
///
/// Handles standard OGC WKB format.
///
/// # Arguments
/// * `wkb` - The WKB-encoded bytes
///
/// # Returns
/// The deserialized geometry on success, or an error if decoding fails.
///
/// # Example
/// ```
/// use geo::{Geometry, Point, point};
/// use tylertoo_core::wkb::{geometry_to_wkb, wkb_to_geometry};
///
/// let original = Geometry::Point(point!(x: 42.0, y: -73.5));
/// let wkb = geometry_to_wkb(&original).unwrap();
/// let restored = wkb_to_geometry(&wkb).unwrap();
/// ```
///
/// # Hostile input
///
/// The bytes come from `--plan` files, which are user input. They pass
/// `check_wkb_value` before the `wkb` reader sees them, the same walk every
/// GeoParquet geometry column gets (see the `wkb_column` module): counts the
/// remaining bytes cannot back, GeometryCollections nested more than 100
/// deep, and the MultiPoint member that panics the reader are all rejected
/// with an error. Both paths therefore accept exactly the same WKB.
///
/// # Errors
///
/// Returns `WkbError::DecodeError` if the bytes are not valid WKB, fail the
/// hostile-input checks above, or hold an empty point inside another
/// geometry, which `geo` cannot represent.
pub fn wkb_to_geometry(wkb: &[u8]) -> Result<Geometry> {
    check_wkb_value(wkb).map_err(|e| WkbError::DecodeError(e.to_string()))?;
    let value = wkb::reader::read_wkb(wkb).map_err(|e| WkbError::DecodeError(e.to_string()))?;
    // An empty point (WKB writes it as NaN NaN) is a top-level
    // `Point(NaN, NaN)`, as `geometry_to_wkb` wrote it; geo-traits' conversion
    // has no value for it.
    if let GeometryType::Point(p) = value.as_type() {
        if p.coord().is_none() {
            return Ok(Geometry::Point(Point::new(f64::NAN, f64::NAN)));
        }
    }
    value.try_to_geometry().ok_or_else(|| {
        WkbError::DecodeError("WKB holds an empty point inside another geometry".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{line_string, point, polygon, Coord, LineString, MultiPolygon, Point, Polygon};

    // ========================================================================
    // Geometry Round-Trip Tests
    // ========================================================================

    #[test]
    fn test_point_round_trip() {
        let original = Geometry::Point(point!(x: 1.5, y: 2.5));
        let wkb = geometry_to_wkb(&original).expect("encode should succeed");
        let restored = wkb_to_geometry(&wkb).expect("decode should succeed");

        match restored {
            Geometry::Point(p) => {
                assert!((p.x() - 1.5).abs() < 1e-10);
                assert!((p.y() - 2.5).abs() < 1e-10);
            }
            _ => panic!("Expected Point, got {:?}", restored),
        }
    }

    #[test]
    fn test_linestring_round_trip() {
        let original = Geometry::LineString(line_string![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 2.0, y: 0.0)
        ]);
        let wkb = geometry_to_wkb(&original).expect("encode should succeed");
        let restored = wkb_to_geometry(&wkb).expect("decode should succeed");

        match restored {
            Geometry::LineString(ls) => {
                assert_eq!(ls.0.len(), 3);
                assert!((ls.0[0].x - 0.0).abs() < 1e-10);
                assert!((ls.0[1].x - 1.0).abs() < 1e-10);
                assert!((ls.0[2].x - 2.0).abs() < 1e-10);
            }
            _ => panic!("Expected LineString, got {:?}", restored),
        }
    }

    #[test]
    fn test_polygon_round_trip() {
        let original = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 4.0, y: 0.0),
            (x: 4.0, y: 4.0),
            (x: 0.0, y: 4.0),
            (x: 0.0, y: 0.0)
        ]);
        let wkb = geometry_to_wkb(&original).expect("encode should succeed");
        let restored = wkb_to_geometry(&wkb).expect("decode should succeed");

        match restored {
            Geometry::Polygon(poly) => {
                assert_eq!(poly.exterior().0.len(), 5);
                assert!(poly.interiors().is_empty());
            }
            _ => panic!("Expected Polygon, got {:?}", restored),
        }
    }

    #[test]
    fn test_polygon_with_hole_round_trip() {
        // Exterior ring
        let exterior = LineString::from(vec![
            Coord { x: 0.0, y: 0.0 },
            Coord { x: 10.0, y: 0.0 },
            Coord { x: 10.0, y: 10.0 },
            Coord { x: 0.0, y: 10.0 },
            Coord { x: 0.0, y: 0.0 },
        ]);
        // Interior hole
        let hole = LineString::from(vec![
            Coord { x: 2.0, y: 2.0 },
            Coord { x: 8.0, y: 2.0 },
            Coord { x: 8.0, y: 8.0 },
            Coord { x: 2.0, y: 8.0 },
            Coord { x: 2.0, y: 2.0 },
        ]);
        let original = Geometry::Polygon(Polygon::new(exterior, vec![hole]));

        let wkb = geometry_to_wkb(&original).expect("encode should succeed");
        let restored = wkb_to_geometry(&wkb).expect("decode should succeed");

        match restored {
            Geometry::Polygon(poly) => {
                assert_eq!(poly.exterior().0.len(), 5);
                assert_eq!(poly.interiors().len(), 1);
                assert_eq!(poly.interiors()[0].0.len(), 5);
            }
            _ => panic!("Expected Polygon, got {:?}", restored),
        }
    }

    #[test]
    fn test_multipolygon_round_trip() {
        let poly1 = polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0)
        ];
        let poly2 = polygon![
            (x: 5.0, y: 5.0),
            (x: 6.0, y: 5.0),
            (x: 6.0, y: 6.0),
            (x: 5.0, y: 6.0),
            (x: 5.0, y: 5.0)
        ];
        let original = Geometry::MultiPolygon(MultiPolygon::new(vec![poly1, poly2]));

        let wkb = geometry_to_wkb(&original).expect("encode should succeed");
        let restored = wkb_to_geometry(&wkb).expect("decode should succeed");

        match restored {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2);
            }
            _ => panic!("Expected MultiPolygon, got {:?}", restored),
        }
    }

    #[test]
    fn test_multipoint_round_trip() {
        use geo::MultiPoint;

        let points = vec![
            Point::new(1.0, 2.0),
            Point::new(3.0, 4.0),
            Point::new(5.0, 6.0),
        ];
        let original = Geometry::MultiPoint(MultiPoint::new(points));

        let wkb = geometry_to_wkb(&original).expect("encode should succeed");
        let restored = wkb_to_geometry(&wkb).expect("decode should succeed");

        match restored {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 3);
            }
            _ => panic!("Expected MultiPoint, got {:?}", restored),
        }
    }

    #[test]
    fn test_multilinestring_round_trip() {
        use geo::MultiLineString;

        let lines = vec![
            line_string![(x: 0.0, y: 0.0), (x: 1.0, y: 1.0)],
            line_string![(x: 2.0, y: 2.0), (x: 3.0, y: 3.0)],
        ];
        let original = Geometry::MultiLineString(MultiLineString::new(lines));

        let wkb = geometry_to_wkb(&original).expect("encode should succeed");
        let restored = wkb_to_geometry(&wkb).expect("decode should succeed");

        match restored {
            Geometry::MultiLineString(mls) => {
                assert_eq!(mls.0.len(), 2);
            }
            _ => panic!("Expected MultiLineString, got {:?}", restored),
        }
    }

    // ========================================================================
    // Edge Cases
    // ========================================================================

    #[test]
    fn test_point_at_origin() {
        let original = Geometry::Point(point!(x: 0.0, y: 0.0));
        let wkb = geometry_to_wkb(&original).expect("encode should succeed");
        let restored = wkb_to_geometry(&wkb).expect("decode should succeed");

        match restored {
            Geometry::Point(p) => {
                assert!((p.x() - 0.0).abs() < 1e-10);
                assert!((p.y() - 0.0).abs() < 1e-10);
            }
            _ => panic!("Expected Point"),
        }
    }

    #[test]
    fn test_point_extreme_coordinates() {
        // Test with coordinates at the edge of typical geographic bounds
        let original = Geometry::Point(point!(x: 180.0, y: -90.0));
        let wkb = geometry_to_wkb(&original).expect("encode should succeed");
        let restored = wkb_to_geometry(&wkb).expect("decode should succeed");

        match restored {
            Geometry::Point(p) => {
                assert!((p.x() - 180.0).abs() < 1e-10);
                assert!((p.y() - (-90.0)).abs() < 1e-10);
            }
            _ => panic!("Expected Point"),
        }
    }

    #[test]
    fn test_decode_invalid_wkb() {
        let invalid_bytes = vec![0x00, 0x01, 0x02, 0x03];
        let result = wkb_to_geometry(&invalid_bytes);
        assert!(result.is_err());
        assert!(matches!(result, Err(WkbError::DecodeError(_))));
    }

    #[test]
    fn test_decode_empty_bytes() {
        let result = wkb_to_geometry(&[]);
        assert!(result.is_err());
    }

    // ========================================================================
    // Hostile Input (#623)
    // ========================================================================

    /// The nightly fuzz run's crash input (#623): a MultiPolygon header that
    /// declares 3,338,651,383 polygons in an 11-byte blob. Its byte-order byte
    /// is 10, which the `wkb` reader rejects outright. The same count behind a
    /// plain MultiPolygon header is named by the count check.
    #[test]
    fn test_fuzz_623_huge_multipolygon_count_is_rejected() {
        let bytes = [10, 198, 255, 193, 255, 247, 198, 255, 198, 198, 255];
        let err = wkb_to_geometry(&bytes).expect_err("byte order 10 is not WKB");
        assert!(err.to_string().contains("byte order 10"), "{err}");
        let bytes = with_count(header(6), 3_338_651_383);
        let err = wkb_to_geometry(&bytes).expect_err("a 9-byte blob holds no polygons");
        assert!(
            err.to_string().contains("declares 3338651383 polygons"),
            "the error must name the impossible count: {err}"
        );
    }

    /// Little-endian WKB header: byte order, then `type_id`.
    fn header(type_id: u32) -> Vec<u8> {
        let mut v = vec![1];
        v.extend_from_slice(&type_id.to_le_bytes());
        v
    }

    fn with_count(mut v: Vec<u8>, n: u32) -> Vec<u8> {
        v.extend_from_slice(&n.to_le_bytes());
        v
    }

    fn coords(mut v: Vec<u8>, values: &[f64]) -> Vec<u8> {
        for x in values {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v
    }

    /// Every count in the format is checked, not just the one the fuzzer hit.
    /// Each blob declares `u32::MAX` elements, which would once have reserved
    /// tens of GB. Point counts are read lazily and fail as truncated; counts
    /// the reader reserves for are named.
    #[test]
    fn test_every_count_site_rejects_an_impossible_count() {
        let named = |bytes: &[u8], what: &str| {
            let err = wkb_to_geometry(bytes).expect_err("the count cannot fit");
            assert!(
                err.to_string()
                    .contains(&format!("declares {} {what}", u32::MAX)),
                "expected a {what} count error, got: {err}"
            );
        };
        named(&with_count(header(3), u32::MAX), "rings");
        named(&with_count(header(5), u32::MAX), "linestrings");
        named(&with_count(header(6), u32::MAX), "polygons");
        named(&with_count(header(7), u32::MAX), "geometries");
        let mut member_polygon = with_count(header(6), 1);
        member_polygon.extend(with_count(header(3), u32::MAX));
        named(&member_polygon, "rings");

        let mut member_line = with_count(header(5), 1);
        member_line.extend(with_count(header(2), u32::MAX));
        for bytes in [
            with_count(header(2), u32::MAX),
            with_count(with_count(header(3), 1), u32::MAX),
            with_count(header(4), u32::MAX),
            member_line,
        ] {
            assert!(wkb_to_geometry(&bytes).is_err(), "{bytes:?}");
        }
    }

    /// A count that exactly fills the remaining bytes is valid.
    #[test]
    fn test_count_that_exactly_fits_is_accepted() {
        let line = coords(with_count(header(2), 2), &[0.0, 0.0, 1.0, 1.0]);
        assert!(matches!(
            wkb_to_geometry(&line).expect("two points in 32 bytes"),
            Geometry::LineString(ls) if ls.0.len() == 2
        ));
        // One coordinate short of the declared two.
        let short = coords(with_count(header(2), 2), &[0.0, 0.0, 1.0]);
        assert!(wkb_to_geometry(&short).is_err());
    }

    /// Z, M and ZM coordinates are 24, 24 and 32 bytes; they decode to XY.
    #[test]
    fn test_iso_z_m_and_zm_coordinates_are_sized_correctly() {
        for (type_id, n_values) in [(1001, 3), (2001, 3), (3001, 4)] {
            let values: Vec<f64> = (1..=n_values).map(f64::from).collect();
            let point = coords(header(type_id), &values);
            assert!(
                matches!(
                    wkb_to_geometry(&point),
                    Ok(Geometry::Point(p)) if (p.x(), p.y()) == (1.0, 2.0)
                ),
                "type {type_id}: a complete point must decode"
            );
            let truncated = &point[..point.len() - 1];
            assert!(
                wkb_to_geometry(truncated).is_err(),
                "type {type_id}: one byte short must fail"
            );
        }
        let line = coords(with_count(header(1002), 2), &[0.0; 6]);
        assert!(matches!(
            wkb_to_geometry(&line),
            Ok(Geometry::LineString(ls)) if ls.0.len() == 2
        ));
    }

    #[test]
    fn test_big_endian_input_decodes() {
        let mut bytes = vec![0];
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&3.0f64.to_be_bytes());
        bytes.extend_from_slice(&4.0f64.to_be_bytes());
        match wkb_to_geometry(&bytes).expect("big-endian point") {
            Geometry::Point(p) => assert_eq!((p.x(), p.y()), (3.0, 4.0)),
            other => panic!("Expected Point, got {other:?}"),
        }
    }

    /// A NaN point (how the pipeline carries a missing point) round-trips as
    /// `Point(NaN, NaN)`; the `wkb` reader alone would call it empty.
    #[test]
    fn test_nan_point_round_trips() {
        let nan = Geometry::Point(Point::new(f64::NAN, f64::NAN));
        match wkb_to_geometry(&geometry_to_wkb(&nan).unwrap()).unwrap() {
            Geometry::Point(p) => assert!(p.x().is_nan() && p.y().is_nan()),
            other => panic!("Expected Point, got {other:?}"),
        }
    }

    /// WKB cannot hold holes without an exterior ring; refuse to write bytes
    /// that would not read back.
    #[test]
    fn test_holes_without_exterior_fail_to_encode() {
        let hole = LineString::from(vec![(1.0, 1.0), (2.0, 1.0), (2.0, 2.0), (1.0, 1.0)]);
        let polygon = Polygon::new(LineString::new(vec![]), vec![hole]);
        for geom in [
            Geometry::Polygon(polygon.clone()),
            Geometry::MultiPolygon(MultiPolygon(vec![polygon])),
        ] {
            assert!(matches!(
                geometry_to_wkb(&geom),
                Err(WkbError::EncodeError(_))
            ));
        }
    }

    /// `--plan` WKB and GeoParquet WKB follow one set of rules: the base type
    /// is `code & 7`, as the `wkb` reader reads it.
    #[test]
    fn test_plan_wkb_follows_the_column_rules() {
        // 8 has no base type; 17 (Triangle) reads as a Point, as it does in a
        // GeoParquet column.
        assert!(wkb_to_geometry(&with_count(header(8), 0)).is_err());
        let triangle_code = coords(header(17), &[1.0, 2.0]);
        assert!(check_wkb_value(&triangle_code).is_ok());
        assert!(matches!(
            wkb_to_geometry(&triangle_code),
            Ok(Geometry::Point(_))
        ));
    }

    /// Wrap `inner` in `levels` GeometryCollections of one member each.
    fn nest(inner: Vec<u8>, levels: usize) -> Vec<u8> {
        (0..levels).fold(inner, |acc, _| {
            let mut v = with_count(header(7), 1);
            v.extend(acc);
            v
        })
    }

    #[test]
    fn test_collection_nesting_is_capped() {
        use crate::wkb_column::MAX_NESTING_DEPTH;
        let point = coords(header(1), &[1.0, 2.0]);
        assert!(
            wkb_to_geometry(&nest(point.clone(), MAX_NESTING_DEPTH)).is_ok(),
            "nesting at the cap decodes"
        );
        let err = wkb_to_geometry(&nest(point, MAX_NESTING_DEPTH + 1))
            .expect_err("one level past the cap");
        assert!(err.to_string().contains("deeper than"), "{err}");
    }

    /// Mixed members of a collection, each with its own byte order, still
    /// decode: the walk follows each member's own header.
    #[test]
    fn test_collection_members_use_their_own_byte_order() {
        let mut be_point = vec![0];
        be_point.extend_from_slice(&1u32.to_be_bytes());
        be_point.extend_from_slice(&5.0f64.to_be_bytes());
        be_point.extend_from_slice(&6.0f64.to_be_bytes());
        let mut gc = with_count(header(7), 2);
        gc.extend(be_point);
        gc.extend(coords(with_count(header(2), 2), &[0.0, 0.0, 1.0, 1.0]));
        match wkb_to_geometry(&gc).expect("mixed-endian collection") {
            Geometry::GeometryCollection(c) => assert_eq!(c.0.len(), 2),
            other => panic!("Expected GeometryCollection, got {other:?}"),
        }
    }
}
