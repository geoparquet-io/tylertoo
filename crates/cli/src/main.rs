//! CLI for tylertoo - Convert GeoParquet to PMTiles
//!
//! This is a thin wrapper around the tylertoo-core library.

/// Global allocator for static-musl builds (#480).
///
/// musl's `mallocng` deterministically fails *small* allocations once a large
/// live heap has been fragmented by many threads: a 38.7M-polygon `tiles` run
/// aborted with `memory allocation of 148448 bytes failed` at the identical
/// input row across two runs, at 28.6 GB RSS on a node with 240 GB granted —
/// >200 GB of headroom. mimalloc's segment/page allocator does not degrade
/// that way, so the musl release binary uses it instead.
///
/// Scoped to `target_env = "musl"` on purpose: glibc, macOS and Windows builds
/// keep the platform allocator (nothing to fix there), and the Python
/// extension module never routes through this crate — a pyo3 `cdylib` must
/// leave the host interpreter's allocator arrangements alone.
///
/// Excluded under `dhat-heap`: dhat installs its own `#[global_allocator]`
/// in tylertoo-core (it must own the allocator to count anything), and rustc
/// allows only one in the crate graph. Heap-profiling a musl binary therefore
/// runs on dhat's allocator and loses this fix for the duration.
#[cfg(all(target_env = "musl", not(feature = "dhat-heap")))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use indicatif::HumanBytes;
use std::path::{Path, PathBuf};
use tylertoo_core::overview::auto_zoom::MaxZoom;
use tylertoo_core::overview::export::FeatureOrder;
use tylertoo_core::overview::ladder::{EntryZoomKind, EntryZoomSpec};

/// Why a suffixed size did not parse: only the overflow branch should
/// mention the ceiling, so the caller's message is built per branch.
#[derive(Debug, PartialEq, Eq)]
enum SizeParseError {
    /// Not `<integer>[K|M|G]` at all.
    Malformed,
    /// Well-formed, but `n * multiplier` does not fit `usize` (#432).
    Overflow,
}

/// Parse `<integer>[K|KB|M|MB|G|GB]` (case-insensitive) to bytes.
///
/// The suffix multiplication is checked (#432): `99999999999999999G` used to
/// wrap in release builds and be accepted as a small byte count.
fn parse_suffixed_size(s: &str) -> Result<usize, SizeParseError> {
    let s = s.trim().to_uppercase();
    let (num_str, multiplier): (&str, usize) = if s.ends_with("G") || s.ends_with("GB") {
        (
            s.trim_end_matches("GB").trim_end_matches("G"),
            1024 * 1024 * 1024,
        )
    } else if s.ends_with("M") || s.ends_with("MB") {
        (s.trim_end_matches("MB").trim_end_matches("M"), 1024 * 1024)
    } else if s.ends_with("K") || s.ends_with("KB") {
        (s.trim_end_matches("KB").trim_end_matches("K"), 1024)
    } else {
        // Assume bytes if no suffix
        (s.as_str(), 1)
    };

    let n = num_str
        .trim()
        .parse::<usize>()
        .map_err(|_| SizeParseError::Malformed)?;
    n.checked_mul(multiplier).ok_or(SizeParseError::Overflow)
}

/// The ceiling clause appended to an overflow error, and nothing else: a
/// malformed value gets the format hint alone.
fn size_ceiling_hint(err: SizeParseError) -> String {
    match err {
        SizeParseError::Malformed => String::new(),
        SizeParseError::Overflow => format!(" (the value must fit in {} bytes)", usize::MAX),
    }
}

/// Parse a human-readable byte size (e.g., "500K", "1M", "2G") as usize.
///
/// A plain integer with no suffix is interpreted as raw bytes, so callers that
/// previously passed a byte count (e.g. `--tile-size-limit 500000`) keep working.
fn parse_size_bytes(s: &str) -> Result<usize, String> {
    parse_suffixed_size(s).map_err(|err| {
        format!(
            "Invalid size: '{s}'. Use a byte count or a suffixed size like '500K', '1M', '2G'{}",
            size_ceiling_hint(err)
        )
    })
}

/// Map a per-tile size-cap CLI value to [`ExportOptions::tile_size_limit`]:
/// `0` (the off switch, e.g. `--max-tile-size 0`) becomes `None` (cap disabled);
/// any positive byte count becomes `Some(n)`. The default `500K` therefore caps;
/// `0` opts out. See issue #280.
/// Default per-tile MVT size cap (tippecanoe parity, #280).
const DEFAULT_MAX_TILE_SIZE: usize = 500 * 1024;

/// Resolve the `tiles` per-tile size cap.
///
/// An explicit `--max-tile-size` always wins. Otherwise `--verbatim` means no
/// cap — a valve that sheds features to fit a byte budget is not verbatim
/// either — and the default is [`DEFAULT_MAX_TILE_SIZE`].
fn resolve_tiles_size_limit(explicit: Option<usize>, verbatim: bool) -> Option<usize> {
    size_limit_opt(explicit.unwrap_or(if verbatim { 0 } else { DEFAULT_MAX_TILE_SIZE }))
}

fn size_limit_opt(n: usize) -> Option<usize> {
    (n > 0).then_some(n)
}

/// Parse `--in-flight-batches`: `auto` (→ the core-sized sentinel 0) or an
/// explicit positive integer. See
/// [`tylertoo_core::overview::convert::resolve_in_flight_batches`] for how the
/// sentinel is expanded at pass-2 setup.
fn parse_in_flight_batches(s: &str) -> Result<usize, String> {
    if s.eq_ignore_ascii_case("auto") {
        return Ok(tylertoo_core::overview::convert::IN_FLIGHT_BATCHES_AUTO);
    }
    match s.parse::<usize>() {
        Ok(0) => Err("in-flight-batches must be `auto` or >= 1".to_string()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("expected `auto` or a positive integer, got `{s}`")),
    }
}

/// Parse `--read-batch-size`: a positive row count, capped at
/// [`READ_BATCH_SIZE_MAX`].
///
/// A batch is fully resident and several coexist (in flight, plus the readers'
/// read-ahead), so an absurd row count is an out-of-memory abort rather than a
/// tuning choice — better rejected at parse time with the ceiling named.
fn parse_read_batch_size(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("read-batch-size must be >= 1".to_string()),
        Ok(n) if n > READ_BATCH_SIZE_MAX => Err(format!(
            "read-batch-size must be <= {READ_BATCH_SIZE_MAX} (a batch is fully resident)"
        )),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("expected a positive integer, got `{s}`")),
    }
}

/// Ceiling on `--read-batch-size`. One million rows of even trivial geometry
/// is already a multi-hundred-MB batch; past that the knob stops bounding
/// memory and starts defeating it.
const READ_BATCH_SIZE_MAX: usize = 1_048_576;

/// Parse `--read-workers`: `auto` (→ the core-sized sentinel 0) or an
/// explicit positive integer, capped at
/// [`tylertoo_core::overview::convert::read_workers_ceiling`] (2× this
/// machine's cores). See
/// [`tylertoo_core::overview::convert::resolve_read_workers`] for how the
/// sentinel is expanded at pass-2 setup; the core clamps out-of-range values
/// with a warning, and this mirrors the bound as a clear parse error.
fn parse_read_workers(s: &str) -> Result<usize, String> {
    if s.eq_ignore_ascii_case("auto") {
        return Ok(tylertoo_core::overview::convert::READ_WORKERS_AUTO);
    }
    let ceiling = tylertoo_core::overview::convert::read_workers_ceiling();
    match s.parse::<usize>() {
        Ok(0) => Err("read-workers must be `auto` or >= 1".to_string()),
        Ok(n) if n > ceiling => Err(format!(
            "read-workers must be `auto` or 1..={ceiling} (2× this machine's cores); \
             every worker is a thread plus its own read-ahead queue"
        )),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("expected `auto` or a positive integer, got `{s}`")),
    }
}

/// Parse `--partition-wave`: `auto` (→ the core-sized sentinel
/// [`tylertoo_core::overview::export::PARTITION_WAVE_AUTO`]) or an explicit
/// positive integer. See [`tylertoo_core::overview::export::resolve_partition_wave`]
/// for how the sentinel is expanded at export start.
fn parse_partition_wave(s: &str) -> Result<usize, String> {
    if s.eq_ignore_ascii_case("auto") {
        return Ok(tylertoo_core::overview::export::PARTITION_WAVE_AUTO);
    }
    match s.parse::<usize>() {
        Ok(0) => Err("partition-wave must be `auto` or >= 1".to_string()),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("expected `auto` or a positive integer, got `{s}`")),
    }
}

/// Parse a --bbox argument: xmin,ymin,xmax,ymax (lon/lat degrees).
fn parse_bbox(s: &str) -> Result<[f64; 4]> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        anyhow::bail!("--bbox must be xmin,ymin,xmax,ymax (4 comma-separated values)");
    }
    let vals: Vec<f64> = parts
        .iter()
        .map(|p| {
            p.trim()
                .parse::<f64>()
                .map_err(|e| anyhow::anyhow!("invalid number in --bbox: {e}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let bbox = [vals[0], vals[1], vals[2], vals[3]];
    if bbox[0] > bbox[2] || bbox[1] > bbox[3] {
        anyhow::bail!("--bbox must satisfy xmin <= xmax and ymin <= ymax");
    }
    Ok(bbox)
}

/// Convert GeoParquet to PMTiles vector tiles and multi-resolution
/// overviews.
///
/// `tylertoo INPUT OUTPUT` with no subcommand runs `tiles`.
#[derive(Parser, Debug)]
#[command(
    name = "tylertoo",
    about = "Convert GeoParquet to PMTiles vector tiles and multi-resolution overviews",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Generate PMTiles vector tiles from GeoParquet (the default command).
    Tiles(Box<TilesArgs>),
    /// Build a multi-resolution overview GeoParquet file.
    Overview(Box<OverviewArgs>),
    /// Check a GeoParquet overview file against the overviews spec.
    Validate(ValidateArgs),
    /// Export a PMTiles archive from an overview GeoParquet file.
    ExportPmtiles(ExportPmtilesArgs),
    /// Decode a PMTiles vector-tile archive back to GeoParquet.
    Decode(DecodeArgs),
    /// Report per-zoom tile sizes for a PMTiles archive, from its
    /// directory alone.
    Stats(StatsArgs),
    /// Build one archive from several inputs, each owning a zoom range.
    Pyramid(PyramidArgs),
    /// Combine PMTiles archives that hold disjoint tiles into one.
    Merge(MergeArgs),
    /// Cut a dataset's tile space into N disjoint shards for a sharded build.
    ShardPlan(ShardPlanArgs),
    /// Emit the full CLI reference as Markdown (docs generator, hidden).
    ///
    /// Compiled only under the `gen-docs` feature; used by CI to regenerate
    /// `docs/reference/cli.md` and diff-guard the committed copy. Not part
    /// of the production binary.
    #[cfg(feature = "gen-docs")]
    #[command(hide = true)]
    GenReferenceDocs,
}

/// Arguments for `tylertoo pyramid`.
///
/// The band rules are documented in the pyramid section of
/// `docs/OVERVIEW_TUNING.md`.
#[derive(Parser, Debug)]
pub struct PyramidArgs {
    /// Output PMTiles archive.
    pub output: PathBuf,

    /// One zoom band as `LO-HI:INPUT[:LAYER]` (repeatable). `INPUT` is a
    /// GeoParquet source or PMTiles archive for zooms `LO` to `HI`, and
    /// `LAYER` defaults to its file stem. The tuning guide covers the colon
    /// rules and the `LO-HI=INPUT[=LAYER]` form:
    /// <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>.
    #[arg(long = "band", required = true, value_name = "LO-HI:INPUT[:LAYER]")]
    pub bands: Vec<String>,

    /// Run the generalization ladder on each GeoParquet band instead of
    /// tiling it verbatim. Use it when a band holds raw features spanning
    /// several zooms.
    #[arg(long)]
    pub generalize: bool,

    /// Per-tile MVT size cap for bands tiled here, such as `500K`. Unset
    /// means no cap, so a band keeps every cell it exists to draw.
    #[arg(long, value_name = "SIZE", value_parser = parse_size_bytes)]
    pub max_tile_size: Option<usize>,

    /// Within-tile feature order for GeoParquet bands: `input`, or a
    /// property name with optional `:asc` or `:desc`. A band without that
    /// column keeps input order with a warning.
    #[arg(long, value_name = "input|COLUMN[:asc|:desc]", default_value = "input")]
    pub feature_order: FeatureOrder,

    /// Directory for the per-band intermediate files, which tylertoo deletes
    /// afterwards. Defaults to the system temp directory.
    #[arg(long, value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    /// Accept a pre-tiled band whose archive holds fewer zooms than the band
    /// declares. Those zooms render empty, so use it only when you want a
    /// sparse pyramid.
    #[arg(long)]
    pub allow_missing_zooms: bool,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    pub force: bool,
}

/// Arguments for `tylertoo merge`.
///
/// The merge copies tile bodies unchanged and refuses inputs whose tile
/// ids overlap; see the sharded builds guide.
#[derive(Parser, Debug)]
pub struct MergeArgs {
    /// Output PMTiles archive.
    pub output: PathBuf,

    /// Two or more input PMTiles archives with disjoint tile ids and one tile
    /// type and compression. The output's bounds, zoom range, and
    /// `vector_layers` are the union of the inputs'.
    #[arg(required = true, value_name = "INPUT")]
    pub inputs: Vec<PathBuf>,

    /// Directory for the spool file that holds tile data until the archive
    /// is complete. Defaults to the system temp directory.
    #[arg(long, value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    /// Write the JSON merge report, with per-zoom tile counts, to this path.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    pub force: bool,
}

/// Arguments for `tylertoo decode`.
///
/// The output schema and its limits are in the `after_help` text below.
#[derive(Parser, Debug)]
#[command(after_help = "\
The output holds the tiles' features, not the original source:
  - simplified: tiling drops vertices at lower zooms. Decode the max
    zoom for the most detail.
  - clipped: tiling cuts features at the buffered tile edges.
  - duplicated: a feature appears once per tile it touches and once per
    zoom. Filter with --zoom or the `zoom` column.
  - lost properties: attributes that tiling dropped do not come back.
Decoding does not restore the input: `A.parquet` -> `B.pmtiles` ->
`C.parquet` gives a C that differs from A.

Output columns, in order:
  - `zoom` (UInt8), `layer` (Utf8), `mvt_id` (UInt64, null when the
    encoder set no id): where each row came from.
  - every property seen in any tile, alphabetical, and null for a
    feature that lacks it. Integers become Int64 and floats Float64.
    A key that mixes the two becomes Float64, and any other mix Utf8.
  - geometry: Well-Known Binary (WKB) in lon/lat (`EPSG:4326`), with a
    bbox covering.
A source property named `zoom`, `layer`, `mvt_id`, or `geometry` is an error.")]
struct DecodeArgs {
    /// Input PMTiles archive (vector tiles).
    #[arg(value_name = "INPUT")]
    input: PathBuf,

    /// Output GeoParquet file.
    #[arg(value_name = "OUTPUT")]
    output: PathBuf,

    /// Decode one zoom level, which suits most uses.
    #[arg(long, conflicts_with_all = ["min_zoom", "max_zoom"])]
    zoom: Option<u8>,

    /// Minimum zoom level to decode.
    #[arg(long)]
    min_zoom: Option<u8>,

    /// Maximum zoom level to decode.
    #[arg(long)]
    max_zoom: Option<u8>,

    /// Only decode features from this MVT layer.
    #[arg(long, value_name = "NAME")]
    layer: Option<String>,

    /// Write the JSON decode report to this path.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    force: bool,

    /// Not supported here (single-file subcommand); accepted so the error
    /// can point at `overview`/`tiles` instead of clap's generic message.
    #[arg(long, value_name = "PATH", hide = true)]
    files_from: Option<PathBuf>,
}

/// Arguments for `tylertoo shard-plan`, step 0 of a sharded build.
///
/// Reads Parquet footers only, so planning a large input takes seconds.
#[derive(Parser, Debug)]
pub struct ShardPlanArgs {
    /// Input GeoParquet that every job in the fleet tiles, in any form
    /// `tiles` accepts. Omit it with `--files-from`.
    #[arg(value_name = "INPUT", required_unless_present = "files_from")]
    pub input: Option<PathBuf>,

    /// Plan for the inputs this manifest lists, one path or URL per line in
    /// dataset row order, instead of `INPUT`. Give every job in the fleet
    /// the same manifest.
    #[arg(long, value_name = "PATH")]
    pub files_from: Option<PathBuf>,

    /// Where to write the shard plan.
    #[arg(short, long, value_name = "PATH")]
    pub output: PathBuf,

    /// How many data shards to cut. Run one `tiles --shard i/N` job per
    /// shard plus one `--shard coarse` job, then merge all N+1 archives.
    #[arg(long, value_name = "N")]
    pub shards: usize,

    /// Zoom to cut at. Shards own the zooms from here to `--max-zoom`, and
    /// the coarse job owns the zooms below. Pick a zoom where each shard
    /// holds a few tiles of data, usually z4 to z8.
    #[arg(long, value_name = "ZOOM", default_value = "6")]
    pub pivot: u8,

    /// Overwrite an existing plan at --output.
    #[arg(short, long)]
    pub force: bool,
}

/// Arguments for `tylertoo export-pmtiles`.
#[derive(Parser, Debug)]
struct ExportPmtilesArgs {
    /// Input overview GeoParquet file from `tylertoo overview`.
    #[arg(value_name = "INPUT")]
    input: PathBuf,

    /// Output PMTiles archive.
    #[arg(value_name = "OUTPUT")]
    output: PathBuf,

    /// MVT layer name written into every tile.
    #[arg(long, default_value = "overview")]
    layer_name: String,

    /// Minimum zoom to declare in the archive metadata, even when the
    /// overview file's coarsest levels are empty. Unset uses the coarsest
    /// level's zoom.
    #[arg(long, value_name = "ZOOM")]
    min_zoom: Option<u8>,

    /// Keep only these properties in the tiles (repeatable). The overview
    /// file keeps every column, and the `--feature-order` column must stay.
    #[arg(long, value_name = "NAME")]
    include_property: Vec<String>,

    /// Drop these properties from the tiles (repeatable).
    /// `--include-property` overrides it.
    #[arg(long, value_name = "NAME")]
    exclude_property: Vec<String>,

    /// Drop every property, writing geometry-only tiles.
    /// `--include-property` overrides it.
    #[arg(long)]
    exclude_all_properties: bool,

    /// Edge buffer around each tile, in tile pixels, so features continue
    /// across tile seams. At most 256 (one tile width).
    #[arg(long, default_value = "8")]
    tile_buffer: u32,

    /// Per-tile MVT size cap, such as `500K`, `1M`, or a byte count, or 0 for
    /// no cap. A tile over the cap drops features until it fits.
    #[arg(long, value_name = "SIZE", visible_alias = "max-tile-size", default_value = "500K", value_parser = parse_size_bytes)]
    tile_size_limit: usize,

    /// Write the JSON export report, with per-zoom tile and feature counts,
    /// to this path.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

    /// Clip every polygon with the full overlay instead of the fast path for
    /// simple rings. Use it for byte-stable tiles: the fast path renders the
    /// same but can start a ring at a different vertex.
    #[arg(long)]
    no_simple_clip_fastpath: bool,

    /// Partitions to export at once, or `auto` to size the wave to the cores
    /// and free memory. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>.
    #[arg(long, value_name = "N|auto", default_value = "auto", value_parser = parse_partition_wave)]
    partition_wave: usize,

    /// Within-tile feature order: `input` keeps source row order, and a
    /// property name, optionally with `:asc` or `:desc`, sorts each tile by
    /// it. A renderer paints in this order unless a style overrides it.
    #[arg(long, value_name = "input|COLUMN[:asc|:desc]", default_value = "input")]
    feature_order: FeatureOrder,

    /// Write this integer column as each feature's MVT id, so
    /// `setFeatureState` keys work across tiles and zooms. Every row
    /// must hold a value from 0 to 2^64-1. Without it, tiles keep
    /// tile-local ids.
    #[arg(long, value_name = "COLUMN")]
    feature_id: Option<String>,

    /// Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at
    /// one zoom, plus their descendants. Ranges that partition a zoom give
    /// disjoint archives, which `tylertoo merge` can join.
    #[arg(long, value_name = "LO..HI")]
    tile_range: Option<String>,

    /// Emit only the tiles at or below this zoom, the coarse half of a
    /// sharded build. A partial overview kept from `tiles --shard coarse`
    /// needs a ceiling at or below its own.
    #[arg(long, value_name = "ZOOM")]
    zoom_ceiling: Option<u8>,

    /// Directory for the export's spill file, used when buffered tiles
    /// exceed the memory budget. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>.
    #[arg(long, value_name = "PATH", help_heading = "Memory & performance")]
    spill_dir: Option<PathBuf>,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    force: bool,

    /// Not supported here (single-file subcommand); accepted so the error
    /// can point at `overview`/`tiles` instead of clap's generic message.
    #[arg(long, value_name = "PATH", hide = true)]
    files_from: Option<PathBuf>,
}

/// Arguments for `tylertoo overview`.
#[derive(Parser, Debug)]
struct OverviewArgs {
    /// Input GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator
    /// (`EPSG:3857`). Pass a file, directory, glob, URL, or `s3://` or
    /// `gs://` prefix, or omit it with `--files-from`.
    #[arg(value_name = "INPUT", required_unless_present = "files_from")]
    input: Option<PathBuf>,

    /// Output overview GeoParquet file.
    #[arg(value_name = "OUTPUT", required_unless_present = "files_from")]
    output: Option<PathBuf>,

    /// Convert the `.parquet` files this manifest lists instead of `INPUT`.
    /// Give one path or URL per line, in dataset row order. Blank lines and
    /// `#` lines do nothing.
    #[arg(long, value_name = "PATH")]
    files_from: Option<PathBuf>,

    /// Level layout: `duplicating` writes each level in full,
    /// and `partitioning` writes each feature once, at its coarsest level.
    #[arg(long, default_value = "duplicating", value_parser = ["duplicating", "partitioning"])]
    mode: String,

    /// Minimum (coarsest) Web Mercator zoom.
    #[arg(long, default_value = "0")]
    min_zoom: u8,

    /// Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a
    /// sample of the input. Ignored with `--gsd`.
    #[arg(long, default_value = "6")]
    max_zoom: MaxZoom,

    /// Comma-separated ground sample distances (GSDs) in meters, each
    /// smaller than the last. Overrides `--min-zoom` and `--max-zoom`.
    #[arg(long, value_name = "GSDS")]
    gsd: Option<String>,

    /// Convert only features whose bbox intersects this lon/lat box. Row
    /// groups outside the box go unread.
    #[arg(long, value_name = "XMIN,YMIN,XMAX,YMAX")]
    bbox: Option<String>,

    /// Also write the third-party `cogp` footer key, for readers of that
    /// overview format. Partitioning mode only.
    #[arg(long)]
    cogp_compat: bool,

    /// Write the JSON conversion report to this path.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    force: bool,

    #[command(flatten)]
    tuning: ConvertTuningArgs,
}

/// Shared convert-tuning knobs, flattened into both `overview` and `tiles` so
/// the one-shot command reaches every quality/memory lever the two-step chain
/// exposes. Levels (`--min-zoom`/`--max-zoom`/`--gsd`), `--bbox`, `--mode`, and
/// `--cogp-compat` stay on the parent command; everything here maps into
/// [`ConvertOptions`] via [`ConvertTuningArgs::build_convert_options`].
#[derive(Args, Debug)]
struct ConvertTuningArgs {
    /// Tile the input exactly as given, with every thinning,
    /// simplification, and density step off. Use it for pre-aggregated or
    /// pre-levelled input. Knobs you set yourself still win.
    #[arg(long, help_heading = "Thinning & visibility")]
    verbatim: bool,

    /// Numeric column that decides which feature wins each thinning cell.
    /// Conflicts with `--class-rank`.
    #[arg(long, value_name = "COL", help_heading = "Ranking")]
    sort_key: Option<String>,

    /// Rank the values of `COL` from high to low to set each feature's
    /// entry zoom, one `--ladder-step` apart from `--min-zoom`. Features
    /// appear from their entry zoom inward, exempt from thinning.
    #[arg(long, value_name = "COL", help_heading = "Ranking")]
    magnitude_ladder: Option<String>,

    /// Zooms between consecutive `--magnitude-ladder` rungs.
    #[arg(
        long,
        value_name = "N",
        default_value = "1",
        help_heading = "Ranking",
        requires = "magnitude_ladder"
    )]
    ladder_step: u8,

    /// Place entry zooms by hand: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...`, for
    /// example `density:5000=4,1000=6`. Unlisted values take the ordinary
    /// visibility gate.
    #[arg(
        long,
        value_name = "SPEC",
        help_heading = "Ranking",
        conflicts_with = "magnitude_ladder"
    )]
    entry_zoom: Option<String>,

    /// Rank categorical classes for cell winners: `COLUMN:VALUE=RANK,...`,
    /// where higher wins. Unlisted values rank below listed ones and above
    /// nulls.
    #[arg(long, value_name = "SPEC", help_heading = "Ranking")]
    class_rank: Option<String>,

    /// Turn off automatic ranking for known schemas (Overture roads
    /// `class`/`road_class`, Overture places `confidence`).
    #[arg(long, help_heading = "Ranking")]
    no_auto_rank: bool,

    /// Convert only features matching this SQL `WHERE` predicate over the
    /// property columns, such as `confidence > 0.8`. Row groups that cannot
    /// match go unread, and the tuning guide has the grammar:
    /// <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>.
    #[arg(
        long,
        value_name = "EXPR",
        visible_alias = "where",
        help_heading = "Filtering"
    )]
    filter: Option<String>,

    /// Keep only these property columns (repeatable) and skip decoding the
    /// rest. Columns that other flags read must stay in the list.
    #[arg(long, value_name = "COL", help_heading = "Properties")]
    include_property: Vec<String>,

    /// Drop these property columns (repeatable). `--include-property`
    /// overrides it.
    #[arg(long, value_name = "COL", help_heading = "Properties")]
    exclude_property: Vec<String>,

    /// Drop every property column, writing geometry only.
    /// `--include-property` overrides it.
    #[arg(long, help_heading = "Properties")]
    exclude_all_properties: bool,

    /// Base of the zoom-to-GSD mapping, `gsd(z) = 40075016.69 / base /
    /// 2^z`: a larger base keeps more detail at every level. No effect with
    /// `--gsd`.
    #[arg(
        long,
        value_name = "F",
        default_value = "1024.0",
        help_heading = "Generalization"
    )]
    gsd_base: f64,

    /// Simplification tolerance in multiples of each level's GSD (default
    /// 1.0, duplicating mode only). Lower values keep more vertices.
    #[arg(long, help_heading = "Generalization")]
    simplify_factor: Option<f64>,

    /// Collapse polygons too small for a level to a single point instead of
    /// dropping them. Fill styles ignore points, so add a circle layer or
    /// use `--collapse-square`.
    #[arg(long, help_heading = "Generalization")]
    collapse: bool,

    /// Replace the polygons a coarse level drops with small placeholder
    /// squares, so the level still shows where the area is. Duplicating mode
    /// only, and the output stays Polygon.
    #[arg(long, conflicts_with = "collapse", help_heading = "Generalization")]
    collapse_square: bool,

    /// Zoom bands that change how polygons render: comma-separated
    /// `LO-HI:KIND`, where `KIND` is `geom`, `point`, or `square`, such as
    /// `0-7:point,8-14:geom`. The band rules are in the tuning guide:
    /// <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>.
    #[arg(long, value_name = "SPEC", help_heading = "Generalization")]
    representation: Option<String>,

    /// Simplify each level from the source instead of from the next finer
    /// level. Slower, but each level stays within its own tolerance of the
    /// source.
    #[arg(long, help_heading = "Generalization")]
    no_cascade: bool,

    /// Point thinning grid cell, as a multiple of the level's GSD (default
    /// 4.0, or 16.0 with `--cluster`). Larger cells keep fewer points.
    #[arg(long, help_heading = "Thinning & visibility")]
    point_thinning: Option<f64>,

    /// Line thinning grid cell, as a multiple of the level's GSD (default
    /// 1.0). Larger cells keep fewer lines.
    #[arg(long, help_heading = "Thinning & visibility")]
    line_thinning: Option<f64>,

    /// Polygon thinning grid cell, as a multiple of the level's GSD
    /// (default 1.0). Larger cells keep fewer polygons.
    #[arg(long, help_heading = "Thinning & visibility")]
    polygon_thinning: Option<f64>,

    /// Drop lines whose bbox diagonal is shorter than this many GSDs at a
    /// level (default 2.0).
    #[arg(long, help_heading = "Thinning & visibility")]
    line_visibility: Option<f64>,

    /// Drop polygons whose bbox diagonal is shorter than this many GSDs at
    /// a level (default 2.0).
    #[arg(long, help_heading = "Thinning & visibility")]
    polygon_visibility: Option<f64>,

    /// Density budget decay: each coarser level keeps 1/rate of the next
    /// finer level's feature budget. Larger values thin mid zooms harder.
    #[arg(
        long,
        value_name = "F",
        default_value = "1.65",
        help_heading = "Density budget"
    )]
    drop_rate: f64,

    /// How strongly the density budget protects sparse areas: 1 cuts every
    /// neighborhood equally, and larger values protect sparse ones more.
    #[arg(
        long,
        value_name = "F",
        default_value = "1.5",
        help_heading = "Density budget"
    )]
    drop_gamma: f64,

    /// Turn off the per-level density budget, leaving cell-winner thinning
    /// only.
    #[arg(long, help_heading = "Density budget")]
    no_density_drop: bool,

    /// Merge each thinning cell's points into its surviving point, which
    /// gains a `point_count` column. Duplicating mode only.
    #[arg(long, help_heading = "Clustering")]
    cluster: bool,

    /// Aggregate a numeric column over each cluster as `COL:OP`, where `OP` is
    /// `sum`, `max`, `min`, or `mean` (repeatable). Requires `--cluster`.
    #[arg(
        long = "accumulate-attribute",
        value_name = "COL:OP",
        help_heading = "Clustering"
    )]
    accumulate_attribute: Vec<String>,

    /// Turn off line coalescing, which joins touching same-class line
    /// segments into longer strokes at coarse levels.
    #[arg(long, help_heading = "Line coalescing")]
    no_coalesce_lines: bool,

    /// Deprecated no-op: coalescing is now the default. Kept so existing
    /// invocations keep working; rejected with partitioning mode (where the
    /// default silently disables instead).
    #[arg(long, hide = true, conflicts_with = "no_coalesce_lines")]
    coalesce_lines: bool,

    /// Continue a line through a junction when the straightest pair turns by
    /// at most this many degrees. Set 0 to stop chains at every junction.
    #[arg(
        long,
        value_name = "DEG",
        default_value = "0.0",
        help_heading = "Line coalescing"
    )]
    coalesce_junction_angle: f64,

    /// Join line ends within this many GSDs of each other when coalescing.
    /// Set 0 to join only ends that match exactly.
    #[arg(
        long,
        value_name = "F",
        default_value = "1.0",
        help_heading = "Line coalescing"
    )]
    coalesce_snap: f64,

    /// Skip coalescing on a level with more candidate lines than this, to
    /// bound memory. Long lines hit a matching geometry-size limit first.
    #[arg(
        long,
        value_name = "ROWS",
        default_value = "2000000",
        help_heading = "Line coalescing"
    )]
    coalesce_max_level_rows: usize,

    /// Maximum rows per output row group, per level. A level that would pass
    /// Parquet's row-group limit gets a larger cap.
    #[arg(long, default_value = "10000", help_heading = "Output layout")]
    row_group_size: usize,

    /// How the row-group cap varies by level: `constant`, or `zoom-scaled`,
    /// which doubles it per zoom step coarser than the finest level.
    #[arg(
        long,
        default_value = "constant",
        value_parser = ["constant", "zoom-scaled"],
        help_heading = "Output layout"
    )]
    row_group_size_policy: String,

    /// Keep Parquet min/max stats on every column, even large string and
    /// geometry columns. Use it when remote clients filter on
    /// property columns.
    #[arg(long, help_heading = "Output layout")]
    full_column_stats: bool,

    /// Load the whole dataset into memory instead of streaming it in two
    /// passes. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>.
    #[arg(long, help_heading = "Memory & performance")]
    no_streaming: bool,

    /// Rows per Arrow read batch. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#how-streaming-bounds-memory>.
    #[arg(
        long,
        value_name = "ROWS",
        default_value = "8192",
        value_parser = parse_read_batch_size,
        help_heading = "Memory & performance"
    )]
    read_batch_size: usize,

    /// Memory profile for writing levels: `speed` buffers in RAM, `bounded`
    /// spills to disk, and `auto` picks per run. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles>.
    #[arg(
        long,
        default_value = "auto",
        value_parser = ["auto", "speed", "bounded"],
        help_heading = "Memory & performance"
    )]
    profile: String,

    /// Read batches in flight at once, or `auto` to size it to the cores.
    /// See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>.
    #[arg(
        long,
        value_name = "N|auto",
        default_value = "auto",
        value_parser = parse_in_flight_batches,
        help_heading = "Memory & performance"
    )]
    in_flight_batches: usize,

    /// Reader threads for the second pass, or `auto` for a quarter of the
    /// cores, up to 4. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#read-concurrency>.
    #[arg(
        long,
        value_name = "N|auto",
        default_value = "auto",
        value_parser = parse_read_workers,
        help_heading = "Memory & performance"
    )]
    read_workers: usize,

    /// Directory for spill files: staged remote input, and on `tiles` the
    /// intermediate overview. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#spill-files>.
    #[arg(long, value_name = "PATH", help_heading = "Memory & performance")]
    spill_dir: Option<PathBuf>,

    /// Write the convert plan to PATH and keep converting, so a later run
    /// can reuse it with `--plan`. A sharded build shares one plan.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "plan",
        help_heading = "Memory & performance"
    )]
    save_plan: Option<PathBuf>,

    /// Reuse the convert plan at PATH and skip the first pass and level
    /// assignment. The plan's fingerprint must match this run's version,
    /// flags, and inputs.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "save_plan",
        help_heading = "Memory & performance"
    )]
    plan: Option<PathBuf>,
}

