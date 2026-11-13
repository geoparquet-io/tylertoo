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
//! Each runs under the default `auto` memory profile (the bare id) and under
//! `--profile speed` (`<fixture>-speed`). Two synthetic inputs, written at
//! startup, run convert only (zooms 0-4: the cells time pass 2's read),
//! under both: `list-heavy` (two row groups of 15k points with a 512-value
//! list, which pass 2 decodes one row at a time) and `small-row-groups`
//! (1,000 groups of 100 points). Pass 2's column-split decode slowed both
//! several-fold under `speed` (#876), where the memory plan always grants
//! the split threads, and no bench timed `speed`. The split runs only in a
//! parallel read, so these two read with 2 workers, and it needs
//! `available_parallelism() / 4 / workers >= 2`
//! (`MemoryBudget::read_decode_plan`), so 16 cores: a 4-core runner times
//! these cells without it.
//!
//! Everything runs on one rayon thread: the global pool is built with one
//! thread (as `RAYON_NUM_THREADS=1` does for the CLI), and `read_workers`
//! (but for the two synthetic inputs) and `in_flight_batches` are 1,
//! which the docs on [`ConvertOptions`]
//! guarantee produce byte-identical output. The global pool rather than a
//! `pool.install` keeps the caller off the pool, as in the CLI. (An early
//! version of this bench used `install` and found that convert deadlocked
//! there; `tests/rayon_pool_reentry.rs` pins the fix.) That keeps
//! CodSpeed's instruction counts deterministic and free of thread wake-ups
//! (the syscall-dominated warning `assign` gets), and the wall-clock gate
//! comparable run to run. Thread scaling is `assign_scaling`'s job.
//!
//! Run with: cargo bench --package tylertoo-core --bench pipeline
#![expect(
    clippy::disallowed_methods,
    reason = "#779: trusted fixture construction"
)] // #779: bench code; writes its own trusted synthetic inputs

#[path = "support/fixtures.rs"]
mod fixtures;

use std::fs::File;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arrow_array::builder::{Int64Builder, ListBuilder};
use arrow_array::{ArrayRef, BinaryArray, Int64Array, RecordBatch};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::internal::overview::export::{export_pmtiles, ExportOptions};
use tylertoo_core::MemoryProfile;
use tylertoo_io::test_support::{ArrowWriter, WriterProperties};

/// Small enough for CodSpeed's Valgrind run, real enough to exercise every
/// geometry family the pipeline treats differently: polygons and lines.
const FIXTURES: &[(&str, &str)] = &[
    ("open-buildings", "open-buildings.parquet"),
    ("road-detections", "road-detections.parquet"),
];

/// `auto` keeps the bare bench id it had before `speed` was timed.
const PROFILES: &[(MemoryProfile, &str)] =
    &[(MemoryProfile::Auto, ""), (MemoryProfile::Speed, "-speed")];

/// Zooms 0-12: deep enough that export cuts real tiles from these
/// city-scale fixtures, shallow enough to stay a CI-sized bench.
fn convert_options(profile: MemoryProfile) -> ConvertOptions {
    let mut options = ConvertOptions::default()
        .with_levels(LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom: 12,
        })
        .with_read_workers(1)
        .with_in_flight_batches(1);
    options.profile = profile;
    options
}

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/realdata")
        .join(name)
}

