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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use arrow_array::{Array, RecordBatch};
use rayon::prelude::*;

use super::super::config::ConfigError;
use super::super::convert::ConvertOptions;
use super::super::ladder::EntryZoomLadder;
use super::super::level::{Mode, OverviewsMeta};
use super::super::reader::OverviewReader;
use super::{
    account_sources, buffer_deg_at_zoom, build_mvt_within, collect_wave, encoded_tile,
    export_level_groups, for_each_member_key, key_to_xy, lnglat_bbox, near_world_edge, plan_level,
    plan_level_groups, plan_levels, size_bboxes, sort_tile_for_encode, tile_fit, tile_key,
    tiles_in_ranges, CheckpointCtx, Crs, EncodedTile, ExportError, ExportLevelCtx, ExportOptions,
    ExportProgress, ExportTimers, FillTally, LevelCtx, LevelMemory, LevelScan, Member,
    PartitionBudget, PublishedNames, ResolvedFeatureId, StreamingPmtilesWriter, TileMvt,
    ZoomReport, DEFAULT_EXTENT,
};
use crate::compression::TileGzipCache;
use crate::tile::{max_tile_index, BboxTileRanges, TileBounds, TileCoord, MAX_LATITUDE};

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

/// Rows per task in [`member_runs`], each sized at every zoom.
const RUN_ROWS: usize = 4096;

/// Sizes rows at every zoom in `zooms` in one parallel pass, with the same
/// members as [`tile_keys`]: per task, per zoom, `(key, members)` runs.
/// Rows come in spatial order, so most consecutive members share a tile at
/// a coarse zoom and fold into one run instead of one hash update each.
fn member_runs(
    bboxes: &[TileBounds],
    zooms: &[u8],
    opts: &ExportOptions,
) -> Vec<Vec<Vec<(u64, usize)>>> {
    let widest = zooms
        .iter()
        .map(|&z| buffer_deg_at_zoom(z, opts))
        .fold(0.0, f64::max);
    bboxes
        .par_chunks(RUN_ROWS)
        .map(|chunk| {
            let rows: Vec<(TileBounds, RowY)> =
                chunk.iter().map(|b| (*b, RowY::new(b, widest))).collect();
            zooms
                .iter()
                .map(|&zoom| {
                    let mut runs: Vec<(u64, usize)> = Vec::new();
                    for (bbox, row) in &rows {
                        row_member_keys(bbox, row, zoom, opts, |key| match runs.last_mut() {
                            Some((k, n)) if *k == key => *n += 1,
                            _ => runs.push((key, 1)),
                        });
                    }
                    runs
                })
                .collect()
        })
        .collect()
}

/// Web Mercator's normalized row (0 north, 1 south) of `lat`, computed
/// exactly as [`crate::tile::lng_lat_to_tile`] computes it before scaling.
fn mercator_v(lat: f64) -> f64 {
    let lat_rad = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    (1.0 - lat_rad.tan().asinh() / std::f64::consts::PI) / 2.0
}

/// A row's Mercator rows at its own latitude edges, and a bound on how far
/// any buffer up to `widest` degrees can move them: enough to place its
/// buffered edges at most zooms without a `tan` per zoom.
struct RowY {
    top: f64,
    bottom: f64,
    /// Max `|dv/dlat|` per degree over the bbox grown by `widest`.
    slope: f64,
}

impl RowY {
    fn new(bbox: &TileBounds, widest: f64) -> Self {
        let phi = (bbox.lat_min.abs().max(bbox.lat_max.abs()) + widest).min(MAX_LATITUDE);
        Self {
            top: mercator_v(bbox.lat_max),
            bottom: mercator_v(bbox.lat_min),
            // dv/dlat = 1 / (360 cos lat), growing with |lat|; 1.001 covers
            // the rounding of `cos`.
            slope: 1.001 / (360.0 * phi.to_radians().cos()),
        }
    }
}

/// [`for_each_member_key`] for one row at `zoom`, the same keys in the same
/// order, with its buffered latitude edges placed from `row` when the
/// buffer cannot move them across a tile row. The row index is monotone in
/// the normalized row, so when both ends of the interval the edge can lie
/// in floor to the same row, the exact edge does too; otherwise, and for
/// any bbox near or across the antimeridian, it takes the exact path.
#[inline]
fn row_member_keys(
    bbox: &TileBounds,
    row: &RowY,
    zoom: u8,
    opts: &ExportOptions,
    mut f: impl FnMut(u64),
) {
    let buffer = buffer_deg_at_zoom(zoom, opts);
    if bbox.lng_min > bbox.lng_max || near_world_edge(bbox, buffer) {
        return for_each_member_key(bbox, zoom, opts, f);
    }
    let n = 2_f64.powi(i32::from(zoom));
    let max = max_tile_index(zoom);
    let index = |v: f64| ((v * n).floor() as u32).min(max);
    let one = |lo: f64, hi: f64| (index(lo) == index(hi)).then(|| index(lo));
    // Float slack on the exact path's own rounding of the row.
    const EPS: f64 = 1e-12;
    let reach = buffer * row.slope + EPS;
    let (Some(y0), Some(y1)) = (
        one(row.top - reach, row.top + EPS),
        one(row.bottom - EPS, row.bottom + reach),
    ) else {
        return for_each_member_key(bbox, zoom, opts, f);
    };
    // Off the world edge the buffered longitudes need no clamp.
    let x = |lng: f64| ((((lng + 180.0) / 360.0) * n).floor() as u32).min(max);
    let ranges = BboxTileRanges {
        y: (y0, y1),
        x: (x(bbox.lng_min - buffer), x(bbox.lng_max + buffer)),
        x2: None,
    };
    for tc in tiles_in_ranges(&ranges, zoom) {
        f(tile_key(tc.x, tc.y, zoom));
    }
}

/// Adds [`member_runs`]' runs into per-zoom counts, one task per zoom.
fn add_runs(into: &mut [HashMap<u64, usize>], runs: Vec<Vec<Vec<(u64, usize)>>>) {
    into.par_iter_mut().enumerate().for_each(|(z, counts)| {
        for (key, n) in runs.iter().filter_map(|task| task.get(z)).flatten() {
            *counts.entry(*key).or_insert(0) += n;
        }
    });
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
    /// The first level the probe can stop a tile at: coarser ones would
    /// clamp ([`candidate_extent`]), so they are neither sized nor checked.
    first_probed: usize,
    kinds: Vec<Kinds>,
    cluster_column: Option<String>,
    release: Option<Box<dyn RowRelease>>,
    /// The canonical band sized at each level's zoom, every row.
    canonical_counts: Vec<HashMap<u64, usize>>,
    /// The canonical band's point rows at each level's zoom, when the band
    /// is mixed.
    canonical_points: Vec<HashMap<u64, usize>>,
    /// The canonical band's bboxes, and its point rows' when it is mixed,
    /// until `finish` sizes them.
    canonical_rows: Vec<TileBounds>,
    canonical_point_rows: Vec<TileBounds>,
    /// Each mixed band's point rows at its own zoom.
    level_points: Vec<HashMap<u64, usize>>,
    /// Bad tiles found while reading: clusters and early releases.
    bad: Vec<(u8, u64)>,
}

