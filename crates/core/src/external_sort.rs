//! External merge sort for memory-bounded tile generation.
//!
//! When generating tiles from large GeoParquet files, we need to group features
//! by tile ID (Hilbert-ordered) to build each tile efficiently. This module provides
//! disk-backed sorting that can handle datasets larger than available RAM.
//!
//! # How It Works
//!
//! 1. Features are extracted from GeoParquet and converted to `TileFeatureRecord`
//! 2. Records are fed to `ShardedTileFeatureSorter`, which partitions by tile_id
//! 3. Each shard independently buffers and sorts its partition
//! 4. When a shard's buffer fills, it's sorted and written to a temp file
//! 5. Final iteration performs k-way merge across all shards
//! 6. Output is an iterator of records sorted by `tile_id`, ready for tile building
//!
//! # Adaptive Consolidation
//!
//! The `extsort` crate opens ALL segment files during merge, which can exhaust
//! file descriptors on large datasets. We use **adaptive consolidation**:
//!
//! - Small shards (≤50 segments): Direct iteration, no extra I/O
//! - Large shards (>50 segments): Consolidate to single intermediate file
//!
//! This ensures bounded file descriptors while avoiding unnecessary I/O overhead
//! for small datasets.
//!
//! ## Resource Bounds
//!
//! | Dataset Size | Segments/Shard | Action      | Extra I/O |
//! |--------------|----------------|-------------|-----------|
//! | 1M records   | ~1             | Direct      | None      |
//! | 50M records  | ~31            | Direct      | None      |
//! | 100M records | ~63            | Consolidate | +1 pass   |
//! | 292M records | ~183           | Consolidate | +1 pass   |
//!
//! ## File Descriptor Usage
//!
//! - During consolidation: max ~50 (one shard) + accumulated intermediates
//! - During final merge: 16 files (one per shard)
//! - Peak: ~200 file descriptors (well under 1024 limit)
//!
//! # Example
//!
//! ```ignore
//! use gpq_tiles_core::external_sort::{TileFeatureRecord, ShardedTileFeatureSorter};
//!
//! let mut sorter = ShardedTileFeatureSorter::new(100_000, 16);  // 100K buffer, 16 shards
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
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};

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

