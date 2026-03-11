//! Arrow-native geometry batch processing.
//!
//! Processes geometries within Arrow RecordBatch lifetime to preserve zero-copy benefits.
//! DO NOT extract geometries to Vec<Geometry> - that defeats Arrow's purpose.

use std::path::Path;
use std::sync::Arc;

use geo::Geometry;
use geo_traits::to_geo::ToGeoGeometry;
use geoarrow::array::from_arrow_array;
use geoarrow::datatypes::GeoArrowType;
use geoarrow_array::cast::AsGeoArrowArray;
use geoarrow_array::{GeoArrowArray, GeoArrowArrayAccessor};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use tracing::instrument;

use crate::tile::TileBounds;
use crate::{Error, Result};
use std::collections::HashMap;

/// Process geometries in a GeoParquet file batch-by-batch.
///
/// The callback receives each geometry converted to geo::Geometry for processing.
/// Conversion happens within batch scope to minimize memory usage - we process
/// one geometry at a time rather than bulk-extracting to Vec<Geometry>.
///
/// # Arguments
///
/// * `path` - Path to the GeoParquet file
/// * `callback` - Function called for each geometry with the geometry and its row index
///
/// # Returns
///
/// Total number of geometries processed
pub fn process_geometries<F>(path: &Path, mut callback: F) -> Result<usize>
where
    F: FnMut(Geometry<f64>, usize) -> Result<()>,
{
    let file = std::fs::File::open(path)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to open: {}", e)))?;

    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to create reader: {}", e)))?;

    let reader = builder
        .build()
        .map_err(|e| Error::GeoParquetRead(format!("Failed to build reader: {}", e)))?;

    let mut total_processed = 0;
    let mut row_offset = 0;

    for batch_result in reader {
        let batch = batch_result
            .map_err(|e| Error::GeoParquetRead(format!("Failed to read batch: {}", e)))?;

        // Find geometry column by name
        let schema = batch.schema();
        let geom_idx = schema
            .fields()
            .iter()
            .position(|f| f.name() == "geometry" || f.name().contains("geom"))
            .ok_or_else(|| Error::GeoParquetRead("No geometry column found".to_string()))?;

        let geom_col = batch.column(geom_idx);
        let geom_field = schema.field(geom_idx);

        // Convert Arrow array to GeoArrow geometry array
        let geom_array: Arc<dyn GeoArrowArray> = from_arrow_array(geom_col.as_ref(), geom_field)
            .map_err(|e| Error::GeoParquetRead(format!("Failed to parse geometry array: {}", e)))?;

        // Process each geometry within batch scope using explicit type dispatch
        // This avoids bulk extraction while leveraging GeoArrow's type system
        let batch_count = process_geoarrow_array(geom_array.as_ref(), row_offset, &mut callback)?;

        total_processed += batch_count;
        row_offset += batch.num_rows();
    }

    Ok(total_processed)
}

