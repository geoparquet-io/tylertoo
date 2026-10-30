//! The one owner of the option rules every frontend shares (#779).
//!
//! The CLI and the Python bindings each spell the convert and export knobs
//! their own way (`--class-rank COL:V=R` against a `class_ranks` dict, three
//! `--collapse*` switches against three booleans), but what the knobs *mean*
//! — which combinations conflict, what a missing value defaults to, which
//! values are out of range, which options a command cannot honour — is
//! decided here, once:
//!
//! - [`ConfigError`]: every rule that needs no input file, typed. The
//!   frontends surface it as is; they hold no copy of any rule.
//! - [`check_output`]: the output-path preflight (an existing file without
//!   `--force`, a directory, an unwritable parent).
//! - [`collapse_mode`], [`entry_zoom`], [`class_ranking`] and
//!   [`representation`]: the flag-shaped inputs, which
//!   [`keywords`] turns into
//!   [`ConvertOptions`](super::convert::ConvertOptions) fields. The
//!   frontends deserialize core options.
//!
//! The option structs validate themselves before any input is read:
//! [`ConvertOptions::validate`](super::convert::ConvertOptions::validate),
//! [`ExportOptions::validate`](super::export::ExportOptions::validate),
//! [`TilesOptions::validate`](super::tiles::TilesOptions::validate) and
//! [`PyramidOptions::validate`](crate::pyramid::PyramidOptions::validate).

use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use super::convert::{
    parse_class_rank, parse_entry_zoom, parse_representation_spec, ClassRanking, RepresentationBand,
};
use super::ladder::{EntryZoomKind, EntryZoomSpec};
use super::simplify::CollapseMode;

/// The default `--ladder-step`: one zoom between `--magnitude-ladder` rungs.
pub const DEFAULT_LADDER_STEP: u8 = 1;

/// What an output file is, for [`ConfigError::OutputIsDirectory`]'s message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// A PMTiles archive (`tiles`, `export-pmtiles`, `pyramid`, `merge`).
    Pmtiles,
    /// A GeoParquet file (`overview`, `decode`).
    GeoParquet,
    /// A shard plan (`shard-plan`, #498). Rewritten in place, so an
    /// existing one needs only to be writable, not its directory.
    ShardPlan,
}

impl std::fmt::Display for OutputKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OutputKind::Pmtiles => "PMTiles",
            OutputKind::GeoParquet => "GeoParquet",
            OutputKind::ShardPlan => "shard plan",
        })
    }
}

