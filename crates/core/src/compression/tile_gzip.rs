//! Bounded memoization of the gzip function, not a tile-dedup decision.
//!
//! The writer still receives a body for every tile and verifies its bytes in
//! serial order. Equal raw bytes can share compression regardless of which
//! Rayon worker reaches them first: gzip is deterministic.

use std::io;
#[expect(
    clippy::disallowed_types,
    reason = "#733: the bounded gzip cache's configured cap belongs to its process-wide owner"
)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
#[expect(
    clippy::disallowed_types,
    reason = "#733: bounded gzip cache owns process-wide memoization"
)]
use std::sync::OnceLock;

use crate::dedup::TileHasher;

// #733: all repeats in the Madagascar profile were <= 1345 raw bytes. Bypass
// large tiles without hashing or allocating cache state (Moldova is the
// zero-duplicate control, and dense tiles must not pay a second memory floor).
const MAX_RAW_BYTES: usize = 1536;
const SHARDS: usize = 32;
const ENTRIES_PER_SHARD: usize = 128;
/// The payload cap a caller asks for, and the cap before any caller
/// configures one: 8 MiB.
const DEFAULT_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
/// The cache's payload in [`SHARDS`] equal slices, the unit an export's
/// `MemoryBudget` grants it in (#788 E1): one shard's share of the
/// default cap.
pub(crate) const PAYLOAD_SLICE_BYTES: usize = DEFAULT_PAYLOAD_BYTES / SHARDS;
/// Slices in the default cap.
pub(crate) const PAYLOAD_SLICES: usize = SHARDS;
#[expect(
    clippy::disallowed_types,
    reason = "#733: the bounded gzip cache's configured cap belongs to its process-wide owner"
)]
static PAYLOAD_CAP: AtomicUsize = AtomicUsize::new(DEFAULT_PAYLOAD_BYTES);

/// Set the cache payload cap to what the caller's `MemoryBudget` granted
/// (#788 E1). The cache keeps no sizing policy of its own; a lowered cap
/// evicts down to it as each shard next stores an entry.
pub(crate) fn configure_payload_cap(bytes: usize) {
    PAYLOAD_CAP.store(bytes, Ordering::Relaxed);
}

/// Process-wide cache payload budget: the configured cap. Charge two
/// allocator headers/padding conservatively as 64 B per entry. In addition,
/// at most `SHARDS * ENTRIES_PER_SHARD * size_of::<Entry>()` bytes back the
/// fixed-capacity slot vectors, plus the static mutexes. With the current
/// Entry this is < 8.25 MiB total at the default cap. The cache is lazy and
/// lasts across waves/exports.
fn payload_budget() -> usize {
    PAYLOAD_CAP.load(Ordering::Relaxed)
}
const ALLOCATION_CHARGE: usize = 64;

struct Entry {
    hash: u64,
    raw: Box<[u8]>,
    gzip: Box<[u8]>,
    hits: u32,
    touched: u64,
}

impl Entry {
    fn charge(&self) -> usize {
        self.raw.len() + self.gzip.len() + ALLOCATION_CHARGE
    }
}

#[derive(Default)]
struct Shard {
    entries: Vec<Entry>,
    used: usize,
    clock: u64,
}

impl Shard {
    fn compress(&mut self, hash: u64, raw: &[u8]) -> io::Result<Vec<u8>> {
        self.clock = self.clock.wrapping_add(1);
        // The hash is only an index hint. Even deliberate collisions must
        // compare raw bytes before sharing the gzip result.
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|e| e.hash == hash && &*e.raw == raw)
        {
            entry.hits = entry.hits.saturating_add(1);
            entry.touched = self.clock;
            return Ok(entry.gzip.to_vec());
        }
        // Hold only this shard's lock through compression. Concurrent copies
        // of one payload do the work once; unrelated shards remain parallel.
        let gzip = super::compress(raw, super::Compression::Gzip)?;
        let charge = raw.len() + gzip.len() + ALLOCATION_CHARGE;
        if charge > payload_budget() / SHARDS {
            return Ok(gzip);
        }
        while !self.entries.is_empty()
            && (self.used + charge > payload_budget() / SHARDS
                || self.entries.len() == ENTRIES_PER_SHARD)
        {
            // Keep frequent uniform tiles through the stream of one-off
            // boundary tiles. Oldest wins ties, deterministically per shard.
            let victim = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| (e.hits, e.touched))
                .unwrap()
                .0;
            self.used -= self.entries.swap_remove(victim).charge();
        }
        if self.entries.capacity() == 0 {
            self.entries = Vec::with_capacity(ENTRIES_PER_SHARD);
        }
        self.entries.push(Entry {
            hash,
            raw: raw.into(),
            gzip: gzip.clone().into_boxed_slice(),
            hits: 1,
            touched: self.clock,
        });
        self.used += charge;
        Ok(gzip)
    }
}

pub(crate) fn compress(raw: &[u8]) -> io::Result<Vec<u8>> {
    if raw.len() > MAX_RAW_BYTES {
        return super::compress(raw, super::Compression::Gzip);
    }
    #[expect(
        clippy::disallowed_types,
        reason = "#733: this bounded content cache is the memoization owner"
    )]
    static CACHE: OnceLock<[Mutex<Shard>; SHARDS]> = OnceLock::new();
    let shards = CACHE.get_or_init(|| std::array::from_fn(|_| Mutex::new(Shard::default())));
    let hash = TileHasher::hash(raw);
    shards[hash as usize % SHARDS]
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .compress(hash, raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_collisions_compare_raw_bytes() {
        let mut cache = Shard::default();
        let a = cache.compress(7, b"first tile").unwrap();
        let b = cache.compress(7, b"other tile").unwrap();
        assert_ne!(a, b);
        assert_eq!(cache.compress(7, b"first tile").unwrap(), a);
        assert_eq!(cache.compress(7, b"other tile").unwrap(), b);
    }

    #[test]
    fn cache_accounts_and_bounds_payload_and_slots() {
        let mut cache = Shard::default();
        for i in 0..1000u64 {
            let raw = i.to_le_bytes().repeat(MAX_RAW_BYTES / 8);
            cache.compress(i, &raw).unwrap();
            assert_eq!(
                cache.used,
                cache.entries.iter().map(Entry::charge).sum::<usize>()
            );
            assert!(cache.used <= payload_budget() / SHARDS);
            assert!(cache.entries.len() <= ENTRIES_PER_SHARD);
            assert_eq!(cache.entries.capacity(), ENTRIES_PER_SHARD);
        }
        assert!(
            payload_budget() + SHARDS * ENTRIES_PER_SHARD * std::mem::size_of::<Entry>()
                < 33 * 1024 * 1024 / 4
        );
    }
}
