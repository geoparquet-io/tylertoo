//! `--valve-scope smooth|zoom` (#768): each tile's valve need, predicted
//! from the export scan, and the floors it puts on the tiles around it.
//! Under `--size-valve merge` the need is a merge threshold over the gaps of
//! lines and polygons; under `drop`, a gap threshold that thins point tiles
//! ([`predict_drop_need`], [`Scan::drop_floors`]).
//!
//! The export scan ([`super::scan_all_levels`]) already visits every row's
//! bbox and every tile it lands in. With `smooth` it also follows each
//! tile's own members in file order ([`Scan`]): a member count, and once a
//! tile holds 1,023 members the band holding most of them from there on and,
//! from its 1,024th member on, a half-octave histogram of each member's gap
//! to the tile's previous member in its band (the merge valve's gap measure,
//! [`gap_to_rect`]). A tile below that holds only its count, as the scan's
//! tile counts do; the rest is checked against the scan's memory budget
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
//!    tile with a histogram is priced by the 256 members the scan kept from
//!    its 1,024th on; those that puts near a cap, by its first 4,096 from
//!    its 1,023rd on in the band holding most of them. Only tiles estimated
//!    well over (`f < PREDICT_BELOW_FIT`)
//!    get a need: a false positive puts floors on its neighbours, a missed
//!    near-cap tile only skips a small step.
//! 2. Need: the merge valve's threshold is a gap quantile. It keeps about
//!    `0.8 × f` of the features on its second encode, a little less when it
//!    takes more (`KEEP_EXP`). The need is the histogram's `1 − keep`
//!    quantile.
//!
//! The floors ([`floors`]) are each tile's own need and its neighbours'
//! needs, halved a ring. `merge_to_fit` starts its ladder at the tile's
//! floor, so a tile predicted over usually fits on its first encode; the
//! ladder stays the backstop that holds the cap. Under `drop` a point tile
//! first drops the members whose gap to the tile's previous member is below
//! its floor (`drop_below_floor`), then thins by stride as before.
//!
//! Sharding: everything a tile's need depends on comes from its own
//! members, in file order, never from row positions or other tiles, and a
//! shard holds every row of the tiles in its range and of their neighbours
//! out to [`SMOOTH_RINGS`]. So a shard computes the floors the full build
//! does for every tile it owns, whatever row groups it pruned.
//! shortcut: a geometry-sourced scan (an overview with no bbox covering)
//! drops null rows before the predictor sees them, so a tile's sample rows
//! can be a few rows off after a null; walk row-aligned bboxes there too if
//! a covering-less overview ever needs sharded smooth floors.

use std::collections::HashMap;
use std::ops::RangeInclusive;

use geo::Rect;
use rayon::prelude::*;

use super::coalesce_valve::{gap_to_rect, gap_to_world_box, world_box, world_xy, GAP_NONE};
use super::{for_each_member_key, key_to_xy, ExportOptions, SizeValve, ValveScope};
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
/// members: the gap filter keeps the isolated points, which cost more bytes
/// than the average member the price was sampled at.
const DROP_KEEP: f64 = 0.9;

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
    /// From the member that reaches [`HIST_MIN_MEMBERS`] on: each member's
    /// gap to the tile's previous member in its band.
    hist: Hist,
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
            hist: Box::new([0; BUCKETS]),
            prev: None,
            run: Run::default(),
            best: Run::default(),
            price_rows: Vec::with_capacity(PRICE_MEMBERS),
        }
    }
}

/// One of a level's [`PARTS`] maps: every tile's member count, and the
/// state of those past [`DENSE_FROM`].
#[derive(Default)]
struct Part {
    counts: HashMap<u64, u64>,
    dense: HashMap<u64, Dense>,
}

type Run = (u16, u32, u64, u64);

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
/// and centre in world units, whether its gap counts, and its band row.
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

/// One run of rows' members, per map of a level: `(key, first, last)` for
/// each stretch of consecutive rows in one tile (Hilbert-sorted rows stay in
/// a tile for long stretches, so a map lookup serves many members).
type Fanned = [Vec<(u64, u32, u32)>; PARTS];

/// log2 of one squared pixel at `zoom`, in squared world units.
fn px2_log2(zoom: u8) -> f64 {
    2.0 * (32.0 - f64::from(zoom) - 12.0)
}
/// A tile, `(zoom, x, y)`.
pub(crate) type Key = (u8, u32, u32);

