//! Overview conversion pipeline (task P5; streaming since H3).
//!
//! [`convert_to_overviews`] wires the existing overview modules into a single
//! GeoParquet → GeoParquet overview build. By default
//! ([`ConvertOptions::streaming`]) it dispatches to the two-pass
//! bounded-memory pipeline in [`super::stream`]; the in-memory reference
//! implementation below (`streaming: false`) proceeds as:
//!
//! 1. **read** the whole input GeoParquet preserving the full property schema
//!    (the entire table is concatenated into one batch). The CRS is detected
//!    from the `geo` metadata and mapped to [`Crs`]; non-4326/3857 inputs and
//!    inputs that already carry a `level` column are rejected (spec Q3, §4.1).
//! 2. **assign** every feature a coarsest level via [`assign::assign_levels`]
//!    over per-feature bbox + [`FeatureKind`] + an optional sort key.
//! 3. **generalize + write**, coarse→fine, feeding [`OverviewWriter`]:
//!    - `duplicating` non-canonical levels: [`simplify::simplify_for_level`]
//!      per feature, dropping [`Simplified::Dropped`];
//!    - `duplicating` canonical (finest) level: original geometry **untouched**
//!      (spec §2.4, value-identity — no simplify round-trip);
//!    - `partitioning` (all levels): original geometry **verbatim** (§2.3).
//!
//! Input (Hilbert) order is preserved within each level (no re-sort).
//! 4. **report**: a [`ConvertReport`] (per-level feature/vertex/byte counts,
//!    totals, duration) is returned and is `serde` `Serialize` for the later
//!    benchmark tasks.
//!
//! The in-memory path is the correctness-first reference: memory is
//! `O(dataset)`. The default streaming path ([`super::stream`]) produces
//! equivalent output in `O(read batch + winner tables)` memory; equivalence
//! is asserted by the `streaming_matches_*` tests below.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use arrow_array::{Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use arrow_select::concat::concat_batches;
use arrow_select::take::take;
use geo::{BoundingRect, Geometry};
use geoarrow::array::{from_arrow_array, GeometryBuilder};
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use crate::input::InputSource;
use crate::input_set::ConvertSource;
use serde::Serialize;

use crate::batch_processor::extract_geometries_opt_from_array;

use super::assign::{
    apply_density_budget, assign_levels, AssignConfig, AssignFeature, DensityBudgetConfig,
    FeatureKind, SUPERCELL_GSD_FACTOR,
};
use super::cluster::{
    build_cluster_tables, verify_sum_invariant, AccumulateSpec, ClusterEntry, POINT_COUNT_COLUMN,
};
use super::coalesce::{
    coalesce_level_lines, CoalesceInput, CoalesceParams, COALESCED_COUNT_COLUMN,
    DEFAULT_COALESCE_MAX_LEVEL_ROWS, DEFAULT_JUNCTION_ANGLE_DEG, DEFAULT_SNAP_GSD_FACTOR,
};
use super::level::{
    gsd_with_base, AccumulatedColumn, ClusteringProvenance, CoalescingProvenance, Crs,
    DensityProvenance, Generalization, GeneralizationLevel, MemoryProfile, Mode, RankingProvenance,
    GSD_TILE_BASE, METERS_PER_DEGREE,
};
use super::simplify::{simplify_cascade, simplify_for_level, Simplified, SimplifyOptions};
use super::writer::{
    LevelSpec, LevelWriteOutcome, OverviewWriter, OverviewWriterOptions, RowGroupSizePolicy,
    WriterError, LEVEL_COLUMN,
};

/// How the caller specifies the overview levels.
#[derive(Debug, Clone, PartialEq)]
pub enum LevelPlan {
    /// A Web Mercator zoom range, mapped through [`gsd_for_zoom`]. `min_zoom`
    /// is the coarsest (level 0), `max_zoom` the finest. Both inclusive.
    ZoomRange {
        /// Coarsest zoom (level 0).
        min_zoom: u8,
        /// Finest zoom (canonical level in duplicating mode).
        max_zoom: u8,
    },
    /// An explicit list of per-level GSDs in **meters**, coarse→fine (strictly
    /// decreasing). No `zoom` is recorded on the levels.
    Gsds(Vec<f64>),
}

/// Maximum number of levels a plan may resolve to. The per-feature winner
/// tables store level indices as `u8` (with `u8::MAX` reserved as the
/// streaming pipeline's "no feature on this row" sentinel), so plans beyond
/// 255 levels are rejected instead of silently wrapping.
pub(super) const MAX_LEVELS: usize = 255;

impl LevelPlan {
    /// Resolve to the coarse→fine list of `(gsd_meters, zoom?)` level specs.
    ///
    /// `gsd_base` is the GSD tile-band base (spec §5.2 / Q6); it scales the
    /// per-zoom GSDs of a [`ZoomRange`](LevelPlan::ZoomRange) plan and has no
    /// effect on an explicit [`Gsds`](LevelPlan::Gsds) plan (those GSDs are
    /// already in meters).
    pub(super) fn resolve(&self, gsd_base: f64) -> Result<Vec<(f64, Option<u8>)>, ConvertError> {
        let check_len = |n: usize| {
            if n > MAX_LEVELS {
                return Err(ConvertError::InvalidLevels(format!(
                    "{n} levels requested; at most {MAX_LEVELS} levels are supported"
                )));
            }
            Ok(())
        };
        match self {
            LevelPlan::ZoomRange { min_zoom, max_zoom } => {
                if min_zoom > max_zoom {
                    return Err(ConvertError::InvalidLevels(format!(
                        "min_zoom {min_zoom} must be <= max_zoom {max_zoom}"
                    )));
                }
                check_len(*max_zoom as usize - *min_zoom as usize + 1)?;
                Ok((*min_zoom..=*max_zoom)
                    .map(|z| (gsd_with_base(z, gsd_base), Some(z)))
                    .collect())
            }
            LevelPlan::Gsds(gsds) => {
                if gsds.is_empty() {
                    return Err(ConvertError::InvalidLevels(
                        "explicit gsd list must be non-empty".to_string(),
                    ));
                }
                check_len(gsds.len())?;
                let mut prev: Option<f64> = None;
                for (i, &g) in gsds.iter().enumerate() {
                    if g <= 0.0 || g.is_nan() {
                        return Err(ConvertError::InvalidLevels(format!(
                            "gsd[{i}] = {g} must be > 0"
                        )));
                    }
                    if let Some(p) = prev {
                        if g >= p {
                            return Err(ConvertError::InvalidLevels(format!(
                                "gsd list must be strictly decreasing coarse→fine (gsd[{i}] = {g} >= previous {p})"
                            )));
                        }
                    }
                    prev = Some(g);
                }
                Ok(gsds.iter().map(|&g| (g, None)).collect())
            }
        }
    }
}

/// A categorical (class-aware) cell-winner ranking (Q1, tier 1 / tier 3).
///
/// Maps the string values of a column to numeric priorities. Higher priority
/// **wins** a grid cell — matching [`assign`](super::assign)'s default
/// `SortDirection::Desc` (larger `sort_key` wins). A feature whose column value
/// is present but not in [`ranks`](ClassRanking::ranks) is assigned
/// [`unknown_rank`](ClassRanking::unknown_rank), which — as long as it is below
/// every named rank — **loses to all named classes but still beats a
/// null/missing value** (nulls encode as `None`, which loses to any `Some` in
/// the priority order).
#[derive(Debug, Clone, PartialEq)]
pub struct ClassRanking {
    /// Name of the (Utf8/LargeUtf8) column whose values are ranked.
    pub column: String,
    /// `(value, priority)` pairs; higher priority wins the cell. Order is
    /// irrelevant (looked up by value).
    pub ranks: Vec<(String, f64)>,
    /// Priority for a present-but-unranked value. Set below every named rank so
    /// unknown classes lose to known ones but beat nulls.
    pub unknown_rank: f64,
}

/// Upper bound on how many `(value, priority)` pairs are echoed into the
/// footer provenance block (§3.5). Larger maps record only the mode + column.
const MAX_PROVENANCE_RANKS: usize = 64;

/// Built-in Overture transportation `class` ranking (Q1, tier 3 auto-detect).
///
/// Spine (highest→lowest, always holds): motorway > trunk > primary >
/// secondary > tertiary > residential > unclassified > service. Below service
/// come the remaining pedestrian/minor classes, then everything unrecognized
/// (rail classes, the literal `unknown`, driveways, …) falls to
/// [`unknown_rank`](ClassRanking::unknown_rank).
pub fn overture_road_ranking(column: String) -> ClassRanking {
    // Descending priorities; spine first (see doc comment), then the tail.
    let ordered = [
        "motorway", // spine
        "trunk",
        "primary",
        "secondary",
        "tertiary",
        "residential",
        "unclassified",
        "service",
        "living_street", // tail (all below service)
        "pedestrian",
        "track",
        "cycleway",
        "bridleway",
        "footway",
        "steps",
        "path",
        "driveway",
        "parking_aisle",
    ];
    let n = ordered.len();
    let ranks = ordered
        .iter()
        .enumerate()
        // Highest priority for the first entry; all strictly positive so every
        // named class beats unknown_rank (0.0).
        .map(|(i, &c)| (c.to_string(), (n - i) as f64))
        .collect();
    ClassRanking {
        column,
        ranks,
        unknown_rank: 0.0,
    }
}

/// Known Overture transportation `class` / `road_class` vocabulary, used only to
/// decide whether an auto-detected `class`/`road_class` column is *actually* a
/// road-class column (overlap gate). Includes rail/pedestrian values that the
/// ranking itself leaves at `unknown_rank`.
pub(super) const KNOWN_ROAD_CLASSES: &[&str] = &[
    "motorway",
    "trunk",
    "primary",
    "secondary",
    "tertiary",
    "residential",
    "unclassified",
    "service",
    "living_street",
    "pedestrian",
    "track",
    "cycleway",
    "bridleway",
    "footway",
    "steps",
    "path",
    "driveway",
    "parking_aisle",
    "unknown",
    // rail subtype classes (present in Overture transportation extracts)
    "standard_gauge",
    "light_rail",
    "tram",
    "subway",
    "monorail",
    "funicular",
];

/// Minimum number of *distinct* known road classes a candidate column must
/// contain before auto-detection treats it as Overture roads.
pub(super) const ROAD_VOCAB_MIN_DISTINCT: usize = 3;

/// Options for [`convert_to_overviews`].
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// Level materialization mode. Default [`Mode::Duplicating`].
    pub mode: Mode,
    /// How levels are specified (zoom range or explicit GSDs).
    pub levels: LevelPlan,
    /// Thinning / visibility / sort configuration for level assignment.
    pub assign: AssignConfig,
    /// Optional column name whose (numeric) value is used as the cell-winner
    /// sort key. Mutually exclusive with [`class_ranking`](Self::class_ranking).
    pub sort_key: Option<String>,
    /// Optional explicit categorical class ranking (Q1 tier 1). Mutually
    /// exclusive with [`sort_key`](Self::sort_key).
    pub class_ranking: Option<ClassRanking>,
    /// Disable tier-3 auto-detection of well-known schemas (Overture roads /
    /// places confidence). No effect when `sort_key` or `class_ranking` is set.
    pub no_auto_rank: bool,
    /// Per-level simplification options (duplicating mode only).
    pub simplify: SimplifyOptions,
    /// Per-level density budget applied after cell-winner thinning (Q2). Default
    /// enabled; disable via `--no-density-drop` to reproduce pre-Q2 behavior.
    pub density: DensityBudgetConfig,
    /// GSD tile-band base for zoom→GSD derivation (spec §5.2 / Q6; the cogp-rs
    /// `base` knob). Default [`GSD_TILE_BASE`] (1024). Larger ⇒ smaller GSDs
    /// (finer detail / less thinning); smaller ⇒ larger GSDs (coarser / more
    /// thinning). No effect on an explicit [`LevelPlan::Gsds`] plan.
    pub gsd_base: f64,
    /// Emit the optional COGP compatibility footer key (§3.1). Default `false`.
    pub cogp_compat_key: bool,
    /// Maximum row-group size in rows for the output writer.
    pub max_row_group_size: usize,
    /// How the per-level row-group cap is derived from `max_row_group_size`
    /// (#202). Default [`RowGroupSizePolicy::Constant`]; `ZoomScaled` doubles
    /// the cap per zoom step below the finest level (fewer requests on coarse
    /// bands that wide viewports read mostly whole anyway).
    pub row_group_size_policy: RowGroupSizePolicy,
    /// Keep full Parquet statistics on every column (including high-cardinality
    /// string/binary property columns and the WKB geometry column). Default
    /// `false`: those stats are suppressed to keep the footer small (H1); the
    /// bbox covering and `level` column always keep their pruning stats. Set
    /// `true` for clients that push property predicates to the remote file.
    pub full_column_stats: bool,
    /// Use the two-pass bounded-memory streaming pipeline (H3). Default `true`.
    ///
    /// Pass 1 streams the input once to build the per-feature winner tables
    /// (level assignment + Q2 density budget); pass 2 streams the input again
    /// per level, simplifying and writing batch-by-batch. Peak memory is
    /// `O(read batch + winner tables)` instead of `O(dataset)`. Set `false`
    /// to use the original in-memory pipeline (kept for comparison; produces
    /// equivalent output).
    pub streaming: bool,
    /// Rows per Arrow read batch in the streaming pipeline (both passes).
    /// Default [`DEFAULT_READ_BATCH_SIZE`]. Larger batches amortize per-batch
    /// overhead at the cost of proportionally more peak memory; smaller
    /// batches bound memory tighter. No effect when `streaming` is `false`.
    pub read_batch_size: usize,
    /// Memory/throughput profile for the streaming pass-2 engine (#213/#212).
    /// Default [`MemoryProfile::Auto`], resolved per mode + estimated output
    /// size at convert entry. Changes speed and peak memory only — output is
    /// byte-identical across profiles. No effect when `streaming` is `false`.
    pub profile: MemoryProfile,
    /// Number of Arrow read batches allowed in flight through the streaming
    /// pass-2 pipeline at once (bounded-channel depth / read-compute overlap
    /// knob). Default [`DEFAULT_IN_FLIGHT_BATCHES`]. Higher improves core
    /// utilization on long-pole geometries at proportionally more peak memory
    /// (`in_flight_batches × read_batch_size` rows resident). No effect when
    /// `streaming` is `false`.
    pub in_flight_batches: usize,
    /// Enable point clustering (plan Q4; opt-in per spec §11 Q4). Duplicating
    /// mode only. When enabled, each level's point cell-winners absorb the
    /// other point features in their cell: the output gains a `point_count`
    /// INT64 NOT NULL column (1 at the canonical level) and the winner keeps
    /// its own geometry and attributes (see [`super::cluster`]). Lines and
    /// polygons are unaffected. Default `false`.
    pub cluster: bool,
    /// Numeric per-cluster attribute aggregation (Q6): for each spec, the
    /// winner's value of the column becomes the aggregate over itself + the
    /// absorbed features at that level. Requires [`cluster`](Self::cluster).
    /// Empty by default.
    pub accumulate: Vec<AccumulateSpec>,
    /// Enable line network coalescing (plan Q3). **Default `true`** (like
    /// `line_thinning = 1.0` and the clustering point grid, chosen by
    /// maintainer render review: defaults should look right). At each
    /// non-canonical duplicating level, touching same-class line segments
    /// are chained into single "stroke" LineStrings BEFORE the visibility
    /// gate and thinning run (see [`super::coalesce`]), so fragmented
    /// networks read as connected arteries at coarse zooms. The output
    /// gains a `coalesced_count` INT32 NOT NULL column (source segments
    /// merged per row; 1 for unmerged rows and at the canonical level).
    /// Points and polygons are unaffected.
    ///
    /// **Partitioning mode**: coalescing cannot be represented there (a
    /// merged chain violates §2.3's feature-once/verbatim contract), so
    /// this option is treated as INERT for partitioning conversions — the
    /// output has no `coalesced_count` column and no coalescing provenance.
    /// (The CLI additionally rejects an *explicit* request.)
    pub coalesce_lines: bool,
    /// Endpoint snap tolerance for coalescing, in GSD multiples (default
    /// [`DEFAULT_SNAP_GSD_FACTOR`] = 1.0): after exact-endpoint chaining,
    /// chain ends within `factor × gsd` of each other are joined. `<= 0`
    /// disables the snap pass (exact coordinate matching only).
    pub coalesce_snap: f64,
    /// Per-level candidate ceiling for coalescing (default
    /// [`DEFAULT_COALESCE_MAX_LEVEL_ROWS`]): chaining holds the level's
    /// candidate line geometries in memory at once, so levels with more
    /// candidate lines than this skip coalescing (with a log) instead of
    /// breaking the streaming pipeline's memory bound.
    pub coalesce_max_level_rows: usize,
    /// Junction continuation threshold for coalescing, in degrees (default
    /// [`DEFAULT_JUNCTION_ANGLE_DEG`] = `0` = OFF, per maintainer render
    /// review — strict degree-2 chaining looks better on road networks).
    /// When `> 0`: at junction nodes (degree >= 3), compatible incident
    /// lines that continue each other within this deviation from straight
    /// merge best-pair-first, so arterials chain THROUGH same-class
    /// crossings (fewer, longer strokes at the cost of over-merging).
    pub coalesce_junction_angle: f64,
    /// Regional extract (#102): `[xmin, ymin, xmax, ymax]` in EPSG:4326
    /// lon/lat degrees. When set, the conversion behaves as if the input
    /// contained only the features whose bounding box intersects this region
    /// (closed-interval AABB test): input row groups whose GeoParquet 1.1
    /// bbox covering statistics don't intersect are skipped at the parquet
    /// footer level (their data pages are never read), and features of the
    /// surviving row groups are filtered exactly by their own bbox. Inputs
    /// without covering statistics degrade gracefully — every row group is
    /// read and only the exact per-feature filter applies, so the output is
    /// identical either way. Default `None` (full-extent conversion,
    /// byte-identical output to a build without this option).
    pub bbox: Option<[f64; 4]>,
    /// Directory for the remote-input disk spill (#219 / #272). A remote
    /// convert stages every fetched column chunk in an anonymous temp file
    /// (growing to ≈1× the touched input bytes) so later passes re-read
    /// from local disk instead of the network. `None` (default) places it
    /// under the process temp dir (`$TMPDIR`); set this to use a roomier
    /// or faster volume instead. The directory must exist (validated up
    /// front). Local inputs never spill, so this has no effect on them.
    pub spill_dir: Option<PathBuf>,
}

/// Default rows per read batch for the streaming pipeline (H3).
pub const DEFAULT_READ_BATCH_SIZE: usize = 8192;

/// Default number of read batches in flight through the pass-2 pipeline
/// (#213). Four keeps a few cores fed on long-pole geometries while bounding
/// resident batches to `4 × read_batch_size` rows.
pub const DEFAULT_IN_FLIGHT_BATCHES: usize = 4;

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 6,
            },
            assign: AssignConfig::default(),
            sort_key: None,
            class_ranking: None,
            no_auto_rank: false,
            simplify: SimplifyOptions::default(),
            density: DensityBudgetConfig::default(),
            gsd_base: GSD_TILE_BASE,
            cogp_compat_key: false,
            max_row_group_size: super::writer::DEFAULT_MAX_ROW_GROUP_SIZE,
            row_group_size_policy: RowGroupSizePolicy::default(),
            full_column_stats: false,
            streaming: true,
            read_batch_size: DEFAULT_READ_BATCH_SIZE,
            profile: MemoryProfile::Auto,
            in_flight_batches: DEFAULT_IN_FLIGHT_BATCHES,
            cluster: false,
            accumulate: Vec::new(),
            coalesce_lines: true,
            coalesce_snap: DEFAULT_SNAP_GSD_FACTOR,
            coalesce_max_level_rows: DEFAULT_COALESCE_MAX_LEVEL_ROWS,
            coalesce_junction_angle: DEFAULT_JUNCTION_ANGLE_DEG,
            bbox: None,
            spill_dir: None,
        }
    }
}

/// Per-level statistics in a [`ConvertReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LevelReport {
    /// Level index in the output file (0 = coarsest).
    pub level: usize,
    /// Ground sample distance in meters for this level.
    pub gsd: f64,
    /// Web Mercator zoom, if the level plan supplied one.
    pub zoom: Option<u8>,
    /// Number of features (rows) written at this level.
    pub feature_count: usize,
    /// Total geometry vertex (coordinate) count across the level's features.
    pub vertex_count: usize,
    /// Uncompressed size of the level's row groups (bytes).
    pub uncompressed_bytes: i64,
    /// Compressed on-disk size of the level's row groups (bytes).
    pub compressed_bytes: i64,
}

/// A planned level omitted from the output because it contained no rows
/// (#211 auto-clamp; spec §7.3 requires empty levels to be omitted and the
/// remaining levels renumbered).
///
/// The most common shape is a coarse prefix: e.g. country-scale buildings
/// where every feature is culled by the visibility gates at world zooms, so
/// the written pyramid starts at the first zoom with visible features.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SkippedLevelReport {
    /// Index of the level in the *planned* (requested) level range
    /// (0 = requested coarsest). NOT an index into the written file.
    pub planned_level: usize,
    /// Planned ground sample distance in meters.
    pub gsd: f64,
    /// Planned Web Mercator zoom, if the level plan supplied one.
    pub zoom: Option<u8>,
}

/// One WARN for the planned levels omitted because no feature is visible at
/// their scale (#211 auto-clamp). No-op when nothing was skipped.
pub(super) fn warn_plan_skipped_levels(
    skipped: &[SkippedLevelReport],
    input_features: usize,
    first_written_gsd: f64,
    first_written_zoom: Option<u8>,
) {
    if skipped.is_empty() {
        return;
    }
    let ids: Vec<String> = skipped
        .iter()
        .map(|s| s.planned_level.to_string())
        .collect();
    let gsd_max = skipped.iter().map(|s| s.gsd).fold(f64::MIN, f64::max);
    let gsd_min = skipped.iter().map(|s| s.gsd).fold(f64::MAX, f64::min);
    let zoom_note = first_written_zoom.map_or_else(String::new, |z| format!(" (zoom {z})"));
    log::warn!(
        "omitting {} empty level(s) [{}] spanning GSD {:.2}–{:.2} m: none of the {} input \
         feature(s) are visible at those scales (visibility gates / density budget); the \
         output pyramid starts at GSD {:.2} m{}. To populate coarse levels, lower \
         --polygon-visibility/--line-visibility, or pass --collapse to keep sub-GSD \
         polygons as representative points (see docs/OVERVIEW_TUNING.md)",
        skipped.len(),
        ids.join(", "),
        gsd_max,
        gsd_min,
        input_features,
        first_written_gsd,
        zoom_note,
    );
}

/// Fold one [`OverviewWriter::write_level`] outcome into the driver
/// bookkeeping shared by both pipelines (#211): a written level appends a
/// [`LevelReport`] renumbered to the written count; an empty level (every
/// candidate collapsed during simplification) warns and records `planned` in
/// the skipped list instead.
pub(super) fn record_level_outcome(
    outcome: LevelWriteOutcome,
    planned: SkippedLevelReport,
    candidates: usize,
    rows: usize,
    vertices: usize,
    level_reports: &mut Vec<LevelReport>,
    skipped: &mut Vec<SkippedLevelReport>,
) {
    match outcome {
        LevelWriteOutcome::SkippedEmpty => {
            log::warn!(
                "level planned at GSD {:.2} m{} became empty after simplification \
                 (all {} candidate feature(s) collapsed); omitted from the output pyramid. \
                 Pass --collapse to keep sub-GSD polygons as representative points at \
                 coarse levels (see docs/OVERVIEW_TUNING.md)",
                planned.gsd,
                planned
                    .zoom
                    .map_or_else(String::new, |z| format!(" (zoom {z})")),
                candidates,
            );
            skipped.push(planned);
        }
        LevelWriteOutcome::Written => level_reports.push(LevelReport {
            level: level_reports.len(),
            gsd: planned.gsd,
            zoom: planned.zoom,
            feature_count: rows,
            vertex_count: vertices,
            uncompressed_bytes: 0,
            compressed_bytes: 0,
        }),
    }
}

/// Result of a conversion, `Serialize` for JSON output (benchmark tasks).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConvertReport {
    /// Level materialization mode used.
    pub mode: Mode,
    /// Per-level statistics, coarse→fine. `level` here is the index in the
    /// WRITTEN file, which is shifted down from the planned range when empty
    /// levels were skipped (see
    /// [`skipped_empty_levels`](Self::skipped_empty_levels)).
    pub levels: Vec<LevelReport>,
    /// Planned levels omitted because they contained no rows (#211
    /// auto-clamp), ordered by planned level. Empty when every planned level
    /// was written. The effective level range of the output is exactly
    /// [`levels`](Self::levels).
    pub skipped_empty_levels: Vec<SkippedLevelReport>,
    /// Number of source features read from the input.
    pub input_features: usize,
    /// Total rows written across all levels.
    pub total_rows: usize,
    /// Total vertices written across all levels.
    pub total_vertices: usize,
    /// Total compressed output size (bytes) across all levels.
    pub total_compressed_bytes: i64,
    /// Total row groups in the input file.
    pub row_groups_total: usize,
    /// Input row groups actually read. Less than
    /// [`row_groups_total`](Self::row_groups_total) only when
    /// [`ConvertOptions::bbox`] pruned row groups via the input's bbox
    /// covering statistics (#102).
    pub row_groups_read: usize,
    /// Features whose bbox spans more than 180° of longitude — almost
    /// certainly antimeridian-crossing geometry stored verbatim. Warned
    /// about (one aggregate `log::warn!`), never mutated; see
    /// `context/ANTIMERIDIAN.md` (issue #188).
    pub antimeridian_suspect_features: usize,
    /// Wall-clock conversion duration in seconds.
    pub duration_secs: f64,
    /// Remote-input fetch counters (#210): range requests issued and bytes
    /// downloaded, against the total object size. `None` for local inputs.
    /// With [`ConvertOptions::bbox`], `bytes_fetched / object_size` is the
    /// fraction of the remote file actually moved.
    pub remote_fetch: Option<crate::input::FetchStats>,
}

