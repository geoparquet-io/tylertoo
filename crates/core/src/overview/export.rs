//! Batch PMTiles export from a GeoParquet overview file (Plan E0).
//!
//! This is the **replacement** for the shelved tile-generation pipeline, not a
//! revival of it. An overview file already contains per-level thinned,
//! simplified, ranked, Hilbert-ordered features; exporting to PMTiles is
//! therefore mechanical, two streaming passes per level (H3(b)):
//!
//! 1. For each overview **level**, resolve its Web Mercator **zoom** (§5.2).
//! 2. **Scan pass**: stream the level band via [`OverviewReader::read_level`]
//!    with `bbox=None`, decoding geometries only, to size every tile at that
//!    zoom (per-tile member counts), count the band's rows, and expand the
//!    overall bounds. The reader already implements the mode semantics —
//!    `duplicating` reads exactly the level's own row-group band,
//!    `partitioning` reads the prefix `0..=level` (features accumulate) — so
//!    this module treats both modes identically: "read level `k`, emit tiles
//!    at level `k`'s zoom".
//! 3. **Partition pass**: split the zoom's tiles into contiguous ascending
//!    `(x, y)` ranges of roughly [`DEFAULT_PARTITION_TARGET`] members each, then
//!    process them in **waves** of [`PARTITION_WAVE`] partitions. Each wave
//!    reads the band **once** (row groups pruned to the wave's combined bbox),
//!    splits every feature into its tiles with a **top-down recursive quadtree
//!    cascade** (see [`feature_tile_members`]) — each feature clipped once per
//!    pyramid level into an already-reduced child region down to the target
//!    zoom, so a vertex takes part in `O(depth)` clips rather than
//!    `O(tiles_spanned)` (issue #226) — and *routes* each resulting member to
//!    its owning partition (see [`process_wave`]). Sharing one read+decode
//!    across a wave replaces the old per-partition re-read (issue #228): a level
//!    now costs `ceil(P / PARTITION_WAVE)` band reads, not `P`. The per-tile
//!    clips reuse the [`clip_geometry`] entry point, MVT-encode via
//!    [`crate::mvt`], and each finished partition's tiles stream immediately to
//!    [`StreamingPmtilesWriter`]. Tiles are written in ascending `(x, y)` order
//!    per zoom — the historical order — so the archive's tile-data layout,
//!    deduplication, and directory are unchanged.
//!
//! ## What this deliberately does NOT do (per `context/archive/CARRYOVER.md`)
//!
//! - **No global cross-zoom external sort / per-tile fan-out.** Tiling is done
//!   one zoom at a time, one tile partition at a time; each partition is
//!   built, drained into the writer, and dropped before the next.
//! - **No per-tile budget retry loop / adaptive re-encode.** Generalization is
//!   precomputed in the overview file. The only safety valve is a single,
//!   optional, non-iterative drop pass for pathologically dense tiles
//!   (see [`ExportOptions::tile_size_limit`]).
//!
//! ## Memory ceiling
//!
//! Peak working set is `O(one wave of partitions + writer state)` (waves are
//! [`PARTITION_WAVE`] partitions processed concurrently): each in-flight
//! partition holds its (feature × intersecting-tile) clipped members plus its
//! encoded tiles, bounded by the partition target — **not** the zoom band's
//! feature count.
//! The scan pass keeps only per-tile member counts (`O(#tiles)`), and the
//! PMTiles writer streams tile bytes to a temp file, keeping only directory
//! entries. There is **no** per-zoom or global accumulation.
//!
//! ## Tile-boundary duplication (expected MVT semantics)
//!
//! The overview *format* stores each feature once per level (no clipping). The
//! PMTiles *export* necessarily reintroduces the classic MVT behaviour: a
//! feature spanning a tile seam is clipped into — and therefore appears in —
//! every tile it touches. Per-zoom exported feature totals will therefore be
//! `>=` the overview level's feature count, the excess being border
//! duplication.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type, UInt32Type,
    UInt64Type, UInt8Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType, Schema};
use geo::{BoundingRect, CoordsIter, Geometry, MapCoords};
use geoarrow::array::from_arrow_array;
use geoarrow_array::GeoArrowArray;
use prost::Message;
use rayon::prelude::*;
use serde::Serialize;

use crate::batch_processor::extract_geometries_from_array;
use crate::clip::clip_geometry;
use crate::compression::{self, Compression};
use crate::dedup::TileHasher;
use crate::mvt::{LayerBuilder, PropertyValue, TileBuilder};
use crate::pmtiles_writer::StreamingPmtilesWriter;
use crate::tile::{tile_ranges_for_bbox, tiles_for_bbox, BboxTileRanges, TileBounds, TileCoord};

use super::level::{zoom_for_gsd, Crs, Mode, OverviewsMeta};
use super::reader::{OverviewReader, ReaderError};
use super::writer::LEVEL_COLUMN;

/// Default MVT tile extent (matches [`crate::mvt::DEFAULT_EXTENT`]).
const DEFAULT_EXTENT: u32 = 4096;

/// Default per-tile edge buffer, in tile pixels (tippecanoe default is 5; we
/// use 8 to match the tile pipeline's historical default).
const DEFAULT_TILE_BUFFER_PX: u32 = 8;

/// Web Mercator projected half-extent in meters (EPSG:3857 axis range is
/// `±WEBMERC_HALF_M`). Used only to reproject a 3857 overview to lon/lat so the
/// (geographic) tile grid math applies.
const WEBMERC_HALF_M: f64 = 20_037_508.342_789_244;

/// Options controlling a PMTiles export.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// MVT layer name written into every tile and the archive metadata.
    pub layer_name: String,
    /// Per-tile edge buffer in **tile pixels**. Converted to coordinate units
    /// per tile (`buffer_deg = tile_width * buffer_px / extent`) and applied as
    /// the clip margin, so features spanning a seam render continuously.
    pub tile_buffer: u32,
    /// MVT tile extent (integer tile-local resolution). Default 4096.
    pub extent: u32,
    /// Optional per-tile MVT size limit in **bytes**. When set, a tile whose
    /// encoded size exceeds the limit triggers the single, non-iterative safety
    /// valve: its lowest-priority features (ranked by geometry size — see the
    /// module docs on why the assignment sort key is not recoverable per-row)
    /// are dropped in one pass and the tile is re-encoded once. When `None`, no
    /// size limit is enforced and `oversized_tiles` is always 0.
    pub tile_size_limit: Option<usize>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            layer_name: "overview".to_string(),
            tile_buffer: DEFAULT_TILE_BUFFER_PX,
            extent: DEFAULT_EXTENT,
            tile_size_limit: None,
        }
    }
}

/// Per-zoom statistics in an [`ExportReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoomReport {
    /// Web Mercator zoom this level was exported at.
    pub zoom: u8,
    /// Overview level index that produced this zoom.
    pub level: usize,
    /// Overview level feature count (rows read from the band).
    pub level_feature_count: usize,
    /// Number of tiles written at this zoom.
    pub tile_count: usize,
    /// Total features written across all tiles at this zoom (>= level feature
    /// count due to tile-boundary duplication).
    pub tile_feature_count: usize,
    /// Number of tiles at this zoom that hit the oversized safety valve.
    pub oversized_tiles: usize,
}

/// Result of an export, `Serialize` for the `--report` JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportReport {
    /// Level materialization mode of the source overview file.
    pub mode: String,
    /// PMTiles header min zoom (coarsest level's zoom).
    pub min_zoom: u8,
    /// PMTiles header max zoom (finest level's zoom).
    pub max_zoom: u8,
    /// Per-zoom statistics, coarse→fine.
    pub zooms: Vec<ZoomReport>,
    /// Total tiles written across all zooms.
    pub total_tiles: usize,
    /// Total features written across all tiles (with border duplication).
    pub total_tile_features: usize,
    /// Total tiles that hit the oversized safety valve.
    pub oversized_tiles: usize,
    /// Wall-clock export duration in seconds.
    pub duration_secs: f64,
}

