//! clap derives over core's actual option structs, plus syntax adapters.
use super::ConfigError;
use crate::overview::auto_zoom::MaxZoom;
use crate::overview::properties::PropertySelection as ExportPropertyArgs;
use crate::overview::tiles::{TilesError, TilesOptions, TILES_DEFAULT_MAX_ZOOM};
use crate::TilesetMetadata as TilesetMetadataArgs;
use clap::{Args, FromArgMatches, Parser};
use std::path::PathBuf;
type ConvertTuningArgs = ConvertArgs;
use super::SizeCap;
use crate::overview::convert::{ConvertError, ConvertOptions, LevelPlan};
use crate::overview::export::ExportOptions;
use crate::overview::level::Mode;
use serde_json::{Map, Value};

/// Why a suffixed size did not parse: only the overflow branch should
/// mention the ceiling, so the caller's message is built per branch.
#[derive(Debug, PartialEq, Eq)]
pub enum SizeParseError {
    /// Not `<integer>[K|M|G]` at all.
    Malformed,
    /// Well-formed, but `n * multiplier` does not fit `usize` (#432).
    Overflow,
}

/// Parse `<integer>[K|KB|M|MB|G|GB]` (case-insensitive) to bytes.
///
/// The suffix multiplication is checked (#432): `99999999999999999G` used to
/// wrap in release builds and be accepted as a small byte count.
///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn parse_suffixed_size(s: &str) -> Result<usize, SizeParseError> {
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
pub fn size_ceiling_hint(err: SizeParseError) -> String {
    match err {
        SizeParseError::Malformed => String::new(),
        SizeParseError::Overflow => format!(" (the value must fit in {} bytes)", usize::MAX),
    }
}

/// Parse a human-readable byte size (e.g., "500K", "1M", "2G") as usize.
///
/// A plain integer with no suffix is interpreted as raw bytes, so callers that
/// previously passed a byte count (e.g. `--tile-size-limit 500000`) keep working.
///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn parse_size_bytes(s: &str) -> Result<usize, String> {
    parse_suffixed_size(s).map_err(|err| {
        format!(
            "Invalid size: '{s}'. Use a byte count or a suffixed size like '500K', '1M', '2G'{}",
            size_ceiling_hint(err)
        )
    })
}

/// Parse `--max-tile-size` / `--max-compressed-tile-size`: `auto` (the
/// command's core default) or a size, where 0 is no cap (#280, #761).
///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn parse_size_cap(s: &str) -> Result<SizeCap, String> {
    if s.trim().eq_ignore_ascii_case("auto") {
        return Ok(SizeCap::Auto);
    }
    parse_size_bytes(s).map(SizeCap::Bytes)
}

/// A flag whose value is one of `values`, parsed into core's type: clap
/// lists the values in `--help`, core's `FromStr` owns the meaning.
pub fn spelled<T>(
    values: &'static [&'static str],
) -> impl clap::builder::TypedValueParser<Value = T>
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
/// [`crate::overview::convert::resolve_in_flight_batches`] for how the
/// sentinel is expanded at pass-2 setup.
///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn parse_in_flight_batches(s: &str) -> Result<usize, String> {
    positive_or_auto(
        s,
        crate::overview::convert::IN_FLIGHT_BATCHES_AUTO,
        "in-flight-batches",
    )
}

fn positive_or_auto(s: &str, automatic: usize, option: &str) -> Result<usize, String> {
    if s.eq_ignore_ascii_case("auto") {
        return Ok(automatic);
    }
    match s.parse::<usize>() {
        Ok(0) => Err(format!("{option} must be `auto` or >= 1")),
        Ok(n) => Ok(n),
        Err(_) => Err(format!("expected `auto` or a positive integer, got `{s}`")),
    }
}

/// Parse the worker spelling; core validates its range and expands `auto`.
///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn parse_read_workers(s: &str) -> Result<usize, String> {
    if s.eq_ignore_ascii_case("auto") {
        Ok(crate::overview::convert::READ_WORKERS_AUTO)
    } else {
        s.parse()
            .map_err(|_| format!("expected `auto` or an integer, got `{s}`"))
    }
}

/// Parse `--partition-wave`: `auto` or an explicit positive partition count.
///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn parse_partition_wave(s: &str) -> Result<usize, String> {
    positive_or_auto(
        s,
        crate::overview::export::PARTITION_WAVE_AUTO,
        "partition-wave",
    )
}

