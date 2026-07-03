//! GeoParquet multi-resolution overviews (COG-style vector overviews).
//!
//! This subtree implements the overview generalization pipeline defined in
//! `context/OVERVIEWS_SPEC.md`: read gpio-sorted GeoParquet → per-level
//! grid cell-winner thinning (`assign`) → per-level **world-space**
//! geometry simplification with tolerance derived from level GSD, not
//! tile pixels (`simplify`) → level-banded GeoParquet writer (`writer`),
//! with the shared metadata model in `level`. No tile clipping, no MVT,
//! no PMTiles.

pub mod assign;
pub mod check;
pub mod convert;
pub mod export;
pub mod level;
pub mod reader;
pub mod simplify;
pub mod writer;
