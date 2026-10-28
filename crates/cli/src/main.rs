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
use clap::{CommandFactory, Parser, Subcommand};
use indicatif::{HumanBytes, HumanCount};
use std::path::{Path, PathBuf};
#[expect(
    clippy::disallowed_types,
    reason = "#452: immutable CLI quiet setting shared by status output"
)]
use std::sync::atomic::{AtomicBool, Ordering};
use tylertoo_core::overview::auto_zoom::MaxZoom;
use tylertoo_core::overview::config::cli::{
    export_options, overview_options, pyramid_options, tiles_options, ExportPmtilesArgs,
    OverviewArgs, PyramidArgs, TilesArgs,
};

use tylertoo_core::overview::config::{self, OutputKind};
use tylertoo_core::overview::tiles::TilesOptions;
use tylertoo_core::TilesetMetadata as TilesetMetadataArgs;

/// `-q/--quiet` (#452), set once in `main` before dispatch.
#[expect(
    clippy::disallowed_types,
    reason = "#452: immutable CLI quiet setting shared by status output"
)]
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
    ExportPmtiles(Box<ExportPmtilesArgs>),
    /// Decode a PMTiles vector-tile archive back to GeoParquet.
    Decode(DecodeArgs),
    /// Report per-zoom tile sizes for a PMTiles archive, from its
    /// directory alone.
    Stats(StatsArgs),
    /// Build one archive from several inputs, each owning a zoom range.
    Pyramid(Box<PyramidArgs>),
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
        Command::ExportPmtiles(args) => run_export_pmtiles(*args),
        Command::Decode(args) => run_decode(args),
        Command::Stats(args) => run_stats(args),
        Command::Pyramid(args) => run_pyramid(*args),
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
    let command = reference_docs::command_for_markdown(Cli::command());
    let body = reference_docs::format(clap_markdown::help_markdown_command_custom(
        &command, &options,
    ));
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

/// Resolve an [`InputSpec`] to the core's [`ConvertSource`](tylertoo_core::input_set::ConvertSource), the one way.
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
            options.validate().map(|_| ())
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
        options.ignored_by_plan_only(&TilesOptions::export_defaults(args.tuning.syntax.verbatim));
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
                if let Some(report_path) = &args.report {
                    write_report(report_path, &info)?;
                }
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
                if let Some(report_path) = &args.report {
                    write_report(report_path, &info)?;
                }
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
        "  {} tiles across z{}..z{} in {:.2}s",
        format_number(export_report.total_tiles as u64),
        export_report.min_zoom,
        export_report.max_zoom,
        convert_report.duration_secs + export_report.duration_secs
    );
    print_diagnostics(
        convert_report
            .diagnostics
            .iter()
            .chain(&export_report.diagnostics),
    );
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
                if args.tuning.syntax.no_collapse {
                    " (drop --no-collapse to keep placeholders)"
                } else {
                    ""
                },
            );
        }
    }

    // The combined report so the one-step run captures both halves the
    // two-step chain would write (`overview --report` + `export-pmtiles
    // --report`). Core owns its shape ({"convert", "export"}).
    if let Some(report_path) = &args.report {
        write_report(report_path, &*report)?;
    }

    Ok(())
}

fn write_report(path: &Path, report: &impl serde::Serialize) -> Result<()> {
    let json = serde_json::to_string_pretty(report).context("failed to serialize tiles report")?;
    std::fs::write(path, json)
        .with_context(|| format!("failed to write report to {}", path.display()))?;
    say!("  report written to {}", path.display());
    Ok(())
}

/// Print the core diagnostic channel for any completed command.
fn print_diagnostics<'a>(
    events: impl IntoIterator<Item = &'a tylertoo_core::diagnostics::Diagnostic>,
) {
    for diagnostic in events {
        say!("  {diagnostic}");
    }
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
    print_diagnostics(report.diagnostics.iter());

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
            "  z{:<2} (level {}): {:>7} tiles, {:>9} features",
            z.zoom,
            z.level,
            z.tile_count,
            z.tile_feature_count,
        );
    }
    say!(
        "\n✓ {} tiles, {} features, {} oversized tiles in {:.2}s",
        report.total_tiles,
        report.total_tile_features,
        report.oversized_tiles,
        report.duration_secs,
    );
    print_diagnostics(report.diagnostics.iter());

    if let Some(path) = &args.report {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| anyhow::anyhow!("serialize report: {e}"))?;
        std::fs::write(path, json)
            .map_err(|e| anyhow::anyhow!("write report {}: {e}", path.display()))?;
        say!("  report → {}", path.display());
    }
    Ok(())
}

