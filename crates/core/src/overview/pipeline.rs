//! Single-read, pipelined pass-2 engine for the streaming converter
//! (#213 / #212).
//!
//! The original pass 2 ([`super::stream`]) re-opened and re-read the entire
//! input **once per emitted level** (15 full-file reads on a 15-level plan) and
//! parallelized only a per-batch `par_iter` within one level at a time —
//! starving to ~2 of 16 threads because reads between batches were serial and a
//! single giant geometry was a per-batch long pole.
//!
//! This engine reads the input **once** and fans each batch to **all** buffered
//! levels at once:
//!
//! - a dedicated reader thread streams the input in order over a bounded
//!   channel (depth = `in_flight`, the read/compute-overlap and backpressure
//!   knob), tagging each batch with its cumulative `row_offset`;
//! - the consumer processes one batch at a time (batches stay in read order, so
//!   no reorder buffer is needed), but parallelizes across levels — every
//!   `(level × feature)` simplification in the batch is a rayon task, so the
//!   giant-geometry long pole at one level overlaps the other levels' work;
//! - each level's finished output batches accumulate in an ordered **sink**
//!   (RAM under the `speed` profile, spilled to a temporary Arrow IPC file under
//!   `bounded`), and after the read completes the sinks drain into the writer in
//!   level order (writers demand levels 0,1,2… contiguously).
//!
//! Output is **byte-identical** to the serial path: within a batch the ascending
//! `selected` order and `process_level_batch`'s order-preserving `par_iter` are
//! unchanged, and batches reach every sink in ascending read order, so each
//! level's row sequence — and therefore its row-group boundaries — match exactly.
//!
//! The finest/canonical level is **not** handled here: it is verbatim, the
//! largest level, and written last, so [`super::stream`] streams it directly
//! into the writer on a second read rather than buffering it
//! ("canonical-streamed-last").

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;
use arrow_schema::Schema;
use crossbeam_channel::bounded;
use rayon::prelude::*;
use tempfile::NamedTempFile;

use crate::input_set::{ConvertSource, ReadPlan, RowGroupSelection};

use super::convert::ConvertError;
use super::level::{MemoryProfile, Mode};
use super::stream::{process_batch_cascade, process_level_batch, LevelStreamCtx, Pass2Timers};
use super::writer::{LevelWriteOutcome, OverviewWriter};

/// How a level's buffered output is held until it is written.
#[derive(Clone, Copy, Debug)]
pub(super) enum SinkBacking {
    /// Buffer output batches in RAM (`speed` profile).
    Ram,
    /// Spill output batches to a temporary Arrow IPC file (`bounded` profile).
    Spill,
}

/// Resolve the [`MemoryProfile`] (including [`MemoryProfile::Auto`]) to a
/// concrete [`SinkBacking`], logging the decision.
///
/// `buffered_rows` is the total winner count of the levels the engine buffers
/// (all but the streamed finest level). `Auto` keeps duplicating mode in RAM
/// (it buffers only the small, geometrically-decayed coarse levels) and keeps
/// partitioning mode in RAM only while the buffered set is small — partitioning
/// buffers full-resolution geometry, so a large buffered set spills. An
/// explicit profile always wins.
pub(super) fn resolve_backing(
    profile: MemoryProfile,
    mode: Mode,
    buffered_rows: usize,
) -> SinkBacking {
    /// Buffered-row count above which `Auto` spills partitioning's
    /// full-resolution geometry instead of holding it in RAM.
    const PARTITIONING_SPILL_ROWS: usize = 2_000_000;

    let backing = match profile {
        MemoryProfile::Speed => SinkBacking::Ram,
        MemoryProfile::Bounded => SinkBacking::Spill,
        MemoryProfile::Auto => match mode {
            Mode::Duplicating => SinkBacking::Ram,
            Mode::Partitioning => {
                if buffered_rows > PARTITIONING_SPILL_ROWS {
                    SinkBacking::Spill
                } else {
                    SinkBacking::Ram
                }
            }
        },
    };
    log::debug!(
        "pass2 memory profile {profile:?} + {mode:?} (buffered ~{buffered_rows} rows) → {backing:?}"
    );
    backing
}

/// A message from the reader thread: one raw input batch, its cumulative row
/// offset, and how long the read+decode took (for [profile] accounting).
struct ReadMsg {
    row_offset: usize,
    batch: RecordBatch,
    read_dur: Duration,
}

/// One level's ordered output buffer.
enum LevelSink {
    Ram(Vec<RecordBatch>),
    Spill(SpillState),
}

impl LevelSink {
    fn new(backing: SinkBacking, out_schema: &Schema) -> Result<Self, ConvertError> {
        Ok(match backing {
            SinkBacking::Ram => LevelSink::Ram(Vec::new()),
            SinkBacking::Spill => LevelSink::Spill(SpillState::new(out_schema)?),
        })
    }

