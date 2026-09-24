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
use crossbeam_channel::{Receiver, Sender};
use rayon::prelude::*;
use tempfile::NamedTempFile;

use crate::input_set::{ConvertSource, ReadPlan, RowGroupSelection};

use super::convert::ConvertError;
use super::level::{MemoryProfile, Mode};
use super::pipe::scoped_pipe;
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
/// share this probe so both honour the same override, container awareness and
/// fallback semantics.
///
/// Order of precedence:
/// 1. `TYLERTOO_AUTO_MEM_LIMIT_BYTES` env override (ops / testing knob — treat
///    the box as having this many bytes of available RAM);
/// 2. `min(cgroup memory headroom, /proc/meminfo MemAvailable)` — the cgroup
///    term is what makes the probe container-aware (#481): under Slurm, Docker
///    or k8s the machine figure can overstate the real budget by an order of
///    magnitude (a 160 GiB cgroup on a 2 TB node), which had `auto` pick
///    `speed` and OOM. "Headroom", not the bare limit: the decision happens
///    after pass 1 has already charged tens of GiB to the cgroup, so the
///    non-reclaimable part of the cgroup's current usage is subtracted — the
///    same sense as the machine term's `MemAvailable` (#485);
/// 3. `None` (callers fall back to a fixed conservative budget — see
///    [`AUTO_FALLBACK_BUDGET_BYTES`] and
///    [`super::export::PARTITION_WAVE_FALLBACK_MAX`]).
///
/// The result is computed once per process and cached: the probe is called ~5×
/// per run (convert's pass-1 grid budget and pass-2 backing decision, export's
/// wave preflight and its log line), every input is process-global, and a
/// cached figure also keeps the paired "decide" and "log/assert" calls from
/// disagreeing when `MemAvailable` drifts between them.
pub(super) fn available_memory_bytes() -> Option<u64> {
    static CACHED: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *CACHED.get_or_init(probe_available_memory_bytes)
}

/// The uncached probe behind [`available_memory_bytes`]. Split out so the unit
/// tests — which drive the readers against fixture trees — never depend on
/// (or poison) the process-wide cache.
fn probe_available_memory_bytes() -> Option<u64> {
    if let Ok(v) = std::env::var("TYLERTOO_AUTO_MEM_LIMIT_BYTES") {
        if let Ok(n) = v.trim().parse::<u64>() {
            return Some(n);
        }
    }
    let machine = read_proc_mem_available();
    let cgroup = cgroup_memory_limit_bytes();
    log_binding_cgroup_limit_once(machine, cgroup);
    effective_available(machine, cgroup)
}

/// Combine the machine-wide availability figure with the cgroup limit: the
/// budget is whichever is smaller, and either may be absent.
fn effective_available(machine: Option<u64>, cgroup: Option<u64>) -> Option<u64> {
    match (machine, cgroup) {
        (Some(m), Some(c)) => Some(m.min(c)),
        (only, None) | (None, only) => only,
    }
}

/// Emitted once per process, at info, when the cgroup limit — not the machine —
/// is what bounds the memory budget. Users on a shared cluster otherwise have
/// no way to see *why* the budget is a twelfth of the node's RAM.
fn log_binding_cgroup_limit_once(machine: Option<u64>, cgroup: Option<u64>) {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    let Some(limit) = cgroup else { return };
    if machine.is_some_and(|m| m <= limit) {
        return;
    }
    LOGGED.call_once(|| {
        let machine_str =
            machine.map_or_else(|| "unknown".to_string(), |m| format!("{:.1} GiB", gib(m)));
        log::info!(
            "memory budget from cgroup limit: {:.1} GiB (machine has {machine_str})",
            gib(limit),
        );
    });
}

fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// At or above this, a cgroup memory limit means "no limit". cgroup v1 writes a
/// sentinel (`u64::MAX` rounded down to a page multiple, typically
/// 9223372036854771712) instead of a word, and that is many orders of magnitude
/// past any real machine's RAM.
const CGROUP_UNLIMITED_SENTINEL: u64 = 1 << 60;

