//! Fuzz the string parsers behind CLI and Python flags: `--representation`,
//! `--band`, `--tile-range`, `--shard`, `--max-zoom`, `--feature-order`, the
//! input URL scheme sniff, and the public `covering::parse_bounds`. Lower stakes than a
//! file (the person running the command typed it), but cheap to cover.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        tylertoo_core::fuzz::fuzz_cli_args(s);
    }
});