/// An option value or combination refused before any input is read.
///
/// Every variant is the caller's to fix, not the data's: the CLI prints it,
/// the Python bindings raise it as `tylertoo.ConfigError` (a `ValueError`).
/// Messages name options by their CLI spelling, as the rest of core's do.
///
/// `#[non_exhaustive]`: new rules are added without a breaking change; match
/// with a wildcard arm.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The output path is an existing directory (#427, #551). The final
    /// rename can never replace one, `--force` or not.
    #[error("{} is a directory; the output must be a {kind} file path", path.display())]
    OutputIsDirectory {
        /// The output path.
        path: PathBuf,
        /// What the output was meant to be.
        kind: OutputKind,
    },
    /// The output exists and overwriting it was not asked for (#427, #551).
    #[error("{} exists (use --force to overwrite){}", path.display(), if matches!(kind, OutputKind::ShardPlan) { ". Every job of a fleet must be given the SAME shard plan, so replacing one mid-build silently re-cuts the tile space." } else { "" })]
    OutputExists {
        /// What the existing output represents.
        kind: OutputKind,
        /// The output path.
        path: PathBuf,
    },
    /// The output cannot be created (#452): a missing or read-only directory.
    #[error(transparent)]
    Output(#[from] crate::atomic_output::OutputNotWritable),
    /// `--spill-dir` is not an existing directory (#272, #427). The spill is
    /// opened lazily, so an unusable directory would otherwise surface only
    /// deep into the run.
    #[error("spill-dir {} is not an existing directory", .0.display())]
    SpillDirNotDirectory(PathBuf),
    /// `--spill-dir` exists but does not accept new files (a read-only
    /// directory): found by creating and removing a probe file.
    #[error("spill-dir {} is not writable: {reason}", dir.display())]
    SpillDirNotWritable {
        /// The spill directory.
        dir: PathBuf,
        /// The OS error from the probe.
        reason: String,
    },
    /// `extent` is zero (#433). Zero quantizes every coordinate to `(0, 0)`:
    /// every polygon and line degenerates, and every consumer then divides
    /// by the layer's `extent: 0`.
    #[error(
        "extent {extent} is not a valid MVT tile extent: the extent is the tile-local \
         coordinate resolution and must be positive (the default is {})",
        super::export::DEFAULT_EXTENT
    )]
    InvalidExtent {
        /// The rejected extent.
        extent: u32,
    },
    /// `--tile-buffer` is wider than
    /// [`MAX_TILE_BUFFER_PX`](super::export::MAX_TILE_BUFFER_PX) (#433).
    #[error(
        "--tile-buffer {buffer} is wider than the {max} tile pixel cap (one full tile width): \
         past it a tile duplicates geometry from tiles it does not border and every feature \
         belongs to O(buffer²) tiles. Lower --tile-buffer to {max} or less (the default is {}).",
        super::export::DEFAULT_TILE_BUFFER_PX
    )]
    TileBufferTooWide {
        /// The requested buffer, in tile pixels.
        buffer: u32,
        /// [`MAX_TILE_BUFFER_PX`](super::export::MAX_TILE_BUFFER_PX).
        max: u32,
    },
    /// `--bbox` is not `xmin,ymin,xmax,ymax` with finite, ordered corners.
    /// A NaN corner matches nothing and an inverted box selects nothing, so
    /// the run would read the input to produce an empty archive.
    #[error(
        "bbox {bbox:?} must hold finite values with xmin <= xmax and ymin <= ymax \
         (xmin,ymin,xmax,ymax)"
    )]
    InvalidBbox {
        /// The rejected bbox.
        bbox: [f64; 4],
    },
    /// The publication metadata cannot fit an archive's metadata section.
    #[error("{0}")]
    MetadataTooLarge(String),
    /// A class ranking built without [`ClassRanking::new`] carries a rank
    /// that is not finite (#428, b8fa58a). A NaN is not ordered, so the
    /// incumbent of a contested cell would silently keep it.
    #[error(
        "rank {rank} for {} must be a finite number (NaN and infinity cannot be ordered \
         against other classes)",
        class.as_deref().map_or_else(|| "unknown (unlisted) classes".to_string(), |c| format!("class {c:?}"))
    )]
    ClassRankNotFinite {
        /// The class, or `None` for the unknown-class rank.
        class: Option<String>,
        /// The rank.
        rank: f64,
    },
    /// A class ranking's column was given without its ranks, or the reverse
    /// (the Python `class_rank_column` / `class_ranks` pair).
    #[error("class_rank_column and class_ranks must be supplied together")]
    ClassRankIncomplete,
    /// A class ranking was given both as a `--class-rank` spec and as its
    /// structured parts.
    #[error("class_rank and class_rank_column/class_ranks spell the same ranking; supply one")]
    ClassRankSpelledTwice,
    /// A spec string (`--entry-zoom`, `--class-rank`, `--representation`,
    /// `--accumulate-attribute`) did not parse. The message names the flag
    /// and the offending part.
    #[error("{message}")]
    InvalidSpec {
        /// The flag, as the CLI spells it.
        option: &'static str,
        /// What is wrong with the spec.
        message: String,
    },
    /// More than one of `--collapse`, `--collapse-square` and
    /// `--no-collapse` (#279, #745).
    #[error("--collapse, --collapse-square and --no-collapse are mutually exclusive")]
    CollapseConflict,
    /// Both `--magnitude-ladder` and `--entry-zoom` (#364).
    #[error("--magnitude-ladder and --entry-zoom are mutually exclusive; supply at most one")]
    EntryZoomConflict,
    /// An explicit request to coalesce lines in partitioning mode (Q3).
    /// Coalescing is on by default and silently inert there; asking for it
    /// is an error the caller should hear about.
    #[error(
        "--coalesce-lines requires --mode duplicating: partitioning places each feature \
         exactly once with geometry verbatim, which a merged chain cannot satisfy"
    )]
    CoalescePartitioningUnsupported,
    /// A hand-written export tile range alongside a shard job (#498). A data
    /// shard's own range prunes the convert's reads too, so the hand-written
    /// one could only be silently ignored; the coarse job owns every zoom
    /// below the pivot, which a range would silently cut.
    #[error(
        "--tile-range cannot be combined with --shard: a data shard's own range restricts the \
         export, and the coarse job owns every zoom below the pivot"
    )]
    TileRangeWithShard,
    /// Both `--save-plan` and `--plan`: one writes the pass-1/assignment
    /// artifact, the other replaces pass 1 with a saved one.
    #[error(
        "--save-plan and --plan are mutually exclusive: one writes the pass-1/assignment \
         artifact, the other replaces pass 1 with a saved one"
    )]
    SavePlanWithPlan,
    /// The coarse job of a sharded build owns `[min_zoom, pivot - 1]`, and a
    /// pivot at or below `min_zoom` leaves it nothing to build (#498, #709).
    /// Without this the whole convert runs before the export refuses an
    /// empty zoom restriction.
    #[error(
        "--shard coarse has no zoom to build: the coarse job owns z{min_zoom}..z{} and the \
         shard plan's pivot is z{pivot}. Lower --min-zoom, or re-cut the plan with a finer \
         --pivot.",
        pivot.saturating_sub(1)
    )]
    CoarseShardHasNoZoom {
        /// The plan's pivot zoom.
        pivot: u8,
        /// The run's minimum zoom.
        min_zoom: u8,
    },
    /// A data shard's range sits at a zoom other than the job's pivot (#709):
    /// the facade reads the pivot from both, so they must agree.
    #[error(
        "the shard's tile range is cut at z{range_pivot}, but the job's pivot is z{pivot}: \
         both come from one shard plan"
    )]
    ShardPivotMismatch {
        /// [`TilesShard::pivot_zoom`](super::tiles::TilesShard::pivot_zoom).
        pivot: u8,
        /// The range's own zoom.
        range_pivot: u8,
    },
    /// `--plan-only` on a data shard (#560): the shard reads only the row
    /// groups its range reaches, so the plan it wrote would cover that subset
    /// and be useless to the rest of the fleet.
    #[error(
        "--plan-only cannot be combined with a data shard (--shard I/N): the shard reads only \
         the row groups its range reaches, so the plan it wrote would cover that subset and be \
         useless to the rest of the fleet. The plan comes from the job that reads everything: \
         `--shard coarse --shard-plan <the fleet's shard plan> --plan-only`"
    )]
    PlanOnlyDataShard,
    /// `--plan-only` without `--save-plan` (#560): there would be nothing to
    /// write.
    #[error("--plan-only requires --save-plan: the plan is the run's only output")]
    PlanOnlyWithoutSavePlan,
    /// `--keep-overview` names a directory that does not exist (#433).
    #[error("--keep-overview directory {} does not exist", dir.display())]
    KeepOverviewDirMissing {
        /// The missing parent directory.
        dir: PathBuf,
    },
    /// `--feature-order` reads a property the convert's selection excludes
    /// (#386): the column would be gone from the intermediate before export
    /// ever saw it.
    #[error(
        "property {name:?} is excluded but --feature-order reads it; keep it in the \
         selection or drop the knob"
    )]
    FeatureOrderColumnExcluded {
        /// The excluded property.
        name: String,
    },
    /// `--feature-id` reads a property the convert's selection excludes
    /// (#443).
    #[error(
        "property {name:?} is excluded but --feature-id reads it; keep it in the \
         selection or drop the flag"
    )]
    FeatureIdColumnExcluded {
        /// The excluded property.
        name: String,
    },
    /// `--feature-id` names an accumulated column: a cluster's aggregated
    /// value identifies no single feature (#443).
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
    /// An option set on a command that would ignore it (#600, #443): the
    /// command derives the value itself, or has no use for it.
    #[error("{option} has no effect on {command}: {reason}")]
    OptionIgnored {
        /// The option, as a Rust field path.
        option: &'static str,
        /// The command or entry point that ignores it.
        command: &'static str,
        /// Why, and what to set instead.
        reason: &'static str,
    },
}

