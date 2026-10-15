//! Fuzz a `--shard-plan` file (untrusted-bytes surface: a plan is cut once
//! and handed to every machine of a sharded build): the JSON, its structure
//! checks, and every range query a shard makes of it.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tylertoo_core::fuzz::fuzz_shard_plan(data);
});
