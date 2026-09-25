//! Cut a dataset's tile space into N disjoint shards (#498) — the first half
//! of a sharded build, the half [`crate::merge`] has been waiting for.
//!
//! ## The geometry
//!
//! A shard is a contiguous run of **pivot-zoom tile ids**. PMTiles orders a
//! zoom's tiles along the Hilbert curve and a node's descendants occupy an
//! exact contiguous id interval at every deeper zoom
//! ([`crate::tile::node_id_range`]), so a contiguous run of pivot tiles
//! `[lo, hi]` names, at every zoom `z >= pivot`, one contiguous id interval
//! too — and the N runs partition the pivot zoom, hence partition every
//! deeper zoom. Shards are therefore disjoint **by construction**, not by
//! bbox intersection: a feature that straddles a shard seam is read by both
//! shards, clipped normally by both, and each emits only the tiles its own
//! range owns. No border double-inclusion, no dedup pass, nothing to
//! re-encode at merge time — which is exactly the flaw the `--bbox`-band
//! workaround #498 describes could not fix.
//!
//! Zooms **below** the pivot belong to no shard. One coarse job owns them
//! (see the workflow below), and because ids ascend with zoom, its ids all
//! sort before every shard's — so coarse and shards merge in the same
//! `tylertoo merge` with nothing special done for either.
//!
//! ## Balancing
//!
//! Equal id-width would be a terrible cut: the tile space is uniform and data
//! never is. [`ShardPlan::compute`] instead estimates rows per pivot tile
//! from **footer metadata only** — each row group's bbox (via the tiered
//! GeoParquet-1.1-covering / GP2.0-native-statistics extraction in
//! [`crate::covering`]) spread over the pivot tiles it covers, weighted by the
//! group's row count — and cuts N runs of roughly equal estimated rows. No
//! data page is read, so planning a planet-scale input costs a footer fetch.
//!
//! The estimate is deliberately crude (a row group is assumed uniform over
//! its bbox). It only has to pick cut points; correctness never depends on
//! it, and a badly balanced cut costs wall time, not tiles.
//!
//! ## The workflow
//!
//! ```text
//! # 0. Plan the cut (footer only, seconds even on a planet).
//! tylertoo shard-plan in.parquet --shards 16 --pivot 6 -o shards.json
//!
//! # 1. The coarse job: zooms [0, pivot-1] over the whole input, and the run
//! #    that writes the convert plan every shard then consumes.
//! tylertoo tiles in.parquet coarse.pmtiles --min-zoom 0 --max-zoom 14 \
//!     --shard coarse --shard-plan shards.json --save-plan convert.plan
//!
//! # 2. The shards: zooms [pivot, 14], one job each, embarrassingly parallel.
//! tylertoo tiles in.parquet shard-$i.pmtiles --min-zoom 0 --max-zoom 14 \
//!     --shard $i/16 --shard-plan shards.json --plan convert.plan
//!
//! # 3. One merge.
//! tylertoo merge out.pmtiles coarse.pmtiles shard-*.pmtiles
//! ```
//!
//! ## Why a shard MUST consume the convert plan
//!
//! The level assignment is not a per-feature function. The density budget
//! water-fills a 128 × GSD super-cell budget over *every* candidate of a
//! level, the level walk carries a running kept count coarse → fine,
//! `--magnitude-ladder` dense-ranks the column's *global* distinct values, and
//! the class-ranking auto-detection picks its column from a global vocabulary
//! scan. A shard folding over its own subset reaches a different answer and
//! the shards' pyramids disagree. So `--shard I/N` without `--plan` is a hard
//! error, and the coarse job — which reads the whole input anyway — is the run
//! that writes the plan.
//!
//! See [`crate::overview::plan_state`] for how a shard re-addresses that
//! global plan onto the subset of row groups it reads.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::RangeInclusive;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::input_set::ConvertSource;
use crate::tile::{hilbert_zoom_base, tile_ranges_for_bbox, TileBounds, TileCoord};

/// Format version of the shard-plan JSON artifact.
pub const SHARD_PLAN_VERSION: u32 = 1;

/// The `"format"` discriminator written into every shard plan, so a JSON file
/// that is not one is named as such rather than deserializing into surprise.
const SHARD_PLAN_FORMAT: &str = "tylertoo-shard-plan";

/// Deepest pivot zoom a shard plan may use.
///
/// The pivot zoom's id space is materialized twice while planning — once as
/// the estimator's weight map, once when a range is turned into the bbox that
/// prunes a shard's row groups — so the cap is what keeps both `O(4^pivot)`
/// walks bounded (z10 is 1,048,576 tiles). It is far above anything useful: a
/// pivot is meant to sit where the dataset has a few tiles per shard, which
/// for any real fleet size is z4–z8.
pub const MAX_SHARD_PIVOT_ZOOM: u8 = 10;

