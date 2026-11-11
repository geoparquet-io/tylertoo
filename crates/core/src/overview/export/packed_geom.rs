//! A member geometry at rest (#815): integer coordinates on a 2^32 grid per
//! 360 degrees, as zigzag varint deltas, as tippecanoe stores features
//! between its passes (serial.cpp `write_geometry`). A member is decoded to
//! f64 only while its tile is made.
//!
//! DIVERGENCE FROM TIPPECANOE: tippecanoe's grid is Web Mercator; this one is
//! lon/lat at the same scale (2^32 units per 360 degrees on both axes), as
//! members are lon/lat. Decoding needs no inverse Mercator per vertex, and
//! the error, at most 4.2e-8 degrees (under 5 mm), is far below a tile unit
//! at any zoom an export makes (z18 at 4096 extent: 9 mm at the equator).
//!
//! Only linear shapes are packed: a point is held inline, and a shape
//! without one finite coordinate per vertex stays f64.

use geo::{Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};

/// Grid units per degree: 2^32 per 360 degrees.
const UNITS_PER_DEGREE: f64 = 4_294_967_296.0 / 360.0;

const LINE_STRING: u8 = 0;
const POLYGON: u8 = 1;
const MULTI_POINT: u8 = 2;
const MULTI_LINE_STRING: u8 = 3;
const MULTI_POLYGON: u8 = 4;

/// `g` packed, or `None` for a point, a shape this format does not hold, or
/// a non-finite coordinate.
pub(super) fn pack(g: &Geometry<f64>) -> Option<Box<[u8]>> {
    let mut w = Writer::default();
    match g {
        Geometry::LineString(ls) => {
            w.buf.push(LINE_STRING);
            w.line(ls)?;
        }
        Geometry::Polygon(p) => {
            w.buf.push(POLYGON);
            w.polygon(p)?;
        }
        Geometry::MultiPoint(mp) => {
            w.buf.push(MULTI_POINT);
            w.count(mp.0.len());
            for p in &mp.0 {
                w.coord(p.0)?;
            }
        }
        Geometry::MultiLineString(mls) => {
            w.buf.push(MULTI_LINE_STRING);
            w.count(mls.0.len());
            for ls in &mls.0 {
                w.line(ls)?;
            }
        }
        Geometry::MultiPolygon(mp) => {
            w.buf.push(MULTI_POLYGON);
            w.count(mp.0.len());
            for p in &mp.0 {
                w.polygon(p)?;
            }
        }
        _ => return None,
    }
    Some(w.buf.into_boxed_slice())
}

/// The geometry `bytes` were packed from, on the grid. `None` when `bytes`
/// are not a packed geometry (a corrupt spill).
pub(super) fn unpack(bytes: &[u8]) -> Option<Geometry<f64>> {
    let (&kind, rest) = bytes.split_first()?;
    let mut r = Reader {
        buf: rest,
        x: 0,
        y: 0,
    };
    let g = match kind {
        LINE_STRING => Geometry::LineString(r.line()?),
        POLYGON => Geometry::Polygon(r.polygon()?),
        MULTI_POINT => {
            let n = r.count()?;
            let mut pts = Vec::with_capacity(n);
            for _ in 0..n {
                pts.push(Point(r.coord()?));
            }
            Geometry::MultiPoint(MultiPoint(pts))
        }
        MULTI_LINE_STRING => {
            let n = r.count()?;
            let mut lines = Vec::with_capacity(n);
            for _ in 0..n {
                lines.push(r.line()?);
            }
            Geometry::MultiLineString(MultiLineString(lines))
        }
        MULTI_POLYGON => {
            let n = r.count()?;
            let mut polys = Vec::with_capacity(n);
            for _ in 0..n {
                polys.push(r.polygon()?);
            }
            Geometry::MultiPolygon(MultiPolygon(polys))
        }
        _ => return None,
    };
    r.buf.is_empty().then_some(g)
}

#[derive(Default)]
struct Writer {
    buf: Vec<u8>,
    x: i64,
    y: i64,
}

impl Writer {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }

    fn count(&mut self, n: usize) {
        self.varint(n as u64);
    }

    fn coord(&mut self, c: Coord<f64>) -> Option<()> {
        let grid = |v: f64| {
            let q = (v * UNITS_PER_DEGREE).round();
            // Every finite lon/lat, buffered past the world edge or not, is
            // far inside i64.
            (q.is_finite() && q.abs() < 2f64.powi(62)).then_some(q as i64)
        };
        let (x, y) = (grid(c.x)?, grid(c.y)?);
        let zigzag = |d: i64| ((d << 1) ^ (d >> 63)) as u64;
        self.varint(zigzag(x - self.x));
        self.varint(zigzag(y - self.y));
        (self.x, self.y) = (x, y);
        Some(())
    }

    fn line(&mut self, ls: &LineString<f64>) -> Option<()> {
        self.count(ls.0.len());
        ls.0.iter().try_for_each(|c| self.coord(*c))
    }

    fn polygon(&mut self, p: &Polygon<f64>) -> Option<()> {
        self.count(1 + p.interiors().len());
        self.line(p.exterior())?;
        p.interiors().iter().try_for_each(|r| self.line(r))
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    x: i64,
    y: i64,
}

