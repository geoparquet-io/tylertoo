//! Coarse-tile work guards for sparse and heavily overlapping polygons.
use super::*;
use crate::valve_counter::{self, Counts};
use geo::{LineString, Polygon};
use std::time::Instant;

fn byte_fingerprints(mvt: &TileMvt) -> (u64, Option<u64>) {
    use xxhash_rust::xxh3::xxh3_64;
    (xxh3_64(&mvt.data), mvt.gzip.as_ref().map(|g| xxh3_64(g)))
}

/// Fixed-seed, jittered parcels in one z0 tile. Coordinates are chosen in tile
/// space then unprojected, so they survive default 4096-unit quantization.
/// Most are disjoint; every eighth column shares an edge with its neighbour.
/// The slow million-row case packs more finely, but still above one tile unit.
fn dense_members(rows: usize) -> Vec<Member> {
    square_members(rows, false)
}

fn square_members(rows: usize, overlapping: bool) -> Vec<Member> {
    let side = (rows as f64).sqrt().ceil() as usize;
    let step = if overlapping {
        0.4
    } else {
        3800.0 / side as f64
    };
    let mut seed = 0x0075_8779_dead_beef_u64;
    let mut random = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / ((1u64 << 53) as f64)
    };
    let unproject = |x: f64, y: f64| {
        (
            x / 4096.0 * 360.0 - 180.0,
            (std::f64::consts::PI * (1.0 - 2.0 * y / 4096.0))
                .sinh()
                .atan()
                .to_degrees(),
        )
    };
    let geoms: Vec<Geometry<f64>> = (0..rows)
        .map(|i| {
            let (col, row) = (i % side, i / side);
            let x = 128.0 + col as f64 * step + if overlapping { 0.2 * random() } else { 0.0 };
            let y = 128.0 + row as f64 * step + if overlapping { 0.2 * random() } else { 0.0 };
            let width = if overlapping {
                4.0
            } else if col % 8 == 0 {
                step
            } else {
                step * (0.55 + 0.25 * random())
            };
            let height = if overlapping {
                width
            } else {
                step * (0.55 + 0.25 * random())
            };
            Geometry::Polygon(Polygon::new(
                LineString::from(vec![
                    unproject(x, y),
                    unproject(x + width, y),
                    unproject(x + width, y + height),
                    unproject(x, y + height),
                    unproject(x, y),
                ]),
                Vec::new(),
            ))
        })
        .collect();
    let gaps = RowGapCursor::new(vec![(0, rows as u64)]).batch(&geoms);
    let parcel_key: Arc<str> = Arc::from("parcel");
    let class_key: Arc<str> = Arc::from("class");
    geoms
        .into_iter()
        .zip(gaps)
        .enumerate()
        .map(|(i, (geom, (gap, band)))| Member {
            key: tile_key(0, 0, 0),
            seq: i as u64,
            geom,
            props: Arc::new(vec![
                (Arc::clone(&parcel_key), PropertyValue::UInt(i as u64)),
                (
                    Arc::clone(&class_key),
                    PropertyValue::String("field".to_owned()),
                ),
            ]),
            id: Some(i as u64),
            gap,
            band,
        })
        .collect()
}

fn check_work(rows: usize) {
    let start = Instant::now();
    let (mvt, counts) = valve_counter::measure(|| {
        let members = dense_members(rows);
        let opts = ExportOptions {
            tile_size_limit: Some(rows * 20),
            compressed_tile_size_limit: Some(rows * 6),
            ..Default::default()
        };
        assert_eq!(opts.size_valve, SizeValve::Merge);
        encode_tile(
            &test_gzip_cache(),
            &members,
            &TileCoord::new(0, 0, 0).bounds(),
            &opts,
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        )
    });
    eprintln!(
        "valve_work rows={rows} passes={} merged={} dropped={} features={} raw={} gzip={:?} elapsed={:?}\n{counts:#?}\npath order: other, first-pass plain, no-merge plain, cache-rejected plain, feature fallback, cache",
        mvt.valve.passes, mvt.valve.merged, mvt.valve.dropped, mvt.features,
        mvt.data.len(), mvt.gzip.as_ref().map(|g| g.len()), start.elapsed(),
    );
    eprintln!(
        "valve_work rows={rows} byte fingerprints: {:?}",
        byte_fingerprints(&mvt)
    );
    assert!(
        (2..=4).contains(&mvt.valve.passes),
        "fixture must exercise 2–4 passes: {counts:#?}"
    );
    assert_eq!(
        mvt.valve.dropped, 0,
        "fixture must exercise merging: {counts:#?}"
    );
    assert_budgets(rows, mvt.valve.passes as usize, &counts);
}

fn assert_budgets(rows: usize, passes: usize, c: &Counts) {
    let mut failures = Vec::new();
    // The cache promises at most one exact test per distinct box-meeting
    // pair over ALL passes, not one per pair per pass. These inputs each
    // have one part, one ring and the same merge key, so its candidate count
    // is the tile's complete distinct-pair budget.
    if c.parts_interact > c.candidate_pairs {
        failures.push("parts_interact exceeds distinct candidate pairs");
    }
    // concat_geometry may copy each member's polygon once per merging pass;
    // the first pass merges nothing. This is a linear ceiling, not a claim
    // that repeating those deep copies is necessary when carrying work on.
    if c.polygon_clones > rows * (passes - 1) {
        failures.push("polygon clones exceed one per member per merging pass");
    }
    // Projection/quantization and bbox-gap measurement belong to the input,
    // not each new grouping. Each input ring/row is processed at most once.
    if c.quantized_new != 1 || c.quantized_none != 0 || c.rings_quantized > rows {
        failures.push("quantization is repeated or the sparse fixture lost its cache");
    }
    if c.gap_rows != rows || c.coalesce_calls != passes {
        failures.push("unexpected row measurement or coalescing work");
    }
    // Input order is the draw order here. Allow one initial normalization;
    // coalescing emits an ordered subsequence, so later passes can carry
    // that order forward without sorting every survivor again.
    if c.draw_sorts > 1 || c.draw_elements > rows {
        failures.push("draw order is sorted again after an order-preserving pass");
    }
    assert!(
        failures.is_empty(),
        "{}\nall counters: {c:#?}",
        failures.join("; ")
    );
}

