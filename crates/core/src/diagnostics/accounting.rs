//! Dense row addresses keep accounting on spill, independent of caller IDs.
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};

use super::{AccountingError, SourceLossCause};
use std::sync::{Arc, Mutex};

pub(super) type RowsMap = BTreeMap<Option<u8>, Rows>;

pub(super) const RECORD_BYTES: u64 = 10;
const RESIDENT_BYTES: usize = 16 << 10;
const PAGE_BYTES: usize = 1024 * RECORD_BYTES as usize;

/// One zoom's ledger file.
#[derive(Debug)]
pub(super) enum LedgerFile {
    /// Resident until it outgrows `RESIDENT_BYTES`, then an open temp file.
    Open(tempfile::SpooledTempFile),
    /// A finished ledger on disk, holding no file descriptor between reads
    /// ([`super::SourceLosses::seal`]).
    Sealed(tempfile::TempPath),
}

impl LedgerFile {
    fn new(dir: Option<&std::path::Path>) -> Self {
        Self::Open(dir.map_or_else(
            || tempfile::spooled_tempfile(RESIDENT_BYTES),
            |dir| tempfile::spooled_tempfile_in(RESIDENT_BYTES, dir),
        ))
    }

    /// The file to write, reopening a sealed one (recording resumed).
    fn open(&mut self) -> std::io::Result<&mut tempfile::SpooledTempFile> {
        if let Self::Sealed(path) = self {
            let mut file = tempfile::spooled_tempfile(RESIDENT_BYTES);
            std::io::copy(&mut std::fs::File::open(&*path)?, &mut file)?;
            *self = Self::Open(file);
        }
        match self {
            Self::Open(file) => Ok(file),
            Self::Sealed(_) => unreachable!("reopened above"),
        }
    }

    /// Fill `buf` from `offset`.
    pub(super) fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        match self {
            Self::Open(file) => {
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(buf)
            }
            Self::Sealed(path) => {
                let mut file = std::fs::File::open(&*path)?;
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(buf)
            }
        }
    }

    fn copy_to(&mut self, out: &mut impl Write) -> std::io::Result<u64> {
        match self {
            Self::Open(file) => {
                file.rewind()?;
                std::io::copy(file, out)
            }
            Self::Sealed(path) => std::io::copy(&mut std::fs::File::open(&*path)?, out),
        }
    }
}

#[derive(Debug)]
pub(super) struct Rows {
    pub(super) file: LedgerFile,
    pub(super) len: u64,
    losses: u64,
    allocated: u64,
    page: Box<[u8; PAGE_BYTES]>,
    page_start: Option<u64>,
    dirty: bool,
}

