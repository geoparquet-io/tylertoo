//! Reprojection of projected-CRS input into EPSG:4326 as it is read (#670).
//!
//! [`ToLonLat`] is built once per input part, from the EPSG code its CRS
//! declares, and rewrites the geometry column of every batch that part's
//! reader yields ([`crate::input_set::SourceStream`]). Every consumer
//! downstream therefore sees lon/lat, and an EPSG:4326 or EPSG:3857 part
//! never builds one, so it does no new work.
//!
//! Accuracy: proj4 definitions carry no grid shifts, so a datum change uses
//! the definition's `+towgs84` Helmert parameters, as proj4js does.

use std::sync::Arc;

use arrow_array::{
    cast::AsArray, make_array, Array, ArrayRef, BinaryViewArray, Float64Array, GenericBinaryArray,
    OffsetSizeTrait, RecordBatch,
};
use arrow_buffer::Buffer;
use arrow_data::ArrayData;
use arrow_schema::{ArrowError, DataType};
use proj4rs::Proj;

use crate::quality::CrsInfo;

/// The transform from one projected or geographic CRS to EPSG:4326 degrees.
#[derive(Debug)]
pub(crate) struct ToLonLat {
    src: Proj,
    dst: Proj,
    /// The source is geographic, so proj4rs wants its input in radians.
    src_degrees: bool,
}

/// The EPSG code a CRS identifier names: `EPSG:28992`, the URN and URL
/// spellings, or a Parquet `srid:28992`.
fn epsg_code(id: &str) -> Option<u16> {
    let up = id.trim().to_ascii_uppercase();
    if !(up.contains("EPSG") || up.starts_with("SRID:")) {
        return None;
    }
    let digits = up.rsplit(|c: char| !c.is_ascii_digit()).next()?;
    digits.parse().ok()
}

impl ToLonLat {
    /// The transform for a declared CRS, or `None` when it names no EPSG
    /// code with a usable definition.
    pub(crate) fn for_crs(info: &CrsInfo) -> Option<Self> {
        let def = crs_definitions::from_code(epsg_code(info.identifier.as_deref()?)?)?;
        // GeoParquet coordinates are always x/y (easting or longitude first),
        // whatever axis order the CRS itself declares.
        let proj4: String = def
            .proj4
            .split_whitespace()
            .filter(|p| !p.starts_with("+axis="))
            .collect::<Vec<_>>()
            .join(" ");
        let src = Proj::from_proj_string(&proj4).ok()?;
        if src.is_geocent() || !src.has_inverse() {
            return None;
        }
        let dst = Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs").ok()?;
        Some(Self {
            src_degrees: src.is_latlong(),
            src,
            dst,
        })
    }

    /// One coordinate in degrees; NaN where the transform fails, which the
    /// reader then drops as a non-finite geometry.
    #[inline]
    pub(crate) fn point(&self, x: f64, y: f64) -> (f64, f64) {
        let mut p = if self.src_degrees {
            (x.to_radians(), y.to_radians(), 0.0)
        } else {
            (x, y, 0.0)
        };
        match proj4rs::transform::transform(&self.src, &self.dst, &mut p) {
            Ok(()) => (p.0.to_degrees(), p.1.to_degrees()),
            Err(_) => (f64::NAN, f64::NAN),
        }
    }

    /// A lon/lat box holding the image of a source-CRS box, for row-group
    /// pruning; `None` (keep the group) when the image crosses the
    /// antimeridian, holds a pole or leaves the projection's domain.
    ///
    /// Each edge is bisected until its image is straight to within ~1 cm, so
    /// an extremum between samples is found; the box is padded by what is
    /// left where the bisection stopped.
    pub(crate) fn bbox(&self, [xmin, ymin, xmax, ymax]: [f64; 4]) -> Option<[f64; 4]> {
        // An inverted box wraps the antimeridian; this transform does not split it.
        if xmin > xmax || ymin > ymax {
            return None;
        }
        let mut acc = Hull {
            b: [
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ],
            slack: 0.0,
        };
        let corners = [(xmin, ymin), (xmax, ymin), (xmax, ymax), (xmin, ymax)];
        for k in 0..4 {
            let (a, b) = (corners[k], corners[(k + 1) % 4]);
            self.edge(
                a,
                self.point(a.0, a.1),
                b,
                self.point(b.0, b.1),
                0,
                &mut acc,
            )?;
        }
        let b = acc.b;
        if b[2] - b[0] > 180.0 {
            return None;
        }
        let pad = acc.slack + 1e-9;
        Some([b[0] - pad, b[1] - pad, b[2] + pad, b[3] + pad])
    }

    /// Add the image of the edge `a`..`b` (excluding `b`) to `acc`.
    fn edge(
        &self,
        a: (f64, f64),
        pa: (f64, f64),
        b: (f64, f64),
        pb: (f64, f64),
        depth: u8,
        acc: &mut Hull,
    ) -> Option<()> {
        if !(pa.0.is_finite() && pa.1.is_finite() && pb.0.is_finite() && pb.1.is_finite()) {
            return None;
        }
        acc.b = [
            acc.b[0].min(pa.0),
            acc.b[1].min(pa.1),
            acc.b[2].max(pa.0),
            acc.b[3].max(pa.1),
        ];
        let m = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        let pm = self.point(m.0, m.1);
        let off = (pm.0 - (pa.0 + pb.0) / 2.0)
            .abs()
            .max((pm.1 - (pa.1 + pb.1) / 2.0).abs());
        if !off.is_finite() {
            return None;
        }
        if depth < 4 || (off > 1e-7 && depth < 12) {
            self.edge(a, pa, m, pm, depth + 1, acc)?;
            return self.edge(m, pm, b, pb, depth + 1, acc);
        }
        acc.slack = acc.slack.max(2.0 * off);
        Some(())
    }

