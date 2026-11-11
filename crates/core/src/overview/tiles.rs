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

use super::cells::CellBand;
use super::config::{check_ignored_export_fields, ConfigError, ExportField};
use super::convert::{
    convert_to_overviews, convert_to_overviews_sources, write_convert_plan_sources, ConvertError,
    ConvertOptions, ConvertReport, LevelPlan, PlanReport,
};
use super::export::{
    export_pmtiles, write_empty_archive, ExportError, ExportOptions, ExportReport, FeatureOrder,
};
use super::writer::WriterError;
use crate::error::Classify;
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
#[non_exhaustive]
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
    #[doc(hidden)]
    pub fn derive_layer_name(&self) -> String {
        let p = match self {
            TilesInput::Path(p) | TilesInput::Manifest(p) => p,
        };
        crate::input_set::derive_layer_name(&p.to_string_lossy())
    }

    #[doc(hidden)]
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

    #[doc(hidden)]
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
#[non_exhaustive]
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
#[non_exhaustive]
pub struct ShardBinding {
    /// The plan ([`crate::shard::ShardPlan::load`]).
    pub plan: crate::shard::ShardPlan,
    /// Where it was read from, for the error messages.
    pub path: PathBuf,
}

impl ShardBinding {
    /// `plan`, read from `path`.
    pub fn new(plan: crate::shard::ShardPlan, path: impl Into<PathBuf>) -> Self {
        Self {
            plan,
            path: path.into(),
        }
    }
}

impl TilesShard {
    /// A job of a sharded fleet; see the fields.
    pub fn new(
        role: ShardRole,
        range: Option<TileRange>,
        pivot_zoom: u8,
        cut_digest: impl Into<String>,
        binding: Option<ShardBinding>,
    ) -> Self {
        Self {
            role,
            range,
            pivot_zoom,
            cut_digest: cut_digest.into(),
            binding,
        }
    }

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
    #[doc(hidden)]
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
    /// zoom-plan run, the build ceiling just below the pivot (#541: pass 1
    /// and the level assignment stay full-range, so any saved plan is
    /// byte-identical to a full run's).
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
#[non_exhaustive]
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
    /// Route each feature to the MVT layer named by its value in this
    /// column (#821, tippecanoe's per-feature `tippecanoe.layer`): one
    /// layer per distinct value, `_unknown` for a null or blank one. The
    /// column decides the layer and is not written as a property. Excludes
    /// [`TilesOptions::layer_name`]; see [`super::layer_column`].
    #[cfg_attr(
        feature = "cli",
        arg(long, value_name = "COLUMN", conflicts_with = "layer_name")
    )]
    #[cfg_attr(feature = "cli", arg(help = "Put each feature in the layer named by its value in COLUMN, and null or blank values in `_unknown`. The column is not written as a property. Each layer is tiled on its own, so the size caps apply per layer", long_help = None))]
    pub layer_column: Option<String>,
    /// Retain the intermediate overview at this path instead of a removed
    /// temp file (#314). Its parent directory must exist.
    #[cfg_attr(feature = "cli", arg(long, value_name = "PATH"))]
    #[cfg_attr(feature = "cli", arg(help = "Save the intermediate overview GeoParquet to PATH after export. Does not change the PMTiles output", long_help = None))]
    pub keep_overview: Option<PathBuf>,
    /// The sharded-fleet job, if any (#498).
    #[serde(skip)]
    #[cfg_attr(feature = "cli", arg(skip = Self::default().shard))]
    pub shard: Option<TilesShard>,
    /// Zoom ranges drawn as cells instead of features (#332), in the same
    /// layer: each zoom holds one polygon per occupied cell, with
    /// `point_count` and the `convert.accumulate` and
    /// `convert.cluster_categories` rollups of the features counted in it.
    /// The cells count every row the filter, bbox and mask keep. A band
    /// may not overlap another or `convert.levels`; a zoom no band covers
    /// is left empty, with a warning. Without `convert.cluster` the rollups
    /// apply to the cells alone. Default: none.
    #[serde(skip)]
    #[cfg_attr(feature = "cli", arg(skip = Self::default().cell_bands))]
    pub cell_bands: Vec<CellBand>,
}

with_setters!(TilesOptions {
    with_convert => convert: ConvertOptions,
    with_export => export: ExportOptions,
    with_layer_name => layer_name: Option<String>,
    with_layer_column => layer_column: Option<String>,
    with_keep_overview => keep_overview: Option<PathBuf>,
    with_shard => shard: Option<TilesShard>,
    with_cell_bands => cell_bands: Vec<CellBand>,
});

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
            layer_column: None,
            keep_overview: None,
            shard: None,
            cell_bands: Vec::new(),
        }
    }
}

/// Export fields the facade derives itself, for
/// [`check_ignored_export_fields`], with what to set instead. Kept beside [`TilesOptions`]'s docs, which list the same five.
const FACADE_OWNED_EXPORT_FIELDS: [(ExportField, &str); 6] = [
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
    (
        ExportField::Profile,
        "the one memory profile is convert.profile",
    ),
];

impl TilesOptions {
    /// The export defaults of a `tiles` run (#761): [`ExportOptions::default`],
    /// or with `verbatim` [`ExportOptions::verbatim`] — no size cap, since a
    /// valve that sheds features to fit a byte budget is not verbatim
    /// either. An explicit cap set on the result still applies. The CLI's
    /// `tiles --verbatim` and the Python `convert(verbatim=True)` both start
    /// here.
    #[doc(hidden)]
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
    /// The first rule broken; each is [`ErrorKind::Usage`](crate::error::ErrorKind::Usage).
    /// Validate these options and return the immutable engine configuration.
    #[doc(hidden)]
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
    #[doc(hidden)]
    pub fn validate_plan_only(&self) -> Result<(), TilesError> {
        self.validate_for(Run::PlanOnly)
    }

    #[doc(hidden)]
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
            ("--tile-format", e.tile_format != defaults.tile_format),
            ("--mlt-profile", e.mlt_profile != defaults.mlt_profile),
            ("--coverage-fill", e.coverage_fill != defaults.coverage_fill),
            (
                "--generate-variable-depth-tile-pyramid",
                e.variable_depth != defaults.variable_depth,
            ),
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
        if !self.cell_bands.is_empty() {
            self.check_cell_bands()?;
            // The feature zooms are an ordinary run's, checked as one.
            return self.features().validate_for(run);
        }
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
        // Under the layer name the export will get, so its rules (an MLT
        // layer needs one) hold before the convert runs.
        match &self.layer_name {
            Some(name) => ExportOptions {
                layer_name: name.clone(),
                ..self.export.clone()
            }
            .validate()
            .map(|_| ())?,
            None => self.export.validate().map(|_| ())?,
        }
        if self.export.variable_depth {
            super::export::variable_depth::check_convert(&self.convert, self.shard.is_some())?;
        }
        check_ignored_export_fields(&self.export, "tiles", &FACADE_OWNED_EXPORT_FIELDS)?;
        if let Some(column) = &self.layer_column {
            self.check_layer_column(column)?;
        }
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

impl TilesOptions {
    /// The run of the feature zooms beside [`TilesOptions::cell_bands`]
    /// (#332): no cell bands; the rollups only when `convert.cluster`
    /// uses them too; and the cell bands in the convert's plan fingerprint,
    /// so a fleet job given other cell bands is refused. These options
    /// themselves without cell bands.
    pub(super) fn features(&self) -> Cow<'_, TilesOptions> {
        if self.cell_bands.is_empty() {
            return Cow::Borrowed(self);
        }
        let mut features = self.clone();
        features.cell_bands = Vec::new();
        let convert = &mut features.convert;
        if !convert.cluster {
            convert.accumulate = Vec::new();
            convert.cluster_categories = None;
        }
        convert.cell_bands = Some(
            self.cell_bands
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(","),
        );
        Cow::Owned(features)
    }

