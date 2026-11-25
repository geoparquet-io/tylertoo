//! `--valve-scope smooth|zoom` (#768): each tile's valve need, predicted
//! from the export scan, and the floors it puts on the tiles around it.
//! The need is the one the tile's own valve reads, decided per tile as the
//! encoder decides it: a point tile (every member a point) always takes the
//! drop valve, so its need is the share of its members to keep
//! ([`predict_drop_need`]) under either `--size-valve`; under `merge`, any
//! other tile's need is a merge threshold over the gaps of its lines and
//! polygons ([`predict_need`]). The two are separate floor maps
//! ([`ValveFloors`]): a point tile reads only drop floors, a tile it merges
//! only merge floors.
//!
//! The export scan ([`super::scan_all_levels`]) already visits every row's
//! bbox and every tile it lands in. With `smooth` it also follows each
//! tile's own members in file order ([`Scan`]): a member count and whether
//! every member is a point, and once a tile holds 1,023 members the band
//! holding most of them from there on and, under `merge`, from its 1,024th
//! member on, a half-octave histogram of each line or polygon member's gap
//! to the tile's previous member in its band (the merge valve's gap measure,
//! [`super::coalesce_valve::gap_to_rect`]). A tile below that holds only its
//! count, as the scan's tile counts do; the rest is checked against the scan's memory budget
//! after every batch, and dropped with a warning when it is over
//! (`--valve-scope tile` from there on).
//!
//! After the scan each tile's need is predicted ([`Scan::predict`]):
//!
//! 1. Size: `count × (raw, gzipped) bytes a member` against the caps in
//!    force, the estimated fit `f`. The bytes a member come from a real
//!    encode of a sample of the tile's own members
//!    ([`super::valve_sample_bytes`]): the footer cannot price attributes
//!    the way MVT does (a value table deduplicated per tile, tags that gzip
//!    to nothing when a column is constant, nested columns written as JSON),
//!    and gzip ratios run 0.2 (Madagascar adm4, 53 columns) to 0.6. Every
//!    point tile and every tile with a histogram is priced by the 256 members the scan kept from
//!    its 1,024th on; those that puts near a cap, by its first 4,096 from
//!    its 1,023rd on in the band holding most of them. Only tiles estimated
//!    well over (`f < PREDICT_BELOW_FIT`)
//!    get a need: a false positive puts floors on its neighbours, a missed
//!    near-cap tile only skips a small step. A tile whose sample prices
//!    nothing (most of its rows clip empty) is priced at its level's mean.
//! 2. Need: a point tile keeps `DROP_KEEP × f` of its members. The merge
//!    valve's threshold is a gap quantile. It keeps about
//!    `0.8 × f` of the features on its second encode, a little less when it
//!    takes more (`KEEP_EXP`). The need is the histogram's `1 − keep`
//!    quantile.
//!
//! The floors ([`floors`]) are each tile's own need and its neighbours'
//! needs, faded by `1 / --valve-max-step` a ring, one rule for both valves.
//! Every tile within reach starts from its floor before its first encode,
//! a tile that would fit on its own too: `merge_to_fit` starts its ladder
//! at it, and a point tile first keeps the share of its members its drop
//! floor sets, by rank (`kept_at_floor`). The size ladder after the floor
//! stays the backstop that holds the cap.
//!
//! Sharding: everything a tile's need depends on comes from its own
//! members, in file order, never from row positions or other tiles, and a
//! shard holds every row of the tiles in its range and of their neighbours
//! out to [`MAX_RINGS`]. So a shard computes the floors the full build
//! does for every tile it owns, whatever row groups it pruned. The scan
//! hands the predictor row-aligned bboxes from either bbox source (a null
//! row is `None`), so a tile's band rows are its rows in the file.

use std::collections::{HashMap, HashSet};
use std::ops::{Range, RangeInclusive};

use geo::Rect;
use rayon::prelude::*;

use super::coalesce_valve::{gap_to_world_box, world_box, world_xy, GAP_NONE};
use super::{
    for_each_member_key, key_to_xy, member_tiles_bound, ExportOptions, SizeValve, ValveScope,
};
use crate::tile::TileBounds;

/// Half-octave gap buckets, relative to the tile's pixel: bucket 8 starts at
/// one squared pixel (4096 per tile side), the last at 2^28 px².
pub(crate) const BUCKETS: usize = 64;
const BUCKET_OFFSET: f64 = 8.0;

/// A tile keeps a gap histogram once it holds this many members: bounds the
/// scan's memory at one histogram per this many memberships. A tile over a
/// cap holds tens of thousands; the few gaps it misses before crossing do
/// not move a quantile.
const HIST_MIN_MEMBERS: u64 = 1024;

/// Members in a tile's first price sample.
pub(crate) const PRICE_MEMBERS: usize = 256;

/// Predict a need only for tiles estimated below this fit.
const PREDICT_BELOW_FIT: f64 = 0.9;

/// Sample a tile's own bytes a member when its level's put it below this.
const CANDIDATE_FIT: f64 = 2.0;

/// Under drop, a point tile over its cap keeps `DROP_KEEP × fit` of its
/// members: a margin on the estimated fit, so most floored tiles fit on
/// their first encode.
const DROP_KEEP: f64 = 0.9;

/// A drop floor is `KEEP_ALL / share`, the share of a point tile's members
/// it keeps: a floor at or under this keeps them all, twice it keeps half.
/// Halving a floor one ring out (`--valve-max-step 2`) doubles the share.
pub(crate) const KEEP_ALL: u64 = 1 << 20;

/// `keep = 0.8 × fit^KEEP_EXP`: with the true first-encode fit, 1.0 under-
/// predicts NZ by 1.16× and Moldova by 1.5×; 1.2 is within 1.2× of both.
const KEEP_EXP: f64 = 1.2;

type Hist = Box<[u32; BUCKETS]>;

/// Raw and gzipped MVT bytes of one member.
pub(crate) type MemberBytes = (f64, f64);

/// The scan state of a tile from its member [`DENSE_FROM`] on; before that a
/// tile holds only its member count ([`Part::counts`]), so the predictor
/// costs a sparse tile what the scan's tile count does. Everything in it
/// depends only on the tile's own members, in file order, so a shard that
/// holds all of a tile's rows (any tile in its range) computes what a
/// monolithic build does, whatever rows around them it pruned.
struct Dense {
    /// Under merge, from the member that reaches [`HIST_MIN_MEMBERS`] on:
    /// each line or polygon member's gap to the tile's previous member in
    /// its band. `None` until the first such gap: a point tile has none.
    hist: Option<Hist>,
    /// The previous member's band and bbox centre (world units).
    prev: Option<(u16, (f64, f64))>,
    /// The band being counted: `(band, members, first and last member's
    /// band rows)`.
    run: Run,
    /// The band with the most members so far, lowest on a tie, as `run`.
    best: Run,
    /// The band and band row of the first [`PRICE_MEMBERS`] members from
    /// the one that reaches [`HIST_MIN_MEMBERS`], packed by [`pack_row`]:
    /// the rows it is priced by.
    price_rows: Vec<u64>,
}

/// A tile turns [`Dense`] at this member: the one before the histogram's
/// first, whose centre the first gap is measured from.
const DENSE_FROM: u64 = HIST_MIN_MEMBERS - 1;

/// `(band, band row)` in one word: a band row is below 2^48.
fn pack_row(band: u16, row: u64) -> u64 {
    (u64::from(band) << 48) | (row & ((1 << 48) - 1))
}

