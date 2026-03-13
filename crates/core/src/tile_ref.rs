//! Lightweight tile reference for memory-efficient sorting.
//!
//! # Problem
//!
//! `TileFeatureRecord` stores full geometry and properties (~400 bytes) for every
//! tile-feature pair. With 30x tile replication across zoom levels, this creates
//! massive memory bloat during sort (117GB for 292M records).
//!
//! # Solution
//!
//! `TileRef` stores only tile coordinates and a handle to the geometry (~41 bytes).
//! Geometry is stored once in `GeometryStore` and retrieved on-demand during encoding.
//!
//! Memory reduction: ~400 bytes → ~41 bytes = **10x improvement**
//!
//! # Pipeline Usage
//!
//! ```ignore
//! // Phase 1: Read - store geometry once, create refs
//! let mut store = GeometryStore::new()?;
//! let handle = store.append(&wkb, &properties)?;
//! let tile_ref = TileRef::new(tile_id, z, x, y, feature_id, handle);
//!
//! // Phase 2: Sort - much smaller records to shuffle
//! sorter.add(tile_ref);
//!
//! // Phase 3: Encode - retrieve geometry for clipping
//! for tile_ref in sorted_refs {
//!     let (wkb, props) = store.read(tile_ref.geometry_handle)?;
//!     // clip and encode...
//! }
//! ```

use crate::geometry_store::GeometryHandle;
use extsort::Sortable;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::io::{Read, Write};

/// Lightweight reference to a feature in a tile.
///
/// Contains tile coordinates, feature ID, and a handle to retrieve geometry
/// from `GeometryStore`. Designed for efficient sorting by `tile_id`.
///
/// Size: ~41 bytes (vs ~400 bytes for `TileFeatureRecord`)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct TileRef {
    /// PMTiles tile ID (Hilbert curve order) - primary sort key
    pub tile_id: u64,
    /// Zoom level
    pub z: u8,
    /// Tile X coordinate
    pub x: u32,
    /// Tile Y coordinate
    pub y: u32,
    /// Original feature ID from source data
    pub feature_id: u64,
    /// Handle to retrieve geometry from GeometryStore
    pub geometry_handle: GeometryHandle,
}

impl TileRef {
    /// Size of this struct in bytes (for memory estimation).
    ///
    /// Breakdown:
    /// - tile_id: 8 bytes
    /// - z: 1 byte
    /// - x: 4 bytes
    /// - y: 4 bytes
    /// - feature_id: 8 bytes
    /// - geometry_handle: 16 bytes (GeometryHandle::SIZE)
    ///
    /// Total: 41 bytes (may be padded to 48 by compiler)
    pub const SIZE: usize = 48; // Conservative estimate with padding

    /// Create a new tile reference.
    pub fn new(
        tile_id: u64,
        z: u8,
        x: u32,
        y: u32,
        feature_id: u64,
        geometry_handle: GeometryHandle,
    ) -> Self {
        Self {
            tile_id,
            z,
            x,
            y,
            feature_id,
            geometry_handle,
        }
    }
}

impl PartialOrd for TileRef {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TileRef {
    fn cmp(&self, other: &Self) -> Ordering {
        // Sort by tile_id (Hilbert curve order) for spatial locality
        self.tile_id.cmp(&other.tile_id)
    }
}

impl Sortable for TileRef {
    fn encode<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        // Use MessagePack for compact binary serialization (same as TileFeatureRecord)
        let bytes = rmp_serde::to_vec(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        // Write length prefix (u32) for framing
        let len = bytes.len() as u32;
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(&bytes)?;
        Ok(())
    }

    fn decode<R: Read>(reader: &mut R) -> std::io::Result<Self> {
        // Read length prefix
        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes)?;
        let len = u32::from_le_bytes(len_bytes) as usize;

        // Read payload
        let mut bytes = vec![0u8; len];
        reader.read_exact(&mut bytes)?;

        // Deserialize
        rmp_serde::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =============================================================================
    // Unit Tests: Construction and Basic Properties
    // =============================================================================

    #[test]
    fn test_new_creates_tile_ref() {
        let handle = GeometryHandle {
            offset: 100,
            wkb_len: 200,
            props_len: 50,
        };

        let tile_ref = TileRef::new(12345, 10, 512, 768, 999, handle);

        assert_eq!(tile_ref.tile_id, 12345);
        assert_eq!(tile_ref.z, 10);
        assert_eq!(tile_ref.x, 512);
        assert_eq!(tile_ref.y, 768);
        assert_eq!(tile_ref.feature_id, 999);
        assert_eq!(tile_ref.geometry_handle, handle);
    }

    #[test]
    fn test_size_constant_is_reasonable() {
        // Actual size should be <= SIZE constant
        let actual_size = std::mem::size_of::<TileRef>();
        assert!(
            actual_size <= TileRef::SIZE,
            "Actual size {} exceeds SIZE constant {}",
            actual_size,
            TileRef::SIZE
        );

        // Should be significantly smaller than TileFeatureRecord (~400 bytes)
        assert!(
            actual_size < 100,
            "TileRef should be < 100 bytes, got {}",
            actual_size
        );
    }

