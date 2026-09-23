//! Error reporting for `decode_pmtiles` on damaged archives (#381).
//!
//! A decompression failure must say *which* section failed and where, so a
//! broken archive can be diagnosed from the message alone instead of by
//! hand-parsing it.
//!
//! Run with:
//!   cargo test --package tylertoo-core --test decode_errors -- --nocapture

use std::path::PathBuf;

use tylertoo_core::compression::compress;
use tylertoo_core::decode::{decode_pmtiles, DecodeError, DecodeOptions};
use tylertoo_core::pmtiles_writer::{encode_directory, DirEntry, Header};
use tylertoo_core::{Compression, StreamingPmtilesWriter};

/// A minimal MVT tile: one layer `t` with one point feature at (1, 1).
const POINT_TILE: &[u8] = &[
    0x1A, 0x11, // Tile.layers, 17 bytes
    0x0A, 0x01, b't', // name
    0x12, 0x07, 0x18, 0x01, 0x22, 0x03, 0x09, 0x02, 0x02, // feature: POINT, MoveTo(1,1)
    0x28, 0x80, 0x20, // extent 4096
    0x78, 0x02, // version 2
];

/// Write a small gzip archive with a few tiles at z5 and return its path
/// plus its bytes.
fn small_archive(dir: &tempfile::TempDir) -> (PathBuf, Vec<u8>) {
    let path = dir.path().join("ok.pmtiles");
    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
    writer.set_layer_name("t");
    for i in 0..8u32 {
        // Vary the payload so tiles are not deduplicated into one run.
        let mut tile = POINT_TILE.to_vec();
        tile[11] = 0x02 + 2 * (i as u8); // MoveTo x = 1 + i
        writer.add_tile(5, i, 7, &tile).unwrap();
    }
    writer.finalize(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    (path, bytes)
}

fn decode_err(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> DecodeError {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    let out = dir.path().join(format!("{name}.parquet"));
    decode_pmtiles(&path, &out, &DecodeOptions::default())
        .expect_err("damaged archive must fail to decode")
}

#[test]
fn directory_decompression_failure_names_section_and_offset() {
    let dir = tempfile::tempdir().unwrap();
    let (_, mut bytes) = small_archive(&dir);
    let header = Header::from_bytes(&bytes[..127]).unwrap();

    // Truncate the root directory's gzip stream by shortening its length in
    // the header — the same failure shape as #377 ("incomplete deflate
    // stream"), reproduced without a leaf directory.
    let damaged = Header {
        root_dir_length: header.root_dir_length / 2,
        ..header
    };
    bytes[..127].copy_from_slice(&damaged.to_bytes());

    let err = decode_err(&dir, "bad-root.pmtiles", &bytes);
    let msg = err.to_string();
    assert!(
        msg.contains("root directory"),
        "message must name the section, got: {msg}"
    );
    assert!(
        msg.contains(&format!("offset {}", header.root_dir_offset)),
        "message must give the byte offset, got: {msg}"
    );
    assert!(
        msg.contains(&format!("{} bytes", damaged.root_dir_length)),
        "message must give the length, got: {msg}"
    );
    assert!(
        msg.contains("deflate") || msg.contains("gzip") || msg.contains("corrupt"),
        "message must keep the underlying decompression error, got: {msg}"
    );
}

#[test]
fn tile_decompression_failure_names_tile_and_offset() {
    let dir = tempfile::tempdir().unwrap();
    let (_, mut bytes) = small_archive(&dir);
    let header = Header::from_bytes(&bytes[..127]).unwrap();

    // Corrupt the gzip header of the first tile in the tile-data section.
    let first_tile = header.tile_data_offset as usize;
    assert_eq!(bytes[first_tile], 0x1f, "expected a gzip magic byte");
    bytes[first_tile] = 0x00;

    let err = decode_err(&dir, "bad-tile.pmtiles", &bytes);
    let msg = err.to_string();
    assert!(
        msg.contains("tile z5/"),
        "message must name the tile, got: {msg}"
    );
    assert!(
        msg.contains(&format!("offset {first_tile}")),
        "message must give the byte offset, got: {msg}"
    );
}

/// Append a hand-built root directory (and optionally a leaf directory) to a
/// valid archive and repoint the header at them. Every other section stays
/// where it was, so only the directory content is hostile.
fn with_directories(bytes: &[u8], root: &[DirEntry], leaf: Option<&[DirEntry]>) -> Vec<u8> {
    let header = Header::from_bytes(&bytes[..127]).unwrap();
    let mut out = bytes.to_vec();
    let mut header = header;
    if let Some(leaf) = leaf {
        let enc = compress(&encode_directory(leaf), header.internal_compression).unwrap();
        header.leaf_dirs_offset = out.len() as u64;
        header.leaf_dirs_length = enc.len() as u64;
        out.extend_from_slice(&enc);
    }
    let enc = compress(&encode_directory(root), header.internal_compression).unwrap();
    header.root_dir_offset = out.len() as u64;
    header.root_dir_length = enc.len() as u64;
    out.extend_from_slice(&enc);
    out[..127].copy_from_slice(&header.to_bytes());
    out
}

#[test]
fn hostile_leaf_offset_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    // A leaf pointer whose offset wraps u64 when added to the section base.
    let root = [DirEntry {
        tile_id: 0,
        offset: u64::MAX - 64,
        length: 10,
        run_length: 0,
    }];
    let err = decode_err(
        &dir,
        "hostile-leaf.pmtiles",
        &with_directories(&bytes, &root, None),
    );
    assert!(
        matches!(&err, DecodeError::InvalidArchive(m) if m.contains("leaf directory")),
        "expected an InvalidArchive naming the leaf directory, got: {err}"
    );
}

#[test]
fn hostile_tile_offset_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    let root = [DirEntry {
        tile_id: 0,
        offset: u64::MAX - 64,
        length: 10,
        run_length: 1,
    }];
    let err = decode_err(
        &dir,
        "hostile-tile.pmtiles",
        &with_directories(&bytes, &root, None),
    );
    assert!(
        matches!(&err, DecodeError::InvalidArchive(m) if m.contains("tile data")),
        "expected an InvalidArchive naming the tile data, got: {err}"
    );
}