impl Sortable for TileFeatureRecord {
    fn encode<W: Write>(&self, writer: &mut W) -> std::io::Result<()> {
        // Use MessagePack for compact binary serialization
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

/// External sorter for tile feature records.
///
/// Wraps `extsort::ExternalSorter` with a convenient API for our use case.
/// Records are buffered in memory until the buffer fills, then sorted chunks
/// are written to disk. Final iteration merges all chunks.
pub struct TileFeatureSorter {
    /// In-memory buffer for records before sorting
    records: Vec<TileFeatureRecord>,
    /// Maximum records to buffer before flushing to disk
    sort_buffer_size: usize,
}

impl TileFeatureSorter {
    /// Create a new sorter with the specified buffer size.
    ///
    /// # Arguments
    ///
    /// * `sort_buffer_size` - Maximum number of records to hold in memory.
    ///   Larger values use more RAM but reduce disk I/O.
    ///   Typical value: 100,000 - 1,000,000 depending on available memory.
    pub fn new(sort_buffer_size: usize) -> Self {
        Self {
            records: Vec::with_capacity(sort_buffer_size.min(1024)),
            sort_buffer_size,
        }
    }

    /// Add a record to be sorted.
    pub fn add(&mut self, record: TileFeatureRecord) {
        self.records.push(record);
    }

    /// Returns the number of records currently buffered.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Returns true if no records have been added.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Estimates the number of segment files that will be created during sorting.
    ///
    /// The `extsort` crate creates a new segment file each time the buffer exceeds
    /// `segment_size`. This estimate is used for adaptive consolidation: if a shard
    /// will create too many segments, we consolidate to a single intermediate file
    /// to avoid exhausting file descriptors.
    ///
    /// Returns 0 for empty sorters, otherwise `ceil(records / buffer_size)`.
    pub fn estimated_segment_count(&self) -> usize {
        if self.records.is_empty() {
            0
        } else {
            (self.records.len() + self.sort_buffer_size - 1) / self.sort_buffer_size
        }
    }

    /// Sort all records and return an iterator over them in tile_id order.
    ///
    /// This consumes the sorter. For datasets that fit in the buffer,
    /// sorting happens entirely in memory. For larger datasets, the
    /// external sorter writes sorted chunks to temp files and merges them.
    pub fn sort(self) -> std::io::Result<impl Iterator<Item = std::io::Result<TileFeatureRecord>>> {
        let sorter = ExternalSorter::new().with_segment_size(self.sort_buffer_size);

        sorter.sort(self.records)
    }
}

/// Default number of shards for the sharded sorter.
/// 16 shards keeps each shard under 1024 file descriptors for datasets up to ~1.6B records.
const DEFAULT_NUM_SHARDS: usize = 16;

/// Target segments per shard to stay safely under the 1024 file descriptor limit.
/// With this target, each shard opens at most ~50 segment files during merge.
const TARGET_SEGMENTS_PER_SHARD: usize = 50;

/// Minimum buffer size to avoid excessive disk I/O for small datasets.
const MIN_BUFFER_SIZE: usize = 100_000;

/// Estimated tile replication factor: each source geometry generates multiple
/// TileFeatureRecords (one per tile it intersects across all zoom levels).
/// With zoom 0-14, a geometry creates records at EACH level, and may touch
/// multiple tiles at higher zooms. Conservative estimate: 30x.
const TILE_REPLICATION_FACTOR: u64 = 30;

/// Calculate optimal sort buffer size based on estimated record count.
///
/// The `extsort` crate opens ALL segment files during merge, so we need to ensure
/// each shard creates at most ~50 segments to stay under the 1024 FD limit.
///
/// **Important:** `total_records` is the Parquet row count (source geometries),
/// but the sorter receives TileFeatureRecords (one per tile a geometry touches).
/// We apply a replication factor to account for this.
///
/// Formula: `buffer_size = max(MIN_BUFFER, total_records * replication / (num_shards * target_segments))`
///
/// # Arguments
///
/// * `total_records` - Source geometry count from Parquet metadata
/// * `num_shards` - Number of shards (default: 16)
///
/// # Returns
///
/// Optimal buffer size that keeps segments per shard under the target.
pub fn calculate_optimal_sort_buffer(total_records: u64, num_shards: usize) -> usize {
    if total_records == 0 {
        return MIN_BUFFER_SIZE;
    }

    // Account for tile replication: each geometry → multiple TileFeatureRecords
    let estimated_tile_records = total_records.saturating_mul(TILE_REPLICATION_FACTOR);

    // Records per shard (assuming even distribution)
    let records_per_shard = estimated_tile_records / num_shards as u64;

    // Required buffer to keep segments under target
    // buffer = ceil(records_per_shard / target_segments)
    let required_buffer = if records_per_shard == 0 {
        MIN_BUFFER_SIZE
    } else {
        ((records_per_shard + TARGET_SEGMENTS_PER_SHARD as u64 - 1)
            / TARGET_SEGMENTS_PER_SHARD as u64) as usize
    };

    // Never go below minimum
    required_buffer.max(MIN_BUFFER_SIZE)
}

/// Sharded external sorter for tile feature records.
///
/// Partitions records by `tile_id % num_shards` to avoid "too many open files" errors.
/// Each shard is an independent `TileFeatureSorter` that creates its own temp files.
/// The final merge is only num_shards-way (typically 16), not thousands-way.
///
/// # Why Sharding?
///
/// The `extsort` crate opens ALL segment files during the merge phase. With large
/// datasets (e.g., 292M records at 100K segment size = 2,920 segments), this exceeds
/// the OS file descriptor limit (typically 1024).
///
/// Sharding solves this by partitioning the data:
/// - 16 shards → ~183 segments per shard → well under 1024 limit
/// - Final merge is only 16-way, trivially handled
pub struct ShardedTileFeatureSorter {
    /// One sorter per shard
    shards: Vec<TileFeatureSorter>,
    /// Number of shards (for modulo calculation)
    num_shards: usize,
    /// Total records added (for statistics)
    total_count: usize,
}

impl ShardedTileFeatureSorter {
    /// Create a new sharded sorter with default shard count (16).
    ///
    /// # Arguments
    ///
    /// * `sort_buffer_size` - Maximum records per shard buffer before flushing to disk.
    pub fn new(sort_buffer_size: usize) -> Self {
        Self::with_shards(sort_buffer_size, DEFAULT_NUM_SHARDS)
    }

    /// Create a new sharded sorter with custom shard count.
    ///
    /// # Arguments
    ///
    /// * `sort_buffer_size` - Maximum records per shard buffer before flushing to disk.
    /// * `num_shards` - Number of shards. More shards = fewer segments per shard.
    ///   Recommended: 16-64 for very large datasets.
    pub fn with_shards(sort_buffer_size: usize, num_shards: usize) -> Self {
        let shards = (0..num_shards)
            .map(|_| TileFeatureSorter::new(sort_buffer_size))
            .collect();

        Self {
            shards,
            num_shards,
            total_count: 0,
        }
    }

    /// Add a record to be sorted.
    ///
    /// The record is routed to the shard determined by `tile_id % num_shards`.
    pub fn add(&mut self, record: TileFeatureRecord) {
        let shard_idx = (record.tile_id as usize) % self.num_shards;
        self.shards[shard_idx].add(record);
        self.total_count += 1;
    }

    /// Returns the total number of records added across all shards.
    pub fn len(&self) -> usize {
        self.total_count
    }

    /// Returns true if no records have been added.
    pub fn is_empty(&self) -> bool {
        self.total_count == 0
    }

    /// Sort all shards and merge into a single sorted iterator.
    ///
    /// Each shard is sorted independently, then merged via min-heap.
    /// With dynamic buffer sizing (see `calculate_optimal_sort_buffer`),
    /// each shard should have ≤50 segments, staying under the FD limit.
    pub fn sort(self) -> std::io::Result<ShardedSortedIterator> {
        self.sort_with_progress(|_, _, _, _| {})
    }

    /// Sort all shards with progress reporting.
    ///
    /// The callback is invoked after each shard completes sorting:
    /// `progress(shard_index, total_shards, records_in_shard, duration_secs)`
    pub fn sort_with_progress<F>(self, progress: F) -> std::io::Result<ShardedSortedIterator>
    where
        F: Fn(usize, usize, usize, f64),
    {
        use std::time::Instant;

        let mut shard_iters: Vec<Box<dyn Iterator<Item = std::io::Result<TileFeatureRecord>>>> =
            Vec::with_capacity(self.num_shards);

        let total_shards = self.num_shards;
        for (idx, shard) in self.shards.into_iter().enumerate() {
            let records_in_shard = shard.len();
            let start = Instant::now();
            let iter = shard.sort()?;
            let duration = start.elapsed().as_secs_f64();
            shard_iters.push(Box::new(iter));
            progress(idx + 1, total_shards, records_in_shard, duration);
        }

        ShardedSortedIterator::new(shard_iters)
    }
}

/// Iterator that merges sorted shards using a min-heap.
///
/// Each shard produces records sorted by tile_id within its partition.
/// Since shards partition tile_id space (shard i has tile_ids where id % N == i),
/// we use a min-heap to always yield the globally smallest tile_id.
pub struct ShardedSortedIterator {
    /// Min-heap of (record, shard_index) pairs, ordered by tile_id
    heap: BinaryHeap<HeapEntry>,
    /// Iterators for each shard
    shard_iters: Vec<Option<Box<dyn Iterator<Item = std::io::Result<TileFeatureRecord>>>>>,
    /// Cached error to return on next iteration
    pending_error: Option<std::io::Error>,
}

/// Entry in the merge heap, ordered by tile_id (min-heap via Reverse ordering).
struct HeapEntry {
    record: TileFeatureRecord,
    shard_idx: usize,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.record.tile_id == other.record.tile_id
            && self.record.feature_id == other.record.feature_id
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering for min-heap (BinaryHeap is max-heap by default)
        other
            .record
            .tile_id
            .cmp(&self.record.tile_id)
            .then_with(|| other.record.feature_id.cmp(&self.record.feature_id))
    }
}

impl ShardedSortedIterator {
    fn new(
        shard_iters: Vec<Box<dyn Iterator<Item = std::io::Result<TileFeatureRecord>>>>,
    ) -> std::io::Result<Self> {
        let mut heap = BinaryHeap::with_capacity(shard_iters.len());
        let mut iters: Vec<Option<Box<dyn Iterator<Item = std::io::Result<TileFeatureRecord>>>>> =
            shard_iters.into_iter().map(Some).collect();

        // Prime the heap with the first record from each shard
        for (shard_idx, iter_opt) in iters.iter_mut().enumerate() {
            if let Some(iter) = iter_opt {
                match iter.next() {
                    Some(Ok(record)) => {
                        heap.push(HeapEntry { record, shard_idx });
                    }
                    Some(Err(e)) => return Err(e),
                    None => {
                        // Shard is empty, mark as exhausted
                        *iter_opt = None;
                    }
                }
            }
        }

        Ok(Self {
            heap,
            shard_iters: iters,
            pending_error: None,
        })
    }

