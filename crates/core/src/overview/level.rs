//! Shared metadata model for GeoParquet overviews.
//!
//! This module implements the footer metadata schema of the GeoParquet
//! Overviews specification (`context/OVERVIEWS_SPEC.md`, §3), the GSD/zoom
//! mapping (§5.2), and the structural validation rules (§3.3, §3.4).
//!
//! The [`OverviewsMeta`] type (de)serializes to/from the exact JSON of the
//! spec examples (§3.6, §3.7, §9.1). It is written into the Parquet footer
//! under a single named key, [`OVERVIEWS_KEY`], and — for `partitioning`
//! files, behind an explicit writer flag — an optional COGP-compatible subset
//! under [`COGP_KEY`].

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize};

/// Footer metadata key under which the overviews JSON object is stored.
///
/// Per spec §3.1 / Q2 (approved): the incubation key is `geo:overviews`,
/// designed for verbatim merger into the `geo` metadata via the official
/// GeoParquet spec process. Implementations MUST keep it a single named
/// constant so the eventual rename is a one-line change.
pub const OVERVIEWS_KEY: &str = "geo:overviews";

/// Footer metadata key for the OPTIONAL COGP compatibility subset (§3.1).
///
/// Emitted only behind an explicit writer flag (default off). Contains the
/// COGP-subset fields (`version`, `levels[].{row_group_end, gsd}`).
pub const COGP_KEY: &str = "cogp";

/// The spec version emitted by this implementation (semver MAJOR.MINOR.PATCH).
pub const SPEC_VERSION: &str = "0.2.0";

/// Web Mercator equatorial circumference in meters (§5.2).
pub const WEBMERC_CIRCUMFERENCE_M: f64 = 40_075_016.69;

/// Assumed tile-band reference for the GSD derivation (§5.2, ~4x a 256px tile).
pub const GSD_TILE_BASE: f64 = 1024.0;

/// Equatorial meters-per-degree factor for geographic (degree) CRS inputs (§7.1).
pub const METERS_PER_DEGREE: f64 = 111_320.0;

/// Coordinate reference system of the overview file (spec Q3 / §7.1).
///
/// v0.1 restricts overviews to these two CRSs. The variant governs how a
/// meters-denominated GSD tolerance is expressed in the geometry's
/// coordinate units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crs {
    /// Geographic lon/lat degrees. Meters are divided by
    /// [`METERS_PER_DEGREE`] (equatorial approximation; high-latitude
    /// datasets see GSD/scale skew, acceptable per §7.1).
    Epsg4326,
    /// Web Mercator meters. Meters are used verbatim.
    Epsg3857,
}

impl Crs {
    /// Convert a distance in meters into this CRS's coordinate units.
    #[inline]
    pub fn meters_to_units(self, meters: f64) -> f64 {
        match self {
            Crs::Epsg3857 => meters,
            Crs::Epsg4326 => meters / METERS_PER_DEGREE,
        }
    }
}

/// Ground sample distance (meters) for a Web Mercator zoom level (§5.2), using
/// an explicit tile-band `base` instead of the [`GSD_TILE_BASE`] default.
///
/// `gsd(z, base) = 40_075_016.69 / base / 2^z`. The `base` is the cogp-rs
/// CLI-configurable knob (spec Q6): a larger base yields smaller GSDs at every
/// zoom (finer detail retained / less thinning), a smaller base yields larger
/// GSDs (coarser / more thinning). See `docs/OVERVIEW_TUNING.md`.
pub fn gsd_with_base(z: u8, base: f64) -> f64 {
    WEBMERC_CIRCUMFERENCE_M / base / 2f64.powi(z as i32)
}

/// Ground sample distance (meters) for a Web Mercator zoom level (§5.2).
///
/// `gsd(z) = 40_075_016.69 / 1024 / 2^z` — matches the cogp-rs convention.
/// Thin wrapper over [`gsd_with_base`] with the default [`GSD_TILE_BASE`].
pub fn gsd(z: u8) -> f64 {
    gsd_with_base(z, GSD_TILE_BASE)
}

/// Inverse of [`gsd`]: the (fractional) Web Mercator zoom for a target GSD.
///
/// `z = log2(40_075_016.69 / 1024 / gsd)`. Returns a float; callers that need
/// an integer band should round/floor per their selection policy.
pub fn zoom_for_gsd(target_gsd: f64) -> f64 {
    (WEBMERC_CIRCUMFERENCE_M / GSD_TILE_BASE / target_gsd).log2()
}

/// A single overview level as stored in the footer metadata (§3.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Level {
    /// Inclusive, 0-based index of the last row group belonging to this level.
    pub row_group_end: i64,
    /// Ground sample distance in meters (> 0), strictly decreasing coarse→fine.
    pub gsd: f64,
    /// OPTIONAL Web Mercator zoom for this level (§5.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zoom: Option<u8>,
}

/// Level materialization mode (§2.2, §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Each level is a self-contained rendering of the whole dataset (§2.2).
    Duplicating,
    /// Each feature appears once at its coarsest level; prefix reads (§2.3).
    Partitioning,
}

