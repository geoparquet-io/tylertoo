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
//!    process them in **waves** of [`resolve_partition_wave`] partitions. Each wave
//!    reads the band **once** (row groups pruned to the wave's combined bbox),
//!    splits every feature into its tiles with a **top-down recursive quadtree
//!    cascade** (see [`feature_tile_members`]) — each feature clipped once per
//!    pyramid level into an already-reduced child region down to the target
//!    zoom, so a vertex takes part in `O(depth)` clips rather than
//!    `O(tiles_spanned)` (issue #226) — and *routes* each resulting member to
//!    its owning partition (see [`process_wave`]). Sharing one read+decode
//!    across a wave replaces the old per-partition re-read (issue #228): a level
//!    now costs `ceil(P / PARTITION_WAVE)` band reads, not `P`. The per-tile
//!    clips reuse the [`clip_geometry_simple`] entry point, MVT-encode via
//!    [`crate::mvt`], and each finished partition's tiles stream immediately to
//!    [`StreamingPmtilesWriter`]. Tiles are written in ascending `(x, y)` order
//!    per zoom — the historical order — so the archive's tile-data layout,
//!    deduplication, and directory are unchanged.
//!
//!    In **partitioning** mode the per-level wave read walks the accumulating
//!    row-group prefix (§5.1), so a coarse row group would be re-read and
//!    re-decoded by every finer level once per wave. Pass 2 therefore switches
//!    to a **single-read fan-out** there (issue #235, the pass-2 analogue of
//!    the #233 scan fix): one reader pass streams every band once, clips each
//!    feature at every including level's zoom, and buffers the members in a
//!    RAM/spill-backed [`MemberStore`] that the same wave loop drains — see
//!    the "Pass-2 single-read fan-out" section below. Duplicating bands are
//!    self-contained, so the per-level wave read stays.
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
//! [`resolve_partition_wave`] partitions processed concurrently): each in-flight
//! partition holds its (feature × intersecting-tile) clipped members plus its
//! encoded tiles. That per-partition transient is usually bounded by the
//! partition target — **not** the zoom band's feature count — but a single
//! dense tile that overshoots the target is one partition whose members are
//! *unbounded* by it. On `auto` the wave width is therefore sized per level from
//! the densest planned partition ([`memory_safe_level_wave`], #311), narrowing
//! (down to serial if need be) so a dense finest zoom stays within RAM instead
//! of OOM-killing the run.
//! The scan pass keeps only per-tile member counts (`O(#tiles)`), and the
//! PMTiles writer streams tile bytes to a temp file, keeping only directory
//! entries. There is **no** per-zoom or global accumulation.
//!
//! The partitioning single-read path (#235) buffers every level's clipped
//! members between its fill and drain phases, but its **RAM** ceiling is
//! unchanged: the auto RAM-vs-spill policy shared with the converter's pass-2
//! sinks ([`auto_backing`]) keeps small buffered sets in RAM and spills large
//! ones to a temp file, the fill's in-RAM buckets are capped at
//! [`MEMBER_STORE_RAM_BUDGET`], and the drain holds one wave of partitions at
//! a time — the same `O(one wave)` ceiling as the per-level read path.
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
use std::fs::File;
use std::io::BufWriter;
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
use crossbeam_channel::bounded;
use geo::{BoundingRect, CoordsIter, Geometry, MapCoords};
use geoarrow::array::from_arrow_array;
use geoarrow_array::GeoArrowArray;
use prost::Message;
use rayon::prelude::*;
use serde::Serialize;
use tempfile::NamedTempFile;

use crate::batch_processor::extract_geometries_from_array;
use crate::clip::{clip_geometry_simple, geometry_is_simple};
use crate::compression::{self, Compression};
use crate::dedup::TileHasher;
use crate::mvt::{LayerBuilder, PropertyValue, TileBuilder};
use crate::pmtiles_writer::StreamingPmtilesWriter;
use crate::tile::{tile_ranges_for_bbox, tiles_for_bbox, BboxTileRanges, TileBounds, TileCoord};

use super::level::{zoom_for_gsd, Crs, Mode, OverviewsMeta};
use super::pipeline::{auto_backing, available_memory_bytes, SinkBacking};
use super::reader::{OverviewReader, ReaderError};
use super::writer::LEVEL_COLUMN;

/// Default MVT tile extent (matches [`crate::mvt::DEFAULT_EXTENT`]).
const DEFAULT_EXTENT: u32 = 4096;

/// Default per-tile edge buffer, in tile pixels (tippecanoe default is 5; we
/// use 8 to match the tile pipeline's historical default).
const DEFAULT_TILE_BUFFER_PX: u32 = 8;

/// Default per-tile MVT size cap, in bytes (500 KiB — matches what `500K`
/// parses to and tippecanoe's on-by-default 500K bar; issue #280). A tile whose
/// encoded MVT exceeds this trips the single-pass drop valve (see
/// [`ExportOptions::tile_size_limit`]). Set `tile_size_limit` to `None` (CLI:
/// `--max-tile-size 0`; Python: `tile_size_limit=0`) to disable the cap.
pub const DEFAULT_TILE_SIZE_LIMIT: usize = 500 * 1024;

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
    /// Per-tile MVT size limit in **bytes**. When `Some(limit)` with
    /// `limit > 0`, a tile whose encoded size exceeds the limit triggers the
    /// single, non-iterative safety valve: [`select_kept_members`] chooses which
    /// features survive (largest-first for sized geometries, uniform spatial
    /// stride for point-dominated tiles — see that function) and the tile is
    /// re-encoded once. When `None` (or `Some(0)`), no size limit is enforced
    /// and `oversized_tiles` is always 0.
    ///
    /// Defaults to `Some(`[`DEFAULT_TILE_SIZE_LIMIT`]`)` (500 KiB — tippecanoe
    /// parity, issue #280).
    pub tile_size_limit: Option<usize>,
    /// Skip the i_overlay boundary-bridge fallback for features whose rings are
    /// already proven simple (issue #239). On a simple ring, Sutherland–Hodgman's
    /// boundary-following clip is a self-touching polygon that is area- and
    /// fill-equivalent to the i_overlay split under nonzero winding, so the
    /// fallback (which fires on ~94% of fine-zoom polygon clips) is wasted work.
    /// Non-simple inputs always keep the fallback, preserving the #94 U-shape
    /// fix. Defaults to `true` (fast path on); the S-H clip of a simple ring is
    /// render-equivalent to the fallback but stored rotated to a different start
    /// vertex, so disable it (`--no-simple-clip-fastpath`) only when byte-stable
    /// tile output is required.
    pub simple_clip_fastpath: bool,
    /// Number of partitions processed per band read (the export concurrency
    /// knob). Default [`PARTITION_WAVE_AUTO`], which [`resolve_partition_wave`]
    /// expands at export start via the memory-budget preflight (#303): the
    /// machine's available parallelism, capped by how many estimated
    /// per-partition transients fit in a fraction of available RAM, floored
    /// at [`PARTITION_WAVE_MIN`] (falls back to the fixed
    /// [`PARTITION_WAVE_FALLBACK_MAX`] cap when RAM cannot be probed). Any
    /// explicit positive value is honoured verbatim. Wider waves keep more
    /// cores busy at proportionally more peak memory (`O(partition_wave)`
    /// partitions resident) and read a wider combined-bbox band per wave.
    /// Output is byte-identical for every value — the wave is a scheduling
    /// concern only.
    pub partition_wave: usize,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            layer_name: "overview".to_string(),
            tile_buffer: DEFAULT_TILE_BUFFER_PX,
            extent: DEFAULT_EXTENT,
            tile_size_limit: Some(DEFAULT_TILE_SIZE_LIMIT),
            simple_clip_fastpath: true,
            partition_wave: PARTITION_WAVE_AUTO,
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

/// Sentinel for [`ExportOptions::partition_wave`] requesting automatic sizing
/// from the machine's available parallelism (see [`resolve_partition_wave`]).
/// This is the library default so an export uses the cores it is given without
/// the caller having to know the box (#293).
pub const PARTITION_WAVE_AUTO: usize = 0;

/// Lower clamp for the auto-sized partition wave. Matches the historical
/// hard-coded width (#228): auto never schedules *narrower* than the tuned
/// baseline, so a small box behaves exactly as before.
pub const PARTITION_WAVE_MIN: usize = 6;

/// Upper clamp for the auto-sized partition wave **when available RAM cannot
/// be probed** (non-Linux without the env override, unreadable
/// `/proc/meminfo`).
///
/// This is #293's original fixed cap: it saturates a 16-core box while
/// bounding the wave transient to a known multiple of the old width-6
/// baseline (measured 1.44× RSS on germany-segments). Where a RAM signal
/// *is* available, the memory-budget preflight (#303) replaces this cap —
/// see [`resolve_partition_wave`] — so a >16-core box with RAM to spare
/// widens past it on `auto`. Explicit values are never clamped by it.
pub const PARTITION_WAVE_FALLBACK_MAX: usize = 16;

