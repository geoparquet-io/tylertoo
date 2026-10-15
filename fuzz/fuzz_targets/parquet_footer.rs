//! Fuzz every footer-only reader of a Parquet file (untrusted-bytes surface:
//! the footer of any GeoParquet or overview file a user opens): the overview
//! checklist, `geo:overviews` validation, row-group bboxes, the batch and
//! spill sizing heuristics, and `--filter` pushdown on column statistics.
//! The input is the thrift footer, without its length and `PAR1` trailer.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::overview::check::fuzz_parquet_footer(data);
});
