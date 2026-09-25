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
//!   `bounded` — by a dedicated per-level writer thread, so the consumer hands
//!   the batch over instead of encoding it, #494), and after the read completes
//!   the sinks drain into the writer in level order (writers demand levels
//!   0,1,2… contiguously).
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

use crate::input_set::{ConvertSource, ReadPlan, ReadSegment, RowGroupSelection};

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

// ============================================================================
// Ordered input reading: one sequential reader, or several merged in order
// ============================================================================

/// What the caller wants from [`read_in_order`], and what it knows about the
/// rows it is about to read.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReadTuning {
    /// Rows per delivered batch. The merge reproduces this chunking exactly,
    /// however many workers actually read.
    pub(super) batch_size: usize,
    /// Reader threads asked for (already resolved; `1` = sequential).
    pub(super) workers: usize,
    /// Pass 1's measured average encoded-geometry bytes per input row (#305),
    /// used only to size the read-ahead against the memory budget. `None`
    /// falls back to [`READ_ROW_BYTES_FALLBACK`].
    pub(super) avg_geom_bytes: Option<u64>,
}

/// Whether [`read_in_order`]'s consumer wants more batches.
pub(super) enum ReadFlow {
    Continue,
    /// The consumer hung up (its own downstream is gone). Not an error: the
    /// real one is reported by whoever owns the downstream.
    Stop,
}

/// Share of the memory budget the pass-2 read-ahead may hold. Small on
/// purpose: the read-ahead competes with the pass-2 output sink, which is what
/// the budget is really for (#294), and a read that is `workers` × deeper than
/// before must not be what pushes a bounded run into swap.
const READ_BUDGET_FRACTION: f64 = 0.10;

/// Floor on a worker's read-ahead, in batches. Below this a worker cannot hold
/// even a small segment, so it stalls mid-segment and its read does not
/// overlap anything — at which point a worker is worse than no worker.
const READ_AHEAD_MIN_BATCHES: usize = 4;

/// Ceiling on a worker's read-ahead, in batches. A deeper queue only buys
/// tolerance for longer consumer stalls, and the cost is resident Arrow data.
const READ_AHEAD_MAX_BATCHES: usize = 32;

/// Assumed decoded bytes per input row when pass 1 measured nothing (an empty
/// scan, or the `--plan` path). Deliberately generous: over-estimating costs
/// reader workers, under-estimating costs RAM.
const READ_ROW_BYTES_FALLBACK: u64 = 1024;

