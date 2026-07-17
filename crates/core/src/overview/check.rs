//! Conformance validator for GeoParquet overview files (spec §6.2).
//!
//! [`validate_file`] opens a file, parses its footer, and runs the full §6.2
//! checklist, returning a [`ValidationReport`] with one pass/fail entry per
//! check. It reports **all** failures (it is not fail-fast), so a producer can
//! see every structural problem in one pass.
//!
//! The validator checks *structure*, not cartographic quality (§6.2): whether
//! the file is a valid GeoParquet 1.1 overview file per this spec, not whether
//! its coarse levels are good renderings.

use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::Type as PhysicalType;
use parquet::file::metadata::ParquetMetaData;
use parquet::file::statistics::Statistics;

use crate::covering::{find_bbox_column_indices, get_geo_metadata, parse_covering_metadata};

use super::level::{Mode, OverviewsMeta, COGP_KEY, OVERVIEWS_KEY};
use super::writer::LEVEL_COLUMN;

/// The overviews-spec MAJOR version this validator supports (§3.8).
pub const SUPPORTED_MAJOR: u64 = 0;

/// Result of a single conformance check.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckResult {
    /// Short stable identifier for the check (e.g. `"level_column_type"`).
    pub name: String,
    /// Whether the check passed.
    pub passed: bool,
    /// Human-readable detail (why it passed/failed).
    pub message: String,
}

/// A collected report of every conformance check run against a file.
#[derive(Debug, Clone, Default)]
pub struct ValidationReport {
    /// One entry per check, in the order they were run.
    pub checks: Vec<CheckResult>,
}

impl ValidationReport {
    fn pass(&mut self, name: &str, message: impl Into<String>) {
        self.checks.push(CheckResult {
            name: name.to_string(),
            passed: true,
            message: message.into(),
        });
    }

    fn fail(&mut self, name: &str, message: impl Into<String>) {
        self.checks.push(CheckResult {
            name: name.to_string(),
            passed: false,
            message: message.into(),
        });
    }

    /// `true` iff every check passed.
    pub fn is_valid(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }

    /// Iterator over the failing checks.
    pub fn failures(&self) -> impl Iterator<Item = &CheckResult> {
        self.checks.iter().filter(|c| !c.passed)
    }

    /// Whether a specific named check passed. `None` if it was not run.
    pub fn check_passed(&self, name: &str) -> Option<bool> {
        self.checks
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.passed)
    }
}

/// Errors that prevent a validation run from starting at all (I/O, footer).
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// I/O error opening the file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Underlying parquet error (footer could not be parsed).
    #[error("parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
}

/// Minimal view of the GeoParquet `geo` metadata for structural checks.
#[derive(Debug, serde::Deserialize)]
struct GeoDoc {
    #[serde(default)]
    primary_column: Option<String>,
    #[serde(default)]
    columns: HashMap<String, serde_json::Value>,
}

