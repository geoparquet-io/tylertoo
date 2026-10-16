//! The one-shot GeoParquet → PMTiles facade (#439).
//!
//! This module owns the *product policy* of the flagship path — everything
//! `tylertoo tiles` does between "the options are built" and "the archive is
//! on disk" — so the CLI and the Python bindings are thin callers that can
//! never drift:
//!
//! - the intermediate overview's lifecycle (#314): retained at
//!   [`TilesOptions::keep_overview`], otherwise a temp file in the spill
//!   dir / `$TMPDIR` / the output directory (in that order), removed on
//!   drop — success or failure alike;
//! - the free-space preflight on the volume holding that intermediate (#314);
//! - the export-knob preflight that needs no file (#433) and the
//!   feature-order / feature-id vs property-selection guard (#386/#443);
//! - the declared-minimum-zoom rule (#380) and a data shard's narrower
//!   declaration (#498);
//! - the empty-shard (#498) and empty-coarse (#541) archive handling;
//! - the combined convert + export report the one-step run captures.
//!
//! The chain is `overview` convert → `export-pmtiles` through a materialized
//! intermediate overview file — this is NOT zero-disk.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::convert::{
    convert_to_overviews, convert_to_overviews_sources, ConvertError, ConvertOptions,
    ConvertReport, LevelPlan,
};
use super::export::{
    export_pmtiles, write_empty_archive, ExportError, ExportOptions, ExportReport, FeatureOrder,
};
use super::writer::WriterError;
use crate::input_set::ConvertSource;
use crate::shard::{ShardRole, TileRange};

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
        match self {
            TilesInput::Path(p) => convert_to_overviews(p, output, options),
            TilesInput::Manifest(m) => ConvertSource::from_manifest(m)
                .map_err(ConvertError::from)
                .and_then(|source| convert_to_overviews_sources(&source, output, options)),
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
}

impl TilesShard {
    /// What *every* job of a fleet applies to its convert, whether or not it
    /// goes on to build tiles (#498): the data shard's row-pruning range and
    /// the fleet's cut digest. The digest is fingerprinted and the range is
    /// not — the cut is fleet-wide, this job's slice of it is not.
    ///
    /// Separate from [`TilesShard::apply_to_convert`] because a `--plan-only`
    /// run (#560) applies exactly this much: it materializes no level, so it
    /// must not carry a ceiling core would refuse for having no pass 2 to
    /// apply it to.
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
    /// [`convert_to_pmtiles`] applies this to its own copy of
    /// [`TilesOptions::convert`], so a caller never hand-syncs the two.
    /// Idempotent, so a caller that has already applied it (the CLI, which
    /// shares one options builder with its `--plan-only` path) is unaffected.
    pub fn apply_to_convert(&self, convert: &mut ConvertOptions) {
        self.apply_selection(convert);
        if self.range.is_none()
            && matches!(convert.levels, LevelPlan::ZoomRange { .. })
            && convert.streaming
        {
            convert.zoom_ceiling = Some(self.pivot_zoom.saturating_sub(1));
        }
    }
}

