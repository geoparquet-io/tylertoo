//! Parallel tile encoding for the streaming pipeline.
//!
//! This module provides parallelized tile encoding using rayon for batches
//! of tiles. Small batches are encoded sequentially to avoid rayon overhead.
//!
//! # Architecture
//!
//! ```text
//! PendingTile(s) → [parallel encode via rayon] → EncodedTile(s) → spool
//! ```
//!
//! The encoder supports both MVT (Mapbox Vector Tiles) and MLT (MapLibre Tiles)
//! formats, dispatching to the appropriate encoder based on `TileFormat`.
//!
//! # Example
//!
//! ```no_run
//! use gpq_tiles_core::parallel_encoding::{encode_tiles_smart, PendingTile, TileFeature};
//! use gpq_tiles_core::{TileFormat, tile::TileCoord};
//!
//! let tiles = vec![
//!     PendingTile::new(0, TileCoord::new(0, 0, 0), vec![]),
//! ];
//!
//! let encoded = encode_tiles_smart(tiles, TileFormat::Mvt, "layer", 16);
//! ```

use crate::hierarchical_clip::WorldClippedGeometry;
use crate::mvt::{LayerBuilder, PropertyValue, TileBuilder};
use crate::streaming_types::TileFormat;
use crate::tile::TileCoord;
use crate::wkb::deserialize_properties;
use prost::Message;
use rayon::prelude::*;
use std::collections::HashMap;

/// Default threshold for switching to parallel encoding.
/// Batches smaller than this are encoded sequentially.
pub const DEFAULT_PARALLEL_THRESHOLD: usize = 16;

/// A feature within a tile, ready for encoding.
///
/// Contains the geometry (as WorldClippedGeometry bytes) and properties
/// (as MessagePack bytes) that will be encoded into the tile format.
#[derive(Debug, Clone)]
pub struct TileFeature {
    /// Unique feature ID
    pub feature_id: u64,
    /// WorldClippedGeometry-encoded geometry (clipped to tile bounds)
    /// This is the internal serialization format, NOT standard WKB.
    pub geometry_bytes: Vec<u8>,
    /// MessagePack-serialized properties
    pub properties_msgpack: Vec<u8>,
}

impl TileFeature {
    /// Create a new tile feature.
    ///
    /// # Arguments
    ///
    /// * `feature_id` - Unique identifier for this feature
    /// * `geometry_bytes` - WorldClippedGeometry serialized bytes
    /// * `properties_msgpack` - MessagePack-serialized property map
    pub fn new(feature_id: u64, geometry_bytes: Vec<u8>, properties_msgpack: Vec<u8>) -> Self {
        Self {
            feature_id,
            geometry_bytes,
            properties_msgpack,
        }
    }

    /// Create a tile feature with geometry and properties.
    ///
    /// Convenience method that serializes the geometry and properties.
    pub fn from_geometry(
        feature_id: u64,
        geometry: &WorldClippedGeometry,
        properties: &HashMap<String, crate::wkb::PropertyValue>,
    ) -> Self {
        let geometry_bytes = geometry.to_bytes();
        let properties_msgpack = crate::wkb::serialize_properties(properties).unwrap_or_default();
        Self::new(feature_id, geometry_bytes, properties_msgpack)
    }
}

/// A tile with features ready for encoding.
#[derive(Debug)]
pub struct PendingTile {
    /// PMTiles tile_id (Hilbert-encoded)
    pub tile_id: u64,
    /// Tile coordinates (z/x/y)
    pub coord: TileCoord,
    /// Features to encode into this tile
    pub features: Vec<TileFeature>,
}

impl PendingTile {
    /// Create a new pending tile.
    pub fn new(tile_id: u64, coord: TileCoord, features: Vec<TileFeature>) -> Self {
        Self {
            tile_id,
            coord,
            features,
        }
    }
}

/// An encoded tile ready for writing to the spool.
#[derive(Debug)]
pub struct EncodedTile {
    /// PMTiles tile_id (preserved for ordering)
    pub tile_id: u64,
    /// Encoded tile data (MVT or MLT bytes)
    pub data: Vec<u8>,
    /// Number of features encoded (for stats)
    pub feature_count: usize,
}

impl EncodedTile {
    /// Create a new encoded tile.
    pub fn new(tile_id: u64, data: Vec<u8>, feature_count: usize) -> Self {
        Self {
            tile_id,
            data,
            feature_count,
        }
    }
}