    fn push(&mut self, batch: RecordBatch) -> Result<(), ConvertError> {
        match self {
            LevelSink::Ram(v) => {
                v.push(batch);
                Ok(())
            }
            LevelSink::Spill(s) => s.push(&batch),
        }
    }
}

/// A level spilled to a temporary Arrow IPC stream file. The write handle and
/// the read handle are independent `reopen()`s of the same temp file; Arrow IPC
/// is a lossless value round-trip, so the reloaded batches are identical to the
/// buffered ones and the final Parquet encode stays byte-identical.
struct SpillState {
    writer: StreamWriter<BufWriter<File>>,
    temp: NamedTempFile,
}

impl SpillState {
    fn new(out_schema: &Schema) -> Result<Self, ConvertError> {
        let temp = NamedTempFile::new()?;
        let write_handle = temp.reopen()?;
        let writer = StreamWriter::try_new(BufWriter::new(write_handle), out_schema)?;
        Ok(SpillState { writer, temp })
    }

    fn push(&mut self, batch: &RecordBatch) -> Result<(), ConvertError> {
        self.writer.write(batch)?;
        Ok(())
    }

    /// Finish writing and reopen the temp file for reading. The returned
    /// [`NamedTempFile`] must be held until the reader is exhausted so the file
    /// is not unlinked mid-read (and is cleaned up on drop afterwards).
    fn into_reader(self) -> Result<(StreamReader<BufReader<File>>, NamedTempFile), ConvertError> {
        let SpillState { mut writer, temp } = self;
        writer.finish()?; // writes EOS + flushes the BufWriter to the file
        drop(writer); // close the write handle
        let read_handle = temp.reopen()?;
        let reader = StreamReader::try_new(BufReader::new(read_handle), None)?;
        Ok((reader, temp))
    }
}