fn unpack_row(packed: u64) -> (u16, u64) {
    ((packed >> 48) as u16, packed & ((1 << 48) - 1))
}

impl Default for Dense {
    fn default() -> Self {
        Self {
            hist: None,
            prev: None,
            run: Run::default(),
            best: Run::default(),
            price_rows: Vec::with_capacity(PRICE_MEMBERS),
        }
    }
}

/// One of a level's [`PARTS`] maps: every tile's member count, with
/// [`NON_POINT`] set once it holds a member that is not a point, and the
/// state of those past [`DENSE_FROM`].
#[derive(Default)]
struct Part {
    counts: HashMap<u64, u64>,
    dense: HashMap<u64, Dense>,
    /// Histograms its dense tiles hold.
    hists: usize,
}

type Run = (u16, u32, u64, u64);

/// The bit of a [`Part::counts`] entry set once the tile holds a member that
/// is not a point: the encoder sends a tile of points alone to the drop
/// valve, under either `--size-valve`. A count is far below 2^63.
const NON_POINT: u64 = 1 << 63;

impl Dense {
    /// The band its members are sampled in: the one holding the most of
    /// its members from [`DENSE_FROM`] on.
    fn densest_run(&self) -> Run {
        if self.run.1 > self.best.1 {
            self.run
        } else {
            self.best
        }
    }
}

/// A tile to price with a sample encode: its level, key, and the band and
/// band rows of its first and last member in the band holding most of its
/// members.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SampleAt {
    pub(crate) level: usize,
    pub(crate) key: u64,
    pub(crate) band: u16,
    pub(crate) row: u64,
    pub(crate) last: u64,
}

/// The predictor's share of the scan: per level, per tile key, the keys
/// split into [`PARTS`] maps so a level's tiles update in parallel.
pub(crate) struct Scan {
    levels: Vec<[Part; PARTS]>,
    /// The dense state outgrew the scan's budget and was dropped
    /// ([`Self::shed`]): only the counts go on, and nothing is predicted.
    shed: bool,
    /// What the scan's budget leaves the predictor ([`Self::set_room`]),
    /// and whether a flush outgrew it: it then stops adding dense state,
    /// and the scan sheds it ([`Self::outgrew`]).
    room: u64,
    over: bool,
    /// The band being read, and its rows already sized.
    band: Option<usize>,
    band_rows: u64,
    /// Rows waiting for [`Self::flush`], and the levels they fan to.
    pending: Vec<Option<TileBounds>>,
    pending_levels: RangeInclusive<usize>,
    /// Time spent sizing rows ([`Self::flush`]): the predictor's share of
    /// the scan, for the log.
    pub(crate) spent: std::time::Duration,
}

/// A row as [`Scan::flush`] sizes it: its bbox, its corners ([`world_box`])
/// and centre in world units, whether it is a point, and its band row.
/// PERF: projected once a row, not once a row and level (France points:
/// four Web Mercator projections per membership were most of the scan's
/// predictor time).
type SizedRow = (TileBounds, [f64; 4], (f64, f64), bool, u64);

/// Rows one [`Scan::flush`] sizes: what one pass over the levels holds.
/// PERF: large, so each flush's fork-join over a level's maps has work
/// enough for the pool (France points: 64Ki rows a flush kept the
/// predictor near three cores).
pub(crate) const FLUSH_ROWS: usize = 1 << 18;

/// Maps a level's tiles are split into, by key.
const PARTS: usize = 64;

/// The map of [`Scan::levels`] that holds `key`.
fn part(key: u64) -> usize {
    (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 58) as usize
}

/// Rows a flush fans out per task.
const FAN_ROWS: usize = 4096;

/// One run of rows' members, per map of a level: `(key, first, last, any
/// row not a point)` for each stretch of consecutive rows in one tile
/// (Hilbert-sorted rows stay in a tile for long stretches, so a map lookup
/// serves many members).
type Fanned = [Vec<(u64, u32, u32, bool)>; PARTS];

/// Most members one window of [`Scan::flush`] fans out at a level, before
/// they are folded into the level's maps: 24 bytes each, so about 12 MiB a
/// level. Without it a flush held every row's members at once, and a few
/// hundred world-spanning rows are millions of members at z6.
const FAN_MEMBERS: u64 = 2 * FLUSH_ROWS as u64;

/// `rows` cut into consecutive windows of at most [`FAN_MEMBERS`] members
/// at `zoom` ([`member_tiles_bound`]), each at least one row.
fn fan_windows(rows: &[SizedRow], zoom: u8, opts: &ExportOptions) -> Vec<Range<usize>> {
    let tiles: Vec<u64> = rows
        .par_iter()
        .with_min_len(2048)
        .map(|r| member_tiles_bound(&r.0, zoom, opts))
        .collect();
    let (mut out, mut start, mut held) = (Vec::new(), 0, 0);
    for (i, t) in tiles.into_iter().enumerate() {
        if i > start && held + t > FAN_MEMBERS {
            out.push(start..i);
            (start, held) = (i, 0);
        }
        held += t;
    }
    out.push(start..rows.len());
    out
}

/// Resident bytes a dense tile's price rows and a gap histogram hold.
const PRICE_ROWS_BYTES: u64 = (PRICE_MEMBERS * std::mem::size_of::<u64>()) as u64;
const HIST_BYTES: u64 = std::mem::size_of::<[u32; BUCKETS]>() as u64;

/// log2 of one squared pixel at `zoom`, in squared world units.
fn px2_log2(zoom: u8) -> f64 {
    2.0 * (32.0 - f64::from(zoom) - 12.0)
}
/// A tile, `(zoom, x, y)`.
pub(crate) type Key = (u8, u32, u32);

/// Each tile's predicted need, by the valve that reads it.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Needs {
    /// Merge thresholds of the tiles the merge valve fits (`--size-valve
    /// merge`, a tile not all points).
    pub(crate) merge: HashMap<Key, u64>,
    /// Drop floors of point tiles, under either valve.
    pub(crate) drop: HashMap<Key, u64>,
    /// Tiles that could be over a cap but that neither sample priced: most
    /// of their rows reach them by bbox but clip empty there.
    pub(crate) unpriced: usize,
}

impl Needs {
    pub(crate) fn len(&self) -> usize {
        self.merge.len() + self.drop.len()
    }
}

/// One valve's starting thresholds: each tile's own, keyed `(zoom, x, y)`,
/// or under `--valve-scope zoom` its zoom's. A tile starts from the larger.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Floors {
    pub(crate) tiles: HashMap<Key, u64>,
    pub(crate) zooms: HashMap<u8, u64>,
    /// The tiles predicted over their cap on their own, whose needs the
    /// floors come from. A tile thinned only by a neighbour's floor is not
    /// an oversized tile.
    pub(crate) own: HashSet<Key>,
}

impl Floors {
    /// The floor of tile `(zoom, x, y)`; 0 for none.
    pub(crate) fn get(&self, (zoom, x, y): Key) -> u64 {
        let own = self.tiles.get(&(zoom, x, y)).copied().unwrap_or(0);
        own.max(self.zooms.get(&zoom).copied().unwrap_or(0))
    }

    fn is_empty(&self) -> bool {
        self.tiles.is_empty() && self.zooms.values().all(|&f| f == 0)
    }

    fn len(&self) -> usize {
        self.tiles.len() + self.zooms.len()
    }