///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn parse_size_auto(s: &str) -> Result<usize, String> {
    if s.trim().eq_ignore_ascii_case("auto") {
        Ok(usize::MAX)
    } else {
        parse_size_bytes(s)
    }
}
pub use super::keywords::ConvertSyntax;

fn scoped_args(
    mut command: clap::Command,
    definition: &clap::Command,
    full: bool,
    scope: &'static str,
) -> clap::Command {
    let mut built = definition.clone();
    built.build();
    let excluded: Vec<_> = built
        .get_groups()
        .find(|group| group.get_id().as_str() == scope)
        .map(|group| group.get_args().cloned().collect())
        .unwrap_or_default();
    for arg in definition
        .get_arguments()
        .filter(|arg| full || !excluded.contains(arg.get_id()))
    {
        command = command.arg(arg.clone().required(false));
    }
    command.group(clap::ArgGroup::new(scope).multiple(true))
}

fn scoped_matches(
    definition: clap::Command,
    matches: &clap::ArgMatches,
    scope: Option<&str>,
) -> Result<clap::ArgMatches, clap::Error> {
    let mut built = definition.clone();
    built.build();
    let excluded: Vec<_> = built
        .get_groups()
        .filter(|group| Some(group.get_id().as_str()) == scope)
        .flat_map(clap::ArgGroup::get_args)
        .collect();
    let mut argv = vec![std::ffi::OsString::from("options")];
    for arg in definition.get_arguments() {
        if excluded.contains(&arg.get_id()) {
            continue;
        }
        let id = arg.get_id().as_str();
        if !matches.try_contains_id(id).unwrap_or(false)
            || matches.value_source(id) != Some(clap::parser::ValueSource::CommandLine)
        {
            continue;
        }
        let Some(long) = arg.get_long() else {
            continue;
        };
        if arg.get_action().takes_values() {
            if let Some(values) = matches.get_raw(id) {
                for value in values {
                    let mut flag = std::ffi::OsString::from(format!("--{long}="));
                    flag.push(value);
                    argv.push(flag);
                }
            }
        } else {
            argv.push(format!("--{long}").into());
        }
    }
    definition
        .mut_args(|arg| {
            let action = arg.get_action().clone();
            let arg = arg.required(false).default_value(None::<&str>);
            match action {
                clap::ArgAction::SetTrue => arg
                    .action(clap::ArgAction::Set)
                    .num_args(0..=1)
                    .default_missing_value("true"),
                clap::ArgAction::SetFalse => arg
                    .action(clap::ArgAction::Set)
                    .num_args(0..=1)
                    .default_missing_value("false"),
                _ => arg,
            }
        })
        .try_get_matches_from(argv)
}

/// The core convert options and their syntax. Only explicitly supplied flags
/// override the command's core defaults (including verbatim and clustering).
#[derive(Debug, Clone)]
pub struct ConvertArgs<const FULL: bool = false> {
    pub options: ConvertOptions,
    pub syntax: ConvertSyntax,
    matches: clap::ArgMatches,
}

impl<const FULL: bool> std::ops::Deref for ConvertArgs<FULL> {
    type Target = ConvertOptions;
    fn deref(&self) -> &ConvertOptions {
        &self.options
    }
}

impl<const FULL: bool> Args for ConvertArgs<FULL> {
    fn augment_args(command: clap::Command) -> clap::Command {
        scoped_args(command, &ConvertSyntax::augment_args(ConvertOptions::augment_args(clap::Command::new("options").disable_help_flag(true))), FULL, "overview_only").mut_arg("include_properties", |arg| arg.help("Keep only these property columns (repeatable) and skip decoding the rest. Columns that other flags read must stay in the list").long_help(None).help_heading("Properties").value_name("COL")).mut_arg("exclude_properties", |arg| arg.help("Drop these property columns (repeatable). `--include-property` overrides it").long_help(None).help_heading("Properties").value_name("COL")).mut_arg("exclude_all_properties", |arg| arg.help("Drop every property column, writing geometry only. `--include-property` overrides it").long_help(None).help_heading("Properties"))
    }
    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
}

