//! Python bindings for gpq-tiles
//!
//! This module exposes the gpq-tiles-core functionality to Python via pyo3.

use gpq_tiles_core::overview::assign::{
    AssignConfig, DensityBudgetConfig, SortDirection, CLUSTER_POINT_THINNING_DEFAULT,
};
use gpq_tiles_core::overview::check::validate_file;
use gpq_tiles_core::overview::cluster::{AccumulateOp, AccumulateSpec};
use gpq_tiles_core::overview::convert::{
    convert_to_overviews, ClassRanking, ConvertError, ConvertOptions, ConvertReport, LevelPlan,
};
use gpq_tiles_core::overview::export::{export_pmtiles as export_pmtiles_core, ExportOptions};
use gpq_tiles_core::overview::level::Mode;
use gpq_tiles_core::overview::simplify::SimplifyOptions;
use gpq_tiles_core::pipeline::{
    generate_tiles_to_writer, generate_tiles_to_writer_with_progress, ProgressEvent, TilerConfig,
};
use gpq_tiles_core::{Compression, DropDensity, PropertyFilter, StreamingPmtilesWriter};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// Convert a ProgressEvent to a Python dict
fn progress_event_to_dict(py: Python<'_>, event: &ProgressEvent) -> PyResult<Py<PyDict>> {
    let dict = PyDict::new(py);

    match event {
        ProgressEvent::PhaseStart { phase, name } => {
            dict.set_item("phase", "start")?;
            dict.set_item("phase_num", *phase)?;
            dict.set_item("name", *name)?;
        }
        ProgressEvent::Phase1Progress {
            row_group,
            total_row_groups,
            features_in_group,
            records_written,
        } => {
            dict.set_item("phase", "phase1_progress")?;
            dict.set_item("row_group", *row_group)?;
            dict.set_item("total_row_groups", *total_row_groups)?;
            dict.set_item("features_in_group", *features_in_group)?;
            dict.set_item("records_written", *records_written)?;
        }
        ProgressEvent::Phase1Complete {
            total_records,
            peak_memory_bytes,
        } => {
            dict.set_item("phase", "phase1_complete")?;
            dict.set_item("total_records", *total_records)?;
            dict.set_item("peak_memory_bytes", *peak_memory_bytes)?;
        }
        ProgressEvent::Phase2Start => {
            dict.set_item("phase", "phase2_start")?;
        }
        ProgressEvent::Phase2Complete => {
            dict.set_item("phase", "phase2_complete")?;
        }
        ProgressEvent::Phase3Progress {
            tiles_written,
            records_processed,
            total_records,
        } => {
            dict.set_item("phase", "phase3_progress")?;
            dict.set_item("tiles_written", *tiles_written)?;
            dict.set_item("records_processed", *records_processed)?;
            dict.set_item("total_records", *total_records)?;
        }
        ProgressEvent::Complete {
            total_tiles,
            peak_memory_bytes,
            duration_secs,
        } => {
            dict.set_item("phase", "complete")?;
            dict.set_item("total_tiles", *total_tiles)?;
            dict.set_item("peak_memory_bytes", *peak_memory_bytes)?;
            dict.set_item("duration_secs", *duration_secs)?;
        }
    }

    Ok(dict.into())
}