/// An [`ExportOptions`](super::export::ExportOptions) field a command that
/// wraps the export sets itself, for [`check_ignored_export_fields`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportField {
    LayerName,
    MinZoom,
    Properties,
    TileRange,
    ZoomCeiling,
    SpillDir,
    Profile,
    Metadata,
}

impl ExportField {
    /// The field as [`ConfigError::OptionIgnored`] names it.
    fn name(self) -> &'static str {
        match self {
            ExportField::LayerName => "export.layer_name",
            ExportField::MinZoom => "export.min_zoom",
            ExportField::Properties => "export.properties",
            ExportField::TileRange => "export.tile_range",
            ExportField::ZoomCeiling => "export.zoom_ceiling",
            ExportField::SpillDir => "export.spill_dir",
            ExportField::Profile => "export.profile",
            ExportField::Metadata => "export.metadata",
        }
    }

    /// Whether `export` sets the field away from its default.
    fn is_set(self, export: &super::export::ExportOptions) -> bool {
        let defaults = super::export::ExportOptions::default();
        match self {
            ExportField::LayerName => export.layer_name != defaults.layer_name,
            ExportField::MinZoom => export.min_zoom.is_some(),
            ExportField::Properties => export.properties != defaults.properties,
            ExportField::TileRange => export.tile_range.is_some(),
            ExportField::ZoomCeiling => export.zoom_ceiling.is_some(),
            ExportField::SpillDir => export.spill_dir.is_some(),
            ExportField::Profile => export.profile != defaults.profile,
            ExportField::Metadata => export.metadata != defaults.metadata,
        }
    }
}

