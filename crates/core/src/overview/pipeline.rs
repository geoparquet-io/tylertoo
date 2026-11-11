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
//! - the read side streams the input in order over a bounded channel, tagging
//!   each batch with its cumulative `row_offset`. It is one reader thread, or
//!   since #494 several reader workers (auto-sized up to
//!   [`super::convert::READ_WORKERS_MAX`]) whose segments
//!   [`super::read::read_in_order`] merges back into the identical batch
//!   sequence;
//! - the consumer takes a **window** of up to half of `in_flight` batches at a
//!   time ([`super::pipe::for_each_window`], #494; the channel gets the other
//!   half, see [`super::pipe::split_in_flight`]), processes them concurrently,
//!   and pushes their outputs in read order, so no reorder buffer is needed.
//!   Each batch also parallelizes within itself. On the default
//!   cascading-duplicating path each rayon task is one **feature**, folded
//!   fine→coarse through every level in series ([`process_batch_cascade`]).
//!   With the cascade off, or in partitioning mode, every `(level × feature)`
//!   simplification is its own rayon task, so the giant-geometry long pole at
//!   one level overlaps the other levels' work. The window is what overlaps the
//!   rest: one batch's serial decode, its per-level build and its slowest
//!   feature no longer hold up the batches behind it;
//! - each level's finished output batches accumulate in an ordered **sink**
//!   (RAM under the `speed` profile, spilled to a temporary Arrow IPC file under
//!   `bounded` — by a dedicated per-level writer thread, so the consumer hands
//!   the batch over instead of encoding it, #494), and after the read completes
//!   the sinks drain into the writer in level order (writers demand levels
//!   0,1,2… contiguously).
//!
//! Output is **byte-identical** to the serial path: within a batch the ascending
//! `selected` order and `process_level_batch`'s order-preserving `par_iter` are
//! unchanged, each batch is processed independently of the others in its
//! window, and batches reach every sink in ascending read order, so each
//! level's row sequence — and therefore its row-group boundaries — match exactly.
//!
//! The finest/canonical level is **not** handled here: it is verbatim, the
//! largest level, and written last, so [`super::stream`] streams it directly
//! into the writer on a second read rather than buffering it
//! ("canonical-streamed-last").

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::time::{Duration, Instant};

#[expect(
    clippy::disallowed_types,
    reason = "#817 B1, #852: pass 2's per-call measurement and sharing flag"
)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

use arrow_array::{Array, ArrayRef, RecordBatch, UInt32Array};
use arrow_schema::{Schema, SchemaRef};
use crossbeam_channel::{Receiver, Sender};
use rayon::prelude::*;
use std::path::Path;
use tempfile::NamedTempFile;
use tylertoo_io::IpcCompression;
use tylertoo_io::IpcStreamReader as StreamReader;
use tylertoo_io::IpcStreamWriter as StreamWriter;

use super::memory::{Grant, MemoryBudget, SINK_ROW_ALLOC_OVERHEAD_BYTES};

use super::convert::ConvertError;
use super::level::{MemoryProfile, Mode};
use super::pipe::{for_each_window, scoped_pipe, split_in_flight};
use super::read::{internal, ReadFlow};
use super::stream::{
    process_batch_cascade, process_level_batch, LevelStreamCtx, Pass2Timers, StreamRead,
};
use super::writer::{LevelWriteOutcome, OverviewWriter};

pub(super) struct BufferOptions<'a> {
    pub(super) profile: MemoryProfile,
    pub(super) backing: SinkBacking,
    pub(super) out_schema: &'a Schema,
    pub(super) spill_dir: Option<&'a Path>,
    /// The writer's encode grant, what one more drain encode holds, and the
    /// read's granted bytes that cap what the drain reuses
    /// ([`MemoryBudget::drain_encodes`]).
    pub(super) drain: (Grant, u64, u64),
    /// The run's budget after this read's grants: what the sink tuner arms
    /// against (#791 item 3), not the copy the read was sized from.
    pub(super) budget: MemoryBudget,
}

/// How a level's buffered output is held until it is written.
#[derive(Clone, Copy, Debug)]
pub(super) enum SinkBacking {
    /// Buffer output batches in RAM (`speed` profile).
    Ram,
    /// Spill output batches to a temporary Arrow IPC file (`bounded` profile).
    Spill,
}

/// The pass-2 spill codec for `profile`. `bounded` spills LZ4 (#715), which
/// writes 2.5-3.9x fewer bytes for +2-12% instructions (#790). `auto` and
/// `speed` spill raw: LZ4 cost an `auto` spill 6-9 s of pass 2 on NZ parcels
/// and Mato Grosso (#815), where raw matched the pre-#852 pass 2.
pub(super) const fn spill_codec(profile: MemoryProfile) -> IpcCompression {
    match profile {
        MemoryProfile::Bounded => IpcCompression::Lz4,
        MemoryProfile::Auto | MemoryProfile::Speed => IpcCompression::None,
    }
}

/// Spilled bytes per Arrow byte under `codec`. LZ4's is the least it saved
/// in #790's measurements (2.5x), so the page-cache reservation is not
/// under-priced.
pub(super) const fn spill_bytes_per_arrow_byte(codec: IpcCompression) -> f64 {
    match codec {
        IpcCompression::None => 1.0,
        IpcCompression::Lz4 => 0.4,
    }
}

/// The profile that sizes pass 2's read-ahead and window once the sink
/// `backing` is known (#815): an `auto` run that spills sizes them as
/// `bounded` does. Its encodes keep `auto`'s grant: as `bounded`'s they
/// cost NZ parcels and Mato Grosso 3-4 s of pass 2.
pub(super) fn sizing_profile(profile: MemoryProfile, backing: SinkBacking) -> MemoryProfile {
    match (profile, backing) {
        (MemoryProfile::Auto, SinkBacking::Spill) => MemoryProfile::Bounded,
        _ => profile,
    }
}

pub(super) const fn spill_codec_name(codec: IpcCompression) -> &'static str {
    match codec {
        IpcCompression::None => "raw",
        IpcCompression::Lz4 => "lz4",
    }
}

/// Buffered-row count above which `Auto` always spills partitioning's
/// full-resolution geometry, regardless of the RAM estimate. Preserves the
/// pre-#294 partitioning safety floor as a strict lower bound (partitioning
/// spills at least as often as before; the RAM gate can only make it spill
/// sooner).
const PARTITIONING_SPILL_ROWS: usize = 2_000_000;

/// Workload-driven backing choice for [`MemoryProfile::Auto`] (#294).
///
/// Chooses [`SinkBacking::Spill`] when the estimated buffered output exceeds
/// `memory`'s own [`MemoryProfile::Auto`] allowance, after every reservation
/// and grant already charged to it — a function of `feature_count × level_count ×
/// mode` (captured by `buffered_rows` and the per-mode byte estimate), NOT a
/// mode-only rule. `shared_from` is the input row count when the RAM sink's
/// levels share their rows' properties ([`MemoryBudget::ram_sink_estimate`]). Partitioning additionally keeps its historical absolute
/// row ceiling, so it spills at least as often as before.
pub(super) fn auto_backing(
    memory: &MemoryBudget,
    mode: Mode,
    buffered_rows: usize,
    shared_from: Option<usize>,
    avg_geom_bytes: Option<u64>,
    row_bytes: Option<u64>,
) -> SinkBacking {
    let estimate =
        memory.ram_sink_estimate(mode, buffered_rows, shared_from, avg_geom_bytes, row_bytes);
    let ram_gate_spill = estimate > memory.allowance(MemoryProfile::Auto);
    let abs_gate_spill =
        matches!(mode, Mode::Partitioning) && buffered_rows > PARTITIONING_SPILL_ROWS;
    if ram_gate_spill || abs_gate_spill {
        SinkBacking::Spill
    } else {
        SinkBacking::Ram
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
///    [`MemoryBudget::remaining`]'s fallback allowance, which export's
///    wave sizing shares).
///
/// The machine/cgroup probe is computed once per process and cached (the
/// `TYLERTOO_AUTO_MEM_LIMIT_BYTES` override is not: it is read on every call,
/// so set it before a run starts, not during one). The probe is called ~5×
/// per run (convert's pass-1 grid budget and pass-2 backing decision, export's
/// wave preflight and its log line), every input is process-global, and a
/// cached figure also keeps the paired "decide" and "log/assert" calls from
/// disagreeing when `MemAvailable` drifts between them.
///
/// Must NOT be called before pass 1 has run: the first call freezes the
/// figure, and a pre-scan call would cache a near-empty-cgroup headroom that
/// inflates every later `auto` decision. Pre-scan checks (the #543 pass-1
/// memory preflight) use the uncached [`probe_preflight_memory_limit`].
pub(super) fn available_memory_bytes() -> Option<u64> {
    #[cfg(test)]
    AVAILABLE_MEMORY_CALLS.with(|c| c.set(c.get() + 1));
    #[expect(
        clippy::disallowed_types,
        reason = "#455: cache the memory probe; fresh hard-limit preflights remain separate"
    )]
    static CACHED: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    available_memory_bytes_in(&CACHED, auto_mem_limit_override())
}

/// [`available_memory_bytes`] over an explicit cache and override. The
/// override is never cached: it is read on every call, so a long-lived
/// process (Python) that sets or clears `TYLERTOO_AUTO_MEM_LIMIT_BYTES`
/// after its first conversion is honoured, and a cached machine figure never
/// outlives the override that was in force when it was probed (found in the #708 work; the other env caches are #804).
#[expect(
    clippy::disallowed_types,
    reason = "#455: the memory probe cache, injectable so the override can be tested"
)]
fn available_memory_bytes_in(
    cache: &std::sync::OnceLock<Option<u64>>,
    over: Option<u64>,
) -> Option<u64> {
    over.or_else(|| *cache.get_or_init(probe_machine_memory_bytes))
}

#[cfg(test)]
thread_local! {
    /// Test seam: calls to [`available_memory_bytes`] made on this thread.
    /// Per-thread, so parallel tests that legitimately warm the process-wide
    /// cache cannot race an assertion that some code path never touched it.
    static AVAILABLE_MEMORY_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Test seam: how many times [`available_memory_bytes`] (and so its
/// process-wide cache) has been touched on the calling thread.
#[cfg(test)]
pub(super) fn available_memory_bytes_calls_on_this_thread() -> usize {
    AVAILABLE_MEMORY_CALLS.with(std::cell::Cell::get)
}

/// The `TYLERTOO_AUTO_MEM_LIMIT_BYTES` override, when set to a plain number.
fn auto_mem_limit_override() -> Option<u64> {
    std::env::var("TYLERTOO_AUTO_MEM_LIMIT_BYTES")
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

/// The uncached machine/cgroup probe, without the override: what the
/// process-wide cache holds.
fn probe_machine_memory_bytes() -> Option<u64> {
    let machine = read_proc_mem_available();
    let cgroup = cgroup_memory_limit_bytes();
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

/// Record the binding cgroup budget in this operation's channel.
pub(super) fn memory_diagnostic(sink: &crate::diagnostics::Diagnostics) {
    let machine = read_proc_mem_available();
    if let Some(limit) =
        cgroup_memory_limit_bytes().filter(|&limit| machine.is_none_or(|m| m > limit))
    {
        sink.once(crate::diagnostics::Diagnostic::MemoryBudget {
            count: 1,
            machine,
            limit,
        });
    }
}

pub(super) fn gib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0 * 1024.0)
}

/// Where a pre-scan memory figure came from (#543). Only a real hard cgroup
/// limit — where going over means an OOM kill — may justify failing a run up
/// front; every other figure is advisory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MemoryLimitSource {
    /// `TYLERTOO_AUTO_MEM_LIMIT_BYTES` — a knob to make `auto` conservative,
    /// not a statement about the box.
    Override,
    /// `/proc/meminfo` `MemAvailable`: momentary, and excludes swap.
    MemAvailable,
    /// cgroup v2 `memory.max` / v1 `memory.limit_in_bytes`: the OOM-kill limit.
    CgroupMax,
    /// cgroup v2 `memory.high`: a throttle ceiling, not an OOM limit.
    CgroupHigh,
}

impl MemoryLimitSource {
    /// Whether exceeding this figure means the kernel kills the process.
    pub(super) fn is_hard(self) -> bool {
        matches!(self, Self::CgroupMax)
    }

    /// How user-facing messages name this figure.
    pub(super) fn describe(self) -> &'static str {
        match self {
            Self::Override => "TYLERTOO_AUTO_MEM_LIMIT_BYTES override",
            Self::MemAvailable => "available memory (MemAvailable)",
            Self::CgroupMax => "cgroup hard memory limit (memory.max)",
            Self::CgroupHigh => "cgroup memory.high throttle limit",
        }
    }
}

/// A pre-scan memory figure and the probe term that supplied it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct MemoryLimit {
    pub(super) bytes: u64,
    pub(super) source: MemoryLimitSource,
}

/// Uncached, source-attributed memory probe for the #543 pass-1 preflight.
///
/// Same terms as [`available_memory_bytes`] (override, else the minimum of
/// `MemAvailable`, cgroup `memory.max` headroom and cgroup `memory.high`
/// headroom), but it (1) never reads or initializes that function's
/// process-wide cache — it runs before pass 1, and caching a pre-scan figure
/// would inflate every later `auto` decision (#485) — and (2) reports which
/// term binds, so the preflight can hard-error only against a real OOM limit.
/// On a tie the hard limit is reported. `None` when nothing could be probed
/// (e.g. macOS without the override).
pub(super) fn probe_preflight_memory_limit() -> Option<MemoryLimit> {
    probe_uncached_memory_limit(true)
}

/// Uncached figure past which the process is killed rather than throttled,
/// for export's dense-tile refusal (#665): the override, else the smaller of
/// `MemAvailable` and the cgroup hard-limit headroom.
///
/// Uncached because in a `tiles` run [`available_memory_bytes`] was frozen
/// during convert's pass 1, before convert freed that memory. cgroup
/// `memory.high` is left out: going over it throttles and reclaims, it does
/// not kill.
pub(super) fn probe_oom_memory_limit() -> Option<MemoryLimit> {
    probe_uncached_memory_limit(false)
}

