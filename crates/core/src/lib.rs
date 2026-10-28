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
pub mod tile;
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

/// Errors that can occur while reading GeoParquet or writing PMTiles.
#[derive(Error, Debug)]
pub enum Error {
    /// Source loss accounting failed.
    #[error(transparent)]
    Accounting(#[from] crate::diagnostics::AccountingError),
    #[error("Failed to read GeoParquet file: {0}")]
    GeoParquetRead(String),

    #[error("Failed to write PMTiles: {0}")]
    PMTilesWrite(String),

    #[error("Failed to read PMTiles: {0}")]
    PMTilesRead(String),

    #[error("Invalid geometry at feature {feature_id}: {reason}")]
    InvalidGeometry { feature_id: usize, reason: String },

    #[error("MVT encoding failed: {0}")]
    MvtEncoding(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The output cannot be written (#452: checked before any work).
    #[error(transparent)]
    Output(#[from] atomic_output::OutputNotWritable),

    /// An option refused before any work
    /// ([`pyramid::PyramidOptions::validate`], #779).
    #[error(transparent)]
    Config(#[from] overview::config::ConfigError),
}

impl Error {
    /// Whether this is an option the caller must fix, refused before any
    /// work (#779), as `ConvertError::is_config` and its siblings classify
    /// theirs.
    pub fn is_config(&self) -> bool {
        matches!(self, Error::Config(_) | Error::InvalidConfig(_))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