/// The rule every command that wraps the export shares (#600, #443): an
/// export field the command sets itself must be left unset, or the caller's
/// value would be silently dropped. `ignored` lists the command's fields,
/// each with why and what to set instead.
///
/// # Errors
///
/// [`ConfigError::OptionIgnored`] for the first field of `ignored` that
/// `export` sets.
pub(crate) fn check_ignored_export_fields(
    export: &super::export::ExportOptions,
    command: &'static str,
    ignored: &[(ExportField, &'static str)],
) -> Result<(), ConfigError> {
    match ignored.iter().find(|(field, _)| field.is_set(export)) {
        Some(&(field, reason)) => Err(ConfigError::OptionIgnored {
            option: field.name(),
            command,
            reason,
        }),
        None => Ok(()),
    }
}

/// The output-path preflight every command that writes a file shares.
///
/// #427, #452, #551: an existing directory is refused (the final rename can
/// never replace one), an existing file is refused unless `force`, and a
/// missing or read-only directory is refused — all before any input is read.
///
/// # Errors
///
/// [`ConfigError::OutputIsDirectory`], [`ConfigError::OutputExists`] or
/// [`ConfigError::Output`].
pub fn check_output(output: &Path, kind: OutputKind, force: bool) -> Result<(), ConfigError> {
    if output.is_dir() {
        return Err(ConfigError::OutputIsDirectory {
            path: output.to_path_buf(),
            kind,
        });
    }
    if output.exists() && !force {
        return Err(ConfigError::OutputExists {
            kind,
            path: output.to_path_buf(),
        });
    }
    Ok(match kind {
        OutputKind::ShardPlan => crate::atomic_output::check_writable_in_place(output),
        OutputKind::Pmtiles | OutputKind::GeoParquet => {
            crate::atomic_output::check_writable(output)
        }
    }?)
}

/// `--spill-dir`'s rule (#272, #427), shared by the convert and the export.
///
/// Unset, or an existing directory that accepts new files. The spill opens
/// lazily, so a read-only directory would otherwise fail deep into the run.
///
/// # Errors
///
/// [`ConfigError::SpillDirNotDirectory`] or
/// [`ConfigError::SpillDirNotWritable`].
pub fn check_spill_dir(spill_dir: Option<&Path>) -> Result<(), ConfigError> {
    let Some(dir) = spill_dir else {
        return Ok(());
    };
    if !dir.is_dir() {
        return Err(ConfigError::SpillDirNotDirectory(dir.to_path_buf()));
    }
    crate::atomic_output::probe_directory(dir).map_err(|e| ConfigError::SpillDirNotWritable {
        dir: dir.to_path_buf(),
        reason: e.to_string(),
    })
}

/// Check an explicitly selected pyramid work directory before reading bands.
///
/// # Errors
/// Returns a diagnostic naming `work_dir` for an unavailable directory.
pub fn check_work_dir(work_dir: Option<&Path>) -> Result<(), ConfigError> {
    if let Some(dir) = work_dir {
        crate::atomic_output::probe_directory(dir).map_err(|error| ConfigError::InvalidSpec {
            option: "--work-dir / work_dir",
            message: format!(
                "--work-dir / work_dir {} cannot be used: {error}",
                dir.display()
            ),
        })?;
    }
    Ok(())
}

/// The below-tolerance polygon disposition from the frontends' switches.
///
/// #279, #745: `--collapse` (points), `--collapse-square` (squares) and
/// `--no-collapse` (drop). None set means [`CollapseMode::Auto`]: squares,
/// or points under an entry-zoom ladder.
///
/// # Errors
///
/// [`ConfigError::CollapseConflict`] when more than one is set.
pub fn collapse_mode(
    collapse: bool,
    collapse_square: bool,
    no_collapse: bool,
) -> Result<CollapseMode, ConfigError> {
    match (collapse, collapse_square, no_collapse) {
        (false, false, false) => Ok(CollapseMode::Auto),
        (true, false, false) => Ok(CollapseMode::Point),
        (false, true, false) => Ok(CollapseMode::Square),
        (false, false, true) => Ok(CollapseMode::Drop),
        _ => Err(ConfigError::CollapseConflict),
    }
}

/// The entry-zoom ladder (#364): derived from a column's values
/// (`--magnitude-ladder`, every `ladder_step` zooms), or spelled out as
/// `COLUMN:VALUE=ZOOM,...` (`--entry-zoom`).
///
/// # Errors
///
/// [`ConfigError::EntryZoomConflict`] when both are given, or
/// [`ConfigError::InvalidSpec`] for a malformed `--entry-zoom`.
pub fn entry_zoom(
    magnitude_ladder: Option<&str>,
    ladder_step: u8,
    entry_zoom: Option<&str>,
) -> Result<Option<EntryZoomSpec>, ConfigError> {
    match (magnitude_ladder, entry_zoom) {
        (Some(_), Some(_)) => Err(ConfigError::EntryZoomConflict),
        (Some(column), None) => Ok(Some(EntryZoomSpec {
            column: column.to_string(),
            kind: EntryZoomKind::DenseRank { step: ladder_step },
        })),
        (None, Some(spec)) => {
            parse_entry_zoom(spec)
                .map(Some)
                .map_err(|message| ConfigError::InvalidSpec {
                    option: "--entry-zoom",
                    message,
                })
        }
        (None, None) => Ok(None),
    }
}

/// A categorical class ranking from either spelling.
///
/// The `--class-rank` spec (`COLUMN:VALUE=RANK,...`), or its structured
/// parts (a column, a value → rank map, and an optional rank for unlisted
/// values). The rules every spelling shares are [`ClassRanking::new`]'s.
///
/// # Errors
///
/// [`ConfigError::ClassRankSpelledTwice`], [`ConfigError::ClassRankIncomplete`]
/// or [`ConfigError::InvalidSpec`].
pub fn class_ranking(
    spec: Option<&str>,
    column: Option<String>,
    ranks: Option<BTreeMap<String, f64>>,
    unknown_rank: Option<f64>,
) -> Result<Option<ClassRanking>, ConfigError> {
    let invalid = |message| ConfigError::InvalidSpec {
        option: "--class-rank",
        message,
    };
    match (spec, column, ranks) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => Err(ConfigError::ClassRankSpelledTwice),
        (Some(spec), None, None) => parse_class_rank(spec).map(Some).map_err(invalid),
        (None, None, None) => Ok(None),
        (None, Some(_), None) | (None, None, Some(_)) => Err(ConfigError::ClassRankIncomplete),
        (None, Some(column), Some(ranks)) => {
            ClassRanking::new(column, ranks.into_iter().collect(), unknown_rank)
                .map(Some)
                .map_err(invalid)
        }
    }
}

/// The `--representation` zoom bands (#317), or none.
///
/// # Errors
///
/// [`ConfigError::InvalidSpec`] for a malformed spec. Whether the bands fit
/// the level plan is checked with the rest of the convert options.
pub fn representation(spec: Option<&str>) -> Result<Vec<RepresentationBand>, ConfigError> {
    spec.map_or(Ok(Vec::new()), |spec| {
        parse_representation_spec(spec).map_err(|e| ConfigError::InvalidSpec {
            option: "--representation",
            message: format!("--representation: {e}"),
        })
    })
}

/// Every rank, and an explicit unknown-class rank, is finite (#428): the
/// one rule and message behind [`ClassRanking::new`] (so `--class-rank` and
/// Python's `class_ranks` read the same) and [`check_class_ranking`].
pub(crate) fn check_ranks_finite(
    ranks: &[(String, f64)],
    unknown_rank: Option<f64>,
) -> Result<(), ConfigError> {
    if let Some((class, rank)) = ranks.iter().find(|(_, r)| !r.is_finite()) {
        return Err(ConfigError::ClassRankNotFinite {
            class: Some(class.clone()),
            rank: *rank,
        });
    }
    match unknown_rank.filter(|u| !u.is_finite()) {
        Some(rank) => Err(ConfigError::ClassRankNotFinite { class: None, rank }),
        None => Ok(()),
    }
}

/// A ranking's ranks are finite (#428). [`ClassRanking::new`] already
/// guarantees it; this catches one built as a struct literal, whose fields
/// are public.
pub(crate) fn check_class_ranking(ranking: &ClassRanking) -> Result<(), ConfigError> {
    check_ranks_finite(&ranking.ranks, Some(ranking.unknown_rank))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #427/#551: one output preflight for every command — directory,
    /// existing file without force, missing parent — all before any read.
    #[test]
    fn check_output_refuses_directories_existing_files_and_missing_parents() {
        let dir = tempfile::tempdir().unwrap();
        let err = check_output(dir.path(), OutputKind::Pmtiles, true).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::OutputIsDirectory {
                    kind: OutputKind::Pmtiles,
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err
            .to_string()
            .ends_with("is a directory; the output must be a PMTiles file path"));

        let existing = dir.path().join("out.pmtiles");
        std::fs::write(&existing, b"x").unwrap();
        let err = check_output(&existing, OutputKind::Pmtiles, false).unwrap_err();
        assert!(matches!(err, ConfigError::OutputExists { .. }), "{err:?}");
        assert!(err
            .to_string()
            .ends_with("exists (use --force to overwrite)"));
        check_output(&existing, OutputKind::Pmtiles, true).expect("--force overwrites");

        let orphan = dir.path().join("no/such/dir/out.parquet");
        let err = check_output(&orphan, OutputKind::GeoParquet, false).unwrap_err();
        assert!(matches!(err, ConfigError::Output(_)), "{err:?}");

        check_output(
            &dir.path().join("new.parquet"),
            OutputKind::GeoParquet,
            false,
        )
        .unwrap();
    }

    /// #272/#427: the spill directory must exist.
    #[test]
    fn check_spill_dir_requires_an_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        check_spill_dir(None).unwrap();
        check_spill_dir(Some(dir.path())).unwrap();
        let missing = dir.path().join("nope");
        let err = check_spill_dir(Some(&missing)).unwrap_err();
        assert!(matches!(err, ConfigError::SpillDirNotDirectory(_)));
        assert!(err.to_string().contains("not an existing directory"));
    }

    /// A read-only spill dir is refused before any read, not with
    /// "Permission denied" when the spill first opens deep into the run.
    #[cfg(unix)]
    #[test]
    fn check_spill_dir_refuses_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        // root ignores directory modes: nothing to assert there.
        if std::fs::File::create(ro.join("as-root")).is_ok() {
            return;
        }
        let err = check_spill_dir(Some(&ro)).unwrap_err();
        assert!(
            matches!(err, ConfigError::SpillDirNotWritable { ref dir, .. } if *dir == ro),
            "{err:?}"
        );
        assert!(err.to_string().contains("not writable"), "{err}");
    }

    /// #279/#745: the three switches resolve to one disposition, and any two
    /// together are refused (the Python check, formerly lib.rs-only).
    #[test]
    fn collapse_mode_resolves_one_switch_and_refuses_two() {
        assert_eq!(
            collapse_mode(false, false, false).unwrap(),
            CollapseMode::Auto
        );
        assert_eq!(
            collapse_mode(true, false, false).unwrap(),
            CollapseMode::Point
        );
        assert_eq!(
            collapse_mode(false, true, false).unwrap(),
            CollapseMode::Square
        );
        assert_eq!(
            collapse_mode(false, false, true).unwrap(),
            CollapseMode::Drop
        );
        for flags in [
            (true, true, false),
            (true, false, true),
            (false, true, true),
        ] {
            let err = collapse_mode(flags.0, flags.1, flags.2).unwrap_err();
            assert!(matches!(err, ConfigError::CollapseConflict), "{flags:?}");
        }
    }

    /// #364: a ladder from a column or spelled out, never both (formerly a
    /// clap conflict on the CLI and a separate check in Python).
    #[test]
    fn entry_zoom_takes_one_spelling() {
        assert!(entry_zoom(None, 1, None).unwrap().is_none());
        let ladder = entry_zoom(Some("pop"), 2, None).unwrap().unwrap();
        assert_eq!(ladder.column, "pop");
        assert!(matches!(ladder.kind, EntryZoomKind::DenseRank { step: 2 }));
        let explicit = entry_zoom(None, 1, Some("d:5000=4,1000=6"))
            .unwrap()
            .unwrap();
        assert_eq!(explicit.column, "d");
        let err = entry_zoom(Some("pop"), 1, Some("d:1=4")).unwrap_err();
        assert!(matches!(err, ConfigError::EntryZoomConflict));
        let err = entry_zoom(None, 1, Some("nonsense")).unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::InvalidSpec {
                    option: "--entry-zoom",
                    ..
                }
            ),
            "{err:?}"
        );
    }

    /// #428 (b8fa58a): a NaN rank is refused through every spelling — the
    /// `--class-rank` spec, the structured parts, and a struct literal.
    #[test]
    fn class_ranking_refuses_non_finite_ranks_in_every_spelling() {
        let err = class_ranking(Some("road:a=nan"), None, None, None).unwrap_err();
        assert!(matches!(err, ConfigError::InvalidSpec { .. }), "{err:?}");
        assert!(err.to_string().contains("finite"), "{err}");

        let ranks = BTreeMap::from([("a".to_string(), f64::NAN)]);
        let structured = class_ranking(None, Some("road".into()), Some(ranks), None).unwrap_err();
        // One rule, one message: the CLI string and the Python dict read the
        // same, and match the struct-literal check's typed error.
        assert_eq!(structured.to_string(), err.to_string());
        let typed = ConfigError::ClassRankNotFinite {
            class: Some("a".into()),
            rank: f64::NAN,
        };
        assert_eq!(err.to_string(), typed.to_string());

        let literal = ClassRanking {
            column: "road".into(),
            ranks: vec![("a".into(), f64::NAN)],
            unknown_rank: 0.0,
        };
        let err = check_class_ranking(&literal).unwrap_err();
        assert!(
            matches!(err, ConfigError::ClassRankNotFinite { class: Some(ref c), .. } if c == "a"),
            "{err:?}"
        );
        let literal = ClassRanking {
            column: "road".into(),
            ranks: vec![("a".into(), 1.0)],
            unknown_rank: f64::INFINITY,
        };
        assert!(matches!(
            check_class_ranking(&literal).unwrap_err(),
            ConfigError::ClassRankNotFinite { class: None, .. }
        ));
    }

    /// `--bbox` corners are finite and ordered, as a typed config error so
    /// every frontend classes it with the other pre-read refusals (the CLI
    /// bailed on the order itself before #779).
    #[test]
    fn bbox_must_be_finite_and_ordered() {
        use crate::overview::convert::{ConvertError, ConvertOptions};
        let with = |bbox| ConvertOptions {
            bbox: Some(bbox),
            ..ConvertOptions::default()
        };
        for bbox in [
            [3.0, 0.0, 1.0, 1.0],
            [0.0, 2.0, 1.0, 1.0],
            [f64::NAN, 0.0, 1.0, 1.0],
        ] {
            let err = with(bbox).validate().unwrap_err();
            assert!(
                matches!(err, ConvertError::Config(ConfigError::InvalidBbox { .. })),
                "{bbox:?}: {err:?}"
            );
            assert!(err.is_config());
        }
        with([0.0, 0.0, 1.0, 1.0]).validate().unwrap();
    }

    /// The structured spelling pairs its parts, and the two spellings are
    /// exclusive (the Python rules, formerly lib.rs-only).
    #[test]
    fn class_ranking_pairs_its_parts() {
        let ranks = || Some(BTreeMap::from([("a".to_string(), 2.0)]));
        assert!(class_ranking(None, None, None, None).unwrap().is_none());
        let r = class_ranking(None, Some("road".into()), ranks(), None)
            .unwrap()
            .unwrap();
        assert_eq!(
            r.unknown_rank, 1.0,
            "unlisted classes rank below every listed one"
        );
        assert!(matches!(
            class_ranking(None, Some("road".into()), None, None).unwrap_err(),
            ConfigError::ClassRankIncomplete
        ));
        assert!(matches!(
            class_ranking(None, None, ranks(), None).unwrap_err(),
            ConfigError::ClassRankIncomplete
        ));
        assert!(matches!(
            class_ranking(Some("road:a=1"), Some("road".into()), ranks(), None).unwrap_err(),
            ConfigError::ClassRankSpelledTwice
        ));
        let spec = class_ranking(Some("road:a=2"), None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(spec.column, "road");
    }

    /// Q3: an explicit `--coalesce-lines` in partitioning mode is refused
    /// (formerly a CLI-only bail; Python's `coalesce_lines=True` was
    /// silently inert), while the default stays silently inert.
    #[test]
    fn explicit_coalescing_is_refused_in_partitioning_mode() {
        use crate::overview::convert::{ConvertError, ConvertOptions, GeneralizationOverrides};
        use crate::overview::level::Mode;

        let partitioning = || ConvertOptions {
            mode: Mode::Partitioning,
            ..ConvertOptions::default()
        };
        let asked = GeneralizationOverrides {
            coalesce_lines: Some(true),
            ..GeneralizationOverrides::default()
        };
        let err = partitioning()
            .with_generalization(&asked, false)
            .unwrap_err();
        assert!(
            matches!(
                err,
                ConvertError::Config(ConfigError::CoalescePartitioningUnsupported)
            ),
            "{err:?}"
        );
        assert!(err.is_config());
        partitioning()
            .with_generalization(&GeneralizationOverrides::default(), false)
            .expect("the default is inert, not an error");
    }

    /// Q4 / verbatim: the mode combinations no pipeline can honour are part
    /// of `ConvertOptions::validate`, so they fail before any input is read
    /// (formerly CLI bails and Python checks, and a core check that ran only
    /// after the input's schema was read).
    #[test]
    fn mode_combinations_are_refused_by_validate() {
        use crate::overview::convert::{ConvertError, ConvertOptions};
        use crate::overview::level::Mode;

        let err = ConvertOptions {
            mode: Mode::Partitioning,
            cluster: true,
            ..ConvertOptions::default()
        }
        .validate()
        .unwrap_err();
        assert!(matches!(err, ConvertError::ClusterPartitioningUnsupported));
        let err = ConvertOptions {
            mode: Mode::Partitioning,
            ..ConvertOptions::default().verbatim()
        }
        .validate()
        .unwrap_err();
        assert!(matches!(err, ConvertError::VerbatimPartitioningUnsupported));
    }

    #[test]
    fn representation_names_the_flag() {
        assert!(representation(None).unwrap().is_empty());
        assert_eq!(representation(Some("0-3:point")).unwrap().len(), 1);
        let err = representation(Some("bogus")).unwrap_err();
        assert!(err.to_string().starts_with("--representation: "), "{err}");
    }
}

