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
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arrow_array::{Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema};
use arrow_select::concat::concat_batches;
use arrow_select::take::take;
use geo::{BoundingRect, Geometry};
use geoarrow::array::{from_arrow_array, GeometryBuilder};
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Serialize;

use crate::batch_processor::extract_geometries_from_array;

use super::assign::{
    apply_density_budget, assign_levels, AssignConfig, AssignFeature, DensityBudgetConfig,
    FeatureKind, SUPERCELL_GSD_FACTOR,
};
use super::cluster::{build_cluster_tables, AccumulateSpec, ClusterEntry, POINT_COUNT_COLUMN};
use super::level::{
    gsd_with_base, AccumulatedColumn, ClusteringProvenance, Crs, DensityProvenance, Generalization,
    GeneralizationLevel, Mode, RankingProvenance, GSD_TILE_BASE,
};
use super::simplify::{simplify_for_level, Simplified, SimplifyOptions};
use super::writer::{LevelSpec, OverviewWriter, OverviewWriterOptions, WriterError, LEVEL_COLUMN};

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

impl LevelPlan {
    /// Resolve to the coarse→fine list of `(gsd_meters, zoom?)` level specs.
    ///
    /// `gsd_base` is the GSD tile-band base (spec §5.2 / Q6); it scales the
    /// per-zoom GSDs of a [`ZoomRange`](LevelPlan::ZoomRange) plan and has no
    /// effect on an explicit [`Gsds`](LevelPlan::Gsds) plan (those GSDs are
    /// already in meters).
    pub(super) fn resolve(&self, gsd_base: f64) -> Result<Vec<(f64, Option<u8>)>, ConvertError> {
        match self {
            LevelPlan::ZoomRange { min_zoom, max_zoom } => {
                if min_zoom > max_zoom {
                    return Err(ConvertError::InvalidLevels(format!(
                        "min_zoom {min_zoom} must be <= max_zoom {max_zoom}"
                    )));
                }
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
}

/// Default rows per read batch for the streaming pipeline (H3).
pub const DEFAULT_READ_BATCH_SIZE: usize = 8192;

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
            full_column_stats: false,
            streaming: true,
            read_batch_size: DEFAULT_READ_BATCH_SIZE,
            cluster: false,
            accumulate: Vec::new(),
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

/// Result of a conversion, `Serialize` for JSON output (benchmark tasks).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConvertReport {
    /// Level materialization mode used.
    pub mode: Mode,
    /// Per-level statistics, coarse→fine.
    pub levels: Vec<LevelReport>,
    /// Number of source features read from the input.
    pub input_features: usize,
    /// Total rows written across all levels.
    pub total_rows: usize,
    /// Total vertices written across all levels.
    pub total_vertices: usize,
    /// Total compressed output size (bytes) across all levels.
    pub total_compressed_bytes: i64,
    /// Wall-clock conversion duration in seconds.
    pub duration_secs: f64,
}

/// Errors from [`convert_to_overviews`].
#[derive(Debug, thiserror::Error)]
pub enum ConvertError {
    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
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
    /// The input already contains a `level` column (§4.1).
    #[error("input already contains a '{LEVEL_COLUMN}' column; not an overview source")]
    LevelColumnPresent,
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
    /// The input has no features, or every feature was dropped from every level.
    #[error("no output rows produced (empty input or all features dropped)")]
    NoData,
}

/// Convert a GeoParquet file into a multi-resolution overview GeoParquet file.
///
/// See the module documentation for the pipeline. Returns a [`ConvertReport`]
/// describing the levels written.
pub fn convert_to_overviews(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &ConvertOptions,
) -> Result<ConvertReport, ConvertError> {
    // Clustering option sanity (Q4), shared by both pipelines: partitioning
    // mode cannot represent per-level counts (see the error's rationale), and
    // aggregation is meaningless without clustering.
    if options.cluster && matches!(options.mode, Mode::Partitioning) {
        return Err(ConvertError::ClusterPartitioningUnsupported);
    }
    if !options.accumulate.is_empty() && !options.cluster {
        return Err(ConvertError::AccumulateWithoutCluster);
    }

    // Two-pass bounded-memory pipeline (H3, default). The in-memory path below
    // is kept as the reference implementation (`streaming: false`).
    if options.streaming {
        return super::stream::convert_streaming(
            input_path.as_ref(),
            output_path.as_ref(),
            options,
        );
    }

    let start = Instant::now();
    let input_path = input_path.as_ref();
    let output_path = output_path.as_ref();

    // A numeric sort key and a categorical class ranking are mutually
    // exclusive (Q1): they would both drive `AssignFeature::sort_key`.
    if options.sort_key.is_some() && options.class_ranking.is_some() {
        return Err(ConvertError::RankingConflict);
    }

    // --- CRS detection + rejection (spec Q3). --------------------------------
    let crs = detect_crs(input_path)?;

    // --- Read the whole input, preserving the full property schema. ----------
    let file = std::fs::File::open(input_path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let input_schema = builder.schema().clone();

    // Reject an input that already carries a `level` column (§4.1).
    // Case-insensitive: DuckDB resolves identifiers case-insensitively,
    // so a source `LEVEL` column would silently shadow ours in
    // `WHERE level = k` (V1 finding F2).
    if input_schema
        .fields()
        .iter()
        .any(|f| f.name().eq_ignore_ascii_case(LEVEL_COLUMN))
    {
        return Err(ConvertError::LevelColumnPresent);
    }

    let geom_idx = find_geometry_column(&input_schema).ok_or(ConvertError::NoGeometryColumn)?;
    let geom_field = input_schema.field(geom_idx).clone();

    // Clustering schema checks + accumulate column resolution (Q4).
    let acc_cols = validate_cluster_schema(&input_schema, options)?;

    let reader = builder.build()?;
    let mut batches: Vec<RecordBatch> = Vec::new();
    for batch in reader {
        batches.push(batch?);
    }
    let full = concat_batches(&input_schema, &batches)?;
    let num_features = full.num_rows();

    // Decode geometries once (in-memory v1).
    let geom_array: Arc<dyn GeoArrowArray> =
        from_arrow_array(full.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
    let mut geometries: Vec<Geometry<f64>> = Vec::with_capacity(num_features);
    extract_geometries_from_array(geom_array.as_ref(), &mut geometries)?;

    // Resolve the cell-winner ranking (Q1): explicit sort key / explicit class
    // ranking / auto-detected well-known schema / size fallback. Returns the
    // per-feature sort keys and the provenance recorded in the footer (§3.5).
    let (sort_keys, ranking_provenance) =
        resolve_ranking(&input_schema, &full, &geometries, options)?;

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
        Some(build_cluster_tables(
            &features,
            &min_levels,
            &level_gsds,
            &options.assign,
            crs,
            &acc_values,
            &ops,
        ))
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
    }
    let mut emitted: Vec<EmittedLevel> = Vec::new();

    for (level, &(gsd_m, zoom)) in level_specs.iter().enumerate() {
        let member_indices: Vec<usize> = match options.mode {
            Mode::Duplicating => assignment.duplicating_at_level(level as u8),
            Mode::Partitioning => assignment.partitioning_at_level(level as u8),
        };

        // Verbatim path: partitioning at every level (§2.3), and duplicating at
        // the canonical (finest) level (§2.4). Otherwise simplify per feature.
        let verbatim = matches!(options.mode, Mode::Partitioning) || level == finest;

        let mut indices = Vec::with_capacity(member_indices.len());
        let mut geoms = Vec::with_capacity(member_indices.len());
        let mut vertex_count = 0usize;

        if verbatim {
            for i in member_indices {
                let g = &geometries[i];
                vertex_count += count_vertices(g);
                indices.push(i);
                geoms.push(g.clone());
            }
        } else {
            for i in member_indices {
                match simplify_for_level(&geometries[i], gsd_m, crs, &options.simplify) {
                    Simplified::Keep(g) => {
                        vertex_count += count_vertices(&g);
                        indices.push(i);
                        geoms.push(g);
                    }
                    Simplified::Dropped => {}
                }
            }
        }

        // Empty levels are not allowed (§7.3): omit and renumber.
        if indices.is_empty() {
            continue;
        }
        emitted.push(EmittedLevel {
            orig: level,
            gsd: gsd_m,
            zoom,
            indices,
            geoms,
            vertex_count,
        });
    }

    if emitted.is_empty() {
        return Err(ConvertError::NoData);
    }

    // --- Build the output writer schema (source schema + geoarrow geometry). -
    let geom_name = geom_field.name().clone();
    // A fresh mixed-Geometry field carries the geoarrow extension the writer /
    // geoparquet encoder detect; each level's geometry array is built as the
    // same type so RecordBatch assembly matches.
    let geom_out_field = mixed_geometry_field(&geom_name);
    let source_schema = build_source_schema(&input_schema, geom_idx, geom_out_field.clone());
    // Writer schema: base + point_count when clustering (Q4).
    let out_schema = if options.cluster {
        append_point_count_field(&source_schema)
    } else {
        source_schema.clone()
    };

    let writer_levels: Vec<LevelSpec> = emitted
        .iter()
        .map(|e| LevelSpec::new(e.gsd, e.zoom))
        .collect();
    let emitted_gsds: Vec<f64> = emitted.iter().map(|e| e.gsd).collect();
    let mut writer_opts = OverviewWriterOptions::new(options.mode, writer_levels);
    writer_opts.max_row_group_size = options.max_row_group_size;
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
            batch = apply_cluster_columns(batch, &out_schema, &e.indices, table, &acc_cols)?;
        }
        writer.write_level(level_idx, Some(e.indices.len()), std::iter::once(batch))?;
        level_reports.push(LevelReport {
            level: level_idx,
            gsd: e.gsd,
            zoom: e.zoom,
            feature_count: e.indices.len(),
            vertex_count: e.vertex_count,
            uncompressed_bytes: 0,
            compressed_bytes: 0,
        });
    }

    let meta = writer.finish()?;

    // --- Fill in real per-level byte sizes from the output footer. -----------
    fill_level_bytes(output_path, &meta, &mut level_reports)?;

    let total_rows: usize = level_reports.iter().map(|l| l.feature_count).sum();
    let total_vertices: usize = level_reports.iter().map(|l| l.vertex_count).sum();
    let total_compressed_bytes: i64 = level_reports.iter().map(|l| l.compressed_bytes).sum();

    Ok(ConvertReport {
        mode: options.mode,
        levels: level_reports,
        input_features: num_features,
        total_rows,
        total_vertices,
        total_compressed_bytes,
        duration_secs: start.elapsed().as_secs_f64(),
    })
}

// ============================================================================
// Helpers
// ============================================================================

/// Detect the input CRS and map it to [`Crs`], rejecting anything that is not
/// EPSG:4326 or EPSG:3857 (spec Q3).
pub(super) fn detect_crs(path: &Path) -> Result<Crs, ConvertError> {
    let info = crate::quality::extract_crs(path)?;
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
        engine: format!("gpq-tiles {}", env!("CARGO_PKG_VERSION")),
        // Only record the base when it deviates from the default: a default run
        // then produces a byte-identical footer to before this knob existed
        // (the levels[].gsd already imply the default base, §5.2 / Q6).
        gsd_base: if options.gsd_base == GSD_TILE_BASE {
            None
        } else {
            Some(options.gsd_base)
        },
        levels,
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
pub(super) fn class_ranking_provenance(mode: &str, cr: &ClassRanking) -> RankingProvenance {
    let ranks = if cr.ranks.len() <= MAX_PROVENANCE_RANKS {
        Some(cr.ranks.clone())
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

    /// Write a valid GeoParquet file (WKB, covering) with id/name/rank props and
    /// the given geometries. `extra_level_col` injects a `level` Int32 column to
    /// exercise the rejection path. `crs_projjson` overrides the geometry CRS.
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
    fn rejects_existing_level_column() {
        let geoms = synthetic_geometries();
        let tin = tempfile::NamedTempFile::new().unwrap();
        let tout = tempfile::NamedTempFile::new().unwrap();
        write_input(tin.path(), &geoms, true, None);

        let opts = ConvertOptions::default();
        let err = convert_to_overviews(tin.path(), tout.path(), &opts).unwrap_err();
        assert!(
            matches!(err, ConvertError::LevelColumnPresent),
            "expected LevelColumnPresent, got {err:?}"
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
        assert_eq!(r.ranks.unwrap().len(), 2);
        assert_eq!(r.unknown_rank, Some(0.0));
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
    fn cluster_rejects_existing_point_count_column() {
        // An input already carrying a `point_count` column is rejected when
        // clustering (case-insensitively), but accepted when clustering is off.
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
        let err = convert_to_overviews(
            tin.path(),
            tout.path(),
            &ConvertOptions {
                cluster: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, ConvertError::PointCountColumnPresent));

        // Without clustering the column is an ordinary property.
        convert_to_overviews(tin.path(), tout.path(), &ConvertOptions::default()).unwrap();
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
}
