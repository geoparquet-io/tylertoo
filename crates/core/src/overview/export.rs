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
//!    PMTiles tile-id (Hilbert) ranges of roughly `DEFAULT_PARTITION_TARGET`
//!    members each, then process them in **waves** of
//!    [`resolve_partition_wave`] partitions. Each wave reads the band **once**
//!    (row groups pruned to the wave's combined bbox), splits every feature
//!    into its tiles with a **top-down recursive quadtree cascade** (see
//!    `feature_tile_members`) — each feature clipped once per pyramid level
//!    into an already-reduced child region down to the target zoom, so a
//!    vertex takes part in `O(depth)` clips rather than `O(tiles_spanned)`
//!    (issue #226) — and *routes* each resulting member to its owning
//!    partition (see `process_wave`). Sharing one read+decode across a wave
//!    replaces the old per-partition re-read (issue #228): a level now costs
//!    `ceil(P / PARTITION_WAVE)` band reads, not `P`. The per-tile clips reuse
//!    the [`clip_geometry_simple`] entry point, MVT-encode via `crate::mvt`,
//!    and each finished partition's tiles stream immediately to
//!    [`StreamingPmtilesWriter`]. Tiles are written in ascending PMTiles
//!    tile-id order per zoom, and levels export in ascending zoom, so the
//!    whole archive is added in globally ascending tile-id order — the
//!    archive is genuinely `clustered: true` (#506; before that it was
//!    row-major `(x, y)` order, an arbitrary historical layout with no such
//!    guarantee).
//!
//!    In **partitioning** mode the per-level wave read walks the accumulating
//!    row-group prefix (§5.1), so a coarse row group would be re-read and
//!    re-decoded by every finer level once per wave. Pass 2 therefore switches
//!    to a **single-read fan-out** there (issue #235, the pass-2 analogue of
//!    the #233 scan fix): one reader pass streams every band once, clips each
//!    feature at every including level's zoom, and buffers the members in a
//!    RAM/spill-backed `MemberStore` that the same wave loop drains — see
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
//! *unbounded* by it. On `auto` each wave therefore holds only the members the
//! budget affords (`memory_safe_wave_members`, #311): `cut_waves` narrows the
//! waves that hold dense partitions (down to serial if need be) and leaves the
//! rest at the ceiling (#876), so a dense finest zoom stays within RAM instead
//! of OOM-killing the run.
//! The scan pass keeps only per-tile member counts (`O(#tiles)`), and the
//! PMTiles writer streams tile bytes to a temp file, keeping only directory
//! entries. There is **no** per-zoom or global accumulation.
//!
//! The partitioning single-read path (#235) buffers every level's clipped
//! members between its fill and drain phases, but its **RAM** ceiling is
//! unchanged: the auto RAM-vs-spill policy shared with the converter's pass-2
//! sinks (`auto_backing`) keeps small buffered sets in RAM and spills large
//! ones to a temp file, the fill's in-RAM buckets are capped at
//! `MEMBER_STORE_RAM_BUDGET`, and the drain holds one wave of partitions at
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

use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

#[cfg(test)]
use crate::mvt::PropertyValue;
#[cfg(test)]
use arrow_array::types::{
    Decimal128Type, Float32Type, Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type, UInt32Type,
    UInt64Type, UInt8Type,
};
use arrow_array::{cast::AsArray, types::Float64Type, Array, RecordBatch};
#[cfg(test)]
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::{DataType, Schema};
use crossbeam_channel::{Receiver, Sender};
use geo::{BoundingRect, CoordsIter, Geometry, MapCoords};
use geoarrow::array::from_arrow_array;
use geoarrow_array::GeoArrowArray;
#[cfg(test)]
use prost::Message;
use rayon::prelude::*;
use serde::Serialize;
use tempfile::NamedTempFile;

use super::{
    coalesce::COALESCED_COUNT_COLUMN,
    config::ConfigError,
    convert::{
        human_bytes, resolve_pass_read_workers, skip_memory_preflight_from_env, ConvertError,
        READ_WORKERS_AUTO, SKIP_MEMORY_PREFLIGHT_ENV,
    },
    junction::{self, JunctionNodes},
    level::{zoom_for_gsd, Crs, MemoryProfile, Mode, OverviewsMeta},
    memory::{Grant, MemoryBudget, MemoryPhase, Price, PriceBasis},
    pipe::scoped_pipe,
    pipeline::{
        available_memory_bytes, probe_oom_memory_limit, sink_backing, MemoryLimit,
        MemoryLimitSource, SinkBacking,
    },
    properties::PropertySelection,
    read::{footer_price, read_in_order_capped, ReadFlow, ReadPass, ReadTuning},
    reader::{OverviewReader, ReaderError},
    stream::{
        add_nanos, preflight_profile_json_path, profile_json_target, sample_rss_peak,
        write_export_profile_json, ExportPhaseWalls, ExportProfileJsonInputs, ExportTimers,
        ExportZoomProfile, RssSampler, RssSamplerReport,
    },
    writer::LEVEL_COLUMN,
};
#[cfg(test)]
use crate::compression;
#[cfg(test)]
use crate::mvt::{LayerBuilder, TileBuilder};
use crate::{
    batch_processor::extract_geometries_from_array,
    clip::{clip_geometry_simple, geometry_is_simple},
    compression::{Compression, TileGzipCache},
    dedup::TileHasher,
    input::InputError,
    input_set::{ConvertSource, RowGroupSelection},
    mvt::{EncodedFeatureGeometry, PropertyRef},
    pmtiles_writer::StreamingPmtilesWriter,
    tile::{tile_ranges_for_bbox, BboxTileRanges, TileBounds, TileCoord, MAX_ZOOM},
    vector_tile::tile::GeomType,
    TilesetMetadata,
};

mod coalesce_valve;
mod tile_data;
use tile_data::TileData;

mod collect;
mod coverage_fill;
mod driver;
mod plan;
mod scan;
mod store;
pub(crate) mod variable_depth;
pub(in crate::overview) use self::scan::geometry_lnglat_bbox;
pub(crate) use self::scan::lnglat_bbox;
use self::{collect::*, driver::*, plan::*, scan::*, store::*};
pub use self::{
    plan::resolve_partition_wave, plan::zoom_for_level, plan::PARTITION_WAVE_AUTO,
    plan::PARTITION_WAVE_MIN,
};
use coalesce_valve::RowGapCursor;
struct PartitionBudget {
    partition_target: usize,
    ceiling_wave: usize,
    auto_wave: bool,
    member_bytes: Option<u64>,
    /// The historical member price for the hard refusal, without valve workspace.
    fit_member_bytes: Option<u64>,
    /// The run's profile: the per-level #311 guard sizes under it.
    profile: MemoryProfile,
    budget: MemoryBudget,
    fit_limit: Option<MemoryLimit>,
}

#[derive(Clone, Copy)]
struct TileTarget<'a> {
    zoom: u8,
    opts: &'a ExportOptions,
    key_lo: u64,
    key_hi: u64,
    assume_simple: bool,
    diagnostics: &'a crate::diagnostics::Diagnostics,
    /// The geometry the clip started from: a piece that is still it is
    /// [`TilePiece::Whole`].
    source: &'a Geometry<f64>,
}

#[cfg(test)]
use coalesce_valve::GAP_NONE;
mod feature_id;
use feature_id::{
    feature_id_values, resolve_feature_id, validate_feature_id_column, ResolvedFeatureId,
};
pub use feature_id::{FeatureIdColumnProblem, InvalidFeatureIdReason};

/// Default MVT tile extent (matches [`crate::mvt::DEFAULT_EXTENT`]).
pub(crate) const DEFAULT_EXTENT: u32 = 4096;

/// Default per-tile edge buffer, in tile pixels: tippecanoe's `-b 5`
/// (`main.cpp:3193`). Was 8 until the 5 px buffer was measured 1.7-4.8%
/// smaller with nothing inside the tile changed (`corpus/SWEEPS.md`).
pub(crate) const DEFAULT_TILE_BUFFER_PX: u32 = 5;

/// The finest-zoom simplification factor lines get when
/// [`ExportOptions::max_zoom_simplify_factor`] is unset: a quarter GSD, one
/// tile unit at extent 4096, tippecanoe's `-S 1`.
pub(crate) const DEFAULT_LINE_MAX_ZOOM_SIMPLIFY_FACTOR: f64 = 0.25;

/// The widest `--tile-buffer` any export accepts, in tile pixels (#433).
///
/// One full tile width (`NOMINAL_TILE_PIXELS`): at the cap a tile carries
/// every feature of its eight neighbours, which is already more than any
/// renderer reads across a seam. Past it a tile starts duplicating geometry
/// from tiles it does not even border, and the cost is not just bytes —
/// membership widening is what makes a feature belong to `O(buffer²)` tiles,
/// so `--tile-buffer 100000` (buffer ≈ 390 tile widths) put every feature in
/// every tile and the export went `O(features × tiles)`. tippecanoe's
/// `--buffer` has no cap; this is a divergence, and a deliberate one, since
/// there is no output past this value that is not also reachable below it.
///
/// This is the one bound. The sharded path's read-pruning margin
/// (`shard::SHARD_READ_MARGIN_TILES`, two pivot tiles = 512 px) is
/// wider than this, so the cap satisfies it with 2x headroom; the
/// compile-time check next to that constant keeps it so, and
/// [`ExportError::TileBufferTooWideForShard`] is no longer raised.
pub const MAX_TILE_BUFFER_PX: u32 = 256;

/// Nominal tile size in pixels — the denominator that makes `tile_buffer` mean
/// "tile pixels", the unit tippecanoe's `--buffer` uses and the one this crate
/// documents.
///
/// Not [`ExportOptions::extent`]: extent is the MVT coordinate resolution
/// (4096), a different quantity that happens to also divide the tile. Dividing
/// by it made the buffer `extent/256` = 16x narrower than the number said, so
/// the default 8 "pixels" was half a pixel and seam continuity was effectively
/// off. A raised extent silently narrowed the buffer further, which is the
/// clearest sign it was the wrong denominator.
pub(crate) const NOMINAL_TILE_PIXELS: f64 = 256.0;

/// The tile buffer as a fraction of one tile's width, from `tile_buffer` in
/// tile pixels.
#[inline]
fn buffer_fraction(opts: &ExportOptions) -> f64 {
    f64::from(opts.tile_buffer) / NOMINAL_TILE_PIXELS
}

/// Default per-tile cap on the raw (uncompressed) MVT, in bytes: 2 MiB.
///
/// A backstop behind [`DEFAULT_COMPRESSED_TILE_SIZE_LIMIT`], which does the
/// work: clients decode and render the raw tile, and full-resolution
/// polygons can compress 4:1, so the compressed cap alone would admit tiles
/// of 2 MB and more. Up to #761 this was the only cap, at 500 KiB, which was
/// about twice as strict as tippecanoe's. A tile over either cap trips the
/// size valve (see [`ExportOptions::tile_size_limit`] and
/// [`ExportOptions::size_valve`]). Set `tile_size_limit` to `None` (CLI:
/// `--max-tile-size 0`; Python: `tile_size_limit=0`) to disable it.
pub const DEFAULT_TILE_SIZE_LIMIT: usize = 2048 * 1024;

/// Default per-tile cap on the gzip-compressed tile, in bytes: 500 KiB.
///
/// tippecanoe's `-M` default (#761). It is what `500K` parses to, slightly
/// above tippecanoe's 500,000. Set `compressed_tile_size_limit` to `None`
/// (CLI: `--max-compressed-tile-size 0`; Python:
/// `max_compressed_tile_size=0`) to disable it.
pub const DEFAULT_COMPRESSED_TILE_SIZE_LIMIT: usize = 500 * 1024;

/// How the export brings a tile that is over its size limit back under it
/// (#745).
///
/// [`SizeValve::Merge`] follows tippecanoe's `--coalesce-densest-as-needed`:
/// the densest lines and polygons are merged into the feature kept just
/// before them. Merging keeps the coverage's area, and the tile is measured
/// again. If still over after all merges, the fallback can lose precision
/// and features. Merged features are never split into their sources. In a
/// tile of lines or polygons, only a lone feature still over the cap at the
/// extent floor is written over it, with a warning. As in tippecanoe, a
/// merged feature
/// keeps the attributes of the feature it was merged into; the absorbed
/// features' attributes are not carried. Points are never merged; a tile of
/// points always takes the stride of [`SizeValve::Drop`]. See
/// `overview::export::coalesce_valve`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SizeValve {
    /// Merge the densest lines and polygons, re-measure, and repeat until
    /// the tile fits (the default). If merging exhausts its gaps, drop singles,
    /// halve the extent to 512, then drop whole features if needed. In a
    /// tile of lines or polygons, only a lone feature still over the cap at
    /// that floor is written over it, with a warning. Merged features are
    /// never split into their sources.
    #[default]
    Merge,
    /// Drop features until the tile fits: the largest sized geometries
    /// survive, or a uniform stride of points. Every feature that remains
    /// keeps exactly its own geometry and attributes; dropped ones leave
    /// holes. The valve before #745 (which dropped once, without
    /// re-measuring).
    Drop,
}

impl SizeValve {
    /// The value as the CLI spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Merge => "merge",
            Self::Drop => "drop",
        }
    }
}

impl std::fmt::Display for SizeValve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SizeValve {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "merge" => Ok(Self::Merge),
            "drop" => Ok(Self::Drop),
            other => Err(format!(
                "unknown size valve {other:?}: expected merge or drop"
            )),
        }
    }
}

use crate::tile::WEBMERC_HALF_M;
pub use crate::tile_format::{MltProfile, TileFormat};

/// Options controlling a PMTiles export.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "cli", derive(clap::Args))]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "cli", command(group(clap::ArgGroup::new("export_only").multiple(true))))]
#[non_exhaustive]
pub struct ExportOptions {
    /// MVT layer name written into every tile and the archive metadata.
    #[cfg_attr(feature = "cli", arg(long, default_value_t = Self::default().layer_name))]
    #[cfg_attr(feature = "cli", arg(help = "MVT layer name written into every tile", long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub layer_name: String,
    /// Per-tile edge buffer in **tile pixels**. Converted to coordinate units
    /// per tile (`buffer_deg = tile_width * buffer_px / 256`) and applied as
    /// the clip margin, so features spanning a seam render continuously.
    /// At most [`MAX_TILE_BUFFER_PX`] (one tile width); wider is
    /// [`ConfigError::TileBufferTooWide`] (#433).
    #[cfg_attr(feature = "cli", arg(long, default_value_t = ExportOptions::default().tile_buffer))]
    #[cfg_attr(feature = "cli", arg(help = "Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most 256 (one tile width)", long_help = None))]
    pub tile_buffer: u32,
    /// MVT tile extent (integer tile-local resolution). Default 4096. Must be
    /// positive ([`ConfigError::InvalidExtent`] otherwise); a value that is
    /// not a power of two is accepted with a warning, since the MVT spec only
    /// recommends one (#433).
    #[cfg_attr(feature = "cli", arg(long, default_value_t = ExportOptions::default().extent))]
    #[cfg_attr(feature = "cli", arg(help = "MVT tile extent: the tile-local coordinate resolution. A power of two is recommended", long_help = None))]
    pub extent: u32,
    /// Per-tile limit on the **raw** MVT size, in bytes. When `Some(limit)`
    /// with `limit > 0`, a tile whose encoded size exceeds it goes through the
    /// size valve ([`Self::size_valve`]). `None` (or `Some(0)`) sets no raw
    /// limit; only with [`Self::compressed_tile_size_limit`] also off is no
    /// size limit enforced and `oversized_tiles` always 0.
    ///
    /// Defaults to `Some(`[`DEFAULT_TILE_SIZE_LIMIT`]`)` (2 MiB, a backstop
    /// behind the compressed limit; #761).
    #[cfg_attr(feature = "cli", arg(long = "max-tile-size", id = "tile_size_limit",
        value_name = "SIZE|auto",
        visible_alias = "tile-size-limit",
        default_value = "auto",
        value_parser = super::config::cli::parse_size_auto
    ))]
    #[cfg_attr(feature = "cli", arg(help = "Per-tile cap on the raw MVT size, such as `2M`, or 0 for no cap: a backstop behind `--max-compressed-tile-size`. `auto` is 2M, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile over a cap goes through `--size-valve`. With several layers the caps apply to the joined tile", long_help = None))]
    pub tile_size_limit: Option<usize>,
    /// Per-tile limit on the **gzip-compressed** tile size, in bytes:
    /// tippecanoe's `-M` / `--maximum-tile-bytes` (#745). Checked alongside
    /// [`Self::tile_size_limit`]; a tile must satisfy both. Defaults to
    /// `Some(`[`DEFAULT_COMPRESSED_TILE_SIZE_LIMIT`]`)` (500 KiB, #761);
    /// `None` or `Some(0)` sets no compressed limit.
    #[serde(rename = "max_compressed_tile_size")]
    #[cfg_attr(feature = "cli", arg(long = "max-compressed-tile-size", id = "max_compressed_tile_size",
        value_name = "SIZE|auto",
        default_value = "auto",
        value_parser = super::config::cli::parse_size_auto
    ))]
    #[cfg_attr(feature = "cli", arg(help = "Per-tile cap on the gzip-compressed tile size, like tippecanoe's `-M`, such as `500K`, or 0 for no cap. `auto` is 500K, or no cap with `tiles --verbatim` and for `pyramid` bands. Both caps apply, to the joined tile when there are several layers", long_help = None))]
    pub compressed_tile_size_limit: Option<usize>,
    /// How a tile over a size limit is brought back under it (#745). Default
    /// [`SizeValve::Merge`].
    #[serde(with = "super::config::spelled")]
    #[cfg_attr(feature = "cli", arg(
        long,
        value_name = "merge|drop",
        default_value_t = ExportOptions::default().size_valve
    ))]
    #[cfg_attr(feature = "cli", arg(help = "How to reduce a tile over its cap: `merge` merges dense lines and polygons, drops singles, halves the extent down to 512, then drops whole features if needed. Merges keep the target's attributes and are never split into their sources. In a tile of lines or polygons, only a lone feature still over the cap at the extent floor is written over it, with a warning. `drop` removes features and keeps each survivor's attributes. Point tiles use dropping in both modes", long_help = None))]
    pub size_valve: SizeValve,
    /// Tile encoding, MVT by default. Each finished MVT tile is converted
    /// to MLT with mlt-core before gzip and both size caps, so the caps
    /// measure MLT bytes. MLT uses PMTiles tile type 6 and metadata format
    /// `mlt`; merge refuses to combine MVT and MLT archives.
    #[serde(with = "super::config::spelled")]
    #[cfg_attr(feature = "cli", arg(
        long,
        default_value = ExportOptions::default().tile_format.as_str(),
        value_parser = super::config::cli::spelled::<TileFormat>(&["mvt", "mlt"])
    ))]
    #[cfg_attr(feature = "cli", arg(help = "Tile format: `mvt` (default) or `mlt` (MapLibre GL JS 5.12 or later, or MapLibre Native); MLT stores each attribute of a layer as one typed column, following mlt-core's MVT conversion: integers mixed with floats become f64 (doubles), other mixes become strings, and attribute order within a feature becomes column order; conversion precedes gzip and both size caps, so the caps measure MLT bytes; merge refuses mixed formats and pyramid bands must all have the given format", long_help = None))]
    pub tile_format: TileFormat,
    /// MLT encoding profile, accepted only with [`TileFormat::Mlt`].
    /// Defaults to [`MltProfile::Web`].
    #[serde(with = "super::config::spelled")]
    #[cfg_attr(feature = "cli", arg(
        long,
        default_value = ExportOptions::default().mlt_profile.as_str(),
        value_parser = super::config::cli::spelled::<MltProfile>(
            &["web", "native", "min-size", "diffable", "fast-encoding"]
        )
    ))]
    #[cfg_attr(feature = "cli", arg(help = "MLT profile, non-default profiles need `--tile-format mlt`: `web` (default) targets GL JS before 6.0 with @maplibre/mlt 1.1, omits FastPFOR, FSST and shared dictionaries, and stores integer columns wider than 32 bits as doubles when every value is exact (absolute value at most 2^53), since GL JS reads 64-bit integers as BigInt and style expressions do not compare BigInt with numbers; mlt-core profiles: `native` for MapLibre Native with every codec and stored polygon triangles, `min-size` for smallest tiles, `diffable` for small binary diffs between releases, `fast-encoding` for quickest encoding", long_help = None))]
    pub mlt_profile: MltProfile,
    /// Experimental (#750): draw, under each tile's own features below the
    /// finest zoom, the area its child tiles cover, built from the finest
    /// level before the export starts (`export/coverage_fill.rs`). Default
    /// `false`. A coarse job ([`Self::zoom_ceiling`]) builds it from the
    /// file's finest level (#771); a data shard ([`Self::tile_range`]) does
    /// not build it.
    #[cfg_attr(feature = "cli", arg(long))]
    #[cfg_attr(feature = "cli", arg(help = "Fill the holes that dense small polygons (parcels, fields, small admin units) leave at coarse zooms: under each tile's features, also draw the area its child tiles cover, with the attributes of one feature it sits under, in the room the tile's own features leave. Adds 10–17% to build time on dense coverages, about 1.5x on scattered sub-pixel fields (which it may also over-draw), and holds the finest level's geometry in memory. A sharded build's coarse job draws it; data shards (`--tile-range`) do not", long_help = None))]
    pub coverage_fill: bool,
    /// #803: end each branch of the pyramid at the coarsest zoom whose tile
    /// can carry every canonical feature under it within both size caps,
    /// tippecanoe's `--generate-variable-depth-tile-pyramid`. The archive's
    /// header max zoom is then the deepest tile written, and a server must
    /// fall back to the nearest ancestor tile for a missing one. Default
    /// `false`; see `export/variable_depth.rs` for the stop rule and the
    /// options it refuses.
    #[cfg_attr(feature = "cli", arg(long = "generate-variable-depth-tile-pyramid"))]
    #[cfg_attr(feature = "cli", arg(help = "Stop each branch of the tile pyramid at the coarsest zoom whose tile holds all of that area's finest-level features within both size caps, like tippecanoe's flag of the same name. Fewer, larger tiles; the server must serve the nearest ancestor for a missing tile. Off by default", long_help = None))]
    pub variable_depth: bool,
    /// Skip the `i_overlay` boundary-bridge fallback for features whose rings are
    /// already proven simple (issue #239). On a simple ring, Sutherland–Hodgman's
    /// boundary-following clip is a self-touching polygon that is area- and
    /// fill-equivalent to the `i_overlay` split under nonzero winding, so the
    /// fallback (which fires on ~94% of fine-zoom polygon clips) is wasted work.
    /// Non-simple inputs always keep the fallback, preserving the #94 U-shape
    /// fix. Defaults to `true` (fast path on); the S-H clip of a simple ring is
    /// render-equivalent to the fallback but stored rotated to a different start
    /// vertex, so disable it (`--no-simple-clip-fastpath`) only when byte-stable
    /// tile output is required.
    #[cfg_attr(feature = "cli", arg(default_value_t = Self::default().simple_clip_fastpath, action = clap::ArgAction::SetFalse, long = "no-simple-clip-fastpath"))]
    #[cfg_attr(feature = "cli", arg(help = "Clip all polygons with the full overlay for byte-stable tiles. The simple-ring fast path renders identically but can change a ring's starting vertex", long_help = None))]
    pub simple_clip_fastpath: bool,
    /// Simplify each feature of the finest (canonical) level once, before
    /// clipping, with tolerance `factor × GSD` at that level's zoom, the
    /// same rule `--simplify-factor` uses for coarser levels. With the default
    /// GSD base, 1 GSD is 4 tile units at extent 4096. It only removes
    /// vertices: a feature, part or ring smaller than the tolerance keeps its
    /// own geometry, and a polygon whose simplified rings would be invalid
    /// keeps its own. Default `None`: lines at
    /// 0.25 × GSD and polygons not at
    /// all, since each polygon is simplified on its own and shared borders
    /// would part (#802). `Some(factor)` applies to lines and polygons alike;
    /// `Some(0.0)` simplifies nothing, which [`Self::verbatim`] sets.
    /// Leaves coarser levels unchanged, except that
    /// variable-depth stop tiles carry the simplified finest-level features.
    /// Leaves the overview file unchanged.
    /// Comparable to tippecanoe's `-S`, which uses tile units at every zoom:
    /// `-S 10` is about factor 2.5.
    #[cfg_attr(
        feature = "cli",
        arg(long, value_name = "FACTOR", help_heading = "Generalization")
    )]
    #[cfg_attr(feature = "cli", arg(help = "Simplify each finest-level (canonical) line and polygon once before clipping, with tolerance factor × GSD at that level's zoom, like --simplify-factor for coarser levels. With the default GSD base, 1 GSD is 4 tile units at extent 4096. Only vertices are removed: features, parts and rings smaller than the tolerance keep their own geometry. Leaves coarser levels unchanged, except that variable-depth stop tiles carry the simplified finest-level features. Leaves the overview file unchanged. Unset, lines get 0.25 and polygons are not simplified, since each polygon is simplified on its own and shared borders would part; `0` simplifies nothing, as `tiles --verbatim` and `pyramid` bands do. Comparable to tippecanoe's -S, which uses tile units at every zoom: -S 1 is about factor 0.25", long_help = None))]
    pub max_zoom_simplify_factor: Option<f64>,
    /// Shared-border pins at the finest level (#802, default **off**): when
    /// [`Self::max_zoom_simplify_factor`] simplifies polygons, rebuild the
    /// level's junction pins and keep them, as
    /// [`SimplifyOptions::shared_nodes`](crate::overview::simplify::SimplifyOptions::shared_nodes)
    /// does for coarser levels. `tiles` sets it from that option.
    #[serde(rename = "no_simplification_of_shared_nodes")]
    #[cfg_attr(
        feature = "cli",
        arg(
            long = "no-simplification-of-shared-nodes",
            help_heading = "Generalization"
        )
    )]
    #[cfg_attr(feature = "cli", arg(help = "With --max-zoom-simplify-factor, keep the vertices where shared polygon borders meet, end, or branch at the finest level, and simplify every shared stretch between them the same way in each polygon. Like tippecanoe's -pn", long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub shared_nodes: bool,
    /// Number of partitions processed per band read (the export concurrency
    /// knob). Default [`PARTITION_WAVE_AUTO`], which [`resolve_partition_wave`]
    /// expands at export start via the memory-budget preflight (#303): the
    /// machine's available parallelism, capped by how many estimated
    /// per-partition transients fit in a fraction of available RAM, floored
    /// at [`PARTITION_WAVE_MIN`] (when RAM cannot be probed, the budget's
    /// fallback allowance stands in for it; under [`ExportOptions::profile`]
    /// `speed` the RAM cap is off). Any explicit positive value is honoured verbatim. Wider waves keep more
    /// cores busy at proportionally more peak memory (`O(partition_wave)`
    /// partitions resident) and read a wider combined-bbox band per wave.
    /// Output is byte-identical for every value — the wave is a scheduling
    /// concern only.
    #[cfg_attr(feature = "cli", arg(long, value_name = "N|auto", default_value = "auto", value_parser = super::config::cli::parse_partition_wave))]
    #[cfg_attr(feature = "cli", arg(help = "Partitions to export at once, or `auto` to size the wave to the cores and free memory. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>", long_help = None))]
    pub partition_wave: usize,
    /// Within-tile feature order (#361). Defaults to [`FeatureOrder::Input`],
    /// the order tylertoo has always emitted.
    #[serde(with = "super::config::spelled")]
    #[cfg_attr(feature = "cli", arg(
        long,
        value_name = "input|COLUMN[:asc|:desc]",
        default_value_t = ExportOptions::default().feature_order
    ))]
    #[cfg_attr(feature = "cli", arg(help = "Feature order within each tile: `input` preserves source row order; `COLUMN[:asc|:desc]` sorts by a property. Most renderers paint in this order unless the style overrides it. A `pyramid` band without that column keeps input order with a warning", long_help = None))]
    pub feature_order: FeatureOrder,
    /// Minimum zoom the archive declares in its metadata, even when the
    /// overview file's coarsest levels are missing (#380).
    ///
    /// The converter omits a level that generalizes to nothing (spec §7.3), so
    /// an overview built for z0..z13 can start at z2. Set to the requested
    /// minimum, `vector_layers[].minzoom` (and [`ExportReport::min_zoom`])
    /// cover it; the empty zooms simply have no tiles, which in PMTiles is an
    /// empty tile. The PMTiles header's own `min_zoom` is NOT widened: it is
    /// always the shallowest zoom that actually holds a tile (#529, #522), as
    /// `go-pmtiles verify` requires. Renderers that build TileJSON from the
    /// header (the pmtiles JS library does) therefore see the narrower range;
    /// the empty zooms would render nothing either way. Must not be finer
    /// than the coarsest level present — that would misdescribe real tiles —
    /// and is rejected if it is. The one-shot `tiles` command passes its
    /// `--min-zoom` here; `export-pmtiles --min-zoom` sets it directly.
    #[cfg_attr(feature = "cli", arg(long, value_name = "ZOOM"))]
    #[cfg_attr(feature = "cli", arg(help = "Minimum zoom in archive metadata, including empty coarse overview levels. Defaults to the coarsest level's zoom", long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub min_zoom: Option<u8>,
    /// Which properties the tiles carry (#386): tippecanoe's `-x` / `-y` /
    /// `-X`, matched against the property names as the tiles would publish
    /// them. The overview file is untouched. Default: keep everything the
    /// file exports.
    #[serde(flatten)]
    #[cfg_attr(feature = "cli", arg(skip = Self::default().properties))]
    pub properties: PropertySelection,
    /// Emit only the tiles inside this pivot-zoom range — one shard of a
    /// sharded build (#498).
    ///
    /// Tiles at zooms **coarser than the pivot** are outside every range, so a
    /// shard never emits them; one coarse job owns them via
    /// [`zoom_ceiling`](Self::zoom_ceiling). At every zoom from the pivot
    /// down, the range names an *exact* contiguous tile-id interval
    /// ([`crate::shard::TileRange::ids_at`]), and N ranges that partition the
    /// pivot zoom partition every deeper zoom — so the shards' archives are
    /// disjoint by construction and merge by blob copy.
    ///
    /// Features are kept **whole** through convert and clip: a feature
    /// straddling a seam is read by both neighbouring shards and clipped
    /// normally by both; each then emits only the tiles its own range owns.
    /// That is what makes the seam exact rather than double-counted (the flaw
    /// in the `--bbox`-band workaround this replaces).
    ///
    /// The restriction is applied at **plan time**, before any clipping: the
    /// per-zoom tile-count map the partitions are cut from is filtered first,
    /// so out-of-range tiles are never clipped, encoded or hashed, and each
    /// wave's band read prunes to the shard's narrower bbox. A shard therefore
    /// costs roughly its share of the export, not the whole of it.
    ///
    /// Default `None` (emit everything).
    #[serde(with = "super::config::tile_range")]
    #[cfg_attr(feature = "cli", arg(value_parser = crate::shard::TileRange::parse, long, value_name = "LO..HI"))]
    #[cfg_attr(feature = "cli", arg(help = "Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at one zoom, plus their descendants. Ranges that partition a zoom give disjoint archives, which `tylertoo merge` can join", long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub tile_range: Option<crate::shard::TileRange>,
    /// Emit only the tiles at or below this zoom — the coarse half of a
    /// sharded build (#498).
    ///
    /// Set to `pivot - 1` on the coarse job, which owns the zooms no shard
    /// does. The convert feeding it must still run the whole zoom range (its
    /// plan is the one every shard consumes, and the level plan is
    /// fingerprinted), so the ceiling belongs here, on the export, rather
    /// than on `--max-zoom`.
    ///
    /// Default `None` (no ceiling).
    #[cfg_attr(feature = "cli", arg(long, value_name = "ZOOM"))]
    #[cfg_attr(feature = "cli", arg(help = "Emit only the tiles at or below this zoom, the coarse half of a sharded build. A partial overview kept from `tiles --shard coarse` needs a ceiling at or below its own", long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub zoom_ceiling: Option<u8>,
    /// Carry a property column through as the MVT feature `id` on every tile
    /// the feature appears in, at every zoom (#443; tippecanoe's
    /// `--use-attribute-for-id`). Matched against the property name as the
    /// tile would publish it -- the same naming `--feature-order` /
    /// `--include-property` / `--exclude-property` use -- and independent of
    /// any suppression already recorded for that column: an explicit
    /// `--feature-id` always wins over the #379 auto-suppression heuristic
    /// and over a `--include-property` / `--exclude-property` naming the same
    /// column.
    ///
    /// The named column must be an integer type (`Int8`..`Int64`,
    /// `UInt8`..`UInt64`) or an unscaled decimal (`DECIMAL(p,0)`), and every
    /// value in every row of the overview file a non-null integer in
    /// `0..=u64::MAX` -- MVT's feature id is `uint64`. The whole column is
    /// checked up front, before any tile is written; a violation is an
    /// [`ExportError::FeatureIdColumn`] or [`ExportError::InvalidFeatureId`]
    /// (naming the overview-file row and level), never a silent
    /// reinterpretation. String and float columns are rejected (a divergence
    /// from tippecanoe, which parses them): cast them to an integer first.
    ///
    /// The column is moved to the feature id, never *also* published as a
    /// regular tile property (tippecanoe's behaviour) -- it is stripped from
    /// `vector_layers` and from every tile's tags regardless of the property
    /// selection in force.
    ///
    /// Aggregated features: a cluster carries its representative's id, a
    /// coalesced line chain its highest-priority member's, and a #384
    /// tiny-polygon carrier its own. Uniqueness across source rows is not
    /// checked.
    ///
    /// Default `None`: features have no MVT id.
    #[cfg_attr(feature = "cli", arg(long, value_name = "COLUMN"))]
    #[cfg_attr(feature = "cli", arg(help = "Use this integer column as the MVT feature id for `setFeatureState` across tiles and zooms. Every row must contain 0 to 2^64-1. By default features have no id", long_help = None))]
    pub feature_id: Option<String>,
    /// Directory for the export's member spill file (#427) -- the on-disk
    /// backing the partitioning single-read pass 2 (#235) falls back to when
    /// the buffered members would not fit the memory budget. Same name and
    /// semantics as [`ConvertOptions::spill_dir`](super::convert::ConvertOptions::spill_dir):
    /// the one scratch-disk knob, checked up front (it must be an existing
    /// directory, [`ConfigError::SpillDirNotDirectory`] otherwise). `None`
    /// uses the process temp directory (`$TMPDIR`).
    ///
    /// The PMTiles archive itself is never spilled here: it is assembled in
    /// place at `<output>.partial` beside the output (#459).
    #[cfg_attr(
        feature = "cli",
        arg(long, value_name = "PATH", help_heading = "Memory & performance")
    )]
    #[cfg_attr(feature = "cli", arg(help = "Directory for the export's spill file, used when buffered tiles exceed the memory budget. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>", long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub spill_dir: Option<PathBuf>,
    /// Memory profile for the export (#788), the same values and rule as
    /// [`ConvertOptions::profile`](super::convert::ConvertOptions::profile).
    /// `speed` sets no limit: the member stores stay in RAM and the
    /// partition wave is the core count. `bounded` keeps export's historical
    /// footprint: the stores spill and the wave keeps its memory-safe cap.
    /// `auto` keeps the stores in RAM when they fit the allowance and
    /// narrows the wave to fit it. Under every profile export still refuses
    /// a tile that alone needs more than the machine's memory (#665): no
    /// profile makes one tile smaller. `tiles` passes its own `--profile`
    /// through. Changes speed and peak memory only; output is
    /// byte-identical across profiles. The CLI and Python extension purge freed
    /// mimalloc pages immediately under `bounded` (at a speed cost); core does
    /// not configure the allocator. See [`MemoryProfile`]. Default [`MemoryProfile::Auto`].
    #[serde(with = "super::config::spelled")]
    #[cfg_attr(feature = "cli", arg(
        long,
        default_value = ExportOptions::default().profile.as_str(),
        value_parser = super::config::cli::spelled::<MemoryProfile>(&["auto", "speed", "bounded"]),
        help_heading = "Memory & performance"
    ))]
    #[cfg_attr(feature = "cli", arg(help = "Memory profile: `speed` holds everything in RAM, `bounded` spills to disk and holds the least, and `auto` sizes every buffer to the machine and spills when it must. The default mimalloc build also purges freed pages immediately under `bounded` (at a speed cost); a `--no-default-features` build uses the system allocator. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles>", long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub profile: MemoryProfile,
    /// Top-level `name`, `description` and `attribution` written to the
    /// archive metadata (tippecanoe's `-n` / `-N` / `-A`).
    ///
    /// Unset fields are left out rather than derived from a filename, and
    /// nothing here changes the tiles. Default: none set.
    #[serde(flatten)]
    #[cfg_attr(feature = "cli", arg(skip = Self::default().metadata))]
    pub metadata: TilesetMetadata,
}

