//! Ordered input reading (#494; moved out of `pipeline.rs` by #538): one
//! sequential reader, or several reader workers over disjoint row-group
//! segments merged back into the identical batch sequence.
//!
//! Both passes read through here: pass 1's scan ([`super::stream`]) and the
//! single-read pass-2 engine ([`super::pipeline`]). The module depends only on
//! the input set, the memory budget and [`ConvertError`], so neither caller
//! has to import the other to read.

use std::time::{Duration, Instant};

use arrow_array::RecordBatch;
use crossbeam_channel::Sender;

use super::convert::ConvertError;
use super::level::MemoryProfile;
use super::memory::{Grant, MemoryBudget, MemoryPhase, Price, PriceBasis};
use crate::input_set::{ConvertSource, ReadPlan, ReadSegment, RowGroupSelection};

/// What the caller wants from [`read_in_order`], and what it knows about the
/// rows it is about to read.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReadTuning {
    pub(super) budget: MemoryBudget,
    /// Rows per delivered batch. The merge reproduces this chunking exactly,
    /// however many workers actually read.
    pub(super) batch_size: usize,
    /// Reader threads asked for (already resolved; `1` = sequential).
    pub(super) workers: usize,
    /// Average encoded-geometry bytes per input row (#305), from the footers
    /// (#717), used only to size the read-ahead against the memory budget.
    /// `None` falls back to [`READ_ROW_BYTES_FALLBACK`].
    pub(super) avg_geom_bytes: Option<u64>,
    /// What the footers say this read decodes ([`footer_price`]): what
    /// prices a read batch and a reader's own row group. `None` falls back to
    /// the per-row model floored at a whole decoded batch.
    pub(super) footer: Option<FooterPrice>,
    /// The run's memory profile, which decides the shape the way
    /// [`MemoryBudget::fit`] does for every phase: `auto` reads with what the
    /// read share affords; `bounded` the same within main's caps
    /// ([`READ_WORKERS_BOUNDED_MAX`] workers, [`READ_AHEAD_BOUNDED_MAX_BATCHES`]
    /// deep); `speed` with every requested worker at the full depth.
    pub(super) profile: MemoryProfile,
}

/// Which pass is reading. Both read through [`read_in_order`], but they feed
/// consumers with different CPU demand, so the reader count is resolved per
/// pass (#723, `convert::resolve_pass_read_workers`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadPass {
    /// Pass 1's scan: a light per-batch decode and key extraction.
    Scan,
    /// Pass 2's level build: per-batch simplification across every level.
    Build,
}

/// Whether [`read_in_order`]'s consumer wants more batches.
pub(super) enum ReadFlow {
    Continue,
    /// The consumer hung up (its own downstream is gone). Not an error: the
    /// real one is reported by whoever owns the downstream.
    Stop,
}

/// Ceiling on a worker's read-ahead under `bounded`: main's cap (#788,
/// `bounded` keeps main's caps per phase; the allowance can only shrink
/// them). The caller asked for memory over speed, so a rich machine does not
/// get to build the full [`READ_AHEAD_MAX_BATCHES`] queue per worker.
///
/// This caps the *generous* end and nothing else. Halving the budget slice
/// instead was measured and rejected: it took a 58-column fixture from 3
/// reader threads to none under `bounded`, and `bounded` is a profile this
/// parallel read exists to speed up (#494's Brazil measurement is a
/// `--profile bounded` run). A ceiling on depth never costs a worker, and it
/// only binds when `affordable / workers` was already above it.
const READ_AHEAD_BOUNDED_MAX_BATCHES: usize = READ_AHEAD_MAX_BATCHES / 2;

/// Most read workers under `bounded`: main's cap, its auto reader count's
/// ceiling (`convert::READ_WORKERS_MAX`), applied to an explicit
/// `--read-workers` too (#788).
const READ_WORKERS_BOUNDED_MAX: usize = 4;

/// Floor on a worker's read-ahead, in batches. Below this a worker cannot hold
/// even a small segment, so it stalls mid-segment and its read does not
/// overlap anything — at which point a worker is worse than no worker.
const READ_AHEAD_MIN_BATCHES: usize = 4;

/// Ceiling on a worker's read-ahead, in batches. A deeper queue only buys
/// tolerance for longer consumer stalls, and the cost is resident Arrow data.
const READ_AHEAD_MAX_BATCHES: usize = 32;

/// A resolved parallel-read shape: how many workers, how deep each one's
/// queue is, and how large a segment may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadShape {
    workers: usize,
    depth: usize,
    segment_target_rows: usize,
}

/// What one resident read batch costs (#716).
///
/// From the footers when [`ReadTuning::row_bytes`] is known
/// ([`footer_row_bytes`]), and never past [`tylertoo_io::decoded_batch_limit`],
/// the most the reader builds into one batch. Measured
/// (`.788/PROFILE-LOG.md`, dhat): Moldova's 8,192-row batches hold 7.6 MiB
/// live, priced at 17 MiB this way where the model below charged 64 MiB.
///
/// Without footer bytes, the per-row model floored at a whole decoded batch.
/// The model's fixed non-geometry term is not decoration: the geometry weight
/// covers ONLY the geometry column, and a read batch carries every projected
/// column — pricing a row at `2 × avg_geom` alone under-charged wide schemas
/// by 17-232×, which is how a bounded run at DEFAULTS once modelled 21 MiB of
/// read-ahead and held +370 MiB.
fn read_batch_price(tuning: ReadTuning) -> Price {
    let batch_size = tuning.batch_size.max(1) as u64;
    let limit = tylertoo_io::decoded_batch_limit();
    match tuning.footer {
        Some(FooterPrice { row_bytes: row, .. }) => Price::new(
            row.saturating_mul(batch_size).clamp(1, limit.max(1)),
            PriceBasis::Footer,
        ),
        None => Price::model(
            tuning
                .budget
                .estimate(MemoryPhase::Read, batch_size, tuning.avg_geom_bytes)
                .max(limit)
                .max(1),
        ),
    }
}

/// What one reader holds besides its queue, in read batches (#716): the
/// largest row group it decodes, at the footers' decoded row. Measured
/// (`.788/PROFILE-LOG.md`, dhat): NZ parcels' pass-1 readers held about 49
/// MiB each inside the parquet reader under 4 workers, unpriced until then.
fn reader_batches(tuning: ReadTuning) -> usize {
    let Some(footer) = tuning.footer else {
        return 0;
    };
    let price = read_batch_price(tuning).bytes.max(1);
    usize::try_from(
        footer
            .row_bytes
            .saturating_mul(footer.group_rows)
            .div_ceil(price),
    )
    .unwrap_or(usize::MAX)
}

