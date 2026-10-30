//! Python bindings for tylertoo
//!
//! This module exposes the tylertoo-core functionality to Python via pyo3.
//!
//! Keywords deserialize into core option structs. Core owns defaults and
//! validation; Python surfaces option refusals as `tylertoo.ConfigError`.
//! `build.rs` generates runtime signatures from core defaults, and stubtest
//! checks the full typed signatures against that runtime surface.

/// Global allocator for the extension's own Rust allocations (#739, #788).
///
/// Unconditional on musl (musllinux wheels), for #480's reason: musl's
/// `mallocng` fails small allocations under heavy multi-threaded
/// fragmentation. Elsewhere it is the `mimalloc` Cargo feature, off by
/// default. Python's allocator is untouched: a Rust `#[global_allocator]`
/// serves only this module's Rust code, and nothing allocated here is freed
/// by the interpreter (results cross into Python as copies). Excluded under
/// `dhat-heap`, whose allocator tylertoo-core installs.
#[cfg(all(
    any(target_env = "musl", feature = "mimalloc"),
    not(feature = "dhat-heap")
))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use pyo3::exceptions::{PyMemoryError, PyRuntimeError, PyTypeError, PyUserWarning, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};
use std::ffi::CString;
use std::path::{Path, PathBuf};
#[expect(
    clippy::disallowed_types,
    reason = "#445: process-wide Python logging bridge reset handle"
)]
use std::sync::OnceLock;
use tylertoo_core::decode::{decode_pmtiles, DecodeError, DecodeOptions};
use tylertoo_core::input_set::ConvertSource;
use tylertoo_core::overview::auto_zoom::MaxZoom;
use tylertoo_core::overview::check::validate_file;
use tylertoo_core::overview::config::keywords as config_keywords;
use tylertoo_core::overview::convert::{
    convert_to_overviews, convert_to_overviews_sources, ConvertError, ConvertOptions,
};
use tylertoo_core::overview::export::{
    export_pmtiles as export_pmtiles_core, ExportError, ExportOptions,
};
use tylertoo_core::overview::tiles::{
    convert_to_pmtiles, intermediate_space_preflight, SpaceRemedies, TilesError, TilesInput,
    TilesOptions, TilesOutcome,
};
use tylertoo_core::pyramid::{build_pyramid, parse_bands};

pyo3::create_exception!(
    _tylertoo,
    ConfigError,
    PyValueError,
    "An option value or combination refused before any input is read. The \
     rules are the core library's, the same ones the CLI enforces."
);

/// The installed log bridge's cache handle (#445), set once at import.
#[expect(
    clippy::disallowed_types,
    reason = "#445: process-wide Python logging bridge reset handle"
)]
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

