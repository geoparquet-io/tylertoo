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

use std::collections::{HashMap, HashSet};

use arrow_array::{Array, RecordBatch};

use super::super::config::ConfigError;
use super::super::convert::ConvertOptions;
use super::super::ladder::EntryZoomLadder;
use super::super::level::{Mode, OverviewsMeta};
use super::{
    key_to_xy, lnglat_bbox, size_bboxes, tile_key, ExportError, ExportOptions, LevelScan,
    DEFAULT_EXTENT,
};
use crate::tile::TileBounds;

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

// ============================================================================
// Rule A′: which tiles may stop
// ============================================================================

/// What a band's footer says about its rows' geometry kinds: the point-count
/// check needs to know which rows are points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kinds {
    /// Every row is a point or multipoint: its bbox counts are point counts.
    Points,
    /// No row is: the point checks have nothing to compare.
    NoPoints,
    /// Some may be: each row's kind comes from its decoded geometry.
    Mixed,
}

impl Kinds {
    /// The kinds a GeoParquet `geometry_types` list names. An empty list
    /// names nothing, so every kind may be present.
    fn of(types: &[String]) -> Self {
        let point = |t: &String| {
            let base = t.split_whitespace().next().unwrap_or("");
            base == "Point" || base == "MultiPoint"
        };
        if types.is_empty() {
            Self::Mixed
        } else if types.iter().all(point) {
            Self::Points
        } else if types.iter().any(point) {
            Self::Mixed
        } else {
            Self::NoPoints
        }
    }

    fn may_have_points(self) -> bool {
        self != Self::NoPoints
    }
}

/// The file's geometry kinds, from its `geo` metadata's primary column.
fn file_kinds(geo_json: Option<&str>) -> Kinds {
    let types: Vec<String> = geo_json
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .and_then(|geo| {
            let primary = geo["primary_column"].as_str()?.to_string();
            let types = geo["columns"][primary]["geometry_types"]
                .as_array()?
                .clone();
            Some(
                types
                    .iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect(),
            )
        })
        .unwrap_or_default();
    Kinds::of(&types)
}

/// #794's hook: a rule that releases a canonical row only from some zoom on.
/// A tile that would carry the row before its release cannot stop, so the
/// row's member tile one zoom above its release, and every ancestor of it,
/// is blocked.
pub(crate) trait RowRelease: Sync + Send {
    /// The canonical columns the rule reads, by name.
    fn columns(&self) -> &[String];
    /// Each row's release zoom, row-aligned with `batch`: `0` for a row
    /// present at every zoom. `bboxes` is row-aligned too; a row without one
    /// is never drawn.
    fn release_zooms(&self, batch: &RecordBatch, bboxes: &[Option<TileBounds>]) -> Vec<u8>;
}

/// The first [`RowRelease`]: an entry-zoom ladder (#364, #446) releases a
/// row at its rung's zoom, read from the rungs the footer lists.
struct EntryRungs {
    column: [String; 1],
    ladder: EntryZoomLadder,
}

impl EntryRungs {
    /// The ladder a footer records, or `None` without one. Rungs that do not
    /// parse are an error: the file would otherwise stop tiles early.
    fn from_meta(meta: &OverviewsMeta) -> Result<Option<Self>, ExportError> {
        let Some(entry) = meta
            .generalization
            .as_ref()
            .and_then(|g| g.entry_zoom.as_ref())
        else {
            return Ok(None);
        };
        // `check_file` refused a ladder without its rungs.
        let rungs = entry.rungs.clone().unwrap_or_default();
        let pairs = rungs
            .iter()
            .map(|(value, zoom)| value.parse::<f64>().map(|v| (v, *zoom)))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| {
                ExportError::Config(ConfigError::VariableDepthConflict {
                    conflict: format!("an entry-zoom rung that does not parse ({e})"),
                })
            })?;
        if pairs.is_empty() {
            return Ok(None);
        }
        let ladder = EntryZoomLadder::explicit(pairs).map_err(|e| {
            ExportError::Config(ConfigError::VariableDepthConflict {
                conflict: format!("an entry-zoom ladder that does not rebuild ({e})"),
            })
        })?;
        Ok(Some(Self {
            column: [entry.column.clone()],
            ladder,
        }))
    }
}

