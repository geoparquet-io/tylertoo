//! Python bindings for tylertoo
//!
//! This module exposes the tylertoo-core functionality to Python via pyo3.

use pyo3::exceptions::{PyMemoryError, PyRuntimeError, PyTypeError, PyUserWarning, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tylertoo_core::input_set::ConvertSource;
use tylertoo_core::overview::assign::{
    AssignConfig, DensityBudgetConfig, SortDirection, CLUSTER_POINT_THINNING_DEFAULT,
};
use tylertoo_core::overview::auto_zoom::MaxZoom;
use tylertoo_core::overview::check::validate_file;
use tylertoo_core::overview::cluster::{AccumulateOp, AccumulateSpec};
use tylertoo_core::overview::convert::{
    convert_to_overviews, convert_to_overviews_sources, parse_representation_spec, ClassRanking,
    ConvertError, ConvertOptions, ConvertReport, LevelPlan,
};
use tylertoo_core::overview::export::{
    export_pmtiles as export_pmtiles_core, ExportOptions, ExportReport, FeatureOrder,
};
use tylertoo_core::overview::ladder::{EntryZoomKind, EntryZoomSpec};
use tylertoo_core::overview::level::{MemoryProfile, Mode};
use tylertoo_core::overview::simplify::{CollapseMode, SimplifyOptions};
use tylertoo_core::overview::tiles::{
    convert_to_pmtiles, TilesError, TilesInput, TilesOptions, TilesOutcome,
};

/// The installed log bridge's cache handle (#445), set once at import.
static LOG_RESET: OnceLock<pyo3_log::ResetHandle> = OnceLock::new();

/// Drop pyo3-log's cached Python loggers and levels, so a `logging`
/// configuration change made since the last call (`basicConfig`,
/// `setLevel`, pytest's `caplog`) applies to this one.
///
/// The cache is what lets a filtered-out record on a worker thread skip the
/// GIL entirely; resetting it once per call keeps that, at the cost of one
/// atomic store. Every entry point calls this first.
fn refresh_log_levels() {
    if let Some(handle) = LOG_RESET.get() {
        handle.reset();
    }
}

/// Parse the `feature_order` kwarg into the core enum, surfacing a bad
/// spelling as a Python `ValueError` rather than a panic (#361).
fn parse_feature_order(s: &str) -> PyResult<FeatureOrder> {
    s.parse::<FeatureOrder>()
        .map_err(pyo3::exceptions::PyValueError::new_err)
}

/// Extract the `max_zoom` kwarg (#444): an `int` zoom, or the string
/// `"auto"` (any case). Parsing and resolution belong to core
/// ([`MaxZoom`]); this only maps Python types onto it with precise errors —
/// an out-of-range int is a `ValueError`, not pyo3's generic extraction
/// failure.
fn extract_max_zoom(obj: &Bound<'_, PyAny>) -> PyResult<MaxZoom> {
    if let Ok(s) = obj.extract::<String>() {
        return match s.parse::<MaxZoom>() {
            Ok(MaxZoom::Auto) => Ok(MaxZoom::Auto),
            _ => Err(PyValueError::new_err(format!(
                "max_zoom must be an int or \"auto\", got {s:?}"
            ))),
        };
    }
    if obj.is_instance_of::<pyo3::types::PyBool>() {
        return Err(PyTypeError::new_err(
            "max_zoom must be an int or \"auto\", got a bool",
        ));
    }
    if let Ok(n) = obj.extract::<i64>() {
        return u8::try_from(n).map(MaxZoom::Fixed).map_err(|_| {
            PyValueError::new_err(format!(
                "max_zoom must be between 0 and 255 (or \"auto\"), got {n}"
            ))
        });
    }
    Err(PyTypeError::new_err(format!(
        "max_zoom must be an int or \"auto\", got {}",
        obj.get_type().name()?
    )))
}

/// Resolve `max_zoom="auto"` (#444) once `options` is built and every cheap
/// check has passed ([`MaxZoom::resolve`] validates the options before
/// reading anything). A fixed zoom returns without opening the input; for
/// `auto` a dedicated source is opened with `open_source`, so the
/// conversion's own source is untouched. Core checks `output` before the
/// estimate reads the input (#452). The GIL is released while the estimate
/// reads.
fn resolve_max_zoom_py(
    py: Python<'_>,
    max_zoom: MaxZoom,
    options: &mut ConvertOptions,
    output: &Path,
    open_source: impl FnOnce() -> Result<ConvertSource, ConvertError> + Send,
) -> PyResult<u8> {
    if !max_zoom.is_auto() {
        return Ok(max_zoom.plan_zoom());
    }
    py.detach(|| {
        let source = open_source()?;
        max_zoom.resolve(&source, options, Some(output))
    })
    .map_err(convert_error_to_py)
}

/// Native body of `tylertoo.convert()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
///
/// Delegates the whole chain to core's tiles facade
/// ([`tylertoo_core::overview::tiles::convert_to_pmtiles`], #439), so the
/// policy — intermediate lifecycle and spill precedence, free-space
/// preflight, knob guards, the #380 declaration, the combined report — can
/// never drift from `tylertoo tiles`.
#[pyfunction]
#[pyo3(
    signature = (input, output, min_zoom=0, max_zoom=MaxZoom::Fixed(14), layer_name=None, tile_size_limit=512000, simple_clip_fastpath=true, feature_order="input", spill_dir=None),
    text_signature = "(input, output, min_zoom=0, max_zoom=14, layer_name=None, tile_size_limit=512000, simple_clip_fastpath=True, feature_order='input', spill_dir=None)"
)]
#[allow(clippy::too_many_arguments)] // mirrors the Python kwarg signature
fn convert(
    py: Python<'_>,
    input: &str,
    output: &str,
    min_zoom: u8,
    #[pyo3(from_py_with = extract_max_zoom)] max_zoom: MaxZoom,
    layer_name: Option<String>,
    tile_size_limit: Option<usize>,
    simple_clip_fastpath: bool,
    feature_order: &str,
    spill_dir: Option<PathBuf>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let input_path = Path::new(input).to_path_buf();
    let output_path = Path::new(output).to_path_buf();

    // #444: built with `auto`'s placeholder, then resolved (a fixed zoom is
    // its own placeholder and comes back verbatim, with no I/O).
    let mut convert_options = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom,
            max_zoom: max_zoom.plan_zoom(),
        },
        // #439: the scratch knob the CLI spells `--spill-dir`. It also picks
        // where the intermediate overview lands (precedence: this, then
        // `$TMPDIR`, then the output's directory), which is why this facade
        // exposes it: `$TMPDIR` is tmpfs in many containers, and the
        // intermediate is at least input-sized.
        spill_dir,
        ..ConvertOptions::default()
    };
    // Resolves `auto` and writes the chosen zoom back into `levels`, which
    // is where the facade reads the declared range from.
    resolve_max_zoom_py(py, max_zoom, &mut convert_options, &output_path, || {
        ConvertSource::resolve_path(&input_path).map_err(ConvertError::from)
    })?;

    let options = TilesOptions {
        convert: convert_options,
        // `None` lets core derive the layer name from the input: file stem
        // for a single file, last path segment for a directory or
        // `s3://`/`gs://` prefix, last literal segment for a glob (core owns
        // the rules).
        layer_name,
        // #314: not exposed on this deprecated one-shot facade; use
        // `overview()` + `export_pmtiles()` instead.
        keep_overview: None,
        // Sharded builds (#498) are a CLI/Rust-API feature for now, like the
        // convert plan they depend on: `tylertoo shard-plan`, `tiles --shard`
        // and `export-pmtiles --tile-range`.
        shard: None,
        tile_range: None,
        tile_buffer: 8,
        // 0 (or None) disables the cap; any positive value is the byte limit.
        tile_size_limit: tile_size_limit.filter(|&n| n > 0),
        simple_clip_fastpath,
        // Convenience wrapper: use the core auto default (core-sized wave).
        partition_wave: tylertoo_core::overview::export::PARTITION_WAVE_AUTO,
        feature_order: parse_feature_order(feature_order)?,
        // #443: not exposed here; `export_pmtiles(..., feature_id=...)`.
        feature_id: None,
    };

    // Release the GIL while the Rust pipelines run.
    let outcome = py
        .detach(|| convert_to_pmtiles(&TilesInput::Path(input_path), &output_path, &options))
        .map_err(tiles_error_to_py)?;
    let report = match outcome {
        TilesOutcome::Converted(report) => report,
        // Unreachable without a shard job, which this facade never sets.
        other => {
            return Err(PyErr::new::<PyRuntimeError, _>(format!(
                "unexpected tiles outcome: {other:?}"
            )))
        }
    };

    // #314: core logs this preflight as a `log::warn`, which reaches Python
    // `logging` (#445) — but only for a caller who looks there, and a full
    // disk mid-run is worth saying louder. Raise it as a `UserWarning` too,
    // after the run (the warning is advisory; the convert either succeeded
    // or already failed with its own error).
    if let Some(msg) = &report.intermediate.space_warning {
        PyErr::warn(
            py,
            &py.get_type::<PyUserWarning>(),
            &CString::new(&**msg)?,
            1,
        )?;
    }

    // The combined report (#439): the two halves the two-step chain's own
    // reports would carry, keyed like the CLI's `tiles --report` JSON.
    let dict = PyDict::new(py);
    dict.set_item("convert", convert_report_to_dict(py, &report.convert)?)?;
    dict.set_item("export", export_report_to_dict(py, &report.export)?)?;
    Ok(dict.into())
}