/// The memory limit (bytes) this process's cgroup imposes, if any.
///
/// Linux-only by construction: on other platforms the cgroup filesystem does
/// not exist, so the probe is skipped outright and the caller keeps the
/// machine figure (`None` on macOS, where `/proc/meminfo` is absent too).
fn cgroup_memory_limit_bytes() -> Option<u64> {
    if cfg!(target_os = "linux") {
        read_cgroup_memory_limit(std::path::Path::new("/"))
    } else {
        None
    }
}

/// Container-aware memory headroom under `root` (`/` in production; a tempdir
/// in tests). cgroup v2 first, then the v1 memory controller.
fn read_cgroup_memory_limit(root: &std::path::Path) -> Option<u64> {
    read_cgroup_v2_limit(root).or_else(|| read_cgroup_v1_limit(root))
}

/// cgroup v2: `/sys/fs/cgroup<path>/memory.{max,high}`, where `<path>` comes
/// from the `0::<path>` line of `/proc/self/cgroup`.
///
/// Both files bound the budget: `memory.max` is the hard limit (OOM kill),
/// `memory.high` the throttle ceiling systemd's `MemoryHigh=` and some Slurm
/// setups use — often with `memory.max` left at `max`, which made a
/// `memory.max`-only probe a no-op there (#485). The level's limit is the
/// smaller of the two.
fn read_cgroup_v2_limit(root: &std::path::Path) -> Option<u64> {
    let rel = proc_self_cgroup_path(root, CgroupSelector::V2).unwrap_or_default();
    let binding = min_limit_along_path(
        &root.join("sys/fs/cgroup"),
        &rel,
        &["memory.max", "memory.high"],
    )?;
    Some(subtract_unreclaimable_usage(
        binding.bytes,
        &binding.dir,
        "memory.current",
        &["inactive_file", "slab_reclaimable"],
    ))
}

/// cgroup v1: `/sys/fs/cgroup/memory<path>/memory.limit_in_bytes`, where
/// `<path>` comes from the `memory`-controller line of `/proc/self/cgroup`.
///
/// Approximation: the v1 memory controller is assumed to be mounted at
/// `/sys/fs/cgroup/memory`. Distros that co-mount controllers expose it at a
/// combined directory instead (`/sys/fs/cgroup/memory,hugetlb`), usually with a
/// `memory` symlink beside it — where the symlink is absent, the probe simply
/// finds nothing and abstains (the machine figure is then the only term), which
/// is the same fail-soft behavior as a missing file. Parsing `/proc/mounts` to
/// find the real mount point would remove the approximation; v1 is legacy
/// enough that it has not been worth the extra failure surface.
fn read_cgroup_v1_limit(root: &std::path::Path) -> Option<u64> {
    let rel = proc_self_cgroup_path(root, CgroupSelector::V1Memory).unwrap_or_default();
    let binding = min_limit_along_path(
        &root.join("sys/fs/cgroup/memory"),
        &rel,
        &["memory.limit_in_bytes"],
    )?;
    Some(subtract_unreclaimable_usage(
        binding.bytes,
        &binding.dir,
        "memory.usage_in_bytes",
        &["total_inactive_file"],
    ))
}

/// Turn a cgroup *limit* into the *headroom* left under it (#485).
///
/// The machine term of the probe is `MemAvailable` — what is still obtainable —
/// while a cgroup limit is a ceiling that the process may already be sitting
/// near: the `auto` decisions run after pass 1 has charged tens of GiB to the
/// cgroup, so the bare limit is systematically optimistic. Subtract the part of
/// current usage that cannot be reclaimed under pressure: current usage minus
/// the reclaimable page cache / slab reported by `memory.stat`.
///
/// Fail-soft: if either the usage file or `memory.stat` cannot be read, no
/// adjustment is made and the bare limit is returned — a probe must never turn
/// a missing file into a wrong (tiny) budget.
fn subtract_unreclaimable_usage(
    limit: u64,
    dir: &std::path::Path,
    usage_file: &str,
    reclaimable_keys: &[&str],
) -> u64 {
    let Some(usage) = read_u64_file(&dir.join(usage_file)) else {
        return limit;
    };
    let Some(reclaimable) = sum_stat_keys(&dir.join("memory.stat"), reclaimable_keys) else {
        return limit;
    };
    limit.saturating_sub(usage.saturating_sub(reclaimable))
}

