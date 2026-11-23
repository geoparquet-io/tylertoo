//! SCRATCH PROTOTYPE (#768): `TYLERTOO_VALVE_SCOPE=predict`, the smooth
//! valve without a probe export.
//!
//! The export scan ([`super::scan_all_levels`]) already visits every row's
//! bbox and every tile it lands in. With `predict` it also walks each band's
//! rows in file order to get the merge gaps the export will see
//! ([`RowGapCursor`], same predecessor rule), and keeps per tile a member
//! count and a half-octave histogram of those gaps ([`Scan`]).
//!
//! After the scan, each tile's need is predicted ([`predict`]):
//!
//! 1. Size: `count × (raw, gzipped) bytes a member` against the caps in
//!    force: the estimated fit `f`. The bytes a member come from a real
//!    encode of a sample of each level's densest tile
//!    ([`super::valve_sample_bytes`]): the footer cannot price attributes
//!    the way MVT does (a value table deduplicated per tile, tags that gzip
//!    to nothing when a column is constant, nested columns written as
//!    JSON), and gzip ratios run 0.2 (Madagascar adm4, 53 columns) to 0.6.
//!    Only tiles estimated well over (`f < PREDICT_BELOW_FIT`) get a need: a
//!    false positive puts floors on up to 288 neighbours, a missed near-cap
//!    tile only skips a small step.
//! 2. Need: the merge valve's threshold is a gap quantile. It keeps about
//!    `0.8 × f` of the features on its second encode, a little less when it
//!    takes more (`KEEP_EXP`). The need is the histogram's `1 − keep`
//!    quantile.
//!
//! Floors are the prototype's smooth rule over neighbours
//! ([`super::valve_scope::smooth_floors`]), and a tile predicted over starts
//! at its own predicted need (`TYLERTOO_VALVE_SELF`), so it usually fits on
//! its first encode. The ladder in `merge_to_fit` stays the backstop: a tile
//! whose real need is higher still raises its threshold until it fits, so
//! the cap holds. A tile's own over-prediction merges it harder than it
//! needed (NZ: kept share 0.85-0.93 of the probe's, median per level).
//!
//! Unlike the probe, the scan reads every tile even under a `--tile-range`
//! shard (the scan is restricted after), so shards predict the same floors.
//! shortcut: a geometry-sourced scan (no bbox covering) drops null rows
//! before the gap walk, so its gaps can shift at a null; walk the
//! row-aligned bboxes there too when a covering-less input needs it.

use std::collections::HashMap;
use std::ops::RangeInclusive;

use geo::Rect;
use rayon::prelude::*;

use super::coalesce_valve::{RowGapCursor, GAP_NONE};
use super::{for_each_member_key, key_to_xy, ExportOptions};
use crate::tile::TileBounds;

/// Half-octave gap buckets, relative to the tile's pixel: bucket 8 starts at
/// one squared pixel (4096 per tile side), the last at 2^28 px².
pub(crate) const BUCKETS: usize = 64;
const BUCKET_OFFSET: f64 = 8.0;

/// A tile keeps a gap histogram once it holds this many members: bounds the
/// scan's memory at one histogram per this many memberships. A tile over a
/// cap holds tens of thousands; the few gaps it misses before crossing do
/// not move a quantile.
const HIST_MIN_MEMBERS: u32 = 1024;

/// Predict a need only for tiles estimated below this fit.
const PREDICT_BELOW_FIT: f64 = 0.9;

/// Sample a tile's own bytes a member when its level's put it below this.
const CANDIDATE_FIT: f64 = 2.0;

/// `keep = 0.8 × fit^KEEP_EXP`: with the true first-encode fit, 1.0 under-
/// predicts NZ by 1.16× and Moldova by 1.5×; 1.2 is within 1.2× of both.
const KEEP_EXP: f64 = 1.2;

type Hist = Box<[u32; BUCKETS]>;

/// Raw and gzipped MVT bytes of one member.
pub(crate) type MemberBytes = (f64, f64);

#[derive(Default)]
struct TileStat {
    count: u32,
    hist: Option<Hist>,
    /// Where most of its members were read: the flush that held the most,
    /// as `(members, band, the band's row its first member was on)`.
    densest_run: (u32, u16, u64),
}