impl RowRelease for EntryRungs {
    fn columns(&self) -> &[String] {
        &self.column
    }

    fn release_zooms(&self, batch: &RecordBatch, bboxes: &[Option<TileBounds>]) -> Vec<u8> {
        let values = batch
            .column_by_name(&self.column[0])
            .and_then(|c| arrow_cast::cast(c, &arrow_schema::DataType::Float64).ok());
        let values = values.as_ref().map(|c| {
            c.as_any()
                .downcast_ref::<arrow_array::Float64Array>()
                .expect("cast to Float64")
        });
        (0..batch.num_rows())
            .map(|i| {
                let v = values.and_then(|c| c.is_valid(i).then(|| c.value(i)));
                match bboxes.get(i) {
                    Some(Some(_)) => self.ladder.entry_zoom(v).unwrap_or(0),
                    _ => 0,
                }
            })
            .collect()
    }
}

/// The parent of tile `key` at `zoom`.
fn parent_key(key: u64, zoom: u8) -> u64 {
    let (x, y) = key_to_xy(key, zoom);
    tile_key(x >> 1, y >> 1, zoom - 1)
}

/// Rule A′'s one owner: the tiles that may not stop. A tile is **bad** when
/// a finer level shows it a different point count than the canonical band,
/// or a cluster, or when it carries a canonical row before its
/// [`RowRelease`]. A bad tile and all of its ancestors are blocked.
#[derive(Debug, Default)]
pub(super) struct StopPolicy {
    /// Blocked tile keys, per zoom.
    blocked: Vec<HashSet<u64>>,
}

impl StopPolicy {
    /// Block `key` at `zoom` and every ancestor.
    fn block(&mut self, mut zoom: u8, mut key: u64) {
        if self.blocked.len() <= usize::from(zoom) {
            self.blocked
                .resize_with(usize::from(zoom) + 1, HashSet::new);
        }
        loop {
            if !self.blocked[usize::from(zoom)].insert(key) {
                return; // Its ancestors were blocked with it.
            }
            if zoom == 0 {
                return;
            }
            key = parent_key(key, zoom);
            zoom -= 1;
        }
    }

    /// Whether the rule lets tile `key` at `zoom` stop. The probe's own
    /// conditions (content, coordinate range, both caps) come on top.
    #[cfg_attr(not(test), expect(dead_code, reason = "#803: the probe reads it"))]
    pub(super) fn may_stop(&self, zoom: u8, key: u64) -> bool {
        self.blocked
            .get(usize::from(zoom))
            .is_none_or(|b| !b.contains(&key))
    }
}

/// Sizes a set of rows at a zoom, as the scan does: one member per tile each
/// row's buffered bbox reaches.
fn tile_keys(bboxes: &[Option<TileBounds>], zoom: u8, opts: &ExportOptions) -> HashMap<u64, usize> {
    size_bboxes(bboxes, zoom, opts).1
}

fn add_counts(into: &mut HashMap<u64, usize>, from: HashMap<u64, usize>) {
    for (k, v) in from {
        *into.entry(k).or_insert(0) += v;
    }
}

/// How [`VdScan::fold`] gets a batch's rows one by one when it needs them:
/// row-aligned bboxes (`None` for a null row), or the decoded geometries.
pub(super) struct RowSource<'a> {
    pub(super) bboxes: &'a dyn Fn() -> Result<Vec<Option<TileBounds>>, ExportError>,
    pub(super) geometries: &'a dyn Fn() -> Result<Vec<Option<geo::Geometry<f64>>>, ExportError>,
}

/// The scan's variable-depth side: everything rule A′ needs, gathered in
/// the scan's one read of every band. The canonical band is sized at every
/// level's zoom; the other bands' point rows and clusters are counted at
/// their own.
pub(super) struct VdScan {
    zooms: Vec<u8>,
    canonical: usize,
    kinds: Vec<Kinds>,
    cluster_column: Option<String>,
    release: Option<Box<dyn RowRelease>>,
    /// The canonical band sized at each level's zoom, every row.
    canonical_counts: Vec<HashMap<u64, usize>>,
    /// The canonical band's point rows at each level's zoom, when the band
    /// is mixed.
    canonical_points: Vec<HashMap<u64, usize>>,
    /// Each mixed band's point rows at its own zoom.
    level_points: Vec<HashMap<u64, usize>>,
    /// Bad tiles found while reading: clusters and early releases.
    bad: Vec<(u8, u64)>,
}