/// Everything that can go wrong cutting, reading or applying a shard plan.
#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    /// `--shard-pivot` outside `1..=`[`MAX_SHARD_PIVOT_ZOOM`].
    #[error(
        "--shard-pivot {pivot} is out of range: the pivot zoom must be between 1 and \
         {MAX_SHARD_PIVOT_ZOOM}. A pivot wants to sit where the dataset has a few tiles per \
         shard, which is z4-z8 for any realistic fleet size."
    )]
    PivotOutOfRange {
        /// The rejected pivot zoom.
        pivot: u8,
    },

    /// The pivot is not strictly coarser than the build's finest zoom.
    #[error(
        "--shard-pivot {pivot} must be coarser than --max-zoom {max_zoom}: shards own zooms \
         [{pivot}, {max_zoom}] and the coarse job owns [0, {}], so a pivot at or past the \
         finest zoom leaves the shards one zoom or nothing to build.",
        pivot.saturating_sub(1)
    )]
    PivotNotCoarser {
        /// The rejected pivot zoom.
        pivot: u8,
        /// The build's finest zoom.
        max_zoom: u8,
    },

    /// `--shards N` was zero or larger than the pivot zoom's tile count.
    #[error(
        "--shards {shards} is out of range for pivot z{pivot}: it must be between 1 and {max} \
         (the number of tiles at that zoom). Use a deeper --shard-pivot for a larger fleet."
    )]
    ShardCountOutOfRange {
        /// The rejected shard count.
        shards: usize,
        /// The pivot zoom it was rejected against.
        pivot: u8,
        /// The largest shard count this pivot can express.
        max: u64,
    },

    /// `--shard I/N` named a shard index at or past `N`.
    #[error(
        "--shard {index}/{shards}: the shard index must be less than the shard count, so the \
         valid indices are 0..={}",
        shards.saturating_sub(1)
    )]
    ShardIndexOutOfRange {
        /// The rejected index.
        index: usize,
        /// The shard count it was rejected against.
        shards: usize,
    },

    /// `--shard I/N` disagrees with the shard plan's own `N`.
    #[error(
        "--shard {index}/{shards} does not match {path}: that plan cuts {plan_shards} shards. \
         Every job in a fleet must be given the same shard plan, and its N must be the N the \
         plan was cut for."
    )]
    ShardCountMismatch {
        /// The index the caller asked for.
        index: usize,
        /// The count the caller asked for.
        shards: usize,
        /// The count the plan carries.
        plan_shards: usize,
        /// The plan's path, for the message.
        path: String,
    },

    /// `--shard` could not be parsed.
    #[error(
        "--shard {value:?} is not a shard selector: write `I/N` (for example `0/16`) for one \
         of the N data shards, or `coarse` for the job that owns the zooms above the pivot and \
         writes the convert plan."
    )]
    BadShardSelector {
        /// The rejected text.
        value: String,
    },

    /// `--tile-range` could not be parsed.
    #[error(
        "--tile-range {value:?} is not a tile-id range: write `LO..HI`, two PMTiles tile ids \
         at the SAME zoom (that zoom becomes the pivot, and the range then names every \
         descendant of those tiles at every deeper zoom)."
    )]
    BadTileRange {
        /// The rejected text.
        value: String,
    },

    /// A `--tile-range` whose two ids sit at different zooms.
    #[error(
        "--tile-range {lo}..{hi}: the two tile ids must be at the same zoom, but {lo} is at \
         z{lo_zoom} and {hi} is at z{hi_zoom}. A range is a run of tiles at one pivot zoom; \
         its descendants at deeper zooms follow from that."
    )]
    TileRangeZoomMismatch {
        /// The low id.
        lo: u64,
        /// The high id.
        hi: u64,
        /// The low id's zoom.
        lo_zoom: u8,
        /// The high id's zoom.
        hi_zoom: u8,
    },

    /// A `--tile-range` whose low id is past its high id.
    #[error("--tile-range {lo}..{hi}: the range is empty (LO must not exceed HI).")]
    EmptyTileRange {
        /// The low id.
        lo: u64,
        /// The high id.
        hi: u64,
    },

    /// A tile id that is not addressable at all.
    #[error("tile id {id} is not a valid PMTiles tile id: {reason}")]
    BadTileId {
        /// The rejected id.
        id: u64,
        /// Why it was rejected.
        reason: String,
    },

    /// The plan file could not be read or written.
    #[error("--shard-plan {path}: {source}")]
    Io {
        /// The plan's path.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },

    /// The plan file is not a shard plan, or is a shard plan this build
    /// cannot read.
    #[error("--shard-plan {path}: {what}")]
    Malformed {
        /// The plan's path.
        path: String,
        /// What is wrong with it.
        what: String,
    },

    /// The plan was cut for a different input than the one being tiled.
    #[error(
        "--shard-plan {path}: saved plan does not match this run: {field} was {saved:?} when \
         the plan was cut but is {now:?} now. Re-cut it with `tylertoo shard-plan`."
    )]
    SourceMismatch {
        /// The plan's path.
        path: String,
        /// Which term disagreed.
        field: String,
        /// The value the plan recorded.
        saved: String,
        /// The value this run sees.
        now: String,
    },

    /// Reading the input's footer failed.
    #[error("shard-plan: reading the input failed: {0}")]
    Input(#[from] crate::input::InputError),
}

/// Which job of a sharded fleet this run is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShardRole {
    /// The run that owns zooms `[0, pivot - 1]` over the whole input and
    /// writes the convert plan the data shards consume.
    Coarse,
    /// Data shard `index` of `shards`, owning its pivot-id run at every zoom
    /// `>= pivot`.
    Shard {
        /// This shard's index, `0 <= index < shards`.
        index: usize,
        /// The fleet size this index was given against.
        shards: usize,
    },
}

impl ShardRole {
    /// Parse a `--shard` value: `coarse`, or `I/N`.
    pub fn parse(value: &str) -> Result<Self, ShardError> {
        let trimmed = value.trim();
        if trimmed.eq_ignore_ascii_case("coarse") {
            return Ok(ShardRole::Coarse);
        }
        let bad = || ShardError::BadShardSelector {
            value: value.to_string(),
        };
        let (i, n) = trimmed.split_once('/').ok_or_else(bad)?;
        let index: usize = i.trim().parse().map_err(|_| bad())?;
        let shards: usize = n.trim().parse().map_err(|_| bad())?;
        if shards == 0 {
            return Err(bad());
        }
        if index >= shards {
            return Err(ShardError::ShardIndexOutOfRange { index, shards });
        }
        Ok(ShardRole::Shard { index, shards })
    }

