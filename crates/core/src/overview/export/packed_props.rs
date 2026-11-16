//! Process-private packed property rows.
pub(super) mod arrow;
use std::sync::Arc;

use crate::mvt::PropertyRef;
#[cfg(test)]
use crate::mvt::PropertyValue;

/// Each chunk has its own name table (deduplicated by content in the spill);
/// only `data` is allocated per row.
/// Each schema column has a tag (0 = absent). Text lengths are fixed u32s; numeric payloads preserve their exact little-endian bits.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PackedProps {
    // Private: the trusted scan walks one tag per name, so a row must never
    // be re-paired with a table of another width.
    names: Arc<[Arc<str>]>,
    data: Arc<[u8]>,
}

impl PackedProps {
    #[cfg(test)]
    pub(super) fn pack(
        names: Arc<[Arc<str>]>,
        values: impl IntoIterator<Item = Option<PropertyValue>>,
    ) -> Self {
        let mut data = Vec::new();
        for value in values {
            append_value(&mut data, value.as_ref().map(PropertyValue::as_ref)).unwrap();
        }
        Self {
            names,
            data: data.into(),
        }
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub(super) fn names(&self) -> &Arc<[Arc<str>]> {
        &self.names
    }

    /// The only constructor for bytes read from the process-private spill.
    pub(super) fn from_spill(
        names: Arc<[Arc<str>]>,
        data: Arc<[u8]>,
    ) -> Result<Self, super::ExportError> {
        let row = Self { names, data };
        row.validate()?;
        Ok(row)
    }

    pub(super) fn iter(&self) -> PackedIter<'_> {
        PackedIter {
            names: self.names.iter(),
            bytes: &self.data,
        }
    }

    pub(super) fn validate(&self) -> Result<(), super::ExportError> {
        self.checked_iter().try_for_each(|p| p.map(|_| ()))
    }

    fn checked_iter(
        &self,
    ) -> impl Iterator<Item = Result<(&str, PropertyRef<'_>), super::ExportError>> {
        let mut bytes = &*self.data;
        let mut names = self.names.iter();
        let mut done = false;
        std::iter::from_fn(move || {
            if done {
                return None;
            }
            let result = (|| {
                for name in names.by_ref() {
                    let tag = take(&mut bytes, 1)?[0];
                    let value = match tag {
                        0 => continue,
                        1 => {
                            let len = u32::from_le_bytes(take(&mut bytes, 4)?.try_into().unwrap())
                                as usize;
                            PropertyRef::String(
                                std::str::from_utf8(take(&mut bytes, len)?)
                                    .map_err(|_| super::spill_corrupt())?,
                            )
                        }
                        2 => PropertyRef::Float(f32::from_bits(u32::from_le_bytes(
                            take(&mut bytes, 4)?.try_into().unwrap(),
                        ))),
                        3 => PropertyRef::Double(f64::from_bits(u64::from_le_bytes(
                            take(&mut bytes, 8)?.try_into().unwrap(),
                        ))),
                        4 => PropertyRef::Int(i64::from_le_bytes(
                            take(&mut bytes, 8)?.try_into().unwrap(),
                        )),
                        5 => PropertyRef::UInt(u64::from_le_bytes(
                            take(&mut bytes, 8)?.try_into().unwrap(),
                        )),
                        6 => match take(&mut bytes, 1)?[0] {
                            0 => PropertyRef::Bool(false),
                            1 => PropertyRef::Bool(true),
                            _ => return Err(super::spill_corrupt()),
                        },
                        _ => return Err(super::spill_corrupt()),
                    };
                    return Ok(Some((&**name, value)));
                }
                done = true;
                if !bytes.is_empty() {
                    return Err(super::spill_corrupt());
                }
                Ok(None)
            })();
            match result {
                Ok(v) => v.map(Ok),
                Err(e) => {
                    done = true;
                    Some(Err(e))
                }
            }
        })
        .take(self.names.len() + 1)
    }

    #[cfg(test)]
    pub(super) fn new(props: Vec<(Arc<str>, PropertyValue)>) -> Self {
        let names = props
            .iter()
            .map(|(k, _)| k.clone())
            .collect::<Vec<_>>()
            .into();
        Self::pack(names, props.into_iter().map(|(_, v)| Some(v)))
    }

