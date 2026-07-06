#![recursion_limit = "256"]
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
//! [`wkb`], [`covering`]), tile math ([`tile`], [`world_coord`]), geometry
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

pub mod batch_processor;
pub mod clip;
pub mod compression;
pub mod covering;
pub mod decode;
pub mod dedup;
pub mod input;
pub mod ioverlay_clip;
pub mod mvt;
pub mod overview;
pub mod pmtiles_writer;
pub mod quality;
pub mod sutherland_hodgman;
pub mod tile;
pub mod wkb;
pub mod world_coord;

// Re-export Compression from compression module for public API
pub use compression::Compression;
// Re-export StreamingPmtilesWriter and related types
pub use pmtiles_writer::{StreamingPmtilesWriter, StreamingWriteStats};
// Re-export CRS validation for public API
pub use quality::{extract_crs, validate_wgs84, CrsInfo};

/// Errors that can occur while reading GeoParquet or writing PMTiles.
#[derive(Error, Debug)]
pub enum Error {
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
}

pub type Result<T> = std::result::Result<T, Error>;