/// Memory/throughput profile for the streaming pass-2 engine.
///
/// The engine reads the input **once** and pipelines Parquet read/decode with
/// parallel per-feature simplification, buffering each output level until it is
/// written (levels are written coarse→fine). This profile selects how that
/// per-level output is buffered — it changes **speed and peak memory only**;
/// output bytes are identical across all profiles and thread counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryProfile {
    /// Resolve per mode + estimated output size at convert entry (duplicating →
    /// [`Speed`](Self::Speed), partitioning → [`Bounded`](Self::Bounded); any
    /// run whose estimated buffered output exceeds a memory budget flips to
    /// `Bounded`). The resolved choice is logged. This is the default.
    #[default]
    Auto,
    /// Buffer each output level's rows in RAM before writing. Fastest; peak RAM
    /// grows with total buffered output.
    Speed,
    /// Spill each output level's rows to a temporary Arrow IPC file and stream
    /// them back at write time, capping peak RAM.
    Bounded,
}

/// OPTIONAL, informative per-level generalization provenance (§3.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Generalization {
    /// Engine identifier, e.g. `"gpq-tiles 0.6.0"`.
    pub engine: String,
    /// OPTIONAL GSD tile-band base used to derive per-level GSDs from zooms
    /// (spec §5.2 / Q6; the cogp-rs `base` knob). Absent when the default
    /// [`GSD_TILE_BASE`] (1024) was used — the footer `levels[].gsd` already
    /// imply it — and present (a single named provenance value) only when a
    /// non-default `--gsd-base` was chosen. Readers MUST tolerate its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gsd_base: Option<f64>,
    /// Parallel to the top-level `levels` array.
    pub levels: Vec<GeneralizationLevel>,
    /// OPTIONAL cell-winner ranking provenance (§3.5, additive; informative).
    ///
    /// Records how the assignment engine broke ties for which feature wins a
    /// grid cell (class-aware priority, Q1). Absent on files written before
    /// this field existed; readers MUST tolerate its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ranking: Option<RankingProvenance>,
    /// OPTIONAL density-budget provenance (§3.5, additive; informative).
    ///
    /// Records the per-level density budget applied after cell-winner thinning
    /// (Q2): the geometric drop-rate and the spatial-fairness gamma. Absent when
    /// the budget was disabled (`--no-density-drop`) or on files written before
    /// this field existed; readers MUST tolerate its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub density_drop: Option<DensityProvenance>,
    /// OPTIONAL point-clustering provenance (§3.5, additive; spec §12 draft).
    ///
    /// Records that clustering was enabled (Q4): the name of the cluster-size
    /// column (`point_count`) and any accumulated attribute columns. Absent
    /// when clustering was off (`--cluster` not passed) or on files written
    /// before this field existed; readers MUST tolerate its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clustering: Option<ClusteringProvenance>,
    /// OPTIONAL line-coalescing provenance (§3.5, additive; spec §13 draft).
    ///
    /// Records that line network coalescing was enabled (Q3): the endpoint
    /// snap tolerance (in GSD multiples) and the name of the
    /// merged-segment-count column (`coalesced_count`). Absent when
    /// coalescing was off (`--coalesce-lines` not passed) or on files
    /// written before this field existed; readers MUST tolerate its absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coalescing: Option<CoalescingProvenance>,
}

/// How line coalescing was applied for a conversion (Q3, spec §13 draft).
///
/// Additive, OPTIONAL provenance embedded in [`Generalization`] (§3.5).
/// When present with `enabled: true`, the file carries the named
/// `coalesced_count` column (INT32 NOT NULL, all 1 at the canonical level);
/// the validator checks those structural facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoalescingProvenance {
    /// Whether coalescing was applied. Always `true` when the block is
    /// emitted; present for forward compatibility.
    pub enabled: bool,
    /// Endpoint snap tolerance in GSD multiples (per level, the snap
    /// distance is `snap_tolerance_gsd_factor × gsd`).
    pub snap_tolerance_gsd_factor: f64,
    /// Junction continuation threshold, degrees (`0` = strict degree-2
    /// chaining; §13.4). REQUIRED on v0.2.0 writers; `None` only when
    /// reading a file written before the member existed — readers MUST
    /// treat absence as *unknown*, never as an implied default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub junction_angle: Option<f64>,
    /// Per-level candidate-line ceiling (§13.4): levels whose candidate
    /// line count exceeded it were written uncoalesced (memory guard).
    /// REQUIRED on v0.2.0 writers; `None` only when reading a file written
    /// before the member existed — readers MUST treat absence as
    /// *unknown*, never as an implied default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_level_rows: Option<u64>,
    /// Name of the merged-segment-count column (this implementation:
    /// `coalesced_count`).
    pub coalesced_count_column: String,
}

/// How point clustering was applied for a conversion (Q4, spec §12 draft).
///
/// Additive, OPTIONAL provenance embedded in [`Generalization`] (§3.5).
/// When present with `enabled: true`, the file carries the named
/// `point_count` column (INT64 NOT NULL) and the listed accumulated columns;
/// the validator checks those structural facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusteringProvenance {
    /// Whether clustering was applied. Always `true` when the block is
    /// emitted; present for forward compatibility.
    pub enabled: bool,
    /// Name of the cluster-size column (this implementation: `point_count`).
    pub point_count_column: String,
    /// Columns whose values were aggregated across clustered points.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accumulated: Vec<AccumulatedColumn>,
}

/// One accumulated attribute column in [`ClusteringProvenance`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccumulatedColumn {
    /// Source column name.
    pub column: String,
    /// Aggregation operator: `sum`, `max`, `min`, or `mean`.
    pub op: String,
}