/// The two uncached probes above: the override, else the binding term of
/// `MemAvailable`, cgroup `memory.max` and, when `with_memory_high`, cgroup
/// `memory.high`.
fn probe_uncached_memory_limit(with_memory_high: bool) -> Option<MemoryLimit> {
    if let Some(bytes) = auto_mem_limit_override() {
        return Some(MemoryLimit {
            bytes,
            source: MemoryLimitSource::Override,
        });
    }
    let machine = read_proc_mem_available().map(|bytes| MemoryLimit {
        bytes,
        source: MemoryLimitSource::MemAvailable,
    });
    let [cg_max, cg_high] = if cfg!(target_os = "linux") {
        read_cgroup_memory_limits_by_source(std::path::Path::new("/"))
    } else {
        [None, None]
    };
    binding_memory_limit([machine, cg_max, cg_high.filter(|_| with_memory_high)])
}

/// The smallest of the probed figures (a hard limit wins a tie).
fn binding_memory_limit(candidates: [Option<MemoryLimit>; 3]) -> Option<MemoryLimit> {
    candidates
        .into_iter()
        .flatten()
        .min_by_key(|l| (l.bytes, !l.source.is_hard()))
}

/// [`read_cgroup_memory_limit`], split by source: `[hard (v2 memory.max or v1
/// limit_in_bytes), v2 memory.high]` headrooms. v2 first; v1 only when v2
/// reports neither.
fn read_cgroup_memory_limits_by_source(root: &std::path::Path) -> [Option<MemoryLimit>; 2] {
    let with = |source| move |bytes| MemoryLimit { bytes, source };
    let v2_max = read_cgroup_v2_headroom(root, &["memory.max"]);
    let v2_high = read_cgroup_v2_headroom(root, &["memory.high"]);
    let hard = if v2_max.is_none() && v2_high.is_none() {
        read_cgroup_v1_limit(root)
    } else {
        v2_max
    };
    [
        hard.map(with(MemoryLimitSource::CgroupMax)),
        v2_high.map(with(MemoryLimitSource::CgroupHigh)),
    ]
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
    read_cgroup_v2_headroom(root, &["memory.max", "memory.high"])
}

/// cgroup v2 headroom under the smallest of `files` along this process's path.
fn read_cgroup_v2_headroom(root: &std::path::Path, files: &[&str]) -> Option<u64> {
    read_cgroup_headroom(
        root,
        CgroupSelector::V2,
        "sys/fs/cgroup",
        files,
        "memory.current",
        &["inactive_file", "slab_reclaimable"],
    )
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
    read_cgroup_headroom(
        root,
        CgroupSelector::V1Memory,
        "sys/fs/cgroup/memory",
        &["memory.limit_in_bytes"],
        "memory.usage_in_bytes",
        &["total_inactive_file"],
    )
}