/// Map a [`TilesError`] to the Python exception type it deserves: the
/// convert half keeps [`convert_error_to_py`]'s mapping; preflight refusals
/// (a bad `keep_overview` directory, the #386/#443 knob guards) are
/// `ValueError`; everything else is `RuntimeError`.
fn tiles_error_to_py(e: TilesError) -> PyErr {
    match e {
        TilesError::Convert(inner) => convert_error_to_py(inner),
        TilesError::KeepOverviewDirMissing { .. }
        | TilesError::FeatureOrderColumnExcluded { .. }
        | TilesError::FeatureIdColumnExcluded { .. }
        | TilesError::FeatureIdAccumulated { .. } => {
            PyErr::new::<PyValueError, _>(with_sources(&e))
        }
        other => PyErr::new::<PyRuntimeError, _>(with_sources(&other)),
    }
}

/// `Display` plus the `#[source]` chain, `": "`-joined — what the CLI shows
/// through anyhow's `Caused by:` and what a Python message would otherwise
/// drop (an I/O error's "Permission denied (os error 13)" lives in
/// `source()`, which `Display` alone omits).
fn with_sources(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cause = e.source();
    while let Some(c) = cause {
        use std::fmt::Write as _;
        let _ = write!(out, ": {c}");
        cause = c.source();
    }
    out
}

