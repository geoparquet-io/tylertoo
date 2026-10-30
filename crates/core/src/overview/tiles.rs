//! The one-shot GeoParquet → PMTiles facade (#439).
//!
//! This module owns the *product policy* of the flagship path — everything
//! `tylertoo tiles` does between "the options are built" and "the archive is
//! on disk" — so the CLI and the Python bindings are thin callers that can
//! never drift:
//!
//! - the whole configuration, [`TilesOptions`] (the convert half's
//!   [`ConvertOptions`] and the export half's [`ExportOptions`]), validated
//!   once and before any input is read ([`TilesOptions::validate`], #779);
//! - the intermediate overview's lifecycle (#314): retained at
//!   [`TilesOptions::keep_overview`], otherwise a temp file in the spill
//!   dir / `$TMPDIR` / the output directory (in that order), removed on
//!   drop — success or failure alike;
//! - the free-space preflight on the volume holding that intermediate (#314);
//! - the declared-minimum-zoom rule (#380) and a data shard's narrower
//!   declaration (#498);
//! - the empty-shard (#498) and empty-coarse (#541) archive handling;
//! - the plan-only run of a fleet's coarse job ([`write_plan`], #560);
//! - the combined convert + export report the one-step run captures.
//!
//! The chain is `overview` convert → `export-pmtiles` through a materialized
//! intermediate overview file — this is NOT zero-disk.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::config::{check_ignored_export_fields, ConfigError, ExportField};
use super::convert::{
    convert_to_overviews, convert_to_overviews_sources, write_convert_plan_sources, ConvertError,
    ConvertOptions, ConvertReport, LevelPlan, PlanReport,
};
use super::export::{
    export_pmtiles, write_empty_archive, ExportError, ExportOptions, ExportReport, FeatureOrder,
};
use super::writer::WriterError;
use crate::input_set::ConvertSource;
use crate::shard::{ShardRole, TileRange};

/// `tylertoo tiles`'s default finest zoom, and [`TilesOptions::default`]'s.
/// (`overview` keeps [`ConvertOptions::default`]'s z6: an overview file is
/// a level ladder, not a full tile pyramid.)
pub const TILES_DEFAULT_MAX_ZOOM: u8 = 14;

/// What the facade reads: a path (file/dir/glob/URL/prefix, resolved by
/// core's input resolution) or a manifest of explicit files/URLs (the CLI's
/// `--files-from`, order preserved verbatim).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TilesInput {
    /// A file, directory, glob, or URL/prefix.
    Path(PathBuf),
    /// A manifest file listing explicit paths/URLs, one per line.
    Manifest(PathBuf),
}

impl TilesInput {
    /// The layer name the archive carries when the caller names none: file
    /// stem for a single file, last path segment for a directory or
    /// `s3://`/`gs://` prefix, last literal segment for a glob, manifest stem for
    /// a manifest (core owns the rules — see
    /// [`crate::input_set::derive_layer_name`]).
    pub fn derive_layer_name(&self) -> String {
        let p = match self {
            TilesInput::Path(p) | TilesInput::Manifest(p) => p,
        };
        crate::input_set::derive_layer_name(&p.to_string_lossy())
    }

    /// Resolve this input to the core's [`ConvertSource`], the one way: a
    /// path through [`ConvertSource::resolve_path`], a manifest through
    /// [`ConvertSource::from_manifest`].
    ///
    /// # Errors
    ///
    /// The input's resolution errors.
    pub fn source(&self) -> Result<ConvertSource, ConvertError> {
        match self {
            TilesInput::Path(p) => Ok(ConvertSource::resolve_path(p)?),
            TilesInput::Manifest(m) => Ok(ConvertSource::from_manifest(m)?),
        }
    }

    /// Run the convert half against this input, dispatching on the input
    /// shape and keeping the typed [`ConvertError`] — the facade's
    /// empty-shard handling (and any caller that routes both shapes) has to
    /// tell "this shard owns no rows" apart from every other failure.
    ///
    /// # Errors
    ///
    /// Whatever [`convert_to_overviews`] /
    /// [`convert_to_overviews_sources`] return, plus the manifest's own
    /// resolution errors for [`TilesInput::Manifest`].
    pub fn convert(
        &self,
        output: &Path,
        options: &ConvertOptions,
    ) -> Result<ConvertReport, ConvertError> {
        options.validate()?;
        match self {
            TilesInput::Path(p) => convert_to_overviews(p, output, options),
            TilesInput::Manifest(_) => {
                convert_to_overviews_sources(&self.source()?, output, options)
            }
        }
    }
}

/// Which job of a sharded fleet this run is, and the slice of the tile space
/// it owns (#498).
///
/// Resolving `--shard`/`--shard-plan` *strings* into this is
/// the caller's argument mapping ([`crate::shard::resolve_range`]); every
/// consequence — the convert's row pruning and digest stamp, the coarse
/// job's convert ceiling, the export range, the declaration, the
/// empty-archive outcomes — is the facade's
/// ([`convert_to_pmtiles`] applies them to its own copy of
/// [`TilesOptions::convert`], so callers never hand-sync the two).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TilesShard {
    /// Coarse job or data shard.
    pub role: ShardRole,
    /// The data shard's range, or `None` for the coarse job — which owns the
    /// zooms below the pivot instead, expressed as an export ceiling.
    pub range: Option<TileRange>,
    /// The fleet's pivot zoom ([`crate::shard::ShardPlan::pivot_zoom`]).
    pub pivot_zoom: u8,
    /// [`crate::shard::ShardPlan::cut_digest_hex`] — the fleet-wide identity
    /// of the cut, stamped into the convert plan's fingerprint so the fleet
    /// is bound to one `shards.json` by construction. The coarse job writes
    /// it into the plan; every data shard must present the same one.
    pub cut_digest: String,
    /// The shard plan this job was cut from, when the caller loaded one: the
    /// facade verifies it against the input it opens (the parts the plan
    /// was cut for) and, once `--max-zoom auto` is resolved on that same
    /// input, checks the pivot against the finest zoom. `None` skips both
    /// (a caller that verified the plan itself, or a test).
    pub binding: Option<ShardBinding>,
}

/// A loaded shard plan and the path it was read from, for
/// [`TilesShard::binding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardBinding {
    /// The plan ([`crate::shard::ShardPlan::load`]).
    pub plan: crate::shard::ShardPlan,
    /// Where it was read from, for the error messages.
    pub path: PathBuf,
}

impl TilesShard {
    /// Verify the job's shard plan against `source`, the input the facade
    /// opened (#498): a plan cut for different parts is refused before any
    /// row is read.
    fn verify_source(&self, source: &ConvertSource) -> Result<(), TilesError> {
        match &self.binding {
            Some(b) => b
                .plan
                .verify_source(source, &b.path)
                .map_err(TilesError::Shard),
            None => Ok(()),
        }
    }

    /// The pivot must not be finer than the build's finest zoom, which is
    /// only known once `--max-zoom auto` is resolved.
    fn check_max_zoom(&self, levels: &LevelPlan) -> Result<(), TilesError> {
        match (&self.binding, levels) {
            (Some(b), LevelPlan::ZoomRange { max_zoom, .. }) => b
                .plan
                .check_max_zoom(*max_zoom, &b.path)
                .map_err(TilesError::Shard),
            _ => Ok(()),
        }
    }
}

impl TilesShard {
    /// What *every* job of a fleet applies to its convert, whether or not it
    /// goes on to build tiles (#498): the data shard's row-pruning range and
    /// the fleet's cut digest. The digest is fingerprinted and the range is
    /// not — the cut is fleet-wide, this job's slice of it is not.
    ///
    /// Separate from the coarse job's build ceiling because a `--plan-only`
    /// run (#560) applies exactly this much: it materializes no level, so it
    /// must not carry a ceiling core would refuse for having no pass 2 to
    /// apply it to. Public for a caller that resolves `--max-zoom auto`
    /// against the shard's own reads before handing the facade its options.
    pub fn apply_selection(&self, convert: &mut ConvertOptions) {
        convert.shard = self.range;
        convert.shard_plan_digest = Some(self.cut_digest.clone());
    }

    /// Every convert-side consequence of a tile-building job (#498/#541):
    /// [`TilesShard::apply_selection`] plus, for the coarse job of a
    /// streaming zoom-plan run, the build ceiling just below the pivot
    /// (#541: pass 1 and the level assignment stay full-range, so any saved
    /// plan is byte-identical to a full run's; `--no-streaming` builds every
    /// level and the export ceiling alone bounds the tiles).
    ///
    /// `keep_finest_level` skips that ceiling: the coverage fill (#771)
    /// draws each coarse tile from the finest level, so a coarse job that
    /// builds the fill needs every level in its intermediate.
    ///
    /// The facade's own (#709): it applies this to its copy of
    /// [`TilesOptions::convert`], so no caller hand-syncs the two.
    /// Idempotent.
    pub(crate) fn apply_to_convert(&self, convert: &mut ConvertOptions, keep_finest_level: bool) {
        self.apply_selection(convert);
        if self.range.is_none()
            && !keep_finest_level
            && matches!(convert.levels, LevelPlan::ZoomRange { .. })
            && convert.streaming
        {
            convert.zoom_ceiling = Some(self.pivot_zoom.saturating_sub(1));
        }
    }
}