/// What [`VdScan::finish`] hands the probe: the policy, and the canonical
/// tiles at each level's zoom.
#[cfg_attr(not(test), expect(dead_code, reason = "#803: the probe reads it"))]
pub(super) struct VdPlan {
    pub(super) policy: StopPolicy,
    /// Canonical members per tile, per level index.
    pub(super) canonical_counts: Vec<HashMap<u64, usize>>,
}

impl VdScan {
    /// The scan state for `meta`'s levels at `zooms`.
    pub(super) fn new(
        meta: &OverviewsMeta,
        geo_json: Option<&str>,
        zooms: Vec<u8>,
    ) -> Result<Self, ExportError> {
        let n = zooms.len();
        let file = file_kinds(geo_json);
        let g = meta.generalization.as_ref();
        let kinds = (0..n)
            .map(|k| {
                g.and_then(|g| g.levels.get(k))
                    .filter(|l| !l.geometry_types.is_empty())
                    .map_or(file, |l| Kinds::of(&l.geometry_types))
            })
            .collect();
        Ok(Self {
            canonical: n - 1,
            kinds,
            cluster_column: g
                .and_then(|g| g.clustering.as_ref())
                .filter(|c| c.enabled)
                .map(|c| c.point_count_column.clone()),
            release: EntryRungs::from_meta(meta)?.map(|r| Box::new(r) as Box<dyn RowRelease>),
            canonical_counts: vec![HashMap::new(); n],
            canonical_points: vec![HashMap::new(); n],
            level_points: vec![HashMap::new(); n],
            bad: Vec::new(),
            zooms,
        })
    }

    /// The same state, emptied: a scan pass that is abandoned and retried
    /// starts over.
    pub(super) fn reset(&mut self) {
        let n = self.zooms.len();
        self.canonical_counts = vec![HashMap::new(); n];
        self.canonical_points = vec![HashMap::new(); n];
        self.level_points = vec![HashMap::new(); n];
        self.bad.clear();
    }

    /// Whether some band's rows need their geometry decoded for their kind.
    pub(super) fn needs_geometry(&self) -> bool {
        self.kinds.contains(&Kinds::Mixed)
    }

    /// The columns the scan must read besides the bbox, by name.
    pub(super) fn columns(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.cluster_column.iter().map(String::as_str).collect();
        if let Some(r) = &self.release {
            out.extend(r.columns().iter().map(String::as_str));
        }
        out
    }