/// Errors from [`export_pmtiles`].
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Opening or reading the overview file failed.
    #[error("overview reader error: {0}")]
    Reader(#[from] ReaderError),
    /// I/O error (writing the PMTiles archive).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Arrow error decoding a level batch.
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// A core-library error (geometry decode, PMTiles write).
    #[error("{0}")]
    Core(#[from] crate::Error),
    /// The input CRS is neither EPSG:4326 nor EPSG:3857.
    #[error("unsupported input CRS {crs:?}: export requires EPSG:4326 or EPSG:3857")]
    UnsupportedCrs {
        /// The rejected CRS identifier.
        crs: String,
    },
    /// The file has no geometry column.
    #[error("overview file has no geometry column")]
    NoGeometryColumn,
}

/// One source feature: its (possibly reprojected-to-4326) geometry and the MVT
/// properties carried over from the overview file (level column + covering
/// struct excluded). Test-support only: production streams members directly
/// instead of materializing features (see [`collect_wave_members`]).
#[cfg(test)]
struct Feature {
    geom: Geometry<f64>,
    props: Vec<(String, PropertyValue)>,
}

/// An encoded tile ready to hand to the PMTiles writer. `data` is already
/// **gzip-compressed** (compression runs in the parallel encode section, not on
/// the serial writer thread — issue #227). `hash` is [`TileHasher::hash`] of the
/// *uncompressed* MVT bytes, the dedup key; `raw_len` is the uncompressed length,
/// kept for the writer's dedup byte-savings stat.
struct EncodedTile {
    x: u32,
    y: u32,
    data: Vec<u8>,
    hash: u64,
    raw_len: usize,
    feature_count: usize,
    oversized: bool,
}

/// Resolve the Web Mercator zoom for overview level `level_idx`.
///
/// Uses the level's explicit `zoom` when present (§3.2). When absent, derives it
/// from the level GSD via the §5.2 inverse — `z = round(log2(C / base / gsd))` —
/// and clamps to `u8`. The rounding rule (nearest integer) is documented here so
/// the mapping is reproducible: a level whose GSD sits between two zooms maps to
/// the nearer one.
pub fn zoom_for_level(meta: &OverviewsMeta, level_idx: usize) -> u8 {
    let level = &meta.levels[level_idx];
    if let Some(z) = level.zoom {
        return z;
    }
    let z = zoom_for_gsd(level.gsd).round();
    z.clamp(0.0, 255.0) as u8
}

/// Members-per-partition target for the partitioned streaming export (H3(b)).
///
/// Each zoom's tiles are split into contiguous `(x, y)` ranges whose summed
/// (feature × tile) member counts reach at least this value; partitions are
/// processed one at a time and streamed to the writer, so this bounds the
/// per-partition working set (clipped geometries + encoded tiles) instead of
/// holding the whole zoom in memory.
const DEFAULT_PARTITION_TARGET: usize = 32_768;

/// Number of partitions grouped into one wave. A wave reads the level band
/// **once** (row groups pruned to the wave's combined bbox), decodes it a single
/// time, and routes the decoded features to its partitions — so a level costs
/// `ceil(P / PARTITION_WAVE)` band reads instead of one per partition (issue
/// #228). Peak memory stays at O(`PARTITION_WAVE` partitions), and one shared
/// decode per wave (rather than one per partition) also lowers the peak. Waves
/// complete in order, so the writer still receives tiles in ascending `(x, y)`
/// order.
const PARTITION_WAVE: usize = 6;

/// Arrow batch size for the partition read passes (the parquet reader default
/// of 1024 rows makes the per-batch parallel clip sections too small to load-
/// balance across cores; 16k rows amortizes decode and barrier overhead while
/// keeping per-batch memory modest).
const EXPORT_BATCH_SIZE: usize = 8_192;

/// Minimum wall-clock gap between incremental checkpoints (Issue #229). After a
/// level finishes we snapshot a valid archive capped at that zoom, but only if
/// this much time has elapsed since the last snapshot — so fast exports (which
/// finish in one `finalize` well under this interval) pay nothing, while a
/// multi-hour run keeps its finished zooms salvageable within a minute of each
/// level completing.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// Minimum wall-clock gap between within-level wave-progress log lines (Issue
/// #229). A level stuck in its scan/clip loop stays diagnosable in minutes (the
/// wave counter advances, or visibly does not) instead of hours of silence.
const WAVE_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Export an overview GeoParquet file to a PMTiles archive.
///
/// See the module documentation for the full pipeline and the design
/// constraints it observes.
pub fn export_pmtiles(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &ExportOptions,
) -> Result<ExportReport, ExportError> {
    export_pmtiles_with_partition_target(input_path, output_path, options, DEFAULT_PARTITION_TARGET)
}

/// Implementation of [`export_pmtiles`] with an explicit partition-size target,
/// exposed separately so tests can force many tiny partitions and verify the
/// archive is byte-identical regardless of partitioning.
fn export_pmtiles_with_partition_target(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &ExportOptions,
    partition_target: usize,
) -> Result<ExportReport, ExportError> {
    let start = Instant::now();
    let input_path = input_path.as_ref();

    let reader = OverviewReader::open(input_path)?;
    let meta = reader.meta().clone();
    let crs = detect_crs(input_path)?;

    let num_levels = reader.num_levels();
    let min_zoom = zoom_for_level(&meta, 0);
    let max_zoom = zoom_for_level(&meta, num_levels - 1);

    // Writer: field metadata + layer name are derived from the first level's
    // schema (property columns, level/covering excluded).
    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)?;
    writer.set_layer_name(&options.layer_name);
    writer.set_fields(field_metadata(
        reader.schema(),
        geometry_index(reader.schema()),
    ));

    // Pass 1 (scan), **all levels in one streaming read of the file** (issue
    // #233). Scanning each level independently re-reads and re-decodes every
    // coarse row group from every finer level's accumulating prefix (§5.1) —
    // `Σ_k |prefix_k|` decodes. Instead read each band once and fan its
    // features to the tile-size accumulators of every level whose prefix
    // includes it. O(#tiles) memory per level; the per-level `LevelScan`s are
    // byte-identical to independent per-level scans.
    let t_scan = Instant::now();
    let scans = scan_all_levels(&reader, crs, &meta)?;
    log::info!(
        "[export] scan complete: {num_levels} levels, single read, {:.2}s",
        t_scan.elapsed().as_secs_f64()
    );

    let mut overall_bounds: Option<TileBounds> = None;
    for scan in &scans {
        if let Some(b) = &scan.bounds {
            match &mut overall_bounds {
                Some(acc) => acc.expand(b),
                None => overall_bounds = Some(*b),
            }
        }
    }

    // Set bounds up front so per-level checkpoints (#229) carry the correct
    // archive bounds, not the writer's empty default. The value is identical to
    // setting it just before `finalize`, so the final bytes are unchanged.
    if let Some(b) = &overall_bounds {
        writer.set_bounds(b);
    }

    // Throttle for incremental checkpoints (#229): a fast export finishes before
    // the first interval elapses and never checkpoints, letting `finalize` do
    // all the work.
    let mut last_checkpoint = Instant::now();

    let mut zooms: Vec<ZoomReport> = Vec::with_capacity(num_levels);

    for (level_idx, scan) in scans.iter().enumerate() {
        let zoom = zoom_for_level(&meta, level_idx);

        // Split the zoom's tiles into contiguous ascending (x, y) ranges of
        // roughly `partition_target` members each.
        let partitions = plan_partitions(&scan.tile_counts, zoom, partition_target);

        // Pass 2: process partitions in ascending tile order, streaming each
        // finished partition's encoded tiles straight to the writer. To hide
        // the per-partition band re-read/decode behind clip work, partitions
        // are processed in small parallel waves (order-preserving collect,
        // then a serial in-order write), so peak memory is O(one wave of
        // partitions + writer state), not O(zoom band).
        let t_tiles = Instant::now();
        let ctx = LevelCtx {
            reader: &reader,
            level_idx,
            crs,
            zoom,
            opts: options,
        };
        let mut tile_count = 0usize;
        let mut tile_feature_count = 0usize;
        let mut oversized = 0usize;
        let mut write_secs = 0f64;
        let total_waves = partitions.len().div_ceil(PARTITION_WAVE);
        // Within-level progress (#229): a long finest level is where runs get
        // stuck, so emit a throttled wave counter. If it advances the level is
        // slow; if it freezes the level is stuck — diagnosable in minutes.
        let mut last_wave_log = Instant::now();
        for (wave_idx, wave) in partitions.chunks(PARTITION_WAVE).enumerate() {
            let results: Vec<Vec<EncodedTile>> = process_wave(&ctx, wave)?;
            let t_write = Instant::now();
            for tiles in &results {
                for t in tiles {
                    tile_feature_count += t.feature_count;
                    if t.oversized {
                        oversized += 1;
                    }
                    writer.add_tile_precompressed(
                        zoom,
                        t.x,
                        t.y,
                        t.hash,
                        &t.data,
                        t.raw_len,
                        t.feature_count,
                    )?;
                }
                tile_count += tiles.len();
            }
            write_secs += t_write.elapsed().as_secs_f64();

            if last_wave_log.elapsed() >= WAVE_LOG_INTERVAL {
                log::info!(
                    "[export] level {}/{num_levels} z{zoom}: wave {}/{total_waves}, \
                     {tile_count} tiles, {:.0}s",
                    level_idx + 1,
                    wave_idx + 1,
                    start.elapsed().as_secs_f64(),
                );
                last_wave_log = Instant::now();
            }
        }
        // Per-level summary at info so operators see progress without RUST_LOG
        // (env_logger defaults to info). Detailed clip/write split stays debug.
        log::info!(
            "[export] level {}/{num_levels} z{zoom} done: {} feats, {tile_count} tiles, \
             {} partitions, {:.1}s (total {:.0}s)",
            level_idx + 1,
            scan.feature_count,
            partitions.len(),
            t_tiles.elapsed().as_secs_f64(),
            start.elapsed().as_secs_f64(),
        );
        log::debug!(
            "[profile] z{zoom} (level {level_idx}, {} feats, {tile_count} tiles, {} partitions): \
             clip+encode+gzip={:.2}s write={write_secs:.2}s",
            scan.feature_count,
            partitions.len(),
            t_tiles.elapsed().as_secs_f64() - write_secs,
        );

        zooms.push(ZoomReport {
            zoom,
            level: level_idx,
            level_feature_count: scan.feature_count,
            tile_count,
            tile_feature_count,
            oversized_tiles: oversized,
        });

        // Salvageable output (#229): snapshot a valid archive capped at this
        // zoom so an interrupted run keeps its finished zooms. Throttled, and
        // skipped on the last level since `finalize` immediately follows and
        // produces the complete archive.
        if level_idx + 1 < num_levels && last_checkpoint.elapsed() >= CHECKPOINT_INTERVAL {
            let t_ckpt = Instant::now();
            writer.checkpoint(output_path.as_ref())?;
            last_checkpoint = Instant::now();
            log::info!(
                "[export] checkpoint written: zooms {}..={zoom} salvageable ({:.2}s)",
                zoom_for_level(&meta, 0),
                t_ckpt.elapsed().as_secs_f64(),
            );
        }
    }

    let t_finalize = Instant::now();
    writer.finalize(output_path.as_ref())?;
    log::info!(
        "[export] finalize complete: {:.2}s (total {:.0}s)",
        t_finalize.elapsed().as_secs_f64(),
        start.elapsed().as_secs_f64(),
    );

    let total_tiles = zooms.iter().map(|z| z.tile_count).sum();
    let total_tile_features = zooms.iter().map(|z| z.tile_feature_count).sum();
    let oversized_tiles = zooms.iter().map(|z| z.oversized_tiles).sum();

    Ok(ExportReport {
        mode: format!("{:?}", reader.mode()).to_lowercase(),
        min_zoom,
        max_zoom,
        zooms,
        total_tiles,
        total_tile_features,
        oversized_tiles,
        duration_secs: start.elapsed().as_secs_f64(),
    })
}

