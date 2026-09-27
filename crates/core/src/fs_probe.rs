//! Best-effort detection of RAM-backed (tmpfs/ramfs) directories.
//!
//! The remote-input disk spill (#219/#272) writes ≈1× the input to a local
//! temp file so later convert passes read from disk instead of re-fetching
//! over the network. If that temp directory lives on a RAM-backed filesystem
//! (tmpfs or ramfs — the common default for `/tmp` and always for `/dev/shm`),
//! the spill trades network bytes for **memory pressure** rather than disk,
//! defeating its purpose and risking OOM on large inputs. #273 warns when the
//! resolved spill directory is RAM-backed and points users at `--spill-dir`.
//!
//! Detection is Linux-only via `statfs(2)` and strictly best-effort: any error
//! (unsupported platform, `statfs` failure, missing path) yields `None`, and
//! callers stay silent on `None`. It never fails the conversion.

use std::path::Path;

/// tmpfs superblock magic (`man 2 statfs`, `linux/magic.h`).
#[cfg(target_os = "linux")]
const TMPFS_MAGIC: u32 = 0x0102_1994;
/// ramfs superblock magic (`linux/magic.h`).
#[cfg(target_os = "linux")]
const RAMFS_MAGIC: u32 = 0x8584_58f6;

/// Whether `path` resides on a RAM-backed filesystem (tmpfs or ramfs).
///
/// - `Some(true)`  — `statfs` reports a tmpfs/ramfs superblock magic.
/// - `Some(false)` — `statfs` succeeded and the filesystem is something else.
/// - `None`        — detection is unavailable (non-Linux) or errored
///   (`statfs` failed, e.g. the path does not exist). Best-effort: callers
///   must treat `None` as "don't know" and stay silent.
#[cfg(target_os = "linux")]
pub fn is_ram_backed(path: &Path) -> Option<bool> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statfs` writes into a caller-provided `struct statfs`. A zeroed
    // value is a valid initial state, and `c_path` is a valid NUL-terminated
    // C string that outlives the call.
    let mut buf = unsafe { std::mem::zeroed::<libc::statfs>() };
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return None;
    }
    // `f_type`'s integer width/signedness varies by libc/arch; the magic
    // values are 32-bit, so truncate to `u32` for a width-independent compare.
    let fs_type = buf.f_type as u32;
    Some(fs_type == TMPFS_MAGIC || fs_type == RAMFS_MAGIC)
}

/// Non-Linux stub: detection unavailable, always `None` (compile-time no-op).
#[cfg(not(target_os = "linux"))]
pub fn is_ram_backed(_path: &Path) -> Option<bool> {
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// `/dev/shm` is tmpfs on essentially every Linux system — the canonical
    /// RAM-backed directory. Detection must flag it.
    #[test]
    fn detects_tmpfs_dev_shm() {
        let shm = Path::new("/dev/shm");
        if !shm.exists() {
            eprintln!("skipping detects_tmpfs_dev_shm: /dev/shm absent");
            return;
        }
        assert_eq!(
            is_ram_backed(shm),
            Some(true),
            "/dev/shm must be detected as RAM-backed"
        );
    }

    /// The repo checkout lives on real disk in this environment. Detection
    /// must not flag a real-disk path (the property that matters: we never
    /// warn spuriously).
    #[test]
    fn real_disk_not_flagged() {
        let cwd = std::env::current_dir().expect("cwd");
        assert_ne!(
            is_ram_backed(&cwd),
            Some(true),
            "a real-disk working directory must not be flagged RAM-backed"
        );
    }

    /// A nonexistent path makes `statfs` fail; detection is best-effort and
    /// returns `None` (never an error, never a false positive).
    #[test]
    fn missing_path_is_undetectable() {
        assert_eq!(
            is_ram_backed(Path::new("/nonexistent/tylertoo-fs-probe-273")),
            None,
            "an unresolvable path must yield None, not a warning"
        );
    }
}