/// Encode a single tile's features into MVT format.
///
/// This is the core MVT encoding function.
fn encode_tile_mvt(pending: &PendingTile, layer_name: &str) -> EncodedTile {
    let mut layer_builder = LayerBuilder::new(layer_name);
    let mut feature_count = 0;

    for feature in &pending.features {
        // Decode geometry from bytes
        let geom = match WorldClippedGeometry::from_bytes(&feature.geometry_bytes) {
            Some(g) => g,
            None => continue, // Skip features with invalid geometry
        };

        // Skip degenerate geometries
        // Using a reasonable extent of 4096 (MVT default)
        if geom.is_degenerate_in_tile(&pending.coord, 4096) {
            continue;
        }

        // Decode properties from MessagePack
        let props: Vec<(String, PropertyValue)> = if feature.properties_msgpack.is_empty() {
            vec![]
        } else {
            match deserialize_properties(&feature.properties_msgpack) {
                Ok(prop_map) => convert_properties(&prop_map),
                Err(_) => vec![], // Skip invalid properties
            }
        };

        // Add feature to layer
        layer_builder.add_feature_world(Some(feature.feature_id), &geom, &props, &pending.coord);
        feature_count += 1;
    }

    // Build the tile
    let layer = layer_builder.build();
    let mut tile_builder = TileBuilder::new();
    tile_builder.add_layer(layer);
    let tile = tile_builder.build();
    let data = tile.encode_to_vec();

    EncodedTile::new(pending.tile_id, data, feature_count)
}

/// Encode a single tile's features into MLT format.
///
/// NOTE: MLT encoding is not yet implemented. This returns an empty tile
/// with a warning. Full MLT support will be added in a future PR.
fn encode_tile_mlt(pending: &PendingTile, _layer_name: &str) -> EncodedTile {
    // TODO: Implement MLT encoding when crate::mlt module is available
    // For now, return an empty tile and log a warning
    tracing::warn!(
        "MLT encoding not yet implemented, tile {} will be empty",
        pending.tile_id
    );
    EncodedTile::new(pending.tile_id, vec![], 0)
}

/// Convert wkb::PropertyValue to mvt::PropertyValue.
///
/// Note: wkb::PropertyValue uses Float(f64) for all floating point,
/// which maps to mvt::PropertyValue::Double(f64).
fn convert_properties(
    props: &HashMap<String, crate::wkb::PropertyValue>,
) -> Vec<(String, PropertyValue)> {
    props
        .iter()
        .map(|(k, v)| {
            let mvt_val = match v {
                crate::wkb::PropertyValue::String(s) => PropertyValue::String(s.clone()),
                crate::wkb::PropertyValue::Int(i) => PropertyValue::Int(*i),
                crate::wkb::PropertyValue::UInt(u) => PropertyValue::UInt(*u),
                // wkb uses Float(f64), map to mvt's Double(f64)
                crate::wkb::PropertyValue::Float(f) => PropertyValue::Double(*f),
                crate::wkb::PropertyValue::Bool(b) => PropertyValue::Bool(*b),
            };
            (k.clone(), mvt_val)
        })
        .collect()
}

/// Encode a single tile's features into the specified format.
///
/// This is the core encoding function that dispatches to MVT or MLT encoders.
pub fn encode_single_tile(
    pending: &PendingTile,
    format: TileFormat,
    layer_name: &str,
) -> EncodedTile {
    match format {
        TileFormat::Mvt => encode_tile_mvt(pending, layer_name),
        TileFormat::Mlt => encode_tile_mlt(pending, layer_name),
    }
}

/// Encode tiles in parallel using rayon.
///
/// Always uses parallel encoding regardless of batch size.
/// For smart batching with threshold, use `encode_tiles_smart`.
pub fn encode_tiles_parallel(
    tiles: Vec<PendingTile>,
    format: TileFormat,
    layer_name: &str,
) -> Vec<EncodedTile> {
    let layer_name = layer_name.to_string(); // Clone for Send
    tiles
        .into_par_iter()
        .map(|pending| encode_single_tile(&pending, format, &layer_name))
        .collect()
}