/// A tile to price with a sample encode: its level, key, and the band and
/// band row its densest run of members starts at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SampleAt {
    pub(crate) level: usize,
    pub(crate) key: u64,
    pub(crate) band: u16,
    pub(crate) row: u64,
}

/// The predictor's share of the scan: per level, per tile key.
pub(crate) struct Scan {
    levels: Vec<HashMap<u64, TileStat>>,
    /// The band being walked and its gap cursor.
    cursor: Option<(usize, RowGapCursor)>,
    /// Rows waiting for [`Self::flush`], row-aligned, and the levels they
    /// fan to.
    pending: Vec<Option<TileBounds>>,
    pending_levels: RangeInclusive<usize>,
    /// The open band's rows already flushed.
    band_rows: u64,
    /// SCRATCH: time spent walking gaps and sizing flushes.
    pub(crate) walk: std::time::Duration,
    pub(crate) sized: std::time::Duration,
}

/// Rows one [`Scan::flush`] sizes: a fixed count, so where a tile's
/// histogram starts does not depend on the pool or the read's batching.
const FLUSH_ROWS: usize = 1 << 16;

/// log2 of one squared pixel at `zoom`, in squared world units.
fn px2_log2(zoom: u8) -> f64 {
    2.0 * (32.0 - f64::from(zoom) - 12.0)
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

/// Per tile key: members, gap histogram, first band row.
type Partial = HashMap<u64, (u32, Hist, u64)>;

fn merge_partial(mut a: Partial, b: Partial) -> Partial {
    if a.len() < b.len() {
        return merge_partial(b, a);
    }
    for (k, (c, h, r)) in b {
        match a.entry(k) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let e = e.get_mut();
                e.0 += c;
                e.1.iter_mut().zip(h.iter()).for_each(|(x, y)| *x += y);
                e.2 = e.2.min(r);
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((c, h, r));
            }
        }
    }
    a
}

impl Scan {
    pub(crate) fn new(levels: usize) -> Self {
        Self {
            levels: (0..levels).map(|_| HashMap::new()).collect(),
            cursor: None,
            pending: Vec::new(),
            pending_levels: 0..=0,
            band_rows: 0,
            walk: std::time::Duration::ZERO,
            sized: std::time::Duration::ZERO,
        }
    }

    /// Forget everything: the scan restarts from the geometry.
    pub(crate) fn reset(&mut self) {
        *self = Self::new(self.levels.len());
    }

    /// One batch of band `j`'s rows, row-aligned (`None`: no bbox), fanned to
    /// `levels`. `groups` gives the band's `(band, rows)` row groups, called
    /// when the band starts.
    pub(crate) fn batch(
        &mut self,
        j: usize,
        groups: impl FnOnce() -> Vec<(u16, u64)>,
        rows: &[Option<TileBounds>],
        levels: RangeInclusive<usize>,
        zooms: &[u8],
        opts: &ExportOptions,
    ) {
        if self.cursor.as_ref().is_none_or(|(b, _)| *b != j) {
            self.flush(zooms, opts);
            self.cursor = Some((j, RowGapCursor::new(groups())));
            self.pending_levels = levels;
            self.band_rows = 0;
        }
        for row in rows {
            self.pending.push(*row);
            if self.pending.len() == FLUSH_ROWS {
                self.flush(zooms, opts);
            }
        }
    }