/// The whole `tiles` configuration: the convert half, the export half, and
/// the facade's own policy inputs (#779: one config, validated once).
///
/// The declared zoom range (#380) is read from `convert.levels`: a
/// [`LevelPlan::ZoomRange`]'s requested minimum is declared even when the
/// coarsest levels generalized to nothing, and its maximum declares an
/// empty data shard's archive. With [`ConvertOptions::auto_max_zoom`] the
/// facade resolves that maximum first, over the source it converts. A GSD
/// ladder declares no requested range.
///
/// The facade derives a few export fields itself, so they must be left at
/// their defaults ([`TilesOptions::validate`] refuses them otherwise):
/// `export.layer_name` (set [`TilesOptions::layer_name`]), `export.min_zoom`
/// (from the level plan), `export.properties` (the selection is applied at
/// convert: set `convert.properties`), `export.zoom_ceiling` (from the
/// shard) and `export.spill_dir` (the one scratch knob is
/// `convert.spill_dir`). `export.tile_range` is the hand-written export
/// range; a data shard's own range replaces it, and the two together are
/// refused.
///
/// Note: #436 will freeze this shape (`#[non_exhaustive]` + a builder);
/// construct it with `..TilesOptions::default()` so that lands as a
/// non-event.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
#[cfg_attr(feature = "cli", derive(clap::Args))]
pub struct TilesOptions {
    /// The convert half ([`super::convert::convert_to_overviews`]). The
    /// facade reads `spill_dir` from here for the intermediate's location
    /// and the export's member spill (#427 — one scratch knob), `properties`
    /// for the knob-column guard, and `levels` for the declared zoom range.
    /// For a sharded run the facade itself applies
    /// [`TilesOptions::shard`]'s convert-side consequences (`shard`,
    /// `shard_plan_digest`, the coarse `zoom_ceiling`) to its own copy, so
    /// leave those fields alone.
    #[cfg_attr(feature = "cli", arg(skip = Self::default().convert))]
    pub convert: ConvertOptions,
    /// The export half ([`super::export::export_pmtiles`]): the size caps
    /// and valve, the tile buffer and extent, the feature order and id, the
    /// publication metadata, the hand-written tile range. See the type docs
    /// for the fields the facade derives itself.
    #[cfg_attr(feature = "cli", arg(skip = Self::default().export))]
    pub export: ExportOptions,
    /// MVT layer name; `None` derives it from the input
    /// ([`TilesInput::derive_layer_name`]).
    #[cfg_attr(feature = "cli", arg(long))]
    #[cfg_attr(feature = "cli", arg(help = "Layer name for the tiles. Defaults to the input's file stem", long_help = None))]
    pub layer_name: Option<String>,
    /// Retain the intermediate overview at this path instead of a removed
    /// temp file (#314). Its parent directory must exist.
    #[cfg_attr(feature = "cli", arg(long, value_name = "PATH"))]
    #[cfg_attr(feature = "cli", arg(help = "Save the intermediate overview GeoParquet to PATH after export. Does not change the PMTiles output", long_help = None))]
    pub keep_overview: Option<PathBuf>,
    /// The sharded-fleet job, if any (#498).
    #[serde(skip)]
    #[cfg_attr(feature = "cli", arg(skip = Self::default().shard))]
    pub shard: Option<TilesShard>,
}

impl Default for TilesOptions {
    fn default() -> Self {
        Self {
            convert: ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: TILES_DEFAULT_MAX_ZOOM,
                },
                ..ConvertOptions::default()
            },
            export: ExportOptions::default(),
            layer_name: None,
            keep_overview: None,
            shard: None,
        }
    }
}

/// Export fields the facade derives itself, for
/// [`check_ignored_export_fields`], with what to set instead. Kept beside [`TilesOptions`]'s docs, which list the same five.
const FACADE_OWNED_EXPORT_FIELDS: [(ExportField, &str); 5] = [
    (
        ExportField::LayerName,
        "the facade names the layer; set TilesOptions::layer_name",
    ),
    (
        ExportField::MinZoom,
        "the facade declares the level plan's minimum zoom",
    ),
    (
        ExportField::Properties,
        "the property selection is applied at convert; set convert.properties",
    ),
    (
        ExportField::ZoomCeiling,
        "a coarse shard job's ceiling comes from TilesOptions::shard",
    ),
    (
        ExportField::SpillDir,
        "the one scratch directory is convert.spill_dir",
    ),
];

impl TilesOptions {
    /// The export defaults of a `tiles` run (#761): [`ExportOptions::default`],
    /// or with `verbatim` [`ExportOptions::verbatim`] — no size cap, since a
    /// valve that sheds features to fit a byte budget is not verbatim
    /// either. An explicit cap set on the result still applies. The CLI's
    /// `tiles --verbatim` and the Python `convert(verbatim=True)` both start
    /// here.
    pub fn export_defaults(verbatim: bool) -> ExportOptions {
        let export = ExportOptions::default();
        if verbatim {
            export.verbatim()
        } else {
            export
        }
    }

    /// Check the whole configuration before any input is read (#779): the
    /// convert half ([`ConvertOptions::validate`]), the export half
    /// ([`ExportOptions::validate`]), and the facade's own rules — the
    /// export fields it derives itself, the `keep_overview` directory
    /// (#433), the feature-order / feature-id columns against the property
    /// selection (#386/#443), a hand-written tile range beside a data
    /// shard's, and a coarse shard job that owns no zoom (#498).
    ///
    /// [`convert_to_pmtiles`] runs this first. A caller with expensive work
    /// of its own before the facade (resolving `--max-zoom auto`, binding a
    /// shard plan) calls it up front too.
    ///
    /// # Errors
    ///
    /// The first rule broken; [`TilesError::is_config`] is true for each.
    /// Validate these options and return the immutable engine configuration.
    pub fn validate(&self) -> Result<super::config::ValidatedConfig<'_, Self>, TilesError> {
        super::config::validate_tiles(self)
    }

    pub(crate) fn check(&self) -> Result<(), TilesError> {
        self.validate_for(Run::Tiles)
    }

    /// [`TilesOptions::validate`] for a plan-only run ([`write_plan`],
    /// #560): the same rules, except that a coarse shard's pivot may sit at
    /// the minimum zoom (nothing is built), and a data shard and a missing
    /// `convert.save_plan` are refused.
    ///
    /// # Errors
    ///
    /// The first rule broken.
    pub fn validate_plan_only(&self) -> Result<(), TilesError> {
        self.validate_for(Run::PlanOnly)
    }

    /// The export options a plan-only run (#560) ignores: every field of
    /// [`TilesOptions::export`] that differs from `defaults` (the
    /// [`TilesOptions::export_defaults`] the caller started from, which a
    /// value equal to is the same run as no value), plus the layer name and
    /// the kept overview. Named by their CLI flags, in `--help` order.
    ///
    /// None of these is a convert option, and the plan's fingerprint is an
    /// allowlist of convert options, so a plan-only run writes the plan the
    /// full coarse job writes with them.
    pub fn ignored_by_plan_only(&self, defaults: &ExportOptions) -> Vec<&'static str> {
        let e = &self.export;
        let m = &e.metadata;
        [
            ("--layer-name", self.layer_name.is_some()),
            ("--name", m.name.is_some()),
            ("--description", m.description.is_some()),
            ("--attribution", m.attribution.is_some()),
            (
                "--max-tile-size",
                e.tile_size_limit != defaults.tile_size_limit,
            ),
            (
                "--max-compressed-tile-size",
                e.compressed_tile_size_limit != defaults.compressed_tile_size_limit,
            ),
            ("--size-valve", e.size_valve != defaults.size_valve),
            ("--coverage-fill", e.coverage_fill != defaults.coverage_fill),
            (
                "--no-simple-clip-fastpath",
                e.simple_clip_fastpath != defaults.simple_clip_fastpath,
            ),
            ("--tile-buffer", e.tile_buffer != defaults.tile_buffer),
            ("--extent", e.extent != defaults.extent),
            (
                "--partition-wave",
                e.partition_wave != defaults.partition_wave,
            ),
            ("--feature-order", e.feature_order != defaults.feature_order),
            ("--feature-id", e.feature_id.is_some()),
            ("--keep-overview", self.keep_overview.is_some()),
            ("--tile-range", e.tile_range.is_some()),
        ]
        .into_iter()
        .filter_map(|(flag, set)| set.then_some(flag))
        .collect()
    }

    fn validate_for(&self, run: Run) -> Result<(), TilesError> {
        // The shard rules first: they name the precise misuse (a plan-only
        // data shard), where convert's own check would only say `--shard`
        // and `--save-plan` exclude each other.
        if let Some(job) = &self.shard {
            if let Some(range) = job.range.filter(|r| r.pivot_zoom() != job.pivot_zoom) {
                return Err(ConfigError::ShardPivotMismatch {
                    pivot: job.pivot_zoom,
                    range_pivot: range.pivot_zoom(),
                }
                .into());
            }
            if self.export.tile_range.is_some() {
                return Err(ConfigError::TileRangeWithShard.into());
            }
            if job.range.is_some() && run == Run::PlanOnly {
                return Err(ConfigError::PlanOnlyDataShard.into());
            }
            // The coarse job owns [min_zoom, pivot - 1]. #560: not under
            // plan-only, which builds no zoom at all — a pivot AT the minimum
            // is exactly the handover shape (an external archive owns every
            // zoom below the pivot), the one build plan-only exists for.
            if let (None, Run::Tiles, LevelPlan::ZoomRange { min_zoom, .. }) =
                (job.range, run, &self.convert.levels)
            {
                if job.pivot_zoom <= *min_zoom {
                    return Err(ConfigError::CoarseShardHasNoZoom {
                        pivot: job.pivot_zoom,
                        min_zoom: *min_zoom,
                    }
                    .into());
                }
            }
        }
        self.convert.validate().map_err(TilesError::Convert)?;
        self.export.validate()?;
        check_ignored_export_fields(&self.export, "tiles", &FACADE_OWNED_EXPORT_FIELDS)?;
        check_keep_overview(self.keep_overview.as_deref())?;
        // #386/#443: the property selection is applied at convert, so an
        // excluded column is already gone from the intermediate before export
        // would sort or id by it.
        check_knob_columns(
            &self.convert,
            &self.export.feature_order,
            self.export.feature_id.as_deref(),
        )?;
        if run == Run::PlanOnly && self.convert.save_plan.is_none() {
            return Err(ConfigError::PlanOnlyWithoutSavePlan.into());
        }
        Ok(())
    }
}

