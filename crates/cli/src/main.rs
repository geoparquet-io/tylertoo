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
use tylertoo_core::overview::export::FeatureOrder;
use tylertoo_core::overview::ladder::{EntryZoomKind, EntryZoomSpec};

/// Parse human-readable memory size (e.g., "8G", "16G", "512M") to bytes.
fn parse_memory_size(s: &str) -> Result<usize, String> {
    let s = s.trim().to_uppercase();
    let (num_str, multiplier) = if s.ends_with("G") || s.ends_with("GB") {
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

    num_str
        .trim()
        .parse::<usize>()
        .map(|n| n * multiplier)
        .map_err(|_| {
            format!(
                "Invalid memory size: '{}'. Use format like '8G', '16G', '512M'",
                s
            )
        })
}

/// Parse a human-readable byte size (e.g., "500K", "1M", "2G") as usize.
///
/// A plain integer with no suffix is interpreted as raw bytes, so callers that
/// previously passed a byte count (e.g. `--tile-size-limit 500000`) keep working.
fn parse_size_bytes(s: &str) -> Result<usize, String> {
    parse_memory_size(s).map_err(|_| {
        format!("Invalid size: '{s}'. Use a byte count or a suffixed size like '500K', '1M', '2G'")
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

/// Top-level CLI: a default (bare) tile pipeline plus subcommands.
///
/// `tylertoo input.parquet output.pmtiles` still works (bare tile pipeline);
/// `tylertoo tiles ...` is the explicit form, and `overview` / `validate`
/// are the GeoParquet-overview subcommands.
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
    /// Generate PMTiles vector tiles (the default pipeline).
    Tiles(Box<TilesArgs>),
    /// Build a multi-resolution overview GeoParquet file.
    Overview(Box<OverviewArgs>),
    /// Validate a GeoParquet overview file against the spec (§6.2).
    Validate(ValidateArgs),
    /// Export a PMTiles archive from an overview GeoParquet file (Plan E0).
    ExportPmtiles(ExportPmtilesArgs),
    /// Decode a PMTiles vector-tile archive back to GeoParquet.
    Decode(DecodeArgs),
    /// Build a multi-band pyramid: several inputs, each owning a zoom range,
    /// one archive (issue #345).
    Pyramid(PyramidArgs),
    /// Concatenate disjoint PMTiles archives into one (issue #498).
    Merge(MergeArgs),
    /// Cut a dataset's tile space into N disjoint shards (issue #498).
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
/// A pyramid serves the same map from a *different input* at different zooms:
/// coarse zooms read a pre-aggregated summary, fine zooms read the raw
/// features. That is not the generalization ladder — a coarse aggregate is a
/// different aggregation, not a thinned sample of the fine one — which is why
/// a band is tiled verbatim by default.
///
/// A band may point at GeoParquet (tiled here, one shot) or at a PMTiles
/// archive already covering that range (merged as-is). The kind is detected
/// from the file, so both spellings are just `--band LO-HI:PATH[:LAYER]`. A
/// pre-tiled archive does not have to match the declared range exactly: it
/// may hold MORE zooms than the band declares (a subrange — see
/// `--allow-missing-zooms` for the reverse, an archive holding FEWER).
#[derive(Parser, Debug)]
pub struct PyramidArgs {
    /// Output PMTiles archive.
    pub output: PathBuf,

    /// A band: `LO-HI:INPUT[:LAYER]`, repeatable.
    ///
    /// INPUT is either a GeoParquet source — tiled here, restricted to this
    /// band's zoom range — or a PMTiles archive already covering that range,
    /// which is merged as-is. For a pre-tiled archive the declared LO-HI does
    /// not have to equal the archive's own zoom range exactly: an archive
    /// that holds MORE zooms than LO-HI is a subrange — the documented way to
    /// split one pre-tiled archive across several bands (e.g. two `--band`
    /// entries pointing at the same z0-z13 archive, one declaring z0-5 and
    /// the other z6-13) — and is accepted quietly. An archive that holds
    /// FEWER zooms than LO-HI is an error by default: those missing zooms
    /// would render silently empty, almost always because LO-HI disagrees
    /// with what the archive was actually tiled with; pass
    /// `--allow-missing-zooms` for a deliberately sparse pyramid. A LO-HI
    /// that shares no zoom at all with the archive is always an error,
    /// regardless of that flag. Which kind of INPUT this is is detected from
    /// the file, not the extension. A source may be remote (`https://`,
    /// `s3://`, `gs://`), read with byte-range requests like every other
    /// subcommand's input; a band ARCHIVE must be local, since the merge
    /// reads it by offset.
    ///
    /// LAYER defaults to the file stem, and several bands may share one layer
    /// name (the usual case: a coarse and a fine aggregate that are the same
    /// layer to a client). Two bands in the SAME layer must not share a zoom
    /// -- they would write the same tile ids. Bands in DIFFERENT layers may
    /// (tippecanoe's -L): at the zooms they share, each tile carries every
    /// band's layer, so `--band 0-13:a.parquet:2024 --band 0-13:b.parquet:2025
    /// --generalize` is one archive with two independently generalized
    /// layers. The per-tile size cap applies to each band's tiles before
    /// they are combined, not to the combined tile. For a pre-tiled archive
    /// LAYER is a label: when bands share zooms it must match the layer
    /// name inside the archive, or the merge is refused rather than write
    /// two layers of one name into a tile.
    ///
    /// Bands are emitted coarsest-first in the merged archive regardless of
    /// listing order.
    ///
    /// Colons in INPUT: the LAYER is only split off the LAST `:` when what
    /// follows it has no `/`, `\` or `:`, so a URL, a Windows drive and a
    /// `2024:06/` directory stay whole. A drive-relative path with no `\`
    /// after the drive, e.g. `C:data.parquet`, also stays whole: a single
    /// ASCII letter before the last `:` is treated as a drive letter, not a
    /// path, even though `data.parquet` alone would otherwise look like a
    /// bare layer name. For the inputs that rule cannot express — one ENDING
    /// in a bare colon segment, e.g. a Hive directory
    /// `admin:country_code=BR` — spell the band `LO-HI=INPUT[=LAYER]`
    /// instead: the range is split at the first `=` and the LAYER at the
    /// last, again only when the segment after it has no `/`, `\` or `:`. An
    /// INPUT that itself ends in `=VALUE` needs an explicit `=LAYER`.
    #[arg(long = "band", required = true, value_name = "LO-HI:INPUT[:LAYER]")]
    pub bands: Vec<String>,

    /// Generalize each GeoParquet band with the normal ladder instead of
    /// tiling it verbatim.
    ///
    /// Off by default: a band's premise is that its input is already the right
    /// resolution for the zooms it owns, so thinning and simplifying it would
    /// re-introduce exactly what banding avoids. Pass this when a band is raw
    /// features spanning several zooms and you do want the ladder inside it.
    #[arg(long)]
    pub generalize: bool,

    /// Per-tile MVT size cap for bands tiled here (e.g. "500K"). Unset means
    /// no cap, matching the verbatim default: a valve that sheds features to
    /// fit a byte budget would drop cells the band exists to draw.
    #[arg(long, value_name = "SIZE", value_parser = parse_size_bytes)]
    pub max_tile_size: Option<usize>,

    /// Directory for the per-band intermediates (removed on the way out).
    /// Defaults to the system temp directory.
    #[arg(long, value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    /// Allow a pre-tiled band's declared zoom range to overshoot what its
    /// archive actually holds.
    ///
    /// Off by default: an overshooting band declares zooms its archive does
    /// not have, and those zooms would render silently empty — almost always
    /// a `--band` range that disagrees with what the archive was actually
    /// tiled with, so it is a hard error unless this is set. Has no effect
    /// on a band whose declared range shares no zoom at all with its
    /// archive (that is always an error), nor on a band that declares a
    /// subrange of its archive (that is always fine, with or without this
    /// flag). Pass this only for a deliberately sparse pyramid.
    #[arg(long)]
    pub allow_missing_zooms: bool,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    pub force: bool,
}

/// Arguments for `tylertoo merge`.
///
/// Merge concatenates PMTiles archives that hold *disjoint* tiles into one,
/// by copying tile bodies verbatim: nothing is decoded, re-simplified or
/// re-encoded, so the merged archive's tiles are bit-identical to its inputs'.
///
/// It is the second half of a sharded build: tile each shard of a large input
/// separately, then merge the shard archives into the archive that ships.
/// A shard's tile ids are disjoint from every other shard's by construction,
/// but they need not form a contiguous slice of the archive's id space — two
/// subtrees at different depths interleave on the Hilbert curve, so one
/// shard's first and last id can straddle ids another shard holds. The merge
/// handles that; disjointness is checked per tile id, not per id range.
///
/// That disjointness is checked, not assumed: if any two inputs claim
/// overlapping tile ids the merge is refused, naming both archives, before
/// any tile is copied. An overlap means the shards were cut wrong (or one was
/// listed twice, or is left over from an earlier run), and merging anyway
/// would produce an archive whose tiles silently shadow each other.
///
/// To combine archives that *do* overlap — different data at different zooms,
/// or several layers over the same zooms — use `tylertoo pyramid`, which
/// merges by band and concatenates layers where they meet.
#[derive(Parser, Debug)]
pub struct MergeArgs {
    /// Output PMTiles archive.
    pub output: PathBuf,

    /// Input PMTiles archives, two or more. They must hold disjoint tile ids
    /// and agree on tile type and tile compression; the merged archive's
    /// bounds are the union of theirs, its zoom range the union of the ranges
    /// they declare, and its `vector_layers` the union of theirs (layers
    /// sharing an id collapse into one entry spanning their combined zooms,
    /// with their fields unioned).
    #[arg(required = true, value_name = "INPUT")]
    pub inputs: Vec<PathBuf>,

    /// Directory for the merge's spool file, which holds the merged tile data
    /// until the archive is assembled. Defaults to the system temp directory
    /// — worth setting when the output is large and `/tmp` is a small tmpfs.
    #[arg(long, value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    /// Write the JSON merge report to this path.
    ///
    /// Includes per-zoom tile counts, which are what a sharded build is
    /// checked against: merging N shards must yield the same tiles per zoom
    /// as tiling the whole input in one pass.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    pub force: bool,
}

/// Arguments for `tylertoo decode`.
///
/// The output is the TILED representation, not the original source data:
/// geometries are simplified per zoom, clipped to (buffered) tile bounds,
/// duplicated across neighboring tiles and zoom levels, and only the
/// properties that survived tiling are present. There is no round-trip
/// guarantee. Matching tippecanoe-decode, nothing is deduplicated; use
/// `--zoom` (or filter the output's `zoom` column) for a single
/// representation, and prefer the maximum zoom for the best detail.
#[derive(Parser, Debug)]
#[command(after_help = "\
The output is the tiled representation, not the original source:
  - simplified: vertices were removed during tiling at lower zooms
    (extract the max zoom for best detail)
  - clipped: features are cut at (buffered) tile boundaries
  - duplicated: a feature appears once per neighboring tile and per
    zoom level; nothing is deduplicated (matches tippecanoe-decode) -
    filter with --zoom or the output's `zoom` column
  - lossy properties: attributes dropped during tiling cannot be
    recovered
There is no round-trip guarantee: A.parquet -> B.pmtiles -> C.parquet
does not reproduce A. See docs/decode.md for details.")]
struct DecodeArgs {
    /// Input PMTiles archive (vector tiles).
    #[arg(value_name = "INPUT")]
    input: PathBuf,

    /// Output GeoParquet file.
    #[arg(value_name = "OUTPUT")]
    output: PathBuf,

    /// Decode a single zoom level (recommended for most uses).
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

    /// Not supported here (single-file subcommand); accepted so the error
    /// can point at `overview`/`tiles` instead of clap's generic message.
    #[arg(long, value_name = "PATH", hide = true)]
    files_from: Option<PathBuf>,
}

/// Arguments for `tylertoo shard-plan` — step 0 of a sharded build (#498).
///
/// Cuts the pivot zoom's tile-id space into N contiguous runs of roughly equal
/// estimated rows and writes them as a small JSON artifact. Every job of the
/// fleet is then given that one file, so all of them agree on the cut by
/// construction rather than by each recomputing an estimate.
///
/// Reads parquet **footers only** — no data page is touched — so planning a
/// planet-scale input takes seconds.
#[derive(Parser, Debug)]
pub struct ShardPlanArgs {
    /// Input GeoParquet (the same input every job of the fleet tiles): a
    /// local file, a directory or glob of partitions, or a remote URL
    /// (s3://, https://, gs://). Resolved exactly as `tiles` resolves it, so
    /// a plan can be cut for whatever a fleet will actually read.
    /// Omit when --files-from is given.
    #[arg(value_name = "INPUT", required_unless_present = "files_from")]
    pub input: Option<PathBuf>,

    /// Plan for the inputs listed in this manifest instead of a positional
    /// INPUT: one local path or remote URL per line, order preserved
    /// VERBATIM (it defines the dataset row order). Same manifest every job
    /// of the fleet is given — the plan binds itself to it part by part.
    #[arg(long, value_name = "PATH")]
    pub files_from: Option<PathBuf>,

    /// Where to write the shard plan.
    #[arg(short, long, value_name = "PATH")]
    pub output: PathBuf,

    /// How many data shards to cut. One `tiles --shard i/N` job per shard,
    /// plus one `--shard coarse` job; all N+1 archives merge in one step.
    #[arg(long, value_name = "N")]
    pub shards: usize,

    /// Zoom to cut at. Shards own zooms [PIVOT, --max-zoom]; the coarse job
    /// owns [0, PIVOT-1].
    ///
    /// Pick it so each shard holds a few tiles' worth of data: too coarse and
    /// the fleet cannot be balanced (there are not enough tiles to cut), too
    /// fine and the coarse job is doing most of the build on its own. z4-z8
    /// covers every realistic fleet size.
    #[arg(long, value_name = "ZOOM", default_value = "6")]
    pub pivot: u8,

    /// Overwrite an existing plan at --output.
    #[arg(short, long)]
    pub force: bool,
}

/// Arguments for `tylertoo export-pmtiles`.
#[derive(Parser, Debug)]
struct ExportPmtilesArgs {
    /// Input overview GeoParquet file (produced by `tylertoo overview`).
    #[arg(value_name = "INPUT")]
    input: PathBuf,

    /// Output PMTiles archive.
    #[arg(value_name = "OUTPUT")]
    output: PathBuf,

    /// MVT layer name written into every tile.
    #[arg(long, default_value = "overview")]
    layer_name: String,

    /// Minimum zoom the archive declares, even if the overview file's coarsest
    /// levels are missing (#380). `overview` omits a level that generalizes to
    /// nothing, so a file built for z0..z13 can start at z2; without this the
    /// header then says z2 and a client set up for the requested range never
    /// asks for the zoomed-out view. The empty zooms hold no tiles (an empty
    /// tile, in PMTiles terms). Must not be finer than the coarsest level
    /// present. Default: the coarsest level's zoom
    #[arg(long, value_name = "ZOOM")]
    min_zoom: Option<u8>,

    /// Keep ONLY these properties in the tiles (repeatable; tippecanoe -y),
    /// matched on the names the tiles publish. The overview file is
    /// untouched. Naming a property the file does not export is an error;
    /// the --feature-order column must stay included
    #[arg(long, value_name = "NAME")]
    include_property: Vec<String>,

    /// Drop these properties from the tiles (repeatable; tippecanoe -x).
    /// Ignored when --include-property is given, as in tippecanoe
    #[arg(long, value_name = "NAME")]
    exclude_property: Vec<String>,

    /// Drop every property (tippecanoe -X): geometry-only tiles. Ignored
    /// when --include-property is given, as in tippecanoe
    #[arg(long)]
    exclude_all_properties: bool,

    /// Per-tile edge buffer, in tile pixels (feature seam continuity).
    #[arg(long, default_value = "8")]
    tile_buffer: u32,

    /// Per-tile MVT size cap (e.g., "500K", "1M", or raw bytes). When a tile
    /// exceeds it, a single non-iterative drop pass sheds features for that tile
    /// only (largest-first for polygons/lines; a uniform spatial stride for
    /// point tiles). Defaults to 500K (tippecanoe parity, #280); pass 0 to
    /// disable the cap. Aliased as --max-tile-size for parity with the `tiles`
    /// command.
    #[arg(long, value_name = "SIZE", alias = "max-tile-size", default_value = "500K", value_parser = parse_size_bytes)]
    tile_size_limit: usize,

    /// Write the JSON export report to this path.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

    /// Disable the simple-clip fast path (issue #239), forcing the i_overlay
    /// boundary-bridge fallback on every polygon clip. The fast path is on by
    /// default (render-equivalent on simple rings); pass this only when you need
    /// byte-stable tile output, since the fast path rotates simple rings to a
    /// different start vertex.
    #[arg(long)]
    no_simple_clip_fastpath: bool,

    /// Partitions processed per band read during export (the export
    /// concurrency knob). `auto` (the default) preflights a memory budget:
    /// the machine's core count, capped by how many estimated per-partition
    /// transients fit in a fraction of available RAM (container-aware: cgroup
    /// v2/v1 limits are respected; floor 6; fixed cap 16 only when RAM cannot
    /// be probed; override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES).
    /// Pass an explicit integer to override.
    /// Wider waves keep more cores busy at proportionally more peak memory
    /// (one wave of partitions resident). The chosen width and the preflight
    /// inputs are logged at export start. Output is byte-identical for every
    /// value.
    #[arg(long, value_name = "N|auto", default_value = "auto", value_parser = parse_partition_wave)]
    partition_wave: usize,

    /// Within-tile feature order (#361): `input` (default) or a property
    /// name, optionally `:asc` / `:desc`.
    ///
    /// MVT does not define draw order, but renderers paint features in the
    /// order the tile lists them, so this is the paint order for any style
    /// that does not override it. `input` emits source row order. Naming a
    /// column sorts within each tile by that property — `--feature-order
    /// level` puts high `level` on top, which is what a nested choropleth
    /// usually wants — with ties kept in input order so output stays
    /// deterministic.
    #[arg(long, value_name = "input|COLUMN[:asc|:desc]", default_value = "input")]
    feature_order: FeatureOrder,

    /// Emit only the tiles in this PMTiles tile-id range (#498): `LO..HI`,
    /// two tile ids AT THE SAME ZOOM.
    ///
    /// That zoom is the pivot. The range then owns every descendant of those
    /// tiles at every deeper zoom — a subtree's ids are contiguous on the
    /// Hilbert curve, so the restriction is an exact interval test at each
    /// zoom, not a bounding approximation. Tiles COARSER than the pivot are
    /// outside the range and are not emitted.
    ///
    /// Ranges that partition the pivot zoom partition every deeper zoom, so
    /// the resulting archives are disjoint by construction and `tylertoo
    /// merge` concatenates them without re-encoding anything. `tiles --shard`
    /// derives this automatically from a shard plan; this flag is the manual
    /// form, for a cut you want to choose yourself
    #[arg(long, value_name = "LO..HI")]
    tile_range: Option<String>,

    /// Emit only the tiles at or below this zoom (#498) — the coarse half of
    /// a sharded build, complementing --tile-range's finer half.
    ///
    /// Named a CEILING, not --max-zoom, because it is the opposite of
    /// --min-zoom here: --min-zoom only widens what the header DECLARES,
    /// while this one decides which zooms are actually emitted. Distinct from
    /// building a shallower pyramid too: the overview file still holds every
    /// level (its convert plan is the one the shards consume, and the level
    /// plan is fingerprinted), and this only decides which of them reach the
    /// archive
    #[arg(long, value_name = "ZOOM")]
    zoom_ceiling: Option<u8>,

    /// Not supported here (single-file subcommand); accepted so the error
    /// can point at `overview`/`tiles` instead of clap's generic message.
    #[arg(long, value_name = "PATH", hide = true)]
    files_from: Option<PathBuf>,
}

/// Arguments for `tylertoo overview`.
#[derive(Parser, Debug)]
struct OverviewArgs {
    /// Input GeoParquet (EPSG:4326 or EPSG:3857): a local file, a directory
    /// or glob of partitions, or a remote URL (s3://, https://, gs://).
    /// s3://.../ and gs://.../ prefixes (trailing slash) are listed to their
    /// .parquet objects. Remote inputs are read with byte-range requests;
    /// with --bbox, only the matching row groups are ever downloaded.
    /// Omit when --files-from is given (then the one positional is OUTPUT).
    #[arg(value_name = "INPUT", required_unless_present = "files_from")]
    input: Option<PathBuf>,

    /// Output overview GeoParquet file.
    #[arg(value_name = "OUTPUT", required_unless_present = "files_from")]
    output: Option<PathBuf>,

    /// Convert the inputs listed in this manifest instead of a positional
    /// INPUT: one local path or remote URL per line; `#` comment lines and
    /// blank lines are skipped; line order is preserved VERBATIM (it defines
    /// the dataset row order). Each line must be a single .parquet
    /// file/object — no directories, globs, or prefixes. Local and remote
    /// entries may be mixed. Usage: --files-from <PATH> OUTPUT.
    #[arg(long, value_name = "PATH")]
    files_from: Option<PathBuf>,

    /// Level materialization mode.
    #[arg(long, default_value = "duplicating", value_parser = ["duplicating", "partitioning"])]
    mode: String,

    /// Minimum (coarsest) Web Mercator zoom for the level range.
    #[arg(long, default_value = "0")]
    min_zoom: u8,

    /// Maximum (finest / canonical) Web Mercator zoom for the level range.
    #[arg(long, default_value = "6")]
    max_zoom: u8,

    /// Explicit comma-separated GSD list (meters, strictly decreasing).
    /// Overrides --min-zoom/--max-zoom when set.
    #[arg(long, value_name = "GSDS")]
    gsd: Option<String>,

    /// Regional extract: only convert features whose bbox intersects this
    /// bounding box (lon/lat degrees: xmin,ymin,xmax,ymax). Row groups whose
    /// GeoParquet 1.1 covering statistics don't intersect are skipped at the
    /// parquet footer level (no data pages read); inputs without covering
    /// stats degrade gracefully (all row groups read, exact per-feature
    /// filter still applies).
    #[arg(long, value_name = "XMIN,YMIN,XMAX,YMAX")]
    bbox: Option<String>,

    /// Emit the optional COGP compatibility footer key (partitioning mode).
    #[arg(long)]
    cogp_compat: bool,

    /// Write the JSON conversion report to this path.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

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
    /// Tile the input EXACTLY AS GIVEN: switch the whole generalization
    /// ladder off at every level (#345 / #360).
    ///
    /// The ladder derives coarse levels from the fine input by thinning and
    /// simplifying. That is right for a road network and wrong for a
    /// pre-aggregated grid: an H3 r6 cell is not a simplified r7 cell, it is
    /// their parent, and its count is their sum. Run an aggregate through the
    /// gates and a coarse level shows SOME cells and silently omits the rest,
    /// instead of showing what they sum to.
    ///
    /// Equivalent to --no-density-drop --no-coalesce-lines --simplify-factor 0
    /// with every thinning factor and visibility gate at 0 — a flag set that
    /// was not even reachable before, since a thinning factor of 0 used to be
    /// rejected. Reach for it when the input is already the right resolution
    /// for the zooms you are asking for: DGGS/cell aggregates, pre-levelled
    /// input, or one band of a pyramid.
    ///
    /// Requires --mode duplicating: partitioning writes each feature at one
    /// level, so with thinning off everything lands in the coarsest level.
    ///
    /// On `tiles` it also disables the per-tile size cap (an unbounded
    /// --max-tile-size), since a valve that sheds features to fit a byte
    /// budget is not verbatim either; pass --max-tile-size explicitly to put
    /// a cap back. The two-step form does NOT inherit that — pass
    /// --tile-size-limit 0 to export-pmtiles.
    ///
    /// Supplies DEFAULTS rather than overriding: any knob you set explicitly
    /// wins, so --verbatim --simplify-factor 0.5 is NEARLY verbatim.
    #[arg(long, help_heading = "Thinning & visibility")]
    verbatim: bool,

    /// Column name used as the cell-winner priority (sort) key. Mutually
    /// exclusive with --class-rank.
    #[arg(long, value_name = "COL", help_heading = "Ranking")]
    sort_key: Option<String>,

    /// Magnitude ladder: let COL decide each feature's ENTRY ZOOM (#364).
    ///
    /// Thinning ranks on geometry, which is backwards whenever a dataset's
    /// most important features are its physically smallest — a population
    /// density layer, say, where dense urban tracts are tiny next to sparse
    /// rural ones. Coarse levels then keep the big low-value polygons and drop
    /// the small high-value ones. --sort-key cannot fix that: it chooses
    /// between features competing for a cell, and the visibility gate has
    /// already dropped the small ones on size.
    ///
    /// A ladder ranks COL's DISTINCT values descending and gives each rank an
    /// entry zoom one --ladder-step apart, starting at --min-zoom. A feature
    /// appears from its entry zoom inward and not before, exempt from the
    /// visibility gate and from thinning throughout. Nothing is deleted: the
    /// finest level still carries every feature.
    ///
    /// Ranking DISTINCT values (SQL DENSE_RANK) rather than the values
    /// themselves keeps the ladder scale-free — mapping a raw value onto the
    /// zoom range strands everything in the upper zooms whenever the values
    /// occupy a narrow part of their nominal scale.
    ///
    /// Implies --collapse unless you pass --collapse-square, so a promoted
    /// feature that simplifies below its level's tolerance survives as a
    /// representative point rather than being dropped again.
    ///
    /// Mutually exclusive with --entry-zoom.
    #[arg(long, value_name = "COL", help_heading = "Ranking")]
    magnitude_ladder: Option<String>,

    /// Zooms between consecutive --magnitude-ladder rungs (default 1).
    #[arg(
        long,
        value_name = "N",
        default_value = "1",
        help_heading = "Ranking",
        requires = "magnitude_ladder"
    )]
    ladder_step: u8,

    /// Explicit entry zooms, for full control over the rungs:
    /// `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...` — e.g.
    /// `--entry-zoom "density:5000=4,1000=6,200=8"`.
    ///
    /// Same semantics as --magnitude-ladder but the rungs are placed by hand
    /// rather than derived. Values the spec does not list get no entry zoom
    /// and take the ordinary gate. Mutually exclusive with --magnitude-ladder.
    #[arg(
        long,
        value_name = "SPEC",
        help_heading = "Ranking",
        conflicts_with = "magnitude_ladder"
    )]
    entry_zoom: Option<String>,

    /// Categorical class ranking (higher priority wins a cell). Format:
    /// `COLUMN:VALUE=RANK,VALUE=RANK,...` — e.g.
    /// `--class-rank road_class:motorway=5,primary=4,residential=2`.
    /// Present-but-unlisted values rank below every listed value (but above
    /// nulls). Mutually exclusive with --sort-key.
    #[arg(long, value_name = "SPEC", help_heading = "Ranking")]
    class_rank: Option<String>,

    /// Disable auto-detection of well-known schemas (Overture roads `class`/
    /// `road_class`, Overture places `confidence`).
    #[arg(long, help_heading = "Ranking")]
    no_auto_rank: bool,

    /// Attribute filter: only convert features matching this SQL-WHERE-style
    /// predicate over the input's property columns, e.g.
    /// "confidence > 0.8", "crop_type IN ('soy', 'corn')",
    /// "note IS NOT NULL AND (class = 'a' OR class = 'b')".
    /// Supports =, !=, <, <=, >, >=, IN (...), IS [NOT] NULL, AND/OR/NOT,
    /// parentheses, 'string' and numeric literals, and "quoted column"
    /// names; timestamp columns compare against 'YYYY-MM-DD' /
    /// 'YYYY-MM-DD HH:MM:SS' / RFC 3339 datetime strings (read as UTC);
    /// nulls follow SQL three-valued logic (a row is kept only when
    /// the predicate is TRUE). Evaluated during the pass-1 scan, so it
    /// composes with --bbox; input row groups whose parquet column
    /// statistics preclude any match are skipped at the footer level (on
    /// remote input those byte ranges are never fetched). Aliased as
    /// --where. See docs/OVERVIEW_TUNING.md.
    #[arg(long, value_name = "EXPR", alias = "where", help_heading = "Filtering")]
    filter: Option<String>,

    /// Keep ONLY these property columns (repeatable; tippecanoe -y). Every
    /// other property is dropped at scan time: the excluded columns are not
    /// decoded and the overview file only carries what was asked for. The
    /// geometry column is always kept. A column another knob
    /// reads (--sort-key, --filter, --accumulate-attribute,
    /// --magnitude-ladder, --class-rank) must stay included; naming a column
    /// the input does not have is an error
    #[arg(long, value_name = "COL", help_heading = "Properties")]
    include_property: Vec<String>,

    /// Drop these property columns (repeatable; tippecanoe -x). Ignored when
    /// --include-property is given, as in tippecanoe. Naming a column the
    /// input does not have only warns
    #[arg(long, value_name = "COL", help_heading = "Properties")]
    exclude_property: Vec<String>,

    /// Drop every property column (tippecanoe -X): geometry-only output.
    /// Ignored when --include-property is given, as in tippecanoe
    #[arg(long, help_heading = "Properties")]
    exclude_all_properties: bool,

    /// GSD tile-band base for the zoom→GSD mapping: gsd(z) = 40075016.69 /
    /// base / 2^z (spec §5.2, cogp-rs default 1024).
    ///
    /// This is the master detail knob for a zoom-range plan. A LARGER base
    /// makes every level's GSD SMALLER, so less is thinned and simplified at a
    /// given zoom (denser, more detailed, larger coarse levels). A SMALLER
    /// base makes GSDs LARGER (sparser, cruder, cheaper coarse levels). It
    /// scales the whole ladder at once, whereas --simplify-factor and the
    /// --*-thinning knobs act relative to each level's GSD. No effect when
    /// --gsd is given (those GSDs are already absolute meters).
    ///
    /// Cheat sheet: coarse levels too sparse → RAISE --gsd-base (or lower the
    /// thinning factors); too crude → lower --simplify-factor. See
    /// docs/OVERVIEW_TUNING.md.
    #[arg(
        long,
        value_name = "F",
        default_value = "1024.0",
        help_heading = "Generalization"
    )]
    gsd_base: f64,

    /// Simplification tolerance factor: RDP tolerance = factor * gsd (meters),
    /// duplicating mode only (default 1.0).
    ///
    /// Controls how much per-feature vertex detail each coarse level sheds.
    /// LOWER = smoother/less aggressive = more vertices kept = crisper but
    /// heavier levels; HIGHER = cruder = fewer vertices = lighter levels. The
    /// canonical (finest) level is always verbatim regardless. A line/polygon
    /// whose bbox diagonal is below the tolerance is dropped entirely, so a
    /// very high factor also thins features, not just vertices.
    ///
    /// Cheat sheet: coarse levels look too crude/blocky → LOWER
    /// --simplify-factor. See docs/OVERVIEW_TUNING.md.
    #[arg(long, help_heading = "Generalization")]
    simplify_factor: Option<f64>,

    /// Collapse below-visibility polygons to a representative point instead of
    /// dropping them (spec Q4 opt-in). Changes the geometry type at coarse
    /// levels (fill-styled renderers silently ignore points — add a circle
    /// layer, or use --collapse-square to stay type-preserving).
    #[arg(long, help_heading = "Generalization")]
    collapse: bool,

    /// Stand in for the polygons a coarse level drops with ~1xGSD placeholder
    /// SQUARES, so the level still shows where the area is (tippecanoe
    /// tiny-polygon reduction; opt-in).
    ///
    /// Two mechanisms, one threshold T = (simplify-factor * gsd)^2 (#384):
    /// every polygon the level does NOT carry — failed the visibility gate,
    /// lost its thinning cell, cut by the density budget — adds its area to
    /// an accumulator for its 32xGSD patch, and each time a patch's total
    /// crosses T the polygon that crossed it is emitted as a T-area square
    /// with its own attributes; a polygon the level DOES carry but that
    /// collapses below T at write time survives as a square with probability
    /// A/T. Either way aggregate area stays truthful: a country of 25 m
    /// fields reads as farmland at z0 instead of vanishing, and dense blocks
    /// read denser than isolated barns. Type-preserving (the output stays
    /// Polygon), so plain fill styles keep working, unlike --collapse.
    /// Deterministic (same input -> same output, engine- and
    /// thread-independent). Duplicating mode only. See
    /// docs/OVERVIEW_TUNING.md.
    #[arg(long, conflicts_with = "collapse", help_heading = "Generalization")]
    collapse_square: bool,

    /// Zoom-band representation selector: comma-separated LO-HI:KIND bands,
    /// e.g. "0-7:point,8-14:geom" or "0-5:square". KIND is geom, point, or
    /// square.
    ///
    /// point: ALL polygonal features in the band become representative
    /// points (centroid) — "dots zoomed out, polygons zoomed in" in ONE
    /// archive, no two-archive merge. In-band polygons bypass the visibility
    /// gate (a dot is always visible) and thin on the point grid.
    /// square: below-tolerance polygons in the band emit area-dithered
    /// ~1xGSD placeholder squares (see --collapse-square) instead of
    /// dropping; visible polygons are untouched. geom: normal (the default
    /// for unlisted zooms). Bands must not overlap, non-geom bands must end
    /// before --max-zoom (the canonical level is always verbatim), and point
    /// bands must be contiguous from the coarsest zoom. Requires a zoom-range
    /// plan (not --gsd) and duplicating mode. Lines and native points are
    /// unaffected by every band kind. See docs/OVERVIEW_TUNING.md.
    #[arg(long, value_name = "SPEC", help_heading = "Generalization")]
    representation: Option<String>,

    /// Disable cascading simplification (#218) and reproduce the pre-cascade
    /// output byte-for-byte.
    ///
    /// By default each coarser level is simplified from the next-finer
    /// level's already-simplified output (tippecanoe-style) and invalid RDP
    /// candidates are repaired via a boolean overlay instead of epsilon-
    /// retried — much faster on duplicating mode, at the cost of coarse-level
    /// coordinates differing slightly from the non-cascaded pipeline (bounded
    /// by ~2x the level tolerance). See docs/OVERVIEW_TUNING.md.
    #[arg(long, help_heading = "Generalization")]
    no_cascade: bool,

    /// Point thinning factor: grid cell size = factor * gsd.
    ///
    /// Default 4.0, or 16.0 when --cluster is enabled (absorbed points are
    /// summarized via point_count rather than dropped, so a coarser grid
    /// gives the familiar graduated-cluster look; chosen from the NYC
    /// pt={4,16,48} sweep).
    ///
    /// One feature survives per grid cell per level, so BIGGER factor = BIGGER
    /// cells = FEWER survivors = SPARSER map; SMALLER = denser. This multiplies
    /// the GSD cell size, so it interacts with --gsd-base (which sets the GSD).
    ///
    /// Cheat sheet: coarse levels too sparse → LOWER the thinning factors.
    #[arg(long, help_heading = "Thinning & visibility")]
    point_thinning: Option<f64>,

    /// Line thinning factor: grid cell size = factor * gsd (default 1.0).
    ///
    /// BIGGER = SPARSER (fewer lines survive per level), SMALLER = denser.
    /// See --point-thinning; this is the roads/line knob. Default retuned
    /// 2.0 -> 1.0 after the Portland sweep (corpus/SWEEPS.md): 1.0
    /// keeps road networks visibly more continuous at coarse zooms.
    #[arg(long, help_heading = "Thinning & visibility")]
    line_thinning: Option<f64>,

    /// Polygon thinning factor: grid cell size = factor * gsd (default 1.0).
    ///
    /// BIGGER = SPARSER, SMALLER = denser. Polygons thin least by default
    /// (1.0) since they tile space rather than cluster.
    #[arg(long, help_heading = "Thinning & visibility")]
    polygon_thinning: Option<f64>,

    /// Line visibility gate in GSD multiples: a line is eligible at a level
    /// only if its bbox diagonal >= factor * gsd (default 2.0).
    ///
    /// This is a hard drop, not a thin: BIGGER = more small lines dropped at
    /// coarse levels (sparser); SMALLER = more small lines kept. The gate is
    /// multiplied by the level GSD, so --gsd-base moves it too.
    #[arg(long, help_heading = "Thinning & visibility")]
    line_visibility: Option<f64>,

    /// Polygon visibility gate in GSD multiples: a polygon is eligible only if
    /// its bbox diagonal >= factor * gsd (default 2.0).
    ///
    /// BIGGER = more small polygons dropped at coarse levels (sparser);
    /// SMALLER = more kept. See --line-visibility. Retuned 4.0 -> 2.0 in the
    /// #259 coarse-zoom sweep (corpus/SWEEPS.md Decision 6): write-time RDP
    /// already drops polygons that simplify below the level tolerance, so
    /// gates above 2.0 starve coarse zooms without making files smaller,
    /// and gates below ~2.0 mostly admit candidates that RDP drops anyway
    /// (use --collapse to keep those as representative points).
    #[arg(long, help_heading = "Thinning & visibility")]
    polygon_visibility: Option<f64>,

    /// Per-level density drop rate: each coarser level keeps 1/rate of the
    /// next finer level's feature budget (default 1.65).
    ///
    /// This is the Q2 knob that stops mid-zoom counts plateauing at ~everything.
    /// Cell-winner thinning stops binding once its grid cell is smaller than the
    /// typical feature spacing, so from ~z9 up every feature survives and coarse
    /// levels over-retain (Portland roads: ours/tippecanoe ≈ 2–3x at z9–z11).
    /// After cell-winner thinning, each level is capped at a budget that decays
    /// geometrically toward coarse zooms — budget(L) = N / rate^(finest−L),
    /// where N is the input feature count — and the lowest-priority survivors
    /// (same class-rank → size → hash order as the cell-winner, spec Q1) are
    /// dropped until the level meets its budget. Levels already sparser than
    /// their budget (the coarse zooms) are untouched, so this only bites the
    /// mid-zoom plateau. BIGGER rate = coarser levels shed harder (sparser mid
    /// zooms, smaller files); SMALLER = gentler. The default 1.65 is smaller than
    /// tippecanoe's nominal 2.5 because our budget anchors on the full canonical
    /// count N (every feature appears at the finest level), not a per-tile
    /// basezoom count. The canonical (finest) level is never dropped. See
    /// docs/OVERVIEW_TUNING.md and corpus/SWEEPS.md.
    #[arg(
        long,
        value_name = "F",
        default_value = "1.65",
        help_heading = "Density budget"
    )]
    drop_rate: f64,

    /// Spatial-fairness strength for the density budget (default 1.5).
    ///
    /// The budget is shared across coarse super-cells (neighborhoods) so a
    /// global rank-ordered cut cannot empty sparse rural areas to keep dense
    /// cities under budget. Each super-cell keeps its top-priority features up
    /// to an allocation proportional to population^(1/gamma): gamma=1 is a
    /// proportional cut (every neighborhood keeps the same fraction); gamma>1 is
    /// SUBLINEAR — dense neighborhoods keep proportionally fewer, sparse ones
    /// proportionally more (they are protected). This is tippecanoe's gamma
    /// dot-dropping ("reduce dots to the 1/gamma power in dense areas") applied
    /// per super-cell. BIGGER = more protection for sparse areas / harder
    /// relative thinning of dense areas. Does not change per-level totals (it
    /// only redistributes which features survive spatially), so it is
    /// independent of --drop-rate. No effect when --no-density-drop is set.
    #[arg(
        long,
        value_name = "F",
        default_value = "1.5",
        help_heading = "Density budget"
    )]
    drop_gamma: f64,

    /// Disable the Q2 per-level density budget entirely (off switch).
    ///
    /// Reverts to pure cell-winner thinning — the pre-Q2 behavior — and emits a
    /// byte-identical footer (no density_drop provenance). Use this to compare
    /// before/after, or when the cell-winner thinning already meets your needs.
    #[arg(long, help_heading = "Density budget")]
    no_density_drop: bool,

    /// Enable point clustering (duplicating mode only; opt-in).
    ///
    /// At each overview level, the surviving point in each thinning grid cell
    /// ABSORBS the other points in its cell instead of them simply vanishing:
    /// the output gains a `point_count` INT64 NOT NULL column recording how
    /// many source features each row represents at its level (tippecanoe /
    /// supercluster convention; always 1 at the canonical level). The winner
    /// keeps its own geometry and attribute values. Lines and polygons are
    /// unaffected (their rows carry point_count = 1). Use for graduated-dot
    /// rendering of dense point data. See docs/OVERVIEW_TUNING.md.
    #[arg(long, help_heading = "Clustering")]
    cluster: bool,

    /// Aggregate a numeric column across clustered points: COL:OP where OP is
    /// sum, max, min, or mean. Repeatable. Requires --cluster.
    ///
    /// At each level the winner's value of COL becomes the aggregate over
    /// itself + the points it absorbed at that level (computed per level from
    /// SOURCE values — mean is exact, never a mean of means). All other
    /// columns keep the winner's own values. Example:
    /// --accumulate-attribute population:sum
    /// --accumulate-attribute confidence:mean
    #[arg(
        long = "accumulate-attribute",
        value_name = "COL:OP",
        help_heading = "Clustering"
    )]
    accumulate_attribute: Vec<String>,

    /// Disable line network coalescing (ON by default; duplicating mode).
    ///
    /// By default, at each non-canonical level touching same-class line
    /// segments are chained into single "stroke" LineStrings BEFORE the
    /// visibility gate and thinning run, so a chain of individually
    /// sub-visibility fragments survives as one long, connected artery —
    /// road/river networks read as continuous lines at coarse zooms instead
    /// of scattered dashes. Chains never merge across class values (when a
    /// class ranking is active); junctions continue only within
    /// --coalesce-junction-angle of straight. The merged feature keeps the
    /// attributes of its highest-priority member, and the output gains a
    /// `coalesced_count` INT32 NOT NULL column (source segments merged per
    /// row; 1 for unmerged rows and everywhere at the canonical level;
    /// withheld from tiles when it is 1 everywhere). Points and polygons are
    /// unaffected. In partitioning mode coalescing
    /// is inert (a merged chain cannot satisfy the feature-once/verbatim
    /// contract). See docs/OVERVIEW_TUNING.md.
    #[arg(long, help_heading = "Line coalescing")]
    no_coalesce_lines: bool,

    /// Deprecated no-op: coalescing is now the default. Kept so existing
    /// invocations keep working; rejected with partitioning mode (where the
    /// default silently disables instead).
    #[arg(long, hide = true, conflicts_with = "no_coalesce_lines")]
    coalesce_lines: bool,

    /// Junction continuation angle for line coalescing, in degrees
    /// (default 0 = OFF: junctions terminate chains, preserving network
    /// topology — chosen from the Portland junction-angle sweep in
    /// corpus/data/bench/q3/, where strict degree-2 chaining rendered
    /// better).
    ///
    /// When > 0: at a junction (3+ same-class segment endpoints meeting),
    /// the pair of lines that best continue each other merge when their
    /// deviation from a straight continuation is at most this angle — best
    /// pair first, so a 4-way crossing continues BOTH through-streets.
    /// BIGGER = chains bend further through junctions (longer, fewer
    /// strokes; risk of merging through genuine turns).
    #[arg(
        long,
        value_name = "DEG",
        default_value = "0.0",
        help_heading = "Line coalescing"
    )]
    coalesce_junction_angle: f64,

    /// Endpoint snap tolerance for line coalescing, in GSD multiples
    /// (default 1.0).
    ///
    /// Exactly-touching endpoints always chain; this knob additionally joins
    /// chain ends within factor * gsd of each other (two endpoints closer
    /// than one ground sample are indistinguishable at that level). BIGGER =
    /// bridges larger digitization gaps (risk: rungs of nearby parallel
    /// lines fusing); 0 = exact endpoint matching only.
    #[arg(
        long,
        value_name = "F",
        default_value = "1.0",
        help_heading = "Line coalescing"
    )]
    coalesce_snap: f64,

    /// Per-level candidate-line ceiling for line coalescing (memory guard).
    ///
    /// Chaining holds the level's candidate line geometries in memory at
    /// once (every line is a candidate at every non-canonical level, since
    /// sub-visibility fragments must be reclaimable). Datasets with more
    /// lines than this skip coalescing with a warning instead of breaking
    /// the streaming pipeline's memory bound; near-canonical levels that
    /// large need coalescing least (segments are individually visible).
    #[arg(
        long,
        value_name = "ROWS",
        default_value = "2000000",
        help_heading = "Line coalescing"
    )]
    coalesce_max_level_rows: usize,

    /// Maximum output row-group size in rows.
    ///
    /// Interpreted per level: a level with at most this many rows is written as
    /// a single row group; a larger level is split into roughly uniform row
    /// groups of at most this size. Coarse bands (few features) therefore become
    /// one broad row group; fine bands keep tight per-row-group bbox statistics.
    ///
    /// This is a request, not a guarantee: it may be raised automatically to fit
    /// parquet's row-group ceiling of 32,768 groups per file (a warning says so,
    /// and the conversion report records the cap actually used).
    #[arg(long, default_value = "10000", help_heading = "Output layout")]
    row_group_size: usize,

    /// Per-level row-group sizing policy (#202).
    ///
    /// `constant`: every level uses --row-group-size as its cap (default).
    /// `zoom-scaled`: the cap doubles per zoom step below the finest level
    /// (cap = row_group_size << (max_zoom - level_zoom)) — coarse bands, which
    /// wide viewports read mostly whole anyway, become fewer/larger row groups
    /// (fewer remote requests) while the finest level keeps tight bbox pruning.
    #[arg(
        long,
        default_value = "constant",
        value_parser = ["constant", "zoom-scaled"],
        help_heading = "Output layout"
    )]
    row_group_size_policy: String,

    /// Keep full Parquet statistics on every column, including high-cardinality
    /// string/binary property columns and the WKB geometry column.
    ///
    /// By default those columns' per-row-group min/max stats are suppressed to
    /// keep the footer small (a 26-char ULID `id` over hundreds of row groups
    /// otherwise bloats the footer to megabytes, paid on every remote query).
    /// The bbox covering and `level` column always keep their pruning stats.
    /// Enable this if remote clients push predicates on property columns and
    /// want row-group skipping on them.
    #[arg(long, help_heading = "Output layout")]
    full_column_stats: bool,

    /// Disable the two-pass bounded-memory streaming pipeline (H3).
    ///
    /// By default the converter streams the input twice: pass 1 builds the
    /// per-feature winner tables (level assignment + density budget) holding
    /// only bboxes/kinds/sort-keys; pass 2 re-reads the input per level and
    /// simplifies + writes batch-by-batch. Peak memory is O(read batch +
    /// winner tables) instead of O(dataset) — e.g. Moldova (632k polygons)
    /// drops from ~5.4 GB to well under 1 GB peak RSS. Output is equivalent
    /// (same level assignments, rows, and footer). This flag reverts to the
    /// original in-memory pipeline, which decodes the whole dataset once and
    /// may be marginally faster on small inputs that comfortably fit in RAM.
    #[arg(long, help_heading = "Memory & performance")]
    no_streaming: bool,

    /// Rows per Arrow read batch in the streaming pipeline (both passes).
    ///
    /// LARGER batches amortize per-batch overhead (slightly faster) at the
    /// cost of proportionally more peak memory; SMALLER batches bound memory
    /// tighter. The default (8192) keeps per-batch transients in the tens of
    /// MB even for vertex-heavy polygon data. Capped at 1048576 rows. No
    /// effect with --no-streaming.
    #[arg(
        long,
        value_name = "ROWS",
        default_value = "8192",
        value_parser = parse_read_batch_size,
        help_heading = "Memory & performance"
    )]
    read_batch_size: usize,

    /// Memory/throughput profile for the single-read pass-2 engine (#213/#212).
    ///
    /// `speed` buffers each output level's rows in RAM (fastest; peak RAM grows
    /// with buffered output). `bounded` spills them to temporary Arrow IPC
    /// files (memory-capped; slight temp-I/O cost). `auto` (default) is
    /// workload-based: it estimates buffered output from feature and level
    /// counts and spills when that exceeds a fraction of available RAM
    /// (container-aware: cgroup v2/v1 limits are respected), so large
    /// duplicating runs prefer bounded instead of risking OOM (override the RAM
    /// figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES). Output is byte-identical
    /// across profiles. No effect with --no-streaming.
    #[arg(
        long,
        default_value = "auto",
        value_parser = ["auto", "speed", "bounded"],
        help_heading = "Memory & performance"
    )]
    profile: String,

    /// Read batches allowed in flight through the streaming pipeline at once
    /// (read/compute-overlap knob; bounded-channel depth) — pass 1's scan and
    /// pass 2's per-level fan-out both use it.
    ///
    /// `auto` (the default) sizes this to the machine's available cores
    /// (clamped to 4..=16); pass an explicit integer to override. Higher
    /// improves core utilization on long-pole geometries at proportionally
    /// more peak memory (in-flight-batches × read-batch-size rows resident,
    /// PER PASS — passes 1 and 2 never run concurrently, so this does not
    /// double). This is no longer the only resident-batch term: pass 2's
    /// readers hold their own read-ahead on top (--read-workers × its queue
    /// depth), and under --profile bounded each level's spill writer holds up
    /// to 3 more. The chosen depth and detected core count are logged at the
    /// start of each pass. No effect with --no-streaming.
    #[arg(
        long,
        value_name = "N|auto",
        default_value = "auto",
        value_parser = parse_in_flight_batches,
        help_heading = "Memory & performance"
    )]
    in_flight_batches: usize,

    /// Reader threads pass 2 splits the input across (issue #494).
    ///
    /// Parquet row groups are independently readable, so pass 2 can read the
    /// input with several threads at once and merge their batches back into
    /// read order. `auto` (the default) takes a quarter of the machine's
    /// cores, capped at 4 — readers decompress and decode, so they compete
    /// with the pool doing the simplification they are feeding. `1` is the
    /// single sequential reader. An explicit value is honoured up to a ceiling
    /// of 2× this machine's cores (at least 4); above that it is rejected.
    ///
    /// Output is byte-identical for every value: workers own disjoint runs of
    /// row groups and the merge reproduces exactly the batch sequence one
    /// reader would have produced.
    ///
    /// Remote inputs always read sequentially (concurrent readers over one
    /// remote source evict each other's fetched chunks) — including a remote
    /// input the run has staged to local disk, since staging is per-part and
    /// the source stays remote. The read-ahead is sized against a modelled
    /// slice of the same memory budget the pass-2 sink uses (and each worker's
    /// queue is capped shallower under --profile bounded), so a small box, or a
    /// wide input, quietly gets fewer workers. Helps most when the input's row
    /// groups are small relative to that budget — `gpio` writes well-sized
    /// ones.
    #[arg(
        long,
        value_name = "N|auto",
        default_value = "auto",
        value_parser = parse_read_workers,
        help_heading = "Memory & performance"
    )]
    read_workers: usize,

    /// Directory for the remote-input spill file (issues #219/#272).
    ///
    /// A remote convert stages every fetched column chunk in an anonymous
    /// temp file — growing to ≈1× the touched input bytes (the whole object
    /// for a full-file convert; only the covering row groups with --bbox) —
    /// so later passes re-read from local disk instead of the network. By
    /// default it lives under the process temp dir ($TMPDIR); point this at
    /// a volume with enough room (a free-space preflight warns about a
    /// projected shortfall). The directory must exist. Local inputs never
    /// spill.
    ///
    /// On `tiles` this directory also hosts the removed-after-export
    /// intermediate overview (#314) — at least input-sized, with its own
    /// free-space preflight — unless --keep-overview is given (then the
    /// intermediate goes to that path instead). Location precedence for
    /// the intermediate: --spill-dir, $TMPDIR, the output directory.
    #[arg(long, value_name = "PATH", help_heading = "Memory & performance")]
    spill_dir: Option<PathBuf>,

    /// Write the convert plan artifact to PATH, then carry on converting.
    ///
    /// The plan is the complete result of pass 1 and the level assignment:
    /// which level every input row enters at, the per-level counts, the
    /// clustering / coalescing / carrier side tables, the resolved ranking
    /// provenance and the dataset-wide tallies — plus a fingerprint of the
    /// inputs and of every thinning-relevant flag (see --plan for exactly
    /// what the fingerprint pins). Re-running with --plan then skips pass 1
    /// and the assignment entirely.
    ///
    /// The assignment is dataset-global: the density budget water-fills a
    /// super-cell budget over every candidate of a level, the level walk
    /// carries a running kept count coarse to fine, and --magnitude-ladder
    /// dense-ranks the whole column. A sharded build must therefore consume
    /// ONE plan rather than recompute the assignment per shard, or the
    /// shards' pyramids disagree.
    ///
    /// PATH's parent directory must exist and be writable; that is checked
    /// up front, before anything is scanned. An existing plan at PATH is
    /// overwritten, with a log line.
    #[arg(
        long,
        value_name = "PATH",
        conflicts_with = "plan",
        help_heading = "Memory & performance"
    )]
    save_plan: Option<PathBuf>,

    /// Reuse the convert plan artifact at PATH instead of running pass 1 and
    /// the level assignment.
    ///
    /// The plan's fingerprint must match this run — the tylertoo version,
    /// every thinning-relevant flag, and each input's identity. A mismatch is
    /// a hard error naming the offending field, so a stale plan never
    /// silently produces a different pyramid. The write-side flags
    /// (--profile, --row-group-size, --in-flight-batches, --spill-dir) are
    /// deliberately NOT fingerprinted, so one plan can be replayed across
    /// them.
    ///
    /// What "input identity" pins: for every part, local or remote, the
    /// path/URL, the byte size, the row count and the row-group layout from
    /// the parquet footer, plus the row groups --bbox/--filter pruned to.
    /// A local part additionally pins its mtime. A remote object has no
    /// mtime and no ETag here, so an object rewritten in place with the same
    /// size, row count and row-group layout is NOT detected; tylertoo warns
    /// when any part is remote.
    ///
    /// PATH must be a readable convert plan; that is checked up front,
    /// before the input is opened.
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