/// Options for [`convert_to_pmtiles`]: the convert half's options plus the
/// export knobs and the facade's own policy inputs.
///
/// The declared zoom range (#380) is read from `convert.levels`: a
/// [`LevelPlan::ZoomRange`]'s requested minimum is declared even when the
/// coarsest levels generalized to nothing, and its (resolved — `--max-zoom
/// auto` is the caller's to resolve first, via
/// [`super::auto_zoom::MaxZoom::resolve`], which writes the chosen zoom back
/// into the plan) maximum declares an empty data shard's archive. A GSD
/// ladder declares no requested range.
///
/// Note: #436 will freeze this shape (`#[non_exhaustive]` + a builder);
/// construct it with `..TilesOptions::default()` so that lands as a
/// non-event.
#[derive(Debug, Clone)]
pub struct TilesOptions {
    /// The convert half ([`super::convert::convert_to_overviews`]). The
    /// facade reads `spill_dir` from here for the intermediate's location
    /// and the export's member spill (#427 — one scratch knob), `properties`
    /// for the knob-column guard, and `levels` for the declared zoom range.
    /// For a sharded run the facade itself applies
    /// [`TilesOptions::shard`]'s convert-side consequences (`shard`,
    /// `shard_plan_digest`, the coarse `zoom_ceiling`) to its own copy, so
    /// leave those fields alone.
    pub convert: ConvertOptions,
    /// MVT layer name; `None` derives it from the input
    /// ([`TilesInput::derive_layer_name`]).
    pub layer_name: Option<String>,
    /// Retain the intermediate overview at this path instead of a removed
    /// temp file (#314). Its parent directory must exist.
    pub keep_overview: Option<PathBuf>,
    /// The sharded-fleet job, if any (#498).
    pub shard: Option<TilesShard>,
    /// A hand-written export tile range, restricting the export only (a
    /// data shard's range prunes the convert's reads too).
    ///
    /// A [`TilesShard::range`] takes precedence where it is set, which is
    /// data shards only; a coarse shard has none, so this applies there.
    /// The CLI and the Python bindings keep the two mutually exclusive, so
    /// in practice at most one is ever set.
    pub tile_range: Option<TileRange>,
    /// Per-tile edge buffer in tile pixels ([`ExportOptions::tile_buffer`]).
    pub tile_buffer: u32,
    /// Per-tile MVT size limit in bytes ([`ExportOptions::tile_size_limit`]).
    pub tile_size_limit: Option<usize>,
    /// See [`ExportOptions::simple_clip_fastpath`].
    pub simple_clip_fastpath: bool,
    /// See [`ExportOptions::partition_wave`].
    pub partition_wave: usize,
    /// Within-tile feature order (#361).
    pub feature_order: FeatureOrder,
    /// Promote this property to the MVT feature id (#443).
    pub feature_id: Option<String>,
}

impl Default for TilesOptions {
    fn default() -> Self {
        let export = ExportOptions::default();
        Self {
            convert: ConvertOptions::default(),
            layer_name: None,
            keep_overview: None,
            shard: None,
            tile_range: None,
            tile_buffer: export.tile_buffer,
            tile_size_limit: export.tile_size_limit,
            simple_clip_fastpath: export.simple_clip_fastpath,
            partition_wave: export.partition_wave,
            feature_order: FeatureOrder::default(),
            feature_id: None,
        }
    }
}

/// The export tile range for a run: a shard's own range where it has one,
/// otherwise the caller's hand-written [`TilesOptions::tile_range`].
///
/// A data shard's range prunes the convert's reads as well as the export, so
/// it wins. A *coarse* shard has `range: None`, so matching on the shard
/// alone discarded an explicitly-set `tile_range` for every coarse job —
/// the "accepts a parameter and ignores it" shape this facade exists to
/// remove. Unreachable from the CLI (clap `conflicts_with`) and from Python
/// (both always `None`), but `TilesOptions` is public API.
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
    /// intermediate looked too small for it. Already logged (`log::warn`);
    /// carried here so a binding without a log bridge can surface it (the
    /// Python `convert()` raises it as a `UserWarning`). `None` when the
    /// volume fits or either side of the comparison is unknowable.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyArchive {
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

/// Errors from [`convert_to_pmtiles`].
///
/// `#[non_exhaustive]`: new failure modes are added without a breaking
/// change; match with a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TilesError {
    /// The convert half failed. Not a `#[source]` chain: the message *is*
    /// the convert error's, prefixed, the way the CLI has always printed it.
    #[error("overview conversion failed: {0}")]
    Convert(ConvertError),
    /// The export half (or its #433 preflight) failed.
    #[error("export failed: {0}")]
    Export(ExportError),
    /// [`TilesOptions::keep_overview`] names a directory that does not
    /// exist (#433 — checked before any work).
    #[error("--keep-overview directory {} does not exist", dir.display())]
    KeepOverviewDirMissing {
        /// The missing parent directory.
        dir: PathBuf,
    },
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
    /// [`TilesOptions::feature_order`] reads a property the convert's
    /// selection excludes (#386): the column would be gone from the
    /// intermediate before export ever saw it, so this is refused before
    /// any work rather than quietly falling back after the whole convert.
    #[error(
        "property {name:?} is excluded but --feature-order reads it; keep it in the \
         selection or drop the knob"
    )]
    FeatureOrderColumnExcluded {
        /// The excluded property.
        name: String,
    },
    /// [`TilesOptions::feature_id`] reads a property the convert's
    /// selection excludes (#443). Same timing as
    /// [`TilesError::FeatureOrderColumnExcluded`].
    #[error(
        "property {name:?} is excluded but --feature-id reads it; keep it in the \
         selection or drop the flag"
    )]
    FeatureIdColumnExcluded {
        /// The excluded property.
        name: String,
    },
    /// [`TilesOptions::feature_id`] names an accumulated column: a
    /// clustered point's accumulated value is a sum/min/max/mean of several
    /// ids — the id of no feature (#443). Export rejects it from the file's
    /// clustering provenance too; checking here saves the convert.
    #[error(
        "--feature-id column {name:?} is also --accumulate-attribute {name}:{op}; a \
         cluster's aggregated value identifies no single feature, so accumulate a \
         different column"
    )]
    FeatureIdAccumulated {
        /// The offending column.
        name: String,
        /// The accumulate op it carries.
        op: String,
    },
}

