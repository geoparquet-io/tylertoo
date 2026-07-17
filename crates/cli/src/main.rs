//! CLI for tylertoo - Convert GeoParquet to PMTiles
//!
//! This is a thin wrapper around the tylertoo-core library.

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use indicatif::HumanBytes;
use std::path::PathBuf;

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

/// Parse `--in-flight-batches`: `auto` (→ the core-sized sentinel 0) or an
/// explicit positive integer. See [`resolve_in_flight_batches`] for how the
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

    /// Per-tile edge buffer, in tile pixels (feature seam continuity).
    #[arg(long, default_value = "8")]
    tile_buffer: u32,

    /// Optional per-tile MVT size limit (e.g., "500K", "1M", or raw bytes).
    /// When a tile exceeds it, a single non-iterative drop pass sheds the
    /// lowest-priority (smallest) features for that tile only. Omit to enforce
    /// no limit. Aliased as --max-tile-size for parity with the `tiles` command.
    #[arg(long, value_name = "SIZE", alias = "max-tile-size", value_parser = parse_size_bytes)]
    tile_size_limit: Option<usize>,

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
    /// transients fit in a fraction of available RAM (floor 6; fixed cap 16
    /// only when RAM cannot be probed; override the RAM figure with
    /// TYLERTOO_AUTO_MEM_LIMIT_BYTES). Pass an explicit integer to override.
    /// Wider waves keep more cores busy at proportionally more peak memory
    /// (one wave of partitions resident). The chosen width and the preflight
    /// inputs are logged at export start. Output is byte-identical for every
    /// value.
    #[arg(long, value_name = "N|auto", default_value = "auto", value_parser = parse_partition_wave)]
    partition_wave: usize,

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
    /// Column name used as the cell-winner priority (sort) key. Mutually
    /// exclusive with --class-rank.
    #[arg(long, value_name = "COL", help_heading = "Ranking")]
    sort_key: Option<String>,

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
    #[arg(long, default_value = "1.0", help_heading = "Generalization")]
    simplify_factor: f64,

    /// Collapse below-visibility polygons to a representative point instead of
    /// dropping them (spec Q4 opt-in).
    #[arg(long, help_heading = "Generalization")]
    collapse: bool,

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
    #[arg(long, default_value = "1.0", help_heading = "Thinning & visibility")]
    line_thinning: f64,

    /// Polygon thinning factor: grid cell size = factor * gsd (default 1.0).
    ///
    /// BIGGER = SPARSER, SMALLER = denser. Polygons thin least by default
    /// (1.0) since they tile space rather than cluster.
    #[arg(long, default_value = "1.0", help_heading = "Thinning & visibility")]
    polygon_thinning: f64,

    /// Line visibility gate in GSD multiples: a line is eligible at a level
    /// only if its bbox diagonal >= factor * gsd (default 2.0).
    ///
    /// This is a hard drop, not a thin: BIGGER = more small lines dropped at
    /// coarse levels (sparser); SMALLER = more small lines kept. The gate is
    /// multiplied by the level GSD, so --gsd-base moves it too.
    #[arg(long, default_value = "2.0", help_heading = "Thinning & visibility")]
    line_visibility: f64,

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
    #[arg(long, default_value = "2.0", help_heading = "Thinning & visibility")]
    polygon_visibility: f64,

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
    /// row; 1 for unmerged rows and everywhere at the canonical level).
    /// Points and polygons are unaffected. In partitioning mode coalescing
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
    /// MB even for vertex-heavy polygon data. No effect with --no-streaming.
    #[arg(
        long,
        value_name = "ROWS",
        default_value = "8192",
        help_heading = "Memory & performance"
    )]
    read_batch_size: usize,

    /// Memory/throughput profile for the single-read pass-2 engine (#213/#212).
    ///
    /// `speed` buffers each output level's rows in RAM (fastest; peak RAM grows
    /// with buffered output). `bounded` spills them to temporary Arrow IPC
    /// files (memory-capped; slight temp-I/O cost). `auto` (default) is
    /// workload-based: it estimates buffered output from feature and level
    /// counts and spills when that exceeds a fraction of available RAM, so large
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

    /// Read batches allowed in flight through the pass-2 pipeline at once
    /// (read/compute-overlap knob; bounded-channel depth).
    ///
    /// `auto` (the default) sizes this to the machine's available cores
    /// (clamped to 4..=16); pass an explicit integer to override. Higher
    /// improves core utilization on long-pole geometries at proportionally
    /// more peak memory (in-flight-batches × read-batch-size rows resident).
    /// The chosen depth and detected core count are logged at pass-2 start.
    /// No effect with --no-streaming.
    #[arg(
        long,
        value_name = "N|auto",
        default_value = "auto",
        value_parser = parse_in_flight_batches,
        help_heading = "Memory & performance"
    )]
    in_flight_batches: usize,

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
    #[arg(long, value_name = "PATH", help_heading = "Memory & performance")]
    spill_dir: Option<PathBuf>,
}