/// What [`VdScan::finish`] hands the probe: the policy, and the canonical
/// tiles at each level's zoom.
pub(super) struct VdPlan {
    pub(super) policy: StopPolicy,
    /// Canonical members per tile, per level index.
    pub(super) canonical_counts: Vec<HashMap<u64, usize>>,
    /// Each level's zoom, and the canonical (finest) level's index.
    zooms: Vec<u8>,
    canonical: usize,
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
            first_probed: zooms[..n - 1]
                .iter()
                .rposition(|&z| candidate_extent(z, zooms[n - 1]).is_none())
                .map_or(0, |i| i + 1),
            kinds,
            cluster_column: g
                .and_then(|g| g.clustering.as_ref())
                .filter(|c| c.enabled)
                .map(|c| c.point_count_column.clone()),
            release: EntryRungs::from_meta(meta)?.map(|r| Box::new(r) as Box<dyn RowRelease>),
            canonical_counts: vec![HashMap::new(); n],
            canonical_points: vec![HashMap::new(); n],
            canonical_rows: Vec::new(),
            canonical_point_rows: Vec::new(),
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
        self.canonical_rows = Vec::new();
        self.canonical_point_rows = Vec::new();
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
        crs: Crs,
        opts: &ExportOptions,
    ) -> Result<(), ExportError> {
        if j < self.first_probed {
            return Ok(()); // Its tiles never stop, so nothing it shows blocks.
        }
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
            // Sized at every coarser zoom in `finish`, in one pass: per
            // batch, the fork-join costs more than the sizing.
            self.canonical_rows.extend(bboxes.iter().flatten());
            if let Some((_, flags)) = decoded.as_ref().filter(|_| mixed) {
                self.canonical_point_rows
                    .extend(points(flags).into_iter().flatten());
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
    pub(super) fn finish(mut self, scans: &[LevelScan], opts: &ExportOptions) -> VdPlan {
        let first = self.first_probed;
        let coarser = &self.zooms[first..self.canonical];
        add_runs(
            &mut self.canonical_counts[first..],
            member_runs(&std::mem::take(&mut self.canonical_rows), coarser, opts),
        );
        add_runs(
            &mut self.canonical_points[first..],
            member_runs(
                &std::mem::take(&mut self.canonical_point_rows),
                coarser,
                opts,
            ),
        );
        let mut policy = StopPolicy::default();
        for (zoom, key) in std::mem::take(&mut self.bad) {
            policy.block(zoom, key);
        }
        let canonical_kinds = self.kinds[self.canonical];
        for (k, scan) in scans
            .iter()
            .enumerate()
            .take(self.canonical)
            .skip(self.first_probed)
        {
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
            zooms: self.zooms,
            canonical: self.canonical,
        }
    }
}

// ============================================================================
// The probe: which tiles do stop
// ============================================================================

/// What the probe reads with: the export's own inputs.
pub(super) struct Probe<'a> {
    pub(super) reader: &'a OverviewReader,
    pub(super) crs: Crs,
    pub(super) published: &'a PublishedNames,
    pub(super) feature_id: Option<&'a ResolvedFeatureId>,
    pub(super) options: &'a ExportOptions,
    pub(super) budget: &'a PartitionBudget,
    pub(super) timers: &'a ExportTimers,
}

/// The stopped tiles' keys, per level.
pub(super) struct Frontier {
    stopped: Vec<HashSet<u64>>,
}

/// The fewest bytes one member can add to a tile: a point feature with an
/// id and no tags (field keys and lengths, type, a move-to of one point).
/// A member that encodes to nothing adds none, so a tile rejected on this
/// bound could rarely have fit; rejecting is always safe, the tile is then
/// drawn at the usual depth.
const MIN_FEATURE_BYTES: usize = 11;

impl VdPlan {
    /// The probe's candidates at level `k`, given the stops so far: tiles
    /// rule A′ lets stop, not under a stop, not over the raw cap on their
    /// member count alone, and inside the coordinate clamp at the level's
    /// candidate extent. `None` when the level has none.
    fn candidates(
        &self,
        k: usize,
        options: &ExportOptions,
        frontier: &Frontier,
    ) -> Option<(ExportOptions, BTreeMap<u64, usize>)> {
        let zoom = self.zooms[k];
        let extent = candidate_extent(zoom, self.zooms[self.canonical])?;
        let opts = ExportOptions {
            extent,
            ..options.clone()
        };
        let raw_cap = opts.tile_size_limit.filter(|&l| l > 0);
        let candidates: BTreeMap<u64, usize> = self.canonical_counts[k]
            .iter()
            .filter(|&(&key, &n)| {
                self.policy.may_stop(zoom, key)
                    && !frontier.covers(&self.zooms, zoom, key)
                    && raw_cap.is_none_or(|cap| n.saturating_mul(MIN_FEATURE_BYTES) <= cap)
                    && within_clamp(key, zoom, &opts)
            })
            .map(|(&key, &n)| (key, n))
            .collect();
        (!candidates.is_empty()).then_some((opts, candidates))
    }

    /// The first level with a candidate. No tile of a coarser level can
    /// stop or sit under a stop, so those levels can be drawn while the
    /// probe runs.
    pub(super) fn first_probed_level(&self, options: &ExportOptions) -> usize {
        let none = Frontier::new(self.zooms.len());
        (0..self.canonical)
            .find(|&k| self.candidates(k, options, &none).is_some())
            .unwrap_or(self.canonical)
    }

    /// The canonical (finest) level's index.
    pub(super) fn canonical(&self) -> usize {
        self.canonical
    }

