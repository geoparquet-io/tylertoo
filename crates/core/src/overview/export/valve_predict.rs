//! [`SizeValve::Hybrid`]'s level thresholds (#745), predicted before any
//! tile is encoded.
//!
//! [`SizeValve::Zoom`] finds each level's merge threshold by exporting
//! everything twice. This predicts it from what the scan already knows:
//!
//! 1. Estimate each tile's size from its member count and the level's mean
//!    row size ([`member_bytes_estimate`]), deliberately on the low side.
//! 2. For the tiles the estimate puts over the limit, read the bboxes once
//!    more and histogram the members' gaps by `floor(log2(gap))`.
//! 3. Each such tile needs the threshold that merges enough of its members
//!    to fit (tippecanoe's first retry: keep `0.8 × limit / size`); the level
//!    takes the largest. Thresholds are powers of two, so the histogram
//!    gives them exactly.
//!
//! A level with no tile over the estimate costs nothing more. The per-tile
//! loop in `merge_to_fit` is the backstop wherever the estimate is low.
//!
//! EXPERIMENTAL: the estimate is uncalibrated; the benchmark reports the
//! predicted threshold next to the largest one a tile ended on.

use std::collections::HashMap;

use arrow_array::RecordBatch;
use crossbeam_channel::{Receiver, Sender};
use geo::{coord, Rect};
use rayon::prelude::*;

use super::coalesce_valve::{RowGapCursor, GAP_NONE};
use super::{
    buffer_deg_at_zoom, covering_batch_bboxes, decode_batch_bboxes, geometry_index, member_ranges,
    near_world_edge, scoped_pipe, tile_key, tiles_in_ranges, ExportError, ExportOptions,
    ExportTimers, LevelScan, ScanBboxSource, EXPORT_BATCH_SIZE, LEVEL_COLUMN, WAVE_READ_IN_FLIGHT,
};
use crate::overview::level::{Crs, Mode};
use crate::overview::reader::OverviewReader;
use crate::tile::TileBounds;

/// Scales the size estimate down so it errs low: a high estimate would merge
/// in tiles that fit, which the backstop cannot undo.
const ESTIMATE_BIAS: f64 = 0.5;

/// Estimated MVT bytes per encoded vertex (two zigzag varint deltas).
const BYTES_PER_VERTEX: f64 = 2.0;

/// Estimated MVT bytes per property (a key and a value index).
const BYTES_PER_PROPERTY: f64 = 2.0;

/// Estimated MVT bytes per feature beyond geometry and tags (header, id,
/// type, lengths).
const BYTES_PER_FEATURE: f64 = 6.0;

/// Typical gzip ratio of an MVT tile, for the compressed limit.
const GZIP_RATIO: f64 = 0.4;

/// One histogram bucket per possible `floor(log2(gap))`.
type GapHistogram = [u32; 64];

/// The predicted merge threshold of every level, `0` where no tile is
/// expected over the limit.
///
/// `scans` and `zooms` are the unrestricted scan results, so a sharded
/// export predicts the same thresholds as a full one. Returns all zeros,
/// without reading anything, when no limit is set or no tile is flagged.
pub(super) fn predict_level_mingaps(
    reader: &OverviewReader,
    crs: Crs,
    scans: &[LevelScan],
    zooms: &[u8],
    opts: &ExportOptions,
    timers: &ExportTimers,
) -> Result<Vec<u64>, ExportError> {
    let num_levels = scans.len();
    let mut flagged: Vec<HashMap<u64, Flagged>> = (0..num_levels)
        .map(|k| flag_tiles(reader, k, &scans[k], opts))
        .collect();
    let n_flagged: usize = flagged.iter().map(HashMap::len).sum();
    if n_flagged == 0 {
        return Ok(vec![0; num_levels]);
    }
    histogram_gaps(reader, crs, zooms, opts, timers, &mut flagged)?;
    let mingaps: Vec<u64> = flagged
        .iter()
        .map(|tiles| tiles.values().map(Flagged::mingap).max().unwrap_or(0))
        .collect();
    log::info!(
        "[export] size valve hybrid: {n_flagged} tile(s) predicted over the limit; \
         level thresholds {mingaps:?}"
    );
    Ok(mingaps)
}

/// A tile predicted over the limit: the share of it that fits, and its
/// members' gap histogram.
struct Flagged {
    fit: f64,
    hist: GapHistogram,
}