impl ConvertTuningArgs {
    /// Build [`ConvertOptions`] from the shared tuning flags, applying the same
    /// validation both `overview` and `tiles` rely on. The parent command owns
    /// `mode`, the `levels` plan, `bbox`, and `cogp_compat` and passes them in.
    fn build_convert_options(
        &self,
        mode: tylertoo_core::overview::level::Mode,
        levels: tylertoo_core::overview::convert::LevelPlan,
        bbox: Option<[f64; 4]>,
        cogp_compat: bool,
    ) -> Result<tylertoo_core::overview::convert::ConvertOptions> {
        use tylertoo_core::overview::assign::{AssignConfig, DensityBudgetConfig, SortDirection};
        use tylertoo_core::overview::convert::ConvertOptions;
        use tylertoo_core::overview::level::{MemoryProfile, Mode};
        use tylertoo_core::overview::simplify::SimplifyOptions;
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

        // Cluster-conditional default: with --cluster, absorbed points are
        // summarized (point_count), so the sparser 16.0 grid is the better look.
        let point_thinning = self.point_thinning.unwrap_or(if self.cluster {
            tylertoo_core::overview::assign::CLUSTER_POINT_THINNING_DEFAULT
        } else {
            AssignConfig::default().point_thinning
        });

        let assign = AssignConfig {
            point_thinning,
            line_thinning: self.line_thinning,
            polygon_thinning: self.polygon_thinning,
            line_visibility: self.line_visibility,
            polygon_visibility: self.polygon_visibility,
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

        // Clustering flags (Q4; also enforced in core).
        if !self.accumulate_attribute.is_empty() && !self.cluster {
            anyhow::bail!("--accumulate-attribute requires --cluster");
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
        let coalesce_lines = !self.no_coalesce_lines;

        Ok(ConvertOptions {
            mode,
            levels,
            assign,
            sort_key: self.sort_key.clone(),
            class_ranking,
            no_auto_rank: self.no_auto_rank,
            simplify: SimplifyOptions {
                factor: self.simplify_factor,
                collapse: self.collapse,
                cascade: !self.no_cascade,
            },
            density: DensityBudgetConfig {
                enabled: !self.no_density_drop,
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
            cluster: self.cluster,
            accumulate,
            coalesce_lines,
            coalesce_snap: self.coalesce_snap,
            coalesce_max_level_rows: self.coalesce_max_level_rows,
            coalesce_junction_angle: self.coalesce_junction_angle,
            bbox,
            spill_dir: self.spill_dir.clone(),
        })
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
    /// exceeds this limit, the export sheds its lowest-priority features in a
    /// single non-iterative pass. Aliased as --tile-size-limit for parity with
    /// `export-pmtiles`.
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
    /// transients fit in a fraction of available RAM (floor 6; fixed cap 16
    /// only when RAM cannot be probed; override the RAM figure with
    /// TYLERTOO_AUTO_MEM_LIMIT_BYTES). Pass an explicit integer to override.
    /// Wider waves keep more cores busy at proportionally more peak memory
    /// (one wave of partitions resident). The chosen width and the preflight
    /// inputs are logged at export start. Output is byte-identical for every
    /// value.
    #[arg(long, value_name = "N|auto", default_value = "auto", value_parser = parse_partition_wave)]
    partition_wave: usize,

    /// Write a JSON report to this path: a combined object with a `convert`
    /// section (the overview build, matching `overview --report`) and an
    /// `export` section (the PMTiles export, matching `export-pmtiles
    /// --report`), so the one-step run captures both halves the two-step
    /// chain would.
    #[arg(long, value_name = "PATH")]
    report: Option<PathBuf>,

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
        Command::Tiles(args) => run_tiles(*args),
    }
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
    const SUBCOMMANDS: [&str; 6] = [
        "tiles",
        "overview",
        "validate",
        "export-pmtiles",
        "decode",
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

/// Convert via the core pipeline, dispatching on the input shape: a path
/// (file/dir/glob/URL/prefix, resolved by core) or a `--files-from`
/// manifest (explicit ordered file list, order preserved verbatim).
fn run_convert(
    spec: &InputSpec,
    output: &std::path::Path,
    options: &tylertoo_core::overview::convert::ConvertOptions,
) -> Result<tylertoo_core::overview::convert::ConvertReport> {
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
    .map_err(|e| anyhow::anyhow!("overview conversion failed: {e}"))
}

/// Run `tylertoo tiles`: the one-shot GeoParquet → PMTiles facade.
///
/// Chains the two product pipelines through a temporary overview file:
/// `overview` (convert, with default knobs) → `export-pmtiles`. The temp
/// file lives next to the output (same filesystem) and is removed on both
/// success and failure via [`tempfile::NamedTempFile`]'s drop guard.
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
    let options = args
        .tuning
        .build_convert_options(Mode::Duplicating, levels, bbox, false)?;

    // Intermediate overview file next to the output (same filesystem);
    // NamedTempFile removes it on drop — success or failure alike.
    let tmp_dir = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let overview_tmp = tempfile::Builder::new()
        .prefix(".tylertoo-overview-")
        .suffix(".parquet")
        .tempfile_in(&tmp_dir)
        .context("failed to create temporary overview file")?;

    let convert_report = run_convert(&spec, overview_tmp.path(), &options)?;

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
        tile_size_limit: args.max_tile_size,
        simple_clip_fastpath: !args.no_simple_clip_fastpath,
        partition_wave: args.partition_wave,
    };
    let export_report = export_pmtiles(overview_tmp.path(), &output, &export_opts)
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
        "  {} tiles across z{}..z{} in {:.2}s",
        format_number(export_report.total_tiles as u64),
        export_report.min_zoom,
        export_report.max_zoom,
        convert_report.duration_secs + export_report.duration_secs
    );

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

    if let Some(report_path) = &args.report {
        let json =
            serde_json::to_string_pretty(&report).context("failed to serialize overview report")?;
        std::fs::write(report_path, json)
            .with_context(|| format!("failed to write report to {}", report_path.display()))?;
        println!("  report written to {}", report_path.display());
    }

    Ok(())
}

/// Parse a `--class-rank` spec: `COLUMN:VALUE=RANK,VALUE=RANK,...`.
///
/// `unknown_rank` (the priority for present-but-unlisted values) is derived as
/// `min(listed ranks) - 1.0`, so unknown classes always lose to every listed
/// value while still beating null/missing values (which lose to any rank).
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
        tile_size_limit: args.tile_size_limit,
        simple_clip_fastpath: !args.no_simple_clip_fastpath,
        partition_wave: args.partition_wave,
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
        assert_eq!(a.tuning.polygon_visibility, 2.0);
        assert!(a.tuning.collapse);
        assert_eq!(a.tuning.drop_rate, 1.3);
        assert_eq!(a.tuning.profile, "bounded");
        assert!(a.tuning.cluster);
        assert!(a.tuning.no_coalesce_lines);
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
        assert!(opts.simplify.collapse);
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
        let allow: BTreeSet<&str> = ["mode", "cogp-compat", "tile-size-limit"]
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
}