/// Charge `budget` for the read-ahead `tuning` will hold (#716): every
/// worker's queue at its resolved depth and its reader's row group, plus the
/// `channel` batches queued
/// between the read and its consumer. `tuning.budget` must be the state
/// `budget` was in before this call, so [`read_in_order`] resolves the same
/// shape from it.
pub(super) fn grant_read_ahead(
    budget: &mut MemoryBudget,
    tuning: ReadTuning,
    channel: usize,
) -> Grant {
    let shape = resolve_read_shape(tuning);
    let batches = shape
        .workers
        .saturating_mul(shape.depth.saturating_add(reader_batches(tuning)))
        .saturating_add(channel);
    budget.fit(
        tuning.profile,
        MemoryPhase::Read,
        read_batch_price(tuning),
        batches,
        batches,
    )
}

/// Decoded bytes per footer byte (#716). A footer's uncompressed size is
/// AFTER dictionary and RLE encoding, so a dictionary string column decodes
/// past it unless the footer also records its unencoded byte-array size,
/// which pyarrow-written files do not. Measured (`.788/PROFILE-LOG.md`,
/// dhat): Moldova's batches hold 0.96× their footer bytes, NZ parcels' (801
/// byte-array chunks without the statistic) 1.9×. Priced at the worse one.
const DECODED_PER_FOOTER_BYTE: u64 = 2;

/// What the footers say a read decodes (#716).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FooterPrice {
    /// Decoded bytes per row over the read's columns.
    pub(super) row_bytes: u64,
    /// Rows in the largest selected row group: what one reader decodes at
    /// once.
    pub(super) group_rows: u64,
}

/// Decoded bytes per row over `columns` of the selected row groups, from the
/// footers: each column's uncompressed size, or its unencoded byte-array
/// size when larger ([`ConvertSource::selected_column_bytes`]), times
/// [`DECODED_PER_FOOTER_BYTE`], plus 8 bytes per column for the offsets and
/// validity a decoded column adds; and the largest selected row group's
/// rows. `None` without rows.
pub(super) fn footer_price(
    source: &ConvertSource,
    row_groups: Option<&RowGroupSelection>,
    columns: impl IntoIterator<Item = usize>,
) -> Result<Option<FooterPrice>, ConvertError> {
    let rows = u64::try_from(source.selected_row_count(row_groups)?).unwrap_or(0);
    if rows == 0 {
        return Ok(None);
    }
    let mut bytes = 0u64;
    let mut count = 0u64;
    for column in columns {
        bytes = bytes.saturating_add(source.selected_column_bytes(row_groups, column)?);
        count += 1;
    }
    let counts = source.part_row_group_row_counts()?;
    let group_rows = counts
        .iter()
        .enumerate()
        .flat_map(|(part, groups)| {
            let picked: Vec<usize> = row_groups
                .and_then(|s| s.parts().get(part))
                .map_or_else(|| (0..groups.len()).collect(), Clone::clone);
            picked.into_iter().filter_map(|g| groups.get(g).copied())
        })
        .max()
        .unwrap_or(0)
        .max(0) as u64;
    Ok(Some(FooterPrice {
        row_bytes: bytes
            .div_ceil(rows)
            .saturating_mul(DECODED_PER_FOOTER_BYTE)
            .saturating_add(count.saturating_mul(8))
            .max(1),
        group_rows,
    }))
}

/// Size the parallel read against the memory budget.
///
/// The load-bearing relationship is `segment_target_rows <= (depth - 1) *
/// batch_size`: **a segment must fit in its worker's queue**. Workers own
/// disjoint segments but the merge consumes them strictly in order, so a
/// worker whose queue fills before its segment ends simply parks until the
/// merge reaches it — every worker takes its turn and the read is sequential
/// again, with extra threads. Sized this way a worker finishes its segment
/// into the queue and starts the next one, which is what makes the reads
/// actually concurrent.
///
/// Everything else falls out of that: the budget fixes how many batches may
/// be resident at once, workers are dropped (never below 1) until each one
/// can hold [`READ_AHEAD_MIN_BATCHES`], and the depth is what is left over.
fn resolve_read_shape(tuning: ReadTuning) -> ReadShape {
    let batch_size = tuning.batch_size.max(1);
    // What the profile affords in batches of `read_batch_price`
    // (`MemoryBudget::affordable`): everything under `speed`, so every
    // requested worker at the full depth; otherwise the read share of the
    // allowance (`memory::READ_SHARE`), which bounds `workers × (depth +
    // reader) × price`. `bounded` also keeps main's worker and depth caps.
    let affordable =
        tuning
            .budget
            .affordable(tuning.profile, MemoryPhase::Read, read_batch_price(tuning));
    let reader = reader_batches(tuning);
    let (max_workers, max_depth) = match tuning.profile {
        MemoryProfile::Bounded => (READ_WORKERS_BOUNDED_MAX, READ_AHEAD_BOUNDED_MAX_BATCHES),
        MemoryProfile::Auto | MemoryProfile::Speed => (usize::MAX, READ_AHEAD_MAX_BATCHES),
    };

    // Closed form of "drop workers until each one can hold the floor":
    // `affordable / w >= READ_AHEAD_MIN_BATCHES` ⟺ `w <= affordable /
    // READ_AHEAD_MIN_BATCHES` (integer division, both sides). Written as a
    // decrementing loop this was O(workers) — unbounded, since an explicit
    // `--read-workers` is the caller's number, and `--read-workers
    // 18446744073709551615` hung the process before it read a byte.
    // Each worker also holds its reader's own row group (`reader`, in
    // batches), so the floor it must afford is that plus the minimum queue.
    let workers = tuning
        .workers
        .clamp(1, max_workers)
        .min((affordable / (READ_AHEAD_MIN_BATCHES + reader)).max(1));
    let depth = (affordable / workers)
        .saturating_sub(reader)
        .clamp(READ_AHEAD_MIN_BATCHES, max_depth);
    ReadShape {
        workers,
        depth,
        segment_target_rows: depth.saturating_sub(1).max(1).saturating_mul(batch_size),
    }
}

/// One worker's report on one segment.
enum SegMsg {
    /// A batch as the worker's own reader produced it, with the time that
    /// read cost (core-seconds for the [profile] split).
    Batch(RecordBatch, Duration),
    /// The segment is complete; the merge moves on to the next one.
    End,
    Err(ConvertError),
}

/// The parallel-read gate (#544): more than one worker, and a read that
/// needs no network ([`ConvertSource::reads_locally`]: a local input, or a
/// remote one whose every chunk the read touches is staged).
fn reads_in_parallel(
    source: &ConvertSource,
    row_groups: Option<&RowGroupSelection>,
    projection: Option<&[usize]>,
    shape: ReadShape,
) -> bool {
    shape.workers > 1 && source.reads_locally(row_groups, projection)
}

