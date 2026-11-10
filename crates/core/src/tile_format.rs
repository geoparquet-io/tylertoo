//! The tile format an export writes (#823): Mapbox Vector Tiles, or
//! MapLibre Tiles converted from them.
//!
//! Every tile is encoded as MVT first. [`finish_tile`] converts it to MLT
//! with `mlt-core`, before the size caps measure it and before gzip, so
//! both pass-2 engines, the size valve and sharded builds see MLT bytes
//! without knowing about the format. The readers (`stats --layers`,
//! `decode`) convert an MLT tile back to MVT with [`mlt_to_mvt_layers`].

use crate::pmtiles_writer::TileType;

/// Tile encoding for tiles, export and pyramid, MVT by default.
/// MLT needs MapLibre GL JS 5.12 or later, or MapLibre Native.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum TileFormat {
    /// Mapbox Vector Tiles (PMTiles tile type 1).
    #[default]
    Mvt,
    /// MapLibre Tiles (PMTiles tile type 6), converted from the MVT.
    Mlt,
}

impl TileFormat {
    /// The value as the CLI spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mvt => "mvt",
            Self::Mlt => "mlt",
        }
    }

    /// The PMTiles header tile type for this format.
    pub fn tile_type(self) -> TileType {
        match self {
            Self::Mvt => TileType::Mvt,
            Self::Mlt => TileType::Mlt,
        }
    }
}

/// MLT encoding settings, accepted only for MLT output.
/// Defaults to [`Self::Web`]; the other variants use mlt-core's profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MltProfile {
    /// Compatibility with GL JS before 6.0 and its @maplibre/mlt 1.1
    /// decoder: no `FastPFOR`, `FSST` or shared dictionaries. Integer columns
    /// wider than 32 bits become doubles when every value is exact
    /// (absolute value at most 2^53), since GL JS reads 64-bit integers as
    /// `BigInt` and style expressions do not compare `BigInt` with numbers.
    #[default]
    Web,
    /// mlt-core's MapLibre Native profile: every codec, and stored
    /// polygon triangles where they pay.
    Native,
    /// mlt-core's smallest-tile profile.
    MinSize,
    /// mlt-core's profile for small binary diffs between releases.
    Diffable,
    /// mlt-core's quickest-encode profile.
    FastEncoding,
}

impl MltProfile {
    /// The value as the CLI spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Native => "native",
            Self::MinSize => "min-size",
            Self::Diffable => "diffable",
            Self::FastEncoding => "fast-encoding",
        }
    }
}

macro_rules! spelled_enum {
    ($ty:ident, $what:literal, [$($variant:ident),*]) => {
        impl std::fmt::Display for $ty {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl std::str::FromStr for $ty {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                [$(Self::$variant),*]
                    .into_iter()
                    .find(|v| v.as_str() == s)
                    .ok_or_else(|| {
                        let all: Vec<_> = [$(Self::$variant),*].iter().map(|v| v.as_str()).collect();
                        format!(concat!("unknown ", $what, " {:?}: expected {}"), s, all.join(", "))
                    })
            }
        }
    };
}
spelled_enum!(TileFormat, "tile format", [Mvt, Mlt]);
spelled_enum!(
    MltProfile,
    "MLT profile",
    [Web, Native, MinSize, Diffable, FastEncoding]
);

/// Whether this build can write and read MLT (the `mlt` cargo feature).
pub(crate) const MLT_SUPPORTED: bool = cfg!(feature = "mlt");

/// The finished tile bytes for `format`: the MVT itself, or its MLT.
///
/// # Panics
///
/// Panics if the MVT cannot be converted, which validation rules out: the
/// export refuses MLT in a build without the `mlt` feature and an empty
/// layer name, and tylertoo's own MVT always parses.
// shortcut: panics rather than threading a Result through every build_mvt
// caller; upgrade if mlt-core gains a failure tylertoo's MVT can trigger.
pub(crate) fn finish_tile(mvt: Vec<u8>, format: TileFormat, profile: MltProfile) -> Vec<u8> {
    match format {
        TileFormat::Mvt => mvt,
        TileFormat::Mlt => mvt_to_mlt(&mvt, profile)
            .unwrap_or_else(|e| panic!("tylertoo's MVT failed to convert to MLT: {e}")),
    }
}

#[cfg(not(feature = "mlt"))]
fn mvt_to_mlt(_: &[u8], _: MltProfile) -> Result<Vec<u8>, String> {
    Err(NO_MLT.to_owned())
}

#[cfg(not(feature = "mlt"))]
const NO_MLT: &str = "this build has no MLT support (the `mlt` cargo feature)";