with_setters!(ExportOptions {
    with_layer_name => layer_name: String,
    with_tile_buffer => tile_buffer: u32,
    with_extent => extent: u32,
    with_tile_size_limit => tile_size_limit: Option<usize>,
    with_compressed_tile_size_limit => compressed_tile_size_limit: Option<usize>,
    with_size_valve => size_valve: SizeValve,
    with_tile_format => tile_format: TileFormat,
    with_mlt_profile => mlt_profile: MltProfile,
    with_coverage_fill => coverage_fill: bool,
    with_simple_clip_fastpath => simple_clip_fastpath: bool,
    with_max_zoom_simplify_factor => max_zoom_simplify_factor: Option<f64>,
    with_shared_nodes => shared_nodes: bool,
    with_partition_wave => partition_wave: usize,
    with_feature_order => feature_order: FeatureOrder,
    with_min_zoom => min_zoom: Option<u8>,
    with_properties => properties: PropertySelection,
    with_tile_range => tile_range: Option<crate::shard::TileRange>,
    with_zoom_ceiling => zoom_ceiling: Option<u8>,
    with_feature_id => feature_id: Option<String>,
    with_spill_dir => spill_dir: Option<PathBuf>,
    with_profile => profile: MemoryProfile,
    with_metadata => metadata: TilesetMetadata,
    with_variable_depth => variable_depth: bool,
});

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            layer_name: "overview".to_string(),
            tile_buffer: DEFAULT_TILE_BUFFER_PX,
            extent: DEFAULT_EXTENT,
            tile_size_limit: Some(DEFAULT_TILE_SIZE_LIMIT),
            compressed_tile_size_limit: Some(DEFAULT_COMPRESSED_TILE_SIZE_LIMIT),
            size_valve: SizeValve::Merge,
            tile_format: TileFormat::Mvt,
            mlt_profile: MltProfile::Web,
            coverage_fill: false,
            variable_depth: false,
            simple_clip_fastpath: true,
            max_zoom_simplify_factor: None,
            shared_nodes: false,
            partition_wave: PARTITION_WAVE_AUTO,
            feature_order: FeatureOrder::default(),
            min_zoom: None,
            properties: PropertySelection::default(),
            tile_range: None,
            zoom_ceiling: None,
            feature_id: None,
            spill_dir: None,
            profile: MemoryProfile::Auto,
            metadata: TilesetMetadata::default(),
        }
    }
}

impl ExportOptions {
    pub(crate) fn has_tile_size_cap(&self) -> bool {
        self.tile_size_limit.is_some_and(|cap| cap > 0)
            || self.compressed_tile_size_limit.is_some_and(|cap| cap > 0)
    }

    /// The export half of a verbatim run (#761): no per-tile size cap, raw
    /// or compressed, since a valve that sheds features to fit a byte budget
    /// is not verbatim either. Everything else is left alone, so an explicit
    /// cap set afterwards still applies. The defaults of `tiles --verbatim`
    /// and of every `pyramid` band.
    #[must_use]
    pub fn verbatim(mut self) -> Self {
        self.tile_size_limit = None;
        self.compressed_tile_size_limit = None;
        // Nor is simplifying the finest level's lines.
        self.max_zoom_simplify_factor = Some(0.0);
        self
    }

    /// Check the option values that need no input file (#433): `extent`
    /// must be positive, `tile_buffer` at most [`MAX_TILE_BUFFER_PX`], and
    /// `metadata` small enough for an archive.
    ///
    /// [`export_pmtiles`] runs this first, before the overview file is
    /// opened, so a bad knob fails before any I/O. A front end that does
    /// expensive work before exporting (the `tiles` facade converts the whole
    /// input first) can call it up front and fail in milliseconds instead.
    ///
    /// A non-power-of-two `extent` is accepted with a `log::warn!`: the MVT
    /// spec only recommends a power of two, but renderers and this crate's
    /// own decoder (see `decode.rs`'s module docs) assume one when they
    /// reason about precision.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidExtent`] if `extent` is 0,
    /// [`ConfigError::TileBufferTooWide`] if `tile_buffer` is above
    /// [`MAX_TILE_BUFFER_PX`], [`ConfigError::SpillDirNotDirectory`] for a
    /// missing spill directory, or [`ConfigError::MetadataTooLarge`] if
    /// `metadata` is too large for an archive's metadata.
    /// Validate these options and return the immutable engine configuration.
    #[doc(hidden)]
    pub fn validate(&self) -> Result<super::config::ValidatedConfig<'_, Self>, ConfigError> {
        super::config::validate_export(self)
    }

    pub(crate) fn check(&self) -> Result<(), ConfigError> {
        for (option, zoom) in [
            ("--min-zoom / min_zoom", self.min_zoom),
            ("--zoom-ceiling / zoom_ceiling", self.zoom_ceiling),
        ] {
            if let Some(zoom) = zoom.filter(|zoom| *zoom > crate::tile::MAX_ZOOM) {
                return Err(ConfigError::InvalidSpec {
                    option,
                    message: format!("{option} {zoom} must not exceed {}", crate::tile::MAX_ZOOM),
                });
            }
        }
        if let Some(factor) = self
            .max_zoom_simplify_factor
            .filter(|f| !(f.is_finite() && *f >= 0.0))
        {
            let option = "--max-zoom-simplify-factor / max_zoom_simplify_factor";
            return Err(ConfigError::InvalidSpec {
                option,
                message: format!("{option} must be a finite number >= 0, got {factor}"),
            });
        }
        if self.tile_format == TileFormat::Mlt && !crate::tile_format::MLT_SUPPORTED {
            return Err(ConfigError::InvalidSpec {
                option: "--tile-format / tile_format",
                message: "this build cannot write MLT: rebuild with the `mlt` cargo feature".into(),
            });
        }
        if self.tile_format == TileFormat::Mvt && self.mlt_profile != MltProfile::default() {
            return Err(ConfigError::OptionIgnored {
                option: "mlt_profile",
                command: "an MVT export",
                reason: "it sets how MLT tiles are encoded; pass --tile-format mlt",
            });
        }
        if self.tile_format == TileFormat::Mlt && self.layer_name.is_empty() {
            return Err(ConfigError::InvalidSpec {
                option: "--layer-name / layer_name",
                message: "an MLT layer needs a name".into(),
            });
        }
        if self.extent == 0 {
            return Err(ConfigError::InvalidExtent {
                extent: self.extent,
            });
        }
        if !self.extent.is_power_of_two() {
            log::warn!(
                "[export] extent {} is not a power of two; the MVT spec recommends one \
                 ({DEFAULT_EXTENT} is the default) and decoders assume one when reasoning \
                 about coordinate precision",
                self.extent
            );
        }
        if self.tile_buffer > MAX_TILE_BUFFER_PX {
            return Err(ConfigError::TileBufferTooWide {
                buffer: self.tile_buffer,
                max: MAX_TILE_BUFFER_PX,
            });
        }
        // #427: fail fast on a bad spill dir, the convert side's rule.
        super::config::check_spill_dir(self.spill_dir.as_deref())?;
        variable_depth::check_options(self)?;
        // Publication strings that cannot fit an archive's metadata would
        // otherwise fail only when the first checkpoint is assembled.
        check_metadata_size(&self.metadata)
    }
}

/// [`TilesetMetadata`] too large for any archive's metadata.
fn check_metadata_size(metadata: &TilesetMetadata) -> Result<(), ConfigError> {
    metadata.check_size().map_err(ConfigError::MetadataTooLarge)
}

/// Per-zoom statistics in an [`ExportReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
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
    /// Tiles written at this zoom that the size valve could not bring under
    /// the limit, including one-feature tiles; written as is. Unlike
    /// `oversized_tiles`, counts tiles still over the limit, not all tiles
    /// that triggered the valve. The report prints one summary line for
    /// the total across zooms.
    pub over_limit_tiles: usize,
    /// Tile members at this zoom that had nothing to encode (#431): empty
    /// geometries and empty `GeometryCollection`s. Counted per (feature,
    /// tile) pair, like `tile_feature_count`. Non-zero means content was lost
    /// after clipping; a warning names the total at the end of the export.
    pub encode_dropped_features: usize,
    /// Tile members at this zoom whose geometry had coordinates but collapsed
    /// at the tile extent and so produced no MVT feature (#431): polygon rings
    /// of zero area, lines of fewer than two points — typically a clip sliver
    /// at a buffered tile edge. Expected on ordinary data; informational, and
    /// never a warning.
    pub encode_quantized_features: usize,
    /// Tile members at this zoom the size valve merged into another member
    /// (#745). Always 0 with [`SizeValve::Drop`].
    pub merged_features: usize,
    /// Tile members at this zoom the size valve dropped (#745): all of the
    /// drop valve's, or a merging valve's when merging alone could not make
    /// a tile fit.
    pub valve_dropped_features: usize,
    /// The most encodes any tile at this zoom took (#745); 1 when every tile
    /// fit at once, whichever the valve, and 0 for a zoom with no tiles.
    pub max_valve_passes: u32,
    /// Encodes all tiles at this zoom took, summed (#750): the tile count
    /// when every tile fit at once. What the size valve costs, where
    /// `max_valve_passes` shows only its worst tile.
    pub valve_passes: u64,
    /// #750 coverage-fill polygons drawn into this zoom's tiles; 0 without
    /// [`ExportOptions::coverage_fill`] and at the finest zoom.
    pub fill_polygons: usize,
    /// Vertices of those fill polygons: the fill's share of the encode work.
    pub fill_vertices: usize,
    /// Fill polygons left out to stay within the fill's share of a tile's
    /// size limit, smallest first.
    pub fill_shed: usize,
    /// #803 stopped tiles at this zoom, counted in `tile_count`: tiles that
    /// hold every finest-level feature under them, with nothing stored
    /// below. 0 without [`ExportOptions::variable_depth`].
    pub stopped_tiles: usize,
    /// The largest of `over_limit_tiles`, for the diagnostic that lists them
    /// (#873).
    #[serde(skip)]
    over_limit_largest: Vec<crate::stats::LargestTile>,
    /// How many of `over_limit_tiles` hold merged features (#873).
    #[serde(skip)]
    over_limit_merged: usize,
    /// #873 tiles at this zoom that the merge valve re-encoded at a reduced
    /// extent (halved down to 512) to fit their cap.
    pub reduced_extent_tiles: usize,
    /// The smallest extent among those tiles; `None` when there are none.
    pub min_reduced_extent: Option<u32>,
    /// #873 merged features the merge valve dropped at the extent floor.
    pub dropped_merged_features: usize,
    /// The source features those dropped merged features carried, also
    /// counted in `valve_dropped_features`.
    pub dropped_merged_sources: usize,
}

/// Result of an export, `Serialize` for the `--report` JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ExportReport {
    /// Exact source losses, disjoint from any source emitted at that zoom.
    pub source_losses: crate::diagnostics::SourceLosses,
    /// Aggregated diagnostics for this export only.
    pub diagnostics: Vec<crate::diagnostics::Diagnostic>,
    /// Level materialization mode of the source overview file.
    pub mode: String,
    /// The declared minimum zoom (coarsest level's zoom unless widened by
    /// [`ExportOptions::min_zoom`]) — what the CLI prints as the export's
    /// zoom range and what `vector_layers[].minzoom` advertises. The PMTiles
    /// header's own `min_zoom` can differ: it always reflects the shallowest
    /// zoom that actually holds a tile (#529, #522), which `go-pmtiles
    /// verify` requires and which is coarser than this value whenever a
    /// requested level generalized away to nothing (#380).
    pub min_zoom: u8,
    /// The declared maximum zoom (finest level's zoom). See `min_zoom` above
    /// for how this relates to the PMTiles header's own `max_zoom`.
    pub max_zoom: u8,
    /// Per-zoom statistics, coarse→fine.
    pub zooms: Vec<ZoomReport>,
    /// Total tiles written across all zooms.
    pub total_tiles: usize,
    /// Total features written across all tiles (with border duplication).
    pub total_tile_features: usize,
    /// Total tiles that hit the oversized safety valve.
    pub oversized_tiles: usize,
    /// Total tiles the size valve could not bring under the limit, including
    /// one-feature tiles; written as is. Unlike `oversized_tiles`, counts
    /// tiles still over the limit, not all tiles that triggered the valve.
    /// The report prints one summary line for these tiles.
    pub over_limit_tiles: usize,
    /// Property columns of the overview file that the export could not
    /// encode and therefore dropped (#434): binary columns and any other
    /// Arrow type with neither an MVT scalar nor a JSON rendering. Each is
    /// warned about once, at resolution, and listed here so the loss is in
    /// the `--report` as well as the log. Struct/list/map columns are NOT
    /// here: they reach the tiles as JSON strings.
    pub skipped_property_columns: Vec<SkippedPropertyColumn>,
    /// Total tile members with nothing to encode, across all zooms — see
    /// [`ZoomReport::encode_dropped_features`] (#431).
    pub encode_dropped_features: usize,
    /// Total tile members that collapsed at the tile extent, across all
    /// zooms — see [`ZoomReport::encode_quantized_features`] (#431).
    pub encode_quantized_features: usize,
    /// Total tile members the size valve merged, across all zooms (#745).
    pub merged_features: usize,
    /// Total tile members the size valve dropped, across all zooms (#745).
    pub valve_dropped_features: usize,
    /// Wall-clock export duration in seconds.
    pub duration_secs: f64,
    /// The #803 variable-depth summary; `None` without
    /// [`ExportOptions::variable_depth`].
    pub variable_depth: Option<VariableDepthReport>,
}

/// How a variable-depth export (#803) stopped its branches. Per-zoom stop
/// counts are [`ZoomReport::stopped_tiles`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct VariableDepthReport {
    /// The finest level's zoom: what [`ExportReport::max_zoom`] declares, and
    /// the precision every stopped tile keeps.
    pub canonical_max_zoom: u8,
    /// The deepest zoom holding a tile: the PMTiles header's `max_zoom`.
    /// Shallower than the canonical one when every branch stopped above it.
    pub stored_max_zoom: Option<u8>,
    /// Tiles probed: drawn from the finest level's features to see whether
    /// they fit both size caps.
    pub probed_tiles: usize,
    /// Probed tiles that fit, and stopped their branch.
    pub stopped_tiles: usize,
}

/// A property column the export dropped because its Arrow type has no MVT
/// encoding (#434). See [`ExportReport::skipped_property_columns`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct SkippedPropertyColumn {
    /// The column's name as the tile would have published it.
    pub name: String,
    /// The Arrow type, rendered with `Debug` (`Binary`, `LargeBinary`, ...).
    pub data_type: String,
}

/// Errors from [`export_pmtiles`].
///
/// `#[non_exhaustive]`: new failure modes are added without a breaking
/// change; match with a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExportError {
    /// Source accounting spill or report exceeded its budget.
    #[error(transparent)]
    Accounting(#[from] crate::diagnostics::AccountingError),
    /// The output cannot be written (#452: checked before the input is
    /// opened).
    #[error(transparent)]
    Output(#[from] crate::atomic_output::OutputNotWritable),
    /// Opening or reading the overview file failed.
    #[error("overview reader error: {0}")]
    Reader(#[from] ReaderError),
    /// An export phase's required resident state exceeds its memory grant.
    /// The display text retains the historical budget diagnostic, without
    /// the preflight escape hatch: `TYLERTOO_SKIP_MEMORY_PREFLIGHT` covers
    /// the up-front checks only, never this one.
    #[error("io error: {phase} needs {needed} bytes but only {available} bytes are available; use a bigger machine or reduce the input with --bbox/--filter.")]
    #[non_exhaustive]
    BudgetExceeded {
        /// Export phase that cannot fit.
        phase: &'static str,
        /// Required resident bytes.
        needed: u64,
        /// Granted workspace bytes.
        available: u64,
    },
    /// I/O error (writing the PMTiles archive).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Arrow error decoding a level batch.
    #[error("arrow error: {0}")]
    // #437: hidden, so the third-party payload type is not stable API.
    #[doc(hidden)]
    Arrow(#[source] arrow_schema::ArrowError),
    /// A core-library error (geometry decode, PMTiles write).
    #[error("{0}")]
    Core(#[from] crate::Error),
    /// The input CRS is neither EPSG:4326 nor EPSG:3857.
    #[error("unsupported CRS {crs:?} in {path}: export requires EPSG:4326 or EPSG:3857")]
    #[non_exhaustive]
    UnsupportedCrs {
        /// The rejected CRS identifier.
        crs: String,
        /// The overview file (#452).
        path: String,
    },
    /// The file has no geometry column.
    #[error(
        "{} has no geometry column",
        path.as_deref().unwrap_or("an overview batch")
    )]
    #[non_exhaustive]
    NoGeometryColumn {
        /// The overview file (#452). `None` only from per-batch decoding,
        /// which runs after the file's own check has passed, so it means a
        /// bug rather than a bad file.
        path: Option<String>,
    },

    #[error(
        "declared min_zoom {declared} is finer than the coarsest level present (zoom \
         {coarsest}): the declared zoom range can widen over empty zooms, not narrow \
         over real tiles (#380)"
    )]
    #[non_exhaustive]
    DeclaredMinZoomTooFine { declared: u8, coarsest: u8 },

    /// A level resolves to a zoom above [`crate::tile::MAX_ZOOM`] (#371).
    ///
    /// Two ways in. The `--gsd` door: an explicit GSD ladder records no `zoom`
    /// on its levels, so the exporter derives one from the GSD, and that
    /// derivation used to clamp to `u8::MAX` — a `--gsd 0.000005` exported at
    /// z255 regardless of `--max-zoom`. And a recorded `zoom` out of range,
    /// which an overview file written before the ceiling existed can carry.
    /// Either way the level is named along with the zoom it resolves to.
    #[error(
        "overview level {level} (gsd {gsd} m) resolves to zoom {implied} — above the \
         maximum supported zoom {ceiling}; beyond z{ceiling} the tile grid overflows \
         32-bit tile coordinates and the PMTiles Hilbert tile-id arithmetic (#371)"
    )]
    #[non_exhaustive]
    LevelZoomAboveCeiling {
        /// Index of the offending level.
        level: usize,
        /// The level's ground sample distance in meters.
        gsd: f64,
        /// The zoom the level resolves to (recorded, or derived from the GSD).
        implied: i64,
        /// [`crate::tile::MAX_ZOOM`].
        ceiling: u8,
    },

    /// A level's GSD is not a finite positive number (#371).
    ///
    /// Distinct from [`LevelZoomAboveCeiling`](ExportError::LevelZoomAboveCeiling):
    /// `0`, a negative GSD and `NaN` have no zoom to be above or below a
    /// ceiling, and reporting them as "implies zoom 9223372036854775807" said
    /// nothing about what was actually wrong. (`+inf` is not one of these — it
    /// is infinitely coarse, which is z0.) The conversion side rejects these up
    /// front; this is the guard for an overview file that already carries one.
    #[error(
        "overview level {level} has gsd {gsd}, which is not a finite positive ground \
         sample distance in meters (#371)"
    )]
    #[non_exhaustive]
    LevelGsdNotFinitePositive {
        /// Index of the offending level.
        level: usize,
        /// The level's recorded GSD.
        gsd: f64,
    },

    /// An option value refused before the overview file is opened
    /// ([`ExportOptions::validate`], #433, #779): the extent, the tile
    /// buffer, the spill directory, the publication metadata.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// `--tile-buffer` is wider than a shard's read-pruning margin (#498).
    ///
    /// No longer raised since #433: the universal cap
    /// ([`MAX_TILE_BUFFER_PX`], enforced as
    /// [`ConfigError::TileBufferTooWide`]) is at most the
    /// shard margin, so a buffer that would trip this trips that first. Kept
    /// so code matching on it keeps compiling.
    #[error(
        "--tile-buffer {buffer} is too wide for a sharded build: a shard prunes its input to \
         the row groups within {max} tile pixels of its range, so a wider buffer could pull \
         geometry into a tile from a row group the shard never read. Lower --tile-buffer to \
         {max} or less (the default is 8), or build without --tile-range."
    )]
    #[non_exhaustive]
    TileBufferTooWideForShard {
        /// The requested buffer, in tile pixels.
        buffer: u32,
        /// `crate::shard::MAX_SHARD_TILE_BUFFER_PX`.
        max: u32,
    },

    /// A shard restriction leaves no zoom to emit at all (#498).
    ///
    /// Named for the *restriction*, not the range: this is not
    /// [`crate::shard::ShardError::EmptyTileRange`] (a range whose LO is past
    /// its HI, which is a malformed range). The range here is perfectly
    /// well-formed — it simply has no overlap with the zooms the overview
    /// file holds.
    #[error(
        "this export would emit no zoom at all: the restriction starts at z{coarsest} and \
         stops at z{finest}. A --tile-range's pivot must be no finer than the overview's \
         finest level, and a coarse job's zoom ceiling no coarser than its coarsest."
    )]
    #[non_exhaustive]
    RestrictionEmitsNoZoom {
        /// The coarsest zoom the restriction would emit.
        coarsest: u8,
        /// The finest zoom the restriction would emit.
        finest: u8,
    },

    /// The overview file is **partial** (#541 review): it was written under a
    /// convert-side zoom ceiling (the coarse job of a sharded build), so it
    /// holds no level finer than `recorded` and its finest level is not the
    /// canonical one. It can only be exported with a zoom ceiling at or
    /// coarser than `recorded`.
    #[error(
        "the overview file is partial: it was built with a zoom ceiling of z{recorded} (the \
         coarse job of a sharded build), so it holds no level finer than z{recorded}{}. Export \
         it with a zoom ceiling of at most z{recorded}, or re-run the convert without the \
         ceiling for a complete pyramid",
        match requested {
            Some(c) => format!(" and cannot be exported up to z{c}"),
            None => " and cannot be exported as a whole pyramid".to_string(),
        }
    )]
    #[non_exhaustive]
    PartialOverview {
        /// The ceiling recorded in the file's footer.
        recorded: u8,
        /// The export's own ceiling, if any.
        requested: Option<u8>,
    },

    /// `--min-zoom` is above [`crate::tile::MAX_ZOOM`] (#371).
    #[error("--min-zoom {declared} is above the maximum supported zoom {ceiling} (#371)")]
    #[non_exhaustive]
    DeclaredMinZoomAboveCeiling {
        /// The requested minimum zoom.
        declared: u8,
        /// [`crate::tile::MAX_ZOOM`].
        ceiling: u8,
    },

    /// Two levels resolve to the same (or a non-increasing) zoom (#506
    /// follow-up).
    ///
    /// `zoom_for_level`'s §3.3 validation only requires `gsd` to be strictly
    /// decreasing across levels, not that the ZOOM it rounds to (the §5.2
    /// inverse) is distinct per level: two GSDs close enough to round to the
    /// same integer zoom, or two GSDs both coarser than the zoom-0
    /// threshold, both pass that check and both resolve here to the same
    /// zoom. Export keys each level's tiles by PMTiles tile id within that
    /// level's own zoom and streams levels ascending (#506); a duplicate
    /// zoom would write that zoom's tile ids a second time. Before #506 this
    /// silently clobbered directory entries (both levels used the same
    /// row-major `(x, y)` key, so the second level's write just overwrote
    /// the first level's slot in the directory — latent tile-data
    /// corruption with no error). Since #506 it instead trips the writer's
    /// ascending-tile-id `debug_assert!` in a debug build. Neither is
    /// acceptable: reject it here, with a real error, before any scan runs.
    #[error(
        "overview levels {previous_level} (gsd {previous_gsd} m) and {level} (gsd {gsd} m) \
         both resolve to zoom {zoom}; every level must resolve to a distinct, strictly \
         increasing zoom"
    )]
    #[non_exhaustive]
    LevelZoomsNotAscending {
        /// Index of the earlier, colliding level.
        previous_level: usize,
        /// The earlier level's resolved zoom (equal to `zoom`).
        previous_zoom: u8,
        /// The earlier level's GSD.
        previous_gsd: f64,
        /// Index of the later, colliding level.
        level: usize,
        /// The later level's resolved zoom (equal to `previous_zoom`).
        zoom: u8,
        /// The later level's GSD.
        gsd: f64,
    },

    /// #803: a column the variable-depth stop rule reads (a footer's
    /// entry-zoom or cluster count column) is missing, or its values do not
    /// read as numbers. Refused rather than read as empty, which would let
    /// tiles stop.
    #[error("--generate-variable-depth-tile-pyramid reads column {column:?}, which {reason}")]
    #[non_exhaustive]
    VariableDepthColumn { column: String, reason: String },

    #[error(
        "included property {name:?} is not a property this overview file exports \
         (exportable: {available})"
    )]
    #[non_exhaustive]
    UnknownProperty { name: String, available: String },

    /// `--include-property` names a column the file has but the export
    /// cannot encode (#434): a binary column, or another Arrow type with
    /// neither an MVT scalar nor a JSON rendering. Distinguished from
    /// [`Self::UnknownProperty`] so the caller learns the column is there
    /// and what to do about it, instead of hunting for a typo.
    #[error(
        "included property {name:?} is a {data_type} column, which the export cannot \
         encode as an MVT value (supported: strings, numbers, booleans, dates and \
         timestamps, decimals, and struct/list/map columns as JSON strings); cast or \
         encode it as text first, e.g. with gpio or DuckDB"
    )]
    #[non_exhaustive]
    UnsupportedProperty { name: String, data_type: String },

    /// `--include-property` names the geometry's bbox covering struct
    /// (#401). The covering is index metadata, never a property, so it is
    /// not exportable. Distinguished from [`Self::UnknownProperty`] because
    /// the column IS in the file; "unknown" would send the caller looking
    /// for a typo.
    #[error(
        "included property {name:?} is the geometry's bbox covering, not a property; \
         the writer regenerates it, so it cannot be selected"
    )]
    #[non_exhaustive]
    CoveringProperty { name: String },

    #[error(
        "property {name:?} is excluded but {knob} reads it; keep it in the selection or \
         drop the knob"
    )]
    #[non_exhaustive]
    PropertyRequiredByKnob { name: String, knob: String },

    /// `--feature-id` (#443) names a column that cannot supply MVT feature
    /// ids at all: absent, not an integer type, also the `--feature-order`
    /// column, or aggregated by `--accumulate-attribute`. Raised before any
    /// row is read.
    #[error("--feature-id column {column:?} {reason}")]
    #[non_exhaustive]
    FeatureIdColumn {
        /// The `--feature-id` column, as the caller named it.
        column: String,
        /// What is wrong with it.
        reason: FeatureIdColumnProblem,
    },

    /// A row of the overview file holds no valid `--feature-id` value
    /// (#443): null, negative, or beyond `u64`. Every row of the file -- all
    /// levels -- is checked up front, before any tile is written, so this
    /// arrives in seconds rather than after the coarser levels export.
    #[error(
        "--feature-id column {column:?} {reason} at row {row} of the overview file (level \
         {level}, z{zoom}; the overview file's own row order, not the source file's): every \
         exported feature needs a non-negative integer id"
    )]
    #[non_exhaustive]
    InvalidFeatureId {
        /// The `--feature-id` column, as the caller named it.
        column: String,
        /// The overview level whose row group holds the row.
        level: usize,
        /// That level's Web Mercator zoom.
        zoom: u8,
        /// 0-based row index in the overview file.
        row: u64,
        /// What is wrong with the value.
        reason: InvalidFeatureIdReason,
    },

    /// A level's densest tile is estimated to need more memory than the
    /// machine has, even exported on its own (#665). Raised after the scan,
    /// before any tile is written, against `MemAvailable` or a cgroup
    /// `memory.max`. A `TYLERTOO_AUTO_MEM_LIMIT_BYTES` override, or
    /// `TYLERTOO_SKIP_MEMORY_PREFLIGHT`, logs it as a warning instead.
    #[error(
        "overview level {level} (z{zoom}) cannot fit in memory: its densest tile, \
         z{zoom}/{x}/{y}, holds {members} features, an estimated {}, more than the {} of \
         {limit_source}. Export holds a whole tile in memory at once and never splits one, so \
         no --partition-wave value fits it. Rebuild the overview (`tiles` or `overview`) to \
         keep fewer features at this zoom: with --verbatim, turn thinning back on for the \
         dense geometry type alone (--point-thinning 4, --line-thinning 1 or \
         --polygon-thinning 1, the defaults) and the rest stays verbatim; or drop --verbatim; \
         or raise --min-zoom above z{zoom} so the dense coarse zooms are not built. If the \
         machine does have the memory, set {SKIP_MEMORY_PREFLIGHT_ENV}=1 to downgrade this to \
         a warning and attempt the export (#665)",
        human_bytes(*.estimate_bytes),
        human_bytes(*.available_bytes)
    )]
    #[non_exhaustive]
    TileExceedsMemory {
        /// 0-based overview level.
        level: usize,
        /// The level's zoom.
        zoom: u8,
        /// Column of the densest tile.
        x: u32,
        /// Row of the densest tile.
        y: u32,
        /// Members of that tile.
        members: usize,
        /// Estimated resident bytes of that tile's members.
        estimate_bytes: u64,
        /// The memory figure the estimate was compared against.
        available_bytes: u64,
        /// Where that figure came from, as messages name it.
        limit_source: &'static str,
    },

    /// A finished MVT tile did not convert to MLT (`--tile-format mlt`,
    /// #823): mlt-core refused it.
    #[error("a tile did not convert to MLT: {message}; rerun with --tile-format mvt")]
    MltConversion {
        /// mlt-core's message.
        message: String,
    },
}