    /// Fold one batch of band `j`. `bboxes` are the scan's own for the batch:
    /// row-aligned unless the batch has null geometries.
    pub(super) fn fold(
        &mut self,
        j: usize,
        batch: &RecordBatch,
        bboxes: &[Option<TileBounds>],
        rows: &RowSource<'_>,
        crs: super::Crs,
        opts: &ExportOptions,
    ) -> Result<(), ExportError> {
        let canonical = j == self.canonical;
        let mixed = self.kinds[j] == Kinds::Mixed;
        let per_row = mixed
            || (!canonical && self.cluster_column.is_some())
            || (canonical && self.release.is_some());
        // Row-aligned bboxes and point flags, decoded only when needed.
        let decoded: Option<(Vec<Option<TileBounds>>, Vec<bool>)> = if mixed {
            let geoms = (rows.geometries)()?;
            Some((
                geoms
                    .iter()
                    .map(|g| {
                        g.as_ref()
                            .and_then(geo::BoundingRect::bounding_rect)
                            .map(|r| lnglat_bbox(r.min().x, r.min().y, r.max().x, r.max().y, crs))
                    })
                    .collect(),
                geoms
                    .iter()
                    .map(|g| {
                        matches!(
                            g,
                            Some(geo::Geometry::Point(_) | geo::Geometry::MultiPoint(_))
                        )
                    })
                    .collect(),
            ))
        } else if per_row && bboxes.len() != batch.num_rows() {
            Some(((rows.bboxes)()?, Vec::new()))
        } else {
            None
        };
        let aligned = decoded.as_ref().map_or(bboxes, |d| d.0.as_slice());
        let points = |flags: &[bool]| -> Vec<Option<TileBounds>> {
            aligned
                .iter()
                .zip(flags)
                .map(|(b, &p)| if p { *b } else { None })
                .collect()
        };
        if canonical {
            for k in 0..self.canonical {
                add_counts(
                    &mut self.canonical_counts[k],
                    tile_keys(bboxes, self.zooms[k], opts),
                );
                if let Some((_, flags)) = decoded.as_ref().filter(|_| mixed) {
                    add_counts(
                        &mut self.canonical_points[k],
                        tile_keys(&points(flags), self.zooms[k], opts),
                    );
                }
            }
            if let Some(release) = &self.release {
                let zooms = release.release_zooms(batch, aligned);
                let mut by_zoom: HashMap<u8, Vec<Option<TileBounds>>> = HashMap::new();
                for (z, b) in zooms.iter().zip(aligned) {
                    if *z > 0 && b.is_some() {
                        by_zoom.entry(*z - 1).or_default().push(*b);
                    }
                }
                for (zoom, rows) in by_zoom {
                    self.bad
                        .extend(tile_keys(&rows, zoom, opts).into_keys().map(|k| (zoom, k)));
                }
            }
            return Ok(());
        }
        let zoom = self.zooms[j];
        if let Some((_, flags)) = decoded.as_ref().filter(|_| mixed) {
            add_counts(
                &mut self.level_points[j],
                tile_keys(&points(flags), zoom, opts),
            );
        }
        if let Some(column) = &self.cluster_column {
            let counts = batch
                .column_by_name(column)
                .and_then(|c| arrow_cast::cast(c, &arrow_schema::DataType::Int64).ok());
            let counts = counts.as_ref().map(|c| {
                c.as_any()
                    .downcast_ref::<arrow_array::Int64Array>()
                    .expect("cast to Int64")
            });
            let clustered: Vec<Option<TileBounds>> = aligned
                .iter()
                .enumerate()
                .filter(|(i, _)| counts.is_some_and(|c| c.is_valid(*i) && c.value(*i) > 1))
                .map(|(_, b)| *b)
                .collect();
            if !clustered.is_empty() {
                self.bad.extend(
                    tile_keys(&clustered, zoom, opts)
                        .into_keys()
                        .map(|k| (zoom, k)),
                );
            }
        }
        Ok(())
    }

    /// Compare every band's point counts with the canonical band's, and
    /// build the policy. `scans` are the scan's own per-level results.
    pub(super) fn finish(mut self, scans: &[LevelScan]) -> VdPlan {
        let mut policy = StopPolicy::default();
        for (zoom, key) in std::mem::take(&mut self.bad) {
            policy.block(zoom, key);
        }
        let canonical_kinds = self.kinds[self.canonical];
        for (k, scan) in scans.iter().enumerate().take(self.canonical) {
            let level_kinds = self.kinds[k];
            if !level_kinds.may_have_points() && !canonical_kinds.may_have_points() {
                continue;
            }
            let all: HashMap<u64, usize> = scan.tile_counts.iter().map(|(&k, &v)| (k, v)).collect();
            let level = match level_kinds {
                Kinds::Points => &all,
                Kinds::Mixed => &self.level_points[k],
                Kinds::NoPoints => &HashMap::new(),
            };
            let empty = HashMap::new();
            let canon = match canonical_kinds {
                Kinds::Points => &self.canonical_counts[k],
                Kinds::Mixed => &self.canonical_points[k],
                Kinds::NoPoints => &empty,
            };
            let zoom = self.zooms[k];
            for key in level.keys().chain(canon.keys()) {
                if level.get(key).copied().unwrap_or(0) != canon.get(key).copied().unwrap_or(0) {
                    policy.block(zoom, *key);
                }
            }
        }
        let mut canonical_counts = self.canonical_counts;
        canonical_counts[self.canonical] = scans[self.canonical]
            .tile_counts
            .iter()
            .map(|(&k, &v)| (k, v))
            .collect();
        VdPlan {
            policy,
            canonical_counts,
        }
    }
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

