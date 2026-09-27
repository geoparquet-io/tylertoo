//! Shared guard for tests backed by the real-data fixtures (issue #369).
//!
//! Included by integration tests in several crates via `#[path]`, so there is
//! one copy of both rules below rather than one per test file:
//!
//! **Resolve from the workspace root, not the current directory.** Cargo runs
//! an integration test with the working directory set to the *package* root,
//! so a `tests/fixtures/...` literal silently means
//! `crates/<pkg>/tests/fixtures/...`. That is how
//! `leaf_directory_integration` came to skip itself everywhere, CI included,
//! while reporting `ok`.
//!
//! **Skip on usability, not existence.** The fixtures are tracked with
//! git-lfs but distributed as a release artifact, so a fresh clone holds
//! ~130-byte pointer files. Those satisfy `Path::exists()`, so an
//! existence-only guard lets the test run against a text file and fail deep
//! in the parquet reader with `Corrupt footer` — a failure that points at the
//! conversion path rather than at the missing fixture.

#![allow(dead_code)] // not every including test uses every helper

use std::path::{Path, PathBuf};

/// Name of the release that carries the fixtures, per
/// `tests/fixtures/realdata/README.md`.
const FIXTURE_RELEASE: &str = "fixtures-v1";

/// Locate a real-data fixture, or return `None` if this test cannot run
/// against it.
///
/// `None` is always accompanied by a printed explanation naming the remedy,
/// so a skipped test says why it skipped instead of passing in silence.
///
/// **On CI it does not skip — it fails.** Skipping is a convenience for a
/// working copy without the fixtures; on CI, which downloads them as a build
/// step, an unusable fixture means that step or its cache is broken, and the
/// coverage is gone. #369 is what that costs: the leaf-directory guard
/// resolved its path against the wrong directory and skipped *every CI run
/// for six months* while reporting `ok`. A guard whose designed outcome is
/// silence would reintroduce exactly that, so `CI` (always set by GitHub
/// Actions) turns the skip back into a failure.
pub fn realdata(name: &str) -> Option<PathBuf> {
    let path = workspace_root().join("tests/fixtures/realdata").join(name);
    match usability(&path) {
        Ok(()) => Some(path),
        Err(why) => {
            let remedy = format!(
                "{} {why}\n  Fetch the fixtures with:\n    \
                 gh release download {FIXTURE_RELEASE} \
                 --dir tests/fixtures/realdata/ --clobber",
                path.display(),
            );
            assert!(
                std::env::var_os("CI").is_none(),
                "fixture unusable, and CI must not skip real-data coverage: {remedy}"
            );
            eprintln!("Skipping: {remedy}");
            None
        }
    }
}

/// Locate a golden PMTiles fixture under `tests/fixtures/golden/`.
///
/// Unlike [`realdata`], these are committed directly (see `.gitattributes` --
/// only `tests/fixtures/realdata/*.parquet` is git-lfs), so every clone,
/// including a fresh one with no LFS fetch, has the real bytes. There is
/// nothing to skip on: a missing or unusable golden fixture is a repo bug,
/// not an environment gap, so this panics rather than silently skipping.
pub fn golden(name: &str) -> PathBuf {
    let path = workspace_root().join("tests/fixtures/golden").join(name);
    usability(&path).unwrap_or_else(|why| {
        panic!("golden fixture {} {why}", path.display());
    });
    path
}

/// Locate a **guard** fixture (`tests/fixtures/guard/`).
///
/// Committed as a plain git blob for the same reason [`golden`] is — only
/// `tests/fixtures/realdata/*.parquet` is routed through git-lfs — so a plain
/// `actions/checkout` already holds the file and no release download, cache or
/// submodule can be the reason it is missing. That matters more here than
/// anywhere: the golden guard's whole job is to fail on an output change, and
/// a fixture that can be absent turns it into a test that can quietly not run
/// (#369's failure mode, #558's consequence).
///
/// So this one never skips and never returns `None`: it panics, everywhere,
/// and names the remedy.
pub fn guard(name: &str) -> PathBuf {
    let path = workspace_root().join("tests/fixtures/guard").join(name);
    if let Err(why) = usability(&path) {
        panic!(
            "guard fixture {} {why}\n  It is committed as a plain git blob; \
             restore it with:\n    git checkout -- tests/fixtures/guard/",
            path.display(),
        );
    }
    path
}