/// Convert GeoParquet to PMTiles
///
/// Args:
///     input (str): Path to input GeoParquet file
///     output (str): Path to output PMTiles file
///     min_zoom (int, optional): Minimum zoom level. Defaults to 0.
///     max_zoom (int, optional): Maximum zoom level. Defaults to 14.
///     drop_density (str, optional): Feature dropping density ("low", "medium", "high"). Defaults to "medium".
///     compression (str, optional): Compression algorithm ("gzip", "brotli", "zstd", "none"). Defaults to "gzip".
///     include (list[str], optional): Whitelist of property names to include. Cannot be used with exclude or exclude_all.
///     exclude (list[str], optional): Blacklist of property names to exclude. Cannot be used with include or exclude_all.
///     exclude_all (bool, optional): Exclude all properties, output geometry only. Defaults to False.
///     layer_name (str, optional): Override the layer name (defaults to input filename stem).
///     deterministic (bool, optional): Enable deterministic (sequential) processing for reproducible output. Defaults to False.
///         When True, disables parallelism to ensure bit-exact reproducibility across runs.
///         Useful for debugging, testing, and compliance workflows. Significantly slower.
///     drop_smallest_as_needed (bool, optional): Enable size-based feature dropping (tippecanoe parity). Defaults to False.
///         When True, features smaller than ``drop_smallest_threshold`` square pixels are dropped
///         when tiles are dense. Useful for building footprints, dense point data, and polygon layers.
///     drop_smallest_threshold (float, optional): Minimum pixel area threshold for smallest-feature dropping. Defaults to 4.0.
///         Only used when ``drop_smallest_as_needed=True``. Features with pixel area below this
///         threshold are candidates for dropping.
///     progress_callback (callable, optional): A callback function that receives progress events as dicts.
///         Each event dict has a "phase" key indicating the event type:
///         - "start": Phase started. Keys: phase_num (int), name (str)
///         - "phase1_progress": Reading row groups. Keys: row_group, total_row_groups, features_in_group, records_written
///         - "phase1_complete": Reading complete. Keys: total_records, peak_memory_bytes
///         - "phase2_start": Sorting started
///         - "phase2_complete": Sorting complete
///         - "phase3_progress": Encoding tiles. Keys: tiles_written, records_processed, total_records
///         - "complete": All done. Keys: total_tiles, peak_memory_bytes, duration_secs
///
/// Returns:
///     None
///
/// Raises:
///     TypeError: If progress_callback is not callable or None
///     ValueError: If invalid parameters or conflicting filter options
///     RuntimeError: If conversion fails
///
/// Example:
///     >>> from gpq_tiles import convert
///     >>> convert("buildings.parquet", "buildings.pmtiles", min_zoom=0, max_zoom=14)
///     >>> convert("buildings.parquet", "buildings.pmtiles", compression="zstd")
///     >>> convert("buildings.parquet", "buildings.pmtiles", include=["name", "height"])
///     >>> convert("buildings.parquet", "buildings.pmtiles", exclude=["internal_id"])
///     >>> convert("buildings.parquet", "buildings.pmtiles", exclude_all=True)
///     >>> convert("buildings.parquet", "buildings.pmtiles", layer_name="my_layer")
///     >>> # With deterministic (sequential) processing
///     >>> convert("buildings.parquet", "buildings.pmtiles", deterministic=True)
///     >>> # Drop smallest features (tippecanoe parity)
///     >>> convert("buildings.parquet", "buildings.pmtiles", drop_smallest_as_needed=True)
///     >>> convert("buildings.parquet", "buildings.pmtiles", drop_smallest_as_needed=True, drop_smallest_threshold=2.0)
///     >>> # With progress callback
///     >>> def on_progress(event):
///     ...     if event["phase"] == "complete":
///     ...         print(f"Generated {event['total_tiles']} tiles")
///     >>> convert("buildings.parquet", "buildings.pmtiles", progress_callback=on_progress)
#[pyfunction]
#[pyo3(signature = (input, output, min_zoom=0, max_zoom=14, drop_density="medium", compression="gzip", include=None, exclude=None, exclude_all=false, layer_name=None, deterministic=false, drop_smallest_as_needed=false, drop_smallest_threshold=4.0, progress_callback=None))]
#[allow(clippy::too_many_arguments)] // Python API mirrors CLI flags; grouping into struct would hurt usability
fn convert(
    py: Python<'_>,
    input: &str,
    output: &str,
    min_zoom: u8,
    max_zoom: u8,
    drop_density: &str,
    compression: &str,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    exclude_all: bool,
    layer_name: Option<String>,
    deterministic: bool,
    drop_smallest_as_needed: bool,
    drop_smallest_threshold: f64,
    progress_callback: Option<Py<PyAny>>,
) -> PyResult<()> {
    // Validate progress_callback is callable if provided
    if let Some(ref cb) = progress_callback {
        if !cb.bind(py).is_callable() {
            return Err(PyErr::new::<pyo3::exceptions::PyTypeError, _>(
                "progress_callback must be callable",
            ));
        }
    }

    // Validate property filter options are mutually exclusive
    let filter_count = include.is_some() as u8 + exclude.is_some() as u8 + exclude_all as u8;
    if filter_count > 1 {
        return Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(
            "Cannot combine include, exclude, and exclude_all. Use only one property filter option.",
        ));
    }

    // Build property filter
    let property_filter = if exclude_all {
        PropertyFilter::ExcludeAll
    } else if let Some(fields) = include {
        PropertyFilter::include(fields)
    } else if let Some(fields) = exclude {
        PropertyFilter::exclude(fields)
    } else {
        PropertyFilter::None
    };

    // Parse drop density
    let drop_density_config = match drop_density.to_lowercase().as_str() {
        "low" => DropDensity::Low,
        "medium" => DropDensity::Medium,
        "high" => DropDensity::High,
        _ => {
            return Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
                "Invalid drop density: '{}'. Valid options: low, medium, high",
                drop_density
            )))
        }
    };

    // Parse compression
    let compression_config = Compression::from_str(compression).ok_or_else(|| {
        PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
            "Invalid compression: '{}'. Valid options: none, gzip, brotli, zstd",
            compression
        ))
    })?;

    // Derive layer name from input filename if not provided
    let input_path = Path::new(input);
    let output_path_str = output.to_string();
    let layer_name_str = layer_name.unwrap_or_else(|| {
        input_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("layer")
            .to_string()
    });

    // Build TilerConfig
    let mut config = TilerConfig::new(min_zoom, max_zoom)
        .with_extent(4096)
        .with_layer_name(&layer_name_str)
        .with_density_drop(matches!(
            drop_density_config,
            DropDensity::Medium | DropDensity::High
        ))
        .with_density_max_per_cell(match drop_density_config {
            DropDensity::Low => 10,
            DropDensity::Medium => 3,
            DropDensity::High => 1,
        })
        .with_property_filter(property_filter)
        .with_quiet(true) // Suppress progress output in Python
        .with_deterministic(deterministic);

    if drop_smallest_as_needed {
        config = config
            .with_drop_smallest_as_needed()
            .with_drop_smallest_threshold(drop_smallest_threshold);
    }

    // Create streaming writer
    let mut writer = StreamingPmtilesWriter::new(compression_config).map_err(|e| {
        PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
            "Failed to create PMTiles writer: {}",
            e
        ))
    })?;

    // Run conversion with or without progress callback
    // Release the GIL while running the Rust code using py.detach()
    if let Some(callback) = progress_callback {
        // Wrap the Python callback in Arc for Sync requirement
        // Py<PyAny> is Send, but not Sync. Arc<Py<PyAny>> + with attach makes it work.
        let callback_arc: Arc<Py<PyAny>> = Arc::new(callback);

        // Create a Rust closure that captures the Arc'd callback
        let rust_progress_callback: gpq_tiles_core::ProgressCallback =
            Box::new(move |event: ProgressEvent| {
                let callback = Arc::clone(&callback_arc);

                // Acquire the GIL to call into Python (PyO3 0.29 uses Python::attach)
                Python::attach(|py| {
                    // Convert event to Python dict
                    match progress_event_to_dict(py, &event) {
                        Ok(dict) => {
                            // Call the Python callback with the dict
                            if let Err(e) = callback.call1(py, (dict,)) {
                                // Log error but don't panic - progress callbacks shouldn't abort the operation
                                eprintln!("Error in progress callback: {}", e);
                            }
                        }
                        Err(e) => {
                            eprintln!("Error converting progress event to dict: {}", e);
                        }
                    }
                });
            });

        // Release the GIL while running the Rust code (PyO3 0.29 uses py.detach())
        py.detach(|| {
            generate_tiles_to_writer_with_progress(
                input_path,
                &config,
                &mut writer,
                rust_progress_callback,
            )
            .map_err(|e| {
                PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                    "Conversion failed: {}",
                    e
                ))
            })
        })?;
    } else {
        // No callback - use generate_tiles_to_writer
        py.detach(|| {
            generate_tiles_to_writer(input_path, &config, &mut writer).map_err(|e| {
                PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                    "Conversion failed: {}",
                    e
                ))
            })
        })?;
    }

    // Finalize and write the PMTiles file
    py.detach(|| {
        writer.finalize(Path::new(&output_path_str)).map_err(|e| {
            PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                "Failed to write PMTiles file: {}",
                e
            ))
        })
    })?;

    Ok(())
}

