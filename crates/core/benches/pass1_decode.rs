//! Pass-1 scan/decode benchmark (#448).
//!
//! Times the Arrow-native geometry decode path CLAUDE.md documents as the
//! columnar-I/O contract: read a GeoParquet file's record batches with a
//! plain `ParquetRecordBatchReaderBuilder`, then for each batch's geometry
//! column call [`geoarrow::array::from_arrow_array`] +
//! [`tylertoo_core::batch_processor::extract_geometries_from_array`] — the
//! exact pair `overview::stream::scan_chunk` (the production pass-1 path)
//! wraps per chunk. Batch decode is the thing this bench isolates; the file
//! open/schema resolution happens once outside the timed closure.
//!
//! Run with: cargo bench --package tylertoo-core --bench pass1_decode

use std::fs::File;
use std::hint::black_box;
use std::path::Path;

use arrow_array::RecordBatch;
use arrow_schema::{Field, SchemaRef};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use geoarrow::array::from_arrow_array;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::time::Duration;
use tylertoo_core::batch_processor::extract_geometries_from_array;

/// Real-world fixtures under `tests/fixtures/realdata/` (#448 note: reuse
/// in-repo fixtures rather than synthesizing new ones). Feature counts are
/// approximate (see the fixtures' own README) and only used for throughput
/// labeling.
const FIXTURES: &[(&str, &str, u64)] = &[
    ("open-buildings", "open-buildings.parquet", 1_000),
    ("road-detections", "road-detections.parquet", 1_000),
    ("fieldmaps-boundaries", "fieldmaps-boundaries.parquet", 3),
];

fn fixture_path(name: &str) -> String {
    format!("../../tests/fixtures/realdata/{name}")
}

/// Load every record batch of a parquet file into memory once, alongside the
/// schema's geometry field — decode itself is what each bench iteration
/// times, not I/O.
fn load_batches(path: &Path) -> Option<(SchemaRef, Field, usize, Vec<RecordBatch>)> {
    if !path.exists() {
        return None;
    }
    let file = File::open(path).ok()?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).ok()?;
    let schema = builder.schema().clone();
    let gidx = schema.index_of("geometry").ok()?;
    let gfield = schema.field(gidx).clone();
    let reader = builder.build().ok()?;
    let batches: Vec<RecordBatch> = reader.filter_map(|b| b.ok()).collect();
    Some((schema, gfield, gidx, batches))
}

/// Decode every batch's geometry column once, matching `scan_chunk`'s
/// `from_arrow_array` + `extract_geometries_from_array` pair.
fn decode_all(gfield: &Field, gidx: usize, batches: &[RecordBatch]) -> usize {
    let mut total = 0usize;
    for batch in batches {
        let garr = from_arrow_array(batch.column(gidx).as_ref(), gfield).unwrap();
        let mut geoms = Vec::new();
        extract_geometries_from_array(garr.as_ref(), &mut geoms).unwrap();
        total += geoms.len();
    }
    total
}

fn bench_pass1_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("pass1_decode");
    // Real-data fixtures are small (KBs-MBs); a short measurement window
    // keeps this bench inside the CI budget without sacrificing signal.
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(30);

    for (label, file, approx_features) in FIXTURES {
        let path = fixture_path(file);
        let Some((_, gfield, gidx, batches)) = load_batches(Path::new(&path)) else {
            eprintln!("pass1_decode: fixture {path} not found, skipping {label}");
            continue;
        };

        group.throughput(Throughput::Elements(*approx_features));
        group.bench_with_input(
            BenchmarkId::from_parameter(label),
            &(gfield, gidx, batches),
            |b, (gfield, gidx, batches)| {
                b.iter(|| black_box(decode_all(gfield, *gidx, batches)));
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_pass1_decode);
criterion_main!(benches);