impl<const FULL: bool> FromArgMatches for ConvertArgs<FULL> {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let complete = scoped_matches(
            ConvertSyntax::augment_args(ConvertOptions::augment_args(
                clap::Command::new("options").disable_help_flag(true),
            )),
            matches,
            (!FULL).then_some("overview_only"),
        )?;
        let matches = &complete;
        let mut options = ConvertOptions::default();
        options.update_from_arg_matches(matches)?;
        options.check_finite_numbers().map_err(|error| {
            clap::Error::raw(clap::error::ErrorKind::ValueValidation, error.to_string())
        })?;
        let mut syntax = ConvertSyntax::default();
        syntax.update_from_arg_matches(matches)?;
        Ok(Self {
            options,
            syntax,
            matches: complete,
        })
    }
    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl<const FULL: bool> ConvertArgs<FULL> {
    pub fn plan_paths(&self) -> impl Iterator<Item = &std::path::Path> {
        self.save_plan
            .as_deref()
            .into_iter()
            .chain(self.plan.as_deref())
    }
    /// Build core options from the explicitly supplied flags.
    ///
    ///
    /// # Errors
    /// Returns the core diagnostic for invalid syntax or conflicting options.
    ///
    /// # Panics
    /// Panics only if clap supplied values incompatible with the core override types.
    pub fn build_convert_options(
        &self,
        mode: Mode,
        levels: LevelPlan,
        bbox: Option<[f64; 4]>,
        cogp_compat: bool,
    ) -> Result<ConvertOptions, ConvertError> {
        let mut options = self.options.clone();
        options.mode = mode;
        options.levels = levels;
        options.bbox = bbox;
        options.cogp_compat_key = cogp_compat;
        let mut explicit = Map::new();
        for id in self.matches.ids() {
            if let Ok(Some(value)) = self.matches.try_get_one::<f64>(id.as_str()) {
                explicit.insert(id.to_string(), (*value).into());
            } else if let Ok(Some(value)) = self.matches.try_get_one::<bool>(id.as_str()) {
                explicit.insert(id.to_string(), (*value).into());
            }
        }
        if self.syntax.explicit_coalesce_lines {
            explicit.insert("coalesce_lines".into(), true.into());
        }
        let overrides = serde_json::from_value(Value::Object(explicit))
            .expect("clap parsed typed generalization values");
        if self.syntax.collapse || self.syntax.collapse_square || self.syntax.no_collapse {
            options.simplify.collapse = super::collapse_mode(
                self.syntax.collapse,
                self.syntax.collapse_square,
                self.syntax.no_collapse,
            )?;
        }
        if self.syntax.magnitude_ladder.is_some() || self.syntax.entry_zoom.is_some() {
            options.entry_zoom = super::entry_zoom(
                self.syntax.magnitude_ladder.as_deref(),
                self.syntax.ladder_step,
                self.syntax.entry_zoom.as_deref(),
            )?;
        }
        if let Some(spec) = self.syntax.class_rank.as_deref() {
            options.class_ranking = super::class_ranking(Some(spec), None, None, None)?;
        }
        if self.syntax.representation.is_some() {
            options.representation = super::representation(self.syntax.representation.as_deref())?;
        }
        options.with_generalization(&overrides, self.syntax.verbatim)
    }
}

/// Deserialize the supplied scope over the option type's core defaults.
fn parse_scoped_options<T: Args + Default>(
    matches: &clap::ArgMatches,
    excluded: Option<&str>,
) -> Result<(T, clap::ArgMatches), clap::Error> {
    let complete = scoped_matches(
        T::augment_args(clap::Command::new("options").disable_help_flag(true)),
        matches,
        excluded,
    )?;
    let mut options = T::default();
    options.update_from_arg_matches(&complete)?;
    Ok((options, complete))
}

/// Export flags parse directly into `ExportOptions`. The supplied-field set is
/// syntax provenance, so `auto` can use each command's own defaults.
#[derive(Debug, Clone)]
pub struct ExportArgs<const FULL: bool = false> {
    options: ExportOptions,
    matches: clap::ArgMatches,
}
impl<const FULL: bool> std::ops::Deref for ExportArgs<FULL> {
    type Target = ExportOptions;
    fn deref(&self) -> &ExportOptions {
        &self.options
    }
}
impl<const FULL: bool> Args for ExportArgs<FULL> {
    fn augment_args(command: clap::Command) -> clap::Command {
        scoped_args(
            command,
            &ExportOptions::augment_args(clap::Command::new("options").disable_help_flag(true)),
            FULL,
            "export_only",
        )
    }
    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        Self::augment_args(command)
    }
}
impl<const FULL: bool> FromArgMatches for ExportArgs<FULL> {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let (options, matches) = parse_scoped_options(matches, (!FULL).then_some("export_only"))?;
        Ok(Self { options, matches })
    }
    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}
