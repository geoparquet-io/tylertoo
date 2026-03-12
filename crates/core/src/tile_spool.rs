//! Tile spool: append-only storage for encoded tiles with sparse deduplication.
//!
//! The tile spool provides a simple, streaming-friendly way to store encoded tiles
//! during tile generation. It's designed for the "sparse spool" pattern where:
//!
//! 1. Tiles are written in arrival order (not tile_id order)
//! 2. Multiple entries for the same tile_id are allowed (late arrivals/updates)
//! 3. Deduplication happens at finalization time - only the LAST entry per tile_id is kept
//!
//! This approach is memory-efficient because:
//! - We don't need to keep all tile data in memory
//! - We only track metadata (SpoolEntry) in memory
//! - Actual tile bytes are written directly to temp file
//!
//! # File Format
//!
//! The spool file is a simple concatenation of gzip-compressed tile data:
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────┐
//! │ offset 0                                                        │
//! │ ┌─────────────────────────────────────────────────────────────┐ │
//! │ │ [gzip-compressed tile data for entry 0]                     │ │
//! │ │ length: entries[0].length bytes                             │ │
//! │ └─────────────────────────────────────────────────────────────┘ │
//! │ offset: entries[0].length                                       │
//! │ ┌─────────────────────────────────────────────────────────────┐ │
//! │ │ [gzip-compressed tile data for entry 1]                     │ │
//! │ │ length: entries[1].length bytes                             │ │
//! │ └─────────────────────────────────────────────────────────────┘ │
//! │ ...                                                             │
//! └─────────────────────────────────────────────────────────────────┘
//! ```
//!
//! # Example
//!
//! ```ignore
//! use gpq_tiles_core::tile_spool::TileSpool;
//!
//! let mut spool = TileSpool::new()?;
//!
//! // Write tiles as they arrive (any order)
//! spool.write_tile(42, &tile_data_1)?;
//! spool.write_tile(17, &tile_data_2)?;
//! spool.write_tile(42, &updated_tile_data)?;  // Update tile 42
//!
//! // Finalize: deduplicate and sort
//! let result = spool.into_sorted_entries()?;
//! // result.entries contains only the LAST entry for each tile_id, sorted
//! ```

use crate::compression::{compress, Compression};
use crate::streaming_types::{SpoolEntry, SpoolResult};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::PathBuf;

/// Tile spool for append-only storage with sparse deduplication.
///
/// Stores encoded tiles in a temp file and tracks entries in memory.
/// Multiple writes to the same tile_id are allowed - only the last one is kept.
pub struct TileSpool {
    /// Buffered writer for the temp file
    file: BufWriter<File>,
    /// Path to the temp file
    path: PathBuf,
    /// Entries tracking each tile write (may have duplicates per tile_id)
    entries: Vec<SpoolEntry>,
    /// Current write offset in the file
    offset: u64,
}

impl TileSpool {
    /// Create a new tile spool with a temp file.
    ///
    /// The temp file is created in the system temp directory with a unique name.
    /// It will be retained after `into_sorted_entries()` returns, as the PMTiles
    /// writer needs to read from it.
    pub fn new() -> io::Result<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::time::{SystemTime, UNIX_EPOCH};