impl ConvertTuningArgs {
    /// Build [`ConvertOptions`] from the shared tuning flags, applying the same
    /// validation both `overview` and `tiles` rely on. The parent command owns
    /// `mode`, the `levels` plan, `bbox`, and `cogp_compat` and passes them in.
    /// The property include/exclude choice (#386).
    fn property_selection(&self) -> tylertoo_core::overview::properties::PropertySelection {
        tylertoo_core::overview::properties::PropertySelection {
            include: (!self.include_property.is_empty()).then(|| self.include_property.clone()),
            exclude: self.exclude_property.clone(),
            exclude_all: self.exclude_all_properties,
        }
    }

    fn build_convert_options(
        &self,
        mode: tylertoo_core::overview::level::Mode,
        levels: tylertoo_core::overview::convert::LevelPlan,
        bbox: Option<[f64; 4]>,
        cogp_compat: bool,
    ) -> Result<tylertoo_core::overview::convert::ConvertOptions> {
        use tylertoo_core::overview::assign::{AssignConfig, DensityBudgetConfig, SortDirection};
        use tylertoo_core::overview::convert::{parse_representation_spec, ConvertOptions};
        use tylertoo_core::overview::level::{MemoryProfile, Mode};
        use tylertoo_core::overview::simplify::{CollapseMode, SimplifyOptions};
        use tylertoo_core::overview::writer::RowGroupSizePolicy;

        let profile = match self.profile.as_str() {
            "auto" => MemoryProfile::Auto,
            "speed" => MemoryProfile::Speed,
            "bounded" => MemoryProfile::Bounded,
            other => anyhow::bail!("invalid --profile '{other}' (auto|speed|bounded)"),
        };

        let row_group_size_policy = match self.row_group_size_policy.as_str() {
            "constant" => RowGroupSizePolicy::Constant,
            "zoom-scaled" => RowGroupSizePolicy::ZoomScaled,
            other => {
                anyhow::bail!("invalid --row-group-size-policy '{other}' (constant|zoom-scaled)")
            }
        };

        // `--verbatim` supplies DEFAULTS, it does not override. Each knob
        // falls back to 0 (off) under the flag and to its ladder default
        // otherwise, so an explicitly-passed value always wins and
        // `--verbatim --simplify-factor 0.5` means what the docs say it means.
        // Applying it wholesale after the fact would silently discard the
        // override — the same mistake `--max-tile-size` already avoids.
        let ladder = AssignConfig::default();
        let tuned = |explicit: Option<f64>, default: f64| {
            explicit.unwrap_or(if self.verbatim { 0.0 } else { default })
        };

        // Cluster-conditional default: with --cluster, absorbed points are
        // summarized (point_count), so the sparser 16.0 grid is the better look.
        let point_thinning = tuned(
            self.point_thinning,
            if self.cluster {
                tylertoo_core::overview::assign::CLUSTER_POINT_THINNING_DEFAULT
            } else {
                ladder.point_thinning
            },
        );

        let assign = AssignConfig {
            point_thinning,
            line_thinning: tuned(self.line_thinning, ladder.line_thinning),
            polygon_thinning: tuned(self.polygon_thinning, ladder.polygon_thinning),
            line_visibility: tuned(self.line_visibility, ladder.line_visibility),
            polygon_visibility: tuned(self.polygon_visibility, ladder.polygon_visibility),
            sort_direction: SortDirection::Desc,
        };

        // --class-rank and --sort-key are mutually exclusive (also enforced in core).
        if self.class_rank.is_some() && self.sort_key.is_some() {
            anyhow::bail!("--class-rank and --sort-key are mutually exclusive");
        }
        let class_ranking = match &self.class_rank {
            Some(spec) => Some(parse_class_rank(spec)?),
            None => None,
        };

        // Entry-zoom ladder (#364): derived from a column, or spelled out.
        // clap enforces that the two are mutually exclusive.
        let entry_zoom = match (&self.magnitude_ladder, &self.entry_zoom) {
            (Some(col), _) => Some(EntryZoomSpec {
                column: col.clone(),
                kind: EntryZoomKind::DenseRank {
                    step: self.ladder_step,
                },
            }),
            (None, Some(spec)) => Some(parse_entry_zoom(spec)?),
            (None, None) => None,
        };

        // Clustering flags (Q4; also enforced in core).
        if !self.accumulate_attribute.is_empty() && !self.cluster {
            anyhow::bail!("--accumulate-attribute requires --cluster");
        }
        if self.verbatim && mode == Mode::Partitioning {
            anyhow::bail!(
                "--verbatim requires --mode duplicating: partitioning places each \
                 feature at exactly one level, so with thinning off every feature \
                 lands in the coarsest level and every finer level is empty"
            );
        }
        if self.cluster && mode == Mode::Partitioning {
            anyhow::bail!(
                "--cluster requires --mode duplicating: a partitioning-mode feature has \
                 one row read across many zoom prefixes, so a per-level point_count \
                 cannot be represented without double counting"
            );
        }
        let accumulate = self
            .accumulate_attribute
            .iter()
            .map(|s| parse_accumulate(s))
            .collect::<Result<Vec<_>>>()?;

        // Coalescing flags (Q3). Coalescing is ON by default (opt out with
        // --no-coalesce-lines); with partitioning mode the default is silently
        // inert (core logs it), but an EXPLICIT --coalesce-lines request is an
        // error the user should hear about.
        if self.coalesce_lines && mode == Mode::Partitioning {
            anyhow::bail!(
                "--coalesce-lines requires --mode duplicating: partitioning places \
                 each feature exactly once with geometry verbatim, which a merged \
                 chain cannot satisfy"
            );
        }
        let coalesce_lines = !self.no_coalesce_lines && !self.verbatim;

        // Zoom-band representation selector (#317 / #279); structural
        // validity against the plan is enforced by core convert validation.
        let representation = match &self.representation {
            Some(spec) => parse_representation_spec(spec)
                .map_err(|e| anyhow::anyhow!("--representation: {e}"))?,
            None => Vec::new(),
        };

        let options = ConvertOptions {
            mode,
            levels,
            assign,
            sort_key: self.sort_key.clone(),
            entry_zoom: entry_zoom.clone(),
            class_ranking,
            no_auto_rank: self.no_auto_rank,
            simplify: SimplifyOptions {
                factor: tuned(
                    self.simplify_factor,
                    tylertoo_core::overview::simplify::DEFAULT_SIMPLIFY_FACTOR,
                ),
                // --collapse and --collapse-square are mutually exclusive
                // (clap conflicts_with); both default off = drop (#279).
                //
                // A magnitude ladder implies --collapse (#364): the ladder
                // admits a small-but-strong feature to a coarse level, and the
                // drop default would then delete it there for simplifying
                // below that level's tolerance — undoing the promotion the
                // caller asked for. An explicit --collapse-square still wins.
                // A ladder implies --collapse, but that is applied in core
                // (`convert_to_overviews_source_strategy`) so every caller
                // gets it — the Python bindings and direct library users
                // included, where the drop default silently deletes the
                // promoted features and can empty the coarsest level outright.
                collapse: if self.collapse_square {
                    CollapseMode::Square
                } else if self.collapse {
                    CollapseMode::Point
                } else {
                    CollapseMode::Drop
                },
                cascade: !self.no_cascade,
            },
            representation,
            density: DensityBudgetConfig {
                // No positive spelling exists for either, so `--verbatim`
                // cannot be overriding an explicit request here.
                enabled: !self.no_density_drop && !self.verbatim,
                drop_rate: self.drop_rate,
                gamma: self.drop_gamma,
            },
            gsd_base: self.gsd_base,
            cogp_compat_key: cogp_compat,
            max_row_group_size: self.row_group_size,
            row_group_size_policy,
            full_column_stats: self.full_column_stats,
            streaming: !self.no_streaming,
            read_batch_size: self.read_batch_size,
            profile,
            in_flight_batches: self.in_flight_batches,
            read_workers: self.read_workers,
            cluster: self.cluster,
            accumulate,
            coalesce_lines,
            coalesce_snap: self.coalesce_snap,
            coalesce_max_level_rows: self.coalesce_max_level_rows,
            coalesce_junction_angle: self.coalesce_junction_angle,
            bbox,
            filter: self.filter.clone(),
            properties: self.property_selection(),
            spill_dir: self.spill_dir.clone(),
            save_plan: self.save_plan.clone(),
            plan: self.plan.clone(),
            // Set by `tiles` from --shard / --shard-plan (#498); the shared
            // tuning set does not own it, since `overview` has no export to
            // restrict and so no shard to be.
            shard: None,
            shard_plan_digest: None,
            // Likewise #541's convert-side level ceiling: it exists to make
            // `tiles --shard coarse` stop at the pivot, and `overview` has
            // no shard role to derive one from.
            zoom_ceiling: None,
        };

        // Logged because "no features were dropped" is a surprising thing to
        // infer from a quiet run — and the message has to tell the truth about
        // a composed run, where an override means the output is NOT verbatim.
        if self.verbatim {
            if options.is_verbatim() {
                log::info!(
                    "[convert] --verbatim: generalization off (no thinning, no \
                     visibility gates, no simplification, no density budget, no \
                     line coalescing); every level reproduces the input"
                );
            } else {
                log::info!(
                    "[convert] --verbatim with overrides: generalization is off \
                     except where you set it explicitly, so some features or \
                     vertices may still be dropped"
                );
            }
        }
        Ok(options)
    }
}

/// Arguments for `tylertoo validate`.
#[derive(Parser, Debug)]
struct ValidateArgs {
    /// GeoParquet overview file to validate.
    #[arg(value_name = "FILE")]
    file: PathBuf,

    /// Not supported here (single-file subcommand); accepted so the error
    /// can point at `overview`/`tiles` instead of clap's generic message.
    #[arg(long, value_name = "PATH", hide = true)]
    files_from: Option<PathBuf>,
}

/// Arguments for `tylertoo stats`.
#[derive(Parser, Debug)]
struct StatsArgs {
    /// PMTiles archive to report on.
    #[arg(value_name = "ARCHIVE")]
    archive: PathBuf,

    /// How many of the largest tiles (by stored size) to list.
    #[arg(long, value_name = "N", default_value = "10")]
    largest: usize,

    /// Print the report as JSON instead of a human-readable table.
    #[arg(long)]
    json: bool,
}

/// Arguments for `tylertoo tiles`: `overview` into a temporary file,
/// then `export-pmtiles` from it.
#[derive(Parser, Debug)]
struct TilesArgs {
    /// Input GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator
    /// (`EPSG:3857`). Pass a file, directory, glob, URL, or `s3://` or
    /// `gs://` prefix, or omit it with `--files-from`.
    #[arg(value_name = "INPUT", required_unless_present = "files_from")]
    input: Option<PathBuf>,

    /// Output PMTiles file. Omit it with `--plan-only`, which writes no
    /// archive.
    #[arg(
        value_name = "OUTPUT",
        required_unless_present_any = ["files_from", "plan_only"]
    )]
    output: Option<PathBuf>,

    /// Convert the `.parquet` files this manifest lists instead of `INPUT`.
    /// Give one path or URL per line, in dataset row order. Blank lines and
    /// `#` lines do nothing.
    #[arg(long, value_name = "PATH")]
    files_from: Option<PathBuf>,

    /// Minimum (coarsest) Web Mercator zoom.
    #[arg(long, default_value = "0")]
    min_zoom: u8,

    /// Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a
    /// sample of the input. Ignored with `--gsd`.
    #[arg(long, default_value = "14")]
    max_zoom: MaxZoom,

    /// Comma-separated ground sample distances (GSDs) in meters, each
    /// smaller than the last. Overrides `--min-zoom` and `--max-zoom`.
    #[arg(long, value_name = "GSDS")]
    gsd: Option<String>,

    /// Convert only features whose bbox intersects this lon/lat box. Row
    /// groups outside the box go unread.
    #[arg(long, value_name = "XMIN,YMIN,XMAX,YMAX")]
    bbox: Option<String>,

    /// Layer name for the tiles. Defaults to the input's file stem.
    #[arg(long)]
    layer_name: Option<String>,

    /// Per-tile MVT size cap, such as `500K` or `1M`, or 0 for no cap
    /// (default 500K, or no cap with `--verbatim`). A tile over the cap drops
    /// features until it fits.
    #[arg(long, value_name = "SIZE", visible_alias = "tile-size-limit", value_parser = parse_size_bytes)]
    max_tile_size: Option<usize>,

    /// Clip every polygon with the full overlay instead of the fast path for
    /// simple rings. Use it for byte-stable tiles: the fast path renders the
    /// same but can start a ring at a different vertex.
    #[arg(long)]
    no_simple_clip_fastpath: bool,

    /// Edge buffer around each tile, in tile pixels, so features continue
    /// across tile seams. At most 256 (one tile width).
    #[arg(
        long,
        default_value_t = tylertoo_core::overview::export::ExportOptions::default().tile_buffer
    )]
    tile_buffer: u32,

    /// Partitions to export at once, or `auto` to size the wave to the cores
    /// and free memory. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>.
    #[arg(long, value_name = "N|auto", default_value = "auto", value_parser = parse_partition_wave)]
    partition_wave: usize,

    /// Within-tile feature order: `input` keeps source row order, and a
    /// property name, optionally with `:asc` or `:desc`, sorts each tile by
    /// it. A renderer paints in this order unless a style overrides it.
    #[arg(long, value_name = "input|COLUMN[:asc|:desc]", default_value = "input")]
    feature_order: FeatureOrder,

    /// Write this integer column as each feature's MVT id, so
    /// `setFeatureState` keys work across tiles and zooms. Every row
    /// must hold a value from 0 to 2^64-1. Without it, tiles keep
    /// tile-local ids.
    #[arg(long, value_name = "COLUMN")]
    feature_id: Option<String>,

    /// Write a JSON report with `convert` and `export` sections, matching
    /// the reports of `overview` and `export-pmtiles`.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

    /// Keep the intermediate overview GeoParquet at PATH instead of
    /// deleting it after the export. The PMTiles output is the same.
    #[arg(long, value_name = "PATH")]
    keep_overview: Option<PathBuf>,

    /// Build one job of a sharded fleet: `I/N` for data shard I of N, or
    /// `coarse` for the zooms below the pivot. Requires `--shard-plan`, and
    /// data shards also need `--plan`. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#sharded-builds>.
    #[arg(
        long,
        value_name = "I/N|coarse",
        requires = "shard_plan",
        help_heading = "Sharded builds"
    )]
    shard: Option<String>,

    /// The shard plan from `tylertoo shard-plan` that every job shares.
    #[arg(long, value_name = "PATH", help_heading = "Sharded builds")]
    shard_plan: Option<PathBuf>,

    /// Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at
    /// one zoom, plus their descendants. Prefer `--shard`, which also skips
    /// input the range cannot reach.
    #[arg(
        long,
        value_name = "LO..HI",
        conflicts_with = "shard",
        help_heading = "Sharded builds"
    )]
    tile_range: Option<String>,

    /// Write the convert plan (`--save-plan`) and stop, with no export and
    /// no OUTPUT. Use it for a coarse job whose tiles the fleet discards.
    #[arg(long, requires = "save_plan", help_heading = "Sharded builds")]
    plan_only: bool,

    /// Print per-level and per-zoom breakdowns.
    #[arg(short, long)]
    verbose: bool,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    force: bool,

    #[command(flatten)]
    tuning: ConvertTuningArgs,
}

