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
use tylertoo_core::decode::{decode_pmtiles, DecodeError, DecodeOptions};
use tylertoo_core::input_set::ConvertSource;
use tylertoo_core::overview::assign::{AssignConfig, DensityBudgetConfig, SortDirection};
use tylertoo_core::overview::auto_zoom::MaxZoom;
use tylertoo_core::overview::check::validate_file;
use tylertoo_core::overview::cluster::{AccumulateOp, AccumulateSpec};
use tylertoo_core::overview::convert::{
    convert_to_overviews, convert_to_overviews_sources, parse_entry_zoom,
    parse_representation_spec, ClassRanking, ConvertError, ConvertOptions, ConvertReport,
    GeneralizationOverrides, LevelPlan,
};
use tylertoo_core::overview::export::{
    export_pmtiles as export_pmtiles_core, ExportError, ExportOptions, ExportReport, FeatureOrder,
};
use tylertoo_core::overview::ladder::{EntryZoomKind, EntryZoomSpec};
use tylertoo_core::overview::level::{MemoryProfile, Mode};
use tylertoo_core::overview::properties::PropertySelection;
use tylertoo_core::overview::simplify::{CollapseMode, SimplifyOptions};
use tylertoo_core::overview::tiles::{
    convert_to_pmtiles, TilesError, TilesInput, TilesOptions, TilesOutcome,
};
use tylertoo_core::pyramid::{build_pyramid, parse_bands, PyramidOptions};

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

/// The `include_properties` / `exclude_properties` / `exclude_all_properties`
/// kwargs as core's [`PropertySelection`] (#445, #402). Core owns the
/// precedence (an include list wins) and the validation; an empty include
/// list keeps no properties, as core documents.
fn property_selection(
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    exclude_all: bool,
) -> PropertySelection {
    PropertySelection {
        include,
        exclude: exclude.unwrap_or_default(),
        exclude_all,
    }
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
    signature = (input, output, min_zoom=0, max_zoom=MaxZoom::Fixed(14), layer_name=None, tile_size_limit=512000, simple_clip_fastpath=true, feature_order="input", spill_dir=None, *, include_properties=None, exclude_properties=None, exclude_all_properties=false),
    text_signature = "(input, output, min_zoom=0, max_zoom=14, layer_name=None, tile_size_limit=512000, simple_clip_fastpath=True, feature_order='input', spill_dir=None, *, include_properties=None, exclude_properties=None, exclude_all_properties=False)"
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
    include_properties: Option<Vec<String>>,
    exclude_properties: Option<Vec<String>>,
    exclude_all_properties: bool,
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
        // Applied at convert time, so unselected columns are never read; the
        // facade's own export keeps every column the overview holds.
        properties: property_selection(
            include_properties,
            exclude_properties,
            exclude_all_properties,
        ),
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
        // Publication metadata is not exposed on this deprecated facade.
        metadata: Default::default(),
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
        | ConvertError::InvalidConfig(_)
        | ConvertError::RankingConflict
        | ConvertError::ClusterPartitioningUnsupported
        | ConvertError::VerbatimPartitioningUnsupported
        | ConvertError::AccumulateWithoutCluster
        | ConvertError::SortKeyColumnMissing { .. }
        | ConvertError::ClassRankColumnMissing { .. }
        | ConvertError::ClassRankColumnNotString { .. }
        | ConvertError::AccumulateColumnMissing { .. }
        | ConvertError::AccumulateColumnNotNumeric { .. }
        | ConvertError::AutoZoomMinAboveCeiling { .. }
        | ConvertError::AutoZoomNoSignal { .. }
        | ConvertError::Properties(_) => PyErr::new::<PyValueError, _>(format!("{}", e)),
        other => PyErr::new::<PyRuntimeError, _>(format!("overview conversion failed: {}", other)),
    }
}

/// Convert a [`ConvertReport`] to a Python dict.
fn convert_report_to_dict(py: Python<'_>, report: &ConvertReport) -> PyResult<Py<PyDict>> {
    report_dict(pythonize::pythonize(py, report)?)
}