    /// `batch` with its `column` geometries in lon/lat, in the same encoding.
    pub(crate) fn batch(
        &self,
        batch: RecordBatch,
        column: &str,
    ) -> Result<RecordBatch, ArrowError> {
        let Ok(i) = batch.schema().index_of(column) else {
            return Ok(batch);
        };
        let mut columns = batch.columns().to_vec();
        columns[i] = self.array(&columns[i])?;
        RecordBatch::try_new(batch.schema(), columns)
    }

    fn array(&self, a: &ArrayRef) -> Result<ArrayRef, ArrowError> {
        Ok(match a.data_type() {
            DataType::Binary => Arc::new(self.wkb_array(a.as_binary::<i32>())),
            DataType::LargeBinary => Arc::new(self.wkb_array(a.as_binary::<i64>())),
            DataType::BinaryView => Arc::new(
                a.as_binary_view()
                    .iter()
                    .map(|v| {
                        v.map(|v| {
                            let mut v = v.to_vec();
                            self.wkb_value(&mut v, &mut Vec::new());
                            v
                        })
                    })
                    .collect::<BinaryViewArray>(),
            ),
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => {
                return Err(ArrowError::NotYetImplemented(
                    "reprojecting a WKT geometry column; reproject the input to EPSG:4326 with \
                     gpio first"
                        .into(),
                ))
            }
            _ => make_array(self.native(a.to_data())?),
        })
    }

    fn wkb_array<O: OffsetSizeTrait>(&self, a: &GenericBinaryArray<O>) -> GenericBinaryArray<O> {
        let mut values = a.values().to_vec();
        let mut coords = Vec::new();
        for w in a.offsets().windows(2) {
            let value = &mut values[w[0].as_usize()..w[1].as_usize()];
            if !value.is_empty() {
                self.wkb_value(value, &mut coords);
            }
        }
        GenericBinaryArray::new(
            a.offsets().clone(),
            Buffer::from_vec(values),
            a.nulls().cloned(),
        )
    }

    /// Rewrite one WKB value's coordinates in place. A value the shared
    /// WKB walk rejects gets an invalid byte-order byte, so the reader
    /// rejects it as malformed instead of reading projected coordinates as
    /// degrees.
    fn wkb_value(&self, value: &mut [u8], coords: &mut Vec<(usize, bool)>) {
        if crate::wkb_column::wkb_coordinates(value, coords).is_err() {
            if let Some(b) = value.first_mut() {
                *b = 0xFF;
            }
            return;
        }
        for &(at, le) in coords.iter() {
            let read = |at: usize| {
                let b: [u8; 8] = value[at..at + 8].try_into().expect("walked");
                if le {
                    f64::from_le_bytes(b)
                } else {
                    f64::from_be_bytes(b)
                }
            };
            let (x, y) = self.point(read(at), read(at + 8));
            for (at, v) in [(at, x), (at + 8, y)] {
                let b = if le { v.to_le_bytes() } else { v.to_be_bytes() };
                value[at..at + 8].copy_from_slice(&b);
            }
        }
    }

    /// Native GeoArrow: transform each coordinate buffer (interleaved
    /// `FixedSizeList<f64>` or separated `x`/`y` struct), keep the layout.
    fn native(&self, d: ArrayData) -> Result<ArrayData, ArrowError> {
        let children = match d.data_type() {
            DataType::FixedSizeList(f, n) if *f.data_type() == DataType::Float64 => {
                if *n < 2 {
                    return Err(ArrowError::InvalidArgumentError(format!(
                        "a coordinate list of width {n} has no x and y"
                    )));
                }
                let c = Float64Array::from(d.child_data()[0].clone());
                let mut v = c.values().to_vec();
                for xy in v.chunks_exact_mut(*n as usize) {
                    (xy[0], xy[1]) = self.point(xy[0], xy[1]);
                }
                vec![Float64Array::new(v.into(), c.nulls().cloned()).into_data()]
            }
            DataType::Struct(fs)
                if fs.len() >= 2
                    && fs[0].name() == "x"
                    && fs[1].name() == "y"
                    && fs.iter().all(|f| *f.data_type() == DataType::Float64) =>
            {
                let mut children = d.child_data().to_vec();
                let (x, y) = (
                    Float64Array::from(children[0].clone()),
                    Float64Array::from(children[1].clone()),
                );
                let (xs, ys): (Vec<f64>, Vec<f64>) = x
                    .values()
                    .iter()
                    .zip(y.values().iter())
                    .map(|(&x, &y)| self.point(x, y))
                    .unzip();
                children[0] = Float64Array::new(xs.into(), x.nulls().cloned()).into_data();
                children[1] = Float64Array::new(ys.into(), y.nulls().cloned()).into_data();
                children
            }
            _ => d
                .child_data()
                .iter()
                .map(|c| self.native(c.clone()))
                .collect::<Result<_, _>>()?,
        };
        d.into_builder().child_data(children).build()
    }
}

/// The bbox [`ToLonLat::bbox`] grows, and its padding.
struct Hull {
    b: [f64; 4],
    slack: f64,
}

#[cfg(test)]
mod tests;