fn main() -> Result<()> {
    // Initialize dhat profiler if feature is enabled
    // This must be at the very start of main() - the profiler outputs
    // dhat-heap.json on Drop (program exit)
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    // Backward-compatible bare invocation: `tylertoo input.parquet out.pmtiles`
    // is rewritten to `tylertoo tiles input.parquet out.pmtiles` when the first
    // positional token is not a known subcommand (and not --help/--version).
    let cli = Cli::parse_from(rewrite_bare_args(std::env::args_os()));

    match cli.command {
        Command::Overview(args) => run_overview(*args),
        Command::Validate(args) => run_validate(args),
        Command::ExportPmtiles(args) => run_export_pmtiles(args),
        Command::Decode(args) => run_decode(args),
        Command::Stats(args) => run_stats(args),
        Command::Pyramid(args) => run_pyramid(args),
        Command::Merge(args) => run_merge(args),
        Command::ShardPlan(args) => run_shard_plan(args),
        Command::Tiles(args) => run_tiles(*args),
        #[cfg(feature = "gen-docs")]
        Command::GenReferenceDocs => {
            print!("{}", gen_reference_markdown());
            Ok(())
        }
    }
}

/// Render the whole clap command tree as Markdown for the CLI reference page.
///
/// Single source of truth is the `#[command]`/`#[arg]` help strings on the
/// clap types in this file — the reference is generated, never hand-edited.
#[cfg(feature = "gen-docs")]
fn gen_reference_markdown() -> String {
    let options = clap_markdown::MarkdownOptions::new()
        .title("CLI reference".to_string())
        .show_footer(false)
        .show_table_of_contents(true);
    let body = clap_markdown::help_markdown_custom::<Cli>(&options);
    format!(
        "<!-- GENERATED FILE — do not edit by hand.\n     \
         Regenerate: cargo run -p tylertoo --features gen-docs -- \
         gen-reference-docs > docs/reference/cli.md\n     \
         CI fails if this file drifts from the clap definitions. -->\n\n{body}"
    )
}

/// Insert an implicit `tiles` subcommand for the backward-compatible bare form.
///
/// If the first non-flag token is already a subcommand (`tiles`/`overview`/
/// `validate`/`help`) or the invocation is a help/version query, the arguments
/// are returned unchanged.
fn rewrite_bare_args<I>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    // `gen-reference-docs` is listed unconditionally so the bare-form rewrite
    // never prepends `tiles` to it. When the `gen-docs` feature is off, clap
    // rejects it as unknown (correct); when on, it routes to the docs generator.
    const SUBCOMMANDS: [&str; 11] = [
        "tiles",
        "overview",
        "validate",
        "export-pmtiles",
        "decode",
        "stats",
        "pyramid",
        "merge",
        "shard-plan",
        "gen-reference-docs",
        "help",
    ];
    let argv: Vec<std::ffi::OsString> = args.into_iter().collect();

    // Nothing to rewrite for a bare `tylertoo` (clap prints help/usage).
    if argv.len() <= 1 {
        return argv;
    }

    let first_positional = argv
        .iter()
        .skip(1)
        .find(|a| !a.to_string_lossy().starts_with('-'));
    let is_subcommand = first_positional
        .map(|a| SUBCOMMANDS.contains(&a.to_string_lossy().as_ref()))
        .unwrap_or(false);
    let is_help_or_version = argv.iter().skip(1).any(|a| {
        matches!(
            a.to_string_lossy().as_ref(),
            "-h" | "--help" | "-V" | "--version"
        )
    });

    if is_subcommand || is_help_or_version {
        return argv;
    }

    let mut rewritten = Vec::with_capacity(argv.len() + 1);
    rewritten.push(argv[0].clone());
    rewritten.push(std::ffi::OsString::from("tiles"));
    rewritten.extend(argv.into_iter().skip(1));
    rewritten
}

/// The resolved input shape of `tiles`/`overview`.
#[derive(Debug)]
enum InputSpec {
    /// Positional INPUT: file, directory, glob, or URL/prefix.
    Path(PathBuf),
    /// `--files-from` manifest of explicit files/URLs.
    Manifest(PathBuf),
}

impl InputSpec {
    /// The input as the user wrote it, for messages that name it in full
    /// (a re-runnable command line, not a summary line).
    fn display(&self) -> String {
        match self {
            InputSpec::Path(p) => p.display().to_string(),
            InputSpec::Manifest(p) => format!("--files-from {}", p.display()),
        }
    }

    /// Label for summary lines (input file name or manifest file name).
    fn label(&self) -> String {
        let p = match self {
            InputSpec::Path(p) | InputSpec::Manifest(p) => p,
        };
        p.file_name().unwrap_or_default().to_string_lossy().into()
    }
}

/// Sort out positional INPUT/OUTPUT vs `--files-from`, shared by `tiles`
/// and `overview`. Without `--files-from`, clap enforces both positionals.
/// With it, exactly ONE positional is expected — the OUTPUT, which clap
/// slots into the INPUT position; a second positional means INPUT was also
/// given, which conflicts with the manifest.
fn resolve_io(
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    files_from: Option<PathBuf>,
) -> Result<(InputSpec, PathBuf)> {
    match files_from {
        None => match (input, output) {
            (Some(i), Some(o)) => Ok((InputSpec::Path(i), o)),
            // Unreachable via clap (`required_unless_present`), kept for
            // direct callers/tests.
            _ => anyhow::bail!("INPUT and OUTPUT are required"),
        },
        Some(manifest) => match (input, output) {
            (Some(out), None) => Ok((InputSpec::Manifest(manifest), out)),
            (Some(input), Some(output)) => anyhow::bail!(
                "--files-from conflicts with the positional INPUT: got both the \
                 manifest and {:?}; pass only the OUTPUT ({:?})",
                input,
                output
            ),
            (None, _) => {
                anyhow::bail!("missing OUTPUT (usage: --files-from <PATH> OUTPUT)")
            }
        },
    }
}

/// `shard-plan`'s input resolution: a positional INPUT or a `--files-from`
/// manifest, exactly the pair `tiles` accepts, minus the OUTPUT (which
/// `shard-plan` takes as `-o`).
fn resolve_io_for_planning(
    input: Option<PathBuf>,
    files_from: Option<PathBuf>,
) -> Result<InputSpec> {
    match (input, files_from) {
        (Some(i), None) => Ok(InputSpec::Path(i)),
        (None, Some(m)) => Ok(InputSpec::Manifest(m)),
        (Some(i), Some(m)) => anyhow::bail!(
            "--files-from conflicts with the positional INPUT: got both {} and the manifest \
             {}; pass one",
            i.display(),
            m.display()
        ),
        // Unreachable via clap (`required_unless_present`), kept for direct
        // callers/tests.
        (None, None) => anyhow::bail!("INPUT is required (or --files-from <PATH>)"),
    }
}

/// Convert via the core pipeline, dispatching on the input shape: a path
/// (file/dir/glob/URL/prefix, resolved by core) or a `--files-from`
/// manifest (explicit ordered file list, order preserved verbatim).
fn run_convert(
    spec: &InputSpec,
    output: &std::path::Path,
    options: &tylertoo_core::overview::convert::ConvertOptions,
) -> Result<tylertoo_core::overview::convert::ConvertReport> {
    run_convert_typed(spec, output, options)
        .map_err(|e| anyhow::anyhow!("overview conversion failed: {e}"))
}

/// [`run_convert`] keeping the typed error.
///
/// `tiles --shard` has to tell "this shard owns no rows" (a legal, expected
/// outcome) apart from every other failure, and the `anyhow` wrapper above
/// erases exactly that distinction.
fn run_convert_typed(
    spec: &InputSpec,
    output: &std::path::Path,
    options: &tylertoo_core::overview::convert::ConvertOptions,
) -> std::result::Result<
    tylertoo_core::overview::convert::ConvertReport,
    tylertoo_core::overview::convert::ConvertError,
> {
    use tylertoo_core::input_set::ConvertSource;
    use tylertoo_core::overview::convert::{
        convert_to_overviews, convert_to_overviews_sources, ConvertError,
    };

    match spec {
        InputSpec::Path(p) => convert_to_overviews(p, output, options),
        InputSpec::Manifest(m) => ConvertSource::from_manifest(m)
            .map_err(ConvertError::from)
            .and_then(|source| convert_to_overviews_sources(&source, output, options)),
    }
}

/// One line naming which half of the tile space this job owns (#498), logged
/// before any work so a fleet's logs say what each task was for.
fn log_shard_job(job: &ShardJob, min_zoom: u8, max_zoom: u8) {
    log::info!(
        "[tiles] shard job {}: {}",
        job.role,
        match job.range {
            Some(r) => format!("zooms z{}..=z{max_zoom} of tile range {r}", job.pivot),
            None => format!("zooms z{min_zoom}..=z{}", job.pivot.saturating_sub(1)),
        }
    );
}

/// Finish an empty data shard: a valid tile-less archive, a clear line about
/// why, and exit 0 (#498).
///
/// See [`tylertoo_core::overview::export::write_empty_archive`] for why this
/// is a success rather than a failure.
fn write_empty_shard(
    output: &std::path::Path,
    layer_name: &str,
    job: &ShardJob,
    range: tylertoo_core::shard::TileRange,
    max_zoom: u8,
    why: &tylertoo_core::overview::convert::ConvertError,
) -> Result<()> {
    tylertoo_core::overview::export::write_empty_archive(
        output,
        layer_name,
        range.pivot_zoom(),
        max_zoom,
    )
    .map_err(|e| anyhow::anyhow!("failed to write the empty shard archive: {e}"))?;
    log::info!(
        "[tiles] shard {} owns no input rows ({why}); wrote an empty archive",
        job.role
    );
    println!(
        "✓ shard {} owns no input rows; wrote an empty archive at {}",
        job.role,
        output.display()
    );
    println!(
        "  z{}..z{max_zoom} declared, 0 tiles. This is normal for a cut whose data is \
         concentrated elsewhere — `tylertoo merge` skips a tile-less input.",
        range.pivot_zoom(),
    );
    Ok(())
}

/// Finish an empty coarse job: a valid tile-less archive over the zooms it
/// owns (`--min-zoom` up to just below the pivot), and exit 0 (#541 review).
///
/// The coarse job's convert-plan (`--save-plan`) has already been written by
/// the time this runs, so the fleet is intact; the coarse half simply holds
/// no tile. `tylertoo merge` skips a tile-less input.
fn write_empty_coarse(
    output: &std::path::Path,
    layer_name: &str,
    job: &ShardJob,
    min_zoom: u8,
    why: &tylertoo_core::overview::convert::ConvertError,
) -> Result<()> {
    let max_zoom = job.pivot.saturating_sub(1);
    tylertoo_core::overview::export::write_empty_archive(output, layer_name, min_zoom, max_zoom)
        .map_err(|e| anyhow::anyhow!("failed to write the empty coarse archive: {e}"))?;
    log::info!(
        "[tiles] shard job {} has nothing to write ({why}); wrote an empty archive",
        job.role
    );
    println!(
        "✓ coarse job has no features at z{min_zoom}..z{max_zoom}; wrote an empty archive at {}",
        output.display()
    );
    println!(
        "  Every feature first appears at or past the pivot z{}, so the data shards hold \
         all of it. Any --save-plan was written as usual; `tylertoo merge` skips a \
         tile-less input.",
        job.pivot
    );
    Ok(())
}

/// `true` when a convert failed only because it had nothing to write.
///
/// Two spellings of the same outcome, depending on how far the run got before
/// it noticed: `NoData` when no level has a winner, `AllLevelsEmpty` when
/// every declared level turned out empty at write time. Both mean "zero rows
/// reached the output", which for a data shard is not an error at all.
fn convert_produced_nothing(e: &tylertoo_core::overview::convert::ConvertError) -> bool {
    use tylertoo_core::overview::convert::ConvertError;
    use tylertoo_core::overview::writer::WriterError;
    matches!(
        e,
        ConvertError::NoData | ConvertError::Writer(WriterError::AllLevelsEmpty { .. })
    )
}

/// Resolve `--max-zoom auto` (#444) for a run whose `options` were built with
/// [`MaxZoom::plan_zoom`] and whose cheap checks have all passed. Core owns
/// the whole rule ([`MaxZoom::resolve`]); a `Fixed` zoom returns verbatim
/// without opening the input. For `auto` a dedicated source is resolved, so
/// the conversion's own source (and any column selection on it) is untouched.
fn resolve_max_zoom(
    max_zoom: MaxZoom,
    spec: &InputSpec,
    options: &mut tylertoo_core::overview::convert::ConvertOptions,
) -> Result<u8> {
    if !max_zoom.is_auto() {
        return Ok(max_zoom.plan_zoom());
    }
    let source = resolve_convert_source(spec)?;
    Ok(max_zoom.resolve(&source, options)?)
}

/// Resolve the level plan shared by `overview` and `tiles`: an explicit
/// `--gsd` list (comma-separated meters, strictly decreasing) overrides the
/// `--min-zoom`/`--max-zoom` range. Kept in one place so the two commands
/// can never drift on how GSD ladders are parsed.
fn resolve_level_plan(
    gsd: Option<&str>,
    min_zoom: u8,
    max_zoom: u8,
) -> Result<tylertoo_core::overview::convert::LevelPlan> {
    use tylertoo_core::overview::convert::LevelPlan;

    match gsd {
        Some(gsd_str) => {
            let gsds = gsd_str
                .split(',')
                .map(|s| s.trim().parse::<f64>())
                .collect::<std::result::Result<Vec<f64>, _>>()
                .map_err(|e| anyhow::anyhow!("invalid --gsd list '{}': {}", gsd_str, e))?;
            Ok(LevelPlan::Gsds(gsds))
        }
        None => Ok(LevelPlan::ZoomRange { min_zoom, max_zoom }),
    }
}

/// Where the removed-after-export intermediate overview of a `tiles` run
/// lives (#314): `--spill-dir` if given (one knob for everything tylertoo
/// puts on scratch disk), else an explicitly-set non-empty `$TMPDIR`, else
/// next to the output (same filesystem as the final artifact), else the
/// process temp dir. Pure — the env value is injected — so the precedence
/// is unit-testable.
fn resolve_intermediate_dir(
    spill_dir: Option<&std::path::Path>,
    tmpdir_env: Option<&std::ffi::OsStr>,
    output: &std::path::Path,
) -> PathBuf {
    if let Some(dir) = spill_dir {
        return dir.to_path_buf();
    }
    if let Some(tmpdir) = tmpdir_env.filter(|t| !t.is_empty()) {
        return PathBuf::from(tmpdir);
    }
    output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
}

/// Lower-bound estimate of the intermediate overview's size for the #314
/// free-space preflight: the local input bytes. A duplicating overview
/// embeds the finest level verbatim on top of the coarse levels, so it is
/// at least input-sized. Local file → its size; local directory → sum of
/// its top-level `.parquet` entries; `--files-from` manifest → sum of the
/// entries that resolve as local files. Remote/glob shapes return `None`
/// (the preflight stays quiet rather than guessing).
fn estimate_local_input_bytes(spec: &InputSpec) -> Option<u64> {
    fn file_len(p: &std::path::Path) -> Option<u64> {
        std::fs::metadata(p)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len())
    }
    match spec {
        InputSpec::Path(p) => {
            let meta = std::fs::metadata(p).ok()?;
            if meta.is_file() {
                return Some(meta.len());
            }
            if !meta.is_dir() {
                return None;
            }
            let sum: u64 = std::fs::read_dir(p)
                .ok()?
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "parquet"))
                .filter_map(|e| file_len(&e.path()))
                .sum();
            (sum > 0).then_some(sum)
        }
        InputSpec::Manifest(m) => {
            let text = std::fs::read_to_string(m).ok()?;
            let sum: u64 = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .filter_map(|l| file_len(std::path::Path::new(l)))
                .sum();
            (sum > 0).then_some(sum)
        }
    }
}

/// #314 preflight safety margin, matching the remote-spill preflight
/// (#272): warn when free space is below the estimate plus 1/20th (5%).
const INTERMEDIATE_MARGIN_DENOM: u64 = 20;

/// #314 free-space preflight decision + message (pure, unit-testable).
/// Returns the warning to emit when the estimated intermediate overview
/// (a lower bound — the local input bytes) plus a 5% margin exceeds
/// `available_bytes` on the volume holding `dir`, or `None` when it fits
/// (or the estimate is unknown/zero).
fn intermediate_space_warning(
    estimated_bytes: u64,
    available_bytes: u64,
    dir: &std::path::Path,
) -> Option<String> {
    if estimated_bytes == 0 {
        return None;
    }
    let need = estimated_bytes + estimated_bytes / INTERMEDIATE_MARGIN_DENOM;
    if available_bytes >= need {
        return None;
    }
    let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    Some(format!(
        "the intermediate overview `tiles` materializes (≥ ~{:.1} GiB — a \
         duplicating overview is at least as large as the input, whose local \
         size is the estimate) may not fit: {} has {:.1} GiB free ({:.1} GiB \
         short, including a 5% margin). Point --spill-dir (or --keep-overview) \
         at a roomier volume, or free up space first.",
        gib(estimated_bytes),
        dir.display(),
        gib(available_bytes),
        gib(need - available_bytes),
    ))
}

/// Emit the #314 intermediate free-space preflight warning, if warranted:
/// probe the volume holding `dir` and compare against the local-input-bytes
/// estimate. Quiet when either side is unknowable (remote input, exotic
/// filesystem) rather than crying wolf.
fn warn_intermediate_space(spec: &InputSpec, dir: &std::path::Path) {
    let Some(estimated) = estimate_local_input_bytes(spec) else {
        return;
    };
    let Ok(available) = fs4::available_space(dir) else {
        return;
    };
    if let Some(msg) = intermediate_space_warning(estimated, available, dir) {
        log::warn!("{msg}");
    }
}

/// Run `tylertoo tiles`: the one-shot GeoParquet → PMTiles facade.
///
/// Chains the two product pipelines through a materialized intermediate
/// overview file (`overview` convert → `export-pmtiles`) — this is NOT
/// zero-disk. The intermediate is retained at `--keep-overview PATH` when
/// given; otherwise it is a temp file (in `--spill-dir` / `$TMPDIR` / the
/// output directory, in that order) removed on both success and failure
/// via [`tempfile::NamedTempFile`]'s drop guard. Its path and size are
/// always logged, and a free-space preflight warns when the chosen volume
/// looks too small for it (#314).
/// Which job of a sharded fleet this `tiles` run is, and the slice of the
/// tile space it owns (#498).
#[derive(Debug, Clone)]
struct ShardJob {
    role: tylertoo_core::shard::ShardRole,
    /// The data shard's range, or `None` for the coarse job — which owns the
    /// zooms below the pivot instead, and expresses that as an export ceiling.
    range: Option<tylertoo_core::shard::TileRange>,
    pivot: u8,
    /// `ShardPlan::cut_digest_hex` — the fleet-wide identity of the cut.
    cut_digest: String,
    /// The bound plan, kept so a `--max-zoom auto` run can re-check the
    /// pivot once the real zoom is known (#444).
    plan: tylertoo_core::shard::ShardPlan,
}

/// Resolve `--shard` / `--shard-plan` into a [`ShardJob`], failing fast on
/// everything that can be known before a byte of input is read.
///
/// The shard plan is verified against the input here as well, so a plan cut
/// for a different (or since-rewritten) file is an error in milliseconds
/// rather than a fleet of jobs that each tile something slightly different.
fn resolve_shard_job(
    spec: &InputSpec,
    shard: Option<&str>,
    shard_plan: Option<&Path>,
    min_zoom: u8,
    max_zoom: u8,
    plan_only: bool,
) -> Result<Option<ShardJob>> {
    use tylertoo_core::shard::ShardRole;

    let Some(shard) = shard else {
        // clap's `requires` makes the reverse pairing impossible, but a plan
        // with no role is a silent no-op worth naming.
        anyhow::ensure!(
            shard_plan.is_none(),
            "--shard-plan needs --shard to say which job this is: `--shard coarse` for the run \
             that owns the zooms coarser than the pivot and writes the convert plan, or \
             `--shard I/N` \
             for data shard I"
        );
        return Ok(None);
    };
    let plan_path = shard_plan.expect("clap requires --shard-plan alongside --shard");
    let role = ShardRole::parse(shard)?;

    // Bind the plan to the input, the same way the convert plan binds itself:
    // a plan cut for a different (or since-rewritten) file is an error here
    // rather than a fleet that each tiles something slightly different.
    let source = resolve_convert_source(spec)?;
    let (plan, range) = tylertoo_core::shard::resolve_range(plan_path, role, Some(&source))?;

    // Core owns the pivot-vs-finest-zoom rule (inclusive: pivot == max_zoom
    // leaves every shard exactly one zoom, which is legal), so the Rust API
    // gets the same guard and the CLI only surfaces it.
    plan.check_max_zoom(max_zoom, plan_path)?;
    // The coarse job's own half, checked with the same "before a byte is
    // read" discipline. It owns [min_zoom, pivot - 1]; a pivot at or below
    // the requested minimum leaves it nothing, and without this the whole
    // convert runs — potentially for hours — before the export refuses an
    // empty zoom restriction.
    //
    // #560: not under --plan-only, which builds no zoom at all — the coarse
    // job there is only the plan's writer. A pivot AT --min-zoom is exactly
    // the handover shape (an external archive owns every zoom below the
    // pivot, and the fleet runs --min-zoom = pivot), so refusing it would
    // refuse the one build plan-only exists for.
    if matches!(role, ShardRole::Coarse) && !plan_only {
        anyhow::ensure!(
            plan.pivot_zoom > min_zoom,
            "--shard coarse with --shard-plan {} has no zoom to build: the coarse job owns \
             z{min_zoom}..z{}, and the plan's pivot is z{}. Lower --min-zoom, or re-cut the \
             plan with a finer --pivot.",
            plan_path.display(),
            plan.pivot_zoom.saturating_sub(1),
            plan.pivot_zoom,
        );
    }
    Ok(Some(ShardJob {
        role,
        range,
        pivot: plan.pivot_zoom,
        // #498: the digest of the CUT, stamped into the convert plan's
        // fingerprint so the fleet is bound to one `shards.json` by
        // construction. Set on the coarse job (which writes the plan) and on
        // every data shard (which must present the same cut).
        cut_digest: plan.cut_digest_hex(),
        plan,
    }))
}

/// Resolve an [`InputSpec`] to the core's [`ConvertSource`], the one way.
///
/// Shared by `tiles --shard` and `shard-plan` so the two can never disagree
/// about what an input *is* — a shard plan cut over a manifest has to bind to
/// the same part list the shard jobs will read.
fn resolve_convert_source(spec: &InputSpec) -> Result<tylertoo_core::input_set::ConvertSource> {
    use tylertoo_core::input_set::ConvertSource;
    Ok(match spec {
        InputSpec::Path(p) => ConvertSource::resolve_path(p)?,
        InputSpec::Manifest(p) => ConvertSource::from_manifest(p)?,
    })
}

/// Output gate for every subcommand that writes a file (#551 for `tiles`,
/// #427 for `overview`, `export-pmtiles` and `decode`): an existing file is
/// refused unless `force`. A directory can never be replaced by the final
/// rename, `--force` or not, so it is refused up front instead of after a
/// whole convert + export.
fn check_output_path(output: &Path, kind: OutputKind, force: bool) -> Result<()> {
    if output.is_dir() {
        anyhow::bail!(
            "{} is a directory; the output must be a {kind} file path",
            output.display()
        );
    }
    if output.exists() && !force {
        anyhow::bail!("{} exists (use --force to overwrite)", output.display());
    }
    Ok(())
}

/// What a subcommand's output is, for [`check_output_path`]'s message.
#[derive(Clone, Copy, Debug)]
enum OutputKind {
    Pmtiles,
    GeoParquet,
}

impl std::fmt::Display for OutputKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OutputKind::Pmtiles => "PMTiles",
            OutputKind::GeoParquet => "GeoParquet",
        })
    }
}

/// `--spill-dir` preflight (#427), shared wording with the convert side
/// (`ConvertOptions` validation) so the two subcommands fail the same way.
/// Core checks it again inside `export_pmtiles`; this runs first so a bad
/// directory is reported before the input is even looked at.
fn check_spill_dir(spill_dir: Option<&Path>) -> Result<()> {
    if let Some(dir) = spill_dir {
        if !dir.is_dir() {
            anyhow::bail!("spill-dir {} is not an existing directory", dir.display());
        }
    }
    Ok(())
}

/// `--feature-order`'s and `--feature-id`'s (#443) columns are read at
/// export, after `tiles`' own convert step has already applied the property
/// selection — an excluded column is gone from the intermediate before
/// export ever sees it, so this is checked here, before any work is done,
/// rather than after running the whole convert and quietly falling back
/// (same wording as export's `PropertyRequiredByKnob`).
fn reject_excluded_knob_columns(
    options: &tylertoo_core::overview::convert::ConvertOptions,
    feature_order: &FeatureOrder,
    feature_id: Option<&String>,
) -> Result<()> {
    if let FeatureOrder::Column { name, .. } = feature_order {
        anyhow::ensure!(
            options.properties.keeps(name),
            "property {name:?} is excluded but --feature-order reads it; keep it in the \
             selection or drop the knob"
        );
    }
    // #443: convert-time-only. This does not apply to export-pmtiles's OWN
    // --include-property/--exclude-property, which `ExportOptions::feature_id`
    // always overrides regardless (the column already exists in the
    // intermediate file either way).
    if let Some(name) = feature_id {
        anyhow::ensure!(
            options.properties.keeps(name),
            "property {name:?} is excluded but --feature-id reads it; keep it in the \
             selection or drop the flag"
        );
        // A clustered point's accumulated value is a sum/min/max/mean of
        // several ids -- the id of no feature. Export rejects it from the
        // file's clustering provenance too; checking here saves the convert.
        if let Some(spec) = options.accumulate.iter().find(|a| a.column == *name) {
            anyhow::bail!(
                "--feature-id column {name:?} is also --accumulate-attribute {name}:{}; a \
                 cluster's aggregated value identifies no single feature, so accumulate a \
                 different column",
                spec.op.as_str()
            );
        }
    }
    Ok(())
}