    /// `true` for the coarse job.
    pub fn is_coarse(self) -> bool {
        matches!(self, ShardRole::Coarse)
    }
}

impl fmt::Display for ShardRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShardRole::Coarse => f.write_str("coarse"),
            ShardRole::Shard { index, shards } => write!(f, "{index}/{shards}"),
        }
    }
}

/// A contiguous run of pivot-zoom tile ids, and with it every descendant of
/// those tiles at every deeper zoom.
///
/// This is what an export is restricted to. Zooms **below** `pivot_zoom` are
/// outside the range entirely — [`TileRange::ids_at`] returns `None` for them
/// — which is how a data shard declines the coarse job's zooms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileRange {
    /// The zoom `lo` and `hi` are tile ids at.
    pub pivot_zoom: u8,
    /// First pivot-zoom tile id in the run, inclusive.
    pub lo: u64,
    /// Last pivot-zoom tile id in the run, inclusive.
    pub hi: u64,
}

impl TileRange {
    /// Build a range from two tile ids at `pivot_zoom`.
    pub fn new(pivot_zoom: u8, lo: u64, hi: u64) -> Result<Self, ShardError> {
        if pivot_zoom == 0 || pivot_zoom > MAX_SHARD_PIVOT_ZOOM {
            return Err(ShardError::PivotOutOfRange { pivot: pivot_zoom });
        }
        if lo > hi {
            return Err(ShardError::EmptyTileRange { lo, hi });
        }
        let base = hilbert_zoom_base(pivot_zoom);
        let last = base + zoom_tile_count(pivot_zoom) - 1;
        for id in [lo, hi] {
            if id < base || id > last {
                return Err(ShardError::BadTileId {
                    id,
                    reason: format!("it is not at z{pivot_zoom} (whose ids are {base}..={last})"),
                });
            }
        }
        Ok(TileRange { pivot_zoom, lo, hi })
    }

    /// Parse `LO..HI`, deriving the pivot zoom from the ids themselves.
    ///
    /// Both ids must sit at the same zoom; that zoom is the pivot, and the
    /// range then owns every descendant of those tiles at every deeper zoom.
    pub fn parse(value: &str) -> Result<Self, ShardError> {
        let bad = || ShardError::BadTileRange {
            value: value.to_string(),
        };
        let (lo_s, hi_s) = value.trim().split_once("..").ok_or_else(bad)?;
        let lo: u64 = lo_s.trim().parse().map_err(|_| bad())?;
        let hi: u64 = hi_s
            .trim()
            .trim_start_matches('=')
            .parse()
            .map_err(|_| bad())?;
        if lo > hi {
            return Err(ShardError::EmptyTileRange { lo, hi });
        }
        let zoom_of = |id: u64| -> Result<u8, ShardError> {
            crate::pmtiles_writer::tile_id_to_zxy(id)
                .map(|(z, _, _)| z)
                .map_err(|e| ShardError::BadTileId {
                    id,
                    reason: e.to_string(),
                })
        };
        let lo_zoom = zoom_of(lo)?;
        let hi_zoom = zoom_of(hi)?;
        if lo_zoom != hi_zoom {
            return Err(ShardError::TileRangeZoomMismatch {
                lo,
                hi,
                lo_zoom,
                hi_zoom,
            });
        }
        TileRange::new(lo_zoom, lo, hi)
    }

    /// The whole pivot zoom — the range a one-shard fleet owns.
    pub fn whole_zoom(pivot_zoom: u8) -> Result<Self, ShardError> {
        if pivot_zoom == 0 || pivot_zoom > MAX_SHARD_PIVOT_ZOOM {
            return Err(ShardError::PivotOutOfRange { pivot: pivot_zoom });
        }
        let base = hilbert_zoom_base(pivot_zoom);
        TileRange::new(pivot_zoom, base, base + zoom_tile_count(pivot_zoom) - 1)
    }

    /// How many pivot tiles the run holds.
    pub fn pivot_tiles(&self) -> u64 {
        self.hi - self.lo + 1
    }

    /// The inclusive PMTiles tile-id interval this range owns **at `zoom`**,
    /// or `None` when `zoom` is coarser than the pivot (those zooms belong to
    /// the coarse job, never to a shard).
    ///
    /// The interval is exact, not a bounding superset: a pivot node's
    /// descendants occupy `base(z) + h * 4^Δ ..= base(z) + (h+1) * 4^Δ - 1`
    /// (see [`crate::tile::node_id_range`]), and a contiguous run of `h`
    /// concatenates those blocks into one contiguous interval.
    pub fn ids_at(&self, zoom: u8) -> Option<RangeInclusive<u64>> {
        if zoom < self.pivot_zoom {
            return None;
        }
        let base_pivot = hilbert_zoom_base(self.pivot_zoom);
        let base_zoom = hilbert_zoom_base(zoom);
        let delta = u32::from(zoom - self.pivot_zoom);
        // Bounded by 4^zoom <= 4^30 < 2^61 for every zoom this crate writes:
        // `h_hi + 1 <= 4^pivot` and `span = 4^(zoom - pivot)`.
        let span = 1u64 << (2 * delta);
        let h_lo = self.lo - base_pivot;
        let h_hi = self.hi - base_pivot;
        Some(base_zoom + h_lo * span..=base_zoom + (h_hi + 1) * span - 1)
    }

    /// `true` when the tile with id `id` at `zoom` belongs to this range.
    pub fn contains(&self, zoom: u8, id: u64) -> bool {
        self.ids_at(zoom).is_some_and(|r| r.contains(&id))
    }

