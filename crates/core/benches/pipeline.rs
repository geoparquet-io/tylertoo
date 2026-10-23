//! End-to-end pipeline benchmark (#759): the production path, convert then
//! export, on small real fixtures.
//!
//! The other benches time kernels (clip, simplify, MVT encode, gzip). The
//! perf work of #713-#737 changed what sits between them: the pass-1 scan,
//! ranged decode, staging, the pyramid merge and the export scan. None of
//! that had a bench, so CodSpeed ran nothing on those PRs. This one runs
//! [`convert_to_overviews`] and [`export_pmtiles`] as the CLI does:
//!
//! - `pipeline_convert/<fixture>`: GeoParquet in, overview GeoParquet out.
//! - `pipeline_export/<fixture>`: that overview in, PMTiles out (the
//!   convert happens once, outside the timed closure).
//!
//! Everything runs on one rayon thread: the global pool is built with one
//! thread (as `RAYON_NUM_THREADS=1` does for the CLI), and `read_workers` /
//! `in_flight_batches` are 1, which the docs on [`ConvertOptions`]
//! guarantee produce byte-identical output. Not a `pool.install` of a
//! private pool: convert then runs *on* the pool's only worker and
//! deadlocks waiting for work queued behind itself. That keeps
//! CodSpeed's instruction counts deterministic and free of thread wake-ups
//! (the syscall-dominated warning `assign` gets), and the wall-clock gate
//! comparable run to run. Thread scaling is `assign_scaling`'s job.
//!
//! Run with: cargo bench --package tylertoo-core --bench pipeline

#[path = "support/fixtures.rs"]
mod fixtures;

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

/// Small enough for CodSpeed's Valgrind run, real enough to exercise every
/// geometry family the pipeline treats differently: polygons and lines.
const FIXTURES: &[(&str, &str)] = &[
    ("open-buildings", "open-buildings.parquet"),
    ("road-detections", "road-detections.parquet"),
];

/// Zooms 0-12: deep enough that export cuts real tiles from these
/// city-scale fixtures, shallow enough to stay a CI-sized bench.
fn convert_options() -> ConvertOptions {
    ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 12,
        },
        read_workers: 1,
        in_flight_batches: 1,
        ..Default::default()
    }
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/realdata")
        .join(name)
}

fn bench_pipeline(c: &mut Criterion) {
    // Before anything touches the global pool. If something already did,
    // the run is still correct, just not single-threaded: say so.
    if let Err(e) = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build_global()
    {
        eprintln!("pipeline: rayon global pool already built ({e}); not single-threaded");
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let options = convert_options();

    let mut convert = c.benchmark_group("pipeline_convert");
    convert
        .sample_size(10)
        .measurement_time(Duration::from_secs(5));
    let mut overviews = Vec::new();
    for (name, file) in FIXTURES {
        let input = fixture_path(file);
        if !input.exists() {
            fixtures::missing("pipeline", file);
            continue;
        }
        let output = dir.path().join(format!("{name}.overview.parquet"));
        convert.bench_with_input(BenchmarkId::from_parameter(name), &input, |b, input| {
            b.iter(|| black_box(convert_to_overviews(input, &output, &options).expect("convert")));
        });
        overviews.push((*name, output));
    }
    convert.finish();

    let export_options = ExportOptions::default();
    let mut export = c.benchmark_group("pipeline_export");
    export
        .sample_size(10)
        .measurement_time(Duration::from_secs(5));
    for (name, overview) in &overviews {
        // The convert bench's last iteration left this overview; under
        // CodSpeed, or with a filter that skipped convert, write it here.
        if !overview.exists() {
            let input = fixture_path(&format!("{name}.parquet"));
            convert_to_overviews(&input, overview, &options).expect("convert");
        }
        let archive = dir.path().join(format!("{name}.pmtiles"));
        export.bench_with_input(
            BenchmarkId::from_parameter(name),
            overview,
            |b, overview| {
                b.iter(|| {
                    black_box(export_pmtiles(overview, &archive, &export_options).expect("export"))
                });
            },
        );
    }
    export.finish();
}

criterion_group!(benches, bench_pipeline);
criterion_main!(benches);