impl Flagged {
    /// The power-of-two threshold that merges enough members for the tile
    /// to fit with tippecanoe's 20% margin.
    fn mingap(&self) -> u64 {
        let n: u64 = self.hist.iter().map(|&c| u64::from(c)).sum();
        if n == 0 {
            return 0;
        }
        let keep = (self.fit * 0.8).min(0.8);
        let merge = ((1.0 - keep) * n as f64).ceil() as u64;
        let mut cum = 0u64;
        for (bucket, &c) in self.hist.iter().enumerate() {
            cum += u64::from(c);
            if cum >= merge {
                // Every gap in bucket `b` is below `2^(b+1)`.
                return 1u64.checked_shl(bucket as u32 + 1).unwrap_or(GAP_NONE);
            }
        }
        GAP_NONE
    }
}

/// The tiles of level `k` whose estimated size is over a limit.
fn flag_tiles(
    reader: &OverviewReader,
    k: usize,
    scan: &LevelScan,
    opts: &ExportOptions,
) -> HashMap<u64, Flagged> {
    let raw = opts.tile_size_limit.filter(|&l| l > 0);
    let gz = opts.compressed_tile_size_limit.filter(|&l| l > 0);
    if raw.is_none() && gz.is_none() {
        return HashMap::new();
    }
    let per_member = member_bytes_estimate(reader, k) * ESTIMATE_BIAS;
    scan.tile_counts
        .iter()
        .filter_map(|(&key, &count)| {
            let est = count as f64 * per_member;
            let mut fit = f64::INFINITY;
            if let Some(l) = raw {
                fit = fit.min(l as f64 / est);
            }
            if let Some(l) = gz {
                fit = fit.min(l as f64 / (est * GZIP_RATIO));
            }
            (fit < 1.0).then_some((key, Flagged { fit, hist: [0; 64] }))
        })
        .collect()
}

/// Estimated MVT bytes of one member of level `k`: its vertices from the
/// geometry column's uncompressed size (16 bytes a vertex in WKB or native
/// encoding), its tags, and its attribute values, which are mostly distinct
/// per feature and so land in the tile's value table about as large as they
/// are in the file.
fn member_bytes_estimate(reader: &OverviewReader, k: usize) -> f64 {
    let Ok(rgs) = reader.row_groups_for_level(k) else {
        return 0.0;
    };
    let schema = reader.schema();
    let Some(g) = geometry_index(schema) else {
        return 0.0;
    };
    let (bytes, rows) = reader.column_bytes_and_rows(&rgs);
    if rows == 0 {
        return 0.0;
    }
    let geometry = schema.field(g).name();
    let covering = match ScanBboxSource::for_reader(reader) {
        ScanBboxSource::Covering { column, .. } => Some(column),
        ScanBboxSource::Geometry => None,
    };
    let per_row = |b: u64| b as f64 / rows as f64;
    let vertices = per_row(bytes.get(geometry).copied().unwrap_or(0)) / 16.0;
    let mut props = 0usize;
    let mut attr_bytes = 0u64;
    for (name, &b) in &bytes {
        if name == geometry || name == LEVEL_COLUMN || covering.as_deref() == Some(name.as_str()) {
            continue;
        }
        props += 1;
        attr_bytes += b;
    }
    BYTES_PER_FEATURE
        + BYTES_PER_VERTEX * vertices
        + BYTES_PER_PROPERTY * props as f64
        + per_row(attr_bytes)
}

