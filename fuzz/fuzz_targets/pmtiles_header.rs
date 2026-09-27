//! Fuzz the PMTiles v3 header parser (untrusted-bytes surface: the first 127
//! bytes of any archive handed to `decode`, `merge`, or `--band`).
//!
//! A header that parses must also survive its own round trip: `to_bytes`
//! re-encodes the repaired fields, and the result has to parse again. That
//! is the invariant the read side relies on when it copies a header forward.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tylertoo_core::pmtiles_writer::Header;

fuzz_target!(|data: &[u8]| {
    if let Ok(header) = Header::from_bytes(data) {
        let bytes = header.to_bytes();
        Header::from_bytes(&bytes).expect("a parsed header re-encodes to a parseable header");
    }
});
