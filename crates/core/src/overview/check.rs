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

use std::fs::File;
use std::path::Path;

use tylertoo_io::ParquetMetaData;
use tylertoo_io::PhysicalType;
use tylertoo_io::Statistics;

use crate::covering::find_bbox_column_indices;
use crate::resolved_schema::ResolvedSchema;

use super::level::{Mode, OverviewsMeta, COGP_KEY, OVERVIEWS_KEY};
use super::writer::LEVEL_COLUMN;

/// The overviews-spec MAJOR version this validator supports (§3.8).
pub use super::level::SUPPORTED_MAJOR;

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

    /// Failures that prevent reading. A marked partial shard overview remains
    /// readable; export separately enforces its recorded zoom ceiling (#541).
    pub(crate) fn read_failures(&self) -> impl Iterator<Item = &CheckResult> {
        self.failures().filter(|c| c.name != "complete_pyramid")
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
///
/// The messages carry no path and no "io error:" prefix: every caller
/// already says `could not open '<path>': ` in front (#452).
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    /// I/O error opening the file.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Underlying parquet error (footer could not be parsed).
    #[error(
        "cannot read the Parquet footer: {}",
        crate::input::parquet_error_detail(.0)
    )]
    Parquet(#[from] tylertoo_io::ParquetError),
}

/// Validate an overview file at `path` against the §6.2 conformance checklist.
///
/// Runs every metadata-only check of [`validate_metadata`], plus the
/// data-reading §12.1 cluster sum-invariant check (which needs the column
/// values, not just row-group statistics).
///
/// # Errors
///
/// Returns a [`CheckError`] if the file cannot be opened or its Parquet footer
/// cannot be parsed. Failed conformance checks are reported in the
/// [`ValidationReport`], not as an error.
pub fn validate_file<P: AsRef<Path>>(path: P) -> Result<ValidationReport, CheckError> {
    let diagnostics = &crate::diagnostics::Diagnostics::default();

    let path = path.as_ref();
    let file = File::open(path)?;
    let metadata = tylertoo_io::read_parquet_metadata(&file)?;
    let reader = match tylertoo_io::reader_metadata(std::sync::Arc::clone(&metadata)) {
        Ok(reader) => reader,
        Err(e) => {
            let mut report = ValidationReport::default();
            report.fail(
                "arrow_schema_decodes",
                format!("the footer's ARROW:schema cannot be decoded: {e}"),
            );
            return Ok(report);
        }
    };
    let descriptor = match ResolvedSchema::from_parquet(reader.schema(), &metadata) {
        Ok(descriptor) => descriptor,
        Err(e) => {
            let mut report = ValidationReport::default();
            report.fail("resolved_schema", e.to_string());
            return Ok(report);
        }
    };
    let mut report = validate_resolved_metadata(&metadata, &descriptor);
    check_cluster_sum_invariant(path, &metadata, &descriptor, &mut report, diagnostics);
    Ok(report)
}

/// Run the §6.2 checklist against already-parsed Parquet metadata.
///
/// Split out so callers holding a [`ParquetMetaData`] (and tests) can validate
/// without re-opening the file.
///
/// # Panics
///
/// Does not panic in practice: the clustering check unwraps the parsed
/// overviews metadata only after confirming it is present.
pub fn validate_metadata(metadata: &ParquetMetaData) -> ValidationReport {
    match ResolvedSchema::from_metadata(metadata) {
        Ok(descriptor) => validate_resolved_metadata(metadata, &descriptor),
        Err(e) => {
            let mut report = ValidationReport::default();
            report.fail("resolved_schema", e.to_string());
            report
        }
    }
}