#[doc(hidden)]
impl From<arrow_schema::ArrowError> for ExportError {
    fn from(e: arrow_schema::ArrowError) -> Self {
        ExportError::Arrow(e)
    }
}

impl crate::error::Classify for ExportError {
    /// [`ErrorKind::Usage`](crate::error::ErrorKind::Usage) is an option
    /// refused before the overview file is read ([`ExportOptions::validate`],
    /// #779); a property selection the file cannot satisfy is
    /// [`ErrorKind::Input`](crate::error::ErrorKind::Input).
    fn kind(&self) -> crate::error::ErrorKind {
        use crate::error::ErrorKind;
        match self {
            ExportError::Config(_) | ExportError::DeclaredMinZoomAboveCeiling { .. } => {
                ErrorKind::Usage
            }
            ExportError::UnknownProperty { .. }
            | ExportError::UnsupportedProperty { .. }
            | ExportError::CoveringProperty { .. }
            | ExportError::PropertyRequiredByKnob { .. }
            | ExportError::VariableDepthColumn { .. } => ErrorKind::Input,
            _ => crate::error::fallback_kind(self),
        }
    }
}

/// One source feature: its (possibly reprojected-to-4326) geometry and the MVT
/// properties carried over from the overview file (level column + covering
/// struct excluded). Test-support only: production streams members directly
/// instead of materializing features (see [`finish_group`]).
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
    /// MVT features in the tile. `0` means every member encoded to nothing
    /// (#431): the carrier exists only to deliver `encode_dropped` and is not
    /// written to the archive.
    feature_count: usize,
    /// Members with nothing to encode (#431); see [`TileMvt::unencodable`].
    encode_dropped: usize,
    /// Members that collapsed at the tile extent (#431); see
    /// [`TileMvt::quantized`].
    encode_quantized: usize,
    oversized: bool,
    /// What the size valve did to the tile (#745).
    valve: ValveTally,
    /// The tile's coverage fill (#750).
    fill: FillTally,
}

/// Arrow batch size for the partition read passes (the parquet reader default
/// of 1024 rows makes the per-batch parallel clip sections too small to load-
/// balance across cores; 8k rows amortizes decode and barrier overhead while
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
///
/// # Errors
///
/// Returns an [`ExportError`] if the options fail [`ExportOptions::validate`]
/// or conflict with the file, if the input is not a readable overview file or
/// has an unsupported CRS, if a level's zoom cannot be resolved, if a feature-
/// id column is unusable, if the spill directory is missing, or if reading,
/// encoding, or writing fails.
pub fn export_pmtiles(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &ExportOptions,
) -> Result<ExportReport, ExportError> {
    export_pmtiles_with_partition_target(input_path, output_path, options, DEFAULT_PARTITION_TARGET)
}

/// Intermediate archives run their ordinary valves; only over-limit tile
/// reporting at shared zooms belongs to the merge.
pub(crate) fn export_pmtiles_for_join(
    input: &Path,
    output: &Path,
    options: &ExportOptions,
    shared_zooms: &[u8],
) -> Result<(ExportReport, JointMembers), ExportError> {
    let config = options.validate()?;
    export_pmtiles_validated(
        input,
        output,
        &config,
        DEFAULT_PARTITION_TARGET,
        &Pass2Knobs::production(false, None),
        &mut export_budget(),
        shared_zooms,
    )
    .map(|run| (run.report, run.joint_members))
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

/// Drop every scanned tile this export does not own (#498).
///
/// Two independent restrictions, both no-ops when unset:
///
/// * [`ExportOptions::zoom_ceiling`] — the coarse job keeps zooms `<= ceiling`.
/// * [`ExportOptions::tile_range`] — a data shard keeps, at each zoom from the
///   pivot down, the tile ids the range owns. Zooms coarser than the pivot are
///   outside every range and are dropped wholesale, which is precisely the
///   half the coarse job keeps: the two restrictions are complements, so a
///   coarse job plus N shards emit each tile exactly once.
///
/// A level's `bounds` are narrowed with its tiles. Bounds are the archive's
/// advertised extent, and unioning N shards' full-dataset extents would
/// advertise every shard as covering the whole world. Intersecting the data
/// extent with the range's tile extent instead keeps each shard honest while
/// still unioning back to exactly the monolithic bbox, because the ranges
/// cover the pivot zoom completely.
fn restrict_scans_to_range(scans: &mut [LevelScan], level_zooms: &[u8], options: &ExportOptions) {
    if options.tile_range.is_none() && options.zoom_ceiling.is_none() {
        return;
    }
    let range_bounds = options.tile_range.as_ref().map(|r| r.tile_bounds());
    let mut kept = 0usize;
    let mut dropped = 0usize;
    for (scan, &zoom) in scans.iter_mut().zip(level_zooms) {
        let ids = match options.tile_range.as_ref() {
            Some(r) => r.ids_at(zoom),
            None => None,
        };
        let above_ceiling = options.zoom_ceiling.is_some_and(|c| zoom > c);
        let before = scan.tile_counts.len();
        if above_ceiling || (options.tile_range.is_some() && ids.is_none()) {
            scan.tile_counts.clear();
        } else if let Some(ids) = ids {
            scan.tile_counts.retain(|key, _| ids.contains(key));
        }
        dropped += before - scan.tile_counts.len();
        kept += scan.tile_counts.len();
        if scan.tile_counts.is_empty() {
            scan.bounds = None;
        } else if let (Some(b), Some(rb)) = (scan.bounds.as_mut(), range_bounds.as_ref()) {
            *b = b.intersect(rb);
        }
    }
    log::info!(
        "[export] shard restriction: keeping {kept} tile(s), dropping {dropped} that belong to \
         another job{}",
        options
            .tile_range
            .as_ref()
            .map_or(String::new(), |r| format!(" (range {r})"))
    );
}

/// Write a valid, **tile-less** PMTiles archive (#498).
///
/// A sharded build cuts N ranges that tile the pivot zoom with no gap, so a
/// dataset concentrated in one corner of the world leaves some of those
/// ranges owning no input rows at all. That is a legal outcome of a legal
/// cut, and the job that draws such a range has to produce something: an
/// array job whose "correct" outcome is a failed task, and a merge whose
/// input list the operator has to hand-edit, is the alternative.
///
/// The archive is byte-valid and simply holds nothing: same tile type, same
/// compression and the same declared layer as a real export of the same
/// fleet, so `tylertoo merge` accepts it in the same list as its siblings
/// (and, seeing no tiles, excludes it from the merged zoom range, bounds and
/// layer declarations rather than letting an empty job widen them).
///
/// `min_zoom`/`max_zoom` are what the archive DECLARES — for a data shard,
/// its own `[pivot, max_zoom]` half. Bounds are deliberately left unset: an
/// archive with no tiles has no extent, and claiming its range's extent would
/// widen the merged bbox on the word of a job that contributed nothing.
///
/// `metadata` is the publication metadata the job's export would have
/// written: an empty shard or coarse job carries the same `name`,
/// `description` and `attribution` as its siblings, so the merged archive
/// keeps them whether or not this job received any data.
///
/// # Errors
///
/// Returns an [`ExportError`] if `metadata` is too large for an archive's
/// metadata (checked before `output` is touched), or if the archive cannot
/// be created or written at `output`.
pub fn write_empty_archive(
    output: &Path,
    layer_name: &str,
    min_zoom: u8,
    max_zoom: u8,
    metadata: &TilesetMetadata,
    tile_format: TileFormat,
) -> Result<(), ExportError> {
    check_metadata_size(metadata)?;
    let mut writer = open_writer(output, layer_name, metadata, min_zoom, tile_format)?;
    writer.set_declared_max_zoom(max_zoom);
    writer.finalize(output)?;
    Ok(())
}

/// The writer every export archive starts from, empty or not: gzip tiles in
/// the tail layout, with the layer name, metadata and declared min zoom set
/// before the first tile, so every checkpoint carries them too.
///
/// #459: the tail layout appends tile bytes straight into
/// `<output>.partial` after a reserved 16 KiB prefix and never copies them
/// again, so a per-level checkpoint rewrites only the header, root
/// directory, metadata and leaves — O(directory), not O(tile data). The
/// spool lives beside the output rather than in `TMPDIR`. Gzip because
/// `merge_shards` refuses inputs whose tile compression disagrees, so an
/// empty shard written any other way would break the very merge it exists
/// to keep working.
fn open_writer(
    output: &Path,
    layer_name: &str,
    metadata: &TilesetMetadata,
    min_zoom: u8,
    tile_format: TileFormat,
) -> Result<StreamingPmtilesWriter, ExportError> {
    let mut writer = StreamingPmtilesWriter::with_tail_layout(output, Compression::Gzip)?;
    writer.set_tile_type(tile_format.tile_type());
    writer.set_layer_name(layer_name);
    writer.set_metadata(metadata.clone());
    writer.set_declared_min_zoom(min_zoom);
    Ok(writer)
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
    export_pmtiles_run(
        input_path,
        output_path,
        options,
        partition_target,
        &Pass2Knobs::production(force_legacy_pass2, backing_override),
    )
    .map(|run| run.report)
}

impl Pass2Knobs {
    fn production(force_legacy_pass2: bool, backing_override: Option<SinkBacking>) -> Self {
        Self {
            force_legacy_pass2,
            backing_override,
            concurrent_levels: true,
            checkpoint_interval: CHECKPOINT_INTERVAL,
            fail_level: None,
            #[cfg(test)]
            panic_write_level: None,
            #[cfg(test)]
            hold_for_fill: false,
            #[cfg(test)]
            stall_first_wave: false,
            memory_limit: None,
            budget_available: None,
        }
    }
}

/// The pass-2 test knobs [`export_pmtiles_impl`] forwards, plus #647's.
/// Production runs `concurrent_levels: true`, [`CHECKPOINT_INTERVAL`] and
/// no `fail_level`.
struct Pass2Knobs {
    force_legacy_pass2: bool,
    backing_override: Option<SinkBacking>,
    /// `false` pins the strictly serial level loop, the byte-identity
    /// oracle for the concurrent level groups.
    concurrent_levels: bool,
    /// The checkpoint throttle. Tests set zero, so a checkpoint is due after
    /// every level.
    checkpoint_interval: Duration,
    /// Fail the export with an injected I/O error when this level starts,
    /// to test what an interrupted group leaves behind.
    fail_level: Option<usize>,
    /// Panic while a level group writes this level's first wave, to test
    /// that a panicking write fails the group instead of hanging it.
    #[cfg(test)]
    panic_write_level: Option<usize>,
    /// Hold each level's last wave in a level group, before it encodes,
    /// until the next level's own store starts to fill or 2 s pass, to
    /// test that the fill does not wait for the encode
    /// ([`ExportWorkTally::fills_beside_encode`]).
    #[cfg(test)]
    hold_for_fill: bool,
    /// Hold each level group's first wave, before it encodes, until every
    /// later wave of the group waits to be written or 2 s pass, to test what
    /// a stalled coarse wave lets queue behind it
    /// ([`ExportWorkTally::peak_queued_waves`]).
    #[cfg(test)]
    stall_first_wave: bool,
    /// The memory figure for the #665 dense-tile refusal. `None` probes
    /// ([`probe_oom_memory_limit`]); tests inject one.
    memory_limit: Option<MemoryLimit>,
    /// The probed figure the run's [`MemoryBudget`] starts from. `None`
    /// probes ([`export_budget`]); tests inject one.
    budget_available: Option<u64>,
}

/// What [`export_pmtiles_run`] returns: the report, plus the level schedule
/// and checkpoints the tests check.
struct Pass2Run {
    report: ExportReport,
    joint_members: JointMembers,
    /// The [`plan_level_groups`] schedule the levels ran in.
    #[cfg(test)]
    level_groups: Vec<std::ops::Range<usize>>,
    /// The zoom each checkpoint was capped at, in order.
    #[cfg(test)]
    checkpoint_zooms: Vec<u8>,
    /// Whether the levels read from the single-read member store (#235).
    #[cfg(test)]
    single_read: bool,
    /// The single-read store's backing, when it ran.
    #[cfg(test)]
    store_backing: Option<SinkBacking>,
    /// The resolved partition-wave ceiling.
    #[cfg(test)]
    ceiling_wave: usize,
    /// Each planned level's wave starts (#876).
    #[cfg(test)]
    wave_starts: Vec<Vec<usize>>,
    /// The export's work counts (#788).
    #[cfg(test)]
    work: ExportWorkTally,
    /// The per-zoom profile entries (#810).
    #[cfg(test)]
    per_zoom: Vec<ExportZoomProfile>,
}

fn export_pmtiles_run(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &ExportOptions,
    partition_target: usize,
    knobs: &Pass2Knobs,
) -> Result<Pass2Run, ExportError> {
    let validated = options.validate()?;
    let mut budget = knobs
        .budget_available
        .map_or_else(export_budget, |b| MemoryBudget::with_available(Some(b)));
    export_pmtiles_validated(
        input_path,
        output_path,
        &validated,
        partition_target,
        knobs,
        &mut budget,
        &[],
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "#779: export orchestration keeps scan, writer and checkpoint lifetimes explicit"
)]
fn export_pmtiles_validated(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    config: &super::config::ValidatedConfig<'_, ExportOptions>,
    partition_target: usize,
    knobs: &Pass2Knobs,
    budget: &mut MemoryBudget,
    shared_zooms: &[u8],
) -> Result<Pass2Run, ExportError> {
    let diagnostics = &crate::diagnostics::Diagnostics::default();
    super::pipeline::memory_diagnostic(diagnostics);
    diagnostics.set_python_loss_budget(budget.python_loss_allowance());
    let options = config.options();
    diagnostics.set_accounting_spill_dir(options.spill_dir.as_deref());
    // #535 step 1 / #517 S2: probe `TYLERTOO_PROFILE_JSON` writability loudly,
    // for the standalone `export-pmtiles` path (which never runs convert's
    // own preflight); redundant but harmless in the `tiles` path.
    preflight_profile_json_path(diagnostics);
    // #433: the knobs that need no file (`extent`, `tile_buffer`) are checked
    // before any I/O, so a bad value fails here rather than after the open.
    // #452: and the output, before the input is opened.
    crate::atomic_output::check_writable(output_path.as_ref())?;
    let start = Instant::now();
    let input_path = input_path.as_ref();

    let reader = OverviewReader::open(input_path)?;
    let meta = reader.meta().clone();
    if options.variable_depth {
        variable_depth::check_file(&meta, reader.mode())?;
    }
    let crs = crs_from_descriptor(reader.resolved_schema(), input_path, diagnostics)?;

    let num_levels = reader.num_levels();
    // #371: every level's zoom is resolved (and ceiling-checked) before any
    // scan runs — a level whose GSD implies a zoom past `MAX_ZOOM` fails here
    // rather than producing an archive with wrapped tile ids.
    //
    // #506 follow-up: `OverviewsMeta::validate` (run by `OverviewReader::open`
    // above) only requires `gsd` to be strictly decreasing across levels, not
    // that the ZOOM each one derives to is distinct — two GSDs close enough
    // to round to the same integer zoom both pass that check. Resolve every
    // level's zoom here and require strict ascent before any scan runs; see
    // [`ExportError::LevelZoomsNotAscending`] for what a collision used to do
    // silently (pre-#506) and what it does now if left unrejected.
    let level_zooms = resolve_level_zooms(&meta, num_levels)?;
    // #498: the zooms this export will actually emit, which is what #380's
    // declared-minimum rules must be measured against. A data shard starts at
    // the pivot however coarse the overview file is (the coarser levels belong
    // to the coarse job), and the coarse job stops at the ceiling.
    let coarsest_zoom = match options.tile_range.as_ref() {
        Some(r) => level_zooms[0].max(r.pivot_zoom()),
        None => level_zooms[0],
    };
    let max_zoom = emitted_max_zoom(&meta, options, level_zooms[num_levels - 1])?;
    if coarsest_zoom > max_zoom {
        return Err(ExportError::RestrictionEmitsNoZoom {
            coarsest: coarsest_zoom,
            finest: max_zoom,
        });
    }
    // #498: the read-pruning safety bound. A shard's convert prunes input row
    // groups against its tile range widened by `SHARD_READ_MARGIN_TILES`
    // pivot tiles; a buffer wider than that margin would let a feature render
    // into a shard's tile from a row group the shard never read. Since #433
    // the universal `MAX_TILE_BUFFER_PX` cap (checked in `validate` above) is
    // at most that margin, so no buffer that reaches here can breach it; the
    // `const` assertion beside `MAX_SHARD_TILE_BUFFER_PX` keeps that true.
    debug_assert!(options.tile_buffer <= crate::shard::MAX_SHARD_TILE_BUFFER_PX);
    // #380: the archive may declare a coarser minimum than the file holds.
    let min_zoom = match options.min_zoom {
        // #371: the declared minimum is written into `vector_layers`, so it
        // obeys the same ceiling as everything else. Checked before the #380 comparison
        // so an out-of-range value is named as such.
        Some(declared) if declared > MAX_ZOOM => {
            return Err(ExportError::DeclaredMinZoomAboveCeiling {
                declared,
                ceiling: MAX_ZOOM,
            });
        }
        Some(declared) if declared > coarsest_zoom => {
            return Err(ExportError::DeclaredMinZoomTooFine {
                declared,
                coarsest: coarsest_zoom,
            });
        }
        Some(declared) if declared < coarsest_zoom => {
            log::info!(
                "[export] widening the declared zoom range to z{declared}..z{max_zoom}; \
                 the file's coarsest level is z{coarsest_zoom}, so z{declared}..z{} hold \
                 no tiles",
                coarsest_zoom - 1
            );
            declared
        }
        _ => coarsest_zoom,
    };

    // Resolve the partition-wave *ceiling* once (auto-sizes from available cores
    // when the caller left it at PARTITION_WAVE_AUTO) and surface it (#293). On
    // `auto` this is only an upper bound: each level narrows the waves that
    // hold dense partitions (#311, #876) — see `memory_safe_wave_members` and
    // `cut_waves` below. An explicit `--partition-wave N` is honoured verbatim and never
    // narrowed (the caller opted into the memory cost).
    let ceiling_wave =
        resolve_and_log_partition_wave(options.partition_wave, options.profile, budget);
    let auto_wave = options.partition_wave == PARTITION_WAVE_AUTO;
    // Data-aware per-member size signal for the #311 guard: the finest level's
    // mean uncompressed row bytes, read once from the Parquet metadata. Every
    // level sizes against the run's one `budget`.
    let mean_member_bytes = reader.finest_level_mean_row_bytes();

    let ResolvedColumns {
        published,
        feature_id,
        skipped: skipped_property_columns,
    } = resolve_export_columns(
        (
            &reader,
            BandRead {
                source: reader.source(),
                budget: *budget,
                profile: options.profile,
            },
        ),
        options,
        &level_zooms,
    )?;
    let fit_member_bytes = export_member_bytes(&reader, &published, mean_member_bytes);
    let member_bytes = fit_member_bytes.map(|bytes| export_member_resident_bytes(bytes, options));

    // Writer: field metadata + layer name are derived from the first level's
    // schema (property columns, level/covering excluded).
    let mut writer = open_writer(
        output_path.as_ref(),
        &options.layer_name,
        &options.metadata,
        min_zoom,
        options.tile_format,
    )?;
    if let Some(&canonical) = level_zooms.last().filter(|_| options.variable_depth) {
        writer.set_variable_depth(canonical);
    }
    // #506: export now adds every tile in ascending PMTiles tile-id (Hilbert)
    // order — per zoom via `tile_key`, and zooms ascend across levels — so
    // the archive is genuinely clustered. Debug-only: catches an ordering
    // regression here rather than downstream as a quietly `clustered: false`
    // archive.
    writer.set_expect_clustered(true);
    // #361: a `--feature-order` column that names nothing is a no-op, and an
    // indistinguishable one — every member's key is `Missing`, so they all
    // compare equal and the stable sort leaves input order. A typo therefore
    // looks exactly like success.
    //
    // Checked against the PUBLISHED names (#359), which is what makes
    // `--feature-order level` work on an input whose own `level` column was
    // renamed to clear the reserved one: the source name is restored on
    // export, so that is the key the tile carries and the key to sort on.
    warn_if_order_column_is_absent(reader.schema(), &published, &options.feature_order);
    writer.set_fields(field_metadata(
        reader.schema(),
        reader.resolved_schema().geometry_index(),
        &published,
    ));

    // Pass 1 (scan), **all levels in one streaming read of the file** (issue
    // #233). Scanning each level independently re-reads and re-decodes every
    // coarse row group from every finer level's accumulating prefix (§5.1) —
    // `Σ_k |prefix_k|` decodes. Instead read each band once and fan its
    // features to the tile-size accumulators of every level whose prefix
    // includes it. O(#tiles) memory per level; the per-level `LevelScan`s are
    // byte-identical to independent per-level scans.
    // #571: the continuous RSS sampler starts HERE, with the scan it is
    // meant to watch, iff profiling is on (no thread otherwise). It is handed
    // to `ExportProgress` below; an early `?` return drops it, which stops
    // the thread.
    let rss_sampler = RssSampler::start_if(profile_json_target().is_some(), "scan");
    let t_scan = Instant::now();
    let timers = Arc::<ExportTimers>::default();
    // #803: rule A′ is gathered in the same read.
    let mut vd_scan = options
        .variable_depth
        .then(|| {
            variable_depth::VdScan::new(&meta, reader.geo_metadata_json(), level_zooms.clone())
        })
        .transpose()?;
    let mut scans = scan_all_levels(
        &reader,
        BandRead {
            source: reader.source(),
            budget: *budget,
            profile: options.profile,
        },
        crs,
        options,
        &timers,
        vd_scan.as_mut(),
        diagnostics,
    )?;

    // #498: a sharded build's export owns a slice of the tile space, not all
    // of it. Restricting the SCAN — the per-zoom tile-count map every
    // partition, key window and wave-read bbox is derived from — is what makes
    // the restriction free rather than a filter bolted on at write time: an
    // out-of-range tile is never planned, so it is never clipped, encoded,
    // hashed or read for.
    restrict_scans_to_range(&mut scans, &level_zooms, options);
    let pins = finest_level_pins(
        (
            &reader,
            BandRead {
                source: reader.source(),
                budget: *budget,
                profile: options.profile,
            },
        ),
        crs,
        options,
        budget,
        diagnostics,
    )?;
    // #803: rule A′'s blocked tiles, once every band is counted.
    let vd_plan = vd_scan.map(|vd| vd.finish(&scans, options));

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

    let mut zooms: Vec<ZoomReport> = Vec::with_capacity(num_levels);
    // #535 step 1: stage timers + per-zoom profile -> a second
    // `TYLERTOO_PROFILE_JSON` line at the end ([`emit_export_profile_json`]).
    // #571: `ExportProgress` also owns the continuous RSS sampler started
    // before the scan (`None` when profiling is off).
    let mut progress = ExportProgress::new(num_levels, timers, rss_sampler, knobs);

    // Plan every level's partitions and wave width up front. The partitioning
    // single-read fill (#235) routes members by (level, partition, wave), so
    // the drain loop below must consume the exact same plan it was filled
    // against; hoisting the planning out of the level loop guarantees that.
    let partition_budget = PartitionBudget {
        partition_target,
        ceiling_wave,
        auto_wave,
        member_bytes,
        fit_member_bytes,
        profile: options.profile,
        budget: *budget,
        fit_limit: knobs.memory_limit.or_else(probe_oom_memory_limit),
    };
    // #803: the levels coarser than every candidate tile cannot change, so
    // they are planned now and drawn while the probe runs; the rest are
    // planned one by one as the probe releases them.
    let vd = vd_plan.map(|plan| {
        let first = plan.first_probed_level(options);
        (plan, first)
    });
    let planned = vd.as_ref().map_or(num_levels, |(_, first)| *first);
    let plans = plan_levels(
        &scans[..planned],
        &level_zooms[..planned],
        &partition_budget,
    )?;
    #[cfg(test)]
    let wave_starts: Vec<Vec<usize>> = plans.iter().map(|p| p.wave_starts.clone()).collect();
    progress.end_scan(t_scan);

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
    // #647: consecutive small levels run as one concurrent group. On `auto`
    // the cap sizes from the same RAM budget as the #311 per-level guard.
    // An explicit wave sizes the cap from its width alone, as #311 does.
    // `concurrent_levels: false` (cap 0) is the serial test oracle. Sized
    // before the store's grant below, like every other plan decision.
    let group_cap = level_group_member_cap(
        ceiling_wave,
        partition_target,
        member_bytes.filter(|_| auto_wave),
        options.profile,
        auto_wave.then_some(&*budget),
    );
    let group_cap = if knobs.concurrent_levels {
        group_cap
    } else {
        0
    };
    let single_read = matches!(reader.mode(), Mode::Partitioning) && !knobs.force_legacy_pass2;
    // The not-yet-flushed buffer of whichever member store runs: the
    // export-wide one (#235), or one multi-wave level's at a time (#740).
    // A fixed 128 MiB under every profile (want = floor): the budget makes
    // no decision here, it only charges the buffer so later grants see it.
    // A filling store narrows it from measured members (#788 step 3).
    let store_grant = budget.fit(
        options.profile,
        MemoryPhase::ExportCollect,
        Price::model(MEMBER_STORE_RAM_BUDGET as u64),
        1,
        1,
    );
    let store_ram = usize::try_from(store_grant.bytes()).unwrap_or(usize::MAX);
    // #733: the tile-gzip cache's payload, granted once before any tile is
    // encoded, in slices of one shard's share of the 8 MiB the Madagascar
    // repeats were measured to need. A tighter budget gets fewer slices,
    // down to one.
    let gzip_grant = budget.fit(
        options.profile,
        MemoryPhase::ExportEncode,
        Price::new(
            crate::compression::tile_gzip::PAYLOAD_SLICE_BYTES as u64,
            PriceBasis::Measured,
        ),
        crate::compression::tile_gzip::PAYLOAD_SLICES,
        // Main's cache under every profile: bounded keeps main's caps.
        crate::compression::tile_gzip::PAYLOAD_SLICES,
    );
    let gzip_cache = Arc::new(TileGzipCache::new(
        usize::try_from(gzip_grant.bytes()).unwrap_or(usize::MAX),
    ));
    let mut store = if single_read {
        let backing = knobs.backing_override.unwrap_or_else(|| {
            fill_backing(
                &scans,
                num_levels,
                options.profile,
                budget,
                mean_member_bytes,
            )
        });
        // #571: entered only when the fill runs, so a duplicating-mode export
        // (no fill) has no `fill` entry in `phase_peaks_mib`.
        progress.enter_phase("fill");
        let t_fill = Instant::now();
        let store = fill_member_store(
            &reader,
            &FanoutCtx {
                // shortcut: like the levels' band reads below, the fill's
                // read-ahead (at most `BAND_READ_AHEAD_BATCHES` per reader)
                // is not charged to the budget; charge a `grant_read_ahead`
                // if a profile shows the fill's peak RSS growing.
                band: BandRead {
                    source: reader.source(),
                    budget: *budget,
                    profile: options.profile,
                },
                crs,
                plans: &plans,
                opts: options,
                published: &published,
                feature_id: feature_id.as_ref(),
                timers: &progress.profile.timers,
                pins: pins.as_ref(),
            },
            StoreBacking {
                backing,
                ram_budget: store_ram,
                resident: 0,
            },
            diagnostics,
        )?;
        progress.end_fill(t_fill);
        Some(store)
    } else {
        None
    };
    #[cfg(test)]
    let store_backing = store.as_ref().map(|s| {
        if s.spill.is_some() {
            SinkBacking::Spill
        } else {
            SinkBacking::Ram
        }
    });
    // #571: the per-level wave loop — the analogous phase to convert's
    // pass-2 output sink, and where a single dense level's clip/encode
    // buffers can peak between the (rare) checkpoint boundaries.
    progress.enter_phase("levels");

    let t_levels = Instant::now();
    // #740: without the export-wide store, a level split into several waves
    // reads its band once into a one-level store instead of once per wave.
    // The store holds every wave of the level (up to 5.7 waves' members on
    // NZ z14) where the wave sizing budgets for one, so its backing is the
    // budget's: in RAM when the level fits, otherwise spilling what the
    // grant does not cover ([`level_read_backing`]). The forced legacy read
    // keeps the per-wave reads (the tests' oracle).
    let level_read = (store.is_none() && !knobs.force_legacy_pass2).then_some(LevelRead {
        forced: knobs.backing_override,
        ram_budget: store_ram,
        member_bytes,
        profile: options.profile,
    });
    #[cfg(test)]
    let work = Mutex::new(ExportWorkTally {
        hold_for_fill: knobs.hold_for_fill,
        stall_first_wave: knobs.stall_first_wave,
        ..ExportWorkTally::default()
    });
    let (head, rest) = scans.split_at_mut(planned);
    let unplanned = (
        LevelScan {
            feature_count: 0,
            bounds: None,
            tile_counts: BTreeMap::new(),
        },
        LevelPlan {
            zoom: 0,
            partitions: Vec::new(),
            wave_starts: Vec::new(),
            narrowed: false,
        },
    );
    let template = ExportLevelCtx {
        reader: &reader,
        band: BandRead {
            source: reader.source(),
            // shortcut: the read-ahead is sized from the budget as the levels
            // start and is not charged to it (the old one-thread pipe was not
            // either); charge a `grant_read_ahead` per band read if a profile
            // shows the levels' peak RSS growing.
            budget: *budget,
            profile: options.profile,
        },
        scan: &unplanned.0,
        plan: &unplanned.1,
        level_idx: 0,
        num_levels,
        crs,
        published: &published,
        feature_id: feature_id.as_ref(),
        options,
        gzip_cache: &gzip_cache,
        start,
        fills: None,
        terminals: &[],
        level_read,
        member_bytes,
        pins: pins.as_ref(),
        #[cfg(test)]
        work: &work,
    };
    let levels: Vec<ExportLevelCtx<'_>> = plans
        .iter()
        .zip(head.iter())
        .enumerate()
        .map(|(level_idx, (plan, scan))| ExportLevelCtx {
            scan,
            plan,
            level_idx,
            ..template
        })
        .collect();
    let (level_groups, gate) = group_schedule(group_cap, &plans, partition_target, options.profile);
    let mem = LevelMemory {
        store: store.as_mut(),
        budget: &mut *budget,
    };
    let ckpt = CheckpointCtx {
        output_path: output_path.as_ref(),
        coarsest_zoom,
    };
    let probe_timers = Arc::clone(&progress.profile.timers);
    #[cfg_attr(not(test), expect(unused_variables, reason = "the tests' schedule"))]
    let (reports, level_groups, probed) = match vd {
        None => {
            let reports = export_level_groups(
                &mut writer,
                mem,
                &levels,
                (&level_groups, gate),
                &ckpt,
                &mut progress,
                diagnostics,
            )?;
            (reports, level_groups, None)
        }
        Some((plan, _)) => {
            let (reports, groups, probed) = variable_depth::export_levels(
                &mut writer,
                mem,
                &ckpt,
                &mut progress,
                diagnostics,
                variable_depth::VdLevels {
                    plan,
                    probe: variable_depth::Probe {
                        reader: &reader,
                        band: template.band,
                        crs,
                        published: &published,
                        feature_id: feature_id.as_ref(),
                        options,
                        budget: &partition_budget,
                        timers: &probe_timers,
                        pins: pins.as_ref(),
                    },
                    planned: (&levels, level_groups, gate),
                    rest,
                    zooms: &level_zooms,
                    template,
                    group_cap,
                },
            )?;
            (reports, groups, Some(probed))
        }
    };
    zooms.extend(reports);

    budget.release(store_grant);
    budget.release(gzip_grant);
    progress.profile.walls.levels = t_levels.elapsed().as_secs_f64();
    progress.enter_phase("finalize");

    let t_finalize = Instant::now();
    writer.finalize(output_path.as_ref())?;
    progress.end_finalize(t_finalize);
    log::info!(
        "[export] finalize complete: {:.2}s (total {:.0}s)",
        t_finalize.elapsed().as_secs_f64(),
        start.elapsed().as_secs_f64(),
    );
    // #571: stop the sampler (a no-op if profiling was off) and fold its
    // findings into the profile `emit` is about to write.
    progress.finish_rss_sampler();

    let mode = format!("{:?}", reader.mode()).to_lowercase();
    progress.profile.emit(
        &ExportProfileTarget {
            output: output_path.as_ref(),
            layer_name: &options.layer_name,
            mode: &mode,
            partition_wave_width: ceiling_wave,
            start,
        },
        diagnostics,
    );

    #[cfg(test)]
    let work = work.lock().unwrap_or_else(PoisonError::into_inner).clone();
    for zoom in &mut zooms {
        if shared_zooms.contains(&zoom.zoom) {
            zoom.over_limit_tiles = 0;
            zoom.over_limit_largest.clear();
            zoom.over_limit_merged = 0;
        }
    }
    Ok(Pass2Run {
        // Move the existing scan counts, retaining only zooms the join can
        // retry. No additional overview scan or count-map copy is needed.
        joint_members: JointMembers {
            member_bytes: fit_member_bytes,
            tile_counts: scans
                .into_iter()
                .zip(&level_zooms)
                .filter(|(_, z)| {
                    shared_zooms.contains(z)
                        && coarsest_zoom <= **z
                        && **z <= max_zoom
                        && !options.coverage_fill
                        && !options.variable_depth
                })
                .flat_map(|(scan, _)| scan.tile_counts)
                .collect(),
        },
        report: ExportReport::from_zooms(
            mode,
            (min_zoom, max_zoom),
            zooms,
            skipped_property_columns,
            start,
        )
        .with_variable_depth(probed)
        .with_diagnostics(diagnostics)?,
        #[cfg(test)]
        level_groups,
        #[cfg(test)]
        checkpoint_zooms: progress.checkpoint_zooms,
        #[cfg(test)]
        single_read,
        #[cfg(test)]
        store_backing,
        #[cfg(test)]
        ceiling_wave,
        #[cfg(test)]
        wave_starts,
        #[cfg(test)]
        work,
        #[cfg(test)]
        per_zoom: progress.profile.per_zoom.clone(),
    })
}

impl ExportReport {
    fn with_diagnostics(
        mut self,
        sink: &crate::diagnostics::Diagnostics,
    ) -> Result<Self, ExportError> {
        use crate::diagnostics::Diagnostic;
        sink.record(Diagnostic::EncodeDropped {
            count: self.encode_dropped_features,
        });
        sink.record(Diagnostic::EncodeQuantized {
            count: self.encode_quantized_features,
        });
        sink.record(Diagnostic::Merged {
            count: self.merged_features,
        });
        sink.record(Diagnostic::ValveDropped {
            count: self.valve_dropped_features,
        });
        sink.record(Diagnostic::MergedDropped {
            count: self.zooms.iter().map(|z| z.dropped_merged_features).sum(),
            sources: self.zooms.iter().map(|z| z.dropped_merged_sources).sum(),
        });
        for z in &self.zooms {
            if let Some(min_extent) = z.min_reduced_extent {
                sink.record(Diagnostic::ExtentReduced {
                    count: z.reduced_extent_tiles,
                    zoom: z.zoom,
                    min_extent,
                });
            }
        }
        let mut largest = Vec::new();
        let tiles = self.zooms.iter().flat_map(|z| &z.over_limit_largest);
        crate::stats::keep_largest(&mut largest, tiles.copied());
        sink.record(Diagnostic::TilesOverLimit {
            count: self.over_limit_tiles,
            largest,
            merged_tiles: self.zooms.iter().map(|z| z.over_limit_merged).sum(),
        });
        for column in &self.skipped_property_columns {
            sink.once(Diagnostic::PropertySkipped {
                count: 1,
                name: column.name.clone(),
                data_type: column.data_type.clone(),
            });
        }
        self.diagnostics = sink.snapshot();
        self.source_losses = sink.source_losses()?;
        self.encode_dropped_features = sink.count(&Diagnostic::EncodeDropped { count: 0 });
        self.encode_quantized_features = sink.count(&Diagnostic::EncodeQuantized { count: 0 });
        self.merged_features = sink.count(&Diagnostic::Merged { count: 0 });
        self.valve_dropped_features = sink.count(&Diagnostic::ValveDropped { count: 0 });
        self.over_limit_tiles = sink.count(&Diagnostic::TilesOverLimit {
            count: 0,
            largest: Vec::new(),
            merged_tiles: 0,
        });
        Ok(self)
    }

    /// The report over the written zooms, with the totals summed from them.
    /// Pulled out of `export_pmtiles_run` to keep it under clippy's
    /// function-length ceiling.
    fn from_zooms(
        mode: String,
        (min_zoom, max_zoom): (u8, u8),
        zooms: Vec<ZoomReport>,
        skipped_property_columns: Vec<SkippedPropertyColumn>,
        start: Instant,
    ) -> Self {
        Self {
            diagnostics: Vec::new(),
            source_losses: Default::default(),
            mode,
            min_zoom,
            max_zoom,
            total_tiles: zooms.iter().map(|z| z.tile_count).sum(),
            total_tile_features: zooms.iter().map(|z| z.tile_feature_count).sum(),
            oversized_tiles: zooms.iter().map(|z| z.oversized_tiles).sum(),
            over_limit_tiles: zooms.iter().map(|z| z.over_limit_tiles).sum(),
            skipped_property_columns,
            encode_dropped_features: zooms.iter().map(|z| z.encode_dropped_features).sum(),
            encode_quantized_features: zooms.iter().map(|z| z.encode_quantized_features).sum(),
            merged_features: zooms.iter().map(|z| z.merged_features).sum(),
            valve_dropped_features: zooms.iter().map(|z| z.valve_dropped_features).sum(),
            zooms,
            duration_secs: start.elapsed().as_secs_f64(),
            variable_depth: None,
        }
    }

    /// Add the #803 summary when `probed` (the probe's tile count) says the
    /// export had variable depth.
    fn with_variable_depth(mut self, probed: Option<usize>) -> Self {
        self.variable_depth = probed.map(|probed_tiles| VariableDepthReport {
            canonical_max_zoom: self.max_zoom,
            stored_max_zoom: self
                .zooms
                .iter()
                .rev()
                .find(|z| z.tile_count > 0)
                .map(|z| z.zoom),
            probed_tiles,
            stopped_tiles: self.zooms.iter().map(|z| z.stopped_tiles).sum(),
        });
        self
    }
}

/// One member's estimated resident size (#665): [`member_resident_bytes`]
/// over the finest level's mean stored row size and the properties every
/// member carries. The [`check_levels_fit`] refusal uses this price; wave,
/// group and store fits add merge-valve workspace through
/// [`export_member_resident_bytes`]. Neither affects output bytes.
/// Pulled out of `export_pmtiles_run` to keep it
/// under clippy's function-length ceiling.
fn export_member_bytes(
    reader: &OverviewReader,
    published: &PublishedNames,
    mean_stored_bytes: Option<u64>,
) -> Option<u64> {
    let properties = reader
        .resolved_schema()
        .geometry_index()
        .map_or(0, |g| property_columns(reader.schema(), g, published).len());
    mean_stored_bytes.map(|mean| member_resident_bytes(mean, properties))
}

/// Every level's zoom, in level order, required to strictly ascend (#371 /
/// #506 follow-up — see the call site in `export_pmtiles_impl`). Pulled out
/// of it to keep that function under clippy's function-length ceiling.
fn resolve_level_zooms(meta: &OverviewsMeta, num_levels: usize) -> Result<Vec<u8>, ExportError> {
    let level_zooms: Vec<u8> = (0..num_levels)
        .map(|level_idx| zoom_for_level(meta, level_idx))
        .collect::<Result<_, _>>()?;
    for i in 1..level_zooms.len() {
        if level_zooms[i] <= level_zooms[i - 1] {
            return Err(ExportError::LevelZoomsNotAscending {
                previous_level: i - 1,
                previous_zoom: level_zooms[i - 1],
                previous_gsd: meta.levels[i - 1].gsd,
                level: i,
                zoom: level_zooms[i],
                gsd: meta.levels[i].gsd,
            });
        }
    }
    Ok(level_zooms)
}

mod packed_props;
/// Within-tile feature order (#361).
///
/// MVT does not define draw order, but renderers paint features in the order
/// the tile lists them, so this *is* the paint order for any style that does
/// not override it (`fill-sort-key` and friends). Neither tylertoo nor
/// tippecanoe specified one before, which made a tiler swap a silent visual
/// change; naming it makes it something a caller can depend on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FeatureOrder {
    /// Input row order — the overview file's row order, which is the source
    /// file's row order restricted to the rows the level kept. This is what
    /// tylertoo has always emitted and stays the default.
    #[default]
    Input,
    /// Ascending (or descending) by an MVT property, ties broken by input
    /// order so the result is fully determined.
    Column {
        /// Property name as it appears in the tile.
        name: String,
        /// Sort high → low instead of low → high.
        descending: bool,
    },
}