/// A resolved parallel-read shape: how many workers, how deep each one's
/// queue is, and how large a segment may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReadShape {
    workers: usize,
    depth: usize,
    segment_target_rows: usize,
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
    // Pass 1 measures the ENCODED geometry; a decoded batch also carries the
    // property columns, offsets and validity, so double it for the estimate
    // (same high bias as the sink's per-row model, and for the same reason).
    let per_row = tuning
        .avg_geom_bytes
        .filter(|&b| b > 0)
        .map_or(READ_ROW_BYTES_FALLBACK, |b| b.saturating_mul(2))
        .max(1);
    let per_batch = (batch_size as u64).saturating_mul(per_row).max(1);
    let budget = (auto_budget_bytes(available_memory_bytes()) as f64 * READ_BUDGET_FRACTION) as u64;
    let affordable = (budget / per_batch).max(1) as usize;

    let mut workers = tuning.workers.max(1);
    while workers > 1 && affordable / workers < READ_AHEAD_MIN_BATCHES {
        workers -= 1;
    }
    let depth = (affordable / workers).clamp(READ_AHEAD_MIN_BATCHES, READ_AHEAD_MAX_BATCHES);
    ReadShape {
        workers,
        depth,
        segment_target_rows: depth.saturating_sub(1).max(1) * batch_size,
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

/// Deliver every selected input row to `on_batch`, in exact read order, in
/// batches of exactly `tuning.batch_size` rows (the last batch of each part
/// short) — reading with one sequential reader or with several concurrent
/// ones, identically either way.
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
/// **Gating.** Remote inputs read sequentially regardless of `workers`:
/// `SourceStream` releases a part's in-memory read cache when it finishes that
/// part, so concurrent readers over one remote source would evict each other's
/// fetched chunks (and the pass-0 staging that normally makes remote reads
/// local is per-part, not per-run). Local inputs — including a remote input
/// already staged to local disk — take the parallel path.
pub(super) fn read_in_order<F>(
    source: &ConvertSource,
    row_groups: Option<&RowGroupSelection>,
    tuning: ReadTuning,
    mut on_batch: F,
) -> Result<(), ConvertError>
where
    F: FnMut(RecordBatch, usize, Duration) -> Result<ReadFlow, ConvertError>,
{
    let batch_size = tuning.batch_size.max(1);
    let shape = resolve_read_shape(tuning);
    let parallel = shape.workers > 1 && !source.is_remote();
    if tuning.workers > 1 && source.is_remote() {
        log::info!(
            "[convert] pass 2: reading sequentially — parallel reads are local-only \
             (a remote source's parts share one chunk cache)"
        );
    }
    if !parallel {
        return read_sequentially(source, row_groups, batch_size, &mut on_batch);
    }
    let segments = source.read_segments(row_groups, shape.segment_target_rows)?;
    if segments.len() < 2 {
        return read_sequentially(source, row_groups, batch_size, &mut on_batch);
    }
    let workers = shape.workers.min(segments.len());
    log::info!(
        "[convert] pass 2 read: {workers} reader thread(s) over {} segment(s) \
         (~{} row(s) each, {} batch(es) in flight per reader)",
        segments.len(),
        shape.segment_target_rows,
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
                "[convert] pass 2 read: largest segment is {biggest} row(s) but a \
                 reader can buffer only {queue_rows} — reads will only partly \
                 overlap (the input's row groups are large relative to the \
                 read-ahead budget)"
            );
        }
    }
    read_in_parallel(
        source,
        &segments,
        batch_size,
        workers,
        shape.depth,
        &mut on_batch,
    )
}