    /// The rules [`TilesOptions::cell_bands`] add (#332): what a cell band
    /// cannot be combined with, and no two bands sharing a zoom.
    fn check_cell_bands(&self) -> Result<(), ConfigError> {
        let band = self.cell_bands[0].to_string();
        let conflict = |option, reason| ConfigError::CellBandConflict {
            band: band.clone(),
            option,
            reason,
        };
        if self.layer_column.is_some() {
            return Err(conflict(
                "--layer-column",
                "the cells and the features share one layer",
            ));
        }
        if self.export.variable_depth {
            return Err(conflict(
                "--generate-variable-depth-tile-pyramid",
                "the cell zooms are joined to the feature zooms by a merge that needs every \
                 tile at every zoom",
            ));
        }
        if self.export.tile_range.is_some() {
            return Err(conflict(
                "--tile-range",
                "a tile range splits the zooms by tile, and a cell can straddle two tiles",
            ));
        }
        if self.convert.auto_max_zoom {
            return Err(conflict("--max-zoom auto", "the bands set the zoom range"));
        }
        let LevelPlan::ZoomRange { min_zoom, max_zoom } = self.convert.levels else {
            return Err(conflict("--gsd", "the bands set the zoom range"));
        };
        let mut bands: Vec<(u8, u8, String)> = self
            .cell_bands
            .iter()
            .map(|b| (b.min_zoom, b.max_zoom, b.to_string()))
            .chain([(
                min_zoom,
                max_zoom,
                format!("{min_zoom}-{max_zoom} (the feature zooms)"),
            )])
            .collect();
        bands.sort();
        for (i, (lo, hi, label)) in bands.iter().enumerate() {
            if lo > hi || *hi > crate::tile::MAX_ZOOM {
                return Err(ConfigError::InvalidSpec {
                    option: "--band",
                    message: format!(
                        "--band: {label} must run from a coarser to a finer zoom, at most z{}",
                        crate::tile::MAX_ZOOM
                    ),
                });
            }
            if let Some((next, _, second)) = bands[i + 1..].iter().find(|b| b.0 <= *hi) {
                return Err(ConfigError::BandsOverlap {
                    first: label.clone(),
                    second: second.clone(),
                    zoom: *next,
                });
            }
        }
        if let Some(job) = &self.shard {
            if let Some(b) = self
                .cell_bands
                .iter()
                .find(|b| b.max_zoom >= job.pivot_zoom)
            {
                return Err(ConfigError::CellBandPastPivot {
                    band: b.to_string(),
                    max_zoom: b.max_zoom,
                    pivot: job.pivot_zoom,
                });
            }
        }
        Ok(())
    }