/// Map a [`ConvertError`] to the Python exception type it deserves:
/// user-input problems (bad options, missing/mistyped columns, invalid level
/// plans) become `ValueError`; everything else (I/O, decode, writer) becomes
/// `RuntimeError`.
fn convert_error_to_py(e: ConvertError) -> PyErr {
    match e {
        ConvertError::InvalidLevels(_)
        | ConvertError::RankingConflict
        | ConvertError::ClusterPartitioningUnsupported
        | ConvertError::AccumulateWithoutCluster
        | ConvertError::SortKeyColumnMissing { .. }
        | ConvertError::ClassRankColumnMissing { .. }
        | ConvertError::ClassRankColumnNotString { .. }
        | ConvertError::AccumulateColumnMissing { .. }
        | ConvertError::AccumulateColumnNotNumeric { .. } => {
            PyErr::new::<PyValueError, _>(format!("{}", e))
        }
        other => PyErr::new::<PyRuntimeError, _>(format!("overview conversion failed: {}", other)),
    }
}

/// Convert a [`ConvertReport`] to a Python dict.
fn convert_report_to_dict(py: Python<'_>, report: &ConvertReport) -> PyResult<Py<PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item(
        "mode",
        match report.mode {
            Mode::Duplicating => "duplicating",
            Mode::Partitioning => "partitioning",
        },
    )?;
    let levels = PyList::empty(py);
    for lvl in &report.levels {
        let d = PyDict::new(py);
        d.set_item("level", lvl.level)?;
        d.set_item("gsd", lvl.gsd)?;
        d.set_item("zoom", lvl.zoom)?;
        d.set_item("feature_count", lvl.feature_count)?;
        d.set_item("vertex_count", lvl.vertex_count)?;
        d.set_item("uncompressed_bytes", lvl.uncompressed_bytes)?;
        d.set_item("compressed_bytes", lvl.compressed_bytes)?;
        levels.append(d)?;
    }
    dict.set_item("levels", levels)?;
    dict.set_item("input_features", report.input_features)?;
    dict.set_item("total_rows", report.total_rows)?;
    dict.set_item("total_vertices", report.total_vertices)?;
    dict.set_item("total_compressed_bytes", report.total_compressed_bytes)?;
    dict.set_item("duration_secs", report.duration_secs)?;
    Ok(dict.into())
}

