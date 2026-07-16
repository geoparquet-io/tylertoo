//! Fuzz the GeoParquet footer `overviews` JSON parser (untrusted-bytes
//! surface: this string comes verbatim from any file a user opens).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = tylertoo_core::overview::level::OverviewsMeta::from_json(s);
    }
});
