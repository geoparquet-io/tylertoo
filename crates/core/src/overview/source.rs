//! Batch sources for the shared conversion stages.

use std::time::Duration;

use super::convert::ConvertError;
use super::read::{read_in_order, ReadFlow, ReadTuning};
use crate::input_set::{ConvertSource, RowGroupSelection};
use arrow_array::RecordBatch;

/// The stages read their input through this trait. Production reads
/// [`ConvertSource`] on demand; tests can swap in [`MemorySource`].
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

#[cfg(test)]
thread_local! {
    /// Test seam (#440): convert on this thread through [`MemorySource`].
    static EAGER_INPUT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `f` with this thread's converts reading through [`MemorySource`]
/// when `eager` is set: the test oracle that pins the stages' row order and
/// addresses to the source, not to how it stores batches (#440).
#[cfg(test)]
pub(crate) fn with_eager_input<R>(eager: bool, f: impl FnOnce() -> R) -> R {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            EAGER_INPUT.set(self.0);
        }
    }
    let _reset = Reset(EAGER_INPUT.replace(eager));
    f()
}

/// Whether [`with_eager_input`] is in force on this thread.
#[cfg(test)]
pub(super) fn eager_input() -> bool {
    EAGER_INPUT.get()
}

/// A test-only eager source (#440). It retains raw rows, including unusable
/// geometries, so pass 1 and pass 2 share exactly the streaming source's row
/// addresses.
#[cfg(test)]
pub(super) struct MemorySource {
    batches: Vec<RecordBatch>,
}

#[cfg(test)]
impl MemorySource {
    /// # Panics
    ///
    /// On multi-partition input, whose row addresses are per part.
    pub(super) fn load(
        source: &ConvertSource,
        row_groups: Option<&RowGroupSelection>,
        mut tuning: ReadTuning,
    ) -> Result<Self, ConvertError> {
        assert!(
            matches!(source, ConvertSource::Single(_)),
            "MemorySource reads single-partition input only"
        );
        // Eager input has no consumer to drain parallel read-ahead queues.
        tuning.workers = 1;
        let mut batches = Vec::new();
        source.read(row_groups, None, tuning, &mut |batch, _, _| {
            batches.push(batch);
            Ok(ReadFlow::Continue)
        })?;
        Ok(Self { batches })
    }
}

#[cfg(test)]
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
            let start = std::time::Instant::now();
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
