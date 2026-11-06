//! Coarse-tile work guards for sparse and heavily overlapping polygons.
use super::*;
use crate::valve_counter::{self, Counts};
use geo::{LineString, Polygon};
use std::time::Instant;

/// Inputs and worker threads belong to the collector, outside this encode
/// ledger. Like `encode_cost_tests`, dhat requires nextest process isolation.
#[cfg(feature = "dhat-heap")]
#[test]
fn valve_encode_peak_live_bytes_matches_price() {
    valve_encode_ledger(
        "squares",
        square_members(100_000, true),
        (5_960_687_766_429_934_319, Some(14_479_824_291_116_707_907)),
    );
}

#[cfg(feature = "dhat-heap")]
#[test]
fn valve_encode_peak_live_bytes_matches_price_vertex_heavy() {
    valve_encode_ledger(
        "vertex-heavy",
        vertex_heavy_members(100_000),
        (12_545_578_015_038_857_891, Some(5_218_640_095_989_453_000)),
    );
}

#[cfg(feature = "dhat-heap")]
fn valve_encode_ledger(fixture: &str, members: Vec<Member>, expected: (u64, Option<u64>)) {
    if std::env::var_os("NEXTEST_EXECUTION_MODE").as_deref()
        != Some(std::ffi::OsStr::new("process-per-test"))
    {
        eprintln!("skipped: the dhat peak requires nextest process-per-test mode");
        return;
    }
    let rows = members.len();
    let opts = ExportOptions {
        tile_size_limit: Some(rows * 2),
        compressed_tile_size_limit: Some(rows / 2),
        ..Default::default()
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    let (peak, passes, dropped, fingerprints) = pool.install(|| {
        let profiler = dhat::Profiler::builder().testing().build();
        let mvt = encode_tile(
            &test_gzip_cache(),
            &members,
            &TileCoord::new(0, 0, 0).bounds(),
            &opts,
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        );
        let measured = (
            dhat::HeapStats::get().max_bytes,
            mvt.valve.passes,
            mvt.valve.dropped,
            byte_fingerprints(&mvt),
        );
        drop(mvt);
        drop(profiler);
        measured
    });
    let gzip_charge = crate::compression::tile_gzip::PAYLOAD_SLICE_BYTES
        * crate::compression::tile_gzip::PAYLOAD_SLICES;
    let estimate = rows as u64 * VALVE_TRANSIENT_BYTES_PER_MEMBER + gzip_charge as u64;
    eprintln!(
        "valve ledger: fixture={fixture} members={rows} peak={peak} \
         peak_per_member={} estimate={estimate} \
         valve_price={VALVE_TRANSIENT_BYTES_PER_MEMBER} gzip_charge={gzip_charge} \
         passes={passes} fingerprints={fingerprints:?}",
        peak as f64 / rows as f64,
    );
    assert!(passes >= 2, "fixture must exercise merging");
    assert_eq!(dropped, 0);
    // Captured before changing the memory price and draw-order scratch:
    // both the raw and gzip bytes must stay exact.
    assert_eq!(fingerprints, expected);
    assert!(
        (estimate / 2..=estimate * 5 / 4).contains(&(peak as u64)),
        "valve encode peak={peak} estimate={estimate}: expected 0.5–1.25 × estimate"
    );
}

/// Deterministic overlapping, jittered-radius rings with 39 distinct vertices
/// plus closure. The 24-unit diameter retains detail after z0 quantization.
#[cfg(feature = "dhat-heap")]
fn vertex_heavy_members(rows: usize) -> Vec<Member> {
    let side = (rows as f64).sqrt().ceil() as usize;
    let mut members = square_members(rows, true);
    for (i, m) in members.iter_mut().enumerate() {
        let x = 128.0 + (i % side) as f64 * 0.4;
        let y = 128.0 + (i / side) as f64 * 0.4;
        let mut ring: Vec<_> = (0..39)
            .map(|j| {
                let angle = std::f64::consts::TAU * j as f64 / 39.0;
                let radius = 10.0 + 2.0 * (7.0 * angle + i as f64 * 0.01).sin();
                let x = x + radius * angle.cos();
                let y = y + radius * angle.sin();
                (
                    x / 4096.0 * 360.0 - 180.0,
                    (std::f64::consts::PI * (1.0 - 2.0 * y / 4096.0))
                        .sinh()
                        .atan()
                        .to_degrees(),
                )
            })
            .collect();
        ring.push(ring[0]);
        m.geom = Geometry::Polygon(Polygon::new(LineString::from(ring), Vec::new()));
    }
    members
}

#[test]
fn valve_price_narrows_only_when_the_wave_or_group_does_not_fit() {
    let opts = ExportOptions::default();
    assert!(valve_merges(&opts));
    let base = member_resident_bytes(100, 2);
    let merged = export_member_resident_bytes(base, &opts);
    assert_eq!(merged, base + VALVE_TRANSIENT_BYTES_PER_MEMBER);
    assert_eq!(export_member_resident_bytes(u64::MAX, &opts), u64::MAX);
    for disabled in [
        ExportOptions {
            size_valve: SizeValve::Drop,
            ..opts.clone()
        },
        ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..opts.clone()
        },
        ExportOptions {
            tile_size_limit: Some(0),
            compressed_tile_size_limit: Some(0),
            ..opts.clone()
        },
    ] {
        assert_eq!(export_member_resident_bytes(base, &disabled), base);
    }
    let partitions = [Partition {
        key_lo: 0,
        key_hi: 0,
        bbox: PruneRegion::of(TileBounds::new(0.0, 0.0, 0.0, 0.0)),
        members: 100_000,
    }];
    let cap = |bytes, budget: &MemoryBudget| {
        level_group_member_cap(8, 100_000, Some(bytes), MemoryProfile::Auto, Some(budget))
    };
    let wave = |bytes, budget: &MemoryBudget| {
        memory_safe_level_wave(8, &partitions, Some(bytes), MemoryProfile::Auto, budget)
    };
    let tight = MemoryBudget::with_available(Some(1024 * 1024 * 1024));
    assert!(cap(merged, &tight) < cap(base, &tight));
    assert!(wave(merged, &tight) < wave(base, &tight));
    let roomy = MemoryBudget::with_available(Some(123 * 1024 * 1024 * 1024));
    assert_eq!(cap(merged, &roomy), cap(base, &roomy));
    assert_eq!(wave(merged, &roomy), wave(base, &roomy));
    assert_eq!(cap(merged, &roomy), 800_000);
    assert_eq!(wave(merged, &roomy), 8);
}

