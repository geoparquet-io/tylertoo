use super::*;
use crate::compression::gzip_counter;
use geo::Point;

fn point_member(x: u32, seq: u64) -> Member {
    Member {
        key: tile_key(x, 0, 1),
        seq,
        geom: Geometry::Point(Point::new(-90.0 + x as f64 * 180.0, 40.0)),
        props: Arc::new(Vec::new()),
        id: None,
        gap: GAP_NONE,
        band: 0,
    }
}

fn encode(
    gzip_cache: &Arc<TileGzipCache>,
    members: Vec<Member>,
    opts: &ExportOptions,
) -> Vec<EncodedTile> {
    encode_members(
        gzip_cache,
        members,
        1,
        opts,
        None,
        &ExportTimers::default(),
        &crate::diagnostics::Diagnostics::default(),
    )
    .unwrap()
}

#[test]
fn first_pass_tile_gzipped_at_most_once() {
    for valve in [SizeValve::Merge, SizeValve::Drop] {
        let opts = ExportOptions {
            size_valve: valve,
            ..Default::default()
        };
        let (tiles, calls) =
            gzip_counter::measure(|| encode(&test_gzip_cache(), vec![point_member(0, 0)], &opts));
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].valve.passes, 1);
        assert!(calls <= 1, "first-pass tile: {calls} gzip calls");
    }
}

// Keep the writer within the observation scope so moving compression into
// its serial path cannot make the counter accidentally pass.
fn write_tiles(tiles: &[EncodedTile]) -> (u64, u64) {
    let mut writer = crate::pmtiles_writer::StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
    for t in tiles {
        writer
            .add_tile_precompressed(
                TileCoord::new(t.x, t.y, 1),
                t.hash,
                &t.data,
                t.raw_len,
                t.feature_count,
            )
            .unwrap();
    }
    (writer.stats().total_tiles, writer.stats().unique_tiles)
}

#[test]
fn duplicate_tile_is_never_gzipped_twice_same_wave() {
    let opts = ExportOptions::default();
    let ((tiles, stats), calls) = gzip_counter::measure(|| {
        let tiles = encode(
            &test_gzip_cache(),
            vec![point_member(0, 0), point_member(1, 1)],
            &opts,
        );
        let stats = write_tiles(&tiles);
        (tiles, stats)
    });
    assert_eq!(tiles.len(), 2);
    assert_eq!(tiles[0].data, tiles[1].data);
    assert_eq!(stats, (2, 1));
    assert!(
        calls <= 1,
        "one unique body, one duplicate: {calls} gzip calls"
    );
}

#[test]
fn duplicate_tile_is_never_gzipped_twice_across_waves() {
    let gzip_cache = test_gzip_cache();
    let opts = ExportOptions::default();
    let ((tiles, stats), calls) = gzip_counter::measure(|| {
        let mut tiles = encode(&gzip_cache, vec![point_member(0, 0)], &opts);
        tiles.extend(encode(&gzip_cache, vec![point_member(1, 1)], &opts));
        let stats = write_tiles(&tiles);
        (tiles, stats)
    });
    assert_eq!(tiles[0].data, tiles[1].data);
    assert_eq!(stats, (2, 1));
    assert!(calls <= 1, "duplicate across waves: {calls} gzip calls");
}