/// The convert half of `tiles`: the [`ConvertOptions`] the facade runs, the
/// sharded-fleet job they were derived from, and the finest zoom (`--max-zoom`,
/// with `auto` resolved, #444).
///
/// Shared by the full facade and `--plan-only` (#560). The convert plan is
/// fingerprinted over these options, so the plan a plan-only run writes is
/// byte-identical to a full run's only if both build the options the same way
/// — which is guaranteed here by construction rather than by review.
///
/// [`ConvertOptions`]: tylertoo_core::overview::convert::ConvertOptions
fn tiles_convert_options(
    args: &TilesArgs,
    spec: &InputSpec,
) -> Result<(
    tylertoo_core::overview::convert::ConvertOptions,
    Option<ShardJob>,
    u8,
)> {
    use tylertoo_core::overview::level::Mode;

    let bbox = args.bbox.as_ref().map(|s| parse_bbox(s)).transpose()?;

    // #444: until `--max-zoom auto` is resolved below — after every check that
    // does not need its value, so a typo or a stale shard plan still fails in
    // milliseconds (#371) — the plan carries `auto`'s placeholder (its
    // ceiling, the most permissive value it can resolve to). A fixed zoom is
    // its own placeholder, so that path is unchanged.
    let max_zoom = args.max_zoom.plan_zoom();

    // Overviews for PMTiles are always duplicating (partitioning can't be
    // exported to per-tile MVT). Every other convert knob comes from the
    // shared tuning set, so `tiles` matches the two-step overview → export.
    let levels = resolve_level_plan(args.gsd.as_deref(), args.min_zoom, max_zoom)?;
    let mut options = args
        .tuning
        .build_convert_options(Mode::Duplicating, levels, bbox, false)?;

    // #498: which job of a sharded fleet this is, and the slice of the tile
    // space it owns. Resolved before any work, against the plan every job of
    // the fleet shares, so a mis-specified `--shard 4/8` against a 16-way plan
    // fails in milliseconds rather than after an hour of tiling.
    let shard = resolve_shard_job(
        spec,
        args.shard.as_deref(),
        args.shard_plan.as_deref(),
        args.min_zoom,
        max_zoom,
        args.plan_only,
    )?;
    if let Some(job) = &shard {
        options.shard = job.range;
        // #498: fingerprinted, unlike `shard` itself — the cut is fleet-wide,
        // this job's slice of it is not. The coarse job writes it into the
        // plan; every shard has to present the same one.
        options.shard_plan_digest = Some(job.cut_digest.clone());
        // #541: the coarse job builds only the levels it exports. Its pass 1
        // and level assignment stay full-range, so the plan it saves is
        // byte-identical to a full run's and every data shard consumes it
        // unchanged. `resolve_shard_job` has already refused a pivot at or
        // below --min-zoom, and a GSD ladder has no zoom to cap against.
        //
        // Only on the streaming pipeline: `--no-streaming` (the in-memory
        // reference path) builds every planned level, so the coarse job falls
        // back to the uncapped convert there. Its tiles are identical either
        // way — the export's own ceiling below is what bounds them — it just
        // pays for the finer levels, which is what the user opted into.
        //
        // #560: and not at all under --plan-only, which materializes no level
        // whatsoever — core refuses a ceiling it has no pass 2 to apply it to.
        if !args.plan_only && job.range.is_none() && args.gsd.is_none() && options.streaming {
            options.zoom_ceiling = Some(job.pivot.saturating_sub(1));
        }
    }

    // #386/#443: the property selection is applied at convert, so an excluded
    // column is already gone from the intermediate before export would sort
    // or id by it — export-pmtiles rejects that pairing outright, and so must
    // the facade, before any work is done, rather than run the whole convert
    // and then quietly fall back. Also under --plan-only (#600): it exports
    // nothing, but it is the fleet's preflight, and the coarse job and every
    // data shard refuse this pairing.
    reject_excluded_knob_columns(&options, &args.feature_order, args.feature_id.as_ref())?;

    // #444: every cheap check has passed; now estimate `auto` (a fixed zoom
    // comes back verbatim, no I/O). Here, not in the callers, so a plan-only
    // run and the full coarse job resolve — and fingerprint — the same zoom.
    // A shard plan was bound against the placeholder, so its pivot is
    // re-checked against the real value.
    let max_zoom = resolve_max_zoom(args.max_zoom, spec, &mut options)?;
    if let (Some(job), Some(plan_path)) = (&shard, args.shard_plan.as_deref()) {
        job.plan.check_max_zoom(max_zoom, plan_path)?;
    }
    if let Some(job) = &shard {
        if args.plan_only {
            // The coarse job's usual line names the zooms it builds; this one
            // builds none (and its pivot may sit at --min-zoom, #560).
            log::info!(
                "[tiles] shard job {}: plan only — writes the convert plan for pivot z{}, \
                 builds no zoom",
                job.role,
                job.pivot
            );
        } else {
            log_shard_job(job, args.min_zoom, max_zoom);
        }
    }
    Ok((options, shard, max_zoom))
}

/// The export knobs `tiles` can check without reading anything (#433): the
/// `--tile-buffer` cap, the `--tile-range` syntax, and the `--keep-overview`
/// directory. Returns the parsed `--tile-range`.
///
/// Shared by the full run and `--plan-only` (#600): plan-only ignores these
/// flags, but it is the fleet's preflight, so a value the coarse job would
/// refuse in milliseconds is refused there too, not after the plan is
/// written and the data shards are queued.
fn preflight_export_flags(args: &TilesArgs) -> Result<Option<tylertoo_core::shard::TileRange>> {
    use tylertoo_core::overview::export::ExportOptions;

    // The full `ExportOptions` are built after the convert (the layer name
    // and shard range come out of it), so this is the same check on the
    // values that are already known.
    ExportOptions {
        tile_buffer: args.tile_buffer,
        ..ExportOptions::default()
    }
    .validate()
    .map_err(|e| anyhow::anyhow!("export failed: {e}"))?;

    let tile_range = args
        .tile_range
        .as_deref()
        .map(tylertoo_core::shard::TileRange::parse)
        .transpose()?;

    if let Some(path) = &args.keep_overview {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            anyhow::ensure!(
                parent.is_dir(),
                "--keep-overview directory {} does not exist",
                parent.display()
            );
        }
    }
    Ok(tile_range)
}

/// The export-only flags set on a `--plan-only` run, by their CLI names in
/// `--help` order (#600).
///
/// None of these is a convert option: `tiles_convert_options` builds the
/// `ConvertOptions` from the shared tuning set, the zoom range, `--gsd`,
/// `--bbox` and the shard job, and `options_digest` (the plan fingerprint) is
/// an allowlist over those. So a plan-only run ignores them and the plan is
/// the one the full coarse job writes with them. `--force` included: the
/// plan file is overwritten regardless, as on a full run.
///
/// A flag with a default counts as set when its value differs from the
/// default; typing the default value explicitly is the same run as omitting
/// it, so there is nothing to report.
fn ignored_export_options(args: &TilesArgs) -> Vec<&'static str> {
    use tylertoo_core::overview::export::{ExportOptions, FeatureOrder, PARTITION_WAVE_AUTO};

    [
        ("--layer-name", args.layer_name.is_some()),
        ("--max-tile-size", args.max_tile_size.is_some()),
        ("--no-simple-clip-fastpath", args.no_simple_clip_fastpath),
        (
            "--tile-buffer",
            args.tile_buffer != ExportOptions::default().tile_buffer,
        ),
        (
            "--partition-wave",
            args.partition_wave != PARTITION_WAVE_AUTO,
        ),
        (
            "--feature-order",
            args.feature_order != FeatureOrder::default(),
        ),
        ("--feature-id", args.feature_id.is_some()),
        ("--report", args.report.is_some()),
        ("--keep-overview", args.keep_overview.is_some()),
        ("--tile-range", args.tile_range.is_some()),
        ("--force", args.force),
    ]
    .into_iter()
    .filter_map(|(flag, set)| set.then_some(flag))
    .collect()
}

/// `tiles --plan-only` (#560): pass 1 + the level assignment, `--save-plan`,
/// stop. No archive, no intermediate overview, no export.
///
/// The fleet's coarse job splits into two products — the zooms below the pivot,
/// and the plan the data shards consume. When an external archive owns those
/// zooms (an aggregate-into-fields handover build) the tiles are thrown away,
/// and this is the job without them.
fn run_plan_only(args: TilesArgs) -> Result<()> {
    use tylertoo_core::overview::convert::{write_convert_plan, write_convert_plan_sources};

    // No OUTPUT is written, so none is accepted: a path on the command line
    // that nothing would ever create is a misunderstanding worth failing on,
    // not a silently ignored argument. (With --files-from the lone positional
    // lands in `input`, which `resolve_io_for_planning` reports on.)
    anyhow::ensure!(
        args.output.is_none(),
        "--plan-only writes no PMTiles archive, so it takes no OUTPUT: got {}. \
         The plan goes to --save-plan; drop the output path",
        args.output.as_ref().expect("checked").display()
    );
    let spec = resolve_io_for_planning(args.input.clone(), args.files_from.clone())?;

    // #600: export flags are ignored here, but not unchecked.
    preflight_export_flags(&args)?;

    let (options, shard, _max_zoom) = tiles_convert_options(&args, &spec)?;
    // A data shard reads a subset of the input, so a plan it wrote would
    // describe only that subset. Core refuses the pairing too (--shard with
    // --save-plan); named here against the flag the user actually typed.
    if let Some(job) = &shard {
        anyhow::ensure!(
            job.range.is_none(),
            "--plan-only cannot be combined with a data shard (--shard I/N): the shard reads \
             only the row groups its range reaches, so the plan it wrote would cover that \
             subset and be useless to the rest of the fleet. The plan comes from the job that \
             reads everything: `--shard coarse --shard-plan <the fleet's shard plan> \
             --plan-only`"
        );
    }
    // #600: the fleet recipe is the coarse job's line plus this flag, and that
    // line carries export-only flags. They cannot move the plan (see
    // `ignored_export_options`), so they are tolerated, and said so once,
    // now that every refusal has run.
    let ignored = ignored_export_options(&args);
    if !ignored.is_empty() {
        log::info!(
            "--plan-only: ignoring export options {}",
            ignored.join(", ")
        );
    }

    let save_plan = args
        .tuning
        .save_plan
        .clone()
        .expect("clap requires --save-plan alongside --plan-only");

    let start = std::time::Instant::now();
    let report = match &spec {
        InputSpec::Path(p) => write_convert_plan(p, &options),
        InputSpec::Manifest(m) => {
            let source = tylertoo_core::input_set::ConvertSource::from_manifest(m)?;
            write_convert_plan_sources(&source, &options)
        }
    }
    .context("writing the convert plan failed")?;

    println!("✓ Wrote the convert plan (no tiles: --plan-only)");
    println!("  input:  {}", spec.display());
    println!(
        "  plan:   {} ({})",
        save_plan.display(),
        HumanBytes(report.plan_bytes)
    );
    println!(
        "  rows:   {} input row(s) → {} feature(s) ({} point, {} line, {} polygon)",
        format_number(report.input_rows as u64),
        format_number(report.input_features as u64),
        format_number(report.points as u64),
        format_number(report.lines as u64),
        format_number(report.polygons as u64),
    );
    println!(
        "  levels: {} planned level(s) populated{}",
        report.levels.len(),
        if report.skipped_empty_levels.is_empty() {
            String::new()
        } else {
            format!(
                ", {} empty and omitted (z{})",
                report.skipped_empty_levels.len(),
                report
                    .skipped_empty_levels
                    .iter()
                    .map(|s| s.zoom.map_or_else(|| "-".to_string(), |z| z.to_string()))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
    );
    // The provenance an operator most often wants to check before spending
    // shard hours: which column decided every cell winner, and the ladder.
    println!(
        "  ranking: {}{}",
        report.ranking.mode,
        report
            .ranking
            .column
            .as_deref()
            .map_or_else(String::new, |c| format!(" on {c:?}"))
    );
    if let Some(spec) = &report.entry_zoom {
        println!("  entry-zoom ladder: {spec}");
    }
    if args.verbose {
        for l in &report.levels {
            println!(
                "  level {:<2} (z{:<2}) gsd {:>10.2} m: {:>9} feature(s)",
                l.planned_level,
                l.zoom.map_or_else(|| "-".to_string(), |z| z.to_string()),
                l.gsd,
                format_number(l.feature_count as u64),
            );
        }
    }
    println!(
        "  time:   {:.2}s total (pass 1 {:.2}s, assignment {:.2}s)",
        start.elapsed().as_secs_f64(),
        report.pass1_secs,
        report.assign_secs,
    );
    if shard.is_some() {
        println!(
            "  next:   give this plan to every data shard with --plan {}",
            save_plan.display()
        );
    } else {
        // No cut digest in the fingerprint, so a data shard (which presents
        // its shard plan's) refuses this plan — say so here rather than at
        // the first shard of the fleet.
        println!(
            "  next:   replay this plan in an unsharded run (`tiles --plan {0}` or \
             `overview --plan {0}`). It records no shard plan, so a fleet's data shards \
             refuse it: for a fleet, re-run with --shard coarse --shard-plan <the fleet's \
             shard plan>",
            save_plan.display()
        );
    }
    Ok(())
}

fn run_tiles(args: TilesArgs) -> Result<()> {
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // #560: the plan-writing half of the coarse job, on its own. Branches
    // before the OUTPUT is resolved, because there is none.
    if args.plan_only {
        return run_plan_only(args);
    }

    let (spec, output) = resolve_io(
        args.input.clone(),
        args.output.clone(),
        args.files_from.clone(),
    )?;

    check_output_path(&output, OutputKind::Pmtiles, args.force)?;

    // #433: the export knobs that need no file are checked before the
    // convert, which on a large input runs for minutes before the export
    // would otherwise refuse them.
    let manual_range = preflight_export_flags(&args)?;

    // Derive the layer name from the input if not given: file stem for a
    // single file, last path segment for a directory or s3://gs:// prefix,
    // last literal segment for a glob, manifest stem for --files-from
    // (core owns the rules — see input_set::derive_layer_name).
    let layer_name = args.layer_name.clone().unwrap_or_else(|| {
        let p = match &spec {
            InputSpec::Path(p) | InputSpec::Manifest(p) => p,
        };
        tylertoo_core::input_set::derive_layer_name(&p.to_string_lossy())
    });

    let (options, shard, max_zoom) = tiles_convert_options(&args, &spec)?;

    // A data shard's range prunes the convert's reads as well as the export;
    // a hand-written `--tile-range` restricts the export only (the two flags
    // conflict, so at most one is set).
    let tile_range = match &shard {
        Some(job) => job.range,
        None => manual_range,
    };

    // The data shard's own range, kept past `tile_range`'s move into
    // `ExportOptions`: the empty-shard branch below needs the pivot zoom.
    let shard_range = shard.as_ref().and_then(|job| job.range);

    // Intermediate overview file (#314): retained at --keep-overview when
    // given; otherwise a temp file in --spill-dir / $TMPDIR / the output
    // directory, removed on drop — success or failure alike.
    let (overview_path, overview_tmp): (PathBuf, Option<tempfile::NamedTempFile>) =
        match &args.keep_overview {
            // Its directory was checked by `preflight_export_flags`.
            Some(path) => (path.clone(), None),
            None => {
                let dir = resolve_intermediate_dir(
                    args.tuning.spill_dir.as_deref(),
                    std::env::var_os("TMPDIR").as_deref(),
                    &output,
                );
                let tmp = tempfile::Builder::new()
                    .prefix(".tylertoo-overview-")
                    .suffix(".parquet")
                    .tempfile_in(&dir)
                    .with_context(|| {
                        format!(
                            "failed to create the intermediate overview file in {} \
                             (location precedence: --spill-dir, $TMPDIR, the output \
                             directory)",
                            dir.display()
                        )
                    })?;
                (tmp.path().to_path_buf(), Some(tmp))
            }
        };

    // #314 free-space preflight: the intermediate is at least input-sized,
    // so warn up front when the chosen volume looks too small.
    let overview_dir = overview_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    warn_intermediate_space(&spec, &overview_dir);

    let convert_report = match run_convert_typed(&spec, &overview_path, &options) {
        Ok(report) => report,
        // #498: a shard whose range owns no input rows is a legal outcome of
        // a legal cut, not a failure. `shard-plan` deliberately cuts N ranges
        // whatever the data looks like — an empty RANGE is legal while a gap
        // is not — so a concentrated dataset routinely leaves some shards
        // with nothing. Failing them would mean an array job with red squares
        // in it that mean "correct", and a merge the operator has to
        // hand-edit. Write the valid empty archive the merge already tolerates
        // and exit 0.
        Err(e) if shard_range.is_some() && convert_produced_nothing(&e) => {
            let job = shard.as_ref().expect("a range implies a shard job");
            let range = shard_range.expect("checked by the guard");
            return write_empty_shard(&output, &layer_name, job, range, max_zoom, &e);
        }
        // #541 review: the coarse job's counterpart. Its convert stops at the
        // pivot, and when every feature first appears finer than that (#211
        // auto-clamp took every coarse level) there is nothing for it to
        // write — but pass 1, the assignment and `--save-plan` all ran and
        // are valid, and the shards still have their work. Same legal
        // outcome as an empty data shard, same answer.
        Err(e @ tylertoo_core::overview::convert::ConvertError::NothingAtOrBelowCeiling { .. }) => {
            let job = shard.as_ref().expect("only the coarse job sets a ceiling");
            return write_empty_coarse(&output, &layer_name, job, args.min_zoom, &e);
        }
        Err(e) => anyhow::bail!("overview conversion failed: {e}"),
    };

    // #314: the disk cost of the one-shot facade must not be silent — name
    // the intermediate's path and size, and its fate.
    let overview_bytes = std::fs::metadata(&overview_path)
        .map(|m| m.len())
        .unwrap_or(0);
    println!(
        "  intermediate overview: {} ({}{})",
        overview_path.display(),
        HumanBytes(overview_bytes),
        if overview_tmp.is_some() {
            ", removed after export; --keep-overview PATH retains it"
        } else {
            ", retained"
        }
    );

    if args.verbose {
        println!(
            "Overview: {} input features → {} rows across {} levels in {:.2}s",
            format_number(convert_report.input_features as u64),
            format_number(convert_report.total_rows as u64),
            convert_report.levels.len(),
            convert_report.duration_secs
        );
    }

    let export_opts = ExportOptions {
        layer_name,
        tile_buffer: args.tile_buffer,
        extent: 4096,
        tile_size_limit: resolve_tiles_size_limit(args.max_tile_size, args.tuning.verbatim),
        simple_clip_fastpath: !args.no_simple_clip_fastpath,
        partition_wave: args.partition_wave,
        feature_order: args.feature_order.clone(),
        // #380: the archive's `vector_layers` declares the zoom range that was
        // asked for, even when the coarsest levels generalized to nothing and
        // were omitted from the overview (the header stays the actual tiles,
        // #529/#522). Only a zoom plan has a requested minimum zoom.
        //
        // #498: a data shard owns no zoom coarser than the pivot, so that is
        // what it declares — claiming the coarse job's half would misdescribe
        // an archive that holds none of it.
        min_zoom: args.gsd.is_none().then_some(match tile_range {
            Some(r) => r.pivot_zoom(),
            None => args.min_zoom,
        }),

        // The property selection was applied on convert (#386): the
        // intermediate overview already carries only the kept columns.
        properties: Default::default(),

        // #498: the two complementary halves of a sharded build. A data shard
        // (or a hand-written --tile-range) takes its range; the coarse job
        // takes everything below the pivot.
        tile_range,
        zoom_ceiling: shard
            .and_then(|job| job.range.is_none().then(|| job.pivot.saturating_sub(1))),
        feature_id: args.feature_id.clone(),
        // #427: one scratch knob — the export's member spill goes where the
        // convert's spill (and the intermediate overview) go.
        spill_dir: args.tuning.spill_dir.clone(),
    };
    let export_report = export_pmtiles(&overview_path, &output, &export_opts)
        .map_err(|e| anyhow::anyhow!("export failed: {e}"))?;

    if args.verbose {
        for z in &export_report.zooms {
            println!(
                "  z{:<2} (level {}): {:>7} tiles, {:>9} features",
                z.zoom, z.level, z.tile_count, z.tile_feature_count
            );
        }
    }

    println!(
        "✓ Converted {} → {}",
        spec.label(),
        output.file_name().unwrap_or_default().to_string_lossy()
    );
    println!(
        "  {}",
        tiles_summary_line(
            export_report.total_tiles,
            export_report.min_zoom,
            export_report.max_zoom,
            convert_report.duration_secs + export_report.duration_secs,
            &SummaryLosses {
                out_of_range: convert_report.out_of_range_features,
                out_of_range_exemplars: &convert_report.out_of_range_exemplars,
                unprojectable: convert_report.unprojectable_features,
                encode_dropped: export_report.encode_dropped_features,
            },
        )
    );
    print_skipped_property_columns(&export_report.skipped_property_columns);
    if let Some(note) = encode_quantized_note(export_report.encode_quantized_features) {
        println!("  {note}");
    }
    // #380: the summary line above covers the requested (declared) range,
    // which can be wider than the archive's own PMTiles header (#529, #522:
    // the header always reflects the zooms that actually hold a tile) — say
    // which zooms in the requested range hold nothing rather than let the
    // summary imply they do.
    let empty_zooms: Vec<String> = convert_report
        .skipped_empty_levels
        .iter()
        .filter_map(|l| l.zoom)
        .map(|z| format!("z{z}"))
        .collect();
    if !empty_zooms.is_empty() {
        // Point at the collapse flags only when neither was passed; with one
        // on, the level was empty because there was less than one placeholder
        // of area in it, and the hint would name the flag already given.
        if args.tuning.collapse || args.tuning.collapse_square {
            println!(
                "  {} declared but empty: less than one placeholder of area there",
                empty_zooms.join(", ")
            );
        } else {
            println!(
                "  {} declared but empty: every feature generalized away there \
                 (see --collapse / --collapse-square), or an entry-zoom ladder \
                 holds every feature out of them",
                empty_zooms.join(", ")
            );
        }
    }

    if let Some(note) = row_group_autoscale_note(&convert_report) {
        println!("{note}");
    }

    // A combined report so the one-step run captures both halves the two-step
    // chain would write (`overview --report` + `export-pmtiles --report`).
    if let Some(report_path) = &args.report {
        let combined = serde_json::json!({
            "convert": convert_report,
            "export": export_report,
        });
        let json =
            serde_json::to_string_pretty(&combined).context("failed to serialize tiles report")?;
        std::fs::write(report_path, json)
            .with_context(|| format!("failed to write report to {}", report_path.display()))?;
        println!("  report written to {}", report_path.display());
    }

    Ok(())
}

/// One-line note for the #507 auto-scale, printed on both the `overview` and
/// `tiles` summaries. `None` when the requested `--row-group-size` was used
/// verbatim, so the common run prints nothing.
fn row_group_autoscale_note(
    report: &tylertoo_core::overview::convert::ConvertReport,
) -> Option<String> {
    report.effective_max_row_group_size.map(|cap| {
        format!(
            "  note: --row-group-size raised to {} so the output stays under parquet's \
             row-group ceiling (see the warning above); pass --row-group-size {} to make \
             it explicit",
            format_number(cap as u64),
            cap
        )
    })
}

/// The feature losses the `tiles` summary line reports (#429, #431, #553).
#[derive(Clone, Copy, Default)]
struct SummaryLosses<'a> {
    /// Features outside the declared CRS range.
    out_of_range: usize,
    /// The first few of those, by row and coordinate (#553).
    out_of_range_exemplars: &'a [tylertoo_core::overview::convert::OutOfRangeExemplar],
    /// Valid lon/lat outside the Web Mercator tiling domain.
    unprojectable: usize,
    /// Tile features dropped at MVT encode (#431).
    encode_dropped: usize,
}

/// The tile-count line of the `tiles` summary (#429).
///
/// Normally a bare count. When features were lost it says so on the same line
/// — a wrong-CRS input reporting a bare "✓ Converted … 0 tiles" was the
/// headline lie of issue #429. The two losses are named separately because
/// they have different fixes: coordinates outside the declared CRS's range
/// (usually a reprojection away), and valid lon/lat outside the Web Mercator
/// tiling domain (nothing to reproject — Mercator does not reach the poles).
/// A ≥99% loss never reaches here (the conversion fails outright), so this
/// covers the partial case and the "some other filter also emptied the
/// archive" one.
///
/// `out_of_range_exemplars` names the first few offending rows and
/// coordinates (#553): root-causing a real case (a5 grid cells with
/// vertices up to 0.6° past ±180°) used to require a separate DuckDB query
/// against the input, when the bare count gave no lead to follow.
fn tiles_summary_line(
    total_tiles: usize,
    min_zoom: u8,
    max_zoom: u8,
    secs: f64,
    losses: &SummaryLosses<'_>,
) -> String {
    let SummaryLosses {
        out_of_range,
        out_of_range_exemplars,
        unprojectable,
        encode_dropped,
    } = *losses;
    let zooms = format!("z{min_zoom}..z{max_zoom}");
    let tiles = format_number(total_tiles as u64);
    let mut losses: Vec<String> = Vec::new();
    if out_of_range > 0 {
        let exemplar =
            tylertoo_core::overview::convert::out_of_range_exemplar_note(out_of_range_exemplars);
        let exemplar = if exemplar.is_empty() {
            String::new()
        } else {
            format!(";{exemplar}")
        };
        losses.push(format!(
            "{} feature(s) dropped (outside the declared CRS range{exemplar})",
            format_number(out_of_range as u64)
        ));
    }
    if unprojectable > 0 {
        losses.push(format!(
            "{} feature(s) dropped (|lat| > 85.05°, outside the Web Mercator tiling domain)",
            format_number(unprojectable as u64)
        ));
    }
    if encode_dropped > 0 {
        // #431: post-clip losses at MVT encode; the core's aggregate warning
        // names the causes. Expected extent collapses are NOT a loss and go
        // through `encode_quantized_note` instead.
        losses.push(format!(
            "{} tile feature(s) dropped at MVT encode (empty geometry or empty \
             GeometryCollection)",
            format_number(encode_dropped as u64)
        ));
    }
    if losses.is_empty() {
        return format!("{tiles} tiles across {zooms} in {secs:.2}s");
    }
    let dropped = losses.join(", ");
    if total_tiles == 0 {
        format!("{tiles} tiles — {dropped} — {zooms} in {secs:.2}s")
    } else {
        format!("{tiles} tiles across {zooms} in {secs:.2}s — {dropped}")
    }
}

/// The informational note for members that collapsed at the tile extent
/// (#431), if any. Deliberately NOT part of the summary line: a clip sliver
/// that quantizes to zero area at a buffered tile edge is routine on any
/// polygon export and is not content loss.
fn encode_quantized_note(encode_quantized: usize) -> Option<String> {
    (encode_quantized > 0).then(|| {
        format!(
            "note: {} tile feature(s) collapsed at the tile extent and were not encoded \
             (zero-area polygon rings or lines of fewer than two points, typically clip \
             slivers at a buffered tile edge) \u{2014} expected, see \
             `encode_quantized_features` in the report",
            format_number(encode_quantized as u64)
        )
    })
}

/// The pyramid build's skipped-tile line, if any (#514 S3).
///
/// `report.skipped` counts every tile a band's archive held outside that
/// band's declared zoom range — which, since #495, is *also* the documented
/// way to split one pre-tiled archive across several `--band` entries (one
/// declaring z0-5, another z6-13, both pointing at the same z0-13 archive).
/// In that workflow the tiles a band leaves behind are picked up by its
/// sibling, so nothing vanished and the old unconditional `!` alarm read as
/// one regardless.
///
/// `archive_split` says whether the build actually is that workflow — see
/// [`bands_split_one_archive`]. This used to key off `skipped ==
/// total_tiles`, which is a count coincidence rather than evidence: a
/// single band allowed to overshoot by `--allow-missing-zooms` hits it as
/// soon as it happens to keep as many tiles as it drops (a z3-6 archive
/// under a z0-4 band keeps 2 and skips 2), and printed the reassuring
/// "expected when splitting" line for tiles that really were silently lost.
fn pyramid_skip_message(skipped: usize, archive_split: bool) -> Option<String> {
    if skipped == 0 {
        return None;
    }
    if archive_split {
        Some(format!(
            "  {} tile(s) fell outside a band's declared subrange and were skipped \
             (expected when splitting one pre-tiled archive across several bands)",
            format_number(skipped as u64)
        ))
    } else {
        // Almost always a --minzoom/--maxzoom that disagrees with --band, so
        // this belongs on stdout next to the counts, not only in the log.
        Some(format!(
            "  ! {} tile(s) dropped: outside the declared band zoom ranges",
            format_number(skipped as u64)
        ))
    }
}

/// Whether this build genuinely splits one pre-tiled archive across several
/// bands — two or more `--band` entries that are PMTiles archives and
/// resolve to the same file (#527 review, finding 2).
///
/// That, not a skipped-tile count, is what makes
/// [`pyramid_skip_message`]'s reassuring wording true: the tiles one band
/// drops are the ones its sibling keeps. Paths are compared through
/// [`canonical_key`], so `./a.pmtiles` and `a.pmtiles` are one archive.
/// Only `BandSource::Archive` bands count — a GeoParquet band is tiled to
/// its own declared range, so it never skips anything and listing one twice
/// is not a split.
fn bands_split_one_archive(bands: &[tylertoo_core::pyramid::Band]) -> bool {
    use std::collections::HashSet;
    use tylertoo_core::pyramid::{classify_band_input, BandSource};

    let mut seen: HashSet<PathBuf> = HashSet::new();
    bands
        .iter()
        .filter(|b| classify_band_input(&b.input) == BandSource::Archive)
        .any(|b| !seen.insert(canonical_key(&b.input)))
}

/// Run `tylertoo overview`: build a multi-resolution overview GeoParquet file.
fn run_overview(args: OverviewArgs) -> Result<()> {
    use tylertoo_core::overview::level::Mode;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (spec, output) = resolve_io(args.input, args.output, args.files_from)?;

    // #427: refuse an existing output (without --force) before any work. The
    // overview itself is written to a sibling and renamed over OUTPUT at the
    // end, so an interrupted run leaves a previous file intact either way.
    check_output_path(&output, OutputKind::GeoParquet, args.force)?;

    let mode = match args.mode.as_str() {
        "duplicating" => Mode::Duplicating,
        "partitioning" => Mode::Partitioning,
        other => anyhow::bail!("invalid --mode '{other}' (duplicating|partitioning)"),
    };

    // #444: the plan is built with `auto`'s placeholder so every cheap check
    // runs first; the estimate (which reads the input) comes last.
    let levels = resolve_level_plan(
        args.gsd.as_deref(),
        args.min_zoom,
        args.max_zoom.plan_zoom(),
    )?;

    let bbox = args.bbox.as_ref().map(|s| parse_bbox(s)).transpose()?;

    let mut options = args
        .tuning
        .build_convert_options(mode, levels, bbox, args.cogp_compat)?;
    resolve_max_zoom(args.max_zoom, &spec, &mut options)?;

    let report = run_convert(&spec, &output, &options)?;

    // Human-readable summary.
    println!();
    println!(
        "✓ Overview {} → {}  ({:?} mode)",
        spec.label(),
        output.file_name().unwrap_or_default().to_string_lossy(),
        report.mode
    );
    println!(
        "  {} input features → {} rows across {} levels in {:.2}s",
        format_number(report.input_features as u64),
        format_number(report.total_rows as u64),
        report.levels.len(),
        report.duration_secs
    );
    println!(
        "  {:>3}  {:>12}  {:>10}  {:>10}  {:>12}",
        "lvl", "gsd(m)", "features", "vertices", "bytes"
    );
    for lvl in &report.levels {
        println!(
            "  {:>3}  {:>12.2}  {:>10}  {:>10}  {:>12}",
            lvl.level,
            lvl.gsd,
            format_number(lvl.feature_count as u64),
            format_number(lvl.vertex_count as u64),
            HumanBytes(lvl.compressed_bytes.max(0) as u64)
        );
    }
    // #429: the aggregate `log::warn!` from the converter already names the
    // declared CRS, its range and the fix; the note here only makes sure the
    // summary itself never reads as an unqualified success.
    if report.out_of_range_features > 0 {
        println!(
            "  note: {} of {} input features reach beyond the coordinate range the \
             file's declared CRS allows and were dropped or clipped \u{2014} see the \
             warning above, and check the real CRS with `gpio inspect <input>`",
            format_number(report.out_of_range_features as u64),
            format_number(report.input_features as u64)
        );
    }
    if report.unprojectable_features > 0 {
        println!(
            "  note: {} of {} input features have valid lon/lat but lie outside the \
             Web Mercator tiling domain (|lat| > 85.05\u{b0}); these features cannot \
             be tiled",
            format_number(report.unprojectable_features as u64),
            format_number(report.input_features as u64)
        );
    }
    if !report.skipped_empty_levels.is_empty() {
        let planned: Vec<String> = report
            .skipped_empty_levels
            .iter()
            .map(|s| match s.zoom {
                Some(z) => format!("z{z}"),
                None => format!("level {}", s.planned_level),
            })
            .collect();
        println!(
            "  note: {} empty level(s) omitted ({}) — no features visible at those \
             scales; the pyramid starts at the coarsest non-empty level",
            report.skipped_empty_levels.len(),
            planned.join(", ")
        );
    }

    if let Some(note) = row_group_autoscale_note(&report) {
        println!("{note}");
    }

    if let Some(report_path) = &args.report {
        let json =
            serde_json::to_string_pretty(&report).context("failed to serialize overview report")?;
        std::fs::write(report_path, json)
            .with_context(|| format!("failed to write report to {}", report_path.display()))?;
        println!("  report written to {}", report_path.display());
    }

    Ok(())
}

/// Parse an `--entry-zoom` spec: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...` (#364).
///
/// Mirrors [`parse_class_rank`]'s grammar so the two read alike; the payload
/// differs (a zoom, not a priority) because a rung places a feature rather
/// than ordering it.
fn parse_entry_zoom(spec: &str) -> Result<tylertoo_core::overview::ladder::EntryZoomSpec> {
    use tylertoo_core::overview::ladder::{EntryZoomKind, EntryZoomSpec};

    let (column, rest) = spec.split_once(':').ok_or_else(|| {
        anyhow::anyhow!(
            "--entry-zoom {spec:?}: expected COLUMN:VALUE=ZOOM,VALUE=ZOOM,... \
             (e.g. \"level:0.5=8,0.2=12\")"
        )
    })?;
    if column.is_empty() {
        anyhow::bail!("--entry-zoom {spec:?}: empty column name");
    }
    let mut rungs = Vec::new();
    for pair in rest.split(',').filter(|p| !p.trim().is_empty()) {
        let (value, zoom) = pair.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("--entry-zoom {spec:?}: expected VALUE=ZOOM, got {pair:?}")
        })?;
        let value: f64 = value
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("--entry-zoom {spec:?}: {value:?} is not a number"))?;
        let zoom: u8 = zoom
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("--entry-zoom {spec:?}: {zoom:?} is not a zoom level"))?;
        rungs.push((value, zoom));
    }
    if rungs.is_empty() {
        anyhow::bail!("--entry-zoom {spec:?}: no VALUE=ZOOM pairs");
    }
    Ok(EntryZoomSpec {
        column: column.to_string(),
        kind: EntryZoomKind::Explicit(rungs),
    })
}