/// Fraction of *available* system RAM the resident wave transient may claim
/// in the auto preflight (#303).
///
/// Deliberately lower than the convert-side `AUTO_RAM_FRACTION` (0.6): the
/// wave transient sits *on top of* export baseline state the estimate does
/// not model (scan accumulators, decode buffers, PMTiles writer directory),
/// so the budget leaves headroom for that baseline. Bias is toward not
/// OOMing — a too-narrow wave only costs wall clock.
const EXPORT_WAVE_RAM_FRACTION: f64 = 0.5;

/// Flat fallback estimate of one wave slot's transient memory cost, used **only
/// by the upfront ceiling** [`memory_safe_wave`] and when the data-aware
/// per-level estimate ([`memory_safe_level_wave`], #311) cannot be computed
/// (finest-level row size unknown).
///
/// Measured (2026-07, #293 benchmark, germany-segments 19.2M-feature z14
/// band): widening 6 → 16 lifted peak RSS 588 → 848 MiB, i.e. ~26 MiB per
/// wave slot. It was calibrated on **lines** and does **not** generalize: on
/// dense finest-zoom polygon data a single dense tile is one partition holding
/// millions of members, so the true per-slot transient runs ~120× this (#311,
/// Brazil field boundaries: ~7.5 GiB/slot). That is why the binding memory
/// guard is now the per-level [`memory_safe_level_wave`], which sizes from the
/// densest planned partition's actual member count; this constant survives only
/// as the coarse upfront ceiling and the last-resort fallback.
const PARTITION_SLOT_TRANSIENT_BYTES: u64 = 64 * 1024 * 1024;

/// In-memory inflation applied to the finest level's mean **stored**
/// (uncompressed Parquet) bytes/row to estimate a resident clipped **member**'s
/// cost (#311).
///
/// A member holds a decoded `geo::Geometry<f64>` (16 B/coord, no column
/// compression, `Vec` capacity slack) and is briefly co-resident with its
/// gzip-encoded MVT copy in the same wave, so 2× the stored size is a
/// deliberately high bias — which is what we want, since a too-narrow wave only
/// costs wall clock while an OOM loses the whole run.
const MEMBER_MEMORY_INFLATION: u64 = 2;

/// The widest wave the memory budget allows: how many
/// [`PARTITION_SLOT_TRANSIENT_BYTES`] slots fit in
/// [`EXPORT_WAVE_RAM_FRACTION`] of available RAM, or
/// [`PARTITION_WAVE_FALLBACK_MAX`] when RAM is unknown (the portable,
/// deterministic #293 behaviour where no probe exists).
fn memory_safe_wave(available_ram_bytes: Option<u64>) -> usize {
    match available_ram_bytes {
        Some(ram) => {
            let budget = ((ram as f64) * EXPORT_WAVE_RAM_FRACTION) as u64;
            (budget / PARTITION_SLOT_TRANSIENT_BYTES) as usize
        }
        None => PARTITION_WAVE_FALLBACK_MAX,
    }
}

/// The auto partition-wave decision (#303): pure so it is unit-testable
/// across the core-count × RAM grid, mirroring #294's `auto_backing`.
///
/// `max(PARTITION_WAVE_MIN, min(cores, memory_safe_wave))` — cores are the
/// useful upper bound (a wave wider than the core count gains nothing), the
/// memory-safe wave keeps the transient inside the RAM budget, and the floor
/// preserves the historical width-6 baseline (its worst-case transient is
/// small and known-safe, and narrower waves only lose throughput).
fn auto_partition_wave(cores: usize, available_ram_bytes: Option<u64>) -> usize {
    cores
        .min(memory_safe_wave(available_ram_bytes))
        .max(PARTITION_WAVE_MIN)
}

/// The memory-safe wave width for a **single level** (#311), the fix for the
/// flat [`PARTITION_SLOT_TRANSIENT_BYTES`] under-count that OOM-killed dense
/// exports on `auto`.
///
/// Where the upfront [`memory_safe_wave`] sizes from a flat per-slot constant,
/// this uses the level's *actual* densest planned partition:
/// `max(partition.members) × mean_member_bytes × [`MEMBER_MEMORY_INFLATION`]`
/// directly estimates the largest partition's resident geometry volume. This
/// captures the real driver — a single dense tile that overshoots
/// [`DEFAULT_PARTITION_TARGET`] is one partition, wholly unbounded by the target
/// — which the flat constant ignores entirely.
///
/// The result is clamped to `ceiling` (the auto/core-sized upper bound, so this
/// never *widens* a wave) but is deliberately allowed to fall **below**
/// [`PARTITION_WAVE_MIN`], down to 1: on pathologically dense data, memory
/// safety outranks the historical throughput floor (a serial wave that finishes
/// beats an OOM that never does).
///
/// Falls back to `ceiling` (prior behaviour) when the per-member size is unknown
/// (empty finest level) or RAM cannot be probed — the same conditions under
/// which the flat preflight already governed.
fn memory_safe_level_wave(
    ceiling: usize,
    partitions: &[Partition],
    mean_member_bytes: Option<u64>,
    available_ram_bytes: Option<u64>,
) -> usize {
    let (Some(mean), Some(ram)) = (mean_member_bytes.filter(|&b| b > 0), available_ram_bytes)
    else {
        return ceiling;
    };
    let max_members = partitions.iter().map(|p| p.members).max().unwrap_or(0);
    if max_members == 0 {
        return ceiling;
    }
    // Saturating throughout: an absurd member count or byte size clamps the
    // per-slot estimate to "definitely serial" (wave 1) rather than overflowing.
    let per_slot = (max_members as u64)
        .saturating_mul(mean)
        .saturating_mul(MEMBER_MEMORY_INFLATION)
        .max(1);
    let budget = ((ram as f64) * EXPORT_WAVE_RAM_FRACTION) as u64;
    let safe = (budget / per_slot).max(1) as usize;
    ceiling.min(safe)
}

/// Resolve a requested partition-wave width to a concrete value.
///
/// [`PARTITION_WAVE_AUTO`] (0) runs the memory-budget preflight (#303): the
/// width is the machine's [`std::thread::available_parallelism`], capped by
/// how many per-partition transients (estimated
/// [`PARTITION_SLOT_TRANSIENT_BYTES`] each) fit in
/// [`EXPORT_WAVE_RAM_FRACTION`] of available RAM (probe shared with the
/// convert-side `--profile auto`: `TYLERTOO_AUTO_MEM_LIMIT_BYTES` override,
/// then `/proc/meminfo` `MemAvailable`), floored at [`PARTITION_WAVE_MIN`].
/// When RAM cannot be probed the cap falls back to
/// [`PARTITION_WAVE_FALLBACK_MAX`], reproducing #293's fixed clamp. Any
/// explicit positive value is honoured verbatim (uncapped — the caller opted
/// in to the memory cost).
///
/// The output is a **scheduling** parameter only: it sets how many partitions
/// are processed per band read, never which features land in which tile or the
/// order tiles reach the writer. Every wave width therefore produces a
/// byte-identical archive (pinned by `export_wave_width_is_byte_invariant` and
/// the partition-invariance / frozen-hash tests).
pub fn resolve_partition_wave(requested: usize) -> usize {
    if requested == PARTITION_WAVE_AUTO {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(PARTITION_WAVE_MIN);
        auto_partition_wave(cores, available_memory_bytes())
    } else {
        requested
    }
}

/// Resolve the partition-wave width (running the #303 memory preflight when
/// the caller left it at [`PARTITION_WAVE_AUTO`]) and log the chosen width
/// alongside the inputs that produced it (detected cores, memory-safe cap,
/// available RAM) once at export start, so both export core utilization
/// (#293) and the preflight decision (#303) are observable — mirrors the
/// `[convert] pass2 auto` decision log line.
fn resolve_and_log_partition_wave(requested: usize) -> usize {
    let wave = resolve_partition_wave(requested);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    if requested == PARTITION_WAVE_AUTO {
        let available = available_memory_bytes();
        let avail_str = available.map_or_else(
            || "unknown".to_string(),
            |b| format!("{} MiB", b / (1024 * 1024)),
        );
        log::info!(
            "[export] partition wave: {wave} partition(s) per band read \
             (auto: {cores} core(s), memory-safe cap {}, avail {avail_str})",
            memory_safe_wave(available),
        );
    } else {
        log::info!(
            "[export] partition wave: {wave} partition(s) per band read \
             (explicit; {cores} core(s) detected)"
        );
    }
    wave
}

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
    export_pmtiles_impl(
        input_path,
        output_path,
        options,
        partition_target,
        false,
        None,
    )
}