    #[test]
    fn test_tile_ref_is_copy() {
        // TileRef should be Copy for efficient passing
        fn assert_copy<T: Copy>() {}
        assert_copy::<TileRef>();
    }

    // =============================================================================
    // Unit Tests: Ordering and Sorting
    // =============================================================================

    #[test]
    fn test_ordering_by_tile_id() {
        let handle = GeometryHandle {
            offset: 0,
            wkb_len: 100,
            props_len: 50,
        };

        let ref1 = TileRef::new(100, 5, 10, 20, 1, handle);
        let ref2 = TileRef::new(200, 5, 11, 21, 2, handle);
        let ref3 = TileRef::new(50, 5, 9, 19, 3, handle);

        // Should order by tile_id
        assert!(ref1 < ref2);
        assert!(ref3 < ref1);
        assert!(ref2 > ref1);
    }

    #[test]
    fn test_sort_stable_by_tile_id() {
        let handle = GeometryHandle {
            offset: 0,
            wkb_len: 100,
            props_len: 50,
        };

        let mut refs = [
            TileRef::new(300, 10, 1, 1, 1, handle),
            TileRef::new(100, 10, 2, 2, 2, handle),
            TileRef::new(200, 10, 3, 3, 3, handle),
            TileRef::new(100, 10, 4, 4, 4, handle), // Same tile_id as second
        ];

        refs.sort();

        assert_eq!(refs[0].tile_id, 100);
        assert_eq!(refs[1].tile_id, 100);
        assert_eq!(refs[2].tile_id, 200);
        assert_eq!(refs[3].tile_id, 300);
    }

    #[test]
    fn test_ordering_ignores_other_fields() {
        let handle1 = GeometryHandle {
            offset: 100,
            wkb_len: 200,
            props_len: 50,
        };
        let handle2 = GeometryHandle {
            offset: 999,
            wkb_len: 500,
            props_len: 100,
        };

        // Same tile_id, different everything else
        let ref1 = TileRef::new(12345, 10, 100, 200, 1, handle1);
        let ref2 = TileRef::new(12345, 15, 999, 888, 999, handle2);

        // Should be equal for sorting purposes
        assert_eq!(ref1.cmp(&ref2), Ordering::Equal);
    }

    // =============================================================================
    // Unit Tests: Serialization (for external sort)
    // =============================================================================

    #[test]
    fn test_serialize_deserialize_roundtrip() {
        let handle = GeometryHandle {
            offset: 12345,
            wkb_len: 500,
            props_len: 100,
        };
        let original = TileRef::new(98765, 12, 2048, 4096, 555, handle);

        // Serialize to bytes
        let bytes = bincode::serialize(&original).expect("Should serialize");

        // Deserialize back
        let deserialized: TileRef = bincode::deserialize(&bytes).expect("Should deserialize");

        assert_eq!(deserialized, original);
    }

    #[test]
    fn test_serialized_size_is_small() {
        let handle = GeometryHandle {
            offset: 1000,
            wkb_len: 200,
            props_len: 50,
        };
        let tile_ref = TileRef::new(12345, 10, 512, 768, 99, handle);

        let bytes = bincode::serialize(&tile_ref).expect("Should serialize");

        // Serialized size should be close to struct size
        assert!(
            bytes.len() < 60,
            "Serialized size should be < 60 bytes, got {}",
            bytes.len()
        );
    }

    #[test]
    fn test_sortable_encode_decode_roundtrip() {
        let handle = GeometryHandle {
            offset: 54321,
            wkb_len: 750,
            props_len: 125,
        };
        let original = TileRef::new(11111, 14, 8192, 4096, 777, handle);

        // Encode using Sortable trait
        let mut buffer = Vec::new();
        original.encode(&mut buffer).expect("Should encode");

        // Decode using Sortable trait
        let mut cursor = std::io::Cursor::new(buffer);
        let decoded = TileRef::decode(&mut cursor).expect("Should decode");

        assert_eq!(decoded, original);
    }

    #[test]
    fn test_sortable_encoded_size_is_compact() {
        let handle = GeometryHandle {
            offset: 1000,
            wkb_len: 200,
            props_len: 50,
        };
        let tile_ref = TileRef::new(12345, 10, 512, 768, 99, handle);

        let mut buffer = Vec::new();
        tile_ref.encode(&mut buffer).expect("Should encode");

        // Encoded size = 4 bytes (length prefix) + MessagePack payload
        // Should be roughly struct size + small overhead
        assert!(
            buffer.len() < 70,
            "Encoded size should be < 70 bytes, got {}",
            buffer.len()
        );
    }

