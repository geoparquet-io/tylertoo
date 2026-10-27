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
use clap::{Args, CommandFactory, Parser, Subcommand};
use indicatif::{HumanBytes, HumanCount};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use tylertoo_core::overview::assign::SortDirection;
use tylertoo_core::overview::auto_zoom::MaxZoom;
use tylertoo_core::overview::config::{self, OutputKind};
use tylertoo_core::overview::convert::ConvertOptions;
use tylertoo_core::overview::export::{ExportOptions, FeatureOrder, SizeValve};
use tylertoo_core::overview::knobs::{ConvertKnobs, ExportKnobs, SizeCap};
use tylertoo_core::overview::level::MemoryProfile;
use tylertoo_core::overview::tiles::{TilesOptions, TILES_DEFAULT_MAX_ZOOM};
use tylertoo_core::overview::writer::RowGroupSizePolicy;

/// `-q/--quiet` (#452), set once in `main` before dispatch.
static QUIET: AtomicBool = AtomicBool::new(false);

/// `println!` for the status and summary lines of a subcommand that writes
/// a file, which `--quiet` silences. What `stats` and `validate` print is
/// their result, so they keep `println!`.
macro_rules! say {
    ($($arg:tt)*) => {
        if !QUIET.load(Ordering::Relaxed) {
            println!($($arg)*);
        }
    };
}

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

/// Parse `--max-tile-size` / `--max-compressed-tile-size`: `auto` (the
/// command's core default) or a size, where 0 is no cap (#280, #761).
fn parse_size_cap(s: &str) -> Result<SizeCap, String> {
    if s.trim().eq_ignore_ascii_case("auto") {
        return Ok(SizeCap::Auto);
    }
    parse_size_bytes(s).map(SizeCap::Bytes)
}

/// A flag whose value is one of `values`, parsed into core's type: clap
/// lists the values in `--help`, core's `FromStr` owns the meaning.
fn spelled<T>(values: &'static [&'static str]) -> impl clap::builder::TypedValueParser<Value = T>
where
    T: std::str::FromStr + Clone + Send + Sync + 'static,
    T::Err: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    clap::builder::TypedValueParser::try_map(
        clap::builder::PossibleValuesParser::new(values.iter().copied()),
        |s| s.parse::<T>(),
    )
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
    // xmin <= xmax and ymin <= ymax is core's rule (`ConvertOptions::validate`).
    Ok([vals[0], vals[1], vals[2], vals[3]])
}

/// Exit-status contract (#452), shown under `tylertoo --help` and in the
/// generated CLI reference. Exit 1 is `main` returning `Err` (anyhow
/// prints it); exit 2 is clap's for a parse error, which core's option refusals
/// (`is_usage_error`) and `--quiet` with `tiles --verbose` reuse.
const EXIT_STATUS_HELP: &str = "\
Exit status:

- 0: success.
- 1: the run failed, with the error on stderr. `validate` also exits 1 when a check fails.
- 2: the command line was rejected: while it was parsed, or by an option or output-path rule checked before any input is read.
- 101: a panic, from an internal bug or an output pipe closed early.";

/// Convert GeoParquet to PMTiles vector tiles and multi-resolution
/// overviews.
///
/// `tylertoo INPUT OUTPUT` with no subcommand runs `tiles`.
#[derive(Parser, Debug)]
#[command(
    name = "tylertoo",
    about = "Convert GeoParquet to PMTiles vector tiles and multi-resolution overviews",
    version,
    after_help = EXIT_STATUS_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    // DIVERGENCE FROM TIPPECANOE: its `-q` also silences warnings ("Work
    // quietly instead of reporting progress or warning messages"). Ours
    // keeps them: several report data loss that a quiet run should still
    // show. See context/ARCHITECTURE.md.
    /// Print only warnings and errors. The output of `stats` and `validate`
    /// still prints.
    #[arg(short, long, global = true)]
    quiet: bool,
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

/// Top-level publication metadata for a PMTiles archive.
///
/// `--layer-name` names the MVT layer; these describe the tileset. Each is
/// stored exactly as given, and an empty string is kept as one.
#[derive(Args, Debug, Clone, Default)]
pub struct TilesetMetadataArgs {
    /// Human-readable name stored in the archive metadata.
    #[arg(long, value_name = "TEXT")]
    pub name: Option<String>,

    /// Description stored in the archive metadata.
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,

    /// Attribution text or HTML stored in the archive metadata.
    #[arg(long, value_name = "TEXT")]
    pub attribution: Option<String>,
}

impl TilesetMetadataArgs {
    /// The core value these flags describe.
    fn to_metadata(&self) -> tylertoo_core::TilesetMetadata {
        let mut meta = tylertoo_core::TilesetMetadata::new();
        meta.name.clone_from(&self.name);
        meta.description.clone_from(&self.description);
        meta.attribution.clone_from(&self.attribution);
        meta
    }
}

/// The export knobs every command that writes tiles shares (#779), flattened
/// into `tiles`, `export-pmtiles` and `pyramid` the way [`ConvertTuningArgs`]
/// is into `tiles` and `overview`. They are core's [`ExportKnobs`], which
/// lays them over the command's core defaults; validation is core's.
#[derive(Args, Debug, Clone)]
struct ExportArgs {
    /// Per-tile cap on the raw MVT size, such as `2M`, or 0 for no cap: a
    /// backstop behind `--max-compressed-tile-size`. `auto` is 2M, or no cap
    /// with `tiles --verbatim` and for `pyramid` bands. A tile over a cap is
    /// merged or thinned until it fits; see `--size-valve`.
    #[arg(
        long,
        value_name = "SIZE|auto",
        visible_alias = "tile-size-limit",
        default_value = "auto",
        value_parser = parse_size_cap
    )]
    max_tile_size: SizeCap,

    /// Per-tile cap on the gzip-compressed tile size, like tippecanoe's
    /// `-M`, such as `500K`, or 0 for no cap. `auto` is 500K, or no cap with
    /// `tiles --verbatim` and for `pyramid` bands. A tile must fit both caps.
    #[arg(
        long,
        value_name = "SIZE|auto",
        default_value = "auto",
        value_parser = parse_size_cap
    )]
    max_compressed_tile_size: SizeCap,

    /// How to reduce a tile that exceeds its cap: `merge` merges the
    /// densest lines and polygons into a neighbor, which keeps its own
    /// attributes; `drop` removes features, leaving holes while preserving
    /// each retained feature's attributes. Point tiles use feature dropping
    /// in both modes.
    #[arg(
        long,
        value_name = "merge|drop",
        default_value_t = ExportOptions::default().size_valve
    )]
    size_valve: SizeValve,

    /// Fill the holes that dense small polygons (parcels, fields, small
    /// admin units) leave at coarse zooms: under each tile's features, also
    /// draw the area its child tiles cover, with the attributes of one feature
    /// it sits under, in the room the tile's own features leave. Adds 10–17%
    /// to build time on dense coverages, about 1.5x on scattered sub-pixel
    /// fields (which it may also over-draw), and holds the finest level's
    /// geometry in memory. A sharded build's coarse job draws it; data
    /// shards (`--tile-range`) do not.
    #[arg(long)]
    coverage_fill: bool,

    /// Clip all polygons with the full overlay for byte-stable tiles. The
    /// simple-ring fast path renders identically but can change a ring's
    /// starting vertex.
    #[arg(long)]
    no_simple_clip_fastpath: bool,

    /// Edge buffer around each tile, in tile pixels, so features continue
    /// across tile seams. At most 256 (one tile width).
    #[arg(long, default_value_t = ExportOptions::default().tile_buffer)]
    tile_buffer: u32,

    /// MVT tile extent: the tile-local coordinate resolution. A power of two
    /// is recommended.
    #[arg(long, default_value_t = ExportOptions::default().extent)]
    extent: u32,

    /// Partitions to export at once, or `auto` to size the wave to the cores
    /// and free memory. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#export-waves>.
    #[arg(long, value_name = "N|auto", default_value = "auto", value_parser = parse_partition_wave)]
    partition_wave: usize,

    /// Feature order within each tile: `input` preserves source row order;
    /// `COLUMN[:asc|:desc]` sorts by a property. Most renderers paint in this
    /// order unless the style overrides it. A `pyramid` band without that
    /// column keeps input order with a warning.
    #[arg(
        long,
        value_name = "input|COLUMN[:asc|:desc]",
        default_value_t = ExportOptions::default().feature_order
    )]
    feature_order: FeatureOrder,

    /// Use this integer column as the MVT feature id for `setFeatureState`
    /// across tiles and zooms. Every row must contain 0 to 2^64-1.
    /// Defaults to tile-local ids.
    #[arg(long, value_name = "COLUMN")]
    feature_id: Option<String>,
}

impl From<&ExportArgs> for ExportKnobs {
    /// The flags as core's knobs: the one place the CLI spells them.
    fn from(args: &ExportArgs) -> Self {
        ExportKnobs {
            tile_size_limit: args.max_tile_size,
            max_compressed_tile_size: args.max_compressed_tile_size,
            size_valve: args.size_valve,
            coverage_fill: args.coverage_fill,
            simple_clip_fastpath: !args.no_simple_clip_fastpath,
            tile_buffer: args.tile_buffer,
            extent: args.extent,
            partition_wave: args.partition_wave,
            feature_order: args.feature_order.clone(),
            feature_id: args.feature_id.clone(),
        }
    }
}

impl ExportArgs {
    /// These flags over `export`, the command's core defaults
    /// ([`ExportKnobs::apply`]).
    fn apply(&self, export: ExportOptions) -> ExportOptions {
        ExportKnobs::from(self).apply(export)
    }
}

/// The export-side property selection (#386): which properties the tiles
/// carry. `tiles` applies its selection at convert instead
/// ([`ConvertTuningArgs`]).
#[derive(Args, Debug, Clone, Default)]
struct ExportPropertyArgs {
    /// Keep only these properties in the tiles (repeatable). The input
    /// keeps every column, and the `--feature-order` column must stay.
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
}

impl ExportPropertyArgs {
    fn selection(&self) -> tylertoo_core::overview::properties::PropertySelection {
        property_selection(
            &self.include_property,
            &self.exclude_property,
            self.exclude_all_properties,
        )
    }
}

/// The include/exclude choice (#386) as core's selection: an empty include
/// list is no include list on the command line.
fn property_selection(
    include: &[String],
    exclude: &[String],
    exclude_all: bool,
) -> tylertoo_core::overview::properties::PropertySelection {
    tylertoo_core::overview::properties::PropertySelection {
        include: (!include.is_empty()).then(|| include.to_vec()),
        exclude: exclude.to_vec(),
        exclude_all,
    }
}

/// Print a composed archive's publication metadata (`merge`, `pyramid`).
///
/// The values written, since an inherited attribution is HTML the map will
/// render and should be visible without re-reading the archive. Marked `!`
/// like the bounds-less inputs: each field left out because the `inputs`
/// disagreed, and the inputs whose metadata did not decode, any of which
/// may have been meant to carry a credit.
fn print_publication(
    meta: &tylertoo_core::TilesetMetadata,
    conflicts: &[String],
    without_metadata: &[String],
    inputs: &str,
) {
    for (key, value) in [
        ("name", &meta.name),
        ("description", &meta.description),
        ("attribution", &meta.attribution),
    ] {
        if let Some(value) = value {
            say!("  {key}: {}", quoted_preview(value));
        }
    }
    for key in conflicts {
        say!("  ! the {inputs} carry different {key}s, so the output has none; set --{key} to give it one");
    }
    if !without_metadata.is_empty() {
        say!(
            "  ! {} of the {inputs} carried no readable metadata, so passed on no name, \
             description or attribution:",
            without_metadata.len()
        );
        for name in without_metadata {
            say!("      {name}");
        }
    }
}

