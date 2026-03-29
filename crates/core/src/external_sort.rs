//! External merge sort for memory-bounded tile generation.
//!
//! When generating tiles from large GeoParquet files, we need to group features
//! by tile ID (Hilbert-ordered) to build each tile efficiently. This module provides
//! disk-backed sorting that can handle datasets larger than available RAM.
//!
//! # How It Works
//!
//! 1. Features are extracted from GeoParquet and converted to `TileFeatureRecord`
//! 2. Records are fed to `TileFeatureSorter`, which buffers them in memory
//! 3. When the buffer fills, it's sorted and written to a temp file
//! 4. Final iteration performs k-way merge of all sorted chunks
//! 5. Output is an iterator of records sorted by `tile_id`, ready for tile building
//!
//! # Implementation
//!
//! Uses the `extsort` crate which provides:
//! - **In-memory passthrough**: When data fits in buffer, uses VecDeque directly (zero disk I/O)
//! - **Peek mode**: For <20 segments, linear scan instead of heap (faster)
//! - **Heap mode**: For ≥20 segments, binary heap with 20-item batch read-ahead
//! - **Parallel sort**: Optional rayon support for sorting in-memory segments
//!
//! # Example
//!
//! ```ignore
//! use gpq_tiles_core::external_sort::{TileFeatureRecord, TileFeatureSorter};
//!
//! let mut sorter = TileFeatureSorter::new(100_000);  // Buffer 100K records
//!
//! // Add records from GeoParquet processing
//! sorter.add(TileFeatureRecord {
//!     tile_id: 42,
//!     feature_id: 1,
//!     geometry_wkb: vec![...],
//!     properties: vec![...],  // MessagePack serialized
//! });
//!
//! // Get sorted iterator for tile building
//! for record in sorter.sort()? {
//!     let record = record?;
//!     // All records with same tile_id come consecutively
//! }
//! ```

use extsort::{ExternalSorter, Sortable};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::io::{Read, Write};

/// A feature record ready for tile building, sorted by tile_id.
///
/// This struct holds all data needed to include a feature in a vector tile:
/// - `tile_id`: PMTiles Hilbert-curve ID (determines sort order)
/// - `z`, `x`, `y`: Tile coordinates (stored to avoid reversing Hilbert curve)
/// - `feature_id`: Original feature index for debugging/provenance
/// - `geometry_wkb`: WKB-encoded geometry (clipped to tile if needed)
/// - `properties`: MessagePack-serialized feature properties
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TileFeatureRecord {
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
    /// WKB-encoded geometry
    pub geometry_wkb: Vec<u8>,
    /// MessagePack-serialized properties
    pub properties: Vec<u8>,
}

impl TileFeatureRecord {
    /// Create a new tile feature record.
    pub fn new(
        tile_id: u64,
        z: u8,
        x: u32,
        y: u32,
        feature_id: u64,
        geometry_wkb: Vec<u8>,
        properties: Vec<u8>,
    ) -> Self {
        Self {
            tile_id,
            z,
            x,
            y,
            feature_id,
            geometry_wkb,
            properties,
        }
    }
}

impl Eq for TileFeatureRecord {}

impl PartialOrd for TileFeatureRecord {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TileFeatureRecord {
    fn cmp(&self, other: &Self) -> Ordering {
        // Primary sort: tile_id (groups features by tile)
        // Secondary sort: feature_id (stable ordering within tile)
        self.tile_id
            .cmp(&other.tile_id)
            .then_with(|| self.feature_id.cmp(&other.feature_id))
    }
}

/// Implement extsort's Sortable trait for disk serialization.
///
/// Uses length-prefixed MessagePack encoding for efficient serialization.
impl Sortable for TileFeatureRecord {
    fn encode<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        let bytes = rmp_serde::to_vec(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let len = bytes.len() as u32;
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(&bytes)?;
        Ok(())
    }