    /// The geographic extent of the run's pivot tiles, widened by one pivot
    /// tile on every side.
    ///
    /// This is the bbox a shard prunes its input row groups against. The
    /// margin is what makes the pruning **safe** rather than merely tight: a
    /// feature just outside a tile still renders into it through the export's
    /// edge buffer, so a row group whose bbox only grazes the run has to be
    /// read. One whole tile is far more than the buffer needs (the buffer is
    /// single-digit tile *pixels*) and over-inclusion costs a read, never a
    /// tile.
    pub fn bounds(&self) -> TileBounds {
        let mut out: Option<TileBounds> = None;
        for id in self.lo..=self.hi {
            let Ok((z, x, y)) = crate::pmtiles_writer::tile_id_to_zxy(id) else {
                continue;
            };
            debug_assert_eq!(z, self.pivot_zoom);
            let tb = TileCoord::new(x, y, z).bounds();
            match &mut out {
                Some(acc) => acc.expand(&tb),
                None => out = Some(tb),
            }
        }
        let Some(b) = out else {
            return TileBounds::new(-180.0, -90.0, 180.0, 90.0);
        };
        let margin = 360.0 / 2f64.powi(i32::from(self.pivot_zoom));
        TileBounds::new(
            (b.lng_min - margin).max(-180.0),
            (b.lat_min - margin).max(-90.0),
            (b.lng_max + margin).min(180.0),
            (b.lat_max + margin).min(90.0),
        )
    }
}

impl fmt::Display for TileRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}..{} (z{}, {} pivot tiles)",
            self.lo,
            self.hi,
            self.pivot_zoom,
            self.pivot_tiles()
        )
    }
}

/// One shard's slice of a [`ShardPlan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardRange {
    /// First pivot-zoom tile id, inclusive.
    pub lo: u64,
    /// Last pivot-zoom tile id, inclusive.
    pub hi: u64,
    /// Rows the estimator expects this shard to see, from footer bboxes
    /// alone. Advisory: it is what the cut was balanced on, and is reported
    /// so a lopsided plan is visible before a fleet burns a day on it.
    pub estimated_rows: u64,
}

/// Identity of one input part, as of the run that cut the plan.
///
/// Lighter than the convert plan's fingerprint on purpose: the convert plan
/// already pins the input hard (down to mtime), and this only has to catch
/// "this shard plan was cut for a different file".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardInput {
    /// The part's display name (local path, or remote URL).
    pub path: String,
    /// Rows the part has in total, from its parquet footer.
    pub num_rows: i64,
    /// Row groups the part has in total, from its parquet footer.
    pub row_groups: usize,
}

/// The cut: N contiguous runs of pivot-zoom tile ids that partition the pivot
/// zoom, plus what they were balanced on.
///
/// Computed once and handed to every job of the fleet, so all of them agree by
/// construction rather than by each recomputing an estimate that a differing
/// library version or a re-statted file could perturb.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardPlan {
    /// Always [`SHARD_PLAN_FORMAT`]; refused otherwise.
    pub format: String,
    /// Artifact format version.
    pub version: u32,
    /// The zoom the ranges are cut at. Shards own `[pivot_zoom, max_zoom]`;
    /// the coarse job owns `[0, pivot_zoom - 1]`.
    pub pivot_zoom: u8,
    /// One range per shard, ascending, contiguous, together covering the
    /// whole pivot zoom.
    pub ranges: Vec<ShardRange>,
    /// The input the plan was cut for.
    pub inputs: Vec<ShardInput>,
    /// Rows the estimator saw in total (the sum the cut was balanced on).
    pub estimated_rows_total: u64,
    /// Rows whose row group carried no usable bbox statistics and were
    /// therefore spread uniformly over the covered extent rather than placed.
    ///
    /// A large share here means the cut is close to a blind equal-width one:
    /// run the input through `gpio` so its row groups carry covering
    /// statistics, and the balance improves for free.
    pub unplaced_rows: u64,
}

impl ShardPlan {
    /// How many shards the plan cuts.
    pub fn shards(&self) -> usize {
        self.ranges.len()
    }

    /// The [`TileRange`] shard `index` owns.
    pub fn range(&self, index: usize) -> Result<TileRange, ShardError> {
        let r = self
            .ranges
            .get(index)
            .ok_or(ShardError::ShardIndexOutOfRange {
                index,
                shards: self.ranges.len(),
            })?;
        TileRange::new(self.pivot_zoom, r.lo, r.hi)
    }

    /// The [`TileRange`] a [`ShardRole`] owns, validating the role's `N`
    /// against the plan's.
    pub fn range_for(&self, role: ShardRole, path: &Path) -> Result<Option<TileRange>, ShardError> {
        match role {
            ShardRole::Coarse => Ok(None),
            ShardRole::Shard { index, shards } => {
                if shards != self.ranges.len() {
                    return Err(ShardError::ShardCountMismatch {
                        index,
                        shards,
                        plan_shards: self.ranges.len(),
                        path: path.display().to_string(),
                    });
                }
                self.range(index).map(Some)
            }
        }
    }

