//! Fuzz the string parsers behind CLI and Python flags: `--bounds`,
//! `--representation`, `--band`, `--tile-range`, `--shard`, `--max-zoom`,
//! `--feature-order`, and the input URL scheme sniff. Lower stakes than a
//! file (the person running the command typed it), but cheap to cover.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        tylertoo_core::fuzz::fuzz_cli_args(s);
    }
});