/// A pythonized core report as the dict it is: every report struct
/// serializes as a map, keyed by its serde field names (the keys of the
/// CLI's `--report` JSON), so the Python and CLI reports cannot drift.
fn report_dict(value: Bound<'_, PyAny>) -> PyResult<Py<PyDict>> {
    Ok(value.cast_into::<PyDict>()?.unbind())
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

/// The entry-zoom ladder (#364, #445): derived from a column's values
/// (`magnitude_ladder`, every `ladder_step` zooms), or spelled out as
/// `COLUMN:VALUE=ZOOM,...` (`entry_zoom`, core's grammar, shared with the
/// CLI's `--entry-zoom`). The two are mutually exclusive, which clap
/// enforces for the CLI.
fn entry_zoom_spec(
    magnitude_ladder: Option<&str>,
    ladder_step: u8,
    entry_zoom: Option<&str>,
) -> PyResult<Option<EntryZoomSpec>> {
    match (magnitude_ladder, entry_zoom) {
        (Some(_), Some(_)) => Err(PyErr::new::<PyValueError, _>(
            "magnitude_ladder and entry_zoom are mutually exclusive; supply at most one",
        )),
        (Some(column), None) => Ok(Some(EntryZoomSpec {
            column: column.to_string(),
            kind: EntryZoomKind::DenseRank { step: ladder_step },
        })),
        (None, Some(spec)) => parse_entry_zoom(spec)
            .map(Some)
            .map_err(PyErr::new::<PyValueError, _>),
        (None, None) => Ok(None),
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
    entry_zoom=None,
    sort_direction="desc",
    class_rank_column=None,
    class_ranks=None,
    class_rank_unknown=None,
    no_auto_rank=false,
    verbatim=false,
    simplify_factor=None,
    collapse=false,
    collapse_square=false,
    representation=None,
    cascade=true,
    point_thinning=None,
    line_thinning=None,
    polygon_thinning=None,
    line_visibility=None,
    polygon_visibility=None,
    drop_rate=1.65,
    drop_gamma=1.5,
    density_drop=None,
    cluster=false,
    accumulate_attributes=None,
    coalesce_lines=None,
    coalesce_snap=1.0,
    coalesce_junction_angle=0.0,
    coalesce_max_level_rows=2_000_000,
    include_properties=None,
    exclude_properties=None,
    exclude_all_properties=false,
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
    text_signature = "(input, output, *, mode='duplicating', min_zoom=0, max_zoom=6, gsds=None, gsd_base=1024.0, sort_key=None, magnitude_ladder=None, ladder_step=1, entry_zoom=None, sort_direction='desc', class_rank_column=None, class_ranks=None, class_rank_unknown=None, no_auto_rank=False, verbatim=False, simplify_factor=None, collapse=False, collapse_square=False, representation=None, cascade=True, point_thinning=None, line_thinning=None, polygon_thinning=None, line_visibility=None, polygon_visibility=None, drop_rate=1.65, drop_gamma=1.5, density_drop=None, cluster=False, accumulate_attributes=None, coalesce_lines=None, coalesce_snap=1.0, coalesce_junction_angle=0.0, coalesce_max_level_rows=2000000, include_properties=None, exclude_properties=None, exclude_all_properties=False, cogp_compat=False, row_group_size=10000, full_column_stats=False, streaming=True, read_batch_size=8192, bbox=None, filter=None, profile='auto', in_flight_batches=0, read_workers=0, spill_dir=None)"
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
    entry_zoom: Option<&str>,
    sort_direction: &str,
    class_rank_column: Option<String>,
    class_ranks: Option<BTreeMap<String, f64>>,
    class_rank_unknown: Option<f64>,
    no_auto_rank: bool,
    verbatim: bool,
    simplify_factor: Option<f64>,
    collapse: bool,
    collapse_square: bool,
    representation: Option<String>,
    cascade: bool,
    point_thinning: Option<f64>,
    line_thinning: Option<f64>,
    polygon_thinning: Option<f64>,
    line_visibility: Option<f64>,
    polygon_visibility: Option<f64>,
    drop_rate: f64,
    drop_gamma: f64,
    density_drop: Option<bool>,
    cluster: bool,
    accumulate_attributes: Option<BTreeMap<String, String>>,
    coalesce_lines: Option<bool>,
    coalesce_snap: f64,
    coalesce_junction_angle: f64,
    coalesce_max_level_rows: usize,
    include_properties: Option<Vec<String>>,
    exclude_properties: Option<Vec<String>>,
    exclude_all_properties: bool,
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
    let mode: Mode = mode.parse().map_err(|_| {
        PyValueError::new_err(format!(
            "Invalid mode: '{mode}'. Valid options: duplicating, partitioning"
        ))
    })?;
    let profile: MemoryProfile = profile.parse().map_err(|_| {
        PyValueError::new_err(format!(
            "Invalid profile: '{profile}'. Valid options: auto, speed, bounded"
        ))
    })?;
    // Case-insensitive here, as it always was for this keyword.
    let sort_direction: SortDirection = sort_direction.to_lowercase().parse().map_err(|_| {
        PyValueError::new_err(format!(
            "Invalid sort_direction: '{sort_direction}'. Valid options: desc, asc"
        ))
    })?;

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

    // `None` is "not set": core resolves it to the normal default, or to
    // off under `verbatim`, so an explicit value always wins (#445).
    let overrides = GeneralizationOverrides {
        point_thinning,
        line_thinning,
        polygon_thinning,
        line_visibility,
        polygon_visibility,
        simplify_factor,
        density_drop,
        coalesce_lines,
    };

    let mut options = ConvertOptions {
        mode,
        levels,
        assign: AssignConfig {
            sort_direction,
            ..AssignConfig::default()
        },
        sort_key,
        entry_zoom: entry_zoom_spec(magnitude_ladder, ladder_step, entry_zoom)?,
        class_ranking,
        no_auto_rank,
        simplify: SimplifyOptions {
            collapse: collapse_mode,
            cascade,
            ..SimplifyOptions::default()
        },
        representation: representation_bands,
        density: DensityBudgetConfig {
            drop_rate,
            gamma: drop_gamma,
            ..DensityBudgetConfig::default()
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
        coalesce_snap,
        coalesce_max_level_rows,
        coalesce_junction_angle,
        bbox: bbox.map(|(xmin, ymin, xmax, ymax)| [xmin, ymin, xmax, ymax]),
        filter,
        properties: property_selection(
            include_properties,
            exclude_properties,
            exclude_all_properties,
        ),
        spill_dir,
        // The convert plan artifact (--save-plan / --plan) is not on the
        // Python surface yet; follow-up.
        save_plan: None,
        plan: None,
        // Sharded builds (#498/#541): CLI + Rust API only, like their plan.
        shard: None,
        shard_plan_digest: None,
        zoom_ceiling: None,
        ..ConvertOptions::default()
    }
    // Last: the defaults read `cluster`, the logged summary `entry_zoom`.
    .with_generalization(&overrides, verbatim)
    .map_err(convert_error_to_py)?;

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
#[pyo3(signature = (input, output, *, layer_name="overview", tile_buffer=8, extent=4096, tile_size_limit=512000, simple_clip_fastpath=true, partition_wave=0, feature_order="input", min_zoom=None, include_properties=None, exclude_properties=None, exclude_all_properties=false, feature_id=None, spill_dir=None, name=None, description=None, attribution=None))]
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
    include_properties: Option<Vec<String>>,
    exclude_properties: Option<Vec<String>>,
    exclude_all_properties: bool,
    feature_id: Option<String>,
    spill_dir: Option<PathBuf>,
    name: Option<String>,
    description: Option<String>,
    attribution: Option<String>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let mut metadata = tylertoo_core::TilesetMetadata::new();
    metadata.name = name;
    metadata.description = description;
    metadata.attribution = attribution;
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
        properties: property_selection(
            include_properties,
            exclude_properties,
            exclude_all_properties,
        ),
        // Sharded builds (#498) are a CLI/Rust-API feature for now, like the
        // convert plan they depend on: `tylertoo shard-plan`, `tiles --shard`
        // and `export-pmtiles --tile-range`.
        tile_range: None,
        zoom_ceiling: None,
        feature_id,
        spill_dir,
        metadata,
    };
    let input_path = Path::new(input).to_path_buf();
    let output_path = Path::new(output).to_path_buf();

    let report = py
        .detach(|| export_pmtiles_core(&input_path, &output_path, &options))
        .map_err(export_error_to_py)?;

    export_report_to_dict(py, &report)
}

/// Map an [`ExportError`] to a Python exception: a property selection the
/// overview file cannot satisfy, or publication metadata too large for an
/// archive, is the caller's input problem, so `ValueError`; everything else
/// keeps the `RuntimeError` it always was.
fn export_error_to_py(e: ExportError) -> PyErr {
    match e {
        ExportError::UnknownProperty { .. }
        | ExportError::UnsupportedProperty { .. }
        | ExportError::CoveringProperty { .. }
        | ExportError::PropertyRequiredByKnob { .. }
        | ExportError::MetadataTooLarge(_) => PyErr::new::<PyValueError, _>(format!("{e}")),
        other => PyErr::new::<PyRuntimeError, _>(format!("export failed: {other}")),
    }
}

/// Convert an [`ExportReport`] to a Python dict (shared by
/// `export_pmtiles()` and the `convert()` facade's combined report, #439).
fn export_report_to_dict(py: Python<'_>, report: &ExportReport) -> PyResult<Py<PyDict>> {
    report_dict(pythonize::pythonize(py, report)?)
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

/// Native body of `tylertoo.decode()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
#[pyfunction]
#[pyo3(signature = (input, output, *, min_zoom=None, max_zoom=None, layer=None))]
fn decode(
    py: Python<'_>,
    input: &str,
    output: &str,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
    layer: Option<String>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let options = DecodeOptions {
        min_zoom,
        max_zoom,
        layer,
    };
    let input_path = Path::new(input).to_path_buf();
    let output_path = Path::new(output).to_path_buf();
    let report = py
        .detach(|| decode_pmtiles(&input_path, &output_path, &options))
        .map_err(decode_error_to_py)?;

    // Keyed like the CLI's `decode --report` JSON (`DecodeReport`'s serde
    // names); `zoom_range` stays a tuple.
    report_dict(pythonize::pythonize(py, &report)?)
}

/// Map a [`DecodeError`] to a Python exception: an inverted zoom range, a
/// raster archive, or a property that collides with a provenance column is
/// the caller's input problem (`ValueError`); a damaged or unreadable
/// archive, or a failed write, is `RuntimeError`. The cause chain is kept,
/// since `DecodeError` leaves it out of its own `Display` (#396).
fn decode_error_to_py(e: DecodeError) -> PyErr {
    match e {
        DecodeError::InvalidZoomRange { .. }
        | DecodeError::NotVectorTiles(_)
        | DecodeError::ReservedColumn(_) => PyErr::new::<PyValueError, _>(with_sources(&e)),
        other => {
            PyErr::new::<PyRuntimeError, _>(format!("decode failed: {}", with_sources(&other)))
        }
    }
}

/// Native body of `tylertoo.pyramid()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
#[pyfunction]
#[pyo3(signature = (bands, output, *, generalize=false, tile_size_limit=None, feature_order="input", work_dir=None, allow_missing_zooms=false, name=None, description=None, attribution=None))]
#[allow(clippy::too_many_arguments)] // mirrors the Python kwarg signature
fn pyramid(
    py: Python<'_>,
    bands: Vec<String>,
    output: &str,
    generalize: bool,
    tile_size_limit: Option<usize>,
    feature_order: &str,
    work_dir: Option<PathBuf>,
    allow_missing_zooms: bool,
    name: Option<String>,
    description: Option<String>,
    attribution: Option<String>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    // Spec grammar, band-set rules and the input-exists check are core's,
    // shared with the CLI's `--band` (#445).
    let bands = parse_bands(&bands).map_err(PyErr::new::<PyValueError, _>)?;
    let mut metadata = tylertoo_core::TilesetMetadata::new();
    metadata.name = name;
    metadata.description = description;
    metadata.attribution = attribution;
    let mut options = PyramidOptions {
        work_dir,
        allow_missing_zooms,
        metadata,
        ..PyramidOptions::default()
    };
    // The default tiles each GeoParquet band verbatim (an aggregate band
    // must keep every feature); `generalize` runs the normal ladder, as the
    // CLI's `--generalize` does.
    if generalize {
        options.convert = ConvertOptions::default();
    }
    // 0 (or None) disables the cap; any positive value is the byte limit.
    options.export.tile_size_limit = tile_size_limit.filter(|&n| n > 0);
    options.export.feature_order = parse_feature_order(feature_order)?;
    let output_path = Path::new(output).to_path_buf();

    let report = py
        .detach(|| build_pyramid(&bands, &output_path, &options))
        .map_err(|e| {
            PyErr::new::<PyRuntimeError, _>(format!("pyramid build failed: {}", with_sources(&e)))
        })?;

    let dict = PyDict::new(py);
    dict.set_item("total_tiles", report.total_tiles)?;
    let per_band = PyList::empty(py);
    for (layer, min_zoom, max_zoom, tiles) in &report.per_band_tiles {
        let d = PyDict::new(py);
        d.set_item("layer", layer)?;
        d.set_item("min_zoom", min_zoom)?;
        d.set_item("max_zoom", max_zoom)?;
        d.set_item("tiles", tiles)?;
        per_band.append(d)?;
    }
    dict.set_item("bands", per_band)?;
    dict.set_item("skipped_tiles", report.skipped)?;
    dict.set_item(
        "bands_without_bounds",
        PyList::new(py, &report.bands_without_bounds)?,
    )?;
    let metadata = PyDict::new(py);
    metadata.set_item("name", &report.metadata.name)?;
    metadata.set_item("description", &report.metadata.description)?;
    metadata.set_item("attribution", &report.metadata.attribution)?;
    dict.set_item("metadata", metadata)?;
    dict.set_item(
        "metadata_conflicts",
        PyList::new(py, &report.metadata_conflicts)?,
    )?;
    dict.set_item(
        "bands_without_metadata",
        PyList::new(py, &report.bands_without_metadata)?,
    )?;
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
    m.add_function(wrap_pyfunction!(decode, m)?)?;
    m.add_function(wrap_pyfunction!(pyramid, m)?)?;
    Ok(())
}