/// Map a [`ConvertError`] to the Python exception type it deserves:
/// user-input problems (bad options, missing/mistyped columns, invalid level
/// plans, an input whose coordinates or CRS the tiler cannot work with)
/// become `ValueError`; the #543 pass-1 memory preflight becomes
/// `MemoryError`; everything else (I/O, decode, writer) becomes
/// `RuntimeError`.
fn convert_error_to_py(e: ConvertError) -> PyErr {
    match e {
        // The box is too small for this input (#543).
        ConvertError::Pass1MemoryFloorExceeded { .. } => {
            PyErr::new::<PyMemoryError, _>(format!("{}", e))
        }
        // The input itself is the problem, not the run (#429).
        ConvertError::AllFeaturesOutOfRange { .. }
        | ConvertError::UnsupportedCrs { .. }
        | ConvertError::InvalidLevels(_)
        | ConvertError::RankingConflict
        | ConvertError::ClusterPartitioningUnsupported
        | ConvertError::AccumulateWithoutCluster
        | ConvertError::SortKeyColumnMissing { .. }
        | ConvertError::ClassRankColumnMissing { .. }
        | ConvertError::ClassRankColumnNotString { .. }
        | ConvertError::AccumulateColumnMissing { .. }
        | ConvertError::AccumulateColumnNotNumeric { .. }
        | ConvertError::AutoZoomMinAboveCeiling { .. }
        | ConvertError::AutoZoomNoSignal { .. } => PyErr::new::<PyValueError, _>(format!("{}", e)),
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
    // Input-quality counters: antimeridian suspects (#188) and the two #429
    // losses (outside the declared CRS's range; valid lon/lat outside the Web
    // Mercator tiling domain). Python callers get the same honesty the CLI
    // summary does.
    dict.set_item(
        "antimeridian_suspect_features",
        report.antimeridian_suspect_features,
    )?;
    dict.set_item("out_of_range_features", report.out_of_range_features)?;
    dict.set_item("unprojectable_features", report.unprojectable_features)?;
    // #553: the first few out-of-range features, by row and coordinate.
    let exemplars = PyList::empty(py);
    for e in &report.out_of_range_exemplars {
        let d = PyDict::new(py);
        d.set_item("part", e.part)?;
        d.set_item("row", e.row)?;
        d.set_item("axis", e.axis)?;
        d.set_item("value", e.value)?;
        exemplars.append(d)?;
    }
    dict.set_item("out_of_range_exemplars", exemplars)?;
    // #644: features drawn wrapped across the antimeridian. Informational.
    dict.set_item("wrapped_features", report.wrapped_features)?;
    // #451: the input declared no CRS at all and WGS84 lon/lat was assumed —
    // the same visibility the Rust report (and its warning) gives.
    dict.set_item("crs_assumed_wgs84", report.crs_assumed_wgs84)?;
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

/// The Python `accumulate_attributes` mapping as core's per-cluster
/// aggregation specs, with the two clustering pre-checks the CLI also makes
/// (core enforces both as well; raising here gives a Python-shaped error
/// naming the keyword argument rather than the Rust option).
fn accumulate_specs(
    accumulate_attributes: Option<BTreeMap<String, String>>,
    cluster: bool,
    mode: Mode,
) -> PyResult<Vec<AccumulateSpec>> {
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
    accumulate_attributes
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
        .collect()
}

/// The `class_rank_column` / `class_ranks` / `class_rank_unknown` kwargs as
/// core's [`ClassRanking`]. The pairing rules are Python-shaped (the CLI
/// spells all three as one `--class-rank` string); the ranking rules — at
/// least one class, finite ranks (#428), unknown defaulting below every
/// class — are core's, shared with `--class-rank` through
/// [`ClassRanking::new`].
fn class_ranking_from_kwargs(
    has_sort_key: bool,
    class_rank_column: Option<String>,
    class_ranks: Option<BTreeMap<String, f64>>,
    class_rank_unknown: Option<f64>,
) -> PyResult<Option<ClassRanking>> {
    if has_sort_key && class_rank_column.is_some() {
        return Err(PyErr::new::<PyValueError, _>(
            "sort_key and class_rank_column are mutually exclusive; supply at most one",
        ));
    }
    match (class_rank_column, class_ranks) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(PyErr::new::<PyValueError, _>(
            "class_rank_column and class_ranks must be supplied together",
        )),
        (Some(column), Some(ranks)) => {
            ClassRanking::new(column, ranks.into_iter().collect(), class_rank_unknown)
                .map(Some)
                .map_err(|e| PyErr::new::<PyValueError, _>(format!("class_ranks: {e}")))
        }
    }
}

/// Native body of `tylertoo.overview()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
#[pyfunction]
#[pyo3(
    signature = (
    input,
    output,
    *,
    mode="duplicating",
    min_zoom=0,
    max_zoom=MaxZoom::Fixed(6),
    gsds=None,
    gsd_base=1024.0,
    sort_key=None,
    magnitude_ladder=None,
    ladder_step=1,
    sort_direction="desc",
    class_rank_column=None,
    class_ranks=None,
    class_rank_unknown=None,
    no_auto_rank=false,
    simplify_factor=1.0,
    collapse=false,
    collapse_square=false,
    representation=None,
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
    filter=None,
    profile="auto",
    in_flight_batches=0,
    read_workers=0,
    spill_dir=None,
),
    text_signature = "(input, output, *, mode='duplicating', min_zoom=0, max_zoom=6, gsds=None, gsd_base=1024.0, sort_key=None, magnitude_ladder=None, ladder_step=1, sort_direction='desc', class_rank_column=None, class_ranks=None, class_rank_unknown=None, no_auto_rank=False, simplify_factor=1.0, collapse=False, collapse_square=False, representation=None, cascade=True, point_thinning=None, line_thinning=1.0, polygon_thinning=1.0, line_visibility=2.0, polygon_visibility=2.0, drop_rate=1.65, drop_gamma=1.5, density_drop=True, cluster=False, accumulate_attributes=None, coalesce_lines=True, coalesce_snap=1.0, coalesce_junction_angle=0.0, coalesce_max_level_rows=2000000, cogp_compat=False, row_group_size=10000, full_column_stats=False, streaming=True, read_batch_size=8192, bbox=None, filter=None, profile='auto', in_flight_batches=0, read_workers=0, spill_dir=None)"
)]
#[allow(clippy::too_many_arguments)] // Python API mirrors CLI flags; grouping into struct would hurt usability
fn overview(
    py: Python<'_>,
    input: &Bound<'_, PyAny>,
    output: &str,
    mode: &str,
    min_zoom: u8,
    #[pyo3(from_py_with = extract_max_zoom)] max_zoom: MaxZoom,
    gsds: Option<Vec<f64>>,
    gsd_base: f64,
    sort_key: Option<String>,
    magnitude_ladder: Option<&str>,
    ladder_step: u8,
    sort_direction: &str,
    class_rank_column: Option<String>,
    class_ranks: Option<BTreeMap<String, f64>>,
    class_rank_unknown: Option<f64>,
    no_auto_rank: bool,
    simplify_factor: f64,
    collapse: bool,
    collapse_square: bool,
    representation: Option<String>,
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
    filter: Option<String>,
    profile: &str,
    in_flight_batches: usize,
    read_workers: usize,
    spill_dir: Option<PathBuf>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
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
        // #444: `auto`'s placeholder until it is resolved below.
        None => LevelPlan::ZoomRange {
            min_zoom,
            max_zoom: max_zoom.plan_zoom(),
        },
    };

    let class_ranking = class_ranking_from_kwargs(
        sort_key.is_some(),
        class_rank_column,
        class_ranks,
        class_rank_unknown,
    )?;

    // Below-tolerance collapse disposition (#279) + zoom-band representation
    // selector (#317); mirrors the CLI flags. Structural validity of the
    // bands against the level plan is enforced by core convert validation.
    if collapse && collapse_square {
        return Err(PyErr::new::<PyValueError, _>(
            "collapse and collapse_square are mutually exclusive",
        ));
    }
    let collapse_mode = if collapse_square {
        CollapseMode::Square
    } else if collapse {
        CollapseMode::Point
    } else {
        CollapseMode::Drop
    };
    let representation_bands = match &representation {
        Some(spec) => parse_representation_spec(spec)
            .map_err(|e| PyErr::new::<PyValueError, _>(format!("representation: {e}")))?,
        None => Vec::new(),
    };

    let accumulate = accumulate_specs(accumulate_attributes, cluster, mode)?;

    // Cluster-conditional default: with cluster=True, absorbed points are
    // summarized (point_count), so the sparser 16.0 grid is the better look.
    let point_thinning = point_thinning.unwrap_or(if cluster {
        CLUSTER_POINT_THINNING_DEFAULT
    } else {
        AssignConfig::default().point_thinning
    });

    let mut options = ConvertOptions {
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
        entry_zoom: magnitude_ladder.map(|column| EntryZoomSpec {
            column: column.to_string(),
            kind: EntryZoomKind::DenseRank { step: ladder_step },
        }),
        class_ranking,
        no_auto_rank,
        simplify: SimplifyOptions {
            factor: simplify_factor,
            collapse: collapse_mode,
            cascade,
        },
        representation: representation_bands,
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
        read_workers,
        cluster,
        accumulate,
        coalesce_lines,
        coalesce_snap,
        coalesce_max_level_rows,
        coalesce_junction_angle,
        bbox: bbox.map(|(xmin, ymin, xmax, ymax)| [xmin, ymin, xmax, ymax]),
        filter,
        // #386: property selection is not on the Python surface yet.
        properties: Default::default(),
        spill_dir,
        // The convert plan artifact (--save-plan / --plan) is not on the
        // Python surface yet; follow-up.
        save_plan: None,
        plan: None,
        // Sharded builds (#498/#541): CLI + Rust API only, like their plan.
        shard: None,
        shard_plan_digest: None,
        zoom_ceiling: None,
    };

    // `str` stays the single-input path (files, directories, globs, remote
    // URLs/prefixes — resolved by core); `list[str]` is an explicit ordered
    // part list (v0.7 multi-partition). The str check runs FIRST: a str is
    // itself a sequence, so Vec extraction would happily split it into
    // characters.
    enum OverviewInput {
        Path(PathBuf),
        List(Vec<String>),
    }
    let parsed = if let Ok(s) = input.extract::<String>() {
        OverviewInput::Path(PathBuf::from(s))
    } else if let Ok(list) = input.extract::<Vec<String>>() {
        OverviewInput::List(list)
    } else {
        return Err(PyTypeError::new_err(
            "input must be a str path/URL or a list[str] of paths/URLs",
        ));
    };
    let output_path = Path::new(output).to_path_buf();

    // #444: every cheap check has passed; now estimate `auto` over a
    // dedicated source (skipped for an explicit `gsds` ladder).
    resolve_max_zoom_py(py, max_zoom, &mut options, &output_path, || match &parsed {
        OverviewInput::Path(p) => ConvertSource::resolve_path(p).map_err(ConvertError::from),
        OverviewInput::List(inputs) => {
            ConvertSource::from_input_list(inputs).map_err(ConvertError::from)
        }
    })?;

    // Release the GIL while the Rust pipeline runs.
    let report = py
        .detach(|| match &parsed {
            OverviewInput::Path(input_path) => {
                convert_to_overviews(input_path, &output_path, &options)
            }
            OverviewInput::List(inputs) => {
                let source = ConvertSource::from_input_list(inputs).map_err(ConvertError::from)?;
                convert_to_overviews_sources(&source, &output_path, &options)
            }
        })
        .map_err(convert_error_to_py)?;

    convert_report_to_dict(py, &report)
}