pub(crate) mod spelled {
    use serde::{de, Deserialize, Deserializer, Serializer};
    use std::fmt::Display;
    use std::str::FromStr;

    pub(crate) fn serialize<S: Serializer, T: Display>(value: &T, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(value)
    }

    pub(crate) fn deserialize<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: Display,
    {
        parse(&String::deserialize(d)?)
    }

    /// [`deserialize`], ignoring case.
    pub(crate) fn deserialize_lowercase<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: Display,
    {
        parse(&String::deserialize(d)?.to_lowercase())
    }

    fn parse<T: FromStr, E: de::Error>(s: &str) -> Result<T, E>
    where
        T::Err: Display,
    {
        s.parse().map_err(E::custom)
    }
}

/// Options which have passed their core validation rules.
///
/// The borrowed options cannot be changed while this proof is alive. There is
/// deliberately no public constructor or mutable accessor.
///
/// ```compile_fail
/// use tylertoo_core::overview::{config::ValidatedConfig, convert::ConvertOptions};
/// let options = ConvertOptions::default();
/// let _config = ValidatedConfig { options: &options };
/// ```
///
/// ```compile_fail
/// use tylertoo_core::overview::convert::ConvertOptions;
/// let mut options = ConvertOptions::default();
/// let config = options.validate().unwrap();
/// options.read_batch_size = 0;
/// assert!(config.options().read_batch_size > 0);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ValidatedConfig<'a, T> {
    options: &'a T,
}

