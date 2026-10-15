//! Fuzz a whole overview file through `OverviewReader` and `validate`
//! (untrusted-bytes surface: the file handed to `export-pmtiles` or
//! `validate`): footer, `geo:overviews` bands, level lookups, pruning, and a
//! full read of every band.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::overview::reader::fuzz_overview_file(data);
});