/// The layers of an uncompressed MLT tile, each re-encoded as a one-layer
/// MVT, with the layer's MLT length on the wire. A layer of a kind this
/// build cannot read is skipped.
///
/// # Errors
///
/// A tile that does not parse as MLT, or a build without the `mlt` feature.
#[cfg(not(feature = "mlt"))]
pub(crate) fn mlt_to_mvt_layers(_: &[u8]) -> Result<Vec<(Vec<u8>, u64)>, String> {
    Err(NO_MLT.to_owned())
}

#[cfg(feature = "mlt")]
fn mvt_to_mlt(mvt: &[u8], profile: MltProfile) -> Result<Vec<u8>, String> {
    use mlt_core::encoder::{EncoderConfig, Network, Profile};
    let config = match profile {
        // GL JS before 6.0 decodes with @maplibre/mlt 1.1, which reads no
        // FastPFOR, FSST or shared dictionaries (#823).
        MltProfile::Web => EncoderConfig::from_profile(Profile::Web(Network::default()))
            .with_fastpfor(false)
            .with_fsst(false)
            .with_shared_dict(false),
        MltProfile::Native => EncoderConfig::from_profile(Profile::Native(Network::default())),
        MltProfile::MinSize => EncoderConfig::from_profile(Profile::MinSize),
        MltProfile::Diffable => EncoderConfig::from_profile(Profile::Diffable),
        MltProfile::FastEncoding => EncoderConfig::from_profile(Profile::FastEncoding),
    };
    let mut out = Vec::new();
    for layer in mlt_core::mvt::mvt_to_tile_layers(mvt).map_err(|e| e.to_string())? {
        let layer = match profile {
            MltProfile::Web => doubles_for_gl_js(layer).map_err(|e| e.to_string())?,
            _ => layer,
        };
        out.extend(layer.encode(config).map_err(|e| e.to_string())?);
    }
    Ok(out)
}

/// `layer` with each integer column the encoder would store as 64-bit (a
/// value outside both `i32` and `u32` ranges) turned into doubles, when
/// every value is exact in one (|v| <= 2^53). GL JS reads a 64-bit integer
/// as a `BigInt`, which style expressions do not compare with numbers.
#[cfg(feature = "mlt")]
fn doubles_for_gl_js(layer: mlt_core::TileLayer) -> mlt_core::MltResult<mlt_core::TileLayer> {
    use mlt_core::{PropKind, PropValue};
    const EXACT: i128 = 1 << 53;
    let int = |v: &PropValue| match v {
        PropValue::I64(v) => v.map(i128::from),
        PropValue::U64(v) => v.map(i128::from),
        _ => None,
    };
    let widen: Vec<bool> = (0..layer.property_kinds().len())
        .map(|col| {
            if !matches!(layer.property_kinds()[col], PropKind::I64 | PropKind::U64) {
                return false;
            }
            let values = || {
                layer
                    .features()
                    .iter()
                    .filter_map(|f| int(&f.properties()[col]))
            };
            let (Some(min), Some(max)) = (values().min(), values().max()) else {
                return false;
            };
            let fits_32 = (min >= 0 && max <= i128::from(u32::MAX))
                || (min >= i128::from(i32::MIN) && max <= i128::from(i32::MAX));
            !fits_32 && min >= -EXACT && max <= EXACT
        })
        .collect();
    if !widen.contains(&true) {
        return Ok(layer);
    }
    let mut builder = layer.builder_like();
    let keys = layer
        .property_names()
        .iter()
        .zip(layer.property_kinds())
        .zip(&widen)
        .map(|((name, &kind), &w)| builder.add_property(name, if w { PropKind::F64 } else { kind }))
        .collect::<mlt_core::MltResult<Vec<_>>>()?;
    for feature in layer.features() {
        let mut row = builder.feature_like(feature);
        for ((value, &key), &w) in feature.properties().iter().zip(&keys).zip(&widen) {
            #[expect(clippy::cast_precision_loss, reason = "every value is within 2^53")]
            let value = if w {
                PropValue::F64(int(value).map(|v| v as f64))
            } else {
                value.clone()
            };
            row.property(key, value)?;
        }
        row.finish()?;
    }
    Ok(builder.finish())
}

#[cfg(feature = "mlt")]
pub(crate) fn mlt_to_mvt_layers(mlt: &[u8]) -> Result<Vec<(Vec<u8>, u64)>, String> {
    let (mut parser, mut decoder) = (mlt_core::Parser::default(), mlt_core::Decoder::default());
    let mut layers = Vec::new();
    for layer in parser.parse_layers(mlt).map_err(|e| e.to_string())? {
        // The size varint holds tag + body; the wire length adds the varint.
        let size = layer.bytes().map_or(0, |b| u64::from(b.size()));
        let wire = size + prost::encoding::encoded_len_varint(size) as u64;
        if let Some(layer) = layer.into_tile(&mut decoder).map_err(|e| e.to_string())? {
            let mvt = mlt_core::mvt::tile_layers_to_mvt(vec![layer]).map_err(|e| e.to_string())?;
            layers.push((mvt, wire));
        }
    }
    Ok(layers)
}