impl<'a, T> ValidatedConfig<'a, T> {
    fn new(options: &'a T) -> Self {
        Self { options }
    }

    /// The validated options, available for reading only.
    pub fn options(&self) -> &'a T {
        self.options
    }
}

pub(crate) mod tile_range {
    use crate::shard::TileRange;
    use serde::{Deserialize, Deserializer, Serializer};

    pub(crate) fn serialize<S: Serializer>(
        value: &Option<TileRange>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(range) => s.serialize_some(&format!("{}..{}", range.lo(), range.hi())),
            None => s.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<TileRange>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|s| TileRange::parse(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

pub(crate) mod accumulation {
    use super::super::cluster::AccumulateSpec;
    use super::super::convert::parse_accumulate;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::collections::BTreeMap;

    pub(crate) fn serialize<S: Serializer>(
        value: &[AccumulateSpec],
        s: S,
    ) -> Result<S::Ok, S::Error> {
        if value.is_empty() {
            return s.serialize_none();
        }
        s.collect_seq(
            value
                .iter()
                .map(|spec| format!("{}:{}", spec.column, spec.op.as_str())),
        )
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Vec<AccumulateSpec>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Specs {
            List(Vec<String>),
            Mapping(BTreeMap<String, String>),
        }
        let specs = match Option::<Specs>::deserialize(d)? {
            Some(Specs::List(specs)) => specs,
            Some(Specs::Mapping(specs)) => specs
                .into_iter()
                .map(|(column, op)| format!("{column}:{op}"))
                .collect(),
            None => Vec::new(),
        };
        specs
            .iter()
            .map(|spec| parse_accumulate(spec).map_err(serde::de::Error::custom))
            .collect()
    }
}

/// Validate convert options before they can reach either engine.
pub(super) fn validate_convert(
    options: &super::convert::ConvertOptions,
) -> Result<ValidatedConfig<'_, super::convert::ConvertOptions>, super::convert::ConvertError> {
    options.check()?;
    Ok(ValidatedConfig::new(options))
}

/// Validate export options before they can reach the export engine.
pub(super) fn validate_export(
    options: &super::export::ExportOptions,
) -> Result<ValidatedConfig<'_, super::export::ExportOptions>, ConfigError> {
    options.check()?;
    Ok(ValidatedConfig::new(options))
}

/// Validate the complete tiles facade configuration.
pub(super) fn validate_tiles(
    options: &super::tiles::TilesOptions,
) -> Result<ValidatedConfig<'_, super::tiles::TilesOptions>, super::tiles::TilesError> {
    options.check()?;
    Ok(ValidatedConfig::new(options))
}