/// Each tile's starting valve threshold: its own, keyed `(zoom, x, y)`, or
/// under `--valve-scope zoom` its zoom's. A tile starts from the larger.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ValveFloors {
    pub(crate) tiles: HashMap<Key, u64>,
    pub(crate) zooms: HashMap<u8, u64>,
}

impl ValveFloors {
    /// The floor of tile `(zoom, x, y)`; 0 for none.
    pub(crate) fn get(&self, (zoom, x, y): Key) -> u64 {
        let own = self.tiles.get(&(zoom, x, y)).copied().unwrap_or(0);
        own.max(self.zooms.get(&zoom).copied().unwrap_or(0))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tiles.is_empty() && self.zooms.values().all(|&f| f == 0)
    }

    pub(crate) fn len(&self) -> usize {
        self.tiles.len() + self.zooms.len()
    }
}

/// The share of a need (a squared distance) that reaches one Chebyshev ring
/// further: 0.5, so the linear merge distance shrinks by 1/sqrt(2) a ring.
const SMOOTH_FACTOR: f64 = 0.5;

/// Rings a tile's need reaches. One: reaches of 3 to 8 changed no
/// density step at z5 to z9 on NZ parcels or Moldova fields by more than
/// 0.08, nor any drawn share or edge gap.
///
/// A shard holds every row within `shard::SHARD_READ_MARGIN_TILES` pivot
/// tiles of its range, which must cover this many rings plus a tile buffer
/// (at most one tile) at the pivot zoom, the widest tiles a shard owns.
pub(crate) const SMOOTH_RINGS: i64 = 1;
const _: () = assert!(
    (SMOOTH_RINGS + 1) as f64 <= crate::shard::SHARD_READ_MARGIN_TILES,
    "a shard must read every row of the tiles its merge floors reach"
);

