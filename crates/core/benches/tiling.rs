// Benchmark suite for tiling performance
//
// Uses real-world GeoParquet fixtures for realistic benchmarks.
//
// Run with: cargo bench --package gpq-tiles-core
//
// Fixtures:
// - open-buildings.parquet (1K features) - quick tests
// - fieldmaps-madagascar-adm4.parquet (17K features) - production-scale benchmarks

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use gpq_tiles_core::batch_processor::extract_geometries;
use gpq_tiles_core::compression::Compression;
use gpq_tiles_core::pipeline::{generate_single_tile, generate_tiles_to_writer, TilerConfig};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
use gpq_tiles_core::tile::TileCoord;
use std::path::Path;

// Path to fixtures (relative to crates/core/)
const FIXTURE_SMALL: &str = "../../tests/fixtures/realdata/open-buildings.parquet";
const FIXTURE_LARGE: &str = "../../tests/fixtures/realdata/fieldmaps-madagascar-adm4.parquet";

/// Load geometries from the small fixture (1K features)
fn load_small_fixture() -> Vec<geo::Geometry<f64>> {
    load_fixture(FIXTURE_SMALL)
}

/// Load geometries from the large fixture (17K features)
#[allow(dead_code)] // Reserved for production-scale benchmarks
fn load_large_fixture() -> Vec<geo::Geometry<f64>> {
    load_fixture(FIXTURE_LARGE)
}

fn load_fixture(fixture_path: &str) -> Vec<geo::Geometry<f64>> {
    let path = Path::new(fixture_path);
    if !path.exists() {
        panic!(
            "Fixture file not found at {}. Run `git lfs pull` if using LFS fixtures.",
            fixture_path
        );
    }
    extract_geometries(path).expect("Failed to load fixture geometries")
}

/// Check if a fixture file exists
fn fixture_exists(fixture_path: &str) -> bool {
    Path::new(fixture_path).exists()
}

/// Benchmark single tile generation at various zoom levels
/// This tests the core tile encoding logic in isolation.
fn bench_single_tile(c: &mut Criterion) {
    let geometries = load_small_fixture();
    let config = TilerConfig::new(0, 14);

    let mut group = c.benchmark_group("single_tile");
    group.throughput(Throughput::Elements(geometries.len() as u64));

    // Benchmark at different zoom levels
    // Z10/516/377 is the main tile covering our fixture
    for (z, x, y) in [(8, 129, 94), (10, 516, 377)] {
        let coord = TileCoord::new(x, y, z);
        group.bench_with_input(BenchmarkId::new("z", z), &coord, |b, coord| {
            b.iter(|| {
                let result = generate_single_tile(&geometries, *coord, &config);
                black_box(result)
            })
        });
    }

    group.finish();
}