/// A point GeoParquet of `groups` row groups of `rows` points: `id`, the
/// columns `extra(first_row, rows)` returns, and a WKB `geometry`.
fn write_points(
    out: &Path,
    groups: usize,
    rows: usize,
    extra: impl Fn(usize, usize) -> Vec<(&'static str, ArrayRef)>,
) {
    let geo = r#"{"version":"1.1.0","primary_column":"geometry","columns":{"geometry":{"encoding":"WKB","geometry_types":["Point"]}}}"#;
    let props = WriterProperties::builder()
        .set_max_row_group_row_count(Some(rows))
        .build();
    let mut writer: Option<ArrowWriter<File>> = None;
    for g in 0..groups {
        let first = g * rows;
        let wkb = (first..first + rows).map(|i| {
            let x = -100.0 + (i % 1000) as f64 / 500.0;
            let y = 19.0 + (i / 1000) as f64 / 400.0;
            let mut p = vec![1u8, 1, 0, 0, 0];
            p.extend(x.to_le_bytes());
            p.extend(y.to_le_bytes());
            p
        });
        let mut columns = vec![(
            "id",
            Arc::new(Int64Array::from_iter_values(
                (first..first + rows).map(|i| i as i64),
            )) as ArrayRef,
        )];
        columns.extend(extra(first, rows));
        columns.push(("geometry", Arc::new(BinaryArray::from_iter_values(wkb))));
        let batch = RecordBatch::try_from_iter(columns).expect("batch");
        let schema = Arc::new(
            batch
                .schema()
                .as_ref()
                .clone()
                .with_metadata([("geo".to_string(), geo.to_string())].into()),
        );
        let batch = batch.with_schema(schema.clone()).expect("geo metadata");
        let writer = writer.get_or_insert_with(|| {
            let file = File::create(out).expect("create");
            ArrowWriter::try_new(file, schema, Some(props.clone())).expect("writer")
        });
        writer.write(&batch).expect("write");
    }
    writer.expect("groups > 0").close().expect("close");
}

/// The two input shapes behind #876's `speed`-only slowdowns, deterministic.
fn synthetic_inputs(dir: &Path) -> Vec<(&'static str, PathBuf)> {
    // Each group's 7.7M list values price its decode past the 64 MiB limit
    // (`batch_plan` in tylertoo-io), so pass 2 decodes it one row at a time.
    let list_heavy = dir.join("list-heavy.parquet");
    write_points(&list_heavy, 2, 15_000, |first, rows| {
        let mut lists = ListBuilder::new(Int64Builder::new());
        for i in first..first + rows {
            lists.values().append_slice(&[(i % 97) as i64; 512]);
            lists.append(true);
        }
        vec![("vals", Arc::new(lists.finish()) as ArrayRef)]
    });
    // Each group decodes ~6 KB, far below what one requested batch of rows
    // would estimate.
    let small_groups = dir.join("small-row-groups.parquet");
    write_points(&small_groups, 1_000, 100, |first, rows| {
        let col = |m: usize| {
            Arc::new(Int64Array::from_iter_values(
                (first..first + rows).map(move |i| (i * m % 1009) as i64),
            )) as ArrayRef
        };
        vec![("a", col(7)), ("b", col(13)), ("c", col(31))]
    });
    vec![
        ("list-heavy", list_heavy),
        ("small-row-groups", small_groups),
    ]
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
    // (name, input, real): a real fixture is also exported; a synthetic one
    // reads in parallel, where pass 2's column split runs.
    let mut inputs = Vec::new();
    for (name, file) in FIXTURES {
        let input = fixture_path(file);
        if input.exists() {
            inputs.push((*name, input, true));
        } else {
            fixtures::missing("pipeline", file);
        }
    }
    let synthetic = synthetic_inputs(dir.path());
    inputs.extend(
        synthetic
            .into_iter()
            .map(|(name, input)| (name, input, false)),
    );

    let mut convert = c.benchmark_group("pipeline_convert");
    convert
        .sample_size(10)
        .measurement_time(Duration::from_secs(5));
    let mut overviews = Vec::new();
    for (name, input, real) in &inputs {
        for &(profile, suffix) in PROFILES {
            let id = format!("{name}{suffix}");
            let mut options = convert_options(profile);
            if !real {
                options = options
                    .with_read_workers(2)
                    .with_levels(LevelPlan::ZoomRange {
                        min_zoom: 0,
                        max_zoom: 4,
                    });
            }
            let output = dir.path().join(format!("{id}.overview.parquet"));
            convert.bench_with_input(BenchmarkId::from_parameter(&id), input, |b, input| {
                b.iter(|| {
                    black_box(convert_to_overviews(input, &output, &options).expect("convert"))
                });
            });
            if *real {
                overviews.push((id, input, output, profile));
            }
        }
    }
    convert.finish();

    let mut export = c.benchmark_group("pipeline_export");
    export
        .sample_size(10)
        .measurement_time(Duration::from_secs(5));
    for (id, input, overview, profile) in &overviews {
        // The convert bench's last iteration left this overview; under
        // CodSpeed, or with a filter that skipped convert, write it here.
        if !overview.exists() {
            convert_to_overviews(input, overview, &convert_options(*profile)).expect("convert");
        }
        let mut export_options = ExportOptions::default();
        export_options.profile = *profile;
        let archive = overview.with_extension("pmtiles");
        export.bench_with_input(BenchmarkId::from_parameter(id), overview, |b, overview| {
            b.iter(|| {
                black_box(export_pmtiles(overview, &archive, &export_options).expect("export"))
            });
        });
    }
    export.finish();
}

criterion_group!(benches, bench_pipeline);
criterion_main!(benches);
