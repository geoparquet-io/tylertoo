//! Write-to-sibling-then-rename for file outputs (#427).
//!
//! Every long-running writer used to open its destination with
//! `File::create`, which truncates it on the spot. A run killed part-way
//! through — Ctrl-C, OOM, a Slurm walltime — then left a footer-less file at
//! the destination *and* had already destroyed whatever good output was there
//! before. The PMTiles exporter fixed this for itself (#459/#528:
//! `<output>.partial` + rename at finalize); this module is the same contract
//! for the GeoParquet outputs (`overview`, `decode`):
//!
//! - the destination is untouched until the output is complete;
//! - the output is built in a uniquely-named sibling
//!   (`<name>.<random>.partial`, created `O_EXCL`, mode 0600 on Unix) in the
//!   destination's directory, so the final publish is a same-filesystem
//!   `rename(2)` and never a copy;
//! - a run that errors out (or drops the guard without publishing) removes
//!   the sibling, so nothing is left beside the user's output;
//! - a run killed outright leaves the sibling, but the previous destination
//!   is still intact.
//!
//! The sibling lives next to the destination, not under `--spill-dir`: a
//! cross-device rename would fail (or, on some platforms, silently degrade
//! to a copy), and the destination volume must hold the output anyway.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use tempfile::TempPath;

/// The destination of an in-progress output, plus the sibling it is being
/// written to. Dropping it without [`PendingOutput::publish`] removes the
/// sibling and leaves the destination untouched.
#[derive(Debug)]
pub(crate) struct PendingOutput {
    /// `None` when the destination is written directly (an existing
    /// non-regular file such as `/dev/null` or a FIFO, which no rename could
    /// replace); `publish` is then a no-op.
    temp: Option<TempPath>,
    target: PathBuf,
}

/// Open a sibling of `target` for writing. The returned [`File`] is the sink;
/// the [`PendingOutput`] renames it over `target` on `publish`.
///
/// A relative `target` with no directory component (`out.parquet`) is
/// created in the current directory, like `File::create` would. A `target`
/// that is an existing symlink is written *through*: the sibling is created
/// beside the link's resolved destination (same filesystem as the rename)
/// and published over that destination, so the link survives and points at
/// the new data, as it would have with `File::create`. An existing
/// destination that is not a regular file (a device like `/dev/null` or
/// `/dev/full`, a FIFO) is opened and written directly, since a rename could
/// never replace it and there is no previous output to protect.
pub(crate) fn create(target: &Path) -> io::Result<(File, PendingOutput)> {
    let resolved;
    let target = match std::fs::symlink_metadata(target) {
        Ok(m) if m.file_type().is_symlink() => {
            resolved = std::fs::canonicalize(target).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("cannot resolve the symlink {}: {e}", target.display()),
                )
            })?;
            resolved.as_path()
        }
        _ => target,
    };
    if let Ok(m) = std::fs::metadata(target) {
        if !m.is_file() && !m.is_dir() {
            let file = File::create(target)?;
            return Ok((
                file,
                PendingOutput {
                    temp: None,
                    target: target.to_path_buf(),
                },
            ));
        }
    }
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} has no file name", target.display()),
            )
        })?
        .to_string_lossy()
        .into_owned();
    let prefix = format!("{name}.");
    let mut builder = tempfile::Builder::new();
    builder.prefix(&prefix).suffix(".partial");
    // `tempfile` creates 0600 by default and `persist` is a plain rename, so
    // the published output would stay owner-only — unreadable from a shared
    // directory or a web-served folder, unlike the PMTiles beside it. Ask for
    // 0666 so the process umask applies exactly as it does to `File::create`.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
    }
    let temp = builder.tempfile_in(dir).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "cannot create the temporary output beside {}: {e}",
                target.display()
            ),
        )
    })?;
    let (file, temp) = temp.into_parts();
    Ok((
        file,
        PendingOutput {
            temp: Some(temp),
            target: target.to_path_buf(),
        },
    ))
}

/// An output path that [`check_writable`] found could not be written.
#[derive(Debug, thiserror::Error)]
#[error("cannot write {}: {reason}", .path.display())]
pub struct OutputNotWritable {
    /// The output path as given.
    pub path: PathBuf,
    /// Why not, naming the directory where that is the cause.
    pub reason: String,
}