/// Errors from [`convert_to_overviews`].
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Opening the input failed (bad URL scheme, remote store error, ...).
    #[error("input error: {0}")]
    Input(#[from] crate::input::InputError),
    /// Underlying parquet error (reading the input).
    #[error("parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// Arrow error (concat / take / batch build).
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// A core-library error (geometry decode, CRS extraction).
    #[error("{0}")]
    Core(#[from] crate::Error),
    /// The overview writer failed.
    #[error("writer error: {0}")]
    Writer(#[from] WriterError),
    /// The input CRS is neither EPSG:4326 nor EPSG:3857 (spec Q3).
    #[error("unsupported input CRS {crs:?}: overviews require EPSG:4326 or EPSG:3857")]
    UnsupportedCrs {
        /// The rejected CRS identifier.
        crs: String,
    },
    /// The input has no geometry column.
    #[error("input has no geometry column")]
    NoGeometryColumn,
    /// The `--sort-key` column is not present in the input schema.
    #[error("sort-key column {name:?} not found in input schema")]
    SortKeyColumnMissing {
        /// The requested column name.
        name: String,
    },
    /// Both a numeric sort key and a categorical class ranking were supplied.
    #[error("--sort-key and --class-rank are mutually exclusive; supply at most one")]
    RankingConflict,
    /// The `--class-rank` column is not present in the input schema.
    #[error("class-rank column {name:?} not found in input schema")]
    ClassRankColumnMissing {
        /// The requested column name.
        name: String,
    },
    /// The `--class-rank` column is not a string (Utf8/LargeUtf8) column.
    #[error("class-rank column {name:?} is {data_type} but must be a string column")]
    ClassRankColumnNotString {
        /// The requested column name.
        name: String,
        /// The actual Arrow data type found.
        data_type: String,
    },
    /// The level plan is invalid (empty / non-monotonic / bad zoom range).
    #[error("invalid level specification: {0}")]
    InvalidLevels(String),
    /// A conversion knob carries a nonsensical value (non-finite or
    /// non-positive where a positive finite value is required).
    #[error("invalid option: {0}")]
    InvalidConfig(String),
    /// `--cluster` was requested in partitioning mode. A partitioning row is
    /// read at MANY display zooms (prefix reads, §2.3) but exists at exactly
    /// one level, so a single stored `point_count` cannot reflect "that
    /// level's grid" for every zoom it is displayed at — and absorbed
    /// features reappear as their own rows at finer levels while remaining
    /// counted in coarser winners, double-counting every prefix sum.
    #[error(
        "--cluster requires duplicating mode: a partitioning-mode feature has one \
         row read across many zoom prefixes, so a per-level point_count cannot be \
         represented without double counting"
    )]
    ClusterPartitioningUnsupported,
    /// `--accumulate-attribute` was supplied without `--cluster`.
    #[error("--accumulate-attribute requires --cluster")]
    AccumulateWithoutCluster,
    /// Multi-partition input (directory / glob) reached the in-memory
    /// reference pipeline, which reads through a single parquet builder.
    #[error(
        "multi-partition input requires the streaming pipeline; \
         remove --no-streaming"
    )]
    MultiPartitionRequiresStreaming,
    /// An `--accumulate-attribute` column is not present in the input schema.
    #[error("accumulate-attribute column {name:?} not found in input schema")]
    AccumulateColumnMissing {
        /// The requested column name.
        name: String,
    },
    /// An `--accumulate-attribute` column is not numeric.
    #[error(
        "accumulate-attribute column {name:?} is {data_type} but must be numeric \
         (int/uint/float)"
    )]
    AccumulateColumnNotNumeric {
        /// The requested column name.
        name: String,
        /// The actual Arrow data type found.
        data_type: String,
    },
    /// The input already contains a `point_count` column (clustering enabled).
    #[error(
        "input already contains a '{POINT_COUNT_COLUMN}' column; rename it before \
         converting with --cluster"
    )]
    PointCountColumnPresent,
    /// The input already contains a `coalesced_count` column (coalescing
    /// enabled).
    #[error(
        "input already contains a '{COALESCED_COUNT_COLUMN}' column; rename it \
         before converting with --coalesce-lines"
    )]
    CoalescedCountColumnPresent,
    /// The input has no features, or every feature was dropped from every level.
    #[error("no output rows produced (empty input or all features dropped)")]
    NoData,
    /// The strict cluster accounting / sum invariant (spec §12.1) was
    /// violated while building the per-level cluster tables: a level's
    /// `point_count` values would not partition the source point set (or a
    /// clustered level thinned its points to zero with source points left
    /// to absorb). This is a producer bug guard — a conforming conversion
    /// can never trip it.
    #[error("cluster invariant violated (spec §12.1): {0}")]
    ClusterInvariant(String),
}

/// Convert a GeoParquet file into a multi-resolution overview GeoParquet file.
///
/// See the module documentation for the pipeline. Returns a [`ConvertReport`]
/// describing the levels written.
/// Validate the numeric conversion knobs (H4 hostile-input hardening).
///
/// A NaN, infinite, or non-positive thinning factor (or GSD base) silently
/// degenerates the assignment grid — every cell-winner pass would skip every
/// feature — so nonsensical values are rejected up front with a clear error
/// instead of producing an "everything at the canonical level" file.
fn validate_options(options: &ConvertOptions) -> Result<(), ConvertError> {
    let positive = |name: &str, v: f64| {
        if !v.is_finite() || v <= 0.0 {
            return Err(ConvertError::InvalidConfig(format!(
                "{name} = {v} must be a finite value > 0"
            )));
        }
        Ok(())
    };
    let non_negative = |name: &str, v: f64| {
        if !v.is_finite() || v < 0.0 {
            return Err(ConvertError::InvalidConfig(format!(
                "{name} = {v} must be a finite value >= 0"
            )));
        }
        Ok(())
    };
    positive("gsd-base", options.gsd_base)?;
    positive("point-thinning", options.assign.point_thinning)?;
    positive("line-thinning", options.assign.line_thinning)?;
    positive("polygon-thinning", options.assign.polygon_thinning)?;
    non_negative("line-visibility", options.assign.line_visibility)?;
    non_negative("polygon-visibility", options.assign.polygon_visibility)?;
    // Negative snap / junction-angle values are documented OFF switches; only
    // NaN is meaningless.
    if options.coalesce_snap.is_nan() {
        return Err(ConvertError::InvalidConfig(
            "coalesce-snap must not be NaN (use <= 0 to disable snapping)".to_string(),
        ));
    }
    if options.coalesce_junction_angle.is_nan() {
        return Err(ConvertError::InvalidConfig(
            "coalesce-junction-angle must not be NaN (use 0 to disable)".to_string(),
        ));
    }
    if let Some(bb) = &options.bbox {
        if bb.iter().any(|v| !v.is_finite()) {
            return Err(ConvertError::InvalidConfig(format!(
                "bbox {bb:?} must contain only finite values"
            )));
        }
        if bb[0] > bb[2] || bb[1] > bb[3] {
            return Err(ConvertError::InvalidConfig(format!(
                "bbox {bb:?} must satisfy xmin <= xmax and ymin <= ymax"
            )));
        }
    }
    if options.in_flight_batches == 0 {
        return Err(ConvertError::InvalidConfig(
            "in-flight-batches must be >= 1".to_string(),
        ));
    }
    // #272: fail fast on a bad spill dir — the spill is best-effort, so a
    // mid-convert creation failure would only surface as a silent degrade
    // to network re-fetch.
    if let Some(dir) = &options.spill_dir {
        if !dir.is_dir() {
            return Err(ConvertError::InvalidConfig(format!(
                "spill-dir {} is not an existing directory",
                dir.display()
            )));
        }
    }
    Ok(())
}

pub fn convert_to_overviews(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &ConvertOptions,
) -> Result<ConvertReport, ConvertError> {
    // Local path, remote URL (s3://, https://, gs:// — #210), or a
    // multi-partition directory / glob pattern (v0.7): directories and
    // globs resolve to an ordered, validated set of local partitions read
    // as one logical dataset; remote objects are read with byte-range
    // requests through the same sync parquet plumbing, composing with the
    // bbox row-group pruning below so pruned row groups are never
    // downloaded at all.
    let source = ConvertSource::resolve_path(input_path.as_ref())?;
    convert_to_overviews_source_strategy(
        &source,
        output_path.as_ref(),
        options,
        super::stream::Pass2Strategy::Pipelined,
    )
}

/// [`convert_to_overviews`] over an already-resolved [`ConvertSource`] —
/// the entry point for callers that build the (possibly multi-partition)
/// source themselves: a `--files-from` manifest
/// ([`ConvertSource::from_manifest`]), an explicit input list
/// ([`ConvertSource::from_input_list`]), or custom object stores.
pub fn convert_to_overviews_sources(
    source: &ConvertSource,
    output_path: &Path,
    options: &ConvertOptions,
) -> Result<ConvertReport, ConvertError> {
    convert_to_overviews_source_strategy(
        source,
        output_path,
        options,
        super::stream::Pass2Strategy::Pipelined,
    )
}

/// [`convert_to_overviews`] over an already-resolved [`InputSource`] — the
/// entry point for callers that construct the source themselves (custom
/// object stores, tests over in-memory stores).
pub fn convert_to_overviews_source(
    source: &InputSource,
    output_path: &Path,
    options: &ConvertOptions,
) -> Result<ConvertReport, ConvertError> {
    // `InputSource` clones are cheap handles sharing caches and counters.
    convert_to_overviews_source_strategy(
        &ConvertSource::single(source.clone()),
        output_path,
        options,
        super::stream::Pass2Strategy::Pipelined,
    )
}

/// [`convert_to_overviews`] over a path with an explicit pass-2
/// [`Pass2Strategy`] — tests pin the serial reference strategy through the
/// exact production setup.
///
/// [`Pass2Strategy`]: super::stream::Pass2Strategy
#[cfg(test)]
pub(crate) fn convert_to_overviews_strategy(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &ConvertOptions,
    strategy: super::stream::Pass2Strategy,
) -> Result<ConvertReport, ConvertError> {
    let source = ConvertSource::resolve_path(input_path.as_ref())?;
    convert_to_overviews_source_strategy(&source, output_path.as_ref(), options, strategy)
}

/// Decode every geometry in `full` (row-aligned to the batch) and drop the rows
/// that cannot participate in level assignment: a null/empty/non-finite
/// geometry, or — under a regional extract (#102) — one whose bbox misses
/// `bbox_units`. The batch and the geometry vec are filtered together so every
/// downstream index stays aligned. Returns the filtered batch and its surviving
/// geometries.
fn decode_and_filter_geometries(
    full: RecordBatch,
    geom_idx: usize,
    geom_field: &Field,
    bbox_units: Option<&[f64; 4]>,
) -> Result<(RecordBatch, Vec<Geometry<f64>>), ConvertError> {
    let geom_array: Arc<dyn GeoArrowArray> =
        from_arrow_array(full.column(geom_idx).as_ref(), geom_field)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
    let mut geom_opts: Vec<Option<Geometry<f64>>> = Vec::with_capacity(full.num_rows());
    extract_geometries_opt_from_array(geom_array.as_ref(), &mut geom_opts)?;
    let mut geom_skipped = 0usize;
    let keep: Vec<bool> = geom_opts
        .iter()
        .map(|g| match g.as_ref().filter(|g| usable_geometry(g)) {
            // Regional extract (#102): drop features whose bbox misses the
            // requested region exactly, independent of row-group pruning.
            // (map_or, not is_none_or: the latter is stable only since Rust
            // 1.82 and the crate MSRV is 1.75.)
            #[allow(clippy::unnecessary_map_or)]
            Some(g) => bbox_units.map_or(true, |bb| bboxes_intersect(&geometry_bbox(g), bb)),
            None => {
                geom_skipped += 1;
                false
            }
        })
        .collect();
    let dropped = keep.iter().filter(|k| !**k).count();
    if dropped == 0 {
        return Ok((full, geom_opts.into_iter().flatten().collect()));
    }
    if geom_skipped > 0 {
        log::warn!(
            "skipping {geom_skipped} of {} input rows with a null, empty, or \
             non-finite geometry",
            full.num_rows()
        );
    }
    let mask = arrow_array::BooleanArray::from(keep.clone());
    let filtered = arrow_select::filter::filter_record_batch(&full, &mask)?;
    let geoms = geom_opts
        .into_iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(g, _)| g.expect("kept rows are Some"))
        .collect();
    Ok((filtered, geoms))
}

/// [`convert_to_overviews_source`] with an explicit pass-2 [`Pass2Strategy`].
/// Runs the full option normalization (validation, cluster/accumulate checks,
/// the partitioning-coalesce-inert rewrite) before dispatching.
pub(crate) fn convert_to_overviews_source_strategy(
    source: &ConvertSource,
    output_path: &Path,
    options: &ConvertOptions,
    strategy: super::stream::Pass2Strategy,
) -> Result<ConvertReport, ConvertError> {
    // Knob sanity (H4), shared by both pipelines.
    validate_options(options)?;
    // #272: place the remote-input disk spill (#219) where the caller asked
    // (no-op for local inputs, which never spill).
    source.set_spill_dir(options.spill_dir.as_deref());
    // Clustering option sanity (Q4), shared by both pipelines: partitioning
    // mode cannot represent per-level counts (see the error's rationale), and
    // aggregation is meaningless without clustering.
    if options.cluster && matches!(options.mode, Mode::Partitioning) {
        return Err(ConvertError::ClusterPartitioningUnsupported);
    }
    if !options.accumulate.is_empty() && !options.cluster {
        return Err(ConvertError::AccumulateWithoutCluster);
    }
    // Coalescing is INERT in partitioning mode (Q3, spec §13.5): a merged
    // chain is a new geometry replacing several source rows, which the
    // feature-once/verbatim contract of §2.3 cannot represent, and removing
    // merged members from finer bands would break prefix reads. Coalescing
    // is on by default, so partitioning conversions silently proceed
    // without it (no column, no provenance); the CLI rejects an EXPLICIT
    // request instead.
    let inert_options: ConvertOptions;
    let options: &ConvertOptions =
        if options.coalesce_lines && matches!(options.mode, Mode::Partitioning) {
            log::info!(
                "line coalescing is inert in partitioning mode (feature-once / \
                 geometry-verbatim contract); converting without it"
            );
            inert_options = ConvertOptions {
                coalesce_lines: false,
                ..options.clone()
            };
            &inert_options
        } else {
            options
        };

    // Two-pass bounded-memory pipeline (H3, default). The in-memory path below
    // is kept as the reference implementation (`streaming: false`).
    if options.streaming {
        return super::stream::convert_streaming_strategy(source, output_path, options, strategy);
    }

    // The in-memory reference path below predates multi-partition input
    // (v0.7) and reads through one parquet builder; multi sources are
    // streaming-only.
    let source_single: &InputSource = match source {
        ConvertSource::Single(s) => s.input(),
        ConvertSource::Multi(_) => return Err(ConvertError::MultiPartitionRequiresStreaming),
    };

    let start = Instant::now();

    // A numeric sort key and a categorical class ranking are mutually
    // exclusive (Q1): they would both drive `AssignFeature::sort_key`.
    if options.sort_key.is_some() && options.class_ranking.is_some() {
        return Err(ConvertError::RankingConflict);
    }

    // --- Read the input footer, preserving the full property schema. ---------
    // (For a remote source, the footer is range-fetched once and cached.)
    let builder = source_single.open()?;
    // `read_schema` matches the raw batches read below; `input_schema` is the
    // possibly-renamed schema used for every downstream (name-based) lookup.
    let read_schema = builder.schema().clone();

    // --- CRS detection + rejection (spec Q3) — footer metadata only. ---------
    let crs = detect_crs_from_kv(builder.metadata().file_metadata().key_value_metadata())?;

    // Reserved-column collisions (#288): rename any input column named `level`
    // / `point_count` / `coalesced_count` (case-insensitive) out of the way
    // rather than reject the file, keeping the reserved output columns
    // authoritative. `options` is cloned so by-name ranking/accumulate options
    // can be rewritten to the renamed columns. The rename preserves column
    // order, so `read_schema` and `input_schema` share indices.
    let mut options = options.clone();
    let (input_schema, _renames) = resolve_reserved_column_collisions(&read_schema, &mut options);
    let options = &options;

    let geom_idx = find_geometry_column(&input_schema).ok_or(ConvertError::NoGeometryColumn)?;
    let geom_field = input_schema.field(geom_idx).clone();

    // Clustering schema checks + accumulate column resolution (Q4).
    let acc_cols = validate_cluster_schema(&input_schema, options)?;
    // Coalescing schema check (Q3).
    validate_coalesce_schema(&input_schema, options)?;

    // Regional extract (#102): prune input row groups by bbox covering
    // statistics (footer-only), before any data pages are read. Groups
    // without stats are kept; the exact per-feature filter below guarantees
    // identical output either way.
    let row_groups_total = builder.metadata().num_row_groups();
    let bbox_units = options.bbox.map(|b| bbox_to_crs_units(&b, crs));
    let (builder, row_groups_read) = match &bbox_units {
        Some(bb) => {
            let sel = select_input_row_groups(builder.metadata(), bb);
            let n = sel.len();
            log::info!("bbox filter: reading {n}/{row_groups_total} input row groups");
            (builder.with_row_groups(sel), n)
        }
        None => (builder, row_groups_total),
    };

    let reader = builder.build()?;
    let mut batches: Vec<RecordBatch> = Vec::new();
    for batch in reader {
        batches.push(batch?);
    }
    // Concat with `read_schema` (the raw batches carry the original names).
    let full = concat_batches(&read_schema, &batches)?;

    // Decode geometries once (in-memory v1), row-aligned. Rows with a null,
    // empty, or non-finite geometry cannot participate in level assignment:
    // they are dropped, from the batch and the geometry vec together, so every
    // downstream index stays aligned (H4: an interleaved null geometry must
    // never shift attributes onto a neighboring row's geometry).
    let (full, geometries) =
        decode_and_filter_geometries(full, geom_idx, &geom_field, bbox_units.as_ref())?;

    // Apply the reserved-column renames (#288) to the in-memory table. Columns
    // are positional, so re-associating them with the renamed schema is a
    // metadata-only relabel (a no-op when nothing collided).
    let full = if Arc::ptr_eq(&input_schema, &read_schema) {
        full
    } else {
        RecordBatch::try_new(input_schema.clone(), full.columns().to_vec())?
    };
    let num_features = full.num_rows();

    // Resolve the cell-winner ranking (Q1): explicit sort key / explicit class
    // ranking / auto-detected well-known schema / size fallback. Returns the
    // per-feature sort keys and the provenance recorded in the footer (§3.5).
    let (sort_keys, ranking_provenance) =
        resolve_ranking(&input_schema, &full, &geometries, options)?;

    // --- Coalescing groups (Q3): interned class values, when class-ranked. ---
    let num_lines = geometries
        .iter()
        .filter(|g| feature_kind(g) == FeatureKind::Line)
        .count();
    let coalesce_on = coalesce_effective(options, num_lines);
    let line_groups: Option<Vec<u32>> = if coalesce_on {
        coalesce_group_column(&ranking_provenance).map(|col| {
            let idx = input_schema.index_of(col).expect("ranking column exists");
            let mut interner = GroupInterner::default();
            let mut groups = Vec::with_capacity(full.num_rows());
            interner.extend(full.column(idx).as_ref(), &mut groups);
            groups
        })
    } else {
        None
    };

    // --- Level assignment. ---------------------------------------------------
    let level_specs = options.levels.resolve(options.gsd_base)?;
    let level_gsds: Vec<f64> = level_specs.iter().map(|(g, _)| *g).collect();

    let features: Vec<AssignFeature> = geometries
        .iter()
        .enumerate()
        .map(|(i, g)| AssignFeature {
            index: i,
            bbox: geometry_bbox(g),
            kind: feature_kind(g),
            sort_key: sort_keys[i],
        })
        .collect();

    // #188 follow-up: count antimeridian-suspect bboxes and warn once.
    let antimeridian_suspect_features = features
        .iter()
        .filter(|f| bbox_antimeridian_suspect(&f.bbox, crs))
        .count();
    warn_antimeridian_suspects(antimeridian_suspect_features);

    let assignment = assign_levels(&features, &level_gsds, &options.assign, crs);
    // Q2: layer the per-level density budget on top of cell-winner thinning.
    // When disabled this is an identity, so `--no-density-drop` reproduces the
    // pre-Q2 assignment (and, since no density_drop provenance is emitted, a
    // byte-identical footer).
    let assignment = if options.density.enabled {
        apply_density_budget(
            &assignment,
            &features,
            &level_gsds,
            &options.assign,
            &options.density,
            crs,
        )
    } else {
        assignment
    };
    let num_levels = level_gsds.len();
    let finest = num_levels.saturating_sub(1);

    // --- Cluster tables (Q4): per level, winner → point_count + aggregates. --
    let cluster_tables = if options.cluster {
        let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();
        let acc_values = extract_accumulate_values(&full, &acc_cols);
        let ops: Vec<_> = options.accumulate.iter().map(|s| s.op).collect();
        let tables = build_cluster_tables(
            &features,
            &min_levels,
            &level_gsds,
            &options.assign,
            crs,
            &acc_values,
            &ops,
        );
        // Strict §12.1 accounting: Σ point_count per level == source point
        // count, and no clustered level thins its points to zero.
        verify_sum_invariant(&features, &min_levels, &tables)
            .map_err(ConvertError::ClusterInvariant)?;
        Some(tables)
    } else {
        None
    };

    // --- Build per-level generalized selections (coarse→fine). ---------------
    // Each emitted entry: (spec, feature indices, geometries, vertex_count).
    struct EmittedLevel {
        /// Index in the resolved level plan (cluster-table key; may differ
        /// from the emitted index when empty levels are omitted, §7.3).
        orig: usize,
        gsd: f64,
        zoom: Option<u8>,
        indices: Vec<usize>,
        geoms: Vec<Geometry<f64>>,
        vertex_count: usize,
        /// Coalescing (Q3): this level's chain table (rep row → merged
        /// geometry + member count). `None` at non-coalesced levels.
        coalesce: Option<CoalesceTable>,
    }
    let mut emitted: Vec<EmittedLevel> = Vec::new();
    let mut skipped: Vec<SkippedLevelReport> = Vec::new();

    for (level, &(gsd_m, zoom)) in level_specs.iter().enumerate() {
        let member_indices: Vec<usize> = match options.mode {
            Mode::Duplicating => assignment.duplicating_at_level(level as u8),
            Mode::Partitioning => assignment.partitioning_at_level(level as u8),
        };

        // Verbatim path: partitioning at every level (§2.3), and duplicating at
        // the canonical (finest) level (§2.4). Otherwise simplify per feature.
        let verbatim = matches!(options.mode, Mode::Partitioning) || level == finest;

        // Coalescing (Q3): at non-canonical duplicating levels, ALL line rows
        // enter the per-level chain stage (pre-gate, pre-thinning — chains of
        // sub-visibility fragments must be reclaimable) and the winner-table
        // path handles only the non-line rows. The chain table's rep rows are
        // added back below with their merged, pre-simplified geometry.
        let coalesce: Option<CoalesceTable> = if coalesce_on && !verbatim {
            let inputs: Vec<CoalesceInput<'_>> = features
                .iter()
                .filter(|f| f.kind == FeatureKind::Line)
                .map(|f| CoalesceInput {
                    index: f.index,
                    geom: &geometries[f.index],
                    sort_key: f.sort_key,
                    group: line_groups.as_ref().map_or(0, |g| g[f.index]),
                })
                .collect();
            Some(build_level_coalesce_table(
                &inputs, level, finest, gsd_m, crs, options,
            ))
        } else {
            None
        };
        let member_indices: Vec<usize> = if let Some(table) = &coalesce {
            let mut v: Vec<usize> = member_indices
                .into_iter()
                .filter(|&i| features[i].kind != FeatureKind::Line)
                .collect();
            v.extend(table.keys().copied());
            v.sort_unstable();
            v
        } else {
            member_indices
        };

        let mut indices = Vec::with_capacity(member_indices.len());
        let mut geoms = Vec::with_capacity(member_indices.len());
        let mut vertex_count = 0usize;

        // Cascading (#218, duplicating default): fold canonical geometry
        // through the fine→coarse GSD chain ending at this level, so this
        // level consumes the next-finer level's output. Same chain the
        // streaming ctxs build — the paths stay in lockstep.
        let cascade_chain: Vec<f64> = if options.simplify.cascade && !verbatim {
            level_specs[level..finest]
                .iter()
                .rev()
                .map(|&(g, _)| g)
                .collect()
        } else {
            Vec::new()
        };

        if verbatim {
            for i in member_indices {
                let g = &geometries[i];
                vertex_count += count_vertices(g);
                indices.push(i);
                geoms.push(g.clone());
            }
        } else {
            for i in member_indices {
                // Chain reps carry their merged, already-simplified geometry.
                if let Some((g, _)) = coalesce.as_ref().and_then(|t| t.get(&i)) {
                    vertex_count += count_vertices(g);
                    indices.push(i);
                    geoms.push(g.clone());
                    continue;
                }
                let simplified = if cascade_chain.is_empty() {
                    simplify_for_level(&geometries[i], gsd_m, crs, &options.simplify)
                } else {
                    simplify_cascade(&geometries[i], &cascade_chain, crs, &options.simplify)
                };
                match simplified {
                    Simplified::Keep(g) => {
                        vertex_count += count_vertices(&g);
                        indices.push(i);
                        geoms.push(g);
                    }
                    Simplified::Dropped => {}
                }
            }
        }

        // Empty levels are not allowed (§7.3): omit and renumber (#211
        // auto-clamp), recording the omission for the report + warning.
        if indices.is_empty() {
            skipped.push(SkippedLevelReport {
                planned_level: level,
                gsd: gsd_m,
                zoom,
            });
            continue;
        }
        emitted.push(EmittedLevel {
            orig: level,
            gsd: gsd_m,
            zoom,
            indices,
            geoms,
            vertex_count,
            coalesce,
        });
    }

    if emitted.is_empty() {
        return Err(ConvertError::NoData);
    }
    warn_plan_skipped_levels(&skipped, num_features, emitted[0].gsd, emitted[0].zoom);

    // --- Build the output writer schema (source schema + geoarrow geometry). -
    let geom_name = geom_field.name().clone();
    // A fresh mixed-Geometry field carries the geoarrow extension the writer /
    // geoparquet encoder detect; each level's geometry array is built as the
    // same type so RecordBatch assembly matches.
    let geom_out_field = mixed_geometry_field(&geom_name);
    let source_schema = build_source_schema(&input_schema, geom_idx, geom_out_field.clone());
    // Writer schema: base + point_count when clustering (Q4) + coalesced_count
    // when coalescing (Q3).
    let cluster_schema = if options.cluster {
        append_point_count_field(&source_schema)
    } else {
        source_schema.clone()
    };
    let out_schema = if options.coalesce_lines {
        append_coalesced_count_field(&cluster_schema)
    } else {
        cluster_schema.clone()
    };

    let writer_levels: Vec<LevelSpec> = emitted
        .iter()
        .map(|e| LevelSpec::new(e.gsd, e.zoom))
        .collect();
    let emitted_gsds: Vec<f64> = emitted.iter().map(|e| e.gsd).collect();
    let mut writer_opts = OverviewWriterOptions::new(options.mode, writer_levels);
    writer_opts.max_row_group_size = options.max_row_group_size;
    writer_opts.row_group_size_policy = options.row_group_size_policy;
    writer_opts.full_column_stats = options.full_column_stats;
    writer_opts.cogp_compat_key = options.cogp_compat_key;
    writer_opts.generalization = Some(build_generalization(
        &emitted_gsds,
        crs,
        options,
        ranking_provenance,
    ));

    let mut writer = OverviewWriter::create(output_path, &out_schema, writer_opts)?;

    // Column indices of the non-geometry source columns (preserve original order).
    let non_geom_cols: Vec<usize> = (0..input_schema.fields().len())
        .filter(|&c| c != geom_idx)
        .collect();

    let mut level_reports = Vec::with_capacity(emitted.len());
    for (level_idx, e) in emitted.iter().enumerate() {
        let mut batch = build_level_batch(
            &source_schema,
            &full,
            &non_geom_cols,
            geom_idx,
            &e.indices,
            &e.geoms,
        )?;
        if let Some(tables) = &cluster_tables {
            // Canonical level: singleton clusters, columns verbatim (§2.4).
            let table = (e.orig != finest).then(|| &tables[e.orig]);
            batch = apply_cluster_columns(batch, &cluster_schema, &e.indices, table, &acc_cols)?;
        }
        if options.coalesce_lines {
            // Canonical level (and guard-skipped runs): table is None ⇒ all 1.
            batch = apply_coalesced_count(batch, &out_schema, &e.indices, e.coalesce.as_ref())?;
        }
        // SkippedEmpty is unreachable here (every emitted level has >= 1
        // feature), but the bookkeeping stays aligned with the streaming path.
        let outcome =
            writer.write_level(level_idx, Some(e.indices.len()), std::iter::once(batch))?;
        record_level_outcome(
            outcome,
            SkippedLevelReport {
                planned_level: e.orig,
                gsd: e.gsd,
                zoom: e.zoom,
            },
            e.indices.len(),
            e.indices.len(),
            e.vertex_count,
            &mut level_reports,
            &mut skipped,
        );
    }
    skipped.sort_by_key(|s| s.planned_level);

    let meta = writer.finish()?;

    // --- Fill in real per-level byte sizes from the output footer. -----------
    fill_level_bytes(output_path, &meta, &mut level_reports)?;

    let total_rows: usize = level_reports.iter().map(|l| l.feature_count).sum();
    let total_vertices: usize = level_reports.iter().map(|l| l.vertex_count).sum();
    let total_compressed_bytes: i64 = level_reports.iter().map(|l| l.compressed_bytes).sum();

    Ok(ConvertReport {
        mode: options.mode,
        levels: level_reports,
        skipped_empty_levels: skipped,
        input_features: num_features,
        total_rows,
        total_vertices,
        total_compressed_bytes,
        row_groups_total,
        row_groups_read,
        antimeridian_suspect_features,
        duration_secs: start.elapsed().as_secs_f64(),
        remote_fetch: log_remote_fetch(source),
    })
}

