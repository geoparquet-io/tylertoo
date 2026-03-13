//! Benchmarks comparing TileRef vs TileFeatureRecord memory and performance.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use gpq_tiles_core::external_sort::TileFeatureRecord;
use gpq_tiles_core::geometry_store::{GeometryHandle, GeometryStore};
use gpq_tiles_core::tile_ref::TileRef;

/// Create a realistic WKB geometry (simple polygon)
fn create_test_geometry(id: u64) -> Vec<u8> {
    // Minimal WKB for a polygon with 5 points (~100-200 bytes typical)
    let mut wkb = vec![0x01, 0x03, 0x00, 0x00, 0x00]; // Polygon header
    wkb.extend_from_slice(&1u32.to_le_bytes()); // 1 ring
    wkb.extend_from_slice(&5u32.to_le_bytes()); // 5 points

    // Add 5 coordinate pairs (x, y as f64)
    for i in 0..5 {
        let x = (id as f64 + i as f64) * 0.1;
        let y = (id as f64 + i as f64) * 0.2;
        wkb.extend_from_slice(&x.to_le_bytes());
        wkb.extend_from_slice(&y.to_le_bytes());
    }

    wkb
}

/// Create realistic MessagePack properties
fn create_test_properties(id: u64) -> Vec<u8> {
    // Minimal MessagePack map with 2 fields (~50-100 bytes typical)
    rmp_serde::to_vec(&serde_json::json!({
        "id": id,
        "name": format!("feature_{}", id),
    }))
    .expect("Should serialize")
}

/// Benchmark: Creating TileFeatureRecord vs TileRef
fn bench_creation(c: &mut Criterion) {
    let mut group = c.benchmark_group("tile_record_creation");

    let wkb = create_test_geometry(42);
    let props = create_test_properties(42);

    group.bench_function("TileFeatureRecord::new", |b| {
        b.iter(|| {
            black_box(TileFeatureRecord::new(
                black_box(12345),
                black_box(10),
                black_box(512),
                black_box(768),
                black_box(999),
                black_box(wkb.clone()),
                black_box(props.clone()),
            ))
        })
    });

    let handle = GeometryHandle {
        offset: 0,
        wkb_len: wkb.len() as u32,
        props_len: props.len() as u32,
    };

    group.bench_function("TileRef::new", |b| {
        b.iter(|| {
            black_box(TileRef::new(
                black_box(12345),
                black_box(10),
                black_box(512),
                black_box(768),
                black_box(999),
                black_box(handle),
            ))
        })
    });

    group.finish();
}

/// Benchmark: Sorting TileFeatureRecord vs TileRef
fn bench_sorting(c: &mut Criterion) {
    let mut group = c.benchmark_group("tile_record_sorting");

    for size in [1_000, 10_000, 100_000] {
        group.throughput(Throughput::Elements(size as u64));

        // Create TileFeatureRecords
        let mut tile_records: Vec<TileFeatureRecord> = (0..size)
            .map(|i| {
                TileFeatureRecord::new(
                    (i * 7) % size as u64, // Pseudo-random tile_id
                    10,
                    (i % 1024) as u32,
                    (i / 1024) as u32,
                    i as u64,
                    create_test_geometry(i as u64),
                    create_test_properties(i as u64),
                )
            })
            .collect();

        group.bench_with_input(
            BenchmarkId::new("TileFeatureRecord", size),
            &size,
            |b, _| {
                b.iter(|| {
                    let mut records = tile_records.clone();
                    records.sort();
                    black_box(records)
                })
            },
        );

        // Create TileRefs (with GeometryStore)
        let mut store = GeometryStore::new().expect("Should create store");
        let mut tile_refs: Vec<TileRef> = (0..size)
            .map(|i| {
                let wkb = create_test_geometry(i as u64);
                let props = create_test_properties(i as u64);
                let handle = store.append(&wkb, &props).expect("Should append");

                TileRef::new(
                    (i * 7) % size as u64,
                    10,
                    (i % 1024) as u32,
                    (i / 1024) as u32,
                    i as u64,
                    handle,
                )
            })
            .collect();

        group.bench_with_input(BenchmarkId::new("TileRef", size), &size, |b, _| {
            b.iter(|| {
                let mut refs = tile_refs.clone();
                refs.sort();
                black_box(refs)
            })
        });
    }

    group.finish();
}

/// Benchmark: Memory allocation patterns
fn bench_memory_allocation(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory_allocation");

    let count = 10_000;
    group.throughput(Throughput::Elements(count as u64));

    group.bench_function("allocate_TileFeatureRecords", |b| {
        b.iter(|| {
            let records: Vec<TileFeatureRecord> = (0..count)
                .map(|i| {
                    TileFeatureRecord::new(
                        i as u64,
                        10,
                        (i % 1024) as u32,
                        (i / 1024) as u32,
                        i as u64,
                        create_test_geometry(i as u64),
                        create_test_properties(i as u64),
                    )
                })
                .collect();
            black_box(records)
        })
    });

    group.bench_function("allocate_TileRefs", |b| {
        b.iter(|| {
            let mut store = GeometryStore::new().expect("Should create store");
            let refs: Vec<TileRef> = (0..count)
                .map(|i| {
                    let wkb = create_test_geometry(i as u64);
                    let props = create_test_properties(i as u64);
                    let handle = store.append(&wkb, &props).expect("Should append");

                    TileRef::new(
                        i as u64,
                        10,
                        (i % 1024) as u32,
                        (i / 1024) as u32,
                        i as u64,
                        handle,
                    )
                })
                .collect();
            black_box((store, refs))
        })
    });

    group.finish();
}

/// Benchmark: Serialization size (for external sort)
fn bench_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("serialization");

    let wkb = create_test_geometry(42);
    let props = create_test_properties(42);

    let record = TileFeatureRecord::new(12345, 10, 512, 768, 999, wkb.clone(), props.clone());

    group.bench_function("serialize_TileFeatureRecord", |b| {
        b.iter(|| {
            let bytes = bincode::serialize(&record).expect("Should serialize");
            black_box(bytes)
        })
    });

    let handle = GeometryHandle {
        offset: 0,
        wkb_len: wkb.len() as u32,
        props_len: props.len() as u32,
    };
    let tile_ref = TileRef::new(12345, 10, 512, 768, 999, handle);

    group.bench_function("serialize_TileRef", |b| {
        b.iter(|| {
            let bytes = bincode::serialize(&tile_ref).expect("Should serialize");
            black_box(bytes)
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_creation,
    bench_sorting,
    bench_memory_allocation,
    bench_serialization
);
criterion_main!(benches);
