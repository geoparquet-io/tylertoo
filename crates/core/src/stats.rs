//! Per-zoom tile-weight report for a PMTiles archive (#552).
//!
//! Every "which zoom is blowing our tile budget" question this week (the
//! handover zoom, the 600 KB cells budget, r5/r7/r8 comparisons) needed
//! per-zoom stored-tile-size stats. This walks an already-opened
//! [`ArchiveIndex`] — header + directories, entries expanded, never a tile
//! body — so an entry's byte range gives the tile's STORED (compressed) size
//! directly. No tile is decompressed and the archive is never read whole
//! into memory: memory cost is O(tile count) u64s, not O(archive size).

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::archive_index::ArchiveIndex;
use crate::Result;

/// One zoom level's tile-size distribution, in STORED (compressed) bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ZoomStats {
    pub zoom: u8,
    pub tile_count: u64,
    /// Mean stored size, rounded down to the nearest byte.
    pub mean: u64,
    /// 50th percentile (nearest-rank over the sorted stored sizes).
    pub p50: u64,
    /// 99th percentile (nearest-rank over the sorted stored sizes).
    pub p99: u64,
    pub max: u64,
}

/// One of the `--largest` biggest tiles in the archive, by stored size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct LargestTile {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub bytes: u64,
}

/// Per-zoom tile-weight report: [`ZoomStats`] for every zoom the archive
/// holds at least one tile at (ascending by zoom), plus the largest tiles
/// overall (descending by size).
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct StatsReport {
    pub per_zoom: Vec<ZoomStats>,
    pub largest: Vec<LargestTile>,
}

/// Nearest-rank percentile over an ascending-sorted slice. `p` is `0..=100`.
/// Never called on an empty slice here (every zoom bucket that reaches this
/// has at least one tile).
fn percentile(sorted_asc: &[u64], p: f64) -> u64 {
    if sorted_asc.is_empty() {
        return 0;
    }
    let idx = ((p / 100.0) * (sorted_asc.len() - 1) as f64).round() as usize;
    sorted_asc[idx.min(sorted_asc.len() - 1)]
}

/// Aggregate one zoom's stored tile sizes (any order) into its [`ZoomStats`].
fn zoom_stats_from_sizes(zoom: u8, mut sizes: Vec<u64>) -> ZoomStats {
    sizes.sort_unstable();
    let n = sizes.len() as u64;
    let sum: u128 = sizes.iter().map(|&b| u128::from(b)).sum();
    let mean = if n == 0 {
        0
    } else {
        (sum / u128::from(n)) as u64
    };
    ZoomStats {
        zoom,
        tile_count: n,
        mean,
        p50: percentile(&sizes, 50.0),
        p99: percentile(&sizes, 99.0),
        max: sizes.last().copied().unwrap_or(0),
    }
}

/// Compute the tile-weight report over an already-opened archive.
///
/// `largest_n` caps how many of the biggest tiles (by stored size) are
/// reported in [`StatsReport::largest`]; `0` reports none. Every tile is
/// visited exactly once via [`ArchiveIndex::tiles`] — only its directory
/// entry (z/x/y and byte range) is touched, its body never read — so this
/// costs directory-sized memory even against a planet-scale archive.
pub fn compute_stats(archive: &ArchiveIndex, largest_n: usize) -> Result<StatsReport> {
    let mut by_zoom: Vec<Vec<u64>> = Vec::new();
    // Min-heap on stored bytes: popping the smallest keeps the top `largest_n`
    // largest tiles seen so far in O(log largest_n) per tile.
    let mut heap: BinaryHeap<Reverse<(u64, u8, u32, u32)>> = BinaryHeap::new();

    for tref in archive.tiles() {
        let tref = tref?;
        let bytes = tref.range.len() as u64;
        let z = tref.z as usize;
        if by_zoom.len() <= z {
            by_zoom.resize(z + 1, Vec::new());
        }
        by_zoom[z].push(bytes);

        if largest_n > 0 {
            heap.push(Reverse((bytes, tref.z, tref.x, tref.y)));
            if heap.len() > largest_n {
                heap.pop();
            }
        }
    }

    let per_zoom = by_zoom
        .into_iter()
        .enumerate()
        .filter(|(_, sizes)| !sizes.is_empty())
        .map(|(z, sizes)| zoom_stats_from_sizes(z as u8, sizes))
        .collect();

    let mut largest: Vec<LargestTile> = heap
        .into_iter()
        .map(|Reverse((bytes, z, x, y))| LargestTile { z, x, y, bytes })
        .collect();
    // Descending by size; ties broken by (z, x, y) for a deterministic order.
    largest.sort_by(|a, b| {
        b.bytes
            .cmp(&a.bytes)
            .then(a.z.cmp(&b.z))
            .then(a.x.cmp(&b.x))
            .then(a.y.cmp(&b.y))
    });

    Ok(StatsReport { per_zoom, largest })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::Compression;
    use crate::pmtiles_writer::StreamingPmtilesWriter;

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

    #[test]
    fn zoom_stats_from_sizes_computes_mean_median_p99_max() {
        // 1..=100: nearest-rank index = round(p/100 * 99).
        // p50 -> round(49.5) = 50 -> sizes[50] = 51.
        // p99 -> round(98.01) = 98 -> sizes[98] = 99.
        let sizes: Vec<u64> = (1..=100).collect();
        let stats = zoom_stats_from_sizes(3, sizes);
        assert_eq!(stats.zoom, 3);
        assert_eq!(stats.tile_count, 100);
        assert_eq!(stats.mean, 50); // sum 5050 / 100 = 50.5, integer division -> 50
        assert_eq!(stats.p50, 51);
        assert_eq!(stats.p99, 99);
        assert_eq!(stats.max, 100);
    }

    #[test]
    fn zoom_stats_single_tile() {
        let stats = zoom_stats_from_sizes(0, vec![42]);
        assert_eq!(stats.tile_count, 1);
        assert_eq!(stats.mean, 42);
        assert_eq!(stats.p50, 42);
        assert_eq!(stats.p99, 42);
        assert_eq!(stats.max, 42);
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
        let z2 = report.per_zoom.iter().find(|z| z.zoom == 2).unwrap();
        assert_eq!(z2.tile_count, 4);
        assert_eq!(z2.mean, 250);
        assert_eq!(z2.max, 400);

        let z3 = report.per_zoom.iter().find(|z| z.zoom == 3).unwrap();
        assert_eq!(z3.tile_count, 1);
        assert_eq!(z3.max, 1000);

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
}