// ============================================================================
// Partitioned tiling + encoding (H3(b))
// ============================================================================

/// Pack a tile `(x, y)` into a single ordered key. Ordering by this key equals
/// ordering by the `(x, y)` tuple — the historical per-zoom write order (the
/// old per-zoom `BTreeMap<(u32, u32), _>` iteration order). Preserving it
/// keeps the archive's tile-data layout, deduplication, and directory
/// byte-identical.
#[inline]
fn tile_key(x: u32, y: u32) -> u64 {
    ((x as u64) << 32) | y as u64
}

/// One tile member: a feature's clipped geometry destined for the tile with
/// key `key`, the feature's band-order sequence number (stable within-tile
/// ordering), and the feature's MVT properties (shared across the feature's
/// tile copies).
struct Member {
    key: u64,
    seq: u64,
    geom: Geometry<f64>,
    props: Arc<Vec<(String, PropertyValue)>>,
}

/// One level's result from the scan pass ([`scan_all_levels`]).
#[cfg_attr(test, derive(Debug, PartialEq))]
struct LevelScan {
    /// Rows in the level band.
    feature_count: usize,
    /// Union of feature bboxes (`None` when no feature has one).
    bounds: Option<TileBounds>,
    /// Member count per tile key, ascending `(x, y)`.
    tile_counts: BTreeMap<u64, usize>,
}

/// A contiguous range of tile keys, processed (and written) as one unit.
struct Partition {
    key_lo: u64,
    key_hi: u64,
    /// Union of the partition's tile bounds plus a one-tile margin, used to
    /// prune row groups on the partition's read pass. Pruning is conservative:
    /// a feature assigned to a tile in this partition has a bbox intersecting
    /// that tile's bounds, hence intersecting this rectangle.
    bbox: TileBounds,
    /// Summed member count (for balancing).
    members: usize,
}

/// Shared per-level context for partition processing.
struct LevelCtx<'a> {
    reader: &'a OverviewReader,
    level_idx: usize,
    crs: Crs,
    zoom: u8,
    opts: &'a ExportOptions,
}

/// Scan **every** level in a single streaming read of the file (issue #233).
///
/// Scanning level `k` independently reads its *render set* — in partitioning
/// mode the accumulating prefix `0..=end_k` (spec §5.1) — so a row group
/// belonging to level `j` is re-read and re-decoded by every finer level
/// `k >= j` whose prefix includes it: `Σ_k |prefix_k|` row-group decodes.
///
/// This reads each level's **own band once** (`Σ_j |band_j|` = every row group
/// exactly once via [`OverviewReader::read_band_with_batch_size`]) and *fans*
/// every decoded feature to the accumulators of every level whose render set
/// includes its band — level `j`'s features contribute to levels `{j}`
/// (duplicating, self-contained bands) or `{j..N}` (partitioning prefix).
///
/// The returned `LevelScan`s are **byte-identical** to `N` independent per-level
/// scans: a level's bounds union is component-wise `min`/`max` and its per-tile
/// counts are integer sums, both order- **and** grouping-independent — so
/// splitting the prefix into per-band reads and merging in band order yields the
/// same bounds rectangle and the same `tile_counts` map (and hence the same
/// partition plan and the same archive). Peak memory is one batch's bboxes plus
/// the `N` per-level `O(#tiles)` count maps.
fn scan_all_levels(
    reader: &OverviewReader,
    crs: Crs,
    meta: &OverviewsMeta,
) -> Result<Vec<LevelScan>, ExportError> {
    let num_levels = reader.num_levels();
    let partitioning = matches!(reader.mode(), Mode::Partitioning);
    let zooms: Vec<u8> = (0..num_levels).map(|k| zoom_for_level(meta, k)).collect();
    let mut scans: Vec<LevelScan> = (0..num_levels)
        .map(|_| LevelScan {
            feature_count: 0,
            bounds: None,
            tile_counts: BTreeMap::new(),
        })
        .collect();

    for j in 0..num_levels {
        // Bands accumulate into finer levels' render sets only in partitioning
        // mode; duplicating bands are self-contained, so band `j` feeds level
        // `j` alone.
        let last = if partitioning { num_levels - 1 } else { j };
        let band_reader = reader.read_band_with_batch_size(j, EXPORT_BATCH_SIZE)?;
        for batch in band_reader {
            // Decode + (for 3857) reproject each feature's bbox once per band;
            // sizing the same bbox into every target level below is bit-identical
            // to reprojecting it once per level, since reprojection is a pure fn.
            let bboxes = decode_batch_bboxes(&batch?, crs)?;
            for k in j..=last {
                let scan = &mut scans[k];
                scan.feature_count += bboxes.len();
                let (batch_bounds, batch_counts) = size_bboxes(&bboxes, zooms[k]);
                if let Some(b) = batch_bounds {
                    match &mut scan.bounds {
                        Some(acc) => acc.expand(&b),
                        None => scan.bounds = Some(b),
                    }
                }
                for (key, v) in batch_counts {
                    *scan.tile_counts.entry(key).or_insert(0) += v;
                }
            }
        }
    }
    Ok(scans)
}

/// Decode one record batch's geometries into per-feature bounding boxes,
/// already reprojected to lon/lat (EPSG:4326) when the file is EPSG:3857 —
/// element `i` is `None` when feature `i` has no bounding rectangle
/// (empty/degenerate). Reprojecting the two bbox corners is monotone per axis,
/// hence bit-identical to the bbox of the reprojected geometry.
fn decode_batch_bboxes(
    batch: &RecordBatch,
    crs: Crs,
) -> Result<Vec<Option<TileBounds>>, ExportError> {
    let schema = batch.schema();
    let geom_idx = geometry_index(&schema).ok_or(ExportError::NoGeometryColumn)?;
    let geom_field = schema.field(geom_idx).clone();
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(batch.num_rows());
    extract_geometries_from_array(garr.as_ref(), &mut geoms)?;
    let bboxes = geoms
        .par_iter()
        .map(|geom| {
            geom.bounding_rect().map(|rect| {
                let bbox = TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
                if matches!(crs, Crs::Epsg3857) {
                    let (lng_min, lat_min) = webmerc_to_lnglat(bbox.lng_min, bbox.lat_min);
                    let (lng_max, lat_max) = webmerc_to_lnglat(bbox.lng_max, bbox.lat_max);
                    TileBounds::new(lng_min, lat_min, lng_max, lat_max)
                } else {
                    bbox
                }
            })
        })
        .collect();
    Ok(bboxes)
}

/// Size a batch of (already-reprojected) feature bboxes at `zoom`: union their
/// bounds and count one member per tile each spans. Sizing is embarrassingly
/// parallel per feature (issue #227): fold into per-thread `(bounds, counts)`
/// accumulators, then reduce. Both reductions (bbox union, per-tile count sum)
/// are commutative and associative, so the merged result is identical
/// regardless of thread scheduling or how features were grouped into batches.
fn size_bboxes(
    bboxes: &[Option<TileBounds>],
    zoom: u8,
) -> (Option<TileBounds>, HashMap<u64, usize>) {
    bboxes
        .par_iter()
        .fold(
            || (None::<TileBounds>, HashMap::<u64, usize>::new()),
            |(mut bounds, mut counts), bbox| {
                if let Some(bbox) = bbox {
                    match &mut bounds {
                        Some(acc) => acc.expand(bbox),
                        None => bounds = Some(*bbox),
                    }
                    for tc in tiles_for_bbox(bbox, zoom) {
                        *counts.entry(tile_key(tc.x, tc.y)).or_insert(0) += 1;
                    }
                }
                (bounds, counts)
            },
        )
        .reduce(
            || (None::<TileBounds>, HashMap::<u64, usize>::new()),
            |(b_a, c_a), (b_b, c_b)| {
                let bounds = match (b_a, b_b) {
                    (Some(mut a), Some(b)) => {
                        a.expand(&b);
                        Some(a)
                    }
                    (Some(a), None) | (None, Some(a)) => Some(a),
                    (None, None) => None,
                };
                // Merge the smaller map into the larger to minimize rehashing.
                let (mut dst, src) = if c_a.len() >= c_b.len() {
                    (c_a, c_b)
                } else {
                    (c_b, c_a)
                };
                for (k, v) in src {
                    *dst.entry(k).or_insert(0) += v;
                }
                (bounds, dst)
            },
        )
}

