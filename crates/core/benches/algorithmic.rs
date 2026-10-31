//! Junction matching, adversarial RDP and unchanged-geometry costs (#788).
//!
//! Replaces the four manual profiling tests in coalesce.rs/simplify.rs.
//! Uses public entry points: junction cases time the whole coalescing level
//! with snap, visibility and thinning off; RDP cases include polygon checks.
//! These are new Criterion series, not continuations of private-kernel counts.
//!
//! `cargo bench -p tylertoo-core --bench algorithmic`
//! `PHASE1_N=1000` selects one synthetic size (also useful with `-- --test`).
//! Otherwise junctions use 100/1k/10k and sawtooth rings use 1k/10k.
//! Use `PHASE1_N=100000` with the `rdp/sawtooth_ring` filter for the expensive
//! quadratic 100k ring.
//!
//! `PHASE1_INPUT=/path/to/madagascar-or-moldova.parquet` enables the real-data
//! equality/clone/cascade group. Loading and collecting equal input/output
//! pairs at z14..3 happen outside the measured loops. No FIFO controls or
//! test-only observers are needed; Criterion owns repetition and measurement.

#[path = "support/fixtures.rs"]
mod fixtures;

use std::hint::black_box;
use std::path::Path;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use geo::{Coord, Geometry, LineString, Polygon};
use tylertoo_core::diagnostics::Diagnostics;
use tylertoo_core::overview::assign::AssignConfig;
use tylertoo_core::overview::coalesce::{coalesce_level_lines, CoalesceInput, CoalesceParams};
use tylertoo_core::overview::simplify::{
    simplify_cascade, simplify_for_level, CascadeStep, Crs, Simplified, SimplifyOptions,
};

fn sizes(default: &[usize]) -> Vec<usize> {
    std::env::var("PHASE1_N").map_or_else(
        |_| default.to_vec(),
        |n| vec![n.parse().expect("PHASE1_N must be a positive integer")],
    )
}

fn junction(n: usize, tied: bool) -> Vec<Geometry<f64>> {
    (0..n)
        .map(|i| {
            let endpoint = if tied {
                (if i < n / 2 { 1. } else { -1. }, 0.)
            } else {
                let theta = std::f64::consts::TAU * i as f64 / n as f64;
                (theta.cos(), theta.sin())
            };
            Geometry::LineString(LineString::from(vec![(0., 0.), endpoint]))
        })
        .collect()
}

fn bench_junctions(c: &mut Criterion) {
    let config = AssignConfig {
        line_visibility: 0.,
        line_thinning: 0.,
        ..AssignConfig::default()
    };
    let params = CoalesceParams {
        snap_gsd_factor: 0.,
        junction_angle_deg: 30.,
        budget: None,
    };
    let mut group = c.benchmark_group("junctions");
    for n in sizes(&[100, 1_000, 10_000]) {
        group.throughput(Throughput::Elements(n as u64));
        for (label, tied) in [("radial", false), ("tied", true)] {
            let geoms = junction(n, tied);
            let lines: Vec<_> = geoms
                .iter()
                .enumerate()
                .map(|(index, geom)| CoalesceInput {
                    index,
                    geom,
                    sort_key: None,
                    group: 0,
                })
                .collect();
            group.bench_with_input(BenchmarkId::new(label, n), &lines, |b, lines| {
                b.iter(|| {
                    black_box(coalesce_level_lines(
                        black_box(lines),
                        1.,
                        Crs::Epsg3857,
                        &config,
                        &params,
                    ))
                });
            });
        }
    }
    group.finish();
}

fn sawtooth_ring(n: usize) -> Geometry<f64> {
    assert!(n >= 4, "sawtooth ring needs at least four coordinates");
    let mut coords: Vec<_> = (0..n - 3)
        .map(|i| Coord {
            x: i as f64,
            y: if i % 2 == 0 { 0. } else { 1. },
        })
        .collect();
    coords.push(Coord {
        x: (n - 4) as f64,
        y: -2.,
    });
    coords.push(Coord { x: 0., y: -2. });
    coords.push(coords[0]);
    Geometry::Polygon(Polygon::new(LineString::new(coords), vec![]))
}