#[test]
fn nested_leaf_directories_are_rejected_not_decoded_as_tiles() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    // The leaf directory itself holds a leaf pointer (run_length 0). PMTiles
    // v3 allows this shape but this decoder does not walk it; it must say so
    // rather than slice directory bytes as a tile.
    let leaf = [DirEntry {
        tile_id: 0,
        offset: 0,
        length: 10,
        run_length: 0,
    }];
    let enc_len = compress(&encode_directory(&leaf), Compression::Gzip)
        .unwrap()
        .len() as u32;
    let root = [DirEntry {
        tile_id: 0,
        offset: 0,
        length: enc_len,
        run_length: 0,
    }];
    let err = decode_err(
        &dir,
        "nested-leaf.pmtiles",
        &with_directories(&bytes, &root, Some(&leaf)),
    );
    let msg = err.to_string();
    assert!(
        matches!(&err, DecodeError::InvalidArchive(_)) && msg.contains("leaf"),
        "expected an InvalidArchive about nested leaf directories, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Hostile archives: resource exhaustion (#417)
// ---------------------------------------------------------------------------

/// Repoint the header's root directory at `raw` bytes appended to the archive.
fn with_raw_root(bytes: &[u8], raw: &[u8]) -> Vec<u8> {
    let mut header = Header::from_bytes(&bytes[..127]).unwrap();
    let mut out = bytes.to_vec();
    header.root_dir_offset = out.len() as u64;
    header.root_dir_length = raw.len() as u64;
    out.extend_from_slice(raw);
    out[..127].copy_from_slice(&header.to_bytes());
    out
}

#[test]
fn hostile_run_length_is_rejected_before_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    // The archive tops out at z5, whose whole address space is 1365 tile ids.
    // A single entry claiming a million-tile run is not a real archive; it is
    // 32 MB of TileRefs (and, at u32::MAX, 137 GB).
    let root = [DirEntry {
        tile_id: 0,
        offset: 0,
        length: 0,
        run_length: 1_000_000,
    }];
    let err = decode_err(
        &dir,
        "hostile-run.pmtiles",
        &with_directories(&bytes, &root, None),
    );
    assert!(
        matches!(&err, DecodeError::InvalidArchive(m) if m.contains("run-length expansion")),
        "expected an InvalidArchive about run-length expansion, got: {err}"
    );
}

#[test]
fn run_length_expansion_total_is_capped_across_entries() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    // Each run is individually plausible; together they address more tiles
    // than the archive's zoom range can hold.
    let root: Vec<DirEntry> = (0..3)
        .map(|i| DirEntry {
            tile_id: i * 700,
            offset: 0,
            length: 0,
            run_length: 700,
        })
        .collect();
    let err = decode_err(
        &dir,
        "hostile-run-total.pmtiles",
        &with_directories(&bytes, &root, None),
    );
    assert!(
        matches!(&err, DecodeError::InvalidArchive(m) if m.contains("run-length expansion")),
        "expected an InvalidArchive about run-length expansion, got: {err}"
    );
}

#[test]
fn directory_decompression_bomb_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    let header = Header::from_bytes(&bytes[..127]).unwrap();
    // 32 MiB of zeros gzips to a few tens of KB: the classic bomb shape, and
    // far past any directory a real archive carries.
    let bomb = compress(&vec![0u8; 32 * 1024 * 1024], header.internal_compression).unwrap();
    assert!(
        bomb.len() < 256 * 1024,
        "the bomb must be small on disk, got {} bytes",
        bomb.len()
    );
    let err = decode_err(&dir, "bomb.pmtiles", &with_raw_root(&bytes, &bomb));
    let msg = err.to_string();
    assert!(
        msg.contains("root directory") && msg.contains("exceeds"),
        "expected a capped-decompression error naming the section, got: {msg}"
    );
}
