//! Error reporting for `decode_pmtiles` on damaged archives (#381).
//!
//! A decompression failure must say *which* section failed and where, so a
//! broken archive can be diagnosed from the message alone instead of by
//! hand-parsing it.
//!
//! Run with:
//!   cargo test --package tylertoo-core --test `decode_errors` -- --nocapture

use std::path::PathBuf;

use tylertoo_core::archive_index::ArchiveIndex;
use tylertoo_core::compression::{compress, MAX_INTERNAL_BYTES};
use tylertoo_core::decode::{decode_pmtiles, DecodeError, DecodeOptions};
use tylertoo_core::pmtiles_writer::{
    encode_directory, tile_id, DirEntry, Header, MAX_LEAF_DIRECTORIES,
};
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

/// The error and every `source()` beneath it, joined the way anyhow's `{:#}`
/// joins them. `DecodeError`'s own `Display` leaves the cause to the chain
/// (#396), so assertions about the underlying codec error read this.
fn chain(err: &DecodeError) -> String {
    let mut parts = vec![err.to_string()];
    let mut cur: Option<&dyn std::error::Error> = std::error::Error::source(err);
    while let Some(e) = cur {
        parts.push(e.to_string());
        cur = e.source();
    }
    parts.join(": ")
}

/// #396: a variant that carries a `source` must not also print it in its own
/// `Display`, or the CLI's anyhow chain shows the cause twice
/// (`…: incomplete deflate stream` then `Caused by: incomplete deflate stream`).
fn assert_cause_printed_once(err: &DecodeError) {
    let source = std::error::Error::source(err)
        .unwrap_or_else(|| panic!("{err:?} must expose its cause as source()"));
    let cause = source.to_string();
    assert!(
        !err.to_string().contains(&cause),
        "Display repeats the source; the chain would print {cause:?} twice: {err}"
    );
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
    let full = chain(&err);
    assert!(
        full.contains("deflate") || full.contains("gzip") || full.contains("corrupt"),
        "the chain must keep the underlying decompression error, got: {full}"
    );
    assert!(
        matches!(&err, DecodeError::Decompress { .. }),
        "got: {err:?}"
    );
    assert_cause_printed_once(&err);
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
    assert!(
        matches!(&err, DecodeError::TileDecompress { .. }),
        "got: {err:?}"
    );
    assert_cause_printed_once(&err);
}

#[test]
fn undecodable_mvt_names_the_tile_and_prints_the_cause_once() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    let mut header = Header::from_bytes(&bytes[..127]).unwrap();

    // Replace the tile data with one gzip stream that inflates fine but holds
    // a truncated protobuf: field 3 (layers) claims 0x7F bytes, none follow.
    let bad = compress(&[0x1A, 0x7F], header.tile_compression).unwrap();
    let mut out = bytes[..header.tile_data_offset as usize].to_vec();
    out.extend_from_slice(&bad);
    let root = [DirEntry {
        tile_id: tile_id(5, 0, 7),
        offset: 0,
        length: bad.len() as u32,
        run_length: 1,
    }];
    let enc = compress(&encode_directory(&root), header.internal_compression).unwrap();
    header.tile_data_length = bad.len() as u64;
    header.root_dir_offset = out.len() as u64;
    header.root_dir_length = enc.len() as u64;
    out.extend_from_slice(&enc);
    out[..127].copy_from_slice(&header.to_bytes());

    let err = decode_err(&dir, "bad-mvt.pmtiles", &out);
    assert!(
        matches!(
            &err,
            DecodeError::Mvt {
                z: 5,
                x: 0,
                y: 7,
                ..
            }
        ),
        "expected an MVT decode error for z5/0/7, got: {err:?}"
    );
    assert_cause_printed_once(&err);
}

#[test]
fn missing_input_prints_the_io_cause_once() {
    let dir = tempfile::tempdir().unwrap();
    let err = decode_pmtiles(
        dir.path().join("absent.pmtiles"),
        dir.path().join("out.parquet"),
        &DecodeOptions::default(),
    )
    .expect_err("a missing input must fail");
    assert!(matches!(&err, DecodeError::Io(_)), "got: {err:?}");
    assert_cause_printed_once(&err);
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
    let msg = chain(&err);
    assert!(
        msg.contains("root directory") && msg.contains("exceeds"),
        "expected a capped-decompression error naming the section, got: {msg}"
    );
}