    /// Cut a plan for `source`, balancing on footer statistics alone.
    pub fn compute(
        source: &ConvertSource,
        pivot_zoom: u8,
        shards: usize,
    ) -> Result<Self, ShardError> {
        if pivot_zoom == 0 || pivot_zoom > MAX_SHARD_PIVOT_ZOOM {
            return Err(ShardError::PivotOutOfRange { pivot: pivot_zoom });
        }
        let tiles = zoom_tile_count(pivot_zoom);
        if shards == 0 || shards as u64 > tiles {
            return Err(ShardError::ShardCountOutOfRange {
                shards,
                pivot: pivot_zoom,
                max: tiles,
            });
        }

        let per_part = source.part_row_group_bounds()?;
        let weights = estimate_pivot_weights(&per_part, pivot_zoom);
        let ranges = cut_ranges(&weights, pivot_zoom, shards);

        let inputs = source
            .parts()
            .iter()
            .zip(source.part_row_counts()?)
            .map(|(part, (num_rows, row_groups))| ShardInput {
                path: part.display_name(),
                num_rows,
                row_groups,
            })
            .collect();

        Ok(ShardPlan {
            format: SHARD_PLAN_FORMAT.to_string(),
            version: SHARD_PLAN_VERSION,
            pivot_zoom,
            ranges,
            inputs,
            estimated_rows_total: weights.placed_total.round() as u64,
            unplaced_rows: weights.unplaced.round() as u64,
        })
    }

    /// Write the plan as JSON.
    pub fn save(&self, path: &Path) -> Result<(), ShardError> {
        let json = serde_json::to_string_pretty(self).map_err(|e| ShardError::Malformed {
            path: path.display().to_string(),
            what: format!("could not be serialized: {e}"),
        })?;
        std::fs::write(path, format!("{json}\n")).map_err(|source| ShardError::Io {
            path: path.display().to_string(),
            source,
        })
    }

    /// Read a plan, refusing anything that is not one.
    pub fn load(path: &Path) -> Result<Self, ShardError> {
        let named = || path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|source| ShardError::Io {
            path: named(),
            source,
        })?;
        let plan: ShardPlan = serde_json::from_str(&text).map_err(|e| ShardError::Malformed {
            path: named(),
            what: format!("is not a readable shard plan: {e}"),
        })?;
        if plan.format != SHARD_PLAN_FORMAT {
            return Err(ShardError::Malformed {
                path: named(),
                what: format!(
                    "is not a shard plan: its format is {:?}, expected {SHARD_PLAN_FORMAT:?}",
                    plan.format
                ),
            });
        }
        if plan.version != SHARD_PLAN_VERSION {
            return Err(ShardError::Malformed {
                path: named(),
                what: format!(
                    "is a v{} shard plan; this tylertoo reads v{SHARD_PLAN_VERSION}. Re-cut it \
                     with `tylertoo shard-plan`.",
                    plan.version
                ),
            });
        }
        plan.check_structure(path)?;
        Ok(plan)
    }

    /// Structural self-consistency, checked on load so a hand-edited plan is
    /// an error here rather than a silent hole in the tile space.
    fn check_structure(&self, path: &Path) -> Result<(), ShardError> {
        let named = || path.display().to_string();
        if self.pivot_zoom == 0 || self.pivot_zoom > MAX_SHARD_PIVOT_ZOOM {
            return Err(ShardError::PivotOutOfRange {
                pivot: self.pivot_zoom,
            });
        }
        if self.ranges.is_empty() {
            return Err(ShardError::Malformed {
                path: named(),
                what: "cuts no shards".to_string(),
            });
        }
        let base = hilbert_zoom_base(self.pivot_zoom);
        let last = base + zoom_tile_count(self.pivot_zoom) - 1;
        let mut expect = base;
        for (i, r) in self.ranges.iter().enumerate() {
            if r.lo != expect || r.hi < r.lo || r.hi > last {
                return Err(ShardError::Malformed {
                    path: named(),
                    what: format!(
                        "shard {i}'s range {}..={} does not continue the partition of z{} \
                         (expected it to start at {expect} and end no later than {last}). The \
                         ranges must tile the pivot zoom exactly, with no gap and no overlap.",
                        r.lo, r.hi, self.pivot_zoom
                    ),
                });
            }
            expect = r.hi + 1;
        }
        if expect != last + 1 {
            return Err(ShardError::Malformed {
                path: named(),
                what: format!(
                    "the ranges stop at {} but z{} runs to {last}; the last shard must reach \
                     the end of the pivot zoom or those tiles would be built by nobody.",
                    expect - 1,
                    self.pivot_zoom
                ),
            });
        }
        Ok(())
    }

    /// Check the plan was cut for the input this run is about to read.
    pub fn verify_source(&self, source: &ConvertSource, path: &Path) -> Result<(), ShardError> {
        let named = || path.display().to_string();
        let now: Vec<ShardInput> = source
            .parts()
            .iter()
            .zip(source.part_row_counts()?)
            .map(|(part, (num_rows, row_groups))| ShardInput {
                path: part.display_name(),
                num_rows,
                row_groups,
            })
            .collect();
        if now.len() != self.inputs.len() {
            return Err(ShardError::SourceMismatch {
                path: named(),
                field: "input part count".to_string(),
                saved: self.inputs.len().to_string(),
                now: now.len().to_string(),
            });
        }
        for (saved, now) in self.inputs.iter().zip(&now) {
            let mismatch = |field: &str, s: String, n: String| ShardError::SourceMismatch {
                path: named(),
                field: format!("input {:?} {field}", saved.path),
                saved: s,
                now: n,
            };
            if saved.path != now.path {
                return Err(mismatch("path", saved.path.clone(), now.path.clone()));
            }
            if saved.num_rows != now.num_rows {
                return Err(mismatch(
                    "row count",
                    saved.num_rows.to_string(),
                    now.num_rows.to_string(),
                ));
            }
            if saved.row_groups != now.row_groups {
                return Err(mismatch(
                    "row group count",
                    saved.row_groups.to_string(),
                    now.row_groups.to_string(),
                ));
            }
        }
        Ok(())
    }
}

/// Convenience: load a plan and resolve one role's range in one step.
pub fn resolve_range(
    plan_path: &Path,
    role: ShardRole,
    source: Option<&ConvertSource>,
) -> Result<(ShardPlan, Option<TileRange>), ShardError> {
    let plan = ShardPlan::load(plan_path)?;
    if let Some(source) = source {
        plan.verify_source(source, plan_path)?;
    }
    let range = plan.range_for(role, plan_path)?;
    Ok((plan, range))
}

