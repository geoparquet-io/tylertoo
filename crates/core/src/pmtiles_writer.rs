//! PMTiles v3 writer implementation.
//!
//! Implements the PMTiles v3 spec: https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md
//!
//! Key design decisions:
//! - Uses Hilbert curve ordering for tile IDs (spatial locality)
//! - Delta-encoded directories for better compression
//! - Configurable compression (gzip, brotli, zstd) for both directories and tiles
//! - Clustered mode for efficient sequential reads

use crate::compression::{self, Compression, MAX_INTERNAL_BYTES};
use crate::dedup::{DeduplicationCache, DeduplicationStats, TileHasher};
use crate::tile::TileBounds;
use crate::world_coord::MAX_LATITUDE;
use crate::{Error, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
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

/// The PMTiles v3 header's fixed on-disk size.
///
/// A reader that fetches the header before it knows anything else about the
/// archive — e.g. [`crate::archive_index::ArchiveIndex`] — needs exactly this
/// many bytes. See [`Header`] for the layout.
pub const HEADER_BYTES: usize = 127;

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
///
/// Its on-disk size is fixed: [`HEADER_BYTES`].
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

impl TileType {
    /// Parse a PMTiles spec byte code back into a tile type (byte 99).
    ///
    /// Returns `None` for codes outside the PMTiles v3 spec (0-5).
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(TileType::Unknown),
            1 => Some(TileType::Mvt),
            2 => Some(TileType::Png),
            3 => Some(TileType::Jpeg),
            4 => Some(TileType::Webp),
            5 => Some(TileType::Avif),
            _ => None,
        }
    }
}

impl Header {
    /// Parse a PMTiles v3 header from the first 127 bytes of an archive.
    ///
    /// Inverse of [`Header::to_bytes`]; the read side of the pipeline (issue
    /// #112) uses this to locate directories and tile data. Fails on short
    /// input, bad magic, unsupported version, or out-of-spec compression /
    /// tile-type codes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Header> {
        let err = |msg: String| Error::PMTilesRead(msg);
        if bytes.len() < 127 {
            return Err(err(format!(
                "file too short for PMTiles header: {} bytes (need 127)",
                bytes.len()
            )));
        }
        if &bytes[0..7] != PMTILES_MAGIC {
            return Err(err("bad magic: not a PMTiles archive".to_string()));
        }
        if bytes[7] != PMTILES_VERSION {
            return Err(err(format!(
                "unsupported PMTiles version {} (only v3 is supported)",
                bytes[7]
            )));
        }

        let read_u64 =
            |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8-byte slice"));
        let read_coord = |at: usize| {
            f64::from(i32::from_le_bytes(
                bytes[at..at + 4].try_into().expect("4-byte slice"),
            )) / 10_000_000.0
        };

        // ---- Semantic validation (#417) -----------------------------------
        // Everything below drives later slices, tile-id math and allocation
        // sizes, and all of it is attacker-controlled in a foreign archive.
        // Nonsense is rejected here, once, rather than defended against at
        // every use. What is merely *sloppy* — a field nothing downstream
        // reads — is repaired and logged: rejecting it would lock out real
        // archives without protecting anything.
        let internal_compression = Compression::from_code(bytes[97])
            .filter(|c| *c != Compression::Unknown)
            .ok_or_else(|| err(format!("invalid internal compression code {}", bytes[97])))?;
        let tile_compression = Compression::from_code(bytes[98])
            .filter(|c| *c != Compression::Unknown)
            .ok_or_else(|| err(format!("invalid tile compression code {}", bytes[98])))?;
        let tile_type = TileType::from_code(bytes[99])
            .ok_or_else(|| err(format!("invalid tile type code {}", bytes[99])))?;

        let (raw_min_zoom, max_zoom, raw_center_zoom) = (bytes[100], bytes[101], bytes[118]);
        // `max_zoom` is load-bearing: `max_expanded_entries` sizes the
        // directory-walk budget from it, and a zoom past z31 has no tile-id
        // space at all. That one is an error.
        if max_zoom > MAX_TILE_ID_ZOOM {
            return Err(err(format!(
                "max zoom {max_zoom} is past z{MAX_TILE_ID_ZOOM}, \
                 the deepest zoom a PMTiles tile id can address"
            )));
        }
        // `min_zoom` and `center_zoom` are not: nothing in the decode,
        // pyramid-merge or export path reads either. They are display
        // metadata, and plenty of real archives carry a sloppy value for
        // them (a merged or hand-edited header, a `min_zoom` left at the
        // source's rather than the archive's). Clamp and say so.
        let min_zoom = if raw_min_zoom > max_zoom {
            log::warn!(
                "PMTiles header declares min zoom {raw_min_zoom} deeper than max zoom \
                 {max_zoom}; reading it as z{max_zoom}"
            );
            max_zoom
        } else {
            raw_min_zoom
        };
        // A center *deeper* than the archive goes nowhere; a center zoom of 0
        // is the near-universal "unset" value even in archives that start at
        // z5, so only the upper end is touched.
        let center_zoom = if raw_center_zoom > max_zoom {
            log::warn!(
                "PMTiles header declares center zoom {raw_center_zoom} deeper than max zoom \
                 {max_zoom}; reading it as z{max_zoom}"
            );
            max_zoom
        } else {
            raw_center_zoom
        };

        // Section bounds. How they compare to the *file* size is checked by
        // the callers that know it (a header may legitimately be parsed from
        // the first 127 bytes of a range request); what can be said here is
        // that no section may wrap u64.
        let root_dir_offset = read_u64(8);
        let root_dir_length = read_u64(16);
        let json_metadata_offset = read_u64(24);
        let json_metadata_length = read_u64(32);
        let leaf_dirs_offset = read_u64(40);
        let leaf_dirs_length = read_u64(48);
        let tile_data_offset = read_u64(56);
        let tile_data_length = read_u64(64);
        for (what, offset, length) in [
            ("root directory", root_dir_offset, root_dir_length),
            ("JSON metadata", json_metadata_offset, json_metadata_length),
            ("leaf directories", leaf_dirs_offset, leaf_dirs_length),
            ("tile data", tile_data_offset, tile_data_length),
        ] {
            if offset.checked_add(length).is_none() {
                return Err(err(format!(
                    "{what} section at offset {offset} with length {length} overflows u64"
                )));
            }
        }

        Ok(Header {
            root_dir_offset,
            root_dir_length,
            json_metadata_offset,
            json_metadata_length,
            leaf_dirs_offset,
            leaf_dirs_length,
            tile_data_offset,
            tile_data_length,
            addressed_tiles_count: read_u64(72),
            tile_entries_count: read_u64(80),
            tile_contents_count: read_u64(88),
            clustered: bytes[96] == 1,
            internal_compression,
            tile_compression,
            tile_type,
            min_zoom,
            max_zoom,
            min_lon: read_coord(102),
            min_lat: read_coord(106),
            max_lon: read_coord(110),
            max_lat: read_coord(114),
            center_zoom,
            center_lon: read_coord(119),
            center_lat: read_coord(123),
        })
    }
}

/// Highest zoom a `u64` PMTiles tile id can address.
///
/// The cumulative Hilbert id at zoom `z` needs `4^z` of headroom, so z31
/// (`4^31 = 2^62`) is the last zoom that fits (#371). This is the *addressing*
/// limit of the format, deliberately one notch above what tylertoo will write
/// ([`crate::tile::MAX_ZOOM`]): the writer's cap is a validation decision, this
/// one is arithmetic.
pub const MAX_TILE_ID_ZOOM: u8 = 31;

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
    // #371: the cumulative base is `sum(4^i for i in 1..z)`, and `4u64.pow(i)`
    // overflows u64 at z32 — a debug panic, a wrapped id in release. z32 is
    // also past what a u64 tile id can address at all, so an out-of-range zoom
    // returns a sentinel beyond the z31 address space rather than a plausible
    // but wrong id: `tile_id_to_zxy` rejects it. Write paths never reach this —
    // `tile::MAX_ZOOM` is enforced at options validation.
    if z > MAX_TILE_ID_ZOOM {
        return u64::MAX;
    }
    // Closed form of `sum(4^i for i in 1..z)` = (4^z - 4) / 3, evaluated in u64
    // (exact for z <= 31: 4^31 = 2^62).
    let base_id: u64 = ((1u64 << (2 * u32::from(z))) - 4) / 3;
    let hilbert_idx = xy_to_hilbert(z, x, y);
    base_id + hilbert_idx + 1
}

/// [`tile_id`] for the writer paths: an out-of-range zoom is an error, not a
/// sentinel (#371).
///
/// `tile_id` returns `u64::MAX` past [`MAX_TILE_ID_ZOOM`] so that a *reading*
/// caller gets an id that cannot decode. A writer must not store that: every
/// tile added above the ceiling would collide on the same directory entry and
/// `finalize` would happily produce an archive addressing one impossible tile.
/// The `add_tile*` family already returns [`std::io::Result`], so the zoom is
/// rejected there — before the id is computed, before any bytes are written.
fn checked_tile_id(z: u8, x: u32, y: u32) -> std::io::Result<u64> {
    if z > MAX_TILE_ID_ZOOM {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "zoom {z} has no PMTiles tile id: the cumulative Hilbert id is u64, \
                 so z{MAX_TILE_ID_ZOOM} is the highest addressable zoom (tylertoo \
                 writes at most z{}) (#371)",
                crate::tile::MAX_ZOOM
            ),
        ));
    }
    Ok(tile_id(z, x, y))
}

