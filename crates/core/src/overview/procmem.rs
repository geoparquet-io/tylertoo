//! What the process's memory is made of, and what its cgroup is charged for
//! (#627).
//!
//! [`super::stream::RssSampler`] (#571) reports a single RSS number per phase.
//! That number cannot answer the question an operator has when a Slurm job
//! reports a `MaxRSS` sitting on the cgroup ceiling: *is this process really
//! holding that much memory, or is the accounting counting page cache?* Two
//! different sources answer the two halves of that question.
//!
//! **The process: `/proc/self/status`.** Its `RssAnon`, `RssFile` and
//! `RssShmem` lines are the kernel's per-process page counters (the same
//! counters `VmRSS` is the sum of), so reading them costs the same whatever
//! the process's size: no page-table walk, no per-mapping output. That makes
//! them cheap enough for the sampler's 250 ms tick, which is what turns a
//! boundary snapshot into a sampled anonymous *peak*.
//!
//! `/proc/self/smaps_rollup` would add a shared/private split, but it walks
//! every page table of the process under the mmap lock — about 55 ms for 4 GiB
//! resident on the machine this was measured on, so seconds at the hundred-GiB
//! scale #627 is about — and its "file-backed" figure has to be derived as
//! `Rss - Anonymous`, which silently counts shared memory as file. The status
//! counters split file and shmem exactly, and nothing in #627 needs the
//! shared/private split.
//!
//! **The cgroup: `memory.current`, `memory.peak`, `memory.stat`.** A process's
//! resident set never contains page cache for a file it did not map, and
//! tylertoo maps no data file, so no per-process figure can show the page
//! cache a job's `read`/`write` traffic leaves behind. The cgroup is charged
//! for it, and under cgroup v2 Slurm's `jobacct_gather/cgroup` reports
//! `memory.current` / `memory.peak` as `MaxRSS`. Only the cgroup's own
//! `memory.stat` (`anon`, `file`, `file_dirty`, `file_writeback`, `shmem`)
//! can say how much of that figure is page cache, how much of the cache is not
//! yet clean, and how much is tmpfs (shmem), which cannot be reclaimed without
//! swap.
//!
//! Everything here is Linux-only and fail-soft: every reader returns `None`
//! when its file is missing or unrecognized (other platforms, cgroup v1,
//! sandboxes), and each field the kernel did not print stays `None`, so a
//! report says `null`, never a misleading `0`. The parsers themselves are
//! portable so they are tested everywhere.

use super::pipeline::{proc_self_cgroup_path, CgroupSelector};

/// The per-process memory counters from `/proc/self/status`, in KiB exactly
/// as the kernel prints them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct ProcStatusMem {
    /// `VmRSS`: the whole resident set (`anon + file + shmem`).
    pub(super) rss_kib: Option<u64>,
    /// `RssAnon`: heap, stacks, anonymous mappings.
    pub(super) anon_kib: Option<u64>,
    /// `RssFile`: resident pages of mapped files (for tylertoo, the binary
    /// and its shared libraries).
    pub(super) file_kib: Option<u64>,
    /// `RssShmem`: resident shared memory (SysV, shared anonymous mappings,
    /// tmpfs files the process has mapped).
    pub(super) shmem_kib: Option<u64>,
    /// `VmSwap`: anonymous pages evicted to swap (not resident, but ours).
    pub(super) swap_kib: Option<u64>,
}

