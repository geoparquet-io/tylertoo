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
use super::writer::LEVEL_COLUMN;
#[cfg(test)]
use crate::mvt::PropertyValue;
#[cfg(test)]
use arrow_array::types::{
    Decimal128Type, Float32Type, Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type, UInt32Type,
    UInt8Type,
};
use arrow_array::{
    cast::AsArray,
    types::{Float64Type, UInt64Type},
    Array, RecordBatch,
};
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
    writer::INPUT_ROW_COLUMN,
};
#[cfg(test)]
use crate::compression;
#[cfg(test)]
use crate::mvt::{LayerBuilder, TileBuilder};
use crate::{
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
pub(crate) mod valve_predict;
use tile_data::TileData;

mod collect;
mod columns;
mod coverage_fill;
mod driver;
mod encode;
mod plan;
mod scan;
mod store;
mod valve;
pub(crate) mod variable_depth;
#[cfg(test)]
pub(crate) use self::encode::joined_tile_probe;
pub(in crate::overview) use self::scan::geometry_lnglat_bbox;
use self::{collect::*, columns::*, driver::*, encode::*, plan::*, scan::*, store::*, valve::*};
pub(crate) use self::{
    columns::webmerc_to_lnglat, encode::JoinedTile, encode::JoinedTileFitter, encode::JointMembers,
    encode::JointSource, scan::lnglat_bbox, valve::draws_one_point, valve::select_kept_by_size,
    valve::valve_predicts, valve::MAX_DROP_PASSES,
};
pub use self::{
    encode::FeatureOrder, plan::resolve_partition_wave, plan::zoom_for_level,
    plan::PARTITION_WAVE_AUTO, plan::PARTITION_WAVE_MIN,
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

#[cfg(test)]
use coalesce_valve::GAP_NONE;
mod feature_id;
use feature_id::{
    feature_id_values, resolve_feature_id, validate_feature_id_column, ResolvedFeatureId,
};
pub use feature_id::{FeatureIdColumnProblem, InvalidFeatureIdReason};

/// Log target of every stage submodule: the stages log as `overview::export`,
/// as they did before #916 split them out, so `RUST_LOG` filters, CLI stderr
/// and Python logger names do not change.
const LOG_TARGET: &str = module_path!();

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
/// This is the one bound. The sharded path's read-pruning margin (two pivot
/// tiles, `shard::SHARD_BUFFER_MARGIN_TILES`; nine when the export predicts
/// valve floors, #768) is wider than this, so the cap satisfies it; the
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
/// points always takes [`SizeValve::Drop`], which keeps its best-ranked
/// points. See `overview::export::coalesce_valve`.
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
    /// survive, or the best-ranked points, spread evenly. Every feature that remains
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

/// How far the size valve's threshold reaches past the tile it fits (#768).
///
/// Applies with a size cap: to the merge valve's merge threshold
/// ([`SizeValve::Merge`]) and to the share of its points a point tile keeps
/// under [`SizeValve::Drop`].
///
/// DIVERGENCE FROM TIPPECANOE: tippecanoe's `--drop-densest-as-needed` and
/// `--coalesce-densest-as-needed` keep one gap threshold per zoom: a tile
/// too big raises it, and the zoom is rebuilt at the largest any tile
/// needed (`tile.cpp`, `traverse_zooms`), so sparse tiles beside a dense
/// one are thinned too. [`ValveScope::Zoom`] is that threshold, predicted
/// from the export scan, in one pass; for points it is one kept share a
/// zoom, as `--drop-fraction-as-needed` keeps. [`ValveScope::Tile`] fits each tile
/// alone, as tylertoo did before #768, so the density can jump at a tile
/// edge where one tile needed thinning and its neighbour did not.
/// [`ValveScope::Smooth`] predicts each tile's need and starts every tile
/// around it from that threshold faded by `1 / --valve-max-step` a tile, so
/// the density steps down over several tiles and the rest of the zoom keeps
/// its features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ValveScope {
    /// The default. Each tile predicted over its cap starts from its
    /// predicted threshold, and every tile around it, one that fits too,
    /// from that threshold faded by `1 / --valve-max-step` a ring of tiles,
    /// out to where it no longer thins anything (at most
    /// `valve_predict::MAX_RINGS` rings). Under `merge` the threshold is a
    /// merge gap; under `drop`, the share of its points a point tile keeps,
    /// by rank, so a tile keeps at most `--valve-max-step` times its
    /// neighbour's share.
    #[default]
    Smooth,
    /// Each tile is thinned only as much as it needs on its own.
    Tile,
    /// Each tile starts from the largest threshold predicted at its zoom
    /// (under `drop`, every point tile keeps the zoom's smallest predicted
    /// share). Not shard-invariant, so refused with `--tile-range`.
    Zoom,
}

impl ValveScope {
    /// The value as the CLI spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Smooth => "smooth",
            Self::Tile => "tile",
            Self::Zoom => "zoom",
        }
    }
}