/// Parse a `--class-rank` spec: `COLUMN:VALUE=RANK,VALUE=RANK,...`.
///
/// `unknown_rank` (the priority for present-but-unlisted values) is derived as
/// `min(listed ranks) - 1.0`, so unknown classes always lose to every listed
/// value while still beating null/missing values (which lose to any rank).
///
/// Ranks must be finite (#428). `f64::from_str` happily accepts `nan` and
/// `inf`, and either one breaks this flag's contract: a NaN is not ordered
/// (the ranking comparator answers "does not beat" in both directions, so the
/// cell incumbent silently keeps the cell), and `min(ranks)` computed with
/// `f64::min` *ignores* NaN — one `nan` entry would leave `unknown_rank` at
/// `+inf - 1.0 = +inf`, making unlisted values outrank every named class,
/// the exact inverse of what this flag documents.
fn parse_class_rank(spec: &str) -> Result<tylertoo_core::overview::convert::ClassRanking> {
    use tylertoo_core::overview::convert::ClassRanking;

    let (column, rest) = spec.split_once(':').ok_or_else(|| {
        anyhow::anyhow!(
            "invalid --class-rank '{spec}': expected COLUMN:VALUE=RANK,... (missing ':')"
        )
    })?;
    let column = column.trim();
    if column.is_empty() {
        anyhow::bail!("invalid --class-rank '{spec}': empty column name");
    }

    let mut ranks: Vec<(String, f64)> = Vec::new();
    for pair in rest.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (value, rank) = pair.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("invalid --class-rank entry '{pair}': expected VALUE=RANK")
        })?;
        let value = value.trim();
        if value.is_empty() {
            anyhow::bail!("invalid --class-rank entry '{pair}': empty value");
        }
        let rank: f64 = rank
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid rank in '{pair}': {e}"))?;
        if !rank.is_finite() {
            anyhow::bail!(
                "invalid rank in '{pair}': ranks must be finite numbers \
                 (NaN and infinity cannot be ordered against other classes)"
            );
        }
        ranks.push((value.to_string(), rank));
    }
    if ranks.is_empty() {
        anyhow::bail!("invalid --class-rank '{spec}': no VALUE=RANK entries");
    }

    // Unknown values must lose to every named class but beat nulls.
    let min_rank = ranks.iter().map(|(_, r)| *r).fold(f64::INFINITY, f64::min);
    Ok(ClassRanking {
        column: column.to_string(),
        ranks,
        unknown_rank: min_rank - 1.0,
    })
}

/// Parse an `--accumulate-attribute` spec: `COL:OP` with OP one of
/// `sum`, `max`, `min`, `mean` (case-insensitive).
fn parse_accumulate(spec: &str) -> Result<tylertoo_core::overview::cluster::AccumulateSpec> {
    use tylertoo_core::overview::cluster::{AccumulateOp, AccumulateSpec};

    let (column, op) = spec.rsplit_once(':').ok_or_else(|| {
        anyhow::anyhow!("invalid --accumulate-attribute '{spec}': expected COL:OP (missing ':')")
    })?;
    let column = column.trim();
    if column.is_empty() {
        anyhow::bail!("invalid --accumulate-attribute '{spec}': empty column name");
    }
    let op = AccumulateOp::parse(op.trim()).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid --accumulate-attribute '{spec}': unknown op {:?} \
             (expected sum, max, min, or mean)",
            op.trim()
        )
    })?;
    Ok(AccumulateSpec {
        column: column.to_string(),
        op,
    })
}

/// Run `tylertoo validate`: check a GeoParquet overview file (spec §6.2).
fn run_validate(args: ValidateArgs) -> Result<()> {
    use tylertoo_core::overview::check::validate_file;

    reject_files_from(args.files_from.as_ref(), "validate")?;
    require_single_local_file(&args.file, "validate")?;

    let report = validate_file(&args.file)
        .map_err(|e| anyhow::anyhow!("could not open '{}': {e}", args.file.display()))?;

    println!("Validating {}", args.file.display());
    for check in &report.checks {
        let mark = if check.passed { "PASS" } else { "FAIL" };
        println!("  [{mark}] {}: {}", check.name, check.message);
    }

    if report.is_valid() {
        println!(
            "\n✓ valid overview file ({} checks passed)",
            report.checks.len()
        );
        Ok(())
    } else {
        let failed = report.failures().count();
        anyhow::bail!("{failed} check(s) failed");
    }
}

/// `validate`, `decode`, and `export-pmtiles` read exactly ONE local file.
/// Multi-partition inputs (directories, globs, `s3://`/`gs://` prefixes,
/// `--files-from` manifests) and remote URLs are converter features
/// (`overview`, `tiles`); reject them here with a one-line pointer instead
/// of an obscure I/O or parquet error.
fn require_single_local_file(input: &std::path::Path, subcommand: &str) -> Result<()> {
    let s = input.to_string_lossy();
    if s.contains("://") {
        // Query string / fragment stripped: `s3://b/set/?x` is a prefix.
        let no_meta = s.split(['?', '#']).next().unwrap_or(&s);
        if no_meta.ends_with('/') {
            anyhow::bail!(
                "`tylertoo {subcommand}` reads a single local file, but {} is a \
                 remote prefix; multi-partition input (directories, globs, \
                 s3://gs:// prefixes, --files-from) is supported by the `overview` \
                 and `tiles` subcommands",
                input.display()
            );
        }
        anyhow::bail!(
            "`tylertoo {subcommand}` does not support remote inputs (got {}); \
             remote URLs (s3://, https://, gs://) are supported by the `overview` \
             and `tiles` subcommands — download the file first (e.g. `aws s3 cp`)",
            input.display()
        );
    }
    let kind = if input.is_dir() {
        "a directory"
    } else if s.contains(['*', '?', '[']) {
        "a glob pattern"
    } else {
        return Ok(());
    };
    anyhow::bail!(
        "`tylertoo {subcommand}` reads a single local file, but {} is {kind}; \
         multi-partition input (directories, globs, s3://gs:// prefixes, \
         --files-from) is supported by the `overview` and `tiles` subcommands",
        input.display()
    );
}

/// Reject `--files-from` on single-file subcommands with the same pointer
/// (clap alone would say only "unexpected argument '--files-from'").
fn reject_files_from(files_from: Option<&PathBuf>, subcommand: &str) -> Result<()> {
    if files_from.is_some() {
        anyhow::bail!(
            "`tylertoo {subcommand}` reads a single local file and does not \
             support --files-from; multi-partition input is supported by the \
             `overview` and `tiles` subcommands"
        );
    }
    Ok(())
}