/// Which run [`TilesOptions::validate_for`] checks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Run {
    /// A full convert + export.
    Tiles,
    /// [`write_plan`]: pass 1 and the assignment only.
    PlanOnly,
}

/// The export tile range for a run: a shard's own range where it has one,
/// otherwise the caller's hand-written `export.tile_range`.
///
/// A data shard's range prunes the convert's reads as well as the export;
/// the two together are refused by [`TilesOptions::validate`]. A *coarse*
/// shard has `range: None`, so the caller's range applies there.
fn resolve_tile_range(
    shard: Option<&TilesShard>,
    tile_range: Option<TileRange>,
) -> Option<TileRange> {
    match shard {
        Some(job) => job.range.or(tile_range),
        None => tile_range,
    }
}

/// The intermediate overview file a [`convert_to_pmtiles`] run materialized
/// (#314).
///
/// Its disk cost must not be silent, so the path, size and fate are
/// reported — mid-run through [`TilesObserver::overview_written`], and on
/// the final [`TilesReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntermediateOverview {
    /// Where the intermediate overview was written.
    pub path: PathBuf,
    /// Its size in bytes right after the convert (0 if unreadable).
    pub bytes: u64,
    /// `true` when [`TilesOptions::keep_overview`] retains it; `false` for a
    /// temp file removed after the export.
    pub retained: bool,
    /// The #314 free-space preflight's warning, when the volume holding the
    /// intermediate looked too small for it. Already logged (`log::warn`)
    /// before the convert started; carried here too. `None` when the volume
    /// fits or either side of the comparison is unknowable. A caller that
    /// must surface it before the run calls [`intermediate_space_preflight`].
    pub space_warning: Option<String>,
}

/// The combined report of a full [`convert_to_pmtiles`] run.
///
/// Serializes to exactly `{"convert": ..., "export": ...}` — the two halves
/// the two-step chain would write (`overview --report` +
/// `export-pmtiles --report`).
#[derive(Debug, Clone, Serialize)]
pub struct TilesReport {
    /// The convert half.
    pub convert: ConvertReport,
    /// The export half.
    pub export: ExportReport,
    /// The intermediate overview (not part of the serialized report).
    #[serde(skip)]
    pub intermediate: IntermediateOverview,
}

impl TilesReport {
    /// Zooms inside the declared range that hold no tiles (#380): the
    /// summary covers the requested range, which can be wider than the
    /// archive's own PMTiles header — these are the zooms a caller should
    /// say hold nothing rather than imply they do.
    pub fn empty_declared_zooms(&self) -> Vec<u8> {
        self.convert
            .skipped_empty_levels
            .iter()
            .filter_map(|l| l.zoom)
            .collect()
    }
}

/// A valid tile-less archive written in place of a convert that had nothing
/// to write: an empty data shard (#498) or an empty coarse job (#541).
///
/// Both are legal outcomes of a legal cut, not failures — a fleet with red
/// squares that mean "correct" would need hand-editing at merge.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EmptyArchive {
    /// Output counts are present on empty outcomes too.
    pub total_tiles: usize,
    /// Output feature count.
    pub total_tile_features: usize,
    /// Rows in this run's selected input stream.
    pub input_rows: u64,
    /// Half-open original input row ranges in this run's accounting domain.
    pub source_row_ranges: Vec<(u64, u64)>,
    /// Diagnostics from this conversion, including when pass 2 emptied it.
    pub diagnostics: Vec<crate::diagnostics::Diagnostic>,
    /// Losses before the empty outcome, in original input row coordinates.
    pub source_losses: crate::diagnostics::SourceLosses,
    /// Which job of the fleet this was.
    pub role: ShardRole,
    /// The fleet's pivot zoom.
    pub pivot_zoom: u8,
    /// Coarsest zoom the empty archive declares.
    pub min_zoom: u8,
    /// Finest zoom the empty archive declares.
    pub max_zoom: u8,
    /// The convert's own words for why there was nothing to write.
    pub why: String,
}

/// What a successful [`convert_to_pmtiles`] run produced.
#[derive(Debug)]
pub enum TilesOutcome {
    /// The full chain ran; the archive holds tiles (or legitimately none)
    /// and both halves reported.
    Converted(Box<TilesReport>),
    /// A data shard whose range owns no input rows wrote a valid empty
    /// archive and succeeded (#498).
    EmptyShard(EmptyArchive),
    /// The coarse job had nothing at or below its ceiling and wrote a valid
    /// empty archive over the zooms it owns (#541).
    EmptyCoarse(EmptyArchive),
}

/// Mid-run notifications from [`convert_to_pmtiles_observed`]. Every method
/// has a no-op default, so an observer implements only what it reports.
pub trait TilesObserver {
    /// The convert half finished and the intermediate overview is on disk;
    /// the export half is about to run. The one moment a user-facing caller
    /// should name the intermediate's path and size (#314) — after a failed
    /// export the file's location matters most.
    fn overview_written(&mut self, _intermediate: &IntermediateOverview, _convert: &ConvertReport) {
    }
}

/// The no-op observer [`convert_to_pmtiles`] runs with.
impl TilesObserver for () {}

/// Errors from [`convert_to_pmtiles`] and [`write_plan`].
///
/// `#[non_exhaustive]`: new failure modes are added without a breaking
/// change; match with a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TilesError {
    /// The convert half failed. Not a `#[source]` chain: the message *is*
    /// the convert error's, prefixed, the way the CLI has always printed it.
    /// A refused option ([`ConvertError::is_config`]) is not prefixed: it is
    /// refused before any conversion starts (#779).
    #[error("{prefix}{0}", prefix = failed_prefix(.0.is_config(), "overview conversion failed: "))]
    Convert(ConvertError),
    /// The export half failed; prefixed like [`TilesError::Convert`].
    #[error("{prefix}{0}", prefix = failed_prefix(.0.is_config(), "export failed: "))]
    Export(ExportError),
    /// An option refused before any work ([`TilesOptions::validate`], #779).
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The output cannot be written (#452: checked before any work).
    #[error(transparent)]
    Output(#[from] crate::atomic_output::OutputNotWritable),
    /// Creating the removed-after-export intermediate overview failed.
    #[error(
        "failed to create the intermediate overview file in {} (location precedence: \
         --spill-dir, $TMPDIR, the output directory)",
        dir.display()
    )]
    IntermediateCreate {
        /// The directory the temp file was to be created in.
        dir: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// Writing an empty data shard's archive failed (#498).
    #[error("failed to write the empty shard archive: {0}")]
    EmptyShardArchive(ExportError),
    /// Writing an empty coarse job's archive failed (#541).
    #[error("failed to write the empty coarse archive: {0}")]
    EmptyCoarseArchive(ExportError),
    /// The job's shard plan does not fit the input or the build (#498):
    /// cut for other parts, or a pivot finer than the resolved finest zoom.
    #[error(transparent)]
    Shard(crate::shard::ShardError),
}

/// `prefix`, unless the error is a refused option, which names no step.
fn failed_prefix(is_config: bool, prefix: &'static str) -> &'static str {
    if is_config {
        ""
    } else {
        prefix
    }
}

impl TilesError {
    /// Whether this is an option the caller must fix, refused before any
    /// input was read, as opposed to a problem with the data or the run.
    /// The Python bindings raise these as `tylertoo.ConfigError`.
    pub fn is_config(&self) -> bool {
        match self {
            TilesError::Config(_) => true,
            TilesError::Convert(e) => e.is_config(),
            TilesError::Export(e) => e.is_config(),
            _ => false,
        }
    }
}

/// The `keep_overview` directory preflight (#433): a retained intermediate
/// into a missing directory is refused before any work, not after a
/// minutes-long convert.
fn check_keep_overview(keep_overview: Option<&Path>) -> Result<(), ConfigError> {
    if let Some(path) = keep_overview {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if !parent.is_dir() {
                return Err(ConfigError::KeepOverviewDirMissing {
                    dir: parent.to_path_buf(),
                });
            }
        }
    }
    Ok(())
}

/// The feature-order / feature-id vs property-selection guard (#386/#443).
fn check_knob_columns(
    options: &ConvertOptions,
    feature_order: &FeatureOrder,
    feature_id: Option<&str>,
) -> Result<(), ConfigError> {
    if let FeatureOrder::Column { name, .. } = feature_order {
        if !options.properties.keeps(name) {
            return Err(ConfigError::FeatureOrderColumnExcluded { name: name.clone() });
        }
    }
    // #443: convert-time-only. This does not apply to export-pmtiles's OWN
    // property selection, which `ExportOptions::feature_id` always overrides
    // regardless (the column already exists in the intermediate either way).
    if let Some(name) = feature_id {
        if !options.properties.keeps(name) {
            return Err(ConfigError::FeatureIdColumnExcluded {
                name: name.to_string(),
            });
        }
        if let Some(spec) = options.accumulate.iter().find(|a| a.column == *name) {
            return Err(ConfigError::FeatureIdAccumulated {
                name: name.to_string(),
                op: spec.op.as_str().to_string(),
            });
        }
    }
    Ok(())
}