fn read_cgroup_headroom(
    root: &std::path::Path,
    selector: CgroupSelector,
    mount: &str,
    files: &[&str],
    usage: &str,
    reclaimable: &[&str],
) -> Option<u64> {
    let rel = proc_self_cgroup_path(root, selector).unwrap_or_default();
    let binding = min_limit_along_path(&root.join(mount), &rel, files)?;
    Some(subtract_unreclaimable_usage(
        binding.bytes,
        &binding.dir,
        usage,
        reclaimable,
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
pub(super) enum CgroupSelector {
    /// The unified-hierarchy line: `0::<path>` (empty controller list).
    V2,
    /// A v1 line whose controller list contains `memory`.
    V1Memory,
}

/// The cgroup path this process belongs to, per `/proc/self/cgroup`
/// (`hierarchy-ID:controller-list:path`). `None` when the file is missing or
/// carries no matching line.
pub(super) fn proc_self_cgroup_path(
    root: &std::path::Path,
    want: CgroupSelector,
) -> Option<String> {
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

/// The [`SinkBacking`] `profile` gives the pass-2 sinks, without logging:
/// `speed` keeps them in RAM, `bounded` spills, and `auto` decides with
/// [`auto_backing`]. The one owner of that rule: [`resolve_backing`] logs
/// it, and the spill page-cache reservation (`stream::reserve_spill_cache`)
/// asks it ahead of the decision.
pub(super) fn sink_backing(
    memory: &MemoryBudget,
    profile: MemoryProfile,
    mode: Mode,
    buffered_rows: usize,
    shared_from: Option<usize>,
    avg_geom_bytes: Option<u64>,
    row_bytes: Option<u64>,
) -> SinkBacking {
    match profile {
        MemoryProfile::Speed => SinkBacking::Ram,
        MemoryProfile::Bounded => SinkBacking::Spill,
        MemoryProfile::Auto => auto_backing(
            memory,
            mode,
            buffered_rows,
            shared_from,
            avg_geom_bytes,
            row_bytes,
        ),
    }
}

/// Resolve the [`MemoryProfile`] (including [`MemoryProfile::Auto`]) to a
/// concrete [`SinkBacking`] with [`sink_backing`], logging the decision.
///
/// `buffered_rows` is the total winner count of the levels the engine buffers
/// (all but the streamed finest level). For `Auto` the choice is workload-based
/// (#294): the estimated buffered output (`f(buffered_rows, mode)`) is compared
/// against the run budget's allowance, so large duplicating runs prefer the
/// bounded spill path instead of the old unconditional RAM buffering. An
/// explicit profile always wins.
pub(super) fn resolve_backing(
    memory: MemoryBudget,
    profile: MemoryProfile,
    mode: Mode,
    buffered_rows: usize,
    shared_from: Option<usize>,
    avg_geom_bytes: Option<u64>,
    row_bytes: Option<u64>,
) -> SinkBacking {
    let backing = sink_backing(
        &memory,
        profile,
        mode,
        buffered_rows,
        shared_from,
        avg_geom_bytes,
        row_bytes,
    );
    if !matches!(profile, MemoryProfile::Auto) {
        log::debug!(
            "pass2 memory profile {profile:?} + {mode:?} (buffered ~{buffered_rows} rows) → {backing:?}"
        );
        return backing;
    }
    let estimate =
        memory.ram_sink_estimate(mode, buffered_rows, shared_from, avg_geom_bytes, row_bytes);
    let budget = memory.allowance(MemoryProfile::Auto);
    let avail_mib = memory.available().map_or_else(
        || "unknown".to_string(),
        |b| format!("{} MiB", b / (1024 * 1024)),
    );
    let geom = match (row_bytes, avg_geom_bytes.filter(|&b| b > 0)) {
        (Some(row), _) => match shared_from {
            Some(input) => format!("footer row {row} B shared from {input} input rows"),
            None => format!("footer row {row} B"),
        },
        (None, Some(b)) => format!("measured avg geom {b} B/row"),
        (None, None) => "unmeasured (calibrated constants)".to_string(),
    };
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

// ============================================================================
// #626: measure the retained sink bytes, and downgrade Ram→Spill once
// ============================================================================

/// Output rows the measurement must carry before its rate may flip a live
/// sink to spill. One batch of a real run clears this immediately; it only
/// stops a handful of rows from a leading sparse batch deciding the run.
const SINK_SAMPLE_MIN_ROWS: usize = 1_024;

/// What the pass-2 sink actually cost per buffered row, and what `auto` did
/// about it. Reported into `TYLERTOO_PROFILE_JSON` (#626 item 4) so a sizing
/// regression is visible without an RSS trace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SinkMeasurement {
    /// The run's private spill encoding, including an auto downgrade.
    pub(super) codec: IpcCompression,
    /// Measured retained bytes per buffered output row — the Arrow array
    /// bytes (geometry + every kept property column) of every batch handed to
    /// the sinks, averaged over the whole pass, plus
    /// [`SINK_ROW_ALLOC_OVERHEAD_BYTES`] per materialized row (per shared row
    /// of a shared batch, #852). `None` when nothing was buffered.
    pub(super) bytes_per_row: Option<u64>,
    /// Output rows the measurement covered: every buffered row of the pass.
    pub(super) sampled_rows: usize,
    /// What the whole buffered set costs in RAM: the bytes already buffered
    /// plus the not-yet-buffered planned rows at the measured rate. At the end
    /// of the pass it is the bytes actually buffered.
    pub(super) projected_bytes: u64,
    /// The RAM budget the projection was compared against
    /// (the `MemoryBudget` allowance).
    pub(super) budget_bytes: u64,
    /// A one-way Ram→Spill downgrade fired mid-pass.
    pub(super) downgraded: bool,
}

/// Runtime sink sizing for [`MemoryProfile::Auto`] (#626).
///
/// The up-front [`auto_backing`] choice is made before a single output row
/// exists: it knows pass 1's geometry weight and nothing about how wide the
/// retained property columns are. This measures the real thing — the Arrow
/// array bytes of the batches actually handed to the sinks, which by
/// construction are exactly the kept property columns plus the geometry
/// column — for **every** buffered batch of the pass, and after each input
/// batch projects the whole buffered set as *bytes already buffered + the
/// not-yet-buffered planned rows at the running average rate*. When that
/// passes the budget it downgrades the Ram sinks to Spill **once**.
///
/// The check never stops. A spatially sorted input can put its sparse rows
/// first, so no prefix of the input can vouch for the rest; and because the
/// bytes already buffered are a term of the projection, the downgrade fires
/// at the latest when what the Ram sinks actually hold reaches the budget,
/// however light the head was and however wrong the planned row count is.
///
/// One-way and once: a sink that has spilled never comes back, so the engine
/// cannot oscillate and no batch is ever written twice. `bounded` and `speed`
/// are unaffected — they are explicit instructions, not estimates — but the
/// measurement still runs for them, because the profile dump is worth the two
/// integer adds per batch.
///
/// **Known biases**, both toward spilling early (the safe direction):
/// `RecordBatch::get_array_memory_size` reports allocated buffer capacity, not
/// live bytes; and it includes each array's fixed struct overhead, which a
/// coarse level's few-row batches amortize over few rows, so the average
/// rate reads higher than the per-row data alone. Both are real allocations
/// while the batches sit in RAM, so neither is corrected for. The remaining
/// bias has no sign: the remainder is projected at the average of the rows
/// seen so far, so a heavy tail is caught as it is buffered, not before.
struct SinkAutoTuner {
    /// Output rows observed so far (every buffered row of the pass).
    rows: usize,
    /// Arrow array bytes of those rows, plus [`SINK_ROW_ALLOC_OVERHEAD_BYTES`]
    /// per row they materialize: per output row of an owned batch, per
    /// shared row of a shared one (#852), whose output rows are positions.
    bytes: u64,
    /// Output rows the plan expects the buffered levels to hold in total.
    planned_rows: usize,
    /// the `MemoryBudget` allowance, captured once.
    budget_bytes: u64,
    /// A downgrade is still possible: `auto`, and the sinks are still Ram.
    armed: bool,
    downgraded: bool,
}

impl SinkAutoTuner {
    /// A downgrade can only apply to a [`MemoryProfile::Auto`] run that
    /// started in [`SinkBacking::Ram`], so only that case probes a budget.
    ///
    /// The probe is skipped outright otherwise: `available_memory_bytes`
    /// caches process-wide on its first call, and a `bounded`/`speed` run must
    /// not have that call moved here (#485).
    fn new(
        profile: MemoryProfile,
        backing: SinkBacking,
        planned_rows: usize,
        memory: MemoryBudget,
    ) -> Self {
        let armed = matches!(profile, MemoryProfile::Auto) && matches!(backing, SinkBacking::Ram);
        let budget_bytes = if armed {
            memory.allowance(MemoryProfile::Auto)
        } else {
            0
        };
        Self::with_budget(armed, planned_rows, budget_bytes)
    }

    /// [`SinkAutoTuner::new`] with the RAM budget supplied instead of probed —
    /// the seam the unit tests drive, so they never mutate the process
    /// environment (nothing in this crate's unit tests does) and never depend
    /// on the box they run on. A `budget_bytes` of `auto_budget_bytes(Some(n))`
    /// is exactly what `TYLERTOO_AUTO_MEM_LIMIT_BYTES=n` produces, since the
    /// override short-circuits the probe to `Some(n)`.
    fn with_budget(armed: bool, planned_rows: usize, budget_bytes: u64) -> Self {
        Self {
            rows: 0,
            bytes: 0,
            planned_rows,
            budget_bytes,
            armed,
            downgraded: false,
        }
    }

    /// Fold one output batch — the exact value about to be buffered — into
    /// the measurement. Runs for every batch of the pass.
    fn observe(&mut self, batch: &LevelBatch) {
        self.rows += batch.num_rows();
        let floor = match batch {
            LevelBatch::Owned(b) => b.num_rows() as u64 * SINK_ROW_ALLOC_OVERHEAD_BYTES,
            LevelBatch::Shared { .. } => 0,
        };
        self.bytes = self
            .bytes
            .saturating_add(batch.held_bytes())
            .saturating_add(floor);
    }

    /// Fold in the rows one input batch's levels share, once.
    fn observe_shared(&mut self, per_level: &[Option<(LevelBatch, usize)>]) {
        if let Some(rows) = per_level.iter().find_map(|out| match out {
            Some((LevelBatch::Shared { rows, .. }, _)) => Some(rows),
            _ => None,
        }) {
            self.bytes = self.bytes.saturating_add(
                rows.held_bytes() + rows.rows as u64 * SINK_ROW_ALLOC_OVERHEAD_BYTES,
            );
        }
    }

    /// Measured retained bytes per buffered output row, floor included.
    /// `None` until at least one row has been observed.
    fn bytes_per_row(&self) -> Option<u64> {
        (self.rows > 0).then(|| self.bytes / self.rows as u64)
    }

    /// What the whole buffered set costs in RAM: the bytes already buffered
    /// (floor included) plus the planned rows not yet seen at the measured
    /// rate. Never below what is already held, even when the plan
    /// under-counts the rows.
    fn projected_bytes(&self) -> u64 {
        self.bytes_per_row().map_or(0, |per_row| {
            let remaining = self.planned_rows.saturating_sub(self.rows) as u64;
            self.bytes.saturating_add(remaining.saturating_mul(per_row))
        })
    }

    /// Whether the measurement now says the Ram sinks cannot hold the run.
    fn should_downgrade(&self) -> bool {
        self.armed
            && !self.downgraded
            && self.rows >= SINK_SAMPLE_MIN_ROWS
            && self.projected_bytes() > self.budget_bytes
    }

    /// The warning a downgrade of `levels` RAM sinks logs.
    fn downgrade_warning(&self, levels: usize) -> String {
        format!(
            "[convert] pass2 auto: measured {} B per buffered row over {} buffered row(s) \
             projects {:.1} GiB for the buffered set — past the {:.1} GiB budget; downgrading \
             {levels} RAM sink(s) to spill (#626)",
            self.bytes_per_row().unwrap_or(0),
            self.rows,
            gib(self.projected_bytes()),
            gib(self.budget_bytes),
        )
    }

    /// Encodes the drain may run: once downgraded to spill, as `bounded`
    /// (#815).
    fn encode_cap(&self, units: usize) -> usize {
        let sized_as = if self.downgraded {
            MemoryProfile::Bounded
        } else {
            MemoryProfile::Auto
        };
        MemoryBudget::encode_units_as(sized_as, units)
    }

    /// Record that the downgrade happened (and disarm: it is one-way).
    fn mark_downgraded(&mut self) {
        self.downgraded = true;
        self.armed = false;
    }

    fn report(&self) -> SinkMeasurement {
        SinkMeasurement {
            codec: IpcCompression::None,
            bytes_per_row: self.bytes_per_row(),
            sampled_rows: self.rows,
            projected_bytes: self.projected_bytes(),
            budget_bytes: self.budget_bytes,
            downgraded: self.downgraded,
        }
    }
}

/// When `tuner` says the RAM sinks cannot hold the run (#626), flush every
/// still-Ram sink into a fresh spill file, adding the bytes handed to the
/// spill writers to `spill_bytes` so the per-level spill accounting stays
/// whole, and stop the levels sharing rows (`share`), which only pays off in
/// RAM. Sinks that already spill are untouched.
#[expect(
    clippy::disallowed_types,
    reason = "#852: the sharing flag the window's batches read"
)]
fn downgrade_if_due(
    tuner: &mut SinkAutoTuner,
    sinks: &mut [LevelSink],
    spill_bytes: &mut [u64],
    (out_schema, spill_dir): (&Schema, Option<&Path>),
    (diagnostics, share): (&crate::diagnostics::Diagnostics, &AtomicBool),
) -> Result<(), ConvertError> {
    if !tuner.should_downgrade() {
        return Ok(());
    }
    diagnostics.warn(tuner.downgrade_warning(sinks.len()));
    for (li, sink) in sinks.iter_mut().enumerate() {
        spill_bytes[li] += sink.downgrade_to_spill(out_schema, spill_dir)?;
    }
    tuner.mark_downgraded();
    share.store(false, Relaxed);
    Ok(())
}

/// A message from the reader thread: one raw input batch, its cumulative row
/// offset, and how long the read+decode took (for [profile] accounting).
struct ReadMsg {
    row_offset: usize,
    batch: RecordBatch,
    read_dur: Duration,
}

/// The non-geometry columns of the rows any buffered level keeps from one
/// input batch, held once for all of them (#852).
///
/// Every duplicating level copies a kept row's properties unchanged, and a
/// row is kept at every level from its own up, so per-level copies held the
/// same properties up to once per level: on Overture segments (98 columns,
/// 3.8 buffered rows per input row) they were most of the sink. Levels now
/// hold only their geometry and a row index into this, and the drain
/// rebuilds each level batch with `take`, value for value what the level
/// copied before.
pub(super) struct SharedRows {
    /// The kept rows' columns, in the source schema's non-geometry order.
    columns: Vec<ArrayRef>,
    /// How many rows they hold.
    rows: usize,
    /// The levels' geometries in one column. A pool of every distinct
    /// geometry they keep, encoded once, when they share enough: a level
    /// whose cascade step left a feature unchanged shares the finer level's
    /// geometry (#499), so per-level columns held it once per level.
    /// Otherwise each level's geometries back to back
    /// ([`LevelGeometry::Range`]): one allocation per input batch, not one
    /// per level, which peak RSS needs (per-level columns built across the
    /// pass-2 workers held 160–190 MiB more on Moldova, at the same live heap).
    geometry: ArrayRef,
}

impl SharedRows {
    /// The rows of `batch` that any of `kept` (per level, ascending batch
    /// row indices) keeps, over `non_geom_cols`, and each level's positions
    /// in them. When every row is kept the input's own columns are shared,
    /// with no copy.
    pub(super) fn new(
        batch: &RecordBatch,
        non_geom_cols: &[usize],
        kept: &[&[usize]],
        geometry: ArrayRef,
    ) -> Result<(Self, Vec<UInt32Array>), ConvertError> {
        let n = batch.num_rows();
        // Input-batch sized, so dropped here rather than held to the drain.
        let mut pos = vec![u32::MAX; n];
        for rows in kept {
            for &i in *rows {
                pos[i] = 0;
            }
        }
        let mut rows: Vec<u32> = Vec::with_capacity(n);
        for (i, p) in pos.iter_mut().enumerate() {
            if *p == 0 {
                *p = u32::try_from(rows.len()).expect("batch rows fit in u32");
                rows.push(u32::try_from(i).expect("batch rows fit in u32"));
            }
        }
        let kept_rows = rows.len();
        let columns = if rows.len() == n {
            non_geom_cols
                .iter()
                .map(|&c| batch.column(c).clone())
                .collect()
        } else {
            let rows = UInt32Array::from(rows);
            non_geom_cols
                .iter()
                .map(|&c| arrow_select::take::take(batch.column(c).as_ref(), &rows, None))
                .collect::<Result<_, _>>()?
        };
        let index = kept
            .iter()
            .map(|rows| rows.iter().map(|&i| pos[i]).collect())
            .collect();
        Ok((
            Self {
                columns,
                rows: kept_rows,
                geometry,
            },
            index,
        ))
    }

    /// What these columns hold in RAM.
    fn held_bytes(&self) -> u64 {
        self.columns
            .iter()
            .chain([&self.geometry])
            .map(|c| c.get_array_memory_size() as u64)
            .sum()
    }
}

/// A shared level batch's geometries in [`SharedRows`]' geometry column.
#[derive(Clone)]
pub(super) enum LevelGeometry {
    /// Positions in the column, a pool of the levels' distinct geometries.
    Pool(UInt32Array),
    /// The column's rows `start..start + len`.
    Range(usize, usize),
}

impl LevelGeometry {
    fn held_bytes(&self) -> usize {
        match self {
            Self::Pool(at) => at.get_array_memory_size(),
            Self::Range(..) => 0,
        }
    }
}

/// One buffered output batch of one level.
pub(super) enum LevelBatch {
    /// A level batch as built.
    Owned(RecordBatch),
    /// The level's own columns, with its other columns held in a
    /// [`SharedRows`] every level built from the same input batch shares.
    Shared {
        schema: SchemaRef,
        rows: Arc<SharedRows>,
        /// Positions in `rows` of this level's rows, in output order.
        index: UInt32Array,
        /// This level's geometries.
        geometry: LevelGeometry,
        /// Where the geometry column goes in the source schema.
        geom_idx: usize,
        /// Any columns the level generates after the source schema's
        /// (`coalesced_count`).
        own: Vec<ArrayRef>,
    },
}

impl LevelBatch {
    /// A shared level batch: its rows at `index` in `rows` and its
    /// geometries as `geometry` holds them ([`SharedRows::new`]).
    pub(super) fn shared(
        schema: SchemaRef,
        rows: &Arc<SharedRows>,
        index: UInt32Array,
        geometry: LevelGeometry,
        geom_idx: usize,
        own: Vec<ArrayRef>,
    ) -> Self {
        Self::Shared {
            schema,
            rows: Arc::clone(rows),
            index,
            geometry,
            geom_idx,
            own,
        }
    }

    pub(super) fn num_rows(&self) -> usize {
        match self {
            Self::Owned(b) => b.num_rows(),
            Self::Shared { index, .. } => index.len(),
        }
    }

    /// What this level batch alone holds in RAM: its shared rows are
    /// counted once per input batch, by the caller.
    fn held_bytes(&self) -> u64 {
        match self {
            Self::Owned(b) => b.get_array_memory_size() as u64,
            Self::Shared {
                index,
                geometry,
                own,
                ..
            } => own
                .iter()
                .map(|c| c.get_array_memory_size() as u64)
                .sum::<u64>()
                .saturating_add((index.get_array_memory_size() + geometry.held_bytes()) as u64),
        }
    }

    /// The level batch the writer takes. A ranged level's geometries are a
    /// slice of the shared column unless `compact`: a spill writes every
    /// value a sliced union's children hold, the whole input batch's.
    pub(super) fn materialize(self, compact: bool) -> Result<RecordBatch, ConvertError> {
        match self {
            Self::Owned(b) => Ok(b),
            Self::Shared {
                schema,
                rows,
                index,
                geometry,
                geom_idx,
                own,
            } => {
                let mut columns = rows
                    .columns
                    .iter()
                    .map(|c| arrow_select::take::take(c.as_ref(), &index, None))
                    .collect::<Result<Vec<_>, _>>()?;
                let geometry = match geometry {
                    LevelGeometry::Pool(at) => {
                        arrow_select::take::take(rows.geometry.as_ref(), &at, None)?
                    }
                    LevelGeometry::Range(start, len) if compact => {
                        let at = UInt32Array::from_iter_values(
                            (start..start + len)
                                .map(|i| u32::try_from(i).expect("batch rows fit in u32")),
                        );
                        arrow_select::take::take(rows.geometry.as_ref(), &at, None)?
                    }
                    LevelGeometry::Range(start, len) => rows.geometry.slice(start, len),
                };
                columns.insert(geom_idx, geometry);
                columns.extend(own);
                Ok(RecordBatch::try_new(schema, columns)?)
            }
        }
    }
}

/// One level's ordered output buffer.
enum LevelSink {
    Ram(Vec<LevelBatch>),
    // Not boxed: `SpillState` used to own the arrow `StreamWriter` inline,
    // which (arrow 59) pushed the variant past clippy's large_enum_variant
    // threshold. The writer now lives on the spill thread, leaving a sender
    // and a join handle here.
    Spill(SpillState),
}

impl LevelSink {
    #[cfg(test)]
    fn new(
        backing: SinkBacking,
        out_schema: &Schema,
        spill_dir: Option<&Path>,
    ) -> Result<Self, ConvertError> {
        Self::with_codec(backing, out_schema, spill_dir, IpcCompression::None)
    }

    fn with_codec(
        backing: SinkBacking,
        out_schema: &Schema,
        spill_dir: Option<&Path>,
        codec: IpcCompression,
    ) -> Result<Self, ConvertError> {
        Ok(match backing {
            SinkBacking::Ram => LevelSink::Ram(Vec::new()),
            SinkBacking::Spill => {
                LevelSink::Spill(SpillState::with_codec(out_schema, spill_dir, codec)?)
            }
        })
    }

    /// Push one output batch into the sink. Returns the bytes handed to the
    /// spill writer (0 for the RAM path — nothing is measured there, since
    /// the goal is accounting for previously-invisible spill I/O).
    fn push(&mut self, batch: LevelBatch) -> Result<u64, ConvertError> {
        match self {
            LevelSink::Ram(v) => {
                v.push(batch);
                Ok(0)
            }
            LevelSink::Spill(s) => s.push(batch.materialize(true)?),
        }
    }

    /// One-way Ram→Spill downgrade (#626): open a spill file for this level
    /// and hand it every batch already buffered, in buffer order, before the
    /// sink accepts anything further. Returns the bytes handed to the spill
    /// writer (0 when the sink was already spilling, which makes this a no-op).
    ///
    /// No data is lost and no row moves: the `Vec` is drained front-to-back
    /// into the same FIFO channel a spill sink has always used, so the level's
    /// row sequence — and therefore its row-group boundaries and output bytes
    /// — are what they would have been had the level spilled from the start.
    fn downgrade_to_spill(
        &mut self,
        out_schema: &Schema,
        spill_dir: Option<&Path>,
    ) -> Result<u64, ConvertError> {
        if !matches!(self, LevelSink::Ram(_)) {
            return Ok(0);
        }
        // Opened BEFORE the buffer is moved out: a spill file that cannot be
        // created leaves the sink exactly as it was, still holding its rows,
        // for the caller's error path to report against.
        let mut state = SpillState::new(out_schema, spill_dir)?;
        let LevelSink::Ram(buffered) = std::mem::replace(self, LevelSink::Ram(Vec::new())) else {
            return Err(internal("sink changed backing during a downgrade"));
        };
        let mut bytes = 0u64;
        for batch in buffered {
            bytes += state.push(batch.materialize(true)?)?;
        }
        *self = LevelSink::Spill(state);
        Ok(bytes)
    }
}

/// Keep the retention measurement at the same boundary as the sink's payload.
fn buffer_batch(
    sink: &mut LevelSink,
    tuner: &mut SinkAutoTuner,
    batch: LevelBatch,
) -> Result<u64, ConvertError> {
    tuner.observe(&batch);
    sink.push(batch)
}

/// A pass-2 spill file under `spill_dir`, or the process temp dir when none
/// is given (#427). Named so a leftover from a killed run is recognizable.
pub(super) fn spill_temp_file(spill_dir: Option<&Path>) -> std::io::Result<NamedTempFile> {
    let dir = spill_dir.map_or_else(std::env::temp_dir, Path::to_path_buf);
    tempfile::Builder::new()
        .prefix("tylertoo-spill-")
        .suffix(".arrow")
        .tempfile_in(&dir)
        .map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("cannot create a spill file in {}: {e}", dir.display()),
            )
        })
}

/// Shared level batches the RAM drain rebuilds at once (at most its encode
/// width). Lines z0-13 auto, pass 2 / peak RSS by width: 1: 1.88 s / 2.74 GB,
/// 4: 1.22 / 2.99, 8: 1.10 / 3.11, 16: 1.11 / 3.39, 64: 1.03 / 3.56 (base,
/// before #852: 1.04 / 3.93). Past 8 the wall barely moves and the peak climbs.
const DRAIN_REBUILD_AHEAD: usize = 8;

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
    #[cfg(test)]
    materialized_rows: u64,
}

/// What a finished spill-writer thread hands back.
struct SpillWriterDone {
    temp: NamedTempFile,
    /// Core-seconds spent in Arrow IPC encode + write on the writer thread.
    write_time: Duration,
}

impl SpillState {
    /// The spill a mid-pass downgrade opens, which only `auto` does.
    /// `spill_dir` is where the spill file goes (#427: `--spill-dir`, the one
    /// scratch knob); `None` is the process temp dir, as before.
    fn new(out_schema: &Schema, spill_dir: Option<&Path>) -> Result<Self, ConvertError> {
        Self::with_codec(out_schema, spill_dir, spill_codec(MemoryProfile::Auto))
    }

