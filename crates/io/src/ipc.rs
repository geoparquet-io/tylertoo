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
    offsets: Option<Vec<u64>>,
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
        let mut writer = Self {
            inner,
            compression,
            offsets: None,
        };
        if !schema
            .flattened_fields()
            .iter()
            .any(|f| matches!(f.data_type(), arrow_schema::DataType::Dictionary(..)))
        {
            writer.offsets = Some(vec![writer.bytes_written()]);
        }
        Ok(writer)
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
        }?;
        let end = self.bytes_written();
        if let Some(offsets) = &mut self.offsets {
            offsets.push(end);
        }
        Ok(())
    }

    /// Schema end followed by each independent batch end, or `None` when
    /// dictionary state requires the serial stream reader, or on a platform
    /// without the positional reads [`IpcIndexedReader`] needs.
    pub fn batch_offsets(&self) -> Option<&[u64]> {
        self.offsets.as_deref().filter(|_| cfg!(any(unix, windows)))
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

/// The LZ4 block size of every spill frame.
const SPILL_BLOCK_SIZE: BlockSize = BlockSize::Max64KB;

/// Decoder buffers for one [`SPILL_BLOCK_SIZE`] block, including its
/// compressed input. Shared by spill admission and the codec owner.
pub const IPC_CODEC_SCRATCH_BYTES: u64 = 2 * match SPILL_BLOCK_SIZE {
    BlockSize::Max64KB => 64 << 10,
    _ => panic!("price the spill block size"),
};

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
            let info = FrameInfo::new().block_size(SPILL_BLOCK_SIZE);
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

/// One open run-local, dictionary-free spill, with its schema decoded once.
/// Independent positional reads let ordered windows share this handle.
#[derive(Debug)]
pub struct IpcIndexedReader {
    file: std::fs::File,
    schema: SchemaRef,
    schema_end: u64,
}

impl IpcIndexedReader {
    /// Open the index of a completed dictionary-free stream.
    ///
    /// # Errors
    /// Returns an error if the schema cannot be read.
    pub fn try_new(file: std::fs::File, schema_end: u64) -> Result<Self, ArrowError> {
        let schema = IpcStreamReader::try_new(
            BufReader::new(FileRange {
                file: &file,
                position: 0,
                end: schema_end,
            }),
            None,
        )?
        .schema();
        Ok(Self {
            file,
            schema,
            schema_end,
        })
    }

    /// Read a batch range without reopening the file or decoding its schema.
    ///
    /// # Errors
    /// Returns an error for invalid offsets, I/O or invalid IPC data.
    pub fn read_batch(&self, start: u64, end: u64) -> Result<RecordBatch, ArrowError> {
        if start < self.schema_end || end < start {
            return Err(ArrowError::ParseError("invalid spill batch offsets".into()));
        }
        guard_arrow("indexed Arrow IPC stream batch", || {
            let range = FileRange {
                file: &self.file,
                position: start,
                end,
            };
            let mut input = SpillInput::try_new(BufReader::new(range))?;
            let mut length = [0; 4];
            input.read_exact(&mut length)?;
            if length == [0xff; 4] {
                input.read_exact(&mut length)?;
            }
            let length = usize::try_from(i32::from_le_bytes(length))
                .map_err(|_| ArrowError::ParseError("invalid spill metadata length".into()))?;
            let mut metadata = vec![0; length];
            input.read_exact(&mut metadata)?;
            let message = arrow_ipc::root_as_message(&metadata)
                .map_err(|e| ArrowError::ParseError(format!("invalid spill message: {e}")))?;
            let batch = message
                .header_as_record_batch()
                .ok_or_else(|| ArrowError::ParseError("missing indexed spill batch".into()))?;
            let length = usize::try_from(message.bodyLength())
                .map_err(|_| ArrowError::ParseError("invalid spill body length".into()))?;
            let mut body = vec![0u8; length];
            input.read_exact(&mut body)?;
            arrow_ipc::reader::RecordBatchDecoder::try_new(
                &body.into(),
                batch,
                self.schema.clone(),
                &Default::default(),
                &message.version(),
            )?
            .read_record_batch()
        })
    }
}

/// A bounded cursor with its own offset, sharing one file without seek races.
struct FileRange<'a> {
    file: &'a std::fs::File,
    position: u64,
    end: u64,
}

impl Read for FileRange<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let len = buffer
            .len()
            .min(usize::try_from(self.end - self.position).unwrap_or(usize::MAX));
        #[cfg(unix)]
        let read =
            std::os::unix::fs::FileExt::read_at(self.file, &mut buffer[..len], self.position)?;
        #[cfg(windows)]
        let read =
            std::os::windows::fs::FileExt::seek_read(self.file, &mut buffer[..len], self.position)?;
        #[cfg(not(any(unix, windows)))]
        let read = return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "positional spill reads",
        ));
        self.position += read as u64;
        Ok(read)
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