/// Per-level scan oracle: reads level `level_idx`'s full render set (the
/// mode-dependent prefix, via [`OverviewReader::read_level_with_batch_size`])
/// and sizes it at `zoom`. This is the pre-#233 independent-per-level read path,
/// retained as the byte-identity oracle for [`scan_all_levels`] — a genuinely
/// different read (one prefix vs a fan of per-band reads) that must yield the
/// same `LevelScan`.
#[cfg(test)]
fn scan_level(
    reader: &OverviewReader,
    level_idx: usize,
    crs: Crs,
    zoom: u8,
) -> Result<LevelScan, ExportError> {
    let batch_reader = reader.read_level_with_batch_size(level_idx, None, EXPORT_BATCH_SIZE)?;
    let mut scan = LevelScan {
        feature_count: 0,
        bounds: None,
        tile_counts: BTreeMap::new(),
    };
    for batch in batch_reader {
        let bboxes = decode_batch_bboxes(&batch?, crs)?;
        scan.feature_count += bboxes.len();
        let (batch_bounds, batch_counts) = size_bboxes(&bboxes, zoom);
        if let Some(b) = batch_bounds {
            match &mut scan.bounds {
                Some(acc) => acc.expand(&b),
                None => scan.bounds = Some(b),
            }
        }
        for (k, v) in batch_counts {
            *scan.tile_counts.entry(k).or_insert(0) += v;
        }
    }
    Ok(scan)
}

/// Split a zoom's tiles (ascending key order) into contiguous partitions of at
/// least `partition_target` members each (the last partition may be smaller).
fn plan_partitions(
    tile_counts: &BTreeMap<u64, usize>,
    zoom: u8,
    partition_target: usize,
) -> Vec<Partition> {
    let target = partition_target.max(1);
    let mut out: Vec<Partition> = Vec::new();
    let mut cur: Option<Partition> = None;
    for (&key, &count) in tile_counts {
        let tb = TileCoord::new((key >> 32) as u32, key as u32, zoom).bounds();
        match cur.as_mut() {
            Some(p) => {
                p.key_hi = key;
                p.bbox.expand(&tb);
                p.members += count;
            }
            None => {
                cur = Some(Partition {
                    key_lo: key,
                    key_hi: key,
                    bbox: tb,
                    members: count,
                });
            }
        }
        if cur.as_ref().is_some_and(|p| p.members >= target) {
            out.push(cur.take().unwrap());
        }
    }
    out.extend(cur);
    // One-tile margin on the pruning bbox: generously covers any rounding in
    // row-group bbox statistics; over-inclusion only reads an extra row group.
    let margin = 360.0 / 2f64.powi(zoom as i32);
    for p in &mut out {
        p.bbox = TileBounds::new(
            p.bbox.lng_min - margin,
            p.bbox.lat_min - margin,
            p.bbox.lng_max + margin,
            p.bbox.lat_max + margin,
        );
    }
    out
}

/// Pass 2, one wave of partitions: **read the band once** (row groups pruned to
/// the wave's combined bbox), clip every feature into its tiles across the
/// wave's whole key range, and *route* each member to its owning partition.
/// Each partition is then grouped-by-tile and MVT-encoded in parallel exactly
/// as a standalone partition would be. Returns one `Vec<EncodedTile>` per
/// partition, in the wave's ascending partition order, each in ascending
/// `(x, y)` order.
///
/// This replaces the old per-partition re-read (issue #228): instead of `P`
/// independent band reads + decodes per level, a wave of `PARTITION_WAVE`
/// partitions shares a single read + decode. Byte-identical to the per-partition
/// path — each partition receives exactly the members with key in its
/// `[key_lo, key_hi]` window (a tile's clipped geometry is window-independent),
/// and within-tile `(key, seq)` order is preserved because row-group pruning
/// keeps kept rows in band order regardless of how wide the read is.
fn process_wave(
    ctx: &LevelCtx<'_>,
    wave: &[Partition],
) -> Result<Vec<Vec<EncodedTile>>, ExportError> {
    debug_assert!(!wave.is_empty(), "wave must be non-empty");

    // Row-group pruning is only valid when the file's coordinates (and thus its
    // row-group bbox statistics) are lon/lat. For 3857 files the stats are in
    // meters; skip pruning (correct, just unpruned). Prune to the *union* of the
    // wave's partition bboxes.
    let bbox = match ctx.crs {
        Crs::Epsg4326 => {
            let mut b = wave[0].bbox;
            for p in &wave[1..] {
                b.expand(&p.bbox);
            }
            Some([b.lng_min, b.lat_min, b.lng_max, b.lat_max])
        }
        Crs::Epsg3857 => None,
    };
    let batch_reader =
        ctx.reader
            .read_level_with_batch_size(ctx.level_idx, bbox, EXPORT_BATCH_SIZE)?;

    let t_collect = Instant::now();
    let mut buckets: Vec<Vec<Member>> = (0..wave.len()).map(|_| Vec::new()).collect();
    let mut seq = 0u64;
    for batch in batch_reader {
        let batch = batch?;
        collect_wave_members(ctx, wave, &batch, &mut seq, &mut buckets)?;
    }
    let collect_secs = t_collect.elapsed().as_secs_f64();
    let n_members: usize = buckets.iter().map(Vec::len).sum();

    let t_encode = Instant::now();
    let tiles: Vec<Vec<EncodedTile>> = buckets
        .into_par_iter()
        .map(|members| encode_members(members, ctx.zoom, ctx.opts))
        .collect::<Result<_, _>>()?;
    let n_tiles: usize = tiles.iter().map(Vec::len).sum();
    log::debug!(
        "[profile]     z{} wave [{:x}..{:x}] ({} partitions): rows_read={seq} \
         members={n_members} tiles={n_tiles} collect={collect_secs:.2}s \
         sort+encode={:.2}s",
        ctx.zoom,
        wave[0].key_lo,
        wave[wave.len() - 1].key_hi,
        wave.len(),
        t_encode.elapsed().as_secs_f64(),
    );
    Ok(tiles)
}

/// The wave partition owning `key`. The wave's partitions are contiguous,
/// disjoint, ascending `[key_lo, key_hi]` ranges, and every emitted key was
/// scanned (so it lies in exactly one of them), giving an unambiguous home.
#[inline]
fn route_partition(wave: &[Partition], key: u64) -> usize {
    // Rightmost partition whose `key_lo <= key`.
    let idx = wave.partition_point(|p| p.key_lo <= key).saturating_sub(1);
    debug_assert!(
        key >= wave[idx].key_lo && key <= wave[idx].key_hi,
        "member key {key:x} falls outside its wave partition [{:x}..{:x}]",
        wave[idx].key_lo,
        wave[idx].key_hi,
    );
    idx
}

/// Decode one record batch and append its members to the wave's per-partition
/// buckets: clip every feature (in parallel) into its intersecting tiles across
/// the wave's whole key range, route each member to its owning partition, then
/// attach shared per-feature properties in band order. `seq` is the running
/// band-order counter (advanced for every row, member-producing or not, so
/// within-tile ordering matches the old whole-band feature indexing).
fn collect_wave_members(
    ctx: &LevelCtx<'_>,
    wave: &[Partition],
    batch: &RecordBatch,
    seq: &mut u64,
    buckets: &mut [Vec<Member>],
) -> Result<(), ExportError> {
    debug_assert_eq!(buckets.len(), wave.len());
    let key_lo = wave[0].key_lo;
    let key_hi = wave[wave.len() - 1].key_hi;

    let schema = batch.schema();
    let geom_idx = geometry_index(&schema).ok_or(ExportError::NoGeometryColumn)?;
    let geom_field = schema.field(geom_idx).clone();
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(batch.num_rows());
    extract_geometries_from_array(garr.as_ref(), &mut geoms)?;
    if matches!(ctx.crs, Crs::Epsg3857) {
        geoms = geoms.par_iter().map(reproject_3857_to_4326).collect();
    }

    // Parallel per-feature clip (H3(c) lever 2), over the wave's whole key range.
    let row_members: Vec<Vec<(u64, Geometry<f64>)>> = geoms
        .par_iter()
        .map(|g| feature_tile_members(g, ctx.zoom, ctx.opts, key_lo, key_hi))
        .collect();
    drop(geoms);

    if row_members.iter().all(|v| v.is_empty()) {
        *seq += row_members.len() as u64;
        return Ok(());
    }

    // Extract property columns once per batch; materialize per-feature props
    // only for rows that produced members.
    let prop_cols = property_columns(&schema, geom_idx);
    let mut extracted: Vec<(String, Vec<Option<PropertyValue>>)> =
        Vec::with_capacity(prop_cols.len());
    for &(idx, ref name) in &prop_cols {
        extracted.push((name.clone(), extract_property_column(batch.column(idx))));
    }
    for (row, items) in row_members.into_iter().enumerate() {
        if items.is_empty() {
            *seq += 1;
            continue;
        }
        let mut props = Vec::with_capacity(extracted.len());
        for (name, col) in &extracted {
            if let Some(v) = &col[row] {
                props.push((name.clone(), v.clone()));
            }
        }
        let props = Arc::new(props);
        for (key, geom) in items {
            buckets[route_partition(wave, key)].push(Member {
                key,
                seq: *seq,
                geom,
                props: Arc::clone(&props),
            });
        }
        *seq += 1;
    }
    Ok(())
}

