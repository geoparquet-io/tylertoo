//! PMTiles v3 writer implementation.
//!
//! Implements the PMTiles v3 spec: https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md
//!
//! Key design decisions:
//! - Uses Hilbert curve ordering for tile IDs (spatial locality)
//! - Delta-encoded directories for better compression
//! - Configurable compression (gzip, brotli, zstd) for both directories and tiles
//! - Clustered mode for efficient sequential reads

use crate::compression::{self, Compression};
use crate::dedup::{DeduplicationCache, DeduplicationStats, TileHasher};
use crate::tile::TileBounds;
use crate::{Error, Result};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// PMTiles v3 magic number
const PMTILES_MAGIC: &[u8; 7] = b"PMTiles";
const PMTILES_VERSION: u8 = 3;

/// Tile type enumeration (byte 99 in header)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TileType {
    Unknown = 0,
    Mvt = 1,
    Png = 2,
    Jpeg = 3,
    Webp = 4,
    Avif = 5,
}

// Compression enum is now imported from crate::compression

/// PMTiles v3 header (127 bytes)
///
/// Layout follows the spec exactly:
/// - Bytes 0-6: Magic "PMTiles"
/// - Byte 7: Version (3)
/// - Bytes 8-95: Offsets and lengths (8 u64s)
/// - Bytes 96-99: Flags (clustered, compression, type)
/// - Bytes 100-101: Zoom levels
/// - Bytes 102-117: Bounds (min_lon, min_lat, max_lon, max_lat as i32 * 10_000_000)
/// - Bytes 118-126: Center (zoom, lon, lat)
#[derive(Debug, Clone)]
pub struct Header {
    pub root_dir_offset: u64,
    pub root_dir_length: u64,
    pub json_metadata_offset: u64,
    pub json_metadata_length: u64,
    pub leaf_dirs_offset: u64,
    pub leaf_dirs_length: u64,
    pub tile_data_offset: u64,
    pub tile_data_length: u64,
    pub addressed_tiles_count: u64,
    pub tile_entries_count: u64,
    pub tile_contents_count: u64,
    pub clustered: bool,
    pub internal_compression: Compression,
    pub tile_compression: Compression,
    pub tile_type: TileType,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub min_lon: f64,
    pub min_lat: f64,
    pub max_lon: f64,
    pub max_lat: f64,
    pub center_zoom: u8,
    pub center_lon: f64,
    pub center_lat: f64,
}

impl Default for Header {
    fn default() -> Self {
        Self {
            root_dir_offset: 127, // Immediately after header
            root_dir_length: 0,
            json_metadata_offset: 0,
            json_metadata_length: 0,
            leaf_dirs_offset: 0,
            leaf_dirs_length: 0,
            tile_data_offset: 0,
            tile_data_length: 0,
            addressed_tiles_count: 0,
            tile_entries_count: 0,
            tile_contents_count: 0,
            clustered: true,
            internal_compression: Compression::Gzip,
            tile_compression: Compression::Gzip,
            tile_type: TileType::Mvt,
            min_zoom: 0,
            max_zoom: 14,
            min_lon: -180.0,
            min_lat: -85.0,
            max_lon: 180.0,
            max_lat: 85.0,
            center_zoom: 0,
            center_lon: 0.0,
            center_lat: 0.0,
        }
    }
}

impl Header {
    /// Serialize header to exactly 127 bytes
    ///
    /// Position encoding follows the spec: multiply by 10,000,000 and store as i32 LE
    pub fn to_bytes(&self) -> [u8; 127] {
        let mut buf = [0u8; 127];

        // Magic (7 bytes) + Version (1 byte)
        buf[0..7].copy_from_slice(PMTILES_MAGIC);
        buf[7] = PMTILES_VERSION;

        // Offsets and lengths (8 bytes each, little-endian)
        buf[8..16].copy_from_slice(&self.root_dir_offset.to_le_bytes());
        buf[16..24].copy_from_slice(&self.root_dir_length.to_le_bytes());
        buf[24..32].copy_from_slice(&self.json_metadata_offset.to_le_bytes());
        buf[32..40].copy_from_slice(&self.json_metadata_length.to_le_bytes());
        buf[40..48].copy_from_slice(&self.leaf_dirs_offset.to_le_bytes());
        buf[48..56].copy_from_slice(&self.leaf_dirs_length.to_le_bytes());
        buf[56..64].copy_from_slice(&self.tile_data_offset.to_le_bytes());
        buf[64..72].copy_from_slice(&self.tile_data_length.to_le_bytes());

        // Tile counts
        buf[72..80].copy_from_slice(&self.addressed_tiles_count.to_le_bytes());
        buf[80..88].copy_from_slice(&self.tile_entries_count.to_le_bytes());
        buf[88..96].copy_from_slice(&self.tile_contents_count.to_le_bytes());

        // Clustered flag
        buf[96] = if self.clustered { 1 } else { 0 };

        // Compression and type
        buf[97] = self.internal_compression as u8;
        buf[98] = self.tile_compression as u8;
        buf[99] = self.tile_type as u8;

        // Zoom levels
        buf[100] = self.min_zoom;
        buf[101] = self.max_zoom;

        // Bounds: lon/lat as i32 * 10,000,000 (spec-compliant encoding)
        let encode_coord = |v: f64| -> [u8; 4] { ((v * 10_000_000.0) as i32).to_le_bytes() };

        buf[102..106].copy_from_slice(&encode_coord(self.min_lon));
        buf[106..110].copy_from_slice(&encode_coord(self.min_lat));
        buf[110..114].copy_from_slice(&encode_coord(self.max_lon));
        buf[114..118].copy_from_slice(&encode_coord(self.max_lat));

        // Center: zoom + lon/lat
        buf[118] = self.center_zoom;
        buf[119..123].copy_from_slice(&encode_coord(self.center_lon));
        buf[123..127].copy_from_slice(&encode_coord(self.center_lat));

        buf
    }
}

/// Convert tile coordinates (z, x, y) to a TileID for PMTiles
///
/// Uses Hilbert curve ordering for spatial locality. The tile ID is a cumulative
/// position on the series of Hilbert curves starting at zoom level 0.
///
/// Examples from spec:
/// - Z=0, X=0, Y=0 → TileID=0
/// - Z=1, X=0, Y=0 → TileID=1
/// - Z=1, X=0, Y=1 → TileID=2
/// - Z=1, X=1, Y=1 → TileID=3
/// - Z=1, X=1, Y=0 → TileID=4
/// - Z=2, X=0, Y=0 → TileID=5
pub fn tile_id(z: u8, x: u32, y: u32) -> u64 {
    if z == 0 {
        return 0;
    }

    // Calculate base ID: sum of all tiles in previous zoom levels
    // At zoom z, there are 4^z tiles. Base for zoom z is sum of 4^i for i in 1..z
    let base_id: u64 = (1..z as u64).map(|i| 4u64.pow(i as u32)).sum();
    let hilbert_idx = xy_to_hilbert(z, x, y);
    base_id + hilbert_idx + 1
}

