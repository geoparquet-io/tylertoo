//! Python bindings for gpq-tiles
//!
//! This module exposes the gpq-tiles-core functionality to Python via pyo3.

use gpq_tiles_core::pipeline::{
    generate_tiles_to_writer, generate_tiles_to_writer_with_progress, ProgressEvent, TilerConfig,
};
use gpq_tiles_core::{Compression, DropDensity, PropertyFilter, StreamingPmtilesWriter};
use pyo3::prelude::*;
use pyo3::types::PyDict;
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

/// gpq_tiles: Fast GeoParquet to PMTiles converter
///
/// This module provides Python bindings for the gpq-tiles Rust library.
#[pymodule]
fn gpq_tiles(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(convert, m)?)?;
    Ok(())
}