    /// The options a [`TilesOptions::layer_column`] run (#821) cannot
    /// honour: each layer is its own convert and export.
    fn check_layer_column(&self, column: &str) -> Result<(), ConfigError> {
        let conflict = |option, reason| ConfigError::LayerColumnConflict { option, reason };
        if self.layer_name.is_some() {
            return Err(conflict(
                "--layer-name",
                "the column's values name the layers",
            ));
        }
        if self.shard.is_some() {
            return Err(conflict(
                "--shard",
                "a sharded build shares one convert plan, and each layer needs its own",
            ));
        }
        if self.convert.save_plan.is_some() || self.convert.plan.is_some() {
            return Err(conflict(
                "--save-plan or --plan",
                "a convert plan covers one convert, and each layer runs its own",
            ));
        }
        if self.keep_overview.is_some() {
            return Err(conflict(
                "--keep-overview",
                "each layer has its own intermediate overview",
            ));
        }
        if self.export.variable_depth {
            return Err(conflict(
                "--generate-variable-depth-tile-pyramid",
                "the layers are joined by a merge that needs every tile at every zoom",
            ));
        }
        super::layer_column::check_knobs(&self.convert, &self.export, column, None)
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
#[non_exhaustive]
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
#[non_exhaustive]
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
    #[doc(hidden)]
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
#[non_exhaustive]
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
#[non_exhaustive]
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
    /// A [`TilesOptions::layer_column`] run tiled each layer and merged
    /// them into one archive (#821).
    Layered(Box<super::layer_column::LayeredReport>),
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
    /// A refused option ([`ErrorKind::Usage`](crate::error::ErrorKind::Usage))
    /// is not prefixed: it is refused before any conversion starts (#779).
    #[error("{prefix}{0}", prefix = failed_prefix(.0.kind(), "overview conversion failed: "))]
    Convert(ConvertError),
    /// The export half failed; prefixed like [`TilesError::Convert`].
    #[error("{prefix}{0}", prefix = failed_prefix(.0.kind(), "export failed: "))]
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
    #[non_exhaustive]
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
    /// The [`TilesOptions::layer_column`] split failed (#821).
    #[error(transparent)]
    LayerColumn(#[from] super::layer_column::LayerColumnError),
    /// One layer of a [`TilesOptions::layer_column`] run failed.
    #[error("layer {layer:?}: {source}")]
    #[non_exhaustive]
    Layer {
        /// The layer.
        layer: String,
        /// Its failure.
        source: Box<TilesError>,
    },
    /// Merging the layers of a [`TilesOptions::layer_column`] run, or the
    /// cell and feature zooms of a [`TilesOptions::cell_bands`] run, failed.
    #[error("merging the layers failed: {0}")]
    Merge(crate::pyramid::PyramidError),
    /// One of [`TilesOptions::cell_bands`] failed (#332).
    #[error("cell band {band}: {source}")]
    #[non_exhaustive]
    CellBand {
        /// The band, as `--band` spells it.
        band: String,
        /// Its failure.
        source: Box<TilesError>,
    },
}

/// `prefix`, unless the error is a refused option, which names no step.
fn failed_prefix(kind: crate::error::ErrorKind, prefix: &'static str) -> &'static str {
    if kind == crate::error::ErrorKind::Usage {
        ""
    } else {
        prefix
    }
}

impl crate::error::Classify for TilesError {
    /// Each half classifies as its own error does.
    fn kind(&self) -> crate::error::ErrorKind {
        match self {
            TilesError::Config(_) => crate::error::ErrorKind::Usage,
            TilesError::Convert(e) => e.kind(),
            TilesError::Export(e) => e.kind(),
            TilesError::LayerColumn(e) => e.kind(),
            TilesError::Layer { source, .. } | TilesError::CellBand { source, .. } => source.kind(),
            TilesError::Merge(e) => e.kind(),
            _ => crate::error::fallback_kind(self),
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
    // #332: the plan is the feature zooms'; cells keep no plan state.
    let options = options.features();
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
pub(super) fn resolve_on_source<'a>(
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
    if let Some(column) = &options.layer_column {
        return super::layer_column::convert_by_layer(input, output, options, column);
    }
    if !options.cell_bands.is_empty() {
        return convert_with_cells(input, || input.source(), output, options, observer);
    }
    convert_to_pmtiles_validated(input, || input.source(), output, &config, observer)
}

/// The facade over `open`'s source (the test seam: [`TilesInput::source`]
/// in production, an in-memory remote in tests). `FnOnce`: the input is
/// opened exactly once, so the `auto` estimate's staged sample is still in
/// the source the conversion reads (#788).
pub(super) fn convert_to_pmtiles_validated(
    input: &TilesInput,
    open: impl FnOnce() -> Result<ConvertSource, ConvertError>,
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
    let source = open().map_err(TilesError::Convert)?;
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
    let (overview_path, overview_tmp): (PathBuf, Option<crate::atomic_output::Reserved>) =
        match &options.keep_overview {
            // Its directory was checked above.
            Some(path) => (path.clone(), None),
            None => {
                let dir = intermediate_dir(options, output);
                let tmp = crate::atomic_output::reserve_path(
                    ".tylertoo-overview-",
                    ".parquet",
                    Some(&dir),
                )
                .map_err(|source| TilesError::IntermediateCreate {
                    dir: dir.clone(),
                    source,
                })?;
                (tmp.to_path_buf(), Some(tmp))
            }
        };

    // Shared disk-footprint preflight: sum staging, pass-2 spill and the
    // intermediate on each destination volume. Warn before execution. Logged here
    // and carried in the report; a caller that must surface it before the
    // run (#709: Python's `UserWarning`) calls `intermediate_space_preflight`.
    let diagnostics = crate::diagnostics::Diagnostics::default();
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
                        options.export.tile_format,
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
                    write_empty_archive(
                        output,
                        &layer_name,
                        min_zoom,
                        max_zoom,
                        metadata,
                        options.export.tile_format,
                    )
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
        // #788: one memory profile governs both halves of the run.
        profile: options.convert.profile,
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

/// A run with [`TilesOptions::cell_bands`] (#332): one read of the input
/// for the cells, then the feature zooms' ordinary run on the same source,
/// then one merge into `output`. Each cell zoom is a table of cells tiled
/// verbatim at its zoom. A data shard owns no zoom a cell band reaches, so
/// it runs its feature zooms alone; the coarse job draws the cells.
///
/// `open` is [`convert_to_pmtiles_validated`]'s test seam.
fn convert_with_cells(
    input: &TilesInput,
    open: impl FnOnce() -> Result<ConvertSource, ConvertError>,
    output: &Path,
    options: &TilesOptions,
    observer: &mut dyn TilesObserver,
) -> Result<TilesOutcome, TilesError> {
    use crate::pyramid::{Band, BandMergeOptions};
    let features = options.features();
    let config = features.validate()?;
    if options
        .shard
        .as_ref()
        .is_some_and(|job| job.range.is_some())
    {
        return convert_to_pmtiles_validated(input, open, output, &config, observer);
    }
    crate::atomic_output::check_writable(output)?;
    let layer = options
        .layer_name
        .clone()
        .unwrap_or_else(|| input.derive_layer_name());
    let diagnostics = crate::diagnostics::Diagnostics::default();
    warn_band_gaps(options, &diagnostics);

    // Opened once: the cells read it first, then the feature run, which
    // finds the fetched chunks in the source's disk spill (#219). The
    // cell read projects its own columns, so it finishes before the
    // feature run locks the source's columns.
    let source = open().map_err(TilesError::Convert)?;
    source.set_spill_dir(options.convert.spill_dir.as_deref());
    let schema = source.schema().map_err(|e| TilesError::Convert(e.into()))?;
    let gates = super::layer_column::Gates::new(&source, &schema, &options.convert)
        .map_err(TilesError::Convert)?;
    let dir = intermediate_dir(options, output);
    if let Some(msg) = space_preflight(
        &source,
        gates.row_groups.as_ref(),
        output,
        options,
        CLI_REMEDIES,
    ) {
        log::warn!("{msg}");
    }
    let reserve = |prefix: &str, suffix: &str| {
        crate::atomic_output::reserve_path(prefix, suffix, Some(&dir)).map_err(|source| {
            TilesError::IntermediateCreate {
                dir: dir.clone(),
                source,
            }
        })
    };

    // Every archive stays reserved until the merge has read it.
    let mut archives: Vec<(Band, crate::atomic_output::Reserved)> = Vec::new();
    let mut cell_zooms = Vec::new();
    for band in &options.cell_bands {
        let in_band = |e| TilesError::CellBand {
            band: band.to_string(),
            source: Box::new(e),
        };
        let tables = (band.min_zoom..=band.max_zoom)
            .map(|_| reserve(".tylertoo-cells-", ".parquet"))
            .collect::<Result<Vec<_>, _>>()?;
        let paths: Vec<&Path> = tables.iter().map(|t| &**t).collect();
        let c = &options.convert;
        let read = ConvertOptions {
            profile: c.profile,
            in_flight_batches: c.in_flight_batches,
            read_workers: c.read_workers,
            spill_dir: c.spill_dir.clone(),
            ..ConvertOptions::default()
        };
        log::info!("[tiles] cell band {band}: counting the features in each cell");
        super::cells::write_cell_tables(
            &source,
            Some(&gates),
            band.system,
            (band.min_zoom, band.max_zoom),
            super::cells::CellRollup {
                accumulate: &c.accumulate,
                categories: c.cluster_categories.as_deref(),
            },
            &paths,
            &read,
            &diagnostics,
        )
        .map_err(|e| in_band(TilesError::Convert(e)))?;
        for (z, table) in (band.min_zoom..=band.max_zoom).zip(&tables) {
            let archive = reserve(".tylertoo-cells-", ".pmtiles")?;
            let report = tile_cell_table(table, z, &archive, &read, &options.export, &layer)
                .map_err(in_band)?;
            log::info!(
                "[tiles] cell band {band}: z{z}: {} tile(s)",
                report.export.total_tiles
            );
            // A table's convert repeats what the feature run reports about
            // the host (its memory budget); its export can lose cells.
            diagnostics.extend(report.export.diagnostics);
            cell_zooms.extend(report.export.zooms);
            archives.push((
                Band::new(archive.to_path_buf(), layer.clone(), z, z),
                archive,
            ));
        }
    }

    let archive = reserve(".tylertoo-features-", ".pmtiles")?;
    let outcome = convert_to_pmtiles_validated(input, || Ok(source), &archive, &config, observer)?;
    let report = match outcome {
        TilesOutcome::Converted(report) => Some(report),
        // A coarse job whose feature zooms hold nothing still has its cells.
        TilesOutcome::EmptyCoarse(_) => None,
        other => {
            return Err(TilesError::Convert(ConvertError::Io(
                std::io::Error::other(format!("unexpected outcome {other:?}")),
            )))
        }
    };
    if let Some(r) = &report {
        archives.push((
            Band::new(
                archive.to_path_buf(),
                layer.clone(),
                r.export.min_zoom,
                r.export.max_zoom,
            ),
            archive,
        ));
    }
    let bands: Vec<Band> = archives.iter().map(|(band, _)| band.clone()).collect();
    let merged = crate::pyramid::merge_bands_with_options(
        &bands,
        output,
        &BandMergeOptions {
            allow_missing_zooms: false,
            metadata: options.export.metadata.clone(),
            spill_dir: Some(dir.clone()),
        },
    )
    .map_err(TilesError::Merge)?;
    diagnostics.extend(merged.diagnostics);
    let cell_features: usize = cell_zooms.iter().map(|z| z.tile_feature_count).sum();
    let Some(mut report) = report else {
        let min_zoom = bands.iter().map(|b| b.min_zoom).min().unwrap_or(0);
        let max_zoom = bands.iter().map(|b| b.max_zoom).max().unwrap_or(0);
        let job = options
            .shard
            .as_ref()
            .expect("only a coarse job ends empty");
        return Ok(TilesOutcome::EmptyCoarse(EmptyArchive {
            total_tiles: merged.total_tiles,
            total_tile_features: cell_features,
            input_rows: 0,
            source_row_ranges: Vec::new(),
            diagnostics: diagnostics.snapshot(),
            source_losses: crate::diagnostics::SourceLosses::default(),
            role: job.role,
            pivot_zoom: job.pivot_zoom,
            min_zoom,
            max_zoom,
            why: "the feature zooms below the pivot hold nothing; the archive holds the cells"
                .to_string(),
        }));
    };
    // The report counts every zoom: the cells' first, then the features'.
    let export = &mut report.export;
    export.min_zoom = export.min_zoom.min(
        options
            .cell_bands
            .iter()
            .map(|b| b.min_zoom)
            .min()
            .unwrap_or(u8::MAX),
    );
    export.max_zoom = export.max_zoom.max(
        options
            .cell_bands
            .iter()
            .map(|b| b.max_zoom)
            .max()
            .unwrap_or(0),
    );
    cell_zooms.append(&mut export.zooms);
    cell_zooms.sort_by_key(|z| z.zoom);
    export.zooms = cell_zooms;
    export.total_tiles = merged.total_tiles;
    export.total_tile_features += cell_features;
    export.diagnostics.extend(diagnostics.snapshot());
    Ok(TilesOutcome::Converted(report))
}

/// Tile one zoom's cell `table` verbatim at zoom `z` into `archive`.
fn tile_cell_table(
    table: &Path,
    z: u8,
    archive: &Path,
    read: &ConvertOptions,
    export: &ExportOptions,
    layer: &str,
) -> Result<Box<TilesReport>, TilesError> {
    let cells = TilesOptions {
        convert: ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: z,
                max_zoom: z,
            },
            ..read.clone()
        }
        .verbatim(),
        // The cell tables carry only cells: no feature column to order or
        // id by, and the merged archive's metadata is the merge's.
        export: ExportOptions {
            metadata: crate::pmtiles_metadata::TilesetMetadata::default(),
            feature_order: FeatureOrder::default(),
            feature_id: None,
            ..export.clone()
        }
        .verbatim(),
        layer_name: Some(layer.to_string()),
        ..TilesOptions::default()
    };
    let input = TilesInput::Path(table.to_path_buf());
    match convert_to_pmtiles_validated(
        &input,
        || input.source(),
        archive,
        &cells.validate()?,
        &mut (),
    )? {
        TilesOutcome::Converted(report) => Ok(report),
        other => Err(TilesError::Convert(ConvertError::Io(
            std::io::Error::other(format!("unexpected outcome {other:?}")),
        ))),
    }
}

/// Warn about each zoom range between two bands that no band covers
/// (#332): it holds no tiles, though the archive declares it.
fn warn_band_gaps(options: &TilesOptions, diagnostics: &crate::diagnostics::Diagnostics) {
    let mut ranges: Vec<(u8, u8)> = options
        .cell_bands
        .iter()
        .map(|b| (b.min_zoom, b.max_zoom))
        .collect();
    if let LevelPlan::ZoomRange { min_zoom, max_zoom } = options.convert.levels {
        ranges.push((min_zoom, max_zoom));
    }
    ranges.sort_unstable();
    for w in ranges.windows(2) {
        if w[1].0 > w[0].1 + 1 {
            diagnostics.warn(format!(
                "no band covers z{}-{}: those zooms hold no tiles, though the archive \
                 declares them",
                w[0].1 + 1,
                w[1].0 - 1
            ));
        }
    }
}

/// `true` when a convert failed only because it had nothing to write.
///
/// Two spellings of the same outcome, depending on how far the run got before
/// it noticed: `NoData` when no level has a winner, `AllLevelsEmpty` when
/// every declared level turned out empty at write time. Both mean "zero rows
/// reached the output", which for a data shard is not an error at all.
pub(super) fn convert_produced_nothing(e: &ConvertError) -> bool {
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
pub(super) fn intermediate_dir(options: &TilesOptions, output: &Path) -> PathBuf {
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
    space_preflight(&input.source().ok()?, None, output, options, remedies)
}

/// [`intermediate_space_preflight`] on an open source, reading `selection`.
/// A `--layer-column` run (#821) also stages its split copy of the input
/// there, beside one layer's conversion at a time; the whole input's
/// conversion bounds each layer's.
pub(super) fn space_preflight(
    source: &ConvertSource,
    selection: Option<&crate::input_set::RowGroupSelection>,
    output: &Path,
    options: &TilesOptions,
    remedies: SpaceRemedies<'_>,
) -> Option<String> {
    let dir = intermediate_dir(options, output);
    let mut footprint = super::disk::DiskFootprint::conversion(
        source,
        selection,
        &options.convert,
        Some(&dir.join("overview.parquet")),
    )
    .ok()?;
    if options.layer_column.is_some() {
        footprint
            .add_staged_layers(source, selection, &options.convert, &dir)
            .ok()?;
    }
    footprint.warning(remedies)
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

    /// #791: the intermediate overview is published by renaming over its
    /// reserved path, which must not exist then: a rename over an existing
    /// file makes ext4 flush the whole overview inside the rename (0.76 s on
    /// Moldova, 13.8 s on Mato Grosso). Deterministic: no timing.
    #[test]
    fn tiles_publishes_the_intermediate_onto_a_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let scratch = dir.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        let input = dir.path().join("in.parquet");
        points_input(&input);
        let output = dir.path().join("out.pmtiles");
        let mut options = zoomed_options(0, 2);
        options.convert.spill_dir = Some(scratch.clone());

        convert_to_pmtiles(&TilesInput::Path(input), &output, &options).expect("must convert");

        let (published, over_existing) =
            crate::atomic_output::renames_over_existing(&scratch, ".tylertoo-overview-");
        assert_eq!(published, 1, "the intermediate is published exactly once");
        assert!(
            over_existing.is_empty(),
            "renamed over an existing file: {over_existing:?}"
        );
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
        assert_eq!(
            crate::error::Classify::kind(&err),
            crate::error::ErrorKind::Usage
        );
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

    /// #788: one memory profile governs both halves of a `tiles` run: the
    /// export runs under the convert's profile, not its own default.
    #[test]
    fn the_export_runs_under_the_converts_memory_profile() {
        let logging = crate::test_log::install();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.parquet");
        let geoms: Vec<Option<Geometry<f64>>> = vec![
            Some(Geometry::Point(Point::new(0.5, 0.5))),
            Some(Geometry::Point(Point::new(10.0, 20.0))),
        ];
        write_input(&input, &geoms, true, None);
        let mut options = zoomed_options(0, 4);
        options.convert.profile = crate::overview::level::MemoryProfile::Bounded;
        assert_ne!(options.export.profile, options.convert.profile);
        crate::test_log::take_thread_lines();
        convert_to_pmtiles(
            &TilesInput::Path(input),
            &dir.path().join("out.pmtiles"),
            &options,
        )
        .unwrap();
        if logging {
            let lines = crate::test_log::take_thread_lines();
            assert!(
                lines.iter().any(
                    |l| l.starts_with("[export] partition wave") && l.contains("under bounded")
                ),
                "{lines:#?}"
            );
        }
    }

    /// A bound shard plan refuses a zoom-range build whose finest zoom is
    /// coarser than the plan's pivot; without a plan there is nothing to check.
    #[test]
    fn a_bound_plan_refuses_a_max_zoom_below_its_pivot() {
        let plan = crate::shard::ShardPlan {
            format: "tylertoo-shard-plan".to_string(),
            version: crate::shard::SHARD_PLAN_VERSION,
            pivot_zoom: 9,
            ranges: Vec::new(),
            inputs: Vec::new(),
            estimated_rows_total: 0,
            unplaced_rows: 0,
        };
        let bound = TilesShard {
            binding: Some(ShardBinding {
                plan,
                path: PathBuf::from("shards.json"),
            }),
            ..shard_job(None, 9)
        };
        let up_to = |max_zoom| LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom,
        };
        assert!(matches!(
            bound.check_max_zoom(&up_to(8)),
            Err(TilesError::Shard(
                crate::shard::ShardError::PivotNotCoarser {
                    pivot: 9,
                    max_zoom: 8,
                    ..
                }
            ))
        ));
        assert!(bound.check_max_zoom(&up_to(9)).is_ok());
        assert!(shard_job(None, 9).check_max_zoom(&up_to(8)).is_ok());
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
            assert_eq!(early.kind(), crate::error::ErrorKind::Usage, "{early:?}");
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

    /// #822: the tippecanoe preset reaches the archive. A point 6.5 tile
    /// pixels east of the z1 seam sits inside the western tile's default
    /// 8 px buffer but outside tippecanoe's 5 px one.
    #[test]
    fn tippecanoe_preset_narrows_the_tile_buffer_in_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("seam.parquet");
        // One z1 tile spans 180 degrees, so a tile pixel is 180/256 degrees.
        let lon = 6.5 * 180.0 / 256.0;
        write_input(&input, &[Some(Point::new(lon, 10.0).into())], false, None);
        let tiles_at_z1 = |keywords: serde_json::Value| {
            let mut keywords = keywords.as_object().unwrap().clone();
            keywords.insert("min_zoom".into(), 1.into());
            keywords.insert("max_zoom".into(), 1.into());
            let options = crate::overview::config::keywords::tiles(keywords).unwrap();
            let output = dir.path().join("out.pmtiles");
            let _ = std::fs::remove_file(&output);
            let outcome =
                convert_to_pmtiles(&TilesInput::Path(input.clone()), &output, &options).unwrap();
            let TilesOutcome::Converted(report) = outcome else {
                panic!("expected a full conversion, got {outcome:?}");
            };
            report.export.total_tile_features
        };
        assert_eq!(tiles_at_z1(serde_json::json!({})), 2, "default 8 px buffer");
        assert_eq!(
            tiles_at_z1(serde_json::json!({"preset": "tippecanoe"})),
            1,
            "tippecanoe's 5 px buffer"
        );
    }

    /// The derived layer name follows core's input rules for both shapes.
    #[cfg(feature = "mlt")]
    #[test]
    fn an_empty_mlt_layer_name_is_refused_before_any_io() {
        let options = TilesOptions {
            layer_name: Some(String::new()),
            export: ExportOptions::default().with_tile_format(crate::TileFormat::Mlt),
            ..TilesOptions::default()
        };
        let err = options.validate().unwrap_err().to_string();
        assert!(err.contains("an MLT layer needs a name"), "{err}");
    }

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

    // --- `--band` cell bands (#332) ---------------------------------------

    use crate::overview::cells::{CellBand, CellSystem};
    use crate::overview::cluster::{AccumulateOp, AccumulateSpec};
    use arrow_array::Array as _;
    use std::collections::BTreeMap;

    /// Small squares on a 0.01° lattice off Manhattan, each with a
    /// `metrics:area` of its row number + 1.
    fn fields(path: &Path, n: usize) {
        let geoms: Vec<Option<geo::Geometry<f64>>> = (0..n)
            .map(|i| {
                let (x, y) = (
                    -74.0 + (i % 40) as f64 * 0.01,
                    40.6 + (i / 40) as f64 * 0.01,
                );
                let rect = geo::Rect::new((x, y), (x + 0.004, y + 0.004));
                Some(geo::Geometry::Polygon(rect.to_polygon()))
            })
            .collect();
        let values: Vec<f64> = (1..=n).map(|v| v as f64).collect();
        crate::overview::testutil::write_input_with_f64(path, &geoms, "metrics:area", &values);
    }

    /// `--band` specs applied to the default tiles options.
    fn banded(specs: &[&str]) -> Result<TilesOptions, ConfigError> {
        let mut options = TilesOptions::default();
        let specs: Vec<String> = specs.iter().map(|s| (*s).to_string()).collect();
        super::super::config::apply_bands(&mut options, &specs, &[])?;
        Ok(options)
    }

    /// Per zoom of `archive`, the `point_count` sum and the `metrics:area`
    /// sum over its cells (each counted once, however many tiles hold it),
    /// and the feature count at the zooms without cells.
    fn decoded_zooms(archive: &Path, dir: &Path) -> BTreeMap<u8, (f64, f64, usize)> {
        use arrow_array::cast::AsArray;
        let out = dir.join(format!(
            "decoded-{}.parquet",
            archive.file_stem().unwrap().to_string_lossy()
        ));
        crate::decode::decode_pmtiles(archive, &out, &crate::decode::DecodeOptions::default())
            .unwrap();
        let source = ConvertSource::resolve_path(&out).unwrap();
        let plan = crate::input_set::ReadPlan {
            batch_size: 1 << 16,
            projection: None,
            row_groups: None,
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut zooms: BTreeMap<u8, (f64, f64, usize)> = BTreeMap::new();
        for batch in source.open_stream(&plan).unwrap() {
            let batch = batch.unwrap();
            let f64s = |name: &str| {
                batch.schema().index_of(name).ok().map(|i| {
                    arrow_cast::cast(batch.column(i), &arrow_schema::DataType::Float64).unwrap()
                })
            };
            let zoom = f64s("zoom").unwrap();
            let zoom = zoom.as_primitive::<arrow_array::types::Float64Type>();
            let count = f64s("point_count");
            let area = f64s("metrics:area").unwrap();
            let area = area.as_primitive::<arrow_array::types::Float64Type>();
            let cell = batch
                .schema()
                .index_of("cell")
                .ok()
                .map(|i| batch.column(i).clone());
            for r in 0..batch.num_rows() {
                let z = zoom.value(r) as u8;
                let entry = zooms.entry(z).or_default();
                let token = cell
                    .as_ref()
                    .map(|c| c.as_string::<i32>())
                    .filter(|c| c.is_valid(r))
                    .map(|c| c.value(r).to_string());
                match (token, &count) {
                    (Some(token), Some(count)) => {
                        if seen.insert((z, token)) {
                            entry.0 += count
                                .as_primitive::<arrow_array::types::Float64Type>()
                                .value(r);
                            entry.1 += area.value(r);
                        }
                    }
                    _ => entry.2 += 1,
                }
            }
        }
        zooms
    }

    #[test]
    fn bands_set_the_zoom_range_representation_and_cells() {
        let o = banded(&["0-7:a5", "8-14:geom"]).unwrap();
        assert_eq!(
            o.convert.levels,
            LevelPlan::ZoomRange {
                min_zoom: 8,
                max_zoom: 14
            }
        );
        assert!(o.convert.representation.is_empty());
        assert_eq!(o.cell_bands, vec![CellBand::new(0, 7, CellSystem::A5)]);

        let o = banded(&["8-14:geom", "0-7:point"]).unwrap();
        assert_eq!(
            o.convert.levels,
            LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 14
            }
        );
        assert_eq!(
            o.convert.representation,
            super::super::config::representation(Some("0-7:point,8-14:geom")).unwrap()
        );
        assert!(o.cell_bands.is_empty());

        for (specs, needle) in [
            (&["0-7:a5"][..], "no geom, point or square band"),
            (&["0-7:hex", "8-14:geom"], "unknown kind \"hex\""),
            (&["7-0:a5", "8-14:geom"], "runs from z7 down to z0"),
            (&["0-6:point", "8-14:geom"], "leave a gap"),
            (&["0-8:point", "8-14:geom"], "overlap"),
            (&["0:a5", "x-14:geom"], "invalid zoom"),
            (&["8-14"], "must be LO-HI:KIND"),
        ] {
            let err = banded(specs).unwrap_err().to_string();
            assert!(err.contains(needle), "{specs:?}: {err}");
        }
        let mut options = TilesOptions::default();
        let err = super::super::config::apply_bands(
            &mut options,
            &["0-7:a5".to_string(), "8-14:geom".to_string()],
            &["min_zoom"],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("--band cannot be combined with min_zoom"),
            "{err}"
        );
    }

    /// Every cell-band refusal fires from the options alone: the input
    /// does not exist, so a rule checked after a read would fail on it.
    #[test]
    fn cell_band_refusals_fire_before_any_read() {
        let dir = tempfile::tempdir().unwrap();
        let input = TilesInput::Path(dir.path().join("missing.parquet"));
        let out = dir.path().join("out.pmtiles");
        let base = banded(&["0-7:a5", "8-14:geom"]).unwrap();
        let shard = |pivot| TilesShard::new(ShardRole::Coarse, None, pivot, "deadbeef", None);
        let cases: Vec<(TilesOptions, &str)> = vec![
            (
                TilesOptions {
                    cell_bands: vec![CellBand::new(0, 8, CellSystem::A5)],
                    ..base.clone()
                },
                "bands 0-8:a5 and 8-14 (the feature zooms) overlap at z8",
            ),
            (
                TilesOptions {
                    cell_bands: vec![
                        CellBand::new(0, 4, CellSystem::H3),
                        CellBand::new(3, 7, CellSystem::A5),
                    ],
                    ..base.clone()
                },
                "bands 0-4:h3 and 3-7:a5 overlap at z3",
            ),
            (
                TilesOptions {
                    layer_column: Some("crop".into()),
                    ..base.clone()
                },
                "cannot be combined with --layer-column",
            ),
            (
                TilesOptions {
                    export: ExportOptions {
                        variable_depth: true,
                        ..ExportOptions::default()
                    },
                    ..base.clone()
                },
                "--generate-variable-depth-tile-pyramid",
            ),
            (
                TilesOptions {
                    export: ExportOptions {
                        tile_range: Some(TileRange::parse("1..2").unwrap()),
                        ..ExportOptions::default()
                    },
                    ..base.clone()
                },
                "--tile-range",
            ),
            (
                TilesOptions {
                    convert: ConvertOptions {
                        auto_max_zoom: true,
                        ..base.convert.clone()
                    },
                    ..base.clone()
                },
                "--max-zoom auto",
            ),
            (
                TilesOptions {
                    convert: ConvertOptions {
                        levels: LevelPlan::Gsds(vec![100.0]),
                        ..base.convert.clone()
                    },
                    ..base.clone()
                },
                "--gsd",
            ),
            (
                TilesOptions {
                    shard: Some(shard(7)),
                    ..base.clone()
                },
                "cell band 0-7:a5 reaches the shard plan's pivot z7",
            ),
        ];
        for (options, needle) in cases {
            let err = convert_to_pmtiles(&input, &out, &options).unwrap_err();
            assert!(matches!(err, TilesError::Config(_)), "{needle}: {err}");
            assert!(err.to_string().contains(needle), "{needle}: {err}");
        }
        // The rollups need no --cluster with a cell band, and a pivot past
        // the cells is a valid coarse job.
        let ok = TilesOptions {
            convert: ConvertOptions {
                accumulate: vec![AccumulateSpec::new("metrics:area", AccumulateOp::Sum)],
                ..base.convert.clone()
            },
            shard: Some(shard(10)),
            ..base.clone()
        };
        ok.validate().unwrap();
    }

    /// `--band 0-3:point --band 4-6:geom` is the representation selector
    /// over the same zooms, byte for byte.
    #[test]
    fn point_and_geom_bands_match_the_representation_selector() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("fields.parquet");
        fields(&src, 200);
        let input = TilesInput::Path(src);
        let banded_out = dir.path().join("banded.pmtiles");
        convert_to_pmtiles(
            &input,
            &banded_out,
            &banded(&["0-3:point", "4-6:geom"]).unwrap(),
        )
        .unwrap();
        let selector_out = dir.path().join("selector.pmtiles");
        let selector = TilesOptions {
            convert: ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 6,
                },
                representation: super::super::config::representation(Some("0-3:point,4-6:geom"))
                    .unwrap(),
                ..TilesOptions::default().convert
            },
            ..TilesOptions::default()
        };
        convert_to_pmtiles(&input, &selector_out, &selector).unwrap();
        assert!(
            std::fs::read(&banded_out).unwrap() == std::fs::read(&selector_out).unwrap(),
            "the archives differ"
        );
    }

    /// The target shape: cells below, the fields above, in one layer. Every
    /// cell zoom counts every field once and sums its area, the rollup
    /// shares the property's name without a warning, and the feature zooms
    /// keep each field's own area.
    #[test]
    fn cells_below_and_features_above_share_one_layer() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("fields.parquet");
        let n = 400;
        fields(&src, n);
        let out = dir.path().join("out.pmtiles");
        let mut options = banded(&["0-5:a5", "6-8:geom"]).unwrap();
        options.convert.accumulate = vec![AccumulateSpec::new("metrics:area", AccumulateOp::Sum)];
        let TilesOutcome::Converted(report) =
            convert_to_pmtiles(&TilesInput::Path(src), &out, &options).unwrap()
        else {
            panic!("expected a converted archive");
        };
        let diagnostics: Vec<String> = report
            .convert
            .diagnostics
            .iter()
            .chain(&report.export.diagnostics)
            .map(ToString::to_string)
            // The host's memory limit, reported under a cgroup.
            .filter(|d| !d.starts_with("memory budget"))
            .collect();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            report
                .export
                .zooms
                .iter()
                .map(|z| z.zoom)
                .collect::<Vec<_>>(),
            (0..=8).collect::<Vec<u8>>()
        );
        assert_eq!((report.export.min_zoom, report.export.max_zoom), (0, 8));
        let area: f64 = (1..=n).map(|v| v as f64).sum();
        let zooms = decoded_zooms(&out, dir.path());
        for z in 0..=5u8 {
            assert_eq!(zooms[&z].0, n as f64, "z{z} point_count");
            assert_eq!(zooms[&z].1, area, "z{z} area");
        }
        for z in 6..=8u8 {
            assert!(zooms[&z].2 > 0, "z{z} has fields: {zooms:?}");
            assert_eq!(zooms[&z].0, 0.0, "z{z} has no cells");
        }
        let metadata = crate::archive_index::ArchiveIndex::open(&out)
            .unwrap()
            .metadata_json()
            .unwrap();
        let layers = metadata["vector_layers"].as_array().unwrap();
        assert_eq!(layers.len(), 1, "{layers:?}");
        assert_eq!(
            (layers[0]["minzoom"].as_u64(), layers[0]["maxzoom"].as_u64()),
            (Some(0), Some(8))
        );
        for field in ["cell", "point_count", "metrics:area"] {
            assert!(
                layers[0]["fields"].get(field).is_some(),
                "{field}: {layers:?}"
            );
        }
    }

    /// A zoom no band covers is allowed, empty, and warned about.
    #[test]
    fn a_gap_between_bands_is_empty_and_warned() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("fields.parquet");
        fields(&src, 50);
        let out = dir.path().join("out.pmtiles");
        let TilesOutcome::Converted(report) = convert_to_pmtiles(
            &TilesInput::Path(src),
            &out,
            &banded(&["0-2:grid", "5-6:geom"]).unwrap(),
        )
        .unwrap() else {
            panic!("expected a converted archive");
        };
        let text: Vec<String> = report
            .export
            .diagnostics
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(
            text.iter().any(|d| d.contains("no band covers z3-4")),
            "{text:?}"
        );
        let zooms = decoded_zooms(&out, dir.path());
        assert_eq!(
            zooms.keys().copied().collect::<Vec<_>>(),
            vec![0, 1, 2, 5, 6]
        );
    }

    /// A field that crosses the bbox is kept by its geometry and counted in
    /// the cell of its centroid, which lies outside the bbox; a field that
    /// misses it is not counted.
    #[test]
    fn a_field_crossing_the_bbox_counts_in_its_centroid_cell() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("fields.parquet");
        let crossing = geo::Rect::new((0.5, 0.5), (3.5, 1.5)).to_polygon();
        let outside = geo::Rect::new((20.0, 20.0), (21.0, 21.0)).to_polygon();
        crate::overview::testutil::write_input_with_f64(
            &src,
            &[
                Some(geo::Geometry::Polygon(crossing)),
                Some(geo::Geometry::Polygon(outside)),
            ],
            "metrics:area",
            &[1.0, 2.0],
        );
        let out = dir.path().join("out.pmtiles");
        let mut options = banded(&["0-3:grid", "4-4:geom"]).unwrap();
        options.convert.bbox = Some([0.0, 0.0, 1.0, 1.0]);
        convert_to_pmtiles(&TilesInput::Path(src), &out, &options).unwrap();
        let zooms = decoded_zooms(&out, dir.path());
        for z in 0..=3u8 {
            assert_eq!(zooms[&z].0, 1.0, "z{z}: only the crossing field");
        }
        // The z3 cell (grid z9) holding the centroid (2, 1), outside the bbox.
        let decoded = dir.path().join("decoded-out.parquet");
        let source = ConvertSource::resolve_path(&decoded).unwrap();
        let schema = source.schema().unwrap();
        let cell = schema.index_of("cell").unwrap();
        let t = crate::tile::lng_lat_to_tile(2.0, 1.0, 9);
        let want = format!("9/{}/{}", t.x, t.y);
        let mut found = false;
        let plan = crate::input_set::ReadPlan {
            batch_size: 1 << 16,
            projection: None,
            row_groups: None,
        };
        for batch in source.open_stream(&plan).unwrap() {
            use arrow_array::cast::AsArray;
            let batch = batch.unwrap();
            found |= batch
                .column(cell)
                .as_string::<i32>()
                .iter()
                .flatten()
                .any(|c| c == want);
        }
        assert!(found, "no cell {want}");
    }

    /// A mask and a filter with SQL NULLs: the cells count exactly the rows
    /// both keep. The filter's column sits at another index in the
    /// projected batch than in the file.
    #[test]
    fn a_mask_and_a_null_filter_gate_the_cells() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("fields.parquet");
        let geoms: Vec<Option<geo::Geometry<f64>>> = (0..40)
            .map(|i| {
                let x = (i % 20) as f64 * 0.1;
                let y = (i / 20) as f64 * 5.0;
                Some(geo::Geometry::Point(geo::Point::new(x, y)))
            })
            .collect();
        // Every third row is NULL, then alternately 1 and -1.
        let values: Vec<Option<f64>> = (0..40)
            .map(|i| (i % 3 != 0).then_some(if i % 2 == 0 { 1.0 } else { -1.0 }))
            .collect();
        let expect = (0..40)
            .filter(|i| i % 3 != 0 && i % 2 == 0 && i / 20 == 0)
            .count();
        let geom_first = {
            let ids = arrow_array::Float64Array::from(values.clone());
            std::sync::Arc::new(ids) as std::sync::Arc<dyn arrow_array::Array>
        };
        crate::overview::testutil::write_input_with_column(&src, &geoms, "v", geom_first, None);
        let mask = dir.path().join("mask.geojson");
        std::fs::write(
            &mask,
            r#"{"type":"Polygon","coordinates":[[[-1,-1],[3,-1],[3,1],[-1,1],[-1,-1]]]}"#,
        )
        .unwrap();
        let out = dir.path().join("out.pmtiles");
        let mut options = banded(&["0-2:s2", "3-3:geom"]).unwrap();
        options.convert.mask = Some(super::super::mask::MaskSource::Path(mask));
        options.convert.filter = Some("v > 0".into());
        options.convert.accumulate = vec![AccumulateSpec::new("v", AccumulateOp::Sum)];
        convert_to_pmtiles(&TilesInput::Path(src), &out, &options).unwrap();
        let zooms = decoded_cells_v(&out, dir.path());
        for z in 0..=2u8 {
            assert_eq!(zooms[&z], (expect as f64, expect as f64), "z{z}");
        }
    }

    /// Per zoom, the `point_count` and `v` sums of the cells.
    fn decoded_cells_v(archive: &Path, dir: &Path) -> BTreeMap<u8, (f64, f64)> {
        use arrow_array::cast::AsArray;
        use arrow_array::types::Float64Type;
        let out = dir.join("decoded-v.parquet");
        crate::decode::decode_pmtiles(archive, &out, &crate::decode::DecodeOptions::default())
            .unwrap();
        let source = ConvertSource::resolve_path(&out).unwrap();
        let plan = crate::input_set::ReadPlan {
            batch_size: 1 << 16,
            projection: None,
            row_groups: None,
        };
        let mut seen = std::collections::BTreeSet::new();
        let mut zooms: BTreeMap<u8, (f64, f64)> = BTreeMap::new();
        for batch in source.open_stream(&plan).unwrap() {
            let batch = batch.unwrap();
            let col = |name: &str| {
                arrow_cast::cast(
                    batch.column(batch.schema().index_of(name).unwrap()),
                    &arrow_schema::DataType::Float64,
                )
                .unwrap()
            };
            let (zoom, count, v) = (col("zoom"), col("point_count"), col("v"));
            let cell = batch
                .column(batch.schema().index_of("cell").unwrap())
                .clone();
            for r in 0..batch.num_rows() {
                if !cell.is_valid(r) {
                    continue;
                }
                let z = zoom.as_primitive::<Float64Type>().value(r) as u8;
                if seen.insert((z, cell.as_string::<i32>().value(r).to_string())) {
                    let e = zooms.entry(z).or_default();
                    e.0 += count.as_primitive::<Float64Type>().value(r);
                    e.1 += v.as_primitive::<Float64Type>().value(r);
                }
            }
        }
        zooms
    }

    /// One read of a remote input: the cells' read and the feature run's
    /// share the source's disk spill, so the cell band costs no more
    /// network than the feature zooms alone plus the rollup column, even
    /// with a chunk cache too small to keep a row group.
    #[cfg(feature = "remote")]
    #[test]
    fn a_cell_band_reads_a_remote_input_once() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("fields.parquet");
        let n = 3000;
        let geoms: Vec<Option<geo::Geometry<f64>>> = (0..n)
            .map(|i| {
                let (x, y) = ((i % 60) as f64 * 0.01, (i / 60) as f64 * 0.01);
                Some(geo::Geometry::Polygon(
                    geo::Rect::new((x, y), (x + 0.004, y + 0.004)).to_polygon(),
                ))
            })
            .collect();
        let values: Vec<f64> = (0..n).map(|v| v as f64).collect();
        crate::overview::testutil::write_input_with_f64_row_groups(
            &src,
            &geoms,
            "metrics:area",
            &values,
            Some(500),
        );
        let bytes = std::fs::read(&src).unwrap();
        let fetched = |options: &TilesOptions, name: &str| {
            let part = crate::input::test_memory_source_with_cap(bytes.clone(), name, 64);
            let config = options.features().into_owned();
            let out = dir.path().join(format!("{name}.pmtiles"));
            if options.cell_bands.is_empty() {
                convert_to_pmtiles_validated(
                    &TilesInput::Path(dir.path().join(name)),
                    || Ok(ConvertSource::single(part.clone())),
                    &out,
                    &config.validate().unwrap(),
                    &mut (),
                )
                .unwrap();
            } else {
                convert_with_cells(
                    &TilesInput::Path(dir.path().join(name)),
                    || Ok(ConvertSource::single(part.clone())),
                    &out,
                    options,
                    &mut (),
                )
                .unwrap();
            }
            part.fetch_stats().unwrap().bytes_fetched
        };
        let mut cells = banded(&["0-5:a5", "6-8:geom"]).unwrap();
        cells.convert.accumulate = vec![AccumulateSpec::new("metrics:area", AccumulateOp::Sum)];
        let features = banded(&["6-8:geom"]).unwrap();
        let with_cells = fetched(&cells, "cells.parquet");
        let alone = fetched(&features, "alone.parquet");
        let area_column: u64 = {
            let meta =
                tylertoo_io::read_parquet_metadata(&bytes::Bytes::from(bytes.clone())).unwrap();
            meta.row_groups()
                .iter()
                .map(|rg| {
                    let c = rg
                        .columns()
                        .iter()
                        .find(|c| c.column_path().string() == "metrics:area")
                        .unwrap();
                    c.compressed_size() as u64
                })
                .sum()
        };
        assert!(
            with_cells <= alone + area_column,
            "the cell band fetched {with_cells} B, the feature zooms alone {alone} B, \
             the rollup column is {area_column} B"
        );
    }
}