    /// Pull the next record from a shard and push it onto the heap.
    fn refill_from_shard(&mut self, shard_idx: usize) {
        if let Some(iter) = &mut self.shard_iters[shard_idx] {
            match iter.next() {
                Some(Ok(record)) => {
                    self.heap.push(HeapEntry { record, shard_idx });
                }
                Some(Err(e)) => {
                    self.pending_error = Some(e);
                    self.shard_iters[shard_idx] = None;
                }
                None => {
                    // Shard exhausted
                    self.shard_iters[shard_idx] = None;
                }
            }
        }
    }
}

impl Iterator for ShardedSortedIterator {
    type Item = std::io::Result<TileFeatureRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        // Return any pending error first
        if let Some(e) = self.pending_error.take() {
            return Some(Err(e));
        }

        // Pop the smallest record from the heap
        let entry = self.heap.pop()?;

        // Refill from the same shard
        self.refill_from_shard(entry.shard_idx);

        Some(Ok(entry.record))
    }
}

// ==================== Adaptive Consolidation ====================

/// Threshold for adaptive consolidation: if a shard has more segments than this,
/// we consolidate to a single intermediate file to avoid file descriptor exhaustion.
///
/// With 16 shards and this threshold, we stay well under the typical 1024 fd limit:
/// - Max open during consolidation: 50 (one shard) + previous temps
/// - Max open during merge: 16 intermediate files
const SEGMENT_CONSOLIDATION_THRESHOLD: usize = 50;

