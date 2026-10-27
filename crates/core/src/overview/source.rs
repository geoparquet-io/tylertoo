//! Batch sources for the shared conversion stages.

use std::time::{Duration, Instant};

use super::convert::{check_in_memory_byte_array_total, ConvertError};
use super::pipeline::{read_in_order, ReadFlow, ReadTuning};
use crate::input_set::{ConvertSource, RowGroupSelection};
use arrow_array::RecordBatch;

/// Storage is the only difference between the conversion engines. Callbacks
/// see the same row order, projections and batch boundaries from either source.
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
}

impl MemorySource {
    #[cfg(test)]
    pub(super) fn num_rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }
    pub(super) fn load(
        source: &ConvertSource,
        row_groups: Option<&RowGroupSelection>,
        tuning: ReadTuning,
        limit: u64,
    ) -> Result<Self, ConvertError> {
        let ConvertSource::Single(single) = source else {
            return Err(ConvertError::MultiPartitionRequiresStreaming);
        };
        check_in_memory_byte_array_total(
            source,
            &single.input().open()?,
            row_groups.map(|s| s.parts()[0].as_slice()),
            limit,
        )?;
        let mut batches = Vec::new();
        source.read(row_groups, None, tuning, &mut |batch, _, _| {
            batches.push(batch);
            Ok(ReadFlow::Continue)
        })?;
        Ok(Self { batches })
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