    fn resident_bytes(&self) -> u64 {
        (self.tiles.capacity() + self.zooms.capacity() + self.own.capacity()) as u64
            * FLOOR_ENTRY_BYTES
    }
}

/// Every tile's starting valve thresholds, one map for each valve a tile can
/// take: the encoder sends a tile of points to the drop valve under either
/// `--size-valve`, so a point tile reads a kept share ([`Self::drop`]); a
/// tile the merge valve fits reads a merge gap ([`Self::merge`]).
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ValveFloors {
    pub(crate) merge: Floors,
    pub(crate) drop: Floors,
}

/// One tile's floors ([`ValveFloors::for_tile`]).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct TileFloor {
    /// Its merge floor, read when the merge valve fits it.
    pub(crate) merge: u64,
    /// Its drop floor, read when it is a point tile.
    pub(crate) drop: u64,
    /// It was predicted over its cap on its own ([`Floors::own`]).
    pub(crate) own: bool,
}

impl ValveFloors {
    /// Tile `key`'s floors; 0 for none.
    pub(crate) fn for_tile(&self, key: Key) -> TileFloor {
        TileFloor {
            merge: self.merge.get(key),
            drop: self.drop.get(key),
            own: self.merge.own.contains(&key) || self.drop.own.contains(&key),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.merge.is_empty() && self.drop.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.merge.len() + self.drop.len()
    }

    /// Bytes the floors hold through the export ([`FLOOR_ENTRY_BYTES`] a
    /// tile floor).
    pub(crate) fn resident_bytes(&self) -> u64 {
        self.merge.resident_bytes() + self.drop.resident_bytes()
    }
}

/// The most Chebyshev rings a tile's need reaches (#768). A floor fades by
/// `1 / --valve-max-step` a ring and stops where it no longer constrains a
/// tile ([`no_op_floor`]); at the default step of 2 a drop floor is back to
/// keep-all within 8 rings for any tile keeping 1/256 of its points or
/// more, the reach the prototype measured (step p95 about ln 2 at every
/// zoom on France and planet points).
///
/// A shard holds every row within `shard::SHARD_READ_MARGIN_TILES` pivot
/// tiles of its range, which must cover this many rings plus a tile buffer
/// (at most one tile) at the pivot zoom, the widest tiles a shard owns.
pub(crate) const MAX_RINGS: i64 = 8;
const _: () = assert!(
    (MAX_RINGS + 1) as f64 <= crate::shard::SHARD_READ_MARGIN_TILES,
    "a shard must read every row of the tiles its valve floors reach"
);

/// Resident bytes of one floor in [`Floors::tiles`], with the map's slack: a
/// 24-byte entry, its control byte, at most half the table empty.
pub(crate) const FLOOR_ENTRY_BYTES: u64 = 52;

/// The floor at or under which a valve does nothing at `zoom`: under drop
/// [`KEEP_ALL`] (it keeps every point); under merge one squared tile grid
/// unit (4096 to a tile), below which only features that quantize onto one
/// grid point would merge.
pub(crate) fn no_op_floor(valve: SizeValve, zoom: u8) -> u64 {
    match valve {
        SizeValve::Drop => KEEP_ALL,
        SizeValve::Merge => px2_log2(zoom).exp2() as u64,
    }
}

/// The floors `need` puts on the tiles, one fade rule for both valves: each
/// tile's own need, and each need times `factor` a Chebyshev ring at the
/// same zoom, the largest where they overlap. A need reaches out while its
/// faded value still constrains a tile (above `stop` at its zoom), at most
/// [`MAX_RINGS`]: a need too large to fade back to `stop` by then at
/// `factor` fades at the even, steeper factor that does, so its reach ends
/// without a cliff. Columns wrap across the antimeridian, as the tiles a
/// feature draws in do; rows stop at the poles. `None` once the map would
/// hold more than `max_floors`: it does not fit the export's memory.
pub(crate) fn floors(
    need: &HashMap<Key, u64>,
    factor: f64,
    stop: impl Fn(u8) -> u64,
    max_floors: usize,
) -> Option<HashMap<Key, u64>> {
    let mut out: HashMap<Key, u64> = HashMap::new();
    for (&(z, x, y), &m) in need {
        let n = 1i64 << z;
        let stop = stop(z);
        let reach = (stop.max(1) as f64 / m as f64).powf(1.0 / (MAX_RINGS + 1) as f64);
        let factor = factor.min(reach);
        for d in 0..=MAX_RINGS {
            let v = (m as f64 * factor.powi(d as i32)) as u64;
            if v <= stop {
                break;
            }
            // The ring's tiles: its top and bottom rows, then its sides.
            let ring = (-d..=d)
                .flat_map(|dx| [(dx, -d), (dx, d)])
                .chain((1 - d..d).flat_map(|dy| [(-d, dy), (d, dy)]));
            for (dx, dy) in ring {
                let (tx, ty) = ((i64::from(x) + dx).rem_euclid(n), i64::from(y) + dy);
                if ty < 0 || ty >= n {
                    continue;
                }
                let e = out.entry((z, tx as u32, ty as u32)).or_insert(0);
                *e = (*e).max(v);
            }
            if out.len() > max_floors {
                return None;
            }
        }
    }
    Some(out)
}

/// The histogram bucket of `gap` at `zoom`.
pub(crate) fn bucket(gap: u64, zoom: u8) -> usize {
    let b = 2.0 * ((gap.max(1) as f64).log2() - px2_log2(zoom)) + BUCKET_OFFSET;
    b.floor().clamp(0.0, (BUCKETS - 1) as f64) as usize
}

/// The gap at quantile `q` of `hist`, interpolated geometrically inside its
/// bucket; `None` for an empty histogram.
pub(crate) fn quantile(hist: &[u32; BUCKETS], q: f64, zoom: u8) -> Option<u64> {
    let total: u64 = hist.iter().map(|&c| u64::from(c)).sum();
    if total == 0 {
        return None;
    }
    let rank = q.clamp(0.0, 1.0) * total as f64;
    let mut below = 0.0;
    for (b, &c) in hist.iter().enumerate() {
        let c = f64::from(c);
        if c > 0.0 && below + c >= rank {
            let t = ((rank - below) / c).clamp(0.0, 1.0);
            let log2 = (b as f64 + t - BUCKET_OFFSET) / 2.0 + px2_log2(zoom);
            return Some(log2.exp2() as u64);
        }
        below += c;
    }
    None
}

impl Scan {
    pub(crate) fn new(levels: usize) -> Self {
        Self {
            levels: (0..levels)
                .map(|_| std::array::from_fn(|_| Part::default()))
                .collect(),
            shed: false,
            room: u64::MAX,
            over: false,
            band: None,
            band_rows: 0,
            pending: Vec::new(),
            pending_levels: 0..=0,
            spent: std::time::Duration::ZERO,
        }
    }

    /// Bytes the dense state holds, beyond the tile counts `tile` holds
    /// too: charged to the export's budget. O(maps): checked against the
    /// scan's budget after every batch.
    pub(crate) fn resident_bytes(&self) -> u64 {
        let dense = std::mem::size_of::<(u64, Dense)>() as u64 + 1;
        self.levels
            .iter()
            .flatten()
            .map(|p| {
                p.dense.capacity() as u64 * dense
                    + p.dense.len() as u64 * PRICE_ROWS_BYTES
                    + p.hists as u64 * HIST_BYTES
            })
            .sum()
    }

