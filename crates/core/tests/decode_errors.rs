//! Error reporting for `decode_pmtiles` on damaged archives (#381).
//!
//! A decompression failure must say *which* section failed and where, so a
//! broken archive can be diagnosed from the message alone instead of by
//! hand-parsing it.
//!
//! Run with:
//!   cargo test --package tylertoo-core --test decode_errors -- --nocapture

use std::path::PathBuf;

use tylertoo_core::decode::{decode_pmtiles, DecodeError, DecodeOptions};
use tylertoo_core::pmtiles_writer::Header;
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