/// A keyword argument that does not translate (an unknown spelling), as
/// `tylertoo.ConfigError`.
fn config_err(message: impl std::fmt::Display) -> PyErr {
    ConfigError::new_err(message.to_string())
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

// The public signatures and docstrings are generated from core defaults.
include!(concat!(env!("OUT_DIR"), "/signatures.rs"));

fn keywords(
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<serde_json::Map<String, serde_json::Value>> {
    let mut out = serde_json::Map::new();
    if let Some(kwargs) = kwargs {
        let mut deserializer = pythonize::Depythonizer::from_object(kwargs);
        config_keywords::validate_rank_syntax(&mut deserializer).map_err(config_err)?;
        let py = kwargs.py();
        let fspath = py.import("os")?.getattr("fspath")?;
        for (key, value) in kwargs.iter() {
            let key = key.extract::<String>()?;
            let value = if !value.is_instance_of::<PyString>() && value.hasattr("__fspath__")? {
                fspath.call1((value,))?
            } else {
                value
            };
            let mut deserializer = pythonize::Depythonizer::from_object(&value);
            let value =
                config_keywords::deserialize_value(&key, &mut deserializer).map_err(config_err)?;
            out.insert(key, value);
        }
    }
    Ok(out)
}

/// Map a [`ConvertError`] to the Python exception type it deserves: a
/// refused option ([`ConvertError::is_config`]) is `tylertoo.ConfigError`;
/// other user-input problems (missing/mistyped columns, an input whose
/// coordinates or CRS the tiler cannot work with) are `ValueError`; the #543
/// pass-1 memory preflight is `MemoryError`; everything else (I/O, decode,
/// writer) is `RuntimeError`.
fn convert_error_to_py(e: ConvertError) -> PyErr {
    if e.is_config() {
        return config_err(e);
    }
    match e {
        // The box is too small for this input (#543).
        ConvertError::BudgetExceeded { .. } => PyMemoryError::new_err(e.to_string()),
        // The input itself is the problem, not the run (#429).
        ConvertError::AllFeaturesOutOfRange { .. }
        | ConvertError::UnsupportedCrs { .. }
        | ConvertError::SortKeyColumnMissing { .. }
        | ConvertError::ClassRankColumnMissing { .. }
        | ConvertError::ClassRankColumnNotString { .. }
        | ConvertError::AccumulateColumnMissing { .. }
        | ConvertError::AccumulateColumnNotNumeric { .. }
        | ConvertError::AutoZoomNoSignal { .. }
        | ConvertError::Properties(_) => PyValueError::new_err(e.to_string()),
        other => PyRuntimeError::new_err(format!("overview conversion failed: {other}")),
    }
}

/// Map an [`ExportError`] to a Python exception: a refused option
/// ([`ExportError::is_config`]) is `tylertoo.ConfigError`; a property
/// selection the overview file cannot satisfy is `ValueError`; everything
/// else keeps the `RuntimeError` it always was.
fn export_error_to_py(e: ExportError) -> PyErr {
    if e.is_config() {
        return config_err(e);
    }
    match e {
        ExportError::UnknownProperty { .. }
        | ExportError::UnsupportedProperty { .. }
        | ExportError::CoveringProperty { .. }
        | ExportError::PropertyRequiredByKnob { .. } => PyValueError::new_err(e.to_string()),
        other => PyRuntimeError::new_err(format!("export failed: {other}")),
    }
}

/// Map a [`TilesError`] the way its halves map, with refused options as
/// `tylertoo.ConfigError`.
fn tiles_error_to_py(e: TilesError) -> PyErr {
    match e {
        TilesError::Convert(inner) => convert_error_to_py(inner),
        TilesError::Export(inner) => export_error_to_py(inner),
        e if e.is_config() => config_err(with_sources(&e)),
        other => PyRuntimeError::new_err(with_sources(&other)),
    }
}

/// Map a pyramid build's error: a refused option is `tylertoo.ConfigError`.
fn pyramid_error_to_py(e: tylertoo_core::Error) -> PyErr {
    match e {
        e if e.is_config() => config_err(with_sources(&e)),
        other => PyRuntimeError::new_err(format!("pyramid build failed: {}", with_sources(&other))),
    }
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
        | DecodeError::ReservedColumn(_) => PyValueError::new_err(with_sources(&e)),
        other => PyRuntimeError::new_err(format!("decode failed: {}", with_sources(&other))),
    }
}

/// A core report as the dict it is: every report struct serializes as a
/// map, keyed by its serde field names (the keys of the CLI's `--report`
/// JSON), so the Python and CLI reports cannot drift.
fn report_dict<T: serde::Serialize>(py: Python<'_>, report: &T) -> PyResult<Py<PyDict>> {
    Ok(pythonize::pythonize(py, report)?
        .cast_into::<PyDict>()?
        .unbind())
}

/// Replace display-only JSON filenames with `PyO3`'s native filesystem paths.
/// Keep every spill cleanup-managed until the entire result is ready.
fn report_dict_with_losses(
    py: Python<'_>,
    report: &impl serde::Serialize,
    losses: &[(&tylertoo_core::diagnostics::SourceLosses, Option<&str>)],
) -> PyResult<Py<PyDict>> {
    let dict = report_dict(py, report)?;
    for (losses, section) in losses {
        if let Some(path) = losses.python_path() {
            let parent = if let Some(section) = section {
                dict.bind(py)
                    .get_item(section)?
                    .ok_or_else(|| PyRuntimeError::new_err("missing report section"))?
            } else {
                dict.bind(py).clone().into_any()
            };
            parent.get_item("source_losses")?.set_item("path", path)?;
        }
    }
    for (losses, _) in losses {
        losses.publish_python();
    }
    Ok(dict)
}

fn check_keywords(
    command: &str,
    values: &serde_json::Map<String, serde_json::Value>,
) -> PyResult<()> {
    let defaults = config_keywords::command_defaults(command);
    for key in values.keys() {
        if !defaults.contains_key(key) {
            return Err(config_err(format!("unknown keyword {key}")));
        }
    }
    Ok(())
}

fn overview_config(
    values: serde_json::Map<String, serde_json::Value>,
) -> PyResult<(MaxZoom, ConvertOptions)> {
    config_with_max_zoom("overview", values, |values| {
        config_keywords::convert(values, ConvertOptions::default()).map_err(convert_error_to_py)
    })
}

fn tiles_config(
    values: serde_json::Map<String, serde_json::Value>,
) -> PyResult<(MaxZoom, TilesOptions)> {
    config_with_max_zoom("convert", values, |values| {
        config_keywords::tiles(values).map_err(tiles_error_to_py)
    })
}

fn config_with_max_zoom<T>(
    command: &str,
    mut values: serde_json::Map<String, serde_json::Value>,
    parse: impl FnOnce(serde_json::Map<String, serde_json::Value>) -> PyResult<T>,
) -> PyResult<(MaxZoom, T)> {
    check_keywords(command, &values)?;
    let max_zoom = config_keywords::max_zoom(&mut values, command).map_err(config_err)?;
    Ok((max_zoom, parse(values)?))
}

fn export_config(values: serde_json::Map<String, serde_json::Value>) -> PyResult<ExportOptions> {
    check_keywords("export_pmtiles", &values)?;
    config_keywords::export(values, ExportOptions::default()).map_err(config_err)
}

fn pyramid_config(
    values: serde_json::Map<String, serde_json::Value>,
) -> PyResult<tylertoo_core::pyramid::PyramidOptions> {
    check_keywords("pyramid", &values)?;
    config_keywords::pyramid(values).map_err(config_err)
}

// --- max_zoom --------------------------------------------------------------------

/// Extract the `max_zoom` kwarg (#444): an `int` zoom, or the string
/// `"auto"` (any case). Parsing and resolution belong to core
/// ([`MaxZoom`]); this only maps Python types onto it with precise errors —
/// an out-of-range int is a `ValueError`, not pyo3's generic extraction
/// failure.
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

// --- functions -------------------------------------------------------------------

/// The #314 remedies as `convert()` spells them (#709).
const PY_REMEDIES: SpaceRemedies<'static> = SpaceRemedies {
    spill_dir: "spill_dir",
    keep_overview: "keep_overview",
};