/// Arguments for `tylertoo tiles` — the one-shot GeoParquet → PMTiles facade.
///
/// This is a thin wrapper that runs `overview` (convert) into a temporary
/// GeoParquet file and then `export-pmtiles` from it. The full convert-tuning
/// set (ranking, generalization, thinning/visibility, density budget,
/// clustering, coalescing, memory/performance) is flattened in below, so a
/// one-shot `tiles` run reaches the same quality and memory levers as the
/// two-step chain — see `--help` for the grouped flags. The legacy per-tile
/// pipeline this command used to run was removed (see issue #177).
#[derive(Parser, Debug)]
struct TilesArgs {
    /// Input GeoParquet (EPSG:4326 or EPSG:3857): a local file, a directory
    /// or glob of partitions, or a remote URL (s3://, https://, gs://).
    /// s3://.../ and gs://.../ prefixes (trailing slash) are listed to their
    /// .parquet objects; remote inputs are read with byte-range requests.
    /// Omit when --files-from is given (then the one positional is OUTPUT).
    #[arg(value_name = "INPUT", required_unless_present = "files_from")]
    input: Option<PathBuf>,

    /// Output PMTiles file.
    #[arg(value_name = "OUTPUT", required_unless_present = "files_from")]
    output: Option<PathBuf>,

