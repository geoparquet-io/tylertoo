//! Tile gzip + dedup benchmark (#448): the two per-tile steps
//! `overview::export` runs on every encoded MVT payload before it is
//! written to the PMTiles archive — content hashing for run-length dedup
//! ([`TileHasher`] / [`DeduplicationCache`]) and gzip compression
//! ([`compression::compress`]).
//!
//! Run with: cargo bench --package tylertoo-core --bench tile_compress_dedup

use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use geo::{Coord, Geometry, LineString, Point};
use prost::Message;
use tylertoo_core::compression::{compress, Compression};
use tylertoo_core::dedup::{DeduplicationCache, TileHasher};
use tylertoo_core::mvt::{LayerBuilder, PropertyValue};
use tylertoo_core::tile::TileBounds;

/// Build a realistic encoded MVT tile of roughly `feature_count` mixed
/// point/line features with the small repeated-tag property shape real
/// layers have (see `mvt_encode.rs`'s `layer_value_dedup` bench).
fn build_tile_bytes(feature_count: usize) -> Vec<u8> {
    let bounds = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
    let mut builder = LayerBuilder::new("layer").with_extent(4096);
    const CATEGORIES: &[&str] = &["a", "b", "c", "d"];
    for i in 0..feature_count {
        let geom = if i % 3 == 0 {
            let x = (i % 100) as f64 / 100.0 - 0.5;
            let y = (i / 100) as f64 / 100.0 - 0.5;
            Geometry::LineString(LineString::new(vec![
                Coord { x, y },
                Coord {
                    x: x + 0.01,
                    y: y + 0.01,
                },
                Coord {
                    x: x + 0.02,
                    y: y - 0.005,
                },
            ]))
        } else {
            Geometry::Point(Point::new(
                (i % 100) as f64 / 100.0 - 0.5,
                (i / 100) as f64 / 100.0 - 0.5,
            ))
        };
        let props = vec![
            (
                "category".to_string(),
                PropertyValue::String(CATEGORIES[i % CATEGORIES.len()].to_string()),
            ),
            ("rank".to_string(), PropertyValue::UInt((i % 10) as u64)),
        ];
        builder.add_feature(Some(i as u64), &geom, &props, &bounds);
    }
    let layer = builder.build();
    let mut tb = tylertoo_core::mvt::TileBuilder::new();
    tb.add_layer(layer);
    tb.build().encode_to_vec()
}

fn bench_gzip(c: &mut Criterion) {
    let mut group = c.benchmark_group("tile_gzip");
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(30);

    for feature_count in [50usize, 500, 2_000] {
        let bytes = build_tile_bytes(feature_count);
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(bytes.len()),
            &bytes,
            |b, bytes| {
                b.iter(|| black_box(compress(black_box(bytes), Compression::Gzip).unwrap()));
            },
        );
    }

    group.finish();
}

/// A stream of tile byte buffers where roughly 1 in 4 repeats an earlier
/// one — the shape real archives have (blank ocean/desert tiles recur).
fn dedup_stream(n: usize) -> Vec<Vec<u8>> {
    let variants: Vec<Vec<u8>> = vec![
        build_tile_bytes(0),
        build_tile_bytes(10),
        build_tile_bytes(50),
        build_tile_bytes(200),
    ];
    (0..n)
        .map(|i| {
            if i % 4 == 0 {
                variants[(i / 4) % variants.len()].clone()
            } else {
                build_tile_bytes(i % 30 + 1)
            }
        })
        .collect()
}

fn bench_dedup(c: &mut Criterion) {
    let mut group = c.benchmark_group("tile_dedup");
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(30);

    for n in [500usize, 2_000] {
        let stream = dedup_stream(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &stream, |b, stream| {
            b.iter(|| {
                let mut cache = DeduplicationCache::new();
                let mut offset = 0u64;
                for tile in stream {
                    let hash = TileHasher::hash(tile);
                    match cache.check(hash) {
                        Some(_) => cache.record_duplicate(tile.len() as u32),
                        None => {
                            cache.record_new(hash, offset, tile.len() as u32, tile.len() as u32);
                            offset += tile.len() as u64;
                        }
                    }
                }
                black_box(cache.into_stats())
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_gzip, bench_dedup);
criterion_main!(benches);
