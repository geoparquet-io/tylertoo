//! Bounds check for WKB values read from GeoParquet geometry columns.
//!
//! GeoParquet WKB columns are decoded by geoarrow-array, which hands each
//! value to the `wkb` crate's reader (`wkb::reader::Wkb::try_new`, 0.9.2).
//! That reader trusts the counts in the data: it reserves a `Vec` for every
//! ring, member line, member polygon and collection member a header declares
//! before it checks that the bytes exist, and it recurses into nested
//! GeometryCollections with no depth limit. A 9-byte value claiming four
//! billion polygons asks the allocator for about 206 GB, and a point wrapped
//! in a thousand one-member collections overflows a thread stack. Both abort
//! the process.
//!
//! [`check_wkb_value`] walks a value once, allocating nothing, and rejects it
//! before the reader sees it when:
//!
//! - a count declares more elements than the remaining bytes can hold (the
//!   reader would reserve memory for all of them, then fail anyway);
//! - GeometryCollections nest more than [`MAX_NESTING_DEPTH`] deep;
//! - a MultiPoint member's SRID flag pushes its coordinates past the end of
//!   the MultiPoint (the reader panics on that when the member is converted);
//! - the value is malformed in any way the reader itself would reject.
//!
//! The walk follows the `wkb` crate's reading rules, not the spec's, so that
//! it accepts exactly what the reader accepts apart from the cases above:
//!
//! - the byte-order byte must be 0 or 1;
//! - the base type is `code & 7`, and the dimensions come from `code / 1000`
//!   (ISO) overridden by the EWKB Z and M flags;
//! - the EWKB SRID flag adds a 4-byte SRID after any header;
//! - the members of a MultiPoint, MultiLineString and MultiPolygon are read
//!   with the parent's byte order and dimensions, and their own byte-order
//!   byte and base type are ignored, though their SRID flag is not;
//! - GeometryCollection members are complete geometries with their own
//!   header;
//! - trailing bytes after the geometry are ignored.
//!
//! After the walk passes, memory grows linearly with the value's length. The
//! worst case is a polygon of empty rings: 4 input bytes per ring become a
//! 32-byte ring in the reader and a 24-byte `geo` line string after
//! conversion, about 14 bytes per input byte while both are alive.
//!
//! Two places run the walk: [`crate::batch_processor`] on the primary
//! geometry column before decoding it, and the overview writer on any other
//! `geoarrow.wkb` column, which is passed through to the output unchanged and
//! read by the geoparquet encoder.
//!
//! This duplicates checks the `wkb` crate should make itself. Once a `wkb`
//! release bounds its counts and its nesting, the walk can go.

use std::fmt;

use arrow_array::cast::AsArray;
use arrow_array::Array;
use arrow_schema::DataType;

/// Deepest GeometryCollection nesting accepted. Real data nests one or two
/// levels; the cap exists so a crafted value cannot recurse the reader, the
/// `geo` conversion, or the pipeline's own per-geometry recursion into a stack
/// overflow. It matches `--filter`'s nesting cap.
pub(crate) const MAX_NESTING_DEPTH: usize = 100;

/// Byte order and type code.
const HEADER_BYTES: u64 = 5;

/// The EWKB SRID that follows the header when the type code's SRID flag is set.
const SRID_BYTES: u64 = 4;

/// Smallest geometry the reader accepts as a member of a MultiLineString,
/// MultiPolygon or GeometryCollection: a header plus a zero count.
const MIN_MEMBER_BYTES: u64 = HEADER_BYTES + 4;

/// Smallest ring: a zero point count.
const MIN_RING_BYTES: u64 = 4;

const EWKB_FLAG_Z: u32 = 0x8000_0000;
const EWKB_FLAG_M: u32 = 0x4000_0000;
const EWKB_FLAG_SRID: u32 = 0x2000_0000;

/// Why [`check_wkb_value`] rejected a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WkbCheckError {
    /// A count declares more elements than the remaining bytes can hold.
    CountTooLarge {
        what: &'static str,
        count: u32,
        offset: u64,
        remaining: u64,
        min_element_bytes: u64,
    },
    /// GeometryCollections nest deeper than [`MAX_NESTING_DEPTH`].
    TooDeep,
    /// A MultiPoint member's SRID flag makes it extend past the MultiPoint.
    MultiPointMemberOverrun { index: u32, offset: u64 },
    /// Anything else the `wkb` crate's reader rejects too: a truncated value,
    /// a byte-order byte other than 0 or 1, an unknown base type.
    Malformed(String),
}