    /// Convert the inputs listed in this manifest instead of a positional
    /// INPUT: one local path or remote URL per line; `#` comment lines and
    /// blank lines are skipped; line order is preserved VERBATIM (it defines
    /// the dataset row order). Each line must be a single .parquet
    /// file/object — no directories, globs, or prefixes. Local and remote
    /// entries may be mixed. Usage: --files-from <PATH> OUTPUT.
    #[arg(long, value_name = "PATH")]
    files_from: Option<PathBuf>,

    /// Minimum (coarsest) Web Mercator zoom level.
    #[arg(long, default_value = "0")]
    min_zoom: u8,

    /// Maximum (finest) Web Mercator zoom level.
    #[arg(long, default_value = "14")]
    max_zoom: u8,

    /// Explicit comma-separated GSD list (meters, strictly decreasing).
    /// Overrides --min-zoom/--max-zoom when set — the same semantics as
    /// `tylertoo overview --gsd`, so the absolute-GSD ladder is reachable in
    /// one step.
    #[arg(long, value_name = "GSDS")]
    gsd: Option<String>,

    /// Regional extract: only convert features whose bbox intersects this
    /// bounding box (lon/lat degrees: xmin,ymin,xmax,ymax). See --bbox in
    /// `tylertoo overview --help` for details.
    #[arg(long, value_name = "XMIN,YMIN,XMAX,YMAX")]
    bbox: Option<String>,

