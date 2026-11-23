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
//! 1. Size: `count × member_bytes` raw, `× GZIP_RATIO` compressed, against
//!    the caps in force: the estimated fit `f`. `member_bytes` comes from the
//!    footer ([`member_bytes`]). Only tiles estimated well over (`f <
//!    PREDICT_BELOW_FIT`) get a need: a false positive puts floors on up to
//!    288 neighbours, a missed near-cap tile only skips a small step.
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

/// Typical gzip ratio of a dense polygon tile (NZ parcels 0.43, Moldova
/// fields 0.46 over their unmerged tiles).
const GZIP_RATIO: f64 = 0.45;

/// Predict a need only for tiles estimated below this fit.
const PREDICT_BELOW_FIT: f64 = 0.9;

/// `keep = 0.8 × fit^KEEP_EXP`: with the true first-encode fit, 1.0 under-
/// predicts NZ by 1.16× and Moldova by 1.5×; 1.2 is within 1.2× of both.
const KEEP_EXP: f64 = 1.2;

type Hist = Box<[u32; BUCKETS]>;

#[derive(Default)]
struct TileStat {
    count: u32,
    hist: Option<Hist>,
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

type Partial = HashMap<u64, (u32, Hist)>;

fn merge_partial(mut a: Partial, b: Partial) -> Partial {
    if a.len() < b.len() {
        return merge_partial(b, a);
    }
    for (k, (c, h)) in b {
        match a.entry(k) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let e = e.get_mut();
                e.0 += c;
                e.1.iter_mut().zip(h.iter()).for_each(|(x, y)| *x += y);
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((c, h));
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
        let cursor = &mut self.cursor.as_mut().expect("a band is open").1;
        let gaps = cursor.batch_rects(&rects);
        let pending: Vec<(TileBounds, u64)> = self
            .pending
            .iter()
            .zip(gaps)
            .filter_map(|(r, (gap, _))| {
                let b = (*r)?;
                let point = b.lng_min == b.lng_max && b.lat_min == b.lat_max;
                Some((b, if point { GAP_NONE } else { gap }))
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
                    .fold(Partial::new, |mut acc, (b, gap)| {
                        let slot = (*gap != GAP_NONE).then(|| bucket(*gap, zoom));
                        for_each_member_key(b, zoom, opts, |key| {
                            let e = acc
                                .entry(key)
                                .or_insert_with(|| (0, Box::new([0; BUCKETS])));
                            e.0 += 1;
                            if let Some(s) = slot {
                                e.1[s] += 1;
                            }
                        });
                        acc
                    })
                    .reduce(Partial::new, merge_partial);
                // A tile keeps a histogram from the flush its count reaches
                // HIST_MIN_MEMBERS on.
                for (key, (count, hist)) in flushed {
                    let t = level.entry(key).or_default();
                    t.count = t.count.saturating_add(count);
                    match &mut t.hist {
                        Some(h) => h.iter_mut().zip(hist.iter()).for_each(|(a, b)| *a += b),
                        None if t.count >= HIST_MIN_MEMBERS => t.hist = Some(hist),
                        None => {}
                    }
                }
            });
        self.pending.clear();
        self.sized += t.elapsed();
    }

    /// Each tile's predicted need, keyed `(zoom, x, y)`; tiles predicted to
    /// fit are absent.
    pub(crate) fn predict(
        &self,
        zooms: &[u8],
        member_bytes: f64,
        opts: &ExportOptions,
    ) -> HashMap<(u8, u32, u32), u64> {
        let caps = (
            opts.tile_size_limit.filter(|&l| l > 0),
            opts.compressed_tile_size_limit.filter(|&l| l > 0),
        );
        let mut out = HashMap::new();
        for (level, &zoom) in self.levels.iter().zip(zooms) {
            for (&key, t) in level {
                let Some(hist) = &t.hist else { continue };
                if let Some(need) = predict_need(hist, t.count, zoom, member_bytes, caps) {
                    let (x, y) = key_to_xy(key, zoom);
                    out.insert((zoom, x, y), need);
                }
            }
        }
        out
    }
}

/// One tile's predicted merge threshold, or `None` when it is predicted to
/// fit (or within the margin of fitting).
pub(crate) fn predict_need(
    hist: &[u32; BUCKETS],
    count: u32,
    zoom: u8,
    member_bytes: f64,
    (raw, gz): (Option<usize>, Option<usize>),
) -> Option<u64> {
    let est = f64::from(count) * member_bytes;
    let mut fit = f64::INFINITY;
    if let Some(l) = raw {
        fit = fit.min(l as f64 / est);
    }
    if let Some(l) = gz {
        fit = fit.min(l as f64 / (est * GZIP_RATIO));
    }
    if fit >= PREDICT_BELOW_FIT {
        return None;
    }
    let keep = (0.8 * fit.powf(KEEP_EXP)).min(0.8);
    // The valve's threshold is one above the quantile gap.
    quantile(hist, 1.0 - keep, zoom).map(|g| g.saturating_add(1))
}

/// Estimated raw MVT bytes of one member of a dense tile, from the footer:
/// a small polygon's geometry and feature header, then per property a tag
/// pair and the column's stored bytes per row (distinct values land in the
/// value table about as large; repeated ones dictionary-encode in both).
/// NZ parcels: 102 against 94 measured; Moldova fields: 64 against 70.
pub(crate) fn member_bytes(column_bytes: &HashMap<String, f64>, properties: &[&str]) -> f64 {
    const GEOMETRY_AND_HEADER: f64 = 24.0;
    const TAG_PAIR: f64 = 2.5;
    GEOMETRY_AND_HEADER
        + properties
            .iter()
            .map(|p| TAG_PAIR + column_bytes.get(*p).copied().unwrap_or(0.0))
            .sum::<f64>()
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
        // 100 bytes a member: 20,971 fill the cap.
        assert_eq!(predict_need(&hist, 19_000, zoom, 100.0, caps), None);
        let mild = predict_need(&hist, 30_000, zoom, 100.0, caps).unwrap();
        let hard = predict_need(&hist, 120_000, zoom, 100.0, caps).unwrap();
        assert!(hard > 2 * mild, "{mild} {hard}");
        // The compressed cap binds when it is the tighter one.
        let gz = (Some(2 * 1024 * 1024), Some(500 * 1024));
        assert!(predict_need(&hist, 15_000, zoom, 100.0, gz).is_some());
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