/// Preflight for a file output (#452): fail in milliseconds, naming the
/// path, instead of after a whole conversion.
///
/// Checks that `target` is not a directory and that its directory exists
/// and accepts new files, which is what every writer needs before its final
/// rename. The directory is probed with a uniquely named, immediately
/// removed sibling. `target` itself is never opened, and a PMTiles
/// `<output>.partial` salvage file beside it is left alone. On Unix an
/// existing output's own permissions do not matter, since the rename
/// replaces it.
///
/// Two kinds of `target` pass without a probe, leaving any error to the
/// writer, which names the path too:
/// - an existing non-regular file (`/dev/null`, a pipe, `/dev/stdout`);
/// - a symlink. The writers disagree on one: the GeoParquet writer writes
///   through it, while the PMTiles writers replace the link itself. A check
///   that guessed either way would refuse outputs the other writer handles.
///
/// # Errors
///
/// [`OutputNotWritable`] naming `target` and, where it is the cause, its
/// directory.
pub fn check_writable(target: &Path) -> Result<(), OutputNotWritable> {
    // `metadata` follows links, including `/proc/self/fd/N` ones that point
    // at a pipe and that `canonicalize` cannot resolve.
    if let Ok(m) = std::fs::metadata(target) {
        if m.is_dir() {
            return not_writable(
                target,
                "it is a directory; give the output a file path".into(),
            );
        }
        if !m.is_file() {
            return Ok(());
        }
    }
    if std::fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(());
    }
    // `Path::parent` ignores a trailing separator, so `out.pmtiles/` would
    // otherwise probe `.`, pass, and fail at the final create or rename.
    if ends_with_separator(target) {
        return not_writable(
            target,
            "it ends in a path separator, so it names a directory; give the output a file path"
                .into(),
        );
    }
    let dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return not_writable(target, format!("{} is not a directory", dir.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return not_writable(
                target,
                format!("directory {} does not exist", dir.display()),
            )
        }
        Err(e) => {
            return not_writable(
                target,
                format!("cannot access directory {}: {e}", dir.display()),
            )
        }
    }
    probe_directory(dir).or_else(|e| {
        not_writable(
            target,
            format!("directory {} is not writable: {e}", dir.display()),
        )
    })
}

/// [`check_writable`] for a file rewritten in place, not renamed over.
///
/// `std::fs::write` and `File::create` truncate an existing file rather
/// than replace it, so that file must be writable, and its directory need
/// not be. It is opened for writing without truncation, so the check
/// cannot destroy it. A path that does not exist yet gets
/// [`check_writable`].
///
/// # Errors
///
/// As [`check_writable`], or an existing file that cannot be opened for
/// writing.
pub fn check_writable_in_place(target: &Path) -> Result<(), OutputNotWritable> {
    if !std::fs::metadata(target).is_ok_and(|m| m.is_file()) {
        return check_writable(target);
    }
    match std::fs::OpenOptions::new().write(true).open(target) {
        Ok(_) => Ok(()),
        Err(e) => not_writable(target, format!("the existing file is not writable: {e}")),
    }
}

/// Separators are ASCII, so the last encoded byte is one only when the last
/// character is.
fn ends_with_separator(path: &Path) -> bool {
    path.as_os_str()
        .as_encoded_bytes()
        .last()
        .is_some_and(|&b| std::path::is_separator(char::from(b)))
}

fn not_writable(target: &Path, reason: String) -> Result<(), OutputNotWritable> {
    Err(OutputNotWritable {
        path: target.to_path_buf(),
        reason,
    })
}