fn run_export_pmtiles(args: ExportPmtilesArgs) -> Result<()> {
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // #427: refuse an existing output (without --force) and a bad --spill-dir
    // before the input is even looked at.
    check_output_path(&args.output, OutputKind::Pmtiles, args.force)?;
    check_spill_dir(args.spill_dir.as_deref())?;
    reject_files_from(args.files_from.as_ref(), "export-pmtiles")?;
    require_single_local_file(&args.input, "export-pmtiles")?;

    let opts = ExportOptions {
        layer_name: args.layer_name,
        tile_buffer: args.tile_buffer,
        extent: 4096,
        tile_size_limit: size_limit_opt(args.tile_size_limit),
        simple_clip_fastpath: !args.no_simple_clip_fastpath,
        partition_wave: args.partition_wave,
        feature_order: args.feature_order.clone(),
        min_zoom: args.min_zoom,

        properties: tylertoo_core::overview::properties::PropertySelection {
            include: (!args.include_property.is_empty()).then(|| args.include_property.clone()),
            exclude: args.exclude_property.clone(),
            exclude_all: args.exclude_all_properties,
        },
        tile_range: args
            .tile_range
            .as_deref()
            .map(tylertoo_core::shard::TileRange::parse)
            .transpose()?,
        zoom_ceiling: args.zoom_ceiling,
        feature_id: args.feature_id.clone(),
        spill_dir: args.spill_dir.clone(),
    };

    println!(
        "Exporting {} → {}",
        args.input.display(),
        args.output.display()
    );
    let report = export_pmtiles(&args.input, &args.output, &opts)
        .map_err(|e| anyhow::anyhow!("export failed: {e}"))?;

    println!(
        "  mode={} zooms z{}..z{}",
        report.mode, report.min_zoom, report.max_zoom
    );
    for z in &report.zooms {
        println!(
            "  z{:<2} (level {}): {:>7} tiles, {:>9} features{}{}{}",
            z.zoom,
            z.level,
            z.tile_count,
            z.tile_feature_count,
            if z.oversized_tiles > 0 {
                format!(", {} oversized", z.oversized_tiles)
            } else {
                String::new()
            },
            if z.encode_dropped_features > 0 {
                format!(", {} unencodable", z.encode_dropped_features)
            } else {
                String::new()
            },
            if z.encode_quantized_features > 0 {
                format!(", {} collapsed at extent", z.encode_quantized_features)
            } else {
                String::new()
            }
        );
    }
    println!(
        "\n✓ {} tiles, {} features, {} oversized tiles in {:.2}s{}",
        report.total_tiles,
        report.total_tile_features,
        report.oversized_tiles,
        report.duration_secs,
        if report.encode_dropped_features > 0 {
            // #431: never let the summary read as an unqualified success.
            format!(
                " \u{2014} {} tile feature(s) dropped at MVT encode (empty geometry or \
                 empty GeometryCollection); see the warning above",
                report.encode_dropped_features
            )
        } else {
            String::new()
        }
    );
    print_skipped_property_columns(&report.skipped_property_columns);
    if let Some(note) = encode_quantized_note(report.encode_quantized_features) {
        println!("  {note}");
    }

    if let Some(path) = &args.report {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| anyhow::anyhow!("serialize report: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| anyhow::anyhow!("write report {}: {e}", path.display()))?;
        println!("  report → {}", path.display());
    }
    Ok(())
}

/// #434: the export already warned once per column through the log; the
/// summary repeats the loss in one line so it is not missed when the log is
/// quiet or scrolled away. Nothing is printed when nothing was dropped.
fn print_skipped_property_columns(
    skipped: &[tylertoo_core::overview::export::SkippedPropertyColumn],
) {
    if skipped.is_empty() {
        return;
    }
    println!(
        "  {} property column{} not exported (no MVT encoding for the type): {}",
        skipped.len(),
        if skipped.len() == 1 { "" } else { "s" },
        skipped
            .iter()
            .map(|c| format!("{:?} ({})", c.name, c.data_type))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// If `spec` is the `LO-HI=INPUT=LAYER` escape form and `layer` is exactly
/// the segment `Band::parse` peeled off its end, the `=LAYER` suffix that was
/// stripped — for callers that want to hint "was this actually part of the
/// path?" when the stripped-down input then fails an existence check.
///
/// Mirrors `Band::parse`'s own separator choice (`=` wins whichever of `:`
/// and `=` appears first) without reaching into its private helpers: this is
/// a best-effort hint, not a re-parse, so a false negative here just means no
/// hint is offered, not a wrong answer.
fn stripped_equals_layer_suffix(spec: &str, layer: &str) -> Option<String> {
    let colon = spec.find(':');
    let equals = spec.find('=');
    let is_equals_form = match (colon, equals) {
        (Some(c), Some(e)) => e < c,
        (None, Some(_)) => true,
        _ => false,
    };
    if !is_equals_form {
        return None;
    }
    let suffix = format!("={layer}");
    spec.ends_with(&suffix).then_some(suffix)
}

/// Run `tylertoo pyramid`: merge per-band PMTiles archives into one (thin
/// facade over `tylertoo_core::pyramid::merge_bands`).
fn run_pyramid(args: PyramidArgs) -> Result<()> {
    use tylertoo_core::pyramid::{build_pyramid, validate_bands, Band};

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if args.output.exists() && !args.force {
        anyhow::bail!(
            "{} exists (use --force to overwrite)",
            args.output.display()
        );
    }

    let bands = args
        .bands
        .iter()
        .map(|s| Band::parse(s).map_err(|e| anyhow::anyhow!(e)))
        .collect::<Result<Vec<_>>>()?;
    validate_bands(&bands).map_err(|e| anyhow::anyhow!(e))?;

    // Report a missing band input here, naming it, rather than letting the
    // reader fail later with a bare "No such file or directory" and no path.
    //
    // This cannot be a `classify_band_input` check: that returns `Archive` only
    // when the file opens, so a missing path always classifies as `Source` and
    // the kind tells us nothing about whether it exists. What it can be is a
    // plain existence test, skipped for the three spellings where `exists()`
    // has no useful answer — a remote URL, a glob, and anything else the
    // reader resolves itself.
    for (spec, b) in args.bands.iter().zip(&bands) {
        let spelled = b.input.to_string_lossy();
        let deferred = spelled.contains("://") || spelled.contains(['*', '?', '[']);
        if !deferred && !b.input.exists() {
            let mut msg = format!("band input not found: {}", b.input.display());
            // A bare Hive partition dir spec like
            // `0-13=admin:country_code=BR` (no trailing filename) silently
            // parses as INPUT=admin:country_code, LAYER=BR: the trailing
            // `=BR` looked like an explicit layer, so it was stripped from
            // the path. If that is what happened here, say so — a bare
            // "not found" gives no hint that a layer was ever peeled off.
            if let Some(suffix) = stripped_equals_layer_suffix(spec, &b.layer) {
                let candidate = format!("{}{suffix}", b.input.display());
                if std::path::Path::new(&candidate).exists() {
                    msg.push_str(&format!(
                        "\n  hint: {candidate:?} exists — the trailing {suffix:?} was \
                         parsed as a layer name (LO-HI=INPUT=LAYER); if it is part of \
                         the path, append an explicit =LAYER instead"
                    ));
                } else {
                    msg.push_str(&format!(
                        "\n  hint: the trailing {suffix:?} was parsed as a layer name \
                         (LO-HI=INPUT=LAYER); if it is part of the path, append an \
                         explicit =LAYER instead"
                    ));
                }
            }
            anyhow::bail!(msg);
        }
    }

    let opts = pyramid_options(&args);

    let report = build_pyramid(&bands, &args.output, &opts)
        .map_err(|e| anyhow::anyhow!("pyramid build failed: {e}"))?;

    for (layer, lo, hi, n) in &report.per_band_tiles {
        println!(
            "  z{lo}-{hi} -> layer {layer:?}: {} tiles",
            format_number(*n as u64)
        );
    }
    // Only consulted when something was skipped: `classify_band_input`
    // opens each band's first bytes, which is pointless when there is no
    // line to print.
    let split = report.skipped > 0 && bands_split_one_archive(&bands);
    if let Some(line) = pyramid_skip_message(report.skipped, split) {
        println!("{line}");
    }
    println!(
        "✓ Built {} band(s) → {} ({} tiles)",
        bands.len(),
        args.output.display(),
        format_number(report.total_tiles as u64)
    );
    Ok(())
}

/// The `pyramid` flags that apply to every GeoParquet band alike, as the
/// library options `build_pyramid` substitutes each band's own layer name
/// and zoom range into. Kept separate from `run_pyramid` so a test can
/// check a flag actually reaches `PyramidOptions` (#374: `--feature-order`
/// was documented for `tiles` and `export-pmtiles` but `pyramid` built its
/// export options from `ExportOptions::default()`).
fn pyramid_options(args: &PyramidArgs) -> tylertoo_core::pyramid::PyramidOptions {
    use tylertoo_core::overview::convert::ConvertOptions;
    use tylertoo_core::overview::export::ExportOptions;
    use tylertoo_core::pyramid::PyramidOptions;

    let convert = if args.generalize {
        ConvertOptions::default()
    } else {
        ConvertOptions::default().verbatim()
    };
    PyramidOptions {
        convert,
        export: ExportOptions {
            tile_size_limit: args.max_tile_size.and_then(size_limit_opt),
            feature_order: args.feature_order.clone(),
            ..ExportOptions::default()
        },
        work_dir: args.work_dir.clone(),
        allow_missing_zooms: args.allow_missing_zooms,
    }
}

/// A path's identity for "is this the same file?", resolved as far as the
/// filesystem allows.
///
/// The file itself need not exist (the output usually does not), so the
/// *parent* is canonicalized and the file name re-joined — that collapses
/// `./out.pmtiles`, `dir/../out.pmtiles` and a symlinked directory to one
/// key. When even the parent cannot be resolved the literal path is the key,
/// which is what the comparison did before and is never worse.
fn canonical_key(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => {
            let parent = if parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            match parent.canonicalize() {
                Ok(real) => real.join(name),
                Err(_) => path.to_path_buf(),
            }
        }
        _ => path.to_path_buf(),
    }
}

/// Fail now if `path` cannot be created, instead of after the work is done.
///
/// Probes by creating (and removing) the file when it does not exist, so a
/// missing directory or a read-only one is reported with the path that caused
/// it. An existing file is left strictly alone — `--force` decides whether it
/// may be overwritten, and this must not truncate it.
fn preflight_writable(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.is_dir() {
            anyhow::bail!(
                "{}: directory {} does not exist",
                path.display(),
                parent.display()
            );
        }
    }
    match std::fs::File::create(path) {
        Ok(_) => {
            let _ = std::fs::remove_file(path);
            Ok(())
        }
        Err(e) => anyhow::bail!("{}: cannot be written ({e})", path.display()),
    }
}

/// Run `tylertoo merge`: several disjoint PMTiles archives → one (thin facade
/// over `tylertoo_core::merge::merge_shards`).
/// `tylertoo shard-plan` — step 0 of a sharded build (#498).
fn run_shard_plan(args: ShardPlanArgs) -> Result<()> {
    use tylertoo_core::shard::ShardPlan;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // No `exists()` precheck: a shard plan is cut for whatever a fleet will
    // read, which may be a directory, a glob, an s3:// prefix or a manifest.
    // `ConvertSource::resolve` names a bad input far better than a bare
    // stat() ever could, and is the same resolution `tiles` performs.
    let spec = resolve_io_for_planning(args.input, args.files_from)?;
    anyhow::ensure!(
        args.force || !args.output.exists(),
        "{} already exists; pass -f/--force to overwrite it. Every job of a fleet must be given \
         the SAME shard plan, so replacing one mid-build silently re-cuts the tile space.",
        args.output.display()
    );
    preflight_writable(&args.output)?;

    let source = resolve_convert_source(&spec)?;
    let plan = ShardPlan::compute(&source, args.pivot, args.shards)?;
    plan.save(&args.output)?;

    let input_label = spec.display();
    println!(
        "Cut {} shard(s) at pivot z{} for {input_label}",
        plan.shards(),
        plan.pivot_zoom,
    );
    let total = plan.estimated_rows_total.max(1);
    for (i, r) in plan.ranges.iter().enumerate() {
        println!(
            "  shard {i:<3} tiles {}..={}  ({:>5} pivot tile(s), ~{} row(s), {:.1}%)",
            r.lo,
            r.hi,
            r.hi - r.lo + 1,
            r.estimated_rows,
            100.0 * r.estimated_rows as f64 / total as f64,
        );
    }
    if plan.unplaced_rows > 0 {
        println!(
            "\n  ! ~{} row(s) could not be placed: their row groups carry no usable bbox \
             statistics (or cover so much of the pivot zoom that they name no cut point), so \
             they were spread evenly instead of balanced. Run the input through `gpio \
             sort hilbert --add-bbox` to give every row group a tight covering, and the cut \
             improves for free.",
            plan.unplaced_rows
        );
    }
    // A range with no estimated rows is legal — the cut has to tile the pivot
    // zoom with no gap, so a dataset concentrated in one corner leaves the
    // rest of the fleet empty — but it is also almost always a sign the fleet
    // is too large for the data. Those jobs still run (and now exit 0 with an
    // empty archive), so the only place to notice is here, at cut time.
    let empty = plan.ranges.iter().filter(|r| r.estimated_rows == 0).count();
    if empty > 0 {
        println!(
            "\n  ! {empty} of {} shard(s) are empty of data: their ranges own pivot tiles the \
             estimator places no rows in. Those jobs will run, find nothing and write a valid \
             empty archive (the merge skips it), so nothing breaks — but a smaller --shards, \
             or a finer --pivot on a concentrated dataset, would balance the fleet better.",
            plan.shards(),
        );
    }
    println!("\n✓ wrote {}", args.output.display());
    println!(
        "\nNext:\n  \
         tylertoo tiles {input_label} coarse.pmtiles --shard coarse --shard-plan {} --save-plan convert.plan\n  \
         tylertoo tiles {input_label} shard-$i.pmtiles --shard $i/{} --shard-plan {} --plan convert.plan\n  \
         tylertoo merge out.pmtiles coarse.pmtiles shard-*.pmtiles",
        args.output.display(),
        plan.shards(),
        args.output.display(),
    );
    Ok(())
}

fn run_merge(args: MergeArgs) -> Result<()> {
    use tylertoo_core::merge::{merge_shards, MergeOptions};

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    if args.output.exists() && !args.force {
        anyhow::bail!(
            "{} exists (use --force to overwrite)",
            args.output.display()
        );
    }

    // Name a missing input here rather than letting the reader fail later
    // with a bare "No such file or directory" and no path. Every input is a
    // local archive read by offset, so `exists()` has a useful answer for all
    // of them — unlike `pyramid`'s bands, which may be globs or URLs.
    //
    // The input/output comparison is on canonical paths: `merge out.pmtiles
    // ./out.pmtiles shard.pmtiles` walked straight past a `PathBuf` equality
    // test. All the reads complete before `finalize` creates the output, so
    // the merge itself would have succeeded — and silently destroyed the
    // input it had just consumed, which is worse than a failed merge.
    let out_key = canonical_key(&args.output);
    for input in &args.inputs {
        if !input.exists() {
            anyhow::bail!("input archive not found: {}", input.display());
        }
        if canonical_key(input) == out_key {
            anyhow::bail!(
                "{} is both an input and the output; the merge would read it and then \
                 overwrite it, destroying the input",
                input.display()
            );
        }
    }

    // Preflight the paths the run will write, before it reads gigabytes: a
    // `--report` in a directory that does not exist, or an output whose parent
    // is missing or read-only, would otherwise surface only at the end. This
    // mirrors `--work-dir`, which fails fast because the writer creates its
    // spool file up front.
    preflight_writable(&args.output).map_err(|e| anyhow::anyhow!("output {e}"))?;
    if let Some(report) = &args.report {
        preflight_writable(report).map_err(|e| anyhow::anyhow!("--report {e}"))?;
    }
    if args.inputs.len() < 2 {
        // Not an error: merging one archive is a well-defined (if pointless)
        // copy, and a script that shards dynamically can legitimately end up
        // with one shard. Worth saying out loud, though.
        eprintln!("note: merging a single archive just rewrites it");
    }

    println!(
        "Merging {} archive(s) → {}",
        args.inputs.len(),
        args.output.display()
    );
    let opts = MergeOptions {
        work_dir: args.work_dir.clone(),
    };
    let report = merge_shards(&args.inputs, &args.output, &opts)
        .map_err(|e| anyhow::anyhow!("merge failed: {e}"))?;

    for (zoom, count) in &report.per_zoom_tile_counts {
        println!("  z{:<2} {:>10} tiles", zoom, format_number(*count));
    }
    if !report.inputs_without_bounds.is_empty() {
        // Almost always a shard exported without bounds, which quietly
        // shrinks the merged archive's extent, so it belongs on stdout next
        // to the counts and not only in the log.
        println!(
            "  ! {} input(s) carried no usable bounds and were excluded from the merged bounds:",
            report.inputs_without_bounds.len()
        );
        for name in &report.inputs_without_bounds {
            println!("      {name}");
        }
    }
    println!(
        "\n✓ {} tiles ({} unique) from {} archive(s), z{}..z{} in {:.2}s",
        format_number(report.tiles_total),
        format_number(report.unique_tiles),
        report.inputs,
        report.min_zoom,
        report.max_zoom,
        report.duration_secs
    );

    if let Some(path) = &args.report {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| anyhow::anyhow!("serialize report: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| anyhow::anyhow!("write report {}: {e}", path.display()))?;
        println!("  report → {}", path.display());
    }
    Ok(())
}

/// Run `tylertoo decode`: PMTiles → GeoParquet (thin facade over
/// `tylertoo_core::decode::decode_pmtiles`).
fn run_decode(args: DecodeArgs) -> Result<()> {
    use tylertoo_core::decode::{decode_pmtiles, DecodeOptions};

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // #427: refuse an existing output (without --force) before any work.
    check_output_path(&args.output, OutputKind::GeoParquet, args.force)?;
    reject_files_from(args.files_from.as_ref(), "decode")?;
    require_single_local_file(&args.input, "decode")?;

    // `--zoom N` is shorthand for `--min-zoom N --max-zoom N` (clap already
    // rejects combining them).
    let (min_zoom, max_zoom) = match args.zoom {
        Some(z) => (Some(z), Some(z)),
        None => (args.min_zoom, args.max_zoom),
    };
    if let (Some(lo), Some(hi)) = (min_zoom, max_zoom) {
        if lo > hi {
            anyhow::bail!("--min-zoom {lo} exceeds --max-zoom {hi}");
        }
    }
    let options = DecodeOptions {
        min_zoom,
        max_zoom,
        layer: args.layer,
    };

    println!(
        "Decoding {} → {}",
        args.input.display(),
        args.output.display()
    );
    let report = decode_pmtiles(&args.input, &args.output, &options)
        .with_context(|| format!("decode failed for {}", args.input.display()))?;

    match report.zoom_range {
        Some((lo, hi)) => println!("  zooms z{lo}..z{hi}, layers: {}", report.layers.join(", ")),
        None => println!("  no features matched the filters"),
    }
    println!(
        "\n✓ {} features from {} tiles ({} skipped as degenerate) in {:.2}s",
        format_number(report.features_written),
        format_number(report.tiles_read),
        report.features_skipped,
        report.elapsed_secs
    );
    println!(
        "  note: output is the tiled representation (simplified, clipped, \
         duplicated across zooms); see `tylertoo decode --help`"
    );

    if let Some(path) = &args.report {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| anyhow::anyhow!("serialize report: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| anyhow::anyhow!("write report {}: {e}", path.display()))?;
        println!("  report → {}", path.display());
    }
    Ok(())
}

/// Run `tylertoo stats`: per-zoom tile-weight report over a PMTiles archive
/// (issue #552). Thin facade over `tylertoo_core::stats::compute_stats`,
/// which does all the work off `ArchiveIndex` (header + directories only).
fn run_stats(args: StatsArgs) -> Result<()> {
    use tylertoo_core::archive_index::ArchiveIndex;
    use tylertoo_core::stats::compute_stats;

    let archive = ArchiveIndex::open(&args.archive).map_err(|e| {
        // `ArchiveIndex` reports read failures under the shared PMTiles
        // error variant ("Failed to write PMTiles: ..."); for a file that
        // is not an archive at all, say so plainly.
        let not_pmtiles = std::fs::File::open(&args.archive)
            .and_then(|mut f| {
                let mut magic = [0u8; 7];
                std::io::Read::read_exact(&mut f, &mut magic).map(|()| magic != *b"PMTiles")
            })
            .unwrap_or(args.archive.is_file());
        if not_pmtiles {
            anyhow::anyhow!(
                "{} is not a PMTiles v3 archive (missing the PMTiles magic): {e}",
                args.archive.display()
            )
        } else {
            anyhow::anyhow!("could not open {}: {e}", args.archive.display())
        }
    })?;
    let report = compute_stats(&archive, args.largest)
        .map_err(|e| anyhow::anyhow!("stats failed for {}: {e}", args.archive.display()))?;

    if args.json {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| anyhow::anyhow!("serialize stats report: {e}"))?;
        println!("{json}");
        return Ok(());
    }

    print_stats_table(&report);
    Ok(())
}

/// Right-align every column of a table to its widest cell (header included),
/// two spaces between columns.
fn render_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    let render_row = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{c:>width$}", width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
    };
    let header_cells: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
    let mut lines = vec![render_row(&header_cells)];
    lines.extend(rows.iter().map(|row| render_row(row)));
    lines.join("\n")
}

/// The human-readable form of a [`tylertoo_core::stats::StatsReport`]: the
/// per-zoom table the issue asked for, followed by the largest tiles (when
/// any were requested).
fn print_stats_table(report: &tylertoo_core::stats::StatsReport) {
    if report.per_zoom.is_empty() {
        println!("(archive holds no tiles)");
        return;
    }

    let rows: Vec<Vec<String>> = report
        .per_zoom
        .iter()
        .map(|z| {
            vec![
                z.z.to_string(),
                format_number(z.tile_count),
                format_number(z.total_bytes),
                format_number(z.mean_bytes),
                format_number(z.p50_bytes),
                format_number(z.p99_bytes),
                format_number(z.max_bytes),
            ]
        })
        .collect();
    println!(
        "{}",
        render_table(&["z", "tiles", "total", "mean", "p50", "p99", "max"], &rows)
    );

    if !report.largest.is_empty() {
        println!("\nLargest {} tile(s):", report.largest.len());
        let largest_rows: Vec<Vec<String>> = report
            .largest
            .iter()
            .map(|t| {
                vec![
                    t.z.to_string(),
                    t.x.to_string(),
                    t.y.to_string(),
                    format_number(t.bytes),
                ]
            })
            .collect();
        println!("{}", render_table(&["z", "x", "y", "bytes"], &largest_rows));
    }
}