/// How the per-level density budget was applied for a conversion (Q2).
///
/// Additive, OPTIONAL, informative provenance embedded in [`Generalization`]
/// (§3.5). Records the tippecanoe-style drop-rate budget and the spatial
/// fairness (gamma) used to decide which cell-winner survivors were dropped to
/// meet each level's budget.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DensityProvenance {
    /// Per-level geometric drop rate: each coarser level keeps `1/drop_rate` of
    /// the next finer level's feature budget.
    pub drop_rate: f64,
    /// Spatial-fairness strength: `1.0` = proportional cut; larger = sublinear
    /// (dense neighborhoods kept to the `1/gamma` power of their population).
    pub gamma: f64,
    /// Super-cell edge length used for fairness, as a multiple of the level GSD.
    pub supercell_gsd_factor: f64,
}

/// How the cell-winner priority (sort) key was chosen for a conversion (Q1).
///
/// Additive, OPTIONAL, informative provenance embedded in [`Generalization`]
/// (§3.5). Records the ranking *tier* that produced the per-feature sort key,
/// so a reviewer can see whether a coarse render was ranked by class, by a
/// numeric column, or fell back to size.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RankingProvenance {
    /// One of: `explicit-sort-key`, `class-ranking`, `auto-overture-roads`,
    /// `auto-confidence`, `size-fallback`.
    pub mode: String,
    /// Source column the ranking read, when one applies (`size-fallback` has
    /// none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// The categorical value→priority map, when small enough to be useful
    /// (class-ranking tiers only). Higher priority wins the cell.
    ///
    /// Serialized as a JSON object map (spec §3.5 v0.2.0), e.g.
    /// `{"motorway": 5, "primary": 4}`. Deserialization additionally
    /// accepts the legacy array-of-pairs shape (`[["motorway", 5.0], …]`)
    /// emitted by gpq-tiles ≤ the 0.1.0/interim footers, so older files
    /// keep reading; only the map shape is ever written.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_ranks"
    )]
    pub ranks: Option<BTreeMap<String, f64>>,
    /// Priority assigned to a present-but-unrecognized categorical value
    /// (class-ranking tiers only). Loses to every named rank, beats a null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unknown_rank: Option<f64>,
}

/// Deserialize [`RankingProvenance::ranks`], accepting both the v0.2.0
/// object-map shape (`{"motorway": 5.0}`) and the legacy array-of-pairs
/// shape (`[["motorway", 5.0]]`) that pre-alignment gpq-tiles footers
/// carry. Only the map shape is ever serialized.
fn deserialize_ranks<'de, D>(deserializer: D) -> Result<Option<BTreeMap<String, f64>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RanksShape {
        Map(BTreeMap<String, f64>),
        LegacyPairs(Vec<(String, f64)>),
    }
    Ok(
        Option::<RanksShape>::deserialize(deserializer)?.map(|s| match s {
            RanksShape::Map(m) => m,
            RanksShape::LegacyPairs(pairs) => pairs.into_iter().collect(),
        }),
    )
}

/// One entry of the [`Generalization`] provenance block (§3.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneralizationLevel {
    /// World-space simplification tolerance, meters.
    pub simplify_tolerance_m: f64,
    /// Cell-winner thinning factor.
    pub thinning_factor: f64,
    /// Minimum bbox-diagonal kept, meters.
    pub visibility_gate_m: f64,
    /// Union of geometry kinds present at this level.
    pub geometry_types: Vec<String>,
}

/// The full `geo:overviews` footer object (§3.2).
///
/// Field declaration order matches the spec examples (§3.6, §3.7) so that the
/// serialized JSON reads identically.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverviewsMeta {
    /// REQUIRED semver spec version.
    pub version: String,
    /// OPTIONAL mode; absent ⇒ readers assume `partitioning` (§3.4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    /// OPTIONAL canonical level pointer (§3.4). `null` in `partitioning` mode.
    ///
    /// Not skipped when `None`: it serializes as an explicit JSON `null`, which
    /// reproduces the §3.7 example exactly. `#[serde(default)]` lets an absent
    /// key deserialize to `None` (spec: "null or absent").
    #[serde(default)]
    pub canonical_level: Option<i64>,
    /// REQUIRED, non-empty, coarse→fine.
    pub levels: Vec<Level>,
    /// OPTIONAL provenance (§3.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generalization: Option<Generalization>,
}