/// Build a multi-resolution GeoParquet overview file (COG-style vector overviews).
///
/// This is the Python equivalent of ``gpq-tiles overview`` with the full CLI
/// knob surface and identical defaults. The pipeline reads a (gpio-sorted)
/// GeoParquet file, thins features per level with grid cell-winner selection,
/// applies the per-level density budget, simplifies geometry in world space,
/// and writes a level-banded GeoParquet file validated by ``validate()`` and
/// exportable to PMTiles by ``export_pmtiles()``.
///
/// Args:
///     input (str): Input GeoParquet file (EPSG:4326 or EPSG:3857).
///     output (str): Output overview GeoParquet file.
///     mode (str, optional): Level materialization mode, "duplicating" (each
///         level is a self-contained rendering) or "partitioning" (each
///         feature appears once at its coarsest level; prefix reads).
///         Defaults to "duplicating".
///     min_zoom (int, optional): Coarsest Web Mercator zoom of the level
///         range. Defaults to 0.
///     max_zoom (int, optional): Finest (canonical) Web Mercator zoom of the
///         level range. Defaults to 6.
///     gsds (list[float], optional): Explicit per-level GSD list in meters,
///         strictly decreasing coarse-to-fine. Overrides min_zoom/max_zoom.
///     gsd_base (float, optional): GSD tile-band base for the zoom-to-GSD
///         mapping: gsd(z) = 40075016.69 / base / 2^z. Larger = finer (denser)
///         levels, smaller = coarser. No effect with explicit gsds.
///         Defaults to 1024.0.
///     sort_key (str, optional): Numeric column used as the cell-winner
///         priority key (higher wins by default; see sort_direction).
///         Mutually exclusive with class_rank_column.
///     sort_direction (str, optional): "desc" (larger sort_key wins, default)
///         or "asc" (smaller wins, e.g. rank columns where 1 is best).
///     class_rank_column (str, optional): String column carrying categorical
///         classes for cell-winner ranking. Requires class_ranks. Mutually
///         exclusive with sort_key.
///     class_ranks (dict[str, float], optional): Map of class value to
///         priority; higher priority wins a cell. Present-but-unlisted values
///         rank below every listed value (but above nulls) unless
///         class_rank_unknown overrides that.
///     class_rank_unknown (float, optional): Priority for present-but-unlisted
///         class values. Defaults to min(class_ranks.values()) - 1.
///     no_auto_rank (bool, optional): Disable auto-detection of well-known
///         schemas (Overture roads class/road_class, Overture places
///         confidence). Defaults to False.
///     simplify_factor (float, optional): RDP tolerance = factor * gsd
///         (duplicating mode only). Lower = crisper but heavier levels;
///         higher = cruder and lighter. Defaults to 1.0.
///     collapse (bool, optional): Collapse below-visibility polygons to a
///         representative point instead of dropping them. Defaults to False.
///     point_thinning (float, optional): Point thinning grid factor (cell
///         size = factor * gsd). Defaults to 4.0, or 16.0 when cluster=True
///         (absorbed points are summarized rather than dropped).
///     line_thinning (float, optional): Line thinning grid factor.
///         Defaults to 1.0.
///     polygon_thinning (float, optional): Polygon thinning grid factor.
///         Defaults to 1.0.
///     line_visibility (float, optional): A line is eligible at a level only
///         if its bbox diagonal >= factor * gsd. Defaults to 2.0.
///     polygon_visibility (float, optional): Same gate for polygons.
///         Defaults to 4.0.
///     drop_rate (float, optional): Per-level density budget drop rate: each
///         coarser level keeps 1/rate of the next finer level's budget.
///         Defaults to 1.65.
///     drop_gamma (float, optional): Spatial-fairness strength for the
///         density budget (1.0 = proportional cut; larger protects sparse
///         neighborhoods). Defaults to 1.5.
///     density_drop (bool, optional): Master switch for the per-level density
///         budget. Defaults to True.
///     cluster (bool, optional): Enable point clustering (duplicating mode
///         only): the surviving point per grid cell absorbs the other points
///         in its cell and the output gains a point_count INT64 column.
///         Defaults to False.
///     accumulate_attributes (dict[str, str], optional): Numeric per-cluster
///         attribute aggregation, mapping column name to operator ("sum",
///         "max", "min", "mean"). Requires cluster=True.
///     coalesce_lines (bool, optional): Chain touching same-class line
///         segments into single "stroke" LineStrings at non-canonical levels;
///         the output gains a coalesced_count INT32 column. Defaults to True.
///         Inert in partitioning mode (feature-once/verbatim contract).
///     coalesce_snap (float, optional): Endpoint snap tolerance in GSD
///         multiples; <= 0 requires exact endpoint matches. Defaults to 1.0.
///     coalesce_junction_angle (float, optional): Junction continuation
///         threshold in degrees; 0 disables (junctions terminate chains).
///         Defaults to 0.0.
///     coalesce_max_level_rows (int, optional): Per-level candidate-line
///         ceiling (memory guard); larger levels skip coalescing with a log.
///         Defaults to 2_000_000.
///     cogp_compat (bool, optional): Emit the optional COGP compatibility
///         footer key. Defaults to False.
///     row_group_size (int, optional): Maximum output row-group size in rows
///         (interpreted per level). Defaults to 10_000.
///     full_column_stats (bool, optional): Keep full Parquet statistics on
///         every column instead of suppressing high-cardinality property and
///         geometry stats. Defaults to False.
///     streaming (bool, optional): Use the two-pass bounded-memory streaming
///         pipeline. Defaults to True.
///     read_batch_size (int, optional): Rows per Arrow read batch in the
///         streaming pipeline. Defaults to 8192.
///
/// Returns:
///     dict: Conversion report with keys "mode", "levels" (list of dicts with
///     "level", "gsd", "zoom", "feature_count", "vertex_count",
///     "uncompressed_bytes", "compressed_bytes"), "input_features",
///     "total_rows", "total_vertices", "total_compressed_bytes",
///     "duration_secs".
///
/// Raises:
///     ValueError: Invalid options (bad mode/direction/op, conflicting or
///         incomplete ranking options, invalid level plan, missing or
///         mistyped columns).
///     RuntimeError: The conversion itself failed (I/O, decode, unsupported
///         CRS, writer errors).
///
/// Example:
///     >>> from gpq_tiles import overview
///     >>> report = overview("moldova.parquet", "moldova-overviews.parquet",
///     ...                   min_zoom=0, max_zoom=10)
///     >>> report = overview("nyc-trees.parquet", "nyc-trees-overviews.parquet",
///     ...                   max_zoom=12, cluster=True,
///     ...                   accumulate_attributes={"count": "sum"})
#[pyfunction]
#[pyo3(signature = (
    input,
    output,
    *,
    mode="duplicating",
    min_zoom=0,
    max_zoom=6,
    gsds=None,
    gsd_base=1024.0,
    sort_key=None,
    sort_direction="desc",
    class_rank_column=None,
    class_ranks=None,
    class_rank_unknown=None,
    no_auto_rank=false,
    simplify_factor=1.0,
    collapse=false,
    point_thinning=None,
    line_thinning=1.0,
    polygon_thinning=1.0,
    line_visibility=2.0,
    polygon_visibility=4.0,
    drop_rate=1.65,
    drop_gamma=1.5,
    density_drop=true,
    cluster=false,
    accumulate_attributes=None,
    coalesce_lines=true,
    coalesce_snap=1.0,
    coalesce_junction_angle=0.0,
    coalesce_max_level_rows=2_000_000,
    cogp_compat=false,
    row_group_size=10_000,
    full_column_stats=false,
    streaming=true,
    read_batch_size=8192,
))]
#[allow(clippy::too_many_arguments)] // Python API mirrors CLI flags; grouping into struct would hurt usability
fn overview(
    py: Python<'_>,
    input: &str,
    output: &str,
    mode: &str,
    min_zoom: u8,
    max_zoom: u8,
    gsds: Option<Vec<f64>>,
    gsd_base: f64,
    sort_key: Option<String>,
    sort_direction: &str,
    class_rank_column: Option<String>,
    class_ranks: Option<BTreeMap<String, f64>>,
    class_rank_unknown: Option<f64>,
    no_auto_rank: bool,
    simplify_factor: f64,
    collapse: bool,
    point_thinning: Option<f64>,
    line_thinning: f64,
    polygon_thinning: f64,
    line_visibility: f64,
    polygon_visibility: f64,
    drop_rate: f64,
    drop_gamma: f64,
    density_drop: bool,
    cluster: bool,
    accumulate_attributes: Option<BTreeMap<String, String>>,
    coalesce_lines: bool,
    coalesce_snap: f64,
    coalesce_junction_angle: f64,
    coalesce_max_level_rows: usize,
    cogp_compat: bool,
    row_group_size: usize,
    full_column_stats: bool,
    streaming: bool,
    read_batch_size: usize,
) -> PyResult<Py<PyDict>> {
    let mode = match mode {
        "duplicating" => Mode::Duplicating,
        "partitioning" => Mode::Partitioning,
        other => {
            return Err(PyErr::new::<PyValueError, _>(format!(
                "Invalid mode: '{}'. Valid options: duplicating, partitioning",
                other
            )))
        }
    };

    let sort_direction = match sort_direction.to_lowercase().as_str() {
        "desc" => SortDirection::Desc,
        "asc" => SortDirection::Asc,
        other => {
            return Err(PyErr::new::<PyValueError, _>(format!(
                "Invalid sort_direction: '{}'. Valid options: desc, asc",
                other
            )))
        }
    };

    // Explicit GSD list overrides the zoom range (like the CLI's --gsd).
    let levels = match gsds {
        Some(gsds) => LevelPlan::Gsds(gsds),
        None => LevelPlan::ZoomRange { min_zoom, max_zoom },
    };

    // Class ranking: column and ranks must be supplied together; the unknown
    // rank defaults to min(ranks) - 1 so unlisted values lose to every listed
    // class but still beat nulls (mirrors the CLI's --class-rank parsing).
    if sort_key.is_some() && class_rank_column.is_some() {
        return Err(PyErr::new::<PyValueError, _>(
            "sort_key and class_rank_column are mutually exclusive; supply at most one",
        ));
    }
    let class_ranking = match (class_rank_column, class_ranks) {
        (None, None) => None,
        (Some(_), None) | (None, Some(_)) => {
            return Err(PyErr::new::<PyValueError, _>(
                "class_rank_column and class_ranks must be supplied together",
            ))
        }
        (Some(column), Some(ranks)) => {
            if ranks.is_empty() {
                return Err(PyErr::new::<PyValueError, _>(
                    "class_ranks must contain at least one value: rank entry",
                ));
            }
            let min_rank = ranks.values().copied().fold(f64::INFINITY, f64::min);
            Some(ClassRanking {
                column,
                ranks: ranks.into_iter().collect(),
                unknown_rank: class_rank_unknown.unwrap_or(min_rank - 1.0),
            })
        }
    };

    // Clustering flags (mirrors the CLI's pre-checks; also enforced in core).
    let accumulate_attributes = accumulate_attributes.unwrap_or_default();
    if !accumulate_attributes.is_empty() && !cluster {
        return Err(PyErr::new::<PyValueError, _>(
            "accumulate_attributes requires cluster=True",
        ));
    }
    if cluster && mode == Mode::Partitioning {
        return Err(PyErr::new::<PyValueError, _>(
            "cluster=True requires mode=\"duplicating\": a partitioning-mode feature has \
             one row read across many zoom prefixes, so a per-level point_count cannot \
             be represented without double counting",
        ));
    }
    let accumulate = accumulate_attributes
        .into_iter()
        .map(|(column, op)| {
            let op = AccumulateOp::parse(&op).ok_or_else(|| {
                PyErr::new::<PyValueError, _>(format!(
                    "Invalid accumulate op '{}' for column '{}'. Valid ops: sum, max, min, mean",
                    op, column
                ))
            })?;
            Ok(AccumulateSpec { column, op })
        })
        .collect::<PyResult<Vec<_>>>()?;

    // Cluster-conditional default: with cluster=True, absorbed points are
    // summarized (point_count), so the sparser 16.0 grid is the better look.
    let point_thinning = point_thinning.unwrap_or(if cluster {
        CLUSTER_POINT_THINNING_DEFAULT
    } else {
        AssignConfig::default().point_thinning
    });

    let options = ConvertOptions {
        mode,
        levels,
        assign: AssignConfig {
            point_thinning,
            line_thinning,
            polygon_thinning,
            line_visibility,
            polygon_visibility,
            sort_direction,
        },
        sort_key,
        class_ranking,
        no_auto_rank,
        simplify: SimplifyOptions {
            factor: simplify_factor,
            collapse,
        },
        density: DensityBudgetConfig {
            enabled: density_drop,
            drop_rate,
            gamma: drop_gamma,
        },
        gsd_base,
        cogp_compat_key: cogp_compat,
        max_row_group_size: row_group_size,
        full_column_stats,
        streaming,
        read_batch_size,
        cluster,
        accumulate,
        coalesce_lines,
        coalesce_snap,
        coalesce_max_level_rows,
        coalesce_junction_angle,
    };

    let input_path = Path::new(input).to_path_buf();
    let output_path = Path::new(output).to_path_buf();

    // Release the GIL while the Rust pipeline runs.
    let report = py
        .detach(|| convert_to_overviews(&input_path, &output_path, &options))
        .map_err(convert_error_to_py)?;

    convert_report_to_dict(py, &report)
}

