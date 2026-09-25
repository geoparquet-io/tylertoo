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
pub(crate) const SHARD_PLAN_VERSION: u32 = 1;

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
pub(crate) const MAX_SHARD_PIVOT_ZOOM: u8 = 10;

/// How far past its own tiles a shard widens the bbox it prunes input row
/// groups against, in whole **pivot** tiles.
///
/// This is the safety bound of the whole read-pruning argument, so it is worth
/// stating exactly. The export clips each feature to its tile's bounds widened
/// by `tile_width(zoom) × tile_buffer / 256`, so a feature outside a tile can
/// still render into it — and a shard that pruned away that feature's row
/// group would emit a tile missing geometry the monolithic run has. The pivot
/// zoom has the widest tiles of any zoom a shard owns, so its buffer is the
/// largest absolute margin any of them needs.
///
/// Two pivot tiles therefore covers every `--tile-buffer` up to **512** tile
/// pixels — two full tile widths, against a default of 8 and a tippecanoe
/// default of 5.
/// [`ExportError::TileBufferTooWideForShard`](crate::overview::export::ExportError::TileBufferTooWideForShard)
/// refuses anything past it rather than silently dropping tiles, so the bound
/// is enforced, not merely assumed.
///
/// The cost of the margin is bounded and small: a couple more row groups read
/// per shard, on an input whose row groups are Hilbert-compact.
pub(crate) const SHARD_READ_MARGIN_TILES: f64 = 2.0;

/// The largest `--tile-buffer` (in tile pixels) a sharded build may use, given
/// [`SHARD_READ_MARGIN_TILES`]. See that constant for the derivation.
pub(crate) const MAX_SHARD_TILE_BUFFER_PX: u32 = 512;

/// Everything that can go wrong cutting, reading or applying a shard plan.
#[derive(Debug, thiserror::Error)]
pub enum ShardError {
    /// `--pivot` outside `1..=`[`MAX_SHARD_PIVOT_ZOOM`].
    #[error(
        "--pivot {pivot} is out of range: the pivot zoom must be between 1 and \
         {MAX_SHARD_PIVOT_ZOOM}. A pivot wants to sit where the dataset has a few tiles per \
         shard, which is z4-z8 for any realistic fleet size."
    )]
    PivotOutOfRange {
        /// The rejected pivot zoom.
        pivot: u8,
    },

    /// A tile-id range whose ids imply a pivot zoom outside
    /// `1..=`[`MAX_SHARD_PIVOT_ZOOM`].
    ///
    /// Distinct from [`ShardError::PivotOutOfRange`] because no `--pivot` was
    /// typed here: the pivot zoom is *derived* from the two tile ids, so
    /// naming that flag would send the reader looking for something they
    /// never passed.
    #[error(
        "these tile ids sit at z{pivot}, which cannot be a pivot zoom: the pivot zoom must be \
         between 1 and {MAX_SHARD_PIVOT_ZOOM}. (z0 is the single world tile — a range there is \
         the whole build, not a shard of it.)"
    )]
    TileRangePivotOutOfRange {
        /// The pivot zoom the ids implied.
        pivot: u8,
    },

    /// The plan's pivot is finer than the build's finest zoom, so the shards
    /// would own no zoom at all.
    ///
    /// Inclusive: `pivot == max_zoom` is legal and gives every shard exactly
    /// one zoom to build. Only a pivot strictly past the finest zoom is an
    /// error. Raised by [`ShardPlan::check_max_zoom`].
    #[error(
        "--shard-plan {path} was cut at pivot z{pivot} but this build stops at --max-zoom \
         {max_zoom}: the shards would own no zoom at all. Re-cut the plan with a coarser \
         --pivot, or raise --max-zoom."
    )]
    PivotNotCoarser {
        /// The rejected pivot zoom.
        pivot: u8,
        /// The build's finest zoom.
        max_zoom: u8,
        /// The plan's path, for the message.
        path: String,
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
         of the N data shards, or `coarse` for the job that owns the zooms coarser than the \
         pivot and \
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

    /// A tile-id range whose low id is past its high id.
    #[error("tile-id range {lo}..{hi} is empty: LO must not exceed HI.")]
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
/// The fields are private and the only way in is [`TileRange::new`] /
/// [`TileRange::parse`], so a `TileRange` in hand is always a *validated*
/// one: a real pivot zoom, two ids that really sit at it, and `lo <= hi`.
/// Deserialization routes through the same constructor (see the manual
/// `Deserialize` below) rather than populating the fields directly, because a
/// hand-edited plan is exactly where an unvalidated range would come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct TileRange {
    /// The zoom `lo` and `hi` are tile ids at.
    pivot_zoom: u8,
    /// First pivot-zoom tile id in the run, inclusive.
    lo: u64,
    /// Last pivot-zoom tile id in the run, inclusive.
    hi: u64,
}

