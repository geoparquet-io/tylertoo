// GeometryStore Benchmark Suite
//
// Tests the disk-backed geometry storage for memory-bounded tile generation.
//
// Run with: cargo bench --package gpq-tiles-core --bench geometry_store

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use gpq_tiles_core::geometry_store::{GeometryHandle, GeometryStore};

/// Generate test data simulating real geometry sizes
fn generate_test_geometries(count: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    (0..count)
        .map(|i| {
            // Simulate building footprint: 200-400 bytes WKB
            let wkb_size = 200 + (i % 200);
            let wkb = vec![(i % 256) as u8; wkb_size];

            // Simulate properties: 50-150 bytes MessagePack
            let props_size = 50 + (i % 100);
            let props = vec![((i + 100) % 256) as u8; props_size];

            (wkb, props)
        })
        .collect()
}

/// Benchmark append throughput
fn bench_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("geometry_store_append");

    for count in [1_000, 10_000, 100_000] {
        let geometries = generate_test_geometries(count);
        let total_bytes: usize = geometries.iter().map(|(w, p)| w.len() + p.len()).sum();

        group.throughput(Throughput::Bytes(total_bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("geometries", count),
            &geometries,
            |b, geometries| {
                b.iter(|| {
                    let mut store = GeometryStore::new().expect("Should create store");
                    for (wkb, props) in geometries {
                        black_box(store.append(wkb, props).expect("Should append"));
                    }
                    store.flush().expect("Should flush");
                    black_box(store)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark read throughput (sequential pattern - typical for Phase 3)
fn bench_read_sequential(c: &mut Criterion) {
    let mut group = c.benchmark_group("geometry_store_read_sequential");

    for count in [1_000, 10_000, 100_000] {
        let geometries = generate_test_geometries(count);
        let total_bytes: usize = geometries.iter().map(|(w, p)| w.len() + p.len()).sum();

        // Pre-populate store
        let mut store = GeometryStore::new().expect("Should create store");
        let handles: Vec<GeometryHandle> = geometries
            .iter()
            .map(|(wkb, props)| store.append(wkb, props).expect("Should append"))
            .collect();
        store.flush().expect("Should flush");

        group.throughput(Throughput::Bytes(total_bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("geometries", count),
            &handles,
            |b, handles| {
                b.iter(|| {
                    // Clone the store to reset read position each iteration
                    let mut store = GeometryStore::new().expect("Should create store");
                    for (wkb, props) in &geometries {
                        store.append(wkb, props).expect("Should append");
                    }
                    store.flush().expect("Should flush");

                    for handle in handles {
                        black_box(store.read(*handle).expect("Should read"));
                    }
                })
            },
        );
    }

    group.finish();
}

/// Benchmark read throughput (random access pattern - simulating tile order)
fn bench_read_random(c: &mut Criterion) {
    let mut group = c.benchmark_group("geometry_store_read_random");
    group.sample_size(30); // Fewer samples for expensive benchmarks

    for count in [1_000, 10_000] {
        let geometries = generate_test_geometries(count);

        // Pre-populate store
        let mut store = GeometryStore::new().expect("Should create store");
        let mut handles: Vec<GeometryHandle> = geometries
            .iter()
            .map(|(wkb, props)| store.append(wkb, props).expect("Should append"))
            .collect();
        store.flush().expect("Should flush");

        // Shuffle handles to simulate random access (tile-sorted != feature-sorted)
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        handles.sort_by_key(|h| {
            let mut hasher = DefaultHasher::new();
            h.offset.hash(&mut hasher);
            hasher.finish()
        });

        let total_bytes: usize = geometries.iter().map(|(w, p)| w.len() + p.len()).sum();

        group.throughput(Throughput::Bytes(total_bytes as u64));
        group.bench_with_input(
            BenchmarkId::new("geometries", count),
            &(geometries.clone(), handles),
            |b, (geometries, handles)| {
                b.iter(|| {
                    let mut store = GeometryStore::new().expect("Should create store");
                    for (wkb, props) in geometries {
                        store.append(wkb, props).expect("Should append");
                    }
                    store.flush().expect("Should flush");

                    for handle in handles {
                        black_box(store.read(*handle).expect("Should read"));
                    }
                })
            },
        );
    }

    group.finish();
}

/// Benchmark tile replication pattern: each geometry read multiple times
fn bench_tile_replication(c: &mut Criterion) {
    let mut group = c.benchmark_group("geometry_store_tile_replication");
    group.sample_size(20); // Expensive benchmark

    // 1000 features, each appearing in 30 tiles (typical)
    let geometries = generate_test_geometries(1_000);

    let mut store = GeometryStore::new().expect("Should create store");
    let handles: Vec<GeometryHandle> = geometries
        .iter()
        .map(|(wkb, props)| store.append(wkb, props).expect("Should append"))
        .collect();
    store.flush().expect("Should flush");

    let reads_per_geometry = 30;
    let total_reads = handles.len() * reads_per_geometry;

    group.throughput(Throughput::Elements(total_reads as u64));

    group.bench_function("1k_features_30x_replication", |b| {
        b.iter(|| {
            let mut store = GeometryStore::new().expect("Should create store");
            for (wkb, props) in &geometries {
                store.append(wkb, props).expect("Should append");
            }
            store.flush().expect("Should flush");

            // Each handle read 30 times
            for _ in 0..reads_per_geometry {
                for handle in &handles {
                    black_box(store.read(*handle).expect("Should read"));
                }
            }
        })
    });

    group.finish();
}

criterion_group!(
    name = fast_benchmarks;
    config = Criterion::default().sample_size(50);
    targets = bench_append
);

criterion_group!(
    name = medium_benchmarks;
    config = Criterion::default().sample_size(30);
    targets = bench_read_sequential, bench_read_random
);

criterion_group!(
    name = slow_benchmarks;
    config = Criterion::default().sample_size(20);
    targets = bench_tile_replication
);

criterion_main!(fast_benchmarks, medium_benchmarks, slow_benchmarks);
