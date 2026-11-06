//! Process CPU time and I/O counters for the profiling sampler (#810).
//!
//! [`super::stream::RssSampler`] reads these at every phase boundary and on
//! its 250 ms tick, but only while `TYLERTOO_PROFILE_JSON` is set.
//!
//! **CPU: `getrusage(RUSAGE_SELF)`.** User and system seconds summed over
//! every thread of the process, on Linux and macOS alike. The same numbers
//! as `/proc/self/stat`'s `utime`/`stime`, without the clock-tick rounding.
//! `None` off Unix.
//!
//! **I/O: `/proc/self/io`.** `rchar` counts every byte `read(2)` returned,
//! page cache included. `read_bytes` and `write_bytes` count what the
//! process made the block layer fetch or dirty, so a warm-cache read shows
//! in `rchar` only. Linux only; `None` elsewhere.

/// The `/proc/self/io` fields the sampler keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ProcIo {
    pub(super) rchar: u64,
    pub(super) read_bytes: u64,
    pub(super) write_bytes: u64,
}

/// Parse `/proc/self/io`. `None` unless all three kept fields are present.
pub(super) fn parse_proc_io(text: &str) -> Option<ProcIo> {
    let (mut rchar, mut read_bytes, mut write_bytes) = (None, None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let slot = match key.trim() {
            "rchar" => &mut rchar,
            "read_bytes" => &mut read_bytes,
            "write_bytes" => &mut write_bytes,
            _ => continue,
        };
        *slot = value.trim().parse().ok();
    }
    Some(ProcIo {
        rchar: rchar?,
        read_bytes: read_bytes?,
        write_bytes: write_bytes?,
    })
}

/// Read `/proc/self/io`. `None` off Linux or when it is unreadable (some
/// hardened kernels and containers hide it).
pub(super) fn read_proc_io() -> Option<ProcIo> {
    if cfg!(target_os = "linux") {
        parse_proc_io(&std::fs::read_to_string("/proc/self/io").ok()?)
    } else {
        None
    }
}

/// `(user, system)` CPU seconds this process has used so far, all threads.
#[cfg(unix)]
pub(super) fn cpu_times() -> Option<(f64, f64)> {
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    // SAFETY: `getrusage` fills the caller-provided `struct rusage`; a
    // zeroed value is a valid initial state for that plain C struct.
    let mut ru = unsafe { std::mem::zeroed::<libc::rusage>() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    (rc == 0).then(|| (secs(ru.ru_utime), secs(ru.ru_stime)))
}

/// `(user, system)` CPU seconds: unknown off Unix.
#[cfg(not(unix))]
pub(super) fn cpu_times() -> Option<(f64, f64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_kept_io_fields() {
        let text = "rchar: 100\nwchar: 7\nsyscr: 3\nsyscw: 1\nread_bytes: 4096\n\
                    write_bytes: 8192\ncancelled_write_bytes: 0\n";
        assert_eq!(
            parse_proc_io(text),
            Some(ProcIo {
                rchar: 100,
                read_bytes: 4096,
                write_bytes: 8192
            })
        );
        assert_eq!(parse_proc_io("rchar: 1\nread_bytes: 2\n"), None);
        assert_eq!(parse_proc_io("garbage"), None);
    }

    #[test]
    fn live_counters_move_forward() {
        if cfg!(unix) {
            let (u0, s0) = cpu_times().expect("getrusage works on unix");
            let mut x = 0u64;
            for i in 0..20_000_000u64 {
                x = x.wrapping_mul(31).wrapping_add(i);
            }
            std::hint::black_box(x);
            let (u1, s1) = cpu_times().unwrap();
            assert!(u1 + s1 > u0 + s0, "CPU time must advance under a spin");
        } else {
            assert!(cpu_times().is_none());
        }
        if !cfg!(target_os = "linux") {
            assert!(read_proc_io().is_none());
        }
    }
}