/// `value` quoted and escaped for one output line, cut after 200 characters.
fn quoted_preview(value: &str) -> String {
    match value.char_indices().nth(200) {
        None => format!("{value:?}"),
        Some((cut, _)) => format!("{:?}… ({} bytes)", &value[..cut], value.len()),
    }
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

    /// Publication metadata for the output. An unset field is inherited from
    /// the band archives.
    #[command(flatten)]
    pub publication: TilesetMetadataArgs,

    /// Generalize each GeoParquet band instead of tiling it verbatim. Use
    /// for raw features spanning several zooms.
    #[arg(long)]
    pub generalize: bool,

    #[command(flatten)]
    export: ExportArgs,

    #[command(flatten)]
    properties: ExportPropertyArgs,

    /// Directory for temporary per-band files, deleted after the build.
    /// Defaults to the system temp directory.
    #[arg(long, value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    /// Accept a pre-tiled band with fewer zooms than declared. Missing
    /// zooms render empty; use only for a sparse pyramid.
    #[arg(long)]
    pub allow_missing_zooms: bool,

    /// Write the JSON pyramid report, with per-band tile counts, to this
    /// path.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

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

    /// Publication metadata for the output. An unset field is inherited from
    /// the inputs.
    #[command(flatten)]
    pub publication: TilesetMetadataArgs,

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
Decoding reconstructs tile features. A round trip from `A.parquet` to
`B.pmtiles` to `C.parquet` changes the data:

- Tiling simplifies vertices at lower zooms. Decode the maximum zoom
  for the most detail.
- Features are clipped at buffered tile edges.
- Features repeat for each tile they touch and each zoom. Filter with
  `--zoom` or the `zoom` column.
- Properties dropped during tiling cannot be recovered.

Output columns, in order:

1. `zoom` (UInt8), `layer` (Utf8), and `mvt_id` (UInt64; null if the
   encoder set no id) identify each row's origin.
2. All properties found in any tile, alphabetically ordered and null
   where absent. Integers become Int64; floats become Float64. Mixed
   integers and floats become Float64; other mixed types become Utf8.
3. `geometry`: Well-Known Binary (WKB) in lon/lat (`EPSG:4326`), with a
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

    /// Number of data shards. Run one `tiles --shard i/N` job per
    /// shard plus one `--shard coarse` job, then merge all N+1 archives.
    #[arg(long, value_name = "N")]
    pub shards: usize,

    /// Pivot zoom: data shards cover this zoom through `--max-zoom`; the
    /// coarse job covers lower zooms. Choose a zoom with a few populated
    /// tiles per shard, usually z4 to z8.
    #[arg(long, value_name = "ZOOM", default_value = "6")]
    pub pivot: u8,

    /// Overwrite an existing plan at `--output`.
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
    #[arg(long, default_value_t = ExportOptions::default().layer_name)]
    layer_name: String,

    #[command(flatten)]
    publication: TilesetMetadataArgs,

    /// Minimum zoom in archive metadata, including empty coarse overview
    /// levels. Defaults to the coarsest level's zoom.
    #[arg(long, value_name = "ZOOM")]
    min_zoom: Option<u8>,

    #[command(flatten)]
    properties: ExportPropertyArgs,

    #[command(flatten)]
    export: ExportArgs,

    /// Write the JSON export report, with per-zoom tile and feature counts,
    /// to this path.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

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

    /// Read `.parquet` files from this manifest instead of `INPUT`: one
    /// path or URL per line, in dataset row order. Ignores blank lines and
    /// lines starting with `#`.
    #[arg(long, value_name = "PATH")]
    files_from: Option<PathBuf>,

    /// Level layout: `duplicating` writes each level in full,
    /// and `partitioning` writes each feature once, at its coarsest level.
    #[arg(long, default_value = ConvertOptions::default().mode.as_str(), value_parser = ["duplicating", "partitioning"])]
    mode: String,

    /// Minimum (coarsest) Web Mercator zoom.
    #[arg(long, default_value_t = default_zooms(&ConvertOptions::default()).0)]
    min_zoom: u8,

    /// Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a
    /// sample of the input. Ignored with `--gsd`.
    #[arg(long, default_value_t = MaxZoom::Fixed(default_zooms(&ConvertOptions::default()).1))]
    max_zoom: MaxZoom,

    /// Comma-separated ground sample distances (GSDs) in meters, each
    /// smaller than the last. Overrides `--min-zoom` and `--max-zoom`.
    #[arg(long, value_name = "GSDS")]
    gsd: Option<String>,

    /// Convert features whose bbox intersects this lon/lat box, or would
    /// one ±360° wrap away (0..360 data). Skips row groups outside the box.
    #[arg(long, value_name = "XMIN,YMIN,XMAX,YMAX")]
    bbox: Option<String>,

    /// Write the third-party `cogp` footer key for readers of that overview
    /// format. Partitioning mode only.
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
    /// Disable thinning, simplification, and density reduction for
    /// pre-aggregated or pre-levelled input. Explicit tuning flags override
    /// these defaults.
    #[arg(long, help_heading = "Thinning & visibility")]
    verbatim: bool,

    /// Numeric column used to choose each thinning cell's winning feature.
    /// Conflicts with `--class-rank`.
    #[arg(long, value_name = "COL", help_heading = "Ranking")]
    sort_key: Option<String>,

    /// Whether the larger (`desc`) or the smaller (`asc`) `--sort-key`
    /// value wins a cell.
    #[arg(
        long,
        default_value = ConvertOptions::default().assign.sort_direction.as_str(),
        value_parser = spelled::<SortDirection>(&["desc", "asc"]),
        help_heading = "Ranking"
    )]
    sort_direction: SortDirection,

    /// Rank `COL` values from highest to lowest, assigning entry zooms
    /// from `--min-zoom` in `--ladder-step` increments. Features appear at
    /// their entry zoom and finer zooms, exempt from thinning.
    #[arg(long, value_name = "COL", help_heading = "Ranking")]
    magnitude_ladder: Option<String>,

    /// Zooms between consecutive `--magnitude-ladder` rungs.
    #[arg(
        long,
        value_name = "N",
        default_value_t = config::DEFAULT_LADDER_STEP,
        help_heading = "Ranking",
        requires = "magnitude_ladder"
    )]
    ladder_step: u8,

    /// Assign entry zooms: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...`, for example
    /// `density:5000=4,1000=6`. Unlisted values use the normal visibility gate.
    // Exclusive with --magnitude-ladder: core's rule
    // (`config::entry_zoom`), not clap's, so Python refuses it alike (#779).
    #[arg(long, value_name = "SPEC", help_heading = "Ranking")]
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

    /// Filter property columns with a SQL `WHERE` predicate, such as
    /// `confidence > 0.8`. Skips row groups that cannot match. Grammar:
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
        default_value_t = ConvertOptions::default().gsd_base,
        help_heading = "Generalization"
    )]
    gsd_base: f64,

    /// Simplification tolerance in multiples of each level's GSD (default
    /// 1.0, duplicating mode only). Lower values keep more vertices.
    #[arg(long, help_heading = "Generalization")]
    simplify_factor: Option<f64>,

    /// Replace polygons too small for a level with a point instead of the
    /// default placeholder square. Fill styles ignore points; add a circle
    /// layer.
    // The collapse trio's exclusivity is core's (`config::collapse_mode`).
    #[arg(long, help_heading = "Generalization")]
    collapse: bool,

    /// Replace polygons too small for a level, and those thinned out at
    /// coarse levels, with small placeholder squares marking their area.
    /// This is the default in duplicating mode; output remains Polygon.
    #[arg(long, help_heading = "Generalization")]
    collapse_square: bool,

    /// Drop polygons too small for a level instead of replacing them with
    /// placeholder squares: smaller files, but a polygon coverage shows holes
    /// at coarse zooms (the default before #745).
    #[arg(long, help_heading = "Generalization")]
    no_collapse: bool,

    /// Polygon representation by zoom band: comma-separated
    /// `LO-HI:KIND`, where `KIND` is `geom`, `point`, or `square`, such as
    /// `0-7:point,8-14:geom`. Band rules:
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
    /// (default 0, off). Larger cells keep fewer polygons and can remove a
    /// polygon whose center falls in a larger neighbor's cell.
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
    /// finer level's feature budget. Larger values thin mid zooms more.
    #[arg(
        long,
        value_name = "F",
        default_value_t = ConvertOptions::default().density.drop_rate,
        help_heading = "Density budget"
    )]
    drop_rate: f64,

    /// Sparse-area protection: 1 reduces every neighborhood equally;
    /// larger values protect sparse areas more.
    #[arg(
        long,
        value_name = "F",
        default_value_t = ConvertOptions::default().density.gamma,
        help_heading = "Density budget"
    )]
    drop_gamma: f64,

    /// Turn off the per-level density budget, leaving cell-winner thinning
    /// only.
    #[arg(long, help_heading = "Density budget")]
    no_density_drop: bool,

    /// Apply the density budget to lines as well as points: smaller, faster
    /// tiles that drop line features at coarse zooms (tippecanoe's
    /// `--drop-lines`).
    #[arg(long, help_heading = "Density budget")]
    drop_lines: bool,

    /// Apply the density budget to polygons as well as points. Removing a
    /// polygon leaves a hole in a coverage such as admin areas or parcels,
    /// so the budget excludes polygons by default (tippecanoe's
    /// `--drop-polygons`).
    #[arg(long, help_heading = "Density budget")]
    drop_polygons: bool,

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
        default_value_t = ConvertOptions::default().coalesce_junction_angle,
        help_heading = "Line coalescing"
    )]
    coalesce_junction_angle: f64,

    /// Join line ends within this many GSDs of each other when coalescing.
    /// Set 0 to join only ends that match exactly.
    #[arg(
        long,
        value_name = "F",
        default_value_t = ConvertOptions::default().coalesce_snap,
        help_heading = "Line coalescing"
    )]
    coalesce_snap: f64,

    /// Skip coalescing above this candidate-line count to bound memory.
    /// Long lines reach a corresponding geometry-size limit first.
    #[arg(
        long,
        value_name = "ROWS",
        default_value_t = ConvertOptions::default().coalesce_max_level_rows,
        help_heading = "Line coalescing"
    )]
    coalesce_max_level_rows: usize,

    /// Maximum rows per output row group at each level. The cap increases
    /// if a level would exceed Parquet's row-group limit.
    #[arg(
        long,
        default_value_t = ConvertOptions::default().max_row_group_size,
        help_heading = "Output layout"
    )]
    row_group_size: usize,

    /// How the row-group cap varies by level: `constant`, or `zoom-scaled`,
    /// which doubles it per zoom step coarser than the finest level.
    #[arg(
        long,
        default_value = ConvertOptions::default().row_group_size_policy.as_str(),
        value_parser = spelled::<RowGroupSizePolicy>(&["constant", "zoom-scaled"]),
        help_heading = "Output layout"
    )]
    row_group_size_policy: RowGroupSizePolicy,

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
        default_value_t = ConvertOptions::default().read_batch_size,
        value_parser = parse_read_batch_size,
        help_heading = "Memory & performance"
    )]
    read_batch_size: usize,

    /// Memory profile for writing levels: `speed` buffers in RAM, `bounded`
    /// spills to disk, and `auto` picks per run. See
    /// <https://geoparquet-io.github.io/tylertoo/guides/scaling/#memory-profiles>.
    #[arg(
        long,
        default_value = ConvertOptions::default().profile.as_str(),
        value_parser = spelled::<MemoryProfile>(&["auto", "speed", "bounded"]),
        help_heading = "Memory & performance"
    )]
    profile: MemoryProfile,

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

    /// Reader threads for both passes, or `auto` for a quarter of the
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

    /// Save the convert plan to PATH and continue converting. Reuse it
    /// with `--plan`; sharded builds share one plan.
    // Exclusive with --plan: core's rule (`ConfigError::SavePlanWithPlan`).
    #[arg(long, value_name = "PATH", help_heading = "Memory & performance")]
    save_plan: Option<PathBuf>,

    /// Reuse a convert plan, skipping the first pass and level assignment.
    /// Its fingerprint must match this run's version, flags, and inputs.
    #[arg(long, value_name = "PATH", help_heading = "Memory & performance")]
    plan: Option<PathBuf>,
}

