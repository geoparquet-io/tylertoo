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

/// FALLBACK estimated in-RAM bytes per buffered output ROW, by mode, used when
/// pass 1 could not measure the input's geometry sizes (empty input). `Auto`
/// multiplies the per-row estimate by `buffered_rows` (the count the pass-2
/// engine holds — every level but the streamed finest) to size the RAM-vs-spill
/// choice against available RAM.
///
/// A buffered row is a whole retained feature (geometry + properties as Arrow
/// arrays), NOT a coordinate — so the cost is dominated by geometry vertex
/// count. Measured pass-2 sink cost per buffered row (2026-07, RSS delta of the
/// pass2 sink phase ÷ `buffered_rows`): germany-segments lines ≈ 9.6 KiB,
/// fieldmaps-adm4 polygons ≈ 6.3 KiB, moldova polygons ≈ 16 KiB. An earlier
/// 256 B/row guess undercounted the real cost ~25× and let `auto` keep a
/// 3.4 GiB sink in RAM on a simulated 2 GiB box (issue #294's exact failure).
/// These constants are set to ~8 KiB (duplicating) / ~16 KiB (partitioning,
/// full-resolution geometry) — biased high so `auto` prefers the (near-free on
/// nvme) spill path over OOM. They steer only the backing choice, never output.
const DUPLICATING_BYTES_PER_ROW: u64 = 8_192;
const PARTITIONING_BYTES_PER_ROW: u64 = 16_384;

/// Measured-path per-row model (#305): `per_row = SINK_ROW_OVERHEAD_BYTES +
/// factor(mode) × avg_geom_bytes`, where `avg_geom_bytes` is pass 1's measured
/// average in-memory Arrow byte size of the encoded geometry column per input
/// row. Pass 1 already decodes every geometry, so the measurement is free (one
/// buffer-size sum per batch) and replaces the one-size-fits-all constants
/// above with the input's actual geometry weight — the dominant, wildly
/// dataset-dependent term (corpus range: ~30 B/row for points to ~11.5 KiB/row
/// for fieldmaps-adm4 boundary polygons).
///
/// Calibration (against the #294 RSS measurements above and corpus footers):
///
/// - `SINK_ROW_OVERHEAD_BYTES` (4 KiB) covers everything that is NOT input
///   geometry: property columns (≤ ~80 B/row on the measured corpora), Arrow
///   offsets/validity, and per-batch allocation slack — which dominates when
///   geometries are tiny.
/// - `DUPLICATING_GEOM_FACTOR` (×2): buffered duplicating rows hold
///   *simplified* copies (≤ input size, near-full-resolution only at the
///   finest buffered level), so ×2 over the encoded input size is the
///   deliberate high-bias margin (spills are near-free; OOM is not). Known
///   under-count: line coalescing (Q3) merges many input rows into fewer,
///   larger buffered rows — the per-row term misses the merge factor, but the
///   row COUNT shrinks far more (germany-segments: ~19 M input rows → ~123 K
///   buffered rows), so the product stays small in absolute terms.
/// - `PARTITIONING_GEOM_FACTOR` (×4): partitioning buffers full-resolution
///   geometry at every buffered level; keep the historical 2× ratio over
///   duplicating from the fallback constants.
///
/// Sanity vs the #294 measurements: fieldmaps-adm4 duplicating estimates
/// ~27 KiB/row vs 6.3 KiB measured (bias high — safe); moldova partitioning
/// ~8 KiB/row vs 16 KiB RSS-measured (RSS deltas overcount true need via
/// allocator slack; content is ~1 KiB/row). Like the fallback constants, the
/// model steers only the backing choice, never output bytes.
const SINK_ROW_OVERHEAD_BYTES: u64 = 4_096;
const DUPLICATING_GEOM_FACTOR: u64 = 2;
const PARTITIONING_GEOM_FACTOR: u64 = 4;

/// Fraction of *available* system RAM the estimated buffered-output set may
/// occupy before `Auto` spills to a temp file instead of holding it in RAM.
const AUTO_RAM_FRACTION: f64 = 0.6;

/// Budget used when available RAM cannot be probed (non-Linux, or
/// `/proc/meminfo` unreadable): a fixed, conservative ceiling so `Auto` still
/// spills very large buffered sets rather than assuming unlimited RAM.
const AUTO_FALLBACK_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Buffered-row count above which `Auto` always spills partitioning's
/// full-resolution geometry, regardless of the RAM estimate. Preserves the
/// pre-#294 partitioning safety floor as a strict lower bound (partitioning
/// spills at least as often as before; the RAM gate can only make it spill
/// sooner).
const PARTITIONING_SPILL_ROWS: usize = 2_000_000;

