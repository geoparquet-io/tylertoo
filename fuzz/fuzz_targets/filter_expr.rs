//! Fuzz the `--filter` expression parser. Its input is a command-line
//! string rather than file bytes, but it is a recursive-descent parser over
//! user text with nested parentheses, `NOT`, and quoted literals — deep
//! nesting or an unterminated quote must come back as `FilterError`, never
//! as a stack overflow or a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tylertoo_core::overview::filter::parse_filter;

fuzz_target!(|data: &[u8]| {
    if let Ok(src) = std::str::from_utf8(data) {
        if let Ok(expr) = parse_filter(src) {
            let _ = expr.column_names();
        }
    }
});
