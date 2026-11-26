//! The temporary overview `tiles` holds in memory (#923).
//!
//! Under `auto` and `speed` the intermediate overview is written here, not
//! to a temp file. Its bytes are cut into one chunk per row group (plus the
//! footer). A chunk is either resident (anonymous memory, charged to the run's
//! [`MemoryBudget`]) or evicted, written at its own offset to one anonymous
//! temp file. A regular file cannot be kept out of writeback, so an overview
//! that lives in one is partly flushed to disk under a cgroup's dirty limit
//! and slows the archive's fsync. A memfd cannot be reclaimed without swap.
//! This store keeps what the budget grants in RAM and gives it back on
//! demand: what does not fit is ordinary page cache, which is today's
//! behaviour.
//!
//! Eviction takes the highest-offset resident chunk first. The writer
//! writes levels in band order and the export reads them in the same order
//! (duplicating: level by level; partitioning: one read of every band), so
//! the last chunk is the one the export reads last.

use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{self, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use bytes::Bytes;

use super::level::MemoryProfile;
use super::memory::{MemoryBudget, MemoryPhase, Price, PriceBasis};

/// A `Copy` handle on a live [`OverviewStore`], so the `Copy`
/// [`MemoryBudget`] can hold the store it reclaims from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StoreId(u64);

/// The live stores by id, and the next id.
#[expect(
    clippy::type_complexity,
    reason = "#923: the registry StoreId resolves against"
)]
static LIVE: Mutex<(u64, Vec<(u64, Weak<OverviewStore>)>)> = Mutex::new((0, Vec::new()));

impl StoreId {
    /// The store, while it lives.
    pub(crate) fn get(self) -> Option<Arc<OverviewStore>> {
        lock(&LIVE)
            .1
            .iter()
            .find(|(id, _)| *id == self.0)
            .and_then(|(_, w)| w.upgrade())
    }
}

/// The chunked, budget-owned temporary overview. See the module docs.
#[derive(Debug)]
#[expect(
    clippy::disallowed_types,
    reason = "#923: the byte counters a run's budget reads live"
)]
pub struct OverviewStore {
    id: u64,
    /// Where the eviction file is created (on the first eviction).
    dir: PathBuf,
    inner: Mutex<Inner>,
    /// Serializes evictions, so their disk writes run outside `inner` and
    /// readers keep reading meanwhile.
    evicting: Mutex<()>,
    /// Bytes of resident chunks, read live by the budget that holds them.
    resident: atomic::AtomicU64,
    /// Resident bytes dropped after the export's last read of them.
    released: atomic::AtomicU64,
}

#[derive(Debug, Default)]
struct Inner {
    /// Sealed chunks, in file order.
    chunks: Vec<Chunk>,
    /// Bytes written since the last cut.
    open: Vec<u8>,
    /// Bytes written in all.
    len: u64,
    /// The eviction file: anonymous, so nothing is left in `dir`.
    file: Option<Arc<File>>,
    /// Bytes ever evicted to the file.
    on_disk: u64,
    /// The writer's budget: a chunk stays resident while it grants it.
    admit: Option<(MemoryBudget, MemoryProfile)>,
}

#[derive(Debug)]
struct Chunk {
    start: u64,
    len: u64,
    state: State,
}

#[derive(Debug)]
enum State {
    Resident(Bytes),
    Disk,
    /// Dropped after the export's last read of it.
    Freed,
}

/// One piece of a read, gathered under the lock and assembled outside it.
enum Piece {
    Mem(Bytes),
    Disk(u64, usize),
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

#[cfg(windows)]
fn write_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    while !buf.is_empty() {
        let n = std::os::windows::fs::FileExt::seek_write(file, buf, offset)?;
        buf = &buf[n..];
        offset += n as u64;
    }
    Ok(())
}