    fn with_codec(
        out_schema: &Schema,
        spill_dir: Option<&Path>,
        codec: IpcCompression,
    ) -> Result<Self, ConvertError> {
        let temp = spill_temp_file(spill_dir)?;
        let write_handle = temp.reopen()?;
        // Constructed on the caller's thread so a broken spill directory or an
        // unwritable temp file fails the conversion here, with the caller's
        // error handling, rather than inside a thread nobody has joined yet.
        let writer = StreamWriter::try_new_with_compression(
            BufWriter::new(write_handle),
            out_schema,
            codec,
        )?;
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
            #[cfg(test)]
            materialized_rows: 0,
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
        #[cfg(test)]
        let rows = batch.column_by_name("geometry").map_or(0, |geometry| {
            (geometry.len() - geometry.null_count()) as u64
        });
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
        #[cfg(test)]
        {
            self.materialized_rows += rows;
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
        #[cfg(test)]
        spill_tests::record_spill(&done.temp, self.materialized_rows);
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
    ///
    /// The `resume_unwind` on the non-unwinding path is therefore deliberate,
    /// not an oversight: a writer thread that panicked while the driver was
    /// merely dropping its sinks has lost data, and swallowing that would turn
    /// a crash into a silently truncated level. The double-panic abort is the
    /// only case worth suppressing, and `thread::panicking()` is exactly that
    /// guard.
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
    /// #626: what a buffered row really cost, and whether `auto` had to
    /// downgrade the sinks to spill mid-pass.
    pub(super) sink: SinkMeasurement,
}

/// Buffer + write levels `0..ctxs.len()` (all but the streamed finest level)
/// from a single read of the input. Returns per-level `(outcome,
/// rows_written, vertex_count, spill_bytes_written)`, in level order — the
/// outcome flags a level the writer skipped because every candidate collapsed
/// during simplification (#211) — plus the engine's stage-timer snapshot.
/// Each level built on its own from the input batch (cascade off, or
/// partitioning), in parallel.
fn process_levels_apart(
    batch: &RecordBatch,
    row_offset: usize,
    ctxs: &[LevelStreamCtx<'_>],
    timers: &Pass2Timers,
) -> Result<Vec<Option<(LevelBatch, usize)>>, ConvertError> {
    ctxs.par_iter()
        .map(|ctx| {
            Ok(process_level_batch(batch, row_offset, ctx, timers)?
                .map(|(b, v)| (LevelBatch::Owned(b), v)))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .collect()
}

pub(super) fn run_pass2_buffered(
    writer: &mut OverviewWriter<File>,
    ctxs: &[LevelStreamCtx<'_>],
    hints: &[usize],
    read: StreamRead<'_>,
    inputs: BufferOptions<'_>,
) -> Result<Pass2EngineResult, ConvertError> {
    let StreamRead {
        source,
        read_tuning,
        row_groups: selected_row_groups,
        in_flight,
        window,
    } = read;
    let BufferOptions {
        profile,
        backing,
        out_schema,
        spill_dir,
        drain: (encode, slot_bytes, read_granted),
        budget,
    } = inputs;
    let num_levels = ctxs.len();
    debug_assert_eq!(num_levels, hints.len());

    let t_engine = Instant::now();
    let timers = Pass2Timers::default();

    let mut sinks = (0..num_levels)
        .map(|_| LevelSink::with_codec(backing, out_schema, spill_dir, spill_codec(profile)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut rows = vec![0usize; num_levels];
    let mut verts = vec![0usize; num_levels];
    // Bytes handed to the spill writer, per level (previously uncounted).
    // Stays all-zero under `SinkBacking::Ram`.
    let mut spill_bytes = vec![0u64; num_levels];
    // #626: measures what a buffered row really costs and, for `auto`, flips
    // the sinks to spill once if the measured rate projects past the budget.
    let mut tuner = SinkAutoTuner::new(profile, backing, hints.iter().sum(), budget);
    #[cfg(test)]
    spill_tests::TUNER_ARMING.with(|c| {
        c.set((
            read_tuning.budget.allowance(MemoryProfile::Auto),
            read_granted,
            tuner.budget_bytes,
        ));
    });

    // Consumer state borrowed mutably by the consumer closure below.
    let (rows_ref, verts_ref, sinks_ref, spill_bytes_ref) =
        (&mut rows, &mut verts, &mut sinks, &mut spill_bytes);
    let tuner_ref = &mut tuner;
    let timers_ref = &timers;
    let cascade = ctxs.first().is_some_and(|c| c.is_cascading_duplicating());
    // #494: `in_flight` is shared between the read-ahead channel and the
    // consumer's compute window, so the resident input stays about what one
    // channel of depth `in_flight` held before the window existed.
    // The window was granted from the run's budget after the read-ahead
    // (`stream::grant_pass2_read`, #716).
    let (read_depth, _) = split_in_flight(in_flight);
    // #817 B1: the largest input batch read, so the drain reuses what the
    // read-ahead and window really held, not what they were granted. And
    // whether the levels share rows (#852): only two or more of them, held
    // in RAM, since a spilled level writes its rows out at once
    // ([`downgrade_if_due`]) and one level would only copy them twice.
    #[expect(
        clippy::disallowed_types,
        reason = "#817 B1, #852: per-call state shared with the read thread and the window"
    )]
    let (peak_batch, share) = (
        AtomicU64::new(0),
        AtomicBool::new(matches!(backing, SinkBacking::Ram) && num_levels > 1),
    );
    #[cfg(test)]
    spill_tests::SHARING.with(|c| c.set(Some(share.load(Relaxed))));
    let peak_batch_ref = &peak_batch;
    scoped_pipe(
        read_depth,
        // Producer: the input, in read order, over the per-part bbox-selected
        // row groups (#102 — the same selection both passes use, so global row
        // indices stay aligned). `read_in_order` is either one synchronous
        // Parquet reader on this thread (the reference path) or several
        // concurrent ones merged back into the identical batch sequence
        // (#494); either way the reader lives off the rayon pool, so reads are
        // never head-of-line blocked behind compute work.
        |tx: &Sender<ReadMsg>| -> Result<(), ConvertError> {
            source.read(
                selected_row_groups,
                None,
                read_tuning,
                &mut |batch, row_offset, read_dur| {
                    peak_batch_ref.fetch_max(batch.get_array_memory_size() as u64, Relaxed);
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
        // Consumer: process a window of batches at a time (#494), each one
        // parallelized within itself as well, and push every batch's output
        // in read order. Because `for_each_window` emits in receive order and
        // each batch is appended before the next one's output, each sink
        // stays in input order without a reorder buffer.
        //
        // Before the window the consumer took one batch at a time, so every
        // batch was a barrier: its serial geometry decode, its slowest feature
        // and its slowest level all finished before the next batch started.
        // A window overlaps those across batches.
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
            let stats = for_each_window(
                &rx,
                window,
                |msg: &ReadMsg| -> Result<Vec<Option<(LevelBatch, usize)>>, ConvertError> {
                    let (batch, row_offset) = (&msg.batch, msg.row_offset);
                    if cascade {
                        process_batch_cascade(
                            batch,
                            row_offset,
                            ctxs,
                            timers_ref,
                            share.load(Relaxed),
                        )
                    } else {
                        process_levels_apart(batch, row_offset, ctxs, timers_ref)
                    }
                },
                |msg, per_level| {
                    Pass2Timers::add_dur(timers_ref.read_cell(), msg.read_dur);
                    tuner_ref.observe_shared(&per_level);
                    for (li, out) in per_level.into_iter().enumerate() {
                        if let Some((out, v)) = out {
                            rows_ref[li] += out.num_rows();
                            verts_ref[li] += v;
                            spill_bytes_ref[li] +=
                                buffer_batch(&mut sinks_ref[li], tuner_ref, out)?;
                        }
                    }
                    downgrade_if_due(
                        tuner_ref,
                        sinks_ref,
                        spill_bytes_ref,
                        (out_schema, spill_dir),
                        (ctxs[0].diagnostics, &share),
                    )?;
                    if last_progress.elapsed().as_secs() >= 10 {
                        last_progress = Instant::now();
                        log::info!(
                            "[convert] pass 2: {} input row(s) processed ({} output \
                             row(s) buffered across {num_levels} level(s))",
                            msg.row_offset + msg.batch.num_rows(),
                            rows_ref.iter().sum::<usize>(),
                        );
                    }
                    Ok(ReadFlow::Continue)
                },
            )?;
            timers_ref.record_windows(stats);
            Ok(())
        },
    )?;

    // Drain each level's sink into the writer, in level order. An empty
    // buffered level is skipped and renumbered by the writer (#211); the
    // outcome is threaded back to the caller with the level's stats.
    let mut outcomes = Vec::with_capacity(num_levels);
    // #817 B1: the read is over, so the drain reuses what it measurably held
    let writer_units = writer.encode_concurrency();
    // ...and only what the process gave back below its peak RSS: freed input
    // the allocator kept is not free for the drain (bounded Moldova still
    // peaked 114 MiB higher on the input bytes alone). None off Linux.
    let below_peak = super::procmem::read_proc_status()
        .and_then(|m| Some(m.hwm_kib?.saturating_sub(m.rss_kib?) * 1024))
        .unwrap_or(0);
    let held = peak_batch.into_inner().saturating_mul(in_flight as u64);
    let width = MemoryBudget::drain_encodes(
        Grant {
            units: tuner.encode_cap(encode.units),
            ..encode
        },
        slot_bytes,
        held.min(read_granted),
        below_peak,
        rayon::current_num_threads(),
    );
    log::debug!(
        "[profile] pass 2 drain: {width} encode(s) on {held} B ({below_peak} B below peak RSS)"
    );
    #[cfg(test)]
    spill_tests::DRAIN_WIDTH.with(|c| c.set(width));
    writer.set_encode_concurrency(width)?;
    for li in 0..num_levels {
        let sink = std::mem::replace(&mut sinks[li], LevelSink::Ram(Vec::new()));
        let t_drain_level = Instant::now();
        let outcome = drain_sink(writer, li, hints[li], sink, width, &timers)?;
        log::debug!(
            "[profile] level {li}: rows={} spill={:.2} MiB drain={:.2}s",
            rows[li],
            spill_bytes[li] as f64 / (1024.0 * 1024.0),
            t_drain_level.elapsed().as_secs_f64(),
        );
        outcomes.push(outcome);
    }
    // Drains the widened encodes past `writer_units` before the caller
    // releases the read grants they were priced against.
    writer.set_encode_concurrency(tuner.encode_cap(writer_units))?;

    if let Some(per_row) = tuner.bytes_per_row() {
        // #626: the number the up-front estimate could only guess at. Info
        // level so a sizing regression is visible in an ordinary run log, not
        // just in `TYLERTOO_PROFILE_JSON`.
        log::info!(
            "[convert] pass2 sink: measured {per_row} B per buffered row over {} buffered \
             row(s) → {:.1} GiB for {} planned buffered row(s){}",
            tuner.rows,
            gib(tuner.projected_bytes()),
            tuner.planned_rows,
            if tuner.downgraded {
                " (downgraded to spill mid-pass)"
            } else {
                ""
            },
        );
    }
    timers.log_engine_summary(t_engine.elapsed().as_secs_f64(), rows.iter().sum());
    let levels = outcomes
        .into_iter()
        .zip(rows)
        .zip(verts)
        .zip(spill_bytes)
        .map(|(((outcome, r), v), s)| (outcome, r, v, s))
        .collect();
    Ok(Pass2EngineResult {
        levels,
        timers,
        sink: SinkMeasurement {
            codec: if matches!(backing, SinkBacking::Spill) || tuner.downgraded {
                spill_codec(profile)
            } else {
                IpcCompression::None
            },
            ..tuner.report()
        },
    })
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
    width: usize,
    timers: &Pass2Timers,
) -> Result<LevelWriteOutcome, ConvertError> {
    let t_drain = Instant::now();
    let outcome = match sink {
        LevelSink::Ram(batches) => {
            // Rebuilt several at a time, in order: one thread rebuilding
            // every shared level batch (#852) left the drain's encodes
            // waiting. On this thread's own pool, so a pool worker helps
            // build what it waits for (`tests/rayon_global_pool_reentry.rs`).
            // A rebuild that fails ends the level unwritten.
            let mut pending = batches.into_iter();
            let rebuilt = std::iter::from_fn(|| {
                let chunk: Vec<LevelBatch> = pending
                    .by_ref()
                    .take(width.clamp(1, DRAIN_REBUILD_AHEAD))
                    .collect();
                (!chunk.is_empty()).then(|| {
                    chunk
                        .into_par_iter()
                        .map(|b| b.materialize(false))
                        .collect::<Vec<_>>()
                })
            })
            .flatten();
            writer.write_level_from(level_idx, Some(hint), rebuilt)?
        }
        LevelSink::Spill(state) => {
            // `_temp` keeps the spill file on disk until the reader is drained.
            // Joining the writer here also folds its encode+write core-seconds
            // into `timers.spill_write`, so the stage split still accounts for
            // the I/O now that it no longer runs on the consumer thread.
            //
            // Known, bounded double-count: the join's own wait sits inside
            // `t_drain` AND the joined writer's `write_time` lands in
            // `spill_write`, so the two stages overlap by however long the
            // writer was still behind at drain time — at most
            // `SPILL_QUEUE_DEPTH + 1` batches' worth of encode. Left in place
            // deliberately: excluding it would mean splitting the join out of
            // the drain timer and losing the (real) wall time the drain spends
            // waiting, which is the thing the drain counter exists to show.
            let (reader, _temp) = state.into_reader(timers)?;
            // #817 B1: the spill decodes on its own thread, so the writer's
            // thread only feeds row groups (on Oregon roads the decode was
            // most of what it did).
            scoped_pipe(
                SPILL_QUEUE_DEPTH,
                move |tx: &Sender<Result<RecordBatch, ConvertError>>| -> Result<(), ConvertError> {
                    for batch in reader {
                        #[cfg(test)]
                        spill_tests::note_decode();
                        // A read error goes to the writer in order and ends
                        // the level unwritten.
                        let failed = batch.is_err();
                        if tx.send(batch.map_err(ConvertError::from)).is_err() || failed {
                            break; // on a writer failure, its error is reported
                        }
                    }
                    Ok(())
                },
                |rx: Receiver<Result<RecordBatch, ConvertError>>| {
                    writer.write_level_from(level_idx, Some(hint), rx.into_iter())
                },
            )?
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
    use crate::input_set::ConvertSource;
    use arrow_array::{ArrayRef, Int64Array};
    use arrow_schema::{DataType, Field};
    use std::sync::Arc;

    // Observe completed files at the spill owner, rather than treating
    // get_array_memory_size (the existing profile counter) as disk bytes.
    // Unique directories isolate simultaneous nextest/plain-cargo tests.
    type SpillObservations = std::collections::BTreeMap<std::path::PathBuf, Vec<(u64, u64)>>;
    static OBSERVED: std::sync::Mutex<SpillObservations> =
        std::sync::Mutex::new(std::collections::BTreeMap::new());

    thread_local! {
        /// The last drain width set on this thread (#817 B1).
        pub(super) static DRAIN_WIDTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        /// The last tuner arming on this thread (#791 item 3): the budget
        /// the read was sized from, the read's grants, the armed budget.
        pub(super) static TUNER_ARMING: std::cell::Cell<(u64, u64, u64)> =
            const { std::cell::Cell::new((0, 0, 0)) };
        /// Whether the last buffered pass 2 on this thread shared rows (#852).
        pub(super) static SHARING: std::cell::Cell<Option<bool>> =
            const { std::cell::Cell::new(None) };
        /// Spilled batches decoded on this thread (#817 B1).
        static DECODED_HERE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    pub(super) fn note_decode() {
        DECODED_HERE.with(|c| c.set(c.get() + 1));
    }

    /// #817 B1: a spilled level decodes on its own thread, not on the one
    /// that feeds the writer (the drain's critical path: Oregon roads spent
    /// 1.2 of its 1.5 core-seconds there decoding).
    #[test]
    fn spill_decodes_off_the_draining_thread() {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let geoms: Vec<_> = (0..2000)
            .map(|i| {
                let x = f64::from(i % 50) * 0.01;
                let y = f64::from(i / 50) * 0.01;
                Some(geo::Geometry::Point(geo::Point::new(x, y)))
            })
            .collect();
        crate::overview::testutil::write_input(tin.path(), &geoms, true, None);
        let tout = tempfile::NamedTempFile::new().unwrap();
        let options = crate::overview::convert::ConvertOptions {
            mode: Mode::Duplicating,
            levels: crate::overview::convert::LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            profile: MemoryProfile::Bounded,
            ..Default::default()
        };
        DECODED_HERE.with(|c| c.set(0));
        crate::overview::convert::convert_to_overviews(tin.path(), tout.path(), &options).unwrap();
        assert_eq!(DECODED_HERE.with(std::cell::Cell::get), 0);
    }

    /// #817 B1: the drain widens only on what the read held, not its grants,
    /// which overstate it. A read of a few KiB buys no slot.
    #[test]
    fn drain_widens_only_on_what_the_read_held() {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let geoms: Vec<_> = (0..2000)
            .map(|i| {
                Some(geo::Geometry::Point(geo::Point::new(
                    f64::from(i % 50) * 0.01,
                    f64::from(i / 50) * 0.01,
                )))
            })
            .collect();
        crate::overview::testutil::write_input(tin.path(), &geoms, true, None);
        let tout = tempfile::NamedTempFile::new().unwrap();
        let options = crate::overview::convert::ConvertOptions {
            mode: Mode::Duplicating,
            levels: crate::overview::convert::LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            profile: MemoryProfile::Bounded,
            ..Default::default()
        };
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(16)
            .build()
            .unwrap();
        let width = pool.install(|| {
            DRAIN_WIDTH.with(|c| c.set(0));
            crate::overview::convert::convert_to_overviews(tin.path(), tout.path(), &options)
                .unwrap();
            DRAIN_WIDTH.with(std::cell::Cell::get)
        });
        assert_eq!(
            width,
            crate::overview::memory::ENCODE_FLOOR,
            "drain width {width}"
        );
    }

    /// Codex review C3: one buffered level has no other level to share its
    /// rows with, so it keeps owned batches: shared, the drain only copied
    /// them a second time. Two or more share.
    #[test]
    fn only_two_or_more_buffered_levels_share_rows() {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let geoms: Vec<_> = (0..2000)
            .map(|i| {
                Some(geo::Geometry::Point(geo::Point::new(
                    f64::from(i % 50) * 0.01,
                    f64::from(i / 50) * 0.01,
                )))
            })
            .collect();
        crate::overview::testutil::write_input(tin.path(), &geoms, true, None);
        let sharing = |min_zoom, max_zoom| {
            let tout = tempfile::NamedTempFile::new().unwrap();
            let options = crate::overview::convert::ConvertOptions {
                mode: Mode::Duplicating,
                levels: crate::overview::convert::LevelPlan::ZoomRange { min_zoom, max_zoom },
                profile: MemoryProfile::Speed,
                ..Default::default()
            };
            SHARING.with(|c| c.set(None));
            let meta =
                crate::overview::convert::convert_to_overviews(tin.path(), tout.path(), &options)
                    .unwrap();
            (meta.levels.len(), SHARING.with(std::cell::Cell::get))
        };
        assert_eq!(sharing(7, 8), (2, Some(false)));
        assert_eq!(sharing(2, 8), (7, Some(true)));
    }

    /// #791 item 3: `auto`'s sink tuner arms from what the budget has left
    /// after the read-ahead and window grants, not from the copy taken before
    /// them, so a RAM sink cannot run past the budget by the grants.
    #[test]
    fn the_sink_tuner_arms_from_the_budget_left_after_the_read_grants() {
        let tin = tempfile::NamedTempFile::new().unwrap();
        let geoms: Vec<_> = (0..2000)
            .map(|i| {
                Some(geo::Geometry::Point(geo::Point::new(
                    f64::from(i % 50) * 0.01,
                    f64::from(i / 50) * 0.01,
                )))
            })
            .collect();
        crate::overview::testutil::write_input(tin.path(), &geoms, true, None);
        let tout = tempfile::NamedTempFile::new().unwrap();
        let options = crate::overview::convert::ConvertOptions {
            mode: Mode::Duplicating,
            levels: crate::overview::convert::LevelPlan::ZoomRange {
                min_zoom: 2,
                max_zoom: 8,
            },
            profile: MemoryProfile::Auto,
            ..Default::default()
        };
        TUNER_ARMING.with(|c| c.set((0, 0, 0)));
        crate::overview::convert::convert_to_overviews(tin.path(), tout.path(), &options).unwrap();
        let (before, granted, armed) = TUNER_ARMING.with(std::cell::Cell::get);
        assert!(granted > 0, "the read holds a grant");
        assert_eq!(armed, before - granted);
    }

    pub(super) fn record_spill(temp: &NamedTempFile, rows: u64) {
        if let Some(observations) = OBSERVED
            .lock()
            .unwrap()
            .get_mut(temp.path().parent().unwrap())
        {
            observations.push((rows, temp.as_file().metadata().unwrap().len()));
        }
    }

    fn measure_verbatim_spill(dir: &Path) -> Vec<(u64, u64)> {
        use super::super::convert::{
            convert_to_overviews, ConvertOptions, GeneralizationOverrides, LevelPlan,
        };
        use geo::{Geometry, LineString, Polygon};

        const ROWS: usize = 128;
        let input = dir.join("input.parquet");
        let output = dir.join("output.parquet");
        let geoms: Vec<_> = (0..ROWS)
            .map(|i| {
                let x = (i % 16) as f64 * 0.1;
                let y = (i / 16) as f64 * 0.1;
                Some(Geometry::Polygon(Polygon::new(
                    LineString::from(vec![
                        (x, y),
                        (x + 0.01, y),
                        (x + 0.01, y + 0.01),
                        (x, y + 0.01),
                        (x, y),
                    ]),
                    vec![],
                )))
            })
            .collect();
        super::super::testutil::write_input(&input, &geoms, false, None);
        let options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 9,
                max_zoom: 13,
            },
            profile: MemoryProfile::Bounded,
            spill_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
        .with_generalization(&GeneralizationOverrides::default(), true)
        .unwrap();
        OBSERVED
            .lock()
            .unwrap()
            .insert(dir.to_path_buf(), Vec::new());
        let report = convert_to_overviews(&input, &output, &options).unwrap();
        assert_eq!(report.levels.len(), 5);
        assert!(report
            .levels
            .iter()
            .all(|level| level.feature_count == ROWS));
        let observations = OBSERVED.lock().unwrap().remove(dir).unwrap();
        assert!(
            !observations.is_empty(),
            "bounded workload must exercise spill"
        );
        assert!(observations.iter().all(|&(_, bytes)| bytes > 0));
        eprintln!(
            "phase1 verbatim: rows/files={observations:?}, output_bytes={}",
            std::fs::metadata(output).unwrap().len()
        );
        observations
    }

    /// #715: a fixed compressible batch must not consume its full raw payload
    /// on disk. The bound leaves room for IPC framing and either candidate
    /// encoding; it is independent of elapsed time and allocator capacity.
    #[test]
    fn phase1_spill_bytes_per_level_are_below_raw_payload() {
        let dir = tempfile::tempdir().unwrap();
        let s = schema("id");
        let mut sink =
            SpillState::with_codec(&s, Some(dir.path()), spill_codec(MemoryProfile::Bounded))
                .unwrap();
        const ROWS: usize = 65536;
        sink.push(batch(&s, vec![0; ROWS])).unwrap();
        let (reader, temp) = sink.into_reader(&Pass2Timers::default()).unwrap();
        let disk_bytes = temp.as_file().metadata().unwrap().len();
        assert_eq!(reader.map(|b| b.unwrap().num_rows()).sum::<usize>(), ROWS);
        assert!(
            disk_bytes <= (ROWS * size_of::<i64>() / 2) as u64,
            "#715: one level wrote {disk_bytes} disk bytes for {} raw bytes",
            ROWS * size_of::<i64>()
        );
    }

    /// Only `bounded` spills LZ4 (#715); `auto` spills raw, up front or in
    /// the mid-pass downgrade ([`SpillState::new`]), since LZ4 cost its
    /// pass 2 6-9 s (#815). The reservation prices each codec's bytes.
    #[test]
    fn only_bounded_spills_lz4() {
        use MemoryProfile::{Auto, Bounded, Speed};
        assert_eq!(spill_codec(Bounded), IpcCompression::Lz4);
        assert_eq!(spill_codec(Auto), IpcCompression::None);
        assert_eq!(spill_codec(Speed), IpcCompression::None);
        assert_eq!(spill_bytes_per_arrow_byte(IpcCompression::None), 1.0);
        assert!(spill_bytes_per_arrow_byte(IpcCompression::Lz4) < 1.0);

        let dir = tempfile::tempdir().unwrap();
        let schema = schema("id");
        let mut sink = SpillState::new(&schema, Some(dir.path())).unwrap();
        const ROWS: usize = 65536;
        sink.push(batch(&schema, vec![0; ROWS])).unwrap();
        let (reader, temp) = sink.into_reader(&Pass2Timers::default()).unwrap();
        assert_eq!(reader.map(|b| b.unwrap().num_rows()).sum::<usize>(), ROWS);
        let disk_bytes = temp.as_file().metadata().unwrap().len();
        assert!(
            disk_bytes >= (ROWS * size_of::<i64>()) as u64,
            "a downgrade spill is raw: {disk_bytes} disk bytes for {} raw bytes",
            ROWS * size_of::<i64>()
        );
    }

    /// #815: `auto` sizes its other buffers as `bounded` once it spills, and
    /// only then.
    #[test]
    fn auto_sizes_as_bounded_only_once_it_spills() {
        use MemoryProfile::{Auto, Bounded, Speed};
        assert_eq!(sizing_profile(Auto, SinkBacking::Spill), Bounded);
        assert_eq!(sizing_profile(Auto, SinkBacking::Ram), Auto);
        assert_eq!(sizing_profile(Speed, SinkBacking::Ram), Speed);
        assert_eq!(sizing_profile(Bounded, SinkBacking::Spill), Bounded);
    }

    /// The current preflight ignores even a measured pass-2 footprint for a
    /// local source. The injected free-space probe avoids filling a volume.
    #[test]
    fn phase1_local_preflight_covers_actual_pass2_spill() {
        let dir = tempfile::tempdir().unwrap();
        let observations = measure_verbatim_spill(dir.path());
        let actual_bytes: u64 = observations.iter().map(|&(_, bytes)| bytes).sum();
        let available = actual_bytes / 2;
        let disk = measured_disk_footprint(dir.path());
        assert!(
            disk.spill_bytes() >= actual_bytes,
            "the pass-2 component must cover the measured spill"
        );
        let warning = disk.warning_with(super::super::tiles::CLI_REMEDIES, |_| {
            Some(("one volume".into(), available))
        });
        assert!(warning.is_some(),
            "local preflight returned no warning for {actual_bytes} measured spill bytes with only {available} bytes available");
    }

    fn measured_disk_footprint(dir: &Path) -> super::super::disk::DiskFootprint {
        use super::super::convert::{ConvertOptions, LevelPlan};
        let source = ConvertSource::resolve_path(&dir.join("input.parquet")).unwrap();
        let options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 9,
                max_zoom: 13,
            },
            profile: MemoryProfile::Bounded,
            spill_dir: Some(dir.to_path_buf()),
            ..Default::default()
        };
        super::super::disk::DiskFootprint::conversion(
            &source,
            None,
            &options,
            Some(&dir.join("output.parquet")),
        )
        .unwrap()
    }

    /// Bound pessimism on the fixed workload even after encoding changes.
    /// The estimate uses raw sizing and must cover actual bytes without a
    /// compression assumption. Its permitted overestimate is at most 32x.
    #[test]
    fn disk_estimate_covers_fixed_spill_within_factor_32() {
        let dir = tempfile::tempdir().unwrap();
        let observations = measure_verbatim_spill(dir.path());
        let actual: u64 = observations.iter().map(|&(_, bytes)| bytes).sum();
        let estimate = measured_disk_footprint(dir.path()).spill_bytes();
        eprintln!("raw disk estimate={estimate}, actual spill={actual}");
        assert!(estimate >= actual, "estimate {estimate} < actual {actual}");
        assert!(
            estimate <= actual.saturating_mul(32),
            "estimate {estimate} > 32 × actual {actual}"
        );
    }

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
        let mut sink = SpillState::new(&s, None).unwrap();
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

    /// #709's error path, using the real OS error instead of a string stand-in.
    /// Both private encodings preserve the OS cause and remove the temp file.
    #[cfg(unix)]
    #[test]
    fn phase1_spill_enospc_preserves_the_os_error() {
        struct FullVolume(FailsAfter);
        impl std::io::Write for FullVolume {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0
                    .write(buf)
                    .map_err(|_| std::io::Error::from_raw_os_error(28))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let s = schema("id");
        for codec in [IpcCompression::None, IpcCompression::Lz4] {
            let writer = StreamWriter::try_new_with_compression(
                FullVolume(FailsAfter { remaining: 512 }),
                &s,
                codec,
            )
            .unwrap();
            let dir = tempfile::tempdir().unwrap();
            let mut sink =
                SpillState::spawn(writer, spill_temp_file(Some(dir.path())).unwrap()).unwrap();
            // A compressible zero batch might fit on the simulated full volume.
            let ids = (0..4096u64)
                .map(|i| i.wrapping_mul(0x9e37_79b9_7f4a_7c15) as i64)
                .collect();
            sink.push(batch(&s, ids)).unwrap();
            let error = sink.into_reader(&Pass2Timers::default()).unwrap_err();
            assert!(
                error.to_string().contains("No space left on device"),
                "{codec:?}: {error}"
            );
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
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

    /// #427: the pass-2 spill file lives under `spill_dir` when one is given
    /// (the one scratch knob for everything tylertoo puts on disk), and is
    /// removed with the sink.
    #[test]
    fn spill_file_lives_under_spill_dir() {
        let dir = tempfile::tempdir().unwrap();
        let s = schema("id");
        let mut sink = SpillState::new(&s, Some(dir.path())).unwrap();
        sink.push(batch(&s, vec![1, 2, 3])).unwrap();
        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(
            entries.len(),
            1,
            "one spill file under the spill dir: {entries:?}"
        );
        let (mut reader, temp) = sink.into_reader(&Pass2Timers::default()).unwrap();
        assert_eq!(reader.next().unwrap().unwrap().num_rows(), 3);
        drop(reader);
        drop(temp);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            0,
            "spill file removed"
        );
    }

    /// A missing spill dir fails the sink up front (the caller's error
    /// handling), never inside the writer thread.
    #[test]
    fn missing_spill_dir_fails_at_construction() {
        let s = schema("id");
        let err = SpillState::new(&s, Some(std::path::Path::new("/nonexistent/tylertoo-427")))
            .err()
            .expect("a missing spill dir must fail");
        assert!(matches!(err, ConvertError::Io(_)), "{err}");
    }

    /// A sink abandoned without `into_reader` (an error path, or an unwind)
    /// must still join its writer rather than leaving a thread parked on a
    /// receiver that never disconnects. The test body is the assertion: a
    /// `Drop` that forgot to drop the sender first would hang here.
    #[test]
    fn dropping_a_sink_joins_its_writer() {
        let s = schema("id");
        let mut sink = SpillState::new(&s, None).unwrap();
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

    /// #665: on Linux `/proc/meminfo` always answers, so export's dense-tile
    /// refusal always has a figure, and never cgroup `memory.high`, which
    /// throttles rather than kills.
    #[cfg(target_os = "linux")]
    #[test]
    fn probe_oom_memory_limit_reads_the_machine_without_memory_high() {
        let limit = probe_oom_memory_limit().expect("Linux has /proc/meminfo");
        assert!(limit.bytes > 0);
        assert_ne!(limit.source, MemoryLimitSource::CgroupHigh);
    }

    /// ARCH-REVIEW §1: `auto` decides on the run budget's own allowance,
    /// `0.6 × (limit − reserved) − grants`, once pass 2's read-ahead and
    /// window are granted. Re-wrapping `available()` in a fresh budget gave
    /// `0.6 × (limit − reserved − grants)`, 0.4 × grants more, so a sink
    /// sized between the two stayed in RAM past the allowance the grants
    /// were fitted to.
    #[test]
    fn auto_decides_on_the_charged_budgets_allowance() {
        use crate::overview::memory::{MemoryPhase, Price, PriceBasis};
        let mut memory = MemoryBudget::with_available(Some(10 * GIB));
        let _grant = memory.fit(
            MemoryProfile::Speed,
            MemoryPhase::Compute,
            Price::new(GIB, PriceBasis::Model),
            2,
            2,
        );
        let allowance = memory.allowance(MemoryProfile::Auto);
        assert_eq!(allowance, (0.6 * (10 * GIB) as f64) as u64 - 2 * GIB);
        let rewrapped = (0.6 * memory.available().unwrap() as f64) as u64;
        // A duplicating sink of unmeasured rows (8 KiB each) between the two.
        let between = ((allowance + rewrapped) / 2 / 8192) as usize;
        let estimate = memory.sink_estimate(Mode::Duplicating, between, None, None);
        assert!(allowance < estimate && estimate < rewrapped);
        assert!(matches!(
            resolve_backing(
                memory,
                MemoryProfile::Auto,
                Mode::Duplicating,
                between,
                None,
                None,
                None
            ),
            SinkBacking::Spill
        ));
        let under = (allowance / 8192) as usize - 1;
        assert!(matches!(
            resolve_backing(
                memory,
                MemoryProfile::Auto,
                Mode::Duplicating,
                under,
                None,
                None,
                None
            ),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn explicit_profiles_ignore_workload() {
        // Speed is always RAM, Bounded always Spill — regardless of size/mode.
        assert!(matches!(
            resolve_backing(
                MemoryBudget::with_available(None),
                MemoryProfile::Speed,
                Mode::Duplicating,
                10_000_000_000,
                None,
                None,
                None
            ),
            SinkBacking::Ram
        ));
        assert!(matches!(
            resolve_backing(
                MemoryBudget::with_available(None),
                MemoryProfile::Bounded,
                Mode::Duplicating,
                1,
                None,
                None,
                None
            ),
            SinkBacking::Spill
        ));
    }

    /// #852 review F2: levels that share their rows' properties hold each
    /// kept input row once, so an input whose per-output-row price passes the
    /// allowance stays in RAM when its shared price fits. Overture segments
    /// (2.8M input rows, 10.7M buffered, 738 B footer row, 140 B geometry):
    /// 7.4 GiB per output row, 3.8 GiB shared, against a 4 GiB allowance.
    /// `bounded` still spills.
    #[test]
    fn auto_keeps_a_shared_sink_in_ram_that_per_row_pricing_would_spill() {
        let memory = MemoryBudget::with_available(Some(4 * GIB * 10 / 6 + GIB / 10));
        let (rows, input, geom, row) = (10_697_457, 2_815_136, Some(140), Some(738));
        let auto =
            |shared_from| auto_backing(&memory, Mode::Duplicating, rows, shared_from, geom, row);
        assert!(matches!(auto(None), SinkBacking::Spill));
        assert!(matches!(auto(Some(input)), SinkBacking::Ram));
        assert!(matches!(
            sink_backing(
                &memory,
                MemoryProfile::Bounded,
                Mode::Duplicating,
                rows,
                Some(input),
                geom,
                row
            ),
            SinkBacking::Spill
        ));
    }

    #[test]
    fn auto_duplicating_small_stays_in_ram() {
        // A tiny buffered set is safe in RAM on a normal box.
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(Some(54 * GIB)),
                Mode::Duplicating,
                10_000,
                None,
                None,
                None
            ),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn auto_duplicating_large_spills() {
        // The #294 fix: a large duplicating buffered set flips to Spill instead
        // of the old unconditional RAM. 10M rows * 8 KiB = 80 GiB > 0.6*54.
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(Some(54 * GIB)),
                Mode::Duplicating,
                10_000_000,
                None,
                None,
                None
            ),
            SinkBacking::Spill
        ));
    }

    #[test]
    fn auto_decision_scales_with_available_ram() {
        // Identical workload flips on available RAM, not a fixed fraction of it.
        let rows = 5_000_000; // 5M * 8 KiB = 40 GiB estimate
        assert!(
            matches!(
                auto_backing(
                    &MemoryBudget::with_available(Some(4 * GIB)),
                    Mode::Duplicating,
                    rows,
                    None,
                    None,
                    None
                ),
                SinkBacking::Spill
            ),
            "small box must spill"
        );
        assert!(
            matches!(
                auto_backing(
                    &MemoryBudget::with_available(Some(256 * GIB)),
                    Mode::Duplicating,
                    rows,
                    None,
                    None,
                    None
                ),
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
            auto_backing(
                &MemoryBudget::with_available(Some(10_000 * GIB)),
                Mode::Partitioning,
                3_000_000,
                None,
                None,
                None
            ),
            SinkBacking::Spill
        ));
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(Some(54 * GIB)),
                Mode::Partitioning,
                100_000,
                None,
                None,
                None
            ),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn auto_uses_fallback_budget_when_ram_unknown() {
        // Unknown RAM → conservative fixed budget, still spilling huge sets.
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(None),
                Mode::Duplicating,
                100_000_000,
                None,
                None,
                None
            ),
            SinkBacking::Spill
        ));
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(None),
                Mode::Duplicating,
                1_000,
                None,
                None,
                None
            ),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn estimate_scales_with_mode_and_rows() {
        assert_eq!(
            MemoryBudget::with_available(None).sink_estimate(Mode::Duplicating, 1_000, None, None),
            8_192_000
        );
        assert_eq!(
            MemoryBudget::with_available(None).sink_estimate(Mode::Partitioning, 1_000, None, None),
            16_384_000
        );
        // Saturating: no overflow panic on absurd counts.
        assert_eq!(
            MemoryBudget::with_available(None).sink_estimate(
                Mode::Duplicating,
                usize::MAX,
                None,
                None
            ),
            u64::MAX
        );
    }

    // --- #305: pass-1 measured geometry sizes drive the per-row estimate. ---

    #[test]
    fn measured_estimate_is_overhead_plus_geometry_margin() {
        // per_row = the 256 B allocator floor + 2× (duplicating) / 4×
        // (partitioning) the measured average encoded-geometry bytes. #626
        // cut the floor from 4 KiB to 256 B: it no longer stands in for the
        // property columns, which are measured during pass 2 instead.
        assert_eq!(
            MemoryBudget::with_available(None).sink_estimate(
                Mode::Duplicating,
                1_000,
                Some(1_000),
                None
            ),
            2_256_000 // (256 + 2*1000) * 1000
        );
        assert_eq!(
            MemoryBudget::with_available(None).sink_estimate(
                Mode::Partitioning,
                1_000,
                Some(1_000),
                None
            ),
            4_256_000 // (256 + 4*1000) * 1000
        );
        // Saturating with a measurement too.
        assert_eq!(
            MemoryBudget::with_available(None).sink_estimate(
                Mode::Duplicating,
                usize::MAX,
                Some(1),
                None
            ),
            u64::MAX
        );
    }

    #[test]
    fn measured_zero_falls_back_to_constants() {
        // A degenerate measurement (0 bytes/row) is treated as unmeasured.
        assert_eq!(
            MemoryBudget::with_available(None).sink_estimate(
                Mode::Duplicating,
                1_000,
                Some(0),
                None
            ),
            8_192_000
        );
        assert_eq!(
            MemoryBudget::with_available(None).sink_estimate(
                Mode::Partitioning,
                1_000,
                Some(0),
                None
            ),
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
            auto_backing(
                &MemoryBudget::with_available(Some(54 * GIB)),
                Mode::Duplicating,
                5_000_000,
                None,
                None,
                None
            ),
            SinkBacking::Spill
        ));
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(Some(54 * GIB)),
                Mode::Duplicating,
                5_000_000,
                None,
                Some(100),
                None
            ),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn measured_huge_geometry_spills_where_constant_kept_ram() {
        // 1M duplicating rows, 25 GiB available (budget 15 GiB): the constant
        // (8 KiB/row → 7.6 GiB) keeps RAM, but an adm4-like measured average
        // (11 534 B/row → ~27 KiB/row → 25 GiB) correctly flips to Spill.
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(Some(25 * GIB)),
                Mode::Duplicating,
                1_000_000,
                None,
                None,
                None
            ),
            SinkBacking::Ram
        ));
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(Some(25 * GIB)),
                Mode::Duplicating,
                1_000_000,
                None,
                Some(11_534),
                None
            ),
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
            auto_backing(
                &MemoryBudget::with_available(Some(20 * GIB)),
                Mode::Partitioning,
                1_000_000,
                None,
                None,
                None
            ),
            SinkBacking::Spill
        ));
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(Some(20 * GIB)),
                Mode::Partitioning,
                1_000_000,
                None,
                Some(983),
                None
            ),
            SinkBacking::Ram
        ));
    }

    #[test]
    fn partitioning_row_ceiling_ignores_measurement() {
        // The historical 2M-row partitioning floor holds even when measurement
        // says the geometries are tiny.
        assert!(matches!(
            auto_backing(
                &MemoryBudget::with_available(Some(10_000 * GIB)),
                Mode::Partitioning,
                3_000_000,
                None,
                Some(8),
                None
            ),
            SinkBacking::Spill
        ));
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use crate::overview::level::gsd;
    use crate::overview::writer::{LevelSpec, OverviewWriterOptions};
    use arrow_array::Int64Array;
    use arrow_schema::{DataType, Field};
    use geoarrow::array::GeometryBuilder;
    use geoarrow::datatypes::GeometryType;
    use geoarrow_array::GeoArrowArray;

    /// #852 review F3: a level batch that fails to rebuild in the drain ends
    /// its level unwritten. The level was finished short before, with the
    /// rows rebuilt ahead of the failure, and the file could still be
    /// finished.
    #[test]
    fn a_failed_rebuild_leaves_no_finished_level() {
        let points: Vec<geo::Geometry> = (0..4)
            .map(|i| geo::Geometry::Point(geo::Point::new(f64::from(i), 0.0)))
            .collect();
        let mut builder =
            GeometryBuilder::new(GeometryType::new(Default::default())).with_prefer_multi(false);
        builder.extend_from_iter(points.iter().map(Some));
        let geometry = builder.finish();
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            geometry.data_type().to_field("geometry", true),
        ]));
        let input = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(0..4)),
                geometry.to_array_ref(),
            ],
        )
        .unwrap();
        // A shared batch whose schema has a column too many cannot rebuild.
        let (rows, mut index) =
            SharedRows::new(&input, &[0], &[&[0, 1, 2, 3]], input.column(1).clone()).unwrap();
        let mut fields = schema.fields().to_vec();
        fields.push(Arc::new(Field::new("extra", DataType::Int64, false)));
        let broken = LevelBatch::shared(
            Arc::new(Schema::new(fields)),
            &Arc::new(rows),
            index.remove(0),
            LevelGeometry::Pool(UInt32Array::from_iter_values(0..4)),
            1,
            Vec::new(),
        );
        let sink = LevelSink::Ram(vec![
            LevelBatch::Owned(input.clone()),
            broken,
            LevelBatch::Owned(input),
        ]);
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let options =
            OverviewWriterOptions::new(Mode::Duplicating, vec![LevelSpec::new(gsd(2), Some(2))]);
        let mut writer = OverviewWriter::create(tmp.path(), &schema, options).unwrap();
        let drained = drain_sink(&mut writer, 0, 12, sink, 1, &Pass2Timers::default());
        assert!(drained.is_err());
        assert!(writer.finish().is_err(), "the short level was finished");
    }
}