/// The workspace root: the nearest ancestor of this crate that holds the
/// fixture directory.
///
/// `CARGO_MANIFEST_DIR` is expanded in the *including* crate, so this works
/// from any package without each test counting `../..` for itself — the
/// counting is what went wrong in #369.
fn workspace_root() -> PathBuf {
    let mut dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    loop {
        if dir.join("tests/fixtures/realdata").is_dir() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            // Report against the manifest dir so the skip message shows where
            // the search started.
            None => return PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        }
    }
}

/// Why this path is not a usable fixture, or `Ok(())` if it is.
fn usability(path: &Path) -> Result<(), String> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("is missing.".to_string()),
        Err(e) => return Err(format!("could not be read: {e}.")),
    };
    classify(&bytes, path)
}

/// Content check, split out so it is testable without touching the disk.
fn classify(bytes: &[u8], path: &Path) -> Result<(), String> {
    if bytes.starts_with(b"version https://git-lfs.github.com/spec/v1") {
        return Err(format!(
            "is an unresolved git-lfs pointer ({} bytes), not the fixture.",
            bytes.len()
        ));
    }
    // Parquet and PMTiles have a magic worth asserting; other fixtures (raw
    // WKB) are accepted on readability alone.
    let is_pmtiles = path.extension().is_some_and(|e| e == "pmtiles");
    if is_pmtiles && !bytes.starts_with(b"PMTiles") {
        return Err(format!(
            "is not a PMTiles archive (missing the PMTiles magic, {} bytes).",
            bytes.len()
        ));
    }
    let is_parquet = path.extension().is_some_and(|e| e == "parquet");
    if is_parquet && !(bytes.starts_with(b"PAR1") && bytes.ends_with(b"PAR1")) {
        return Err(format!(
            "is not a Parquet file (missing the PAR1 magic, {} bytes).",
            bytes.len()
        ));
    }
    if bytes.is_empty() {
        return Err("is empty.".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod fixture_guard_tests {
    use super::*;

    #[test]
    fn lfs_pointer_is_not_a_usable_fixture() {
        // The exact shape a clone without git-lfs leaves behind. `exists()`
        // is true for this, which is the whole bug.
        let pointer = b"version https://git-lfs.github.com/spec/v1\n\
                        oid sha256:0123456789abcdef\nsize 145816\n";
        let err = classify(pointer, Path::new("open-buildings.parquet")).unwrap_err();
        assert!(err.contains("git-lfs pointer"), "{err}");
    }

    #[test]
    fn real_parquet_is_usable() {
        let mut buf = b"PAR1".to_vec();
        buf.extend_from_slice(&[0u8; 32]);
        buf.extend_from_slice(b"PAR1");
        assert!(classify(&buf, Path::new("x.parquet")).is_ok());
    }

    #[test]
    fn truncated_parquet_is_rejected() {
        // A half-written or half-downloaded fixture fails the footer check
        // the same way the reader would, but with a message that says so.
        let err = classify(b"PAR1garbage", Path::new("x.parquet")).unwrap_err();
        assert!(err.contains("PAR1"), "{err}");
        let err = classify(b"", Path::new("x.parquet")).unwrap_err();
        assert!(err.contains("PAR1"), "{err}");
    }

    #[test]
    fn pmtiles_fixtures_need_the_pmtiles_magic() {
        let mut buf = b"PMTiles\x03".to_vec();
        buf.extend_from_slice(&[0u8; 16]);
        assert!(classify(&buf, Path::new("x.pmtiles")).is_ok());
        let err = classify(b"garbage", Path::new("x.pmtiles")).unwrap_err();
        assert!(err.contains("PMTiles magic"), "{err}");
        let err = classify(b"", Path::new("x.pmtiles")).unwrap_err();
        assert!(err.contains("PMTiles magic"), "{err}");
    }

    #[test]
    fn non_parquet_fixtures_are_accepted_on_content() {
        // Raw WKB has no magic to check; only emptiness and the pointer
        // shape disqualify it.
        assert!(classify(&[0x01, 0x03, 0x00], Path::new("poly.wkb")).is_ok());
        assert!(classify(b"", Path::new("poly.wkb")).is_err());
    }

    #[test]
    fn workspace_root_holds_the_fixture_directory() {
        // The #369 path bug in assertion form: whatever crate includes this,
        // the resolved root must actually contain the fixtures.
        let root = workspace_root();
        assert!(
            root.join("tests/fixtures/realdata").is_dir(),
            "resolved workspace root {root:?} has no fixture directory"
        );
    }
}