/// Convert x,y coordinates to Hilbert curve index at zoom level z
///
/// Implementation follows the standard Hilbert curve algorithm:
/// https://en.wikipedia.org/wiki/Hilbert_curve
fn xy_to_hilbert(z: u8, x: u32, y: u32) -> u64 {
    let n = 1u32 << z;
    let mut rx: u32;
    let mut ry: u32;
    let mut s: u32;
    let mut d: u64 = 0;
    let mut x = x;
    let mut y = y;

    s = n / 2;
    while s > 0 {
        rx = if (x & s) > 0 { 1 } else { 0 };
        ry = if (y & s) > 0 { 1 } else { 0 };
        d += (s as u64) * (s as u64) * ((3 * rx) ^ ry) as u64;

        // Rotate quadrant - use n-1 (full grid size - 1) not s-1
        if ry == 0 {
            if rx == 1 {
                x = n - 1 - x;
                y = n - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
}

// ============================================================================
// Task 8: Directory Encoding
// ============================================================================

/// A directory entry pointing to tile data
///
/// In PMTiles, directories are columnar: all tile_ids are stored together,
/// then all run_lengths, then all lengths, then all offsets.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32, // Number of consecutive tiles with same data (0 = leaf directory)
}

/// Encode a u64 as a varint (protobuf-style, little-endian)
///
/// Each byte uses 7 bits for data, MSB indicates continuation.
pub fn encode_varint(mut value: u64, buf: &mut Vec<u8>) {
    while value >= 0x80 {
        buf.push((value as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Decode a varint from bytes
///
/// Returns (value, bytes_consumed) or None if invalid/incomplete.
pub fn decode_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate() {
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
        if shift >= 64 {
            return None; // Overflow
        }
    }
    None
}

/// Encode directory entries in PMTiles columnar format with delta encoding
///
/// Format: count, delta_tile_ids[], run_lengths[], lengths[], offsets[]
/// All values are varints. Tile IDs use simple delta encoding.
///
/// Offset encoding follows the PMTiles v3 spec:
/// - If offset equals expected position (contiguous), encode as 0
/// - Otherwise, encode as offset + 1
///
/// This allows efficient representation of contiguous tile data (common case).
pub fn encode_directory(entries: &[DirEntry]) -> Vec<u8> {
    let mut buf = Vec::new();

    // Number of entries
    encode_varint(entries.len() as u64, &mut buf);

    if entries.is_empty() {
        return buf;
    }

    // Delta-encoded tile IDs
    let mut last_id = 0u64;
    for entry in entries {
        encode_varint(entry.tile_id - last_id, &mut buf);
        last_id = entry.tile_id;
    }

    // Run lengths
    for entry in entries {
        encode_varint(entry.run_length as u64, &mut buf);
    }

    // Lengths
    for entry in entries {
        encode_varint(entry.length as u64, &mut buf);
    }

    // Offset encoding per PMTiles v3 spec:
    // - For contiguous entries (offset == expected_offset): encode 0
    // - Otherwise: encode offset + 1
    let mut expected_offset = 0u64;
    for (i, entry) in entries.iter().enumerate() {
        let is_contiguous = i > 0 && entry.offset == expected_offset;
        if is_contiguous {
            encode_varint(0, &mut buf);
        } else {
            encode_varint(entry.offset + 1, &mut buf);
        }

        // Update expected offset for next entry (only if this entry has data)
        if entry.run_length > 0 {
            expected_offset = entry.offset + entry.length as u64;
        }
    }

    buf
}

/// Decode directory entries from PMTiles columnar format
///
/// This is the inverse of encode_directory, used for reading and testing.
pub fn decode_directory(data: &[u8]) -> Option<Vec<DirEntry>> {
    let mut offset = 0;

    // Number of entries
    let (count, consumed) = decode_varint(&data[offset..])?;
    offset += consumed;
    let count = count as usize;

    if count == 0 {
        return Some(Vec::new());
    }

    let mut entries = Vec::with_capacity(count);

    // Decode delta-encoded tile IDs
    let mut last_id = 0u64;
    for _ in 0..count {
        let (delta, consumed) = decode_varint(&data[offset..])?;
        offset += consumed;
        last_id += delta;
        entries.push(DirEntry {
            tile_id: last_id,
            offset: 0,
            length: 0,
            run_length: 0,
        });
    }

    // Decode run lengths
    for entry in entries.iter_mut() {
        let (run_length, consumed) = decode_varint(&data[offset..])?;
        offset += consumed;
        entry.run_length = run_length as u32;
    }

    // Decode lengths
    for entry in entries.iter_mut() {
        let (length, consumed) = decode_varint(&data[offset..])?;
        offset += consumed;
        entry.length = length as u32;
    }

    // Decode offsets (with contiguous encoding)
    let mut expected_offset = 0u64;
    for (i, entry) in entries.iter_mut().enumerate() {
        let (encoded_offset, consumed) = decode_varint(&data[offset..])?;
        offset += consumed;

        if encoded_offset == 0 && i > 0 {
            // Contiguous: use expected offset
            entry.offset = expected_offset;
        } else {
            // Explicit offset (stored as offset + 1)
            entry.offset = encoded_offset.saturating_sub(1);
        }

        // Update expected offset for next entry
        if entry.run_length > 0 {
            expected_offset = entry.offset + entry.length as u64;
        }
    }

    Some(entries)
}

// ============================================================================
// Leaf Directory Support (Issue #88)
// ============================================================================

/// Maximum size for root directory to fit in initial 16KB HTTP range request.
/// PMTiles header is 127 bytes, leaving 16384 - 127 = 16257 bytes for root directory.
const MAX_ROOT_DIR_BYTES: usize = 16384 - 127;

/// Initial leaf size when partitioning entries (matches tippecanoe)
const INITIAL_LEAF_SIZE: usize = 4096;

/// Result of building root and leaf directories
#[derive(Debug)]
pub struct DirectoryLayout {
    /// Compressed root directory (may contain leaf pointers or direct tile entries)
    pub root_bytes: Vec<u8>,
    /// Compressed leaf directories concatenated (empty if no leaves needed)
    pub leaves_bytes: Vec<u8>,
    /// Number of leaf directories (0 if all entries fit in root)
    pub num_leaves: usize,
}

/// Build leaf directories by partitioning entries into chunks.
///
/// Each chunk becomes a leaf directory. The root directory contains
/// pointers to these leaves (entries with run_length=0).
///
/// # Arguments
/// * `entries` - All tile directory entries
/// * `leaf_size` - Number of entries per leaf directory
/// * `compression` - Compression algorithm to use
fn build_root_leaves(
    entries: &[DirEntry],
    leaf_size: usize,
    compression: Compression,
) -> std::io::Result<DirectoryLayout> {
    let mut root_entries = Vec::new();
    let mut leaves_bytes = Vec::new();
    let mut num_leaves = 0;

    // Partition entries into leaf directories
    for chunk in entries.chunks(leaf_size) {
        num_leaves += 1;

        // Serialize and compress this leaf
        let leaf_encoded = encode_directory(chunk);
        let leaf_compressed = compression::compress(&leaf_encoded, compression)?;

        // Root entry points to this leaf:
        // - tile_id = first tile ID in this leaf
        // - offset = position within leaves_bytes
        // - length = size of compressed leaf
        // - run_length = 0 (indicates leaf pointer, not tile entry)
        root_entries.push(DirEntry {
            tile_id: chunk[0].tile_id,
            offset: leaves_bytes.len() as u64,
            length: leaf_compressed.len() as u32,
            run_length: 0, // CRITICAL: 0 means this is a leaf pointer
        });

        leaves_bytes.extend(leaf_compressed);
    }

    // Serialize and compress root directory
    let root_encoded = encode_directory(&root_entries);
    let root_compressed = compression::compress(&root_encoded, compression)?;

    Ok(DirectoryLayout {
        root_bytes: root_compressed,
        leaves_bytes,
        num_leaves,
    })
}

/// Create optimized directory structure, using leaf directories if needed.
///
/// Follows the tippecanoe algorithm:
/// 1. Try to fit all entries in a single root directory
/// 2. If root exceeds MAX_ROOT_DIR_BYTES, partition into leaf directories
/// 3. If root still exceeds limit, double leaf_size and retry
///
/// This ensures the root directory always fits in the initial HTTP range request,
/// which is critical for pmtiles-js and other clients that fetch 16KB initially.
///
/// # Arguments
/// * `entries` - All tile directory entries (must be sorted by tile_id)
/// * `compression` - Compression algorithm to use
pub fn make_root_leaves(
    entries: &[DirEntry],
    compression: Compression,
) -> std::io::Result<DirectoryLayout> {
    // Try single directory first (no leaves)
    let single_encoded = encode_directory(entries);
    let single_compressed = compression::compress(&single_encoded, compression)?;

    if single_compressed.len() <= MAX_ROOT_DIR_BYTES {
        // Fits in root - no leaf directories needed
        return Ok(DirectoryLayout {
            root_bytes: single_compressed,
            leaves_bytes: Vec::new(),
            num_leaves: 0,
        });
    }

    // Need leaf directories - iterate with increasing leaf_size until root fits
    let mut leaf_size = INITIAL_LEAF_SIZE;

    loop {
        let layout = build_root_leaves(entries, leaf_size, compression)?;

        if layout.root_bytes.len() <= MAX_ROOT_DIR_BYTES {
            return Ok(layout);
        }

        // Root still too big - double leaf_size (fewer, larger leaves = smaller root)
        leaf_size *= 2;

        // Safety check: if leaf_size exceeds entry count, something is wrong
        if leaf_size > entries.len() * 2 {
            // Fall back to single leaf containing everything
            // (This shouldn't happen in practice)
            return build_root_leaves(entries, entries.len(), compression);
        }
    }
}

/// Compress data with gzip (backward compatibility wrapper)
pub fn gzip_compress(data: &[u8]) -> std::io::Result<Vec<u8>> {
    compression::compress(data, Compression::Gzip)
}

// ============================================================================
// Task 9: Full PMTiles Writer
// ============================================================================

/// Tile entry with hash for deduplication
#[derive(Debug, Clone)]
struct TileEntry {
    /// Compressed tile data (only stored for unique tiles)
    data: Option<Vec<u8>>,
    /// Hash of uncompressed content (for deduplication)
    hash: u64,
    /// Uncompressed size (for stats)
    #[allow(dead_code)] // Reserved for future deduplication stats reporting
    uncompressed_size: u32,
}

/// PMTiles v3 writer
///
/// Accumulates tiles in memory (sorted by tile_id via BTreeMap),
/// then writes the complete archive on finalize.
///
/// Supports tile deduplication: identical tiles are stored once and
/// referenced via PMTiles' `run_length` feature.
pub struct PmtilesWriter {
    /// tile_id -> tile entry (data + hash)
    tiles: BTreeMap<u64, TileEntry>,
    min_zoom: u8,
    max_zoom: u8,
    bounds: TileBounds,
    layer_name: String,
    /// Field metadata: field name -> MVT type ("String", "Number", "Boolean")
    fields: HashMap<String, String>,
    /// Total feature count across all tiles
    total_features: u64,
    /// Feature count per zoom level
    features_per_zoom: HashMap<u8, u64>,
    /// Compression algorithm for tile data
    tile_compression: Compression,
    /// Compression algorithm for internal data (directories, metadata)
    internal_compression: Compression,
    /// Whether deduplication is enabled
    dedup_enabled: bool,
    /// Deduplication cache for tracking seen tiles
    dedup_cache: DeduplicationCache,
}

impl PmtilesWriter {
    /// Create a new PMTiles writer with default gzip compression
    ///
    /// Deduplication is disabled by default for backward compatibility.
    /// Call `enable_deduplication(true)` to enable it.
    pub fn new() -> Self {
        Self {
            tiles: BTreeMap::new(),
            min_zoom: 255,
            max_zoom: 0,
            bounds: TileBounds::empty(),
            layer_name: "layer".to_string(),
            fields: HashMap::new(),
            total_features: 0,
            features_per_zoom: HashMap::new(),
            tile_compression: Compression::Gzip,
            internal_compression: Compression::Gzip,
            dedup_enabled: false,
            dedup_cache: DeduplicationCache::new(),
        }
    }

    /// Create a new PMTiles writer with specified compression
    ///
    /// Both tile data and internal data (directories, metadata) will use
    /// the same compression algorithm. Deduplication is disabled by default.
    pub fn with_compression(compression: Compression) -> Self {
        Self {
            tiles: BTreeMap::new(),
            min_zoom: 255,
            max_zoom: 0,
            bounds: TileBounds::empty(),
            layer_name: "layer".to_string(),
            fields: HashMap::new(),
            total_features: 0,
            features_per_zoom: HashMap::new(),
            tile_compression: compression,
            internal_compression: compression,
            dedup_enabled: false,
            dedup_cache: DeduplicationCache::new(),
        }
    }

    /// Enable or disable tile deduplication
    pub fn enable_deduplication(&mut self, enabled: bool) {
        self.dedup_enabled = enabled;
    }

    /// Set the compression algorithm for tile data
    pub fn set_tile_compression(&mut self, compression: Compression) {
        self.tile_compression = compression;
    }

    /// Set the compression algorithm for internal data (directories, metadata)
    pub fn set_internal_compression(&mut self, compression: Compression) {
        self.internal_compression = compression;
    }

    /// Get the current tile compression setting
    pub fn tile_compression(&self) -> Compression {
        self.tile_compression
    }

    /// Get the current internal compression setting
    pub fn internal_compression(&self) -> Compression {
        self.internal_compression
    }

    /// Check if deduplication is enabled
    pub fn is_dedup_enabled(&self) -> bool {
        self.dedup_enabled
    }

    /// Get current deduplication statistics
    pub fn dedup_stats(&self) -> &DeduplicationStats {
        self.dedup_cache.stats()
    }

    /// Set the layer name for vector_layers metadata
    pub fn set_layer_name(&mut self, name: &str) {
        self.layer_name = name.to_string();
    }

    /// Set field metadata for vector_layers.fields
    ///
    /// Field types should be MVT-style: "String", "Number", or "Boolean"
    pub fn set_fields(&mut self, fields: HashMap<String, String>) {
        self.fields = fields;
    }

    /// Build the fields JSON object string
    fn build_fields_json(&self) -> String {
        if self.fields.is_empty() {
            return "{}".to_string();
        }

        // Sort field names for deterministic output
        let mut field_pairs: Vec<_> = self.fields.iter().collect();
        field_pairs.sort_by_key(|(k, _)| *k);

        let field_strings: Vec<String> = field_pairs
            .iter()
            .map(|(name, type_str)| format!(r#""{}":"{}""#, name, type_str))
            .collect();

        format!("{{{}}}", field_strings.join(","))
    }

    /// Build the tilestats JSON fragment
    fn build_tilestats_json(&self) -> String {
        if self.total_features == 0 {
            return String::new();
        }

        format!(
            r#""tilestats":{{"layerCount":1,"layers":[{{"layer":"{}","count":{},"attributeCount":{}}}]}},"#,
            self.layer_name,
            self.total_features,
            self.fields.len()
        )
    }

    /// Add a tile (will be gzip compressed)
    ///
    /// The tile data should be uncompressed MVT bytes.
    /// Use `add_tile_with_count` if you have feature count available.
    pub fn add_tile(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> std::io::Result<()> {
        self.add_tile_with_count(z, x, y, data, 0)
    }

    /// Add a tile with feature count for tilestats
    ///
    /// The tile data should be uncompressed MVT bytes.
    ///
    /// If deduplication is enabled, identical tiles will be stored once
    /// and referenced via PMTiles' `run_length` feature.
    pub fn add_tile_with_count(
        &mut self,
        z: u8,
        x: u32,
        y: u32,
        data: &[u8],
        feature_count: usize,
    ) -> std::io::Result<()> {
        let id = tile_id(z, x, y);
        let uncompressed_size = data.len() as u32;

        // Track zoom range
        self.min_zoom = self.min_zoom.min(z);
        self.max_zoom = self.max_zoom.max(z);

        // Track feature counts for tilestats
        self.total_features += feature_count as u64;
        *self.features_per_zoom.entry(z).or_insert(0) += feature_count as u64;

        if self.dedup_enabled {
            // Hash uncompressed data for deduplication
            let hash = TileHasher::hash(data);

            if self.dedup_cache.check(hash).is_some() {
                // Duplicate tile - store reference only (no data)
                self.dedup_cache.record_duplicate(uncompressed_size);
                self.tiles.insert(
                    id,
                    TileEntry {
                        data: None, // No data stored for duplicates
                        hash,
                        uncompressed_size,
                    },
                );
            } else {
                // New unique tile - compress using configured algorithm and store
                let compressed = compression::compress(data, self.tile_compression)?;
                let compressed_len = compressed.len() as u32;

                // Record in cache (offset will be calculated at write time)
                self.dedup_cache
                    .record_new(hash, 0, compressed_len, uncompressed_size);

                self.tiles.insert(
                    id,
                    TileEntry {
                        data: Some(compressed),
                        hash,
                        uncompressed_size,
                    },
                );
            }
        } else {
            // No deduplication - store every tile
            let compressed = compression::compress(data, self.tile_compression)?;
            let hash = TileHasher::hash(data);
            self.tiles.insert(
                id,
                TileEntry {
                    data: Some(compressed),
                    hash,
                    uncompressed_size,
                },
            );
        }

        Ok(())
    }

    /// Add a pre-compressed tile
    ///
    /// Use this if the tile data is already gzip compressed.
    /// Note: Deduplication is not available for pre-compressed tiles
    /// since we cannot hash the original content.
    pub fn add_tile_compressed(
        &mut self,
        z: u8,
        x: u32,
        y: u32,
        compressed_data: Vec<u8>,
    ) -> std::io::Result<()> {
        let id = tile_id(z, x, y);
        // For pre-compressed tiles, use a unique hash based on the compressed data
        // This won't deduplicate as effectively but preserves the API
        let hash = TileHasher::hash(&compressed_data);
        self.tiles.insert(
            id,
            TileEntry {
                data: Some(compressed_data),
                hash,
                uncompressed_size: 0, // Unknown
            },
        );

        self.min_zoom = self.min_zoom.min(z);
        self.max_zoom = self.max_zoom.max(z);

        Ok(())
    }

    /// Set geographic bounds for the tileset
    ///
    /// Latitude values are clamped to Web Mercator bounds (±85.05°).
    pub fn set_bounds(&mut self, bounds: &TileBounds) {
        self.bounds = TileBounds::new(
            bounds.lng_min,
            bounds.lat_min.clamp(-85.05, 85.05),
            bounds.lng_max,
            bounds.lat_max.clamp(-85.05, 85.05),
        );
    }

    /// Get the number of tiles added
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// Write the PMTiles archive to a file
    ///
    /// Layout: [Header (127)] [Root Directory] [Metadata] [Tile Data]
    ///
    /// When deduplication is enabled, identical tiles share storage and
    /// consecutive identical tiles use run_length encoding in the directory.
    pub fn write_to_file(&self, path: &Path) -> Result<()> {
        let file = File::create(path)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to create file: {}", e)))?;
        let mut writer = BufWriter::new(file);

        // Build tile data buffer and directory entries with deduplication
        let mut tile_data_buf = Vec::new();
        let mut entries = Vec::new();

        // Map hash -> (offset, length) for deduplication
        let mut hash_to_offset: HashMap<u64, (u64, u32)> = HashMap::new();
        let mut unique_contents = 0u64;

        if self.dedup_enabled {
            // With deduplication: store unique tiles, reference duplicates
            for (&id, entry) in &self.tiles {
                let (offset, length) = if let Some(ref data) = entry.data {
                    // Unique tile - write to buffer and record location
                    let offset = tile_data_buf.len() as u64;
                    let length = data.len() as u32;
                    tile_data_buf.extend_from_slice(data);
                    hash_to_offset.insert(entry.hash, (offset, length));
                    unique_contents += 1;
                    (offset, length)
                } else {
                    // Duplicate tile - look up existing location
                    *hash_to_offset.get(&entry.hash).expect("Hash must exist")
                };

                // Check if this can extend the previous entry's run_length
                // (same offset = same content, consecutive tile_id)
                if let Some(last) = entries.last_mut() {
                    let last_entry: &mut DirEntry = last;
                    if last_entry.offset == offset
                        && id == last_entry.tile_id + last_entry.run_length as u64
                    {
                        // Extend run_length instead of adding new entry
                        last_entry.run_length += 1;
                        continue;
                    }
                }

                entries.push(DirEntry {
                    tile_id: id,
                    offset,
                    length,
                    run_length: 1,
                });
            }
        } else {
            // Without deduplication: store every tile
            for (&id, entry) in &self.tiles {
                let data = entry.data.as_ref().expect("Non-dedup tiles must have data");
                entries.push(DirEntry {
                    tile_id: id,
                    offset: tile_data_buf.len() as u64,
                    length: data.len() as u32,
                    run_length: 1,
                });
                tile_data_buf.extend_from_slice(data);
                unique_contents += 1;
            }
        }

        // Encode and compress directory using configured internal compression
        let dir_bytes = encode_directory(&entries);
        let compressed_dir = compression::compress(&dir_bytes, self.internal_compression)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to compress directory: {}", e)))?;

        // JSON metadata with vector_layers and tilestats
        let min_z = if self.min_zoom == 255 {
            0
        } else {
            self.min_zoom
        };
        let max_z = if self.max_zoom == 0 && self.tiles.is_empty() {
            0
        } else {
            self.max_zoom
        };
        let fields_json = self.build_fields_json();
        let tilestats_json = self.build_tilestats_json();
        let metadata = format!(
            r#"{{"vector_layers":[{{"id":"{}","minzoom":{},"maxzoom":{},"fields":{}}}],{}"format":"pbf","generator":"gpq-tiles"}}"#,
            self.layer_name, min_z, max_z, fields_json, tilestats_json
        );
        let compressed_metadata =
            compression::compress(metadata.as_bytes(), self.internal_compression)
                .map_err(|e| Error::PMTilesWrite(format!("Failed to compress metadata: {}", e)))?;

        // Calculate section offsets
        let root_dir_offset = 127u64;
        let root_dir_length = compressed_dir.len() as u64;
        let metadata_offset = root_dir_offset + root_dir_length;
        let metadata_length = compressed_metadata.len() as u64;
        let tile_data_offset = metadata_offset + metadata_length;
        let tile_data_length = tile_data_buf.len() as u64;

        // Build header
        let header = Header {
            root_dir_offset,
            root_dir_length,
            json_metadata_offset: metadata_offset,
            json_metadata_length: metadata_length,
            leaf_dirs_offset: 0, // No leaf directories (simple archive)
            leaf_dirs_length: 0,
            tile_data_offset,
            tile_data_length,
            addressed_tiles_count: self.tiles.len() as u64,
            tile_entries_count: entries.len() as u64,
            tile_contents_count: unique_contents,
            clustered: true,
            internal_compression: self.internal_compression,
            tile_compression: self.tile_compression,
            tile_type: TileType::Mvt,
            min_zoom: if self.min_zoom == 255 {
                0
            } else {
                self.min_zoom
            },
            max_zoom: if self.max_zoom == 0 && self.tiles.is_empty() {
                0
            } else {
                self.max_zoom
            },
            min_lon: self.bounds.lng_min,
            min_lat: self.bounds.lat_min,
            max_lon: self.bounds.lng_max,
            max_lat: self.bounds.lat_max,
            center_zoom: if self.tiles.is_empty() {
                0
            } else {
                (self.min_zoom + self.max_zoom) / 2
            },
            center_lon: (self.bounds.lng_min + self.bounds.lng_max) / 2.0,
            center_lat: (self.bounds.lat_min + self.bounds.lat_max) / 2.0,
        };

        // Write all sections
        writer
            .write_all(&header.to_bytes())
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write header: {}", e)))?;
        writer
            .write_all(&compressed_dir)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write directory: {}", e)))?;
        writer
            .write_all(&compressed_metadata)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write metadata: {}", e)))?;
        writer
            .write_all(&tile_data_buf)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write tile data: {}", e)))?;

        writer
            .flush()
            .map_err(|e| Error::PMTilesWrite(format!("Failed to flush: {}", e)))?;

        Ok(())
    }
}

impl Default for PmtilesWriter {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// StreamingPmtilesWriter - Writes tile data to temp file immediately
// ============================================================================

use std::path::PathBuf;

/// Directory entry for streaming writer (minimal memory footprint).
/// Only stores what's needed for final directory encoding.
#[derive(Debug, Clone)]
struct StreamingDirEntry {
    tile_id: u64,
    offset: u64,
    length: u32,
}

/// Statistics about streaming write operations.
#[derive(Debug, Clone, Default)]
pub struct StreamingWriteStats {
    /// Total tiles added (including duplicates)
    pub total_tiles: u64,
    /// Unique tiles written to disk
    pub unique_tiles: u64,
    /// Bytes written to temp file
    pub bytes_written: u64,
    /// Bytes saved by deduplication
    pub bytes_saved_dedup: u64,
}

impl StreamingWriteStats {
    /// Calculate memory used by directory entries (approximate).
    /// Each StreamingDirEntry is ~24 bytes (tile_id: 8, offset: 8, length: 4 + padding).
    /// We estimate based on total_tiles since each tile gets a directory entry.
    pub fn estimated_memory_bytes(&self) -> u64 {
        // Each entry: tile_id (8) + offset (8) + length (4) = 20 bytes + padding ≈ 24 bytes
        // Plus HashMap entry overhead for dedup cache: ~40 bytes per unique
        // Plus Vec overhead: ~8 bytes
        self.total_tiles * 24 + self.unique_tiles * 40
    }
}

/// PMTiles writer that streams tile data to disk immediately.
///
/// Unlike `PmtilesWriter` which accumulates all tiles in memory,
/// `StreamingPmtilesWriter` writes compressed tile data to a temp file
/// as tiles are added. Only the small directory entries (~32 bytes each)
/// are kept in memory.
///
/// # Memory Usage
///
/// For 30,000 tiles:
/// - `PmtilesWriter`: ~1.2 GB (all tile data in memory)
/// - `StreamingPmtilesWriter`: ~2-3 MB (only directory entries)
///
/// # Example
///
/// ```no_run
/// use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
/// use gpq_tiles_core::compression::Compression;
/// use std::path::Path;
///
/// let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
/// writer.add_tile(0, 0, 0, &[0x1a, 0x00]).unwrap();
/// writer.add_tile(1, 0, 0, &[0x1a, 0x01]).unwrap();
/// writer.finalize(Path::new("output.pmtiles")).unwrap();
/// ```
pub struct StreamingPmtilesWriter {
    /// Buffered writer for temp file (tile data written immediately)
    temp_file: Option<BufWriter<File>>,
    /// Path to temp file (for cleanup and final assembly)
    temp_path: PathBuf,
    /// Directory entries (minimal memory: ~32 bytes each)
    entries: Vec<StreamingDirEntry>,
    /// Deduplication: hash → (offset, length) for detecting duplicates
    dedup_cache: HashMap<u64, (u64, u32)>,
    /// Current write offset in temp file
    current_offset: u64,
    /// Min zoom level seen
    min_zoom: u8,
    /// Max zoom level seen
    max_zoom: u8,
    /// Geographic bounds
    bounds: TileBounds,
    /// Layer name for metadata
    layer_name: String,
    /// Field metadata
    fields: HashMap<String, String>,
    /// Compression for tile data
    tile_compression: Compression,
    /// Compression for internal data (directories, metadata)
    internal_compression: Compression,
    /// Statistics
    stats: StreamingWriteStats,
    /// Total feature count
    total_features: u64,
    /// Whether finalize has been called (prevents double cleanup)
    finalized: bool,
}

impl StreamingPmtilesWriter {
    /// Create a new streaming writer with the specified compression.
    ///
    /// Creates a temp file in the system temp directory for tile data.
    pub fn new(compression: Compression) -> std::io::Result<Self> {
        Self::with_temp_dir(compression, std::env::temp_dir())
    }

    /// Create a new streaming writer with a custom temp directory.
    pub fn with_temp_dir(compression: Compression, temp_dir: PathBuf) -> std::io::Result<Self> {
        use std::time::{SystemTime, UNIX_EPOCH};

        // Generate unique temp file name with timestamp + process/thread IDs for parallel safety
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let tid = std::thread::current().id();
        let temp_path = temp_dir.join(format!("gpq-tiles-{}-{}-{:?}.tmp", timestamp, pid, tid));

        let file = File::create(&temp_path)?;
        let temp_file = BufWriter::with_capacity(64 * 1024, file); // 64KB buffer

        Ok(Self {
            temp_file: Some(temp_file),
            temp_path,
            entries: Vec::new(),
            dedup_cache: HashMap::new(),
            current_offset: 0,
            min_zoom: 255,
            max_zoom: 0,
            bounds: TileBounds::empty(),
            layer_name: "layer".to_string(),
            fields: HashMap::new(),
            tile_compression: compression,
            internal_compression: compression,
            stats: StreamingWriteStats::default(),
            total_features: 0,
            finalized: false,
        })
    }

    /// Get the path to the temp file (for testing).
    pub fn temp_path(&self) -> &Path {
        &self.temp_path
    }

    /// Set the layer name for metadata.
    pub fn set_layer_name(&mut self, name: &str) {
        self.layer_name = name.to_string();
    }

    /// Set field metadata.
    pub fn set_fields(&mut self, fields: HashMap<String, String>) {
        self.fields = fields;
    }

    /// Set geographic bounds.
    ///
    /// Latitude values are clamped to Web Mercator bounds (±85.05°).
    pub fn set_bounds(&mut self, bounds: &TileBounds) {
        self.bounds = TileBounds::new(
            bounds.lng_min,
            bounds.lat_min.clamp(-85.05, 85.05),
            bounds.lng_max,
            bounds.lat_max.clamp(-85.05, 85.05),
        );
    }

    /// Get current statistics.
    pub fn stats(&self) -> &StreamingWriteStats {
        &self.stats
    }

    /// Add a tile (writes immediately to temp file if unique).
    ///
    /// Tiles are compressed and written immediately. Duplicate tiles
    /// (same content) are detected and not written again.
    pub fn add_tile(&mut self, z: u8, x: u32, y: u32, data: &[u8]) -> std::io::Result<()> {
        self.add_tile_with_count(z, x, y, data, 0)
    }

    /// Add a tile with feature count.
    pub fn add_tile_with_count(
        &mut self,
        z: u8,
        x: u32,
        y: u32,
        data: &[u8],
        feature_count: usize,
    ) -> std::io::Result<()> {
        let temp_file = self
            .temp_file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("Writer already finalized"))?;

        let id = tile_id(z, x, y);
        self.stats.total_tiles += 1;
        self.total_features += feature_count as u64;

        // Track zoom range
        self.min_zoom = self.min_zoom.min(z);
        self.max_zoom = self.max_zoom.max(z);

        // Hash uncompressed data for deduplication
        let hash = crate::dedup::TileHasher::hash(data);

        // Check for duplicate
        if let Some((offset, length)) = self.dedup_cache.get(&hash) {
            // Duplicate - just add directory entry pointing to existing data
            self.entries.push(StreamingDirEntry {
                tile_id: id,
                offset: *offset,
                length: *length,
            });
            self.stats.bytes_saved_dedup += data.len() as u64;
            return Ok(());
        }

        // New unique tile - compress and write to temp file
        let compressed = compression::compress(data, self.tile_compression)?;
        let compressed_len = compressed.len() as u32;

        temp_file.write_all(&compressed)?;

        // Record in dedup cache and directory
        let offset = self.current_offset;
        self.dedup_cache.insert(hash, (offset, compressed_len));
        self.entries.push(StreamingDirEntry {
            tile_id: id,
            offset,
            length: compressed_len,
        });

        self.current_offset += compressed_len as u64;
        self.stats.unique_tiles += 1;
        self.stats.bytes_written += compressed_len as u64;

        Ok(())
    }

    /// Finalize the PMTiles file.
    ///
    /// Reads tile data from temp file and assembles the final PMTiles archive
    /// with header, directory, metadata, and tile data sections.
    ///
    /// The temp file is deleted after successful finalization.
    pub fn finalize(mut self, output_path: &Path) -> Result<StreamingWriteStats> {
        // Take the temp file handle to close it properly
        let temp_file = self
            .temp_file
            .take()
            .ok_or_else(|| Error::PMTilesWrite("Writer already finalized".to_string()))?;

        // Flush and close temp file
        let inner = temp_file
            .into_inner()
            .map_err(|e| Error::PMTilesWrite(format!("Failed to flush temp file: {}", e)))?;
        drop(inner); // Explicitly close

        // Sort entries by tile_id for clustered mode
        self.entries.sort_by_key(|e| e.tile_id);

        // Build run-length encoded directory entries
        let dir_entries = self.build_directory_entries();

        // Build directory structure with leaf directories if needed (Issue #88)
        // This ensures root directory fits in the initial 16KB HTTP range request
        let dir_layout = make_root_leaves(&dir_entries, self.internal_compression)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to build directory: {}", e)))?;

        // Build metadata JSON
        let metadata = self.build_metadata_json();
        let compressed_metadata =
            compression::compress(metadata.as_bytes(), self.internal_compression)
                .map_err(|e| Error::PMTilesWrite(format!("Failed to compress metadata: {}", e)))?;

        // Calculate section offsets
        // Layout: Header | Root Dir | Metadata | Leaf Dirs | Tile Data
        let root_dir_offset = 127u64;
        let root_dir_length = dir_layout.root_bytes.len() as u64;
        let metadata_offset = root_dir_offset + root_dir_length;
        let metadata_length = compressed_metadata.len() as u64;
        let leaf_dirs_offset = metadata_offset + metadata_length;
        let leaf_dirs_length = dir_layout.leaves_bytes.len() as u64;
        let tile_data_offset = leaf_dirs_offset + leaf_dirs_length;
        let tile_data_length = self.current_offset;

        // Build header
        let header = Header {
            root_dir_offset,
            root_dir_length,
            json_metadata_offset: metadata_offset,
            json_metadata_length: metadata_length,
            leaf_dirs_offset: if leaf_dirs_length > 0 {
                leaf_dirs_offset
            } else {
                0
            },
            leaf_dirs_length,
            tile_data_offset,
            tile_data_length,
            addressed_tiles_count: self.stats.total_tiles,
            tile_entries_count: dir_entries.len() as u64,
            tile_contents_count: self.stats.unique_tiles,
            clustered: true,
            internal_compression: self.internal_compression,
            tile_compression: self.tile_compression,
            tile_type: TileType::Mvt,
            min_zoom: if self.min_zoom == 255 {
                0
            } else {
                self.min_zoom
            },
            max_zoom: if self.max_zoom == 0 && self.entries.is_empty() {
                0
            } else {
                self.max_zoom
            },
            min_lon: self.bounds.lng_min,
            min_lat: self.bounds.lat_min,
            max_lon: self.bounds.lng_max,
            max_lat: self.bounds.lat_max,
            center_zoom: if self.entries.is_empty() {
                0
            } else {
                (self.min_zoom + self.max_zoom) / 2
            },
            center_lon: (self.bounds.lng_min + self.bounds.lng_max) / 2.0,
            center_lat: (self.bounds.lat_min + self.bounds.lat_max) / 2.0,
        };

        // Create output file and write all sections
        let output_file = File::create(output_path)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to create output file: {}", e)))?;
        let mut writer = BufWriter::new(output_file);

        // Write header
        writer
            .write_all(&header.to_bytes())
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write header: {}", e)))?;

        // Write root directory
        writer
            .write_all(&dir_layout.root_bytes)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write root directory: {}", e)))?;

        // Write metadata
        writer
            .write_all(&compressed_metadata)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write metadata: {}", e)))?;

        // Write leaf directories (if any)
        if !dir_layout.leaves_bytes.is_empty() {
            writer.write_all(&dir_layout.leaves_bytes).map_err(|e| {
                Error::PMTilesWrite(format!("Failed to write leaf directories: {}", e))
            })?;
        }

        // Copy tile data from temp file
        let mut temp_reader = File::open(&self.temp_path)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to reopen temp file: {}", e)))?;
        std::io::copy(&mut temp_reader, &mut writer)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to copy tile data: {}", e)))?;

        writer
            .flush()
            .map_err(|e| Error::PMTilesWrite(format!("Failed to flush output: {}", e)))?;

        // Clean up temp file
        let _ = std::fs::remove_file(&self.temp_path);

        // Mark as finalized so Drop doesn't try to clean up again
        self.finalized = true;

        Ok(self.stats.clone())
    }

    /// Build directory entries with run-length encoding for consecutive identical tiles.
    fn build_directory_entries(&self) -> Vec<DirEntry> {
        let mut dir_entries = Vec::new();

        for entry in &self.entries {
            // Check if this extends the previous entry's run
            if let Some(last) = dir_entries.last_mut() {
                let last_entry: &mut DirEntry = last;
                if last_entry.offset == entry.offset
                    && entry.tile_id == last_entry.tile_id + last_entry.run_length as u64
                {
                    last_entry.run_length += 1;
                    continue;
                }
            }

            dir_entries.push(DirEntry {
                tile_id: entry.tile_id,
                offset: entry.offset,
                length: entry.length,
                run_length: 1,
            });
        }

        dir_entries
    }

    /// Build metadata JSON string.
    fn build_metadata_json(&self) -> String {
        let min_z = if self.min_zoom == 255 {
            0
        } else {
            self.min_zoom
        };
        let max_z = if self.max_zoom == 0 && self.entries.is_empty() {
            0
        } else {
            self.max_zoom
        };

        let fields_json = self.build_fields_json();
        let tilestats_json = self.build_tilestats_json();

        format!(
            r#"{{"vector_layers":[{{"id":"{}","minzoom":{},"maxzoom":{},"fields":{}}}],{}"format":"pbf","generator":"gpq-tiles"}}"#,
            self.layer_name, min_z, max_z, fields_json, tilestats_json
        )
    }

    fn build_fields_json(&self) -> String {
        if self.fields.is_empty() {
            return "{}".to_string();
        }

        let mut field_pairs: Vec<_> = self.fields.iter().collect();
        field_pairs.sort_by_key(|(k, _)| *k);

        let field_strings: Vec<String> = field_pairs
            .iter()
            .map(|(name, type_str)| format!(r#""{}":"{}""#, name, type_str))
            .collect();

        format!("{{{}}}", field_strings.join(","))
    }

    fn build_tilestats_json(&self) -> String {
        if self.total_features == 0 {
            return String::new();
        }

        format!(
            r#""tilestats":{{"layerCount":1,"layers":[{{"layer":"{}","count":{},"attributeCount":{}}}]}},"#,
            self.layer_name,
            self.total_features,
            self.fields.len()
        )
    }
}

impl Drop for StreamingPmtilesWriter {
    fn drop(&mut self) {
        // Clean up temp file if it still exists (e.g., if finalize wasn't called)
        if !self.finalized {
            let _ = std::fs::remove_file(&self.temp_path);
        }
    }
}

// ============================================================================
// Tests (TDD)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // -------------------------------------------------------------------------
    // Task 7: Header and Structures Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_header_size_is_127_bytes() {
        let header = Header::default();
        let bytes = header.to_bytes();
        assert_eq!(
            bytes.len(),
            127,
            "PMTiles v3 header must be exactly 127 bytes"
        );
    }

    #[test]
    fn test_header_magic_and_version() {
        let header = Header::default();
        let bytes = header.to_bytes();
        assert_eq!(&bytes[0..7], b"PMTiles", "Magic number must be 'PMTiles'");
        assert_eq!(bytes[7], 3, "Version must be 3");
    }

    #[test]
    fn test_header_default_offsets() {
        let header = Header::default();
        let bytes = header.to_bytes();

        // Root directory offset should be 127 (immediately after header)
        let root_offset = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        assert_eq!(root_offset, 127);
    }

    #[test]
    fn test_header_bounds_encoding() {
        let header = Header {
            min_lon: -122.4194, // San Francisco
            min_lat: 37.7749,
            max_lon: -122.3894,
            max_lat: 37.8049,
            ..Default::default()
        };

        let bytes = header.to_bytes();

        // Decode min_lon (bytes 102-105)
        let min_lon_encoded = i32::from_le_bytes(bytes[102..106].try_into().unwrap());
        let min_lon_decoded = min_lon_encoded as f64 / 10_000_000.0;
        assert!(
            (min_lon_decoded - header.min_lon).abs() < 0.0001,
            "Lon encoding should preserve precision to ~0.0001 degrees"
        );
    }

    #[test]
    fn test_tile_id_zoom_0() {
        // At zoom 0, there's only one tile (0,0,0) with ID 0
        assert_eq!(tile_id(0, 0, 0), 0);
    }

    #[test]
    fn test_tile_id_zoom_1_matches_spec() {
        // From PMTiles spec examples:
        // Z=1, X=0, Y=0 → TileID=1
        // Z=1, X=0, Y=1 → TileID=2
        // Z=1, X=1, Y=1 → TileID=3
        // Z=1, X=1, Y=0 → TileID=4
        assert_eq!(tile_id(1, 0, 0), 1);
        assert_eq!(tile_id(1, 0, 1), 2);
        assert_eq!(tile_id(1, 1, 1), 3);
        assert_eq!(tile_id(1, 1, 0), 4);
    }

    #[test]
    fn test_tile_id_zoom_2_base() {
        // Z=2, X=0, Y=0 → TileID=5 (base for zoom 2)
        assert_eq!(tile_id(2, 0, 0), 5);
    }

    #[test]
    fn test_tile_id_unique_at_each_zoom() {
        // All tiles at a given zoom should have unique IDs
        for z in 0..=4u8 {
            let mut ids = Vec::new();
            let n = 1u32 << z;
            for y in 0..n {
                for x in 0..n {
                    ids.push(tile_id(z, x, y));
                }
            }
            let original_len = ids.len();
            ids.sort();
            ids.dedup();
            assert_eq!(
                ids.len(),
                original_len,
                "All tile IDs at zoom {} should be unique",
                z
            );
        }
    }

    #[test]
    fn test_tile_id_increasing_with_zoom() {
        // Max ID at zoom z should be less than min ID at zoom z+1
        for z in 0..4u8 {
            let n = 1u32 << z;
            let max_id_at_z = (0..n)
                .flat_map(|y| (0..n).map(move |x| tile_id(z, x, y)))
                .max()
                .unwrap();

            let min_id_at_z_plus_1 = tile_id(z + 1, 0, 0);

            assert!(
                max_id_at_z < min_id_at_z_plus_1,
                "Max ID at zoom {} ({}) should be < min ID at zoom {} ({})",
                z,
                max_id_at_z,
                z + 1,
                min_id_at_z_plus_1
            );
        }
    }

    // -------------------------------------------------------------------------
    // Task 8: Directory Encoding Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_encode_varint_small_values() {
        // Values < 128 encode to single byte
        let mut buf = Vec::new();
        encode_varint(0, &mut buf);
        assert_eq!(buf, vec![0]);

        buf.clear();
        encode_varint(1, &mut buf);
        assert_eq!(buf, vec![1]);

        buf.clear();
        encode_varint(127, &mut buf);
        assert_eq!(buf, vec![127]);
    }

    #[test]
    fn test_encode_varint_128() {
        // 128 = 0x80 needs 2 bytes: [0x80, 0x01]
        let mut buf = Vec::new();
        encode_varint(128, &mut buf);
        assert_eq!(buf, vec![0x80, 0x01]);
    }

    #[test]
    fn test_encode_varint_300() {
        // 300 = 0x12C = 0b1_0010_1100
        // Low 7 bits: 0010_1100 = 0x2C, with continuation: 0xAC
        // High bits: 0000_0010 = 0x02
        let mut buf = Vec::new();
        encode_varint(300, &mut buf);
        assert_eq!(buf, vec![0xAC, 0x02]);
    }

    #[test]
    fn test_varint_roundtrip() {
        let test_values = [0u64, 1, 127, 128, 255, 256, 300, 16383, 16384, u64::MAX];

        for &value in &test_values {
            let mut buf = Vec::new();
            encode_varint(value, &mut buf);
            let (decoded, bytes_consumed) = decode_varint(&buf).expect("Should decode");
            assert_eq!(decoded, value, "Roundtrip failed for {}", value);
            assert_eq!(bytes_consumed, buf.len());
        }
    }

    #[test]
    fn test_encode_directory_empty() {
        let entries: Vec<DirEntry> = vec![];
        let encoded = encode_directory(&entries);
        // Should just be count = 0
        assert_eq!(encoded, vec![0]);
    }

    #[test]
    fn test_encode_directory_single_entry() {
        let entries = vec![DirEntry {
            tile_id: 1,
            offset: 0,
            length: 100,
            run_length: 1,
        }];
        let encoded = encode_directory(&entries);

        // Should start with count = 1
        assert!(!encoded.is_empty());
        assert_eq!(encoded[0], 1);
    }

    #[test]
    fn test_encode_directory_multiple_entries() {
        let entries = vec![
            DirEntry {
                tile_id: 5,
                offset: 0,
                length: 100,
                run_length: 1,
            },
            DirEntry {
                tile_id: 42,
                offset: 100,
                length: 200,
                run_length: 1,
            },
            DirEntry {
                tile_id: 69,
                offset: 300,
                length: 50,
                run_length: 1,
            },
        ];
        let encoded = encode_directory(&entries);

        // Should start with count = 3
        assert_eq!(encoded[0], 3);

        // The encoding should be smaller than naive (due to delta encoding)
        // Each entry would be ~24 bytes naive, but delta should compress
        assert!(encoded.len() < entries.len() * 24);
    }

    #[test]
    fn test_gzip_compress_roundtrip() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let original = b"Hello, PMTiles! This is test data.";
        let compressed = gzip_compress(original).expect("Should compress");

        // Should be shorter than original (for non-trivial data)
        // Note: very small inputs might expand

        // Decompress and verify
        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .expect("Should decompress");

        assert_eq!(decompressed, original);
    }

    // -------------------------------------------------------------------------
    // Task 9: Full Writer Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_writer_creation() {
        let writer = PmtilesWriter::new();
        assert_eq!(writer.tile_count(), 0);
    }

    #[test]
    fn test_writer_add_single_tile() {
        let mut writer = PmtilesWriter::new();
        let mvt_data = vec![0x1a, 0x00]; // Minimal MVT-like data

        writer.add_tile(0, 0, 0, &mvt_data).unwrap();
        assert_eq!(writer.tile_count(), 1);
    }

    #[test]
    fn test_writer_creates_valid_pmtiles_file() {
        let mut writer = PmtilesWriter::new();

        // Add a minimal tile
        let mvt_data = vec![0x1a, 0x00];
        writer.add_tile(0, 0, 0, &mvt_data).unwrap();
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        let path = Path::new("/tmp/test-pmtiles-writer.pmtiles");
        let _ = fs::remove_file(path);

        writer.write_to_file(path).expect("Should write file");

        // Verify file exists and has correct structure
        assert!(path.exists(), "File should exist");

        let data = fs::read(path).unwrap();

        // Check magic number and version
        assert_eq!(&data[0..7], b"PMTiles");
        assert_eq!(data[7], 3);

        // Check file is at least header size + some data
        assert!(data.len() > 127);

        // Check root directory offset points to position 127
        let root_offset = u64::from_le_bytes(data[8..16].try_into().unwrap());
        assert_eq!(root_offset, 127);

        // Clean up
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_writer_multiple_tiles_multiple_zooms() {
        let mut writer = PmtilesWriter::new();

        // Add tiles at zooms 0, 1, 2
        for z in 0..3u8 {
            let n = 1u32 << z;
            for x in 0..n {
                for y in 0..n {
                    let mvt_data = vec![0x1a, z, x as u8, y as u8];
                    writer.add_tile(z, x, y, &mvt_data).unwrap();
                }
            }
        }

        // Should have 1 + 4 + 16 = 21 tiles
        assert_eq!(writer.tile_count(), 21);

        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        let path = Path::new("/tmp/test-pmtiles-multi.pmtiles");
        let _ = fs::remove_file(path);

        writer.write_to_file(path).expect("Should write file");

        // Verify basic structure
        let data = fs::read(path).unwrap();
        assert_eq!(&data[0..7], b"PMTiles");
        assert_eq!(data[7], 3);

        // Check tile counts in header
        let addressed_count = u64::from_le_bytes(data[72..80].try_into().unwrap());
        assert_eq!(addressed_count, 21);

        // Check zoom range
        assert_eq!(data[100], 0); // min_zoom
        assert_eq!(data[101], 2); // max_zoom

        // Clean up
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_writer_empty_tileset() {
        let writer = PmtilesWriter::new();

        let path = Path::new("/tmp/test-pmtiles-empty.pmtiles");
        let _ = fs::remove_file(path);

        writer.write_to_file(path).expect("Should write empty file");

        let data = fs::read(path).unwrap();
        assert_eq!(&data[0..7], b"PMTiles");

        // Clean up
        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_writer_tile_ordering() {
        let mut writer = PmtilesWriter::new();

        // Add tiles in random order
        writer.add_tile(2, 3, 3, &[1, 2, 3]).unwrap();
        writer.add_tile(0, 0, 0, &[4, 5, 6]).unwrap();
        writer.add_tile(1, 1, 0, &[7, 8, 9]).unwrap();

        // BTreeMap should maintain Hilbert curve order
        assert_eq!(writer.tile_count(), 3);

        let path = Path::new("/tmp/test-pmtiles-ordering.pmtiles");
        let _ = fs::remove_file(path);

        writer.write_to_file(path).expect("Should write file");

        // Should succeed (clustered mode requires sorted tiles)
        assert!(path.exists());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_writer_bounds_preserved() {
        let mut writer = PmtilesWriter::new();
        writer.add_tile(0, 0, 0, &[1, 2, 3]).unwrap();

        let bounds = TileBounds::new(-122.5, 37.7, -122.3, 37.9);
        writer.set_bounds(&bounds);

        let path = Path::new("/tmp/test-pmtiles-bounds.pmtiles");
        let _ = fs::remove_file(path);

        writer.write_to_file(path).expect("Should write file");

        let data = fs::read(path).unwrap();

        // Decode bounds from header
        let decode_coord = |offset: usize| -> f64 {
            let val = i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            val as f64 / 10_000_000.0
        };

        let min_lon = decode_coord(102);
        let min_lat = decode_coord(106);
        let max_lon = decode_coord(110);
        let max_lat = decode_coord(114);

        assert!((min_lon - bounds.lng_min).abs() < 0.0001);
        assert!((min_lat - bounds.lat_min).abs() < 0.0001);
        assert!((max_lon - bounds.lng_max).abs() < 0.0001);
        assert!((max_lat - bounds.lat_max).abs() < 0.0001);

        let _ = fs::remove_file(path);
    }

    // -------------------------------------------------------------------------
    // Field Metadata Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_build_fields_json_empty() {
        let writer = PmtilesWriter::new();
        assert_eq!(writer.build_fields_json(), "{}");
    }

    #[test]
    fn test_build_fields_json_with_fields() {
        let mut writer = PmtilesWriter::new();
        let mut fields = HashMap::new();
        fields.insert("name".to_string(), "String".to_string());
        fields.insert("area".to_string(), "Number".to_string());
        writer.set_fields(fields);

        let json = writer.build_fields_json();
        // Fields are sorted alphabetically
        assert_eq!(json, r#"{"area":"Number","name":"String"}"#);
    }

    #[test]
    fn test_writer_field_metadata_in_output() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let mut writer = PmtilesWriter::new();
        writer.add_tile(0, 0, 0, &[1, 2, 3]).unwrap();
        writer.set_layer_name("buildings");

        let mut fields = HashMap::new();
        fields.insert("name".to_string(), "String".to_string());
        fields.insert("height".to_string(), "Number".to_string());
        writer.set_fields(fields);

        let path = Path::new("/tmp/test-pmtiles-fields.pmtiles");
        let _ = fs::remove_file(path);

        writer.write_to_file(path).expect("Should write file");

        let data = fs::read(path).unwrap();

        // Extract metadata offset and length from header
        let metadata_offset = u64::from_le_bytes(data[24..32].try_into().unwrap()) as usize;
        let metadata_length = u64::from_le_bytes(data[32..40].try_into().unwrap()) as usize;

        // Decompress the metadata
        let compressed_metadata = &data[metadata_offset..metadata_offset + metadata_length];
        let mut decoder = GzDecoder::new(compressed_metadata);
        let mut metadata_json = String::new();
        decoder
            .read_to_string(&mut metadata_json)
            .expect("Should decompress metadata");

        // Verify fields are present
        assert!(metadata_json.contains(r#""height":"Number""#));
        assert!(metadata_json.contains(r#""name":"String""#));
        assert!(metadata_json.contains(r#""id":"buildings""#));

        let _ = fs::remove_file(path);
    }

    // -------------------------------------------------------------------------
    // Compression Configuration Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_writer_with_compression_constructor() {
        let writer = PmtilesWriter::with_compression(Compression::Brotli);
        assert_eq!(writer.tile_compression(), Compression::Brotli);
        assert_eq!(writer.internal_compression(), Compression::Brotli);
    }

    #[test]
    fn test_writer_set_compression() {
        let mut writer = PmtilesWriter::new();
        assert_eq!(writer.tile_compression(), Compression::Gzip); // default

        writer.set_tile_compression(Compression::Zstd);
        assert_eq!(writer.tile_compression(), Compression::Zstd);

        writer.set_internal_compression(Compression::Brotli);
        assert_eq!(writer.internal_compression(), Compression::Brotli);
    }

    #[test]
    fn test_writer_brotli_compression() {
        let mut writer = PmtilesWriter::with_compression(Compression::Brotli);
        let mvt_data = vec![0x1a; 100]; // Compressible data

        writer.add_tile(0, 0, 0, &mvt_data).unwrap();
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        let path = Path::new("/tmp/test-pmtiles-brotli.pmtiles");
        let _ = fs::remove_file(path);

        writer
            .write_to_file(path)
            .expect("Should write file with brotli");

        let data = fs::read(path).unwrap();

        // Verify header
        assert_eq!(&data[0..7], b"PMTiles");
        assert_eq!(data[7], 3);

        // Check compression bytes in header (97 = internal, 98 = tile)
        assert_eq!(data[97], Compression::Brotli as u8);
        assert_eq!(data[98], Compression::Brotli as u8);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_writer_zstd_compression() {
        let mut writer = PmtilesWriter::with_compression(Compression::Zstd);
        let mvt_data = vec![0x1a; 100];

        writer.add_tile(0, 0, 0, &mvt_data).unwrap();
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        let path = Path::new("/tmp/test-pmtiles-zstd.pmtiles");
        let _ = fs::remove_file(path);

        writer
            .write_to_file(path)
            .expect("Should write file with zstd");

        let data = fs::read(path).unwrap();

        // Check compression bytes in header
        assert_eq!(data[97], Compression::Zstd as u8);
        assert_eq!(data[98], Compression::Zstd as u8);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_writer_no_compression() {
        let mut writer = PmtilesWriter::with_compression(Compression::None);
        let mvt_data = vec![0x1a, 0x00];

        writer.add_tile(0, 0, 0, &mvt_data).unwrap();
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        let path = Path::new("/tmp/test-pmtiles-none.pmtiles");
        let _ = fs::remove_file(path);

        writer
            .write_to_file(path)
            .expect("Should write file without compression");

        let data = fs::read(path).unwrap();

        // Check compression bytes in header
        assert_eq!(data[97], Compression::None as u8);
        assert_eq!(data[98], Compression::None as u8);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_writer_mixed_compression() {
        // Test different compression for internal vs tile data
        let mut writer = PmtilesWriter::new();
        writer.set_internal_compression(Compression::Gzip);
        writer.set_tile_compression(Compression::Zstd);

        let mvt_data = vec![0x1a; 100];
        writer.add_tile(0, 0, 0, &mvt_data).unwrap();
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        let path = Path::new("/tmp/test-pmtiles-mixed.pmtiles");
        let _ = fs::remove_file(path);

        writer
            .write_to_file(path)
            .expect("Should write file with mixed compression");

        let data = fs::read(path).unwrap();

        // Check compression bytes in header
        assert_eq!(data[97], Compression::Gzip as u8); // internal
        assert_eq!(data[98], Compression::Zstd as u8); // tile

        let _ = fs::remove_file(path);
    }

    // -------------------------------------------------------------------------
    // Tile Deduplication Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_writer_dedup_identical_tiles() {
        let mut writer = PmtilesWriter::new();
        writer.enable_deduplication(true);

        // Add 3 identical tiles at consecutive positions
        let ocean_tile = vec![0x1a, 0x00]; // Same content
        writer.add_tile(1, 0, 0, &ocean_tile).unwrap();
        writer.add_tile(1, 0, 1, &ocean_tile).unwrap();
        writer.add_tile(1, 1, 1, &ocean_tile).unwrap();

        // 3 tiles addressed, but only 1 unique content
        assert_eq!(writer.tile_count(), 3);

        let stats = writer.dedup_stats();
        assert_eq!(stats.total_tiles, 3);
        assert_eq!(stats.unique_tiles, 1);
        assert_eq!(stats.duplicates_eliminated, 2);
    }

    #[test]
    fn test_writer_dedup_mixed_tiles() {
        let mut writer = PmtilesWriter::new();
        writer.enable_deduplication(true);

        // Add tiles: A, A, B, A, B, B
        let tile_a = vec![0x1a, 0x01];
        let tile_b = vec![0x1a, 0x02];

        writer.add_tile(0, 0, 0, &tile_a).unwrap();
        writer.add_tile(1, 0, 0, &tile_a).unwrap(); // dup
        writer.add_tile(1, 0, 1, &tile_b).unwrap();
        writer.add_tile(1, 1, 1, &tile_a).unwrap(); // dup
        writer.add_tile(1, 1, 0, &tile_b).unwrap(); // dup
        writer.add_tile(2, 0, 0, &tile_b).unwrap(); // dup

        let stats = writer.dedup_stats();
        assert_eq!(stats.total_tiles, 6);
        assert_eq!(stats.unique_tiles, 2);
        assert_eq!(stats.duplicates_eliminated, 4);
    }

    #[test]
    fn test_writer_dedup_disabled_by_default() {
        let writer = PmtilesWriter::new();
        // Deduplication should be disabled by default for backward compatibility
        assert!(!writer.is_dedup_enabled());
    }

    #[test]
    fn test_writer_dedup_file_size_reduction() {
        // Test with deduplication
        let mut writer_dedup = PmtilesWriter::new();
        writer_dedup.enable_deduplication(true);

        let ocean_tile = vec![0x1a, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05];
        for z in 0..3u8 {
            let n = 1u32 << z;
            for x in 0..n {
                for y in 0..n {
                    writer_dedup.add_tile(z, x, y, &ocean_tile).unwrap();
                }
            }
        }

        let path_dedup = Path::new("/tmp/test-pmtiles-dedup-enabled.pmtiles");
        let _ = fs::remove_file(path_dedup);
        writer_dedup.write_to_file(path_dedup).unwrap();
        let size_dedup = fs::metadata(path_dedup).unwrap().len();

        // Test without deduplication
        let mut writer_no_dedup = PmtilesWriter::new();
        // Dedup disabled by default

        for z in 0..3u8 {
            let n = 1u32 << z;
            for x in 0..n {
                for y in 0..n {
                    writer_no_dedup.add_tile(z, x, y, &ocean_tile).unwrap();
                }
            }
        }

        let path_no_dedup = Path::new("/tmp/test-pmtiles-dedup-disabled.pmtiles");
        let _ = fs::remove_file(path_no_dedup);
        writer_no_dedup.write_to_file(path_no_dedup).unwrap();
        let size_no_dedup = fs::metadata(path_no_dedup).unwrap().len();

        // Deduplicated file should be smaller
        assert!(
            size_dedup < size_no_dedup,
            "Deduplicated file ({} bytes) should be smaller than non-deduplicated ({} bytes)",
            size_dedup,
            size_no_dedup
        );

        let _ = fs::remove_file(path_dedup);
        let _ = fs::remove_file(path_no_dedup);
    }

    #[test]
    fn test_writer_dedup_run_length_consecutive() {
        let mut writer = PmtilesWriter::new();
        writer.enable_deduplication(true);

        // Add consecutive tiles with same content (should use run_length)
        let tile = vec![0x1a, 0x10];
        // Zoom 1 tiles: IDs 1, 2, 3, 4 (consecutive in Hilbert order)
        writer.add_tile(1, 0, 0, &tile).unwrap(); // ID 1
        writer.add_tile(1, 0, 1, &tile).unwrap(); // ID 2
        writer.add_tile(1, 1, 1, &tile).unwrap(); // ID 3
        writer.add_tile(1, 1, 0, &tile).unwrap(); // ID 4

        let path = Path::new("/tmp/test-pmtiles-runlength.pmtiles");
        let _ = fs::remove_file(path);
        writer.write_to_file(path).unwrap();

        let data = fs::read(path).unwrap();

        // Verify header counts
        let addressed_count = u64::from_le_bytes(data[72..80].try_into().unwrap());
        let entries_count = u64::from_le_bytes(data[80..88].try_into().unwrap());
        let contents_count = u64::from_le_bytes(data[88..96].try_into().unwrap());

        // 4 tiles addressed
        assert_eq!(addressed_count, 4);
        // But only 1 directory entry (run_length = 4)
        assert_eq!(entries_count, 1);
        // And only 1 unique content
        assert_eq!(contents_count, 1);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn test_writer_dedup_header_stats() {
        let mut writer = PmtilesWriter::new();
        writer.enable_deduplication(true);

        // 10 tiles, 3 unique contents
        let tile_a = vec![0x1a, 0x01];
        let tile_b = vec![0x1a, 0x02];
        let tile_c = vec![0x1a, 0x03];

        // Pattern: A A A B B C A B C C
        for _ in 0..3 {
            writer.add_tile(0, 0, 0, &tile_a).unwrap(); // Will deduplicate
        }
        // Only first A is added at z=0, rest are different coords
        // Actually, let me use different coords properly:
        let mut writer2 = PmtilesWriter::new();
        writer2.enable_deduplication(true);

        // Add 10 tiles with 3 unique contents at zoom 0-2
        writer2.add_tile(0, 0, 0, &tile_a).unwrap();
        writer2.add_tile(1, 0, 0, &tile_a).unwrap(); // dup A
        writer2.add_tile(1, 0, 1, &tile_a).unwrap(); // dup A
        writer2.add_tile(1, 1, 1, &tile_b).unwrap();
        writer2.add_tile(1, 1, 0, &tile_b).unwrap(); // dup B
        writer2.add_tile(2, 0, 0, &tile_c).unwrap();
        writer2.add_tile(2, 0, 1, &tile_a).unwrap(); // dup A
        writer2.add_tile(2, 1, 0, &tile_b).unwrap(); // dup B
        writer2.add_tile(2, 1, 1, &tile_c).unwrap(); // dup C
        writer2.add_tile(2, 2, 0, &tile_c).unwrap(); // dup C

        let path = Path::new("/tmp/test-pmtiles-header-stats.pmtiles");
        let _ = fs::remove_file(path);
        writer2.write_to_file(path).unwrap();

        let data = fs::read(path).unwrap();

        let addressed_count = u64::from_le_bytes(data[72..80].try_into().unwrap());
        let contents_count = u64::from_le_bytes(data[88..96].try_into().unwrap());

        assert_eq!(addressed_count, 10, "Should address 10 tiles");
        assert_eq!(contents_count, 3, "Should have 3 unique contents");

        let _ = fs::remove_file(path);
    }

    // =========================================================================
    // StreamingPmtilesWriter Tests (TDD)
    // =========================================================================

    #[test]
    fn test_streaming_writer_creates_temp_file() {
        let writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        // Temp file should exist
        assert!(
            writer.temp_path().exists(),
            "Temp file should be created at {:?}",
            writer.temp_path()
        );

        // Clean up happens on drop
        let temp_path = writer.temp_path().to_path_buf();
        drop(writer);
        assert!(
            !temp_path.exists(),
            "Temp file should be cleaned up on drop"
        );
    }

    #[test]
    fn test_streaming_writer_add_tile_writes_to_temp() {
        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        // Add a tile
        let mvt_data = vec![0x1a, 0x00, 0x01, 0x02];
        writer.add_tile(0, 0, 0, &mvt_data).unwrap();

        // Stats should reflect the write
        let stats = writer.stats();
        assert_eq!(stats.total_tiles, 1);
        assert_eq!(stats.unique_tiles, 1);
        assert!(
            stats.bytes_written > 0,
            "Should have written bytes to temp file"
        );

        // Note: The BufWriter may not have flushed to disk yet, so we check our
        // internal stats rather than file metadata (which requires flush)
        assert_eq!(
            stats.bytes_written, writer.current_offset,
            "bytes_written should match current_offset"
        );
    }

    #[test]
    fn test_streaming_writer_dedup_same_content() {
        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        // Add 3 tiles with identical content
        let ocean_tile = vec![0x1a, 0x00];
        writer.add_tile(1, 0, 0, &ocean_tile).unwrap();
        writer.add_tile(1, 0, 1, &ocean_tile).unwrap();
        writer.add_tile(1, 1, 1, &ocean_tile).unwrap();

        let stats = writer.stats();
        assert_eq!(stats.total_tiles, 3, "Should track 3 total tiles");
        assert_eq!(stats.unique_tiles, 1, "Should only have 1 unique tile");
        assert!(
            stats.bytes_saved_dedup > 0,
            "Should have saved bytes via deduplication"
        );
    }

    #[test]
    fn test_streaming_writer_finalize_creates_valid_pmtiles() {
        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        // Add a few tiles
        writer.add_tile(0, 0, 0, &[0x1a, 0x00]).unwrap();
        writer.add_tile(1, 0, 0, &[0x1a, 0x01]).unwrap();
        writer.add_tile(1, 0, 1, &[0x1a, 0x02]).unwrap();
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        let output_path = Path::new("/tmp/test-streaming-pmtiles.pmtiles");
        let _ = fs::remove_file(output_path);

        let stats = writer.finalize(output_path).expect("Should finalize");

        // Verify file was created with valid PMTiles structure
        assert!(output_path.exists(), "Output file should exist");

        let data = fs::read(output_path).unwrap();

        // Check magic and version
        assert_eq!(&data[0..7], b"PMTiles", "Should have PMTiles magic");
        assert_eq!(data[7], 3, "Should be version 3");

        // Check tile counts in header
        let addressed_count = u64::from_le_bytes(data[72..80].try_into().unwrap());
        assert_eq!(addressed_count, 3, "Should have 3 addressed tiles");

        // Verify stats
        assert_eq!(stats.total_tiles, 3);
        assert_eq!(stats.unique_tiles, 3); // All different content

        // Clean up
        let _ = fs::remove_file(output_path);
    }

    #[test]
    fn test_streaming_writer_memory_bounded() {
        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        // Add many tiles (simulating a large file scenario)
        // Even with 1000 tiles, memory should stay low
        // Use valid coordinates for each zoom level
        let mut count = 0;
        for z in 0..10u8 {
            let max_coord = 1u32 << z; // Valid range: 0 to max_coord-1
            for x in 0..max_coord.min(10) {
                for y in 0..max_coord.min(10) {
                    let data = vec![0x1a, z, (x & 0xFF) as u8, (y & 0xFF) as u8, count as u8];
                    writer.add_tile(z, x, y, &data).unwrap();
                    count += 1;
                    if count >= 1000 {
                        break;
                    }
                }
                if count >= 1000 {
                    break;
                }
            }
            if count >= 1000 {
                break;
            }
        }

        let stats = writer.stats();

        // Memory estimate should be bounded: ~64 bytes per entry (24 dir + 40 dedup)
        // 1000 tiles × 64 bytes = ~64KB (not 40MB of tile data)
        let estimated_mem = stats.estimated_memory_bytes();
        assert!(
            estimated_mem < 200_000, // Less than 200KB
            "Memory usage should be bounded, got {} bytes",
            estimated_mem
        );

        // Clean up (finalize not needed for this test)
    }

    #[test]
    fn test_streaming_writer_matches_non_streaming_output() {
        // Create identical content with both writers and compare output
        let tiles_data = vec![
            (0, 0, 0, vec![0x1a, 0x00]),
            (1, 0, 0, vec![0x1a, 0x01]),
            (1, 0, 1, vec![0x1a, 0x02]),
            (1, 1, 1, vec![0x1a, 0x00]), // Duplicate content
        ];

        // Non-streaming writer (with dedup enabled for fair comparison)
        let mut non_streaming = PmtilesWriter::with_compression(Compression::Gzip);
        non_streaming.enable_deduplication(true);
        non_streaming.set_layer_name("test");
        non_streaming.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));
        for (z, x, y, data) in &tiles_data {
            non_streaming.add_tile(*z, *x, *y, data).unwrap();
        }

        let non_streaming_path = Path::new("/tmp/test-compare-non-streaming.pmtiles");
        let _ = fs::remove_file(non_streaming_path);
        non_streaming.write_to_file(non_streaming_path).unwrap();

        // Streaming writer
        let mut streaming = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        streaming.set_layer_name("test");
        streaming.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));
        for (z, x, y, data) in &tiles_data {
            streaming.add_tile(*z, *x, *y, data).unwrap();
        }

        let streaming_path = Path::new("/tmp/test-compare-streaming.pmtiles");
        let _ = fs::remove_file(streaming_path);
        streaming.finalize(streaming_path).unwrap();

        // Compare key header fields
        let ns_data = fs::read(non_streaming_path).unwrap();
        let s_data = fs::read(streaming_path).unwrap();

        // Magic and version should match
        assert_eq!(
            &ns_data[0..8],
            &s_data[0..8],
            "Header magic/version should match"
        );

        // Addressed tiles count should match
        let ns_addressed = u64::from_le_bytes(ns_data[72..80].try_into().unwrap());
        let s_addressed = u64::from_le_bytes(s_data[72..80].try_into().unwrap());
        assert_eq!(ns_addressed, s_addressed, "Addressed tiles should match");

        // Unique contents should match
        let ns_contents = u64::from_le_bytes(ns_data[88..96].try_into().unwrap());
        let s_contents = u64::from_le_bytes(s_data[88..96].try_into().unwrap());
        assert_eq!(ns_contents, s_contents, "Unique contents should match");

        // Clean up
        let _ = fs::remove_file(non_streaming_path);
        let _ = fs::remove_file(streaming_path);
    }

    #[test]
    fn test_streaming_writer_with_feature_count() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("buildings");

        // Add tiles with feature counts
        writer
            .add_tile_with_count(0, 0, 0, &[0x1a, 0x00], 100)
            .unwrap();
        writer
            .add_tile_with_count(1, 0, 0, &[0x1a, 0x01], 50)
            .unwrap();
        writer
            .add_tile_with_count(1, 0, 1, &[0x1a, 0x02], 75)
            .unwrap();

        let output_path = Path::new("/tmp/test-streaming-features.pmtiles");
        let _ = fs::remove_file(output_path);
        writer.finalize(output_path).unwrap();

        let data = fs::read(output_path).unwrap();

        // Extract metadata and check tilestats
        let metadata_offset = u64::from_le_bytes(data[24..32].try_into().unwrap()) as usize;
        let metadata_length = u64::from_le_bytes(data[32..40].try_into().unwrap()) as usize;
        let compressed_metadata = &data[metadata_offset..metadata_offset + metadata_length];

        let mut decoder = GzDecoder::new(compressed_metadata);
        let mut metadata_json = String::new();
        decoder.read_to_string(&mut metadata_json).unwrap();

        // Should have tilestats with total feature count
        assert!(
            metadata_json.contains("\"count\":225"),
            "Should have total feature count 225, got: {}",
            metadata_json
        );

        let _ = fs::remove_file(output_path);
    }

    // -------------------------------------------------------------------------
    // Leaf Directory Tests (Issue #88)
    // -------------------------------------------------------------------------

    /// PMTiles initial HTTP range request size (16KB)
    const INITIAL_FETCH_SIZE: usize = 16384;
    /// PMTiles header size
    const HEADER_SIZE: usize = 127;
    /// Maximum root directory size that fits in initial fetch
    const MAX_ROOT_DIR_SIZE: usize = INITIAL_FETCH_SIZE - HEADER_SIZE;

    #[test]
    fn test_large_archive_uses_leaf_directories() {
        // Create enough tiles to exceed 16KB root directory
        // gzip compresses directory entries very well (~2-5 bytes/entry compressed)
        // Need 10,000+ entries to reliably exceed the 16KB threshold
        let num_tiles = 10_000;

        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        // Add many tiles at zoom 12 (distributed across tile space)
        // Using higher zoom means larger tile_ids = less compressible
        for i in 0..num_tiles {
            let x = i % 4096;
            let y = i / 4096;
            let data = vec![0x1a, (i & 0xff) as u8, ((i >> 8) & 0xff) as u8];
            writer.add_tile(12, x as u32, y as u32, &data).unwrap();
        }

        let output_path = Path::new("/tmp/test-leaf-directories.pmtiles");
        let _ = fs::remove_file(output_path);
        writer.finalize(output_path).unwrap();

        // Read the header and verify leaf directories are used
        let data = fs::read(output_path).unwrap();

        // Extract header fields
        let root_dir_length = u64::from_le_bytes(data[16..24].try_into().unwrap()) as usize;
        let leaf_dirs_offset = u64::from_le_bytes(data[40..48].try_into().unwrap());
        let leaf_dirs_length = u64::from_le_bytes(data[48..56].try_into().unwrap());

        // Debug output
        eprintln!(
            "Archive stats: root_dir_length={}, leaf_dirs_offset={}, leaf_dirs_length={}, MAX={}",
            root_dir_length, leaf_dirs_offset, leaf_dirs_length, MAX_ROOT_DIR_SIZE
        );

        // Root directory must fit in initial fetch (16KB - 127 byte header)
        assert!(
            root_dir_length <= MAX_ROOT_DIR_SIZE,
            "Root directory ({} bytes) must fit in initial fetch ({} bytes)",
            root_dir_length,
            MAX_ROOT_DIR_SIZE
        );

        // With 10,000 tiles, we MUST have leaf directories
        assert!(
            leaf_dirs_offset > 0,
            "Large archive should have leaf directories (offset={})",
            leaf_dirs_offset
        );
        assert!(
            leaf_dirs_length > 0,
            "Large archive should have leaf directories (length={})",
            leaf_dirs_length
        );

        let _ = fs::remove_file(output_path);
    }

    #[test]
    fn test_small_archive_no_leaf_directories() {
        // Small archive should NOT use leaf directories (they're overhead)
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");

        // Add just a few tiles
        for i in 0..10 {
            let data = vec![0x1a, i as u8];
            writer.add_tile(0, 0, 0, &data).unwrap();
        }

        let output_path = Path::new("/tmp/test-no-leaf-directories.pmtiles");
        let _ = fs::remove_file(output_path);
        writer.finalize(output_path).unwrap();

        let data = fs::read(output_path).unwrap();
        let leaf_dirs_offset = u64::from_le_bytes(data[40..48].try_into().unwrap());
        let leaf_dirs_length = u64::from_le_bytes(data[48..56].try_into().unwrap());

        // Small archive should have no leaf directories
        assert_eq!(
            leaf_dirs_offset, 0,
            "Small archive should not have leaf directories"
        );
        assert_eq!(
            leaf_dirs_length, 0,
            "Small archive should not have leaf directories"
        );

        let _ = fs::remove_file(output_path);
    }

    #[test]
    fn test_leaf_directory_entries_have_run_length_zero() {
        // When leaf directories are used, root entries pointing to them
        // must have run_length = 0 (per PMTiles spec)
        let num_tiles = 3000;

        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");

        for i in 0..num_tiles {
            let x = i % 1024;
            let y = i / 1024;
            let data = vec![0x1a, (i & 0xff) as u8];
            writer.add_tile(10, x as u32, y as u32, &data).unwrap();
        }

        let output_path = Path::new("/tmp/test-leaf-run-length.pmtiles");
        let _ = fs::remove_file(output_path);
        writer.finalize(output_path).unwrap();

        let data = fs::read(output_path).unwrap();

        // Extract and decompress root directory
        let root_dir_offset = u64::from_le_bytes(data[8..16].try_into().unwrap()) as usize;
        let root_dir_length = u64::from_le_bytes(data[16..24].try_into().unwrap()) as usize;
        let leaf_dirs_length = u64::from_le_bytes(data[48..56].try_into().unwrap());

        // Only check if we have leaf directories
        if leaf_dirs_length > 0 {
            let compressed_root = &data[root_dir_offset..root_dir_offset + root_dir_length];

            use flate2::read::GzDecoder;
            use std::io::Read;
            let mut decoder = GzDecoder::new(compressed_root);
            let mut decompressed = Vec::new();
            decoder.read_to_end(&mut decompressed).unwrap();

            // Decode directory to verify run_length = 0 for leaf pointers
            let entries = decode_directory(&decompressed).unwrap();

            // All entries in root should be leaf pointers (run_length = 0)
            for entry in &entries {
                assert_eq!(
                    entry.run_length, 0,
                    "Root directory entries pointing to leaves must have run_length=0, got {}",
                    entry.run_length
                );
            }
        }

        let _ = fs::remove_file(output_path);
    }
}