/// #626: the run-time sink measurement and the one-way Ram→Spill downgrade.
///
/// The budget is passed in rather than probed ([`SinkAutoTuner::with_budget`])
/// so these never read `/proc`, never mutate the process environment, and
/// never depend on the box. `auto_budget_bytes(Some(n))` is exactly the budget
/// `TYLERTOO_AUTO_MEM_LIMIT_BYTES=n` yields.
#[cfg(test)]
mod sink_sample_tests {
    use super::*;
    use arrow_array::{ArrayRef, BinaryArray, Float64Array};
    use arrow_schema::{DataType, Field};
    use std::sync::Arc;

    /// Rows per synthetic batch. Comfortably over [`SINK_SAMPLE_MIN_ROWS`], so
    /// one batch is already a decision-grade sample.
    const ROWS: usize = 4_096;

    /// A batch of `ROWS` rows: one WKB-ish geometry column of 8-byte values
    /// plus `props` `Float64` property columns — the shape a buffered output
    /// batch has (geometry + every KEPT property column).
    fn sample_batch(props: usize) -> RecordBatch {
        let mut fields: Vec<Field> = vec![Field::new("geometry", DataType::Binary, false)];
        let geom: ArrayRef = Arc::new(BinaryArray::from_iter_values(
            (0..ROWS).map(|i| (i as u64).to_le_bytes()),
        ));
        let mut cols: Vec<ArrayRef> = vec![geom];
        for p in 0..props {
            fields.push(Field::new(format!("p{p}"), DataType::Float64, false));
            cols.push(Arc::new(Float64Array::from_iter_values(
                (0..ROWS).map(|i| i as f64),
            )));
        }
        RecordBatch::try_new(Arc::new(Schema::new(fields)), cols).unwrap()
    }

