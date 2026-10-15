//! Fuzz an input archive's `vector_layers` metadata as `pyramid` and
//! `merge` read it (untrusted-bytes surface: the metadata JSON of any
//! `--band` or merge input). Two leading bytes are the header's zoom range;
//! the hook panics if a declared range comes back out of order or past z31.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::pyramid::fuzz_layer_zooms(data);
});
