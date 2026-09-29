//! `/proc/self/smaps_rollup`: the anonymous vs file-backed split of the
//! resident set (#627).
//!
//! [`super::stream::RssSampler`] (#571) reports a single RSS number per phase.
//! That number cannot answer the question an operator actually has when a
//! Slurm job reports a `MaxRSS` sitting on the cgroup ceiling: *is this
//! process really holding 192 GiB of anonymous memory, or is the accounting
//! counting reclaimable page cache?*
//!
//! `smaps_rollup` answers the first half directly. It is the kernel's
//! pre-summed walk of every mapping in the address space, so it costs one
//! read of a few hundred bytes — cheap enough to take at phase boundaries,
//! unlike `/proc/self/smaps`, which emits one stanza per mapping.
//!
//! The fields we keep:
//!
//! | field | meaning |
//! |---|---|
//! | `Rss` | total resident — the same quantity `memory_stats` reports |
//! | `Anonymous` | resident pages with no file behind them (heap, stacks) |
//! | `Shared_Clean` / `Shared_Dirty` | resident pages mapped by >1 process |
//! | `Private_Clean` / `Private_Dirty` | resident pages mapped by this one |
//! | `Swap` | anonymous pages evicted to swap (not resident, but ours) |
//!
//! `Rss - Anonymous` is the file-backed resident total: pages of some file
//! that this process has *mapped*. Crucially, that is NOT the same as the
//! page cache the process's `read(2)`/`write(2)` traffic left behind — cache
//! for an unmapped file is charged to the cgroup but is in no process's RSS.
//! See `docs/diving-deeper/bounded-memory.md` for how the two numbers
//! interact with what Slurm reports.
//!
//! Everything here is Linux-only: [`read_smaps_rollup`] is `None` on every
//! other platform, and `None` on Linux too when the file is missing or
//! unreadable (it needs `CONFIG_PROC_PAGE_MONITOR`, and some sandboxes hide
//! it). The parser itself is portable so it can be tested everywhere.

/// The subset of `/proc/self/smaps_rollup` worth carrying into the profile
/// report, in KiB exactly as the kernel prints it (no float conversion: these
/// are cross-checked against the file by hand).
///
/// Every field is optional because the set of lines `smaps_rollup` prints has
/// grown over kernel releases and may shrink again in a sandbox; a missing
/// line must degrade to `null` in the report, never to a wrong `0`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct SmapsRollup {
    pub(super) rss_kib: Option<u64>,
    pub(super) anonymous_kib: Option<u64>,
    pub(super) shared_clean_kib: Option<u64>,
    pub(super) shared_dirty_kib: Option<u64>,
    pub(super) private_clean_kib: Option<u64>,
    pub(super) private_dirty_kib: Option<u64>,
    pub(super) swap_kib: Option<u64>,
}

impl SmapsRollup {
    /// Resident pages backed by a mapped file: `Rss - Anonymous`.
    ///
    /// `None` when either input is missing, or (defensively) when `Anonymous`
    /// exceeds `Rss` — the two lines are summed in one pass over the mappings
    /// but are not atomic with respect to a concurrently faulting process, so
    /// a negative difference is possible and is reported as unknown rather
    /// than wrapped.
    pub(super) fn file_backed_kib(&self) -> Option<u64> {
        self.rss_kib?.checked_sub(self.anonymous_kib?)
    }

