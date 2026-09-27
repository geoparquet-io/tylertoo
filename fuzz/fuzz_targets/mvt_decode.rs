//! Fuzz the MVT tile decoder (untrusted-bytes surface: every tile body
//! `decode` reads from a foreign archive, after decompression).
//!
//! The first byte picks the zoom (`z & 31`), so the tile-local-to-lon/lat
//! transform is exercised across the whole `z <= 31` range the decoder
//! accepts; the rest is the uncompressed protobuf.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&first, tile)) = data.split_first() else {
        return;
    };
    let z = first & 31;
    // A tile at the far corner of its zoom, so the coordinate lift sees the
    // largest world offsets the zoom allows.
    let max = if z == 0 { 0 } else { (1u32 << z) - 1 };
    let _ = tylertoo_core::decode::fuzz_decode_tile(z, max, max, tile);
});