pub(crate) fn validate_resolved_metadata(
    metadata: &ParquetMetaData,
    descriptor: &ResolvedSchema,
) -> ValidationReport {
    let mut report = ValidationReport::default();
    let num_row_groups = metadata.num_row_groups();

    // --- GeoParquet 1.1: geo metadata + primary column + covering (§6.2). ----
    if !descriptor.geometry_columns().is_empty() && descriptor.version().is_some() {
        report.pass(
            "geoparquet_geo_metadata",
            "'geo' metadata present with geometry column(s)",
        );
    } else {
        report.fail(
            "geoparquet_geo_metadata",
            "no valid GeoParquet geometry declaration",
        );
    }
    let covering = descriptor.covering();
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

        if !descriptor.renames().is_empty() {
            match descriptor.validate_renames() {
                Ok(()) => report.pass(
                    "renamed_columns",
                    "every rename maps real output and source columns",
                ),
                Err(message) => report.fail("renamed_columns", message),
            }
        }

        // #541 review: a partial overview (convert-side zoom ceiling) cannot
        // satisfy §2.4 — its finest level is a simplified one, not the
        // verbatim source — so it is not a conforming file, however sound its
        // structure. Only reported when the footer marks it; complete files
        // get no extra entry.
        if let Some(z) = meta.generalization.as_ref().and_then(|g| g.zoom_ceiling) {
            report.fail(
                "complete_pyramid",
                format!(
                    "partial overview: built with a zoom ceiling of z{z} (the coarse job of \
                     a sharded build), so no level finer than z{z} was written and the \
                     finest level is not the canonical source level (§2.4); export it only \
                     with a zoom ceiling of at most z{z}"
                ),
            );
        }
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

/// Explicit §3.4 `mode/canonical_level` consistency (separate report entry).
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
/// - every row group's `point_count` stats must have `min >= 1`, and the
///   canonical level band's must be exactly `min == max == 1` (every
///   canonical cluster is a singleton).
fn check_clustering(
    metadata: &ParquetMetaData,
    meta: &OverviewsMeta,
    clustering: &crate::overview::level::ClusteringProvenance,
    report: &mut ValidationReport,
) {
    check_count_conformance(metadata, meta, CountConformance {
        kind: "clustering", mode_key: "cluster_mode",
        mode_detail: "a partitioning row is read across many zoom prefixes; a per-level point_count would double count",
        name: &clustering.point_count_column, column_key: "cluster_point_count_column",
        values_key: "cluster_point_count_values", label: "point_count", physical: PhysicalType::INT64,
    }, report);
}

/// Coalescing count columns follow the same positive-count and canonical
/// singleton rules as clustering, with an INT32 physical representation.
fn check_coalescing(
    metadata: &ParquetMetaData,
    meta: &OverviewsMeta,
    coalescing: &crate::overview::level::CoalescingProvenance,
    report: &mut ValidationReport,
) {
    check_count_conformance(metadata, meta, CountConformance {
        kind: "coalescing", mode_key: "coalesce_mode",
        mode_detail: "a merged chain replaces several source rows, which partitioning's feature-once contract cannot represent",
        name: &coalescing.coalesced_count_column, column_key: "coalesce_count_column",
        values_key: "coalesce_count_values", label: "coalesced_count", physical: PhysicalType::INT32,
    }, report);
}

struct CountConformance<'a> {
    kind: &'static str,
    mode_key: &'static str,
    mode_detail: &'static str,
    name: &'a str,
    column_key: &'static str,
    values_key: &'static str,
    label: &'static str,
    physical: PhysicalType,
}