signature_convert! {
fn convert(
    py: Python<'_>,
    input: &str,
    output: &str,
    args: &Bound<'_, pyo3::types::PyTuple>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let input_path = PathBuf::from(input);
    let output_path = PathBuf::from(output);

    let mut values = keywords(kwargs)?;
    if args.len() > config_keywords::CONVERT_POSITIONAL.len() {
        return Err(PyTypeError::new_err("convert received too many positional arguments"));
    }
    let positional = PyDict::new(py);
    for (key, value) in config_keywords::CONVERT_POSITIONAL.iter().zip(args.iter()) {
        if values.contains_key(*key) { return Err(PyTypeError::new_err(format!("multiple values for {key}"))); }
        positional.set_item(*key, value)?;
    }

    values.extend(keywords(Some(&positional))?);
    let (max_zoom, mut options) = tiles_config(values)?;
    // #709/#779: core's whole preflight, before `max_zoom="auto"` reads the
    // input; the facade runs it again.
    options.validate().map_err(tiles_error_to_py)?;
    // Resolves `auto` and writes the chosen zoom back into `levels`, which
    // is where the facade reads the declared range from.
    resolve_max_zoom_py(py, max_zoom, &mut options.convert, &output_path, || {
        ConvertSource::resolve_path(&input_path).map_err(ConvertError::from)
    })?;

    // #314/#709: core's free-space preflight, raised before the run starts
    // (so it is heard on the failure it predicts) and naming the kwargs.
    // Core also logs it, CLI-spelled, which reaches Python `logging` (#445).
    let input = TilesInput::Path(input_path);
    if let Some(msg) = intermediate_space_preflight(&input, &output_path, &options, PY_REMEDIES) {
        PyErr::warn(py, &py.get_type::<PyUserWarning>(), &CString::new(msg)?, 1)?;
    }

    // Release the GIL while the Rust pipelines run.
    let outcome = py.detach(|| convert_to_pmtiles(&input, &output_path, &options));
    match outcome.map_err(tiles_error_to_py)? {
        // The combined report (#439): the two halves the two-step chain's own
        // reports would carry, keyed like the CLI's `tiles --report` JSON.
        TilesOutcome::Converted(mut report) => {
            report.convert.source_losses = report.convert.source_losses
                .for_python()
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            report.export.source_losses = report.export.source_losses
                .for_python()
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
            report_dict_with_losses(py, &*report, &[
                (&report.convert.source_losses, Some("convert")),
                (&report.export.source_losses, Some("export")),
            ])
        },
        // Unreachable without a shard job, which this facade never sets.
        other => Err(PyRuntimeError::new_err(format!(
            "unexpected tiles outcome: {other:?}"
        ))),
    }
}
}

