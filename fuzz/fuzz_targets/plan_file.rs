//! Fuzz a saved `--plan` file (untrusted-bytes surface: a plan is a file a
//! user can be handed). The input is the v6 loss partition and Arrow IPC
//! payload; the shared writer restores magic and checksum so mutations reach
//! the decoder rather than stopping at the checksum (#512).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::overview::fuzz_convert_plan(data);
});