    /// Size the pending rows into every level they fan to, in the pool.
    pub(crate) fn flush(&mut self, zooms: &[u8], opts: &ExportOptions) {
        if self.pending.is_empty() {
            return;
        }
        let t = std::time::Instant::now();
        let rects: Vec<Option<Rect<f64>>> = self
            .pending
            .par_iter()
            .with_min_len(2048)
            .map(|r| r.map(|b| Rect::new((b.lng_min, b.lat_min), (b.lng_max, b.lat_max))))
            .collect();
        let (band, cursor) = self.cursor.as_mut().expect("a band is open");
        let band = u16::try_from(*band).unwrap_or(u16::MAX);
        let gaps = cursor.batch_rects(&rects);
        let first = self.band_rows;
        let pending: Vec<(TileBounds, u64, u64)> = self
            .pending
            .iter()
            .zip(gaps)
            .zip(first..)
            .filter_map(|((r, (gap, _)), row)| {
                let b = (*r)?;
                let point = b.lng_min == b.lng_max && b.lat_min == b.lat_max;
                Some((b, if point { GAP_NONE } else { gap }, row))
            })
            .collect();
        self.walk += t.elapsed();
        let t = std::time::Instant::now();
        let range = self.pending_levels.clone();
        self.levels[range.clone()]
            .par_iter_mut()
            .zip(&zooms[range])
            .for_each(|(level, &zoom)| {
                let flushed = pending
                    .par_iter()
                    .with_min_len(2048)
                    .fold(Partial::new, |mut acc, (b, gap, row)| {
                        let slot = (*gap != GAP_NONE).then(|| bucket(*gap, zoom));
                        for_each_member_key(b, zoom, opts, |key| {
                            let e = acc
                                .entry(key)
                                .or_insert_with(|| (0, Box::new([0; BUCKETS]), *row));
                            e.0 += 1;
                            e.2 = e.2.min(*row);
                            if let Some(s) = slot {
                                e.1[s] += 1;
                            }
                        });
                        acc
                    })
                    .reduce(Partial::new, merge_partial);
                // A tile keeps a histogram from the flush its count reaches
                // HIST_MIN_MEMBERS on.
                for (key, (count, hist, row)) in flushed {
                    let t = level.entry(key).or_default();
                    t.count = t.count.saturating_add(count);
                    if count > t.densest_run.0 {
                        t.densest_run = (count, band, row);
                    }
                    match &mut t.hist {
                        Some(h) => h.iter_mut().zip(hist.iter()).for_each(|(a, b)| *a += b),
                        None if t.count >= HIST_MIN_MEMBERS => t.hist = Some(hist),
                        None => {}
                    }
                }
            });
        self.band_rows += self.pending.len() as u64;
        self.pending.clear();
        self.sized += t.elapsed();
    }

    /// Each tile's predicted need, keyed `(zoom, x, y)`; tiles predicted to
    /// fit are absent.
    /// Each level's tile with the most members among those with a
    /// histogram (the ones that can be predicted over), lowest key on a tie.
    pub(crate) fn densest(&self) -> Vec<Option<SampleAt>> {
        self.levels
            .iter()
            .enumerate()
            .map(|(level, tiles)| {
                tiles
                    .iter()
                    .filter(|(_, t)| t.hist.is_some())
                    .max_by_key(|(&key, t)| (t.count, std::cmp::Reverse(key)))
                    .map(|(&key, t)| sample_at(level, key, t))
            })
            .collect()
    }

    /// The tiles worth their own sample: estimated under
    /// `CANDIDATE_FIT` at their level's bytes a member. Tiles of one level
    /// run up to 1.5× apart in bytes a member (Moldova gzip 26-32 at z9).
    pub(crate) fn candidates(
        &self,
        level_bytes: &[Option<MemberBytes>],
        opts: &ExportOptions,
    ) -> Vec<SampleAt> {
        let caps = caps(opts);
        let mut out: Vec<SampleAt> = self
            .levels
            .iter()
            .zip(level_bytes)
            .enumerate()
            .filter_map(|(level, (tiles, bytes))| Some((level, tiles, (*bytes)?)))
            .flat_map(|(level, tiles, bytes)| {
                tiles
                    .iter()
                    .filter(move |(_, t)| {
                        t.hist.is_some() && fit(t.count, bytes, caps) < CANDIDATE_FIT
                    })
                    .map(move |(&key, t)| sample_at(level, key, t))
            })
            .collect();
        out.sort_by_key(|s| (s.level, s.key));
        out
    }

    /// Needs of the tiles predicted over, each priced at its own sampled
    /// `(raw, gzipped)` bytes a member (`tile_bytes`), else its level's; a
    /// tile with neither predicts nothing.
    pub(crate) fn predict(
        &self,
        zooms: &[u8],
        level_bytes: &[Option<MemberBytes>],
        tile_bytes: &HashMap<(usize, u64), MemberBytes>,
        opts: &ExportOptions,
    ) -> HashMap<(u8, u32, u32), u64> {
        let caps = caps(opts);
        let mut out = HashMap::new();
        for (index, ((level, &zoom), bytes)) in
            self.levels.iter().zip(zooms).zip(level_bytes).enumerate()
        {
            for (&key, t) in level {
                let Some(hist) = &t.hist else { continue };
                let Some(bytes) = tile_bytes.get(&(index, key)).copied().or(*bytes) else {
                    continue;
                };
                if let Some(need) = predict_need(hist, t.count, zoom, bytes, caps) {
                    let (x, y) = key_to_xy(key, zoom);
                    out.insert((zoom, x, y), need);
                }
            }
        }
        out
    }
}