/// Structural validation errors (§3.3, §3.4). One variant per rule so callers
/// (and tests) can assert exactly which invariant was violated.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OverviewValidationError {
    /// `levels` is empty (§3.3).
    #[error("levels must be non-empty")]
    EmptyLevels,
    /// `version` is not a valid `MAJOR.MINOR.PATCH` semver string (§3.2).
    #[error("version {0:?} is not valid semver MAJOR.MINOR.PATCH")]
    InvalidVersion(String),
    /// A `row_group_end` is < 0 or >= `num_row_groups` (§3.3).
    #[error("levels[{index}].row_group_end = {value} out of range [0, {num_row_groups})")]
    RowGroupOutOfRange {
        /// Level index.
        index: usize,
        /// Offending value.
        value: i64,
        /// Total row group count.
        num_row_groups: i64,
    },
    /// `row_group_end` is not strictly increasing (§3.3).
    #[error(
        "levels[{index}].row_group_end = {value} is not strictly greater than previous {previous}"
    )]
    RowGroupNotIncreasing {
        /// Level index.
        index: usize,
        /// Offending value.
        value: i64,
        /// Previous level's value.
        previous: i64,
    },
    /// Final `row_group_end` != `num_row_groups - 1` (§3.3).
    #[error("final row_group_end = {final_value} must equal num_row_groups - 1 = {expected}")]
    RowGroupFinalMismatch {
        /// The last level's `row_group_end`.
        final_value: i64,
        /// Expected value (`num_row_groups - 1`).
        expected: i64,
    },
    /// A `gsd` is not > 0 (§3.3).
    #[error("levels[{index}].gsd = {value} must be > 0")]
    GsdNotPositive {
        /// Level index.
        index: usize,
        /// Offending value.
        value: f64,
    },
    /// `gsd` is not strictly decreasing (§3.3).
    #[error("levels[{index}].gsd = {value} is not strictly less than previous {previous}")]
    GsdNotDecreasing {
        /// Level index.
        index: usize,
        /// Offending value.
        value: f64,
        /// Previous level's value.
        previous: f64,
    },
    /// `zoom` is present on some but not all levels (§3.3, all-or-none).
    #[error("zoom must be present on all levels or none; levels[{index}] disagrees")]
    ZoomPartial {
        /// Level index that broke the all-or-none rule.
        index: usize,
    },
    /// `zoom` is not strictly increasing (§3.3).
    #[error("levels[{index}].zoom = {value} is not strictly greater than previous {previous}")]
    ZoomNotIncreasing {
        /// Level index.
        index: usize,
        /// Offending value.
        value: u8,
        /// Previous level's value.
        previous: u8,
    },
    /// `duplicating` mode with `canonical_level` != `L-1` (§3.4).
    #[error("duplicating mode requires canonical_level = {expected} (L-1), got {actual:?}")]
    CanonicalLevelMismatch {
        /// Expected value (`levels.len() - 1`).
        expected: i64,
        /// Actual value found.
        actual: Option<i64>,
    },
    /// `partitioning` (or absent) mode with a non-null `canonical_level` (§3.4).
    #[error("partitioning mode requires canonical_level = null, got {actual}")]
    CanonicalLevelNotNull {
        /// Actual value found.
        actual: i64,
    },
}

impl OverviewsMeta {
    /// Serialize to the footer JSON string (`geo:overviews` value).
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Parse from a footer JSON string.
    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }

    /// Serialize the OPTIONAL COGP-compatibility subset (§3.1): `version` and
    /// `levels[].{row_group_end, gsd}` only. Value of the [`COGP_KEY`] footer
    /// key when the writer's compatibility flag is enabled.
    pub fn to_cogp_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&CogpMeta {
            version: self.version.clone(),
            levels: self
                .levels
                .iter()
                .map(|l| CogpLevel {
                    row_group_end: l.row_group_end,
                    gsd: l.gsd,
                })
                .collect(),
        })
    }

    /// Validate the structural invariants of §3.3 and §3.4 against the actual
    /// number of row groups in the file.
    pub fn validate(&self, num_row_groups: i64) -> Result<(), OverviewValidationError> {
        if !is_semver(&self.version) {
            return Err(OverviewValidationError::InvalidVersion(
                self.version.clone(),
            ));
        }

        if self.levels.is_empty() {
            return Err(OverviewValidationError::EmptyLevels);
        }

        // row_group_end: in range, strictly increasing, final == num_row_groups - 1.
        let mut prev_end: Option<i64> = None;
        for (i, level) in self.levels.iter().enumerate() {
            if level.row_group_end < 0 || level.row_group_end >= num_row_groups {
                return Err(OverviewValidationError::RowGroupOutOfRange {
                    index: i,
                    value: level.row_group_end,
                    num_row_groups,
                });
            }
            if let Some(prev) = prev_end {
                if level.row_group_end <= prev {
                    return Err(OverviewValidationError::RowGroupNotIncreasing {
                        index: i,
                        value: level.row_group_end,
                        previous: prev,
                    });
                }
            }
            prev_end = Some(level.row_group_end);
        }
        let final_end = self.levels.last().unwrap().row_group_end;
        if final_end != num_row_groups - 1 {
            return Err(OverviewValidationError::RowGroupFinalMismatch {
                final_value: final_end,
                expected: num_row_groups - 1,
            });
        }

        // gsd: all > 0, strictly decreasing.
        let mut prev_gsd: Option<f64> = None;
        for (i, level) in self.levels.iter().enumerate() {
            // NaN-safe positivity check (avoids negated comparison on f64).
            if !matches!(
                level.gsd.partial_cmp(&0.0),
                Some(std::cmp::Ordering::Greater)
            ) {
                return Err(OverviewValidationError::GsdNotPositive {
                    index: i,
                    value: level.gsd,
                });
            }
            if let Some(prev) = prev_gsd {
                if level.gsd >= prev {
                    return Err(OverviewValidationError::GsdNotDecreasing {
                        index: i,
                        value: level.gsd,
                        previous: prev,
                    });
                }
            }
            prev_gsd = Some(level.gsd);
        }

        // zoom: all-or-none; strictly increasing where present.
        let any_zoom = self.levels.iter().any(|l| l.zoom.is_some());
        if any_zoom {
            let mut prev_zoom: Option<u8> = None;
            for (i, level) in self.levels.iter().enumerate() {
                let Some(z) = level.zoom else {
                    return Err(OverviewValidationError::ZoomPartial { index: i });
                };
                if let Some(prev) = prev_zoom {
                    if z <= prev {
                        return Err(OverviewValidationError::ZoomNotIncreasing {
                            index: i,
                            value: z,
                            previous: prev,
                        });
                    }
                }
                prev_zoom = Some(z);
            }
        }

        // mode / canonical_level consistency (§3.4). Absent mode ⇒ partitioning.
        match self.mode {
            Some(Mode::Duplicating) => {
                let expected = self.levels.len() as i64 - 1;
                if self.canonical_level != Some(expected) {
                    return Err(OverviewValidationError::CanonicalLevelMismatch {
                        expected,
                        actual: self.canonical_level,
                    });
                }
            }
            Some(Mode::Partitioning) | None => {
                if let Some(actual) = self.canonical_level {
                    return Err(OverviewValidationError::CanonicalLevelNotNull { actual });
                }
            }
        }

        Ok(())
    }
}

