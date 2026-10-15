//! Fuzz the capped decompressors (untrusted-bytes surface: every directory,
//! metadata block and tile body of an input archive). The first byte picks
//! the codec; the hook panics if the output passes the cap.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::fuzz::fuzz_decompress(data);
});