    /// The measured DATA term alone (floor removed), so the two shapes are
    /// compared on what the estimate used to guess at.
    fn measured_column_bytes_per_row(props: usize) -> u64 {
        let mut tuner = SinkAutoTuner::with_budget(false, ROWS, 0);
        tuner.observe(&LevelBatch::Owned(sample_batch(props)));
        tuner.bytes_per_row().unwrap() - SINK_ROW_ALLOC_OVERHEAD_BYTES
    }

    /// The estimator's whole point (#626): property width is the term the
    /// 4 KiB constant never saw. 200 float columns must measure orders of
    /// magnitude heavier per row than the geometry column on its own.
    #[test]
    fn wide_properties_measure_far_heavier_than_geometry_alone() {
        let geometry_only = measured_column_bytes_per_row(0);
        let wide = measured_column_bytes_per_row(200);
        assert!(
            wide >= geometry_only.saturating_mul(100),
            "200 float property columns measured {wide} B/row vs {geometry_only} B/row for \
             geometry alone — under 100×, so the sample is not seeing the properties"
        );
    }

    /// #626 case 1 (under-estimate): a wide-property run that the up-front
    /// estimate left in RAM must trip the downgrade once the sample lands.
    ///
    /// The budget is the one `TYLERTOO_AUTO_MEM_LIMIT_BYTES=2_000_000_000`
    /// produces; 200 float columns over 50 M planned buffered rows is ~80 GiB,
    /// far past it.
    #[test]
    fn wide_properties_trip_the_downgrade_under_a_small_limit() {
        let budget =
            MemoryBudget::with_available(Some(2_000_000_000)).allowance(MemoryProfile::Auto);
        let mut tuner = SinkAutoTuner::with_budget(true, 50_000_000, budget);
        assert!(
            !tuner.should_downgrade(),
            "nothing sampled yet — a tuner must not decide on zero rows"
        );
        tuner.observe(&LevelBatch::Owned(sample_batch(200)));
        assert!(
            tuner.should_downgrade(),
            "measured {:?} B/row over {} planned rows projects {} B against a {budget} B budget",
            tuner.bytes_per_row(),
            tuner.planned_rows,
            tuner.projected_bytes(),
        );
    }