/// The plan-only run of a fleet's coarse job (#560): pass 1 and the level
/// assignment, written to `convert.save_plan`; no intermediate overview, no
/// export, no archive.
///
/// Takes the same [`TilesOptions`] the full coarse job runs with, so the
/// plan is byte-identical to the one that job writes. The export options
/// are ignored but not unchecked: a value the full job would refuse before
/// reading anything is refused here too, so the plan-only run stays the
/// fleet's preflight. [`TilesOptions::ignored_by_plan_only`] names them.
/// A coarse shard's pivot may sit at the minimum zoom here (the handover
/// shape), since nothing is built.
///
/// # Errors
///
/// [`TilesOptions::validate`]'s, plus [`ConfigError::PlanOnlyDataShard`]
/// and [`ConfigError::PlanOnlyWithoutSavePlan`], then the convert's.
pub fn write_plan(input: &TilesInput, options: &TilesOptions) -> Result<PlanReport, TilesError> {
    options.validate_for(Run::PlanOnly)?;
    let convert = match &options.shard {
        Some(job) => {
            let mut convert = options.convert.clone();
            job.apply_selection(&mut convert);
            Cow::Owned(convert)
        }
        None => Cow::Borrowed(&options.convert),
    };
    let source = input.source().map_err(TilesError::Convert)?;
    let convert = resolve_on_source(&source, options.shard.as_ref(), convert)?;
    write_convert_plan_sources(&source, &convert).map_err(TilesError::Convert)
}

/// The facade's one input-reading preamble, on the source it then converts:
/// verify the shard plan against it, resolve `--max-zoom auto` over it
/// ([`super::auto_zoom::resolve_auto_max_zoom`], so the estimate and the
/// conversion share one source and a remote sample is fetched once, #788),
/// then check the shard pivot against the resolved finest zoom.
fn resolve_on_source<'a>(
    source: &ConvertSource,
    shard: Option<&TilesShard>,
    convert: Cow<'a, ConvertOptions>,
) -> Result<Cow<'a, ConvertOptions>, TilesError> {
    if let Some(job) = shard {
        job.verify_source(source)?;
    }
    // The estimate's staged sample lands in the spill; place it where the
    // conversion will (#272) before the first chunk creates the file.
    source.set_spill_dir(convert.spill_dir.as_deref());
    let convert = match super::auto_zoom::resolve_auto_max_zoom(source, &convert, None)
        .map_err(TilesError::Convert)?
    {
        Some(resolved) => Cow::Owned(resolved),
        None => convert,
    };
    if let Some(job) = shard {
        job.check_max_zoom(&convert.levels)?;
    }
    Ok(convert)
}

/// Convert GeoParquet to a PMTiles archive in one call, with the default
/// no-op [`TilesObserver`]. See [`convert_to_pmtiles_observed`].
///
/// # Errors
///
/// See [`TilesError`].
pub fn convert_to_pmtiles(
    input: &TilesInput,
    output: &Path,
    options: &TilesOptions,
) -> Result<TilesOutcome, TilesError> {
    convert_to_pmtiles_observed(input, output, options, &mut ())
}

/// Convert GeoParquet to a PMTiles archive in one call: the `tylertoo tiles`
/// facade (#439).
///
/// Chains the convert ([`super::convert::convert_to_overviews`]) into a
/// materialized intermediate overview, then the export
/// ([`super::export::export_pmtiles`]) into `output`, owning the policy
/// listed in the [module docs](self).
///
/// `observer` hears about the intermediate overview between the two halves.
///
/// # Errors
///
/// See [`TilesError`]. An empty data shard and an empty coarse job are NOT
/// errors: they return [`TilesOutcome::EmptyShard`] /
/// [`TilesOutcome::EmptyCoarse`] with a valid tile-less archive written.
pub fn convert_to_pmtiles_observed(
    input: &TilesInput,
    output: &Path,
    options: &TilesOptions,
    observer: &mut dyn TilesObserver,
) -> Result<TilesOutcome, TilesError> {
    let config = options.validate()?;
    convert_to_pmtiles_validated(input, output, &config, observer)
}

fn convert_to_pmtiles_validated(
    input: &TilesInput,
    output: &Path,
    config: &super::config::ValidatedConfig<'_, TilesOptions>,
    observer: &mut dyn TilesObserver,
) -> Result<TilesOutcome, TilesError> {
    let options = config.options();
    // #452: before the intermediate is created — which would otherwise be
    // the first thing to trip over a missing output directory, with a
    // message about the intermediate — and long before the export.
    crate::atomic_output::check_writable(output)?;

    // #498/#541: the convert-side consequences of a sharded job are the
    // facade's, not the caller's. Applied to our own copy, so a Rust caller
    // that sets only `TilesOptions::shard` still gets the data shard's row
    // pruning, the fleet digest the merge binding needs, and the coarse
    // job's ceiling. #771: a coarse job that draws the coverage fill keeps
    // every level, since the fill is built from the finest.
    let convert_options = match &options.shard {
        Some(job) => {
            let mut convert = options.convert.clone();
            job.apply_to_convert(&mut convert, options.export.coverage_fill);
            Cow::Owned(convert)
        }
        None => Cow::Borrowed(&options.convert),
    };
    // The input is opened once: the shard binding, the `auto` estimate and
    // the conversion all read this source. Before the declared zoom range
    // below, which needs the resolved finest zoom.
    let source = input.source().map_err(TilesError::Convert)?;
    let convert_options = resolve_on_source(&source, options.shard.as_ref(), convert_options)?;

    // The declared zoom range (#380): only a zoom plan has one. `None` for a
    // GSD ladder — it declares no requested range, and the empty-archive
    // outcomes below fall back to the pivot it inherently knows.
    let zoom_plan = match convert_options.levels {
        LevelPlan::ZoomRange { min_zoom, max_zoom } => Some((min_zoom, max_zoom)),
        LevelPlan::Gsds(_) => None,
    };

    // Derive the layer name from the input if not given (core owns the
    // rules — see input_set::derive_layer_name).
    let layer_name = options
        .layer_name
        .clone()
        .unwrap_or_else(|| input.derive_layer_name());

    // A data shard's range prunes the convert's reads as well as the export;
    // a hand-written range restricts the export only (validate refuses the
    // two together).
    let tile_range = resolve_tile_range(options.shard.as_ref(), options.export.tile_range);

    // The data shard's own range, kept past `tile_range`'s move into
    // `ExportOptions`: the empty-shard branch below needs the pivot zoom.
    let shard_range = options.shard.as_ref().and_then(|job| job.range);
    let metadata = &options.export.metadata;

    // Intermediate overview file (#314): retained at `keep_overview` when
    // given; otherwise a temp file in the spill dir / $TMPDIR / the output
    // directory, removed on drop — success or failure alike. A TempPath, not
    // an open NamedTempFile: convert publishes by renaming a sibling over
    // this path, which Windows refuses while a handle to it is open (#678).
    let (overview_path, overview_tmp): (PathBuf, Option<tempfile::TempPath>) =
        match &options.keep_overview {
            // Its directory was checked above.
            Some(path) => (path.clone(), None),
            None => {
                let dir = intermediate_dir(options, output);
                let tmp = tempfile::Builder::new()
                    .prefix(".tylertoo-overview-")
                    .suffix(".parquet")
                    .tempfile_in(&dir)
                    .map_err(|source| TilesError::IntermediateCreate {
                        dir: dir.clone(),
                        source,
                    })?
                    .into_temp_path();
                (tmp.to_path_buf(), Some(tmp))
            }
        };

    // Shared disk-footprint preflight: sum staging, pass-2 spill and the
    // intermediate on each destination volume. Warn before execution. Logged here
    // and carried in the report; a caller that must surface it before the
    // run (#709: Python's `UserWarning`) calls `intermediate_space_preflight`.
    let diagnostics = crate::diagnostics::Diagnostics::default();
    let source = input.source().map_err(TilesError::Convert)?;
    let space_warning = super::disk::DiskFootprint::conversion(
        &source,
        None,
        &convert_options,
        Some(&overview_path),
    )
    .ok()
    .and_then(|footprint| footprint.warning(CLI_REMEDIES));
    if let Some(msg) = &space_warning {
        log::warn!("{msg}");
    }

    let convert_report = match super::convert::convert_to_overviews_source_diagnostics(
        &source,
        &overview_path,
        &convert_options,
        super::stream::Pass2Strategy::Pipelined,
        &diagnostics,
    ) {
        Ok(report) => report,
        Err(e) => {
            // #498: a shard whose range owns no input rows is a legal
            // outcome of a legal cut, not a failure. `shard-plan`
            // deliberately cuts N ranges whatever the data looks like — an
            // empty RANGE is legal while a gap is not — so a concentrated
            // dataset routinely leaves some shards with nothing. Failing
            // them would mean an array job with red squares in it that mean
            // "correct", and a merge the operator has to hand-edit. Write
            // the valid empty archive the merge already tolerates and
            // succeed.
            if convert_produced_nothing(&e) {
                if let (Some(job), Some(range)) = (&options.shard, shard_range) {
                    // The shard owns [pivot, finest]; a GSD ladder has no
                    // finest zoom to declare, so the pivot stands alone.
                    let max_zoom = zoom_plan.map_or_else(|| range.pivot_zoom(), |(_, max)| max);
                    write_empty_archive(
                        output,
                        &layer_name,
                        range.pivot_zoom(),
                        max_zoom,
                        metadata,
                    )
                    .map_err(TilesError::EmptyShardArchive)?;
                    log::info!(
                        "[tiles] shard {} owns no input rows ({e}); wrote an empty archive",
                        job.role
                    );
                    return Ok(TilesOutcome::EmptyShard(EmptyArchive {
                        total_tiles: 0,
                        total_tile_features: 0,
                        input_rows: diagnostics.input_row_count(),
                        source_row_ranges: diagnostics.source_row_ranges(),
                        diagnostics: diagnostics.snapshot(),
                        source_losses: diagnostics
                            .source_losses()
                            .map_err(ConvertError::from)
                            .map_err(TilesError::Convert)?,
                        role: job.role,
                        pivot_zoom: job.pivot_zoom,
                        min_zoom: range.pivot_zoom(),
                        max_zoom,
                        why: e.to_string(),
                    }));
                }
            }
            // #541: the coarse job's counterpart. Its convert stops at the
            // pivot, and when every feature first appears finer than that
            // (#211 auto-clamp took every coarse level) there is nothing for
            // it to write — but pass 1, the assignment and any saved plan
            // all ran and are valid, and the shards still have their work.
            // Same legal outcome as an empty data shard, same answer.
            if matches!(e, ConvertError::NothingAtOrBelowCeiling { .. }) {
                if let Some(job) = options.shard.as_ref().filter(|job| job.range.is_none()) {
                    // A ceiling needs a zoom plan (core refuses it on a GSD
                    // ladder), so the fallback is unreachable in practice.
                    let min_zoom = zoom_plan.map_or(0, |(min, _)| min);
                    let max_zoom = job.pivot_zoom.saturating_sub(1);
                    write_empty_archive(output, &layer_name, min_zoom, max_zoom, metadata)
                        .map_err(TilesError::EmptyCoarseArchive)?;
                    log::info!(
                        "[tiles] shard job {} has nothing to write ({e}); wrote an empty archive",
                        job.role
                    );
                    return Ok(TilesOutcome::EmptyCoarse(EmptyArchive {
                        total_tiles: 0,
                        total_tile_features: 0,
                        input_rows: diagnostics.input_row_count(),
                        source_row_ranges: diagnostics.source_row_ranges(),
                        diagnostics: diagnostics.snapshot(),
                        source_losses: diagnostics
                            .source_losses()
                            .map_err(ConvertError::from)
                            .map_err(TilesError::Convert)?,
                        role: job.role,
                        pivot_zoom: job.pivot_zoom,
                        min_zoom,
                        max_zoom,
                        why: e.to_string(),
                    }));
                }
            }
            return Err(TilesError::Convert(e));
        }
    };

    // #314: the disk cost of the one-shot facade must not be silent — the
    // observer gets the intermediate's path, size and fate before the export
    // starts, so a failed export still named them.
    let overview_bytes = std::fs::metadata(&overview_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let intermediate = IntermediateOverview {
        path: overview_path.clone(),
        bytes: overview_bytes,
        retained: overview_tmp.is_none(),
        space_warning,
    };
    observer.overview_written(&intermediate, &convert_report);

    let export_opts = ExportOptions {
        layer_name,
        // #380: the archive's `vector_layers` declares the zoom range that
        // was asked for, even when the coarsest levels generalized to
        // nothing and were omitted from the overview (the header stays the
        // actual tiles, #529/#522). Only a zoom plan has a requested minimum
        // zoom.
        //
        // #498: a data shard owns no zoom coarser than the pivot, so that is
        // what it declares — claiming the coarse job's half would
        // misdescribe an archive that holds none of it.
        min_zoom: zoom_plan.map(|(min, _)| match tile_range {
            Some(r) => r.pivot_zoom(),
            None => min,
        }),
        // The property selection was applied on convert (#386): the
        // intermediate overview already carries only the kept columns.
        properties: Default::default(),
        // #498: the two complementary halves of a sharded build. A data
        // shard (or a hand-written tile range) takes its range; the coarse
        // job takes everything below the pivot.
        tile_range,
        zoom_ceiling: options.shard.as_ref().and_then(|job| {
            job.range
                .is_none()
                .then(|| job.pivot_zoom.saturating_sub(1))
        }),
        // #427: one scratch knob — the export's member spill goes where the
        // convert's spill (and the intermediate overview) go.
        spill_dir: options.convert.spill_dir.clone(),
        ..options.export.clone()
    };
    let export_report =
        export_pmtiles(&overview_path, output, &export_opts).map_err(TilesError::Export)?;

    Ok(TilesOutcome::Converted(Box::new(TilesReport {
        convert: convert_report,
        export: export_report,
        intermediate,
    })))
}

/// `true` when a convert failed only because it had nothing to write.
///
/// Two spellings of the same outcome, depending on how far the run got before
/// it noticed: `NoData` when no level has a winner, `AllLevelsEmpty` when
/// every declared level turned out empty at write time. Both mean "zero rows
/// reached the output", which for a data shard is not an error at all.
fn convert_produced_nothing(e: &ConvertError) -> bool {
    matches!(
        e,
        ConvertError::NoData | ConvertError::Writer(WriterError::AllLevelsEmpty { .. })
    )
}

/// Where the removed-after-export intermediate overview lives (#314): the
/// convert's spill dir if set (one knob for everything tylertoo puts on
/// scratch disk), else an explicitly-set non-empty `$TMPDIR`, else next to
/// the output (same filesystem as the final artifact), else the process
/// temp dir. Pure — the env value is injected — so the precedence is
/// unit-testable.
/// Where the facade puts the intermediate overview: `keep_overview`'s
/// directory when retained, else [`resolve_intermediate_dir`]'s choice.
fn intermediate_dir(options: &TilesOptions, output: &Path) -> PathBuf {
    match &options.keep_overview {
        Some(path) => path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        None => resolve_intermediate_dir(
            options.convert.spill_dir.as_deref(),
            std::env::var_os("TMPDIR").as_deref(),
            output,
        ),
    }
}

/// The CLI's names for the #314 remedies, which the facade's log uses.
pub(super) const CLI_REMEDIES: SpaceRemedies = SpaceRemedies {
    spill_dir: "--spill-dir",
    keep_overview: "--keep-overview",
};

/// How a caller spells the two #314 remedies in
/// [`intermediate_space_preflight`]'s warning: `--spill-dir` on the CLI,
/// `spill_dir` in Python.
#[derive(Debug, Clone, Copy)]
pub struct SpaceRemedies<'a> {
    /// The spill-directory option.
    pub spill_dir: &'a str,
    /// The keep-the-overview option.
    pub keep_overview: &'a str,
}