fn check_count_conformance(
    metadata: &ParquetMetaData,
    meta: &OverviewsMeta,
    spec: CountConformance<'_>,
    report: &mut ValidationReport,
) {
    let CountConformance {
        kind,
        mode_key,
        mode_detail,
        name,
        column_key,
        values_key,
        label,
        physical,
    } = spec;
    if meta.mode != Some(Mode::Duplicating) {
        report.fail(
            mode_key,
            format!(
                "{kind} metadata requires duplicating mode, found {:?} ({mode_detail})",
                meta.mode
            ),
        );
        return;
    }
    report.pass(
        mode_key,
        format!("{kind} metadata on a duplicating-mode file"),
    );

    let descr = metadata.file_metadata().schema_descr();
    let Some(col_idx) = (0..descr.num_columns()).find(|&i| descr.column(i).path().string() == name)
    else {
        report.fail(
            column_key,
            format!("{kind} metadata names column {name:?} but it is absent"),
        );
        return;
    };
    let col = descr.column(col_idx);
    if col.physical_type() != physical || col.max_def_level() != 0 {
        report.fail(
            column_key,
            format!(
                "{name:?} must be {physical:?} NOT NULL (physical={:?}, max_def_level={})",
                col.physical_type(),
                col.max_def_level()
            ),
        );
        return;
    }
    report.pass(column_key, format!("{name:?} is {physical:?} NOT NULL"));

    let stat_kind = if physical == PhysicalType::INT32 {
        "Int32"
    } else {
        "Int64"
    };
    let mut problems = Vec::new();
    for rg in 0..metadata.num_row_groups() {
        let is_canonical = level_for_rg(meta, rg).map(|k| k as i64) == meta.canonical_level;
        let stats = metadata.row_group(rg).column(col_idx).statistics();
        let bounds = match stats {
            Some(Statistics::Int32(s)) if physical == PhysicalType::INT32 => Some((
                s.min_opt().copied().map(i64::from),
                s.max_opt().copied().map(i64::from),
            )),
            Some(Statistics::Int64(s)) if physical == PhysicalType::INT64 => {
                Some((s.min_opt().copied(), s.max_opt().copied()))
            }
            Some(other) => {
                problems.push(format!(
                    "RG {rg}: {label} stats not {stat_kind} ({other:?})"
                ));
                None
            }
            None => {
                problems.push(format!("RG {rg}: {label} column has no statistics"));
                None
            }
        };
        match bounds {
            Some((Some(min), Some(max))) => {
                if min < 1 {
                    problems.push(format!("RG {rg}: {label} min={min} < 1"));
                }
                if is_canonical && (min != 1 || max != 1) {
                    problems.push(format!(
                        "RG {rg} (canonical): {label} min={min} max={max}, expected 1"
                    ));
                }
            }
            Some(_) => problems.push(format!("RG {rg}: {label} has no min/max stats")),
            None => {}
        }
    }
    if problems.is_empty() {
        report.pass(
            values_key,
            format!("{label} >= 1 everywhere; canonical level all 1"),
        );
    } else {
        report.fail(values_key, problems.join("; "));
    }
}

/// §12.1 strict cluster sum invariant, shared by validation and reader open: when clustering provenance is present and enabled on a structurally
/// sound duplicating file, every level's `Σ point_count` over its **point**
/// rows must equal the source point count exactly — taken as the number of
/// point rows in the canonical band, which is the source data verbatim
/// (§2.4). Implies the derived producer obligation: no clustered level may
/// thin its points to zero while the source contains points.
///
/// Skipped (no report entry) when the prerequisites are missing — the
/// structural checks of [`validate_metadata`] already fail those files.
pub(crate) fn check_cluster_sum_invariant(
    path: &Path,
    metadata: &ParquetMetaData,
    descriptor: &ResolvedSchema,
    report: &mut ValidationReport,
    diagnostics: &crate::diagnostics::Diagnostics,
) {
    // A malformed count/level type must be reported by the metadata validator,
    // never passed into Arrow's infallible primitive casts. A partial file has
    // no canonical source band against which to check the sum.
    if report.check_passed("cluster_point_count_column") != Some(true)
        || report.check_passed("level_column") != Some(true)
        || report.check_passed("overviews_structure") != Some(true)
        || report.check_passed("complete_pyramid") == Some(false)
    {
        return;
    }
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

    if descriptor.source_is_subset() {
        report.pass("cluster_sum_invariant", "spatial subset: levels are independently cropped; whole-source mass equality is not applicable");
        return;
    }

    match cluster_sum_by_level(path, &meta, clustering, descriptor, diagnostics) {
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
                let mut message = problems.join("; ");
                if footer_value(metadata, crate::resolved_schema::RESOLVED_SCHEMA_KEY).is_none()
                    && footer_value(metadata, crate::resolved_schema::SOURCE_SUBSET_KEY).is_none()
                {
                    message.push_str(&format!(
                        "; legacy clustered overview (overview format {}): tylertoo 0.8.x \
                         and earlier did not record spatial-subset provenance, so a cropped \
                         shard cannot be distinguished from invalid cluster counts. Recreate \
                         this overview from the original GeoParquet with current tylertoo",
                        meta.version
                    ));
                }
                report.fail("cluster_sum_invariant", message);
            }
        }
    }
}

