//! Fuzz `pyramid::BandArchive` end to end (untrusted-bytes surface: a whole
//! archive passed as `--band`), through the `fuzzing`-feature hook in core.
//!
//! This is the one target that crosses the decompression cap (#417): header,
//! directory walk with its expansion budgets, metadata decompress + JSON
//! parse, and a read of every addressed tile body. A bomb here should come
//! back as an `Err`, never as an OOM.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::pyramid::fuzz_band_archive(data);
});