/// Validate an overview file at `path` against the §6.2 conformance checklist.
///
/// Runs every metadata-only check of [`validate_metadata`], plus the
/// data-reading §12.1 cluster sum-invariant check (which needs the column
/// values, not just row-group statistics).
pub fn validate_file<P: AsRef<Path>>(path: P) -> Result<ValidationReport, CheckError> {
    let path = path.as_ref();
    let file = File::open(path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let metadata = builder.metadata().clone();
    let mut report = validate_metadata(&metadata);
    check_cluster_sum_invariant(path, &metadata, &mut report);
    Ok(report)
}

/// Run the §6.2 checklist against already-parsed Parquet metadata.
///
/// Split out so callers holding a [`ParquetMetaData`] (and tests) can validate
/// without re-opening the file.
pub fn validate_metadata(metadata: &ParquetMetaData) -> ValidationReport {
    let mut report = ValidationReport::default();
    let num_row_groups = metadata.num_row_groups();

    // --- GeoParquet 1.1: geo metadata + primary column + covering (§6.2). ----
    let geo_json = get_geo_metadata(metadata).ok().flatten();
    match &geo_json {
        None => {
            report.fail(
                "geoparquet_geo_metadata",
                "no 'geo' footer metadata: not a GeoParquet file",
            );
        }
        Some(j) => match serde_json::from_str::<GeoDoc>(j) {
            Ok(doc) if doc.primary_column.is_some() || !doc.columns.is_empty() => {
                report.pass(
                    "geoparquet_geo_metadata",
                    "'geo' metadata present with geometry column(s)",
                );
            }
            Ok(_) => report.fail(
                "geoparquet_geo_metadata",
                "'geo' metadata declares no geometry columns",
            ),
            Err(e) => report.fail(
                "geoparquet_geo_metadata",
                format!("'geo' metadata is not valid JSON: {e}"),
            ),
        },
    }

    // Covering declared (§4.4).
    let covering = geo_json
        .as_deref()
        .and_then(|j| parse_covering_metadata(j).ok().flatten());
    match &covering {
        Some(_) => report.pass("geoparquet_covering_declared", "bbox covering declared"),
        None => report.fail(
            "geoparquet_covering_declared",
            "no bbox covering declared in geo metadata (§4.4)",
        ),
    }

    // --- geo:overviews key present / parses / version / MAJOR (§6.2). --------
    let ov_json = footer_value(metadata, OVERVIEWS_KEY);
    let meta: Option<OverviewsMeta> = match ov_json {
        None => {
            report.fail("overviews_key_present", "'geo:overviews' footer key absent");
            None
        }
        Some(j) => match OverviewsMeta::from_json(&j) {
            Err(e) => {
                report.fail(
                    "overviews_key_present",
                    format!("'geo:overviews' is not valid JSON: {e}"),
                );
                None
            }
            Ok(meta) => {
                report.pass(
                    "overviews_key_present",
                    "'geo:overviews' present and parses",
                );
                // version semver + MAJOR supported.
                match parse_semver_major(&meta.version) {
                    None => report.fail(
                        "overviews_version",
                        format!("version {:?} is not valid semver", meta.version),
                    ),
                    Some(major) if major != SUPPORTED_MAJOR => report.fail(
                        "overviews_version",
                        format!("unsupported MAJOR version {major} (supported: {SUPPORTED_MAJOR})"),
                    ),
                    Some(_) => report.pass(
                        "overviews_version",
                        format!(
                            "version {} (MAJOR {SUPPORTED_MAJOR} supported)",
                            meta.version
                        ),
                    ),
                }
                Some(meta)
            }
        },
    };

    // --- structural invariants via level::validate (§3.3, §3.4). ------------
    if let Some(meta) = &meta {
        match meta.validate(num_row_groups as i64) {
            Ok(()) => report.pass(
                "overviews_structure",
                "levels/gsd/zoom/canonical invariants satisfied",
            ),
            Err(e) => report.fail("overviews_structure", e.to_string()),
        }

        // Explicit mode/canonical_level consistency (§3.4).
        check_mode_canonical(meta, &mut report);
    }

    // --- cogp compatibility key agreement (§3.1). ----------------------------
    if let Some(cogp_json) = footer_value(metadata, COGP_KEY) {
        match &meta {
            None => report.fail(
                "cogp_agreement",
                "'cogp' key present but 'geo:overviews' is missing/invalid",
            ),
            Some(meta) => match compare_cogp(meta, &cogp_json) {
                Ok(()) => report.pass("cogp_agreement", "'cogp' key agrees with 'geo:overviews'"),
                Err(e) => report.fail("cogp_agreement", e),
            },
        }
    }

    // --- level column: exists, INT32, NOT NULL (§4.1). ----------------------
    let level_col_idx = find_level_column(metadata);
    match level_col_idx {
        None => report.fail(
            "level_column",
            format!("no '{LEVEL_COLUMN}' column present"),
        ),
        Some(idx) => {
            let col = metadata.file_metadata().schema_descr().column(idx);
            let phys_ok = col.physical_type() == PhysicalType::INT32;
            let required = col.max_def_level() == 0;
            if phys_ok && required {
                report.pass(
                    "level_column",
                    format!("'{LEVEL_COLUMN}' is INT32 NOT NULL"),
                );
            } else {
                report.fail(
                    "level_column",
                    format!(
                        "'{LEVEL_COLUMN}' must be INT32 NOT NULL (physical={:?}, max_def_level={})",
                        col.physical_type(),
                        col.max_def_level()
                    ),
                );
            }
        }
    }

    // --- column↔footer consistency: per-RG level stats == footer level (§4.1).
    if let (Some(meta), Some(idx)) = (&meta, level_col_idx) {
        check_level_footer_consistency(metadata, meta, idx, &mut report);
    }

    // --- clustering metadata (Q4): point_count column + canonical values. ----
    if let Some(cl) = meta
        .as_ref()
        .and_then(|m| m.generalization.as_ref())
        .and_then(|g| g.clustering.as_ref())
        .filter(|c| c.enabled)
    {
        check_clustering(metadata, meta.as_ref().unwrap(), cl, &mut report);
    }

    // --- coalescing metadata (Q3): coalesced_count column + canonical values.
    if let Some(co) = meta
        .as_ref()
        .and_then(|m| m.generalization.as_ref())
        .and_then(|g| g.coalescing.as_ref())
        .filter(|c| c.enabled)
    {
        check_coalescing(metadata, meta.as_ref().unwrap(), co, &mut report);
    }

    // --- covering column per-RG min/max stats present (§4.4). ----------------
    match &covering {
        None => { /* already failed geoparquet_covering_declared */ }
        Some(spec) => match find_bbox_column_indices(metadata, spec) {
            None => report.fail(
                "covering_stats",
                "covering bbox columns not found in schema",
            ),
            Some(indices) => {
                let cols = [indices.xmin, indices.ymin, indices.xmax, indices.ymax];
                let mut missing = Vec::new();
                for rg in 0..num_row_groups {
                    for &c in &cols {
                        let chunk = metadata.row_group(rg).column(c);
                        let has = chunk
                            .statistics()
                            .map(|s| s.min_bytes_opt().is_some() && s.max_bytes_opt().is_some())
                            .unwrap_or(false);
                        if !has {
                            missing.push(rg);
                            break;
                        }
                    }
                }
                if missing.is_empty() {
                    report.pass(
                        "covering_stats",
                        "all row groups carry covering min/max statistics",
                    );
                } else {
                    report.fail(
                        "covering_stats",
                        format!("row groups missing covering min/max stats: {missing:?}"),
                    );
                }
            }
        },
    }

    report
}

/// Explicit §3.4 mode/canonical_level consistency (separate report entry).
fn check_mode_canonical(meta: &OverviewsMeta, report: &mut ValidationReport) {
    let l = meta.levels.len() as i64;
    match meta.mode {
        Some(Mode::Duplicating) => {
            if meta.canonical_level == Some(l - 1) {
                report.pass("mode_canonical", "duplicating: canonical_level == L-1");
            } else {
                report.fail(
                    "mode_canonical",
                    format!(
                        "duplicating requires canonical_level = {} (L-1), got {:?}",
                        l - 1,
                        meta.canonical_level
                    ),
                );
            }
        }
        Some(Mode::Partitioning) | None => {
            if meta.canonical_level.is_none() {
                report.pass(
                    "mode_canonical",
                    "partitioning: canonical_level is null/absent",
                );
            } else {
                report.fail(
                    "mode_canonical",
                    format!(
                        "partitioning requires canonical_level = null, got {:?}",
                        meta.canonical_level
                    ),
                );
            }
        }
    }
}

/// Clustering conformance (Q4, spec §12 draft): when the footer's
/// `generalization.clustering` block is present and enabled,
/// - the mode must be `duplicating` (clustering cannot be represented in
///   partitioning mode without double counting);
/// - the named point-count column must exist as INT64 NOT NULL;
/// - every row group's point_count stats must have `min >= 1`, and the
///   canonical level band's must be exactly `min == max == 1` (every
///   canonical cluster is a singleton).
fn check_clustering(
    metadata: &ParquetMetaData,
    meta: &OverviewsMeta,
    clustering: &crate::overview::level::ClusteringProvenance,
    report: &mut ValidationReport,
) {
    // Mode: duplicating only.
    match meta.mode {
        Some(Mode::Duplicating) => report.pass(
            "cluster_mode",
            "clustering metadata on a duplicating-mode file",
        ),
        other => {
            report.fail(
                "cluster_mode",
                format!(
                    "clustering metadata requires duplicating mode, found {other:?} \
                     (a partitioning row is read across many zoom prefixes; a \
                     per-level point_count would double count)"
                ),
            );
            return;
        }
    }

    // Column exists, INT64, NOT NULL.
    let name = clustering.point_count_column.as_str();
    let descr = metadata.file_metadata().schema_descr();
    let col_idx = (0..descr.num_columns()).find(|&i| descr.column(i).path().string() == name);
    let col_idx = match col_idx {
        None => {
            report.fail(
                "cluster_point_count_column",
                format!("clustering metadata names column {name:?} but it is absent"),
            );
            return;
        }
        Some(idx) => {
            let col = descr.column(idx);
            let phys_ok = col.physical_type() == PhysicalType::INT64;
            let required = col.max_def_level() == 0;
            if phys_ok && required {
                report.pass(
                    "cluster_point_count_column",
                    format!("{name:?} is INT64 NOT NULL"),
                );
                idx
            } else {
                report.fail(
                    "cluster_point_count_column",
                    format!(
                        "{name:?} must be INT64 NOT NULL (physical={:?}, max_def_level={})",
                        col.physical_type(),
                        col.max_def_level()
                    ),
                );
                return;
            }
        }
    };

    // Values: min >= 1 everywhere; canonical band exactly 1 (via RG stats).
    let canonical = meta.canonical_level;
    let mut problems: Vec<String> = Vec::new();
    for rg in 0..metadata.num_row_groups() {
        let is_canonical = level_for_rg(meta, rg).map(|k| k as i64) == canonical;
        let chunk = metadata.row_group(rg).column(col_idx);
        match chunk.statistics() {
            Some(Statistics::Int64(s)) => match (s.min_opt(), s.max_opt()) {
                (Some(&min), Some(&max)) => {
                    if min < 1 {
                        problems.push(format!("RG {rg}: point_count min={min} < 1"));
                    }
                    if is_canonical && (min != 1 || max != 1) {
                        problems.push(format!(
                            "RG {rg} (canonical): point_count min={min} max={max}, expected 1"
                        ));
                    }
                }
                _ => problems.push(format!("RG {rg}: point_count has no min/max stats")),
            },
            Some(other) => {
                problems.push(format!("RG {rg}: point_count stats not Int64 ({other:?})"))
            }
            None => problems.push(format!("RG {rg}: point_count column has no statistics")),
        }
    }
    if problems.is_empty() {
        report.pass(
            "cluster_point_count_values",
            "point_count >= 1 everywhere; canonical level all 1",
        );
    } else {
        report.fail("cluster_point_count_values", problems.join("; "));
    }
}

/// Coalescing conformance (Q3, spec §13 draft): when the footer's
/// `generalization.coalescing` block is present and enabled,
/// - the mode must be `duplicating` (merged chains cannot satisfy
///   partitioning's feature-once / geometry-verbatim contract);
/// - the named merged-count column must exist as INT32 NOT NULL;
/// - every row group's coalesced_count stats must have `min >= 1`, and the
///   canonical level band's must be exactly `min == max == 1` (the
///   canonical level is never coalesced, §2.4).
fn check_coalescing(
    metadata: &ParquetMetaData,
    meta: &OverviewsMeta,
    coalescing: &crate::overview::level::CoalescingProvenance,
    report: &mut ValidationReport,
) {
    // Mode: duplicating only.
    match meta.mode {
        Some(Mode::Duplicating) => report.pass(
            "coalesce_mode",
            "coalescing metadata on a duplicating-mode file",
        ),
        other => {
            report.fail(
                "coalesce_mode",
                format!(
                    "coalescing metadata requires duplicating mode, found {other:?} \
                     (a merged chain replaces several source rows, which \
                     partitioning's feature-once contract cannot represent)"
                ),
            );
            return;
        }
    }

    // Column exists, INT32, NOT NULL.
    let name = coalescing.coalesced_count_column.as_str();
    let descr = metadata.file_metadata().schema_descr();
    let col_idx = (0..descr.num_columns()).find(|&i| descr.column(i).path().string() == name);
    let col_idx = match col_idx {
        None => {
            report.fail(
                "coalesce_count_column",
                format!("coalescing metadata names column {name:?} but it is absent"),
            );
            return;
        }
        Some(idx) => {
            let col = descr.column(idx);
            let phys_ok = col.physical_type() == PhysicalType::INT32;
            let required = col.max_def_level() == 0;
            if phys_ok && required {
                report.pass(
                    "coalesce_count_column",
                    format!("{name:?} is INT32 NOT NULL"),
                );
                idx
            } else {
                report.fail(
                    "coalesce_count_column",
                    format!(
                        "{name:?} must be INT32 NOT NULL (physical={:?}, max_def_level={})",
                        col.physical_type(),
                        col.max_def_level()
                    ),
                );
                return;
            }
        }
    };

    // Values: min >= 1 everywhere; canonical band exactly 1 (via RG stats).
    let canonical = meta.canonical_level;
    let mut problems: Vec<String> = Vec::new();
    for rg in 0..metadata.num_row_groups() {
        let is_canonical = level_for_rg(meta, rg).map(|k| k as i64) == canonical;
        let chunk = metadata.row_group(rg).column(col_idx);
        match chunk.statistics() {
            Some(Statistics::Int32(s)) => match (s.min_opt(), s.max_opt()) {
                (Some(&min), Some(&max)) => {
                    if min < 1 {
                        problems.push(format!("RG {rg}: coalesced_count min={min} < 1"));
                    }
                    if is_canonical && (min != 1 || max != 1) {
                        problems.push(format!(
                            "RG {rg} (canonical): coalesced_count min={min} max={max}, expected 1"
                        ));
                    }
                }
                _ => problems.push(format!("RG {rg}: coalesced_count has no min/max stats")),
            },
            Some(other) => problems.push(format!(
                "RG {rg}: coalesced_count stats not Int32 ({other:?})"
            )),
            None => problems.push(format!("RG {rg}: coalesced_count column has no statistics")),
        }
    }
    if problems.is_empty() {
        report.pass(
            "coalesce_count_values",
            "coalesced_count >= 1 everywhere; canonical level all 1",
        );
    } else {
        report.fail("coalesce_count_values", problems.join("; "));
    }
}

/// §12.1 strict cluster sum invariant (data-reading check, `validate_file`
/// only): when clustering provenance is present and enabled on a structurally
/// sound duplicating file, every level's `Σ point_count` over its **point**
/// rows must equal the source point count exactly — taken as the number of
/// point rows in the canonical band, which is the source data verbatim
/// (§2.4). Implies the derived producer obligation: no clustered level may
/// thin its points to zero while the source contains points.
///
/// Skipped (no report entry) when the prerequisites are missing — the
/// structural checks of [`validate_metadata`] already fail those files.
fn check_cluster_sum_invariant(
    path: &Path,
    metadata: &ParquetMetaData,
    report: &mut ValidationReport,
) {
    // Prerequisites: parseable footer, enabled clustering, duplicating mode
    // with a valid canonical level, the named INT64 count column, the level
    // column, and a geometry column (needed to tell point rows apart).
    let Some(meta) =
        footer_value(metadata, OVERVIEWS_KEY).and_then(|j| OverviewsMeta::from_json(&j).ok())
    else {
        return;
    };
    let Some(clustering) = meta
        .generalization
        .as_ref()
        .and_then(|g| g.clustering.as_ref())
        .filter(|c| c.enabled)
    else {
        return;
    };
    if meta.mode != Some(Mode::Duplicating) {
        return; // cluster_mode already failed
    }
    let Some(canonical) = meta.canonical_level.filter(|&c| c >= 0) else {
        return; // mode_canonical already failed
    };

    match cluster_sum_by_level(path, &meta, clustering) {
        Err(e) => report.fail(
            "cluster_sum_invariant",
            format!("could not read cluster columns: {e}"),
        ),
        Ok(None) => { /* prerequisite column absent: already failed elsewhere */ }
        Ok(Some(per_level)) => {
            let Some(&(_, expected)) = per_level.get(canonical as usize) else {
                return; // canonical band outside levels: structure failed
            };
            let mut problems: Vec<String> = Vec::new();
            for (level, &(point_rows, sum)) in per_level.iter().enumerate() {
                if expected > 0 && point_rows == 0 {
                    problems.push(format!(
                        "level {level} has no point row to absorb {expected} \
                         source points"
                    ));
                } else if sum != expected {
                    problems.push(format!(
                        "level {level}: sum(point_count) over point rows = \
                         {sum}, expected source point count {expected}"
                    ));
                }
            }
            if problems.is_empty() {
                report.pass(
                    "cluster_sum_invariant",
                    format!(
                        "every level's point_count sums to the source point \
                         count ({expected}) (§12.1)"
                    ),
                );
            } else {
                report.fail("cluster_sum_invariant", problems.join("; "));
            }
        }
    }
}

/// Per level: `(point-row count, Σ point_count over point rows)`, read from
/// the file with a projection over the geometry / `level` / count columns.
/// `Ok(None)` when a prerequisite column is missing from the schema.
#[allow(clippy::type_complexity)]
fn cluster_sum_by_level(
    path: &Path,
    meta: &OverviewsMeta,
    clustering: &crate::overview::level::ClusteringProvenance,
) -> Result<Option<Vec<(usize, i64)>>, String> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Int32Type, Int64Type};
    use geoarrow::array::from_arrow_array;
    use parquet::arrow::ProjectionMask;

    use super::convert::{feature_kind, find_geometry_column};
    use crate::batch_processor::extract_geometries_opt_from_array;

    let file = File::open(path).map_err(|e| e.to_string())?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).map_err(|e| e.to_string())?;
    let schema = builder.schema().clone();

    let Some(geom_idx) = find_geometry_column(&schema) else {
        return Ok(None);
    };
    let Ok(pc_idx) = schema.index_of(&clustering.point_count_column) else {
        return Ok(None);
    };
    let Ok(level_idx) = schema.index_of(LEVEL_COLUMN) else {
        return Ok(None);
    };
    let geom_field = schema.field(geom_idx).clone();

    let mask = ProjectionMask::roots(builder.parquet_schema(), [geom_idx, pc_idx, level_idx]);
    let reader = builder
        .with_projection(mask)
        .build()
        .map_err(|e| e.to_string())?;

    let mut per_level: Vec<(usize, i64)> = vec![(0, 0); meta.levels.len()];
    let mut geoms: Vec<Option<geo::Geometry<f64>>> = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| e.to_string())?;
        let bschema = batch.schema();
        let g = batch.column(
            bschema
                .index_of(geom_field.name())
                .map_err(|e| e.to_string())?,
        );
        let levels = batch
            .column(bschema.index_of(LEVEL_COLUMN).map_err(|e| e.to_string())?)
            .as_primitive::<Int32Type>()
            .clone();
        let counts = batch
            .column(
                bschema
                    .index_of(&clustering.point_count_column)
                    .map_err(|e| e.to_string())?,
            )
            .as_primitive::<Int64Type>()
            .clone();

        let garr = from_arrow_array(g.as_ref(), &geom_field)
            .map_err(|e| format!("geometry decode: {e}"))?;
        geoms.clear();
        extract_geometries_opt_from_array(garr.as_ref(), &mut geoms)
            .map_err(|e| format!("geometry decode: {e}"))?;

        for (i, geom) in geoms.iter().enumerate() {
            // Only point rows participate in the sum (§12.1); null geometry
            // rows (foreign writers) cannot be classified and are skipped.
            let is_point = geom
                .as_ref()
                .is_some_and(|g| feature_kind(g) == super::assign::FeatureKind::Point);
            if !is_point {
                continue;
            }
            let level = levels.value(i);
            if level < 0 || level as usize >= per_level.len() {
                return Err(format!("row carries out-of-range level {level}"));
            }
            let entry = &mut per_level[level as usize];
            entry.0 += 1;
            entry.1 += counts.value(i);
        }
    }
    Ok(Some(per_level))
}