/// Snapshot (and `log::info!`) the remote fetch counters at the end of a
/// conversion; `None` (and silent) when no part of the input is remote.
/// Multi-part sources report counters summed over their remote parts.
pub(super) fn log_remote_fetch(source: &ConvertSource) -> Option<crate::input::FetchStats> {
    let stats = source.fetch_stats()?;
    let pct = if stats.object_size > 0 {
        100.0 * stats.bytes_fetched as f64 / stats.object_size as f64
    } else {
        0.0
    };
    log::info!(
        "remote input: {} range requests, {:.2} MiB fetched of a {:.2} MiB object ({:.1}%)",
        stats.requests,
        stats.bytes_fetched as f64 / (1024.0 * 1024.0),
        stats.object_size as f64 / (1024.0 * 1024.0),
        pct
    );
    Some(stats)
}

// ============================================================================
// Helpers
// ============================================================================

/// Detect the input CRS from parsed parquet key-value metadata and map it to
/// [`Crs`], rejecting anything that is not EPSG:4326 or EPSG:3857 (spec Q3).
/// Metadata-only so remote inputs (#210) pay no extra footer fetch.
pub(crate) fn detect_crs_from_kv(
    kv: Option<&Vec<parquet::file::metadata::KeyValue>>,
) -> Result<Crs, ConvertError> {
    let info = crate::quality::crs_info_from_kv_metadata(kv)?;
    if info.is_wgs84 {
        return Ok(Crs::Epsg4326);
    }
    if let Some(id) = &info.identifier {
        let up = id.to_uppercase();
        if up.contains("3857") || up.contains("900913") {
            return Ok(Crs::Epsg3857);
        }
    }
    Err(ConvertError::UnsupportedCrs {
        crs: info
            .identifier
            .clone()
            .or_else(|| info.name.clone())
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

/// Half the Web Mercator world extent in meters (`±` = the x/y range of
/// EPSG:3857). Matches the constant used by the export reprojection.
const WEBMERC_HALF_M: f64 = 20_037_508.342_789_244;

/// Web Mercator latitude clamp (the projection diverges at the poles).
const WEBMERC_MAX_LAT: f64 = 85.051_128_779_806_59;

/// Reproject one EPSG:4326 point (lon/lat degrees) to EPSG:3857 (meters) —
/// the exact inverse of the export path's `webmerc_to_lnglat`. Latitude is
/// clamped to the projection's valid range.
#[inline]
fn lnglat_to_webmerc(lng: f64, lat: f64) -> (f64, f64) {
    use std::f64::consts::{FRAC_PI_4, PI};
    let x = lng / 180.0 * WEBMERC_HALF_M;
    let lat = lat.clamp(-WEBMERC_MAX_LAT, WEBMERC_MAX_LAT);
    let y = (FRAC_PI_4 + lat.to_radians() / 2.0).tan().ln() / PI * WEBMERC_HALF_M;
    (x, y)
}

/// Express a `[xmin, ymin, xmax, ymax]` EPSG:4326 bbox in the input file's
/// coordinate units ([`ConvertOptions::bbox`] is always lon/lat degrees; a
/// 3857 input stores meters).
pub(super) fn bbox_to_crs_units(bbox: &[f64; 4], crs: Crs) -> [f64; 4] {
    match crs {
        Crs::Epsg4326 => *bbox,
        Crs::Epsg3857 => {
            let (xmin, ymin) = lnglat_to_webmerc(bbox[0], bbox[1]);
            let (xmax, ymax) = lnglat_to_webmerc(bbox[2], bbox[3]);
            [xmin, ymin, xmax, ymax]
        }
    }
}

/// Closed-interval AABB intersection of two `[xmin, ymin, xmax, ymax]` boxes
/// (touching edges count as intersecting, matching
/// [`crate::covering::RowGroupBounds::intersects`]).
pub(super) fn bboxes_intersect(a: &[f64; 4], b: &[f64; 4]) -> bool {
    a[0] <= b[2] && a[2] >= b[0] && a[1] <= b[3] && a[3] >= b[1]
}

/// Row groups of the input whose bbox covering statistics intersect
/// `bbox_units` (`[xmin, ymin, xmax, ymax]` in the file's CRS units).
///
/// Statistics-only: operates purely on the parsed parquet footer
/// ([`crate::covering::extract_row_group_bounds_from_metadata`]); no data
/// pages are touched. Row groups with missing/unparseable covering
/// statistics are conservatively KEPT (graceful degradation — the exact
/// per-feature bbox filter downstream guarantees correctness either way).
pub(crate) fn select_input_row_groups(
    metadata: &parquet::file::metadata::ParquetMetaData,
    bbox_units: &[f64; 4],
) -> Vec<usize> {
    let bounds = crate::covering::extract_row_group_bounds_from_metadata(metadata)
        .unwrap_or_else(|_| vec![None; metadata.num_row_groups()]);
    let filter = crate::tile::TileBounds {
        lng_min: bbox_units[0],
        lat_min: bbox_units[1],
        lng_max: bbox_units[2],
        lat_max: bbox_units[3],
    };
    (0..metadata.num_row_groups())
        .filter(|&i| match bounds.get(i).and_then(|b| b.as_ref()) {
            Some(b) => b.intersects(&filter),
            None => true, // no stats — must read to stay correct
        })
        .collect()
}

/// Find the primary geometry column index (name `geometry`, else first `geom*`).
pub(super) fn find_geometry_column(schema: &Schema) -> Option<usize> {
    schema
        .fields()
        .iter()
        .position(|f| f.name() == "geometry")
        .or_else(|| {
            schema
                .fields()
                .iter()
                .position(|f| f.name().contains("geom"))
        })
}

/// `[xmin, ymin, xmax, ymax]` of a geometry (`[0;4]` when the bbox is undefined).
pub(super) fn geometry_bbox(g: &Geometry<f64>) -> [f64; 4] {
    match g.bounding_rect() {
        Some(r) => [r.min().x, r.min().y, r.max().x, r.max().y],
        None => [0.0, 0.0, 0.0, 0.0],
    }
}

/// Whether a feature bbox is antimeridian-suspect: wider than 180° of
/// longitude. A real feature that wide is essentially impossible; the near
/// certain cause is an antimeridian-crossing geometry stored verbatim, whose
/// min/max bbox inflates to ~360° (see `context/ANTIMERIDIAN.md`, #188).
/// Detection only — geometry is never mutated.
pub(super) fn bbox_antimeridian_suspect(bbox: &[f64; 4], crs: Crs) -> bool {
    bbox[2] - bbox[0] > crs.meters_to_units(180.0 * METERS_PER_DEGREE)
}

/// Emit the single aggregate antimeridian warning (#188 follow-up). Called
/// once per convert, from both the streaming and in-memory paths.
pub(super) fn warn_antimeridian_suspects(count: usize) {
    if count > 0 {
        log::warn!(
            "{count} feature(s) have bounding boxes wider than 180° of longitude — \
             likely antimeridian-crossing geometry. These will be assigned to \
             overly coarse levels and defeat bbox pruning; pre-split them at \
             ±180° before converting (see docs/advanced-usage.md, \
             \"Antimeridian-Crossing Geometry\")."
        );
    }
}

/// Object-size threshold above which a *full-file* remote convert emits the
/// #267 download-first nudge. Below ~1 GiB the local spill footprint and the
/// second-pass disk read are cheap enough not to warrant a warning.
pub(super) const FULL_FILE_REMOTE_WARN_BYTES: u64 = 1 << 30;

/// #267 decision + message (pure, so it is unit-testable without capturing
/// logs). Returns the one-line nudge to emit, or `None` to stay quiet.
///
/// Fires only for a *whole-file* remote convert of a large object: the disk
/// spill (#219) already bounds network traffic to ≈1× the object, but a
/// full-file remote convert still stages ≈the object's bytes under `$TMPDIR`
/// and re-reads them from disk on the second pass. For a region of interest,
/// `--bbox` fetches only the covering row groups and skips the spill entirely,
/// so we point the user there (or to a download-first workflow). Stays quiet
/// for local inputs (OS page cache), for effective bbox extracts (fewer row
/// groups read than the file holds), and for objects below the threshold.
pub(super) fn full_file_remote_warning(
    remote_parts: usize,
    row_groups_read: usize,
    row_groups_total: usize,
    object_size: u64,
) -> Option<String> {
    if remote_parts == 0
        || row_groups_read < row_groups_total
        || object_size < FULL_FILE_REMOTE_WARN_BYTES
    {
        return None;
    }
    let gib = object_size as f64 / (1024.0 * 1024.0 * 1024.0);
    // Multi-partition sources (v0.7) sum object_size over their remote
    // parts, so name the part count: 20 × 600 MB partitions trip the same
    // ≥1 GiB total-transfer threshold as one 12 GiB object.
    let what = if remote_parts > 1 {
        format!("{remote_parts} remote partitions totalling {gib:.1} GiB")
    } else {
        format!("a {gib:.1} GiB object")
    };
    Some(format!(
        "full-file remote convert of {what}: the input is fetched once over \
         the network (≈1× — the local spill keeps later passes off the network, \
         #219) and staged under $TMPDIR. For a region of interest pass --bbox to \
         fetch only the covering row groups (and skip the spill); otherwise \
         downloading first (e.g. `aws s3 cp`) and converting locally avoids the \
         second-pass disk read. Point --spill-dir (or $TMPDIR) at fast local disk \
         with room for it, not a small tmpfs.",
    ))
}

/// Emit the #267 nudge for a full-file remote convert, if warranted. Thin
/// logging wrapper over [`full_file_remote_warning`] (for a multi source,
/// `object_size` is summed over the remote parts and the part count is
/// named in the message).
pub(super) fn warn_full_file_remote(
    source: &ConvertSource,
    row_groups_read: usize,
    row_groups_total: usize,
) {
    let object_size = source.fetch_stats().map_or(0, |s| s.object_size);
    let remote_parts = source.parts().iter().filter(|p| p.is_remote()).count();
    if let Some(msg) =
        full_file_remote_warning(remote_parts, row_groups_read, row_groups_total, object_size)
    {
        log::warn!("{msg}");
    }
}

/// #272 spill-preflight safety margin: warn when the spill volume's free
/// space is below the projected spill size plus 1/20th (5%) of it — spill
/// bookkeeping is exact but the volume is shared with everything else the
/// process (and host) writes during the convert.
const SPILL_MARGIN_DENOM: u64 = 20;

/// #272 decision + message (pure, so it is unit-testable without touching
/// a filesystem). Returns the warning to emit when the projected input
/// spill (≈ the selected input bytes, see
/// [`crate::input::selected_compressed_bytes`]) plus a 5% safety margin
/// exceeds `available_bytes` on the spill volume, or `None` when it fits.
pub(super) fn spill_space_warning(
    estimated_spill_bytes: u64,
    available_bytes: u64,
    spill_dir: &Path,
) -> Option<String> {
    let need = estimated_spill_bytes + estimated_spill_bytes / SPILL_MARGIN_DENOM;
    if available_bytes >= need {
        return None;
    }
    let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    Some(format!(
        "projected input spill (≈{:.1} GiB — the selected input bytes are \
         staged on local disk so later passes stay off the network, #219) may \
         not fit: {} has {:.1} GiB free ({:.1} GiB short, including a 5% \
         margin). If the volume fills mid-convert the spill degrades to \
         network re-fetch; pass --spill-dir (spill_dir) to place it on a \
         roomier volume, or free up space first.",
        gib(estimated_spill_bytes),
        spill_dir.display(),
        gib(available_bytes),
        gib(need - available_bytes),
    ))
}

/// #272 preflight gate over [`spill_space_warning`], with the free-space
/// probe injected so the gating is unit-testable with a fake probe. Only a
/// remote input spills, so for local inputs (and an empty selection) the
/// probe is never even called; a failed probe (`None` — unsupported
/// filesystem, permission error) stays quiet rather than crying wolf.
pub(super) fn spill_space_check(
    is_remote: bool,
    estimated_spill_bytes: u64,
    spill_dir: &Path,
    probe: impl FnOnce(&Path) -> Option<u64>,
) -> Option<String> {
    if !is_remote || estimated_spill_bytes == 0 {
        return None;
    }
    let available = probe(spill_dir)?;
    spill_space_warning(estimated_spill_bytes, available, spill_dir)
}

/// Free bytes available to the current user on the volume holding `dir`
/// (statvfs / GetDiskFreeSpaceEx via `fs4`), or `None` if the probe fails.
/// Compiled to a stub without the `remote` feature — nothing spills there,
/// and [`spill_space_check`] never probes for a local input.
fn probe_available_space(dir: &Path) -> Option<u64> {
    #[cfg(feature = "remote")]
    {
        fs4::available_space(dir).ok()
    }
    #[cfg(not(feature = "remote"))]
    {
        let _ = dir;
        None
    }
}

/// Emit the #272 spill free-space preflight warning, if warranted. Thin
/// logging wrapper over [`spill_space_check`] with the real filesystem
/// probe; `spill_dir = None` means the process temp dir, exactly where the
/// spill file would go.
pub(super) fn warn_spill_space(
    source: &ConvertSource,
    estimated_spill_bytes: u64,
    spill_dir: Option<&Path>,
) {
    let dir = spill_dir.map_or_else(std::env::temp_dir, Path::to_path_buf);
    if let Some(msg) = spill_space_check(
        source.is_remote(),
        estimated_spill_bytes,
        &dir,
        probe_available_space,
    ) {
        log::warn!("{msg}");
    }
}

/// Whether a decoded geometry can participate in level assignment: it must
/// carry at least one coordinate and every coordinate must be finite.
///
/// Rows failing this check (alongside null-geometry rows) are **skipped with
/// a warning** rather than converted: an empty geometry has no location to
/// thin against, and a NaN/infinite coordinate would silently collapse into
/// grid cell `(0, 0)` (H4 hostile-input hardening).
pub(super) fn usable_geometry(g: &Geometry<f64>) -> bool {
    use geo::coords_iter::CoordsIter;
    let mut any = false;
    for c in g.coords_iter() {
        if !c.x.is_finite() || !c.y.is_finite() {
            return false;
        }
        any = true;
    }
    any
}

/// Map a geometry to the [`FeatureKind`] used for thinning / visibility.
pub(super) fn feature_kind(g: &Geometry<f64>) -> FeatureKind {
    match g {
        Geometry::Point(_) | Geometry::MultiPoint(_) => FeatureKind::Point,
        Geometry::LineString(_) | Geometry::MultiLineString(_) | Geometry::Line(_) => {
            FeatureKind::Line
        }
        _ => FeatureKind::Polygon,
    }
}

/// Count coordinates (vertices) in a geometry.
pub(super) fn count_vertices(g: &Geometry<f64>) -> usize {
    use geo::coords_iter::CoordsIter;
    g.coords_count()
}

/// Extract an optional f64 sort key per row from a numeric Arrow column.
/// Non-numeric columns and null values yield `None`.
pub(super) fn extract_sort_keys(col: &dyn Array) -> Vec<Option<f64>> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type,
        UInt32Type, UInt64Type, UInt8Type,
    };
    use arrow_schema::DataType;

    let n = col.len();
    macro_rules! collect_prim {
        ($ty:ty) => {{
            let a = col.as_primitive::<$ty>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(a.value(i) as f64)
                    }
                })
                .collect()
        }};
    }
    match col.data_type() {
        DataType::Int8 => collect_prim!(Int8Type),
        DataType::Int16 => collect_prim!(Int16Type),
        DataType::Int32 => collect_prim!(Int32Type),
        DataType::Int64 => collect_prim!(Int64Type),
        DataType::UInt8 => collect_prim!(UInt8Type),
        DataType::UInt16 => collect_prim!(UInt16Type),
        DataType::UInt32 => collect_prim!(UInt32Type),
        DataType::UInt64 => collect_prim!(UInt64Type),
        DataType::Float32 => collect_prim!(Float32Type),
        DataType::Float64 => collect_prim!(Float64Type),
        _ => vec![None; n],
    }
}

/// A mixed-`Geometry` GeoArrow field carrying the geoarrow extension metadata.
pub(super) fn mixed_geometry_field(name: &str) -> Arc<Field> {
    use geoarrow_array::GeoArrowArray;
    let typ = GeometryType::new(Default::default());
    let empty = GeometryBuilder::new(typ).with_prefer_multi(false).finish();
    Arc::new(empty.data_type().to_field(name, true))
}

/// Build the writer source schema: original fields, geometry field replaced by
/// the geoarrow-typed field, no file-level metadata (the encoder regenerates it).
pub(super) fn build_source_schema(
    input_schema: &Schema,
    geom_idx: usize,
    geom_out_field: Arc<Field>,
) -> Schema {
    let fields: Vec<Arc<Field>> = input_schema
        .fields()
        .iter()
        .enumerate()
        .map(|(i, f)| {
            if i == geom_idx {
                geom_out_field.clone()
            } else {
                f.clone()
            }
        })
        .collect();
    Schema::new(fields)
}

/// Assemble one level's record batch: non-geometry columns via `take` on the
/// selected indices (preserving input order), geometry rebuilt from `geoms`.
pub(super) fn build_level_batch(
    source_schema: &Schema,
    full: &RecordBatch,
    non_geom_cols: &[usize],
    geom_idx: usize,
    indices: &[usize],
    geoms: &[Geometry<f64>],
) -> Result<RecordBatch, ConvertError> {
    let take_idx = UInt32Array::from(indices.iter().map(|&i| i as u32).collect::<Vec<_>>());

    // Build columns in the source schema order (geometry field kept in place).
    let mut columns: Vec<Arc<dyn Array>> = Vec::with_capacity(source_schema.fields().len());
    let mut non_geom_iter = non_geom_cols.iter();
    for i in 0..source_schema.fields().len() {
        if i == geom_idx {
            let typ = GeometryType::new(Default::default());
            let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
            b.extend_from_iter(geoms.iter().map(Some));
            columns.push(b.finish().to_array_ref());
        } else {
            let src_col = *non_geom_iter.next().expect("non-geom column index");
            let taken = take(full.column(src_col).as_ref(), &take_idx, None)?;
            columns.push(taken);
        }
    }
    Ok(RecordBatch::try_new(
        Arc::new(source_schema.clone()),
        columns,
    )?)
}

// ============================================================================
// Clustering helpers (Q4) — shared by the in-memory and streaming pipelines
// ============================================================================

/// Validate the clustering-related schema constraints and resolve the
/// accumulate columns to schema indices (parallel to `options.accumulate`).
///
/// Checks (clustering enabled only):
/// - the input does not already carry a `point_count` column
///   (case-insensitive, mirroring the `level` column rule §4.1);
/// - every accumulate column exists and is numeric.
pub(super) fn validate_cluster_schema(
    schema: &Schema,
    options: &ConvertOptions,
) -> Result<Vec<usize>, ConvertError> {
    if !options.cluster {
        return Ok(Vec::new());
    }
    // Backstop: both pipelines run `resolve_reserved_column_collisions` first,
    // which renames any colliding `point_count` away (#288), so this normally
    // never fires — it guards a direct caller that skipped the resolver.
    if schema
        .fields()
        .iter()
        .any(|f| f.name().eq_ignore_ascii_case(POINT_COUNT_COLUMN))
    {
        return Err(ConvertError::PointCountColumnPresent);
    }
    let mut indices = Vec::with_capacity(options.accumulate.len());
    for spec in &options.accumulate {
        let idx =
            schema
                .index_of(&spec.column)
                .map_err(|_| ConvertError::AccumulateColumnMissing {
                    name: spec.column.clone(),
                })?;
        let dt = schema.field(idx).data_type();
        if !is_numeric_type(dt) {
            return Err(ConvertError::AccumulateColumnNotNumeric {
                name: spec.column.clone(),
                data_type: format!("{dt:?}"),
            });
        }
        indices.push(idx);
    }
    Ok(indices)
}

/// Whether an Arrow type is accepted by `--accumulate-attribute`.
fn is_numeric_type(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float32
            | DataType::Float64
    )
}

/// The writer source schema with the trailing `point_count` INT64 NOT NULL
/// column appended (clustering enabled).
pub(super) fn append_point_count_field(schema: &Schema) -> Schema {
    let mut fields: Vec<Arc<Field>> = schema.fields().iter().cloned().collect();
    fields.push(Arc::new(Field::new(
        POINT_COUNT_COLUMN,
        DataType::Int64,
        false,
    )));
    Schema::new(fields)
}

/// Per-feature accumulate values (one vector per spec, parallel to the rows),
/// extracted from the resolved column indices of a batch-shaped table.
pub(super) fn extract_accumulate_values(
    batch: &RecordBatch,
    acc_col_indices: &[usize],
) -> Vec<Vec<Option<f64>>> {
    acc_col_indices
        .iter()
        .map(|&idx| extract_sort_keys(batch.column(idx).as_ref()))
        .collect()
}

/// Append the `point_count` column to a level batch and overwrite the
/// accumulate columns for clustered rows (Q4).
///
/// - `batch`: the level batch built by [`build_level_batch`] (base schema).
/// - `out_schema`: base schema + trailing `point_count` field.
/// - `global_indices`: per batch row, the source-row index used as the
///   cluster-table key.
/// - `table`: the level's cluster table; `None` at the canonical level (all
///   singletons — every column passes through verbatim, count = 1).
/// - `acc_cols`: schema indices of the accumulate columns (parallel to the
///   aggregates in each [`ClusterEntry`]).
///
/// Singleton rows (absent from the table) keep every source value; only
/// non-singleton winners have `point_count > 1` and rewritten aggregates.
/// Aggregates are computed in `f64`; written back in the column's original
/// type (integer columns round to nearest — relevant for `mean`).
pub(super) fn apply_cluster_columns(
    batch: RecordBatch,
    out_schema: &Schema,
    global_indices: &[usize],
    table: Option<&HashMap<usize, ClusterEntry>>,
    acc_cols: &[usize],
) -> Result<RecordBatch, ConvertError> {
    use arrow_array::Int64Array;

    debug_assert_eq!(batch.num_rows(), global_indices.len());
    let mut columns: Vec<Arc<dyn Array>> = batch.columns().to_vec();

    // Overwrite accumulate columns for clustered rows.
    if let Some(table) = table {
        for (s, &col_idx) in acc_cols.iter().enumerate() {
            let overrides: Vec<Option<f64>> = global_indices
                .iter()
                .map(|g| table.get(g).and_then(|e| e.aggregates[s]))
                .collect();
            if overrides.iter().any(|o| o.is_some()) {
                columns[col_idx] = overwrite_numeric_column(&columns[col_idx], &overrides)?;
            }
        }
    }

    // Append point_count (1 for singletons / non-points / canonical rows).
    let counts: Vec<i64> = global_indices
        .iter()
        .map(|g| table.and_then(|t| t.get(g)).map_or(1, |e| e.point_count))
        .collect();
    columns.push(Arc::new(Int64Array::from(counts)));

    Ok(RecordBatch::try_new(Arc::new(out_schema.clone()), columns)?)
}

/// Rebuild a numeric column with per-row override values (`None` keeps the
/// original value, including its nullness). Aggregates arrive as `f64`;
/// integer columns round to nearest.
fn overwrite_numeric_column(
    col: &Arc<dyn Array>,
    overrides: &[Option<f64>],
) -> Result<Arc<dyn Array>, ConvertError> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::{
        Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type,
        UInt32Type, UInt64Type, UInt8Type,
    };
    use arrow_array::PrimitiveArray;

    macro_rules! rebuild {
        ($ty:ty, $cast:expr) => {{
            let a = col.as_primitive::<$ty>();
            let rebuilt: PrimitiveArray<$ty> = (0..a.len())
                .map(|i| match overrides[i] {
                    Some(v) => Some($cast(v)),
                    None => {
                        if a.is_null(i) {
                            None
                        } else {
                            Some(a.value(i))
                        }
                    }
                })
                .collect();
            Ok(Arc::new(rebuilt))
        }};
    }
    match col.data_type() {
        DataType::Int8 => rebuild!(Int8Type, |v: f64| v.round() as i8),
        DataType::Int16 => rebuild!(Int16Type, |v: f64| v.round() as i16),
        DataType::Int32 => rebuild!(Int32Type, |v: f64| v.round() as i32),
        DataType::Int64 => rebuild!(Int64Type, |v: f64| v.round() as i64),
        DataType::UInt8 => rebuild!(UInt8Type, |v: f64| v.round() as u8),
        DataType::UInt16 => rebuild!(UInt16Type, |v: f64| v.round() as u16),
        DataType::UInt32 => rebuild!(UInt32Type, |v: f64| v.round() as u32),
        DataType::UInt64 => rebuild!(UInt64Type, |v: f64| v.round() as u64),
        DataType::Float32 => rebuild!(Float32Type, |v: f64| v as f32),
        DataType::Float64 => rebuild!(Float64Type, |v: f64| v),
        other => Err(ConvertError::AccumulateColumnNotNumeric {
            name: "<accumulate column>".to_string(),
            data_type: format!("{other:?}"),
        }),
    }
}

// ============================================================================
// Coalescing helpers (Q3) — shared by the in-memory and streaming pipelines
// ============================================================================

/// One level's coalescing result: rep source row → (simplified merged
/// geometry, number of source segments merged).
pub(super) type CoalesceTable = HashMap<usize, (Geometry<f64>, i32)>;

/// Validate the coalescing-related schema constraint: the input must not
/// already carry a `coalesced_count` column (case-insensitive, mirroring the
/// `level` / `point_count` rules).
pub(super) fn validate_coalesce_schema(
    schema: &Schema,
    options: &ConvertOptions,
) -> Result<(), ConvertError> {
    if !options.coalesce_lines {
        return Ok(());
    }
    // Backstop: both pipelines run `resolve_reserved_column_collisions` first,
    // which renames any colliding `coalesced_count` away (#288), so this
    // normally never fires — it guards a direct caller that skipped the
    // resolver.
    if schema
        .fields()
        .iter()
        .any(|f| f.name().eq_ignore_ascii_case(COALESCED_COUNT_COLUMN))
    {
        return Err(ConvertError::CoalescedCountColumnPresent);
    }
    Ok(())
}

// ============================================================================
// Reserved-column collision handling (#288) — shared by both pipelines
// ============================================================================

/// The output columns the overview writer reserves for a given conversion.
/// `level` is always appended (§4.1); `point_count` and `coalesced_count` are
/// appended only when clustering / coalescing are enabled, so they are reserved
/// only in those modes — mirroring [`validate_cluster_schema`] /
/// [`validate_coalesce_schema`].
fn reserved_output_columns(options: &ConvertOptions) -> Vec<&'static str> {
    let mut names = vec![LEVEL_COLUMN];
    if options.cluster {
        names.push(POINT_COUNT_COLUMN);
    }
    if options.coalesce_lines {
        names.push(COALESCED_COUNT_COLUMN);
    }
    names
}

