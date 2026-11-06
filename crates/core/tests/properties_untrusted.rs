//! Property: the untrusted readers turn arbitrary bytes into errors, never
//! panics (#677; replaces mutation testing). Inputs are bounded and the case
//! count is capped. Valid-prefix cases (a real magic number, then noise)
//! reach past the first check. The long runs live in `fuzz/`. Parquet is
//! covered by the deeper mutated-valid-file case below; IPC keeps the noise
//! case because there is no valid IPC builder here without a new dependency.
use proptest::prelude::*;
use std::io::Cursor;
use tylertoo_io::{open_ipc, open_parquet};

fn noise() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..2048),
        // Looks like an Arrow IPC file: magic, noise, footer length, magic.
        (prop::collection::vec(any::<u8>(), 0..512), any::<u32>()).prop_map(|(mid, len)| {
            let mut v = b"ARROW1\0\0".to_vec();
            v.extend(mid);
            v.extend(len.to_le_bytes());
            v.extend(b"ARROW1");
            v
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn open_ipc_never_panics(bytes in noise()) {
        if let Ok(reader) = open_ipc(Cursor::new(bytes)) {
            for batch in reader.take(4) {
                let _ = batch;
            }
        }
    }
}

/// A small valid Parquet file (ints, strings, nulls).
fn valid_parquet() -> Vec<u8> {
    use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use std::sync::Arc;
    use tylertoo_io::test_support::ArrowWriter;
    let ids: ArrayRef = Arc::new(Int64Array::from_iter(
        (0..64).map(|i| (i % 5 != 0).then_some(i)),
    ));
    let names: ArrayRef = Arc::new(StringArray::from_iter(
        (0..64).map(|i| (i % 7 != 0).then(|| format!("name-{i}"))),
    ));
    let batch = RecordBatch::try_from_iter([("id", ids), ("name", names)]).unwrap();
    let mut out = Vec::new();
    let mut w = ArrowWriter::try_new(&mut out, batch.schema(), None).unwrap();
    w.write(&batch).unwrap();
    w.close().unwrap();
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Start from a valid file so the footer, schema and page decoders are
    /// reached, then flip bytes (biased to the footer): still no panic.
    #[test]
    fn mutated_valid_parquet_never_panics(
        flips in prop::collection::vec((any::<prop::sample::Index>(), any::<u8>(), any::<bool>()), 1..6),
    ) {
        let mut bytes = valid_parquet();
        let len = bytes.len();
        for (at, val, footer) in flips {
            let i = if footer { len - 1 - at.index(len.min(400)) } else { at.index(len) };
            bytes[i] ^= val | 1;
        }
        if let Ok(source) = open_parquet(bytes::Bytes::from(bytes)) {
            if let Ok(reader) = source.build() {
                for batch in reader.take(4) {
                    let _ = batch;
                }
            }
        }
    }
}

#[cfg(feature = "fuzzing")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn decoder_hooks_never_panic(
        bytes in prop::collection::vec(any::<u8>(), 0..1024),
        z in 0u8..=22,
    ) {
        let _ = tylertoo_core::internal::decode::fuzz_decode_tile(z, 0, 0, &bytes);
        let _ = tylertoo_core::internal::decode::fuzz_decode_archive(&bytes);
    }
}