/// Per level: `(point-row count, Σ point_count over point rows)`, read from
/// the file with a projection over the geometry / `level` / count columns.
/// `Ok(None)` when a prerequisite column is missing from the schema.
fn cluster_sum_by_level(
    path: &Path,
    meta: &OverviewsMeta,
    clustering: &crate::overview::level::ClusteringProvenance,
    descriptor: &ResolvedSchema,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Option<Vec<(usize, i64)>>, String> {
    use super::convert::feature_kind;
    use crate::batch_processor::extract_geometries_opt_from_array;
    use arrow_array::cast::AsArray;
    use arrow_array::types::{Int32Type, Int64Type};
    use geoarrow::array::from_arrow_array;

    let file = File::open(path).map_err(|e| e.to_string())?;
    let builder = tylertoo_io::open_parquet(file).map_err(|e| e.to_string())?;
    let schema = builder.schema().clone();

    let Some(geom_idx) = descriptor.geometry_index() else {
        return Ok(None);
    };
    let Ok(pc_idx) = schema.index_of(&clustering.point_count_column) else {
        return Ok(None);
    };
    let Ok(level_idx) = schema.index_of(LEVEL_COLUMN) else {
        return Ok(None);
    };
    let geom_field = schema.field(geom_idx).clone();

    // The reader's default batch, capped by the owner (#607) so one batch of
    // the widest byte-array column stays under arrow's 2 GiB offset limit.
    let reader = builder
        .with_projection_roots([geom_idx, pc_idx, level_idx])
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
        extract_geometries_opt_from_array(garr.as_ref(), &mut geoms, diagnostics)
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
            // Checked (#430): a hostile point_count column must not wrap the
            // sum (release) or panic (debug).
            entry.1 = entry.1.checked_add(counts.value(i)).ok_or_else(|| {
                format!("level {level}: sum(point_count) overflows i64 (hostile point_count)")
            })?;
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
        // Checked (#430): a negative `row_group_end` is not a row-group
        // index (an `as usize` cast would map -1 to usize::MAX and claim
        // every row group); a band ending at usize::MAX has no successor.
        let end = usize::try_from(level.row_group_end).ok()?;
        if rg >= start && rg <= end {
            return Some(k);
        }
        start = end.checked_add(1)?;
    }
    None
}

/// Compare a `cogp` compatibility key's JSON against the authoritative [`OverviewsMeta`].
///
/// See §3.1. The `cogp` subset is `version` and
/// `levels[].{row_group_end, gsd}`; disagreement (including differing level
/// count) is an error.
///
/// # Errors
///
/// Returns a description of the first mismatch if `cogp_json` is not valid
/// JSON, has no `levels` array, or disagrees with `meta` on the version, the
/// level count, or any level's `row_group_end` or `gsd`.
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

use super::level::parse_semver_major;

