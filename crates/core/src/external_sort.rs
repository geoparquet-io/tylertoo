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

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use tempfile::TempDir;

/// A feature record ready for tile building, sorted by tile_id.
///
/// This struct holds all data needed to include a feature in a vector tile:
/// - `tile_id`: PMTiles Hilbert-curve ID (determines sort order)
/// - `z`, `x`, `y`: Tile coordinates (stored to avoid reversing Hilbert curve)
/// - `feature_id`: Original feature index for debugging/provenance
/// - `original_hilbert`: Hilbert index of the ORIGINAL (unclipped) geometry centroid
/// - `geometry_wkb`: WKB-encoded geometry (clipped to tile if needed)
/// - `properties`: MessagePack-serialized feature properties
///
/// # Sorting
///
/// Records are sorted by `(tile_id, original_hilbert)`. The `original_hilbert` field
/// enables correct gap-based density dropping (tippecanoe's `--drop-densest-as-needed`).
/// Features must be sorted by their original Hilbert index within each tile for the
/// gap-based algorithm to work correctly.
///
/// See: https://github.com/geoparquet-io/gpq-tiles/issues/145
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
    /// Hilbert index of the ORIGINAL geometry centroid (before clipping).
    /// Used as secondary sort key for correct gap-based density dropping.
    pub original_hilbert: u64,
    /// WKB-encoded geometry
    pub geometry_wkb: Vec<u8>,
    /// MessagePack-serialized properties
    pub properties: Vec<u8>,
}

impl TileFeatureRecord {
    /// Create a new tile feature record.
    ///
    /// # Arguments
    ///
    /// * `tile_id` - PMTiles tile ID (Hilbert curve order)
    /// * `z`, `x`, `y` - Tile coordinates
    /// * `feature_id` - Original feature index for debugging/provenance
    /// * `original_hilbert` - Hilbert index of the ORIGINAL geometry centroid (before clipping)
    /// * `geometry_wkb` - Serialized geometry bytes
    /// * `properties` - MessagePack-serialized properties
    pub fn new(
        tile_id: u64,
        z: u8,
        x: u32,
        y: u32,
        feature_id: u64,
        original_hilbert: u64,
        geometry_wkb: Vec<u8>,
        properties: Vec<u8>,
    ) -> Self {
        Self {
            tile_id,
            z,
            x,
            y,
            feature_id,
            original_hilbert,
            geometry_wkb,
            properties,
        }
    }

    /// Encode a record to a writer with length prefix.
    fn encode<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        let bytes = rmp_serde::to_vec(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let len = bytes.len() as u32;
        writer.write_all(&len.to_le_bytes())?;
        writer.write_all(&bytes)?;
        Ok(())
    }

    /// Decode a record from a reader with length prefix.
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

impl Eq for TileFeatureRecord {}

impl PartialOrd for TileFeatureRecord {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TileFeatureRecord {
    fn cmp(&self, other: &Self) -> Ordering {
        // Primary sort: tile_id (groups features by tile)
        // Secondary sort: original_hilbert (enables gap-based density dropping)
        //
        // CHANGED in #145: Previously sorted by feature_id, but gap-based dropping
        // requires features within a tile to be sorted by their original Hilbert index.
        self.tile_id
            .cmp(&other.tile_id)
            .then_with(|| self.original_hilbert.cmp(&other.original_hilbert))
    }
}

/// External sorter for tile feature records.
///
/// **Memory-bounded**: When the in-memory buffer fills, records are sorted
/// and flushed to a temp file. Final `sort()` performs k-way merge of all
/// segments.
///
/// This ensures memory usage is bounded to O(buffer_size) regardless of
/// how many records are added.
pub struct TileFeatureSorter {
    /// In-memory buffer for records before flushing
    buffer: Vec<TileFeatureRecord>,
    /// Maximum records to buffer before flushing to disk
    buffer_capacity: usize,
    /// Temp directory for segment files
    temp_dir: Option<TempDir>,
    /// Paths to sorted segment files
    segment_paths: Vec<PathBuf>,
    /// Total records added (for len())
    total_records: usize,
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
        Self {
            buffer: Vec::with_capacity(buffer_capacity.min(1024)),
            buffer_capacity: buffer_capacity.max(1000), // Minimum 1000 to avoid too many segments
            temp_dir: None,
            segment_paths: Vec::new(),
            total_records: 0,
        }
    }