    #[test]
    fn test_sortable_multiple_records() {
        let handle = GeometryHandle {
            offset: 0,
            wkb_len: 100,
            props_len: 50,
        };

        // Encode 3 records sequentially
        let mut buffer = Vec::new();
        for i in 0..3u64 {
            let tile_ref = TileRef::new(i * 100, 5, (i * 10) as u32, (i * 20) as u32, i, handle);
            tile_ref.encode(&mut buffer).expect("Should encode");
        }

        // Decode 3 records sequentially
        let mut cursor = std::io::Cursor::new(buffer);
        for i in 0..3u64 {
            let decoded = TileRef::decode(&mut cursor).expect("Should decode");
            assert_eq!(decoded.tile_id, i * 100);
            assert_eq!(decoded.x, (i * 10) as u32);
            assert_eq!(decoded.y, (i * 20) as u32);
            assert_eq!(decoded.feature_id, i);
        }
    }

    // =============================================================================
    // Integration Tests: Memory Efficiency
    // =============================================================================

    #[test]
    fn test_memory_savings_vs_tile_feature_record() {
        // Simulate 292M records with 30x tile replication
        let num_features = 10_000_000; // 10M for test
        let tiles_per_feature = 30;
        let total_tile_refs = num_features * tiles_per_feature;

        // Old approach: ~400 bytes per TileFeatureRecord
        let old_memory_mb = (total_tile_refs * 400) / 1_000_000;

        // New approach: ~48 bytes per TileRef (+ geometry storage)
        let tile_ref_memory_mb = (total_tile_refs * TileRef::SIZE) / 1_000_000;

        // Geometry stored once: avg 400 bytes geometry + 100 bytes props
        let geometry_storage_mb = (num_features * 500) / 1_000_000;
        let new_memory_mb = tile_ref_memory_mb + geometry_storage_mb;

        // Calculate reduction
        let reduction_factor = old_memory_mb as f64 / new_memory_mb as f64;

        // Should achieve >5x reduction (conservative - actual is ~6x with padding)
        assert!(
            reduction_factor > 5.0,
            "Expected >5x reduction, got {}x (old={} MB, new={} MB)",
            reduction_factor,
            old_memory_mb,
            new_memory_mb
        );

        println!(
            "Memory reduction: {} MB → {} MB ({}x improvement)",
            old_memory_mb, new_memory_mb, reduction_factor
        );
    }

    // =============================================================================
    // Integration Tests: Pipeline Pattern
    // =============================================================================

    #[test]
    fn test_tile_ref_with_geometry_store() {
        use crate::geometry_store::GeometryStore;

        let mut store = GeometryStore::new().expect("Should create store");

        // Simulate Phase 1: Read features, store geometry once
        let wkb = vec![0x01, 0x01, 0x00, 0x00, 0x00]; // Point WKB
        let props =
            rmp_serde::to_vec(&serde_json::json!({"name": "test"})).expect("Should serialize");

        let handle = store.append(&wkb, &props).expect("Should append");

        // Create TileRef for each tile this feature appears in
        let mut tile_refs = vec![
            TileRef::new(100, 5, 10, 15, 1, handle),
            TileRef::new(101, 5, 11, 15, 1, handle),
            TileRef::new(102, 5, 10, 16, 1, handle),
        ];

        // Phase 2: Sort by tile_id
        tile_refs.sort();

        store.flush().expect("Should flush");

        // Phase 3: Retrieve geometry for each tile
        for tile_ref in &tile_refs {
            let (retrieved_wkb, retrieved_props) =
                store.read(tile_ref.geometry_handle).expect("Should read");
            assert_eq!(retrieved_wkb, wkb);
            assert_eq!(retrieved_props, props);
        }
    }

    #[test]
    fn test_many_features_many_tiles() {
        use crate::geometry_store::GeometryStore;

        let mut store = GeometryStore::new().expect("Should create store");
        let mut tile_refs = Vec::new();

        // Simulate 1000 features, each in 30 tiles
        for feature_id in 0u64..1000 {
            let wkb = format!("geometry_{}", feature_id).into_bytes();
            let props = format!("{{\"id\":{}}}", feature_id).into_bytes();
            let handle = store.append(&wkb, &props).expect("Should append");

            // Each feature appears in 30 tiles (z5 to z8, various x/y)
            for tile_num in 0u64..30 {
                let tile_id = feature_id * 100 + tile_num; // Fake Hilbert IDs
                let z = 5 + (tile_num % 4) as u8;
                let x = (feature_id % 32) as u32;
                let y = (tile_num % 32) as u32;

                tile_refs.push(TileRef::new(tile_id, z, x, y, feature_id, handle));
            }
        }

        // Should have 30,000 refs
        assert_eq!(tile_refs.len(), 30_000);

        // Sort by tile_id
        tile_refs.sort();

        store.flush().expect("Should flush");

        // Verify we can retrieve all geometries
        let sample_indices = [0, 1000, 15000, 29999];
        for &idx in &sample_indices {
            let tile_ref = &tile_refs[idx];
            let (wkb, _props) = store.read(tile_ref.geometry_handle).expect("Should read");
            assert!(!wkb.is_empty());
        }
    }
}