    /// The bytes the scan's budget leaves the predictor, set before each
    /// batch: a flush stops adding dense state past it.
    pub(crate) fn set_room(&mut self, room: u64) {
        self.room = room;
    }

    /// A flush outgrew [`Self::set_room`]: the scan sheds the predictor.
    pub(crate) fn outgrew(&self) -> bool {
        self.over
    }

    /// Drop every tile's dense state, keeping the counts the export scan
    /// takes from here: the predictor outgrew the scan's budget, and the
    /// export goes on as under `--valve-scope tile`.
    pub(crate) fn shed(&mut self) {
        self.shed = true;
        for p in self.levels.iter_mut().flatten() {
            p.dense = HashMap::new();
            p.hists = 0;
        }
    }

    pub(crate) fn is_shed(&self) -> bool {
        self.shed
    }

    /// Every level's tiles and their member counts: the export scan's tile
    /// counts, which it takes from here when it predicts. Each map is freed
    /// as it is copied, so the two are never both whole.
    pub(crate) fn into_tile_counts(self) -> Vec<std::collections::BTreeMap<u64, usize>> {
        self.levels
            .into_iter()
            .map(|parts| {
                let mut out = std::collections::BTreeMap::new();
                for p in parts {
                    drop(p.dense);
                    out.extend(
                        p.counts
                            .into_iter()
                            .map(|(k, c)| (k, (c & !NON_POINT) as usize)),
                    );
                }
                out
            })
            .collect()
    }

    /// Forget everything: the scan restarts from the geometry.
    /// A shed scan stays shed: the rescan reads the same rows.
    pub(crate) fn reset(&mut self) {
        let (shed, room) = (self.shed, self.room);
        *self = Self::new(self.levels.len());
        (self.shed, self.room) = (shed, room);
    }

    /// One batch of band `j`'s rows, row-aligned (`None`: no bbox), fanned
    /// to `levels`.
    pub(crate) fn batch(
        &mut self,
        j: usize,
        rows: &[Option<TileBounds>],
        levels: RangeInclusive<usize>,
        zooms: &[u8],
        opts: &ExportOptions,
    ) {
        if self.band != Some(j) {
            self.flush(zooms, opts);
            self.band = Some(j);
            self.band_rows = 0;
            self.pending_levels = levels;
        }
        let mut rows = rows;
        while !rows.is_empty() {
            let take = rows.len().min(FLUSH_ROWS - self.pending.len());
            self.pending.extend_from_slice(&rows[..take]);
            rows = &rows[take..];
            if self.pending.len() == FLUSH_ROWS {
                self.flush(zooms, opts);
            }
        }
    }

