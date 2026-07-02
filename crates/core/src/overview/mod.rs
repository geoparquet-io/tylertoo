//! GeoParquet multi-resolution overviews.
//!
//! See `context/OVERVIEWS_SPEC.md` for the authoritative format spec.
//!
//! This module is assembled from several sibling submodules (level
//! assignment, per-level simplification, writer, reader). Only the pure
//! level-assignment engine lives here so far.

pub mod assign;
