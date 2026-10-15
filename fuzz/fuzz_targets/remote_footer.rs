//! Fuzz the remote GeoParquet reader (untrusted-bytes surface: an object at a
//! `s3://`, `gs://` or `https://` URL), with an in-memory object store in
//! place of the network: the ranged footer fetch and its range checks
//! (#430, #586), row-group staging, and a full read.
//!
//! Needs the `remote` feature: `cargo fuzz run --features remote remote_footer`.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::input::fuzz_remote_footer(data);
});
