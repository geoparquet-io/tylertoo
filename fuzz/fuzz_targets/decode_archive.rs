//! Fuzz `decode` end to end on an in-memory archive (untrusted-bytes
//! surface: the archive handed to `tylertoo decode`): header, directory walk
//! and budgets, capped tile decompression, MVT decode and the property
//! checks, short of writing GeoParquet.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::decode::fuzz_decode_archive(data);
});