fn sample_at(level: usize, key: u64, t: &TileStat) -> SampleAt {
    SampleAt {
        level,
        key,
        band: t.densest_run.1,
        row: t.densest_run.2,
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
fn fit(count: u32, (raw_member, gz_member): MemberBytes, (raw, gz): Caps) -> f64 {
    let n = f64::from(count);
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
    count: u32,
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

    #[test]
    fn a_tile_is_sampled_where_its_densest_run_starts_and_only_near_a_cap() {
        let opts = ExportOptions::default();
        let zooms = [10u8];
        // Band 0: 100 rows elsewhere, then 70,000 small features inside one
        // z10 tile, spread over two flushes; band 1 adds a sparse tile.
        let inside = |i: u32| {
            let x = 10.0 + f64::from(i % 300) * 1e-4;
            let y = 10.0 + f64::from(i / 300) * 1e-5;
            Some(TileBounds::new(x, y, x + 1e-5, y + 1e-5))
        };
        let far = Some(TileBounds::new(-60.0, -30.0, -59.9999, -29.9999));
        let mut rows = vec![far; 100];
        rows.extend((0..70_000).map(inside));
        let mut scan = Scan::new(1);
        scan.batch(
            0,
            || vec![(0, rows.len() as u64)],
            &rows,
            0..=0,
            &zooms,
            &opts,
        );
        let sparse = vec![far; 2_000];
        scan.batch(1, || vec![(1, 2_000)], &sparse, 0..=0, &zooms, &opts);
        scan.flush(&zooms, &opts);

        let dense = scan.densest()[0].expect("a tile with a histogram");
        // The first flush holds 65,436 of its rows from band row 100 on.
        assert_eq!((dense.band, dense.row), (0, 100));
        // 70,000 members at 100 bytes raw: 3.4x the 2 MiB cap; the far tile
        // (2,100 members) is nowhere near it and is not sampled.
        let bytes = [Some((100.0, 20.0))];
        let c = scan.candidates(&bytes, &opts);
        assert_eq!(c, vec![dense]);
        let need = scan.predict(&zooms, &bytes, &HashMap::new(), &opts);
        assert_eq!(need.len(), 1);
        // Its own sampled bytes win over the level's: at 20 bytes raw and 5
        // gzipped it fits.
        let own = HashMap::from([((0, dense.key), (20.0, 5.0))]);
        assert!(scan.predict(&zooms, &bytes, &own, &opts).is_empty());
    }

    #[test]
    fn floors_skip_self_and_the_zoom_max_is_the_largest_need() {
        use super::super::valve_scope::{smooth_floors, zoom_floors};
        let need = HashMap::from([((7, 10, 10), 1_000u64), ((7, 30, 30), 64), ((8, 0, 0), 5)]);
        let f = smooth_floors(&need, 0.5, false);
        assert_eq!(
            f.get(&(7, 10, 10)),
            None,
            "a tile's own need is not its floor"
        );
        assert_eq!(f[&(7, 11, 10)], 500);
        assert_eq!(f[&(7, 12, 8)], 250);
        assert_eq!(f[&(7, 18, 10)], 3, "eight rings out");
        assert_eq!(f.get(&(7, 19, 10)), None, "nine rings out");
        assert_eq!(f[&(7, 31, 31)], 32);
        assert_eq!(f[&(8, 1, 0)], 2);
        assert!(f.keys().all(|&(z, x, y)| x < 1 << z && y < 1 << z));
        let with_self = smooth_floors(&need, 0.5, true);
        assert_eq!(with_self[&(7, 10, 10)], 1_000);
        assert_eq!(zoom_floors(&need), HashMap::from([(7, 1_000), (8, 5)]));
    }
}