/// Full implementation with the #235 test knobs: `force_legacy_pass2` pins the
/// pre-#235 per-level wave-read pass 2 (the byte-identity oracle the
/// single-read fan-out is tested against; also the production duplicating
/// path), and `backing_override` forces the member store's RAM/spill backing
/// instead of the auto decision. Production callers pass `(false, None)`.
fn export_pmtiles_impl(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &ExportOptions,
    partition_target: usize,
    force_legacy_pass2: bool,
    backing_override: Option<SinkBacking>,
) -> Result<ExportReport, ExportError> {
    let start = Instant::now();
    let input_path = input_path.as_ref();

    let reader = OverviewReader::open(input_path)?;
    let meta = reader.meta().clone();
    let crs = detect_crs(input_path)?;

    let num_levels = reader.num_levels();
    let min_zoom = zoom_for_level(&meta, 0);
    let max_zoom = zoom_for_level(&meta, num_levels - 1);

    // Resolve the partition-wave *ceiling* once (auto-sizes from available cores
    // when the caller left it at PARTITION_WAVE_AUTO) and surface it (#293). On
    // `auto` this is only an upper bound: each level narrows it further from its
    // own densest partition (#311) — see the per-level `memory_safe_level_wave`
    // below. An explicit `--partition-wave N` is honoured verbatim and never
    // narrowed (the caller opted into the memory cost).
    let ceiling_wave = resolve_and_log_partition_wave(options.partition_wave);
    let auto_wave = options.partition_wave == PARTITION_WAVE_AUTO;
    // Data-aware per-member size signal for the #311 guard: the finest level's
    // mean uncompressed row bytes, read once from the Parquet metadata. Probe
    // RAM once too so every level sizes against the same budget.
    let mean_member_bytes = reader.finest_level_mean_row_bytes();
    let available_ram = available_memory_bytes();

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

    // Plan every level's partitions and wave width up front. The partitioning
    // single-read fill (#235) routes members by (level, partition, wave), so
    // the drain loop below must consume the exact same plan it was filled
    // against; hoisting the planning out of the level loop guarantees that.
    let plans: Vec<LevelPlan> = scans
        .iter()
        .enumerate()
        .map(|(level_idx, scan)| {
            let zoom = zoom_for_level(&meta, level_idx);

            // Split the zoom's tiles into contiguous ascending (x, y) ranges
            // of roughly `partition_target` members each.
            let partitions = plan_partitions(&scan.tile_counts, zoom, partition_target);

            // Per-level memory guard (#311): on `auto`, narrow the wave from
            // THIS level's densest planned partition so a dense finest zoom
            // does not OOM the whole run. Explicit waves are honoured verbatim.
            let wave = if auto_wave {
                let w = memory_safe_level_wave(
                    ceiling_wave,
                    &partitions,
                    mean_member_bytes,
                    available_ram,
                );
                if w < ceiling_wave {
                    let densest = partitions.iter().map(|p| p.members).max().unwrap_or(0);
                    let budget_mib = available_ram
                        .map(|r| ((r as f64 * EXPORT_WAVE_RAM_FRACTION) as u64) / (1024 * 1024))
                        .unwrap_or(0);
                    log::info!(
                        "[export] level {}/{num_levels} z{zoom}: wave {ceiling_wave} → {w} \
                         (densest partition {densest} members × ~{} B/member × {MEMBER_MEMORY_INFLATION}, \
                         {budget_mib} MiB budget) — #311 density guard",
                        level_idx + 1,
                        mean_member_bytes.unwrap_or(0),
                    );
                }
                w
            } else {
                ceiling_wave
            };
            LevelPlan {
                zoom,
                partitions,
                wave,
            }
        })
        .collect();

    // Pass 2 read strategy (#235): in partitioning mode a level's render set
    // is the accumulating row-group prefix (§5.1), so the legacy per-level
    // wave read re-reads and re-decodes every coarse row group once per finer
    // level per wave. Instead read every band exactly once, clip each feature
    // at every including level's zoom during that single pass, and buffer the
    // clipped members in a per-(level, wave) store whose RAM footprint is
    // bounded by the same auto RAM-vs-spill policy as the converter's pass-2
    // sinks. Duplicating bands are self-contained — the per-level wave read
    // already touches each row group for exactly one level — so the legacy
    // path is kept there (and as the tests' byte-identity oracle).
    let single_read = matches!(reader.mode(), Mode::Partitioning) && !force_legacy_pass2;
    let mut store = if single_read {
        let buffered_rows: usize = scans.iter().map(|s| s.feature_count).sum();
        let backing = backing_override.unwrap_or_else(|| {
            let b = auto_backing(
                Mode::Partitioning,
                buffered_rows,
                available_ram,
                mean_member_bytes,
            );
            log::info!(
                "[export] pass2 single-read fan-out (#235): ~{buffered_rows} buffered \
                 member row(s) across {num_levels} level(s) → {b:?}"
            );
            b
        });
        Some(fill_member_store(&reader, crs, &plans, options, backing)?)
    } else {
        None
    };

    for (level_idx, plan) in plans.iter().enumerate() {
        let scan = &scans[level_idx];
        let zoom = plan.zoom;
        let partitions = &plan.partitions;
        let partition_wave = plan.wave;

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
        let total_waves = partitions.len().div_ceil(partition_wave);
        // Within-level progress (#229): a long finest level is where runs get
        // stuck, so emit a throttled wave counter. If it advances the level is
        // slow; if it freezes the level is stuck — diagnosable in minutes.
        let mut last_wave_log = Instant::now();
        for (wave_idx, wave) in partitions.chunks(partition_wave).enumerate() {
            let results: Vec<Vec<EncodedTile>> = match store.as_mut() {
                Some(s) => encode_wave_from_store(s, level_idx, wave_idx, wave, zoom, options)?,
                None => process_wave(&ctx, wave)?,
            };
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

/// One level's precomputed pass-2 plan: its Web Mercator zoom, its planned
/// partitions, and its resolved wave width. Computed once before pass 2 so the
/// partitioning single-read fill (#235) and the drain loop agree exactly on
/// partition boundaries and wave chunking.
struct LevelPlan {
    zoom: u8,
    partitions: Vec<Partition>,
    wave: usize,
}

// ============================================================================
// Pass-2 single-read fan-out for partitioning mode (issue #235)
// ============================================================================
//
// In partitioning mode a level's render set is the accumulating row-group
// prefix `0..=end_k` (spec §5.1), so the legacy pass 2 — one bbox-pruned
// prefix read per wave per level — re-reads and re-decodes a coarse row group
// once per finer level per wave. The machinery below reads each band exactly
// once (the pass-2 analogue of #233's scan fix), clips every feature at every
// including level's zoom during that single pass, and buffers the clipped
// members in a per-(level, wave) [`MemberStore`] until the drain loop encodes
// them in the historical level/partition/tile order.
//
// **Byte identity.** A tile member's clipped geometry is key-window
// independent (pinned by `recursive_split_is_partition_range_invariant`), so
// clipping once over the full key range and routing by key yields the same
// members the per-wave range-restricted clips produced. Within-tile order is
// carried by `seq` — here the global row index in band order, which restricts
// to exactly the relative order the legacy pruned prefix reads yielded — and
// `encode_members` sorts by the unique `(key, seq)` before grouping, so buffer
// and spill order are irrelevant. The drain replays the identical ascending
// (level, partition, tile) write order.
//
// **Memory.** The store's RAM footprint follows the converter's pass-2 sink
// policy ([`auto_backing`]): small buffered sets stay in RAM; large ones spill
// member records to one temp file. Under spill backing the in-RAM buckets are
// bounded by [`MEMBER_STORE_RAM_BUDGET`] during the fill (largest buckets are
// flushed first, in whole segments, so spill I/O stays chunky and sequential),
// and every bucket is flushed at fill end. The drain then holds only one
// wave's members at a time — the same `O(one wave of partitions)` ceiling as
// the legacy path.

/// In-RAM ceiling (estimated bytes) for the member store's not-yet-flushed
/// buckets under spill backing. Crossing it flushes the largest buckets down
/// to half the ceiling; the fill's buffered transient is therefore bounded
/// regardless of input size, while flushes stay large enough to keep the spill
/// file's segments chunky.
const MEMBER_STORE_RAM_BUDGET: usize = 128 * 1024 * 1024;

/// Bounded-channel depth between the band reader thread and the clip/route
/// consumer of the single-read fill — the read/compute overlap and
/// backpressure knob, mirroring `overview/pipeline.rs`.
const SINGLE_READ_IN_FLIGHT: usize = 4;

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Coordinates spill as raw IEEE-754 bit patterns (`to_bits`/`from_bits`), so
/// the round-trip is bit-exact — including negative zero — and the re-encoded
/// MVT bytes cannot drift through the spill.
fn put_coord(buf: &mut Vec<u8>, c: geo::Coord<f64>) {
    put_u64(buf, c.x.to_bits());
    put_u64(buf, c.y.to_bits());
}

fn put_line_string(buf: &mut Vec<u8>, ls: &geo::LineString<f64>) {
    put_u32(buf, ls.0.len() as u32);
    for c in &ls.0 {
        put_coord(buf, *c);
    }
}

fn put_polygon(buf: &mut Vec<u8>, p: &geo::Polygon<f64>) {
    put_u32(buf, 1 + p.interiors().len() as u32);
    put_line_string(buf, p.exterior());
    for r in p.interiors() {
        put_line_string(buf, r);
    }
}

/// Serialize one geometry for the member spill (tag byte + payload). Covers
/// every [`Geometry`] variant so clip pass-throughs of any decoded input shape
/// survive the spill.
fn encode_geometry(buf: &mut Vec<u8>, g: &Geometry<f64>) {
    match g {
        Geometry::Point(p) => {
            buf.push(0);
            put_coord(buf, p.0);
        }
        Geometry::Line(l) => {
            buf.push(1);
            put_coord(buf, l.start);
            put_coord(buf, l.end);
        }
        Geometry::LineString(ls) => {
            buf.push(2);
            put_line_string(buf, ls);
        }
        Geometry::Polygon(p) => {
            buf.push(3);
            put_polygon(buf, p);
        }
        Geometry::MultiPoint(mp) => {
            buf.push(4);
            put_u32(buf, mp.0.len() as u32);
            for p in &mp.0 {
                put_coord(buf, p.0);
            }
        }
        Geometry::MultiLineString(mls) => {
            buf.push(5);
            put_u32(buf, mls.0.len() as u32);
            for ls in &mls.0 {
                put_line_string(buf, ls);
            }
        }
        Geometry::MultiPolygon(mp) => {
            buf.push(6);
            put_u32(buf, mp.0.len() as u32);
            for p in &mp.0 {
                put_polygon(buf, p);
            }
        }
        Geometry::GeometryCollection(gc) => {
            buf.push(7);
            put_u32(buf, gc.0.len() as u32);
            for g in &gc.0 {
                encode_geometry(buf, g);
            }
        }
        Geometry::Rect(r) => {
            buf.push(8);
            put_coord(buf, r.min());
            put_coord(buf, r.max());
        }
        Geometry::Triangle(t) => {
            buf.push(9);
            put_coord(buf, t.v1());
            put_coord(buf, t.v2());
            put_coord(buf, t.v3());
        }
    }
}

/// Serialize one member: key, seq, properties, geometry. Property floats spill
/// as bit patterns for the same exactness guarantee as coordinates.
fn encode_member(buf: &mut Vec<u8>, m: &Member) {
    put_u64(buf, m.key);
    put_u64(buf, m.seq);
    put_u32(buf, m.props.len() as u32);
    for (name, v) in m.props.iter() {
        put_u32(buf, name.len() as u32);
        buf.extend_from_slice(name.as_bytes());
        match v {
            PropertyValue::String(s) => {
                buf.push(0);
                put_u32(buf, s.len() as u32);
                buf.extend_from_slice(s.as_bytes());
            }
            PropertyValue::Float(f) => {
                buf.push(1);
                put_u32(buf, f.to_bits());
            }
            PropertyValue::Double(d) => {
                buf.push(2);
                put_u64(buf, d.to_bits());
            }
            PropertyValue::Int(i) => {
                buf.push(3);
                put_u64(buf, *i as u64);
            }
            PropertyValue::UInt(u) => {
                buf.push(4);
                put_u64(buf, *u);
            }
            PropertyValue::Bool(b) => {
                buf.push(5);
                buf.push(*b as u8);
            }
        }
    }
    encode_geometry(buf, &m.geom);
}

/// The "spill decode failed" error: the temp file is written and read by this
/// process only, so any mismatch is a bug (or disk corruption), never user
/// input.
fn spill_corrupt() -> ExportError {
    ExportError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "member spill decode: truncated or corrupt record",
    ))
}

/// Bounds-checked reader over one spill segment's bytes.
struct SpillCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> SpillCursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        SpillCursor { buf, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ExportError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        let Some(end) = end else {
            return Err(spill_corrupt());
        };
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, ExportError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, ExportError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, ExportError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn coord(&mut self) -> Result<geo::Coord<f64>, ExportError> {
        let x = f64::from_bits(self.u64()?);
        let y = f64::from_bits(self.u64()?);
        Ok(geo::coord! { x: x, y: y })
    }

    fn line_string(&mut self) -> Result<geo::LineString<f64>, ExportError> {
        let n = self.u32()? as usize;
        let mut coords = Vec::with_capacity(n);
        for _ in 0..n {
            coords.push(self.coord()?);
        }
        Ok(geo::LineString(coords))
    }

    fn polygon(&mut self) -> Result<geo::Polygon<f64>, ExportError> {
        let rings = self.u32()? as usize;
        if rings == 0 {
            return Err(spill_corrupt());
        }
        let exterior = self.line_string()?;
        let mut interiors = Vec::with_capacity(rings - 1);
        for _ in 1..rings {
            interiors.push(self.line_string()?);
        }
        Ok(geo::Polygon::new(exterior, interiors))
    }

    fn string(&mut self) -> Result<String, ExportError> {
        let n = self.u32()? as usize;
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| spill_corrupt())
    }
}