/// The spelling [`FeatureOrder::from_str`](std::str::FromStr) parses:
/// `input`, or `COLUMN` / `COLUMN:desc`.
impl std::fmt::Display for FeatureOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input => f.write_str("input"),
            Self::Column {
                name,
                descending: false,
            } => f.write_str(name),
            Self::Column {
                name,
                descending: true,
            } => write!(f, "{name}:desc"),
        }
    }
}

impl std::str::FromStr for FeatureOrder {
    type Err = String;

    /// `input` | `<column>` | `<column>:asc` | `<column>:desc`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "input" {
            return Ok(Self::Input);
        }
        let (name, descending) = match s.rsplit_once(':') {
            Some((n, "asc")) => (n, false),
            Some((n, "desc")) => (n, true),
            // A trailing `:something-else` is a mistyped direction far more
            // often than a column whose name contains a colon; say so rather
            // than silently sorting by a column that does not exist.
            Some((_, other)) => {
                return Err(format!(
                    "unknown sort direction {other:?}: expected `asc` or `desc` \
                     (as in `--feature-order level:desc`)"
                ))
            }
            None => (s, false),
        };
        if name.is_empty() {
            return Err("expected `input` or a column name, e.g. `level:desc`".to_string());
        }
        Ok(Self::Column {
            name: name.to_string(),
            descending,
        })
    }
}

/// A member's sort key for [`FeatureOrder::Column`].
///
/// Numeric property variants collapse to one `f64` class so a column read as
/// `Int` in one batch and `Double` in another does not split into two
/// incomparable groups. `Missing` sorts first (painted underneath): a feature
/// the style cannot rank should not be promoted above ones it can.
///
/// The variant order *is* the class order — `Missing` < `Number` < `Text` —
/// and [`OrderKey::cmp`] is a genuine total order, which the caller's
/// `sort_by` requires. Deferring to `f64`'s *partial* order and calling an
/// incomparable pair equal would not be one: with `1.0 == NaN`, `NaN == 2.0`
/// and `1.0 < 2.0`, equality is not transitive, and the standard library
/// detects that and panics. So a NaN — how plenty of sources spell nodata in
/// a float column — never reaches `Number`; [`OrderKey::of`] files it under
/// `Missing`, which is what it is: a value the style cannot rank.
enum OrderKey<'a> {
    Missing,
    Number(f64),
    Text(&'a str),
}

impl<'a> OrderKey<'a> {
    fn of(m: &'a Member, name: &str) -> Self {
        let Some((_, v)) = m.props.iter().find(|(k, _)| *k == name) else {
            return Self::Missing;
        };
        match v {
            PropertyRef::String(s) => Self::Text(s),
            PropertyRef::Float(f) => Self::number(f as f64),
            PropertyRef::Double(d) => Self::number(d),
            PropertyRef::Int(i) => Self::number(i as f64),
            PropertyRef::UInt(u) => Self::number(u as f64),
            PropertyRef::Bool(b) => Self::number(b as u8 as f64),
        }
    }

    /// `Number`, except that a NaN is unrankable and files under `Missing`.
    /// Keeping the invariant here means `cmp` has no incomparable case left.
    fn number(v: f64) -> Self {
        if v.is_nan() {
            Self::Missing
        } else {
            Self::Number(v)
        }
    }

    /// A total order, so `sort_by` is well-defined: classes rank
    /// `Missing` < `Number` < `Text`, numbers by value (`total_cmp`, which is
    /// total by construction and needs no NaN case given the invariant above),
    /// text lexicographically. Equal keys are left to the caller's stable sort,
    /// which resolves them by input order.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Missing, Self::Missing) => Ordering::Equal,
            (Self::Missing, _) => Ordering::Less,
            (_, Self::Missing) => Ordering::Greater,
            (Self::Number(a), Self::Number(b)) => a.total_cmp(b),
            (Self::Number(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Number(_)) => Ordering::Greater,
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
        }
    }
}

/// Warn when `order` names a column the layer does not publish.
///
/// Checked against the published property names rather than the raw schema, so
/// it stays right for a column whose name was restored from the rename
/// provenance (#359) and for one that is deliberately not exported.
fn warn_if_order_column_is_absent(
    schema: &Schema,
    published: &PublishedNames,
    order: &FeatureOrder,
) {
    let FeatureOrder::Column { name, .. } = order else {
        return;
    };
    if !order_column_is_absent(schema, published, order) {
        return;
    }
    log::warn!(
        "--feature-order names {name:?}, which this layer does not publish, so \
         the order is unchanged (input row order). Available: {}",
        published_property_names(schema, published).join(", ")
    );
}

/// Whether `order` names a column the layer will not publish. `false` for
/// [`FeatureOrder::Input`], which names nothing.
fn order_column_is_absent(
    schema: &Schema,
    published: &PublishedNames,
    order: &FeatureOrder,
) -> bool {
    let FeatureOrder::Column { name, .. } = order else {
        return false;
    };
    !published_property_names(schema, published)
        .iter()
        .any(|n| n == name)
}

/// The MVT property keys this schema will advertise.
fn published_property_names(schema: &Schema, published: &PublishedNames) -> Vec<String> {
    let geom_idx = published
        .geometry
        .as_ref()
        .and_then(|name| schema.index_of(name).ok());
    #[cfg(test)]
    let geom_idx = geom_idx.or_else(|| geometry_index(schema));
    let Some(geom_idx) = geom_idx else {
        return Vec::new();
    };
    property_columns(schema, geom_idx, published)
        .into_iter()
        .map(|(_, n)| n)
        .collect()
}

/// Order members for encoding: tile-major always, then within each tile by
/// `order`.
///
/// The `(key, seq)` sort is load-bearing beyond aesthetics — `encode_members`
/// groups tiles by scanning runs of equal `key`, so members must stay
/// contiguous per tile. A within-tile reorder therefore runs as a second,
/// per-run pass rather than as a different global sort key.
/// Compare two members of one tile under a [`FeatureOrder::Column`] request.
///
/// The column decides, and `seq` — the source row index — breaks ties, so the
/// result is a strict order rather than merely a consistent one: the output is
/// fully determined even when the caller's sort is unstable or the input was
/// not already in `seq` order (which is the case after the oversized-tile
/// valve has ranked by vertex count). `descending` reverses the column only;
/// tied features keep input order either way.
fn compare_by_feature_order(
    a: &Member,
    b: &Member,
    name: &str,
    descending: bool,
) -> std::cmp::Ordering {
    let ord = OrderKey::of(a, name).cmp(&OrderKey::of(b, name));
    let ord = if descending { ord.reverse() } else { ord };
    ord.then_with(|| a.seq.cmp(&b.seq))
}

fn sort_members_for_encode(members: &mut [Member], order: &FeatureOrder) {
    members.par_sort_unstable_by_key(|m| (m.key, m.seq));
    let FeatureOrder::Column { name, descending } = order else {
        return;
    };
    // `chunk_by_mut` yields the same runs `encode_members` will group on.
    for tile in members.chunk_by_mut(|a, b| a.key == b.key) {
        tile.sort_by(|a, b| compare_by_feature_order(a, b, name, *descending));
    }
}

/// One tile's members, already in `(key, seq)` order, into feature order:
/// [`sort_members_for_encode`]'s per-tile step.
fn sort_tile_for_encode(tile: &mut [Member], order: &FeatureOrder) {
    if let FeatureOrder::Column { name, descending } = order {
        tile.sort_by(|a, b| compare_by_feature_order(a, b, name, *descending));
    }
}