    /// Layer name for the output tiles (default: derived from input filename).
    #[arg(long)]
    layer_name: Option<String>,

    /// Maximum tile size (e.g., "500K", "1M", or raw bytes). When a tile
    /// exceeds this limit, the export sheds features in a single non-iterative
    /// pass (largest-first for polygons/lines; a uniform spatial stride for
    /// point tiles). Defaults to 500K (tippecanoe parity, #280); pass 0 to
    /// disable the cap. Aliased as --tile-size-limit for parity with
    /// `export-pmtiles`.
    /// With --verbatim and no explicit value, the cap is disabled: a valve
    /// that sheds features to fit a byte budget is not verbatim either.
    #[arg(long, value_name = "SIZE", alias = "tile-size-limit", value_parser = parse_size_bytes)]
    max_tile_size: Option<usize>,

    /// Disable the simple-clip fast path (issue #239), forcing the i_overlay
    /// boundary-bridge fallback on every polygon clip. The fast path is on by
    /// default (render-equivalent on simple rings); pass this only when you need
    /// byte-stable tile output, since the fast path rotates simple rings to a
    /// different start vertex.
    #[arg(long)]
    no_simple_clip_fastpath: bool,

    /// Per-tile edge buffer, in tile pixels, carried across tile seams so
    /// features don't clip at boundaries.
    #[arg(long, default_value = "8")]
    tile_buffer: u32,