fn decode_geometry(cur: &mut SpillCursor<'_>) -> Result<Geometry<f64>, ExportError> {
    Ok(match cur.u8()? {
        0 => Geometry::Point(geo::Point(cur.coord()?)),
        1 => Geometry::Line(geo::Line::new(cur.coord()?, cur.coord()?)),
        2 => Geometry::LineString(cur.line_string()?),
        3 => Geometry::Polygon(cur.polygon()?),
        4 => {
            let n = cur.u32()? as usize;
            let mut pts = Vec::with_capacity(n);
            for _ in 0..n {
                pts.push(geo::Point(cur.coord()?));
            }
            Geometry::MultiPoint(geo::MultiPoint(pts))
        }
        5 => {
            let n = cur.u32()? as usize;
            let mut lines = Vec::with_capacity(n);
            for _ in 0..n {
                lines.push(cur.line_string()?);
            }
            Geometry::MultiLineString(geo::MultiLineString(lines))
        }
        6 => {
            let n = cur.u32()? as usize;
            let mut polys = Vec::with_capacity(n);
            for _ in 0..n {
                polys.push(cur.polygon()?);
            }
            Geometry::MultiPolygon(geo::MultiPolygon(polys))
        }
        7 => {
            let n = cur.u32()? as usize;
            let mut geoms = Vec::with_capacity(n);
            for _ in 0..n {
                geoms.push(decode_geometry(cur)?);
            }
            Geometry::GeometryCollection(geo::GeometryCollection(geoms))
        }
        8 => Geometry::Rect(geo::Rect::new(cur.coord()?, cur.coord()?)),
        9 => Geometry::Triangle(geo::Triangle::new(cur.coord()?, cur.coord()?, cur.coord()?)),
        _ => return Err(spill_corrupt()),
    })
}

fn decode_member(cur: &mut SpillCursor<'_>) -> Result<Member, ExportError> {
    let key = cur.u64()?;
    let seq = cur.u64()?;
    let n_props = cur.u32()? as usize;
    let mut props = Vec::with_capacity(n_props);
    for _ in 0..n_props {
        let name = cur.string()?;
        let value = match cur.u8()? {
            0 => PropertyValue::String(cur.string()?),
            1 => PropertyValue::Float(f32::from_bits(cur.u32()?)),
            2 => PropertyValue::Double(f64::from_bits(cur.u64()?)),
            3 => PropertyValue::Int(cur.u64()? as i64),
            4 => PropertyValue::UInt(cur.u64()?),
            5 => PropertyValue::Bool(cur.u8()? != 0),
            _ => return Err(spill_corrupt()),
        };
        props.push((name, value));
    }
    let geom = decode_geometry(cur)?;
    Ok(Member {
        key,
        seq,
        geom,
        props: Arc::new(props),
    })
}

/// Rough in-RAM cost of one buffered member, for the fill-phase budget only
/// (never output-bound): struct + `Vec` headers, 16 B per coordinate, and the
/// property payload (spilled per member, so counted per member).
fn member_bytes_estimate(m: &Member) -> usize {
    48 + m.geom.coords_count() * 16
        + m.props
            .iter()
            .map(|(n, v)| {
                24 + n.len()
                    + match v {
                        PropertyValue::String(s) => s.len(),
                        _ => 8,
                    }
            })
            .sum::<usize>()
}