/// #510 review, S2-3: `ArchiveIndex::open` sized its reads from lengths the
/// archive declares, bounded only by the file's own length. A header claiming
/// a 256 MiB metadata block (or root directory) made `open` allocate that
/// much — per input, and `tylertoo merge` opens every shard at once — and
/// then SUCCEED, holding the block raw for the index's lifetime. Both reads
/// are capped at `MAX_INTERNAL_BYTES` before the allocation, with #417's
/// wording.
///
/// The oversized region is a sparse tail, so the probe costs no disk.
fn with_sparse_tail(dir: &tempfile::TempDir, name: &str, bytes: &[u8], extra: u64) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.set_len(bytes.len() as u64 + extra).unwrap();
    path
}

#[test]
fn archive_index_metadata_read_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    let mut header = Header::from_bytes(&bytes[..127]).unwrap();
    const HUGE: u64 = 256 * 1024 * 1024;
    header.json_metadata_offset = bytes.len() as u64;
    header.json_metadata_length = HUGE;
    let mut out = bytes.clone();
    out[..127].copy_from_slice(&header.to_bytes());
    let path = with_sparse_tail(&dir, "huge-metadata.pmtiles", &out, HUGE);

    let err = match ArchiveIndex::open(&path) {
        Ok(_) => panic!("a 256 MiB metadata block must be refused, not allocated"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("metadata") && err.contains("exceeds"),
        "expected a capped-read error naming the section, got: {err}"
    );
    assert!(
        err.contains(&MAX_INTERNAL_BYTES.to_string()),
        "the message must give the ceiling, got: {err}"
    );
}

#[test]
fn archive_index_directory_read_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    let mut header = Header::from_bytes(&bytes[..127]).unwrap();
    const HUGE: u64 = 256 * 1024 * 1024;
    header.root_dir_offset = bytes.len() as u64;
    header.root_dir_length = HUGE;
    let mut out = bytes.clone();
    out[..127].copy_from_slice(&header.to_bytes());
    let path = with_sparse_tail(&dir, "huge-root.pmtiles", &out, HUGE);

    let err = match ArchiveIndex::open(&path) {
        Ok(_) => panic!("a 256 MiB root directory must be refused, not allocated"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("root dir") && err.contains("exceeds"),
        "expected a capped-read error naming the section, got: {err}"
    );
}

#[test]
fn tile_body_decompression_bomb_is_capped() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    let header = Header::from_bytes(&bytes[..127]).unwrap();

    // A tile *body* that gzips to a few tens of KB and expands past
    // MAX_TILE_BYTES (64 MiB). The directory ceiling does not cover this
    // path: tile bodies are decompressed with their own, larger ceiling.
    let bomb = compress(&vec![0u8; 80 * 1024 * 1024], header.tile_compression).unwrap();
    assert!(
        bomb.len() < 512 * 1024,
        "the bomb must be small on disk, got {} bytes",
        bomb.len()
    );

    // Append the bomb as the archive's tile data and point one entry at it.
    let mut out = bytes.to_vec();
    let mut header = header;
    header.tile_data_offset = out.len() as u64;
    header.tile_data_length = bomb.len() as u64;
    out.extend_from_slice(&bomb);
    let root = [DirEntry {
        tile_id: 0,
        offset: 0,
        length: bomb.len() as u32,
        run_length: 1,
    }];
    let enc = compress(&encode_directory(&root), header.internal_compression).unwrap();
    header.root_dir_offset = out.len() as u64;
    header.root_dir_length = enc.len() as u64;
    out.extend_from_slice(&enc);
    out[..127].copy_from_slice(&header.to_bytes());

    let err = decode_err(&dir, "tile-bomb.pmtiles", &out);
    let msg = chain(&err);
    assert!(
        matches!(&err, DecodeError::TileDecompress { .. }) && msg.contains("exceeds"),
        "expected a capped tile decompression naming the tile, got: {msg}"
    );
}