/// Clip (or fast-path pass through) one feature into every tile it intersects
/// at `zoom` whose key falls within `[key_lo, key_hi]`, via a **top-down
/// recursive quadtree cascade** (issue #226, tippecanoe's tiling model).
///
/// ## Why recursion (the tiles×vertices blowup)
///
/// The straightforward loop — enumerate every tile the feature bbox covers and
/// clip the feature's **full** geometry against each — costs
/// `Σ_features (tiles_spanned × vertices)`. A large admin polygon covering a
/// fraction `F` of the map touches `≈ (F·2^z)²` tiles at zoom `z`, each a full
/// ring clip; at z12–14 this is billions of clip-vertex ops (the adm4 export
/// DNF'd at 3h13m). This is the spatial analogue of #218's repeated work.
///
/// Instead we walk the tile pyramid from the root down to `zoom`. At each node
/// we clip the parent's **already-reduced** geometry to the node's bounds plus
/// buffer, then split into four children. Because a child's buffered bounds are
/// contained in its parent's (the pixel buffer in world units doubles per level
/// up, so `child ± buf(child) ⊆ parent ± buf(parent)` on both axes, Mercator
/// latitude included), the cascade is a proper superset chain and
/// `clip(clip(G, parent), leaf) = clip(G, leaf)` — the leaf result is the same
/// clip as the direct loop would produce (modulo float-noise / ring-normalization
/// on genuine seam-crossers; interior features pass through byte-identical). Each
/// vertex now takes part in `O(depth)` clips, not `O(tiles_spanned)`.
///
/// ## Identical leaf set
///
/// The recursion is bounded by [`tile_ranges_for_bbox`] — the same range math
/// [`tiles_for_bbox`] uses — so the emitted key set equals
/// `tiles_for_bbox(feature_bbox) ∩ [key_lo, key_hi]` exactly. That keeps the
/// scan pass's per-tile counts valid (every emitted key lies in a planned
/// partition) and the archive's tile set unchanged.
fn feature_tile_members(
    geom: &Geometry<f64>,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
) -> Vec<(u64, Geometry<f64>)> {
    let Some(rect) = geom.bounding_rect() else {
        return Vec::new();
    };
    let bbox = TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
    // Target leaf-tile ranges at `zoom` — the authority for which tiles this
    // feature belongs to (shared with `tiles_for_bbox`).
    let ranges = tile_ranges_for_bbox(&bbox, zoom);
    let mut out = Vec::new();

    // Dispatch on the *direct* path's cost — the ticket's own model,
    // `tiles_spanned × vertex_count`. The recursive cascade wins big when that
    // product explodes (few huge admin polygons: high span AND high vertex
    // count, the adm4 blowup), but its per-node overhead (an intermediate clip
    // + trig-heavy `TileCoord::bounds()` at every pyramid node) is dead weight
    // for the common case of many features that are already small — overview
    // levels are pre-simplified, so a deep-zoom feature carries few vertices and
    // the plain per-tile clip is cheaper. Routing those through the direct loop
    // keeps them byte-identical to the pre-#226 path and avoids a regression on
    // dense corpora, while the giant polygons that actually caused the DNF take
    // the cascade.
    let direct_cost = tile_span(&ranges).saturating_mul(geom.coords_count() as u64);
    if direct_cost <= DIRECT_CLIP_BUDGET {
        feature_tile_members_direct(geom, &bbox, zoom, opts, key_lo, key_hi, &mut out);
    } else {
        // Start the descent at the feature's covering tile (the deepest tile
        // whose bounds contain the whole feature bbox) rather than the world
        // root: every level above it is a single-child pass-through, so
        // skipping them is free and yields an identical leaf set.
        let root = covering_tile(&ranges, zoom);
        split_feature_into_tiles(
            root, geom, &bbox, zoom, opts, key_lo, key_hi, &ranges, &mut out,
        );
    }
    out
}

/// Direct-path cost budget, in `tiles_spanned × vertices` clip-vertex ops. At
/// or below it the per-tile direct clip beats the recursive cascade's per-node
/// overhead; above it the cascade's `O(depth)` vertex participation wins. Tuned
/// so pre-simplified overview features (low vertex count, modest span) stay on
/// the direct — and byte-identical — path, and only genuinely huge polygons
/// recurse.
const DIRECT_CLIP_BUDGET: u64 = 100_000;

/// Number of leaf tiles the feature's bbox covers at the target zoom (both
/// x-bands when it crosses the antimeridian).
#[inline]
fn tile_span(ranges: &BboxTileRanges) -> u64 {
    let x1 = (ranges.x.1 - ranges.x.0 + 1) as u64;
    let x2 = ranges.x2.map_or(0, |(a, b)| (b - a + 1) as u64);
    let y = (ranges.y.1 - ranges.y.0 + 1) as u64;
    (x1 + x2) * y
}

/// The pre-#226 direct path: enumerate every tile the feature bbox covers and
/// clip the feature's full geometry against each (fast-pathing interior
/// features). Used for features whose direct cost is under
/// [`DIRECT_CLIP_BUDGET`]; its output is byte-identical to the historical
/// export. Appends `(key, geom)` members within `[key_lo, key_hi]` to `out`.
fn feature_tile_members_direct(
    geom: &Geometry<f64>,
    bbox: &TileBounds,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
    out: &mut Vec<(u64, Geometry<f64>)>,
) {
    for tc in tiles_for_bbox(bbox, zoom) {
        let key = tile_key(tc.x, tc.y);
        if key < key_lo || key > key_hi {
            continue;
        }
        let tb = tc.bounds();
        let buffer_deg = tb.width() * opts.tile_buffer as f64 / opts.extent as f64;
        if bbox_within_buffered(bbox, &tb, buffer_deg) {
            out.push((key, geom.clone()));
        } else if let Some(clipped) = clip_geometry(geom, &tb, buffer_deg) {
            out.push((key, clipped));
        }
    }
}

/// The deepest tile whose subtree contains every tile the feature covers — the
/// common ancestor of the target range corners. Starting the cascade here
/// instead of at the world root skips the pure single-child descent while
/// producing an identical leaf set. Antimeridian-crossing ranges (two x-bands)
/// have no single covering tile below the root, so fall back to `(0, 0, 0)`.
#[inline]
fn covering_tile(ranges: &BboxTileRanges, zoom: u8) -> TileCoord {
    if ranges.x2.is_some() {
        return TileCoord::new(0, 0, 0);
    }
    let (x_lo, x_hi) = ranges.x;
    let (y_lo, y_hi) = ranges.y;
    // Smallest shift `s` at which both corners share a tile = deepest common
    // ancestor (at zoom `zoom - s`).
    let mut s = 0u8;
    while s < zoom && !((x_lo >> s) == (x_hi >> s) && (y_lo >> s) == (y_hi >> s)) {
        s += 1;
    }
    TileCoord::new(x_lo >> s, y_lo >> s, zoom - s)
}

/// `true` when tile node `(x, y, z)`'s descendant-leaf footprint at `zoom`
/// overlaps the target ranges (i.e. the node's subtree contains at least one
/// tile the feature bbox covers).
#[inline]
fn node_overlaps_ranges(node: TileCoord, zoom: u8, ranges: &BboxTileRanges) -> bool {
    let shift = (zoom - node.z) as u32;
    let x_lo = (node.x as u64) << shift;
    let x_hi = (((node.x as u64) + 1) << shift) - 1;
    let y_lo = (node.y as u64) << shift;
    let y_hi = (((node.y as u64) + 1) << shift) - 1;
    let overlaps = |a: (u32, u32), b: (u64, u64)| a.0 as u64 <= b.1 && b.0 <= a.1 as u64;
    if !overlaps(ranges.y, (y_lo, y_hi)) {
        return false;
    }
    overlaps(ranges.x, (x_lo, x_hi)) || ranges.x2.is_some_and(|x2| overlaps(x2, (x_lo, x_hi)))
}

/// `true` when tile node `(x, y, z)`'s descendant-leaf key range overlaps the
/// partition key window `[key_lo, key_hi]`. Keys are `x`-major
/// (`(x << 32) | y`), so a node's subtree spans `[min_key, max_key]` with
/// `min = (x_lo << 32) | y_lo`, `max = (x_hi << 32) | y_hi`. The test is
/// conservative (the subtree key range has row-major gaps), which only lets a
/// branch be visited — the exact leaf key guard drops any non-member — so it
/// never skips a needed tile. It is what keeps a giant feature's per-partition
/// work bounded to the partition's slice instead of re-walking the whole
/// subtree once per partition it spans.
#[inline]
fn node_key_overlaps(node: TileCoord, zoom: u8, key_lo: u64, key_hi: u64) -> bool {
    let shift = (zoom - node.z) as u32;
    let x_lo = (node.x as u64) << shift;
    let x_hi = (((node.x as u64) + 1) << shift) - 1;
    let y_lo = (node.y as u64) << shift;
    let y_hi = (((node.y as u64) + 1) << shift) - 1;
    let min_key = (x_lo << 32) | y_lo;
    let max_key = (x_hi << 32) | y_hi;
    max_key >= key_lo && min_key <= key_hi
}