/// The single spill file behind a [`MemberStore`]: bucket flushes append
/// length-indexed segments; `segments[level][wave]` records each segment's
/// `(offset, len)` in append order.
struct MemberSpill {
    writer: BufWriter<File>,
    read: File,
    /// Keeps the anonymous temp file alive (unlinked on drop).
    _temp: NamedTempFile,
    offset: u64,
    segments: Vec<Vec<Vec<(u64, u64)>>>,
}

/// Per-(level, wave) buffered tile members for the partitioning single-read
/// pass 2 (#235), with RAM or spill backing. See the section comment above for
/// the ordering and memory arguments.
struct MemberStore {
    /// `[level][wave]` in-RAM member buckets.
    buckets: Vec<Vec<Vec<Member>>>,
    /// Estimated bytes held by each in-RAM bucket.
    bucket_bytes: Vec<Vec<usize>>,
    /// Total estimated bytes across all in-RAM buckets.
    buffered_bytes: usize,
    /// Present under spill backing only.
    spill: Option<MemberSpill>,
}

impl MemberStore {
    fn new(plans: &[LevelPlan], backing: SinkBacking) -> Result<Self, ExportError> {
        let shape: Vec<usize> = plans
            .iter()
            .map(|p| p.partitions.len().div_ceil(p.wave.max(1)))
            .collect();
        let buckets = shape
            .iter()
            .map(|&w| (0..w).map(|_| Vec::new()).collect())
            .collect();
        let bucket_bytes = shape.iter().map(|&w| vec![0usize; w]).collect();
        let spill = match backing {
            SinkBacking::Ram => None,
            SinkBacking::Spill => {
                let temp = NamedTempFile::new()?;
                let writer = BufWriter::new(temp.reopen()?);
                let read = temp.reopen()?;
                Some(MemberSpill {
                    writer,
                    read,
                    _temp: temp,
                    offset: 0,
                    segments: shape
                        .iter()
                        .map(|&w| (0..w).map(|_| Vec::new()).collect())
                        .collect(),
                })
            }
        };
        Ok(MemberStore {
            buckets,
            bucket_bytes,
            buffered_bytes: 0,
            spill,
        })
    }

    fn push(&mut self, level: usize, wave: usize, m: Member) -> Result<(), ExportError> {
        let est = member_bytes_estimate(&m);
        self.buckets[level][wave].push(m);
        self.bucket_bytes[level][wave] += est;
        self.buffered_bytes += est;
        if self.spill.is_some() && self.buffered_bytes > MEMBER_STORE_RAM_BUDGET {
            self.flush_largest_until(MEMBER_STORE_RAM_BUDGET / 2)?;
        }
        Ok(())
    }

    /// Flush the largest in-RAM buckets (whole segments — chunky, sequential
    /// spill writes) until the buffered estimate drops to `floor`.
    fn flush_largest_until(&mut self, floor: usize) -> Result<(), ExportError> {
        while self.buffered_bytes > floor {
            let mut best = (0usize, 0usize, 0usize);
            for (li, level) in self.bucket_bytes.iter().enumerate() {
                for (wi, &b) in level.iter().enumerate() {
                    if b > best.2 {
                        best = (li, wi, b);
                    }
                }
            }
            if best.2 == 0 {
                break;
            }
            self.flush_bucket(best.0, best.1)?;
        }
        Ok(())
    }

    /// Serialize one bucket's members as a single appended spill segment and
    /// clear the bucket.
    fn flush_bucket(&mut self, level: usize, wave: usize) -> Result<(), ExportError> {
        use std::io::Write;
        let members = std::mem::take(&mut self.buckets[level][wave]);
        let bytes = std::mem::take(&mut self.bucket_bytes[level][wave]);
        self.buffered_bytes -= bytes;
        if members.is_empty() {
            return Ok(());
        }
        let spill = self
            .spill
            .as_mut()
            .expect("flush_bucket requires spill backing");
        let mut buf = Vec::with_capacity(bytes + 16);
        put_u64(&mut buf, members.len() as u64);
        for m in &members {
            encode_member(&mut buf, m);
        }
        spill.writer.write_all(&buf)?;
        spill.segments[level][wave].push((spill.offset, buf.len() as u64));
        spill.offset += buf.len() as u64;
        Ok(())
    }

    /// Finish the fill phase. Under spill backing every remaining bucket is
    /// flushed (the store was chosen *because* the buffered set is large; the
    /// tail is negligible and this keeps the drain's RAM at one wave) and the
    /// writer is flushed so `take_wave` reads observe every segment.
    fn finish_fill(&mut self) -> Result<(), ExportError> {
        use std::io::Write;
        if self.spill.is_some() {
            for li in 0..self.buckets.len() {
                for wi in 0..self.buckets[li].len() {
                    self.flush_bucket(li, wi)?;
                }
            }
            self.spill
                .as_mut()
                .expect("spill checked above")
                .writer
                .flush()?;
        }
        Ok(())
    }

    /// Take one wave's members out of the store: spill segments (in append
    /// order) followed by any in-RAM remainder. Member order is irrelevant to
    /// the output — `encode_members` sorts by the unique `(key, seq)`.
    fn take_wave(&mut self, level: usize, wave: usize) -> Result<Vec<Member>, ExportError> {
        use std::io::{Read, Seek, SeekFrom};
        let mut out = Vec::new();
        if let Some(spill) = self.spill.as_mut() {
            for &(off, len) in &spill.segments[level][wave] {
                spill.read.seek(SeekFrom::Start(off))?;
                let mut buf = vec![0u8; len as usize];
                spill.read.read_exact(&mut buf)?;
                let mut cur = SpillCursor::new(&buf);
                let n = cur.u64()? as usize;
                out.reserve(n);
                for _ in 0..n {
                    out.push(decode_member(&mut cur)?);
                }
                if !cur.is_empty() {
                    return Err(spill_corrupt());
                }
            }
            spill.segments[level][wave] = Vec::new();
        }
        let bytes = std::mem::take(&mut self.bucket_bytes[level][wave]);
        self.buffered_bytes -= bytes;
        out.append(&mut self.buckets[level][wave]);
        Ok(out)
    }
}

/// The single-read fill (#235): a dedicated reader thread streams every band
/// once, in band order, over a bounded channel (mirroring
/// `overview/pipeline.rs`); the consumer decodes each batch once and fans its
/// clipped members to every level whose render set includes the band — in
/// partitioning mode, band `j` feeds levels `{j..N}`.
fn fill_member_store(
    reader: &OverviewReader,
    crs: Crs,
    plans: &[LevelPlan],
    opts: &ExportOptions,
    backing: SinkBacking,
) -> Result<MemberStore, ExportError> {
    let num_levels = plans.len();
    let mut store = MemberStore::new(plans, backing)?;
    let t_fill = Instant::now();
    let mut seq = 0u64;
    let (tx, rx) = bounded::<(usize, RecordBatch)>(SINGLE_READ_IN_FLIGHT);
    let store_ref = &mut store;
    let seq_ref = &mut seq;
    std::thread::scope(|scope| -> Result<(), ExportError> {
        let reader_handle = scope.spawn(move || -> Result<(), ExportError> {
            for band in 0..num_levels {
                let band_reader = reader.read_band_with_batch_size(band, EXPORT_BATCH_SIZE)?;
                for batch in band_reader {
                    if tx.send((band, batch?)).is_err() {
                        return Ok(()); // consumer dropped the receiver (error path)
                    }
                }
            }
            Ok(())
        });

        // Consumer: batches arrive in band order (bands are contiguous
        // ascending row groups), so the running `seq` is the global file row
        // index — the band-order sequence every level's within-tile ordering
        // derives from.
        let consume: Result<(), ExportError> = (|| {
            for (band, batch) in rx.iter() {
                fanout_batch_members(&batch, band, crs, plans, opts, seq_ref, store_ref)?;
            }
            Ok(())
        })();
        drop(rx);

        match reader_handle.join() {
            Ok(read_result) => read_result?,
            Err(payload) => std::panic::resume_unwind(payload),
        }
        consume
    })?;
    store.finish_fill()?;
    log::info!(
        "[export] pass2 fill (#235): {seq} row(s) read once and fanned across \
         {num_levels} level(s) in {:.2}s",
        t_fill.elapsed().as_secs_f64(),
    );
    Ok(store)
}