/// Fuzz hook (#633): decode `thrift` as a Parquet footer (the bytes before
/// the trailing length and `PAR1`) and put it through every tylertoo reader
/// that works on footer metadata alone.
///
/// That is: this module's §6.2 checklist ([`validate_metadata`], which keeps
/// going past a failed `validate()`), the `geo:overviews` parse and
/// `validate` that [`super::reader::OverviewReader::open`] runs, the
/// row-group bbox readers (covering columns and native geo statistics), the
/// spill-size and batch-size heuristics, and `--filter` pushdown over the
/// footer's column statistics, with a few filters bound to the footer's own
/// columns. The footer goes through the owner's decode
/// ([`tylertoo_io::decode_footer`]: the #690 pre-check, then parquet-rs
/// inside the panic boundary), and the embedded `ARROW:schema` is left to
/// the whole-file targets. Only the `fuzz/` crate calls this; the `fuzzing`
/// feature is never on in a normal build.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_parquet_footer(thrift: &[u8]) -> Result<ValidationReport, tylertoo_io::ParquetError> {
    use super::filter::{BoundFilter, CmpOp, FilterExpr, Literal};
    use super::level::Crs;

    // The owner's decode (#779): the #690 pre-check, then parquet-rs's
    // decode inside the panic boundary.
    let md = tylertoo_io::decode_footer(thrift)?;
    let report = validate_metadata(&md);

    let n = md.num_row_groups();
    if let Some(json) = md
        .file_metadata()
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|kv| kv.key == OVERVIEWS_KEY))
        .and_then(|kv| kv.value.as_deref())
    {
        if let Ok(meta) = OverviewsMeta::from_json(json) {
            let _ = meta.validate(n as i64);
        }
    }

    let _ = crate::covering::extract_row_group_bounds_from_metadata(&md);
    let geom = crate::covering::resolve_geometry_column_name(&md);
    for crs in [Crs::Epsg4326, Crs::Epsg3857] {
        let _ = crate::covering::extract_row_group_bounds_tiered(&md, crs, geom.as_deref());
    }

    let some: Vec<usize> = (0..n).step_by(2).collect();
    for selected in [None, Some(some.as_slice())] {
        let _ = crate::input::selected_compressed_bytes(&md, selected);
        let _ = tylertoo_io::read_batch_plan(&md, selected, 8192, tylertoo_io::byte_array_limit());
        let _ = tylertoo_io::largest_byte_array_column(&md, selected, |i| i % 2 == 0);
    }

    // Without the key-value metadata: the embedded `ARROW:schema` hint is
    // arrow-ipc's to decode (guarded by the owner since #779, #683);
    // `overview_file` and `remote_footer` reach that through the real open.
    // Here the filters only need column names and types.
    if let Ok(schema) = tylertoo_io::schema_without_metadata(&md) {
        for field in schema.fields() {
            let column = field.name().clone();
            let filters = [
                FilterExpr::IsNull {
                    column: column.clone(),
                    negated: true,
                },
                FilterExpr::Compare {
                    column: column.clone(),
                    op: CmpOp::Gt,
                    value: Literal::Number(0.0),
                },
                FilterExpr::Compare {
                    column: column.clone(),
                    op: CmpOp::Le,
                    value: Literal::String("m".into()),
                },
                FilterExpr::In {
                    column,
                    values: vec![Literal::Bool(true), Literal::Timestamp(0)],
                    negated: false,
                },
            ];
            for f in filters {
                if let Ok(bound) = BoundFilter::bind(&f, &schema, &[]) {
                    let _ = bound.select_row_groups(&md);
                }
            }
        }
    }
    Ok(report)
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

    /// #452: callers prefix the path ("could not open 'x': ..."), so the
    /// error itself must not add "io error:" / "parquet error: Parquet
    /// error:" layers on top.
    #[test]
    fn open_errors_carry_no_stuttering_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let msg = validate_file(dir.path().join("missing.parquet"))
            .unwrap_err()
            .to_string();
        assert!(msg.starts_with("No such file"), "{msg}");

        let corrupt = dir.path().join("corrupt.parquet");
        std::fs::write(&corrupt, b"definitely not parquet").unwrap();
        let msg = validate_file(&corrupt).unwrap_err().to_string();
        assert!(msg.contains("Corrupt footer"), "{msg}");
        assert!(!msg.to_lowercase().contains("parquet error"), "{msg}");
    }

    // --- fixture builders ---------------------------------------------------

    fn with_rename_provenance(path: &Path, renames: serde_json::Value, persisted: bool) {
        use tylertoo_io::test_support::{ArrowWriter, WriterProperties};
        use tylertoo_io::KeyValue;
        let builder = tylertoo_io::open_parquet(File::open(path).unwrap()).unwrap();
        let mut kv = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .clone();
        kv.retain(|kv| {
            kv.key != crate::resolved_schema::RESOLVED_SCHEMA_KEY && kv.key != "ARROW:schema"
        });
        let footer = kv.iter_mut().find(|kv| kv.key == OVERVIEWS_KEY).unwrap();
        let mut json: serde_json::Value =
            serde_json::from_str(footer.value.as_deref().unwrap()).unwrap();
        json["generalization"] = serde_json::json!({
            "engine": "test", "levels": [], "renamed_columns": renames
        });
        footer.value = Some(json.to_string());
        let descriptor = if persisted {
            let schema = Schema::new_with_metadata(
                builder.schema().fields().clone(),
                kv.iter()
                    .filter_map(|kv| kv.value.as_ref().map(|v| (kv.key.clone(), v.clone())))
                    .collect(),
            );
            Some(
                ResolvedSchema::resolve(&schema)
                    .unwrap()
                    .footer_json()
                    .unwrap(),
            )
        } else {
            None
        };
        let schema = Arc::new(Schema::new(builder.schema().fields().clone()));
        let batches: Vec<_> = builder.build().unwrap().map(Result::unwrap).collect();
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(
                kv.into_iter()
                    .map(|kv| KeyValue::new(kv.key, kv.value))
                    .collect(),
            ))
            .build();
        let mut writer =
            ArrowWriter::try_new(File::create(path).unwrap(), schema, Some(props)).unwrap();
        for batch in batches {
            writer.write(&batch).unwrap();
        }
        if let Some(json) = descriptor {
            writer.append_key_value_metadata(KeyValue::new(
                crate::resolved_schema::RESOLVED_SCHEMA_KEY.into(),
                json,
            ));
        }
        writer.close().unwrap();
    }

    #[test]
    fn rename_provenance_is_checked_by_check_and_reader_open() {
        for (renames, persisted) in [
            serde_json::json!({"absent": "level"}),
            serde_json::json!({"id": "absent_source"}),
            serde_json::json!({"id": "id"}),
        ]
        .into_iter()
        .flat_map(|r| [(r.clone(), false), (r, true)])
        {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            write_fixture(tmp.path(), Mode::Duplicating, &[vec![0, 1]], false);
            with_rename_provenance(tmp.path(), renames.clone(), persisted);
            let report = validate_file(tmp.path()).unwrap();
            assert_eq!(
                report.check_passed("renamed_columns"),
                Some(false),
                "{renames}: {report:?}"
            );
            let md = tylertoo_io::read_parquet_metadata(&File::open(tmp.path()).unwrap()).unwrap();
            assert_eq!(
                validate_metadata(&md).check_passed("renamed_columns"),
                Some(false)
            );
            let error = super::super::reader::OverviewReader::open(tmp.path()).unwrap_err();
            assert!(error.to_string().contains("renamed_columns"), "{error}");
        }
    }

    #[test]
    fn valid_and_ambiguous_rename_provenance_remains_readable() {
        for (renames, persisted) in [
            serde_json::json!({"id": "level"}),
            // The spec permits duplicate source names; publication restores at most one.
            serde_json::json!({"id": "level", "name": "level"}),
            // A published target prevents restoration, but is still real provenance.
            serde_json::json!({"id": "name"}),
        ]
        .into_iter()
        .flat_map(|r| [(r.clone(), false), (r, true)])
        {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            write_fixture(tmp.path(), Mode::Duplicating, &[vec![0, 1]], false);
            with_rename_provenance(tmp.path(), renames.clone(), persisted);
            let report = validate_file(tmp.path()).unwrap();
            assert_eq!(
                report.check_passed("renamed_columns"),
                Some(true),
                "{renames}: {report:?}"
            );
            super::super::reader::OverviewReader::open(tmp.path()).unwrap();
        }
    }

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
        tylertoo_io::read_parquet_metadata(&file)
            .unwrap()
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
        use tylertoo_io::test_support::ArrowWriter;
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
    #[cfg(test)]
    fn write_cluster_fixture(
        path: &std::path::Path,
        level_ids: &[Vec<i64>],
        point_counts: Option<&[Vec<i64>]>,
    ) {
        write_cluster_fixture_with_subset(path, level_ids, point_counts, false);
    }

    fn write_cluster_fixture_with_subset(
        path: &std::path::Path,
        level_ids: &[Vec<i64>],
        point_counts: Option<&[Vec<i64>]>,
        source_is_subset: bool,
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
        opts.source_is_subset = source_is_subset;
        opts.generalization = Some(Generalization {
            engine: "tylertoo test".to_string(),
            gsd_base: None,
            cascade: None,
            collapse: None,
            representation: None,
            levels: vec![],
            ranking: None,
            entry_zoom: None,
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
            renamed_columns: None,
            zoom_ceiling: None,
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
    fn legacy_clustered_shard_has_versioned_actionable_error() {
        use crate::overview::{
            export::{export_pmtiles, ExportOptions},
            reader::OverviewReader,
        };
        use crate::resolved_schema::{RESOLVED_SCHEMA_KEY, SOURCE_SUBSET_KEY};
        use tylertoo_io::test_support::{ArrowWriter, WriterProperties};
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("current.parquet");
        let legacy = dir.path().join("legacy.parquet");
        // A boundary cluster carries an out-of-shard point at level 0;
        // canonical cropping retains only one of its source points.
        write_cluster_fixture_with_subset(
            &current,
            &[vec![0], vec![0]],
            Some(&[vec![2], vec![1]]),
            true,
        );
        assert!(validate_file(&current).unwrap().is_valid());
        OverviewReader::open(&current).unwrap();
        export_pmtiles(
            &current,
            dir.path().join("current.pmtiles"),
            &ExportOptions::default(),
        )
        .unwrap();

        let builder = tylertoo_io::open_parquet(File::open(&current).unwrap()).unwrap();
        let mut metadata = builder.schema().metadata().clone();
        metadata.remove(RESOLVED_SCHEMA_KEY);
        metadata.remove(SOURCE_SUBSET_KEY);
        let schema = Arc::new(Schema::new_with_metadata(
            builder.schema().fields().clone(),
            metadata,
        ));
        let kvs = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .filter(|kv| {
                ![RESOLVED_SCHEMA_KEY, SOURCE_SUBSET_KEY, "ARROW:schema"].contains(&kv.key.as_str())
            })
            .cloned()
            .collect();
        let props = WriterProperties::builder()
            .set_key_value_metadata(Some(kvs))
            .build();
        let mut writer =
            ArrowWriter::try_new(File::create(&legacy).unwrap(), schema.clone(), Some(props))
                .unwrap();
        let groups = builder.metadata().num_row_groups();
        for rg in 0..groups {
            for batch in tylertoo_io::open_parquet(File::open(&current).unwrap())
                .unwrap()
                .with_row_groups(vec![rg])
                .build()
                .unwrap()
            {
                writer
                    .write(
                        &RecordBatch::try_new(schema.clone(), batch.unwrap().columns().to_vec())
                            .unwrap(),
                    )
                    .unwrap();
            }
            writer.flush().unwrap();
        }
        writer.close().unwrap();
        let report = validate_file(&legacy).unwrap();
        let failure = report
            .failures()
            .find(|c| c.name == "cluster_sum_invariant")
            .unwrap();
        assert!(failure.message.contains("0.8.x"), "{}", failure.message);
        assert!(failure.message.contains("Recreate"), "{}", failure.message);
        let error = OverviewReader::open(&legacy).unwrap_err().to_string();
        assert!(
            error.contains("0.8.x") && error.contains("Recreate"),
            "{error}"
        );
        let archive = dir.path().join("legacy.pmtiles");
        let error = export_pmtiles(&legacy, &archive, &ExportOptions::default())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("0.8.x") && error.contains("Recreate"),
            "{error}"
        );
        assert!(!archive.exists());
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
    #[cfg(test)]
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
            entry_zoom: None,
            density_drop: None,
            clustering: None,
            coalescing: Some(CoalescingProvenance {
                enabled: true,
                snap_tolerance_gsd_factor: 1.0,
                junction_angle: Some(0.0),
                max_level_rows: Some(2_000_000),
                coalesced_count_column: "coalesced_count".to_string(),
                merged: None,
            }),
            renamed_columns: None,
            zoom_ceiling: None,
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

    /// #430: hostile `row_group_end` values (negative, or `i64::MAX` so the
    /// next band's start would overflow) must not panic or misreport a band;
    /// a level whose end cannot be a row-group index covers nothing.
    #[test]
    fn level_for_rg_hostile_row_group_end_does_not_panic() {
        let level = |end: i64| Level {
            row_group_end: end,
            gsd: 1000.0,
            zoom: None,
        };
        let meta = |levels: Vec<Level>| OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Duplicating),
            canonical_level: Some(0),
            levels,
            generalization: None,
        };
        // Negative end: not a row-group index, covers no row group.
        let negative = meta(vec![level(-1), level(3)]);
        assert_eq!(level_for_rg(&negative, 0), None);
        assert_eq!(level_for_rg(&negative, 3), None);
        // i64::MAX end: the band is open-ended; the (impossible) next band
        // start must not overflow.
        let huge = meta(vec![level(1), level(i64::MAX), level(5)]);
        assert_eq!(level_for_rg(&huge, 0), Some(0));
        assert_eq!(level_for_rg(&huge, 2), Some(1));
        assert_eq!(level_for_rg(&huge, i64::MAX as usize), Some(1));
        // Beyond every representable end: no band, no wrap.
        assert_eq!(level_for_rg(&huge, usize::MAX), None);
    }

    /// #430: a hostile `point_count` column whose per-level sum overflows
    /// i64 must be reported as a check failure, never a panic (debug) or a
    /// wrapped, silently wrong sum (release).
    #[test]
    fn clustering_sum_overflow_is_reported_not_panic() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Level 0 has two point rows (ids 0 and 2) whose counts overflow.
        write_cluster_fixture(
            tmp.path(),
            &[vec![0, 2], vec![0, 1, 2]],
            Some(&[vec![i64::MAX, i64::MAX], vec![1, 1, 1]]),
        );
        let report = validate_file(tmp.path()).unwrap();
        assert!(!report.is_valid());
        assert_eq!(report.check_passed("cluster_sum_invariant"), Some(false));
        let msg = report
            .failures()
            .find(|f| f.name == "cluster_sum_invariant")
            .map(|f| f.message.clone())
            .unwrap_or_default();
        assert!(
            msg.contains("overflow"),
            "expected overflow report, got: {msg}"
        );
    }

    #[test]
    fn parse_semver_major_cases() {
        assert_eq!(parse_semver_major("0.1.0"), Some(0));
        assert_eq!(parse_semver_major("1.2.3"), Some(1));
        assert_eq!(parse_semver_major("1.0"), None);
        assert_eq!(parse_semver_major("x.y.z"), None);
        assert_eq!(parse_semver_major("1.2.3.4"), None);
        assert_eq!(parse_semver_major("1..3"), None);
        assert_eq!(parse_semver_major("1.2.3-rc1"), None);
        assert_eq!(parse_semver_major("99999999999999999999.0.0"), None);
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
        use tylertoo_io::test_support::ArrowWriter;
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

    /// Writes a one-row-group file with `rows` rows of `row_bytes` bytes each
    /// in a binary column, and returns its footer.
    fn wide_rows_meta(rows: usize, row_bytes: usize) -> ParquetMetaData {
        use arrow_array::BinaryArray;
        use tylertoo_io::test_support::ArrowWriter;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "geometry",
            DataType::Binary,
            false,
        )]));
        let row = vec![7u8; row_bytes];
        let values: Vec<&[u8]> = (0..rows).map(|_| row.as_slice()).collect();
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(BinaryArray::from(values))])
            .unwrap();
        {
            let file = File::create(tmp.path()).unwrap();
            let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        }
        meta_of(tmp.path())
    }

    #[test]
    fn cluster_read_batch_rows_lowers_the_default_for_wide_rows() {
        // #607: 4 MiB rows put 1024 of them at 4 GiB, past arrow's i32
        // offsets. The owner's cap, which the cluster read now goes
        // through, keeps a batch near 1 GiB: 256 rows.
        let md = wide_rows_meta(4, 4 << 20);
        let rows = tylertoo_io::read_batch_plan(&md, None, 1024, tylertoo_io::byte_array_limit());
        assert!(rows < 1024, "{rows}");
        assert!((rows as u64) * (4 << 20) <= 1 << 31, "{rows}");
    }

    #[test]
    fn cluster_read_batch_rows_keeps_the_default_for_narrow_rows() {
        let md = wide_rows_meta(16, 100);
        assert_eq!(
            tylertoo_io::read_batch_plan(&md, None, 1024, tylertoo_io::byte_array_limit()),
            1024
        );
    }
}