/// The `keep_overview` directory preflight (#433): a retained intermediate
/// into a missing directory is refused before any work, not after a
/// minutes-long convert.
///
/// Shared by [`convert_to_pmtiles`] and the CLI's own early preflight (which
/// runs it before the output path is even resolved), so the wording exists
/// once.
///
/// # Errors
///
/// Returns [`TilesError::KeepOverviewDirMissing`] when `keep_overview` names
/// a path whose parent directory does not exist.
pub fn check_keep_overview(keep_overview: Option<&Path>) -> Result<(), TilesError> {
    if let Some(path) = keep_overview {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            if !parent.is_dir() {
                return Err(TilesError::KeepOverviewDirMissing {
                    dir: parent.to_path_buf(),
                });
            }
        }
    }
    Ok(())
}

/// The feature-order / feature-id vs property-selection guard (#386/#443).
///
/// Shared by [`convert_to_pmtiles`] and the CLI's `--plan-only` preflight
/// (#600: plan-only exports nothing, but it is the fleet's preflight, and
/// the coarse job and every data shard refuse these pairings).
///
/// # Errors
///
/// Returns [`TilesError::FeatureOrderColumnExcluded`],
/// [`TilesError::FeatureIdColumnExcluded`] or
/// [`TilesError::FeatureIdAccumulated`] on the refused pairings.
pub fn check_knob_columns(
    options: &ConvertOptions,
    feature_order: &FeatureOrder,
    feature_id: Option<&str>,
) -> Result<(), TilesError> {
    if let FeatureOrder::Column { name, .. } = feature_order {
        if !options.properties.keeps(name) {
            return Err(TilesError::FeatureOrderColumnExcluded { name: name.clone() });
        }
    }
    // #443: convert-time-only. This does not apply to export-pmtiles's OWN
    // property selection, which `ExportOptions::feature_id` always overrides
    // regardless (the column already exists in the intermediate either way).
    if let Some(name) = feature_id {
        if !options.properties.keeps(name) {
            return Err(TilesError::FeatureIdColumnExcluded {
                name: name.to_string(),
            });
        }
        if let Some(spec) = options.accumulate.iter().find(|a| a.column == *name) {
            return Err(TilesError::FeatureIdAccumulated {
                name: name.to_string(),
                op: spec.op.as_str().to_string(),
            });
        }
    }
    Ok(())
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
    // #433: the export knobs that need no file are checked before the
    // convert, which on a large input runs for minutes before the export
    // would otherwise refuse them. The full `ExportOptions` are built after
    // the convert; this is the same check on the values already known.
    ExportOptions {
        tile_buffer: options.tile_buffer,
        ..ExportOptions::default()
    }
    .validate()
    .map_err(TilesError::Export)?;
    check_keep_overview(options.keep_overview.as_deref())?;
    // #386/#443: the property selection is applied at convert, so an
    // excluded column is already gone from the intermediate before export
    // would sort or id by it — refused before any work is done.
    check_knob_columns(
        &options.convert,
        &options.feature_order,
        options.feature_id.as_deref(),
    )?;

    // #498/#541: the convert-side consequences of a sharded job are the
    // facade's, not the caller's. Applied to our own copy, so a Rust caller
    // that sets only `TilesOptions::shard` still gets the data shard's row
    // pruning, the fleet digest the merge binding needs, and the coarse
    // job's ceiling — and the CLI, which applies the same rules to build its
    // `--plan-only` options, is unaffected (the application is idempotent).
    let convert_options = match &options.shard {
        Some(job) => {
            let mut convert = options.convert.clone();
            job.apply_to_convert(&mut convert);
            Cow::Owned(convert)
        }
        None => Cow::Borrowed(&options.convert),
    };

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
    // a hand-written range restricts the export only (callers keep the two
    // flags mutually exclusive, so at most one is set).
    let tile_range = resolve_tile_range(options.shard.as_ref(), options.tile_range);

    // The data shard's own range, kept past `tile_range`'s move into
    // `ExportOptions`: the empty-shard branch below needs the pivot zoom.
    let shard_range = options.shard.as_ref().and_then(|job| job.range);

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
                let dir = resolve_intermediate_dir(
                    options.convert.spill_dir.as_deref(),
                    std::env::var_os("TMPDIR").as_deref(),
                    output,
                );
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

    // #314 free-space preflight: the intermediate is at least input-sized,
    // so warn up front when the chosen volume looks too small. The message
    // is also carried on the report's `IntermediateOverview` for callers
    // without a log bridge.
    let overview_dir = overview_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let space_warning = warn_intermediate_space(input, &overview_dir);

    let convert_report = match input.convert(&overview_path, &convert_options) {
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
                    write_empty_archive(output, &layer_name, range.pivot_zoom(), max_zoom)
                        .map_err(TilesError::EmptyShardArchive)?;
                    log::info!(
                        "[tiles] shard {} owns no input rows ({e}); wrote an empty archive",
                        job.role
                    );
                    return Ok(TilesOutcome::EmptyShard(EmptyArchive {
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
                    write_empty_archive(output, &layer_name, min_zoom, max_zoom)
                        .map_err(TilesError::EmptyCoarseArchive)?;
                    log::info!(
                        "[tiles] shard job {} has nothing to write ({e}); wrote an empty archive",
                        job.role
                    );
                    return Ok(TilesOutcome::EmptyCoarse(EmptyArchive {
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
        tile_buffer: options.tile_buffer,
        extent: ExportOptions::default().extent,
        tile_size_limit: options.tile_size_limit,
        simple_clip_fastpath: options.simple_clip_fastpath,
        partition_wave: options.partition_wave,
        feature_order: options.feature_order.clone(),
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
        feature_id: options.feature_id.clone(),
        // #427: one scratch knob — the export's member spill goes where the
        // convert's spill (and the intermediate overview) go.
        spill_dir: options.convert.spill_dir.clone(),
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

/// Lower-bound estimate of the intermediate overview's size for the #314
/// free-space preflight: the local input bytes. A duplicating overview
/// embeds the finest level verbatim on top of the coarse levels, so it is
/// at least input-sized. Local file → its size; local directory → the sum
/// of its `.parquet` files, recursively, through the same listing the
/// convert itself reads ([`crate::input_set::list_parquet_files`] — a
/// hive-partitioned set is estimated by what will actually be read, not by
/// its empty top level); manifest → the sum of the entries that resolve as
/// local files. Remote/glob shapes return `None` (the preflight stays quiet
/// rather than guessing).
fn estimate_local_input_bytes(input: &TilesInput) -> Option<u64> {
    fn file_len(p: &Path) -> Option<u64> {
        std::fs::metadata(p)
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len())
    }
    match input {
        TilesInput::Path(p) => {
            let meta = std::fs::metadata(p).ok()?;
            if meta.is_file() {
                return Some(meta.len());
            }
            if !meta.is_dir() {
                return None;
            }
            let sum: u64 = crate::input_set::list_parquet_files(p)
                .ok()?
                .iter()
                .filter_map(|f| file_len(f))
                .fold(0u64, u64::saturating_add);
            (sum > 0).then_some(sum)
        }
        TilesInput::Manifest(m) => {
            let text = std::fs::read_to_string(m).ok()?;
            let sum: u64 = crate::input_set::manifest_entries(&text)
                .iter()
                .filter_map(|(_, entry)| file_len(Path::new(entry)))
                .fold(0u64, u64::saturating_add);
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
    dir: &Path,
) -> Option<String> {
    if estimated_bytes == 0 {
        return None;
    }
    // Saturating, like the #272 spill preflight's math (#686): an absurd
    // estimate must degrade to "warn", never to an overflow panic.
    let need = estimated_bytes.saturating_add(estimated_bytes / INTERMEDIATE_MARGIN_DENOM);
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
        gib(need.saturating_sub(available_bytes)),
    ))
}

/// Emit the #314 intermediate free-space preflight warning, if warranted,
/// and return it for the report: probe the volume holding `dir`
/// ([`crate::overview::convert`]'s shared `fs4` probe, the one the #272
/// remote-spill preflight uses) and compare against the local-input-bytes
/// estimate. Quiet (`None`) when either side is unknowable (remote input,
/// exotic filesystem) rather than crying wolf.
fn warn_intermediate_space(input: &TilesInput, dir: &Path) -> Option<String> {
    let estimated = estimate_local_input_bytes(input)?;
    let available = super::convert::probe_available_space(dir)?;
    let msg = intermediate_space_warning(estimated, available, dir)?;
    log::warn!("{msg}");
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::testutil::write_input;
    use geo::{Geometry, LineString, Point, Polygon};

    fn points_input(path: &Path) {
        let geoms: Vec<Option<Geometry<f64>>> = vec![
            Some(Geometry::Point(Point::new(0.5, 0.5))),
            Some(Geometry::Point(Point::new(10.0, 20.0))),
            Some(Geometry::Point(Point::new(-30.0, -10.0))),
        ];
        write_input(path, &geoms, false, None);
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
            }),
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

        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[..8], b"PMTiles\x03", "a valid, readable archive");
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
            }),
            ..zoomed_options(0, 14)
        };

        let outcome = convert_to_pmtiles(&TilesInput::Path(input), &output, &options)
            .expect("an empty coarse job must succeed");

        let TilesOutcome::EmptyCoarse(empty) = outcome else {
            panic!("expected an empty coarse job, got {outcome:?}");
        };
        assert_eq!(empty.role, ShardRole::Coarse);
        assert_eq!(empty.pivot_zoom, 3);
        assert_eq!(empty.min_zoom, 0, "declares the zoom plan's minimum");
        assert_eq!(empty.max_zoom, 2, "declares up to just below the pivot");

        let bytes = std::fs::read(&output).unwrap();
        assert_eq!(&bytes[..8], b"PMTiles\x03", "a valid, readable archive");
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
        };
        coarse.apply_to_convert(&mut convert);
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
        }
        .apply_to_convert(&mut convert);
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
    /// byte, as `export failed: ...`.
    #[test]
    fn too_wide_tile_buffer_fails_before_converting() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberately NO input file: the preflight must fail first, so the
        // missing input is never reached.
        let input = dir.path().join("never-read.parquet");
        let output = dir.path().join("out.pmtiles");

        let options = TilesOptions {
            tile_buffer: 100_000,
            ..zoomed_options(0, 2)
        };
        let err = convert_to_pmtiles(&TilesInput::Path(input), &output, &options).unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("export failed: "), "{msg}");
        assert!(matches!(err, TilesError::Export(_)), "{err:?}");
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
            feature_id: Some("name".to_string()),
            ..TilesOptions::default()
        };
        let err = convert_to_pmtiles(&TilesInput::Path(input), &output, &options).unwrap_err();
        assert!(
            matches!(err, TilesError::FeatureIdColumnExcluded { .. }),
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

    /// #314 free-space preflight: quiet when the estimated intermediate
    /// (plus the 5% margin) fits, a warning naming the directory, the
    /// shortfall, and the `--spill-dir` escape hatch when it does not.
    #[test]
    fn intermediate_space_warning_thresholds() {
        let dir = Path::new("/some/volume");
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
    /// the sum of existing local manifest entries for a manifest.
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
            estimate_local_input_bytes(&TilesInput::Path(f1.clone())),
            Some(100)
        );
        // Directory: only .parquet entries count.
        assert_eq!(
            estimate_local_input_bytes(&TilesInput::Path(dir.path().to_path_buf())),
            Some(150)
        );
        // Nonexistent / remote-looking path: quiet None.
        assert_eq!(
            estimate_local_input_bytes(&TilesInput::Path(PathBuf::from("s3://bucket/x.parquet"))),
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
            estimate_local_input_bytes(&TilesInput::Manifest(manifest)),
            Some(150)
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
