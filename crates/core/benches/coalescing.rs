// Coalescing Benchmark Suite
//
// Compares tile generation with and without coalescing to measure:
// - Output size reduction
// - Processing time overhead
//
// Run with: cargo bench --package gpq-tiles-core --bench coalescing
//
// Fixtures:
// - open-buildings.parquet (1K features) - dense points, ideal for coalescing
// - fieldmaps-madagascar-adm4.parquet (17K features) - polygons, moderate density

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use gpq_tiles_core::compression::Compression;
use gpq_tiles_core::pipeline::{generate_tiles_to_writer, TilerConfig};
use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
use std::path::Path;

// Fixture paths (relative to crates/core/)
const FIXTURE_BUILDINGS: &str = "../../tests/fixtures/realdata/open-buildings.parquet";
const FIXTURE_POLYGONS: &str = "../../tests/fixtures/realdata/fieldmaps-madagascar-adm4.parquet";

/// Helper to check if a fixture exists
fn fixture_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Run pipeline and return (output_size, peak_memory)
fn run_pipeline(path: &Path, config: &TilerConfig) -> (usize, usize) {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    let output_path = std::path::PathBuf::from(format!("/tmp/bench-coalesce-{}.pmtiles", id));

    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
    let stats = generate_tiles_to_writer(path, config, &mut writer).expect("Pipeline should work");
    writer.finalize(&output_path).expect("Should finalize");

    let size = fs::metadata(&output_path)
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    let _ = fs::remove_file(&output_path);

    (size, stats.peak_bytes)
}

/// Benchmark coalescing on open-buildings (dense points)
fn bench_coalescing_buildings(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_BUILDINGS) {
        eprintln!("Skipping buildings benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_BUILDINGS);

    let mut group = c.benchmark_group("coalescing_buildings");
    group.sample_size(20);

    // Test at different zoom ranges
    for max_zoom in [8, 10] {
        // Baseline (no coalescing)
        let config_baseline = TilerConfig::new(0, max_zoom).with_quiet(true);
        group.bench_with_input(
            BenchmarkId::new("baseline", max_zoom),
            &config_baseline,
            |b, config| {
                b.iter(|| {
                    let result = run_pipeline(fixture_path, config);
                    black_box(result)
                })
            },
        );

        // With coalescing
        let config_coalesce = TilerConfig::new(0, max_zoom)
            .with_quiet(true)
            .with_coalesce_densest()
            .with_coalesce_min_density(10.0); // Lower threshold for small fixture
        group.bench_with_input(
            BenchmarkId::new("coalesced", max_zoom),
            &config_coalesce,
            |b, config| {
                b.iter(|| {
                    let result = run_pipeline(fixture_path, config);
                    black_box(result)
                })
            },
        );
    }

    group.finish();

    // Print size comparison (outside timing)
    println!("\n=== Size Comparison (open-buildings) ===");
    for max_zoom in [8, 10] {
        let config_baseline = TilerConfig::new(0, max_zoom).with_quiet(true);
        let config_coalesce = TilerConfig::new(0, max_zoom)
            .with_quiet(true)
            .with_coalesce_densest()
            .with_coalesce_min_density(10.0);

        let (baseline_size, _) = run_pipeline(fixture_path, &config_baseline);
        let (coalesce_size, _) = run_pipeline(fixture_path, &config_coalesce);

        let reduction = if baseline_size > 0 {
            (1.0 - coalesce_size as f64 / baseline_size as f64) * 100.0
        } else {
            0.0
        };

        println!(
            "z0-{}: baseline={} bytes, coalesced={} bytes ({:.1}% {})",
            max_zoom,
            baseline_size,
            coalesce_size,
            reduction.abs(),
            if reduction > 0.0 { "smaller" } else { "larger" }
        );
    }
}

/// Benchmark coalescing on fieldmaps polygons (moderate density)
fn bench_coalescing_polygons(c: &mut Criterion) {
    if !fixture_exists(FIXTURE_POLYGONS) {
        eprintln!("Skipping polygons benchmark: fixture not found");
        return;
    }

    let fixture_path = Path::new(FIXTURE_POLYGONS);

    let mut group = c.benchmark_group("coalescing_polygons");
    group.sample_size(10); // Larger dataset, fewer samples

    for max_zoom in [6, 8] {
        // Baseline
        let config_baseline = TilerConfig::new(0, max_zoom).with_quiet(true);
        group.bench_with_input(
            BenchmarkId::new("baseline", max_zoom),
            &config_baseline,
            |b, config| {
                b.iter(|| {
                    let result = run_pipeline(fixture_path, config);
                    black_box(result)
                })
            },
        );

        // With coalescing
        let config_coalesce = TilerConfig::new(0, max_zoom)
            .with_quiet(true)
            .with_coalesce_densest();
        group.bench_with_input(
            BenchmarkId::new("coalesced", max_zoom),
            &config_coalesce,
            |b, config| {
                b.iter(|| {
                    let result = run_pipeline(fixture_path, config);
                    black_box(result)
                })
            },
        );
    }

    group.finish();

    // Print size comparison
    println!("\n=== Size Comparison (fieldmaps-madagascar) ===");
    for max_zoom in [6, 8] {
        let config_baseline = TilerConfig::new(0, max_zoom).with_quiet(true);
        let config_coalesce = TilerConfig::new(0, max_zoom)
            .with_quiet(true)
            .with_coalesce_densest();

        let (baseline_size, _) = run_pipeline(fixture_path, &config_baseline);
        let (coalesce_size, _) = run_pipeline(fixture_path, &config_coalesce);

        let reduction = if baseline_size > 0 {
            (1.0 - coalesce_size as f64 / baseline_size as f64) * 100.0
        } else {
            0.0
        };

        println!(
            "z0-{}: baseline={} bytes, coalesced={} bytes ({:.1}% {})",
            max_zoom,
            baseline_size,
            coalesce_size,
            reduction.abs(),
            if reduction > 0.0 { "smaller" } else { "larger" }
        );
    }
}

criterion_group!(
    benches,
    bench_coalescing_buildings,
    bench_coalescing_polygons
);
criterion_main!(benches);