/// Drains an iterator of `TileFeatureRecord` results into a temporary file.
///
/// This is used during adaptive consolidation: when a shard has too many segments,
/// we drain its sorted output to a single file, closing all segment file handles.
/// The resulting file can then be read back via `ConsolidatedFileIter`.
///
/// # Arguments
///
/// * `iter` - Iterator yielding `Result<TileFeatureRecord>` (typically from `TileFeatureSorter::sort()`)
///
/// # Returns
///
/// A `File` handle positioned at the start, ready for reading.
fn consolidate_to_file<I>(iter: I) -> std::io::Result<File>
where
    I: Iterator<Item = std::io::Result<TileFeatureRecord>>,
{
    // Create temp file - will be automatically cleaned up when dropped
    let temp_file = tempfile::tempfile()?;
    let mut writer = BufWriter::with_capacity(1 << 20, temp_file); // 1MB buffer

    for result in iter {
        let record = result?;
        record.encode(&mut writer)?;
    }

    writer.flush()?;

    // Get the underlying file and seek to start for reading
    let mut file = writer.into_inner()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

/// Iterator that reads `TileFeatureRecord`s from a consolidated temp file.
///
/// Created by `consolidate_to_file()`, this provides streaming access to records
/// that were written to disk during adaptive consolidation.
pub struct ConsolidatedFileIter {
    reader: BufReader<File>,
}

impl ConsolidatedFileIter {
    /// Create a new iterator from a file handle.
    ///
    /// The file should be positioned at the start and contain records
    /// written by `consolidate_to_file()`.
    pub fn new(file: File) -> std::io::Result<Self> {
        Ok(Self {
            reader: BufReader::with_capacity(1 << 20, file), // 1MB buffer
        })
    }
}

impl Iterator for ConsolidatedFileIter {
    type Item = std::io::Result<TileFeatureRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        // Try to decode a record; EOF is signaled by reading 0 bytes for the length prefix
        let mut len_bytes = [0u8; 4];
        match self.reader.read_exact(&mut len_bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return None,
            Err(e) => return Some(Err(e)),
        }

        let len = u32::from_le_bytes(len_bytes) as usize;
        let mut bytes = vec![0u8; len];

        match self.reader.read_exact(&mut bytes) {
            Ok(()) => {}
            Err(e) => return Some(Err(e)),
        }

        match rmp_serde::from_slice(&bytes) {
            Ok(record) => Some(Ok(record)),
            Err(e) => Some(Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e))),
        }
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
    fn test_sortable_encode_decode_roundtrip() {
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
    fn test_sorter_large_dataset() {
        // Test with enough records to potentially trigger external sorting
        let mut sorter = TileFeatureSorter::new(100); // Small buffer to force disk spill

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

    // ==================== ShardedTileFeatureSorter tests ====================

    #[test]
    fn test_sharded_sorter_basic_operations() {
        let mut sorter = ShardedTileFeatureSorter::new(1000);
        assert!(sorter.is_empty());
        assert_eq!(sorter.len(), 0);

        sorter.add(TileFeatureRecord::new(1, 0, 0, 0, 1, vec![], vec![]));
        assert!(!sorter.is_empty());
        assert_eq!(sorter.len(), 1);
    }

    #[test]
    fn test_sharded_sorter_sorts_by_tile_id() {
        let mut sorter = ShardedTileFeatureSorter::with_shards(1000, 4);

        // Add records out of order, spanning multiple shards
        sorter.add(TileFeatureRecord::new(7, 0, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(3, 0, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(1, 0, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(5, 0, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(2, 0, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(6, 0, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(4, 0, 0, 0, 1, vec![], vec![]));
        sorter.add(TileFeatureRecord::new(0, 0, 0, 0, 1, vec![], vec![]));

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted.len(), 8);
        for (i, record) in sorted.iter().enumerate() {
            assert_eq!(
                record.tile_id, i as u64,
                "Record at position {} has wrong tile_id {}",
                i, record.tile_id
            );
        }
    }

    #[test]
    fn test_sharded_sorter_stable_within_tile() {
        let mut sorter = ShardedTileFeatureSorter::with_shards(1000, 4);

        // Multiple features in same tile (same shard since same tile_id)
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
    fn test_sharded_sorter_large_dataset() {
        // Test with enough records to trigger external sorting in each shard
        let mut sorter = ShardedTileFeatureSorter::with_shards(100, 4); // Small buffer to force disk spill

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
    fn test_sharded_sorter_empty() {
        let sorter = ShardedTileFeatureSorter::new(1000);
        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();
        assert!(sorted.is_empty());
    }

    #[test]
    fn test_sharded_sorter_single_shard() {
        // With 1 shard, should behave like non-sharded sorter
        let mut sorter = ShardedTileFeatureSorter::with_shards(1000, 1);

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
    fn test_sharded_sorter_many_shards() {
        // Test with more shards than records
        let mut sorter = ShardedTileFeatureSorter::with_shards(1000, 32);

        for i in 0..10 {
            sorter.add(TileFeatureRecord::new(i, 0, 0, 0, 1, vec![], vec![]));
        }

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted.len(), 10);
        for (i, record) in sorted.iter().enumerate() {
            assert_eq!(record.tile_id, i as u64);
        }
    }

    #[test]
    fn test_sharded_sorter_preserves_data() {
        let mut sorter = ShardedTileFeatureSorter::with_shards(1000, 4);

        let geom = vec![0x01, 0x02, 0x03];
        let props = vec![0x04, 0x05, 0x06];

        sorter.add(TileFeatureRecord::new(
            42,
            10,
            100,
            200,
            999,
            geom.clone(),
            props.clone(),
        ));

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();

        assert_eq!(sorted.len(), 1);
        let record = &sorted[0];
        assert_eq!(record.tile_id, 42);
        assert_eq!(record.z, 10);
        assert_eq!(record.x, 100);
        assert_eq!(record.y, 200);
        assert_eq!(record.feature_id, 999);
        assert_eq!(record.geometry_wkb, geom);
        assert_eq!(record.properties, props);
    }

    // ==================== Adaptive Consolidation Tests ====================

    #[test]
    fn test_estimated_segment_count_empty() {
        let sorter = TileFeatureSorter::new(100);
        assert_eq!(sorter.estimated_segment_count(), 0);
    }

    #[test]
    fn test_estimated_segment_count_under_buffer() {
        let mut sorter = TileFeatureSorter::new(100);
        for i in 0..50 {
            sorter.add(TileFeatureRecord::new(i, 0, 0, 0, i, vec![], vec![]));
        }
        // 50 records with buffer 100 = 1 segment (in-memory, no spill)
        assert_eq!(sorter.estimated_segment_count(), 1);
    }

    #[test]
    fn test_estimated_segment_count_exact_buffer() {
        let mut sorter = TileFeatureSorter::new(100);
        for i in 0..100 {
            sorter.add(TileFeatureRecord::new(i, 0, 0, 0, i, vec![], vec![]));
        }
        // 100 records with buffer 100 = 1 segment
        assert_eq!(sorter.estimated_segment_count(), 1);
    }

    #[test]
    fn test_estimated_segment_count_multiple_segments() {
        let mut sorter = TileFeatureSorter::new(100);
        for i in 0..350 {
            sorter.add(TileFeatureRecord::new(i, 0, 0, 0, i, vec![], vec![]));
        }
        // 350 records with buffer 100 = ceil(350/100) = 4 segments
        assert_eq!(sorter.estimated_segment_count(), 4);
    }

    #[test]
    fn test_estimated_segment_count_large_dataset() {
        let mut sorter = TileFeatureSorter::new(100);
        for i in 0..10250 {
            sorter.add(TileFeatureRecord::new(i, 0, 0, 0, i, vec![], vec![]));
        }
        // 10250 records with buffer 100 = ceil(10250/100) = 103 segments
        assert_eq!(sorter.estimated_segment_count(), 103);
    }

    // ==================== ConsolidatedFileIter Tests ====================

    #[test]
    fn test_consolidate_and_read_back_roundtrip() {
        // Create some records
        let records: Vec<TileFeatureRecord> = (0..10)
            .map(|i| TileFeatureRecord::new(i, 0, 0, 0, i, vec![i as u8], vec![]))
            .collect();

        // Consolidate to file
        let file = consolidate_to_file(records.clone().into_iter().map(Ok)).unwrap();

        // Read back via ConsolidatedFileIter
        let iter = ConsolidatedFileIter::new(file).unwrap();
        let read_back: Vec<_> = iter.map(|r| r.unwrap()).collect();

        assert_eq!(read_back.len(), 10);
        for (i, record) in read_back.iter().enumerate() {
            assert_eq!(record.tile_id, i as u64);
            assert_eq!(record.geometry_wkb, vec![i as u8]);
        }
    }

    #[test]
    fn test_consolidate_empty_iterator() {
        let records: Vec<TileFeatureRecord> = vec![];
        let file = consolidate_to_file(records.into_iter().map(Ok)).unwrap();
        let iter = ConsolidatedFileIter::new(file).unwrap();
        let read_back: Vec<_> = iter.map(|r| r.unwrap()).collect();
        assert!(read_back.is_empty());
    }

    #[test]
    fn test_consolidate_preserves_all_fields() {
        let original = TileFeatureRecord::new(
            12345,
            8,
            100,
            200,
            9999,
            vec![0x01, 0x02, 0x03, 0x04, 0x05],
            vec![0x82, 0xa4, b't', b'e', b's', b't'],
        );

        let file = consolidate_to_file(vec![original.clone()].into_iter().map(Ok)).unwrap();
        let mut iter = ConsolidatedFileIter::new(file).unwrap();

        let read_back = iter.next().unwrap().unwrap();
        assert_eq!(read_back, original);
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_consolidate_large_dataset() {
        // Test with 10K records to ensure streaming works
        let records: Vec<TileFeatureRecord> = (0..10_000)
            .map(|i| {
                TileFeatureRecord::new(
                    i,
                    (i % 15) as u8,
                    (i % 1000) as u32,
                    (i / 1000) as u32,
                    i,
                    vec![(i % 256) as u8; 50], // 50 bytes of geometry
                    vec![(i % 128) as u8; 20], // 20 bytes of properties
                )
            })
            .collect();

        let file = consolidate_to_file(records.clone().into_iter().map(Ok)).unwrap();
        let iter = ConsolidatedFileIter::new(file).unwrap();
        let read_back: Vec<_> = iter.map(|r| r.unwrap()).collect();

        assert_eq!(read_back.len(), 10_000);
        for (i, record) in read_back.iter().enumerate() {
            assert_eq!(record.tile_id, i as u64, "Mismatch at index {}", i);
        }
    }

    // ==================== Adaptive Sort Behavior Tests ====================

    #[test]
    fn test_adaptive_sort_small_dataset_direct_path() {
        // Small dataset: stays under segment threshold, uses direct iteration
        // 100 records / 100 buffer = 1 segment per shard (well under 50 threshold)
        let mut sorter = ShardedTileFeatureSorter::with_shards(100, 4);

        for i in 0..100 {
            sorter.add(TileFeatureRecord::new(i, 0, 0, 0, i, vec![], vec![]));
        }

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(sorted.len(), 100);

        // Verify correct global ordering
        for (i, record) in sorted.iter().enumerate() {
            assert_eq!(record.tile_id, i as u64);
        }
    }

    #[test]
    fn test_adaptive_sort_large_dataset_consolidation_path() {
        // Large dataset: exceeds segment threshold, triggers consolidation
        // 10K records / 10 buffer / 4 shards = ~250 segments per shard (> 50 threshold)
        let mut sorter = ShardedTileFeatureSorter::with_shards(10, 4);

        for i in 0..10_000 {
            sorter.add(TileFeatureRecord::new(
                i,
                0,
                0,
                0,
                i,
                vec![(i % 256) as u8],
                vec![],
            ));
        }

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(sorted.len(), 10_000);

        // Verify correct global ordering
        for (i, record) in sorted.iter().enumerate() {
            assert_eq!(record.tile_id, i as u64, "Mismatch at position {}", i);
        }
    }

    #[test]
    fn test_adaptive_sort_mixed_shard_sizes() {
        // Create uneven distribution: some shards will consolidate, others won't
        // Use buffer=10, 4 shards
        // Add many records to tile_ids 0, 4, 8, 12 (all go to shard 0)
        // Add few records to tile_ids 1, 2, 3 (go to shards 1, 2, 3)
        let mut sorter = ShardedTileFeatureSorter::with_shards(10, 4);

        // Shard 0: 1000 records → ~100 segments (consolidation)
        for i in 0..1000 {
            let tile_id = (i * 4) as u64; // 0, 4, 8, 12, 16, ...
            sorter.add(TileFeatureRecord::new(
                tile_id,
                0,
                0,
                0,
                i as u64,
                vec![],
                vec![],
            ));
        }

        // Shards 1, 2, 3: 10 records each → 1 segment each (direct)
        for shard in 1..4 {
            for i in 0..10 {
                let tile_id = shard + (i * 4); // e.g., shard 1: 1, 5, 9, 13, ...
                sorter.add(TileFeatureRecord::new(
                    tile_id as u64,
                    0,
                    0,
                    0,
                    (shard * 1000 + i) as u64,
                    vec![],
                    vec![],
                ));
            }
        }

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(sorted.len(), 1030);

        // Verify sorted order
        let mut prev_tile_id = 0u64;
        for record in &sorted {
            assert!(
                record.tile_id >= prev_tile_id,
                "Out of order: {} after {}",
                record.tile_id,
                prev_tile_id
            );
            prev_tile_id = record.tile_id;
        }
    }

    #[test]
    fn test_adaptive_sort_preserves_data_through_consolidation() {
        // Large dataset that triggers consolidation, verify data integrity
        let mut sorter = ShardedTileFeatureSorter::with_shards(10, 4);

        let test_records: Vec<TileFeatureRecord> = (0..5000)
            .map(|i| {
                TileFeatureRecord::new(
                    i,
                    (i % 15) as u8,
                    (i % 1000) as u32,
                    (i / 1000) as u32,
                    i * 2,                                         // Distinct feature_id
                    vec![(i % 256) as u8; 10 + (i % 50) as usize], // Variable geometry
                    vec![(i % 128) as u8; 5 + (i % 20) as usize],  // Variable properties
                )
            })
            .collect();

        for record in &test_records {
            sorter.add(record.clone());
        }

        let sorted: Vec<_> = sorter.sort().unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(sorted.len(), 5000);

        // Verify each record's data is preserved
        for record in &sorted {
            let i = record.tile_id;
            assert_eq!(record.z, (i % 15) as u8);
            assert_eq!(record.x, (i % 1000) as u32);
            assert_eq!(record.y, (i / 1000) as u32);
            assert_eq!(record.feature_id, i * 2);
            assert_eq!(record.geometry_wkb.len(), 10 + (i % 50) as usize);
            assert_eq!(record.properties.len(), 5 + (i % 20) as usize);
        }
    }
}