/// One node of the recursive cascade: reduce `cur` (the geometry already
/// clipped to this node's parent buffered region, with bounding box `cur_bbox`)
/// to this node, then either emit the leaf member or split into four children.
#[allow(clippy::too_many_arguments)]
fn split_feature_into_tiles(
    node: TileCoord,
    cur: &Geometry<f64>,
    cur_bbox: &TileBounds,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
    ranges: &BboxTileRanges,
    out: &mut Vec<(u64, Geometry<f64>)>,
) {
    // Prune: outside the feature's target tiles, or outside this partition.
    if !node_overlaps_ranges(node, zoom, ranges) || !node_key_overlaps(node, zoom, key_lo, key_hi) {
        return;
    }

    let tb = node.bounds();
    let buffer_deg = tb.width() * opts.tile_buffer as f64 / opts.extent as f64;

    if node.z == zoom {
        // Leaf tile. The prune above already proved this tile is in the
        // feature's target set; enforce the exact partition key window here.
        let key = tile_key(node.x, node.y);
        if key < key_lo || key > key_hi {
            return;
        }
        // Fast path (H3(c) lever 4): a feature whose bbox lies entirely within
        // the buffered tile is unaffected by clipping — emit as-is. For a
        // feature interior to the leaf this reproduces the direct loop's
        // `geom.clone()` byte-for-byte, since every ancestor also fast-pathed
        // and `cur` is still the original geometry.
        if bbox_within_buffered(cur_bbox, &tb, buffer_deg) {
            out.push((key, cur.clone()));
        } else if let Some(clipped) = clip_geometry(cur, &tb, buffer_deg) {
            out.push((key, clipped));
        }
        return;
    }

    // Internal node: shrink `cur` to this node's buffered bounds before
    // splitting, so each child clips an already-halved geometry. Clipping to
    // `node ± buffer` (not the bare node) preserves the superset chain that
    // makes the cascade equal to a direct leaf clip.
    let children = node.children().expect("non-leaf node has children");
    if bbox_within_buffered(cur_bbox, &tb, buffer_deg) {
        // Feature already inside this node's buffered bounds — no clip needed;
        // descend with the same geometry (this is what makes the coarse top of
        // the pyramid essentially free).
        for child in children {
            split_feature_into_tiles(
                child, cur, cur_bbox, zoom, opts, key_lo, key_hi, ranges, out,
            );
        }
    } else if let Some(clipped) = clip_geometry(cur, &tb, buffer_deg) {
        let cbbox = match clipped.bounding_rect() {
            Some(r) => TileBounds::new(r.min().x, r.min().y, r.max().x, r.max().y),
            None => return,
        };
        for child in children {
            split_feature_into_tiles(
                child, &clipped, &cbbox, zoom, opts, key_lo, key_hi, ranges, out,
            );
        }
    }
    // else: `cur` does not intersect this node's buffered bounds — whole
    // subtree pruned.
}

/// Test helper: run the direct path unconditionally (bypassing the dispatch
/// budget) and collect its members — the oracle the recursive cascade is
/// verified against.
#[cfg(test)]
fn members_direct_vec(
    geom: &Geometry<f64>,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
) -> Vec<(u64, Geometry<f64>)> {
    let Some(rect) = geom.bounding_rect() else {
        return Vec::new();
    };
    let bbox = TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
    let mut out = Vec::new();
    feature_tile_members_direct(geom, &bbox, zoom, opts, key_lo, key_hi, &mut out);
    out
}

/// Test helper: run the recursive cascade unconditionally (bypassing the
/// dispatch budget) and collect its members.
#[cfg(test)]
fn members_recursive_vec(
    geom: &Geometry<f64>,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
) -> Vec<(u64, Geometry<f64>)> {
    let Some(rect) = geom.bounding_rect() else {
        return Vec::new();
    };
    let bbox = TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
    let ranges = tile_ranges_for_bbox(&bbox, zoom);
    let root = covering_tile(&ranges, zoom);
    let mut out = Vec::new();
    split_feature_into_tiles(
        root, geom, &bbox, zoom, opts, key_lo, key_hi, &ranges, &mut out,
    );
    out
}

fn encode_members(
    mut members: Vec<Member>,
    zoom: u8,
    opts: &ExportOptions,
) -> Result<Vec<EncodedTile>, ExportError> {
    members.par_sort_unstable_by_key(|m| (m.key, m.seq));

    // Group boundaries (manual scan: `slice::chunk_by` needs Rust 1.77, MSRV
    // is 1.75).
    let mut groups: Vec<&[Member]> = Vec::new();
    let mut start = 0usize;
    for i in 1..=members.len() {
        if i == members.len() || members[i].key != members[start].key {
            groups.push(&members[start..i]);
            start = i;
        }
    }

    // Encode AND gzip-compress every tile in the parallel section, so the serial
    // writer loop only dedups and appends bytes (issue #227). Compression must
    // match the writer's `tile_compression` (Gzip) so `add_tile_precompressed`
    // stays byte-identical to the old serial `add_tile_with_count` path.
    groups
        .into_par_iter()
        .filter_map(|g| {
            let (x, y) = ((g[0].key >> 32) as u32, g[0].key as u32);
            let tb = TileCoord::new(x, y, zoom).bounds();
            let (data, count, oversized) = encode_tile(g, &tb, opts);
            if count == 0 {
                return None;
            }
            let hash = TileHasher::hash(&data);
            let raw_len = data.len();
            let compressed = match compression::compress(&data, Compression::Gzip) {
                Ok(c) => c,
                Err(e) => return Some(Err(ExportError::from(e))),
            };
            Some(Ok(EncodedTile {
                x,
                y,
                data: compressed,
                hash,
                raw_len,
                feature_count: count,
                oversized,
            }))
        })
        .collect()
}

/// `true` when `bbox` lies entirely within `tb` expanded by `buffer` on every
/// side. Clipping such a feature to the buffered tile is a geometric no-op, so
/// the clip can be skipped (and BooleanOps ring normalization avoided).
#[inline]
fn bbox_within_buffered(bbox: &TileBounds, tb: &TileBounds, buffer: f64) -> bool {
    bbox.lng_min >= tb.lng_min - buffer
        && bbox.lat_min >= tb.lat_min - buffer
        && bbox.lng_max <= tb.lng_max + buffer
        && bbox.lat_max <= tb.lat_max + buffer
}

/// Encode a single tile's members to MVT bytes, applying the oversized valve.
/// Returns `(bytes, features_encoded, oversized)`.
fn encode_tile(
    members: &[Member],
    tb: &TileBounds,
    opts: &ExportOptions,
) -> (Vec<u8>, usize, bool) {
    let data = build_mvt(members.iter(), tb, opts);

    match opts.tile_size_limit {
        Some(limit) if data.len() > limit && members.len() > 1 => {
            // Single, non-iterative drop pass: rank members by geometry size
            // (coordinate count) descending — the biggest features carry the
            // tile's visual signal — and keep a proportional prefix.
            let mut ranked: Vec<&Member> = members.iter().collect();
            ranked.sort_by_key(|m| std::cmp::Reverse(m.geom.coords_count()));
            let keep_frac = limit as f64 / data.len() as f64;
            let keep = ((members.len() as f64 * keep_frac).floor() as usize).max(1);
            let data = build_mvt(ranked.iter().take(keep).copied(), tb, opts);
            log::warn!(
                "oversized tile ({} bytes > {limit} limit): dropped {} of {} features (one pass)",
                data.len(),
                members.len() - keep,
                members.len()
            );
            (data, keep, true)
        }
        _ => {
            let count = members.len();
            (data, count, false)
        }
    }
}

/// Build the MVT bytes for a sequence of tile members.
fn build_mvt<'a>(
    members: impl IntoIterator<Item = &'a Member>,
    tb: &TileBounds,
    opts: &ExportOptions,
) -> Vec<u8> {
    let mut layer = LayerBuilder::new(opts.layer_name.clone()).with_extent(opts.extent);
    for (i, m) in members.into_iter().enumerate() {
        layer.add_feature(Some(i as u64), &m.geom, &m.props, tb);
    }
    let mut tb_builder = TileBuilder::new();
    tb_builder.add_layer(layer.build());
    tb_builder.build().encode_to_vec()
}

// ============================================================================
// Reading + property extraction
// ============================================================================

/// Test-support: the pre-partitioning in-memory reference path — build every
/// member for a fully materialized feature slice at `zoom` (unbounded key
/// range) and encode via the production member machinery.
#[cfg(test)]
fn encode_level_tiles(features: &[Feature], zoom: u8, opts: &ExportOptions) -> Vec<EncodedTile> {
    let mut members = Vec::new();
    for (fi, f) in features.iter().enumerate() {
        let props = Arc::new(f.props.clone());
        for (key, geom) in feature_tile_members(&f.geom, zoom, opts, 0, u64::MAX) {
            members.push(Member {
                key,
                seq: fi as u64,
                geom,
                props: Arc::clone(&props),
            });
        }
    }
    // `encode_members` returns gzip-COMPRESSED tile bytes (issue #227 moved
    // compression into the parallel encode section). This helper exists to
    // exercise MVT *encoding*, so decompress each tile back to raw MVT bytes;
    // the assertions in the tests below operate on the uncompressed payload.
    let mut tiles = encode_members(members, zoom, opts).expect("in-memory gzip is infallible");
    for t in &mut tiles {
        t.data = crate::compression::decompress(&t.data, Compression::Gzip)
            .expect("gzip roundtrip of just-compressed tile");
    }
    tiles
}