/// Per-RG `level` column statistics must be `min == max ==` the footer-implied
/// level for that RG index (§4.1). Records one summarizing entry.
fn check_level_footer_consistency(
    metadata: &ParquetMetaData,
    meta: &OverviewsMeta,
    level_col_idx: usize,
    report: &mut ValidationReport,
) {
    let mut problems: Vec<String> = Vec::new();
    for rg in 0..metadata.num_row_groups() {
        let expected = match level_for_rg(meta, rg) {
            Some(k) => k as i32,
            None => {
                problems.push(format!("RG {rg}: no footer level covers this row group"));
                continue;
            }
        };
        let chunk = metadata.row_group(rg).column(level_col_idx);
        match chunk.statistics() {
            Some(Statistics::Int32(s)) => match (s.min_opt(), s.max_opt()) {
                (Some(&min), Some(&max)) => {
                    if min != expected || max != expected {
                        problems.push(format!(
                            "RG {rg}: level stats min={min} max={max}, expected {expected}"
                        ));
                    }
                }
                _ => problems.push(format!("RG {rg}: level column has no min/max stats")),
            },
            Some(other) => problems.push(format!("RG {rg}: level stats not Int32 ({other:?})")),
            None => problems.push(format!("RG {rg}: level column has no statistics")),
        }
    }
    if problems.is_empty() {
        report.pass(
            "level_footer_consistency",
            "every row group's level stats match the footer",
        );
    } else {
        report.fail("level_footer_consistency", problems.join("; "));
    }
}