impl ConvertTuningArgs {
    /// `--save-plan` and `--plan`, for [`preflight_report`].
    fn plan_paths(&self) -> impl Iterator<Item = &Path> {
        self.save_plan
            .as_deref()
            .into_iter()
            .chain(self.plan.as_deref())
    }

    /// Build [`ConvertOptions`] from the shared tuning flags through core's
    /// [`ConvertKnobs`] (#779). The parent command owns `mode`, the `levels`
    /// plan, `bbox`, and `cogp_compat` and passes them in. Every rule is
    /// core's; the errors here are core's, surfaced as they are.
    fn build_convert_options(
        &self,
        mode: tylertoo_core::overview::level::Mode,
        levels: tylertoo_core::overview::convert::LevelPlan,
        bbox: Option<[f64; 4]>,
        cogp_compat: bool,
    ) -> Result<ConvertOptions> {
        Ok(ConvertKnobs::from(self).convert_options(mode, levels, bbox, cogp_compat)?)
    }
}

impl From<&ConvertTuningArgs> for ConvertKnobs {
    /// The flags as core's knobs: the one place the CLI spells them.
    fn from(args: &ConvertTuningArgs) -> Self {
        ConvertKnobs {
            sort_key: args.sort_key.clone(),
            sort_direction: args.sort_direction,
            magnitude_ladder: args.magnitude_ladder.clone(),
            ladder_step: args.ladder_step,
            entry_zoom: args.entry_zoom.clone(),
            class_rank: args.class_rank.clone(),
            // Python's structured spelling of `--class-rank`.
            class_rank_column: None,
            class_ranks: None,
            class_rank_unknown: None,
            no_auto_rank: args.no_auto_rank,
            verbatim: args.verbatim,
            simplify_factor: args.simplify_factor,
            collapse: args.collapse,
            collapse_square: args.collapse_square,
            no_collapse: args.no_collapse,
            representation: args.representation.clone(),
            cascade: !args.no_cascade,
            point_thinning: args.point_thinning,
            line_thinning: args.line_thinning,
            polygon_thinning: args.polygon_thinning,
            line_visibility: args.line_visibility,
            polygon_visibility: args.polygon_visibility,
            drop_rate: args.drop_rate,
            drop_gamma: args.drop_gamma,
            // No positive spelling exists for density drop, so `--verbatim`
            // cannot be overriding an explicit request; `--coalesce-lines`
            // is the deprecated positive spelling of the default.
            density_drop: args.no_density_drop.then_some(false),
            drop_lines: args.drop_lines,
            drop_polygons: args.drop_polygons,
            cluster: args.cluster,
            accumulate_attributes: args.accumulate_attribute.clone(),
            coalesce_lines: if args.no_coalesce_lines {
                Some(false)
            } else {
                args.coalesce_lines.then_some(true)
            },
            coalesce_snap: args.coalesce_snap,
            coalesce_junction_angle: args.coalesce_junction_angle,
            coalesce_max_level_rows: args.coalesce_max_level_rows,
            // An empty `--include-property` list is no list at all.
            include_properties: (!args.include_property.is_empty())
                .then(|| args.include_property.clone()),
            exclude_properties: Some(args.exclude_property.clone()),
            exclude_all_properties: args.exclude_all_properties,
            gsd_base: args.gsd_base,
            row_group_size: args.row_group_size,
            row_group_size_policy: args.row_group_size_policy,
            full_column_stats: args.full_column_stats,
            streaming: !args.no_streaming,
            read_batch_size: args.read_batch_size,
            filter: args.filter.clone(),
            profile: args.profile,
            in_flight_batches: args.in_flight_batches,
            read_workers: args.read_workers,
            spill_dir: args.spill_dir.clone(),
            save_plan: args.save_plan.clone(),
            plan: args.plan.clone(),
        }
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

    /// Number of largest tiles to list, ranked by stored size.
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

    /// Read `.parquet` files from this manifest instead of `INPUT`: one
    /// path or URL per line, in dataset row order. Ignores blank lines and
    /// lines starting with `#`.
    #[arg(long, value_name = "PATH")]
    files_from: Option<PathBuf>,

    /// Minimum (coarsest) Web Mercator zoom.
    #[arg(long, default_value_t = default_zooms(&TilesOptions::default().convert).0)]
    min_zoom: u8,

    /// Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a
    /// sample of the input. Ignored with `--gsd`.
    #[arg(long, default_value_t = MaxZoom::Fixed(TILES_DEFAULT_MAX_ZOOM))]
    max_zoom: MaxZoom,

    /// Comma-separated ground sample distances (GSDs) in meters, each
    /// smaller than the last. Overrides `--min-zoom` and `--max-zoom`.
    #[arg(long, value_name = "GSDS")]
    gsd: Option<String>,

    /// Convert features whose bbox intersects this lon/lat box, or would
    /// one ±360° wrap away (0..360 data). Skips row groups outside the box.
    #[arg(long, value_name = "XMIN,YMIN,XMAX,YMAX")]
    bbox: Option<String>,

    /// Layer name for the tiles. Defaults to the input's file stem.
    #[arg(long)]
    layer_name: Option<String>,

    #[command(flatten)]
    publication: TilesetMetadataArgs,

    #[command(flatten)]
    export: ExportArgs,

    /// Write a JSON report with `convert` and `export` sections, matching
    /// the reports of `overview` and `export-pmtiles`.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

    /// Save the intermediate overview GeoParquet to PATH after export.
    /// Does not change the PMTiles output.
    #[arg(long, value_name = "PATH")]
    keep_overview: Option<PathBuf>,

    /// Build one shard: `I/N` for data shard I of N, or
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
    // Exclusive with --shard: core's rule (`ConfigError::TileRangeWithShard`).
    #[arg(long, value_name = "LO..HI", help_heading = "Sharded builds")]
    tile_range: Option<String>,

    /// Write the convert plan (`--save-plan`) and stop, with no export and
    /// no OUTPUT. Use it for a coarse job whose tiles the fleet discards.
    // Needs --save-plan: core's rule (`ConfigError::PlanOnlyWithoutSavePlan`).
    #[arg(long, help_heading = "Sharded builds")]
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

fn main() -> Result<std::process::ExitCode> {
    // Initialize dhat profiler if feature is enabled
    // This must be at the very start of main() - the profiler outputs
    // dhat-heap.json on Drop (program exit). TYLERTOO_DHAT=off skips it:
    // CI's `--all-features` test runs compile this in, and the CLI snapshot
    // tests (tests/cli_snapshots.rs) pin stderr, which dhat's exit summary
    // would change.
    #[cfg(feature = "dhat-heap")]
    let _profiler = (std::env::var_os("TYLERTOO_DHAT").as_deref()
        != Some(std::ffi::OsStr::new("off")))
    .then(dhat::Profiler::new_heap);

    // Backward-compatible bare invocation: `tylertoo input.parquet out.pmtiles`
    // is rewritten to `tylertoo tiles input.parquet out.pmtiles` when the first
    // positional token is not a known subcommand (and not --help/--version).
    let cli = Cli::parse_from(rewrite_bare_args(std::env::args_os()));

    // One logger for every subcommand: INFO by default, RUST_LOG overrides,
    // and `--quiet` caps whatever that gave at WARN.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if cli.quiet {
        QUIET.store(true, Ordering::Relaxed);
        log::set_max_level(log::max_level().min(log::LevelFilter::Warn));
    }

    let result = match cli.command {
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
    };
    match result {
        Ok(()) => Ok(std::process::ExitCode::SUCCESS),
        // #779: an option core refused before any work is a usage error,
        // like clap's: exit 2, the message alone (it names no failed step).
        Err(e) if is_usage_error(&e) => {
            eprintln!("Error: {e:#}");
            Ok(std::process::ExitCode::from(2))
        }
        Err(e) => Err(e),
    }
}

/// Whether `e` is one of core's option refusals (#779), wherever it sits in
/// the chain: a [`config::ConfigError`], or an error its type classes as
/// config (`is_config`). These exit 2, the code clap gives its own usage
/// errors, so a script can tell "fix the command" from "the run failed".
fn is_usage_error(e: &anyhow::Error) -> bool {
    use tylertoo_core::overview::convert::ConvertError;
    use tylertoo_core::overview::export::ExportError;
    use tylertoo_core::overview::tiles::TilesError;
    e.chain().any(|c| {
        c.is::<config::ConfigError>()
            || c.downcast_ref::<ConvertError>()
                .is_some_and(ConvertError::is_config)
            || c.downcast_ref::<ExportError>()
                .is_some_and(ExportError::is_config)
            || c.downcast_ref::<TilesError>()
                .is_some_and(TilesError::is_config)
            || c.downcast_ref::<tylertoo_core::Error>()
                .is_some_and(tylertoo_core::Error::is_config)
    })
}

/// `what failed: e`, or `e` alone (keeping its type, for
/// [`is_usage_error`]) when `e` is an option refused before any work.
fn step_failed<E>(what: &str, e: E, is_config: fn(&E) -> bool) -> anyhow::Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    if is_config(&e) {
        anyhow::Error::new(e)
    } else {
        anyhow::anyhow!("{what} failed: {e}")
    }
}

#[cfg(feature = "gen-docs")]
mod reference_docs;

/// Render the CLI reference from the clap help strings and option definitions.
#[cfg(feature = "gen-docs")]
fn gen_reference_markdown() -> String {
    let options = clap_markdown::MarkdownOptions::new()
        .title("CLI reference".to_string())
        .show_footer(false)
        .show_table_of_contents(false);
    let body = reference_docs::format(clap_markdown::help_markdown_custom::<Cli>(&options));
    format!(
        "<!-- GENERATED FILE — do not edit by hand.\n     \
         Regenerate: cargo run -p tylertoo --features gen-docs -- \
         gen-reference-docs > docs/reference/cli.md\n     \
         CI fails if this file drifts from the clap definitions. -->\n\n{body}"
    )
}