    #[cfg(test)]
    pub(super) fn owned(&self) -> Vec<(Arc<str>, PropertyValue)> {
        self.iter()
            .map(|(k, v)| (Arc::from(k), v.to_owned()))
            .collect()
    }
}

/// A single forward scan, with no fallible iterator adapter or varint decode.
/// Construction and spill reload already established the wire invariants.
pub(super) struct PackedIter<'a> {
    names: std::slice::Iter<'a, Arc<str>>,
    bytes: &'a [u8],
}

impl<'a> PackedIter<'a> {
    #[inline]
    fn fixed<const N: usize>(&mut self) -> [u8; N] {
        let (value, tail) = self.bytes.split_at(N);
        self.bytes = tail;
        value.try_into().unwrap()
    }
}

impl<'a> Iterator for PackedIter<'a> {
    type Item = (&'a str, PropertyRef<'a>);
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let name = self.names.next()?;
            let tag = self.fixed::<1>()[0];
            let value = match tag {
                0 => continue,
                1 => {
                    let len = u32::from_le_bytes(self.fixed()) as usize;
                    let (text, tail) = self.bytes.split_at(len);
                    self.bytes = tail;
                    // SAFETY: private immutable bytes come only from typed
                    // writers or from_spill's complete validation. This scan
                    // reads the same fixed lengths used at that boundary.
                    PropertyRef::String(unsafe { trusted_text(text) })
                }
                2 => PropertyRef::Float(f32::from_bits(u32::from_le_bytes(self.fixed()))),
                3 => PropertyRef::Double(f64::from_bits(u64::from_le_bytes(self.fixed()))),
                4 => PropertyRef::Int(i64::from_le_bytes(self.fixed())),
                5 => PropertyRef::UInt(u64::from_le_bytes(self.fixed())),
                6 => PropertyRef::Bool(self.fixed::<1>()[0] != 0),
                _ => unreachable!("validated packed tag"),
            };
            return Some((name, value));
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.names.len()))
    }
}

