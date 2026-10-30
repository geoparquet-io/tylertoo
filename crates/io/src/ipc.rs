//! IPC files for saved plans and streams for run-local spills.

use lz4_flex::frame::{BlockSize, FrameDecoder, FrameEncoder, FrameInfo};
use std::io::{BufRead, BufReader, Read, Write};

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

/// Buffer encoding for private spill streams; saved-plan files are unchanged.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum IpcCompression {
    /// Store raw Arrow buffers.
    #[default]
    None,
    /// Compress the private IPC stream in independent 64 KiB LZ4 blocks.
    Lz4,
}

struct CountWrite<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for CountWrite<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.bytes = self.bytes.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// An IPC stream writer for a conversion's temporary spill.
pub struct IpcStreamWriter<W: Write> {
    inner: SpillWriter<W>,
    compression: IpcCompression,
}

impl<W: Write> IpcStreamWriter<W> {
    /// Start an uncompressed stream with its schema.
    ///
    /// # Errors
    ///
    /// Returns an Arrow or I/O error while writing the schema.
    pub fn try_new(sink: W, schema: &Schema) -> Result<Self, ArrowError> {
        Self::try_new_with_compression(sink, schema, IpcCompression::None)
    }

    /// Start a private spill stream with an explicit buffer encoding.
    ///
    /// # Errors
    ///
    /// Returns an Arrow or I/O error while writing the schema.
    pub fn try_new_with_compression(
        sink: W,
        schema: &Schema,
        compression: IpcCompression,
    ) -> Result<Self, ArrowError> {
        let sink = CountWrite {
            inner: sink,
            bytes: 0,
        };
        let inner = match compression {
            IpcCompression::None => SpillWriter::Raw(Box::new(
                arrow_ipc::writer::StreamWriter::try_new(sink, schema)?,
            )),
            IpcCompression::Lz4 => {
                let mut writer = arrow_ipc::writer::StreamWriter::try_new(
                    FrameSink {
                        sink: Some(sink),
                        encoder: None,
                    },
                    schema,
                )?;
                writer.get_mut().end_frame()?;
                SpillWriter::Lz4(Box::new(writer))
            }
        };
        Ok(Self { inner, compression })
    }

    /// Encoded bytes accepted by the sink, including schema and framing.
    /// After a successful finish this is the complete stream's byte count.
    pub fn bytes_written(&self) -> u64 {
        match &self.inner {
            SpillWriter::Raw(writer) => writer.get_ref().bytes,
            SpillWriter::Lz4(writer) => writer.get_ref().get_ref().bytes,
        }
    }

    /// Append one spill batch.
    ///
    /// # Errors
    ///
    /// Returns an Arrow or I/O error while writing the batch.
    pub fn write(&mut self, batch: &RecordBatch) -> Result<(), ArrowError> {
        match &mut self.inner {
            SpillWriter::Raw(writer) => writer.write(batch),
            SpillWriter::Lz4(writer) => {
                writer.write(batch)?;
                writer.get_mut().end_frame()
            }
        }
    }

    /// Write end-of-stream and flush the sink.
    ///
    /// # Errors
    ///
    /// Returns an I/O error while completing the stream.
    pub fn finish(&mut self) -> Result<(), ArrowError> {
        match &mut self.inner {
            SpillWriter::Raw(writer) => writer.finish()?,
            SpillWriter::Lz4(writer) => {
                writer.finish()?;
                writer.get_mut().end_frame()?;
                writer.get_mut().flush()?;
            }
        }
        log::debug!(
            "IPC spill complete: compression={:?} physical_bytes={}",
            self.compression,
            self.bytes_written()
        );
        Ok(())
    }
}

enum SpillWriter<W: Write> {
    Raw(Box<arrow_ipc::writer::StreamWriter<CountWrite<W>>>),
    Lz4(Box<arrow_ipc::writer::StreamWriter<FrameSink<CountWrite<W>>>>),
}

/// Codec scratch exists only during one IPC schema/batch/EOS write. Leaving
/// it attached to idle spill writers adds a fixed per-level live-heap floor.
struct FrameSink<W: Write> {
    sink: Option<W>,
    encoder: Option<Box<FrameEncoder<W>>>,
}

impl<W: Write> FrameSink<W> {
    fn get_ref(&self) -> &W {
        self.encoder.as_ref().map_or_else(
            || self.sink.as_ref().expect("inactive frame owns its sink"),
            |encoder| encoder.get_ref(),
        )
    }

