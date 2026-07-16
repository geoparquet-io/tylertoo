//! Fuzz the WKB geometry decoder (untrusted-bytes surface: geometry blobs
//! come verbatim from any GeoParquet file a user opens).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::wkb::wkb_to_geometry(data);
});
