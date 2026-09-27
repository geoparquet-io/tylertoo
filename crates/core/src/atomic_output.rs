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
    temp: TempPath,
    target: PathBuf,
}

/// Open a sibling of `target` for writing. The returned [`File`] is the sink;
/// the [`PendingOutput`] renames it over `target` on `publish`.
///
/// A relative `target` with no directory component (`out.parquet`) is
/// created in the current directory, like `File::create` would.
pub(crate) fn create(target: &Path) -> io::Result<(File, PendingOutput)> {
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
    let temp = tempfile::Builder::new()
        .prefix(&format!("{name}."))
        .suffix(".partial")
        .tempfile_in(dir)
        .map_err(|e| {
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
            temp,
            target: target.to_path_buf(),
        },
    ))
}

impl PendingOutput {
    /// Where the output is being written before publish.
    #[cfg(test)]
    pub(crate) fn temp_path(&self) -> &Path {
        &self.temp
    }

    /// Rename the completed sibling over the destination, replacing whatever
    /// was there. The caller must have flushed and closed its handle first.
    ///
    /// On failure the sibling is removed (it may be incomplete from the
    /// caller's point of view only if the caller published early, which is
    /// a bug there), and the destination is left as it was.
    pub(crate) fn publish(self) -> io::Result<()> {
        // `TempPath::persist` is a rename that keeps the guard's cleanup
        // semantics on failure (the sibling is deleted when the error is
        // dropped) and on Windows replaces an existing destination the way
        // `std::fs::rename` does on Unix.
        self.temp.persist(&self.target).map_err(|e| {
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

    #[cfg(unix)]
    #[test]
    fn sibling_is_private_to_the_owner() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.parquet");
        let (_f, pending) = create(&target).unwrap();
        let mode = std::fs::metadata(pending.temp_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "sibling mode {mode:o}");
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
}
