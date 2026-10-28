//! Dense row addresses keep accounting on spill, independent of caller IDs.
use std::collections::BTreeMap;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};

use super::{AccountingError, SourceLoss, SourceLossCause};
use serde::Deserialize;

const RECORD_BYTES: u64 = 10;
const RESIDENT_BYTES: usize = 16 << 10;

#[derive(Debug)]
struct Rows {
    file: tempfile::SpooledTempFile,
    len: u64,
    allocated: u64,
}

#[derive(Debug)]
pub(super) struct Accounting {
    rows: BTreeMap<Option<u8>, Rows>,
    report_budget: usize,
    spill_dir: Option<std::path::PathBuf>,
}

impl Default for Accounting {
    fn default() -> Self {
        Self {
            rows: BTreeMap::new(),
            report_budget: super::MAX_LOSS_REPORT_BYTES,
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
        let rows = self.rows.entry(zoom).or_insert_with(|| Rows {
            file: self.spill_dir.as_ref().map_or_else(
                || tempfile::spooled_tempfile(RESIDENT_BYTES),
                |dir| tempfile::spooled_tempfile_in(RESIDENT_BYTES, dir),
            ),
            len: 0,
            allocated: 0,
        });
        let mut record = [0u8; RECORD_BYTES as usize];
        if row < rows.len {
            rows.file.seek(SeekFrom::Start(offset - RECORD_BYTES))?;
            rows.file.read_exact(&mut record)?;
        }
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
        // Growing a sparse file does not allocate missing rows in memory.
        if offset > rows.allocated {
            let allocated = offset
                .checked_next_multiple_of(RESIDENT_BYTES as u64)
                .unwrap_or(offset);
            rows.file.set_len(allocated)?;
            rows.allocated = allocated;
        }
        rows.len = rows.len.max(row + 1);
        rows.file.seek(SeekFrom::Start(offset - RECORD_BYTES))?;
        rows.file.write_all(&record)?;
        Ok(())
    }

    pub(super) fn losses(&mut self) -> Result<Vec<SourceLoss>, AccountingError> {
        let mut losses = Vec::new();
        for (&zoom, rows) in &mut self.rows {
            rows.file.rewind()?;
            let mut reader = BufReader::new(&mut rows.file);
            for id in 0..rows.len {
                let mut record = [0u8; RECORD_BYTES as usize];
                reader.read_exact(&mut record)?;
                if record[0] & 3 != 1 {
                    continue;
                }
                let needed = (losses.len() + 1).saturating_mul(std::mem::size_of::<SourceLoss>());
                if needed > self.report_budget {
                    return Err(AccountingError::BudgetExceeded {
                        needed,
                        available: self.report_budget,
                    });
                }
                if losses.len() == losses.capacity() {
                    let max_entries = self.report_budget / std::mem::size_of::<SourceLoss>();
                    let capacity = (losses.len() + 1).next_power_of_two().min(max_entries);
                    losses
                        .try_reserve_exact(capacity - losses.len())
                        .map_err(|_| AccountingError::BudgetExceeded {
                            needed: capacity.saturating_mul(std::mem::size_of::<SourceLoss>()),
                            available: self.report_budget,
                        })?;
                }
                losses.push(SourceLoss {
                    id,
                    zoom,
                    feature_id: (record[0] & 4 != 0)
                        .then(|| u64::from_le_bytes(record[2..].try_into().unwrap())),
                    cause: SourceLossCause::deserialize(serde::de::value::U32Deserializer::<
                        serde::de::value::Error,
                    >::new(u32::from(
                        record[1] - 1,
                    )))
                    .map_err(std::io::Error::other)?,
                });
            }
        }
        Ok(losses)
    }

    #[cfg(test)]
    pub(super) fn resident_bytes(&self) -> usize {
        self.rows.values().filter(|r| !r.file.is_rolled()).count() * RESIDENT_BYTES
    }
}