signature_overview! {
fn overview(
    py: Python<'_>,
    input: &Bound<'_, PyAny>,
    output: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let (max_zoom, mut options) = overview_config(keywords(kwargs)?)?;
    // #779: every rule that needs no input, before anything is read.
    options.validate().map_err(convert_error_to_py)?;

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
    let output_path = PathBuf::from(output);

    // #444: every cheap check has passed; now estimate `auto` over a
    // dedicated source (skipped for an explicit `gsds` ladder).
    resolve_max_zoom_py(py, max_zoom, &mut options, &output_path, || match &parsed {
        OverviewInput::Path(p) => ConvertSource::resolve_path(p).map_err(ConvertError::from),
        OverviewInput::List(inputs) => {
            ConvertSource::from_input_list(inputs).map_err(ConvertError::from)
        }
    })?;

    // Release the GIL while the Rust pipeline runs.
    let mut report = py
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

    report.source_losses = report.source_losses
                .for_python()
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    report_dict_with_losses(py, &report, &[(&report.source_losses, None)])
}
}

signature_export_pmtiles! {
fn export_pmtiles(
    py: Python<'_>,
    input: &str,
    output: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let options = export_config(keywords(kwargs)?)?;
    let input_path = PathBuf::from(input);
    let output_path = PathBuf::from(output);

    let mut report = py
        .detach(|| export_pmtiles_core(&input_path, &output_path, &options))
        .map_err(export_error_to_py)?;

    report.source_losses = report.source_losses
                .for_python()
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
    report_dict_with_losses(py, &report, &[(&report.source_losses, None)])
}
}

signature_validate! {
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
}

signature_decode! {
fn decode(
    py: Python<'_>,
    input: &str,
    output: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let values = keywords(kwargs)?;
    let mut defaults = config_keywords::command_defaults("decode");
    for key in values.keys() { if !defaults.contains_key(key) { return Err(config_err(format!("unknown keyword {key}"))); } }
    defaults.extend(values);
    let options = DecodeOptions {
        min_zoom: serde_json::from_value(defaults.remove("min_zoom").unwrap()).map_err(config_err)?,
        max_zoom: serde_json::from_value(defaults.remove("max_zoom").unwrap()).map_err(config_err)?,
        layer: serde_json::from_value(defaults.remove("layer").unwrap()).map_err(config_err)?,
    };
    let input_path = PathBuf::from(input);
    let output_path = PathBuf::from(output);
    let report = py
        .detach(|| decode_pmtiles(&input_path, &output_path, &options))
        .map_err(decode_error_to_py)?;

    // Keyed like the CLI's `decode --report` JSON (`DecodeReport`'s serde
    // names); `zoom_range` stays a tuple.
    report_dict(py, &report)
}
}

signature_pyramid! {
fn pyramid(
    py: Python<'_>,
    bands: Vec<String>,
    output: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    // Spec grammar, band-set rules and the input-exists check are core's,
    // shared with the CLI's `--band` (#445).
    let options = pyramid_config(keywords(kwargs)?)?;
    options.validate().map_err(pyramid_error_to_py)?;
    let bands = parse_bands(&bands).map_err(config_err)?;
    let output_path = PathBuf::from(output);

    let report = py
        .detach(|| build_pyramid(&bands, &output_path, &options))
        .map_err(pyramid_error_to_py)?;
    // The same shape as the CLI's `pyramid --report` JSON.
    report_dict(py, &report)
}
}

/// Native body of `tylertoo._defaults()`: every keyword default of every
/// function, keyed `{function: {keyword: default}}`, built from the core
/// `Default` impls (#779), also used to generate native signatures.
#[pyfunction]
fn _defaults(py: Python<'_>) -> PyResult<Py<PyDict>> {
    let out = PyDict::new(py);
    for name in ["convert", "overview", "export_pmtiles", "pyramid", "decode"] {
        out.set_item(
            name,
            pythonize::pythonize(py, &config_keywords::command_defaults(name))?,
        )?;
    }
    Ok(out.unbind())
}