/// Record which of a tile's members reached it and which were lost, by
/// source row, when a `--feature-id` is set (#443): a source drawn in any
/// tile at any zoom is never lost.
fn account_sources(
    members: &[Member],
    mvt: &TileMvt,
    zoom: u8,
    opts: &ExportOptions,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(), ExportError> {
    use crate::diagnostics::SourceLossCause;
    if opts.feature_id.is_none() {
        return Ok(());
    }
    let cause = if mvt.valve.dropped > 0 {
        SourceLossCause::ValveDropped
    } else if mvt.valve.merged > 0 {
        SourceLossCause::Merged
    } else {
        SourceLossCause::EncodeDropped
    };
    let surviving: std::collections::BTreeSet<u64> = mvt.source_ids.iter().copied().collect();
    // The merge valve owns per-source causes, including kept merges on a
    // tile that also dropped singles. Prefer those to the tile-wide default.
    let losses: std::collections::BTreeMap<u64, _> = mvt.source_losses.iter().copied().collect();
    for member in members {
        if let Some(id) = member.id {
            let cause = losses.get(&member.seq).copied().unwrap_or(cause);
            if !surviving.contains(&member.seq) {
                diagnostics.source_member_loss(member.seq, id, zoom, cause)?;
            }
        }
    }
    if mvt.features > 0 {
        for &id in &mvt.source_ids {
            diagnostics.source_emitted(id, zoom)?;
        }
    }
    Ok(())
}

/// The writer's [`EncodedTile`] for tile `x`/`y`: gzip-compressed here, in
/// the parallel encode section (#227). A tile whose every member encoded to
/// nothing (#431) is an empty carrier the writer loop skips: there is no
/// tile to write, but the drops must still reach the report.
fn encoded_tile(x: u32, y: u32, mvt: TileMvt, fill: FillTally) -> Result<EncodedTile, ExportError> {
    check_mlt(&mvt)?;
    if mvt.features == 0 {
        return Ok(EncodedTile {
            x,
            y,
            data: Vec::new(),
            hash: 0,
            raw_len: 0,
            feature_count: 0,
            encode_dropped: mvt.unencodable,
            encode_quantized: mvt.quantized,
            oversized: mvt.oversized,
            valve: mvt.valve,
            // The tile is not written, so neither is its fill.
            fill: FillTally::default(),
        });
    }
    let hash = TileHasher::hash(&mvt.data);
    let raw_len = mvt.data.len();
    let data = mvt.data.into_gzip(&mvt.gzip_cache)?;
    Ok(EncodedTile {
        x,
        y,
        data,
        hash,
        raw_len,
        feature_count: mvt.features,
        encode_dropped: mvt.unencodable,
        encode_quantized: mvt.quantized,
        oversized: mvt.oversized,
        valve: mvt.valve,
        fill,
    })
}

#[cfg(test)]
fn encode_members(
    gzip_cache: &Arc<TileGzipCache>,
    members: Vec<Member>,
    zoom: u8,
    opts: &ExportOptions,
    fills: Option<&coverage_fill::FillMap>,
    timers: &ExportTimers,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<EncodedTile>, ExportError> {
    encode_layer_members(
        gzip_cache,
        members,
        zoom,
        &TileEncoding::single(opts),
        fills,
        timers,
        diagnostics,
    )
}

fn encode_layer_members(
    gzip_cache: &Arc<TileGzipCache>,
    mut members: Vec<Member>,
    zoom: u8,
    opts: &TileEncoding<'_>,
    fills: Option<&coverage_fill::FillMap>,
    timers: &ExportTimers,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<EncodedTile>, ExportError> {
    if opts.layers.is_empty() {
        sort_members_for_encode(&mut members, &opts.feature_order);
    } else {
        // Joint reads already ordered each layer; preserve their concatenation.
        members.sort_by_key(|m| m.key);
    }

    // Runs of equal key. `members` is sorted by key above, so consecutive
    // grouping is total grouping.
    let groups: Vec<&[Member]> = members.chunk_by(|a, b| a.key == b.key).collect();

    // Encode AND gzip-compress every tile in the parallel section, so the serial
    // writer loop only dedups and appends bytes (issue #227). Compression must
    // match the writer's `tile_compression` (Gzip) so `add_tile_precompressed`
    // stays byte-identical to the old serial `add_tile_with_count` path.
    // #535: `encode` is timed per tile, inside this closure (core-seconds).
    let encode_group = |g: &[Member]| -> Option<Result<EncodedTile, ExportError>> {
        let _encode = ExportTimers::scope(&timers.encode);
        let (x, y) = key_to_xy(g[0].key, zoom);
        let tb = TileCoord::new(x, y, zoom).bounds();
        // #788: the tile's fill is owned for the encode and dropped once
        // its fill commands are built; a spilled fill's read can fail.
        let fill = fills.map(|f| {
            ExportTimers::time(&timers.fill_encode, || {
                f.get(g[0].key).map(|polys| {
                    polys.map(|polys| fill_features(&polys, g, &tb, opts, diagnostics))
                })
            })
        });
        let (fill, mut fill_tally) = match fill.transpose() {
            Ok(fill) => fill.flatten().unwrap_or_default(),
            Err(error) => return Some(Err(error)),
        };
        let mvt = encode_tile_layers(
            gzip_cache,
            g,
            &tb,
            opts,
            &fill,
            &mut fill_tally,
            diagnostics,
        );
        if let Err(error) = account_sources(g, &mvt, zoom, opts, diagnostics) {
            return Some(Err(error));
        }
        if let Some(f) = fills {
            f.note_shed(fill_tally.shed);
        }
        Some(encoded_tile(x, y, mvt, fill_tally))
    };
    // Longest tile first, one tile per task: a tile's valve passes are a
    // serial chain, so a dense tile that starts late sets the wall of its
    // level group. The results are put back in key order, so the output is
    // unchanged.
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(groups[i].len()));
    let mut done: Vec<(usize, Option<Result<EncodedTile, ExportError>>)> = order
        .into_par_iter()
        .with_max_len(1)
        .map(|i| (i, encode_group(groups[i])))
        .collect();
    done.sort_unstable_by_key(|(i, _)| *i);
    done.into_iter().filter_map(|(_, r)| r).collect()
}

#[cfg(test)]
#[path = "export/encode_cost_tests.rs"]
mod encode_cost_tests;

#[cfg(test)]
#[path = "export/valve_cost_tests.rs"]
mod valve_cost_tests;

/// `true` when `bbox` lies entirely within `tb` expanded by `buffer` on every
/// side. Clipping such a feature to the buffered tile is a geometric no-op, so
/// the clip can be skipped (and `BooleanOps` ring normalization avoided).
#[inline]
fn bbox_within_buffered(bbox: &TileBounds, tb: &TileBounds, buffer: f64) -> bool {
    bbox.lng_min >= tb.lng_min - buffer
        && bbox.lat_min >= tb.lat_min - buffer
        && bbox.lng_max <= tb.lng_max + buffer
        && bbox.lat_max <= tb.lat_max + buffer
}

/// One tile's MVT bytes plus the encode tallies the report needs.
struct TileMvt {
    gzip_cache: Arc<TileGzipCache>,
    source_ids: Vec<u64>,
    source_losses: Vec<(u64, crate::diagnostics::SourceLossCause)>,
    /// Raw bytes and the gzip of exactly those bytes. Mutable access to the
    /// raw buffer invalidates gzip before exposing the buffer.
    data: TileData,
    /// MVT features actually written to the layer. A `GeometryCollection`
    /// member contributes one per geometry kind it holds (#431).
    features: usize,
    /// Members with nothing to encode — empty geometries or empty
    /// collections (#431). Content loss; gates the export warning.
    unencodable: usize,
    /// Members with coordinates that collapsed at the tile extent — polygon
    /// rings of zero area, lines of fewer than two points (clip slivers at a
    /// buffered edge). Expected; informational only.
    quantized: usize,
    /// Whether the oversized valve fired.
    oversized: bool,
    /// What the size valve did to the tile (#745).
    valve: ValveTally,
    /// Why the MVT did not convert to the export's MLT, if it did not;
    /// `data` is then empty and the export fails at this tile.
    mlt_error: Option<String>,
}

// A read-only cached-gzip view for fill's existing cost observer. All raw
// mutation goes through TileData's invalidating mutable view.
impl std::ops::Deref for TileMvt {
    type Target = TileData;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

/// What the size valve did to one tile (#745), for the report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ValveTally {
    /// Members merged into another member.
    merged: usize,
    /// Members dropped: by the drop valve, or by the merge valve's fallback
    /// when merging alone could not make the tile fit.
    dropped: usize,
    /// Whole merged features dropped, and their source-member count.
    dropped_merged: usize,
    dropped_merged_sources: usize,
    /// Reduced grid of the tile; per-zoom aggregate keeps the minimum.
    reduced_extent: Option<u32>,
    reduced_tiles: usize,
    /// Encodes the tile took, the first included: 1 for a tile that fit at
    /// once, under either valve.
    passes: u32,
    /// The merge threshold the tile ended on.
    mingap: u64,
    /// Tiles the valve left over the limit: 0 or 1 per tile, summed (#814).
    over_limit: usize,
    /// The over-cap tile's lone survivor absorbed another source feature.
    over_limit_merged: bool,
}

/// One #750 coverage-fill feature of a tile: the fill polygons that joined
/// one of the tile's features, drawn with that feature's id and tags. Built
/// once per tile; the fill then takes only the room the size valve leaves
/// ([`with_fill`]).
struct FillFeature {
    id: Option<u64>,
    source_row: u64,
    props: SharedProps,
    cmds: Vec<u32>,
    /// Its polygons and their vertices, for the report.
    polygons: usize,
    vertices: usize,
    /// Its area in tile units², which decides what is shed first.
    area: f64,
}

impl FillFeature {
    fn encode_fill(
        id: Option<u64>,
        source_row: u64,
        props: SharedProps,
        polys: &[&coverage_fill::FillPolygon],
        diagnostics: &crate::diagnostics::Diagnostics,
    ) -> Self {
        #[cfg(test)]
        coverage_fill::cost_probe::update(|c| c.fill_polygons_encoded += polys.len());
        Self {
            id,
            source_row,
            props,
            cmds: crate::mvt::encode_tile_unit_polygons(polys.iter().copied(), diagnostics),
            polygons: polys.len(),
            vertices: polys.iter().flat_map(|p| p.iter()).map(Vec::len).sum(),
            area: polys.iter().map(|p| coverage_fill::poly_area(p)).sum(),
        }
    }
}

/// What a tile's coverage fill drew (#750), for the report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FillTally {
    /// Fill polygons drawn.
    polygons: usize,
    /// Their vertices, holes included.
    vertices: usize,
    /// Fill polygons left out to stay within [`fill_tile_bytes`], or to leave
    /// the tile's own features their room ([`with_fill`]).
    shed: usize,
}

/// A tile's fill features: every fill polygon joins the member whose tile-unit
/// bbox holds it ([`coverage_fill::assign_hosts`]), and each host's polygons
/// are encoded once as one MultiPolygon.
fn fill_features(
    polys: &[coverage_fill::FillPolygon],
    members: &[Member],
    tb: &TileBounds,
    opts: &ExportOptions,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> (Vec<FillFeature>, FillTally) {
    if members.is_empty() || polys.is_empty() {
        return (Vec::new(), FillTally::default());
    }
    let (kept, shed) = coverage_fill::within_budget(polys, fill_tile_bytes(opts));
    let polys: Vec<&coverage_fill::FillPolygon> = kept.into_iter().map(|i| &polys[i]).collect();
    let polys = polys.as_slice();
    let tally = FillTally {
        polygons: polys.len(),
        vertices: polys.iter().flat_map(|p| p.iter()).map(Vec::len).sum(),
        shed,
    };
    let proj = crate::mvt::TileProjector::new(tb, opts.extent);
    // A polygon fill takes a polygon's tags when the tile has polygons: a
    // road's or a point's would colour it by the wrong attributes.
    let polygonal =
        |m: &Member| matches!(*m.geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
    let any_polygon = members.iter().any(polygonal);
    let hosts: Vec<[f64; 4]> = members
        .iter()
        .map(|m| match m.geom.bounding_rect() {
            Some(_) if any_polygon && !polygonal(m) => [f64::INFINITY; 4],
            Some(r) => {
                let (x0, y0) = proj.project(r.min().x, r.max().y);
                let (x1, y1) = proj.project(r.max().x, r.min().y);
                [x0, y0, x1, y1]
            }
            None => [f64::INFINITY; 4],
        })
        .collect();
    let mut groups: std::collections::BTreeMap<usize, Vec<&coverage_fill::FillPolygon>> =
        std::collections::BTreeMap::new();
    for (poly, host) in polys
        .iter()
        .copied()
        .zip(coverage_fill::assign_hosts(polys.iter().copied(), &hosts))
    {
        groups.entry(host).or_default().push(poly);
    }
    let feats = groups
        .into_iter()
        .map(|(host, group)| {
            FillFeature::encode_fill(
                members[host].id,
                members[host].seq,
                members[host].props.clone(),
                &group,
                diagnostics,
            )
        })
        .filter(|f| !f.cmds.is_empty())
        .collect();

    (feats, tally)
}

/// Tile-byte allowance for a tile's #750 fill: a quarter of its size limit,
/// with a compressed limit read as about twice that many raw bytes. This is
/// separate from the memory workspace granted under `MemoryPhase::Fill`.
/// Unlimited without a limit. At half the limit the size valve ran up to
/// ten passes per dense tile squeezing the tile's own features into the
/// rest, each pass re-encoding the whole tile; at a quarter, coverage on FTW
/// Moldova was the same and the export 27% faster.
fn fill_tile_bytes(opts: &ExportOptions) -> usize {
    let raw = opts.tile_size_limit.filter(|&l| l > 0);
    let gz = opts
        .compressed_tile_size_limit
        .filter(|&l| l > 0)
        .map(|l| l.saturating_mul(2));
    match (raw, gz) {
        (Some(a), Some(b)) => a.min(b) / 4,
        (Some(a), None) | (None, Some(a)) => a / 4,
        (None, None) => usize::MAX,
    }
}

/// What [`build_mvt`] needs from a tile feature: a plain member, or a member
/// with others merged into it (#745).
trait TileFeature: Sync {
    fn geometry(&self) -> &Geometry<f64>;
    fn coords_count(&self) -> usize {
        self.geometry().coords_count()
    }
    fn properties(&self) -> &SharedProps;
    fn feature_id(&self) -> Option<u64>;
    fn source_row(&self) -> u64;
    fn band(&self) -> u16;
}

impl TileFeature for Member {
    fn band(&self) -> u16 {
        #[cfg(test)]
        JOINT_BAND_READS.with(|n| n.set(n.get() + 1));
        self.band
    }
    fn geometry(&self) -> &Geometry<f64> {
        &self.geom
    }
    fn properties(&self) -> &SharedProps {
        &self.props
    }
    fn feature_id(&self) -> Option<u64> {
        self.id
    }
    fn source_row(&self) -> u64 {
        self.seq
    }
}

#[cfg(test)]
thread_local! {
    static JOINT_BAND_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl TileFeature for coalesce_valve::Merged<'_> {
    fn band(&self) -> u16 {
        self.target.band
    }
    fn geometry(&self) -> &Geometry<f64> {
        self.geometry()
    }
    fn coords_count(&self) -> usize {
        self.coords_count()
    }
    fn properties(&self) -> &SharedProps {
        self.props()
    }
    fn feature_id(&self) -> Option<u64> {
        self.target.id
    }
    fn source_row(&self) -> u64 {
        self.target.seq
    }
}

impl<T: TileFeature + ?Sized> TileFeature for &T {
    fn band(&self) -> u16 {
        (**self).band()
    }
    fn geometry(&self) -> &Geometry<f64> {
        (**self).geometry()
    }
    fn coords_count(&self) -> usize {
        (**self).coords_count()
    }
    fn properties(&self) -> &SharedProps {
        (**self).properties()
    }
    fn feature_id(&self) -> Option<u64> {
        (**self).feature_id()
    }
    fn source_row(&self) -> u64 {
        (**self).source_row()
    }
}

/// The valve's export options and optional joint layer mapping. A normal
/// export uses an empty mapping and the original single-layer build path.
struct TileEncoding<'a> {
    options: &'a ExportOptions,
    layers: &'a [(u16, String)],
}

impl<'a> TileEncoding<'a> {
    /// One layer named by the options, the ordinary export.
    fn single(options: &'a ExportOptions) -> Self {
        Self {
            options,
            layers: &[],
        }
    }
}

impl std::ops::Deref for TileEncoding<'_> {
    type Target = ExportOptions;
    fn deref(&self) -> &Self::Target {
        self.options
    }
}

/// Encode a single tile's members to MVT bytes, applying the size valve.
///
/// The valve fits the tile's own features alone, exactly as without a #750
/// fill; the fill then takes only the room they leave ([`with_fill`]), so
/// it never makes the valve merge or drop a real feature. `fill_tally`
/// counts what of the fill is drawn and shed.
#[cfg(test)]
fn encode_tile(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[Member],
    tb: &TileBounds,
    opts: &ExportOptions,
    fill: &[FillFeature],
    fill_tally: &mut FillTally,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    encode_tile_layers(
        gzip_cache,
        members,
        tb,
        &TileEncoding::single(opts),
        fill,
        fill_tally,
        diagnostics,
    )
}

fn encode_tile_layers(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[Member],
    tb: &TileBounds,
    opts: &TileEncoding<'_>,
    fill: &[FillFeature],
    fill_tally: &mut FillTally,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    if opts.size_valve == SizeValve::Merge && !is_point_like(members) {
        return merge_to_fit(gzip_cache, members, tb, opts, fill, fill_tally, diagnostics);
    }
    drop_to_fit(gzip_cache, members, tb, opts, fill, fill_tally, diagnostics)
}

/// Retained input for the merge's joint valve. No overview is opened until a
/// joined tile exceeds a cap. Cell bands can retain one source per zoom.
#[derive(Clone)]
pub(crate) struct JointSource {
    pub path: PathBuf,
    pub options: ExportOptions,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub members: JointMembers,
}

/// The source export's scan counts and resident member price, before valves
/// shed or merge members. Tile ids already include the zoom.
#[derive(Clone, Default)]
pub(crate) struct JointMembers {
    tile_counts: BTreeMap<u64, usize>,
    member_bytes: Option<u64>,
}

/// A merge's retained overview plans. Footers, columns, CRS and zoom ladders
/// are resolved once, then reused by every zoom's restricted wave reads.
pub(crate) struct JoinedTileFitter<'a> {
    sources: Vec<OpenedJointSource<'a>>,
}

struct OpenedJointSource<'a> {
    source: &'a JointSource,
    reader: OverviewReader,
    columns: ResolvedColumns,
    crs: Crs,
    zooms: Vec<u8>,
    /// #802: the finest level's pins, as the source export built them, so a
    /// refitted max-zoom tile keeps the borders its source export kept.
    pins: Option<JunctionNodes>,
}

impl<'a> JoinedTileFitter<'a> {
    /// Resolve once per pending zoom, before the merge sizes its chunks.
    pub(crate) fn tile_wave(&self, tiles: &BTreeSet<u64>, opts: &ExportOptions) -> usize {
        #[cfg(test)]
        joined_tile_probe::PROFILE.with(|profile| profile.set(Some(opts.profile)));
        self.tile_wave_under(tiles, opts, &export_budget())
    }

    fn tile_wave_under(
        &self,
        tiles: &BTreeSet<u64>,
        opts: &ExportOptions,
        budget: &MemoryBudget,
    ) -> usize {
        joined_tile_wave_under(
            tiles,
            self.sources.iter().map(|s| &s.source.members),
            opts,
            budget,
        )
    }

    /// The joins this fitter may refit; the rest are reported and left to
    /// the source valves (see [`joined_tiles_within`]).
    pub(crate) fn within_memory(
        &self,
        tiles: BTreeSet<u64>,
        diagnostics: &crate::diagnostics::Diagnostics,
    ) -> BTreeSet<u64> {
        let limit = probe_oom_memory_limit().filter(|limit| {
            limit.source != MemoryLimitSource::Override && !skip_memory_preflight_from_env()
        });
        let Some(limit) = limit else {
            return tiles;
        };
        let fit = joined_tiles_within(
            &tiles,
            self.sources.iter().map(|s| &s.source.members),
            limit,
        );
        if let Some(first) = tiles.difference(&fit).next() {
            diagnostics.warn(format!(
                "{} joined tile(s) over a size cap (first id {first}) were not refitted: \
                 their members across layers exceed the {} of {} bytes",
                tiles.len() - fit.len(),
                limit.source.describe(),
                limit.bytes
            ));
        }
        fit
    }

    pub(crate) fn open(
        sources: impl IntoIterator<Item = &'a JointSource>,
    ) -> Result<Self, ExportError> {
        let started = Instant::now();
        let replay = &crate::diagnostics::Diagnostics::silent();
        let sources = sources
            .into_iter()
            .map(|source| {
                let reader = OverviewReader::open(&source.path)?;
                let zooms = resolve_level_zooms(reader.meta(), reader.num_levels())?;
                let band = BandRead {
                    source: reader.source(),
                    budget: export_budget(),
                    profile: source.options.profile,
                };
                let columns = resolve_export_columns((&reader, band), &source.options, &zooms)?;
                let crs = crs_from_descriptor(reader.resolved_schema(), &source.path, replay)?;
                let pins = finest_level_pins(
                    (&reader, band),
                    crs,
                    &source.options,
                    &mut export_budget(),
                    replay,
                )?;
                Ok(OpenedJointSource {
                    source,
                    reader,
                    columns,
                    crs,
                    zooms,
                    pins,
                })
            })
            .collect::<Result<Vec<_>, ExportError>>()?;
        log::debug!(
            "[profile] joined plans: {} overview(s), {:.3}s",
            sources.len(),
            started.elapsed().as_secs_f64()
        );
        Ok(Self { sources })
    }

    /// Fit one wave of joins. The merge releases its replacements before
    /// requesting another wave. Only valve counts from replay are published.
    pub(crate) fn fit_joined_tiles(
        &self,
        zoom: u8,
        tiles: &BTreeSet<u64>,
        opts: &ExportOptions,
        diagnostics: &crate::diagnostics::Diagnostics,
    ) -> Result<BTreeMap<u64, JoinedTile>, ExportError> {
        #[cfg(test)]
        joined_tile_probe::ZOOMS.with(|zooms| zooms.set(zooms.get() | (1 << zoom)));
        let started = Instant::now();
        let replay = &crate::diagnostics::Diagnostics::silent();
        // The replay's discarded loss ledger still spills; keep it where the
        // source exports spilled, never the system temp directory.
        replay.set_accounting_spill_dir(
            self.sources
                .iter()
                .find_map(|s| s.source.options.spill_dir.as_deref()),
        );
        let cache = Arc::new(TileGzipCache::new(8 * 1024 * 1024));
        let timers = ExportTimers::default();
        // A partition per requested tile gives the existing encoder parallel
        // valve chains, while a wave reads each layer only once. The tile-set
        // restriction also prunes disjoint bboxes and prevents clipping holes
        // between requested ids, as in variable-depth and shard exports.
        let tile_counts = tiles.iter().map(|&id| (id, 1)).collect();
        let plan = LevelPlan::cut(
            zoom,
            plan_partitions(&tile_counts, zoom, 1),
            self.tile_wave(tiles, opts),
            usize::MAX,
        );
        let mut replacements = BTreeMap::new();
        for wave in plan.waves() {
            let mut buckets: Vec<Vec<Member>> = (0..wave.len()).map(|_| Vec::new()).collect();
            let mut layers = Vec::new();
            let mut seq_base = 0;
            for OpenedJointSource {
                source,
                reader,
                columns,
                crs,
                zooms,
                pins,
            } in &self.sources
            {
                if zoom < source.min_zoom || zoom > source.max_zoom {
                    continue;
                }
                let Some(level_idx) = zooms.iter().position(|&z| z == zoom) else {
                    continue;
                };
                let ctx = LevelCtx {
                    reader,
                    band: BandRead {
                        source: reader.source(),
                        budget: export_budget(),
                        profile: source.options.profile,
                    },
                    level_idx,
                    crs: *crs,
                    zoom,
                    opts: &source.options,
                    gzip_cache: &cache,
                    published: &columns.published,
                    feature_id: columns.feature_id.as_ref(),
                    timers: &timers,
                    fills: None,
                    tiles: Some(&tile_counts),
                    pins: pins.as_ref(),
                    #[cfg(test)]
                    work: &Mutex::new(ExportWorkTally::default()),
                };
                let collected = collect_wave(&ctx, wave, replay)?.buckets;
                let mut bands = BTreeMap::new();
                let mut next_seq = seq_base;
                for (bucket, mut members) in buckets.iter_mut().zip(collected) {
                    sort_members_for_encode(&mut members, &source.options.feature_order);
                    for m in &mut members {
                        let next_band = u16::try_from(layers.len()).map_err(|_| {
                            ExportError::Io(std::io::Error::other("too many joint valve bands"))
                        })?;
                        m.band = *bands.entry(m.band).or_insert_with(|| {
                            layers.push((next_band, source.options.layer_name.clone()));
                            next_band
                        });
                        m.seq += seq_base;
                        next_seq = next_seq.max(m.seq + 1);
                    }
                    bucket.extend(members);
                }
                seq_base = next_seq;
            }
            // DIVERGENCE FROM TIPPECANOE: gaps and predecessors remain local to
            // each overview row group, with concatenated layer sequences rather
            // than one global spatial stream (tile.cpp:1176-1195, 2016-2050).
            let encoded = encode_buckets(
                &cache,
                (buckets, None),
                zoom,
                &TileEncoding {
                    options: opts,
                    layers: &layers,
                },
                None,
                &timers,
                replay,
            )?;
            for tile in encoded.into_iter().flatten() {
                diagnostics.record(crate::diagnostics::Diagnostic::Merged {
                    count: tile.valve.merged,
                });
                diagnostics.record(crate::diagnostics::Diagnostic::ValveDropped {
                    count: tile.valve.dropped,
                });
                replacements.insert(
                    tile_key(tile.x, tile.y, zoom),
                    JoinedTile {
                        data: tile.data,
                        raw_len: tile.raw_len,
                        merged: tile.valve.over_limit_merged,
                        #[cfg(test)]
                        _live: joined_tile_probe::LiveTile::new(),
                    },
                );
            }
        }
        let stages = timers.stage_secs();
        log::debug!(
            "[profile] joined z{zoom}: {} tile(s), wall={:.3}s; \
             core-seconds read={:.3} decode={:.3} clip={:.3} encode={:.3}",
            tiles.len(),
            started.elapsed().as_secs_f64(),
            stages.band_read,
            stages.decode,
            stages.clip,
            stages.encode,
        );
        if opts.feature_id.is_some() && !replacements.is_empty() {
            // shortcut: the per-layer loss ledgers are sealed before the merge,
            // so a joint refit's drops are not in them; feed them in if needed.
            diagnostics.warn(
                "--feature-id: joined tiles over a size cap were refitted across layers; \
                 the source losses list each layer's own export and miss the joint \
                 valve's merges and drops"
                    .to_string(),
            );
        }
        Ok(replacements)
    }
}

/// Price the densest pending tile from original overview members, summed
/// across sources, using the same resident model and budget as level waves.
fn joined_tile_wave_under<'a>(
    tiles: &BTreeSet<u64>,
    sources: impl Iterator<Item = &'a JointMembers> + Clone,
    opts: &ExportOptions,
    budget: &MemoryBudget,
) -> usize {
    let resident = tiles
        .iter()
        .map(|&id| joined_tile_resident(id, sources.clone(), opts))
        .max()
        .unwrap_or(0);
    resolve_partition_wave_under(opts.partition_wave, opts.profile, budget).min(budget.affordable(
        opts.profile,
        MemoryPhase::ExportCollect,
        Price::new(resident, PriceBasis::Footer),
    ))
}

/// A joined tile's members across its sources, priced as an export prices one.
fn joined_tile_resident<'a>(
    id: u64,
    sources: impl Iterator<Item = &'a JointMembers>,
    opts: &ExportOptions,
) -> u64 {
    sources.fold(0u64, |total, source| {
        let per_member = source
            .member_bytes
            .map_or(0, |bytes| export_member_resident_bytes(bytes, opts));
        let count = source.tile_counts.get(&id).copied().unwrap_or(0) as u64;
        total.saturating_add(count.saturating_mul(per_member))
    })
}

/// The joins whose members fit `limit` together, priced as [`check_levels_fit`]
/// prices a tile. An export refuses a tile that cannot (#665), but each source
/// export already wrote its part, so a join that cannot is not refitted: it
/// keeps the source valves' result and the over-cap warning instead of
/// risking an OOM kill.
fn joined_tiles_within<'a>(
    tiles: &BTreeSet<u64>,
    sources: impl Iterator<Item = &'a JointMembers> + Clone,
    limit: MemoryLimit,
) -> BTreeSet<u64> {
    tiles
        .iter()
        .copied()
        .filter(|id| {
            let resident = sources.clone().fold(0u64, |total, source| {
                let count = source.tile_counts.get(id).copied().unwrap_or(0) as u64;
                total.saturating_add(count.saturating_mul(source.member_bytes.unwrap_or(0)))
            });
            resident <= limit.bytes
        })
        .collect()
}

/// A fitted join, already gzipped by the parallel export encoder.
pub(crate) struct JoinedTile {
    pub data: Vec<u8>,
    pub raw_len: usize,
    pub merged: bool,
    #[cfg(test)]
    _live: joined_tile_probe::LiveTile,
}

/// Observe actual replacement lifetimes on the merge's thread, including
/// unwritten replacements retained across calls to the fitter.
#[cfg(test)]
pub(crate) mod joined_tile_probe {
    use std::cell::Cell;

    thread_local! {
        static COUNTS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
        pub(super) static PROFILE: Cell<Option<super::MemoryProfile>> = const { Cell::new(None) };
        pub(super) static ZOOMS: Cell<u32> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        COUNTS.with(|counts| counts.set((0, 0)));
        PROFILE.with(|profile| profile.set(None));
        ZOOMS.with(|zooms| zooms.set(0));
    }

    pub(crate) fn counts() -> (usize, usize) {
        COUNTS.with(Cell::get)
    }

    pub(crate) fn profile() -> Option<super::MemoryProfile> {
        PROFILE.with(Cell::get)
    }

    pub(crate) fn zooms() -> u32 {
        ZOOMS.with(Cell::get)
    }

    pub(super) struct LiveTile;

    impl LiveTile {
        pub(super) fn new() -> Self {
            COUNTS.with(|counts| {
                let (live, peak) = counts.get();
                counts.set((live + 1, peak.max(live + 1)));
            });
            Self
        }
    }

    impl Drop for LiveTile {
        fn drop(&mut self) {
            COUNTS.with(|counts| {
                let (live, peak) = counts.get();
                counts.set((live - 1, peak));
            });
        }
    }
}

/// Encodes [`with_fill`] may take to fit a #750 fill into a tile's slack
/// before it writes the tile without one.
const MAX_FILL_PASSES: u32 = 4;

/// The tile the size valve fitted (`fitted`) with as much of its #750 fill
/// as fits in the room left, re-encoding the same features with a subset of
/// the fill through `encode` (the merge valve's cached encode, so a heavily
/// merged tile is not encoded the slow way again): all of it if the tile still
/// fits, else the largest fill features whose bytes fit the slack, else
/// none. A tile the valve could not fit gets no fill. Each extra encode
/// counts as a valve pass. A reduced grid or a dropped merge also sheds
/// fill: its old commands must not redraw a dropped merge's source.
fn with_fill(
    mut fitted: TileMvt,
    opts: &ExportOptions,
    fill: &[FillFeature],
    tally: &mut FillTally,
    mut encode: impl FnMut(&[&FillFeature]) -> TileMvt,
) -> TileMvt {
    // Observe successful gzip-buffer replacement, rather than assuming a
    // fit check compressed. This guard can pass when the fit owner reuses
    // cached bytes, without changing the observer to predict that fix.
    #[cfg(test)]
    fn note_gzip(before: Option<*const u8>, mvt: &TileMvt) {
        let after = mvt.gzip.as_ref().map(|g| g.as_ptr());
        if after.is_some() && after != before {
            coverage_fill::cost_probe::update(|c| {
                c.fill_gzip_calls += 1;
                c.cached_gzip_rechecks += usize::from(before.is_some());
            });
        }
    }
    if fill.is_empty() {
        return fitted;
    }
    #[cfg(test)]
    let base_gzip_before = fitted.gzip.as_ref().map(|g| g.as_ptr());

    // Drawn and shed, from the fill features themselves.
    let total: usize = fill.iter().map(|f| f.polygons).sum();
    let no_fill = |tally: &mut FillTally, fitted: TileMvt| {
        tally.shed += total;
        tally.polygons = 0;
        tally.vertices = 0;
        fitted
    };
    if fitted.valve.reduced_extent.is_some()
        || fitted.valve.dropped_merged > 0
        || tile_fit(&mut fitted, opts).unwrap_or(f64::INFINITY) < 1.0
    {
        #[cfg(test)]
        note_gzip(base_gzip_before, &fitted);
        return no_fill(tally, fitted);
    }
    #[cfg(test)]
    note_gzip(base_gzip_before, &fitted);
    // Largest first; drawn in their own order.
    let mut by_area: Vec<usize> = (0..fill.len()).collect();
    by_area.sort_by(|&a, &b| fill[b].area.total_cmp(&fill[a].area).then(a.cmp(&b)));
    let mut keep = fill.len();
    let mut tried = 0;
    for pass in 1..=MAX_FILL_PASSES {
        tried = pass;
        let mut kept: Vec<usize> = by_area[..keep].to_vec();
        kept.sort_unstable();
        let subset: Vec<&FillFeature> = kept.iter().map(|&i| &fill[i]).collect();
        let mut mvt = encode(&subset);

        #[cfg(test)]
        coverage_fill::cost_probe::update(|c| {
            c.fill_passes += 1;
        });
        #[cfg(test)]
        let gzip_before = mvt.gzip.as_ref().map(|g| g.as_ptr());
        let fit = tile_fit(&mut mvt, opts).unwrap_or(f64::INFINITY);
        #[cfg(test)]
        note_gzip(gzip_before, &mvt);
        if fit >= 1.0 {
            let (polygons, vertices) = subset
                .iter()
                .fold((0, 0), |(p, v), f| (p + f.polygons, v + f.vertices));
            tally.shed += total - polygons;
            tally.polygons = polygons;
            tally.vertices = vertices;
            return TileMvt {
                oversized: fitted.oversized,
                valve: ValveTally {
                    passes: fitted.valve.passes + pass,
                    ..fitted.valve
                },
                ..mvt
            };
        }
        // Shed the fill's share of the overrun, with a margin: the kept
        // fill's bytes, less what the whole tile is over by.
        let fill_bytes = mvt.data.len().saturating_sub(fitted.data.len()).max(1) as f64;
        let over = mvt.data.len() as f64 * (1.0 - fit);
        let share = ((fill_bytes - over) / fill_bytes * 0.9).max(0.0);
        let budget = subset.iter().map(|f| f.cmds.len()).sum::<usize>() as f64 * share;
        let mut used = 0usize;
        let next = by_area[..keep]
            .iter()
            .take_while(|&&i| {
                used += fill[i].cmds.len();
                used as f64 <= budget
            })
            .count();
        keep = next.min(keep - 1);
        if keep == 0 {
            break;
        }
    }
    // None of the fill fit: the tile goes without, but its encodes count.
    fitted.valve.passes += tried;
    no_fill(tally, fitted)
}

/// Whether the size valve can merge anything: [`SizeValve::Merge`] with a
/// size limit in force. Otherwise no gap is ever compared, and the read
/// skips measuring them (#745).
fn valve_merges(opts: &ExportOptions) -> bool {
    opts.size_valve == SizeValve::Merge && opts.has_tile_size_cap()
}

/// Whether every member has at most one coordinate: a tile of points (or of
/// polygons collapsed to centroid points, the #259 dot-fill recipe).
fn is_point_like<M: TileFeature>(members: &[M]) -> bool {
    members.iter().all(|m| m.geometry().coords_count() <= 1)
}

