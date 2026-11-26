//! #876: the export's memory probe returns mimalloc's freed pages to the OS
//! before it reads what the process holds. A `tiles` run probes just after
//! the convert freed its pass-2 state, which mimalloc purges only a second
//! later, so up to ~1.8 GiB of free memory counted as used and z13 spilled.
//! Runs on the CLI's own allocator, which the library tests do not.
#![cfg(all(target_os = "linux", feature = "mimalloc", not(feature = "dhat-heap")))]

use rayon::prelude::*;
use tylertoo_core::internal::overview::export::{resolve_partition_wave, PARTITION_WAVE_AUTO};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn rss_anon_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let kib = status
        .lines()
        .find_map(|l| l.strip_prefix("RssAnon:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .unwrap();
    kib << 10
}

/// `total` bytes in `chunk`-byte allocations, built on the pool's workers.
fn on_the_pool(total: usize, chunk: usize) -> Vec<Vec<u8>> {
    (0..total / chunk)
        .into_par_iter()
        .map(|i| vec![u8::try_from(i % 251).unwrap() + 1; chunk])
        .collect()
}

/// Built on the pool's workers and freed here, as pass 2's sinks are: half in
/// 1 MiB chunks, half in 4 KiB ones. The probe collects once per process, so
/// one test covers both sizes: releasing more than the large half can supply
/// means the small chunks were returned too.
#[test]
fn the_export_probe_returns_freed_mimalloc_pages() {
    assert!(std::env::var_os("TYLERTOO_AUTO_MEM_LIMIT_BYTES").is_none());
    const HALF: usize = 128 << 20;
    let want = (HALF + HALF / 2) as u64;
    let base = rss_anon_bytes();
    let large = on_the_pool(HALF, 1 << 20);
    let small = on_the_pool(HALF, 4 << 10);
    drop((large, small));
    let before = rss_anon_bytes();
    let kept = before.saturating_sub(base);
    if kept < want {
        // e.g. MIMALLOC_PURGE_DELAY=0: mimalloc returned the pages on free.
        eprintln!(
            "mimalloc kept {} MiB after the free; nothing to release",
            kept >> 20
        );
        return;
    }
    let _ = resolve_partition_wave(PARTITION_WAVE_AUTO);
    let released = before.saturating_sub(rss_anon_bytes());
    assert!(
        released >= want,
        "released {} of {} MiB",
        released >> 20,
        kept >> 20
    );
}