/// Native body of `tylertoo.export_pmtiles()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
#[pyfunction]
#[pyo3(signature = (input, output, *, layer_name="overview", tile_buffer=8, extent=4096, tile_size_limit=512000, simple_clip_fastpath=true, partition_wave=0, feature_order="input", min_zoom=None, feature_id=None, spill_dir=None))]
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
    partition_wave: usize,
    feature_order: &str,
    min_zoom: Option<u8>,
    feature_id: Option<String>,
    spill_dir: Option<PathBuf>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let options = ExportOptions {
        layer_name: layer_name.to_string(),
        tile_buffer,
        extent,
        // 0 (or None) disables the cap; any positive value is the byte limit.
        tile_size_limit: tile_size_limit.filter(|&n| n > 0),
        simple_clip_fastpath,
        partition_wave,
        feature_order: parse_feature_order(feature_order)?,
        // #380: the declared minimum; None keeps the coarsest level's zoom.
        min_zoom,
        properties: Default::default(),
        // Sharded builds (#498) are a CLI/Rust-API feature for now, like the
        // convert plan they depend on: `tylertoo shard-plan`, `tiles --shard`
        // and `export-pmtiles --tile-range`.
        tile_range: None,
        zoom_ceiling: None,
        feature_id,
        spill_dir,
    };
    let input_path = Path::new(input).to_path_buf();
    let output_path = Path::new(output).to_path_buf();

    let report = py
        .detach(|| export_pmtiles_core(&input_path, &output_path, &options))
        .map_err(|e| PyErr::new::<PyRuntimeError, _>(format!("export failed: {}", e)))?;

    export_report_to_dict(py, &report)
}