    /// Size the pending rows into every level they fan to: the levels in
    /// the pool, each level's rows in file order, so a tile's state follows
    /// its own members one by one.
    #[expect(
        clippy::disallowed_types,
        reason = "#768: one flush's dense-state charge, shared by its parallel tasks; no diagnostic globals"
    )]
    pub(crate) fn flush(&mut self, zooms: &[u8], opts: &ExportOptions) {
        if self.pending.is_empty() {
            return;
        }
        let t = std::time::Instant::now();
        let band = u16::try_from(self.band.expect("a band is open")).unwrap_or(u16::MAX);
        let first = self.band_rows;
        // Each row's bbox, its centre, and whether it is a point (a point's
        // gap never counts: points never merge). Computed once for every
        // level. Under drop only a point tile has a need, and no gap counts.
        let drop = opts.size_valve == SizeValve::Drop;
        let rows: Vec<SizedRow> = self
            .pending
            .par_iter()
            .enumerate()
            .with_min_len(2048)
            .filter_map(|(i, r)| {
                let row = first + i as u64;
                let b = (*r)?;
                let rect = Rect::new((b.lng_min, b.lat_min), (b.lng_max, b.lat_max));
                let c = rect.center();
                let point = b.lng_min == b.lng_max && b.lat_min == b.lat_max;
                Some((b, world_box(&rect), world_xy(c.x, c.y), point, row))
            })
            .collect();
        // Per level, a window of rows at a time: per run of rows, each map's
        // (key, row) members in file order; then each map follows its members
        // in that order. A tile's state depends only on its members' order,
        // not on where runs or windows break.
        let range = self.pending_levels.clone();
        let shed = self.shed || self.over;
        // Dense state a flush adds, charged as it is allocated, so it stops
        // at the budget's room rather than after the flush (superagent, #913).
        let (base, room) = (self.resident_bytes(), self.room);
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        let added = AtomicU64::new(0);
        let over = AtomicBool::new(false);
        let charge = |bytes: u64| {
            if base + added.fetch_add(bytes, Ordering::Relaxed) + bytes > room {
                over.store(true, Ordering::Relaxed);
            }
        };
        let dense_bytes = std::mem::size_of::<(u64, Dense)>() as u64 + 1 + PRICE_ROWS_BYTES;
        self.levels[range.clone()]
            .par_iter_mut()
            .zip(&zooms[range])
            .for_each(|(level, &zoom)| {
                for window in fan_windows(&rows, zoom, opts) {
                    let start = window.start;
                    let fanned: Vec<Fanned> = rows[window]
                        .par_chunks(FAN_ROWS)
                        .enumerate()
                        .map(|(c, run)| {
                            let mut out: Fanned = std::array::from_fn(|_| Vec::new());
                            for (i, (b, _, _, point, _)) in run.iter().enumerate() {
                                let i = (start + c * FAN_ROWS + i) as u32;
                                for_each_member_key(b, zoom, opts, |key| {
                                    let runs = &mut out[part(key)];
                                    match runs.last_mut() {
                                        Some(r) if r.0 == key && r.2 + 1 == i => {
                                            r.2 = i;
                                            r.3 |= !point;
                                        }
                                        _ => runs.push((key, i, i, !point)),
                                    }
                                });
                            }
                            out
                        })
                        .collect();
                    level.par_iter_mut().enumerate().for_each(|(p, part)| {
                        for &(key, first, last, lines) in fanned.iter().flat_map(|run| &run[p]) {
                            let entry = part.counts.entry(key).or_insert(0);
                            let before = *entry & !NON_POINT;
                            *entry += u64::from(last - first) + 1;
                            if lines {
                                *entry |= NON_POINT;
                            }
                            let (count, lines) = (*entry & !NON_POINT, *entry & NON_POINT != 0);
                            if shed || count < DENSE_FROM || over.load(Ordering::Relaxed) {
                                continue;
                            }
                            if drop && lines {
                                // Under drop a tile that is not all points has
                                // no need: the drop valve fits it alone.
                                part.dense.remove(&key);
                                continue;
                            }
                            // The members before the tile's `DENSE_FROM`th need
                            // only their count.
                            let skip = DENSE_FROM.saturating_sub(before + 1) as u32;
                            let t = part.dense.entry(key).or_insert_with(|| {
                                charge(dense_bytes);
                                Dense::default()
                            });
                            for (n, i) in (before + 1 + u64::from(skip)..).zip(first + skip..=last)
                            {
                                let (_, corners, centre, point, row) = &rows[i as usize];
                                if t.run.0 != band || t.run.1 == 0 {
                                    if t.run.1 > t.best.1 {
                                        t.best = t.run;
                                    }
                                    t.run = (band, 0, *row, *row);
                                }
                                t.run.1 += 1;
                                t.run.3 = *row;
                                if n >= HIST_MIN_MEMBERS && t.price_rows.len() < PRICE_MEMBERS {
                                    t.price_rows.push(pack_row(band, *row));
                                }
                                if drop {
                                    continue;
                                }
                                let gap = match t.prev {
                                    Some((b, p)) if b == band && !*point => {
                                        gap_to_world_box(p, corners)
                                    }
                                    _ => GAP_NONE,
                                };
                                t.prev = Some((band, *centre));
                                if n >= HIST_MIN_MEMBERS && gap != GAP_NONE {
                                    let hist = t.hist.get_or_insert_with(|| {
                                        charge(HIST_BYTES);
                                        part.hists += 1;
                                        Box::new([0; BUCKETS])
                                    });
                                    hist[bucket(gap, zoom)] += 1;
                                }
                            }
                        }
                    });
                }
            });
        self.over |= over.into_inner();
        self.band_rows += self.pending.len() as u64;
        self.pending.clear();
        self.spent += t.elapsed();
    }

    /// The tiles that can be predicted over (those with a gap histogram), in
    /// `(level, key)` order: each is priced by a sample of its own members,
    /// never by another tile's, so the price is the same in any shard.
    pub(crate) fn priced(&self) -> Vec<SampleAt> {
        self.select(|_, _| true)
    }

    /// The rows `at` (one of [`Self::priced`]) is first priced by, as
    /// `(band, band row)`, in file order.
    pub(crate) fn price_rows(&self, at: &SampleAt) -> impl Iterator<Item = (u16, u64)> + '_ {
        self.levels[at.level][part(at.key)]
            .dense
            .get(&at.key)
            .into_iter()
            .flat_map(|t| t.price_rows.iter().map(|&r| unpack_row(r)))
    }

    /// The tiles worth a larger sample: estimated under `CANDIDATE_FIT` at
    /// their first sample's bytes a member, or that sample priced nothing
    /// (its rows clipped empty): the larger one reads 16 times the rows.
    pub(crate) fn candidates(
        &self,
        bytes: &HashMap<(usize, u64), MemberBytes>,
        opts: &ExportOptions,
    ) -> Vec<SampleAt> {
        let caps = caps(opts);
        self.select(|s, count| {
            bytes
                .get(&(s.level, s.key))
                .is_none_or(|&b| fit(count, b, caps) < CANDIDATE_FIT)
        })
    }

    /// Tile `key`'s members at `level`.
    #[cfg(test)]
    fn count(&self, level: usize, key: u64) -> u64 {
        self.levels[level][part(key)]
            .counts
            .get(&key)
            .map_or(0, |&c| c & !NON_POINT)
    }

    /// Each dense tile that can be predicted over, its level, key, member
    /// count and whether it is a point tile: a point tile (the drop valve
    /// thins it, under either valve), or one with a gap histogram (an
    /// empty one predicts nothing, so it is not worth pricing).
    fn dense(&self) -> impl Iterator<Item = (usize, u64, &Dense, u64, bool)> + '_ {
        self.levels
            .iter()
            .enumerate()
            .flat_map(move |(level, parts)| {
                parts.iter().flat_map(move |p| {
                    p.dense.iter().filter_map(move |(&key, t)| {
                        let c = p.counts[&key];
                        let points = c & NON_POINT == 0;
                        let gaps = t.hist.as_ref().is_some_and(|h| h.iter().any(|&n| n > 0));
                        (points || gaps).then_some((level, key, t, c & !NON_POINT, points))
                    })
                })
            })
    }

    fn select(&self, keep: impl Fn(&SampleAt, u64) -> bool) -> Vec<SampleAt> {
        let mut out: Vec<SampleAt> = self
            .dense()
            .map(|(level, key, t, count, _)| (sample_at(level, key, t), count))
            .filter(|(s, count)| keep(s, *count))
            .map(|(s, _)| s)
            .collect();
        out.sort_by_key(|s| (s.level, s.key));
        out
    }

    /// Needs of the tiles predicted over, each priced at its own sampled
    /// `(raw, gzipped)` bytes a member, by the valve that fits it: a point
    /// tile's drop floor, under either valve; under merge, any other tile's
    /// merge threshold. A tile neither sample priced is counted
    /// ([`Needs::unpriced`]), not predicted.
    pub(crate) fn predict(
        &self,
        zooms: &[u8],
        bytes: &HashMap<(usize, u64), MemberBytes>,
        opts: &ExportOptions,
    ) -> Needs {
        let caps = caps(opts);
        let mut out = Needs::default();
        for (level, key, t, count, points) in self.dense() {
            let Some(&b) = bytes.get(&(level, key)) else {
                out.unpriced += 1;
                continue;
            };
            let zoom = zooms[level];
            let (map, need) = if points {
                (&mut out.drop, predict_drop_need(count, b, caps))
            } else {
                let hist = t
                    .hist
                    .as_deref()
                    .expect("dense() keeps a non-point tile by its gaps");
                (&mut out.merge, predict_need(hist, count, zoom, b, caps))
            };
            if let Some(need) = need {
                let (x, y) = key_to_xy(key, zoom);
                map.insert((zoom, x, y), need);
            }
        }
        out
    }
}

/// The floors `needs` sets under `opts.valve_scope`, each valve's from its
/// own needs: none under [`ValveScope::Tile`]; under `zoom` each zoom's
/// largest need for every tile at it; under `smooth` [`floors`], faded by
/// `1 / opts.valve_max_step` a ring. `None` when the floors would hold
/// more than `max_floors` tiles.
pub(crate) fn valve_floors(
    needs: &Needs,
    opts: &ExportOptions,
    max_floors: usize,
) -> Option<ValveFloors> {
    let of = |need: &HashMap<Key, u64>, valve: SizeValve, max_floors: usize| {
        let own = need.keys().copied().collect();
        Some(match opts.valve_scope {
            ValveScope::Tile => Floors::default(),
            ValveScope::Zoom => {
                let mut zooms: HashMap<u8, u64> = HashMap::new();
                for (&(z, _, _), &m) in need {
                    let e = zooms.entry(z).or_insert(0);
                    *e = (*e).max(m);
                }
                Floors {
                    zooms,
                    own,
                    ..Floors::default()
                }
            }
            ValveScope::Smooth => Floors {
                tiles: floors(
                    need,
                    opts.valve_max_step.recip(),
                    |z| no_op_floor(valve, z),
                    max_floors,
                )?,
                own,
                ..Floors::default()
            },
        })
    };
    let merge = of(&needs.merge, SizeValve::Merge, max_floors)?;
    let drop = of(
        &needs.drop,
        SizeValve::Drop,
        max_floors.saturating_sub(merge.tiles.len()),
    )?;
    Some(ValveFloors { merge, drop })
}

fn sample_at(level: usize, key: u64, t: &Dense) -> SampleAt {
    SampleAt {
        level,
        key,
        band: t.densest_run().0,
        row: t.densest_run().2,
        last: t.densest_run().3,
    }
}

type Caps = (Option<usize>, Option<usize>);

fn caps(opts: &ExportOptions) -> Caps {
    (
        opts.tile_size_limit.filter(|&l| l > 0),
        opts.compressed_tile_size_limit.filter(|&l| l > 0),
    )
}

/// The estimated fit of `count` members at `(raw, gzipped)` bytes each.
fn fit(count: u64, (raw_member, gz_member): MemberBytes, (raw, gz): Caps) -> f64 {
    let n = count as f64;
    let mut fit = f64::INFINITY;
    if let Some(l) = raw {
        fit = fit.min(l as f64 / (n * raw_member));
    }
    if let Some(l) = gz {
        fit = fit.min(l as f64 / (n * gz_member));
    }
    fit
}

