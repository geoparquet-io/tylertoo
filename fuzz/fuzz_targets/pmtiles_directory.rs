//! Fuzz the PMTiles directory decoder (untrusted-bytes surface: root and leaf
//! directories are archive-controlled, and #397 was exactly the class of bug
//! libFuzzer finds here — a trusted entry count sizing an allocation).
//!
//! A directory that decodes must round-trip: `encode_directory` is the
//! writer's inverse, and the read side assumes `decode(encode(d)) == d`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use tylertoo_core::pmtiles_writer::{decode_directory, encode_directory};

fuzz_target!(|data: &[u8]| {
    if let Some(entries) = decode_directory(data) {
        let encoded = encode_directory(&entries);
        let again = decode_directory(&encoded).expect("re-encoded directory decodes");
        assert_eq!(
            again.len(),
            entries.len(),
            "entry count survives the round trip"
        );
        for (a, b) in entries.iter().zip(&again) {
            assert_eq!(
                (a.tile_id, a.offset, a.length, a.run_length),
                (b.tile_id, b.offset, b.length, b.run_length),
                "entry survives the round trip"
            );
        }
    }
});
