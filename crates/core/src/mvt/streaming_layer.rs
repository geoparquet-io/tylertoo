//! Serialize features and table entries as they arrive instead of retaining
//! their protobuf object graph. The shared tag index owns interning.
use std::borrow::Cow;

use prost::encoding::{encode_key, encode_varint, encoded_len_varint, message, uint32, WireType};

use super::tag_index::TagIndex;
use super::{EncodedFeatureGeometry, Feature, PropertyValue};

pub(crate) struct StreamingLayerBuilder<'a> {
    name: &'a str,
    extent: u32,
    index: TagIndex<'a>,
    features: Vec<u8>,
    keys: Vec<u8>,
    values: Vec<u8>,
    count: usize,
    unencodable: usize,
    quantized: usize,
}

fn bytes_field(tag: u32, bytes: &[u8], out: &mut Vec<u8>) {
    encode_key(tag, WireType::LengthDelimited, out);
    encode_varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

impl<'a> StreamingLayerBuilder<'a> {
    pub(crate) fn new(name: &'a str, extent: u32) -> Self {
        Self {
            name,
            extent,
            index: TagIndex::default(),
            features: Vec::new(),
            keys: Vec::new(),
            values: Vec::new(),
            count: 0,
            unencodable: 0,
            quantized: 0,
        }
    }

    pub(crate) fn add_encoded_feature<K: AsRef<str>>(
        &mut self,
        id: Option<u64>,
        geometry: EncodedFeatureGeometry,
        properties: &'a [(K, PropertyValue)],
    ) {
        match geometry {
            EncodedFeatureGeometry::Parts(parts) => {
                let tags = self.index.tags(
                    properties,
                    Cow::Borrowed,
                    |key| bytes_field(3, key.as_bytes(), &mut self.keys),
                    |value| message::encode(4, &value.to_mvt_value(), &mut self.values),
                );
                for (commands, kind) in parts {
                    let feature = Feature {
                        id,
                        tags: tags.clone(),
                        r#type: Some(kind as i32),
                        geometry: commands,
                    };
                    message::encode(2, &feature, &mut self.features);
                    self.count += 1;
                }
            }
            EncodedFeatureGeometry::Unencodable => self.unencodable += 1,
            EncodedFeatureGeometry::Quantized => self.quantized += 1,
        }
    }

    pub(crate) fn feature_count(&self) -> usize {
        self.count
    }
    pub(crate) fn dropped_features(&self) -> usize {
        self.unencodable
    }
    pub(crate) fn quantized_features(&self) -> usize {
        self.quantized
    }

    pub(crate) fn build(self) -> Vec<u8> {
        let Self {
            name,
            extent,
            index,
            features,
            keys,
            values,
            ..
        } = self;
        // The lookup tables are no longer needed. Release them before the
        // final contiguous tile buffer overlaps the serialized layer buffers.
        drop(index);
        let name_len = 1 + encoded_len_varint(name.len() as u64) + name.len();
        let layer_len = name_len
            + features.len()
            + keys.len()
            + values.len()
            + uint32::encoded_len(5, &extent)
            + uint32::encoded_len(15, &2);
        let mut out = Vec::with_capacity(1 + encoded_len_varint(layer_len as u64) + layer_len);
        encode_key(3, WireType::LengthDelimited, &mut out);
        encode_varint(layer_len as u64, &mut out);
        // prost emits fields in tag order, including required/default fields:
        // name(1), features(2), keys(3), values(4), extent(5), version(15).
        bytes_field(1, name.as_bytes(), &mut out);
        out.extend_from_slice(&features);
        out.extend_from_slice(&keys);
        out.extend_from_slice(&values);
        uint32::encode(5, &extent, &mut out);
        uint32::encode(15, &2, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{LayerBuilder, TileBuilder};
    use crate::vector_tile::tile::GeomType;
    use prost::Message;

    #[test]
    fn serialized_layer_is_byte_identical_to_materialized_layer() {
        let props: Vec<_> = vec![
            PropertyValue::String("same".into()),
            PropertyValue::UInt(u64::MAX),
            PropertyValue::Int(-1),
            PropertyValue::Double(-0.0),
            PropertyValue::Double(0.0),
            PropertyValue::Double(f64::from_bits(0x7ff8_0000_0000_0042)),
            PropertyValue::Double(f64::NAN),
            PropertyValue::Float(f32::NAN),
            PropertyValue::Bool(false),
            PropertyValue::Bool(true),
        ]
        .into_iter()
        .map(|v| {
            vec![
                ("key".to_owned(), v),
                ("repeat".to_owned(), PropertyValue::String("same".into())),
            ]
        })
        .collect();
        for name in ["", "layer"] {
            for extent in [0, 4096] {
                let mut materialized = LayerBuilder::new(name).with_extent(extent);
                let mut serialized = StreamingLayerBuilder::new(name, extent);
                for (i, p) in props.iter().enumerate() {
                    let parts = vec![
                        (vec![9, 2, 2], GeomType::Point),
                        (vec![9, 4, 4], GeomType::Point),
                    ];
                    let id = (i % 2 == 0).then_some(i as u64);
                    materialized.add_encoded_feature(
                        id,
                        EncodedFeatureGeometry::Parts(parts.clone()),
                        p,
                    );
                    serialized.add_encoded_feature(id, EncodedFeatureGeometry::Parts(parts), p);
                }
                for quantized in [false, true] {
                    let geom = || {
                        if quantized {
                            EncodedFeatureGeometry::Quantized
                        } else {
                            EncodedFeatureGeometry::Unencodable
                        }
                    };
                    materialized.add_encoded_feature(None, geom(), &props[0]);
                    serialized.add_encoded_feature(None, geom(), &props[0]);
                }
                assert_eq!(serialized.feature_count(), materialized.feature_count());
                assert_eq!(
                    serialized.dropped_features(),
                    materialized.dropped_features()
                );
                assert_eq!(
                    serialized.quantized_features(),
                    materialized.quantized_features()
                );
                let mut tile = TileBuilder::new();
                tile.add_layer(materialized.build());
                assert_eq!(serialized.build(), tile.build().encode_to_vec());
            }
        }
    }
}