/// Benchmark full production pipeline at various zoom ranges
/// This tests the actual code path used in production (geometry-centric, parallel).
fn bench_full_pipeline(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_SMALL) {
        eprintln!("Skipping full_pipeline benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_SMALL);

    let mut group = c.benchmark_group("full_pipeline");
    // Throughput is measured in features (not tiles, since tile count varies by zoom)
    group.throughput(Throughput::Elements(1000)); // ~1K features in small fixture

    // Benchmark different zoom ranges
    for max_zoom in [8, 10] {
        let config = TilerConfig::new(0, max_zoom).with_quiet(true);
        group.bench_with_input(
            BenchmarkId::new("max_zoom", max_zoom),
            &config,
            |b, config| {
                b.iter(|| {
                    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)
                        .expect("Should create writer");
                    let stats = generate_tiles_to_writer(fixture_path, config, &mut writer)
                        .expect("generate_tiles failed");
                    // Don't finalize - we just want to measure tile generation
                    black_box(stats)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark with density dropping enabled vs disabled
fn bench_density_dropping(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_SMALL) {
        eprintln!("Skipping density_dropping benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_SMALL);

    let mut group = c.benchmark_group("density_dropping");
    group.throughput(Throughput::Elements(1000));

    // Without density dropping (default)
    let config_no_drop = TilerConfig::new(0, 10).with_quiet(true);
    group.bench_function("no_density_drop", |b| {
        b.iter(|| {
            let mut writer =
                StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
            let stats = generate_tiles_to_writer(fixture_path, &config_no_drop, &mut writer)
                .expect("generate_tiles failed");
            black_box(stats)
        })
    });

    // With density dropping
    let config_with_drop = TilerConfig::new(0, 10)
        .with_density_drop(true)
        .with_density_cell_size(32)
        .with_quiet(true);
    group.bench_function("with_density_drop", |b| {
        b.iter(|| {
            let mut writer =
                StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
            let stats = generate_tiles_to_writer(fixture_path, &config_with_drop, &mut writer)
                .expect("generate_tiles failed");
            black_box(stats)
        })
    });

    group.finish();
}

/// Benchmark Hilbert vs Z-order sorting
fn bench_hilbert_vs_zorder(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_SMALL) {
        eprintln!("Skipping hilbert_vs_zorder benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_SMALL);

    let mut group = c.benchmark_group("hilbert_vs_zorder");
    group.throughput(Throughput::Elements(1000));

    // Hilbert (default)
    let config_hilbert = TilerConfig::new(0, 10).with_hilbert(true).with_quiet(true);
    group.bench_function("hilbert", |b| {
        b.iter(|| {
            let mut writer =
                StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
            let stats = generate_tiles_to_writer(fixture_path, &config_hilbert, &mut writer)
                .expect("generate_tiles failed");
            black_box(stats)
        })
    });

    // Z-order
    let config_zorder = TilerConfig::new(0, 10).with_hilbert(false).with_quiet(true);
    group.bench_function("zorder", |b| {
        b.iter(|| {
            let mut writer =
                StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
            let stats = generate_tiles_to_writer(fixture_path, &config_zorder, &mut writer)
                .expect("generate_tiles failed");
            black_box(stats)
        })
    });

    group.finish();
}

/// Benchmark parallel vs sequential (deterministic) encoding
/// This measures the impact of parallel tile encoding.
fn bench_parallel_vs_sequential(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_SMALL) {
        eprintln!("Skipping parallel_vs_sequential benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_SMALL);

    let mut group = c.benchmark_group("parallel_vs_sequential");
    group.throughput(Throughput::Elements(1000));

    // Parallel (default) - uses rayon for tile encoding
    let config_parallel = TilerConfig::new(0, 10).with_quiet(true);
    group.bench_function("parallel", |b| {
        b.iter(|| {
            let mut writer =
                StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
            let stats = generate_tiles_to_writer(fixture_path, &config_parallel, &mut writer)
                .expect("generate_tiles failed");
            black_box(stats)
        })
    });

    // Sequential (deterministic) - single-threaded for reproducibility
    let config_sequential = TilerConfig::new(0, 10)
        .with_quiet(true)
        .with_deterministic(true);
    group.bench_function("sequential", |b| {
        b.iter(|| {
            let mut writer =
                StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
            let stats = generate_tiles_to_writer(fixture_path, &config_sequential, &mut writer)
                .expect("generate_tiles failed");
            black_box(stats)
        })
    });

    group.finish();
}

/// Benchmark parallel vs sequential on LARGE fixture for more significant results
fn bench_large_parallel_vs_sequential(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_LARGE) {
        eprintln!("Skipping large_parallel_vs_sequential benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_LARGE);

    let mut group = c.benchmark_group("large_parallel_vs_sequential");
    group.throughput(Throughput::Elements(17465));
    group.sample_size(10);

    // Parallel (default)
    let config_parallel = TilerConfig::new(0, 10).with_quiet(true);
    group.bench_function("parallel", |b| {
        b.iter(|| {
            let mut writer =
                StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
            let stats = generate_tiles_to_writer(fixture_path, &config_parallel, &mut writer)
                .expect("generate_tiles failed");
            black_box(stats)
        })
    });

    // Sequential (deterministic)
    let config_sequential = TilerConfig::new(0, 10)
        .with_quiet(true)
        .with_deterministic(true);
    group.bench_function("sequential", |b| {
        b.iter(|| {
            let mut writer =
                StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
            let stats = generate_tiles_to_writer(fixture_path, &config_sequential, &mut writer)
                .expect("generate_tiles failed");
            black_box(stats)
        })
    });

    group.finish();
}

/// Benchmark production pipeline on LARGE fixture (17K features)
/// This is the main benchmark for measuring real-world performance.
fn bench_large_full_pipeline(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_LARGE) {
        eprintln!("Skipping large_full_pipeline benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_LARGE);

    let mut group = c.benchmark_group("large_full_pipeline");
    group.throughput(Throughput::Elements(17465)); // ~17K features in large fixture
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
                        .expect("generate_tiles failed");
                    black_box(stats)
                })
            },
        );
    }

    group.finish();
}

/// Benchmark compression algorithms on the large fixture
fn bench_compression(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_LARGE) {
        eprintln!("Skipping compression benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_LARGE);

    let mut group = c.benchmark_group("compression");
    group.throughput(Throughput::Elements(17465));
    group.sample_size(10);

    let config = TilerConfig::new(0, 10).with_quiet(true);

    for compression in [Compression::None, Compression::Gzip, Compression::Zstd] {
        let name = match compression {
            Compression::None => "none",
            Compression::Gzip => "gzip",
            Compression::Zstd => "zstd",
            Compression::Brotli => "brotli",
            _ => "unknown",
        };
        group.bench_function(name, |b| {
            b.iter(|| {
                let mut writer =
                    StreamingPmtilesWriter::new(compression).expect("Should create writer");
                let stats = generate_tiles_to_writer(fixture_path, &config, &mut writer)
                    .expect("generate_tiles failed");
                black_box(stats)
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_single_tile,
    bench_full_pipeline,
    bench_parallel_vs_sequential,
    bench_density_dropping,
    bench_hilbert_vs_zorder,
);

criterion_group!(
    name = large_benches;
    config = Criterion::default().sample_size(10);
    targets = bench_large_full_pipeline, bench_large_parallel_vs_sequential, bench_compression
);

criterion_main!(benches, large_benches);
