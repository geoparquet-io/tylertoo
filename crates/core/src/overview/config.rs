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
//!   [`representation`]: the flag-shaped inputs both frontends translate
//!   into [`ConvertOptions`](super::convert::ConvertOptions) fields.
//!
//! The option structs validate themselves before any input is read:
//! [`ConvertOptions::validate`](super::convert::ConvertOptions::validate),
//! [`ExportOptions::validate`](super::export::ExportOptions::validate),
//! [`TilesOptions::validate`](super::tiles::TilesOptions::validate) and
//! [`PyramidOptions::validate`](crate::pyramid::PyramidOptions::validate).

use std::collections::BTreeMap;
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
}

impl std::fmt::Display for OutputKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OutputKind::Pmtiles => "PMTiles",
            OutputKind::GeoParquet => "GeoParquet",
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
    #[error("{} exists (use --force to overwrite)", path.display())]
    OutputExists {
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
    /// `extent` is zero (#433). Zero quantizes every coordinate to `(0, 0)`:
    /// every polygon and line degenerates, and every consumer then divides
    /// by the layer's `extent: 0`.
    #[error(
        "extent {extent} is not a valid MVT tile extent: the extent is the tile-local \
         coordinate resolution and must be positive (the default is 4096)"
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
         belongs to O(buffer²) tiles. Lower --tile-buffer to {max} or less (the default is 8)."
    )]
    TileBufferTooWide {
        /// The requested buffer, in tile pixels.
        buffer: u32,
        /// [`MAX_TILE_BUFFER_PX`](super::export::MAX_TILE_BUFFER_PX).
        max: u32,
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
        class.as_deref().map_or_else(|| "unlisted classes".to_string(), |c| format!("class {c:?}"))
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
    /// A hand-written export tile range alongside a data shard's own (#498).
    /// The shard's range prunes the convert's reads too, so the hand-written
    /// one could only be silently ignored.
    #[error(
        "--tile-range cannot be combined with a data shard (--shard I/N): the shard's own range \
         restricts the export"
    )]
    TileRangeWithDataShard,
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
            path: output.to_path_buf(),
        });
    }
    Ok(crate::atomic_output::check_writable(output)?)
}

/// `--spill-dir`'s rule (#272, #427), shared by the convert and the export:
/// unset, or an existing directory.
///
/// # Errors
///
/// [`ConfigError::SpillDirNotDirectory`].
pub fn check_spill_dir(spill_dir: Option<&Path>) -> Result<(), ConfigError> {
    match spill_dir {
        Some(dir) if !dir.is_dir() => Err(ConfigError::SpillDirNotDirectory(dir.to_path_buf())),
        _ => Ok(()),
    }
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
                .map_err(|e| invalid(format!("class_ranks: {e}")))
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

/// A ranking's ranks are finite (#428). [`ClassRanking::new`] already
/// guarantees it; this catches one built as a struct literal.
pub(crate) fn check_class_ranking(ranking: &ClassRanking) -> Result<(), ConfigError> {
    if let Some((class, rank)) = ranking.ranks.iter().find(|(_, r)| !r.is_finite()) {
        return Err(ConfigError::ClassRankNotFinite {
            class: Some(class.clone()),
            rank: *rank,
        });
    }
    if !ranking.unknown_rank.is_finite() {
        return Err(ConfigError::ClassRankNotFinite {
            class: None,
            rank: ranking.unknown_rank,
        });
    }
    Ok(())
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
        let err = class_ranking(None, Some("road".into()), Some(ranks), None).unwrap_err();
        assert!(err.to_string().contains("finite"), "{err}");

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