/// The share of a tile that fits its size limits: the smallest
/// `limit / size` over the limits in force, so `>= 1.0` fits (infinite with
/// no limit). Compresses the tile only when a compressed limit is set, and
/// keeps the bytes for the writer. Reuses gzip only while the raw data is
/// unchanged; `TileData` invalidates it on every mutable raw-buffer access.
/// Both currently active limits are recomputed even on a cache hit.
fn tile_fit(mvt: &mut TileMvt, opts: &ExportOptions) -> Result<f64, ExportError> {
    check_mlt(mvt)?;
    let mut fit = f64::INFINITY;
    if let Some(limit) = opts.tile_size_limit.filter(|&l| l > 0) {
        fit = fit.min(limit as f64 / mvt.data.len() as f64);
    }
    if let Some(limit) = opts.compressed_tile_size_limit.filter(|&l| l > 0) {
        let gz = mvt.data.gzipped(&mvt.gzip_cache)?;
        fit = fit.min(limit as f64 / gz.len() as f64);
    }
    Ok(fit)
}

/// The export's failure for a tile whose MVT did not convert to MLT.
fn check_mlt(mvt: &TileMvt) -> Result<(), ExportError> {
    match &mvt.mlt_error {
        Some(message) => Err(ExportError::MltConversion {
            message: message.clone(),
        }),
        None => Ok(()),
    }
}

/// Encodes the drop valve may take before it gives up and writes the tile
/// over its limit. Each pass keeps the share of the last that fit, so a
/// tile needing more than a few is pathological.
pub(crate) const MAX_DROP_PASSES: u32 = 8;

/// How many of `keep` features the drop loops keep on cut `cut` (0-based)
/// of a tile at `fit`: the share that fits, with tippecanoe's 0.8 margin
/// from the second cut on. Without it a tile just over its cap shed a
/// sliver per pass and ran out of passes still over (#873).
fn drop_keep(keep: usize, fit: f64, cut: u32) -> usize {
    // DIVERGENCE FROM TIPPECANOE: tile.cpp applies its 0.8 margin on the
    // first cut too. We keep the first cut as before, so a tile that fit
    // after one cut is byte-identical.
    (keep as f64 * fit * if cut == 0 { 1.0 } else { 0.8 }).floor() as usize
}

/// One grid unit per pixel on a 512-pixel tile before losing whole features.
const MIN_VALVE_EXTENT: u32 = 512;

/// The drop valve: while the tile is over its limit, keep a proportional
/// count of members and re-encode. `select_kept_members` decides which
/// survive.
///
/// Before #745 this was a single pass that never re-measured, and a tile
/// whose survivors still encoded too big was written over its limit (45 of
/// 1,286 tiles on the FTW Moldova benchmark). The first pass is unchanged,
/// so a tile that fit after one pass is byte-identical to before.
fn drop_to_fit(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[Member],
    tb: &TileBounds,
    opts: &TileEncoding<'_>,
    fill: &[FillFeature],
    fill_tally: &mut FillTally,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    let layers = opts.layers;
    let mut full = build_tile_layers(gzip_cache, layers, members, tb, opts, &[], diagnostics);
    let mut fit = tile_fit(&mut full, opts).unwrap_or(f64::INFINITY);
    if fit >= 1.0 || members.len() <= 1 {
        // One encode, as the merge valve counts a tile that fits at once.
        let fitted = TileMvt {
            valve: ValveTally {
                passes: 1,
                over_limit: usize::from(fit < 1.0),
                ..ValveTally::default()
            },
            ..full
        };
        return with_fill(fitted, opts, fill, fill_tally, |f| {
            build_tile_layers(gzip_cache, layers, members, tb, opts, f, diagnostics)
        });
    }
    // Later passes need only the original size for the warning.
    let full_bytes = full.data.len();
    drop(full);
    let mut keep = members.len();
    let mut passes = 1u32;
    loop {
        passes += 1;
        keep = drop_keep(keep, fit, passes - 2).clamp(1, keep.saturating_sub(1).max(1));
        let kept = shed_to_fit(members, keep, opts);
        let mut shed = build_tile_layers(gzip_cache, layers, &kept, tb, opts, &[], diagnostics);
        fit = tile_fit(&mut shed, opts).unwrap_or(f64::INFINITY);
        if fit >= 1.0 || keep <= 1 || passes >= MAX_DROP_PASSES {
            log::debug!(
                "oversized tile ({} bytes): dropped {} of {} features ({passes} encodes){}",
                full_bytes,
                members.len() - keep,
                members.len(),
                if fit < 1.0 {
                    "; still over the limit"
                } else {
                    ""
                },
            );
            let fitted = TileMvt {
                oversized: true,
                valve: ValveTally {
                    dropped: members.len() - keep,
                    passes,
                    over_limit: usize::from(fit < 1.0),
                    ..ValveTally::default()
                },
                ..shed
            };
            return with_fill(fitted, opts, fill, fill_tally, |f| {
                build_tile_layers(gzip_cache, layers, &kept, tb, opts, f, diagnostics)
            });
        }
    }
}

/// How many more passes a stalling tile may need, at the rate its last pass
/// shrank it, before the merge valve stops raising the threshold step by
/// step and merges everything (#758).
const MAX_PROJECTED_PASSES: f64 = 4.0;

/// Has the merge valve stalled? From the fit before and after a pass: the
/// tile did not shrink, or shrinking at that rate it would need more than
/// [`MAX_PROJECTED_PASSES`] further passes to fit. Merging features that do
/// not touch saves only their tags, so on detailed, scattered polygons each
/// pass shrinks the tile by a few percent.
fn stalled(prev_fit: f64, fit: f64) -> bool {
    let rate = fit / prev_fit;
    rate <= 1.0 || (1.0 / fit).ln() / rate.ln() > MAX_PROJECTED_PASSES
}

/// The merge valve (#745): merge members whose gap is below the threshold
/// into the member kept before them, re-encode, and raise the threshold
/// until the tile fits. See [`coalesce_valve`]. A tile that stops shrinking
/// ([`stalled`]) for two passes in a row merges everything it can at once
/// (#758).
///
/// A tile under its limit is encoded exactly as [`drop_to_fit`] would. When
/// the threshold cannot rise any further and the tile is still over, the
/// unmerged features are dropped, smallest first. Then halve the extent to
/// 512 before dropping merged features, smallest first. Only a lone feature
/// that still exceeds the cap can be written over it, with a warning.
fn merge_to_fit(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[Member],
    tb: &TileBounds,
    opts: &TileEncoding<'_>,
    fill: &[FillFeature],
    fill_tally: &mut FillTally,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    let layers = opts.layers;
    // The merge walks the file's spatial order; `--feature-order` sorted the
    // tile by a column instead.
    let mut ordered: Vec<&Member> = members.iter().collect();
    if !matches!(opts.feature_order, FeatureOrder::Input) {
        ordered.sort_unstable_by_key(|m| m.seq);
    }
    // Every pass keeps targets from `ordered`. Restricting this global
    // stable order to those targets gives the same two stable sorts, without
    // repeating the comparisons after each merge or drop.
    let mut draw_indices: Vec<usize> = (0..ordered.len()).collect();
    #[cfg(test)]
    crate::valve_counter::draw_sort(draw_indices.len());
    draw_indices.sort_by_key(|&i| ordered[i].seq);
    if let FeatureOrder::Column { name, descending } = &opts.feature_order {
        #[cfg(test)]
        crate::valve_counter::draw_sort(draw_indices.len());
        draw_indices
            .sort_by(|&a, &b| compare_by_feature_order(ordered[a], ordered[b], name, *descending));
    }
    let mut draw_rank = vec![0; ordered.len()];
    for (rank, i) in draw_indices.into_iter().enumerate() {
        draw_rank[i] = rank;
    }
    // Store positions rather than references: each pass owns new merged
    // features, but the tile-sized scratch buffer survives every pass.
    let mut ranked = vec![None; ordered.len()];
    let mut draw_order = |feats: &mut Vec<&coalesce_valve::Merged<'_>>| {
        for (i, m) in feats.iter().enumerate() {
            ranked[draw_rank[m.members[0]]] = Some(i);
        }
        let mut dest = 0;
        for rank in 0..ranked.len() {
            let Some(src) = ranked[rank].take() else {
                continue;
            };
            if src != dest {
                // The feature displaced from `dest` now lives at `src`.
                ranked[draw_rank[feats[dest].members[0]]] = Some(src);
                feats.swap(dest, src);
            }
            dest += 1;
        }
    };
    // #758: from the second pass on, every member's polygons are projected,
    // quantized and cleaned once, so a pass only unions, and only the
    // components that changed since the pass before. A tile that fits at
    // once never builds this. Below the pair cap, interactions are cached;
    // above it, candidates are swept on demand. Parts are tested only when
    // a pass puts both in one feature: on NZ Parcels under a 2 MB gzip cap,
    // where most oversized tiles fit on their second pass, testing every
    // touching pair up front cost 15% of the export.
    // Built once, on the first pass that merges; `Some(None)` only when the
    // part count cannot fit the cache's u32 indices.
    let mut quantized: Option<Option<crate::mvt::QuantizedPolygons>> = None;
    let mut build = |feats: &[&coalesce_valve::Merged<'_>], pass: u32, fill: &[&FillFeature]| {
        // Pass 1 merges nothing, and a pass that still merges nothing (a
        // tile with no mergeable gaps, on its way to the drop fallback) has
        // nothing to reuse.
        if pass == 1 || feats.iter().all(|m| m.members.len() < 2) {
            return build_tile_layers(gzip_cache, layers, feats, tb, opts, fill, diagnostics);
        }
        let cached = quantized.get_or_insert_with(|| {
            crate::mvt::QuantizedPolygons::new(
                ordered.iter().map(|m| (&*m.geom, u64::from(m.band))),
                tb,
                opts.extent,
            )
        });
        let Some(q) = cached.as_mut() else {
            return build_tile_layers(gzip_cache, layers, feats, tb, opts, fill, diagnostics);
        };
        q.next_pass();
        let q = &*q;
        build_tile_layers_with(gzip_cache, layers, feats, opts, fill, |m| {
            q.encode(&m.members, diagnostics).unwrap_or_else(|| {
                EncodedFeatureGeometry::encode(m.geometry(), tb, opts.extent, diagnostics)
            })
        })
    };
    let mut gaps: Option<Vec<u64>> = None;
    let mut mingap = 0u64;
    let mut keep_fraction = 1.0_f64;
    let mut prev_fit: Option<f64> = None;
    let mut stalls = 0u32;
    let mut passes = 0u32;
    loop {
        passes += 1;
        let merged = coalesce_valve::coalesce_members(&ordered, mingap);
        let absorbed: usize = merged.iter().map(|m| m.absorbed).sum();
        let mut feats: Vec<&coalesce_valve::Merged<'_>> = merged.iter().collect();
        draw_order(&mut feats);
        let mut mvt = build(&feats, passes, &[]);
        let fit = tile_fit(&mut mvt, opts).unwrap_or(f64::INFINITY);
        let tally = ValveTally {
            merged: absorbed,
            dropped: 0,
            passes,
            mingap,
            over_limit: usize::from(fit < 1.0),
            ..ValveTally::default()
        };
        if fit >= 1.0 {
            let fitted = TileMvt {
                oversized: passes > 1,
                valve: tally,
                ..mvt
            };
            let mut fitted =
                with_fill(fitted, opts, fill, fill_tally, |f| build(&feats, passes, f));
            if absorbed > 0 && opts.feature_id.is_some() {
                account_merged_sources(&merged, &feats, &ordered, &mut fitted);
            }
            return fitted;
        }
        // tippecanoe's `mingap_fraction`: aim for the share of features
        // that would fit, with a 20% margin, never keeping more than 80%.
        keep_fraction = (keep_fraction * fit * 0.8).min(0.8);
        // #758: a tile that shrinks too slowly to fit within a few more
        // passes, twice in a row, merges everything it can in one step. One
        // slow pass is not enough: gzip barely rewards a pass that merges
        // only a few features, so a tile just over a compressed cap can look
        // stalled once and still fit a pass later.
        // DIVERGENCE FROM TIPPECANOE: tippecanoe keeps raising the threshold
        // one quantile at a time until it fits or runs out of gaps.
        stalls = if prev_fit.is_some_and(|p| stalled(p, fit)) {
            stalls + 1
        } else {
            0
        };
        if stalls >= 2 {
            keep_fraction = 0.0;
        }
        prev_fit = Some(fit);
        let g = gaps.get_or_insert_with(|| coalesce_valve::mergeable_gaps(&ordered));
        if let Some(next) = coalesce_valve::next_mingap(g, keep_fraction, mingap) {
            mingap = next;
            continue;
        }
        // Merged as far as the gaps allow and still over.
        // DIVERGENCE FROM TIPPECANOE: tippecanoe exits ("Can't increase
        // feature gap threshold further"); we finish the tile.
        let (whole, single): (Vec<_>, Vec<_>) = feats.iter().copied().partition(|m| m.absorbed > 0);
        let mut kept = feats;
        let mut dropped = 0;
        let mut out = mvt;
        let mut fit = fit;
        if !single.is_empty() {
            // Drop unmerged features until it fits: fewest vertices first,
            // points thinned evenly (`select_kept_members`).
            // With merged features in the tile, every single may go before
            // merged coverage is touched; alone, the largest single stays.
            let min_keep = usize::from(whole.is_empty());
            let mut keep = single.len();
            for cut in 0..MAX_DROP_PASSES {
                // Nothing left to drop: go straight to the backstop.
                if keep <= min_keep {
                    break;
                }
                keep = drop_keep(keep, fit, cut).min(keep - 1).max(min_keep);
                kept = whole.clone();
                if keep > 0 {
                    kept.extend(select_kept_members(&single, keep).into_iter().copied());
                }
                draw_order(&mut kept);
                passes += 1;
                // The previous bytes cannot be returned after dropping more
                // features. Release them before building their replacement.
                drop(out);
                out = build(&kept, passes, &[]);
                fit = tile_fit(&mut out, opts).unwrap_or(f64::INFINITY);
                if fit >= 1.0 || keep == 0 {
                    break;
                }
            }
            // The cuts ran out with singles left beside merged features: the
            // rest go too, before the extent drops or a merge is touched.
            if fit < 1.0 && !whole.is_empty() && kept.len() > whole.len() {
                kept = whole.clone();
                draw_order(&mut kept);
                passes += 1;
                drop(out);
                out = build(&kept, passes, &[]);
                fit = tile_fit(&mut out, opts).unwrap_or(f64::INFINITY);
            }
            dropped = single.len() - (kept.len() - whole.len());
        }
        let mut tally = ValveTally {
            dropped,
            passes,
            over_limit: usize::from(fit < 1.0),
            ..tally
        };
        if fit < 1.0 {
            out = reduce_merged_to_fit(
                gzip_cache,
                &mut kept,
                tb,
                (opts, opts.layers),
                diagnostics,
                out,
                &mut tally,
            );
        }
        log::debug!(
            "oversized tile: merged {absorbed} of {} features and dropped {} sources \
             ({} merged features carrying {} sources){}",
            members.len(),
            tally.dropped,
            tally.dropped_merged,
            tally.dropped_merged_sources,
            if tally.over_limit > 0 {
                "; single remaining feature still over the limit, written as is"
            } else {
                ""
            },
        );
        let fitted = TileMvt {
            oversized: true,
            valve: tally,
            ..out
        };
        let mut fitted = with_fill(fitted, opts, fill, fill_tally, |f| {
            build(&kept, tally.passes, f)
        });
        if opts.feature_id.is_some() {
            account_merged_sources(&merged, &kept, &ordered, &mut fitted);
        }
        return fitted;
    }
}

/// Re-encode the exhausted merge valve's survivors on coarser grids, then
/// drop whole features at the floor. Only a lone feature may stay over.
fn reduce_merged_to_fit(
    gzip_cache: &Arc<TileGzipCache>,
    kept: &mut Vec<&coalesce_valve::Merged<'_>>,
    tb: &TileBounds,
    (opts, layers): (&ExportOptions, &[(u16, String)]),
    diagnostics: &crate::diagnostics::Diagnostics,
    mut out: TileMvt,
    tally: &mut ValveTally,
) -> TileMvt {
    // DIVERGENCE FROM TIPPECANOE: tile.cpp:2946 exits when coalescing
    // exhausts gaps. Its detail loop (3031) runs only without an as-needed
    // strategy and reaches 128. We reduce detail after merging, to 512,
    // then drop whole features. A lone feature still over the cap at that
    // floor is kept with a warning, without splitting it into its sources.
    let mut reduced = opts.clone();
    let mut fit = tile_fit(&mut out, opts).unwrap_or(f64::INFINITY);
    // A coarser grid only moves points; a point tile is dropped as the drop
    // valve drops it.
    let points_only = kept
        .iter()
        .all(|m| matches!(m.geometry(), Geometry::Point(_) | Geometry::MultiPoint(_)));
    while fit < 1.0 && reduced.extent > MIN_VALVE_EXTENT && !points_only {
        reduced.extent = (reduced.extent >> 1).max(MIN_VALVE_EXTENT);
        tally.passes += 1;
        drop(out);
        // Plain encode quantizes, unions and cleans geometry on this grid.
        // Never reuse the original grid's QuantizedPolygons or fill.
        out = build_tile_layers(gzip_cache, layers, kept, tb, &reduced, &[], diagnostics);
        fit = tile_fit(&mut out, &reduced).unwrap_or(f64::INFINITY);
    }
    while fit < 1.0 && kept.len() > 1 {
        let keep = drop_keep(kept.len(), fit, 1).clamp(1, kept.len() - 1);
        let selected: HashSet<_> = select_kept_members(kept, keep)
            .iter()
            .map(|m| m.members[0])
            .collect();
        // Restrict the existing draw_order to its survivors, including
        // feature-order columns, on every encode used to measure the cap.
        kept.retain(|m| {
            if selected.contains(&m.members[0]) {
                return true;
            }
            tally.dropped += m.members.len();
            // Its absorbed sources are lost, not merged: count each once.
            tally.merged -= m.absorbed;
            if m.absorbed > 0 {
                tally.dropped_merged += 1;
                tally.dropped_merged_sources += m.members.len();
            }
            false
        });
        tally.passes += 1;
        drop(out);
        out = build_tile_layers(gzip_cache, layers, kept, tb, &reduced, &[], diagnostics);
        fit = tile_fit(&mut out, &reduced).unwrap_or(f64::INFINITY);
    }
    tally.over_limit = usize::from(fit < 1.0);
    tally.over_limit_merged = fit < 1.0 && kept.len() == 1 && kept[0].absorbed > 0;
    tally.reduced_extent = (reduced.extent < opts.extent).then_some(reduced.extent);
    if let Some(extent) = tally.reduced_extent {
        log::debug!("tile bounds {tb:?} written at extent {extent}");
    }
    out
}

/// The merge valve's per-source loss owner, after the final encode (including
/// fill). The encoder tracks targets; propagate their encode losses to absorbed
/// sources, otherwise classify kept merges as merged and dropped features'
/// sources as valve-dropped, whether or not the backstop ran.
fn account_merged_sources(
    merged: &[coalesce_valve::Merged<'_>],
    kept: &[&coalesce_valve::Merged<'_>],
    ordered: &[&Member],
    out: &mut TileMvt,
) {
    use crate::diagnostics::SourceLossCause;
    let retained: HashSet<_> = kept.iter().map(|m| m.members[0]).collect();
    let losses: HashMap<_, _> = out.source_losses.iter().copied().collect();
    for m in merged {
        let retained = retained.contains(&m.members[0]);
        let cause = if retained {
            losses
                .get(&m.target.seq)
                .copied()
                .unwrap_or(SourceLossCause::Merged)
        } else {
            SourceLossCause::ValveDropped
        };
        for &i in &m.members[usize::from(retained)..] {
            out.source_losses.push((ordered[i].seq, cause));
        }
    }
}

/// The survivors of the oversized-tile valve, **in the order they are drawn**.
///
/// Two separate decisions, deliberately kept apart: [`select_kept_members`]
/// decides *which* features survive, ranking sized geometry by vertex count;
/// this then restores the caller's draw order over them. Emitting the
/// selection ranking as the draw order would silently override a pinned
/// `--feature-order` — and only on oversized tiles, i.e. exactly the dense
/// ones a caller pins an order for. The default [`FeatureOrder::Input`] keeps
/// the vertex-count order, which is the pre-#280 behaviour and byte-identical
/// to it.
fn shed_to_fit<'a>(members: &'a [Member], keep: usize, opts: &ExportOptions) -> Vec<&'a Member> {
    let mut kept = select_kept_members(members, keep);
    if let FeatureOrder::Column { name, descending } = &opts.feature_order {
        kept.sort_by(|a, b| compare_by_feature_order(a, b, name, *descending));
    }
    kept
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
///   across member order. Overview rows are in the source file's order, which
///   for gpio-optimized input is Hilbert order, so a tile's members arrive in
///   space-filling-curve order; an even stride then spreads the survivors
///   across the tile — the same spatially uniform thinning tippecanoe
///   achieves by dropping every Nth feature in sequence.
///
/// `keep` is clamped to `1..=members.len()`.
fn select_kept_members<M: TileFeature>(members: &[M], keep: usize) -> Vec<&M> {
    select_kept_by_size(members, keep, is_point_like(members), |m| {
        m.geometry().coords_count()
    })
}

/// Shared drop ranking for overview members and decoded joined-tile features.
/// The latter have geometry commands rather than source coordinates.
pub(crate) fn select_kept_by_size<M>(
    members: &[M],
    keep: usize,
    point_like: bool,
    size: impl Fn(&M) -> usize,
) -> Vec<&M> {
    let n = members.len();
    let keep = keep.clamp(1, n);
    if point_like {
        // Evenly spaced indices 0, n/keep, 2n/keep, … — strictly increasing and
        // in-bounds because keep ≤ n.
        (0..keep).map(|i| &members[i * n / keep]).collect()
    } else {
        let mut ranked: Vec<&M> = members.iter().collect();
        ranked.sort_by_key(|m| std::cmp::Reverse(size(m)));
        ranked.truncate(keep);
        ranked
    }
}

/// Vertices at which [`build_mvt`] encodes a tile's geometries in parallel
/// (#647). Coarse zooms hold a few tiles with thousands of members each (a
/// single z3 tile of the Madagascar fixture is 489 KB), and the per-tile
/// parallel section in [`encode_members`] runs each of those on one thread.
///
/// The encode cost follows the vertex count, not the member count, so the
/// threshold counts vertices. On an idle 16-core pool the parallel encode
/// broke even at about 500–1,000 vertices: 528 vertices took 26 µs serial
/// and 24 µs parallel, and 1,028 took 38 µs and 27 µs. At 4,096 vertices it
/// was 1.4–3.5× faster. The threshold sits well above break-even because
/// in production the other cores are often busy with other tiles.
const PARALLEL_ENCODE_MIN_COORDS: usize = 4096;

/// Whether [`build_mvt`] encodes these members' geometries in parallel: two
/// or more members holding at least [`PARALLEL_ENCODE_MIN_COORDS`] vertices.
/// The count stops at the threshold, so a small tile costs one short scan.
fn encode_in_parallel<M: TileFeature>(members: &[M]) -> bool {
    if members.len() < 2 {
        return false;
    }
    let mut coords = 0usize;
    for m in members {
        coords += m.coords_count();
        if coords >= PARALLEL_ENCODE_MIN_COORDS {
            return true;
        }
    }
    false
}

/// Build the MVT bytes for a sequence of tile members.
///
/// The geometry encode (projection, quantization, ring cleaning) is pure per
/// member, so on a tile that [`encode_in_parallel`] selects it runs in
/// parallel. Tag interning stays serial, in member order, which keeps the
/// key/value tables and the bytes identical to the serial loop. A smaller
/// tile encodes and interns each member in turn, with no buffer.
fn build_mvt<M: TileFeature>(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[M],
    tb: &TileBounds,
    opts: &ExportOptions,
    fill: &[&FillFeature],
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    build_mvt_with(gzip_cache, members, opts, fill, |m| {
        EncodedFeatureGeometry::encode(m.geometry(), tb, opts.extent, diagnostics)
    })
}

// One encoder for both valves. The ordinary export takes the original build
// path; only a joint retry supplies a band-to-layer mapping.
fn build_tile_layers<M: TileFeature>(
    cache: &Arc<TileGzipCache>,
    layers: &[(u16, String)],
    members: &[M],
    tb: &TileBounds,
    opts: &ExportOptions,
    fill: &[&FillFeature],
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    if layers.is_empty() {
        return build_mvt(cache, members, tb, opts, fill, diagnostics);
    }
    build_tile_layers_with(cache, layers, members, opts, fill, |m| {
        EncodedFeatureGeometry::encode(m.geometry(), tb, opts.extent, diagnostics)
    })
}

fn build_tile_layers_with<M: TileFeature>(
    cache: &Arc<TileGzipCache>,
    layers: &[(u16, String)],
    members: &[M],
    opts: &ExportOptions,
    fill: &[&FillFeature],
    encode: impl Fn(&M) -> EncodedFeatureGeometry + Sync,
) -> TileMvt {
    if layers.is_empty() {
        return build_mvt_with(cache, members, opts, fill, encode);
    }
    debug_assert!(fill.is_empty());
    let mut out = build_mvt_with(cache, &[] as &[M], opts, &[], &encode);
    let mut bytes = Vec::new();
    // Group in one member walk, preserving both the first occurrence of
    // each layer and the draw order within it. Rescanning all members for
    // every layer, with a band-list search per member, was quadratic in
    // the number of layers on every valve pass.
    let mut grouped: Vec<(&String, Vec<&M>)> = Vec::new();
    let mut bands = BTreeMap::new();
    for (band, name) in layers {
        let index = grouped
            .iter()
            .position(|(n, _)| *n == name)
            .unwrap_or_else(|| {
                grouped.push((name, Vec::new()));
                grouped.len() - 1
            });
        bands.insert(*band, index);
    }
    for member in members {
        if let Some(&index) = bands.get(&member.band()) {
            grouped[index].1.push(member);
        }
    }
    grouped.retain(|(_, members)| !members.is_empty());
    // Layers encode independently; the joint valve re-encodes all of them on
    // every pass, so build them in parallel and join in layer order.
    let built: Vec<TileMvt> = grouped
        .into_par_iter()
        .map(|(name, selected)| {
            let layer_opts = ExportOptions {
                layer_name: name.clone(),
                tile_format: TileFormat::Mvt,
                ..opts.clone()
            };
            build_mvt_with(cache, &selected, &layer_opts, &[], |m| encode(*m))
        })
        .collect();
    for layer in built {
        bytes.extend_from_slice(&layer.data);
        out.features += layer.features;
        out.unencodable += layer.unencodable;
        out.quantized += layer.quantized;
        out.source_ids.extend(layer.source_ids);
        out.source_losses.extend(layer.source_losses);
    }
    let (data, error) =
        match crate::tile_format::finish_tile(bytes, opts.tile_format, opts.mlt_profile) {
            Ok(data) => (data, None),
            Err(e) => (Vec::new(), Some(e)),
        };
    out.data = data.into();
    out.mlt_error = error;
    out
}

/// [`build_mvt`] with each feature's geometry encoded by `encode`, which must
/// return what [`EncodedFeatureGeometry::encode`] would (#758: the merge
/// valve's cached encode).
fn build_mvt_with<M: TileFeature>(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[M],
    opts: &ExportOptions,
    fill: &[&FillFeature],
    encode: impl Fn(&M) -> EncodedFeatureGeometry + Sync,
) -> TileMvt {
    build_mvt_within(gzip_cache, members, opts, fill, encode, usize::MAX)
        .expect("no limit to exceed")
}

/// [`build_mvt_with`], given up (`None`) as soon as the layer's encoded
/// features and tables pass `raw_limit` bytes: the tile can only grow, so
/// it is already over a raw cap of that size (#803's probe).
fn build_mvt_within<M: TileFeature>(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[M],
    opts: &ExportOptions,
    fill: &[&FillFeature],
    encode: impl Fn(&M) -> EncodedFeatureGeometry + Sync,
    raw_limit: usize,
) -> Option<TileMvt> {
    // The MVT's size says nothing of the MLT's, so only an MVT build can
    // give up early; an MLT tile is measured after conversion.
    let raw_limit = match opts.tile_format {
        TileFormat::Mvt => raw_limit,
        _ => usize::MAX,
    };
    let mut source_ids = Vec::new();
    let mut source_losses = Vec::new();
    let mut track = |id: Option<u64>, geom: &EncodedFeatureGeometry| {
        use crate::diagnostics::SourceLossCause;
        if let Some(id) = id {
            match geom {
                EncodedFeatureGeometry::Parts(_) => source_ids.push(id),
                EncodedFeatureGeometry::Unencodable => {
                    source_losses.push((id, SourceLossCause::EncodeDropped))
                }
                EncodedFeatureGeometry::Quantized => {
                    source_losses.push((id, SourceLossCause::Quantized))
                }
            }
        }
    };
    let mut layer = crate::mvt::StreamingLayerBuilder::new(&opts.layer_name, opts.extent);
    // #750: the coverage fill comes first, so a renderer draws it under the
    // tile's own features. Each fill feature carries its host's id and tags.
    for f in fill {
        let geom = EncodedFeatureGeometry::Parts(vec![(f.cmds.clone(), GeomType::Polygon)]);
        track(f.id.map(|_| f.source_row), &geom);
        layer.add_borrowed_feature(f.id, geom, f.props.iter());
    }
    if encode_in_parallel(members) {
        // Bound the geometry staging buffer as well as the serialized layer.
        // Intern and emit each chunk in original member order; only geometry
        // projection runs in parallel, exactly as before.
        const GEOMETRY_CHUNK_MEMBERS: usize = 4096;
        for chunk in members.chunks(GEOMETRY_CHUNK_MEMBERS) {
            let encoded: Vec<EncodedFeatureGeometry> = chunk.par_iter().map(&encode).collect();
            for (m, geom) in chunk.iter().zip(encoded) {
                track(m.feature_id().map(|_| m.source_row()), &geom);
                layer.add_borrowed_feature(m.feature_id(), geom, m.properties().iter());
            }
            if layer.encoded_len() > raw_limit {
                return None;
            }
        }
    } else {
        for m in members {
            let geom = encode(m);
            track(m.feature_id().map(|_| m.source_row()), &geom);
            layer.add_borrowed_feature(m.feature_id(), geom, m.properties().iter());
            if layer.encoded_len() > raw_limit {
                return None;
            }
        }
    }
    // The fill is drawn but is not the data's own features: the report,
    // the tilestats and the empty-tile check count members only (#750).
    let features = layer.feature_count() - fill.len();
    let unencodable = layer.dropped_features();
    let quantized = layer.quantized_features();
    let (data, mlt_error) =
        match crate::tile_format::finish_tile(layer.build(), opts.tile_format, opts.mlt_profile) {
            Ok(data) => (data, None),
            Err(e) => (Vec::new(), Some(e)),
        };
    Some(TileMvt {
        gzip_cache: Arc::clone(gzip_cache),
        source_ids,
        source_losses,
        data: data.into(),
        features,
        unencodable,
        quantized,
        oversized: false,
        valve: ValveTally::default(),
        mlt_error,
    })
}

// ============================================================================
// Reading + property extraction
// ============================================================================

/// Test-support: the pre-partitioning in-memory reference path — build every
/// member for a fully materialized feature slice at `zoom` (unbounded key
/// range) and encode via the production member machinery.
///
/// #443: resolves `opts.feature_id` directly against `f.props` (there is no
/// Arrow schema on this path), stripping the named property out of the
/// member's tags exactly as the production Arrow path does -- so this
/// exercises the same "moved, not copied" contract with the same error types.
#[cfg(test)]
fn test_gzip_cache() -> Arc<TileGzipCache> {
    Arc::new(TileGzipCache::new(8 * 1024 * 1024))
}

#[cfg(test)]
fn encode_level_tiles(
    features: &[Feature],
    zoom: u8,
    opts: &ExportOptions,
) -> Result<Vec<EncodedTile>, ExportError> {
    let mut members = Vec::new();
    let mut gaps = RowGapCursor::new(vec![(0, features.len() as u64)]);
    for (fi, f) in features.iter().enumerate() {
        let seq = fi as u64;
        let id = match &opts.feature_id {
            None => None,
            Some(name) => Some(feature_id::feature_id_from_property(
                find_property_value(&f.props, name),
                name,
                zoom,
                seq,
            )?),
        };
        let props: SharedProps = SharedProps::new(
            f.props
                .iter()
                .filter(|(n, _)| opts.feature_id.as_ref() != Some(n))
                .map(|(n, v)| (Arc::from(n.as_str()), v.clone()))
                .collect(),
        );
        // #745: the slice stands in for one row group of one band.
        let (gap, band) = gaps.next_row(f.geom.bounding_rect());
        for (key, geom) in feature_tile_members(
            &f.geom,
            zoom,
            opts,
            0,
            u64::MAX,
            &crate::diagnostics::Diagnostics::default(),
        ) {
            members.push(Member {
                key,
                seq,
                geom: geom.into(),
                props: props.clone(),
                id,
                gap,
                band,
            });
        }
    }
    // `encode_members` returns gzip-COMPRESSED tile bytes (issue #227 moved
    // compression into the parallel encode section). This helper exists to
    // exercise MVT *encoding*, so decompress each tile back to raw MVT bytes;
    // the assertions in the tests below operate on the uncompressed payload.
    let mut tiles = encode_members(
        &test_gzip_cache(),
        members,
        zoom,
        opts,
        None,
        &ExportTimers::default(),
        &crate::diagnostics::Diagnostics::default(),
    )
    .expect("in-memory gzip is infallible");
    // #431: all-dropped carriers hold no tile bytes; the writer skips them too.
    tiles.retain(|t| t.feature_count > 0);
    for t in &mut tiles {
        t.data = crate::compression::decompress_capped(
            &t.data,
            Compression::Gzip,
            crate::compression::MAX_TILE_BYTES,
        )
        .expect("gzip roundtrip of just-compressed tile");
    }
    Ok(tiles)
}

/// Find a named property's value in an unindexed `(name, value)` list, for
/// the [`encode_level_tiles`] test-support path (no Arrow column to read).
#[cfg(test)]
fn find_property_value<'a>(
    props: &'a [(String, PropertyValue)],
    name: &str,
) -> Option<&'a PropertyValue> {
    props.iter().find(|(n, _)| n == name).map(|(_, v)| v)
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
        let batch = batch.map_err(|e| read_error(e.into()))?;
        decode_batch(&batch, geometry_name(reader), crs, &mut out)?;
    }
    Ok(out)
}