    /// Probe every candidate tile, coarse to fine: a tile stops when the
    /// canonical band's members under it, encoded at the canonical level's
    /// precision (extent `4096 << (M - z)`), fit both size caps without the
    /// valve. A stopped tile's descendants are not probed. `each` gets every
    /// level coarser than the canonical one, in order, as soon as it is
    /// probed; the probe stops early, with no error, when it returns false.
    ///
    /// DIVERGENCE FROM TIPPECANOE: tippecanoe probes on its fixed world-30
    /// grid; this uses the canonical level's grid (world `M + 12` bits), so
    /// a stopped tile draws exactly what the canonical tiles would.
    pub(super) fn probe(
        self,
        p: &Probe<'_>,
        diagnostics: &crate::diagnostics::Diagnostics,
        mut each: impl FnMut(Probed) -> bool,
    ) -> Result<usize, ExportError> {
        let mut frontier = Frontier::new(self.zooms.len());
        // Rows the probe reads are read again by the export proper, so
        // their decode and clip warnings are counted there, not here.
        let quiet = crate::diagnostics::Diagnostics::silent();
        let gzip_cache = Arc::new(TileGzipCache::new(
            crate::compression::tile_gzip::PAYLOAD_SLICE_BYTES,
        ));
        #[cfg(test)]
        let work = std::sync::Mutex::default();
        // The probe's members are held beside the levels' own waves, which
        // the budget already sizes: a quarter of their width keeps the peak
        // where the levels alone put it.
        let budget = PartitionBudget {
            ceiling_wave: (p.budget.ceiling_wave / PROBE_WAVE_SHARE).max(1),
            ..*p.budget
        };
        let (mut probed, mut stops) = (0usize, 0usize);
        for k in 0..self.canonical {
            let zoom = self.zooms[k];
            let mut level = Probed {
                level: k,
                stopped: HashSet::new(),
                terminals: Vec::new(),
            };
            if let Some((opts, candidates)) = self.candidates(k, p.options, &frontier) {
                probed += candidates.len();
                let plan = plan_level(&candidates, zoom, &budget, || {
                    format!("variable-depth probe z{zoom}")
                });
                let ctx = LevelCtx {
                    reader: p.reader,
                    level_idx: self.canonical,
                    crs: p.crs,
                    zoom,
                    opts: &opts,
                    gzip_cache: &gzip_cache,
                    published: p.published,
                    feature_id: p.feature_id,
                    timers: p.timers,
                    fills: None,
                    tiles: Some(&candidates),
                    #[cfg(test)]
                    work: &work,
                };
                let t = std::time::Instant::now();
                for wave in plan.partitions.chunks(plan.wave) {
                    let members: Vec<Member> = collect_wave(&ctx, wave, &quiet)?
                        .buckets
                        .into_iter()
                        .flatten()
                        .collect();
                    probe_wave(members, &ctx, &mut level, diagnostics)?;
                }
                level
                    .terminals
                    .sort_unstable_by_key(|t| tile_key(t.x, t.y, zoom));
                log::info!(
                    "[export] variable depth z{zoom}: {} of {} candidate(s) stopped, {:.2}s",
                    level.terminals.len(),
                    candidates.len(),
                    t.elapsed().as_secs_f64(),
                );
            }
            stops += level.stopped.len();
            frontier.stopped[k].clone_from(&level.stopped);
            if !each(level) {
                return Ok(probed);
            }
        }
        log::info!("[export] variable depth: probed {probed} tile(s), stopped {stops}");
        Ok(probed)
    }
}

/// The probe's share of the partition wave: it gets `1 / PROBE_WAVE_SHARE`.
const PROBE_WAVE_SHARE: usize = 4;

/// One level's probe result: its stopped tiles' keys, and the tiles
/// themselves in key order.
pub(super) struct Probed {
    pub(super) level: usize,
    pub(super) stopped: HashSet<u64>,
    pub(super) terminals: Vec<EncodedTile>,
}