/// Decode one band batch and fan its members out: clip every feature (in
/// parallel) at each including level's zoom over the **full** key range, then
/// route each member to its `(level, partition wave)` bucket. The legacy
/// analogue is [`collect_wave_members`]; see the section comment for why the
/// results are byte-identical.
fn fanout_batch_members(
    batch: &RecordBatch,
    band: usize,
    crs: Crs,
    plans: &[LevelPlan],
    opts: &ExportOptions,
    seq: &mut u64,
    store: &mut MemberStore,
) -> Result<(), ExportError> {
    let schema = batch.schema();
    let geom_idx = geometry_index(&schema).ok_or(ExportError::NoGeometryColumn)?;
    let geom_field = schema.field(geom_idx).clone();
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(batch.num_rows());
    extract_geometries_from_array(garr.as_ref(), &mut geoms)?;
    if matches!(crs, Crs::Epsg3857) {
        geoms = geoms.par_iter().map(reproject_3857_to_4326).collect();
    }

    // Band `j` belongs to every level `k >= j`'s render set (partitioning
    // prefix). A level with no planned partitions sized no tiles, so no
    // feature in its render set can emit members — skip it (routing into an
    // empty plan would be unreachable anyway).
    let targets: Vec<usize> = (band..plans.len())
        .filter(|&k| !plans[k].partitions.is_empty())
        .collect();
    // `[target][row]` -> the row's `(key, clipped geometry)` members at that
    // target level.
    type RowMembers = Vec<Vec<(u64, Geometry<f64>)>>;
    let mut per_level: Vec<RowMembers> = targets
        .iter()
        .map(|&k| {
            geoms
                .par_iter()
                .map(|g| feature_tile_members(g, plans[k].zoom, opts, 0, u64::MAX))
                .collect()
        })
        .collect();
    drop(geoms);

    if per_level.iter().all(|rows| rows.iter().all(Vec::is_empty)) {
        *seq += batch.num_rows() as u64;
        return Ok(());
    }

    // Extract property columns once per batch; materialize per-feature props
    // only for rows that produced members at any level, shared across the
    // row's members via `Arc` (as the legacy path does per wave).
    let prop_cols = property_columns(&schema, geom_idx);
    let mut extracted: Vec<(String, Vec<Option<PropertyValue>>)> =
        Vec::with_capacity(prop_cols.len());
    for &(idx, ref name) in &prop_cols {
        extracted.push((name.clone(), extract_property_column(batch.column(idx))));
    }
    for row in 0..batch.num_rows() {
        if per_level.iter().all(|rows| rows[row].is_empty()) {
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
        for (ti, &k) in targets.iter().enumerate() {
            let items = std::mem::take(&mut per_level[ti][row]);
            let plan = &plans[k];
            for (key, geom) in items {
                let pidx = route_partition(&plan.partitions, key);
                let wave = pidx / plan.wave.max(1);
                store.push(
                    k,
                    wave,
                    Member {
                        key,
                        seq: *seq,
                        geom,
                        props: Arc::clone(&props),
                    },
                )?;
            }
        }
        *seq += 1;
    }
    Ok(())
}

/// Pass-2 drain for one wave under the single-read path: take the wave's
/// buffered members, bucket them per partition (exactly as
/// [`collect_wave_members`] routes), and encode each partition as
/// [`process_wave`] would — same parallel section, same ascending output
/// order.
fn encode_wave_from_store(
    store: &mut MemberStore,
    level_idx: usize,
    wave_idx: usize,
    wave: &[Partition],
    zoom: u8,
    opts: &ExportOptions,
) -> Result<Vec<Vec<EncodedTile>>, ExportError> {
    let members = store.take_wave(level_idx, wave_idx)?;
    let mut buckets: Vec<Vec<Member>> = (0..wave.len()).map(|_| Vec::new()).collect();
    for m in members {
        buckets[route_partition(wave, m.key)].push(m);
    }
    buckets
        .into_par_iter()
        .map(|members| encode_members(members, zoom, opts))
        .collect::<Result<_, _>>()
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
/// independent band reads + decodes per level, a wave of `partition_wave`
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
    // Validate the feature's ring simplicity **once** here, then thread the
    // result through every clip this feature performs (issue #237, RC3). The
    // O(V²) self-intersection scan that `clip_polygon` used to run on *every*
    // tile now runs a single time per feature; a continental polygon touching
    // thousands of tiles pays it once instead of thousands of times. The clip
    // output is unchanged — see `clip::clip_geometry_simple`.
    let assume_simple = geometry_is_simple(geom);

    let direct_cost = tile_span(&ranges).saturating_mul(geom.coords_count() as u64);
    if direct_cost <= DIRECT_CLIP_BUDGET {
        feature_tile_members_direct(
            geom,
            &bbox,
            zoom,
            opts,
            key_lo,
            key_hi,
            assume_simple,
            &mut out,
        );
    } else {
        // Start the descent at the feature's covering tile (the deepest tile
        // whose bounds contain the whole feature bbox) rather than the world
        // root: every level above it is a single-child pass-through, so
        // skipping them is free and yields an identical leaf set.
        let root = covering_tile(&ranges, zoom);
        split_feature_into_tiles(
            root,
            geom,
            &bbox,
            zoom,
            opts,
            key_lo,
            key_hi,
            &ranges,
            assume_simple,
            &mut out,
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

/// Cascade depth (levels remaining below a node) above which the four child
/// subtrees are split across Rayon threads (issue #237, RC4).
///
/// The wave already runs one `par_iter` task per feature, but a single
/// continental polygon — the adm4 straggler carries ~100k vertices and covers
/// thousands of tiles — serializes an entire level on one core while the other
/// 15 sit idle once the small features finish. Forking its cascade lets those
/// idle cores steal the subtrees. Only the coarse top of a large cascade forks
/// (a node this shallow roots ≥ `4^DEPTH` leaves, so the fork amortizes); the
/// dense lower levels recurse sequentially to avoid drowning the pool in
/// microtasks. Small features take the direct path and never reach here.
const CASCADE_PARALLEL_DEPTH: u32 = 3;

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
/// `assume_simple` is the per-feature ring-simplicity hint (issue #237, RC3).
#[allow(clippy::too_many_arguments)]
fn feature_tile_members_direct(
    geom: &Geometry<f64>,
    bbox: &TileBounds,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
    assume_simple: bool,
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
        } else if let Some(clipped) = clip_geometry_simple(
            geom,
            &tb,
            buffer_deg,
            assume_simple,
            opts.simple_clip_fastpath,
        ) {
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
    assume_simple: bool,
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
        } else if let Some(clipped) = clip_geometry_simple(
            cur,
            &tb,
            buffer_deg,
            assume_simple,
            opts.simple_clip_fastpath,
        ) {
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
        fanout_children(
            children,
            node.z,
            cur,
            cur_bbox,
            zoom,
            opts,
            key_lo,
            key_hi,
            ranges,
            assume_simple,
            out,
        );
    } else if let Some(clipped) = clip_geometry_simple(
        cur,
        &tb,
        buffer_deg,
        assume_simple,
        opts.simple_clip_fastpath,
    ) {
        let cbbox = match clipped.bounding_rect() {
            Some(r) => TileBounds::new(r.min().x, r.min().y, r.max().x, r.max().y),
            None => return,
        };
        fanout_children(
            children,
            node.z,
            &clipped,
            &cbbox,
            zoom,
            opts,
            key_lo,
            key_hi,
            ranges,
            assume_simple,
            out,
        );
    }
    // else: `cur` does not intersect this node's buffered bounds — whole
    // subtree pruned.
}

/// Recurse into a node's four children with `child_geom` (already reduced to the
/// parent's buffered bounds). Near the top of a large cascade the two halves run
/// on separate Rayon threads (issue #237, RC4); deeper down — or for the common
/// small-cascade case — it stays sequential. Emission order is irrelevant:
/// `collect_wave_members` buckets members by tile key, and a feature emits at
/// most one member per key, so parallel interleaving cannot change the archive.
#[allow(clippy::too_many_arguments)]
fn fanout_children(
    children: [TileCoord; 4],
    node_z: u8,
    child_geom: &Geometry<f64>,
    child_bbox: &TileBounds,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
    ranges: &BboxTileRanges,
    assume_simple: bool,
    out: &mut Vec<(u64, Geometry<f64>)>,
) {
    if (zoom - node_z) as u32 > CASCADE_PARALLEL_DEPTH {
        let mut left = Vec::new();
        let mut right = Vec::new();
        rayon::join(
            || {
                for &child in &children[..2] {
                    split_feature_into_tiles(
                        child,
                        child_geom,
                        child_bbox,
                        zoom,
                        opts,
                        key_lo,
                        key_hi,
                        ranges,
                        assume_simple,
                        &mut left,
                    );
                }
            },
            || {
                for &child in &children[2..] {
                    split_feature_into_tiles(
                        child,
                        child_geom,
                        child_bbox,
                        zoom,
                        opts,
                        key_lo,
                        key_hi,
                        ranges,
                        assume_simple,
                        &mut right,
                    );
                }
            },
        );
        out.append(&mut left);
        out.append(&mut right);
    } else {
        for child in children {
            split_feature_into_tiles(
                child,
                child_geom,
                child_bbox,
                zoom,
                opts,
                key_lo,
                key_hi,
                ranges,
                assume_simple,
                out,
            );
        }
    }
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
    let assume_simple = geometry_is_simple(geom);
    let mut out = Vec::new();
    feature_tile_members_direct(
        geom,
        &bbox,
        zoom,
        opts,
        key_lo,
        key_hi,
        assume_simple,
        &mut out,
    );
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
    let assume_simple = geometry_is_simple(geom);
    let mut out = Vec::new();
    split_feature_into_tiles(
        root,
        geom,
        &bbox,
        zoom,
        opts,
        key_lo,
        key_hi,
        &ranges,
        assume_simple,
        &mut out,
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
        // `limit > 0` so `Some(0)` is a no-op off switch (the CLI/Python `0`
        // disable value never reaches here as `Some`, but guard the core API too).
        Some(limit) if limit > 0 && data.len() > limit && members.len() > 1 => {
            // Single, non-iterative drop pass. Keep a proportional count and let
            // `select_kept_members` decide *which* features survive.
            let keep_frac = limit as f64 / data.len() as f64;
            let keep = ((members.len() as f64 * keep_frac).floor() as usize).max(1);
            let kept = select_kept_members(members, keep);
            let keep = kept.len();
            let data = build_mvt(kept, tb, opts);
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

/// Choose which `keep` of `members` survive the oversized-tile drop pass.
///
/// The single-pass valve has exactly one per-feature ranking signal — geometry
/// vertex count — and that signal is degenerate for point tiles, so there are
/// two regimes:
///
/// * **Sized geometries** (lines / polygons): keep the `keep` largest by
///   coordinate count. The biggest features carry the tile's visual signal;
///   this is the pre-#280 behaviour and is byte-identical to it.
/// * **Point-dominated tiles** (every member ≤ 1 coordinate — the #259 dot-fill
///   recipe collapses dense polygons to centroid points): vertex count carries
///   no signal, so a size-ranked prefix would just keep whichever points happen
///   to sort first and clump them in one corner. Instead keep a uniform stride
///   across member order. Overview rows are Hilbert-sorted (§ writer), so a
///   tile's members arrive in space-filling-curve order; an even stride
///   therefore spreads the survivors across the tile — the same spatially
///   uniform thinning tippecanoe achieves by dropping every Nth feature in
///   sequence.
///
/// `keep` is clamped to `1..=members.len()`.
fn select_kept_members(members: &[Member], keep: usize) -> Vec<&Member> {
    let n = members.len();
    let keep = keep.clamp(1, n);
    let point_like = members.iter().all(|m| m.geom.coords_count() <= 1);
    if point_like {
        // Evenly spaced indices 0, n/keep, 2n/keep, … — strictly increasing and
        // in-bounds because keep ≤ n.
        (0..keep).map(|i| &members[i * n / keep]).collect()
    } else {
        let mut ranked: Vec<&Member> = members.iter().collect();
        ranked.sort_by_key(|m| std::cmp::Reverse(m.geom.coords_count()));
        ranked.truncate(keep);
        ranked
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
pub(super) fn webmerc_to_lnglat(x: f64, y: f64) -> (f64, f64) {
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
    ///
    /// REPIN (rename gpq-tiles → tylertoo): the archive metadata embeds
    /// `"generator":"tylertoo"` (was `"gpq-tiles"`) — the only output-bound
    /// byte change from the rename, so the hash was re-captured on the same
    /// fixture. The export logic is unchanged.
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
            "390f2b1c51a8a29c",
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

    /// The auto memory preflight (#303): pure decision function, exercised
    /// across the RAM/core grid the way #294's `auto_backing` tests are.
    #[test]
    fn auto_partition_wave_memory_preflight() {
        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * MIB;
        // Plenty of RAM: cores drive the width — including past the old fixed
        // 16 cap (the whole point of #303).
        assert_eq!(auto_partition_wave(64, Some(256 * GIB)), 64);
        assert_eq!(auto_partition_wave(24, Some(256 * GIB)), 24);
        // The #293 target box (16 cores / 54 GiB) resolves exactly as before.
        assert_eq!(auto_partition_wave(16, Some(54 * GIB)), 16);
        // Tight RAM caps below the core count:
        // 4 GiB avail → 2 GiB wave budget → 32 slots of 64 MiB.
        assert_eq!(auto_partition_wave(64, Some(4 * GIB)), 32);
        // 2 GiB avail → 1 GiB budget → 16 slots.
        assert_eq!(auto_partition_wave(64, Some(2 * GIB)), 16);
        // 1 GiB avail → 512 MiB budget → 8 slots.
        assert_eq!(auto_partition_wave(64, Some(GIB)), 8);
        // Very tight RAM floors at MIN, never below (historical width; its
        // worst-case transient is small and known-safe).
        assert_eq!(auto_partition_wave(16, Some(256 * MIB)), PARTITION_WAVE_MIN);
        assert_eq!(auto_partition_wave(16, Some(0)), PARTITION_WAVE_MIN);
        // Small boxes keep the floor regardless of RAM.
        assert_eq!(auto_partition_wave(2, Some(256 * GIB)), PARTITION_WAVE_MIN);
        // RAM unknown (non-Linux, unreadable /proc/meminfo): fall back to the
        // #293 fixed cap — portable and deterministic where no probe exists.
        assert_eq!(auto_partition_wave(64, None), PARTITION_WAVE_FALLBACK_MAX);
        assert_eq!(auto_partition_wave(8, None), 8);
        assert_eq!(auto_partition_wave(2, None), PARTITION_WAVE_MIN);
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
                bbox: TileBounds::new(0.0, 0.0, 0.0, 0.0),
                members: m,
            })
            .collect()
    }

    /// The data-aware per-level guard (#311): sizes from the *densest* planned
    /// partition, not a flat constant, and may drop below the throughput floor
    /// when memory demands it.
    #[test]
    fn memory_safe_level_wave_uses_densest_partition() {
        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * MIB;

        // Sparse level: small partitions × modest members → the estimate is far
        // under budget, so the ceiling is preserved untouched.
        let sparse = partitions_with_members(&[32_768, 30_000, 31_000]);
        assert_eq!(
            memory_safe_level_wave(16, &sparse, Some(512), Some(54 * GIB)),
            16,
            "a sparse level must not narrow below the ceiling"
        );

        // Dense level: one tile overshoots the target into a 6.5M-member
        // partition of ~600 B/member. per-slot ≈ 6.5M × 600 × 2 ≈ 7.3 GiB;
        // budget = 0.5 × 54 GiB = 27 GiB → 3 slots. This is the Brazil case that
        // OOM'd at wave 16, now capped to a safe 3.
        let dense = partitions_with_members(&[6_500_000, 100, 200]);
        assert_eq!(
            memory_safe_level_wave(16, &dense, Some(600), Some(54 * GIB)),
            3,
        );

        // Extreme density can fall *below* PARTITION_WAVE_MIN, all the way to a
        // serial wave — memory safety outranks the historical floor.
        // Result 1 is deliberately below PARTITION_WAVE_MIN (6): the memory
        // guard bypasses the throughput floor rather than OOM.
        let extreme = partitions_with_members(&[60_000_000, 100]);
        assert_eq!(
            memory_safe_level_wave(16, &extreme, Some(600), Some(54 * GIB)),
            1,
        );

        // The clamp is one-directional: a roomy budget never widens past the
        // ceiling the caller already chose.
        assert_eq!(
            memory_safe_level_wave(4, &sparse, Some(512), Some(256 * GIB)),
            4,
        );

        // Fallbacks preserve prior behaviour: unknown member size (empty finest
        // level), unmeasurable RAM, or an empty partition list all return the
        // ceiling verbatim.
        assert_eq!(
            memory_safe_level_wave(16, &dense, None, Some(54 * GIB)),
            16,
            "unknown member size falls back to the ceiling"
        );
        assert_eq!(
            memory_safe_level_wave(16, &dense, Some(0), Some(54 * GIB)),
            16,
            "a degenerate zero member size falls back to the ceiling"
        );
        assert_eq!(
            memory_safe_level_wave(16, &dense, Some(600), None),
            16,
            "unprobeable RAM falls back to the ceiling"
        );
        assert_eq!(
            memory_safe_level_wave(16, &[], Some(600), Some(54 * GIB)),
            16,
            "an empty level falls back to the ceiling"
        );
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
            auto_partition_wave(cores, available_memory_bytes()),
            "auto must equal the preflight decision on this box's cores + RAM"
        );
        // Explicit values pass through verbatim, including above any auto cap.
        assert_eq!(resolve_partition_wave(1), 1);
        assert_eq!(resolve_partition_wave(6), 6);
        assert_eq!(
            resolve_partition_wave(PARTITION_WAVE_FALLBACK_MAX + 100),
            PARTITION_WAVE_FALLBACK_MAX + 100
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
        // Pin the fast path OFF: this oracle asserts the recursive cascade and
        // the direct clip produce byte-identical MVT, which is a property of the
        // splitter (#226) and holds only under the deterministic i_overlay clip.
        // With `simple_clip_fastpath` on (the default), Sutherland–Hodgman
        // rotates a clipped simple ring to a different start vertex, and the
        // rotation differs between the recursive-halving and direct-clip paths —
        // render-equivalent but not byte-identical, so the two paths legitimately
        // diverge in bytes here. That render-equivalence is covered separately by
        // the `clip.rs` `fastpath_*_render_equivalent` tests.
        let opts = ExportOptions {
            simple_clip_fastpath: false,
            ..Default::default()
        };
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

        // No limit (explicit `None`, since the default is now 500 KiB): no
        // oversized tiles.
        let no_limit = ExportOptions {
            tile_size_limit: None,
            ..Default::default()
        };
        let none = encode_level_tiles(&feats, 6, &no_limit);
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

    /// #280: the per-tile cap is on by default at 500 KiB (== what `500K`
    /// parses to; tippecanoe parity), not disabled.
    #[test]
    fn default_export_caps_tiles_at_500_kib() {
        assert_eq!(DEFAULT_TILE_SIZE_LIMIT, 500 * 1024);
        assert_eq!(
            ExportOptions::default().tile_size_limit,
            Some(DEFAULT_TILE_SIZE_LIMIT)
        );
    }

    /// Build a bare point [`Member`] at `(x, y)` for the drop-valve unit tests.
    fn point_member(x: f64, y: f64, seq: u64) -> Member {
        Member {
            key: 0,
            seq,
            geom: Geometry::Point(Point::new(x, y)),
            props: Arc::new(Vec::new()),
        }
    }

    /// #280 spatial fairness: on a point-dominated tile, vertex count carries no
    /// ranking signal, so the valve must keep a uniform stride across member
    /// (≈ Hilbert) order — not a first-N prefix that would clump survivors.
    #[test]
    fn drop_valve_point_tile_keeps_uniform_stride() {
        let members: Vec<Member> = (0..10).map(|i| point_member(i as f64, 0.0, i)).collect();
        let kept = select_kept_members(&members, 5);
        let xs: Vec<f64> = kept
            .iter()
            .map(|m| match &m.geom {
                Geometry::Point(p) => p.x(),
                _ => unreachable!("built points"),
            })
            .collect();
        // Even stride 0,2,4,6,8 — spread across the tile — not the prefix 0..5.
        assert_eq!(xs, vec![0.0, 2.0, 4.0, 6.0, 8.0]);
    }

    /// Sized geometries keep the largest-by-vertex-count survivors (pre-#280
    /// behaviour, unchanged): the biggest features carry the tile's visual
    /// signal.
    #[test]
    fn drop_valve_sized_tile_keeps_largest() {
        let members: Vec<Member> = (1..=5)
            .map(|n| {
                let coords: Vec<(f64, f64)> = (0..n).map(|k| (k as f64, 0.0)).collect();
                Member {
                    key: 0,
                    seq: n as u64,
                    geom: Geometry::LineString(LineString::from(coords)),
                    props: Arc::new(Vec::new()),
                }
            })
            .collect();
        let kept = select_kept_members(&members, 2);
        let counts: Vec<usize> = kept.iter().map(|m| m.geom.coords_count()).collect();
        assert_eq!(counts, vec![5, 4]);
    }

    /// `Some(0)` is the core off switch (CLI `--max-tile-size 0` / Python
    /// `tile_size_limit=0` map to it): the valve must never fire, no matter how
    /// dense the tile.
    #[test]
    fn size_limit_zero_disables_valve() {
        let members: Vec<Member> = (0..64)
            .map(|i| point_member(-100.0 + i as f64 * 0.001, 40.0, i))
            .collect();
        let tb = TileCoord::new(0, 0, 0).bounds();
        let opts = ExportOptions {
            tile_size_limit: Some(0),
            ..Default::default()
        };
        let (_data, count, oversized) = encode_tile(&members, &tb, &opts);
        assert!(!oversized, "Some(0) must disable the cap");
        assert_eq!(count, members.len());
    }

    // --- #235: partitioning single-read fan-out pass 2 -----------------------

    /// Round-trip exactness of the member spill codec: key, seq, every
    /// [`PropertyValue`] variant, and every [`Geometry`] variant, verified
    /// bit-exactly (via re-encoded bytes, since `PartialEq` would call
    /// `-0.0 == 0.0` equal and hide a sign flip through the spill).
    #[test]
    fn member_spill_codec_roundtrip_is_exact() {
        use geo::{
            coord, GeometryCollection, Line, MultiLineString, MultiPoint, MultiPolygon, Rect,
            Triangle,
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
        let props: Vec<(String, PropertyValue)> = vec![
            ("s".to_string(), PropertyValue::String("héllo".to_string())),
            ("f".to_string(), PropertyValue::Float(-0.0f32)),
            ("d".to_string(), PropertyValue::Double(f64::MIN_POSITIVE)),
            ("i".to_string(), PropertyValue::Int(-42)),
            ("u".to_string(), PropertyValue::UInt(u64::MAX)),
            ("b".to_string(), PropertyValue::Bool(true)),
        ];
        for (i, g) in geoms.into_iter().enumerate() {
            let m = Member {
                key: tile_key(7, 11) + i as u64,
                seq: u64::MAX - i as u64,
                geom: g,
                props: Arc::new(props.clone()),
            };
            let mut buf = Vec::new();
            encode_member(&mut buf, &m);
            let mut cur = SpillCursor::new(&buf);
            let back = decode_member(&mut cur).unwrap();
            assert!(cur.is_empty(), "trailing bytes after decode (geometry {i})");
            assert_eq!(back.key, m.key);
            assert_eq!(back.seq, m.seq);
            assert_eq!(*back.props, *m.props);
            let mut buf2 = Vec::new();
            encode_member(&mut buf2, &back);
            assert_eq!(buf, buf2, "re-encoded member bytes differ (geometry {i})");
        }
    }

    /// The member store's spill path: bucket flushes append segments to one
    /// temp file; `take_wave` reads a wave's segments back in append order
    /// (plus any RAM remainder), returning exactly the members pushed to it.
    #[test]
    fn member_store_spill_segments_roundtrip() {
        // One level, two single-partition waves (wave width 1).
        let plans = vec![LevelPlan {
            zoom: 4,
            partitions: partitions_with_members(&[10, 10]),
            wave: 1,
        }];
        let mut store = MemberStore::new(&plans, SinkBacking::Spill).unwrap();
        let mk = |key: u64, seq: u64| Member {
            key,
            seq,
            geom: Geometry::Point(Point::new(seq as f64, -1.0)),
            props: Arc::new(vec![("id".to_string(), PropertyValue::Int(seq as i64))]),
        };
        store.push(0, 0, mk(0, 0)).unwrap();
        store.push(0, 1, mk(1, 1)).unwrap();
        // Force a mid-fill flush so wave 0 spans two spill segments.
        store.flush_bucket(0, 0).unwrap();
        store.push(0, 0, mk(0, 2)).unwrap();
        store.finish_fill().unwrap();

        let w0 = store.take_wave(0, 0).unwrap();
        let w1 = store.take_wave(0, 1).unwrap();
        assert_eq!(w0.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(w1.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![1]);
        assert!(w0.iter().all(|m| m.key == 0));
        assert!(w1.iter().all(|m| m.key == 1));
        assert_eq!(w0[0].props[0].1, PropertyValue::Int(0));
    }

    /// Rich 3-level **partitioning** fixture for the #235 byte-equality tests:
    /// per-band mixes of points, seam-crossing lines, and multi-tile polygons
    /// with tiny row groups so bands span several row groups and coarse
    /// features genuinely fan into finer levels' render sets.
    fn partitioning_equivalence_fixture(path: &Path) {
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
        let mut coords = Vec::new();
        for k in 0..200 {
            coords.push((30.0 + k as f64 * 0.3, -20.0 + (k as f64 * 0.1).sin()));
        }
        let wiggly = Geometry::LineString(LineString::from(coords));
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
                export_pmtiles_impl(tin.path(), t_legacy.path(), &opts, target, true, None)
                    .unwrap();
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
        let r_many =
            export_pmtiles_with_partition_target(tin.path(), t_many.path(), &opts, 1).unwrap();
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
}