/// Tiles at `zoom`: `4^zoom`.
fn zoom_tile_count(zoom: u8) -> u64 {
    1u64 << (2 * u32::from(zoom))
}

/// The estimator's output: per-pivot-tile row weight, plus the totals that
/// make the estimate's quality visible.
#[derive(Debug, Default)]
struct PivotWeights {
    /// `tile id -> estimated rows`. Sparse: tiles no row group covers are
    /// absent, and the cut walks the ids between them at zero cost.
    per_tile: BTreeMap<u64, f64>,
    /// Rows placed from a usable row-group bbox.
    placed_total: f64,
    /// Rows from row groups with no usable bbox (spread uniformly instead).
    unplaced: f64,
}

/// Spread every row group's rows over the pivot tiles its bbox covers.
///
/// A row group is assumed uniform over its bbox, which is the only thing the
/// footer supports. `gpio`-optimized input makes that close to true (Hilbert
/// sorting gives each row group a compact bbox); unsorted input makes it
/// worse, and the plan reports how much of the dataset could not be placed at
/// all so the difference is visible rather than silently absorbed.
fn estimate_pivot_weights(
    per_part: &[Vec<Option<crate::covering::RowGroupBounds>>],
    pivot_zoom: u8,
) -> PivotWeights {
    let mut w = PivotWeights::default();
    let mut covered: Option<TileBounds> = None;
    let mut placed_groups = 0usize;
    let mut blind_groups = 0usize;

    for rg in per_part.iter().flatten() {
        let Some(rg) = rg else {
            // No usable bbox: the group's position is unknown. `RowGroupBounds`
            // is also what carries `num_rows`, so an absent entry carries no
            // count either — held back and weighted below as one average
            // group, so it nudges the cut without inventing volume.
            blind_groups += 1;
            continue;
        };
        let bbox = TileBounds::new(rg.xmin, rg.ymin, rg.xmax, rg.ymax);
        if !bbox.is_valid() {
            blind_groups += 1;
            continue;
        }
        let tiles = pivot_tiles_for_bbox(&bbox, pivot_zoom);
        if tiles.is_empty() {
            blind_groups += 1;
            continue;
        }
        match &mut covered {
            Some(acc) => acc.expand(&bbox),
            None => covered = Some(bbox),
        }
        placed_groups += 1;
        let share = rg.num_rows as f64 / tiles.len() as f64;
        w.placed_total += rg.num_rows as f64;
        for id in tiles {
            *w.per_tile.entry(id).or_insert(0.0) += share;
        }
    }

    // Blind groups spread uniformly over the extent the rest of the file
    // covers. With no statistics anywhere the extent is the world and the cut
    // degrades to equal id width — the honest answer, and `unplaced_rows`
    // says so out loud.
    if blind_groups > 0 {
        let extent = covered.unwrap_or_else(|| TileBounds::new(-180.0, -85.0, 180.0, 85.0));
        let tiles = pivot_tiles_for_bbox(&extent, pivot_zoom);
        if !tiles.is_empty() {
            let avg = if placed_groups == 0 {
                1.0
            } else {
                w.placed_total / placed_groups as f64
            };
            let total = avg * blind_groups as f64;
            w.unplaced = total;
            let share = total / tiles.len() as f64;
            for id in tiles {
                *w.per_tile.entry(id).or_insert(0.0) += share;
            }
        }
    }
    w
}

/// The pivot-zoom tile ids a bbox covers.
fn pivot_tiles_for_bbox(bbox: &TileBounds, pivot_zoom: u8) -> Vec<u64> {
    let ranges = tile_ranges_for_bbox(bbox, pivot_zoom);
    let bands = std::iter::once(ranges.x).chain(ranges.x2);
    let mut out = Vec::new();
    for (x0, x1) in bands {
        for x in x0..=x1 {
            for y in ranges.y.0..=ranges.y.1 {
                out.push(crate::pmtiles_writer::tile_id(pivot_zoom, x, y));
            }
        }
    }
    out
}