/// The shared conversion disk-footprint preflight on its own (#709).
///
/// The warning [`convert_to_pmtiles`] logs, for a caller that must surface
/// it before the run, as Python's `UserWarning` does, in its own spelling
/// of the remedies. Includes staging, raw pass-2 spill and the intermediate,
/// grouped by destination volume. The facade keeps this compatibility entry
/// point so Python can warn before releasing the GIL.
///
/// `None` when the volume fits, or when either side of the comparison is
/// unknowable (an unreadable footer or an exotic filesystem) rather than crying
/// wolf.
pub fn intermediate_space_preflight(
    input: &TilesInput,
    output: &Path,
    options: &TilesOptions,
    remedies: SpaceRemedies<'_>,
) -> Option<String> {
    let source = input.source().ok()?;
    let dir = intermediate_dir(options, output);
    super::disk::DiskFootprint::conversion(
        &source,
        None,
        &options.convert,
        Some(&dir.join("overview.parquet")),
    )
    .ok()?
    .warning(remedies)
}

fn resolve_intermediate_dir(
    spill_dir: Option<&Path>,
    tmpdir_env: Option<&std::ffi::OsStr>,
    output: &Path,
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
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::testutil::write_input;
    use crate::TilesetMetadata;
    use geo::{Geometry, LineString, Point, Polygon};

    fn points_input(path: &Path) {
        let geoms: Vec<Option<Geometry<f64>>> = vec![
            Some(Geometry::Point(Point::new(0.5, 0.5))),
            Some(Geometry::Point(Point::new(10.0, 20.0))),
            Some(Geometry::Point(Point::new(-30.0, -10.0))),
        ];
        write_input(path, &geoms, false, None);
    }

    /// Publication metadata for the empty-archive tests: an empty job
    /// writes the same `name`/`attribution` a job with data would, so a
    /// fleet whose shards are all empty still carries its credit.
    fn grid_metadata() -> TilesetMetadata {
        TilesetMetadata::new()
            .with_name("Grid")
            .with_attribution("<b>Survey</b>")
    }

    fn assert_grid_metadata(path: &Path) {
        let json = crate::archive_index::ArchiveIndex::open(path)
            .unwrap()
            .metadata_json()
            .unwrap();
        assert_eq!(json["name"], "Grid");
        assert_eq!(json["attribution"], "<b>Survey</b>");
        assert!(json.get("description").is_none(), "unset stays absent");
    }

    fn zoomed_options(min_zoom: u8, max_zoom: u8) -> TilesOptions {
        TilesOptions {
            convert: ConvertOptions {
                levels: LevelPlan::ZoomRange { min_zoom, max_zoom },
                ..ConvertOptions::default()
            },
            ..TilesOptions::default()
        }
    }

    /// #498: a data shard whose range owns none of the input's rows is a
    /// legal outcome, not a failure — a valid tile-less archive and
    /// [`TilesOutcome::EmptyShard`] declaring `[pivot, finest]`, so `merge`
    /// can exclude it by reading it. Exercised here and not only through
    /// the CLI's `sharded_builds`, which the quick tier does not run.
    #[test]
    fn an_empty_data_shard_writes_a_tile_less_archive() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        // A bbox covering, so the shard's range can prune this row group
        // away entirely — which is what leaves the convert with no rows at
        // all, the condition this outcome is about.
        let geoms: Vec<Option<Geometry<f64>>> = vec![
            Some(Geometry::Point(Point::new(0.5, 0.5))),
            Some(Geometry::Point(Point::new(10.0, 20.0))),
            Some(Geometry::Point(Point::new(-30.0, -10.0))),
        ];
        write_input(&input, &geoms, true, None);
        let output = dir.path().join("out.pmtiles");

        // #498: every job of a fleet consumes ONE dataset-global plan, so
        // the shard below needs a real one — written here by the run that
        // reads the whole input, under the same cut digest the shard
        // presents (the fingerprint binds the two).
        let plan = dir.path().join("convert.plan");
        let digest = "0123456789abcdef".to_string();
        let fleet_convert = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 6,
            },
            shard_plan_digest: Some(digest.clone()),
            ..ConvertOptions::default()
        };
        super::super::convert::convert_to_overviews(
            &input,
            dir.path().join("plan-run.parquet"),
            &ConvertOptions {
                save_plan: Some(plan.clone()),
                ..fleet_convert.clone()
            },
        )
        .expect("the plan-writing run must succeed");

        // The fixture's points all sit near the middle of the world; the
        // Hilbert curve starts in a corner, so the first pivot-zoom ids own
        // none of them.
        let base = crate::tile::hilbert_zoom_base(4);
        let range = TileRange::new(4, base, base + 1).expect("a valid pivot range");
        let options = TilesOptions {
            convert: ConvertOptions {
                plan: Some(plan),
                ..fleet_convert
            },
            shard: Some(TilesShard {
                role: ShardRole::Shard {
                    index: 3,
                    shards: 4,
                },
                range: Some(range),
                pivot_zoom: 4,
                cut_digest: digest,
                binding: None,
            }),
            export: ExportOptions {
                metadata: grid_metadata(),
                ..ExportOptions::default()
            },
            ..TilesOptions::default()
        };

        let outcome = convert_to_pmtiles(&TilesInput::Path(input), &output, &options)
            .expect("an empty shard must succeed");

        let TilesOutcome::EmptyShard(empty) = outcome else {
            panic!("expected an empty data shard, got {outcome:?}");
        };
        assert_eq!(
            empty.role,
            ShardRole::Shard {
                index: 3,
                shards: 4
            }
        );
        assert_eq!(empty.pivot_zoom, 4);
        // A data shard owns [pivot, finest]: the pivot it was cut at, and
        // the zoom plan's maximum — never the plan's minimum, which belongs
        // to the coarse job.
        assert_eq!(empty.min_zoom, 4, "declares the pivot, not the plan's min");
        assert_eq!(empty.max_zoom, 6, "declares the zoom plan's maximum");
        assert!(!empty.why.is_empty(), "carries the convert's own words");
        let json = serde_json::to_value(&empty).unwrap();
        assert_eq!(json["total_tiles"], 0);
        assert!(json["diagnostics"].is_array());
        assert!(json["source_losses"].is_array());

        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[..8], b"PMTiles\x03", "a valid, readable archive");
        assert_grid_metadata(&output);
    }

    /// #541: a coarse job whose ceiling sits below every level the data
    /// reaches writes the same kind of archive, declaring `[plan min,
    /// pivot-1]` — the half a data shard does not own. The ceiling itself
    /// comes from the shard the facade was handed (#439), which is what
    /// makes this outcome reachable without the caller setting it.
    #[test]
    fn an_empty_coarse_job_writes_a_tile_less_archive() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        // Tiny polygons: auto-clamped (#211) out of every coarse level, so
        // a z0..z2 ceiling has nothing at or below it.
        let geoms: Vec<Option<Geometry<f64>>> = (0..20)
            .map(|k| {
                let (x, y) = (f64::from(k) * 0.01, 0.0);
                Some(Geometry::Polygon(Polygon::new(
                    LineString::from(vec![
                        (x, y),
                        (x + 0.0001, y),
                        (x + 0.0001, y + 0.0001),
                        (x, y + 0.0001),
                        (x, y),
                    ]),
                    vec![],
                )))
            })
            .collect();
        write_input(&input, &geoms, false, None);
        let output = dir.path().join("out.pmtiles");

        // Pivot 3 ⇒ the facade caps the convert at z2, which this fixture
        // reaches no level at or below. The coarse job needs no plan: it
        // reads the whole input and is the run that would write one.
        let options = TilesOptions {
            shard: Some(TilesShard {
                role: ShardRole::Coarse,
                range: None,
                pivot_zoom: 3,
                cut_digest: "0123456789abcdef".to_string(),
                binding: None,
            }),
            export: ExportOptions {
                metadata: grid_metadata(),
                ..ExportOptions::default()
            },
            ..zoomed_options(0, 14)
        };

        let outcome = convert_to_pmtiles(&TilesInput::Path(input), &output, &options)
            .expect("an empty coarse job must succeed");

        let TilesOutcome::EmptyCoarse(empty) = outcome else {
            panic!("expected an empty coarse job, got {outcome:?}");
        };
        assert_eq!(empty.role, ShardRole::Coarse);
        let json = serde_json::to_value(&empty).unwrap();
        assert_eq!(json["total_tiles"], 0);
        assert!(json["diagnostics"]
            .as_array()
            .is_some_and(|events| !events.is_empty()));
        assert_eq!(empty.pivot_zoom, 3);
        assert_eq!(empty.min_zoom, 0, "declares the zoom plan's minimum");
        assert_eq!(empty.max_zoom, 2, "declares up to just below the pivot");

        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[..8], b"PMTiles\x03", "a valid, readable archive");
        assert_grid_metadata(&output);
    }

    #[test]
    fn coarse_job_emptied_during_pass2_retains_diagnostics_and_cleans_intermediate() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        let square = |x: f64, y: f64| {
            geo::Rect::new(
                geo::Coord { x, y },
                geo::Coord {
                    x: x + 0.0001,
                    y: y + 0.0001,
                },
            )
            .to_polygon()
        };
        write_input(
            &input,
            &[
                Some(Geometry::MultiPolygon(geo::MultiPolygon(vec![
                    square(-80.0, 10.0),
                    square(80.0, 30.0),
                ]))),
                Some(Geometry::Point(Point::new(f64::NAN, 0.0))),
            ],
            false,
            None,
        );
        let output = dir.path().join("out.pmtiles");
        let outcome = convert_to_pmtiles(
            &TilesInput::Path(input),
            &output,
            &TilesOptions {
                shard: Some(TilesShard {
                    role: ShardRole::Coarse,
                    range: None,
                    pivot_zoom: 3,
                    cut_digest: "0123456789abcdef".into(),
                    binding: None,
                }),
                ..zoomed_options(0, 3)
            },
        )
        .unwrap();
        let TilesOutcome::EmptyCoarse(report) = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(report.total_tiles, 0);
        assert_eq!(report.input_rows, 2);
        assert!(report.diagnostics.iter().any(|event| matches!(
            event,
            crate::diagnostics::Diagnostic::NonFiniteGeometry { count: 1 }
        )));
        assert_eq!(report.source_losses.len(), 1);
        assert_eq!(report.source_losses.iter().next().unwrap().unwrap().id, 1);
        assert!(std::fs::read_dir(dir.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tylertoo-overview-")));
    }

    /// #439/#498: the facade applies the shard's convert-side consequences
    /// itself, so a caller that sets only `TilesOptions::shard` still gets
    #[test]
    fn a_coarse_shard_keeps_an_explicit_tile_range() {
        let base5 = crate::tile::hilbert_zoom_base(5);
        let explicit = TileRange::new(5, base5, base5 + 7).unwrap();

        // No shard: the caller's range passes through.
        assert_eq!(resolve_tile_range(None, Some(explicit)), Some(explicit));
        assert_eq!(resolve_tile_range(None, None), None);

        // A coarse shard has no range of its own, so the caller's applies
        // rather than being silently dropped.
        let coarse = TilesShard {
            role: ShardRole::Coarse,
            range: None,
            pivot_zoom: 5,
            cut_digest: "deadbeef".to_string(),
            binding: None,
        };
        assert_eq!(
            resolve_tile_range(Some(&coarse), Some(explicit)),
            Some(explicit),
            "a coarse shard must not discard the caller's tile_range"
        );
        assert_eq!(resolve_tile_range(Some(&coarse), None), None);

        // A data shard's own range prunes the convert too, so it wins.
        let shard_range = TileRange::new(5, base5 + 2, base5 + 3).unwrap();
        let data = TilesShard {
            role: ShardRole::Shard {
                index: 3,
                shards: 4,
            },
            range: Some(shard_range),
            pivot_zoom: 5,
            cut_digest: "deadbeef".to_string(),
            binding: None,
        };
        assert_eq!(
            resolve_tile_range(Some(&data), Some(explicit)),
            Some(shard_range),
            "a data shard's range takes precedence"
        );
    }

    /// the row pruning, the fingerprinted cut digest and the coarse job's
    /// ceiling — the three fields callers used to hand-sync.
    #[test]
    fn the_facade_applies_the_shards_convert_side_consequences() {
        // #771: a coarse job that draws the coverage fill keeps every level.
        let mut filled = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            ..ConvertOptions::default()
        };
        TilesShard {
            role: ShardRole::Coarse,
            range: None,
            pivot_zoom: 5,
            cut_digest: "deadbeef".to_string(),
            binding: None,
        }
        .apply_to_convert(&mut filled, true);
        assert_eq!(
            filled.zoom_ceiling, None,
            "#771: the fill needs the finest level"
        );
        assert_eq!(filled.shard_plan_digest.as_deref(), Some("deadbeef"));

        let mut convert = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            ..ConvertOptions::default()
        };
        assert!(convert.streaming, "the ceiling is streaming-only");

        let coarse = TilesShard {
            role: ShardRole::Coarse,
            range: None,
            pivot_zoom: 5,
            cut_digest: "deadbeef".to_string(),
            binding: None,
        };
        coarse.apply_to_convert(&mut convert, false);
        assert_eq!(convert.shard, None, "the coarse job prunes no rows");
        assert_eq!(convert.shard_plan_digest.as_deref(), Some("deadbeef"));
        assert_eq!(
            convert.zoom_ceiling,
            Some(4),
            "#541: the coarse job builds only up to just below the pivot"
        );

        // A data shard prunes rows and takes no ceiling.
        let base5 = crate::tile::hilbert_zoom_base(5);
        let range = TileRange::new(5, base5, base5 + 7).unwrap();
        let mut convert = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            ..ConvertOptions::default()
        };
        TilesShard {
            role: ShardRole::Shard {
                index: 3,
                shards: 4,
            },
            range: Some(range),
            pivot_zoom: 5,
            cut_digest: "deadbeef".to_string(),
            binding: None,
        }
        .apply_to_convert(&mut convert, false);
        assert_eq!(convert.shard, Some(range));
        assert_eq!(convert.zoom_ceiling, None, "only the coarse job is capped");

        // #560: `apply_selection` is the subset a plan-only run applies —
        // the selection without a ceiling it has no pass 2 for.
        let mut convert = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            ..ConvertOptions::default()
        };
        coarse.apply_selection(&mut convert);
        assert_eq!(convert.shard_plan_digest.as_deref(), Some("deadbeef"));
        assert_eq!(convert.zoom_ceiling, None);
    }

    /// The facade's happy path: a PMTiles archive, a combined report whose
    /// JSON is exactly `{"convert", "export"}`, the #380 declared minimum,
    /// and a temp intermediate that is gone afterwards.
    #[test]
    fn facade_converts_and_cleans_up_the_intermediate() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        points_input(&input);
        let output = dir.path().join("out.pmtiles");

        struct Saw(Option<IntermediateOverview>);
        impl TilesObserver for Saw {
            fn overview_written(&mut self, i: &IntermediateOverview, c: &ConvertReport) {
                assert!(c.input_features > 0);
                self.0 = Some(i.clone());
            }
        }
        let mut saw = Saw(None);

        let outcome = convert_to_pmtiles_observed(
            &TilesInput::Path(input),
            &output,
            &zoomed_options(0, 2),
            &mut saw,
        )
        .expect("facade must convert");

        let TilesOutcome::Converted(report) = outcome else {
            panic!("expected a full conversion, got {outcome:?}");
        };

        // The archive exists and is PMTiles v3.
        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[..8], b"PMTiles\x03");

        // #380: the requested minimum is declared.
        assert_eq!(report.export.min_zoom, 0);
        assert_eq!(report.export.max_zoom, 2);
        assert_eq!(report.convert.input_features, 3);

        // The observer heard about the intermediate before the export, and
        // the drop guard removed it afterwards.
        let seen = saw.0.expect("observer must fire");
        assert!(!seen.retained);
        assert!(seen.bytes > 0, "intermediate size is reported");
        assert_eq!(seen, report.intermediate);
        assert!(
            !report.intermediate.path.exists(),
            "temp intermediate must be removed: {}",
            report.intermediate.path.display()
        );

        // The combined report serializes to exactly the two halves.
        let json = serde_json::to_value(&*report).unwrap();
        let keys: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(keys, ["convert", "export"]);
    }

    /// #314: `keep_overview` retains the intermediate at the named path.
    #[test]
    fn keep_overview_retains_the_intermediate() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        points_input(&input);
        let output = dir.path().join("out.pmtiles");
        let kept = dir.path().join("kept-overview.parquet");

        let options = TilesOptions {
            keep_overview: Some(kept.clone()),
            ..zoomed_options(0, 2)
        };
        let outcome =
            convert_to_pmtiles(&TilesInput::Path(input), &output, &options).expect("must convert");
        let TilesOutcome::Converted(report) = outcome else {
            panic!("expected a full conversion");
        };
        assert!(report.intermediate.retained);
        assert_eq!(report.intermediate.path, kept);
        assert!(kept.exists(), "retained intermediate must survive");
    }

    /// #433: a `keep_overview` into a missing directory is refused before
    /// any work, with the CLI's exact wording.
    #[test]
    fn keep_overview_missing_directory_is_refused_up_front() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        points_input(&input);
        let output = dir.path().join("out.pmtiles");
        let missing = dir.path().join("no/such/dir/kept.parquet");

        let options = TilesOptions {
            keep_overview: Some(missing),
            ..zoomed_options(0, 2)
        };
        let err = convert_to_pmtiles(&TilesInput::Path(input), &output, &options).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--keep-overview directory") && msg.contains("does not exist"),
            "unexpected message: {msg}"
        );
        assert!(!output.exists(), "nothing may be written");
    }

    /// #433: a tile buffer past the cap fails before the convert reads a
    /// byte, as a typed config error (#779), not after the convert as an
    /// export failure.
    #[test]
    fn too_wide_tile_buffer_fails_before_converting() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberately NO input file: the preflight must fail first, so the
        // missing input is never reached.
        let input = dir.path().join("never-read.parquet");
        let output = dir.path().join("out.pmtiles");

        let options = TilesOptions {
            export: ExportOptions {
                tile_buffer: 100_000,
                ..ExportOptions::default()
            },
            ..zoomed_options(0, 2)
        };
        let err = convert_to_pmtiles(&TilesInput::Path(input), &output, &options).unwrap_err();
        assert!(err.to_string().contains("--tile-buffer 100000"), "{err}");
        assert!(
            matches!(
                err,
                TilesError::Config(ConfigError::TileBufferTooWide {
                    buffer: 100_000,
                    ..
                })
            ),
            "{err:?}"
        );
        assert!(err.is_config());
    }

    /// #386/#443: the knob-column guard fires before any work, with the
    /// CLI's exact wording — for `feature_order`, `feature_id`, and an
    /// accumulated `feature_id` column.
    #[test]
    fn knob_column_guard_matches_the_cli_wording() {
        use crate::overview::cluster::{AccumulateOp, AccumulateSpec};
        use crate::overview::properties::PropertySelection;

        let excluded = ConvertOptions {
            properties: PropertySelection {
                exclude: vec!["rank".to_string()],
                ..PropertySelection::default()
            },
            ..ConvertOptions::default()
        };
        let order = FeatureOrder::Column {
            name: "rank".to_string(),
            descending: false,
        };
        let err = check_knob_columns(&excluded, &order, None).unwrap_err();
        assert_eq!(
            err.to_string(),
            "property \"rank\" is excluded but --feature-order reads it; keep it in the \
             selection or drop the knob"
        );

        let err = check_knob_columns(&excluded, &FeatureOrder::Input, Some("rank")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "property \"rank\" is excluded but --feature-id reads it; keep it in the \
             selection or drop the flag"
        );

        let accumulated = ConvertOptions {
            accumulate: vec![AccumulateSpec {
                column: "pop".to_string(),
                op: AccumulateOp::Sum,
            }],
            ..ConvertOptions::default()
        };
        let err = check_knob_columns(&accumulated, &FeatureOrder::Input, Some("pop")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "--feature-id column \"pop\" is also --accumulate-attribute pop:sum; a \
             cluster's aggregated value identifies no single feature, so accumulate a \
             different column"
        );

        // Kept columns pass.
        check_knob_columns(&ConvertOptions::default(), &order, Some("rank")).unwrap();
    }

    /// The guard also runs inside the facade itself, so a binding that
    /// never calls `check_knob_columns` still gets it (#439 — the Python
    /// drift this module exists to end).
    #[test]
    fn facade_runs_the_knob_column_guard() {
        use crate::overview::properties::PropertySelection;

        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("never-read.parquet");
        let output = dir.path().join("out.pmtiles");

        let options = TilesOptions {
            convert: ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 2,
                },
                properties: PropertySelection {
                    exclude: vec!["name".to_string()],
                    ..PropertySelection::default()
                },
                ..ConvertOptions::default()
            },
            export: ExportOptions {
                feature_id: Some("name".to_string()),
                ..ExportOptions::default()
            },
            ..TilesOptions::default()
        };
        let err = convert_to_pmtiles(&TilesInput::Path(input), &output, &options).unwrap_err();
        assert!(
            matches!(
                err,
                TilesError::Config(ConfigError::FeatureIdColumnExcluded { .. })
            ),
            "{err:?}"
        );
    }

    /// A convert failure keeps the CLI's historical message shape, with no
    /// duplicated source chain (the message IS the chain).
    #[test]
    fn convert_failure_message_shape() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("missing.parquet");
        let output = dir.path().join("out.pmtiles");

        let err = convert_to_pmtiles(&TilesInput::Path(input), &output, &zoomed_options(0, 2))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("overview conversion failed: "), "{msg}");
        use std::error::Error as _;
        assert!(err.source().is_none(), "no duplicated Caused-by chain");
    }

    #[test]
    fn resolve_intermediate_dir_precedence() {
        use std::ffi::OsStr;
        let out = Path::new("/data/out/tiles.pmtiles");

        // Spill dir wins over everything.
        assert_eq!(
            resolve_intermediate_dir(Some(Path::new("/spill")), Some(OsStr::new("/tmpdir")), out),
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
            resolve_intermediate_dir(None, None, Path::new("tiles.pmtiles")),
            std::env::temp_dir()
        );
    }

    /// A shard job for the validation tests below.
    fn shard_job(range: Option<TileRange>, pivot_zoom: u8) -> TilesShard {
        TilesShard {
            role: if range.is_some() {
                ShardRole::Shard {
                    index: 0,
                    shards: 2,
                }
            } else {
                ShardRole::Coarse
            },
            range,
            pivot_zoom,
            cut_digest: "deadbeef".to_string(),
            binding: None,
        }
    }

    /// #779: every rule the CLI and the Python bindings used to hold their
    /// own copy of is `TilesOptions::validate`'s, typed, and runs before any
    /// input is read — the input below does not exist, and the facade
    /// refuses each configuration without reaching it.
    #[test]
    fn validate_owns_every_frontend_rule_before_any_read() {
        use crate::overview::cluster::{AccumulateOp, AccumulateSpec};
        use crate::overview::properties::PropertySelection;

        let dir = tempfile::tempdir().unwrap();
        let input = TilesInput::Path(dir.path().join("never-read.parquet"));
        let output = dir.path().join("out.pmtiles");
        let base = || zoomed_options(0, 8);
        let refused = |options: TilesOptions| -> TilesError {
            let early = options.validate().expect_err("validate must refuse");
            let late =
                convert_to_pmtiles(&input, &output, &options).expect_err("the facade must refuse");
            assert_eq!(early.to_string(), late.to_string());
            assert!(early.is_config(), "{early:?}");
            assert!(!output.exists());
            early
        };
        let base5 = crate::tile::hilbert_zoom_base(5);
        let range = TileRange::new(5, base5, base5 + 7).unwrap();

        type Case = (TilesOptions, fn(&TilesError) -> bool);
        let cases: Vec<Case> = vec![
            // #433 (1e37cbe): the export knobs that need no file, formerly
            // re-checked through a throwaway ExportOptions in the CLI.
            (
                TilesOptions {
                    export: ExportOptions {
                        extent: 0,
                        ..ExportOptions::default()
                    },
                    ..base()
                },
                |e| matches!(e, TilesError::Config(ConfigError::InvalidExtent { .. })),
            ),
            // #427 (492502f): the spill directory, formerly the CLI's
            // `check_spill_dir`.
            (
                TilesOptions {
                    convert: ConvertOptions {
                        spill_dir: Some(PathBuf::from("/nonexistent/tylertoo-779")),
                        ..base().convert
                    },
                    ..base()
                },
                |e| {
                    matches!(
                        e,
                        TilesError::Convert(ConvertError::Config(
                            ConfigError::SpillDirNotDirectory(_)
                        ))
                    )
                },
            ),
            // #428 (b8fa58a): a NaN class rank, formerly refused only by
            // the CLI's and Python's parsers.
            (
                TilesOptions {
                    convert: ConvertOptions {
                        class_ranking: Some(crate::overview::convert::ClassRanking {
                            column: "class".into(),
                            ranks: vec![("a".into(), f64::NAN)],
                            unknown_rank: 0.0,
                        }),
                        ..base().convert
                    },
                    ..base()
                },
                |e| {
                    matches!(
                        e,
                        TilesError::Convert(ConvertError::Config(
                            ConfigError::ClassRankNotFinite { .. }
                        ))
                    )
                },
            ),
            // Q1: --class-rank with --sort-key, formerly a CLI bail and a
            // Python check ("also enforced in core" — now only there).
            (
                TilesOptions {
                    convert: ConvertOptions {
                        sort_key: Some("pop".into()),
                        class_ranking: Some(
                            crate::overview::convert::parse_class_rank("class:a=1").unwrap(),
                        ),
                        ..base().convert
                    },
                    ..base()
                },
                |e| matches!(e, TilesError::Convert(ConvertError::RankingConflict)),
            ),
            // Q4: accumulate without cluster, formerly a CLI bail and a
            // Python check; now refused before the input is opened.
            (
                TilesOptions {
                    convert: ConvertOptions {
                        accumulate: vec![AccumulateSpec {
                            column: "pop".into(),
                            op: AccumulateOp::Sum,
                        }],
                        ..base().convert
                    },
                    ..base()
                },
                |e| {
                    matches!(
                        e,
                        TilesError::Convert(ConvertError::AccumulateWithoutCluster)
                    )
                },
            ),
            // #386/#443: the knob columns against the convert's selection.
            (
                TilesOptions {
                    convert: ConvertOptions {
                        properties: PropertySelection {
                            exclude: vec!["rank".into()],
                            ..PropertySelection::default()
                        },
                        ..base().convert
                    },
                    export: ExportOptions {
                        feature_order: FeatureOrder::Column {
                            name: "rank".into(),
                            descending: true,
                        },
                        ..ExportOptions::default()
                    },
                    ..base()
                },
                |e| {
                    matches!(
                        e,
                        TilesError::Config(ConfigError::FeatureOrderColumnExcluded { .. })
                    )
                },
            ),
            // #600: an export field the facade derives itself would be
            // silently overwritten; it is refused instead.
            (
                TilesOptions {
                    export: ExportOptions {
                        spill_dir: Some(dir.path().to_path_buf()),
                        ..ExportOptions::default()
                    },
                    ..base()
                },
                |e| {
                    matches!(
                        e,
                        TilesError::Config(ConfigError::OptionIgnored {
                            option: "export.spill_dir",
                            ..
                        })
                    )
                },
            ),
            // #498: a hand-written range beside a shard job, formerly a clap
            // `conflicts_with` only, silently resolved in core. A data shard
            // and the coarse job alike.
            (
                TilesOptions {
                    export: ExportOptions {
                        tile_range: Some(range),
                        ..ExportOptions::default()
                    },
                    shard: Some(shard_job(Some(range), 5)),
                    ..base()
                },
                |e| matches!(e, TilesError::Config(ConfigError::TileRangeWithShard)),
            ),
            (
                TilesOptions {
                    export: ExportOptions {
                        tile_range: Some(range),
                        ..ExportOptions::default()
                    },
                    shard: Some(shard_job(None, 5)),
                    ..base()
                },
                |e| matches!(e, TilesError::Config(ConfigError::TileRangeWithShard)),
            ),
            // #709: a data shard whose range disagrees with its pivot.
            (
                TilesOptions {
                    shard: Some(shard_job(Some(range), 4)),
                    ..base()
                },
                |e| {
                    matches!(
                        e,
                        TilesError::Config(ConfigError::ShardPivotMismatch {
                            pivot: 4,
                            range_pivot: 5
                        })
                    )
                },
            ),
            // #498/#709: a coarse job whose pivot sits at its minimum zoom
            // owns nothing, formerly the CLI's `resolve_shard_job` only.
            (
                TilesOptions {
                    shard: Some(shard_job(None, 0)),
                    ..base()
                },
                |e| {
                    matches!(
                        e,
                        TilesError::Config(ConfigError::CoarseShardHasNoZoom {
                            pivot: 0,
                            min_zoom: 0
                        })
                    )
                },
            ),
        ];
        for (options, expected) in cases {
            let err = refused(options);
            assert!(expected(&err), "unexpected error: {err:?}");
        }
    }

    /// #560/#600: a plan-only run validates the same configuration (the
    /// export options are ignored, not unchecked), refuses a data shard and a
    /// missing `--save-plan`, and lets a coarse pivot sit at the minimum
    /// zoom (the handover shape). It names what it ignores.
    #[test]
    fn write_plan_checks_what_it_ignores() {
        let dir = tempfile::tempdir().unwrap();
        let input = TilesInput::Path(dir.path().join("never-read.parquet"));
        let plan = dir.path().join("convert.plan");
        let planned = |options: TilesOptions| TilesOptions {
            convert: ConvertOptions {
                save_plan: Some(plan.clone()),
                ..options.convert
            },
            ..options
        };

        let err = write_plan(&input, &zoomed_options(0, 4)).unwrap_err();
        assert!(matches!(
            err,
            TilesError::Config(ConfigError::PlanOnlyWithoutSavePlan)
        ));

        let base5 = crate::tile::hilbert_zoom_base(5);
        let range = TileRange::new(5, base5, base5 + 7).unwrap();
        // As a frontend builds it: the selection applied to convert too, so
        // convert's own `shard` + `save_plan` refusal is also in play and
        // must not mask the precise plan-only one.
        let job = shard_job(Some(range), 5);
        let mut options = planned(TilesOptions {
            shard: Some(job.clone()),
            ..zoomed_options(0, 8)
        });
        job.apply_selection(&mut options.convert);
        let err = write_plan(&input, &options).unwrap_err();
        assert!(
            matches!(err, TilesError::Config(ConfigError::PlanOnlyDataShard)),
            "{err:?}"
        );

        let err = write_plan(
            &input,
            &planned(TilesOptions {
                export: ExportOptions {
                    tile_buffer: 999,
                    ..ExportOptions::default()
                },
                ..zoomed_options(0, 4)
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("--tile-buffer 999"), "{err}");

        // The handover shape: pivot at the minimum. Validation passes; the
        // missing input is what fails, so validation did not refuse it.
        let err = write_plan(
            &input,
            &planned(TilesOptions {
                shard: Some(shard_job(None, 0)),
                ..zoomed_options(0, 4)
            }),
        )
        .unwrap_err();
        assert!(matches!(err, TilesError::Convert(_)), "{err:?}");

        let ignored = TilesOptions {
            layer_name: Some("x".into()),
            export: ExportOptions {
                tile_buffer: 16,
                feature_id: Some("id".into()),
                ..ExportOptions::default()
            },
            ..TilesOptions::default()
        };
        assert_eq!(
            ignored.ignored_by_plan_only(&ExportOptions::default()),
            ["--layer-name", "--tile-buffer", "--feature-id"]
        );
        // A value equal to the run's own default is no value at all.
        let verbatim = TilesOptions {
            export: TilesOptions::export_defaults(true),
            ..TilesOptions::default()
        };
        assert!(verbatim
            .ignored_by_plan_only(&TilesOptions::export_defaults(true))
            .is_empty());
    }

    /// #761: a verbatim `tiles` run has no size cap unless one is asked for;
    /// the default run has both.
    #[test]
    fn export_defaults_follow_verbatim() {
        let capped = TilesOptions::export_defaults(false);
        assert!(capped.tile_size_limit.is_some() && capped.compressed_tile_size_limit.is_some());
        let verbatim = TilesOptions::export_defaults(true);
        assert_eq!(verbatim.tile_size_limit, None);
        assert_eq!(verbatim.compressed_tile_size_limit, None);
        assert_eq!(
            TilesOptions::default().convert.levels,
            LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: TILES_DEFAULT_MAX_ZOOM
            }
        );
    }

    /// The derived layer name follows core's input rules for both shapes.
    #[test]
    fn derive_layer_name_follows_input_rules() {
        assert_eq!(
            TilesInput::Path(PathBuf::from("/data/buildings.parquet")).derive_layer_name(),
            "buildings"
        );
        assert_eq!(
            TilesInput::Manifest(PathBuf::from("/data/parts.txt")).derive_layer_name(),
            "parts"
        );
    }
}