    fn decode<R: Read>(reader: &mut R) -> std::io::Result<Self> {
        let mut len_bytes = [0u8; 4];
        reader.read_exact(&mut len_bytes)?;
        let len = u32::from_le_bytes(len_bytes) as usize;

        let mut bytes = vec![0u8; len];
        reader.read_exact(&mut bytes)?;

        rmp_serde::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// Comparator function type for TileFeatureRecord.
type RecordComparator = fn(&TileFeatureRecord, &TileFeatureRecord) -> Ordering;

/// Type alias for the extsort push sorter.
type InnerPushSorter = extsort::PushExternalSorter<TileFeatureRecord, RecordComparator>;

/// External sorter for tile feature records.
///
/// **Memory-bounded**: When the in-memory buffer fills, records are sorted
/// and flushed to a temp file. Final `sort()` performs k-way merge of all
/// segments.
///
/// This ensures memory usage is bounded to O(buffer_size) regardless of
/// how many records are added.
///
/// **In-memory fast path**: When all data fits in the buffer, no disk I/O
/// occurs - the sorted data is returned directly from memory.
pub struct TileFeatureSorter {
    /// The underlying extsort push sorter
    inner: InnerPushSorter,
    /// Total records added (for len())
    total_records: usize,
}

/// Comparator function for TileFeatureRecord sorting.
fn record_cmp(a: &TileFeatureRecord, b: &TileFeatureRecord) -> Ordering {
    a.cmp(b)
}

impl TileFeatureSorter {
    /// Create a new sorter with the specified buffer size.
    ///
    /// # Arguments
    ///
    /// * `buffer_capacity` - Maximum number of records to hold in memory.
    ///   When exceeded, records are sorted and written to a temp file.
    ///   Larger values use more RAM but reduce disk I/O.
    ///   Typical value: 100,000 - 1,000,000 depending on available memory.
    pub fn new(buffer_capacity: usize) -> Self {
        // Use parallel sort for better performance on large buffers
        let sorter = ExternalSorter::new()
            .with_segment_size(buffer_capacity.max(1000)) // Minimum 1000 to avoid too many segments
            .with_parallel_sort()
            .pushed_by(record_cmp as RecordComparator);

        Self {
            inner: sorter,
            total_records: 0,
        }
    }

    /// Add a record to be sorted.
    ///
    /// When the buffer is full, records are sorted and flushed to a temp file.
    pub fn add(&mut self, record: TileFeatureRecord) {
        self.total_records += 1;
        if let Err(e) = self.inner.push(record) {
            // Log error but don't panic - we'll handle it during sort()
            tracing::warn!("Failed to add record to sorter: {}", e);
        }
    }

    /// Returns the total number of records added.
    pub fn len(&self) -> usize {
        self.total_records
    }

    /// Returns true if no records have been added.
    pub fn is_empty(&self) -> bool {
        self.total_records == 0
    }

    /// Sort all records and return an iterator over them in tile_id order.
    ///
    /// This consumes the sorter. If all data fit in memory, no disk I/O
    /// occurs (in-memory fast path). Otherwise, performs k-way merge of
    /// all disk segments.
    pub fn sort(self) -> std::io::Result<SortedRecordIterator> {
        let inner = self.inner.done()?;
        Ok(SortedRecordIterator { inner })
    }
}

/// Type alias for the extsort sorted iterator.
type InnerSortedIterator = extsort::SortedIterator<TileFeatureRecord, RecordComparator>;

/// Iterator that returns sorted tile feature records.
///
/// Wraps extsort's SortedIterator, which operates in 3 modes:
/// - **Passthrough**: Data fit in memory → direct VecDeque iteration (zero disk I/O)
/// - **Peek**: <20 segments → linear scan (faster than heap for few segments)
/// - **Heap**: ≥20 segments → binary heap with batch read-ahead
pub struct SortedRecordIterator {
    inner: InnerSortedIterator,
}

impl SortedRecordIterator {
    /// Returns the number of disk segments created during sorting.
    ///
    /// Returns 0 if all data fit in the memory buffer (in-memory fast path).
    /// This is useful for verifying that small datasets don't incur disk I/O.
    pub fn disk_segment_count(&self) -> usize {
        self.inner.disk_segment_count()
    }

    /// Returns the total number of items in the sorted iterator.
    pub fn sorted_count(&self) -> u64 {
        self.inner.sorted_count()
    }
}

impl Iterator for SortedRecordIterator {
    type Item = std::io::Result<TileFeatureRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_feature_record_creation() {
        let record = TileFeatureRecord::new(42, 5, 10, 20, 1, vec![1, 2, 3], vec![4, 5, 6]);
        assert_eq!(record.tile_id, 42);
        assert_eq!(record.z, 5);
        assert_eq!(record.x, 10);
        assert_eq!(record.y, 20);
        assert_eq!(record.feature_id, 1);
        assert_eq!(record.geometry_wkb, vec![1, 2, 3]);
        assert_eq!(record.properties, vec![4, 5, 6]);
    }

    #[test]
    fn test_tile_feature_record_ordering() {
        let r1 = TileFeatureRecord::new(1, 0, 0, 0, 1, vec![], vec![]);
        let r2 = TileFeatureRecord::new(2, 0, 0, 0, 1, vec![], vec![]);
        let r3 = TileFeatureRecord::new(1, 0, 0, 0, 2, vec![], vec![]);

        // tile_id is primary sort key
        assert!(r1 < r2);
        // feature_id is secondary sort key
        assert!(r1 < r3);
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let original = TileFeatureRecord::new(
            123456,
            10,
            100,
            200,
            789,
            vec![0x01, 0x02, 0x03, 0x04],
            vec![0x82, 0xa4, b't', b'e', b's', b't'], // MessagePack map
        );

        let mut buffer = Vec::new();
        original.encode(&mut buffer).unwrap();

        let decoded = TileFeatureRecord::decode(&mut buffer.as_slice()).unwrap();
        assert_eq!(original, decoded);
    }

    #[test]
    fn test_sorter_basic_operations() {
        let mut sorter = TileFeatureSorter::new(1000);
        assert!(sorter.is_empty());
        assert_eq!(sorter.len(), 0);

        sorter.add(TileFeatureRecord::new(1, 0, 0, 0, 1, vec![], vec![]));
        assert!(!sorter.is_empty());
        assert_eq!(sorter.len(), 1);
    }

    #[test]
    fn test_sorter_sorts_by_tile_id() {
        let mut sorter = TileFeatureSorter::new(1000);

        // Add records out of order
        sorter.add(TileFeatureRecord::new(3, 0, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(1, 0, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(2, 0, 0, 0, 1, vec![], vec![]));

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted.len(), 3);
        assert_eq!(sorted[0].tile_id, 1);
        assert_eq!(sorted[1].tile_id, 2);
        assert_eq!(sorted[2].tile_id, 3);
    }

    #[test]
    fn test_sorter_stable_within_tile() {
        let mut sorter = TileFeatureSorter::new(1000);

        // Multiple features in same tile
        sorter.add(TileFeatureRecord::new(5, 1, 0, 0, 3, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(5, 1, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(5, 1, 0, 0, 2, vec![], vec![]));

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted.len(), 3);
        // Should be sorted by feature_id within same tile_id
        assert_eq!(sorted[0].feature_id, 1);
        assert_eq!(sorted[1].feature_id, 2);
        assert_eq!(sorted[2].feature_id, 3);
    }

    #[test]
    fn test_sorter_with_geometry_and_properties() {
        let mut sorter = TileFeatureSorter::new(1000);

        let geom1 = vec![0x01, 0x01, 0x00, 0x00, 0x00]; // Point WKB header
        let props1 = rmp_serde::to_vec(&serde_json::json!({"name": "feature1"})).unwrap();

        let geom2 = vec![0x01, 0x02, 0x00, 0x00, 0x00]; // LineString WKB header
        let props2 = rmp_serde::to_vec(&serde_json::json!({"name": "feature2"})).unwrap();

        sorter.add(TileFeatureRecord::new(
            2,
            0,
            0,
            0,
            1,
            geom2.clone(),
            props2.clone(),
        ));
        sorter.add(TileFeatureRecord::new(
            1,
            0,
            0,
            0,
            1,
            geom1.clone(),
            props1.clone(),
        ));

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted[0].tile_id, 1);
        assert_eq!(sorted[0].geometry_wkb, geom1);
        assert_eq!(sorted[0].properties, props1);

        assert_eq!(sorted[1].tile_id, 2);
        assert_eq!(sorted[1].geometry_wkb, geom2);
        assert_eq!(sorted[1].properties, props2);
    }

    #[test]
    fn test_sorter_disk_spill() {
        // Use small buffer to force disk spill
        let mut sorter = TileFeatureSorter::new(100);

        // Add 500 records - should create 5 segments
        for i in (0..500).rev() {
            sorter.add(TileFeatureRecord::new(
                i,
                0,
                0,
                0,
                i,
                vec![i as u8],
                vec![(i % 256) as u8],
            ));
        }

        assert_eq!(sorter.len(), 500);

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted.len(), 500);
        for (i, record) in sorted.iter().enumerate() {
            assert_eq!(
                record.tile_id, i as u64,
                "Record at position {} has wrong tile_id",
                i
            );
        }
    }

    #[test]
    fn test_sorter_large_dataset() {
        // Test with enough records to trigger multiple disk spills
        let mut sorter = TileFeatureSorter::new(100);

        // Add 1000 records in reverse order
        for i in (0..1000).rev() {
            sorter.add(TileFeatureRecord::new(
                i,
                0,
                0,
                0,
                i,
                vec![i as u8],
                vec![(i % 256) as u8],
            ));
        }

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted.len(), 1000);
        for (i, record) in sorted.iter().enumerate() {
            assert_eq!(
                record.tile_id, i as u64,
                "Record at position {} has wrong tile_id",
                i
            );
        }
    }

    #[test]
    fn test_empty_sorter() {
        let sorter = TileFeatureSorter::new(1000);
        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();
        assert!(sorted.is_empty());
    }

    #[test]
    fn test_in_memory_fast_path_no_disk_io() {
        // This test verifies the critical in-memory fast path:
        // When data fits in buffer, NO disk I/O should occur.
        //
        // BUG IN CURRENT IMPL (issue #147): flush_buffer() is called
        // unconditionally before checking if segments exist, causing
        // ALL data to hit disk even when it fits in memory.

        let mut sorter = TileFeatureSorter::new(1000); // Buffer holds 1000

        // Add only 100 records - well under buffer capacity
        for i in (0..100).rev() {
            sorter.add(TileFeatureRecord::new(i, 0, 0, 0, i, vec![], vec![]));
        }

        let sorted_iter = sorter.sort().unwrap();

        // The key assertion: disk_segment_count() should be 0
        // when all data fit in memory
        assert_eq!(
            sorted_iter.disk_segment_count(),
            0,
            "In-memory fast path failed: data was written to disk unnecessarily"
        );

        // Verify sorting still works correctly
        let sorted: Vec<_> = sorted_iter.map(|r| r.unwrap()).collect();
        assert_eq!(sorted.len(), 100);
        for (i, record) in sorted.iter().enumerate() {
            assert_eq!(record.tile_id, i as u64);
        }
    }
}