impl<R: Read> SpillInput<R> {
    fn try_new(mut reader: R) -> Result<Self, ArrowError> {
        let mut prefix = [0; 4];
        reader.read_exact(&mut prefix)?;
        let compressed = prefix == [0x04, 0x22, 0x4d, 0x18];
        let reader = std::io::Cursor::new(prefix).chain(reader);
        Ok(if compressed {
            Self::Lz4(Box::new(FrameInput {
                decoder: FrameDecoder::new(BufReader::new(reader)),
            }))
        } else {
            Self::Raw(reader)
        })
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
    pub fn try_new(reader: R, projection: Option<Vec<usize>>) -> Result<Self, ArrowError> {
        #[expect(
            clippy::disallowed_methods,
            reason = "#779: stream owner guards schema decoding and every next call"
        )]
        let inner = guard_arrow("Arrow IPC stream schema", || {
            arrow_ipc::reader::StreamReader::try_new(SpillInput::try_new(reader)?, projection)
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
    use arrow_array::{Array, ArrayRef, Int64Array};
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
            let before = writer.bytes_written();
            writer.finish().unwrap();
            // Raw: the 8-byte end-of-stream marker. LZ4: that marker in a
            // frame of its own, closed by the frame's end mark.
            assert!(writer.bytes_written() >= before + 8, "{encoding:?}");
            if encoding == IpcCompression::None {
                assert_eq!(writer.bytes_written(), before + 8);
            }
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
    /// Random access must not decompress any preceding record batch:
    /// zeroing every earlier batch's bytes must not affect the read.
    #[test]
    fn indexed_spill_reads_only_requested_batch() {
        use std::io::{Seek, SeekFrom};
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        for codec in [IpcCompression::None, IpcCompression::Lz4] {
            let mut file = tempfile::tempfile().unwrap();
            let mut writer =
                IpcStreamWriter::try_new_with_compression(&mut file, &schema, codec).unwrap();
            let mut inputs = Vec::new();
            for i in 0..8 {
                let values = (0..65536).map(|n| i * 65536 + n).collect::<Vec<i64>>();
                let batch =
                    RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(values))])
                        .unwrap();
                writer.write(&batch).unwrap();
                inputs.push(batch);
            }
            writer.finish().unwrap();
            let offsets = writer.batch_offsets().unwrap().to_vec();
            drop(writer);
            file.seek(SeekFrom::Start(offsets[0])).unwrap();
            let zeros = usize::try_from(offsets[7] - offsets[0]).unwrap();
            file.write_all(&vec![0; zeros]).unwrap();
            let indexed = IpcIndexedReader::try_new(file, offsets[0]).unwrap();
            let last = indexed.read_batch(offsets[7], offsets[8]).unwrap();
            assert_eq!(last, inputs[7], "{codec:?}");
        }
    }
    /// The file and parsed schema belong to the level, not individual jobs.
    /// Damaging the already-read schema must not affect subsequent batches.
    #[test]
    fn indexed_spill_caches_schema_and_shares_one_open_file() {
        use std::io::{Seek, SeekFrom};
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
        for codec in [IpcCompression::None, IpcCompression::Lz4] {
            let mut file = tempfile::tempfile().unwrap();
            let mut writer =
                IpcStreamWriter::try_new_with_compression(&mut file, &schema, codec).unwrap();
            let batches: Vec<_> = (0..7)
                .map(|id| {
                    RecordBatch::try_new(
                        schema.clone(),
                        vec![Arc::new(Int64Array::from(vec![id; 65536]))],
                    )
                    .unwrap()
                })
                .collect();
            for batch in &batches {
                writer.write(batch).unwrap();
            }
            writer.finish().unwrap();
            let offsets = writer.batch_offsets().unwrap().to_vec();
            drop(writer);
            let indexed = IpcIndexedReader::try_new(file.try_clone().unwrap(), offsets[0]).unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&[0; 8]).unwrap();
            std::thread::scope(|scope| {
                let readers: Vec<_> = (0..7)
                    .rev()
                    .map(|i| {
                        let indexed = &indexed;
                        let offsets = &offsets;
                        scope.spawn(move || indexed.read_batch(offsets[i], offsets[i + 1]).unwrap())
                    })
                    .collect();
                for (i, reader) in (0..7).rev().zip(readers) {
                    assert_eq!(reader.join().unwrap(), batches[i], "{codec:?}, batch {i}");
                }
            });
        }
    }

    #[test]
    fn indexed_spill_keeps_dictionary_context_on_serial_reader() {
        use arrow_array::{types::Int32Type, DictionaryArray};
        for codec in [IpcCompression::None, IpcCompression::Lz4] {
            let first: DictionaryArray<Int32Type> =
                vec![Some("alpha"), Some("beta")].into_iter().collect();
            let second: DictionaryArray<Int32Type> =
                vec![Some("gamma"), Some("delta")].into_iter().collect();
            let schema = Arc::new(Schema::new(vec![Field::new(
                "name",
                first.data_type().clone(),
                false,
            )]));
            let batches = [first, second]
                .into_iter()
                .map(|array| RecordBatch::try_new(schema.clone(), vec![Arc::new(array)]).unwrap())
                .collect::<Vec<_>>();
            let mut data = Vec::new();
            let mut writer =
                IpcStreamWriter::try_new_with_compression(&mut data, &schema, codec).unwrap();
            for batch in &batches {
                writer.write(batch).unwrap();
            }
            writer.finish().unwrap();
            assert!(writer.batch_offsets().is_none());
            drop(writer);
            let actual = IpcStreamReader::try_new(Cursor::new(data), None)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(actual, batches);
        }
        let nested = DataType::List(Arc::new(Field::new(
            "item",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            true,
        )));
        let schema = Schema::new(vec![Field::new("nested", nested, true)]);
        let writer = IpcStreamWriter::try_new(Vec::new(), &schema).unwrap();
        assert!(writer.batch_offsets().is_none());
    }
}
