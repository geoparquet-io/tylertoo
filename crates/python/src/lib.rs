//! Python bindings for tylertoo
//!
//! This module exposes the tylertoo-core functionality to Python via pyo3.
//!
//! Pure translation (#779): each keyword argument maps onto a core option,
//! every default comes from the core `Default` impls ([`defaults`], exposed as
//! `tylertoo._defaults()`, which `__init__.py` reads its defaults from), and
//! every option rule is core's ([`TilesOptions::validate`] and friends),
//! raised as `tylertoo.ConfigError`. The keyword groups shared between
//! functions are declared once each and extracted from the call's keyword
//! arguments: the tuning and export knobs are core's own
//! ([`ConvertKnobs`], [`ExportKnobs`]), deserialized as they are, so
//! `convert()` takes every tuning knob `overview()` does and every export
//! knob `export_pmtiles()` does, as `tiles` does on the CLI.

use pyo3::exceptions::{
    PyException, PyMemoryError, PyRuntimeError, PyTypeError, PyUserWarning, PyValueError,
};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};
use std::ffi::CString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tylertoo_core::decode::{decode_pmtiles, DecodeError, DecodeOptions};
use tylertoo_core::input_set::ConvertSource;
use tylertoo_core::overview::auto_zoom::MaxZoom;
use tylertoo_core::overview::check::validate_file;
use tylertoo_core::overview::convert::{
    convert_to_overviews, convert_to_overviews_sources, ConvertError, ConvertOptions, LevelPlan,
};
use tylertoo_core::overview::export::{
    export_pmtiles as export_pmtiles_core, ExportError, ExportOptions,
};
use tylertoo_core::overview::knobs::{ConvertKnobs, ExportKnobs};
use tylertoo_core::overview::level::Mode;
use tylertoo_core::overview::properties::PropertySelection;
use tylertoo_core::overview::tiles::{
    convert_to_pmtiles, intermediate_space_preflight, SpaceRemedies, TilesError, TilesInput,
    TilesOptions, TilesOutcome, TILES_DEFAULT_MAX_ZOOM,
};
use tylertoo_core::pyramid::{build_pyramid, parse_bands, PyramidOptions};
use tylertoo_core::shard::TileRange;

pyo3::create_exception!(
    _tylertoo,
    ConfigError,
    PyValueError,
    "An option value or combination refused before any input is read. The \
     rules are the core library's, the same ones the CLI enforces."
);

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

/// A keyword argument that does not translate (an unknown spelling), as
/// `tylertoo.ConfigError`.
fn config_err(message: impl std::fmt::Display) -> PyErr {
    ConfigError::new_err(message.to_string())
}

/// Parse a string keyword through the core type's `FromStr`.
fn parse_kwarg<T: std::str::FromStr>(name: &str, value: &str) -> PyResult<T>
where
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|e| config_err(format!("{name}: {e}")))
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

// --- keyword groups ----------------------------------------------------------

/// The zoom range, ladder and box `overview()` and `convert()` take besides
/// the shared tuning knobs ([`ConvertKnobs`]), which each command owns.
#[derive(FromPyObject, IntoPyObject)]
#[pyo3(from_item_all)]
struct LevelKwargs {
    min_zoom: u8,
    gsds: Option<Vec<f64>>,
    bbox: Option<(f64, f64, f64, f64)>,
}

impl Default for LevelKwargs {
    fn default() -> Self {
        let min_zoom = match ConvertOptions::default().levels {
            LevelPlan::ZoomRange { min_zoom, .. } => min_zoom,
            LevelPlan::Gsds(_) => 0,
        };
        Self {
            min_zoom,
            gsds: None,
            bbox: None,
        }
    }
}

impl LevelKwargs {
    /// The level plan: an explicit GSD ladder, or the zoom range with
    /// `max_zoom`'s placeholder (#444: `auto` is resolved later).
    fn levels(&self, max_zoom: MaxZoom) -> LevelPlan {
        match &self.gsds {
            Some(gsds) => LevelPlan::Gsds(gsds.clone()),
            None => LevelPlan::ZoomRange {
                min_zoom: self.min_zoom,
                max_zoom: max_zoom.plan_zoom(),
            },
        }
    }
}

/// The convert options `overview()` and `convert()` describe: the call's
/// keywords as core's [`ConvertKnobs`], which builds the options (#779).
fn convert_options(
    kwargs: Option<&Bound<'_, PyDict>>,
    mode: Mode,
    max_zoom: MaxZoom,
    cogp_compat: bool,
) -> PyResult<ConvertOptions> {
    let tuning: ConvertKnobs = knobs(kwargs)?;
    let level: LevelKwargs = group(kwargs)?;
    let bbox = level
        .bbox
        .map(|(xmin, ymin, xmax, ymax)| [xmin, ymin, xmax, ymax]);
    tuning
        .convert_options(mode, level.levels(max_zoom), bbox, cogp_compat)
        .map_err(convert_error_to_py)
}