/// Export an overview GeoParquet file to a PMTiles archive.
///
/// Python equivalent of ``gpq-tiles export-pmtiles``: each overview level
/// becomes one Web Mercator zoom of MVT tiles (gzip-compressed).
///
/// Args:
///     input (str): Input overview GeoParquet file (produced by ``overview()``).
///     output (str): Output PMTiles archive.
///     layer_name (str, optional): MVT layer name written into every tile and
///         the archive metadata. Defaults to "overview".
///     tile_buffer (int, optional): Per-tile edge buffer in tile pixels
///         (feature seam continuity). Defaults to 8.
///     extent (int, optional): MVT tile extent (tile-local resolution).
///         Defaults to 4096.
///     tile_size_limit (int, optional): Per-tile MVT size limit in bytes.
///         A tile exceeding it sheds its lowest-priority (smallest) features
///         in a single non-iterative drop pass. Defaults to None (no limit).
///
/// Returns:
///     dict: Export report with keys "mode", "min_zoom", "max_zoom", "zooms"
///     (list of dicts with "zoom", "level", "level_feature_count",
///     "tile_count", "tile_feature_count", "oversized_tiles"), "total_tiles",
///     "total_tile_features", "oversized_tiles", "duration_secs".
///
/// Raises:
///     RuntimeError: The export failed (not an overview file, unsupported
///         CRS, I/O errors).
///
/// Example:
///     >>> from gpq_tiles import export_pmtiles
///     >>> report = export_pmtiles("moldova-overviews.parquet", "moldova.pmtiles",
///     ...                         layer_name="admin")
///     >>> print(report["total_tiles"])
#[pyfunction]
#[pyo3(signature = (input, output, *, layer_name="overview", tile_buffer=8, extent=4096, tile_size_limit=None))]
fn export_pmtiles(
    py: Python<'_>,
    input: &str,
    output: &str,
    layer_name: &str,
    tile_buffer: u32,
    extent: u32,
    tile_size_limit: Option<usize>,
) -> PyResult<Py<PyDict>> {
    let options = ExportOptions {
        layer_name: layer_name.to_string(),
        tile_buffer,
        extent,
        tile_size_limit,
    };
    let input_path = Path::new(input).to_path_buf();
    let output_path = Path::new(output).to_path_buf();

    let report = py
        .detach(|| export_pmtiles_core(&input_path, &output_path, &options))
        .map_err(|e| PyErr::new::<PyRuntimeError, _>(format!("export failed: {}", e)))?;

    let dict = PyDict::new(py);
    dict.set_item("mode", &report.mode)?;
    dict.set_item("min_zoom", report.min_zoom)?;
    dict.set_item("max_zoom", report.max_zoom)?;
    let zooms = PyList::empty(py);
    for z in &report.zooms {
        let d = PyDict::new(py);
        d.set_item("zoom", z.zoom)?;
        d.set_item("level", z.level)?;
        d.set_item("level_feature_count", z.level_feature_count)?;
        d.set_item("tile_count", z.tile_count)?;
        d.set_item("tile_feature_count", z.tile_feature_count)?;
        d.set_item("oversized_tiles", z.oversized_tiles)?;
        zooms.append(d)?;
    }
    dict.set_item("zooms", zooms)?;
    dict.set_item("total_tiles", report.total_tiles)?;
    dict.set_item("total_tile_features", report.total_tile_features)?;
    dict.set_item("oversized_tiles", report.oversized_tiles)?;
    dict.set_item("duration_secs", report.duration_secs)?;
    Ok(dict.into())
}