    // --- rule A′ -------------------------------------------------------------

    use super::super::super::cluster::{AccumulateOp, AccumulateSpec};
    use super::super::super::ladder::{EntryZoomKind, EntryZoomSpec};
    use super::super::Crs;
    use super::super::{resolve_level_zooms, scan_all_levels, tile_key, ExportTimers};
    use super::{VdPlan, VdScan};

    /// Convert `geoms` with an f64 column `v` over z0..=`max_zoom`.
    fn overview_v(
        dir: &Path,
        geoms: &[Geometry<f64>],
        v: &[f64],
        max_zoom: u8,
        convert: ConvertOptions,
    ) -> std::path::PathBuf {
        let src = dir.join("src_v.parquet");
        let geoms: Vec<_> = geoms.iter().cloned().map(Some).collect();
        testutil::write_input_with_f64(&src, &geoms, "v", v);
        let ov = dir.join("ov_v.parquet");
        convert_to_overviews(
            &src,
            &ov,
            &ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom,
                },
                ..convert
            },
        )
        .unwrap();
        ov
    }

    /// Rule A′ for an overview, through the export's own scan.
    fn plan(ov: &Path) -> VdPlan {
        let reader = OverviewReader::open(ov).unwrap();
        let meta = reader.meta().clone();
        let zooms = resolve_level_zooms(&meta, reader.num_levels()).unwrap();
        let mut scan = VdScan::new(&meta, reader.geo_metadata_json(), zooms).unwrap();
        let scans = scan_all_levels(
            &reader,
            Crs::Epsg4326,
            &vd(),
            &ExportTimers::default(),
            Some(&mut scan),
            &crate::diagnostics::Diagnostics::default(),
        )
        .unwrap();
        scan.finish(&scans)
    }

    /// The key of the tile holding `(lng, lat)` at `zoom`.
    fn key_at(lng: f64, lat: f64, zoom: u8) -> u64 {
        let t = crate::tile::lng_lat_to_tile(lng, lat, zoom);
        tile_key(t.x, t.y, zoom)
    }

    fn pt(x: f64, y: f64) -> Geometry<f64> {
        Geometry::Point(Point::new(x, y))
    }

    /// STOP-RULE.md's counterexample: the z6 tile holding `w` shows the same
    /// points as the canonical band, but at z8 `w` absorbs the laddered
    /// `q1`, `q2` (entry z10) that sit just outside the z6 tile's buffer, so
    /// stopping at z6 would serve the wrong cluster and the wrong sums.
    #[test]
    fn finer_cluster_blocks_coarser_tile() {
        let dir = tempfile::tempdir().unwrap();
        let geoms = [
            pt(5.600, 0.5),
            pt(6.040, 0.5),
            pt(5.805, 0.5),
            pt(5.806, 0.5),
            pt(2.000, 3.0),
        ];
        let v = [10.0, 20.0, 100.0, 100.0, 1.0];
        let mut convert = ConvertOptions {
            cluster: true,
            accumulate: vec![AccumulateSpec {
                column: "v".into(),
                op: AccumulateOp::Sum,
            }],
            entry_zoom: Some(EntryZoomSpec {
                column: "v".into(),
                kind: EntryZoomKind::Explicit(vec![(100.0, 10)]),
            }),
            ..ConvertOptions::default()
        };
        convert.density.enabled = false;
        convert.assign.point_thinning = 16.0;
        let ov = overview_v(dir.path(), &geoms, &v, 10, convert);
        let plan = plan(&ov);
        assert!(
            !plan.policy.may_stop(8, key_at(5.600, 0.5, 8)),
            "w's z8 tile"
        );
        assert!(
            !plan.policy.may_stop(6, key_at(5.600, 0.5, 6)),
            "T, two zooms up"
        );
        assert!(
            !plan.policy.may_stop(0, key_at(5.600, 0.5, 0)),
            "and the root"
        );
        // The filler, far from the cluster, has nothing to hide.
        assert!(plan.policy.may_stop(8, key_at(2.0, 3.0, 8)));
    }

    /// A point thinned out of a coarse level blocks its tile there and every
    /// ancestor; an isolated point's tiles stay free.
    #[test]
    fn thinned_point_blocks_ancestors() {
        let dir = tempfile::tempdir().unwrap();
        let mut geoms: Vec<Geometry<f64>> = (0..400)
            .map(|i| pt(10.0 + (i % 20) as f64 * 1e-4, 45.0 + (i / 20) as f64 * 1e-4))
            .collect();
        geoms.push(pt(-70.0, -30.0));
        let v = vec![1.0; geoms.len()];
        let mut convert = ConvertOptions::default();
        convert.density.enabled = false;
        let ov = overview_v(dir.path(), &geoms, &v, 8, convert);
        let plan = plan(&ov);
        for z in 0..8 {
            assert!(
                !plan.policy.may_stop(z, key_at(10.0, 45.0, z)),
                "dense z{z}"
            );
        }
        for z in 1..8 {
            assert!(
                plan.policy.may_stop(z, key_at(-70.0, -30.0, z)),
                "isolated z{z}"
            );
        }
        assert!(
            !plan.policy.may_stop(0, key_at(-70.0, -30.0, 0)),
            "the root holds both"
        );
    }

    fn square(x: f64, y: f64, side: f64) -> Geometry<f64> {
        Geometry::Polygon(geo::Polygon::new(
            geo::LineString::from(vec![
                (x, y),
                (x + side, y),
                (x + side, y + side),
                (x, y + side),
                (x, y),
            ]),
            vec![],
        ))
    }

    /// The visibility gate drops tiny polygons from the coarse levels. That
    /// is a resolution loss, which never blocks a stop.
    #[test]
    fn gate_only_polygon_loss_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut geoms: Vec<Geometry<f64>> = (0..100)
            .map(|i| {
                square(
                    10.0 + (i % 10) as f64 * 1e-3,
                    45.0 + (i / 10) as f64 * 1e-3,
                    2e-4,
                )
            })
            .collect();
        // One polygon every level keeps, so the coarse levels exist.
        geoms.push(square(9.0, 44.0, 3.0));
        let v = vec![1.0; geoms.len()];
        let ov = overview_v(dir.path(), &geoms, &v, 8, ConvertOptions::default());
        let reader = OverviewReader::open(&ov).unwrap();
        let first = reader.meta().levels.len();
        assert!(first > 1);
        let plan = plan(&ov);
        let zooms = resolve_level_zooms(reader.meta(), first).unwrap();
        for (k, counts) in plan.canonical_counts.iter().enumerate() {
            for key in counts.keys() {
                assert!(plan.policy.may_stop(zooms[k], *key), "z{} {key}", zooms[k]);
            }
        }
    }

    /// A laddered polygon is released at its rung's zoom: every tile that
    /// would show it earlier is blocked, and from its rung on it blocks
    /// nothing.
    #[test]
    fn laddered_polygon_blocks_until_entry() {
        let dir = tempfile::tempdir().unwrap();
        let geoms = [square(10.0, 45.0, 0.5), square(-70.0, -30.0, 0.5)];
        let v = [5.0, 1.0];
        let convert = ConvertOptions {
            entry_zoom: Some(EntryZoomSpec {
                column: "v".into(),
                kind: EntryZoomKind::Explicit(vec![(5.0, 4)]),
            }),
            ..ConvertOptions::default()
        };
        let ov = overview_v(dir.path(), &geoms, &v, 6, convert);
        let plan = plan(&ov);
        for z in 0..4 {
            assert!(!plan.policy.may_stop(z, key_at(10.2, 45.2, z)), "z{z}");
        }
        for z in 4..=6 {
            assert!(plan.policy.may_stop(z, key_at(10.2, 45.2, z)), "z{z}");
        }
        for z in 1..=6 {
            assert!(
                plan.policy.may_stop(z, key_at(-69.8, -29.8, z)),
                "unladdered z{z}"
            );
        }
    }
}