/// Insert an implicit `tiles` subcommand for the backward-compatible bare form.
///
/// If the first token after the global flags is already a subcommand (any
/// of clap's, or `help`) or the invocation is a help/version query, the
/// arguments are returned unchanged.
fn rewrite_bare_args<I>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    // The subcommands come from clap itself, so a new one cannot be missed.
    // `help` is clap's built-in. `gen-reference-docs` is reserved even when
    // the `gen-docs` feature compiles it out, so the bare-form rewrite never
    // prepends `tiles` to it: clap then rejects it as unknown (correct).
    let is_subcommand_name = |name: &str| {
        matches!(name, "help" | "gen-reference-docs")
            || Cli::command()
                .get_subcommands()
                .any(|c| c.get_name() == name)
    };
    let argv: Vec<std::ffi::OsString> = args.into_iter().collect();

    // Nothing to rewrite for a bare `tylertoo` (clap prints help/usage).
    if argv.len() <= 1 {
        return argv;
    }

    // A subcommand can only follow the global flags, none of which takes a
    // value. Looking past any other flag for "the first positional" would
    // take a flag's value for one: `tylertoo --name overview in out` named
    // the tileset `overview` and was routed to the `overview` subcommand.
    let first_after_globals = argv
        .iter()
        .skip(1)
        .find(|a| !matches!(a.to_string_lossy().as_ref(), "-q" | "--quiet"));
    let is_subcommand = first_after_globals
        .map(|a| is_subcommand_name(a.to_string_lossy().as_ref()))
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

    /// The input path, or the manifest's.
    fn path(&self) -> &Path {
        match self {
            InputSpec::Path(p) | InputSpec::Manifest(p) => p,
        }
    }

    /// Label for summary lines (input file name or manifest file name).
    fn label(&self) -> String {
        self.path()
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into()
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
///
/// The dispatch itself is core's ([`TilesInput::convert`], #439) — the
/// `tiles` facade routes both shapes the same way, and two copies of that
/// would drift. This only adds `overview`'s `anyhow` context.
///
/// [`TilesInput::convert`]: tylertoo_core::overview::tiles::TilesInput::convert
fn run_convert(
    spec: &InputSpec,
    output: &std::path::Path,
    options: &tylertoo_core::overview::convert::ConvertOptions,
) -> Result<tylertoo_core::overview::convert::ConvertReport> {
    tiles_input(spec).convert(output, options).map_err(|e| {
        step_failed(
            "overview conversion",
            e,
            tylertoo_core::overview::convert::ConvertError::is_config,
        )
    })
}

/// The core-side view of a resolved input shape.
fn tiles_input(spec: &InputSpec) -> tylertoo_core::overview::tiles::TilesInput {
    use tylertoo_core::overview::tiles::TilesInput;
    match spec {
        InputSpec::Path(p) => TilesInput::Path(p.clone()),
        InputSpec::Manifest(m) => TilesInput::Manifest(m.clone()),
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
    // `None`: the caller ran `config::check_output` on the output first.
    Ok(max_zoom.resolve(&source, options, None)?)
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

impl ShardJob {
    /// The core-side view of this job: everything the facade needs, without
    /// the bound plan the CLI keeps for its own `--max-zoom auto` re-check.
    /// Core owns every consequence of it (#439).
    fn as_tiles_shard(&self) -> tylertoo_core::overview::tiles::TilesShard {
        tylertoo_core::overview::tiles::TilesShard {
            role: self.role,
            range: self.range,
            pivot_zoom: self.pivot,
            cut_digest: self.cut_digest.clone(),
        }
    }
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
    max_zoom: u8,
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
    // The coarse job's own half (a pivot at or below --min-zoom leaves it
    // nothing to build, unless --plan-only) is core's rule
    // (`ConfigError::CoarseShardHasNoZoom`, #779), checked by the
    // `TilesOptions::validate` that runs right after this.
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

/// The `(min_zoom, max_zoom)` of a core default's zoom plan, for the clap
/// defaults (#779: one copy of each default, in core).
fn default_zooms(options: &ConvertOptions) -> (u8, u8) {
    match options.levels {
        tylertoo_core::overview::convert::LevelPlan::ZoomRange { min_zoom, max_zoom } => {
            (min_zoom, max_zoom)
        }
        tylertoo_core::overview::convert::LevelPlan::Gsds(_) => {
            unreachable!("a core default is a zoom range")
        }
    }
}

/// The [`TilesOptions`] a `tiles` command line describes: pure translation
/// (#779), with `--max-zoom auto`'s placeholder and no shard job yet —
/// [`resolve_tiles_job`] adds both, and reads the input to do it.
///
/// Shared by the full facade and `--plan-only` (#560). The convert plan is
/// fingerprinted over these options, so the plan a plan-only run writes is
/// byte-identical to a full run's only if both build the options the same
/// way — guaranteed here by construction rather than by review.
fn tiles_options(args: &TilesArgs) -> Result<TilesOptions> {
    use tylertoo_core::overview::level::Mode;

    // #444: until `--max-zoom auto` is resolved — after every check that
    // does not need its value, so a typo or a stale shard plan still fails
    // in milliseconds (#371) — the plan carries `auto`'s placeholder.
    let levels = resolve_level_plan(
        args.gsd.as_deref(),
        args.min_zoom,
        args.max_zoom.plan_zoom(),
    )?;
    let bbox = args.bbox.as_ref().map(|s| parse_bbox(s)).transpose()?;
    // Overviews for PMTiles are always duplicating (partitioning can't be
    // exported to per-tile MVT).
    let convert = args
        .tuning
        .build_convert_options(Mode::Duplicating, levels, bbox, false)?;
    let export = args
        .export
        .apply(TilesOptions::export_defaults(args.tuning.verbatim));
    Ok(TilesOptions {
        convert,
        export: ExportOptions {
            metadata: args.publication.to_metadata(),
            tile_range: parse_tile_range(args.tile_range.as_deref())?,
            ..export
        },
        // `None` lets core derive the layer name from the input.
        layer_name: args.layer_name.clone(),
        keep_overview: args.keep_overview.clone(),
        shard: None,
    })
}

/// `--tile-range`, parsed by core.
fn parse_tile_range(range: Option<&str>) -> Result<Option<tylertoo_core::shard::TileRange>> {
    Ok(range
        .map(tylertoo_core::shard::TileRange::parse)
        .transpose()?)
}

/// The run-time half of a `tiles` configuration, which reads the input:
/// the sharded-fleet job (#498) and `--max-zoom auto` (#444). Each step
/// re-runs core's validation first, so everything that can fail without
/// reading fails before the next read.
fn resolve_tiles_job(
    args: &TilesArgs,
    spec: &InputSpec,
    options: &mut TilesOptions,
) -> Result<Option<ShardJob>> {
    let validate = |options: &TilesOptions| {
        if args.plan_only {
            options.validate_plan_only()
        } else {
            options.validate()
        }
    };
    validate(options)?;
    let max_zoom = args.max_zoom.plan_zoom();
    let shard = resolve_shard_job(
        spec,
        args.shard.as_deref(),
        args.shard_plan.as_deref(),
        max_zoom,
    )?;
    if let Some(job) = &shard {
        // The fleet-wide selection now, so `--max-zoom auto` estimates over
        // the rows this job reads; the facade applies the rest (#439).
        let tiles_shard = job.as_tiles_shard();
        tiles_shard.apply_selection(&mut options.convert);
        options.shard = Some(tiles_shard);
        validate(options)?;
    }
    // #444: every cheap check has passed; now estimate `auto` (a fixed zoom
    // comes back verbatim, no I/O). A shard plan was bound against the
    // placeholder, so its pivot is re-checked against the real value.
    let max_zoom = resolve_max_zoom(args.max_zoom, spec, &mut options.convert)?;
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
    Ok(shard)
}

/// The flags a `--plan-only` run ignores (#600): the export options core
/// names ([`TilesOptions::ignored_by_plan_only`]), then this CLI's own
/// `--report` and `--force`.
fn plan_only_ignored(args: &TilesArgs, options: &TilesOptions) -> Vec<&'static str> {
    let mut ignored =
        options.ignored_by_plan_only(&TilesOptions::export_defaults(args.tuning.verbatim));
    ignored.extend(
        [("--report", args.report.is_some()), ("--force", args.force)]
            .into_iter()
            .filter_map(|(flag, set)| set.then_some(flag)),
    );
    ignored
}

/// `tiles --plan-only` (#560): pass 1 + the level assignment, `--save-plan`,
/// stop. No archive, no intermediate overview, no export.
///
/// The fleet's coarse job splits into two products — the zooms below the pivot,
/// and the plan the data shards consume. When an external archive owns those
/// zooms (an aggregate-into-fields handover build) the tiles are thrown away,
/// and this is the job without them.
fn run_plan_only(args: TilesArgs) -> Result<()> {
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

    // #600: export flags are ignored here, but not unchecked: core validates
    // the same `TilesOptions` the full coarse job runs with.
    let mut options = tiles_options(&args)?;
    let shard = resolve_tiles_job(&args, &spec, &mut options)?;
    // #600: the fleet recipe is the coarse job's line plus this flag, and that
    // line carries export-only flags. They cannot move the plan, so they are
    // tolerated, and said so once, now that every refusal has run. Core names
    // the ignored export options; `--report` and `--force` are this CLI's.
    let ignored = plan_only_ignored(&args, &options);
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
    let report = tylertoo_core::overview::tiles::write_plan(&tiles_input(&spec), &options)
        .map_err(|e| match e {
            tylertoo_core::overview::tiles::TilesError::Convert(e) => {
                anyhow::Error::new(e).context("writing the convert plan failed")
            }
            other => other.into(),
        })?;

    say!("✓ Wrote the convert plan (no tiles: --plan-only)");
    say!("  input:  {}", spec.display());
    say!(
        "  plan:   {} ({})",
        save_plan.display(),
        HumanBytes(report.plan_bytes)
    );
    say!(
        "  rows:   {} input row(s) → {} feature(s) ({} point, {} line, {} polygon)",
        format_number(report.input_rows as u64),
        format_number(report.input_features as u64),
        format_number(report.points as u64),
        format_number(report.lines as u64),
        format_number(report.polygons as u64),
    );
    say!(
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
    say!(
        "  ranking: {}{}",
        report.ranking.mode,
        report
            .ranking
            .column
            .as_deref()
            .map_or_else(String::new, |c| format!(" on {c:?}"))
    );
    if let Some(spec) = &report.entry_zoom {
        say!("  entry-zoom ladder: {spec}");
    }
    if args.verbose {
        for l in &report.levels {
            say!(
                "  level {:<2} (z{:<2}) gsd {:>10.2} m: {:>9} feature(s)",
                l.planned_level,
                l.zoom.map_or_else(|| "-".to_string(), |z| z.to_string()),
                l.gsd,
                format_number(l.feature_count as u64),
            );
        }
    }
    say!(
        "  time:   {:.2}s total (pass 1 {:.2}s, assignment {:.2}s)",
        start.elapsed().as_secs_f64(),
        report.pass1_secs,
        report.assign_secs,
    );
    if shard.is_some() {
        say!(
            "  next:   give this plan to every data shard with --plan {}",
            save_plan.display()
        );
    } else {
        // No cut digest in the fingerprint, so a data shard (which presents
        // its shard plan's) refuses this plan — say so here rather than at
        // the first shard of the fleet.
        say!(
            "  next:   replay this plan in an unsharded run (`tiles --plan {0}` or \
             `overview --plan {0}`). It records no shard plan, so a fleet's data shards \
             refuse it: for a fleet, re-run with --shard coarse --shard-plan <the fleet's \
             shard plan>",
            save_plan.display()
        );
    }
    Ok(())
}

/// Run `tylertoo tiles`: the one-shot GeoParquet → PMTiles facade.
///
/// The policy — intermediate-overview lifecycle and preflights, the #380
/// declaration rule, empty shard/coarse outcomes, the combined report —
/// lives in core ([`tylertoo_core::overview::tiles`], #439). This maps the
/// parsed arguments onto [`TilesOptions`] and prints.
///
/// [`TilesOptions`]: tylertoo_core::overview::tiles::TilesOptions
/// Whether a convert with `options` stands in for dropped polygons with
/// placeholder squares (#745), for the empty-zoom hint: duplicating mode,
/// with the square disposition globally (the default, unless an entry-zoom
/// ladder turns it into points) or in a `--representation` square band.
fn makes_placeholder_squares(options: &tylertoo_core::overview::convert::ConvertOptions) -> bool {
    use tylertoo_core::overview::level::Mode;
    use tylertoo_core::overview::simplify::{CollapseMode, Representation};

    let global = match options.simplify.collapse {
        CollapseMode::Square => true,
        CollapseMode::Auto => options.entry_zoom.is_none(),
        CollapseMode::Point | CollapseMode::Drop => false,
    };
    let banded = options
        .representation
        .iter()
        .any(|b| b.repr == Representation::Square);
    matches!(options.mode, Mode::Duplicating) && (global || banded)
}

fn run_tiles(args: TilesArgs) -> Result<()> {
    use tylertoo_core::overview::convert::ConvertReport;
    use tylertoo_core::overview::tiles::{self, IntermediateOverview, TilesObserver, TilesOutcome};

    // Checked here, not with clap's `conflicts_with`: `quiet` is a global
    // arg that only `tiles` has a `verbose` to conflict with, and clap's
    // debug asserts reject a conflict target missing from any subcommand.
    if args.verbose && QUIET.load(Ordering::Relaxed) {
        // Built, so the usage line reads `tylertoo tiles ...`, not the root's.
        let mut cli = Cli::command();
        cli.build();
        cli.find_subcommand_mut("tiles")
            .expect("tiles is a subcommand")
            .error(
                clap::error::ErrorKind::ArgumentConflict,
                "--quiet and --verbose cannot be used together",
            )
            .exit();
    }

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

    config::check_output(&output, OutputKind::Pmtiles, args.force)?;
    let named: Vec<&Path> = [output.as_path(), spec.path()]
        .into_iter()
        .chain(args.keep_overview.as_deref())
        .chain(args.shard_plan.as_deref())
        .chain(args.tuning.plan_paths())
        .collect();
    preflight_report(args.report.as_deref(), &named)?;

    // #433/#779: the whole configuration is checked before the convert,
    // which on a large input runs for minutes before the export would
    // otherwise refuse a knob; core re-checks inside the facade.
    let mut tiles_options = tiles_options(&args)?;
    resolve_tiles_job(&args, &spec, &mut tiles_options)?;
    let squares = makes_placeholder_squares(&tiles_options.convert);
    let input = tiles_input(&spec);

    // #314: the disk cost of the one-shot facade must not be silent — name
    // the intermediate's path, size and fate between the two halves, so a
    // failed export has already said where the file is.
    struct Printer {
        verbose: bool,
    }
    impl TilesObserver for Printer {
        fn overview_written(&mut self, intermediate: &IntermediateOverview, c: &ConvertReport) {
            say!(
                "  intermediate overview: {} ({}{})",
                intermediate.path.display(),
                HumanBytes(intermediate.bytes),
                if intermediate.retained {
                    ", retained"
                } else {
                    ", removed after export; --keep-overview PATH retains it"
                }
            );
            if self.verbose {
                say!(
                    "Overview: {} input features → {} rows across {} levels in {:.2}s",
                    format_number(c.input_features as u64),
                    format_number(c.total_rows as u64),
                    c.levels.len(),
                    c.duration_secs
                );
            }
        }
    }
    let mut printer = Printer {
        verbose: args.verbose,
    };

    let report =
        match tiles::convert_to_pmtiles_observed(&input, &output, &tiles_options, &mut printer)? {
            // #498 / #541: an empty data shard and an empty coarse job are
            // legal outcomes — core wrote the valid tile-less archive; this
            // only reports it.
            TilesOutcome::EmptyShard(info) => {
                say!(
                    "✓ shard {} owns no input rows; wrote an empty archive at {}",
                    info.role,
                    output.display()
                );
                say!(
                    "  z{}..z{} declared, 0 tiles. This is normal for a cut whose data is \
                     concentrated elsewhere — `tylertoo merge` skips a tile-less input.",
                    info.min_zoom,
                    info.max_zoom,
                );
                return Ok(());
            }
            TilesOutcome::EmptyCoarse(info) => {
                say!(
                    "✓ coarse job has no features at z{}..z{}; wrote an empty archive at {}",
                    info.min_zoom,
                    info.max_zoom,
                    output.display()
                );
                say!(
                    "  Every feature first appears at or past the pivot z{}, so the data shards \
                     hold all of it. Any --save-plan was written as usual; `tylertoo merge` \
                     skips a tile-less input.",
                    info.pivot_zoom
                );
                return Ok(());
            }
            TilesOutcome::Converted(report) => report,
        };
    let (convert_report, export_report) = (&report.convert, &report.export);

    if args.verbose {
        for z in &export_report.zooms {
            say!(
                "  z{:<2} (level {}): {:>7} tiles, {:>9} features",
                z.zoom,
                z.level,
                z.tile_count,
                z.tile_feature_count
            );
        }
    }

    say!(
        "✓ Converted {} → {}",
        spec.label(),
        output.file_name().unwrap_or_default().to_string_lossy()
    );
    say!(
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
        say!("  {note}");
    }
    if let Some(note) = wrapped_note_line(
        convert_report.wrapped_features,
        convert_report.input_features,
    ) {
        say!("  {note}");
    }
    // #380: the summary line above covers the requested (declared) range,
    // which can be wider than the archive's own PMTiles header (#529, #522:
    // the header always reflects the zooms that actually hold a tile) — say
    // which zooms in the requested range hold nothing rather than let the
    // summary imply they do.
    let empty_zooms: Vec<String> = report
        .empty_declared_zooms()
        .iter()
        .map(|z| format!("z{z}"))
        .collect();
    if !empty_zooms.is_empty() {
        // With placeholder squares on, the level was empty because there was
        // less than one placeholder of area in it. Otherwise point at the
        // generalization, and at `--no-collapse` only when it was given.
        if squares {
            say!(
                "  {} declared but empty: less than one placeholder of area there",
                empty_zooms.join(", ")
            );
        } else {
            say!(
                "  {} declared but empty: every feature generalized away there{}, \
                 or an entry-zoom ladder holds every feature out of them",
                empty_zooms.join(", "),
                if args.tuning.no_collapse {
                    " (drop --no-collapse to keep placeholders)"
                } else {
                    ""
                },
            );
        }
    }

    if let Some(note) = row_group_autoscale_note(convert_report) {
        say!("{note}");
    }

    // The combined report so the one-step run captures both halves the
    // two-step chain would write (`overview --report` + `export-pmtiles
    // --report`). Core owns its shape ({"convert", "export"}).
    if let Some(report_path) = &args.report {
        let json =
            serde_json::to_string_pretty(&*report).context("failed to serialize tiles report")?;
        std::fs::write(report_path, json)
            .with_context(|| format!("failed to write report to {}", report_path.display()))?;
        say!("  report written to {}", report_path.display());
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

/// The informational note for features drawn wrapped across the antimeridian
/// (#644), if any. Not part of the summary line's losses: wrapped data (0..360
/// longitudes, dateline cells) is valid input and nothing was dropped. It is
/// shown because a file in a projected CRS but labeled lon/lat, with
/// `|x| ≤ 540`, would otherwise tile silently at the wrong place. Worded for
/// either CRS: the report does not carry it, and for EPSG:3857 input the edge
/// is the world edge in x, not a longitude.
fn wrapped_note_line(wrapped: usize, input_features: usize) -> Option<String> {
    (wrapped > 0).then(|| {
        format!(
            "note: {} of {} input features reach past the world edge and were drawn \
             wrapped across the antimeridian \u{2014} expected for 0..360 data and \
             dateline cells; otherwise check the real CRS with `gpio inspect <input>`",
            format_number(wrapped as u64),
            format_number(input_features as u64)
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

/// The line explaining why the per-band tile counts sum past the total, or
/// `None` when they add up (#403).
///
/// A tile id several bands wrote at a shared zoom is one tile in the archive
/// (its layers concatenated) but appears in each of those bands' counts, so
/// `report.per_band_tiles` can sum to more than `report.total_tiles`.
fn pyramid_shared_note(per_band_sum: usize, total: usize) -> Option<String> {
    (per_band_sum > total).then(|| {
        format!(
            "  band counts sum to {} but the archive holds {} tiles: a tile shared by \
             several layers counts once per band but once in the total",
            format_number(per_band_sum as u64),
            format_number(total as u64)
        )
    })
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

/// The [`ConvertOptions`] an `overview` command line describes: pure
/// translation (#779), with `--max-zoom auto`'s placeholder (#444).
fn overview_options(args: &OverviewArgs) -> Result<ConvertOptions> {
    let mode = args.mode.parse().map_err(anyhow::Error::msg)?;
    let levels = resolve_level_plan(
        args.gsd.as_deref(),
        args.min_zoom,
        args.max_zoom.plan_zoom(),
    )?;
    let bbox = args.bbox.as_ref().map(|s| parse_bbox(s)).transpose()?;
    args.tuning
        .build_convert_options(mode, levels, bbox, args.cogp_compat)
}

/// Run `tylertoo overview`: build a multi-resolution overview GeoParquet file.
fn run_overview(args: OverviewArgs) -> Result<()> {
    let (spec, output) = resolve_io(
        args.input.clone(),
        args.output.clone(),
        args.files_from.clone(),
    )?;

    // #427: refuse an existing output (without --force) before any work. The
    // overview itself is written to a sibling and renamed over OUTPUT at the
    // end, so an interrupted run leaves a previous file intact either way.
    config::check_output(&output, OutputKind::GeoParquet, args.force)?;
    let named: Vec<&Path> = [output.as_path(), spec.path()]
        .into_iter()
        .chain(args.tuning.plan_paths())
        .collect();
    preflight_report(args.report.as_deref(), &named)?;

    let mut options = overview_options(&args)?;
    // #444/#779: every rule that needs no input, then the estimate (which
    // reads the input) last.
    options.validate()?;
    resolve_max_zoom(args.max_zoom, &spec, &mut options)?;

    let report = run_convert(&spec, &output, &options)?;

    // Human-readable summary.
    say!();
    say!(
        "✓ Overview {} → {}  ({:?} mode)",
        spec.label(),
        output.file_name().unwrap_or_default().to_string_lossy(),
        report.mode
    );
    say!(
        "  {} input features → {} rows across {} levels in {:.2}s",
        format_number(report.input_features as u64),
        format_number(report.total_rows as u64),
        report.levels.len(),
        report.duration_secs
    );
    say!(
        "  {:>3}  {:>12}  {:>10}  {:>10}  {:>12}",
        "lvl",
        "gsd(m)",
        "features",
        "vertices",
        "bytes"
    );
    for lvl in &report.levels {
        say!(
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
        say!(
            "  note: {} of {} input features reach beyond the coordinate range the \
             file's declared CRS allows and were dropped or clipped \u{2014} see the \
             warning above, and check the real CRS with `gpio inspect <input>`",
            format_number(report.out_of_range_features as u64),
            format_number(report.input_features as u64)
        );
    }
    if report.unprojectable_features > 0 {
        say!(
            "  note: {} of {} input features have valid lon/lat but lie outside the \
             Web Mercator tiling domain (|lat| > 85.05\u{b0}); these features cannot \
             be tiled",
            format_number(report.unprojectable_features as u64),
            format_number(report.input_features as u64)
        );
    }
    if let Some(note) = wrapped_note_line(report.wrapped_features, report.input_features) {
        say!("  {note}");
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
        say!(
            "  note: {} empty level(s) omitted ({}) — no features visible at those \
             scales; the pyramid starts at the coarsest non-empty level",
            report.skipped_empty_levels.len(),
            planned.join(", ")
        );
    }

    if let Some(note) = row_group_autoscale_note(&report) {
        say!("{note}");
    }

    if let Some(report_path) = &args.report {
        let json =
            serde_json::to_string_pretty(&report).context("failed to serialize overview report")?;
        std::fs::write(report_path, json)
            .with_context(|| format!("failed to write report to {}", report_path.display()))?;
        say!("  report written to {}", report_path.display());
    }

    Ok(())
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

/// The [`ExportOptions`] an `export-pmtiles` command line describes: pure
/// translation (#779) over [`ExportOptions::default`].
fn export_options(args: &ExportPmtilesArgs) -> Result<ExportOptions> {
    Ok(ExportOptions {
        layer_name: args.layer_name.clone(),
        min_zoom: args.min_zoom,
        properties: args.properties.selection(),
        tile_range: parse_tile_range(args.tile_range.as_deref())?,
        zoom_ceiling: args.zoom_ceiling,
        spill_dir: args.spill_dir.clone(),
        metadata: args.publication.to_metadata(),
        ..args.export.apply(ExportOptions::default())
    })
}

fn run_export_pmtiles(args: ExportPmtilesArgs) -> Result<()> {
    use tylertoo_core::overview::export::export_pmtiles;

    // #427: refuse an existing output (without --force) and a bad option
    // before the input is even looked at. The option rules are core's.
    config::check_output(&args.output, OutputKind::Pmtiles, args.force)?;
    preflight_report(args.report.as_deref(), &[&args.output, &args.input])?;
    let opts = export_options(&args)?;
    opts.validate()?;
    reject_files_from(args.files_from.as_ref(), "export-pmtiles")?;
    require_single_local_file(&args.input, "export-pmtiles")?;

    say!(
        "Exporting {} → {}",
        args.input.display(),
        args.output.display()
    );
    let report = export_pmtiles(&args.input, &args.output, &opts).map_err(|e| {
        step_failed(
            "export",
            e,
            tylertoo_core::overview::export::ExportError::is_config,
        )
    })?;

    say!(
        "  mode={} zooms z{}..z{}",
        report.mode,
        report.min_zoom,
        report.max_zoom
    );
    for z in &report.zooms {
        say!(
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
    say!(
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
        say!("  {note}");
    }

    if let Some(path) = &args.report {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| anyhow::anyhow!("serialize report: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| anyhow::anyhow!("write report {}: {e}", path.display()))?;
        say!("  report → {}", path.display());
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
    say!(
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

/// Run `tylertoo pyramid`: merge per-band PMTiles archives into one (thin
/// facade over `tylertoo_core::pyramid::merge_bands`).
fn run_pyramid(args: PyramidArgs) -> Result<()> {
    use tylertoo_core::pyramid::{build_pyramid, parse_bands};

    // #452: the archive is created only after every band is tiled.
    config::check_output(&args.output, OutputKind::Pmtiles, args.force)?;
    preflight_report(args.report.as_deref(), &[&args.output])?;

    // Spec parsing, band-set validation and the band-input existence check
    // (with its stripped-`=LAYER` hint) all live in core (#445).
    let bands = parse_bands(&args.bands).map_err(|e| anyhow::anyhow!(e))?;

    // #779: the option rules, before any band is tiled.
    let opts = pyramid_options(&args);
    opts.validate()?;

    let report = build_pyramid(&bands, &args.output, &opts)
        .map_err(|e| step_failed("pyramid build", e, tylertoo_core::Error::is_config))?;

    for (layer, lo, hi, n) in &report.per_band_tiles {
        say!(
            "  z{lo}-{hi} -> layer {layer:?}: {} tiles",
            format_number(*n as u64)
        );
    }
    // Only consulted when something was skipped: `classify_band_input`
    // opens each band's first bytes, which is pointless when there is no
    // line to print.
    let split = report.skipped > 0 && bands_split_one_archive(&bands);
    if let Some(line) = pyramid_skip_message(report.skipped, split) {
        say!("{line}");
    }
    let per_band_sum = report.per_band_tiles.iter().map(|(.., n)| n).sum();
    if let Some(line) = pyramid_shared_note(per_band_sum, report.total_tiles) {
        say!("{line}");
    }
    print_publication(
        &report.metadata,
        &report.metadata_conflicts,
        &report.bands_without_metadata,
        "bands",
    );
    say!(
        "✓ Built {} band(s) → {} ({} tiles)",
        bands.len(),
        args.output.display(),
        format_number(report.total_tiles as u64)
    );
    if let Some(path) = &args.report {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| anyhow::anyhow!("serialize report: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| anyhow::anyhow!("write report {}: {e}", path.display()))?;
        say!("  report → {}", path.display());
    }
    Ok(())
}

/// The `pyramid` flags that apply to every GeoParquet band alike, as the
/// library options `build_pyramid` substitutes each band's own layer name
/// and zoom range into: pure translation (#779) over
/// [`PyramidOptions::new`](tylertoo_core::pyramid::PyramidOptions::new).
/// Kept separate from `run_pyramid` so a test can check a flag actually
/// reaches `PyramidOptions` (#374).
fn pyramid_options(args: &PyramidArgs) -> tylertoo_core::pyramid::PyramidOptions {
    let base = tylertoo_core::pyramid::PyramidOptions::new(args.generalize);
    tylertoo_core::pyramid::PyramidOptions {
        export: ExportOptions {
            properties: args.properties.selection(),
            ..args.export.apply(base.export)
        },
        work_dir: args.work_dir.clone(),
        allow_missing_zooms: args.allow_missing_zooms,
        // The final archive's overrides; the band exports write none, so
        // GeoParquet bands contribute no inherited value.
        metadata: args.publication.to_metadata(),
        ..base
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

/// `--report` preflight (#452): the report is written when the run ends, so
/// a bad path would otherwise throw the finished run away. It is rewritten
/// in place, so an existing report must itself be writable. It must not be
/// any of `others` (the paths the command line names for the run to write
/// or read): written last, it would replace the finished output, or the
/// source data, with JSON. Only those paths are compared, not the files a
/// directory, glob or `--files-from` input expands to.
fn preflight_report(report: Option<&Path>, others: &[&Path]) -> Result<()> {
    let Some(path) = report else {
        return Ok(());
    };
    let key = canonical_key(path);
    if let Some(other) = others.iter().find(|o| canonical_key(o) == key) {
        anyhow::bail!(
            "--report {} is the same file as {}; the report would overwrite it",
            path.display(),
            other.display()
        );
    }
    tylertoo_core::atomic_output::check_writable_in_place(path)
        .map_err(|e| anyhow::anyhow!("--report: {e}"))
}

/// Run `tylertoo merge`: several disjoint PMTiles archives → one (thin facade
/// over `tylertoo_core::merge::merge_shards`).
/// `tylertoo shard-plan` — step 0 of a sharded build (#498).
fn run_shard_plan(args: ShardPlanArgs) -> Result<()> {
    use tylertoo_core::shard::ShardPlan;

    // No `exists()` precheck: a shard plan is cut for whatever a fleet will
    // read, which may be a directory, a glob, an s3:// prefix or a manifest.
    // `ConvertSource::resolve` names a bad input far better than a bare
    // stat() ever could, and is the same resolution `tiles` performs.
    let spec = resolve_io_for_planning(args.input, args.files_from)?;
    // Every job of a fleet must be given the SAME shard plan, so replacing
    // one mid-build silently re-cuts the tile space: --force to overwrite.
    config::check_output(&args.output, OutputKind::ShardPlan, args.force)?;

    let source = resolve_convert_source(&spec)?;
    let plan = ShardPlan::compute(&source, args.pivot, args.shards)?;
    plan.save(&args.output)?;

    let input_label = spec.display();
    say!(
        "Cut {} shard(s) at pivot z{} for {input_label}",
        plan.shards(),
        plan.pivot_zoom,
    );
    let total = plan.estimated_rows_total.max(1);
    for (i, r) in plan.ranges.iter().enumerate() {
        say!(
            "  shard {i:<3} tiles {}..={}  ({:>5} pivot tile(s), ~{} row(s), {:.1}%)",
            r.lo,
            r.hi,
            r.hi - r.lo + 1,
            r.estimated_rows,
            100.0 * r.estimated_rows as f64 / total as f64,
        );
    }
    // This and the empty-shard note are `println!`, not `say!`: they are
    // warnings, which `--quiet` keeps, and core logs no copy of them.
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
    say!("\n✓ wrote {}", args.output.display());
    say!(
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

    // Preflight the paths the run will write, before it reads gigabytes: an
    // existing output without --force, a `--report` in a directory that does
    // not exist, or an output whose parent is missing or read-only, would
    // otherwise surface only at the end. This mirrors `--work-dir`, which
    // fails fast because the writer creates its spool file up front.
    config::check_output(&args.output, OutputKind::Pmtiles, args.force)?;
    let written_or_read: Vec<&Path> = std::iter::once(args.output.as_path())
        .chain(args.inputs.iter().map(PathBuf::as_path))
        .collect();
    preflight_report(args.report.as_deref(), &written_or_read)?;

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

    if args.inputs.len() < 2 {
        // Not an error: merging one archive is a well-defined (if pointless)
        // copy, and a script that shards dynamically can legitimately end up
        // with one shard. Worth saying out loud, though.
        if !QUIET.load(Ordering::Relaxed) {
            eprintln!("note: merging a single archive just rewrites it");
        }
    }

    say!(
        "Merging {} archive(s) → {}",
        args.inputs.len(),
        args.output.display()
    );
    let opts = MergeOptions {
        work_dir: args.work_dir.clone(),
        metadata: args.publication.to_metadata(),
    };
    let report = merge_shards(&args.inputs, &args.output, &opts)
        .map_err(|e| anyhow::anyhow!("merge failed: {e}"))?;

    for (zoom, count) in &report.per_zoom_tile_counts {
        say!("  z{:<2} {:>10} tiles", zoom, format_number(*count));
    }
    if !report.inputs_without_bounds.is_empty() {
        // Almost always a shard exported without bounds, which quietly
        // shrinks the merged archive's extent, so it belongs on stdout next
        // to the counts and not only in the log.
        say!(
            "  ! {} input(s) carried no usable bounds and were excluded from the merged bounds:",
            report.inputs_without_bounds.len()
        );
        for name in &report.inputs_without_bounds {
            say!("      {name}");
        }
    }
    print_publication(
        &report.metadata,
        &report.metadata_conflicts,
        &report.inputs_without_metadata,
        "inputs",
    );
    say!(
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
        say!("  report → {}", path.display());
    }
    Ok(())
}

/// Run `tylertoo decode`: PMTiles → GeoParquet (thin facade over
/// `tylertoo_core::decode::decode_pmtiles`).
fn run_decode(args: DecodeArgs) -> Result<()> {
    use tylertoo_core::decode::{decode_pmtiles, DecodeOptions};

    // #427: refuse an existing output (without --force) before any work.
    config::check_output(&args.output, OutputKind::GeoParquet, args.force)?;
    preflight_report(args.report.as_deref(), &[&args.output, &args.input])?;
    reject_files_from(args.files_from.as_ref(), "decode")?;
    require_single_local_file(&args.input, "decode")?;

    // `--zoom N` is shorthand for `--min-zoom N --max-zoom N` (clap already
    // rejects combining them).
    let (min_zoom, max_zoom) = match args.zoom {
        Some(z) => (Some(z), Some(z)),
        None => (args.min_zoom, args.max_zoom),
    };
    let options = DecodeOptions {
        min_zoom,
        max_zoom,
        layer: args.layer,
    };
    // Before the "Decoding" line, so an inverted range prints nothing else.
    options.validate()?;

    say!(
        "Decoding {} → {}",
        args.input.display(),
        args.output.display()
    );
    let report = decode_pmtiles(&args.input, &args.output, &options)
        .with_context(|| format!("decode failed for {}", args.input.display()))?;

    match report.zoom_range {
        Some((lo, hi)) => say!("  zooms z{lo}..z{hi}, layers: {}", report.layers.join(", ")),
        None => say!("  no features matched the filters"),
    }
    say!(
        "\n✓ {} features from {} tiles ({} skipped as degenerate) in {:.2}s",
        format_number(report.features_written),
        format_number(report.tiles_read),
        report.features_skipped,
        report.elapsed_secs
    );
    say!(
        "  note: output is the tiled representation (simplified, clipped, \
         duplicated across zooms); see `tylertoo decode --help`"
    );

    if let Some(path) = &args.report {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| anyhow::anyhow!("serialize report: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| anyhow::anyhow!("write report {}: {e}", path.display()))?;
        say!("  report → {}", path.display());
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

/// Format a number with thousands separators. A `String`, not the
/// `HumanCount` itself, so the `{:>10}`-style padding at call sites applies
/// (`HumanCount`'s `Display` ignores width).
fn format_number(n: u64) -> String {
    HumanCount(n).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every subcommand clap knows is left alone by the bare-form rewrite,
    /// and the names it used to hard-code still are.
    #[test]
    fn bare_form_rewrite_leaves_every_subcommand_alone() {
        let mut names: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        names.extend(
            [
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
            ]
            .map(String::from),
        );
        for name in names {
            let argv: Vec<std::ffi::OsString> = ["tylertoo", name.as_str(), "a.parquet"]
                .map(Into::into)
                .to_vec();
            assert_eq!(rewrite_bare_args(argv.clone()), argv, "{name}");
        }
        let bare: Vec<std::ffi::OsString> = ["tylertoo", "in.parquet", "out.pmtiles"]
            .map(Into::into)
            .to_vec();
        assert_eq!(rewrite_bare_args(bare)[1], "tiles");
    }

    #[test]
    fn format_number_groups_thousands() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(999), "999");
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
        assert_eq!(format_number(u64::MAX), "18,446,744,073,709,551,615");
        assert_eq!(format!("{:>8}", format_number(1_000)), "   1,000");
    }

    /// clap's own consistency checks over the whole command tree (conflict
    /// targets that exist, unique short flags, ...), which otherwise run
    /// only when a debug build happens to parse the offending subcommand.
    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    /// #452: `-q/--quiet` is global: accepted before or after any
    /// subcommand.
    #[test]
    fn quiet_is_accepted_on_either_side_of_any_subcommand() {
        for args in [
            &["tylertoo", "-q", "stats", "a.pmtiles"][..],
            &["tylertoo", "stats", "a.pmtiles", "--quiet"],
            &[
                "tylertoo",
                "merge",
                "-q",
                "out.pmtiles",
                "a.pmtiles",
                "b.pmtiles",
            ],
        ] {
            let cli = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert!(cli.quiet, "{args:?}");
        }
        assert!(
            !Cli::try_parse_from(["tylertoo", "stats", "a.pmtiles"])
                .unwrap()
                .quiet
        );
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

    /// What a plan-only command line ignores, through the real builder.
    fn ignored_export_options(args: &TilesArgs) -> Vec<&'static str> {
        plan_only_ignored(args, &tiles_options(args).expect("the options build"))
    }

    /// #600: every export-only flag parses alongside `--plan-only` and is
    /// named by core's list (plus the CLI's own two); nothing set, nothing
    /// named.
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

        // Several at once: one list, core's in --help order, then the CLI's.
        let a = parse_plan_only(&["--force", "--layer-name", "x", "--report", "r.json"]).unwrap();
        assert_eq!(
            ignored_export_options(&a),
            vec!["--layer-name", "--report", "--force"]
        );

        // The clap default IS export's default, so a bare run never names
        // --tile-buffer.
        assert_eq!(
            bare.export.tile_buffer,
            ExportOptions::default().tile_buffer
        );
        // Under --verbatim the caps' defaults are off, so the bare caps are
        // not "set" either.
        let a = parse_plan_only(&["--verbatim"]).unwrap();
        assert!(ignored_export_options(&a).is_empty());

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
    /// #779: they are core's rules now, not clap's, so the line parses and
    /// core refuses it (`tests/config_rules.rs` runs the binary).
    #[test]
    fn plan_only_contradictions_parse_and_are_refused_by_core() {
        let a = match Cli::try_parse_from(["tylertoo", "tiles", "in.parquet", "--plan-only"])
            .expect("parses: the rule is core's")
            .command
        {
            Command::Tiles(a) => *a,
            other => panic!("expected tiles subcommand, got {other:?}"),
        };
        let err = tiles_options(&a).unwrap().validate_plan_only().unwrap_err();
        assert!(
            matches!(
                err,
                tylertoo_core::overview::tiles::TilesError::Config(
                    config::ConfigError::PlanOnlyWithoutSavePlan
                )
            ),
            "{err:?}"
        );

        parse_plan_only(&[
            "--shard",
            "coarse",
            "--shard-plan",
            "s.json",
            "--tile-range",
            "21..40",
        ])
        .expect("parses: core refuses it once the shard plan is read");
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

        assert_eq!(parse_pyramid(&[]).export.feature_order, FeatureOrder::Input);
        assert_eq!(
            pyramid_options(&parse_pyramid(&[])).export.feature_order,
            FeatureOrder::Input
        );

        assert_eq!(
            parse_pyramid(&["--feature-order", "level:desc"])
                .export
                .feature_order,
            column("level", true)
        );
        let opts = pyramid_options(&parse_pyramid(&["--feature-order", "level:desc"]));
        assert_eq!(opts.export.feature_order, column("level", true));
        // The rest of the export options are untouched by the new knob, and
        // a band keeps every feature: neither cap is on (#761).
        assert_eq!(opts.export.tile_size_limit, None);
        assert_eq!(opts.export.compressed_tile_size_limit, None);
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

        assert_eq!(parse_tiles(&[]).export.feature_order, FeatureOrder::Input);
        assert_eq!(parse_export(&[]).export.feature_order, FeatureOrder::Input);

        assert_eq!(
            parse_tiles(&["--feature-order", "level:desc"])
                .export
                .feature_order,
            column("level", true)
        );
        assert_eq!(
            parse_export(&["--feature-order", "level"])
                .export
                .feature_order,
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
        assert_eq!(parse_tiles(&[]).export.feature_id, None);
        assert_eq!(parse_export(&[]).export.feature_id, None);

        assert_eq!(
            parse_tiles(&["--feature-id", "osm_id"]).export.feature_id,
            Some("osm_id".to_string())
        );
        assert_eq!(
            parse_export(&["--feature-id", "osm_id"]).export.feature_id,
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
        assert_eq!(a.tuning.profile, MemoryProfile::Bounded);
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

    /// #317 / #279: the representation selector and the square disposition
    /// are reachable on BOTH `overview` and the one-shot `tiles` facade, and
    /// they build the right core options.
    #[test]
    fn representation_and_collapse_square_flags() {
        use tylertoo_core::overview::convert::{ConvertError, LevelPlan, RepresentationBand};
        use tylertoo_core::overview::level::Mode;
        use tylertoo_core::overview::simplify::{CollapseMode, Representation};

        let a = parse_tiles(&["--representation", "0-7:point,8-13:geom"]);
        assert_eq!(
            a.tuning.representation.as_deref(),
            Some("0-7:point,8-13:geom")
        );

        let b = parse_tiles(&["--collapse-square"]);
        assert!(b.tuning.collapse_square);

        // --collapse and --collapse-square conflict: core's rule (#779).
        let both = parse_tiles(&["--collapse", "--collapse-square"]);
        let err = both
            .tuning
            .build_convert_options(
                Mode::Duplicating,
                LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 4,
                },
                None,
                false,
            )
            .unwrap_err();
        assert!(
            err.downcast_ref::<ConvertError>().is_some_and(|e| matches!(
                e,
                ConvertError::Config(config::ConfigError::CollapseConflict)
            )),
            "{err:?}"
        );

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
        // #745: no flag is the auto disposition (squares, points under a
        // ladder).
        assert_eq!(opts.simplify.collapse, CollapseMode::Auto);

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
            default.export.partition_wave,
            tylertoo_core::overview::export::PARTITION_WAVE_AUTO
        );
        // Explicit override is honoured verbatim.
        let overridden = parse_tiles(&["--partition-wave", "6"]);
        assert_eq!(overridden.export.partition_wave, 6);
    }

    #[test]
    fn export_pmtiles_partition_wave_defaults_auto_and_overrides() {
        assert_eq!(
            parse_export(&[]).export.partition_wave,
            tylertoo_core::overview::export::PARTITION_WAVE_AUTO
        );
        assert_eq!(
            parse_export(&["--partition-wave", "12"])
                .export
                .partition_wave,
            12
        );
    }

    /// One `--max-tile-size`, with `--tile-size-limit` as its alias, on every
    /// command that writes tiles (#779: one `ExportArgs`).
    #[test]
    fn tile_size_limit_is_an_alias_of_max_tile_size_everywhere() {
        let a = parse_tiles(&["--max-tile-size", "500K"]);
        let b = parse_tiles(&["--tile-size-limit", "500K"]);
        assert_eq!(a.export.max_tile_size, SizeCap::Bytes(500 * 1024));
        assert_eq!(a.export.max_tile_size, b.export.max_tile_size);
        assert_eq!(
            parse_export(&["--max-tile-size", "1M"])
                .export
                .max_tile_size,
            parse_export(&["--tile-size-limit", "1M"])
                .export
                .max_tile_size,
        );
        assert_eq!(
            parse_pyramid(&["--tile-size-limit", "1M"])
                .export
                .max_tile_size,
            SizeCap::Bytes(1024 * 1024)
        );
    }

    /// #761: `auto` is the command's core default — 500K gzip and 2M raw for
    /// `tiles` and `export-pmtiles`, none under `tiles --verbatim` and for
    /// `pyramid` bands (#345/#360: verbatim means every feature reaches the
    /// tile) — `0` turns a cap off, and an explicit size always wins.
    #[test]
    fn size_caps_resolve_against_each_commands_core_default() {
        let tiles = |flags: &[&str]| tiles_options(&parse_tiles(flags)).unwrap().export;
        let caps = |e: &ExportOptions| (e.tile_size_limit, e.compressed_tile_size_limit);
        assert_eq!(caps(&tiles(&[])), (Some(2048 * 1024), Some(500 * 1024)));
        assert_eq!(caps(&tiles(&["--verbatim"])), (None, None));
        assert_eq!(
            caps(&tiles(&["--verbatim", "--max-tile-size", "1K"])),
            (Some(1024), None)
        );
        assert_eq!(
            caps(&tiles(&["--max-compressed-tile-size", "0"])),
            (Some(2048 * 1024), None)
        );
        assert_eq!(
            caps(&tiles(&["--max-tile-size", "auto"])),
            caps(&tiles(&[]))
        );

        let export = export_options(&parse_export(&[])).unwrap();
        assert_eq!(caps(&export), (Some(2048 * 1024), Some(500 * 1024)));
        let export = export_options(&parse_export(&["--max-tile-size", "0"])).unwrap();
        assert_eq!(export.tile_size_limit, None);

        let pyramid = pyramid_options(&parse_pyramid(&[])).export;
        assert_eq!(caps(&pyramid), (None, None));
        let pyramid = pyramid_options(&parse_pyramid(&["--max-compressed-tile-size", "300K"]));
        assert_eq!(pyramid.export.compressed_tile_size_limit, Some(300 * 1024));

        assert!(parse_size_cap("banana").is_err());
        assert_eq!(parse_size_cap("AUTO").unwrap(), SizeCap::Auto);
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

    /// #745: the empty-zoom hint blames "less than one placeholder" only when
    /// squares are actually made: not under a ladder's points, `--collapse`
    /// or `--no-collapse`, and not in partitioning mode.
    #[test]
    fn placeholder_squares_follow_the_resolved_disposition() {
        assert!(makes_placeholder_squares(&verbatim_opts(&[])));
        assert!(makes_placeholder_squares(&verbatim_opts(&[
            "--collapse-square"
        ])));
        assert!(!makes_placeholder_squares(&verbatim_opts(&["--collapse"])));
        assert!(!makes_placeholder_squares(&verbatim_opts(&[
            "--no-collapse"
        ])));
        assert!(!makes_placeholder_squares(&verbatim_opts(&[
            "--magnitude-ladder",
            "level"
        ])));
        assert!(makes_placeholder_squares(&verbatim_opts(&[
            "--magnitude-ladder",
            "level",
            "--collapse-square"
        ])));
        assert!(makes_placeholder_squares(&verbatim_opts(&[
            "--no-collapse",
            "--representation",
            "0-3:square,4-6:geom"
        ])));
        let partitioning = tylertoo_core::overview::convert::ConvertOptions {
            mode: tylertoo_core::overview::level::Mode::Partitioning,
            ..verbatim_opts(&[])
        };
        assert!(!makes_placeholder_squares(&partitioning));
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
        assert_eq!(cli.simplify.collapse, CollapseMode::Auto);
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
        assert!(msg.contains("verbatim requires duplicating mode"), "{msg}");

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
        assert_eq!(plain.assign.polygon_thinning, 0.0);
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
    fn build_convert_options_leaves_the_shared_rules_to_core() {
        use tylertoo_core::overview::convert::{ConvertError, LevelPlan};
        use tylertoo_core::overview::level::Mode;

        let levels = || LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 6,
        };
        // #779: the builder translates; core's `validate` refuses. These
        // were `bail!`s here, copied in Python, "also enforced in core".
        let refused = |flags: &[&str], mode: Mode| -> ConvertError {
            parse_tiles(flags)
                .tuning
                .build_convert_options(mode, levels(), None, false)
                .expect("the builder only translates")
                .validate()
                .expect_err("core refuses")
        };
        assert!(matches!(
            refused(
                &["--class-rank", "k:a=1", "--sort-key", "height"],
                Mode::Duplicating
            ),
            ConvertError::RankingConflict
        ));
        assert!(matches!(
            refused(&["--accumulate-attribute", "pop:sum"], Mode::Duplicating),
            ConvertError::AccumulateWithoutCluster
        ));
        assert!(matches!(
            refused(&["--cluster"], Mode::Partitioning),
            ConvertError::ClusterPartitioningUnsupported
        ));
        parse_tiles(&["--cluster"])
            .tuning
            .build_convert_options(Mode::Duplicating, levels(), None, false)
            .unwrap()
            .validate()
            .unwrap();
        // Rules that hold for the flags themselves fire while building.
        let err = parse_tiles(&["--coalesce-lines"])
            .tuning
            .build_convert_options(Mode::Partitioning, levels(), None, false)
            .unwrap_err();
        assert!(
            err.to_string().contains("--coalesce-lines requires"),
            "{err}"
        );
        // A NaN class rank (b8fa58a): refused by core's parser.
        let err = parse_tiles(&["--class-rank", "k:a=nan"])
            .tuning
            .build_convert_options(Mode::Duplicating, levels(), None, false)
            .unwrap_err();
        assert!(err.to_string().contains("finite"), "{err}");
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

    /// #644: wrapped features are valid input, so they get their own note,
    /// never a loss in the summary line, and nothing at zero.
    #[test]
    fn wrapped_note_line_is_separate_and_silent_at_zero() {
        assert_eq!(wrapped_note_line(0, 10), None);
        let note = wrapped_note_line(1234, 5000).unwrap();
        assert!(
            note.contains("1,234 of 5,000 input features reach past the world edge")
                && note.contains("drawn wrapped across the antimeridian")
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

    // --- #403: per-band counts can sum past the total -------------------

    /// Disjoint bands (or shared zooms with no tile in common): the per-band
    /// lines add up to the total and need no explanation.
    #[test]
    fn pyramid_shared_note_is_silent_when_the_counts_add_up() {
        assert_eq!(pyramid_shared_note(0, 0), None);
        assert_eq!(pyramid_shared_note(12, 12), None);
    }

    /// A tile id several bands wrote at a shared zoom counts once per band
    /// but once in the total, so the per-band lines sum past it. Say why,
    /// next to the numbers, or the summary reads as tiles going missing.
    #[test]
    fn pyramid_shared_note_explains_a_sum_above_the_total() {
        let msg = pyramid_shared_note(1500, 1000).unwrap();
        assert!(msg.contains("1,500") && msg.contains("1,000"), "{msg}");
        assert!(msg.contains("once per band"), "{msg}");
        assert!(!msg.contains('!'), "nothing is wrong: {msg}");
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

    /// #510 review, S3-6: an output path in a directory that does not exist
    /// used to surface only after the merge had read every input. Core's
    /// output preflight probes it up front, like `--work-dir`.
    #[test]
    fn output_preflight_rejects_a_missing_directory_and_keeps_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let check = |p: &Path| config::check_output(p, OutputKind::Pmtiles, true);
        let err = check(&dir.path().join("nope").join("out.pmtiles"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");

        // A writable directory passes and leaves nothing behind.
        let fresh = dir.path().join("out.pmtiles");
        check(&fresh).unwrap();
        assert!(!fresh.exists(), "the probe must not leave a file behind");

        // An existing file is not touched — `--force` owns that decision.
        let existing = dir.path().join("kept.pmtiles");
        std::fs::write(&existing, b"keep me").unwrap();
        check(&existing).unwrap();
        assert_eq!(std::fs::read(&existing).unwrap(), b"keep me");
    }

    // --- Publication metadata (--name / --description / --attribution) ----

    const PUBLICATION_FLAGS: [&str; 3] = ["name", "description", "attribution"];

    /// Which of the three flags `cmd` shows, in `PUBLICATION_FLAGS` order.
    fn publication_flags_of(cmd: &clap::Command) -> Vec<bool> {
        PUBLICATION_FLAGS
            .iter()
            .map(|flag| {
                cmd.get_arguments()
                    .any(|a| a.get_long() == Some(flag) && !a.is_hide_set())
            })
            .collect()
    }

    /// The flags are on every command that writes a PMTiles archive, as one
    /// optional single string each, and on no command that does not.
    #[test]
    fn publication_flags_are_on_every_producer_and_no_other_command() {
        use clap::CommandFactory;
        for (label, cmd) in [
            ("tiles", TilesArgs::command()),
            ("export-pmtiles", ExportPmtilesArgs::command()),
            ("pyramid", PyramidArgs::command()),
            ("merge", MergeArgs::command()),
        ] {
            assert_eq!(publication_flags_of(&cmd), [true; 3], "{label}");
            for arg in cmd
                .get_arguments()
                .filter(|a| a.get_long().is_some_and(|l| PUBLICATION_FLAGS.contains(&l)))
            {
                assert!(!arg.is_required_set(), "{label} {arg:?}");
                assert!(matches!(arg.get_action(), clap::ArgAction::Set), "{label}");
            }
        }
        for (label, cmd) in [
            ("overview", OverviewArgs::command()),
            ("validate", ValidateArgs::command()),
            ("decode", DecodeArgs::command()),
            ("stats", StatsArgs::command()),
            ("shard-plan", ShardPlanArgs::command()),
        ] {
            assert_eq!(publication_flags_of(&cmd), [false; 3], "{label}");
        }
    }

    #[test]
    fn publication_flags_keep_values_exactly() {
        use tylertoo_core::TilesetMetadata;
        let html = "<a href=\"https://www.nps.gov/\">\u{a9} NPS</a>\nline";
        let a = parse_tiles(&[
            "--name",
            "tiles",
            "--description",
            "",
            "--attribution",
            html,
        ]);
        assert_eq!(
            a.publication.to_metadata(),
            TilesetMetadata::new()
                .with_name("tiles")
                .with_description("")
                .with_attribution(html)
        );
        assert_eq!(
            parse_tiles(&[]).publication.to_metadata(),
            TilesetMetadata::new()
        );
        // A value that looks like a flag needs the `=` form.
        assert_eq!(
            parse_tiles(&["--name=--help"]).publication.name.as_deref(),
            Some("--help")
        );
        // One value per flag: clap's ordinary refusal, no appending.
        assert!(Cli::try_parse_from([
            "tylertoo",
            "tiles",
            "in.parquet",
            "out.pmtiles",
            "--name",
            "a",
            "--name",
            "b"
        ])
        .is_err());
        // The bare form with the flags after INPUT OUTPUT, even with a value
        // that names a subcommand.
        let argv = rewrite_bare_args(
            ["tylertoo", "in.parquet", "out.pmtiles", "--name", "tiles"].map(Into::into),
        );
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Tiles(a) => assert_eq!(a.publication.name.as_deref(), Some("tiles")),
            other => panic!("expected tiles, got {other:?}"),
        }
        // ...and before them: the value is not taken for the subcommand.
        for value in ["overview", "merge", "help"] {
            let argv = rewrite_bare_args(
                ["tylertoo", "--name", value, "in.parquet", "out.pmtiles"].map(Into::into),
            );
            match Cli::try_parse_from(argv).unwrap().command {
                Command::Tiles(a) => {
                    assert_eq!(a.publication.name.as_deref(), Some(value));
                    assert_eq!(a.input.as_deref(), Some(Path::new("in.parquet")));
                }
                other => panic!("--name {value}: expected tiles, got {other:?}"),
            }
        }
        // A subcommand after the global `-q` is still a subcommand.
        let argv: Vec<std::ffi::OsString> = ["tylertoo", "-q", "merge", "a.pmtiles", "out.pmtiles"]
            .map(Into::into)
            .to_vec();
        assert_eq!(rewrite_bare_args(argv.clone()), argv);
    }

    /// `--plan-only` writes no archive, so the three flags are reported as
    /// ignored, in `--help` order, and change nothing about the plan.
    #[test]
    fn plan_only_names_publication_flags_as_ignored() {
        for (argv, name) in [
            (&["--name", ""][..], "--name"),
            (&["--description", "d"][..], "--description"),
            (&["--attribution", ""][..], "--attribution"),
        ] {
            let a = parse_plan_only(argv).unwrap();
            assert_eq!(ignored_export_options(&a), vec![name], "{argv:?}");
        }
        let a = parse_plan_only(&[
            "--attribution",
            "a",
            "--force",
            "--layer-name",
            "x",
            "--name",
            "n",
        ])
        .unwrap();
        assert_eq!(
            ignored_export_options(&a),
            vec!["--layer-name", "--name", "--attribution", "--force"]
        );
    }

    /// Too large for any archive's metadata: refused by the preflight, on a
    /// full run and on `--plan-only` alike, before anything is converted.
    #[test]
    fn oversized_publication_values_fail_the_export_preflight() {
        let huge = "\"".repeat(tylertoo_core::compression::MAX_INTERNAL_BYTES as usize / 2);
        let err = tiles_options(&parse_tiles(&["--attribution", &huge]))
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("attribution"), "{err}");
        let a = parse_plan_only(&["--description", &huge]).unwrap();
        let err = tiles_options(&a)
            .unwrap()
            .validate_plan_only()
            .unwrap_err()
            .to_string();
        assert!(err.contains("description"), "{err}");
    }
}

// Surface-parity gates (#779, track G2): flag/kwarg parity, defaults,
// parsed-but-unused flags and pyramid/tiles flag parity. The test source lives
// with the other CLI tests; it is compiled here because the Args structs and
// the option builders are private to this binary crate.
#[cfg(test)]
#[path = "../tests/parity/in_crate.rs"]
mod surface_parity;
