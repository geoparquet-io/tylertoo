//! Fuzz the GeoParquet `geo` key-value JSON (untrusted-bytes surface: the
//! string comes verbatim from the footer of any file a user opens), through
//! every reader of it: CRS detection and classification (#518), the bbox
//! covering parse, and the primary-column lookup.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = tylertoo_core::overview::convert::fuzz_geo_metadata(s);
    }
});
