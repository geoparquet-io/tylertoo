//! In-memory overview conversion pipeline (task P5).
//!
//! [`convert_to_overviews`] wires the existing overview modules into a single
//! GeoParquet → GeoParquet overview build:
//!
//! 1. **read** the whole input GeoParquet preserving the full property schema
//!    (in-memory v1: the entire table is concatenated into one batch; the
//!    streaming refactor is task V4). The CRS is detected from the `geo`
//!    metadata and mapped to [`Crs`]; non-4326/3857 inputs and inputs that
//!    already carry a `level` column are rejected (spec Q3, §4.1).
//! 2. **assign** every feature a coarsest level via [`assign::assign_levels`]
//!    over per-feature bbox + [`FeatureKind`] + an optional sort key.
//! 3. **generalize + write**, coarse→fine, feeding [`OverviewWriter`]:
//!    - `duplicating` non-canonical levels: [`simplify::simplify_for_level`]
//!      per feature, dropping [`Simplified::Dropped`];
//!    - `duplicating` canonical (finest) level: original geometry **untouched**
//!      (spec §2.4, value-identity — no simplify round-trip);
//!    - `partitioning` (all levels): original geometry **verbatim** (§2.3).
//!    Input (Hilbert) order is preserved within each level (no re-sort).
//! 4. **report**: a [`ConvertReport`] (per-level feature/vertex/byte counts,
//!    totals, duration) is returned and is `serde` `Serialize` for the later
//!    benchmark tasks.
//!
//! This is the correctness-first, in-memory implementation. Memory is
//! `O(dataset)`; the bounded-memory streaming pipeline is a later task.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arrow_array::{Array, RecordBatch, UInt32Array};
use arrow_schema::{Field, Schema};
use arrow_select::concat::concat_batches;
use arrow_select::take::take;
use geo::{BoundingRect, Geometry};
use geoarrow::array::{from_arrow_array, GeometryBuilder};
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Serialize;

use crate::batch_processor::extract_geometries_from_array;