    fn end_frame(&mut self) -> Result<(), ArrowError> {
        if let Some(encoder) = &mut self.encoder {
            encoder
                .try_finish()
                .map_err(|e| ArrowError::ExternalError(Box::new(e)))?;
            // try_finish succeeded, so into_inner can release both scratch
            // buffers without a second finish (which would write another EOS).
            self.sink = self.encoder.take().map(|encoder| encoder.into_inner());
        }
        Ok(())
    }
}

impl<W: Write> Write for FrameSink<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.encoder.is_none() {
            let sink = self
                .sink
                .take()
                .ok_or_else(|| std::io::Error::other("closed LZ4 frame"))?;
            let info = FrameInfo::new().block_size(BlockSize::Max64KB);
            self.encoder = Some(Box::new(FrameEncoder::with_frame_info(info, sink)));
        }
        self.encoder
            .as_mut()
            .expect("active frame owns an encoder")
            .write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(encoder) = &mut self.encoder {
            encoder.flush()
        } else {
            self.sink
                .as_mut()
                .expect("inactive frame owns its sink")
                .flush()
        }
    }
}

type Prefixed<R> = std::io::Chain<std::io::Cursor<[u8; 4]>, R>;

enum SpillInput<R: Read> {
    Raw(Prefixed<R>),
    Lz4(Box<FrameInput<R>>),
}

struct FrameInput<R: Read> {
    decoder: FrameDecoder<BufReader<Prefixed<R>>>,
}

impl<R: Read> Read for FrameInput<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let read = self.decoder.read(buffer)?;
            if read != 0 || buffer.is_empty() {
                return Ok(read);
            }
            // FrameDecoder returns zero at every frame boundary. Only expose
            // EOF to Arrow after the physical stream ends, preserving IPC
            // messages that straddle schema/batch/EOS frame boundaries.
            if self.decoder.get_mut().fill_buf()?.is_empty() {
                return Ok(0);
            }
        }
    }
}

impl<R: Read> std::fmt::Debug for SpillInput<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SpillInput")
    }
}

impl<R: Read> Read for SpillInput<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Raw(reader) => reader.read(buffer),
            Self::Lz4(reader) => reader.read(buffer),
        }
    }
}

/// A spill reader that discards decoder state after an error or panic.
#[derive(Debug)]
pub struct IpcStreamReader<R: Read> {
    inner: arrow_ipc::reader::StreamReader<SpillInput<R>>,
    done: bool,
}

impl<R: Read> IpcStreamReader<R> {
    /// Open a stream produced by this run's spill writer.
    ///
    /// # Errors
    ///
    /// Returns an Arrow error if the schema is invalid or the decoder panics.
    pub fn try_new(mut reader: R, projection: Option<Vec<usize>>) -> Result<Self, ArrowError> {
        #[expect(
            clippy::disallowed_methods,
            reason = "#779: stream owner guards schema decoding and every next call"
        )]
        let inner = guard_arrow("Arrow IPC stream schema", || {
            let mut prefix = [0; 4];
            reader.read_exact(&mut prefix)?;
            let compressed = prefix == [0x04, 0x22, 0x4d, 0x18];
            let reader = std::io::Cursor::new(prefix).chain(reader);
            let reader = if compressed {
                SpillInput::Lz4(Box::new(FrameInput {
                    decoder: FrameDecoder::new(BufReader::new(reader)),
                }))
            } else {
                SpillInput::Raw(reader)
            };
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

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{ArrayRef, Int64Array};
    use arrow_schema::{DataType, Field};
    use std::io::Cursor;
    use std::sync::Arc;

    #[test]
    fn spill_encodings_roundtrip_and_count_complete_bytes() {
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        let input = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![0; 65536])) as ArrayRef],
        )
        .unwrap();
        let mut sizes = Vec::new();
        for encoding in [IpcCompression::None, IpcCompression::Lz4] {
            let mut data = Vec::new();
            let mut writer =
                IpcStreamWriter::try_new_with_compression(&mut data, &schema, encoding).unwrap();
            writer.write(&input).unwrap();
            writer.finish().unwrap();
            let bytes = writer.bytes_written();
            drop(writer);
            assert_eq!(bytes, data.len() as u64);
            let mut reader = IpcStreamReader::try_new(Cursor::new(data), None).unwrap();
            assert_eq!(reader.next().unwrap().unwrap(), input);
            assert!(reader.next().is_none());
            sizes.push(bytes);
        }
        assert!(sizes[1] < sizes[0] / 2, "{sizes:?}");
    }
}