/// Probe one wave's tiles from their canonical `members`, adding the ones
/// that stop to `level`.
fn probe_wave(
    mut members: Vec<Member>,
    ctx: &LevelCtx<'_>,
    level: &mut Probed,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(), ExportError> {
    let (zoom, opts) = (ctx.zoom, ctx.opts);
    members.par_sort_unstable_by_key(|m| (m.key, m.seq));
    // Largest first, one tile per task, as `encode_members` does; each task
    // sorts its own tile into feature order.
    let mut tiles: Vec<(usize, &mut [Member])> = members
        .chunk_by_mut(|a, b| a.key == b.key)
        .enumerate()
        .collect();
    tiles.sort_by_key(|(_, t)| std::cmp::Reverse(t.len()));
    let mut stops: Vec<(usize, &[Member], TileMvt, Vec<_>)> = tiles
        .into_par_iter()
        .with_max_len(1)
        .map(|(i, tile)| {
            let _encode = ExportTimers::scope(&ctx.timers.encode);
            sort_tile_for_encode(tile, &opts.feature_order);
            let tile: &[Member] = tile;
            Ok(probe_tile(ctx.gzip_cache, tile, zoom, opts)?
                .map(|(mvt, events)| (i, tile, mvt, events)))
        })
        .filter_map(Result::transpose)
        .collect::<Result<_, ExportError>>()?;
    stops.sort_unstable_by_key(|(i, ..)| *i);
    for (_, tile, mvt, events) in stops {
        // Only the chosen tile's events and sources count, at its zoom; the
        // options' own extent does not enter the accounting.
        account_sources(tile, &mvt, zoom, opts, diagnostics)?;
        diagnostics.extend(events);
        let key = tile[0].key;
        let (x, y) = key_to_xy(key, zoom);
        level.stopped.insert(key);
        level
            .terminals
            .push(encoded_tile(x, y, mvt, FillTally::default())?);
    }
    // Freeing the members one by one is a serial walk over every clipped
    // geometry; let the pool do it while the next wave reads.
    rayon::spawn(move || drop(members));
    Ok(())
}

/// The extent a stopped tile at `zoom` carries the canonical level's
/// precision at, or `None` when even the tile's own corner would reach the
/// MVT coordinate clamp.
fn candidate_extent(zoom: u8, max_zoom: u8) -> Option<u32> {
    let extent = u64::from(DEFAULT_EXTENT) << (max_zoom - zoom).min(32);
    u32::try_from(extent)
        .ok()
        .filter(|&e| i64::from(e) < i64::from(crate::mvt::TILE_COORD_CLAMP))
}

/// Whether every coordinate a member of tile `key` can carry (the tile plus
/// its clip buffer) projects strictly inside the MVT coordinate clamp at
/// `opts.extent`, so nothing the tile draws is clamped.
fn within_clamp(key: u64, zoom: u8, opts: &ExportOptions) -> bool {
    let (x, y) = key_to_xy(key, zoom);
    let tb = TileCoord::new(x, y, zoom).bounds();
    let buf = buffer_deg_at_zoom(zoom, opts);
    let proj = crate::mvt::TileProjector::new(&tb, opts.extent);
    let limit = f64::from(crate::mvt::TILE_COORD_CLAMP);
    [
        (tb.lng_min - buf, tb.lat_min - buf),
        (tb.lng_max + buf, tb.lat_max + buf),
    ]
    .into_iter()
    .all(|(lng, lat)| {
        let (px, py) = proj.project(lng, lat);
        px.is_finite() && py.is_finite() && px.round().abs() < limit && py.round().abs() < limit
    })
}

/// A stopped tile's encode: `None` unless every member of `tile` encodes,
/// at `opts.extent`, into a tile within both caps. Never the valve: a tile
/// that needs it is not a stop. Its encode events are returned for the
/// caller to keep only if the tile is chosen.
fn probe_tile(
    gzip_cache: &Arc<TileGzipCache>,
    tile: &[Member],
    zoom: u8,
    opts: &ExportOptions,
) -> Result<Option<(TileMvt, Vec<crate::diagnostics::Diagnostic>)>, ExportError> {
    let (x, y) = key_to_xy(tile[0].key, zoom);
    let tb = TileCoord::new(x, y, zoom).bounds();
    let staged = crate::diagnostics::Diagnostics::silent();
    // The raw cap is checked as the tile is built, so a tile far over it
    // costs only the members that fill the cap, and is never compressed.
    let raw_cap = opts
        .tile_size_limit
        .filter(|&l| l > 0)
        .unwrap_or(usize::MAX);
    let Some(mut mvt) = build_mvt_within(
        gzip_cache,
        tile,
        opts,
        &[],
        |m| crate::mvt::EncodedFeatureGeometry::encode(&m.geom, &tb, opts.extent, &staged),
        raw_cap,
    ) else {
        return Ok(None);
    };
    if mvt.features == 0 || mvt.data.len() > raw_cap || tile_fit(&mut mvt, opts)? < 1.0 {
        return Ok(None);
    }
    Ok(Some((mvt, staged.snapshot())))
}

impl Frontier {
    pub(super) fn new(levels: usize) -> Self {
        Self {
            stopped: vec![HashSet::new(); levels],
        }
    }

    /// Whether a stopped tile at a zoom coarser than `zoom` covers `key`.
    fn covers(&self, zooms: &[u8], zoom: u8, key: u64) -> bool {
        let (x, y) = key_to_xy(key, zoom);
        zooms
            .iter()
            .zip(&self.stopped)
            .take_while(|(&z, _)| z < zoom)
            .any(|(&z, stopped)| {
                let shift = zoom - z;
                !stopped.is_empty() && stopped.contains(&tile_key(x >> shift, y >> shift, z))
            })
    }

    /// Take in a probed level's stops, before any finer level is restricted.
    pub(super) fn add(&mut self, level: &Probed) {
        self.stopped[level.level].clone_from(&level.stopped);
    }

    /// Drop level `k`'s stopped tiles, and every tile under a coarser stop,
    /// from its scan, so the export plans and draws only what remains.
    pub(super) fn restrict(&self, k: usize, scan: &mut LevelScan, zooms: &[u8]) {
        let zoom = zooms[k];
        scan.tile_counts
            .retain(|&key, _| !self.stopped[k].contains(&key) && !self.covers(zooms, zoom, key));
    }
}

// ============================================================================
// Drawing the levels around the probe
// ============================================================================

/// What [`export_levels`] draws: the rule-A′ plan to probe with, the levels
/// planned before the probe with their groups, and the scans of the rest.
pub(super) struct VdLevels<'a> {
    pub(super) plan: VdPlan,
    pub(super) probe: Probe<'a>,
    pub(super) planned: (&'a [ExportLevelCtx<'a>], Vec<Range<usize>>),
    pub(super) rest: &'a mut [LevelScan],
    pub(super) zooms: &'a [u8],
    /// Every level's shared context; each level sets its own scan, plan,
    /// index and stopped tiles.
    pub(super) template: ExportLevelCtx<'a>,
    /// The [`plan_level_groups`] member cap.
    pub(super) group_cap: usize,
}

/// What [`export_levels`] returns: the level reports, the group schedule
/// and the tiles probed.
type Drawn = (Vec<ZoomReport>, Vec<Range<usize>>, usize);

/// Export every level with variable depth: the levels no tile of which can
/// stop are drawn while the probe runs on its own thread, then the rest,
/// with their stopped tiles and without their subtrees. Returns the reports and
/// the group schedule, as [`export_level_groups`] would, and the tiles probed.
///
/// PERF (#803): the coarse levels of a dense file are a few huge tiles,
/// each fitted by a serial valve, so they leave most of the pool idle; the
/// probe reads and encodes the canonical band in that room.
pub(super) fn export_levels(
    writer: &mut StreamingPmtilesWriter,
    mem: LevelMemory<'_>,
    ckpt: &CheckpointCtx<'_>,
    progress: &mut ExportProgress,
    diagnostics: &crate::diagnostics::Diagnostics,
    run: VdLevels<'_>,
) -> Result<Drawn, ExportError> {
    let VdLevels {
        plan,
        probe,
        planned: (levels, mut groups),
        rest,
        zooms,
        template,
        group_cap,
    } = run;
    let LevelMemory { mut store, budget } = mem;
    let canonical = plan.canonical();
    let first = levels.len();
    let partition_budget = probe.budget;
    std::thread::scope(|scope| {
        let (tx, rx) = std::sync::mpsc::channel();
        let prober = scope.spawn(move || {
            let t = std::time::Instant::now();
            let probed = plan.probe(&probe, diagnostics, |level| tx.send(level).is_ok());
            (probed, t.elapsed().as_secs_f64())
        });
        let mut reports = export_level_groups(
            writer,
            LevelMemory {
                store: store.as_deref_mut(),
                budget: &mut *budget,
            },
            levels,
            &groups,
            ckpt,
            progress,
            diagnostics,
        )?;
        // The rest are drawn as the probe releases them: a level once it
        // and every coarser one are probed, the canonical level last, each
        // time with every level released so far as one run of groups.
        let mut frontier = Frontier::new(zooms.len());
        let mut terminals: Vec<Vec<EncodedTile>> = (0..zooms.len()).map(|_| Vec::new()).collect();
        let (mut received, mut next, mut rest) = (0, first, rest);
        while next <= canonical {
            let ready = |received: usize| {
                if next < canonical {
                    received > next
                } else {
                    received == canonical
                }
            };
            while received < canonical {
                let level = if ready(received) {
                    match rx.try_recv() {
                        Ok(level) => level,
                        Err(_) => break,
                    }
                } else {
                    match rx.recv() {
                        Ok(level) => level,
                        // The probe failed, or ended early.
                        Err(_) => return Err(prober_error(prober)),
                    }
                };
                frontier.add(&level);
                debug_assert!(level.terminals.is_empty() || level.level >= first);
                terminals[level.level] = level.terminals;
                received += 1;
            }
            let end = if received == canonical {
                canonical + 1
            } else {
                received
            };
            let (now, later) = std::mem::take(&mut rest).split_at_mut(end - next);
            rest = later;
            for (k, scan) in (next..).zip(now.iter_mut()) {
                frontier.restrict(k, scan, zooms);
            }
            let now: &[LevelScan] = now;
            let plans = plan_levels(now, &zooms[next..end], partition_budget)?;
            let now_levels: Vec<ExportLevelCtx<'_>> = plans
                .iter()
                .zip(now)
                .enumerate()
                .map(|(i, (plan, scan))| ExportLevelCtx {
                    scan,
                    plan,
                    level_idx: next + i,
                    terminals: &terminals[next + i],
                    ..template
                })
                .collect();
            let now_groups = plan_level_groups(&plans, group_cap);
            reports.extend(export_level_groups(
                writer,
                LevelMemory {
                    store: store.as_deref_mut(),
                    budget: &mut *budget,
                },
                &now_levels,
                &now_groups,
                ckpt,
                progress,
                diagnostics,
            )?);
            groups.extend(now_groups.into_iter().map(|g| g.start + next..g.end + next));
            next = end;
        }
        drop(rx);
        let (probed, secs) = prober
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        let probed = probed?;
        progress.profile.walls.probe = secs;
        Ok((reports, groups, probed))
    })
}