impl Reader<'_> {
    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = self.buf.split_first()?;
            self.buf = rest;
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Some(v);
            }
        }
        None
    }

    /// A count, bounded by the bytes left (every element takes at least
    /// one), so a corrupt count cannot reserve a huge vector.
    fn count(&mut self) -> Option<usize> {
        let n = usize::try_from(self.varint()?).ok()?;
        (n <= self.buf.len()).then_some(n)
    }

    fn coord(&mut self) -> Option<Coord<f64>> {
        let unzigzag = |v: u64| ((v >> 1) as i64) ^ -((v & 1) as i64);
        self.x = self.x.wrapping_add(unzigzag(self.varint()?));
        self.y = self.y.wrapping_add(unzigzag(self.varint()?));
        Some(Coord {
            x: self.x as f64 / UNITS_PER_DEGREE,
            y: self.y as f64 / UNITS_PER_DEGREE,
        })
    }

    fn line(&mut self) -> Option<LineString<f64>> {
        let n = self.count()?;
        let mut coords = Vec::with_capacity(n);
        for _ in 0..n {
            coords.push(self.coord()?);
        }
        Some(LineString(coords))
    }

    fn polygon(&mut self) -> Option<Polygon<f64>> {
        let rings = self.count()?;
        if rings == 0 {
            return None;
        }
        let exterior = self.line()?;
        let mut interiors = Vec::with_capacity(rings - 1);
        for _ in 1..rings {
            interiors.push(self.line()?);
        }
        Some(Polygon::new(exterior, interiors))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::CoordsIter;

    /// A parcel-like ring: `n` vertices about a metre apart near Wellington.
    fn ring(n: usize) -> LineString<f64> {
        let mut c: Vec<Coord<f64>> = (0..n)
            .map(|i| {
                let t = i as f64 / n as f64 * std::f64::consts::TAU;
                Coord {
                    x: 174.776 + 0.0002 * t.cos(),
                    y: -41.286 + 0.0002 * t.sin(),
                }
            })
            .collect();
        c.push(c[0]);
        LineString(c)
    }

    /// #815: a vertex at rest takes a few bytes, where f64 took 16.
    #[test]
    fn a_packed_vertex_takes_at_most_four_bytes() {
        let g = Geometry::Polygon(Polygon::new(ring(200), vec![ring(50)]));
        let bytes = pack(&g).unwrap();
        let vertices = g.coords_count();
        assert!(
            bytes.len() <= 4 * vertices + 8,
            "{} bytes for {vertices} vertices",
            bytes.len()
        );
    }

    #[test]
    fn unpack_restores_every_shape_on_the_grid() {
        let half_unit = 0.5 / UNITS_PER_DEGREE;
        let shapes = [
            Geometry::LineString(ring(9)),
            Geometry::Polygon(Polygon::new(ring(12), vec![ring(5), ring(4)])),
            Geometry::MultiPoint(MultiPoint(vec![
                Point::new(-179.9, 85.0),
                Point::new(181.2, -85.1),
            ])),
            Geometry::MultiLineString(MultiLineString(vec![ring(3), ring(7)])),
            Geometry::MultiPolygon(MultiPolygon(vec![
                Polygon::new(ring(6), vec![]),
                Polygon::new(ring(8), vec![ring(4)]),
            ])),
        ];
        for g in shapes {
            let back = unpack(&pack(&g).unwrap()).unwrap();
            assert_eq!(std::mem::discriminant(&g), std::mem::discriminant(&back));
            assert_eq!(g.coords_count(), back.coords_count());
            for (a, b) in g.coords_iter().zip(back.coords_iter()) {
                assert!((a.x - b.x).abs() <= half_unit && (a.y - b.y).abs() <= half_unit);
            }
            // The grid is a fixed point: a second round trip is exact.
            assert_eq!(unpack(&pack(&back).unwrap()).unwrap(), back);
        }
    }

    #[test]
    fn points_odd_shapes_and_non_finite_coordinates_stay_f64() {
        assert!(pack(&Geometry::Point(Point::new(1.0, 2.0))).is_none());
        let rect = geo::Rect::new(Coord { x: 0.0, y: 0.0 }, Coord { x: 1.0, y: 1.0 });
        assert!(pack(&Geometry::Rect(rect)).is_none());
        let nan = LineString(vec![
            Coord { x: 0.0, y: 0.0 },
            Coord {
                x: f64::NAN,
                y: 1.0,
            },
        ]);
        assert!(pack(&Geometry::LineString(nan)).is_none());
    }

    #[test]
    fn corrupt_bytes_do_not_unpack() {
        let bytes = pack(&Geometry::LineString(ring(5))).unwrap();
        assert!(unpack(&bytes[..bytes.len() - 1]).is_none());
        assert!(unpack(&[9]).is_none());
        assert!(unpack(&[LINE_STRING, 0xff, 0xff, 0xff, 0xff, 0x0f]).is_none());
        let mut trailing = bytes.to_vec();
        trailing.push(0);
        assert!(unpack(&trailing).is_none());
    }
}