    /// #626 case 2 (over-estimate): a two-property input must NOT spill under
    /// a generous limit. Under the old 4 KiB-per-row proxy the same 50 M rows
    /// modelled ~191 GiB and spilled; measured, they are ~1 GiB.
    #[test]
    fn two_properties_stay_in_ram_under_a_generous_limit() {
        let budget = MemoryBudget::with_available(Some(64 * 1024 * 1024 * 1024))
            .allowance(MemoryProfile::Auto);
        let mut tuner = SinkAutoTuner::with_budget(true, 50_000_000, budget);
        tuner.observe(&LevelBatch::Owned(sample_batch(2)));
        assert!(
            !tuner.should_downgrade(),
            "measured {:?} B/row projects {} B — must fit the {budget} B budget",
            tuner.bytes_per_row(),
            tuner.projected_bytes(),
        );
        // And the old constant-based model is what it is being contrasted
        // with: 4 KiB/row would have blown the same budget.
        assert!(50_000_000u64 * 4_096 > budget);
    }

    /// #788: grants can use the whole allowance before pass 2 starts. In
    /// `sink_downgrade.rs`'s skewed fixture under a 180 MB limit, the
    /// writer's encode floor alone is 4 x 250.6 MB. A saturated allowance
    /// never leaves a RAM sink unguarded: `auto` spills up front, and a
    /// RAM sink armed against a zero allowance downgrades on its first
    /// sample.
    #[test]
    fn a_saturated_allowance_spills_and_never_disarms() {
        use crate::overview::memory::{MemoryPhase, Price, PriceBasis};
        let mut memory = MemoryBudget::with_available(Some(180_000_000));
        let _encodes = memory.fit(
            MemoryProfile::Auto,
            MemoryPhase::Encode,
            Price::new(250_600_000, PriceBasis::Footer),
            64,
            4,
        );
        assert_eq!(memory.allowance(MemoryProfile::Auto), 0);
        assert!(matches!(
            resolve_backing(
                memory,
                MemoryProfile::Auto,
                Mode::Duplicating,
                59_689,
                None,
                Some(26),
                None
            ),
            SinkBacking::Spill
        ));
        let mut tuner = SinkAutoTuner::new(MemoryProfile::Auto, SinkBacking::Ram, ROWS, memory);
        tuner.observe(&LevelBatch::Owned(sample_batch(0)));
        assert!(tuner.should_downgrade(), "a zero allowance must downgrade");
    }

    /// `bounded` and `speed` are instructions, not estimates: the tuner still
    /// measures (the profile dump wants the number) but can never act.
    #[test]
    fn explicit_profiles_are_never_downgraded() {
        for (profile, backing) in [
            (MemoryProfile::Speed, SinkBacking::Ram),
            (MemoryProfile::Bounded, SinkBacking::Spill),
        ] {
            let mut tuner = SinkAutoTuner::new(
                profile,
                backing,
                usize::MAX,
                MemoryBudget::with_available(None),
            );
            tuner.observe(&LevelBatch::Owned(sample_batch(200)));
            assert!(
                tuner.bytes_per_row().is_some(),
                "{profile:?} must still be measured for the profile dump"
            );
            assert!(
                !tuner.should_downgrade(),
                "{profile:?} must never be second-guessed by the sample"
            );
        }
    }

    /// The measurement has no window: batches far into the pass still move
    /// the rate, so nothing seen late is ignored.
    #[test]
    fn the_rate_keeps_moving_for_the_whole_pass() {
        let mut tuner = SinkAutoTuner::with_budget(false, ROWS, 0);
        for _ in 0..1_000 {
            tuner.observe(&LevelBatch::Owned(sample_batch(1)));
        }
        let before = tuner.bytes_per_row().unwrap();
        tuner.observe(&LevelBatch::Owned(sample_batch(200)));
        assert!(
            tuner.bytes_per_row().unwrap() > before,
            "a heavy batch after 1000 light ones must raise the rate"
        );
        assert_eq!(tuner.rows, 1_001 * ROWS);
    }

    /// A spatially sorted input can put its sparse rows first. A light head
    /// that fits the budget must not vouch for a heavy tail that does not:
    /// the check runs for the whole pass, so the tail trips the downgrade.
    #[test]
    fn a_light_head_does_not_hide_a_heavy_tail() {
        const HEAD: usize = 64;
        const TAIL: usize = 64;
        let planned = (HEAD + TAIL) * ROWS;
        // Between what the light head projects (~0.15 GB) and what the heavy
        // tail alone buffers (~0.4 GB).
        let budget = 300_000_000;
        let mut tuner = SinkAutoTuner::with_budget(true, planned, budget);
        for _ in 0..HEAD {
            tuner.observe(&LevelBatch::Owned(sample_batch(0)));
            assert!(
                !tuner.should_downgrade(),
                "the light head projects {} B — under the {budget} B budget",
                tuner.projected_bytes(),
            );
        }
        let mut fired = false;
        for _ in 0..TAIL {
            tuner.observe(&LevelBatch::Owned(sample_batch(200)));
            if tuner.should_downgrade() {
                fired = true;
                break;
            }
        }
        assert!(
            fired,
            "a heavy tail buffering past the {budget} B budget must downgrade; last \
             projection {} B over {} observed rows",
            tuner.projected_bytes(),
            tuner.rows,
        );
    }

    /// The bytes actually buffered are a floor on the projection: when the
    /// plan under-counts the rows, what is already held still trips the
    /// downgrade once it crosses the budget.
    #[test]
    fn bytes_already_buffered_past_the_budget_always_downgrade() {
        let budget = 10_000_000;
        // The plan claims one batch; the run delivers many.
        let mut tuner = SinkAutoTuner::with_budget(true, ROWS, budget);
        let mut observed = 0u64;
        while observed <= budget {
            let b = sample_batch(200);
            observed += b.get_array_memory_size() as u64;
            tuner.observe(&LevelBatch::Owned(b));
        }
        assert!(
            tuner.should_downgrade(),
            "{observed} B already buffered is past the {budget} B budget, but the \
             projection says {} B",
            tuner.projected_bytes(),
        );
    }

    /// The load-bearing safety property (#626 item 2): a downgrade must not
    /// lose a row. Everything buffered before the flip, plus everything
    /// pushed after it, must come back out of the spill file — in order.
    #[test]
    fn a_mid_stream_downgrade_keeps_every_buffered_row() {
        let schema = Schema::new(vec![Field::new("p0", DataType::Float64, false)]);
        let batch = |base: f64| {
            RecordBatch::try_new(
                Arc::new(schema.clone()),
                vec![Arc::new(Float64Array::from(vec![base, base + 1.0])) as ArrayRef],
            )
            .unwrap()
        };
        let mut sink = LevelSink::new(SinkBacking::Ram, &schema, None).unwrap();
        for i in 0..10 {
            sink.push(LevelBatch::Owned(batch(i as f64 * 10.0)))
                .unwrap();
        }
        let flushed = sink.downgrade_to_spill(&schema, None).unwrap();
        assert!(flushed > 0, "the flush must account for the bytes it moved");
        assert!(matches!(sink, LevelSink::Spill(_)), "it is one-way");
        for i in 10..20 {
            sink.push(LevelBatch::Owned(batch(i as f64 * 10.0)))
                .unwrap();
        }

        let LevelSink::Spill(state) = sink else {
            unreachable!("just asserted")
        };
        let (reader, _temp) = state.into_reader(&Pass2Timers::default()).unwrap();
        let got: Vec<f64> = reader
            .flat_map(|b| {
                b.unwrap()
                    .column(0)
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .unwrap()
                    .values()
                    .to_vec()
            })
            .collect();
        let want: Vec<f64> = (0..20)
            .flat_map(|i| [i as f64 * 10.0, i as f64 * 10.0 + 1.0])
            .collect();
        assert_eq!(got, want, "a downgrade must preserve every row, in order");
    }