/// The floors `need` puts on the tiles: the largest of each tile's own need
/// and its neighbours' needs times `SMOOTH_FACTOR` a ring, out to
/// `SMOOTH_RINGS`, at the same zoom.
pub(crate) fn floors(need: &HashMap<Key, u64>) -> HashMap<Key, u64> {
    let mut out = HashMap::new();
    for (&(z, x, y), &m) in need {
        let n = 1i64 << z;
        for dy in -SMOOTH_RINGS..=SMOOTH_RINGS {
            for dx in -SMOOTH_RINGS..=SMOOTH_RINGS {
                let (tx, ty) = (i64::from(x) + dx, i64::from(y) + dy);
                if tx < 0 || ty < 0 || tx >= n || ty >= n {
                    continue;
                }
                let d = dx.abs().max(dy.abs());
                let v = (m as f64 * SMOOTH_FACTOR.powi(d as i32)) as u64;
                let e = out.entry((z, tx as u32, ty as u32)).or_insert(0);
                *e = (*e).max(v);
            }
        }
    }
    out.retain(|_, v| *v > 0);
    out
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
            band: None,
            band_rows: 0,
            pending: Vec::new(),
            pending_levels: 0..=0,
            spent: std::time::Duration::ZERO,
        }
    }

    /// Bytes the per-tile state holds: charged to the export's budget.
    /// O(maps): checked against the scan's budget after every batch.
    pub(crate) fn resident_bytes(&self) -> u64 {
        let count = std::mem::size_of::<(u64, u64)>() as u64 + 1;
        let dense = std::mem::size_of::<(u64, Dense)>() as u64 + 1;
        let heap = std::mem::size_of::<[u32; BUCKETS]>() as u64
            + (PRICE_MEMBERS * std::mem::size_of::<u64>()) as u64;
        self.levels
            .iter()
            .flatten()
            .map(|p| {
                p.counts.capacity() as u64 * count
                    + p.dense.capacity() as u64 * dense
                    + p.dense.len() as u64 * heap
            })
            .sum()
    }

    /// Drop every tile's dense state, keeping the counts the export scan
    /// takes from here: the predictor outgrew the scan's budget, and the
    /// export goes on as under `--valve-scope tile`.
    pub(crate) fn shed(&mut self) {
        self.shed = true;
        for p in self.levels.iter_mut().flatten() {
            p.dense = HashMap::new();
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
                    out.extend(p.counts.into_iter().map(|(k, c)| (k, c as usize)));
                }
                out
            })
            .collect()
    }

    /// Forget everything: the scan restarts from the geometry.
    /// A shed scan stays shed: the rescan reads the same rows.
    pub(crate) fn reset(&mut self) {
        let shed = self.shed;
        *self = Self::new(self.levels.len());
        self.shed = shed;
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
    pub(crate) fn flush(&mut self, zooms: &[u8], opts: &ExportOptions) {
        if self.pending.is_empty() {
            return;
        }
        let t = std::time::Instant::now();
        let band = u16::try_from(self.band.expect("a band is open")).unwrap_or(u16::MAX);
        let first = self.band_rows;
        // Each row's bbox, its centre, and whether its gap counts: lines and
        // polygons under merge (points never merge), points under drop (the
        // only members a drop floor thins). Computed once for every level.
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
                Some((b, world_box(&rect), world_xy(c.x, c.y), point == drop, row))
            })
            .collect();
        // Per level, per run of rows, each map's (key, row) members in file
        // order; then each map follows its members in that order.
        let range = self.pending_levels.clone();
        let shed = self.shed;
        let fanned: Vec<Vec<Fanned>> = zooms[range.clone()]
            .par_iter()
            .map(|&zoom| {
                rows.par_chunks(FAN_ROWS)
                    .enumerate()
                    .map(|(c, run)| {
                        let mut out: Fanned = std::array::from_fn(|_| Vec::new());
                        for (i, (b, ..)) in run.iter().enumerate() {
                            let i = (c * FAN_ROWS + i) as u32;
                            for_each_member_key(b, zoom, opts, |key| {
                                let runs = &mut out[part(key)];
                                match runs.last_mut() {
                                    Some(r) if r.0 == key && r.2 + 1 == i => r.2 = i,
                                    _ => runs.push((key, i, i)),
                                }
                            });
                        }
                        out
                    })
                    .collect()
            })
            .collect();
        self.levels[range.clone()]
            .par_iter_mut()
            .zip(fanned)
            .zip(&zooms[range])
            .for_each(|((level, fanned), &zoom)| {
                level.par_iter_mut().enumerate().for_each(|(p, part)| {
                    for &(key, first, last) in fanned.iter().flat_map(|run| &run[p]) {
                        let count = part.counts.entry(key).or_insert(0);
                        let before = *count;
                        *count += u64::from(last - first) + 1;
                        if shed || *count < DENSE_FROM {
                            continue;
                        }
                        // The members before the tile's `DENSE_FROM`th need
                        // only their count.
                        let skip = DENSE_FROM.saturating_sub(before + 1) as u32;
                        let t = part.dense.entry(key).or_default();
                        for (n, i) in (before + 1 + u64::from(skip)..).zip(first + skip..=last) {
                            let (_, corners, centre, valved, row) = &rows[i as usize];
                            if t.run.0 != band || t.run.1 == 0 {
                                if t.run.1 > t.best.1 {
                                    t.best = t.run;
                                }
                                t.run = (band, 0, *row, *row);
                            }
                            t.run.1 += 1;
                            t.run.3 = *row;
                            let gap = match t.prev {
                                Some((b, p)) if b == band && *valved => {
                                    gap_to_world_box(p, corners)
                                }
                                _ => GAP_NONE,
                            };
                            t.prev = Some((band, *centre));
                            if n >= HIST_MIN_MEMBERS {
                                if t.price_rows.len() < PRICE_MEMBERS {
                                    t.price_rows.push(pack_row(band, *row));
                                }
                                if gap != GAP_NONE {
                                    t.hist[bucket(gap, zoom)] += 1;
                                }
                            }
                        }
                    }
                });
            });
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
    /// their first sample's bytes a member.
    pub(crate) fn candidates(
        &self,
        bytes: &HashMap<(usize, u64), MemberBytes>,
        opts: &ExportOptions,
    ) -> Vec<SampleAt> {
        let caps = caps(opts);
        self.select(|s, count| {
            bytes
                .get(&(s.level, s.key))
                .is_some_and(|&b| fit(count, b, caps) < CANDIDATE_FIT)
        })
    }

    /// Tile `key`'s members at `level`.
    fn count(&self, level: usize, key: u64) -> u64 {
        self.levels[level][part(key)]
            .counts
            .get(&key)
            .copied()
            .unwrap_or(0)
    }

    /// Each dense tile with a histogram, its level, key and member count.
    fn dense(&self) -> impl Iterator<Item = (usize, u64, &Dense, u64)> + '_ {
        self.levels
            .iter()
            .enumerate()
            .flat_map(move |(level, parts)| {
                parts.iter().flat_map(move |p| {
                    p.dense
                        .iter()
                        // An empty histogram predicts nothing: not worth pricing.
                        .filter(|(_, t)| t.hist.iter().any(|&c| c > 0))
                        .map(move |(&key, t)| (level, key, t, p.counts[&key]))
                })
            })
    }

    fn select(&self, keep: impl Fn(&SampleAt, u64) -> bool) -> Vec<SampleAt> {
        let mut out: Vec<SampleAt> = self
            .dense()
            .map(|(level, key, t, count)| (sample_at(level, key, t), count))
            .filter(|(s, count)| keep(s, *count))
            .map(|(s, _)| s)
            .collect();
        out.sort_by_key(|s| (s.level, s.key));
        out
    }

    /// Needs of the tiles predicted over, each priced at its own sampled
    /// `(raw, gzipped)` bytes a member; a tile without a price predicts
    /// nothing.
    pub(crate) fn predict(
        &self,
        zooms: &[u8],
        bytes: &HashMap<(usize, u64), MemberBytes>,
        opts: &ExportOptions,
    ) -> HashMap<Key, u64> {
        let caps = caps(opts);
        let need = match opts.size_valve {
            SizeValve::Merge => predict_need,
            SizeValve::Drop => predict_drop_need,
        };
        let mut out = HashMap::new();
        for (level, key, t, count) in self.dense() {
            let Some(&b) = bytes.get(&(level, key)) else {
                continue;
            };
            let zoom = zooms[level];
            if let Some(need) = need(&t.hist, count, zoom, b, caps) {
                let (x, y) = key_to_xy(key, zoom);
                out.insert((zoom, x, y), need);
            }
        }
        out
    }
}

