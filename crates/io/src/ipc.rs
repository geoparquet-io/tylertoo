//! IPC files for saved plans and streams for run-local spills.

use std::io::{Read, Write};

use arrow_array::RecordBatch;
use arrow_schema::{ArrowError, Schema, SchemaRef};

use crate::untrusted::{guard_arrow, guarded_next};

/// A saved-plan IPC file writer.
pub struct IpcFileWriter<W: Write>(arrow_ipc::writer::FileWriter<W>);

impl<W: Write> IpcFileWriter<W> {
    /// Start a file at the sink's current position.
    ///
    /// # Errors
    ///
    /// Returns an Arrow or I/O error while writing the schema.
    pub fn try_new(sink: W, schema: &Schema) -> Result<Self, ArrowError> {
        arrow_ipc::writer::FileWriter::try_new(sink, schema).map(Self)
    }

    /// Write a saved-plan record batch.
    ///
    /// # Errors
    ///
    /// Returns an Arrow or I/O error while encoding the batch.
    pub fn write(&mut self, batch: &RecordBatch) -> Result<(), ArrowError> {
        self.0.write(batch)
    }

    /// Write the footer and flush the sink.
    ///
    /// # Errors
    ///
    /// Returns an I/O error while completing the file.
    pub fn finish(&mut self) -> Result<(), ArrowError> {
        self.0.finish()
    }

    /// Finish the file and return its sink.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be completed.
    pub fn into_inner(self) -> Result<W, ArrowError> {
        self.0.into_inner()
    }
}

/// An IPC stream writer for a conversion's temporary spill.
pub struct IpcStreamWriter<W: Write>(arrow_ipc::writer::StreamWriter<W>);

impl<W: Write> IpcStreamWriter<W> {
    /// Start a stream with its schema.
    ///
    /// # Errors
    ///
    /// Returns an Arrow or I/O error while writing the schema.
    pub fn try_new(sink: W, schema: &Schema) -> Result<Self, ArrowError> {
        arrow_ipc::writer::StreamWriter::try_new(sink, schema).map(Self)
    }

    /// Append one spill batch.
    ///
    /// # Errors
    ///
    /// Returns an Arrow or I/O error while writing the batch.
    pub fn write(&mut self, batch: &RecordBatch) -> Result<(), ArrowError> {
        self.0.write(batch)
    }

    /// Write end-of-stream and flush the sink.
    ///
    /// # Errors
    ///
    /// Returns an I/O error while completing the stream.
    pub fn finish(&mut self) -> Result<(), ArrowError> {
        self.0.finish()
    }
}

/// A spill reader that discards decoder state after an error or panic.
#[derive(Debug)]
pub struct IpcStreamReader<R: Read> {
    inner: arrow_ipc::reader::StreamReader<R>,
    done: bool,
}

impl<R: Read> IpcStreamReader<R> {
    /// Open a stream produced by this run's spill writer.
    ///
    /// # Errors
    ///
    /// Returns an Arrow error if the schema is invalid or the decoder panics.
    pub fn try_new(reader: R, projection: Option<Vec<usize>>) -> Result<Self, ArrowError> {
        let inner = guard_arrow("Arrow IPC stream schema", || {
            arrow_ipc::reader::StreamReader::try_new(reader, projection)
        })?;
        Ok(Self { inner, done: false })
    }

    /// The stream's Arrow schema.
    pub fn schema(&self) -> SchemaRef {
        self.inner.schema()
    }
}

impl<R: Read> Iterator for IpcStreamReader<R> {
    type Item = Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        let inner = &mut self.inner;
        guarded_next(&mut self.done, "Arrow IPC stream batch", || inner.next())
    }
}