/// Read every band's bboxes once, in file order, and add each row's gap to
/// the histogram of every flagged tile it lands in.
fn histogram_gaps(
    reader: &OverviewReader,
    crs: Crs,
    zooms: &[u8],
    opts: &ExportOptions,
    timers: &ExportTimers,
    flagged: &mut [HashMap<u64, Flagged>],
) -> Result<(), ExportError> {
    let num_levels = flagged.len();
    let partitioning = matches!(reader.mode(), Mode::Partitioning);
    let source = ScanBboxSource::for_reader(reader);
    let root = match &source {
        ScanBboxSource::Covering { column, .. } => reader.schema().index_of(column).ok(),
        ScanBboxSource::Geometry => geometry_index(reader.schema()),
    };
    let all_row_groups: Vec<usize> = (0..reader.num_row_groups()).collect();
    let mut cursor = RowGapCursor::new(reader.row_group_bands(&all_row_groups));
    let cursor_ref = &mut cursor;
    let flagged_ref = &mut *flagged;
    scoped_pipe(
        WAVE_READ_IN_FLIGHT,
        |tx: &Sender<(usize, RecordBatch)>| -> Result<(), ExportError> {
            for j in 0..num_levels {
                let mut band_reader = ExportTimers::time(&timers.scan_read, || match root {
                    Some(r) => reader.read_band_projected(j, EXPORT_BATCH_SIZE, &[r]),
                    None => reader.read_band_with_batch_size(j, EXPORT_BATCH_SIZE),
                })?;
                while let Some(batch) = ExportTimers::time(&timers.scan_read, || band_reader.next())
                {
                    if tx.send((j, batch?)).is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        },
        |rx: Receiver<(usize, RecordBatch)>| -> Result<(), ExportError> {
            for (j, batch) in rx.iter() {
                let bboxes = match &source {
                    ScanBboxSource::Covering { column, names } => {
                        match covering_batch_bboxes(&batch, column, names, crs)? {
                            Some(b) => b,
                            // An empty or non-finite covering row: the
                            // prediction gives up and the backstop does the
                            // work.
                            None => return Ok(()),
                        }
                    }
                    ScanBboxSource::Geometry => decode_batch_bboxes(&batch, crs)?,
                };
                let gaps: Vec<u64> = bboxes
                    .iter()
                    .map(|b| cursor_ref.next_row(b.as_ref().map(to_rect)).0)
                    .collect();
                let last = if partitioning { num_levels - 1 } else { j };
                for k in j..=last {
                    if flagged_ref[k].is_empty() {
                        continue;
                    }
                    let counts = bucket_counts(&bboxes, &gaps, zooms[k], opts, &flagged_ref[k]);
                    for (key, hist) in counts {
                        if let Some(f) = flagged_ref[k].get_mut(&key) {
                            for (dst, src) in f.hist.iter_mut().zip(hist) {
                                *dst += src;
                            }
                        }
                    }
                }
            }
            Ok(())
        },
    )
}

/// Per flagged tile, the histogram of this batch's gaps landing in it.
fn bucket_counts(
    bboxes: &[Option<TileBounds>],
    gaps: &[u64],
    zoom: u8,
    opts: &ExportOptions,
    flagged: &HashMap<u64, Flagged>,
) -> HashMap<u64, GapHistogram> {
    let buffer = buffer_deg_at_zoom(zoom, opts);
    bboxes
        .par_iter()
        .zip(gaps.par_iter())
        .fold(
            HashMap::<u64, GapHistogram>::new,
            |mut acc, (bbox, &gap)| {
                let Some(bbox) = bbox else { return acc };
                // DIVERGENCE FROM THE MEMBER PATH: rows near the antimeridian,
                // whose wrapped copies `feature_tile_members` places, are left
                // out. A prediction only.
                if gap == GAP_NONE || near_world_edge(bbox, buffer) {
                    return acc;
                }
                let bucket = 63 - gap.max(1).leading_zeros() as usize;
                let ranges = member_ranges(bbox, zoom, opts);
                for tc in tiles_in_ranges(&ranges, zoom) {
                    let key = tile_key(tc.x, tc.y, zoom);
                    if flagged.contains_key(&key) {
                        acc.entry(key).or_insert([0; 64])[bucket] += 1;
                    }
                }
                acc
            },
        )
        .reduce(HashMap::new, |mut a, b| {
            for (key, hist) in b {
                let dst = a.entry(key).or_insert([0; 64]);
                for (d, s) in dst.iter_mut().zip(hist) {
                    *d += s;
                }
            }
            a
        })
}

fn to_rect(b: &TileBounds) -> Rect<f64> {
    Rect::new(
        coord! { x: b.lng_min, y: b.lat_min },
        coord! { x: b.lng_max, y: b.lat_max },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_merges_the_densest_share() {
        // 100 members: 50 in bucket 3, 50 in bucket 10. A tile at half its
        // limit keeps 40%, so it must merge 60: both buckets.
        let mut hist = [0u32; 64];
        hist[3] = 50;
        hist[10] = 50;
        let half = Flagged { fit: 0.5, hist };
        assert_eq!(half.mingap(), 1 << 11);
        // Barely over (fit 0.99): keep 79.2%, merge 21: bucket 3 is enough.
        let barely = Flagged { fit: 0.99, hist };
        assert_eq!(barely.mingap(), 1 << 4);
        // No members with a gap: nothing to merge.
        let empty = Flagged {
            fit: 0.1,
            hist: [0; 64],
        };
        assert_eq!(empty.mingap(), 0);
    }

    #[test]
    fn top_bucket_saturates() {
        let mut hist = [0u32; 64];
        hist[63] = 10;
        assert_eq!(Flagged { fit: 0.1, hist }.mingap(), GAP_NONE);
    }
}