/// One tile's predicted merge threshold, or `None` when it is predicted to
/// fit (or within the margin of fitting).
pub(crate) fn predict_need(
    hist: &[u32; BUCKETS],
    count: u64,
    zoom: u8,
    bytes: MemberBytes,
    caps: Caps,
) -> Option<u64> {
    let fit = fit(count, bytes, caps);
    if fit >= PREDICT_BELOW_FIT {
        return None;
    }
    let keep = (0.8 * fit.powf(KEEP_EXP)).min(0.8);
    // The valve's threshold is one above the quantile gap.
    quantile(hist, 1.0 - keep, zoom).map(|g| g.saturating_add(1))
}

/// #768: one point tile's predicted drop floor, under either valve:
/// `KEEP_ALL / (DROP_KEEP × fit)`, the floor that keeps `DROP_KEEP × fit` of
/// its members, or `None` when it is predicted to fit.
pub(crate) fn predict_drop_need(count: u64, bytes: MemberBytes, caps: Caps) -> Option<u64> {
    let fit = fit(count, bytes, caps);
    (fit < PREDICT_BELOW_FIT).then(|| (KEEP_ALL as f64 / (DROP_KEEP * fit)) as u64)
}

#[cfg(test)]
/// Points 1e-6..9.7e-5 degrees apart along rows 0.1 degrees wide, from
/// lon/lat 10: inside z10 tile (540, 483).
pub(crate) fn dense_points(n: u32) -> Vec<Option<TileBounds>> {
    let (mut x, mut y) = (0.0, 0.0);
    (0..n)
        .map(|i| {
            x += 1e-6 * f64::from(1 + (i * 7919) % 97);
            if x > 0.1 {
                (x, y) = (0.0, y + 1e-4);
            }
            Some(TileBounds::new(10.0 + x, 10.0 + y, 10.0 + x, 10.0 + y))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_and_quantile_round_trip() {
        let zoom = 7;
        let mut hist = [0u32; BUCKETS];
        // 100 gaps of 2^30, 100 of 2^34.
        hist[bucket(1 << 30, zoom)] += 100;
        hist[bucket(1 << 34, zoom)] += 100;
        let lo = quantile(&hist, 0.25, zoom).unwrap();
        let hi = quantile(&hist, 0.75, zoom).unwrap();
        // Within the half-octave bucket of each.
        assert!((1u64 << 30..1u64 << 31).contains(&lo), "{lo}");
        assert!((1u64 << 34..1u64 << 35).contains(&hi), "{hi}");
        assert_eq!(quantile(&[0; BUCKETS], 0.5, zoom), None);
    }

    #[test]
    fn need_only_for_tiles_well_over_and_grows_with_density() {
        let zoom = 10;
        let mut hist = [0u32; BUCKETS];
        for (i, c) in hist.iter_mut().enumerate().skip(20).take(20) {
            *c = 100 + i as u32;
        }
        let caps = (Some(2 * 1024 * 1024), None);
        // 100 bytes a member raw, 20 gzipped: 20,971 fill the raw cap.
        let bytes = (100.0, 20.0);
        assert_eq!(predict_need(&hist, 19_000, zoom, bytes, caps), None);
        let mild = predict_need(&hist, 30_000, zoom, bytes, caps).unwrap();
        let hard = predict_need(&hist, 120_000, zoom, bytes, caps).unwrap();
        assert!(hard > 2 * mild, "{mild} {hard}");
        // The compressed cap binds when it is the tighter one, at the
        // measured gzip bytes: 25,600 fill 500 KiB at 20 bytes, 12,800 at 40.
        let gz = (Some(2 * 1024 * 1024), Some(500 * 1024));
        assert_eq!(predict_need(&hist, 15_000, zoom, bytes, gz), None);
        assert!(predict_need(&hist, 15_000, zoom, (100.0, 40.0), gz).is_some());
    }

    /// Small features inside one z10 tile, spaced so their gaps vary.
    fn inside(i: u32) -> TileBounds {
        let x = 10.0 + f64::from(i % 300) * 1e-4;
        let y = 10.0 + f64::from(i / 300) * 1e-5;
        TileBounds::new(x, y, x + 1e-5, y + 1e-5)
    }

    fn far(i: u32) -> TileBounds {
        let x = -60.0 + f64::from(i % 50) * 1e-3;
        TileBounds::new(x, -30.0, x + 1e-4, -29.9999)
    }

    /// One scan of `bands`, each a band's rows, at z10.
    fn scan_of(bands: &[Vec<Option<TileBounds>>]) -> Scan {
        let (zooms, opts) = ([10u8], ExportOptions::default());
        let mut scan = Scan::new(1);
        for (j, rows) in bands.iter().enumerate() {
            // Uneven batches, as a reader makes them.
            for chunk in rows.chunks(7_000) {
                scan.batch(j, chunk, 0..=0, &zooms, &opts);
            }
        }
        scan.flush(&zooms, &opts);
        scan
    }

    fn key_of(b: TileBounds) -> u64 {
        let mut key = None;
        for_each_member_key(&b, 10, &ExportOptions::default(), |k| key = Some(k));
        key.unwrap()
    }

    #[test]
    fn a_tile_is_sampled_in_its_densest_band_and_only_near_a_cap() {
        let opts = ExportOptions::default();
        // Band 0: 500 members of the tile. Band 1: 100 rows elsewhere, then
        // 70,000 members.
        let band0: Vec<_> = (0..500).map(|i| Some(inside(i))).collect();
        let mut band1: Vec<_> = (0..100).map(|i| Some(far(i))).collect();
        band1.extend((500..70_500).map(|i| Some(inside(i))));
        let scan = scan_of(&[band0, band1]);
        let key = key_of(inside(0));
        // Only the dense tile holds a histogram; it is sampled where its
        // densest band starts.
        let priced = scan.priced();
        assert_eq!(
            priced,
            vec![SampleAt {
                level: 0,
                key,
                band: 1,
                // Band 1's member 523, the tile's 1,023rd.
                row: 622,
                last: 70_099
            }]
        );
        // The far tile, under a thousand members, holds only its count.
        let held = |f: fn(&Part) -> usize| scan.levels[0].iter().map(f).sum::<usize>();
        assert_eq!((held(|p| p.counts.len()), held(|p| p.dense.len())), (2, 1));
        // 70,500 members at 100 bytes raw: 3.4x the 2 MiB cap.
        let over = HashMap::from([((0, key), (100.0, 20.0))]);
        assert_eq!(scan.candidates(&over, &opts), priced);
        assert_eq!(scan.predict(&[10], &over, &opts).len(), 1);
        // At 20 bytes raw and 5 gzipped it is near the cap but fits; at 2 it
        // is not worth a larger sample.
        let near = HashMap::from([((0, key), (20.0, 5.0))]);
        assert_eq!(scan.candidates(&near, &opts), priced);
        assert_eq!(scan.predict(&[10], &near, &opts).len(), 0);
        let small = HashMap::from([((0, key), (2.0, 0.5))]);
        assert!(scan.candidates(&small, &opts).is_empty());
    }

    /// A data shard holds every row of the tiles in its range, in the same
    /// order, but prunes row groups elsewhere, so the band rows around them
    /// and where any fixed-size flush falls both move. A tile's state, its
    /// sample and its need must not.
    #[test]
    fn a_tiles_prediction_is_the_same_whatever_rows_a_shard_pruned() {
        let opts = ExportOptions::default();
        // Row identities: tile members are their index, far rows 1,000,000+.
        let tile = |i: u32| (i, Some(inside(i)));
        let other = |i: u32| (1_000_000 + i, Some(far(i)));
        let mut full: Vec<(u32, Option<TileBounds>)> = (0..40_000).map(other).collect();
        for i in 0..30_000 {
            full.push(tile(i));
            if i % 3 == 0 {
                full.push(other(50_000 + i));
            }
        }
        full.extend((100_000..140_000).map(other));
        // The shard pruned the first block and half the last.
        let shard: Vec<_> = full
            .iter()
            .copied()
            .filter(|&(id, _)| !(1_000_000..1_040_000).contains(&id) && id < 1_120_000)
            .collect();
        assert!(shard.len() < full.len());
        let key = key_of(inside(0));
        let at = |rows: &[(u32, Option<TileBounds>)]| {
            let scan = scan_of(&[rows.iter().map(|r| r.1).collect()]);
            let t = &scan.levels[0][part(key)].dense[&key];
            let s: Vec<_> = scan.priced().into_iter().filter(|s| s.key == key).collect();
            assert_eq!(s.len(), 1);
            let bytes = HashMap::from([((0, key), (100.0, 20.0))]);
            (
                scan.count(0, key),
                t.hist.clone().expect("the tile's gaps"),
                // The sampled rows, by identity.
                rows[s[0].row as usize].0,
                scan.price_rows(&s[0])
                    .map(|(_, r)| rows[r as usize].0)
                    .collect::<Vec<_>>(),
                scan.predict(&[10], &bytes, &opts),
            )
        };
        let (a, b) = (at(&full), at(&shard));
        assert_eq!(a.0, 30_000);
        assert!(a.1.iter().any(|&c| c > 0));
        assert_eq!(a.2, 1022, "sampled from the member the tile turns dense at");
        assert_eq!(
            a.3,
            (1023..1023 + PRICE_MEMBERS as u32).collect::<Vec<_>>(),
            "priced from the member that starts the histogram"
        );
        assert_eq!(a.4.len(), 1);
        assert_eq!(a, b);
    }

    /// #768: a point tile's need is the floor that keeps `DROP_KEEP × fit`
    /// of its members under either valve, since the encoder sends a tile of
    /// points to the drop valve under both; the same on any pool.
    #[test]
    fn a_point_tiles_need_is_a_kept_share_under_either_valve() {
        let zooms = [10u8];
        let rows = dense_points(70_000);
        let key = key_of(rows[0].unwrap());
        let bytes = HashMap::from([((0, key), (100.0, 20.0))]);
        let need = |opts: &ExportOptions, threads: usize| {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            pool.install(|| {
                let mut scan = Scan::new(1);
                scan.batch(0, &rows, 0..=0, &zooms, opts);
                scan.flush(&zooms, opts);
                (scan.priced().len(), scan.predict(&zooms, &bytes, opts))
            })
        };
        let drop = ExportOptions {
            size_valve: SizeValve::Drop,
            ..ExportOptions::default()
        };
        let (priced, got) = need(&drop, 4);
        assert_eq!(priced, 1);
        assert!(got.merge.is_empty());
        assert_eq!((priced, got.clone()), need(&drop, 1));
        assert_eq!(
            need(&ExportOptions::default(), 4),
            (priced, got.clone()),
            "the default merge valve thins a point tile by the same share"
        );
        // 70,000 members at 100 bytes: fit 0.30 of the 2 MiB cap.
        let f = fit(70_000, (100.0, 20.0), caps(&drop));
        let share = KEEP_ALL as f64 / got.drop[&(10, 540, 483)] as f64;
        assert!((share / (DROP_KEEP * f) - 1.0).abs() < 1e-6, "{share} {f}");
    }

    /// #768: a tile holding any member that is not a point is the merge
    /// valve's under merge, so its need is a merge threshold, and under drop
    /// it has none; one point among them makes it no point tile.
    #[test]
    fn a_tile_not_all_points_takes_a_merge_need_and_none_under_drop() {
        let zooms = [10u8];
        let mut rows: Vec<_> = (0..70_000).map(|i| Some(inside(i))).collect();
        rows[5] = dense_points(1)[0];
        let key = key_of(inside(0));
        let bytes = HashMap::from([((0, key), (100.0, 20.0))]);
        let predict = |opts: &ExportOptions| {
            let mut scan = Scan::new(1);
            scan.batch(0, &rows, 0..=0, &zooms, opts);
            scan.flush(&zooms, opts);
            scan.predict(&zooms, &bytes, opts)
        };
        let merge = predict(&ExportOptions::default());
        assert!(merge.drop.is_empty());
        assert_eq!(merge.merge.len(), 1);
        let drop = predict(&ExportOptions {
            size_valve: SizeValve::Drop,
            ..ExportOptions::default()
        });
        assert_eq!(drop.len(), 0, "{drop:?}");
    }

    /// #768: one fade rule: a need fades by the factor a ring and reaches
    /// out while it still constrains a tile, at most `MAX_RINGS`.
    #[test]
    fn floors_fade_by_the_factor_and_reach_while_they_constrain() {
        let need = HashMap::from([((7, 10, 10), 256u64), ((7, 30, 30), 64), ((8, 0, 0), 5)]);
        let f = floors(&need, 0.5, |_| 1, usize::MAX).unwrap();
        assert_eq!(f[&(7, 10, 10)], 256, "a tile starts at its own need");
        assert_eq!(f[&(7, 11, 10)], 128);
        assert_eq!(f[&(7, 9, 9)], 128, "a diagonal neighbour is one ring out");
        assert_eq!(f[&(7, 13, 7)], 32, "three rings out");
        // 64 fades to 2 at ring 5; ring 6 would be the stop, 1.
        assert_eq!(f[&(7, 35, 30)], 2);
        assert_eq!(f.get(&(7, 36, 30)), None);
        assert_eq!(f[&(8, 1, 0)], 2);
        assert_eq!(f[&(8, 255, 0)], 2, "a column wraps across the antimeridian");
        assert_eq!(f.get(&(8, 0, 255)), None, "rows stop at the pole");
        assert!(f.keys().all(|&(z, x, y)| x < 1 << z && y < 1 << z));
        // The reach stops where the floor no longer constrains: a need of
        // 64 above a stop of 8 reaches 2 rings at half a ring (32, 16).
        let stopped = floors(
            &HashMap::from([((9, 100, 100), 64)]),
            0.5,
            |_| 8,
            usize::MAX,
        )
        .unwrap();
        assert_eq!(stopped.len(), 25, "{stopped:?}");
        assert_eq!(stopped[&(9, 102, 98)], 16);
        // A need too large to fade back to the stop within MAX_RINGS at its
        // factor (1,000 stops at 0.8 is 168 eight rings out) fades at the even,
        // steeper factor that ends at the stop one ring past the reach: no
        // cliff at its edge.
        let r = MAX_RINGS as u32;
        let far = floors(
            &HashMap::from([((12, 100, 100), 1_000_000)]),
            0.8,
            |_| 1_000,
            usize::MAX,
        )
        .unwrap();
        assert!(far.contains_key(&(12, 100 + r, 100)));
        assert!(!far.contains_key(&(12, 101 + r, 100)));
        assert_eq!(far.len(), (2 * r as usize + 1).pow(2));
        let ring = |d: u32| far[&(12, 100 + d, 100)] as f64;
        let step = 1_000f64.powf(1.0 / f64::from(r + 1));
        for d in 1..=r {
            let got = ring(d - 1) / ring(d);
            assert!(
                (got - step).abs() < 0.05 * step,
                "ring {d}: {got} vs {step}"
            );
        }
        assert!(
            ring(r) <= 1_000.0 * step * 1.05,
            "the edge steps to the stop by {}",
            ring(r) / 1_000.0
        );
        // Floors past the memory they may hold are refused, not truncated.
        assert!(floors(&need, 0.5, |_| 0, 10).is_none());
    }

    /// #768: the scopes: `tile` sets none, `zoom` the zoom's largest need
    /// on every tile, `smooth` the fade at `1 / --valve-max-step`, stopping
    /// at each valve's own no-op floor.
    #[test]
    fn valve_scopes_set_their_floors() {
        let need = Needs {
            drop: HashMap::from([((7, 10, 10), 64 * KEEP_ALL), ((8, 0, 0), 5)]),
            ..Needs::default()
        };
        let opts = |valve_scope, size_valve, valve_max_step| ExportOptions {
            valve_scope,
            size_valve,
            valve_max_step,
            ..ExportOptions::default()
        };
        let tile = valve_floors(&need, &opts(ValveScope::Tile, SizeValve::Drop, 2.0), 0);
        assert_eq!(tile, Some(ValveFloors::default()));
        let zoom = valve_floors(&need, &opts(ValveScope::Zoom, SizeValve::Drop, 2.0), 0).unwrap();
        assert_eq!(
            zoom.drop.get((7, 99, 3)),
            64 * KEEP_ALL,
            "every tile, the zoom's max"
        );
        assert_eq!(zoom.drop.get((8, 9, 9)), 5);
        assert_eq!(zoom.drop.get((9, 0, 0)), 0);
        assert!(zoom.for_tile((7, 10, 10)).own && !zoom.for_tile((7, 99, 3)).own);
        let drop = valve_floors(
            &need,
            &opts(ValveScope::Smooth, SizeValve::Drop, 2.0),
            usize::MAX,
        )
        .unwrap();
        // 64x keep-all halves back to keep-all in 6 rings; the 5 sets nothing.
        assert_eq!(drop.drop.get((7, 15, 10)), 2 * KEEP_ALL);
        assert_eq!(drop.drop.get((7, 16, 10)), 0);
        assert_eq!(drop.drop.get((8, 0, 0)), 0);
        assert_eq!(
            drop.for_tile((7, 15, 10)).merge,
            0,
            "a drop need sets no merge floor"
        );
        let gentle = valve_floors(
            &need,
            &opts(ValveScope::Smooth, SizeValve::Drop, 1.25),
            usize::MAX,
        )
        .unwrap();
        // At 1.25 a ring, 64x would still keep 1/10 eight rings out, so it
        // fades at the even 64^(1/9) a ring that ends at keep-all.
        let even = (64 * KEEP_ALL) as f64 / 64f64.powf(1.0 / 9.0);
        assert_eq!(gentle.drop.get((7, 11, 10)), even as u64);
        assert!(gentle.drop.get((7, 18, 10)) > KEEP_ALL);
        assert_eq!(gentle.drop.get((7, 19, 10)), 0);
        assert!(gentle.drop.tiles.len() > drop.drop.tiles.len());
        // A merge need's stop is one squared grid unit at the zoom.
        let unit = no_op_floor(SizeValve::Merge, 7);
        let merge = valve_floors(
            &Needs {
                merge: HashMap::from([((7, 10, 10), unit * 4)]),
                ..Needs::default()
            },
            &opts(ValveScope::Smooth, SizeValve::Merge, 2.0),
            usize::MAX,
        )
        .unwrap();
        assert_eq!(merge.merge.get((7, 11, 10)), unit * 2);
        assert_eq!(merge.merge.get((7, 12, 10)), 0);
        assert!(merge.drop.is_empty());
    }

    /// A tile's state follows its own members in file order, whether a
    /// flush fans its rows out in one window or in many (superagent, #913):
    /// rows spanning thousands of tiles each are cut into windows, and come
    /// out as if fed one row at a time.
    #[test]
    fn wide_rows_fan_out_in_windows_with_the_same_state() {
        let (zooms, opts) = ([8u8], ExportOptions::default());
        let rows: Vec<Option<TileBounds>> = (0..2_000u32)
            .map(|i| {
                let x = f64::from(i % 7);
                Some(if i % 2 == 0 {
                    TileBounds::new(-30.0 + x, -20.0, 30.0 + x, 20.0)
                } else {
                    TileBounds::new(x, 1.0, x + 1e-4, 1.0001)
                })
            })
            .collect();
        let members: u64 = rows
            .iter()
            .flatten()
            .map(|b| member_tiles_bound(b, 8, &opts))
            .sum();
        assert!(
            members > 2 * FAN_MEMBERS,
            "{members}: the flush must cut windows"
        );
        let mut whole = Scan::new(1);
        whole.batch(0, &rows, 0..=0, &zooms, &opts);
        whole.flush(&zooms, &opts);
        let mut by_row = Scan::new(1);
        for row in rows.chunks(1) {
            by_row.batch(0, row, 0..=0, &zooms, &opts);
            by_row.flush(&zooms, &opts);
        }
        assert!(!whole.priced().is_empty());
        assert_eq!(whole.priced(), by_row.priced());
        assert_eq!(whole.into_tile_counts(), by_row.into_tile_counts());
    }

    /// The bound a flush windows by never undercounts the members a row
    /// fans to: inside the world, across the antimeridian, past 180°.
    #[test]
    fn member_tiles_bound_covers_every_member() {
        let opts = ExportOptions::default();
        for b in [
            TileBounds::new(10.0, 10.0, 10.0, 10.0),
            TileBounds::new(-30.0, -20.0, 30.0, 20.0),
            TileBounds::new(170.0, -5.0, -170.0, 5.0),
            TileBounds::new(179.9, 0.0, 200.0, 1.0),
            TileBounds::new(-180.0, -85.0, 180.0, 85.0),
        ] {
            for zoom in [0u8, 3, 6, 9] {
                let mut n = 0u64;
                for_each_member_key(&b, zoom, &opts, |_| n += 1);
                let bound = member_tiles_bound(&b, zoom, &opts);
                assert!(n <= bound, "{b:?} z{zoom}: {n} members, bound {bound}");
            }
        }
    }

    /// World-covering rows make every tile dense at once: a flush stops
    /// adding dense state at the budget's room, not after (superagent,
    /// #913), and says so for the scan to shed.
    #[test]
    fn a_flush_stops_adding_dense_state_at_its_room() {
        let (zooms, opts) = ([6u8], ExportOptions::default());
        let rows = vec![Some(TileBounds::new(-179.0, -80.0, 179.0, 80.0)); 1_100];
        let held = |room: u64| {
            let mut scan = Scan::new(1);
            scan.set_room(room);
            scan.batch(0, &rows, 0..=0, &zooms, &opts);
            scan.flush(&zooms, &opts);
            (scan.outgrew(), scan.resident_bytes())
        };
        let (over, all) = held(u64::MAX);
        assert!(!over);
        let room = all / 16;
        let (over, bounded) = held(room);
        assert!(over, "{all} bytes unbounded, room {room}");
        assert!(
            bounded < all / 4,
            "{bounded} bytes held, room {room} of {all}"
        );
    }
}
