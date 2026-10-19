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
/// Checks what [`create`], the PMTiles writer and a final rename will need:
/// that `target` is not a directory and that its directory exists and
/// accepts new files. The directory is probed with a uniquely named,
/// immediately removed sibling. `target` itself is never opened: an
/// existing output is replaced by a rename, so its own permissions do not
/// matter, and a PMTiles `<output>.partial` salvage file beside it is left
/// alone. An existing non-regular `target` (`/dev/null`, a FIFO) is written
/// directly and passes without a probe.
///
/// # Errors
///
/// [`OutputNotWritable`] naming `target` and, where it is the cause, its
/// directory.
pub fn check_writable(target: &Path) -> Result<(), OutputNotWritable> {
    let fail = |reason: String| {
        Err(OutputNotWritable {
            path: target.to_path_buf(),
            reason,
        })
    };
    // Resolve a symlink the way `create` does, so the probe lands in the
    // directory the output will be published into.
    let resolved;
    let real = match std::fs::symlink_metadata(target) {
        Ok(m) if m.file_type().is_symlink() => match std::fs::canonicalize(target) {
            Ok(p) => {
                resolved = p;
                resolved.as_path()
            }
            Err(e) => return fail(format!("cannot resolve the symlink: {e}")),
        },
        _ => target,
    };
    if let Ok(m) = std::fs::metadata(real) {
        if m.is_dir() {
            return fail("it is a directory; give the output a file path".to_string());
        }
        if !m.is_file() {
            return Ok(());
        }
    }
    let dir = match real.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return fail(format!("{} is not a directory", dir.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return fail(format!("directory {} does not exist", dir.display()))
        }
        Err(e) => return fail(format!("cannot access directory {}: {e}", dir.display())),
    }
    match tempfile::Builder::new()
        .prefix(".tylertoo-write-probe.")
        .tempfile_in(dir)
    {
        Ok(_probe) => Ok(()), // removed on drop
        Err(e) => fail(format!("directory {} is not writable: {e}", dir.display())),
    }
}

/// [`check_writable`] for a file rewritten in place, not renamed over.
///
/// A file written with `std::fs::write` or `File::create` needs an existing
/// regular file to be writable too. It is opened for writing without
/// truncation, so the check cannot destroy it.
///
/// # Errors
///
/// As [`check_writable`], plus an existing file that cannot be opened for
/// writing.
pub fn check_writable_in_place(target: &Path) -> Result<(), OutputNotWritable> {
    check_writable(target)?;
    if std::fs::metadata(target).is_ok_and(|m| m.is_file()) {
        if let Err(e) = std::fs::OpenOptions::new().write(true).open(target) {
            return Err(OutputNotWritable {
                path: target.to_path_buf(),
                reason: format!("the existing file is not writable: {e}"),
            });
        }
    }
    Ok(())
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
    }
}