impl ProcStatusMem {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Parse the body of `/proc/self/status`, keeping only the memory lines.
///
/// `None` when no recognized line is present, so garbage or an empty read is
/// "no breakdown" rather than a breakdown of zeroes. Keys match exactly, so
/// `RssAnon:` is never confused with anything that merely starts with it.
pub(super) fn parse_proc_status(text: &str) -> Option<ProcStatusMem> {
    let mut out = ProcStatusMem::default();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let field = match key.trim() {
            "VmRSS" => &mut out.rss_kib,
            "RssAnon" => &mut out.anon_kib,
            "RssFile" => &mut out.file_kib,
            "RssShmem" => &mut out.shmem_kib,
            "VmSwap" => &mut out.swap_kib,
            _ => continue,
        };
        // `<whitespace><number> kB`: the first token must be a number; the
        // unit is always `kB` for these lines and is not relied on.
        if let Some(value) = rest.split_whitespace().next().and_then(|t| t.parse().ok()) {
            *field = Some(value);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Read and parse `/proc/self/status`. `None` off Linux, or when the file is
/// unreadable or holds none of the memory lines. Never fails the caller.
///
/// A runtime `cfg!` rather than a `#[cfg]` pair, so the parser is live code on
/// every platform (no dead-code warning off Linux) and one body is compiled
/// and linted everywhere.
pub(super) fn read_proc_status() -> Option<ProcStatusMem> {
    if cfg!(target_os = "linux") {
        parse_proc_status(&std::fs::read_to_string("/proc/self/status").ok()?)
    } else {
        None
    }
}

/// The `phase_rss_breakdown` JSON object for one phase boundary.
pub(super) fn proc_status_json(m: &ProcStatusMem) -> serde_json::Value {
    serde_json::json!({
        "rss_kib": m.rss_kib,
        "anon_kib": m.anon_kib,
        "file_kib": m.file_kib,
        "shmem_kib": m.shmem_kib,
        "swap_kib": m.swap_kib,
    })
}

/// This process's cgroup v2 memory accounting, in bytes exactly as the kernel
/// prints them (`memory.current`, `memory.peak` and `memory.stat` are all
/// byte counts, unlike `/proc`'s KiB).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct CgroupMem {
    /// `memory.current`: everything charged to the cgroup right now,
    /// including page cache.
    pub(super) current_bytes: Option<u64>,
    /// `memory.peak`: the high-water mark of `memory.current` since the
    /// cgroup was created (kernel 5.19+; `None` before).
    pub(super) peak_bytes: Option<u64>,
    /// `memory.stat anon`: anonymous memory, the cgroup-wide counterpart of
    /// `RssAnon`.
    pub(super) anon_bytes: Option<u64>,
    /// `memory.stat file`: page cache, including tmpfs/shmem pages.
    pub(super) file_bytes: Option<u64>,
    /// `memory.stat file_dirty`: page cache not yet written back.
    pub(super) file_dirty_bytes: Option<u64>,
    /// `memory.stat file_writeback`: page cache being written back now.
    pub(super) file_writeback_bytes: Option<u64>,
    /// `memory.stat shmem`: tmpfs and shared memory — part of `file`, but
    /// not reclaimable without swap.
    pub(super) shmem_bytes: Option<u64>,
}

/// Parse a cgroup v2 `memory.stat` body (`key value` per line) into the stat
/// fields of a [`CgroupMem`]. Keys match exactly (`file` is not `file_dirty`).
/// A key that is absent or unparseable stays `None`.
pub(super) fn parse_memory_stat(text: &str, out: &mut CgroupMem) {
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(key), Some(value)) = (fields.next(), fields.next()) else {
            continue;
        };
        let field = match key {
            "anon" => &mut out.anon_bytes,
            "file" => &mut out.file_bytes,
            "file_dirty" => &mut out.file_dirty_bytes,
            "file_writeback" => &mut out.file_writeback_bytes,
            "shmem" => &mut out.shmem_bytes,
            _ => continue,
        };
        if let Ok(v) = value.parse() {
            *field = Some(v);
        }
    }
}