        // Use a combination of PID, timestamp, and atomic counter for uniqueness
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        let path = std::env::temp_dir().join(format!(
            "gpq-tiles-spool-{}-{}-{}.tmp",
            std::process::id(),
            timestamp,
            count
        ));
        Self::with_path(path)
    }

    /// Create a tile spool at a specific path.
    ///
    /// Useful for testing or when you need control over file placement.
    pub fn with_path(path: PathBuf) -> io::Result<Self> {
        let file = File::create(&path)?;
        let file = BufWriter::with_capacity(64 * 1024, file); // 64KB buffer

        Ok(Self {
            file,
            path,
            entries: Vec::new(),
            offset: 0,
        })
    }

    /// Write a tile to the spool.
    ///
    /// The tile data is gzip-compressed before writing. An entry is appended
    /// to track the tile_id, offset, and length.
    ///
    /// # Arguments
    ///
    /// * `tile_id` - PMTiles tile_id (z/x/y encoded as u64)
    /// * `data` - Uncompressed tile data (typically MVT-encoded protobuf)
    ///
    /// # Returns
    ///
    /// The length of the compressed data written.
    pub fn write_tile(&mut self, tile_id: u64, data: &[u8]) -> io::Result<u32> {
        // Compress the tile data with gzip
        let compressed = compress(data, Compression::Gzip)?;
        let length = compressed.len() as u32;

        // Write to file
        self.file.write_all(&compressed)?;

        // Track the entry
        self.entries.push(SpoolEntry {
            tile_id,
            spool_offset: self.offset,
            length,
        });

        // Update offset for next write
        self.offset += length as u64;

        Ok(length)
    }

    /// Write pre-compressed tile data to the spool.
    ///
    /// Use this when the tile data is already compressed (e.g., from another spool).
    ///
    /// # Arguments
    ///
    /// * `tile_id` - PMTiles tile_id (z/x/y encoded as u64)
    /// * `compressed_data` - Pre-compressed tile data
    pub fn write_tile_compressed(
        &mut self,
        tile_id: u64,
        compressed_data: &[u8],
    ) -> io::Result<u32> {
        let length = compressed_data.len() as u32;

        // Write to file
        self.file.write_all(compressed_data)?;

        // Track the entry
        self.entries.push(SpoolEntry {
            tile_id,
            spool_offset: self.offset,
            length,
        });

        // Update offset for next write
        self.offset += length as u64;

        Ok(length)
    }

    /// Get the number of entries written (may include duplicates).
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// Get the total bytes written to the spool file.
    pub fn bytes_written(&self) -> u64 {
        self.offset
    }

    /// Get the path to the spool file.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Finalize the spool: flush, deduplicate, and sort entries.
    ///
    /// This consumes the spool and returns a `SpoolResult` containing:
    /// - The path to the spool file (for the PMTiles writer to read from)
    /// - Deduplicated and sorted entries (only the LAST entry per tile_id is kept)
    ///
    /// # Deduplication Strategy
    ///
    /// When multiple entries exist for the same tile_id, only the LAST one is kept.
    /// This supports the "sparse spool" pattern where late-arriving features can
    /// trigger a tile update.
    pub fn into_sorted_entries(mut self) -> io::Result<SpoolResult> {
        // Flush any buffered writes
        self.file.flush()?;

        // Deduplicate: keep only the LAST entry for each tile_id
        // We iterate in reverse order, inserting into a HashMap
        // This naturally keeps the last entry for each tile_id
        let mut deduped: HashMap<u64, SpoolEntry> = HashMap::with_capacity(self.entries.len());

        for entry in self.entries.into_iter().rev() {
            // Only insert if we haven't seen this tile_id yet (which means
            // we'd be keeping an earlier entry, but since we're iterating
            // in reverse, the first one we see is actually the last written)
            deduped.entry(entry.tile_id).or_insert(entry);
        }

        // Collect and sort by tile_id
        let mut entries: Vec<SpoolEntry> = deduped.into_values().collect();
        entries.sort_by_key(|e| e.tile_id);

        Ok(SpoolResult {
            path: self.path,
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::fs;
    use std::io::Read;

    // -------------------------------------------------------------------------
    // Unit Tests for TileSpool
    // -------------------------------------------------------------------------

    #[test]
    fn test_new_creates_temp_file() {
        let spool = TileSpool::new().expect("Should create spool");
        assert!(spool.path().exists(), "Temp file should exist");
        assert_eq!(spool.entry_count(), 0);
        assert_eq!(spool.bytes_written(), 0);

        // Cleanup
        let path = spool.path().clone();
        drop(spool);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_write_tile_compresses_data() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Write a compressible tile
        let data = b"Hello, PMTiles! ".repeat(100);
        let written_len = spool.write_tile(42, &data).expect("Should write tile");

        // Verify compression happened
        assert!(
            written_len < data.len() as u32,
            "Compressed size {} should be less than original {}",
            written_len,
            data.len()
        );
        assert_eq!(spool.entry_count(), 1);
        assert_eq!(spool.bytes_written(), written_len as u64);

        // Cleanup
        let path = spool.path().clone();
        drop(spool);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_write_multiple_tiles() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Write multiple tiles
        let tile1 = b"Tile 1 data";
        let tile2 = b"Tile 2 data with more content";
        let tile3 = b"Tile 3";

        let len1 = spool.write_tile(1, tile1).expect("Write tile 1");
        let len2 = spool.write_tile(2, tile2).expect("Write tile 2");
        let len3 = spool.write_tile(3, tile3).expect("Write tile 3");

        assert_eq!(spool.entry_count(), 3);
        assert_eq!(spool.bytes_written(), (len1 + len2 + len3) as u64);

        // Cleanup
        let path = spool.path().clone();
        drop(spool);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_write_tile_compressed() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Pre-compress some data
        let data = b"Pre-compressed tile data";
        let compressed = compress(data, Compression::Gzip).expect("Compress");

        let written_len = spool
            .write_tile_compressed(42, &compressed)
            .expect("Should write compressed tile");

        assert_eq!(written_len, compressed.len() as u32);
        assert_eq!(spool.entry_count(), 1);

        // Cleanup
        let path = spool.path().clone();
        drop(spool);
        let _ = fs::remove_file(path);
    }

    // -------------------------------------------------------------------------
    // Deduplication Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_deduplication_keeps_last_entry() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Write the same tile_id multiple times
        spool.write_tile(42, b"First version").expect("Write v1");
        spool.write_tile(42, b"Second version").expect("Write v2");
        spool
            .write_tile(42, b"Third version - final")
            .expect("Write v3");

        assert_eq!(spool.entry_count(), 3, "Should have 3 entries before dedup");

        let result = spool.into_sorted_entries().expect("Finalize");

        // Should only have 1 entry after dedup
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].tile_id, 42);

        // Verify the entry points to the last written data
        // The last entry should have the highest offset
        let entry = &result.entries[0];

        // Read and decompress the data
        let mut file = File::open(&result.path).expect("Open spool file");
        let mut compressed = vec![0u8; entry.length as usize];
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(entry.spool_offset))
            .expect("Seek");
        file.read_exact(&mut compressed).expect("Read");

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).expect("Decompress");

        assert_eq!(
            decompressed, b"Third version - final",
            "Should keep the last version"
        );

        // Cleanup
        let _ = fs::remove_file(&result.path);
    }

    #[test]
    fn test_deduplication_with_multiple_tile_ids() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Write tiles in arbitrary order with some duplicates
        spool.write_tile(3, b"Tile 3 v1").expect("Write");
        spool.write_tile(1, b"Tile 1 v1").expect("Write");
        spool.write_tile(2, b"Tile 2 v1").expect("Write");
        spool.write_tile(1, b"Tile 1 v2 - updated").expect("Write");
        spool.write_tile(3, b"Tile 3 v2 - final").expect("Write");

        assert_eq!(spool.entry_count(), 5);

        let result = spool.into_sorted_entries().expect("Finalize");

        // Should have 3 unique tile_ids
        assert_eq!(result.entries.len(), 3);

        // Should be sorted by tile_id
        assert_eq!(result.entries[0].tile_id, 1);
        assert_eq!(result.entries[1].tile_id, 2);
        assert_eq!(result.entries[2].tile_id, 3);

        // Cleanup
        let _ = fs::remove_file(&result.path);
    }

    // -------------------------------------------------------------------------
    // Sorting Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_entries_sorted_by_tile_id() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Write in reverse order
        for tile_id in (0..100).rev() {
            spool
                .write_tile(tile_id, &format!("Tile {}", tile_id).into_bytes())
                .expect("Write");
        }

        let result = spool.into_sorted_entries().expect("Finalize");

        // Verify sorted order
        for (i, entry) in result.entries.iter().enumerate() {
            assert_eq!(
                entry.tile_id, i as u64,
                "Entry at position {} should have tile_id {}",
                i, i
            );
        }

        // Cleanup
        let _ = fs::remove_file(&result.path);
    }

    // -------------------------------------------------------------------------
    // Integration Tests - Round Trip
    // -------------------------------------------------------------------------

    #[test]
    fn test_round_trip_write_and_read() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Test data with known content
        let tiles: Vec<(u64, Vec<u8>)> = vec![
            (1, b"Tile one data with some content".to_vec()),
            (5, b"Tile five - larger data ".repeat(50)),
            (42, b"The answer tile".to_vec()),
        ];

        // Write all tiles
        for (tile_id, data) in &tiles {
            spool.write_tile(*tile_id, data).expect("Write tile");
        }

        let result = spool.into_sorted_entries().expect("Finalize");

        assert_eq!(result.entries.len(), tiles.len());

        // Verify each tile can be read back correctly
        let mut file = File::open(&result.path).expect("Open spool file");

        for (expected_id, expected_data) in &tiles {
            // Find the entry for this tile_id
            let entry = result
                .entries
                .iter()
                .find(|e| e.tile_id == *expected_id)
                .expect("Should find entry");

            // Read compressed data
            let mut compressed = vec![0u8; entry.length as usize];
            use std::io::Seek;
            file.seek(std::io::SeekFrom::Start(entry.spool_offset))
                .expect("Seek");
            file.read_exact(&mut compressed).expect("Read");

            // Decompress
            let mut decoder = GzDecoder::new(&compressed[..]);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed).expect("Decompress");

            assert_eq!(
                decompressed, *expected_data,
                "Round-trip failed for tile_id {}",
                expected_id
            );
        }

        // Cleanup
        let _ = fs::remove_file(&result.path);
    }

    #[test]
    fn test_empty_spool() {
        let spool = TileSpool::new().expect("Should create spool");

        let result = spool.into_sorted_entries().expect("Finalize");

        assert!(result.entries.is_empty());
        assert!(result.path.exists());

        // Cleanup
        let _ = fs::remove_file(&result.path);
    }

    #[test]
    fn test_empty_tile_data() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Write an empty tile
        spool.write_tile(1, &[]).expect("Write empty tile");

        let result = spool.into_sorted_entries().expect("Finalize");

        assert_eq!(result.entries.len(), 1);

        // Verify we can read back empty data
        let entry = &result.entries[0];
        let mut file = File::open(&result.path).expect("Open spool file");
        let mut compressed = vec![0u8; entry.length as usize];
        use std::io::Seek;
        file.seek(std::io::SeekFrom::Start(entry.spool_offset))
            .expect("Seek");
        file.read_exact(&mut compressed).expect("Read");

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).expect("Decompress");

        assert!(decompressed.is_empty(), "Should be empty after decompress");

        // Cleanup
        let _ = fs::remove_file(&result.path);
    }

    // -------------------------------------------------------------------------
    // Edge Cases
    // -------------------------------------------------------------------------

    #[test]
    fn test_large_tile_data() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Write a large tile (~1MB)
        let large_data = vec![0x42u8; 1_000_000];
        let written_len = spool.write_tile(1, &large_data).expect("Write large tile");

        // Gzip should compress this well (uniform data)
        assert!(
            written_len < 10_000,
            "Uniform data should compress to < 10KB, got {}",
            written_len
        );

        let result = spool.into_sorted_entries().expect("Finalize");
        assert_eq!(result.entries.len(), 1);

        // Cleanup
        let _ = fs::remove_file(&result.path);
    }

    #[test]
    fn test_many_entries() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Write many tiles
        let count = 10_000;
        for i in 0..count {
            spool
                .write_tile(i, &format!("Tile {}", i).into_bytes())
                .expect("Write");
        }

        assert_eq!(spool.entry_count(), count as usize);

        let result = spool.into_sorted_entries().expect("Finalize");

        assert_eq!(result.entries.len(), count as usize);

        // Verify sorted
        for (i, entry) in result.entries.iter().enumerate() {
            assert_eq!(entry.tile_id, i as u64);
        }

        // Cleanup
        let _ = fs::remove_file(&result.path);
    }

    #[test]
    fn test_high_tile_ids() {
        let mut spool = TileSpool::new().expect("Should create spool");

        // Use very high tile_ids (like those at zoom 14+)
        let high_ids: Vec<u64> = vec![
            u64::MAX - 100,
            u64::MAX - 50,
            u64::MAX - 1,
            1_000_000_000,
            5_000_000_000,
        ];

        for (i, &tile_id) in high_ids.iter().enumerate() {
            spool
                .write_tile(tile_id, &format!("High tile {}", i).into_bytes())
                .expect("Write");
        }

        let result = spool.into_sorted_entries().expect("Finalize");

        assert_eq!(result.entries.len(), high_ids.len());

        // Verify sorted (ascending)
        let mut sorted_ids = high_ids.clone();
        sorted_ids.sort();
        for (entry, expected_id) in result.entries.iter().zip(sorted_ids.iter()) {
            assert_eq!(entry.tile_id, *expected_id);
        }

        // Cleanup
        let _ = fs::remove_file(&result.path);
    }

    #[test]
    fn test_with_custom_path() {
        let custom_path = std::env::temp_dir().join("custom-spool-test.tmp");

        let mut spool = TileSpool::with_path(custom_path.clone()).expect("Create with custom path");

        spool.write_tile(1, b"Test data").expect("Write");

        let result = spool.into_sorted_entries().expect("Finalize");

        assert_eq!(result.path, custom_path);
        assert!(custom_path.exists());

        // Cleanup
        let _ = fs::remove_file(&custom_path);
    }
}