fn bench_rdp(c: &mut Criterion) {
    let mut group = c.benchmark_group("rdp/sawtooth_ring");
    group.sample_size(10);
    let opts = SimplifyOptions {
        factor: 0.99,
        ..SimplifyOptions::default()
    };
    let diagnostics = Diagnostics::default();
    for n in sizes(&[1_000, 10_000]) {
        let geom = sawtooth_ring(n);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &geom, |b, geom| {
            b.iter(|| {
                black_box(simplify_for_level(
                    black_box(geom),
                    1.,
                    Crs::Epsg3857,
                    &opts,
                    &diagnostics,
                ))
            });
        });
    }
    group.finish();
}

fn real_geometries(path: &Path) -> Vec<Geometry<f64>> {
    let builder = tylertoo_io::open_parquet(std::fs::File::open(path).unwrap()).unwrap();
    let schema = builder.schema().clone();
    let idx = schema.index_of("geometry").unwrap();
    let field = schema.field(idx);
    let mut geoms = Vec::new();
    for batch in builder.build().unwrap() {
        let batch = batch.unwrap();
        let array = geoarrow::array::from_arrow_array(batch.column(idx).as_ref(), field).unwrap();
        tylertoo_core::batch_processor::extract_geometries_from_array(
            array.as_ref(),
            &mut geoms,
            &Diagnostics::default(),
        )
        .unwrap();
    }
    geoms
}

fn bench_identity(c: &mut Criterion) {
    let Some(path) = std::env::var_os("PHASE1_INPUT") else {
        return; // Opt-in: real-data preparation can retain many equal pairs.
    };
    let path = Path::new(&path);
    if !path.exists() {
        fixtures::missing("algorithmic/identity", &path.display().to_string());
        return;
    }
    let geoms = real_geometries(path);
    let steps: Vec<_> = (3..=14)
        .rev()
        .map(|z| {
            CascadeStep::geom(40_075_016.685_578_5 / (256. * f64::from(1u32 << z)))
                .with_zoom(Some(z))
        })
        .collect();
    let opts = SimplifyOptions::default();
    let diagnostics = Diagnostics::default();
    let mut equal = Vec::new();
    for source in &geoms {
        let mut current = source.clone();
        for step in &steps {
            match simplify_for_level(
                &current,
                step.gsd_meters,
                Crs::Epsg4326,
                &opts,
                &diagnostics,
            ) {
                Simplified::Keep(next) => {
                    if current == next {
                        equal.push((current.clone(), next.clone()));
                    }
                    current = next;
                }
                Simplified::Dropped => break,
            }
        }
    }
    assert!(
        !equal.is_empty(),
        "identity fixture produced no equal pairs"
    );
    let mut group = c.benchmark_group("identity");
    group.throughput(Throughput::Elements(equal.len() as u64));
    group.bench_function("equal", |b| {
        b.iter(|| {
            for (a, other) in &equal {
                black_box(black_box(a) == black_box(other));
            }
        });
    });
    group.bench_function("clone", |b| {
        b.iter(|| {
            for (a, _) in &equal {
                black_box(black_box(a).clone());
            }
        });
    });
    group.throughput(Throughput::Elements(geoms.len() as u64));
    group.bench_function("cascade", |b| {
        b.iter(|| {
            for source in &geoms {
                black_box(simplify_cascade(
                    black_box(source),
                    &steps,
                    Crs::Epsg4326,
                    &opts,
                    &diagnostics,
                ));
            }
        });
    });
    group.finish();
}

criterion_group!(benches, bench_junctions, bench_rdp, bench_identity);
criterion_main!(benches);