use super::assign::{assign_levels, AssignConfig, AssignFeature, FeatureKind};
use super::level::{gsd as gsd_for_zoom, Crs, Generalization, GeneralizationLevel, Mode};
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
    fn resolve(&self) -> Result<Vec<(f64, Option<u8>)>, ConvertError> {
        match self {
            LevelPlan::ZoomRange { min_zoom, max_zoom } => {
                if min_zoom > max_zoom {
                    return Err(ConvertError::InvalidLevels(format!(
                        "min_zoom {min_zoom} must be <= max_zoom {max_zoom}"
                    )));
                }
                Ok((*min_zoom..=*max_zoom)
                    .map(|z| (gsd_for_zoom(z), Some(z)))
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
                    if !(g > 0.0) {
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

/// Options for [`convert_to_overviews`].
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// Level materialization mode. Default [`Mode::Duplicating`].
    pub mode: Mode,
    /// How levels are specified (zoom range or explicit GSDs).
    pub levels: LevelPlan,
    /// Thinning / visibility / sort configuration for level assignment.
    pub assign: AssignConfig,
    /// Optional column name whose value is used as the cell-winner sort key.
    pub sort_key: Option<String>,
    /// Per-level simplification options (duplicating mode only).
    pub simplify: SimplifyOptions,
    /// Emit the optional COGP compatibility footer key (§3.1). Default `false`.
    pub cogp_compat_key: bool,
    /// Maximum row-group size in rows for the output writer.
    pub max_row_group_size: usize,
}

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
            simplify: SimplifyOptions::default(),
            cogp_compat_key: false,
            max_row_group_size: super::writer::DEFAULT_MAX_ROW_GROUP_SIZE,
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
    /// The level plan is invalid (empty / non-monotonic / bad zoom range).
    #[error("invalid level specification: {0}")]
    InvalidLevels(String),
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
    let start = Instant::now();
    let input_path = input_path.as_ref();
    let output_path = output_path.as_ref();

    // --- CRS detection + rejection (spec Q3). --------------------------------
    let crs = detect_crs(input_path)?;

    // --- Read the whole input, preserving the full property schema. ----------
    let file = std::fs::File::open(input_path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let input_schema = builder.schema().clone();

    // Reject an input that already carries a `level` column (§4.1).
    if input_schema
        .fields()
        .iter()
        .any(|f| f.name() == LEVEL_COLUMN)
    {
        return Err(ConvertError::LevelColumnPresent);
    }

    let geom_idx = find_geometry_column(&input_schema).ok_or(ConvertError::NoGeometryColumn)?;
    let geom_field = input_schema.field(geom_idx).clone();

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

    // Optional sort-key column.
    let sort_keys: Vec<Option<f64>> = match &options.sort_key {
        None => vec![None; num_features],
        Some(name) => {
            let idx = input_schema
                .index_of(name)
                .map_err(|_| ConvertError::SortKeyColumnMissing { name: name.clone() })?;
            extract_sort_keys(full.column(idx))
        }
    };

    // --- Level assignment. ---------------------------------------------------
    let level_specs = options.levels.resolve()?;
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
    let num_levels = level_gsds.len();
    let finest = num_levels.saturating_sub(1);

    // --- Build per-level generalized selections (coarse→fine). ---------------
    // Each emitted entry: (spec, feature indices, geometries, vertex_count).
    struct EmittedLevel {
        gsd: f64,
        zoom: Option<u8>,
        indices: Vec<usize>,
        geoms: Vec<Geometry<f64>>,
        vertex_count: usize,
    }
    let mut emitted: Vec<EmittedLevel> = Vec::new();

    for level in 0..num_levels {
        let (gsd_m, zoom) = level_specs[level];
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

    let writer_levels: Vec<LevelSpec> = emitted
        .iter()
        .map(|e| LevelSpec::new(e.gsd, e.zoom))
        .collect();
    let emitted_gsds: Vec<f64> = emitted.iter().map(|e| e.gsd).collect();
    let mut writer_opts = OverviewWriterOptions::new(options.mode, writer_levels);
    writer_opts.max_row_group_size = options.max_row_group_size;
    writer_opts.cogp_compat_key = options.cogp_compat_key;
    writer_opts.generalization = Some(build_generalization(&emitted_gsds, crs, options));

    let mut writer = OverviewWriter::create(output_path, &source_schema, writer_opts)?;

    // Column indices of the non-geometry source columns (preserve original order).
    let non_geom_cols: Vec<usize> = (0..input_schema.fields().len())
        .filter(|&c| c != geom_idx)
        .collect();

    let mut level_reports = Vec::with_capacity(emitted.len());
    for (level_idx, e) in emitted.iter().enumerate() {
        let batch = build_level_batch(
            &source_schema,
            &full,
            &non_geom_cols,
            geom_idx,
            &e.indices,
            &e.geoms,
        )?;
        writer.write_level(level_idx, std::iter::once(batch))?;
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
fn detect_crs(path: &Path) -> Result<Crs, ConvertError> {
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
fn find_geometry_column(schema: &Schema) -> Option<usize> {
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
fn geometry_bbox(g: &Geometry<f64>) -> [f64; 4] {
    match g.bounding_rect() {
        Some(r) => [r.min().x, r.min().y, r.max().x, r.max().y],
        None => [0.0, 0.0, 0.0, 0.0],
    }
}

/// Map a geometry to the [`FeatureKind`] used for thinning / visibility.
fn feature_kind(g: &Geometry<f64>) -> FeatureKind {
    match g {
        Geometry::Point(_) | Geometry::MultiPoint(_) => FeatureKind::Point,
        Geometry::LineString(_) | Geometry::MultiLineString(_) | Geometry::Line(_) => {
            FeatureKind::Line
        }
        _ => FeatureKind::Polygon,
    }
}

/// Count coordinates (vertices) in a geometry.
fn count_vertices(g: &Geometry<f64>) -> usize {
    use geo::coords_iter::CoordsIter;
    g.coords_count()
}

/// Extract an optional f64 sort key per row from a numeric Arrow column.
/// Non-numeric columns and null values yield `None`.
fn extract_sort_keys(col: &dyn Array) -> Vec<Option<f64>> {
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
fn mixed_geometry_field(name: &str) -> Arc<Field> {
    use geoarrow_array::GeoArrowArray;
    let typ = GeometryType::new(Default::default());
    let empty = GeometryBuilder::new(typ).with_prefer_multi(false).finish();
    Arc::new(empty.data_type().to_field(name, true))
}

/// Build the writer source schema: original fields, geometry field replaced by
/// the geoarrow-typed field, no file-level metadata (the encoder regenerates it).
fn build_source_schema(
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
fn build_level_batch(
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

/// Build informative generalization provenance (§3.5) from the emitted gsds.
fn build_generalization(gsds: &[f64], _crs: Crs, options: &ConvertOptions) -> Generalization {
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
        levels,
    }
}

/// Fill each level report's byte sizes by summing its row-group band from the
/// output file's Parquet footer.
fn fill_level_bytes(
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
            for i in 0..batch.num_rows() {
                out.push((
                    ids.value(i),
                    names.value(i).to_string(),
                    ranks.value(i),
                    gvec[i].clone(),
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
