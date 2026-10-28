//! Immutable, bounded views of the exact source-loss ledger.
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{ser::SerializeSeq, Deserialize, Serialize};

use super::{accounting, AccountingError, SourceLoss, SourceLossCause};

/// Exact source losses backed by spill, without a resident vector of all rows.
///
/// Clones share an immutable snapshot. Iteration and serialization read bounded
/// chunks and report spill I/O errors to the caller.
#[derive(Debug, Clone)]
pub struct SourceLosses {
    pub(super) rows: Arc<Mutex<accounting::RowsMap>>,
    pub(super) count: u64,
    pub(super) budget: usize,
    pub(super) spill_dir: Option<PathBuf>,
    pub(super) input_rows: Option<crate::input_set::StreamRowLocator>,
    pub(super) python_path: Option<PathBuf>,
}

impl Default for SourceLosses {
    fn default() -> Self {
        accounting::Accounting::default().losses().unwrap()
    }
}

impl SourceLosses {
    /// Number of disjoint losses; does not read the spill.
    pub fn len(&self) -> usize {
        self.count as usize
    }
    /// Whether the report contains no losses.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
    /// Read the exact rows in zoom and source-row order using bounded memory.
    pub fn iter(&self) -> impl Iterator<Item = Result<SourceLoss, AccountingError>> + '_ {
        let zooms: Vec<_> = self
            .rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(&z, r)| (z, r.len))
            .collect();
        let mut page = [0u8; 10_240];
        let mut zoom_index = 0;
        let mut row = 0u64;
        let mut page_start = u64::MAX;
        let mut failed = false;
        std::iter::from_fn(move || {
            if failed {
                return None;
            }
            loop {
                let &(zoom, len) = zooms.get(zoom_index)?;
                if row == len {
                    zoom_index += 1;
                    row = 0;
                    page_start = u64::MAX;
                    continue;
                }
                if page_start == u64::MAX || row >= page_start + 1024 {
                    page_start = row;
                    let n = (len - row).min(1024) as usize * accounting::RECORD_BYTES as usize;
                    let result = (|| {
                        let mut ledger = self.rows.lock().unwrap_or_else(|e| e.into_inner());
                        let rows = ledger.get_mut(&zoom).ok_or_else(|| {
                            std::io::Error::other("source loss snapshot lost its zoom")
                        })?;
                        rows.file
                            .seek(SeekFrom::Start(row * accounting::RECORD_BYTES))?;
                        rows.file.read_exact(&mut page[..n])
                    })();
                    if let Err(e) = result {
                        failed = true;
                        return Some(Err(e.into()));
                    }
                }
                let at = (row - page_start) as usize * accounting::RECORD_BYTES as usize;
                let record = &page[at..at + accounting::RECORD_BYTES as usize];
                let mut id = row;
                row += 1;
                if record[0] & 3 != 1 {
                    continue;
                }
                if zoom.is_none() {
                    if let Some(locator) = &self.input_rows {
                        let Some(global) = locator.global_row(id as usize) else {
                            failed = true;
                            return Some(Err(std::io::Error::other(
                                "source loss is outside the selected input rows",
                            )
                            .into()));
                        };
                        id = global;
                    }
                }
                let cause = SourceLossCause::deserialize(serde::de::value::U32Deserializer::<
                    serde::de::value::Error,
                >::new(u32::from(
                    record[1] - 1,
                )));
                return Some(
                    cause
                        .map(|cause| SourceLoss {
                            id,
                            zoom,
                            cause,
                            feature_id: (record[0] & 4 != 0).then(|| {
                                let mut bytes = [0u8; 8];
                                bytes.copy_from_slice(&record[2..]);
                                u64::from_le_bytes(bytes)
                            }),
                        })
                        .map_err(|e| std::io::Error::other(e).into()),
                );
            }
        })
    }
    pub(crate) fn try_from_iter(
        losses: impl IntoIterator<Item = Result<SourceLoss, AccountingError>>,
    ) -> Result<Self, AccountingError> {
        Self::try_from_iter_in(losses, None)
    }
    pub(crate) fn try_from_iter_in(
        losses: impl IntoIterator<Item = Result<SourceLoss, AccountingError>>,
        spill_dir: Option<&std::path::Path>,
    ) -> Result<Self, AccountingError> {
        let mut ledger = accounting::Accounting::default();
        ledger.set_spill_dir(spill_dir);
        for loss in losses {
            let loss = loss?;
            ledger.record(loss.id, loss.feature_id, loss.zoom, Some(loss.cause))?;
        }
        ledger.losses()
    }
    pub(crate) fn write_json_lines(&self, writer: &mut impl Write) -> Result<(), AccountingError> {
        for loss in self.iter() {
            serde_json::to_writer(&mut *writer, &loss?).map_err(std::io::Error::other)?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    }
    /// Prepare a bounded Python dict. Large lists become `{count, path}`;
    /// `path` names an exact JSON Lines file owned by the caller, who removes it.
    ///
    /// # Errors
    /// Returns an error if the spill cannot be read, written, or persisted.
    pub fn for_python(&self) -> Result<Self, AccountingError> {
        let mut out = self.clone();
        // Conservative allowance for Python dicts, strings, ints and list slots.
        if self.len() > self.budget / 1024 && out.python_path.is_none() {
            let mut file = self.spill_dir.as_ref().map_or_else(
                tempfile::NamedTempFile::new,
                tempfile::NamedTempFile::new_in,
            )?;
            {
                let mut writer = std::io::BufWriter::new(file.as_file_mut());
                self.write_json_lines(&mut writer)?;
                writer.flush()?;
            }
            let (_, path) = file.keep().map_err(|e| e.error)?;
            out.python_path = Some(path);
        }
        Ok(out)
    }
}

impl Serialize for SourceLosses {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::Error;
        if let Some(path) = &self.python_path {
            #[derive(Serialize)]
            struct Spilled<'a> {
                count: u64,
                path: &'a std::path::Path,
            }
            return Spilled {
                count: self.count,
                path,
            }
            .serialize(serializer);
        }
        let mut seq = serializer.serialize_seq(Some(self.len()))?;
        for loss in self.iter() {
            seq.serialize_element(&loss.map_err(S::Error::custom)?)?;
        }
        seq.end()
    }
}
impl<'de> Deserialize<'de> for SourceLosses {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = SourceLosses;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of source losses")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                use serde::de::Error;
                let mut ledger = accounting::Accounting::default();
                while let Some(loss) = seq.next_element::<SourceLoss>()? {
                    ledger
                        .record(loss.id, loss.feature_id, loss.zoom, Some(loss.cause))
                        .map_err(A::Error::custom)?;
                }
                ledger.losses().map_err(A::Error::custom)
            }
        }
        deserializer.deserialize_seq(Visitor)
    }
}
impl PartialEq for SourceLosses {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
            && self
                .iter()
                .zip(other.iter())
                .all(|(a, b)| matches!((a,b), (Ok(a), Ok(b)) if a == b))
    }
}
impl Eq for SourceLosses {}