/// Encode tiles with smart batching.
///
/// Uses parallel encoding for batches >= batch_threshold,
/// sequential encoding for smaller batches to avoid rayon overhead.
///
/// # Arguments
///
/// * `tiles` - Tiles to encode
/// * `format` - Target tile format (MVT or MLT)
/// * `layer_name` - Name of the layer in the output tile
/// * `batch_threshold` - Minimum batch size for parallel encoding
pub fn encode_tiles_smart(
    tiles: Vec<PendingTile>,
    format: TileFormat,
    layer_name: &str,
    batch_threshold: usize,
) -> Vec<EncodedTile> {
    if tiles.len() >= batch_threshold {
        encode_tiles_parallel(tiles, format, layer_name)
    } else {
        // Sequential encoding for small batches
        tiles
            .into_iter()
            .map(|pending| encode_single_tile(&pending, format, layer_name))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world_coord::WorldCoord;

    // ========== Type Tests ==========

    #[test]
    fn test_tile_feature_creation() {
        let feature = TileFeature::new(
            42,
            vec![0x01, 0x02, 0x03], // mock geometry bytes
            vec![0x80],             // msgpack empty map
        );
        assert_eq!(feature.feature_id, 42);
        assert_eq!(feature.geometry_bytes, vec![0x01, 0x02, 0x03]);
        assert_eq!(feature.properties_msgpack, vec![0x80]);
    }

    #[test]
    fn test_tile_feature_from_geometry() {
        let geom = WorldClippedGeometry::Point(WorldCoord {
            x: 1_000_000,
            y: 2_000_000,
        });
        let props = HashMap::new();

        let feature = TileFeature::from_geometry(42, &geom, &props);

        assert_eq!(feature.feature_id, 42);
        assert!(!feature.geometry_bytes.is_empty());
        // Empty props should serialize to msgpack empty map
        assert!(!feature.properties_msgpack.is_empty());
    }

    #[test]
    fn test_pending_tile_creation() {
        let coord = TileCoord::new(10, 20, 5);
        let features = vec![
            TileFeature::new(1, vec![0x01], vec![]),
            TileFeature::new(2, vec![0x02], vec![]),
        ];
        let pending = PendingTile::new(12345, coord, features);

        assert_eq!(pending.tile_id, 12345);
        assert_eq!(pending.coord.z, 5);
        assert_eq!(pending.coord.x, 10);
        assert_eq!(pending.coord.y, 20);
        assert_eq!(pending.features.len(), 2);
    }

    #[test]
    fn test_encoded_tile_creation() {
        let encoded = EncodedTile::new(999, vec![0x0A, 0x0B, 0x0C], 5);
        assert_eq!(encoded.tile_id, 999);
        assert_eq!(encoded.data, vec![0x0A, 0x0B, 0x0C]);
        assert_eq!(encoded.feature_count, 5);
    }

    // ========== Encoding Tests ==========

    #[test]
    fn test_encode_single_tile_empty_mvt() {
        let coord = TileCoord::new(0, 0, 0);
        let pending = PendingTile::new(0, coord, vec![]);
        let encoded = encode_single_tile(&pending, TileFormat::Mvt, "test_layer");

        // Empty tile produces valid MVT (protobuf with empty layer)
        assert!(!encoded.data.is_empty());
        assert_eq!(encoded.tile_id, 0);
        assert_eq!(encoded.feature_count, 0);
    }

    #[test]
    fn test_encode_single_tile_preserves_tile_id() {
        let coord = TileCoord::new(5, 10, 8);
        let pending = PendingTile::new(54321, coord, vec![]);
        let encoded = encode_single_tile(&pending, TileFormat::Mvt, "layer");

        assert_eq!(encoded.tile_id, 54321, "tile_id must be preserved");
    }

    #[test]
    fn test_encode_single_tile_with_point() {
        // Create a valid WorldClippedGeometry point
        let geom = WorldClippedGeometry::Point(WorldCoord {
            x: 2_147_483_648, // Middle of world x
            y: 2_147_483_648, // Middle of world y
        });
        let geometry_bytes = geom.to_bytes();

        let feature = TileFeature::new(1, geometry_bytes, vec![]);
        let coord = TileCoord::new(0, 0, 0);
        let pending = PendingTile::new(42, coord, vec![feature]);

        let encoded = encode_single_tile(&pending, TileFormat::Mvt, "points");

        assert!(!encoded.data.is_empty());
        assert_eq!(encoded.tile_id, 42);
        assert_eq!(encoded.feature_count, 1);
    }

    #[test]
    fn test_encode_single_tile_with_properties() {
        let geom = WorldClippedGeometry::Point(WorldCoord {
            x: 2_147_483_648,
            y: 2_147_483_648,
        });

        let mut props = HashMap::new();
        props.insert(
            "name".to_string(),
            crate::wkb::PropertyValue::String("test_feature".to_string()),
        );
        props.insert("count".to_string(), crate::wkb::PropertyValue::Int(42));

        let feature = TileFeature::from_geometry(1, &geom, &props);
        let coord = TileCoord::new(0, 0, 0);
        let pending = PendingTile::new(0, coord, vec![feature]);

        let encoded = encode_single_tile(&pending, TileFormat::Mvt, "layer");

        assert!(!encoded.data.is_empty());
        assert_eq!(encoded.feature_count, 1);
    }

    #[test]
    fn test_encode_single_tile_mlt_returns_empty() {
        // MLT not yet implemented - should return empty tile
        let coord = TileCoord::new(0, 0, 0);
        let pending = PendingTile::new(0, coord, vec![]);

        let encoded = encode_single_tile(&pending, TileFormat::Mlt, "layer");

        assert!(encoded.data.is_empty());
        assert_eq!(encoded.feature_count, 0);
    }

    // ========== Parallel Encoding Tests ==========

    #[test]
    fn test_encode_tiles_parallel_empty() {
        let tiles: Vec<PendingTile> = vec![];
        let encoded = encode_tiles_parallel(tiles, TileFormat::Mvt, "layer");
        assert!(encoded.is_empty());
    }

    #[test]
    fn test_encode_tiles_parallel_multiple() {
        let tiles: Vec<PendingTile> = (0..20)
            .map(|i| PendingTile::new(i as u64, TileCoord::new(i, 0, 10), vec![]))
            .collect();

        let encoded = encode_tiles_parallel(tiles, TileFormat::Mvt, "test");
        assert_eq!(encoded.len(), 20);

        // Verify all tile_ids are present (order may vary due to parallelism)
        let mut ids: Vec<u64> = encoded.iter().map(|e| e.tile_id).collect();
        ids.sort();
        assert_eq!(ids, (0..20).collect::<Vec<u64>>());
    }

    #[test]
    fn test_encode_tiles_parallel_with_features() {
        let tiles: Vec<PendingTile> = (0..5)
            .map(|i| {
                let geom = WorldClippedGeometry::Point(WorldCoord {
                    x: 2_147_483_648,
                    y: 2_147_483_648,
                });
                let feature = TileFeature::new(i as u64, geom.to_bytes(), vec![]);
                PendingTile::new(i as u64, TileCoord::new(i, 0, 5), vec![feature])
            })
            .collect();

        let encoded = encode_tiles_parallel(tiles, TileFormat::Mvt, "test");
        assert_eq!(encoded.len(), 5);

        // Each tile should have 1 feature
        for e in &encoded {
            assert_eq!(e.feature_count, 1);
            assert!(!e.data.is_empty());
        }
    }

    // ========== Smart Batching Tests ==========

    #[test]
    fn test_encode_tiles_smart_below_threshold() {
        // Small batch - should use sequential encoding
        let tiles: Vec<PendingTile> = (0..5)
            .map(|i| PendingTile::new(i, TileCoord::new(i as u32, 0, 5), vec![]))
            .collect();

        let encoded = encode_tiles_smart(tiles, TileFormat::Mvt, "layer", 16);
        assert_eq!(encoded.len(), 5);
    }

    #[test]
    fn test_encode_tiles_smart_at_threshold() {
        // Exactly at threshold - should use parallel encoding
        let tiles: Vec<PendingTile> = (0..16)
            .map(|i| PendingTile::new(i, TileCoord::new(i as u32, 0, 5), vec![]))
            .collect();

        let encoded = encode_tiles_smart(tiles, TileFormat::Mvt, "layer", 16);
        assert_eq!(encoded.len(), 16);
    }

    #[test]
    fn test_encode_tiles_smart_above_threshold() {
        // Above threshold - should use parallel encoding
        let tiles: Vec<PendingTile> = (0..32)
            .map(|i| PendingTile::new(i, TileCoord::new(i as u32, 0, 5), vec![]))
            .collect();

        let encoded = encode_tiles_smart(tiles, TileFormat::Mvt, "layer", 16);
        assert_eq!(encoded.len(), 32);
    }

    #[test]
    fn test_default_parallel_threshold() {
        assert_eq!(DEFAULT_PARALLEL_THRESHOLD, 16);
    }

    // ========== Property Conversion Tests ==========

    #[test]
    fn test_convert_properties_empty() {
        let props: HashMap<String, crate::wkb::PropertyValue> = HashMap::new();
        let converted = convert_properties(&props);
        assert!(converted.is_empty());
    }

    #[test]
    fn test_convert_properties_all_types() {
        let mut props = HashMap::new();
        props.insert(
            "string".to_string(),
            crate::wkb::PropertyValue::String("hello".to_string()),
        );
        props.insert("int".to_string(), crate::wkb::PropertyValue::Int(-42));
        props.insert("uint".to_string(), crate::wkb::PropertyValue::UInt(42));
        // wkb uses Float(f64) for all floating point
        props.insert("float".to_string(), crate::wkb::PropertyValue::Float(2.5));
        props.insert("bool".to_string(), crate::wkb::PropertyValue::Bool(true));

        let converted = convert_properties(&props);

        assert_eq!(converted.len(), 5);

        // Verify types are correct
        let converted_map: HashMap<_, _> = converted.into_iter().collect();
        assert!(matches!(
            converted_map.get("string"),
            Some(PropertyValue::String(_))
        ));
        assert!(matches!(
            converted_map.get("int"),
            Some(PropertyValue::Int(-42))
        ));
        assert!(matches!(
            converted_map.get("uint"),
            Some(PropertyValue::UInt(42))
        ));
        // wkb::Float(f64) maps to mvt::Double(f64)
        assert!(matches!(
            converted_map.get("float"),
            Some(PropertyValue::Double(_))
        ));
        assert!(matches!(
            converted_map.get("bool"),
            Some(PropertyValue::Bool(true))
        ));
    }
}