/// Test-support: decode one record batch into [`Feature`]s, excluding the
/// `level` column and any struct/list column (the bbox covering) from
/// properties.
#[cfg(test)]
fn decode_batch(
    batch: &RecordBatch,
    geometry: Option<&str>,
    crs: Crs,
    out: &mut Vec<Feature>,
) -> Result<(), ExportError> {
    let schema = batch.schema();
    let geom_idx = batch_geometry_index(batch, geometry)?;
    let geom_field = schema.field(geom_idx).clone();

    // Decode geometries.
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::GeoParquetReadError::decode("geometry decode", e))?;
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(batch.num_rows());
    extract_geometries_from_array(
        garr.as_ref(),
        &mut geoms,
        &crate::diagnostics::Diagnostics::default(),
    )?;

    // Pre-extract every exportable property column once.
    let prop_cols = property_columns(&schema, geom_idx, &PublishedNames::identity());
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

/// The index of the overview file's geometry column in the file's schema
/// ([`OverviewReader::schema`], which carries the footer's `geo` key).
///
/// Resolved exactly as convert resolves its input and as
/// [`crate::resolved_schema::ResolvedSchema::geometry_name`] resolves the footer for
/// pruning (#518): the `geo` metadata's `primary_column`, then the name
/// heuristic (`geometry`, else the first `geom*`). The overview writer pins
/// `primary_column` to convert's geometry column, so a column named `SHAPE`
/// (an Esri file geodatabase) resolves by name (#675). Choosing by `geoarrow.*`
/// extension type instead would pick the first of several geometry columns
/// in schema order, which is not necessarily the primary one.
#[cfg(test)]
fn geometry_index(schema: &Schema) -> Option<usize> {
    crate::resolved_schema::ResolvedSchema::resolve(schema)
        .ok()?
        .geometry_index()
}

/// The other geometry column the name heuristic alone would pick, when it
/// differs from `geom_idx` ([`crate::resolved_schema::ResolvedSchema::geometry_index`]).
///
/// Overview files converted before #675 left `primary_column` to the
/// geoparquet crate, which names the alphabetically first geometry column,
/// so an input with `geom` and a `centroid` beside it was written declaring
/// `centroid`. Export used to take `geom` by name and now follows the footer;
/// only a re-convert fixes such a file, so the export says so.
#[cfg(test)]
fn geometry_name_disagreement(schema: &Schema, geom_idx: usize) -> Option<&str> {
    crate::resolved_schema::geometry_name_disagreement(schema, geom_idx)
}

/// The name of the overview file's geometry column ([`crate::resolved_schema::ResolvedSchema::geometry_index`]).
fn geometry_name(reader: &OverviewReader) -> Option<&str> {
    reader.resolved_schema().geometry_name()
}

/// The index of the geometry column `geometry` ([`geometry_name`]) in a batch
/// read from the overview file.
///
/// [`crate::resolved_schema::ResolvedSchema::geometry_index`] cannot resolve a batch: the Parquet reader builds each
/// batch's schema from its columns alone, so the file's `geo` key, and with
/// it `primary_column`, is gone, and the name heuristic would take an
/// attribute such as `geom_type` over a geometry column named `SHAPE`. Field
/// names survive in every batch, projected ones included.
fn batch_geometry_index(batch: &RecordBatch, geometry: Option<&str>) -> Result<usize, ExportError> {
    geometry
        .and_then(|name| batch.schema().index_of(name).ok())
        .ok_or(ExportError::NoGeometryColumn { path: None })
}

/// The MVT property key each overview-file column is published under (#359).
///
/// A source column that collided with a reserved overview column was renamed
/// on the way into the overview GeoParquet (`level` → `level_`, #288), because
/// in *that* file the reserved column is authoritative and both must coexist.
/// The PMTiles export carries no `level` property — it is asserted out of MVT
/// — so by tile-writing time the source name is free again and the reason for
/// the move has lapsed. Publishing the internal name instead hands the caller
/// a property their data never had, and a style keyed on the real name paints
/// nothing, with no error to point at.
///
/// Restoration is conditional, not automatic: a rename is undone only when its
/// source name is not already published by another column. `point_count` and
/// `coalesced_count` *are* real MVT properties in the modes that append them,
/// unless withheld (#379), so a column moved aside from one of those keeps the
/// renamed name — merging two columns into one key would be worse than the
/// wrong name.
///
/// It also carries the *suppressed* columns (#379): the `coalesced_count`
/// provenance counter the converter appends whenever line coalescing is on
/// (the default), when it never left 1 because nothing was merged (a polygon
/// layer through the coalescer, say). A column that is 1 on every row is not
/// a property of the data; in the tiles it is schema noise a style editor
/// shows as a real attribute, and a tag pair per feature. The overview file
/// keeps the column — its validator relies on it — this only decides what
/// the MVT carries. Whether the counter ever exceeded 1 is read from
/// row-group statistics, so the check costs nothing and a file whose stats
/// are missing keeps the column (nothing is known). `point_count` is never
/// withheld: `--cluster` is an explicit opt-in whose documented contract is
/// the column (a style scales symbols by it), and a schema that depends on
/// whether anything happened to cluster would break such styles. Suppression
/// is decided *before* restoration, so a withheld counter's name is free for
/// a source column that was moved aside from it.
#[derive(Debug, Clone, Default)]
struct PublishedNames {
    /// Schema field name → published key, for the columns that differ.
    /// Empty for the identity mapping, which is the overwhelmingly common
    /// case (nothing collided, or the file predates the provenance).
    restored: HashMap<String, String>,
    /// Schema field names that are not exported at all (#379).
    suppressed: HashSet<String>,
    /// The bbox covering column the file's `geo` metadata declares for the
    /// geometry column, when it declares one. A covering is never a
    /// property (see [`super::writer::is_covering_column`]); this is the declared spelling,
    /// alongside the default names and the shape rule.
    covering: Option<String>,
    /// The geometry column's name ([`geometry_name`]), resolved once from the
    /// file schema for the batches read from it ([`batch_geometry_index`]).
    geometry: Option<String>,
    properties: Option<HashSet<usize>>,
    /// The canonical level's index and its own view of the names, which also
    /// withholds the `coalesced_count` counter: that level never coalesces,
    /// so the counter is 1 on every one of its rows by construction (spec
    /// §13.2). Decided from the footer alone, so every build (sharded or
    /// not, at any thread count) withholds it from the same tiles.
    canonical: Option<(usize, Box<PublishedNames>)>,
}

impl PublishedNames {
    /// Every column keeps its schema name.
    #[cfg(test)]
    fn identity() -> Self {
        Self::default()
    }

    /// Resolve publication names for `schema` from a file's `renamed_columns`
    /// provenance (output name → source name; see [`Generalization`]), with
    /// `suppressed` the schema columns the export withholds (#379).
    #[cfg(test)]
    fn from_renames(
        renames: &BTreeMap<String, String>,
        schema: &Schema,
        suppressed: HashSet<String>,
    ) -> Self {
        let restored = crate::resolved_schema::publication_names(schema, renames, &suppressed);
        for (renamed, source) in &restored {
            log::info!(
                "[export] publishing overview column {renamed:?} under its \
                 source name {source:?} (it was renamed on convert to clear \
                 the reserved overview column, #288/#359)"
            );
        }
        Self {
            restored,
            suppressed,
            covering: None,
            geometry: None,
            properties: None,
            canonical: None,
        }
    }

    /// Build from an open overview file: suppression of a `coalesced_count`
    /// counter that never left 1 (#379), then the rename mapping from the
    /// footer — in that order, so a withheld counter's name counts as free.
    /// `point_count` is deliberately not considered (see the type doc).
    fn from_reader(reader: &OverviewReader) -> Self {
        let meta = reader.meta();
        let mut suppressed = HashSet::new();
        let coalescing = meta
            .generalization
            .as_ref()
            .and_then(|g| g.coalescing.as_ref())
            // The footer names the column, but only the converter's own
            // counter is eligible: a tampered or foreign footer must not be
            // able to withhold a real data column.
            .filter(|c| c.coalesced_count_column == COALESCED_COUNT_COLUMN);
        if let Some(c) = coalescing {
            let column = c.coalesced_count_column.as_str();
            // #541 review: prefer the converter's own record of whether any
            // chain merged, which describes the whole conversion. The
            // row-group statistic only sees the levels this file holds, so a
            // zoom-capped coarse job (whose merges all happen finer than the
            // ceiling) would withhold a column the monolithic build publishes,
            // and the two builds' coarse tiles would differ. Files written
            // before the flag existed fall back to the statistic.
            let never_merged = match c.merged {
                Some(merged) => !merged,
                None => reader.int_column_max(column) == Some(1),
            };
            if never_merged {
                log::info!(
                    "[export] not exporting {column:?}: it is 1 on every row \
                     (no lines were merged), so it says nothing about the data; \
                     the overview file keeps it (#379)"
                );
                suppressed.insert(column.to_string());
            }
        }
        let descriptor = reader.resolved_schema();
        let names = Self {
            restored: descriptor.publication_names(&suppressed),
            suppressed,
            covering: descriptor
                .covering()
                .and_then(|c| c.xmin_path.first().cloned()),
            geometry: descriptor.geometry_name().map(str::to_owned),
            properties: Some(descriptor.properties().iter().copied().collect()),
            canonical: None,
        };
        names
    }

    /// Withhold the converter's `coalesced_count` from the canonical level's
    /// tiles, where it is 1 on every row (#379 scoped per level). The column
    /// stays at the coarser levels whenever any chain merged. A caller who
    /// names the counter in `--include-property` keeps it everywhere; a
    /// `--feature-order` on it is unaffected, since a key missing from every
    /// member of a tile ranks them all equal, as all-1 values did. A capped
    /// coarse job's file (`zoom_ceiling`) holds no canonical level, so it
    /// withholds nothing here.
    fn withhold_counter_at_canonical(
        &mut self,
        meta: &OverviewsMeta,
        selection: &PropertySelection,
    ) {
        let column = COALESCED_COUNT_COLUMN;
        let coalesced = meta.generalization.as_ref().is_some_and(|g| {
            g.zoom_ceiling.is_none()
                && g.coalescing
                    .as_ref()
                    .is_some_and(|c| c.coalesced_count_column == column)
        });
        let Some(canonical) = meta.canonical_level.and_then(|c| usize::try_from(c).ok()) else {
            return;
        };
        let published = self.publish(column);
        let named = selection
            .include
            .as_ref()
            .is_some_and(|names| names.iter().any(|n| n == published));
        if !coalesced || named || self.is_suppressed(column) {
            return;
        }
        let mut view = self.clone();
        view.suppress(column);
        self.canonical = Some((canonical, Box::new(view)));
    }

    /// The names level `level_idx`'s features are published under.
    fn at_level(&self, level_idx: usize) -> &Self {
        match &self.canonical {
            Some((canonical, view)) if *canonical == level_idx => view,
            _ => self,
        }
    }

    /// Apply a caller's property selection (#386) on top of the file-derived
    /// naming: every exportable property whose *published* name the
    /// selection drops is withheld. Names are matched as the tile would
    /// carry them, which is what a style author has in front of them, so a
    /// column restored from a reserved-column rename is addressed by its
    /// restored name. An `include` naming no exportable property is an
    /// error; excluding the `--feature-order` column is too, since the sort
    /// would then have nothing to read.
    ///
    /// An explicit `include` wins over the #379 heuristic: a counter the
    /// file-derived naming withheld because it never left 1 is exported
    /// after all when the caller names it, since the column is plainly in
    /// the file and asking for it is not a typo.
    fn with_selection(
        mut self,
        schema: &Schema,
        geom_idx: usize,
        selection: &PropertySelection,
        feature_order: &FeatureOrder,
    ) -> Result<Self, ExportError> {
        if selection.is_identity() {
            return Ok(self);
        }
        if let Some(include) = &selection.include {
            let exported: Vec<String> = property_columns(schema, geom_idx, &self)
                .into_iter()
                .map(|(_, n)| n)
                .collect();
            for name in include {
                if exported.contains(name) {
                    continue;
                }
                // Suppression is keyed by schema name; the include list
                // speaks published names, so map through the rename table.
                let withheld = schema
                    .fields()
                    .iter()
                    .map(|f| f.name())
                    .find(|n| self.is_suppressed(n) && self.publish(n) == name)
                    .cloned();
                if let Some(schema_name) = withheld {
                    log::info!(
                        "[export] exporting {schema_name:?} after all: it was withheld \
                         (#379) but --include-property names it"
                    );
                    self.suppressed.remove(&schema_name);
                }
            }
        }
        let exportable: Vec<(usize, String)> = property_columns(schema, geom_idx, &self);
        let published: Vec<&str> = exportable.iter().map(|(_, n)| n.as_str()).collect();
        if let Some(include) = &selection.include {
            for name in include {
                if !published.contains(&name.as_str()) {
                    // #434: a column that IS in the file but has no MVT
                    // encoding gets its own message; "unknown" would send
                    // the caller looking for a typo that is not there.
                    // Scanned over the same candidates `property_columns`
                    // draws from, so the bbox covering struct (index
                    // metadata, never a property) falls through to its own
                    // check below rather than being called an unencodable
                    // struct.
                    if let Some((_, f)) = candidate_columns(schema, geom_idx, &self)
                        .find(|(_, f)| self.publish(f.name()) == name)
                    {
                        return Err(ExportError::UnsupportedProperty {
                            name: name.clone(),
                            data_type: format!("{:?}", f.data_type()),
                        });
                    }
                    // #401: the covering is in the file but is never a
                    // property; say so, as the convert side does.
                    let geom = schema.fields().get(geom_idx).map(|g| g.name().as_str());
                    if schema.fields().iter().any(|f| {
                        f.name() == name
                            && super::writer::is_covering_column(f, geom, self.covering.as_deref())
                    }) {
                        return Err(ExportError::CoveringProperty { name: name.clone() });
                    }
                    return Err(ExportError::UnknownProperty {
                        name: name.clone(),
                        available: published
                            .iter()
                            .map(|n| format!("{n:?}"))
                            .collect::<Vec<_>>()
                            .join(", "),
                    });
                }
            }
        }
        // Every column the selection can address, exportable or not (#434):
        // excluding a binary column the export would otherwise drop with a
        // warning is a real choice, not a no-op, and silences that warning.
        let candidates: Vec<(usize, String)> = candidate_columns(schema, geom_idx, &self)
            .map(|(i, f)| (i, self.publish(f.name()).to_string()))
            .collect();
        for name in &selection.exclude {
            if !candidates.iter().any(|(_, n)| n == name) {
                log::warn!(
                    "[export] excluded property {name:?} is not exported by this file anyway"
                );
            }
        }
        if let FeatureOrder::Column { name, .. } = feature_order {
            if published.contains(&name.as_str()) && !selection.keeps(name) {
                return Err(ExportError::PropertyRequiredByKnob {
                    name: name.clone(),
                    knob: "--feature-order".to_string(),
                });
            }
        }
        let mut dropped = 0usize;
        for (idx, name) in &candidates {
            if !selection.keeps(name) {
                self.suppressed.insert(schema.field(*idx).name().clone());
                if exportable.iter().any(|(i, _)| i == idx) {
                    dropped += 1;
                }
            }
        }
        let kept: Vec<&str> = published
            .iter()
            .copied()
            .filter(|n| selection.keeps(n))
            .collect();
        log::info!(
            "[export] property selection: keeping {} of {} ({}), dropping {dropped}",
            kept.len(),
            published.len(),
            if kept.is_empty() {
                "none — geometry only".to_string()
            } else {
                kept.iter()
                    .map(|n| format!("{n:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
        Ok(self)
    }

    /// Whether a schema column is withheld from the tiles entirely.
    fn is_suppressed(&self, schema_name: &str) -> bool {
        self.suppressed.contains(schema_name)
    }

    /// Withhold a schema column from the tiles entirely (#443: a
    /// `--feature-id` column is moved to the feature id and must never also
    /// publish as a regular property, so this is applied unconditionally
    /// after resolving it -- regardless of what the ordinary property
    /// selection already decided for that column).
    fn suppress(&mut self, schema_name: &str) {
        self.suppressed.insert(schema_name.to_string());
    }

    /// The MVT key for a schema field name.
    fn publish<'a>(&'a self, schema_name: &'a str) -> &'a str {
        self.restored
            .get(schema_name)
            .map_or(schema_name, String::as_str)
    }
}

/// The `(index, published name)` of every exportable property column:
/// everything that is not the geometry column, not the `level` column, not
/// the geometry's bbox covering struct, and whose type the export can encode
/// ([`is_exportable`]: MVT scalars directly, struct/list/map columns as JSON
/// strings). The index addresses the real schema column; the name is what the
/// tile advertises, which differs when a rename is restored ([`PublishedNames`]).
fn property_columns(
    schema: &Schema,
    geom_idx: usize,
    published: &PublishedNames,
) -> Vec<(usize, String)> {
    candidate_columns(schema, geom_idx, published)
        .filter(|&(_, f)| is_exportable(f.data_type()))
        .map(|(i, f)| (i, published.publish(f.name()).to_string()))
        .collect()
}

/// The property columns the export drops because their type has no MVT
/// encoding (#434), in schema order. Everything [`property_columns`] would
/// consider and [`is_exportable`] rejects; a column the selection already
/// withheld is not "dropped", it was asked for, so it is not listed.
fn skipped_property_columns(
    schema: &Schema,
    geom_idx: usize,
    published: &PublishedNames,
) -> Vec<SkippedPropertyColumn> {
    candidate_columns(schema, geom_idx, published)
        .filter(|&(_, f)| !is_exportable(f.data_type()))
        .map(|(_, f)| SkippedPropertyColumn {
            name: published.publish(f.name()).to_string(),
            data_type: format!("{:?}", f.data_type()),
        })
        .collect()
}

/// Every schema column that is a property candidate: not the geometry, not
/// the `level` column, not a bbox covering struct ([`super::writer::is_covering_column`]:
/// index metadata, not user data -- so it is neither exported nor warned
/// about), and not withheld by the selection.
fn candidate_columns<'a>(
    schema: &'a Schema,
    geom_idx: usize,
    published: &'a PublishedNames,
) -> impl Iterator<Item = (usize, &'a arrow_schema::Field)> + 'a {
    let geom = schema.fields().get(geom_idx).map(|g| g.name().as_str());
    schema
        .fields()
        .iter()
        .enumerate()
        .filter(move |&(i, f)| {
            published.properties.as_ref().map_or(
                i != geom_idx
                    && !f.name().eq_ignore_ascii_case(LEVEL_COLUMN)
                    && !super::writer::is_covering_column(f, geom, published.covering.as_deref()),
                |indices| indices.contains(&i),
            ) && !published.is_suppressed(f.name())
        })
        .map(|(i, f)| (i, f.as_ref()))
}

/// The columns pass 2 attaches to each member: the exportable properties
/// ([`property_columns`]) and the resolved `--feature-id` column (#443).
struct AttrColumns<'a> {
    names: PropertyNames,
    /// `(column index, published name)` in schema order.
    props: Vec<(usize, Arc<str>)>,
    feature_id: Option<&'a ResolvedFeatureId>,
}

impl<'a> AttrColumns<'a> {
    fn new(
        schema: &Schema,
        geom_idx: usize,
        published: &PublishedNames,
        feature_id: Option<&'a ResolvedFeatureId>,
    ) -> Self {
        let props: Vec<_> = property_columns(schema, geom_idx, published)
            .into_iter()
            .map(|(idx, name)| (idx, Arc::<str>::from(name)))
            .collect();
        let names: PropertyNames = props
            .iter()
            .map(|(_, n)| n.clone())
            .collect::<Vec<_>>()
            .into();
        Self {
            props,
            names,
            feature_id,
        }
    }

    /// Extract every column of one batch once, for per-row materialization.
    ///
    /// PERF (#803): the batch gets its own copy of the schema table. Cloning
    /// the process-wide `Arc` per row and member would make every pool thread
    /// bump one refcount cache line (~5% more cycles at equal instructions,
    /// #874). `SpillNames` dedupes copies by content. Values are written
    /// directly into rows.
    fn extract(&self, batch: &RecordBatch) -> Result<BatchAttributes<'a>, ExportError> {
        Ok(BatchAttributes {
            names: self.names.iter().cloned().collect::<Vec<_>>().into(),
            rows: packed_props::arrow::extract(
                self.props
                    .iter()
                    .map(|(idx, _)| batch.column(*idx).as_ref()),
                batch.num_rows(),
            )?,
            ids: self.feature_id.map(|fid| {
                let ids = feature_id_values(batch.column(fid.idx))
                    .map(|v| v.into_iter().map(Result::ok).collect())
                    .unwrap_or_default();
                (fid, ids)
            }),
            #[cfg(debug_assertions)]
            read: vec![false; batch.num_rows()],
        })
    }
}

/// One batch's extracted [`AttrColumns`].
struct BatchAttributes<'a> {
    names: PropertyNames,
    rows: Vec<packed_props::arrow::Row>,
    /// The `--feature-id` column and its per-row ids (`None` = invalid).
    ids: Option<(&'a ResolvedFeatureId, Vec<Option<u64>>)>,
    /// Rows [`Self::row`] has already moved out (debug builds only).
    #[cfg(debug_assertions)]
    read: Vec<bool>,
}

impl BatchAttributes<'_> {
    /// Row `row`'s `--feature-id`, if one is set.
    fn id(&self, row: usize) -> Result<Option<u64>, ExportError> {
        Ok(match &self.ids {
            None => None,
            // `validate_feature_id_column` already vouched for every row of
            // the file, so this only fires if the file changed underneath
            // the export.
            Some((fid, ids)) => Some(ids.get(row).copied().flatten().ok_or_else(|| {
                ExportError::Arrow(arrow_schema::ArrowError::InvalidArgumentError(format!(
                    "--feature-id column {:?} holds an invalid value that the up-front check \
                     did not see; was the overview file modified during the export?",
                    fid.name
                )))
            })?),
        })
    }

    /// Row `row`'s properties, shared across its members via `Arc`.
    ///
    /// Moves the already-filled row buffer into immutable shared storage.
    /// Call once per row; debug builds catch a second call.
    fn props(&mut self, row: usize) -> SharedProps {
        #[cfg(debug_assertions)]
        assert!(
            !std::mem::replace(&mut self.read[row], true),
            "BatchAttributes::props({row}) read twice: its values were already moved out"
        );
        #[cfg(test)]
        PROPS_BUILT.with(|n| n.set(n.get() + 1));
        std::mem::take(&mut self.rows[row]).finish(self.names.clone())
    }
}

#[cfg(test)]
thread_local! {
    /// Rows whose properties [`BatchAttributes::props`] built on this thread
    /// (test counter).
    static PROPS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// MVT-encodable Arrow scalar types.
///
/// MVT's value union is string/float/double/int/uint/bool — it has no temporal
/// type and no decimal. That is a reason to *encode* those, not to drop them:
/// a dropped column is silent data loss, and a time column is usually the axis
/// the map is read along. Temporal types render as ISO 8601 strings (the
/// tippecanoe convention) and decimals as doubles; see
/// [`packed_props::arrow`].
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
    ) || is_temporal_scalar(dt)
        || matches!(dt, DataType::Decimal128(_, _) | DataType::Decimal256(_, _))
}

/// Arrow types that reach the tile as a JSON string (#434): struct, list,
/// large list, fixed-size list and map columns. tippecanoe does the same for
/// nested GeoJSON attributes -- `stringify_value` in `read_json.cpp` turns
/// any object or array into an `mvt_string` holding its `json_stringify`
/// text -- and it is what the Overture `names` struct and `sources` list
/// need to survive the export at all. The nested leaves are rendered by
/// the direct Arrow writer's scalar rules; a leaf type without one
/// (binary, an unrenderable timezone) renders as JSON `null` rather than
/// dropping the whole column.
/// Whether a (possibly dictionary-encoded) type renders as a JSON string,
/// i.e. can key a JSON object.
fn is_string_type(dt: &DataType) -> bool {
    match dt {
        DataType::Utf8 | DataType::LargeUtf8 => true,
        DataType::Dictionary(_, value) => is_string_type(value),
        _ => false,
    }
}

fn is_json_encoded(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Struct(_)
            | DataType::List(_)
            | DataType::LargeList(_)
            | DataType::FixedSizeList(_, _)
            | DataType::Map(_, _)
    )
}

/// Whether a column of this type is exported as a property: an MVT scalar
/// ([`is_supported_scalar`]), a JSON-encoded nested type
/// ([`is_json_encoded`]), or a dictionary over either (unwrapped on
/// extraction). [`property_columns`], [`field_metadata`] and
/// the direct Arrow writer all gate on this one predicate, so a column
/// advertised in `vector_layers.fields` is always one the tiles can fill.
fn is_exportable(dt: &DataType) -> bool {
    match dt {
        DataType::Dictionary(_, value) => is_exportable(value),
        dt => is_supported_scalar(dt) || is_json_encoded(dt),
    }
}

/// The `vector_layers.fields` type name for an exportable column, or `None`
/// for one the export drops.
fn field_type_name(dt: &DataType) -> Option<&'static str> {
    match dt {
        DataType::Utf8 | DataType::LargeUtf8 => Some("String"),
        DataType::Boolean => Some("Boolean"),
        DataType::Dictionary(_, value) => field_type_name(value),
        // Temporal values reach the tile as ISO 8601 text and nested values
        // as JSON text, so the field must be advertised as what a client
        // will actually read.
        dt if is_temporal_scalar(dt) || is_json_encoded(dt) => Some("String"),
        dt if is_supported_scalar(dt) => Some("Number"),
        _ => None,
    }
}

/// Arrow types rendered as ISO 8601 strings rather than numbers.
///
/// Rendering is delegated to `arrow_cast`'s own formatter, so the text matches
/// what every other Arrow consumer shows for the same value instead of a
/// hand-rolled variant of it.
fn is_temporal_scalar(dt: &DataType) -> bool {
    match dt {
        DataType::Date32 | DataType::Date64 | DataType::Time32(_) | DataType::Time64(_) => true,
        // A timezone is only supported if we can render it -- see
        // `utc_equivalent_tz`. Claiming otherwise would advertise the field in
        // `vector_layers.fields` and then emit null for every value, which is
        // worse than dropping the column, because the archive then promises a
        // field that is not on a single feature.
        DataType::Timestamp(_, tz) => tz.as_ref().is_none_or(|tz| utc_equivalent_tz(tz)),
        _ => false,
    }
}