    /// Add a record to be sorted.
    ///
    /// When the buffer is full, records are sorted and flushed to a temp file.
    pub fn add(&mut self, record: TileFeatureRecord) {
        self.buffer.push(record);
        self.total_records += 1;

        // Flush to disk when buffer is full
        if self.buffer.len() >= self.buffer_capacity {
            if let Err(e) = self.flush_buffer() {
                // Log error but don't panic - we'll handle it during sort()
                tracing::warn!("Failed to flush sort buffer to disk: {}", e);
            }
        }
    }

    /// Flush the in-memory buffer to a sorted segment file.
    fn flush_buffer(&mut self) -> std::io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        // Create temp directory if needed
        if self.temp_dir.is_none() {
            self.temp_dir = Some(TempDir::new()?);
        }

        // Sort the buffer
        self.buffer.sort();

        // Write to segment file
        let segment_path = self
            .temp_dir
            .as_ref()
            .unwrap()
            .path()
            .join(format!("segment_{:04}.bin", self.segment_paths.len()));

        let file = File::create(&segment_path)?;
        let mut writer = BufWriter::new(file);

        for record in self.buffer.drain(..) {
            record.encode(&mut writer)?;
        }

        writer.flush()?;
        self.segment_paths.push(segment_path);

        tracing::debug!(
            "Flushed {} records to segment {} (total segments: {})",
            self.buffer_capacity,
            self.segment_paths.len() - 1,
            self.segment_paths.len()
        );

        Ok(())
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
    /// This consumes the sorter. Performs k-way merge of all segments
    /// plus any remaining in-memory records.
    pub fn sort(mut self) -> std::io::Result<SortedRecordIterator> {
        // Flush any remaining records
        self.flush_buffer()?;

        // If no segments (everything fit in memory), just return empty iterator
        if self.segment_paths.is_empty() {
            return Ok(SortedRecordIterator {
                heap: BinaryHeap::new(),
                _temp_dir: self.temp_dir,
            });
        }

        // Open all segment files and create merge heap
        let mut heap = BinaryHeap::new();

        for path in self.segment_paths.iter() {
            let file = File::open(path)?;
            let mut reader = BufReader::new(file);

            // Read first record from each segment
            match TileFeatureRecord::decode(&mut reader) {
                Ok(record) => {
                    heap.push(SegmentEntry { record, reader });
                }
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // Empty segment, skip
                }
                Err(e) => return Err(e),
            }
        }

        Ok(SortedRecordIterator {
            heap,
            _temp_dir: self.temp_dir,
        })
    }
}

/// Entry in the merge heap for k-way merge.
struct SegmentEntry {
    record: TileFeatureRecord,
    reader: BufReader<File>,
}

impl Eq for SegmentEntry {}

impl PartialEq for SegmentEntry {
    fn eq(&self, other: &Self) -> bool {
        self.record == other.record
    }
}

impl PartialOrd for SegmentEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SegmentEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse order for min-heap behavior (BinaryHeap is max-heap)
        other.record.cmp(&self.record)
    }
}

/// Iterator that performs k-way merge of sorted segments.
pub struct SortedRecordIterator {
    heap: BinaryHeap<SegmentEntry>,
    _temp_dir: Option<TempDir>, // Keep alive until iteration completes
}

impl Iterator for SortedRecordIterator {
    type Item = std::io::Result<TileFeatureRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut entry = self.heap.pop()?;

        let result = entry.record.clone();

        // Try to read next record from this segment
        match TileFeatureRecord::decode(&mut entry.reader) {
            Ok(next_record) => {
                entry.record = next_record;
                self.heap.push(entry);
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Segment exhausted, don't push back
            }
            Err(e) => {
                // Return error on next call
                return Some(Err(e));
            }
        }

