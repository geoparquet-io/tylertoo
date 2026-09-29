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

use geo::Geometry;
use geozero::wkb::Wkb;
use geozero::{CoordDimensions, ToGeo, ToWkb};

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
/// Uses standard OGC WKB format with XY coordinates (no Z or M dimensions).
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
pub fn geometry_to_wkb(geom: &Geometry) -> Result<Vec<u8>> {
    geom.to_wkb(CoordDimensions::xy())
        .map_err(|e| WkbError::EncodeError(e.to_string()))
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
/// The bytes are walked once before geozero sees them, and rejected when any
/// count in them declares more elements than the remaining bytes can hold.
/// geozero reserves space for every declared element up front, so without
/// this walk an 11-byte blob claiming three billion polygons asks the
/// allocator for 160 GB and aborts the process. After the walk, geozero's
/// allocations are bounded by a small multiple of `wkb.len()`.
/// GeometryCollections nested more than 100 deep are rejected too, since both
/// the walk and geozero recurse once per level.
pub fn wkb_to_geometry(wkb: &[u8]) -> Result<Geometry> {
    check_wkb_bounds(wkb).map_err(WkbError::DecodeError)?;
    Wkb(wkb)
        .to_geo()
        .map_err(|e| WkbError::DecodeError(e.to_string()))
}

/// Deepest GeometryCollection nesting [`wkb_to_geometry`] accepts. Real data
/// nests one or two levels; the cap exists so a crafted blob cannot recurse
/// the decoder into a stack overflow.
const MAX_NESTING_DEPTH: usize = 100;

/// Smallest encoding of any geometry the walk accepts: a 5-byte header plus a
/// 4-byte count (an empty LineString, Polygon, Multi* or collection).
const MIN_GEOMETRY_BYTES: u64 = 9;

/// Smallest MultiPoint member: a 5-byte header plus an XY coordinate.
const MIN_MEMBER_POINT_BYTES: u64 = 5 + 16;

/// Walk OGC/ISO WKB exactly as geozero's `Wkb` reader does, allocating
/// nothing, and fail on the first count the remaining bytes cannot back.
///
/// It mirrors geozero rather than the spec where the two differ, so that it
/// accepts every blob geozero decodes: any non-zero byte-order byte means
/// little-endian, the type code is `dims * 1000 + base`, and the members of a
/// Multi* take their byte order and dimensions from their own header while
/// their base type is not checked. Trailing bytes are ignored, as geozero
/// ignores them. Curve, surface and triangle types are rejected: geozero
/// cannot build a `geo::Geometry` from them, and the one case it tolerates
/// (an empty one inside a collection) it silently drops.
///
/// Returns the number of bytes the geometry occupies, which is where geozero
/// stops reading too.
fn check_wkb_bounds(wkb: &[u8]) -> std::result::Result<usize, String> {
    let mut cursor = Cursor { buf: wkb, pos: 0 };
    cursor.geometry(0)?;
    Ok(cursor.pos)
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

/// A geometry header: byte order, base type, and bytes per coordinate.
#[derive(Clone, Copy)]
struct Header {
    little_endian: bool,
    base_type: u32,
    coord_bytes: u64,
}

impl Cursor<'_> {
    fn remaining(&self) -> u64 {
        (self.buf.len() - self.pos) as u64
    }

    fn skip(&mut self, n: u64) -> std::result::Result<(), String> {
        if n > self.remaining() {
            return Err(format!(
                "WKB truncated: needs {n} more bytes at offset {}, {} remain",
                self.pos,
                self.remaining()
            ));
        }
        self.pos += n as usize;
        Ok(())
    }

    fn u32(&mut self, little_endian: bool) -> std::result::Result<u32, String> {
        let start = self.pos;
        self.skip(4)?;
        let bytes: [u8; 4] = self.buf[start..self.pos]
            .try_into()
            .expect("skip(4) advanced exactly four bytes");
        Ok(if little_endian {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn header(&mut self) -> std::result::Result<Header, String> {
        let start = self.pos;
        self.skip(1)?;
        let little_endian = self.buf[start] != 0;
        let type_id = self.u32(little_endian)?;
        let dims = type_id / 1000;
        let has_z = matches!(dims, 1 | 3);
        let has_m = matches!(dims, 2 | 3);
        Ok(Header {
            little_endian,
            base_type: type_id % 1000,
            coord_bytes: 8 * (2 + u64::from(has_z) + u64::from(has_m)),
        })
    }

    /// Read a count and check that `count` elements of at least
    /// `min_element_bytes` each fit in what is left.
    fn count(
        &mut self,
        header: Header,
        min_element_bytes: u64,
        what: &str,
    ) -> std::result::Result<u32, String> {
        let n = self.u32(header.little_endian)?;
        let remaining = self.remaining();
        if u64::from(n) * min_element_bytes > remaining {
            return Err(format!(
                "WKB declares {n} {what} but only {remaining} bytes remain \
                 (each needs at least {min_element_bytes})"
            ));
        }
        Ok(n)
    }

    fn points(&mut self, header: Header) -> std::result::Result<(), String> {
        let n = self.count(header, header.coord_bytes, "points")?;
        self.skip(u64::from(n) * header.coord_bytes)
    }

    fn rings(&mut self, header: Header) -> std::result::Result<(), String> {
        for _ in 0..self.count(header, 4, "rings")? {
            self.points(header)?;
        }
        Ok(())
    }

    fn geometry(&mut self, depth: usize) -> std::result::Result<(), String> {
        let header = self.header()?;
        match header.base_type {
            1 => self.skip(header.coord_bytes),
            2 => self.points(header),
            3 => self.rings(header),
            4 => {
                for _ in 0..self.count(header, MIN_MEMBER_POINT_BYTES, "points")? {
                    let member = self.header()?;
                    self.skip(member.coord_bytes)?;
                }
                Ok(())
            }
            5 => {
                for _ in 0..self.count(header, MIN_GEOMETRY_BYTES, "linestrings")? {
                    let member = self.header()?;
                    self.points(member)?;
                }
                Ok(())
            }
            6 => {
                for _ in 0..self.count(header, MIN_GEOMETRY_BYTES, "polygons")? {
                    let member = self.header()?;
                    self.rings(member)?;
                }
                Ok(())
            }
            7 => {
                if depth >= MAX_NESTING_DEPTH {
                    return Err(format!(
                        "WKB nests GeometryCollections deeper than {MAX_NESTING_DEPTH} levels"
                    ));
                }
                for _ in 0..self.count(header, MIN_GEOMETRY_BYTES, "geometries")? {
                    self.geometry(depth + 1)?;
                }
                Ok(())
            }
            other => Err(format!("unsupported WKB geometry type {other}")),
        }
    }
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
    /// declares 3,338,651,383 polygons in an 11-byte blob. geozero reserves
    /// room for every declared polygon before reading one, so this asked the
    /// allocator for 160 GB.
    #[test]
    fn test_fuzz_623_huge_multipolygon_count_is_rejected() {
        let bytes = [10, 198, 255, 193, 255, 247, 198, 255, 198, 198, 255];
        let err = wkb_to_geometry(&bytes).expect_err("an 11-byte blob holds no polygons");
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

    fn assert_count_rejected(bytes: &[u8], what: &str) {
        let err = wkb_to_geometry(bytes).expect_err("the count cannot fit");
        assert!(
            err.to_string()
                .contains(&format!("declares {} {what}", u32::MAX)),
            "expected a {what} count error, got: {err}"
        );
    }

    /// Every count in the format is checked, not just the one the fuzzer hit.
    /// Each blob declares `u32::MAX` elements, which would reserve tens of GB.
    #[test]
    fn test_every_count_site_rejects_an_impossible_count() {
        assert_count_rejected(&with_count(header(2), u32::MAX), "points");
        assert_count_rejected(&with_count(header(3), u32::MAX), "rings");
        assert_count_rejected(&with_count(with_count(header(3), 1), u32::MAX), "points");
        assert_count_rejected(&with_count(header(4), u32::MAX), "points");
        assert_count_rejected(&with_count(header(5), u32::MAX), "linestrings");
        let mut member_line = with_count(header(5), 1);
        member_line.extend(with_count(header(2), u32::MAX));
        assert_count_rejected(&member_line, "points");
        let mut member_polygon = with_count(header(6), 1);
        member_polygon.extend(with_count(header(3), u32::MAX));
        assert_count_rejected(&member_polygon, "rings");
        assert_count_rejected(&with_count(header(6), u32::MAX), "polygons");
        assert_count_rejected(&with_count(header(7), u32::MAX), "geometries");
    }

    /// The walk must step over exactly the bytes each geometry occupies. A
    /// walk that mis-sizes coordinates can still pass, because geozero then
    /// fails on the same input, so the byte count is asserted directly.
    #[test]
    fn test_walk_consumes_exactly_the_geometry() {
        let point_zm = coords(header(3001), &[1.0, 2.0, 3.0, 4.0]);
        let point_z = coords(header(1001), &[1.0, 2.0, 3.0]);
        let point_m = coords(header(2001), &[1.0, 2.0, 3.0]);
        let line_zm = coords(with_count(header(3002), 2), &[0.0; 8]);
        let mut multi_line = with_count(header(5), 2);
        for _ in 0..2 {
            multi_line.extend(coords(with_count(header(2), 2), &[0.0, 0.0, 1.0, 1.0]));
        }
        let mut multi_point_zm = with_count(header(4), 2);
        for _ in 0..2 {
            multi_point_zm.extend(coords(header(3001), &[1.0, 2.0, 3.0, 4.0]));
        }
        let round_trips: Vec<Vec<u8>> = [
            Geometry::Point(point!(x: 1.0, y: 2.0)),
            Geometry::Polygon(polygon![
                (x: 0.0, y: 0.0), (x: 4.0, y: 0.0), (x: 4.0, y: 4.0), (x: 0.0, y: 0.0)
            ]),
        ]
        .iter()
        .map(|g| geometry_to_wkb(g).expect("encode"))
        .collect();
        for bytes in [
            point_zm,
            point_z,
            point_m,
            line_zm,
            multi_line,
            multi_point_zm,
        ]
        .into_iter()
        .chain(round_trips)
        {
            assert_eq!(
                check_wkb_bounds(&bytes),
                Ok(bytes.len()),
                "the walk must end exactly at the end of {bytes:?}"
            );
        }
    }

    /// A count only slightly too large is caught by the count check itself,
    /// not left for the truncation check or for geozero.
    #[test]
    fn test_a_count_one_element_too_large_is_rejected_by_the_count_check() {
        let line = coords(with_count(header(2), 3), &[0.0, 0.0, 1.0, 1.0]);
        let err = check_wkb_bounds(&line).expect_err("three points need 48 bytes, 32 remain");
        assert!(err.contains("declares 3 points"), "{err}");
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

    /// Z, M and ZM coordinates are 24, 24 and 32 bytes. The walk must size
    /// them as geozero does, or it would reject valid 3D input.
    #[test]
    fn test_iso_z_m_and_zm_coordinates_are_sized_correctly() {
        for (type_id, n_values) in [(1001, 3), (2001, 3), (3001, 4)] {
            let values: Vec<f64> = (0..n_values).map(f64::from).collect();
            let point = coords(header(type_id), &values);
            assert!(
                matches!(wkb_to_geometry(&point), Ok(Geometry::Point(_))),
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
        let point = coords(header(1), &[1.0, 2.0]);
        assert!(
            wkb_to_geometry(&nest(point.clone(), MAX_NESTING_DEPTH)).is_ok(),
            "nesting at the cap decodes"
        );
        let err = wkb_to_geometry(&nest(point, MAX_NESTING_DEPTH + 1))
            .expect_err("one level past the cap");
        assert!(err.to_string().contains("deeper than"), "{err}");
    }

    #[test]
    fn test_unsupported_types_are_rejected_by_name() {
        // CircularString (8) and Triangle (17): geozero reads them but cannot
        // build a geo::Geometry from them.
        for type_id in [8, 17] {
            let err = wkb_to_geometry(&with_count(header(type_id), 0))
                .expect_err("not representable as geo::Geometry");
            assert!(
                err.to_string()
                    .contains(&format!("unsupported WKB geometry type {type_id}")),
                "{err}"
            );
        }
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