#[cfg(test)]
thread_local! {
    /// Reader threads the calling thread's last [`read_in_order`] used: 1
    /// for a sequential read (test counter, #544).
    static LAST_READ_WORKERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Deliver every selected input row to `on_batch`, in exact read order, in
/// batches of at most `tuning.batch_size` rows, preserving the I/O owner's
/// memory and offset bounds with either sequential or concurrent readers.
///
/// **Why this exists (#494).** Pass 2 read the input from a single thread
/// iterating one `ParquetRecordBatchReader`: 110.6 of the 403 wall seconds of
/// a bounded Brazil-55M pass 2, and the largest single stage in the profile.
/// Parquet row groups are independently readable, so the work parallelizes —
/// the hard part is putting it back together.
///
/// **Why the merge re-chunks.** The sequential reader chains all of a part's
/// selected row groups into one continuous decode, so its batches are exactly
/// `batch_size` rows regardless of where the row-group boundaries fall.
/// Workers reading disjoint runs necessarily produce a short batch at each
/// run's end, and those boundaries are NOT free: the overview writer slices
/// its row groups out of the batches it is handed, one `write` call per slice,
/// and parquet checks its data-page limits per call — so a differently-chunked
/// stream of the same rows can produce different page boundaries and therefore
/// different bytes. The merge therefore re-assembles the exact chunking a
/// single reader would have produced. Batches that already line up are passed
/// through untouched; only the batch straddling a worker seam is copied.
///
/// **Gating** ([`reads_in_parallel`], #544). A remote input reads
/// sequentially unless every chunk this read touches is already in its
/// parts' disk spill (pass-0 staging):
/// `SourceStream` releases a part's in-memory read cache when it finishes
/// that part, so concurrent readers fetching over one remote source would
/// evict each other's chunks. A fully staged read fetches nothing, so it
/// reads in parallel like a local one
/// ([`ConvertSource::reads_locally`]).
///
/// `projection` is a [`ReadPlan::projection`]: root columns of the schema the
/// source exposes, `None` for all of them. Every worker reads the same one.
/// Pass 2 reads every column; pass 1 reads the geometry plus the columns its
/// flags name (#717).
pub(super) fn read_in_order<F>(
    source: &ConvertSource,
    row_groups: Option<&RowGroupSelection>,
    projection: Option<&[usize]>,
    tuning: ReadTuning,
    mut on_batch: F,
) -> Result<(), ConvertError>
where
    F: FnMut(RecordBatch, usize, Duration) -> Result<ReadFlow, ConvertError>,
{
    let batch_size = tuning.batch_size.max(1);
    let shape = resolve_read_shape(tuning);
    let parallel = reads_in_parallel(source, row_groups, projection, shape);
    if shape.workers > 1 && !parallel {
        log::info!(
            "[convert] reading sequentially: remote input not fully staged \
             (parallel readers would evict each other's fetched chunks)"
        );
    }
    #[cfg(test)]
    LAST_READ_WORKERS.with(|w| w.set(1));
    if !parallel {
        return read_sequentially(source, row_groups, projection, batch_size, &mut on_batch);
    }
    let segments = plan_segments(source, row_groups, shape)?;
    if segments.len() < 2 {
        return read_sequentially(source, row_groups, projection, batch_size, &mut on_batch);
    }
    let workers = shape.workers.min(segments.len());
    #[cfg(test)]
    LAST_READ_WORKERS.with(|w| w.set(workers));
    log::info!(
        "[convert] read: {workers} reader thread(s) over {} segment(s) \
         (~{} row(s) each, {} read-ahead batch(es) per reader)",
        segments.len(),
        segments.iter().map(|s| s.rows).sum::<usize>() / segments.len(),
        shape.depth,
    );
    // A row group larger than a worker's whole queue cannot be buffered ahead,
    // so that worker parks mid-segment until the merge reaches it and the
    // reads serialize again. Worth saying out loud: it is a property of the
    // INPUT's row-group sizing, and `gpio` can fix it at the source.
    let queue_rows = shape.depth.saturating_mul(batch_size);
    if let Some(biggest) = segments.iter().map(|s| s.rows).max() {
        if biggest > queue_rows {
            log::debug!(
                "[convert] read: largest segment is {biggest} row(s) but a \
                 reader can buffer only {queue_rows} — reads will only partly \
                 overlap (the input's row groups are large relative to the \
                 read-ahead budget)"
            );
        }
    }
    read_in_parallel(
        source,
        &segments,
        projection,
        batch_size,
        workers,
        shape.depth,
        &mut on_batch,
    )
}

/// Segments each reader worker should get, at least, when the row groups
/// allow it (#717).
///
/// [`ReadShape::segment_target_rows`] is a ceiling set by what a worker can
/// buffer. On a machine with memory to spare it is large, and on its own it
/// cut a 20-row-group input into 3 segments for 4 workers: the merge then
/// drained worker 0's long first segment at one reader's speed while the
/// others sat on full queues. Smaller segments keep every worker reading
/// for the whole pass. Each one costs a reader open (the footer is reused)
/// and, at its seam, one spliced batch. On the 20× Madagascar input, going
/// from 1 to 4 segments per worker took pass 1 from 0.46 s to 0.26 s and pass
/// 2 from 2.2 s to 1.55 s.
const SEGMENTS_PER_WORKER: usize = 4;

/// The segments [`read_in_order`] hands its workers: at most the shape's
/// ceiling each, and small enough that every worker gets
/// [`SEGMENTS_PER_WORKER`] of them when the row groups allow. Row groups are
/// never split, so a run of large ones still gives fewer segments.
fn plan_segments(
    source: &ConvertSource,
    row_groups: Option<&RowGroupSelection>,
    shape: ReadShape,
) -> Result<Vec<ReadSegment>, ConvertError> {
    let rows = usize::try_from(source.selected_row_count(row_groups)?).unwrap_or(0);
    let even = rows
        .div_ceil(shape.workers.saturating_mul(SEGMENTS_PER_WORKER).max(1))
        .max(1);
    Ok(source.read_segments(row_groups, shape.segment_target_rows.min(even))?)
}

/// How many segments [`read_in_order`] would read concurrently for this
/// source, selection and tuning; `0` when it would read sequentially.
///
/// Test seam: an equivalence test between worker counts compares the
/// sequential reader with itself unless the fixture actually splits, and this
/// is the same gating `read_in_order` applies.
#[cfg(test)]
pub(super) fn parallel_read_segments(
    source: &ConvertSource,
    row_groups: Option<&RowGroupSelection>,
    tuning: ReadTuning,
) -> usize {
    let shape = resolve_read_shape(tuning);
    if !reads_in_parallel(source, row_groups, None, shape) {
        return 0;
    }
    match plan_segments(source, row_groups, shape) {
        Ok(segments) if segments.len() >= 2 => segments.len(),
        _ => 0,
    }
}

/// The pre-#494 path, unchanged and still the reference: one reader, one
/// thread, batches straight through.
fn read_sequentially<F>(
    source: &ConvertSource,
    row_groups: Option<&RowGroupSelection>,
    projection: Option<&[usize]>,
    batch_size: usize,
    on_batch: &mut F,
) -> Result<(), ConvertError>
where
    F: FnMut(RecordBatch, usize, Duration) -> Result<ReadFlow, ConvertError>,
{
    let mut reader = source.open_stream(&ReadPlan {
        batch_size,
        projection,
        row_groups,
    })?;
    let mut row_offset = 0usize;
    loop {
        let t_read = Instant::now();
        match reader.next() {
            None => return Ok(()),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(batch)) => {
                // An empty batch carries no rows and no offset advance, so
                // delivering one is a no-op the parallel merge does not make
                // either ([`Regrouper::push`] drops it). Skipping keeps the
                // two paths' delivered batch sequences identical even if a
                // reader ever hands one back.
                if batch.num_rows() == 0 {
                    continue;
                }
                let read_dur = t_read.elapsed();
                let offset = row_offset;
                row_offset += batch.num_rows();
                if matches!(on_batch(batch, offset, read_dur)?, ReadFlow::Stop) {
                    return Ok(());
                }
            }
        }
    }
}

