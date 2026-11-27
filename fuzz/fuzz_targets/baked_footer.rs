//! Fuzz the footer key-value reader and rewriter behind `geo:pmtiles`
//! (`bake`, `strip`, and every archive reader handed a Parquet file). The
//! input is the thrift footer, without its length and `PAR1` trailer.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::internal::fuzz::fuzz_baked_footer(data);
});