/// Convert an [`ExportReport`] to a Python dict (shared by
/// `export_pmtiles()` and the `convert()` facade's combined report, #439).
fn export_report_to_dict(py: Python<'_>, report: &ExportReport) -> PyResult<Py<PyDict>> {
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
        d.set_item("encode_dropped_features", z.encode_dropped_features)?;
        d.set_item("encode_quantized_features", z.encode_quantized_features)?;
        zooms.append(d)?;
    }
    dict.set_item("zooms", zooms)?;
    dict.set_item("total_tiles", report.total_tiles)?;
    dict.set_item("total_tile_features", report.total_tile_features)?;
    dict.set_item("oversized_tiles", report.oversized_tiles)?;
    let skipped = PyList::empty(py);
    for col in &report.skipped_property_columns {
        let d = PyDict::new(py);
        d.set_item("name", &col.name)?;
        d.set_item("data_type", &col.data_type)?;
        skipped.append(d)?;
    }
    dict.set_item("skipped_property_columns", skipped)?;
    dict.set_item("encode_dropped_features", report.encode_dropped_features)?;
    dict.set_item(
        "encode_quantized_features",
        report.encode_quantized_features,
    )?;
    dict.set_item("duration_secs", report.duration_secs)?;
    Ok(dict.into())
}