/// `workers` readers over disjoint segments, merged back into read order on
/// this thread.
///
/// Segment `i` is read by worker `i % workers`, so while the merge drains
/// worker 0's segment the other workers are filling their queues with the
/// segments that come next. Each worker has its own channel: a single shared
/// queue would let a worker several segments ahead consume the whole budget
/// and starve the worker the merge is actually waiting for.
///
/// **The assignment is static round-robin — there is no work stealing.** A
/// worker that draws an unusually expensive segment (a badly compressed row
/// group, a vertex-heavy run) stalls the merge behind it even while the other
/// workers sit on full queues with nothing to do. Read-ahead absorbs most of
/// it: the merge keeps draining the queues of the segments that follow, which
/// is `depth` batches per worker of slack. Segments are also size-capped, not
/// count-capped ([`ConvertSource::read_segments`]), so the variance is in
/// decode cost rather than row count. Worth revisiting only if a profile shows
/// the merge waiting — the fix (a shared work queue plus a reorder buffer)
/// costs exactly the in-order simplicity this merge is built on.
fn read_in_parallel<F>(
    source: &ConvertSource,
    segments: &[ReadSegment],
    projection: Option<&[usize]>,
    batch_size: usize,
    workers: usize,
    depth: usize,
    on_batch: &mut F,
) -> Result<(), ConvertError>
where
    F: FnMut(RecordBatch, usize, Duration) -> Result<ReadFlow, ConvertError>,
{
    std::thread::scope(|scope| -> Result<(), ConvertError> {
        // Receivers live in THIS frame, so returning by any path — including
        // an unwind — drops them before the scope joins, releasing any worker
        // parked in `send` (the `super::pipe` liveness contract, #362).
        let mut rxs = Vec::with_capacity(workers);
        for w in 0..workers {
            let (tx, rx) = crossbeam_channel::bounded::<SegMsg>(depth);
            rxs.push(rx);
            // Named so a stack dump, `perf`, or an OS thread listing tells a
            // reader worker apart from the rayon pool and the spill writers.
            std::thread::Builder::new()
                .name(format!("tylertoo-read-{w}"))
                .spawn_scoped(scope, move || {
                    read_segments_for_worker(
                        source, segments, projection, batch_size, workers, w, &tx,
                    )
                })?;
        }

        let mut regroup = Regrouper::default();
        let mut row_offset = 0usize;
        let mut pending_read = Duration::ZERO;
        let mut current_part: Option<usize> = None;

        let mut prev_rg: Option<usize> = None;
        for (seq, segment) in segments.iter().enumerate() {
            // The merge's correctness rests entirely on `read_segments`
            // emitting parts in order and, within a part, ascending disjoint
            // row-group runs — a cross-module contract that is otherwise only
            // stated in prose. Enforce it here so a change on either side
            // fails a debug build instead of silently reordering rows.
            if let Some(prev) = current_part {
                debug_assert!(
                    segment.part >= prev,
                    "read_segments must emit parts in order: part {} after part {prev}",
                    segment.part
                );
                debug_assert!(
                    segment.part > prev
                        || prev_rg.is_none()
                        || segment.row_groups.first().copied() > prev_rg,
                    "read_segments must emit ascending, disjoint row-group runs \
                     within a part: {:?} after row group {prev_rg:?}",
                    segment.row_groups.first()
                );
            }
            prev_rg = segment.row_groups.last().copied();
            // A part boundary is a batch boundary: the sequential reader opens
            // a fresh reader per part, so a part's last batch is short.
            if current_part != Some(segment.part) {
                while let Some(tail) = regroup.take_all()? {
                    if matches!(
                        deliver(tail, &mut row_offset, &mut pending_read, on_batch)?,
                        ReadFlow::Stop
                    ) {
                        return Ok(());
                    }
                }
                current_part = Some(segment.part);
            }
            loop {
                match rxs[seq % workers].recv() {
                    // A disconnected channel means the worker returned without
                    // sending `End`, which it does only by panicking: the
                    // typed error below is returned, unwinds out of the scope,
                    // and `thread::scope`'s join then re-raises the worker's
                    // panic, superseding it. That precedence is intended — the
                    // panic is the cause and this is the symptom — so the
                    // message only has to be right for the impossible case
                    // where the worker somehow returned cleanly.
                    Err(_) => {
                        return Err(internal(
                            "a pass-2 reader worker ended without closing its segment",
                        ))
                    }
                    Ok(SegMsg::Err(e)) => return Err(e),
                    Ok(SegMsg::End) => break,
                    Ok(SegMsg::Batch(batch, read_dur)) => {
                        pending_read += read_dur;
                        regroup.push(batch);
                        while regroup.rows >= batch_size || regroup.bounded_rows() < regroup.rows {
                            let out = regroup.take(batch_size.min(regroup.rows))?;
                            if matches!(
                                deliver(out, &mut row_offset, &mut pending_read, on_batch)?,
                                ReadFlow::Stop
                            ) {
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
        while let Some(tail) = regroup.take_all()? {
            if matches!(
                deliver(tail, &mut row_offset, &mut pending_read, on_batch)?,
                ReadFlow::Stop
            ) {
                break;
            }
        }
        Ok(())
    })
}

/// Worker `w`: read segments `w, w + workers, …` in order, streaming each
/// one's batches over `tx` and closing it with [`SegMsg::End`].
///
/// A send failure means the merge hung up (it errored, or the consumer
/// stopped), so the worker returns quietly — the real error belongs to
/// whoever hung up, exactly as in [`super::pipe`].
fn read_segments_for_worker(
    source: &ConvertSource,
    segments: &[ReadSegment],
    projection: Option<&[usize]>,
    batch_size: usize,
    workers: usize,
    w: usize,
    tx: &Sender<SegMsg>,
) {
    let mut seq = w;
    while seq < segments.len() {
        let mut stream = match source.open_segment(&segments[seq], projection, batch_size) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(SegMsg::Err(e.into()));
                return;
            }
        };
        loop {
            let t_read = Instant::now();
            match stream.next() {
                None => break,
                Some(Err(e)) => {
                    let _ = tx.send(SegMsg::Err(e.into()));
                    return;
                }
                Some(Ok(batch)) => {
                    if tx.send(SegMsg::Batch(batch, t_read.elapsed())).is_err() {
                        return;
                    }
                }
            }
        }
        if tx.send(SegMsg::End).is_err() {
            return;
        }
        seq += workers;
    }
}

/// Hand one re-assembled batch to the consumer, advancing the global row
/// offset and attributing the read time accumulated since the previous one.
/// The per-batch attribution is arbitrary; the SUM is what the [profile]
/// stage split reports, and it is exact.
fn deliver<F>(
    batch: RecordBatch,
    row_offset: &mut usize,
    pending_read: &mut Duration,
    on_batch: &mut F,
) -> Result<ReadFlow, ConvertError>
where
    F: FnMut(RecordBatch, usize, Duration) -> Result<ReadFlow, ConvertError>,
{
    let offset = *row_offset;
    *row_offset += batch.num_rows();
    on_batch(batch, offset, std::mem::take(pending_read))
}

/// The in-order merge's re-chunker: a FIFO of batch slices that hands back
/// runs up to a requested row count within the I/O owner's bounds.
///
/// Batches that already line up cost nothing — [`Regrouper::take`] returns the
/// queued batch itself when it holds exactly the requested rows — so in the
/// common case (row groups an exact multiple of the batch size, or a seam that
/// happens to land on one) nothing is copied at all. Only a run spanning a
/// worker seam is concatenated.
struct Regrouper {
    queue: std::collections::VecDeque<RecordBatch>,
    rows: usize,
    limit: u64,
}

impl Default for Regrouper {
    fn default() -> Self {
        Self {
            queue: Default::default(),
            rows: 0,
            limit: tylertoo_io::byte_array_limit(),
        }
    }
}

impl Regrouper {
    fn bounded_rows(&self) -> usize {
        tylertoo_io::concat_prefix_rows(&self.queue, self.limit)
    }
    fn push(&mut self, batch: RecordBatch) {
        if batch.num_rows() > 0 {
            self.rows += batch.num_rows();
            self.queue.push_back(batch);
        }
    }

    /// Remove at most `n` rows, without joining past the reader's bound.
    /// `n` must be positive and `<= self.rows`.
    #[expect(
        clippy::disallowed_methods,
        reason = "#779: I/O owner bounds the prefix before a segment seam is concatenated"
    )] // #779: seam joins (#494)
    fn take(&mut self, n: usize) -> Result<RecordBatch, ConvertError> {
        debug_assert!(n > 0 && n <= self.rows);
        let n = n.min(self.bounded_rows());
        let front = self
            .queue
            .front()
            .ok_or_else(|| internal("regrouper asked for rows it does not hold"))?;
        if front.num_rows() == n {
            self.rows -= n;
            return self
                .queue
                .pop_front()
                .ok_or_else(|| internal("regrouper queue vanished"));
        }
        let schema = front.schema();
        let mut parts: Vec<RecordBatch> = Vec::new();
        let mut need = n;
        while need > 0 {
            let batch = self
                .queue
                .pop_front()
                .ok_or_else(|| internal("regrouper ran out of rows mid-take"))?;
            if batch.num_rows() <= need {
                need -= batch.num_rows();
                parts.push(batch);
            } else {
                parts.push(batch.slice(0, need));
                let rest = batch.slice(need, batch.num_rows() - need);
                self.queue.push_front(rest);
                need = 0;
            }
        }
        self.rows -= n;
        Ok(arrow_select::concat::concat_batches(&schema, &parts)?)
    }

    /// Remove a bounded prefix of the remainder, or `None` when empty.
    fn take_all(&mut self) -> Result<Option<RecordBatch>, ConvertError> {
        if self.rows == 0 {
            return Ok(None);
        }
        self.take(self.rows).map(Some)
    }
}

/// An invariant this module owns was broken — never reachable from user input,
/// so it carries no advice, only the broken invariant.
pub(super) fn internal(what: &str) -> ConvertError {
    ConvertError::Io(std::io::Error::other(format!("internal: {what}")))
}

/// The ordered parallel reader (#494): that `--read-workers N` delivers
/// exactly what one reader delivers, whatever N is and however unevenly the
/// workers are paced.
#[cfg(test)]
mod read_tests {
    use super::*;
    use arrow_array::{ArrayRef, Int64Array};
    use arrow_schema::{DataType, Field, Schema};
    use std::fs::File;
    use std::sync::Arc;
    use tylertoo_io::test_support::ArrowWriter;
    use tylertoo_io::test_support::WriterProperties;

    #[test]
    fn regrouping_preserves_byte_safe_reader_splits() {
        use arrow_array::StringArray;
        let mut regroup = Regrouper {
            limit: 1000,
            ..Default::default()
        };
        for value in ["x".repeat(600), "y".repeat(600)] {
            let mut values = vec![String::new(); 50];
            values[0] = value;
            regroup.push(
                RecordBatch::try_from_iter(vec![(
                    "name",
                    Arc::new(StringArray::from(values)) as ArrayRef,
                )])
                .unwrap(),
            );
        }
        let batch = regroup.take(100).unwrap();
        let array = batch
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert!(
            *array.value_offsets().last().unwrap() <= 1000,
            "parallel regrouping reconstructed an oversized column"
        );
        assert_eq!(batch.num_rows(), 50);
        let tail = regroup.take_all().unwrap().unwrap();
        assert_eq!(tail.num_rows(), 50);
        assert_eq!(
            tail.column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0),
            "y".repeat(600)
        );
        assert!(regroup.take_all().unwrap().is_none());
    }

    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]))
    }

    /// Write `rows` sequentially-numbered rows into `path`, `rg_rows` per row
    /// group — the shape that decides how the input can be split.
    fn write_input(path: &std::path::Path, rows: i64, rg_rows: usize) {
        let s = schema();
        let batch = RecordBatch::try_new(
            s.clone(),
            vec![Arc::new(Int64Array::from((0..rows).collect::<Vec<_>>())) as ArrayRef],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(rg_rows))
            .build();
        let mut w = ArrowWriter::try_new(File::create(path).unwrap(), s, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    }

    /// `(row counts per delivered batch, the ids in delivery order, the
    /// row offsets each batch was announced at)`.
    type Delivered = (Vec<usize>, Vec<i64>, Vec<usize>);

    /// Read `source` through [`read_in_order`] with a forced shape, optionally
    /// pausing in the consumer to pace the workers unevenly.
    fn read_all(
        source: &ConvertSource,
        batch_size: usize,
        workers: usize,
        stall: Option<Duration>,
    ) -> Delivered {
        let (mut counts, mut ids, mut offsets) = (Vec::new(), Vec::new(), Vec::new());
        read_in_order(
            source,
            None,
            None,
            ReadTuning {
                budget: MemoryBudget::with_available(Some(256 << 30)),
                batch_size,
                workers,
                // Tiny rows: the read-ahead sizing must not silently collapse
                // the worker count in these fixtures.
                avg_geom_bytes: Some(8),
                footer: None,
                profile: MemoryProfile::Auto,
            },
            |batch, offset, _dur| {
                if let Some(d) = stall {
                    std::thread::sleep(d);
                }
                counts.push(batch.num_rows());
                offsets.push(offset);
                ids.extend(
                    batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap()
                        .values()
                        .iter()
                        .copied(),
                );
                Ok(ReadFlow::Continue)
            },
        )
        .unwrap();
        (counts, ids, offsets)
    }

    /// The whole contract in one assertion: for a batch size that does NOT
    /// divide the row-group size — so every worker seam falls mid-batch and
    /// the merge has to splice — 2, 3 and 4 workers must deliver byte-for-byte
    /// what 1 worker delivers, batch boundaries included. Batch boundaries are
    /// not cosmetic: the overview writer issues one column-writer call per
    /// slice it is handed, and parquet checks its data-page limits per call.
    #[test]
    fn parallel_reads_reproduce_the_sequential_batch_sequence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.parquet");
        // 9 row groups of 700 rows; batch 128 divides neither 700 nor 6300.
        write_input(&path, 6300, 700);
        let source = ConvertSource::resolve(path.to_str().unwrap()).unwrap();
        // Guard the guard: if the read-ahead sizing ever leaves this fixture
        // with a single segment, the comparison below is vacuous.
        let shape = resolve_read_shape(ReadTuning {
            budget: MemoryBudget::with_available(Some(256 << 30)),
            batch_size: 128,
            workers: 4,
            avg_geom_bytes: Some(8),
            footer: None,
            profile: MemoryProfile::Auto,
        });
        let segments = plan_segments(&source, None, shape).unwrap();
        assert!(
            segments.len() > 1 && shape.workers > 1,
            "fixture must actually split: {} segment(s), {} worker(s)",
            segments.len(),
            shape.workers
        );
        let seam: usize = segments[0].rows;
        assert_ne!(
            seam % 128,
            0,
            "the seam must fall mid-batch, or the merge's splice path is never taken"
        );
        let reference = read_all(&source, 128, 1, None);
        assert_eq!(
            reference.0.iter().sum::<usize>(),
            6300,
            "the reference read must cover every row"
        );
        assert_eq!(
            *reference.0.last().unwrap(),
            6300 % 128,
            "only the final batch may be short"
        );
        for workers in [2usize, 3, 4] {
            let got = read_all(&source, 128, workers, None);
            assert_eq!(
                got, reference,
                "--read-workers {workers} must deliver the same batches, ids and \
                 row offsets as a single reader"
            );
        }
    }

    /// #717: the queue bound alone let a roomy machine cut 4 workers' worth
    /// of input into 2–3 segments, and the merge then waited on worker 0's
    /// long first segment while the rest sat idle. With room to spare, each
    /// worker gets several segments, as far as the row groups allow.
    #[test]
    fn parallel_reads_give_each_worker_several_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.parquet");
        // 9 row groups of 700 rows: the queue bound alone allows 3968-row
        // segments, i.e. two segments for four workers.
        write_input(&path, 6300, 700);
        let source = ConvertSource::resolve(path.to_str().unwrap()).unwrap();
        let tuning = ReadTuning {
            budget: MemoryBudget::with_available(Some(256 << 30)),
            batch_size: 128,
            workers: 4,
            avg_geom_bytes: Some(8),
            footer: None,
            profile: MemoryProfile::Auto,
        };
        let shape = resolve_read_shape(tuning);
        assert_eq!(shape.workers, 4);
        assert!(
            source
                .read_segments(None, shape.segment_target_rows)
                .unwrap()
                .len()
                < 4,
            "the queue bound alone must under-split this fixture, or the test is vacuous"
        );
        let segments = plan_segments(&source, None, shape).unwrap();
        assert_eq!(
            segments
                .iter()
                .map(|s| s.row_groups.clone())
                .collect::<Vec<_>>(),
            (0..9).map(|g| vec![g]).collect::<Vec<_>>(),
            "one row group per segment: 6300 rows / (4 workers × 4) is under a row group"
        );
        assert_eq!(parallel_read_segments(&source, None, tuning), 9);
    }

    /// #544: a remote source whose read is fully staged to the local spill
    /// reads in parallel like a local one; one that is not yet staged, or
    /// staged for other columns, still reads sequentially. Both reproduce the
    /// sequential reader's batches. On main every remote source read
    /// sequentially, staged or not (`!source.is_remote()`).
    #[cfg(feature = "remote")]
    #[test]
    fn a_fully_staged_remote_source_reads_in_parallel() {
        let s = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("twice", DataType::Int64, false),
        ]));
        let ids: Vec<i64> = (0..3000).collect();
        let batch = RecordBatch::try_new(
            s.clone(),
            vec![
                Arc::new(Int64Array::from(ids.clone())) as ArrayRef,
                Arc::new(Int64Array::from(
                    ids.iter().map(|i| i * 2).collect::<Vec<_>>(),
                )) as ArrayRef,
            ],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(250))
            .build();
        let mut bytes = Vec::new();
        let mut w = ArrowWriter::try_new(&mut bytes, s, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        // A fresh source per case: a read fills the spill as it fetches.
        let fresh = || {
            ConvertSource::single(crate::input::test_memory_source(
                bytes.clone(),
                "staged.parquet",
            ))
        };
        let read = |source: &ConvertSource, workers: usize, projection: Option<&[usize]>| {
            let mut out: Vec<(usize, Vec<i64>)> = Vec::new();
            read_in_order(
                source,
                None,
                projection,
                ReadTuning {
                    budget: MemoryBudget::with_available(Some(256 << 30)),
                    batch_size: 96,
                    workers,
                    avg_geom_bytes: Some(8),
                    footer: None,
                    profile: MemoryProfile::Auto,
                },
                |batch, offset, _dur| {
                    let col = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .unwrap();
                    out.push((offset, col.values().to_vec()));
                    Ok(ReadFlow::Continue)
                },
            )
            .unwrap();
            (out, LAST_READ_WORKERS.with(std::cell::Cell::get))
        };
        let stage_first_column = || {
            let source = fresh();
            let plan = source.staging_plan_for(None, Some(&[0])).unwrap();
            source
                .stage_within(&plan, crate::input::unbudgeted_stage_admission())
                .unwrap();
            source
        };

        let (sequential, workers) = read(&fresh(), 4, None);
        assert_eq!(workers, 1, "nothing staged: sequential");
        // Column 0 staged: a full read still needs column 1 from the network.
        assert_eq!(read(&stage_first_column(), 4, None).1, 1);
        // A read of column 0 alone needs nothing more.
        let (projected, workers) = read(&stage_first_column(), 4, Some(&[0]));
        assert!(workers > 1, "a fully staged read used {workers} reader(s)");
        assert_eq!(projected, sequential);
    }

    /// Pass 1 reads a projection — the geometry plus the columns its flags
    /// name — and #717 moved it onto this reader. Every worker must read the
    /// projected columns only, and the merge must still reproduce the
    /// sequential reader's batches exactly.
    #[test]
    fn parallel_reads_honour_a_projection() {
        use arrow_array::Float64Array;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.parquet");
        let s = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("half", DataType::Float64, false),
        ]));
        let ids: Vec<i64> = (0..3000).collect();
        let batch = RecordBatch::try_new(
            s.clone(),
            vec![
                Arc::new(Int64Array::from(ids.clone())) as ArrayRef,
                Arc::new(Float64Array::from(
                    ids.iter().map(|&i| i as f64 / 2.0).collect::<Vec<_>>(),
                )) as ArrayRef,
            ],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(250))
            .build();
        let mut w = ArrowWriter::try_new(File::create(&path).unwrap(), s, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        let source = ConvertSource::resolve(path.to_str().unwrap()).unwrap();
        let tuning = ReadTuning {
            budget: MemoryBudget::with_available(Some(256 << 30)),
            batch_size: 96,
            workers: 4,
            avg_geom_bytes: Some(8),
            footer: None,
            profile: MemoryProfile::Auto,
        };
        assert!(
            parallel_read_segments(&source, None, tuning) > 1,
            "fixture must split, or the parallel path is never taken"
        );

        let read = |workers: usize| {
            let mut out: Vec<(usize, Vec<f64>)> = Vec::new();
            read_in_order(
                &source,
                None,
                Some(&[1]),
                ReadTuning {
                    budget: MemoryBudget::with_available(Some(256 << 30)),
                    batch_size: 96,
                    workers,
                    avg_geom_bytes: Some(8),
                    footer: None,
                    profile: MemoryProfile::Auto,
                },
                |batch, offset, _dur| {
                    assert_eq!(batch.num_columns(), 1, "only the projected column");
                    assert_eq!(batch.schema().field(0).name(), "half");
                    let col = batch
                        .column(0)
                        .as_any()
                        .downcast_ref::<Float64Array>()
                        .unwrap();
                    out.push((offset, col.values().to_vec()));
                    Ok(ReadFlow::Continue)
                },
            )
            .unwrap();
            out
        };
        let reference = read(1);
        let values: Vec<f64> = reference.iter().flat_map(|(_, v)| v.clone()).collect();
        assert_eq!(
            values,
            ids.iter().map(|&i| i as f64 / 2.0).collect::<Vec<_>>()
        );
        for workers in [2usize, 4] {
            assert_eq!(
                read(workers),
                reference,
                "--read-workers {workers} must read the same projected batches"
            );
        }
    }

    /// Uneven pacing is the interesting case for an in-order merge: a
    /// consumer that stalls lets the fast workers run far ahead and fill their
    /// queues, so the merge sees segments arrive wildly out of completion
    /// order. Delivery order must not notice.
    #[test]
    fn in_order_merge_survives_uneven_worker_pacing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.parquet");
        write_input(&path, 4000, 333);
        let source = ConvertSource::resolve(path.to_str().unwrap()).unwrap();
        let reference = read_all(&source, 64, 1, None);
        let paced = read_all(&source, 64, 4, Some(Duration::from_micros(200)));
        assert_eq!(
            paced, reference,
            "a stalling consumer must not reorder or re-chunk the stream"
        );
    }

    /// Parts are read sequentially and a part's last batch is short, so a part
    /// boundary is a batch boundary — a merge that let rows flow across one
    /// would produce a different (and wrong) chunking.
    #[test]
    fn part_boundaries_stay_batch_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        // Three parts, none a multiple of the batch size.
        for (i, rows) in [(0, 500i64), (1, 301), (2, 777)] {
            write_input(&dir.path().join(format!("p{i}.parquet")), rows, 120);
        }
        let source = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(source.parts().len(), 3, "fixture must resolve to 3 parts");
        let reference = read_all(&source, 64, 1, None);
        // Per part: full 64-row batches then a short tail.
        let expected: Vec<usize> = [500usize, 301, 777]
            .iter()
            .flat_map(|&n| {
                let mut v = vec![64; n / 64];
                if n % 64 != 0 {
                    v.push(n % 64);
                }
                v
            })
            .collect();
        assert_eq!(
            reference.0, expected,
            "the sequential reader must end each part on a short batch"
        );
        for workers in [2usize, 4] {
            assert_eq!(
                read_all(&source, 64, workers, None),
                reference,
                "--read-workers {workers} must not carry rows across a part boundary"
            );
        }
    }

    /// `--read-workers 1` is the reference path, not a one-worker instance of
    /// the parallel one: it must open a single stream and hand its batches
    /// straight through.
    #[test]
    fn one_worker_is_the_sequential_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.parquet");
        write_input(&path, 1000, 250);
        let source = ConvertSource::resolve(path.to_str().unwrap()).unwrap();
        let (counts, ids, offsets) = read_all(&source, 300, 1, None);
        assert_eq!(counts, vec![300, 300, 300, 100]);
        assert_eq!(offsets, vec![0, 300, 600, 900]);
        assert_eq!(ids, (0..1000i64).collect::<Vec<_>>());
    }

    /// A consumer that stops mid-stream must not hang: the workers are parked
    /// on full queues, and dropping the receivers is what releases them.
    #[test]
    fn a_stopping_consumer_releases_the_workers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("in.parquet");
        write_input(&path, 20_000, 500);
        let source = ConvertSource::resolve(path.to_str().unwrap()).unwrap();
        let (done_tx, done_rx) = crossbeam_channel::bounded::<usize>(1);
        std::thread::spawn(move || {
            let mut seen = 0usize;
            read_in_order(
                &source,
                None,
                None,
                ReadTuning {
                    budget: MemoryBudget::with_available(Some(256 << 30)),
                    batch_size: 64,
                    workers: 4,
                    avg_geom_bytes: Some(8),
                    footer: None,
                    profile: MemoryProfile::Auto,
                },
                |_batch, _offset, _dur| {
                    seen += 1;
                    Ok(if seen >= 3 {
                        ReadFlow::Stop
                    } else {
                        ReadFlow::Continue
                    })
                },
            )
            .unwrap();
            let _ = done_tx.send(seen);
        });
        let seen = done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("a stopping consumer must not deadlock its reader workers");
        assert_eq!(seen, 3);
    }

    /// The read-ahead sizing is what makes the parallelism real: a segment
    /// must fit in its worker's queue, and the worker count must fall rather
    /// than the per-worker queue going below the useful floor.
    #[test]
    fn read_shape_keeps_segments_inside_the_queue() {
        let shape = resolve_read_shape(ReadTuning {
            budget: MemoryBudget::with_available(Some(256 << 30)),
            batch_size: 8192,
            workers: 4,
            avg_geom_bytes: Some(64),
            footer: None,
            profile: MemoryProfile::Auto,
        });
        assert!(shape.workers >= 1 && shape.workers <= 4);
        assert!(shape.depth >= READ_AHEAD_MIN_BATCHES);
        assert!(shape.depth <= READ_AHEAD_MAX_BATCHES);
        assert!(
            shape.segment_target_rows <= (shape.depth - 1) * 8192,
            "a segment that cannot fit in its worker's queue makes the worker \
             park mid-segment, which serializes the reads again"
        );
    }

    /// A per-row estimate large enough that even one worker's floor queue
    /// exceeds the budget still yields a usable (single-worker) shape rather
    /// than zero workers or a zero-length segment.
    #[test]
    fn read_shape_degrades_to_one_worker_under_memory_pressure() {
        let shape = resolve_read_shape(ReadTuning {
            budget: MemoryBudget::with_available(Some(256 << 30)),
            batch_size: 8192,
            // 512 MiB per row: nothing is affordable.
            workers: 4,
            avg_geom_bytes: Some(512 * 1024 * 1024),
            footer: None,
            profile: MemoryProfile::Auto,
        });
        assert_eq!(shape.workers, 1, "an unaffordable read-ahead drops workers");
        assert!(shape.segment_target_rows >= 1);
    }

    #[test]
    fn read_ahead_prices_the_decoders_maximum_batch_allocation() {
        let shape = resolve_read_shape(ReadTuning {
            budget: MemoryBudget::with_available(Some(512 << 20)),
            batch_size: 100,
            workers: 8,
            avg_geom_bytes: Some(8),
            footer: None,
            profile: MemoryProfile::Bounded,
        });
        assert_eq!(
            shape.workers, 1,
            "tiny geometries do not price wide or dictionary-expanded properties"
        );
    }

    /// #716: footer bytes price a read batch at what it decodes to, capped at
    /// the reader's largest batch, and granting the read-ahead charges every
    /// queued batch to the run's budget, so the window sized after it sees
    /// what is left.
    #[test]
    fn the_read_ahead_is_priced_from_the_footers_and_charged() {
        let tuning = ReadTuning {
            budget: MemoryBudget::with_available(Some(2 << 30)),
            batch_size: 8192,
            workers: 4,
            avg_geom_bytes: Some(400),
            footer: Some(FooterPrice {
                row_bytes: 1_000,
                group_rows: 0,
            }),
            profile: MemoryProfile::Bounded,
        };
        let price = read_batch_price(tuning);
        assert_eq!((price.bytes, price.basis), (8_192_000, PriceBasis::Footer));
        let huge = read_batch_price(ReadTuning {
            footer: Some(FooterPrice {
                row_bytes: u64::MAX,
                group_rows: 0,
            }),
            ..tuning
        });
        assert_eq!(huge.bytes, tylertoo_io::decoded_batch_limit());

        let mut budget = tuning.budget;
        let before = budget.remaining();
        let shape = resolve_read_shape(tuning);
        let grant = grant_read_ahead(&mut budget, tuning, 3);
        assert_eq!(grant.units, shape.workers * shape.depth + 3);
        assert_eq!(budget.remaining(), before - grant.bytes());
        // The read layer resolves the same shape from the pre-grant snapshot.
        assert_eq!(resolve_read_shape(tuning), shape);
        // Without footer bytes, the model floored at a whole decoded batch.
        let model = read_batch_price(ReadTuning {
            footer: None,
            ..tuning
        });
        assert_eq!(model.basis, PriceBasis::Model);
        assert!(model.bytes >= tylertoo_io::decoded_batch_limit());
    }

    /// #716: each read worker is also charged the row group its reader
    /// decodes, so a large-row-group input under a tight allowance gets fewer
    /// workers, and the grant covers every worker's row group.
    #[test]
    fn each_read_worker_is_charged_its_readers_row_group() {
        let small = ReadTuning {
            budget: MemoryBudget::with_available(Some(2 << 30)),
            batch_size: 8192,
            workers: 4,
            avg_geom_bytes: Some(400),
            footer: Some(FooterPrice {
                row_bytes: 1_000,
                group_rows: 8192,
            }),
            profile: MemoryProfile::Auto,
        };
        // 0.10 × 0.6 × 2 GiB is 15 batches of 8,192,000 B: a one-batch row
        // group leaves 3 workers of 4 + 1, as unpriced (15 / 4).
        assert_eq!(reader_batches(small), 1);
        assert_eq!(resolve_read_shape(small).workers, 3);
        // A 65,536-row group is 8 batches per reader: one worker fits.
        let large = ReadTuning {
            footer: Some(FooterPrice {
                row_bytes: 1_000,
                group_rows: 65_536,
            }),
            ..small
        };
        assert_eq!(reader_batches(large), 8);
        let shape = resolve_read_shape(large);
        assert_eq!(shape.workers, 1);
        let mut budget = large.budget;
        let grant = grant_read_ahead(&mut budget, large, 3);
        assert_eq!(grant.units, shape.workers * (shape.depth + 8) + 3);
        // `bounded` keeps main's caps, which the budget can only shrink;
        // `speed` reads at its maximum whatever the budget affords (#788).
        let bounded = |workers, available| {
            resolve_read_shape(ReadTuning {
                budget: MemoryBudget::with_available(Some(available)),
                workers,
                profile: MemoryProfile::Bounded,
                ..small
            })
        };
        let rich = bounded(8, 64 << 30);
        assert_eq!(
            (rich.workers, rich.depth),
            (READ_WORKERS_BOUNDED_MAX, READ_AHEAD_BOUNDED_MAX_BATCHES)
        );
        let tight = bounded(8, 2 << 30);
        assert_eq!(
            tight,
            resolve_read_shape(ReadTuning {
                workers: 8,
                profile: MemoryProfile::Auto,
                ..small
            }),
            "under a tight budget bounded reads no wider than auto"
        );
        let speed = resolve_read_shape(ReadTuning {
            profile: MemoryProfile::Speed,
            ..large
        });
        assert_eq!((speed.workers, speed.depth), (4, READ_AHEAD_MAX_BATCHES));
        // Without footers there is nothing to price a reader with.
        assert_eq!(
            reader_batches(ReadTuning {
                footer: None,
                ..small
            }),
            0
        );
    }
}