impl OverviewStore {
    /// An empty store whose evicted chunks go to an anonymous file in `dir`.
    #[expect(
        clippy::disallowed_types,
        reason = "#923: the byte counters a run's budget reads live"
    )]
    pub(crate) fn new(dir: PathBuf) -> Arc<Self> {
        let mut live = lock(&LIVE);
        live.0 += 1;
        let store = Arc::new(Self {
            id: live.0,
            dir,
            inner: Mutex::default(),
            evicting: Mutex::default(),
            resident: atomic::AtomicU64::new(0),
            released: atomic::AtomicU64::new(0),
        });
        live.1.push((store.id, Arc::downgrade(&store)));
        store
    }

    pub(crate) fn id(&self) -> StoreId {
        StoreId(self.id)
    }

    /// Admit the chunks the writer seals against `budget` under `profile`.
    /// Without it every chunk stays resident. An admission already set
    /// stays (a test's).
    pub(super) fn admit_under(&self, budget: MemoryBudget, profile: MemoryProfile) {
        lock(&self.inner).admit.get_or_insert((budget, profile));
    }

    /// Bytes written.
    pub fn len(&self) -> u64 {
        lock(&self.inner).len
    }

    /// Whether nothing was written.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes held in memory now.
    pub fn resident_bytes(&self) -> u64 {
        self.resident.load(Ordering::Acquire)
    }

    /// Bytes ever written to the eviction file.
    pub fn disk_bytes(&self) -> u64 {
        lock(&self.inner).on_disk
    }

    /// Resident bytes dropped after the export's last read of them.
    pub fn released_bytes(&self) -> u64 {
        self.released.load(Ordering::Acquire)
    }

    pub(crate) fn append(&self, buf: &[u8]) {
        let mut inner = lock(&self.inner);
        inner.open.extend_from_slice(buf);
        inner.len += buf.len() as u64;
    }

    fn file(&self, inner: &mut Inner) -> io::Result<Arc<File>> {
        if inner.file.is_none() {
            inner.file = Some(Arc::new(tempfile::tempfile_in(&self.dir)?));
        }
        Ok(Arc::clone(inner.file.as_ref().expect("just created")))
    }

    /// Seal the bytes written since the last cut as one chunk: resident when
    /// the writer's budget grants it, otherwise written to the file. The
    /// overview writer cuts at every row group's end (its flush).
    pub(crate) fn cut(&self) -> io::Result<()> {
        let mut inner = lock(&self.inner);
        if inner.open.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut inner.open);
        let len = data.len() as u64;
        let start = inner.len - len;
        let admitted = match &mut inner.admit {
            None => true,
            Some((budget, profile)) => {
                // Charged as a grant: out of the allowance one for one.
                let fits = budget.room(*profile) >= len;
                if fits {
                    let _held = budget.fit(
                        *profile,
                        MemoryPhase::Write,
                        Price::new(len, PriceBasis::Measured),
                        1,
                        1,
                    );
                }
                fits
            }
        };
        let state = if admitted {
            self.resident.fetch_add(len, Ordering::AcqRel);
            State::Resident(Bytes::from(data))
        } else {
            let file = self.file(&mut inner)?;
            write_at(&file, &data, start)?;
            inner.on_disk += len;
            State::Disk
        };
        inner.chunks.push(Chunk { start, len, state });
        Ok(())
    }

    /// `length` bytes from `start`, from memory or the file.
    pub(crate) fn read(&self, start: u64, length: usize) -> io::Result<Bytes> {
        let end = start.saturating_add(length as u64);
        let (pieces, file) = {
            let inner = lock(&self.inner);
            let sealed = inner.len - inner.open.len() as u64;
            if end > sealed {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("overview read {start}..{end} past its sealed {sealed} bytes"),
                ));
            }
            let first = inner.chunks.partition_point(|c| c.start + c.len <= start);
            let mut pieces = Vec::new();
            for c in inner.chunks[first..].iter().take_while(|c| c.start < end) {
                let lo = start.max(c.start);
                let hi = end.min(c.start + c.len);
                pieces.push(match &c.state {
                    State::Resident(b) => {
                        Piece::Mem(b.slice((lo - c.start) as usize..(hi - c.start) as usize))
                    }
                    State::Disk => Piece::Disk(lo, (hi - lo) as usize),
                    State::Freed => {
                        return Err(io::Error::other(format!(
                            "overview bytes {lo}..{hi} were released after their last read"
                        )))
                    }
                });
            }
            (pieces, inner.file.clone())
        };
        if let [Piece::Mem(b)] = pieces.as_slice() {
            return Ok(b.clone());
        }
        let mut out = Vec::with_capacity(length);
        for piece in pieces {
            match piece {
                Piece::Mem(b) => out.extend_from_slice(&b),
                Piece::Disk(at, n) => {
                    let file = file.as_ref().expect("an evicted chunk has a file");
                    let from = out.len();
                    out.resize(from + n, 0);
                    super::super::input::read_exact_at(file, &mut out[from..], at)?;
                }
            }
        }
        Ok(out.into())
    }

    /// Evict resident chunks, highest offset first, until `want` bytes are
    /// freed or none is resident. Returns the bytes freed.
    pub(crate) fn evict(&self, want: u64) -> io::Result<u64> {
        let _one = lock(&self.evicting);
        let mut freed = 0;
        while freed < want {
            let (idx, start, data, file) = {
                let mut inner = lock(&self.inner);
                let Some(idx) = inner
                    .chunks
                    .iter()
                    .rposition(|c| matches!(c.state, State::Resident(_)))
                else {
                    break;
                };
                let file = self.file(&mut inner)?;
                let c = &inner.chunks[idx];
                let State::Resident(data) = &c.state else {
                    unreachable!()
                };
                (idx, c.start, data.clone(), file)
            };
            write_at(&file, &data, start)?;
            let mut inner = lock(&self.inner);
            let c = &mut inner.chunks[idx];
            if matches!(c.state, State::Resident(_)) {
                c.state = State::Disk;
                let len = c.len;
                inner.on_disk += len;
                self.resident.fetch_sub(len, Ordering::AcqRel);
                freed += len;
            }
        }
        if freed > 0 {
            log::debug!("[overview] evicted {freed} B to disk");
        }
        Ok(freed)
    }

    /// Drop every chunk that ends at or before `end`: the export has read
    /// those bytes for the last time.
    pub(crate) fn free_below(&self, end: u64) {
        let mut inner = lock(&self.inner);
        let mut freed = 0;
        for c in inner
            .chunks
            .iter_mut()
            .take_while(|c| c.start + c.len <= end)
        {
            if matches!(c.state, State::Resident(_)) {
                freed += c.len;
            }
            c.state = State::Freed;
        }
        self.resident.fetch_sub(freed, Ordering::AcqRel);
        self.released.fetch_add(freed, Ordering::AcqRel);
    }
}