/// Estimated peak RAM (bytes) the pass-2 sink would hold if `buffered_rows`
/// output rows of `mode` were kept in RAM. `avg_geom_bytes` is pass 1's
/// measured average encoded-geometry size per input row (#305); `None` (or a
/// degenerate 0) falls back to the calibrated per-mode constants. Saturating,
/// so absurd counts never overflow (they simply clamp to a "definitely spill"
/// value).
fn estimate_buffered_bytes(mode: Mode, buffered_rows: usize, avg_geom_bytes: Option<u64>) -> u64 {
    let per_row = match avg_geom_bytes {
        Some(avg) if avg > 0 => {
            let factor = match mode {
                Mode::Duplicating => DUPLICATING_GEOM_FACTOR,
                Mode::Partitioning => PARTITIONING_GEOM_FACTOR,
            };
            SINK_ROW_OVERHEAD_BYTES.saturating_add(factor.saturating_mul(avg))
        }
        _ => match mode {
            Mode::Duplicating => DUPLICATING_BYTES_PER_ROW,
            Mode::Partitioning => PARTITIONING_BYTES_PER_ROW,
        },
    };
    (buffered_rows as u64).saturating_mul(per_row)
}

/// The RAM budget `Auto` compares the estimate against: a fraction of available
/// system RAM, or a fixed fallback when RAM cannot be determined.
fn auto_budget_bytes(available_ram_bytes: Option<u64>) -> u64 {
    match available_ram_bytes {
        Some(ram) => ((ram as f64) * AUTO_RAM_FRACTION) as u64,
        None => AUTO_FALLBACK_BUDGET_BYTES,
    }
}

/// Workload-driven backing choice for [`MemoryProfile::Auto`] (#294).
///
/// Chooses [`SinkBacking::Spill`] when the estimated buffered output exceeds a
/// fraction of available RAM — a function of `feature_count × level_count ×
/// mode` (captured by `buffered_rows` and the per-mode byte estimate), NOT a
/// mode-only rule. Partitioning additionally keeps its historical absolute
/// row ceiling, so it spills at least as often as before.
pub(super) fn auto_backing(
    mode: Mode,
    buffered_rows: usize,
    available_ram_bytes: Option<u64>,
    avg_geom_bytes: Option<u64>,
) -> SinkBacking {
    let estimate = estimate_buffered_bytes(mode, buffered_rows, avg_geom_bytes);
    let budget = auto_budget_bytes(available_ram_bytes);
    let ram_gate_spill = estimate > budget;
    let abs_gate_spill =
        matches!(mode, Mode::Partitioning) && buffered_rows > PARTITIONING_SPILL_ROWS;
    if ram_gate_spill || abs_gate_spill {
        SinkBacking::Spill
    } else {
        SinkBacking::Ram
    }
}

/// RAM budget for the pass-1 winner-grid waves (#306), by memory profile.
///
/// `bounded` and `auto` cap the concurrently-live per-level grids at the same
/// fraction-of-available-RAM budget the pass-2 sink decision uses (#294) —
/// honouring the `TYLERTOO_AUTO_MEM_LIMIT_BYTES` override and the conservative
/// fallback when RAM cannot be probed. `speed` opts out (unbounded, the
/// pre-#306 behavior): by contract it trades RAM for wall time, and any wave
/// split can serialize grid builds. The budget steers scheduling only — the
/// assignment is identical for every value.
pub(super) fn pass1_grid_budget_bytes(profile: MemoryProfile) -> u64 {
    match profile {
        MemoryProfile::Speed => u64::MAX,
        MemoryProfile::Bounded | MemoryProfile::Auto => auto_budget_bytes(available_memory_bytes()),
    }
}

/// Available system RAM in bytes for auto memory budgets — the convert-side
/// `Auto` profile (#294) and the export-side partition-wave preflight (#303)
/// share this probe so both honour the same override and fallback semantics.
///
/// Order of precedence:
/// 1. `TYLERTOO_AUTO_MEM_LIMIT_BYTES` env override (ops / testing knob — treat
///    the box as having this many bytes of available RAM);
/// 2. Linux `/proc/meminfo` `MemAvailable`;
/// 3. `None` (callers fall back to a fixed conservative budget — see
///    [`AUTO_FALLBACK_BUDGET_BYTES`] and
///    [`super::export::PARTITION_WAVE_FALLBACK_MAX`]).
pub(super) fn available_memory_bytes() -> Option<u64> {
    if let Ok(v) = std::env::var("TYLERTOO_AUTO_MEM_LIMIT_BYTES") {
        if let Ok(n) = v.trim().parse::<u64>() {
            return Some(n);
        }
    }
    read_proc_mem_available()
}

#[cfg(target_os = "linux")]
fn read_proc_mem_available() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        // Format: "MemAvailable:   12345678 kB"
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb.saturating_mul(1024));
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn read_proc_mem_available() -> Option<u64> {
    None
}