impl Scan {
    /// The floors `need` sets under `scope` ([`ValveScope::Tile`] sets
    /// none): under `zoom` each zoom's largest need for every tile at it;
    /// under `smooth`, [`floors`] for merge and [`Self::drop_floors`] for
    /// drop.
    pub(crate) fn floors(
        &self,
        scope: ValveScope,
        need: &HashMap<Key, u64>,
        (zooms, bytes): (&[u8], &HashMap<(usize, u64), MemberBytes>),
        opts: &ExportOptions,
    ) -> ValveFloors {
        match (scope, opts.size_valve) {
            (ValveScope::Tile, _) => ValveFloors::default(),
            (ValveScope::Zoom, _) => {
                let mut zoom_max: HashMap<u8, u64> = HashMap::new();
                for (&(z, _, _), &m) in need {
                    let e = zoom_max.entry(z).or_insert(0);
                    *e = (*e).max(m);
                }
                ValveFloors {
                    tiles: HashMap::new(),
                    zooms: zoom_max,
                }
            }
            (ValveScope::Smooth, SizeValve::Merge) => ValveFloors {
                tiles: floors(need),
                zooms: HashMap::new(),
            },
            (ValveScope::Smooth, SizeValve::Drop) => ValveFloors {
                tiles: self.drop_floors(need, zooms, bytes, opts),
                zooms: HashMap::new(),
            },
        }
    }