/// Validate a complete pyramid before reading any band.
pub(crate) fn validate_pyramid(
    options: &crate::pyramid::PyramidOptions,
) -> Result<ValidatedConfig<'_, crate::pyramid::PyramidOptions>, crate::Error> {
    options.check()?;
    Ok(ValidatedConfig::new(options))
}

/// A per-tile size cap as a caller states it: `auto` (the command's own core
/// default) or a byte count, `0` for no cap (#761). Python also takes `None`
/// for no cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeCap {
    /// The command's own default cap.
    #[default]
    Auto,
    /// This many bytes; `0` is no cap.
    Bytes(usize),
}

impl SizeCap {
    /// This cap over `default`, the command's own: `Auto` keeps it, `0`
    /// turns the cap off, any other size replaces it.
    pub fn or(self, default: Option<usize>) -> Option<usize> {
        match self {
            SizeCap::Auto => default,
            SizeCap::Bytes(n) => (n > 0).then_some(n),
        }
    }
}

impl Serialize for SizeCap {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            SizeCap::Auto => s.serialize_str("auto"),
            SizeCap::Bytes(n) => s.serialize_u64(*n as u64),
        }
    }
}

impl<'de> Deserialize<'de> for SizeCap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct CapVisitor;
        impl Visitor<'_> for CapVisitor {
            type Value = SizeCap;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("\"auto\", None or a byte count")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<SizeCap, E> {
                if s.trim().eq_ignore_ascii_case("auto") {
                    Ok(SizeCap::Auto)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(s), &self))
                }
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<SizeCap, E> {
                usize::try_from(n)
                    .map(SizeCap::Bytes)
                    .map_err(|_| E::invalid_value(de::Unexpected::Unsigned(n), &self))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<SizeCap, E> {
                u64::try_from(n)
                    .map_err(|_| E::invalid_value(de::Unexpected::Signed(n), &self))
                    .and_then(|n| self.visit_u64(n))
            }
            fn visit_unit<E: de::Error>(self) -> Result<SizeCap, E> {
                Ok(SizeCap::Bytes(0))
            }
            fn visit_none<E: de::Error>(self) -> Result<SizeCap, E> {
                Ok(SizeCap::Bytes(0))
            }
        }
        d.deserialize_any(CapVisitor)
    }
}

#[cfg(feature = "cli")]
pub mod cli;
pub mod keywords;
mod values;

pub(super) fn finite_number(name: &str, value: f64) -> Result<(), String> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(format!("{name} = {value} must be a finite value"))
    }
}