/// Whether a timestamp's timezone is one we can render without a tz database.
///
/// `arrow-array` only understands named zones when its (implicit, undocumented)
/// `chrono-tz` feature is on, which pulls in the whole IANA database; without
/// it `ArrayFormatter::try_new` fails outright for anything but a numeric
/// offset. That matters because parquet's `TIMESTAMP` carries a single
/// `isAdjustedToUTC` bool, which the parquet crate maps to literally
/// `Some("UTC")` -- so the tz-aware case is the COMMON one, not an exotic one,
/// and dropping it would take the time axis with it.
///
/// Every timestamp reaching the export comes from an overview parquet file, so
/// the tz is always `None` or a UTC spelling. Those we normalise to the fixed
/// offset `+00:00`, which arrow renders identically (`...Z`) with no extra
/// dependency. A genuine named zone cannot arrive by that route; if one ever
/// does it is not advertised and not encoded, exactly as before this change.
fn utc_equivalent_tz(tz: &str) -> bool {
    matches!(
        tz.trim(),
        "UTC" | "utc" | "Utc" | "Z" | "z" | "+00:00" | "-00:00" | "00:00"
    )
}

/// The datatype to format a temporal array as: a UTC-equivalent timestamp is
/// restated with a numeric offset so arrow will render it without `chrono-tz`.
fn formattable_temporal_type(dt: &DataType) -> DataType {
    match dt {
        DataType::Timestamp(unit, Some(tz)) if utc_equivalent_tz(tz) => {
            DataType::Timestamp(*unit, Some("+00:00".into()))
        }
        other => other.clone(),
    }
}

/// Field-type metadata (`name -> "String"|"Number"|"Boolean"`) for the archive
/// `vector_layers.fields` block.
fn field_metadata(
    schema: &Schema,
    geom_idx: Option<usize>,
    published: &PublishedNames,
) -> HashMap<String, String> {
    let geom_idx = geom_idx.unwrap_or(usize::MAX);
    candidate_columns(schema, geom_idx, published)
        .filter_map(|(_, f)| {
            field_type_name(f.data_type())
                .map(|ty| (published.publish(f.name()).to_string(), ty.to_string()))
        })
        .collect()
}

/// Extract one Arrow column into per-row optional [`PropertyValue`]s. Null cells
/// and unsupported types yield `None`.
#[cfg(test)]
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
        // Decimals are numbers, so scale them out rather than drop the column.
        //
        // An unscaled decimal (scale <= 0) is an integer, and `DECIMAL(38,0)` is
        // a common carrier for IDs and parcel numbers -- routing one through f64
        // would silently round it past 53 bits, turning an ID into a different
        // ID. Those go out as an MVT int when they fit one. Everything else
        // becomes a double, the only MVT numeric wide enough; past 53 bits of
        // significand that loses precision (an `i128 as f64` rounds -- it is far
        // too small to reach infinity), which is the same loss every other f64
        // consumer takes and still beats dropping the value.
        DataType::Decimal128(_, scale) => {
            let a = col.as_primitive::<Decimal128Type>();
            let scale = i32::from(*scale);
            let f = 10f64.powi(scale);
            (0..n)
                .map(|i| {
                    (!a.is_null(i)).then(|| {
                        let raw = a.value(i);
                        let exact = (scale <= 0)
                            .then(|| {
                                10i128
                                    .checked_pow((-scale) as u32)
                                    .and_then(|m| raw.checked_mul(m))
                            })
                            .flatten()
                            .and_then(|v| i64::try_from(v).ok());
                        match exact {
                            Some(v) => PropertyValue::Int(v),
                            None => PropertyValue::Double(raw as f64 / f),
                        }
                    })
                })
                .collect()
        }
        // 256-bit decimals do not fit an i128 fast path, so let arrow render
        // the scaled text and parse that. Slower per value, but this is a rare
        // type and arrow already owns the scaling rules.
        DataType::Decimal256(_, _) => {
            let opts = FormatOptions::default();
            match ArrayFormatter::try_new(col, &opts) {
                Ok(fmt) => (0..n)
                    .map(|i| {
                        // Arrow's decimal display is always `-?\d+(\.\d+)?`, so
                        // the parse does not fail in practice. If it ever did,
                        // drop the value rather than manufacture a NaN: a NaN
                        // encodes into the tile fine but renders any downstream
                        // tile-to-GeoJSON step invalid.
                        (!col.is_null(i))
                            .then(|| fmt.value(i).to_string().parse().ok())
                            .flatten()
                            .map(PropertyValue::Double)
                    })
                    .collect(),
                Err(_) => vec![None; n],
            }
        }
        // MVT has no temporal type. Render with arrow_cast's own formatter so
        // the text matches what every other Arrow consumer shows, instead of
        // dropping the column and taking the time axis with it.
        //
        // DIVERGENCE FROM TIPPECANOE: tippecanoe has no opinion here -- it
        // passes through whatever text its input already held, which for a
        // DuckDB-written parquet is `2026-09-14 12:34:56`. We emit arrow's
        // rendering, `2026-09-14T12:34:56` (or `...Z` when the column is
        // UTC-stamped): ISO 8601, unambiguous about the separator, and
        // identical to what `tylertoo decode` and every other Arrow reader will
        // show for the same value. Only the tz-stamped form carries an offset,
        // so only that one is also RFC 3339. Matching the space form would mean
        // re-implementing arrow's formatter to be less correct.
        //
        // `Date32` renders as `2026-09-14` and `Date64` as `2026-09-14T00:00:00`
        // -- arrow's own distinction, kept deliberately. Truncating `Date64`
        // would discard a time component the type is allowed to carry, and
        // parquet's DATE always decodes to `Date32`, so the split is unreachable
        // from the tiling path anyway.
        dt if is_temporal_scalar(dt) => {
            // Nulls are filtered below, so `FormatOptions`' null rendering is
            // never consulted.
            let opts = FormatOptions::default();
            let retyped: Option<arrow_array::ArrayRef> = match formattable_temporal_type(dt) {
                t if t == *dt => None,
                t => col
                    .to_data()
                    .into_builder()
                    .data_type(t)
                    .build()
                    .ok()
                    .map(arrow_array::make_array),
            };
            let target: &dyn Array = retyped.as_deref().unwrap_or(col);
            let out = match ArrayFormatter::try_new(target, &opts) {
                Ok(fmt) => (0..n)
                    .map(|i| {
                        (!col.is_null(i)).then(|| PropertyValue::String(fmt.value(i).to_string()))
                    })
                    .collect(),
                // Unreachable: `is_temporal_scalar` only admits types arrow can
                // format, and `field_metadata` gates on the same predicate, so
                // an un-formattable column is never selected or advertised.
                Err(e) => {
                    log::warn!("cannot format {dt:?} column as text, dropping it: {e}");
                    vec![None; n]
                }
            };
            out
        }
        // A dictionary is an encoding, not a type: unwrap it to its value
        // type (one `arrow_cast` pass over the batch, cheap) and extract that.
        // `is_exportable` admits a dictionary only over an exportable value
        // type, so the cast cannot land on the fallthrough below.
        DataType::Dictionary(_, value) => match arrow_cast::cast(col, value) {
            Ok(flat) => extract_property_column(flat.as_ref()),
            Err(e) => {
                log::warn!(
                    "cannot unwrap {:?} column, dropping it: {e}",
                    col.data_type()
                );
                vec![None; n]
            }
        },
        // #434: nested columns as JSON strings, the tippecanoe convention
        // (see `is_json_encoded`). One string per non-null row; a null row
        // stays null (no property), as for every scalar column.
        dt if is_json_encoded(dt) => json_property_column(col),
        _ => vec![None; n],
    }
}

// ============================================================================
// Nested properties as JSON (#434)
// ============================================================================

/// Render every row of a struct/list/map column as a JSON string.
///
/// The column is first resolved into a [`JsonNode`] tree -- one node per
/// nested Arrow array, with every scalar leaf extracted ONCE for the whole
/// batch through [`extract_property_column`] -- and then each row is written
/// straight into one reusable byte buffer. So the per-row cost is the write
/// itself: no intermediate `serde_json::Value` tree, no per-value
/// formatter, and exactly one `String` allocation per non-null row.
#[cfg(test)]
fn json_property_column(col: &dyn Array) -> Vec<Option<PropertyValue>> {
    let node = JsonNode::resolve(col);
    let mut buf = Vec::with_capacity(64);
    (0..col.len())
        .map(|i| {
            if col.is_null(i) {
                return None;
            }
            buf.clear();
            node.write(&mut buf, i);
            // Only `serde_json` and ASCII punctuation ever wrote into `buf`,
            // so it is valid UTF-8; the checked conversion is belt and braces.
            String::from_utf8(buf.clone())
                .ok()
                .map(PropertyValue::String)
        })
        .collect()
}

/// A list or map column's value offsets rebased to its first row, and the
/// span of its child values its rows address. A sliced column (#740's
/// per-chunk extraction) keeps its whole child array, so resolving only the
/// span keeps a chunk's work proportional to its own rows.
#[cfg(test)]
fn rebased_offsets<O: arrow_array::OffsetSizeTrait>(
    offsets: &[O],
) -> (Vec<usize>, std::ops::Range<usize>) {
    let first = offsets.first().map_or(0, |o| o.as_usize());
    let last = offsets.last().map_or(first, |o| o.as_usize());
    (
        offsets.iter().map(|o| o.as_usize() - first).collect(),
        first..last,
    )
}

/// One nested Arrow array, ready to be written row by row as JSON.
#[cfg(test)]
enum JsonNode {
    /// A scalar leaf, already extracted for the whole array. An unsupported
    /// leaf type extracts to all-`None`, which renders as `null` per value.
    Leaf(Vec<Option<PropertyValue>>),
    /// A struct: its field names and one node per child, indexed by the
    /// same row.
    Struct {
        array: arrow_array::StructArray,
        fields: Vec<(String, JsonNode)>,
    },
    /// A variable-length list: `offsets[i]..offsets[i + 1]` of `values`.
    List {
        offsets: Vec<usize>,
        array: arrow_array::ArrayRef,
        values: Box<JsonNode>,
    },
    /// A map: `offsets[i]..offsets[i + 1]` of `(key, value)` pairs. A
    /// string key TYPE makes a JSON object; any other key type makes an
    /// array of two-element arrays, since JSON object keys can only be
    /// strings. Decided once per column from the type (`string_keys`), so
    /// every row and every batch agree on the shape.
    Map {
        offsets: Vec<usize>,
        array: arrow_array::ArrayRef,
        string_keys: bool,
        keys: Box<JsonNode>,
        values: Box<JsonNode>,
    },
}

#[cfg(test)]
impl JsonNode {
    fn resolve(col: &dyn Array) -> Self {
        match col.data_type() {
            DataType::Struct(_) => {
                let array = col.as_struct().clone();
                let fields = array
                    .fields()
                    .iter()
                    .zip(array.columns())
                    .map(|(f, child)| (f.name().clone(), JsonNode::resolve(child.as_ref())))
                    .collect();
                JsonNode::Struct { array, fields }
            }
            DataType::List(_) => {
                let a = col.as_list::<i32>();
                let (offsets, span) = rebased_offsets(a.value_offsets());
                JsonNode::List {
                    offsets,
                    array: arrow_array::make_array(col.to_data()),
                    values: Box::new(JsonNode::resolve(
                        a.values().slice(span.start, span.len()).as_ref(),
                    )),
                }
            }
            DataType::LargeList(_) => {
                let a = col.as_list::<i64>();
                let (offsets, span) = rebased_offsets(a.value_offsets());
                JsonNode::List {
                    offsets,
                    array: arrow_array::make_array(col.to_data()),
                    values: Box::new(JsonNode::resolve(
                        a.values().slice(span.start, span.len()).as_ref(),
                    )),
                }
            }
            DataType::FixedSizeList(_, _) => {
                let a = col.as_fixed_size_list();
                let width = usize::try_from(a.value_length()).unwrap_or(0);
                JsonNode::List {
                    offsets: (0..=a.len()).map(|i| i * width).collect(),
                    array: arrow_array::make_array(col.to_data()),
                    values: Box::new(JsonNode::resolve(a.values().as_ref())),
                }
            }
            DataType::Map(_, _) => {
                let a = col.as_map();
                let (offsets, span) = rebased_offsets(a.value_offsets());
                JsonNode::Map {
                    offsets,
                    array: arrow_array::make_array(col.to_data()),
                    // Decided once from the key TYPE, not from the values:
                    // every row of the batch (and every batch of the column)
                    // then takes the same shape, and a key type with no
                    // rendering (binary) cannot masquerade as strings.
                    string_keys: is_string_type(a.keys().data_type()),
                    keys: Box::new(JsonNode::resolve(
                        a.keys().slice(span.start, span.len()).as_ref(),
                    )),
                    values: Box::new(JsonNode::resolve(
                        a.values().slice(span.start, span.len()).as_ref(),
                    )),
                }
            }
            // Unwrapped HERE, not through the leaf's own dictionary arm: a
            // dictionary over a struct or list must resolve into a nested
            // node, or its values would be JSON-stringified as a column of
            // their own and then embedded as string literals (double-encoded).
            DataType::Dictionary(_, value) => match arrow_cast::cast(col, value) {
                Ok(flat) => JsonNode::resolve(flat.as_ref()),
                Err(_) => JsonNode::Leaf(vec![None; col.len()]),
            },
            _ => JsonNode::Leaf(extract_property_column(col)),
        }
    }

    /// Whether row `i` of this node is null (renders as `null`).
    fn is_null(&self, i: usize) -> bool {
        match self {
            JsonNode::Leaf(values) => values.get(i).is_none_or(Option::is_none),
            JsonNode::Struct { array, .. } => array.is_null(i),
            JsonNode::List { array, .. } | JsonNode::Map { array, .. } => array.is_null(i),
        }
    }

    /// Write row `i` as JSON. `serde_json` does every string escape and
    /// number rendering (a non-finite float becomes `null`, as `serde_json`
    /// renders it everywhere); only the punctuation is written by hand.
    fn write(&self, out: &mut Vec<u8>, i: usize) {
        if self.is_null(i) {
            out.extend_from_slice(b"null");
            return;
        }
        match self {
            JsonNode::Leaf(values) => {
                // `is_null` above already returned for a `None`.
                let value = values[i].as_ref().expect("checked non-null");
                write_json_scalar(out, value);
            }
            JsonNode::Struct { fields, .. } => {
                out.push(b'{');
                for (k, (name, child)) in fields.iter().enumerate() {
                    if k > 0 {
                        out.push(b',');
                    }
                    write_json_str(out, name);
                    out.push(b':');
                    child.write(out, i);
                }
                out.push(b'}');
            }
            JsonNode::List {
                offsets, values, ..
            } => {
                out.push(b'[');
                for (k, j) in (offsets[i]..offsets[i + 1]).enumerate() {
                    if k > 0 {
                        out.push(b',');
                    }
                    values.write(out, j);
                }
                out.push(b']');
            }
            JsonNode::Map {
                offsets,
                string_keys,
                keys,
                values,
                ..
            } => {
                let string_keys = *string_keys;
                out.push(if string_keys { b'{' } else { b'[' });
                for (k, j) in (offsets[i]..offsets[i + 1]).enumerate() {
                    if k > 0 {
                        out.push(b',');
                    }
                    if string_keys {
                        // Arrow forbids null map keys; if one ever arrives,
                        // a quoted "null" keeps the object valid JSON.
                        if keys.is_null(j) {
                            write_json_str(out, "null");
                        } else {
                            keys.write(out, j);
                        }
                        out.push(b':');
                        values.write(out, j);
                    } else {
                        out.push(b'[');
                        keys.write(out, j);
                        out.push(b',');
                        values.write(out, j);
                        out.push(b']');
                    }
                }
                out.push(if string_keys { b'}' } else { b']' });
            }
        }
    }
}

/// Write one scalar as JSON.
#[cfg(test)]
fn write_json_scalar(out: &mut Vec<u8>, value: &PropertyValue) {
    // `serde_json::to_writer` into a `Vec` cannot fail: the writer is
    // infallible and every variant here is serializable.
    let _ = match value {
        PropertyValue::String(s) => serde_json::to_writer(&mut *out, s),
        PropertyValue::Bool(b) => serde_json::to_writer(&mut *out, b),
        PropertyValue::Int(v) => serde_json::to_writer(&mut *out, v),
        PropertyValue::UInt(v) => serde_json::to_writer(&mut *out, v),
        PropertyValue::Float(v) => serde_json::to_writer(&mut *out, v),
        PropertyValue::Double(v) => serde_json::to_writer(&mut *out, v),
    };
}

/// Write one string as a JSON string literal.
fn write_json_str(out: &mut Vec<u8>, s: &str) {
    let _ = serde_json::to_writer(&mut *out, s);
}

// ============================================================================
// CRS
// ============================================================================

/// Detect the overview file's CRS. A null/absent CRS is GeoParquet's default
/// EPSG:4326. EPSG:3857 is accepted (reprojected on read); anything else errors.
///
/// Classification is [`crate::quality::classify_crs_info`], the single shared
/// classifier — NOT a local substring match (#519). The hand-rolled
/// `identifier.contains("3857")` this replaces never saw a PROJJSON blob
/// whose `id` is structured rather than spelled out in the string, which is
/// exactly what an overview converted from a Web Mercator input now declares.
#[cfg(test)]
fn detect_crs(
    path: &Path,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Crs, ExportError> {
    let file = File::open(path).map_err(crate::Error::Io)?;
    let metadata = tylertoo_io::read_parquet_metadata(&file).map_err(|e| {
        crate::Error::from(crate::GeoParquetReadError::Schema {
            message: e.to_string(),
        })
    })?;
    let descriptor = crate::resolved_schema::ResolvedSchema::from_metadata(&metadata)?;
    crs_from_descriptor(&descriptor, path, diagnostics)
}

fn crs_from_descriptor(
    descriptor: &crate::resolved_schema::ResolvedSchema,
    path: &Path,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Crs, ExportError> {
    use crate::quality::CrsKind;
    descriptor.record_diagnostics(diagnostics);
    let info = descriptor.crs_info().clone();
    match crate::quality::classify_crs_info(&info) {
        CrsKind::Wgs84 => Ok(Crs::Epsg4326),
        CrsKind::WebMercator => Ok(Crs::Epsg3857),
        // A null CRS (identifier None, name None) is the GeoParquet default
        // 4326 — the long-standing export default, kept deliberately.
        CrsKind::Unknown if info.identifier.is_none() && info.name.is_none() => Ok(Crs::Epsg4326),
        CrsKind::Unknown => Err(ExportError::UnsupportedCrs {
            crs: info
                .identifier
                .or(info.name)
                .unwrap_or_else(|| "unknown".to_string()),
            path: path.display().to_string(),
        }),
    }
}

/// Reproject one EPSG:3857 point (meters) to EPSG:4326 (lon/lat degrees).
#[inline]
pub(crate) fn webmerc_to_lnglat(x: f64, y: f64) -> (f64, f64) {
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
mod tests;

#[cfg(test)]
mod diagnostic_row_alignment {
    use super::*;

    /// A kept merge's absorbed rows have the same cause whether dropping
    /// singles fits the tile or the extent backstop must finish it.
    #[test]
    fn merge_valve_source_causes_do_not_depend_on_extent_backstop() {
        use crate::diagnostics::{Diagnostics, SourceLossCause};

        let members: Vec<_> = [
            Geometry::LineString(geo::LineString::from(vec![(-10., 0.), (10., 0.)])),
            Geometry::LineString(geo::LineString::from(vec![(-10., 0.), (10., 0.)])),
            Geometry::Point(geo::Point::new(0., 0.)),
            Geometry::Point(geo::Point::new(0., 0.)),
        ]
        .into_iter()
        .enumerate()
        .map(|(row, geom)| Member {
            key: 0,
            seq: row as u64,
            geom: geom.into(),
            props: SharedProps::new(vec![]),
            id: Some(100 + row as u64),
            gap: if row == 0 { GAP_NONE } else { 1 },
            band: 0,
        })
        .collect();
        let mut opts = ExportOptions {
            feature_id: Some("id".into()),
            size_valve: SizeValve::Merge,
            compressed_tile_size_limit: None,
            ..Default::default()
        };
        let cache = test_gzip_cache();
        let tb = TileCoord::new(0, 0, 0).bounds();
        let ordered: Vec<_> = members.iter().collect();
        let merged = coalesce_valve::coalesce_members(&ordered, 2);
        // Exactly enough room for the merged lines and one unmerged point.
        let singles_limit = build_mvt(
            &cache,
            &merged[..2],
            &tb,
            &opts,
            &[],
            &Diagnostics::default(),
        )
        .data
        .len();
        let merged_limit = build_mvt(&cache, &merged, &tb, &opts, &[], &Diagnostics::default())
            .data
            .len();
        for (limit, dropped, backstop) in [
            (merged_limit, 0, false),
            (1, 2, true),
            (singles_limit, 1, false),
        ] {
            opts.tile_size_limit = Some(limit);
            let diagnostics = Diagnostics::default();
            let mvt = encode_tile(
                &cache,
                &members,
                &tb,
                &opts,
                &[],
                &mut FillTally::default(),
                &diagnostics,
            );
            assert_eq!(mvt.valve.reduced_extent.is_some(), backstop);
            assert_eq!(mvt.valve.merged, 1);
            assert_eq!(mvt.valve.dropped, dropped);
            assert!(mvt.source_ids.contains(&0), "merged feature is kept");
            account_sources(&members, &mvt, 0, &opts, &diagnostics).unwrap();
            let losses = diagnostics.source_losses().unwrap();
            let actual: Vec<_> = losses
                .iter()
                .map(|loss| {
                    let loss = loss.unwrap();
                    assert_eq!(loss.feature_id, Some(100 + loss.id));
                    (loss.id, loss.cause)
                })
                .collect();
            let mut expected = vec![(1, SourceLossCause::Merged)];
            if dropped == 2 {
                expected.push((2, SourceLossCause::ValveDropped));
            }
            if dropped > 0 {
                expected.push((3, SourceLossCause::ValveDropped));
            }
            assert_eq!(actual, expected, "backstop={backstop}");
            assert_eq!(
                mvt.source_losses, expected,
                "valve records each source's cause"
            );
        }
    }

    #[test]
    fn mutation_regression_source_loss_valve_causes() {
        use crate::diagnostics::{Diagnostics, SourceLossCause};

        let line = || Geometry::LineString(geo::LineString::from(vec![(-10., 0.), (10., 0.)]));
        let point = || Geometry::Point(geo::Point::new(0., 0.));
        let empty = || Geometry::GeometryCollection(geo::GeometryCollection::default());
        // With no valve loss, every lost row has an explicit encode cause.
        // That overrides the tile fallback, making `merged >= 0` equivalent
        // to `merged > 0` for the recorded losses in the zero/zero case.
        for (geoms, valve, limit, dropped, merged, expected) in [
            (
                vec![point(), empty()],
                SizeValve::Drop,
                None,
                0,
                0,
                vec![(1, SourceLossCause::EncodeDropped)],
            ),
            (
                vec![line(), line()],
                SizeValve::Merge,
                Some(1),
                0,
                1,
                vec![(1, SourceLossCause::Merged)],
            ),
            (
                vec![point(), point()],
                SizeValve::Drop,
                Some(1),
                1,
                0,
                vec![(1, SourceLossCause::ValveDropped)],
            ),
            (
                vec![line(), line(), point(), point()],
                SizeValve::Merge,
                Some(1),
                2,
                1,
                vec![
                    (1, SourceLossCause::Merged),
                    (2, SourceLossCause::ValveDropped),
                    (3, SourceLossCause::ValveDropped),
                ],
            ),
        ] {
            let members: Vec<_> = geoms
                .into_iter()
                .enumerate()
                .map(|(row, geom)| Member {
                    key: 0,
                    seq: row as u64,
                    geom: geom.into(),
                    props: SharedProps::new(vec![]),
                    id: Some(100 + row as u64),
                    gap: if row == 0 { GAP_NONE } else { 1 },
                    band: 0,
                })
                .collect();
            let opts = ExportOptions {
                feature_id: Some("id".into()),
                size_valve: valve,
                tile_size_limit: limit,
                compressed_tile_size_limit: None,
                ..Default::default()
            };
            let expected_features = members.len() - expected.len();
            let cache = test_gzip_cache();
            let mvt = encode_tile(
                &cache,
                &members,
                &TileCoord::new(0, 0, 0).bounds(),
                &opts,
                &[],
                &mut FillTally::default(),
                &Diagnostics::default(),
            );
            assert_eq!(
                (mvt.valve.dropped, mvt.valve.merged),
                (dropped, merged),
                "valve={:?}, features={}, bytes={}",
                mvt.valve,
                mvt.features,
                mvt.data.len()
            );
            let diagnostics = Diagnostics::default();
            let tiles = encode_members(
                &cache,
                members,
                0,
                &opts,
                None,
                &ExportTimers::default(),
                &diagnostics,
            )
            .unwrap();
            assert_eq!(tiles.len(), 1);
            assert_eq!(tiles[0].feature_count, expected_features);
            let losses = diagnostics.source_losses().unwrap();
            let actual: Vec<_> = losses
                .iter()
                .map(|loss| {
                    let loss = loss.unwrap();
                    assert_eq!(loss.feature_id, Some(100 + loss.id));
                    (loss.id, loss.cause)
                })
                .collect();
            assert_eq!(actual, expected, "dropped={dropped}, merged={merged}");
        }
    }

    #[test]
    fn duplicate_feature_ids_do_not_cancel_another_source_rows_loss() {
        let diagnostics = crate::diagnostics::Diagnostics::default();
        let geoms = vec![
            Geometry::Point(geo::Point::new(1.0, 1.0)),
            Geometry::Polygon(geo::Polygon::new(
                geo::LineString::from(vec![
                    (0.0, 0.0),
                    (0.000001, 0.0),
                    (0.000001, 0.000001),
                    (0.0, 0.0),
                ]),
                vec![],
            )),
        ];
        let members: Vec<Member> = geoms
            .into_iter()
            .enumerate()
            .map(|(row, geom)| {
                diagnostics
                    .source_member_loss(
                        row as u64,
                        7,
                        0,
                        crate::diagnostics::SourceLossCause::ClipDropped,
                    )
                    .unwrap();
                Member {
                    key: 0,
                    seq: row as u64,
                    geom: geom.into(),
                    id: Some(7),
                    props: SharedProps::new(vec![]),
                    gap: GAP_NONE,
                    band: 0,
                }
            })
            .collect();
        assert_eq!(members[0].source_row(), 0);
        let tiles = encode_members(
            &test_gzip_cache(),
            members,
            0,
            &ExportOptions {
                feature_id: Some("id".into()),
                tile_size_limit: None,
                compressed_tile_size_limit: None,
                ..Default::default()
            },
            None,
            &ExportTimers::default(),
            &diagnostics,
        )
        .unwrap();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].feature_count, 1);
        let losses = diagnostics.source_losses().unwrap();
        assert_eq!(losses.len(), 1);
        assert_eq!(losses.iter().next().unwrap().unwrap().id, 1);
        assert_eq!(losses.iter().next().unwrap().unwrap().feature_id, Some(7));
        assert_eq!(
            losses.iter().next().unwrap().unwrap().cause,
            crate::diagnostics::SourceLossCause::Quantized
        );
    }

    /// A tile whose own features all quantize away is not written, so a
    /// #750 fill drawn for it must not count its host as emitted: an
    /// emission cancels the host's recorded loss. `features` excludes the fill, while
    /// `source_ids` includes it, which is what the `features > 0` guard in
    /// `encode_members` separates.
    #[test]
    fn mutation_regression_fill_of_an_empty_tile_does_not_emit_its_host() {
        use crate::diagnostics::{Diagnostics, SourceLossCause};

        let triangle = Geometry::Polygon(geo::Polygon::new(
            geo::LineString::from(vec![
                (0.0, 0.0),
                (0.000001, 0.0),
                (0.000001, 0.000001),
                (0.0, 0.0),
            ]),
            vec![],
        ));
        let members = vec![Member {
            key: 0,
            seq: 0,
            geom: triangle.into(),
            props: SharedProps::new(vec![]),
            id: Some(7),
            gap: GAP_NONE,
            band: 0,
        }];
        let fills = coverage_fill::tests::test_fill_map([(
            0,
            0,
            vec![vec![vec![
                (0, 0),
                (1000, 0),
                (1000, 1000),
                (0, 1000),
                (0, 0),
            ]]],
        )]);
        let opts = ExportOptions {
            feature_id: Some("id".into()),
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..Default::default()
        };
        let diagnostics = Diagnostics::default();
        // The host was already lost upstream; an emission would cancel that.
        diagnostics
            .source_member_loss(0, 7, 0, SourceLossCause::ClipDropped)
            .unwrap();
        let tiles = encode_members(
            &test_gzip_cache(),
            members,
            0,
            &opts,
            Some(&fills),
            &ExportTimers::default(),
            &diagnostics,
        )
        .unwrap();
        assert!(tiles.iter().all(|t| t.feature_count == 0), "no own feature");
        let losses = diagnostics.source_losses().unwrap();
        let causes: Vec<_> = losses.iter().map(|l| l.unwrap().cause).collect();
        assert_eq!(causes, vec![SourceLossCause::ClipDropped]);
    }

    #[test]
    fn export_decode_keeps_null_geometry_rows_aligned_with_feature_ids() {
        let input = tempfile::NamedTempFile::new().unwrap();
        crate::overview::testutil::write_input(
            input.path(),
            &[None, Some(Geometry::Point(geo::Point::new(12.0, 34.0)))],
            true,
            None,
        );
        let source = crate::input::InputSource::from_path(input.path()).unwrap();
        let batch = source
            .open()
            .unwrap()
            .build()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let schema = batch.schema();
        let diagnostics = crate::diagnostics::Diagnostics::default();
        let geoms = decode_batch_geometries(
            &batch,
            &schema,
            geometry_index(&schema).unwrap(),
            Crs::Epsg4326,
            &ExportTimers::default(),
            &diagnostics,
        )
        .unwrap();
        assert_eq!(geoms.len(), 2, "null rows must retain their source ID slot");
        assert_eq!(geoms[0].coords_count(), 0);
        assert_eq!(geoms[1], Geometry::Point(geo::Point::new(12.0, 34.0)));
    }
}