/// A whole-file `u64` (`memory.current`, `memory.usage_in_bytes`). `None` when
/// the file is missing or does not hold a plain number.
fn read_u64_file(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Sum the named keys of a `key value`-per-line `memory.stat`. `None` only when
/// the file cannot be read; keys that are absent or unparseable contribute 0,
/// which biases the result toward a smaller (safer) headroom figure.
fn sum_stat_keys(path: &std::path::Path, keys: &[&str]) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut total: u64 = 0;
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(key), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        if keys.contains(&key) {
            total = total.saturating_add(value.parse().unwrap_or(0));
        }
    }
    Some(total)
}

/// Which `/proc/self/cgroup` line to pick.
#[derive(Clone, Copy)]
enum CgroupSelector {
    /// The unified-hierarchy line: `0::<path>` (empty controller list).
    V2,
    /// A v1 line whose controller list contains `memory`.
    V1Memory,
}

/// The cgroup path this process belongs to, per `/proc/self/cgroup`
/// (`hierarchy-ID:controller-list:path`). `None` when the file is missing or
/// carries no matching line.
fn proc_self_cgroup_path(root: &std::path::Path, want: CgroupSelector) -> Option<String> {
    let text = std::fs::read_to_string(root.join("proc/self/cgroup")).ok()?;
    text.lines().find_map(|line| {
        let mut parts = line.splitn(3, ':');
        let hierarchy = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        let selected = match want {
            CgroupSelector::V2 => hierarchy == "0" && controllers.is_empty(),
            CgroupSelector::V1Memory => controllers.split(',').any(|c| c == "memory"),
        };
        selected.then(|| path.to_string())
    })
}

/// The binding cgroup limit: its value and the directory that supplied it (the
/// usage/stat files of that same level are what the headroom adjustment reads).
struct BindingCgroupLimit {
    dir: std::path::PathBuf,
    bytes: u64,
}

/// The smallest limit written in any of `files` at `base` or any directory
/// along `rel`, with the directory it came from.
///
/// cgroup v2 enforces the **minimum** limit over the whole chain — an
/// ancestor's tighter `memory.max` binds a child that set none — so the
/// effective limit is the min over the path, not the leaf's value. `None` when
/// every level is unlimited, absent or unparseable, and also when `rel`
/// contains a `..` component: a path that would escape the mount root means the
/// probe cannot trust what it is reading, so it abstains rather than reporting
/// some other cgroup's limit.
fn min_limit_along_path(
    base: &std::path::Path,
    rel: &str,
    files: &[&str],
) -> Option<BindingCgroupLimit> {
    let mut dir = base.to_path_buf();
    let mut best = level_limit(&dir, files);
    for component in rel.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return None;
        }
        dir.push(component);
        best = match (best, level_limit(&dir, files)) {
            (Some(a), Some(b)) => Some(if b.bytes < a.bytes { b } else { a }),
            (only, None) | (None, only) => only,
        };
    }
    best
}

/// The limit one cgroup directory imposes: the smallest of `files` that holds a
/// real value (v2 has two — the hard `memory.max` and the throttle
/// `memory.high`), or `None` when the level sets none.
fn level_limit(dir: &std::path::Path, files: &[&str]) -> Option<BindingCgroupLimit> {
    files
        .iter()
        .filter_map(|file| parse_cgroup_limit(&dir.join(file)))
        .min()
        .map(|bytes| BindingCgroupLimit {
            dir: dir.to_path_buf(),
            bytes,
        })
}

/// One cgroup limit file: a byte count, or `None` for `max`, the v1 "unlimited"
/// sentinel, a missing file, or anything unparseable (never panics — a probe
/// must not be able to fail a conversion).
///
/// `0` is a real limit, not "unset": a cgroup with `memory.high` (or
/// `memory.max`) at 0 is throttled to nothing, and reporting it as such forces
/// the bounded path — the fail-safe direction.
fn parse_cgroup_limit(path: &std::path::Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: u64 = text.trim().parse().ok()?;
    (value < CGROUP_UNLIMITED_SENTINEL).then_some(value)
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
    // Boxed: arrow 59 grew `StreamWriter`, taking `SpillState` past clippy's
    // large_enum_variant threshold, so every Ram variant would otherwise carry
    // the spill variant's footprint.
    Spill(Box<SpillState>),
}

