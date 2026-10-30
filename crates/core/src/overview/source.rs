//! Batch sources for the shared conversion stages.

use std::time::{Duration, Instant};

use super::convert::{check_in_memory_byte_array_total, ConvertError};
use super::memory::MemoryPhase;
use super::read::{read_in_order, ReadFlow, ReadTuning};
use crate::input_set::{ConvertSource, RowGroupSelection};
use arrow_array::RecordBatch;

/// Storage is the only difference between the conversion engines. Callbacks
/// see the same row order and projections from either source.
pub(super) trait BatchSource: Sync {
    fn read(
        &self,
        row_groups: Option<&RowGroupSelection>,
        projection: Option<&[usize]>,
        tuning: ReadTuning,
        on_batch: &mut dyn FnMut(RecordBatch, usize, Duration) -> Result<ReadFlow, ConvertError>,
    ) -> Result<(), ConvertError>;
}

impl BatchSource for ConvertSource {
    fn read(
        &self,
        row_groups: Option<&RowGroupSelection>,
        projection: Option<&[usize]>,
        tuning: ReadTuning,
        on_batch: &mut dyn FnMut(RecordBatch, usize, Duration) -> Result<ReadFlow, ConvertError>,
    ) -> Result<(), ConvertError> {
        read_in_order(self, row_groups, projection, tuning, on_batch)
    }
}

/// An eager source adapter. It retains raw rows, including unusable geometries,
/// so pass 1 and pass 2 share exactly the streaming source's row addresses.
pub(super) struct MemorySource {
    batches: Vec<RecordBatch>,
    resident_bytes: u64,
}

impl MemorySource {
    pub(super) fn resident_bytes(&self) -> u64 {
        self.resident_bytes
    }
    #[cfg(test)]
    pub(super) fn num_rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }
    pub(super) fn load(
        source: &ConvertSource,
        row_groups: Option<&RowGroupSelection>,
        mut tuning: ReadTuning,
        limit: u64,
    ) -> Result<Self, ConvertError> {
        let ConvertSource::Single(single) = source else {
            return Err(ConvertError::MultiPartitionRequiresStreaming);
        };
        let input = single.input().open()?;
        let groups = row_groups.map(|s| s.parts()[0].as_slice());
        check_in_memory_byte_array_total(source, &input, groups, limit)?;
        let retained = tylertoo_io::decoded_input_bytes(input.metadata(), groups, |root| {
            source
                .column_projection()
                .is_none_or(|columns| columns.contains(&root))
        });
        let rows = source.selected_row_count(row_groups)?.max(0) as u64;
        let scan = tuning.budget.estimate(MemoryPhase::Scan, rows, None);
        // Leave space for the next bounded decode/cast as well as pass 1.
        let transient = retained
            .min(tylertoo_io::decoded_batch_limit())
            .saturating_mul(2);
        tuning.budget.require(
            MemoryPhase::EagerInput,
            retained.saturating_add(scan).saturating_add(transient),
        )?;
        // Eager input has no consumer to drain parallel read-ahead queues.
        tuning.workers = 1;
        let mut batches = Vec::new();
        let mut resident_bytes = 0u64;
        source.read(row_groups, None, tuning, &mut |batch, _, _| {
            resident_bytes = resident_bytes.saturating_add(batch.get_array_memory_size() as u64);
            tuning.budget.require(
                MemoryPhase::EagerInput,
                resident_bytes
                    .saturating_add(scan)
                    .saturating_add(transient),
            )?;
            batches.push(batch);
            Ok(ReadFlow::Continue)
        })?;
        Ok(Self {
            batches,
            resident_bytes,
        })
    }
}

impl BatchSource for MemorySource {
    fn read(
        &self,
        _row_groups: Option<&RowGroupSelection>,
        projection: Option<&[usize]>,
        _tuning: ReadTuning,
        on_batch: &mut dyn FnMut(RecordBatch, usize, Duration) -> Result<ReadFlow, ConvertError>,
    ) -> Result<(), ConvertError> {
        let mut offset = 0;
        for batch in &self.batches {
            let start = Instant::now();
            let projected = match projection {
                Some(columns) => batch.project(columns)?,
                None => batch.clone(),
            };
            if matches!(
                on_batch(projected, offset, start.elapsed())?,
                ReadFlow::Stop
            ) {
                break;
            }
            offset += batch.num_rows();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{level::MemoryProfile, memory::MemoryBudget};
    use super::*;
    use arrow_array::{ArrayRef, Float64Array};
    use std::sync::Arc;
    use tylertoo_io::test_support::ArrowWriter;

    #[test]
    fn eager_numeric_properties_are_refused_before_loading_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.parquet");
        let batch = RecordBatch::try_from_iter((0..100).map(|i| {
            (
                format!("property_{i}"),
                Arc::new(Float64Array::from(vec![0.; 100])) as ArrayRef,
            )
        }))
        .unwrap();
        let mut writer =
            ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), batch.schema(), None)
                .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let source = ConvertSource::resolve_path(&path).unwrap();
        // Poison a data page after caching its footer. A check made only
        // after allocation/decoding would return a decode error, not the
        // eager-input budget error asserted below.
        use std::io::{Read, Seek, SeekFrom, Write};
        let input = source.parts()[0].open().unwrap();
        let column = input.metadata().row_group(0).column(0);
        let offset = column
            .dictionary_page_offset()
            .unwrap_or(column.data_page_offset()) as u64;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        let mut header = [0u8; 32];
        file.read_exact(&mut header).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&[0; 32]).unwrap();
        let loaded = MemorySource::load(
            &source,
            None,
            ReadTuning {
                budget: MemoryBudget::with_available(Some(32_000)),
                batch_size: 10,
                workers: 1,
                avg_geom_bytes: None,
                profile: MemoryProfile::Bounded,
            },
            u64::MAX,
        );
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&header).unwrap();
        assert!(
            matches!(
                loaded,
                Err(ConvertError::BudgetExceeded {
                    phase: "eager input",
                    ..
                })
            ),
            "eager input must be charged before decoding numeric properties"
        );
        let mut budget = MemoryBudget::with_available(Some(2_000_000));
        let loaded = MemorySource::load(
            &source,
            None,
            ReadTuning {
                budget,
                batch_size: 10,
                workers: 2,
                avg_geom_bytes: None,
                profile: MemoryProfile::Bounded,
            },
            u64::MAX,
        )
        .unwrap();
        assert_eq!(loaded.num_rows(), 100);
        assert!(loaded.resident_bytes() >= 100 * 100 * 8);
        budget
            .reserve(MemoryPhase::EagerInput, loaded.resident_bytes())
            .unwrap();
        assert_eq!(
            budget.available(),
            Some(2_000_000 - loaded.resident_bytes())
        );

        // The footer estimate must follow the source's property selection.
        source.restrict_columns(vec![0]).unwrap();
        let projected = MemorySource::load(
            &source,
            None,
            ReadTuning {
                budget: MemoryBudget::with_available(Some(32_000)),
                batch_size: 10,
                workers: 1,
                avg_geom_bytes: None,
                profile: MemoryProfile::Bounded,
            },
            u64::MAX,
        )
        .unwrap();
        assert_eq!(projected.num_rows(), 100);
        assert!(projected.resident_bytes() < loaded.resident_bytes());
    }
}