/// Cut the pivot zoom into `shards` contiguous id runs of roughly equal
/// estimated rows.
///
/// A greedy walk in ascending id order: close the current shard as soon as its
/// accumulated weight reaches the running target `(i + 1) * total / shards`.
/// The runs are forced to stay non-empty and to cover the zoom exactly — a
/// dataset concentrated in one tile still yields `shards` ranges, most of them
/// empty of data, because an empty *range* is legal while a *gap* is not.
fn cut_ranges(weights: &PivotWeights, pivot_zoom: u8, shards: usize) -> Vec<ShardRange> {
    let base = hilbert_zoom_base(pivot_zoom);
    let tiles = zoom_tile_count(pivot_zoom);
    let last = base + tiles - 1;
    let total: f64 = weights.per_tile.values().sum();
    let n = shards as u64;

    // No signal anywhere: equal id width is the only defensible cut.
    if total <= 0.0 {
        return (0..n)
            .map(|i| ShardRange {
                lo: base + tiles * i / n,
                hi: base + tiles * (i + 1) / n - 1,
                estimated_rows: 0,
            })
            .collect();
    }

    let mut out: Vec<ShardRange> = Vec::with_capacity(shards);
    let mut iter = weights.per_tile.iter().peekable();
    let mut cursor = base;
    // Cumulative weight over every tile consumed so far, compared against a
    // running target; carrying it (rather than resetting per shard) keeps a
    // shard that overshot from pushing the error onto all the later ones.
    let mut acc = 0f64;
    let mut shard_acc = 0f64;

    for i in 0..shards {
        let remaining = (shards - i) as u64;
        if remaining == 1 {
            for (_, w) in iter.by_ref() {
                shard_acc += *w;
            }
            out.push(ShardRange {
                lo: cursor,
                hi: last,
                estimated_rows: shard_acc.round() as u64,
            });
            break;
        }
        // Leave at least one tile for every shard still to come, so a range
        // is never empty and the partition always closes.
        let latest_end = last - (remaining - 1);
        let target = total * (i + 1) as f64 / shards as f64;
        let mut end = cursor;
        while let Some((id, w)) = iter.peek().map(|&(id, w)| (*id, *w)) {
            if id > latest_end {
                break;
            }
            iter.next();
            acc += w;
            shard_acc += w;
            end = id;
            if acc >= target {
                break;
            }
        }
        let hi = end.clamp(cursor, latest_end);
        out.push(ShardRange {
            lo: cursor,
            hi,
            estimated_rows: shard_acc.round() as u64,
        });
        shard_acc = 0.0;
        cursor = hi + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(
        xmin: f64,
        ymin: f64,
        xmax: f64,
        ymax: f64,
        rows: usize,
    ) -> crate::covering::RowGroupBounds {
        crate::covering::RowGroupBounds {
            row_group_idx: 0,
            xmin,
            ymin,
            xmax,
            ymax,
            num_rows: rows,
        }
    }

    #[test]
    fn shard_role_parses_both_forms() {
        assert_eq!(ShardRole::parse("coarse").unwrap(), ShardRole::Coarse);
        assert_eq!(ShardRole::parse("COARSE").unwrap(), ShardRole::Coarse);
        assert_eq!(
            ShardRole::parse("3/16").unwrap(),
            ShardRole::Shard {
                index: 3,
                shards: 16
            }
        );
        // I must be < N.
        let err = ShardRole::parse("16/16").unwrap_err().to_string();
        assert!(err.contains("less than the shard count"), "{err}");
        assert!(ShardRole::parse("nonsense").is_err());
        assert!(ShardRole::parse("1/0").is_err());
    }

    #[test]
    fn whole_zoom_range_owns_every_id_at_every_deeper_zoom() {
        for pivot in 1..=6u8 {
            let r = TileRange::whole_zoom(pivot).unwrap();
            for z in pivot..=10u8 {
                let ids = r.ids_at(z).expect("zoom at or past the pivot is owned");
                assert_eq!(*ids.start(), hilbert_zoom_base(z), "z{z} start");
                assert_eq!(
                    *ids.end(),
                    hilbert_zoom_base(z) + zoom_tile_count(z) - 1,
                    "z{z} end"
                );
            }
            // Zooms below the pivot belong to nobody in shard-land.
            for z in 0..pivot {
                assert!(r.ids_at(z).is_none(), "z{z} must be outside a shard range");
            }
        }
    }

    /// The load-bearing disjointness property: N ranges that partition the
    /// pivot zoom partition EVERY deeper zoom's id space exactly — no
    /// overlap, no gap.
    #[test]
    fn shard_ranges_partition_every_deeper_zoom() {
        let pivot = 4u8;
        let weights = PivotWeights::default();
        for shards in [1usize, 2, 3, 5, 7, 16] {
            let ranges = cut_ranges(&weights, pivot, shards);
            assert_eq!(ranges.len(), shards);
            for z in pivot..=9u8 {
                let mut expect = hilbert_zoom_base(z);
                for r in &ranges {
                    let tr = TileRange::new(pivot, r.lo, r.hi).unwrap();
                    let ids = tr.ids_at(z).unwrap();
                    assert_eq!(*ids.start(), expect, "shards={shards} z{z} gap/overlap");
                    expect = *ids.end() + 1;
                }
                assert_eq!(
                    expect,
                    hilbert_zoom_base(z) + zoom_tile_count(z),
                    "shards={shards} z{z} must reach the end of the zoom"
                );
            }
        }
    }

    /// The balancing property, stated as a comparison rather than a constant:
    /// against a skewed input the density-balanced cut must divide the
    /// estimated ROWS far more evenly than the equal-id-width cut does.
    ///
    /// Asserting only "within Nx of even" would pass for an input whose skew
    /// happens to sit at the curve's midpoint, where equal width is already
    /// the right answer — so the equal-width control is what gives this test
    /// teeth.
    #[test]
    fn cut_balances_estimated_rows_not_id_width() {
        let pivot = 3u8;
        let base = hilbert_zoom_base(pivot);
        let tiles = zoom_tile_count(pivot);
        // Dense head, thin tail: the first 8 of z3's 64 tiles hold 800 of the
        // 856 rows.
        let mut weights = PivotWeights::default();
        for i in 0..tiles {
            weights
                .per_tile
                .insert(base + i, if i < 8 { 100.0 } else { 1.0 });
        }
        weights.placed_total = weights.per_tile.values().sum();

        let ranges = cut_ranges(&weights, pivot, 2);
        assert_eq!(ranges.len(), 2);
        let widths: Vec<u64> = ranges.iter().map(|r| r.hi - r.lo + 1).collect();
        let balanced = imbalance(&ranges.iter().map(|r| r.estimated_rows).collect::<Vec<_>>());

        // The control: what an equal-id-width cut would have weighed.
        let mid = base + tiles / 2;
        let mut equal_width = [0f64; 2];
        for (&id, &w) in &weights.per_tile {
            equal_width[usize::from(id >= mid)] += w;
        }
        let naive = imbalance(&equal_width.map(|w| w.round() as u64));

        assert!(
            naive > 1.8,
            "the control is meant to be badly unbalanced, was {naive:.2}x"
        );
        assert!(
            balanced < 1.3,
            "the balanced cut should be close to even, was {balanced:.2}x off (widths {widths:?})"
        );
        // Equal-width would have put ~96% of the rows in one shard; the
        // balanced cut gives shard 0 a narrow head instead.
        assert!(
            widths[0] < widths[1] / 4,
            "a dense head must produce a narrow first shard, got {widths:?}"
        );
    }

    /// The estimator spreads a row group's rows over exactly the pivot tiles
    /// its bbox covers, conserving the row count.
    #[test]
    fn estimator_spreads_each_row_group_over_its_covered_tiles() {
        let pivot = 2u8;
        // One row group over a bbox, one with no statistics at all.
        let w = estimate_pivot_weights(&[vec![Some(rgb(-180.0, 0.0, -90.0, 66.0, 400))]], pivot);
        let total: f64 = w.per_tile.values().sum();
        assert!(
            (total - 400.0).abs() < 1e-6,
            "rows must be conserved, got {total}"
        );
        assert_eq!(w.unplaced, 0.0);
        // The bbox is the top-left quarter of the world: z2 tiles x in 0..=0,
        // y in 0..=1 at most — never the whole zoom.
        assert!(
            w.per_tile.len() < zoom_tile_count(pivot) as usize,
            "a quarter-world bbox must not weight every tile"
        );
    }

    /// `max(parts) / mean(parts)` — 1.0 is perfectly even, larger is worse.
    fn imbalance(parts: &[u64]) -> f64 {
        let total: u64 = parts.iter().sum();
        if total == 0 {
            return 1.0;
        }
        let mean = total as f64 / parts.len() as f64;
        parts.iter().map(|&p| p as f64).fold(0.0, f64::max) / mean
    }

    #[test]
    fn cut_with_no_statistics_falls_back_to_equal_width() {
        let pivot = 3u8;
        let weights = estimate_pivot_weights(&[vec![None, None]], pivot);
        let ranges = cut_ranges(&weights, pivot, 4);
        let widths: Vec<u64> = ranges.iter().map(|r| r.hi - r.lo + 1).collect();
        assert_eq!(widths, vec![16, 16, 16, 16], "z3 has 64 tiles");
    }

    #[test]
    fn tile_range_parse_round_trips_and_rejects_mixed_zooms() {
        let lo = crate::pmtiles_writer::tile_id(4, 0, 0);
        let hi = crate::pmtiles_writer::tile_id(4, 3, 3);
        let r = TileRange::parse(&format!("{lo}..{hi}")).unwrap();
        assert_eq!(r.pivot_zoom, 4);
        assert_eq!((r.lo, r.hi), (lo, hi));

        let z5 = crate::pmtiles_writer::tile_id(5, 0, 0);
        let err = TileRange::parse(&format!("{lo}..{z5}"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("same zoom"), "{err}");
        assert!(TileRange::parse("nope").is_err());
    }

    #[test]
    fn range_bounds_cover_the_pivot_tiles_with_a_margin() {
        let pivot = 2u8;
        let base = hilbert_zoom_base(pivot);
        let r = TileRange::new(pivot, base, base).unwrap();
        let b = r.bounds();
        let (_, x, y) = crate::pmtiles_writer::tile_id_to_zxy(base).unwrap();
        let tb = TileCoord::new(x, y, pivot).bounds();
        assert!(b.lng_min <= tb.lng_min && b.lng_max >= tb.lng_max);
        assert!(b.lat_min <= tb.lat_min && b.lat_max >= tb.lat_max);
    }

    #[test]
    fn plan_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shards.json");
        let plan = ShardPlan {
            format: SHARD_PLAN_FORMAT.to_string(),
            version: SHARD_PLAN_VERSION,
            pivot_zoom: 2,
            ranges: cut_ranges(&PivotWeights::default(), 2, 2),
            inputs: vec![ShardInput {
                path: "in.parquet".to_string(),
                num_rows: 10,
                row_groups: 1,
            }],
            estimated_rows_total: 0,
            unplaced_rows: 0,
        };
        plan.save(&path).unwrap();
        let back = ShardPlan::load(&path).unwrap();
        assert_eq!(plan, back);
        assert_eq!(back.shards(), 2);
        assert!(back.range_for(ShardRole::Coarse, &path).unwrap().is_none());
        assert!(back
            .range_for(
                ShardRole::Shard {
                    index: 0,
                    shards: 2
                },
                &path
            )
            .unwrap()
            .is_some());
        // N must match the plan's N.
        let err = back
            .range_for(
                ShardRole::Shard {
                    index: 0,
                    shards: 4,
                },
                &path,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("cuts 2 shards"), "{err}");
    }

    #[test]
    fn a_plan_with_a_hole_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("holey.json");
        let mut plan = ShardPlan {
            format: SHARD_PLAN_FORMAT.to_string(),
            version: SHARD_PLAN_VERSION,
            pivot_zoom: 2,
            ranges: cut_ranges(&PivotWeights::default(), 2, 2),
            inputs: vec![],
            estimated_rows_total: 0,
            unplaced_rows: 0,
        };
        // Punch a one-tile hole between the two shards.
        plan.ranges[0].hi -= 1;
        plan.save(&path).unwrap();
        let err = ShardPlan::load(&path).unwrap_err().to_string();
        assert!(err.contains("no gap and no overlap"), "{err}");
    }

    #[test]
    fn a_json_file_that_is_not_a_shard_plan_is_named_as_such() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("other.json");
        std::fs::write(&path, r#"{"format":"something-else","version":1}"#).unwrap();
        let err = ShardPlan::load(&path).unwrap_err().to_string();
        assert!(err.contains("shard-plan"), "{err}");
    }
}
