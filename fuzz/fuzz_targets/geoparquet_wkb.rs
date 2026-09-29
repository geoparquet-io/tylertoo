//! Fuzz the GeoParquet WKB column decoder (untrusted-bytes surface: every
//! value in the WKB geometry column of any GeoParquet file a user opens).
//!
//! Each input is one WKB value, decoded as `batch_processor` decodes a column:
//! geoarrow-array's `WkbArray` handing it to the `wkb` crate's reader, behind
//! tylertoo's bounds check. The hook also panics when the check and the reader
//! disagree about a value, so a gap in the check shows up as a crash rather
//! than waiting for an input that aborts.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::batch_processor::fuzz_geoparquet_wkb(data);
});
