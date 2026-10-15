//! Fuzz the GeoParquet footer `overviews` JSON parser (untrusted-bytes
//! surface: this string comes verbatim from any file a user opens). A
//! document that parses is then checked against a row-group count taken from
//! the first input byte, as `OverviewReader::open` checks it against the
//! file's.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&n, json)) = data.split_first() else {
        return;
    };
    if let Ok(s) = std::str::from_utf8(json) {
        if let Ok(meta) = tylertoo_core::overview::level::OverviewsMeta::from_json(s) {
            let _ = meta.validate(i64::from(n));
        }
    }
});