impl LevelSink {
    fn new(backing: SinkBacking, out_schema: &Schema) -> Result<Self, ConvertError> {
        Ok(match backing {
            SinkBacking::Ram => LevelSink::Ram(Vec::new()),
            SinkBacking::Spill => LevelSink::Spill(Box::new(SpillState::new(out_schema)?)),
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

    // Consumer state borrowed mutably by the consumer closure below.
    let (rows_ref, verts_ref, sinks_ref) = (&mut rows, &mut verts, &mut sinks);
    let timers_ref = &timers;
    let cascade = ctxs.first().is_some_and(|c| c.is_cascading_duplicating());
    scoped_pipe(
        in_flight,
        |tx: &Sender<ReadMsg>| -> Result<(), ConvertError> {
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
        },
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
        |rx: Receiver<ReadMsg>| -> Result<(), ConvertError> {
            // Heartbeat (#242): a planet-scale pass 2 runs for
            // minutes-to-hours; without this the phase is silent at info
            // level. Time-based so small inputs stay quiet.
            let mut last_progress = Instant::now();
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
        },
    )?;

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

/// Unit tests for the container-aware memory probe (#481).
///
/// Every reader takes a filesystem root so these build fake `/proc` + `/sys`
/// trees in a tempdir — the tests are therefore platform-independent and run
/// on macOS, where the real cgroup paths do not exist.
#[cfg(test)]
mod cgroup_tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// Write `contents` to `root/rel`, creating parent directories.
    fn write_file(root: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        path
    }

    /// A fake root with a cgroup-v2 `/proc/self/cgroup` line for `path`.
    fn v2_root(path: &str) -> TempDir {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "proc/self/cgroup", &format!("0::{path}\n"));
        dir
    }

    #[test]
    fn v2_leaf_limit_is_used() {
        let dir = v2_root("/slurm/job_42");
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/job_42/memory.max",
            "171798691840\n",
        );
        assert_eq!(
            read_cgroup_memory_limit(dir.path()),
            Some(160 * GIB),
            "a leaf memory.max must be reported as the cgroup limit"
        );
    }

    #[test]
    fn v2_ancestor_limit_binds_when_leaf_is_max() {
        let dir = v2_root("/slurm/job_42");
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/memory.max",
            "171798691840\n",
        );
        write_file(dir.path(), "sys/fs/cgroup/slurm/job_42/memory.max", "max\n");
        assert_eq!(
            read_cgroup_memory_limit(dir.path()),
            Some(160 * GIB),
            "an unlimited leaf must inherit the nearest limited ancestor"
        );
    }

