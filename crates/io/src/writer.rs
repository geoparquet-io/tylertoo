//! Parquet writer construction and physical overview encoding policy.

use std::io::Write;

use arrow_array::RecordBatch;
use arrow_schema::{DataType, Schema, SchemaRef};
use parquet::arrow::arrow_writer::ArrowRowGroupWriterFactory;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::errors::ParquetError;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::ColumnPath;

/// Physical encoding options for an overview file. Level and row-group
/// boundaries remain controlled by the overview pipeline.
#[derive(Debug, Clone)]
pub struct OverviewWriteOptions {
    /// ZSTD compression level, validated before writing the file header.
    pub zstd_level: i32,
    /// Retain statistics on geometry and string/binary properties too.
    pub full_column_stats: bool,
    /// WKB geometry columns, whose dictionary encoding is disabled.
    pub geometry_columns: Vec<String>,
    /// Generated bbox structs, whose children are spatial pruning indexes.
    pub covering_columns: Vec<String>,
}

/// A Parquet batch writer constructed from this crate's encoding policy.
pub struct BatchWriter<W: Write + Send>(ArrowWriter<W>);

/// Construct a default Parquet writer (used by decoded-tile export).
///
/// # Errors
///
/// Returns an error if the schema or sink cannot be written.
pub fn batch_writer<W: Write + Send>(
    sink: W,
    schema: SchemaRef,
) -> Result<BatchWriter<W>, ParquetError> {
    ArrowWriter::try_new(sink, schema, None).map(BatchWriter)
}

/// Construct the overview writer: ZSTD, manual row-group boundaries, no
/// geometry/bbox dictionaries, and statistics tuned for spatial pruning.
///
/// `source_schema` describes properties before the GeoParquet encoder adds
/// covering columns; those generated columns always retain page statistics.
///
/// # Errors
///
/// Returns an error for an invalid compression level, schema or sink.
pub fn overview_writer<W: Write + Send>(
    sink: W,
    target_schema: SchemaRef,
    source_schema: &Schema,
    options: &OverviewWriteOptions,
) -> Result<BatchWriter<W>, ParquetError> {
    let props = overview_properties(options, source_schema)?;
    ArrowWriter::try_new(sink, target_schema, Some(props)).map(BatchWriter)
}

impl<W: Write + Send> BatchWriter<W> {
    /// Encode a batch, retaining row groups until explicitly flushed.
    ///
    /// # Errors
    ///
    /// Returns the underlying schema, encoding or I/O error.
    pub fn write(&mut self, batch: &RecordBatch) -> Result<(), ParquetError> {
        self.0.write(batch)
    }

    /// Add file metadata before closing the writer.
    pub fn append_key_value_metadata(&mut self, metadata: KeyValue) {
        self.0.append_key_value_metadata(metadata);
    }

    /// Finish the file and its footer.
    ///
    /// # Errors
    ///
    /// Returns an encoding or I/O error while completing the file.
    pub fn close(self) -> Result<(), ParquetError> {
        self.0.close().map(|_| ())
    }

    /// Split the writer into its ordered sink and parallel column factory.
    /// This preserves exactly the schema, properties and embedded Arrow schema
    /// that the serial writer would use.
    ///
    /// # Errors
    ///
    /// Returns an error if the writer cannot be converted.
    pub fn into_serialized_writer(
        self,
    ) -> Result<(SerializedFileWriter<W>, ArrowRowGroupWriterFactory), ParquetError> {
        self.0.into_serialized_writer()
    }
}

fn overview_properties(
    options: &OverviewWriteOptions,
    source_schema: &Schema,
) -> Result<WriterProperties, ParquetError> {
    let mut builder = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(options.zstd_level)?))
        .set_max_row_group_row_count(Some(usize::MAX))
        .set_statistics_enabled(EnabledStatistics::Page);

    for geom in &options.geometry_columns {
        builder = builder.set_column_dictionary_enabled(ColumnPath::from(geom.clone()), false);
    }
    for covering in &options.covering_columns {
        for child in ["xmin", "ymin", "xmax", "ymax"] {
            builder = builder.set_column_dictionary_enabled(
                ColumnPath::from(vec![covering.clone(), child.to_string()]),
                false,
            );
        }
    }
    if !options.full_column_stats {
        for field in source_schema.fields() {
            let is_geometry = options.geometry_columns.contains(field.name());
            let is_string_or_binary = matches!(
                field.data_type(),
                DataType::Utf8
                    | DataType::LargeUtf8
                    | DataType::Utf8View
                    | DataType::Binary
                    | DataType::LargeBinary
                    | DataType::BinaryView
            );
            if is_geometry || is_string_or_binary {
                builder = builder.set_column_statistics_enabled(
                    ColumnPath::from(field.name().clone()),
                    EnabledStatistics::None,
                );
            }
        }
    }
    Ok(builder.build())
}