/// A whole-file byte count (`memory.current`, `memory.peak`).
fn read_u64(path: &std::path::Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// This process's cgroup v2 memory accounting, read under `root` (`/` in
/// production, a tempdir in tests).
///
/// The directory is `<root>/sys/fs/cgroup<path>`, `<path>` being the `0::`
/// line of `/proc/self/cgroup`. `None` — never zeroes — when:
///
/// - there is no `0::` line, or its path tries to escape the mount with `..`;
/// - that directory has no readable `memory.current`. This is how cgroup v1
///   and hybrid hosts are told apart from v2: their `0::` line names a
///   directory of the (controller-less) unified tree, or of a v1 mount, that
///   carries no v2 memory files. It also excludes the host's root cgroup,
///   which has `memory.stat` but no `memory.current` and would describe the
///   whole machine, not this job.
pub(super) fn read_cgroup_mem_under(root: &std::path::Path) -> Option<CgroupMem> {
    let rel = proc_self_cgroup_path(root, CgroupSelector::V2)?;
    let mut dir = root.join("sys/fs/cgroup");
    for component in rel.split('/') {
        match component {
            "" | "." => {}
            ".." => return None,
            c => dir.push(c),
        }
    }
    let mut out = CgroupMem {
        current_bytes: Some(read_u64(&dir.join("memory.current"))?),
        peak_bytes: read_u64(&dir.join("memory.peak")),
        ..CgroupMem::default()
    };
    if let Ok(text) = std::fs::read_to_string(dir.join("memory.stat")) {
        parse_memory_stat(&text, &mut out);
    }
    Some(out)
}

/// This process's cgroup v2 memory accounting. `None` off Linux, on cgroup v1,
/// or wherever the files are hidden.
pub(super) fn read_cgroup_mem() -> Option<CgroupMem> {
    if cfg!(target_os = "linux") {
        read_cgroup_mem_under(std::path::Path::new("/"))
    } else {
        None
    }
}

/// The `phase_cgroup` JSON object for one phase boundary.
pub(super) fn cgroup_json(m: &CgroupMem) -> serde_json::Value {
    serde_json::json!({
        "memory_current_bytes": m.current_bytes,
        "memory_peak_bytes": m.peak_bytes,
        "anon_bytes": m.anon_bytes,
        "file_bytes": m.file_bytes,
        "file_dirty_bytes": m.file_dirty_bytes,
        "file_writeback_bytes": m.file_writeback_bytes,
        "shmem_bytes": m.shmem_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    /// A real `/proc/self/status` body (Linux 6.x), trimmed but keeping lines
    /// that share prefixes with the ones we want.
    const STATUS: &str = "\
Name:\ttylertoo
Umask:\t0022
State:\tR (running)
Pid:\t4242
VmPeak:\t 9437184 kB
VmSize:\t 9437184 kB
VmHWM:\t 7340032 kB
VmRSS:\t 7340032 kB
RssAnon:\t 7208960 kB
RssFile:\t  122880 kB
RssShmem:\t    8192 kB
VmData:\t 7300000 kB
VmSwap:\t    1024 kB
HugetlbPages:\t       0 kB
Threads:\t12
";

    #[test]
    fn parses_every_kept_status_field() {
        assert_eq!(
            parse_proc_status(STATUS),
            Some(ProcStatusMem {
                rss_kib: Some(7_340_032),
                anon_kib: Some(7_208_960),
                file_kib: Some(122_880),
                shmem_kib: Some(8_192),
                swap_kib: Some(1_024),
            })
        );
    }

    /// `VmRSS` is exactly the sum of the three counters: the split is exact,
    /// not derived by subtraction.
    #[test]
    fn status_split_sums_to_vmrss() {
        let m = parse_proc_status(STATUS).expect("parses");
        assert_eq!(
            m.anon_kib.unwrap() + m.file_kib.unwrap() + m.shmem_kib.unwrap(),
            m.rss_kib.unwrap()
        );
    }

    /// `VmHWM`/`VmPeak` must not be taken for `VmRSS`, and a kernel that
    /// predates `RssAnon` (pre-4.5) leaves the split unknown, not zero.
    #[test]
    fn status_keys_match_exactly_and_missing_lines_stay_none() {
        let m = parse_proc_status("VmHWM:\t 999 kB\nVmPeak:\t 999 kB\nVmRSS:\t 512 kB\n")
            .expect("VmRSS alone is a result");
        assert_eq!(m.rss_kib, Some(512));
        assert_eq!(m.anon_kib, None);
        assert_eq!(m.file_kib, None);
        assert_eq!(m.shmem_kib, None);
        assert_eq!(m.swap_kib, None);
    }

    #[test]
    fn status_garbage_is_rejected() {
        assert_eq!(parse_proc_status(""), None);
        assert_eq!(parse_proc_status("Name:\tx\nThreads:\t4\n"), None);
        assert_eq!(parse_proc_status("\0\u{1}junk"), None);
        let m = parse_proc_status("RssAnon:\t-1 kB\nVmSwap:\t8 kB\n").expect("VmSwap parses");
        assert_eq!(m.anon_kib, None, "a negative value is not a u64");
        assert_eq!(m.swap_kib, Some(8));
    }

    #[test]
    fn status_json_renders_missing_fields_as_null() {
        let v = proc_status_json(&parse_proc_status("RssAnon:\t1024 kB\n").unwrap());
        assert_eq!(v["anon_kib"], 1_024);
        for key in ["rss_kib", "file_kib", "shmem_kib", "swap_kib"] {
            assert!(
                v.as_object().unwrap().contains_key(key) && v[key].is_null(),
                "{key} must be present-and-null: {v}"
            );
        }
    }

    #[test]
    fn reads_the_live_status_on_linux_and_nothing_elsewhere() {
        let got = read_proc_status();
        if cfg!(target_os = "linux") {
            let m = got.expect("/proc/self/status exists on every Linux");
            let rss = m
                .rss_kib
                .expect("VmRSS is always printed for a user process");
            assert!(rss > 0);
            // Kernels since 4.5 print the split. The kernel prints `VmRSS` as
            // the sum of the same three counter reads, so it adds up exactly.
            if let (Some(a), Some(f), Some(s)) = (m.anon_kib, m.file_kib, m.shmem_kib) {
                assert!(a > 0, "a running test process has a heap: {m:?}");
                assert_eq!(a + f + s, rss, "the split must sum to VmRSS: {m:?}");
            }
        } else {
            assert_eq!(got, None);
        }
    }

    /// A trimmed real cgroup v2 `memory.stat` (Linux 6.x): note `file` next to
    /// `file_mapped`/`file_dirty`/`file_writeback`/`file_thp`, and `anon` next
    /// to `anon_thp`, which must not be confused.
    const MEMORY_STAT: &str = "\
anon 7516192768
file 193273528320
kernel 402653184
kernel_stack 1048576
pagetables 16777216
sock 0
shmem 4294967296
file_mapped 126976000
file_dirty 2147483648
file_writeback 536870912
swapcached 0
anon_thp 4294967296
file_thp 0
shmem_thp 0
inactive_anon 0
active_anon 7516192768
inactive_file 150000000000
active_file 43273528320
";

    #[test]
    fn parses_every_kept_memory_stat_field() {
        let mut m = CgroupMem::default();
        parse_memory_stat(MEMORY_STAT, &mut m);
        assert_eq!(
            m,
            CgroupMem {
                current_bytes: None,
                peak_bytes: None,
                anon_bytes: Some(7_516_192_768),
                file_bytes: Some(193_273_528_320),
                file_dirty_bytes: Some(2_147_483_648),
                file_writeback_bytes: Some(536_870_912),
                shmem_bytes: Some(4_294_967_296),
            }
        );
    }

    #[test]
    fn memory_stat_missing_keys_stay_none() {
        let mut m = CgroupMem::default();
        parse_memory_stat("anon 4096\nanon_thp 0\nfile_mapped 7\n", &mut m);
        assert_eq!(m.anon_bytes, Some(4_096));
        assert_eq!(m.file_bytes, None, "file_mapped is not file");
        assert_eq!(m.file_dirty_bytes, None);
        assert_eq!(m.shmem_bytes, None);
    }

    fn write_file(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// A fake cgroup v2 root: the `0::` line of a Slurm task cgroup, with its
    /// memory files.
    fn v2_root() -> TempDir {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "proc/self/cgroup",
            "0::/system.slice/slurmstepd.scope/job_42/step_0/user/task_0\n",
        );
        let leaf = "sys/fs/cgroup/system.slice/slurmstepd.scope/job_42/step_0/user/task_0";
        write_file(
            dir.path(),
            &format!("{leaf}/memory.current"),
            "206158430208\n",
        );
        write_file(dir.path(), &format!("{leaf}/memory.peak"), "206158430208\n");
        write_file(dir.path(), &format!("{leaf}/memory.stat"), MEMORY_STAT);
        dir
    }

    #[test]
    fn reads_the_v2_leaf_cgroup() {
        let root = v2_root();
        let m = read_cgroup_mem_under(root.path()).expect("a v2 leaf with memory files");
        assert_eq!(m.current_bytes, Some(206_158_430_208));
        assert_eq!(m.peak_bytes, Some(206_158_430_208));
        assert_eq!(m.anon_bytes, Some(7_516_192_768));
        assert_eq!(m.file_bytes, Some(193_273_528_320));
        assert_eq!(m.file_dirty_bytes, Some(2_147_483_648));
        assert_eq!(m.file_writeback_bytes, Some(536_870_912));
        assert_eq!(m.shmem_bytes, Some(4_294_967_296));
    }

    /// Pre-5.19 kernels have no `memory.peak`, and a restricted cgroup may
    /// hide `memory.stat`: what is missing is `None`, what is there is kept.
    #[test]
    fn v2_missing_peak_and_stat_stay_none() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "proc/self/cgroup", "0::/job\n");
        write_file(dir.path(), "sys/fs/cgroup/job/memory.current", "1048576\n");
        let m = read_cgroup_mem_under(dir.path()).expect("memory.current alone is a result");
        assert_eq!(m.current_bytes, Some(1_048_576));
        assert_eq!(m.peak_bytes, None);
        assert_eq!(m.anon_bytes, None);
        assert_eq!(m.file_bytes, None);
    }

    /// cgroup v1 (and hybrid) hosts: the memory controller has its own line
    /// and its own mount, and the `0::` line (present on hybrid hosts) names a
    /// directory with no v2 memory files. That must read as "no v2 figures",
    /// never as zeroes — even though the v1 directory has a `memory.stat`.
    #[test]
    fn cgroup_v1_and_hybrid_hosts_yield_none() {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "proc/self/cgroup",
            "4:memory:/slurm/uid_1000/job_42\n1:name=systemd:/\n0::/\n",
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/memory/slurm/uid_1000/job_42/memory.usage_in_bytes",
            "4096\n",
        );
        write_file(
            dir.path(),
            "sys/fs/cgroup/memory/slurm/uid_1000/job_42/memory.stat",
            "total_rss 4096\ntotal_cache 0\n",
        );
        assert_eq!(read_cgroup_mem_under(dir.path()), None);

        // Pure v1: no `0::` line at all.
        let v1 = TempDir::new().unwrap();
        write_file(v1.path(), "proc/self/cgroup", "4:memory:/job\n");
        assert_eq!(read_cgroup_mem_under(v1.path()), None);
    }

    /// The host's root cgroup has `memory.stat` (machine-wide) but no
    /// `memory.current`; reporting it would describe the machine, not the job.
    #[test]
    fn host_root_cgroup_without_memory_current_yields_none() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "proc/self/cgroup", "0::/\n");
        write_file(dir.path(), "sys/fs/cgroup/memory.stat", MEMORY_STAT);
        assert_eq!(read_cgroup_mem_under(dir.path()), None);
    }

    #[test]
    fn missing_proc_self_cgroup_or_escaping_path_yields_none() {
        let empty = TempDir::new().unwrap();
        assert_eq!(read_cgroup_mem_under(empty.path()), None);

        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "proc/self/cgroup", "0::/../../etc\n");
        write_file(dir.path(), "etc/memory.current", "1\n");
        write_file(dir.path(), "sys/etc/memory.current", "1\n");
        assert_eq!(read_cgroup_mem_under(dir.path()), None);
    }

    #[test]
    fn cgroup_json_renders_missing_fields_as_null() {
        let v = cgroup_json(&CgroupMem {
            current_bytes: Some(8),
            ..CgroupMem::default()
        });
        assert_eq!(v["memory_current_bytes"], 8);
        for key in [
            "memory_peak_bytes",
            "anon_bytes",
            "file_bytes",
            "file_dirty_bytes",
            "file_writeback_bytes",
            "shmem_bytes",
        ] {
            assert!(
                v.as_object().unwrap().contains_key(key) && v[key].is_null(),
                "{key} must be present-and-null: {v}"
            );
        }
    }
}