/// Format a number with thousands separators
fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tylertoo_core::overview::cluster::AccumulateOp;

    /// #428: `f64::from_str` accepts `nan`, `inf` and `-inf`, and either one
    /// breaks `--class-rank`'s contract. A NaN is not ordered, so the cell
    /// incumbent would silently keep every cell it contests; worse, the
    /// `unknown_rank` derivation uses `f64::min`, which IGNORES NaN — one
    /// `nan` entry leaves the fold at `+inf`, so unlisted values would
    /// outrank every named class, the inverse of the documented rule. The
    /// parser rejects them with an explanation rather than producing that.
    #[test]
    fn class_rank_rejects_non_finite_ranks() {
        for spec in [
            "cls:motorway=nan,trunk=2",
            "cls:motorway=NaN",
            "cls:motorway=inf,trunk=2",
            "cls:motorway=-inf",
            "cls:motorway=infinity",
        ] {
            let err = parse_class_rank(spec)
                .unwrap_err()
                .to_string()
                .to_ascii_lowercase();
            assert!(
                err.contains("finite"),
                "'{spec}' must be rejected as non-finite, got: {err}"
            );
        }

        // Control: ordinary ranks still parse, and the unknown rank is
        // min(ranks) - 1 — below every named class, above a null.
        let cr = parse_class_rank("cls:motorway=3,trunk=2,primary=1").unwrap();
        assert_eq!(cr.column, "cls");
        assert_eq!(cr.unknown_rank, 0.0);
        assert_eq!(cr.ranks.len(), 3);
    }

    // --- single-file-only rejections (v0.7 PR-C) -----------------------------

    /// `validate`/`decode`/`export-pmtiles` are single-file only: a
    /// directory, glob, or remote-prefix input must fail with a one-line
    /// error naming the subcommand and pointing at `overview`/`tiles`,
    /// not an obscure I/O or parquet error.
    #[test]
    fn single_file_subcommands_reject_multi_inputs() {
        let dir = tempfile::tempdir().unwrap();

        // Directory input.
        let err = require_single_local_file(dir.path(), "validate")
            .unwrap_err()
            .to_string();
        assert!(err.contains("validate"), "names the subcommand: {err}");
        assert!(err.contains("single"), "says single-file only: {err}");
        assert!(
            err.contains("overview") && err.contains("tiles"),
            "points at the multi-partition subcommands: {err}"
        );

        // Remote prefix (trailing slash).
        let err =
            require_single_local_file(std::path::Path::new("s3://bucket/set/"), "export-pmtiles")
                .unwrap_err()
                .to_string();
        assert!(err.contains("export-pmtiles"), "{err}");
        assert!(err.contains("overview") && err.contains("tiles"), "{err}");

        // Glob pattern.
        let err = require_single_local_file(std::path::Path::new("/data/*.parquet"), "decode")
            .unwrap_err()
            .to_string();
        assert!(err.contains("decode"), "{err}");
        assert!(err.contains("overview") && err.contains("tiles"), "{err}");

        // A plain (single) local file passes.
        let f = dir.path().join("x.parquet");
        std::fs::write(&f, b"x").unwrap();
        assert!(require_single_local_file(&f, "validate").is_ok());
        // Nonexistent single file also passes (open reports the io error).
        assert!(require_single_local_file(
            std::path::Path::new("/no/such/file.parquet"),
            "validate"
        )
        .is_ok());

        // A single remote object keeps the historical download-first pointer.
        let err =
            require_single_local_file(std::path::Path::new("s3://bucket/file.parquet"), "validate")
                .unwrap_err()
                .to_string();
        assert!(err.contains("remote"), "{err}");
        assert!(err.contains("aws s3 cp"), "{err}");
    }

    #[test]
    fn parse_accumulate_valid_specs() {
        let s = parse_accumulate("population:sum").unwrap();
        assert_eq!(s.column, "population");
        assert_eq!(s.op, AccumulateOp::Sum);

        // Case-insensitive op, trimmed parts.
        let s = parse_accumulate(" confidence : MEAN ").unwrap();
        assert_eq!(s.column, "confidence");
        assert_eq!(s.op, AccumulateOp::Mean);

        let s = parse_accumulate("x:min").unwrap();
        assert_eq!(s.op, AccumulateOp::Min);
        let s = parse_accumulate("x:max").unwrap();
        assert_eq!(s.op, AccumulateOp::Max);
    }

    #[test]
    fn parse_accumulate_rejects_bad_specs() {
        assert!(parse_accumulate("population").is_err(), "missing op");
        assert!(parse_accumulate(":sum").is_err(), "empty column");
        assert!(parse_accumulate("pop:median").is_err(), "unknown op");
        assert!(parse_accumulate("pop:").is_err(), "empty op");
    }

    // --- --files-from (v0.7 multi-partition) ---------------------------------

    /// `--files-from` parses on BOTH subcommands; the single positional is
    /// the OUTPUT (clap slots it into the INPUT position; `resolve_io` swaps).
    #[test]
    fn files_from_parses_on_both_subcommands() {
        let cli =
            Cli::try_parse_from(["tylertoo", "tiles", "--files-from", "m.txt", "out.pmtiles"])
                .expect("tiles --files-from should parse");
        let Command::Tiles(a) = cli.command else {
            panic!("expected tiles");
        };
        let (spec, out) = resolve_io(a.input, a.output, a.files_from).unwrap();
        assert!(matches!(spec, InputSpec::Manifest(ref m) if m == &PathBuf::from("m.txt")));
        assert_eq!(out, PathBuf::from("out.pmtiles"));

        let cli = Cli::try_parse_from([
            "tylertoo",
            "overview",
            "--files-from",
            "m.txt",
            "out.parquet",
        ])
        .expect("overview --files-from should parse");
        let Command::Overview(a) = cli.command else {
            panic!("expected overview");
        };
        let (spec, out) = resolve_io(a.input, a.output, a.files_from).unwrap();
        assert!(matches!(spec, InputSpec::Manifest(_)));
        assert_eq!(out, PathBuf::from("out.parquet"));
    }

    /// `--files-from` conflicts with a positional INPUT; INPUT stays
    /// required without it; OUTPUT is still required with it.
    #[test]
    fn files_from_conflicts_with_positional_input() {
        let cli = Cli::try_parse_from([
            "tylertoo",
            "overview",
            "--files-from",
            "m.txt",
            "in.parquet",
            "out.parquet",
        ])
        .expect("clap accepts; resolve_io rejects");
        let Command::Overview(a) = cli.command else {
            panic!("expected overview");
        };
        let err = resolve_io(a.input, a.output, a.files_from).unwrap_err();
        assert!(
            err.to_string().contains("conflicts"),
            "conflict named: {err}"
        );

        // Without --files-from, INPUT/OUTPUT stay clap-required.
        assert!(Cli::try_parse_from(["tylertoo", "overview"]).is_err());
        assert!(Cli::try_parse_from(["tylertoo", "tiles", "only-one"]).is_err());

        // With --files-from but no positional at all: OUTPUT is missing.
        let cli = Cli::try_parse_from(["tylertoo", "tiles", "--files-from", "m.txt"])
            .expect("parses; resolve_io names the missing OUTPUT");
        let Command::Tiles(a) = cli.command else {
            panic!("expected tiles");
        };
        let err = resolve_io(a.input, a.output, a.files_from).unwrap_err();
        assert!(err.to_string().contains("OUTPUT"), "names OUTPUT: {err}");
    }

    /// Parse a `tiles` invocation and return its args (INPUT/OUTPUT are dummies).
    fn parse_tiles(flags: &[&str]) -> TilesArgs {
        let mut argv = vec!["tylertoo", "tiles", "in.parquet", "out.pmtiles"];
        argv.extend_from_slice(flags);
        match Cli::try_parse_from(argv)
            .expect("tiles args should parse")
            .command
        {
            Command::Tiles(a) => *a,
            other => panic!("expected tiles subcommand, got {other:?}"),
        }
    }

    /// Parse a `tiles --plan-only` invocation (no OUTPUT, as the run takes).
    fn parse_plan_only(flags: &[&str]) -> Result<TilesArgs, clap::Error> {
        let mut argv = vec![
            "tylertoo",
            "tiles",
            "in.parquet",
            "--save-plan",
            "p.plan",
            "--plan-only",
        ];
        argv.extend_from_slice(flags);
        Cli::try_parse_from(argv).map(|cli| match cli.command {
            Command::Tiles(a) => *a,
            other => panic!("expected tiles subcommand, got {other:?}"),
        })
    }

    /// #600: every export-only flag parses alongside `--plan-only` and is
    /// named by `ignored_export_options`; nothing set, nothing named.
    #[test]
    fn plan_only_parses_and_names_every_export_only_flag() {
        let bare = parse_plan_only(&[]).expect("bare --plan-only parses");
        assert!(ignored_export_options(&bare).is_empty());

        for (argv, name) in [
            (&["--layer-name", "x"][..], "--layer-name"),
            (&["--max-tile-size", "1M"][..], "--max-tile-size"),
            (
                &["--no-simple-clip-fastpath"][..],
                "--no-simple-clip-fastpath",
            ),
            (&["--tile-buffer", "4"][..], "--tile-buffer"),
            (&["--partition-wave", "4"][..], "--partition-wave"),
            (&["--feature-order", "pop:desc"][..], "--feature-order"),
            (&["--feature-id", "id"][..], "--feature-id"),
            (&["--report", "r.json"][..], "--report"),
            (&["--keep-overview", "o.parquet"][..], "--keep-overview"),
            (&["--tile-range", "21..40"][..], "--tile-range"),
            (&["--force"][..], "--force"),
        ] {
            let a = parse_plan_only(argv)
                .unwrap_or_else(|e| panic!("--plan-only with {argv:?} must parse: {e}"));
            assert_eq!(ignored_export_options(&a), vec![name], "{argv:?}");
        }

        // Several at once: one list, in --help order.
        let a = parse_plan_only(&["--force", "--layer-name", "x", "--report", "r.json"]).unwrap();
        assert_eq!(
            ignored_export_options(&a),
            vec!["--layer-name", "--report", "--force"]
        );

        // The clap default IS export's default, so a bare run never names
        // --tile-buffer.
        assert_eq!(
            bare.tile_buffer,
            tylertoo_core::overview::export::ExportOptions::default().tile_buffer
        );

        // A default typed explicitly is the same run as omitting it.
        let a = parse_plan_only(&[
            "--tile-buffer",
            "8",
            "--partition-wave",
            "auto",
            "--feature-order",
            "input",
        ])
        .unwrap();
        assert!(ignored_export_options(&a).is_empty());
    }

    /// #600 kept these hard errors: `--plan-only` still requires
    /// `--save-plan`, and `--tile-range` still conflicts with `--shard`.
    #[test]
    fn plan_only_still_refuses_contradictions_at_parse_time() {
        let err = Cli::try_parse_from(["tylertoo", "tiles", "in.parquet", "--plan-only"])
            .expect_err("--plan-only without --save-plan");
        assert!(err.to_string().contains("--save-plan"), "{err}");

        let err = parse_plan_only(&[
            "--shard",
            "coarse",
            "--shard-plan",
            "s.json",
            "--tile-range",
            "21..40",
        ])
        .expect_err("--tile-range with --shard");
        assert!(err.to_string().contains("cannot be used with"), "{err}");
    }

    fn parse_export(flags: &[&str]) -> ExportPmtilesArgs {
        let mut argv = vec!["tylertoo", "export-pmtiles", "in.parquet", "out.pmtiles"];
        argv.extend_from_slice(flags);
        match Cli::try_parse_from(argv)
            .expect("export-pmtiles args should parse")
            .command
        {
            Command::ExportPmtiles(a) => a,
            other => panic!("expected export-pmtiles subcommand, got {other:?}"),
        }
    }

    fn parse_pyramid(flags: &[&str]) -> PyramidArgs {
        let mut argv = vec![
            "tylertoo",
            "pyramid",
            "out.pmtiles",
            "--band",
            "0-5:coarse.parquet",
        ];
        argv.extend_from_slice(flags);
        match Cli::try_parse_from(argv)
            .expect("pyramid args should parse")
            .command
        {
            Command::Pyramid(a) => a,
            other => panic!("expected pyramid subcommand, got {other:?}"),
        }
    }

    /// #374: `pyramid` takes the same `--feature-order` as `tiles` and
    /// `export-pmtiles`, once for the whole pyramid, and it has to reach
    /// `PyramidOptions::export` — the library already honours it per band,
    /// the CLI just never set it, so `ExportOptions::default()` always won.
    #[test]
    fn feature_order_flag_reaches_pyramid() {
        let column = |name: &str, descending| FeatureOrder::Column {
            name: name.to_string(),
            descending,
        };

        assert_eq!(parse_pyramid(&[]).feature_order, FeatureOrder::Input);
        assert_eq!(
            pyramid_options(&parse_pyramid(&[])).export.feature_order,
            FeatureOrder::Input
        );

        assert_eq!(
            parse_pyramid(&["--feature-order", "level:desc"]).feature_order,
            column("level", true)
        );
        let opts = pyramid_options(&parse_pyramid(&["--feature-order", "level:desc"]));
        assert_eq!(opts.export.feature_order, column("level", true));
        // The rest of the export options are untouched by the new knob.
        assert_eq!(opts.export.tile_size_limit, None);
        assert_eq!(
            pyramid_options(&parse_pyramid(&["--feature-order", "level"]))
                .export
                .feature_order,
            column("level", false)
        );

        // Same parser as the other two commands: a bad direction is rejected
        // at parse time.
        let mut argv = vec![
            "tylertoo",
            "pyramid",
            "out.pmtiles",
            "--band",
            "0-5:a.parquet",
        ];
        argv.extend_from_slice(&["--feature-order", "level:dsc"]);
        assert!(Cli::try_parse_from(argv).is_err());
    }

    /// #361: the flag has to actually reach `ExportOptions` on both commands.
    /// `FromStr` coverage alone would pass with the flag wired to nothing.
    #[test]
    fn feature_order_flag_reaches_both_commands() {
        let column = |name: &str, descending| FeatureOrder::Column {
            name: name.to_string(),
            descending,
        };

        assert_eq!(parse_tiles(&[]).feature_order, FeatureOrder::Input);
        assert_eq!(parse_export(&[]).feature_order, FeatureOrder::Input);

        assert_eq!(
            parse_tiles(&["--feature-order", "level:desc"]).feature_order,
            column("level", true)
        );
        assert_eq!(
            parse_export(&["--feature-order", "level"]).feature_order,
            column("level", false)
        );

        // A bad direction is rejected at parse time rather than silently
        // sorting by a column that does not exist.
        let mut argv = vec!["tylertoo", "tiles", "in.parquet", "out.pmtiles"];
        argv.extend_from_slice(&["--feature-order", "level:dsc"]);
        assert!(Cli::try_parse_from(argv).is_err());
    }

    /// #443: the flag parses on both `tiles` and `export-pmtiles` (both
    /// copy it verbatim into `ExportOptions::feature_id`; the end-to-end
    /// behaviour is covered in `tests/tiles_facade.rs` and core's export
    /// tests).
    #[test]
    fn feature_id_flag_parses_on_both_commands() {
        assert_eq!(parse_tiles(&[]).feature_id, None);
        assert_eq!(parse_export(&[]).feature_id, None);

        assert_eq!(
            parse_tiles(&["--feature-id", "osm_id"]).feature_id,
            Some("osm_id".to_string())
        );
        assert_eq!(
            parse_export(&["--feature-id", "osm_id"]).feature_id,
            Some("osm_id".to_string())
        );
    }

    #[test]
    fn parse_size_bytes_accepts_suffixed_and_raw() {
        assert_eq!(parse_size_bytes("500K").unwrap(), 500 * 1024);
        assert_eq!(parse_size_bytes("1M").unwrap(), 1024 * 1024);
        // A plain integer is raw bytes — keeps pre-reconciliation invocations working.
        assert_eq!(parse_size_bytes("500000").unwrap(), 500_000);
        assert!(parse_size_bytes("banana").is_err());
    }

    /// #432: a size whose suffix multiplication overflows `usize` must be a
    /// parse error, not a wrapped (silently small) byte count. Before the
    /// fix `99999999999999999G` wrapped in release builds and was accepted.
    #[test]
    fn parse_size_bytes_rejects_overflow() {
        let ceiling = format!("must fit in {} bytes", usize::MAX);
        for s in [
            "99999999999999999G",
            "9223372036854775808K",
            "18446744073709551615M",
        ] {
            assert_eq!(parse_suffixed_size(s), Err(SizeParseError::Overflow), "{s}");
            // The public message (--max-tile-size / --tile-size-limit) names
            // the ceiling on this branch instead of hiding it behind the
            // generic format hint.
            let err = parse_size_bytes(s).expect_err(s);
            assert!(err.starts_with("Invalid size: "), "{s}: {err}");
            assert!(err.contains(&ceiling), "{s}: {err}");
        }
        // The largest representable value still parses.
        assert_eq!(
            parse_size_bytes(&usize::MAX.to_string()).unwrap(),
            usize::MAX
        );
    }

    /// The ceiling clause is for the overflow branch only: garbage gets the
    /// format hint, not a 20-digit number that has nothing to do with it.
    #[test]
    fn malformed_size_error_does_not_mention_the_ceiling() {
        for s in ["banana", "", "1.5G", "-1M", "G"] {
            assert_eq!(
                parse_suffixed_size(s),
                Err(SizeParseError::Malformed),
                "{s:?}"
            );
            let err = parse_size_bytes(s).expect_err(s);
            assert!(err.starts_with("Invalid size: "), "{s:?}: {err}");
            assert!(!err.contains("must fit in"), "{s:?}: {err}");
        }
    }

    #[test]
    fn parse_in_flight_batches_accepts_auto_and_positive() {
        // `auto` maps to the core-sized sentinel; case-insensitive.
        assert_eq!(
            parse_in_flight_batches("auto").unwrap(),
            tylertoo_core::overview::convert::IN_FLIGHT_BATCHES_AUTO
        );
        assert_eq!(parse_in_flight_batches("AUTO").unwrap(), 0);
        // Explicit positive integers pass through.
        assert_eq!(parse_in_flight_batches("8").unwrap(), 8);
        // Explicit 0 is rejected (use `auto`); non-numeric is rejected.
        assert!(parse_in_flight_batches("0").is_err());
        assert!(parse_in_flight_batches("banana").is_err());
    }

    #[test]
    fn parse_read_workers_accepts_auto_and_positive() {
        // `auto` maps to the core-sized sentinel; case-insensitive.
        assert_eq!(
            parse_read_workers("auto").unwrap(),
            tylertoo_core::overview::convert::READ_WORKERS_AUTO
        );
        assert_eq!(parse_read_workers("AUTO").unwrap(), 0);
        // Explicit positive integers pass through, up to the ceiling.
        assert_eq!(parse_read_workers("1").unwrap(), 1);
        let ceiling = tylertoo_core::overview::convert::read_workers_ceiling();
        assert_eq!(parse_read_workers(&ceiling.to_string()).unwrap(), ceiling);
        // Explicit 0 is rejected (use `auto`); non-numeric is rejected; a
        // value above the ceiling is rejected with the ceiling named, rather
        // than reaching the engine as a thread count.
        assert!(parse_read_workers("0").is_err());
        assert!(parse_read_workers("banana").is_err());
        let err = parse_read_workers(&usize::MAX.to_string()).unwrap_err();
        assert!(
            err.contains(&ceiling.to_string()),
            "the rejection must name the ceiling, got: {err}"
        );
    }

    #[test]
    fn parse_read_batch_size_rejects_zero_and_absurd() {
        assert_eq!(parse_read_batch_size("8192").unwrap(), 8192);
        assert_eq!(
            parse_read_batch_size(&READ_BATCH_SIZE_MAX.to_string()).unwrap(),
            READ_BATCH_SIZE_MAX
        );
        assert!(parse_read_batch_size("0").is_err());
        assert!(parse_read_batch_size("banana").is_err());
        assert!(parse_read_batch_size(&(READ_BATCH_SIZE_MAX + 1).to_string()).is_err());
    }

    #[test]
    fn tiles_accepts_convert_tuning_flags() {
        // #249: every shared convert knob must be reachable on the one-shot command.
        let a = parse_tiles(&[
            "--polygon-visibility",
            "2.0",
            "--collapse",
            "--drop-rate",
            "1.3",
            "--profile",
            "bounded",
            "--cluster",
            "--no-coalesce-lines",
        ]);
        // `Some` rather than a bare 2.0: the knob records whether it was
        // given, so an explicit 2.0 is distinguishable from silence. That is
        // what lets --verbatim supply a different default without clobbering
        // an override.
        assert_eq!(a.tuning.polygon_visibility, Some(2.0));
        assert!(a.tuning.collapse);
        assert_eq!(a.tuning.drop_rate, 1.3);
        assert_eq!(a.tuning.profile, "bounded");
        assert!(a.tuning.cluster);
        assert!(a.tuning.no_coalesce_lines);
    }

    // --- #314 ergonomic one-step: --keep-overview + intermediate location ----

    /// `--keep-overview` parses to a path on `tiles` and defaults to off.
    #[test]
    fn tiles_keep_overview_flag_parses() {
        assert!(parse_tiles(&[]).keep_overview.is_none());
        let a = parse_tiles(&["--keep-overview", "ov.parquet"]);
        assert_eq!(a.keep_overview, Some(PathBuf::from("ov.parquet")));
    }

    /// Intermediate-overview directory precedence (#314): `--spill-dir`
    /// beats an explicitly-set `$TMPDIR`, which beats the output's own
    /// directory; a bare output filename with no `$TMPDIR` falls back to
    /// the process temp dir.
    #[test]
    fn resolve_intermediate_dir_precedence() {
        use std::ffi::OsStr;
        let out = std::path::Path::new("/data/out/tiles.pmtiles");

        // --spill-dir wins over everything.
        assert_eq!(
            resolve_intermediate_dir(
                Some(std::path::Path::new("/spill")),
                Some(OsStr::new("/tmpdir")),
                out
            ),
            PathBuf::from("/spill")
        );
        // $TMPDIR (explicitly set, non-empty) beats the output directory.
        assert_eq!(
            resolve_intermediate_dir(None, Some(OsStr::new("/tmpdir")), out),
            PathBuf::from("/tmpdir")
        );
        // An empty $TMPDIR is ignored.
        assert_eq!(
            resolve_intermediate_dir(None, Some(OsStr::new("")), out),
            PathBuf::from("/data/out")
        );
        // Default: next to the output (same filesystem as the final artifact).
        assert_eq!(
            resolve_intermediate_dir(None, None, out),
            PathBuf::from("/data/out")
        );
        // Bare output filename: process temp dir fallback.
        assert_eq!(
            resolve_intermediate_dir(None, None, std::path::Path::new("tiles.pmtiles")),
            std::env::temp_dir()
        );
    }

    /// #314 free-space preflight: quiet when the estimated intermediate
    /// (plus the 5% margin) fits, a warning naming the directory, the
    /// shortfall, and the `--spill-dir` escape hatch when it does not.
    #[test]
    fn intermediate_space_warning_thresholds() {
        let dir = std::path::Path::new("/some/volume");
        let gib = 1024u64 * 1024 * 1024;

        // Fits comfortably: no warning.
        assert!(intermediate_space_warning(10 * gib, 11 * gib, dir).is_none());
        // Exactly the estimate + 5% margin still fits.
        let est = 20 * gib;
        assert!(intermediate_space_warning(est, est + est / 20, dir).is_none());
        // One byte short of the margin: warn, naming dir and remedies.
        let msg =
            intermediate_space_warning(est, est + est / 20 - 1, dir).expect("shortfall must warn");
        assert!(msg.contains("/some/volume"), "names the directory: {msg}");
        assert!(msg.contains("--spill-dir"), "names the remedy: {msg}");
        assert!(msg.contains("--keep-overview"), "names the remedy: {msg}");
        // Zero estimate (unknown/empty input): never warns.
        assert!(intermediate_space_warning(0, 0, dir).is_none());
    }

    /// The intermediate size estimate is the local input bytes: file size
    /// for a file, the sum of top-level .parquet sizes for a directory,
    /// the sum of existing local manifest entries for --files-from.
    #[test]
    fn estimate_local_input_bytes_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("a.parquet");
        let f2 = dir.path().join("b.parquet");
        std::fs::write(&f1, vec![0u8; 100]).unwrap();
        std::fs::write(&f2, vec![0u8; 50]).unwrap();
        std::fs::write(dir.path().join("ignored.txt"), vec![0u8; 999]).unwrap();

        // Single file.
        assert_eq!(
            estimate_local_input_bytes(&InputSpec::Path(f1.clone())),
            Some(100)
        );
        // Directory: only .parquet entries count.
        assert_eq!(
            estimate_local_input_bytes(&InputSpec::Path(dir.path().to_path_buf())),
            Some(150)
        );
        // Nonexistent / remote-looking path: quiet None.
        assert_eq!(
            estimate_local_input_bytes(&InputSpec::Path(PathBuf::from("s3://bucket/x.parquet"))),
            None
        );
        // Manifest: sum of existing local entries; comments/blanks skipped.
        let manifest = dir.path().join("m.txt");
        std::fs::write(
            &manifest,
            format!(
                "# comment\n{}\n\n{}\ns3://bucket/remote.parquet\n",
                f1.display(),
                f2.display()
            ),
        )
        .unwrap();
        assert_eq!(
            estimate_local_input_bytes(&InputSpec::Manifest(manifest)),
            Some(150)
        );
    }

    /// #317 / #279: the representation selector and the square disposition
    /// are reachable on BOTH `overview` and the one-shot `tiles` facade, and
    /// they build the right core options.
    #[test]
    fn representation_and_collapse_square_flags() {
        use tylertoo_core::overview::convert::{LevelPlan, RepresentationBand};
        use tylertoo_core::overview::level::Mode;
        use tylertoo_core::overview::simplify::{CollapseMode, Representation};

        let a = parse_tiles(&["--representation", "0-7:point,8-13:geom"]);
        assert_eq!(
            a.tuning.representation.as_deref(),
            Some("0-7:point,8-13:geom")
        );

        let b = parse_tiles(&["--collapse-square"]);
        assert!(b.tuning.collapse_square);

        // --collapse and --collapse-square conflict.
        assert!(Cli::try_parse_from([
            "tylertoo",
            "tiles",
            "in.parquet",
            "out.pmtiles",
            "--collapse",
            "--collapse-square",
        ])
        .is_err());

        // build_convert_options maps both through to core.
        let opts = b
            .tuning
            .build_convert_options(
                Mode::Duplicating,
                LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 14,
                },
                None,
                false,
            )
            .unwrap();
        assert_eq!(opts.simplify.collapse, CollapseMode::Square);

        let opts = a
            .tuning
            .build_convert_options(
                Mode::Duplicating,
                LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 14,
                },
                None,
                false,
            )
            .unwrap();
        assert_eq!(
            opts.representation,
            vec![
                RepresentationBand {
                    min_zoom: 0,
                    max_zoom: 7,
                    repr: Representation::Point
                },
                RepresentationBand {
                    min_zoom: 8,
                    max_zoom: 13,
                    repr: Representation::Geometry
                },
            ]
        );
        assert_eq!(opts.simplify.collapse, CollapseMode::Drop);

        // A malformed spec is a CLI-level error.
        let bad = parse_tiles(&["--representation", "0-7:blob"]);
        assert!(bad
            .tuning
            .build_convert_options(
                Mode::Duplicating,
                LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 14,
                },
                None,
                false,
            )
            .is_err());
    }

    /// The same flags parse on the two-step `overview` subcommand.
    #[test]
    fn overview_accepts_representation_flags() {
        let argv = [
            "tylertoo",
            "overview",
            "in.parquet",
            "out.parquet",
            "--representation",
            "0-5:square",
            "--max-zoom",
            "12",
        ];
        let cli = Cli::try_parse_from(argv).expect("overview should accept --representation");
        match cli.command {
            Command::Overview(a) => {
                assert_eq!(a.tuning.representation.as_deref(), Some("0-5:square"));
            }
            other => panic!("expected overview subcommand, got {other:?}"),
        }
    }

    #[test]
    fn parse_partition_wave_accepts_auto_and_positive() {
        // `auto` maps to the core-sized sentinel; case-insensitive.
        assert_eq!(
            parse_partition_wave("auto").unwrap(),
            tylertoo_core::overview::export::PARTITION_WAVE_AUTO
        );
        assert_eq!(parse_partition_wave("AUTO").unwrap(), 0);
        // Explicit positive integers pass through (6 keeps the old behavior).
        assert_eq!(parse_partition_wave("6").unwrap(), 6);
        assert_eq!(parse_partition_wave("32").unwrap(), 32);
        // Explicit 0 is rejected (use `auto`); non-numeric is rejected.
        assert!(parse_partition_wave("0").is_err());
        assert!(parse_partition_wave("banana").is_err());
    }

    #[test]
    fn tiles_partition_wave_defaults_auto_and_overrides() {
        // Default is the auto sentinel (0) on the one-shot command.
        let default = parse_tiles(&[]);
        assert_eq!(
            default.partition_wave,
            tylertoo_core::overview::export::PARTITION_WAVE_AUTO
        );
        // Explicit override is honoured verbatim.
        let overridden = parse_tiles(&["--partition-wave", "6"]);
        assert_eq!(overridden.partition_wave, 6);
    }

    #[test]
    fn export_pmtiles_partition_wave_defaults_auto_and_overrides() {
        let parse_export = |flags: &[&str]| -> ExportPmtilesArgs {
            let mut argv = vec!["tylertoo", "export-pmtiles", "in.parquet", "out.pmtiles"];
            argv.extend_from_slice(flags);
            match Cli::try_parse_from(argv)
                .expect("export-pmtiles args should parse")
                .command
            {
                Command::ExportPmtiles(a) => a,
                other => panic!("expected export-pmtiles subcommand, got {other:?}"),
            }
        };
        assert_eq!(
            parse_export(&[]).partition_wave,
            tylertoo_core::overview::export::PARTITION_WAVE_AUTO
        );
        assert_eq!(parse_export(&["--partition-wave", "12"]).partition_wave, 12);
    }

    #[test]
    fn tiles_size_limit_alias_matches_max_tile_size() {
        // The two spellings are aliases and both accept human-readable sizes.
        let a = parse_tiles(&["--max-tile-size", "500K"]);
        let b = parse_tiles(&["--tile-size-limit", "500K"]);
        assert_eq!(a.max_tile_size, Some(500 * 1024));
        assert_eq!(a.max_tile_size, b.max_tile_size);
    }

    #[test]
    fn tile_size_cap_defaults_to_500k_and_zero_disables() {
        // #280: the per-tile cap is on by default at 500K on both commands.
        assert_eq!(parse_tiles(&[]).max_tile_size, None);
        assert_eq!(resolve_tiles_size_limit(None, false), Some(500 * 1024));
        assert_eq!(parse_export(&[]).tile_size_limit, 500 * 1024);

        // `0` is the off switch: the CLI value maps to `None` (cap disabled).
        assert_eq!(
            parse_tiles(&["--max-tile-size", "0"]).max_tile_size,
            Some(0)
        );
        assert_eq!(size_limit_opt(0), None);
        assert_eq!(size_limit_opt(500 * 1024), Some(500 * 1024));
    }

    /// #345/#360: verbatim means every feature reaches the tile, so the
    /// default size valve — which sheds features to fit a byte budget — must
    /// not quietly apply. An explicit value still wins, so a caller who wants
    /// a cap with verbatim generalization can say so.
    #[test]
    fn verbatim_disables_the_tile_size_cap_unless_asked_otherwise() {
        assert_eq!(resolve_tiles_size_limit(None, true), None);
        assert_eq!(
            resolve_tiles_size_limit(None, false),
            Some(DEFAULT_MAX_TILE_SIZE)
        );
        assert_eq!(resolve_tiles_size_limit(Some(1024), true), Some(1024));
        assert_eq!(resolve_tiles_size_limit(Some(0), false), None);
    }

    /// The flag reaches both commands that build a conversion, and switches
    /// the whole ladder off rather than one knob.
    #[test]
    fn verbatim_flag_switches_off_the_whole_ladder() {
        let tiles = parse_tiles(&["--verbatim"]);
        assert!(tiles.tuning.verbatim);
        let opts = tiles
            .tuning
            .build_convert_options(
                tylertoo_core::overview::level::Mode::Duplicating,
                tylertoo_core::overview::convert::LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 6,
                },
                None,
                false,
            )
            .unwrap();
        assert!(opts.is_verbatim(), "the flag must reach ConvertOptions");

        // ...and is off by default.
        let plain = parse_tiles(&[]);
        assert!(!plain.tuning.verbatim);
        let opts = plain
            .tuning
            .build_convert_options(
                tylertoo_core::overview::level::Mode::Duplicating,
                tylertoo_core::overview::convert::LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 6,
                },
                None,
                false,
            )
            .unwrap();
        assert!(!opts.is_verbatim());
    }

    /// Build convert options for `tiles` with the given flags, on a plain
    /// duplicating z0-6 plan.
    fn verbatim_opts(flags: &[&str]) -> tylertoo_core::overview::convert::ConvertOptions {
        parse_tiles(flags)
            .tuning
            .build_convert_options(
                tylertoo_core::overview::level::Mode::Duplicating,
                tylertoo_core::overview::convert::LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 6,
                },
                None,
                false,
            )
            .unwrap()
    }

    /// The `--entry-zoom` grammar, which had no tests despite +116 lines of
    /// new surface and a mandatory-TDD policy.
    #[test]
    fn entry_zoom_spec_grammar() {
        use tylertoo_core::overview::ladder::EntryZoomKind;

        let ok = |spec: &str| parse_entry_zoom(spec).unwrap_or_else(|e| panic!("{spec:?}: {e}"));

        let s = ok("density:5000=4,1000=6,200=8");
        assert_eq!(s.column, "density");
        match &s.kind {
            EntryZoomKind::Explicit(pairs) => {
                assert_eq!(pairs, &[(5000.0, 4), (1000.0, 6), (200.0, 8)]);
            }
            other => panic!("expected explicit rungs, got {other:?}"),
        }

        // Whitespace and a trailing comma are tolerated.
        let s = ok("density: 5000 = 4 , 200 = 8 ,");
        match &s.kind {
            EntryZoomKind::Explicit(pairs) => assert_eq!(pairs, &[(5000.0, 4), (200.0, 8)]),
            other => panic!("{other:?}"),
        }

        for bad in [
            "density",          // no ':'
            ":5000=4",          // empty column
            "density:",         // no pairs
            "density:5000",     // missing '='
            "density:x=4",      // value not a number
            "density:5000=z",   // zoom not a number
            "density:5000=-1",  // negative zoom
            "density:5000=999", // beyond u8
        ] {
            assert!(parse_entry_zoom(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// #364: a ladder implies collapse-to-point, and that is applied in CORE so
    /// every caller gets it — the CLI is not the only entry point, and the
    /// Python bindings previously lost the entire coarsest level to the drop
    /// default. `--collapse-square` still wins.
    #[test]
    fn a_ladder_implies_collapse_to_point_for_every_caller() {
        use tylertoo_core::overview::simplify::CollapseMode;

        // The CLI itself leaves the mode alone...
        let cli = verbatim_opts(&["--magnitude-ladder", "level"]);
        assert_eq!(cli.simplify.collapse, CollapseMode::Drop);
        assert_eq!(
            verbatim_opts(&["--magnitude-ladder", "level", "--collapse-square"])
                .simplify
                .collapse,
            CollapseMode::Square,
            "an explicit --collapse-square is not overridden"
        );

        // Core applies the implication (covered by
        // `ladder_implies_collapse_to_point` in overview::convert), so a
        // library or Python caller that builds ConvertOptions directly gets it
        // too — which is what the CLI must NOT duplicate.
        assert!(
            cli.entry_zoom.is_some(),
            "the ladder reaches ConvertOptions"
        );
    }

    /// #364/#367: a ladder is not verbatim, and the run must not claim it is.
    ///
    /// `--verbatim` logs "every level reproduces the input" when
    /// `is_verbatim()` holds. An entry-zoom ladder deliberately holds features
    /// OUT of coarse levels, so a laddered run does not reproduce its input
    /// there — reporting it as verbatim would make that line a lie on exactly
    /// the runs that combine the two.
    #[test]
    fn a_ladder_means_the_run_is_not_verbatim() {
        let laddered = verbatim_opts(&["--verbatim", "--magnitude-ladder", "level"]);
        assert!(
            laddered.entry_zoom.is_some(),
            "precondition: the ladder must reach ConvertOptions"
        );
        assert!(
            !laddered.is_verbatim(),
            "a ladder holds features out of coarse levels, so this is not verbatim"
        );
        // The generalization knobs ARE still all off — that half is unchanged,
        // which is what keeps the partitioning guard firing.
        assert!(laddered.simplify.factor == 0.0 && !laddered.density.enabled);

        // Without a ladder, --verbatim is still verbatim.
        assert!(verbatim_opts(&["--verbatim"]).is_verbatim());
    }

    /// Verbatim in partitioning mode produces an empty pyramid, so it is
    /// rejected rather than silently emitted.
    ///
    /// Partitioning writes each feature at exactly its `min_level`; with
    /// thinning off every feature wins level 0, so the whole dataset lands in
    /// the coarsest level and every finer level is omitted as empty — with a
    /// warning that blames the visibility gates, which is doubly misleading.
    #[test]
    fn verbatim_is_rejected_in_partitioning_mode() {
        let err = parse_tiles(&["--verbatim"])
            .tuning
            .build_convert_options(
                tylertoo_core::overview::level::Mode::Partitioning,
                tylertoo_core::overview::convert::LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 6,
                },
                None,
                false,
            )
            .expect_err("verbatim + partitioning must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("--verbatim requires --mode duplicating"),
            "{msg}"
        );

        // Duplicating is fine, and partitioning without --verbatim is fine.
        assert!(parse_tiles(&["--verbatim"])
            .tuning
            .build_convert_options(
                tylertoo_core::overview::level::Mode::Duplicating,
                tylertoo_core::overview::convert::LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 6,
                },
                None,
                false,
            )
            .is_ok());
        assert!(parse_tiles(&[])
            .tuning
            .build_convert_options(
                tylertoo_core::overview::level::Mode::Partitioning,
                tylertoo_core::overview::convert::LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 6,
                },
                None,
                false,
            )
            .is_ok());
    }

    /// `--verbatim` is a set of DEFAULTS, not an override.
    ///
    /// The whole documented point of the flag is that it composes — "apply it
    /// and override afterwards for nearly-verbatim". If it is applied last and
    /// wholesale, `--verbatim --simplify-factor 0.5` silently means
    /// `--simplify-factor 0`, which is the opposite of what the docs promise
    /// and gives no sign it ignored you.
    #[test]
    fn explicit_tuning_flags_win_over_verbatim() {
        let opts = verbatim_opts(&[
            "--verbatim",
            "--simplify-factor",
            "0.5",
            "--polygon-thinning",
            "2.0",
            "--line-visibility",
            "3.0",
        ]);
        assert_eq!(opts.simplify.factor, 0.5, "explicit --simplify-factor");
        assert_eq!(
            opts.assign.polygon_thinning, 2.0,
            "explicit --polygon-thinning"
        );
        assert_eq!(
            opts.assign.line_visibility, 3.0,
            "explicit --line-visibility"
        );

        // Everything NOT named still goes to the verbatim default.
        assert_eq!(opts.assign.point_thinning, 0.0);
        assert_eq!(opts.assign.line_thinning, 0.0);
        assert_eq!(opts.assign.polygon_visibility, 0.0);
        assert!(!opts.density.enabled);

        // And bare --verbatim still zeroes the lot.
        let bare = verbatim_opts(&["--verbatim"]);
        assert_eq!(bare.simplify.factor, 0.0);
        assert_eq!(bare.assign.polygon_thinning, 0.0);
        assert_eq!(bare.assign.line_visibility, 0.0);

        // A default run is unchanged.
        let plain = verbatim_opts(&[]);
        assert_eq!(plain.simplify.factor, 1.0);
        assert_eq!(plain.assign.polygon_thinning, 1.0);
        assert_eq!(plain.assign.line_visibility, 2.0);
        assert_eq!(plain.assign.polygon_visibility, 2.0);
        assert_eq!(plain.assign.line_thinning, 1.0);
        assert_eq!(plain.assign.point_thinning, 4.0);
    }

    #[test]
    fn tiles_build_convert_options_threads_tuning() {
        use tylertoo_core::overview::convert::LevelPlan;
        use tylertoo_core::overview::level::{MemoryProfile, Mode};

        let a = parse_tiles(&[
            "--polygon-visibility",
            "2.0",
            "--collapse",
            "--drop-rate",
            "1.3",
            "--profile",
            "bounded",
        ]);
        let opts = a
            .tuning
            .build_convert_options(
                Mode::Duplicating,
                LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 9,
                },
                None,
                false,
            )
            .expect("valid tuning should build options");

        assert_eq!(opts.assign.polygon_visibility, 2.0);
        assert_eq!(
            opts.simplify.collapse,
            tylertoo_core::overview::simplify::CollapseMode::Point
        );
        assert_eq!(opts.density.drop_rate, 1.3);
        assert!(matches!(opts.profile, MemoryProfile::Bounded));
    }

    #[test]
    fn build_convert_options_enforces_shared_validation() {
        use tylertoo_core::overview::convert::LevelPlan;
        use tylertoo_core::overview::level::Mode;

        let levels = || LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 6,
        };

        // --class-rank and --sort-key are mutually exclusive.
        let a = parse_tiles(&["--class-rank", "k:a=1", "--sort-key", "height"]);
        assert!(a
            .tuning
            .build_convert_options(Mode::Duplicating, levels(), None, false)
            .is_err());

        // --accumulate-attribute requires --cluster.
        let a = parse_tiles(&["--accumulate-attribute", "pop:sum"]);
        assert!(a
            .tuning
            .build_convert_options(Mode::Duplicating, levels(), None, false)
            .is_err());

        // --cluster requires duplicating mode.
        let a = parse_tiles(&["--cluster"]);
        assert!(a
            .tuning
            .build_convert_options(Mode::Partitioning, levels(), None, false)
            .is_err());
        assert!(a
            .tuning
            .build_convert_options(Mode::Duplicating, levels(), None, false)
            .is_ok());
    }

    // --- #316: tuning parity between `tiles` and the two-step chain ----------

    /// `--max-zoom` parses through core's `MaxZoom` (numbers and `auto`,
    /// any case); clap surfaces a bad value as a usage error.
    #[test]
    fn max_zoom_flag_parses_numbers_and_auto() {
        assert_eq!(
            parse_tiles(&["--max-zoom", "9"]).max_zoom,
            MaxZoom::Fixed(9)
        );
        assert_eq!(parse_tiles(&["--max-zoom", "AUTO"]).max_zoom, MaxZoom::Auto);
        assert_eq!(parse_tiles(&[]).max_zoom, MaxZoom::Fixed(14));
    }

    /// #444: a `Fixed` `--max-zoom` resolves to the exact same number with
    /// **zero I/O** — the input does not even exist — and leaves the options
    /// untouched: the numeric path hands core the same `u8` it always did.
    #[test]
    fn resolve_max_zoom_fixed_is_pure_and_does_not_touch_the_input() {
        let spec = InputSpec::Path(PathBuf::from("/nonexistent/definitely-not-a-file.parquet"));
        for z in [0u8, 6, 14, 30] {
            let mut options = tylertoo_core::overview::convert::ConvertOptions::default();
            let before = format!("{:?}", options.levels);
            assert_eq!(
                resolve_max_zoom(MaxZoom::Fixed(z), &spec, &mut options).unwrap(),
                z
            );
            assert_eq!(format!("{:?}", options.levels), before);
        }
    }

    #[test]
    fn resolve_level_plan_gsd_overrides_zoom_range() {
        use tylertoo_core::overview::convert::LevelPlan;

        // No --gsd → the zoom range is used verbatim.
        match resolve_level_plan(None, 2, 11).unwrap() {
            LevelPlan::ZoomRange { min_zoom, max_zoom } => {
                assert_eq!((min_zoom, max_zoom), (2, 11));
            }
            other => panic!("expected ZoomRange, got {other:?}"),
        }

        // An explicit list wins, is parsed in order, and tolerates whitespace.
        match resolve_level_plan(Some("1000, 500 ,250"), 0, 14).unwrap() {
            LevelPlan::Gsds(gsds) => assert_eq!(gsds, vec![1000.0, 500.0, 250.0]),
            other => panic!("expected Gsds, got {other:?}"),
        }

        // A malformed list errors, naming the flag.
        let err = resolve_level_plan(Some("1000,banana"), 0, 14).unwrap_err();
        assert!(err.to_string().contains("--gsd"), "names the flag: {err}");
    }

    #[test]
    fn tiles_gsd_and_report_flags_thread_through() {
        use tylertoo_core::overview::convert::LevelPlan;

        // --gsd on `tiles` reaches the same absolute-GSD ladder as `overview`.
        let a = parse_tiles(&["--gsd", "800,400,200"]);
        match resolve_level_plan(a.gsd.as_deref(), a.min_zoom, a.max_zoom.plan_zoom()).unwrap() {
            LevelPlan::Gsds(gsds) => assert_eq!(gsds, vec![800.0, 400.0, 200.0]),
            other => panic!("expected Gsds, got {other:?}"),
        }

        // Without --gsd the min/max zoom range still drives the plan.
        let a = parse_tiles(&["--min-zoom", "3", "--max-zoom", "10"]);
        assert!(a.gsd.is_none());

        // --report is accepted and captured as a path.
        let a = parse_tiles(&["--report", "out/report.json"]);
        assert_eq!(a.report, Some(PathBuf::from("out/report.json")));
    }

    /// #371: the CLI is deliberately not where the zoom ceiling lives — clap
    /// parses `--max-zoom 33` and `--gsd 0.000005` fine, and the level plan
    /// carries them verbatim. Core rejects both at options validation, so the
    /// library, the Python bindings and the CLI all get the same guarantee
    /// from one place. This pins the hand-off: what the CLI hands core is the
    /// out-of-range value, unclamped.
    #[test]
    fn tiles_passes_an_out_of_range_zoom_through_to_core_unclamped() {
        use tylertoo_core::overview::convert::LevelPlan;

        let a = parse_tiles(&["--min-zoom", "30", "--max-zoom", "33"]);
        assert_eq!(
            a.max_zoom,
            MaxZoom::Fixed(33),
            "the CLI must not silently clamp"
        );
        match resolve_level_plan(a.gsd.as_deref(), a.min_zoom, a.max_zoom.plan_zoom()).unwrap() {
            LevelPlan::ZoomRange { max_zoom, .. } => assert_eq!(max_zoom, 33),
            other => panic!("expected ZoomRange, got {other:?}"),
        }

        let a = parse_tiles(&["--gsd", "0.000005"]);
        match resolve_level_plan(a.gsd.as_deref(), a.min_zoom, a.max_zoom.plan_zoom()).unwrap() {
            LevelPlan::Gsds(gsds) => assert_eq!(gsds, vec![0.000_005]),
            other => panic!("expected Gsds, got {other:?}"),
        }
    }

    /// Guards against future drift: every non-hidden long flag on `overview`
    /// or `export-pmtiles` must be reachable on the one-step `tiles` command,
    /// or be an explicitly allow-listed structural exception. `--spill-dir`,
    /// `--gsd`, and `--report` are covered here by virtue of being present.
    #[test]
    fn tiles_has_parity_with_two_step_flags() {
        use clap::CommandFactory;
        use std::collections::BTreeSet;

        /// Non-hidden long spellings a command accepts (primary + visible aliases).
        fn longs(cmd: &clap::Command) -> BTreeSet<String> {
            let mut set = BTreeSet::new();
            for arg in cmd.get_arguments() {
                if arg.is_hide_set() {
                    continue;
                }
                if let Some(long) = arg.get_long() {
                    set.insert(long.to_string());
                }
                if let Some(aliases) = arg.get_visible_aliases() {
                    set.extend(aliases.into_iter().map(str::to_string));
                }
            }
            set
        }

        let tiles_longs = longs(&TilesArgs::command());

        // Flags on `overview`/`export-pmtiles` intentionally NOT mirrored on
        // `tiles`, each for a structural reason:
        //   mode / cogp-compat -> `tiles` is always duplicating (partitioning
        //                         can't be exported to per-tile MVT), so the
        //                         mode knob and its partitioning-only footer
        //                         key are meaningless here.
        //   tile-size-limit    -> reachable on `tiles` as --max-tile-size (the
        //                         two are hidden aliases of each other), so the
        //                         cap IS present, just under the other spelling.
        //   zoom-ceiling       -> the coarse half of a sharded build (#498).
        //                         `tiles` already owns --max-zoom, which means
        //                         "build this deep", and derives the export
        //                         ceiling from `--shard coarse` against the
        //                         shard plan's pivot. A second flag carrying a
        //                         zoom number with the OPPOSITE meaning is the
        //                         exact footgun the rename off --max-zoom was
        //                         meant to remove. The mechanism is reachable
        //                         on `tiles`; only this spelling of it is not.
        let allow: BTreeSet<&str> = ["mode", "cogp-compat", "tile-size-limit", "zoom-ceiling"]
            .into_iter()
            .collect();

        let mut missing: Vec<String> = longs(&OverviewArgs::command())
            .into_iter()
            .chain(longs(&ExportPmtilesArgs::command()))
            .filter(|long| !tiles_longs.contains(long) && !allow.contains(long.as_str()))
            .collect();
        missing.sort();
        missing.dedup();

        assert!(
            missing.is_empty(),
            "flags on overview/export-pmtiles not surfaced on `tiles` — add each \
             to TilesArgs, or to the allow-list with a documented reason: {missing:?}"
        );
    }
    // --- #429: out-of-range honesty in the tiles summary ---------------------

    /// A bare "N tiles" line is fine only when nothing was lost. With lost
    /// features the line must name them — and name WHICH loss, since the two
    /// have different fixes — and a zero-tile archive must never read as an
    /// unqualified success.
    #[test]
    fn tiles_summary_line_names_out_of_range_losses() {
        let clean = tiles_summary_line(
            1234,
            0,
            14,
            1.5,
            &SummaryLosses {
                ..SummaryLosses::default()
            },
        );
        assert_eq!(clean, "1,234 tiles across z0..z14 in 1.50s");

        let empty = tiles_summary_line(
            0,
            0,
            14,
            0.05,
            &SummaryLosses {
                out_of_range: 3,
                ..SummaryLosses::default()
            },
        );
        assert!(
            empty.starts_with(
                "0 tiles \u{2014} 3 feature(s) dropped (outside the declared CRS range)"
            ),
            "a wrong-CRS run must not read as a clean success: {empty}"
        );

        let partial = tiles_summary_line(
            10,
            0,
            14,
            0.2,
            &SummaryLosses {
                out_of_range: 1,
                ..SummaryLosses::default()
            },
        );
        assert!(
            partial.contains("10 tiles across z0..z14")
                && partial.contains("1 feature(s) dropped (outside the declared CRS range)"),
            "a partial loss still reports its tiles AND its losses: {partial}"
        );

        // The Mercator-domain loss is named separately: nothing to reproject.
        let polar = tiles_summary_line(
            0,
            0,
            14,
            0.05,
            &SummaryLosses {
                unprojectable: 7,
                ..SummaryLosses::default()
            },
        );
        assert!(
            polar.contains(
                "7 feature(s) dropped (|lat| > 85.05\u{b0}, outside the Web Mercator \
                 tiling domain)"
            ) && !polar.contains("declared CRS range"),
            "an Arctic extract must be told why it tiled to nothing: {polar}"
        );

        // Both at once, both named.
        let both = tiles_summary_line(
            5,
            0,
            14,
            0.1,
            &SummaryLosses {
                out_of_range: 2,
                unprojectable: 3,
                ..SummaryLosses::default()
            },
        );
        assert!(
            both.contains("2 feature(s) dropped (outside the declared CRS range)")
                && both.contains("3 feature(s) dropped (|lat| > 85.05\u{b0}"),
            "{both}"
        );

        // #431: encode-time drops are a post-clip loss and are named as such.
        let encode = tiles_summary_line(
            5,
            0,
            14,
            0.1,
            &SummaryLosses {
                encode_dropped: 4,
                ..SummaryLosses::default()
            },
        );
        assert!(
            encode.contains("5 tiles across z0..z14")
                && encode.contains(
                    "4 tile feature(s) dropped at MVT encode (empty geometry or empty \
                     GeometryCollection)"
                ),
            "{encode}"
        );
    }

    /// #431 review: extent collapses are expected and never qualify the
    /// success line; they get their own note, naming what they are.
    #[test]
    fn encode_quantized_note_is_separate_and_silent_at_zero() {
        assert_eq!(encode_quantized_note(0), None);
        let note = encode_quantized_note(2).unwrap();
        assert!(
            note.contains("2 tile feature(s) collapsed at the tile extent")
                && note.contains("lines of fewer than two points")
                && !note.contains("dropped"),
            "{note}"
        );
    }

    /// #553: a bare count sent a real investigation to a separate DuckDB
    /// query against the input to find the offending rows. The summary line
    /// must name them itself.
    #[test]
    fn tiles_summary_line_names_out_of_range_exemplars() {
        use tylertoo_core::overview::convert::OutOfRangeExemplar;

        let exemplars = vec![
            OutOfRangeExemplar {
                part: None,
                row: 1041,
                axis: "lon",
                value: 180.548,
            },
            OutOfRangeExemplar {
                part: None,
                row: 2210,
                axis: "lon",
                value: 180.101,
            },
        ];
        let msg = tiles_summary_line(
            0,
            0,
            14,
            0.05,
            &SummaryLosses {
                out_of_range: 19,
                out_of_range_exemplars: &exemplars,
                ..SummaryLosses::default()
            },
        );
        assert!(
            msg.contains(
                "19 feature(s) dropped (outside the declared CRS range; e.g. lon \
                          180.548 (row 1041), lon 180.101 (row 2210))"
            ),
            "the summary must name the offending coordinates and rows, not just a count: {msg}"
        );
    }

    // --- #514 S3: the pyramid skip line ---------------------------------

    /// Nothing skipped, nothing printed.
    #[test]
    fn pyramid_skip_message_is_silent_when_nothing_was_skipped() {
        assert_eq!(pyramid_skip_message(0, true), None);
        assert_eq!(pyramid_skip_message(0, false), None);
    }

    /// The documented way to split one pre-tiled archive across several
    /// `--band` entries means the tiles one band drops are the ones its
    /// sibling keeps -- nothing vanished, so it gets a plain note instead of
    /// the "!" alarm.
    #[test]
    fn pyramid_skip_message_downgrades_the_subrange_split_case() {
        let msg = pyramid_skip_message(7, true).unwrap();
        assert!(!msg.contains('!'), "{msg}");
        assert!(msg.contains("7") && msg.contains("skipped"), "{msg}");
    }

    /// Anything that is not a genuine split keeps the "!" alarm: the tiles
    /// really are gone and no sibling band picked them up.
    #[test]
    fn pyramid_skip_message_keeps_the_alarm_without_a_split() {
        let msg = pyramid_skip_message(3, false).unwrap();
        assert!(msg.contains('!'), "{msg}");
        assert!(msg.contains("dropped"), "{msg}");
    }

    /// #527 review, finding 2: the reassuring line used to be keyed on
    /// `skipped == total_tiles`, a count coincidence. A single band that
    /// `--allow-missing-zooms` let overshoot hits it whenever it happens to
    /// keep as many tiles as it drops -- core's own
    /// `band_partial_overlap_allowed_with_flag_warns` is exactly that shape:
    /// a z3-6 archive under a z0-4 band keeps 2 and skips 2 -- and the two
    /// silently lost tiles were reported as an expected split. Equal counts
    /// alone must not downgrade the alarm.
    #[test]
    fn pyramid_skip_message_does_not_trust_equal_counts() {
        let msg = pyramid_skip_message(2, false).unwrap();
        assert!(
            msg.contains('!'),
            "kept == dropped is not evidence of a split: {msg}"
        );
    }

    /// The split signal is two `--band` entries naming the same archive,
    /// however each one spells the path. A band listed once is not a split,
    /// and neither is a GeoParquet source listed twice (it is tiled to its
    /// own range and never skips anything).
    #[test]
    fn bands_split_one_archive_detects_a_shared_archive() {
        use tylertoo_core::pyramid::Band;

        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("pre.pmtiles");
        // `classify_band_input` sniffs the magic; the rest is never read.
        std::fs::write(&archive, b"PMTiles\x03").unwrap();
        let source = dir.path().join("cells.parquet");
        std::fs::write(&source, b"PAR1").unwrap();

        let band = |lo: u8, hi: u8, p: PathBuf| Band {
            input: p,
            layer: "agg".to_string(),
            min_zoom: lo,
            max_zoom: hi,
        };

        assert!(!bands_split_one_archive(&[band(0, 13, archive.clone())]));
        assert!(bands_split_one_archive(&[
            band(0, 5, archive.clone()),
            band(6, 13, archive.clone()),
        ]));
        // Same file, different spelling of the path.
        let dotted = dir.path().join(".").join("pre.pmtiles");
        assert!(bands_split_one_archive(&[
            band(0, 5, archive.clone()),
            band(6, 13, dotted),
        ]));
        // A GeoParquet source twice is not an archive split.
        assert!(!bands_split_one_archive(&[
            band(0, 5, source.clone()),
            band(6, 13, source),
        ]));
    }

    /// #510 review, S3-7: `merge`'s "input is also the output" guard compared
    /// raw `PathBuf`s, so `./out.pmtiles` walked straight past it and the run
    /// overwrote the input it had just read. Spellings of one path must key
    /// the same.
    #[test]
    fn canonical_key_collapses_spellings_of_one_path() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.pmtiles");
        std::fs::write(&out, b"x").unwrap();

        let dotted = dir.path().join(".").join("out.pmtiles");
        let round_trip = dir.path().join("sub").join("..").join("out.pmtiles");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert_eq!(canonical_key(&out), canonical_key(&dotted));
        assert_eq!(canonical_key(&out), canonical_key(&round_trip));

        // A path that does not exist yet still keys by its canonical parent —
        // the output usually does not exist when the guard runs.
        let missing = dir.path().join("new.pmtiles");
        let missing_dotted = dir.path().join(".").join("new.pmtiles");
        assert_eq!(canonical_key(&missing), canonical_key(&missing_dotted));
        assert_ne!(canonical_key(&missing), canonical_key(&out));
    }

    /// #510 review, S3-6: a `--report` (or output) path in a directory that
    /// does not exist used to surface only after the merge had read every
    /// input. It is probed up front, like `--work-dir`.
    #[test]
    fn preflight_writable_rejects_a_missing_directory_and_keeps_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let err = preflight_writable(&dir.path().join("nope").join("report.json"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");

        // A writable directory passes and leaves nothing behind.
        let fresh = dir.path().join("report.json");
        preflight_writable(&fresh).unwrap();
        assert!(!fresh.exists(), "the probe must not leave a file behind");

        // An existing file is not touched — `--force` owns that decision.
        let existing = dir.path().join("out.pmtiles");
        std::fs::write(&existing, b"keep me").unwrap();
        preflight_writable(&existing).unwrap();
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");
    }
}