/// Resolve the [`MemoryProfile`] (including [`MemoryProfile::Auto`]) to a
/// concrete [`SinkBacking`], logging the decision.
///
/// `buffered_rows` is the total winner count of the levels the engine buffers
/// (all but the streamed finest level). For `Auto` the choice is workload-based
/// (#294): the estimated buffered output (`f(buffered_rows, mode)`) is compared
/// against a fraction of *available* RAM, so large duplicating runs prefer the
/// bounded spill path instead of the old unconditional RAM buffering. An
/// explicit profile always wins.
pub(super) fn resolve_backing(
    profile: MemoryProfile,
    mode: Mode,
    buffered_rows: usize,
    avg_geom_bytes: Option<u64>,
) -> SinkBacking {
    match profile {
        MemoryProfile::Speed => {
            log::debug!(
                "pass2 memory profile Speed + {mode:?} (buffered ~{buffered_rows} rows) → Ram"
            );
            SinkBacking::Ram
        }
        MemoryProfile::Bounded => {
            log::debug!(
                "pass2 memory profile Bounded + {mode:?} (buffered ~{buffered_rows} rows) → Spill"
            );
            SinkBacking::Spill
        }
        MemoryProfile::Auto => {
            let available = available_memory_bytes();
            let backing = auto_backing(mode, buffered_rows, available, avg_geom_bytes);
            let estimate = estimate_buffered_bytes(mode, buffered_rows, avg_geom_bytes);
            let budget = auto_budget_bytes(available);
            let avail_mib = available.map_or_else(
                || "unknown".to_string(),
                |b| format!("{} MiB", b / (1024 * 1024)),
            );
            let geom = avg_geom_bytes.filter(|&b| b > 0).map_or_else(
                || "unmeasured (calibrated constants)".to_string(),
                |b| format!("measured avg geom {b} B/row"),
            );
            // Info-level: the auto decision drives peak RAM and is the primary
            // diagnostic for #294 / #295 (paired with the [rss] phase logs).
            log::info!(
                "[convert] pass2 auto + {mode:?}: buffered ~{buffered_rows} rows, \
                 {geom}, est {} MiB vs budget {} MiB (avail {avail_mib}) → {backing:?}",
                estimate / (1024 * 1024),
                budget / (1024 * 1024),
            );
            backing
        }
    }
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
    // Absolute writer level index of this buffered set's first level (#332):
    // H3 aggregate levels are written directly by the driver as a coarse
    // prefix, so the buffered geom levels start at `base_level`, not 0.
    base_level: usize,
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
        outcomes.push(drain_sink(writer, base_level + li, hints[li], sink)?);
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

#[cfg(test)]
mod backing_tests {
    use super::*;
    use crate::overview::level::{MemoryProfile, Mode};

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn explicit_profiles_ignore_workload() {
        // Speed is always RAM, Bounded always Spill — regardless of size/mode.
        assert!(matches!(
            resolve_backing(
                MemoryProfile::Speed,
                Mode::Duplicating,
                10_000_000_000,
                None
            ),
            SinkBacking::Ram
        ));
        assert!(matches!(
            resolve_backing(MemoryProfile::Bounded, Mode::Duplicating, 1, None),
            SinkBacking::Spill
        ));
    }

    #[test]
    fn auto_duplicating_small_stays_in_ram() {
        // A tiny buffered set is safe in RAM on a normal box.
        assert!(matches!(
            auto_backing(Mode::Duplicating, 10_000, Some(54 * GIB), None),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn auto_duplicating_large_spills() {
        // The #294 fix: a large duplicating buffered set flips to Spill instead
        // of the old unconditional RAM. 10M rows * 8 KiB = 80 GiB > 0.6*54.
        assert!(matches!(
            auto_backing(Mode::Duplicating, 10_000_000, Some(54 * GIB), None),
            SinkBacking::Spill
        ));
    }

    #[test]
    fn auto_decision_scales_with_available_ram() {
        // Identical workload flips on available RAM, not a fixed fraction of it.
        let rows = 5_000_000; // 5M * 8 KiB = 40 GiB estimate
        assert!(
            matches!(
                auto_backing(Mode::Duplicating, rows, Some(4 * GIB), None),
                SinkBacking::Spill
            ),
            "small box must spill"
        );
        assert!(
            matches!(
                auto_backing(Mode::Duplicating, rows, Some(256 * GIB), None),
                SinkBacking::Ram
            ),
            "huge box may keep it in RAM"
        );
    }

    #[test]
    fn auto_partitioning_preserves_row_ceiling() {
        // Even with effectively unlimited RAM, partitioning still spills above
        // the historical 2M-row floor (strict superset of pre-#294 behavior).
        assert!(matches!(
            auto_backing(Mode::Partitioning, 3_000_000, Some(10_000 * GIB), None),
            SinkBacking::Spill
        ));
        assert!(matches!(
            auto_backing(Mode::Partitioning, 100_000, Some(54 * GIB), None),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn auto_uses_fallback_budget_when_ram_unknown() {
        // Unknown RAM → conservative fixed budget, still spilling huge sets.
        assert!(matches!(
            auto_backing(Mode::Duplicating, 100_000_000, None, None),
            SinkBacking::Spill
        ));
        assert!(matches!(
            auto_backing(Mode::Duplicating, 1_000, None, None),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn estimate_scales_with_mode_and_rows() {
        assert_eq!(
            estimate_buffered_bytes(Mode::Duplicating, 1_000, None),
            8_192_000
        );
        assert_eq!(
            estimate_buffered_bytes(Mode::Partitioning, 1_000, None),
            16_384_000
        );
        // Saturating: no overflow panic on absurd counts.
        assert_eq!(
            estimate_buffered_bytes(Mode::Duplicating, usize::MAX, None),
            u64::MAX
        );
    }

    // --- #305: pass-1 measured geometry sizes drive the per-row estimate. ---

    #[test]
    fn measured_estimate_is_overhead_plus_geometry_margin() {
        // per_row = 4 KiB overhead + 2× (duplicating) / 4× (partitioning) the
        // measured average encoded-geometry bytes.
        assert_eq!(
            estimate_buffered_bytes(Mode::Duplicating, 1_000, Some(1_000)),
            6_096_000 // (4096 + 2*1000) * 1000
        );
        assert_eq!(
            estimate_buffered_bytes(Mode::Partitioning, 1_000, Some(1_000)),
            8_096_000 // (4096 + 4*1000) * 1000
        );
        // Saturating with a measurement too.
        assert_eq!(
            estimate_buffered_bytes(Mode::Duplicating, usize::MAX, Some(1)),
            u64::MAX
        );
    }

    #[test]
    fn measured_zero_falls_back_to_constants() {
        // A degenerate measurement (0 bytes/row) is treated as unmeasured.
        assert_eq!(
            estimate_buffered_bytes(Mode::Duplicating, 1_000, Some(0)),
            8_192_000
        );
        assert_eq!(
            estimate_buffered_bytes(Mode::Partitioning, 1_000, Some(0)),
            16_384_000
        );
    }

    #[test]
    fn measured_tiny_geometry_keeps_ram_where_constant_spills() {
        // 5M duplicating rows on a 54 GiB box: the calibrated constant
        // (8 KiB/row → 38 GiB) exceeds the 32.4 GiB budget and spills, but a
        // measured tiny-geometry corpus (points/small lines, ~100 B/row →
        // ~4.3 KiB/row → 20 GiB) fits and stays in RAM.
        assert!(matches!(
            auto_backing(Mode::Duplicating, 5_000_000, Some(54 * GIB), None),
            SinkBacking::Spill
        ));
        assert!(matches!(
            auto_backing(Mode::Duplicating, 5_000_000, Some(54 * GIB), Some(100)),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn measured_huge_geometry_spills_where_constant_kept_ram() {
        // 1M duplicating rows, 25 GiB available (budget 15 GiB): the constant
        // (8 KiB/row → 7.6 GiB) keeps RAM, but an adm4-like measured average
        // (11 534 B/row → ~27 KiB/row → 25 GiB) correctly flips to Spill.
        assert!(matches!(
            auto_backing(Mode::Duplicating, 1_000_000, Some(25 * GIB), None),
            SinkBacking::Ram
        ));
        assert!(matches!(
            auto_backing(Mode::Duplicating, 1_000_000, Some(25 * GIB), Some(11_534)),
            SinkBacking::Spill
        ));
    }

    #[test]
    fn measured_partitioning_small_geometry_keeps_ram() {
        // 1M partitioning rows, 20 GiB available (budget 12 GiB): the constant
        // (16 KiB/row → 15.3 GiB) spills, but a moldova-like measured average
        // (983 B/row → ~8 KiB/row → 7.5 GiB) fits and stays in RAM. Below the
        // 2M-row ceiling, so only the RAM gate is in play.
        assert!(matches!(
            auto_backing(Mode::Partitioning, 1_000_000, Some(20 * GIB), None),
            SinkBacking::Spill
        ));
        assert!(matches!(
            auto_backing(Mode::Partitioning, 1_000_000, Some(20 * GIB), Some(983)),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn partitioning_row_ceiling_ignores_measurement() {
        // The historical 2M-row partitioning floor holds even when measurement
        // says the geometries are tiny.
        assert!(matches!(
            auto_backing(Mode::Partitioning, 3_000_000, Some(10_000 * GIB), Some(8)),
            SinkBacking::Spill
        ));
    }
}