/// Test-support: read every feature (geometry + carried properties) of a level
/// band into memory.
#[cfg(test)]
fn read_level_features(
    reader: &OverviewReader,
    level_idx: usize,
    crs: Crs,
) -> Result<Vec<Feature>, ExportError> {
    let batch_reader = reader.read_level(level_idx, None)?;
    let mut out = Vec::new();
    for batch in batch_reader {
        let batch = batch?;
        decode_batch(&batch, crs, &mut out)?;
    }
    Ok(out)
}

/// Test-support: decode one record batch into [`Feature`]s, excluding the
/// `level` column and any struct/list column (the bbox covering) from
/// properties.
#[cfg(test)]
fn decode_batch(batch: &RecordBatch, crs: Crs, out: &mut Vec<Feature>) -> Result<(), ExportError> {
    let schema = batch.schema();
    let geom_idx = geometry_index(&schema).ok_or(ExportError::NoGeometryColumn)?;
    let geom_field = schema.field(geom_idx).clone();

    // Decode geometries.
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(batch.num_rows());
    extract_geometries_from_array(garr.as_ref(), &mut geoms)?;

    // Pre-extract every exportable property column once.
    let prop_cols = property_columns(&schema, geom_idx);
    let mut extracted: Vec<(String, Vec<Option<PropertyValue>>)> =
        Vec::with_capacity(prop_cols.len());
    for &(idx, ref name) in &prop_cols {
        extracted.push((name.clone(), extract_property_column(batch.column(idx))));
    }

    for row in 0..batch.num_rows() {
        let mut geom = geoms[row].clone();
        if matches!(crs, Crs::Epsg3857) {
            geom = reproject_3857_to_4326(&geom);
        }
        let mut props = Vec::with_capacity(extracted.len());
        for (name, col) in &extracted {
            if let Some(v) = &col[row] {
                props.push((name.clone(), v.clone()));
            }
        }
        out.push(Feature { geom, props });
    }
    Ok(())
}

/// The index of the primary geometry column (`geometry`, else first `geom*`).
fn geometry_index(schema: &Schema) -> Option<usize> {
    schema
        .fields()
        .iter()
        .position(|f| f.name() == "geometry")
        .or_else(|| {
            schema
                .fields()
                .iter()
                .position(|f| f.name().contains("geom"))
        })
}

/// The `(index, name)` of every exportable property column: everything that is
/// not the geometry column, not the `level` column, and whose type is a
/// supported MVT scalar (struct/list covering columns are skipped).
fn property_columns(schema: &Schema, geom_idx: usize) -> Vec<(usize, String)> {
    schema
        .fields()
        .iter()
        .enumerate()
        .filter(|&(i, f)| {
            i != geom_idx
                && !f.name().eq_ignore_ascii_case(LEVEL_COLUMN)
                && is_supported_scalar(f.data_type())
        })
        .map(|(i, f)| (i, f.name().clone()))
        .collect()
}

/// MVT-encodable Arrow scalar types.
fn is_supported_scalar(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Boolean
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float32
            | DataType::Float64
    )
}

/// Field-type metadata (`name -> "String"|"Number"|"Boolean"`) for the archive
/// `vector_layers.fields` block.
fn field_metadata(schema: &Schema, geom_idx: Option<usize>) -> HashMap<String, String> {
    let geom_idx = geom_idx.unwrap_or(usize::MAX);
    let mut out = HashMap::new();
    for (i, f) in schema.fields().iter().enumerate() {
        if i == geom_idx || f.name().eq_ignore_ascii_case(LEVEL_COLUMN) {
            continue;
        }
        let ty = match f.data_type() {
            DataType::Utf8 | DataType::LargeUtf8 => "String",
            DataType::Boolean => "Boolean",
            dt if is_supported_scalar(dt) => "Number",
            _ => continue,
        };
        out.insert(f.name().clone(), ty.to_string());
    }
    out
}

/// Extract one Arrow column into per-row optional [`PropertyValue`]s. Null cells
/// and unsupported types yield `None`.
fn extract_property_column(col: &dyn arrow_array::Array) -> Vec<Option<PropertyValue>> {
    let n = col.len();
    macro_rules! prim {
        ($ty:ty, $variant:ident, $cast:ty) => {{
            let a = col.as_primitive::<$ty>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(PropertyValue::$variant(a.value(i) as $cast))
                    }
                })
                .collect()
        }};
    }
    macro_rules! strcol {
        ($off:ty) => {{
            let a = col.as_string::<$off>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(PropertyValue::String(a.value(i).to_string()))
                    }
                })
                .collect()
        }};
    }
    match col.data_type() {
        DataType::Utf8 => strcol!(i32),
        DataType::LargeUtf8 => strcol!(i64),
        DataType::Boolean => {
            let a = col.as_boolean();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(PropertyValue::Bool(a.value(i)))
                    }
                })
                .collect()
        }
        DataType::Int8 => prim!(Int8Type, Int, i64),
        DataType::Int16 => prim!(Int16Type, Int, i64),
        DataType::Int32 => prim!(Int32Type, Int, i64),
        DataType::Int64 => prim!(Int64Type, Int, i64),
        DataType::UInt8 => prim!(UInt8Type, UInt, u64),
        DataType::UInt16 => prim!(UInt16Type, UInt, u64),
        DataType::UInt32 => prim!(UInt32Type, UInt, u64),
        DataType::UInt64 => prim!(UInt64Type, UInt, u64),
        DataType::Float32 => prim!(Float32Type, Float, f32),
        DataType::Float64 => prim!(Float64Type, Double, f64),
        _ => vec![None; n],
    }
}

// ============================================================================
// CRS
// ============================================================================