    /// Partitions processed per band read during the export phase (the export
    /// concurrency knob). `auto` (the default) preflights a memory budget:
    /// the machine's core count, capped by how many estimated per-partition
    /// transients fit in a fraction of available RAM (container-aware: cgroup
    /// v2/v1 limits are respected; floor 6; fixed cap 16 only when RAM cannot
    /// be probed; override the RAM figure with TYLERTOO_AUTO_MEM_LIMIT_BYTES).
    /// Pass an explicit integer to override.
    /// Wider waves keep more cores busy at proportionally more peak memory
    /// (one wave of partitions resident). The chosen width and the preflight
    /// inputs are logged at export start. Output is byte-identical for every
    /// value.
    #[arg(long, value_name = "N|auto", default_value = "auto", value_parser = parse_partition_wave)]
    partition_wave: usize,

    /// Within-tile feature order (#361): `input` (default) or a property
    /// name, optionally `:asc` / `:desc`.
    ///
    /// MVT does not define draw order, but renderers paint features in the
    /// order the tile lists them, so this is the paint order for any style
    /// that does not override it. `input` emits source row order. Naming a
    /// column sorts within each tile by that property — `--feature-order
    /// level` puts high `level` on top, which is what a nested choropleth
    /// usually wants — with ties kept in input order so output stays
    /// deterministic.
    #[arg(long, value_name = "input|COLUMN[:asc|:desc]", default_value = "input")]
    feature_order: FeatureOrder,