/// The compiled extension, imported as `tylertoo._tylertoo`.
///
/// `python/tylertoo/__init__.py` reexports the native functions directly.
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
    m.add("ConfigError", m.py().get_type::<ConfigError>())?;
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    m.add_function(wrap_pyfunction!(overview, m)?)?;
    m.add_function(wrap_pyfunction!(export_pmtiles, m)?)?;
    m.add_function(wrap_pyfunction!(validate, m)?)?;
    m.add_function(wrap_pyfunction!(decode, m)?)?;
    m.add_function(wrap_pyfunction!(pyramid, m)?)?;
    m.add_function(wrap_pyfunction!(_defaults, m)?)?;
    Ok(())
}

#[cfg(test)]
mod config_parity {
    use super::*;
    use clap::Parser;
    use serde_json::{json, Value};
    use tylertoo_core::overview::config::cli;
    use tylertoo_core::overview::convert::LevelPlan;

    fn native_keywords(value: Value) -> serde_json::Map<String, Value> {
        Python::attach(|py| {
            let value = pythonize::pythonize(py, &value).unwrap();
            keywords(Some(value.cast::<PyDict>().unwrap())).unwrap()
        })
    }

    #[test]
    fn overview_cli_python_core_configs_are_equal() {
        let defaults = cli::OverviewArgs::parse_from(["overview", "in", "out"]);
        let cli_defaults = cli::overview_options(&defaults).unwrap();
        let (_, python) = overview_config(native_keywords(json!({}))).unwrap();
        let core = ConvertOptions::default()
            .with_generalization(&Default::default(), false)
            .unwrap();
        let expected = serde_json::to_value(core.validate().unwrap().options()).unwrap();
        assert_eq!(
            serde_json::to_value(cli_defaults.validate().unwrap().options()).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::to_value(python.validate().unwrap().options()).unwrap(),
            expected
        );
        let args = cli::OverviewArgs::parse_from([
            "overview",
            "in",
            "out",
            "--min-zoom",
            "1",
            "--max-zoom",
            "3",
            "--row-group-size",
            "1234",
            "--no-cascade",
            "--drop-lines",
            "--sort-direction",
            "asc",
            "--include-property",
            "id",
        ]);
        let cli = cli::overview_options(&args).unwrap();
        let (_, python) = overview_config(native_keywords(json!({"min_zoom": 1, "max_zoom": 3, "row_group_size": 1234, "cascade": false, "drop_lines": true, "sort_direction": "asc", "include_properties": ["id"]}))).unwrap();
        let mut core = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 3,
            },
            max_row_group_size: 1234,
            ..Default::default()
        };
        core.simplify.cascade = false;
        core.density.kinds.lines = true;
        core.assign.sort_direction = tylertoo_core::overview::assign::SortDirection::Asc;
        core.properties.include = Some(vec!["id".into()]);
        let core = serde_json::to_value(core.validate().unwrap().options()).unwrap();
        assert_eq!(
            serde_json::to_value(cli.validate().unwrap().options()).unwrap(),
            core
        );
        assert_eq!(
            serde_json::to_value(python.validate().unwrap().options()).unwrap(),
            core
        );
    }

    #[test]
    fn tiles_cli_python_core_configs_are_equal() {
        let defaults = cli::TilesArgs::parse_from(["tiles", "in", "out"]);
        let cli_defaults = cli::tiles_options(&defaults).unwrap();
        let (_, python) = tiles_config(native_keywords(json!({}))).unwrap();
        let core = {
            let mut options = TilesOptions::default();
            options.convert = options
                .convert
                .with_generalization(&Default::default(), false)
                .unwrap();
            options
        };
        let expected = serde_json::to_value(core.validate().unwrap().options()).unwrap();
        assert_eq!(
            serde_json::to_value(cli_defaults.validate().unwrap().options()).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::to_value(python.validate().unwrap().options()).unwrap(),
            expected
        );
        let args = cli::TilesArgs::parse_from([
            "tiles",
            "in",
            "out",
            "--verbatim",
            "--point-thinning",
            "2",
            "--extent",
            "8192",
            "--max-compressed-tile-size",
            "1M",
            "--layer-name",
            "places",
        ]);
        let cli = cli::tiles_options(&args).unwrap();
        let (_, python) = tiles_config(native_keywords(json!({"verbatim": true, "point_thinning": 2.0, "extent": 8192, "max_compressed_tile_size": 1048576, "layer_name": "places"}))).unwrap();
        let mut core = TilesOptions::default();
        core.convert = core.convert.verbatim();
        core.convert.assign.point_thinning = 2.0;
        core.export = core.export.verbatim();
        core.export.extent = 8192;
        core.export.compressed_tile_size_limit = Some(1048576);
        core.layer_name = Some("places".into());
        let core = serde_json::to_value(core.validate().unwrap().options()).unwrap();
        assert_eq!(
            serde_json::to_value(cli.validate().unwrap().options()).unwrap(),
            core
        );
        assert_eq!(
            serde_json::to_value(python.validate().unwrap().options()).unwrap(),
            core
        );
    }

    #[test]
    fn export_cli_python_core_configs_are_equal() {
        let defaults = cli::ExportPmtilesArgs::parse_from(["export-pmtiles", "in", "out"]);
        let cli_defaults = cli::export_options(&defaults).unwrap();
        let python = export_config(native_keywords(json!({}))).unwrap();
        let core = ExportOptions::default();
        let expected = serde_json::to_value(core.validate().unwrap().options()).unwrap();
        assert_eq!(
            serde_json::to_value(cli_defaults.validate().unwrap().options()).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::to_value(python.validate().unwrap().options()).unwrap(),
            expected
        );
        let args = cli::ExportPmtilesArgs::parse_from([
            "export-pmtiles",
            "in",
            "out",
            "--extent",
            "8192",
            "--tile-buffer",
            "4",
            "--name",
            "Example",
            "--include-property",
            "rank",
            "--feature-order",
            "rank:desc",
            "--no-simple-clip-fastpath",
        ]);
        let cli = cli::export_options(&args).unwrap();
        let python = export_config(native_keywords(json!({"extent": 8192, "tile_buffer": 4, "name": "Example", "include_properties": ["rank"], "feature_order": "rank:desc", "simple_clip_fastpath": false}))).unwrap();
        let mut core = ExportOptions {
            extent: 8192,
            tile_buffer: 4,
            simple_clip_fastpath: false,
            feature_order: "rank:desc".parse().unwrap(),
            ..Default::default()
        };
        core.metadata.name = Some("Example".into());
        core.properties.include = Some(vec!["rank".into()]);
        let core = serde_json::to_value(core.validate().unwrap().options()).unwrap();
        assert_eq!(
            serde_json::to_value(cli.validate().unwrap().options()).unwrap(),
            core
        );
        assert_eq!(
            serde_json::to_value(python.validate().unwrap().options()).unwrap(),
            core
        );
    }

    #[test]
    fn pyramid_cli_python_core_configs_are_equal() {
        let defaults =
            cli::PyramidArgs::parse_from(["pyramid", "out", "--band", "0-3:never-read.parquet"]);
        let cli_defaults = cli::pyramid_options(&defaults);
        let python = pyramid_config(native_keywords(json!({}))).unwrap();
        let core = tylertoo_core::pyramid::PyramidOptions::default();
        let expected = serde_json::to_value(core.validate().unwrap().options()).unwrap();
        assert_eq!(
            serde_json::to_value(cli_defaults.validate().unwrap().options()).unwrap(),
            expected
        );
        assert_eq!(
            serde_json::to_value(python.validate().unwrap().options()).unwrap(),
            expected
        );
        let args = cli::PyramidArgs::parse_from([
            "pyramid",
            "out",
            "--band",
            "0-3:never-read.parquet",
            "--generalize",
            "--max-tile-size",
            "1M",
            "--extent",
            "8192",
            "--name",
            "Bands",
            "--allow-missing-zooms",
        ]);
        let cli = cli::pyramid_options(&args);
        let python = pyramid_config(native_keywords(json!({"generalize": true, "tile_size_limit": 1048576, "extent": 8192, "name": "Bands", "allow_missing_zooms": true}))).unwrap();
        let mut core = tylertoo_core::pyramid::PyramidOptions::new(true);
        core.export.tile_size_limit = Some(1048576);
        core.export.extent = 8192;
        core.metadata.name = Some("Bands".into());
        core.allow_missing_zooms = true;
        let core = serde_json::to_value(core.validate().unwrap().options()).unwrap();
        assert_eq!(
            serde_json::to_value(cli.validate().unwrap().options()).unwrap(),
            core
        );
        assert_eq!(
            serde_json::to_value(python.validate().unwrap().options()).unwrap(),
            core
        );
    }
}