/// Buffer + write levels `0..ctxs.len()` (all but the streamed finest level)
/// from a single read of the input. Returns `(outcome, rows_written,
/// vertex_count)` per level, in level order — the outcome flags a level the
/// writer skipped because every candidate collapsed during simplification
/// (#211).
#[allow(clippy::too_many_arguments)]
pub(super) fn run_pass2_buffered(
    writer: &mut OverviewWriter<File>,
    ctxs: &[LevelStreamCtx<'_>],
    hints: &[usize],
    source: &ConvertSource,
    read_batch_size: usize,
    selected_row_groups: Option<&RowGroupSelection>,
    in_flight: usize,
    backing: SinkBacking,
    out_schema: &Schema,
) -> Result<Vec<(LevelWriteOutcome, usize, usize)>, ConvertError> {
    let num_levels = ctxs.len();
    debug_assert_eq!(num_levels, hints.len());

    let t_engine = Instant::now();
    let timers = Pass2Timers::default();

    let mut sinks: Vec<LevelSink> = Vec::with_capacity(num_levels);
    for _ in 0..num_levels {
        sinks.push(LevelSink::new(backing, out_schema)?);
    }
    let mut rows = vec![0usize; num_levels];
    let mut verts = vec![0usize; num_levels];

    // Build the single-pass stream here (per-part bbox-selected row groups,
    // #102 — the same selection both passes use, so global row indices stay
    // aligned), then hand it to a dedicated reader thread. A synchronous
    // Parquet reader driven from a dedicated thread (not the rayon pool)
    // keeps reads from being head-of-line blocked behind compute work. The
    // stream borrows `source` (multi-partition sources open part i+1 lazily),
    // so the reader runs on a scoped thread.
    let mut reader = source.open_stream(&ReadPlan {
        batch_size: read_batch_size.max(1),
        projection: None,
        row_groups: selected_row_groups,
    })?;

    let (tx, rx) = bounded::<ReadMsg>(in_flight.max(1));
    // Consumer state borrowed mutably by the in-scope consumer below.
    let (rows_ref, verts_ref, sinks_ref) = (&mut rows, &mut verts, &mut sinks);
    let timers_ref = &timers;
    std::thread::scope(|scope| -> Result<(), ConvertError> {
        let reader_handle = scope.spawn(move || -> Result<(), ConvertError> {
            let mut row_offset = 0usize;
            loop {
                let t_read = Instant::now();
                match reader.next() {
                    None => break,
                    Some(Ok(batch)) => {
                        let read_dur = t_read.elapsed();
                        let offset = row_offset;
                        row_offset += batch.num_rows();
                        if tx
                            .send(ReadMsg {
                                row_offset: offset,
                                batch,
                                read_dur,
                            })
                            .is_err()
                        {
                            break; // consumer dropped the receiver (error path)
                        }
                    }
                    Some(Err(e)) => return Err(e.into()),
                }
            }
            Ok(())
        });

        // Consumer: process batches in read order; parallelize within each
        // batch. Because batches arrive in order and are appended before the
        // next is pulled, each sink stays in input order without a reorder
        // buffer.
        //
        // Cascading (#218, duplicating default): one call decodes the batch's
        // member geometries once and folds each feature fine→coarse, so level
        // k reuses level k+1's output instead of re-simplifying canonical
        // geometry. Otherwise (cascade off, or partitioning where every
        // feature lands on exactly one level) fan out per level as before.
        let cascade = ctxs.first().is_some_and(|c| c.is_cascading_duplicating());
        // Heartbeat (#242): a planet-scale pass 2 runs for minutes-to-hours;
        // without this the phase is silent at info level. Time-based so small
        // inputs stay quiet.
        let mut last_progress = Instant::now();
        let consume: Result<(), ConvertError> = (|| {
            for msg in rx.iter() {
                Pass2Timers::add_dur(timers_ref.read_cell(), msg.read_dur);
                let batch = &msg.batch;
                let row_offset = msg.row_offset;
                let per_level: Vec<Option<(RecordBatch, usize)>> = if cascade {
                    process_batch_cascade(batch, row_offset, ctxs, timers_ref)?
                } else {
                    let results: Vec<Result<Option<(RecordBatch, usize)>, ConvertError>> = (0
                        ..num_levels)
                        .into_par_iter()
                        .map(|li| process_level_batch(batch, row_offset, &ctxs[li], timers_ref))
                        .collect();
                    let mut v = Vec::with_capacity(num_levels);
                    for res in results {
                        v.push(res?);
                    }
                    v
                };
                for (li, out) in per_level.into_iter().enumerate() {
                    if let Some((out, v)) = out {
                        rows_ref[li] += out.num_rows();
                        verts_ref[li] += v;
                        sinks_ref[li].push(out)?;
                    }
                }
                if last_progress.elapsed().as_secs() >= 10 {
                    last_progress = Instant::now();
                    log::info!(
                        "[convert] pass 2: {} input row(s) processed ({} output \
                         row(s) buffered across {num_levels} level(s))",
                        row_offset + batch.num_rows(),
                        rows_ref.iter().sum::<usize>(),
                    );
                }
            }
            Ok(())
        })();
        drop(rx); // ensure the reader thread can stop if the consumer errored

        // Join the reader before propagating: a reader error is the more
        // likely root cause and takes precedence over a downstream consume
        // error.
        match reader_handle.join() {
            Ok(read_result) => read_result?,
            Err(payload) => std::panic::resume_unwind(payload),
        }
        consume
    })?;

    // Drain each level's sink into the writer, in level order. An empty
    // buffered level is skipped and renumbered by the writer (#211); the
    // outcome is threaded back to the caller with the level's stats.
    let mut outcomes = Vec::with_capacity(num_levels);
    for li in 0..num_levels {
        let sink = std::mem::replace(&mut sinks[li], LevelSink::Ram(Vec::new()));
        outcomes.push(drain_sink(writer, li, hints[li], sink)?);
    }

    timers.log_engine_summary(t_engine.elapsed().as_secs_f64(), rows.iter().sum());
    Ok(outcomes
        .into_iter()
        .zip(rows.into_iter().zip(verts))
        .map(|(outcome, (r, v))| (outcome, r, v))
        .collect())
}

/// Drain one level's sink into `writer.write_level`. The RAM path is
/// infallible; the spill path reuses the error-parking discipline of
/// `write_level_streaming` because `write_level` consumes an infallible
/// iterator but Arrow IPC read-back can fail.
fn drain_sink(
    writer: &mut OverviewWriter<File>,
    level_idx: usize,
    hint: usize,
    sink: LevelSink,
) -> Result<LevelWriteOutcome, ConvertError> {
    match sink {
        LevelSink::Ram(batches) => {
            Ok(writer.write_level(level_idx, Some(hint), batches.into_iter())?)
        }
        LevelSink::Spill(state) => {
            // `_temp` keeps the spill file on disk until the reader is drained.
            let (mut reader, _temp) = state.into_reader()?;
            let err: std::cell::RefCell<Option<ConvertError>> = std::cell::RefCell::new(None);
            let iter = std::iter::from_fn(|| match reader.next() {
                None => None,
                Some(Ok(b)) => Some(b),
                Some(Err(e)) => {
                    *err.borrow_mut() = Some(ConvertError::Arrow(e));
                    None
                }
            });
            let res = writer.write_level(level_idx, Some(hint), iter);
            if let Some(e) = err.borrow_mut().take() {
                return Err(e); // spill read error takes precedence over the writer's
            }
            Ok(res?)
        }
    }
}