/// An uncompressed MLT tile as one MVT, every layer in order.
///
/// # Errors
///
/// As [`mlt_to_mvt_layers`].
pub(crate) fn mlt_to_mvt(mlt: &[u8]) -> Result<Vec<u8>, String> {
    // An MVT tile is a repeated `layers` field, so concatenating one-layer
    // tiles is concatenating their layers.
    Ok(mlt_to_mvt_layers(mlt)?
        .into_iter()
        .flat_map(|(mvt, _)| mvt)
        .collect())
}

#[cfg(all(test, feature = "mlt"))]
mod tests {
    use super::*;
    use mlt_core::geo_types::{Geometry, Point};
    use mlt_core::mvt::{mvt_to_tile_layers, tile_layers_to_mvt};
    use mlt_core::{PropKind, PropValue, TileLayer};

    /// A one-layer MVT whose features carry `values` in one column `v`.
    fn mvt_with(values: &[PropValue]) -> Vec<u8> {
        let kind = values[0].kind();
        let mut b = TileLayer::builder("l", 4096).unwrap();
        let key = b.add_property("v", kind).unwrap();
        for (i, v) in values.iter().enumerate() {
            let mut f = b.feature(Geometry::Point(Point::new(i as i32, 2 * i as i32)));
            f.id(Some(i as u64 + 1));
            f.property(key, v.clone()).unwrap();
            f.finish().unwrap();
        }
        tile_layers_to_mvt(vec![b.finish()]).unwrap()
    }

    fn mlt_column(mvt: &[u8], profile: MltProfile) -> (PropKind, Vec<PropValue>) {
        let mlt = finish_tile(mvt.to_vec(), TileFormat::Mlt, profile);
        let back = mvt_to_tile_layers(mlt_to_mvt(&mlt).unwrap()).unwrap();
        let layer = &back[0];
        let kind = layer.property_kinds()[0];
        (
            kind,
            layer
                .features()
                .iter()
                .map(|f| f.properties()[0].clone())
                .collect(),
        )
    }

    #[test]
    fn mvt_is_passed_through_untouched() {
        let mvt = mvt_with(&[PropValue::I64(Some(1))]);
        assert_eq!(
            finish_tile(mvt.clone(), TileFormat::Mvt, MltProfile::Web),
            mvt
        );
    }

    #[test]
    fn mlt_round_trips_geometry_ids_and_values() {
        let mvt = mvt_with(&[
            PropValue::Str(Some("a".into())),
            PropValue::Str(None),
            PropValue::Str(Some("c".into())),
        ]);
        let mlt = finish_tile(mvt.clone(), TileFormat::Mlt, MltProfile::Web);
        assert_ne!(mlt, mvt);
        assert_eq!(
            mvt_to_tile_layers(mlt_to_mvt(&mlt).unwrap()).unwrap(),
            mvt_to_tile_layers(&mvt).unwrap()
        );
    }

    #[test]
    fn web_profile_stores_wide_exact_integers_as_doubles() {
        let mvt = mvt_with(&[
            PropValue::I64(Some(5_000_000_000)),
            PropValue::I64(Some(-3)),
        ]);
        let (kind, values) = mlt_column(&mvt, MltProfile::Web);
        assert_eq!(kind, PropKind::F64);
        assert_eq!(
            values,
            [PropValue::F64(Some(5e9)), PropValue::F64(Some(-3.0))]
        );
        // Native keeps the 64-bit column.
        assert_eq!(mlt_column(&mvt, MltProfile::Native).0, PropKind::I64);
    }

    #[test]
    fn web_profile_keeps_integers_beyond_2_pow_53() {
        let mvt = mvt_with(&[PropValue::I64(Some((1 << 53) + 1)), PropValue::I64(Some(0))]);
        assert_eq!(mlt_column(&mvt, MltProfile::Web).0, PropKind::I64);
    }

    #[test]
    fn layer_lengths_are_the_mlt_wire_lengths() {
        let a = mvt_with(&[PropValue::Bool(Some(true))]);
        let mlt = finish_tile(a, TileFormat::Mlt, MltProfile::Web);
        let layers = mlt_to_mvt_layers(&mlt).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].1, mlt.len() as u64);
    }

    #[test]
    fn spellings_round_trip() {
        for f in [TileFormat::Mvt, TileFormat::Mlt] {
            assert_eq!(f.as_str().parse::<TileFormat>().unwrap(), f);
        }
        assert!("pbf"
            .parse::<TileFormat>()
            .unwrap_err()
            .contains("mvt, mlt"));
        assert_eq!(
            "min-size".parse::<MltProfile>().unwrap(),
            MltProfile::MinSize
        );
    }
}