/// Convert x,y coordinates to Hilbert curve index at zoom level z
///
/// Implementation follows the standard Hilbert curve algorithm:
/// https://en.wikipedia.org/wiki/Hilbert_curve
pub(crate) fn xy_to_hilbert(z: u8, x: u32, y: u32) -> u64 {
    // #371: `1u32 << z` overflows at z32 — in release it masks the shift to
    // `z & 31`, so z32 yields n = 1 and every tile id in the archive is wrong
    // with no diagnostic. u64 covers the whole z<=31 PMTiles address space.
    //
    // The assert and the clamp are unreachable through either of this
    // function's two callers, both of which bound `z` themselves before
    // calling in: `tile_id` returns the out-of-range sentinel before it gets
    // here, and `crate::tile::node_id_range` (#506) carries the identical
    // debug_assert + clamp pair on its own `target_z` before it does. They
    // are kept as this function's own guard rail regardless — the assert
    // restates the contract at the point that actually needs it, and the
    // clamp keeps a release build total (an unclamped `1u64 << z` still
    // masks for z >= 64) rather than silently resuming with a wrong `n`.
    debug_assert!(z <= MAX_TILE_ID_ZOOM, "tile_id must bound z first (#371)");
    let n: u64 = 1u64 << z.min(MAX_TILE_ID_ZOOM);
    let mut rx: u64;
    let mut ry: u64;
    let mut s: u64;
    let mut d: u64 = 0;
    let mut x = u64::from(x);
    let mut y = u64::from(y);

    s = n / 2;
    while s > 0 {
        rx = if (x & s) > 0 { 1 } else { 0 };
        ry = if (y & s) > 0 { 1 } else { 0 };
        d += s * s * ((3 * rx) ^ ry);

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

/// Hard ceiling on how many tiles one directory walk may expand to (#417).
///
/// **The number this picks is a peak-memory target, not an address count.**
/// Each expanded entry costs tens of bytes of reader state — 32 bytes for a
/// `TileRef` in [`crate::decode`], more for a `BTreeMap` node in the pyramid
/// merge — and both are held for the whole walk. 2^26 entries is therefore
/// about 2.1 GB of `TileRef` (more in the merge): heavy, but a bound a real
/// machine survives, chosen so that no header value can turn a kilobyte of
/// directory into tens of gigabytes of resident memory.
///
/// It deliberately does *not* track the archive's address space. A fully
/// dense z0-z14 pyramid addresses ~3.6e8 tiles, so `max_zoom = 14` — an
/// entirely ordinary header byte — would otherwise license 8.6 GB here. The
/// ceiling cannot be much tighter, though: a *legitimate* planet-scale band
/// (land-covering features at z13 is tens of millions of tiles) must still
/// merge, so the constant sits above real archives and below the attack.
///
/// [`max_expanded_entries`] takes this and the archive's own address space,
/// whichever is smaller.
pub const MAX_EXPANDED_TILE_ENTRIES: u64 = 1 << 26;

/// Hard ceiling on how many leaf directories one walk may visit (#417).
///
/// [`MAX_EXPANDED_TILE_ENTRIES`] bounds the entries a walk accumulates, but
/// an empty leaf costs nothing against that budget while still costing a
/// bounded-but-real decompression (up to
/// [`crate::compression::MAX_INTERNAL_BYTES`]) and a transient `DirEntry`
/// allocation. A root full of pointers at the same leaf body is a ~50 KB
/// file that would otherwise keep a reader busy for as long as the pointers
/// last. One huge leaf and a million tiny ones are separate attacks, so this
/// is a separate cap.
///
/// 16384 tracks the entry budget: this writer partitions leaves at
/// `INITIAL_LEAF_SIZE` (4096) entries apiece, so 16384 leaves address the
/// same 67M tiles [`MAX_EXPANDED_TILE_ENTRIES`] admits — a sanely packed
/// archive exhausts both budgets together rather than tripping the leaf cap
/// while entries remain.
pub const MAX_LEAF_DIRECTORIES: usize = 16384;

/// Number of tile ids addressable at or above `max_zoom`'s pyramid: the sum
/// of `4^z` for `z` in `0..=max_zoom`, saturating at
/// [`MAX_EXPANDED_TILE_ENTRIES`].
fn tile_address_space(max_zoom: u8) -> u64 {
    (0..=max_zoom.min(MAX_TILE_ID_ZOOM))
        .try_fold(0u64, |acc, z| acc.checked_add(1u64 << (2 * u32::from(z))))
        .unwrap_or(u64::MAX)
}

/// How many tiles a directory walk over `header`'s archive may expand to.
///
/// Two bounds, whichever is tighter: the archive cannot address more tiles
/// than its own zoom range holds, and no archive worth reading expands past
/// [`MAX_EXPANDED_TILE_ENTRIES`]. Both operands are derived from the header,
/// which [`Header::from_bytes`] has already range-checked.
///
/// Safe on a hand-built [`Header`] that never went through
/// [`Header::from_bytes`] as well: `tile_address_space` clamps `max_zoom`
/// to [`MAX_TILE_ID_ZOOM`] itself, and the `min` with
/// [`MAX_EXPANDED_TILE_ENTRIES`] means the result is bounded whatever
/// `max_zoom` says.
pub fn max_expanded_entries(header: &Header) -> u64 {
    tile_address_space(header.max_zoom).min(MAX_EXPANDED_TILE_ENTRIES)
}

/// Convert a PMTiles TileID back to tile coordinates (z, x, y).
///
/// Inverse of [`tile_id`]. Supports zoom levels 0-31 (the range a u64
/// cumulative Hilbert ID can address); returns an error for IDs beyond z31.
pub fn tile_id_to_zxy(id: u64) -> Result<(u8, u32, u32)> {
    let mut acc: u64 = 0;
    for z in 0u8..=MAX_TILE_ID_ZOOM {
        let num = 1u64 << (2 * u64::from(z));
        if id - acc < num {
            let (x, y) = hilbert_d2xy(z, id - acc);
            return Ok((z, x, y));
        }
        acc += num;
    }
    Err(Error::PMTilesRead(format!(
        "tile id {id} exceeds the zoom 31 address space"
    )))
}

/// Convert a Hilbert curve index back to x,y coordinates at zoom level z
///
/// Standard inverse Hilbert algorithm (d2xy), mirroring [`xy_to_hilbert`]:
/// https://en.wikipedia.org/wiki/Hilbert_curve
fn hilbert_d2xy(z: u8, d: u64) -> (u32, u32) {
    let n = 1u64 << z;
    let (mut x, mut y) = (0u64, 0u64);
    let mut t = d;
    let mut s = 1u64;
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        // Rotate quadrant - the inverse uses s-1 (current block size - 1),
        // where the forward transform uses n-1 (see xy_to_hilbert).
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (x as u32, y as u32)
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
///
/// Only the canonical encoding is accepted (#417). A u64 varint is at most
/// ten bytes, and the tenth carries exactly one payload bit: `shift` is 63
/// there, so bits 1-6 of `byte & 0x7f` would be shifted straight out of the
/// register. Silently wrapping means an attacker picks any value they like
/// and the reader sees a plausible small one instead — an offset, a length
/// or a tile id that passed no check the caller believes it passed.
pub fn decode_varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate() {
        if shift == 63 && byte & 0x7f > 1 {
            return None; // Non-canonical: the tenth byte's high bits do not fit a u64.
        }
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

        // Update expected offset for next entry. The spec's contiguity rule
        // applies to every entry, leaf pointers (run_length == 0) included:
        // their `length` is the compressed leaf size and leaves are laid out
        // back to back in the leaf section (#377).
        expected_offset = entry.offset + entry.length as u64;
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

    if count == 0 {
        return Some(Vec::new());
    }

    // The count is an archive-controlled varint and it sizes an allocation,
    // so it is checked against what the body could possibly hold rather than
    // trusted (#397): every entry contributes at least one varint byte to
    // each of the four columns.
    let count = usize::try_from(count).ok()?;
    if count > (data.len() - offset) / 4 {
        return None;
    }

    let mut entries = Vec::with_capacity(count);

    // Decode delta-encoded tile IDs
    let mut last_id = 0u64;
    for _ in 0..count {
        let (delta, consumed) = decode_varint(&data[offset..])?;
        offset += consumed;
        // Both operands come from the archive: a wrapping sum would land the
        // entry on an unrelated (and in-bounds-looking) tile id.
        last_id = last_id.checked_add(delta)?;
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
        // Truncating here would turn a 2^32 run length into 0 — a *leaf
        // pointer* — and the walker would then slice tile bytes as a
        // directory.
        entry.run_length = u32::try_from(run_length).ok()?;
    }

    // Decode lengths
    for entry in entries.iter_mut() {
        let (length, consumed) = decode_varint(&data[offset..])?;
        offset += consumed;
        // Likewise: 2^32 + 10 must not quietly become a 10-byte read.
        entry.length = u32::try_from(length).ok()?;
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

        // Update expected offset for next entry — for leaf pointers too.
        // tippecanoe and go-pmtiles encode every leaf after the first as
        // contiguous; gating this on run_length > 0 resolved them all to
        // offset 0 and `decode` failed with "incomplete deflate stream" (#377).
        // Both operands come from the archive, so the sum is checked.
        expected_offset = entry.offset.checked_add(u64::from(entry.length))?;
    }

    Some(entries)
}

/// Read every addressed directory entry out of a PMTiles archive's raw
/// bytes: the root directory, with any leaf directory it points at expanded
/// inline.
///
/// Entries come back in the order the root directory stores them, which is
/// ascending tile id — the same order [`StreamingPmtilesWriter::entries_are_clustered`]
/// walks, so a caller comparing the two notions of "clustered" is comparing
/// like with like.
///
/// Bounded by two independent budgets (#417), mirroring the walk
/// [`crate::pyramid::BandArchive::open`] used to perform inline before this
/// was extracted: total expanded entries
/// ([`max_expanded_entries`]) and leaf-directory count
/// ([`MAX_LEAF_DIRECTORIES`]), since one huge leaf and a million tiny ones
/// are different attacks. Multi-level leaf directories (a leaf pointing at
/// another leaf) are rejected rather than silently mis-parsed as tiles — the
/// spec allows arbitrary depth, but no writer here produces more than one
/// level.
pub(crate) fn read_all_entries(bytes: &[u8], header: &Header) -> Result<Vec<DirEntry>> {
    read_all_entries_from(&bytes, header)
}

/// Where [`read_all_entries_from`] gets the bytes it needs.
///
/// A whole archive in memory (`&[u8]`) and a `File` read by offset
/// ([`crate::archive_index::ArchiveIndex`]) answer this identically, which is
/// the point: the directory walk — and every ceiling and error message #417
/// put on it — is written once and both readers inherit it. Only the *four*
/// ranges a walk actually touches (root dir, each leaf dir) are ever fetched,
/// so the file-backed source never materializes tile data.
///
/// Callers bounds-check `offset + len` against [`Self::total_len`] before
/// calling, so an implementation may assume the range is in bounds.
pub(crate) trait ArchiveBytes {
    /// The archive's total size in bytes.
    fn total_len(&self) -> u64;
    /// `len` bytes at `offset`, borrowed when the source already holds them.
    fn read_range(&self, offset: usize, len: usize) -> Result<std::borrow::Cow<'_, [u8]>>;
}

impl ArchiveBytes for &[u8] {
    fn total_len(&self) -> u64 {
        self.len() as u64
    }

    fn read_range(&self, offset: usize, len: usize) -> Result<std::borrow::Cow<'_, [u8]>> {
        Ok(std::borrow::Cow::Borrowed(&self[offset..offset + len]))
    }
}

/// [`read_all_entries`] over any [`ArchiveBytes`] source.
pub(crate) fn read_all_entries_from<S: ArchiveBytes + ?Sized>(
    src: &S,
    header: &Header,
) -> Result<Vec<DirEntry>> {
    let past_end = |what: &str| Error::PMTilesWrite(format!("{what} past end of archive"));
    let slice = |off: u64, len: u64, what: &str| -> Result<std::borrow::Cow<'_, [u8]>> {
        // #417, extended in #510: the declared length is checked against the
        // internal-section ceiling BEFORE anything is allocated. Bounding it
        // by the file's own length is no bound at all for a file-backed
        // source — a header claiming a 256 MiB root directory made a reader
        // allocate 256 MiB per archive, and a merge opens every shard at
        // once. The section is decompressed under the same ceiling anyway, so
        // a compressed body larger than it cannot be a legitimate directory.
        if len > MAX_INTERNAL_BYTES {
            return Err(Error::PMTilesWrite(format!(
                "{what} claims {len} bytes, which exceeds the {MAX_INTERNAL_BYTES}-byte \
                 ceiling on an archive's internal sections"
            )));
        }
        // Both come from the archive; a `as usize` truncation on a 32-bit
        // target would turn a wild offset into a plausible in-range one.
        let start = usize::try_from(off).map_err(|_| past_end(what))?;
        let end = usize::try_from(len)
            .ok()
            .and_then(|l| start.checked_add(l))
            .filter(|&e| u64::try_from(e).is_ok_and(|e| e <= src.total_len()))
            .ok_or_else(|| past_end(what))?;
        src.read_range(start, end - start)
    };
    // An entry's range must lie inside the section it is relative to, not
    // merely inside the file: a leaf pointer aimed at the tile data would
    // otherwise be parsed as a directory (#417).
    let within = |off: u64, len: u64, section_len: u64, what: &str| -> Result<()> {
        match off.checked_add(len) {
            Some(end) if end <= section_len => Ok(()),
            _ => Err(Error::PMTilesWrite(format!(
                "{what} at {off} ({len} bytes) extends past its {section_len}-byte section"
            ))),
        }
    };
    let dir = |raw: &[u8], what: &str| -> Result<Vec<DirEntry>> {
        let plain = compression::decompress_capped(
            raw,
            header.internal_compression,
            compression::MAX_INTERNAL_BYTES,
        )
        .map_err(|e| Error::PMTilesWrite(format!("{what}: {e}")))?;
        decode_directory(&plain).ok_or_else(|| Error::PMTilesWrite(format!("undecodable {what}")))
    };

    let root = dir(
        &slice(header.root_dir_offset, header.root_dir_length, "root dir")?,
        "root dir",
    )?;
    // Entries accumulated across the walk are bounded by what the archive
    // could legitimately address; the number of leaves is bounded
    // separately, since an empty leaf costs no entries while still costing a
    // bounded-but-real decompression apiece.
    let entry_limit = max_expanded_entries(header);
    let mut entry_budget = entry_limit;
    let mut leaves_visited = 0usize;
    let too_many_entries = || {
        Error::PMTilesWrite(format!(
            "directory entries exceed this archive's limit of {entry_limit} entries"
        ))
    };

    let mut entries = Vec::new();
    for e in root {
        if e.run_length != 0 {
            entry_budget = entry_budget.checked_sub(1).ok_or_else(too_many_entries)?;
            entries.push(e);
            continue;
        }
        leaves_visited += 1;
        if leaves_visited > MAX_LEAF_DIRECTORIES {
            return Err(Error::PMTilesWrite(format!(
                "root directory points at more than {MAX_LEAF_DIRECTORIES} leaf directories"
            )));
        }
        within(
            e.offset,
            u64::from(e.length),
            header.leaf_dirs_length,
            "leaf dir",
        )?;
        // Base and entry offset both come from the archive, so the sum is
        // checked rather than wrapped into a plausible-looking one.
        let leaf_at = header
            .leaf_dirs_offset
            .checked_add(e.offset)
            .ok_or_else(|| Error::PMTilesWrite("leaf dir offset overflow".to_string()))?;
        let leaf = slice(leaf_at, u64::from(e.length), "leaf dir")?;
        let decoded = dir(&leaf, "leaf dir")?;
        // Spent before the entries are kept, not after: a root full of
        // pointers at one 16 MiB leaf body is a ~50 KB file that would
        // otherwise accumulate entries until the process died.
        entry_budget = entry_budget
            .checked_sub(decoded.len() as u64)
            .ok_or_else(too_many_entries)?;
        for inner in decoded {
            // run_length 0 inside a leaf is a second-level leaf pointer. The
            // spec allows arbitrarily deep directories; this reader handles
            // one level, and falling through would emit directory bytes as a
            // tile.
            if inner.run_length == 0 {
                return Err(Error::PMTilesWrite(
                    "multi-level leaf directories are not supported".to_string(),
                ));
            }
            entries.push(inner);
        }
    }

    Ok(entries)
}

/// Whether directory entries, in the order a reader walks them (ascending
/// tile id), obey the PMTiles v3 "clustered" contract.
///
/// Enforces the rule go-pmtiles' `verify` *intends* for a clustered archive:
/// walking entries in order with a high-water mark on how far into the
/// tile-data section has been read, each entry's offset must either extend
/// that mark (`offset == end`, a freshly written tile) or exactly match an
/// offset already seen earlier in the walk *with the same length* (a
/// deduplication back-reference to a whole prior tile — legal in a
/// clustered archive, since a reader that already streamed those bytes can
/// just reuse them verbatim). Anything else — an offset ahead of the mark,
/// a back-reference into the *middle* of an earlier tile rather than its
/// exact start, or one that claims a different number of bytes than the
/// tile it points at — means a client streaming tile data in directory
/// order would have to seek backwards past unread bytes, forwards over a
/// gap, or land mid-tile or past a tile's end, which is exactly what
/// "clustered" promises never happens.
///
/// # Relationship to go-pmtiles
///
/// The shipped go-pmtiles clustered check is **inert**: `pmtiles/verify.go:111`
/// does `offsets.Add(e.Offset)` for *every* entry before the
/// `!offsets.Contains(e.Offset)` test at `pmtiles/verify.go:126-133`, so by
/// the time the guarded branch is reached the current entry's own offset is
/// always already in the set and the `e.Offset != currentOffset` complaint
/// can never fire. Even if it did, it is a `logger.Printf` warning, not an
/// error. So this predicate is not a port of what go-pmtiles *does* — it is
/// the rule go-pmtiles is written to express, actually enforced.
///
/// The seen-set is also populated differently on purpose: go-pmtiles adds
/// every entry's offset, this adds only fresh appends. The two are
/// equivalent for the test that matters, because a legal back-reference
/// points at an offset that was already appended (and therefore already
/// seen), and an illegal one is exactly the case go-pmtiles' unconditional
/// `Add` masks.
pub(crate) fn offsets_are_clustered(entries: impl IntoIterator<Item = (u64, u64)>) -> bool {
    let mut end = 0u64;
    // offset -> length of the tile appended at that offset. The length is
    // tracked, not just the offset, so a back-reference must name a whole
    // prior tile rather than a prefix or an overrun of one.
    let mut seen: HashMap<u64, u64> = HashMap::new();
    for (offset, length) in entries {
        if offset == end {
            seen.insert(offset, length);
            end = end.saturating_add(length);
        } else if seen.get(&offset) == Some(&length) {
            // dedup back-reference to an exact previously-seen entry offset
            // *and* its exact length; `end` unchanged
        } else {
            return false;
        }
    }
    true
}

/// Whether the PMTiles archive at `path` is genuinely clustered, independent
/// of what its header claims.
///
/// Re-derives the same [`offsets_are_clustered`] predicate
/// [`StreamingPmtilesWriter::entries_are_clustered`] uses when it stamps the
/// header, but from the bytes actually on disk via [`read_all_entries`] —
/// so a writer bug that sets the flag wrong cannot also fool this check by
/// sharing its assumptions. Intended for tests and tooling (a go-pmtiles
/// `verify`-alike), not the write path itself.
///
/// On top of the ordering predicate this also checks the two archive-level
/// invariants go-pmtiles `verify` enforces around it, because an archive
/// that fails either is one the tool rejects outright:
///
/// * every entry lies inside the tile-data section —
///   `offset + length <= header.tile_data_length` (`pmtiles/verify.go:122-124`);
/// * `header.tile_contents_count` equals the number of *distinct* offsets
///   the directory references (`pmtiles/verify.go:148-150`, a hard error
///   there). This is what catches a writer that stored the same content
///   twice and left one copy unreferenced.
pub fn verify_clustered(path: &Path) -> Result<bool> {
    let bytes = std::fs::read(path)
        .map_err(|e| Error::PMTilesRead(format!("failed to read {}: {e}", path.display())))?;
    let header = Header::from_bytes(&bytes)?;
    let entries = read_all_entries(&bytes, &header)?;

    // Every entry must lie wholly inside the tile-data section. `checked_add`
    // so an offset/length pair that wraps u64 fails rather than aliasing a
    // small in-range end.
    for e in &entries {
        match e.offset.checked_add(u64::from(e.length)) {
            Some(end) if end <= header.tile_data_length => {}
            _ => return Ok(false),
        }
    }

    // The header's stored-blob count must match the directory's distinct
    // referenced offsets: a mismatch means bytes in the tile-data section
    // that nothing points at (or a miscount), which go-pmtiles hard-errors.
    let distinct_offsets: HashSet<u64> = entries.iter().map(|e| e.offset).collect();
    if distinct_offsets.len() as u64 != header.tile_contents_count {
        return Ok(false);
    }

    Ok(offsets_are_clustered(
        entries.iter().map(|e| (e.offset, u64::from(e.length))),
    ))
}

// ============================================================================
// Leaf Directory Support (Issue #88)
// ============================================================================

/// Maximum size for root directory to fit in initial 16KB HTTP range request.
/// PMTiles header is 127 bytes, leaving 16384 - 127 = 16257 bytes for root directory.
const MAX_ROOT_DIR_BYTES: usize = 16384 - 127;

/// Bytes a **tail-layout** archive reserves up front for its header and root
/// directory (#459), zero-padded to the boundary.
///
/// 16,384 is not a round number picked for looks. It is the initial range
/// request every PMTiles client makes, the budget [`MAX_ROOT_DIR_BYTES`]
/// already sizes the root against — and, decisively, the *only* padding
/// go-pmtiles' `verify` tolerates. `verify.go` (v1.31.2, L84-89) accepts a
/// file whose length equals either
///
/// ```text
/// 127 + root + metadata + leaves + tile_data        // the packed layout
/// 16384     + metadata + leaves + tile_data         // this one
/// ```
///
/// and rejects everything else, so an archive may have slack *here* and
/// nowhere else. That is what makes the tail layout possible: tile data can
/// start at a fixed offset known before the first tile is written, which is
/// what lets a checkpoint rewrite only the prefix and the tail instead of
/// re-copying every tile byte.
pub const TAIL_LAYOUT_PREFIX_BYTES: u64 = 16384;

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

/// Render the TileJSON `fields` object for a layer.
///
/// Shared by `PmtilesWriter` and `StreamingPmtilesWriter`: both emit the same
/// metadata, and a second copy of this drifted once already. Field names are
/// sorted so the output is byte-for-byte deterministic.
fn fields_json(fields: &HashMap<String, String>) -> String {
    if fields.is_empty() {
        return "{}".to_string();
    }

    let mut field_pairs: Vec<_> = fields.iter().collect();
    field_pairs.sort_by_key(|(k, _)| *k);

    let field_strings: Vec<String> = field_pairs
        .iter()
        .map(|(name, type_str)| format!(r#""{}":"{}""#, name, type_str))
        .collect();

    format!("{{{}}}", field_strings.join(","))
}

/// Render the TileJSON `tilestats` fragment, including its trailing comma.
///
/// Returns an empty string when the archive holds no features, which keeps the
/// surrounding metadata object valid. Shared with `fields_json` above.
fn tilestats_json(layer_name: &str, total_features: u64, field_count: usize) -> String {
    if total_features == 0 {
        return String::new();
    }

    format!(
        r#""tilestats":{{"layerCount":1,"layers":[{{"layer":"{}","count":{},"attributeCount":{}}}]}},"#,
        layer_name, total_features, field_count
    )
}

/// Tile entry with hash for deduplication
#[derive(Debug, Clone)]
struct TileEntry {
    /// Compressed tile data (only stored for unique tiles)
    data: Option<Vec<u8>>,
    /// Hash of uncompressed content (for deduplication)
    hash: u64,
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
    /// Verbatim `vector_layers` array, when the archive holds more than the one
    /// layer `layer_name`/`fields` can describe (a merged band pyramid). When
    /// set it replaces the single-layer entry the writer would otherwise build.
    vector_layers_json: Option<String>,
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
            vector_layers_json: None,
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
            vector_layers_json: None,
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

    /// Replace the whole `vector_layers` array with a verbatim JSON array.
    ///
    /// A single-layer archive is described by `layer_name` + `fields`; a merged
    /// band pyramid has several layers, each with its own zoom range and field
    /// set, which that pair cannot express.
    pub fn set_vector_layers_json(&mut self, json: String) {
        self.vector_layers_json = Some(json);
    }

    /// Set field metadata for vector_layers.fields
    ///
    /// Field types should be MVT-style: "String", "Number", or "Boolean"
    pub fn set_fields(&mut self, fields: HashMap<String, String>) {
        self.fields = fields;
    }

    /// Build the fields JSON object string
    fn build_fields_json(&self) -> String {
        fields_json(&self.fields)
    }

    /// Build the tilestats JSON fragment
    fn build_tilestats_json(&self) -> String {
        tilestats_json(&self.layer_name, self.total_features, self.fields.len())
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
        let id = checked_tile_id(z, x, y)?;
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
        let id = checked_tile_id(z, x, y)?;
        // For pre-compressed tiles, use a unique hash based on the compressed data
        // This won't deduplicate as effectively but preserves the API
        let hash = TileHasher::hash(&compressed_data);
        self.tiles.insert(
            id,
            TileEntry {
                data: Some(compressed_data),
                hash,
            },
        );

        self.min_zoom = self.min_zoom.min(z);
        self.max_zoom = self.max_zoom.max(z);

        Ok(())
    }

    /// Set geographic bounds for the tileset
    ///
    /// Latitude values are clamped to the Web Mercator bound
    /// (`±`[`crate::world_coord::MAX_LATITUDE`]).
    pub fn set_bounds(&mut self, bounds: &TileBounds) {
        self.bounds = TileBounds::new(
            bounds.lng_min,
            bounds.lat_min.clamp(-MAX_LATITUDE, MAX_LATITUDE),
            bounds.lng_max,
            bounds.lat_max.clamp(-MAX_LATITUDE, MAX_LATITUDE),
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
            // With deduplication: store unique tiles, reference duplicates.
            //
            // Two passes (#516): a duplicate's tile_id is not necessarily
            // greater than the tile_id of the tile that carries its bytes --
            // which one "carries" the data is decided by add order
            // (whichever `add_tile` call for a given hash happened first),
            // not by tile_id. A single pass over `self.tiles` in tile_id
            // order can therefore reach a duplicate before its carrier and
            // find no entry in `hash_to_offset` yet.
            //
            // Pass 1: write every carrying tile's bytes and record its
            // offset, walking tile_id order (matters for determinism and
            // clusteredness, not byte correctness -- pass 2 resolves every
            // hash regardless of the order this pass visits carriers in).
            //
            // First carrier wins: more than one tile can hold data for the
            // same hash (`add_tile_compressed` never consults the dedup
            // cache, so two identical pre-compressed blobs both arrive
            // carrying bytes). Storing each of them and letting the last
            // `insert` win would append dead bytes nothing references and
            // leave `tile_contents_count` above the directory's distinct
            // offset count -- which go-pmtiles `verify` rejects outright.
            // Skipping the later carriers deduplicates them instead.
            for entry in self.tiles.values() {
                if let Some(ref data) = entry.data {
                    if hash_to_offset.contains_key(&entry.hash) {
                        // another carrier already stored these bytes
                        continue;
                    }
                    let offset = tile_data_buf.len() as u64;
                    let length = data.len() as u32;
                    tile_data_buf.extend_from_slice(data);
                    hash_to_offset.insert(entry.hash, (offset, length));
                    unique_contents += 1;
                }
            }

            // Pass 2: build directory entries in tile_id order. Every hash
            // now resolves, regardless of how add order and tile_id order
            // related to each other.
            for (&id, entry) in &self.tiles {
                // Not infallible: re-adding a tile_id with different content
                // replaces the only `TileEntry` that carried some earlier
                // hash's bytes, orphaning any duplicate still pointing at it.
                let (offset, length) = *hash_to_offset.get(&entry.hash).ok_or_else(|| {
                    Error::PMTilesWrite(format!(
                        "no stored bytes for tile id {id} (content hash {:#018x}): \
                         its carrier tile was overwritten by a later add",
                        entry.hash
                    ))
                })?;

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
                // Not infallible either: `enable_deduplication(false)` after
                // tiles were added leaves behind entries recorded as
                // references to a carrier tile, with no data of their own,
                // which this branch has no `hash_to_offset` to resolve.
                let data = entry.data.as_ref().ok_or_else(|| {
                    Error::PMTilesWrite(format!(
                        "no stored bytes for tile id {id} (content hash {:#018x}): \
                         it was recorded as a deduplication reference to a carrier \
                         tile, but this archive is being written with deduplication \
                         disabled",
                        entry.hash
                    ))
                })?;
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

        // Split into a root directory plus leaf directories when the entries do
        // not fit the spec's 16 KiB root budget. Writing one oversized root
        // instead produces an archive that readers reject outright: go-pmtiles
        // reads the first 16 KiB and panics slicing past it. This only bites
        // above a few thousand tiles, which is why it went unnoticed while this
        // writer was exercised solely by small tests.
        let layout = make_root_leaves(&entries, self.internal_compression)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to build directories: {}", e)))?;
        let compressed_dir = layout.root_bytes;
        let leaves_bytes = layout.leaves_bytes;

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
        let tilestats_json = self.build_tilestats_json();
        let vector_layers = match &self.vector_layers_json {
            Some(json) => json.clone(),
            None => format!(
                r#"[{{"id":"{}","minzoom":{},"maxzoom":{},"fields":{}}}]"#,
                self.layer_name,
                min_z,
                max_z,
                self.build_fields_json()
            ),
        };
        let metadata = format!(
            r#"{{"vector_layers":{},{}"format":"pbf","generator":"tylertoo"}}"#,
            vector_layers, tilestats_json
        );
        let compressed_metadata =
            compression::compress(metadata.as_bytes(), self.internal_compression)
                .map_err(|e| Error::PMTilesWrite(format!("Failed to compress metadata: {}", e)))?;

        // Calculate section offsets
        let root_dir_offset = 127u64;
        let root_dir_length = compressed_dir.len() as u64;
        let metadata_offset = root_dir_offset + root_dir_length;
        let metadata_length = compressed_metadata.len() as u64;
        // Leaf directories sit between the metadata and the tile data, and
        // `DirEntry::offset` for a leaf is relative to leaf_dirs_offset.
        let leaf_dirs_offset = metadata_offset + metadata_length;
        let leaf_dirs_length = leaves_bytes.len() as u64;
        let tile_data_offset = leaf_dirs_offset + leaf_dirs_length;
        let tile_data_length = tile_data_buf.len() as u64;

        // Without deduplication, `self.tiles` (a `BTreeMap`) walked in
        // tile_id order is clustered by construction: each tile's offset is
        // the buffer's length at that point in the ascending walk, so
        // offsets are strictly monotonic. With deduplication (#516), a
        // duplicate's tile_id can fall anywhere relative to the tile_id of
        // the tile that carries its bytes (carrying is decided by add
        // order), so a duplicate can be the first entry, in tile_id order,
        // to reference an offset that some *other* hash's carrier claimed
        // later in the walk -- a legitimate multi-hash interleaving the
        // panic this predicate replaced would have hit too. So this is
        // derived honestly from the entries actually written, the same way
        // `StreamingPmtilesWriter::entries_are_clustered` does, rather than
        // asserted.
        let clustered =
            offsets_are_clustered(entries.iter().map(|e| (e.offset, u64::from(e.length))));

        // A non-clustered archive is still valid, but it costs readers extra
        // seeks, so say so rather than shipping the regression silently --
        // the same signal `StreamingPmtilesWriter::write_archive` emits.
        if !clustered {
            log::warn!(
                "PMTiles writer: {} is not clustered -- a duplicate's bytes are \
                 carried by a tile that sorts after it, so a streaming reader must \
                 seek backwards; readers still serve every tile correctly",
                path.display()
            );
        }

        // Build header
        let header = Header {
            root_dir_offset,
            root_dir_length,
            json_metadata_offset: metadata_offset,
            json_metadata_length: metadata_length,
            // Always the section position, never 0 -- even with no leaves. See
            // the note on `leaf_dirs_offset` in `StreamingPmtilesWriter`.
            leaf_dirs_offset,
            leaf_dirs_length,
            tile_data_offset,
            tile_data_length,
            addressed_tiles_count: self.tiles.len() as u64,
            tile_entries_count: entries.len() as u64,
            tile_contents_count: unique_contents,
            clustered,
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
            .write_all(&leaves_bytes)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write leaf directories: {}", e)))?;
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
    /// Bytes written by archive assembly — every `checkpoint` plus the
    /// closing `finalize` (#459).
    ///
    /// This is the I/O a salvageable run costs *on top of* `bytes_written`,
    /// and the number the tail layout exists to shrink. Under the packed
    /// layout it grows as `checkpoints × tile_data`; under the tail layout it
    /// grows as `checkpoints × (16 KiB + metadata + leaves)`, independent of
    /// how much tile data is on disk.
    pub checkpoint_bytes_written: u64,
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
/// use tylertoo_core::pmtiles_writer::StreamingPmtilesWriter;
/// use tylertoo_core::compression::Compression;
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
    /// Minimum zoom the archive *declares* even when no tile exists there
    /// (#380). Widens `vector_layers[].minzoom` to `min(declared, seen)` —
    /// but never the PMTiles header (#529, #522): `go-pmtiles verify`
    /// requires the header's `min_zoom` to be the shallowest zoom that
    /// actually holds a tile, so the header always reports the observed
    /// value regardless of this field.
    declared_min_zoom: Option<u8>,
    /// Maximum zoom the archive *declares* even when no tile exists there —
    /// the mirror of `declared_min_zoom`, used by `tylertoo merge` to union
    /// the shards' declared ranges rather than the tiles' observed one. Like
    /// `declared_min_zoom`, this reaches `vector_layers[].maxzoom` only,
    /// never the header.
    declared_max_zoom: Option<u8>,
    /// Geographic bounds
    bounds: TileBounds,
    /// Layer name for metadata
    layer_name: String,
    /// Field metadata
    fields: HashMap<String, String>,
    /// Verbatim `vector_layers` array, when the archive holds more than the one
    /// layer `layer_name`/`fields` can describe (a merged band pyramid). When
    /// set it replaces the single-layer entry the writer would otherwise build.
    vector_layers_json: Option<String>,
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
    /// Debug-only ordering contract (#506): when set, every `add_tile*` call
    /// `debug_assert!`s its tile id is strictly greater than the previous
    /// one. A caller that has arranged to add tiles in ascending PMTiles
    /// tile-id (Hilbert) order — the export path, after this PR — opts in so
    /// an ordering regression fails fast in a debug build instead of quietly
    /// shipping a `clustered: false` archive. Never checked in release (the
    /// `clustered` header byte is derived honestly regardless, via
    /// [`Self::entries_are_clustered`], so a violation here is a perf/quality
    /// regression, not a correctness bug worth a release-mode cost).
    expect_clustered: bool,
    /// Whether every `add_tile*` so far arrived with a strictly greater tile
    /// id than the one before it. Tracked unconditionally (unlike
    /// `expect_clustered`, which only *asserts* it) because it makes the
    /// header's `clustered` byte an O(1) determination on the production
    /// path: see [`Self::write_archive`].
    adds_ascending: bool,
    /// Where the tile spool lives and how assembly reaches the output (#459).
    layout: SpoolLayout,
    /// How many tail-layout checkpoints have landed. `Drop` keeps
    /// `<output>.partial` only once there is at least one — before that the
    /// file is a bare prefix of zeros plus loose tile bytes, not an archive
    /// anyone could salvage.
    tail_checkpoints: u64,
    /// Whether a tail-layout writer has fallen back to the packed layout (the
    /// root directory outgrew the reserved prefix). Latched, because the
    /// fallback publishes to the output path and there is no salvage artifact
    /// beside it any more.
    tail_fell_back: bool,
    /// Whether `<output>.partial`'s on-disk prefix currently describes
    /// something that is *not* a valid archive (#528 review, F1).
    ///
    /// The tail layout's whole point is that the next tile bytes land on top
    /// of the last checkpoint's metadata and leaf directories. The moment
    /// that happens the header still sitting in the prefix points at sections
    /// that no longer exist, so a reader following it resolves into tile
    /// bytes and hands back garbage *without noticing*. Rather than let the
    /// salvage artifact degrade from "valid" to "silently wrong", the first
    /// append after a checkpoint zeroes the prefix's magic first
    /// ([`Self::invalidate_tail_prefix_before_append`]); this flag is what
    /// keeps that to one syscall per checkpoint interval instead of one per
    /// tile. Starts `true` because a freshly reserved prefix is all zeros,
    /// which is already no archive at all.
    tail_dirty: bool,
    /// Test hook: pretend the root directory overran the 16 KiB prefix, to
    /// exercise the packed-layout fallback. `make_root_leaves` will not
    /// produce such a root in practice, and the fallback must still be
    /// covered.
    force_tail_root_overflow: bool,
}

/// Where a [`StreamingPmtilesWriter`]'s tile spool lives, and therefore what
/// assembling an archive costs (#459).
#[derive(Debug, Clone)]
enum SpoolLayout {
    /// The spool is a scratch file in a temp directory, and the archive is
    /// assembled by writing header + directories + metadata and then copying
    /// the whole spool after them. Every checkpoint pays for the copy, so a
    /// run with *n* checkpoints writes the tile data *n+1* times.
    ///
    /// This is the original layout, kept verbatim for callers that hand the
    /// writer no output path up front (`tylertoo merge`, the pyramid builder,
    /// every existing test) and as the fallback if the root directory ever
    /// outgrows the tail layout's prefix.
    Spooled,
    /// The spool **is** the output's tile-data section: tiles append into
    /// `<output>.partial` from [`TAIL_LAYOUT_PREFIX_BYTES`] onward and are
    /// never copied again. Assembly rewrites the 16 KiB prefix (header + root
    /// directory) and re-appends metadata + leaf directories at the tail, so a
    /// checkpoint costs O(directory) rather than O(tile data).
    Tail {
        /// The output this writer was created for. Assembly refuses a
        /// different path: the prefix was reserved in *this* file, so writing
        /// elsewhere would mean copying after all.
        output_path: PathBuf,
    },
}

/// Everything an archive needs besides the tile bytes, built once per
/// assembly pass and placed by whichever layout is in use.
struct ArchiveSections {
    /// Compressed root directory (tile entries, or leaf pointers).
    root_bytes: Vec<u8>,
    /// Compressed leaf directories, concatenated; empty when none are needed.
    leaves_bytes: Vec<u8>,
    /// Compressed metadata JSON.
    metadata: Vec<u8>,
    /// Directory entries after run-length encoding — the header's
    /// `tile_entries_count`.
    dir_entry_count: u64,
    /// The honestly-derived `clustered` header flag.
    clustered: bool,
}

/// Which layout an assembly pass actually used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveWriteMode {
    /// Prefix + tail rewritten in place in `<output>.partial`.
    Tail,
    /// Packed archive built elsewhere and renamed over the output.
    FullCopy,
}

/// `<output>.partial` — the sibling file archive assembly writes before
/// publishing. Under the tail layout it is also the live spool and the
/// mid-run salvage artifact.
fn partial_path_for(output_path: &Path) -> PathBuf {
    let mut os = output_path.as_os_str().to_owned();
    os.push(".partial");
    PathBuf::from(os)
}

/// `<output>.partial.prev` — where a *previous* run's salvage archive is
/// stepped aside to when a new tail-layout writer opens the same output
/// (#528 review, F2). At most one generation is kept, and a successful
/// `finalize` removes it.
fn prev_partial_path_for(output_path: &Path) -> PathBuf {
    let mut os = output_path.as_os_str().to_owned();
    os.push(".partial.prev");
    PathBuf::from(os)
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
        let temp_path = temp_dir.join(format!("tylertoo-{}-{}-{:?}.tmp", timestamp, pid, tid));

        let file = File::create(&temp_path)?;
        let temp_file = BufWriter::with_capacity(64 * 1024, file); // 64KB buffer

        Ok(Self::from_spool(
            temp_file,
            temp_path,
            compression,
            SpoolLayout::Spooled,
        ))
    }

    /// Create a writer whose tile spool **is** the output archive (#459).
    ///
    /// Instead of spooling tiles to a scratch file and copying them into the
    /// output on every `checkpoint`, this opens `<output_path>.partial`,
    /// reserves the first [`TAIL_LAYOUT_PREFIX_BYTES`] for the header and root
    /// directory, and appends tile bytes straight after it. Assembly then
    /// rewrites only that prefix and re-appends metadata + leaf directories at
    /// the file's tail, so each checkpoint costs O(directory size) instead of
    /// O(tile data) — the difference between a planet-scale run re-copying
    /// tens of gigabytes per checkpoint and writing a few megabytes.
    ///
    /// Two consequences worth knowing:
    ///
    /// * **The spool lives next to the output, not in `TMPDIR`.** The target
    ///   filesystem needs room for the archive; a small `/tmp` no longer
    ///   matters.
    /// * **`<output>.partial` is the salvage artifact** ([`Self::salvage_path`]),
    ///   with exactly this guarantee: it is a complete, readable archive as
    ///   of the last checkpoint *if no tile has been appended since*, and
    ///   otherwise it is **detectably invalid** — never valid-looking and
    ///   wrong. The first append after a checkpoint overwrites the
    ///   metadata/leaf sections the prefix points at, so that append first
    ///   zeroes the prefix's magic
    ///   ([`Self::invalidate_tail_prefix_before_append`]) and every reader
    ///   rejects the file until the next checkpoint rebuilds the prefix and
    ///   tail from the (untouched) tile data. A crash mid-level therefore
    ///   salvages back to the last checkpoint, or to nothing — not to
    ///   plausible garbage.
    /// * **A previous run's `<output>.partial` is not destroyed.** If one is
    ///   found here it is moved to `<output>.partial.prev` before this run
    ///   reserves its prefix; a successful [`Self::finalize`] removes it.
    ///
    /// [`Self::checkpoint`] and [`Self::finalize`] must be called with the
    /// same `output_path`; a different one is an error, since the reservation
    /// was made in this file.
    ///
    /// Creating the writer touches the output's directory, so an unwritable or
    /// missing one fails *here* rather than after the run has done all its
    /// work. It does not create the directory: a typo in the path should stop
    /// the run, not invent a folder.
    pub fn with_tail_layout(output_path: &Path, compression: Compression) -> std::io::Result<Self> {
        let partial_path = partial_path_for(output_path);

        // A `<output>.partial` already sitting here is a previous run's
        // salvage archive — the thing #459 exists to leave behind. Creating
        // this writer truncates that file, so a scripted rerun would destroy
        // the crashed run's only recoverable output *before* producing
        // anything of its own (#528 review, F2). Step it aside instead. One
        // generation is kept; a successful `finalize` removes it. (The
        // alternative, opening the spool lazily, would mean `temp_file: None`
        // no longer meant "finalized" throughout the writer.)
        if std::fs::metadata(&partial_path).is_ok_and(|m| m.len() > 0) {
            let prev = prev_partial_path_for(output_path);
            match std::fs::rename(&partial_path, &prev) {
                Ok(()) => log::info!(
                    "[pmtiles] an earlier run left {}; moved to {} before starting",
                    partial_path.display(),
                    prev.display()
                ),
                Err(e) => log::warn!(
                    "[pmtiles] could not move the earlier {} aside ({e}); it will be overwritten",
                    partial_path.display()
                ),
            }
        }

        let mut file = File::create(&partial_path).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("cannot create {}: {e}", partial_path.display()),
            )
        })?;
        // Reserve the prefix eagerly, as zeros. Writing it now (rather than
        // seeking past it) means the first checkpoint's padding is already
        // zero-filled and the file never contains a sparse hole whose
        // contents depend on the filesystem.
        file.write_all(&vec![0u8; TAIL_LAYOUT_PREFIX_BYTES as usize])?;
        let temp_file = BufWriter::with_capacity(64 * 1024, file);

        Ok(Self::from_spool(
            temp_file,
            partial_path,
            compression,
            SpoolLayout::Tail {
                output_path: output_path.to_path_buf(),
            },
        ))
    }

    fn from_spool(
        temp_file: BufWriter<File>,
        temp_path: PathBuf,
        compression: Compression,
        layout: SpoolLayout,
    ) -> Self {
        Self {
            temp_file: Some(temp_file),
            temp_path,
            entries: Vec::new(),
            dedup_cache: HashMap::new(),
            current_offset: 0,
            min_zoom: 255,
            max_zoom: 0,
            declared_min_zoom: None,
            declared_max_zoom: None,
            bounds: TileBounds::empty(),
            layer_name: "layer".to_string(),
            fields: HashMap::new(),
            vector_layers_json: None,
            tile_compression: compression,
            internal_compression: compression,
            stats: StreamingWriteStats::default(),
            total_features: 0,
            finalized: false,
            expect_clustered: false,
            adds_ascending: true,
            layout,
            tail_checkpoints: 0,
            tail_fell_back: false,
            tail_dirty: true,
            force_tail_root_overflow: false,
        }
    }

    /// Where a mid-run salvage archive can be found, for a writer created with
    /// [`Self::with_tail_layout`]: `<output>.partial`, valid as of the last
    /// [`Self::checkpoint`] — provided no tile has been added since that
    /// checkpoint, in which case the file is instead *detectably* invalid.
    /// See [`Self::with_tail_layout`] for why the guarantee is stated that
    /// way.
    ///
    /// `None` for the spooled layout — and for a tail-layout writer that has
    /// fallen back to it — because those checkpoints publish to the output
    /// path itself.
    pub fn salvage_path(&self) -> Option<&Path> {
        match self.layout {
            SpoolLayout::Tail { .. } if !self.tail_fell_back => Some(&self.temp_path),
            _ => None,
        }
    }

    /// Test hook: pretend the root directory overran the reserved prefix, so
    /// the packed-layout fallback is reachable. `make_root_leaves` sizes the
    /// root against `16384 - 127` and will not produce an oversized one, which
    /// is exactly why the fallback needs a hook to cover.
    #[cfg(test)]
    fn force_tail_root_overflow(&mut self) {
        self.force_tail_root_overflow = true;
    }

    /// Opt into the debug-only ascending-tile-id assertion (#506): see the
    /// `expect_clustered` field doc for what it checks and why it is
    /// debug-only.
    pub fn set_expect_clustered(&mut self, expect: bool) {
        self.expect_clustered = expect;
    }

    /// Record that `id` is about to be added: maintain `adds_ascending`, and
    /// `debug_assert!` that `id` continues the ascending run this writer was
    /// told to expect (when `expect_clustered` is set), given the most
    /// recently added entry (if any).
    ///
    /// `adds_ascending` is maintained whether or not the caller opted into
    /// the assertion, because [`Self::write_archive`] uses it to skip the
    /// O(unique-offsets) clustered predicate entirely.
    #[inline]
    fn note_add(&mut self, id: u64) {
        let Some(last_id) = self.entries.last().map(|e| e.tile_id) else {
            return;
        };
        if id > last_id {
            return;
        }
        self.adds_ascending = false;
        debug_assert!(
            !self.expect_clustered,
            "expect_clustered: tile id {id} did not continue the ascending run \
             (last added was {last_id}); the caller opted into tile-id-ordered adds \
             but did not deliver them"
        );
    }

    /// Get the path to the temp file (for testing).
    pub fn temp_path(&self) -> &Path {
        &self.temp_path
    }

    /// Set the layer name for metadata.
    pub fn set_layer_name(&mut self, name: &str) {
        self.layer_name = name.to_string();
    }

    /// Declare a minimum zoom for the archive regardless of which zooms end
    /// up holding tiles (#380). The layer's `minzoom` in `vector_layers`
    /// becomes `min(declared, coarsest tile written)`: an empty zoom in a
    /// PMTiles archive is just an absent tile, so declaring z0 over a
    /// pyramid whose coarsest level generalized to nothing is honest, and
    /// clients that build TileJSON from `vector_layers` see the requested
    /// range.
    ///
    /// This does NOT widen the PMTiles header's `min_zoom` (#529, #522):
    /// `go-pmtiles verify` requires the header to equal the shallowest zoom
    /// that actually holds a tile ("header MinZoom does not match min tile
    /// z"), and a widened header failed verification on every archive that
    /// used this. The header always reports what the directory actually
    /// addresses; only the metadata declares the wider, requested range. A
    /// declared value finer than a written tile is ignored either way — the
    /// declaration can only widen `vector_layers`, never narrow it over real
    /// tiles.
    pub fn set_declared_min_zoom(&mut self, zoom: u8) {
        self.declared_min_zoom = Some(zoom);
    }

    /// Declare a maximum zoom for the archive regardless of which zooms end
    /// up holding tiles — the mirror of [`Self::set_declared_min_zoom`], for
    /// the same reason at the other end of the range. Like the minimum, this
    /// widens `vector_layers[].maxzoom` only, never the header's `max_zoom`
    /// (#529, #522).
    ///
    /// `tylertoo merge` uses it to fold a shard's declared range into the
    /// merged `vector_layers`: a shard covering a sliver of the world
    /// legitimately has no tile at its own deepest zoom, and deriving the
    /// merged layer's maximum from the deepest tile actually copied would
    /// quietly narrow the range every such shard set declares. A declaration
    /// coarser than a written tile is ignored.
    pub fn set_declared_max_zoom(&mut self, zoom: u8) {
        self.declared_max_zoom = Some(zoom);
    }

    /// The header's minimum zoom: the coarsest tile actually written, never
    /// widened by a declared minimum (#529, #522). `go-pmtiles verify`
    /// rejects a header whose `min_zoom` does not match the shallowest tile
    /// the directory addresses, so this — unlike [`Self::layer_min_zoom`] —
    /// ignores `declared_min_zoom` entirely. An archive with no tiles is
    /// z0..z0.
    fn actual_min_zoom(&self) -> u8 {
        if self.entries.is_empty() {
            0
        } else {
            self.min_zoom
        }
    }

    /// The header's maximum zoom: the deepest tile actually written, never
    /// widened by a declared maximum. See [`Self::actual_min_zoom`].
    fn actual_max_zoom(&self) -> u8 {
        if self.entries.is_empty() {
            0
        } else {
            self.max_zoom
        }
    }

    /// `vector_layers[].minzoom` (and the single-layer metadata fallback's
    /// minzoom): the coarsest tile written, widened by any declared minimum.
    /// Unlike [`Self::actual_min_zoom`], this is advertised metadata a
    /// renderer reads to decide what range to request — not a claim about
    /// which tiles physically exist in the directory — so #380 lets it be
    /// wider than the header. An archive with no tiles is z0..z0 whatever was
    /// declared — its max zoom collapses to 0, and a declared minimum above
    /// that would invert the range.
    fn layer_min_zoom(&self) -> u8 {
        if self.entries.is_empty() {
            return 0;
        }
        match self.declared_min_zoom {
            Some(d) => self.min_zoom.min(d),
            None => self.min_zoom,
        }
    }

    /// `vector_layers[].maxzoom`: the deepest tile written, widened by any
    /// declared maximum. See [`Self::layer_min_zoom`].
    fn layer_max_zoom(&self) -> u8 {
        if self.entries.is_empty() {
            return 0;
        }
        match self.declared_max_zoom {
            Some(d) => self.max_zoom.max(d),
            None => self.max_zoom,
        }
    }

    /// Set field metadata.
    pub fn set_fields(&mut self, fields: HashMap<String, String>) {
        self.fields = fields;
    }

    /// Replace the whole `vector_layers` array with a verbatim JSON array.
    ///
    /// A single-layer archive is described by `layer_name` + `fields`; a merged
    /// band pyramid has several layers, each with its own zoom range and field
    /// set, which that pair cannot express. Honoured by the one metadata
    /// assembler (`build_metadata_json`) both `checkpoint` and `finalize` use,
    /// so a checkpointed archive and the final one cannot disagree.
    pub fn set_vector_layers_json(&mut self, json: String) {
        self.vector_layers_json = Some(json);
    }

    /// Set geographic bounds.
    ///
    /// Latitude values are clamped to the Web Mercator bound
    /// (`±`[`crate::world_coord::MAX_LATITUDE`]).
    pub fn set_bounds(&mut self, bounds: &TileBounds) {
        self.bounds = TileBounds::new(
            bounds.lng_min,
            bounds.lat_min.clamp(-MAX_LATITUDE, MAX_LATITUDE),
            bounds.lng_max,
            bounds.lat_max.clamp(-MAX_LATITUDE, MAX_LATITUDE),
        );
    }

    /// Get current statistics.
    pub fn stats(&self) -> &StreamingWriteStats {
        &self.stats
    }

    /// Make `<output>.partial` **detectably** invalid before the first tile
    /// byte that will overwrite a checkpoint's tail (#528 review, F1).
    ///
    /// Under the tail layout, metadata and leaf directories live immediately
    /// after the tile data, and the next append lands on top of them. Until
    /// that append the file is a valid archive as of the last checkpoint —
    /// the property #459 sells. The instant it happens, the header still in
    /// the prefix describes sections that are now tile bytes, and a reader
    /// walking its leaf pointers resolves into that garbage and returns wrong
    /// tiles rather than an error. "Valid, one level stale" silently becoming
    /// "readable and wrong" is the worst of the three outcomes, so we trade
    /// it for the third: zero the prefix's magic (and version) so every
    /// reader — ours, go-pmtiles, any salvage tool — rejects the file cleanly
    /// until the next checkpoint rewrites the whole prefix anyway.
    ///
    /// Ordering is the point: this write must land *before* the tile bytes,
    /// never after, or a crash in between is exactly the case it exists to
    /// prevent. It goes through its own handle, so it reaches the file while
    /// the tile bytes are still in the spool's `BufWriter`. It is not
    /// `fsync`ed: this guards process death (OOM kill, SIGINT, panic), where
    /// the page cache survives and write order is all that matters, and
    /// syncing would flush every dirty tile page in a multi-gigabyte file on
    /// what is meant to be a cheap path.
    ///
    /// `tail_dirty` keeps this to one `open`+`write` per checkpoint interval
    /// — once per zoom level in practice — rather than one per tile.
    fn invalidate_tail_prefix_before_append(&mut self) -> std::io::Result<()> {
        if self.tail_dirty {
            return Ok(());
        }
        if let SpoolLayout::Tail { .. } = self.layout {
            // Opening for write starts at offset 0; magic is bytes 0..7 and
            // the version byte is 7.
            File::options()
                .write(true)
                .open(&self.temp_path)?
                .write_all(&[0u8; 8])?;
        }
        self.tail_dirty = true;
        Ok(())
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
        let id = checked_tile_id(z, x, y)?;
        self.note_add(id);

        if self.temp_file.is_none() {
            return Err(std::io::Error::other("Writer already finalized"));
        }

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

        // New unique tile - compress and write to temp file. A dedup hit
        // above writes nothing, so it leaves any checkpointed archive intact;
        // only an append can clobber the tail, and only that path invalidates.
        let compressed = compression::compress(data, self.tile_compression)?;
        let compressed_len = compressed.len() as u32;

        self.invalidate_tail_prefix_before_append()?;
        self.temp_file
            .as_mut()
            .expect("checked above")
            .write_all(&compressed)?;

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

    /// Add a tile whose bytes are **already compressed** with this writer's
    /// `tile_compression`.
    ///
    /// This is the parallel-friendly counterpart of [`Self::add_tile_with_count`]:
    /// the caller compresses tile bytes off-thread (e.g. inside a Rayon
    /// `par_iter`) and hands the finished bytes here so the serial ordering loop
    /// never runs gzip. To keep deduplication byte-for-byte identical to the
    /// serial path, `hash` MUST be the [`TileHasher::hash`] of the tile's
    /// **uncompressed** MVT bytes (never the compressed bytes), and `raw_len` the
    /// uncompressed length (used only for the dedup byte-savings stat). Because
    /// compression is deterministic, an identical uncompressed hash implies
    /// identical compressed bytes, so the archive is bit-identical to compressing
    /// serially.
    #[allow(clippy::too_many_arguments)]
    pub fn add_tile_precompressed(
        &mut self,
        z: u8,
        x: u32,
        y: u32,
        hash: u64,
        compressed: &[u8],
        raw_len: usize,
        feature_count: usize,
    ) -> std::io::Result<()> {
        let id = checked_tile_id(z, x, y)?;
        self.note_add(id);

        if self.temp_file.is_none() {
            return Err(std::io::Error::other("Writer already finalized"));
        }

        self.stats.total_tiles += 1;
        self.total_features += feature_count as u64;

        // Track zoom range
        self.min_zoom = self.min_zoom.min(z);
        self.max_zoom = self.max_zoom.max(z);

        // Dedup on the uncompressed hash (same key as add_tile_with_count).
        if let Some((offset, length)) = self.dedup_cache.get(&hash) {
            self.entries.push(StreamingDirEntry {
                tile_id: id,
                offset: *offset,
                length: *length,
            });
            self.stats.bytes_saved_dedup += raw_len as u64;
            return Ok(());
        }

        // New unique tile - write the pre-compressed bytes verbatim.
        let compressed_len = compressed.len() as u32;
        self.invalidate_tail_prefix_before_append()?;
        self.temp_file
            .as_mut()
            .expect("checked above")
            .write_all(compressed)?;

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
        // Assemble the complete archive (byte-identical to the last checkpoint,
        // if any). This flushes the temp buffer in place but leaves the handle
        // open so we can close it deterministically below.
        let mode = self.write_archive(output_path)?;

        // Close the spool handle before touching the file by path.
        drop(self.temp_file.take());

        match mode {
            // Tail layout: the spool already *is* the finished archive, sitting
            // at `<output>.partial`. Publishing is one rename — no copy, and no
            // window in which the output exists but is incomplete.
            ArchiveWriteMode::Tail => {
                // A failed rename leaves a *complete, correct* archive at
                // `self.temp_path`. Say so in the error: the only other
                // mention of the path is a `log::info!` in `Drop`, which is a
                // no-op for Python callers and embedders with no log backend
                // installed (#528 review, F3).
                std::fs::rename(&self.temp_path, output_path).map_err(|e| {
                    Error::PMTilesWrite(format!(
                        "Failed to publish archive: {}; the complete archive was written to {} \
                         -- rename it manually",
                        e,
                        self.temp_path.display()
                    ))
                })?;
                // The output is now complete, so a previous run's salvage
                // copy of the same output is dead weight (#528 review, F2).
                let _ = std::fs::remove_file(prev_partial_path_for(output_path));
            }
            // Packed layout: the archive was assembled elsewhere and already
            // renamed over the output; the spool is now dead weight.
            ArchiveWriteMode::FullCopy => {
                let _ = std::fs::remove_file(&self.temp_path);
            }
        }

        // Mark as finalized so Drop doesn't try to clean up again
        self.finalized = true;

        Ok(self.stats.clone())
    }

    /// Write a valid, self-contained PMTiles archive containing every tile
    /// added so far, *without* consuming the writer or closing the temp file
    /// (Issue #229 — salvageable output).
    ///
    /// The export loop calls this after each finished level so an interrupted
    /// run still yields a valid archive capped at the last completed zoom
    /// instead of losing hours of compute. `finalize` routes through the same
    /// assembler, so the final archive is byte-identical whether or not any
    /// intermediate checkpoints were taken.
    ///
    /// Under the spooled layout the archive is written to a sibling
    /// `<output>.partial` file and then atomically renamed over `output_path`,
    /// so a kill mid-write never corrupts a previously-checkpointed archive.
    /// Under the tail layout (#459) `<output>.partial` *is* the live archive
    /// and is left in place — see [`Self::with_tail_layout`] and
    /// [`Self::salvage_path`].
    pub fn checkpoint(&mut self, output_path: &Path) -> Result<()> {
        self.write_archive(output_path)?;
        Ok(())
    }

    /// Shared archive assembler backing both [`checkpoint`](Self::checkpoint)
    /// and [`finalize`](Self::finalize). Flushes the temp buffer in place (the
    /// handle stays open so the writer remains usable), builds the header,
    /// directories and metadata, then hands them to whichever layout this
    /// writer uses. Returns the layout actually used, which is what tells
    /// `finalize` whether publishing is a rename or already done.
    fn write_archive(&mut self, output_path: &Path) -> Result<ArchiveWriteMode> {
        let sections = self.build_sections(output_path)?;

        // The tail layout is legal only while header + root fit the reserved
        // prefix. `make_root_leaves` targets exactly that budget
        // (`MAX_ROOT_DIR_BYTES == 16384 - 127`) and spills into leaves to stay
        // under it, so this holds by construction. If it ever stops holding,
        // writing the root anyway would run it into the tile data: fall back to
        // the packed layout — a slow checkpoint beats a corrupt archive.
        let root_fits = !self.force_tail_root_overflow
            && (HEADER_BYTES as u64 + sections.root_bytes.len() as u64) <= TAIL_LAYOUT_PREFIX_BYTES;

        match &self.layout {
            SpoolLayout::Tail {
                output_path: reserved,
            } => {
                if reserved != output_path {
                    return Err(Error::PMTilesWrite(format!(
                        "tail-layout writer reserved its prefix in {} but was asked to write {}; \
                         a tail-layout writer can only publish the output it was created for",
                        partial_path_for(reserved).display(),
                        output_path.display()
                    )));
                }
                if !root_fits {
                    log::error!(
                        "PMTiles writer: root directory ({} bytes) does not fit the \
                         {TAIL_LAYOUT_PREFIX_BYTES}-byte tail-layout prefix; falling back to \
                         the packed layout, which re-copies the tile data on every checkpoint",
                        sections.root_bytes.len()
                    );
                    self.tail_fell_back = true;
                    self.write_packed_archive(&sections, output_path)?;
                    return Ok(ArchiveWriteMode::FullCopy);
                }
                self.write_tail_archive(&sections)?;
                self.tail_checkpoints += 1;
                Ok(ArchiveWriteMode::Tail)
            }
            SpoolLayout::Spooled => {
                self.write_packed_archive(&sections, output_path)?;
                Ok(ArchiveWriteMode::FullCopy)
            }
        }
    }

    /// Flush the spool and build everything an archive needs besides the tile
    /// bytes: the root and leaf directories, the metadata JSON, and the
    /// honestly-derived `clustered` flag. Layout-independent, so both layouts
    /// write the same directories over the same tile bytes.
    fn build_sections(&mut self, output_path: &Path) -> Result<ArchiveSections> {
        // Flush buffered tile bytes to the spool on disk without consuming
        // the handle — the writer must stay usable after a checkpoint.
        match self.temp_file.as_mut() {
            Some(tf) => tf
                .flush()
                .map_err(|e| Error::PMTilesWrite(format!("Failed to flush temp file: {}", e)))?,
            None => return Err(Error::PMTilesWrite("Writer already finalized".to_string())),
        }

        // Sort entries by tile_id for clustered mode. Tile ids are unique, so
        // this is deterministic and idempotent — re-sorting between checkpoints
        // and later adds yields the same final ordering as sorting once.
        self.entries.sort_by_key(|e| e.tile_id);

        // Whether the directory this run is about to write is *actually*
        // clustered — sorting by tile_id (above) does not imply it. Callers
        // add tiles in whatever order they discover them (ascending tile id
        // for export, since #506; tile-id order for a pyramid merge), and the
        // offsets in `self.entries` reflect add order, not tile-id order.
        // Deriving the header flag from those offsets, after the sort, means
        // the flag can never claim more than the bytes on disk actually
        // deliver. Layout-independent: the tail layout shifts where tile
        // data *starts*, not the relative offsets these entries hold (#459).
        //
        // Fast path: ascending adds imply clustered by construction, so the
        // production case (#506 export, #510 merge) never pays for the
        // predicate. Each add either appends fresh bytes at `current_offset`
        // -- which is exactly the running end of the tile data -- or is a
        // dedup back-reference to the *exact* offset and length some earlier
        // add recorded in `dedup_cache`. Both are what
        // `offsets_are_clustered` accepts, and ascending adds mean the
        // tile_id sort above left the entries in add order, so walking the
        // directory walks the appends in the order they happened. The
        // predicate stays as the honest fallback for out-of-order callers
        // (and as what `verify_clustered` re-derives from disk), but it
        // allocates a hash map proportional to the unique-offset count --
        // measured ~2.4 GB steady at planet scale, at peak RSS -- so it must
        // not run when the answer is already known.
        let clustered = self.adds_ascending || self.entries_are_clustered();

        // `expect_clustered`'s `debug_assert!` (in `note_add`)
        // catches an ordering regression while adding tiles, but only in a
        // debug build — a release build silently ships a `clustered: false`
        // archive with no signal at all (#506 review, F6). This is the
        // release-mode fallback: the caller promised ascending adds and the
        // derived flag says the promise was not kept, so say so at `warn`
        // rather than staying silent. Not an error — the archive is still
        // valid, just not what the caller asked for.
        if self.expect_clustered && !clustered {
            log::warn!(
                "PMTiles writer: caller opted into ascending-tile-id adds \
                 (expect_clustered) for {}, but the written archive is not \
                 actually clustered -- an add arrived out of tile-id order \
                 and this build has debug assertions disabled, so the \
                 regression only surfaces here",
                output_path.display()
            );
        }

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

        Ok(ArchiveSections {
            root_bytes: dir_layout.root_bytes,
            leaves_bytes: dir_layout.leaves_bytes,
            metadata: compressed_metadata,
            dir_entry_count: dir_entries.len() as u64,
            clustered,
        })
    }

    /// Stamp the header for a given section placement. The caller decides
    /// where the sections sit; everything else — counts, zoom range, bounds,
    /// compression — is the writer's state and is identical across layouts.
    fn build_header(
        &self,
        sections: &ArchiveSections,
        metadata_offset: u64,
        leaf_dirs_offset: u64,
        tile_data_offset: u64,
    ) -> Header {
        Header {
            root_dir_offset: HEADER_BYTES as u64,
            root_dir_length: sections.root_bytes.len() as u64,
            json_metadata_offset: metadata_offset,
            json_metadata_length: sections.metadata.len() as u64,
            // Always the section position, never 0.
            //
            // go-pmtiles REJECTS an archive whose leaf-directory offset is 0:
            //
            //     Failed to verify archive, Leaf directories offset=0 must not be 0
            //
            // Pointing it at the (empty) leaf section instead verifies clean.
            // Zeroing it affected every small archive written through this
            // path, which is the production one.
            leaf_dirs_offset,
            leaf_dirs_length: sections.leaves_bytes.len() as u64,
            tile_data_offset,
            tile_data_length: self.current_offset,
            addressed_tiles_count: self.stats.total_tiles,
            tile_entries_count: sections.dir_entry_count,
            tile_contents_count: self.stats.unique_tiles,
            clustered: sections.clustered,
            internal_compression: self.internal_compression,
            tile_compression: self.tile_compression,
            tile_type: TileType::Mvt,
            min_zoom: self.actual_min_zoom(),
            max_zoom: self.actual_max_zoom(),
            min_lon: self.bounds.lng_min,
            min_lat: self.bounds.lat_min,
            max_lon: self.bounds.lng_max,
            max_lat: self.bounds.lat_max,
            center_zoom: if self.entries.is_empty() {
                0
            } else {
                (self.actual_min_zoom() + self.actual_max_zoom()) / 2
            },
            center_lon: (self.bounds.lng_min + self.bounds.lng_max) / 2.0,
            center_lat: (self.bounds.lat_min + self.bounds.lat_max) / 2.0,
        }
    }

    /// Tail-directory assembly (#459): rewrite only the prefix and the tail.
    ///
    /// ```text
    /// [0]      header (127 B)
    /// [127]    root directory
    /// [..]     zero padding            <- the only slack go-pmtiles tolerates
    /// [16384]  tile data               <- written once, never copied
    /// [..]     json metadata           }  rebuilt each checkpoint;
    /// [..]     leaf directories        }  overwritten by the next tiles
    /// ```
    ///
    /// Order matters for crash-consistency: the tail is written and flushed
    /// *before* the prefix that points at it, and the prefix lives entirely
    /// below offset 16384, so a torn prefix write cannot touch a tile byte.
    /// The next checkpoint rebuilds both from the tile data, which is the only
    /// section this function never writes.
    fn write_tail_archive(&mut self, sections: &ArchiveSections) -> Result<()> {
        let data_end = TAIL_LAYOUT_PREFIX_BYTES + self.current_offset;
        let metadata_offset = data_end;
        let leaf_dirs_offset = metadata_offset + sections.metadata.len() as u64;
        let header = self.build_header(
            sections,
            metadata_offset,
            leaf_dirs_offset,
            TAIL_LAYOUT_PREFIX_BYTES,
        );

        // A second handle: the spool's own `BufWriter` keeps its cursor at the
        // tile-data end so the next `add_tile` appends straight over the tail
        // we are about to write, which is exactly what makes the tail cheap.
        let mut file = File::options()
            .write(true)
            .open(&self.temp_path)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to reopen partial: {}", e)))?;

        // Drop the previous checkpoint's tail before appending this one, so the
        // file length is always exactly `16384 + tile + metadata + leaves` —
        // go-pmtiles' padded-length rule admits no other slack.
        file.set_len(data_end)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to truncate tail: {}", e)))?;
        file.seek(SeekFrom::Start(data_end))
            .map_err(|e| Error::PMTilesWrite(format!("Failed to seek to tail: {}", e)))?;
        file.write_all(&sections.metadata)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write metadata: {}", e)))?;
        file.write_all(&sections.leaves_bytes)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write leaf directories: {}", e)))?;
        file.sync_data()
            .map_err(|e| Error::PMTilesWrite(format!("Failed to sync tail: {}", e)))?;

        // Publish by overwriting the prefix last, in one write: until the
        // header lands, the archive still describes the previous checkpoint.
        let mut prefix = vec![0u8; TAIL_LAYOUT_PREFIX_BYTES as usize];
        prefix[..HEADER_BYTES].copy_from_slice(&header.to_bytes());
        prefix[HEADER_BYTES..HEADER_BYTES + sections.root_bytes.len()]
            .copy_from_slice(&sections.root_bytes);
        file.seek(SeekFrom::Start(0))
            .map_err(|e| Error::PMTilesWrite(format!("Failed to seek to prefix: {}", e)))?;
        file.write_all(&prefix)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write prefix: {}", e)))?;
        file.sync_all()
            .map_err(|e| Error::PMTilesWrite(format!("Failed to sync archive: {}", e)))?;

        // The prefix on disk now describes this file exactly, so the file is
        // a valid archive again until the next append lands on the tail.
        self.tail_dirty = false;

        self.stats.checkpoint_bytes_written += prefix.len() as u64
            + sections.metadata.len() as u64
            + sections.leaves_bytes.len() as u64;

        Ok(())
    }

    /// Packed assembly: `Header | Root | Metadata | Leaves | Tile Data`, built
    /// in a sibling file and atomically renamed over `output_path`. Every call
    /// copies the whole tile spool, which is what #459 exists to avoid — this
    /// path remains for the spooled layout (callers that never hand the writer
    /// an output path up front) and as the tail layout's fallback.
    fn write_packed_archive(
        &mut self,
        sections: &ArchiveSections,
        output_path: &Path,
    ) -> Result<()> {
        let metadata_offset = HEADER_BYTES as u64 + sections.root_bytes.len() as u64;
        let leaf_dirs_offset = metadata_offset + sections.metadata.len() as u64;
        let tile_data_offset = leaf_dirs_offset + sections.leaves_bytes.len() as u64;
        let header = self.build_header(
            sections,
            metadata_offset,
            leaf_dirs_offset,
            tile_data_offset,
        );

        let partial_path = match &self.layout {
            SpoolLayout::Spooled => partial_path_for(output_path),
            // The tail layout's spool IS `<output>.partial`; assembling into it
            // would mean writing over the file we are reading the tile data
            // from. Use a distinct sibling for the (never-in-practice) fallback.
            SpoolLayout::Tail { .. } => {
                let mut os = output_path.as_os_str().to_owned();
                os.push(".partial-packed");
                PathBuf::from(os)
            }
        };

        let output_file = File::create(&partial_path)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to create output file: {}", e)))?;
        let mut writer = BufWriter::new(output_file);

        writer
            .write_all(&header.to_bytes())
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write header: {}", e)))?;
        writer
            .write_all(&sections.root_bytes)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write root directory: {}", e)))?;
        writer
            .write_all(&sections.metadata)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to write metadata: {}", e)))?;
        if !sections.leaves_bytes.is_empty() {
            writer.write_all(&sections.leaves_bytes).map_err(|e| {
                Error::PMTilesWrite(format!("Failed to write leaf directories: {}", e))
            })?;
        }

        // Copy tile data from the spool. Under the tail layout the tile bytes
        // start after the reserved prefix rather than at byte 0.
        let mut temp_reader = File::open(&self.temp_path)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to reopen temp file: {}", e)))?;
        let data_start = self.spool_data_start();
        if data_start > 0 {
            temp_reader
                .seek(SeekFrom::Start(data_start))
                .map_err(|e| Error::PMTilesWrite(format!("Failed to seek temp file: {}", e)))?;
        }
        std::io::copy(&mut temp_reader.take(self.current_offset), &mut writer)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to copy tile data: {}", e)))?;

        writer
            .flush()
            .map_err(|e| Error::PMTilesWrite(format!("Failed to flush output: {}", e)))?;
        // Close the output handle before renaming so the bytes are fully on disk.
        drop(writer);

        // Atomically publish the assembled archive.
        std::fs::rename(&partial_path, output_path)
            .map_err(|e| Error::PMTilesWrite(format!("Failed to publish archive: {}", e)))?;

        self.stats.checkpoint_bytes_written += tile_data_offset + self.current_offset;

        Ok(())
    }

    /// Byte offset at which tile data begins inside this writer's spool file.
    fn spool_data_start(&self) -> u64 {
        match self.layout {
            SpoolLayout::Tail { .. } => TAIL_LAYOUT_PREFIX_BYTES,
            SpoolLayout::Spooled => 0,
        }
    }

    /// Whether `self.entries`, already sorted by tile_id, is genuinely
    /// clustered: see [`offsets_are_clustered`] for the predicate. Must be
    /// called after the tile_id sort in [`Self::write_archive`] — before it,
    /// `self.entries` is in add order, which is not necessarily tile-id order
    /// for every caller (a pyramid merge, say, may still add out of order),
    /// and the check would be meaningless. Export itself now adds in
    /// ascending tile-id order already (#506), but this function does not —
    /// and should not — assume that of every caller; it re-derives the truth
    /// from the sorted offsets regardless of how `self.entries` got here.
    fn entries_are_clustered(&self) -> bool {
        offsets_are_clustered(self.entries.iter().map(|e| (e.offset, u64::from(e.length))))
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
        let min_z = self.layer_min_zoom();
        let max_z = self.layer_max_zoom();

        let tilestats_json = self.build_tilestats_json();
        let vector_layers = match &self.vector_layers_json {
            Some(json) => json.clone(),
            None => format!(
                r#"[{{"id":"{}","minzoom":{},"maxzoom":{},"fields":{}}}]"#,
                self.layer_name,
                min_z,
                max_z,
                self.build_fields_json()
            ),
        };

        format!(
            r#"{{"vector_layers":{},{}"format":"pbf","generator":"tylertoo"}}"#,
            vector_layers, tilestats_json
        )
    }

    fn build_fields_json(&self) -> String {
        fields_json(&self.fields)
    }

    fn build_tilestats_json(&self) -> String {
        tilestats_json(&self.layer_name, self.total_features, self.fields.len())
    }
}

impl Drop for StreamingPmtilesWriter {
    fn drop(&mut self) {
        if self.finalized {
            return;
        }
        match self.layout {
            // A scratch file in TMPDIR: nothing to salvage, always remove it.
            SpoolLayout::Spooled => {
                let _ = std::fs::remove_file(&self.temp_path);
            }
            // `<output>.partial` is the salvage artifact (#459) — but only once
            // a checkpoint has made it an archive. Before that it is a prefix
            // of zeros plus loose tile bytes, which would just be litter beside
            // the user's output.
            SpoolLayout::Tail { .. } => {
                if self.tail_checkpoints == 0 {
                    let _ = std::fs::remove_file(&self.temp_path);
                } else if self.tail_dirty {
                    log::info!(
                        "[pmtiles] run ended without finalize, mid-level; {} holds the tile \
                         data but its header was invalidated by the tiles added after the \
                         last checkpoint, so it is not a readable archive",
                        self.temp_path.display()
                    );
                } else {
                    log::info!(
                        "[pmtiles] run ended without finalize; salvageable archive left at {}",
                        self.temp_path.display()
                    );
                }
            }
        }
    }
}

// ============================================================================
// Tests (TDD)
// ============================================================================

#[cfg(test)]
mod tests {

    /// An archive whose directory outgrows the spec's 16 KiB root budget must
    /// spill into leaf directories. Writing one oversized root instead makes a
    /// file readers reject: go-pmtiles reads the first 16 KiB of root and
    /// panics slicing past it ("slice bounds out of range [:48771] with
    /// capacity 16384" on a 23,559-tile pyramid). Only tiny archives were ever
    /// written through this path, so it stayed hidden.
    #[test]
    fn large_archive_spills_into_leaf_directories() {
        let mut writer = PmtilesWriter::new();
        // Matching the scale that exposed this: tens of thousands of tiles, so
        // the entry offsets and lengths do not delta-encode down to nothing.
        // z8 is 256x256, comfortably more than 24,000 addresses.
        // Gapped addresses, all distinct: a real pyramid's tiles are sparse, so
        // the tile-id deltas are large and the directory does not compress to
        // nothing the way a solid block of sequential ids would.
        // Irregular lengths from a tiny LCG, so the entry offsets are the
        // uneven numbers a real archive has. A regular pattern gzips down to
        // nothing and never reaches the budget this test is about.
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        for i in 0..40_000u32 {
            let (x, y) = ((i % 200) * 20, (i / 200) * 20);
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            // Pre-compressed, the path a band merge uses: it also skips 40k
            // gzip calls that would make this test take half a minute.
            let len = 64 + (rng >> 33) as usize % 4096;
            writer
                .add_tile_compressed(14, x, y, vec![(i % 251) as u8; len])
                .unwrap();
        }
        let tmp = tempfile::NamedTempFile::new().unwrap();
        writer.write_to_file(tmp.path()).unwrap();

        let bytes = std::fs::read(tmp.path()).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();
        assert!(
            header.root_dir_length <= 16384,
            "root directory must fit the spec budget, got {}",
            header.root_dir_length
        );
        assert!(
            header.leaf_dirs_length > 0,
            "an archive too big for one root must have leaf directories"
        );
        // Sections must not overlap: leaves sit between metadata and tile data.
        assert_eq!(
            header.leaf_dirs_offset,
            header.json_metadata_offset + header.json_metadata_length
        );
        assert_eq!(
            header.tile_data_offset,
            header.leaf_dirs_offset + header.leaf_dirs_length
        );

        // Structure is not the point -- READABILITY is. The bug this fixes
        // produced a file whose header looked perfectly reasonable, which is
        // exactly why it survived: a wrong leaf-offset base would satisfy every
        // assertion above and still hand a reader garbage. So walk the
        // directories the way a reader does and check the bytes come back.
        let read_dir = |raw: &[u8]| -> Vec<DirEntry> {
            let plain = compression::decompress_capped(
                raw,
                header.internal_compression,
                compression::MAX_INTERNAL_BYTES,
            )
            .unwrap();
            decode_directory(&plain).expect("directory must decode")
        };
        let root = read_dir(
            &bytes[header.root_dir_offset as usize
                ..(header.root_dir_offset + header.root_dir_length) as usize],
        );
        assert!(
            root.iter().any(|e| e.run_length == 0),
            "a spilled archive's root must contain at least one leaf pointer"
        );

        let mut found: HashMap<u64, Vec<u8>> = HashMap::new();
        for e in &root {
            let leaves = if e.run_length == 0 {
                // A leaf pointer: `offset` is relative to leaf_dirs_offset.
                let start = (header.leaf_dirs_offset + e.offset) as usize;
                read_dir(&bytes[start..start + e.length as usize])
            } else {
                vec![e.clone()]
            };
            for le in leaves {
                let start = (header.tile_data_offset + le.offset) as usize;
                found.insert(
                    le.tile_id,
                    bytes[start..start + le.length as usize].to_vec(),
                );
            }
        }
        assert_eq!(found.len(), 40_000, "every tile must be addressable");

        // Spot-check content at both ends and the middle. The payload is a run
        // of `(i % 251)` bytes, so a mis-resolved pointer gives a wrong byte.
        for i in [0u32, 1, 19_899, 39_998, 39_999] {
            let (x, y) = ((i % 200) * 20, (i / 200) * 20);
            let data = found
                .get(&tile_id(14, x, y))
                .unwrap_or_else(|| panic!("tile {i} (z14/{x}/{y}) not found"));
            let want = (i % 251) as u8;
            assert!(
                data.iter().all(|&b| b == want),
                "tile {i} (z14/{x}/{y}) resolved to the wrong bytes: \
                 expected all {want}, got {:?}..",
                &data[..data.len().min(8)]
            );
        }
    }
    use super::*;
    use std::fs;

    // -------------------------------------------------------------------------
    // Tail-directory layout (#459)
    // -------------------------------------------------------------------------

    /// A deterministic, dedup-proof tile payload: distinct bytes AND distinct
    /// length per index, so no two tiles collide in the dedup cache and a
    /// mis-resolved directory entry yields visibly wrong bytes.
    fn tail_payload(i: u32) -> Vec<u8> {
        vec![(i % 251) as u8 + 1; 200 + (i as usize % 97)]
    }

    /// `(z, x, y)` for the i-th tile of the tail-layout fixtures, ascending in
    /// PMTiles tile id so the writer's clustered contract holds.
    fn tail_coord(i: u32) -> (u8, u32, u32) {
        let (z, x, y) = tile_id_to_zxy(tile_id(8, 0, 0) + u64::from(i)).unwrap();
        (z, x, y)
    }

    /// Every tile an archive addresses, read the way a client does: parse the
    /// header, walk root + leaf directories, slice at `tile_data_offset`.
    fn read_archive_tiles(bytes: &[u8]) -> HashMap<u64, Vec<u8>> {
        let header = Header::from_bytes(bytes).expect("header must parse");
        let entries = read_all_entries(bytes, &header).expect("directories must decode");
        let mut out = HashMap::new();
        for e in &entries {
            for r in 0..u64::from(e.run_length.max(1)) {
                let start = (header.tile_data_offset + e.offset) as usize;
                let end = start + e.length as usize;
                assert!(end <= bytes.len(), "entry points past EOF");
                out.insert(e.tile_id + r, bytes[start..end].to_vec());
            }
        }
        out
    }

    /// Both layouts must produce the same *archive*, and the tail layout must
    /// produce the same bytes whether or not checkpoints ran along the way.
    /// The whole point of #459 is that checkpoints stop costing tile-data I/O —
    /// not that they change what lands on disk.
    #[test]
    fn tail_checkpoint_then_finalize_is_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let with_ckpt = dir.path().join("with.pmtiles");
        let without = dir.path().join("without.pmtiles");

        for (out, checkpoints) in [(&with_ckpt, true), (&without, false)] {
            let mut w = StreamingPmtilesWriter::with_tail_layout(out, Compression::Gzip).unwrap();
            w.set_layer_name("tail");
            w.set_expect_clustered(true);
            for i in 0..40u32 {
                let (z, x, y) = tail_coord(i);
                w.add_tile(z, x, y, &tail_payload(i)).unwrap();
                if checkpoints && i % 7 == 0 {
                    w.checkpoint(out).unwrap();
                }
            }
            w.finalize(out).unwrap();
        }

        assert_eq!(
            fs::read(&with_ckpt).unwrap(),
            fs::read(&without).unwrap(),
            "checkpointing must not change the final archive"
        );
        // And the spool must be gone: `<output>.partial` is renamed into place.
        assert!(!with_ckpt.with_extension("pmtiles.partial").exists());
    }

    /// The mid-run salvage artifact is `<output>.partial`, and it must be a
    /// *complete, readable* archive as of every checkpoint — not merely a file
    /// The salvage artifact must never degrade from "valid" to
    /// "valid-looking and wrong" (#528 review, F1).
    ///
    /// The first tile appended after a checkpoint lands on top of that
    /// checkpoint's metadata and leaf directories, while the header in the
    /// prefix still points at them. Without the invalidating write, a crash
    /// in that window leaves a file whose magic, header and root directory
    /// all parse, whose leaf pointers resolve into tile bytes, and which
    /// therefore hands back garbage tiles with no error anywhere. Two things
    /// are pinned here: the file is *rejected* in that window, and it is a
    /// real archive again once the next checkpoint lands.
    #[test]
    fn tail_partial_is_detectably_invalid_between_checkpoint_and_next_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("salvage.pmtiles");
        let partial = dir.path().join("salvage.pmtiles.partial");

        let mut w = StreamingPmtilesWriter::with_tail_layout(&out, Compression::Gzip).unwrap();
        w.set_layer_name("tail");

        for i in 0..6u32 {
            let (z, x, y) = tail_coord(i);
            w.add_tile(z, x, y, &tail_payload(i)).unwrap();
        }
        w.checkpoint(&out).unwrap();

        // Baseline: a checkpoint with nothing added after it IS a valid
        // archive -- the existing property, kept pinned right here so the
        // two halves of the guarantee cannot drift apart.
        let bytes = fs::read(&partial).unwrap();
        Header::from_bytes(&bytes).expect("checkpoint alone leaves a valid archive");
        assert_eq!(read_archive_tiles(&bytes).len(), 6);

        // One more add, no checkpoint: the tail is now being overwritten, so
        // the file must be rejected rather than read.
        let (z, x, y) = tail_coord(6);
        w.add_tile(z, x, y, &tail_payload(6)).unwrap();

        let bytes = fs::read(&partial).unwrap();
        assert_ne!(
            &bytes[0..7],
            b"PMTiles",
            "the first post-checkpoint add must invalidate the on-disk prefix"
        );
        assert!(
            Header::from_bytes(&bytes).is_err(),
            "a partial written past its last checkpoint must be detectably invalid, \
             not silently readable"
        );

        // Simulate the crash: drop without finalizing. The file stays (a
        // checkpoint landed, so there are tiles worth keeping) and stays
        // invalid -- nothing repairs it behind our back.
        drop(w);
        assert!(partial.exists(), "a checkpointed partial survives the drop");
        assert!(
            Header::from_bytes(&fs::read(&partial).unwrap()).is_err(),
            "dropping the writer must not resurrect a half-overwritten archive"
        );
    }

    /// The invalidation is one write on a cold path, and the next checkpoint
    /// undoes it: after checkpoint -> add -> checkpoint the partial is a
    /// valid archive holding *both* generations of tiles.
    #[test]
    fn tail_partial_becomes_valid_again_at_the_next_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("again.pmtiles");
        let partial = dir.path().join("again.pmtiles.partial");

        let mut w = StreamingPmtilesWriter::with_tail_layout(&out, Compression::Gzip).unwrap();
        w.set_layer_name("tail");

        for i in 0..4u32 {
            let (z, x, y) = tail_coord(i);
            w.add_tile(z, x, y, &tail_payload(i)).unwrap();
        }
        w.checkpoint(&out).unwrap();

        for i in 4..9u32 {
            let (z, x, y) = tail_coord(i);
            w.add_tile(z, x, y, &tail_payload(i)).unwrap();
        }
        assert!(
            Header::from_bytes(&fs::read(&partial).unwrap()).is_err(),
            "invalid while the tail is being overwritten"
        );

        w.checkpoint(&out).unwrap();
        let bytes = fs::read(&partial).unwrap();
        let tiles = read_archive_tiles(&bytes);
        assert_eq!(tiles.len(), 9, "both generations are addressable again");
        for j in 0..9u32 {
            let (z, x, y) = tail_coord(j);
            let raw = tiles.get(&tile_id(z, x, y)).expect("tile present");
            let plain =
                compression::decompress_capped(raw, Compression::Gzip, compression::MAX_TILE_BYTES)
                    .unwrap();
            assert_eq!(plain, tail_payload(j), "tile {j} bytes after re-checkpoint");
        }

        w.finalize(&out).unwrap();
        Header::from_bytes(&fs::read(&out).unwrap()).unwrap();
    }

    /// A duplicate tile writes no bytes, so it cannot clobber the tail and
    /// must leave the checkpointed archive readable. The invalidation belongs
    /// to the append path only.
    #[test]
    fn tail_partial_survives_a_dedup_only_add_after_a_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("dedup.pmtiles");
        let partial = dir.path().join("dedup.pmtiles.partial");

        let mut w = StreamingPmtilesWriter::with_tail_layout(&out, Compression::Gzip).unwrap();
        w.set_layer_name("tail");
        for i in 0..4u32 {
            let (z, x, y) = tail_coord(i);
            w.add_tile(z, x, y, &tail_payload(i)).unwrap();
        }
        w.checkpoint(&out).unwrap();

        // Same bytes as tile 0 -- a dedup hit: a directory entry, no append.
        let (z, x, y) = tail_coord(9);
        w.add_tile(z, x, y, &tail_payload(0)).unwrap();

        let bytes = fs::read(&partial).unwrap();
        Header::from_bytes(&bytes).expect("a dedup-only add writes nothing, so nothing is stale");
        assert_eq!(
            read_archive_tiles(&bytes).len(),
            4,
            "still the last checkpoint's archive, unharmed"
        );
    }

    /// Constructing a writer must not destroy the previous run's salvage
    /// archive (#528 review, F2). `<output>.partial` is stepped aside to
    /// `<output>.partial.prev` before the new run reserves its prefix.
    #[test]
    fn constructing_a_tail_writer_preserves_an_earlier_partial() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("rerun.pmtiles");
        let partial = dir.path().join("rerun.pmtiles.partial");
        let prev = dir.path().join("rerun.pmtiles.partial.prev");

        // A crashed run's salvage archive, left behind.
        {
            let mut w = StreamingPmtilesWriter::with_tail_layout(&out, Compression::Gzip).unwrap();
            w.set_layer_name("first");
            for i in 0..5u32 {
                let (z, x, y) = tail_coord(i);
                w.add_tile(z, x, y, &tail_payload(i)).unwrap();
            }
            w.checkpoint(&out).unwrap();
        }
        let salvaged = fs::read(&partial).unwrap();
        assert_eq!(read_archive_tiles(&salvaged).len(), 5);

        // The rerun.
        let mut w = StreamingPmtilesWriter::with_tail_layout(&out, Compression::Gzip).unwrap();
        assert!(
            prev.exists(),
            "the earlier salvage archive must be moved aside, not truncated"
        );
        assert_eq!(
            fs::read(&prev).unwrap(),
            salvaged,
            "moved aside byte-for-byte"
        );

        w.set_layer_name("second");
        for i in 0..3u32 {
            let (z, x, y) = tail_coord(i);
            w.add_tile(z, x, y, &tail_payload(i)).unwrap();
        }
        w.finalize(&out).unwrap();

        assert_eq!(read_archive_tiles(&fs::read(&out).unwrap()).len(), 3);
        assert!(
            !prev.exists(),
            "a successful finalize clears the superseded salvage copy"
        );
    }

    /// whose header parses.
    #[test]
    fn tail_checkpoint_partial_is_a_valid_archive_at_every_step() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("salvage.pmtiles");
        let partial = dir.path().join("salvage.pmtiles.partial");

        let mut w = StreamingPmtilesWriter::with_tail_layout(&out, Compression::Gzip).unwrap();
        w.set_layer_name("tail");
        w.set_expect_clustered(true);
        assert_eq!(w.salvage_path(), Some(partial.as_path()));

        for i in 0..24u32 {
            let (z, x, y) = tail_coord(i);
            w.add_tile(z, x, y, &tail_payload(i)).unwrap();
            w.checkpoint(&out).unwrap();

            let bytes = fs::read(&partial).unwrap();
            let tiles = read_archive_tiles(&bytes);
            assert_eq!(
                tiles.len(),
                i as usize + 1,
                "checkpoint {i}: every tile added so far must be addressable"
            );
            for j in 0..=i {
                let (z, x, y) = tail_coord(j);
                let raw = tiles.get(&tile_id(z, x, y)).expect("tile must be present");
                let plain = compression::decompress_capped(
                    raw,
                    Compression::Gzip,
                    compression::MAX_TILE_BYTES,
                )
                .unwrap();
                assert_eq!(plain, tail_payload(j), "checkpoint {i}: tile {j} bytes");
            }
            // Metadata must survive at the tail too, not just the directories.
            let header = Header::from_bytes(&bytes).unwrap();
            let meta = compression::decompress_capped(
                &bytes[header.json_metadata_offset as usize
                    ..(header.json_metadata_offset + header.json_metadata_length) as usize],
                header.internal_compression,
                compression::MAX_INTERNAL_BYTES,
            )
            .unwrap();
            assert!(String::from_utf8(meta)
                .unwrap()
                .contains("\"vector_layers\""));
        }
        w.finalize(&out).unwrap();
        assert!(!partial.exists(), "finalize renames the partial into place");
    }

    /// The layout contract go-pmtiles `verify` enforces (verify.go v1.31.2,
    /// L84-89): a file's length must equal either `127 + root + metadata +
    /// leaves + tile_data` or `16384 + metadata + leaves + tile_data`. The tail
    /// layout targets the second form, so the sections must be exactly
    /// contiguous from offset 16384 on, with the *only* slack being the
    /// zero-padding between the root directory and 16384.
    #[test]
    fn tail_layout_sections_are_contiguous_and_sum_to_file_len() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("contig.pmtiles");
        let mut w = StreamingPmtilesWriter::with_tail_layout(&out, Compression::Gzip).unwrap();
        w.set_layer_name("tail");
        w.set_expect_clustered(true);
        // Enough entries to spill into leaf directories, so the leaf section is
        // non-empty and its placement at the tail is actually exercised.
        let mut rng = 0x2545_F491_4F6C_DD1Du64;
        for i in 0..30_000u32 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let len = 64 + (rng >> 33) as usize % 512;
            let (z, x, y) = tile_id_to_zxy(tile_id(14, 0, 0) + u64::from(i) * 7).unwrap();
            w.add_tile_precompressed(z, x, y, u64::from(i), &vec![(i % 251) as u8; len], len, 1)
                .unwrap();
        }
        w.finalize(&out).unwrap();

        let bytes = fs::read(&out).unwrap();
        let h = Header::from_bytes(&bytes).unwrap();
        assert_eq!(h.root_dir_offset, HEADER_BYTES as u64);
        assert!(
            h.leaf_dirs_length > 0,
            "this fixture must spill into leaves"
        );
        assert!(
            h.root_dir_offset + h.root_dir_length <= TAIL_LAYOUT_PREFIX_BYTES,
            "root must fit the 16 KiB prefix"
        );
        assert_eq!(h.tile_data_offset, TAIL_LAYOUT_PREFIX_BYTES);
        assert_eq!(
            h.json_metadata_offset,
            h.tile_data_offset + h.tile_data_length
        );
        assert_eq!(
            h.leaf_dirs_offset,
            h.json_metadata_offset + h.json_metadata_length
        );
        assert_eq!(
            bytes.len() as u64,
            TAIL_LAYOUT_PREFIX_BYTES
                + h.json_metadata_length
                + h.leaf_dirs_length
                + h.tile_data_length,
            "file length must match go-pmtiles' padded-length formula"
        );
        // The padding between root and tile data must be zeros, not stale bytes.
        let pad_start = (h.root_dir_offset + h.root_dir_length) as usize;
        assert!(bytes[pad_start..TAIL_LAYOUT_PREFIX_BYTES as usize]
            .iter()
            .all(|&b| b == 0));
        assert!(h.clustered);
        assert!(verify_clustered(&out).unwrap());
        assert_eq!(read_archive_tiles(&bytes).len(), 30_000);
    }

    /// The tail layout is only legal while the root directory fits the 16 KiB
    /// prefix. `make_root_leaves` guarantees that today, but the tail writer
    /// must not corrupt an archive if it ever stops doing so: it falls back to
    /// the packed full-copy layout rather than overrunning the prefix.
    #[test]
    fn tail_root_overflow_falls_back_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("overflow.pmtiles");
        let mut w = StreamingPmtilesWriter::with_tail_layout(&out, Compression::Gzip).unwrap();
        w.set_layer_name("tail");
        w.force_tail_root_overflow();
        for i in 0..20u32 {
            let (z, x, y) = tail_coord(i);
            w.add_tile(z, x, y, &tail_payload(i)).unwrap();
        }
        w.checkpoint(&out).unwrap();
        // The fallback publishes to `output_path` itself (the packed layout
        // cannot be written over the spool it reads from), and leaves a valid
        // archive there.
        let ckpt = read_archive_tiles(&fs::read(&out).unwrap());
        assert_eq!(ckpt.len(), 20);
        assert_eq!(
            w.salvage_path(),
            None,
            "a fallen-back writer publishes to the output, so there is no \
             `.partial` to point a salvage at"
        );

        w.finalize(&out).unwrap();
        let bytes = fs::read(&out).unwrap();
        let h = Header::from_bytes(&bytes).unwrap();
        assert_ne!(
            h.tile_data_offset, TAIL_LAYOUT_PREFIX_BYTES,
            "the fallback must use the packed layout, not the tail layout"
        );
        assert_eq!(
            bytes.len() as u64,
            HEADER_BYTES as u64
                + h.root_dir_length
                + h.json_metadata_length
                + h.leaf_dirs_length
                + h.tile_data_length,
            "the fallback must produce a packed, gap-free archive"
        );
        let tiles = read_archive_tiles(&bytes);
        assert_eq!(tiles.len(), 20);
        for i in 0..20u32 {
            let (z, x, y) = tail_coord(i);
            let plain = compression::decompress_capped(
                &tiles[&tile_id(z, x, y)],
                Compression::Gzip,
                compression::MAX_TILE_BYTES,
            )
            .unwrap();
            assert_eq!(plain, tail_payload(i));
        }
        assert!(!out.with_extension("pmtiles.partial").exists());
    }

    /// The point of #459, measured: a checkpoint must cost O(directory), not
    /// O(tile spool). Under the old layout each checkpoint re-copied the whole
    /// spool, so N checkpoints over a G-byte spool wrote ~N*G bytes.
    #[test]
    fn tail_checkpoint_io_is_directory_sized_not_spool_sized() {
        let dir = tempfile::tempdir().unwrap();
        let tail_out = dir.path().join("tail.pmtiles");
        let packed_out = dir.path().join("packed.pmtiles");

        // ~4 MB of incompressible-ish tile data, checkpointed 8 times.
        let add_all = |w: &mut StreamingPmtilesWriter, out: &Path| {
            let mut rng = 0x9E37_79B9_7F4A_7C15u64;
            for i in 0..800u32 {
                rng = rng
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let len = 4096 + (rng >> 33) as usize % 2048;
                let (z, x, y) = tile_id_to_zxy(tile_id(12, 0, 0) + u64::from(i)).unwrap();
                w.add_tile_precompressed(
                    z,
                    x,
                    y,
                    u64::from(i),
                    &vec![(i % 251) as u8; len],
                    len,
                    1,
                )
                .unwrap();
                if i % 100 == 99 {
                    w.checkpoint(out).unwrap();
                }
            }
        };

        let mut tail =
            StreamingPmtilesWriter::with_tail_layout(&tail_out, Compression::Gzip).unwrap();
        add_all(&mut tail, &tail_out);
        let tail_ckpt_bytes = tail.stats().checkpoint_bytes_written;
        let spool_bytes = tail.stats().bytes_written;
        tail.finalize(&tail_out).unwrap();

        // The legacy spooled layout, same tiles, for the before/after number.
        let mut packed = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        add_all(&mut packed, &packed_out);
        let packed_ckpt_bytes = packed.stats().checkpoint_bytes_written;
        packed.finalize(&packed_out).unwrap();

        // 8 checkpoints over a spool that grows to ~4 MB. The packed layout
        // re-copies the whole spool every time, so it writes the triangular
        // sum `Σ k/8 · spool = 4.5 · spool` — the quadratic growth #459 is
        // about. The tail layout writes only the 16 KiB prefix + tail sections.
        assert!(
            packed_ckpt_bytes > 4 * spool_bytes,
            "packed checkpoints should re-copy the spool each time \
             (wrote {packed_ckpt_bytes} over a {spool_bytes}-byte spool)"
        );
        assert!(
            tail_ckpt_bytes < spool_bytes / 4,
            "tail checkpoints must be directory-sized, not spool-sized \
             (wrote {tail_ckpt_bytes} over a {spool_bytes}-byte spool)"
        );
        // Measured on this fixture: a 4,083,340-byte spool costs 18,399,629
        // bytes of packed checkpoint I/O and 132,263 bytes of tail checkpoint
        // I/O — 139x less, and the gap widens with every additional gigabyte
        // of tiles, because only one of the two scales with them.
        //
        // Same archive content either way.
        assert_eq!(
            read_archive_tiles(&fs::read(&tail_out).unwrap()),
            read_archive_tiles(&fs::read(&packed_out).unwrap()),
        );
    }

    /// A torn prefix write (killed between the tail write and the prefix
    /// write) must leave the tile data intact: the prefix is the only thing
    /// rewritten in place, and it lives entirely below offset 16384.
    #[test]
    fn tail_torn_prefix_leaves_tile_data_intact() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("torn.pmtiles");
        let partial = dir.path().join("torn.pmtiles.partial");
        let mut w = StreamingPmtilesWriter::with_tail_layout(&out, Compression::Gzip).unwrap();
        w.set_layer_name("tail");
        for i in 0..16u32 {
            let (z, x, y) = tail_coord(i);
            w.add_tile(z, x, y, &tail_payload(i)).unwrap();
        }
        w.checkpoint(&out).unwrap();
        let good = fs::read(&partial).unwrap();

        // Simulate the tear: scribble over the prefix, then re-checkpoint.
        {
            let mut f = fs::OpenOptions::new().write(true).open(&partial).unwrap();
            f.write_all(&[0xABu8; 4096]).unwrap();
        }
        w.checkpoint(&out).unwrap();
        assert_eq!(
            fs::read(&partial).unwrap(),
            good,
            "the next checkpoint must rebuild prefix and tail from intact tile data"
        );
    }

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

    /// #371: the cumulative base is now the closed form `(4^z - 4) / 3`
    /// instead of `sum(4^i for i in 1..z)` with `4u64.pow`, which overflowed
    /// u64 at z32. The two must agree everywhere the old one was defined.
    #[test]
    fn tile_id_base_closed_form_matches_the_summation() {
        for z in 1..=MAX_TILE_ID_ZOOM {
            let summed: u64 = (1..u64::from(z)).map(|i| 4u64.pow(i as u32)).sum();
            let closed = ((1u64 << (2 * u32::from(z))) - 4) / 3;
            assert_eq!(closed, summed, "base id mismatch at z{z}");
        }
    }

    /// #371 boundary: exact tile ids at the write ceiling. Before the fix
    /// `xy_to_hilbert`'s `1u32 << z` was fine at z30 but the whole family of
    /// shifts was one zoom from masking; these values pin the arithmetic.
    #[test]
    fn tile_id_exact_values_at_max_zoom() {
        // base(30) = (4^30 - 4) / 3 = (2^60 - 4) / 3
        let base = ((1u64 << 60) - 4) / 3;
        assert_eq!(base, 384_307_168_202_282_324);
        assert_eq!(tile_id(30, 0, 0), base + 1);
        // The Hilbert curve at any zoom starts (0,0) and ends (n-1, 0).
        let n = crate::tile::max_tile_index(30);
        assert_eq!(n, (1u32 << 30) - 1);
        assert_eq!(tile_id(30, n, 0), base + 4u64.pow(30));
        // Every id at z30 lands inside z30's own block.
        for (x, y) in [(0u32, 0u32), (n, 0), (0, n), (n, n), (12_345, 678_910)] {
            let id = tile_id(30, x, y);
            assert!(
                id > base && id <= base + 4u64.pow(30),
                "z30 id {id} outside its block for ({x}, {y})"
            );
            assert_eq!(tile_id_to_zxy(id).unwrap(), (30, x, y));
        }
    }

    /// #371: past the u64 address space `tile_id` must not hand back a
    /// plausible-but-wrong id. In release `1u32 << 32` masked to `n = 1`,
    /// which silently collapsed every tile in the archive onto a few ids.
    #[test]
    fn tile_id_above_the_address_space_is_rejected_not_wrapped() {
        for z in [32u8, 33, 64, 255] {
            let id = tile_id(z, 0, 0);
            assert_eq!(id, u64::MAX, "z{z} must return the out-of-range sentinel");
            assert!(
                tile_id_to_zxy(id).is_err(),
                "the sentinel must not decode as a real tile"
            );
        }
    }

    /// #371: the sentinel is only useful if the writers refuse it. Before this
    /// check, `add_tile(32, ..)` stored a directory entry keyed `u64::MAX` —
    /// every tile past the ceiling collapsing onto one impossible id — and
    /// `finalize` succeeded, producing an archive no reader can address.
    #[test]
    fn writers_reject_a_zoom_with_no_tile_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rejected.pmtiles");
        let tile = [0x1a, 0x00];

        for z in [32u8, 33, 255] {
            let mut streaming = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
            streaming.set_layer_name("t");
            let err = streaming
                .add_tile(z, 0, 0, &tile)
                .expect_err("a zoom past the address space must be refused");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                err.to_string().contains(&format!("zoom {z}")),
                "error names the zoom: {err}"
            );
            assert!(streaming
                .add_tile_precompressed(z, 0, 0, 0, &tile, tile.len(), 0)
                .is_err());

            let mut buffered = PmtilesWriter::new();
            assert!(buffered.add_tile(z, 0, 0, &tile).is_err());
            assert!(buffered
                .add_tile_compressed(z, 0, 0, tile.to_vec())
                .is_err());
        }

        // The ceiling itself is still writable, and the archive finalizes.
        let mut streaming = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        streaming.set_layer_name("t");
        streaming.add_tile(MAX_TILE_ID_ZOOM, 0, 0, &tile).unwrap();
        streaming.finalize(&path).unwrap();
        let data = fs::read(&path).unwrap();
        assert_eq!(
            Header::from_bytes(&data[..127]).unwrap().max_zoom,
            MAX_TILE_ID_ZOOM
        );
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
    fn test_tile_id_to_zxy_matches_spec_examples() {
        assert_eq!(tile_id_to_zxy(0).unwrap(), (0, 0, 0));
        assert_eq!(tile_id_to_zxy(1).unwrap(), (1, 0, 0));
        assert_eq!(tile_id_to_zxy(2).unwrap(), (1, 0, 1));
        assert_eq!(tile_id_to_zxy(3).unwrap(), (1, 1, 1));
        assert_eq!(tile_id_to_zxy(4).unwrap(), (1, 1, 0));
        assert_eq!(tile_id_to_zxy(5).unwrap(), (2, 0, 0));
    }

    #[test]
    fn test_tile_id_to_zxy_inverts_tile_id_exhaustively() {
        // Exhaustive round-trip at low zooms...
        for z in 0..=5u8 {
            let n = 1u32 << z;
            for y in 0..n {
                for x in 0..n {
                    assert_eq!(
                        tile_id_to_zxy(tile_id(z, x, y)).unwrap(),
                        (z, x, y),
                        "round-trip z={z} x={x} y={y}"
                    );
                }
            }
        }
        // ...and spot checks at high zooms, including corners.
        for (z, x, y) in [
            (14u8, 4823u32, 6160u32),
            (14, 0, 0),
            (14, (1 << 14) - 1, (1 << 14) - 1),
            (20, 123_456, 654_321),
            (31, (1u32 << 31) - 1, 0),
        ] {
            assert_eq!(tile_id_to_zxy(tile_id(z, x, y)).unwrap(), (z, x, y));
        }
    }

    // ---- Hostile archives (#417) -------------------------------------------

    /// Hand-assemble a directory body: count, then the four varint columns.
    fn hostile_directory(
        count: u64,
        deltas: &[u64],
        run_lengths: &[u64],
        lengths: &[u64],
        offsets: &[u64],
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        encode_varint(count, &mut buf);
        for column in [deltas, run_lengths, lengths, offsets] {
            for &v in column {
                encode_varint(v, &mut buf);
            }
        }
        buf
    }

    #[test]
    fn decode_directory_rejects_run_length_past_u32() {
        // 2^32 truncates to 0 — a tile entry silently becomes a *leaf
        // pointer*, and the reader then slices tile bytes as a directory.
        let data = hostile_directory(1, &[1], &[1u64 << 32], &[10], &[1]);
        assert!(
            decode_directory(&data).is_none(),
            "a run length past u32 must be rejected, not truncated"
        );
    }

    #[test]
    fn decode_directory_rejects_length_past_u32() {
        // 2^32 + 10 truncates to 10: a 4 GiB claim becomes a 10-byte read.
        let data = hostile_directory(1, &[1], &[1], &[(1u64 << 32) + 10], &[1]);
        assert!(
            decode_directory(&data).is_none(),
            "a length past u32 must be rejected, not truncated"
        );
    }

    #[test]
    fn decode_directory_rejects_tile_id_delta_overflow() {
        // Two deltas that wrap u64 when summed: the second entry's id would
        // land back near zero and address an unrelated tile.
        let data = hostile_directory(2, &[u64::MAX, u64::MAX], &[1, 1], &[10, 10], &[1, 0]);
        assert!(
            decode_directory(&data).is_none(),
            "a tile-id delta sum that wraps u64 must be rejected"
        );
    }

    #[test]
    fn decode_directory_rejects_entry_count_the_body_cannot_hold() {
        // The count is an archive-controlled varint that is trusted straight
        // into `Vec::with_capacity`: u64::MAX aborts the process on capacity
        // overflow before a single entry is read.
        let mut data = Vec::new();
        encode_varint(u64::MAX, &mut data);
        assert!(
            decode_directory(&data).is_none(),
            "an entry count the body cannot possibly hold must be rejected"
        );
    }

    #[test]
    fn header_rejects_zoom_past_the_tile_id_space() {
        let mut bytes = Header::default().to_bytes();
        bytes[101] = 32; // max_zoom
        assert!(
            Header::from_bytes(&bytes).is_err(),
            "z32 has no PMTiles tile-id space"
        );
    }

    /// `min_zoom` and `center_zoom` are display metadata — no reader path
    /// touches either — so a sloppy-but-otherwise-valid archive is repaired
    /// rather than refused. Rejecting them protected nothing and locked out
    /// real files.
    #[test]
    fn header_clamps_inverted_zoom_range_instead_of_rejecting_it() {
        let mut bytes = Header::default().to_bytes();
        bytes[100] = 10; // min_zoom
        bytes[101] = 5; // max_zoom
        let header = Header::from_bytes(&bytes).expect("a sloppy min zoom must not be fatal");
        assert_eq!(header.max_zoom, 5);
        assert_eq!(header.min_zoom, 5, "min zoom is clamped to max zoom");
    }

    #[test]
    fn header_clamps_center_zoom_past_max_zoom() {
        let mut bytes = Header::default().to_bytes();
        bytes[101] = 6; // max_zoom
        bytes[118] = 7; // center_zoom
        let header = Header::from_bytes(&bytes).expect("a sloppy center zoom must not be fatal");
        assert_eq!(header.center_zoom, 6, "center zoom is clamped to max zoom");
    }

    #[test]
    fn header_rejects_unknown_compression_codes() {
        // Byte 0 parses as `Compression::Unknown`, which every codec call
        // then fails on. Reject it here, where the message can say which
        // field was nonsense, rather than at the first directory read.
        let mut bytes = Header::default().to_bytes();
        bytes[97] = 0; // internal_compression
        let err = Header::from_bytes(&bytes).expect_err("unknown internal compression");
        assert!(
            err.to_string().contains("internal compression"),
            "message must name the field, got: {err}"
        );

        let mut bytes = Header::default().to_bytes();
        bytes[98] = 0; // tile_compression
        let err = Header::from_bytes(&bytes).expect_err("unknown tile compression");
        assert!(
            err.to_string().contains("tile compression"),
            "message must name the field, got: {err}"
        );
    }

    #[test]
    fn decode_varint_rejects_a_non_canonical_tenth_byte() {
        // The tenth byte of a u64 varint carries exactly one payload bit.
        // Anything above 1 there used to be shifted out of the register,
        // handing the caller a small, plausible value of the attacker's
        // choosing instead of the bytes that were actually written.
        let mut data = vec![0xFFu8; 9];
        data.push(0x02);
        assert!(
            decode_varint(&data).is_none(),
            "the tenth byte's high bits must be rejected, not wrapped"
        );

        // The two canonical tenth bytes still decode.
        let mut ok = vec![0xFFu8; 9];
        ok.push(0x01);
        assert_eq!(decode_varint(&ok), Some((u64::MAX, 10)));
        let mut round = Vec::new();
        encode_varint(u64::MAX, &mut round);
        assert_eq!(decode_varint(&round), Some((u64::MAX, round.len())));
    }

    #[test]
    fn header_rejects_section_bounds_that_overflow_u64() {
        for header in [
            Header {
                root_dir_offset: u64::MAX,
                root_dir_length: 2,
                ..Default::default()
            },
            Header {
                json_metadata_offset: u64::MAX - 1,
                json_metadata_length: 8,
                ..Default::default()
            },
            Header {
                leaf_dirs_offset: u64::MAX,
                leaf_dirs_length: 1,
                ..Default::default()
            },
            Header {
                tile_data_offset: u64::MAX,
                tile_data_length: u64::MAX,
                ..Default::default()
            },
        ] {
            assert!(
                Header::from_bytes(&header.to_bytes()).is_err(),
                "section offset + length must not wrap u64: {header:?}"
            );
        }
    }

    #[test]
    fn test_tile_id_to_zxy_rejects_out_of_range() {
        // One past the last z31 ID must error rather than wrap.
        let past_z31 = (0..=31u8).map(|z| 1u64 << (2 * u64::from(z))).sum::<u64>();
        assert!(tile_id_to_zxy(past_z31).is_err());
        assert!(tile_id_to_zxy(u64::MAX).is_err());
    }

    #[test]
    fn test_header_from_bytes_roundtrips_to_bytes() {
        let header = Header {
            root_dir_offset: 127,
            root_dir_length: 421,
            json_metadata_offset: 548,
            json_metadata_length: 33,
            leaf_dirs_offset: 581,
            leaf_dirs_length: 1290,
            tile_data_offset: 1871,
            tile_data_length: 999_999,
            addressed_tiles_count: 42,
            tile_entries_count: 40,
            tile_contents_count: 39,
            clustered: true,
            internal_compression: Compression::Gzip,
            tile_compression: Compression::Zstd,
            tile_type: TileType::Mvt,
            min_zoom: 3,
            max_zoom: 14,
            min_lon: -75.1652,
            min_lat: -33.8688,
            max_lon: 151.2093,
            max_lat: 48.8566,
            center_zoom: 8,
            center_lon: 2.3522,
            center_lat: 39.9526,
        };
        let parsed = Header::from_bytes(&header.to_bytes()).unwrap();

        assert_eq!(parsed.root_dir_offset, header.root_dir_offset);
        assert_eq!(parsed.root_dir_length, header.root_dir_length);
        assert_eq!(parsed.json_metadata_offset, header.json_metadata_offset);
        assert_eq!(parsed.json_metadata_length, header.json_metadata_length);
        assert_eq!(parsed.leaf_dirs_offset, header.leaf_dirs_offset);
        assert_eq!(parsed.leaf_dirs_length, header.leaf_dirs_length);
        assert_eq!(parsed.tile_data_offset, header.tile_data_offset);
        assert_eq!(parsed.tile_data_length, header.tile_data_length);
        assert_eq!(parsed.addressed_tiles_count, header.addressed_tiles_count);
        assert_eq!(parsed.tile_entries_count, header.tile_entries_count);
        assert_eq!(parsed.tile_contents_count, header.tile_contents_count);
        assert_eq!(parsed.clustered, header.clustered);
        assert_eq!(parsed.internal_compression, header.internal_compression);
        assert_eq!(parsed.tile_compression, header.tile_compression);
        assert_eq!(parsed.tile_type, header.tile_type);
        assert_eq!(parsed.min_zoom, header.min_zoom);
        assert_eq!(parsed.max_zoom, header.max_zoom);
        // Coordinates go through the i32 * 1e7 spec encoding: 1e-7 precision.
        for (got, want) in [
            (parsed.min_lon, header.min_lon),
            (parsed.min_lat, header.min_lat),
            (parsed.max_lon, header.max_lon),
            (parsed.max_lat, header.max_lat),
            (parsed.center_lon, header.center_lon),
            (parsed.center_lat, header.center_lat),
        ] {
            assert!((got - want).abs() < 1e-6, "{got} vs {want}");
        }
        assert_eq!(parsed.center_zoom, header.center_zoom);
    }

    #[test]
    fn test_header_from_bytes_rejects_garbage() {
        // Too short.
        assert!(Header::from_bytes(&[0u8; 50]).is_err());
        // Bad magic.
        let mut bytes = Header::default().to_bytes();
        bytes[0] = b'X';
        assert!(Header::from_bytes(&bytes).is_err());
        // Bad version.
        let mut bytes = Header::default().to_bytes();
        bytes[7] = 2;
        assert!(Header::from_bytes(&bytes).is_err());
        // Out-of-spec compression code.
        let mut bytes = Header::default().to_bytes();
        bytes[97] = 9;
        assert!(Header::from_bytes(&bytes).is_err());
        // Out-of-spec tile type code.
        let mut bytes = Header::default().to_bytes();
        bytes[99] = 9;
        assert!(Header::from_bytes(&bytes).is_err());
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

    /// Three leaf pointers laid out the way tippecanoe / go-pmtiles write a
    /// root directory: every entry has run_length = 0 and every offset after
    /// the first is encoded as 0 ("contiguous with the previous entry").
    fn tippecanoe_style_leaf_root() -> Vec<u8> {
        let mut buf = Vec::new();
        encode_varint(3, &mut buf);
        // tile ids: 0, 1000, 2000 (delta-encoded)
        for delta in [0, 1000, 1000] {
            encode_varint(delta, &mut buf);
        }
        // run lengths: all 0 (leaf pointers)
        for _ in 0..3 {
            encode_varint(0, &mut buf);
        }
        // compressed leaf lengths
        for len in [100, 200, 50] {
            encode_varint(len, &mut buf);
        }
        // offsets: explicit 0 (stored as 0 + 1), then contiguous, contiguous
        for encoded_offset in [1, 0, 0] {
            encode_varint(encoded_offset, &mut buf);
        }
        buf
    }

    fn dir_tuples(entries: &[DirEntry]) -> Vec<(u64, u64, u32, u32)> {
        entries
            .iter()
            .map(|e| (e.tile_id, e.offset, e.length, e.run_length))
            .collect()
    }

    #[test]
    fn test_decode_directory_resolves_contiguous_leaf_offsets() {
        // Issue #377: the contiguous-offset rule applies to leaf pointers too.
        // Before the fix every leaf after the first decoded to offset 0, so
        // `decode` sliced the wrong bytes and gzip failed with
        // "incomplete deflate stream" on any tippecanoe archive with leaves.
        let entries = decode_directory(&tippecanoe_style_leaf_root()).unwrap();
        assert_eq!(
            dir_tuples(&entries),
            vec![(0, 0, 100, 0), (1000, 100, 200, 0), (2000, 300, 50, 0)]
        );
    }

    #[test]
    fn test_encode_directory_contiguous_leaf_entries_encode_as_zero() {
        // The encoder must apply the same rule, so our root directories are
        // as compact as the spec allows and round-trip through any reader.
        let entries = vec![
            DirEntry {
                tile_id: 0,
                offset: 0,
                length: 100,
                run_length: 0,
            },
            DirEntry {
                tile_id: 1000,
                offset: 100,
                length: 200,
                run_length: 0,
            },
            DirEntry {
                tile_id: 2000,
                offset: 300,
                length: 50,
                run_length: 0,
            },
        ];
        let encoded = encode_directory(&entries);
        assert_eq!(encoded, tippecanoe_style_leaf_root());
        assert_eq!(
            dir_tuples(&decode_directory(&encoded).unwrap()),
            dir_tuples(&entries)
        );
    }

    #[test]
    fn test_decode_directory_rejects_offset_that_overflows_the_accumulator() {
        // An explicit offset varint of u64::MAX decodes to u64::MAX - 1; the
        // contiguous accumulator (offset + length) must fail as a decode
        // error rather than overflow. Hand-encoded: two tile entries.
        let mut buf = Vec::new();
        encode_varint(2, &mut buf); // count
        encode_varint(0, &mut buf); // tile id 0
        encode_varint(1, &mut buf); // tile id 1 (delta)
        encode_varint(1, &mut buf); // run lengths
        encode_varint(1, &mut buf);
        encode_varint(5, &mut buf); // lengths
        encode_varint(5, &mut buf);
        encode_varint(u64::MAX, &mut buf); // offsets: hostile, then contiguous
        encode_varint(0, &mut buf);
        assert!(decode_directory(&buf).is_none());
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

    /// A full-world extent must survive `set_bounds` at the exact Web Mercator
    /// latitude bound. The header stores latitude as `i32 = degrees * 1e7`, so
    /// the clamp is directly observable there: the old `±85.05` clamp wrote
    /// ±850_500_000 and shaved ~0.0011° (~125 m) off the top and bottom of
    /// every world-spanning archive (#416).
    #[test]
    fn test_writer_bounds_keep_exact_mercator_latitude() {
        let mut writer = PmtilesWriter::new();
        writer.add_tile(0, 0, 0, &[1, 2, 3]).unwrap();
        writer.set_bounds(&TileBounds::new(
            -180.0,
            -85.051_128_779_806_59,
            180.0,
            85.051_128_779_806_59,
        ));

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bounds-mercator-clamp.pmtiles");
        writer.write_to_file(&path).expect("Should write file");

        let data = fs::read(&path).unwrap();
        let min_lat = i32::from_le_bytes(data[106..110].try_into().unwrap());
        let max_lat = i32::from_le_bytes(data[114..118].try_into().unwrap());

        assert_eq!(max_lat, 850_511_287, "max_lat must keep the exact bound");
        assert_eq!(min_lat, -850_511_287, "min_lat must keep the exact bound");
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

    /// #529, #522: `go-pmtiles verify` rejects a header whose `min_zoom` is
    /// declared below the shallowest tile the archive actually holds
    /// ("header MinZoom does not match min tile z"). #380's declared-minimum
    /// widening must not reach the header — it still reaches
    /// `vector_layers[].minzoom`, which is what TileJSON-building clients
    /// read to decide the range to request (letting them overzoom from a
    /// coarser level than the archive holds), while the PMTiles v3 header
    /// stays an honest description of what the directory addresses.
    #[test]
    fn streaming_writer_declared_min_zoom_widens_metadata_but_not_header() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("declared.pmtiles");
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("t");
        writer.set_declared_min_zoom(0);
        writer.add_tile(2, 1, 1, &[0x1a, 0x00]).unwrap();
        writer.add_tile(4, 5, 5, &[0x1a, 0x01]).unwrap();
        writer.finalize(&path).unwrap();

        let data = fs::read(&path).unwrap();
        let header = Header::from_bytes(&data[..127]).unwrap();
        assert_eq!(
            header.min_zoom, 2,
            "header must be the shallowest zoom that actually holds a tile, \
             never the declared minimum -- go-pmtiles verify checks this"
        );
        assert_eq!(header.max_zoom, 4);

        let start = header.json_metadata_offset as usize;
        let end = start + header.json_metadata_length as usize;
        let mut json = String::new();
        GzDecoder::new(&data[start..end])
            .read_to_string(&mut json)
            .unwrap();
        assert!(
            json.contains(r#""minzoom":0"#),
            "vector_layers must still advertise the declared minimum: {json}"
        );
    }

    /// A declared minimum finer than the coarsest tile present cannot narrow
    /// the range: the tiles are there, the header must cover them.
    #[test]
    fn streaming_writer_declared_min_zoom_never_hides_written_tiles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("declared-narrow.pmtiles");
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("t");
        writer.set_declared_min_zoom(3);
        writer.add_tile(2, 1, 1, &[0x1a, 0x00]).unwrap();
        writer.finalize(&path).unwrap();
        let data = fs::read(&path).unwrap();
        assert_eq!(Header::from_bytes(&data[..127]).unwrap().min_zoom, 2);
    }

    /// A declared minimum over an archive that got no tiles at all must not
    /// outrun the zero max zoom the empty archive collapses to: the header
    /// stays z0..z0 (min <= max), as it was before declared minimums existed.
    #[test]
    fn streaming_writer_declared_min_zoom_over_zero_tiles_stays_z0() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("declared-empty.pmtiles");
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("t");
        writer.set_declared_min_zoom(3);
        writer.finalize(&path).unwrap();

        let data = fs::read(&path).unwrap();
        let header = Header::from_bytes(&data[..127]).unwrap();
        assert_eq!(header.min_zoom, 0, "empty archive is z0..z0, not z3..z0");
        assert_eq!(header.max_zoom, 0);
        assert_eq!(header.center_zoom, 0);

        let start = header.json_metadata_offset as usize;
        let end = start + header.json_metadata_length as usize;
        let mut json = String::new();
        GzDecoder::new(&data[start..end])
            .read_to_string(&mut json)
            .unwrap();
        assert!(
            json.contains(r#""minzoom":0"#),
            "vector_layers of an empty archive stay at minzoom 0: {json}"
        );
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

    /// Streaming counterpart of `test_writer_bounds_keep_exact_mercator_latitude`:
    /// the streaming writer has its own `set_bounds`, so it needs its own
    /// guard against the clamp regressing to a rounded ±85.05 (#416).
    #[test]
    fn test_streaming_writer_bounds_keep_exact_mercator_latitude() {
        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");
        writer.add_tile(0, 0, 0, &[0x1a, 0x00]).unwrap();
        writer.set_bounds(&TileBounds::new(
            -180.0,
            -85.051_128_779_806_59,
            180.0,
            85.051_128_779_806_59,
        ));

        let dir = tempfile::tempdir().unwrap();
        let output_path = dir.path().join("streaming-bounds-mercator-clamp.pmtiles");
        writer.finalize(&output_path).expect("Should finalize");

        let data = fs::read(&output_path).unwrap();
        let min_lat = i32::from_le_bytes(data[106..110].try_into().unwrap());
        let max_lat = i32::from_le_bytes(data[114..118].try_into().unwrap());

        assert_eq!(max_lat, 850_511_287, "max_lat must keep the exact bound");
        assert_eq!(min_lat, -850_511_287, "min_lat must keep the exact bound");
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

        // A small archive has no leaf directories...
        assert_eq!(
            leaf_dirs_length, 0,
            "Small archive should not have leaf directories"
        );
        // ...but its OFFSET must still point at the (empty) leaf section rather
        // than being zeroed. This test previously asserted 0, which is what
        // made every small archive this writer produced fail go-pmtiles:
        //
        //     Failed to verify archive, Leaf directories offset=0 must not be 0
        //
        // The section sits between the metadata and the tile data, so with no
        // leaves it is an empty span at the end of the metadata -- which is
        // also where the tile data begins.
        let metadata_offset = u64::from_le_bytes(data[24..32].try_into().unwrap());
        let metadata_length = u64::from_le_bytes(data[32..40].try_into().unwrap());
        let tile_data_offset = u64::from_le_bytes(data[56..64].try_into().unwrap());
        assert_ne!(
            leaf_dirs_offset, 0,
            "leaf_dirs_offset must never be 0 -- go-pmtiles rejects the archive"
        );
        assert_eq!(leaf_dirs_offset, metadata_offset + metadata_length);
        assert_eq!(leaf_dirs_offset, tile_data_offset);

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

    #[test]
    fn add_tile_precompressed_matches_add_tile_with_count() {
        // Issue #227 moves gzip off the serial writer thread: the export loop
        // now compresses tiles inside the parallel encode section and hands the
        // finished bytes (plus the *uncompressed* hash) to
        // add_tile_precompressed. The resulting archive must be byte-identical
        // to compressing serially via add_tile_with_count, including dedup: the
        // 4th tile below repeats the 1st tile's content.
        let tiles: Vec<(u8, u32, u32, Vec<u8>)> = vec![
            (0, 0, 0, vec![0x1a, 0x05, b'h', b'e', b'l', b'l', b'o']),
            (1, 0, 0, vec![0x1a, 0x03, b'a', b'b', b'c']),
            (1, 0, 1, vec![0x1a, 0x03, b'x', b'y', b'z']),
            (1, 1, 1, vec![0x1a, 0x05, b'h', b'e', b'l', b'l', b'o']), // dup of (0,0,0)
        ];
        let dir = std::env::temp_dir();

        // Baseline: writer compresses each tile serially.
        let mut serial = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        serial.set_layer_name("test");
        serial.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));
        for (z, x, y, data) in &tiles {
            serial.add_tile_with_count(*z, *x, *y, data, 1).unwrap();
        }
        let serial_path = dir.join("gpq-227-serial.pmtiles");
        let _ = fs::remove_file(&serial_path);
        serial.finalize(&serial_path).unwrap();

        // New path: compress up front, hand bytes + uncompressed hash to writer.
        let mut parallel = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        parallel.set_layer_name("test");
        parallel.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));
        for (z, x, y, data) in &tiles {
            let hash = TileHasher::hash(data);
            let compressed = compression::compress(data, Compression::Gzip).unwrap();
            parallel
                .add_tile_precompressed(*z, *x, *y, hash, &compressed, data.len(), 1)
                .unwrap();
        }
        let parallel_path = dir.join("gpq-227-parallel.pmtiles");
        let _ = fs::remove_file(&parallel_path);
        parallel.finalize(&parallel_path).unwrap();

        let a = fs::read(&serial_path).unwrap();
        let b = fs::read(&parallel_path).unwrap();
        assert_eq!(
            a, b,
            "precompressed archive must be byte-identical to serial compression"
        );

        let _ = fs::remove_file(&serial_path);
        let _ = fs::remove_file(&parallel_path);
    }

    #[test]
    fn checkpoint_produces_valid_capped_pmtiles() {
        // Issue #229: a mid-run checkpoint must produce a fully valid PMTiles
        // archive capped at the zooms written so far, WITHOUT consuming the
        // writer — the export loop keeps adding finer levels afterwards.
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        // Two coarse levels finished (z0: 1 tile, z1: 2 tiles).
        writer.add_tile(0, 0, 0, &[0x1a, 0x00]).unwrap();
        writer.add_tile(1, 0, 0, &[0x1a, 0x01]).unwrap();
        writer.add_tile(1, 0, 1, &[0x1a, 0x02]).unwrap();

        let ckpt_path = std::env::temp_dir().join("gpq-229-checkpoint.pmtiles");
        let _ = fs::remove_file(&ckpt_path);
        writer
            .checkpoint(&ckpt_path)
            .expect("checkpoint should succeed");

        // Checkpoint is a valid, capped archive.
        let ck = fs::read(&ckpt_path).unwrap();
        assert_eq!(&ck[0..7], b"PMTiles", "checkpoint has PMTiles magic");
        assert_eq!(ck[7], 3, "checkpoint is version 3");
        assert_eq!(ck[100], 0, "checkpoint min_zoom == 0");
        assert_eq!(
            ck[101], 1,
            "checkpoint max_zoom capped at last finished level"
        );
        let ck_addressed = u64::from_le_bytes(ck[72..80].try_into().unwrap());
        assert_eq!(ck_addressed, 3, "checkpoint holds the 3 finished tiles");

        // Writer is still usable: add a finer level and finalize.
        writer.add_tile(2, 0, 0, &[0x1a, 0x03]).unwrap();
        let final_path = std::env::temp_dir().join("gpq-229-checkpoint-final.pmtiles");
        let _ = fs::remove_file(&final_path);
        writer
            .finalize(&final_path)
            .expect("finalize after checkpoint");

        let fin = fs::read(&final_path).unwrap();
        assert_eq!(fin[101], 2, "final max_zoom includes the finer level");
        let fin_addressed = u64::from_le_bytes(fin[72..80].try_into().unwrap());
        assert_eq!(fin_addressed, 4, "final holds all 4 tiles");

        let _ = fs::remove_file(&ckpt_path);
        let _ = fs::remove_file(&final_path);
    }

    #[test]
    fn checkpoint_then_finalize_byte_identical_to_no_checkpoint() {
        // Issue #229: intermediate checkpoints must NOT change the final bytes.
        // Both checkpoint and finalize route through the same archive assembler,
        // so a run that checkpoints midway must be byte-identical to one that
        // never checkpoints. The 4th tile dups the 1st to exercise dedup.
        let tiles: Vec<(u8, u32, u32, Vec<u8>)> = vec![
            (0, 0, 0, vec![0x1a, 0x05, b'h', b'e', b'l', b'l', b'o']),
            (1, 0, 0, vec![0x1a, 0x03, b'a', b'b', b'c']),
            (1, 0, 1, vec![0x1a, 0x03, b'x', b'y', b'z']),
            (1, 1, 1, vec![0x1a, 0x05, b'h', b'e', b'l', b'l', b'o']), // dup of (0,0,0)
        ];
        let dir = std::env::temp_dir();

        // Baseline: no checkpoint, single finalize.
        let mut plain = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        plain.set_layer_name("test");
        plain.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));
        for (z, x, y, data) in &tiles {
            plain.add_tile(*z, *x, *y, data).unwrap();
        }
        let plain_path = dir.join("gpq-229-plain.pmtiles");
        let _ = fs::remove_file(&plain_path);
        plain.finalize(&plain_path).unwrap();

        // Checkpointed: assemble the archive after the first tile, keep going.
        let mut ckpt = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        ckpt.set_layer_name("test");
        ckpt.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));
        let ckpt_path = dir.join("gpq-229-intermediate.pmtiles");
        let final_path = dir.join("gpq-229-checkpointed-final.pmtiles");
        let _ = fs::remove_file(&final_path);
        for (i, (z, x, y, data)) in tiles.iter().enumerate() {
            ckpt.add_tile(*z, *x, *y, data).unwrap();
            if i == 0 {
                let _ = fs::remove_file(&ckpt_path);
                ckpt.checkpoint(&ckpt_path).unwrap();
            }
        }
        ckpt.finalize(&final_path).unwrap();

        let a = fs::read(&plain_path).unwrap();
        let b = fs::read(&final_path).unwrap();
        assert_eq!(
            a, b,
            "checkpointed run must be byte-identical to the non-checkpointed finalize"
        );

        let _ = fs::remove_file(&plain_path);
        let _ = fs::remove_file(&ckpt_path);
        let _ = fs::remove_file(&final_path);
    }

    // -------------------------------------------------------------------------
    // Clustered header flag honesty
    // -------------------------------------------------------------------------

    /// An export adds tiles zoom-by-zoom in row-major `(x, y)` order, not
    /// tile-id (Hilbert) order — but the writer used to stamp every archive
    /// `clustered: true` regardless. Once `self.entries` is sorted by
    /// tile_id for the directory, a row-major add order leaves offsets
    /// scattered rather than monotonic, so a header claiming `clustered`
    /// here would be a lie go-pmtiles' `verify` catches ("out-of-order entry
    /// in clustered archive").
    #[test]
    fn clustered_false_when_tiles_added_row_major() {
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        // z2 has 16 tiles; row-major (y outer, x inner) add order is not
        // Hilbert tile-id order at any zoom >= 1, so sorting by tile_id
        // afterwards does not recover the order these offsets were assigned
        // in. Distinct, differently-sized payloads so nothing dedups.
        for y in 0..4u32 {
            for x in 0..4u32 {
                let n = y * 4 + x;
                let data = vec![n as u8; 20 + n as usize];
                writer.add_tile(2, x, y, &data).unwrap();
            }
        }

        let tmp = tempfile::NamedTempFile::new().unwrap();
        writer.finalize(tmp.path()).unwrap();

        let bytes = fs::read(tmp.path()).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();
        assert!(
            !header.clustered,
            "row-major add order is not tile-id order; the header must not claim clustered"
        );
        assert_eq!(
            header.clustered,
            verify_clustered(tmp.path()).unwrap(),
            "the writer's own flag must agree with what the bytes on disk deliver"
        );
    }

    /// The counterpart to `clustered_false_when_tiles_added_row_major`:
    /// tiles added in ascending tile-id order keep the directory's offsets
    /// monotonic, so the archive genuinely is clustered and the header may
    /// say so.
    #[test]
    fn clustered_true_when_added_in_tile_id_order() {
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        let mut tiles: Vec<(u8, u32, u32)> = (0..4u32)
            .flat_map(|y| (0..4u32).map(move |x| (2u8, x, y)))
            .collect();
        tiles.sort_by_key(|&(z, x, y)| tile_id(z, x, y));

        for (z, x, y) in &tiles {
            let n = x + y * 4;
            let data = vec![n as u8; 20 + n as usize];
            writer.add_tile(*z, *x, *y, &data).unwrap();
        }

        let tmp = tempfile::NamedTempFile::new().unwrap();
        writer.finalize(tmp.path()).unwrap();

        let bytes = fs::read(tmp.path()).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();
        assert!(
            header.clustered,
            "adding tiles in tile-id order keeps offsets monotonic"
        );
        assert_eq!(header.clustered, verify_clustered(tmp.path()).unwrap());
    }

    /// A dedup back-reference — an entry whose offset points at bytes
    /// already accounted for, not at the running high-water mark — is legal
    /// in a clustered archive (go-pmtiles' `verify` allows it explicitly).
    /// Tiles are still added in tile-id order here; only the *content*
    /// repeats, exercising the `offset + length <= end` branch of the
    /// predicate rather than only the trivial always-append case covered by
    /// `clustered_true_when_added_in_tile_id_order`.
    #[test]
    fn clustered_true_with_duplicate_backreference() {
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");
        writer.set_bounds(&TileBounds::new(-180.0, -85.0, 180.0, 85.0));

        let mut tiles: Vec<(u8, u32, u32)> = (0..4u32)
            .flat_map(|y| (0..4u32).map(move |x| (2u8, x, y)))
            .collect();
        tiles.sort_by_key(|&(z, x, y)| tile_id(z, x, y));

        let dup_data = vec![0xABu8; 32];
        for (i, (z, x, y)) in tiles.iter().enumerate() {
            // Every third tile repeats the same content, so the dedup
            // back-reference points well behind the running high-water mark,
            // not merely at the immediately preceding entry.
            let data = if i % 3 == 0 {
                dup_data.clone()
            } else {
                let n = x + y * 4;
                vec![n as u8; 20 + n as usize]
            };
            writer.add_tile(*z, *x, *y, &data).unwrap();
        }

        let tmp = tempfile::NamedTempFile::new().unwrap();
        writer.finalize(tmp.path()).unwrap();

        let bytes = fs::read(tmp.path()).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();
        assert!(
            header.clustered,
            "a dedup back-reference into already-written bytes is legal in a clustered archive"
        );
        assert_eq!(header.clustered, verify_clustered(tmp.path()).unwrap());
    }

    /// #516: go-pmtiles' `verify` accepts a back-reference only to an
    /// *exact* previously-seen entry offset, not to any offset that merely
    /// falls within bytes already accounted for. An entry pointing into the
    /// middle of an earlier tile — offset 5 landing inside a first tile that
    /// spans bytes [0, 10) — used to pass `offsets_are_clustered` (it
    /// satisfied `offset + length <= end`) but must fail here, since no
    /// reader ever wrote an entry whose offset was exactly 5.
    #[test]
    fn offsets_are_clustered_rejects_backreference_into_tile_middle() {
        assert!(!offsets_are_clustered([(0, 10), (5, 3)]));
        // The exact start of the first tile is still a legal back-reference.
        assert!(offsets_are_clustered([(0, 10), (0, 10)]));
        // A fresh append after a legal dedup is still fine too.
        assert!(offsets_are_clustered([(0, 10), (0, 10), (10, 4)]));
    }

    /// F8 (#506 review): `set_expect_clustered(true)` pins a real ordering
    /// contract, not just documentation -- two out-of-order adds must trip
    /// the `debug_assert!` in `note_add`. Debug-only: with
    /// assertions disabled the panic never fires (see [`Self::write_archive`]'s
    /// `log::warn!` for the release-mode fallback signal instead), so this
    /// test only runs in a debug build.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "did not continue the ascending run")]
    fn expect_clustered_panics_on_out_of_order_adds() {
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");
        writer.set_expect_clustered(true);

        // z2 tile ids: (0,0)=5, (3,3)=15 (per `tile_id`'s own doc example).
        // Adding the higher id first, then a lower one, is strictly out of
        // order.
        writer.add_tile(2, 3, 3, &[1, 2, 3]).unwrap();
        let _ = writer.add_tile(2, 0, 0, &[4, 5, 6]);
    }

    // -------------------------------------------------------------------------
    // #516: PmtilesWriter dedup panics on out-of-order adds
    // -------------------------------------------------------------------------

    /// Offset-assignment regression for the two-pass rewrite (#516): the same
    /// tiles as `test_writer_dedup_run_length_consecutive` (every duplicate
    /// added after its carrier -- a "currently-working" input under the
    /// pre-#516 single-pass code too). A change here means the two-pass
    /// assignment altered the layout for an input the single-pass code
    /// already wrote correctly, which it must not.
    ///
    /// Pinned structurally -- the decoded directory tuples and the tile-data
    /// section bytes -- rather than as a whole-file length plus xxh3 (#516
    /// review, S4). A whole-file pin also fails whenever the metadata JSON or
    /// the compression library's output shifts by a byte, neither of which
    /// this test has anything to say about; what #516 touched is which offset
    /// each directory entry gets, and that is what is pinned.
    #[test]
    fn dedup_two_pass_offset_assignment_preserves_layout_for_existing_working_input() {
        let mut writer = PmtilesWriter::new();
        writer.enable_deduplication(true);
        let tile = vec![0x1a, 0x10];
        writer.add_tile(1, 0, 0, &tile).unwrap();
        writer.add_tile(1, 0, 1, &tile).unwrap();
        writer.add_tile(1, 1, 1, &tile).unwrap();
        writer.add_tile(1, 1, 0, &tile).unwrap();

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        writer.write_to_file(path).unwrap();
        let bytes = fs::read(path).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();

        // The four z1 tile ids are 1..=4 and all four share one blob at
        // offset 0, so the directory collapses to a single run-length-4
        // entry -- exactly what the pre-#516 single-pass code produced.
        let expected_blob = compression::compress(&tile, Compression::Gzip).unwrap();
        let entries = read_all_entries(&bytes, &header).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.tile_id, e.offset, e.length, e.run_length))
                .collect::<Vec<_>>(),
            vec![(1u64, 0u64, expected_blob.len() as u32, 4u32)],
            "one blob at offset 0, addressed by a single run of four consecutive tile ids"
        );

        let tile_data = &bytes[header.tile_data_offset as usize
            ..(header.tile_data_offset + header.tile_data_length) as usize];
        assert_eq!(
            tile_data,
            &expected_blob[..],
            "the tile-data section must be exactly the one shared compressed blob"
        );
        assert_eq!(header.addressed_tiles_count, 4);
        assert_eq!(header.tile_entries_count, 1);
        assert_eq!(header.tile_contents_count, 1);
    }

    /// #516 repro: `hash_to_offset.get(&entry.hash).expect("Hash must exist")`
    /// panicked when a duplicate's tile_id was lower than the tile_id of the
    /// tile that carries the bytes, because `self.tiles` (a `BTreeMap`) is
    /// walked in tile_id order and the single pass had not yet recorded the
    /// carrier's offset when it reached the duplicate. `add_tile(2,3,3,...)`
    /// then `add_tile(0,0,0,...)` with identical content is exactly that:
    /// the second add's tile_id (0) is lower than the first's, but the first
    /// add is the one that carries the bytes (it was added -- and hashed --
    /// first).
    #[test]
    fn dedup_survives_duplicate_with_lower_tile_id_than_its_carrier() {
        let mut writer = PmtilesWriter::new();
        writer.enable_deduplication(true);

        writer.add_tile(2, 3, 3, b"shared").unwrap();
        writer.add_tile(0, 0, 0, b"shared").unwrap();

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        writer.write_to_file(path).expect("must not panic (#516)");

        let stats = writer.dedup_stats();
        assert_eq!(stats.total_tiles, 2);
        assert_eq!(
            stats.unique_tiles, 1,
            "identical content must collapse to one stored blob"
        );
        assert_eq!(stats.duplicates_eliminated, 1);

        let bytes = fs::read(path).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();

        assert_eq!(header.addressed_tiles_count, 2, "both tiles are addressed");
        assert_eq!(
            header.tile_contents_count, 1,
            "only one unique blob is stored, regardless of which tile_id carried it"
        );
        let actually_clustered = verify_clustered(path).unwrap();
        assert!(
            actually_clustered,
            "a single unique blob referenced by two tile_ids is trivially clustered"
        );
        assert_eq!(
            header.clustered, actually_clustered,
            "the header's own clustered claim must match the independent, file-side check"
        );
    }

    // -------------------------------------------------------------------------
    // #516 review: pass-1 carrier clobber, strict predicate, O(1) clustered
    // -------------------------------------------------------------------------

    /// The predicate must reject a back-reference that names an offset it has
    /// seen but not the whole tile stored there (#516 review, S3-3). Before
    /// this, `[(0, 10), (0, 25)]` verified as clustered: the second entry's
    /// offset was in the seen set, and nothing checked that 25 bytes from
    /// offset 0 is a different (and, at 25 > 10, out-of-bounds) blob than the
    /// 10-byte tile actually written there. The doc comment already promised
    /// "a whole prior tile"; now the code enforces it.
    #[test]
    fn offsets_are_clustered_rejects_backreference_with_mismatched_length() {
        // Same offset, longer than the tile actually stored there.
        assert!(!offsets_are_clustered([(0, 10), (0, 25)]));
        // Same offset, shorter -- a prefix of a prior tile is not a tile.
        assert!(!offsets_are_clustered([(0, 10), (10, 5), (10, 999)]));
        // An exact whole-tile back-reference, then a fresh append: still fine.
        assert!(offsets_are_clustered([(0, 10), (0, 10), (10, 4)]));
    }

    /// Two carriers for one hash must not each write their bytes (#516
    /// review, S2). `add_tile_compressed` never consults the dedup cache, so
    /// two identical pre-compressed blobs both arrive as `TileEntry`s holding
    /// data. Pass 1 used to `insert` per carrier, last write winning: the
    /// first blob's bytes stayed in the tile-data section with nothing
    /// pointing at them, and `tile_contents_count` (2) outran the directory's
    /// distinct offset count (1) -- which go-pmtiles `verify` rejects with a
    /// hard error, not a warning. First-carrier-wins is strictly better than
    /// the pre-#516 behaviour: it actually deduplicates the second blob.
    #[test]
    fn dedup_pass_one_stores_shared_bytes_once_across_carriers() {
        let mut writer = PmtilesWriter::new();
        writer.enable_deduplication(true);
        let blob = compression::compress(b"identical tile bytes", Compression::Gzip).unwrap();
        writer.add_tile_compressed(1, 0, 0, blob.clone()).unwrap();
        writer.add_tile_compressed(1, 1, 0, blob.clone()).unwrap();

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        writer.write_to_file(path).unwrap();
        let bytes = fs::read(path).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();

        // z1 tile ids 1 and 4 are not consecutive, so no run-length merge --
        // two entries sharing the single blob at offset 0.
        let entries = read_all_entries(&bytes, &header).unwrap();
        let len = blob.len() as u32;
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.tile_id, e.offset, e.length, e.run_length))
                .collect::<Vec<_>>(),
            vec![(1u64, 0u64, len, 1u32), (4u64, 0u64, len, 1u32)],
            "both carriers must reference the one stored blob"
        );

        assert_eq!(
            header.tile_data_length,
            blob.len() as u64,
            "the second carrier's bytes must not be appended as dead weight"
        );
        let distinct: HashSet<u64> = entries.iter().map(|e| e.offset).collect();
        assert_eq!(
            distinct.len() as u64,
            header.tile_contents_count,
            "go-pmtiles `verify` hard-errors when TileContentsCount disagrees with \
             the directory's distinct referenced offsets (pmtiles/verify.go:148-150)"
        );
        assert_eq!(header.tile_contents_count, 1);
        assert_eq!(header.addressed_tiles_count, 2);
        assert!(header.clustered, "one blob, referenced twice, is clustered");
        assert!(
            verify_clustered(path).unwrap(),
            "the file-side re-derivation must agree with the header"
        );
    }

    /// Re-adding a tile id with different content replaces the only
    /// `TileEntry` that carried an earlier hash's bytes, orphaning the
    /// duplicate still pointing at it. That used to `expect`-panic in pass 2
    /// (#516 review, S3-1); `write_to_file` already returns `Result`, so it
    /// must report the orphan rather than abort the process.
    #[test]
    fn dedup_errors_when_a_carrier_tile_is_overwritten() {
        let mut writer = PmtilesWriter::new();
        writer.enable_deduplication(true);

        writer.add_tile(0, 0, 0, b"XXXX").unwrap(); // carries hash(XXXX)
        writer.add_tile(1, 0, 0, b"XXXX").unwrap(); // duplicate, no data
        writer.add_tile(0, 0, 0, b"YYYY").unwrap(); // replaces the carrier

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let err = writer
            .write_to_file(tmp.path())
            .expect_err("the orphaned hash must surface as an error, not a panic");
        let msg = err.to_string();
        assert!(
            msg.contains("carrier"),
            "the error must name the cause (an overwritten carrier tile), got: {msg}"
        );
    }

    /// The scenario the pass-2 comment describes, pinned (#516 review, S3-6):
    /// a duplicate whose carrier sorts *after* it, interleaved with a second
    /// hash whose carrier sorts between them. `(2,3,3)` carries `X`, `(1,0,0)`
    /// carries `Y`, `(0,0,0)` duplicates `X`. In tile-id order the directory
    /// reads `[0 -> offset(X), 1 -> offset(Y)=0, 15 -> offset(X)]`, whose very
    /// first entry already points past the frontier: legitimately unclustered,
    /// and both the header and the file-side check must say so rather than
    /// claim otherwise. The archive is still byte-correct -- every tile serves
    /// its own content -- it just costs a reader a backward seek.
    #[test]
    fn interleaved_carriers_produce_an_honestly_unclustered_archive() {
        let mut writer = PmtilesWriter::new();
        writer.enable_deduplication(true);

        writer.add_tile(2, 3, 3, b"XXXXXXXX").unwrap();
        writer.add_tile(1, 0, 0, b"YYYYYYYY").unwrap();
        writer.add_tile(0, 0, 0, b"XXXXXXXX").unwrap();

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path();
        writer.write_to_file(path).unwrap();
        let bytes = fs::read(path).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();

        assert!(
            !header.clustered,
            "the header must not claim clustered for an interleaved dedup layout"
        );
        assert!(
            !verify_clustered(path).unwrap(),
            "and the independent file-side check must agree"
        );

        // Byte correctness is untouched: each tile id still serves its own
        // content, unclustered or not.
        let entries = read_all_entries(&bytes, &header).unwrap();
        let expected: Vec<(u64, &[u8])> = vec![
            (tile_id(0, 0, 0), b"XXXXXXXX".as_slice()),
            (tile_id(1, 0, 0), b"YYYYYYYY".as_slice()),
            (tile_id(2, 3, 3), b"XXXXXXXX".as_slice()),
        ];
        assert_eq!(entries.len(), expected.len());
        for (e, (want_id, want_bytes)) in entries.iter().zip(expected) {
            assert_eq!(e.tile_id, want_id);
            let start = (header.tile_data_offset + e.offset) as usize;
            let raw = &bytes[start..start + e.length as usize];
            let served = compression::decompress(raw, header.tile_compression).unwrap();
            assert_eq!(
                served, want_bytes,
                "tile id {} must serve its own content",
                e.tile_id
            );
        }
    }

    /// Ascending adds imply a clustered archive by construction, so
    /// `write_archive` reads the tracked flag instead of building the
    /// predicate's offset map -- ~2.4 GB of it at planet scale, at peak RSS
    /// (#516 review, S3-4). The derived header byte must be identical either
    /// way, which the file-side `verify_clustered` (which always runs the
    /// predicate) confirms here.
    #[test]
    fn streaming_writer_derives_clustered_from_ascending_adds() {
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");

        // Ascending tile ids: z0 (0,0)=0, z1 (0,0)=1, z1 (0,1)=2, z1 (1,1)=3.
        // The third add repeats the first's content, so the directory carries
        // a dedup back-reference to offset 0 -- the case the predicate exists
        // for, and the one the fast path must still get right.
        writer.add_tile(0, 0, 0, b"aaaa").unwrap();
        writer.add_tile(1, 0, 0, b"bbbb").unwrap();
        writer.add_tile(1, 0, 1, b"aaaa").unwrap();
        writer.add_tile(1, 1, 1, b"cccc").unwrap();
        assert!(
            writer.adds_ascending,
            "every add continued the ascending run"
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        writer.finalize(&path).unwrap();

        let bytes = fs::read(&path).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();
        assert!(header.clustered, "ascending adds are clustered");
        assert!(
            verify_clustered(&path).unwrap(),
            "and the predicate, re-derived from disk, must reach the same answer \
             the O(1) fast path did"
        );
    }

    /// The counterpart: an out-of-order caller clears `adds_ascending`, so the
    /// flag short-circuit cannot fire and the honest predicate decides. It
    /// must derive `false` here, not inherit an optimistic default.
    #[test]
    fn streaming_writer_out_of_order_adds_fall_back_to_the_predicate() {
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        writer.set_layer_name("test");

        // z2 (3,3)=15 added before z1 (0,0)=1: the bytes land in add order,
        // so after the tile_id sort the first entry points past the frontier.
        writer.add_tile(2, 3, 3, b"aaaa").unwrap();
        writer.add_tile(1, 0, 0, b"bbbb").unwrap();
        assert!(
            !writer.adds_ascending,
            "a descending add must clear the fast-path flag"
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        writer.finalize(&path).unwrap();

        let bytes = fs::read(&path).unwrap();
        let header = Header::from_bytes(&bytes).unwrap();
        assert!(
            !header.clustered,
            "the fallback predicate must report the archive as it is"
        );
        assert!(!verify_clustered(&path).unwrap(), "and the file agrees");
    }
}