/// Detect the overview file's CRS. A null/absent CRS is GeoParquet's default
/// EPSG:4326. EPSG:3857 is accepted (reprojected on read); anything else errors.
fn detect_crs(path: &Path) -> Result<Crs, ExportError> {
    let info = crate::quality::extract_crs(path).map_err(ExportError::Core)?;
    if info.is_wgs84 {
        return Ok(Crs::Epsg4326);
    }
    if let Some(id) = &info.identifier {
        let up = id.to_uppercase();
        if up.contains("3857") || up.contains("900913") {
            return Ok(Crs::Epsg3857);
        }
    }
    // A null CRS (identifier None, name None) is the GeoParquet default 4326.
    if info.identifier.is_none() && info.name.is_none() {
        return Ok(Crs::Epsg4326);
    }
    Err(ExportError::UnsupportedCrs {
        crs: info
            .identifier
            .or(info.name)
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

/// Reproject one EPSG:3857 point (meters) to EPSG:4326 (lon/lat degrees).
#[inline]
fn webmerc_to_lnglat(x: f64, y: f64) -> (f64, f64) {
    let lng = x / WEBMERC_HALF_M * 180.0;
    let lat = (2.0 * (y / WEBMERC_HALF_M * std::f64::consts::PI).exp().atan()
        - std::f64::consts::FRAC_PI_2)
        .to_degrees();
    (lng, lat)
}

/// Reproject a geometry from EPSG:3857 (meters) to EPSG:4326 (lon/lat degrees)
/// so the geographic tile grid math applies.
fn reproject_3857_to_4326(g: &Geometry<f64>) -> Geometry<f64> {
    g.map_coords(|c| {
        let (lng, lat) = webmerc_to_lnglat(c.x, c.y);
        geo::coord! { x: lng, y: lat }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{command_decode, zigzag_decode};
    use crate::overview::level::{gsd, Level, Mode};
    use crate::overview::writer::{
        LevelSpec, LevelWriteOutcome, OverviewWriter, OverviewWriterOptions,
    };
    use crate::vector_tile::tile::GeomType;
    use crate::vector_tile::Tile;
    use arrow_array::{Int64Array, RecordBatch, StringArray};
    use arrow_schema::{Field, Schema};
    use geo::{Geometry, LineString, Point};
    use geoarrow::array::GeometryBuilder;
    use geoarrow::datatypes::GeometryType;

    // --- fixture builders ----------------------------------------------------

    fn build_geometry_array(geoms: &[Geometry<f64>]) -> geoarrow::array::GeometryArray {
        let typ = GeometryType::new(Default::default());
        let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
        b.extend_from_iter(geoms.iter().map(Some));
        b.finish()
    }

    fn geometry_field() -> Field {
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

    /// Write a 2-level duplicating overview fixture. `level_geoms[k]` are the
    /// geometries (and ids) at level k. Levels use z = 2 + 2k.
    fn write_fixture(path: &Path, level_geoms: &[(Vec<i64>, Vec<Geometry<f64>>)]) -> OverviewsMeta {
        let schema = Arc::new(source_schema());
        let specs: Vec<LevelSpec> = (0..level_geoms.len())
            .map(|k| LevelSpec::new(gsd((2 + 2 * k) as u8), Some((2 + 2 * k) as u8)))
            .collect();
        let mut opts = OverviewWriterOptions::new(Mode::Duplicating, specs);
        opts.max_row_group_size = 10_000;
        let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
        for (k, (ids, geoms)) in level_geoms.iter().enumerate() {
            assert_eq!(
                writer
                    .write_level(
                        k,
                        Some(ids.len()),
                        std::iter::once(batch(&schema, ids, geoms)),
                    )
                    .unwrap(),
                LevelWriteOutcome::Written
            );
        }
        writer.finish().unwrap()
    }

    /// Decode all (geom_type, coords, keys) of a raw MVT tile's single layer.
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

    /// Rich 2-level fixture used by the byte-equivalence tests: points, a
    /// seam-crossing line, a concave polygon and a many-vertex line, with
    /// id/name properties, at zooms 2 and 4.
    fn equivalence_fixture(path: &Path) {
        let pa = Geometry::Point(Point::new(-120.0, 40.0));
        let pb = Geometry::Point(Point::new(120.0, -40.0));
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
    /// produced by the pre-refactor per-zoom BTreeMap implementation. The
    /// reference hash was captured from that implementation (commit b8a1635)
    /// on this exact fixture before the partitioned-streaming rewrite.
    ///
    /// REPIN (issue #112): the MVT winding fix (orient_polygon_for_mvt now
    /// emits spec-compliant positive-area exterior rings) intentionally
    /// changed polygon command bytes; the hash was re-captured from the
    /// fixed encoder. The anchor still guards the export restructure — the
    /// partition-invariance test alongside it is unchanged.
    #[test]
    fn export_archive_matches_pre_refactor_reference() {
        let tin = tempfile::NamedTempFile::new().unwrap();
        equivalence_fixture(tin.path());
        let tout = tempfile::NamedTempFile::new().unwrap();
        let opts = ExportOptions {
            layer_name: "ref".to_string(),
            ..Default::default()
        };
        let report = export_pmtiles(tin.path(), tout.path(), &opts).unwrap();
        assert_eq!(report.total_tiles, 12);
        let bytes = std::fs::read(tout.path()).unwrap();
        assert_eq!(
            format!("{:016x}", crate::dedup::TileHasher::hash(&bytes)),
            "a108f1607d994a92",
            "archive bytes diverged from the pre-refactor reference"
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
        let r_many =
            export_pmtiles_with_partition_target(tin.path(), t_many.path(), &opts, 1).unwrap();
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

    // --- recursive splitter equivalence (issue #226) -------------------------

    /// Collect a single feature's `(key, geom)` members into a key→geom map,
    /// asserting the "one member per feature per tile" invariant both paths
    /// hold.
    fn members_by_key(v: Vec<(u64, Geometry<f64>)>) -> HashMap<u64, Geometry<f64>> {
        let mut m = HashMap::new();
        for (k, g) in v {
            assert!(m.insert(k, g).is_none(), "duplicate member for one feature");
        }
        m
    }

    /// Encode one tile member to MVT bytes on its own tile bounds — the exact
    /// quantized output the archive would carry, so byte-equality here means
    /// tile-output equivalence.
    fn tile_mvt(key: u64, geom: &Geometry<f64>, zoom: u8, opts: &ExportOptions) -> Vec<u8> {
        let (x, y) = ((key >> 32) as u32, key as u32);
        let tb = TileCoord::new(x, y, zoom).bounds();
        let m = Member {
            key,
            seq: 0,
            geom: geom.clone(),
            props: Arc::new(Vec::new()),
        };
        build_mvt(std::iter::once(&m), &tb, opts)
    }

    /// The recursive cascade must emit the same tile-key set as the direct
    /// (full-geometry, per-tile) oracle, and each tile's encoded MVT bytes must
    /// match.
    fn assert_recursive_matches_direct(geom: &Geometry<f64>, zoom: u8, opts: &ExportOptions) {
        let rec = members_by_key(members_recursive_vec(geom, zoom, opts, 0, u64::MAX));
        let dir = members_by_key(members_direct_vec(geom, zoom, opts, 0, u64::MAX));
        let mut rk: Vec<u64> = rec.keys().copied().collect();
        rk.sort_unstable();
        let mut dk: Vec<u64> = dir.keys().copied().collect();
        dk.sort_unstable();
        assert_eq!(rk, dk, "tile key set diverges at z{zoom}");
        for k in rk {
            assert_eq!(
                tile_mvt(k, &rec[&k], zoom, opts),
                tile_mvt(k, &dir[&k], zoom, opts),
                "tile ({}, {}) MVT diverges at z{zoom}",
                (k >> 32) as u32,
                k as u32,
            );
        }
    }

    /// Feature corpus + zoom caps chosen so the oracle's `tiles×vertices` cost
    /// stays bounded: interior/seam-crossing features run deep, wide features
    /// only at coarse zooms.
    fn equivalence_corpus() -> Vec<(&'static str, Geometry<f64>, Vec<u8>)> {
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

    /// Cross-check the recursive cascade against the direct oracle across a
    /// corpus that exercises interior, seam-crossing, concave, wide, dense and
    /// antimeridian features at a range of zooms (incl. depth-14 cascades). The
    /// full unbounded key range stands in for a single all-covering partition.
    #[test]
    fn recursive_split_matches_direct_oracle() {
        let opts = ExportOptions::default();
        for (name, geom, zooms) in equivalence_corpus() {
            for z in zooms {
                // Panics carry the feature name via the zoom-tagged messages;
                // prefix here for quick triage.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    assert_recursive_matches_direct(&geom, z, &opts);
                }))
                .unwrap_or_else(|_| panic!("equivalence failed for feature '{name}' at z{z}"));
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
                let full = members_by_key(feature_tile_members(&geom, z, &opts, 0, u64::MAX));
                if full.is_empty() {
                    continue;
                }
                // Split at the median emitted key so both halves are non-empty.
                let mut keys: Vec<u64> = full.keys().copied().collect();
                keys.sort_unstable();
                let split = keys[keys.len() / 2];
                let lo = feature_tile_members(&geom, z, &opts, 0, split);
                let hi = feature_tile_members(&geom, z, &opts, split + 1, u64::MAX);
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
        assert_eq!(zoom_for_level(&meta, 0), 4);
        assert_eq!(zoom_for_level(&meta, 1), 7);
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
        assert_eq!(zoom_for_level(&meta, 0), 3);
        assert_eq!(zoom_for_level(&meta, 1), 8);
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
        let schema = Arc::new(source_schema());
        let specs: Vec<LevelSpec> = (0..level_geoms.len())
            .map(|k| LevelSpec::new(gsd((2 + 2 * k) as u8), Some((2 + 2 * k) as u8)))
            .collect();
        let mut opts = OverviewWriterOptions::new(Mode::Partitioning, specs);
        opts.max_row_group_size = 2;
        let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
        for (k, (ids, geoms)) in level_geoms.iter().enumerate() {
            assert_eq!(
                writer
                    .write_level(
                        k,
                        Some(ids.len()),
                        std::iter::once(batch(&schema, ids, geoms))
                    )
                    .unwrap(),
                LevelWriteOutcome::Written
            );
        }
        writer.finish().unwrap()
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

        let scans = scan_all_levels(&reader, Crs::Epsg4326, &meta).unwrap();
        assert_eq!(scans.len(), num_levels);

        // Feature counts must equal the accumulating prefix sizes (3, 3+4, 3+4+5).
        assert_eq!(scans[0].feature_count, 3);
        assert_eq!(scans[1].feature_count, 7);
        assert_eq!(scans[2].feature_count, 12);

        for (level_idx, scan) in scans.iter().enumerate() {
            let zoom = zoom_for_level(&meta, level_idx);
            let oracle = scan_level(&reader, level_idx, Crs::Epsg4326, zoom).unwrap();
            assert_eq!(
                *scan, oracle,
                "fan-out scan differs from per-level prefix scan at level {level_idx}"
            );
            // Guard against a vacuous pass: the level must actually size tiles.
            assert!(!scan.tile_counts.is_empty());
            assert!(scan.bounds.is_some());
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
        let tiles = encode_level_tiles(&feats, 2, &ExportOptions::default());
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
        let tiles = encode_level_tiles(&feats, 4, &opts);
        assert!(tiles.len() >= 2, "wide line must span multiple tiles");
        let extent = opts.extent as i32;
        let slack = extent * opts.tile_buffer as i32 / opts.extent as i32 + 4;
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
        let tiles = encode_level_tiles(&feats, 4, &opts);
        assert_eq!(tiles.len(), 1, "fully-contained feature => exactly 1 tile");
        assert_eq!(tiles[0].feature_count, 1);

        let tc = TileCoord::new(tiles[0].x, tiles[0].y, 4);
        let member = Member {
            key: tile_key(tiles[0].x, tiles[0].y),
            seq: 0,
            geom: poly.clone(),
            props: Arc::new(vec![]),
        };
        let expected = build_mvt([&member], &tc.bounds(), &opts);
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
        let tiles = encode_level_tiles(&feats, 4, &opts);
        assert!(tiles.len() >= 2, "seam-crossing line must span tiles");
        let extent = opts.extent as i32;
        let slack = (opts.tile_buffer as i32) + 4;
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

        // No limit: no oversized tiles.
        let none = encode_level_tiles(&feats, 6, &ExportOptions::default());
        assert!(none.iter().all(|t| !t.oversized));
        let full_count: usize = none.iter().map(|t| t.feature_count).sum();

        // Tiny limit: valve fires and drops features.
        let opts = ExportOptions {
            tile_size_limit: Some(64),
            ..Default::default()
        };
        let limited = encode_level_tiles(&feats, 6, &opts);
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
}