impl<const FULL: bool> ExportArgs<FULL> {
    /// Apply explicit export flags over command defaults.
    ///
    /// # Panics
    /// Panics only if clap supplied a value incompatible with its typed parser.
    pub fn apply(&self, mut base: ExportOptions) -> ExportOptions {
        let caps = (base.tile_size_limit, base.compressed_tile_size_limit);
        base.update_from_arg_matches(&self.matches)
            .expect("clap parsed typed export values");
        let cap = |key| {
            self.matches
                .get_raw(key)
                .and_then(|mut values| values.next())
                .map(|value| {
                    parse_size_cap(&value.to_string_lossy()).expect("clap parsed a size cap")
                })
                .unwrap_or_default()
        };
        base.tile_size_limit = cap("tile_size_limit").or(caps.0);
        base.compressed_tile_size_limit = cap("max_compressed_tile_size").or(caps.1);
        base
    }
}
/// Clap fields from a facade's actual option struct, over its core defaults.
#[derive(Debug, Clone)]
pub struct FacadeArgs<T> {
    pub options: T,
}
impl<T> std::ops::Deref for FacadeArgs<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.options
    }
}
impl<T: Args + Default> Args for FacadeArgs<T> {
    fn augment_args(command: clap::Command) -> clap::Command {
        T::augment_args(command)
    }
    fn augment_args_for_update(command: clap::Command) -> clap::Command {
        T::augment_args_for_update(command)
    }
}
impl<T: Args + Default> FromArgMatches for FacadeArgs<T> {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        let (options, _) = parse_scoped_options(matches, None)?;
        Ok(Self { options })
    }
    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

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
    pub export: ExportArgs,

    #[command(flatten)]
    pub properties: ExportPropertyArgs,

    /// Write the JSON pyramid report, with per-band tile counts, to this
    /// path.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    pub force: bool,
    #[command(flatten)]
    pub facade: FacadeArgs<crate::pyramid::PyramidOptions>,
}

#[derive(Parser, Debug)]
pub struct ExportPmtilesArgs {
    /// Input overview GeoParquet file from `tylertoo overview`.
    #[arg(value_name = "INPUT")]
    pub input: PathBuf,

    /// Output PMTiles archive.
    #[arg(value_name = "OUTPUT")]
    pub output: PathBuf,

    #[command(flatten)]
    pub publication: TilesetMetadataArgs,

    #[command(flatten)]
    pub properties: ExportPropertyArgs,

    #[command(flatten)]
    pub export: ExportArgs<true>,

    /// Write the JSON export report, with per-zoom tile and feature counts,
    /// to this path.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    pub force: bool,

    /// Not supported here (single-file subcommand); accepted so the error
    /// can point at `overview`/`tiles` instead of clap's generic message.
    #[arg(long, value_name = "PATH", hide = true)]
    pub files_from: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct OverviewArgs {
    /// Input GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator
    /// (`EPSG:3857`). Pass a file, directory, glob, URL, or `s3://` or
    /// `gs://` prefix, or omit it with `--files-from`.
    #[arg(value_name = "INPUT", required_unless_present = "files_from")]
    pub input: Option<PathBuf>,

    /// Output overview GeoParquet file.
    #[arg(value_name = "OUTPUT", required_unless_present = "files_from")]
    pub output: Option<PathBuf>,

    /// Read `.parquet` files from this manifest instead of `INPUT`: one
    /// path or URL per line, in dataset row order. Ignores blank lines and
    /// lines starting with `#`.
    #[arg(long, value_name = "PATH")]
    pub files_from: Option<PathBuf>,

    /// Minimum (coarsest) Web Mercator zoom.
    #[arg(long, default_value_t = default_zooms(&ConvertOptions::default()).0)]
    pub min_zoom: u8,

    /// Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a
    /// sample of the input. Ignored with `--gsd`.
    #[arg(long, default_value_t = MaxZoom::Fixed(default_zooms(&ConvertOptions::default()).1))]
    pub max_zoom: MaxZoom,

    /// Comma-separated ground sample distances (GSDs) in meters, each
    /// smaller than the last. Overrides `--min-zoom` and `--max-zoom`.
    #[arg(long, value_name = "GSDS")]
    pub gsd: Option<String>,

    /// Write the JSON conversion report to this path.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    pub force: bool,

    #[command(flatten)]
    pub tuning: ConvertArgs<true>,
}

#[derive(Parser, Debug)]
pub struct TilesArgs {
    /// Input GeoParquet in lon/lat (`EPSG:4326`) or Web Mercator
    /// (`EPSG:3857`). Pass a file, directory, glob, URL, or `s3://` or
    /// `gs://` prefix, or omit it with `--files-from`.
    #[arg(value_name = "INPUT", required_unless_present = "files_from")]
    pub input: Option<PathBuf>,