    #[test]
    fn v2_takes_the_minimum_along_the_path() {
        let dir = v2_root("/slurm/job_42");
        // Ancestor is tighter than the leaf: cgroup v2 enforces the minimum.
        write_file(dir.path(), "sys/fs/cgroup/memory.max", "max\n");
        write_file(dir.path(), "sys/fs/cgroup/slurm/memory.max", "1073741824\n");
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/job_42/memory.max",
            "171798691840\n",
        );
        assert_eq!(
            read_cgroup_memory_limit(dir.path()),
            Some(GIB),
            "the tightest limit along the path must win"
        );
    }

    #[test]
    fn v2_all_max_is_unlimited() {
        let dir = v2_root("/slurm/job_42");
        write_file(dir.path(), "sys/fs/cgroup/memory.max", "max\n");
        write_file(dir.path(), "sys/fs/cgroup/slurm/memory.max", "max\n");
        write_file(dir.path(), "sys/fs/cgroup/slurm/job_42/memory.max", "max\n");
        assert_eq!(read_cgroup_memory_limit(dir.path()), None);
    }

    #[test]
    fn v2_root_cgroup_without_proc_file_still_reads_the_mount_root() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "sys/fs/cgroup/memory.max", "171798691840\n");
        assert_eq!(
            read_cgroup_memory_limit(dir.path()),
            Some(160 * GIB),
            "a missing /proc/self/cgroup must not hide the mount-root limit"
        );
    }

    #[test]
    fn v1_limit_is_used() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "proc/self/cgroup", "7:memory:/slurm/job_42\n");
        write_file(
            dir.path(),
            "sys/fs/cgroup/memory/slurm/job_42/memory.limit_in_bytes",
            "171798691840\n",
        );
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(160 * GIB));
    }

    #[test]
    fn v1_controller_root_limit_is_used_without_a_proc_path() {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            "171798691840",
        );
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(160 * GIB));
    }

    #[test]
    fn v1_unlimited_sentinel_is_none() {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            "9223372036854771712\n",
        );
        assert_eq!(
            read_cgroup_memory_limit(dir.path()),
            None,
            "the v1 'unlimited' sentinel must not be treated as a real limit"
        );
    }

    #[test]
    fn absent_files_are_none() {
        let dir = TempDir::new().unwrap();
        assert_eq!(read_cgroup_memory_limit(dir.path()), None);
    }

    // --- memory.high (#485, S2-2) ---------------------------------------

    #[test]
    fn v2_memory_high_binds_when_max_is_unlimited() {
        // systemd MemoryHigh= / some Slurm setups throttle via memory.high and
        // leave memory.max at "max" — a memory.max-only probe was a no-op there.
        let dir = v2_root("/user.slice/job");
        write_file(
            dir.path(),
            "sys/fs/cgroup/user.slice/job/memory.max",
            "max\n",
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/user.slice/job/memory.high",
            "171798691840\n",
        );
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(160 * GIB));
    }

    #[test]
    fn v2_takes_the_lower_of_max_and_high() {
        let dir = v2_root("/user.slice/job");
        write_file(
            dir.path(),
            "sys/fs/cgroup/user.slice/job/memory.max",
            "171798691840\n",
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/user.slice/job/memory.high",
            "1073741824\n",
        );
        assert_eq!(
            read_cgroup_memory_limit(dir.path()),
            Some(GIB),
            "the throttle ceiling binds below the hard limit"
        );
        // …and an ancestor's memory.high still binds a leaf that set neither.
        let dir = v2_root("/user.slice/job");
        write_file(
            dir.path(),
            "sys/fs/cgroup/user.slice/memory.high",
            "1073741824\n",
        );
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(GIB));
    }

    // --- headroom: limit minus non-reclaimable usage (#485, S2-1) -------

    #[test]
    fn v2_subtracts_non_reclaimable_current_usage() {
        let dir = v2_root("/slurm/job_42");
        let cg = "sys/fs/cgroup/slurm/job_42";
        write_file(dir.path(), &format!("{cg}/memory.max"), "171798691840\n");
        write_file(
            dir.path(),
            &format!("{cg}/memory.current"),
            &(100 * GIB).to_string(),
        );
        write_file(
            dir.path(),
            &format!("{cg}/memory.stat"),
            &format!(
                "anon {}\ninactive_file {}\nslab_reclaimable {}\nslab_unreclaimable {}\n",
                70 * GIB,
                20 * GIB,
                10 * GIB,
                GIB
            ),
        );
        // 160 GiB limit − (100 GiB used − 30 GiB reclaimable) = 90 GiB.
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(90 * GIB));
    }

    #[test]
    fn v2_usage_is_read_from_the_binding_level() {
        // The ancestor's limit binds, so the ancestor's usage is what counts —
        // reading the leaf's (smaller) usage would overstate the headroom.
        let dir = v2_root("/slurm/job_42");
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/memory.max",
            &(8 * GIB).to_string(),
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/memory.current",
            &(6 * GIB).to_string(),
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/memory.stat",
            "inactive_file 0\n",
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/job_42/memory.max",
            &(64 * GIB).to_string(),
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/job_42/memory.current",
            &(1024 * 1024).to_string(),
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/job_42/memory.stat",
            "inactive_file 0\n",
        );
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(2 * GIB));
    }

    #[test]
    fn v2_unreadable_usage_or_stat_leaves_the_limit_alone() {
        // memory.current present, memory.stat missing → no adjustment.
        let dir = v2_root("/slurm/job_42");
        let cg = "sys/fs/cgroup/slurm/job_42";
        write_file(dir.path(), &format!("{cg}/memory.max"), "171798691840\n");
        write_file(
            dir.path(),
            &format!("{cg}/memory.current"),
            &(100 * GIB).to_string(),
        );
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(160 * GIB));

        // memory.stat present, memory.current missing → no adjustment either.
        let dir = v2_root("/slurm/job_42");
        write_file(dir.path(), &format!("{cg}/memory.max"), "171798691840\n");
        write_file(
            dir.path(),
            &format!("{cg}/memory.stat"),
            "inactive_file 0\n",
        );
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(160 * GIB));
    }

    #[test]
    fn usage_above_the_limit_saturates_to_zero() {
        let dir = v2_root("/slurm/job_42");
        let cg = "sys/fs/cgroup/slurm/job_42";
        write_file(dir.path(), &format!("{cg}/memory.max"), &GIB.to_string());
        write_file(
            dir.path(),
            &format!("{cg}/memory.current"),
            &(4 * GIB).to_string(),
        );
        write_file(
            dir.path(),
            &format!("{cg}/memory.stat"),
            "inactive_file 0\n",
        );
        assert_eq!(
            read_cgroup_memory_limit(dir.path()),
            Some(0),
            "an over-budget cgroup reports no headroom, never an underflowed one"
        );
    }

    #[test]
    fn v1_subtracts_total_inactive_file() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "proc/self/cgroup", "7:memory:/slurm/job_42\n");
        let cg = "sys/fs/cgroup/memory/slurm/job_42";
        write_file(
            dir.path(),
            &format!("{cg}/memory.limit_in_bytes"),
            "171798691840\n",
        );
        write_file(
            dir.path(),
            &format!("{cg}/memory.usage_in_bytes"),
            &(100 * GIB).to_string(),
        );
        write_file(
            dir.path(),
            &format!("{cg}/memory.stat"),
            &format!("cache {}\ntotal_inactive_file {}\n", 40 * GIB, 30 * GIB),
        );
        // 160 GiB − (100 GiB − 30 GiB) = 90 GiB.
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(90 * GIB));
    }

    // --- hardening (#485, S3-2 / S3-3) ----------------------------------

    #[test]
    fn zero_limit_is_a_real_limit() {
        let dir = v2_root("/slurm/job_42");
        write_file(dir.path(), "sys/fs/cgroup/slurm/job_42/memory.high", "0\n");
        assert_eq!(
            read_cgroup_memory_limit(dir.path()),
            Some(0),
            "a zero limit must force the bounded path, not read as 'unlimited'"
        );
    }

    #[test]
    fn dot_dot_in_the_cgroup_path_makes_the_probe_abstain() {
        let dir = v2_root("/slurm/../other/job");
        write_file(
            dir.path(),
            "sys/fs/cgroup/memory.max",
            &(8 * GIB).to_string(),
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/other/job/memory.max",
            &(4 * GIB).to_string(),
        );
        assert_eq!(
            read_cgroup_memory_limit(dir.path()),
            None,
            "a path that could escape the mount root must not yield a limit"
        );
    }

    #[test]
    fn garbage_content_is_none_without_panicking() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "proc/self/cgroup", "not a cgroup file at all\n");
        write_file(
            dir.path(),
            "sys/fs/cgroup/memory.max",
            "\u{1f600} not a number",
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/memory/memory.limit_in_bytes",
            "-1",
        );
        assert_eq!(read_cgroup_memory_limit(dir.path()), None);
    }

    #[test]
    fn effective_available_takes_the_minimum() {
        // The #481 case: 2 TB machine, 160 GiB cgroup.
        assert_eq!(
            effective_available(Some(2048 * GIB), Some(160 * GIB)),
            Some(160 * GIB),
            "the cgroup limit must cap the machine figure"
        );
        assert_eq!(
            effective_available(Some(8 * GIB), Some(160 * GIB)),
            Some(8 * GIB)
        );
        assert_eq!(effective_available(Some(8 * GIB), None), Some(8 * GIB));
        assert_eq!(effective_available(None, Some(160 * GIB)), Some(160 * GIB));
        assert_eq!(effective_available(None, None), None);
    }
}