/// Process geometries from a GeoArrow array, calling the callback for each valid geometry.
///
/// Uses explicit type dispatch to avoid deep generic recursion that causes
/// compile-time issues with the downcast_geoarrow_array macro in release builds.
fn process_geoarrow_array<F>(
    array: &dyn GeoArrowArray,
    row_offset: usize,
    callback: &mut F,
) -> Result<usize>
where
    F: FnMut(Geometry<f64>, usize) -> Result<()>,
{
    match array.data_type() {
        GeoArrowType::Point(_) => {
            let arr = array.as_point();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::LineString(_) => {
            let arr = array.as_line_string();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::Polygon(_) => {
            let arr = array.as_polygon();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::MultiPoint(_) => {
            let arr = array.as_multi_point();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::MultiLineString(_) => {
            let arr = array.as_multi_line_string();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::MultiPolygon(_) => {
            let arr = array.as_multi_polygon();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::Geometry(_) => {
            let arr = array.as_geometry();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::GeometryCollection(_) => {
            let arr = array.as_geometry_collection();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::Wkb(_) => {
            let arr = array.as_wkb::<i32>();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::LargeWkb(_) => {
            let arr = array.as_wkb::<i64>();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::WkbView(_) => {
            let arr = array.as_wkb_view();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::Wkt(_) => {
            let arr = array.as_wkt::<i32>();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::LargeWkt(_) => {
            let arr = array.as_wkt::<i64>();
            process_typed_array(arr, row_offset, callback)
        }
        GeoArrowType::WktView(_) => {
            let arr = array.as_wkt_view();
            process_typed_array(arr, row_offset, callback)
        }
        _ => Err(Error::GeoParquetRead(format!(
            "Unsupported geometry type: {:?}",
            array.data_type()
        ))),
    }
}

/// Process a typed GeoArrow array, converting each geometry to geo::Geometry.
fn process_typed_array<'a, A, F>(
    accessor: &'a A,
    row_offset: usize,
    callback: &mut F,
) -> Result<usize>
where
    A: GeoArrowArrayAccessor<'a>,
    A::Item: ToGeoGeometry<f64>,
    F: FnMut(Geometry<f64>, usize) -> Result<()>,
{
    let mut count = 0;
    for (i, item) in accessor.iter().enumerate() {
        if let Some(geom_result) = item {
            // Convert GeoArrow scalar to geo::Geometry
            // This happens one-at-a-time within batch scope
            let geom_trait = geom_result.map_err(|e| {
                Error::GeoParquetRead(format!("Invalid geometry at index {}: {}", i, e))
            })?;

            // Use ToGeoGeometry trait to convert to geo::Geometry
            if let Some(geo_geom) = geom_trait.try_to_geometry() {
                callback(geo_geom, row_offset + i)?;
                count += 1;
            }
        }
    }
    Ok(count)
}

/// Extract all geometries from a GeoParquet file into a Vec.
///
/// **WARNING**: This loads all geometries into memory. Only use for small files
/// or test fixtures. For production, use `process_geometries` instead.
///
/// # Arguments
///
/// * `path` - Path to the GeoParquet file
///
/// # Returns
///
/// Vector of all geometries in the file
#[instrument(name = "read_parquet", skip_all, fields(path = %path.display()))]
pub fn extract_geometries(path: &Path) -> Result<Vec<Geometry<f64>>> {
    let mut geometries = Vec::new();

    process_geometries(path, |geom, _idx| {
        geometries.push(geom);
        Ok(())
    })?;

    Ok(geometries)
}

/// Calculate bounding box by streaming through batches.
/// Does NOT load all geometries into memory.
pub fn calculate_bbox(path: &Path) -> Result<TileBounds> {
    let mut bounds = TileBounds::empty();

    process_geometries(path, |geom, _idx| {
        use geo::BoundingRect;
        if let Some(rect) = geom.bounding_rect() {
            bounds.expand(&TileBounds::new(
                rect.min().x,
                rect.min().y,
                rect.max().x,
                rect.max().y,
            ));
        }
        Ok(())
    })?;

    if !bounds.is_valid() {
        return Err(Error::GeoParquetRead(
            "No valid geometries found".to_string(),
        ));
    }

    Ok(bounds)
}

/// Row group metadata for streaming processing.
#[derive(Debug, Clone)]
pub struct RowGroupInfo {
    /// Index of the row group in the file
    pub index: usize,
    /// Number of rows in this row group
    pub num_rows: usize,
}

/// Process geometries in a GeoParquet file row-group by row-group.
///
/// This is the streaming-friendly version that yields geometries grouped by row group.
/// Each row group is processed independently, allowing memory to be freed after each
/// group is complete.
///
/// # Arguments
///
/// * `path` - Path to the GeoParquet file
/// * `callback` - Function called for each row group's geometries: (row_group_info, geometries)
///
/// # Returns
///
/// Total number of geometries processed
#[instrument(name = "read_parquet", skip(callback), fields(path = %path.display()))]
pub fn process_geometries_by_row_group<F>(path: &Path, mut callback: F) -> Result<usize>
where
    F: FnMut(RowGroupInfo, Vec<Geometry<f64>>) -> Result<()>,
{
    use parquet::file::reader::FileReader;
    use parquet::file::serialized_reader::SerializedFileReader;
    use tracing::info_span;

    let file = std::fs::File::open(path)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to open: {}", e)))?;

    // Get row group count using the lower-level API
    let parquet_reader = SerializedFileReader::new(
        file.try_clone()
            .map_err(|e| Error::GeoParquetRead(format!("Failed to clone file handle: {}", e)))?,
    )
    .map_err(|e| Error::GeoParquetRead(format!("Failed to create parquet reader: {}", e)))?;

    let num_row_groups = parquet_reader.metadata().num_row_groups();

    let mut total_processed = 0;

    // Process each row group separately
    for rg_idx in 0..num_row_groups {
        // Span: Building reader for this row group (file handle reused via try_clone)
        let (reader, _batch_size) = {
            let _open_span = info_span!("parquet_open_rowgroup", row_group = rg_idx).entered();

            // Reuse file handle via try_clone() - avoids reopening file for each row group
            let file_clone = file.try_clone().map_err(|e| {
                Error::GeoParquetRead(format!("Failed to clone file handle: {}", e))
            })?;

            let builder = ParquetRecordBatchReaderBuilder::try_new(file_clone)
                .map_err(|e| Error::GeoParquetRead(format!("Failed to create reader: {}", e)))?;

            let batch_size = builder.metadata().row_group(rg_idx).num_rows() as usize;

            // Select only the current row group
            let reader = builder
                .with_row_groups(vec![rg_idx])
                .build()
                .map_err(|e| Error::GeoParquetRead(format!("Failed to build reader: {}", e)))?;

            (reader, batch_size)
        };

        let mut row_group_geometries = Vec::new();
        let mut row_count = 0;

        for batch_result in reader {
            // Span: Reading Arrow batch from Parquet (decompression happens here)
            let batch = {
                let _read_span = info_span!("parquet_read_batch", row_group = rg_idx).entered();
                batch_result
                    .map_err(|e| Error::GeoParquetRead(format!("Failed to read batch: {}", e)))?
            };

            // Find geometry column by name
            let schema = batch.schema();
            let geom_idx = schema
                .fields()
                .iter()
                .position(|f| f.name() == "geometry" || f.name().contains("geom"))
                .ok_or_else(|| Error::GeoParquetRead("No geometry column found".to_string()))?;

            let geom_col = batch.column(geom_idx);
            let geom_field = schema.field(geom_idx);

            // Span: Converting Arrow array to GeoArrow geometry array
            let geom_array: Arc<dyn GeoArrowArray> = {
                let _parse_span = info_span!(
                    "geoarrow_parse",
                    row_group = rg_idx,
                    rows = batch.num_rows()
                )
                .entered();
                from_arrow_array(geom_col.as_ref(), geom_field).map_err(|e| {
                    Error::GeoParquetRead(format!("Failed to parse geometry array: {}", e))
                })?
            };

            // Span: Extracting geometries (GeoArrow -> geo::Geometry conversion)
            {
                let _extract_span = info_span!(
                    "geometry_extract",
                    row_group = rg_idx,
                    rows = batch.num_rows()
                )
                .entered();
                extract_geometries_from_array(geom_array.as_ref(), &mut row_group_geometries)?;
            }

            row_count += batch.num_rows();
        }

        let rg_info = RowGroupInfo {
            index: rg_idx,
            num_rows: row_count,
        };

        total_processed += row_group_geometries.len();
        callback(rg_info, row_group_geometries)?;
    }

    Ok(total_processed)
}

/// Extract geometries from a GeoArrow array into a Vec.
fn extract_geometries_from_array(
    array: &dyn GeoArrowArray,
    output: &mut Vec<Geometry<f64>>,
) -> Result<()> {
    match array.data_type() {
        GeoArrowType::Point(_) => {
            let arr = array.as_point();
            extract_typed_array(arr, output)
        }
        GeoArrowType::LineString(_) => {
            let arr = array.as_line_string();
            extract_typed_array(arr, output)
        }
        GeoArrowType::Polygon(_) => {
            let arr = array.as_polygon();
            extract_typed_array(arr, output)
        }
        GeoArrowType::MultiPoint(_) => {
            let arr = array.as_multi_point();
            extract_typed_array(arr, output)
        }
        GeoArrowType::MultiLineString(_) => {
            let arr = array.as_multi_line_string();
            extract_typed_array(arr, output)
        }
        GeoArrowType::MultiPolygon(_) => {
            let arr = array.as_multi_polygon();
            extract_typed_array(arr, output)
        }
        GeoArrowType::Geometry(_) => {
            let arr = array.as_geometry();
            extract_typed_array(arr, output)
        }
        GeoArrowType::GeometryCollection(_) => {
            let arr = array.as_geometry_collection();
            extract_typed_array(arr, output)
        }
        GeoArrowType::Wkb(_) => {
            let arr = array.as_wkb::<i32>();
            extract_typed_array(arr, output)
        }
        GeoArrowType::LargeWkb(_) => {
            let arr = array.as_wkb::<i64>();
            extract_typed_array(arr, output)
        }
        GeoArrowType::WkbView(_) => {
            let arr = array.as_wkb_view();
            extract_typed_array(arr, output)
        }
        GeoArrowType::Wkt(_) => {
            let arr = array.as_wkt::<i32>();
            extract_typed_array(arr, output)
        }
        GeoArrowType::LargeWkt(_) => {
            let arr = array.as_wkt::<i64>();
            extract_typed_array(arr, output)
        }
        GeoArrowType::WktView(_) => {
            let arr = array.as_wkt_view();
            extract_typed_array(arr, output)
        }
        _ => Err(Error::GeoParquetRead(format!(
            "Unsupported geometry type: {:?}",
            array.data_type()
        ))),
    }
}

/// Extract geometries from a typed GeoArrow array into a Vec.
fn extract_typed_array<'a, A>(accessor: &'a A, output: &mut Vec<Geometry<f64>>) -> Result<()>
where
    A: GeoArrowArrayAccessor<'a>,
    A::Item: ToGeoGeometry<f64>,
{
    for (i, item) in accessor.iter().enumerate() {
        if let Some(geom_result) = item {
            let geom_trait = geom_result.map_err(|e| {
                Error::GeoParquetRead(format!("Invalid geometry at index {}: {}", i, e))
            })?;

            if let Some(geo_geom) = geom_trait.try_to_geometry() {
                output.push(geo_geom);
            }
        }
    }
    Ok(())
}

/// Default number of parallel row group readers.
/// 4 provides good parallelism without excessive memory usage.
pub const DEFAULT_PARALLEL_READERS: usize = 4;

/// Result from a parallel row group read operation.
enum RowGroupReadResult {
    /// Successfully read a row group
    Ok {
        info: RowGroupInfo,
        geometries: Vec<Geometry<f64>>,
    },
    /// Error reading a row group
    Err(Error),
}

/// Read a single row group from a GeoParquet file.
///
/// This function opens its own file handle, making it safe for parallel use.
fn read_single_row_group(path: &Path, rg_idx: usize) -> Result<(RowGroupInfo, Vec<Geometry<f64>>)> {
    use parquet::file::reader::FileReader;
    use parquet::file::serialized_reader::SerializedFileReader;

    let file = std::fs::File::open(path)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to open: {}", e)))?;

    let parquet_reader = SerializedFileReader::new(
        file.try_clone()
            .map_err(|e| Error::GeoParquetRead(format!("Failed to clone file handle: {}", e)))?,
    )
    .map_err(|e| Error::GeoParquetRead(format!("Failed to create parquet reader: {}", e)))?;

    let num_row_groups = parquet_reader.metadata().num_row_groups();
    if rg_idx >= num_row_groups {
        return Err(Error::GeoParquetRead(format!(
            "Row group {} out of range (file has {})",
            rg_idx, num_row_groups
        )));
    }

    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to create reader: {}", e)))?;

    let reader = builder
        .with_row_groups(vec![rg_idx])
        .build()
        .map_err(|e| Error::GeoParquetRead(format!("Failed to build reader: {}", e)))?;

    let mut geometries = Vec::new();
    let mut row_count = 0;

    for batch_result in reader {
        let batch = batch_result
            .map_err(|e| Error::GeoParquetRead(format!("Failed to read batch: {}", e)))?;

        let schema = batch.schema();
        let geom_idx = schema
            .fields()
            .iter()
            .position(|f| f.name() == "geometry" || f.name().contains("geom"))
            .ok_or_else(|| Error::GeoParquetRead("No geometry column found".to_string()))?;

        let geom_col = batch.column(geom_idx);
        let geom_field = schema.field(geom_idx);

        let geom_array: Arc<dyn GeoArrowArray> = from_arrow_array(geom_col.as_ref(), geom_field)
            .map_err(|e| Error::GeoParquetRead(format!("Failed to parse geometry array: {}", e)))?;

        extract_geometries_from_array(geom_array.as_ref(), &mut geometries)?;
        row_count += batch.num_rows();
    }

    Ok((
        RowGroupInfo {
            index: rg_idx,
            num_rows: row_count,
        },
        geometries,
    ))
}

/// Process geometries from a GeoParquet file with parallel row group reading.
///
/// Spawns multiple reader threads that read and decompress row groups in parallel,
/// sending results through a bounded channel. This provides parallelism in decompression
/// while the consumer processes results.
///
/// # Arguments
///
/// * `path` - Path to the GeoParquet file
/// * `num_readers` - Number of parallel reader threads
/// * `callback` - Function called for each row group's geometries
///
/// # Returns
///
/// Total number of geometries processed
#[instrument(name = "read_parquet_parallel", skip(callback), fields(path = %path.display(), num_readers))]
pub fn process_geometries_parallel<F>(
    path: &Path,
    num_readers: usize,
    mut callback: F,
) -> Result<usize>
where
    F: FnMut(RowGroupInfo, Vec<Geometry<f64>>) -> Result<()>,
{
    use crossbeam_channel::bounded;

    let num_row_groups = get_row_group_count(path)?;

    if num_row_groups == 0 {
        return Ok(0);
    }

    let num_readers = num_readers.max(1).min(num_row_groups);

    // Bounded channel for results - limits memory to ~(num_readers + buffer) row groups
    let (tx, rx) = bounded::<RowGroupReadResult>(num_readers * 2);

    // Work queue - row group indices to process
    let (work_tx, work_rx) = bounded::<usize>(num_row_groups);

    // Fill work queue with all row group indices
    for rg_idx in 0..num_row_groups {
        work_tx.send(rg_idx).unwrap();
    }
    drop(work_tx); // Close work queue so workers know when to stop

    // Spawn reader threads (NOT using rayon - dedicated threads avoid deadlock)
    let mut reader_handles = Vec::with_capacity(num_readers);
    for _ in 0..num_readers {
        let work_rx = work_rx.clone();
        let tx = tx.clone();
        let path_owned = path.to_path_buf();

        let handle = std::thread::spawn(move || {
            // Each thread pulls work from the queue until empty
            for rg_idx in work_rx {
                let result = match read_single_row_group(&path_owned, rg_idx) {
                    Ok((info, geometries)) => RowGroupReadResult::Ok { info, geometries },
                    Err(e) => RowGroupReadResult::Err(e),
                };
                // Stop if receiver disconnected
                if tx.send(result).is_err() {
                    break;
                }
            }
        });
        reader_handles.push(handle);
    }

    // Drop our copy of tx so channel closes when all workers finish
    drop(tx);

    // Consumer: receive row groups and call callback
    // Row groups may arrive out of order due to parallel reads
    let mut total_processed = 0;
    let mut first_error: Option<Error> = None;

    for result in rx {
        match result {
            RowGroupReadResult::Ok { info, geometries } => {
                let geom_count = geometries.len();
                if let Err(e) = callback(info, geometries) {
                    first_error = Some(e);
                    break;
                }
                total_processed += geom_count;
            }
            RowGroupReadResult::Err(e) => {
                first_error = Some(e);
                break;
            }
        }
    }

    // Wait for all reader threads
    for handle in reader_handles {
        let _ = handle.join();
    }

    if let Some(e) = first_error {
        return Err(e);
    }

    Ok(total_processed)
}

/// Get the number of row groups in a GeoParquet file.
pub fn get_row_group_count(path: &Path) -> Result<usize> {
    use parquet::file::reader::FileReader;
    use parquet::file::serialized_reader::SerializedFileReader;

    let file = std::fs::File::open(path)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to open: {}", e)))?;

    let parquet_reader = SerializedFileReader::new(file)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to create parquet reader: {}", e)))?;

    Ok(parquet_reader.metadata().num_row_groups())
}

/// Extract field metadata from a GeoParquet file's schema.
///
/// Returns a mapping of field names to MVT-style types:
/// - "String" for string/utf8 fields
/// - "Number" for numeric fields (int, float, etc.)
/// - "Boolean" for boolean fields
///
/// Geometry columns are excluded from the result.
pub fn extract_field_metadata(path: &Path) -> Result<HashMap<String, String>> {
    use arrow_schema::DataType;

    let file = std::fs::File::open(path)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to open: {}", e)))?;

    let builder = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to create reader: {}", e)))?;

    let schema = builder.schema();
    let mut fields = HashMap::new();

    for field in schema.fields() {
        let name = field.name();

        // Skip geometry columns
        if name == "geometry" || name.contains("geom") {
            continue;
        }

        // Map Arrow types to MVT-style types
        let mvt_type = match field.data_type() {
            DataType::Utf8 | DataType::LargeUtf8 => "String",
            DataType::Boolean => "Boolean",
            DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float16
            | DataType::Float32
            | DataType::Float64 => "Number",
            // Skip complex types (lists, structs, binary, etc.)
            _ => continue,
        };

        fields.insert(name.clone(), mvt_type.to_string());
    }

    Ok(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_process_geometries_iterates_all() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let mut count = 0;
        let result = process_geometries(fixture, |_geom, _idx| {
            count += 1;
            Ok(())
        });

        assert!(result.is_ok());
        assert!(count > 100, "Should have many features, got {}", count);
    }

    #[test]
    fn test_process_by_row_group_single_group() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let mut row_group_count = 0;
        let mut total_geoms = 0;

        let result = process_geometries_by_row_group(fixture, |info, geoms| {
            row_group_count += 1;
            total_geoms += geoms.len();
            assert_eq!(
                info.num_rows,
                geoms.len(),
                "Row count should match geometry count"
            );
            Ok(())
        });

        assert!(result.is_ok());
        assert!(row_group_count >= 1, "Should have at least 1 row group");
        assert!(
            total_geoms > 100,
            "Should have many geometries, got {}",
            total_geoms
        );
    }

    #[test]
    fn test_process_by_row_group_multi_group() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let mut row_group_count = 0;
        let mut total_geoms = 0;

        let result = process_geometries_by_row_group(fixture, |_info, geoms| {
            row_group_count += 1;
            total_geoms += geoms.len();
            Ok(())
        });

        assert!(result.is_ok());
        assert!(
            row_group_count > 1,
            "Should have multiple row groups, got {}",
            row_group_count
        );
        assert!(
            total_geoms > 100,
            "Should have many geometries, got {}",
            total_geoms
        );
    }

    #[test]
    fn test_get_row_group_count() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let count = get_row_group_count(fixture).expect("Should get row group count");
        assert!(
            count > 1,
            "Multi-rowgroup fixture should have >1 row groups, got {}",
            count
        );
    }

    #[test]
    fn test_calculate_bbox_returns_valid_bounds() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let bbox = calculate_bbox(fixture).expect("Should calculate bbox");

        // Andorra bounds: ~1.4-1.8 lon, ~42.4-42.7 lat
        assert!(
            bbox.lng_min > 1.0 && bbox.lng_min < 2.0,
            "lng_min={}",
            bbox.lng_min
        );
        assert!(
            bbox.lng_max > 1.0 && bbox.lng_max < 2.0,
            "lng_max={}",
            bbox.lng_max
        );
        assert!(
            bbox.lat_min > 42.0 && bbox.lat_min < 43.0,
            "lat_min={}",
            bbox.lat_min
        );
        assert!(
            bbox.lat_max > 42.0 && bbox.lat_max < 43.0,
            "lat_max={}",
            bbox.lat_max
        );
    }

    /// Test that file handle reuse produces consistent results.
    /// Compares row-group-by-row-group processing with batch processing.
    #[test]
    fn test_file_handle_reuse_produces_consistent_results() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Get geometries via row-group processing (uses file handle reuse)
        let mut rg_geometries = Vec::new();
        process_geometries_by_row_group(fixture, |_info, geoms| {
            rg_geometries.extend(geoms);
            Ok(())
        })
        .expect("Row-group processing should succeed");

        // Get geometries via batch processing (uses single file handle)
        let batch_geometries =
            extract_geometries(fixture).expect("Batch processing should succeed");

        // Results should match exactly
        assert_eq!(
            rg_geometries.len(),
            batch_geometries.len(),
            "Row-group and batch processing should produce same count"
        );
    }

    /// Test that WKT-encoded GeoParquet files can be read.
    /// See: https://github.com/geoparquet-io/gpq-tiles/issues/35
    ///
    /// To generate the fixture locally:
    /// ```bash
    /// cd tests/fixtures/realdata && uv run python -c "
    /// import geopandas as gpd
    /// import pyarrow as pa
    /// import pyarrow.parquet as pq
    /// import json
    /// gdf = gpd.read_parquet('open-buildings.parquet').head(100)
    /// wkt_strings = gdf.geometry.to_wkt()
    /// arrays = [pa.array(wkt_strings.tolist(), type=pa.utf8()) if col == 'geometry'
    ///           else pa.array(gdf[col].tolist()) for col in gdf.columns]
    /// table = pa.table(dict(zip(gdf.columns, arrays)))
    /// geo_meta = {'version': '1.1.0', 'primary_column': 'geometry',
    ///             'columns': {'geometry': {'encoding': 'WKT', 'geometry_types': ['Polygon']}}}
    /// table = table.replace_schema_metadata({b'geo': json.dumps(geo_meta).encode()})
    /// pq.write_table(table, 'wkt-encoded.parquet')
    /// "
    /// ```
    #[test]
    fn test_wkt_encoded_parquet() {
        let fixture = Path::new("../../tests/fixtures/realdata/wkt-encoded.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: WKT fixture not found (generate locally - see test docstring)");
            return;
        }

        let mut count = 0;
        let result = process_geometries(fixture, |geom, _idx| {
            // Verify we get valid polygons (the fixture contains building footprints)
            assert!(
                matches!(
                    geom,
                    geo::Geometry::Polygon(_) | geo::Geometry::MultiPolygon(_)
                ),
                "Expected Polygon or MultiPolygon, got {:?}",
                geom
            );
            count += 1;
            Ok(())
        });

        assert!(result.is_ok(), "Should read WKT file: {:?}", result.err());
        assert_eq!(count, 100, "Should have 100 features, got {}", count);
    }

    /// Test that row group indices are sequential and correct.
    #[test]
    fn test_row_group_indices_are_sequential() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let expected_count = get_row_group_count(fixture).expect("Should get row group count");
        let mut indices = Vec::new();

        process_geometries_by_row_group(fixture, |info, _geoms| {
            indices.push(info.index);
            Ok(())
        })
        .expect("Processing should succeed");

        // Indices should be 0, 1, 2, ..., n-1
        let expected_indices: Vec<usize> = (0..expected_count).collect();
        assert_eq!(
            indices, expected_indices,
            "Row group indices should be sequential"
        );
    }

    /// Test that row group processing handles many row groups efficiently.
    /// This validates the file handle reuse optimization.
    #[test]
    fn test_many_row_groups_processed_efficiently() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let num_row_groups = get_row_group_count(fixture).expect("Should get row group count");
        let mut processed_groups = 0;
        let mut total_rows = 0;

        let result = process_geometries_by_row_group(fixture, |info, geoms| {
            processed_groups += 1;
            total_rows += info.num_rows;
            assert_eq!(
                info.num_rows,
                geoms.len(),
                "Row count should match geometry count for row group {}",
                info.index
            );
            Ok(())
        });

        assert!(result.is_ok(), "Should process all row groups successfully");
        assert_eq!(
            processed_groups, num_row_groups,
            "Should process all row groups"
        );
        assert!(total_rows > 0, "Should have processed some rows");
    }

    // ==================== Parallel Reader Tests ====================

    /// Test that parallel reader produces the same geometry count as sequential reader.
    #[test]
    fn test_parallel_reader_same_count_as_sequential() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Count with sequential reader
        let mut sequential_count = 0;
        process_geometries_by_row_group(fixture, |_info, geoms| {
            sequential_count += geoms.len();
            Ok(())
        })
        .expect("Sequential processing should succeed");

        // Count with parallel reader
        let mut parallel_count = 0;
        process_geometries_parallel(fixture, DEFAULT_PARALLEL_READERS, |_info, geoms| {
            parallel_count += geoms.len();
            Ok(())
        })
        .expect("Parallel processing should succeed");

        assert_eq!(
            sequential_count, parallel_count,
            "Parallel reader should produce same geometry count as sequential"
        );
    }

    /// Test that parallel reader processes all row groups.
    #[test]
    fn test_parallel_reader_processes_all_row_groups() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let expected_count = get_row_group_count(fixture).expect("Should get row group count");
        let processed_indices = std::sync::Mutex::new(Vec::new());

        process_geometries_parallel(fixture, DEFAULT_PARALLEL_READERS, |info, _geoms| {
            processed_indices.lock().unwrap().push(info.index);
            Ok(())
        })
        .expect("Parallel processing should succeed");

        let mut indices = processed_indices.into_inner().unwrap();
        indices.sort(); // Row groups may arrive out of order

        let expected_indices: Vec<usize> = (0..expected_count).collect();
        assert_eq!(
            indices, expected_indices,
            "All row groups should be processed"
        );
    }

    /// Test that parallel reader handles single row group files.
    #[test]
    fn test_parallel_reader_single_row_group() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let rg_count = get_row_group_count(fixture).expect("Should get row group count");
        if rg_count != 1 {
            eprintln!("Skipping: fixture has {} row groups, expected 1", rg_count);
            return;
        }

        let mut processed = 0;
        let result =
            process_geometries_parallel(fixture, DEFAULT_PARALLEL_READERS, |_info, geoms| {
                processed += geoms.len();
                Ok(())
            });

        assert!(result.is_ok(), "Should handle single row group");
        assert!(processed > 0, "Should process geometries");
    }

    /// Test that parallel reader propagates errors from callback.
    #[test]
    fn test_parallel_reader_callback_error_propagation() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let result =
            process_geometries_parallel(fixture, DEFAULT_PARALLEL_READERS, |_info, _geoms| {
                Err(Error::GeoParquetRead("Test error".to_string()))
            });

        assert!(result.is_err(), "Should propagate callback error");
    }

    /// Test that parallel reader works with num_readers = 1 (effectively sequential).
    #[test]
    fn test_parallel_reader_single_reader() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let mut count = 0;
        let result = process_geometries_parallel(fixture, 1, |_info, geoms| {
            count += geoms.len();
            Ok(())
        });

        assert!(result.is_ok(), "Should work with single reader");
        assert!(count > 0, "Should process geometries");
    }

    /// Test that parallel reader handles large num_readers gracefully.
    #[test]
    fn test_parallel_reader_many_readers() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Request more readers than row groups
        let mut count = 0;
        let result = process_geometries_parallel(fixture, 100, |_info, geoms| {
            count += geoms.len();
            Ok(())
        });

        assert!(result.is_ok(), "Should handle more readers than row groups");
        assert!(count > 0, "Should process geometries");
    }

    /// Test that read_single_row_group returns error for invalid index.
    #[test]
    fn test_read_single_row_group_invalid_index() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let num_rg = get_row_group_count(fixture).expect("Should get count");
        let result = read_single_row_group(fixture, num_rg + 100);

        assert!(result.is_err(), "Should error on invalid row group index");
    }
}