    /// True when no recognized field was found at all — the signal
    /// [`parse_smaps_rollup`] uses to reject garbage.
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Parse the body of `/proc/self/smaps_rollup`.
///
/// Returns `None` when `text` contains none of the recognized fields, so
/// garbage (or an empty read) is reported as "no breakdown" rather than as a
/// breakdown of zeroes. Unrecognized lines, the leading address-range header,
/// and lines whose value does not parse as a number are skipped individually.
///
/// Matching is on the exact key before the colon, so `Anonymous:` is never
/// confused with `AnonHugePages:`, nor `Rss:` with `RssShmem:`.
pub(super) fn parse_smaps_rollup(text: &str) -> Option<SmapsRollup> {
    let mut out = SmapsRollup::default();
    for line in text.lines() {
        // The header line is `<start>-<end> ---p <offset> <dev> <inode> [rollup]`
        // and has no colon; every field line is `Name:<pad><value> kB`.
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let field = match key.trim() {
            "Rss" => &mut out.rss_kib,
            "Anonymous" => &mut out.anonymous_kib,
            "Shared_Clean" => &mut out.shared_clean_kib,
            "Shared_Dirty" => &mut out.shared_dirty_kib,
            "Private_Clean" => &mut out.private_clean_kib,
            "Private_Dirty" => &mut out.private_dirty_kib,
            "Swap" => &mut out.swap_kib,
            _ => continue,
        };
        // `<spaces><number> kB`. Take the first whitespace-separated token and
        // require it to be a number; the unit suffix is always `kB` in every
        // kernel that prints these, and is not trusted to exist.
        if let Some(value) = rest.split_whitespace().next().and_then(|t| t.parse().ok()) {
            *field = Some(value);
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Read and parse `/proc/self/smaps_rollup`.
///
/// `None` when the file does not exist, cannot be read, or holds nothing this
/// parser recognizes. Never fails the caller: this is diagnostics.
#[cfg(target_os = "linux")]
pub(super) fn read_smaps_rollup() -> Option<SmapsRollup> {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    parse_smaps_rollup(&text)
}

/// Non-Linux stub: there is no `smaps_rollup`, so there is no breakdown.
#[cfg(not(target_os = "linux"))]
pub(super) fn read_smaps_rollup() -> Option<SmapsRollup> {
    None
}

/// The `smaps` JSON object for one phase boundary, or `null` when no
/// breakdown was captured (non-Linux, or an unreadable `smaps_rollup`).
///
/// Values are KiB, matching the kernel's own units so a report can be diffed
/// against a live `cat /proc/<pid>/smaps_rollup` without arithmetic. The
/// derived `file_backed_kib` is `rss_kib - anonymous_kib`.
pub(super) fn smaps_json(rollup: Option<&SmapsRollup>) -> serde_json::Value {
    match rollup {
        Some(r) => serde_json::json!({
            "rss_kib": r.rss_kib,
            "anonymous_kib": r.anonymous_kib,
            "file_backed_kib": r.file_backed_kib(),
            "shared_clean_kib": r.shared_clean_kib,
            "shared_dirty_kib": r.shared_dirty_kib,
            "private_clean_kib": r.private_clean_kib,
            "private_dirty_kib": r.private_dirty_kib,
            "swap_kib": r.swap_kib,
        }),
        None => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `smaps_rollup` body (Linux 5.14, trimmed to the lines a 6.x
    /// kernel also prints), including the address-range header and several
    /// fields we deliberately ignore.
    const REAL: &str = "\
55d4c0000000-7ffd8c7f9000 ---p 00000000 00:00 0                          [rollup]
Rss:             7340032 kB
Pss:             7331840 kB
Pss_Dirty:       7208960 kB
Shared_Clean:       8192 kB
Shared_Dirty:          0 kB
Private_Clean:    122880 kB
Private_Dirty:   7208960 kB
Referenced:      7340032 kB
Anonymous:       7208960 kB
KSM:                   0 kB
LazyFree:              0 kB
AnonHugePages:   4194304 kB
ShmemPmdMapped:        0 kB
FilePmdMapped:         0 kB
Shared_Hugetlb:        0 kB
Private_Hugetlb:       0 kB
Swap:               1024 kB
SwapPss:            1024 kB
Locked:                0 kB
";

    #[test]
    fn parses_every_kept_field_from_a_real_rollup() {
        let got = parse_smaps_rollup(REAL).expect("a real rollup must parse");
        assert_eq!(
            got,
            SmapsRollup {
                rss_kib: Some(7_340_032),
                anonymous_kib: Some(7_208_960),
                shared_clean_kib: Some(8_192),
                shared_dirty_kib: Some(0),
                private_clean_kib: Some(122_880),
                private_dirty_kib: Some(7_208_960),
                swap_kib: Some(1_024),
            }
        );
    }

    #[test]
    fn file_backed_is_rss_minus_anonymous() {
        let got = parse_smaps_rollup(REAL).expect("a real rollup must parse");
        // 7 GiB resident, 6.875 GiB of it anonymous: 128 MiB is mapped file
        // (the binary's text and rodata), which is the whole point of #627.
        assert_eq!(got.file_backed_kib(), Some(131_072));
    }

    /// `Anonymous:` must not be matched by the `AnonHugePages:` line that
    /// follows it, nor `Rss:` by anything that merely starts with `Rss`.
    #[test]
    fn matches_keys_exactly_not_by_prefix() {
        let got = parse_smaps_rollup(
            "AnonHugePages:   4194304 kB\nRssShmem:      99 kB\nRss:      512 kB\n",
        )
        .expect("the exact `Rss:` line must still be found");
        assert_eq!(got.rss_kib, Some(512));
        assert_eq!(got.anonymous_kib, None, "AnonHugePages is not Anonymous");
    }

    /// A kernel (or sandbox) that prints only some of the lines yields a
    /// partial breakdown, not a breakdown of zeroes.
    #[test]
    fn missing_fields_stay_none() {
        let got = parse_smaps_rollup("Rss:   2048 kB\nAnonymous:  1024 kB\n")
            .expect("two known fields are enough");
        assert_eq!(got.rss_kib, Some(2_048));
        assert_eq!(got.anonymous_kib, Some(1_024));
        assert_eq!(got.shared_clean_kib, None);
        assert_eq!(got.private_dirty_kib, None);
        assert_eq!(got.swap_kib, None);
        assert_eq!(got.file_backed_kib(), Some(1_024));
    }

    #[test]
    fn file_backed_is_unknown_when_either_input_is_missing() {
        let rss_only = parse_smaps_rollup("Rss:   2048 kB\n").expect("one known field parses");
        assert_eq!(rss_only.file_backed_kib(), None);
        let anon_only =
            parse_smaps_rollup("Anonymous:   2048 kB\n").expect("one known field parses");
        assert_eq!(anon_only.file_backed_kib(), None);
    }

    /// The two lines are not sampled atomically, so `Anonymous > Rss` is
    /// possible; it must report unknown rather than wrap around `u64`.
    #[test]
    fn file_backed_does_not_wrap_when_anonymous_exceeds_rss() {
        let got = parse_smaps_rollup("Rss:   100 kB\nAnonymous:   200 kB\n").expect("parses");
        assert_eq!(got.file_backed_kib(), None);
    }

    #[test]
    fn garbage_is_rejected() {
        assert_eq!(parse_smaps_rollup(""), None);
        assert_eq!(parse_smaps_rollup("not a proc file at all\n"), None);
        assert_eq!(parse_smaps_rollup("\0\u{1}\u{2}binary junk\n"), None);
        assert_eq!(
            parse_smaps_rollup("Referenced:   7340032 kB\nLocked:  0 kB\n"),
            None,
            "only fields we do not keep is the same as no breakdown"
        );
    }

    /// An unparseable value on a known line is skipped, leaving that field
    /// `None` — it must not abort the whole parse or yield a bogus number.
    #[test]
    fn unparseable_values_are_skipped_not_fatal() {
        let got = parse_smaps_rollup("Rss:   not-a-number kB\nAnonymous:   1024 kB\n")
            .expect("the good line still parses");
        assert_eq!(got.rss_kib, None);
        assert_eq!(got.anonymous_kib, Some(1_024));

        // A negative value is not a `u64`; same treatment.
        let negative = parse_smaps_rollup("Rss:   -1 kB\nSwap:  8 kB\n").expect("Swap parses");
        assert_eq!(negative.rss_kib, None);
        assert_eq!(negative.swap_kib, Some(8));
    }

    /// Tabs, a missing `kB` suffix and a value jammed against the colon are
    /// all tolerated: nothing about this format is load-bearing except the
    /// key and the first token after the colon.
    #[test]
    fn tolerates_whitespace_and_suffix_variations() {
        let got = parse_smaps_rollup("Rss:\t\t4096\nAnonymous:2048 kB\n  Swap:  16 kB\n")
            .expect("parses");
        assert_eq!(got.rss_kib, Some(4_096));
        assert_eq!(got.anonymous_kib, Some(2_048));
        assert_eq!(got.swap_kib, Some(16), "a leading-space key still matches");
    }

    /// A line with no value at all after the colon leaves the field `None`
    /// rather than panicking on the empty token.
    #[test]
    fn empty_value_leaves_the_field_none() {
        let got = parse_smaps_rollup("Rss:\nAnonymous:   1024 kB\n").expect("the good line parses");
        assert_eq!(got.rss_kib, None);
        assert_eq!(got.anonymous_kib, Some(1_024));
    }

    #[test]
    fn json_is_null_without_a_breakdown() {
        assert_eq!(smaps_json(None), serde_json::Value::Null);
    }

    #[test]
    fn json_carries_the_kib_values_and_the_derived_split() {
        let rollup = parse_smaps_rollup(REAL).expect("parses");
        let v = smaps_json(Some(&rollup));
        assert_eq!(v["rss_kib"], 7_340_032);
        assert_eq!(v["anonymous_kib"], 7_208_960);
        assert_eq!(v["file_backed_kib"], 131_072);
        assert_eq!(v["shared_clean_kib"], 8_192);
        assert_eq!(v["shared_dirty_kib"], 0);
        assert_eq!(v["private_clean_kib"], 122_880);
        assert_eq!(v["private_dirty_kib"], 7_208_960);
        assert_eq!(v["swap_kib"], 1_024);
    }

    /// A missing field must serialize as JSON `null`, not be omitted — a
    /// consumer reading `smaps.rss_kib` should see an explicit unknown.
    #[test]
    fn json_renders_missing_fields_as_null() {
        let rollup = parse_smaps_rollup("Anonymous:   1024 kB\n").expect("parses");
        let v = smaps_json(Some(&rollup));
        assert!(v["rss_kib"].is_null());
        assert!(v["file_backed_kib"].is_null());
        assert_eq!(v["anonymous_kib"], 1_024);
        assert!(
            v.as_object().expect("an object").contains_key("rss_kib"),
            "an unknown field must be present-and-null, not absent"
        );
    }

    /// On Linux the real file must be readable and self-consistent; elsewhere
    /// the stub must simply say `None` without touching the filesystem.
    #[test]
    fn reads_the_live_process_on_linux_and_nothing_elsewhere() {
        let got = read_smaps_rollup();
        if cfg!(target_os = "linux") {
            // `smaps_rollup` needs CONFIG_PROC_PAGE_MONITOR; a kernel or
            // sandbox without it legitimately yields `None`.
            if let Some(r) = got {
                let rss = r.rss_kib.expect("a live rollup always prints Rss");
                assert!(rss > 0, "this test process is resident: {r:?}");
                let anon = r.anonymous_kib.expect("a live rollup prints Anonymous");
                assert!(
                    anon <= rss,
                    "anonymous ({anon} KiB) cannot exceed Rss ({rss} KiB) in one read"
                );
            }
        } else {
            assert_eq!(got, None, "there is no smaps_rollup off Linux");
        }
    }
}
