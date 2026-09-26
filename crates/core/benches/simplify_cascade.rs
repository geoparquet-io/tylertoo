//! Pass-2 cascading simplification benchmark (#218, #448).
//!
//! Times [`simplify_cascade`] — the fine->coarse fold every conversion path
//! (serial, in-memory, pipelined) drives per feature per level — over real
//! polygon fixtures at a production-shaped chain: 10 levels, coarse to
//! fine, mostly full-geometry steps with one zoom-band `Point` step at the
//! coarse end (#317), matching how a `tiles` run's level plan actually looks.
//!
//! Run with: cargo bench --package tylertoo-core --bench simplify_cascade

use std::fs::File;
use std::hint::black_box;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use geo::Geometry;
use geoarrow::array::from_arrow_array;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use tylertoo_core::batch_processor::extract_geometries_from_array;
use tylertoo_core::overview::simplify::{simplify_cascade, CascadeStep, Crs, SimplifyOptions};

/// Path to the Antarctica polygon fixture (316k coords).
const ANTARCTICA_FIXTURE: &str = "../../tests/fixtures/realdata/antarctica-polygon.wkb";
/// Path to the FieldMaps boundaries fixture (3 large admin polygons).
const BOUNDARIES_FIXTURE: &str = "../../tests/fixtures/realdata/fieldmaps-boundaries.parquet";

fn load_antarctica() -> Option<Geometry<f64>> {
    let path = Path::new(ANTARCTICA_FIXTURE);
    if !path.exists() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let mut wkb_data = Vec::new();
    file.read_to_end(&mut wkb_data).ok()?;
    use geozero::ToGeo;
    geozero::wkb::Wkb(wkb_data).to_geo().ok()
}

/// The largest polygon (by vertex count) in the FieldMaps boundaries fixture.
fn load_largest_boundary() -> Option<Geometry<f64>> {
    let path = Path::new(BOUNDARIES_FIXTURE);
    if !path.exists() {
        return None;
    }
    let file = File::open(path).ok()?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).ok()?;
    let schema = builder.schema().clone();
    let gidx = schema.index_of("geometry").ok()?;
    let gfield = schema.field(gidx).clone();
    let mut best: Option<Geometry<f64>> = None;
    let mut best_len = 0usize;
    for batch in builder.build().ok()? {
        let batch = batch.ok()?;
        let garr = from_arrow_array(batch.column(gidx).as_ref(), &gfield).ok()?;
        let mut geoms = Vec::new();
        extract_geometries_from_array(garr.as_ref(), &mut geoms).ok()?;
        for g in geoms {
            let len = geo::coords_iter::CoordsIter::coords_count(&g);
            if len > best_len {
                best_len = len;
                best = Some(g);
            }
        }
    }
    best
}

/// Web-Mercator GSD (meters per pixel at a 256-px tile) for a zoom level.
fn gsd(z: u32) -> f64 {
    40_075_016.685_578_5 / (256.0 * f64::from(1u32 << z))
}

/// A production-shaped level plan: z14 down to z0 (finest first), full
/// geometry through z3, then a zoom-band point representation for the
/// coarsest three levels (#317) — the shape a continental admin dataset's
/// plan typically takes.
fn cascade_steps() -> Vec<CascadeStep> {
    let mut steps: Vec<CascadeStep> = (3..=14).rev().map(|z| CascadeStep::geom(gsd(z))).collect();
    steps.push(CascadeStep::point(gsd(2)));
    steps.push(CascadeStep::point(gsd(1)));
    steps.push(CascadeStep::point(gsd(0)));
    steps
}

fn bench_simplify_cascade(c: &mut Criterion) {
    let mut group = c.benchmark_group("simplify_cascade");
    group.measurement_time(Duration::from_secs(5));
    group.sample_size(20);

    let steps = cascade_steps();
    let opts = SimplifyOptions::default();

    let fixtures: Vec<(&str, Option<Geometry<f64>>)> = vec![
        ("antarctica_316k", load_antarctica()),
        ("fieldmaps_boundary", load_largest_boundary()),
    ];

    for (label, geom) in fixtures {
        let Some(geom) = geom else {
            eprintln!("simplify_cascade: fixture for {label} not found, skipping");
            continue;
        };
        let verts = geo::coords_iter::CoordsIter::coords_count(&geom) as u64;
        group.throughput(Throughput::Elements(verts));
        group.bench_with_input(BenchmarkId::from_parameter(label), &geom, |b, geom| {
            b.iter(|| {
                black_box(simplify_cascade(
                    black_box(geom),
                    black_box(&steps),
                    Crs::Epsg3857,
                    black_box(&opts),
                ))
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_simplify_cascade);
criterion_main!(benches);