        Some(Ok(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_feature_record_creation() {
        let record = TileFeatureRecord::new(42, 5, 10, 20, 1, 5000, vec![1, 2, 3], vec![4, 5, 6]);
        assert_eq!(record.tile_id, 42);
        assert_eq!(record.z, 5);
        assert_eq!(record.x, 10);
        assert_eq!(record.y, 20);
        assert_eq!(record.feature_id, 1);
        assert_eq!(record.original_hilbert, 5000);
        assert_eq!(record.geometry_wkb, vec![1, 2, 3]);
        assert_eq!(record.properties, vec![4, 5, 6]);
    }

    #[test]
    fn test_tile_feature_record_ordering() {
        // tile_id=1, original_hilbert=100
        let r1 = TileFeatureRecord::new(1, 0, 0, 0, 1, 100, vec![], vec![]);
        // tile_id=2, original_hilbert=100
        let r2 = TileFeatureRecord::new(2, 0, 0, 0, 1, 100, vec![], vec![]);
        // tile_id=1, original_hilbert=200
        let r3 = TileFeatureRecord::new(1, 0, 0, 0, 2, 200, vec![], vec![]);

        // tile_id is primary sort key
        assert!(r1 < r2);
        // original_hilbert is secondary sort key (not feature_id!)
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
            999999, // original_hilbert
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

        sorter.add(TileFeatureRecord::new(1, 0, 0, 0, 1, 100, vec![], vec![]));
        assert!(!sorter.is_empty());
        assert_eq!(sorter.len(), 1);
    }

    #[test]
    fn test_sorter_sorts_by_tile_id() {
        let mut sorter = TileFeatureSorter::new(1000);

        // Add records out of order (same original_hilbert, different tile_id)
        sorter.add(TileFeatureRecord::new(3, 0, 0, 0, 1, 100, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(1, 0, 0, 0, 1, 100, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(2, 0, 0, 0, 1, 100, vec![], vec![]));

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted.len(), 3);
        assert_eq!(sorted[0].tile_id, 1);
        assert_eq!(sorted[1].tile_id, 2);
        assert_eq!(sorted[2].tile_id, 3);
    }

    #[test]
    fn test_sorter_stable_within_tile_by_hilbert() {
        // UPDATED in #145: Now sorts by original_hilbert, not feature_id
        let mut sorter = TileFeatureSorter::new(1000);

        // Multiple features in same tile with different original_hilbert values
        // feature_id is intentionally different from hilbert order to prove sorting
        sorter.add(TileFeatureRecord::new(
            5,
            1,
            0,
            0,
            100,
            3000,
            vec![],
            vec![],
        )); // hilbert=3000
        sorter.add(TileFeatureRecord::new(
            5,
            1,
            0,
            0,
            200,
            1000,
            vec![],
            vec![],
        )); // hilbert=1000
        sorter.add(TileFeatureRecord::new(
            5,
            1,
            0,
            0,
            300,
            2000,
            vec![],
            vec![],
        )); // hilbert=2000

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted.len(), 3);
        // Should be sorted by original_hilbert (1000 < 2000 < 3000), NOT feature_id
        assert_eq!(sorted[0].original_hilbert, 1000);
        assert_eq!(sorted[0].feature_id, 200);
        assert_eq!(sorted[1].original_hilbert, 2000);
        assert_eq!(sorted[1].feature_id, 300);
        assert_eq!(sorted[2].original_hilbert, 3000);
        assert_eq!(sorted[2].feature_id, 100);
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
            100, // original_hilbert
            geom2.clone(),
            props2.clone(),
        ));
        sorter.add(TileFeatureRecord::new(
            1,
            0,
            0,
            0,
            1,
            100, // original_hilbert
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
        // Each record has tile_id = i, original_hilbert = i (so sort order is deterministic)
        for i in (0..500).rev() {
            sorter.add(TileFeatureRecord::new(
                i,
                0,
                0,
                0,
                i, // feature_id
                i, // original_hilbert
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
                i, // feature_id
                i, // original_hilbert
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
}
