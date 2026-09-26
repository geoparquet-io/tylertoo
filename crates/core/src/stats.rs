//! Per-zoom tile-weight report for a PMTiles archive (#552).
//!
//! Every "which zoom is blowing our tile budget" question this week (the
//! handover zoom, the 600 KB cells budget, r5/r7/r8 comparisons) needed
//! per-zoom stored-tile-size stats. This walks an already-opened
//! [`ArchiveIndex`]'s directory entries — header + directories, never a tile
//! body — so an entry's length is the tile's STORED (compressed) size
//! directly. No tile is decompressed and the archive is never read whole
//! into memory.
//!
//! Aggregation is per directory entry, with multiplicity: a run of `k`
//! identical tiles contributes one `(stored_len, k)` pair (split at zoom
//! boundaries when the run crosses one), and every statistic is weighted by
//! those counts. Run lengths are never expanded, so memory and time are
//! O(directory entries), not O(addressed tiles), and the report is not
//! subject to [`ArchiveIndex::tiles`]' run-length expansion ceiling — a
//! mostly-ocean archive whose runs address billions of tiles reports as
//! cheaply as its directory reads.
//!
//! Sizes are per ADDRESSED tile: every member of a run, and every tile that
//! deduplicates onto another's body, counts at the full length of the shared
//! body. So `tile_count × mean_bytes` (= `total_bytes`) overstates the
//! archive's tile-data section whenever runs or deduplication are present —
//! it is the bytes a client would fetch, not the bytes on disk.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::archive_index::ArchiveIndex;
use crate::pmtiles_writer::tile_id_to_zxy;
use crate::{Error, Result};

/// One zoom level's tile-size distribution, in STORED (compressed) bytes per
/// addressed tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct ZoomStats {
    pub z: u8,
    /// Addressed tiles at this zoom (run members counted individually).
    pub tile_count: u64,
    /// Sum of stored sizes over addressed tiles (saturating at `u64::MAX`).
    /// Run members and deduplicated tiles each count at the shared body's
    /// full length, so this can exceed the bytes actually on disk.
    pub total_bytes: u64,
    /// Mean stored size, rounded down to the nearest byte.
    pub mean_bytes: u64,
    /// 50th percentile, nearest-rank: the smallest size `s` such that at
    /// least `ceil(0.50 × tile_count)` tiles are `<= s`.
    pub p50_bytes: u64,
    /// 99th percentile, nearest-rank (see [`Self::p50_bytes`]).
    pub p99_bytes: u64,
    pub max_bytes: u64,
}

/// One of the `--largest` biggest tiles in the archive, by stored size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[non_exhaustive]
pub struct LargestTile {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub bytes: u64,
}

/// Per-zoom tile-weight report: [`ZoomStats`] for every zoom the archive
/// holds at least one tile at (ascending by zoom), plus the largest tiles
/// overall (descending by size, ties by ascending PMTiles tile id — i.e.
/// lower zoom first, then Hilbert order within a zoom).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
#[non_exhaustive]
pub struct StatsReport {
    pub per_zoom: Vec<ZoomStats>,
    pub largest: Vec<LargestTile>,
}

/// First PMTiles tile id at zoom `z`: `(4^z - 1) / 3`. Valid for `z <= 32`
/// (the value at 32 is the one-past-the-end of z31's block).
fn zoom_base(z: u8) -> u64 {
    debug_assert!(z <= 32);
    (((1u128 << (2 * u32::from(z))) - 1) / 3) as u64
}

/// Nearest-rank percentile over `(size, count)` pairs sorted ascending by
/// size. `p` is `1..=100`; `n` is the total count (> 0). The 1-based rank is
/// `max(1, ceil(p/100 × n))`; the answer is the size at which the cumulative
/// count first reaches it.
fn weighted_percentile(sorted: &[(u64, u64)], n: u64, p: u64) -> u64 {
    let rank = ((u128::from(p) * u128::from(n)).div_ceil(100)).max(1);
    let mut cum: u128 = 0;
    for &(size, count) in sorted {
        cum += u128::from(count);
        if cum >= rank {
            return size;
        }
    }
    sorted.last().map_or(0, |&(s, _)| s)
}