/// #711: a capped tile must not first materialize an unbounded protobuf layer.
/// Inputs belong to the collector and are allocated before profiling: this
/// budget covers only transient allocation inside the per-tile encode owner.
/// dhat's heap stats are process-global, so the budget is asserted only under
/// nextest's process-per-test mode; plain `cargo test` runs other tests as
/// threads in this process, their allocations land in the peak, and it skips.
#[cfg(feature = "dhat-heap")]
#[test]
fn dense_capped_tile_encode_peak_live_bytes_is_bounded() {
    if std::env::var_os("NEXTEST_EXECUTION_MODE").as_deref()
        != Some(std::ffi::OsStr::new("process-per-test"))
    {
        eprintln!("skipped: the dhat peak is only valid with one test per process (nextest)");
        return;
    }
    const ROWS: usize = 100_000;
    const RAW_CAP: usize = 64 * 1024;
    const ENCODE_BUDGET: usize = 16 * 1024 * 1024;
    let keys: Vec<Arc<str>> = ["id", "confidence", "basic_category", "operating_status"]
        .into_iter()
        .map(Arc::from)
        .collect();
    let members: Vec<_> = (0..ROWS)
        .map(|i| {
            let mut m = point_member(0, i as u64);
            m.props = Arc::new(vec![
                (
                    Arc::clone(&keys[0]),
                    PropertyValue::String(format!("place-{i:016x}")),
                ),
                (
                    Arc::clone(&keys[1]),
                    PropertyValue::Double((i % 100) as f64 / 100.0),
                ),
                (
                    Arc::clone(&keys[2]),
                    PropertyValue::String("shop".to_owned()),
                ),
                (
                    Arc::clone(&keys[3]),
                    PropertyValue::String("open".to_owned()),
                ),
            ]);
            m
        })
        .collect();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    let (peak, bytes, features) = pool.install(|| {
        let profiler = dhat::Profiler::builder().testing().build();
        let opts = ExportOptions {
            tile_size_limit: Some(RAW_CAP),
            compressed_tile_size_limit: None,
            size_valve: SizeValve::Drop,
            ..Default::default()
        };
        let mvt = encode_tile(
            &test_gzip_cache(),
            &members,
            &TileCoord::new(0, 0, 1).bounds(),
            &opts,
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        );
        let measured = (
            dhat::HeapStats::get().max_bytes,
            mvt.data.len(),
            mvt.features,
        );
        drop(mvt);
        drop(profiler);
        measured
    });
    eprintln!(
        "dense capped tile encode peak: {peak} bytes; output {bytes} bytes; features {features}"
    );
    assert!(features > 0 && features < ROWS);
    assert!(bytes <= RAW_CAP);
    assert!(
        peak <= ENCODE_BUDGET,
        "dense capped tile encode peak: {peak} bytes, budget {ENCODE_BUDGET}; output {bytes} bytes"
    );
}
/// Exceeds the memo's admission size, so a memo hit cannot hide a fit recheck.
fn fit_payload() -> TileMvt {
    TileMvt {
        source_ids: Vec::new(),
        source_losses: Vec::new(),
        gzip_cache: test_gzip_cache(),
        data: (0..8192)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>()
            .into(),
        features: 1,
        unencodable: 0,
        quantized: 0,
        oversized: true,
        valve: ValveTally::default(),
    }
}

#[test]
fn tile_fit_reuses_only_unchanged_bytes_and_rechecks_active_limits() {
    let mut mvt = fit_payload();
    let opts = ExportOptions {
        tile_size_limit: Some(16_384),
        compressed_tile_size_limit: Some(1024),
        ..Default::default()
    };
    let (first, initial_calls) = gzip_counter::measure(|| tile_fit(&mut mvt, &opts).unwrap());
    assert_eq!(initial_calls, 1);
    let ((again, tighter, raw_only), repeat_calls) = gzip_counter::measure(|| {
        let again = tile_fit(&mut mvt, &opts).unwrap();
        let tighter = tile_fit(
            &mut mvt,
            &ExportOptions {
                tile_size_limit: Some(4096),
                compressed_tile_size_limit: Some(1),
                ..opts.clone()
            },
        )
        .unwrap();
        let raw_only = tile_fit(
            &mut mvt,
            &ExportOptions {
                tile_size_limit: Some(4096),
                compressed_tile_size_limit: None,
                ..opts.clone()
            },
        )
        .unwrap();
        (again, tighter, raw_only)
    });
    assert_eq!(
        repeat_calls, 0,
        "unchanged tile bytes never need another gzip"
    );
    assert_eq!(first, again);
    assert!(
        tighter < 0.5,
        "the current compressed limit must still apply"
    );
    assert_eq!(raw_only, 0.5, "the current raw limit must still apply");

    mvt.data[0] ^= 1;
    assert!(
        mvt.gzip.is_none(),
        "an in-place edit invalidates gzip immediately"
    );
    let (_, mutation_calls) = gzip_counter::measure(|| tile_fit(&mut mvt, &opts).unwrap());
    assert_eq!(mutation_calls, 1, "changed bytes must be compressed");
    let decoded = compression::decompress(mvt.gzip.as_ref().unwrap(), Compression::Gzip).unwrap();
    assert_eq!(decoded.as_slice(), mvt.data.as_slice());

    let (_, missing_calls) = gzip_counter::measure(|| tile_fit(&mut fit_payload(), &opts).unwrap());
    assert_eq!(
        missing_calls, 1,
        "a missing cache uses the normal compression path"
    );
}