#[test]
fn truncation_mid_varint_in_a_directory_column_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);
    let header = Header::from_bytes(&bytes[..127]).unwrap();

    // A directory whose last column (offsets) ends in a continuation byte
    // with nothing after it. `decode_varint` must run out of input and the
    // walker must report rather than index past the buffer.
    let root = [DirEntry {
        tile_id: 1,
        offset: 0,
        length: 4,
        run_length: 1,
    }];
    let mut body = encode_directory(&root);
    body.pop();
    body.push(0x80); // continuation bit set, no successor byte
    let raw = compress(&body, header.internal_compression).unwrap();

    let err = decode_err(
        &dir,
        "truncated-varint.pmtiles",
        &with_raw_root(&bytes, &raw),
    );
    assert!(
        matches!(&err, DecodeError::InvalidArchive(m) if m.contains("undecodable root directory")),
        "expected an undecodable-directory error, got: {err}"
    );
}

#[test]
fn unknown_internal_compression_in_the_header_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_, mut bytes) = small_archive(&dir);

    // Code 0 is `Compression::Unknown` — in spec range, but every codec call
    // fails on it. The header parse rejects it, where the message can name
    // the field.
    bytes[97] = 0;
    let err = decode_err(&dir, "unknown-internal.pmtiles", &bytes);
    let msg = err.to_string();
    assert!(
        msg.contains("internal compression"),
        "expected the header parse to name the field, got: {msg}"
    );

    let (_, mut bytes) = small_archive(&dir);
    bytes[98] = 0; // tile_compression
    let err = decode_err(&dir, "unknown-tile.pmtiles", &bytes);
    let msg = err.to_string();
    assert!(
        msg.contains("tile compression"),
        "expected the header parse to name the field, got: {msg}"
    );
}

#[test]
fn leaf_flattening_is_capped_before_entries_accumulate() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);

    // Every root pointer aims at the *same* leaf body. Each costs ~5 bytes on
    // disk and yields a whole leaf's worth of entries, so the walk used to
    // accumulate without limit. The archive tops out at z5 (1365 addressable
    // tile ids), so 4 x 400 is already past what it could address.
    let leaf: Vec<DirEntry> = (0..400)
        .map(|i| DirEntry {
            tile_id: i + 1,
            offset: 0,
            length: 4,
            run_length: 1,
        })
        .collect();
    let leaf_len = compress(&encode_directory(&leaf), Compression::Gzip)
        .unwrap()
        .len() as u32;
    let root: Vec<DirEntry> = (0..4)
        .map(|_| DirEntry {
            tile_id: 0,
            offset: 0,
            length: leaf_len,
            run_length: 0,
        })
        .collect();

    let err = decode_err(
        &dir,
        "leaf-flattening.pmtiles",
        &with_directories(&bytes, &root, Some(&leaf)),
    );
    assert!(
        matches!(&err, DecodeError::InvalidArchive(m) if m.contains("directory entries")),
        "expected an entry-budget error, got: {err}"
    );
}

#[test]
fn a_root_full_of_empty_leaf_pointers_is_capped_by_leaf_count() {
    let dir = tempfile::tempdir().unwrap();
    let (_, bytes) = small_archive(&dir);

    // An *empty* leaf costs nothing against the entry budget while still
    // costing a bounded decompression apiece — the second half of the attack,
    // and why the leaf count is capped separately.
    let leaf: Vec<DirEntry> = Vec::new();
    let leaf_len = compress(&encode_directory(&leaf), Compression::Gzip)
        .unwrap()
        .len() as u32;
    let root: Vec<DirEntry> = (0..=MAX_LEAF_DIRECTORIES)
        .map(|_| DirEntry {
            tile_id: 0,
            offset: 0,
            length: leaf_len,
            run_length: 0,
        })
        .collect();

    let err = decode_err(
        &dir,
        "many-leaves.pmtiles",
        &with_directories(&bytes, &root, Some(&leaf)),
    );
    assert!(
        matches!(&err, DecodeError::InvalidArchive(m) if m.contains("leaf directories")),
        "expected a leaf-count error, got: {err}"
    );
}

