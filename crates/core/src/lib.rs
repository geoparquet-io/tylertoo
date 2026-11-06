#![recursion_limit = "256"]
// #779: clippy.toml's disallowed-methods name the single owner of parquet
// and IPC reads and of output-file creation. `#[cfg(test)]` code reads and
// writes its own trusted fixtures, so the lint is off in the test build
// only; production code is still linted by the non-test build that
// `cargo clippy --all-targets` also checks.
#![cfg_attr(
    test,
    expect(
        clippy::disallowed_methods,
        reason = "#779: tests construct trusted fixtures; production is checked by the non-test build"
    )
)] // #779: test build only
//! Core library for GeoParquet multi-resolution overviews and PMTiles export.
//!
//! The product pipeline lives in [`overview`]: convert a (gpio-sorted)
//! GeoParquet file into a level-banded overview GeoParquet file
//! ([`overview::convert`]), validate it against the spec
//! ([`overview::check`]), and export it to a PMTiles vector-tile archive
//! ([`overview::export`]).
//!
//! The remaining top-level modules are shared infrastructure the overview
//! pipeline builds on: Arrow/Parquet geometry decoding ([`batch_processor`],
//! [`wkb`], [`covering`]), tile math ([`tile`]), geometry
//! clipping ([`clip`], [`ioverlay_clip`], [`sutherland_hodgman`]), MVT
//! encoding ([`mvt`]), and the PMTiles v3 writer ([`pmtiles_writer`],
//! [`compression`], [`dedup`]).
//!
//! The legacy per-tile pipeline (`pipeline`, `Converter`) was removed in
//! favor of the overview pipeline; see `context/ARCHITECTURE.md` for the
//! decision record. The one-shot GeoParquet → PMTiles UX survives as the
//! CLI `tiles` facade, which chains overview convert → export-pmtiles
//! through a temporary file.
//!
//! # Memory Profiling
//!
//! Build with `--features dhat-heap` to enable heap profiling with dhat.
//! When enabled, the program will write `dhat-heap.json` on exit which can
//! be analyzed at <https://nnethercote.github.io/dh_view/dh_view.html>

// dhat global allocator - must be at crate root
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use thiserror::Error;

// Include the protobuf-generated code
pub mod vector_tile {
    include!(concat!(env!("OUT_DIR"), "/vector_tile.rs"));
}

pub mod archive_index;
pub mod atomic_output;
pub mod batch_processor;
pub mod clip;
pub mod compression;
pub mod covering;
pub mod decode;
pub mod dedup;
pub mod diagnostics;
pub mod error;
pub mod fs_probe;
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzz;
pub mod input;
pub mod input_set;
pub mod ioverlay_clip;
pub mod merge;
pub mod mvt;
pub mod overview;
mod pmtiles_metadata;
pub mod pmtiles_writer;
pub mod pyramid;
pub mod quality;
pub mod resolved_schema;
pub mod shard;
pub mod stats;
pub mod sutherland_hodgman;
#[cfg(test)]
pub(crate) mod test_log;
pub mod tile;
#[cfg(test)]
mod valve_counter;
pub mod wkb;
pub(crate) mod wkb_column;

// Re-export Compression from compression module for public API
pub use compression::Compression;
// Re-export the archive-level publication metadata type
pub use pmtiles_metadata::TilesetMetadata;
// Re-export StreamingPmtilesWriter and related types
pub use pmtiles_writer::{StreamingPmtilesWriter, StreamingWriteStats};
// Re-export CRS validation for public API
pub use quality::{extract_crs, validate_wgs84, CrsInfo};

/// Errors that can occur while reading GeoParquet or reading and writing
/// PMTiles.
///
/// `#[non_exhaustive]`: new failure modes are added without a breaking
/// change; match with a wildcard arm. [`error::Classify::kind`] says what a
/// failure means to the caller.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Source loss accounting failed.
    #[error(transparent)]
    Accounting(#[from] crate::diagnostics::AccountingError),
    #[error("Failed to read GeoParquet file: {0}")]
    GeoParquetRead(String),

    /// Writing a PMTiles archive failed (#438).
    #[error("Failed to write PMTiles: {0}")]
    Write(pmtiles_writer::WriteError),

    /// A PMTiles archive could not be read (#438).
    #[error("Failed to read PMTiles: {0}")]
    Archive(archive_index::ArchiveError),

    /// `error`, at `path`: the archive or output it concerns, for a caller
    /// that holds several.
    #[error("{}: {error}", path.display())]
    At {
        /// The archive or output.
        path: std::path::PathBuf,
        /// What went wrong there.
        error: Box<Error>,
    },

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The output cannot be written (#452: checked before any work).
    #[error(transparent)]
    Output(#[from] atomic_output::OutputNotWritable),
}

// Not `#[from]`: the inner error's message is already in this one's, so it
// is not also a `source()` the CLI would print a second time.
impl From<pmtiles_writer::WriteError> for Error {
    fn from(e: pmtiles_writer::WriteError) -> Self {
        Error::Write(e)
    }
}

impl From<archive_index::ArchiveError> for Error {
    fn from(e: archive_index::ArchiveError) -> Self {
        Error::Archive(e)
    }
}

impl Error {
    /// `error`, attributed to `path` ([`Error::At`]).
    pub(crate) fn at(path: &std::path::Path, error: impl Into<Error>) -> Self {
        Error::At {
            path: path.to_path_buf(),
            error: Box::new(error.into()),
        }
    }
}

impl error::Classify for Error {
    fn kind(&self) -> error::ErrorKind {
        match self {
            Error::InvalidConfig(_) => error::ErrorKind::Usage,
            Error::At { error, .. } => error.kind(),
            _ => error::ErrorKind::Runtime,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::error::{Classify, ErrorKind};
    use super::*;

    #[test]
    fn only_configuration_errors_are_classified_as_usage() {
        assert_eq!(
            Error::InvalidConfig("invalid band options".into()).kind(),
            ErrorKind::Usage
        );
        for error in [
            Error::GeoParquetRead("invalid footer".into()),
            archive_index::ArchiveError::Header("invalid header".into()).into(),
            pmtiles_writer::WriteError::Finalized.into(),
            Error::Io(std::io::Error::other("disk full")),
            Error::Accounting(diagnostics::AccountingError::Io(std::io::Error::other(
                "spill failed",
            ))),
        ] {
            assert_eq!(error.kind(), ErrorKind::Runtime, "{error}");
        }
    }

    /// An error attributed to a path keeps its own kind and prefixes the path.
    #[test]
    fn an_attributed_error_keeps_its_kind() {
        let at = Error::at(
            std::path::Path::new("a.pmtiles"),
            Error::InvalidConfig("x".into()),
        );
        assert_eq!(at.kind(), ErrorKind::Usage);
        assert_eq!(at.to_string(), "a.pmtiles: Invalid configuration: x");
    }
}