/// Native body of `tylertoo.validate()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
#[pyfunction]
#[pyo3(signature = (file))]
fn validate(py: Python<'_>, file: &str) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
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

/// The compiled extension, imported as `tylertoo._tylertoo`.
///
/// `python/tylertoo/__init__.py` wraps it with the public, typed API.
///
/// Importing it installs pyo3-log as the `log` backend (#445), so core's
/// diagnostics reach Python's `logging` under `tylertoo_core.*` (the Rust
/// module path, `::` → `.`) — the warnings and progress lines the CLI
/// prints. Records below DEBUG are never forwarded. Forwarding takes the GIL,
/// so every core call here runs inside `py.detach`: a worker thread logging
/// while this thread waited on it with the GIL held would deadlock.
#[pymodule]
fn _tylertoo(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Another extension in this process may already own the global logger;
    // keep it rather than fail the import.
    if let Ok(handle) =
        pyo3_log::Logger::new(m.py(), pyo3_log::Caching::LoggersAndLevels)?.install()
    {
        let _ = LOG_RESET.set(handle);
    }
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    m.add_function(wrap_pyfunction!(overview, m)?)?;
    m.add_function(wrap_pyfunction!(export_pmtiles, m)?)?;
    m.add_function(wrap_pyfunction!(validate, m)?)?;
    Ok(())
}