/// The footer-implied level index for row group `rg` per the §3.3 span rule.
/// `None` if `rg` falls outside every level band.
pub fn level_for_rg(meta: &OverviewsMeta, rg: usize) -> Option<usize> {
    let mut start = 0usize;
    for (k, level) in meta.levels.iter().enumerate() {
        let end = level.row_group_end as usize;
        if rg >= start && rg <= end {
            return Some(k);
        }
        start = end + 1;
    }
    None
}

/// Compare a `cogp` compatibility key's JSON against the authoritative
/// [`OverviewsMeta`] (§3.1). The `cogp` subset is `version` and
/// `levels[].{row_group_end, gsd}`; disagreement (including differing level
/// count) is an error.
pub fn compare_cogp(meta: &OverviewsMeta, cogp_json: &str) -> Result<(), String> {
    let v: serde_json::Value = serde_json::from_str(cogp_json)
        .map_err(|e| format!("'cogp' key is not valid JSON: {e}"))?;

    let cogp_version = v.get("version").and_then(|x| x.as_str());
    if cogp_version != Some(meta.version.as_str()) {
        return Err(format!(
            "cogp.version {:?} != geo:overviews.version {:?}",
            cogp_version, meta.version
        ));
    }

    let cogp_levels = v
        .get("levels")
        .and_then(|x| x.as_array())
        .ok_or_else(|| "cogp.levels missing or not an array".to_string())?;

    if cogp_levels.len() != meta.levels.len() {
        return Err(format!(
            "cogp has {} levels, geo:overviews has {}",
            cogp_levels.len(),
            meta.levels.len()
        ));
    }

    for (i, (cl, ml)) in cogp_levels.iter().zip(meta.levels.iter()).enumerate() {
        let rge = cl.get("row_group_end").and_then(|x| x.as_i64());
        if rge != Some(ml.row_group_end) {
            return Err(format!(
                "cogp.levels[{i}].row_group_end {:?} != {:?}",
                rge, ml.row_group_end
            ));
        }
        let gsd = cl.get("gsd").and_then(|x| x.as_f64());
        if gsd != Some(ml.gsd) {
            return Err(format!("cogp.levels[{i}].gsd {:?} != {:?}", gsd, ml.gsd));
        }
    }
    Ok(())
}

