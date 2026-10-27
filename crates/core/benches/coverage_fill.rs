//! Coverage fill benchmark (#750): `export_pmtiles` with and without
//! `--coverage-fill`, on scattered sub-pixel fields.
//!
//! Round 2 of the fill doubled its export time on FTW Moldova and only the
//! 25-minute coalesce benchmark showed it. This bench puts the fill's own
//! cost (the pyramid built before the export, and its features encoded into
//! every coarser tile) on every PR that touches the export, as the gap
//! between `export/fill` and `export/off`.
//!
//! Input: the first 1,500 fields of `ftw-moldova-z10.parquet` (one z10 tile
//! of the FTW Moldova benchmark input, in Hilbert order), converted once to
//! an overview of z0–z12 outside the timed loop. The size valve's passes are
//! held under ratchets by `tests/coverage_fill.rs` instead; a larger input
//! that keeps the valve busy is too slow under the `CodSpeed` simulation.
//!
//! Run with: cargo bench --package tylertoo-core --bench `coverage_fill`
#![allow(clippy::disallowed_methods)] // #779: test code; reads and writes its own trusted fixtures

#[path = "support/fixtures.rs"]
mod fixtures;

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};
use tylertoo_io::open_parquet;
use tylertoo_io::test_support::ArrowWriter;

const FIXTURE: &str = "../../tests/fixtures/realdata/ftw-moldova-z10.parquet";
const FIELDS: usize = 1_500;

/// The fixture's first [`FIELDS`] rows, schema and GeoParquet metadata
/// kept, written to `out`.
fn write_prefix(out: &Path) -> Option<()> {
    let builder = open_parquet(File::open(FIXTURE).ok()?).ok()?;
    let schema = builder.schema().clone();
    let reader = builder.with_limit(FIELDS).build().ok()?;
    let mut writer = ArrowWriter::try_new(File::create(out).ok()?, schema, None).ok()?;
    for batch in reader {
        writer.write(&batch.ok()?).ok()?;
    }
    writer.close().ok()?;
    Some(())
}

/// Whether `path` is a real Parquet file: not missing, and not the pointer
/// file a checkout without large-file support leaves in its place.
fn is_parquet(path: &str) -> bool {
    let mut magic = [0u8; 4];
    File::open(path)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic))
        .is_ok_and(|()| &magic == b"PAR1")
}

fn bench_coverage_fill(c: &mut Criterion) {
    if !is_parquet(FIXTURE) {
        fixtures::missing("coverage_fill", FIXTURE);
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("fields.parquet");
    write_prefix(&input).expect("write the fixture prefix");
    let overview = dir.path().join("overview.parquet");
    convert_to_overviews(
        &input,
        &overview,
        &ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 12,
            },
            ..Default::default()
        },
    )
    .expect("convert");
    let archive = dir.path().join("out.pmtiles");

    let mut group = c.benchmark_group("coverage_fill");
    group.measurement_time(Duration::from_secs(10));
    group.sample_size(10);
    for (name, fill) in [("export/off", false), ("export/fill", true)] {
        let opts = ExportOptions {
            coverage_fill: fill,
            ..ExportOptions::default()
        };
        group.bench_function(name, |b| {
            b.iter(|| export_pmtiles(&overview, &archive, &opts).expect("export"));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_coverage_fill);
criterion_main!(benches);