    /// The drop valve's floors (#768): each point tile predicted over its
    /// cap starts from its own need, and passes half of it (`SMOOTH_FACTOR`)
    /// to the tiles one ring out that are themselves estimated over their
    /// cap at their own sampled bytes a member. A tile that fits gets no
    /// floor: a gap floor drops stacked and clustered points at any
    /// strength, so smoothing onto tiles that fit thinned true Africa at z5
    /// to 0.31 of `tile` (planet, 8 rings at 0.5); over-cap tiles only keep
    /// it at 1.00 and still cut the France z5 0° seam (edge-strip |ln
    /// kept-share ratio| p50/p95 0.14/0.34 against `tile`'s 1.03/1.18).
    pub(crate) fn drop_floors(
        &self,
        need: &HashMap<Key, u64>,
        zooms: &[u8],
        bytes: &HashMap<(usize, u64), MemberBytes>,
        opts: &ExportOptions,
    ) -> HashMap<Key, u64> {
        let caps = caps(opts);
        let over = |(z, x, y): Key| {
            let Some(level) = zooms.iter().position(|&lz| lz == z) else {
                return false;
            };
            let key = super::tile_key(x, y, z);
            bytes
                .get(&(level, key))
                .is_some_and(|&b| fit(self.count(level, key), b, caps) < 1.0)
        };
        let mut out = need.clone();
        for (&(z, x, y), &m) in need {
            let v = (m as f64 * SMOOTH_FACTOR) as u64;
            let n = 1i64 << z;
            for (dx, dy) in (-1..=1).flat_map(|dx| (-1..=1).map(move |dy| (dx, dy))) {
                let (tx, ty) = (i64::from(x) + dx, i64::from(y) + dy);
                if (dx, dy) == (0, 0) || tx < 0 || ty < 0 || tx >= n || ty >= n {
                    continue;
                }
                let k = (z, tx as u32, ty as u32);
                if v > 0 && over(k) {
                    let e = out.entry(k).or_insert(0);
                    *e = (*e).max(v);
                }
            }
        }
        out
    }
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

/// #768: one point tile's predicted drop threshold under `--size-valve
/// drop`: the gap above which `DROP_KEEP × fit` of its point gaps lie, or
/// `None` when it is predicted to fit. Members with a smaller gap drop.
pub(crate) fn predict_drop_need(
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
    quantile(hist, 1.0 - DROP_KEEP * fit, zoom).map(|g| g.saturating_add(1))
}

/// #768: each member's gap to the member before it in `members` (in file
/// order) when both are points of one band, else [`GAP_NONE`]: the gap the
/// predictor's histogram counts for a point tile ([`Scan::flush`]), from
/// the tile's own members only, so a shard measures what a full build does.
pub(crate) fn point_gaps<'a>(
    members: impl Iterator<Item = (u16, Option<(f64, f64)>)> + 'a,
) -> impl Iterator<Item = u64> + 'a {
    let mut prev: Option<(u16, (f64, f64))> = None;
    members.map(move |(band, point)| {
        // A member with no point (an empty geometry) has no bbox: the scan
        // never counts it, so it leaves the predecessor as is.
        let Some((x, y)) = point else {
            return GAP_NONE;
        };
        let rect = Rect::new((x, y), (x, y));
        let gap = match prev {
            Some((b, p)) if b == band => gap_to_rect(p, &rect),
            _ => GAP_NONE,
        };
        prev = Some((band, world_xy(x, y)));
        gap
    })
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
        assert!(scan.predict(&[10], &near, &opts).is_empty());
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
                t.hist.clone(),
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

    /// #768: under drop a point tile's need is the gap its kept share starts
    /// at, from the point gaps the merge valve ignores; the same on any pool.
    #[test]
    fn a_drop_need_prices_point_gaps_on_any_pool() {
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
        assert_eq!(
            need(&ExportOptions::default(), 4),
            (0, HashMap::new()),
            "points never merge, so merge prices no point tile"
        );
        let (priced, got) = need(&drop, 4);
        assert_eq!(priced, 1);
        assert_eq!((priced, got.clone()), need(&drop, 1));
        let g = got[&(10, 540, 483)];
        // 70,000 members at 100 bytes: fit 0.30 of the 2 MiB cap; the kept
        // share at the need is within a bucket of DROP_KEEP × fit.
        let f = fit(70_000, (100.0, 20.0), caps(&drop));
        let points = rows.iter().map(|r| (0, r.map(|b| (b.lng_min, b.lat_min))));
        let kept = point_gaps(points).filter(|&gap| gap >= g).count();
        let share = kept as f64 / 70_000.0;
        assert!((share / (DROP_KEEP * f) - 1.0).abs() < 0.2, "{share} {f}");
    }

    #[test]
    fn floors_fade_by_ring_and_keep_a_tiles_own_need() {
        let need = HashMap::from([((7, 10, 10), 1_000u64), ((7, 30, 30), 64), ((8, 0, 0), 5)]);
        let f = floors(&need);
        let zoom = Scan::new(0).floors(
            ValveScope::Zoom,
            &need,
            (&[], &HashMap::new()),
            &ExportOptions::default(),
        );
        assert_eq!(
            zoom.get((7, 99, 3)),
            1_000,
            "zoom: every tile, the zoom's max"
        );
        assert_eq!(zoom.get((8, 9, 9)), 5);
        assert_eq!(zoom.get((9, 0, 0)), 0);
        assert_eq!(f[&(7, 10, 10)], 1_000, "a tile starts at its own need");
        assert_eq!(f[&(7, 11, 10)], 500);
        assert_eq!(f[&(7, 9, 9)], 500, "a diagonal neighbour is one ring out");
        let r = SMOOTH_RINGS as u32;
        assert_eq!(f[&(7, 10 + r, 10)], 1_000 >> r, "the last ring");
        assert_eq!(f.get(&(7, 11 + r, 10)), None, "past the last ring");
        assert_eq!(f[&(7, 31, 31)], 32);
        assert_eq!(f[&(8, 1, 0)], 2);
        assert!(f.keys().all(|&(z, x, y)| x < 1 << z && y < 1 << z));
    }
}
