//! GeoParquet multi-resolution overviews (COG-style vector overviews) —
//! the product pipeline of this crate.
//!
//! This subtree implements the overview generalization pipeline defined in
//! `context/OVERVIEWS_SPEC.md`: read gpio-sorted GeoParquet → per-level
//! grid cell-winner thinning (`assign`) → per-level **world-space**
//! geometry simplification with tolerance derived from level GSD, not
//! tile pixels (`simplify`) → level-banded GeoParquet writer (`writer`),
//! with the shared metadata model in `level`. The conversion itself does
//! no tile clipping, no MVT, no PMTiles; `export` then turns an overview
//! file into a PMTiles archive (one zoom per level), and `check` validates
//! files against the spec (§6.2).

pub mod assign;
pub mod check;
pub mod cluster;
pub mod coalesce;
pub mod convert;
pub mod export;
pub mod filter;
#[cfg(test)]
mod hostile;
pub mod level;
mod pipeline;
pub mod reader;
pub mod simplify;
mod stream;
pub mod writer;