    /// Output PMTiles file. Omit it with `--plan-only`, which writes no
    /// archive.
    #[arg(
        value_name = "OUTPUT",
        required_unless_present_any = ["files_from", "plan_only"]
    )]
    pub output: Option<PathBuf>,

    /// Read `.parquet` files from this manifest instead of `INPUT`: one
    /// path or URL per line, in dataset row order. Ignores blank lines and
    /// lines starting with `#`.
    #[arg(long, value_name = "PATH")]
    pub files_from: Option<PathBuf>,

    /// Minimum (coarsest) Web Mercator zoom.
    #[arg(long, default_value_t = default_zooms(&TilesOptions::default().convert).0)]
    pub min_zoom: u8,

    /// Maximum (finest) Web Mercator zoom, or `auto` to estimate it from a
    /// sample of the input. Ignored with `--gsd`.
    #[arg(long, default_value_t = MaxZoom::Fixed(TILES_DEFAULT_MAX_ZOOM))]
    pub max_zoom: MaxZoom,

    /// Comma-separated ground sample distances (GSDs) in meters, each
    /// smaller than the last. Overrides `--min-zoom` and `--max-zoom`.
    #[arg(long, value_name = "GSDS")]
    pub gsd: Option<String>,

    #[command(flatten)]
    pub publication: TilesetMetadataArgs,

    #[command(flatten)]
    pub export: ExportArgs,

    /// Write a JSON report with `convert` and `export` sections, matching
    /// the reports of `overview` and `export-pmtiles`.
    #[arg(long, value_name = "PATH")]
    pub report: Option<PathBuf>,

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
    pub shard: Option<String>,

    /// The shard plan from `tylertoo shard-plan` that every job shares.
    #[arg(long, value_name = "PATH", help_heading = "Sharded builds")]
    pub shard_plan: Option<PathBuf>,

    /// Emit only the tiles in this tile-id range, `LO..HI`: two tile ids at
    /// one zoom, plus their descendants. Prefer `--shard`, which also skips
    /// input the range cannot reach.
    // Exclusive with --shard: core's rule (`ConfigError::TileRangeWithShard`).
    #[arg(long, value_name = "LO..HI", help_heading = "Sharded builds")]
    pub tile_range: Option<String>,

    /// Write the convert plan (`--save-plan`) and stop, with no export and
    /// no OUTPUT. Use it for a coarse job whose tiles the fleet discards.
    // Needs --save-plan: core's rule (`ConfigError::PlanOnlyWithoutSavePlan`).
    #[arg(long, help_heading = "Sharded builds")]
    pub plan_only: bool,

    /// Print per-level and per-zoom breakdowns.
    #[arg(short, long)]
    pub verbose: bool,

    /// Overwrite the output if it exists.
    #[arg(short, long)]
    pub force: bool,

    #[command(flatten)]
    pub tuning: ConvertTuningArgs,
    #[command(flatten)]
    pub facade: FacadeArgs<TilesOptions>,
}

///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn parse_bbox(s: &str) -> Result<[f64; 4], ConfigError> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(ConfigError::InvalidSpec {
            option: "--bbox",
            message: "--bbox must be xmin,ymin,xmax,ymax (4 comma-separated values)".into(),
        });
    }
    let vals: Vec<f64> = parts
        .iter()
        .map(|p| {
            p.trim()
                .parse::<f64>()
                .map_err(|e| ConfigError::InvalidSpec {
                    option: "--bbox",
                    message: format!("invalid number in --bbox: {e}"),
                })
        })
        .collect::<Result<Vec<_>, ConfigError>>()?;
    // xmin <= xmax and ymin <= ymax is core's rule (`ConvertOptions::validate`).
    Ok([vals[0], vals[1], vals[2], vals[3]])
}