impl fmt::Display for WkbCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CountTooLarge {
                what,
                count,
                offset,
                remaining,
                min_element_bytes,
            } => write!(
                f,
                "WKB declares {count} {what} at byte {offset} but only {remaining} bytes \
                 remain (each needs at least {min_element_bytes})"
            ),
            Self::TooDeep => write!(
                f,
                "WKB nests GeometryCollections deeper than {MAX_NESTING_DEPTH} levels"
            ),
            Self::MultiPointMemberOverrun { index, offset } => write!(
                f,
                "WKB MultiPoint member {index} at byte {offset} sets the SRID flag, \
                 which runs its coordinates past the end of the MultiPoint"
            ),
            Self::Malformed(msg) => f.write_str(msg),
        }
    }
}

type CheckResult<T> = std::result::Result<T, WkbCheckError>;

/// Walk one WKB value by the `wkb` crate's rules (see the module docs) and
/// return the number of bytes the geometry occupies, which is where the
/// reader stops too.
pub(crate) fn check_wkb_value(buf: &[u8]) -> CheckResult<u64> {
    let mut walk = Walk { buf, pos: 0 };
    walk.geometry(0)?;
    Ok(walk.pos as u64)
}

/// Run [`check_wkb_value`] over every non-null value of a WKB column stored
/// as `Binary`, `LargeBinary` or `BinaryView`, and return the index of the
/// first value it rejects with the reason. An array of any other type holds no
/// WKB and passes.
pub(crate) fn check_wkb_column(
    array: &dyn Array,
) -> std::result::Result<(), (usize, WkbCheckError)> {
    fn check<'a>(
        values: impl Iterator<Item = Option<&'a [u8]>>,
    ) -> std::result::Result<(), (usize, WkbCheckError)> {
        for (i, value) in values.enumerate() {
            if let Some(bytes) = value {
                check_wkb_value(bytes).map_err(|e| (i, e))?;
            }
        }
        Ok(())
    }
    match array.data_type() {
        DataType::Binary => check(array.as_binary::<i32>().iter()),
        DataType::LargeBinary => check(array.as_binary::<i64>().iter()),
        DataType::BinaryView => check(array.as_binary_view().iter()),
        _ => Ok(()),
    }
}

#[derive(Clone, Copy)]
struct Header {
    little_endian: bool,
    /// Bytes per coordinate: 16, 24 or 32.
    coord_bytes: u64,
    has_srid: bool,
}