#[test]
fn valve_price_keeps_preflight_threshold_and_prices_the_wave() {
    let opts = ExportOptions::default();
    assert!(valve_merges(&opts));
    let base = member_resident_bytes(100, 2);
    let merged = export_member_resident_bytes(base, &opts);
    let scans = [LevelScan {
        feature_count: 100_000,
        bounds: None,
        tile_counts: BTreeMap::from([(tile_key(0, 0, 0), 100_000)]),
    }];
    let budget = MemoryBudget::with_available(Some(1024 * 1024 * 1024));
    let plan = |limit_bytes| {
        plan_levels(
            &scans,
            &[0],
            &PartitionBudget {
                partition_target: 100_000,
                ceiling_wave: 8,
                auto_wave: true,
                member_bytes: Some(merged),
                fit_member_bytes: Some(base),
                profile: MemoryProfile::Auto,
                budget,
                fit_limit: Some(MemoryLimit {
                    bytes: limit_bytes,
                    source: MemoryLimitSource::CgroupMax,
                }),
            },
        )
    };
    let plans = plan(base * 100_000).expect("the pre-valve refusal price must still fit");
    let expected = memory_safe_level_wave(
        8,
        &plans[0].partitions,
        Some(merged),
        MemoryProfile::Auto,
        &budget,
    );
    assert_eq!(plans[0].wave, expected);
    assert!(
        expected
            < memory_safe_level_wave(
                8,
                &plans[0].partitions,
                Some(base),
                MemoryProfile::Auto,
                &budget,
            )
    );
    assert!(matches!(
        plan(base * 100_000 - 1),
        Err(ExportError::TileExceedsMemory { .. })
    ));
}

fn byte_fingerprints(mvt: &TileMvt) -> (u64, Option<u64>) {
    use xxhash_rust::xxh3::xxh3_64;
    (xxh3_64(&mvt.data), mvt.gzip.as_ref().map(|g| xxh3_64(g)))
}

#[test]
fn valve_coalescing_defers_polygon_clones_until_plain_geometry_is_needed() {
    let members = square_members(3, true);
    let ordered: Vec<_> = members.iter().collect();
    let (merged, counts) =
        valve_counter::measure(|| coalesce_valve::coalesce_members(&ordered, u64::MAX));
    assert_eq!(merged.len(), 1);
    assert_eq!(
        counts.polygon_clones, 0,
        "cached encoding needs only indices"
    );
    let (coords, counts) = valve_counter::measure(|| merged[0].geometry().coords_count());
    assert_eq!(coords, 15, "plain fallback still carries every polygon");
    assert_eq!(counts.polygon_clones, 3);
    let (_, counts) = valve_counter::measure(|| merged[0].geometry());
    assert_eq!(counts.polygon_clones, 0, "materialize at most once");
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
        "valve_work rows={rows} passes={} merged={} dropped={} features={} raw={} gzip={:?} elapsed={:?}\n{counts:#?}",
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
