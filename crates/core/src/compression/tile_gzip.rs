//! Bounded memoization of the gzip function, not a tile-dedup decision.
//!
//! The writer still receives a body for every tile and verifies its bytes in
//! serial order. Equal raw bytes can share compression regardless of which
//! Rayon worker reaches them first: gzip is deterministic.

use std::io;
use std::sync::Mutex;

use crate::dedup::TileHasher;

// #733: all repeats in the Madagascar profile were <= 1345 raw bytes. Bypass
// large tiles without hashing or allocating cache state (Moldova is the
// zero-duplicate control, and dense tiles must not pay a second memory floor).
const MAX_RAW_BYTES: usize = 1536;
const SHARDS: usize = 32;
const ENTRIES_PER_SHARD: usize = 128;
/// The measured per-shard payload unit an export's budget grants.
pub(crate) const PAYLOAD_SLICE_BYTES: usize = 256 * 1024;
/// Shards in the measured 8 MiB working set.
pub(crate) const PAYLOAD_SLICES: usize = SHARDS;

/// One export's immutable granted cap and byte-verified compression memo.
/// Payload charges include 64 B per entry for allocation headers/padding;
/// fixed-capacity entry slots add less than 0.25 MiB at the measured cap.
/// No configuration or entries are shared with another export.
pub(crate) struct TileGzipCache {
    payload_cap: usize,
    shards: [Mutex<Shard>; SHARDS],
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
    fn compress(&mut self, hash: u64, raw: &[u8], cap: usize) -> io::Result<Vec<u8>> {
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
        if charge > cap {
            return Ok(gzip);
        }
        while !self.entries.is_empty()
            && (self.used + charge > cap || self.entries.len() == ENTRIES_PER_SHARD)
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

impl TileGzipCache {
    pub(crate) fn new(payload_cap: usize) -> Self {
        Self {
            payload_cap,
            shards: std::array::from_fn(|_| Mutex::new(Shard::default())),
        }
    }

    pub(crate) fn compress(&self, raw: &[u8]) -> io::Result<Vec<u8>> {
        if raw.len() > MAX_RAW_BYTES {
            return super::compress(raw, super::Compression::Gzip);
        }
        let hash = TileHasher::hash(raw);
        self.shards[hash as usize % SHARDS]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .compress(hash, raw, self.payload_cap / SHARDS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two live exports must keep the caps their respective ledgers granted.
    #[test]
    fn export_caches_keep_independent_granted_caps() {
        let a = TileGzipCache::new(8 * 1024 * 1024);
        let b = TileGzipCache::new(0);
        let (bodies, calls) = crate::compression::gzip_counter::measure(|| {
            [
                a.compress(b"independent export caps").unwrap(),
                b.compress(b"independent export caps").unwrap(),
                a.compress(b"independent export caps").unwrap(),
                b.compress(b"independent export caps").unwrap(),
            ]
        });
        assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(
            calls, 3,
            "cached export: one gzip; zero-cap export: two gzips"
        );
    }

    #[test]
    fn hash_collisions_compare_raw_bytes() {
        let mut cache = Shard::default();
        let a = cache
            .compress(7, b"first tile", PAYLOAD_SLICE_BYTES)
            .unwrap();
        let b = cache
            .compress(7, b"other tile", PAYLOAD_SLICE_BYTES)
            .unwrap();
        assert_ne!(a, b);
        assert_eq!(
            cache
                .compress(7, b"first tile", PAYLOAD_SLICE_BYTES)
                .unwrap(),
            a
        );
        assert_eq!(
            cache
                .compress(7, b"other tile", PAYLOAD_SLICE_BYTES)
                .unwrap(),
            b
        );
    }

    #[test]
    fn cache_accounts_and_bounds_payload_and_slots() {
        let mut cache = Shard::default();
        for i in 0..1000u64 {
            let raw = i.to_le_bytes().repeat(MAX_RAW_BYTES / 8);
            cache.compress(i, &raw, PAYLOAD_SLICE_BYTES).unwrap();
            assert_eq!(
                cache.used,
                cache.entries.iter().map(Entry::charge).sum::<usize>()
            );
            assert!(cache.used <= PAYLOAD_SLICE_BYTES);
            assert!(cache.entries.len() <= ENTRIES_PER_SHARD);
            assert_eq!(cache.entries.capacity(), ENTRIES_PER_SHARD);
        }
        assert!(
            PAYLOAD_SLICE_BYTES * SHARDS
                + SHARDS * ENTRIES_PER_SHARD * std::mem::size_of::<Entry>()
                < 33 * 1024 * 1024 / 4
        );
    }
}