    /// Write a JSON report to this path: a combined object with a `convert`
    /// section (the overview build, matching `overview --report`) and an
    /// `export` section (the PMTiles export, matching `export-pmtiles
    /// --report`), so the one-step run captures both halves the two-step
    /// chain would.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

    /// Write the intermediate overview GeoParquet to PATH and RETAIN it,
    /// instead of a temp file removed after the export — one run then yields
    /// both artifacts: the reusable multi-resolution overview (queryable,
    /// re-exportable, see `tylertoo overview`) and the PMTiles. The PMTiles
    /// output is identical either way. Without this flag the intermediate is
    /// written to --spill-dir if given, else $TMPDIR if set, else the output
    /// directory, and deleted once the export finishes (see the note on the
    /// materialized intermediate under --spill-dir).
    #[arg(long, value_name = "PATH")]
    keep_overview: Option<PathBuf>,

    /// Build one job of a sharded fleet (#498): `I/N` for data shard I of N,
    /// or `coarse` for the job that owns the zooms coarser than the pivot.
    ///
    /// Requires --shard-plan. A data shard additionally requires --plan: the
    /// level assignment is dataset-global (the density budget water-fills a
    /// super-cell over every candidate of a level, the level walk carries a
    /// running kept count, --magnitude-ladder dense-ranks the whole column),
    /// so a shard that recomputed it over its own rows would disagree with
    /// its siblings and the seams would not line up. The coarse job, which
    /// reads the whole input anyway, is the run that writes that plan with
    /// --save-plan.
    ///
    /// A data shard reads only the row groups whose bbox reaches its range
    /// and emits only the tiles its range owns. Features are kept WHOLE: one
    /// straddling a seam is read and clipped by both neighbours, and each
    /// emits only its own tiles — so there is no border double-inclusion to
    /// dedup afterwards, and the merge is a blob copy.
    ///
    /// The three build steps, after `tylertoo shard-plan` has cut the plan:
    /// (1) `--shard coarse --shard-plan shards.json --save-plan convert.plan`;
    /// (2) one job per shard, `--shard $i/16 --shard-plan shards.json --plan
    /// convert.plan`; (3) `tylertoo merge out.pmtiles coarse.pmtiles
    /// shard-*.pmtiles`. See the Sharded builds guide for the full recipe
    #[arg(
        long,
        value_name = "I/N|coarse",
        requires = "shard_plan",
        help_heading = "Sharded builds"
    )]
    shard: Option<String>,

    /// The shard plan every job of the fleet shares, from `tylertoo
    /// shard-plan`. It fixes the pivot zoom and the N tile-id ranges, so the
    /// jobs agree on the cut by construction
    #[arg(long, value_name = "PATH", help_heading = "Sharded builds")]
    shard_plan: Option<PathBuf>,

    /// Emit only the tiles in this PMTiles tile-id range: `LO..HI`, two tile
    /// ids AT THE SAME ZOOM (#498). The manual form of a shard's restriction,
    /// for a cut you want to choose yourself.
    ///
    /// That zoom is the pivot, and the range owns every descendant of those
    /// tiles at every deeper zoom; tiles coarser than the pivot are outside it
    /// and are not emitted. Ranges that partition the pivot zoom partition
    /// every deeper zoom, so the archives are disjoint by construction and
    /// `tylertoo merge` concatenates them without re-encoding.
    ///
    /// This restricts the EXPORT only. `--shard` is the form that also prunes
    /// the convert's input to the row groups the range can reach, which is
    /// what makes a shard cheaper than the whole build rather than merely
    /// narrower — prefer it unless you are cutting by hand. The two are
    /// mutually exclusive: `--shard` derives its range from the shard plan
    #[arg(
        long,
        value_name = "LO..HI",
        conflicts_with = "shard",
        help_heading = "Sharded builds"
    )]
    tile_range: Option<String>,

    /// Enable verbose output (per-level and per-zoom breakdowns).
    #[arg(short, long)]
    verbose: bool,

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
    const SUBCOMMANDS: [&str; 10] = [
        "tiles",
        "overview",
        "validate",
        "export-pmtiles",
        "decode",
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
    if matches!(role, ShardRole::Coarse) {
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

fn run_tiles(args: TilesArgs) -> Result<()> {
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};
    use tylertoo_core::overview::level::Mode;

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let (spec, output) = resolve_io(args.input, args.output, args.files_from)?;

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

    let bbox = args.bbox.as_ref().map(|s| parse_bbox(s)).transpose()?;

    // Overviews for PMTiles are always duplicating (partitioning can't be
    // exported to per-tile MVT). Every other convert knob comes from the
    // shared tuning set, so `tiles` matches the two-step overview → export.
    let levels = resolve_level_plan(args.gsd.as_deref(), args.min_zoom, args.max_zoom)?;
    let mut options = args
        .tuning
        .build_convert_options(Mode::Duplicating, levels, bbox, false)?;

    // #498: which job of a sharded fleet this is, and the slice of the tile
    // space it owns. Resolved before any work, against the plan every job of
    // the fleet shares, so a mis-specified `--shard 4/8` against a 16-way plan
    // fails in milliseconds rather than after an hour of tiling.
    let shard = resolve_shard_job(
        &spec,
        args.shard.as_deref(),
        args.shard_plan.as_deref(),
        args.min_zoom,
        args.max_zoom,
    )?;
    // A data shard's range prunes the convert's reads as well as the export;
    // a hand-written `--tile-range` restricts the export only (the two flags
    // conflict, so at most one is set).
    let tile_range = match (&shard, &args.tile_range) {
        (Some(job), _) => job.range,
        (None, Some(text)) => Some(tylertoo_core::shard::TileRange::parse(text)?),
        (None, None) => None,
    };
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
        if job.range.is_none() && args.gsd.is_none() {
            options.zoom_ceiling = Some(job.pivot.saturating_sub(1));
        }
        log_shard_job(job, args.min_zoom, args.max_zoom);
    }

    // The data shard's own range, kept past `tile_range`'s move into
    // `ExportOptions`: the empty-shard branch below needs the pivot zoom.
    let shard_range = shard.as_ref().and_then(|job| job.range);

    // #386: the property selection is applied at convert, so an excluded
    // column is already gone from the intermediate when export would sort
    // on it — export-pmtiles rejects that pairing outright, and so must the
    // facade, before any work is done, rather than run the whole convert
    // and then quietly fall back to input order. Same wording as export's
    // `PropertyRequiredByKnob`.
    if let FeatureOrder::Column { name, .. } = &args.feature_order {
        anyhow::ensure!(
            options.properties.keeps(name),
            "property {name:?} is excluded but --feature-order reads it; keep it in the \
             selection or drop the knob"
        );
    }

    // Intermediate overview file (#314): retained at --keep-overview when
    // given; otherwise a temp file in --spill-dir / $TMPDIR / the output
    // directory, removed on drop — success or failure alike.
    let (overview_path, overview_tmp): (PathBuf, Option<tempfile::NamedTempFile>) =
        match &args.keep_overview {
            Some(path) => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    anyhow::ensure!(
                        parent.is_dir(),
                        "--keep-overview directory {} does not exist",
                        parent.display()
                    );
                }
                (path.clone(), None)
            }
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
            return write_empty_shard(&output, &layer_name, job, range, args.max_zoom, &e);
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
        // #380: the archive declares the zoom range that was asked for, even
        // when the coarsest levels generalized to nothing and were omitted
        // from the overview. Only a zoom plan has a requested minimum zoom.
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
            convert_report.out_of_range_features,
            convert_report.unprojectable_features,
        )
    );
    // #380: the header covers the requested range; say which zooms in it
    // hold nothing rather than let the range above imply they do.
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
fn tiles_summary_line(
    total_tiles: usize,
    min_zoom: u8,
    max_zoom: u8,
    secs: f64,
    out_of_range: usize,
    unprojectable: usize,
) -> String {
    let zooms = format!("z{min_zoom}..z{max_zoom}");
    let tiles = format_number(total_tiles as u64);
    let mut losses: Vec<String> = Vec::new();
    if out_of_range > 0 {
        losses.push(format!(
            "{} feature(s) dropped (outside the declared CRS range)",
            format_number(out_of_range as u64)
        ));
    }
    if unprojectable > 0 {
        losses.push(format!(
            "{} feature(s) dropped (|lat| > 85.05°, outside the Web Mercator tiling domain)",
            format_number(unprojectable as u64)
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

    let mode = match args.mode.as_str() {
        "duplicating" => Mode::Duplicating,
        "partitioning" => Mode::Partitioning,
        other => anyhow::bail!("invalid --mode '{other}' (duplicating|partitioning)"),
    };

    let levels = resolve_level_plan(args.gsd.as_deref(), args.min_zoom, args.max_zoom)?;

    let bbox = args.bbox.as_ref().map(|s| parse_bbox(s)).transpose()?;

    let options = args
        .tuning
        .build_convert_options(mode, levels, bbox, args.cogp_compat)?;

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
            "  z{:<2} (level {}): {:>7} tiles, {:>9} features{}",
            z.zoom,
            z.level,
            z.tile_count,
            z.tile_feature_count,
            if z.oversized_tiles > 0 {
                format!(", {} oversized", z.oversized_tiles)
            } else {
                String::new()
            }
        );
    }
    println!(
        "\n✓ {} tiles, {} features, {} oversized tiles in {:.2}s",
        report.total_tiles,
        report.total_tile_features,
        report.oversized_tiles,
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
    use tylertoo_core::overview::convert::ConvertOptions;
    use tylertoo_core::overview::export::ExportOptions;
    use tylertoo_core::pyramid::{build_pyramid, validate_bands, Band, PyramidOptions};

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

    let convert = if args.generalize {
        ConvertOptions::default()
    } else {
        ConvertOptions::default().verbatim()
    };
    let opts = PyramidOptions {
        convert,
        export: ExportOptions {
            tile_size_limit: args.max_tile_size.and_then(size_limit_opt),
            ..ExportOptions::default()
        },
        work_dir: args.work_dir.clone(),
        allow_missing_zooms: args.allow_missing_zooms,
    };

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
             optimize` to give every row group a tight covering, and the cut improves for \
             free.",
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
    /// the OUTPUT (clap slots it into the INPUT position; resolve_io swaps).
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

    #[test]
    fn parse_size_bytes_accepts_suffixed_and_raw() {
        assert_eq!(parse_size_bytes("500K").unwrap(), 500 * 1024);
        assert_eq!(parse_size_bytes("1M").unwrap(), 1024 * 1024);
        // A plain integer is raw bytes — keeps pre-reconciliation invocations working.
        assert_eq!(parse_size_bytes("500000").unwrap(), 500_000);
        assert!(parse_size_bytes("banana").is_err());
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
        match resolve_level_plan(a.gsd.as_deref(), a.min_zoom, a.max_zoom).unwrap() {
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
        assert_eq!(a.max_zoom, 33, "the CLI must not silently clamp");
        match resolve_level_plan(a.gsd.as_deref(), a.min_zoom, a.max_zoom).unwrap() {
            LevelPlan::ZoomRange { max_zoom, .. } => assert_eq!(max_zoom, 33),
            other => panic!("expected ZoomRange, got {other:?}"),
        }

        let a = parse_tiles(&["--gsd", "0.000005"]);
        match resolve_level_plan(a.gsd.as_deref(), a.min_zoom, a.max_zoom).unwrap() {
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
        let clean = tiles_summary_line(1234, 0, 14, 1.5, 0, 0);
        assert_eq!(clean, "1,234 tiles across z0..z14 in 1.50s");

        let empty = tiles_summary_line(0, 0, 14, 0.05, 3, 0);
        assert!(
            empty.starts_with(
                "0 tiles \u{2014} 3 feature(s) dropped (outside the declared CRS range)"
            ),
            "a wrong-CRS run must not read as a clean success: {empty}"
        );

        let partial = tiles_summary_line(10, 0, 14, 0.2, 1, 0);
        assert!(
            partial.contains("10 tiles across z0..z14")
                && partial.contains("1 feature(s) dropped (outside the declared CRS range)"),
            "a partial loss still reports its tiles AND its losses: {partial}"
        );

        // The Mercator-domain loss is named separately: nothing to reproject.
        let polar = tiles_summary_line(0, 0, 14, 0.05, 0, 7);
        assert!(
            polar.contains(
                "7 feature(s) dropped (|lat| > 85.05\u{b0}, outside the Web Mercator \
                 tiling domain)"
            ) && !polar.contains("declared CRS range"),
            "an Arctic extract must be told why it tiled to nothing: {polar}"
        );

        // Both at once, both named.
        let both = tiles_summary_line(5, 0, 14, 0.1, 2, 3);
        assert!(
            both.contains("2 feature(s) dropped (outside the declared CRS range)")
                && both.contains("3 feature(s) dropped (|lat| > 85.05\u{b0}"),
            "{both}"
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
