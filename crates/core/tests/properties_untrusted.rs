//! Property: the untrusted readers turn arbitrary bytes into errors, never
//! panics (#677; replaces mutation testing). Inputs are bounded and the case
//! count is capped. Valid-prefix cases (a real magic number, then noise)
//! reach past the first check. The long runs live in `fuzz/`.
use proptest::prelude::*;
use std::io::Cursor;
use tylertoo_io::{open_ipc, open_parquet};

fn noise() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..2048),
        // Looks like a Parquet file: magic, noise, footer length, magic.
        (prop::collection::vec(any::<u8>(), 0..512), any::<u32>()).prop_map(|(mid, len)| {
            let mut v = b"PAR1".to_vec();
            v.extend(mid);
            v.extend(len.to_le_bytes());
            v.extend(b"PAR1");
            v
        }),
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
    fn open_parquet_never_panics(bytes in noise()) {
        let _ = open_parquet(bytes::Bytes::from(bytes));
    }

    #[test]
    fn open_ipc_never_panics(bytes in noise()) {
        if let Ok(reader) = open_ipc(Cursor::new(bytes)) {
            for batch in reader.take(4) {
                let _ = batch;
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
        let _ = tylertoo_core::decode::fuzz_decode_tile(z, 0, 0, &bytes);
        let _ = tylertoo_core::decode::fuzz_decode_archive(&bytes);
    }
}