/// The archive's `name`, `description` and `attribution`.
#[derive(FromPyObject, IntoPyObject, Default)]
#[pyo3(from_item_all)]
struct PublicationKwargs {
    name: Option<String>,
    description: Option<String>,
    attribution: Option<String>,
}

impl PublicationKwargs {
    fn metadata(self) -> tylertoo_core::TilesetMetadata {
        let mut metadata = tylertoo_core::TilesetMetadata::new();
        metadata.name = self.name;
        metadata.description = self.description;
        metadata.attribution = self.attribution;
        metadata
    }
}

/// The export-side property selection of `export_pmtiles()` and
/// `pyramid()` (#386).
#[derive(FromPyObject, IntoPyObject, Default)]
#[pyo3(from_item_all)]
struct PropertyKwargs {
    include_properties: Option<Vec<String>>,
    exclude_properties: Option<Vec<String>>,
    exclude_all_properties: bool,
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

/// Extract one keyword group from the call's keyword arguments.
fn group<'py, T: for<'a> FromPyObject<'a, 'py, Error = PyErr>>(
    kwargs: Option<&Bound<'py, PyDict>>,
) -> PyResult<T> {
    match kwargs {
        Some(kwargs) => kwargs.extract(),
        None => Err(PyTypeError::new_err("missing keyword arguments")),
    }
}

/// Deserialize one of core's knob groups ([`ConvertKnobs`], [`ExportKnobs`])
/// from the call's keyword arguments: its serde field names are the keyword
/// names, so no keyword is mapped here by hand (#779). Keywords of other
/// groups are left alone, as [`group`] leaves them.
///
/// An `os.PathLike` is taken wherever a `str` is, as everywhere else in this
/// API. A wrong type (`True` for a size, a `str` for a list of names) is a
/// `TypeError`, a value core refuses (`size_valve="squash"`) a
/// `tylertoo.ConfigError`; both name the keyword.
fn knobs<T: serde::de::DeserializeOwned>(kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<T> {
    let kwargs = kwargs.ok_or_else(|| PyTypeError::new_err("missing keyword arguments"))?;
    let py = kwargs.py();
    let fspath = py.import("os")?.getattr("fspath")?;
    let plain = PyDict::new(py);
    for (key, value) in kwargs.iter() {
        let value = if !value.is_instance_of::<PyString>() && value.hasattr("__fspath__")? {
            fspath.call1((value,))?
        } else {
            value
        };
        plain.set_item(key, value)?;
    }
    let mut de = pythonize::Depythonizer::from_object(plain.as_any());
    serde_path_to_error::deserialize(&mut de).map_err(|e| {
        let keyword = e.path().to_string();
        let err = PyErr::from(e.into_inner());
        let message = format!("{keyword}: {}", err.value(py));
        // pythonize raises serde's own messages as a bare `Exception`; a
        // pyo3 extraction error (an overflow, a non-str) keeps its type.
        if !err.get_type(py).is(py.get_type::<PyException>()) {
            return PyErr::from_type(err.get_type(py), message);
        }
        let detail = message[keyword.len() + 2..].to_string();
        if ["invalid type", "missing field", "invalid length"]
            .iter()
            .any(|p| detail.starts_with(p))
        {
            PyTypeError::new_err(message)
        } else {
            config_err(message)
        }
    })
}

/// The `tile_range` keyword (#498) as core's [`TileRange`].
fn tile_range_kwarg(kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Option<TileRange>> {
    item::<Option<String>>(kwargs, "tile_range")?
        .map(|r| TileRange::parse(&r))
        .transpose()
        .map_err(config_err)
}

/// One keyword the functions below take on their own.
fn item<'py, T: for<'a> FromPyObject<'a, 'py, Error = PyErr>>(
    kwargs: Option<&Bound<'py, PyDict>>,
    key: &str,
) -> PyResult<T> {
    let value = kwargs
        .and_then(|k| k.get_item(key).transpose())
        .transpose()?
        .ok_or_else(|| PyTypeError::new_err(format!("missing keyword argument {key:?}")))?;
    value
        .extract()
        .map_err(|e: PyErr| PyTypeError::new_err(format!("{key}: {e}")))
}

// --- errors --------------------------------------------------------------------

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
        ConvertError::Pass1MemoryFloorExceeded { .. } => PyMemoryError::new_err(e.to_string()),
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

// --- max_zoom --------------------------------------------------------------------

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

/// The `max_zoom` keyword, as [`MaxZoom`].
fn max_zoom_kwarg(kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<MaxZoom> {
    let value = kwargs
        .and_then(|k| k.get_item("max_zoom").transpose())
        .transpose()?
        .ok_or_else(|| PyTypeError::new_err("missing keyword argument \"max_zoom\""))?;
    extract_max_zoom(&value)
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

// --- functions -------------------------------------------------------------------

/// The #314 remedies as `convert()` spells them (#709).
const PY_REMEDIES: SpaceRemedies<'static> = SpaceRemedies {
    spill_dir: "spill_dir",
    keep_overview: "keep_overview",
};

/// Native body of `tylertoo.convert()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
///
/// Delegates the whole chain to core's tiles facade
/// ([`tylertoo_core::overview::tiles::convert_to_pmtiles`], #439), so the
/// policy — intermediate lifecycle and spill precedence, free-space
/// preflight, every option rule, the #380 declaration, the combined report —
/// can never drift from `tylertoo tiles`.
#[pyfunction]
#[pyo3(signature = (input, output, **kwargs))]
fn convert(
    py: Python<'_>,
    input: &str,
    output: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let input_path = PathBuf::from(input);
    let output_path = PathBuf::from(output);

    let max_zoom = max_zoom_kwarg(kwargs)?;
    // Overviews for PMTiles are always duplicating. #444: built with
    // `auto`'s placeholder, then resolved below.
    let convert = convert_options(kwargs, Mode::Duplicating, max_zoom, false)?;
    let verbatim: bool = item(kwargs, "verbatim")?;
    let export: ExportKnobs = knobs(kwargs)?;
    let publication: PublicationKwargs = group(kwargs)?;
    let mut options = TilesOptions {
        convert,
        export: ExportOptions {
            metadata: publication.metadata(),
            tile_range: tile_range_kwarg(kwargs)?,
            ..export.apply(TilesOptions::export_defaults(verbatim))
        },
        // `None` lets core derive the layer name from the input.
        layer_name: item(kwargs, "layer_name")?,
        keep_overview: item(kwargs, "keep_overview")?,
        // Sharded builds (#498) are a CLI/Rust-API feature.
        shard: None,
    };
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
        TilesOutcome::Converted(report) => report_dict(py, &*report),
        // Unreachable without a shard job, which this facade never sets.
        other => Err(PyRuntimeError::new_err(format!(
            "unexpected tiles outcome: {other:?}"
        ))),
    }
}

/// Native body of `tylertoo.overview()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
#[pyfunction]
#[pyo3(signature = (input, output, **kwargs))]
fn overview(
    py: Python<'_>,
    input: &Bound<'_, PyAny>,
    output: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let mode: String = item(kwargs, "mode")?;
    let mode: Mode = parse_kwarg("mode", &mode)?;
    let max_zoom = max_zoom_kwarg(kwargs)?;
    let mut options = convert_options(kwargs, mode, max_zoom, item(kwargs, "cogp_compat")?)?;
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

    report_dict(py, &report)
}

/// Native body of `tylertoo.export_pmtiles()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
#[pyfunction]
#[pyo3(signature = (input, output, **kwargs))]
fn export_pmtiles(
    py: Python<'_>,
    input: &str,
    output: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let export: ExportKnobs = knobs(kwargs)?;
    let publication: PublicationKwargs = group(kwargs)?;
    let properties: PropertyKwargs = group(kwargs)?;
    let options = ExportOptions {
        layer_name: item(kwargs, "layer_name")?,
        // #380: the declared minimum; None keeps the coarsest level's zoom.
        min_zoom: item(kwargs, "min_zoom")?,
        properties: property_selection(
            properties.include_properties,
            properties.exclude_properties,
            properties.exclude_all_properties,
        ),
        tile_range: tile_range_kwarg(kwargs)?,
        zoom_ceiling: item(kwargs, "zoom_ceiling")?,
        spill_dir: item(kwargs, "spill_dir")?,
        metadata: publication.metadata(),
        ..export.apply(ExportOptions::default())
    };
    let input_path = PathBuf::from(input);
    let output_path = PathBuf::from(output);

    let report = py
        .detach(|| export_pmtiles_core(&input_path, &output_path, &options))
        .map_err(export_error_to_py)?;

    report_dict(py, &report)
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
#[pyo3(signature = (input, output, **kwargs))]
fn decode(
    py: Python<'_>,
    input: &str,
    output: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    let options = DecodeOptions {
        min_zoom: item(kwargs, "min_zoom")?,
        max_zoom: item(kwargs, "max_zoom")?,
        layer: item(kwargs, "layer")?,
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

/// Native body of `tylertoo.pyramid()`.
///
/// The documented, typed Python API is `python/tylertoo/__init__.py`; it
/// passes every argument here by keyword.
#[pyfunction]
#[pyo3(signature = (bands, output, **kwargs))]
fn pyramid(
    py: Python<'_>,
    bands: Vec<String>,
    output: &str,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyDict>> {
    refresh_log_levels();
    // Spec grammar, band-set rules and the input-exists check are core's,
    // shared with the CLI's `--band` (#445).
    let bands = parse_bands(&bands).map_err(PyValueError::new_err)?;
    let export: ExportKnobs = knobs(kwargs)?;
    let publication: PublicationKwargs = group(kwargs)?;
    let properties: PropertyKwargs = group(kwargs)?;
    // The default tiles each GeoParquet band verbatim (an aggregate band
    // must keep every feature); `generalize` runs the normal ladder, as the
    // CLI's `--generalize` does. Core owns both defaults.
    let base = PyramidOptions::new(item(kwargs, "generalize")?);
    let options = PyramidOptions {
        export: ExportOptions {
            properties: property_selection(
                properties.include_properties,
                properties.exclude_properties,
                properties.exclude_all_properties,
            ),
            ..export.apply(base.export)
        },
        work_dir: item(kwargs, "work_dir")?,
        allow_missing_zooms: item(kwargs, "allow_missing_zooms")?,
        metadata: publication.metadata(),
        ..base
    };
    let output_path = PathBuf::from(output);

    let report = py
        .detach(|| build_pyramid(&bands, &output_path, &options))
        .map_err(pyramid_error_to_py)?;
    // The same shape as the CLI's `pyramid --report` JSON.
    report_dict(py, &report)
}

/// Native body of `tylertoo._defaults()`: every keyword default of every
/// function, keyed `{function: {keyword: default}}`, built from the core
/// `Default` impls (#779). `__init__.py` reads its defaults from this, and
/// the parity tests check the CLI's against it.
#[pyfunction]
fn _defaults(py: Python<'_>) -> PyResult<Py<PyDict>> {
    let out = PyDict::new(py);
    let merged = |groups: &[Bound<'_, PyAny>]| -> PyResult<Bound<'_, PyDict>> {
        let d = PyDict::new(py);
        for g in groups {
            d.update(g.cast::<pyo3::types::PyMapping>()?)?;
        }
        Ok(d)
    };
    let tuning = || -> PyResult<Bound<'_, PyAny>> {
        let d = merged(&[
            pythonize::pythonize(py, &ConvertKnobs::default())?,
            LevelKwargs::default().into_pyobject(py)?.into_any(),
        ])?;
        Ok(d.into_any())
    };
    let export = || pythonize::pythonize(py, &ExportKnobs::default());
    let publication = || PublicationKwargs::default().into_pyobject(py);
    let properties = || PropertyKwargs::default().into_pyobject(py);

    let overview_max = match ConvertOptions::default().levels {
        LevelPlan::ZoomRange { max_zoom, .. } => max_zoom,
        LevelPlan::Gsds(_) => 0,
    };
    let overview = merged(&[tuning()?.into_any()])?;
    overview.set_item("mode", ConvertOptions::default().mode.as_str())?;
    overview.set_item("max_zoom", overview_max)?;
    overview.set_item("cogp_compat", ConvertOptions::default().cogp_compat_key)?;
    out.set_item("overview", overview)?;

    let convert = merged(&[
        tuning()?.into_any(),
        export()?.into_any(),
        publication()?.into_any(),
    ])?;
    let tiles = TilesOptions::default();
    convert.set_item("max_zoom", TILES_DEFAULT_MAX_ZOOM)?;
    convert.set_item("layer_name", tiles.layer_name)?;
    convert.set_item("keep_overview", tiles.keep_overview)?;
    convert.set_item("tile_range", None::<String>)?;
    out.set_item("convert", convert)?;

    let e = ExportOptions::default();
    let export_pmtiles = merged(&[
        export()?.into_any(),
        publication()?.into_any(),
        properties()?.into_any(),
    ])?;
    export_pmtiles.set_item("layer_name", e.layer_name)?;
    export_pmtiles.set_item("min_zoom", e.min_zoom)?;
    export_pmtiles.set_item("tile_range", None::<String>)?;
    export_pmtiles.set_item("zoom_ceiling", e.zoom_ceiling)?;
    export_pmtiles.set_item("spill_dir", e.spill_dir)?;
    out.set_item("export_pmtiles", export_pmtiles)?;

    let p = PyramidOptions::default();
    let pyramid = merged(&[
        export()?.into_any(),
        publication()?.into_any(),
        properties()?.into_any(),
    ])?;
    pyramid.set_item("generalize", false)?;
    pyramid.set_item("work_dir", p.work_dir)?;
    pyramid.set_item("allow_missing_zooms", p.allow_missing_zooms)?;
    out.set_item("pyramid", pyramid)?;

    let d = DecodeOptions::default();
    let decode = PyDict::new(py);
    decode.set_item("min_zoom", d.min_zoom)?;
    decode.set_item("max_zoom", d.max_zoom)?;
    decode.set_item("layer", d.layer)?;
    out.set_item("decode", decode)?;
    Ok(out.unbind())
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