/// The pre-#494 path, unchanged and still the reference: one reader, one
/// thread, batches straight through.
fn read_sequentially<F>(
    source: &ConvertSource,
    row_groups: Option<&RowGroupSelection>,
    batch_size: usize,
    on_batch: &mut F,
) -> Result<(), ConvertError>
where
    F: FnMut(RecordBatch, usize, Duration) -> Result<ReadFlow, ConvertError>,
{
    let mut reader = source.open_stream(&ReadPlan {
        batch_size,
        projection: None,
        row_groups,
    })?;
    let mut row_offset = 0usize;
    loop {
        let t_read = Instant::now();
        match reader.next() {
            None => return Ok(()),
            Some(Err(e)) => return Err(e.into()),
            Some(Ok(batch)) => {
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
fn read_in_parallel<F>(
    source: &ConvertSource,
    segments: &[ReadSegment],
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
            scope.spawn(move || {
                read_segments_for_worker(source, segments, batch_size, workers, w, &tx)
            });
        }

        let mut regroup = Regrouper::default();
        let mut row_offset = 0usize;
        let mut pending_read = Duration::ZERO;
        let mut current_part: Option<usize> = None;

        for (seq, segment) in segments.iter().enumerate() {
            // A part boundary is a batch boundary: the sequential reader opens
            // a fresh reader per part, so a part's last batch is short.
            if current_part != Some(segment.part) {
                if let Some(tail) = regroup.take_all()? {
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
                        while regroup.rows >= batch_size {
                            let out = regroup.take(batch_size)?;
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
        if let Some(tail) = regroup.take_all()? {
            deliver(tail, &mut row_offset, &mut pending_read, on_batch)?;
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
    batch_size: usize,
    workers: usize,
    w: usize,
    tx: &Sender<SegMsg>,
) {
    let mut seq = w;
    while seq < segments.len() {
        let mut stream = match source.open_segment(&segments[seq], batch_size, None) {
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
/// runs of an exact row count.
///
/// Batches that already line up cost nothing — [`Regrouper::take`] returns the
/// queued batch itself when it holds exactly the requested rows — so in the
/// common case (row groups an exact multiple of the batch size, or a seam that
/// happens to land on one) nothing is copied at all. Only a run spanning a
/// worker seam is concatenated.
#[derive(Default)]
struct Regrouper {
    queue: std::collections::VecDeque<RecordBatch>,
    rows: usize,
}

impl Regrouper {
    fn push(&mut self, batch: RecordBatch) {
        if batch.num_rows() > 0 {
            self.rows += batch.num_rows();
            self.queue.push_back(batch);
        }
    }

    /// Remove and return exactly `n` rows from the front. `n` must be `<=
    /// self.rows`.
    fn take(&mut self, n: usize) -> Result<RecordBatch, ConvertError> {
        debug_assert!(n > 0 && n <= self.rows);
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

    /// Remove and return everything queued, or `None` when empty.
    fn take_all(&mut self) -> Result<Option<RecordBatch>, ConvertError> {
        if self.rows == 0 {
            return Ok(None);
        }
        self.take(self.rows).map(Some)
    }
}

/// The ordered parallel reader (#494): that `--read-workers N` delivers
/// exactly what one reader delivers, whatever N is and however unevenly the
/// workers are paced.
#[cfg(test)]
mod read_tests {
    use super::*;
    use arrow_array::{ArrayRef, Int64Array};
    use arrow_schema::{DataType, Field};
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;
    use std::sync::Arc;

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
            ReadTuning {
                batch_size,
                workers,
                // Tiny rows: the read-ahead sizing must not silently collapse
                // the worker count in these fixtures.
                avg_geom_bytes: Some(8),
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
            batch_size: 128,
            workers: 4,
            avg_geom_bytes: Some(8),
        });
        let segments = source
            .read_segments(None, shape.segment_target_rows)
            .unwrap();
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
                ReadTuning {
                    batch_size: 64,
                    workers: 4,
                    avg_geom_bytes: Some(8),
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
            batch_size: 8192,
            workers: 4,
            avg_geom_bytes: Some(64),
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
            batch_size: 8192,
            // 512 MiB per row: nothing is affordable.
            workers: 4,
            avg_geom_bytes: Some(512 * 1024 * 1024),
        });
        assert_eq!(shape.workers, 1, "an unaffordable read-ahead drops workers");
        assert!(shape.segment_target_rows >= 1);
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
    // Not boxed: `SpillState` used to own the arrow `StreamWriter` inline,
    // which (arrow 59) pushed the variant past clippy's large_enum_variant
    // threshold. The writer now lives on the spill thread, leaving a sender
    // and a join handle here.
    Spill(SpillState),
}

impl LevelSink {
    fn new(backing: SinkBacking, out_schema: &Schema) -> Result<Self, ConvertError> {
        Ok(match backing {
            SinkBacking::Ram => LevelSink::Ram(Vec::new()),
            SinkBacking::Spill => LevelSink::Spill(SpillState::new(out_schema)?),
        })
    }

    /// Push one output batch into the sink. Returns the bytes handed to the
    /// spill writer (0 for the RAM path — nothing is measured there, since
    /// the goal is accounting for previously-invisible spill I/O).
    fn push(&mut self, batch: RecordBatch) -> Result<u64, ConvertError> {
        match self {
            LevelSink::Ram(v) => {
                v.push(batch);
                Ok(0)
            }
            LevelSink::Spill(s) => s.push(batch),
        }
    }
}

/// An invariant this module owns was broken — never reachable from user input,
/// so it carries no advice, only the broken invariant.
fn internal(what: &str) -> ConvertError {
    ConvertError::Io(std::io::Error::other(format!("internal: {what}")))
}

/// Batches a level's spill-writer thread may hold ahead of the consumer.
///
/// The queue exists to absorb write jitter (an fsync-adjacent stall, a
/// compaction pause on the temp filesystem), not to buffer a level's output:
/// depth 2 is one batch being encoded plus one waiting, which is enough to
/// keep the writer busy while the consumer builds the next batch, and it caps
/// the extra resident set at `2 × out_batch × spilled_levels` — a bounded
/// profile spills EVERY buffered level, so a deeper queue multiplies by the
/// level count and would undo what the bounded profile is for.
const SPILL_QUEUE_DEPTH: usize = 2;

/// A level spilled to a temporary Arrow IPC stream file, written by a
/// dedicated thread. The write handle and the read handle are independent
/// `reopen()`s of the same temp file; Arrow IPC is a lossless value round-trip,
/// so the reloaded batches are identical to the buffered ones and the final
/// Parquet encode stays byte-identical.
///
/// **Why a thread (#494).** The Arrow IPC encode+write used to run inline in
/// [`LevelSink::push`], i.e. on the single consumer thread that also drives
/// every level's per-batch compute — 65.7 core-seconds of a 403-second bounded
/// Brazil-55M pass 2, all of it blocking the one thread the whole pipeline is
/// serialized through. Handing each level's batches to its own writer over a
/// bounded channel takes that off the critical path without changing a byte:
/// one sender, one receiver, FIFO, so the level's batches reach the file in
/// exactly the order the consumer produced them.
struct SpillState {
    /// `None` once the stream has been closed — by [`SpillState::into_reader`],
    /// or by a `push` that joined the writer to report its error.
    tx: Option<Sender<RecordBatch>>,
    /// `None` once joined. The writer returns the temp file it finished plus
    /// the core-seconds it spent encoding and writing (folded into
    /// `Pass2Timers::spill_write` at join time, since the thread cannot borrow
    /// the driver's timers).
    handle: Option<std::thread::JoinHandle<Result<SpillWriterDone, ConvertError>>>,
}

/// What a finished spill-writer thread hands back.
struct SpillWriterDone {
    temp: NamedTempFile,
    /// Core-seconds spent in Arrow IPC encode + write on the writer thread.
    write_time: Duration,
}

impl SpillState {
    fn new(out_schema: &Schema) -> Result<Self, ConvertError> {
        let temp = NamedTempFile::new()?;
        let write_handle = temp.reopen()?;
        // Constructed on the caller's thread so a broken spill directory or an
        // unwritable temp file fails the conversion here, with the caller's
        // error handling, rather than inside a thread nobody has joined yet.
        let writer = StreamWriter::try_new(BufWriter::new(write_handle), out_schema)?;
        Self::spawn(writer, temp)
    }

    /// Start the writer thread for an already-opened IPC stream.
    ///
    /// Generic over the stream's sink for one reason: a spill write fails only
    /// on I/O the production path cannot provoke on demand (a full disk, a
    /// spill directory yanked mid-run), so the failure-surfacing contract is
    /// untestable without being able to hand the writer a sink that fails.
    /// Production monomorphizes this exactly once, over `BufWriter<File>`.
    fn spawn<W: std::io::Write + Send + 'static>(
        mut writer: StreamWriter<W>,
        temp: NamedTempFile,
    ) -> Result<Self, ConvertError> {
        let (tx, rx) = crossbeam_channel::bounded::<RecordBatch>(SPILL_QUEUE_DEPTH);
        let handle = std::thread::Builder::new()
            .name("tylertoo-spill".to_string())
            .spawn(move || -> Result<SpillWriterDone, ConvertError> {
                let mut write_time = Duration::ZERO;
                for batch in rx.iter() {
                    let t = Instant::now();
                    writer.write(&batch)?;
                    write_time += t.elapsed();
                }
                let t = Instant::now();
                writer.finish()?; // writes EOS + flushes the BufWriter
                drop(writer); // close the write handle
                write_time += t.elapsed();
                Ok(SpillWriterDone { temp, write_time })
            })?;
        Ok(SpillState {
            tx: Some(tx),
            handle: Some(handle),
        })
    }

    /// Hand one batch to this level's writer thread, reporting its approximate
    /// in-memory byte size. Blocks only when the writer is [`SPILL_QUEUE_DEPTH`]
    /// batches behind.
    ///
    /// A closed channel means the writer thread already returned — necessarily
    /// with an error, since it only stops early on one — so the send failure is
    /// resolved by joining and reporting that error rather than by inventing a
    /// "channel closed" of its own (the #486 surfacing rule: report the cause,
    /// never the symptom).
    fn push(&mut self, batch: RecordBatch) -> Result<u64, ConvertError> {
        let bytes = batch.get_array_memory_size() as u64;
        let Some(tx) = &self.tx else {
            return Err(internal("spill sink pushed after it was closed"));
        };
        if tx.send(batch).is_err() {
            self.tx = None; // release the writer's receiver before joining
                            // The writer stops early only on an error, so `?` here IS the
                            // report; falling through means it finished cleanly with the
                            // stream still open, which it cannot do.
            self.join()?;
            return Err(internal("spill writer stopped without reporting an error"));
        }
        Ok(bytes)
    }

    /// Join the writer thread, resuming its panic on this thread rather than
    /// flattening it into an error. `Ok(None)` when it was already joined.
    fn join(&mut self) -> Result<Option<SpillWriterDone>, ConvertError> {
        let Some(handle) = self.handle.take() else {
            return Ok(None);
        };
        match handle.join() {
            Ok(res) => res.map(Some),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Close the stream, join the writer, and reopen the temp file for reading,
    /// folding the writer's core-seconds into `timers`. The returned
    /// [`NamedTempFile`] must be held until the reader is exhausted so the file
    /// is not unlinked mid-read (and is cleaned up on drop afterwards).
    fn into_reader(
        mut self,
        timers: &Pass2Timers,
    ) -> Result<(StreamReader<BufReader<File>>, NamedTempFile), ConvertError> {
        // Dropping the sender is what ends the writer's `rx.iter()`; without it
        // the join below would never return.
        self.tx = None;
        let done = self
            .join()?
            .ok_or_else(|| internal("spill writer joined twice"))?;
        Pass2Timers::add_dur(timers.spill_write_cell(), done.write_time);
        let read_handle = done.temp.reopen()?;
        let reader = StreamReader::try_new(BufReader::new(read_handle), None)?;
        Ok((reader, done.temp))
    }
}

impl Drop for SpillState {
    /// A sink abandoned on an error path (or an unwind) must not leave its
    /// writer thread parked on a receiver that never disconnects: drop the
    /// sender first, then join. Failures are already lost on this path — the
    /// error that got us here is the one worth reporting — but a panic inside
    /// the writer is re-raised only when we are not already unwinding, since
    /// panicking in a `Drop` during an unwind aborts the process.
    fn drop(&mut self) {
        self.tx = None;
        let Some(handle) = self.handle.take() else {
            return;
        };
        if let Err(payload) = handle.join() {
            if !std::thread::panicking() {
                std::panic::resume_unwind(payload);
            }
        }
    }
}

/// Per-level `(outcome, rows_written, vertex_count, spill_bytes_written)` plus
/// the engine's aggregated [`Pass2Timers`], returned by [`run_pass2_buffered`]
/// ([profile] / `TYLERTOO_PROFILE_JSON` instrumentation — the measurement base
/// for the pass-2 throughput work). `spill_bytes_written` is 0 under
/// [`SinkBacking::Ram`], since nothing is written to disk there.
pub(super) struct Pass2EngineResult {
    pub(super) levels: Vec<(LevelWriteOutcome, usize, usize, u64)>,
    pub(super) timers: Pass2Timers,
}

/// Buffer + write levels `0..ctxs.len()` (all but the streamed finest level)
/// from a single read of the input. Returns per-level `(outcome,
/// rows_written, vertex_count, spill_bytes_written)`, in level order — the
/// outcome flags a level the writer skipped because every candidate collapsed
/// during simplification (#211) — plus the engine's stage-timer snapshot.
#[allow(clippy::too_many_arguments)]
pub(super) fn run_pass2_buffered(
    writer: &mut OverviewWriter<File>,
    ctxs: &[LevelStreamCtx<'_>],
    hints: &[usize],
    source: &ConvertSource,
    read_tuning: ReadTuning,
    selected_row_groups: Option<&RowGroupSelection>,
    in_flight: usize,
    backing: SinkBacking,
    out_schema: &Schema,
) -> Result<Pass2EngineResult, ConvertError> {
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
    // Bytes handed to the spill writer, per level (previously uncounted).
    // Stays all-zero under `SinkBacking::Ram`.
    let mut spill_bytes = vec![0u64; num_levels];

    // Consumer state borrowed mutably by the consumer closure below.
    let (rows_ref, verts_ref, sinks_ref, spill_bytes_ref) =
        (&mut rows, &mut verts, &mut sinks, &mut spill_bytes);
    let timers_ref = &timers;
    let cascade = ctxs.first().is_some_and(|c| c.is_cascading_duplicating());
    scoped_pipe(
        in_flight,
        // Producer: the input, in read order, over the per-part bbox-selected
        // row groups (#102 — the same selection both passes use, so global row
        // indices stay aligned). `read_in_order` is either one synchronous
        // Parquet reader on this thread (the reference path) or several
        // concurrent ones merged back into the identical batch sequence
        // (#494); either way the reader lives off the rayon pool, so reads are
        // never head-of-line blocked behind compute work.
        |tx: &Sender<ReadMsg>| -> Result<(), ConvertError> {
            read_in_order(
                source,
                selected_row_groups,
                read_tuning,
                |batch, row_offset, read_dur| {
                    Ok(
                        if tx
                            .send(ReadMsg {
                                row_offset,
                                batch,
                                read_dur,
                            })
                            .is_err()
                        {
                            ReadFlow::Stop // consumer dropped the receiver (error path)
                        } else {
                            ReadFlow::Continue
                        },
                    )
                },
            )
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
                        spill_bytes_ref[li] += sinks_ref[li].push(out)?;
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
        let t_drain_level = Instant::now();
        let outcome = drain_sink(writer, li, hints[li], sink, &timers)?;
        log::debug!(
            "[profile] level {li}: rows={} spill={:.2} MiB drain={:.2}s",
            rows[li],
            spill_bytes[li] as f64 / (1024.0 * 1024.0),
            t_drain_level.elapsed().as_secs_f64(),
        );
        outcomes.push(outcome);
    }

    timers.log_engine_summary(t_engine.elapsed().as_secs_f64(), rows.iter().sum());
    let levels = outcomes
        .into_iter()
        .zip(rows)
        .zip(verts)
        .zip(spill_bytes)
        .map(|(((outcome, r), v), s)| (outcome, r, v, s))
        .collect();
    Ok(Pass2EngineResult { levels, timers })
}

/// Drain one level's sink into `writer.write_level`, timing the call into
/// `timers.drain` (previously invisible: this runs serially, one level at a
/// time, after the parallel read loop finishes). The RAM path is infallible;
/// the spill path reuses the error-parking discipline of
/// `write_level_streaming` because `write_level` consumes an infallible
/// iterator but Arrow IPC read-back can fail.
fn drain_sink(
    writer: &mut OverviewWriter<File>,
    level_idx: usize,
    hint: usize,
    sink: LevelSink,
    timers: &Pass2Timers,
) -> Result<LevelWriteOutcome, ConvertError> {
    let t_drain = Instant::now();
    let outcome = match sink {
        LevelSink::Ram(batches) => {
            writer.write_level(level_idx, Some(hint), batches.into_iter())?
        }
        LevelSink::Spill(state) => {
            // `_temp` keeps the spill file on disk until the reader is drained.
            // Joining the writer here also folds its encode+write core-seconds
            // into `timers.spill_write`, so the stage split still accounts for
            // the I/O now that it no longer runs on the consumer thread.
            let (mut reader, _temp) = state.into_reader(timers)?;
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
            res?
        }
    };
    Pass2Timers::add_dur(timers.drain_cell(), t_drain.elapsed());
    Ok(outcome)
}

/// The off-thread spill writer (#494): order, accounting, and how a failing
/// or abandoned writer surfaces.
#[cfg(test)]
mod spill_tests {
    use super::*;
    use arrow_array::{ArrayRef, Int64Array};
    use arrow_schema::{DataType, Field};
    use std::sync::Arc;

    fn schema(name: &str) -> Schema {
        Schema::new(vec![Field::new(name, DataType::Int64, false)])
    }

    /// One batch holding `ids`, against `schema`.
    fn batch(schema: &Schema, ids: Vec<i64>) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![Arc::new(Int64Array::from(ids)) as ArrayRef],
        )
        .unwrap()
    }

    /// The load-bearing property: a level's batches must come back out of the
    /// spill file in exactly the order they were pushed. The writer runs on
    /// its own thread now, so "the consumer wrote it" and "the file has it"
    /// are no longer the same moment — but one sender over a FIFO channel to
    /// one writer keeps the sequence, and `into_reader` joins before reading.
    #[test]
    fn spill_writer_preserves_push_order() {
        let s = schema("id");
        let mut sink = SpillState::new(&s).unwrap();
        // Comfortably more than SPILL_QUEUE_DEPTH, so the consumer really does
        // block on a full queue and the two threads interleave.
        let pushed: Vec<Vec<i64>> = (0..64i64).map(|i| vec![i * 10, i * 10 + 1]).collect();
        let mut bytes = 0u64;
        for ids in &pushed {
            bytes += sink.push(batch(&s, ids.clone())).unwrap();
        }
        let timers = Pass2Timers::default();
        let (reader, _temp) = sink.into_reader(&timers).unwrap();
        let got: Vec<Vec<i64>> = reader
            .map(|b| {
                let b = b.unwrap();
                let col = b
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .unwrap()
                    .values()
                    .to_vec();
                col
            })
            .collect();
        assert_eq!(got, pushed, "spill read-back must match push order exactly");
        assert!(
            bytes > 0,
            "push must report the in-memory bytes it handed on"
        );
        assert!(
            timers.stage_secs().spill_write > 0.0,
            "the writer thread's encode+write core-seconds must be folded back \
             into the stage split at join time — otherwise moving the I/O off \
             the consumer thread would simply make it invisible again"
        );
    }

    /// A sink that accepts the IPC header and then fails — the shape of a
    /// spill filesystem that fills up mid-level.
    struct FailsAfter {
        remaining: usize,
    }

    impl std::io::Write for FailsAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("spill device is out of space"));
            }
            let n = buf.len().min(self.remaining);
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer that fails mid-stream must surface ITS error, not a derived
    /// "channel closed" (#486): a failed `send` only means the thread already
    /// returned, so the sink joins it and reports what it returned. Before the
    /// writer moved off the consumer thread this was simply `push`'s own `?`;
    /// the thread must not have made the failure quieter.
    #[test]
    fn spill_writer_error_surfaces_as_the_writers_own_error() {
        let s = schema("id");
        // Enough budget for the IPC schema message, not for the batches.
        let writer = StreamWriter::try_new(FailsAfter { remaining: 512 }, &s).unwrap();
        let mut sink = SpillState::spawn(writer, NamedTempFile::new().unwrap()).unwrap();
        // The queue absorbs the first pushes, so the error lands on a later
        // push or at join — either way it must be the writer's own.
        let mut err = None;
        for _ in 0..(SPILL_QUEUE_DEPTH + 16) {
            if let Err(e) = sink.push(batch(&s, vec![1, 2, 3])) {
                err = Some(e);
                break;
            }
        }
        let err = match err {
            Some(e) => e,
            None => sink
                .into_reader(&Pass2Timers::default())
                .expect_err("a writer whose sink failed must fail the sink"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("out of space"),
            "the writer's own I/O error must be what surfaces, got: {msg}"
        );
        assert!(
            !msg.contains("internal:"),
            "an internal placeholder must never stand in for the writer's own \
             error, got: {msg}"
        );
    }

    /// A sink abandoned without `into_reader` (an error path, or an unwind)
    /// must still join its writer rather than leaving a thread parked on a
    /// receiver that never disconnects. The test body is the assertion: a
    /// `Drop` that forgot to drop the sender first would hang here.
    #[test]
    fn dropping_a_sink_joins_its_writer() {
        let s = schema("id");
        let mut sink = SpillState::new(&s).unwrap();
        for i in 0..8i64 {
            sink.push(batch(&s, vec![i])).unwrap();
        }
        drop(sink);
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