/// Validate a GeoParquet overview file against the overviews spec checklist.
///
/// Python equivalent of ``gpq-tiles validate``: runs every structural
/// conformance check (footer metadata, level column, row-group banding, bbox
/// covering, provenance blocks, ...) and returns the structured results
/// instead of raising on failure.
///
/// Args:
///     file (str): GeoParquet overview file to validate.
///
/// Returns:
///     dict: ``{"valid": bool, "checks": [{"name": str, "passed": bool,
///     "message": str}, ...]}`` where "valid" is True iff every check passed.
///
/// Raises:
///     RuntimeError: The file could not be opened or its Parquet footer could
///         not be parsed (validation never started).
///
/// Example:
///     >>> from gpq_tiles import validate
///     >>> result = validate("moldova-overviews.parquet")
///     >>> assert result["valid"], [c for c in result["checks"] if not c["passed"]]
#[pyfunction]
#[pyo3(signature = (file))]
fn validate(py: Python<'_>, file: &str) -> PyResult<Py<PyDict>> {
    let path = Path::new(file).to_path_buf();
    let report = py.detach(|| validate_file(&path)).map_err(|e| {
        PyErr::new::<PyRuntimeError, _>(format!("could not open '{}': {}", file, e))
    })?;

    let dict = PyDict::new(py);
    dict.set_item("valid", report.is_valid())?;
    let checks = PyList::empty(py);
    for check in &report.checks {
        let d = PyDict::new(py);
        d.set_item("name", &check.name)?;
        d.set_item("passed", check.passed)?;
        d.set_item("message", &check.message)?;
        checks.append(d)?;
    }
    dict.set_item("checks", checks)?;
    Ok(dict.into())
}

/// gpq_tiles: Fast GeoParquet to PMTiles converter
///
/// This module provides Python bindings for the gpq-tiles Rust library.
#[pymodule]
fn gpq_tiles(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    m.add_function(wrap_pyfunction!(overview, m)?)?;
    m.add_function(wrap_pyfunction!(export_pmtiles, m)?)?;
    m.add_function(wrap_pyfunction!(validate, m)?)?;
    Ok(())
}