impl Rows {
    fn flush(&mut self) -> std::io::Result<()> {
        if self.dirty {
            let file = self.file.open()?;
            file.seek(SeekFrom::Start(self.page_start.unwrap()))?;
            file.write_all(self.page.as_slice())?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Move a ledger that went to disk to a named file closed between reads.
    pub(super) fn seal(&mut self, dir: Option<&std::path::Path>) -> std::io::Result<()> {
        self.flush()?;
        if matches!(&self.file, LedgerFile::Open(file) if file.is_rolled()) {
            let mut sealed = dir.map_or_else(
                tempfile::NamedTempFile::new,
                tempfile::NamedTempFile::new_in,
            )?;
            self.file.copy_to(&mut sealed)?;
            sealed.as_file().sync_data()?;
            self.file = LedgerFile::Sealed(sealed.into_temp_path());
        }
        Ok(())
    }
    fn record(&mut self, offset: u64) -> std::io::Result<&mut [u8]> {
        let start = offset / PAGE_BYTES as u64 * PAGE_BYTES as u64;
        if self.page_start != Some(start) {
            self.flush()?;
            self.page.fill(0);
            if start < self.allocated {
                self.file.read_at(start, self.page.as_mut_slice())?;
            } else {
                // Sparse growth stays on spill, including distant row addresses.
                let allocated = start.checked_add(PAGE_BYTES as u64).ok_or_else(|| {
                    std::io::Error::other("source accounting page address overflow")
                })?;
                self.file.open()?.set_len(allocated)?;
                self.allocated = allocated;
            }
            self.page_start = Some(start);
        }
        self.dirty = true;
        let at = (offset - start) as usize;
        Ok(&mut self.page[at..at + RECORD_BYTES as usize])
    }
}

#[derive(Debug)]
pub(super) struct Accounting {
    rows: Arc<Mutex<RowsMap>>,
    report_budget: usize,
    spill_dir: Option<std::path::PathBuf>,
}

impl Default for Accounting {
    fn default() -> Self {
        Self {
            rows: Arc::default(),
            report_budget: super::MAX_PYTHON_LOSS_BYTES,
            spill_dir: None,
        }
    }
}

impl Accounting {
    pub(super) fn set_spill_dir(&mut self, path: Option<&std::path::Path>) {
        self.spill_dir = path.map(std::path::Path::to_owned);
    }

    pub(super) fn set_budget(&mut self, bytes: usize) {
        self.report_budget = bytes;
    }

    pub(super) fn record(
        &mut self,
        row: u64,
        feature_id: Option<u64>,
        zoom: Option<u8>,
        cause: Option<SourceLossCause>,
    ) -> Result<(), AccountingError> {
        let offset = row
            .checked_mul(RECORD_BYTES)
            .and_then(|n| n.checked_add(RECORD_BYTES))
            .ok_or_else(|| std::io::Error::other("source accounting row address overflow"))?;
        // Reports own immutable snapshots. Fork only if recording resumes after
        // a snapshot has escaped; the normal final report shares the ledger.
        if Arc::strong_count(&self.rows) != 1 {
            let mut copy = BTreeMap::new();
            for (&zoom, rows) in self.rows.lock().unwrap().iter_mut() {
                let mut file = LedgerFile::new(self.spill_dir.as_deref());
                rows.flush()?;
                rows.file.copy_to(file.open()?)?;
                copy.insert(
                    zoom,
                    Rows {
                        file,
                        len: rows.len,
                        allocated: rows.allocated,
                        losses: rows.losses,
                        page: Box::new([0; PAGE_BYTES]),
                        page_start: None,
                        dirty: false,
                    },
                );
            }
            self.rows = Arc::new(Mutex::new(copy));
        }
        let mut ledger = self.rows.lock().unwrap();
        let rows = ledger.entry(zoom).or_insert_with(|| Rows {
            file: LedgerFile::new(self.spill_dir.as_deref()),
            len: 0,
            losses: 0,
            allocated: 0,
            page: Box::new([0; PAGE_BYTES]),
            page_start: None,
            dirty: false,
        });
        let record = rows.record(offset - RECORD_BYTES)?;
        let was_lost = record[0] & 3 == 1;
        // Surviving anywhere wins, regardless of the arrival order of tile
        // tasks. Duplicate MVT IDs are separate rows and never cancel.
        if record[0] & 2 == 0 {
            if let Some(cause) = cause {
                let reason = cause as u8 + 1;
                record[1] = if record[1] == 0 {
                    reason
                } else {
                    record[1].min(reason)
                };
                record[0] |= 1;
            } else {
                record[0] |= 2;
            }
        }
        if let Some(id) = feature_id {
            record[0] |= 4;
            record[2..].copy_from_slice(&id.to_le_bytes());
        }
        let is_lost = record[0] & 3 == 1;
        rows.len = rows.len.max(row + 1);
        if was_lost != is_lost {
            if is_lost {
                rows.losses += 1;
            } else {
                rows.losses -= 1;
            }
        }
        Ok(())
    }

    pub(super) fn losses(&mut self) -> Result<super::SourceLosses, AccountingError> {
        let count = {
            let mut rows = self.rows.lock().unwrap();
            for rows in rows.values_mut() {
                rows.flush()?;
            }
            rows.values().map(|r| r.losses).sum()
        };
        Ok(super::SourceLosses {
            rows: self.rows.clone(),
            count,
            budget: self.report_budget,
            spill_dir: self.spill_dir.clone(),
            input_rows: None,
            python_file: None,
        })
    }

    #[cfg(test)]
    pub(super) fn resident_bytes(&self) -> usize {
        self.rows
            .lock()
            .unwrap()
            .values()
            .map(|r| {
                PAGE_BYTES
                    + match &r.file {
                        LedgerFile::Open(file) if !file.is_rolled() => RESIDENT_BYTES,
                        _ => 0,
                    }
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #821: sealing a snapshot keeps its losses and closes its ledgers'
    /// descriptors, and recording can resume after it.
    #[test]
    fn sealed_snapshots_read_back_and_hold_no_descriptor() {
        let mut ledger = Accounting::default();
        // 4,000 rows outgrow RESIDENT_BYTES, so each zoom's ledger is on disk.
        for zoom in 0..4 {
            for row in 0..4000 {
                let cause = (row % 3 == 0).then_some(SourceLossCause::ValveDropped);
                ledger.record(row, None, Some(zoom), cause).unwrap();
            }
        }
        let snapshot = ledger.losses().unwrap();
        let ids = |s: &super::super::SourceLosses| -> Vec<(Option<u8>, u64)> {
            s.iter()
                .map(|l| l.map(|l| (l.zoom, l.id)))
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let before = ids(&snapshot);
        #[cfg(target_os = "linux")]
        let fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
        #[cfg(target_os = "linux")]
        let open = fds();
        snapshot.seal().unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(fds() + 4, open, "one descriptor per zoom closed");
        assert_eq!(ids(&snapshot), before);

        ledger
            .record(5000, None, Some(0), Some(SourceLossCause::ValveDropped))
            .unwrap();
        assert_eq!(ledger.losses().unwrap().len(), before.len() + 1);
        assert_eq!(ids(&snapshot), before, "the sealed snapshot is unchanged");
    }

    #[test]
    fn spilled_pages_preserve_sparse_rows_causes_ids_and_survival() {
        let mut ledger = Accounting::default();
        for row in [0, 1023, 1024, 10_000, 1] {
            ledger
                .record(row, Some(7), Some(4), Some(SourceLossCause::ValveDropped))
                .unwrap();
        }
        ledger.record(1023, None, Some(4), None).unwrap();
        ledger
            .record(1023, None, Some(4), Some(SourceLossCause::Quantized))
            .unwrap();
        ledger
            .record(0, None, Some(4), Some(SourceLossCause::Quantized))
            .unwrap();
        let snapshot = ledger.losses().unwrap();
        let losses: Vec<_> = snapshot.iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(
            losses.iter().map(|l| l.id).collect::<Vec<_>>(),
            [0, 1, 1024, 10_000]
        );
        assert_eq!(snapshot.len(), losses.len());
        assert_eq!(losses[0].cause, SourceLossCause::Quantized);
        assert!(losses.iter().all(|l| l.feature_id == Some(7)));
        ledger.record(10_000, None, Some(4), None).unwrap();
        assert_eq!(ledger.losses().unwrap().len(), 3);
        assert_eq!(snapshot.iter().count(), 4);
    }
}
