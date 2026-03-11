// Production Pipeline Benchmark Suite
//
// Tests the production code path: geometry-centric processing with external sort.
// This is the only code path after the v0.4.0 consolidation.
//
// Run with: cargo bench --package gpq-tiles-core --bench streaming
//
// Fixtures:
// - open-buildings.parquet (1K features) - quick validation
// - multi-rowgroup-small.parquet - multi-row-group processing
// - fieldmaps-madagascar-adm4.parquet (17K features) - production scale

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use gpq_tiles_core::batch_processor::{
    process_geometries_by_row_group, process_geometries_parallel, DEFAULT_PARALLEL_READERS,
};
use gpq_tiles_core::compression::Compression;
use gpq_tiles_core::pipeline::{generate_tiles_to_writer, TilerConfig};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
use std::path::Path;

// Fixture paths (relative to crates/core/)
const FIXTURE_SMALL: &str = "../../tests/fixtures/realdata/open-buildings.parquet";
const FIXTURE_MULTI_RG: &str = "../../tests/fixtures/streaming/multi-rowgroup-small.parquet";
const FIXTURE_LARGE: &str = "../../tests/fixtures/realdata/fieldmaps-madagascar-adm4.parquet";

/// Helper to check if a fixture exists
fn fixture_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Benchmark production pipeline on small fixture
fn bench_small_fixture(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_SMALL) {
        eprintln!("Skipping small fixture benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_SMALL);

    let mut group = c.benchmark_group("production_small");
    group.throughput(Throughput::Elements(1000)); // ~1K features

    for max_zoom in [6, 8, 10] {
        let config = TilerConfig::new(0, max_zoom).with_quiet(true);
        group.bench_with_input(
            BenchmarkId::new("max_zoom", max_zoom),
            &config,
            |b, config| {
                b.iter(|| {
                    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)
                        .expect("Should create writer");
                    let stats = generate_tiles_to_writer(fixture_path, config, &mut writer)
                        .expect("Pipeline should work");
                    black_box(stats)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark production pipeline on multi-row-group fixture
fn bench_multi_rowgroup(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_MULTI_RG) {
        eprintln!("Skipping multi-rowgroup benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_MULTI_RG);

    let mut group = c.benchmark_group("production_multi_rg");
    group.sample_size(30);

    for max_zoom in [6, 8, 10] {
        let config = TilerConfig::new(0, max_zoom).with_quiet(true);
        group.bench_with_input(
            BenchmarkId::new("max_zoom", max_zoom),
            &config,
            |b, config| {
                b.iter(|| {
                    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)
                        .expect("Should create writer");
                    let stats = generate_tiles_to_writer(fixture_path, config, &mut writer)
                        .expect("Pipeline should work");
                    black_box(stats)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark production pipeline on large fixture (17K features)
/// This is the main production-scale benchmark.
fn bench_large_fixture(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_LARGE) {
        eprintln!("Skipping large fixture benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_LARGE);

    let mut group = c.benchmark_group("production_large");
    group.throughput(Throughput::Elements(17465)); // ~17K features
    group.sample_size(10);

    for max_zoom in [8, 10, 12] {
        let config = TilerConfig::new(0, max_zoom).with_quiet(true);
        group.bench_with_input(
            BenchmarkId::new("max_zoom", max_zoom),
            &config,
            |b, config| {
                b.iter(|| {
                    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)
                        .expect("Should create writer");
                    let stats = generate_tiles_to_writer(fixture_path, config, &mut writer)
                        .expect("Pipeline should work");
                    black_box(stats)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark with different memory budgets
fn bench_memory_budgets(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_LARGE) {
        eprintln!("Skipping memory budget benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_LARGE);

    let mut group = c.benchmark_group("memory_budgets");
    group.sample_size(10);

    let base_config = TilerConfig::new(0, 10).with_quiet(true);

    // Test different memory budgets
    for budget_mb in [100, 256, 512] {
        let config = base_config
            .clone()
            .with_memory_budget(budget_mb * 1024 * 1024);
        group.bench_with_input(
            BenchmarkId::new("budget_mb", budget_mb),
            &config,
            |b, config| {
                b.iter(|| {
                    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)
                        .expect("Should create writer");
                    let stats = generate_tiles_to_writer(fixture_path, config, &mut writer)
                        .expect("Pipeline should work");
                    black_box(stats)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark just the row-group reading: sequential vs parallel.
///
/// Compares the performance of:
/// - Sequential: process_geometries_by_row_group (original)
/// - Parallel: process_geometries_parallel (new, #108 + #109)
///
/// This isolates the parquet reading from tile generation to measure
/// the raw I/O and decompression throughput improvement.
fn bench_rowgroup_reading(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_MULTI_RG) {
        eprintln!("Skipping row-group reading benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_MULTI_RG);

    let mut group = c.benchmark_group("rowgroup_reading");
    group.sample_size(30);

    // Sequential baseline (original implementation)
    group.bench_function("sequential", |b| {
        b.iter(|| {
            let mut total = 0usize;
            process_geometries_by_row_group(fixture_path, |_info, geoms| {
                total += geoms.len();
                Ok(())
            })
            .expect("Should read all row groups");
            black_box(total)
        })
    });

    // Parallel implementation (Issues #108 + #109)
    group.bench_function("parallel_4_readers", |b| {
        b.iter(|| {
            let total = std::sync::atomic::AtomicUsize::new(0);
            process_geometries_parallel(fixture_path, DEFAULT_PARALLEL_READERS, |_info, geoms| {
                total.fetch_add(geoms.len(), std::sync::atomic::Ordering::Relaxed);
                Ok(())
            })
            .expect("Should read all row groups");
            black_box(total.load(std::sync::atomic::Ordering::Relaxed))
        })
    });

    // Parallel with different concurrency levels
    for num_readers in [1, 2, 8] {
        group.bench_function(format!("parallel_{}_readers", num_readers), |b| {
            b.iter(|| {
                let total = std::sync::atomic::AtomicUsize::new(0);
                process_geometries_parallel(fixture_path, num_readers, |_info, geoms| {
                    total.fetch_add(geoms.len(), std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                })
                .expect("Should read all row groups");
                black_box(total.load(std::sync::atomic::Ordering::Relaxed))
            })
        });
    }

    group.finish();
}

/// Benchmark tiny polygon accumulation vs dropping (Issue #85)
///
/// This benchmark compares performance with and without tiny polygon accumulation
/// to verify there's no significant performance regression from the feature.
fn bench_tiny_polygon_accumulation(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_SMALL) {
        eprintln!("Skipping tiny polygon accumulation benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_SMALL);

    let mut group = c.benchmark_group("tiny_polygon_handling");
    group.throughput(Throughput::Elements(1000)); // ~1K features
    group.sample_size(30);

    // Benchmark with accumulation ENABLED (default, matches tippecanoe)
    let config_accumulation = TilerConfig::new(0, 8)
        .with_quiet(true)
        .with_tiny_polygon_accumulation(true);

    group.bench_with_input(
        BenchmarkId::new("mode", "accumulation"),
        &config_accumulation,
        |b, config| {
            b.iter(|| {
                let mut writer =
                    StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
                let stats = generate_tiles_to_writer(fixture_path, config, &mut writer)
                    .expect("Pipeline should work");
                black_box(stats)
            })
        },
    );

    // Benchmark with accumulation DISABLED (legacy dropping behavior)
    let config_dropping = TilerConfig::new(0, 8)
        .with_quiet(true)
        .with_tiny_polygon_accumulation(false);

    group.bench_with_input(
        BenchmarkId::new("mode", "dropping"),
        &config_dropping,
        |b, config| {
            b.iter(|| {
                let mut writer =
                    StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
                let stats = generate_tiles_to_writer(fixture_path, config, &mut writer)
                    .expect("Pipeline should work");
                black_box(stats)
            })
        },
    );

    group.finish();
}

criterion_group!(
    name = fast_benchmarks;
    config = Criterion::default().sample_size(50);
    targets = bench_small_fixture
);

criterion_group!(
    name = medium_benchmarks;
    config = Criterion::default().sample_size(30);
    targets = bench_multi_rowgroup, bench_rowgroup_reading, bench_tiny_polygon_accumulation
);

criterion_group!(
    name = slow_benchmarks;
    config = Criterion::default().sample_size(10);
    targets = bench_large_fixture, bench_memory_budgets
);

criterion_main!(fast_benchmarks, medium_benchmarks, slow_benchmarks);