/// Run `tylertoo pyramid`: merge per-band PMTiles archives into one (thin
/// facade over `tylertoo_core::pyramid::merge_bands`).
fn run_pyramid(args: PyramidArgs) -> Result<()> {
    use tylertoo_core::pyramid::{build_pyramid, parse_bands};

    // #452: the archive is created only after every band is tiled.
    config::check_output(&args.output, OutputKind::Pmtiles, args.force)?;
    preflight_report(args.report.as_deref(), &[&args.output])?;

    // #779: the option rules, before any band is tiled.
    let opts = pyramid_options(&args);
    opts.validate()?;

    // Spec parsing, band-set validation and the band-input existence check
    // (with its stripped-`=LAYER` hint) all live in core (#445).
    let bands = parse_bands(&args.bands).map_err(|message| config::ConfigError::InvalidSpec {
        option: "--band",
        message,
    })?;

    let report = build_pyramid(&bands, &args.output, &opts)
        .map_err(|e| step_failed("pyramid build", e, tylertoo_core::Error::is_config))?;

    for (layer, lo, hi, n) in &report.per_band_tiles {
        say!(
            "  z{lo}-{hi} -> layer {layer:?}: {} tiles",
            format_number(*n as u64)
        );
    }
    print_diagnostics(report.diagnostics.iter());
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
        metadata: args.publication.clone(),
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
    use tylertoo_core::overview::config::cli::resolve_level_plan;
    use tylertoo_core::overview::config::cli::{
        parse_in_flight_batches, parse_partition_wave, parse_read_workers, parse_size_bytes,
        parse_size_cap,
    };
    use tylertoo_core::overview::config::cli::{parse_suffixed_size, SizeParseError};
    use tylertoo_core::overview::config::SizeCap;
    use tylertoo_core::overview::export::ExportOptions;
    use tylertoo_core::overview::export::FeatureOrder;
    use tylertoo_core::overview::level::MemoryProfile;

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

    fn parse_command(base: &[&str], flags: &[&str]) -> Command {
        let mut argv = base.to_vec();
        argv.extend_from_slice(flags);
        Cli::try_parse_from(argv)
            .expect("command args should parse")
            .command
    }

    /// Parse a `tiles` invocation and return its args (INPUT/OUTPUT are dummies).
    fn parse_tiles(flags: &[&str]) -> TilesArgs {
        match parse_command(&["tylertoo", "tiles", "in.parquet", "out.pmtiles"], flags) {
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
        match parse_command(
            &["tylertoo", "export-pmtiles", "in.parquet", "out.pmtiles"],
            flags,
        ) {
            Command::ExportPmtiles(a) => *a,
            other => panic!("expected export-pmtiles subcommand, got {other:?}"),
        }
    }

    fn parse_pyramid(flags: &[&str]) -> PyramidArgs {
        match parse_command(
            &[
                "tylertoo",
                "pyramid",
                "out.pmtiles",
                "--band",
                "0-5:coarse.parquet",
            ],
            flags,
        ) {
            Command::Pyramid(a) => *a,
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
        // Numeric values pass to core validation, including the auto sentinel.
        assert_eq!(parse_read_workers("0").unwrap(), 0);
        assert!(parse_read_workers("banana").is_err());
        assert_eq!(
            parse_read_workers(&usize::MAX.to_string()).unwrap(),
            usize::MAX
        );
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
        assert_eq!(a.tuning.assign.polygon_visibility, 2.0);
        assert!(a.tuning.syntax.collapse);
        assert_eq!(a.tuning.density.drop_rate, 1.3);
        assert_eq!(a.tuning.profile, MemoryProfile::Bounded);
        assert!(a.tuning.cluster);
        assert!(!a.tuning.coalesce_lines);
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
            a.tuning.syntax.representation.as_deref(),
            Some("0-7:point,8-13:geom")
        );

        let b = parse_tiles(&["--collapse-square"]);
        assert!(b.tuning.syntax.collapse_square);

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
            matches!(
                err,
                ConvertError::Config(config::ConfigError::CollapseConflict)
            ),
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
                assert_eq!(
                    a.tuning.syntax.representation.as_deref(),
                    Some("0-5:square")
                );
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
        assert_eq!(a.export.tile_size_limit, Some(500 * 1024));
        assert_eq!(a.export.tile_size_limit, b.export.tile_size_limit);
        assert_eq!(
            parse_export(&["--max-tile-size", "1M"])
                .export
                .tile_size_limit,
            parse_export(&["--tile-size-limit", "1M"])
                .export
                .tile_size_limit,
        );
        assert_eq!(
            parse_pyramid(&["--tile-size-limit", "1M"])
                .export
                .tile_size_limit,
            Some(1024 * 1024)
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
        assert!(tiles.tuning.syntax.verbatim);
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
        assert!(!plain.tuning.syntax.verbatim);
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

    #[test]
    fn option_values_keep_leading_hyphens() {
        let args = parse_tiles(&["--sort-key=-height", "--feature-id=-id"]);
        let options = tiles_options(&args).unwrap();
        assert_eq!(options.convert.sort_key.as_deref(), Some("-height"));
        assert_eq!(options.export.feature_id.as_deref(), Some("-id"));
    }

    // APFS refuses non-UTF-8 names (EILSEQ), so macOS cannot hold such a path.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn native_spill_paths_do_not_round_trip_through_json() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let directory = tempfile::tempdir().unwrap();
        let spill = directory
            .path()
            .join(OsString::from_vec(b"spill-\xff".to_vec()));
        std::fs::create_dir(&spill).unwrap();
        let mut argv: Vec<_> = [
            "tylertoo",
            "tiles",
            "in.parquet",
            "out.pmtiles",
            "--spill-dir",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        argv.push(spill.as_os_str().to_owned());
        let Command::Tiles(args) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("tiles")
        };
        let options = tiles_options(&args).unwrap();
        options.validate().unwrap();
        assert_eq!(options.convert.spill_dir.as_deref(), Some(spill.as_path()));
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
            a.publication.clone(),
            TilesetMetadata::new()
                .with_name("tiles")
                .with_description("")
                .with_attribution(html)
        );
        assert_eq!(parse_tiles(&[]).publication.clone(), TilesetMetadata::new());
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