/// Rename any input column that collides (case-insensitively) with a reserved
/// overview output column, so real-world data whose properties happen to be
/// named `level` (Overture buildings' floor number), `LEVEL` (admin data),
/// `point_count`, etc. converts instead of being rejected (#288). Each
/// colliding column is renamed by appending `_`, looping until the name is
/// unique against both the existing columns and the reserved names. The
/// reserved output column stays authoritative.
///
/// The rename is metadata-only and **order-preserving** — type, nullability,
/// and field metadata are kept, and columns are not reordered — so every
/// downstream index- and projection-based path (which addresses columns
/// positionally) stays valid against the raw input. Any by-name option that
/// referenced a renamed column (`sort_key`, `class_ranking.column`,
/// `accumulate[].column`) is rewritten in `options` to the new name so it still
/// resolves. A `log::warn!` is emitted per rename.
///
/// Returns the rewritten schema — the same `Arc` when nothing collided, so
/// callers can cheaply detect the no-op via [`Arc::ptr_eq`] — and the applied
/// `(old, new)` renames.
pub(super) fn resolve_reserved_column_collisions(
    input_schema: &SchemaRef,
    options: &mut ConvertOptions,
) -> (SchemaRef, Vec<(String, String)>) {
    let reserved = reserved_output_columns(options);
    let match_reserved = |name: &str| -> Option<&'static str> {
        reserved
            .iter()
            .copied()
            .find(|r| name.eq_ignore_ascii_case(r))
    };

    let mut taken: HashSet<String> = input_schema
        .fields()
        .iter()
        .map(|f| f.name().to_ascii_lowercase())
        .collect();
    let mut renames: Vec<(String, String)> = Vec::new();
    let mut new_fields: Vec<Arc<Field>> = Vec::with_capacity(input_schema.fields().len());

    for field in input_schema.fields() {
        let name = field.name();
        let Some(reserved_name) = match_reserved(name) else {
            new_fields.push(field.clone());
            continue;
        };
        // Append `_` until the candidate collides with no existing column and
        // no reserved name. `_` can never introduce a `geom`/`geometry`
        // substring, so geometry detection stays unaffected.
        let mut candidate = format!("{name}_");
        while taken.contains(&candidate.to_ascii_lowercase())
            || match_reserved(&candidate).is_some()
        {
            candidate.push('_');
        }
        taken.insert(candidate.to_ascii_lowercase());
        log::warn!(
            "input column {name:?} collides with the reserved overview column \
             {reserved_name:?}; renaming the input column to {candidate:?} in \
             the output (the reserved {reserved_name:?} column is authoritative)"
        );
        renames.push((name.to_string(), candidate.clone()));
        new_fields.push(Arc::new(
            Field::new(candidate, field.data_type().clone(), field.is_nullable())
                .with_metadata(field.metadata().clone()),
        ));
    }

    if renames.is_empty() {
        return (input_schema.clone(), renames);
    }

    // A by-name option that pointed at a renamed column must follow the rename,
    // or a supported invocation (`--sort-key level`) would fail with
    // `*ColumnMissing` (or, for coalesce grouping, panic) downstream.
    let remap = |col: &str| -> Option<String> {
        renames
            .iter()
            .find(|(old, _)| col.eq_ignore_ascii_case(old))
            .map(|(_, new)| new.clone())
    };
    if let Some(new) = options.sort_key.as_deref().and_then(remap) {
        options.sort_key = Some(new);
    }
    if let Some(cr) = options.class_ranking.as_mut() {
        if let Some(new) = remap(&cr.column) {
            cr.column = new;
        }
    }
    for spec in &mut options.accumulate {
        if let Some(new) = remap(&spec.column) {
            spec.column = new;
        }
    }

    let schema = Arc::new(Schema::new_with_metadata(
        new_fields,
        input_schema.metadata().clone(),
    ));
    (schema, renames)
}

/// The writer schema with the trailing `coalesced_count` INT32 NOT NULL
/// column appended (coalescing enabled).
pub(super) fn append_coalesced_count_field(schema: &Schema) -> Schema {
    let mut fields: Vec<Arc<Field>> = schema.fields().iter().cloned().collect();
    fields.push(Arc::new(Field::new(
        COALESCED_COUNT_COLUMN,
        DataType::Int32,
        false,
    )));
    Schema::new(fields)
}

/// Append the `coalesced_count` column to a level batch: chain reps take
/// their member count from `table`; every other row (non-lines, unmerged
/// lines, canonical rows — `table` is `None` there) carries `1`.
pub(super) fn apply_coalesced_count(
    batch: RecordBatch,
    out_schema: &Schema,
    global_indices: &[usize],
    table: Option<&CoalesceTable>,
) -> Result<RecordBatch, ConvertError> {
    use arrow_array::Int32Array;

    debug_assert_eq!(batch.num_rows(), global_indices.len());
    let mut columns: Vec<Arc<dyn Array>> = batch.columns().to_vec();
    let counts: Vec<i32> = global_indices
        .iter()
        .map(|g| table.and_then(|t| t.get(g)).map_or(1, |(_, c)| *c))
        .collect();
    columns.push(Arc::new(Int32Array::from(counts)));
    Ok(RecordBatch::try_new(Arc::new(out_schema.clone()), columns)?)
}

/// The class column driving coalescing compatibility groups, when the Q1
/// ranking is class-based (explicit `--class-rank` or auto-detected Overture
/// roads). Numeric rankings (`--sort-key`, auto-confidence) and the size
/// fallback have no class semantics: all lines are compatible.
pub(super) fn coalesce_group_column(ranking: &RankingProvenance) -> Option<&str> {
    match ranking.mode.as_str() {
        "class-ranking" | "auto-overture-roads" => ranking.column.as_deref(),
        _ => None,
    }
}

/// Incremental string→id interner for coalescing compatibility groups.
/// Null values map to [`GroupInterner::NULL_GROUP`] (all nulls compatible
/// with each other, never with a named class).
#[derive(Debug, Default)]
pub(super) struct GroupInterner {
    map: HashMap<String, u32>,
}

impl GroupInterner {
    /// Group id assigned to null/missing class values.
    pub(super) const NULL_GROUP: u32 = u32::MAX;

    /// Intern one column's values, appending a group id per row to `out`.
    /// Non-string columns intern every row as [`Self::NULL_GROUP`] (callers
    /// only pass validated class-ranking columns, which are strings).
    pub(super) fn extend(&mut self, col: &dyn Array, out: &mut Vec<u32>) {
        use arrow_array::cast::AsArray;

        macro_rules! intern {
            ($arr:expr) => {{
                let a = $arr;
                for i in 0..a.len() {
                    if a.is_null(i) {
                        out.push(Self::NULL_GROUP);
                    } else {
                        let next = self.map.len() as u32;
                        let id = *self.map.entry(a.value(i).to_string()).or_insert(next);
                        out.push(id);
                    }
                }
            }};
        }
        match col.data_type() {
            DataType::Utf8 => intern!(col.as_string::<i32>()),
            DataType::LargeUtf8 => intern!(col.as_string::<i64>()),
            _ => out.extend(std::iter::repeat(Self::NULL_GROUP).take(col.len())),
        }
    }
}

/// Run the chain stage for one level: chain + gate + thin + per-level
/// density budget. The budget mirrors the Q2 geometric ladder over the LINE
/// candidate count — `budget(L) = num_lines / drop_rate^(finest − L)`, with
/// the same [`MIN_DENSITY_LEVEL_FEATURES`](super::assign) floor and
/// spatial-fairness gamma — so coalescing does not bypass the mid-zoom cap
/// the budget was calibrated for. Deterministic; shared verbatim by both
/// pipelines (and the streaming counting pass) so their outputs and hints
/// stay identical.
pub(super) fn coalesce_level_chains(
    inputs: &[CoalesceInput<'_>],
    level: usize,
    finest: usize,
    gsd_m: f64,
    crs: Crs,
    options: &ConvertOptions,
) -> Vec<super::coalesce::CoalescedLine> {
    let budget = if options.density.enabled
        && options.density.drop_rate > 1.0
        && !options.density.drop_rate.is_nan()
        && level < finest
    {
        let keep = 1.0 / options.density.drop_rate;
        let raw = inputs.len() as f64 * keep.powi((finest - level) as i32);
        let max_chains = (raw.round() as usize).max(super::assign::MIN_DENSITY_LEVEL_FEATURES);
        Some((max_chains, options.density.gamma))
    } else {
        None
    };
    coalesce_level_lines(
        inputs,
        gsd_m,
        crs,
        &options.assign,
        &CoalesceParams {
            snap_gsd_factor: options.coalesce_snap,
            junction_angle_deg: options.coalesce_junction_angle,
            budget,
        },
    )
}

/// Build one level's [`CoalesceTable`]: run the chain stage
/// ([`coalesce_level_chains`]), then simplify each surviving chain for the
/// level. Chains that degenerate during simplification are dropped (default
/// knobs never hit this: the 2×GSD gate is stricter than the 1×GSD simplify
/// drop gate).
pub(super) fn build_level_coalesce_table(
    inputs: &[CoalesceInput<'_>],
    level: usize,
    finest: usize,
    gsd_m: f64,
    crs: Crs,
    options: &ConvertOptions,
) -> CoalesceTable {
    let chains = coalesce_level_chains(inputs, level, finest, gsd_m, crs, options);
    let mut table = CoalesceTable::with_capacity(chains.len());
    for chain in chains {
        match simplify_for_level(&chain.geom, gsd_m, crs, &options.simplify) {
            Simplified::Keep(g) => {
                table.insert(chain.rep, (g, chain.count));
            }
            Simplified::Dropped => {}
        }
    }
    table
}

/// Whether coalescing is effectively active for this conversion: enabled,
/// and the candidate line count fits the per-level memory guard. Logs when
/// the guard trips (the file still carries the `coalesced_count` column,
/// all 1, and the coalescing provenance).
pub(super) fn coalesce_effective(options: &ConvertOptions, num_lines: usize) -> bool {
    if !options.coalesce_lines {
        return false;
    }
    if num_lines > options.coalesce_max_level_rows {
        log::warn!(
            "coalescing skipped: {num_lines} candidate lines exceed \
             --coalesce-max-level-rows {} (chaining holds a level's line \
             geometries in memory; near-canonical levels this large need \
             coalescing least). Output keeps the coalesced_count column \
             (all 1).",
            options.coalesce_max_level_rows
        );
        return false;
    }
    true
}

/// Build informative generalization provenance (§3.5) from the emitted gsds.
pub(super) fn build_generalization(
    gsds: &[f64],
    _crs: Crs,
    options: &ConvertOptions,
    ranking: RankingProvenance,
) -> Generalization {
    let levels = gsds
        .iter()
        .map(|&gsd_m| GeneralizationLevel {
            simplify_tolerance_m: match options.mode {
                Mode::Duplicating => options.simplify.factor * gsd_m,
                Mode::Partitioning => 0.0,
            },
            thinning_factor: options.assign.polygon_thinning,
            visibility_gate_m: options.assign.polygon_visibility * gsd_m,
            geometry_types: Vec::new(),
        })
        .collect();
    Generalization {
        engine: format!("tylertoo {}", env!("CARGO_PKG_VERSION")),
        // Only record the base when it deviates from the default: a default run
        // then produces a byte-identical footer to before this knob existed
        // (the levels[].gsd already imply the default base, §5.2 / Q6).
        gsd_base: if options.gsd_base == GSD_TILE_BASE {
            None
        } else {
            Some(options.gsd_base)
        },
        levels,
        // Recorded only when cascading applied (#218, duplicating default):
        // a --no-cascade run omits the member so its footer stays
        // byte-identical to pre-cascade output. Partitioning never
        // simplifies, so it never cascades.
        cascade: if matches!(options.mode, Mode::Duplicating) && options.simplify.cascade {
            Some(true)
        } else {
            None
        },
        ranking: Some(ranking),
        // Record the density budget only when it was applied; a disabled run
        // omits the block so its footer matches pre-Q2 output.
        density_drop: if options.density.enabled {
            Some(DensityProvenance {
                drop_rate: options.density.drop_rate,
                gamma: options.density.gamma,
                supercell_gsd_factor: SUPERCELL_GSD_FACTOR,
            })
        } else {
            None
        },
        // Recorded only when coalescing was requested; a non-coalesced run
        // emits a byte-identical footer to before this feature existed.
        coalescing: if options.coalesce_lines {
            Some(CoalescingProvenance {
                enabled: true,
                snap_tolerance_gsd_factor: options.coalesce_snap,
                // §13.4 (v0.2.0): the junction-continuation threshold and the
                // per-level candidate ceiling are REQUIRED provenance so the
                // generalization is reproducible from the file alone.
                junction_angle: Some(options.coalesce_junction_angle),
                max_level_rows: Some(options.coalesce_max_level_rows as u64),
                coalesced_count_column: COALESCED_COUNT_COLUMN.to_string(),
            })
        } else {
            None
        },
        // Recorded only when clustering was applied; a non-clustered run emits
        // a byte-identical footer to before this feature existed.
        clustering: if options.cluster {
            Some(ClusteringProvenance {
                enabled: true,
                point_count_column: POINT_COUNT_COLUMN.to_string(),
                accumulated: options
                    .accumulate
                    .iter()
                    .map(|s| AccumulatedColumn {
                        column: s.column.clone(),
                        op: s.op.as_str().to_string(),
                    })
                    .collect(),
            })
        } else {
            None
        },
    }
}

/// Resolve the cell-winner ranking for a conversion (Q1). Returns per-feature
/// sort keys (parallel to `full`'s rows / `geometries`) plus the provenance
/// block (§3.5). Tiers, in priority order:
///
/// 1. explicit numeric `--sort-key`;
/// 2. explicit categorical `class_ranking`;
/// 3. auto-detected Overture roads (`road_class`/`class`) or places
///    (`confidence`, points only) — unless `no_auto_rank`;
/// 4. `size-fallback` (no keys; assignment ranks by bbox diagonal + hash).
///
/// The chosen tier is logged (`log::info!`) so corpus runs show what happened.
fn resolve_ranking(
    input_schema: &Schema,
    full: &RecordBatch,
    geometries: &[Geometry<f64>],
    options: &ConvertOptions,
) -> Result<(Vec<Option<f64>>, RankingProvenance), ConvertError> {
    let n = full.num_rows();

    // Tier 1a: explicit numeric --sort-key.
    if let Some(name) = &options.sort_key {
        let idx = input_schema
            .index_of(name)
            .map_err(|_| ConvertError::SortKeyColumnMissing { name: name.clone() })?;
        let keys = extract_sort_keys(full.column(idx));
        log::info!("overview ranking: explicit numeric sort-key column {name:?}");
        return Ok((
            keys,
            RankingProvenance {
                mode: "explicit-sort-key".to_string(),
                column: Some(name.clone()),
                ranks: None,
                unknown_rank: None,
            },
        ));
    }

    // Tier 1b: explicit categorical class ranking.
    if let Some(cr) = &options.class_ranking {
        let idx = input_schema.index_of(&cr.column).map_err(|_| {
            ConvertError::ClassRankColumnMissing {
                name: cr.column.clone(),
            }
        })?;
        let keys = extract_class_ranks(full.column(idx), cr)?;
        log::info!(
            "overview ranking: explicit class-ranking on column {:?} ({} named classes, unknown_rank={})",
            cr.column,
            cr.ranks.len(),
            cr.unknown_rank
        );
        return Ok((keys, class_ranking_provenance("class-ranking", cr)));
    }

    // Tier 3: auto-detection of well-known schemas.
    if !options.no_auto_rank {
        // Overture transportation road classes.
        if let Some((idx, col_name)) = find_road_class_column(input_schema, full) {
            let cr = overture_road_ranking(col_name.clone());
            let keys = extract_class_ranks(full.column(idx), &cr)?;
            log::info!(
                "overview ranking: auto-detected Overture road classes in column {col_name:?}; \
                 applying built-in ranking (motorway > … > service > tail)"
            );
            return Ok((keys, class_ranking_provenance("auto-overture-roads", &cr)));
        }
        // Overture places confidence (numeric, point datasets only).
        if let Some((idx, col_name)) = find_confidence_column(input_schema, geometries) {
            let keys = extract_sort_keys(full.column(idx));
            log::info!(
                "overview ranking: auto-detected Overture places confidence column {col_name:?} \
                 (numeric point ranking)"
            );
            return Ok((
                keys,
                RankingProvenance {
                    mode: "auto-confidence".to_string(),
                    column: Some(col_name),
                    ranks: None,
                    unknown_rank: None,
                },
            ));
        }
    }

    // Fallback: size (bbox diagonal) + deterministic hash (existing behavior).
    log::info!(
        "overview ranking: no sort key specified or auto-detected; using size + \
         deterministic-hash fallback"
    );
    Ok((
        vec![None; n],
        RankingProvenance {
            mode: "size-fallback".to_string(),
            column: None,
            ranks: None,
            unknown_rank: None,
        },
    ))
}

/// Provenance for a categorical class ranking, echoing the map when small.
/// The footer shape is a JSON object map (spec §3.5 v0.2.0), so the ordered
/// pair list collapses into a `BTreeMap` here.
pub(super) fn class_ranking_provenance(mode: &str, cr: &ClassRanking) -> RankingProvenance {
    let ranks = if cr.ranks.len() <= MAX_PROVENANCE_RANKS {
        Some(cr.ranks.iter().cloned().collect())
    } else {
        None
    };
    RankingProvenance {
        mode: mode.to_string(),
        column: Some(cr.column.clone()),
        ranks,
        unknown_rank: Some(cr.unknown_rank),
    }
}

/// Map each row's string value to its class priority. Null values → `None`
/// (they lose to every ranked feature). A present-but-unranked value maps to
/// [`ClassRanking::unknown_rank`].
pub(super) fn extract_class_ranks(
    col: &dyn Array,
    ranking: &ClassRanking,
) -> Result<Vec<Option<f64>>, ConvertError> {
    use arrow_array::cast::AsArray;

    let map: HashMap<&str, f64> = ranking
        .ranks
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    let n = col.len();

    macro_rules! collect_str {
        ($arr:expr) => {{
            let a = $arr;
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(*map.get(a.value(i)).unwrap_or(&ranking.unknown_rank))
                    }
                })
                .collect()
        }};
    }

    match col.data_type() {
        DataType::Utf8 => Ok(collect_str!(col.as_string::<i32>())),
        DataType::LargeUtf8 => Ok(collect_str!(col.as_string::<i64>())),
        other => Err(ConvertError::ClassRankColumnNotString {
            name: ranking.column.clone(),
            data_type: format!("{other:?}"),
        }),
    }
}

/// Auto-detect an Overture road-class column: a Utf8/LargeUtf8 column named
/// `road_class` or `class` (case-insensitive) whose values overlap the known
/// transportation vocabulary by at least [`ROAD_VOCAB_MIN_DISTINCT`] distinct
/// classes. Returns `(column index, column name)`.
fn find_road_class_column(schema: &Schema, full: &RecordBatch) -> Option<(usize, String)> {
    for (idx, f) in schema.fields().iter().enumerate() {
        let lname = f.name().to_ascii_lowercase();
        if lname != "road_class" && lname != "class" {
            continue;
        }
        if !matches!(f.data_type(), DataType::Utf8 | DataType::LargeUtf8) {
            continue;
        }
        if column_overlaps_road_vocab(full.column(idx)) {
            return Some((idx, f.name().clone()));
        }
    }
    None
}

/// True if the string column contains at least [`ROAD_VOCAB_MIN_DISTINCT`]
/// distinct values from [`KNOWN_ROAD_CLASSES`].
fn column_overlaps_road_vocab(col: &dyn Array) -> bool {
    use arrow_array::cast::AsArray;

    let vocab: HashSet<&str> = KNOWN_ROAD_CLASSES.iter().copied().collect();
    let mut found: HashSet<&str> = HashSet::new();

    macro_rules! scan {
        ($arr:expr) => {{
            let a = $arr;
            for i in 0..a.len() {
                if a.is_null(i) {
                    continue;
                }
                if let Some(&hit) = vocab.get(a.value(i)) {
                    found.insert(hit);
                    if found.len() >= ROAD_VOCAB_MIN_DISTINCT {
                        return true;
                    }
                }
            }
        }};
    }

    match col.data_type() {
        DataType::Utf8 => scan!(col.as_string::<i32>()),
        DataType::LargeUtf8 => scan!(col.as_string::<i64>()),
        _ => return false,
    }
    found.len() >= ROAD_VOCAB_MIN_DISTINCT
}

/// Auto-detect an Overture places `confidence` column: a Float32/Float64 column
/// named `confidence` (case-insensitive), applied only when the dataset is
/// predominantly points. Returns `(column index, column name)`.
fn find_confidence_column(
    schema: &Schema,
    geometries: &[Geometry<f64>],
) -> Option<(usize, String)> {
    if geometries.is_empty() {
        return None;
    }
    let points = geometries
        .iter()
        .filter(|g| matches!(feature_kind(g), FeatureKind::Point))
        .count();
    // Require a point majority; confidence ranking is a points convention.
    if points * 2 < geometries.len() {
        return None;
    }
    for (idx, f) in schema.fields().iter().enumerate() {
        if f.name().eq_ignore_ascii_case("confidence")
            && matches!(f.data_type(), DataType::Float32 | DataType::Float64)
        {
            return Some((idx, f.name().clone()));
        }
    }
    None
}

