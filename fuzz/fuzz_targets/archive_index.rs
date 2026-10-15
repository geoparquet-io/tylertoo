//! Fuzz `archive_index::ArchiveIndex` and the `stats` pass over it
//! (untrusted-bytes surface: an archive handed to `stats` or `merge`).
//! Unlike `band_archive`, any compression and tile type gets past the open.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::fuzz::fuzz_archive_index(data);
});
