//! #803: the variable-depth tile pyramid, tippecanoe's
//! `--generate-variable-depth-tile-pyramid`.
//!
//! A branch of the pyramid ends at the coarsest zoom `z` whose tile, encoded
//! from the canonical band at extent `4096 << (M - z)`, fits both byte caps
//! and loses nothing the canonical band holds. The tile is written in place
//! of the ordinary level-`z` tile and its descendants are not exported.
//!
//! This module owns the refusals and the stop rule. The options it cannot
//! honour are refused before any output is written.

use super::super::config::ConfigError;
use super::super::convert::ConvertOptions;
use super::super::level::{Mode, OverviewsMeta};
use super::{ExportOptions, DEFAULT_EXTENT};

/// The refusals that need no file: the export options alone.
///
/// # Errors
///
/// [`ConfigError::VariableDepthConflict`] naming the first conflict.
pub(crate) fn check_options(options: &ExportOptions) -> Result<(), ConfigError> {
    if !options.variable_depth {
        return Ok(());
    }
    let conflict = if options.extent != DEFAULT_EXTENT {
        Some(format!(
            "--extent {} (the base extent must be {DEFAULT_EXTENT})",
            options.extent
        ))
    } else if options.coverage_fill {
        Some("--coverage-fill".to_string())
    } else if options.tile_range.is_some() {
        Some("--tile-range (a sharded build)".to_string())
    } else if options.zoom_ceiling.is_some() {
        Some("--zoom-ceiling (a sharded build)".to_string())
    } else {
        None
    };
    conflict.map_or(Ok(()), |conflict| {
        Err(ConfigError::VariableDepthConflict { conflict })
    })
}

/// The refusals the `tiles` facade checks against its convert options,
/// before anything is read. Lines and polygons are never count-checked, so
/// convert options that drop whole lines or polygons at coarse levels would
/// let a coarse tile stop while hiding features its finer zooms show.
///
/// # Errors
///
/// [`ConfigError::VariableDepthConflict`] naming the first conflict.
pub(crate) fn check_convert(convert: &ConvertOptions, sharded: bool) -> Result<(), ConfigError> {
    let defaults = super::super::assign::AssignConfig::default();
    let conflict = if sharded {
        Some("--shard (a sharded build)")
    } else if convert.mode == Mode::Partitioning {
        Some("--mode partitioning")
    } else if convert.assign.polygon_thinning > 0.0 {
        Some("--polygon-thinning above 0")
    } else if convert.assign.line_thinning != defaults.line_thinning {
        Some("a non-default --line-thinning")
    } else if convert.density.enabled && convert.density.kinds.lines {
        Some("--drop-lines")
    } else if convert.density.enabled && convert.density.kinds.polygons {
        Some("--drop-polygons")
    } else {
        None
    };
    conflict.map_or(Ok(()), |conflict| {
        Err(ConfigError::VariableDepthConflict {
            conflict: conflict.to_string(),
        })
    })
}

