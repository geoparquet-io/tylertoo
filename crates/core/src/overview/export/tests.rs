use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
use arrow_schema::{Field, Schema};
use geo::{Geometry, LineString, Point};
use geoarrow::{array::GeometryBuilder, datatypes::GeometryType};

use super::*;
use crate::{
    mvt::{command_decode, zigzag_decode},
    overview::{
        level::{gsd, ClusteringProvenance, CoalescingProvenance, Generalization, Level, Mode},
        writer::{LevelSpec, LevelWriteOutcome, OverviewWriter, OverviewWriterOptions},
    },
    vector_tile::{tile::GeomType, Tile},
};

// --- fixture builders ----------------------------------------------------

pub(super) fn build_geometry_array(geoms: &[Geometry<f64>]) -> geoarrow::array::GeometryArray {
    let typ = GeometryType::new(Default::default());
    let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(Some));
    b.finish()
}

pub(super) fn geometry_field() -> Field {
    build_geometry_array(&[Geometry::Point(Point::new(0.0, 0.0))])
        .data_type()
        .to_field("geometry", true)
}

fn source_schema() -> Schema {
    Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        geometry_field(),
    ])
}

fn batch(schema: &Arc<Schema>, ids: &[i64], geoms: &[Geometry<f64>]) -> RecordBatch {
    let id = Int64Array::from(ids.to_vec());
    let name = StringArray::from(ids.iter().map(|i| format!("f{i}")).collect::<Vec<_>>());
    let geom = build_geometry_array(geoms);
    RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(id), Arc::new(name), Arc::new(geom.to_array_ref())],
    )
    .unwrap()
}

fn write_mode_batches(
    path: &Path,
    mode: Mode,
    max_row_group_size: usize,
    batches: impl ExactSizeIterator<Item = RecordBatch>,
) -> OverviewsMeta {
    let specs = (0..batches.len())
        .map(|k| LevelSpec::new(gsd((2 + 2 * k) as u8), Some((2 + 2 * k) as u8)))
        .collect();
    let mut opts = OverviewWriterOptions::new(mode, specs);
    opts.max_row_group_size = max_row_group_size;
    let mut writer = OverviewWriter::create(path, &Arc::new(source_schema()), opts).unwrap();
    for (k, batch) in batches.enumerate() {
        assert_eq!(
            writer
                .write_level(k, Some(batch.num_rows()), std::iter::once(batch))
                .unwrap(),
            LevelWriteOutcome::Written
        );
    }
    writer.finish().unwrap()
}

/// Write a 2-level duplicating overview fixture. `level_geoms[k]` are the
/// geometries (and ids) at level k. Levels use z = 2 + 2k.
/// Write a one-batch-per-level overview file and return its metadata.
///
/// `write_fixture` and `write_partitioning_fixture` are the two modes
/// callers want; both delegate here so the level-spec ladder and the
/// write loop exist once.
fn write_mode_fixture(
    path: &Path,
    level_geoms: &[(Vec<i64>, Vec<Geometry<f64>>)],
    mode: Mode,
    max_row_group_size: usize,
) -> OverviewsMeta {
    let schema = Arc::new(source_schema());
    write_mode_batches(
        path,
        mode,
        max_row_group_size,
        level_geoms
            .iter()
            .map(|(ids, geoms)| batch(&schema, ids, geoms)),
    )
}

fn write_fixture(path: &Path, level_geoms: &[(Vec<i64>, Vec<Geometry<f64>>)]) -> OverviewsMeta {
    write_mode_fixture(path, level_geoms, Mode::Duplicating, 10_000)
}

/// Decode all (`geom_type`, coords, keys) of a raw MVT tile's single layer.
fn decode_tile(bytes: &[u8]) -> Tile {
    Tile::decode(bytes).unwrap()
}

/// Decode a feature's absolute tile-local coordinates from its command stream.
fn decode_coords(geom: &[u32]) -> Vec<(i32, i32)> {
    let mut coords = Vec::new();
    let (mut cx, mut cy) = (0i32, 0i32);
    let mut i = 0;
    while i < geom.len() {
        let (cmd, count) = command_decode(geom[i]);
        i += 1;
        if cmd == 7 {
            // ClosePath: no params.
            continue;
        }
        for _ in 0..count {
            cx += zigzag_decode(geom[i]);
            cy += zigzag_decode(geom[i + 1]);
            coords.push((cx, cy));
            i += 2;
        }
    }
    coords
}

// --- tests ---------------------------------------------------------------

fn equivalence_shapes() -> (Geometry<f64>, Geometry<f64>, Geometry<f64>) {
    let wide = Geometry::LineString(LineString::from(vec![
        (-100.0, 10.0),
        (-80.0, 12.0),
        (-60.0, 8.0),
        (-40.0, 11.0),
    ]));
    let concave = Geometry::Polygon(geo::Polygon::new(
        LineString::from(vec![
            (-100.0, 25.0),
            (-98.0, 25.0),
            (-98.0, 27.0),
            (-99.0, 25.5),
            (-100.0, 27.0),
            (-100.0, 25.0),
        ]),
        vec![],
    ));
    let mut coords = Vec::new();
    for k in 0..200 {
        coords.push((30.0 + k as f64 * 0.3, -20.0 + (k as f64 * 0.1).sin()));
    }
    let wiggly = Geometry::LineString(LineString::from(coords));
    (wide, concave, wiggly)
}

/// Rich 2-level fixture used by the byte-equivalence tests: points, a
/// seam-crossing line, a concave polygon and a many-vertex line, with
/// id/name properties, at zooms 2 and 4.
#[cfg(test)]
fn equivalence_fixture(path: &Path) {
    let pa = Geometry::Point(Point::new(-120.0, 40.0));
    let pb = Geometry::Point(Point::new(120.0, -40.0));
    let (wide, concave, wiggly) = equivalence_shapes();
    write_fixture(
        path,
        &[
            (vec![0, 1], vec![pa.clone(), pb.clone()]),
            (vec![0, 1, 2, 3, 4], vec![pa, pb, wide, concave, wiggly]),
        ],
    );
}

/// Byte-level anchor for the H3(b) export restructure: the whole archive
/// (header + directory + metadata + tile data) must hash to the value
/// produced by the pre-refactor per-zoom `BTreeMap` implementation. The
/// reference hash was captured from that implementation (commit b8a1635)
/// on this exact fixture before the partitioned-streaming rewrite.
///
/// REPIN (issue #112): the MVT winding fix (`orient_polygon_for_mvt` now
/// emits spec-compliant positive-area exterior rings) intentionally
/// changed polygon command bytes; the hash was re-captured from the
/// fixed encoder. The anchor still guards the export restructure — the
/// partition-invariance test alongside it is unchanged.
///
/// REPIN (rename gpq-tiles → tylertoo): the archive metadata embeds
/// `"generator":"tylertoo"` (was `"gpq-tiles"`) — the only output-bound
/// byte change from the rename, so the hash was re-captured on the same
/// fixture. The export logic is unchanged.
///
/// REPIN (clustered header flag honesty): the header's `clustered` byte
/// (offset 96) now reflects the directory's actual offset layout instead
/// of a hardcoded `true` — this fixture's 13-tile export adds tiles
/// zoom-by-zoom in row-major order, which is not tile-id order, so the
/// byte flips `1` -> `0`. Verified this is the *only* byte that moved:
/// flipping byte 96 of the new archive back to `1` reproduces the prior
/// hash (`0dfdf8a0bb941a0c`) exactly. No tile data, directory entry, or
/// metadata byte changed.
///
/// REPIN (#506, tile-id-ordered export): tiles are now added to the
/// writer in ascending PMTiles tile-id (Hilbert) order per zoom instead
/// of row-major `(x, y)` order, so the on-disk directory and tile-data
/// layout changes even though the tile *set* does not — hence a new
/// archive hash. Verified before re-blessing (both checks below already
/// assert `total_tiles == 13` and every counted tile has features > 0,
/// so this note only adds the two checks those don't cover):
/// * `verify_clustered(path)` and `header.clustered` are both now
///   `true` for this archive (they were both honestly `false` before
///   #506, per the REPIN note above and `pmtiles_clustered.rs`).
/// * Per-tile MVT **content** is unchanged: dumping
///   `z/x/y -> hash(decompressed tile bytes)` for all 13 tiles on this
///   exact fixture, on the pre-#506 tree and on this tree, produces the
///   same 13 `(z, x, y)` keys with the same 13 content hashes in both
///   dumps — the only difference between the two runs is
///   `clustered: false -> true`. Only the ORDER tiles were written in
///   (and therefore their byte offsets) changed, not what any tile
///   contains.
///
/// REPIN (#459, tail-directory layout): export now writes the archive in
/// the tail layout — `header | root | zero padding | tile data @16384 |
/// metadata | leaves` — so that a checkpoint rewrites only the prefix and
/// the tail instead of re-copying every tile byte. The *sections* moved;
/// nothing inside them did. Verified before re-blessing by dumping both
/// trees on this exact fixture (pre-change `ce11a1d5f51410d2`, this tree
/// `16b372437283d228`):
/// * All 13 `z/x/y -> hash(decompressed tile bytes)` pairs are identical,
///   same 13 keys, same 13 hashes, same lengths.
/// * The whole **tile-data section** hashes the same on both trees
///   (`68c6b75f056eb29f`, 1303 bytes) — byte-for-byte, in the same order.
/// * The root directory (`4e367810b90b24df`) and the compressed metadata
///   (`4c71bb05cb6e3e99`) are unchanged too; directory entry offsets are
///   relative to `tile_data_offset`, so moving the section does not
///   rewrite them.
/// * `clustered` and `verify_clustered()` are both still `true`.
///
/// The only differences: `tile_data_offset` 348 -> 16384, and the file
/// grows by the 16,201 zero bytes of prefix padding (1651 -> 17852). That
/// padding is the one slack go-pmtiles' `verify` accepts (verify.go
/// L84-89) — `pmtiles verify` passes on the new archive.
#[test]
fn export_archive_matches_pre_refactor_reference() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(tin.path());
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        layer_name: "ref".to_string(),
        // The defaults this reference was blessed at: an 8 px buffer and
        // no max-zoom simplification of its lines.
        tile_buffer: 8,
        max_zoom_simplify_factor: Some(0.0),
        ..Default::default()
    };
    let report = export_pmtiles(tin.path(), tout.path(), &opts).unwrap();
    // 12 before the tile-buffer fix. The buffer is now the tile pixels it
    // always claimed to be (8 px = 128 MVT units, not 8), and membership
    // comes from the buffer-expanded bbox, so one more tile carries seam
    // overlap and every seam-adjacent tile's bytes change. Re-blessed
    // deliberately -- this reference guards against UNINTENDED byte drift.
    assert_eq!(report.total_tiles, 13);
    let bytes = std::fs::read(tout.path()).unwrap();
    assert_eq!(
        format!("{:016x}", crate::dedup::TileHasher::hash(&bytes)),
        // RE-BLESSED (#459): the archive's SECTIONS moved (tail layout),
        // its contents did not -- the tile-data section, root directory
        // and metadata all hash the same as before. See the REPIN note
        // above this test for the full before/after evidence.
        // #443: default features no longer carry tile-local ids.
        // RE-BLESSED: gzip moved from zlib-rs to libdeflate (level 6).
        // The compressed bytes changed; the tiles under them did not
        // (the same tree with zlib-rs still hashes caf04e1922c86b7b).
        "6184eb9615bf50b8",
        "archive bytes diverged from the pre-refactor reference"
    );

    // A hash only says "not what it was". Assert the new count is RIGHT,
    // not merely different: each counted tile must really be in the archive
    // and carry at least one feature, so the extra tile is genuine seam
    // overlap rather than an empty one conjured by over-wide membership.
    use crate::{
        compression,
        pmtiles_writer::{decode_directory, tile_id_to_zxy, Header},
    };
    let header = Header::from_bytes(&bytes).unwrap();
    let root = compression::decompress_capped(
        &bytes[header.root_dir_offset as usize
            ..(header.root_dir_offset + header.root_dir_length) as usize],
        header.internal_compression,
        compression::MAX_INTERNAL_BYTES,
    )
    .unwrap();
    let entries = decode_directory(&root).expect("root directory must decode");
    assert!(
        entries.iter().all(|e| e.run_length > 0),
        "13 tiles must fit the root directory; this walk does not follow leaves"
    );
    let mut seen = 0usize;
    for e in &entries {
        let start = (header.tile_data_offset + e.offset) as usize;
        let raw = &bytes[start..start + e.length as usize];
        let plain = compression::decompress_capped(
            raw,
            header.tile_compression,
            compression::MAX_TILE_BYTES,
        )
        .unwrap();
        let decoded = decode_tile(&plain);
        let features: usize = decoded.layers.iter().map(|l| l.features.len()).sum();
        let zxy = tile_id_to_zxy(e.tile_id).unwrap();
        assert!(
            features > 0,
            "tile {zxy:?} was emitted with no features -- membership is too wide"
        );
        seen += e.run_length as usize;
    }
    assert_eq!(
        seen, 13,
        "every counted tile must be addressable in the archive"
    );
}

/// The PMTiles directory is always sorted by tile id (a writer invariant
/// independent of add order — see `StreamingPmtilesWriter::write_archive`
/// — and the format's own contract), so walking it ascending is not by
/// itself evidence of #506. What #506 changes is whether the underlying
/// tile DATA is laid out in that same order.
///
/// That is NOT the same claim as "every entry's offset is >= the
/// previous entry's offset" (#506 review, F4): a deduplication
/// back-reference legally points an entry at an EARLIER tile's already-written
/// bytes (`offsets_are_clustered`'s own contract — see
/// `pmtiles_writer::verify_clustered`), and that is still a `clustered`
/// archive under the PMTiles v3 definition (a forward-streaming reader
/// just re-serves bytes it already has buffered). A raw offset-monotonicity
/// assertion here would be too strict — it happened to hold on this
/// fixture only because none of its 13 tiles are non-adjacent duplicates
/// of each other, not because export guarantees it. The real predicate
/// this test checks is [`crate::pmtiles_writer::verify_clustered`], the
/// same file-side re-derivation the header's own `clustered` byte is
/// checked against.
///
/// Since #516 landed, `verify_clustered` *is* the strict walk — every
/// first-seen offset must equal the running end of the data written so
/// far, and a repeated offset must name a whole prior tile — so this
/// test pins the tool's rule through the shared
/// [`crate::pmtiles_writer::offsets_are_clustered`] predicate rather
/// than re-implementing it. What remains to check separately is byte
/// accounting: the predicate proves no entry lands in a gap, not that
/// the section has no trailing bytes nothing references.
#[test]
fn export_emits_tiles_in_ascending_tile_id() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(tin.path());
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        layer_name: "ref".to_string(),
        ..Default::default()
    };
    export_pmtiles(tin.path(), tout.path(), &opts).unwrap();

    use crate::{
        compression,
        pmtiles_writer::{
            decode_directory, offsets_are_clustered, tile_id_to_zxy, verify_clustered, DirEntry,
            Header,
        },
    };
    let bytes = std::fs::read(tout.path()).unwrap();
    let header = Header::from_bytes(&bytes).unwrap();
    let root = compression::decompress_capped(
        &bytes[header.root_dir_offset as usize
            ..(header.root_dir_offset + header.root_dir_length) as usize],
        header.internal_compression,
        compression::MAX_INTERNAL_BYTES,
    )
    .unwrap();
    let entries = decode_directory(&root).expect("root directory must decode");
    assert!(
        entries.iter().all(|e| e.run_length > 0),
        "fixture must fit the root directory; this walk does not follow leaves"
    );
    assert!(
        entries.len() >= 8,
        "fixture must exercise several tiles across multiple zooms, got {}",
        entries.len()
    );

    // The directory is always sorted by tile id (a writer invariant, not
    // specific to #506) -- asserted here as a sanity check on the walk
    // itself, not as the test's main claim.
    let mut prev: Option<&DirEntry> = None;
    for e in &entries {
        if let Some(p) = prev {
            assert!(
                e.tile_id > p.tile_id,
                "directory entries must be strictly ascending by tile id"
            );
        }
        prev = Some(e);
    }

    // The actual #506 claim: the tile DATA is genuinely laid out in
    // ascending tile-id order, dedup back-references included --
    // `verify_clustered` re-derives this from the offsets directly
    // rather than assuming raw monotonicity.
    assert!(
        verify_clustered(tout.path()).unwrap(),
        "export must write tile data in genuinely clustered (ascending \
         tile-id) order -- verify_clustered on the exported archive says \
         it is not"
    );

    // ...and the same rule go-pmtiles `verify` is written to express,
    // pinned through the shared predicate rather than a third hand-rolled
    // walk (#516 review). Before #516 this test re-implemented the strict
    // walk because `verify_clustered` was laxer than the tool; #516 made
    // the library-side helper strict, so the hand-rolled copy would only
    // drift from the thing it is meant to pin.
    assert!(
        offsets_are_clustered(entries.iter().map(|e| (e.offset, u64::from(e.length)))),
        "out-of-order entry in clustered archive (first tile id {}, z/x/y {:?}): \
         every first-seen offset must be the running end of the tile data \
         written so far, and a repeat must name a whole prior tile",
        entries[0].tile_id,
        tile_id_to_zxy(entries[0].tile_id).unwrap()
    );

    // Byte accounting, which the ordering predicate does not cover: the
    // distinct offsets the directory references must tile the whole
    // tile-data section, leaving no unreferenced trailing bytes.
    let mut blobs: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    for e in &entries {
        blobs.insert(e.offset, u64::from(e.length));
    }
    let accounted: u64 = blobs.values().sum();
    assert_eq!(
        accounted, header.tile_data_length,
        "the referenced blobs must account for every byte of the tile-data section"
    );
}

/// The counterpart to [`export_emits_tiles_in_ascending_tile_id`]: the
/// header's derived `clustered` byte (#501) must now come out `true` for
/// a real export, and must agree with the independent file-side
/// [`crate::pmtiles_writer::verify_clustered`] re-derivation.
/// `pmtiles_clustered.rs` pins the same claim on a real-world fixture;
/// this is the always-available oracle on the in-memory one.
#[test]
fn export_header_clustered_is_true() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(tin.path());
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        layer_name: "ref".to_string(),
        ..Default::default()
    };
    export_pmtiles(tin.path(), tout.path(), &opts).unwrap();

    use crate::pmtiles_writer::{verify_clustered, Header};
    let bytes = std::fs::read(tout.path()).unwrap();
    let header = Header::from_bytes(&bytes).unwrap();
    assert!(
        header.clustered,
        "export archive must be genuinely clustered (#506)"
    );
    assert_eq!(
        header.clustered,
        verify_clustered(tout.path()).unwrap(),
        "the header's claim must match the independent file-side re-derivation"
    );
}

/// The archive must be byte-identical regardless of partitioning:
/// `target=1` forces (roughly) one partition per tile — the maximum
/// partition count and maximal per-partition band re-reads — and must
/// produce exactly the bytes of the default (large-target) export. With
/// `export_archive_matches_pre_refactor_reference` this pins the
/// multi-partition path to the pre-refactor output.
#[test]
fn partitioned_export_is_partition_invariant() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(tin.path());
    let opts = ExportOptions {
        layer_name: "ref".to_string(),
        ..Default::default()
    };
    let t_many = tempfile::NamedTempFile::new().unwrap();
    let t_one = tempfile::NamedTempFile::new().unwrap();
    let r_many = export_pmtiles_with_partition_target(tin.path(), t_many.path(), &opts, 1).unwrap();
    let r_one = export_pmtiles(tin.path(), t_one.path(), &opts).unwrap();

    let b_many = std::fs::read(t_many.path()).unwrap();
    let b_one = std::fs::read(t_one.path()).unwrap();
    assert_eq!(
        b_many, b_one,
        "partitioned archive bytes diverge from single-partition archive"
    );
    // Reports must agree on everything except wall-clock duration.
    assert_eq!(r_many.zooms, r_one.zooms);
    assert_eq!(r_many.total_tiles, r_one.total_tiles);
    assert_eq!(r_many.total_tile_features, r_one.total_tile_features);
    assert_eq!(r_many.oversized_tiles, r_one.oversized_tiles);
}

/// The auto memory preflight (#303): pure decision function, exercised
/// across the RAM/core grid the way #294's `auto_backing` tests are.
#[test]
fn auto_partition_wave_memory_preflight() {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    // Plenty of RAM: cores drive the width — including past the old fixed
    // 16 cap (the whole point of #303).
    assert_eq!(
        auto_partition_wave(
            64,
            MemoryProfile::Auto,
            &MemoryBudget::with_available(Some(256 * GIB))
        ),
        64
    );
    assert_eq!(
        auto_partition_wave(
            24,
            MemoryProfile::Auto,
            &MemoryBudget::with_available(Some(256 * GIB))
        ),
        24
    );
    // The #293 target box (16 cores / 54 GiB) resolves exactly as before.
    assert_eq!(
        auto_partition_wave(
            16,
            MemoryProfile::Auto,
            &MemoryBudget::with_available(Some(54 * GIB))
        ),
        16
    );
    // Tight RAM caps below the core count:
    // 4 GiB avail → 2 GiB wave budget → 32 slots of 64 MiB.
    let avail = |bytes: u64| MemoryBudget::with_available(Some(bytes));
    assert_eq!(
        auto_partition_wave(64, MemoryProfile::Auto, &avail(4 * GIB)),
        32
    );
    // 2 GiB avail → 1 GiB budget → 16 slots.
    assert_eq!(
        auto_partition_wave(64, MemoryProfile::Auto, &avail(2 * GIB)),
        16
    );
    // 1 GiB avail → 512 MiB budget → 8 slots.
    assert_eq!(auto_partition_wave(64, MemoryProfile::Auto, &avail(GIB)), 8);
    // Very tight RAM floors at MIN, never below (historical width; its
    // worst-case transient is small and known-safe).
    assert_eq!(
        auto_partition_wave(
            16,
            MemoryProfile::Auto,
            &MemoryBudget::with_available(Some(256 * MIB))
        ),
        PARTITION_WAVE_MIN
    );
    assert_eq!(
        auto_partition_wave(
            16,
            MemoryProfile::Auto,
            &MemoryBudget::with_available(Some(0))
        ),
        PARTITION_WAVE_MIN
    );
    // Small boxes keep the floor regardless of RAM.
    assert_eq!(
        auto_partition_wave(
            2,
            MemoryProfile::Auto,
            &MemoryBudget::with_available(Some(256 * GIB))
        ),
        PARTITION_WAVE_MIN
    );
    // RAM unknown (non-Linux, unreadable /proc/meminfo): the budget's
    // fallback allowance (4 GiB) prices the slots, as for every phase:
    // 4 GiB × 0.5/0.6 / 64 MiB = 53.
    assert_eq!(
        auto_partition_wave(64, MemoryProfile::Auto, &MemoryBudget::with_available(None)),
        53
    );
    // `speed` sets no memory cap: the cores decide (#788).
    assert_eq!(
        auto_partition_wave(64, MemoryProfile::Speed, &avail(GIB)),
        64
    );
    // `bounded` keeps main's memory-safe cap.
    assert_eq!(
        auto_partition_wave(64, MemoryProfile::Bounded, &avail(GIB)),
        8
    );
    assert_eq!(
        auto_partition_wave(8, MemoryProfile::Auto, &MemoryBudget::with_available(None)),
        8
    );
    assert_eq!(
        auto_partition_wave(2, MemoryProfile::Auto, &MemoryBudget::with_available(None)),
        PARTITION_WAVE_MIN
    );
}

/// Build a partition list from raw member counts (bbox/keys are irrelevant
/// to the memory-safe sizing, which only reads `.members`).
fn partitions_with_members(counts: &[usize]) -> Vec<Partition> {
    counts
        .iter()
        .enumerate()
        .map(|(i, &m)| Partition {
            key_lo: i as u64,
            key_hi: i as u64,
            bbox: PruneRegion::of(TileBounds::new(0.0, 0.0, 0.0, 0.0)),
            members: m,
        })
        .collect()
}

/// The data-aware per-level guard (#311): a wave may hold the members the
/// budget affords, and may drop below the throughput floor when memory
/// demands it.
#[test]
fn memory_safe_wave_members_prices_one_member() {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    let dense_bytes = Some(member_resident_bytes(600, 0));
    let budget = MemoryBudget::with_available(Some(54 * GIB));
    let cap = memory_safe_wave_members(dense_bytes, MemoryProfile::Auto, &budget);
    // 0.5 × 54 GiB at ~1424 B/member ≈ 20M members: the Brazil level
    // that OOM'd at wave 16 (one 6.5M-member partition) fits 3 of them.
    assert_eq!(cap / 6_500_000, 3, "cap {cap}");

    // Fallbacks keep prior behaviour: unknown or zero member size, and
    // speed, put no member cap on a wave.
    for bytes in [None, Some(0)] {
        assert_eq!(
            memory_safe_wave_members(bytes, MemoryProfile::Auto, &budget),
            usize::MAX
        );
    }
    assert_eq!(
        memory_safe_wave_members(
            dense_bytes,
            MemoryProfile::Auto,
            &MemoryBudget::with_available(Some(8))
        ),
        1,
        "a tiny machine affords one member: every partition is its own wave"
    );
    assert_eq!(
        memory_safe_wave_members(
            dense_bytes,
            MemoryProfile::Speed,
            &MemoryBudget::with_available(Some(1 << 20))
        ),
        usize::MAX,
        "speed keeps the ceiling: no memory cap (#788)"
    );
    let unprobed = memory_safe_wave_members(
        dense_bytes,
        MemoryProfile::Auto,
        &MemoryBudget::with_available(None),
    );
    assert!(
        unprobed < cap,
        "unprobeable RAM is priced by the budget's 4 GiB fallback allowance, not uncapped: {unprobed}"
    );
}

/// #876: a wave is cut by the members actually in it, so one dense
/// partition narrows only its own wave, not the whole level.
#[test]
fn cut_waves_narrows_only_the_dense_partitions_wave() {
    let starts = |counts: &[usize], ceiling, cap| {
        cut_waves(&partitions_with_members(counts), ceiling, cap).0
    };
    // No member cap: uniform waves of the ceiling, as before.
    assert_eq!(starts(&[1; 10], 4, usize::MAX), vec![0, 4, 8]);
    // A dense partition shares its wave while the sum fits, then the
    // small ones after it run at the full ceiling.
    assert_eq!(starts(&[90, 5, 5, 5, 5, 5, 5, 5, 5], 4, 100), vec![0, 3, 7]);
    // The old rule priced every slot at the densest (90): one partition
    // a wave, nine waves.
    // A partition over the cap on its own is its own wave (refused or
    // not, that is `densest_tile_over_limit`'s call, not the cut's).
    assert_eq!(starts(&[5, 500, 5, 5], 4, 100), vec![0, 1, 2]);
    // The Brazil shape: 6.5M + two tiny partitions fit one wave.
    assert_eq!(starts(&[6_500_000, 100, 200], 16, 20_000_000), vec![0]);
    // A cap of one member makes every partition its own wave.
    assert_eq!(starts(&[3, 3, 3], 16, 1), vec![0, 1, 2]);
    // An empty level has no waves.
    assert_eq!(starts(&[], 16, 100), Vec::<usize>::new());
}

/// #876 review: the plan records that the member cap moved a boundary
/// even when the wave count is the same as an uncapped cut, so the
/// density-guard log line still fires.
#[test]
fn level_plan_flags_a_member_cap_cut_even_when_the_count_holds() {
    let capped = LevelPlan::cut(3, partitions_with_members(&[90, 5, 5, 5, 5]), 4, 100);
    assert_eq!(capped.wave_starts, vec![0, 3]);
    assert!(capped.narrowed);
    let free = LevelPlan::cut(3, partitions_with_members(&[90, 5, 5, 5, 5]), 4, usize::MAX);
    assert_eq!(free.wave_starts, vec![0, 4]);
    assert!(!free.narrowed);
    assert!(!LevelPlan::cut(3, Vec::new(), 4, 1).narrowed);
}

/// #876 review: `wave(w)` is the `w`-th of `waves()` in constant time, and
/// empty past the last wave (an empty level's one synthetic wave).
#[test]
fn level_plan_wave_indexes_like_waves() {
    let same = |a: &[Partition], b: &[Partition]| {
        a.len() == b.len() && std::ptr::eq(a.as_ptr(), b.as_ptr())
    };
    let plan = LevelPlan::cut(
        3,
        partitions_with_members(&[90, 5, 5, 5, 5, 5, 5, 5, 5]),
        4,
        100,
    );
    for (w, wave) in plan.waves().enumerate() {
        assert!(same(plan.wave(w), wave), "wave {w}");
    }
    assert!(plan.wave(plan.wave_count()).is_empty());
    assert!(LevelPlan::cut(3, Vec::new(), 4, 100).wave(0).is_empty());
}

/// #876: a level's routing and its wave slices agree for uneven waves.
#[test]
fn level_plan_routes_each_partition_to_the_wave_that_holds_it() {
    let plan = LevelPlan::cut(
        3,
        partitions_with_members(&[90, 5, 5, 5, 5, 5, 5, 5, 5]),
        4,
        100,
    );
    assert_eq!(plan.wave_count(), 3);
    assert_eq!(plan.widest_wave(), 4);
    let mut seen = 0;
    for (w, wave) in plan.waves().enumerate() {
        for _ in wave {
            assert_eq!(plan.wave_of(seen), w, "partition {seen}");
            seen += 1;
        }
    }
    assert_eq!(seen, 9);
}

/// #665: the resident model covers every shape it was calibrated on,
/// by at most 55%. Rows are (stored B/row, published properties,
/// measured B/member); see [`MEMBER_BASE_BYTES`].
#[test]
fn member_resident_bytes_covers_the_665_calibration() {
    for (stored, properties, measured) in
        [(115, 4, 618), (96, 4, 879), (213, 16, 2_334), (107, 4, 647)]
    {
        let estimate = member_resident_bytes(stored, properties);
        assert!(
            (measured..=measured * 31 / 20).contains(&estimate),
            "({stored} B, {properties} properties): {estimate} B/member must cover the \
             measured {measured} B, within 55%"
        );
    }
    // The two property-less shapes (460 and 432 B measured) sit above
    // the base price on the aarch64 host (see [`MEMBER_BASE_BYTES`]):
    // pin the known gaps (about 1% and 22%): the estimate may under-cover
    // the measured value by at most 25%.
    for (stored, measured) in [(115, 460), (57, 432)] {
        let estimate = member_resident_bytes(stored, 0);
        assert!(
            estimate >= measured * 3 / 4,
            "({stored} B, 0 properties): {estimate} B/member under-covers {measured} B by \
             more than 25% (known gaps: about 1% and 22%)"
        );
    }
    assert_eq!(member_resident_bytes(u64::MAX, usize::MAX), u64::MAX);
}

const GIB: u64 = 1024 * 1024 * 1024;

/// A #665 memory figure from `source`.
fn limit(bytes: u64, source: MemoryLimitSource) -> MemoryLimit {
    MemoryLimit { bytes, source }
}

/// #665: a tile that cannot fit in memory even on its own is refused,
/// naming it; one that fits serially is not.
#[test]
fn densest_tile_over_limit_refuses_only_what_no_wave_fits() {
    let avail = |bytes| limit(bytes, MemoryLimitSource::MemAvailable);

    // The #665 run: `--verbatim` puts all 81M Overture places in the
    // z0 tile, with four properties at ~105 B/row stored, on a box with
    // ~57 GiB available. No wave width can help.
    let places = 81_455_423;
    let z0_counts = BTreeMap::from([(tile_key(0, 0, 0), places)]);
    let err = densest_tile_over_limit(
        0,
        0,
        &z0_counts,
        member_resident_bytes(105, 4),
        avail(57 * GIB),
    )
    .expect("81M points in one z0 tile cannot fit in 57 GiB");
    let ExportError::TileExceedsMemory {
        level,
        zoom,
        x,
        y,
        members,
        estimate_bytes,
        available_bytes,
        limit_source,
    } = err
    else {
        panic!("wrong error: {err}");
    };
    assert_eq!((level, zoom, x, y, members), (0, 0, 0, 0, places));
    assert!(estimate_bytes > available_bytes);
    assert_eq!(limit_source, MemoryLimitSource::MemAvailable.describe());
    let msg = err.to_string();
    for needle in [
        "z0/0/0",
        "GiB",
        "MemAvailable",
        "--partition-wave",
        "Rebuild the overview",
        "--verbatim",
        "--point-thinning 4",
        "--min-zoom",
        SKIP_MEMORY_PREFLIGHT_ENV,
    ] {
        assert!(msg.contains(needle), "message must name {needle}: {msg}");
    }

    // The #311 Brazil level: dense, but one tile fits serially, so the
    // wave guard handles it and nothing is refused.
    let brazil_counts = BTreeMap::from([(0, 6_500_000), (1, 100)]);
    assert!(densest_tile_over_limit(
        13,
        14,
        &brazil_counts,
        member_resident_bytes(600, 3),
        avail(54 * GIB),
    )
    .is_none());

    // The threshold is the whole figure: exactly full fits, one more
    // member does not.
    let at = BTreeMap::from([(0, 1_000)]);
    assert!(densest_tile_over_limit(0, 0, &at, 1_000, avail(1_000_000)).is_none());
    let over = BTreeMap::from([(0, 1_001)]);
    assert!(densest_tile_over_limit(0, 0, &over, 1_000, avail(1_000_000)).is_some());

    // Nothing scanned never refuses.
    assert!(densest_tile_over_limit(0, 0, &BTreeMap::new(), 1_000, avail(1)).is_none());
}

/// #665: the refusal names the densest tile, the first of equals, not
/// the first tile.
#[test]
fn densest_tile_over_limit_names_the_densest_tile() {
    let zoom = 2;
    let counts: BTreeMap<u64, usize> = [((0, 0), 10), ((1, 0), 700), ((2, 2), 700), ((3, 3), 290)]
        .into_iter()
        .map(|((x, y), c)| (tile_key(x, y, zoom), c))
        .collect();
    let first_dense = counts
        .iter()
        .find(|&(_, &c)| c == 700)
        .map(|(&k, _)| key_to_xy(k, zoom))
        .unwrap();
    let err = densest_tile_over_limit(
        2,
        zoom,
        &counts,
        1_000,
        limit(1, MemoryLimitSource::CgroupMax),
    )
    .expect("700 members cannot fit in 1 byte");
    let ExportError::TileExceedsMemory {
        level: 2,
        zoom: 2,
        x,
        y,
        members: 700,
        ..
    } = err
    else {
        panic!("wrong error: {err:?}");
    };
    assert_eq!((x, y), first_dense);
}

/// #665: a partition of many small tiles is priced tile by tile, so
/// clipped pieces of large polygons, each priced at a whole stored row,
/// are not refused for adding up past memory as a partition.
#[test]
fn check_levels_fit_prices_tiles_not_partitions() {
    let zoom = 12;
    let tile_counts: BTreeMap<u64, usize> =
        (0..32).map(|x| (tile_key(x, 0, zoom), 1_024)).collect();
    let partitions = plan_partitions(&tile_counts, zoom, DEFAULT_PARTITION_TARGET);
    assert_eq!(partitions.len(), 1);
    let plans = [LevelPlan::cut(zoom, partitions, 1, usize::MAX)];
    let scans = [LevelScan {
        feature_count: 32 * 1_024,
        bounds: None,
        tile_counts,
    }];
    // 400 KB rows: the partition prices at 12.5 GiB, each tile at 400 MiB.
    let per_member = Some(member_resident_bytes(200_000, 0));
    let ram = Some(limit(8 * GIB, MemoryLimitSource::CgroupMax));
    assert!(check_levels_fit(&plans, &scans, per_member, ram, false).is_ok());
}

/// #665: `MemAvailable` and a cgroup `memory.max` refuse. The override,
/// a skip, or an unknown figure or member size only let it through.
#[test]
fn check_levels_fit_refuses_only_hard_figures() {
    let zoom = 0;
    let tile_counts = BTreeMap::from([(tile_key(0, 0, zoom), 1_000)]);
    let plans = [
        LevelPlan::cut(
            zoom,
            plan_partitions(&BTreeMap::new(), zoom, 1),
            1,
            usize::MAX,
        ),
        LevelPlan::cut(zoom, plan_partitions(&tile_counts, zoom, 1), 1, usize::MAX),
    ];
    let scans = [
        LevelScan {
            feature_count: 0,
            bounds: None,
            tile_counts: BTreeMap::new(),
        },
        LevelScan {
            feature_count: 1_000,
            bounds: None,
            tile_counts,
        },
    ];
    let check = |per_member, source: Option<MemoryLimitSource>, skip| {
        check_levels_fit(
            &plans,
            &scans,
            per_member,
            source.map(|s| limit(1, s)),
            skip,
        )
    };
    for source in [
        MemoryLimitSource::MemAvailable,
        MemoryLimitSource::CgroupMax,
    ] {
        let err = check(Some(1_000), Some(source), false)
            .expect_err("1,000 members cannot fit in 1 byte");
        assert!(
            matches!(err, ExportError::TileExceedsMemory { level: 1, .. }),
            "{source:?}: {err:?}"
        );
        assert!(check(Some(1_000), Some(source), true).is_ok(), "{source:?}");
    }
    assert!(check(Some(1_000), Some(MemoryLimitSource::Override), false).is_ok());
    assert!(check(Some(1_000), None, false).is_ok());
    assert!(check(None, Some(MemoryLimitSource::MemAvailable), false).is_ok());
    assert!(check(Some(0), Some(MemoryLimitSource::MemAvailable), false).is_ok());
}

/// #665 end to end: the refusal comes after the scan and before any
/// tile, so it leaves neither an archive nor a `.partial`, and under the
/// override the same file exports.
#[test]
fn dense_tile_refusal_writes_nothing() {
    for mode in [Mode::Duplicating, Mode::Partitioning] {
        let input = tempfile::NamedTempFile::new().unwrap();
        level_group_fixture(input.path(), mode);
        let opts = ExportOptions {
            layer_name: "dense".to_string(),
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let run = |name: &str, source| {
            let out = dir.path().join(name);
            let res = export_pmtiles_run(
                input.path(),
                &out,
                &opts,
                DEFAULT_PARTITION_TARGET,
                &Pass2Knobs {
                    memory_limit: Some(limit(1, source)),
                    ..knobs(false, None, true)
                },
            );
            (res, out)
        };

        let (res, out) = run("refused.pmtiles", MemoryLimitSource::MemAvailable);
        let err = res.err().expect("no tile fits in 1 byte");
        let ExportError::TileExceedsMemory {
            members,
            estimate_bytes,
            ..
        } = err
        else {
            panic!("{mode:?}: wrong error: {err}");
        };
        // Each member is priced at its stored bytes on top of the base.
        assert!(
            estimate_bytes > members as u64 * MEMBER_BASE_BYTES,
            "{mode:?}: {estimate_bytes} B for {members} members"
        );
        assert!(
            !out.exists(),
            "{mode:?}: a refused export writes no archive"
        );
        assert!(
            !out.with_extension("pmtiles.partial").exists(),
            "{mode:?}: a refused export leaves no .partial"
        );

        let (res, out) = run("override.pmtiles", MemoryLimitSource::Override);
        res.unwrap_or_else(|e| panic!("{mode:?}: the override only warns: {e}"));
        assert!(out.exists(), "{mode:?}");
    }
}

#[test]
fn resolve_partition_wave_auto_and_explicit() {
    // Auto (0) runs the memory preflight against the real probes; the
    // result must match the pure decision function fed the same inputs
    // and never fall below the floor.
    let auto = resolve_partition_wave(PARTITION_WAVE_AUTO);
    assert!(
        auto >= PARTITION_WAVE_MIN,
        "auto wave {auto} below floor {PARTITION_WAVE_MIN}"
    );
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(PARTITION_WAVE_MIN);
    assert_eq!(
        auto,
        auto_partition_wave(cores, MemoryProfile::Auto, &export_budget()),
        "auto must equal the preflight decision on this box's cores + RAM"
    );
    // Explicit values pass through verbatim, including above any auto cap.
    assert_eq!(resolve_partition_wave(1), 1);
    assert_eq!(resolve_partition_wave(6), 6);
    assert_eq!(
        resolve_partition_wave(PARTITION_WAVE_MIN + 100),
        PARTITION_WAVE_MIN + 100
    );
}

/// The archive must be byte-identical regardless of wave width: the wave is
/// a scheduling parameter (how many partitions share a band read), never a
/// change to tile membership or write order. A narrow explicit wave (1) and
/// a wide one (16) must produce exactly the same bytes — this is the
/// equivalence gate for #293's auto-scaling default.
#[test]
fn export_wave_width_is_byte_invariant() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(tin.path());
    let t_narrow = tempfile::NamedTempFile::new().unwrap();
    let t_wide = tempfile::NamedTempFile::new().unwrap();
    let narrow = ExportOptions {
        layer_name: "ref".to_string(),
        partition_wave: 1,
        ..Default::default()
    };
    let wide = ExportOptions {
        layer_name: "ref".to_string(),
        partition_wave: 16,
        ..Default::default()
    };
    // Force many partitions so multiple waves actually form at both widths.
    export_pmtiles_with_partition_target(tin.path(), t_narrow.path(), &narrow, 1).unwrap();
    export_pmtiles_with_partition_target(tin.path(), t_wide.path(), &wide, 1).unwrap();
    assert_eq!(
        std::fs::read(t_narrow.path()).unwrap(),
        std::fs::read(t_wide.path()).unwrap(),
        "archive bytes diverge between wave widths 1 and 16"
    );
}

/// #876: the auto guard cuts waves by the members in them, so a tight
/// budget gives a level waves of different widths. The archive must
/// still be the bytes of an explicit one-partition wave, in both the
/// duplicating (per-wave reads) and partitioning (#235 store) modes, RAM
/// or spill.
#[test]
fn uneven_auto_waves_are_byte_invariant() {
    let mut uneven = 0;
    let fixtures: [fn(&Path); 2] = [equivalence_fixture, partitioning_equivalence_fixture];
    for fixture in fixtures {
        let input = tempfile::NamedTempFile::new().unwrap();
        fixture(input.path());
        let run = |partition_wave: usize, available: Option<u64>| {
            let opts = ExportOptions {
                layer_name: "ref".to_string(),
                partition_wave,
                ..Default::default()
            };
            let out = tempfile::NamedTempFile::new().unwrap();
            let run = export_pmtiles_run(
                input.path(),
                out.path(),
                &opts,
                1,
                &Pass2Knobs {
                    budget_available: available,
                    ..knobs(false, None, true)
                },
            )
            .unwrap();
            (std::fs::read(out.path()).unwrap(), run.wave_starts)
        };
        let (reference, _) = run(1, Some(100 << 30));
        for shift in 10..=34 {
            let (bytes, starts) = run(PARTITION_WAVE_AUTO, Some(1 << shift));
            assert_eq!(bytes, reference, "budget 2^{shift}");
            uneven += starts
                .iter()
                .filter(|s| s.windows(3).any(|w| w[1] - w[0] != w[2] - w[1]))
                .count();
        }
    }
    assert!(uneven > 0, "no budget cut a level into uneven waves");
}

fn plan_with_members(zoom: u8, counts: &[usize], wave: usize) -> LevelPlan {
    LevelPlan::cut(zoom, partitions_with_members(counts), wave, usize::MAX)
}

proptest::proptest! {
    /// The group gate's bookkeeping on random waves of encoded partitions,
    /// member prices, write orders and one failed wave: the gate holds
    /// exactly the members not yet encoded plus the queued share of every
    /// encoded wave not yet written, a failed wave gives its share back,
    /// and the gate ends empty. Without a member price a partition keeps
    /// its members; under `bounded` (no overlap) it keeps nothing.
    #[test]
    fn in_flight_holds_queued_output_until_it_is_written(
        waves in proptest::collection::vec(
            proptest::collection::vec((0usize..60, 0usize..3, 0usize..6_000), 1..6),
            1..6,
        ),
        (overlap, price) in (proptest::bool::ANY, proptest::option::of(0u64..5_000)),
        order in proptest::collection::vec(proptest::num::usize::ANY, 6),
        failed in proptest::num::usize::ANY,
    ) {
        let tile = |bytes: usize| EncodedTile {
            x: 0,
            y: 0,
            data: vec![0; bytes],
            hash: 0,
            raw_len: bytes,
            feature_count: 1,
            encode_dropped: 0,
            encode_quantized: 0,
            oversized: false,
            valve: ValveTally::default(),
            fill: FillTally::default(),
        };
        let gate = InFlight {
            queue: QueueCharge::new(overlap, price),
            ..InFlight::new(usize::MAX)
        };
        let held = || *gate.held.lock().unwrap();
        let failed = failed % waves.len();
        let mut queued: Vec<usize> = Vec::new();
        for (k, wave) in waves.iter().enumerate() {
            let admitted = gate.admit(wave.iter().map(|p| p.0).sum());
            let mut share = 0;
            for &(members, tiles, bytes) in wave {
                let tiles: Vec<EncodedTile> = (0..tiles).map(|_| tile(bytes)).collect();
                let before = held();
                admitted.encoded(members, &tiles);
                let kept = gate.queued_share(members, &tiles);
                if overlap && price.unwrap_or(0) == 0 && !tiles.is_empty() {
                    proptest::prop_assert_eq!(kept, members);
                }
                proptest::prop_assert_eq!(held(), before - members + kept);
                share += kept;
            }
            if !overlap {
                proptest::prop_assert_eq!(share, 0);
            }
            if k == failed {
                let before = held();
                drop(admitted);
                proptest::prop_assert_eq!(held(), before - share);
            } else {
                queued.push(admitted.take_queued());
                proptest::prop_assert_eq!(*queued.last().unwrap(), share);
                drop(admitted);
            }
        }
        proptest::prop_assert_eq!(held(), queued.iter().sum::<usize>());
        for pick in order {
            if queued.is_empty() {
                break;
            }
            let written = queued.swap_remove(pick % queued.len());
            gate.release(written);
            proptest::prop_assert_eq!(held(), queued.iter().sum::<usize>());
        }
        for written in queued {
            gate.release(written);
        }
        proptest::prop_assert_eq!(held(), 0);
    }

    /// The level planner on random level sizes, waves and caps: every
    /// level in exactly one group, in order; a single-wave level shares
    /// a group only within the member cap, and under `bounded` (no
    /// overlap) only while the group's single-wave members sum within
    /// it; the gate never runs past the cap; `bounded` keeps today's
    /// schedule: no overlap, no early store fill, its cap the largest
    /// wave plus the slack.
    #[test]
    fn level_schedule_respects_the_member_cap(
        levels in proptest::collection::vec(
            (proptest::collection::vec(0usize..400, 0..9), 1usize..5),
            0..10,
        ),
        member_cap in 0usize..1_500,
        target in 1usize..200,
    ) {
        let plans: Vec<LevelPlan> = levels
            .iter()
            .enumerate()
            .map(|(z, (counts, wave))| plan_with_members(z as u8, counts, *wave))
            .collect();
        let members = |p: &LevelPlan| p.partitions.iter().map(|q| q.members).sum::<usize>();
        let single = |p: &LevelPlan| p.wave_count() <= 1;
        for profile in [MemoryProfile::Auto, MemoryProfile::Speed, MemoryProfile::Bounded] {
            let (groups, gate) = group_schedule(member_cap, &plans, target, profile);
            let flat: Vec<usize> = groups.iter().flat_map(Clone::clone).collect();
            proptest::prop_assert_eq!(flat, (0..plans.len()).collect::<Vec<_>>());
            proptest::prop_assert!(gate.cap <= member_cap && gate.single_wave_cap <= member_cap);
            let overlap = profile != MemoryProfile::Bounded;
            proptest::prop_assert_eq!(gate.overlap, overlap);
            for g in groups.iter().filter(|g| g.len() > 1) {
                let singles: Vec<&LevelPlan> =
                    plans[g.clone()].iter().filter(|p| single(p)).collect();
                proptest::prop_assert!(singles.iter().all(|p| members(p) <= member_cap));
                if !overlap {
                    let sum: usize = singles.iter().map(|p| members(p)).sum();
                    proptest::prop_assert!(sum <= member_cap, "{g:?}: {sum}");
                }
            }
            if !overlap {
                let largest_wave = plans
                    .iter()
                    .flat_map(|p| p.waves().map(|w| w.iter().map(|q| q.members).sum::<usize>()).collect::<Vec<_>>())
                    .max()
                    .unwrap_or(0);
                proptest::prop_assert_eq!(gate.cap, member_cap.min(with_wave_slack(largest_wave)));
                proptest::prop_assert_eq!(
                    &groups,
                    &plan_level_groups(&plans, member_cap, (gate.waves, false))
                );
            }
        }
    }
}

/// #647: consecutive single-wave levels coalesce into one group while
/// their summed members fit the cap, or, overlapping, while each fits
/// it; a multi-wave level, or a level that alone overshoots the cap,
/// runs on its own exactly as before.
#[test]
fn plan_level_groups_coalesces_single_wave_levels_under_the_cap() {
    let plans = vec![
        plan_with_members(3, &[10], 6),
        plan_with_members(4, &[], 6), // empty level: zero members
        plan_with_members(5, &[30], 6),
        plan_with_members(6, &[40, 20], 6),
        plan_with_members(7, &[90], 6), // 40+20+90 > 100: starts a new group
        plan_with_members(8, &[5; 7], 6), // 7 partitions / wave 6: multi-wave
        plan_with_members(9, &[500], 6), // alone over the cap
        plan_with_members(10, &[1], 6),
    ];
    assert_eq!(
        plan_level_groups(&plans, 100, (false, false)),
        vec![0..4, 4..5, 5..6, 6..7, 7..8],
    );
    // A cap of 0 is the serial schedule.
    assert_eq!(
        plan_level_groups(&plans, 0, (false, true)),
        (0..plans.len()).map(|i| i..i + 1).collect::<Vec<_>>(),
    );
    // A cap of 1 is not: an empty level still joins a 1-member level.
    let tiny = vec![
        plan_with_members(3, &[], 6),
        plan_with_members(4, &[1], 6),
        plan_with_members(5, &[1], 6),
    ];
    assert_eq!(
        plan_level_groups(&tiny, 1, (false, false)),
        vec![0..2, 2..3]
    );
    assert_eq!(plan_level_groups(&tiny, 1, (false, true)), vec![0..3]);
    // A multi-wave level under the cap does not open a group for the
    // single-wave level after it.
    let multi_first = vec![
        plan_with_members(3, &[1; 7], 6),
        plan_with_members(4, &[1], 6),
    ];
    assert_eq!(
        plan_level_groups(&multi_first, 100, (false, true)),
        vec![0..1, 1..2]
    );
    assert!(plan_level_groups(&[], 100, (false, true)).is_empty());
    // #817: with overlapping waves a multi-wave level joins the open
    // group, adding nothing to its sum; a level alone over the cap
    // still runs alone.
    assert_eq!(
        plan_level_groups(&plans, 100, (true, false)),
        vec![0..4, 4..6, 6..7, 7..8],
    );
    // Overlapping, a level joins whenever it alone fits.
    assert_eq!(
        plan_level_groups(&plans, 100, (false, true)),
        vec![0..5, 5..6, 6..7, 7..8],
    );
    assert_eq!(
        plan_level_groups(&plans, 100, (true, true)),
        vec![0..6, 6..7, 7..8],
    );
    let joined = plan_level_groups(&multi_first, 100, (true, true));
    assert!(joined.len() == 1 && joined[0] == (0..2), "{joined:?}");
}

/// A single-wave level joins the open group when it alone fits the
/// member cap, whatever the group already holds: the in-flight gate, not
/// the group's sum, bounds what is held, so the finest level's collect
/// overlaps the coarser levels' last encodes. A level over the cap still
/// runs alone.
#[test]
fn a_single_wave_level_joins_the_open_group_when_it_alone_fits_the_cap() {
    let fits = vec![
        plan_with_members(10, &[10], 6),
        plan_with_members(11, &[20], 6),
        plan_with_members(12, &[30], 6), // 10+20+30 > 40, 30 <= 40
    ];
    assert_eq!(plan_level_groups(&fits, 40, (true, true)), vec![0..3]);
    // `bounded` keeps the group's sum within the cap.
    assert_eq!(
        plan_level_groups(&fits, 40, (true, false)),
        vec![0..2, 2..3]
    );
    let over = vec![
        plan_with_members(10, &[10], 6),
        plan_with_members(11, &[20], 6),
        plan_with_members(12, &[50], 6), // 50 > 40
    ];
    assert_eq!(plan_level_groups(&over, 40, (true, true)), vec![0..2, 2..3]);
}

/// #817: a group's members in flight never exceed the export's largest
/// wave, or the narrowest wave of full partitions when that is larger,
/// plus the slack, and never the member cap.
#[test]
fn the_in_flight_cap_is_the_largest_wave() {
    let plans = vec![
        plan_with_members(3, &[10], 6),
        plan_with_members(4, &[20], 6),
        plan_with_members(5, &[30], 6),
    ];
    let cap = |member_cap, plans: &[LevelPlan], target| {
        group_schedule(member_cap, plans, target, MemoryProfile::Auto)
            .1
            .cap
    };
    assert_eq!(cap(100, &plans, 1), with_wave_slack(30));
    assert_eq!(cap(31, &plans, 1), 31);
    // A multi-wave level's largest wave, not its whole band, is the bound.
    let multi = vec![
        plan_with_members(3, &[10], 6),
        plan_with_members(4, &[10], 6),
        plan_with_members(5, &[3; 12], 6),
    ];
    assert_eq!(cap(100, &multi, 1), with_wave_slack(18));
    // The narrowest wave of full partitions is the floor, and the serial
    // cap stays 0.
    assert_eq!(
        cap(1_000, &plans, 64),
        with_wave_slack(64 * PARTITION_WAVE_MIN)
    );
    assert_eq!(cap(0, &plans, 64), 0);
}

/// #815: under `bounded` a multi-wave level's next wave is admitted
/// while the wave before it still encodes its last partitions, as long
/// as those hold at most the slack; members in flight never pass the
/// largest wave plus the slack.
#[test]
fn bounded_admits_the_next_wave_beside_the_last_partitions_of_the_one_before() {
    // Two waves of eight 4-member partitions: waves of 32 members.
    let plans = vec![plan_with_members(5, &[4; 16], 8)];
    let (groups, gate) = group_schedule(1_000, &plans, 1, MemoryProfile::Bounded);
    assert_eq!(groups, vec![0..1]);
    assert!(
        gate.waves,
        "bounded runs its waves through the group pipeline"
    );
    assert_eq!(gate.cap, with_wave_slack(32));
    assert_eq!(gate.cap, 36);
    let in_flight = InFlight::new(gate.cap);
    let first = in_flight.admit(32);
    std::thread::scope(|s| {
        // Two partitions still encoding (8 members, past the slack of 4):
        // the next wave waits.
        first.encoded(24, &[]);
        let next = s.spawn(|| {
            let wave = in_flight.admit(32);
            // Beside the last partition, at the cap and not past it.
            assert_eq!(*in_flight.held.lock().unwrap(), 36);
            drop(wave);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!next.is_finished(), "admitted past the slack");
        // One partition left (4 members): the next wave goes in beside it.
        first.encoded(4, &[]);
        next.join().unwrap();
    });
    // The last partition still holds its 4.
    assert_eq!(*in_flight.held.lock().unwrap(), 4);
    drop(first);
    assert_eq!(*in_flight.held.lock().unwrap(), 0);
    // A level the density guard narrowed to two quarter-size partitions
    // holds its wave plus the slack, not six full-target partitions.
    let narrowed = vec![plan_with_members(5, &[4; 16], 2)];
    let (_, gate) = group_schedule(1_000, &narrowed, 100, MemoryProfile::Bounded);
    assert_eq!(gate.cap, with_wave_slack(8));
    // Auto takes the same slack; a member cap below the wave still
    // bounds it.
    assert_eq!(
        group_schedule(1_000, &plans, 1, MemoryProfile::Auto).1.cap,
        36
    );
    assert_eq!(
        group_schedule(30, &plans, 1, MemoryProfile::Bounded).1.cap,
        30
    );
}

/// #815: under `bounded`, small single-wave levels share one group and
/// overlap under the full-target floor, as before the multi-wave slack
/// cap; only a group with a multi-wave level takes that cap.
#[test]
fn bounded_small_single_wave_levels_keep_the_floor() {
    let plans = vec![
        plan_with_members(3, &[10], 6),
        plan_with_members(4, &[20], 6),
        plan_with_members(5, &[30], 6),
        plan_with_members(6, &[40], 6),
    ];
    let (groups, gate) = group_schedule(10_000, &plans, 64, MemoryProfile::Bounded);
    assert_eq!(groups, vec![0..4]);
    assert_eq!(gate.single_wave_cap, 64 * PARTITION_WAVE_MIN);
    assert_eq!(gate.cap, with_wave_slack(40));
    // The budget still bounds it.
    assert_eq!(
        group_schedule(100, &plans, 64, MemoryProfile::Bounded)
            .1
            .single_wave_cap,
        100
    );
}

/// #817: the collect waits while the next level would take the members
/// in flight past the cap, and goes on once an encode releases them. A
/// level larger than the cap is admitted when nothing is in flight.
#[test]
fn in_flight_admits_a_level_only_within_the_cap() {
    let gate = InFlight::new(5);
    let first = gate.admit(4);
    std::thread::scope(|s| {
        let waiter = s.spawn(|| drop(gate.admit(3)));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!waiter.is_finished(), "admitted 7 members past a cap of 5");
        drop(first);
        waiter.join().unwrap();
    });
    drop(gate.admit(9));
    assert_eq!(*gate.held.lock().unwrap(), 0);
}

/// #817: small single-wave levels whose summed members exceed the
/// largest wave still form one concurrent group: the cap bounds members
/// in flight, not the group.
#[test]
fn small_levels_stay_one_concurrent_group() {
    let input = tempfile::NamedTempFile::new().unwrap();
    let level = |l: i64| -> (Vec<i64>, Vec<Geometry<f64>>) {
        let pts = (0..5)
            .map(|k| Geometry::Point(Point::new(-150.3 + k as f64 * 70.0, -40.3 + l as f64)))
            .collect();
        ((0..5).map(|k| l * 10 + k).collect(), pts)
    };
    write_mode_fixture(
        input.path(),
        &[level(0), level(1), level(2), level(3)],
        Mode::Duplicating,
        64,
    );
    let opts = ExportOptions {
        layer_name: "small".to_string(),
        partition_wave: 64,
        ..Default::default()
    };
    let run = |concurrent_levels: bool| {
        let out = tempfile::NamedTempFile::new().unwrap();
        let run = export_pmtiles_run(
            input.path(),
            out.path(),
            &opts,
            1,
            &knobs(false, None, concurrent_levels),
        )
        .unwrap();
        (std::fs::read(out.path()).unwrap(), run)
    };
    let (serial_bytes, _) = run(false);
    let (concurrent_bytes, concurrent) = run(true);
    assert_eq!(concurrent.level_groups, ONE_GROUP_OF_FOUR);
    assert_eq!(concurrent_bytes, serial_bytes);
}

/// #817: a multi-wave level joins the level group before it and its
/// waves run through the group's pipeline, so the next wave is read
/// while one encodes; under `bounded` too since #815.
#[test]
fn multi_wave_levels_overlap_their_waves() {
    let input = tempfile::NamedTempFile::new().unwrap();
    let pts = |n: usize, l: i64| -> (Vec<i64>, Vec<Geometry<f64>>) {
        let g = (0..n)
            .map(|k| Geometry::Point(Point::new(-150.3 + k as f64 * 70.0, -40.3)))
            .collect();
        ((0..n as i64).map(|k| l * 10 + k).collect(), g)
    };
    write_mode_fixture(input.path(), &[pts(1, 0), pts(5, 1)], Mode::Duplicating, 64);
    let run = |profile: MemoryProfile, concurrent_levels: bool| {
        let opts = ExportOptions {
            layer_name: "waves".to_string(),
            partition_wave: 2,
            profile,
            // The 8 px buffer puts each edge point in both of its tiles.
            tile_buffer: 8,
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        let run = export_pmtiles_run(
            input.path(),
            out.path(),
            &opts,
            1,
            &knobs(false, None, concurrent_levels),
        )
        .unwrap();
        (std::fs::read(out.path()).unwrap(), run)
    };
    let (serial_bytes, serial) = run(MemoryProfile::Auto, false);
    assert_eq!(serial.work.group_waves, 0);
    let (auto_bytes, auto) = run(MemoryProfile::Auto, true);
    let groups = &auto.level_groups;
    assert!(groups.len() == 1 && groups[0] == (0..2), "{groups:?}");
    // The 5 points sit on tile edges: 10 one-member tiles, in waves of 2.
    assert_eq!(auto.work.group_waves, 5);
    assert_eq!(auto_bytes, serial_bytes);
    assert_eq!(auto.report.zooms, serial.report.zooms);
    let (bounded_bytes, bounded) = run(MemoryProfile::Bounded, true);
    assert_eq!(bounded.level_groups, auto.level_groups);
    assert_eq!(bounded.work.group_waves, 5);
    assert_eq!(bounded_bytes, serial_bytes);
}

/// A multi-wave level's own store fills while the level before it still
/// encodes under `auto` and `speed`; under `bounded` it waits until the
/// group holds no members. The archive is the serial loop's either way.
#[test]
fn level_store_fills_beside_the_previous_level_encode() {
    let input = tempfile::NamedTempFile::new().unwrap();
    let pts = |n: usize, l: i64| -> (Vec<i64>, Vec<Geometry<f64>>) {
        let g = (0..n)
            .map(|k| Geometry::Point(Point::new(-150.3 + k as f64 * 70.0, -40.3)))
            .collect();
        ((0..n as i64).map(|k| l * 10 + k).collect(), g)
    };
    write_mode_fixture(
        input.path(),
        &[pts(1, 0), pts(5, 1), pts(5, 2)],
        Mode::Duplicating,
        64,
    );
    let run = |profile: MemoryProfile, concurrent_levels: bool| {
        let opts = ExportOptions {
            layer_name: "waves".to_string(),
            partition_wave: 2,
            profile,
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        let run = export_pmtiles_run(
            input.path(),
            out.path(),
            &opts,
            1,
            &Pass2Knobs {
                hold_for_fill: true,
                ..knobs(false, None, concurrent_levels)
            },
        )
        .unwrap();
        (std::fs::read(out.path()).unwrap(), run)
    };
    let (serial_bytes, _) = run(MemoryProfile::Auto, false);
    for (profile, beside) in [
        (MemoryProfile::Auto, 2),
        (MemoryProfile::Speed, 2),
        (MemoryProfile::Bounded, 0),
    ] {
        let (bytes, run) = run(profile, true);
        assert_eq!(run.level_groups, vec![0..3], "{profile:?}");
        assert_eq!(run.work.fills_beside_encode, beside, "{profile:?}");
        assert_eq!(bytes, serial_bytes, "{profile:?}");
    }
}

/// A coarse wave that stalls does not let the finer levels' encoded
/// waves queue behind it past the group's cap: each queued wave keeps a
/// share of the cap until it is written.
#[test]
fn a_stalled_coarse_wave_bounds_the_encoded_waves_queued_behind_it() {
    let input = tempfile::NamedTempFile::new().unwrap();
    // Seven levels of one point in one tile: single-wave levels of one
    // member under a cap of 2, all in one group.
    let levels: Vec<(Vec<i64>, Vec<Geometry<f64>>)> = (0..7)
        .map(|l| (vec![l], vec![Geometry::Point(Point::new(37.123, 23.456))]))
        .collect();
    write_mode_fixture(input.path(), &levels, Mode::Duplicating, 64);
    let run = |profile: MemoryProfile, concurrent_levels: bool| {
        let opts = ExportOptions {
            layer_name: "stall".to_string(),
            partition_wave: 2,
            profile,
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        let run = export_pmtiles_run(
            input.path(),
            out.path(),
            &opts,
            1,
            &Pass2Knobs {
                stall_first_wave: true,
                ..knobs(false, None, concurrent_levels)
            },
        )
        .unwrap();
        (std::fs::read(out.path()).unwrap(), run)
    };
    let (serial_bytes, _) = run(MemoryProfile::Auto, false);
    for profile in [MemoryProfile::Auto, MemoryProfile::Speed] {
        let (bytes, run) = run(profile, true);
        assert_eq!(run.level_groups, vec![0..7], "{profile:?}");
        assert!(
            run.report.zooms.iter().all(|z| z.tile_count == 1),
            "{profile:?}"
        );
        // The stalled wave holds 1 of the cap's 2; one queued wave holds
        // the other. With one worker the stalled encode holds it, so no
        // finer level encodes and nothing queues.
        let queued = usize::from(rayon::current_num_threads() > 1);
        assert_eq!(run.work.peak_queued_waves, queued, "{profile:?}");
        assert_eq!(bytes, serial_bytes, "{profile:?}");
    }
}

/// An early fill's wave grant covers every member still in flight, even
/// a preceding wave the gate let past its cap because nothing else was
/// held.
#[test]
fn early_fill_grant_covers_an_oversized_preceding_wave() {
    let input = tempfile::NamedTempFile::new().unwrap();
    // Three 10-point clusters, one z2 tile each: waves of 20 and 10
    // members against a cap of 2. Then 5 edge points: waves of 2.
    let cluster: Vec<Geometry<f64>> = [-134.7, -44.7, 45.3]
        .iter()
        .flat_map(|&x| std::iter::repeat_n(Geometry::Point(Point::new(x, 30.3)), 10))
        .collect();
    let edge: Vec<Geometry<f64>> = (0..5)
        .map(|k| Geometry::Point(Point::new(-150.3 + f64::from(k) * 70.0, -40.3)))
        .collect();
    write_mode_fixture(
        input.path(),
        &[((0..30).collect(), cluster), ((100..105).collect(), edge)],
        Mode::Duplicating,
        64,
    );
    for profile in [
        MemoryProfile::Auto,
        MemoryProfile::Speed,
        MemoryProfile::Bounded,
    ] {
        let opts = ExportOptions {
            layer_name: "waves".to_string(),
            partition_wave: 2,
            profile,
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        let run = export_pmtiles_run(
            input.path(),
            out.path(),
            &opts,
            1,
            &Pass2Knobs {
                hold_for_fill: true,
                // A fixed budget: macOS probes no memory, and a run
                // without a budget takes no grants.
                budget_available: Some(64 << 30),
                ..knobs(false, None, true)
            },
        )
        .unwrap();
        assert_eq!(run.level_groups, vec![0..2], "{profile:?}");
        let fills = &run.work.fill_grants;
        assert_eq!(fills.len(), 2, "{profile:?}: {fills:?}");
        assert!(
            fills.iter().all(|&(held, units)| units >= held),
            "{profile:?}: {fills:?}"
        );
    }
}

/// #647: the group cap is one full ceiling-width wave of target-sized
/// partitions, narrowed by the same RAM budget the #311 per-level guard
/// uses when the member size and RAM are both known.
#[test]
fn level_group_member_cap_respects_the_wave_ram_budget() {
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    // Unknown member size or RAM: the wave-shaped cap alone.
    assert_eq!(
        level_group_member_cap(
            6,
            32_768,
            None,
            MemoryProfile::Auto,
            Some(&MemoryBudget::with_available(Some(GIB)))
        ),
        6 * 32_768
    );
    assert_eq!(
        level_group_member_cap(6, 32_768, Some(600), MemoryProfile::Auto, None),
        6 * 32_768
    );
    // A member size of 0 is unknown too, not a free member.
    assert_eq!(
        level_group_member_cap(
            6,
            32_768,
            Some(0),
            MemoryProfile::Auto,
            Some(&MemoryBudget::with_available(Some(0)))
        ),
        6 * 32_768
    );
    // Roomy RAM never widens past the wave-shaped cap.
    assert_eq!(
        level_group_member_cap(
            6,
            32_768,
            Some(100),
            MemoryProfile::Auto,
            Some(&MemoryBudget::with_available(Some(256 * GIB)))
        ),
        6 * 32_768
    );
    // Tight RAM narrows it: 1 GiB × 0.5 / 2000 B resident = 268_435.
    assert_eq!(
        level_group_member_cap(
            16,
            32_768,
            Some(2000),
            MemoryProfile::Auto,
            Some(&MemoryBudget::with_available(Some(GIB)))
        ),
        (GIB / 2 / 2000) as usize
    );
}

/// The three pass-2 paths the #647 tests run: the legacy per-wave read,
/// and the single-read store under RAM and spill backing.
const PASS2_PATHS: [(bool, Option<SinkBacking>); 3] = [
    (true, None),
    (false, Some(SinkBacking::Ram)),
    (false, Some(SinkBacking::Spill)),
];

/// Test knobs with the checkpoint throttle at zero, so a checkpoint is
/// due after every level.
fn knobs(
    force_legacy_pass2: bool,
    backing_override: Option<SinkBacking>,
    concurrent_levels: bool,
) -> Pass2Knobs {
    Pass2Knobs {
        force_legacy_pass2,
        backing_override,
        concurrent_levels,
        checkpoint_interval: Duration::ZERO,
        fail_level: None,
        panic_write_level: None,
        hold_for_fill: false,
        stall_first_wave: false,
        memory_limit: None,
        budget_available: None,
    }
}

/// Run the duplicating-mode wave path of [`equivalence_fixture`] with one
/// tile per partition and one partition per wave, so each multi-tile
/// level takes several waves (#740's shape: more partitions than the wave
/// is wide), on the production path. Returns the run and the per-level
/// band row counts.
fn multi_wave_duplicating_run() -> (Pass2Run, BTreeMap<usize, u64>) {
    let input = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(input.path());
    let out = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        layer_name: "ref".to_string(),
        partition_wave: 1,
        ..Default::default()
    };
    let run = export_pmtiles_run(
        input.path(),
        out.path(),
        &opts,
        1,
        &knobs(false, None, false),
    )
    .unwrap();
    let band_rows = run
        .report
        .zooms
        .iter()
        .map(|z| (z.level, z.level_feature_count as u64))
        .collect();
    (run, band_rows)
}

/// #817 review: a panic while a group writes a wave fails the group and
/// re-raises, as an encode panic does. Before, the write ran outside the
/// encode's `catch_unwind`, so the next wave of a multi-wave level waited
/// for that write forever.
#[test]
fn level_group_write_panic_does_not_hang() {
    // #815: `bounded` runs its multi-wave levels through the group too.
    for profile in [MemoryProfile::Auto, MemoryProfile::Bounded] {
        let input = tempfile::NamedTempFile::new().unwrap();
        equivalence_fixture(input.path());
        let opts = ExportOptions {
            layer_name: "ref".to_string(),
            partition_wave: 1,
            profile,
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let good = export_pmtiles_run(
            input.path(),
            dir.path().join("good.pmtiles"),
            &opts,
            1,
            &knobs(false, None, true),
        )
        .unwrap();
        let level = good
            .report
            .zooms
            .iter()
            .find(|z| z.tile_count >= 3)
            .expect("the fixture must give a level three waves")
            .level;
        let (tx, rx) = std::sync::mpsc::channel();
        let out = dir.path().join("panics.pmtiles");
        std::thread::spawn(move || {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                export_pmtiles_run(
                    input.path(),
                    &out,
                    &opts,
                    1,
                    &Pass2Knobs {
                        panic_write_level: Some(level),
                        ..knobs(false, None, true)
                    },
                )
                .map(|_| ())
            }));
            let _ = tx.send(run.is_err());
        });
        let panicked = rx
            .recv_timeout(Duration::from_secs(30))
            .unwrap_or_else(|_| panic!("{profile:?}: a panicking group write hung the export"));
        assert!(
            panicked,
            "{profile:?}: the write panic must reach the caller"
        );
    }
}

/// #740 (read amplification): a duplicating-mode level split into several
/// waves must read its band's rows once, not once per wave: re-reading
/// each row group a wave's bbox touches multiplies the band read.
#[test]
fn multi_wave_level_reads_each_band_row_once() {
    let (run, band_rows) = multi_wave_duplicating_run();
    // One tile per partition and one partition per wave: a level of
    // several tiles takes several waves.
    assert!(
        run.report.zooms.iter().any(|z| z.tile_count > 1),
        "the fixture must give some level several waves"
    );
    assert_eq!(
        run.work.band_rows_read, band_rows,
        "rows read per level (left) vs the level's band rows (right)"
    );
}

/// #740: a multi-wave duplicating level's single read, into a one-level
/// member store under either backing, writes the same archive and report
/// as the per-wave reads, with narrow waves so levels take several.
#[test]
fn multi_wave_level_single_read_matches_per_wave_reads() {
    let input = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(input.path());
    for (target, wave) in [(1usize, 1usize), (1, 3), (DEFAULT_PARTITION_TARGET, 1)] {
        let opts = ExportOptions {
            layer_name: "ref".to_string(),
            partition_wave: wave,
            ..Default::default()
        };
        let run = |force_legacy: bool, backing: Option<SinkBacking>| {
            let out = tempfile::NamedTempFile::new().unwrap();
            let run = export_pmtiles_run(
                input.path(),
                out.path(),
                &opts,
                target,
                &knobs(force_legacy, backing, false),
            )
            .unwrap();
            (std::fs::read(out.path()).unwrap(), run)
        };
        let (legacy_bytes, legacy) = run(true, None);
        for backing in [SinkBacking::Ram, SinkBacking::Spill] {
            let (bytes, single) = run(false, Some(backing));
            assert_eq!(
                bytes, legacy_bytes,
                "single read ({backing:?}, target {target}, wave {wave}) diverges"
            );
            assert_eq!(single.report.zooms, legacy.report.zooms);
            assert_eq!(single.report.total_tiles, legacy.report.total_tiles);
            if target == 1 && wave < 3 {
                assert_ne!(
                    single.work.band_rows_read, legacy.work.band_rows_read,
                    "the per-wave oracle must re-read for this to test anything"
                );
            }
        }
    }
}

/// #740: a multi-wave level's one-level store keeps the level in RAM
/// when the run's budget affords it, spilling nothing, and spills what
/// the budget cannot hold. Both write the per-wave reads' archive.
#[test]
fn multi_wave_level_store_spills_only_what_the_budget_cannot_hold() {
    let input = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(input.path());
    let opts = ExportOptions {
        layer_name: "ref".to_string(),
        partition_wave: 1,
        ..Default::default()
    };
    let run = |force_legacy: bool, budget_available: Option<u64>| {
        let out = tempfile::NamedTempFile::new().unwrap();
        let run = export_pmtiles_run(
            input.path(),
            out.path(),
            &opts,
            1,
            &Pass2Knobs {
                budget_available,
                ..knobs(force_legacy, None, false)
            },
        )
        .unwrap();
        (std::fs::read(out.path()).unwrap(), run)
    };
    let (legacy, _) = run(true, None);

    let (bytes, roomy) = run(false, Some(100 << 30));
    assert_eq!(bytes, legacy, "the in-RAM level store diverges");
    assert!(
        !roomy.work.level_spill_bytes.is_empty(),
        "the fixture must give some level several waves"
    );
    assert!(
        roomy.work.level_spill_bytes.values().all(|&b| b == 0),
        "a level the budget affords spilled: {:?}",
        roomy.work.level_spill_bytes
    );

    let (bytes, tight) = run(false, Some(1 << 20));
    assert_eq!(bytes, legacy, "the spilling level store diverges");
    assert!(
        tight.work.level_spill_bytes.values().any(|&b| b > 0),
        "a level the budget cannot hold must spill: {:?}",
        tight.work.level_spill_bytes
    );

    // #810: the profile's per-zoom spill_bytes and wave_width are the
    // level store's own figures.
    for run in [&roomy, &tight] {
        let profiled: Vec<u64> = run.per_zoom.iter().map(|z| z.spill_bytes).collect();
        let spilled: Vec<u64> = (0..run.per_zoom.len())
            .map(|l| run.work.level_spill_bytes.get(&l).copied().unwrap_or(0))
            .collect();
        assert_eq!(
            profiled, spilled,
            "per_zoom spill_bytes vs the level stores"
        );
        assert!(
            run.per_zoom.iter().all(|z| z.wave_width == 1),
            "partition_wave: 1"
        );
    }
}

/// The export-wide #235 store and the wave width follow the run's
/// memory profile as convert's sinks do (#788): `speed` keeps the store
/// in RAM and sizes the wave from the cores however tight the budget,
/// `bounded` spills the store however roomy, `auto` decides from the
/// budget; the archive is the same bytes under all of them.
#[test]
fn export_store_and_wave_follow_the_memory_profile() {
    let input = tempfile::NamedTempFile::new().unwrap();
    partitioning_equivalence_fixture(input.path());
    let cores = std::thread::available_parallelism().map_or(PARTITION_WAVE_MIN, |n| n.get());
    let run = |profile: MemoryProfile, available: u64| {
        let opts = ExportOptions {
            layer_name: "ref".to_string(),
            profile,
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        let run = export_pmtiles_run(
            input.path(),
            out.path(),
            &opts,
            DEFAULT_PARTITION_TARGET,
            &Pass2Knobs {
                budget_available: Some(available),
                ..knobs(false, None, true)
            },
        )
        .unwrap();
        assert!(run.single_read, "the fixture must take the #235 store");
        (
            std::fs::read(out.path()).unwrap(),
            run.store_backing,
            run.ceiling_wave,
        )
    };
    let (reference, backing, _) = run(MemoryProfile::Auto, 100 << 30);
    assert!(
        matches!(backing, Some(SinkBacking::Ram)),
        "auto with room: RAM"
    );
    let (bytes, backing, wave) = run(MemoryProfile::Auto, 1 << 20);
    assert!(
        matches!(backing, Some(SinkBacking::Spill)),
        "auto when tight: spill"
    );
    assert_eq!(wave, PARTITION_WAVE_MIN, "auto when tight: the floor");
    assert_eq!(bytes, reference);
    let (bytes, backing, wave) = run(MemoryProfile::Speed, 1 << 20);
    assert!(
        matches!(backing, Some(SinkBacking::Ram)),
        "speed: RAM however tight"
    );
    assert_eq!(
        wave,
        cores.max(PARTITION_WAVE_MIN),
        "speed: the cores, no memory cap"
    );
    assert_eq!(bytes, reference);
    let (bytes, backing, _) = run(MemoryProfile::Bounded, 100 << 30);
    assert!(
        matches!(backing, Some(SinkBacking::Spill)),
        "bounded: spill however roomy"
    );
    assert_eq!(bytes, reference);
    let (_, _, wave) = run(MemoryProfile::Bounded, 1 << 20);
    assert_eq!(wave, PARTITION_WAVE_MIN, "bounded keeps main's memory cap");
}

/// The level store follows the run's memory profile (#788): `bounded`
/// holds main's shape, spilling every multi-wave level however roomy
/// the budget; `speed` keeps every level in RAM however tight; the
/// archive is the same bytes under all three.
#[test]
fn level_store_backing_follows_the_memory_profile() {
    let input = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(input.path());
    let run = |profile: MemoryProfile, budget_available: Option<u64>| {
        let opts = ExportOptions {
            layer_name: "ref".to_string(),
            partition_wave: 1,
            profile,
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        let run = export_pmtiles_run(
            input.path(),
            out.path(),
            &opts,
            1,
            &Pass2Knobs {
                budget_available,
                ..knobs(false, None, false)
            },
        )
        .unwrap();
        (
            std::fs::read(out.path()).unwrap(),
            run.work.level_spill_bytes,
        )
    };
    let (auto, auto_spill) = run(MemoryProfile::Auto, Some(100 << 30));
    assert!(
        !auto_spill.is_empty() && auto_spill.values().all(|&b| b == 0),
        "auto with room keeps every level in RAM: {auto_spill:?}"
    );
    let (bounded, bounded_spill) = run(MemoryProfile::Bounded, Some(100 << 30));
    assert_eq!(bounded, auto, "bounded changed the archive");
    assert!(
        bounded_spill.values().all(|&b| b > 0),
        "bounded must spill every multi-wave level: {bounded_spill:?}"
    );
    let (speed, speed_spill) = run(MemoryProfile::Speed, Some(1 << 20));
    assert_eq!(speed, auto, "speed changed the archive");
    assert!(
        speed_spill.values().all(|&b| b == 0),
        "speed must keep every level in RAM: {speed_spill:?}"
    );
}

/// A partition whose tiles sit on both sides of the antimeridian (the
/// Chatham Islands at -176.5 and the NZ mainland at 174.8) must prune its
/// read to those two regions, not to a union spanning every longitude
/// that also selects the row group at 140 in between. One row per row group, so
/// rows read count the row groups each wave selects.
#[test]
fn antimeridian_partition_prunes_to_both_sides_only() {
    let chatham = Geometry::Point(Point::new(-176.5, -43.9));
    let mainland = Geometry::Point(Point::new(174.8, -41.3));
    let between = Geometry::Point(Point::new(140.0, -42.0));
    let rows = vec![chatham, mainland, between];
    let input = tempfile::NamedTempFile::new().unwrap();
    write_mode_fixture(
        input.path(),
        &[
            (vec![0, 1, 2], rows.clone()),
            (vec![0, 1, 2], rows.clone()),
            (vec![0, 1, 2], rows),
        ],
        Mode::Duplicating,
        1,
    );
    // The fixture's levels are z2, z4 and z6.
    let (level, zoom) = (2, 6);
    let key = |x: f64, y: f64| {
        let t = crate::tile::lng_lat_to_tile(x, y, zoom);
        tile_key(t.x, t.y, zoom)
    };
    let (c, m, b) = (key(-176.5, -43.9), key(174.8, -41.3), key(140.0, -42.0));
    assert!(
        !(c.min(m)..=c.max(m)).contains(&b),
        "the fixture needs Chatham and the mainland adjacent in tile order"
    );
    // A target of two members puts Chatham and the mainland in one
    // partition and the row in between in another.
    let opts = ExportOptions {
        layer_name: "ref".to_string(),
        partition_wave: 1,
        ..Default::default()
    };
    let out = tempfile::NamedTempFile::new().unwrap();
    let legacy = export_pmtiles_run(
        input.path(),
        out.path(),
        &opts,
        2,
        &knobs(true, None, false),
    )
    .unwrap();
    assert_eq!(
        legacy.work.band_rows_read.get(&level),
        Some(&3),
        "z{zoom}: each of the level's three row groups is read once, by the wave over its tiles"
    );
}

/// #740 (parallelism): the per-row collect work (geometry decode,
/// property materialization, member routing) must not run serially on
/// the wave's consumer thread, outside the Rayon pool. On `origin/main`
/// all of it does: only the clip is parallel, and the profiled collect
/// keeps about 8 of 64 cores busy (moldova z0-14, 8.5 busy cores while
/// the consumer is in `collect_wave`, the consumer itself busy 70% of
/// that wall).
#[test]
fn wave_collect_does_no_per_row_work_on_the_consumer_thread() {
    let (run, band_rows) = multi_wave_duplicating_run();
    assert!(band_rows.values().sum::<u64>() > 0);
    assert_eq!(
        run.work.serial_collect_rows, 0,
        "rows whose collect work ran serially on the consumer thread"
    );
}

/// #817: a band read's batches are collected in groups, so a 64-thread
/// pool joins once per group instead of once per 8,192-row batch. The
/// grouping is scheduling only: one thread, which collects batch by
/// batch, writes the same bytes.
///
/// #876: `speed` gathers 2,048 rows per thread, so 8 threads take two
/// batches a group; a tight `auto` budget keeps 512 rows per thread, one
/// batch a group. Every group size writes the same bytes.
#[test]
fn band_batches_are_collected_in_groups() {
    let input = tempfile::NamedTempFile::new().unwrap();
    let points: Vec<Geometry<f64>> = (0..20_000)
        .map(|i| {
            let f = f64::from(i);
            Geometry::Point(Point::new(
                -170.0 + (f * 0.0171) % 340.0,
                -60.0 + (f * 0.0067) % 120.0,
            ))
        })
        .collect();
    let ids: Vec<i64> = (0..20_000).collect();
    write_fixture(input.path(), &[(ids, points)]);
    let opts = ExportOptions {
        layer_name: "ref".to_string(),
        ..Default::default()
    };
    let run = |threads: usize, opts: &ExportOptions, available: u64| {
        let out = tempfile::NamedTempFile::new().unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let run = pool.install(|| {
            export_pmtiles_run(
                input.path(),
                out.path(),
                opts,
                DEFAULT_PARTITION_TARGET,
                &Pass2Knobs {
                    budget_available: Some(available),
                    ..knobs(false, None, false)
                },
            )
            .unwrap()
        });
        (run.work, std::fs::read(out.path()).unwrap())
    };
    let (ample, tight) = (100 << 30, 1 << 30);
    let (wide, wide_bytes) = run(64, &opts, ample);
    assert_eq!(wide.collect_batches, 3, "20,000 rows in 8,192-row batches");
    assert_eq!(
        wide.collect_groups, 1,
        "64 threads start at 32,768 rows per group"
    );
    assert_eq!(
        wide.band_reads_on_consumer, 0,
        "a one-segment read ran on the collecting thread"
    );
    let (one, one_bytes) = run(1, &opts, ample);
    assert_eq!(one.collect_groups, 3, "one thread collects batch by batch");
    assert!(wide_bytes == one_bytes, "grouping changed the archive");
    let speed = ExportOptions {
        profile: MemoryProfile::Speed,
        ..opts.clone()
    };
    let (eight, eight_bytes) = run(8, &speed, ample);
    assert_eq!(
        eight.collect_groups, 2,
        "8 threads collect 16,384 rows a group"
    );
    assert!(
        eight_bytes == one_bytes,
        "2,048-row groups changed the archive"
    );
    let (small, small_bytes) = run(8, &opts, tight);
    assert_eq!(
        small.collect_groups, 3,
        "a tight auto budget keeps 512 rows per thread a group"
    );
    assert!(
        small_bytes == one_bytes,
        "512-row groups changed the archive"
    );
    let bounded = ExportOptions {
        profile: MemoryProfile::Bounded,
        ..opts.clone()
    };
    let (tight, tight_bytes) = run(64, &bounded, ample);
    assert_eq!(tight.collect_groups, 3, "bounded collects batch by batch");
    assert_eq!(
        tight.collect_overlaps, 0,
        "bounded held two clipped groups at once"
    );
    assert!(tight_bytes == one_bytes, "bounded changed the archive");
    // Review of 86c897dc: the serial finish dropped its measures, so the
    // peak was the last, 3,616-row batch, not a full one.
    assert_eq!(
        tight.collect_peak_arrow, one.collect_peak_arrow,
        "bounded's peak is its largest batch"
    );
}

/// #876, review of 86c897dc: a group closes before a batch would take it
/// past its row cap, so it never holds more rows than `rescale` priced.
/// At 8 threads the x3 cap is 12,288 rows, 1.5 of the 8,192-row batches:
/// gathering a second batch made 16,384-row groups, two of which pass
/// the 24 MiB share that priced two of 12,288 rows at 1 KiB a row, and
/// the measure, at the same 1 KiB a row, kept x3.
#[test]
fn collect_groups_never_pass_the_rows_they_were_priced_at() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(8)
        .build()
        .unwrap();
    pool.install(|| {
        let share = 24 << 20;
        let mut group = CollectGroup {
            share: Some(share),
            ..CollectGroup::sized(MemoryProfile::Auto, &MemoryBudget::with_available(None))
        };
        let batch = RecordBatch::try_from_iter([(
            "id",
            Arc::new(Int64Array::from((0..8_192).collect::<Vec<i64>>())) as ArrayRef,
        )])
        .unwrap();
        let mut held = Vec::new();
        for _ in 0..12 {
            group
                .push(batch.clone(), 1, &mut |b: Vec<RecordBatch>| {
                    let rows: usize = b.iter().map(RecordBatch::num_rows).sum();
                    held.push(rows);
                    Ok(Some(GroupResidency {
                        arrow: rows * 1_024,
                        members: 0,
                        rows,
                        max_batch_rows: 8_192,
                    }))
                })
                .unwrap();
        }
        assert_eq!(group.peak_scale, 3, "x3 is the largest that fits the share");
        assert!(
            held.iter().all(|&rows| 2 * rows * 1_024 <= share),
            "groups of {held:?} rows"
        );
    });
}

/// #876: the group's size per profile. `speed` gathers 2,048 rows per
/// thread under 4x the reader's batch byte cap; `bounded` one batch. `auto`
/// starts at 512 rows per thread and one byte cap and grows one step per
/// finished group while two groups at the next size, projected from the
/// measured bytes per row (Arrow plus clipped members), fit the read share;
/// a heavy group shrinks it at once, never below the start.
#[test]
fn collect_group_size_follows_the_profile_and_budget() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(64)
        .build()
        .unwrap();
    pool.install(|| {
        let limit = usize::try_from(tylertoo_io::decoded_batch_limit()).unwrap();
        let group = |profile, available: u64| {
            CollectGroup::sized(profile, &MemoryBudget::with_available(Some(available)))
        };
        let size = |g: &CollectGroup| (g.max_rows, g.max_bytes);
        let at = |scale: usize| (512 * scale * 64, scale * limit);
        // Measured: `bytes` resident over `rows`, in 8,192-row batches.
        let measured = |bytes: usize, rows: usize| GroupResidency {
            arrow: bytes / 2,
            members: bytes - bytes / 2,
            rows,
            max_batch_rows: 8_192,
        };
        let heavy = measured(1 << 40, 1 << 20);

        let mut speed = group(MemoryProfile::Speed, 1 << 30);
        assert_eq!(size(&speed), at(4));
        speed.rescale(heavy);
        assert_eq!(size(&speed), at(4), "speed does not adapt");

        let mut bounded = group(MemoryProfile::Bounded, 100 << 30);
        assert_eq!(size(&bounded), (1, limit));
        bounded.rescale(measured(1, 1));
        assert_eq!(size(&bounded), (1, limit), "bounded does not adapt");

        // Small members: one step per finished group, up to x4.
        let mut grows = group(MemoryProfile::Auto, 100 << 30);
        for scale in 1..=4 {
            assert_eq!(size(&grows), at(scale));
            grows.rescale(measured(1 << 20, grows.max_rows));
        }
        assert_eq!(size(&grows), at(4));
        // 5 GiB at x4: two groups of x3 pass the 6 GiB read share, of x2 fit.
        grows.rescale(measured(5 << 30, grows.max_rows));
        assert_eq!(size(&grows), at(2), "a heavy group shrinks at once");
        grows.rescale(heavy);
        assert_eq!(size(&grows), at(1), "never below the start");

        // Members far past the Arrow bytes on a small budget: 40 MiB a
        // group, two of which pass 1 GiB's 61 MiB read share.
        let mut stays = group(MemoryProfile::Auto, 1 << 30);
        for _ in 0..3 {
            stays.rescale(measured(40 << 20, stays.max_rows));
            assert_eq!(size(&stays), at(1));
        }
        assert_eq!(stays.peak_resident, 40 << 20);
    });
}

/// #876: the fill's group size never shows in the archive or its loss
/// report. Polygons spanning about 4 z4 tiles, whose clipped members hold
/// many times their Arrow bytes, filled into a forced-spill level store
/// over several full groups (`speed`, 2 batches a group on 8 threads),
/// one batch a group (`bounded`), and a tight `auto` budget that keeps
/// today's size because the members do not fit it: the same bytes and
/// the same source losses (specks the tile grid drops, recorded in row
/// order), with the gaps measured across group seams.
#[test]
fn fill_group_size_is_byte_invariant_on_many_tile_polygons() {
    let input = tempfile::NamedTempFile::new().unwrap();
    let rows = 2 * 8_192 + 512;
    let polygons: Vec<Geometry<f64>> = (0..rows)
        .map(|i| {
            let f = f64::from(i);
            let (x, y) = (-175.0 + (f * 0.0173) % 325.0, -75.0 + (f * 0.0071) % 125.0);
            // Every 64th a speck the tile grid drops: a source loss.
            let side = if i % 64 == 0 { 1e-7 } else { 25.0 };
            Geometry::Polygon(geo::Polygon::new(
                LineString::from(vec![
                    (x, y),
                    (x + side, y),
                    (x + side, y + side),
                    (x, y + side),
                    (x, y),
                ]),
                vec![],
            ))
        })
        .collect();
    let ids: Vec<i64> = (0..i64::from(rows)).collect();
    write_fixture(
        input.path(),
        &[
            (vec![0], vec![Geometry::Point(Point::new(0.0, 0.0))]),
            (ids, polygons),
        ],
    );
    let run = |profile: MemoryProfile, available: u64| {
        let opts = ExportOptions {
            layer_name: "ref".to_string(),
            feature_id: Some("id".into()),
            partition_wave: 2,
            profile,
            ..Default::default()
        };
        let out = tempfile::NamedTempFile::new().unwrap();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(8)
            .build()
            .unwrap();
        let run = pool.install(|| {
            export_pmtiles_run(
                input.path(),
                out.path(),
                &opts,
                1,
                &Pass2Knobs {
                    budget_available: Some(available),
                    ..knobs(false, Some(SinkBacking::Spill), false)
                },
            )
            .unwrap()
        });
        let losses: Vec<_> = run
            .report
            .source_losses
            .iter()
            .map(Result::unwrap)
            .collect();
        (run.work, std::fs::read(out.path()).unwrap(), losses)
    };
    let (one, one_bytes, one_losses) = run(MemoryProfile::Bounded, 100 << 30);
    assert!(!one_losses.is_empty(), "the valve dropped nothing");
    assert!(
        one.level_spill_bytes.values().sum::<u64>() > 0,
        "nothing spilled"
    );
    let (full, full_bytes, full_losses) = run(MemoryProfile::Speed, 100 << 30);
    assert_eq!(full.collect_peak_scale, 4);
    assert!(
        full.collect_groups < one.collect_groups,
        "{} groups vs {}",
        full.collect_groups,
        one.collect_groups
    );
    assert!(full_bytes == one_bytes, "x4 groups changed the archive");
    assert_eq!(full_losses, one_losses);
    let (tight, tight_bytes, tight_losses) = run(MemoryProfile::Auto, 256 << 20);
    assert_eq!(
        tight.collect_peak_scale, 1,
        "the members fit no larger group"
    );
    assert!(
        tight.collect_peak_members > 4 * tight.collect_peak_arrow,
        "members {} vs Arrow {} bytes",
        tight.collect_peak_members,
        tight.collect_peak_arrow
    );
    assert!(
        tight_bytes == one_bytes,
        "the tight auto fill changed the archive"
    );
    assert_eq!(tight_losses, one_losses);
}

/// #817: a collect group closes before a batch that would take it past
/// the byte cap, and at either cap; a batch over the cap alone is a
/// group of its own. On review of 81cd824 the cap was checked only
/// after the batch joined, so two 48 MiB batches made a 96 MiB group.
#[test]
fn collect_groups_stay_within_the_byte_cap() {
    let batch = |rows: i64| {
        RecordBatch::try_from_iter([(
            "id",
            Arc::new(Int64Array::from((0..rows).collect::<Vec<_>>())) as ArrayRef,
        )])
        .unwrap()
    };
    let mut group = CollectGroup {
        max_rows: 100,
        max_bytes: 64,
        ..CollectGroup::sized(MemoryProfile::Speed, &MemoryBudget::with_available(None))
    };
    let mut flushed: Vec<Vec<usize>> = Vec::new();
    let mut flush = |b: Vec<RecordBatch>| {
        flushed.push(b.iter().map(RecordBatch::num_rows).collect());
        Ok(None)
    };
    for (rows, bytes) in [(1, 48), (2, 48), (3, 10), (4, 100), (5, 1), (90, 1)] {
        group.push(batch(rows), bytes, &mut flush).unwrap();
    }
    assert_eq!(flushed, vec![vec![1], vec![2, 3], vec![4]]);
    let pending: Vec<usize> = group.batches.iter().map(RecordBatch::num_rows).collect();
    assert_eq!(pending, [5, 90], "under both caps: still gathering");
    // bounded: one batch a group.
    let mut one = CollectGroup::sized(MemoryProfile::Bounded, &MemoryBudget::with_available(None));
    let mut sizes = Vec::new();
    for rows in [3, 1, 2] {
        one.push(batch(rows), 8, &mut |b| {
            sizes.push(b.len());
            Ok(None)
        })
        .unwrap();
    }
    assert_eq!(sizes, [1, 1, 1]);
}

/// #817: a band read's input error keeps its class. A hostile page
/// header arrives from the reader as `ConvertError::Input`; flattened to
/// a string, it read as a runtime failure.
#[test]
fn band_read_errors_keep_their_class() {
    use crate::error::{Classify, ErrorKind};
    let hostile =
        || arrow_schema::ArrowError::ExternalError(Box::new(tylertoo_io::Error::HostilePageHeader));
    let e = read_error(ConvertError::Input(InputError::Arrow(hostile())));
    assert_eq!(e.kind(), ErrorKind::Input, "{e}");
    let e = read_error(ConvertError::Input(InputError::Decode(
        tylertoo_io::Error::HostilePageHeader,
    )));
    assert_eq!(e.kind(), ErrorKind::Input, "{e}");
}

/// #817 (S1): the scan sizes a batch in a few tasks, not one per pool
/// thread: dozens of accumulator maps per batch leave the pool spinning.
#[test]
fn size_bboxes_splits_a_batch_into_few_tasks() {
    let bboxes: Vec<Option<TileBounds>> = (0..EXPORT_BATCH_SIZE)
        .map(|i| {
            let x = -170.0 + (i as f64 * 0.04) % 340.0;
            Some(TileBounds::new(x, 10.0, x + 0.01, 10.01))
        })
        .collect();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(64)
        .build()
        .unwrap();
    let opts = ExportOptions::default();
    let (_, counts) = pool.install(|| size_bboxes(&bboxes, 12, &opts, true));
    assert!(counts.values().sum::<usize>() >= EXPORT_BATCH_SIZE);
    let accumulators: usize = pool
        .broadcast(|_| SIZE_ACCUMULATORS.with(std::cell::Cell::take))
        .into_iter()
        .sum();
    assert!(
        accumulators <= EXPORT_BATCH_SIZE / SIZE_BBOXES_MIN_ROWS,
        "{accumulators} accumulators for one batch"
    );
}

/// #647: running a group of levels concurrently is scheduling only. Every
/// pass-2 path (duplicating per-wave reads, the partitioning legacy read,
/// and the single-read store under both backings) must write the same
/// bytes and report as the strictly serial level loop.
#[test]
fn concurrent_levels_are_byte_identical_to_serial() {
    let t_dup = tempfile::NamedTempFile::new().unwrap();
    equivalence_fixture(t_dup.path());
    let t_part = tempfile::NamedTempFile::new().unwrap();
    partitioning_equivalence_fixture(t_part.path());
    for input in [t_dup.path(), t_part.path()] {
        for (target, wave) in [(DEFAULT_PARTITION_TARGET, 6), (4, 16)] {
            let opts = ExportOptions {
                layer_name: "ref".to_string(),
                partition_wave: wave,
                ..Default::default()
            };
            for (force_legacy_pass2, backing_override) in PASS2_PATHS {
                let run = |concurrent_levels: bool| {
                    let out = tempfile::NamedTempFile::new().unwrap();
                    let run = export_pmtiles_run(
                        input,
                        out.path(),
                        &opts,
                        target,
                        &knobs(force_legacy_pass2, backing_override, concurrent_levels),
                    )
                    .unwrap();
                    (std::fs::read(out.path()).unwrap(), run)
                };
                let (serial_bytes, serial) = run(false);
                let (concurrent_bytes, concurrent) = run(true);
                // #235: only a partitioning export reads from the store,
                // and only when the test does not force the legacy read.
                let single_read = input == t_part.path() && !force_legacy_pass2;
                assert_eq!(serial.single_read, single_read, "{input:?}");
                assert_eq!(concurrent.single_read, single_read, "{input:?}");
                assert!(
                    concurrent.level_groups.iter().any(|g| g.len() > 1),
                    "no level group formed (target {target}, wave {wave}): {:?}",
                    concurrent.level_groups
                );
                assert_eq!(
                    concurrent_bytes, serial_bytes,
                    "concurrent levels diverge (target {target}, wave {wave}, \
                     legacy {force_legacy_pass2}, {backing_override:?})"
                );
                assert_eq!(concurrent.report.zooms, serial.report.zooms);
            }
        }
    }
}

/// The schedule with all four levels of [`level_group_fixture`] in one
/// group. It is one range on purpose, not the range's elements.
#[expect(
    clippy::single_range_in_vec_init,
    reason = "#647: one range represents a group of four levels, not four individual indices"
)]
const ONE_GROUP_OF_FOUR: [std::ops::Range<usize>; 1] = [0..4];

/// Members in the dense z8 tile of [`level_group_fixture`]. Each is a
/// 5-vertex ring, so the tile crosses [`PARALLEL_ENCODE_MIN_COORDS`].
const DENSE_TILE_RINGS: usize = 1_800;

/// #647: four levels (z2, z4, z6, z8). z2–z6 hold one point each. z8
/// holds a dense tile of [`DENSE_TILE_RINGS`] small rings plus 8 points
/// in 8 other tiles, so at a partition target of 1 it spans several
/// waves.
fn level_group_fixture(path: &Path, mode: Mode) {
    let mut ids = Vec::new();
    let mut finest = Vec::new();
    for i in 0..DENSE_TILE_RINGS {
        let x = 10.2 + (i % 35) as f64 * 0.015;
        let y = 10.2 + (i / 35) as f64 * 0.015;
        ids.push(100 + i as i64);
        finest.push(Geometry::Polygon(geo::Polygon::new(
            LineString::from(vec![
                (x, y),
                (x + 0.01, y),
                (x + 0.01, y + 0.01),
                (x, y + 0.01),
                (x, y),
            ]),
            vec![],
        )));
    }
    for k in 0..8 {
        ids.push(10 + k);
        finest.push(Geometry::Point(Point::new(
            -150.3 + k as f64 * 35.0,
            -50.3 + k as f64 * 12.0,
        )));
    }
    let point = |x, y| vec![Geometry::Point(Point::new(x, y))];
    write_mode_fixture(
        path,
        &[
            (vec![0], point(-60.3, 30.3)),
            (vec![1], point(60.3, -30.3)),
            (vec![2], point(100.3, 20.3)),
            (ids, finest),
        ],
        mode,
        64,
    );
}

/// #647: a group writes each level in level order as soon as it and every
/// coarser level of the group are encoded, and takes a due checkpoint
/// after each one. With the throttle at zero, both schedules checkpoint
/// after every level but the last, and the archives match. At a target
/// of 1 the fixture forms a group of several levels followed by the
/// multi-wave z8. At the default target all four levels form one group,
/// and the dense z8 tile encodes in parallel inside it.
#[test]
fn level_groups_commit_and_checkpoint_every_level_in_order() {
    const { assert!(DENSE_TILE_RINGS * 5 >= 2 * PARALLEL_ENCODE_MIN_COORDS) };
    for mode in [Mode::Duplicating, Mode::Partitioning] {
        let input = tempfile::NamedTempFile::new().unwrap();
        level_group_fixture(input.path(), mode);
        for (target, wave) in [(1, 4), (DEFAULT_PARTITION_TARGET, 6)] {
            let opts = ExportOptions {
                layer_name: "groups".to_string(),
                partition_wave: wave,
                ..Default::default()
            };
            for (force_legacy_pass2, backing_override) in PASS2_PATHS {
                let run = |concurrent_levels: bool| {
                    let out = tempfile::NamedTempFile::new().unwrap();
                    let run = export_pmtiles_run(
                        input.path(),
                        out.path(),
                        &opts,
                        target,
                        &knobs(force_legacy_pass2, backing_override, concurrent_levels),
                    )
                    .unwrap();
                    (std::fs::read(out.path()).unwrap(), run)
                };
                let (serial_bytes, serial) = run(false);
                let (concurrent_bytes, concurrent) = run(true);
                let ctx = format!(
                    "{mode:?}, target {target}, wave {wave}, legacy {force_legacy_pass2}, \
                     {backing_override:?}"
                );
                assert_eq!(serial.level_groups, vec![0..1, 1..2, 2..3, 3..4], "{ctx}");
                let groups = &concurrent.level_groups;
                if target == 1 {
                    // #817: the multi-wave z8 level joins the group
                    // before it, its waves overlapping in the pipeline.
                    assert!(groups[0].len() > 1, "{ctx}: {groups:?}");
                    let last = groups.last().map(ExactSizeIterator::len);
                    assert!(last > Some(1), "{ctx}: {groups:?}");
                } else {
                    assert_eq!(groups, &ONE_GROUP_OF_FOUR, "{ctx}");
                }
                assert_eq!(serial.checkpoint_zooms, vec![2, 4, 6], "{ctx}");
                assert_eq!(concurrent.checkpoint_zooms, vec![2, 4, 6], "{ctx}");
                assert_eq!(concurrent_bytes, serial_bytes, "{ctx}");
                assert_eq!(concurrent.report.zooms, serial.report.zooms, "{ctx}");
            }
        }
    }
}

/// #229: a checkpoint is skipped until the throttle interval has passed
/// since the last one, after a serial level and after a level of a group.
#[test]
fn checkpoint_throttle_skips_levels_inside_the_interval() {
    let input = tempfile::NamedTempFile::new().unwrap();
    level_group_fixture(input.path(), Mode::Duplicating);
    let opts = ExportOptions {
        layer_name: "groups".to_string(),
        partition_wave: 6,
        ..Default::default()
    };
    for concurrent_levels in [false, true] {
        let out = tempfile::NamedTempFile::new().unwrap();
        let run = export_pmtiles_run(
            input.path(),
            out.path(),
            &opts,
            DEFAULT_PARTITION_TARGET,
            &Pass2Knobs {
                checkpoint_interval: Duration::from_secs(3600),
                ..knobs(true, None, concurrent_levels)
            },
        )
        .unwrap();
        assert_eq!(run.report.zooms.len(), 4);
        assert!(
            run.checkpoint_zooms.is_empty(),
            "concurrent {concurrent_levels}: {:?}",
            run.checkpoint_zooms
        );
    }
}

/// #647: a level's work time is its own read, clip, encode and gzip. A
/// serial level's is its wall less its write. A grouped level's is its
/// collect plus its encode, without the wait for a coarser level.
#[test]
fn level_walls_count_only_the_level_own_work() {
    let serial = LevelWalls::serial(5.0, 1.5);
    assert_eq!((serial.wall_secs, serial.work_secs), (5.0, 3.5));
    let grouped = LevelWalls::grouped(9.0, 2.0, 3.0);
    assert_eq!((grouped.wall_secs, grouped.work_secs), (9.0, 5.0));
}

/// #647: when a level fails inside a group, the coarser levels of that
/// group are already written and checkpointed, so the `.partial` salvage
/// archive holds exactly their tiles.
#[test]
fn level_group_failure_keeps_committed_levels_salvageable() {
    for mode in [Mode::Duplicating, Mode::Partitioning] {
        let input = tempfile::NamedTempFile::new().unwrap();
        level_group_fixture(input.path(), mode);
        for profile in [MemoryProfile::Auto, MemoryProfile::Bounded] {
            let opts = ExportOptions {
                layer_name: "groups".to_string(),
                partition_wave: 6,
                profile,
                ..Default::default()
            };
            for (force_legacy_pass2, backing_override) in PASS2_PATHS {
                let ctx = format!(
                    "{mode:?}, {profile:?}, legacy {force_legacy_pass2}, {backing_override:?}"
                );
                let dir = tempfile::tempdir().unwrap();
                let good = export_pmtiles_run(
                    input.path(),
                    dir.path().join("good.pmtiles"),
                    &opts,
                    DEFAULT_PARTITION_TARGET,
                    &knobs(force_legacy_pass2, backing_override, true),
                )
                .unwrap();
                assert_eq!(good.level_groups, ONE_GROUP_OF_FOUR, "{ctx}");
                let coarse_tiles: usize = good.report.zooms[..3].iter().map(|z| z.tile_count).sum();

                let out = dir.path().join("failed.pmtiles");
                let err = export_pmtiles_run(
                    input.path(),
                    &out,
                    &opts,
                    DEFAULT_PARTITION_TARGET,
                    &Pass2Knobs {
                        fail_level: Some(3),
                        ..knobs(force_legacy_pass2, backing_override, true)
                    },
                )
                .err()
                .unwrap_or_else(|| panic!("{ctx}: the injected fault must fail the export"));
                assert!(err.to_string().contains("injected"), "{ctx}: {err}");
                let partial =
                    std::fs::read(out.with_extension("pmtiles.partial")).unwrap_or_else(|e| {
                        panic!("{ctx}: no salvage archive after a mid-group failure: {e}")
                    });
                let header = crate::pmtiles_writer::Header::from_bytes(&partial)
                    .unwrap_or_else(|e| panic!("{ctx}: salvage archive is invalid: {e}"));
                assert_eq!(
                    header.addressed_tiles_count, coarse_tiles as u64,
                    "{ctx}: the salvage archive must hold exactly z2–z6"
                );
            }
        }
    }
}

/// #647: the parallel geometry encode is chosen by the tile's vertex
/// count, not its member count. A few large polygons go parallel. Many
/// points stay serial, and so does one member of any size.
#[test]
fn parallel_encode_threshold_counts_vertices() {
    let member = |geom: Geometry| Member {
        key: 0,
        seq: 0,
        rank: point_rank(0),
        geom: geom.into(),
        props: SharedProps::new(Vec::new()),
        id: None,
        gap: GAP_NONE,
        band: 0,
    };
    let quarter = PARALLEL_ENCODE_MIN_COORDS / 4;
    let ring: Vec<(f64, f64)> = (0..=quarter)
        .map(|k| {
            let a = k as f64 / quarter as f64 * std::f64::consts::TAU;
            (a.cos(), a.sin())
        })
        .collect();
    let few_big: Vec<Member> = (0..4)
        .map(|_| {
            member(Geometry::Polygon(geo::Polygon::new(
                LineString::from(ring.clone()),
                vec![],
            )))
        })
        .collect();
    assert!(encode_in_parallel(&few_big));
    assert!(!encode_in_parallel(&few_big[..1]));

    let many_points: Vec<Member> = (0..PARALLEL_ENCODE_MIN_COORDS - 1)
        .map(|i| member(Geometry::Point(Point::new(i as f64 * 1e-4, 0.0))))
        .collect();
    assert!(!encode_in_parallel(&many_points));
    let refs: Vec<&Member> = many_points.iter().collect();
    assert!(!encode_in_parallel(&refs));
}

/// #647: a tile with many vertices encodes its geometries in parallel,
/// and a small tile encodes them serially. Both must match the serial
/// `add_feature` loop: same first-encounter key/value tables, same
/// feature order, same counts of empty and collapsed members.
#[test]
fn build_mvt_parallel_geometry_encode_matches_serial() {
    let tb = TileCoord::new(0, 0, 1).bounds();
    let opts = ExportOptions::default();
    let n = PARALLEL_ENCODE_MIN_COORDS;
    let members: Vec<Member> = (0..n)
        .map(|i| {
            let x = -170.0 + (i % 300) as f64 * 0.5;
            let y = 10.0 + (i / 300) as f64 * 0.5;
            let geom = match i % 5 {
                0 => Geometry::Point(Point::new(x, y)),
                1 => Geometry::Polygon(geo::Polygon::new(
                    LineString::from(vec![(x, y), (x + 0.4, y), (x + 0.4, y + 0.4), (x, y)]),
                    vec![],
                )),
                // Collapses at the tile extent: a quantized drop.
                2 => Geometry::Polygon(geo::Polygon::new(
                    LineString::from(vec![(x, y), (x + 1e-9, y), (x + 1e-9, y + 1e-9), (x, y)]),
                    vec![],
                )),
                // No coordinates: an unencodable drop.
                3 => Geometry::GeometryCollection(geo::GeometryCollection::new_from(vec![])),
                _ => Geometry::GeometryCollection(geo::GeometryCollection::new_from(vec![
                    Geometry::Point(Point::new(x, y)),
                    Geometry::LineString(LineString::from(vec![(x, y), (x + 0.3, y + 0.2)])),
                ])),
            };
            let props = vec![
                ("name".into(), PropertyValue::String(format!("f{}", i % 37))),
                ("rank".into(), PropertyValue::Int((i % 11) as i64)),
            ];
            Member {
                key: 0,
                seq: i as u64,
                rank: point_rank(i as u64),
                geom: geom.into(),
                props: SharedProps::new(props),
                id: None,
                gap: GAP_NONE,
                band: 0,
            }
        })
        .collect();

    assert!(encode_in_parallel(&members));
    assert!(!encode_in_parallel(&members[..40]));

    for tile_members in [&members[..], &members[..40]] {
        let mut serial = LayerBuilder::new(opts.layer_name.clone()).with_extent(opts.extent);
        for m in tile_members {
            // Through the public `String`-keyed entry point, so this
            // also pins that the export's interned `Arc<str>` keys
            // (#461) encode the same bytes.
            let props: Vec<(String, PropertyValue)> = m
                .props
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_owned()))
                .collect();
            serial.add_feature(
                m.feature_id(),
                &m.geom,
                &props,
                &tb,
                &crate::diagnostics::Diagnostics::default(),
            );
        }
        let (features, unencodable, quantized) = (
            serial.feature_count(),
            serial.dropped_features(),
            serial.quantized_features(),
        );
        assert!(
            unencodable > 0 && quantized > 0,
            "fixture must exercise both drops"
        );
        let mut tile = TileBuilder::new();
        tile.add_layer(serial.build());
        let serial_bytes = tile.build().encode_to_vec();

        let built = build_mvt(
            &test_gzip_cache(),
            tile_members,
            &tb,
            &opts,
            &[],
            &crate::diagnostics::Diagnostics::default(),
        );
        assert_eq!(built.data, serial_bytes);
        assert_eq!(built.features, features);
        assert_eq!(built.unencodable, unencodable);
        assert_eq!(built.quantized, quantized);
    }
}

/// A conforming foreign producer with a float32 covering: the scan must
/// read geometry because only float64 covering values are bit-exact.
fn rewrite_with_float32_covering(src: &Path, dst: &Path) {
    rewrite_overview(src, dst, None, true);
}

/// Rewrite overview file `src` to `dst` row group for row group (same
/// boundaries, schema and footer keys). `bad_row` blanks the WKB geometry
/// of that global row — an undecodable value the export's geometry decode
/// rejects with a typed error; `float32_covering` narrows the bbox covering to float32
/// ([`rewrite_with_float32_covering`]).
fn rewrite_overview(src: &Path, dst: &Path, bad_row: Option<usize>, float32_covering: bool) {
    use arrow_array::{Array, BinaryArray};
    use tylertoo_io::{test_support::ArrowWriter, KeyValue};

    let probe = tylertoo_io::open_parquet(File::open(src).unwrap()).unwrap();
    let in_schema = probe.schema().clone();
    let num_rgs = probe.metadata().num_row_groups();
    let kvs: Vec<KeyValue> = probe
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|kv| {
            kv.key != "ARROW:schema" && kv.key != crate::resolved_schema::RESOLVED_SCHEMA_KEY
        })
        .collect();
    let mut schema_metadata = in_schema.metadata().clone();
    // This fixture models a foreign producer, without a persisted descriptor.
    schema_metadata.remove(crate::resolved_schema::RESOLVED_SCHEMA_KEY);
    let fields: Vec<Field> = in_schema
        .fields()
        .iter()
        .map(|f| {
            if float32_covering && f.name() == "bbox" {
                let DataType::Struct(children) = f.data_type() else {
                    unreachable!()
                };
                f.as_ref().clone().with_data_type(DataType::Struct(
                    children
                        .iter()
                        .map(|c| Arc::new(c.as_ref().clone().with_data_type(DataType::Float32)))
                        .collect(),
                ))
            } else {
                f.as_ref().clone()
            }
        })
        .collect();
    let schema = Arc::new(Schema::new_with_metadata(fields, schema_metadata));
    let geom_idx = schema.index_of("geometry").unwrap();

    let mut writer =
        ArrowWriter::try_new(File::create(dst).unwrap(), schema.clone(), None).unwrap();
    let mut row0 = 0usize;
    for rg in 0..num_rgs {
        let reader = tylertoo_io::open_parquet(File::open(src).unwrap())
            .unwrap()
            .with_row_groups(vec![rg])
            .with_batch_rows(usize::MAX >> 1)
            .build()
            .unwrap();
        for batch in reader {
            let batch = batch.unwrap();
            let columns = batch
                .columns()
                .iter()
                .enumerate()
                .map(|(i, col)| arrow_cast::cast(col, schema.field(i).data_type()).unwrap())
                .collect();
            let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
            let n = batch.num_rows();
            let batch = if bad_row.is_some_and(|bad_row| (row0..row0 + n).contains(&bad_row)) {
                let bad_row = bad_row.unwrap();
                let geoms = batch
                    .column(geom_idx)
                    .as_any()
                    .downcast_ref::<BinaryArray>()
                    .expect("overview geometry is WKB Binary");
                let blanked: BinaryArray = (0..n)
                    .map(|i| {
                        Some(if row0 + i == bad_row {
                            &[] as &[u8]
                        } else {
                            geoms.value(i)
                        })
                    })
                    .collect();
                let mut cols = batch.columns().to_vec();
                cols[geom_idx] = Arc::new(blanked);
                RecordBatch::try_new(schema.clone(), cols).unwrap()
            } else {
                batch
            };
            row0 += n;
            writer.write(&batch).unwrap();
        }
        writer.flush().unwrap();
    }
    for kv in kvs {
        writer.append_key_value_metadata(kv);
    }
    writer.close().unwrap();
}

/// #362's shape at the #535 read/compute pipe in [`scan_all_levels`]: the
/// consumer (bbox decode) fails several batches in — past the pipe's
/// depth — while the producer still has batches left to read. Its error
/// must come back promptly, not wedge the producer in `send`.
///
/// The scan is the site an export reaches first (it decodes every
/// geometry before any wave runs). [`process_wave`]'s pipe is the same
/// shape — [`scoped_pipe`] owning the receiver, a producer that treats a
/// failed `send` as a clean stop — and the helper itself is covered by
/// `pipe::tests`.
#[test]
fn scan_consumer_error_mid_read_returns_promptly() {
    const BATCHES: usize = 10;
    let n = BATCHES * EXPORT_BATCH_SIZE;
    // Fourth batch: past depth + 2 in flight, with six batches unread.
    let bad_row = 3 * EXPORT_BATCH_SIZE + 17;
    let ids: Vec<i64> = (0..n as i64).collect();
    let geoms: Vec<Geometry<f64>> = (0..n)
        .map(|i| {
            Geometry::Point(Point::new(
                (i % 359) as f64 * 0.5 - 90.0,
                (i % 157) as f64 * 0.5 - 39.0,
            ))
        })
        .collect();
    let good = tempfile::NamedTempFile::new().unwrap();
    write_mode_fixture(good.path(), &[(ids, geoms)], Mode::Duplicating, 10_000);
    let bad = tempfile::NamedTempFile::new().unwrap();
    // #730: the scan reads the bbox covering, not the geometry, so a blank
    // geometry fails the scan's consumer only in a file without one.
    rewrite_overview(good.path(), bad.path(), Some(bad_row), true);
    let bad_path = bad.path().to_path_buf();

    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    std::thread::spawn(move || {
        let out = tempfile::NamedTempFile::new().unwrap();
        let _ = done_tx.send(export_pmtiles(&bad_path, out.path(), &Default::default()));
    });
    let result = done_rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("export deadlocked on a mid-read consumer error (#362 shape)");
    let err = result.expect_err("a blank WKB value must fail the export");
    // The decode's index is batch-local: 17 pins the failure to the bad
    // row inside the fourth batch, i.e. the consumer's error, surfaced.
    assert!(
        err.to_string().contains("Invalid geometry at index 17"),
        "expected the consumer's geometry-decode error, got: {err}"
    );
}

// --- the scan's bbox covering source (#730) ------------------------------

/// A `write_mode_fixture` whose level geometries may be null.
fn write_mode_fixture_opt(
    path: &Path,
    level_geoms: &[Vec<Option<Geometry<f64>>>],
    mode: Mode,
) -> OverviewsMeta {
    write_opt_batches(path, level_geoms, mode, 3)
}

/// [`write_mode_fixture_opt`] in row groups of up to `max_row_group_size`.
fn write_opt_batches(
    path: &Path,
    level_geoms: &[Vec<Option<Geometry<f64>>>],
    mode: Mode,
    max_row_group_size: usize,
) -> OverviewsMeta {
    let schema = Arc::new(source_schema());
    let mut next_id = 0i64;
    let batches = level_geoms.iter().map(|geoms| {
        let ids: Vec<i64> = (next_id..next_id + geoms.len() as i64).collect();
        next_id += geoms.len() as i64;
        let typ = GeometryType::new(Default::default());
        let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
        b.extend_from_iter(geoms.iter().map(Option::as_ref));
        let name = StringArray::from(ids.iter().map(|i| format!("f{i}")).collect::<Vec<_>>());
        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(ids.clone())),
                Arc::new(name),
                Arc::new(b.finish().to_array_ref()),
            ],
        )
        .unwrap()
    });
    write_mode_batches(path, mode, max_row_group_size, batches)
}

fn square(x: f64, y: f64, w: f64) -> geo::Polygon<f64> {
    geo::Polygon::new(
        LineString::from(vec![(x, y), (x + w, y), (x + w, y + w), (x, y + w), (x, y)]),
        vec![],
    )
}

/// Features of every shape the covering has to agree with the geometry
/// on, in degrees: points, lines, holed polygons, multipolygons, and
/// features on the antimeridian and the world edge (wrap copies).
fn covering_corpus(scale: f64) -> Vec<Vec<Option<Geometry<f64>>>> {
    let p = |x: f64, y: f64| Some(Geometry::Point(Point::new(x * scale, y * scale)));
    let holed = geo::Polygon::new(
        LineString::from(vec![(10.0, 10.0), (30.0, 10.0), (30.0, 30.0), (10.0, 30.0)])
            .map_coords(|c| c * scale),
        vec![
            LineString::from(vec![(15.0, 15.0), (20.0, 15.0), (20.0, 20.0)])
                .map_coords(|c| c * scale),
        ],
    );
    let multi = geo::MultiPolygon::new(vec![
        square(-60.0, -20.0, 5.0).map_coords(|c| c * scale),
        square(-40.0, 5.0, 3.0).map_coords(|c| c * scale),
    ]);
    vec![
        vec![
            p(-120.0, 40.0),
            p(179.999, -16.0),
            Some(Geometry::Polygon(holed)),
        ],
        vec![
            Some(Geometry::LineString(
                LineString::from(vec![(-70.0, -10.0), (-40.0, 5.0), (20.0, 8.0)])
                    .map_coords(|c| c * scale),
            )),
            p(-180.0, 0.0),
            Some(Geometry::MultiPolygon(multi)),
            p(0.0, 0.0),
        ],
        vec![
            p(-118.0, 34.0),
            p(-117.5, 33.8),
            Some(Geometry::Polygon(
                square(177.0, 60.0, 2.9).map_coords(|c| c * scale),
            )),
            p(2.0, 48.0),
            p(179.9, 84.0),
        ],
    ]
}

/// An `auto` band read of `source` with no memory limit.
pub(super) fn test_band(source: &ConvertSource) -> BandRead<'_> {
    BandRead {
        source,
        budget: MemoryBudget::with_available(None),
        profile: MemoryProfile::Auto,
    }
}

/// The export scan reads a band of several row groups with the ordered
/// parallel reader and sizes it at once under `auto`, and with one
/// reader, batch by batch, under `bounded`, to the same scans (#817: one reader thread capped the scan at about
/// one core).
#[test]
fn scan_reads_and_sizes_a_band_in_parallel() {
    let input = tempfile::NamedTempFile::new().unwrap();
    let points: Vec<Geometry<f64>> = (0..40_000)
        .map(|i| {
            let f = f64::from(i);
            Geometry::Point(Point::new(
                -170.0 + (f * 0.0171) % 340.0,
                -60.0 + (f * 0.0067) % 120.0,
            ))
        })
        .collect();
    write_fixture(input.path(), &[((0..40_000).collect(), points)]);
    let reader = OverviewReader::open(input.path()).unwrap();
    let source = reader.source();
    let scan = |profile| {
        let flushes = SCAN_SIZE_FLUSHES.with(std::cell::Cell::get);
        let scans = scan_all_levels(
            &reader,
            BandRead {
                profile,
                ..test_band(source)
            },
            Crs::Epsg4326,
            &ExportOptions::default(),
            &ExportTimers::default(),
            None,
            None,
            &crate::diagnostics::Diagnostics::default(),
        )
        .unwrap();
        let flushes = SCAN_SIZE_FLUSHES.with(std::cell::Cell::get) - flushes;
        (scans, BAND_READ_WORKERS.with(std::cell::Cell::get), flushes)
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(64)
        .build()
        .unwrap();
    let (auto, readers, flushes) = pool.install(|| scan(MemoryProfile::Auto));
    // The auto reader count follows the host's cores: one on one CPU.
    let several = resolve_pass_read_workers(ReadPass::Scan, READ_WORKERS_AUTO) > 1;
    assert_eq!(
        readers > 1,
        several,
        "auto scanned a 4-row-group band with {readers} reader(s)"
    );
    assert_eq!(
        flushes, 1,
        "64 threads size a 40,000-row band at once, not batch by batch"
    );
    let (bounded, readers, flushes) = pool.install(|| scan(MemoryProfile::Bounded));
    assert_eq!(readers, 1, "bounded keeps the one scan reader");
    assert_eq!(flushes, 5, "bounded sizes batch by batch");
    assert_eq!(auto, bounded);
}

/// #768: 6,000 small squares in each of two z2 tiles under a 20 kB raw
/// cap, and the export scan of them with the valve predictor, under
/// `budget`.
fn floored_scan(
    budget: &mut MemoryBudget,
) -> (
    Vec<LevelScan>,
    valve_predict::ValveFloors,
    Vec<crate::diagnostics::Diagnostic>,
) {
    floored_scan_of(budget, None)
}

/// [`floored_scan`] with, at row `hollow`, a member of the west tile
/// whose bbox reaches it and whose clip there is empty: an L around its
/// north-west corner.
fn floored_scan_of(
    budget: &mut MemoryBudget,
    hollow: Option<usize>,
) -> (
    Vec<LevelScan>,
    valve_predict::ValveFloors,
    Vec<crate::diagnostics::Diagnostic>,
) {
    let input = tempfile::NamedTempFile::new().unwrap();
    let mut squares = two_tile_squares();
    if let Some(at) = hollow {
        let l = LineString::from(vec![
            (-10.0, 1.0),
            (-5.0, 1.0),
            (-5.0, 75.0),
            (50.0, 75.0),
            (50.0, 78.0),
            (-10.0, 78.0),
            (-10.0, 1.0),
        ]);
        squares.insert(at, Geometry::Polygon(geo::Polygon::new(l, vec![])));
    }
    let n = squares.len() as i64;
    write_fixture(input.path(), &[((0..n).collect(), squares)]);
    floors_of_file(input.path(), budget)
}

/// The 12,000 squares of [`floored_scan`]: one grid in each of two z2
/// tiles, side by side, west first.
fn two_tile_squares() -> Vec<Geometry<f64>> {
    (0..12_000)
        .map(|i| {
            let east = if i < 6_000 { 0.0 } else { 90.0 };
            let i = i % 6_000;
            let (x, y) = (
                east + 1.0 + f64::from(i % 100) * 0.8,
                1.0 + f64::from(i / 100),
            );
            let d = 0.2 + f64::from((i * 7919) % 13) * 0.02;
            Geometry::Polygon(geo::Rect::new((x, y), (x + d, y + d)).to_polygon())
        })
        .collect()
}

/// [`floored_scan`]'s scan and floors of the overview at `path`.
fn floors_of_file(
    path: &Path,
    budget: &mut MemoryBudget,
) -> (
    Vec<LevelScan>,
    valve_predict::ValveFloors,
    Vec<crate::diagnostics::Diagnostic>,
) {
    let reader = OverviewReader::open(path).unwrap();
    let zooms = resolve_level_zooms(reader.meta(), reader.num_levels()).unwrap();
    let opts = ExportOptions {
        tile_size_limit: Some(20_000),
        compressed_tile_size_limit: None,
        ..Default::default()
    };
    let cols =
        resolve_export_columns((&reader, test_band(reader.source())), &opts, &zooms).unwrap();
    let diagnostics = crate::diagnostics::Diagnostics::default();
    let (scans, floors) = scan_with_floors(
        &reader,
        Crs::Epsg4326,
        (&cols.published, cols.feature_id.as_ref()),
        (&opts, &zooms),
        budget,
        (&ExportTimers::default(), None),
        &diagnostics,
    )
    .unwrap();
    (scans, floors, diagnostics.snapshot())
}

/// #768: the predictor's state is charged to the export's budget while
/// its floors are priced, and returned once they are built; the floors
/// themselves stay charged, since pass 2 reads them.
#[test]
fn scan_with_floors_returns_the_budget_it_held() {
    let mut budget = MemoryBudget::with_available(Some(64 << 30));
    let before = budget.available().unwrap();
    let (scans, floors, diagnostics) = floored_scan(&mut budget);
    assert!(!floors.is_empty(), "the cap must bind");
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
    assert!(floors.resident_bytes() > 0);
    assert_eq!(budget.available(), Some(before - floors.resident_bytes()));
    assert!(scans[0].tile_counts.values().sum::<usize>() >= 12_000);
}

/// #768: a tile is priced by the members its first sample's rows clip
/// to; a row whose bbox reaches the tile but whose clip is empty leaves
/// one fewer, and the tile is still priced.
#[test]
fn a_price_sample_with_an_empty_clip_still_prices_its_tile() {
    let west = (2, 2, 1);
    let floor = |hollow| {
        let mut budget = MemoryBudget::with_available(Some(64 << 30));
        floored_scan_of(&mut budget, hollow)
            .1
            .merge
            .tiles
            .get(&west)
            .copied()
    };
    let whole = floor(None).expect("a floor") as f64;
    // The west tile's 1,100th member: inside its first price sample.
    // Unpriced, the tile would keep only its east neighbour's halved
    // need as its floor.
    let hollow = floor(Some(1_099)).expect("a floor") as f64;
    assert!(hollow / whole > 0.75, "{hollow} against {whole}");
}

/// #768: an overview with no usable bbox covering is scanned from its
/// geometry, which drops null rows; the predictor still addresses each
/// tile's price rows by their place in the file, so the geometry scan
/// predicts the floors the covering scan does over nulls before the east
/// tile. Read 2,000 rows early, the east tile's price rows would be the
/// west tile's and the nulls: no member of it, so no need of its own.
#[test]
fn nulls_before_a_tile_leave_a_geometry_scans_floors() {
    let dir = tempfile::tempdir().unwrap();
    let mut geoms: Vec<Option<Geometry<f64>>> = two_tile_squares().into_iter().map(Some).collect();
    geoms.splice(6_000..6_000, std::iter::repeat_n(None, 2_000));
    let with = dir.path().join("with.parquet");
    let without = dir.path().join("without.parquet");
    write_opt_batches(&with, &[geoms], Mode::Duplicating, 10_000);
    rewrite_with_float32_covering(&with, &without);
    assert_eq!(
        ScanBboxSource::for_reader(&OverviewReader::open(&without).unwrap()),
        ScanBboxSource::Geometry
    );
    let floors = |path: &Path| {
        let mut budget = MemoryBudget::with_available(Some(64 << 30));
        floors_of_file(path, &mut budget).1
    };
    let want = floors(&with);
    assert_eq!(want.merge.own.len(), 2, "{want:?}");
    assert_eq!(floors(&without), want);
}

/// #768: the price samples are taken a chunk of tiles at a time, as
/// many as the export's share of the budget holds, to the same floors.
#[test]
fn price_samples_are_chunked_by_the_budget_to_the_same_floors() {
    let chunks = |available| {
        let before = SAMPLE_CHUNKS.with(std::cell::Cell::get);
        let (_, floors, diagnostics) =
            floored_scan(&mut MemoryBudget::with_available(Some(available)));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        (floors, SAMPLE_CHUNKS.with(std::cell::Cell::get) - before)
    };
    let (roomy, whole) = chunks(64 << 30);
    assert!(roomy.merge.tiles.len() >= 2, "{roomy:?}");
    // Two priced tiles, both candidates: one chunk a sample.
    assert_eq!(whole, 2);
    // 64 KiB holds one tile's first sample, not two.
    let (tight, chunked) = chunks(64 << 10);
    assert_eq!(chunked, 4);
    assert_eq!(tight, roomy);
}

/// #768: a predictor over the scan's budget drops its dense state and
/// says so; the export goes on as under `--valve-scope tile`, with the
/// same tile counts.
#[test]
fn a_predictor_over_the_scan_budget_falls_back_to_tile() {
    let mut roomy = MemoryBudget::with_available(Some(64 << 30));
    let (want, ..) = floored_scan(&mut roomy);
    let mut tight = MemoryBudget::with_available(Some(1 << 10));
    let (scans, floors, diagnostics) = floored_scan(&mut tight);
    assert!(floors.is_empty());
    assert_eq!(
        diagnostics,
        vec![crate::diagnostics::Diagnostic::ValveFloorsShed {
            count: 1,
            scope: "smooth".into(),
            predictor: true,
        }]
    );
    let counts = |s: &[LevelScan]| s.iter().map(|l| l.tile_counts.clone()).collect::<Vec<_>>();
    assert_eq!(counts(&scans), counts(&want));
}

/// #768: the tile counts are what `tile` holds too; only the dense state
/// the predictor adds is checked against the scan's budget, so a scan of
/// many sparse tiles beside one dense tile keeps its floors.
#[test]
fn many_sparse_tiles_beside_a_small_dense_state_do_not_shed() {
    let zooms = [14u8];
    let opts = ExportOptions::default();
    let mut p = valve_predict::Scan::new(1);
    p.batch(0, &valve_predict::dense_points(2_000), 0..=0, &zooms, &opts);
    // 200,000 points, one per z14 tile.
    let sparse: Vec<_> = (0..200_000u32)
        .map(|i| {
            let (x, y) = (
                -170.0 + f64::from(i % 1000) * 0.03,
                -60.0 + f64::from(i / 1000) * 0.03,
            );
            Some(TileBounds::new(x, y, x, y))
        })
        .collect();
    p.batch(1, &sparse, 0..=0, &zooms, &opts);
    p.flush(&zooms, &opts);
    // Room for the dense tile's state, not for 200,000 counts.
    let budget = MemoryBudget::with_available(Some(4 << 20));
    let diagnostics = crate::diagnostics::Diagnostics::default();
    shed_over_budget(
        &mut p,
        (&budget, MemoryProfile::Bounded),
        &opts,
        &diagnostics,
    );
    assert!(!p.is_shed(), "{} bytes", p.resident_bytes());
    assert!(diagnostics.snapshot().is_empty());
}

/// Both sources' scans of one file, for every level: (covering, geometry).
fn scans_by_source(path: &Path, crs: Crs) -> (Option<Vec<LevelScan>>, Vec<LevelScan>) {
    let reader = OverviewReader::open(path).unwrap();
    let source = ScanBboxSource::for_reader(&reader);
    assert!(
        matches!(source, ScanBboxSource::Covering { .. }),
        "the overview writer's f64 covering must be the scan's source: {source:?}"
    );
    let timers = ExportTimers::default();
    let opts = ExportOptions::default();
    let covering = scan_all_levels_from(
        (&reader, test_band(reader.source())),
        crs,
        &opts,
        &timers,
        &source,
        None,
        None,
        &crate::diagnostics::Diagnostics::default(),
    )
    .unwrap();
    let geometry = scan_all_levels_from(
        (&reader, test_band(reader.source())),
        crs,
        &opts,
        &timers,
        &ScanBboxSource::Geometry,
        None,
        None,
        &crate::diagnostics::Diagnostics::default(),
    )
    .unwrap()
    .unwrap();
    // #768: with the predictor the scan takes its tile counts from it.
    let mut predict = valve_predict::Scan::new(reader.num_levels());
    let predicted = scan_all_levels_from(
        (&reader, test_band(reader.source())),
        crs,
        &opts,
        &timers,
        &source,
        None,
        Some(&mut predict),
        &crate::diagnostics::Diagnostics::default(),
    )
    .unwrap()
    .map(|mut scans| {
        for (scan, counts) in scans.iter_mut().zip(predict.into_tile_counts()) {
            scan.tile_counts = counts;
        }
        scans
    });
    assert_eq!(predicted, covering, "the predictor's tile counts differ");
    let s = timers.stage_secs();
    assert!(
        s.scan_read > 0.0 && s.scan_decode > 0.0 && s.scan_size > 0.0,
        "the scan must charge its scan_* stages: {} {} {}",
        s.scan_read,
        s.scan_decode,
        s.scan_size
    );
    (covering, geometry)
}

/// #730: the overview writer's covering is the scan's source, and the
/// scan it gives is the geometry scan exactly — in both modes, in both
/// CRSs (a 3857 file's covering is in metres and reprojects like the
/// geometry's bbox), across wrap copies, holes and null geometries.
#[test]
fn covering_scan_matches_geometry_scan() {
    const M_PER_DEG: f64 = 111_000.0;
    for (crs, scale) in [(Crs::Epsg4326, 1.0), (Crs::Epsg3857, M_PER_DEG)] {
        for mode in [Mode::Partitioning, Mode::Duplicating] {
            let mut levels = covering_corpus(scale);
            // A null geometry: no covering row, no feature on either path.
            levels[1].push(None);
            let tmp = tempfile::NamedTempFile::new().unwrap();
            write_mode_fixture_opt(tmp.path(), &levels, mode);
            let (covering, geometry) = scans_by_source(tmp.path(), crs);
            assert_eq!(
                covering.as_ref(),
                Some(&geometry),
                "{crs:?} {mode:?}: covering scan differs from the geometry scan"
            );
            assert!(
                geometry.iter().all(|s| !s.tile_counts.is_empty()),
                "vacuous: every level must size tiles"
            );
            let own = |k: usize| levels[k].iter().flatten().count();
            let last = levels.len() - 1;
            let expected = match mode {
                Mode::Partitioning => (0..=last).map(own).sum::<usize>(),
                Mode::Duplicating => own(last),
            };
            assert_eq!(
                geometry[last].feature_count, expected,
                "null rows are not features"
            );
        }
    }
}

/// #730: an empty geometry has a `[+inf, +inf, -inf, -inf]` covering row
/// and no bbox on the geometry path (and an empty point is not even
/// counted there), so the covering pass is abandoned and the scan comes
/// from the geometry — the same scan as before the covering source.
#[test]
fn empty_geometry_covering_row_falls_back_to_the_geometry() {
    let mut levels = covering_corpus(1.0);
    levels[0].push(Some(Geometry::MultiPolygon(geo::MultiPolygon::new(vec![]))));
    let tmp = tempfile::NamedTempFile::new().unwrap();
    write_mode_fixture_opt(tmp.path(), &levels, Mode::Partitioning);
    let (covering, geometry) = scans_by_source(tmp.path(), Crs::Epsg4326);
    assert_eq!(
        covering, None,
        "an empty geometry's covering row must abandon the pass"
    );
    let reader = OverviewReader::open(tmp.path()).unwrap();
    let scans = scan_all_levels(
        &reader,
        test_band(reader.source()),
        Crs::Epsg4326,
        &ExportOptions::default(),
        &ExportTimers::default(),
        None,
        None,
        &crate::diagnostics::Diagnostics::default(),
    )
    .unwrap();
    assert_eq!(scans, geometry);
}

/// #730's fallback: a float32 covering uses the geometry scan and exports
/// to the same archive bytes as a bit-exact float64 covering.
#[test]
fn export_float32_covering_matches_export_with_f64_covering() {
    for mode in [Mode::Partitioning, Mode::Duplicating] {
        let dir = tempfile::tempdir().unwrap();
        let with = dir.path().join("with.parquet");
        let without = dir.path().join("without.parquet");
        write_mode_fixture_opt(&with, &covering_corpus(1.0), mode);
        rewrite_with_float32_covering(&with, &without);
        assert_eq!(
            ScanBboxSource::for_reader(&OverviewReader::open(&without).unwrap()),
            ScanBboxSource::Geometry,
            "the float32 covering must use the geometry scan"
        );
        let archive = |input: &Path, name: &str| {
            let out = dir.path().join(name);
            export_pmtiles(input, &out, &ExportOptions::default()).unwrap();
            std::fs::read(&out).unwrap()
        };
        assert_eq!(
            archive(&with, "with.pmtiles"),
            archive(&without, "without.pmtiles"),
            "{mode:?}: the archive depends on covering precision"
        );
    }
}

/// #730: `covering_batch_bboxes` takes a covering row only when it is a
/// finite, ordered box. Each rejected row breaks exactly one of the six
/// conditions, so no single one can be dropped from the check.
#[test]
fn covering_row_must_be_finite_and_ordered() {
    use arrow_array::{Float64Array, StructArray};
    use arrow_schema::DataType;
    let names = ["xmin", "ymin", "xmax", "ymax"].map(String::from);
    let batch_of = |row: [f64; 4]| {
        let children: Vec<(Arc<Field>, ArrayRef)> = names
            .iter()
            .zip(row)
            .map(|(n, v)| {
                let field = Arc::new(Field::new(n, DataType::Float64, true));
                (field, Arc::new(Float64Array::from(vec![v])) as ArrayRef)
            })
            .collect();
        let strukt: ArrayRef = Arc::new(StructArray::from(children));
        let schema = Schema::new(vec![Field::new("bbox", strukt.data_type().clone(), true)]);
        RecordBatch::try_new(Arc::new(schema), vec![strukt]).unwrap()
    };
    let read = |row: [f64; 4]| {
        covering_batch_bboxes(&batch_of(row), "bbox", &names, Crs::Epsg4326).unwrap()
    };
    assert_eq!(
        read([1.0, 2.0, 3.0, 4.0]),
        Some(vec![Some(TileBounds::new(1.0, 2.0, 3.0, 4.0))])
    );
    let inf = f64::INFINITY;
    for row in [
        [3.0, 2.0, 1.0, 4.0], // x unordered
        [1.0, 4.0, 3.0, 2.0], // y unordered
        [-inf, 2.0, 3.0, 4.0],
        [1.0, 2.0, inf, 4.0],
        [1.0, -inf, 3.0, 4.0],
        [1.0, 2.0, 3.0, inf],
        [f64::NAN, 2.0, 3.0, 4.0],
    ] {
        assert_eq!(read(row), None, "{row:?} must abandon the covering pass");
    }
}

/// #730's documented divergence: an invalid polygon whose hole lies
/// outside its shell has a covering that includes the hole, while the
/// geometry path's `bounding_rect()` reads the shell alone. The covering
/// scan is then a superset — it counts tiles the clip never emits to, and
/// its bounds are wider — but every tile the archive holds is unchanged.
#[test]
fn escaping_hole_covering_scan_is_a_superset() {
    use crate::archive_index::ArchiveIndex;
    let shell = square(0.0, 0.0, 10.0);
    let invalid = geo::Polygon::new(
        shell.exterior().clone(),
        vec![square(40.0, 40.0, 5.0).exterior().clone()],
    );
    let levels = vec![
        vec![Some(Geometry::Point(Point::new(-50.0, -20.0)))],
        vec![Some(Geometry::Polygon(invalid))],
    ];
    let dir = tempfile::tempdir().unwrap();
    let with = dir.path().join("with.parquet");
    write_mode_fixture_opt(&with, &levels, Mode::Partitioning);
    let (covering, geometry) = scans_by_source(&with, Crs::Epsg4326);
    let covering = covering.expect("an escaping hole is finite: no fallback");
    let finest = levels.len() - 1;
    let (c, g) = (&covering[finest], &geometry[finest]);
    assert_eq!(c.feature_count, g.feature_count);
    assert!(
        g.tile_counts
            .iter()
            .all(|(k, n)| c.tile_counts.get(k) >= Some(n)),
        "the covering scan must plan every tile the geometry scan does"
    );
    assert!(
        c.tile_counts.len() > g.tile_counts.len(),
        "vacuous: the hole must reach tiles the shell does not"
    );
    assert!(c.bounds.unwrap().lat_max > g.bounds.unwrap().lat_max);

    let without = dir.path().join("without.parquet");
    rewrite_with_float32_covering(&with, &without);
    let tiles = |input: &Path, name: &str| {
        let out = dir.path().join(name);
        export_pmtiles(input, &out, &ExportOptions::default()).unwrap();
        let idx = ArchiveIndex::open(&out).unwrap();
        idx.tiles()
            .map(|t| {
                let t = t.unwrap();
                ((t.z, t.x, t.y), idx.read_range(t.range).unwrap())
            })
            .collect::<BTreeMap<_, _>>()
    };
    assert_eq!(
        tiles(&with, "with.pmtiles"),
        tiles(&without, "without.pmtiles"),
        "the covering's extra planned tiles must not change any tile"
    );
}

/// Collect a single feature's `(key, geom)` members into a key→geom map,
/// asserting the "one member per feature per tile" invariant both paths
/// hold.
pub(super) fn members_by_key(v: Vec<(u64, Geometry<f64>)>) -> HashMap<u64, Geometry<f64>> {
    let mut m = HashMap::new();
    for (k, g) in v {
        assert!(m.insert(k, g).is_none(), "duplicate member for one feature");
    }
    m
}

/// Encode one tile member to MVT bytes on its own tile bounds — the exact
/// quantized output the archive would carry, so byte-equality here means
/// tile-output equivalence.
pub(super) fn tile_mvt(key: u64, geom: &Geometry<f64>, zoom: u8, opts: &ExportOptions) -> Vec<u8> {
    let (x, y) = key_to_xy(key, zoom);
    let tb = TileCoord::new(x, y, zoom).bounds();
    let m = Member {
        key,
        seq: 0,
        rank: point_rank(0),
        geom: geom.clone().into(),
        props: SharedProps::new(Vec::new()),
        id: None,
        gap: GAP_NONE,
        band: 0,
    };
    build_mvt(
        &test_gzip_cache(),
        std::slice::from_ref(&m),
        &tb,
        opts,
        &[],
        &crate::diagnostics::Diagnostics::default(),
    )
    .data
    .into_raw()
}

/// A fill feature of one `side`-unit square at (x, y), hosted by `id`.
pub(super) fn fill_square(id: u64, x: i32, y: i32, side: i32) -> FillFeature {
    FillFeature::encode_fill(
        Some(id),
        id,
        SharedProps::new(Vec::new()),
        &[&vec![vec![
            (x, y),
            (x + side, y),
            (x + side, y + side),
            (x, y + side),
        ]]],
        &crate::diagnostics::Diagnostics::default(),
    )
}

/// Feature corpus + zoom caps chosen so the oracle's `tiles×vertices` cost
/// stays bounded: interior/seam-crossing features run deep, wide features
/// only at coarse zooms.
pub(super) fn equivalence_corpus() -> Vec<(&'static str, Geometry<f64>, Vec<u8>)> {
    // A small polygon that straddles a z14 seam near NYC.
    let seam_poly = Geometry::Polygon(geo::Polygon::new(
        LineString::from(vec![
            (-73.985, 40.700),
            (-73.955, 40.700),
            (-73.955, 40.730),
            (-73.985, 40.730),
            (-73.985, 40.700),
        ]),
        vec![],
    ));
    let concave = Geometry::Polygon(geo::Polygon::new(
        LineString::from(vec![
            (-100.0, 25.0),
            (-98.0, 25.0),
            (-98.0, 27.0),
            (-99.0, 25.5),
            (-100.0, 27.0),
            (-100.0, 25.0),
        ]),
        vec![],
    ));
    let big_box = Geometry::Polygon(geo::Polygon::new(
        LineString::from(vec![
            (-20.0, -20.0),
            (20.0, -20.0),
            (20.0, 20.0),
            (-20.0, 20.0),
            (-20.0, -20.0),
        ]),
        vec![],
    ));
    let mut wig = Vec::new();
    for k in 0..200 {
        wig.push((30.0 + k as f64 * 0.3, -20.0 + (k as f64 * 0.1).sin()));
    }
    let wiggly = Geometry::LineString(LineString::from(wig));
    let antimeridian = Geometry::LineString(LineString::from(vec![
        (170.0, 5.0),
        (178.0, 8.0),
        (-176.0, 6.0),
        (-170.0, 9.0),
    ]));
    vec![
        (
            "interior_point",
            Geometry::Point(Point::new(-73.97, 40.71)),
            vec![4, 8, 14],
        ),
        ("seam_poly", seam_poly, vec![10, 12, 14]),
        ("concave", concave, vec![4, 6, 8]),
        ("big_box", big_box, vec![3, 5, 6]),
        ("wiggly", wiggly, vec![4, 6]),
        ("antimeridian", antimeridian, vec![3, 5]),
    ]
}

// ---- world-edge wrap (#342) ---------------------------------------------

/// A lon/lat rectangle as a polygon.
fn rect_polygon(lng_min: f64, lat_min: f64, lng_max: f64, lat_max: f64) -> Geometry<f64> {
    Geometry::Polygon(geo::Polygon::new(
        LineString::from(vec![
            (lng_min, lat_min),
            (lng_max, lat_min),
            (lng_max, lat_max),
            (lng_min, lat_max),
            (lng_min, lat_min),
        ]),
        vec![],
    ))
}

/// The lon/lat bbox of a member geometry.
fn member_bbox(g: &Geometry<f64>) -> TileBounds {
    let r = g.bounding_rect().expect("member has a bbox");
    TileBounds::new(r.min().x, r.min().y, r.max().x, r.max().y)
}

/// #342 reproducer: a cell stored as `x: 178 … 181` overhangs +180. The
/// 180°–181° part belongs on the west edge of the map, at -180°…-179°.
/// tippecanoe writes it to z2 tile (0, 1); tylertoo used to clip it away at
/// the world edge and write no west-edge tile at all.
#[test]
fn overhanging_polygon_wraps_onto_the_west_edge_tile() {
    let opts = ExportOptions::default();
    let zoom = 2;
    let members = members_by_key(feature_tile_members(
        &rect_polygon(178.0, 1.0, 181.0, 3.0),
        zoom,
        &opts,
        0,
        u64::MAX,
        &crate::diagnostics::Diagnostics::default(),
    ));
    let east = members
        .get(&tile_key(3, 1, zoom))
        .expect("the in-range part stays in its own tile (3, 1)");
    let west = members.get(&tile_key(0, 1, zoom)).unwrap_or_else(|| {
        panic!(
            "the overhanging part must wrap onto tile (0, 1); emitted tiles: {:?}",
            members
                .keys()
                .map(|&k| key_to_xy(k, zoom))
                .collect::<Vec<_>>()
        )
    });

    // The west piece is the 180°–181° sliver moved to -180°…-179°.
    let w = member_bbox(west);
    assert!(
        (w.lng_max - -179.0).abs() < 1e-9,
        "wrapped piece must end at -179°: {w:?}"
    );
    assert!(
        w.lng_min < -179.0,
        "wrapped piece reaches the map edge: {w:?}"
    );
    // The east piece keeps its in-range extent.
    let e = member_bbox(east);
    assert!(
        (e.lng_min - 178.0).abs() < 1e-9,
        "east piece starts at 178°: {e:?}"
    );
    // The buffer also reaches the row below; every member sits in one of
    // the two edge columns.
    let columns: HashSet<u32> = members.keys().map(|&k| key_to_xy(k, zoom).0).collect();
    assert_eq!(
        columns,
        HashSet::from([0, 3]),
        "only the two edge columns at z2"
    );
}

/// A feature stored wholly past +180° (the 0..360 convention) is drawn at
/// its wrapped longitude, as tippecanoe draws it, instead of vanishing.
#[test]
fn out_of_domain_point_is_drawn_at_its_wrapped_longitude() {
    let opts = ExportOptions::default();
    for zoom in [0u8, 4, 10] {
        let members = members_by_key(feature_tile_members(
            &Geometry::Point(geo::Point::new(200.0, 10.0)),
            zoom,
            &opts,
            0,
            u64::MAX,
            &crate::diagnostics::Diagnostics::default(),
        ));
        let home = crate::tile::lng_lat_to_tile(-160.0, 10.0, zoom);
        let Some(Geometry::Point(p)) = members.get(&tile_key(home.x, home.y, zoom)) else {
            panic!(
                "z{zoom}: a point at lng 200 must land in the tile holding lng -160; \
                 emitted {members:?}"
            );
        };
        assert!((p.x() - -160.0).abs() < 1e-9, "z{zoom}: drawn at {p:?}");
        assert_eq!(members.len(), 1, "z{zoom}: one tile only: {members:?}");
    }
}

/// Every tile that holds a feature whole shares the decoded geometry
/// instead of holding its own copy (manby holds each feature once).
#[test]
fn tiles_holding_a_feature_whole_share_its_geometry() {
    let opts = ExportOptions::default();
    let zoom = 4;
    let square = |x: f64, y: f64| {
        Geometry::Polygon(
            geo::Rect::new(
                geo::coord! { x: x, y: y },
                geo::coord! { x: x + 1e-6, y: y + 1e-6 },
            )
            .to_polygon(),
        )
    };
    // On the x=7 | x=8 border: inside both tiles' buffers.
    let source = Arc::new(square(1e-7, 10.0));
    let members = shared_tile_members(
        MemberSource::Shared(&source),
        None,
        zoom,
        &opts,
        (0, u64::MAX),
        &crate::diagnostics::Diagnostics::default(),
    );
    assert_eq!(members.len(), 2);
    let shared = |members: &[(u64, MemberGeom)], key: u64, source: &Arc<Geometry<f64>>| {
        members
            .iter()
            .any(|(k, g)| *k == key && matches!(g, MemberGeom::Shared(g) if Arc::ptr_eq(g, source)))
    };
    for (key, _) in &members {
        assert!(
            shared(&members, *key, &source),
            "tile {key} holds its own copy"
        );
    }

    // Without a geometry other levels share, two whole tiles share one
    // copy, and a single whole tile holds its own.
    let members = shared_tile_members(
        MemberSource::Owned((*source).clone()),
        None,
        zoom,
        &opts,
        (0, u64::MAX),
        &crate::diagnostics::Diagnostics::default(),
    );
    let [(_, MemberGeom::Shared(a)), (_, MemberGeom::Shared(b))] = &members[..] else {
        panic!("two whole tiles hold their own copies");
    };
    assert!(Arc::ptr_eq(a, b));
    let inside = square(11.0, 10.0);
    let coords = |g: &Geometry<f64>| match g {
        Geometry::Polygon(p) => p.exterior().0.as_ptr(),
        _ => unreachable!(),
    };
    let decoded = coords(&inside);
    let members = shared_tile_members(
        MemberSource::Owned(inside),
        None,
        zoom,
        &opts,
        (0, u64::MAX),
        &crate::diagnostics::Diagnostics::default(),
    );
    let [(_, MemberGeom::Owned(g))] = &members[..] else {
        panic!("one whole tile holds its own geometry");
    };
    assert_eq!(coords(g), decoded, "the tile copied the decoded geometry");

    // A feature past the direct clip budget takes the cascade: the one
    // tile that holds it whole shares it too.
    let line: geo::LineString<f64> = (0..=DIRECT_CLIP_BUDGET)
        .map(|i| geo::coord! { x: 11.0 + i as f64 * 1e-6, y: 10.0 })
        .collect();
    let source = Arc::new(Geometry::LineString(line));
    let members = shared_tile_members(
        MemberSource::Shared(&source),
        None,
        zoom,
        &opts,
        (0, u64::MAX),
        &crate::diagnostics::Diagnostics::default(),
    );
    assert_eq!(members.len(), 1);
    assert!(
        shared(&members, members[0].0, &source),
        "the cascade copied it"
    );

    // On the world edge, the stored copy's tile shares it; the shifted
    // copy in the far tile's buffer is its own.
    let lng = 180.0 - 0.5 * buffer_deg_at_zoom(zoom, &opts);
    let source = Arc::new(square(lng, 10.0));
    let members = shared_tile_members(
        MemberSource::Shared(&source),
        None,
        zoom,
        &opts,
        (0, u64::MAX),
        &crate::diagnostics::Diagnostics::default(),
    );
    let y = crate::tile::lng_lat_to_tile(lng, 10.0, zoom).y;
    assert!(shared(&members, tile_key(15, y, zoom), &source));
    assert!(!shared(&members, tile_key(0, y, zoom), &source));

    // A point has no coordinates to share: each tile holds it inline.
    let source = Arc::new(Geometry::Point(geo::Point::new(1e-6, 10.0)));
    let members = shared_tile_members(
        MemberSource::Shared(&source),
        None,
        zoom,
        &opts,
        (0, u64::MAX),
        &crate::diagnostics::Diagnostics::default(),
    );
    assert_eq!(members.len(), 2);
    assert!(members
        .iter()
        .all(|(_, g)| matches!(g, MemberGeom::Owned(_))));
}

/// Tile buffers continue across the antimeridian. A point just west of
/// +180° sits inside the buffer of the westmost tile column, so that tile
/// carries it too. tippecanoe does the same: its z0 clip copies every
/// feature within the buffer of the world edge 360° to the other side.
#[test]
fn tile_buffer_continues_across_the_antimeridian() {
    let opts = ExportOptions::default();
    let zoom = 4;
    let buffer = buffer_deg_at_zoom(zoom, &opts);
    let lng = 180.0 - 0.5 * buffer;
    let members = members_by_key(feature_tile_members(
        &Geometry::Point(geo::Point::new(lng, 10.0)),
        zoom,
        &opts,
        0,
        u64::MAX,
        &crate::diagnostics::Diagnostics::default(),
    ));
    let home = crate::tile::lng_lat_to_tile(lng, 10.0, zoom);
    assert_eq!(home.x, 15);
    assert!(members.contains_key(&tile_key(15, home.y, zoom)));
    let Some(Geometry::Point(p)) = members.get(&tile_key(0, home.y, zoom)) else {
        panic!("the point is in tile x=0's buffer; emitted {members:?}");
    };
    assert!(
        (p.x() - (lng - 360.0)).abs() < 1e-9,
        "the far-side copy is shifted by 360°: {p:?}"
    );

    // Beyond the buffer: no far-side copy.
    let lng = 180.0 - 1.5 * buffer;
    let members = members_by_key(feature_tile_members(
        &Geometry::Point(geo::Point::new(lng, 10.0)),
        zoom,
        &opts,
        0,
        u64::MAX,
        &crate::diagnostics::Diagnostics::default(),
    ));
    assert_eq!(
        members.len(),
        1,
        "{lng}° is beyond x=0's buffer: {members:?}"
    );
}

/// When both copies of a feature reach the same tile (z0 holds the whole
/// world), the tile still gets ONE member: the pieces are joined into one
/// multi-geometry, as tippecanoe's concatenated copies are.
#[test]
fn both_wrapped_copies_in_one_tile_make_one_member() {
    let opts = ExportOptions::default();
    let out = feature_tile_members(
        &rect_polygon(178.0, 1.0, 181.0, 3.0),
        0,
        &opts,
        0,
        u64::MAX,
        &crate::diagnostics::Diagnostics::default(),
    );
    assert_eq!(out.len(), 1, "one member per tile: {out:?}");
    let Geometry::MultiPolygon(mp) = &out[0].1 else {
        panic!("the two pieces join into a MultiPolygon: {:?}", out[0].1);
    };
    assert_eq!(mp.0.len(), 2, "east piece plus wrapped west piece: {mp:?}");
    let b = member_bbox(&out[0].1);
    assert!(b.lng_min < -179.0 && b.lng_max > 178.0, "{b:?}");

    // Lines and points join within their own family.
    let line = Geometry::LineString(LineString::from(vec![(178.0, 1.0), (181.0, 2.0)]));
    let out = feature_tile_members(
        &line,
        0,
        &opts,
        0,
        u64::MAX,
        &crate::diagnostics::Diagnostics::default(),
    );
    assert_eq!(out.len(), 1);
    assert!(
        matches!(&out[0].1, Geometry::MultiLineString(m) if m.0.len() == 2),
        "{:?}",
        out[0].1
    );
    let pt = Geometry::Point(geo::Point::new(179.99, 0.0));
    let out = feature_tile_members(
        &pt,
        0,
        &opts,
        0,
        u64::MAX,
        &crate::diagnostics::Diagnostics::default(),
    );
    assert_eq!(out.len(), 1);
    assert!(
        matches!(&out[0].1, Geometry::MultiPoint(m) if m.0.len() == 2),
        "{:?}",
        out[0].1
    );
}

/// The scan plans a wrapped feature's tiles from the same copies the emit
/// clips, and counts a tile two copies share only once.
#[test]
fn scan_plans_wrapped_copies_and_counts_shared_tiles_once() {
    use std::collections::HashSet;
    let opts = ExportOptions::default();
    let geoms = [
        rect_polygon(178.0, -1.0, 181.0, 1.0),
        rect_polygon(-182.0, 40.0, -175.0, 45.0),
        Geometry::Point(geo::Point::new(200.0, 10.0)),
        Geometry::Point(geo::Point::new(179.99, -30.0)),
        // Whole-world polygon: both copies touch the buffered world.
        rect_polygon(-180.0, -60.0, 180.0, -50.0),
    ];
    for zoom in [0u8, 2, 5, 9] {
        for g in &geoms {
            let bbox = member_bbox(g);
            let (_, counts) = size_bboxes(&[Some(bbox)], zoom, &opts, true);
            assert!(
                counts.values().all(|&c| c == 1),
                "z{zoom}: a tile counted twice for one feature {g:?}"
            );
            let planned: HashSet<u64> = counts.keys().copied().collect();
            for (key, _) in feature_tile_members(
                g,
                zoom,
                &opts,
                0,
                u64::MAX,
                &crate::diagnostics::Diagnostics::default(),
            ) {
                assert!(
                    planned.contains(&key),
                    "z{zoom}: emitted key {:?} was not planned for {g:?}",
                    key_to_xy(key, zoom)
                );
            }
        }
    }
}

/// The archive's advertised bounds are the wrapped data extent, inside
/// ±180°: a header saying lng 181 is not a valid PMTiles bbox.
#[test]
fn scan_bounds_wrap_into_the_lng_domain() {
    let opts = ExportOptions::default();
    let bounds = |b: TileBounds| size_bboxes(&[Some(b)], 4, &opts, true).0.expect("bounds");

    let b = bounds(TileBounds::new(178.0, -1.0, 181.0, 1.0));
    assert_eq!((b.lng_min, b.lng_max), (-180.0, 180.0), "{b:?}");
    let b = bounds(TileBounds::new(200.0, 10.0, 210.0, 12.0));
    assert_eq!((b.lng_min, b.lng_max), (-160.0, -150.0), "{b:?}");
    // In-range bboxes are untouched, edge contact included.
    for raw in [
        TileBounds::new(10.0, -1.0, 12.0, 1.0),
        TileBounds::new(170.0, -1.0, 180.0, 1.0),
        TileBounds::new(-180.0, -60.0, 180.0, -50.0),
    ] {
        assert_eq!(bounds(raw), raw);
    }
}

/// The duplicating-mode wave read prunes row groups by the wave's tile
/// bbox, which lies inside the ±180° domain. A wrapped copy is drawn in a
/// tile whose bbox is one world width away from the row group's stored
/// stats, so the prune must test the shifted wave bbox too, or the
/// wrapped piece is lost whenever its tile reads in a wave of its own.
#[test]
fn wave_row_group_prune_keeps_wrapped_copies() {
    use crate::pmtiles_writer::{decode_directory, tile_id_to_zxy, Header};
    let tin = tempfile::NamedTempFile::new().unwrap();
    // One feature per row group: the dateline cell, a 0..360 point and
    // an ordinary point that keeps the row-group stats apart.
    write_mode_fixture(
        tin.path(),
        &[(
            vec![0, 1, 2],
            vec![
                rect_polygon(178.0, 1.0, 181.0, 3.0),
                Geometry::Point(geo::Point::new(250.0, -40.0)),
                Geometry::Point(geo::Point::new(10.0, 10.0)),
            ],
        )],
        Mode::Duplicating,
        1,
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        partition_wave: 1,
        ..Default::default()
    };
    export_pmtiles_with_partition_target(tin.path(), tout.path(), &opts, 1).unwrap();

    let bytes = std::fs::read(tout.path()).unwrap();
    let header = Header::from_bytes(&bytes).unwrap();
    let root = crate::compression::decompress_capped(
        &bytes[header.root_dir_offset as usize
            ..(header.root_dir_offset + header.root_dir_length) as usize],
        header.internal_compression,
        crate::compression::MAX_INTERNAL_BYTES,
    )
    .unwrap();
    let tiles: HashSet<(u8, u32, u32)> = decode_directory(&root)
        .unwrap()
        .iter()
        .flat_map(|e| {
            (0..u64::from(e.run_length)).map(move |i| tile_id_to_zxy(e.tile_id + i).unwrap())
        })
        .collect();
    // The cell's 180°–181° part lands at -180°…-179°: z2 tile (0, 1).
    assert!(tiles.contains(&(2, 0, 1)), "dateline overhang: {tiles:?}");
    // The point at lng 250 lands at lng -110, in a row of its own.
    let home = crate::tile::lng_lat_to_tile(-110.0, -40.0, 2);
    assert_eq!((home.x, home.y), (0, 2));
    assert!(
        tiles.contains(&(2, home.x, home.y)),
        "0..360 point: {tiles:?}"
    );
}

/// A feature stored wholly past the buffered world (the 0..360
/// convention) is tiled through its wrapped copy alone. Its stored copy
/// reaches no tile, so the scan must not plan one for it: `expand_bbox`
/// would collapse that copy onto the east-edge column, where the clip
/// then drops it, and every such feature would inflate that column's
/// planned counts.
#[test]
fn stored_copy_outside_the_buffered_world_plans_no_tile() {
    let opts = ExportOptions::default();
    for zoom in [0u8, 4, 10] {
        let bbox = TileBounds::new(200.0, 10.0, 200.0, 10.0);
        let (_, counts) = size_bboxes(&[Some(bbox)], zoom, &opts, true);
        let home = crate::tile::lng_lat_to_tile(-160.0, 10.0, zoom);
        let planned: Vec<_> = counts.keys().map(|&k| key_to_xy(k, zoom)).collect();
        assert_eq!(
            planned,
            vec![(home.x, home.y)],
            "z{zoom}: only the wrapped copy's tile"
        );
    }
    // Beyond one wrap nothing reaches the grid.
    let far = TileBounds::new(600.0, 10.0, 600.0, 10.0);
    assert!(size_bboxes(&[Some(far)], 4, &opts, true).1.is_empty());
    assert!(feature_tile_members(
        &Geometry::Point(geo::Point::new(600.0, 10.0)),
        4,
        &opts,
        0,
        u64::MAX,
        &crate::diagnostics::Diagnostics::default()
    )
    .is_empty());
}

/// The scan pass plans the partitions and the emit pass fills them; the
/// export requires every emitted key to land in a planned partition, and
/// `route_partition` only `debug_assert!`s it, so in release a mismatch
/// silently writes tiles out of key order. Both sides go through
/// `member_ranges`; this pins that they agree.
#[test]
fn scan_counts_cover_every_emitted_member_key() {
    use std::collections::HashSet;
    for tile_buffer in [0, 8, 64] {
        let opts = ExportOptions {
            tile_buffer,
            ..Default::default()
        };
        for zoom in [1u8, 4, 9] {
            let geoms = [
                Geometry::Point(geo::Point::new(0.05, 10.0)),
                Geometry::Point(geo::Point::new(-179.9, -80.0)),
                Geometry::Point(geo::Point::new(179.9, 84.0)),
                Geometry::LineString(geo::LineString::from(vec![(-20.0, -10.0), (25.0, 30.0)])),
            ];
            let bboxes: Vec<Option<TileBounds>> = geoms
                .iter()
                .map(|g| {
                    g.bounding_rect()
                        .map(|r| TileBounds::new(r.min().x, r.min().y, r.max().x, r.max().y))
                })
                .collect();
            let (_, counts) = size_bboxes(&bboxes, zoom, &opts, true);
            let planned: HashSet<u64> = counts.keys().copied().collect();
            for g in &geoms {
                for (key, _) in feature_tile_members(
                    g,
                    zoom,
                    &opts,
                    0,
                    u64::MAX,
                    &crate::diagnostics::Diagnostics::default(),
                ) {
                    assert!(
                        planned.contains(&key),
                        "emitted key {key} at z{zoom} buffer {tile_buffer} was not \
                         planned by the scan pass"
                    );
                }
            }
        }
    }
}

/// Splitting the key space into partitions must not change the union of
/// emitted tiles or their bytes: the key-range prune plus the exact leaf
/// key guard must partition the feature's tiles cleanly (no drops, dups, or
/// altered clips at a partition seam).
#[test]
fn recursive_split_is_partition_range_invariant() {
    let opts = ExportOptions::default();
    for (name, geom, zooms) in equivalence_corpus() {
        for z in zooms {
            let full = members_by_key(feature_tile_members(
                &geom,
                z,
                &opts,
                0,
                u64::MAX,
                &crate::diagnostics::Diagnostics::default(),
            ));
            if full.is_empty() {
                continue;
            }
            // Split at the median emitted key so both halves are non-empty.
            let mut keys: Vec<u64> = full.keys().copied().collect();
            keys.sort_unstable();
            let split = keys[keys.len() / 2];
            let lo = feature_tile_members(
                &geom,
                z,
                &opts,
                0,
                split,
                &crate::diagnostics::Diagnostics::default(),
            );
            let hi = feature_tile_members(
                &geom,
                z,
                &opts,
                split + 1,
                u64::MAX,
                &crate::diagnostics::Diagnostics::default(),
            );
            let mut union = HashMap::new();
            for (k, g) in lo.into_iter().chain(hi) {
                assert!(
                    union.insert(k, g).is_none(),
                    "tile {k:x} emitted by both partitions ('{name}' z{z})"
                );
            }
            let mut uk: Vec<u64> = union.keys().copied().collect();
            uk.sort_unstable();
            assert_eq!(uk, keys, "partitioned tile set diverges ('{name}' z{z})");
            for k in keys {
                assert_eq!(
                    tile_mvt(k, &union[&k], z, &opts),
                    tile_mvt(k, &full[&k], z, &opts),
                    "partitioned tile {k:x} bytes diverge ('{name}' z{z})"
                );
            }
        }
    }
}

#[test]
fn zoom_mapping_uses_explicit_zoom() {
    let meta = OverviewsMeta {
        version: "0.1.0".to_string(),
        mode: Some(Mode::Duplicating),
        canonical_level: Some(1),
        levels: vec![
            Level {
                row_group_end: 0,
                gsd: gsd(4),
                zoom: Some(4),
            },
            Level {
                row_group_end: 1,
                gsd: gsd(7),
                zoom: Some(7),
            },
        ],
        generalization: None,
    };
    assert_eq!(zoom_for_level(&meta, 0).unwrap(), 4);
    assert_eq!(zoom_for_level(&meta, 1).unwrap(), 7);
}

#[test]
fn zoom_mapping_derives_from_gsd_when_absent() {
    // No zoom on the levels: derive by rounding zoom_for_gsd(gsd).
    let meta = OverviewsMeta {
        version: "0.1.0".to_string(),
        mode: Some(Mode::Duplicating),
        canonical_level: Some(1),
        levels: vec![
            Level {
                row_group_end: 0,
                gsd: gsd(3),
                zoom: None,
            },
            Level {
                row_group_end: 1,
                gsd: gsd(8),
                zoom: None,
            },
        ],
        generalization: None,
    };
    assert_eq!(zoom_for_level(&meta, 0).unwrap(), 3);
    assert_eq!(zoom_for_level(&meta, 1).unwrap(), 8);
}

/// #371, the `--gsd` door: `zoom_for_level` used to `clamp(0.0, 255.0)`,
/// so an overview level with a fine enough GSD exported at z255 — past
/// every shift and `pow` in the tile-id and buffer math — no matter what
/// `--max-zoom` said. It now errors, naming the GSD and the implied zoom.
#[test]
fn zoom_for_level_rejects_a_gsd_implying_a_zoom_above_the_ceiling() {
    let meta = OverviewsMeta {
        version: "0.1.0".to_string(),
        mode: Some(Mode::Duplicating),
        canonical_level: Some(0),
        levels: vec![Level {
            row_group_end: 0,
            // 5e-6 m/px implies z ~= 33.
            gsd: 0.000_005,
            zoom: None,
        }],
        generalization: None,
    };
    let err = zoom_for_level(&meta, 0).expect_err("a z33 gsd must be rejected");
    assert!(
        matches!(
            err,
            ExportError::LevelZoomAboveCeiling {
                implied: 33,
                ceiling: 30,
                ..
            }
        ),
        "got: {err}"
    );
    let msg = err.to_string();
    assert!(msg.contains("0.000005"), "error names the gsd: {msg}");
    assert!(msg.contains("33"), "error names the implied zoom: {msg}");
}

/// The ceiling is inclusive on the export side too (#371).
#[test]
fn zoom_for_level_accepts_the_ceiling_exactly() {
    let meta = OverviewsMeta {
        version: "0.1.0".to_string(),
        mode: Some(Mode::Duplicating),
        canonical_level: Some(1),
        levels: vec![
            Level {
                row_group_end: 0,
                gsd: gsd(MAX_ZOOM),
                zoom: None,
            },
            Level {
                row_group_end: 1,
                gsd: gsd(MAX_ZOOM),
                zoom: Some(MAX_ZOOM),
            },
        ],
        generalization: None,
    };
    assert_eq!(zoom_for_level(&meta, 0).unwrap(), MAX_ZOOM);
    assert_eq!(zoom_for_level(&meta, 1).unwrap(), MAX_ZOOM);
}

/// #371: the degenerate GSDs are told apart. `+inf` is infinitely coarse —
/// it inverts to z = -inf, which is z0, the same floor any too-coarse level
/// lands on. `0`, `-inf`, a negative GSD and `NaN` are not ground sample
/// distances at all, and used to be reported (or, for `-inf`, not reported
/// at all) as "implies zoom 9223372036854775807". A subnormal GSD is a real
/// one, just far too fine: that stays the ceiling error.
#[test]
fn zoom_for_level_sorts_out_the_non_finite_gsds() {
    let with_gsd = |g: f64| OverviewsMeta {
        version: "0.1.0".to_string(),
        mode: Some(Mode::Duplicating),
        canonical_level: Some(0),
        levels: vec![Level {
            row_group_end: 0,
            gsd: g,
            zoom: None,
        }],
        generalization: None,
    };

    // Infinitely coarse -> the whole world in one tile.
    assert_eq!(zoom_for_level(&with_gsd(f64::INFINITY), 0).unwrap(), 0);

    for bad in [0.0, f64::NEG_INFINITY, f64::NAN, -1.0] {
        let meta = with_gsd(bad);
        let err = zoom_for_level(&meta, 0).expect_err("a bad gsd must be rejected");
        assert!(
            matches!(err, ExportError::LevelGsdNotFinitePositive { level: 0, .. }),
            "gsd {bad}: got {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("finite positive") && !msg.contains("9223372036854775807"),
            "gsd {bad}: {msg}"
        );
    }

    // A subnormal GSD is a real one, just unreachably fine: still the
    // ceiling error, not the "not a gsd" one.
    let err = zoom_for_level(&with_gsd(f64::MIN_POSITIVE / 2.0), 0)
        .expect_err("a subnormal gsd is above the ceiling");
    assert!(
        matches!(err, ExportError::LevelZoomAboveCeiling { .. }),
        "got: {err}"
    );
}

/// #371: the per-tile buffer is derived from the tile width, which was
/// `360.0 / f64::from(1u32 << zoom)` — a debug panic and a masked shift
/// (hence a wildly wrong buffer) from z32 up. Exact at the ceiling, and
/// finite and shrinking all the way past it.
#[test]
fn buffer_deg_is_exact_at_max_zoom_and_never_overflows() {
    let opts = ExportOptions::default();
    let at = |z: u8| buffer_deg_at_zoom(z, &opts);
    let frac = buffer_fraction(&opts);
    assert_eq!(at(0), 360.0 * frac);
    assert_eq!(at(MAX_ZOOM), 360.0 / 1_073_741_824.0 * frac);
    // Defensive: reachable from PMTiles-reading paths that bypass the
    // level plan, so it must stay finite and monotone past the ceiling.
    let mut prev = at(MAX_ZOOM);
    for z in [31u8, 32, 33, 64, 255] {
        let v = at(z);
        assert!(v.is_finite() && v >= 0.0, "buffer at z{z} is {v}");
        assert!(
            v < prev,
            "buffer must keep shrinking at z{z}: {v} !< {prev}"
        );
        prev = v;
    }
}

#[test]
fn partitioning_prefix_semantics_via_reader() {
    // Verify that in partitioning mode, reading level k returns the prefix
    // (accumulating features), which export relies on.
    let schema = Arc::new(source_schema());
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let specs = vec![
        LevelSpec::new(gsd(4), Some(4)),
        LevelSpec::new(gsd(6), Some(6)),
    ];
    let mut opts = OverviewWriterOptions::new(Mode::Partitioning, specs);
    opts.max_row_group_size = 10_000;
    let mut w = OverviewWriter::create(tmp.path(), &schema, opts).unwrap();
    assert_eq!(
        w.write_level(
            0,
            Some(1),
            std::iter::once(batch(
                &schema,
                &[0],
                &[Geometry::Point(Point::new(1.0, 1.0))],
            )),
        )
        .unwrap(),
        LevelWriteOutcome::Written
    );
    assert_eq!(
        w.write_level(
            1,
            Some(1),
            std::iter::once(batch(
                &schema,
                &[1],
                &[Geometry::Point(Point::new(2.0, 2.0))],
            )),
        )
        .unwrap(),
        LevelWriteOutcome::Written
    );
    w.finish().unwrap();

    let reader = OverviewReader::open(tmp.path()).unwrap();
    let l0 = read_level_features(&reader, 0, Crs::Epsg4326).unwrap();
    let l1 = read_level_features(&reader, 1, Crs::Epsg4326).unwrap();
    // Level 0 band = {feature 0}. Level 1 = prefix {0,1}.
    assert_eq!(l0.len(), 1);
    assert_eq!(l1.len(), 2);
}

/// Write an `N`-level **partitioning** fixture: `level_geoms[k]` is level
/// `k`'s own band (the features added at that level). Small row groups
/// (`max_row_group_size = 2`) force each band to span several row groups, so
/// a per-band read (`start..=end`) genuinely differs from a prefix read
/// (`0..=end`). Levels use `z = 2 + 2k`.
fn write_partitioning_fixture(
    path: &Path,
    level_geoms: &[(Vec<i64>, Vec<Geometry<f64>>)],
) -> OverviewsMeta {
    write_mode_fixture(path, level_geoms, Mode::Partitioning, 2)
}

/// #233: the single-read fan-out scan ([`scan_all_levels`]) must produce, for
/// every level, a `LevelScan` byte-identical to the independent per-level
/// prefix scan ([`scan_level`]) it replaces. In partitioning mode this is the
/// load-bearing case: level `k`'s render set is the accumulating prefix
/// `0..=k`, so a coarse band fans into every finer level. Equal `feature_count`,
/// `bounds`, and `tile_counts` ⇒ identical partition plans ⇒ identical archive.
#[test]
fn fanout_scan_matches_per_level_prefix_scan_partitioning() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    // Three bands with distinct, partly-overlapping footprints so counts
    // accumulate across levels and every level has non-empty bounds.
    let poly = |x: f64, y: f64| {
        Geometry::Polygon(geo::Polygon::new(
            LineString::from(vec![
                (x, y),
                (x + 8.0, y),
                (x + 8.0, y + 6.0),
                (x, y + 6.0),
                (x, y),
            ]),
            vec![],
        ))
    };
    let line = Geometry::LineString(LineString::from(vec![
        (-70.0, -10.0),
        (-40.0, 5.0),
        (-10.0, -5.0),
        (20.0, 8.0),
    ]));
    let meta = write_partitioning_fixture(
        tmp.path(),
        &[
            // Level 0 (coarsest, z2): a few spread points + one polygon.
            (
                vec![0, 1, 2],
                vec![
                    Geometry::Point(Point::new(-120.0, 40.0)),
                    Geometry::Point(Point::new(100.0, -30.0)),
                    poly(-100.0, 20.0),
                ],
            ),
            // Level 1 (z4): a line crossing several tiles + points.
            (
                vec![3, 4, 5, 6],
                vec![
                    line,
                    Geometry::Point(Point::new(0.0, 0.0)),
                    Geometry::Point(Point::new(-119.5, 39.5)),
                    poly(10.0, -40.0),
                ],
            ),
            // Level 2 (finest, z6): denser cluster of points + a polygon.
            (
                vec![7, 8, 9, 10, 11],
                vec![
                    Geometry::Point(Point::new(-118.0, 34.0)),
                    Geometry::Point(Point::new(-117.5, 33.8)),
                    Geometry::Point(Point::new(-117.0, 34.2)),
                    Geometry::Point(Point::new(2.0, 48.0)),
                    poly(30.0, 30.0),
                ],
            ),
        ],
    );

    let reader = OverviewReader::open(tmp.path()).unwrap();
    assert!(matches!(reader.mode(), Mode::Partitioning));
    let num_levels = reader.num_levels();
    assert_eq!(num_levels, 3);

    let opts = ExportOptions::default();
    let scans = scan_all_levels(
        &reader,
        test_band(reader.source()),
        Crs::Epsg4326,
        &opts,
        &ExportTimers::default(),
        None,
        None,
        &crate::diagnostics::Diagnostics::default(),
    )
    .unwrap();
    assert_eq!(scans.len(), num_levels);

    // Feature counts must equal the accumulating prefix sizes (3, 3+4, 3+4+5).
    assert_eq!(scans[0].feature_count, 3);
    assert_eq!(scans[1].feature_count, 7);
    assert_eq!(scans[2].feature_count, 12);

    for (level_idx, scan) in scans.iter().enumerate() {
        let zoom = zoom_for_level(&meta, level_idx).unwrap();
        let oracle = scan_level(&reader, level_idx, Crs::Epsg4326, zoom, &opts).unwrap();
        assert_eq!(
            *scan, oracle,
            "fan-out scan differs from per-level prefix scan at level {level_idx}"
        );
        // Guard against a vacuous pass: the level must actually size tiles.
        assert!(!scan.tile_counts.is_empty());
        assert!(scan.bounds.is_some());
    }
}

/// MVT has no temporal value type, so a DATE or TIMESTAMP column has to be
/// encoded as something — and "nothing" is the one choice that loses data
/// without saying so. These were dropped outright: absent from every
/// feature and from `vector_layers.fields`, with no warning. A fire
/// archive whose `acq_date` silently vanishes looks complete and has no
/// time axis.
#[test]
fn temporal_and_decimal_columns_survive_as_properties() {
    use arrow_array::{Date32Array, Decimal128Array, TimestampMicrosecondArray};

    // 2026-09-14 = 20710 days after the Unix epoch.
    let dates = Date32Array::from(vec![Some(20710), None]);
    let got = packed_props::arrow::packed_column(&dates);
    assert_eq!(
        got[0],
        Some(PropertyValue::String("2026-09-14".to_string())),
        "DATE must render as an ISO date"
    );
    assert_eq!(got[1], None, "null stays null");

    // 2026-09-14T12:34:56Z in microseconds.
    let ts = TimestampMicrosecondArray::from(vec![Some(1_789_389_296_000_000), None]);
    let got = packed_props::arrow::packed_column(&ts);
    let rendered = match &got[0] {
        Some(PropertyValue::String(s)) => s.clone(),
        other => panic!("TIMESTAMP must render as a string, got {other:?}"),
    };
    assert!(
        rendered.starts_with("2026-09-14T12:34:56"),
        "TIMESTAMP must render as ISO 8601, got {rendered:?}"
    );
    assert_eq!(got[1], None);

    // DECIMAL is a number, not a string: 1.50 at scale 2.
    let dec = Decimal128Array::from(vec![Some(150_i128), None])
        .with_precision_and_scale(4, 2)
        .unwrap();
    let got = packed_props::arrow::packed_column(&dec);
    match got[0] {
        Some(PropertyValue::Double(v)) => assert!(
            (v - 1.5).abs() < 1e-9,
            "DECIMAL(4,2) 150 must be 1.5, got {v}"
        ),
        ref other => panic!("DECIMAL must render as a double, got {other:?}"),
    }
    assert_eq!(got[1], None);
}

/// A UTC-stamped TIMESTAMP is the COMMON parquet shape, not an exotic one:
/// parquet records a single `isAdjustedToUTC` bool, which the parquet crate
/// maps to literally `Some("UTC")`, so every tz-aware column arrives spelled
/// that way.
///
/// arrow refuses to format a NAMED zone without its `chrono-tz` feature, so
/// the first cut of this change advertised such a column as a String field
/// and then emitted null for every value -- strictly worse than dropping it,
/// because the archive promised a field that was on no feature at all.
#[test]
fn utc_stamped_timestamps_encode_rather_than_vanishing() {
    use arrow_array::TimestampMicrosecondArray;
    use arrow_schema::{Field, TimeUnit};

    let dt = DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));
    let ts = TimestampMicrosecondArray::from(vec![Some(1_789_389_296_000_000), None])
        .with_data_type(dt.clone());

    let got = packed_props::arrow::packed_column(&ts);
    let rendered = match &got[0] {
        Some(PropertyValue::String(s)) => s.clone(),
        other => panic!("UTC-stamped TIMESTAMP must render as a string, got {other:?}"),
    };
    assert!(
        rendered.starts_with("2026-09-14T12:34:56"),
        "expected an ISO 8601 rendering, got {rendered:?}"
    );
    assert!(
        rendered.ends_with('Z') || rendered.contains('+'),
        "a tz-stamped value must carry its offset, got {rendered:?}"
    );
    assert_eq!(got[1], None, "null stays null");

    let schema = Schema::new(vec![Field::new("acq", dt, true)]);
    assert_eq!(
        field_metadata(&schema, None, &PublishedNames::identity())
            .get("acq")
            .map(String::as_str),
        Some("String")
    );
    assert_eq!(
        property_columns(&schema, usize::MAX, &PublishedNames::identity()).len(),
        1
    );
}

/// #359: a source column moved aside for a reserved overview column is
/// published under the name the caller's data actually had. The reserved
/// `level` column never reaches MVT properties, so by tile-writing time
/// the name is free and the reason for the move no longer applies — a
/// renderer styling on `level` must find `level`, not `level_`.
#[test]
fn renamed_source_column_is_published_under_its_source_name() {
    use arrow_schema::Field;

    let schema = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("level_", DataType::Float64, true),
        Field::new(LEVEL_COLUMN, DataType::UInt8, false),
        Field::new("geometry", DataType::Binary, false),
    ]);
    let published = PublishedNames::from_renames(
        &BTreeMap::from([("level_".to_string(), "level".to_string())]),
        &schema,
        HashSet::new(),
    );

    let cols = property_columns(&schema, 3, &published);
    let names: Vec<&str> = cols.iter().map(|(_, n)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["id", "level"],
        "the renamed column must publish as `level`, and tylertoo's own \
         `level` must still be excluded"
    );
    // The index still addresses the real (renamed) schema column.
    assert_eq!(cols[1].0, 1);

    let fields = field_metadata(&schema, Some(3), &published);
    assert_eq!(fields.get("level").map(String::as_str), Some("Number"));
    assert!(
        !fields.contains_key("level_"),
        "the internal name must not be advertised: {fields:?}"
    );
}

/// Restoration is only safe while the source name is actually free. With
/// clustering on, `point_count` IS an exported property, so restoring a
/// moved-aside `point_count_` onto it would collide two columns into one
/// MVT key. The move still applies here, so the renamed name stands.
#[test]
fn rename_is_not_restored_when_the_source_name_is_still_occupied() {
    use arrow_schema::Field;

    let schema = Schema::new(vec![
        Field::new("point_count_", DataType::Int64, true),
        Field::new("point_count", DataType::Int64, false),
        Field::new("geometry", DataType::Binary, false),
    ]);
    let published = PublishedNames::from_renames(
        &BTreeMap::from([("point_count_".to_string(), "point_count".to_string())]),
        &schema,
        HashSet::new(),
    );

    let names: Vec<String> = property_columns(&schema, 2, &published)
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert_eq!(
        names,
        vec!["point_count_", "point_count"],
        "restoring onto a live exported column would merge two columns"
    );
}

/// Two columns renamed away from the SAME source name: at most one may be
/// restored onto it.
///
/// `resolve_reserved_column_collisions` iterates fields rather than
/// distinct names, so an input carrying two columns both called `level`
/// yields `{"level_": "level", "level__": "level"}` in the provenance.
/// Restoring both would put `level` on a feature twice — a malformed tile,
/// and strictly worse than `main`, which publishes the two distinct
/// renamed names. The guard therefore has to close over the names it has
/// already handed out, not just over the schema's.
#[test]
fn two_renames_onto_one_source_name_restore_at_most_one() {
    use arrow_schema::Field;

    let schema = Schema::new(vec![
        Field::new("level_", DataType::Float64, true),
        Field::new("level__", DataType::Float64, true),
        Field::new("level", DataType::Int32, false),
        Field::new("geometry", DataType::Binary, false),
    ]);
    let published = PublishedNames::from_renames(
        &BTreeMap::from([
            ("level_".to_string(), "level".to_string()),
            ("level__".to_string(), "level".to_string()),
        ]),
        &schema,
        HashSet::new(),
    );

    let names: Vec<String> = property_columns(&schema, 3, &published)
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert_eq!(
        names.iter().filter(|n| *n == "level").count(),
        1,
        "the source name may be published once, not twice: {names:?}"
    );
    // `renames` is a BTreeMap, so which one wins is deterministic.
    assert_eq!(names, vec!["level", "level__"]);
}

/// No provenance (a file written before #359, or a run that renamed
/// nothing) publishes every column under its schema name.
#[test]
fn absent_rename_provenance_publishes_schema_names() {
    use arrow_schema::Field;

    let schema = Schema::new(vec![
        Field::new("level_", DataType::Float64, true),
        Field::new("geometry", DataType::Binary, false),
    ]);
    let names: Vec<String> = property_columns(&schema, 1, &PublishedNames::identity())
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert_eq!(names, vec!["level_"]);
}

/// #461: every row's property names point at the one per-schema name
/// table rather than at a fresh `String` per row, and the rows still
/// carry the right `(name, value)` pairs (nulls omitted, schema order).
#[test]
fn row_props_share_interned_names() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("name", DataType::Utf8, true),
        Field::new("rank", DataType::Int64, true),
        Field::new("geometry", DataType::Binary, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec![Some("a"), None, Some("c")])) as ArrayRef,
            Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(3)])),
            Arc::new(arrow_array::BinaryArray::from(vec![None::<&[u8]>; 3])),
        ],
    )
    .unwrap();
    let published = PublishedNames::identity();
    let cols = AttrColumns::new(&schema, 2, &published, None);
    let mut attrs = cols.extract(&batch).unwrap();
    let rows: Vec<SharedProps> = (0..3).map(|r| attrs.props(r)).collect();

    let pairs = |p: &SharedProps| -> Vec<(String, PropertyValue)> {
        p.iter()
            .map(|(k, v)| (k.to_string(), v.to_owned()))
            .collect()
    };
    assert_eq!(
        pairs(&rows[0]),
        vec![
            ("name".to_string(), PropertyValue::String("a".into())),
            ("rank".to_string(), PropertyValue::Int(1)),
        ]
    );
    assert_eq!(
        pairs(&rows[1]),
        vec![("rank".to_string(), PropertyValue::Int(2))]
    );
    assert_eq!(
        pairs(&rows[2]),
        vec![
            ("name".to_string(), PropertyValue::String("c".into())),
            ("rank".to_string(), PropertyValue::Int(3)),
        ]
    );

    // The names are refcounted handles to the batch's table, not copies.
    let (name_key, rank_key) = (&attrs.names[0], &attrs.names[1]);
    assert!(Arc::ptr_eq(&rows[0].names()[0], name_key));
    assert!(Arc::ptr_eq(&rows[2].names()[0], name_key));
    for row in &rows {
        assert!(Arc::ptr_eq(row.names().last().unwrap(), rank_key));
    }
}

/// A spilled member read back with every byte truncated or altered is
/// an error or a valid member, never a panic or an unchecked string.
#[test]
fn member_codec_survives_truncation_and_byte_flips() {
    let member = Member {
        key: 9,
        seq: 3,
        rank: point_rank(3),
        geom: Geometry::Point(Point::new(1.5, -2.5)).into(),
        props: SharedProps::new(vec![
            ("empty".into(), PropertyValue::String(String::new())),
            ("s".into(), PropertyValue::String("héllo".into())),
            ("f".into(), PropertyValue::Float(f32::NEG_INFINITY)),
            ("d".into(), PropertyValue::Double(f64::NAN)),
            ("i".into(), PropertyValue::Int(i64::MIN)),
            ("u".into(), PropertyValue::UInt(u64::MAX)),
            ("b".into(), PropertyValue::Bool(false)),
        ]),
        id: Some(7),
        gap: GAP_NONE,
        band: 2,
    };
    let mut names = SpillNames::default();
    names.register(member.props.names());
    let mut bytes = Vec::new();
    encode_member(&mut bytes, &member, &names);
    for end in 0..bytes.len() {
        assert!(
            decode_member(&mut SpillCursor::new(&bytes[..end]), &names).is_err(),
            "accepted truncation at {end}"
        );
    }
    let mut ok = 0usize;
    for at in 0..bytes.len() {
        for value in 0..=255u8 {
            let mut bad = bytes.clone();
            bad[at] = value;
            if let Ok(m) = decode_member(&mut SpillCursor::new(&bad), &names) {
                ok += 1;
                // Whatever decoded must iterate without panicking.
                m.props.iter().for_each(drop);
            }
        }
    }
    assert!(ok > 0, "the unaltered bytes always decode");
}

/// #461: `BatchAttributes::props` moves values out, so a second read of
/// the same row would silently return no properties. Debug builds catch
/// that misuse instead of dropping a feature's attributes.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "read twice")]
fn row_read_twice_panics_in_debug() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("rank", DataType::Int64, true),
        Field::new("geometry", DataType::Binary, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(vec![Some(1)])) as ArrayRef,
            Arc::new(arrow_array::BinaryArray::from(vec![None::<&[u8]>])),
        ],
    )
    .unwrap();
    let published = PublishedNames::identity();
    let mut attrs = AttrColumns::new(&schema, 1, &published, None)
        .extract(&batch)
        .unwrap();
    attrs.props(0);
    let _ = attrs.props(0);
}

/// The selected-columns predicate and the extraction arms must agree: a
/// column advertised in `vector_layers.fields` has to be fillable. A named
/// timezone is not renderable without a tz database, so it must be neither
/// #434 follow-up (tippecanoe parity gate): a bbox covering struct is
/// index metadata whatever it is called. A file with no `geo` metadata
/// and a covering named `geometry_bbox` (the madagascar fixture) must
/// not have that struct JSON-encoded into every feature, advertised, or
/// warned about -- and asking for it by name is the covering error
/// (#401), not "unsupported".
#[test]
fn covering_struct_is_recognised_by_shape_whatever_its_name() {
    use arrow_schema::Fields;

    let bbox_of = |dt: DataType| {
        DataType::Struct(Fields::from(vec![
            Field::new("xmin", dt.clone(), true),
            Field::new("ymin", dt.clone(), true),
            Field::new("xmax", dt.clone(), true),
            Field::new("ymax", dt, true),
        ]))
    };
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        // Covering under a non-default name, float children: shape rule.
        Field::new("geometry_bbox", bbox_of(DataType::Float32), true),
        // Covering under a default name with non-float children: name rule.
        Field::new("bbox", bbox_of(DataType::Int32), true),
        // Same shape but not a covering: a real property, JSON-encoded.
        Field::new(
            "extent",
            DataType::Struct(Fields::from(vec![
                Field::new("w", DataType::Float64, true),
                Field::new("h", DataType::Float64, true),
            ])),
            true,
        ),
        Field::new("geometry", DataType::Binary, false),
    ]);
    let published = PublishedNames::identity();
    let geom_idx = 4;

    let names: Vec<String> = property_columns(&schema, geom_idx, &published)
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert_eq!(
        names,
        vec!["id", "extent"],
        "coverings must not be properties"
    );
    assert!(
        skipped_property_columns(&schema, geom_idx, &published).is_empty(),
        "coverings must not be warned about either"
    );
    let fields = field_metadata(&schema, Some(geom_idx), &published);
    assert!(!fields.contains_key("geometry_bbox"), "{fields:?}");
    assert!(!fields.contains_key("bbox"), "{fields:?}");
    assert_eq!(fields.get("extent").map(String::as_str), Some("String"));

    for name in ["geometry_bbox", "bbox"] {
        let err = PublishedNames::identity()
            .with_selection(
                &schema,
                geom_idx,
                &PropertySelection {
                    include: Some(vec![name.to_string()]),
                    ..Default::default()
                },
                &FeatureOrder::Input,
            )
            .unwrap_err();
        assert!(
            matches!(&err, ExportError::CoveringProperty { name: n } if n == name),
            "{name}: expected CoveringProperty (#401), got {err:?}"
        );
    }

    // The declared covering name (from `geo` metadata) is honoured even
    // for a struct that matches neither the default names nor the
    // float-children rule.
    let schema = Schema::new(vec![
        Field::new("my_cover", bbox_of(DataType::Int32), true),
        Field::new("geometry", DataType::Binary, false),
    ]);
    let declared = PublishedNames {
        covering: Some("my_cover".to_string()),
        ..PublishedNames::identity()
    };
    assert!(property_columns(&schema, 1, &declared).is_empty());
    assert!(
        !property_columns(&schema, 1, &PublishedNames::identity()).is_empty(),
        "without the declaration an int-children struct under a custom name is a property"
    );
}

/// selected nor advertised -- dropping it is honest, promising it is not.
#[test]
fn unrenderable_timezone_is_neither_selected_nor_advertised() {
    use arrow_schema::{Field, TimeUnit};

    let dt = DataType::Timestamp(TimeUnit::Microsecond, Some("America/New_York".into()));
    assert!(
        !is_supported_scalar(&dt),
        "a named zone must not be claimed"
    );

    let schema = Schema::new(vec![Field::new("local", dt, true)]);
    assert!(!field_metadata(&schema, None, &PublishedNames::identity()).contains_key("local"));
    assert!(property_columns(&schema, usize::MAX, &PublishedNames::identity()).is_empty());
}

/// An unscaled DECIMAL is an integer, and `DECIMAL(38,0)` is a common
/// carrier for IDs. Routing one through f64 rounds it past 53 bits, turning
/// an ID into a different ID, so exact values go out as MVT ints.
#[test]
fn unscaled_decimals_stay_exact() {
    use arrow_array::Decimal128Array;

    let id = 9_007_199_254_740_993_i128; // 2^53 + 1: not representable in f64
    let dec = Decimal128Array::from(vec![Some(id), None])
        .with_precision_and_scale(38, 0)
        .unwrap();
    let got = packed_props::arrow::packed_column(&dec);
    assert_eq!(
        got[0],
        Some(PropertyValue::Int(9_007_199_254_740_993)),
        "an unscaled decimal must survive exactly, not round through f64"
    );
    assert_eq!(got[1], None);

    // Negative scale is arrow's `unscaled * 10^-scale`: 150 at scale -3 is
    // 150000, still an exact integer.
    let neg = Decimal128Array::from(vec![Some(150_i128)])
        .with_precision_and_scale(10, -3)
        .unwrap();
    assert_eq!(
        packed_props::arrow::packed_column(&neg)[0],
        Some(PropertyValue::Int(150_000))
    );

    // Too big for i64 falls back to the lossy double rather than dropping.
    let huge = Decimal128Array::from(vec![Some(i128::MAX)])
        .with_precision_and_scale(38, 0)
        .unwrap();
    assert!(matches!(
        packed_props::arrow::packed_column(&huge)[0],
        Some(PropertyValue::Double(_))
    ));
}

/// `Decimal256` has no i128 fast path, so it goes through arrow's text.
#[test]
fn decimal256_encodes_as_a_double() {
    use arrow_array::Decimal256Array;
    use arrow_buffer::i256;

    let dec = Decimal256Array::from(vec![Some(i256::from_i128(150)), None])
        .with_precision_and_scale(40, 2)
        .unwrap();
    let got = packed_props::arrow::packed_column(&dec);
    match got[0] {
        Some(PropertyValue::Double(v)) => {
            assert!(
                (v - 1.5).abs() < 1e-9,
                "DECIMAL256(40,2) 150 must be 1.5, got {v}"
            );
        }
        ref other => panic!("DECIMAL256 must render as a double, got {other:?}"),
    }
    assert_eq!(got[1], None);
}

/// The remaining temporal spellings all render, and all as strings.
#[test]
fn date64_and_time_columns_render_as_iso_text() {
    use arrow_array::{Date64Array, Time32SecondArray, Time64MicrosecondArray};

    // 2026-09-14T00:00:00 in milliseconds.
    let d64 = Date64Array::from(vec![Some(1_789_344_000_000)]);
    assert_eq!(
        packed_props::arrow::packed_column(&d64)[0],
        Some(PropertyValue::String("2026-09-14T00:00:00".to_string())),
        "Date64 keeps arrow's datetime rendering -- truncating would discard \
         a time component the type may legitimately carry"
    );

    let t32 = Time32SecondArray::from(vec![Some(45_296)]);
    assert_eq!(
        packed_props::arrow::packed_column(&t32)[0],
        Some(PropertyValue::String("12:34:56".to_string()))
    );

    let t64 = Time64MicrosecondArray::from(vec![Some(45_296_000_000)]);
    match &packed_props::arrow::packed_column(&t64)[0] {
        Some(PropertyValue::String(s)) => assert!(s.starts_with("12:34:56"), "got {s:?}"),
        other => panic!("Time64 must render as a string, got {other:?}"),
    }
}

/// End to end: a DATE and a UTC TIMESTAMP written into an overview file
/// must come back out of a decoded tile as properties.
///
/// The unit tests above pin `extract_property_column` and `field_metadata`
/// in isolation; this one pins that the two actually meet in the tile. The
/// original bug was invisible at the unit level precisely because nothing
/// checked the whole path.
#[test]
fn temporal_properties_reach_a_decoded_tile() {
    use arrow_array::{Date32Array, TimestampMicrosecondArray};
    use arrow_schema::TimeUnit;

    let tz = DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));
    let schema = Arc::new(Schema::new(vec![
        Field::new("acq_date", DataType::Date32, false),
        Field::new("acq_ts", tz.clone(), false),
        geometry_field(),
    ]));
    let geoms = vec![Geometry::Point(Point::new(-120.0, 40.0))];

    let tmp = tempfile::NamedTempFile::new().unwrap();
    let mut opts =
        OverviewWriterOptions::new(Mode::Duplicating, vec![LevelSpec::new(gsd(2), Some(2))]);
    opts.max_row_group_size = 10_000;
    let mut writer = OverviewWriter::create(tmp.path(), &schema, opts).unwrap();
    let rb = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Date32Array::from(vec![20710])),
            Arc::new(
                TimestampMicrosecondArray::from(vec![1_789_389_296_000_000]).with_data_type(tz),
            ),
            Arc::new(build_geometry_array(&geoms).to_array_ref()),
        ],
    )
    .unwrap();
    assert_eq!(
        writer.write_level(0, Some(1), std::iter::once(rb)).unwrap(),
        LevelWriteOutcome::Written
    );
    writer.finish().unwrap();

    let reader = OverviewReader::open(tmp.path()).unwrap();
    let feats = read_level_features(&reader, 0, Crs::Epsg4326).unwrap();
    let tiles = encode_level_tiles(&feats, 2, &ExportOptions::default()).unwrap();
    assert_eq!(tiles.len(), 1);

    let decoded = decode_tile(&tiles[0].data);
    let layer = &decoded.layers[0];
    for want in ["acq_date", "acq_ts"] {
        assert!(
            layer.keys.iter().any(|k| k == want),
            "{want} never reached the tile; keys were {:?}",
            layer.keys
        );
    }
    let strings: Vec<&str> = layer
        .values
        .iter()
        .filter_map(|v| v.string_value.as_deref())
        .collect();
    assert!(
        strings.contains(&"2026-09-14"),
        "DATE value missing from the tile: {strings:?}"
    );
    assert!(
        strings.iter().any(|v| v.starts_with("2026-09-14T12:34:56")),
        "TIMESTAMP value missing from the tile: {strings:?}"
    );
}

/// A column the export cannot encode must not be advertised in
/// `vector_layers.fields` either, and one it can must be.
#[test]
fn temporal_and_decimal_columns_are_advertised_in_field_metadata() {
    use arrow_schema::{Field, TimeUnit};

    let schema = Schema::new(vec![
        Field::new("d", DataType::Date32, true),
        Field::new("ts", DataType::Timestamp(TimeUnit::Microsecond, None), true),
        Field::new("dec", DataType::Decimal128(10, 3), true),
    ]);
    let meta = field_metadata(&schema, None, &PublishedNames::identity());
    assert_eq!(meta.get("d").map(String::as_str), Some("String"));
    assert_eq!(meta.get("ts").map(String::as_str), Some("String"));
    assert_eq!(meta.get("dec").map(String::as_str), Some("Number"));
}

// --- #434: nested columns as JSON strings, unsupported ones warned ------

/// The Overture shape: `names` is a struct holding a string and a
/// string-keyed map, `sources` a list of strings, plus a binary column
/// the export cannot encode. `n` rows per level, two levels, one row per
/// row group so the once-per-export accounting is exercised across many
/// row groups. Row `null_row` (when given) has null `names` and
/// `sources`.
fn write_overture_fixture(path: &Path, n: usize, null_row: Option<usize>, binary: &str) {
    use arrow_array::{
        builder::{ListBuilder, MapBuilder, StringBuilder},
        BinaryArray, StructArray,
    };

    let primary = StringArray::from(
        (0..n)
            .map(|i| (Some(i) != null_row).then(|| format!("Place {i}")))
            .collect::<Vec<_>>(),
    );
    let mut common = MapBuilder::new(None, StringBuilder::new(), StringBuilder::new());
    for i in 0..n {
        if Some(i) == null_row {
            common.append(false).unwrap();
            continue;
        }
        common.keys().append_value("fr");
        common.values().append_value(format!("Lieu {i}"));
        common.keys().append_value("de");
        common.values().append_value(format!("Ort {i}"));
        common.append(true).unwrap();
    }
    let common = common.finish();
    let names_nulls = null_row.map(|r| {
        arrow_array::BooleanArray::from((0..n).map(|i| i != r).collect::<Vec<_>>())
            .values()
            .clone()
            .into()
    });
    let names = StructArray::new(
        vec![
            Arc::new(Field::new("primary", DataType::Utf8, true)),
            Arc::new(Field::new("common", common.data_type().clone(), true)),
        ]
        .into(),
        vec![Arc::new(primary) as ArrayRef, Arc::new(common) as ArrayRef],
        names_nulls,
    );
    let mut sources = ListBuilder::new(StringBuilder::new());
    for i in 0..n {
        if Some(i) == null_row {
            sources.append_null();
            continue;
        }
        sources.values().append_value("osm");
        sources.values().append_value("meta");
        sources.append(true);
    }
    let sources = sources.finish();
    let raw = BinaryArray::from_iter_values((0..n).map(|i| format!("blob{i}").into_bytes()));

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("names", names.data_type().clone(), true),
        Field::new("sources", sources.data_type().clone(), true),
        Field::new(binary, DataType::Binary, false),
        geometry_field(),
    ]));
    let specs = vec![
        LevelSpec::new(gsd(2), Some(2)),
        LevelSpec::new(gsd(4), Some(4)),
    ];
    let mut opts = OverviewWriterOptions::new(Mode::Duplicating, specs);
    opts.max_row_group_size = 1;
    let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
    for level in 0..2 {
        let geoms: Vec<Geometry<f64>> = (0..n)
            .map(|i| Geometry::Point(Point::new(-120.0 + i as f64 * 0.01, 40.0)))
            .collect();
        let rb = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from((0..n as i64).collect::<Vec<_>>())),
                Arc::new(names.clone()),
                Arc::new(sources.clone()),
                Arc::new(raw.clone()),
                Arc::new(build_geometry_array(&geoms).to_array_ref()),
            ],
        )
        .unwrap();
        assert_eq!(
            writer
                .write_level(level, Some(n), std::iter::once(rb))
                .unwrap(),
            LevelWriteOutcome::Written
        );
    }
    writer.finish().unwrap();
}

/// The archive's `vector_layers[].fields` (name -> type) over every layer.
fn archive_fields(path: &Path) -> HashMap<String, String> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .unwrap()
        .read_to_end(&mut buf)
        .unwrap();
    // PMTiles v3 header: the JSON metadata offset/length live at bytes 24..40.
    let off = u64::from_le_bytes(buf[24..32].try_into().unwrap()) as usize;
    let len = u64::from_le_bytes(buf[32..40].try_into().unwrap()) as usize;
    let json = compression::decompress_capped(
        &buf[off..off + len],
        Compression::Gzip,
        compression::MAX_INTERNAL_BYTES,
    )
    .expect("archive metadata is gzip");
    let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
    v["vector_layers"]
        .as_array()
        .expect("vector_layers")
        .iter()
        .flat_map(|l| {
            l["fields"].as_object().into_iter().flat_map(|o| {
                o.iter()
                    .map(|(k, t)| (k.clone(), t.as_str().unwrap().to_string()))
            })
        })
        .collect()
}

/// The decoded parquet's property column names (everything `decode`
/// adds and the geometry left out).
fn decoded_property_names(pmtiles: &Path) -> Vec<String> {
    let decoded = tempfile::NamedTempFile::new().unwrap();
    crate::decode::decode_pmtiles(
        pmtiles,
        decoded.path(),
        &crate::decode::DecodeOptions::default(),
    )
    .unwrap();
    let file = std::fs::File::open(decoded.path()).unwrap();
    tylertoo_io::open_parquet(file)
        .unwrap()
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .filter(|n| {
            !matches!(
                n.as_str(),
                "zoom" | "layer" | "mvt_id" | "geometry" | "bbox"
            )
        })
        .collect()
}

/// #434, end to end on the Overture shape: the struct and the list reach
/// the tiles as JSON strings and are advertised as `String`; the binary
/// column is dropped, counted in the report, and never advertised; the
/// file's own bbox covering struct is neither exported nor counted.
#[test]
fn overture_shaped_nested_columns_export_as_json_strings() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_overture_fixture(tin.path(), 3, Some(2), "raw");
    let tout = tempfile::NamedTempFile::new().unwrap();
    let report = export_pmtiles(tin.path(), tout.path(), &ExportOptions::default()).unwrap();

    assert_eq!(
        report.skipped_property_columns,
        vec![SkippedPropertyColumn {
            name: "raw".to_string(),
            data_type: "Binary".to_string(),
        }],
        "the binary column is the one and only skipped column; the bbox \
         covering must not be counted"
    );

    let fields = archive_fields(tout.path());
    assert_eq!(fields.get("names").map(String::as_str), Some("String"));
    assert_eq!(fields.get("sources").map(String::as_str), Some("String"));
    assert_eq!(fields.get("id").map(String::as_str), Some("Number"));
    assert!(
        !fields.contains_key("raw"),
        "binary must not be advertised: {fields:?}"
    );
    assert!(
        !fields.contains_key("bbox"),
        "the covering is not a property: {fields:?}"
    );

    let mut names = decoded_property_names(tout.path());
    names.sort();
    assert_eq!(names, vec!["id", "names", "sources"]);

    // The tile values themselves: one JSON string per non-null row.
    let reader = OverviewReader::open(tin.path()).unwrap();
    let feats = read_level_features(&reader, 0, Crs::Epsg4326).unwrap();
    let tiles = encode_level_tiles(&feats, 2, &ExportOptions::default()).unwrap();
    let strings: Vec<String> = tiles
        .iter()
        .flat_map(|t| decode_tile(&t.data).layers)
        .flat_map(|l| l.values)
        .filter_map(|v| v.string_value)
        .collect();
    for want in [
        r#"{"primary":"Place 0","common":{"fr":"Lieu 0","de":"Ort 0"}}"#,
        r#"{"primary":"Place 1","common":{"fr":"Lieu 1","de":"Ort 1"}}"#,
        r#"["osm","meta"]"#,
    ] {
        assert!(
            strings.iter().any(|s| s == want),
            "{want} missing from the tile strings: {strings:?}"
        );
    }
    assert!(
        !strings.iter().any(|s| s.contains("Place 2") || s == "null"),
        "a null struct row must carry no property at all: {strings:?}"
    );
    // Each feature's tag count: `id` + `names` + `sources` for rows 0
    // and 1, `id` alone for the null row.
    let tag_counts: Vec<usize> = tiles
        .iter()
        .flat_map(|t| decode_tile(&t.data).layers)
        .flat_map(|l| l.features)
        .map(|f| f.tags.len() / 2)
        .collect();
    let mut sorted = tag_counts.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![1, 3, 3], "tags per feature: {tag_counts:?}");
}

/// #434: the warning is said once per skipped column per export -- not
/// once per row group, level or tile -- and the report lists it once.
#[test]
fn skipped_column_is_warned_and_reported_once_per_export() {
    use std::sync::Mutex;

    struct CountingLogger;
    static HITS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    impl log::Log for CountingLogger {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Warn
        }
        fn log(&self, record: &log::Record) {
            if record.level() == log::Level::Warn {
                HITS.lock().unwrap().push(record.args().to_string());
            }
        }
        fn flush(&self) {}
    }
    // Another test in this binary may have installed a logger first;
    // then only the report half of this test is checkable.
    let logging = log::set_logger(&CountingLogger).is_ok();
    // Only with our own logger: the level is process-global too, and
    // lowering it under another test's logger drops that test's lines.
    if logging {
        log::set_max_level(log::LevelFilter::Warn);
    }

    // 6 rows x 2 levels at one row per row group: 12 row groups, and
    // every level's export re-extracts the columns per batch.
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_overture_fixture(tin.path(), 6, None, "raw_once_434");
    let tout = tempfile::NamedTempFile::new().unwrap();
    let report = export_pmtiles(tin.path(), tout.path(), &ExportOptions::default()).unwrap();
    assert_eq!(report.skipped_property_columns.len(), 1);
    assert_eq!(report.skipped_property_columns[0].name, "raw_once_434");
    assert!(report.total_tiles >= 2, "both levels exported");

    if logging {
        // The logger is process-global, so sibling tests' warnings land
        // here too: count only this fixture's uniquely named column.
        let hits = HITS.lock().unwrap();
        let raw_warnings = hits
            .iter()
            .filter(|m| m.contains("not exporting property \"raw_once_434\""))
            .count();
        assert_eq!(
            raw_warnings, 1,
            "one warning per skipped column per export; warnings were {hits:?}"
        );
        assert!(
            hits.iter()
                .filter(|m| m.contains("not exporting property"))
                .all(|m| !m.contains("bbox")),
            "the covering struct must not be warned about: {hits:?}"
        );
    }
}

/// #434: `--include-property` addresses a JSON-encoded column like any
/// other, and naming a column the export cannot encode says so instead
/// of calling it unknown.
#[test]
fn property_selection_covers_json_encoded_columns() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_overture_fixture(tin.path(), 2, None, "raw");

    let opts = ExportOptions {
        properties: PropertySelection {
            include: Some(vec!["names".to_string()]),
            ..Default::default()
        },
        ..ExportOptions::default()
    };
    let tout = tempfile::NamedTempFile::new().unwrap();
    export_pmtiles(tin.path(), tout.path(), &opts).expect("include names exports");
    assert_eq!(decoded_property_names(tout.path()), vec!["names"]);
    let fields = archive_fields(tout.path());
    assert_eq!(fields.len(), 1, "{fields:?}");
    assert_eq!(fields.get("names").map(String::as_str), Some("String"));

    let opts = ExportOptions {
        properties: PropertySelection {
            exclude: vec!["names".to_string(), "raw".to_string()],
            ..Default::default()
        },
        ..ExportOptions::default()
    };
    let tout = tempfile::NamedTempFile::new().unwrap();
    let report = export_pmtiles(tin.path(), tout.path(), &opts).expect("exclude exports");
    let mut names = decoded_property_names(tout.path());
    names.sort();
    assert_eq!(names, vec!["id", "sources"]);
    assert!(
        report.skipped_property_columns.is_empty(),
        "an excluded binary column was asked for, not dropped: {:?}",
        report.skipped_property_columns
    );

    let opts = ExportOptions {
        properties: PropertySelection {
            include: Some(vec!["raw".to_string()]),
            ..Default::default()
        },
        ..ExportOptions::default()
    };
    let tout = tempfile::NamedTempFile::new().unwrap();
    let err = export_pmtiles(tin.path(), tout.path(), &opts).unwrap_err();
    match err {
        ExportError::UnsupportedProperty { name, data_type } => {
            assert_eq!(name, "raw");
            assert_eq!(data_type, "Binary");
        }
        other => panic!("expected UnsupportedProperty, got {other:?}"),
    }

    // The file's bbox covering struct is index metadata, not a
    // property. Asking for it gets its own error (#401), as on the
    // convert side: never "an unencodable struct" (structs ARE
    // encodable, and that message would lie), and never "unknown",
    // which sends the caller hunting for a typo in a column the file
    // plainly has.
    let opts = ExportOptions {
        properties: PropertySelection {
            include: Some(vec!["bbox".to_string()]),
            ..Default::default()
        },
        ..ExportOptions::default()
    };
    let tout = tempfile::NamedTempFile::new().unwrap();
    let err = export_pmtiles(tin.path(), tout.path(), &opts).unwrap_err();
    assert!(
        matches!(&err, ExportError::CoveringProperty { name } if name == "bbox"),
        "the covering must be named as the covering, got {err:?}"
    );
    assert!(err.to_string().contains("regenerates"), "{err}");
}

/// #434 review: the map shape is decided from the key TYPE. A binary
/// key has no rendering, so it must not be mistaken for a string key
/// (`{null:"x"}` is not JSON); it renders as pairs with `null` keys,
/// and the output parses.
#[test]
fn binary_keyed_map_renders_as_pairs_and_stays_valid_json() {
    use arrow_array::builder::{BinaryBuilder, MapBuilder, StringBuilder};

    let mut m = MapBuilder::new(None, BinaryBuilder::new(), StringBuilder::new());
    m.keys().append_value(b"k1");
    m.values().append_value("x");
    m.keys().append_value(b"k2");
    m.values().append_value("y");
    m.append(true).unwrap();
    m.append(true).unwrap(); // empty map
    let m = m.finish();
    let got = packed_props::arrow::packed_column(&m);
    let rendered: Vec<&str> = got
        .iter()
        .map(|v| match v {
            Some(PropertyValue::String(s)) => s.as_str(),
            other => panic!("expected a string, got {other:?}"),
        })
        .collect();
    assert_eq!(rendered, vec![r#"[[null,"x"],[null,"y"]]"#, "[]"]);
    for s in &rendered {
        serde_json::from_str::<serde_json::Value>(s).expect("valid JSON");
    }
    // The empty row alone must take the same (array) shape: the choice
    // does not depend on which keys a batch happens to hold.
    let empty_only = m.slice(1, 1);
    assert_eq!(
        packed_props::arrow::packed_column(&empty_only)[0],
        Some(PropertyValue::String("[]".to_string()))
    );
}

/// #434 review: a dictionary-encoded struct nested in a list must
/// resolve into a nested node, not be stringified as its own column and
/// embedded as a string literal (double-encoded).
#[test]
fn dictionary_encoded_struct_inside_a_list_is_not_double_encoded() {
    use arrow_array::{
        types::Int8Type, DictionaryArray, Int32Array, Int8Array, ListArray, StructArray,
    };
    use arrow_buffer::OffsetBuffer;

    let values = StructArray::from(vec![(
        Arc::new(Field::new("n", DataType::Int32, true)),
        Arc::new(Int32Array::from(vec![10, 20])) as ArrayRef,
    )]);
    let dict = DictionaryArray::<Int8Type>::try_new(
        Int8Array::from(vec![Some(1), Some(0), None]),
        Arc::new(values),
    )
    .unwrap();
    let item = Arc::new(Field::new("item", dict.data_type().clone(), true));
    let list = ListArray::new(
        item,
        OffsetBuffer::from_lengths([3, 0]),
        Arc::new(dict),
        None,
    );
    let got = packed_props::arrow::packed_column(&list);
    assert_eq!(
        got[0],
        Some(PropertyValue::String(
            r#"[{"n":20},{"n":10},null]"#.to_string()
        )),
        "nested struct values must be JSON values, not string literals"
    );
    assert_eq!(got[1], Some(PropertyValue::String("[]".to_string())));
}

/// #434: the JSON rendering, value by value. Nulls inside a struct are
/// `null` members (the struct is still a value), a non-finite float is
/// `null`, a non-string map key makes an array of pairs, fixed-size and
/// large lists render like lists, nested lists nest, and a
/// dictionary-encoded column is unwrapped to its value type.
#[test]
fn json_property_rendering_covers_every_nested_kind() {
    use arrow_array::{
        builder::{
            FixedSizeListBuilder, Int32Builder, LargeListBuilder, ListBuilder, MapBuilder,
            StringBuilder,
        },
        types::Int8Type,
        DictionaryArray, Float64Array, Int32Array, StructArray,
    };

    // struct { n: Int32?, x: Float64?, tags: List<Int32>? }
    let n = Int32Array::from(vec![Some(1), None, Some(3)]);
    let x = Float64Array::from(vec![Some(1.5), Some(f64::NAN), Some(-0.25)]);
    let mut tags = ListBuilder::new(Int32Builder::new());
    tags.values().append_value(7);
    tags.values().append_value(8);
    tags.append(true);
    tags.append_null();
    tags.append(true); // empty list
    let tags = tags.finish();
    let st = StructArray::from(vec![
        (
            Arc::new(Field::new("n", DataType::Int32, true)),
            Arc::new(n) as ArrayRef,
        ),
        (
            Arc::new(Field::new("x", DataType::Float64, true)),
            Arc::new(x) as ArrayRef,
        ),
        (
            Arc::new(Field::new("tags", tags.data_type().clone(), true)),
            Arc::new(tags) as ArrayRef,
        ),
    ]);
    let got = packed_props::arrow::packed_column(&st);
    let strings: Vec<Option<&str>> = got
        .iter()
        .map(|v| match v {
            Some(PropertyValue::String(s)) => Some(s.as_str()),
            None => None,
            other => panic!("nested column must render as a string: {other:?}"),
        })
        .collect();
    assert_eq!(
        strings,
        vec![
            Some(r#"{"n":1,"x":1.5,"tags":[7,8]}"#),
            Some(r#"{"n":null,"x":null,"tags":null}"#),
            Some(r#"{"n":3,"x":-0.25,"tags":[]}"#),
        ]
    );
    assert_eq!(
        field_type_name(st.data_type()),
        Some("String"),
        "a struct is advertised as a string"
    );

    // Map<Int32, Utf8>: keys are not strings, so pairs, not an object.
    let mut m = MapBuilder::new(None, Int32Builder::new(), StringBuilder::new());
    m.keys().append_value(1);
    m.values().append_value("one");
    m.keys().append_value(2);
    m.values().append_null();
    m.append(true).unwrap();
    let m = m.finish();
    assert_eq!(
        packed_props::arrow::packed_column(&m)[0],
        Some(PropertyValue::String(r#"[[1,"one"],[2,null]]"#.to_string()))
    );

    // FixedSizeList<Int32, 2> and LargeList<Utf8>, with a quote to escape.
    let mut fsl = FixedSizeListBuilder::new(Int32Builder::new(), 2);
    fsl.values().append_value(1);
    fsl.values().append_value(2);
    fsl.append(true);
    let fsl = fsl.finish();
    assert_eq!(
        packed_props::arrow::packed_column(&fsl)[0],
        Some(PropertyValue::String("[1,2]".to_string()))
    );
    let mut ll = LargeListBuilder::new(StringBuilder::new());
    ll.values().append_value("say \"hi\"");
    ll.append(true);
    let ll = ll.finish();
    assert_eq!(
        packed_props::arrow::packed_column(&ll)[0],
        Some(PropertyValue::String(r#"["say \"hi\""]"#.to_string()))
    );

    // Dictionary<Int8, Utf8> unwraps to plain strings, not JSON.
    let dict: DictionaryArray<Int8Type> = vec![Some("a"), None, Some("b")].into_iter().collect();
    assert_eq!(
        packed_props::arrow::packed_column(&dict),
        vec![
            Some(PropertyValue::String("a".to_string())),
            None,
            Some(PropertyValue::String("b".to_string())),
        ]
    );
    assert_eq!(field_type_name(dict.data_type()), Some("String"));
    assert!(is_exportable(dict.data_type()));

    // Binary has no encoding at all: not exportable, not advertised.
    assert!(!is_exportable(&DataType::Binary));
    assert!(!is_exportable(&DataType::LargeBinary));
    assert_eq!(field_type_name(&DataType::Binary), None);
    assert_eq!(
        field_type_name(&DataType::Dictionary(
            Box::new(DataType::Int32),
            Box::new(DataType::Binary)
        )),
        None,
        "a dictionary over an unsupported value type is unsupported"
    );
}

/// #740: the collect extracts properties per row chunk, from zero-copy
/// slices of the batch's columns. A slice of a nested column must render
/// exactly the rows the whole column renders there, for every nested
/// kind: a list or map slice keeps its whole child array, which the
/// rendering rebases onto the slice.
#[test]
fn sliced_nested_columns_render_like_the_whole_column() {
    use arrow_array::{
        builder::{
            FixedSizeListBuilder, Int32Builder, LargeListBuilder, ListBuilder, MapBuilder,
            StringBuilder,
        },
        StructArray,
    };

    let mut list = ListBuilder::new(Int32Builder::new());
    let mut large = LargeListBuilder::new(StringBuilder::new());
    let mut map = MapBuilder::new(None, StringBuilder::new(), Int32Builder::new());
    let mut fixed = FixedSizeListBuilder::new(Int32Builder::new(), 2);
    for row in 0..7i32 {
        if row % 3 == 1 {
            list.append_null();
            large.append_null();
            map.append(false).unwrap();
        } else {
            for v in 0..row {
                list.values().append_value(row * 10 + v);
                large.values().append_value(format!("r{row}v{v}"));
                map.keys().append_value(format!("k{v}"));
                map.values().append_value(v);
            }
            list.append(true);
            large.append(true);
            map.append(true).unwrap();
        }
        fixed.values().append_value(row);
        fixed.values().append_value(-row);
        fixed.append(row % 2 == 0);
    }
    let list: ArrayRef = Arc::new(list.finish());
    let nested = StructArray::from(vec![(
        Arc::new(Field::new("l", list.data_type().clone(), true)),
        Arc::clone(&list),
    )]);
    let columns: Vec<ArrayRef> = vec![
        list,
        Arc::new(large.finish()),
        Arc::new(map.finish()),
        Arc::new(fixed.finish()),
        Arc::new(nested),
    ];
    for column in &columns {
        let whole = packed_props::arrow::packed_column(column.as_ref());
        assert!(whole.iter().any(Option::is_some));
        for offset in 0..column.len() {
            for len in 0..=column.len() - offset {
                assert_eq!(
                    packed_props::arrow::packed_column(column.slice(offset, len).as_ref()),
                    whole[offset..offset + len],
                    "{:?} rows {offset}..{}",
                    column.data_type(),
                    offset + len
                );
            }
        }
    }
}

#[test]
fn export_feature_counts_and_level_absent_from_props() {
    // Two features far apart so at a coarse zoom each lands in its own tile.
    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let b = Geometry::Point(Point::new(120.0, -40.0));
    let tmp = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tmp.path(),
        &[
            (vec![0, 1], vec![a.clone(), b.clone()]),
            (vec![0, 1], vec![a.clone(), b.clone()]),
        ],
    );

    let reader = OverviewReader::open(tmp.path()).unwrap();
    let feats = read_level_features(&reader, 0, Crs::Epsg4326).unwrap();
    assert_eq!(feats.len(), 2);

    // Level 0 -> zoom 2. Two far-apart points => two distinct tiles.
    let tiles = encode_level_tiles(&feats, 2, &ExportOptions::default()).unwrap();
    assert_eq!(tiles.len(), 2, "two far-apart points => two tiles");
    for t in &tiles {
        assert_eq!(t.feature_count, 1);
        let decoded = decode_tile(&t.data);
        let layer = &decoded.layers[0];
        assert_eq!(layer.features.len(), 1);
        // `level` column must NOT appear as an MVT property key.
        assert!(
            !layer.keys.iter().any(|k| k.eq_ignore_ascii_case("level")),
            "level column leaked into MVT properties: {:?}",
            layer.keys
        );
        // Carried props: id + name present.
        assert!(layer.keys.iter().any(|k| k == "id"));
        assert!(layer.keys.iter().any(|k| k == "name"));
        assert_eq!(
            decoded.layers[0].features[0].r#type,
            Some(GeomType::Point as i32)
        );
    }
}

#[test]
fn clipped_features_within_tile_plus_buffer() {
    // A line spanning a wide longitude range: at a fine-ish zoom it crosses
    // several tiles and must be clipped into each, with coords bounded by
    // [-buffer, extent+buffer].
    // Span ~60° of longitude so at z4 (22.5°/tile) it crosses several tiles.
    let line = Geometry::LineString(LineString::from(vec![
        (-100.0, 10.0),
        (-80.0, 12.0),
        (-60.0, 8.0),
        (-40.0, 11.0),
    ]));
    let tmp = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tmp.path(),
        &[(vec![0], vec![line.clone()]), (vec![0], vec![line.clone()])],
    );
    let reader = OverviewReader::open(tmp.path()).unwrap();
    let feats = read_level_features(&reader, 1, Crs::Epsg4326).unwrap();

    let opts = ExportOptions::default();
    // Level 1 -> zoom 4.
    let tiles = encode_level_tiles(&feats, 4, &opts).unwrap();
    assert!(tiles.len() >= 2, "wide line must span multiple tiles");
    let extent = opts.extent as i32;
    let slack = extent * opts.tile_buffer as i32 / 256 + 4;
    for t in &tiles {
        let decoded = decode_tile(&t.data);
        for f in &decoded.layers[0].features {
            for (x, y) in decode_coords(&f.geometry) {
                assert!(
                    x >= -slack - extent && x <= extent + slack + extent,
                    "x {x} outside buffered tile bounds"
                );
                assert!(
                    y >= -slack - extent && y <= extent + slack + extent,
                    "y {y} outside buffered tile bounds"
                );
            }
        }
    }
}

#[test]
fn bbox_contained_feature_bypasses_clip_with_identical_output() {
    // A concave polygon fully inside one z4 tile (z4 tile width 22.5°; the
    // tile containing lng -100..., lat ~40 spans well past this 2° shape).
    // The fast path must emit the geometry as-is: the encoded tile bytes
    // must equal an MVT built from the *unclipped* geometry.
    let poly = Geometry::Polygon(geo::Polygon::new(
        LineString::from(vec![
            (-100.0, 25.0),
            (-98.0, 25.0),
            (-98.0, 27.0),
            (-99.0, 25.5), // concavity: BooleanOps clip normalizes rings
            (-100.0, 27.0),
            (-100.0, 25.0),
        ]),
        vec![],
    ));
    let feats = vec![Feature {
        geom: poly.clone(),
        props: vec![],
    }];
    let opts = ExportOptions::default();
    let tiles = encode_level_tiles(&feats, 4, &opts).unwrap();
    assert_eq!(tiles.len(), 1, "fully-contained feature => exactly 1 tile");
    assert_eq!(tiles[0].feature_count, 1);

    let tc = TileCoord::new(tiles[0].x, tiles[0].y, 4);
    let member = Member {
        key: tile_key(tiles[0].x, tiles[0].y, 4),
        seq: 0,
        rank: point_rank(0),
        geom: poly.clone().into(),
        props: SharedProps::new(vec![]),
        id: None,
        gap: GAP_NONE,
        band: 0,
    };
    let expected = build_mvt(
        &test_gzip_cache(),
        &[&member],
        &tc.bounds(),
        &opts,
        &[],
        &crate::diagnostics::Diagnostics::default(),
    )
    .data
    .into_raw();
    assert_eq!(
        tiles[0].data, expected,
        "contained feature must bypass the clip (geometry emitted as-is)"
    );
}

#[test]
fn seam_crossing_feature_still_clips() {
    // A line spanning several z4 tiles must still be clipped per tile:
    // it lands in >= 2 tiles and no tile carries the full-extent geometry.
    let line = Geometry::LineString(LineString::from(vec![(-100.0, 10.0), (-40.0, 11.0)]));
    let feats = vec![Feature {
        geom: line,
        props: vec![],
    }];
    let opts = ExportOptions::default();
    let tiles = encode_level_tiles(&feats, 4, &opts).unwrap();
    assert!(tiles.len() >= 2, "seam-crossing line must span tiles");
    let extent = opts.extent as i32;
    // The buffer is in TILE PIXELS; in MVT units that is
    // buffer_px * extent / 256 (128 units for the default 8 px at 4096).
    let slack = opts.tile_buffer as i32 * extent / 256 + 4;
    for t in &tiles {
        let decoded = decode_tile(&t.data);
        for f in &decoded.layers[0].features {
            for (x, y) in decode_coords(&f.geometry) {
                assert!(
                    x >= -slack && x <= extent + slack && y >= -slack && y <= extent + slack,
                    "({x},{y}) outside buffered tile: geometry was not clipped"
                );
            }
        }
    }
}

/// Net polygon area of a decoded MVT feature, in tile units: the
/// surveyor's formula per ring (`MoveTo` starts a ring), outer rings
/// positive and holes negative under MVT's winding rule.
fn mvt_polygon_area(geom: &[u32]) -> f64 {
    let mut area = 0.0;
    let mut ring: Vec<(f64, f64)> = Vec::new();
    let close = |ring: &mut Vec<(f64, f64)>, area: &mut f64| {
        let n = ring.len();
        for k in 0..n {
            let (x0, y0) = ring[k];
            let (x1, y1) = ring[(k + 1) % n];
            *area += (x0 * y1 - x1 * y0) / 2.0;
        }
        ring.clear();
    };
    let (mut cx, mut cy) = (0i64, 0i64);
    let mut i = 0;
    while i < geom.len() {
        let (cmd, count) = crate::mvt::command_decode(geom[i]);
        i += 1;
        if cmd == 7 {
            close(&mut ring, &mut area);
            continue;
        }
        for _ in 0..count {
            if cmd == 1 && !ring.is_empty() {
                close(&mut ring, &mut area);
            }
            cx += i64::from(crate::mvt::zigzag_decode(geom[i]));
            cy += i64::from(crate::mvt::zigzag_decode(geom[i + 1]));
            ring.push((cx as f64, cy as f64));
            i += 2;
        }
    }
    area
}

/// (raw bytes, net polygon area, vertex count, features) of one encoded
/// tile.
fn tile_area_stats(t: &EncodedTile) -> (usize, f64, usize, usize) {
    let tile = decode_tile(&t.data);
    let mut area = 0.0;
    let mut vertices = 0;
    let mut features = 0;
    for layer in &tile.layers {
        for f in &layer.features {
            area += mvt_polygon_area(&f.geometry);
            vertices += decode_coords(&f.geometry).len();
            features += 1;
        }
    }
    (t.data.len(), area.abs(), vertices, features)
}

/// A 30×30 grid of touching squares, all inside tile 10/512/511, with a
/// few properties each: a contiguous polygon coverage in one tile.
fn coverage_grid() -> Vec<Feature> {
    let d = 0.005;
    let mut out = Vec::new();
    for row in 0..30 {
        for col in 0..30 {
            let (x, y) = (0.1 + col as f64 * d, 0.1 + row as f64 * d);
            out.push(Feature {
                geom: Geometry::Polygon(geo::Polygon::new(
                    LineString::from(vec![(x, y), (x + d, y), (x + d, y + d), (x, y + d), (x, y)]),
                    vec![],
                )),
                props: vec![
                    (
                        "name".to_string(),
                        PropertyValue::String(format!("parcel-{row}-{col}")),
                    ),
                    ("row".to_string(), PropertyValue::Int(row)),
                ],
            });
        }
    }
    out
}

/// #745: on a polygon coverage over its size limit, the drop valve
/// leaves holes; every merging valve fits the limit, draws the whole
/// area, and dissolves the shared edges.
#[test]
fn merging_valves_keep_a_coverage_whole() {
    let feats = coverage_grid();
    let reference = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(reference.len(), 1, "the grid sits in one tile");
    let (full_bytes, full_area, full_vertices, full_features) = tile_area_stats(&reference[0]);
    assert_eq!(full_features, 900);
    let limit = full_bytes / 4;

    let run = |valve: SizeValve| {
        let tiles = encode_level_tiles(
            &feats,
            10,
            &ExportOptions {
                tile_size_limit: Some(limit),
                size_valve: valve,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(tiles.len(), 1);
        (tile_area_stats(&tiles[0]), tiles[0].valve)
    };

    let ((_, drop_area, _, _), drop_tally) = run(SizeValve::Drop);
    assert!(drop_tally.dropped > 0 && drop_tally.merged == 0);
    assert!(
        drop_area < 0.5 * full_area,
        "the drop valve should leave holes ({drop_area} of {full_area})"
    );

    {
        let valve = SizeValve::Merge;
        let ((bytes, area, vertices, features), tally) = run(valve);
        assert!(bytes <= limit, "{valve:?}: {bytes} > {limit}");
        assert!(
            (area - full_area).abs() <= 1e-6 * full_area,
            "{valve:?}: drew {area} of {full_area}"
        );
        assert_eq!(tally.dropped, 0, "{valve:?} must not fall back to dropping");
        assert_eq!(tally.merged + features, full_features, "{valve:?}");
        assert!(tally.passes > 1 && tally.mingap > 0, "{valve:?}: {tally:?}");
        assert!(
            vertices < full_vertices / 2,
            "{valve:?}: shared edges should dissolve ({vertices} vs {full_vertices})"
        );
        // A coverage shrinks steadily as it merges, so the valve raises
        // the threshold step by step and never jumps to merging all of
        // it into one feature (#758).
        assert!(features > 1, "{valve:?}: merged everything: {tally:?}");
    }
}

/// #758: a pass that did not shrink the tile, or shrank it too slowly
/// to fit within [`MAX_PROJECTED_PASSES`] more, is a stall; a steady
/// shrink is not.
#[test]
fn stalled_tells_a_slow_shrink_from_a_steady_one() {
    for (prev, fit, want) in [
        (0.5, 0.5, true),   // no shrink
        (0.5, 0.45, true),  // grew
        (0.5, 0.52, true),  // ~17 more passes at this rate
        (0.9, 0.91, true),  // ~9 more
        (0.5, 0.7, false),  // ~1 more
        (0.9, 0.98, false), // well under 1 more
        (0.3, 0.6, false),  // ~0.7 more
    ] {
        assert_eq!(stalled(prev, fit), want, "{prev} -> {fit}");
    }
}

/// #873: lower the extent before shedding merged geometry. Compare
/// area on the same grid, with room for half a coarse unit per axis.
#[test]
fn merge_fallback_reduces_extent_before_dropping_merged_features() {
    let feats = disjoint_rings(200, 240);
    let full = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..Default::default()
        },
    )
    .unwrap();
    let gzip_size = crate::compression::compress(&full[0].data, Compression::Gzip)
        .unwrap()
        .len();
    let tiles = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: Some(gzip_size / 4),
            ..Default::default()
        },
    )
    .unwrap();
    let extent = decode_tile(&tiles[0].data).layers[0].extent.unwrap();
    let (_, area, _, _) = tile_area_stats(&tiles[0]);
    let (_, full_area, _, _) = tile_area_stats(&full[0]);
    assert!((MIN_VALVE_EXTENT..4096).contains(&extent));
    assert_eq!(tiles[0].valve.reduced_extent, Some(extent));
    assert_eq!(tiles[0].valve.dropped, 0);
    assert_eq!(tiles[0].valve.over_limit, 0);
    let normalized = area * (4096.0 / f64::from(extent)).powi(2);
    // The fine-grid radius is about 35 units. Moving its boundary by
    // half a coarse diagonal bounds the area error by perimeter * d.
    let d = std::f64::consts::FRAC_1_SQRT_2 * (4096.0 / f64::from(extent) + 1.0);
    let tolerance = 200.0 * (std::f64::consts::TAU * 36.0 * d + std::f64::consts::PI * d * d);
    assert!(
        (normalized - full_area).abs() <= tolerance,
        "{normalized} vs {full_area}, tolerance {tolerance}"
    );
}

/// Disjoint polygons, `n` of them on a 20-wide grid inside tile
/// 10/512/511, each a ring of `sides` vertices: they never dissolve, so
/// merging saves only their tags.
fn disjoint_rings(n: i64, sides: u32) -> Vec<Feature> {
    (0..n)
        .map(|i| {
            let (cx, cy) = (0.1 + (i % 20) as f64 * 0.01, 0.1 + (i / 20) as f64 * 0.01);
            let ring: Vec<(f64, f64)> = (0..=sides)
                .map(|k| {
                    let a = std::f64::consts::TAU * f64::from(k % sides) / f64::from(sides);
                    (cx + 0.003 * a.cos(), cy + 0.003 * a.sin())
                })
                .collect();
            Feature {
                geom: Geometry::Polygon(geo::Polygon::new(LineString::from(ring), vec![])),
                props: vec![("n".to_string(), PropertyValue::Int(i))],
            }
        })
        .collect()
}

/// A shared dictionary dominates the tile. Proportional cuts alone
/// approach the cap too slowly to finish within eight encodes.
/// #873: a drop-valve tile just over its cap used to shed a sliver per
/// pass and end over the cap after 8 passes; the 0.8 margin finishes it.
#[test]
fn drop_to_fit_margin_finishes_a_stalling_tile() {
    let mut feats = disjoint_rings(400, 4);
    for f in &mut feats {
        f.props = vec![("shared".into(), PropertyValue::String("x".repeat(20_000)))];
    }
    let tiles = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            size_valve: SizeValve::Drop,
            tile_size_limit: Some(21_000),
            compressed_tile_size_limit: None,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(tiles.len(), 1);
    assert!(
        tiles[0].data.len() <= 21_000,
        "{}: {:?}",
        tiles[0].data.len(),
        tiles[0].valve
    );
    assert_eq!(tiles[0].valve.over_limit, 0);
}

/// The merge itself fits; a shared dictionary makes single-drop cuts
/// shrink slowly. The margin must finish before the extent backstop.
#[test]
fn merge_valve_single_drop_margin_finishes_a_stalling_tile() {
    let members: Vec<_> = disjoint_rings(400, 4)
        .into_iter()
        .enumerate()
        .map(|(i, f)| Member {
            key: tile_key(512, 511, 10),
            seq: i as u64,
            rank: point_rank(i as u64),
            geom: f.geom.into(),
            props: SharedProps::new(if i < 2 {
                vec![]
            } else {
                vec![("shared".into(), PropertyValue::String("x".repeat(20_000)))]
            }),
            id: None,
            gap: if i == 1 { 1 } else { GAP_NONE },
            band: 0,
        })
        .collect();
    let opts = ExportOptions {
        size_valve: SizeValve::Merge,
        tile_size_limit: Some(21_000),
        compressed_tile_size_limit: None,
        ..Default::default()
    };
    let mvt = encode_tile(
        &test_gzip_cache(),
        &members,
        &TileCoord::new(512, 511, 10).bounds(),
        &opts,
        &[],
        &mut FillTally::default(),
        &crate::diagnostics::Diagnostics::default(),
    );
    let v = mvt.valve;
    assert_eq!(v.merged, 1, "{v:?}");
    assert!(v.dropped > 0, "{v:?}");
    assert_eq!(v.over_limit, 0, "{v:?}");
    assert!(mvt.data.len() <= 21_000, "{v:?}");
    assert_eq!(v.reduced_extent, None, "single-drop cuts must fit: {v:?}");
    assert_eq!(v.dropped_merged, 0, "{v:?}");
}

/// #768: `smooth` is the default; both values round-trip their spelling.
#[test]
fn valve_scope_defaults_to_smooth_and_parses_its_spelling() {
    assert_eq!(ExportOptions::default().valve_scope, ValveScope::Smooth);
    for scope in [ValveScope::Smooth, ValveScope::Tile, ValveScope::Zoom] {
        assert_eq!(scope.as_str().parse::<ValveScope>(), Ok(scope));
    }
    assert!("neighbours".parse::<ValveScope>().is_err());
}

/// #768: `--valve-max-step` is a ratio above 1 (2 by default); its
/// fade is the reciprocal, and anything else fails before any I/O.
#[test]
fn valve_max_step_is_a_finite_ratio_above_one() {
    assert_eq!(ExportOptions::default().valve_max_step, 2.0);
    for ok in [1.000_1, 1.25, 2.0, 1e9] {
        let opts = ExportOptions::default().with_valve_max_step(ok);
        assert!(opts.check().is_ok(), "{ok}");
    }
    for bad in [1.0, 0.5, 0.0, -2.0, f64::NAN, f64::INFINITY] {
        let err = ExportOptions::default()
            .with_valve_max_step(bad)
            .check()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("--valve-max-step") && err.contains("above 1"),
            "{err}"
        );
    }
    // Only `smooth` fades, so a step under `tile` or `zoom` is refused
    // as ignored, not silently dropped.
    for scope in [ValveScope::Tile, ValveScope::Zoom] {
        let opts = ExportOptions {
            valve_scope: scope,
            ..ExportOptions::default()
        };
        assert!(opts.check().is_ok());
        let err = opts.with_valve_max_step(1.25).check().unwrap_err();
        assert!(err.to_string().contains("valve_max_step"), "{err}");
    }
}

/// #768: `tile` predicts nothing, so every tile is fitted alone as
/// before; `smooth` and `zoom` predict under either valve, with a cap.
/// `zoom` needs the whole input, so a shard refuses it.
#[test]
fn valve_scope_tile_predicts_nothing_and_zoom_refuses_a_shard() {
    for valve in [SizeValve::Merge, SizeValve::Drop] {
        let opts = |scope| ExportOptions {
            size_valve: valve,
            valve_scope: scope,
            ..Default::default()
        };
        assert!(!valve_predicts(&opts(ValveScope::Tile)));
        assert!(valve_predicts(&opts(ValveScope::Smooth)));
        assert!(valve_predicts(&opts(ValveScope::Zoom)));
        let uncapped = ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..opts(ValveScope::Smooth)
        };
        assert!(!valve_predicts(&uncapped));
    }
    let shard = |scope| ExportOptions {
        valve_scope: scope,
        tile_range: Some(crate::shard::TileRange::parse("5..12").unwrap()),
        ..Default::default()
    };
    let err = shard(ValveScope::Zoom).check().unwrap_err();
    assert!(err.to_string().contains("--valve-scope zoom"), "{err}");
    assert!(shard(ValveScope::Smooth).check().is_ok());
}

/// #768: `--valve-scope smooth` starts a tile's merge ladder at its
/// floor, so a tile that fits on its own still merges the gaps under
/// its floor, in one encode; with no floor it merges nothing.
#[test]
fn merge_valve_starts_from_the_tiles_floor() {
    let members: Vec<_> = disjoint_rings(400, 4)
        .into_iter()
        .enumerate()
        .map(|(i, f)| Member {
            key: tile_key(512, 511, 10),
            seq: i as u64,
            rank: point_rank(i as u64),
            geom: f.geom.into(),
            props: SharedProps::new(vec![]),
            id: None,
            gap: if i == 0 { GAP_NONE } else { i as u64 },
            band: 0,
        })
        .collect();
    let opts = ExportOptions {
        size_valve: SizeValve::Merge,
        ..Default::default()
    };
    let floors = valve_predict::ValveFloors {
        merge: valve_predict::Floors {
            tiles: HashMap::from([((10, 512, 511), 101)]),
            ..Default::default()
        },
        ..Default::default()
    };
    let encode = |floors| {
        encode_tile_layers(
            &test_gzip_cache(),
            &members,
            &TileCoord::new(512, 511, 10).bounds(),
            &TileEncoding::single(&opts)
                .with_floors(floors)
                .for_tile(10, 512, 511),
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        )
    };
    let alone = encode(None);
    assert_eq!(
        (alone.valve.merged, alone.valve.passes),
        (0, 1),
        "{:?}",
        alone.valve
    );
    assert!(!alone.oversized);
    let floored = encode(Some(&floors));
    let v = floored.valve;
    // Gaps 1..=100 sit under the floor of 101.
    assert_eq!((v.merged, v.passes), (100, 1), "{v:?}");
    assert_eq!(v.over_limit, 0, "{v:?}");
    // A neighbour's floor merged a tile that fits on its own: not an
    // oversized tile. Its own predicted need would make it one.
    assert!(!floored.oversized, "{v:?}");
    let mut own = floors.clone();
    own.merge.own.insert((10, 512, 511));
    assert!(encode(Some(&own)).oversized);
}

/// #768: a merge floor short of what a binding cap needs is only where
/// the ladder starts: it climbs from there and the tile ends under the
/// cap.
#[test]
fn a_merge_floor_under_a_binding_cap_still_fits() {
    let members: Vec<_> = disjoint_rings(400, 4)
        .into_iter()
        .enumerate()
        .map(|(i, f)| Member {
            key: tile_key(512, 511, 10),
            seq: i as u64,
            rank: point_rank(i as u64),
            geom: f.geom.into(),
            props: SharedProps::new(vec![]),
            id: None,
            gap: if i == 0 { GAP_NONE } else { i as u64 },
            band: 0,
        })
        .collect();
    let floors = valve_predict::ValveFloors {
        merge: valve_predict::Floors {
            tiles: HashMap::from([((10, 512, 511), 101)]),
            ..Default::default()
        },
        ..Default::default()
    };
    let encode = |opts: &ExportOptions, floors| {
        encode_tile_layers(
            &test_gzip_cache(),
            &members,
            &TileCoord::new(512, 511, 10).bounds(),
            &TileEncoding::single(opts)
                .with_floors(floors)
                .for_tile(10, 512, 511),
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        )
    };
    let uncapped = ExportOptions {
        size_valve: SizeValve::Merge,
        tile_size_limit: None,
        compressed_tile_size_limit: None,
        ..Default::default()
    };
    let floored = encode(&uncapped, Some(&floors));
    // A cap the floor alone does not meet.
    let cap = floored.data.len() * 2 / 3;
    let capped = ExportOptions {
        tile_size_limit: Some(cap),
        ..uncapped
    };
    let mvt = encode(&capped, Some(&floors));
    let v = mvt.valve;
    assert!(v.passes > 1 && v.merged > floored.valve.merged, "{v:?}");
    assert_eq!(v.over_limit, 0, "{v:?}");
    assert!(mvt.data.len() <= cap, "{} > {cap}", mvt.data.len());
}

/// #768: a point tile over its cap beside a denser over-cap one is
/// thinned toward it under the default `--valve-scope smooth`: the denser
/// tile's predicted need, halved one ring out, keeps twice the dense
/// tile's share of its points, fewer than its own need would. A tile
/// that fits on its own beside the dense one is thinned toward it too,
/// so the density fades instead of jumping at its edge. Each tile is
/// priced at its own real encode, and the cap binds the pairs' tile
/// without the floor.
#[test]
fn an_over_cap_point_tile_beside_a_denser_one_thins_toward_it() {
    let zooms = [10u8];
    // East of the dense tile (540, 483): 12,544 points on a 0.0012 by
    // 0.001 degree grid, each with a partner 1e-5 degrees after it.
    let pairs: Vec<Option<TileBounds>> = (0..25_088u32)
        .map(|i| {
            let (cell, partner) = (i / 2, i % 2);
            let x = 10.2 + f64::from(cell % 112) * 0.0012 + f64::from(partner) * 1e-5;
            let y = 10.0 + f64::from(cell / 112) * 0.001;
            Some(TileBounds::new(x, y, x, y))
        })
        .collect();
    // West, in (539, 483): 2,000 points that fit on their own.
    let sparse: Vec<Option<TileBounds>> = (0..2_000u32)
        .map(|i| {
            let (x, y) = (
                9.6 + f64::from(i % 50) * 0.004,
                10.0 + f64::from(i / 50) * 0.002,
            );
            Some(TileBounds::new(x, y, x, y))
        })
        .collect();
    let dense = valve_predict::dense_points(70_000);
    let members = |rows: &[Option<TileBounds>], band: u16| -> Vec<Member> {
        rows.iter()
            .enumerate()
            .map(|(i, r)| Member {
                band,
                ..point_member(r.unwrap().lng_min, r.unwrap().lat_min, i as u64)
            })
            .collect()
    };
    let (a, b, west) = ((10, 540, 483), (10, 541, 483), (10, 539, 483));
    let encode =
        |members: &[Member], opts: &ExportOptions, floors, (z, x, y): valve_predict::Key| {
            encode_tile_layers(
                &test_gzip_cache(),
                members,
                &TileCoord::new(x, y, z).bounds(),
                &TileEncoding::single(opts)
                    .with_floors(floors)
                    .for_tile(z, x, y),
                &[],
                &mut FillTally::default(),
                &crate::diagnostics::Diagnostics::default(),
            )
        };
    let uncapped = ExportOptions {
        size_valve: SizeValve::Drop,
        tile_size_limit: None,
        compressed_tile_size_limit: None,
        ..Default::default()
    };
    let on_pairs = members(&pairs, 1);
    // The cap the pairs' tile is over by a third.
    let cap = encode(&on_pairs, &uncapped, None, b).data.len() * 3 / 4;
    let opts = ExportOptions {
        tile_size_limit: Some(cap),
        ..uncapped.clone()
    };
    assert_eq!(opts.valve_scope, ValveScope::Smooth);
    let mut scan = valve_predict::Scan::new(1);
    scan.batch(0, &dense, 0..=0, &zooms, &opts);
    scan.batch(1, &pairs, 0..=0, &zooms, &opts);
    scan.batch(2, &sparse, 0..=0, &zooms, &opts);
    scan.flush(&zooms, &opts);
    // Every priced tile at its own encode's raw and gzipped bytes a
    // member, as `valve_sample_bytes` prices it.
    let cache = test_gzip_cache();
    let mut bytes: HashMap<(usize, u64), MemberBytes> = HashMap::new();
    for (rows, band, (z, x, y)) in [(&dense, 0, a), (&pairs, 1, b), (&sparse, 2, west)] {
        let m = members(rows, band);
        let mut mvt = encode(&m, &uncapped, None, (z, x, y));
        let n = m.len() as f64;
        let gz = mvt.data.gzipped(&cache).unwrap().len() as f64;
        bytes.insert((0, tile_key(x, y, z)), (mvt.data.len() as f64 / n, gz / n));
    }
    let priced: Vec<u64> = scan.priced().iter().map(|t| t.key).collect();
    assert_eq!(priced.len(), 3, "{priced:?}");
    let needs = scan.predict(&zooms, &bytes, &opts);
    let need = &needs.drop;
    assert!(need.contains_key(&a) && need.contains_key(&b), "{need:?}");
    let floors = valve_predict::valve_floors(&needs, &opts, usize::MAX).unwrap();
    let floors_of = |k| floors.drop.get(k);
    // One fade rule: each need halves a ring, onto every tile in reach,
    // until it is back to keep-all.
    let fade = |k: valve_predict::Key, d: i32| {
        need.get(&k)
            .map_or(0, |&n| (n as f64 * 0.5f64.powi(d)) as u64)
    };
    assert_eq!(floors_of(b), need[&a] / 2);
    assert!(floors_of(b) > need[&b], "the denser neighbour binds");
    assert_eq!(
        floors_of(west),
        fade(a, 1),
        "a tile that fits is floored too"
    );
    let two_out = fade(a, 2).max(fade(b, 1));
    assert!(two_out > valve_predict::KEEP_ALL, "{two_out}");
    assert_eq!(floors_of((10, 542, 483)), two_out, "the fade goes on");

    // The over-cap tile, encoded as the export does with its floor: it
    // keeps the share the dense neighbour's halved floor implies, which
    // fits, so nothing more drops.
    let thinned = encode(&on_pairs, &opts, Some(&floors), b);
    let keep = kept_at_floor(&on_pairs, floors_of(b)).len();
    let share = 25_088 * valve_predict::KEEP_ALL / floors_of(b);
    assert!(
        keep.abs_diff(share as usize) < 25_088 / 100,
        "{keep} {share}"
    );
    assert_eq!(thinned.valve.dropped, 25_088 - keep, "{:?}", thinned.valve);
    assert_eq!(thinned.valve.over_limit, 0);
    assert!(thinned.data.len() <= cap);
    assert!(thinned.oversized);
    // The sparse tile fits on its own, yet its floor thins it toward its
    // dense neighbour, before any encode, and it still fits the cap.
    let on_sparse = members(&sparse, 2);
    let west_mvt = encode(&on_sparse, &opts, Some(&floors), west);
    assert_eq!(
        west_mvt.valve.dropped,
        on_sparse.len() - kept_at_floor(&on_sparse, floors_of(west)).len()
    );
    assert!(west_mvt.valve.dropped > 0 && west_mvt.data.len() <= cap);
    assert!(
        !west_mvt.oversized,
        "a tile that fits, thinned only by its neighbour's floor, is not oversized"
    );
    // `tile` sets no floors: the plain drop valve thins it by rank.
    let alone = encode(&on_pairs, &opts, None, b);
    assert!(alone.valve.dropped > 0, "{:?}", alone.valve);
    let plain = drop_members_to_fit(
        &test_gzip_cache(),
        &on_pairs,
        &TileCoord::new(541, 483, 10).bounds(),
        &TileEncoding::single(&opts),
        &[],
        &mut FillTally::default(),
        &crate::diagnostics::Diagnostics::default(),
    );
    assert_eq!(alone.data, plain.data);
}

/// #768: a drop floor keeps its share of a point tile's members by
/// rank, spread evenly over their input order, so a dense cluster stays as much
/// denser than its surroundings as it was: the floor thins the tile, it
/// does not flatten it (a gap floor dropped the cluster first).
#[test]
fn a_drop_floor_keeps_its_share_and_the_tiles_relative_density() {
    // A one-degree cluster of 4,000 points, with 500 spread over the
    // tile before it and 500 after, in file order as a Hilbert sort
    // leaves them.
    let spread = |i: u64| {
        let x = -150.0 + ((i * 7919) % 300) as f64;
        point_member(x, -60.0 + ((i * 104_729) % 120) as f64, i)
    };
    let members: Vec<Member> = (0..500u64)
        .map(spread)
        .chain((500..4_500u64).map(|i| {
            let j = (i - 500) as f64;
            point_member(
                10.0 + (j % 64.0) / 64.0,
                20.0 + (j / 64.0).floor() / 64.0,
                i,
            )
        }))
        .chain((4_500..5_000u64).map(spread))
        .collect();
    let in_cluster = |m: &&&Member| (500..4_500).contains(&m.seq);
    // A quarter: the floor of a neighbour whose need is eight times
    // KEEP_ALL, halved one ring out.
    let kept = kept_at_floor(&members, valve_predict::KEEP_ALL * 8 / 2);
    assert_eq!(kept.len(), 5_000 / 4);
    let share = kept.iter().filter(in_cluster).count() as f64 / kept.len() as f64;
    assert!((share - 0.8).abs() < 0.01, "cluster share {share}, was 0.8");
    // A floor at or under KEEP_ALL keeps every member.
    assert_eq!(
        kept_at_floor(&members, valve_predict::KEEP_ALL).len(),
        5_000
    );
    assert_eq!(kept_at_floor(&members, 1).len(), 5_000);
    // Any floor keeps at least one member, and an empty tile none.
    assert_eq!(kept_at_floor(&members, u64::MAX).len(), 1);
    assert!(kept_at_floor(&[], u64::MAX).is_empty());
}

/// #768: rows a filter or the convert's thinning left with gaps (here
/// every odd row) still keep their floor's share, spread evenly: a floor
/// keeps a count of the tile's best-ranked points, not those below a
/// fixed rank, which every odd row's bit-reversed rank exceeds.
#[test]
fn a_drop_floor_keeps_its_share_of_gapped_rows() {
    let odd: Vec<Member> = (0..10_000u64)
        .map(|i| point_member(i as f64 * 1e-4, 1.0, 2 * i + 1))
        .collect();
    let rows = kept_rows(&kept_at_floor(&odd, share_floor(0.25)));
    assert_eq!(rows.len(), 2_500);
    assert!(
        rows.windows(2).all(|w| w[1] - w[0] == 8),
        "{:?}",
        &rows[..8]
    );
}

/// #768: an overview written before the convert marked its `input_row`
/// column has none of its own, so a column of that name is the user's: a
/// property, published, never a rank. Only the marked column ranks.
#[test]
fn only_the_converts_marked_input_row_column_is_a_rank() {
    for marked in [false, true] {
        let mut row = Field::new(INPUT_ROW_COLUMN, DataType::UInt64, false);
        if marked {
            row = row.with_metadata(
                [(
                    super::super::writer::INPUT_ROW_MARKER.to_string(),
                    "1".into(),
                )]
                .into(),
            );
        }
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            geometry_field(),
            row,
        ]);
        let published = PublishedNames::default();
        let attrs = AttrColumns::new(&schema, 1, &published, None);
        assert_eq!(attrs.input_row.is_some(), marked);
        let props: Vec<String> = property_columns(&schema, 1, &published)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(
            props.contains(&INPUT_ROW_COLUMN.to_string()),
            !marked,
            "{props:?}"
        );
    }
}

/// Points on rows `rows`, one a row along a line, in input order.
fn run_of_points(rows: std::ops::Range<u64>) -> Vec<Member> {
    rows.map(|r| point_member(r as f64 * 1e-4, 1.0, r))
        .collect()
}

fn kept_rows(kept: &[&Member]) -> Vec<u64> {
    kept.iter().map(|m| m.seq).collect()
}

/// The drop floor of a kept `share` (1 / that floor's multiple of
/// `KEEP_ALL`).
fn share_floor(share: f64) -> u64 {
    (valve_predict::KEEP_ALL as f64 / share) as u64
}

/// How many of `want` `got` lacks.
fn lacks(got: &[u64], want: &[u64]) -> usize {
    want.iter().filter(|r| !got.contains(r)).count()
}

/// #768: point thinning keeps points by a rank of their own, not a
/// stride counted from each tile's first member: a point a tile keeps
/// is kept by a child tile that keeps at least the same share, and two
/// tiles at one share keep, between them, what their union would, but
/// for the odd point where a tile's count rounds.
#[test]
fn point_thinning_nests_across_zooms_and_agrees_at_tile_edges() {
    let parent = run_of_points(0..4_096);
    let kept = kept_at_floor(&parent, share_floor(1.0 / 8.0));
    // A child tile holds a run of the parent's rows.
    let child = run_of_points(1_000..2_100);
    let in_child: Vec<u64> = kept_rows(&kept)
        .into_iter()
        .filter(|r| (1_000..2_100).contains(r))
        .collect();
    let at_parents = kept_rows(&kept_at_floor(&child, share_floor(1.0 / 8.0)));
    assert!(
        lacks(&at_parents, &in_child) <= 1,
        "the child at the parent's share keeps the parent's points"
    );
    let deeper = kept_rows(&kept_at_floor(&child, share_floor(0.3)));
    assert!(in_child.iter().all(|r| deeper.contains(r)));
    // Two neighbours at one share keep what the whole run does.
    let mut halves = kept_rows(&kept_at_floor(&run_of_points(0..1_500), share_floor(0.125)));
    halves.extend(kept_rows(&kept_at_floor(
        &run_of_points(1_500..4_096),
        share_floor(0.125),
    )));
    assert!(lacks(&halves, &kept_rows(&kept)) <= 1);
    assert!(halves.len().abs_diff(kept.len()) <= 1);
    // The ladder after the floor keeps nested counts too.
    let few = kept_rows(&select_kept_members(&child, 50));
    let more = kept_rows(&select_kept_members(&child, 200));
    assert_eq!((few.len(), more.len()), (50, 200));
    assert!(few.iter().all(|r| more.contains(r)));
}

/// #768: a rank keeps its points evenly along the input order (the
/// Hilbert curve), from any first row: at a power-of-two share exactly
/// every n-th row, at any other within twice the spacing.
#[test]
fn point_rank_spreads_the_kept_points_evenly() {
    for start in [0u64, 1, 777, 1 << 40] {
        let rows = kept_rows(&kept_at_floor(
            &run_of_points(start..start + 3_000),
            share_floor(0.125),
        ));
        assert!(rows.windows(2).all(|w| w[1] - w[0] == 8), "{start}");
        let rows = kept_rows(&kept_at_floor(
            &run_of_points(start..start + 3_000),
            share_floor(0.3),
        ));
        assert!(rows.windows(2).all(|w| w[1] - w[0] <= 4), "{start}");
        assert!(
            (rows.len() as f64 / 900.0 - 1.0).abs() < 0.02,
            "{}",
            rows.len()
        );
    }
}

proptest::proptest! {
    /// #768: for any run of input rows, a split of it into two tiles and
    /// two shares, the larger share keeps a superset, and the split
    /// halves keep exactly what the run keeps.
    #[test]
    fn point_floors_nest_and_split_cleanly(
        start in 0u64..1 << 50,
        len in 1u64..3_000,
        cut in 0.0f64..1.0,
        low in 0.001f64..1.0,
        more in 0.0f64..1.0,
    ) {
        let high = low + (1.0 - low) * more;
        let run = run_of_points(start..start + len);
        let at = start + (len as f64 * cut) as u64;
        let (left, right) = (run_of_points(start..at), run_of_points(at..start + len));
        let whole = kept_rows(&kept_at_floor(&run, share_floor(low)));
        let wider = kept_rows(&kept_at_floor(&run, share_floor(high)));
        proptest::prop_assert!(whole.iter().all(|r| wider.contains(r)));
        let mut halves = kept_rows(&kept_at_floor(&left, share_floor(low)));
        halves.extend(kept_rows(&kept_at_floor(&right, share_floor(low))));
        // Each half's count rounds on its own, and a half keeps at least
        // one point; the rank's even spread (a van der Corput sequence)
        // bounds where the whole run's cut falls in each half.
        let slack = 2 + (len as f64).log2() as usize;
        proptest::prop_assert!(halves.len() <= whole.len() + 2);
        proptest::prop_assert!(lacks(&halves, &whole) <= slack, "{:?}", (lacks(&halves, &whole), slack));
    }
}

/// #768: a point floor that keeps too many still ends under the cap.
/// The rank ladder finishes the tile from what the floor kept.
#[test]
fn a_point_floor_short_of_the_need_still_keeps_the_cap() {
    let tb = crate::tile::tile_bounds(0, 0, 0);
    let members: Vec<Member> = (0..400u64)
        .map(|i| {
            let x = -170.0 + 0.85 * i as f64 + ((i * 7919) % 13) as f64 * 0.01;
            point_member(x, 0.37 * i as f64 - 70.0, i)
        })
        .collect();
    // Keeps about four fifths of the points.
    let floor = valve_predict::KEEP_ALL * 5 / 4;
    let below = members.len() - kept_at_floor(&members, floor).len();
    assert!((75..=85).contains(&below), "{below}");
    let opts = ExportOptions {
        size_valve: SizeValve::Drop,
        tile_size_limit: None,
        compressed_tile_size_limit: None,
        ..Default::default()
    };
    let floors = valve_predict::ValveFloors {
        drop: valve_predict::Floors {
            tiles: HashMap::from([((0, 0, 0), floor)]),
            ..Default::default()
        },
        ..Default::default()
    };
    let drop_at = |floors: Option<&valve_predict::ValveFloors>, opts: &ExportOptions| {
        encode_tile_layers(
            &test_gzip_cache(),
            &members,
            &tb,
            &TileEncoding::single(opts)
                .with_floors(floors)
                .for_tile(0, 0, 0),
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        )
    };
    let cap = drop_at(None, &opts).data.len() * 6 / 10;
    let capped = ExportOptions {
        tile_size_limit: Some(cap),
        ..opts.clone()
    };
    let mvt = drop_at(Some(&floors), &capped);
    assert_eq!(mvt.valve.over_limit, 0, "{:?}", mvt.valve);
    assert!(mvt.data.len() <= cap);
    assert!(mvt.valve.dropped > below, "{:?}", mvt.valve);
    // Under a looser cap the floor alone fits the tile: it drops exactly
    // the share it does not keep, where the ladder alone would drop fewer.
    let loose = ExportOptions {
        tile_size_limit: Some(cap * 3 / 2),
        ..opts.clone()
    };
    assert_eq!(drop_at(Some(&floors), &loose).valve.dropped, below);
    assert!(drop_at(None, &loose).valve.dropped < below);
    // Under merge a point tile still takes the drop valve, and reads its
    // drop floor; a merge floor does not thin it.
    let merge = ExportOptions {
        size_valve: SizeValve::Merge,
        ..opts
    };
    assert_eq!(drop_at(Some(&floors), &merge).valve.dropped, below);
    let gap = valve_predict::ValveFloors {
        merge: floors.drop.clone(),
        ..Default::default()
    };
    assert_eq!(drop_at(Some(&gap), &merge).valve.dropped, 0);
}

/// #768: the encoder consumes a floor without knowing the scope: under
/// `smooth` and `zoom` alike a point tile that fits without its floor
/// is thinned to it before its first encode (one encode, no trial).
#[test]
fn a_drop_floor_thins_a_tile_that_fits_under_any_scope() {
    let tb = crate::tile::tile_bounds(0, 0, 0);
    let members: Vec<Member> = (0..400u64)
        .map(|i| point_member(-170.0 + 0.85 * i as f64, 0.37 * i as f64 - 70.0, i))
        .collect();
    let floors = valve_predict::ValveFloors {
        drop: valve_predict::Floors {
            tiles: HashMap::from([((0, 0, 0), u64::MAX)]),
            own: [(0, 0, 0)].into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let encode = |scope| {
        let opts = ExportOptions {
            size_valve: SizeValve::Drop,
            valve_scope: scope,
            ..Default::default()
        };
        encode_tile_layers(
            &test_gzip_cache(),
            &members,
            &tb,
            &TileEncoding::single(&opts)
                .with_floors(Some(&floors))
                .for_tile(0, 0, 0),
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        )
    };
    let smooth = encode(ValveScope::Smooth);
    assert_eq!(
        (smooth.valve.dropped, smooth.oversized, smooth.valve.passes),
        (399, true, 1),
        "{:?}",
        smooth.valve
    );
    let zoom = encode(ValveScope::Zoom);
    assert_eq!(zoom.data, smooth.data);
}

/// #873: the margin starts on the second cut, so a tile that fits on
/// its first cut is byte-identical to before.
#[test]
fn drop_to_fit_first_cut_stays_byte_identical() {
    let feats = disjoint_rings(400, 4);
    let opts = ExportOptions {
        size_valve: SizeValve::Drop,
        tile_size_limit: None,
        compressed_tile_size_limit: None,
        ..Default::default()
    };
    let full = encode_level_tiles(&feats, 10, &opts).unwrap();
    let limit = full[0].data.len() * 3 / 4;
    let keep = (feats.len() as f64 * limit as f64 / full[0].data.len() as f64).floor() as usize;
    let first = encode_level_tiles(&feats[..keep], 10, &opts).unwrap();
    assert!(
        first[0].data.len() <= limit,
        "fixture must fit on its first cut"
    );
    let tiles = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: Some(limit),
            ..opts
        },
    )
    .unwrap();
    assert_eq!(tiles[0].valve.passes, 2);
    assert_eq!(tiles[0].data, first[0].data);
}

/// Distinct bands leave multiple merged features at the floor, so the
/// final backstop can shed whole merges. Fill uses the old grid.
#[test]
fn merge_valve_floor_then_drop_accounts_for_sources_and_fill() {
    use crate::diagnostics::{Diagnostics, SourceLossCause};
    let members: Vec<_> = disjoint_rings(80, 4)
        .into_iter()
        .enumerate()
        .map(|(i, f)| Member {
            key: tile_key(512, 511, 10),
            seq: i as u64,
            rank: point_rank(i as u64),
            geom: f.geom.into(),
            props: SharedProps::new(vec![]),
            id: Some(i as u64),
            gap: if i % 2 == 0 { GAP_NONE } else { 1 },
            band: (i / 2) as u16,
        })
        .collect();
    for extent in [4096, MIN_VALVE_EXTENT] {
        let opts = ExportOptions {
            extent,
            feature_id: Some("id".into()),
            tile_size_limit: Some(300),
            compressed_tile_size_limit: None,
            ..Default::default()
        };
        let diagnostics = Diagnostics::default();
        let mut fill = FillTally::default();
        let mvt = encode_tile(
            &test_gzip_cache(),
            &members,
            &TileCoord::new(512, 511, 10).bounds(),
            &opts,
            &[fill_square(78, 0, 0, 100)],
            &mut fill,
            &diagnostics,
        );
        assert!(mvt.data.len() <= 300, "{:?}", mvt.valve);
        assert_eq!(
            mvt.valve.reduced_extent,
            (extent > MIN_VALVE_EXTENT).then_some(MIN_VALVE_EXTENT)
        );
        assert!(mvt.valve.dropped_merged > 0);
        assert_eq!(
            mvt.valve.dropped_merged_sources,
            2 * mvt.valve.dropped_merged
        );
        assert_eq!(mvt.valve.dropped, mvt.valve.dropped_merged_sources);
        assert_eq!(fill.shed, 1);
        assert_eq!(fill.polygons, 0);
        let tile = decode_tile(&mvt.data);
        assert_eq!(tile.layers[0].extent, Some(MIN_VALVE_EXTENT));
        let ids: Vec<_> = tile.layers[0]
            .features
            .iter()
            .map(|f| f.id.unwrap())
            .collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "draw order: {ids:?}");
        account_sources(&members, &mvt, 10, &opts, &diagnostics).unwrap();
        let losses = diagnostics.source_losses().unwrap();
        let dropped = losses
            .iter()
            .map(Result::unwrap)
            .filter(|l| l.cause == SourceLossCause::ValveDropped)
            .count();
        assert_eq!(dropped, mvt.valve.dropped_merged_sources);
    }
}

/// Read disjoint exterior rings for the reduced-grid property test.
fn valve_test_polygons(commands: &[u32]) -> geo::MultiPolygon<f64> {
    let mut out = Vec::new();
    let mut ring = Vec::new();
    let (mut x, mut y) = (0i32, 0i32);
    let mut i = 0;
    while i < commands.len() {
        let (cmd, count) = command_decode(commands[i]);
        i += 1;
        if cmd == 7 {
            ring.push(ring[0]);
            out.push(geo::Polygon::new(
                LineString::from(std::mem::take(&mut ring)),
                vec![],
            ));
            continue;
        }
        assert!(cmd == 1 || cmd == 2);
        for _ in 0..count {
            x += zigzag_decode(commands[i]);
            y += zigzag_decode(commands[i + 1]);
            i += 2;
            ring.push((f64::from(x), f64::from(y)));
        }
    }
    geo::MultiPolygon(out)
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(32))]
    #[test]
    fn merge_valve_reduced_extent_preserves_valid_polygon_area(
        sides in 120u32..260,
        dx in -0.0005f64..0.0005,
        dy in -0.0005f64..0.0005,
    ) {
        use geo::{Area, MapCoords, Validation};
        let mut feats = disjoint_rings(120, sides);
        for f in &mut feats {
            f.geom = f.geom.map_coords(|c| geo::Coord { x: c.x + dx, y: c.y + dy });
        }
        let opts = ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..Default::default()
        };
        let full = encode_level_tiles(&feats, 10, &opts).unwrap();
        let limit = crate::compression::compress(&full[0].data, Compression::Gzip).unwrap().len() / 2;
        let tiles = encode_level_tiles(&feats, 10, &ExportOptions {
            compressed_tile_size_limit: Some(limit), ..opts
        }).unwrap();
        proptest::prop_assert_eq!(tiles[0].valve.dropped, 0);
        proptest::prop_assert_eq!(tiles[0].valve.over_limit, 0);
        // Merging alone can save half the gzip bytes (identical rings
        // compress well): then the tile fits at full extent, as it must.
        if tiles[0].valve.reduced_extent.is_none() {
            let gz = crate::compression::compress(&tiles[0].data, Compression::Gzip).unwrap();
            proptest::prop_assert!(gz.len() <= limit);
            return Ok(());
        }
        let layer = &decode_tile(&tiles[0].data).layers[0];
        let extent = layer.extent.unwrap();
        proptest::prop_assert!(extent < 4096);
        let mut area = 0.0;
        for f in &layer.features {
            let polygons = valve_test_polygons(&f.geometry);
            proptest::prop_assert!(polygons.is_valid(), "{:?}", polygons.validation_errors());
            area += polygons.unsigned_area();
        }
        let full_area = tile_area_stats(&full[0]).1;
        let scale = 4096.0 / f64::from(extent);
        // Two independent roundings, each within half a grid diagonal.
        let d = std::f64::consts::FRAC_1_SQRT_2 * (scale + 1.0);
        let tolerance = 120.0 * (std::f64::consts::TAU * 36.0 * d + std::f64::consts::PI * d * d);
        proptest::prop_assert!((area * scale * scale - full_area).abs() <= tolerance);
    }
}

/// #873: a lone merged feature is kept over the cap at the extent floor.
/// #873 review: the merge fallback drops every unmerged feature before it
/// lowers the extent or drops merged coverage. Keeping the last single
/// (here a line with a huge tag) made the backstop drop the merged
/// squares, which were tiny, and write the line alone.
#[test]
fn merge_valve_drops_the_last_unmerged_feature_before_merged_coverage() {
    let mut feats = disjoint_rings(2, 4);
    feats.push(Feature {
        geom: Geometry::LineString(LineString::from(
            (0..100)
                .map(|k| (0.1 + k as f64 * 0.001, 0.2 + (k % 2) as f64 * 0.001))
                .collect::<Vec<_>>(),
        )),
        props: vec![("big".to_string(), PropertyValue::String("x".repeat(20_000)))],
    });
    let tiles = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: Some(1_000),
            compressed_tile_size_limit: None,
            size_valve: SizeValve::Merge,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(tiles.len(), 1);
    let (bytes, area, _, features) = tile_area_stats(&tiles[0]);
    let v = tiles[0].valve;
    assert!(bytes <= 1_000, "{bytes}: {v:?}");
    assert_eq!(v.over_limit, 0, "{v:?}");
    assert!(features >= 1 && area > 0.0, "the squares must stay: {v:?}");
    assert_eq!(v.dropped_merged, 0, "{v:?}");
    assert_eq!(v.reduced_extent, None, "{v:?}");
}

/// #873 review: when the eight proportional cuts leave unmerged features
/// in a tile that holds merged ones, the rest go too before the extent
/// drops or merged coverage is touched. 400 multipoints sharing a 20 KB tag
/// used to outlast the cuts and outrank the merged squares by vertices.
#[test]
fn merge_valve_drops_singles_left_after_the_cuts_before_merged_coverage() {
    let mut feats = disjoint_rings(2, 4);
    let big = "x".repeat(20_000);
    feats.extend((0..400).map(|_| {
        Feature {
            geom: Geometry::MultiPoint(geo::MultiPoint(
                (0..12)
                    .map(|k| geo::Point::new(0.1 + k as f64 * 0.001, 0.2))
                    .collect(),
            )),
            props: vec![("big".to_string(), PropertyValue::String(big.clone()))],
        }
    }));
    let tiles = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: Some(19_900),
            compressed_tile_size_limit: None,
            size_valve: SizeValve::Merge,
            ..Default::default()
        },
    )
    .unwrap();
    let v = tiles[0].valve;
    let (bytes, area, _, _) = tile_area_stats(&tiles[0]);
    assert!(bytes <= 19_900, "{bytes}: {v:?}");
    assert_eq!(v.over_limit, 0, "{v:?}");
    assert_eq!(v.dropped_merged, 0, "{v:?}");
    assert_eq!(v.reduced_extent, None, "{v:?}");
    assert!(area > 0.0, "the squares must stay: {v:?}");
}

/// #873 review: a tile of points (a MultiPoint has many coordinates, so
/// it reaches the merge valve) is never re-encoded at a coarser extent:
/// that moves points and saves almost nothing. It is handled as the drop
/// valve handles it.
#[test]
fn merge_valve_never_lowers_the_extent_of_a_point_tile() {
    let feats = vec![Feature {
        geom: Geometry::MultiPoint(geo::MultiPoint(
            (0..3_000)
                .map(|k| {
                    geo::Point::new(0.1 + (k % 60) as f64 * 0.003, 0.1 + (k / 60) as f64 * 0.003)
                })
                .collect(),
        )),
        props: vec![],
    }];
    let encode = |size_valve| {
        encode_level_tiles(
            &feats,
            10,
            &ExportOptions {
                tile_size_limit: Some(1_000),
                compressed_tile_size_limit: None,
                size_valve,
                ..Default::default()
            },
        )
        .unwrap()
    };
    let (merge, drop) = (encode(SizeValve::Merge), encode(SizeValve::Drop));
    assert_eq!(merge[0].valve.reduced_extent, None, "{:?}", merge[0].valve);
    assert_eq!(merge[0].data, drop[0].data);
}

#[test]
fn merge_valve_writes_a_lone_merged_feature_over_the_floor_and_warns() {
    let feats = disjoint_rings(400, 4);
    let full = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..Default::default()
        },
    )
    .unwrap();
    let limit = full[0].data.len() / 20;
    let tiles = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: Some(limit),
            compressed_tile_size_limit: None,
            size_valve: SizeValve::Merge,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(tiles.len(), 1);
    let (bytes, area, _, features) = tile_area_stats(&tiles[0]);
    let v = tiles[0].valve;
    assert!(v.merged > 0, "the valve should merge first: {v:?}");
    assert!(bytes > limit, "fixture must exceed the cap: {v:?}");
    assert_eq!(v.over_limit, 1, "{v:?}");
    assert!(v.over_limit_merged, "{v:?}");
    assert_eq!(v.reduced_extent, Some(MIN_VALVE_EXTENT));
    assert_eq!(decode_tile(&tiles[0].data).layers[0].extent, Some(512));
    assert_eq!(features, 1);
    assert!(area > 0.0, "the tile must still draw: {v:?}");

    let report = export_valve_fixture(&feats, limit);
    assert_eq!(report.over_limit_tiles, 1);
    let warning = report
        .diagnostics
        .iter()
        .find(|d| matches!(d, crate::diagnostics::Diagnostic::TilesOverLimit { .. }))
        .unwrap();
    let crate::diagnostics::Diagnostic::TilesOverLimit {
        count,
        largest,
        merged_tiles,
    } = warning
    else {
        unreachable!()
    };
    assert_eq!(*count, 1);
    assert_eq!(*merged_tiles, 1);
    assert_eq!(largest.len(), 1);
    assert_eq!((largest[0].z, largest[0].x, largest[0].y), (10, 512, 511));
    let line = warning.to_string();
    assert!(line.contains("z10/512/511"), "{line}");
    assert!(
        line.contains("1 of them hold a single merged feature over the cap at the extent floor"),
        "{line}"
    );
}

/// Run the real writer/report/diagnostics path on one valve tile at z10.
fn export_valve_fixture(feats: &[Feature], limit: usize) -> ExportReport {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let schema = Arc::new(source_schema());
    let mut writer = OverviewWriter::create(
        tin.path(),
        &schema,
        OverviewWriterOptions::new(Mode::Duplicating, vec![LevelSpec::new(gsd(10), Some(10))]),
    )
    .unwrap();
    let ids: Vec<_> = (0..feats.len() as i64).collect();
    let geoms: Vec<_> = feats.iter().map(|f| f.geom.clone()).collect();
    assert_eq!(
        writer
            .write_level(
                0,
                Some(feats.len()),
                std::iter::once(batch(&schema, &ids, &geoms))
            )
            .unwrap(),
        LevelWriteOutcome::Written
    );
    writer.finish().unwrap();
    let tout = tempfile::NamedTempFile::new().unwrap();
    export_pmtiles(
        tin.path(),
        tout.path(),
        &ExportOptions {
            size_valve: SizeValve::Merge,
            tile_size_limit: Some(limit),
            compressed_tile_size_limit: None,
            ..Default::default()
        },
    )
    .unwrap()
}

/// The unmerged line goes before the merged squares (#873 review), so the
/// lone survivor over the cap is the merge, and the warning counts it as one.
/// The opposite case, a lone unmerged survivor, is
/// `merge_valve_lone_unmerged_feature_skips_single_drop_encode`.
#[test]
fn merge_valve_over_limit_keeps_the_merged_survivor_over_a_single() {
    let mut feats = disjoint_rings(2, 4);
    feats.push(Feature {
        geom: Geometry::LineString(LineString::from(
            (0..200)
                .map(|k| {
                    (
                        0.1 + k as f64 * 0.001,
                        0.25 + if k % 2 == 0 { 0.01 } else { 0.0 },
                    )
                })
                .collect::<Vec<_>>(),
        )),
        props: vec![],
    });
    let report = export_valve_fixture(&feats, 1);
    assert!(report.merged_features > 0);
    assert_eq!(report.valve_dropped_features, 1, "the line is dropped");
    assert_eq!(report.over_limit_tiles, 1);
    assert_eq!(report.zooms[0].tile_feature_count, 1);
    assert_eq!(report.zooms[0].dropped_merged_features, 0);
    let warning = report
        .diagnostics
        .iter()
        .find(|d| matches!(d, crate::diagnostics::Diagnostic::TilesOverLimit { .. }))
        .unwrap();
    let crate::diagnostics::Diagnostic::TilesOverLimit { merged_tiles, .. } = warning else {
        unreachable!()
    };
    assert_eq!(*merged_tiles, 1, "{warning}");
    assert!(warning.to_string().contains("single merged feature"));
}

#[test]
fn merge_valve_lone_unmerged_feature_skips_single_drop_encode() {
    let tiles = encode_level_tiles(
        &disjoint_rings(1, 4),
        10,
        &ExportOptions {
            size_valve: SizeValve::Merge,
            tile_size_limit: Some(1),
            compressed_tile_size_limit: None,
            ..Default::default()
        },
    )
    .unwrap();
    let v = tiles[0].valve;
    assert_eq!(v.merged, 0);
    assert_eq!(v.dropped, 0);
    assert_eq!(v.over_limit, 1);
    assert!(!v.over_limit_merged);
    assert_eq!(v.reduced_extent, Some(512));
    // Initial encode, then 2048, 1024, 512; no unchanged single-drop encode.
    assert_eq!(v.passes, 4, "{v:?}");
}

/// #873, the shape of the FTW Brazil z8 and Overture São Paulo z12
/// tiles: detailed disjoint polygons under the default compressed cap
/// alone. Fully merged, their vertices still exceed the cap.
#[test]
fn merge_valve_fits_detailed_polygons_under_the_compressed_cap() {
    let feats = disjoint_rings(200, 240);
    let gz = |t: &EncodedTile| {
        crate::compression::compress(&t.data, Compression::Gzip)
            .unwrap()
            .len()
    };
    let full = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..Default::default()
        },
    )
    .unwrap();
    let limit = gz(&full[0]) / 4;
    let tiles = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: Some(limit),
            size_valve: SizeValve::Merge,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(tiles.len(), 1);
    let (_, area, _, features) = tile_area_stats(&tiles[0]);
    let v = tiles[0].valve;
    let bytes = gz(&tiles[0]);
    assert!(v.merged > 0, "the valve should merge first: {v:?}");
    assert!(
        bytes <= limit,
        "written over the cap: {bytes} > {limit}: {v:?}"
    );
    assert_eq!(v.over_limit, 0, "{v:?}");
    assert!(
        features >= 1 && area > 0.0,
        "the tile must still draw: {v:?}"
    );
}

/// #758: merging features that do not touch saves only their tags, so
/// on a tile of detailed, disjoint polygons each pass barely shrinks the
/// tile. Once a pass shows the tile cannot fit within a few more at that
/// rate, the valve merges everything it can in one step instead of
/// raising the threshold pass after pass (it took 13 passes, each
/// re-encoding the whole tile, on an FTW Moldova z6 tile that never fit).
#[test]
fn a_stalled_merge_jumps_to_merging_everything() {
    let feats: Vec<Feature> = (0..200)
        .map(|i| {
            let (cx, cy) = (0.1 + (i % 20) as f64 * 0.01, 0.1 + (i / 20) as f64 * 0.01);
            let ring: Vec<(f64, f64)> = (0..=240)
                .map(|k| {
                    let a = std::f64::consts::TAU * f64::from(k % 240) / 240.0;
                    (cx + 0.003 * a.cos(), cy + 0.003 * a.sin())
                })
                .collect();
            Feature {
                geom: Geometry::Polygon(geo::Polygon::new(LineString::from(ring), vec![])),
                props: vec![("n".to_string(), PropertyValue::Int(i))],
            }
        })
        .collect();
    let full = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..Default::default()
        },
    )
    .unwrap();
    let (full_bytes, full_area, _, _) = tile_area_stats(&full[0]);
    let tiles = encode_level_tiles(
        &feats,
        10,
        &ExportOptions {
            tile_size_limit: Some(full_bytes / 2),
            size_valve: SizeValve::Merge,
            ..Default::default()
        },
    )
    .unwrap();
    let (_, area, _, features) = tile_area_stats(&tiles[0]);
    let v = tiles[0].valve;
    // Two passes that barely shrink it, then one that merges everything.
    let extent = decode_tile(&tiles[0].data).layers[0].extent.unwrap();
    let detail_passes = 4096u32.ilog2() - extent.ilog2();
    assert!(
        v.passes - detail_passes <= 4,
        "the merge should stop inching: {v:?}"
    );
    assert_eq!((features, v.merged), (1, 199), "everything merges: {v:?}");
    let scale = 4096.0 / f64::from(extent);
    let d = std::f64::consts::FRAC_1_SQRT_2 * (scale + 1.0);
    let tolerance = 200.0 * (std::f64::consts::TAU * 36.0 * d + std::f64::consts::PI * d * d);
    assert!(
        (area * scale * scale - full_area).abs() <= tolerance,
        "drew {area} at extent {extent}, fine area {full_area}"
    );
}

/// #745: a merging valve leaves a tile that fits exactly as the drop
/// valve would: nothing merges, nothing is re-encoded.
#[test]
fn merging_valve_leaves_fitting_tiles_alone() {
    let feats = coverage_grid();
    let encode = |valve: SizeValve| {
        encode_level_tiles(
            &feats,
            10,
            &ExportOptions {
                size_valve: valve,
                ..Default::default()
            },
        )
        .unwrap()
    };
    let (drop, tile) = (encode(SizeValve::Drop), encode(SizeValve::Merge));
    assert_eq!(drop[0].data, tile[0].data);
    assert_eq!(tile[0].valve.merged, 0);
    assert_eq!(decode_tile(&tile[0].data).layers[0].extent, Some(4096));
    assert_eq!(tile[0].valve.reduced_extent, None);
    // Both valves count the one encode a fitting tile took.
    assert_eq!(tile[0].valve.passes, 1);
    assert_eq!(drop[0].valve, tile[0].valve);
}

/// #745: the compressed limit is measured on the gzip bytes, and both
/// valves, which re-measure, bring the tile under it.
#[test]
fn compressed_limit_is_enforced() {
    let feats = coverage_grid();
    let reference = encode_members_for_test(&feats, None, SizeValve::Drop);
    let limit = reference.len() / 3;
    let dropped = encode_members_for_test(&feats, Some(limit), SizeValve::Drop);
    assert!(dropped.len() <= limit, "drop: {} > {limit}", dropped.len());
    let merged = encode_members_for_test(&feats, Some(limit), SizeValve::Merge);
    assert!(merged.len() <= limit, "{} > {limit}", merged.len());
}

/// The gzip bytes of the grid's one tile, with no raw limit.
fn encode_members_for_test(
    feats: &[Feature],
    compressed_limit: Option<usize>,
    valve: SizeValve,
) -> Vec<u8> {
    let opts = ExportOptions {
        tile_size_limit: None,
        compressed_tile_size_limit: compressed_limit,
        size_valve: valve,
        ..Default::default()
    };
    let mut members = Vec::new();
    let mut gaps = RowGapCursor::new(vec![(0, feats.len() as u64)]);
    for (i, f) in feats.iter().enumerate() {
        let (gap, band) = gaps.next_row(f.geom.bounding_rect());
        let props: SharedProps = SharedProps::new(
            f.props
                .iter()
                .map(|(n, v)| (Arc::from(n.as_str()), v.clone()))
                .collect(),
        );
        for (key, geom) in feature_tile_members(
            &f.geom,
            10,
            &opts,
            0,
            u64::MAX,
            &crate::diagnostics::Diagnostics::default(),
        ) {
            members.push(Member {
                key,
                seq: i as u64,
                rank: point_rank(i as u64),
                geom: geom.into(),
                props: props.clone(),
                id: None,
                gap,
                band,
            });
        }
    }
    let tiles = encode_members(
        &test_gzip_cache(),
        members,
        10,
        &opts,
        None,
        &ExportTimers::default(),
        &crate::diagnostics::Diagnostics::default(),
    )
    .unwrap();
    assert_eq!(tiles.len(), 1);
    tiles.into_iter().next().unwrap().data
}

#[test]
fn oversized_valve_fires_with_tiny_limit() {
    // Many vertices in one tile; a tiny byte limit forces the single-pass
    // drop valve.
    let mut coords = Vec::new();
    for k in 0..200 {
        coords.push((-100.0 + k as f64 * 0.001, 40.0 + (k as f64 * 0.01).sin()));
    }
    let line = Geometry::LineString(LineString::from(coords));
    let tmp = tempfile::NamedTempFile::new().unwrap();
    // Put many copies of the line in one level so a single tile is heavy.
    let geoms: Vec<Geometry<f64>> = (0..20).map(|_| line.clone()).collect();
    let ids: Vec<i64> = (0..20).collect();
    write_fixture(
        tmp.path(),
        &[(ids.clone(), geoms.clone()), (ids.clone(), geoms.clone())],
    );
    let reader = OverviewReader::open(tmp.path()).unwrap();
    let feats = read_level_features(&reader, 1, Crs::Epsg4326).unwrap();

    // No limit (both caps off explicitly, since both are on by default):
    // no oversized tiles.
    let no_limit = ExportOptions {
        tile_size_limit: None,
        compressed_tile_size_limit: None,
        ..Default::default()
    };
    let none = encode_level_tiles(&feats, 6, &no_limit).unwrap();
    assert!(none.iter().all(|t| !t.oversized));
    let full_count: usize = none.iter().map(|t| t.feature_count).sum();

    // Tiny limit: valve fires and drops features.
    let opts = ExportOptions {
        tile_size_limit: Some(64),
        ..Default::default()
    };
    let limited = encode_level_tiles(&feats, 6, &opts).unwrap();
    assert!(
        limited.iter().any(|t| t.oversized),
        "tiny --tile-size-limit must trip the valve"
    );
    let limited_count: usize = limited.iter().map(|t| t.feature_count).sum();
    assert!(
        limited_count < full_count,
        "valve must drop features ({limited_count} !< {full_count})"
    );
}

/// #814: tiles the valve leaves over the limit, a lone feature included,
/// are counted on the report and summed in one diagnostic.
#[test]
fn tiles_left_over_the_limit_are_counted_in_the_report() {
    let coords: Vec<_> = (0..200)
        .map(|k| (-100.0 + k as f64 * 0.001, 40.0 + (k as f64 * 0.01).sin()))
        .collect();
    let line = Geometry::LineString(LineString::from(coords));
    let tin = tempfile::NamedTempFile::new().unwrap();
    let ids: Vec<i64> = (0..20).collect();
    write_fixture(
        tin.path(),
        &[(vec![0], vec![line.clone()]), (ids, vec![line; 20])],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        tile_size_limit: Some(1),
        ..Default::default()
    };
    let report = export_pmtiles(tin.path(), tout.path(), &opts).unwrap();
    assert_eq!(
        report.over_limit_tiles, report.total_tiles,
        "no tile with a feature fits in one byte"
    );
    let summary: Vec<_> = report
        .diagnostics
        .iter()
        .filter(|d| matches!(d, crate::diagnostics::Diagnostic::TilesOverLimit { .. }))
        .collect();
    assert_eq!(summary.len(), 1);
    assert_eq!(summary[0].count(), report.over_limit_tiles);
    // #873: only tiles with a remaining merged feature count toward
    // that explanation in the warning; the lone line's tiles do not.
    let crate::diagnostics::Diagnostic::TilesOverLimit { merged_tiles, .. } = summary[0] else {
        unreachable!()
    };
    assert!(
        (1..report.over_limit_tiles).contains(merged_tiles),
        "{merged_tiles} of {}",
        report.over_limit_tiles
    );
    let reduced: usize = report
        .diagnostics
        .iter()
        .filter_map(|d| {
            if let crate::diagnostics::Diagnostic::ExtentReduced {
                count, min_extent, ..
            } = d
            {
                assert_eq!(*min_extent, MIN_VALVE_EXTENT);
                Some(*count)
            } else {
                None
            }
        })
        .sum();
    assert!(reduced > 0);
    assert_eq!(
        reduced,
        report
            .zooms
            .iter()
            .map(|z| z.reduced_extent_tiles)
            .sum::<usize>()
    );
}

/// #814: `max_zoom_simplify_factor` thins the finest zoom's tiles, keeps
/// every feature there, and leaves coarser zooms alone, in both pass-2
/// engines. A feature under the tolerance keeps its own geometry.
#[test]
fn max_zoom_simplify_factor_thins_only_the_finest_zoom() {
    let wiggly = |x0: f64, r: f64| {
        let ring: Vec<(f64, f64)> = (0..400)
            .map(|k| {
                let a = f64::from(k) / 400.0 * std::f64::consts::TAU;
                let r = r * (1.0 + 0.002 * (f64::from(k) * 7.0).sin());
                (x0 + r * a.cos(), 0.5 + r * a.sin())
            })
            .collect();
        Geometry::Polygon(geo::Polygon::new(LineString::from(ring), vec![]))
    };
    let (big, tiny) = (wiggly(0.0, 5.0), wiggly(20.0, 1e-4));
    for mode in [Mode::Duplicating, Mode::Partitioning] {
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_mode_fixture(
            tin.path(),
            &[
                (vec![0], vec![big.clone()]),
                (vec![0, 1], vec![big.clone(), tiny.clone()]),
            ],
            mode,
            2,
        );
        let run = |factor: Option<f64>| {
            let tout = tempfile::NamedTempFile::new().unwrap();
            let opts = ExportOptions {
                max_zoom_simplify_factor: factor,
                ..Default::default()
            };
            let report = export_pmtiles(tin.path(), tout.path(), &opts).unwrap();
            (report, stored_tiles(&std::fs::read(tout.path()).unwrap()))
        };
        let (r0, t0) = run(None);
        let (r1, t1) = run(Some(1.0));
        let finest = r0.max_zoom;
        for (z0, z1) in r0.zooms.iter().zip(&r1.zooms) {
            assert_eq!(z0.tile_feature_count, z1.tile_feature_count, "{mode:?}");
        }
        assert_eq!(t0.keys().collect::<Vec<_>>(), t1.keys().collect::<Vec<_>>());
        let mut thinned = 0;
        for (id, b0) in &t0 {
            let z = crate::pmtiles_writer::tile_id_to_zxy(*id).unwrap().0;
            let (raw0, raw1) = (
                compression::decompress(b0, compression::Compression::Gzip).unwrap(),
                compression::decompress(&t1[id], compression::Compression::Gzip).unwrap(),
            );
            if z < finest {
                assert_eq!(raw0, raw1, "{mode:?}: z{z} must not change");
            } else if raw1.len() < raw0.len() {
                thinned += 1;
            }
        }
        assert!(thinned > 0, "{mode:?}: no finest-zoom tile was thinned");
    }
}

/// Max-zoom simplification only removes vertices: a hole under the
/// tolerance keeps its original ring, in a polygon and in a
/// multipolygon part, while the exterior still simplifies.
#[test]
fn max_zoom_simplify_keeps_holes_under_the_tolerance() {
    use geo::{CoordsIter, MultiPolygon};
    let mut ring: Vec<(f64, f64)> = (0..200)
        .map(|k| (f64::from(k) * 1e-5, if k % 2 == 0 { 1e-6 } else { -1e-6 }))
        .collect();
    ring.extend([(2e-3, 1e-3), (0.0, 1e-3), (0.0, 1e-6)]);
    // A 0.3 m hole, under a quarter GSD at z14 (about 0.6 m).
    let hole = LineString::from(vec![
        (1e-3, 5e-4),
        (1e-3 + 2e-6, 5e-4),
        (1e-3 + 2e-6, 5e-4 + 2e-6),
        (1e-3, 5e-4 + 2e-6),
        (1e-3, 5e-4),
    ]);
    let poly = geo::Polygon::new(LineString::from(ring), vec![hole.clone()]);
    let explicit = ExportOptions {
        max_zoom_simplify_factor: Some(0.25),
        ..Default::default()
    };
    let diagnostics = crate::diagnostics::Diagnostics::default();
    let check = |out: &geo::Polygon<f64>| {
        assert!(
            out.exterior().0.len() < poly.exterior().0.len(),
            "the exterior simplifies"
        );
        assert_eq!(out.interiors(), std::slice::from_ref(&hole));
    };
    let single = Geometry::Polygon(poly.clone());
    let out = finest_zoom_geometry(&single, Some((14, None)), &explicit, &diagnostics);
    let Geometry::Polygon(p) = out.as_ref() else {
        panic!("{out:?}")
    };
    check(p);
    let multi = Geometry::MultiPolygon(MultiPolygon::new(vec![poly.clone()]));
    let out = finest_zoom_geometry(&multi, Some((14, None)), &explicit, &diagnostics);
    let Geometry::MultiPolygon(m) = out.as_ref() else {
        panic!("{out:?}")
    };
    assert_eq!(m.0.len(), 1);
    check(&m.0[0]);
    assert!(out.coords_count() < multi.coords_count());
}

/// A part under the tolerance keeps its own geometry, as a whole feature
/// under it does: dropping it would lose a piece that can still span
/// distinct tile coordinates. Lines under the default, and polygon parts
/// under an explicit factor.
#[test]
fn max_zoom_simplify_keeps_parts_under_the_tolerance() {
    use geo::{CoordsIter, MultiLineString, MultiPolygon};
    // A 0.1 m zigzag that simplifies, beside a 0.3 m part, both under a
    // quarter GSD at z14 (about 0.6 m).
    let zig: Vec<(f64, f64)> = (0..200)
        .map(|k| (f64::from(k) * 1e-5, if k % 2 == 0 { 1e-6 } else { -1e-6 }))
        .collect();
    let short = LineString::from(vec![(0.01, 0.01), (0.01 + 3e-6, 0.01)]);
    let lines = Geometry::MultiLineString(MultiLineString::new(vec![
        LineString::from(zig.clone()),
        short.clone(),
    ]));
    let mut ring = zig;
    ring.extend([(2e-3, 1e-3), (0.0, 1e-3), (0.0, 1e-6)]);
    let tiny = geo::Polygon::new(
        LineString::from(vec![
            (0.01, 0.01),
            (0.01 + 2e-6, 0.01),
            (0.01, 0.01 + 2e-6),
            (0.01, 0.01),
        ]),
        vec![],
    );
    let polys = Geometry::MultiPolygon(MultiPolygon::new(vec![
        geo::Polygon::new(LineString::from(ring), vec![]),
        tiny.clone(),
    ]));
    let diagnostics = crate::diagnostics::Diagnostics::default();
    let out = finest_zoom_geometry(
        &lines,
        Some((14, None)),
        &ExportOptions::default(),
        &diagnostics,
    );
    let Geometry::MultiLineString(m) = out.as_ref() else {
        panic!("{out:?}")
    };
    assert!(
        out.coords_count() < lines.coords_count(),
        "the zigzag simplifies"
    );
    assert_eq!(m.0.len(), 2);
    assert_eq!(m.0[1], short);
    let explicit = ExportOptions {
        max_zoom_simplify_factor: Some(0.25),
        ..Default::default()
    };
    let out = finest_zoom_geometry(&polys, Some((14, None)), &explicit, &diagnostics);
    let Geometry::MultiPolygon(m) = out.as_ref() else {
        panic!("{out:?}")
    };
    assert!(
        out.coords_count() < polys.coords_count(),
        "the zigzag simplifies"
    );
    assert_eq!(m.0.len(), 2);
    assert_eq!(m.0[1], tiny);
}

/// Unset, `max_zoom_simplify_factor` simplifies lines at the finest zoom
/// at [`DEFAULT_LINE_MAX_ZOOM_SIMPLIFY_FACTOR`] and leaves polygons alone;
/// an explicit factor applies to both, `0` to neither, and so does
/// [`ExportOptions::verbatim`].
#[test]
fn max_zoom_simplify_defaults_to_lines_only() {
    use geo::CoordsIter;
    // A 0.1 m zigzag, under a quarter GSD at z14 (about 0.6 m).
    let zig: Vec<(f64, f64)> = (0..200)
        .map(|k| (f64::from(k) * 1e-5, if k % 2 == 0 { 1e-6 } else { -1e-6 }))
        .collect();
    let line = Geometry::LineString(LineString::from(zig.clone()));
    let mut ring = zig;
    ring.extend([(2e-3, 1e-3), (0.0, 1e-3), (0.0, 1e-6)]);
    let poly = Geometry::Polygon(geo::Polygon::new(LineString::from(ring), vec![]));
    let diagnostics = crate::diagnostics::Diagnostics::default();
    let kept = |g: &Geometry<f64>, opts: &ExportOptions| {
        let out = finest_zoom_geometry(g, Some((14, None)), opts, &diagnostics);
        out.coords_count() == g.coords_count()
    };
    let with = |f| ExportOptions {
        max_zoom_simplify_factor: f,
        ..Default::default()
    };
    assert!(!kept(&line, &ExportOptions::default()), "default line");
    assert!(kept(&poly, &ExportOptions::default()), "default polygon");
    assert_eq!(
        finest_zoom_geometry(
            &line,
            Some((14, None)),
            &ExportOptions::default(),
            &diagnostics
        ),
        finest_zoom_geometry(&line, Some((14, None)), &with(Some(0.25)), &diagnostics),
        "the default line factor is 0.25"
    );
    assert!(!kept(&line, &with(Some(0.25))) && !kept(&poly, &with(Some(0.25))));
    assert!(kept(&line, &with(Some(0.0))) && kept(&poly, &with(Some(0.0))));
    let verbatim = ExportOptions::default().verbatim();
    assert!(kept(&line, &verbatim) && kept(&poly, &verbatim));
    // Coarser levels are convert's to simplify.
    assert!(matches!(
        finest_zoom_geometry(&line, None, &ExportOptions::default(), &diagnostics),
        std::borrow::Cow::Borrowed(_)
    ));
}

/// #814: a variable-depth stop draws canonical geometry at an ancestor's
/// zoom, simplified at the canonical zoom's tolerance, not the ancestor's.
/// The wiggle sits between the two tolerances (0.022° and 0.088°).
#[test]
fn variable_depth_stops_simplify_at_the_canonical_zoom() {
    let ring: Vec<(f64, f64)> = (0..400)
        .map(|k| {
            let a = f64::from(k) / 400.0 * std::f64::consts::TAU;
            let r = 5.0 + if k % 2 == 0 { 0.03 } else { -0.03 };
            (r * a.cos(), 20.0 + r * a.sin())
        })
        .collect();
    let poly = Geometry::Polygon(geo::Polygon::new(LineString::from(ring), vec![]));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[(vec![0], vec![poly.clone()]), (vec![0], vec![poly])],
    );
    let vertices = |factor: Option<f64>| {
        let tout = tempfile::NamedTempFile::new().unwrap();
        let opts = ExportOptions {
            variable_depth: true,
            max_zoom_simplify_factor: factor,
            ..Default::default()
        };
        let report = export_pmtiles(tin.path(), tout.path(), &opts).unwrap();
        assert!(report.zooms[0].stopped_tiles > 0, "no tile stopped at z2");
        stored_tiles(&std::fs::read(tout.path()).unwrap())
            .values()
            .map(|b| {
                let raw = compression::decompress(b, compression::Compression::Gzip).unwrap();
                decode_tile(&raw).layers[0]
                    .features
                    .iter()
                    .map(|f| decode_coords(&f.geometry).len())
                    .sum::<usize>()
            })
            .sum::<usize>()
    };
    let (kept, simplified) = (vertices(None), vertices(Some(1.0)));
    assert!(
        simplified > kept / 2,
        "stopped tile lost the canonical detail: {simplified} of {kept} vertices"
    );
}

/// #814: an empty carrier (every member quantized away) is not a written
/// tile, so it never counts as over the limit.
#[test]
fn an_all_quantized_tile_is_not_counted_over_the_limit() {
    let speck = Geometry::Polygon(geo::Polygon::new(
        LineString::from(vec![(0.0, 0.0), (1e-7, 0.0), (1e-7, 1e-7), (0.0, 0.0)]),
        vec![],
    ));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[(vec![0], vec![speck.clone()]), (vec![0], vec![speck])],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        tile_size_limit: Some(1),
        ..Default::default()
    };
    let report = export_pmtiles(tin.path(), tout.path(), &opts).unwrap();
    assert!(report.encode_quantized_features > 0);
    assert_eq!(report.total_tiles, 0);
    assert_eq!(report.over_limit_tiles, 0);
    assert!(!report
        .diagnostics
        .iter()
        .any(|d| matches!(d, crate::diagnostics::Diagnostic::TilesOverLimit { .. })));
}

#[test]
fn max_zoom_simplify_factor_must_be_finite_and_non_negative() {
    let with = |f| ExportOptions {
        max_zoom_simplify_factor: Some(f),
        ..Default::default()
    };
    for bad in [-1.0, f64::NAN, f64::INFINITY] {
        assert!(
            matches!(with(bad).validate(), Err(ConfigError::InvalidSpec { .. })),
            "{bad}"
        );
    }
    assert!(with(0.0).validate().is_ok());
}

/// #280, #761: the per-tile caps are on by default: tippecanoe's 500 KiB
/// (== what `500K` parses to) on the gzip-compressed tile, and a 2 MiB
/// backstop on the raw one.
#[test]
fn default_export_caps_tiles_at_500_kib_compressed_and_2_mib_raw() {
    assert_eq!(DEFAULT_COMPRESSED_TILE_SIZE_LIMIT, 500 * 1024);
    assert_eq!(DEFAULT_TILE_SIZE_LIMIT, 2048 * 1024);
    let opts = ExportOptions::default();
    assert_eq!(
        opts.compressed_tile_size_limit,
        Some(DEFAULT_COMPRESSED_TILE_SIZE_LIMIT)
    );
    assert_eq!(opts.tile_size_limit, Some(DEFAULT_TILE_SIZE_LIMIT));
}

/// Build a bare point [`Member`] at `(x, y)` for the drop-valve unit tests.
pub(super) fn point_member(x: f64, y: f64, seq: u64) -> Member {
    Member {
        key: 0,
        seq,
        rank: point_rank(seq),
        geom: Geometry::Point(Point::new(x, y)).into(),
        props: SharedProps::new(Vec::new()),
        id: None,
        gap: GAP_NONE,
        band: 0,
    }
}

#[test]
fn joined_wave_prices_original_members_even_when_source_valve_output_is_small() {
    let input = tempfile::NamedTempFile::new().unwrap();
    let output = tempfile::NamedTempFile::new().unwrap();
    let points = vec![Geometry::Point(Point::new(0.0, 0.0)); 4096];
    write_fixture(input.path(), &[(vec![0; points.len()], points)]);
    let opts = ExportOptions {
        partition_wave: 8,
        tile_size_limit: Some(1),
        compressed_tile_size_limit: None,
        ..Default::default()
    };
    let (_, members) =
        export_pmtiles_for_join((input.path(), None), output.path(), &opts, &[2]).unwrap();
    let tiles = stored_tiles(&std::fs::read(output.path()).unwrap());
    let raw = compression::decompress(
        tiles.values().next().unwrap(),
        compression::Compression::Gzip,
    )
    .unwrap();
    let joined = [raw.as_slice(), raw.as_slice()].concat();
    assert!(joined.len() < 1024, "the source valves leave a tiny join");
    assert_eq!(decode_tile(&joined).layers.len(), 2);
    let source = JointSource {
        members,
        path: input.path().to_path_buf(),
        options: opts.clone(),
        min_zoom: 2,
        max_zoom: 2,
    };
    let fitter = JoinedTileFitter::open([&source, &source]).unwrap();
    let pending = BTreeSet::from([*tiles.keys().next().unwrap()]);
    assert_eq!(
        source.members.tile_counts[tiles.keys().next().unwrap()],
        4096
    );
    assert_eq!(
        fitter.tile_wave_under(
            &pending,
            &opts,
            &MemoryBudget::with_available(Some(4 << 20))
        ),
        1,
        "price the original overview members collected by the retry"
    );
}

#[test]
fn joined_refit_keeps_the_source_exports_shared_border_pins() {
    // Two squares sharing a jagged border: its inner vertices are pins.
    let border = [(1.0, 0.0), (1.1, 0.25), (0.9, 0.5), (1.1, 0.75), (1.0, 1.0)];
    let left: Vec<_> = [(0.0, 0.0)]
        .into_iter()
        .chain(border)
        .chain([(0.0, 1.0), (0.0, 0.0)])
        .collect();
    let right: Vec<_> = border
        .into_iter()
        .chain([(2.0, 1.0), (2.0, 0.0), (1.0, 0.0)])
        .collect();
    let squares: Vec<Geometry<f64>> = [left, right]
        .into_iter()
        .map(|ring| Geometry::Polygon(geo::Polygon::new(geo::LineString::from(ring), vec![])))
        .collect();
    let input = tempfile::NamedTempFile::new().unwrap();
    write_fixture(input.path(), &[(vec![0; squares.len()], squares)]);
    let source = |shared_nodes| JointSource {
        members: JointMembers {
            tile_counts: BTreeMap::new(),
            member_bytes: None,
        },
        path: input.path().to_path_buf(),
        options: ExportOptions {
            max_zoom_simplify_factor: Some(1.0),
            ..Default::default()
        }
        .with_shared_nodes(shared_nodes),
        min_zoom: 0,
        max_zoom: 0,
    };
    let (on, off) = (source(true), source(false));
    let fitter = JoinedTileFitter::open([&on, &off]).unwrap();
    assert!(
        fitter.sources[0].pins.is_some(),
        "the flag's pins reach the refit"
    );
    assert!(fitter.sources[1].pins.is_none(), "no pins without the flag");
}

#[test]
fn joined_refit_skips_a_join_whose_members_exceed_memory() {
    let per_member = member_resident_bytes(600, 4);
    let layer = JointMembers {
        tile_counts: BTreeMap::from([(1, 100), (2, 1_000_000)]),
        member_bytes: Some(per_member),
    };
    let pending = BTreeSet::from([1, 2]);
    let both = [&layer, &layer];
    // Each layer's share of tile 2 fits; the two together do not.
    let ram = limit(1_500_000 * per_member, MemoryLimitSource::MemAvailable);
    assert_eq!(
        joined_tiles_within(&pending, both.into_iter(), ram),
        BTreeSet::from([1])
    );
    let roomy = limit(2_000_000 * per_member, MemoryLimitSource::MemAvailable);
    assert_eq!(
        joined_tiles_within(&pending, both.into_iter(), roomy),
        pending
    );
}

#[test]
fn joined_wave_uses_the_densest_tile_and_run_profile() {
    let budget = MemoryBudget::with_available(Some(256 << 20));
    let mut opts = ExportOptions {
        partition_wave: 8,
        ..Default::default()
    };
    let pending = BTreeSet::from([1, 2]);
    let small = JointMembers {
        tile_counts: BTreeMap::from([(1, 100), (2, 100), (3, 6_500_000)]),
        member_bytes: Some(member_resident_bytes(600, 4)),
    };
    let dense = JointMembers {
        tile_counts: BTreeMap::from([(1, 100), (2, 6_500_000)]),
        ..small.clone()
    };
    for profile in [MemoryProfile::Auto, MemoryProfile::Bounded] {
        opts.profile = profile;
        assert_eq!(
            joined_tile_wave_under(&pending, [&small].into_iter(), &opts, &budget),
            8
        );
        assert_eq!(
            joined_tile_wave_under(&pending, [&dense].into_iter(), &opts, &budget),
            1
        );
    }
    opts.profile = MemoryProfile::Speed;
    assert_eq!(
        joined_tile_wave_under(&pending, [&dense].into_iter(), &opts, &budget),
        8
    );
    opts.profile = MemoryProfile::Auto;
    opts.partition_wave = PARTITION_WAVE_AUTO;
    assert_eq!(
        joined_tile_wave_under(&pending, [&dense].into_iter(), &opts, &budget),
        1
    );
}

#[test]
fn joined_wave_sums_layers_for_each_pending_tile() {
    let budget = MemoryBudget::with_available(Some(256 << 20));
    let opts = ExportOptions {
        partition_wave: 8,
        ..Default::default()
    };
    let members = JointMembers {
        tile_counts: BTreeMap::from([(1, 20_000), (2, 100)]),
        member_bytes: Some(member_resident_bytes(600, 4)),
    };
    let pending = BTreeSet::from([1, 2]);
    let one = joined_tile_wave_under(&pending, [&members].into_iter(), &opts, &budget);
    assert!(one > 1, "one source fits concurrent tiles");
    assert_eq!(
        joined_tile_wave_under(&pending, [&members, &members].into_iter(), &opts, &budget),
        1
    );
    let disjoint = JointMembers {
        tile_counts: BTreeMap::from([(2, 20_000)]),
        ..members.clone()
    };
    assert_eq!(
        joined_tile_wave_under(&pending, [&members, &disjoint].into_iter(), &opts, &budget),
        one,
        "sum sources within a tile, not the whole zoom"
    );
    let overflow = JointMembers {
        tile_counts: BTreeMap::from([(1, usize::MAX)]),
        member_bytes: Some(u64::MAX),
    };
    assert_eq!(
        joined_tile_wave_under(&pending, [&overflow, &overflow].into_iter(), &opts, &budget),
        1
    );
}

#[test]
fn joined_layer_encoder_groups_members_once() {
    let members: Vec<_> = (0..32)
        .map(|i| Member {
            band: (i % 4) as u16,
            id: Some(i),
            ..point_member(i as f64, 0.0, i)
        })
        .collect();
    let layers = [
        (0, "a".into()),
        (1, "b".into()),
        (2, "a".into()),
        (3, "b".into()),
        (4, "empty".into()),
    ];
    let cache = test_gzip_cache();
    let opts = ExportOptions::default();
    let bounds = TileCoord::new(0, 0, 0).bounds();
    let diagnostics = crate::diagnostics::Diagnostics::silent();
    JOINT_BAND_READS.with(|n| n.set(0));
    let actual = build_tile_layers(&cache, &layers, &members, &bounds, &opts, &[], &diagnostics);
    assert_eq!(JOINT_BAND_READS.with(std::cell::Cell::get), members.len());
    let mut expected = Vec::new();
    for (band, name) in [(0, "a"), (1, "b")] {
        let selected: Vec<_> = members.iter().filter(|m| m.band % 2 == band).collect();
        let opts = ExportOptions {
            layer_name: name.into(),
            ..opts.clone()
        };
        expected.extend_from_slice(
            &build_mvt(&cache, &selected, &bounds, &opts, &[], &diagnostics).data,
        );
    }
    assert_eq!(
        &*actual.data,
        expected.as_slice(),
        "preserve layer and feature order"
    );
}

/// A wave's buckets hold exactly their members: grown by pushes they
/// held up to twice their member structs at the export's peak.
#[test]
fn wave_buckets_hold_exactly_their_members() {
    let wave: Vec<Partition> = [(0, 9), (10, 19), (20, 29)]
        .into_iter()
        .map(|(key_lo, key_hi)| Partition {
            key_lo,
            key_hi,
            bbox: PruneRegion::of(TileBounds::new(0.0, 0.0, 0.0, 0.0)),
            members: 0,
        })
        .collect();
    let members = (0..29)
        .map(|i| Member {
            key: i,
            ..point_member(0.0, 0.0, i)
        })
        .collect();
    let buckets = bucket_members(members, &wave);
    assert_eq!(
        buckets.iter().map(Vec::len).collect::<Vec<_>>(),
        [10, 10, 9]
    );
    assert!(buckets.iter().all(|b| b.capacity() == b.len()));
}

// --- #235: partitioning single-read fan-out pass 2 -----------------------

/// Round-trip exactness of the member spill codec: key, seq, every
/// [`PropertyValue`] variant, and every [`Geometry`] variant, verified
/// bit-exactly (via re-encoded bytes, since `PartialEq` would call
/// `-0.0 == 0.0` equal and hide a sign flip through the spill).
#[test]
fn member_spill_codec_roundtrip_is_exact() {
    use geo::{
        coord, GeometryCollection, Line, MultiLineString, MultiPoint, MultiPolygon, Rect, Triangle,
    };
    let poly = geo::Polygon::new(
        LineString::from(vec![
            (0.0, 0.0),
            (4.0, 0.0),
            (4.0, 4.0),
            (0.0, 4.0),
            (0.0, 0.0),
        ]),
        vec![LineString::from(vec![
            (1.0, 1.0),
            (2.0, 1.0),
            (2.0, 2.0),
            (1.0, 2.0),
            (1.0, 1.0),
        ])],
    );
    let geoms: Vec<Geometry<f64>> = vec![
        Geometry::Point(Point::new(-0.0, std::f64::consts::PI)),
        Geometry::Line(Line::new(
            coord! { x: -1.5, y: 2.5 },
            coord! { x: 3.25, y: -4.75 },
        )),
        Geometry::LineString(LineString::from(vec![(1.1, 2.2), (3.3, 4.4)])),
        Geometry::Polygon(poly.clone()),
        Geometry::MultiPoint(MultiPoint::from(vec![
            Point::new(1.0, 2.0),
            Point::new(-3.0, -4.0),
        ])),
        Geometry::MultiLineString(MultiLineString::new(vec![LineString::from(vec![
            (0.0, 1.0),
            (2.0, 3.0),
        ])])),
        Geometry::MultiPolygon(MultiPolygon::new(vec![poly.clone()])),
        Geometry::GeometryCollection(GeometryCollection::from(vec![
            Geometry::Point(Point::new(9.0, 9.0)),
            Geometry::Polygon(poly),
        ])),
        Geometry::Rect(Rect::new(
            coord! { x: 0.0, y: 0.0 },
            coord! { x: 1.0, y: 1.0 },
        )),
        Geometry::Triangle(Triangle::new(
            coord! { x: 0.0, y: 0.0 },
            coord! { x: 1.0, y: 0.0 },
            coord! { x: 0.0, y: 1.0 },
        )),
    ];
    let props: Vec<(Arc<str>, PropertyValue)> = vec![
        ("s".into(), PropertyValue::String("héllo".to_string())),
        ("f".into(), PropertyValue::Float(-0.0f32)),
        ("d".into(), PropertyValue::Double(f64::MIN_POSITIVE)),
        ("i".into(), PropertyValue::Int(-42)),
        ("u".into(), PropertyValue::UInt(u64::MAX)),
        ("b".into(), PropertyValue::Bool(true)),
    ];
    for (i, g) in geoms.into_iter().enumerate() {
        // #443: every other member carries a `--feature-id` value, so the
        // roundtrip covers both `Some` and `None` through the spill wire
        // format.
        let id = (i % 2 == 0).then_some(u64::MAX - 7 - i as u64);
        let m = Member {
            key: tile_key(7, 11, 5) + i as u64,
            seq: u64::MAX - i as u64,
            rank: point_rank(u64::MAX - i as u64),
            geom: g.into(),
            props: SharedProps::new(props.clone()),
            id,
            // #745: both the "no predecessor" sentinel and real values.
            gap: if i % 3 == 0 {
                GAP_NONE
            } else {
                12_345 * i as u64
            },
            band: i as u16,
        };
        let mut buf = Vec::new();
        let mut names = SpillNames::default();
        names.register(m.props.names());
        encode_member(&mut buf, &m, &names);
        let mut cur = SpillCursor::new(&buf);
        let back = decode_member(&mut cur, &names).unwrap();
        assert!(cur.is_empty(), "trailing bytes after decode (geometry {i})");
        assert_eq!(back.key, m.key);
        assert_eq!(back.seq, m.seq);
        assert_eq!((back.gap, back.band), (m.gap, m.band));
        assert_eq!(
            back.id, m.id,
            "feature id must survive the spill (geometry {i})"
        );
        assert_eq!(back.props, m.props);
        let mut buf2 = Vec::new();
        encode_member(&mut buf2, &back, &names);
        assert_eq!(buf, buf2, "re-encoded member bytes differ (geometry {i})");
    }
}

#[test]
fn member_spill_rejects_invalid_utf8() {
    let member = Member {
        key: 0,
        seq: 0,
        rank: point_rank(0),
        geom: Geometry::Point(Point::new(0.0, 0.0)).into(),
        props: SharedProps::new(vec![(
            "text".into(),
            PropertyValue::String("utf8-marker".into()),
        )]),
        id: None,
        gap: GAP_NONE,
        band: 0,
    };
    let mut names = SpillNames::default();
    names.register(member.props.names());
    let mut bytes = Vec::new();
    encode_member(&mut bytes, &member, &names);
    let offset = bytes
        .windows(b"utf8-marker".len())
        .position(|w| w == b"utf8-marker")
        .unwrap();
    bytes[offset] = 0xff;
    assert!(decode_member(&mut SpillCursor::new(&bytes), &names).is_err());
}

#[test]
fn spill_names_keep_the_registered_table_handle() {
    // `id` runs once per spilled member: a hit on the caller's own
    // `Arc` must not fall back to comparing every name.
    let table: PropertyNames = Arc::from(vec![Arc::<str>::from("a"), Arc::from("b")]);
    let mut names = SpillNames::default();
    names.register(&table);
    names.register(&table);
    assert_eq!(names.tables.len(), 1);
    assert!(Arc::ptr_eq(&names.tables[0], &table));
    let equal: PropertyNames = Arc::from(vec![Arc::<str>::from("a"), Arc::from("b")]);
    assert_eq!(names.id(&equal), 0, "equal content shares the id");
}

#[test]
fn spill_segment_with_hostile_counts_is_an_error() {
    let names = SpillNames::default();
    // A member count no buffer could hold must fail, not allocate or panic.
    for count in [u64::MAX, 1 << 40, 3] {
        let buf = count.to_le_bytes();
        assert!(decode_segment(&buf, &names).is_err(), "count {count}");
    }
    // A property table id that was never registered.
    let mut cur = Vec::new();
    for v in [0u64, 0, 0] {
        cur.extend_from_slice(&v.to_le_bytes());
    }
    cur.extend_from_slice(&0u32.to_le_bytes()); // band
    cur.push(0); // no feature id
    cur.extend_from_slice(&7u32.to_le_bytes());
    cur.extend_from_slice(&0u32.to_le_bytes());
    assert!(decode_member(&mut SpillCursor::new(&cur), &names).is_err());
}

/// #427: a `spill_dir` that is not an existing directory fails the
/// export before any tile is written, with a typed error.
#[test]
fn missing_spill_dir_is_rejected_up_front() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.parquet");
    write_fixture(
        &input,
        &[(vec![1], vec![Geometry::Point(Point::new(0.5, 0.5))])],
    );
    let out = dir.path().join("out.pmtiles");
    let opts = ExportOptions {
        spill_dir: Some(dir.path().join("no-such-dir")),
        ..ExportOptions::default()
    };
    let err = export_pmtiles(&input, &out, &opts).unwrap_err();
    assert!(
        matches!(
            err,
            ExportError::Config(ConfigError::SpillDirNotDirectory(_))
        ),
        "{err}"
    );
    assert!(
        err.to_string().contains("not an existing directory"),
        "{err}"
    );
    assert!(!out.exists(), "nothing written");
}

/// #427: the member spill file lives under `ExportOptions::spill_dir`
/// when one is given, and is removed with the store.
#[test]
fn member_store_spill_lives_under_spill_dir() {
    let dir = tempfile::tempdir().unwrap();
    let plans = vec![LevelPlan::cut(
        4,
        partitions_with_members(&[10]),
        1,
        usize::MAX,
    )];
    let store = MemberStore::new(
        &plans,
        SinkBacking::Spill,
        MEMBER_STORE_RAM_BUDGET,
        Some(dir.path()),
    )
    .unwrap();
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        1,
        "one spill file under the spill dir"
    );
    drop(store);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "spill file removed"
    );
}

/// #815: `bounded` plans a level that takes several waves in a quarter
/// of the partition target, so each wave holds a quarter of the members;
/// a one-wave level, and `auto`, keep the target.
#[test]
fn bounded_multi_wave_levels_hold_a_quarter_of_the_members_per_wave() {
    let plan = |tiles: u64, profile| {
        let counts: BTreeMap<u64, usize> = (0..tiles)
            .map(|t| (tile_key(u32::try_from(t).unwrap(), 0, 10), 1_000))
            .collect();
        plan_level(
            &counts,
            10,
            &PartitionBudget {
                partition_target: 8_000,
                ceiling_wave: 4,
                auto_wave: false,
                member_bytes: None,
                fit_member_bytes: None,
                profile,
                budget: MemoryBudget::with_available(None),
                fit_limit: None,
            },
            String::new,
        )
    };
    let wave_members = |p: &LevelPlan| {
        p.waves()
            .map(|w| w.iter().map(|p| p.members).sum::<usize>())
            .max()
            .unwrap()
    };
    // 64 tiles of 1,000 members: 8 partitions, two waves of four.
    let auto = plan(64, MemoryProfile::Auto);
    assert_eq!(wave_members(&auto), 32_000);
    assert_eq!(wave_members(&plan(64, MemoryProfile::Bounded)), 8_000);
    // 32 tiles: one wave, kept whole.
    assert_eq!(plan(32, MemoryProfile::Bounded).partitions.len(), 4);
}

/// Write a partitioning fixture of `level_geoms` to `path` and fill the
/// single-read member store from it, every level one wave.
fn fill_partitioning_fixture(
    path: &Path,
    level_geoms: &[(Vec<i64>, Vec<Geometry<f64>>)],
) -> MemberStore {
    write_partitioning_fixture(path, level_geoms);
    fill_store(path, MemoryProfile::Auto)
}

/// Fill the single-read member store from the overview at `path` under
/// `profile`, every level one wave.
fn fill_store(path: &Path, profile: MemoryProfile) -> MemberStore {
    let reader = OverviewReader::open(path).unwrap();
    let meta = reader.meta().clone();
    let opts = ExportOptions::default();
    let diagnostics = crate::diagnostics::Diagnostics::default();
    let scans = scan_all_levels(
        &reader,
        test_band(reader.source()),
        Crs::Epsg4326,
        &opts,
        &ExportTimers::default(),
        None,
        None,
        &diagnostics,
    )
    .unwrap();
    let plans: Vec<LevelPlan> = scans
        .iter()
        .enumerate()
        .map(|(k, scan)| {
            let zoom = zoom_for_level(&meta, k).unwrap();
            let partitions = plan_partitions(&scan.tile_counts, zoom, DEFAULT_PARTITION_TARGET);
            let wave = partitions.len().max(1);
            LevelPlan::cut(zoom, partitions, wave, usize::MAX)
        })
        .collect();
    fill_member_store(
        &reader,
        &FanoutCtx {
            band: BandRead {
                profile,
                ..test_band(reader.source())
            },
            crs: Crs::Epsg4326,
            plans: &plans,
            opts: &opts,
            published: &PublishedNames::from_reader(&reader),
            feature_id: None,
            timers: &ExportTimers::default(),
            pins: None,
        },
        StoreBacking {
            backing: SinkBacking::Ram,
            ram_budget: MEMBER_STORE_RAM_BUDGET,
            resident: 0,
        },
        &diagnostics,
    )
    .unwrap()
}

/// The single-read fill reads each band with the ordered parallel reader
/// under `auto` and with one reader under `bounded` (#833: it read
/// through a second, serial reader).
#[test]
fn fill_reads_bands_with_the_ordered_reader() {
    let input = tempfile::NamedTempFile::new().unwrap();
    let points: Vec<Geometry<f64>> = (0..40_000)
        .map(|i| {
            let f = f64::from(i);
            Geometry::Point(Point::new(
                -170.0 + (f * 0.0171) % 340.0,
                -60.0 + (f * 0.0067) % 120.0,
            ))
        })
        .collect();
    write_mode_fixture(
        input.path(),
        &[((0..40_000).collect(), points)],
        Mode::Partitioning,
        10_000,
    );
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(64)
        .build()
        .unwrap();
    let readers = |profile| {
        pool.install(|| {
            BAND_READ_WORKERS.with(|w| w.set(0));
            fill_store(input.path(), profile);
            BAND_READ_WORKERS.with(std::cell::Cell::get)
        })
    };
    // The auto reader count follows the host's cores: one on one CPU.
    let several = resolve_pass_read_workers(ReadPass::Build, READ_WORKERS_AUTO) > 1;
    let auto = readers(MemoryProfile::Auto);
    assert_eq!(auto > 1, several, "auto filled with {auto} reader(s)");
    assert_eq!(
        readers(MemoryProfile::Bounded),
        1,
        "bounded keeps one reader"
    );
}

/// A row of a partitioning fill that yields no member builds no
/// properties: only the rows with members do (Codex C2).
#[test]
fn partitioning_fill_builds_properties_only_for_rows_with_members() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let square = Geometry::Polygon(geo::Polygon::new(
        LineString::from(vec![(10.3, 10.3), (10.4, 10.3), (10.4, 10.4), (10.3, 10.3)]),
        vec![],
    ));
    let empty = Geometry::MultiPolygon(geo::MultiPolygon::new(vec![]));
    let before = PROPS_BUILT.with(std::cell::Cell::get);
    let store = fill_partitioning_fixture(
        tmp.path(),
        &[(vec![0, 1, 2], vec![empty.clone(), square, empty])],
    );
    let rows: HashSet<u64> = store
        .buckets
        .iter()
        .flatten()
        .flatten()
        .map(|m| m.seq)
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(PROPS_BUILT.with(std::cell::Cell::get) - before, 1);
}

/// #815: in a partitioning fill a feature that no level clips is held
/// once. Every level's member of it shares the decoded geometry, where
/// each level held its own copy (the copies were most of a Moldova
/// partitioning export's peak heap).
#[test]
fn partitioning_fill_holds_an_unclipped_feature_once() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let square = |x: f64, y: f64| {
        Geometry::Polygon(geo::Polygon::new(
            LineString::from(vec![(x, y), (x + 0.1, y), (x + 0.1, y + 0.1), (x, y)]),
            vec![],
        ))
    };
    // Squares well inside one tile at every level's zoom (z2, z4, z6).
    let store = fill_partitioning_fixture(
        tmp.path(),
        &[
            (vec![0, 1], vec![square(-100.3, 40.3), square(10.3, 10.3)]),
            (vec![2], vec![square(50.3, -20.3)]),
            (vec![3], vec![square(-60.3, 5.3)]),
        ],
    );
    let members: Vec<&Member> = store.buckets.iter().flatten().flatten().collect();
    // At least one member per feature and level it renders at.
    assert!(members.len() >= 2 + 3 + 4, "{}", members.len());
    let held: HashSet<*const Geometry<f64>> = members
        .iter()
        .map(|m| std::ptr::from_ref::<Geometry<f64>>(&m.geom))
        .collect();
    assert_eq!(held.len(), 4, "one geometry per feature");
}

#[test]
fn member_store_ram_wave_reuses_its_allocation() {
    let plans = vec![LevelPlan::cut(
        4,
        partitions_with_members(&[10]),
        1,
        usize::MAX,
    )];
    for backing in [SinkBacking::Ram, SinkBacking::Spill] {
        let mut store = MemberStore::new(&plans, backing, MEMBER_STORE_RAM_BUDGET, None).unwrap();
        for seq in 0..3 {
            store
                .push(
                    0,
                    0,
                    Member {
                        key: 0,
                        seq,
                        rank: point_rank(seq),
                        geom: Geometry::Point(Point::new(seq as f64, -1.0)).into(),
                        props: SharedProps::new(vec![]),
                        id: None,
                        gap: GAP_NONE,
                        band: 0,
                    },
                )
                .unwrap();
        }
        let allocation = store.buckets[0][0].as_ptr();
        let members = store.take_wave(0, 0).unwrap();
        assert_eq!(store.buckets[0][0].capacity(), 0, "drained bucket released");
        assert_eq!(members.as_ptr(), allocation, "no second wave allocation");
        assert_eq!(members.iter().map(|m| m.seq).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!(store.buffered_bytes, 0);
        assert_eq!(store.bucket_bytes[0][0], 0);
        assert!(store.take_wave(0, 0).unwrap().is_empty());
    }
}

/// The member store's spill path: bucket flushes append segments to one
/// temp file; `take_wave` reads a wave's segments back in append order
/// (plus any RAM remainder), returning exactly the members pushed to it.
#[test]
fn member_store_spill_segments_roundtrip() {
    // One level, two single-partition waves (wave width 1).
    let plans = vec![LevelPlan::cut(
        4,
        partitions_with_members(&[10, 10]),
        1,
        usize::MAX,
    )];
    let mut store =
        MemberStore::new(&plans, SinkBacking::Spill, MEMBER_STORE_RAM_BUDGET, None).unwrap();
    let mk = |key: u64, seq: u64| Member {
        key,
        seq,
        rank: point_rank(seq),
        geom: Geometry::Point(Point::new(seq as f64, -1.0)).into(),
        props: SharedProps::new(vec![("id".into(), PropertyValue::Int(seq as i64))]),
        id: None,
        gap: GAP_NONE,
        band: 0,
    };
    store.push(0, 0, mk(0, 0)).unwrap();
    store.push(0, 1, mk(1, 1)).unwrap();
    // Force a mid-fill flush so wave 0 spans two spill segments.
    store.flush_bucket(0, 0).unwrap();
    store.push(0, 0, mk(0, 2)).unwrap();
    store.finish_fill().unwrap();
    // Also exercise a RAM remainder after the completed spill segments.
    store.push(0, 0, mk(0, 3)).unwrap();

    let w0 = store.take_wave(0, 0).unwrap();
    let w1 = store.take_wave(0, 1).unwrap();
    assert_eq!(w0.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![0, 2, 3]);
    assert_eq!(w1.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![1]);
    assert!(store.buckets[0].iter().all(|bucket| bucket.capacity() == 0));
    assert_eq!(store.buffered_bytes, 0);
    assert!(w0.iter().all(|m| m.key == 0));
    assert!(w1.iter().all(|m| m.key == 1));
    assert_eq!(
        w0[0].props.iter().next().unwrap().1.to_owned(),
        PropertyValue::Int(0)
    );
}

/// The fill-phase budget's per-member cost: the `Member` struct, 16 B per
/// coordinate, and the packed row's bytes (a tag per column, a 4 B length
/// and the text for a string, else the 1-8 B payload). The schema table
/// is shared and not charged (#461).
#[test]
fn member_bytes_estimate_charges_handles_and_payloads() {
    let m = |props: Vec<(Arc<str>, PropertyValue)>| Member {
        key: 0,
        seq: 0,
        rank: point_rank(0),
        geom: Geometry::LineString(LineString::from(vec![(0.0, 0.0), (1.0, 1.0)])).into(),
        props: SharedProps::new(props),
        id: None,
        gap: GAP_NONE,
        band: 0,
    };
    let base = std::mem::size_of::<Member>() + 2 * 16;
    assert_eq!(member_bytes_estimate(&m(vec![])), base);
    let long_name: Arc<str> = "a_very_long_property_name".into();
    assert_eq!(
        member_bytes_estimate(&m(vec![
            (long_name, PropertyValue::String("hello world!".into())),
            ("rank".into(), PropertyValue::Int(7)),
        ])),
        base + (1 + 4 + 12) + (1 + 8)
    );
}

/// #461: a wave read back from the spill shares one schema table per
/// distinct column set within a segment, instead of allocating names per
/// member. Each segment gets its own copy, so the threads decoding
/// segments do not share one refcount (#803).
#[test]
fn member_store_spill_reload_shares_schema_tables() {
    let plans = vec![LevelPlan::cut(
        4,
        partitions_with_members(&[10]),
        1,
        usize::MAX,
    )];
    let mut store =
        MemberStore::new(&plans, SinkBacking::Spill, MEMBER_STORE_RAM_BUDGET, None).unwrap();
    let mk = |seq: u64, props: Vec<(Arc<str>, PropertyValue)>| Member {
        key: 0,
        seq,
        rank: point_rank(seq),
        geom: Geometry::Point(Point::new(seq as f64, 0.0)).into(),
        props: SharedProps::new(props),
        id: None,
        gap: GAP_NONE,
        band: 0,
    };
    let full = |seq: u64| {
        mk(
            seq,
            vec![
                ("name".into(), PropertyValue::String(format!("f{seq}"))),
                ("rank".into(), PropertyValue::Int(seq as i64)),
            ],
        )
    };
    store.push(0, 0, full(0)).unwrap();
    // A null `name` puts `rank` at position 0.
    store
        .push(0, 0, mk(1, vec![("rank".into(), PropertyValue::Int(1))]))
        .unwrap();
    store.push(0, 0, full(2)).unwrap();
    store.flush_bucket(0, 0).unwrap();
    store.push(0, 0, full(3)).unwrap();
    store.finish_fill().unwrap();

    let w = store.take_wave(0, 0).unwrap();
    assert_eq!(
        w.iter().map(|m| m.seq).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    let names: Vec<_> = w[0].props.names().iter().map(|n| &**n).collect();
    assert_eq!(names, ["name", "rank"]);
    assert_eq!(
        w[1].props.names().len(),
        1,
        "the null-shifted row's own table"
    );
    assert!(
        Arc::ptr_eq(w[0].props.names(), w[2].props.names()),
        "equal tables share one handle within a segment"
    );
    assert!(
        !Arc::ptr_eq(w[0].props.names(), w[3].props.names()),
        "each segment copies its tables"
    );
    assert_eq!(w[0].props.names(), w[3].props.names());
    assert_eq!(
        w[3].props.iter().next().unwrap().1.to_owned(),
        PropertyValue::String("f3".into())
    );
}

/// #740: a flushed bucket larger than [`SPILL_SEGMENT_MEMBERS`] is cut
/// into segments that are encoded and decoded in parallel; the wave comes
/// back whole, in push order, at its exact length, and still sharing one
/// name handle per property name across every segment.
/// The member store's fill buffer is priced from the members it has
/// measured, not the 128 MiB model: once one segment's worth is pushed,
/// it narrows to two whole segments per bucket at their mean size, and
/// the store keeps spilling in whole segments and reading back in order.
#[test]
fn member_store_buffer_is_priced_from_measured_members() {
    let plans = vec![LevelPlan::cut(
        4,
        partitions_with_members(&[10, 10, 10]),
        1,
        usize::MAX,
    )];
    let mut store =
        MemberStore::new(&plans, SinkBacking::Spill, MEMBER_STORE_RAM_BUDGET, None).unwrap();
    let member = |seq: u64| Member {
        key: 0,
        seq,
        rank: point_rank(seq),
        geom: Geometry::Point(Point::new(seq as f64, 0.0)).into(),
        props: SharedProps::new(Vec::new()),
        id: None,
        gap: GAP_NONE,
        band: 0,
    };
    let per_member = member_bytes_estimate(&member(0));
    let n = 20 * SPILL_SEGMENT_MEMBERS;
    for seq in 0..n as u64 {
        store.push(0, (seq % 3) as usize, member(seq)).unwrap();
        if seq + 1 == SPILL_SEGMENT_MEMBERS as u64 {
            assert_eq!(
                store.buffer,
                3 * STORE_BUFFER_SEGMENTS * SPILL_SEGMENT_MEMBERS * per_member,
                "two segments for each of the three buckets"
            );
        }
    }
    assert!(store.buffer < MEMBER_STORE_RAM_BUDGET);
    assert!(store.spilled_bytes() > 0, "a full buffer must spill");
    store.finish_fill().unwrap();
    let mut seen = 0;
    for wave in 0..3 {
        let w = store.take_wave(0, wave).unwrap();
        assert!(w.iter().all(|m| m.seq % 3 == wave as u64));
        seen += w.len();
    }
    assert_eq!(seen, n);
}

#[test]
fn member_store_parallel_segments_roundtrip_in_order() {
    let plans = vec![LevelPlan::cut(
        4,
        partitions_with_members(&[10]),
        1,
        usize::MAX,
    )];
    let mut store =
        MemberStore::new(&plans, SinkBacking::Spill, MEMBER_STORE_RAM_BUDGET, None).unwrap();
    let n = 2 * SPILL_SEGMENT_MEMBERS + 5;
    for seq in 0..n as u64 {
        let name: Arc<str> = format!("p{}", seq % 3).into();
        store
            .push(
                0,
                0,
                Member {
                    key: 0,
                    seq,
                    rank: point_rank(seq),
                    geom: Geometry::Point(Point::new(seq as f64, 0.0)).into(),
                    props: SharedProps::new(vec![(name, PropertyValue::UInt(seq))]),
                    id: None,
                    gap: GAP_NONE,
                    band: 0,
                },
            )
            .unwrap();
    }
    store.finish_fill().unwrap();
    assert!(
        store.spill.as_ref().unwrap().segments[0][0].len() >= 3,
        "the bucket must span several segments"
    );
    let w = store.take_wave(0, 0).unwrap();
    assert_eq!((w.len(), w.capacity()), (n, n));
    assert!(w.iter().enumerate().all(|(i, m)| m.seq == i as u64
        && m.props.iter().next().unwrap().1.to_owned() == PropertyValue::UInt(i as u64)));
    for m in &w[3..] {
        let first = &w[(m.seq % 3) as usize].props.names()[0];
        assert!(Arc::ptr_eq(&m.props.names()[0], first), "seq {}", m.seq);
    }
}

/// Rich 3-level **partitioning** fixture for the #235 byte-equality tests:
/// per-band mixes of points, seam-crossing lines, and multi-tile polygons
/// with tiny row groups so bands span several row groups and coarse
/// features genuinely fan into finer levels' render sets.
#[cfg(test)]
fn partitioning_equivalence_fixture(path: &Path) {
    let (wide, concave, wiggly) = equivalence_shapes();
    let big_box = Geometry::Polygon(geo::Polygon::new(
        LineString::from(vec![
            (-20.0, -20.0),
            (20.0, -20.0),
            (20.0, 20.0),
            (-20.0, 20.0),
            (-20.0, -20.0),
        ]),
        vec![],
    ));
    write_partitioning_fixture(
        path,
        &[
            // Level 0 (z2): spread points + a continent-scale polygon.
            (
                vec![0, 1, 2],
                vec![
                    Geometry::Point(Point::new(-120.0, 40.0)),
                    Geometry::Point(Point::new(120.0, -40.0)),
                    big_box,
                ],
            ),
            // Level 1 (z4): a seam-crossing line + points + concave poly.
            (
                vec![3, 4, 5, 6],
                vec![
                    wide,
                    Geometry::Point(Point::new(0.0, 0.0)),
                    Geometry::Point(Point::new(-119.5, 39.5)),
                    concave,
                ],
            ),
            // Level 2 (z6): dense line + clustered points.
            (
                vec![7, 8, 9, 10],
                vec![
                    wiggly,
                    Geometry::Point(Point::new(-118.0, 34.0)),
                    Geometry::Point(Point::new(2.0, 48.0)),
                    Geometry::Point(Point::new(-117.5, 33.8)),
                ],
            ),
        ],
    );
}

/// #235: the partitioning single-read fan-out pass 2 must produce a
/// byte-identical archive (and equal report) to the legacy per-level
/// wave-read pass 2 it replaces — under both member-store backings, and
/// with a tiny partition target plus explicit narrow waves so many
/// partitions and multiple waves per level actually form.
#[test]
fn partitioning_single_read_pass2_matches_legacy() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    partitioning_equivalence_fixture(tin.path());
    for (target, wave) in [(1usize, 1usize), (1, 3), (DEFAULT_PARTITION_TARGET, 2)] {
        let opts = ExportOptions {
            layer_name: "ref".to_string(),
            partition_wave: wave,
            ..Default::default()
        };
        let t_legacy = tempfile::NamedTempFile::new().unwrap();
        let r_legacy =
            export_pmtiles_impl(tin.path(), t_legacy.path(), &opts, target, true, None).unwrap();
        let legacy_bytes = std::fs::read(t_legacy.path()).unwrap();
        for backing in [SinkBacking::Ram, SinkBacking::Spill] {
            let t_new = tempfile::NamedTempFile::new().unwrap();
            let r_new = export_pmtiles_impl(
                tin.path(),
                t_new.path(),
                &opts,
                target,
                false,
                Some(backing),
            )
            .unwrap();
            assert_eq!(
                std::fs::read(t_new.path()).unwrap(),
                legacy_bytes,
                "single-read ({backing:?}, target {target}, wave {wave}) \
                 archive diverges from legacy pass 2"
            );
            assert_eq!(r_new.zooms, r_legacy.zooms);
            assert_eq!(r_new.total_tiles, r_legacy.total_tiles);
            assert_eq!(r_new.total_tile_features, r_legacy.total_tile_features);
            assert_eq!(r_new.oversized_tiles, r_legacy.oversized_tiles);
        }
    }
}

/// A 40×40 grid of touching 0.5° squares spanning several z4 and many z6
/// tiles, split into `bands` level bands in row order.
fn coverage_bands(bands: usize) -> Vec<(Vec<i64>, Vec<Geometry<f64>>)> {
    let mut all: Vec<(i64, Geometry<f64>)> = Vec::new();
    for row in 0..40 {
        for col in 0..40 {
            let (x, y) = (-10.0 + col as f64 * 0.5, -10.0 + row as f64 * 0.5);
            let g = Geometry::Polygon(geo::Polygon::new(
                LineString::from(vec![
                    (x, y),
                    (x + 0.5, y),
                    (x + 0.5, y + 0.5),
                    (x, y + 0.5),
                    (x, y),
                ]),
                vec![],
            ));
            all.push((row * 40 + col, g));
        }
    }
    let per = all.len().div_ceil(bands);
    all.chunks(per).map(|c| c.iter().cloned().unzip()).collect()
}

/// #745: every merging valve gives the same bytes whatever the partition
/// target or wave width, and the partitioning single-read pass 2 gives
/// the same bytes as the legacy wave read under both store backings.
/// Gaps reset at each row group, and the small row groups here make
/// those resets fall inside tiles and partitions.
#[test]
fn merging_valves_are_partition_and_path_invariant() {
    {
        let valve = SizeValve::Merge;
        let opts = |wave: usize| ExportOptions {
            layer_name: "ref".to_string(),
            tile_size_limit: Some(3_000),
            size_valve: valve,
            partition_wave: wave,
            ..Default::default()
        };

        // Duplicating: every level holds the whole grid.
        let dup = tempfile::NamedTempFile::new().unwrap();
        let grid = coverage_bands(1).remove(0);
        write_mode_fixture(
            dup.path(),
            &[grid.clone(), grid.clone(), grid],
            Mode::Duplicating,
            97,
        );
        let t_many = tempfile::NamedTempFile::new().unwrap();
        let t_one = tempfile::NamedTempFile::new().unwrap();
        let r_many =
            export_pmtiles_with_partition_target(dup.path(), t_many.path(), &opts(1), 1).unwrap();
        let r_one = export_pmtiles(dup.path(), t_one.path(), &opts(16)).unwrap();
        assert!(
            r_one.merged_features > 0,
            "{valve:?}: the valve must merge here"
        );
        assert_eq!(r_one.valve_dropped_features, 0, "{valve:?}");
        // Every tile takes at least one encode; the valve's tiles more.
        for z in &r_one.zooms {
            assert!(
                z.valve_passes >= z.tile_count as u64
                    && z.valve_passes >= u64::from(z.max_valve_passes),
                "{valve:?}: {z:?}"
            );
        }
        assert!(
            r_one
                .zooms
                .iter()
                .any(|z| z.valve_passes > z.tile_count as u64),
            "{valve:?}: {:?}",
            r_one.zooms
        );
        assert_eq!(
            std::fs::read(t_many.path()).unwrap(),
            std::fs::read(t_one.path()).unwrap(),
            "{valve:?}: duplicating archives diverge across partition targets"
        );
        assert_eq!(r_many.zooms, r_one.zooms, "{valve:?}");

        // Partitioning: three bands; single-read vs legacy.
        let part = tempfile::NamedTempFile::new().unwrap();
        write_mode_fixture(part.path(), &coverage_bands(3), Mode::Partitioning, 97);
        for (target, wave) in [(1usize, 1usize), (1, 3), (DEFAULT_PARTITION_TARGET, 2)] {
            let t_legacy = tempfile::NamedTempFile::new().unwrap();
            let r_legacy = export_pmtiles_impl(
                part.path(),
                t_legacy.path(),
                &opts(wave),
                target,
                true,
                None,
            )
            .unwrap();
            assert!(r_legacy.merged_features > 0, "{valve:?}");
            let legacy_bytes = std::fs::read(t_legacy.path()).unwrap();
            for backing in [SinkBacking::Ram, SinkBacking::Spill] {
                let t_new = tempfile::NamedTempFile::new().unwrap();
                let r_new = export_pmtiles_impl(
                    part.path(),
                    t_new.path(),
                    &opts(wave),
                    target,
                    false,
                    Some(backing),
                )
                .unwrap();
                assert_eq!(
                    std::fs::read(t_new.path()).unwrap(),
                    legacy_bytes,
                    "{valve:?}: single-read ({backing:?}, target {target}, wave {wave}) \
                     diverges from legacy"
                );
                assert_eq!(r_new.zooms, r_legacy.zooms, "{valve:?}");
            }
        }
    }
}

/// #235: the production partitioning path (auto backing) is partition- and
/// wave-invariant, mirroring `partitioned_export_is_partition_invariant`
/// for the new pass 2.
#[test]
fn partitioning_export_is_partition_invariant() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    partitioning_equivalence_fixture(tin.path());
    let opts = ExportOptions {
        layer_name: "ref".to_string(),
        ..Default::default()
    };
    let t_many = tempfile::NamedTempFile::new().unwrap();
    let t_one = tempfile::NamedTempFile::new().unwrap();
    let r_many = export_pmtiles_with_partition_target(tin.path(), t_many.path(), &opts, 1).unwrap();
    let r_one = export_pmtiles(tin.path(), t_one.path(), &opts).unwrap();
    assert_eq!(
        std::fs::read(t_many.path()).unwrap(),
        std::fs::read(t_one.path()).unwrap(),
        "partitioning archives diverge across partition targets"
    );
    assert_eq!(r_many.zooms, r_one.zooms);
    assert_eq!(r_many.total_tiles, r_one.total_tiles);
}

#[test]
fn export_pmtiles_writes_nonempty_archive() {
    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let b = Geometry::Point(Point::new(120.0, -40.0));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[
            (vec![0, 1], vec![a.clone(), b.clone()]),
            (vec![0, 1], vec![a.clone(), b.clone()]),
        ],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let report = export_pmtiles(tin.path(), tout.path(), &ExportOptions::default()).unwrap();

    assert_eq!(report.min_zoom, 2);
    assert_eq!(report.max_zoom, 4);
    assert_eq!(report.zooms.len(), 2);
    assert!(report.total_tiles >= 2);
    // The archive exists and has a PMTiles header.
    let meta = std::fs::metadata(tout.path()).unwrap();
    assert!(meta.len() > 127, "archive must be larger than the header");
    let mut magic = [0u8; 7];
    use std::io::Read;
    std::fs::File::open(tout.path())
        .unwrap()
        .read_exact(&mut magic)
        .unwrap();
    assert_eq!(&magic, b"PMTiles");
}

// --- provenance columns that carry no information (#379) ----------------

/// A `Generalization` block declaring only line coalescing, optionally
/// with rename provenance (#288/#359).
fn coalescing_generalization(renames: Option<BTreeMap<String, String>>) -> Generalization {
    Generalization {
        engine: "tylertoo test".to_string(),
        line_thinning: None,
        gsd_base: None,
        cascade: None,
        collapse: None,
        representation: None,
        levels: vec![],
        ranking: None,
        entry_zoom: None,
        density_drop: None,
        clustering: None,
        coalescing: Some(CoalescingProvenance {
            enabled: true,
            snap_tolerance_gsd_factor: 1.0,
            junction_angle: Some(0.0),
            max_level_rows: Some(2_000_000),
            coalesced_count_column: "coalesced_count".to_string(),
            merged: None,
        }),
        renamed_columns: renames,
        zoom_ceiling: None,
    }
}

/// A `Generalization` block declaring only clustering (`--cluster`).
fn clustering_generalization() -> Generalization {
    Generalization {
        clustering: Some(ClusteringProvenance {
            enabled: true,
            point_count_column: "point_count".to_string(),
            accumulated: vec![],
            categories: None,
        }),
        coalescing: None,
        ..coalescing_generalization(None)
    }
}

/// Two-level fixture with the given footer provenance whose typed
/// `counter` column holds `counts[k]` at level k. Schema: `id`, then
/// `user_column` (an INT32 user column, when given), `geometry`, `counter`.
fn write_counter_fixture(
    path: &Path,
    generalization: Generalization,
    counter: &str,
    counts: &[Vec<i32>],
    user_column: Option<&str>,
) {
    use arrow_array::Int32Array;
    let is_point_count = counter == "point_count";
    write_typed_counter_fixture(
        path,
        generalization,
        Field::new(
            counter,
            if is_point_count {
                DataType::Int64
            } else {
                DataType::Int32
            },
            false,
        ),
        &counts
            .iter()
            .map(|c| {
                if is_point_count {
                    Arc::new(Int64Array::from(
                        c.iter().copied().map(i64::from).collect::<Vec<_>>(),
                    )) as ArrayRef
                } else {
                    Arc::new(Int32Array::from(c.clone())) as ArrayRef
                }
            })
            .collect::<Vec<_>>(),
        user_column,
    );
}

/// [`write_counter_fixture`] with the counter column's Arrow field and
/// per-level arrays supplied by the caller, for columns the converter
/// never writes itself (a pyarrow-style unsigned or nullable counter,
/// #399).
fn write_typed_counter_fixture(
    path: &Path,
    generalization: Generalization,
    counter: Field,
    counts: &[ArrayRef],
    user_column: Option<&str>,
) {
    use arrow_array::Int32Array;

    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let b = Geometry::Point(Point::new(120.0, -40.0));
    let mut fields = vec![Field::new("id", DataType::Int64, false)];
    if let Some(name) = user_column {
        fields.push(Field::new(name, DataType::Int32, false));
    }
    fields.push(geometry_field());
    fields.push(counter);
    let schema = Arc::new(Schema::new(fields));
    let specs: Vec<LevelSpec> = (0..counts.len())
        .map(|k| LevelSpec::new(gsd((2 + 2 * k) as u8), Some((2 + 2 * k) as u8)))
        .collect();
    let mut opts = OverviewWriterOptions::new(Mode::Duplicating, specs);
    opts.generalization = Some(generalization);
    let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
    for (k, level_counts) in counts.iter().enumerate() {
        let n = level_counts.len();
        let geoms: Vec<Geometry<f64>> = (0..n)
            .map(|i| if i % 2 == 0 { a.clone() } else { b.clone() })
            .collect();
        let mut columns: Vec<ArrayRef> = vec![Arc::new(Int64Array::from(
            (0..n as i64).collect::<Vec<_>>(),
        ))];
        if user_column.is_some() {
            columns.push(Arc::new(Int32Array::from(vec![7; n])));
        }
        columns.push(Arc::new(build_geometry_array(&geoms).to_array_ref()));
        columns.push(level_counts.clone());
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        assert_eq!(
            writer
                .write_level(k, Some(n), std::iter::once(batch))
                .unwrap(),
            LevelWriteOutcome::Written
        );
    }
    writer.finish().unwrap();
}

/// Two-level fixture whose footer says coalescing was on and whose
/// `coalesced_count` column holds `counts[k]` at level k.
fn write_coalesced_fixture(path: &Path, counts: &[Vec<i32>]) {
    write_counter_fixture(
        path,
        coalescing_generalization(None),
        "coalesced_count",
        counts,
        None,
    );
}

/// Export the fixture and return the property names the tiles carry, as
/// seen through `decode_pmtiles` (the union of every layer's keys).
fn exported_property_names(input: &Path) -> Vec<String> {
    let tout = tempfile::NamedTempFile::new().unwrap();
    export_pmtiles(input, tout.path(), &ExportOptions::default()).unwrap();
    decoded_property_names(tout.path())
}

#[test]
fn coalesced_count_is_not_exported_when_it_is_1_everywhere() {
    // A polygon/point-only input gets the column too (coalescing is on by
    // default) but nothing was ever merged: every value is 1. That is not
    // a property of the data and must not reach the tiles.
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_coalesced_fixture(tin.path(), &[vec![1, 1], vec![1, 1]]);
    let names = exported_property_names(tin.path());
    assert_eq!(names, vec!["id"], "coalesced_count leaked into the tiles");

    // ... and the layer must not advertise it either.
    let reader = OverviewReader::open(tin.path()).unwrap();
    let published = PublishedNames::from_reader(&reader);
    let fields = field_metadata(
        reader.schema(),
        reader.resolved_schema().geometry_index(),
        &published,
    );
    assert!(
        !fields.contains_key("coalesced_count"),
        "vector_layers fields must not advertise it: {fields:?}"
    );
}

/// An explicit `--include-property coalesced_count` wins over the
/// max==1 heuristic: the column is plainly in the file, and a caller who
/// names it wants it (a style that keys on it, a debugging pass).
#[test]
fn coalesced_count_explicit_include_overrides_suppression() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_coalesced_fixture(tin.path(), &[vec![1, 1], vec![1, 1]]);
    let opts = ExportOptions {
        properties: PropertySelection {
            include: Some(vec!["coalesced_count".to_string()]),
            ..Default::default()
        },
        ..ExportOptions::default()
    };
    let tout = tempfile::NamedTempFile::new().unwrap();
    export_pmtiles(tin.path(), tout.path(), &opts).expect("explicit include exports");
    let decoded = tempfile::NamedTempFile::new().unwrap();
    crate::decode::decode_pmtiles(
        tout.path(),
        decoded.path(),
        &crate::decode::DecodeOptions::default(),
    )
    .unwrap();
    let file = std::fs::File::open(decoded.path()).unwrap();
    let names: Vec<String> = tylertoo_io::open_parquet(file)
        .unwrap()
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .filter(|n| {
            !matches!(
                n.as_str(),
                "zoom" | "layer" | "mvt_id" | "geometry" | "bbox"
            )
        })
        .collect();
    assert_eq!(names, vec!["coalesced_count"]);

    // ... and the layer advertises it.
    let reader = OverviewReader::open(tin.path()).unwrap();
    let geom_idx = reader.resolved_schema().geometry_index().unwrap();
    let published = PublishedNames::from_reader(&reader)
        .with_selection(
            reader.schema(),
            geom_idx,
            &opts.properties,
            &opts.feature_order,
        )
        .unwrap();
    let fields = field_metadata(reader.schema(), Some(geom_idx), &published);
    assert_eq!(
        fields.keys().collect::<Vec<_>>(),
        vec!["coalesced_count"],
        "{fields:?}"
    );
}

#[test]
fn coalesced_count_is_exported_when_coalescing_merged_something() {
    // Level 0 merged two segments somewhere: the column now carries real
    // information and stays a property.
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_coalesced_fixture(tin.path(), &[vec![2, 1], vec![1, 1]]);
    let names = exported_property_names(tin.path());
    assert_eq!(names, vec!["coalesced_count", "id"]);
}

/// Every decoded feature as `(zoom, coalesced_count)`, `None` where the
/// feature does not carry the key.
fn decoded_coalesced_counts(pmtiles: &Path) -> Vec<(u8, Option<i64>)> {
    use arrow_array::cast::AsArray;
    let decoded = tempfile::NamedTempFile::new().unwrap();
    crate::decode::decode_pmtiles(
        pmtiles,
        decoded.path(),
        &crate::decode::DecodeOptions::default(),
    )
    .unwrap();
    let file = std::fs::File::open(decoded.path()).unwrap();
    let mut out = Vec::new();
    for batch in tylertoo_io::open_parquet(file).unwrap().build().unwrap() {
        let batch = batch.unwrap();
        let zooms =
            arrow_cast::cast(batch.column_by_name("zoom").unwrap(), &DataType::UInt8).unwrap();
        let counts = batch
            .column_by_name("coalesced_count")
            .map(|c| arrow_cast::cast(c, &DataType::Int64).unwrap());
        for row in 0..batch.num_rows() {
            let count = counts.as_ref().and_then(|c| {
                let c = c.as_primitive::<arrow_array::types::Int64Type>();
                c.is_valid(row).then(|| c.value(row))
            });
            out.push((
                zooms
                    .as_primitive::<arrow_array::types::UInt8Type>()
                    .value(row),
                count,
            ));
        }
    }
    out
}

/// End to end on real lines: six chains of 30 touching ~111 m segments
/// and four long lines. The canonical (finest) level never coalesces, so
/// every count there is 1 by construction (spec §13.2) and its tiles
/// carry no `coalesced_count`, while a coarser level that merged the
/// chains keeps the counter with the merged count.
#[test]
fn coalesced_count_is_left_off_canonical_tiles_and_kept_where_chains_merged() {
    use tylertoo_io::test_support::ArrowWriter;
    let dir = tempfile::tempdir().unwrap();
    let (input, overview, out) = (
        dir.path().join("lines.parquet"),
        dir.path().join("lines_overview.parquet"),
        dir.path().join("lines.pmtiles"),
    );
    let line = |pts: &[(f64, f64)]| geo::Geometry::LineString(geo::LineString::from(pts.to_vec()));
    let mut geoms = Vec::new();
    for c in 0..6 {
        let (x0, y0) = (-50.0 + f64::from(c) * 17.0, -20.0 + f64::from(c) * 8.0);
        for k in 0..30 {
            let x = x0 + f64::from(k) * 0.001;
            geoms.push(line(&[(x, y0), (x + 0.001, y0)]));
        }
    }
    for l in 0..4 {
        let y = -60.0 + f64::from(l) * 35.0;
        geoms.push(line(&[(-120.0, y), (-60.0, y + 5.0), (0.0, y - 3.0)]));
    }
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("geometry", DataType::Binary, false),
    ]));
    let wkb: Vec<Vec<u8>> = geoms
        .iter()
        .map(|g| crate::wkb::geometry_to_wkb(g).unwrap())
        .collect();
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from((0..wkb.len() as i64).collect::<Vec<_>>())),
            Arc::new(arrow_array::BinaryArray::from(
                wkb.iter().map(Vec::as_slice).collect::<Vec<_>>(),
            )),
        ],
    )
    .unwrap();
    let geo_meta = serde_json::json!({
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {"geometry": {"encoding": "WKB", "geometry_types": ["LineString"]}}
    });
    let props = tylertoo_io::test_support::WriterProperties::builder()
        .set_key_value_metadata(Some(vec![tylertoo_io::KeyValue::new(
            "geo".to_string(),
            geo_meta.to_string(),
        )]))
        .build();
    let mut w =
        ArrowWriter::try_new(std::fs::File::create(&input).unwrap(), schema, Some(props)).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();

    let opts = crate::overview::convert::ConvertOptions::default().with_levels(
        crate::overview::convert::LevelPlan::ZoomRange {
            min_zoom: 1,
            max_zoom: 12,
        },
    );
    crate::overview::convert::convert_to_overviews(&input, &overview, &opts).unwrap();
    export_pmtiles(&overview, &out, &ExportOptions::default()).unwrap();

    let features = decoded_coalesced_counts(&out);
    let canonical: Vec<_> = features.iter().filter(|(z, _)| *z == 12).collect();
    assert!(
        !canonical.is_empty(),
        "the canonical zoom must hold features"
    );
    assert!(
        canonical.iter().all(|(_, c)| c.is_none()),
        "canonical-level tiles must not carry coalesced_count: {canonical:?}"
    );
    assert!(
        features.iter().any(|&(z, c)| z < 12 && c == Some(30)),
        "a merged chain must keep its count of 30 at a coarser zoom"
    );
}

/// #379 must not undo #359: once the converter's own `coalesced_count` is
/// withheld, the name is free again, so a user column that was moved
/// aside to `coalesced_count_` on convert (#288) is given its name back.
#[test]
fn suppressed_counter_frees_its_name_for_a_renamed_source_column() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_counter_fixture(
        tin.path(),
        coalescing_generalization(Some(BTreeMap::from([(
            "coalesced_count_".to_string(),
            "coalesced_count".to_string(),
        )]))),
        "coalesced_count",
        &[vec![1, 1], vec![1, 1]],
        Some("coalesced_count_"),
    );

    let reader = OverviewReader::open(tin.path()).unwrap();
    let published = PublishedNames::from_reader(&reader);
    let schema = reader.schema();
    let names: Vec<String> = property_columns(schema, geometry_index(schema).unwrap(), &published)
        .into_iter()
        .map(|(_, n)| n)
        .collect();
    assert_eq!(
        names,
        vec!["id", "coalesced_count"],
        "the withheld counter no longer occupies its name"
    );
    let fields = field_metadata(schema, geometry_index(schema), &published);
    assert_eq!(
        fields.get("coalesced_count").map(String::as_str),
        Some("Number")
    );
    assert!(!fields.contains_key("coalesced_count_"), "{fields:?}");

    // End to end: the tiles carry the user's column under its own name.
    let mut exported = exported_property_names(tin.path());
    exported.sort();
    assert_eq!(exported, vec!["coalesced_count", "id"]);
}

/// `point_count` is a documented part of the `--cluster` contract (a
/// style scales symbols by it), so it stays even when nothing clustered:
/// the max==1 rule applies to `coalesced_count` only.
#[test]
fn point_count_is_exported_even_when_it_is_1_everywhere() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_counter_fixture(
        tin.path(),
        clustering_generalization(),
        "point_count",
        &[vec![1, 1], vec![1, 1]],
        None,
    );
    let names = exported_property_names(tin.path());
    assert_eq!(names, vec!["id", "point_count"], "point_count must stay");

    let reader = OverviewReader::open(tin.path()).unwrap();
    let published = PublishedNames::from_reader(&reader);
    let fields = field_metadata(
        reader.schema(),
        reader.resolved_schema().geometry_index(),
        &published,
    );
    assert_eq!(
        fields.get("point_count").map(String::as_str),
        Some("Number")
    );
}

#[test]
fn invalid_cluster_count_type_is_reported_before_reading_data() {
    use arrow_array::Int32Array;
    let input = tempfile::NamedTempFile::new().unwrap();
    write_typed_counter_fixture(
        input.path(),
        clustering_generalization(),
        Field::new("point_count", DataType::Int32, false),
        &[Arc::new(Int32Array::from(vec![1, 1])) as ArrayRef],
        None,
    );
    let report = crate::overview::check::validate_file(input.path()).unwrap();
    assert_eq!(
        report.check_passed("cluster_point_count_column"),
        Some(false)
    );
    assert_eq!(report.check_passed("cluster_sum_invariant"), None);
    let error = OverviewReader::open(input.path()).unwrap_err();
    assert!(
        error.to_string().contains("cluster_point_count_column"),
        "{error}"
    );
}

/// The footer names the counter column, but only the converter's own
/// `coalesced_count` may be withheld: a footer pointing the name at a
/// real data column must not make the export drop that column.
#[test]
fn footer_cannot_redirect_suppression_onto_a_data_column() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    let mut generalization = coalescing_generalization(None);
    generalization
        .coalescing
        .as_mut()
        .unwrap()
        .coalesced_count_column = "id".to_string();
    // Two rows per level, so `id` is 0/1: its max really is 1.
    write_counter_fixture(
        tin.path(),
        generalization,
        "coalesced_count",
        &[vec![1, 1], vec![1, 1]],
        None,
    );
    let error = OverviewReader::open(tin.path()).unwrap_err();
    assert!(
        error.to_string().contains("coalesce_count_column"),
        "{error}"
    );
}

/// A nullable counter contradicts the coalescing provenance. Both entry
/// points must reject it before publication can suppress any property.
#[test]
fn nullable_coalesced_count_is_rejected_as_invalid_provenance() {
    use arrow_array::Int32Array;

    let tin = tempfile::NamedTempFile::new().unwrap();
    write_typed_counter_fixture(
        tin.path(),
        coalescing_generalization(None),
        Field::new("coalesced_count", DataType::Int32, true),
        &[
            Arc::new(Int32Array::from(vec![Some(1), None])) as ArrayRef,
            Arc::new(Int32Array::from(vec![Some(1), Some(1)])) as ArrayRef,
        ],
        None,
    );
    let report = crate::overview::check::validate_file(tin.path()).unwrap();
    assert_eq!(report.check_passed("coalesce_count_column"), Some(false));
    let error = OverviewReader::open(tin.path()).unwrap_err();
    assert!(
        error.to_string().contains("coalesce_count_column"),
        "{error}"
    );
}

/// #399: a foreign writer's `UInt32` `coalesced_count` is compared as
/// signed by a naive statistics fold, so a row group whose real max is
/// 2^31 would fold *below* one whose max is 1 and the counter would be
/// withheld although it plainly carries information. It must stay
/// exported.
#[test]
fn unsigned_coalesced_count_is_not_withheld() {
    use arrow_array::UInt32Array;

    let tin = tempfile::NamedTempFile::new().unwrap();
    write_typed_counter_fixture(
        tin.path(),
        coalescing_generalization(None),
        Field::new("coalesced_count", DataType::UInt32, false),
        &[
            Arc::new(UInt32Array::from(vec![1u32, 1 << 31])) as ArrayRef,
            Arc::new(UInt32Array::from(vec![1u32, 1])) as ArrayRef,
        ],
        None,
    );
    let reader = OverviewReader::open(tin.path()).unwrap();
    assert_eq!(reader.int_column_max("coalesced_count"), None);
    let names = exported_property_names(tin.path());
    assert_eq!(names, vec!["coalesced_count", "id"]);
}

// --- declared minimum zoom (#380) ----------------------------------------

/// The converter omits levels that generalize to nothing (§7.3), so an
/// overview built for z0..z4 can start at z2. The report still reflects
/// the range that was asked for (what the CLI prints and what
/// `vector_layers[].minzoom` advertises), but the PMTiles header must
/// stay the range the directory actually addresses (z2..z4): #529/#522
/// found `go-pmtiles verify` rejecting every archive whose header
/// declared a coarser minimum than its shallowest real tile.
#[test]
fn export_declared_min_zoom_widens_report_but_header_reflects_actual_tiles() {
    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let b = Geometry::Point(Point::new(120.0, -40.0));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[
            (vec![0, 1], vec![a.clone(), b.clone()]),
            (vec![0, 1], vec![a.clone(), b.clone()]),
        ],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        min_zoom: Some(0),
        ..ExportOptions::default()
    };
    let report = export_pmtiles(tin.path(), tout.path(), &opts).unwrap();
    assert_eq!(report.min_zoom, 0, "report must cover the declared range");
    assert_eq!(report.max_zoom, 4);
    assert_eq!(
        report.zooms.len(),
        2,
        "no phantom per-zoom rows for empty zooms"
    );

    let data = std::fs::read(tout.path()).unwrap();
    let header = crate::pmtiles_writer::Header::from_bytes(&data[..127]).unwrap();
    assert_eq!(
        header.min_zoom, 2,
        "header must be the shallowest zoom with an actual tile, not the declared z0"
    );
    assert_eq!(header.max_zoom, 4);
    crate::archive_index::assert_header_zooms_match_tiles(tout.path());

    // Default options over the same file (whose coarse zooms are empty):
    // the header must satisfy the same invariant.
    let tdef = tempfile::NamedTempFile::new().unwrap();
    export_pmtiles(tin.path(), tdef.path(), &ExportOptions::default()).unwrap();
    crate::archive_index::assert_header_zooms_match_tiles(tdef.path());
}

/// Declaring a minimum finer than the coarsest level would misdescribe
/// tiles that exist; that is a caller error, not something to paper over.
#[test]
fn export_declared_min_zoom_finer_than_coarsest_level_is_rejected() {
    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[(vec![0], vec![a.clone()]), (vec![0], vec![a.clone()])],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        min_zoom: Some(3),
        ..ExportOptions::default()
    };
    let err = export_pmtiles(tin.path(), tout.path(), &opts).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("min_zoom") && msg.contains('3') && msg.contains('2'),
        "error must name the declared and actual minimum: {msg}"
    );
}

/// #371: `--min-zoom` is written into the archive header, so it obeys the
/// same ceiling as everything else — and is rejected naming the flag and
/// the limit rather than the #380 "finer than the coarsest level" message.
#[test]
fn export_declared_min_zoom_above_the_ceiling_is_rejected() {
    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[(vec![0], vec![a.clone()]), (vec![0], vec![a.clone()])],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        min_zoom: Some(33),
        ..ExportOptions::default()
    };
    let err = export_pmtiles(tin.path(), tout.path(), &opts).unwrap_err();
    assert_eq!(
        crate::error::Classify::kind(&err),
        crate::error::ErrorKind::Usage,
        "got: {err}"
    );
    let msg = err.to_string();
    assert!(msg.contains("33"), "error names the value: {msg}");
    assert!(msg.contains("--min-zoom"), "error names the flag: {msg}");
    assert!(msg.contains("30"), "error names the limit: {msg}");
}

/// #433: `extent` was validated nowhere. `extent = 0` quantizes every
/// coordinate to (0,0) — every polygon and line silently degenerates and
/// drops — and the layer is still written with `extent: 0`, which every
/// consumer then divides by. Refused before the file is even opened.
#[test]
fn export_rejects_extent_zero() {
    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[(vec![0], vec![a.clone()]), (vec![0], vec![a.clone()])],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        extent: 0,
        ..ExportOptions::default()
    };
    let err = export_pmtiles(tin.path(), tout.path(), &opts).unwrap_err();
    assert!(
        matches!(
            err,
            ExportError::Config(ConfigError::InvalidExtent { extent: 0 })
        ),
        "got: {err}"
    );
    let msg = err.to_string();
    assert!(msg.contains("extent"), "error names the knob: {msg}");
    assert_eq!(
        std::fs::metadata(tout.path()).unwrap().len(),
        0,
        "nothing is written for a rejected extent"
    );
}

/// #433: `ExportOptions::validate` is the one gate. A non-power-of-two
/// extent is legal MVT (the spec only says "should"), so it is accepted
/// with a warning rather than refused; zero is refused; the default is
/// silent.
#[test]
fn export_options_validate_sorts_out_extents() {
    let with_extent = |extent: u32| ExportOptions {
        extent,
        ..ExportOptions::default()
    };
    with_extent(DEFAULT_EXTENT).validate().unwrap();
    with_extent(256).validate().unwrap();
    // Warned about, not refused.
    with_extent(4095).validate().unwrap();
    with_extent(1000).validate().unwrap();
    let err = with_extent(0).validate().unwrap_err();
    assert!(
        matches!(err, ConfigError::InvalidExtent { extent: 0 }),
        "got: {err}"
    );
}

/// #433: `--tile-buffer 100000` was accepted, making the clip margin ~390
/// tile widths and the per-tile membership O(features × tiles). The cap is
/// one full tile width ([`MAX_TILE_BUFFER_PX`]); at the cap is fine, one
/// past it is refused naming the value and the cap.
#[test]
fn export_rejects_a_tile_buffer_past_the_cap() {
    let with_buffer = |tile_buffer: u32| ExportOptions {
        tile_buffer,
        ..ExportOptions::default()
    };
    with_buffer(0).validate().unwrap();
    with_buffer(MAX_TILE_BUFFER_PX).validate().unwrap();

    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[(vec![0], vec![a.clone()]), (vec![0], vec![a.clone()])],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    for buffer in [MAX_TILE_BUFFER_PX + 1, 100_000] {
        let err = export_pmtiles(tin.path(), tout.path(), &with_buffer(buffer)).unwrap_err();
        assert!(
            matches!(
                err,
                ExportError::Config(ConfigError::TileBufferTooWide { buffer: b, max })
                    if b == buffer && max == MAX_TILE_BUFFER_PX
            ),
            "buffer {buffer}: got {err}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("--tile-buffer")
                && msg.contains(&buffer.to_string())
                && msg.contains(&MAX_TILE_BUFFER_PX.to_string()),
            "buffer {buffer}: error names the flag, the value and the cap: {msg}"
        );
    }
}

/// #433 reconciling #498: the universal cap is at most the shard
/// read-pruning margin, so one constant governs both — a sharded export
/// with a too-wide buffer gets the universal refusal, never the shard one
/// (whose "lower it to 512" advice would be wrong under a 256 cap).
#[test]
fn a_sharded_tile_buffer_is_governed_by_the_universal_cap() {
    const _: () = assert!(MAX_TILE_BUFFER_PX <= crate::shard::MAX_SHARD_TILE_BUFFER_PX);
    let opts = ExportOptions {
        tile_buffer: crate::shard::MAX_SHARD_TILE_BUFFER_PX + 1,
        tile_range: Some(crate::shard::TileRange::parse("5..12").unwrap()),
        ..ExportOptions::default()
    };
    let err = opts.validate().unwrap_err();
    assert!(
        matches!(err, ConfigError::TileBufferTooWide { .. }),
        "got: {err}"
    );
    // At the universal cap a sharded export is fine too.
    ExportOptions {
        tile_buffer: MAX_TILE_BUFFER_PX,
        ..opts
    }
    .validate()
    .unwrap();
}

/// #371, end to end: an overview file written before the ceiling existed
/// can record an out-of-range zoom in its level metadata. Nothing derives
/// that zoom, so the GSD check never sees it — the export itself has to
/// refuse the file rather than write an archive whose z33 tile ids are
/// wrapped nonsense.
#[test]
fn export_rejects_a_level_whose_recorded_zoom_is_above_the_ceiling() {
    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let schema = Arc::new(source_schema());
    let specs = vec![
        LevelSpec::new(gsd(2), Some(2)),
        // What an older tylertoo would happily have written.
        LevelSpec::new(gsd(4), Some(33)),
    ];
    let tin = tempfile::NamedTempFile::new().unwrap();
    let mut writer = OverviewWriter::create(
        tin.path(),
        &schema,
        OverviewWriterOptions::new(Mode::Duplicating, specs),
    )
    .unwrap();
    for k in 0..2 {
        assert_eq!(
            writer
                .write_level(
                    k,
                    Some(1),
                    std::iter::once(batch(&schema, &[0], std::slice::from_ref(&a))),
                )
                .unwrap(),
            LevelWriteOutcome::Written
        );
    }
    writer.finish().unwrap();

    let tout = tempfile::NamedTempFile::new().unwrap();
    let err = export_pmtiles(tin.path(), tout.path(), &ExportOptions::default())
        .expect_err("a recorded z33 level must be refused");
    assert!(
        matches!(
            err,
            ExportError::LevelZoomAboveCeiling {
                level: 1,
                implied: 33,
                ceiling: 30,
                ..
            }
        ),
        "got: {err}"
    );
}

/// #506 follow-up (F1): `OverviewsMeta::validate` only requires `gsd` to
/// be strictly decreasing across levels, not that the derived ZOOM is
/// distinct — two GSDs close enough to round to the same integer zoom
/// both pass it. Pre-#506 this silently wrote that zoom's tiles twice,
/// clobbering directory entries with no error (both levels shared the
/// same row-major key); post-#506 it instead trips the writer's
/// ascending-tile-id `debug_assert!`. Either way it must be a clean,
/// named `ExportError`, caught before any scan runs.
#[test]
fn export_rejects_levels_whose_derived_zooms_collide() {
    // Guard the fixture's own premise: both GSDs must round to z4 and
    // must be strictly decreasing (gsd(4) is the exact z4 value; scaling
    // it up by less than sqrt(2) keeps the derived zoom at 4, since the
    // z3/z4 rounding boundary sits at gsd(4) * sqrt(2)). If a future
    // change to the GSD/zoom formula moves this, the test fails loudly
    // here instead of silently stopping to exercise the collision.
    let gsd_a = gsd(4) * 1.10;
    let gsd_b = gsd(4) * 1.02;
    assert!(
        gsd_a > gsd_b,
        "fixture premise: levels must stay coarse -> fine (gsd strictly decreasing)"
    );
    assert_eq!(
        zoom_for_gsd(gsd_a).round() as u8,
        4,
        "fixture premise: gsd_a must derive to z4"
    );
    assert_eq!(
        zoom_for_gsd(gsd_b).round() as u8,
        4,
        "fixture premise: gsd_b must derive to z4"
    );

    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let schema = Arc::new(source_schema());
    // No explicit zoom on either level: both derive from gsd, and both
    // derive to z4.
    let specs = vec![LevelSpec::new(gsd_a, None), LevelSpec::new(gsd_b, None)];
    let tin = tempfile::NamedTempFile::new().unwrap();
    let mut writer = OverviewWriter::create(
        tin.path(),
        &schema,
        OverviewWriterOptions::new(Mode::Duplicating, specs),
    )
    .unwrap();
    for k in 0..2 {
        assert_eq!(
            writer
                .write_level(
                    k,
                    Some(1),
                    std::iter::once(batch(&schema, &[0], std::slice::from_ref(&a))),
                )
                .unwrap(),
            LevelWriteOutcome::Written
        );
    }
    writer.finish().unwrap();

    let tout = tempfile::NamedTempFile::new().unwrap();
    let err = export_pmtiles(tin.path(), tout.path(), &ExportOptions::default())
        .expect_err("two levels deriving the same zoom must be refused");
    assert!(
        matches!(
            err,
            ExportError::LevelZoomsNotAscending {
                previous_level: 0,
                previous_zoom: 4,
                level: 1,
                zoom: 4,
                ..
            }
        ),
        "got: {err}"
    );
}

// --- property selection at export (#386) --------------------------------

/// Export the plain two-column fixture (`id`, `name`) under a selection
/// and return the property names the tiles carry.
fn exported_names_with(selection: PropertySelection) -> Result<Vec<String>, ExportError> {
    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let b = Geometry::Point(Point::new(120.0, -40.0));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[
            (vec![0, 1], vec![a.clone(), b.clone()]),
            (vec![0, 1], vec![a.clone(), b.clone()]),
        ],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        properties: selection,
        ..ExportOptions::default()
    };
    export_pmtiles(tin.path(), tout.path(), &opts)?;
    let decoded = tempfile::NamedTempFile::new().unwrap();
    crate::decode::decode_pmtiles(
        tout.path(),
        decoded.path(),
        &crate::decode::DecodeOptions::default(),
    )
    .unwrap();
    let file = std::fs::File::open(decoded.path()).unwrap();
    Ok(tylertoo_io::open_parquet(file)
        .unwrap()
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .filter(|n| {
            !matches!(
                n.as_str(),
                "zoom" | "layer" | "mvt_id" | "geometry" | "bbox"
            )
        })
        .collect())
}

#[test]
fn export_property_selection_include_exclude_and_none() {
    let include = PropertySelection {
        include: Some(vec!["name".to_string()]),
        ..Default::default()
    };
    assert_eq!(exported_names_with(include).unwrap(), vec!["name"]);

    let exclude = PropertySelection {
        exclude: vec!["name".to_string()],
        ..Default::default()
    };
    assert_eq!(exported_names_with(exclude).unwrap(), vec!["id"]);

    let none = PropertySelection {
        exclude_all: true,
        ..Default::default()
    };
    assert!(exported_names_with(none).unwrap().is_empty());
}

#[test]
fn export_property_selection_rejects_unknown_include() {
    let typo = PropertySelection {
        include: Some(vec!["nmae".to_string()]),
        ..Default::default()
    };
    let msg = exported_names_with(typo).unwrap_err().to_string();
    assert!(
        msg.contains("\"nmae\"") && msg.contains("\"name\""),
        "{msg}"
    );
}

#[test]
fn export_property_selection_cannot_drop_the_feature_order_column() {
    let a = Geometry::Point(Point::new(-120.0, 40.0));
    let tin = tempfile::NamedTempFile::new().unwrap();
    write_fixture(
        tin.path(),
        &[(vec![0], vec![a.clone()]), (vec![0], vec![a])],
    );
    let tout = tempfile::NamedTempFile::new().unwrap();
    let opts = ExportOptions {
        feature_order: FeatureOrder::Column {
            name: "id".to_string(),
            descending: false,
        },
        properties: PropertySelection {
            exclude: vec!["id".to_string()],
            ..Default::default()
        },
        ..ExportOptions::default()
    };
    let msg = export_pmtiles(tin.path(), tout.path(), &opts)
        .unwrap_err()
        .to_string();
    assert!(
        msg.contains("--feature-order") && msg.contains("\"id\""),
        "{msg}"
    );
}

// ========================================================================
// `--feature-id` (#443): stable MVT feature ids
// ========================================================================
// Value conversion and column resolution are unit-tested in
// `export/feature_id.rs`; these exercise the whole export.

/// The encode path: a feature's `--feature-id` value becomes the MVT
/// feature id at every zoom it is encoded at, and the id column is never
/// also published as a regular tag.
#[test]
fn feature_id_reaches_the_decoded_tile_at_every_zoom_and_is_not_duplicated() {
    let feats = vec![
        Feature {
            geom: Geometry::Point(Point::new(-120.0, 40.0)),
            props: vec![
                ("id".to_string(), PropertyValue::Int(4242)),
                ("name".to_string(), PropertyValue::String("a".to_string())),
            ],
        },
        Feature {
            geom: Geometry::Point(Point::new(120.0, -40.0)),
            props: vec![
                ("id".to_string(), PropertyValue::UInt(7)),
                ("name".to_string(), PropertyValue::String("b".to_string())),
            ],
        },
    ];
    let opts = ExportOptions {
        feature_id: Some("id".to_string()),
        ..Default::default()
    };
    for zoom in [2u8, 4u8] {
        let tiles = encode_level_tiles(&feats, zoom, &opts).unwrap();
        let mut seen_ids: Vec<u64> = Vec::new();
        for t in &tiles {
            let decoded = decode_tile(&t.data);
            let keys = &decoded.layers[0].keys;
            for f in &decoded.layers[0].features {
                seen_ids.push(f.id.expect("feature-id column must produce an MVT id"));
                let tag_names: Vec<&str> = f
                    .tags
                    .iter()
                    .step_by(2)
                    .map(|&k| keys[k as usize].as_str())
                    .collect();
                assert!(
                    !tag_names.contains(&"id"),
                    "the feature-id column must not also be a tile property at z{zoom}"
                );
                assert!(
                    tag_names.contains(&"name"),
                    "other properties must still be published at z{zoom}"
                );
            }
        }
        seen_ids.sort_unstable();
        assert_eq!(
            seen_ids,
            vec![7, 4242],
            "both feature ids present at z{zoom}"
        );
    }
}

fn two_point_fixture(path: &Path) {
    write_fixture(
        path,
        &[(
            vec![1, 2],
            vec![
                Geometry::Point(Point::new(-120.0, 40.0)),
                Geometry::Point(Point::new(120.0, -40.0)),
            ],
        )],
    );
}

fn export_with_feature_id(input: &Path, opts: ExportOptions) -> Result<ExportReport, ExportError> {
    let out = tempfile::NamedTempFile::new().unwrap();
    export_pmtiles(input, out.path(), &opts)
}

/// An id column also named in `--feature-order` is rejected up front
/// rather than silently breaking the sort.
#[test]
fn feature_id_conflicts_with_feature_order_on_the_same_column() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    two_point_fixture(tmp.path());
    let err = export_with_feature_id(
        tmp.path(),
        ExportOptions {
            feature_id: Some("id".to_string()),
            feature_order: FeatureOrder::Column {
                name: "id".to_string(),
                descending: false,
            },
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        ExportError::FeatureIdColumn {
            column,
            reason: FeatureIdColumnProblem::ConflictsWithFeatureOrder,
        } if column == "id"
    ));
}

/// An unknown column and a non-integer column (the fixture's string
/// `name`) are clear errors, not a silent fallback.
#[test]
fn feature_id_unknown_or_non_integer_column_is_rejected() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    two_point_fixture(tmp.path());
    let with = |name: &str| ExportOptions {
        feature_id: Some(name.to_string()),
        ..Default::default()
    };
    let err = export_with_feature_id(tmp.path(), with("nope")).unwrap_err();
    assert!(
        matches!(
            &err,
            ExportError::FeatureIdColumn {
                column,
                reason: FeatureIdColumnProblem::NotFound { available },
            } if column == "nope" && available == "\"id\", \"name\""
        ),
        "{err:?}"
    );
    let err = export_with_feature_id(tmp.path(), with("name")).unwrap_err();
    assert!(
        matches!(
            &err,
            ExportError::FeatureIdColumn {
                column,
                reason: FeatureIdColumnProblem::NotInteger { .. },
            } if column == "name"
        ),
        "{err:?}"
    );
    assert!(err.to_string().contains("gpio or DuckDB"), "{err}");
}

/// `--feature-id` naming a column also listed in `--exclude-property`
/// still works: tippecanoe extracts the id attribute before its own
/// filter runs, so this must be a harmless no-op, not an error.
#[test]
fn feature_id_column_named_in_exclude_property_still_works() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    two_point_fixture(tmp.path());
    let report = export_with_feature_id(
        tmp.path(),
        ExportOptions {
            feature_id: Some("id".to_string()),
            properties: PropertySelection {
                exclude: vec!["id".to_string()],
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    assert!(report.total_tiles > 0);
}

/// Write an overview whose `id` column is nullable Int64, one batch per
/// level, in `mode`, with two-row row groups (so several row groups per
/// level, some of which the statistics short-circuit can skip).
fn write_nullable_id_fixture(path: &Path, mode: Mode, levels: &[Vec<Option<i64>>]) {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("name", DataType::Utf8, false),
        geometry_field(),
    ]));
    let specs: Vec<LevelSpec> = (0..levels.len())
        .map(|k| LevelSpec::new(gsd((2 + 2 * k) as u8), Some((2 + 2 * k) as u8)))
        .collect();
    let mut opts = OverviewWriterOptions::new(mode, specs);
    opts.max_row_group_size = 2;
    let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
    let mut n = 0usize;
    for (k, ids) in levels.iter().enumerate() {
        let geoms: Vec<Geometry<f64>> = (0..ids.len())
            .map(|i| {
                let t = (n + i) as f64;
                Geometry::Point(Point::new(-150.0 + t * 7.0, -60.0 + t * 5.0))
            })
            .collect();
        n += ids.len();
        let names: Vec<String> = (0..ids.len()).map(|i| format!("f{k}_{i}")).collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(ids.clone())),
                Arc::new(StringArray::from(names)),
                Arc::new(build_geometry_array(&geoms).to_array_ref()),
            ],
        )
        .unwrap();
        assert_eq!(
            writer
                .write_level(k, Some(ids.len()), std::iter::once(batch))
                .unwrap(),
            LevelWriteOutcome::Written
        );
    }
    writer.finish().unwrap();
}

/// S2-2: a bad id anywhere in the file -- here in the FINEST level --
/// fails the export before the output is created, naming the true
/// overview-file row and its level, in both modes and on both pass-2
/// engines (the partitioning single-read fan-out and the legacy per-wave
/// read).
#[test]
fn feature_id_bad_value_fails_up_front_with_the_overview_row_and_level() {
    let good = |base: i64, n: i64| (base..base + n).map(Some).collect::<Vec<_>>();
    let cases: [(Option<i64>, InvalidFeatureIdReason); 2] = [
        (None, InvalidFeatureIdReason::Null),
        (
            Some(-9),
            InvalidFeatureIdReason::Negative {
                value: "-9".to_string(),
            },
        ),
    ];
    for mode in [Mode::Duplicating, Mode::Partitioning] {
        for (bad, reason) in cases.clone() {
            // Level 0: 3 rows, level 1: 4 rows, level 2: 5 rows with the
            // bad value at index 3 => overview-file row 3 + 4 + 3 = 10.
            let mut finest = good(100, 5);
            finest[3] = bad;
            let levels = vec![good(0, 3), good(10, 4), finest];
            let dir = tempfile::tempdir().unwrap();
            let input = dir.path().join("ov.parquet");
            write_nullable_id_fixture(&input, mode, &levels);
            let opts = ExportOptions {
                feature_id: Some("id".to_string()),
                ..Default::default()
            };
            for legacy in [false, true] {
                let out = dir.path().join(format!("out-{legacy}.pmtiles"));
                let err = export_pmtiles_impl(
                    &input,
                    &out,
                    &opts,
                    DEFAULT_PARTITION_TARGET,
                    legacy,
                    None,
                )
                .unwrap_err();
                match &err {
                    ExportError::InvalidFeatureId {
                        column,
                        level,
                        zoom,
                        row,
                        reason: got,
                    } => {
                        assert_eq!(column, "id");
                        assert_eq!((*level, *zoom, *row), (2, 6, 10), "{mode:?}: {err}");
                        assert_eq!(got, &reason);
                    }
                    other => panic!("{mode:?}: unexpected {other:?}"),
                }
                assert!(err.to_string().contains("row 10 of the overview file"));
                let leftovers: Vec<_> = std::fs::read_dir(dir.path())
                    .unwrap()
                    .map(|e| e.unwrap().file_name().into_string().unwrap())
                    .filter(|n| n.starts_with("out-"))
                    .collect();
                assert!(
                    leftovers.is_empty(),
                    "{mode:?}: nothing may be written before the check fails: {leftovers:?}"
                );
            }
        }
    }
}

/// The same fixture with only valid ids exports, and the ids survive the
/// partitioning single-read fan-out under both member-store backings
/// (the spill codec carries them) byte-identically to the legacy path.
#[test]
fn pruned_export_waves_keep_distinct_source_row_addresses() {
    let input = tempfile::NamedTempFile::new().unwrap();
    let tiny = geo::Rect::new(
        geo::Coord { x: 120.0, y: -40.0 },
        geo::Coord {
            x: 120.000001,
            y: -39.999999,
        },
    )
    .to_polygon();
    write_mode_fixture(
        input.path(),
        &[
            (
                vec![100, 200],
                vec![
                    Geometry::Point(Point::new(-120.0, 40.0)),
                    Geometry::Polygon(tiny.clone()),
                ],
            ),
            (
                vec![100, 200],
                vec![
                    Geometry::Point(Point::new(-120.0, 40.0)),
                    Geometry::Polygon(tiny),
                ],
            ),
        ],
        Mode::Duplicating,
        1,
    );
    let reader = OverviewReader::open(input.path()).unwrap();
    assert_eq!(reader.num_row_groups(), 4);
    assert_eq!(
        reader
            .selected_row_groups(1, Some([-157.5, -90.0, -90.0, 156.0]))
            .unwrap(),
        vec![2]
    );
    let output = tempfile::NamedTempFile::new().unwrap();
    let report = export_pmtiles_impl(
        input.path(),
        output.path(),
        &ExportOptions {
            feature_id: Some("id".into()),
            partition_wave: 1,
            ..Default::default()
        },
        1,
        true,
        None,
    )
    .unwrap();
    let losses: Vec<_> = report
        .source_losses
        .iter()
        .map(Result::unwrap)
        .filter(|loss| loss.zoom == Some(4))
        .collect();
    assert_eq!(losses.len(), 1);
    assert_eq!(losses[0].id, 3);
    assert_eq!(losses[0].feature_id, Some(200));
}

#[test]
fn feature_id_partitioning_single_read_matches_legacy_under_both_backings() {
    let tin = tempfile::NamedTempFile::new().unwrap();
    partitioning_equivalence_fixture(tin.path());
    let opts = ExportOptions {
        feature_id: Some("id".to_string()),
        partition_wave: 2,
        ..Default::default()
    };
    let t_legacy = tempfile::NamedTempFile::new().unwrap();
    export_pmtiles_impl(tin.path(), t_legacy.path(), &opts, 1, true, None).unwrap();
    let legacy_bytes = std::fs::read(t_legacy.path()).unwrap();
    let t_plain = tempfile::NamedTempFile::new().unwrap();
    export_pmtiles_impl(
        tin.path(),
        t_plain.path(),
        &ExportOptions {
            feature_id: None,
            ..opts.clone()
        },
        1,
        true,
        None,
    )
    .unwrap();
    assert_ne!(
        std::fs::read(t_plain.path()).unwrap(),
        legacy_bytes,
        "--feature-id must change the tiles (ids + the id tag moved out)"
    );
    for backing in [SinkBacking::Ram, SinkBacking::Spill] {
        let t_new = tempfile::NamedTempFile::new().unwrap();
        export_pmtiles_impl(tin.path(), t_new.path(), &opts, 1, false, Some(backing)).unwrap();
        assert_eq!(
            std::fs::read(t_new.path()).unwrap(),
            legacy_bytes,
            "single-read ({backing:?}) diverges from legacy with --feature-id"
        );
    }
}

/// A schema of `(name, is_geometry)` fields, with a `geo` key naming
/// `primary` when given.
fn geometry_schema(fields: &[(&str, bool)], primary: Option<&str>) -> Schema {
    let fields: Vec<Field> = fields
        .iter()
        .map(|&(name, geom)| {
            if geom {
                GeometryType::new(Default::default()).to_field(name, true)
            } else {
                Field::new(name, DataType::Utf8, true)
            }
        })
        .collect();
    let metadata = primary
        .map(|p| {
            let json = format!(r#"{{"version":"1.1.0","primary_column":"{p}","columns":{{}}}}"#);
            HashMap::from([("geo".to_string(), json)])
        })
        .unwrap_or_default();
    Schema::new_with_metadata(fields, metadata)
}

/// #675: the file schema resolves by `primary_column`, wherever the
/// column sits and whatever it is called, and only then by name.
#[test]
fn geometry_index_follows_primary_column_then_the_name_heuristic() {
    let primary_second = geometry_schema(
        &[
            ("geom_type", false),
            ("centroid", true),
            ("footprint", true),
        ],
        Some("footprint"),
    );
    assert_eq!(
        geometry_index(&primary_second),
        Some(2),
        "primary_column, not the first geoarrow field"
    );
    let shape = geometry_schema(&[("geom_type", false), ("SHAPE", true)], Some("SHAPE"));
    assert_eq!(geometry_index(&shape), Some(1));
    // No `geo` key and no extension metadata: the name heuristic.
    let bare = geometry_schema(&[("name", false), ("geometry", false)], None);
    assert_eq!(geometry_index(&bare), Some(1), "`geometry` by name");
    let geom_prefix = geometry_schema(&[("name", false), ("the_geom", false)], None);
    assert_eq!(
        geometry_index(&geom_prefix),
        Some(1),
        "first `geom*` by name"
    );
    let none = geometry_schema(&[("id", false), ("name", false)], None);
    assert_eq!(geometry_index(&none), None, "no geometry column at all");
}

/// #675: a batch's schema has lost the file's `geo` key, so it is looked
/// up by the name the file schema resolved, not re-resolved: a fresh
/// heuristic would take `geom_type` over `SHAPE`.
#[test]
fn batch_geometry_index_uses_the_file_resolved_name() {
    let file = geometry_schema(&[("geom_type", false), ("SHAPE", true)], Some("SHAPE"));
    let batch_schema = Arc::new(Schema::new(file.fields().clone()));
    assert_eq!(
        geometry_index(&batch_schema),
        Some(0),
        "the heuristic misfires"
    );
    let batch = RecordBatch::new_empty(batch_schema);
    assert_eq!(batch_geometry_index(&batch, Some("SHAPE")).unwrap(), 1);
    assert!(matches!(
        batch_geometry_index(&batch, None),
        Err(ExportError::NoGeometryColumn { path: None })
    ));
    assert!(matches!(
        batch_geometry_index(&batch, Some("geometry")),
        Err(ExportError::NoGeometryColumn { path: None })
    ));
}

/// #675 review: a footer naming a different geometry column than the
/// name heuristic is flagged; an attribute the heuristic would pick, or
/// agreement, is not.
#[test]
fn geometry_name_disagreement_flags_only_another_geometry_column() {
    let stale = geometry_schema(&[("centroid", true), ("geom", true)], Some("centroid"));
    assert_eq!(geometry_name_disagreement(&stale, 0), Some("geom"));
    let pinned = geometry_schema(&[("centroid", true), ("geom", true)], Some("geom"));
    assert_eq!(geometry_name_disagreement(&pinned, 1), None);
    let shape = geometry_schema(&[("geom_type", false), ("SHAPE", true)], Some("SHAPE"));
    assert_eq!(
        geometry_name_disagreement(&shape, 1),
        None,
        "geom_type is no geometry"
    );
    let lone = geometry_schema(&[("id", false), ("footprint", true)], Some("footprint"));
    assert_eq!(geometry_name_disagreement(&lone, 1), None);
}

// -------------------------------------------------------------------------
// Publication metadata (name / description / attribution)
// -------------------------------------------------------------------------

/// The archive's metadata JSON object, read from the header's offsets.
fn metadata_object(bytes: &[u8]) -> serde_json::Map<String, serde_json::Value> {
    let header = crate::pmtiles_writer::Header::from_bytes(bytes).unwrap();
    let start = header.json_metadata_offset as usize;
    let raw = &bytes[start..start + header.json_metadata_length as usize];
    let plain = compression::decompress(raw, header.internal_compression).unwrap();
    serde_json::from_slice(&plain).expect("archive metadata must be a JSON object")
}

/// Every addressed tile's stored (compressed) bytes, by tile id.
fn stored_tiles(bytes: &[u8]) -> std::collections::BTreeMap<u64, Vec<u8>> {
    use crate::pmtiles_writer::{read_all_entries, Header};
    let header = Header::from_bytes(bytes).unwrap();
    let mut out = std::collections::BTreeMap::new();
    for e in read_all_entries(bytes, &header).unwrap() {
        let start = (header.tile_data_offset + e.offset) as usize;
        for r in 0..u64::from(e.run_length) {
            out.insert(
                e.tile_id + r,
                bytes[start..start + e.length as usize].to_vec(),
            );
        }
    }
    out
}

const PUBLICATION_KEYS: [&str; 3] = ["name", "description", "attribution"];

fn park_metadata() -> TilesetMetadata {
    TilesetMetadata::new()
        .with_name("Park Tiles 4")
        .with_description("")
        .with_attribution(r#"<a href="https://www.nps.gov/">National Park Service</a> ©"#)
}

fn assert_park_metadata(map: &serde_json::Map<String, serde_json::Value>, ctx: &str) {
    assert_eq!(map["name"], "Park Tiles 4", "{ctx}");
    assert_eq!(map["description"], "", "{ctx}");
    assert_eq!(
        map["attribution"], r#"<a href="https://www.nps.gov/">National Park Service</a> ©"#,
        "{ctx}"
    );
}

#[test]
fn export_writes_publication_metadata_without_touching_tiles() {
    let input = tempfile::NamedTempFile::new().unwrap();
    level_group_fixture(input.path(), Mode::Duplicating);
    let dir = tempfile::tempdir().unwrap();
    let plain = ExportOptions {
        layer_name: "parks".to_string(),
        ..Default::default()
    };
    let published = ExportOptions {
        metadata: park_metadata(),
        ..plain.clone()
    };
    let (a, b) = (dir.path().join("a.pmtiles"), dir.path().join("b.pmtiles"));
    let report_a = export_pmtiles(input.path(), &a, &plain).unwrap();
    let report_b = export_pmtiles(input.path(), &b, &published).unwrap();
    assert_eq!(
        report_a.zooms, report_b.zooms,
        "publication metadata changes no tile"
    );

    let (a, b) = (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    let (mut meta_a, mut meta_b) = (metadata_object(&a), metadata_object(&b));
    for key in PUBLICATION_KEYS {
        assert!(!meta_a.contains_key(key), "default export adds no {key}");
    }
    assert_park_metadata(&meta_b, "export");
    for key in PUBLICATION_KEYS {
        meta_b.remove(key);
    }
    meta_a.remove("generator");
    meta_b.remove("generator");
    assert_eq!(meta_a, meta_b, "structural metadata keeps its meaning");
    assert_eq!(
        stored_tiles(&a),
        stored_tiles(&b),
        "tile payloads unchanged"
    );
}

#[test]
fn interrupted_export_salvage_keeps_publication_metadata() {
    let input = tempfile::NamedTempFile::new().unwrap();
    level_group_fixture(input.path(), Mode::Duplicating);
    let opts = ExportOptions {
        layer_name: "groups".to_string(),
        partition_wave: 6,
        metadata: park_metadata(),
        ..Default::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("failed.pmtiles");
    let err = export_pmtiles_run(
        input.path(),
        &out,
        &opts,
        DEFAULT_PARTITION_TARGET,
        &Pass2Knobs {
            fail_level: Some(3),
            ..knobs(true, None, false)
        },
    )
    .err()
    .expect("the injected fault must fail the export");
    assert!(err.to_string().contains("injected"), "{err}");
    let partial = std::fs::read(out.with_extension("pmtiles.partial")).unwrap();
    assert_park_metadata(&metadata_object(&partial), "checkpoint");
}

#[test]
fn empty_archive_helpers_write_requested_metadata_only() {
    let dir = tempfile::tempdir().unwrap();
    let (plain, published) = (
        dir.path().join("plain.pmtiles"),
        dir.path().join("published.pmtiles"),
    );
    write_empty_archive(
        &plain,
        "parks",
        3,
        9,
        &TilesetMetadata::default(),
        TileFormat::Mvt,
    )
    .unwrap();
    write_empty_archive(&published, "parks", 3, 9, &park_metadata(), TileFormat::Mvt).unwrap();

    let mut plain = metadata_object(&std::fs::read(&plain).unwrap());
    let mut published = metadata_object(&std::fs::read(&published).unwrap());
    for key in PUBLICATION_KEYS {
        assert!(
            !plain.contains_key(key),
            "default empty archive adds no {key}"
        );
    }
    assert_park_metadata(&published, "empty archive");
    for key in PUBLICATION_KEYS {
        published.remove(key);
    }
    plain.remove("generator");
    published.remove("generator");
    assert_eq!(
        plain, published,
        "declared layer and zooms keep their meaning"
    );
}

#[test]
fn oversized_publication_metadata_fails_before_reading_input() {
    let opts = ExportOptions {
        // Fits before JSON escaping, not after: each quote becomes `\"`.
        metadata: TilesetMetadata::new()
            .with_description("\"".repeat(compression::MAX_INTERNAL_BYTES as usize / 2)),
        ..Default::default()
    };
    let err = opts.validate().unwrap_err();
    assert!(err.to_string().contains("description"), "{err}");

    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-overview.parquet");
    let out = dir.path().join("out.pmtiles");
    let err = export_pmtiles(&missing, &out, &opts).unwrap_err();
    assert!(err.to_string().contains("description"), "{err}");
    assert!(!out.exists());

    let err =
        write_empty_archive(&out, "parks", 0, 4, &opts.metadata, TileFormat::Mvt).unwrap_err();
    assert!(err.to_string().contains("description"), "{err}");
    assert!(!out.exists() && !out.with_extension("pmtiles.partial").exists());
}

/// #750: the coverage fill seeded one zoom below the finest level is
/// lifted zoom by zoom to the coarsest level, and every coarser level
/// draws it. The pyramid's per-zoom line reports the area it built.
#[test]
fn coverage_fill_is_lifted_to_every_coarser_level() {
    let logging = crate::test_log::install();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("overview.parquet");
    let host = Geometry::Point(Point::new(12.0, 42.0));
    let field = Geometry::Polygon(
        geo::Rect::new(
            geo::coord! { x: 10.0, y: 40.0 },
            geo::coord! { x: 15.0, y: 45.0 },
        )
        .to_polygon(),
    );
    // Levels at z2, z4 and z6: the fill seeds at z5 from the z6 field.
    write_fixture(
        &path,
        &[
            (vec![0], vec![host.clone()]),
            (vec![0], vec![host]),
            (vec![1], vec![field]),
        ],
    );
    crate::test_log::take_thread_lines();
    let report = export_pmtiles(
        &path,
        dir.path().join("out.pmtiles"),
        &ExportOptions {
            coverage_fill: true,
            ..ExportOptions::default()
        },
    )
    .unwrap();
    let fill = |zoom: u8| {
        report
            .zooms
            .iter()
            .find(|z| z.zoom == zoom)
            .map(|z| z.fill_polygons)
    };
    assert!(fill(2) > Some(0), "{:?}", report.zooms);
    assert!(fill(4) > Some(0), "{:?}", report.zooms);
    if logging {
        let lines = crate::test_log::take_thread_lines();
        let z2 = lines
            .iter()
            .find(|l| l.contains("coverage fill z2:"))
            .expect("the pyramid logs z2");
        assert!(
            z2.contains("z2: 1 tiles") && !z2.contains("area 0 "),
            "{z2}"
        );
        let summary = lines
            .iter()
            .find(|l| l.contains("vertices lifted"))
            .expect("the fill logs its summary");
        let vertices: usize = summary
            .split(" vertices lifted")
            .next()
            .and_then(|head| head.rsplit(' ').next())
            .and_then(|n| n.parse().ok())
            .expect("a vertex count");
        assert!(vertices > 0, "{summary}");
    }
}

/// A level store backing the caller forced is used as is, and the
/// budget grants it nothing.
#[test]
fn a_forced_level_store_backing_is_used_as_is() {
    let plan = LevelPlan::cut(4, partitions_with_members(&[10, 10]), 1, usize::MAX);
    let mut budget = MemoryBudget::with_available(Some(1 << 30));
    let read = LevelRead {
        forced: Some(SinkBacking::Ram),
        ram_budget: 7,
        member_bytes: Some(100),
        profile: MemoryProfile::Bounded,
    };
    let (backing, grants) = level_read_backing(read, &plan, &mut budget, 0);
    assert!(matches!(
        backing,
        StoreBacking {
            backing: SinkBacking::Ram,
            ram_budget: 7,
            resident: 0
        }
    ));
    assert!(grants.is_empty());
}

/// A level its grant covers only in part spills, keeping the granted
/// bytes resident.
#[test]
fn a_partly_granted_level_store_keeps_its_grant_resident() {
    let plan = LevelPlan::cut(4, partitions_with_members(&[1000, 1000]), 1, usize::MAX);
    let read = LevelRead {
        forced: None,
        ram_budget: 7,
        member_bytes: Some(100),
        profile: MemoryProfile::Auto,
    };
    // A 300 kB budget grants the 1,000-member wave and 666 of the
    // level's 2,000 members.
    let mut budget = MemoryBudget::with_available(Some(300_000));
    let (backing, grants) = level_read_backing(read, &plan, &mut budget, 0);
    assert!(matches!(
        backing,
        StoreBacking {
            backing: SinkBacking::Spill,
            ram_budget: 7,
            resident: 66_600
        }
    ));
    assert_eq!(grants[1].bytes(), 66_600);
}

/// A level whose footer prices its members at zero bytes has no member
/// size to grant from: it spills whole and holds no grant.
#[test]
fn a_level_with_zero_byte_members_spills_whole() {
    let plan = LevelPlan::cut(4, partitions_with_members(&[1000, 1000]), 1, usize::MAX);
    let read = LevelRead {
        forced: None,
        ram_budget: 7,
        member_bytes: Some(0),
        profile: MemoryProfile::Auto,
    };
    let mut budget = MemoryBudget::with_available(Some(300_000));
    let (backing, grants) = level_read_backing(read, &plan, &mut budget, 0);
    assert!(matches!(
        backing,
        StoreBacking {
            backing: SinkBacking::Spill,
            ram_budget: 7,
            resident: 0
        }
    ));
    assert!(grants.is_empty());
}

/// A batch's collect work is cut into one chunk per pool thread.
#[test]
fn collect_chunks_are_one_per_pool_thread() {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .build()
        .unwrap();
    let rows = pool.install(|| [1, 2, 9, 400].map(collect_chunk_rows));
    assert_eq!(rows, [1, 1, 3, 100]);
}

/// The consumer-thread guard counts every row a chunk collects outside
/// the pool, so a regression to the serial loop would show.
#[test]
fn a_chunk_collected_off_the_pool_is_counted() {
    let schema = Arc::new(Schema::new(vec![
        Field::new("rank", DataType::Int64, true),
        Field::new("geometry", DataType::Binary, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(Int64Array::from(vec![Some(1), Some(2), Some(3)])) as ArrayRef,
            Arc::new(arrow_array::BinaryArray::from(vec![None::<&[u8]>; 3])),
        ],
    )
    .unwrap();
    let published = PublishedNames::identity();
    let attrs = AttrColumns::new(&schema, 1, &published, None);
    let work = Mutex::new(ExportWorkTally::default());
    let locator = crate::input_set::StreamRowLocator::new(&[vec![3]], None);
    let input = ChunkInput {
        work: &work,
        wave: &[],
        batch: &batch,
        attrs: &attrs,
        row_gaps: &[(0, 0); 3],
        locator: &locator,
        first_read: 0,
    };
    let mut items: Vec<TileMembers> = (0..3).map(|_| Vec::new()).collect();
    assert!(rayon::current_thread_index().is_none());
    collect_member_chunk(&input, 0, &mut items).unwrap();
    collect_member_chunk(&input, 1, &mut items[1..]).unwrap();
    assert_eq!(work.lock().unwrap().serial_collect_rows, 5);
    // Every row was memberless: none built its properties (Codex C2).
    assert_eq!(PROPS_BUILT.with(std::cell::Cell::get), 0);
}

/// A flushed bucket is written as whole [`SPILL_SEGMENT_MEMBERS`]-member
/// segments, whatever the pool's thread count.
#[test]
fn a_flush_writes_whole_segments_on_any_pool() {
    for threads in [1, 2, 4] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        let segments = pool.install(|| {
            let plans = vec![LevelPlan::cut(
                4,
                partitions_with_members(&[10]),
                1,
                usize::MAX,
            )];
            let mut store =
                MemberStore::new(&plans, SinkBacking::Spill, MEMBER_STORE_RAM_BUDGET, None)
                    .unwrap();
            store.buckets[0][0] = (0..2 * SPILL_SEGMENT_MEMBERS as u64)
                .map(|seq| Member {
                    key: 0,
                    seq,
                    rank: point_rank(seq),
                    geom: Geometry::Point(Point::new(seq as f64, 0.0)).into(),
                    props: SharedProps::new(Vec::new()),
                    id: None,
                    gap: GAP_NONE,
                    band: 0,
                })
                .collect();
            store.flush_bucket(0, 0).unwrap();
            store.spill.as_ref().unwrap().segments[0][0]
                .iter()
                .map(|s| s.members)
                .collect::<Vec<_>>()
        });
        assert_eq!(
            segments, [SPILL_SEGMENT_MEMBERS; 2],
            "{threads} pool thread(s)"
        );
    }
}

/// With no `--feature-id`, even later parallel chunks omit feature ids.
#[test]
fn build_mvt_omits_ids_past_the_first_parallel_chunk() {
    let tb = TileCoord::new(0, 0, 1).bounds();
    let n = 4096 + 3;
    let members: Vec<Member> = (0..n)
        .map(|i| Member {
            key: 0,
            seq: i as u64,
            rank: point_rank(i as u64),
            geom: Geometry::Point(Point::new(
                -170.0 + (i % 300) as f64 * 0.5,
                10.0 + (i / 300) as f64 * 0.5,
            ))
            .into(),
            props: SharedProps::new(Vec::new()),
            id: None,
            gap: GAP_NONE,
            band: 0,
        })
        .collect();
    assert!(encode_in_parallel(&members));
    let built = build_mvt(
        &test_gzip_cache(),
        &members,
        &tb,
        &ExportOptions::default(),
        &[],
        &crate::diagnostics::Diagnostics::default(),
    );
    let ids: Vec<Option<u64>> = decode_tile(&built.data).layers[0]
        .features
        .iter()
        .map(|f| f.id)
        .collect();
    assert_eq!(ids, vec![None; n]);
}

/// The stage submodules log as `overview::export` (#916), so `RUST_LOG`
/// filters, CLI stderr and Python logger names stayed what they were.
#[test]
fn stage_logs_keep_the_export_target() {
    assert_eq!(LOG_TARGET, "tylertoo_core::overview::export");
}
