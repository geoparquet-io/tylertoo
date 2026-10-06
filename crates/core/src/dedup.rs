//! Tile deduplication for PMTiles.
//!
//! Implements content-addressable tile deduplication using XXH3 hashing.
//! Identical tiles are stored once and referenced via PMTiles' `run_length` feature.
//!
//! # Strategy (following tippecanoe)
//!
//! 1. Hash tile content BEFORE compression (raw MVT bytes)
//! 2. Track seen hashes → (offset, length) in deduplication cache
//! 3. For consecutive identical tiles, use `run_length` encoding
//! 4. Track statistics: original count, unique count, savings %

use std::collections::HashMap;

/// Statistics about tile deduplication
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeduplicationStats {
    /// Total number of tiles (including duplicates)
    pub total_tiles: u64,
    /// Number of unique tile contents
    pub unique_tiles: u64,
    /// Number of duplicate tiles eliminated
    pub duplicates_eliminated: u64,
    /// Bytes saved by deduplication (uncompressed)
    pub bytes_saved: u64,
}

impl DeduplicationStats {
    /// Create empty stats
    pub fn new() -> Self {
        Self::default()
    }

    /// Calculate deduplication ratio (0.0 = no dedup, 1.0 = all duplicates)
    pub fn dedup_ratio(&self) -> f64 {
        if self.total_tiles == 0 {
            return 0.0;
        }
        self.duplicates_eliminated as f64 / self.total_tiles as f64
    }

    /// Human-readable savings percentage
    pub fn savings_percent(&self) -> f64 {
        self.dedup_ratio() * 100.0
    }
}

/// Fast tile content hasher using XXH3 (64-bit)
///
/// XXH3 is a non-cryptographic hash optimized for speed.
/// Collision resistance is sufficient for deduplication (not security).
pub struct TileHasher;

impl TileHasher {
    /// Hash tile content (should be uncompressed MVT bytes)
    pub fn hash(data: &[u8]) -> u64 {
        xxhash_rust::xxh3::xxh3_64(data)
    }
}

/// Cache for tracking seen tiles and their storage locations
///
/// Maps content hash → (offset, `compressed_length`) for deduplication
#[derive(Debug, Default)]
pub struct DeduplicationCache {
    /// Hash → (offset in tile data buffer, compressed length)
    seen: HashMap<u64, (u64, u32)>,
    stats: DeduplicationStats,
}

impl DeduplicationCache {
    /// Create a new deduplication cache
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if tile content has been seen before
    ///
    /// Returns Some((offset, length)) if duplicate, None if new
    pub fn check(&self, hash: u64) -> Option<(u64, u32)> {
        self.seen.get(&hash).copied()
    }

    /// Record a new unique tile
    ///
    /// Call this AFTER writing the tile data to get the correct offset
    pub fn record_new(
        &mut self,
        hash: u64,
        offset: u64,
        compressed_len: u32,
        uncompressed_len: u32,
    ) {
        self.seen.insert(hash, (offset, compressed_len));
        self.stats.total_tiles += 1;
        self.stats.unique_tiles += 1;
        // No bytes saved for new tiles
        let _ = uncompressed_len; // unused but kept for API consistency
    }

    /// Record a duplicate tile (content already exists)
    ///
    /// Updates stats but doesn't store - the existing entry is used
    pub fn record_duplicate(&mut self, uncompressed_len: u32) {
        self.stats.total_tiles += 1;
        self.stats.duplicates_eliminated += 1;
        self.stats.bytes_saved += uncompressed_len as u64;
    }

    /// Get current deduplication statistics
    pub fn stats(&self) -> &DeduplicationStats {
        &self.stats
    }

    /// Consume cache and return final statistics
    pub fn into_stats(self) -> DeduplicationStats {
        self.stats
    }

    /// Number of unique tiles seen
    pub fn unique_count(&self) -> usize {
        self.seen.len()
    }
}