/// Fill each level report's byte sizes by summing its row-group band from the
/// output file's Parquet footer.
pub(super) fn fill_level_bytes(
    output_path: &Path,
    meta: &super::level::OverviewsMeta,
    reports: &mut [LevelReport],
) -> Result<(), ConvertError> {
    let file = std::fs::File::open(output_path)?;
    let pq = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let pmeta = pq.metadata();

    let mut start = 0usize;
    for (level, report) in meta.levels.iter().zip(reports.iter_mut()) {
        let end = level.row_group_end as usize;
        let mut uncompressed = 0i64;
        let mut compressed = 0i64;
        for rg in start..=end {
            let rgm = pmeta.row_group(rg);
            uncompressed += rgm.total_byte_size();
            compressed += rgm.compressed_size();
        }
        report.uncompressed_bytes = uncompressed;
        report.compressed_bytes = compressed;
        start = end + 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::check::validate_file;
    use crate::overview::level::gsd;
    use crate::overview::reader::OverviewReader;
    use arrow_array::{Float64Array, Int64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use geo::{Geometry, LineString, Point, Polygon};
    use geoarrow::array::GeometryBuilder;
    use geoarrow::datatypes::GeometryType;
    use geoarrow_array::GeoArrowArray;
    use geoparquet::writer::{
        GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
    };
    use parquet::arrow::ArrowWriter;

    use crate::batch_processor::extract_geometries_from_array;

    /// #267: the download-first nudge fires only for a large *whole-file*
    /// remote convert, and stays quiet for local inputs, effective bbox
    /// extracts, and small objects.
    #[test]
    fn full_file_remote_warning_gated_on_large_unpruned_remote() {
        const BIG: u64 = 4 << 30; // 4 GiB, above the 1 GiB threshold
                                  // Local input (0 remote parts): re-reads hit
                                  // the OS page cache — never warn.
        assert!(full_file_remote_warning(0, 8, 8, BIG).is_none());
        // Effective bbox extract (fewer row groups read): never warn.
        assert!(full_file_remote_warning(1, 2, 8, BIG).is_none());
        // Small remote object: below threshold — never warn.
        assert!(full_file_remote_warning(1, 8, 8, 100 << 20).is_none());
        // Just below the threshold: still quiet.
        assert!(full_file_remote_warning(1, 8, 8, FULL_FILE_REMOTE_WARN_BYTES - 1).is_none());
        // At the threshold: warns (guard is a strict `<`).
        assert!(full_file_remote_warning(1, 8, 8, FULL_FILE_REMOTE_WARN_BYTES).is_some());
        // Large full-file remote: warns, and the nudge names the cheaper paths.
        let msg = full_file_remote_warning(1, 8, 8, BIG)
            .expect("large full-file remote convert should warn");
        assert!(msg.contains("--bbox"), "nudge should mention --bbox: {msg}");
        assert!(
            msg.contains("4.0 GiB"),
            "nudge should state the object size: {msg}"
        );
        assert!(
            !msg.contains("partitions"),
            "single object: no part count in the message: {msg}"
        );
    }

    /// v0.7 multi-partition: the summed object size trips the same
    /// threshold (20 × 600 MB with no bbox IS a ≥1 GiB full-file remote
    /// convert), and the message names the part count so the total is not
    /// mistaken for one object.
    #[test]
    fn full_file_remote_warning_names_partition_count() {
        const PART: u64 = 600 << 20; // 600 MB per partition
        let msg = full_file_remote_warning(20, 40, 40, 20 * PART)
            .expect("20 x 600 MB unpruned remote parts should warn");
        assert!(
            msg.contains("20 remote partitions"),
            "message names the part count: {msg}"
        );
        assert!(
            msg.contains("11.7 GiB"),
            "message states the summed size: {msg}"
        );
        // Summed size below the threshold stays quiet regardless of count.
        assert!(full_file_remote_warning(20, 40, 40, 500 << 20).is_none());
    }

    /// #272: the spill free-space preflight warns when the projected spill
    /// (≈ the selected input bytes, plus a 5% safety margin) exceeds the
    /// free space on the spill volume — naming the directory, the shortfall,
    /// and the `--spill-dir` escape hatch.
    #[test]
    fn spill_space_warning_names_dir_and_shortfall() {
        let dir = Path::new("/mnt/scratch");
        let msg = spill_space_warning(10 << 30, 1 << 30, dir).expect("shortfall should warn");
        assert!(
            msg.contains("/mnt/scratch"),
            "warning names the spill dir: {msg}"
        );
        assert!(
            msg.contains("--spill-dir"),
            "warning suggests --spill-dir: {msg}"
        );
        // need = 10 GiB + 5% = 10.5 GiB; available 1 GiB → 9.5 GiB short.
        assert!(
            msg.contains("9.5 GiB"),
            "warning states the shortfall: {msg}"
        );
    }

    /// #272: ample space stays quiet, and the margin boundary is exact —
    /// available == estimated + 5% is enough, one byte less is not.
    #[test]
    fn spill_space_warning_quiet_with_ample_space() {
        let dir = Path::new("/tmp");
        assert!(spill_space_warning(1 << 30, 20 << 30, dir).is_none());
        let est: u64 = 20 << 20;
        let need = est + est / 20;
        assert!(spill_space_warning(est, need, dir).is_none());
        assert!(spill_space_warning(est, need - 1, dir).is_some());
    }

    /// #272: local inputs never spill, so the free-space probe must not
    /// even run for them; a failed probe (None) stays quiet rather than
    /// crying wolf; a zero estimate (empty selection) stays quiet too.
    #[test]
    fn spill_space_check_gating() {
        let dir = Path::new("/tmp");
        assert!(spill_space_check(false, 10 << 30, dir, |_| panic!(
            "free-space probe must not run for local inputs"
        ))
        .is_none());
        assert!(spill_space_check(true, 10 << 30, dir, |_| None).is_none());
        assert!(spill_space_check(true, 0, dir, |_| Some(0)).is_none());
        assert!(spill_space_check(true, 10 << 30, dir, |_| Some(1)).is_some());
    }

    /// #272: a configured spill dir must exist — fail fast at option
    /// validation instead of silently degrading to network re-fetch when
    /// the spill file cannot be created mid-convert.
    #[test]
    fn validate_options_rejects_missing_spill_dir() {
        let opts = ConvertOptions {
            spill_dir: Some(std::path::PathBuf::from(
                "/nonexistent/tylertoo-spill-dir-272",
            )),
            ..Default::default()
        };
        let err = validate_options(&opts).expect_err("missing spill dir must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("spill-dir"), "error names the option: {msg}");
        assert!(
            msg.contains("/nonexistent/tylertoo-spill-dir-272"),
            "error names the path: {msg}"
        );
    }

    // --- synthetic GeoParquet input builders --------------------------------

    /// A mix of points, lines, and polygons spread far apart so coarse-level
    /// thinning keeps distinct cell winners.
    fn synthetic_geometries() -> Vec<Geometry<f64>> {
        let mut geoms = Vec::new();
        // Points on a coarse grid (meters, EPSG:3857-ish scale via 4326? we use
        // 4326 degrees here, but coordinates are just far apart).
        for i in 0..6 {
            let x = i as f64 * 5.0;
            let y = i as f64 * 3.0;
            geoms.push(Geometry::Point(Point::new(x, y)));
        }
        // A few multi-vertex lines.
        for i in 0..4 {
            let base = 40.0 + i as f64 * 10.0;
            let ls = LineString::from(
                (0..12)
                    .map(|k| {
                        (
                            base + k as f64 * 0.5,
                            (k as f64 * 0.6).sin() + i as f64 * 8.0,
                        )
                    })
                    .collect::<Vec<_>>(),
            );
            geoms.push(Geometry::LineString(ls));
        }
        // Polygons of varying size.
        for i in 0..4 {
            let cx = -60.0 + i as f64 * 12.0;
            let cy = -40.0 - i as f64 * 5.0;
            let half = 2.0 + i as f64 * 1.5;
            let ext = LineString::from(vec![
                (cx - half, cy - half),
                (cx + half, cy - half),
                (cx + half, cy + half),
                (cx - half, cy + half),
                (cx - half, cy - half),
            ]);
            geoms.push(Geometry::Polygon(Polygon::new(ext, vec![])));
        }
        geoms
    }

    fn build_geometry_array(geoms: &[Geometry<f64>]) -> geoarrow::array::GeometryArray {
        let typ = GeometryType::new(Default::default());
        let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
        b.extend_from_iter(geoms.iter().map(Some));
        b.finish()
    }

    /// Field names of an overview output file, in schema order.
    fn output_column_names(path: &Path) -> Vec<String> {
        let file = std::fs::File::open(path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        builder
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }

    /// Write a valid GeoParquet file (WKB, covering) with id/name/rank props and
    /// the given geometries. `extra_level_col` injects a `level` Int32 column to
    /// exercise the reserved-column auto-rename path (#288). `crs_projjson`
    /// overrides the geometry CRS.
    fn write_input(
        path: &Path,
        geoms: &[Geometry<f64>],
        extra_level_col: bool,
        crs_metadata: Option<geoarrow::datatypes::Metadata>,
    ) {
        let n = geoms.len();
        let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
        let name = StringArray::from((0..n).map(|i| format!("f{i}")).collect::<Vec<_>>());
        let rank = Float64Array::from((0..n).map(|i| (n - i) as f64).collect::<Vec<_>>());

        let geom_arr = if let Some(md) = crs_metadata {
            let typ = GeometryType::new(Arc::new(md));
            let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
            b.extend_from_iter(geoms.iter().map(Some));
            b.finish()
        } else {
            build_geometry_array(geoms)
        };
        let geom_field = geom_arr.data_type().to_field("geometry", true);

        let mut fields = vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(Field::new("name", DataType::Utf8, false)),
            Arc::new(Field::new("rank", DataType::Float64, false)),
        ];
        let mut columns: Vec<Arc<dyn Array>> = vec![Arc::new(id), Arc::new(name), Arc::new(rank)];
        if extra_level_col {
            fields.push(Arc::new(Field::new("level", DataType::Int32, false)));
            columns.push(Arc::new(arrow_array::Int32Array::from(vec![0i32; n])));
        }
        fields.push(Arc::new(geom_field));
        columns.push(geom_arr.to_array_ref());

        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

        let gpq_options = GeoParquetWriterOptionsBuilder::default()
            .set_encoding(GeoParquetWriterEncoding::WKB)
            .set_generate_covering(true)
            .build();
        let encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
        let target_schema = encoder.target_schema();

        let file = std::fs::File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, target_schema, None).unwrap();
        // encode_record_batch requires &mut encoder.
        let mut encoder = encoder;
        let encoded = encoder.encode_record_batch(&batch).unwrap();
        writer.write(&encoded).unwrap();
        writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
        writer.close().unwrap();
    }

    /// Read (id, name, rank, geometry) for a single level from an overview file.
    fn read_level_rows(
        reader: &OverviewReader,
        level: usize,
    ) -> Vec<(i64, String, f64, Geometry<f64>)> {
        use arrow_array::cast::AsArray;
        use arrow_array::types::Float64Type;
        let rdr = reader.read_level(level, None).unwrap();
        let mut out = Vec::new();
        for batch in rdr {
            let batch = batch.unwrap();
            let ids = batch
                .column(batch.schema().index_of("id").unwrap())
                .as_primitive::<arrow_array::types::Int64Type>()
                .clone();
            let names = batch
                .column(batch.schema().index_of("name").unwrap())
                .as_string::<i32>()
                .clone();
            let ranks = batch
                .column(batch.schema().index_of("rank").unwrap())
                .as_primitive::<Float64Type>()
                .clone();
            let gcol = batch.column(batch.schema().index_of("geometry").unwrap());
            let garr: Arc<dyn GeoArrowArray> = from_arrow_array(
                gcol.as_ref(),
                batch
                    .schema()
                    .field(batch.schema().index_of("geometry").unwrap()),
            )
            .unwrap();
            let mut gvec = Vec::new();
            extract_geometries_from_array(garr.as_ref(), &mut gvec).unwrap();
            for (i, g) in gvec.iter().enumerate() {
                out.push((
                    ids.value(i),
                    names.value(i).to_string(),
                    ranks.value(i),
                    g.clone(),
                ));
            }
        }
        out
    }

    // --- multi-partition input (v0.7) ----------------------------------------

    /// Write a valid GeoParquet partition holding `geoms[range]` with the
    /// SAME global id/name/rank values [`write_input`] assigns, so the
    /// concatenation of partitions equals the single file row-for-row.
    /// `row_group_rows` caps rows per row group (None = single row group).
    fn write_input_partition(
        path: &Path,
        geoms: &[Geometry<f64>],
        range: std::ops::Range<usize>,
        row_group_rows: Option<usize>,
    ) {
        use parquet::file::properties::WriterProperties;
        let total = geoms.len();
        let idx: Vec<usize> = range.collect();
        let id = Int64Array::from(idx.iter().map(|&i| i as i64).collect::<Vec<_>>());
        let name = StringArray::from(idx.iter().map(|&i| format!("f{i}")).collect::<Vec<_>>());
        let rank = Float64Array::from(idx.iter().map(|&i| (total - i) as f64).collect::<Vec<_>>());
        let part_geoms: Vec<Geometry<f64>> = idx.iter().map(|&i| geoms[i].clone()).collect();
        let geom_arr = build_geometry_array(&part_geoms);
        let geom_field = geom_arr.data_type().to_field("geometry", true);

        let fields = vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(Field::new("name", DataType::Utf8, false)),
            Arc::new(Field::new("rank", DataType::Float64, false)),
            Arc::new(geom_field),
        ];
        let columns: Vec<Arc<dyn Array>> = vec![
            Arc::new(id),
            Arc::new(name),
            Arc::new(rank),
            geom_arr.to_array_ref(),
        ];
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

        let gpq_options = GeoParquetWriterOptionsBuilder::default()
            .set_encoding(GeoParquetWriterEncoding::WKB)
            .set_generate_covering(true)
            .build();
        let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
        let target_schema = encoder.target_schema();
        let props = row_group_rows.map(|n| {
            WriterProperties::builder()
                .set_max_row_group_row_count(Some(n))
                .build()
        });
        let file = std::fs::File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, target_schema, props).unwrap();
        let encoded = encoder.encode_record_batch(&batch).unwrap();
        writer.write(&encoded).unwrap();
        writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
        writer.close().unwrap();
    }

    /// Convert `input` and export to PMTiles; returns the archive bytes.
    /// PMTiles export is byte-deterministic (unlike the overview parquet
    /// footer), so multi/single equivalence is asserted on these bytes.
    fn convert_and_export(input: &Path, workdir: &Path, opts: &ConvertOptions) -> Vec<u8> {
        use crate::overview::export::{export_pmtiles, ExportOptions};
        let stem = input
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .replace('.', "_");
        let overview = workdir.join(format!("{stem}-overview.parquet"));
        let pmtiles = workdir.join(format!("{stem}.pmtiles"));
        convert_to_overviews(input, &overview, opts).unwrap();
        export_pmtiles(&overview, &pmtiles, &ExportOptions::default()).unwrap();
        std::fs::read(&pmtiles).unwrap()
    }

    fn multi_test_options() -> ConvertOptions {
        ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            ..Default::default()
        }
    }

    /// THE anchor: identical rows as (a) one parquet file and (b) three
    /// partition files must produce byte-identical PMTiles. All passes see
    /// the same rows in the same order (winner tables are keyed by global
    /// row offset), so the outputs match exactly.
    #[test]
    fn multi_partition_output_matches_single_file() {
        let geoms = synthetic_geometries();
        let n = geoms.len();
        let dir = tempfile::tempdir().unwrap();
        let single = dir.path().join("single.parquet");
        write_input_partition(&single, &geoms, 0..n, None);
        let parts = dir.path().join("parts");
        std::fs::create_dir(&parts).unwrap();
        write_input_partition(&parts.join("part-000.parquet"), &geoms, 0..5, None);
        write_input_partition(&parts.join("part-001.parquet"), &geoms, 5..9, None);
        write_input_partition(&parts.join("part-002.parquet"), &geoms, 9..n, None);

        let opts = multi_test_options();
        let report =
            convert_to_overviews(&parts, dir.path().join("probe-overview.parquet"), &opts).unwrap();
        assert_eq!(report.input_features, n);
        assert_eq!(report.row_groups_total, 3, "one row group per partition");

        let pm_single = convert_and_export(&single, dir.path(), &opts);
        let pm_multi = convert_and_export(&parts, dir.path(), &opts);
        assert!(
            pm_single == pm_multi,
            "multi-partition output must be byte-identical to single-file \
             ({} vs {} bytes)",
            pm_single.len(),
            pm_multi.len()
        );
    }

    /// A 0-row partition mid-set is skipped cleanly: global row offsets are
    /// unaffected and the output still matches the single-file equivalent.
    #[test]
    fn multi_partition_zero_row_part_matches_single_file() {
        let geoms = synthetic_geometries();
        let n = geoms.len();
        let dir = tempfile::tempdir().unwrap();
        let single = dir.path().join("single.parquet");
        write_input_partition(&single, &geoms, 0..n, None);
        let parts = dir.path().join("parts");
        std::fs::create_dir(&parts).unwrap();
        write_input_partition(&parts.join("part-000.parquet"), &geoms, 0..5, None);
        write_input_partition(&parts.join("part-001.parquet"), &geoms, 5..5, None); // 0 rows
        write_input_partition(&parts.join("part-002.parquet"), &geoms, 5..n, None);

        let opts = multi_test_options();
        let pm_single = convert_and_export(&single, dir.path(), &opts);
        let pm_multi = convert_and_export(&parts, dir.path(), &opts);
        assert!(
            pm_single == pm_multi,
            "0-row partition must not shift rows or offsets"
        );
    }

    /// bbox row-group pruning (#102) composes with multi-partition input:
    /// the selection is per part, offsets stay aligned across the pruned
    /// read, and the output matches the single-file bbox extract.
    #[test]
    fn multi_partition_bbox_selection_matches_single_file() {
        let geoms = synthetic_geometries();
        let n = geoms.len();
        let dir = tempfile::tempdir().unwrap();
        let single = dir.path().join("single.parquet");
        write_input_partition(&single, &geoms, 0..n, Some(2));
        let parts = dir.path().join("parts");
        std::fs::create_dir(&parts).unwrap();
        write_input_partition(&parts.join("part-000.parquet"), &geoms, 0..6, Some(2));
        write_input_partition(&parts.join("part-001.parquet"), &geoms, 6..10, Some(2));
        write_input_partition(&parts.join("part-002.parquet"), &geoms, 10..n, Some(2));

        // Points (x 0..25) and polygons (x -78..-15) only; the lines
        // (x 40..96) fall outside, so their row groups prune away.
        let bbox = Some([-100.0, -70.0, 30.0, 20.0]);
        let opts = ConvertOptions {
            bbox,
            ..multi_test_options()
        };
        let report =
            convert_to_overviews(&parts, dir.path().join("probe-overview.parquet"), &opts).unwrap();
        assert!(
            report.row_groups_read < report.row_groups_total,
            "bbox must prune row groups across parts: {}/{}",
            report.row_groups_read,
            report.row_groups_total
        );

        let pm_single = convert_and_export(&single, dir.path(), &opts);
        let pm_multi = convert_and_export(&parts, dir.path(), &opts);
        assert!(
            pm_single == pm_multi,
            "per-part bbox selection must keep offsets aligned"
        );
    }

    /// The in-memory reference path (`--no-streaming`) does not support
    /// multi-partition input; it must fail with a clear error.
    #[test]
    fn multi_partition_requires_streaming_pipeline() {
        let geoms = synthetic_geometries();
        let dir = tempfile::tempdir().unwrap();
        let parts = dir.path().join("parts");
        std::fs::create_dir(&parts).unwrap();
        write_input_partition(&parts.join("part-000.parquet"), &geoms, 0..5, None);
        write_input_partition(
            &parts.join("part-001.parquet"),
            &geoms,
            5..geoms.len(),
            None,
        );

        let opts = ConvertOptions {
            streaming: false,
            ..multi_test_options()
        };
        let err = convert_to_overviews(&parts, dir.path().join("out.parquet"), &opts).unwrap_err();
        assert!(
            matches!(err, ConvertError::MultiPartitionRequiresStreaming),
            "expected MultiPartitionRequiresStreaming, got {err:?}"
        );
    }

    /// Partitions disagreeing on CRS are rejected at resolve time with an
    /// error naming the offending file.
    #[test]
    fn multi_partition_crs_mismatch_rejected() {
        let geoms = synthetic_geometries();
        let dir = tempfile::tempdir().unwrap();
        let parts = dir.path().join("parts");
        std::fs::create_dir(&parts).unwrap();
        write_input(&parts.join("a.parquet"), &geoms, false, None);
        // EPSG:32633 in the second partition (also differs in geometry
        // extension metadata — either check may fire; both name the file).
        let projjson = serde_json::json!({
            "type": "ProjectedCRS",
            "name": "UTM zone 33N",
            "id": { "authority": "EPSG", "code": 32633 }
        });
        let md = geoarrow::datatypes::Metadata::new(
            geoarrow::datatypes::Crs::from_projjson(projjson),
            None,
        );
        write_input(&parts.join("b.parquet"), &geoms, false, Some(md));

        let err = convert_to_overviews(
            &parts,
            dir.path().join("out.parquet"),
            &multi_test_options(),
        )
        .unwrap_err();
        match err {
            ConvertError::Input(crate::input::InputError::IncompatiblePartition {
                offender,
                ..
            }) => {
                assert!(offender.ends_with("b.parquet"), "offender: {offender}");
            }
            other => panic!("expected IncompatiblePartition, got {other:?}"),
        }
    }

    // --- tests --------------------------------------------------------------

    #[test]
    fn duplicating_canonical_matches_input() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();

        // Output validates clean.
        let vr = validate_file(tout.path()).unwrap();
        assert!(
            vr.is_valid(),
            "failures: {:?}",
            vr.failures().collect::<Vec<_>>()
        );

        let reader = OverviewReader::open(tout.path()).unwrap();
        assert_eq!(reader.mode(), Mode::Duplicating);
        let canonical = reader.num_levels() - 1;

        // Canonical row count == input row count, values identical & in order.
        let rows = read_level_rows(&reader, canonical);
        assert_eq!(rows.len(), geoms.len());
        for (i, (id, name, rank, geom)) in rows.iter().enumerate() {
            assert_eq!(*id, i as i64);
            assert_eq!(name, &format!("f{i}"));
            assert_eq!(*rank, (geoms.len() - i) as f64);
            assert_eq!(geom, &geoms[i], "canonical geometry must be verbatim");
        }

        // Report canonical level feature count matches input.
        assert_eq!(report.input_features, geoms.len());
        assert_eq!(report.levels[canonical].feature_count, geoms.len());
    }

    #[test]
    fn duplicating_coarse_levels_monotone() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 10,
            },
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();

        // Feature and vertex counts are monotonically non-decreasing coarse→fine.
        for w in report.levels.windows(2) {
            assert!(
                w[0].feature_count <= w[1].feature_count,
                "feature counts not monotone: {:?}",
                report.levels
            );
            assert!(
                w[0].vertex_count <= w[1].vertex_count,
                "vertex counts not monotone: {:?}",
                report.levels
            );
        }
        // Canonical has all features.
        assert_eq!(report.levels.last().unwrap().feature_count, geoms.len());

        // level column consistent with footer bands (validator covers this).
        assert!(validate_file(tout.path()).unwrap().is_valid());
    }

    #[test]
    fn partitioning_total_equals_input() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            mode: Mode::Partitioning,
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();

        let vr = validate_file(tout.path()).unwrap();
        assert!(
            vr.is_valid(),
            "failures: {:?}",
            vr.failures().collect::<Vec<_>>()
        );

        let reader = OverviewReader::open(tout.path()).unwrap();
        assert_eq!(reader.mode(), Mode::Partitioning);

        // Total rows across all levels == input (each feature exactly once).
        assert_eq!(report.total_rows, geoms.len());

        // Union of all levels reproduces the input id set.
        let mut all_ids = Vec::new();
        for level in 0..reader.num_levels() {
            for (id, _, _, _) in read_level_rows(&reader, level) {
                all_ids.push(id);
            }
        }
        all_ids.sort();
        assert_eq!(all_ids, (0..geoms.len() as i64).collect::<Vec<_>>());

        // Partitioning: geometry verbatim at every level.
        for level in 0..reader.num_levels() {
            for (id, _, _, geom) in read_level_rows(&reader, level) {
                assert_eq!(geom, geoms[id as usize], "partitioning geometry verbatim");
            }
        }
    }

    #[test]
    fn explicit_gsd_list_works() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::Gsds(vec![gsd(3), gsd(6), gsd(9)]),
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        assert!(validate_file(tout.path()).unwrap().is_valid());
        // gsds recorded strictly decreasing.
        for w in report.levels.windows(2) {
            assert!(w[0].gsd > w[1].gsd);
        }
        assert_eq!(report.levels.last().unwrap().feature_count, geoms.len());
    }

    #[test]
    fn default_gsd_base_footer_gsds_match_const() {
        // Regression: a default-flag (gsd_base == GSD_TILE_BASE) conversion must
        // produce footer GSDs byte-identical to the constant-base `gsd(z)` —
        // i.e. this knob is inert at its default, so default output is unchanged.
        use crate::overview::level::gsd;
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            ..Default::default()
        };
        assert_eq!(
            opts.gsd_base, GSD_TILE_BASE,
            "default must be the const base"
        );
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();

        let reader = OverviewReader::open(tout.path()).unwrap();
        // Footer GSDs equal the const-base gsd(z) for each level's zoom, exactly.
        for level in &reader.meta().levels {
            let z = level.zoom.expect("zoom-range plan records zooms");
            assert_eq!(level.gsd, gsd(z), "footer gsd must match const gsd(z={z})");
        }
        // Default base is NOT echoed into provenance (implied by the GSDs).
        assert_eq!(
            reader.meta().generalization.as_ref().unwrap().gsd_base,
            None,
            "default gsd_base must be absent from provenance"
        );
    }

    #[test]
    fn nondefault_gsd_base_scales_footer_gsds() {
        // Footer GSDs scale with `--gsd-base`: doubling the base halves every
        // level GSD, and the non-default base is recorded in provenance.
        use crate::overview::level::{gsd_with_base, GSD_TILE_BASE};
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let base = GSD_TILE_BASE * 2.0;
        let opts = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            gsd_base: base,
            ..Default::default()
        };
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();

        let reader = OverviewReader::open(tout.path()).unwrap();
        assert!(validate_file(tout.path()).unwrap().is_valid());
        for level in &reader.meta().levels {
            let z = level.zoom.unwrap();
            assert_eq!(
                level.gsd,
                gsd_with_base(z, base),
                "scaled footer gsd (z={z})"
            );
        }
        // Non-default base recorded in provenance.
        assert_eq!(
            reader.meta().generalization.as_ref().unwrap().gsd_base,
            Some(base),
            "non-default gsd_base must be recorded"
        );
    }

    #[test]
    fn rejects_unsupported_crs() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();

        // EPSG:32633 (UTM 33N) PROJJSON: neither 4326 nor 3857.
        // Note: no "WGS 84" in the name — `is_wgs84_projjson` name-matches that.
        let projjson = serde_json::json!({
            "type": "ProjectedCRS",
            "name": "UTM zone 33N",
            "id": { "authority": "EPSG", "code": 32633 }
        });
        let md = geoarrow::datatypes::Metadata::new(
            geoarrow::datatypes::Crs::from_projjson(projjson),
            None,
        );
        write_input(tin.path(), &geoms, false, Some(md));

        let opts = ConvertOptions::default();
        let err = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap_err();
        assert!(
            matches!(err, ConvertError::UnsupportedCrs { .. }),
            "expected UnsupportedCrs, got {err:?}"
        );
    }

    #[test]
    fn renames_existing_level_column() {
        // #288: an input `level` property must be auto-renamed (not rejected)
        // so flagship data (Overture buildings' floor-number `level`) converts.
        // The reserved `level` output column stays authoritative; the source
        // column becomes `level_`.
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, true, None);

        let opts = ConvertOptions::default();
        convert_to_overviews(tin.path(), tout.path(), &opts)
            .expect("colliding `level` column must be auto-renamed, not rejected");

        // Output validates and carries exactly one authoritative `level` column
        // plus the renamed source column `level_`.
        let report = crate::overview::check::validate_file(tout.path()).unwrap();
        assert!(
            report.is_valid(),
            "failures: {:?}",
            report.failures().collect::<Vec<_>>()
        );
        let names = output_column_names(tout.path());
        assert_eq!(
            names
                .iter()
                .filter(|n| n.eq_ignore_ascii_case("level"))
                .count(),
            1,
            "exactly one authoritative `level` column, names={names:?}"
        );
        assert!(
            names.iter().any(|n| n == "level_"),
            "renamed source column `level_` present, names={names:?}"
        );
    }

    #[test]
    fn resolver_renames_and_loops_suffix() {
        // `level` collides; `level_` already exists → the suffix loop keeps
        // appending until free (`level__`). Order and other columns are kept.
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("level", DataType::Int32, false),
            Field::new("level_", DataType::Int32, false),
            Field::new("geometry", DataType::Binary, false),
        ]));
        let mut opts = ConvertOptions::default();
        let (out, renames) = resolve_reserved_column_collisions(&schema, &mut opts);
        assert_eq!(renames, vec![("level".to_string(), "level__".to_string())]);
        let names: Vec<_> = out.fields().iter().map(|f| f.name().clone()).collect();
        assert_eq!(names, vec!["id", "level__", "level_", "geometry"]);
    }

    #[test]
    fn resolver_rewrites_option_column_references() {
        // A `--sort-key` that named the (case-insensitive) reserved column must
        // follow the rename, or ranking would fail with SortKeyColumnMissing.
        let schema = Arc::new(Schema::new(vec![
            Field::new("level", DataType::Int32, false),
            Field::new("geometry", DataType::Binary, false),
        ]));
        let mut opts = ConvertOptions {
            sort_key: Some("LEVEL".to_string()),
            ..Default::default()
        };
        let (_out, renames) = resolve_reserved_column_collisions(&schema, &mut opts);
        assert_eq!(renames.len(), 1);
        assert_eq!(opts.sort_key.as_deref(), Some("level_"));
    }

    #[test]
    fn resolver_noop_when_no_collision() {
        // No reserved-name collision → same `Arc` returned (cheap no-op) and no
        // renames.
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("geometry", DataType::Binary, false),
        ]));
        let mut opts = ConvertOptions::default();
        let (out, renames) = resolve_reserved_column_collisions(&schema, &mut opts);
        assert!(renames.is_empty());
        assert!(Arc::ptr_eq(&out, &schema));
    }

    #[test]
    fn resolver_reserves_count_columns_only_when_enabled() {
        // `point_count` is a normal passthrough property unless clustering is
        // on; likewise `coalesced_count` needs coalescing.
        let schema = Arc::new(Schema::new(vec![
            Field::new("point_count", DataType::Int64, false),
            Field::new("geometry", DataType::Binary, false),
        ]));
        // Clustering off, coalescing off → no collision.
        let mut off = ConvertOptions {
            cluster: false,
            coalesce_lines: false,
            ..Default::default()
        };
        let (_out, renames) = resolve_reserved_column_collisions(&schema, &mut off);
        assert!(
            renames.is_empty(),
            "point_count is a passthrough without --cluster"
        );
        // Clustering on → reserved, so renamed.
        let mut on = ConvertOptions {
            cluster: true,
            ..Default::default()
        };
        let (_out, renames) = resolve_reserved_column_collisions(&schema, &mut on);
        assert_eq!(
            renames,
            vec![("point_count".to_string(), "point_count_".to_string())]
        );
    }

    /// Write a GeoParquet file with `id` (Int64) + `road_class` (Utf8) +
    /// geometry, for the class-ranking tests. Uses the default (WGS84) CRS.
    fn write_class_input(path: &Path, geoms: &[Geometry<f64>], classes: &[Option<&str>]) {
        let n = geoms.len();
        assert_eq!(n, classes.len());
        let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
        let class = StringArray::from(classes.to_vec());
        let geom_arr = build_geometry_array(geoms);
        let geom_field = geom_arr.data_type().to_field("geometry", true);

        let fields = vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(Field::new("road_class", DataType::Utf8, true)),
            Arc::new(geom_field),
        ];
        let columns: Vec<Arc<dyn Array>> =
            vec![Arc::new(id), Arc::new(class), geom_arr.to_array_ref()];
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

        let gpq_options = GeoParquetWriterOptionsBuilder::default()
            .set_encoding(GeoParquetWriterEncoding::WKB)
            .set_generate_covering(true)
            .build();
        let encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
        let target_schema = encoder.target_schema();
        let file = std::fs::File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, target_schema, None).unwrap();
        let mut encoder = encoder;
        let encoded = encoder.encode_record_batch(&batch).unwrap();
        writer.write(&encoded).unwrap();
        writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
        writer.close().unwrap();
    }

    /// Coarsest (min) level each `id` appears at, scanned across all levels.
    fn min_level_by_id(reader: &OverviewReader) -> std::collections::HashMap<i64, usize> {
        use arrow_array::cast::AsArray;
        use arrow_array::types::Int64Type;
        let mut out: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
        for level in 0..reader.num_levels() {
            let rdr = reader.read_level(level, None).unwrap();
            for batch in rdr {
                let batch = batch.unwrap();
                let ids = batch
                    .column(batch.schema().index_of("id").unwrap())
                    .as_primitive::<Int64Type>()
                    .clone();
                for i in 0..batch.num_rows() {
                    let id = ids.value(i);
                    out.entry(id)
                        .and_modify(|l| *l = (*l).min(level))
                        .or_insert(level);
                }
            }
        }
        out
    }

    // --- Q1 class-ranking unit tests ----------------------------------------

    #[test]
    fn class_rank_maps_named_unknown_null() {
        // named → its rank; present-but-unlisted → unknown_rank; null → None.
        let col = StringArray::from(vec![
            Some("motorway"),
            Some("driveway"), // unlisted → unknown_rank
            None,             // null → None
        ]);
        let ranking = ClassRanking {
            column: "road_class".to_string(),
            ranks: vec![("motorway".to_string(), 5.0)],
            unknown_rank: 0.0,
        };
        let keys = extract_class_ranks(&col, &ranking).unwrap();
        assert_eq!(keys, vec![Some(5.0), Some(0.0), None]);
    }

    #[test]
    fn class_rank_rejects_non_string_column() {
        let col = Float64Array::from(vec![1.0, 2.0]);
        let ranking = ClassRanking {
            column: "road_class".to_string(),
            ranks: vec![("x".to_string(), 1.0)],
            unknown_rank: 0.0,
        };
        let err = extract_class_ranks(&col, &ranking).unwrap_err();
        assert!(matches!(err, ConvertError::ClassRankColumnNotString { .. }));
    }

    #[test]
    fn overture_road_ranking_spine_is_ordered() {
        let cr = overture_road_ranking("road_class".to_string());
        let rank = |v: &str| -> f64 {
            cr.ranks
                .iter()
                .find(|(k, _)| k == v)
                .map(|(_, r)| *r)
                .unwrap()
        };
        // Spine strictly descending.
        let spine = [
            "motorway",
            "trunk",
            "primary",
            "secondary",
            "tertiary",
            "residential",
            "unclassified",
            "service",
        ];
        for w in spine.windows(2) {
            assert!(rank(w[0]) > rank(w[1]), "{} !> {}", w[0], w[1]);
        }
        // Tail classes all rank below service, above unknown_rank.
        for tail in ["living_street", "footway", "path", "cycleway", "track"] {
            assert!(rank(tail) < rank("service"), "{tail} !< service");
            assert!(rank(tail) > cr.unknown_rank, "{tail} !> unknown");
        }
        // Rail / literal-unknown are not named → fall to unknown_rank.
        assert!(cr.ranks.iter().all(|(k, _)| k != "standard_gauge"));
    }

    // --- Q1 mutual-exclusion + auto-detection tests -------------------------

    #[test]
    fn sort_key_and_class_ranking_conflict() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            sort_key: Some("rank".to_string()),
            class_ranking: Some(ClassRanking {
                column: "road_class".to_string(),
                ranks: vec![("motorway".to_string(), 1.0)],
                unknown_rank: 0.0,
            }),
            ..Default::default()
        };
        let err = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap_err();
        assert!(matches!(err, ConvertError::RankingConflict));
    }

    /// Lines with 5+ distinct Overture road classes, spread apart so each wins
    /// its own coarse cell — used to exercise auto-detection provenance.
    fn overture_line_geoms() -> (Vec<Geometry<f64>>, Vec<Option<&'static str>>) {
        let classes = [
            "motorway",
            "trunk",
            "primary",
            "residential",
            "footway",
            "service",
        ];
        let mut geoms = Vec::new();
        let mut cls = Vec::new();
        for (i, c) in classes.iter().enumerate() {
            let base = i as f64 * 2.0; // far apart → distinct cells
            let ls = LineString::from(vec![(base, 0.0), (base + 0.5, 0.3)]);
            geoms.push(Geometry::LineString(ls));
            cls.push(Some(*c));
        }
        (geoms, cls)
    }

    fn ranking_mode_of(path: &Path) -> String {
        let reader = OverviewReader::open(path).unwrap();
        reader
            .meta()
            .generalization
            .as_ref()
            .unwrap()
            .ranking
            .as_ref()
            .unwrap()
            .mode
            .clone()
    }

    #[test]
    fn auto_detect_overture_roads_triggers() {
        let (geoms, classes) = overture_line_geoms();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_class_input(tin.path(), &geoms, &classes);

        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 10,
            },
            ..Default::default()
        };
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        assert_eq!(ranking_mode_of(tout.path()), "auto-overture-roads");
    }

    #[test]
    fn auto_detect_disabled_falls_back_to_size() {
        let (geoms, classes) = overture_line_geoms();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_class_input(tin.path(), &geoms, &classes);

        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 10,
            },
            no_auto_rank: true,
            ..Default::default()
        };
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        assert_eq!(ranking_mode_of(tout.path()), "size-fallback");
    }

    #[test]
    fn auto_detect_non_trigger_without_class_column() {
        // synthetic_geometries has no road_class column → size fallback.
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions::default();
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        assert_eq!(ranking_mode_of(tout.path()), "size-fallback");
    }

    #[test]
    fn class_rank_provenance_recorded() {
        let (geoms, classes) = overture_line_geoms();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_class_input(tin.path(), &geoms, &classes);

        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 10,
            },
            class_ranking: Some(ClassRanking {
                column: "road_class".to_string(),
                ranks: vec![("motorway".to_string(), 5.0), ("footway".to_string(), 1.0)],
                unknown_rank: 0.0,
            }),
            ..Default::default()
        };
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        let reader = OverviewReader::open(tout.path()).unwrap();
        let r = reader
            .meta()
            .generalization
            .as_ref()
            .unwrap()
            .ranking
            .clone()
            .unwrap();
        assert_eq!(r.mode, "class-ranking");
        assert_eq!(r.column.as_deref(), Some("road_class"));
        let ranks = r.ranks.unwrap();
        assert_eq!(ranks.len(), 2);
        assert_eq!(ranks.get("motorway"), Some(&5.0));
        assert_eq!(ranks.get("footway"), Some(&1.0));
        assert_eq!(r.unknown_rank, Some(0.0));

        // §3.5 (v0.2.0): the footer JSON carries ranks as an object map, not
        // an array of pairs.
        let json = overviews_footer_json(tout.path());
        assert!(
            json.contains(r#""ranks":{"footway":1.0,"motorway":5.0}"#),
            "footer must serialize ranks as an object map, got {json}"
        );
    }

    // --- Q1 regression: high class beats larger low class in a shared cell ---

    #[test]
    fn high_class_small_feature_wins_coarse_cell() {
        // Two lines sharing one coarse cell. The high-class line is SMALLER
        // (shorter bbox diagonal) than the low-class line. Under size-only
        // ranking the big low-class line would win the coarse level; with class
        // ranking the small high-class line must win it (coarser min_level).
        // Both clear the level-0 line visibility gate.
        //
        // 4326 units. gsd(4) ≈ 2445.98 m ⇒ 0.02197°; line gate = 2·gsd ⇒
        // 0.04394°; cell size = 2·gsd ⇒ 0.04394°. Both bboxes centered at
        // (0.02, 0.01) ⇒ same cell (0,0).
        let big_low = Geometry::LineString(LineString::from(vec![
            (-0.03, -0.04),
            (0.07, 0.06), // diag ≈ 0.1414° (well over gate), larger
        ]));
        let small_high = Geometry::LineString(LineString::from(vec![
            (0.0, 0.0),
            (0.04, 0.02), // diag ≈ 0.0447° (just over gate), smaller
        ]));
        let geoms = vec![big_low, small_high];
        let classes = vec![Some("footway"), Some("motorway")];

        let tin = tempfile::NamedTempFile::new().unwrap();

        // Baseline: size-fallback → the BIG low-class line (id 0) wins coarse.
        {
            let tout = tempfile::NamedTempFile::new().unwrap();
            write_class_input(tin.path(), &geoms, &classes);
            let opts = ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 4,
                    max_zoom: 10,
                },
                no_auto_rank: true, // force size fallback
                ..Default::default()
            };
            convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
            let reader = OverviewReader::open(tout.path()).unwrap();
            let ml = min_level_by_id(&reader);
            assert!(
                ml[&0] < ml[&1],
                "size fallback: big low-class (id0) should win coarse, got {ml:?}"
            );
        }

        // Class ranking: the SMALL high-class line (id 1) wins the coarse cell.
        {
            let tout = tempfile::NamedTempFile::new().unwrap();
            write_class_input(tin.path(), &geoms, &classes);
            let opts = ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 4,
                    max_zoom: 10,
                },
                class_ranking: Some(ClassRanking {
                    column: "road_class".to_string(),
                    ranks: vec![("motorway".to_string(), 5.0), ("footway".to_string(), 1.0)],
                    unknown_rank: 0.0,
                }),
                ..Default::default()
            };
            convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
            let reader = OverviewReader::open(tout.path()).unwrap();
            let ml = min_level_by_id(&reader);
            assert!(
                ml[&1] < ml[&0],
                "class ranking: small high-class (id1) must win coarse, got {ml:?}"
            );
            assert_eq!(ml[&1], 0, "high-class line should reach the coarsest level");
        }
    }

    // --- Q2 density budget --------------------------------------------------

    fn density_provenance_of(path: &Path) -> Option<crate::overview::level::DensityProvenance> {
        let reader = OverviewReader::open(path).unwrap();
        reader
            .meta()
            .generalization
            .as_ref()
            .unwrap()
            .density_drop
            .clone()
    }

    /// Many points on a grid, spread so cell-winner keeps them all at fine
    /// levels — enough features that the density budget binds at a mid level.
    fn grid_points(n: usize) -> Vec<Geometry<f64>> {
        (0..n)
            .map(|i| {
                let x = (i % 40) as f64 * 0.3 - 6.0;
                let y = (i / 40) as f64 * 0.3 - 6.0;
                Geometry::Point(Point::new(x, y))
            })
            .collect()
    }

    #[test]
    fn density_provenance_recorded_by_default() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            ..Default::default()
        };
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        let d = density_provenance_of(tout.path()).expect("default run records density_drop");
        assert_eq!(d.drop_rate, DensityBudgetConfig::default().drop_rate);
        assert_eq!(d.gamma, DensityBudgetConfig::default().gamma);
        assert_eq!(d.supercell_gsd_factor, SUPERCELL_GSD_FACTOR);
    }

    #[test]
    fn density_disabled_omits_provenance_and_keeps_canonical() {
        let geoms = grid_points(600);
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            density: DensityBudgetConfig {
                enabled: false,
                ..DensityBudgetConfig::default()
            },
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        // Off switch: no density_drop provenance (footer matches pre-Q2).
        assert!(
            density_provenance_of(tout.path()).is_none(),
            "disabled budget must not emit provenance"
        );
        assert!(validate_file(tout.path()).unwrap().is_valid());
        assert_eq!(report.levels.last().unwrap().feature_count, geoms.len());
    }

    #[test]
    fn density_thins_midlevels_keeps_canonical() {
        let geoms = grid_points(600);
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout_on = tempfile::NamedTempFile::new().unwrap();
        let tout_off = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            ..Default::default()
        };
        let on = convert_to_overviews(tin.path(), tout_on.path(), &base).unwrap();
        let off = convert_to_overviews(
            tin.path(),
            tout_off.path(),
            &ConvertOptions {
                density: DensityBudgetConfig {
                    enabled: false,
                    ..DensityBudgetConfig::default()
                },
                ..base.clone()
            },
        )
        .unwrap();

        // Canonical fidelity: both keep every feature at the finest level.
        assert_eq!(on.levels.last().unwrap().feature_count, geoms.len());
        assert_eq!(off.levels.last().unwrap().feature_count, geoms.len());
        assert!(validate_file(tout_on.path()).unwrap().is_valid());

        // The budget removes mid-level features that cell-winner alone retained:
        // the on-budget run writes strictly fewer total rows than the off run.
        assert!(
            on.total_rows < off.total_rows,
            "density budget should thin mid levels: on={} off={}",
            on.total_rows,
            off.total_rows
        );
        // Counts remain monotone non-decreasing coarse→fine under the budget.
        for w in on.levels.windows(2) {
            assert!(w[0].feature_count <= w[1].feature_count);
        }
    }

    // --- H3 streaming / in-memory equivalence -------------------------------

    /// Raw `geo:overviews` footer JSON of an overview file.
    fn overviews_footer_json(path: &Path) -> String {
        let file = std::fs::File::open(path).unwrap();
        let b = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        b.metadata()
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .find(|kv| kv.key == crate::overview::level::OVERVIEWS_KEY)
            .expect("geo:overviews key")
            .value
            .clone()
            .unwrap()
    }

    /// Convert the same input through the in-memory and streaming paths and
    /// assert equivalent outputs: per-level feature/vertex counts, footer
    /// metadata (byte-identical JSON), and row values in order at every level.
    /// Assumes a `write_input`-shaped file (id/name/rank/geometry columns).
    fn assert_streaming_equivalent(input: &Path, base: &ConvertOptions) {
        let mem_out = tempfile::NamedTempFile::new().unwrap();
        let stream_out = tempfile::NamedTempFile::new().unwrap();

        let mem_opts = ConvertOptions {
            streaming: false,
            ..base.clone()
        };
        let stream_opts = ConvertOptions {
            streaming: true,
            ..base.clone()
        };
        let mem = convert_to_overviews(input, mem_out.path(), &mem_opts).unwrap();
        let strm = convert_to_overviews(input, stream_out.path(), &stream_opts).unwrap();

        // Reports agree on everything but duration.
        assert_eq!(mem.mode, strm.mode);
        assert_eq!(mem.input_features, strm.input_features);
        assert_eq!(mem.total_rows, strm.total_rows);
        assert_eq!(mem.total_vertices, strm.total_vertices);
        assert_eq!(mem.levels.len(), strm.levels.len());
        for (a, b) in mem.levels.iter().zip(&strm.levels) {
            assert_eq!(a.level, b.level);
            assert_eq!(a.gsd, b.gsd, "level {} gsd", a.level);
            assert_eq!(a.zoom, b.zoom, "level {} zoom", a.level);
            assert_eq!(
                a.feature_count, b.feature_count,
                "level {} feature count",
                a.level
            );
            assert_eq!(
                a.vertex_count, b.vertex_count,
                "level {} vertex count",
                a.level
            );
        }

        // Footer metadata byte-identical; both files validate.
        assert_eq!(
            overviews_footer_json(mem_out.path()),
            overviews_footer_json(stream_out.path()),
            "geo:overviews footers differ"
        );
        assert!(validate_file(mem_out.path()).unwrap().is_valid());
        assert!(validate_file(stream_out.path()).unwrap().is_valid());

        // Row-level equality per level (ids, attributes, geometry, order).
        let mr = OverviewReader::open(mem_out.path()).unwrap();
        let sr = OverviewReader::open(stream_out.path()).unwrap();
        assert_eq!(mr.num_levels(), sr.num_levels());
        for level in 0..mr.num_levels() {
            assert_eq!(
                read_level_rows(&mr, level),
                read_level_rows(&sr, level),
                "level {level} rows differ"
            );
        }
    }

    /// Assert two overview output files are structurally + logically identical:
    /// same row-group layout (the deterministic on-disk structure), same footer
    /// `geo:overviews` metadata, and same per-level row content (ids, attrs,
    /// geometry, order). This is the meaningful "byte-identical" invariant — the
    /// Parquet writer's footer metadata region is not byte-deterministic
    /// run-to-run (serial-vs-serial also differs by a few bytes there), so a raw
    /// `Vec<u8>` compare is not a valid equivalence check.
    fn assert_outputs_equivalent(a: &Path, b: &Path, ctx: &str) {
        use parquet::file::reader::{FileReader, SerializedFileReader};

        // Row-group layout (num groups, rows per group, column count).
        let layout = |p: &Path| -> Vec<(i64, usize)> {
            let r = SerializedFileReader::new(std::fs::File::open(p).unwrap()).unwrap();
            let md = r.metadata();
            (0..md.num_row_groups())
                .map(|i| (md.row_group(i).num_rows(), md.row_group(i).columns().len()))
                .collect()
        };
        assert_eq!(layout(a), layout(b), "{ctx}: row-group layout differs");

        // Footer geo:overviews metadata (semantic).
        assert_eq!(
            overviews_footer_json(a),
            overviews_footer_json(b),
            "{ctx}: geo:overviews footer differs"
        );

        // Per-level row content + order.
        let ra = OverviewReader::open(a).unwrap();
        let rb = OverviewReader::open(b).unwrap();
        assert_eq!(
            ra.num_levels(),
            rb.num_levels(),
            "{ctx}: level count differs"
        );
        for level in 0..ra.num_levels() {
            assert_eq!(
                read_level_rows(&ra, level),
                read_level_rows(&rb, level),
                "{ctx}: level {level} rows differ"
            );
        }
    }

    /// The single-read pipelined engine (#213/#212) must produce output
    /// identical to the serial per-level-re-read reference — across both modes,
    /// both sink backings (speed = RAM, bounded = Arrow IPC spill), and the
    /// clustering feature. The bounded case also proves the spill round-trip is
    /// lossless.
    #[test]
    fn pipelined_matches_serial() {
        use super::super::level::MemoryProfile;
        use super::super::stream::Pass2Strategy;

        // A polygon set (duplicating/clustering simplify paths) and a dense
        // point grid (partitioning fans many features across several levels, so
        // the engine buffers/spills multiple non-finest levels).
        let poly_in = tempfile::NamedTempFile::new().unwrap();
        write_input(poly_in.path(), &synthetic_geometries(), false, None);
        let grid_in = tempfile::NamedTempFile::new().unwrap();
        write_input(grid_in.path(), &grid_points(600), false, None);

        let cases: Vec<(&str, &Path, ConvertOptions)> = vec![
            (
                "duplicating",
                poly_in.path(),
                ConvertOptions {
                    mode: Mode::Duplicating,
                    levels: LevelPlan::ZoomRange {
                        min_zoom: 1,
                        max_zoom: 10,
                    },
                    ..Default::default()
                },
            ),
            (
                "partitioning",
                grid_in.path(),
                ConvertOptions {
                    mode: Mode::Partitioning,
                    levels: LevelPlan::ZoomRange {
                        min_zoom: 1,
                        max_zoom: 9,
                    },
                    ..Default::default()
                },
            ),
            (
                "clustering",
                grid_in.path(),
                ConvertOptions {
                    mode: Mode::Duplicating,
                    cluster: true,
                    levels: LevelPlan::ZoomRange {
                        min_zoom: 1,
                        max_zoom: 9,
                    },
                    ..Default::default()
                },
            ),
        ];

        for (name, input, base) in &cases {
            let serial_out = tempfile::NamedTempFile::new().unwrap();
            convert_to_overviews_strategy(input, serial_out.path(), base, Pass2Strategy::Serial)
                .unwrap();

            for profile in [MemoryProfile::Speed, MemoryProfile::Bounded] {
                let opts = ConvertOptions {
                    profile,
                    ..base.clone()
                };
                let piped_out = tempfile::NamedTempFile::new().unwrap();
                convert_to_overviews_strategy(
                    input,
                    piped_out.path(),
                    &opts,
                    Pass2Strategy::Pipelined,
                )
                .unwrap();
                assert_outputs_equivalent(
                    serial_out.path(),
                    piped_out.path(),
                    &format!("{name}/{profile:?}"),
                );
            }
        }
    }

    /// Pipelined output must be invariant to batching/overlap knobs
    /// (`read_batch_size`, `in_flight_batches`) — proving the ordered-sink /
    /// no-reorder-buffer invariant holds regardless of how the single read is
    /// chunked or how many batches overlap in flight.
    #[test]
    fn pipelined_invariant_to_batching_knobs() {
        use super::super::stream::Pass2Strategy;

        let geoms = grid_points(600);
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let base = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 9,
            },
            ..Default::default()
        };

        let convert = |read_batch_size: usize, in_flight_batches: usize| {
            let opts = ConvertOptions {
                read_batch_size,
                in_flight_batches,
                ..base.clone()
            };
            let out = tempfile::NamedTempFile::new().unwrap();
            convert_to_overviews_strategy(tin.path(), out.path(), &opts, Pass2Strategy::Pipelined)
                .unwrap();
            out
        };

        let reference = convert(7, 1);
        for (rbs, ifb) in [(64usize, 4usize), (4096, 8)] {
            let candidate = convert(rbs, ifb);
            assert_outputs_equivalent(
                reference.path(),
                candidate.path(),
                &format!("read_batch_size={rbs} in_flight_batches={ifb}"),
            );
        }
    }

    #[test]
    fn streaming_matches_in_memory_duplicating() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let base = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 10,
            },
            ..Default::default()
        };
        assert_streaming_equivalent(tin.path(), &base);
    }

    #[test]
    fn streaming_matches_in_memory_partitioning() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let base = ConvertOptions {
            mode: Mode::Partitioning,
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            ..Default::default()
        };
        assert_streaming_equivalent(tin.path(), &base);
    }

    #[test]
    fn streaming_matches_with_small_read_batches_and_density_budget() {
        // Many features + a tiny read batch: exercises batch-boundary handling
        // in both passes AND the Q2 density-budget cuts in the winner tables.
        let geoms = grid_points(600);
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            read_batch_size: 7,
            ..Default::default()
        };
        assert_streaming_equivalent(tin.path(), &base);
    }

    #[test]
    fn streaming_matches_with_explicit_sort_key() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            sort_key: Some("rank".to_string()),
            ..Default::default()
        };
        assert_streaming_equivalent(tin.path(), &base);
    }

    #[test]
    fn streaming_auto_rank_matches_in_memory() {
        // Auto-detected Overture road-class ranking must resolve identically in
        // the streaming pass-1 (incremental vocab scan) and in-memory paths.
        let (geoms, classes) = overture_line_geoms();
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_class_input(tin.path(), &geoms, &classes);

        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 10,
            },
            read_batch_size: 2,
            ..Default::default()
        };
        let mem_out = tempfile::NamedTempFile::new().unwrap();
        let stream_out = tempfile::NamedTempFile::new().unwrap();
        convert_to_overviews(
            tin.path(),
            mem_out.path(),
            &ConvertOptions {
                streaming: false,
                ..base.clone()
            },
        )
        .unwrap();
        convert_to_overviews(tin.path(), stream_out.path(), &base).unwrap();

        assert_eq!(ranking_mode_of(mem_out.path()), "auto-overture-roads");
        assert_eq!(ranking_mode_of(stream_out.path()), "auto-overture-roads");
        assert_eq!(
            overviews_footer_json(mem_out.path()),
            overviews_footer_json(stream_out.path())
        );
        let mr = OverviewReader::open(mem_out.path()).unwrap();
        let sr = OverviewReader::open(stream_out.path()).unwrap();
        assert_eq!(min_level_by_id(&mr), min_level_by_id(&sr));
    }

    // --- Q4 point clustering -------------------------------------------------

    /// Read the `point_count` column for one level, in row order.
    fn read_point_counts(reader: &OverviewReader, level: usize) -> Vec<i64> {
        use arrow_array::cast::AsArray;
        use arrow_array::types::Int64Type;
        let rdr = reader.read_level(level, None).unwrap();
        let mut out = Vec::new();
        for batch in rdr {
            let batch = batch.unwrap();
            let idx = batch.schema().index_of("point_count").unwrap();
            let col = batch.column(idx).as_primitive::<Int64Type>().clone();
            assert_eq!(col.null_count(), 0, "point_count must be NOT NULL");
            out.extend(col.values().iter().copied());
        }
        out
    }

    /// Read a Float64 column for one level, in row order (None = null).
    fn read_f64_column(reader: &OverviewReader, level: usize, name: &str) -> Vec<Option<f64>> {
        use arrow_array::cast::AsArray;
        use arrow_array::types::Float64Type;
        let rdr = reader.read_level(level, None).unwrap();
        let mut out = Vec::new();
        for batch in rdr {
            let batch = batch.unwrap();
            let idx = batch.schema().index_of(name).unwrap();
            let col = batch.column(idx).as_primitive::<Float64Type>().clone();
            for i in 0..col.len() {
                out.push(if col.is_null(i) {
                    None
                } else {
                    Some(col.value(i))
                });
            }
        }
        out
    }

    #[test]
    fn cluster_duplicating_end_to_end_counts_partition_every_level() {
        // 600 grid points, clustering on: every level's point_count sums to
        // the source count, canonical counts are all 1, file validates.
        let geoms = grid_points(600);
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            cluster: true,
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        let vr = validate_file(tout.path()).unwrap();
        assert!(
            vr.is_valid(),
            "failures: {:?}",
            vr.failures().collect::<Vec<_>>()
        );

        let reader = OverviewReader::open(tout.path()).unwrap();
        let canonical = reader.num_levels() - 1;
        for level in 0..reader.num_levels() {
            let counts = read_point_counts(&reader, level);
            assert!(counts.iter().all(|&c| c >= 1), "level {level} counts >= 1");
            assert_eq!(
                counts.iter().sum::<i64>(),
                geoms.len() as i64,
                "level {level}: sum(point_count) must equal source count"
            );
        }
        // Canonical: every cluster is a singleton.
        assert!(read_point_counts(&reader, canonical)
            .iter()
            .all(|&c| c == 1));
        // Density budget bites mid-levels: some coarse level must actually
        // cluster (a count > 1), or this test tests nothing.
        let coarse = read_point_counts(&reader, 0);
        assert!(coarse.iter().any(|&c| c > 1), "no clustering happened");
        assert!(coarse.len() < geoms.len());
        assert_eq!(report.levels.last().unwrap().feature_count, geoms.len());
    }

    /// §12.1 sum-invariant property across knob combinations: clustering
    /// crossed with the density budget (on/off), point-thinning grid sizes,
    /// and both pipelines (streaming / in-memory) — every produced file must
    /// partition the source point set at every level, pass the
    /// `cluster_sum_invariant` validator rule, and keep a singleton-only
    /// canonical band. Coalescing stays at its default (on): it only touches
    /// lines and must not interact with point accounting.
    #[test]
    fn cluster_sum_invariant_across_knob_combinations() {
        let geoms = grid_points(300);
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        for streaming in [true, false] {
            for density in [true, false] {
                for thinning in [2.0, 8.0] {
                    let tout = tempfile::NamedTempFile::new().unwrap();
                    let mut opts = ConvertOptions {
                        levels: LevelPlan::ZoomRange {
                            min_zoom: 0,
                            max_zoom: 6,
                        },
                        cluster: true,
                        streaming,
                        ..Default::default()
                    };
                    opts.density.enabled = density;
                    opts.assign.point_thinning = thinning;
                    let label =
                        format!("streaming={streaming} density={density} thinning={thinning}");
                    convert_to_overviews(tin.path(), tout.path(), &opts)
                        .unwrap_or_else(|e| panic!("{label}: {e}"));

                    let vr = validate_file(tout.path()).unwrap();
                    assert!(
                        vr.is_valid(),
                        "{label}: failures: {:?}",
                        vr.failures().collect::<Vec<_>>()
                    );
                    assert_eq!(
                        vr.check_passed("cluster_sum_invariant"),
                        Some(true),
                        "{label}"
                    );

                    let reader = OverviewReader::open(tout.path()).unwrap();
                    let canonical = reader.num_levels() - 1;
                    for level in 0..reader.num_levels() {
                        let counts = read_point_counts(&reader, level);
                        assert!(
                            !counts.is_empty(),
                            "{label}: level {level} thinned points to zero"
                        );
                        assert_eq!(
                            counts.iter().sum::<i64>(),
                            geoms.len() as i64,
                            "{label}: level {level} must partition the source set"
                        );
                    }
                    assert!(
                        read_point_counts(&reader, canonical)
                            .iter()
                            .all(|&c| c == 1),
                        "{label}: canonical band must be singleton-only"
                    );
                }
            }
        }
    }

    #[test]
    fn cluster_accumulate_sum_and_mean_consistent() {
        // The `rank` column (Float64, values n..1) accumulated as sum: every
        // level's Σ rank over rows must equal the source Σ (clusters
        // partition the source set and sum is additive). Canonical stays
        // verbatim.
        let geoms = grid_points(400);
        let n = geoms.len();
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);
        let source_sum: f64 = (1..=n).map(|v| v as f64).sum();

        // sum
        let tout = tempfile::NamedTempFile::new().unwrap();
        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            cluster: true,
            accumulate: vec![AccumulateSpec {
                column: "rank".to_string(),
                op: super::super::cluster::AccumulateOp::Sum,
            }],
            ..Default::default()
        };
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        let reader = OverviewReader::open(tout.path()).unwrap();
        for level in 0..reader.num_levels() {
            let ranks = read_f64_column(&reader, level, "rank");
            let total: f64 = ranks.iter().flatten().sum();
            assert!(
                (total - source_sum).abs() < 1e-6,
                "level {level}: rank sum {total} != source {source_sum}"
            );
        }
        // Canonical: verbatim source values (id i has rank n - i).
        let canonical = reader.num_levels() - 1;
        let rows = read_level_rows(&reader, canonical);
        for (id, _, rank, _) in rows {
            assert_eq!(rank, (n as i64 - id) as f64, "canonical rank verbatim");
        }

        // mean: Σ (mean_i × point_count_i) must reproduce the source sum at
        // every level (mean computed from source values, not mean-of-means).
        let tout2 = tempfile::NamedTempFile::new().unwrap();
        let opts_mean = ConvertOptions {
            accumulate: vec![AccumulateSpec {
                column: "rank".to_string(),
                op: super::super::cluster::AccumulateOp::Mean,
            }],
            ..opts.clone()
        };
        convert_to_overviews(tin.path(), tout2.path(), &opts_mean).unwrap();
        let reader2 = OverviewReader::open(tout2.path()).unwrap();
        for level in 0..reader2.num_levels() {
            let means = read_f64_column(&reader2, level, "rank");
            let counts = read_point_counts(&reader2, level);
            let total: f64 = means
                .iter()
                .zip(&counts)
                .map(|(m, &c)| m.unwrap() * c as f64)
                .sum();
            assert!(
                (total - source_sum).abs() < 1e-6,
                "level {level}: Σ mean×count {total} != source {source_sum}"
            );
        }
    }

    #[test]
    fn cluster_footer_provenance_recorded() {
        let geoms = grid_points(100);
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            cluster: true,
            accumulate: vec![AccumulateSpec {
                column: "rank".to_string(),
                op: super::super::cluster::AccumulateOp::Mean,
            }],
            ..Default::default()
        };
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        let reader = OverviewReader::open(tout.path()).unwrap();
        let c = reader
            .meta()
            .generalization
            .as_ref()
            .unwrap()
            .clustering
            .clone()
            .expect("clustering provenance recorded");
        assert!(c.enabled);
        assert_eq!(c.point_count_column, "point_count");
        assert_eq!(c.accumulated.len(), 1);
        assert_eq!(c.accumulated[0].column, "rank");
        assert_eq!(c.accumulated[0].op, "mean");

        // Off by default: no clustering block, no point_count column.
        let tout_off = tempfile::NamedTempFile::new().unwrap();
        convert_to_overviews(tin.path(), tout_off.path(), &ConvertOptions::default()).unwrap();
        let r_off = OverviewReader::open(tout_off.path()).unwrap();
        assert!(r_off
            .meta()
            .generalization
            .as_ref()
            .unwrap()
            .clustering
            .is_none());
    }

    #[test]
    fn cluster_option_errors() {
        let geoms = grid_points(20);
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        // Partitioning + cluster is rejected.
        let err = convert_to_overviews(
            tin.path(),
            tout.path(),
            &ConvertOptions {
                mode: Mode::Partitioning,
                cluster: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, ConvertError::ClusterPartitioningUnsupported));

        // Accumulate without cluster is rejected.
        let err = convert_to_overviews(
            tin.path(),
            tout.path(),
            &ConvertOptions {
                accumulate: vec![AccumulateSpec {
                    column: "rank".to_string(),
                    op: super::super::cluster::AccumulateOp::Sum,
                }],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, ConvertError::AccumulateWithoutCluster));

        // Missing accumulate column is rejected (both pipelines).
        for streaming in [true, false] {
            let err = convert_to_overviews(
                tin.path(),
                tout.path(),
                &ConvertOptions {
                    cluster: true,
                    streaming,
                    accumulate: vec![AccumulateSpec {
                        column: "nonexistent".to_string(),
                        op: super::super::cluster::AccumulateOp::Sum,
                    }],
                    ..Default::default()
                },
            )
            .unwrap_err();
            assert!(
                matches!(err, ConvertError::AccumulateColumnMissing { .. }),
                "streaming={streaming}: got {err:?}"
            );
        }

        // Non-numeric accumulate column is rejected.
        let err = convert_to_overviews(
            tin.path(),
            tout.path(),
            &ConvertOptions {
                cluster: true,
                accumulate: vec![AccumulateSpec {
                    column: "name".to_string(),
                    op: super::super::cluster::AccumulateOp::Max,
                }],
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ConvertError::AccumulateColumnNotNumeric { .. }
        ));
    }

    #[test]
    fn cluster_renames_existing_point_count_column() {
        // #288: an input already carrying a `point_count` column (case-
        // insensitively) is auto-renamed when clustering, and is an ordinary
        // passthrough property when clustering is off.
        let geoms = grid_points(10);
        let n = geoms.len();
        let tin = tempfile::NamedTempFile::new().unwrap();
        {
            let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
            let pc = Int64Array::from(vec![7i64; n]);
            let geom_arr = build_geometry_array(&geoms);
            let geom_field = geom_arr.data_type().to_field("geometry", true);
            let fields = vec![
                Arc::new(Field::new("id", DataType::Int64, false)),
                Arc::new(Field::new("Point_Count", DataType::Int64, false)),
                Arc::new(geom_field),
            ];
            let columns: Vec<Arc<dyn Array>> =
                vec![Arc::new(id), Arc::new(pc), geom_arr.to_array_ref()];
            let schema = Arc::new(Schema::new(fields));
            let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
            let gpq_options = GeoParquetWriterOptionsBuilder::default()
                .set_encoding(GeoParquetWriterEncoding::WKB)
                .set_generate_covering(true)
                .build();
            let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
            let target_schema = encoder.target_schema();
            let file = std::fs::File::create(tin.path()).unwrap();
            let mut writer = ArrowWriter::try_new(file, target_schema, None).unwrap();
            writer
                .write(&encoder.encode_record_batch(&batch).unwrap())
                .unwrap();
            writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
            writer.close().unwrap();
        }

        let tout = tempfile::NamedTempFile::new().unwrap();
        convert_to_overviews(
            tin.path(),
            tout.path(),
            &ConvertOptions {
                cluster: true,
                ..Default::default()
            },
        )
        .expect("colliding `Point_Count` column must be auto-renamed, not rejected");
        let names = output_column_names(tout.path());
        assert!(
            names.iter().any(|n| n == "Point_Count_"),
            "renamed source column present, names={names:?}"
        );
        assert_eq!(
            names
                .iter()
                .filter(|n| n.eq_ignore_ascii_case("point_count"))
                .count(),
            1,
            "one authoritative `point_count`, names={names:?}"
        );

        // Without clustering the column is an ordinary passthrough property
        // (kept verbatim, no reserved column added).
        convert_to_overviews(tin.path(), tout.path(), &ConvertOptions::default()).unwrap();
        let names = output_column_names(tout.path());
        assert!(
            names.iter().any(|n| n == "Point_Count"),
            "passthrough column kept verbatim, names={names:?}"
        );
    }

    #[test]
    fn streaming_matches_in_memory_clustering() {
        // Clustering + accumulation must be byte-equivalent between the
        // streaming and in-memory pipelines, including with tiny read batches
        // and the density budget's orphan-cell handling.
        let geoms = grid_points(600);
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            read_batch_size: 7,
            cluster: true,
            accumulate: vec![
                AccumulateSpec {
                    column: "rank".to_string(),
                    op: super::super::cluster::AccumulateOp::Sum,
                },
                AccumulateSpec {
                    column: "rank".to_string(),
                    op: super::super::cluster::AccumulateOp::Mean,
                },
            ],
            ..Default::default()
        };
        // NOTE: two specs on one column — the LAST rewrite wins per column;
        // exercised here purely for pipeline equivalence.
        assert_streaming_equivalent(tin.path(), &base);

        // Explicitly compare point_count per level too (read_level_rows in
        // the shared helper does not include it).
        let mem_out = tempfile::NamedTempFile::new().unwrap();
        let stream_out = tempfile::NamedTempFile::new().unwrap();
        convert_to_overviews(
            tin.path(),
            mem_out.path(),
            &ConvertOptions {
                streaming: false,
                ..base.clone()
            },
        )
        .unwrap();
        convert_to_overviews(tin.path(), stream_out.path(), &base).unwrap();
        let mr = OverviewReader::open(mem_out.path()).unwrap();
        let sr = OverviewReader::open(stream_out.path()).unwrap();
        for level in 0..mr.num_levels() {
            assert_eq!(
                read_point_counts(&mr, level),
                read_point_counts(&sr, level),
                "level {level} point_count differs"
            );
        }
    }

    // --- Q3 line coalescing ---------------------------------------------------

    /// Read the `coalesced_count` column for one level, in row order.
    fn read_coalesced_counts(reader: &OverviewReader, level: usize) -> Vec<i32> {
        use arrow_array::cast::AsArray;
        use arrow_array::types::Int32Type;
        let rdr = reader.read_level(level, None).unwrap();
        let mut out = Vec::new();
        for batch in rdr {
            let batch = batch.unwrap();
            let idx = batch.schema().index_of("coalesced_count").unwrap();
            let col = batch.column(idx).as_primitive::<Int32Type>().clone();
            assert_eq!(col.null_count(), 0, "coalesced_count must be NOT NULL");
            out.extend(col.values().iter().copied());
        }
        out
    }

    /// A chain of `n` touching collinear segments (each 0.01° long) starting
    /// at (0,0), plus one far-away point. Each segment alone is below the
    /// coarse-level line visibility gate; the chain is well above it.
    fn fragment_chain_geoms(n: usize) -> Vec<Geometry<f64>> {
        let mut geoms: Vec<Geometry<f64>> = (0..n)
            .map(|i| {
                let x0 = i as f64 * 0.01;
                Geometry::LineString(LineString::from(vec![(x0, 0.0), (x0 + 0.01, 0.0)]))
            })
            .collect();
        geoms.push(Geometry::Point(Point::new(5.0, 5.0)));
        geoms
    }

    #[test]
    fn coalesce_reclaims_sub_visibility_fragments_and_keeps_canonical() {
        // z4 gsd ≈ 2446 m ⇒ gate 2·gsd ≈ 0.0439°. Segments are 0.01° (fail
        // alone); the 6-segment chain is 0.06° (passes). Without coalescing
        // the coarse level holds only the point; with it, point + one artery.
        let geoms = fragment_chain_geoms(6);
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 10,
            },
            no_auto_rank: true,
            ..Default::default() // coalescing is ON by default
        };

        // Baseline (opt-out): fragments vanish from the coarse level.
        let tout_off = tempfile::NamedTempFile::new().unwrap();
        let off = convert_to_overviews(
            tin.path(),
            tout_off.path(),
            &ConvertOptions {
                coalesce_lines: false,
                ..opts.clone()
            },
        )
        .unwrap();
        assert_eq!(
            off.levels[0].feature_count, 1,
            "without coalescing only the point survives level 0: {:?}",
            off.levels
        );

        // Coalescing (default): the chain survives as ONE feature, count 6.
        let tout = tempfile::NamedTempFile::new().unwrap();
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        assert_eq!(
            report.levels[0].feature_count, 2,
            "chain + point at level 0: {:?}",
            report.levels
        );

        let vr = validate_file(tout.path()).unwrap();
        assert!(
            vr.is_valid(),
            "failures: {:?}",
            vr.failures().collect::<Vec<_>>()
        );

        let reader = OverviewReader::open(tout.path()).unwrap();
        let counts0 = read_coalesced_counts(&reader, 0);
        let mut sorted = counts0.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![1, 6], "point=1, merged chain=6: {counts0:?}");

        // Canonical level: every source row verbatim, all counts 1.
        let canonical = reader.num_levels() - 1;
        let rows = read_level_rows(&reader, canonical);
        assert_eq!(rows.len(), geoms.len());
        for (id, _, _, geom) in &rows {
            assert_eq!(
                geom, &geoms[*id as usize],
                "canonical geometry verbatim (never coalesced)"
            );
        }
        assert!(read_coalesced_counts(&reader, canonical)
            .iter()
            .all(|&c| c == 1));
    }

    #[test]
    fn coalesce_groups_by_auto_detected_class() {
        // Two touching motorway segments merge; the footway touching the
        // same chain end does not (class mismatch). Extra far-away classed
        // lines trigger the Overture auto-detection vocab gate.
        let mut geoms = vec![
            Geometry::LineString(LineString::from(vec![(0.0, 0.0), (0.1, 0.0)])),
            Geometry::LineString(LineString::from(vec![(0.1, 0.0), (0.2, 0.0)])),
            Geometry::LineString(LineString::from(vec![(0.2, 0.0), (0.2, 0.1)])),
        ];
        let mut classes = vec![Some("motorway"), Some("motorway"), Some("footway")];
        for (i, c) in ["primary", "service", "residential"].iter().enumerate() {
            geoms.push(Geometry::LineString(LineString::from(vec![
                (3.0 + i as f64, 3.0),
                (3.1 + i as f64, 3.05),
            ])));
            classes.push(Some(*c));
        }
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_class_input(tin.path(), &geoms, &classes);

        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 10,
            },
            coalesce_lines: true,
            ..Default::default()
        };
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        assert_eq!(ranking_mode_of(tout.path()), "auto-overture-roads");

        let reader = OverviewReader::open(tout.path()).unwrap();
        let counts0 = read_coalesced_counts(&reader, 0);
        assert_eq!(
            counts0.iter().filter(|&&c| c == 2).count(),
            1,
            "exactly one 2-segment motorway chain: {counts0:?}"
        );
        assert!(
            counts0.iter().all(|&c| c <= 2),
            "footway never merges into the motorway chain: {counts0:?}"
        );
    }

    #[test]
    fn coalesce_inert_in_partitioning() {
        // Partitioning cannot represent merged chains (§13.5); with
        // coalescing on by default, partitioning conversions proceed
        // WITHOUT it: no coalesced_count column, no coalescing provenance.
        let geoms = fragment_chain_geoms(3);
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        for streaming in [true, false] {
            let tout = tempfile::NamedTempFile::new().unwrap();
            let report = convert_to_overviews(
                tin.path(),
                tout.path(),
                &ConvertOptions {
                    mode: Mode::Partitioning,
                    coalesce_lines: true, // the default; explicit for clarity
                    streaming,
                    ..Default::default()
                },
            )
            .unwrap();
            // Feature-once: total rows == input rows, no merging happened.
            assert_eq!(report.total_rows, geoms.len(), "streaming={streaming}");
            let reader = OverviewReader::open(tout.path()).unwrap();
            assert!(
                reader
                    .meta()
                    .generalization
                    .as_ref()
                    .unwrap()
                    .coalescing
                    .is_none(),
                "no coalescing provenance in partitioning mode"
            );
            let batch_schema = reader.read_level(0, None).unwrap().next().unwrap().unwrap();
            assert!(
                batch_schema.schema().index_of("coalesced_count").is_err(),
                "no coalesced_count column in partitioning mode"
            );
            assert!(validate_file(tout.path()).unwrap().is_valid());
        }
    }

    #[test]
    fn coalesce_renames_existing_coalesced_count_column() {
        // #288: an input already carrying a `coalesced_count` column (case-
        // insensitively) is auto-renamed when coalescing, and is an ordinary
        // passthrough property when coalescing is off.
        let geoms = fragment_chain_geoms(2);
        let n = geoms.len();
        let tin = tempfile::NamedTempFile::new().unwrap();
        {
            let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
            let cc = arrow_array::Int32Array::from(vec![7i32; n]);
            let geom_arr = build_geometry_array(&geoms);
            let geom_field = geom_arr.data_type().to_field("geometry", true);
            let fields = vec![
                Arc::new(Field::new("id", DataType::Int64, false)),
                Arc::new(Field::new("Coalesced_Count", DataType::Int32, false)),
                Arc::new(geom_field),
            ];
            let columns: Vec<Arc<dyn Array>> =
                vec![Arc::new(id), Arc::new(cc), geom_arr.to_array_ref()];
            let schema = Arc::new(Schema::new(fields));
            let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
            let gpq_options = GeoParquetWriterOptionsBuilder::default()
                .set_encoding(GeoParquetWriterEncoding::WKB)
                .set_generate_covering(true)
                .build();
            let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
            let target_schema = encoder.target_schema();
            let file = std::fs::File::create(tin.path()).unwrap();
            let mut writer = ArrowWriter::try_new(file, target_schema, None).unwrap();
            writer
                .write(&encoder.encode_record_batch(&batch).unwrap())
                .unwrap();
            writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
            writer.close().unwrap();
        }

        let tout = tempfile::NamedTempFile::new().unwrap();
        for streaming in [true, false] {
            convert_to_overviews(
                tin.path(),
                tout.path(),
                &ConvertOptions {
                    coalesce_lines: true,
                    streaming,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| {
                panic!("streaming={streaming}: `Coalesced_Count` must be auto-renamed, got {e}")
            });
            let names = output_column_names(tout.path());
            assert!(
                names.iter().any(|n| n == "Coalesced_Count_"),
                "streaming={streaming}: renamed source column present, names={names:?}"
            );
        }
        // With coalescing disabled the column is an ordinary passthrough
        // property (kept verbatim).
        convert_to_overviews(
            tin.path(),
            tout.path(),
            &ConvertOptions {
                coalesce_lines: false,
                ..Default::default()
            },
        )
        .unwrap();
        let names = output_column_names(tout.path());
        assert!(
            names.iter().any(|n| n == "Coalesced_Count"),
            "passthrough column kept verbatim, names={names:?}"
        );
    }

    #[test]
    fn coalesce_footer_provenance_recorded() {
        let geoms = fragment_chain_geoms(3);
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            coalesce_lines: true,
            coalesce_snap: 1.5,
            coalesce_junction_angle: 30.0,
            coalesce_max_level_rows: 123_456,
            ..Default::default()
        };
        convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        let reader = OverviewReader::open(tout.path()).unwrap();
        let c = reader
            .meta()
            .generalization
            .as_ref()
            .unwrap()
            .coalescing
            .clone()
            .expect("coalescing provenance recorded");
        assert!(c.enabled);
        assert_eq!(c.snap_tolerance_gsd_factor, 1.5);
        // §13.4 (v0.2.0): junction angle + memory-guard ceiling recorded so
        // the generalization is reproducible from the file alone.
        assert_eq!(c.junction_angle, Some(30.0));
        assert_eq!(c.max_level_rows, Some(123_456));
        assert_eq!(c.coalesced_count_column, "coalesced_count");

        // Opt-out (`--no-coalesce-lines`): no coalescing block recorded.
        let tout_off = tempfile::NamedTempFile::new().unwrap();
        convert_to_overviews(
            tin.path(),
            tout_off.path(),
            &ConvertOptions {
                coalesce_lines: false,
                ..Default::default()
            },
        )
        .unwrap();
        let r_off = OverviewReader::open(tout_off.path()).unwrap();
        assert!(r_off
            .meta()
            .generalization
            .as_ref()
            .unwrap()
            .coalescing
            .is_none());
    }

    #[test]
    fn coalesce_guard_skips_chaining_but_keeps_column() {
        // A max-level-rows guard smaller than the line count: no chaining
        // happens (fragments still vanish at coarse levels) but the schema
        // and provenance stay stable (coalesced_count all 1).
        let geoms = fragment_chain_geoms(6);
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 10,
            },
            no_auto_rank: true,
            coalesce_lines: true,
            coalesce_max_level_rows: 2, // < 6 lines → guard trips
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        assert_eq!(
            report.levels[0].feature_count, 1,
            "guard-skipped run behaves like non-coalesced: {:?}",
            report.levels
        );
        let reader = OverviewReader::open(tout.path()).unwrap();
        for level in 0..reader.num_levels() {
            assert!(read_coalesced_counts(&reader, level)
                .iter()
                .all(|&c| c == 1));
        }
        assert!(validate_file(tout.path()).unwrap().is_valid());
    }

    #[test]
    fn streaming_matches_in_memory_coalescing() {
        // Coalescing must be byte-equivalent between the streaming and
        // in-memory pipelines, including with tiny read batches (chain reps
        // scattered across batch boundaries).
        let geoms = fragment_chain_geoms(6);
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 10,
            },
            no_auto_rank: true,
            coalesce_lines: true,
            read_batch_size: 2,
            ..Default::default()
        };
        assert_streaming_equivalent(tin.path(), &base);

        // Explicitly compare coalesced_count per level too.
        let mem_out = tempfile::NamedTempFile::new().unwrap();
        let stream_out = tempfile::NamedTempFile::new().unwrap();
        convert_to_overviews(
            tin.path(),
            mem_out.path(),
            &ConvertOptions {
                streaming: false,
                ..base.clone()
            },
        )
        .unwrap();
        convert_to_overviews(tin.path(), stream_out.path(), &base).unwrap();
        let mr = OverviewReader::open(mem_out.path()).unwrap();
        let sr = OverviewReader::open(stream_out.path()).unwrap();
        for level in 0..mr.num_levels() {
            assert_eq!(
                read_coalesced_counts(&mr, level),
                read_coalesced_counts(&sr, level),
                "level {level} coalesced_count differs"
            );
        }
    }

    #[test]
    fn streaming_matches_in_memory_coalescing_with_class_groups() {
        // Same equivalence with the auto-detected class ranking driving the
        // compatibility groups (interner parity across batch boundaries).
        let mut geoms = vec![
            Geometry::LineString(LineString::from(vec![(0.0, 0.0), (0.1, 0.0)])),
            Geometry::LineString(LineString::from(vec![(0.1, 0.0), (0.2, 0.0)])),
            Geometry::LineString(LineString::from(vec![(0.2, 0.0), (0.2, 0.1)])),
        ];
        let mut classes = vec![Some("motorway"), Some("motorway"), Some("footway")];
        for (i, c) in ["primary", "service", "residential", "trunk"]
            .iter()
            .enumerate()
        {
            geoms.push(Geometry::LineString(LineString::from(vec![
                (3.0 + i as f64, 3.0),
                (3.1 + i as f64, 3.05),
            ])));
            classes.push(Some(*c));
        }
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_class_input(tin.path(), &geoms, &classes);

        let base = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 4,
                max_zoom: 10,
            },
            coalesce_lines: true,
            read_batch_size: 2,
            ..Default::default()
        };
        let mem_out = tempfile::NamedTempFile::new().unwrap();
        let stream_out = tempfile::NamedTempFile::new().unwrap();
        convert_to_overviews(
            tin.path(),
            mem_out.path(),
            &ConvertOptions {
                streaming: false,
                ..base.clone()
            },
        )
        .unwrap();
        convert_to_overviews(tin.path(), stream_out.path(), &base).unwrap();
        assert_eq!(
            overviews_footer_json(mem_out.path()),
            overviews_footer_json(stream_out.path())
        );
        let mr = OverviewReader::open(mem_out.path()).unwrap();
        let sr = OverviewReader::open(stream_out.path()).unwrap();
        assert_eq!(mr.num_levels(), sr.num_levels());
        for level in 0..mr.num_levels() {
            assert_eq!(
                read_coalesced_counts(&mr, level),
                read_coalesced_counts(&sr, level),
                "level {level} coalesced_count differs"
            );
        }
    }

    #[test]
    fn sort_key_column_missing_errors() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            sort_key: Some("nonexistent".to_string()),
            ..Default::default()
        };
        let err = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap_err();
        assert!(matches!(err, ConvertError::SortKeyColumnMissing { .. }));
    }

    // ========================================================================
    // Bbox row-group filtering tests (#102)
    // ========================================================================

    /// Write a multi-row-group GeoParquet file with covering column stats so
    /// row-group pruning can actually bite. Each row group contains one point
    /// at `(x, y)` with id = row-group index.
    fn write_multi_rg_input(path: &Path, coords: &[(f64, f64)], with_covering: bool) {
        use parquet::file::properties::WriterProperties;

        let geoms: Vec<Geometry<f64>> = coords
            .iter()
            .map(|&(x, y)| Geometry::Point(Point::new(x, y)))
            .collect();
        let n = geoms.len();
        let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
        let geom_arr = build_geometry_array(&geoms);
        let geom_field = geom_arr.data_type().to_field("geometry", true);
        let fields = vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(geom_field),
        ];
        let columns: Vec<Arc<dyn Array>> = vec![Arc::new(id), geom_arr.to_array_ref()];
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

        let gpq_options = GeoParquetWriterOptionsBuilder::default()
            .set_encoding(GeoParquetWriterEncoding::WKB)
            .set_generate_covering(with_covering)
            .build();
        let encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
        let target_schema = encoder.target_schema();
        // Row-group size = 1 to force n row groups.
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(1))
            .build();
        let file = std::fs::File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, target_schema, Some(props)).unwrap();
        let mut encoder = encoder;
        let encoded = encoder.encode_record_batch(&batch).unwrap();
        writer.write(&encoded).unwrap();
        writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
        writer.close().unwrap();
    }

    /// Read all ids from all levels of an overview file.
    fn read_all_ids(reader: &OverviewReader) -> Vec<i64> {
        use arrow_array::cast::AsArray;
        let mut ids = Vec::new();
        for level in 0..reader.num_levels() {
            let rdr = reader.read_level(level, None).unwrap();
            for batch in rdr {
                let batch = batch.unwrap();
                let col = batch
                    .column(batch.schema().index_of("id").unwrap())
                    .as_primitive::<arrow_array::types::Int64Type>();
                ids.extend(col.iter().flatten());
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    #[test]
    fn bbox_filter_matches_posthoc_filter() {
        // 4 row groups with points at (0,0), (10,10), (20,20), (30,30).
        // A bbox around (10,10) should keep only id=1.
        let coords = vec![(0.0, 0.0), (10.0, 10.0), (20.0, 20.0), (30.0, 30.0)];
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_multi_rg_input(tin.path(), &coords, true);

        // Full unfiltered conversion.
        let tout_full = tempfile::NamedTempFile::new().unwrap();
        let opts_full = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 6,
                max_zoom: 6,
            },
            ..Default::default()
        };
        let report_full = convert_to_overviews(tin.path(), tout_full.path(), &opts_full).unwrap();
        assert_eq!(report_full.row_groups_total, 4);
        assert_eq!(report_full.row_groups_read, 4);
        let reader_full = OverviewReader::open(tout_full.path()).unwrap();
        let ids_full = read_all_ids(&reader_full);

        // Bbox-filtered conversion: keep only (10,10).
        let tout_bbox = tempfile::NamedTempFile::new().unwrap();
        let opts_bbox = ConvertOptions {
            bbox: Some([9.0, 9.0, 11.0, 11.0]),
            ..opts_full.clone()
        };
        let report_bbox = convert_to_overviews(tin.path(), tout_bbox.path(), &opts_bbox).unwrap();
        // The bbox intersects only one row group, so pruning should fire.
        assert_eq!(report_bbox.row_groups_total, 4);
        assert_eq!(
            report_bbox.row_groups_read, 1,
            "bbox pruning did not fire: read {} row groups",
            report_bbox.row_groups_read
        );
        let reader_bbox = OverviewReader::open(tout_bbox.path()).unwrap();
        let ids_bbox = read_all_ids(&reader_bbox);
        assert_eq!(ids_bbox, vec![1], "bbox filter kept wrong ids");

        // Correctness: filtered == unfiltered then post-hoc filtered.
        let ids_posthoc: Vec<i64> = ids_full
            .into_iter()
            .filter(|&id| {
                let (x, y) = coords[id as usize];
                (9.0..=11.0).contains(&x) && (9.0..=11.0).contains(&y)
            })
            .collect();
        assert_eq!(ids_bbox, ids_posthoc);
    }

    #[test]
    fn bbox_filter_stats_free_degradation() {
        // Same layout but WITHOUT covering column (stats-free input).
        let coords = vec![(0.0, 0.0), (10.0, 10.0), (20.0, 20.0), (30.0, 30.0)];
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_multi_rg_input(tin.path(), &coords, false);

        let tout = tempfile::NamedTempFile::new().unwrap();
        let opts = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 6,
                max_zoom: 6,
            },
            bbox: Some([9.0, 9.0, 11.0, 11.0]),
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();
        // Without stats, all row groups are read (graceful degradation).
        assert_eq!(report.row_groups_total, 4);
        assert_eq!(
            report.row_groups_read, 4,
            "stats-free should read all row groups"
        );
        // Exact per-feature filter still applies: only id=1 survives.
        let reader = OverviewReader::open(tout.path()).unwrap();
        let ids = read_all_ids(&reader);
        assert_eq!(ids, vec![1], "exact filter did not apply");
    }

    #[test]
    fn bbox_filter_nothing_intersects() {
        let coords = vec![(0.0, 0.0), (10.0, 10.0)];
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_multi_rg_input(tin.path(), &coords, true);

        let tout = tempfile::NamedTempFile::new().unwrap();
        let opts = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 6,
                max_zoom: 6,
            },
            bbox: Some([100.0, 100.0, 110.0, 110.0]), // far away
            ..Default::default()
        };
        let err = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap_err();
        // No features survive → NoData error.
        assert!(
            matches!(err, ConvertError::NoData),
            "expected NoData, got {err:?}"
        );
    }

    #[test]
    fn bbox_filter_everything_intersects() {
        let coords = vec![(0.0, 0.0), (10.0, 10.0)];
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_multi_rg_input(tin.path(), &coords, true);

        // Full conversion (no bbox).
        let tout_full = tempfile::NamedTempFile::new().unwrap();
        let opts_full = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 6,
                max_zoom: 6,
            },
            ..Default::default()
        };
        let _report_full = convert_to_overviews(tin.path(), tout_full.path(), &opts_full).unwrap();
        let reader_full = OverviewReader::open(tout_full.path()).unwrap();
        let ids_full = read_all_ids(&reader_full);

        // Bbox containing everything.
        let tout_bbox = tempfile::NamedTempFile::new().unwrap();
        let opts_bbox = ConvertOptions {
            bbox: Some([-1.0, -1.0, 11.0, 11.0]),
            ..opts_full.clone()
        };
        let report_bbox = convert_to_overviews(tin.path(), tout_bbox.path(), &opts_bbox).unwrap();
        // All row groups intersect, so row_groups_read == row_groups_total.
        assert_eq!(report_bbox.row_groups_read, report_bbox.row_groups_total);
        let reader_bbox = OverviewReader::open(tout_bbox.path()).unwrap();
        let ids_bbox = read_all_ids(&reader_bbox);
        assert_eq!(ids_bbox, ids_full);
    }

    // ========================================================================
    // Remote input tests (#210)
    // ========================================================================

    #[cfg(feature = "remote")]
    mod remote_input {
        use super::*;
        use crate::input::{test_memory_source, InputSource};

        /// Byte span `[start, end)` of each row group's data pages.
        fn row_group_spans(bytes: &[u8]) -> Vec<std::ops::Range<u64>> {
            let builder =
                ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes.to_vec()))
                    .unwrap();
            builder
                .metadata()
                .row_groups()
                .iter()
                .map(|rg| {
                    let mut start = u64::MAX;
                    let mut end = 0u64;
                    for col in rg.columns() {
                        let (s, len) = col.byte_range();
                        start = start.min(s);
                        end = end.max(s + len);
                    }
                    start..end
                })
                .collect()
        }

        /// THE #210 headline property: a `--bbox` extract from a remote file
        /// must fetch byte ranges ONLY from the bbox-selected row groups (plus
        /// the footer) — pruned row groups are never downloaded at all.
        fn assert_bbox_extract_fetches_only_selected(streaming: bool) {
            // 4 single-point row groups at (0,0), (10,10), (20,20), (30,30)
            // with covering stats; a bbox around (10,10) selects only rg 1.
            let coords = vec![(0.0, 0.0), (10.0, 10.0), (20.0, 20.0), (30.0, 30.0)];
            let tin = tempfile::NamedTempFile::new().unwrap();
            write_multi_rg_input(tin.path(), &coords, true);
            let bytes = std::fs::read(tin.path()).unwrap();
            let spans = row_group_spans(&bytes);
            assert_eq!(spans.len(), 4);

            let source = test_memory_source(bytes, "multi.parquet");
            let tout = tempfile::NamedTempFile::new().unwrap();
            let opts = ConvertOptions {
                mode: Mode::Duplicating,
                levels: LevelPlan::ZoomRange {
                    min_zoom: 6,
                    max_zoom: 6,
                },
                bbox: Some([9.0, 9.0, 11.0, 11.0]),
                streaming,
                ..Default::default()
            };
            let report = convert_to_overviews_source(&source, tout.path(), &opts).unwrap();

            assert_eq!(report.row_groups_total, 4);
            assert_eq!(report.row_groups_read, 1, "bbox pruning must fire");
            let ids = read_all_ids(&OverviewReader::open(tout.path()).unwrap());
            assert_eq!(ids, vec![1], "only the (10,10) feature survives");

            // No fetched range may touch a pruned row group's data pages.
            let fetched = source.fetched_ranges().unwrap();
            assert!(!fetched.is_empty());
            for (i, span) in spans.iter().enumerate() {
                if i == 1 {
                    continue;
                }
                for r in &fetched {
                    assert!(
                        r.end <= span.start || r.start >= span.end,
                        "fetched range {r:?} overlaps PRUNED row group {i} ({span:?})"
                    );
                }
            }
            // ... while the selected row group's data pages WERE fetched.
            assert!(
                fetched
                    .iter()
                    .any(|r| r.start >= spans[1].start && r.end <= spans[1].end),
                "selected row group 1 ({:?}) never fetched: {fetched:?}",
                spans[1]
            );

            // And the report carries the savings.
            let stats = report.remote_fetch.expect("remote stats in report");
            assert!(stats.requests as usize >= fetched.len());
            assert!(
                stats.bytes_fetched < stats.object_size,
                "bbox extract must move fewer bytes than the object: {stats:?}"
            );
        }

        #[test]
        fn bbox_extract_streaming_fetches_only_selected_row_groups() {
            assert_bbox_extract_fetches_only_selected(true);
        }

        #[test]
        fn bbox_extract_in_memory_fetches_only_selected_row_groups() {
            assert_bbox_extract_fetches_only_selected(false);
        }

        /// A full (no-bbox) remote conversion must produce the same result as
        /// the same conversion over the local file.
        #[test]
        fn remote_convert_matches_local_convert() {
            let geoms = synthetic_geometries();
            let tin = tempfile::NamedTempFile::new().unwrap();
            write_input(tin.path(), &geoms, false, None);
            let opts = ConvertOptions::default();

            let tout_local = tempfile::NamedTempFile::new().unwrap();
            let report_local = convert_to_overviews(tin.path(), tout_local.path(), &opts).unwrap();
            assert!(report_local.remote_fetch.is_none(), "local input: no stats");

            let source = test_memory_source(std::fs::read(tin.path()).unwrap(), "in.parquet");
            let tout_remote = tempfile::NamedTempFile::new().unwrap();
            let report_remote =
                convert_to_overviews_source(&source, tout_remote.path(), &opts).unwrap();

            assert_eq!(report_remote.input_features, report_local.input_features);
            assert_eq!(report_remote.total_rows, report_local.total_rows);
            assert_eq!(
                read_all_ids(&OverviewReader::open(tout_remote.path()).unwrap()),
                read_all_ids(&OverviewReader::open(tout_local.path()).unwrap()),
            );
            assert!(report_remote.remote_fetch.is_some());
        }

        /// #286 + #287: a full remote convert must COALESCE its data-page
        /// fetches to ~one range request per selected row group. A row
        /// group's column chunks are a contiguous byte span, so staging that
        /// span up front (in parallel) turns the whole conversion's reads
        /// into one request per row group — instead of a separate serial
        /// request per column chunk per pass (#287), and in particular
        /// instead of pass 2 re-fetching, cold, the property columns that
        /// pass 1's geometry-only projection skipped (#286).
        #[test]
        fn remote_convert_coalesces_fetches_to_one_request_per_row_group() {
            let geoms = synthetic_geometries();
            let n = geoms.len();
            // Several row groups, each carrying property columns (id, name,
            // rank) that pass 1's geometry+ranking projection does not fully
            // read — the #286 pass-2 re-fetch shape.
            let tin = tempfile::NamedTempFile::new().unwrap();
            write_input_partition(tin.path(), &geoms, 0..n, Some(2));
            let bytes = std::fs::read(tin.path()).unwrap();
            let rg = row_group_spans(&bytes).len();
            assert!(rg >= 3, "test needs several row groups, got {rg}");

            let opts = ConvertOptions::default();

            // Reference: the local convert of the identical bytes.
            let tout_local = tempfile::NamedTempFile::new().unwrap();
            convert_to_overviews(tin.path(), tout_local.path(), &opts).unwrap();
            let local_ids = read_all_ids(&OverviewReader::open(tout_local.path()).unwrap());

            let source = test_memory_source(bytes, "staged.parquet");
            let tout = tempfile::NamedTempFile::new().unwrap();
            let report = convert_to_overviews_source(&source, tout.path(), &opts).unwrap();

            // Staging must not change the output.
            assert_eq!(
                read_all_ids(&OverviewReader::open(tout.path()).unwrap()),
                local_ids,
                "staged remote convert must match the local convert row-for-row"
            );

            let stats = report.remote_fetch.expect("remote stats in report");
            // THE property: one coalesced request per selected row group, plus
            // a small fixed footer/metadata overhead — NOT ~one request per
            // column chunk per pass.
            const FOOTER_SLACK: u64 = 4;
            assert!(
                stats.requests <= rg as u64 + FOOTER_SLACK,
                "expected ~1 request per row group (<= {} for {rg} row groups); \
                 got {} — fetches not coalesced (#287) or properties re-fetched (#286)",
                rg as u64 + FOOTER_SLACK,
                stats.requests,
            );
            // Coalescing must not over-fetch: still ≈1× the object (#219).
            assert!(
                stats.bytes_fetched <= stats.object_size + stats.object_size / 2,
                "staging must stay ≈1× the object: {} of {} bytes",
                stats.bytes_fetched,
                stats.object_size,
            );
        }

        // --- multi-partition remote input (v0.7 PR-B) ------------------------

        /// Partition bytes for `geoms[range]`, via the shared local writer.
        fn partition_bytes(
            geoms: &[Geometry<f64>],
            range: std::ops::Range<usize>,
            row_group_rows: Option<usize>,
        ) -> Vec<u8> {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            write_input_partition(tmp.path(), geoms, range, row_group_rows);
            std::fs::read(tmp.path()).unwrap()
        }

        /// Convert a remote ConvertSource and export to PMTiles; returns
        /// the archive bytes (PMTiles export is byte-deterministic).
        fn convert_sources_and_export(
            source: &crate::input_set::ConvertSource,
            workdir: &Path,
            tag: &str,
            opts: &ConvertOptions,
        ) -> Vec<u8> {
            use crate::overview::export::{export_pmtiles, ExportOptions};
            let overview = workdir.join(format!("{tag}-overview.parquet"));
            let pmtiles = workdir.join(format!("{tag}.pmtiles"));
            convert_to_overviews_sources(source, &overview, opts).unwrap();
            export_pmtiles(&overview, &pmtiles, &ExportOptions::default()).unwrap();
            std::fs::read(&pmtiles).unwrap()
        }

        /// #219's ≈1× guarantee must hold PER PART across the streaming
        /// pipeline's three passes (assign, coarse levels, finest last):
        /// each part's fetched ranges sum to at most ~1.5× its object size
        /// (footer overhead), and no byte range crosses the network twice.
        #[test]
        fn multi_part_three_pass_moves_each_part_once() {
            let geoms = synthetic_geometries();
            let n = geoms.len();
            let (source, parts) = crate::input::test_memory_multi_source(vec![
                ("p0.parquet", partition_bytes(&geoms, 0..5, None)),
                ("p1.parquet", partition_bytes(&geoms, 5..9, None)),
                ("p2.parquet", partition_bytes(&geoms, 9..n, None)),
            ]);
            assert_eq!(parts.len(), 3);

            let tout = tempfile::NamedTempFile::new().unwrap();
            let report =
                convert_to_overviews_sources(&source, tout.path(), &multi_test_options()).unwrap();
            assert_eq!(report.input_features, n);

            let summed = report.remote_fetch.expect("multi remote reports stats");
            let mut object_total = 0;
            for part in &parts {
                let stats = part.fetch_stats().expect("remote part has stats");
                object_total += stats.object_size;
                assert!(
                    stats.bytes_fetched <= stats.object_size + stats.object_size / 2,
                    "part {} moved {} bytes for a {}-byte object (>1.5x, #219 \
                     must hold per part)",
                    part.display_name(),
                    stats.bytes_fetched,
                    stats.object_size,
                );
                // No byte range crosses the network twice for any part.
                let mut seen = std::collections::HashSet::new();
                for r in part.fetched_ranges().expect("remote part logs ranges") {
                    assert!(
                        seen.insert((r.start, r.end)),
                        "part {}: range {r:?} fetched more than once (#219)",
                        part.display_name(),
                    );
                }
            }
            assert_eq!(
                summed.object_size, object_total,
                "ConvertReport.remote_fetch.object_size sums the parts"
            );
        }

        /// #286 + #287 across the multi-partition path (the demo scenario):
        /// each remote part must coalesce ITS OWN selected row groups to ~one
        /// range request per row group, so a prefix of N partitions pays one
        /// TTFB per row group — not ~one per column chunk per pass.
        #[test]
        fn multi_part_remote_coalesces_per_part_row_groups() {
            let geoms = synthetic_geometries();
            let n = geoms.len();
            // Each partition carries several row groups (2 rows each) with
            // property columns (id, name, rank) that pass 1 skips — the #286
            // shape, per part.
            let p0 = partition_bytes(&geoms, 0..5, Some(2));
            let p1 = partition_bytes(&geoms, 5..9, Some(2));
            let p2 = partition_bytes(&geoms, 9..n, Some(2));
            let rg = [&p0, &p1, &p2].map(|b| row_group_spans(b).len());
            assert!(
                rg.iter().all(|&r| r >= 2),
                "each part needs several row groups: {rg:?}"
            );

            let (source, parts) = crate::input::test_memory_multi_source(vec![
                ("p0.parquet", p0),
                ("p1.parquet", p1),
                ("p2.parquet", p2),
            ]);
            let tout = tempfile::NamedTempFile::new().unwrap();
            convert_to_overviews_sources(&source, tout.path(), &multi_test_options()).unwrap();

            const FOOTER_SLACK: u64 = 4;
            for (i, part) in parts.iter().enumerate() {
                let stats = part.fetch_stats().expect("remote part has stats");
                assert!(
                    stats.requests <= rg[i] as u64 + FOOTER_SLACK,
                    "part {i}: expected ~1 request per row group (<= {} for {} \
                     row groups); got {} — not coalesced (#287) or properties \
                     re-fetched (#286)",
                    rg[i] as u64 + FOOTER_SLACK,
                    rg[i],
                    stats.requests,
                );
            }
        }

        /// bbox pruning an ENTIRE part: its row groups are pruned at the
        /// footer level, so the network never touches that part's data
        /// pages — only its footer (fetched once at set construction).
        #[test]
        fn multi_part_bbox_prunes_part_to_footer_only() {
            let geoms = synthetic_geometries();
            let n = geoms.len();
            // p1 holds ONLY the lines (x 40..96); the bbox below excludes them.
            let p1_bytes = partition_bytes(&geoms, 6..10, None);
            let p1_spans = row_group_spans(&p1_bytes);
            let (source, parts) = crate::input::test_memory_multi_source(vec![
                ("p0.parquet", partition_bytes(&geoms, 0..6, None)),
                ("p1.parquet", p1_bytes),
                ("p2.parquet", partition_bytes(&geoms, 10..n, None)),
            ]);

            let opts = ConvertOptions {
                bbox: Some([-100.0, -70.0, 30.0, 20.0]),
                ..multi_test_options()
            };
            let tout = tempfile::NamedTempFile::new().unwrap();
            let report = convert_to_overviews_sources(&source, tout.path(), &opts).unwrap();
            assert!(
                report.row_groups_read < report.row_groups_total,
                "bbox must prune the lines part: {}/{}",
                report.row_groups_read,
                report.row_groups_total
            );

            let pruned = &parts[1];
            let fetched = pruned.fetched_ranges().expect("remote part logs ranges");
            assert!(
                !fetched.is_empty(),
                "the footer itself is fetched at set construction"
            );
            for r in &fetched {
                for span in &p1_spans {
                    assert!(
                        r.end <= span.start || r.start >= span.end,
                        "pruned part fetched data-page range {r:?} \
                         (row-group span {span:?}) — must be footer-only"
                    );
                }
            }
        }

        /// The PR-A anchor, over the wire: the same rows as one remote
        /// object and as three remote partitions under one prefix must
        /// produce byte-identical PMTiles.
        #[test]
        fn multi_part_remote_output_matches_single_remote() {
            let geoms = synthetic_geometries();
            let n = geoms.len();
            let dir = tempfile::tempdir().unwrap();
            let opts = multi_test_options();

            let (single, _) = crate::input::test_memory_multi_source(vec![(
                "single.parquet",
                partition_bytes(&geoms, 0..n, None),
            )]);
            let (multi, parts) = crate::input::test_memory_multi_source(vec![
                ("part-000.parquet", partition_bytes(&geoms, 0..5, None)),
                ("part-001.parquet", partition_bytes(&geoms, 5..9, None)),
                ("part-002.parquet", partition_bytes(&geoms, 9..n, None)),
            ]);
            assert_eq!(parts.len(), 3);

            let pm_single = convert_sources_and_export(&single, dir.path(), "single", &opts);
            let pm_multi = convert_sources_and_export(&multi, dir.path(), "multi", &opts);
            assert!(
                pm_single == pm_multi,
                "remote multi-partition output must be byte-identical to the \
                 single remote object ({} vs {} bytes)",
                pm_single.len(),
                pm_multi.len()
            );
        }

        /// A throwaway localhost HTTP/1.1 server that serves one byte blob with
        /// Range support — the hermetic, no-network stand-in for object storage.
        /// Answers `HEAD` (size) and ranged/full `GET` exactly as object_store's
        /// HTTP store expects. Returns the base URL (`http://127.0.0.1:PORT`);
        /// the accept loop runs on a detached thread until the test process
        /// exits. Binding happens before return, so the listen backlog absorbs
        /// any connect that races the accept loop.
        fn serve_bytes_over_http(body: Vec<u8>) -> String {
            use std::io::{BufRead, BufReader, Write};
            use std::net::TcpListener;
            use std::sync::Arc;

            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let body = Arc::new(body);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let body = Arc::clone(&body);
                    std::thread::spawn(move || {
                        let peer = stream.try_clone().unwrap();
                        let mut reader = BufReader::new(peer);
                        let size = body.len() as u64;
                        // One connection may carry several keep-alive requests.
                        loop {
                            let mut request_line = String::new();
                            match reader.read_line(&mut request_line) {
                                Ok(0) | Err(_) => break, // client closed
                                Ok(_) => {}
                            }
                            let mut parts = request_line.split_whitespace();
                            let method = parts.next().unwrap_or("").to_string();
                            if method.is_empty() {
                                break;
                            }
                            // Consume headers; only Range matters.
                            let mut range: Option<(u64, u64)> = None;
                            loop {
                                let mut header = String::new();
                                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                                    break;
                                }
                                if header == "\r\n" || header == "\n" {
                                    break;
                                }
                                let lower = header.to_ascii_lowercase();
                                let Some(spec) = lower
                                    .strip_prefix("range:")
                                    .and_then(|v| v.trim().strip_prefix("bytes="))
                                else {
                                    continue;
                                };
                                let spec = spec.split(',').next().unwrap_or("").trim();
                                let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
                                let (start, end) = if a.is_empty() {
                                    // Suffix range: the last N bytes.
                                    let n: u64 = b.trim().parse().unwrap_or(0);
                                    (size.saturating_sub(n), size.saturating_sub(1))
                                } else {
                                    let start = a.trim().parse().unwrap_or(0);
                                    let end = if b.trim().is_empty() {
                                        size.saturating_sub(1)
                                    } else {
                                        b.trim().parse().unwrap_or(size - 1)
                                    };
                                    (start, end.min(size.saturating_sub(1)))
                                };
                                range = Some((start, end));
                            }
                            let response: Vec<u8> = match (method.as_str(), range) {
                                ("HEAD", _) => format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                                     Accept-Ranges: bytes\r\nContent-Length: {size}\r\n\r\n"
                                )
                                .into_bytes(),
                                ("GET", Some((start, end))) => {
                                    let slice = &body[start as usize..=end as usize];
                                    let mut resp = format!(
                                        "HTTP/1.1 206 Partial Content\r\n\
                                         Content-Type: application/octet-stream\r\n\
                                         Accept-Ranges: bytes\r\n\
                                         Content-Range: bytes {start}-{end}/{size}\r\n\
                                         Content-Length: {}\r\n\r\n",
                                        slice.len()
                                    )
                                    .into_bytes();
                                    resp.extend_from_slice(slice);
                                    resp
                                }
                                ("GET", None) => {
                                    let mut resp = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                                         Accept-Ranges: bytes\r\nContent-Length: {size}\r\n\r\n"
                                    )
                                    .into_bytes();
                                    resp.extend_from_slice(&body);
                                    resp
                                }
                                _ => {
                                    b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n"
                                        .to_vec()
                                }
                            };
                            if stream.write_all(&response).is_err() {
                                break;
                            }
                            let _ = stream.flush();
                        }
                    });
                }
            });
            format!("http://{addr}")
        }

        /// #262 end-to-end guard: a full conversion from an `http://` input must
        /// succeed and match the local conversion. Without `allow_http` on the
        /// store the `http://` input builder-errors immediately, so this test
        /// fails the moment that regresses — no network, no credentials.
        #[test]
        fn remote_http_convert_matches_local() {
            let geoms = synthetic_geometries();
            let tin = tempfile::NamedTempFile::new().unwrap();
            write_input(tin.path(), &geoms, false, None);
            let opts = ConvertOptions::default();

            let tout_local = tempfile::NamedTempFile::new().unwrap();
            let report_local = convert_to_overviews(tin.path(), tout_local.path(), &opts).unwrap();

            let base = serve_bytes_over_http(std::fs::read(tin.path()).unwrap());
            let url = format!("{base}/in.parquet");
            let source = InputSource::from_str_input(&url).unwrap();
            assert!(source.is_remote(), "http:// input must be remote");

            let tout_remote = tempfile::NamedTempFile::new().unwrap();
            let report_remote =
                convert_to_overviews_source(&source, tout_remote.path(), &opts).unwrap();

            assert_eq!(report_remote.input_features, report_local.input_features);
            assert_eq!(
                read_all_ids(&OverviewReader::open(tout_remote.path()).unwrap()),
                read_all_ids(&OverviewReader::open(tout_local.path()).unwrap()),
            );
            let stats = report_remote
                .remote_fetch
                .expect("http input reports fetch stats");
            assert!(
                stats.bytes_fetched > 0 && stats.requests > 0,
                "http conversion moved bytes: {stats:?}"
            );
        }

        /// A URL with an unsupported scheme surfaces a helpful error through
        /// the public `convert_to_overviews` path.
        #[test]
        fn unsupported_scheme_errors_through_convert() {
            let tout = tempfile::NamedTempFile::new().unwrap();
            let err = convert_to_overviews(
                Path::new("ftp://example.com/x.parquet"),
                tout.path(),
                &ConvertOptions::default(),
            )
            .unwrap_err();
            assert!(matches!(err, ConvertError::Input(_)), "got: {err}");
            assert!(err.to_string().contains("s3://"), "helpful message: {err}");
        }

        /// Network integration test against the bench bucket (issue #210):
        /// a full remote city extract must move only a fraction of the
        /// object's bytes. Skips (passing trivially, loudly) when
        /// credentials or network are unavailable — CI has neither.
        ///
        /// Locally: `AWS_PROFILE=<profile> AWS_REGION=us-east-2 cargo test
        /// --features remote remote_s3_city_extract -- --nocapture`
        #[test]
        fn remote_s3_city_extract_integration() {
            // gpio-optimized (Hilbert-sorted, bbox covering, 20k-row row
            // groups) copy of the NYC corpus input; row-group granularity
            // bounds the minimum fetch, so finer groups mean bigger savings.
            const URL: &str = "s3://tylertoo-bench/corpus/points-nyc-medium.rg20k.parquet";
            let source = match InputSource::from_str_input(URL) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "SKIP remote_s3_city_extract_integration (no credentials/network): {e}"
                    );
                    return;
                }
            };
            let tout = tempfile::NamedTempFile::new().unwrap();
            // A ~1 km neighborhood window inside the NYC dataset (the input
            // is Hilbert-sorted, so a small window prunes most row groups).
            // Default (streaming) pipeline: its per-pass/per-level re-reads
            // must be absorbed by the column-chunk cache, so the byte
            // assertion below also guards the multi-pass refetch behavior.
            let opts = ConvertOptions {
                bbox: Some([-73.99, 40.72, -73.98, 40.73]),
                ..Default::default()
            };
            let report = match convert_to_overviews_source(&source, tout.path(), &opts) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("SKIP remote_s3_city_extract_integration (network flake?): {e}");
                    return;
                }
            };
            assert!(report.input_features > 0, "bbox should select features");
            assert!(
                report.row_groups_read < report.row_groups_total,
                "row-group pruning should fire on the Hilbert-sorted input \
                 ({}/{} read)",
                report.row_groups_read,
                report.row_groups_total
            );
            let stats = report.remote_fetch.expect("remote stats");
            assert!(
                stats.bytes_fetched * 4 < stats.object_size,
                "city extract should move <25% of the remote object even \
                 across streaming passes: {stats:?}"
            );
            eprintln!(
                "remote_s3_city_extract_integration: {} requests, {} of {} bytes ({:.2}%)",
                stats.requests,
                stats.bytes_fetched,
                stats.object_size,
                100.0 * stats.bytes_fetched as f64 / stats.object_size as f64
            );
        }
    }

    // --- empty coarse levels: auto-clamp (#211) ------------------------------

    /// Tiny (~10 m) squares spread far apart: they fail the polygon
    /// visibility gate (2 × GSD) at every coarse zoom, so only the canonical
    /// level keeps them.
    fn tiny_polygons(n: usize) -> Vec<Geometry<f64>> {
        (0..n)
            .map(|i| {
                let cx = -150.0 + (i % 10) as f64 * 3.0;
                let cy = -60.0 + (i / 10) as f64 * 1.5;
                let h = 5e-5;
                let ext = LineString::from(vec![
                    (cx - h, cy - h),
                    (cx + h, cy - h),
                    (cx + h, cy + h),
                    (cx - h, cy + h),
                    (cx - h, cy - h),
                ]);
                Geometry::Polygon(Polygon::new(ext, vec![]))
            })
            .collect()
    }

    /// Convert `tiny_polygons` over z0..z4 and assert the pyramid is clamped
    /// to the canonical level, with the skipped planned levels recorded.
    fn assert_clamped_pyramid(mode: Mode, streaming: bool) {
        let geoms = tiny_polygons(20);
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            mode,
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 4,
            },
            streaming,
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();

        // Only the canonical level survives; the pyramid clamps to it.
        assert_eq!(report.levels.len(), 1, "expected a single written level");
        assert_eq!(report.levels[0].level, 0);
        assert_eq!(report.levels[0].zoom, Some(4));
        assert_eq!(report.levels[0].feature_count, geoms.len());
        assert_eq!(report.total_rows, geoms.len());

        // The skipped planned levels are recorded coarse→fine with their
        // planned zoom and a positive GSD.
        let skipped: Vec<(usize, Option<u8>)> = report
            .skipped_empty_levels
            .iter()
            .map(|s| (s.planned_level, s.zoom))
            .collect();
        assert_eq!(
            skipped,
            vec![(0, Some(0)), (1, Some(1)), (2, Some(2)), (3, Some(3))]
        );
        assert!(report.skipped_empty_levels.iter().all(|s| s.gsd > 0.0));

        // The clamped file is valid, readable, and exportable: the PMTiles
        // header starts at the clamped (canonical) zoom.
        let vr = validate_file(tout.path()).unwrap();
        assert!(
            vr.is_valid(),
            "failures: {:?}",
            vr.failures().collect::<Vec<_>>()
        );
        let reader = OverviewReader::open(tout.path()).unwrap();
        assert_eq!(reader.num_levels(), 1);
        let tpm = tempfile::NamedTempFile::new().unwrap();
        let export = crate::overview::export::export_pmtiles(
            tout.path(),
            tpm.path(),
            &crate::overview::export::ExportOptions::default(),
        )
        .unwrap();
        assert_eq!(export.min_zoom, 4);
        assert_eq!(export.max_zoom, 4);
    }

    #[test]
    fn empty_coarse_levels_clamped_duplicating_memory() {
        assert_clamped_pyramid(Mode::Duplicating, false);
    }

    #[test]
    fn empty_coarse_levels_clamped_duplicating_streaming() {
        assert_clamped_pyramid(Mode::Duplicating, true);
    }

    #[test]
    fn empty_coarse_levels_clamped_partitioning_memory() {
        assert_clamped_pyramid(Mode::Partitioning, false);
    }

    #[test]
    fn empty_coarse_levels_clamped_partitioning_streaming() {
        assert_clamped_pyramid(Mode::Partitioning, true);
    }

    /// The degenerate extreme — NO level has any rows (empty input) — must
    /// remain a hard, actionable error in both pipelines.
    #[test]
    fn all_levels_empty_is_hard_error() {
        for streaming in [false, true] {
            let tin = tempfile::NamedTempFile::new().unwrap();
            let tout = tempfile::NamedTempFile::new().unwrap();
            write_input(tin.path(), &[], false, None);
            let opts = ConvertOptions {
                mode: Mode::Duplicating,
                levels: LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 3,
                },
                streaming,
                ..Default::default()
            };
            let err = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap_err();
            assert!(
                matches!(err, ConvertError::NoData),
                "streaming={streaming}: expected NoData, got {err:?}"
            );
        }
    }

    /// #211 regression: a feature can pass the assign visibility gate (huge
    /// bbox) yet be dropped by simplification at write time (degenerate
    /// sliver). The streaming pass 2 must skip the now-empty level instead of
    /// failing the whole conversion with `EmptyLevel`.
    #[test]
    fn write_time_empty_level_skipped_streaming() {
        let mut geoms = tiny_polygons(8);
        // Pathological feature: a MultiPolygon of two ~10 m squares 160°
        // apart. Its whole-feature bbox diagonal passes the assign visibility
        // gate at every zoom, but each PART is far below the per-level
        // simplify tolerance, so `simplify_for_level` drops the feature at
        // every non-canonical level.
        let square = |cx: f64, cy: f64| {
            let h = 5e-5;
            Polygon::new(
                LineString::from(vec![
                    (cx - h, cy - h),
                    (cx + h, cy - h),
                    (cx + h, cy + h),
                    (cx - h, cy + h),
                    (cx - h, cy - h),
                ]),
                vec![],
            )
        };
        geoms.push(Geometry::MultiPolygon(geo::MultiPolygon::new(vec![
            square(-80.0, 10.0),
            square(80.0, 30.0),
        ])));

        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, false, None);

        let opts = ConvertOptions {
            mode: Mode::Duplicating,
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 3,
            },
            streaming: true,
            ..Default::default()
        };
        let report = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap();

        // Levels z0..z2 each contained only the sliver, which simplification
        // dropped: they are skipped at write time and the pyramid clamps to
        // the canonical level.
        assert_eq!(report.levels.len(), 1);
        assert_eq!(report.levels[0].level, 0);
        assert_eq!(report.levels[0].zoom, Some(3));
        assert_eq!(report.levels[0].feature_count, geoms.len());
        assert_eq!(
            report
                .skipped_empty_levels
                .iter()
                .map(|s| s.planned_level)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        let vr = validate_file(tout.path()).unwrap();
        assert!(
            vr.is_valid(),
            "failures: {:?}",
            vr.failures().collect::<Vec<_>>()
        );
        let reader = OverviewReader::open(tout.path()).unwrap();
        assert_eq!(reader.num_levels(), 1);
    }
}