/// Shared typed wire writer; strings enter as valid UTF-8.
fn append_value(
    data: &mut Vec<u8>,
    value: Option<PropertyRef<'_>>,
) -> Result<(), super::ExportError> {
    match value {
        None => data.push(0),
        Some(PropertyRef::String(s)) => {
            let len = wire_len(s.len())?;
            data.push(1);
            data.extend_from_slice(&len.to_le_bytes());
            data.extend_from_slice(s.as_bytes());
        }
        Some(PropertyRef::Float(v)) => {
            data.push(2);
            data.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        Some(PropertyRef::Double(v)) => {
            data.push(3);
            data.extend_from_slice(&v.to_bits().to_le_bytes());
        }
        Some(PropertyRef::Int(v)) => {
            data.push(4);
            data.extend_from_slice(&v.to_le_bytes());
        }
        Some(PropertyRef::UInt(v)) => {
            data.push(5);
            data.extend_from_slice(&v.to_le_bytes());
        }
        Some(PropertyRef::Bool(v)) => {
            data.push(6);
            data.push(u8::from(v));
        }
    }
    Ok(())
}

/// A text or row length as the u32 the wire carries. A longer value is
/// reachable only from a multi-gigabyte `LargeUtf8` cell or nested value, and
/// must fail the export rather than panic or wrap.
pub(super) fn wire_len(len: usize) -> Result<u32, super::ExportError> {
    u32::try_from(len).map_err(|_| {
        super::ExportError::Arrow(arrow_schema::ArrowError::InvalidArgumentError(format!(
            "a property value of {len} bytes exceeds the 4 GiB the export can carry"
        )))
    })
}

/// Borrow text from an immutable row after its construction boundary checked it.
///
/// # Safety
/// `bytes` must be exactly a string payload written from a valid `&str`, or
/// checked by `PackedProps::from_spill`, and must not have changed since.
#[inline]
unsafe fn trusted_text(bytes: &[u8]) -> &str {
    // SAFETY: the caller carries the row's UTF-8 construction/validation proof.
    unsafe { std::str::from_utf8_unchecked(bytes) }
}

fn take<'a>(bytes: &mut &'a [u8], n: usize) -> Result<&'a [u8], super::ExportError> {
    if n > bytes.len() {
        return Err(super::spill_corrupt());
    }
    let (v, tail) = bytes.split_at(n);
    *bytes = tail;
    Ok(v)
}
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::mvt::PropertyRef;
    #[cfg(test)]
    use crate::mvt::PropertyValue;

    #[test]
    fn packed_rows_keep_null_positions_and_borrow_text() {
        let names = Arc::from(vec![
            Arc::from("missing"),
            Arc::from("text"),
            Arc::from("n"),
        ]);
        let row = PackedProps::pack(
            names,
            [
                None,
                Some(PropertyValue::String("héllo".into())),
                Some(PropertyValue::Int(-42)),
            ],
        );
        let got: Vec<_> = row.iter().collect();
        assert_eq!(
            got,
            vec![
                ("text", PropertyRef::String("héllo")),
                ("n", PropertyRef::Int(-42))
            ]
        );
        let PropertyRef::String(s) = got[0].1 else {
            panic!()
        };
        let start = row.data.as_ptr() as usize;
        assert!((start..start + row.data.len()).contains(&(s.as_ptr() as usize)));
        assert!(row.data.len() < 2 * 40 + "héllo".len());
    }
    #[test]
    fn packed_validation_rejects_truncation_tags_utf8_and_trailing_bytes() {
        let row = PackedProps::new(vec![
            ("s".into(), PropertyValue::String("x".repeat(128))),
            ("f".into(), PropertyValue::Float(-0.0)),
            (
                "d".into(),
                PropertyValue::Double(f64::from_bits(0x7ff8_0000_0000_0042)),
            ),
            ("i".into(), PropertyValue::Int(i64::MIN)),
            ("u".into(), PropertyValue::UInt(u64::MAX)),
            ("b".into(), PropertyValue::Bool(true)),
        ]);
        row.validate().unwrap();
        for end in 0..row.data.len() {
            let truncated = PackedProps {
                names: row.names.clone(),
                data: Arc::from(&row.data[..end]),
            };
            assert!(
                truncated.validate().is_err(),
                "accepted truncation at {end}"
            );
        }
        for bytes in [
            vec![7],
            vec![1, 1, 0xff],
            vec![1, 0xff],
            vec![6, 2],
            vec![0, 0],
        ] {
            let bad = PackedProps {
                names: Arc::from(vec![Arc::from("s")]),
                data: bytes.into(),
            };
            assert!(bad.validate().is_err());
        }
    }

    #[test]
    fn packed_encoder_keeps_first_seen_keys_values_and_float_bits() {
        use crate::mvt::{
            EncodedFeatureGeometry, LayerBuilder, StreamingLayerBuilder, TileBuilder,
        };
        use crate::vector_tile::tile::GeomType;
        use prost::Message;
        let names: Arc<[Arc<str>]> =
            Arc::from(vec![Arc::from("a"), Arc::from("b"), Arc::from("c")]);
        let values = [
            vec![
                None,
                Some(PropertyValue::String("héllo".into())),
                Some(PropertyValue::Double(-0.0)),
            ],
            vec![
                Some(PropertyValue::String("héllo".into())),
                None,
                Some(PropertyValue::Float(0.0)),
            ],
            vec![
                Some(PropertyValue::Double(f64::NAN)),
                Some(PropertyValue::Float(f32::NAN)),
                Some(PropertyValue::UInt(u64::MAX)),
            ],
        ];
        let packed: Vec<_> = values
            .iter()
            .map(|v| PackedProps::pack(names.clone(), v.clone()))
            .collect();
        let mut materialized = LayerBuilder::new("test");
        let mut streaming = StreamingLayerBuilder::new("test", 4096);
        for (i, row) in packed.iter().enumerate() {
            let geometry = || EncodedFeatureGeometry::Parts(vec![(vec![9, 2, 2], GeomType::Point)]);
            let props: Vec<_> = names
                .iter()
                .zip(&values[i])
                .filter_map(|(n, v)| v.clone().map(|v| (n.clone(), v)))
                .collect();
            materialized.add_encoded_feature(Some(i as u64), geometry(), &props);
            streaming.add_borrowed_feature(Some(i as u64), geometry(), row.iter());
        }
        let mut tile = TileBuilder::new();
        tile.add_layer(materialized.build());
        assert_eq!(streaming.build(), tile.build().encode_to_vec());
    }
    #[test]
    fn spill_reload_validates_utf8_before_trusted_iteration() {
        let names: Arc<[Arc<str>]> = Arc::from(vec![Arc::from("s")]);
        assert!(
            PackedProps::from_spill(names.clone(), Arc::from([1u8, 1, 0, 0, 0, 0xff])).is_err()
        );
        let row = PackedProps::pack(names.clone(), [Some(PropertyValue::String("héllo".into()))]);
        let back = PackedProps::from_spill(names, row.data.clone()).unwrap();
        assert_eq!(
            back.iter().collect::<Vec<_>>(),
            vec![("s", PropertyRef::String("héllo"))]
        );
    }
    #[test]
    fn packed_traversal_uses_fixed_lengths_and_one_schema_scan() {
        let row = PackedProps::pack(
            Arc::from(vec![Arc::from("s"), Arc::from("null"), Arc::from("n")]),
            [
                Some(PropertyValue::String("x".repeat(128))),
                None,
                Some(PropertyValue::UInt(u64::MAX)),
            ],
        );
        assert_eq!(&row.bytes()[..5], &[1, 128, 0, 0, 0]);
        let mut iter = row.iter();
        assert_eq!(iter.size_hint(), (0, Some(3)));
        assert_eq!(
            iter.next(),
            Some(("s", PropertyRef::String(&"x".repeat(128))))
        );
        assert_eq!(iter.next(), Some(("n", PropertyRef::UInt(u64::MAX))));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn wide_sparse_schema_yields_only_present_columns_in_order() {
        let names: Arc<[Arc<str>]> = (0..2000).map(|i| Arc::from(format!("c{i}"))).collect();
        let mut values: Vec<Option<PropertyValue>> = vec![None; 2000];
        values[0] = Some(PropertyValue::Int(1));
        values[1234] = Some(PropertyValue::String("mid".into()));
        values[1999] = Some(PropertyValue::Bool(true));
        let row = PackedProps::pack(names.clone(), values);
        assert_eq!(row.bytes().len(), 2000 + 8 + (4 + 3) + 1);
        let back = PackedProps::from_spill(names, Arc::from(row.bytes())).unwrap();
        assert_eq!(
            back.iter().collect::<Vec<_>>(),
            vec![
                ("c0", PropertyRef::Int(1)),
                ("c1234", PropertyRef::String("mid")),
                ("c1999", PropertyRef::Bool(true)),
            ]
        );
        let empty = PackedProps::pack(Arc::from(Vec::<Arc<str>>::new()), []);
        assert!(empty.bytes().is_empty() && empty.iter().next().is_none());
    }

    #[test]
    fn non_finite_floats_empty_text_and_extremes_keep_their_bits() {
        let names: Arc<[Arc<str>]> = ["e", "f0", "fi", "fn", "d0", "di", "dn", "i", "u"]
            .into_iter()
            .map(Arc::from)
            .collect();
        let nan32 = f32::from_bits(0x7fc0_1234);
        let nan64 = f64::from_bits(0xfff8_0000_dead_beef);
        let row = PackedProps::pack(
            names.clone(),
            [
                Some(PropertyValue::String(String::new())),
                Some(PropertyValue::Float(-0.0)),
                Some(PropertyValue::Float(f32::INFINITY)),
                Some(PropertyValue::Float(nan32)),
                Some(PropertyValue::Double(-0.0)),
                Some(PropertyValue::Double(f64::NEG_INFINITY)),
                Some(PropertyValue::Double(nan64)),
                Some(PropertyValue::Int(i64::MIN)),
                Some(PropertyValue::UInt(u64::MAX)),
            ],
        );
        let back = PackedProps::from_spill(names, Arc::from(row.bytes())).unwrap();
        let got: Vec<_> = back.iter().map(|(_, v)| v).collect();
        assert_eq!(got[0], PropertyRef::String(""));
        let bits = |v: &PropertyRef<'_>| match *v {
            PropertyRef::Float(f) => u64::from(f.to_bits()),
            PropertyRef::Double(d) => d.to_bits(),
            _ => unreachable!(),
        };
        assert_eq!(
            got[1..7].iter().map(bits).collect::<Vec<_>>(),
            [
                (-0.0f32).to_bits() as u64,
                f32::INFINITY.to_bits() as u64,
                nan32.to_bits() as u64,
                (-0.0f64).to_bits(),
                f64::NEG_INFINITY.to_bits(),
                nan64.to_bits(),
            ]
        );
        assert_eq!(got[7], PropertyRef::Int(i64::MIN));
        assert_eq!(got[8], PropertyRef::UInt(u64::MAX));
    }

    /// Every single-byte alteration of a row is an error or a valid row whose
    /// scan stays in bounds; a table of another width never validates
    /// bytes written for this one.
    #[test]
    fn altered_spill_rows_never_panic_and_other_tables_are_rejected() {
        let names: Arc<[Arc<str>]> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .map(Arc::from)
            .collect();
        let row = PackedProps::pack(
            names.clone(),
            [
                Some(PropertyValue::String("héllo".into())),
                None,
                Some(PropertyValue::Double(1.5)),
                Some(PropertyValue::Bool(true)),
                Some(PropertyValue::Int(-1)),
            ],
        );
        for at in 0..row.bytes().len() {
            for value in 0..=255u8 {
                let mut bad = row.bytes().to_vec();
                bad[at] = value;
                if let Ok(ok) = PackedProps::from_spill(names.clone(), bad.into()) {
                    let fast: Vec<_> = ok.iter().collect();
                    let checked: Vec<_> = ok.checked_iter().collect::<Result<_, _>>().unwrap();
                    assert_eq!(format!("{fast:?}"), format!("{checked:?}")); // Debug: NaN != NaN
                }
            }
        }
        for width in [0, 1, 4, 6, 9] {
            let other: Arc<[Arc<str>]> = (0..width).map(|i| Arc::from(format!("n{i}"))).collect();
            assert!(
                PackedProps::from_spill(other, Arc::from(row.bytes())).is_err(),
                "a table of {width} accepted a row written for 5"
            );
        }
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn value() -> impl Strategy<Value = Option<PropertyValue>> {
            prop_oneof![
                Just(None),
                ".{0,40}".prop_map(|s| Some(PropertyValue::String(s))),
                any::<u32>().prop_map(|b| Some(PropertyValue::Float(f32::from_bits(b)))),
                any::<u64>().prop_map(|b| Some(PropertyValue::Double(f64::from_bits(b)))),
                any::<i64>().prop_map(|v| Some(PropertyValue::Int(v))),
                any::<u64>().prop_map(|v| Some(PropertyValue::UInt(v))),
                any::<bool>().prop_map(|v| Some(PropertyValue::Bool(v))),
            ]
        }

        /// The packed codec reproduces any row bit for bit, in schema order,
        /// through the spill's validating constructor.
        fn same(a: &PropertyRef<'_>, b: &PropertyValue) -> bool {
            match (a, b) {
                (PropertyRef::String(x), PropertyValue::String(y)) => *x == y,
                (PropertyRef::Float(x), PropertyValue::Float(y)) => x.to_bits() == y.to_bits(),
                (PropertyRef::Double(x), PropertyValue::Double(y)) => x.to_bits() == y.to_bits(),
                (PropertyRef::Int(x), PropertyValue::Int(y)) => x == y,
                (PropertyRef::UInt(x), PropertyValue::UInt(y)) => x == y,
                (PropertyRef::Bool(x), PropertyValue::Bool(y)) => x == y,
                _ => false,
            }
        }

        proptest! {
            #[test]
            fn packed_rows_round_trip(values in proptest::collection::vec(value(), 0..60)) {
                let names: Arc<[Arc<str>]> =
                    (0..values.len()).map(|i| Arc::from(format!("c{i}"))).collect();
                let row = PackedProps::pack(names.clone(), values.clone());
                let back = PackedProps::from_spill(names, Arc::from(row.bytes())).unwrap();
                let want: Vec<_> = values
                    .iter()
                    .enumerate()
                    .filter_map(|(i, v)| v.as_ref().map(|v| (format!("c{i}"), v)))
                    .collect();
                let got: Vec<_> = back.iter().collect();
                prop_assert_eq!(got.len(), want.len());
                for ((gn, gv), (wn, wv)) in got.iter().zip(&want) {
                    prop_assert_eq!(*gn, wn.as_str());
                    prop_assert!(same(gv, wv));
                }
            }
        }
    }
}