    /// #852: the levels built from one input batch hold its properties once
    /// between them, and each rebuilds exactly the batch it would have
    /// copied: the input's columns at its rows, its own geometry in place.
    #[test]
    fn levels_hold_one_input_batchs_properties_once() {
        let n = 1_000usize;
        let schema = Arc::new(Schema::new(vec![
            Field::new("name", DataType::Utf8, false),
            Field::new("geometry", DataType::Float64, false),
            Field::new("id", DataType::Float64, false),
        ]));
        let name: ArrayRef = Arc::new(arrow_array::StringArray::from_iter_values(
            (0..n).map(|i| format!("{i:0>200}")),
        ));
        let input = RecordBatch::try_new(
            schema.clone(),
            vec![
                name,
                Arc::new(Float64Array::from_iter_values((0..n).map(|i| -(i as f64)))),
                Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64))),
            ],
        )
        .unwrap();
        // Nested selections, finest first, as duplicating levels keep rows.
        let levels: Vec<Vec<usize>> = (0..4)
            .map(|l| (0..n).filter(|i| i % 10 >= l).collect())
            .collect();
        // Every level keeps each row's geometry unchanged: one pooled copy.
        let pool: ArrayRef = Arc::new(Float64Array::from_iter_values(
            (0..n).map(|i| i as f64 * 0.5),
        ));
        let kept: Vec<&[usize]> = levels.iter().map(Vec::as_slice).collect();
        let (rows, index) = SharedRows::new(&input, &[0, 2], &kept, pool.clone()).unwrap();
        let rows = Arc::new(rows);
        let mut held = rows.held_bytes();
        let per_level: Vec<Option<(LevelBatch, usize)>> = levels
            .iter()
            .zip(index)
            .map(|(kept, index)| {
                let at = UInt32Array::from_iter_values(kept.iter().map(|&i| i as u32));
                let level = LevelBatch::shared(
                    schema.clone(),
                    &rows,
                    index,
                    LevelGeometry::Pool(at),
                    1,
                    Vec::new(),
                );
                held += level.held_bytes();
                Some((level, 0))
            })
            .collect();
        // The sink floor is paid once per shared row, not once per level.
        let mut tuner = SinkAutoTuner::with_budget(false, 0, 0);
        tuner.observe_shared(&per_level);
        for (level, _) in per_level.iter().flatten() {
            tuner.observe(level);
        }
        assert_eq!(
            tuner.projected_bytes(),
            held + n as u64 * SINK_ROW_ALLOC_OVERHEAD_BYTES
        );
        let mut copied = 0u64;
        for (kept, (level, _)) in levels.iter().zip(per_level.into_iter().flatten()) {
            let at = UInt32Array::from_iter_values(kept.iter().map(|&i| i as u32));
            let geometry = arrow_select::take::take(pool.as_ref(), &at, None).unwrap();
            let built = level.materialize(true).unwrap();
            let idx = UInt32Array::from_iter_values(kept.iter().map(|&i| i as u32));
            let want = RecordBatch::try_new(
                schema.clone(),
                vec![
                    arrow_select::take::take(input.column(0).as_ref(), &idx, None).unwrap(),
                    geometry,
                    arrow_select::take::take(input.column(2).as_ref(), &idx, None).unwrap(),
                ],
            )
            .unwrap();
            assert_eq!(built, want);
            copied += want.get_array_memory_size() as u64;
        }
        // Four levels kept 3.4 copies of the rows; the sink holds one.
        assert!(
            held * 2 < copied,
            "shared levels hold {held} B against {copied} B of copies"
        );
    }

    /// A level batch whose input batch pooled no geometries (each level a
    /// range of one column holding them back to back) rebuilds the batch a
    /// pooled one does, compacted or as a slice, and holds nothing of its
    /// geometry but the shared column, counted once.
    #[test]
    fn ranged_and_pooled_level_geometries_rebuild_the_same_batch() {
        let n = 100usize;
        let schema = Arc::new(Schema::new(vec![
            Field::new("name", DataType::Utf8, false),
            Field::new("geometry", DataType::Float64, false),
            Field::new("id", DataType::Float64, false),
        ]));
        let input = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(arrow_array::StringArray::from_iter_values(
                    (0..n).map(|i| format!("row {i}")),
                )),
                Arc::new(Float64Array::from_iter_values((0..n).map(|i| -(i as f64)))),
                Arc::new(Float64Array::from_iter_values((0..n).map(|i| i as f64))),
            ],
        )
        .unwrap();
        let levels: Vec<Vec<usize>> = (0..3)
            .map(|l| (0..n).filter(|i| i % 4 >= l).collect())
            .collect();
        let kept: Vec<&[usize]> = levels.iter().map(Vec::as_slice).collect();
        let geometry = |i: usize| i as f64 * 0.5;
        let pool: ArrayRef = Arc::new(Float64Array::from_iter_values((0..n).map(geometry)));
        // The same geometries, each level's back to back.
        let column: ArrayRef = Arc::new(Float64Array::from_iter_values(
            levels.iter().flatten().map(|&i| geometry(i)),
        ));
        let (pooled, pooled_index) = SharedRows::new(&input, &[0, 2], &kept, pool).unwrap();
        let (ranged, ranged_index) =
            SharedRows::new(&input, &[0, 2], &kept, column.clone()).unwrap();
        let (pooled, ranged) = (Arc::new(pooled), Arc::new(ranged));
        let mut held = ranged.held_bytes();
        let mut start = 0;
        let mut per_level = Vec::new();
        let mut pooled_levels = Vec::new();
        let ranged_level = |index: &UInt32Array, start: usize, len: usize| {
            LevelBatch::shared(
                schema.clone(),
                &ranged,
                index.clone(),
                LevelGeometry::Range(start, len),
                1,
                Vec::new(),
            )
        };
        let mut sliced = Vec::new();
        for ((rows, pi), ri) in levels.iter().zip(pooled_index).zip(ranged_index) {
            let level = ranged_level(&ri, start, rows.len());
            sliced.push(ranged_level(&ri, start, rows.len()));
            start += rows.len();
            assert_eq!(level.held_bytes(), ri.get_array_memory_size() as u64);
            held += level.held_bytes();
            per_level.push(Some((level, 0)));
            let at = UInt32Array::from_iter_values(rows.iter().map(|&i| i as u32));
            pooled_levels.push(LevelBatch::shared(
                schema.clone(),
                &pooled,
                pi,
                LevelGeometry::Pool(at),
                1,
                Vec::new(),
            ));
        }
        let mut tuner = SinkAutoTuner::with_budget(false, 0, 0);
        tuner.observe_shared(&per_level);
        for (level, _) in per_level.iter().flatten() {
            tuner.observe(level);
        }
        assert_eq!(
            tuner.projected_bytes(),
            held + ranged.rows as u64 * SINK_ROW_ALLOC_OVERHEAD_BYTES
        );
        for (((level, _), sliced), pooled) in per_level
            .into_iter()
            .flatten()
            .zip(sliced)
            .zip(pooled_levels)
        {
            let want = pooled.materialize(true).unwrap();
            let rows = want.num_rows();
            let (sliced, compact) = (
                sliced.materialize(false).unwrap(),
                level.materialize(true).unwrap(),
            );
            assert_eq!(sliced, want);
            assert_eq!(compact, want);
            // The writer's slice reads the shared column in place; a spill's
            // compacted copy owns only the level's geometries.
            let within = |c: &RecordBatch| {
                let at = c.column(1).to_data().buffers()[0].as_ptr() as usize;
                let base = column.to_data().buffers()[0].as_ptr() as usize;
                (base..base + column.len() * 8).contains(&at)
            };
            assert!(within(&sliced) && !within(&compact));
            assert_eq!(compact.column(1).to_data().buffers()[0].len(), rows * 8);
        }
    }

    /// Ranged levels of real geometry (#852) spill only their own rows,
    /// pushed to a spilling sink or flushed by a Ram→Spill downgrade: each
    /// level's spill is the size of its own batch's, not of every level's
    /// geometries a slice of the shared column still reaches, and reads back
    /// as that batch.
    #[test]
    fn ranged_levels_spill_only_their_own_geometries() {
        use geoarrow::array::GeometryBuilder;
        use geoarrow::datatypes::GeometryType;
        use geoarrow_array::GeoArrowArray;
        let column = |geoms: &[geo::Geometry]| {
            let mut b = GeometryBuilder::new(GeometryType::new(Default::default()))
                .with_prefer_multi(false);
            b.extend_from_iter(geoms.iter().map(Some));
            b.finish()
        };
        // A ring of `n` vertices around (x, y).
        let polygon = |x: f64, y: f64, n: usize| {
            let ring: Vec<(f64, f64)> = (0..n)
                .map(|k| {
                    let a = k as f64 / n as f64 * std::f64::consts::TAU;
                    (x + a.cos(), y + a.sin())
                })
                .collect();
            geo::Geometry::Polygon(geo::Polygon::new(ring.into(), vec![]))
        };
        let n = 200usize;
        // Finest first; each coarser level keeps fewer rows, simplified more.
        let levels: Vec<Vec<usize>> = (0..3)
            .map(|l| (0..n).filter(|i| i % 3 >= l).collect())
            .collect();
        let level_geoms: Vec<Vec<geo::Geometry>> = levels
            .iter()
            .enumerate()
            .map(|(l, rows)| {
                rows.iter()
                    .map(|&i| polygon(i as f64, 0.0, 64 >> (2 * l)))
                    .collect()
            })
            .collect();
        let field = column(&level_geoms[0])
            .data_type()
            .to_field("geometry", true);
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            field,
        ]));
        let input = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(arrow_array::Int64Array::from_iter_values(0..n as i64)),
                column(
                    &(0..n)
                        .map(|i| polygon(i as f64, 0.0, 64))
                        .collect::<Vec<_>>(),
                )
                .to_array_ref(),
            ],
        )
        .unwrap();
        let kept: Vec<&[usize]> = levels.iter().map(Vec::as_slice).collect();
        let all: Vec<geo::Geometry> = level_geoms.iter().flatten().cloned().collect();
        let (rows, index) =
            SharedRows::new(&input, &[0], &kept, column(&all).to_array_ref()).unwrap();
        let rows = Arc::new(rows);
        let mut start = 0;
        for ((kept, geoms), index) in levels.iter().zip(&level_geoms).zip(index) {
            let at = start;
            let ranged = || {
                LevelBatch::shared(
                    schema.clone(),
                    &rows,
                    index.clone(),
                    LevelGeometry::Range(at, kept.len()),
                    1,
                    Vec::new(),
                )
            };
            start += kept.len();
            // The batch the level would have built on its own.
            let own = RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(arrow_array::Int64Array::from_iter_values(
                        kept.iter().map(|&i| i as i64),
                    )),
                    column(geoms).to_array_ref(),
                ],
            )
            .unwrap();
            let mut reference = LevelSink::new(SinkBacking::Spill, &schema, None).unwrap();
            reference.push(LevelBatch::Owned(own.clone())).unwrap();
            let mut pushed = LevelSink::new(SinkBacking::Spill, &schema, None).unwrap();
            pushed.push(ranged()).unwrap();
            let mut downgraded = LevelSink::new(SinkBacking::Ram, &schema, None).unwrap();
            downgraded.push(ranged()).unwrap();
            downgraded.downgrade_to_spill(&schema, None).unwrap();
            let spilled = |sink: LevelSink| {
                let LevelSink::Spill(state) = sink else {
                    unreachable!("a spilling sink")
                };
                let (reader, temp) = state.into_reader(&Pass2Timers::default()).unwrap();
                let batches: Vec<RecordBatch> = reader.map(Result::unwrap).collect();
                (batches, temp.as_file().metadata().unwrap().len())
            };
            let (want, want_bytes) = spilled(reference);
            for (how, sink) in [("pushed", pushed), ("downgraded", downgraded)] {
                let (got, bytes) = spilled(sink);
                assert_eq!(got, want, "{how}: reads back as the level's own batch");
                assert_eq!(bytes, want_bytes, "{how}: spills only the level's own rows");
            }
        }
    }

    /// Downgrading an already-spilled sink is a no-op, so the one-way rule
    /// cannot be violated by a second call.
    #[test]
    fn downgrading_a_spilled_sink_is_a_no_op() {
        let schema = Schema::new(vec![Field::new("p0", DataType::Float64, false)]);
        let mut sink = LevelSink::new(SinkBacking::Spill, &schema, None).unwrap();
        assert_eq!(sink.downgrade_to_spill(&schema, None).unwrap(), 0);
        assert!(matches!(sink, LevelSink::Spill(_)));
    }

    /// The reported figure is what the profile dump publishes.
    #[test]
    fn the_report_carries_the_measurement_and_the_verdict() {
        let budget =
            MemoryBudget::with_available(Some(2_000_000_000)).allowance(MemoryProfile::Auto);
        let mut tuner = SinkAutoTuner::with_budget(true, 50_000_000, budget);
        assert_eq!(
            tuner.report(),
            SinkMeasurement {
                budget_bytes: budget,
                ..SinkMeasurement::default()
            },
            "before any sample there is no rate and no verdict"
        );
        tuner.observe(&LevelBatch::Owned(sample_batch(200)));
        tuner.mark_downgraded();
        let report = tuner.report();
        assert_eq!(report.sampled_rows, ROWS);
        assert!(report.bytes_per_row.is_some_and(|b| b > 1_600));
        assert_eq!(report.budget_bytes, budget);
        assert!(report.downgraded);
        assert!(
            !tuner.should_downgrade(),
            "a downgraded tuner is disarmed — the flip is one-way"
        );
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

    /// The env override is not frozen by the cache: set after the cache is
    /// populated it wins, and cleared again the cached figure returns. This
    /// is what made Python's later `TYLERTOO_AUTO_MEM_LIMIT_BYTES` a no-op.
    #[test]
    #[expect(clippy::disallowed_types, reason = "#455: exercises the probe cache")]
    fn override_is_honoured_after_the_cache_is_populated() {
        let cache = std::sync::OnceLock::new();
        assert_eq!(available_memory_bytes_in(&cache, None), cache_value(&cache));
        let machine = *cache.get().unwrap();
        assert_eq!(
            available_memory_bytes_in(&cache, Some(1 << 20)),
            Some(1 << 20)
        );
        assert_eq!(available_memory_bytes_in(&cache, None), machine);
    }

    #[expect(clippy::disallowed_types, reason = "#455: exercises the probe cache")]
    fn cache_value(cache: &std::sync::OnceLock<Option<u64>>) -> Option<u64> {
        *cache.get().unwrap()
    }

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

    /// #543: the preflight probe splits the cgroup figure by source, so the
    /// hard `memory.max` and the throttle `memory.high` stay distinguishable.
    #[test]
    fn preflight_limits_split_v2_max_and_high() {
        let dir = v2_root("/slurm/job_42");
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/memory.max",
            "171798691840\n",
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/slurm/job_42/memory.high",
            "107374182400\n",
        );
        assert_eq!(
            read_cgroup_memory_limits_by_source(dir.path()),
            [
                Some(MemoryLimit {
                    bytes: 160 * GIB,
                    source: MemoryLimitSource::CgroupMax,
                }),
                Some(MemoryLimit {
                    bytes: 100 * GIB,
                    source: MemoryLimitSource::CgroupHigh,
                }),
            ]
        );
        // The auto probe's combined figure is unchanged: the lower of the two.
        assert_eq!(read_cgroup_memory_limit(dir.path()), Some(100 * GIB));
    }

    /// #543: a v1 `memory.limit_in_bytes` is a hard (OOM) limit.
    #[test]
    fn preflight_limits_treat_v1_as_hard() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "proc/self/cgroup", "7:memory:/slurm/job_42\n");
        write_file(
            dir.path(),
            "sys/fs/cgroup/memory/slurm/job_42/memory.limit_in_bytes",
            "171798691840\n",
        );
        assert_eq!(
            read_cgroup_memory_limits_by_source(dir.path()),
            [
                Some(MemoryLimit {
                    bytes: 160 * GIB,
                    source: MemoryLimitSource::CgroupMax,
                }),
                None,
            ]
        );
    }

    /// #543: the binding preflight figure is the smallest, reported with its
    /// source; a hard limit wins a tie.
    #[test]
    fn preflight_binding_limit_is_the_minimum_with_its_source() {
        let l = |bytes, source| Some(MemoryLimit { bytes, source });
        assert_eq!(
            binding_memory_limit([
                l(50, MemoryLimitSource::MemAvailable),
                l(80, MemoryLimitSource::CgroupMax),
                None,
            ]),
            l(50, MemoryLimitSource::MemAvailable)
        );
        assert_eq!(
            binding_memory_limit([
                l(80, MemoryLimitSource::MemAvailable),
                l(80, MemoryLimitSource::CgroupMax),
                l(90, MemoryLimitSource::CgroupHigh),
            ]),
            l(80, MemoryLimitSource::CgroupMax)
        );
        assert_eq!(binding_memory_limit([None, None, None]), None);
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