/// Aggregate one zoom's `(stored_len, multiplicity)` pairs (any order) into
/// its [`ZoomStats`].
fn zoom_stats_from_weighted(z: u8, mut sizes: Vec<(u64, u64)>) -> ZoomStats {
    sizes.sort_unstable();
    let n: u64 = sizes.iter().map(|&(_, c)| c).sum();
    let total: u128 = sizes
        .iter()
        .map(|&(s, c)| u128::from(s) * u128::from(c))
        .sum();
    let (mean, p50, p99) = if n == 0 {
        (0, 0, 0)
    } else {
        (
            (total / u128::from(n)) as u64,
            weighted_percentile(&sizes, n, 50),
            weighted_percentile(&sizes, n, 99),
        )
    };
    ZoomStats {
        z,
        tile_count: n,
        total_bytes: u64::try_from(total).unwrap_or(u64::MAX),
        mean_bytes: mean,
        p50_bytes: p50,
        p99_bytes: p99,
        max_bytes: sizes.last().map_or(0, |&(s, _)| s),
    }
}

/// Compute the tile-weight report over an already-opened archive.
///
/// `largest_n` caps how many of the biggest tiles (by stored size) are
/// reported in [`StatsReport::largest`]; `0` reports none. The output is
/// exactly the first `largest_n` addressed tiles ordered by (stored bytes
/// descending, tile id ascending).
///
/// Walks [`ArchiveIndex::entries`] once without expanding run lengths:
/// memory is O(directory entries + `largest_n`).
pub fn compute_stats(archive: &ArchiveIndex, largest_n: usize) -> Result<StatsReport> {
    let tile_data_length = archive.header().tile_data_length;
    let mut by_zoom: Vec<Vec<(u64, u64)>> = Vec::new();
    // Min-heap keyed (bytes, Reverse(id)): its top is the WORST kept tile —
    // smallest bytes, and among equal bytes the highest tile id — so popping
    // it keeps exactly the first `largest_n` by (bytes desc, id asc).
    let mut heap: BinaryHeap<Reverse<(u64, Reverse<u64>)>> = BinaryHeap::new();

    for e in archive.entries() {
        // Same bounds check `ArchiveIndex::tiles` applies: an entry aimed
        // outside the tile-data section is a corrupt directory (#417).
        match e.offset.checked_add(u64::from(e.length)) {
            Some(end) if end <= tile_data_length => {}
            _ => {
                return Err(Error::PMTilesWrite(
                    "tile data past end of archive".to_string(),
                ))
            }
        }
        let bytes = u64::from(e.length);
        let run = u64::from(e.run_length.max(1));
        let first = e.tile_id;
        let last = first
            .checked_add(run - 1)
            .ok_or_else(|| Error::PMTilesWrite("tile id past end of range".to_string()))?;
        let bad = |err: Error| Error::PMTilesWrite(format!("bad tile id: {err}"));
        let (z_first, _, _) = tile_id_to_zxy(first).map_err(bad)?;
        let (z_last, _, _) = tile_id_to_zxy(last).map_err(bad)?;

        // Tile ids are contiguous per zoom, so a run that crosses a zoom
        // boundary splits into one contiguous segment per zoom it touches.
        if by_zoom.len() <= usize::from(z_last) {
            by_zoom.resize(usize::from(z_last) + 1, Vec::new());
        }
        for z in z_first..=z_last {
            let lo = first.max(zoom_base(z));
            let hi = last.min(zoom_base(z + 1) - 1);
            by_zoom[usize::from(z)].push((bytes, hi - lo + 1));
        }

        if largest_n > 0 {
            // A run's members share one size, so its best candidates are its
            // lowest ids; stop at the first one that would not be kept.
            for id in first..=last.min(first.saturating_add(largest_n as u64 - 1)) {
                let key = (bytes, Reverse(id));
                if heap.len() == largest_n {
                    let Some(Reverse(worst)) = heap.peek() else {
                        break;
                    };
                    if key <= *worst {
                        break;
                    }
                    heap.pop();
                }
                heap.push(Reverse(key));
            }
        }
    }

    let per_zoom = by_zoom
        .into_iter()
        .enumerate()
        .filter(|(_, sizes)| !sizes.is_empty())
        .map(|(z, sizes)| zoom_stats_from_weighted(z as u8, sizes))
        .collect();

    let mut kept: Vec<(u64, u64)> = heap
        .into_iter()
        .map(|Reverse((bytes, Reverse(id)))| (bytes, id))
        .collect();
    kept.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let largest = kept
        .into_iter()
        .map(|(bytes, id)| {
            // Already validated above (every kept id lies inside a checked run).
            let (z, x, y) = tile_id_to_zxy(id)?;
            Ok(LargestTile { z, x, y, bytes })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(StatsReport { per_zoom, largest })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::Compression;
    use crate::pmtiles_writer::{tile_id, StreamingPmtilesWriter};

    /// A synthetic archive: `Compression::None` so the stored byte count is
    /// exactly the length of the buffer handed to `add_tile` — the whole
    /// point being to control tile sizes precisely instead of depending on
    /// gzip's output length.
    fn write_synthetic(path: &std::path::Path, tiles: &[(u8, u32, u32, usize)]) {
        let mut w = StreamingPmtilesWriter::new(Compression::None).unwrap();
        w.set_layer_name("synthetic");
        for (i, &(z, x, y, size)) in tiles.iter().enumerate() {
            // Vary the fill byte per tile so identical-size tiles are not
            // deduplicated into a single stored copy.
            let data = vec![i as u8; size];
            w.add_tile(z, x, y, &data).unwrap();
        }
        w.finalize(path).unwrap();
    }

    /// Like [`write_synthetic`] but with explicit fill bytes, so tiles with
    /// the same `(fill, size)` are byte-identical and dedup/run-length encode.
    fn write_with_bodies(path: &std::path::Path, tiles: &[(u8, u32, u32, u8, usize)]) {
        let mut w = StreamingPmtilesWriter::new(Compression::None).unwrap();
        w.set_layer_name("synthetic");
        for &(z, x, y, fill, size) in tiles {
            w.add_tile(z, x, y, &vec![fill; size]).unwrap();
        }
        w.finalize(path).unwrap();
    }

    fn unit(sizes: impl IntoIterator<Item = u64>) -> Vec<(u64, u64)> {
        sizes.into_iter().map(|s| (s, 1)).collect()
    }

    #[test]
    fn zoom_base_matches_tile_id() {
        for z in 0..=10u8 {
            assert_eq!(zoom_base(z), tile_id(z, 0, 0), "z{z}");
        }
        assert_eq!(zoom_base(1), 1);
        assert_eq!(zoom_base(2), 5);
    }

    #[test]
    fn zoom_stats_nearest_rank_mean_median_p99_max() {
        // 1..=100: nearest-rank rank = ceil(p/100 * 100) = p, so p50 is the
        // 50th smallest (50) and p99 the 99th smallest (99).
        let stats = zoom_stats_from_weighted(3, unit(1..=100));
        assert_eq!(stats.z, 3);
        assert_eq!(stats.tile_count, 100);
        assert_eq!(stats.total_bytes, 5050);
        assert_eq!(stats.mean_bytes, 50); // 50.5 rounded down
        assert_eq!(stats.p50_bytes, 50);
        assert_eq!(stats.p99_bytes, 99);
        assert_eq!(stats.max_bytes, 100);
    }

    #[test]
    fn nearest_rank_on_small_counts() {
        // n = 3: p50 rank = ceil(1.5) = 2 -> 20; p99 rank = ceil(2.97) = 3.
        let stats = zoom_stats_from_weighted(0, unit([30, 10, 20]));
        assert_eq!(stats.p50_bytes, 20);
        assert_eq!(stats.p99_bytes, 30);
        // n = 4: p50 rank = 2 -> 20 (the lower middle, not an interpolation).
        let stats = zoom_stats_from_weighted(0, unit([10, 20, 30, 40]));
        assert_eq!(stats.p50_bytes, 20);
        assert_eq!(stats.p99_bytes, 40);
    }

    #[test]
    fn weighted_stats_equal_expanded_stats() {
        // (10 x 98) + (500 x 1) + (900 x 1): n = 100, p50 rank 50 -> 10,
        // p99 rank 99 -> 500, max 900, total 980 + 500 + 900 = 2380.
        let stats = zoom_stats_from_weighted(4, vec![(900, 1), (10, 98), (500, 1)]);
        assert_eq!(stats.tile_count, 100);
        assert_eq!(stats.total_bytes, 2380);
        assert_eq!(stats.mean_bytes, 23);
        assert_eq!(stats.p50_bytes, 10);
        assert_eq!(stats.p99_bytes, 500);
        assert_eq!(stats.max_bytes, 900);
        let expanded: Vec<u64> = std::iter::repeat_n(10, 98).chain([500, 900]).collect();
        assert_eq!(stats, zoom_stats_from_weighted(4, unit(expanded)));
    }

    #[test]
    fn zoom_stats_single_tile() {
        let stats = zoom_stats_from_weighted(0, vec![(42, 1)]);
        assert_eq!(stats.tile_count, 1);
        assert_eq!(stats.total_bytes, 42);
        assert_eq!(stats.mean_bytes, 42);
        assert_eq!(stats.p50_bytes, 42);
        assert_eq!(stats.p99_bytes, 42);
        assert_eq!(stats.max_bytes, 42);
    }

    #[test]
    fn compute_stats_over_synthetic_archive_matches_by_hand_computation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.pmtiles");

        // z2: four tiles of sizes 100, 200, 300, 400 -> mean 250, max 400.
        // z3: one tile of size 1000.
        write_synthetic(
            &path,
            &[
                (2, 0, 0, 100),
                (2, 1, 0, 200),
                (2, 0, 1, 300),
                (2, 1, 1, 400),
                (3, 0, 0, 1000),
            ],
        );

        let archive = ArchiveIndex::open(&path).unwrap();
        let report = compute_stats(&archive, 10).unwrap();

        assert_eq!(report.per_zoom.len(), 2);
        let z2 = report.per_zoom.iter().find(|z| z.z == 2).unwrap();
        assert_eq!(z2.tile_count, 4);
        assert_eq!(z2.total_bytes, 1000);
        assert_eq!(z2.mean_bytes, 250);
        assert_eq!(z2.p50_bytes, 200);
        assert_eq!(z2.p99_bytes, 400);
        assert_eq!(z2.max_bytes, 400);

        let z3 = report.per_zoom.iter().find(|z| z.z == 3).unwrap();
        assert_eq!(z3.tile_count, 1);
        assert_eq!(z3.max_bytes, 1000);

        // Largest tile overall is the z3 one, sized 1000.
        assert_eq!(
            report.largest[0],
            LargestTile {
                z: 3,
                x: 0,
                y: 0,
                bytes: 1000
            }
        );
        assert_eq!(report.largest.len(), 5);
    }

    #[test]
    fn compute_stats_caps_largest_to_n() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.pmtiles");
        write_synthetic(
            &path,
            &[
                (5, 0, 0, 10),
                (5, 1, 0, 50),
                (5, 2, 0, 30),
                (5, 3, 0, 90),
                (5, 4, 0, 20),
            ],
        );

        let archive = ArchiveIndex::open(&path).unwrap();
        let report = compute_stats(&archive, 2).unwrap();

        assert_eq!(report.largest.len(), 2);
        assert_eq!(report.largest[0].bytes, 90);
        assert_eq!(report.largest[1].bytes, 50);
    }

    #[test]
    fn compute_stats_largest_zero_reports_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.pmtiles");
        write_synthetic(&path, &[(1, 0, 0, 10)]);

        let archive = ArchiveIndex::open(&path).unwrap();
        let report = compute_stats(&archive, 0).unwrap();
        assert!(report.largest.is_empty());
        assert_eq!(report.per_zoom.len(), 1);
    }

    /// Ties at the `--largest` cutoff resolve to the lowest tile ids, however
    /// the tiles arrive: the output is exactly the first N by
    /// (bytes desc, tile id asc).
    #[test]
    fn largest_ties_at_cutoff_keep_lowest_tile_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ties.pmtiles");
        // All sixteen z2 tiles the same size, distinct bodies (no runs).
        let tiles: Vec<(u8, u32, u32, usize)> = (0..4u32)
            .flat_map(|x| (0..4u32).map(move |y| (2u8, x, y, 64usize)))
            .collect();
        write_synthetic(&path, &tiles);

        let archive = ArchiveIndex::open(&path).unwrap();
        let report = compute_stats(&archive, 3).unwrap();
        let ids: Vec<u64> = report
            .largest
            .iter()
            .map(|t| tile_id(t.z, t.x, t.y))
            .collect();
        assert_eq!(ids, vec![zoom_base(2), zoom_base(2) + 1, zoom_base(2) + 2]);
    }

    /// Identical tile bodies: a run of consecutive identical tiles and a
    /// non-adjacent duplicate. Every ADDRESSED tile counts, each at the full
    /// shared body length.
    #[test]
    fn identical_bodies_count_per_addressed_tile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dups.pmtiles");
        // z2 ids 5..=20 in Hilbert order. Fill the first four ids with one
        // body (a run of 4), the next with a distinct body, then repeat the
        // first body at a later id (deduplicated, not a run).
        let order: Vec<(u32, u32)> = (0..16u64)
            .map(|d| {
                let (_, x, y) = tile_id_to_zxy(zoom_base(2) + d).unwrap();
                (x, y)
            })
            .collect();
        let mut tiles = Vec::new();
        for &(x, y) in &order[0..4] {
            tiles.push((2u8, x, y, 7u8, 100usize));
        }
        tiles.push((2, order[4].0, order[4].1, 8, 300));
        tiles.push((2, order[6].0, order[6].1, 7, 100));
        write_with_bodies(&path, &tiles);

        let archive = ArchiveIndex::open(&path).unwrap();
        // Precondition: the writer really did run-length encode.
        assert!(archive.entries().iter().any(|e| e.run_length == 4));

        let report = compute_stats(&archive, 10).unwrap();
        assert_eq!(report.per_zoom.len(), 1);
        let z2 = report.per_zoom[0];
        assert_eq!(z2.tile_count, 6);
        assert_eq!(z2.total_bytes, 5 * 100 + 300);
        assert_eq!(z2.mean_bytes, 800 / 6);
        assert_eq!(z2.p50_bytes, 100);
        assert_eq!(z2.p99_bytes, 300);
        assert_eq!(z2.max_bytes, 300);
        assert_eq!(report.largest.len(), 6);
        assert_eq!(report.largest[0].bytes, 300);
        assert!(report.largest[1..].iter().all(|t| t.bytes == 100));
    }

    /// A run that crosses a zoom boundary is split at it: z0's single tile
    /// and z1's first tile share one body, so the writer emits one entry with
    /// run_length 2 spanning ids 0 and 1.
    #[test]
    fn run_crossing_a_zoom_boundary_is_split_per_zoom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cross.pmtiles");
        let (_, x1, y1) = tile_id_to_zxy(1).unwrap();
        write_with_bodies(
            &path,
            &[(0, 0, 0, 9, 50), (1, x1, y1, 9, 50), (1, 1, 1, 3, 20)],
        );

        let archive = ArchiveIndex::open(&path).unwrap();
        assert!(
            archive
                .entries()
                .iter()
                .any(|e| e.tile_id == 0 && e.run_length == 2),
            "precondition: one run spanning z0 and z1: {:?}",
            archive.entries()
        );

        let report = compute_stats(&archive, 1).unwrap();
        assert_eq!(report.per_zoom.len(), 2);
        let z0 = report.per_zoom[0];
        assert_eq!((z0.z, z0.tile_count, z0.max_bytes), (0, 1, 50));
        let z1 = report.per_zoom[1];
        assert_eq!((z1.z, z1.tile_count, z1.total_bytes), (1, 2, 70));
        assert_eq!(z1.max_bytes, 50);
        // Ties between the run's z0 and z1 members go to the lower id (z0).
        assert_eq!(
            report.largest,
            vec![LargestTile {
                z: 0,
                x: 0,
                y: 0,
                bytes: 50
            }]
        );
    }

    #[test]
    fn empty_archive_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.pmtiles");
        write_synthetic(&path, &[]);

        let archive = ArchiveIndex::open(&path).unwrap();
        let report = compute_stats(&archive, 10).unwrap();
        assert!(report.per_zoom.is_empty());
        assert!(report.largest.is_empty());
    }
}
