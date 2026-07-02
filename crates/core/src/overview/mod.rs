//! GeoParquet multi-resolution overviews (COG-style vector overviews).
//!
//! This subtree implements the overview generalization pipeline defined in
//! `context/OVERVIEWS_SPEC.md`: read gpio-sorted GeoParquet → per-level
//! grid cell-winner thinning → per-level **world-space** geometry
//! simplification (tolerance derived from level GSD, not tile pixels) →
//! level-banded GeoParquet writer. No tile clipping, no MVT, no PMTiles.
//!
//! Sibling modules (`assign`, `writer`, `reader`, `level`) are added by
//! their own tasks and merged separately; this file intentionally declares
//! only what task P2 owns.

pub mod simplify;