struct Walk<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Walk<'_> {
    fn remaining(&self) -> u64 {
        (self.buf.len() - self.pos) as u64
    }

    fn skip(&mut self, n: u64) -> CheckResult<()> {
        if n > self.remaining() {
            return Err(WkbCheckError::Malformed(format!(
                "WKB truncated: needs {n} bytes at byte {}, but only {} remain",
                self.pos,
                self.remaining()
            )));
        }
        self.pos += n as usize;
        Ok(())
    }

    fn u32_at(&self, pos: usize, little_endian: bool) -> CheckResult<u32> {
        let bytes: [u8; 4] = pos
            .checked_add(4)
            .and_then(|end| self.buf.get(pos..end))
            .and_then(|b| b.try_into().ok())
            .ok_or_else(|| {
                WkbCheckError::Malformed(format!(
                    "WKB truncated: needs 4 bytes at byte {pos}, but only {} remain",
                    self.buf.len().saturating_sub(pos)
                ))
            })?;
        Ok(if little_endian {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn u32(&mut self, little_endian: bool) -> CheckResult<u32> {
        let v = self.u32_at(self.pos, little_endian)?;
        self.pos += 4;
        Ok(v)
    }

    /// A complete geometry's header: byte order, type code, optional SRID.
    /// Returns the header and the base type (1–7).
    fn full_header(&mut self) -> CheckResult<(Header, u32)> {
        let little_endian = match self.buf.get(self.pos) {
            Some(0) => false,
            Some(1) => true,
            Some(other) => {
                return Err(WkbCheckError::Malformed(format!(
                    "WKB byte order {other} at byte {} is neither 0 nor 1",
                    self.pos
                )))
            }
            None => {
                return Err(WkbCheckError::Malformed(format!(
                    "WKB truncated: no geometry at byte {}",
                    self.pos
                )))
            }
        };
        let code = self.u32_at(self.pos + 1, little_endian)?;
        let base_type = code & 0x7;
        if base_type == 0 {
            return Err(WkbCheckError::Malformed(format!(
                "WKB type code {code} at byte {} has no geometry type",
                self.pos
            )));
        }
        let header = Header {
            little_endian,
            coord_bytes: coord_bytes(code),
            has_srid: code & EWKB_FLAG_SRID != 0,
        };
        self.pos += HEADER_BYTES as usize;
        self.skip_srid(header)?;
        Ok((header, base_type))
    }

    /// A Multi* member's header, which the reader reads with the parent's
    /// byte order and dimensions: only its SRID flag counts.
    fn member_header(&mut self, parent: Header) -> CheckResult<Header> {
        let code = self.u32_at(self.pos + 1, parent.little_endian)?;
        let header = Header {
            has_srid: code & EWKB_FLAG_SRID != 0,
            ..parent
        };
        self.pos += HEADER_BYTES as usize;
        self.skip_srid(header)?;
        Ok(header)
    }

    fn skip_srid(&mut self, header: Header) -> CheckResult<()> {
        if header.has_srid {
            self.skip(SRID_BYTES)?;
        }
        Ok(())
    }

    /// Read a count, and reject it when `count` elements of at least
    /// `min_element_bytes` each cannot fit in what is left.
    fn count(
        &mut self,
        header: Header,
        min_element_bytes: u64,
        what: &'static str,
    ) -> CheckResult<u32> {
        let offset = self.pos as u64;
        let count = self.u32(header.little_endian)?;
        let remaining = self.remaining();
        if u64::from(count) * min_element_bytes > remaining {
            return Err(WkbCheckError::CountTooLarge {
                what,
                count,
                offset,
                remaining,
                min_element_bytes,
            });
        }
        Ok(count)
    }

    /// A point count and the coordinates after it (a LineString body or a
    /// ring). The reader does not allocate per point, so an oversized count
    /// is only a truncation.
    fn points(&mut self, header: Header) -> CheckResult<()> {
        let n = self.u32(header.little_endian)?;
        self.skip(u64::from(n) * header.coord_bytes)
    }

    fn rings(&mut self, header: Header) -> CheckResult<()> {
        for _ in 0..self.count(header, MIN_RING_BYTES, "rings")? {
            self.points(header)?;
        }
        Ok(())
    }

    fn multi_point(&mut self, header: Header) -> CheckResult<()> {
        // The reader checks the whole MultiPoint's length up front with a
        // fixed member stride and allocates nothing.
        let n = self.u32(header.little_endian)?;
        let stride = HEADER_BYTES + header.coord_bytes;
        self.skip(u64::from(n) * stride)?;
        // Each member is read later from the start of its slot to the end of
        // the MultiPoint, and a member whose SRID flag is set needs four bytes
        // more than its slot. Every slot is at least 21 bytes, so only the
        // last member can run out of room. The reader unwraps that read, so
        // it panics.
        if let Some(last) = n.checked_sub(1) {
            let offset = self.pos - stride as usize;
            let code = self.u32_at(offset + 1, header.little_endian)?;
            if code & EWKB_FLAG_SRID != 0 {
                return Err(WkbCheckError::MultiPointMemberOverrun {
                    index: last,
                    offset: offset as u64,
                });
            }
        }
        Ok(())
    }

    fn geometry(&mut self, depth: usize) -> CheckResult<()> {
        let (header, base_type) = self.full_header()?;
        match base_type {
            1 => self.skip(header.coord_bytes),
            2 => self.points(header),
            3 => self.rings(header),
            4 => self.multi_point(header),
            5 => {
                for _ in 0..self.count(header, MIN_MEMBER_BYTES, "linestrings")? {
                    let member = self.member_header(header)?;
                    self.points(member)?;
                }
                Ok(())
            }
            6 => {
                for _ in 0..self.count(header, MIN_MEMBER_BYTES, "polygons")? {
                    let member = self.member_header(header)?;
                    self.rings(member)?;
                }
                Ok(())
            }
            7 => {
                if depth >= MAX_NESTING_DEPTH {
                    return Err(WkbCheckError::TooDeep);
                }
                for _ in 0..self.count(header, MIN_MEMBER_BYTES, "geometries")? {
                    self.geometry(depth + 1)?;
                }
                Ok(())
            }
            _ => unreachable!("base_type is code & 7 and not 0"),
        }
    }
}

/// Bytes per coordinate for a type code, by the `wkb` crate's rule: ISO
/// dimensions from `code / 1000`, overridden by the EWKB Z and M flags.
fn coord_bytes(code: u32) -> u64 {
    let (mut z, mut m) = match code / 1000 {
        1 => (true, false),
        2 => (false, true),
        3 => (true, true),
        _ => (false, false),
    };
    let (ewkb_z, ewkb_m) = (code & EWKB_FLAG_Z != 0, code & EWKB_FLAG_M != 0);
    if ewkb_z || ewkb_m {
        (z, m) = (ewkb_z, ewkb_m);
    }
    8 * (2 + u64::from(z) + u64::from(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::BinaryArray;
    use geo::Geometry;
    use geo_traits::to_geo::ToGeoGeometry;
    use geoarrow::array::WkbArray;
    use geoarrow::datatypes::WkbType;
    use geoarrow_array::GeoArrowArrayAccessor;

    // ------------------------------------------------------------------
    // WKB builders
    // ------------------------------------------------------------------

    /// Byte order, then the type code in that order.
    fn header_in(little_endian: bool, code: u32) -> Vec<u8> {
        let mut v = vec![u8::from(little_endian)];
        v.extend_from_slice(&if little_endian {
            code.to_le_bytes()
        } else {
            code.to_be_bytes()
        });
        v
    }

    fn header(code: u32) -> Vec<u8> {
        header_in(true, code)
    }

    fn u32_le(mut v: Vec<u8>, n: u32) -> Vec<u8> {
        v.extend_from_slice(&n.to_le_bytes());
        v
    }

    fn f64s(mut v: Vec<u8>, values: &[f64]) -> Vec<u8> {
        for x in values {
            v.extend_from_slice(&x.to_le_bytes());
        }
        v
    }

    fn point(x: f64, y: f64) -> Vec<u8> {
        f64s(header(1), &[x, y])
    }

    fn collection(members: &[Vec<u8>]) -> Vec<u8> {
        let mut v = u32_le(header(7), members.len() as u32);
        for m in members {
            v.extend_from_slice(m);
        }
        v
    }

    /// A point inside `depth` nested one-member GeometryCollections.
    fn nested_point(depth: usize) -> Vec<u8> {
        let mut v = Vec::new();
        for _ in 0..depth {
            v.extend_from_slice(&u32_le(header(7), 1));
        }
        v.extend_from_slice(&point(1.0, 2.0));
        v
    }

    /// The same value decoded the way a GeoParquet column decodes it:
    /// geoarrow-array's `WkbArray` handing the bytes to the `wkb` crate.
    fn wkb_crate_size(bytes: &[u8]) -> Option<u64> {
        let array = WkbArray::from((BinaryArray::from_vec(vec![bytes]), WkbType::default()));
        array.value(0).ok().map(|w| w.buf().len() as u64)
    }

    fn wkb_crate_geometry(bytes: &[u8]) -> Option<Geometry<f64>> {
        let array = WkbArray::from((BinaryArray::from_vec(vec![bytes]), WkbType::default()));
        array.value(0).ok().and_then(|w| w.try_to_geometry())
    }

    /// The walk must agree with the `wkb` crate's reader: a value it accepts
    /// the reader accepts and ends at the same byte, and a value it finds
    /// malformed the reader rejects. Only call this on inputs the walk does
    /// not reject for a count, the nesting cap or a MultiPoint overrun —
    /// those abort or panic the reader.
    fn assert_agrees(bytes: &[u8]) {
        match check_wkb_value(bytes) {
            Ok(size) => {
                assert_eq!(
                    wkb_crate_size(bytes),
                    Some(size),
                    "walk accepted {bytes:02x?} ({size} bytes); the reader must too"
                );
                // Converting must not panic either.
                let _ = wkb_crate_geometry(bytes);
            }
            Err(WkbCheckError::Malformed(msg)) => assert_eq!(
                wkb_crate_size(bytes),
                None,
                "walk rejected {bytes:02x?} ({msg}); the reader accepts it"
            ),
            Err(other) => panic!("expected accept or malformed for {bytes:02x?}, got: {other}"),
        }
    }

    fn assert_count_too_large(bytes: &[u8], expect_what: &str) {
        match check_wkb_value(bytes) {
            Err(WkbCheckError::CountTooLarge { what, count, .. }) => {
                assert_eq!(what, expect_what);
                assert_eq!(count, u32::MAX);
            }
            other => panic!("expected a {expect_what} count rejection, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // The inputs from #632
    // ------------------------------------------------------------------

    /// #632: a 9-byte MultiPolygon claiming 4,294,967,295 polygons. The
    /// reader reserves a `Vec` for all of them (about 206 GB) before reading
    /// one.
    #[test]
    fn issue_632_huge_multipolygon_count_is_rejected() {
        let bytes = [1, 6, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        let err = check_wkb_value(&bytes).unwrap_err();
        assert_eq!(
            err.to_string(),
            "WKB declares 4294967295 polygons at byte 5 but only 0 bytes remain \
             (each needs at least 9)"
        );
    }

    /// #632: a point inside 1,000 nested one-member GeometryCollections. The
    /// reader recurses once per level and overflows a 2 MB stack. The walk
    /// must reject it without recursing that deep itself, so it runs on a
    /// 2 MB thread here.
    #[test]
    fn issue_632_deep_collection_nesting_is_rejected() {
        let bytes = nested_point(1000);
        let result = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || check_wkb_value(&bytes))
            .unwrap()
            .join()
            .expect("the walk must not overflow the stack");
        assert_eq!(result, Err(WkbCheckError::TooDeep));
    }

    #[test]
    fn nesting_cap_is_exact() {
        let at_cap = nested_point(MAX_NESTING_DEPTH);
        assert_agrees(&at_cap);
        assert!(check_wkb_value(&at_cap).is_ok());
        assert_eq!(
            check_wkb_value(&nested_point(MAX_NESTING_DEPTH + 1)),
            Err(WkbCheckError::TooDeep)
        );
    }

    /// Values at the nesting cap decode and convert on a 2 MB stack, the
    /// default for spawned threads, in the debug build tests run in.
    #[test]
    fn nesting_cap_decodes_on_a_small_stack() {
        let bytes = nested_point(MAX_NESTING_DEPTH);
        let geom = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || wkb_crate_geometry(&bytes))
            .unwrap()
            .join()
            .expect("decoding at the cap must not overflow a 2 MB stack");
        let mut g = geom.expect("a valid value decodes");
        for _ in 0..MAX_NESTING_DEPTH {
            let Geometry::GeometryCollection(gc) = g else {
                panic!("expected a collection")
            };
            assert_eq!(gc.0.len(), 1);
            g = gc.0.into_iter().next().unwrap();
        }
        assert_eq!(g, Geometry::Point(geo::point!(x: 1.0, y: 2.0)));
    }

    // ------------------------------------------------------------------
    // Every allocation site
    // ------------------------------------------------------------------

    /// Every count the reader reserves memory for is checked, at the top level
    /// and inside a member.
    #[test]
    fn every_allocating_count_is_checked() {
        assert_count_too_large(&u32_le(header(3), u32::MAX), "rings");
        assert_count_too_large(&u32_le(header(5), u32::MAX), "linestrings");
        assert_count_too_large(&u32_le(header(6), u32::MAX), "polygons");
        assert_count_too_large(&u32_le(header(7), u32::MAX), "geometries");
        // A polygon member of a MultiPolygon.
        let mut mp = u32_le(header(6), 1);
        mp.extend_from_slice(&header(3));
        assert_count_too_large(&u32_le(mp, u32::MAX), "rings");
        // A collection member of a collection.
        let mut gc = u32_le(header(7), 1);
        gc.extend_from_slice(&header(6));
        assert_count_too_large(&u32_le(gc, u32::MAX), "polygons");
    }

    /// A count of exactly as many minimum-size elements as the bytes hold is
    /// accepted; one more is rejected before the reader sees it.
    #[test]
    fn count_boundary_is_exact() {
        // Two empty rings take 8 bytes.
        let two_rings = u32_le(u32_le(u32_le(header(3), 2), 0), 0);
        assert_agrees(&two_rings);
        assert_eq!(check_wkb_value(&two_rings), Ok(17));
        // Three empty rings take 12 bytes, which a fourth cannot share.
        let four_rings = u32_le(u32_le(u32_le(u32_le(header(3), 4), 0), 0), 0);
        assert!(matches!(
            check_wkb_value(&four_rings),
            Err(WkbCheckError::CountTooLarge {
                count: 4,
                remaining: 12,
                ..
            })
        ));

        // Two empty member linestrings take 18 bytes.
        let empty_ls = u32_le(header(2), 0);
        let mut mls = u32_le(header(5), 2);
        mls.extend_from_slice(&empty_ls);
        mls.extend_from_slice(&empty_ls);
        assert_agrees(&mls);
        mls[5] = 3;
        assert!(matches!(
            check_wkb_value(&mls),
            Err(WkbCheckError::CountTooLarge {
                count: 3,
                remaining: 18,
                ..
            })
        ));
    }

    /// Point counts are not an allocation site (the reader slices, it does
    /// not reserve), so a too-large one is plain truncation, which the reader
    /// rejects too.
    #[test]
    fn oversized_point_counts_are_truncation() {
        assert_agrees(&u32_le(header(2), u32::MAX));
        assert_agrees(&u32_le(header(4), u32::MAX));
        assert_agrees(&u32_le(u32_le(header(3), 1), u32::MAX));
    }

    // ------------------------------------------------------------------
    // The MultiPoint member panic
    // ------------------------------------------------------------------

    /// A MultiPoint whose last member sets the SRID flag: the reader's length
    /// check passes, then converting the member reads four bytes past the
    /// MultiPoint and panics on the `unwrap`.
    fn multipoint_last_member_srid() -> Vec<u8> {
        let mut v = u32_le(header(4), 2);
        v.extend_from_slice(&point(1.0, 2.0));
        v.extend_from_slice(&f64s(header(1 | EWKB_FLAG_SRID), &[3.0, 4.0]));
        v
    }

    #[test]
    fn multipoint_member_srid_overrun_is_rejected() {
        let bytes = multipoint_last_member_srid();
        assert_eq!(
            check_wkb_value(&bytes),
            Err(WkbCheckError::MultiPointMemberOverrun {
                index: 1,
                offset: 30
            })
        );
        let panicked = std::panic::catch_unwind(|| wkb_crate_geometry(&bytes)).is_err();
        assert!(
            panicked,
            "this input is only worth rejecting if the reader panics on it"
        );
    }

    /// A member that is not last has room for the SRID inside the MultiPoint,
    /// so the reader reads it (shifted) without panicking, and the walk
    /// accepts it as the reader does.
    #[test]
    fn multipoint_member_srid_with_room_is_accepted() {
        let mut v = u32_le(header(4), 2);
        v.extend_from_slice(&f64s(header(1 | EWKB_FLAG_SRID), &[3.0, 4.0]));
        v.extend_from_slice(&point(1.0, 2.0));
        assert_agrees(&v);
        assert!(check_wkb_value(&v).is_ok());
    }

    // ------------------------------------------------------------------
    // Agreement with the reader on valid values
    // ------------------------------------------------------------------

    fn valid_geometries() -> Vec<Geometry<f64>> {
        use geo::{
            line_string, point, polygon, GeometryCollection, MultiLineString, MultiPoint,
            MultiPolygon,
        };
        let ls = line_string![(x: 0.0, y: 0.0), (x: 1.0, y: 1.0), (x: 2.0, y: 0.0)];
        let poly = polygon!(
            exterior: [(x: 0.0, y: 0.0), (x: 4.0, y: 0.0), (x: 4.0, y: 4.0), (x: 0.0, y: 0.0)],
            interiors: [[(x: 1.0, y: 1.0), (x: 2.0, y: 1.0), (x: 2.0, y: 2.0), (x: 1.0, y: 1.0)]],
        );
        vec![
            Geometry::Point(point!(x: 1.5, y: -2.5)),
            Geometry::LineString(ls.clone()),
            Geometry::Polygon(poly.clone()),
            Geometry::MultiPoint(MultiPoint::from(vec![(0.0, 0.0), (1.0, 2.0), (3.0, 4.0)])),
            Geometry::MultiLineString(MultiLineString::new(vec![ls.clone(), ls.clone()])),
            Geometry::MultiPolygon(MultiPolygon::new(vec![poly.clone(), poly.clone()])),
            Geometry::GeometryCollection(GeometryCollection::new_from(vec![
                Geometry::Point(point!(x: 0.0, y: 0.0)),
                Geometry::Polygon(poly),
                Geometry::GeometryCollection(GeometryCollection::new_from(vec![
                    Geometry::LineString(ls),
                ])),
            ])),
        ]
    }

    /// Every OGC type written by a real encoder is accepted, with the size
    /// the reader reports, and decodes to the same geometry.
    #[test]
    fn valid_values_are_accepted_unchanged() {
        for geom in valid_geometries() {
            let bytes = crate::wkb::geometry_to_wkb(&geom).unwrap();
            assert_eq!(check_wkb_value(&bytes), Ok(bytes.len() as u64), "{geom:?}");
            assert_agrees(&bytes);
            assert_eq!(wkb_crate_geometry(&bytes), Some(geom));
        }
    }

    /// Trailing bytes are ignored, as the reader ignores them.
    #[test]
    fn trailing_bytes_are_ignored() {
        let mut bytes = point(1.0, 2.0);
        bytes.extend_from_slice(&[0xde, 0xad]);
        assert_eq!(check_wkb_value(&bytes), Ok(21));
        assert_agrees(&bytes);
    }

    /// Byte orders, ISO and EWKB dimensions and the SRID flag follow the
    /// reader's rules.
    #[test]
    fn header_variants_follow_the_reader() {
        // Big-endian point.
        let mut be = header_in(false, 1);
        be.extend_from_slice(&1.0f64.to_be_bytes());
        be.extend_from_slice(&2.0f64.to_be_bytes());
        assert_eq!(check_wkb_value(&be), Ok(21));
        assert_agrees(&be);

        // ISO Z, M, ZM and EWKB Z, M, ZM points.
        for (code, dims) in [
            (1001, 3),
            (2001, 3),
            (3001, 4),
            (1 | EWKB_FLAG_Z, 3),
            (1 | EWKB_FLAG_M, 3),
            (1 | EWKB_FLAG_Z | EWKB_FLAG_M, 4),
            // EWKB flags override the ISO dimensions.
            (3001 | EWKB_FLAG_Z, 3),
        ] {
            let bytes = f64s(header(code), &vec![1.0; dims]);
            assert_eq!(
                check_wkb_value(&bytes),
                Ok(5 + 8 * dims as u64),
                "code {code:#x}"
            );
            assert_agrees(&bytes);
            // One coordinate short.
            assert_agrees(&bytes[..bytes.len() - 8]);
        }

        // EWKB SRID on a point, a linestring and a polygon.
        let srid_point = f64s(u32_le(header(1 | EWKB_FLAG_SRID), 4326), &[1.0, 2.0]);
        assert_eq!(check_wkb_value(&srid_point), Ok(25));
        assert_agrees(&srid_point);
        let srid_ls = f64s(
            u32_le(u32_le(header(2 | EWKB_FLAG_SRID), 4326), 1),
            &[1.0, 2.0],
        );
        assert_eq!(check_wkb_value(&srid_ls), Ok(29));
        assert_agrees(&srid_ls);
        let srid_poly = u32_le(u32_le(header(3 | EWKB_FLAG_SRID), 4326), 0);
        assert_eq!(check_wkb_value(&srid_poly), Ok(13));
        assert_agrees(&srid_poly);

        // The base type is `code & 7`, so 9 reads as a point.
        let odd_code = f64s(header(9), &[1.0, 2.0]);
        assert_agrees(&odd_code);
        assert!(check_wkb_value(&odd_code).is_ok());

        // Byte order other than 0 or 1, an unknown type, and empty input.
        assert_agrees(&f64s(
            {
                let mut h = header(1);
                h[0] = 2;
                h
            },
            &[1.0, 2.0],
        ));
        assert_agrees(&f64s(header(8), &[1.0, 2.0]));
        assert_agrees(&[]);
        assert_eq!(
            check_wkb_value(&[]).unwrap_err().to_string(),
            "WKB truncated: no geometry at byte 0"
        );
        assert_agrees(&[1]);
        assert_agrees(&[1, 1, 0, 0]);
    }

    /// Multi* members take the parent's byte order and dimensions; their own
    /// byte-order byte and base type are not read, but their SRID flag is.
    #[test]
    fn multi_members_follow_the_reader() {
        // A MultiLineString member with a nonsense byte order and base type.
        let mut member = u32_le(header(0xff), 1);
        member[0] = 7;
        let member = f64s(member, &[1.0, 2.0]);
        let mut mls = u32_le(header(5), 1);
        mls.extend_from_slice(&member);
        assert_eq!(check_wkb_value(&mls), Ok(9 + 25));
        assert_agrees(&mls);

        // A MultiPolygon member with the SRID flag.
        let mut mp = u32_le(header(6), 1);
        mp.extend_from_slice(&u32_le(u32_le(header(3 | EWKB_FLAG_SRID), 4326), 0));
        assert_eq!(check_wkb_value(&mp), Ok(9 + 13));
        assert_agrees(&mp);

        // A ZM MultiLineString: the member is read as ZM although its own
        // code says XY.
        let mut zm = u32_le(header(3005), 1);
        zm.extend_from_slice(&f64s(u32_le(header(2), 1), &[1.0, 2.0, 3.0, 4.0]));
        assert_eq!(check_wkb_value(&zm), Ok(9 + 9 + 32));
        assert_agrees(&zm);

        // A collection's members keep their own byte order.
        let mut be_point = header_in(false, 1);
        be_point.extend_from_slice(&1.0f64.to_be_bytes());
        be_point.extend_from_slice(&2.0f64.to_be_bytes());
        let gc = collection(&[be_point, point(3.0, 4.0)]);
        assert_eq!(check_wkb_value(&gc), Ok(9 + 21 + 21));
        assert_agrees(&gc);
    }

    /// Randomly damaged copies of valid values: whenever the walk accepts one
    /// or calls it malformed, the reader must agree. The damaged copies the
    /// walk rejects for a count, the nesting cap or a MultiPoint overrun are
    /// not handed to the reader, since those are the ones that abort it.
    #[test]
    fn damaged_values_agree_with_the_reader() {
        let mut seeds: Vec<Vec<u8>> = valid_geometries()
            .iter()
            .map(|g| crate::wkb::geometry_to_wkb(g).unwrap())
            .collect();
        seeds.push(nested_point(3));
        seeds.push(u32_le(u32_le(header(3), 2), 0));
        seeds.push(f64s(u32_le(header(1 | EWKB_FLAG_SRID), 4326), &[1.0, 2.0]));

        // xorshift64: deterministic, no dependency.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut checked = [0usize; 3];
        for _ in 0..50_000 {
            let seed = &seeds[(next() % seeds.len() as u64) as usize];
            let mut bytes = seed.clone();
            for _ in 0..=next() % 3 {
                match next() % 4 {
                    // Overwrite a byte, biased towards headers and counts.
                    0 | 1 if !bytes.is_empty() => {
                        let i = (next() % bytes.len().min(40) as u64) as usize;
                        bytes[i] = next() as u8;
                    }
                    // Set a small count or type anywhere.
                    2 if bytes.len() >= 4 => {
                        let i = (next() % (bytes.len() - 3) as u64) as usize;
                        let v = (next() % 8) as u32;
                        bytes[i..i + 4].copy_from_slice(&v.to_le_bytes());
                    }
                    _ => {
                        let n = (next() % (bytes.len() as u64 + 1)) as usize;
                        bytes.truncate(n);
                    }
                }
            }
            match check_wkb_value(&bytes) {
                Ok(_) => {
                    checked[0] += 1;
                    assert_agrees(&bytes);
                }
                Err(WkbCheckError::Malformed(_)) => {
                    checked[1] += 1;
                    assert_agrees(&bytes);
                }
                Err(_) => checked[2] += 1,
            }
        }
        // The damage must exercise all three outcomes, or the test proves
        // little.
        assert!(checked.iter().all(|&n| n > 1000), "{checked:?}");
    }
}