#[test]
fn valve_work_per_pass_is_bounded() {
    check_work(100_000);
}

/// Hundreds of box neighbours per part exceed the stored-pair cap. The
/// quantized geometry must survive that cap, and connected parts need no
/// further exact interaction tests within the same component.
#[test]
fn valve_work_overlapping_parts_is_bounded() {
    let rows = 50_000;
    let start = Instant::now();
    let (mvt, c) = valve_counter::measure(|| {
        let members = square_members(rows, true);
        encode_tile(
            &test_gzip_cache(),
            &members,
            &TileCoord::new(0, 0, 0).bounds(),
            &ExportOptions {
                tile_size_limit: Some(rows * 2),
                compressed_tile_size_limit: Some(rows / 2),
                ..Default::default()
            },
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        )
    });
    eprintln!(
        "valve_work overlapping rows={rows} passes={} merged={} dropped={} raw={} gzip={:?} elapsed={:?}\n{c:#?}",
        mvt.valve.passes, mvt.valve.merged, mvt.valve.dropped,
        mvt.data.len(), mvt.gzip.as_ref().map(|g| g.len()), start.elapsed(),
    );
    eprintln!(
        "valve_work overlapping rows={rows} byte fingerprints: {:?}",
        byte_fingerprints(&mvt)
    );
    assert!(mvt.valve.passes >= 2, "fixture must exercise merging");
    assert_eq!(mvt.valve.dropped, 0);
    assert!(
        c.candidate_pairs > 64 * rows,
        "fixture must exceed the pair cap"
    );
    assert!(
        c.quantized_new == 1
            && c.quantized_none == 0
            && c.rings_quantized <= rows
            && c.parts_interact <= 4 * rows
            && c.draw_elements <= 2 * rows,
        "dense tile exceeded work budgets: {c:#?}"
    );
}

#[test]
fn valve_work_draw_rank_matches_stable_sorts() {
    for drop_only in [false, true] {
        for order in [
            FeatureOrder::Input,
            FeatureOrder::Column {
                name: "order".into(),
                descending: false,
            },
            FeatureOrder::Column {
                name: "order".into(),
                descending: true,
            },
        ] {
            let mut members = dense_members(400);
            for (i, m) in members.iter_mut().enumerate() {
                if i % 7 != 0 {
                    Arc::make_mut(&mut m.props)
                        .push((Arc::from("order"), PropertyValue::UInt((i % 5) as u64)));
                }
                if drop_only {
                    m.gap = GAP_NONE;
                }
            }
            members.reverse();
            members.rotate_left(37);
            let opts = ExportOptions {
                tile_size_limit: Some(8_000),
                compressed_tile_size_limit: None,
                feature_order: order,
                ..Default::default()
            };
            let bounds = TileCoord::new(0, 0, 0).bounds();
            let diagnostics = crate::diagnostics::Diagnostics::default();
            let cache = test_gzip_cache();
            let (mvt, c) = valve_counter::measure(|| {
                encode_tile(
                    &cache,
                    &members,
                    &bounds,
                    &opts,
                    &[],
                    &mut FillTally::default(),
                    &diagnostics,
                )
            });
            assert!(mvt.valve.passes >= 2);
            assert_eq!(mvt.valve.dropped > 0, drop_only);
            assert!(c.draw_elements <= 2 * members.len());
            let mut ordered: Vec<_> = members.iter().collect();
            if !matches!(opts.feature_order, FeatureOrder::Input) {
                ordered.sort_unstable_by_key(|m| m.seq);
            }
            let merged = coalesce_valve::coalesce_members(&ordered, mvt.valve.mingap);
            let mut feats: Vec<_> = merged.iter().collect();
            // Reference: the original two stable sorts, followed by plain
            // encoding, independent of the valve's ranks and geometry cache.
            feats.sort_by_key(|m| m.target.seq);
            if let FeatureOrder::Column { name, descending } = &opts.feature_order {
                feats.sort_by(|a, b| {
                    compare_by_feature_order(a.target, b.target, name, *descending)
                });
            }
            if drop_only {
                // All fixtures' squares have equal coordinate counts, so
                // the old drop selection keeps the draw-ordered prefix.
                feats = select_kept_members(&feats, members.len() - mvt.valve.dropped)
                    .into_iter()
                    .copied()
                    .collect();
            }
            let reference = build_mvt(&cache, &feats, &bounds, &opts, &[], &diagnostics);
            assert_eq!(
                mvt.data, reference.data,
                "drop={drop_only}, {:?}",
                opts.feature_order
            );
        }
    }
}

#[test]
#[ignore = "slow million-parcel valve diagnostic; run explicitly with --ignored --nocapture"]
fn valve_work_per_pass_is_bounded_slow_1m() {
    check_work(1_000_000);
}