crate::tile_format::spelled_enum!(ValveScope, "valve scope", [Smooth, Tile, Zoom]);

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
    #[cfg_attr(feature = "cli", arg(help = format!("Edge buffer around each tile, in tile pixels, so features continue across tile seams. At most {MAX_TILE_BUFFER_PX} (one tile width)"), long_help = None))]
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
    #[cfg_attr(feature = "cli", arg(help = format!("Per-tile cap on the raw MVT size, or zero for no cap: a backstop behind `--max-compressed-tile-size`. `auto` is {}, or no cap with `tiles --verbatim` and for `pyramid` bands. A tile over a cap goes through `--size-valve`. With several layers the caps apply to the joined tile", super::convert::human_bytes(DEFAULT_TILE_SIZE_LIMIT as u64)), long_help = None))]
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
    #[cfg_attr(feature = "cli", arg(help = format!("Per-tile cap on the gzip-compressed tile size, like tippecanoe's `-M`, or zero for no cap. `auto` is {}, or no cap with `tiles --verbatim` and for `pyramid` bands. Both caps apply, to the joined tile when there are several layers", super::convert::human_bytes(DEFAULT_COMPRESSED_TILE_SIZE_LIMIT as u64)), long_help = None))]
    pub compressed_tile_size_limit: Option<usize>,
    /// How a tile over a size limit is brought back under it (#745). Default
    /// [`SizeValve::Merge`].
    #[serde(with = "super::config::spelled")]
    #[cfg_attr(feature = "cli", arg(
        long,
        value_name = "merge|drop",
        default_value_t = ExportOptions::default().size_valve
    ))]
    #[cfg_attr(feature = "cli", arg(help = format!("How to reduce a tile over its cap: `merge` merges dense lines and polygons, drops singles, halves the extent down to {floor}, then drops whole features if needed. Merges keep the target's attributes and are never split into their sources. In a tile of lines or polygons, only a lone feature still over the cap at the extent floor is written over it, with a warning. `drop` removes features and keeps each survivor's attributes. Point tiles use dropping in both modes", floor = valve::MIN_VALVE_EXTENT), long_help = None))]
    pub size_valve: SizeValve,
    /// How far the size valve's threshold reaches past the tile it fits
    /// (#768). Default [`ValveScope::Smooth`].
    #[serde(with = "super::config::spelled")]
    #[cfg_attr(feature = "cli", arg(
        long,
        value_name = "smooth|tile|zoom",
        default_value_t = ExportOptions::default().valve_scope
    ))]
    #[cfg_attr(feature = "cli", arg(help = "How far the size valve's thinning reaches past the tile that needs it: `smooth` predicts each tile's threshold from the export scan and fades it over the tiles around, so neighboring tiles differ by at most `--valve-max-step` (in point tiles, in the share of points they keep) and the density steps down over several tiles instead of jumping at a tile edge; tiles near a dense one are thinned too. `tile` thins each tile only as much as it needs on its own: the most detail, with visible steps at tile edges. `zoom` starts every tile from the largest threshold predicted at its zoom, like tippecanoe's one threshold per zoom: no edges, the least detail. On `tiles` and `export-pmtiles`, `--preset tippecanoe` sets it; not with `--tile-range`", long_help = None))]
    pub valve_scope: ValveScope,
    /// The largest density step `--valve-scope smooth` allows between
    /// neighbouring tiles (#768): a tile keeps at most this many times the
    /// share of points its neighbour keeps, or merges at most this many
    /// times less hard. Each tile's floor fades by `1 / valve_max_step` a
    /// ring and reaches out until it no longer thins anything, at most
    /// `valve_predict::MAX_RINGS` (8) rings: a floor that would still thin
    /// there fades at the even, larger step that ends it by then. Must be
    /// finite and above 1; default 2; only under `smooth`.
    #[cfg_attr(feature = "cli", arg(
        long,
        value_name = "RATIO",
        default_value_t = ExportOptions::default().valve_max_step
    ))]
    #[cfg_attr(feature = "cli", arg(help = format!("With `--valve-scope smooth`, the largest density step allowed between neighboring tiles: a tile keeps at most RATIO times the points its neighbor keeps, or merges at most RATIO times less hard, so the thinning around a dense tile fades over the tiles beside it. The fade reaches at most {rings} tiles out: a tile thinned more than RATIO^{exponent} fades by the larger even step that ends there. Smaller values spread the thinning further; larger values come closer to `--valve-scope tile`. Must be above one", rings = valve_predict::MAX_RINGS, exponent = valve_predict::MAX_RINGS + 1), long_help = None))]
    pub valve_max_step: f64,
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
    #[cfg_attr(feature = "cli", arg(help = format!("MLT profile, non-default profiles need `--tile-format mlt`: `web` (default) supports web renderers, omits FastPFOR, FSST, and shared dictionaries, and stores integer columns wider than {small_bits} bits as doubles when every value is exact (absolute value at most {exact}), since GL JS reads {large_bits}-bit integers as BigInt and style expressions do not compare BigInt with numbers; mlt-core profiles: `native` for MapLibre Native with every codec and stored polygon triangles, `min-size` for smallest tiles, `diffable` for small binary diffs between releases, `fast-encoding` for quickest encoding", small_bits = u32::BITS, large_bits = u64::BITS, exact = crate::tile_format::MAX_EXACT_DOUBLE_INTEGER), long_help = None))]
    pub mlt_profile: MltProfile,
    /// Experimental (#750): draw, under each tile's own features below the
    /// finest zoom, the area its child tiles cover, built from the finest
    /// level before the export starts (`export/coverage_fill.rs`). Default
    /// `false`. A coarse job ([`Self::zoom_ceiling`]) builds it from the
    /// file's finest level (#771); a data shard ([`Self::tile_range`]) does
    /// not build it.
    #[cfg_attr(feature = "cli", arg(long))]
    #[cfg_attr(feature = "cli", arg(help = "Fill the holes that dense small polygons (parcels, fields, small admin units) leave at coarse zooms: under each tile's features, also draw the area its child tiles cover, with the attributes of one feature it sits under, in the room the tile's own features leave. Can overdraw scattered fields and holds the finest level's geometry in memory. A sharded `tiles` build's coarse job draws it; data shards (`--tile-range`) do not", long_help = None))]
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
    #[cfg_attr(feature = "cli", arg(help = format!("Simplify each finest-level (canonical) line and polygon once before clipping, with tolerance factor × GSD at that level's zoom, like `overview --simplify-factor` for coarser levels. Only vertices are removed: features, parts and rings smaller than the tolerance keep their own geometry. Leaves coarser levels unchanged, except that variable-depth stop tiles carry the simplified finest-level features. Leaves the overview file unchanged. Unset, lines get {DEFAULT_LINE_MAX_ZOOM_SIMPLIFY_FACTOR} and polygons are not simplified, since each polygon is simplified on its own and shared borders would part. Zero simplifies nothing, as `tiles --verbatim` and `pyramid` bands do."), long_help = None))]
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
    #[cfg_attr(feature = "cli", arg(help = "Memory profile: `speed` holds everything in RAM, `bounded` spills to disk and holds the least, and `auto` sizes every buffer to the machine and spills when it must. The default mimalloc build also purges freed pages immediately under `bounded` (at a speed cost); a build without mimalloc uses the system allocator. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles>", long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub profile: MemoryProfile,
    /// Worker threads in the global Rayon pool (#876). `None` (default):
    /// a positive `RAYON_NUM_THREADS`, else every core under `auto` and
    /// `speed` and at most [`WORKER_THREADS_MAX`] under `bounded`
    /// ([`resolve_worker_threads`]). `0` is refused. The pool is
    /// process-wide: the first run in a process sizes it, and later runs
    /// keep it. A set count also caps the core count the read workers,
    /// in-flight batches and partition wave are sized from. Changes speed
    /// and peak memory only.
    ///
    /// [`WORKER_THREADS_MAX`]: super::convert::WORKER_THREADS_MAX
    /// [`resolve_worker_threads`]: super::convert::resolve_worker_threads
    #[cfg_attr(
        feature = "cli",
        arg(long, value_name = "N", help_heading = "Memory & performance")
    )]
    #[cfg_attr(feature = "cli", arg(help = format!("Worker threads for the whole run. Default: every core under `auto` and `speed`, at most {workers_max} under `bounded`; `RAYON_NUM_THREADS` is used when this is unset. The pool is process-wide, so in Python the first call sets it. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#worker-threads>", workers_max = super::convert::WORKER_THREADS_MAX), long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub workers: Option<usize>,
    /// Bytes of memory the run may use (#876), in place of the detected
    /// figure (the smaller of the cgroup headroom and `MemAvailable`).
    /// `None` (default): `TYLERTOO_AUTO_MEM_LIMIT_BYTES` if set, else the
    /// detected figure. `0` is refused. Every memory probe of the run reads
    /// it, through the process-wide override that options validation sets.
    #[cfg_attr(feature = "cli", arg(
        long,
        value_name = "SIZE",
        value_parser = super::config::cli::parse_size_bytes,
        help_heading = "Memory & performance"
    ))]
    #[cfg_attr(feature = "cli", arg(help = "Memory the run may use, such as `24G`, in place of the detected available memory (cgroup limit or `MemAvailable`); overrides `TYLERTOO_AUTO_MEM_LIMIT_BYTES`. See <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-limit>", long_help = None))]
    #[cfg_attr(feature = "cli", arg(group = "export_only"))]
    pub memory_limit: Option<usize>,
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
    with_valve_scope => valve_scope: ValveScope,
    with_valve_max_step => valve_max_step: f64,
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
    with_workers => workers: Option<usize>,
    with_memory_limit => memory_limit: Option<usize>,
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
            valve_scope: ValveScope::Smooth,
            valve_max_step: 2.0,
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
            workers: None,
            memory_limit: None,
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
        super::config::check_run_resources(self.workers, self.memory_limit)?;
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
        if !(self.valve_max_step.is_finite() && self.valve_max_step > 1.0) {
            let option = "--valve-max-step / valve_max_step";
            return Err(ConfigError::InvalidSpec {
                option,
                message: format!(
                    "{option} is the largest density step between neighbouring tiles and must \
                     be a finite number above 1 (2 is the default), got {}",
                    self.valve_max_step
                ),
            });
        }
        if self.tile_format == TileFormat::Mlt && !crate::tile_format::MLT_SUPPORTED {
            return Err(ConfigError::InvalidSpec {
                option: "--tile-format / tile_format",
                message: "this build cannot write MLT: rebuild with the `mlt` cargo feature".into(),
            });
        }
        if self.valve_scope != ValveScope::Smooth
            && self.valve_max_step != Self::default().valve_max_step
        {
            return Err(ConfigError::OptionIgnored {
                option: "valve_max_step",
                command: "a --valve-scope tile or zoom export",
                reason: "it sets how far --valve-scope smooth fades; pass --valve-scope smooth",
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
        if self.valve_scope == ValveScope::Zoom && self.tile_range.is_some() {
            // A zoom's largest need comes from tiles a shard never reads.
            return Err(ConfigError::InvalidSpec {
                option: "--valve-scope / valve_scope",
                message: "--valve-scope zoom takes each zoom's largest predicted need over the \
                          whole input, which a shard (--tile-range) cannot see; use smooth or \
                          tile (--preset tippecanoe sets zoom)"
                    .into(),
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

/// A tile's `(merge, drop)` valve floors ([`predicted_valve_floors`]).
#[doc(hidden)]
pub type FloorPair = (u64, u64);

/// #768: the valve floors an export of `overview` under `options` starts
/// its tiles from, keyed `(zoom, tile id)`, as `(merge, drop)`: for the
/// shard parity oracle, which compares a shard's floors with the monolithic
/// build's.
///
/// # Errors
///
/// As [`export_pmtiles`], for the scan and the price samples.
#[doc(hidden)]
pub fn predicted_valve_floors(
    overview: &Path,
    options: &ExportOptions,
) -> Result<BTreeMap<(u8, u64), FloorPair>, ExportError> {
    // The export it predicts validates and applies these first.
    options.validate()?;
    let diagnostics = crate::diagnostics::Diagnostics::default();
    let reader = OverviewReader::open(overview)?;
    let crs = crs_from_descriptor(reader.resolved_schema(), overview, &diagnostics)?;
    let zooms = resolve_level_zooms(reader.meta(), reader.num_levels())?;
    let mut budget = export_budget();
    let band = BandRead {
        source: reader.source(),
        budget,
        profile: options.profile,
    };
    let cols = resolve_export_columns((&reader, band), options, &zooms)?;
    let (_, floors) = scan_with_floors(
        &reader,
        crs,
        (&cols.published, cols.feature_id.as_ref()),
        (options, &zooms),
        &mut budget,
        (&ExportTimers::default(), None),
        &diagnostics,
    )?;
    Ok(floors
        .merge
        .tiles
        .keys()
        .chain(floors.drop.tiles.keys())
        .map(|&(z, x, y)| {
            let f = floors.for_tile((z, x, y));
            ((z, tile_key(x, y, z)), (f.merge, f.drop))
        })
        .collect())
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
    // #876: the per-tile trace, drained to its file when the export returns.
    let _tile_trace = crate::tile_trace::Session::start(diagnostics);
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
    // groups against its tile range widened by at least
    // `SHARD_BUFFER_MARGIN_TILES` pivot tiles; a buffer wider than that margin would let a feature render
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
    // #768: the valve floors, from the whole scan (before a shard restricts
    // it). Each tile's need comes from its own members alone, so a shard,
    // which holds every row of the tiles in its range and of their
    // neighbours (`shard::SHARD_READ_MARGIN_TILES`), predicts the floors the
    // full build does.
    let (mut scans, floors) = scan_with_floors(
        &reader,
        crs,
        (&published, feature_id.as_ref()),
        (options, &level_zooms),
        budget,
        (&timers, vd_scan.as_mut()),
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
        floors: Some(&floors),
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

#[cfg(test)]
#[path = "export/encode_cost_tests.rs"]
mod encode_cost_tests;
mod packed_props;

#[cfg(test)]
mod tests;
#[cfg(test)]
#[path = "export/valve_cost_tests.rs"]
mod valve_cost_tests;

#[cfg(all(test, feature = "cli"))]
#[path = "help_tests.rs"]
mod help_tests;