/// The error a probe that stopped sending ended with.
fn prober_error(
    prober: std::thread::ScopedJoinHandle<'_, (Result<usize, ExportError>, f64)>,
) -> ExportError {
    match prober.join() {
        Ok((Err(error), _)) => error,
        Ok((Ok(_), _)) => ExportError::Io(std::io::Error::other(
            "the variable-depth probe ended before every level was decided",
        )),
        Err(panic) => std::panic::resume_unwind(panic),
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

    /// The fused multi-zoom sizing counts exactly what per-zoom
    /// `tile_keys` counts, world-edge copies and null rows included.
    #[test]
    fn member_runs_match_tile_keys() {
        use crate::tile::TileBounds;
        // Small boxes at every zoom, wide ones only where they span few tiles
        // (a 30° box spans ~10^6 tiles at z14).
        let check = |widths: &[f64], zooms: std::ops::RangeInclusive<u8>| {
            let bboxes: Vec<Option<TileBounds>> = (0..4000)
                .map(|i| {
                    let f = f64::from(i);
                    (i % 97 != 0).then(|| {
                        let lng = -180.0 + (f * 0.3591) % 360.0;
                        let lat = -89.0 + (f * 0.1781) % 178.0;
                        let w = widths[i as usize % widths.len()];
                        TileBounds::new(lng, lat, (lng + w).min(180.0), (lat + w).min(90.0))
                    })
                })
                .collect();
            let opts = ExportOptions::default();
            let zooms: Vec<u8> = zooms.collect();
            let mut fused = vec![std::collections::HashMap::new(); zooms.len()];
            let rows: Vec<TileBounds> = bboxes.iter().flatten().copied().collect();
            super::add_runs(&mut fused, super::member_runs(&rows, &zooms, &opts));
            for (k, &z) in zooms.iter().enumerate() {
                assert_eq!(fused[k], super::tile_keys(&bboxes, z, &opts), "z{z}");
            }
        };
        check(&[0.0, 1e-7, 0.01, 0.3], 0..=14);
        check(&[2.0, 30.0], 0..=6);
    }

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
        scan.finish(&scans, &vd())
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

    // --- the probe -----------------------------------------------------------

    use std::collections::{BTreeMap, HashSet};

    use prost::Message;

    use super::super::{
        buffer_deg_at_zoom, export_pmtiles_with_partition_target, ExportReport, VariableDepthReport,
    };
    use super::{candidate_extent, within_clamp};
    use crate::archive_index::ArchiveIndex;
    use crate::tile::TileCoord;
    use crate::vector_tile::Tile;

    type Tiles = BTreeMap<(u8, u32, u32), Tile>;

    fn read_tiles(path: &Path) -> Tiles {
        let index = ArchiveIndex::open(path).unwrap();
        index
            .tiles()
            .map(|tile| {
                let tile = tile.unwrap();
                let gz = index.read_range(tile.range).unwrap();
                let raw = crate::compression::decompress_capped(
                    &gz,
                    crate::compression::Compression::Gzip,
                    crate::compression::MAX_TILE_BYTES,
                )
                .unwrap();
                (
                    (tile.z, tile.x, tile.y),
                    Tile::decode(raw.as_slice()).unwrap(),
                )
            })
            .collect()
    }

    /// Export `ov` with variable depth and `opts`; the report and every tile.
    fn export_vd(ov: &Path, opts: ExportOptions) -> (ExportReport, Tiles) {
        let out = ov.with_extension("vd.pmtiles");
        let report = export_pmtiles(
            ov,
            &out,
            &ExportOptions {
                variable_depth: true,
                ..opts
            },
        )
        .unwrap();
        (report, read_tiles(&out))
    }

    fn extent(tile: &Tile) -> u32 {
        tile.layers[0].extent.unwrap_or(4096)
    }

    /// The stopped tiles: coarser than `max_zoom` and wider than 4096.
    fn stopped(tiles: &Tiles, max_zoom: u8) -> Vec<(u8, u32, u32)> {
        tiles
            .iter()
            .filter(|(&(z, ..), t)| z < max_zoom && extent(t) != 4096)
            .map(|(&k, _)| k)
            .collect()
    }

    /// The archive contract: a stopped tile's extent is `4096 << (M - z)`,
    /// and nothing is stored below it or deeper than `M`.
    fn assert_contract(tiles: &Tiles, max_zoom: u8) {
        let stops = stopped(tiles, max_zoom);
        for &(z, x, y) in &stops {
            assert_eq!(
                extent(&tiles[&(z, x, y)]),
                4096 << (max_zoom - z),
                "z{z}/{x}/{y}"
            );
        }
        for &(z, x, y) in tiles.keys() {
            assert!(z <= max_zoom);
            for &(sz, sx, sy) in &stops {
                let under = z > sz && (x >> (z - sz), y >> (z - sz)) == (sx, sy);
                assert!(!under, "z{z}/{x}/{y} is under the stop z{sz}/{sx}/{sy}");
            }
        }
    }

    /// The input points, and their `v`, inside tile `t`'s buffered bounds.
    fn canonical_under(t: (u8, u32, u32), input: &[(f64, f64, f64)]) -> (usize, f64) {
        let tb = TileCoord::new(t.1, t.2, t.0).bounds();
        let buf = buffer_deg_at_zoom(t.0, &ExportOptions::default());
        let inside: Vec<f64> = input
            .iter()
            .filter(|(x, y, _)| {
                (tb.lng_min - buf..=tb.lng_max + buf).contains(x)
                    && (tb.lat_min - buf..=tb.lat_max + buf).contains(y)
            })
            .map(|&(.., v)| v)
            .collect();
        (inside.len(), inside.iter().sum())
    }

    /// A tile's features' sum of the double property `name`.
    fn sum_of(tile: &Tile, name: &str) -> f64 {
        let layer = &tile.layers[0];
        let key = layer.keys.iter().position(|k| k == name).unwrap() as u32;
        layer
            .features
            .iter()
            .filter_map(|f| {
                f.tags
                    .chunks(2)
                    .find(|kv| kv[0] == key)
                    .map(|kv| layer.values[kv[1] as usize].double_value.unwrap())
            })
            .sum()
    }

    fn features(tile: &Tile) -> usize {
        tile.layers[0].features.len()
    }

    /// A dense block of 2,500 points and five sparse points far away, every
    /// level holding all of them (no thinning, no density drop).
    fn dense_and_sparse() -> Vec<(f64, f64, f64)> {
        let mut input: Vec<(f64, f64, f64)> = (0..2500)
            .map(|i| {
                (
                    10.0 + (i % 50) as f64 * 1e-4,
                    45.0 + (i / 50) as f64 * 1e-4,
                    i as f64,
                )
            })
            .collect();
        input.extend((0..5).map(|i| (-70.0 + i as f64 * 0.01, -30.0, 1.0)));
        input
    }

    fn unthinned(dir: &Path, input: &[(f64, f64, f64)], max_zoom: u8) -> std::path::PathBuf {
        let geoms: Vec<_> = input.iter().map(|&(x, y, _)| pt(x, y)).collect();
        let v: Vec<f64> = input.iter().map(|&(.., v)| v).collect();
        let mut convert = ConvertOptions::default();
        convert.density.enabled = false;
        convert.assign.point_thinning = 0.0;
        overview_v(dir, &geoms, &v, max_zoom, convert)
    }

    fn raw_cap(bytes: usize) -> ExportOptions {
        ExportOptions {
            tile_size_limit: Some(bytes),
            compressed_tile_size_limit: None,
            ..ExportOptions::default()
        }
    }

    /// The sparse branch stops as soon as its tile leaves the dense block;
    /// the dense branch never fits and is drawn to the finest zoom.
    #[test]
    fn sparse_branch_stops_dense_descends() {
        let dir = tempfile::tempdir().unwrap();
        let input = dense_and_sparse();
        let ov = unthinned(dir.path(), &input, 10);
        let (report, tiles) = export_vd(&ov, raw_cap(20_000));
        assert_contract(&tiles, 10);
        let sparse = (1, 0, 1);
        assert_eq!(stopped(&tiles, 10), vec![sparse]);
        assert_eq!(extent(&tiles[&sparse]), 4096 << 9);
        assert_eq!(features(&tiles[&sparse]), 5);
        let dense = crate::tile::lng_lat_to_tile(10.0, 45.0, 10);
        assert_eq!(extent(&tiles[&(10, dense.x, dense.y)]), 4096);
        assert_eq!(report.max_zoom, 10);
    }

    /// Nothing is stored below a stopped tile, wherever the stops fall.
    #[test]
    fn stopped_subtree_has_no_descendants() {
        let dir = tempfile::tempdir().unwrap();
        // Scattered points: several branches stop, at different zooms.
        let input: Vec<(f64, f64, f64)> = (0..400)
            .map(|i| {
                let f = f64::from(i);
                ((f * 37.0) % 340.0 - 170.0, (f * 13.0) % 160.0 - 80.0, f)
            })
            .collect();
        let ov = unthinned(dir.path(), &input, 8);
        let (_, tiles) = export_vd(&ov, raw_cap(400));
        let stops = stopped(&tiles, 8);
        assert!(stops.len() > 3, "{stops:?}");
        assert!(
            stops
                .iter()
                .map(|s| s.0)
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1
        );
        assert_contract(&tiles, 8);
    }

    /// A tile over its cap is not a stop, though the valve could make it
    /// fit: every stopped tile holds every canonical point under it.
    #[test]
    fn probe_never_valves() {
        let dir = tempfile::tempdir().unwrap();
        // 300 points with distinct values: about 8 KB at z0.
        let input: Vec<(f64, f64, f64)> = (0..300)
            .map(|i| (-150.0 + f64::from(i), 10.0 + f64::from(i % 7), f64::from(i)))
            .collect();
        let ov = unthinned(dir.path(), &input, 6);
        let cap = 5_000;
        let (report, tiles) = export_vd(&ov, raw_cap(cap));
        assert_eq!(
            extent(&tiles[&(0, 0, 0)]),
            4096,
            "z0 is drawn with the valve"
        );
        assert!(report.valve_dropped_features > 0);
        let stops = stopped(&tiles, 6);
        assert!(!stops.is_empty());
        for s in stops {
            assert_eq!(features(&tiles[&s]), canonical_under(s, &input).0, "{s:?}");
            assert!(tiles[&s].encode_to_vec().len() <= cap);
        }
        assert_contract(&tiles, 6);
    }

    /// The raw cap and the gzip cap each stop a tile from stopping.
    #[test]
    fn both_caps_apply() {
        let dir = tempfile::tempdir().unwrap();
        let input: Vec<(f64, f64, f64)> =
            (0..5).map(|i| (10.0 + f64::from(i), 45.0, 1.0)).collect();
        let ov = unthinned(dir.path(), &input, 6);
        let (_, tiles) = export_vd(&ov, raw_cap(1_000_000));
        assert_eq!(tiles.keys().copied().collect::<Vec<_>>(), vec![(0, 0, 0)]);
        assert_eq!(extent(&tiles[&(0, 0, 0)]), 4096 << 6);
        let (_, tiles) = export_vd(&ov, raw_cap(40));
        assert!(stopped(&tiles, 6).is_empty());
        let gzip = ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: Some(40),
            ..ExportOptions::default()
        };
        let (_, tiles) = export_vd(&ov, gzip);
        assert!(stopped(&tiles, 6).is_empty());
    }

    /// The archive says it is variable-depth, with its canonical max zoom;
    /// the header's max zoom is the deepest stored tile; the report keeps
    /// the canonical and stored max zooms apart; and merge and pyramid
    /// refuse the archive, since their tiles would hide its stopped ones.
    #[test]
    fn marked_archive_reports_its_depth_and_refuses_merging() {
        let dir = tempfile::tempdir().unwrap();
        let input: Vec<(f64, f64, f64)> =
            (0..5).map(|i| (10.0 + f64::from(i), 45.0, 1.0)).collect();
        let ov = unthinned(dir.path(), &input, 6);
        let (report, tiles) = export_vd(&ov, raw_cap(1_000_000));
        assert_eq!(tiles.keys().copied().collect::<Vec<_>>(), vec![(0, 0, 0)]);
        let out = ov.with_extension("vd.pmtiles");
        let index = ArchiveIndex::open(&out).unwrap();
        assert_eq!(index.header().max_zoom, 0);
        assert!(index.is_variable_depth());
        assert_eq!(
            index.metadata_json().unwrap()["tylertoo:variable_depth"],
            serde_json::json!({"enabled": true, "canonical_maxzoom": 6})
        );
        assert_eq!(report.max_zoom, 6);
        assert_eq!(
            report.variable_depth,
            Some(VariableDepthReport {
                canonical_max_zoom: 6,
                stored_max_zoom: Some(0),
                probed_tiles: 1,
                stopped_tiles: 1,
            })
        );
        assert_eq!(report.zooms[0].stopped_tiles, 1);

        let merged = dir.path().join("merged.pmtiles");
        let err = crate::merge::merge_shards(
            std::slice::from_ref(&out),
            &merged,
            &crate::merge::MergeOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("variable-depth"), "{err}");
        let bands = crate::pyramid::parse_bands(&[format!("0-6:{}", out.display())]).unwrap();
        let err = crate::pyramid::merge_bands(&bands, &merged).unwrap_err();
        assert!(err.to_string().contains("variable-depth"), "{err}");
        assert!(!merged.exists());

        let off = dir.path().join("off.pmtiles");
        let report = export_pmtiles(&ov, &off, &raw_cap(1_000_000)).unwrap();
        assert_eq!(report.variable_depth, None);
        assert!(report.zooms.iter().all(|z| z.stopped_tiles == 0));
        assert!(!ArchiveIndex::open(&off).unwrap().is_variable_depth());
    }

    /// A rejected probe's repairs are not the export's: with every probe
    /// over the gzip cap, nothing stops, and the tiles and diagnostics match
    /// the export without variable depth.
    #[test]
    fn rejected_probes_leave_no_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        // Self-crossing bowties: every encode of one repairs it.
        // Lopsided, so their signed area is not zero, and big enough for the
        // visibility gate to keep them at z0.
        let geoms: Vec<Geometry> = (0..20)
            .map(|i| {
                let (x, y) = (-170.0 + f64::from(i) * 17.0, 10.0);
                Geometry::Polygon(geo::Polygon::new(
                    geo::LineString::from(vec![
                        (x, y),
                        (x + 8.0, y + 8.0),
                        (x + 8.0, y),
                        (x, y + 2.0),
                        (x, y),
                    ]),
                    vec![],
                ))
            })
            .collect();
        let v: Vec<f64> = (0..20).map(f64::from).collect();
        let ov = overview_v(dir.path(), &geoms, &v, 6, ConvertOptions::default());
        let gzip = ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: Some(40),
            ..ExportOptions::default()
        };
        let off_path = dir.path().join("off.pmtiles");
        let off = export_pmtiles(&ov, &off_path, &gzip).unwrap();
        let (on, tiles) = export_vd(&ov, gzip);
        let vd = on.variable_depth.as_ref().unwrap();
        assert!(vd.probed_tiles > 0);
        assert_eq!(vd.stopped_tiles, 0);
        assert!(
            off.diagnostics
                .iter()
                .any(|d| matches!(d, crate::diagnostics::Diagnostic::PolygonRepaired { .. })),
            "{:?}",
            off.diagnostics
        );
        // The memory budget note reads free RAM, which moves between runs.
        let export_events = |r: &ExportReport| -> Vec<_> {
            r.diagnostics
                .iter()
                .filter(|d| !matches!(d, crate::diagnostics::Diagnostic::MemoryBudget { .. }))
                .cloned()
                .collect()
        };
        assert_eq!(export_events(&on), export_events(&off));
        assert_eq!(on.source_losses, off.source_losses);
        assert_eq!(tiles, read_tiles(&off_path));
    }

    /// A tile's features' integer `id`s.
    fn ids(tile: &Tile) -> Vec<i64> {
        let layer = &tile.layers[0];
        let key = layer.keys.iter().position(|k| k == "id").unwrap() as u32;
        layer
            .features
            .iter()
            .filter_map(|f| {
                let kv = f.tags.chunks(2).find(|kv| kv[0] == key)?;
                let v = &layer.values[kv[1] as usize];
                v.int_value
                    .or(v.sint_value)
                    .or(v.uint_value.map(|u| i64::try_from(u).unwrap()))
            })
            .collect()
    }

    /// Parity with tippecanoe 2.79.0 on `tests/fixtures/variable-depth`, in
    /// its configuration (`-r1`: every point at every zoom, so thinning and
    /// the density budget off here). The stop sets need not match: the caps
    /// differ (tippecanoe's `-O 1000` features against our byte caps) and
    /// so does the precision (world 30 against world 26). What must hold is
    /// the contract, and that every point the finest tiles draw is drawn
    /// where its branch ends, under the default caps and under a raw cap
    /// near 1,000 of these points (29 bytes each at the canonical extent).
    ///
    /// Measured: tippecanoe stops 115 tiles at z10-z13 (header z14). With
    /// the default caps all 9,598 points (278 KB raw) fit one z3 tile, so
    /// that is the only stop and the header is z3. Under the 29,000-byte
    /// raw cap 25 tiles stop, 6 of them in tippecanoe's set, header z14:
    /// ours stop coarser, one of our stops covering several of theirs.
    #[test]
    fn tippecanoe_parity_fixture_keeps_the_contract() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/variable-depth");
        let manifest: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(fixture.join("manifest.json")).unwrap())
                .unwrap();
        let max_zoom = u8::try_from(manifest["canonical_maxzoom"].as_u64().unwrap()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ov = dir.path().join("ov.parquet");
        let mut convert = ConvertOptions::default();
        convert.density.enabled = false;
        convert.assign.point_thinning = 0.0;
        convert_to_overviews(
            fixture.join("points.parquet"),
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
        let theirs: HashSet<(u8, u32, u32)> = manifest["runs"]["tippecanoe"]["stopped_tiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let n = |i: usize| u32::try_from(t[i].as_u64().unwrap()).unwrap();
                (u8::try_from(n(0)).unwrap(), n(1), n(2))
            })
            .collect();
        assert_eq!(theirs.len(), 115);
        let rows = usize::try_from(manifest["input"]["rows"].as_u64().unwrap()).unwrap();

        for (opts, expect) in [(ExportOptions::default(), 3), (raw_cap(29_000), max_zoom)] {
            // The finest tiles of the export without variable depth, under
            // the same caps (so with the same valve at the finest zoom).
            let off = dir.path().join("off.pmtiles");
            export_pmtiles(&ov, &off, &opts).unwrap();
            let all: HashSet<i64> = read_tiles(&off)
                .iter()
                .filter(|(&(z, ..), _)| z == max_zoom)
                .flat_map(|(_, t)| ids(t))
                .collect();
            let (report, tiles) = export_vd(&ov, opts);
            assert_contract(&tiles, max_zoom);
            let deepest = tiles.keys().map(|&(z, ..)| z).max().unwrap();
            let out = ov.with_extension("vd.pmtiles");
            assert_eq!(ArchiveIndex::open(&out).unwrap().header().max_zoom, deepest);
            assert_eq!(
                report.variable_depth.as_ref().unwrap().stored_max_zoom,
                Some(deepest)
            );
            // Every point is drawn in the tile its branch ends at.
            let stops = stopped(&tiles, max_zoom);
            let drawn: HashSet<i64> = tiles
                .iter()
                .filter(|(&(z, x, y), _)| z == max_zoom || stops.contains(&(z, x, y)))
                .flat_map(|(_, t)| ids(t))
                .collect();
            // A stopped tile can also hold buffer points that a valved
            // neighbor at the finest zoom dropped.
            assert!(all.is_subset(&drawn));
            assert!(drawn.len() <= rows);
            assert!(!stops.is_empty());
            assert_eq!(deepest, expect);
            log::info!(
                "parity: {} stopped, {} in tippecanoe's set",
                stops.len(),
                stops.iter().filter(|t| theirs.contains(t)).count()
            );
        }
    }

    /// A candidate's extent, buffer included, stays inside the MVT
    /// coordinate clamp, so a stopped tile never clamps a coordinate.
    #[test]
    fn candidate_extent_never_clamps() {
        assert_eq!(candidate_extent(2, 14), None);
        assert_eq!(candidate_extent(3, 14), Some(1 << 23));
        assert_eq!(candidate_extent(0, 11), Some(1 << 23));
        assert_eq!(candidate_extent(14, 14), Some(4096));
        let opts = |extent| ExportOptions {
            extent,
            ..ExportOptions::default()
        };
        let key = super::tile_key(4, 3, 3);
        assert!(within_clamp(key, 3, &opts(1 << 23)));
        // Inside the clamp itself, but not with the buffer.
        assert!(!within_clamp(key, 3, &opts((1 << 24) - 4096)));
        // At M = 14 nothing coarser than z3 is probed, so z0-z2 are drawn.
        let dir = tempfile::tempdir().unwrap();
        let input: Vec<(f64, f64, f64)> = vec![(10.0, 45.0, 1.0)];
        let ov = unthinned(dir.path(), &input, 14);
        let (_, tiles) = export_vd(&ov, ExportOptions::default());
        assert_eq!(stopped(&tiles, 14), vec![(3, 4, 2)]);
        assert_contract(&tiles, 14);
    }

    /// With clustering, a stopped tile draws the canonical points one by
    /// one: its feature count and its sum are those of the points under it.
    #[test]
    fn clustered_terminal_counts_and_sums_match() {
        let dir = tempfile::tempdir().unwrap();
        let mut input: Vec<(f64, f64, f64)> = (0..10)
            .map(|i| (10.0 + f64::from(i) * 0.05, 45.0, f64::from(i + 1)))
            .collect();
        input.push((-70.0, -30.0, 7.0));
        let geoms: Vec<_> = input.iter().map(|&(x, y, _)| pt(x, y)).collect();
        let v: Vec<f64> = input.iter().map(|&(.., v)| v).collect();
        let mut convert = ConvertOptions {
            cluster: true,
            accumulate: vec![AccumulateSpec {
                column: "v".into(),
                op: AccumulateOp::Sum,
            }],
            ..ConvertOptions::default()
        };
        convert.density.enabled = false;
        let ov = overview_v(dir.path(), &geoms, &v, 10, convert);
        let (_, tiles) = export_vd(&ov, ExportOptions::default());
        assert_contract(&tiles, 10);
        let stops = stopped(&tiles, 10);
        assert!(stops.contains(&(1, 0, 1)), "{stops:?}");
        assert!(
            stops.iter().any(|s| canonical_under(*s, &input).0 > 1),
            "{stops:?}"
        );
        for s in stops {
            let (n, sum) = canonical_under(s, &input);
            assert_eq!(features(&tiles[&s]), n, "{s:?}");
            assert_eq!(sum_of(&tiles[&s], "v"), sum, "{s:?}");
        }
    }

    /// The probe's output depends on neither the thread count nor how the
    /// tiles are cut into partitions and waves.
    #[test]
    fn output_is_identical_across_threads_and_waves() {
        let dir = tempfile::tempdir().unwrap();
        let mut input = dense_and_sparse();
        input.extend((0..200).map(|i| (100.0 + f64::from(i) * 0.3, -10.0, f64::from(i))));
        let ov = unthinned(dir.path(), &input, 10);
        let run = |threads: usize, wave: usize, target: usize| {
            let out = dir
                .path()
                .join(format!("{threads}-{wave}-{target}.pmtiles"));
            let opts = ExportOptions {
                variable_depth: true,
                partition_wave: wave,
                ..raw_cap(20_000)
            };
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| export_pmtiles_with_partition_target(&ov, &out, &opts, target))
                .unwrap();
            std::fs::read(out).unwrap()
        };
        let reference = run(1, 1, 1);
        assert!(!stopped(&read_tiles(&dir.path().join("1-1-1.pmtiles")), 10).is_empty());
        assert_eq!(run(8, 4, 64), reference);
        assert_eq!(
            run(4, ExportOptions::default().partition_wave, 100_000),
            reference
        );
    }
}