///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn resolve_level_plan(
    gsd: Option<&str>,
    min_zoom: u8,
    max_zoom: u8,
) -> Result<crate::overview::convert::LevelPlan, ConfigError> {
    use crate::overview::convert::LevelPlan;

    match gsd {
        Some(gsd_str) => {
            let gsds = gsd_str
                .split(',')
                .map(|s| s.trim().parse::<f64>())
                .collect::<std::result::Result<Vec<f64>, _>>()
                .map_err(|e| ConfigError::InvalidSpec {
                    option: "--gsd",
                    message: format!("invalid --gsd list {gsd_str:?}: {e}"),
                })?;
            Ok(LevelPlan::Gsds(gsds))
        }
        None => Ok(LevelPlan::ZoomRange { min_zoom, max_zoom }),
    }
}

///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn level_plan(
    gsd: Option<&str>,
    min_zoom: u8,
    max_zoom: MaxZoom,
    bbox: Option<[f64; 4]>,
) -> Result<(crate::overview::convert::LevelPlan, Option<[f64; 4]>), ConfigError> {
    Ok((
        resolve_level_plan(gsd, min_zoom, max_zoom.plan_zoom())?,
        bbox,
    ))
}

pub fn default_zooms(options: &ConvertOptions) -> (u8, u8) {
    match options.levels {
        crate::overview::convert::LevelPlan::ZoomRange { min_zoom, max_zoom } => {
            (min_zoom, max_zoom)
        }
        crate::overview::convert::LevelPlan::Gsds(_) => {
            unreachable!("a core default is a zoom range")
        }
    }
}

///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn tiles_options(args: &TilesArgs) -> Result<TilesOptions, TilesError> {
    use crate::overview::level::Mode;

    let (levels, bbox) = level_plan(
        args.gsd.as_deref(),
        args.min_zoom,
        args.max_zoom,
        args.tuning.options.bbox,
    )?;
    // Overviews for PMTiles are always duplicating (partitioning can't be
    // exported to per-tile MVT).
    let mut convert = args
        .tuning
        .build_convert_options(Mode::Duplicating, levels, bbox, false)
        .map_err(TilesError::Convert)?;
    convert.auto_max_zoom = args.max_zoom.is_auto();
    let export = args
        .export
        .apply(TilesOptions::export_defaults(args.tuning.syntax.verbatim));
    Ok(TilesOptions {
        convert,
        export: ExportOptions {
            metadata: args.publication.clone(),
            tile_range: parse_tile_range(args.tile_range.as_deref())?,
            ..export
        },
        ..args.facade.options.clone()
    })
}

///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn parse_tile_range(
    range: Option<&str>,
) -> Result<Option<crate::shard::TileRange>, ConfigError> {
    range
        .map(|s| {
            crate::shard::TileRange::parse(s).map_err(|e| ConfigError::InvalidSpec {
                option: "--tile-range",
                message: format!("--tile-range: {e}"),
            })
        })
        .transpose()
}

///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn overview_options(args: &OverviewArgs) -> Result<ConvertOptions, ConvertError> {
    let (levels, bbox) = level_plan(
        args.gsd.as_deref(),
        args.min_zoom,
        args.max_zoom,
        args.tuning.options.bbox,
    )?;
    let mut options = args.tuning.build_convert_options(
        args.tuning.options.mode,
        levels,
        bbox,
        args.tuning.options.cogp_compat_key,
    )?;
    options.auto_max_zoom = args.max_zoom.is_auto();
    Ok(options)
}

///
/// # Errors
/// Returns a specific configuration error for invalid syntax or option values.
pub fn export_options(args: &ExportPmtilesArgs) -> Result<ExportOptions, ConfigError> {
    Ok(ExportOptions {
        properties: args.properties.clone(),
        metadata: args.publication.clone(),
        ..args.export.apply(ExportOptions::default())
    })
}

pub fn pyramid_options(args: &PyramidArgs) -> crate::pyramid::PyramidOptions {
    let base = crate::pyramid::PyramidOptions::new(args.generalize);
    crate::pyramid::PyramidOptions {
        export: ExportOptions {
            properties: args.properties.clone(),
            ..args.export.apply(base.export)
        },
        // The final archive's overrides; the band exports write none, so
        // GeoParquet bands contribute no inherited value.
        metadata: args.publication.clone(),
        convert: base.convert,
        ..args.facade.options.clone()
    }
}

impl std::ops::Deref for TilesArgs {
    type Target = TilesOptions;
    fn deref(&self) -> &Self::Target {
        &self.facade
    }
}

impl std::ops::Deref for PyramidArgs {
    type Target = crate::pyramid::PyramidOptions;
    fn deref(&self) -> &Self::Target {
        &self.facade
    }
}