// ============================================================================
// Tests (TDD)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // DeduplicationStats Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_dedup_stats_default() {
        let stats = DeduplicationStats::new();
        assert_eq!(stats.total_tiles, 0);
        assert_eq!(stats.unique_tiles, 0);
        assert_eq!(stats.duplicates_eliminated, 0);
        assert_eq!(stats.bytes_saved, 0);
    }

    #[test]
    fn test_dedup_stats_ratio_empty() {
        let stats = DeduplicationStats::new();
        assert_eq!(stats.dedup_ratio(), 0.0);
    }

    #[test]
    fn test_dedup_stats_ratio_no_duplicates() {
        let stats = DeduplicationStats {
            total_tiles: 100,
            unique_tiles: 100,
            duplicates_eliminated: 0,
            bytes_saved: 0,
        };
        assert_eq!(stats.dedup_ratio(), 0.0);
        assert_eq!(stats.savings_percent(), 0.0);
    }

    #[test]
    fn test_dedup_stats_ratio_half_duplicates() {
        let stats = DeduplicationStats {
            total_tiles: 100,
            unique_tiles: 50,
            duplicates_eliminated: 50,
            bytes_saved: 50000,
        };
        assert_eq!(stats.dedup_ratio(), 0.5);
        assert_eq!(stats.savings_percent(), 50.0);
    }

    #[test]
    fn test_dedup_stats_ratio_mostly_duplicates() {
        let stats = DeduplicationStats {
            total_tiles: 1000,
            unique_tiles: 10,
            duplicates_eliminated: 990,
            bytes_saved: 990000,
        };
        assert_eq!(stats.dedup_ratio(), 0.99);
        assert_eq!(stats.savings_percent(), 99.0);
    }

    // -------------------------------------------------------------------------
    // TileHasher Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_hasher_empty_data() {
        let hash = TileHasher::hash(&[]);
        // XXH3 of empty input is a specific value
        assert_ne!(hash, 0); // Not zero (which would be suspicious)
    }

    #[test]
    fn test_hasher_consistent() {
        let data = b"Hello, PMTiles!";
        let hash1 = TileHasher::hash(data);
        let hash2 = TileHasher::hash(data);
        assert_eq!(hash1, hash2, "Same input should produce same hash");
    }

    #[test]
    fn test_hasher_different_data() {
        let hash1 = TileHasher::hash(b"tile A content");
        let hash2 = TileHasher::hash(b"tile B content");
        assert_ne!(
            hash1, hash2,
            "Different input should produce different hash"
        );
    }

    #[test]
    fn test_hasher_mvt_like_data() {
        // Simulate MVT-like binary data (protobuf header + data)
        let mvt1 = vec![0x1a, 0x10, 0x00, 0x01, 0x02, 0x03];
        let mvt2 = vec![0x1a, 0x10, 0x00, 0x01, 0x02, 0x03];
        let mvt3 = vec![0x1a, 0x10, 0x00, 0x01, 0x02, 0x04]; // Different last byte

        let hash1 = TileHasher::hash(&mvt1);
        let hash2 = TileHasher::hash(&mvt2);
        let hash3 = TileHasher::hash(&mvt3);

        assert_eq!(hash1, hash2, "Identical MVT data should have same hash");
        assert_ne!(
            hash1, hash3,
            "Different MVT data should have different hash"
        );
    }

    #[test]
    fn test_hasher_large_tile() {
        // Larger tile data (like a real MVT tile)
        let large_tile: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        let hash = TileHasher::hash(&large_tile);

        // Should complete quickly and produce valid hash
        assert_ne!(hash, 0);

        // Same data should hash the same
        let hash2 = TileHasher::hash(&large_tile);
        assert_eq!(hash, hash2);
    }

    // -------------------------------------------------------------------------
    // DeduplicationCache Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_cache_new_is_empty() {
        let cache = DeduplicationCache::new();
        assert_eq!(cache.unique_count(), 0);
        assert_eq!(cache.stats().total_tiles, 0);
    }

    #[test]
    fn test_cache_check_unseen_returns_none() {
        let cache = DeduplicationCache::new();
        assert!(cache.check(12345).is_none());
    }

    #[test]
    fn test_cache_record_new_tile() {
        let mut cache = DeduplicationCache::new();
        let hash = TileHasher::hash(b"tile content");

        // Record a new tile at offset 0, compressed length 100, uncompressed 200
        cache.record_new(hash, 0, 100, 200);

        assert_eq!(cache.unique_count(), 1);
        assert_eq!(cache.stats().total_tiles, 1);
        assert_eq!(cache.stats().unique_tiles, 1);
        assert_eq!(cache.stats().duplicates_eliminated, 0);
    }

    #[test]
    fn test_cache_check_seen_returns_location() {
        let mut cache = DeduplicationCache::new();
        let hash = TileHasher::hash(b"tile content");

        cache.record_new(hash, 1000, 150, 300);

        let location = cache.check(hash);
        assert_eq!(location, Some((1000, 150)));
    }

    #[test]
    fn test_cache_record_duplicate() {
        let mut cache = DeduplicationCache::new();
        let hash = TileHasher::hash(b"tile content");

        // First occurrence - new tile
        cache.record_new(hash, 0, 100, 200);

        // Second occurrence - duplicate (uncompressed size 200)
        cache.record_duplicate(200);

        assert_eq!(cache.unique_count(), 1); // Still only 1 unique
        assert_eq!(cache.stats().total_tiles, 2);
        assert_eq!(cache.stats().unique_tiles, 1);
        assert_eq!(cache.stats().duplicates_eliminated, 1);
        assert_eq!(cache.stats().bytes_saved, 200);
    }

    #[test]
    fn test_cache_multiple_unique_tiles() {
        let mut cache = DeduplicationCache::new();

        for i in 0..5u32 {
            let data = format!("unique tile {}", i);
            let hash = TileHasher::hash(data.as_bytes());
            cache.record_new(hash, i as u64 * 100, 50, 100);
        }

        assert_eq!(cache.unique_count(), 5);
        assert_eq!(cache.stats().total_tiles, 5);
        assert_eq!(cache.stats().unique_tiles, 5);
        assert_eq!(cache.stats().duplicates_eliminated, 0);
    }

    #[test]
    fn test_cache_mixed_unique_and_duplicate() {
        let mut cache = DeduplicationCache::new();

        // First tile - unique
        let hash1 = TileHasher::hash(b"ocean tile");
        cache.record_new(hash1, 0, 50, 100);

        // Second tile - different, unique
        let hash2 = TileHasher::hash(b"land tile");
        cache.record_new(hash2, 50, 200, 500);

        // Third tile - duplicate of first (ocean tile again)
        cache.record_duplicate(100);

        // Fourth tile - duplicate of first (ocean tile again)
        cache.record_duplicate(100);

        // Fifth tile - duplicate of second
        cache.record_duplicate(500);

        assert_eq!(cache.unique_count(), 2);
        assert_eq!(cache.stats().total_tiles, 5);
        assert_eq!(cache.stats().unique_tiles, 2);
        assert_eq!(cache.stats().duplicates_eliminated, 3);
        assert_eq!(cache.stats().bytes_saved, 700); // 100 + 100 + 500
    }

    #[test]
    fn test_cache_into_stats() {
        let mut cache = DeduplicationCache::new();
        let hash = TileHasher::hash(b"test");
        cache.record_new(hash, 0, 10, 20);
        cache.record_duplicate(20);

        let stats = cache.into_stats();
        assert_eq!(stats.total_tiles, 2);
        assert_eq!(stats.duplicates_eliminated, 1);
    }

    #[test]
    fn test_dedup_ratio_realistic_scenario() {
        // Simulate a global tileset with lots of ocean tiles
        let mut cache = DeduplicationCache::new();

        // 1 ocean tile (appears 700 times)
        let ocean_hash = TileHasher::hash(b"empty ocean tile");
        cache.record_new(ocean_hash, 0, 50, 100);
        for _ in 0..699 {
            cache.record_duplicate(100);
        }

        // 300 unique land tiles
        for i in 0..300 {
            let data = format!("land tile {}", i);
            let hash = TileHasher::hash(data.as_bytes());
            cache.record_new(hash, 50 + i * 200, 200, 500);
        }

        let stats = cache.stats();
        assert_eq!(stats.total_tiles, 1000);
        assert_eq!(stats.unique_tiles, 301);
        assert_eq!(stats.duplicates_eliminated, 699);

        // 69.9% deduplication ratio
        assert!((stats.dedup_ratio() - 0.699).abs() < 0.001);
    }
}