/// Create and remove a uniquely named file in `dir`. Not `tempfile`: its
/// errors append the random probe name, which means nothing to a user.
fn probe_directory(dir: &Path) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    loop {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let probe = dir.join(format!(".tylertoo-write-probe.{}.{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
        {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                return Ok(());
            }
            // Left behind by an earlier process with the same pid.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}

impl PendingOutput {
    /// Where the output is being written before publish.
    #[cfg(test)]
    pub(crate) fn temp_path(&self) -> &Path {
        self.temp.as_deref().unwrap_or(&self.target)
    }

    /// Rename the completed sibling over the destination, replacing whatever
    /// was there. The caller must have flushed and closed its handle first.
    ///
    /// On failure the sibling is removed (it may be incomplete from the
    /// caller's point of view only if the caller published early, which is
    /// a bug there), and the destination is left as it was.
    pub(crate) fn publish(self) -> io::Result<()> {
        let Some(temp) = self.temp else {
            return Ok(()); // written directly, nothing to publish
        };
        // `TempPath::persist` is a rename that keeps the guard's cleanup
        // semantics on failure (the sibling is deleted when the error is
        // dropped) and on Windows replaces an existing destination the way
        // `std::fs::rename` does on Unix.
        temp.persist(&self.target).map_err(|e| {
            io::Error::new(
                e.error.kind(),
                format!(
                    "cannot publish {} over {}: {}",
                    e.path.display(),
                    self.target.display(),
                    e.error
                ),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The contract the issue asks for: an interrupted write (here: the
    /// guard dropped without `publish`) leaves the previous destination
    /// byte-for-byte intact and no sibling behind.
    #[test]
    fn interrupted_write_leaves_destination_intact_and_no_litter() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.parquet");
        std::fs::write(&target, b"previous good output").unwrap();

        {
            let (mut file, pending) = create(&target).unwrap();
            file.write_all(b"half an output").unwrap();
            // While the write is in flight the destination is untouched and
            // the sibling sits beside it.
            assert_eq!(
                std::fs::read(&target).unwrap(),
                b"previous good output",
                "destination truncated while the output was in flight"
            );
            assert_eq!(pending.temp_path().parent().unwrap(), dir.path());
            assert_eq!(listing(dir.path()).len(), 2);
            drop(file);
            drop(pending); // "interrupted": never published
        }

        assert_eq!(std::fs::read(&target).unwrap(), b"previous good output");
        assert_eq!(listing(dir.path()), vec!["out.parquet".to_string()]);
    }

    #[test]
    fn publish_replaces_destination_and_removes_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.parquet");
        std::fs::write(&target, b"old").unwrap();

        let (mut file, pending) = create(&target).unwrap();
        file.write_all(b"new complete output").unwrap();
        drop(file);
        pending.publish().unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"new complete output");
        assert_eq!(listing(dir.path()), vec!["out.parquet".to_string()]);
    }

    #[test]
    fn publish_creates_a_missing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("fresh.parquet");
        let (mut file, pending) = create(&target).unwrap();
        file.write_all(b"x").unwrap();
        drop(file);
        pending.publish().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"x");
        assert_eq!(listing(dir.path()), vec!["fresh.parquet".to_string()]);
    }

    /// Two concurrent writers to the same destination never share a sibling
    /// (the sibling is `O_EXCL`-created with a random component).
    #[test]
    fn siblings_are_unique_per_writer() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.parquet");
        let (_a, pa) = create(&target).unwrap();
        let (_b, pb) = create(&target).unwrap();
        assert_ne!(pa.temp_path(), pb.temp_path());
        let name = pa.temp_path().file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("out.parquet.") && name.ends_with(".partial"),
            "sibling name should identify its destination: {name}"
        );
    }

    /// The published file must carry the same mode `File::create` would
    /// have given it (0666 & !umask), not `tempfile`'s owner-only default:
    /// a shared directory must not end up with an unreadable `.parquet`
    /// next to a readable `.pmtiles`.
    #[cfg(unix)]
    #[test]
    fn published_file_has_file_create_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // Control: what `File::create` yields under the current umask.
        let control = dir.path().join("control");
        File::create(&control).unwrap();
        let expected = std::fs::metadata(&control).unwrap().permissions().mode() & 0o777;

        let target = dir.path().join("out.parquet");
        let (_f, pending) = create(&target).unwrap();
        let sibling_mode = std::fs::metadata(pending.temp_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(sibling_mode, expected, "sibling mode {sibling_mode:o}");
        pending.publish().unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, expected,
            "published mode {mode:o}, expected {expected:o}"
        );
    }

    /// A symlinked destination is written through, like `File::create`
    /// would: the link survives and its target holds the new data, with no
    /// sibling left beside either.
    #[cfg(unix)]
    #[test]
    fn symlinked_destination_is_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("real");
        std::fs::create_dir(&real_dir).unwrap();
        let real = real_dir.join("data.parquet");
        std::fs::write(&real, b"old").unwrap();
        let link = dir.path().join("link.parquet");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let (mut file, pending) = create(&link).unwrap();
        assert_eq!(
            pending.temp_path().parent().unwrap(),
            std::fs::canonicalize(&real_dir).unwrap(),
            "sibling goes beside the resolved target"
        );
        file.write_all(b"new").unwrap();
        drop(file);
        pending.publish().unwrap();

        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), real);
        assert_eq!(std::fs::read(&real).unwrap(), b"new");
        assert_eq!(std::fs::read(&link).unwrap(), b"new");
        assert_eq!(listing(&real_dir), vec!["data.parquet".to_string()]);
        assert_eq!(
            listing(dir.path()),
            vec!["link.parquet".to_string(), "real".to_string()]
        );
    }

    /// A device or FIFO destination is written directly: no sibling (there
    /// is nothing beside `/dev/null` we may create, and no previous output to
    /// protect), and the device's own errors surface from the write.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_regular_destination_is_written_directly() {
        let null = Path::new("/dev/null");
        if !null.exists() {
            return;
        }
        let (mut file, pending) = create(null).unwrap();
        assert_eq!(pending.temp_path(), null, "no sibling for a device");
        file.write_all(b"discarded").unwrap();
        drop(file);
        pending.publish().unwrap();

        let full = Path::new("/dev/full");
        if full.exists() {
            let (mut file, _pending) = create(full).unwrap();
            let err = file.write_all(b"x").unwrap_err();
            assert!(err.to_string().contains("No space left"), "{err}");
        }
    }

    #[test]
    fn missing_parent_directory_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nope").join("out.parquet");
        let err = create(&target).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(
            err.to_string().contains("out.parquet"),
            "error names the destination: {err}"
        );
    }

    // --- check_writable (#452) ----------------------------------------------

    #[test]
    fn check_writable_accepts_a_new_file_in_a_writable_directory() {
        let dir = tempfile::tempdir().unwrap();
        check_writable(&dir.path().join("out.pmtiles")).unwrap();
        assert!(listing(dir.path()).is_empty(), "the probe is removed");
    }

    #[test]
    fn check_writable_names_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("nope");
        let target = parent.join("out.pmtiles");
        let msg = check_writable(&target).unwrap_err().to_string();
        assert!(msg.contains(&target.display().to_string()), "{msg}");
        assert!(msg.contains(&parent.display().to_string()), "{msg}");
        assert!(msg.contains("does not exist"), "{msg}");
    }

    #[test]
    fn check_writable_refuses_a_directory_and_a_file_as_parent() {
        let dir = tempfile::tempdir().unwrap();
        let msg = check_writable(dir.path()).unwrap_err().to_string();
        assert!(msg.contains("is a directory"), "{msg}");

        let file = dir.path().join("plain");
        std::fs::write(&file, b"x").unwrap();
        let msg = check_writable(&file.join("out.pmtiles"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("not a directory"), "{msg}");
    }

    /// An existing output is replaced by a rename, so its own mode does not
    /// matter, and the check must leave it byte-for-byte alone. In
    /// particular a PMTiles run's `<output>.partial` salvage file is never
    /// touched: the Tail layout rotates it to `.partial.prev`.
    #[test]
    fn check_writable_leaves_existing_files_alone() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.pmtiles");
        let partial = dir.path().join("out.pmtiles.partial");
        std::fs::write(&target, b"previous output").unwrap();
        std::fs::write(&partial, b"salvage").unwrap();
        check_writable(&target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"previous output");
        assert_eq!(std::fs::read(&partial).unwrap(), b"salvage");
        assert_eq!(listing(dir.path()), ["out.pmtiles", "out.pmtiles.partial"]);
    }

    /// Devices are written directly by [`create`], so there is no directory
    /// to probe (and `/dev` is not writable for ordinary users).
    #[test]
    fn check_writable_passes_devices_through() {
        for dev in ["/dev/null", "/dev/full"] {
            let dev = Path::new(dev);
            if dev.exists() {
                check_writable(dev).unwrap();
            }
        }
    }

    /// `--report /dev/stdout` and `--report >(jq .)` name a `/proc/self/fd`
    /// link that `canonicalize` cannot resolve for a pipe; `std::fs::write`
    /// writes through it fine, so the check must not refuse it.
    #[cfg(target_os = "linux")]
    #[test]
    fn check_writable_passes_stdout_and_fd_links_through() {
        check_writable(Path::new("/dev/stdout")).unwrap();
        check_writable_in_place(Path::new("/dev/stdout")).unwrap();
        let (read, write) = std::io::pipe().unwrap();
        use std::os::fd::AsRawFd;
        let link = PathBuf::from(format!("/dev/fd/{}", write.as_raw_fd()));
        check_writable(&link).unwrap();
        check_writable_in_place(&link).unwrap();
        drop((read, write));
    }

    /// A symlinked output passes unprobed: the GeoParquet writer writes
    /// through the link, the PMTiles writers replace it, so a dangling link
    /// is a fine PMTiles output.
    #[cfg(unix)]
    #[test]
    fn check_writable_passes_symlinks_through() {
        let dir = tempfile::tempdir().unwrap();
        let dangling = dir.path().join("out.pmtiles");
        std::os::unix::fs::symlink(dir.path().join("nope").join("x"), &dangling).unwrap();
        check_writable(&dangling).unwrap();
        assert_eq!(listing(dir.path()), ["out.pmtiles"]);
    }

    /// `out.pmtiles/` names a directory, which no writer can create as a
    /// file; `Path::parent` drops the trailing slash, so without its own
    /// check the probe of `.` passed and the run failed only at the end.
    #[test]
    fn check_writable_refuses_a_trailing_separator() {
        let dir = tempfile::tempdir().unwrap();
        let mut target = dir.path().join("out.pmtiles").into_os_string();
        target.push(std::path::MAIN_SEPARATOR_STR);
        let target = PathBuf::from(target);
        let msg = check_writable(&target).unwrap_err().to_string();
        assert!(msg.contains("names a directory"), "{msg}");
        let msg = check_writable_in_place(&target).unwrap_err().to_string();
        assert!(msg.contains("names a directory"), "{msg}");
        assert!(listing(dir.path()).is_empty());
    }

    /// A bare file name lives in the current directory: `Path::parent`
    /// gives `""` for it, which is not a missing directory.
    #[test]
    fn check_writable_resolves_a_bare_file_name_to_the_current_directory() {
        check_writable(Path::new("tylertoo-check-writable-probe.pmtiles")).unwrap();
    }

    /// A file rewritten in place (a JSON report, a shard plan) needs the
    /// existing file itself to be writable, which a rename-published output
    /// does not; and the check must not truncate it.
    #[cfg(unix)]
    #[test]
    fn check_writable_in_place_probes_an_existing_file_without_truncating() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let report = dir.path().join("report.json");
        check_writable_in_place(&report).unwrap();
        assert!(!report.exists(), "a new file is not created");

        std::fs::write(&report, b"{}").unwrap();
        check_writable_in_place(&report).unwrap();
        assert_eq!(std::fs::read(&report).unwrap(), b"{}");

        std::fs::set_permissions(&report, std::fs::Permissions::from_mode(0o444)).unwrap();
        // root ignores file modes: nothing to assert there.
        if std::fs::OpenOptions::new()
            .write(true)
            .open(&report)
            .is_ok()
        {
            return;
        }
        check_writable(&report).expect("a rename replaces a read-only file fine");
        let msg = check_writable_in_place(&report).unwrap_err().to_string();
        assert!(msg.contains(&report.display().to_string()), "{msg}");
        assert!(msg.contains("not writable"), "{msg}");
        assert_eq!(std::fs::read(&report).unwrap(), b"{}");
    }

    /// The other way round: a writable file in a read-only directory can be
    /// rewritten in place, so only the rename-published check refuses it.
    #[cfg(unix)]
    #[test]
    fn check_writable_in_place_ignores_a_read_only_directory_for_an_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        let report = ro.join("report.json");
        std::fs::write(&report, b"{}").unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        check_writable_in_place(&report).unwrap();
        // root ignores directory modes: the rest only holds for others.
        if File::create(ro.join("as-root")).is_err() {
            assert!(check_writable(&report).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn check_writable_names_a_read_only_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        // root ignores directory modes: nothing to assert there.
        if File::create(ro.join("as-root")).is_ok() {
            return;
        }
        let msg = check_writable(&ro.join("out.pmtiles"))
            .unwrap_err()
            .to_string();
        assert!(msg.contains(&ro.display().to_string()), "{msg}");
        assert!(msg.contains("not writable"), "{msg}");
        assert!(
            !msg.contains("probe"),
            "the probe name means nothing: {msg}"
        );
    }
}
