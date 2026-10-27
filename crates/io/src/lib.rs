//! Checked Parquet and Arrow IPC I/O for tylertoo.
//!
//! Raw readers remain private. The few upstream types exported below are
//! required by the remote byte-range adapter, statistics pruning, and parallel
//! row-group encoding; none permits constructing an unchecked reader.

mod ipc;
mod untrusted;
mod writer;

pub use ipc::{IpcFileWriter, IpcStreamReader, IpcStreamWriter};
pub use untrusted::{
    byte_array_limit, check_byte_array_ceiling, decode_footer, input_error,
    largest_byte_array_column, load_reader_metadata, open_ipc, open_parquet,
    open_parquet_with_metadata, precheck_footer, read_batch_plan, read_parquet_metadata,
    reader_metadata, schema_without_metadata, LargestByteArrayColumn, ParquetSource,
    ReaderMetadata, UntrustedBatchReader, UntrustedIpcReader, BYTE_ARRAY_BATCH_LIMIT,
    DEFAULT_BATCH_ROWS,
};
pub use writer::{batch_writer, overview_writer, BatchWriter, OverviewWriteOptions};

/// Parallel overview encoding maps Arrow arrays to Parquet leaves with this function.
pub use parquet::arrow::arrow_writer::compute_leaves;
/// Encoded column chunks cross from parallel encode tasks to the ordered writer.
pub use parquet::arrow::arrow_writer::ArrowColumnChunk;
/// Column encoders are sent to the parallel overview encode tasks.
pub use parquet::arrow::arrow_writer::ArrowColumnWriter;
/// The overview pipeline creates column encoders for each manually bounded row group.
pub use parquet::arrow::arrow_writer::ArrowRowGroupWriterFactory;
/// Column projection masks are needed to select nested leaves as well as roots.
pub use parquet::arrow::ProjectionMask;
/// Converted types identify legacy timestamp statistics during overview inspection.
pub use parquet::basic::ConvertedType;
/// Integer annotations distinguish signed and unsigned statistics.
pub use parquet::basic::IntType;
/// Logical types interpret statistics consistently with the Arrow schema.
pub use parquet::basic::LogicalType;
/// Physical types identify byte-array leaves for footer-only size accounting.
pub use parquet::basic::Type as PhysicalType;
/// Existing core error enums retain the upstream error and its source chain.
pub use parquet::errors::ParquetError;
/// Key-value metadata carries GeoParquet and overview format descriptions.
pub use parquet::file::metadata::KeyValue;
/// Pruning and validation inspect row groups without decoding record batches.
pub use parquet::file::metadata::ParquetMetaData;
/// Row-group statistics are consumed by bbox and attribute pruning.
pub use parquet::file::metadata::RowGroupMetaData;
/// Remote byte-range readers implement this upstream input trait.
pub use parquet::file::reader::ChunkReader;
/// Remote byte-range readers report their object size through this trait.
pub use parquet::file::reader::Length;
/// Pruning inspects the concrete min/max statistics for each physical type.
pub use parquet::file::statistics::Statistics;
/// The ordered overview writer appends row groups encoded on worker threads.
pub use parquet::file::writer::SerializedFileWriter;

/// Errors refused by the input boundary before or during upstream decoding.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A row cannot fit the byte-offset ceiling even after batch splitting.
    #[error("column '{column}' holds {bytes} bytes in one row, past the {limit_bytes}-byte byte-array ceiling (i32 offsets). Split or simplify those features before tiling.")]
    ByteArrayRowTooLarge {
        /// The offending leaf column.
        column: String,
        /// Bytes addressed by one row.
        bytes: u64,
        /// The per-array byte ceiling.
        limit_bytes: u64,
    },
    /// A decoder panicked; its damaged state is discarded.
    #[error("panicked while decoding the {what}: {message}")]
    DecoderPanic {
        /// What was being decoded.
        what: &'static str,
        /// The panic message and source location.
        message: String,
    },
    /// A footer declares a count that its bytes cannot hold.
    #[error("refusing a malformed Parquet footer: {0}")]
    HostileFooter(String),
    /// An IPC footer declares a block outside the file.
    #[error("refusing a malformed Arrow IPC footer: {0}")]
    HostileIpcFooter(String),
}

/// Fixture construction only, excluded from the shipping API baseline.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    /// Fixtures need raw encoding to construct deliberately malformed inputs.
    pub use parquet::arrow::ArrowWriter;
    /// Fixtures construct footer column chunks without allocating huge data pages.
    pub use parquet::file::metadata::ColumnChunkMetaData;
    /// Fixtures mutate offsets in existing footer chunks.
    pub use parquet::file::metadata::ColumnChunkMetaDataBuilder;
    /// Fixtures construct file-level footer records.
    pub use parquet::file::metadata::FileMetaData;
    /// Fixtures serialize mutated footers for boundary regressions.
    pub use parquet::file::metadata::ParquetMetaDataWriter;
    /// Fixtures combine synthetic row-group records.
    pub use parquet::file::metadata::{KeyValue, ParquetMetaData, RowGroupMetaData};
    /// Fixtures choose which column statistics are present.
    pub use parquet::file::properties::EnabledStatistics;
    /// Fixtures vary dictionary and statistics settings to exercise read planning.
    pub use parquet::file::properties::WriterProperties;
    /// Fixtures construct native Parquet geospatial bounding boxes.
    pub use parquet::geospatial::bounding_box::BoundingBox;
    /// Fixtures construct native Parquet geospatial statistics.
    pub use parquet::geospatial::statistics::GeospatialStatistics;
    /// Fixtures build physical schemas for synthetic footer statistics.
    pub use parquet::schema::types::SchemaDescriptor;
    /// Fixtures build physical columns for synthetic footer statistics.
    pub use parquet::schema::types::Type;
}