/// The value of footer key `key`, if present.
fn footer_value(metadata: &ParquetMetaData, key: &str) -> Option<String> {
    metadata
        .file_metadata()
        .key_value_metadata()?
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.clone())
}

/// Index (into the schema descriptor / row-group columns) of the `level` column.
fn find_level_column(metadata: &ParquetMetaData) -> Option<usize> {
    let descr = metadata.file_metadata().schema_descr();
    (0..descr.num_columns()).find(|&i| descr.column(i).path().string() == LEVEL_COLUMN)
}

/// Parse a `MAJOR.MINOR.PATCH` semver and return MAJOR, or `None` if invalid.
fn parse_semver_major(v: &str) -> Option<u64> {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    parts[0].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::level::{gsd, Level};
    use crate::overview::writer::{
        LevelSpec, LevelWriteOutcome, OverviewWriter, OverviewWriterOptions,
    };
    use arrow_array::{Int64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema, SchemaRef};
    use geo::{Geometry, LineString, Point, Polygon};
    use geoarrow::array::GeometryBuilder;
    use geoarrow::datatypes::GeometryType;
    use geoarrow_array::GeoArrowArray;
    use std::sync::Arc;

    // --- fixture builders ---------------------------------------------------

    fn geom_for(id: i64) -> Geometry {
        if id % 2 == 0 {
            Geometry::Point(Point::new(id as f64, id as f64))
        } else {
            let x = id as f64;
            let ext = LineString::from(vec![
                (x, x),
                (x + 1.0, x),
                (x + 1.0, x + 1.0),
                (x, x + 1.0),
                (x, x),
            ]);
            Geometry::Polygon(Polygon::new(ext, vec![]))
        }
    }

    fn build_geometry_array(ids: &[i64]) -> geoarrow::array::GeometryArray {
        let geoms: Vec<Option<Geometry>> = ids.iter().map(|&id| Some(geom_for(id))).collect();
        let typ = GeometryType::new(Default::default());
        let mut builder = GeometryBuilder::new(typ).with_prefer_multi(false);
        builder.extend_from_iter(geoms.iter().map(|x| x.as_ref()));
        builder.finish()
    }

    fn geometry_field() -> Field {
        let arr = build_geometry_array(&[0]);
        arr.data_type().to_field("geometry", true)
    }

    fn source_schema() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            geometry_field(),
        ])
    }

    fn source_batch(schema: &SchemaRef, ids: &[i64]) -> RecordBatch {
        let id_array = Int64Array::from(ids.to_vec());
        let name_array =
            StringArray::from(ids.iter().map(|id| format!("f{id}")).collect::<Vec<_>>());
        let geom_array = build_geometry_array(ids);
        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(id_array),
                Arc::new(name_array),
                Arc::new(geom_array.to_array_ref()),
            ],
        )
        .unwrap()
    }

    fn write_fixture(
        path: &std::path::Path,
        mode: Mode,
        level_ids: &[Vec<i64>],
        cogp: bool,
    ) -> OverviewsMeta {
        let schema = Arc::new(source_schema());
        let specs: Vec<LevelSpec> = (0..level_ids.len())
            .map(|k| {
                let z = (2 + 2 * k) as u8;
                LevelSpec::new(gsd(z), Some(z))
            })
            .collect();
        let mut opts = OverviewWriterOptions::new(mode, specs);
        opts.cogp_compat_key = cogp;
        let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
        for (k, ids) in level_ids.iter().enumerate() {
            assert_eq!(
                writer
                    .write_level(
                        k,
                        Some(ids.len()),
                        std::iter::once(source_batch(&schema, ids)),
                    )
                    .unwrap(),
                LevelWriteOutcome::Written
            );
        }
        writer.finish().unwrap()
    }

    fn meta_of(path: &std::path::Path) -> ParquetMetaData {
        let file = File::open(path).unwrap();
        ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .metadata()
            .as_ref()
            .clone()
    }

    // --- tests --------------------------------------------------------------

    #[test]
    fn good_duplicating_file_passes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 2], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]],
            false,
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(
            report.is_valid(),
            "unexpected failures: {:?}",
            report.failures().collect::<Vec<_>>()
        );
        // Spot-check specific checks ran and passed.
        assert_eq!(report.check_passed("level_column"), Some(true));
        assert_eq!(report.check_passed("level_footer_consistency"), Some(true));
        assert_eq!(report.check_passed("covering_stats"), Some(true));
        assert_eq!(report.check_passed("overviews_structure"), Some(true));
        assert_eq!(report.check_passed("mode_canonical"), Some(true));
    }

    #[test]
    fn good_partitioning_file_with_cogp_passes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            Mode::Partitioning,
            &[vec![0, 2], vec![1, 3], vec![4, 5]],
            true,
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(
            report.is_valid(),
            "unexpected failures: {:?}",
            report.failures().collect::<Vec<_>>()
        );
        assert_eq!(report.check_passed("cogp_agreement"), Some(true));
    }

    #[test]
    fn plain_parquet_missing_key_fails() {
        use parquet::arrow::ArrowWriter;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1i64]))])
                .unwrap();
        {
            let file = File::create(tmp.path()).unwrap();
            let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        }
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        assert_eq!(report.check_passed("overviews_key_present"), Some(false));
        assert_eq!(report.check_passed("geoparquet_geo_metadata"), Some(false));
        assert_eq!(report.check_passed("level_column"), Some(false));
    }

    // --- Q4 clustering checks -------------------------------------------------

    /// Write a 2-level duplicating fixture whose footer carries clustering
    /// provenance. `point_counts` supplies per-level `point_count` values
    /// (parallel to `level_ids`); `None` omits the column entirely.
    fn write_cluster_fixture(
        path: &std::path::Path,
        level_ids: &[Vec<i64>],
        point_counts: Option<&[Vec<i64>]>,
    ) {
        use crate::overview::level::{AccumulatedColumn, ClusteringProvenance, Generalization};
        use arrow_array::Int64Array as I64;

        let mut fields = vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(Field::new("name", DataType::Utf8, false)),
            Arc::new(geometry_field()),
        ];
        if point_counts.is_some() {
            fields.push(Arc::new(Field::new("point_count", DataType::Int64, false)));
        }
        let schema = Arc::new(Schema::new(fields));

        let specs: Vec<LevelSpec> = (0..level_ids.len())
            .map(|k| {
                let z = (2 + 2 * k) as u8;
                LevelSpec::new(gsd(z), Some(z))
            })
            .collect();
        let mut opts = OverviewWriterOptions::new(Mode::Duplicating, specs);
        opts.generalization = Some(Generalization {
            engine: "tylertoo test".to_string(),
            gsd_base: None,
            cascade: None,
            collapse: None,
            representation: None,
            levels: vec![],
            ranking: None,
            density_drop: None,
            clustering: Some(ClusteringProvenance {
                enabled: true,
                point_count_column: "point_count".to_string(),
                accumulated: vec![AccumulatedColumn {
                    column: "id".to_string(),
                    op: "sum".to_string(),
                }],
            }),
            coalescing: None,
        });

        let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
        for (k, ids) in level_ids.iter().enumerate() {
            let id_array = I64::from(ids.to_vec());
            let name_array =
                StringArray::from(ids.iter().map(|id| format!("f{id}")).collect::<Vec<_>>());
            let geom_array = build_geometry_array(ids);
            let mut columns: Vec<Arc<dyn arrow_array::Array>> = vec![
                Arc::new(id_array),
                Arc::new(name_array),
                Arc::new(geom_array.to_array_ref()),
            ];
            if let Some(counts) = point_counts {
                columns.push(Arc::new(I64::from(counts[k].clone())));
            }
            let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
            assert_eq!(
                writer
                    .write_level(k, Some(ids.len()), std::iter::once(batch))
                    .unwrap(),
                LevelWriteOutcome::Written
            );
        }
        writer.finish().unwrap();
    }

    #[test]
    fn clustering_good_file_passes() {
        // Fixture geometry: even ids are points, odd ids polygons. Canonical
        // level [0, 1, 2] has 2 source points (0, 2); the level-0 point row
        // (id 0) must carry point_count 2 (§12.1 sum invariant).
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_cluster_fixture(
            tmp.path(),
            &[vec![0], vec![0, 1, 2]],
            Some(&[vec![2], vec![1, 1, 1]]),
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(
            report.is_valid(),
            "unexpected failures: {:?}",
            report.failures().collect::<Vec<_>>()
        );
        assert_eq!(report.check_passed("cluster_mode"), Some(true));
        assert_eq!(
            report.check_passed("cluster_point_count_column"),
            Some(true)
        );
        assert_eq!(
            report.check_passed("cluster_point_count_values"),
            Some(true)
        );
        assert_eq!(report.check_passed("cluster_sum_invariant"), Some(true));
    }

    #[test]
    fn clustering_sum_invariant_violation_fails() {
        // Row-group statistics are conformant (min >= 1, canonical all 1),
        // but the level-0 point sums to 3 while the source holds 2 points:
        // an absorbed point was double counted (§12.1). Only the sum check
        // catches this.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_cluster_fixture(
            tmp.path(),
            &[vec![0], vec![0, 1, 2]],
            Some(&[vec![3], vec![1, 1, 1]]),
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        assert_eq!(
            report.check_passed("cluster_point_count_values"),
            Some(true),
            "stats-level checks must stay green (the violation is data-level)"
        );
        assert_eq!(report.check_passed("cluster_sum_invariant"), Some(false));
    }

    #[test]
    fn clustering_level_without_point_row_fails() {
        // Level 0 holds only a polygon (id 1) while the source has 2 points:
        // the derived producer obligation (§12.1) requires a surviving point
        // row per clustered level to absorb the counts.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_cluster_fixture(
            tmp.path(),
            &[vec![1], vec![0, 1, 2]],
            Some(&[vec![1], vec![1, 1, 1]]),
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        let failed = report
            .failures()
            .find(|c| c.name == "cluster_sum_invariant")
            .expect("cluster_sum_invariant must fail");
        assert!(
            failed.message.contains("no point row"),
            "unexpected message: {}",
            failed.message
        );
    }

    #[test]
    fn clustering_metadata_without_column_fails() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_cluster_fixture(tmp.path(), &[vec![0], vec![0, 1, 2]], None);
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        assert_eq!(
            report.check_passed("cluster_point_count_column"),
            Some(false)
        );
    }

    #[test]
    fn clustering_canonical_count_not_one_fails() {
        // Canonical level carries a point_count of 2 → conformance failure.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_cluster_fixture(
            tmp.path(),
            &[vec![0], vec![0, 1, 2]],
            Some(&[vec![3], vec![1, 2, 1]]),
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        assert_eq!(
            report.check_passed("cluster_point_count_values"),
            Some(false)
        );
    }

    #[test]
    fn clustering_zero_count_fails() {
        // A point_count of 0 anywhere violates the >= 1 invariant.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_cluster_fixture(
            tmp.path(),
            &[vec![0], vec![0, 1, 2]],
            Some(&[vec![0], vec![1, 1, 1]]),
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        assert_eq!(
            report.check_passed("cluster_point_count_values"),
            Some(false)
        );
    }

    // --- Q3 coalescing checks -------------------------------------------------

    /// Write a duplicating fixture whose footer carries coalescing
    /// provenance. `coalesced_counts` supplies per-level `coalesced_count`
    /// values (parallel to `level_ids`); `None` omits the column entirely.
    fn write_coalesce_fixture(
        path: &std::path::Path,
        level_ids: &[Vec<i64>],
        coalesced_counts: Option<&[Vec<i32>]>,
    ) {
        use crate::overview::level::{CoalescingProvenance, Generalization};
        use arrow_array::Int32Array as I32;

        let mut fields = vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(Field::new("name", DataType::Utf8, false)),
            Arc::new(geometry_field()),
        ];
        if coalesced_counts.is_some() {
            fields.push(Arc::new(Field::new(
                "coalesced_count",
                DataType::Int32,
                false,
            )));
        }
        let schema = Arc::new(Schema::new(fields));

        let specs: Vec<LevelSpec> = (0..level_ids.len())
            .map(|k| {
                let z = (2 + 2 * k) as u8;
                LevelSpec::new(gsd(z), Some(z))
            })
            .collect();
        let mut opts = OverviewWriterOptions::new(Mode::Duplicating, specs);
        opts.generalization = Some(Generalization {
            engine: "tylertoo test".to_string(),
            gsd_base: None,
            cascade: None,
            collapse: None,
            representation: None,
            levels: vec![],
            ranking: None,
            density_drop: None,
            clustering: None,
            coalescing: Some(CoalescingProvenance {
                enabled: true,
                snap_tolerance_gsd_factor: 1.0,
                junction_angle: Some(0.0),
                max_level_rows: Some(2_000_000),
                coalesced_count_column: "coalesced_count".to_string(),
            }),
        });

        let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
        for (k, ids) in level_ids.iter().enumerate() {
            let id_array = Int64Array::from(ids.to_vec());
            let name_array =
                StringArray::from(ids.iter().map(|id| format!("f{id}")).collect::<Vec<_>>());
            let geom_array = build_geometry_array(ids);
            let mut columns: Vec<Arc<dyn arrow_array::Array>> = vec![
                Arc::new(id_array),
                Arc::new(name_array),
                Arc::new(geom_array.to_array_ref()),
            ];
            if let Some(counts) = coalesced_counts {
                columns.push(Arc::new(I32::from(counts[k].clone())));
            }
            let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
            assert_eq!(
                writer
                    .write_level(k, Some(ids.len()), std::iter::once(batch))
                    .unwrap(),
                LevelWriteOutcome::Written
            );
        }
        writer.finish().unwrap();
    }

    #[test]
    fn coalescing_good_file_passes() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_coalesce_fixture(
            tmp.path(),
            &[vec![0], vec![0, 1, 2]],
            Some(&[vec![3], vec![1, 1, 1]]),
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(
            report.is_valid(),
            "unexpected failures: {:?}",
            report.failures().collect::<Vec<_>>()
        );
        assert_eq!(report.check_passed("coalesce_mode"), Some(true));
        assert_eq!(report.check_passed("coalesce_count_column"), Some(true));
        assert_eq!(report.check_passed("coalesce_count_values"), Some(true));
    }

    #[test]
    fn coalescing_metadata_without_column_fails() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_coalesce_fixture(tmp.path(), &[vec![0], vec![0, 1, 2]], None);
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        assert_eq!(report.check_passed("coalesce_count_column"), Some(false));
    }

    #[test]
    fn coalescing_canonical_count_not_one_fails() {
        // Canonical level carries a coalesced_count of 2 → conformance fail.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_coalesce_fixture(
            tmp.path(),
            &[vec![0], vec![0, 1, 2]],
            Some(&[vec![3], vec![1, 2, 1]]),
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        assert_eq!(report.check_passed("coalesce_count_values"), Some(false));
    }

    #[test]
    fn coalescing_zero_count_fails() {
        // A coalesced_count of 0 anywhere violates the >= 1 invariant.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_coalesce_fixture(
            tmp.path(),
            &[vec![0], vec![0, 1, 2]],
            Some(&[vec![0], vec![1, 1, 1]]),
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        assert_eq!(report.check_passed("coalesce_count_values"), Some(false));
    }

    #[test]
    fn no_coalescing_metadata_skips_checks() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 2], vec![0, 1, 2, 3]],
            false,
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(report.is_valid());
        assert_eq!(report.check_passed("coalesce_mode"), None);
        assert_eq!(report.check_passed("coalesce_count_column"), None);
    }

    #[test]
    fn no_clustering_metadata_skips_checks() {
        // A plain duplicating fixture never runs the cluster_* checks.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 2], vec![0, 1, 2, 3]],
            false,
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(report.is_valid());
        assert_eq!(report.check_passed("cluster_mode"), None);
        assert_eq!(report.check_passed("cluster_point_count_column"), None);
    }

    #[test]
    fn compare_cogp_agreement_and_disagreement() {
        let meta = OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Partitioning),
            canonical_level: None,
            levels: vec![
                Level {
                    row_group_end: 0,
                    gsd: 1000.0,
                    zoom: Some(6),
                },
                Level {
                    row_group_end: 3,
                    gsd: 500.0,
                    zoom: Some(7),
                },
            ],
            generalization: None,
        };
        // Agreement: exactly the subset the writer emits.
        let agree = r#"{"version":"0.1.0","levels":[
            {"row_group_end":0,"gsd":1000.0},{"row_group_end":3,"gsd":500.0}]}"#;
        assert!(compare_cogp(&meta, agree).is_ok());

        // Disagreement in row_group_end.
        let bad_rge = r#"{"version":"0.1.0","levels":[
            {"row_group_end":1,"gsd":1000.0},{"row_group_end":3,"gsd":500.0}]}"#;
        assert!(compare_cogp(&meta, bad_rge).is_err());

        // Disagreement in gsd.
        let bad_gsd = r#"{"version":"0.1.0","levels":[
            {"row_group_end":0,"gsd":999.0},{"row_group_end":3,"gsd":500.0}]}"#;
        assert!(compare_cogp(&meta, bad_gsd).is_err());

        // Disagreement in version.
        let bad_ver = r#"{"version":"0.2.0","levels":[
            {"row_group_end":0,"gsd":1000.0},{"row_group_end":3,"gsd":500.0}]}"#;
        assert!(compare_cogp(&meta, bad_ver).is_err());

        // Level-count mismatch.
        let bad_len = r#"{"version":"0.1.0","levels":[{"row_group_end":0,"gsd":1000.0}]}"#;
        assert!(compare_cogp(&meta, bad_len).is_err());
    }

    #[test]
    fn level_for_rg_maps_bands() {
        // Bands: level0 = RG 0..=1, level1 = 2..=5, level2 = 6..=14.
        let meta = OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Duplicating),
            canonical_level: Some(2),
            levels: vec![
                Level {
                    row_group_end: 1,
                    gsd: 9783.94,
                    zoom: Some(2),
                },
                Level {
                    row_group_end: 5,
                    gsd: 2445.98,
                    zoom: Some(4),
                },
                Level {
                    row_group_end: 14,
                    gsd: 611.5,
                    zoom: Some(6),
                },
            ],
            generalization: None,
        };
        assert_eq!(level_for_rg(&meta, 0), Some(0));
        assert_eq!(level_for_rg(&meta, 1), Some(0));
        assert_eq!(level_for_rg(&meta, 2), Some(1));
        assert_eq!(level_for_rg(&meta, 5), Some(1));
        assert_eq!(level_for_rg(&meta, 6), Some(2));
        assert_eq!(level_for_rg(&meta, 14), Some(2));
        assert_eq!(level_for_rg(&meta, 15), None);
    }

    #[test]
    fn parse_semver_major_cases() {
        assert_eq!(parse_semver_major("0.1.0"), Some(0));
        assert_eq!(parse_semver_major("1.2.3"), Some(1));
        assert_eq!(parse_semver_major("1.0"), None);
        assert_eq!(parse_semver_major("x.y.z"), None);
    }

    #[test]
    fn mode_canonical_synthetic_violation() {
        // Duplicating with wrong canonical_level → explicit mode_canonical fail.
        let meta = OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Duplicating),
            canonical_level: Some(0),
            levels: vec![
                Level {
                    row_group_end: 0,
                    gsd: 1000.0,
                    zoom: None,
                },
                Level {
                    row_group_end: 1,
                    gsd: 500.0,
                    zoom: None,
                },
            ],
            generalization: None,
        };
        let mut report = ValidationReport::default();
        check_mode_canonical(&meta, &mut report);
        assert_eq!(report.check_passed("mode_canonical"), Some(false));
    }

    #[test]
    fn validate_metadata_reports_all_failures_not_fail_fast() {
        // Plain parquet: multiple independent checks should each be recorded.
        use parquet::arrow::ArrowWriter;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1i64]))])
                .unwrap();
        {
            let file = File::create(tmp.path()).unwrap();
            let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        }
        let md = meta_of(tmp.path());
        let report = validate_metadata(&md);
        // At least the three independent structural checks are all present and failed.
        let failed: Vec<&str> = report.failures().map(|c| c.name.as_str()).collect();
        assert!(failed.contains(&"geoparquet_geo_metadata"));
        assert!(failed.contains(&"geoparquet_covering_declared"));
        assert!(failed.contains(&"overviews_key_present"));
        assert!(failed.contains(&"level_column"));
    }
}
