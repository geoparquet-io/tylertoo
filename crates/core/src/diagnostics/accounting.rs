//! Dense row addresses keep accounting on spill, independent of caller IDs.
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};

use super::{AccountingError, SourceLossCause};
use std::sync::{Arc, Mutex};

pub(super) type RowsMap = BTreeMap<Option<u8>, Rows>;

pub(super) const RECORD_BYTES: u64 = 10;
const RESIDENT_BYTES: usize = 16 << 10;
const PAGE_BYTES: usize = 1024 * RECORD_BYTES as usize;

#[derive(Debug)]
pub(super) struct Rows {
    pub(super) file: tempfile::SpooledTempFile,
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
            self.file.seek(SeekFrom::Start(self.page_start.unwrap()))?;
            self.file.write_all(self.page.as_slice())?;
            self.dirty = false;
        }
        Ok(())
    }
    fn record(&mut self, offset: u64) -> std::io::Result<&mut [u8]> {
        let start = offset / PAGE_BYTES as u64 * PAGE_BYTES as u64;
        if self.page_start != Some(start) {
            self.flush()?;
            self.page.fill(0);
            if start < self.allocated {
                self.file.seek(SeekFrom::Start(start))?;
                self.file.read_exact(self.page.as_mut_slice())?;
            } else {
                // Sparse growth stays on spill, including distant row addresses.
                let allocated = start.checked_add(PAGE_BYTES as u64).ok_or_else(|| {
                    std::io::Error::other("source accounting page address overflow")
                })?;
                self.file.set_len(allocated)?;
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
                let mut file = self.spill_dir.as_ref().map_or_else(
                    || tempfile::spooled_tempfile(RESIDENT_BYTES),
                    |dir| tempfile::spooled_tempfile_in(RESIDENT_BYTES, dir),
                );
                rows.flush()?;
                rows.file.rewind()?;
                std::io::copy(&mut rows.file, &mut file)?;
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
            file: self.spill_dir.as_ref().map_or_else(
                || tempfile::spooled_tempfile(RESIDENT_BYTES),
                |dir| tempfile::spooled_tempfile_in(RESIDENT_BYTES, dir),
            ),
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
                    + if r.file.is_rolled() {
                        0
                    } else {
                        RESIDENT_BYTES
                    }
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
