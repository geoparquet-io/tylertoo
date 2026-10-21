//! Tile content hashing for PMTiles deduplication.
//!
//! [`TileHasher`] keys the PMTiles writers' dedup maps: identical tiles are
//! stored once and referenced again from the directory (`run_length` for
//! consecutive repeats). A hash hit is only a candidate; the writers compare
//! the bytes before they reuse a stored tile (#531).

/// Fast tile content hasher using XXH3 (64-bit)
///
/// XXH3 is a non-cryptographic hash optimized for speed. It is not
/// collision-resistant: two different tiles can share a hash by chance
/// (~3e-4 per archive at 10^8 unique tiles) or by construction, when someone
/// else supplies the tiles. A matching hash therefore marks a candidate
/// duplicate only. The PMTiles writers compare the bytes before they reuse a
/// stored tile (#531).
pub struct TileHasher;

impl TileHasher {
    /// Hash tile content (should be uncompressed MVT bytes)
    pub fn hash(data: &[u8]) -> u64 {
        xxhash_rust::xxh3::xxh3_64(data)
    }
}

// ============================================================================
// Tests (TDD)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
}