impl<'de> Deserialize<'de> for TileRange {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // A shadow struct with the same shape, so the wire format is
        // unchanged and every field still has to be present — but the values
        // reach `TileRange` only through the validating constructor.
        #[derive(Deserialize)]
        struct Raw {
            pivot_zoom: u8,
            lo: u64,
            hi: u64,
        }
        let raw = Raw::deserialize(d)?;
        TileRange::new(raw.pivot_zoom, raw.lo, raw.hi).map_err(serde::de::Error::custom)
    }
}

impl TileRange {
    /// The zoom `lo` and `hi` are tile ids at.
    pub fn pivot_zoom(&self) -> u8 {
        self.pivot_zoom
    }

    /// First pivot-zoom tile id in the run, inclusive.
    pub fn lo(&self) -> u64 {
        self.lo
    }

    /// Last pivot-zoom tile id in the run, inclusive.
    pub fn hi(&self) -> u64 {
        self.hi
    }

    /// Build a range from two tile ids at `pivot_zoom`.
    pub fn new(pivot_zoom: u8, lo: u64, hi: u64) -> Result<Self, ShardError> {
        if pivot_zoom == 0 || pivot_zoom > MAX_SHARD_PIVOT_ZOOM {
            return Err(ShardError::TileRangePivotOutOfRange { pivot: pivot_zoom });
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
        // `..=` is accepted as a synonym for `..` (both are inclusive here),
        // but only ONE `=`: `strip_prefix` rather than `trim_start_matches`,
        // so `5..==12` is the typo it looks like rather than silently the
        // same range.
        let hi_s = hi_s.trim();
        let hi: u64 = hi_s
            .strip_prefix('=')
            .unwrap_or(hi_s)
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
    ///
    /// Test-only: production always cuts ranges through [`ShardPlan`], and a
    /// one-shard "fleet" is not a thing anyone runs. It exists because the
    /// partition property is most cleanly stated against the whole zoom.
    #[cfg(test)]
    pub fn whole_zoom(pivot_zoom: u8) -> Result<Self, ShardError> {
        if pivot_zoom == 0 || pivot_zoom > MAX_SHARD_PIVOT_ZOOM {
            return Err(ShardError::TileRangePivotOutOfRange { pivot: pivot_zoom });
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
        if zoom < self.pivot_zoom || zoom > crate::tile::MAX_ZOOM {
            return None;
        }
        // Every step is checked rather than merely argued to be in range.
        // `4^30 < 2^61` makes the argument true for every zoom this crate can
        // write, and `zoom > MAX_ZOOM` is already out above — but `zoom` is a
        // plain `u8` reaching here from an overview file's level table, and
        // "unreachable" arithmetic that panics on a hostile input is the bug
        // class #417/#430 exist to prevent.
        let base_pivot = hilbert_zoom_base(self.pivot_zoom);
        let base_zoom = hilbert_zoom_base(zoom);
        let delta = u32::from(zoom - self.pivot_zoom);
        let span = 1u64.checked_shl(2 * delta)?;
        let h_lo = self.lo.checked_sub(base_pivot)?;
        let h_hi = self.hi.checked_sub(base_pivot)?;
        let start = base_zoom.checked_add(h_lo.checked_mul(span)?)?;
        let end = base_zoom
            .checked_add(h_hi.checked_add(1)?.checked_mul(span)?)?
            .checked_sub(1)?;
        Some(start..=end)
    }

    /// The exact geographic extent of the run's pivot tiles.
    ///
    /// This is what a shard archive advertises as its bounds (intersected with
    /// the data's own extent). Because the N ranges cover the pivot zoom
    /// completely, unioning the shards' bounds back at merge time reproduces
    /// the monolithic bbox rather than N overlapping world-sized ones.
    pub fn tile_bounds(&self) -> TileBounds {
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
        out.unwrap_or_else(|| TileBounds::new(-180.0, -90.0, 180.0, 90.0))
    }

    /// [`TileRange::tile_bounds`] widened by [`SHARD_READ_MARGIN_TILES`] pivot
    /// tiles on every side — the bbox a shard prunes its **input row groups**
    /// against.
    ///
    /// The margin is what makes the pruning safe rather than merely tight: a
    /// feature just outside a tile still renders into it through the export's
    /// edge buffer, so a row group whose bbox only grazes the run has to be
    /// read. Over-inclusion costs a read; under-inclusion costs a tile, so the
    /// margin is deliberately far wider than the buffer it covers — see
    /// [`SHARD_READ_MARGIN_TILES`] for the exact bound.
    ///
    /// Named for what it is *for*: this is the READ bbox, not the range's
    /// extent. [`TileRange::tile_bounds`] is the exact extent, and is what an
    /// archive advertises.
    pub fn read_bounds(&self) -> TileBounds {
        let b = self.tile_bounds();
        let margin = SHARD_READ_MARGIN_TILES * 360.0 / 2f64.powi(i32::from(self.pivot_zoom));
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
    /// Rows the estimator saw in total (the sum the cut was balanced on) —
    /// placed rows plus [`unplaced_rows`](Self::unplaced_rows), so a shard's
    /// share of it is a true share and the N shares sum to 100%.
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

    /// A stable 64-bit digest of **the cut itself** — the pivot zoom and the
    /// lo/hi sequence, and nothing else (#498).
    ///
    /// This is what binds a fleet to one cut. The coarse job stamps it into
    /// the convert plan's fingerprint, and every data shard has to present
    /// the same one, so "same convert plan ⇒ same cut" holds by construction
    /// rather than by the operator remembering not to re-run `shard-plan`
    /// mid-build. Two plans cut from the same input with the same `--shards`
    /// and `--pivot` agree; a re-cut with a different `--shards`, a different
    /// `--pivot`, or a since-rebalanced input does not.
    ///
    /// Deliberately NOT over the whole artifact: `estimated_rows` and the
    /// input fingerprint are advisory, and a plan re-cut to identical ranges
    /// with a different estimate is the same cut. What must match is which
    /// tiles each job owns.
    pub fn cut_digest(&self) -> u64 {
        let mut h = xxhash_rust::xxh3::Xxh3::new();
        // Domain-separated and length-prefixed, so no two different cuts can
        // serialize to the same byte string.
        h.update(b"tylertoo-shard-cut/v1");
        h.update(&[self.pivot_zoom]);
        h.update(&(self.ranges.len() as u64).to_le_bytes());
        for r in &self.ranges {
            h.update(&r.lo.to_le_bytes());
            h.update(&r.hi.to_le_bytes());
        }
        h.digest()
    }

    /// [`ShardPlan::cut_digest`] as the fixed-width hex string the convert
    /// plan's fingerprint records.
    pub fn cut_digest_hex(&self) -> String {
        format!("{:016x}", self.cut_digest())
    }

    /// Refuse a plan whose pivot is finer than the build's finest zoom.
    ///
    /// Inclusive: `pivot == max_zoom` leaves every shard exactly one zoom,
    /// which is legal (and is what a one-zoom fleet looks like). Only a pivot
    /// strictly past `max_zoom` leaves the shards nothing at all.
    ///
    /// Lives here rather than in the CLI so the Rust API gets the same guard,
    /// and so the variant that names it is constructed where it is decided.
    pub fn check_max_zoom(&self, max_zoom: u8, path: &Path) -> Result<(), ShardError> {
        if self.pivot_zoom > max_zoom {
            return Err(ShardError::PivotNotCoarser {
                pivot: self.pivot_zoom,
                max_zoom,
                path: path.display().to_string(),
            });
        }
        Ok(())
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
        // Real per-group row counts, so a group with no usable bbox is still
        // weighted by what it actually holds rather than by "one average
        // group" (#498 review): the footers are already parsed, so this is
        // free, and on an input where MOST groups lack statistics the
        // difference is the whole balance.
        let per_part_rows = source.part_row_group_row_counts()?;
        let weights = estimate_pivot_weights(&per_part, &per_part_rows, pivot_zoom);
        let ranges = cut_ranges(&weights, pivot_zoom, shards);

        let inputs = shard_inputs(source)?;

        Ok(ShardPlan {
            format: SHARD_PLAN_FORMAT.to_string(),
            version: SHARD_PLAN_VERSION,
            pivot_zoom,
            ranges,
            inputs,
            // Placed AND spread: the per-shard `estimated_rows` include
            // their share of the spread rows, so a total that excluded them
            // made the reported percentages sum past 100%.
            estimated_rows_total: (weights.placed_total + weights.unplaced).round() as u64,
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
        let now = shard_inputs(source)?;
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

/// The input identity a plan records, and the one a run presents back.
///
/// One builder for both sides on purpose: `compute` writing it one way and
/// `verify_source` reading it another is precisely how an input-binding check
/// stops binding anything.
fn shard_inputs(source: &ConvertSource) -> Result<Vec<ShardInput>, ShardError> {
    Ok(source
        .parts()
        .iter()
        .zip(source.part_row_counts()?)
        .map(|(part, (num_rows, row_groups))| ShardInput {
            path: part.display_name(),
            num_rows,
            row_groups,
        })
        .collect())
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
///
/// `per_part_rows[p][g]` is part `p`'s row group `g`'s footer row count,
/// parallel to `per_part`. It is what lets a group with no usable bbox still
/// carry its real weight into the spread.
///
/// Cost: one `BTreeMap` update per covered pivot tile, with no intermediate
/// `Vec` per group, and a group covering more than
/// [`MAX_GROUP_PIVOT_TILES`] tiles is spread flat instead of enumerated —
/// see that constant.
fn estimate_pivot_weights(
    per_part: &[Vec<Option<crate::covering::RowGroupBounds>>],
    per_part_rows: &[Vec<i64>],
    pivot_zoom: u8,
) -> PivotWeights {
    let mut w = PivotWeights::default();
    let mut covered: Option<TileBounds> = None;
    // Rows held back for the flat spread at the end: groups with no usable
    // bbox, and groups whose bbox is so broad that enumerating it would cost
    // more than the signal is worth.
    let mut spread_rows = 0f64;

    for (p, groups) in per_part.iter().enumerate() {
        for (g, rg) in groups.iter().enumerate() {
            // The footer count, which exists whether or not the bbox does.
            let footer_rows = per_part_rows
                .get(p)
                .and_then(|v| v.get(g))
                .copied()
                .unwrap_or(0)
                .max(0) as f64;
            let Some(rg) = rg else {
                spread_rows += footer_rows;
                continue;
            };
            let bbox = TileBounds::new(rg.xmin, rg.ymin, rg.xmax, rg.ymax);
            if !bbox.is_valid() {
                spread_rows += footer_rows;
                continue;
            }
            let ranges = tile_ranges_for_bbox(&bbox, pivot_zoom);
            let tiles = pivot_tile_count(&ranges);
            if tiles == 0 {
                spread_rows += footer_rows;
                continue;
            }
            let rows = rg.num_rows as f64;
            if tiles > MAX_GROUP_PIVOT_TILES {
                // Broad enough to carry no cut-point signal worth a million
                // map updates. Held back for the same flat spread the
                // statistics-less groups get, and counted as unplaced so the
                // plan says out loud that it happened.
                match &mut covered {
                    Some(acc) => acc.expand(&bbox),
                    None => covered = Some(bbox),
                }
                spread_rows += rows;
                continue;
            }
            match &mut covered {
                Some(acc) => acc.expand(&bbox),
                None => covered = Some(bbox),
            }
            w.placed_total += rows;
            let share = rows / tiles as f64;
            for_each_pivot_tile(&ranges, pivot_zoom, |id| {
                *w.per_tile.entry(id).or_insert(0.0) += share;
            });
        }
    }

    // Held-back groups spread uniformly over the extent the rest of the file
    // covers. With no statistics anywhere the extent is the world and the cut
    // degrades to equal id width — the honest answer, and `unplaced_rows`
    // says so out loud. Enumerated ONCE, however many groups fed it.
    if spread_rows > 0.0 {
        let extent = covered.unwrap_or_else(|| TileBounds::new(-180.0, -85.0, 180.0, 85.0));
        let ranges = tile_ranges_for_bbox(&extent, pivot_zoom);
        let tiles = pivot_tile_count(&ranges);
        if tiles > 0 {
            w.unplaced = spread_rows;
            let share = spread_rows / tiles as f64;
            for_each_pivot_tile(&ranges, pivot_zoom, |id| {
                *w.per_tile.entry(id).or_insert(0.0) += share;
            });
        }
    }
    w
}

/// Past this many pivot tiles, a single row group's bbox is spread flat
/// rather than enumerated tile by tile.
///
/// Two reasons, and the second is the real one. The cheap one: the pivot zoom
/// holds up to `4^`[`MAX_SHARD_PIVOT_ZOOM`] = 1,048,576 tiles, and a
/// planet-scale input has hundreds of thousands of row groups — a world-bbox
/// group enumerated per tile would be `10^11` map updates for an estimate
/// nobody's correctness depends on. The real one: a group spanning more than
/// 4,096 pivot tiles has already lost every cut point it could have named. A
/// fleet is tens of shards, not thousands, so such a group is diffuse at the
/// scale the cut works at, and flat is what it actually looks like.
const MAX_GROUP_PIVOT_TILES: u64 = 4096;

/// How many pivot-zoom tiles a [`BboxTileRanges`] covers — arithmetic, with
/// no enumeration, so the threshold above can be tested before paying for it.
fn pivot_tile_count(ranges: &crate::tile::BboxTileRanges) -> u64 {
    let height = u64::from(ranges.y.1.saturating_sub(ranges.y.0)) + 1;
    let width: u64 = std::iter::once(ranges.x)
        .chain(ranges.x2)
        .map(|(x0, x1)| u64::from(x1.saturating_sub(x0)) + 1)
        .sum();
    width.saturating_mul(height)
}

/// Call `f` with every pivot-zoom tile id a [`BboxTileRanges`] covers.
///
/// A closure rather than a returned `Vec`: this runs once per row group on an
/// input that may have hundreds of thousands of them, and the intermediate
/// allocation was pure overhead.
fn for_each_pivot_tile(
    ranges: &crate::tile::BboxTileRanges,
    pivot_zoom: u8,
    mut f: impl FnMut(u64),
) {
    for (x0, x1) in std::iter::once(ranges.x).chain(ranges.x2) {
        for x in x0..=x1 {
            for y in ranges.y.0..=ranges.y.1 {
                f(crate::pmtiles_writer::tile_id(pivot_zoom, x, y));
            }
        }
    }
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
        let w = estimate_pivot_weights(
            &[vec![Some(rgb(-180.0, 0.0, -90.0, 66.0, 400))]],
            &[vec![400]],
            pivot,
        );
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
        let weights = estimate_pivot_weights(&[vec![None, None]], &[vec![7, 11]], pivot);
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
        let b = r.read_bounds();
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

    /// The cut digest names the CUT and nothing else: the same pivot and the
    /// same ranges agree however the plan got there, a different cut does
    /// not, and the advisory estimates do not enter into it.
    #[test]
    fn cut_digest_covers_the_cut_and_only_the_cut() {
        let plan = |pivot: u8, shards: usize| ShardPlan {
            format: SHARD_PLAN_FORMAT.to_string(),
            version: SHARD_PLAN_VERSION,
            pivot_zoom: pivot,
            ranges: cut_ranges(&PivotWeights::default(), pivot, shards),
            inputs: vec![],
            estimated_rows_total: 0,
            unplaced_rows: 0,
        };
        let a = plan(3, 4);
        assert_eq!(
            a.cut_digest(),
            plan(3, 4).cut_digest(),
            "same cut, same digest"
        );
        assert_ne!(
            a.cut_digest(),
            plan(3, 2).cut_digest(),
            "--shards must move it"
        );
        assert_ne!(
            a.cut_digest(),
            plan(4, 4).cut_digest(),
            "--pivot must move it"
        );

        // Advisory fields are NOT in it: a re-cut that lands on identical
        // ranges with a different estimate is the same cut.
        let mut same_cut_other_estimate = plan(3, 4);
        same_cut_other_estimate.estimated_rows_total = 999;
        same_cut_other_estimate.unplaced_rows = 7;
        same_cut_other_estimate.inputs = vec![ShardInput {
            path: "elsewhere.parquet".to_string(),
            num_rows: 5,
            row_groups: 1,
        }];
        assert_eq!(a.cut_digest(), same_cut_other_estimate.cut_digest());

        // Moving a boundary by one tile, keeping the partition valid, is a
        // different cut — this is the case the binding exists for.
        let mut moved = plan(3, 4);
        moved.ranges[0].hi += 1;
        moved.ranges[1].lo += 1;
        assert_ne!(
            a.cut_digest(),
            moved.cut_digest(),
            "a moved seam is a new cut"
        );

        // Hex form is fixed width, so a fingerprint string never varies in
        // shape.
        assert_eq!(a.cut_digest_hex().len(), 16);
    }

    /// The pivot-vs-finest-zoom rule is INCLUSIVE: `pivot == max_zoom` leaves
    /// every shard exactly one zoom, which is legal.
    #[test]
    fn check_max_zoom_is_inclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shards.json");
        let plan = ShardPlan {
            format: SHARD_PLAN_FORMAT.to_string(),
            version: SHARD_PLAN_VERSION,
            pivot_zoom: 4,
            ranges: cut_ranges(&PivotWeights::default(), 4, 2),
            inputs: vec![],
            estimated_rows_total: 0,
            unplaced_rows: 0,
        };
        assert!(plan.check_max_zoom(5, &path).is_ok());
        assert!(
            plan.check_max_zoom(4, &path).is_ok(),
            "one zoom is still a zoom"
        );
        let err = plan.check_max_zoom(3, &path).unwrap_err().to_string();
        assert!(
            err.contains("pivot z4") && err.contains("--max-zoom 3"),
            "{err}"
        );
    }

    /// `..=` is one `=`, not "any number of them", and a `TileRange` from a
    /// hand-edited plan is validated on the way in rather than trusted.
    #[test]
    fn tile_range_parse_and_deserialize_are_both_validating() {
        let lo = crate::pmtiles_writer::tile_id(4, 0, 0);
        let hi = crate::pmtiles_writer::tile_id(4, 3, 3);
        assert_eq!(
            TileRange::parse(&format!("{lo}..={hi}")).unwrap(),
            TileRange::new(4, lo, hi).unwrap(),
            "`..=` is an accepted synonym for `..`"
        );
        assert!(
            TileRange::parse(&format!("{lo}..=={hi}")).is_err(),
            "a second `=` is the typo it looks like"
        );

        // Deserialization routes through `new`, so a JSON range that is not a
        // range is an error, not a struct.
        let bad = format!(r#"{{"pivot_zoom":4,"lo":{hi},"hi":{lo}}}"#);
        let err = serde_json::from_str::<TileRange>(&bad)
            .unwrap_err()
            .to_string();
        assert!(err.contains("is empty"), "{err}");
        let wrong_zoom = format!(r#"{{"pivot_zoom":5,"lo":{lo},"hi":{hi}}}"#);
        assert!(serde_json::from_str::<TileRange>(&wrong_zoom).is_err());
        // z0 has no pivot, and says so without naming a flag nobody typed.
        let z0 = r#"{"pivot_zoom":0,"lo":0,"hi":0}"#;
        let err = serde_json::from_str::<TileRange>(z0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot be a pivot zoom"), "{err}");
        assert!(!err.contains("--pivot"), "no flag was typed here: {err}");
    }

    /// `ids_at` is total: no zoom, however absurd, panics it.
    #[test]
    fn ids_at_never_panics_on_an_out_of_range_zoom() {
        let r = TileRange::new(3, hilbert_zoom_base(3), hilbert_zoom_base(3) + 3).unwrap();
        for z in 0..=u8::MAX {
            let ids = r.ids_at(z);
            assert_eq!(
                ids.is_some(),
                (3..=crate::tile::MAX_ZOOM).contains(&z),
                "z{z}"
            );
        }
    }

    /// The duality the whole range algebra rests on: `hilbert_zoom_base(z)`
    /// is exactly the id `pmtiles_writer::tile_id` gives the first tile of
    /// zoom `z`, and the zoom holds `4^z` consecutive ids after it.
    ///
    /// Two modules, two derivations (a closed form here, a Hilbert walk
    /// there). If they ever drift, every `ids_at` interval silently addresses
    /// the wrong tiles — so it is pinned here rather than assumed.
    #[test]
    fn hilbert_zoom_base_is_the_writers_first_tile_id() {
        for z in 0..=10u8 {
            let base = hilbert_zoom_base(z);
            let ids: std::collections::BTreeSet<u64> = (0..1u32 << z)
                .flat_map(|x| (0..1u32 << z).map(move |y| crate::pmtiles_writer::tile_id(z, x, y)))
                .collect();
            assert_eq!(ids.len() as u64, zoom_tile_count(z), "z{z} tile count");
            assert_eq!(*ids.first().unwrap(), base, "z{z} first id");
            assert_eq!(
                *ids.last().unwrap(),
                base + zoom_tile_count(z) - 1,
                "z{z} last id"
            );
        }
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