impl Drop for OverviewStore {
    fn drop(&mut self) {
        lock(&LIVE).1.retain(|(id, _)| *id != self.id);
    }
}

/// The overview writer's sink: the temp file, or the store. Its `flush`
/// cuts the store's chunk, and the writer flushes at every row group's end.
#[derive(Debug)]
pub(crate) enum OverviewSink {
    File(File),
    Store(Arc<OverviewStore>),
}

impl io::Write for OverviewSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::File(f) => f.write(buf),
            Self::Store(s) => {
                s.append(buf);
                Ok(buf.len())
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(f) => f.flush(),
            Self::Store(s) => s.cut(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn filled(chunks: &[&[u8]], budget: Option<MemoryBudget>) -> Arc<OverviewStore> {
        let dir = tempfile::tempdir().unwrap();
        let store = OverviewStore::new(dir.keep());
        if let Some(b) = budget {
            store.admit_under(b, MemoryProfile::Auto);
        }
        let mut sink = OverviewSink::Store(Arc::clone(&store));
        for c in chunks {
            sink.write_all(c).unwrap();
            sink.flush().unwrap();
        }
        store
    }

    fn data() -> (Vec<Vec<u8>>, Vec<u8>) {
        let chunks: Vec<Vec<u8>> = (0..7u8)
            .map(|i| {
                (0..(1000 + i as usize * 37))
                    .map(|j| (j as u8) ^ i)
                    .collect()
            })
            .collect();
        let all = chunks.concat();
        (chunks, all)
    }

    fn check_reads(store: &OverviewStore, all: &[u8]) {
        for (start, len) in [
            (0, all.len()),
            (0, 1),
            (999, 2),
            (1500, 3000),
            (all.len() - 5, 5),
        ] {
            let got = store.read(start as u64, len).unwrap();
            assert_eq!(&got[..], &all[start..start + len], "{start}+{len}");
        }
        assert!(store.read(all.len() as u64 - 1, 2).is_err());
    }

    #[test]
    fn round_trips_resident_evicted_and_mixed() {
        let (chunks, all) = data();
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        // All resident.
        let store = filled(&refs, None);
        assert_eq!(store.resident_bytes(), all.len() as u64);
        assert_eq!(store.disk_bytes(), 0);
        check_reads(&store, &all);
        // All evicted: the budget grants nothing.
        let store = filled(&refs, Some(MemoryBudget::with_available(Some(0))));
        assert_eq!(store.resident_bytes(), 0);
        assert_eq!(store.disk_bytes(), all.len() as u64);
        check_reads(&store, &all);
        // Mixed: the budget holds the first chunks, the rest go to disk.
        let store = filled(&refs, Some(MemoryBudget::with_available(Some(5000))));
        let resident = store.resident_bytes();
        assert!(resident > 0 && resident < all.len() as u64, "{resident}");
        assert_eq!(resident + store.disk_bytes(), all.len() as u64);
        check_reads(&store, &all);
        // Evicting after the fact takes the last chunks first.
        let store = filled(&refs, None);
        assert_eq!(store.evict(1).unwrap(), chunks[6].len() as u64);
        assert_eq!(
            lock(&store.inner)
                .chunks
                .iter()
                .filter(|c| matches!(c.state, State::Disk))
                .count(),
            1
        );
        assert!(matches!(lock(&store.inner).chunks[6].state, State::Disk));
        check_reads(&store, &all);
    }

    #[test]
    fn concurrent_reads_agree_while_chunks_are_evicted() {
        let (chunks, all) = data();
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        let store = filled(&refs, None);
        std::thread::scope(|s| {
            for t in 0..8 {
                let (store, all) = (&store, &all);
                s.spawn(move || {
                    for i in 0..400usize {
                        let start = (i * 131 + t * 17) % (all.len() - 300);
                        let len = 1 + (i * 7) % 299;
                        assert_eq!(
                            &store.read(start as u64, len).unwrap()[..],
                            &all[start..start + len]
                        );
                    }
                });
            }
            s.spawn(|| {
                while store.resident_bytes() > 0 {
                    store.evict(500).unwrap();
                }
            });
        });
        assert_eq!(store.resident_bytes(), 0);
        check_reads(&store, &all);
    }

    #[test]
    fn freed_chunks_return_their_bytes_and_refuse_reads() {
        let (chunks, all) = data();
        let refs: Vec<&[u8]> = chunks.iter().map(Vec::as_slice).collect();
        let store = filled(&refs, None);
        let two = (chunks[0].len() + chunks[1].len()) as u64;
        store.free_below(two + 1);
        assert_eq!(store.resident_bytes(), all.len() as u64 - two);
        assert!(store.read(0, 1).is_err());
        assert_eq!(
            &store.read(two, 10).unwrap()[..],
            &all[two as usize..two as usize + 10]
        );
    }
}