/// COGP-compatibility subset serializer target (§3.1).
#[derive(Serialize)]
struct CogpMeta {
    version: String,
    levels: Vec<CogpLevel>,
}

#[derive(Serialize)]
struct CogpLevel {
    row_group_end: i64,
    gsd: f64,
}

/// Minimal `MAJOR.MINOR.PATCH` semver check (all numeric, three parts).
fn is_semver(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid 3-level duplicating meta matching §9.1 for reuse.
    fn duplicating_example() -> OverviewsMeta {
        OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Duplicating),
            canonical_level: Some(2),
            levels: vec![
                Level {
                    row_group_end: 1,
                    gsd: 9783.94,
                    zoom: Some(2),
                },
                Level {
                    row_group_end: 5,
                    gsd: 2445.98,
                    zoom: Some(4),
                },
                Level {
                    row_group_end: 14,
                    gsd: 611.50,
                    zoom: Some(6),
                },
            ],
            generalization: None,
        }
    }

    fn partitioning_example() -> OverviewsMeta {
        OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Partitioning),
            canonical_level: None,
            levels: vec![
                Level {
                    row_group_end: 0,
                    gsd: 1000.0,
                    zoom: Some(6),
                },
                Level {
                    row_group_end: 3,
                    gsd: 500.0,
                    zoom: Some(7),
                },
                Level {
                    row_group_end: 12,
                    gsd: 100.0,
                    zoom: Some(9),
                },
            ],
            generalization: None,
        }
    }

    #[test]
    fn gsd_reference_values() {
        // Spec §5.2 reference table.
        assert!((gsd(2) - 9783.94).abs() < 0.01);
        assert!((gsd(4) - 2445.98).abs() < 0.01);
        assert!((gsd(6) - 611.50).abs() < 0.01);
        assert!((gsd(9) - 76.44).abs() < 0.01);
    }

    #[test]
    fn gsd_with_base_default_matches_const_gsd() {
        // The parameterized variant with the default base is identical to the
        // constant-base `gsd` (byte-for-byte f64), for every zoom.
        for z in 0u8..=16 {
            assert_eq!(gsd_with_base(z, GSD_TILE_BASE), gsd(z), "z={z}");
        }
    }

    #[test]
    fn gsd_with_base_scales_inversely_with_base() {
        // Doubling the base halves the GSD at every zoom; halving doubles it.
        for z in 0u8..=12 {
            let d = gsd(z);
            assert!((gsd_with_base(z, GSD_TILE_BASE * 2.0) - d / 2.0).abs() < 1e-9);
            assert!((gsd_with_base(z, GSD_TILE_BASE / 2.0) - d * 2.0).abs() < 1e-9);
        }
    }

    #[test]
    fn zoom_for_gsd_inverts_gsd() {
        for z in 0u8..=16 {
            let back = zoom_for_gsd(gsd(z));
            assert!((back - z as f64).abs() < 1e-9, "z={z} back={back}");
        }
    }

    #[test]
    fn roundtrip_duplicating() {
        let meta = duplicating_example();
        let json = meta.to_json().unwrap();
        let parsed = OverviewsMeta::from_json(&json).unwrap();
        assert_eq!(meta, parsed);
    }

    #[test]
    fn roundtrip_partitioning_null_canonical() {
        let meta = partitioning_example();
        let json = meta.to_json().unwrap();
        // Partitioning serializes canonical_level explicitly as null (§3.7).
        assert!(
            json.contains("\"canonical_level\":null"),
            "expected explicit null canonical_level, got {json}"
        );
        let parsed = OverviewsMeta::from_json(&json).unwrap();
        assert_eq!(meta, parsed);
    }

    #[test]
    fn parse_spec_example_field_names() {
        // Verbatim §3.6 example: exact field names must deserialize.
        let src = r#"{
            "version": "0.1.0",
            "mode": "duplicating",
            "canonical_level": 2,
            "levels": [
                { "row_group_end": 1,  "gsd": 9783.94, "zoom": 2 },
                { "row_group_end": 5,  "gsd": 2445.98, "zoom": 4 },
                { "row_group_end": 14, "gsd": 611.50,  "zoom": 6 }
            ]
        }"#;
        let meta = OverviewsMeta::from_json(src).unwrap();
        assert_eq!(meta.version, "0.1.0");
        assert_eq!(meta.mode, Some(Mode::Duplicating));
        assert_eq!(meta.canonical_level, Some(2));
        assert_eq!(meta.levels.len(), 3);
        assert_eq!(meta.levels[0].row_group_end, 1);
        assert_eq!(meta.levels[2].zoom, Some(6));
        meta.validate(15).unwrap();
    }

    #[test]
    fn parse_partitioning_example_absent_and_null_canonical_equivalent() {
        let with_null = r#"{"version":"0.1.0","mode":"partitioning","canonical_level":null,
            "levels":[{"row_group_end":0,"gsd":1000},{"row_group_end":3,"gsd":500}]}"#;
        let absent = r#"{"version":"0.1.0","mode":"partitioning",
            "levels":[{"row_group_end":0,"gsd":1000},{"row_group_end":3,"gsd":500}]}"#;
        let a = OverviewsMeta::from_json(with_null).unwrap();
        let b = OverviewsMeta::from_json(absent).unwrap();
        assert_eq!(a.canonical_level, None);
        assert_eq!(b.canonical_level, None);
        assert_eq!(a, b);
    }

    #[test]
    fn cogp_subset_serializer() {
        let meta = partitioning_example();
        let json = meta.to_cogp_json().unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["version"], "0.1.0");
        assert!(v.get("mode").is_none());
        assert!(v.get("canonical_level").is_none());
        assert_eq!(v["levels"][0]["row_group_end"], 0);
        assert_eq!(v["levels"][0]["gsd"], 1000.0);
        // Only the two COGP-subset fields per level.
        assert!(v["levels"][0].get("zoom").is_none());
    }

    #[test]
    fn validate_accepts_valid_duplicating() {
        duplicating_example().validate(15).unwrap();
    }

    #[test]
    fn validate_rejects_empty_levels() {
        let meta = OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Duplicating),
            canonical_level: Some(0),
            levels: vec![],
            generalization: None,
        };
        assert_eq!(
            meta.validate(0).unwrap_err(),
            OverviewValidationError::EmptyLevels
        );
    }

    #[test]
    fn validate_rejects_bad_version() {
        let mut meta = duplicating_example();
        meta.version = "1.0".to_string();
        assert!(matches!(
            meta.validate(15).unwrap_err(),
            OverviewValidationError::InvalidVersion(_)
        ));
    }

    #[test]
    fn validate_rejects_row_group_out_of_range() {
        let meta = duplicating_example();
        // final row_group_end is 14 -> requires num_row_groups >= 15.
        assert!(matches!(
            meta.validate(10).unwrap_err(),
            OverviewValidationError::RowGroupOutOfRange { .. }
        ));
    }

    #[test]
    fn validate_rejects_non_increasing_row_group_end() {
        let mut meta = duplicating_example();
        meta.levels[1].row_group_end = 1; // equal to level 0 -> not strictly increasing
        assert!(matches!(
            meta.validate(15).unwrap_err(),
            OverviewValidationError::RowGroupNotIncreasing { .. }
        ));
    }

    #[test]
    fn validate_rejects_final_row_group_mismatch() {
        let meta = duplicating_example(); // final = 14
        assert!(matches!(
            meta.validate(20).unwrap_err(),
            OverviewValidationError::RowGroupFinalMismatch {
                final_value: 14,
                expected: 19
            }
        ));
    }

    #[test]
    fn validate_rejects_non_positive_gsd() {
        let mut meta = duplicating_example();
        meta.levels[2].gsd = 0.0;
        assert!(matches!(
            meta.validate(15).unwrap_err(),
            OverviewValidationError::GsdNotPositive { .. }
        ));
    }

    #[test]
    fn validate_rejects_non_decreasing_gsd() {
        let mut meta = duplicating_example();
        meta.levels[1].gsd = 9783.94; // equal to level 0
        assert!(matches!(
            meta.validate(15).unwrap_err(),
            OverviewValidationError::GsdNotDecreasing { .. }
        ));
    }

    #[test]
    fn validate_rejects_partial_zoom() {
        let mut meta = duplicating_example();
        meta.levels[1].zoom = None;
        assert!(matches!(
            meta.validate(15).unwrap_err(),
            OverviewValidationError::ZoomPartial { index: 1 }
        ));
    }

    #[test]
    fn validate_rejects_non_increasing_zoom() {
        let mut meta = duplicating_example();
        meta.levels[1].zoom = Some(2); // equal to level 0
        assert!(matches!(
            meta.validate(15).unwrap_err(),
            OverviewValidationError::ZoomNotIncreasing { .. }
        ));
    }

    #[test]
    fn validate_rejects_duplicating_wrong_canonical() {
        let mut meta = duplicating_example();
        meta.canonical_level = Some(1); // should be 2
        assert!(matches!(
            meta.validate(15).unwrap_err(),
            OverviewValidationError::CanonicalLevelMismatch { expected: 2, .. }
        ));
    }

    #[test]
    fn validate_rejects_partitioning_non_null_canonical() {
        let mut meta = partitioning_example();
        meta.canonical_level = Some(2);
        assert!(matches!(
            meta.validate(13).unwrap_err(),
            OverviewValidationError::CanonicalLevelNotNull { actual: 2 }
        ));
    }

    #[test]
    fn validate_absent_mode_requires_null_canonical() {
        let mut meta = partitioning_example();
        meta.mode = None;
        meta.canonical_level = None;
        meta.validate(13).unwrap();
        meta.canonical_level = Some(0);
        assert!(matches!(
            meta.validate(13).unwrap_err(),
            OverviewValidationError::CanonicalLevelNotNull { .. }
        ));
    }

    #[test]
    fn ranking_provenance_roundtrip_class_ranking() {
        // A class-ranking generalization block survives JSON round-trip and
        // preserves mode / column / ranks / unknown_rank (§3.5, additive).
        let mut meta = duplicating_example();
        meta.generalization = Some(Generalization {
            engine: "gpq-tiles test".to_string(),
            gsd_base: None,
            levels: vec![],
            ranking: Some(RankingProvenance {
                mode: "auto-overture-roads".to_string(),
                column: Some("road_class".to_string()),
                ranks: Some(BTreeMap::from([
                    ("motorway".to_string(), 18.0),
                    ("service".to_string(), 11.0),
                ])),
                unknown_rank: Some(0.0),
            }),
            density_drop: None,
            clustering: None,
            coalescing: None,
        });
        let json = meta.to_json().unwrap();
        // v0.2.0 (§3.5): ranks serialize as a JSON object map, NOT pairs.
        assert!(
            json.contains(r#""ranks":{"motorway":18.0,"service":11.0}"#),
            "ranks must serialize as an object map, got {json}"
        );
        let parsed = OverviewsMeta::from_json(&json).unwrap();
        assert_eq!(meta, parsed);
        let r = parsed.generalization.unwrap().ranking.unwrap();
        assert_eq!(r.mode, "auto-overture-roads");
        assert_eq!(r.column.as_deref(), Some("road_class"));
        assert_eq!(r.unknown_rank, Some(0.0));
        assert_eq!(r.ranks.unwrap().len(), 2);
    }

    #[test]
    fn ranking_ranks_legacy_array_of_pairs_still_reads() {
        // Files written before the v0.2.0 alignment carry ranks as an array
        // of [value, priority] pairs; readers accept both shapes but only
        // ever emit the map.
        let src = r#"{
            "version": "0.1.0", "mode": "duplicating", "canonical_level": 0,
            "levels": [ { "row_group_end": 0, "gsd": 611.50, "zoom": 6 } ],
            "generalization": {
                "engine": "gpq-tiles 0.5.0",
                "levels": [],
                "ranking": {
                    "mode": "class-ranking",
                    "column": "road_class",
                    "ranks": [["motorway", 5.0], ["primary", 4.0]],
                    "unknown_rank": 0.0
                }
            }
        }"#;
        let meta = OverviewsMeta::from_json(src).unwrap();
        let r = meta.generalization.clone().unwrap().ranking.unwrap();
        let ranks = r.ranks.unwrap();
        assert_eq!(ranks.get("motorway"), Some(&5.0));
        assert_eq!(ranks.get("primary"), Some(&4.0));
        // Re-serialization normalizes to the object-map shape.
        let json = meta.to_json().unwrap();
        assert!(
            json.contains(r#""ranks":{"motorway":5.0,"primary":4.0}"#),
            "re-emit must use the map shape, got {json}"
        );
    }

    #[test]
    fn density_provenance_roundtrip_and_absent_tolerated() {
        // A density_drop block survives JSON round-trip preserving its fields,
        // and a generalization block without the key deserializes to None.
        let mut meta = duplicating_example();
        meta.generalization = Some(Generalization {
            engine: "gpq-tiles test".to_string(),
            gsd_base: None,
            levels: vec![],
            ranking: None,
            density_drop: Some(DensityProvenance {
                drop_rate: 1.8,
                gamma: 1.5,
                supercell_gsd_factor: 128.0,
            }),
            clustering: None,
            coalescing: None,
        });
        let json = meta.to_json().unwrap();
        let parsed = OverviewsMeta::from_json(&json).unwrap();
        assert_eq!(meta, parsed);
        let d = parsed.generalization.unwrap().density_drop.unwrap();
        assert_eq!(d.drop_rate, 1.8);
        assert_eq!(d.gamma, 1.5);
        assert_eq!(d.supercell_gsd_factor, 128.0);

        // Absent key → None (additive-field tolerance).
        let src = r#"{
            "version": "0.1.0", "mode": "duplicating", "canonical_level": 0,
            "levels": [ { "row_group_end": 0, "gsd": 611.50, "zoom": 6 } ],
            "generalization": {
                "engine": "gpq-tiles 0.1.0",
                "levels": []
            }
        }"#;
        let m = OverviewsMeta::from_json(src).unwrap();
        assert!(m.generalization.unwrap().density_drop.is_none());
    }

    #[test]
    fn coalescing_provenance_roundtrip_and_absent_tolerated() {
        // A coalescing block survives JSON round-trip; absent key → None.
        let mut meta = duplicating_example();
        meta.generalization = Some(Generalization {
            engine: "gpq-tiles test".to_string(),
            gsd_base: None,
            levels: vec![],
            ranking: None,
            density_drop: None,
            clustering: None,
            coalescing: Some(CoalescingProvenance {
                enabled: true,
                snap_tolerance_gsd_factor: 1.0,
                junction_angle: Some(0.0),
                max_level_rows: Some(2_000_000),
                coalesced_count_column: "coalesced_count".to_string(),
            }),
        });
        let json = meta.to_json().unwrap();
        // §13.4 (v0.2.0): all five members present when emitted.
        for key in [
            r#""enabled":true"#,
            r#""snap_tolerance_gsd_factor":1.0"#,
            r#""junction_angle":0.0"#,
            r#""max_level_rows":2000000"#,
            r#""coalesced_count_column":"coalesced_count""#,
        ] {
            assert!(json.contains(key), "missing {key} in {json}");
        }
        let parsed = OverviewsMeta::from_json(&json).unwrap();
        assert_eq!(meta, parsed);
        let c = parsed.generalization.unwrap().coalescing.unwrap();
        assert!(c.enabled);
        assert_eq!(c.snap_tolerance_gsd_factor, 1.0);
        assert_eq!(c.junction_angle, Some(0.0));
        assert_eq!(c.max_level_rows, Some(2_000_000));
        assert_eq!(c.coalesced_count_column, "coalesced_count");

        // Absent key → None (additive-field tolerance).
        let src = r#"{
            "version": "0.1.0", "mode": "duplicating", "canonical_level": 0,
            "levels": [ { "row_group_end": 0, "gsd": 611.50, "zoom": 6 } ],
            "generalization": { "engine": "gpq-tiles 0.1.0", "levels": [] }
        }"#;
        let m = OverviewsMeta::from_json(src).unwrap();
        assert!(m.generalization.unwrap().coalescing.is_none());
    }

    #[test]
    fn coalescing_provenance_older_file_missing_new_members_reads_as_unknown() {
        // A pre-v0.2.0-alignment footer records only the snap factor; the
        // two new members MUST deserialize as None (unknown), never as an
        // implied default (§13.4 read-compat).
        let src = r#"{
            "version": "0.1.0", "mode": "duplicating", "canonical_level": 0,
            "levels": [ { "row_group_end": 0, "gsd": 611.50, "zoom": 6 } ],
            "generalization": {
                "engine": "gpq-tiles 0.5.0", "levels": [],
                "coalescing": {
                    "enabled": true,
                    "snap_tolerance_gsd_factor": 1.0,
                    "coalesced_count_column": "coalesced_count"
                }
            }
        }"#;
        let m = OverviewsMeta::from_json(src).unwrap();
        let c = m.generalization.unwrap().coalescing.unwrap();
        assert!(c.enabled);
        assert_eq!(c.junction_angle, None, "absent means unknown, not 0");
        assert_eq!(
            c.max_level_rows, None,
            "absent means unknown, not a default"
        );
    }

    #[test]
    fn clustering_provenance_roundtrip_and_absent_tolerated() {
        // A clustering block survives JSON round-trip; absent key → None.
        let mut meta = duplicating_example();
        meta.generalization = Some(Generalization {
            engine: "gpq-tiles test".to_string(),
            gsd_base: None,
            levels: vec![],
            ranking: None,
            density_drop: None,
            clustering: Some(ClusteringProvenance {
                enabled: true,
                point_count_column: "point_count".to_string(),
                accumulated: vec![AccumulatedColumn {
                    column: "confidence".to_string(),
                    op: "mean".to_string(),
                }],
            }),
            coalescing: None,
        });
        let json = meta.to_json().unwrap();
        let parsed = OverviewsMeta::from_json(&json).unwrap();
        assert_eq!(meta, parsed);
        let c = parsed.generalization.unwrap().clustering.unwrap();
        assert!(c.enabled);
        assert_eq!(c.point_count_column, "point_count");
        assert_eq!(c.accumulated.len(), 1);
        assert_eq!(c.accumulated[0].column, "confidence");
        assert_eq!(c.accumulated[0].op, "mean");

        // Absent key → None (additive-field tolerance); empty accumulated
        // list is omitted from serialization.
        let src = r#"{
            "version": "0.1.0", "mode": "duplicating", "canonical_level": 0,
            "levels": [ { "row_group_end": 0, "gsd": 611.50, "zoom": 6 } ],
            "generalization": { "engine": "gpq-tiles 0.1.0", "levels": [] }
        }"#;
        let m = OverviewsMeta::from_json(src).unwrap();
        assert!(m.generalization.unwrap().clustering.is_none());
    }

    #[test]
    fn ranking_provenance_absent_field_tolerated() {
        // A generalization block written before `ranking` existed (no key)
        // deserializes with `ranking == None`.
        let src = r#"{
            "version": "0.1.0",
            "mode": "duplicating",
            "canonical_level": 0,
            "levels": [ { "row_group_end": 0, "gsd": 611.50, "zoom": 6 } ],
            "generalization": {
                "engine": "gpq-tiles 0.1.0",
                "levels": [
                    { "simplify_tolerance_m": 0, "thinning_factor": 1.0,
                      "visibility_gate_m": 0, "geometry_types": [] }
                ]
            }
        }"#;
        let meta = OverviewsMeta::from_json(src).unwrap();
        assert!(meta.generalization.unwrap().ranking.is_none());
    }

    #[test]
    fn validate_single_level_degenerate() {
        // §7.4 single-level duplicating file is conformant.
        let meta = OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Duplicating),
            canonical_level: Some(0),
            levels: vec![Level {
                row_group_end: 0,
                gsd: 611.5,
                zoom: Some(6),
            }],
            generalization: None,
        };
        meta.validate(1).unwrap();
    }
}
