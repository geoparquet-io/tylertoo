//! The bounded spill codec must not select multi-MiB scratch from input size.
use std::sync::Arc;

use arrow_array::{ArrayRef, Int64Array, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use tylertoo_io::{IpcCompression, IpcStreamReader, IpcStreamWriter};

#[test]
fn lz4_spill_bounds_frame_blocks_to_64k() {
    let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
    let input = RecordBatch::try_new(
        schema.clone(),
        vec![Arc::new(Int64Array::from(vec![0; 1_048_576])) as ArrayRef],
    )
    .unwrap();
    let mut bytes = Vec::new();
    let mut writer =
        IpcStreamWriter::try_new_with_compression(&mut bytes, &schema, IpcCompression::Lz4)
            .unwrap();
    writer.write(&input).unwrap();
    writer.write(&input.slice(1, 37)).unwrap();
    writer.finish().unwrap();
    drop(writer);
    // Each frame in the private IPC stream bounds encoder and decoder block
    // scratch even for an eight-MiB Arrow buffer. BD bits 4..6 = 4
    // denote LZ4's 64-KiB block bound; the old body codec chose 4 MiB.
    assert_eq!(&bytes[..4], &[0x04, 0x22, 0x4d, 0x18]);
    assert_eq!(bytes[5] & 0x70, 0x40);
    let mut reader = IpcStreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
    assert_eq!(reader.next().unwrap().unwrap(), input);
    assert_eq!(reader.next().unwrap().unwrap(), input.slice(1, 37));
    assert!(reader.next().is_none());
}

#[test]
fn invalid_later_frame_is_an_error_instead_of_end_of_stream() {
    let schema = Schema::new(vec![Field::new("id", DataType::Int64, false)]);
    let batch = RecordBatch::try_new(
        Arc::new(schema.clone()),
        vec![Arc::new(Int64Array::from(vec![7; 32]))],
    )
    .unwrap();
    let mut bytes = Vec::new();
    let mut writer =
        IpcStreamWriter::try_new_with_compression(&mut bytes, &schema, IpcCompression::Lz4)
            .unwrap();
    writer.write(&batch).unwrap();
    writer.finish().unwrap();
    drop(writer);
    let second = bytes
        .windows(4)
        .enumerate()
        .filter(|(_, window)| *window == [0x04, 0x22, 0x4d, 0x18])
        .nth(1)
        .unwrap()
        .0;
    bytes[second] ^= 0xff;
    let mut reader = IpcStreamReader::try_new(std::io::Cursor::new(bytes), None).unwrap();
    assert!(reader.next().unwrap().is_err());
    assert!(reader.next().is_none());
}
