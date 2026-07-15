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
use gpq_tiles_core::overview::level::{MemoryProfile, Mode};
use gpq_tiles_core::overview::simplify::SimplifyOptions;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::collections::BTreeMap;
use std::path::Path;

/// Convert GeoParquet to PMTiles in one shot (overview facade).
///
/// .. deprecated::
///     ``convert()`` no longer runs the removed legacy per-tile pipeline.
///     It is now a thin facade that chains ``overview()`` (convert, with
///     default knobs) into a temporary GeoParquet file and then
///     ``export_pmtiles()`` to the requested output. For control over
///     generalization quality (ranking, clustering, coalescing, thinning),
///     call ``overview()`` and ``export_pmtiles()`` directly.
///
/// The legacy keyword arguments ``drop_density``, ``compression``,
/// ``include``, ``exclude``, ``exclude_all``, ``deterministic``,
/// ``drop_smallest_as_needed``, ``drop_smallest_threshold`` and
/// ``progress_callback`` were removed with the legacy pipeline; passing
/// them raises ``TypeError``.
///
/// Args:
///     input (str): Path to input GeoParquet file (EPSG:4326 or EPSG:3857),
///         or a remote URL (``s3://``, ``https://``, ``gs://``) read via
///         byte-range requests.
///     output (str): Path to output PMTiles file.
///     min_zoom (int, optional): Minimum (coarsest) zoom level. Defaults to 0.
///     max_zoom (int, optional): Maximum (finest) zoom level. Defaults to 14.
///     layer_name (str, optional): Override the MVT layer name (defaults to
///         the input filename stem).
///     tile_size_limit (int, optional): Per-tile MVT size limit in bytes.
///         A tile exceeding it sheds its lowest-priority features. Defaults
///         to None (no limit).
///     simple_clip_fastpath (bool, optional): Skip the i_overlay boundary-bridge
///         fallback for features whose rings are already simple (issue #239).
///         Faster fine-zoom polygon export; output is render-equivalent on
///         simple rings but stores them rotated to a different start vertex.
///         Defaults to True; set False for byte-stable tile output.
///
/// Returns:
///     None
///
/// Raises:
///     ValueError: Invalid zoom range or conversion options.
///     RuntimeError: The conversion or export failed.
///
/// Example:
///     >>> from gpq_tiles import convert
///     >>> convert("buildings.parquet", "buildings.pmtiles", min_zoom=0, max_zoom=14)
///     >>> convert("buildings.parquet", "buildings.pmtiles", layer_name="my_layer")
#[pyfunction]
#[pyo3(signature = (input, output, min_zoom=0, max_zoom=14, layer_name=None, tile_size_limit=None, simple_clip_fastpath=true))]
#[allow(clippy::too_many_arguments)] // mirrors the Python kwarg signature
fn convert(
    py: Python<'_>,
    input: &str,
    output: &str,
    min_zoom: u8,
    max_zoom: u8,
    layer_name: Option<String>,
    tile_size_limit: Option<usize>,
    simple_clip_fastpath: bool,
) -> PyResult<()> {
    let input_path = Path::new(input).to_path_buf();
    let output_path = Path::new(output).to_path_buf();

    // Derive layer name from input filename if not provided.
    let layer_name = layer_name.unwrap_or_else(|| {
        input_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("layer")
            .to_string()
    });

    let options = ConvertOptions {
        levels: LevelPlan::ZoomRange { min_zoom, max_zoom },
        ..ConvertOptions::default()
    };

    let export_options = ExportOptions {
        layer_name,
        tile_buffer: 8,
        extent: 4096,
        tile_size_limit,
        simple_clip_fastpath,
    };

    // Intermediate overview file next to the output (same filesystem);
    // NamedTempFile removes it on drop — success or failure alike.
    let tmp_dir = output_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let overview_tmp = tempfile::Builder::new()
        .prefix(".gpq-tiles-overview-")
        .suffix(".parquet")
        .tempfile_in(&tmp_dir)
        .map_err(|e| {
            PyErr::new::<PyRuntimeError, _>(format!(
                "failed to create temporary overview file: {}",
                e
            ))
        })?;

    // Release the GIL while the Rust pipelines run.
    py.detach(|| convert_to_overviews(&input_path, overview_tmp.path(), &options))
        .map_err(convert_error_to_py)?;
    py.detach(|| export_pmtiles_core(overview_tmp.path(), &output_path, &export_options))
        .map_err(|e| PyErr::new::<PyRuntimeError, _>(format!("export failed: {}", e)))?;

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
    let skipped = PyList::empty(py);
    for s in &report.skipped_empty_levels {
        let d = PyDict::new(py);
        d.set_item("planned_level", s.planned_level)?;
        d.set_item("gsd", s.gsd)?;
        d.set_item("zoom", s.zoom)?;
        skipped.append(d)?;
    }
    dict.set_item("skipped_empty_levels", skipped)?;
    dict.set_item("input_features", report.input_features)?;
    dict.set_item("total_rows", report.total_rows)?;
    dict.set_item("total_vertices", report.total_vertices)?;
    dict.set_item("total_compressed_bytes", report.total_compressed_bytes)?;
    dict.set_item("row_groups_total", report.row_groups_total)?;
    dict.set_item("row_groups_read", report.row_groups_read)?;
    dict.set_item("duration_secs", report.duration_secs)?;
    // Remote-input fetch counters (#210); None for local inputs.
    match &report.remote_fetch {
        Some(stats) => {
            let rf = PyDict::new(py);
            rf.set_item("requests", stats.requests)?;
            rf.set_item("bytes_fetched", stats.bytes_fetched)?;
            rf.set_item("object_size", stats.object_size)?;
            dict.set_item("remote_fetch", rf)?;
        }
        None => dict.set_item("remote_fetch", py.None())?,
    }
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
///     input (str): Input GeoParquet file (EPSG:4326 or EPSG:3857): a local
///         path or a remote URL (``s3://``, ``https://``, ``gs://``) read via
///         byte-range requests. With ``bbox``, only the matching row groups
///         of a remote input are ever downloaded.
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
///     cascade (bool, optional): Cascading simplification (duplicating mode
///         only): simplify each coarser level from the next-finer level's
///         already-simplified output (tippecanoe-style) and repair invalid
///         RDP candidates via a boolean overlay instead of epsilon-retrying.
///         Much faster; coarse-level coordinates differ slightly from the
///         non-cascaded pipeline. Set False to reproduce pre-cascade output
///         byte-for-byte. Defaults to True.
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
///         Defaults to 2.0 (retuned from 4.0 in the #259 coarse-zoom
///         sweep; see corpus/SWEEPS.md Decision 6).
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
///     bbox (tuple, optional): Regional extract as ``(xmin, ymin, xmax,
///         ymax)`` in EPSG:4326 lon/lat degrees. Only features whose bbox
///         intersects the region are converted; input row groups whose bbox
///         covering statistics don't intersect are skipped without reading
///         their data pages (inputs without covering stats read everything
///         and rely on the exact per-feature filter). Defaults to None
///         (full extent).
///     profile (str, optional): Memory/throughput profile for the single-read
///         pass-2 engine: "speed" (buffer output in RAM), "bounded" (spill to
///         temporary Arrow IPC files), or "auto" (pick per mode + estimated
///         size). Output is byte-identical across profiles. Defaults to "auto".
///     in_flight_batches (int, optional): Read batches allowed in flight through
///         the pass-2 pipeline at once (read/compute-overlap knob). Higher
///         improves core utilization at proportionally more peak memory.
///         Defaults to 4.
///
/// Returns:
///     dict: Conversion report with keys "mode", "levels" (list of dicts with
///     "level", "gsd", "zoom", "feature_count", "vertex_count",
///     "uncompressed_bytes", "compressed_bytes"), "skipped_empty_levels"
///     (list of dicts with "planned_level", "gsd", "zoom": planned levels
///     omitted because no feature is visible at their scale — the written
///     pyramid is auto-clamped to the non-empty levels), "input_features",
///     "total_rows", "total_vertices", "total_compressed_bytes",
///     "row_groups_total", "row_groups_read", "duration_secs", and
///     "remote_fetch" (None for local inputs; for remote URLs a dict with
///     "requests", "bytes_fetched", "object_size").
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
    cascade=true,
    point_thinning=None,
    line_thinning=1.0,
    polygon_thinning=1.0,
    line_visibility=2.0,
    polygon_visibility=2.0,
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
    bbox=None,
    profile="auto",
    in_flight_batches=4,
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
    cascade: bool,
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
    bbox: Option<(f64, f64, f64, f64)>,
    profile: &str,
    in_flight_batches: usize,
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

    let profile = match profile {
        "auto" => MemoryProfile::Auto,
        "speed" => MemoryProfile::Speed,
        "bounded" => MemoryProfile::Bounded,
        other => {
            return Err(PyErr::new::<PyValueError, _>(format!(
                "Invalid profile: '{}'. Valid options: auto, speed, bounded",
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
            cascade,
        },
        density: DensityBudgetConfig {
            enabled: density_drop,
            drop_rate,
            gamma: drop_gamma,
        },
        gsd_base,
        cogp_compat_key: cogp_compat,
        max_row_group_size: row_group_size,
        row_group_size_policy: Default::default(),
        full_column_stats,
        streaming,
        read_batch_size,
        profile,
        in_flight_batches,
        cluster,
        accumulate,
        coalesce_lines,
        coalesce_snap,
        coalesce_max_level_rows,
        coalesce_junction_angle,
        bbox: bbox.map(|(xmin, ymin, xmax, ymax)| [xmin, ymin, xmax, ymax]),
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
///     simple_clip_fastpath (bool, optional): Skip the i_overlay boundary-bridge
///         fallback for features whose rings are already simple (issue #239).
///         Faster fine-zoom polygon export; output is render-equivalent on
///         simple rings but stores them rotated to a different start vertex.
///         Defaults to True; set False for byte-stable tile output.
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
#[pyo3(signature = (input, output, *, layer_name="overview", tile_buffer=8, extent=4096, tile_size_limit=None, simple_clip_fastpath=true))]
#[allow(clippy::too_many_arguments)] // mirrors the Python kwarg signature
fn export_pmtiles(
    py: Python<'_>,
    input: &str,
    output: &str,
    layer_name: &str,
    tile_buffer: u32,
    extent: u32,
    tile_size_limit: Option<usize>,
    simple_clip_fastpath: bool,
) -> PyResult<Py<PyDict>> {
    let options = ExportOptions {
        layer_name: layer_name.to_string(),
        tile_buffer,
        extent,
        tile_size_limit,
        simple_clip_fastpath,
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