/// [`POINT_TILE`] with the layer declaring `extent = 0` (field 5, varint 0).
const EXTENT_ZERO_TILE: &[u8] = &[
    0x1A, 0x10, // Tile.layers, 16 bytes
    0x0A, 0x01, b't', // name
    0x12, 0x07, 0x18, 0x01, 0x22, 0x03, 0x09, 0x02, 0x02, // feature: POINT, MoveTo(1,1)
    0x28, 0x00, // extent 0
    0x78, 0x02, // version 2
];

/// #433: a layer declaring `extent = 0` used to decode: every tile-local unit
/// is `tile_width / extent`, so each coordinate came out as ±inf (or NaN at
/// the origin) and went straight into the output parquet. Refused, naming
/// the tile and the layer, before any coordinate is transformed.
#[test]
fn a_layer_declaring_extent_zero_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("extent-zero.pmtiles");
    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
    writer.set_layer_name("t");
    writer.add_tile(5, 3, 7, EXTENT_ZERO_TILE).unwrap();
    writer.finalize(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();

    let err = decode_err(&dir, "extent-zero-copy.pmtiles", &bytes);
    assert!(
        matches!(&err, DecodeError::InvalidArchive(m) if m.contains("extent 0")),
        "expected an extent-0 rejection, got: {err}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("z5/3/7") && msg.contains("\"t\""),
        "message must name the tile and the layer, got: {msg}"
    );
}

/// An inverted zoom range is a caller mistake, not an empty selection: it
/// fails before the archive is read, and writes nothing (#445 — the check
/// used to live only in the CLI, so library and Python callers silently got
/// an empty file).
#[test]
fn inverted_zoom_range_is_rejected_before_any_work() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = small_archive(&dir);
    let out = dir.path().join("out.parquet");
    let options = DecodeOptions {
        min_zoom: Some(8),
        max_zoom: Some(5),
        layer: None,
    };
    let err = decode_pmtiles(&path, &out, &options).expect_err("inverted range must fail");
    assert!(
        matches!(
            err,
            DecodeError::InvalidZoomRange {
                min_zoom: 8,
                max_zoom: 5
            }
        ),
        "{err:?}"
    );
    assert_eq!(err.to_string(), "minimum zoom 8 exceeds maximum zoom 5");
    assert!(!out.exists());

    // Checked before the input is opened: a missing file still reports the
    // range, not an I/O error.
    let missing = dir.path().join("missing.pmtiles");
    assert!(matches!(
        decode_pmtiles(&missing, &out, &options),
        Err(DecodeError::InvalidZoomRange { .. })
    ));
}

/// `min_zoom == max_zoom` is one zoom, not an inverted range. (z6 holds no
/// tiles in this archive, so the decode succeeds with nothing written; the
/// archive's z5 payloads exist to exercise the error paths above.)
#[test]
fn equal_zoom_bounds_are_one_zoom_not_an_inverted_range() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = small_archive(&dir);
    let out = dir.path().join("out.parquet");
    let options = DecodeOptions {
        min_zoom: Some(6),
        max_zoom: Some(6),
        layer: None,
    };
    assert!(options.validate().is_ok());
    let report = decode_pmtiles(&path, &out, &options).unwrap();
    assert_eq!(report.tiles_read, 0);
    assert_eq!(report.zoom_range, None);
}

/// A missing input names its path somewhere in the error chain. The CLI
/// checks the input first, but a library or Python caller (#445) otherwise
/// got a bare "No such file or directory".
#[test]
fn a_missing_input_names_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.pmtiles");
    let out = dir.path().join("out.parquet");
    let err = decode_pmtiles(&missing, &out, &DecodeOptions::default()).unwrap_err();
    assert!(matches!(err, DecodeError::Io(_)), "{err:?}");
    let mut chain = err.to_string();
    let mut cause = std::error::Error::source(&err);
    while let Some(c) = cause {
        chain.push_str(&format!(": {c}"));
        cause = c.source();
    }
    assert!(chain.contains("absent.pmtiles"), "{chain}");
}