/// The refusals an overview file's footer reveals: checked once it is open,
/// before the archive is created.
///
/// # Errors
///
/// [`ConfigError::VariableDepthConflict`] naming the first conflict.
pub(crate) fn check_file(meta: &OverviewsMeta, mode: Mode) -> Result<(), ConfigError> {
    let g = meta.generalization.as_ref();
    let unlisted_rungs = g
        .and_then(|g| g.entry_zoom.as_ref())
        .filter(|e| e.rungs.is_none());
    let conflict = if mode == Mode::Partitioning {
        Some("a partitioning-mode overview".to_string())
    } else if let Some(z) = g.and_then(|g| g.zoom_ceiling) {
        Some(format!(
            "a partial overview (written under a zoom ceiling of {z})"
        ))
    } else if g.is_some_and(|g| g.levels.iter().any(|l| l.thinning_factor > 0.0)) {
        Some("an overview built with --polygon-thinning above 0".to_string())
    } else {
        unlisted_rungs.map(|e| {
            format!(
                "an entry-zoom ladder whose {} rungs the file does not list (at most 64)",
                e.rung_count
            )
        })
    };
    conflict.map_or(Ok(()), |conflict| {
        Err(ConfigError::VariableDepthConflict { conflict })
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use geo::{Geometry, Point};

    use super::super::super::config::ConfigError;
    use super::super::super::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
    use super::super::super::level::{EntryZoomProvenance, Mode};
    use super::super::super::reader::OverviewReader;
    use super::super::super::testutil;
    use super::super::super::tiles::{TilesError, TilesOptions};
    use super::super::{export_pmtiles, ExportError, ExportOptions};

    /// A small grid of points near (10°E, 45°N).
    fn points(n: usize) -> Vec<Option<Geometry<f64>>> {
        (0..n)
            .map(|i| {
                let (r, c) = (i / 16, i % 16);
                Some(Geometry::Point(Point::new(
                    10.0 + c as f64 * 0.01,
                    45.0 + r as f64 * 0.01,
                )))
            })
            .collect()
    }

    /// Convert `n` points to an overview at `path` with `convert`'s options
    /// over z0..=6.
    fn overview(dir: &Path, n: usize, convert: ConvertOptions) -> std::path::PathBuf {
        let src = dir.join("src.parquet");
        testutil::write_input(&src, &points(n), true, None);
        let ov = dir.join("ov.parquet");
        convert_to_overviews(
            &src,
            &ov,
            &ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 6,
                },
                ..convert
            },
        )
        .unwrap();
        ov
    }

    fn vd() -> ExportOptions {
        ExportOptions {
            variable_depth: true,
            ..ExportOptions::default()
        }
    }

    fn assert_conflict(result: Result<impl std::fmt::Debug, ExportError>, needle: &str) {
        match result {
            Err(ExportError::Config(ConfigError::VariableDepthConflict { conflict })) => {
                assert!(conflict.contains(needle), "{conflict:?} lacks {needle:?}");
            }
            other => panic!("expected a variable-depth conflict naming {needle}, got {other:?}"),
        }
    }

    #[test]
    fn option_conflicts_fail_before_output() {
        let dir = tempfile::tempdir().unwrap();
        let ov = overview(dir.path(), 64, ConvertOptions::default());
        let cases: Vec<(ExportOptions, &str)> = vec![
            (
                ExportOptions {
                    extent: 8192,
                    ..vd()
                },
                "--extent 8192",
            ),
            (
                ExportOptions {
                    coverage_fill: true,
                    ..vd()
                },
                "--coverage-fill",
            ),
            (
                ExportOptions {
                    tile_range: Some(crate::shard::TileRange::parse("1..4").unwrap()),
                    ..vd()
                },
                "--tile-range",
            ),
            (
                ExportOptions {
                    zoom_ceiling: Some(3),
                    ..vd()
                },
                "--zoom-ceiling",
            ),
        ];
        for (options, needle) in cases {
            let out = dir.path().join("out.pmtiles");
            assert_conflict(export_pmtiles(&ov, &out, &options), needle);
            assert!(!out.exists(), "{needle}: nothing may be written");
        }
    }

    #[test]
    fn file_conflicts_fail_before_output() {
        let dir = tempfile::tempdir().unwrap();
        for (convert, needle) in [
            (
                ConvertOptions {
                    mode: Mode::Partitioning,
                    ..ConvertOptions::default()
                },
                "partitioning",
            ),
            (
                ConvertOptions {
                    zoom_ceiling: Some(3),
                    ..ConvertOptions::default()
                },
                "partial overview",
            ),
        ] {
            let ov = overview(dir.path(), 64, convert);
            let out = dir.path().join("out.pmtiles");
            assert_conflict(export_pmtiles(&ov, &out, &vd()), needle);
            assert!(!out.exists(), "{needle}: nothing may be written");
        }
    }

    #[test]
    fn footer_thinning_and_unlisted_rungs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ov = overview(dir.path(), 64, ConvertOptions::default());
        let meta = OverviewReader::open(&ov).unwrap().meta().clone();
        super::check_file(&meta, Mode::Duplicating).expect("a default overview is accepted");

        let mut thinned = meta.clone();
        thinned.generalization.as_mut().unwrap().levels[0].thinning_factor = 1.0;
        let err = super::check_file(&thinned, Mode::Duplicating).unwrap_err();
        assert!(err.to_string().contains("--polygon-thinning"), "{err}");

        let mut laddered = meta;
        laddered.generalization.as_mut().unwrap().entry_zoom = Some(EntryZoomProvenance {
            mode: "dense-rank".into(),
            column: "v".into(),
            step: Some(1),
            base_zoom: Some(0),
            max_zoom: Some(6),
            rung_count: 65,
            rungs: None,
        });
        let err = super::check_file(&laddered, Mode::Duplicating).unwrap_err();
        assert!(err.to_string().contains("65 rungs"), "{err}");
    }

    #[test]
    fn tiles_refuses_whole_feature_drops_of_lines_and_polygons() {
        let base = || {
            let mut t = TilesOptions::default();
            t.export.variable_depth = true;
            t
        };
        base().validate().expect("the defaults are accepted");
        type Set = fn(&mut TilesOptions);
        let cases: Vec<(Set, &str)> = vec![
            (
                |t| t.convert.assign.polygon_thinning = 1.0,
                "--polygon-thinning",
            ),
            (|t| t.convert.assign.line_thinning = 2.0, "--line-thinning"),
            (|t| t.convert.density.kinds.lines = true, "--drop-lines"),
            (
                |t| t.convert.density.kinds.polygons = true,
                "--drop-polygons",
            ),
            (|t| t.export.coverage_fill = true, "--coverage-fill"),
        ];
        for (set, needle) in cases {
            let mut t = base();
            set(&mut t);
            let Err(err) = t.validate() else {
                panic!("{needle} must be refused")
            };
            assert!(
                matches!(
                    &err,
                    TilesError::Config(ConfigError::VariableDepthConflict { conflict })
                        if conflict.contains(needle)
                ),
                "{needle}: {err:?}"
            );
        }
    }

    #[test]
    fn pyramid_refuses_variable_depth() {
        let mut p = crate::pyramid::PyramidOptions::default();
        p.export.variable_depth = true;
        let Err(err) = p.validate() else {
            panic!("refused")
        };
        assert!(err.to_string().contains("pyramid bands"), "{err}");
    }
}
