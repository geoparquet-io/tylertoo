//! GeoParquet multi-resolution overviews (COG-style) — see
//! `context/OVERVIEWS_SPEC.md`.
//!
//! Sibling tasks add their own module declarations here; keep this minimal.

pub mod level;
pub mod writer;
