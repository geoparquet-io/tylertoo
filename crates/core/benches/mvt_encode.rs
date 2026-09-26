//! MVT encode benchmark (#448): the noding sweep + quantize-clean polygon
//! path, and the alloc-free value-dedup path #559 introduced.
//!
//! Two groups:
//! - `encode_polygon`: [`encode_polygon`] on real/synthetic rings, which
//!   quantizes to tile-integer space, runs the [`NODE_MAX_EDGES`]-gated
//!   noding sweep, cleans pinches, and orients the result (#383/#461).
//! - `layer_value_dedup`: [`LayerBuilder::add_feature`] +
//!   [`LayerBuilder::build`] over a feature set whose properties repeat
//!   heavily across features (the common case: a handful of distinct
//!   `highway`/`admin_level`-style values shared by thousands of rows) —
//!   the shape the #559 alloc-free `ScalarKey`/string dedup targets.
//!
//! Run with: cargo bench --package tylertoo-core --bench mvt_encode

use std::fs::File;
use std::hint::black_box;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use geo::{Coord, Geometry, LineString, Point, Polygon};
use tylertoo_core::mvt::{encode_polygon, LayerBuilder, PropertyValue};
use tylertoo_core::tile::TileBounds;

/// Path to the Antarctica polygon fixture (316k coords, exercises the
/// noding sweep's `NODE_MAX_EDGES` cap boundary).
const ANTARCTICA_FIXTURE: &str = "../../tests/fixtures/realdata/antarctica-polygon.wkb";

fn load_antarctica_polygon() -> Option<Polygon<f64>> {
    let path = Path::new(ANTARCTICA_FIXTURE);
    if !path.exists() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let mut wkb_data = Vec::new();
    file.read_to_end(&mut wkb_data).ok()?;
    use geozero::ToGeo;
    match geozero::wkb::Wkb(wkb_data).to_geo().ok()? {
        Geometry::Polygon(p) => Some(p),
        _ => None,
    }
}

/// A polygon with N vertices approximating a circle, small enough to stay
/// under [`NODE_MAX_EDGES`] so the full noding sweep runs (not just the
/// cheap early-out).
fn generate_circle_polygon(n: usize, center: (f64, f64), radius: f64) -> Polygon<f64> {
    let mut coords: Vec<Coord<f64>> = Vec::with_capacity(n + 1);
    for i in 0..n {
        let angle = 2.0 * std::f64::consts::PI * (i as f64) / (n as f64);
        coords.push(Coord {
            x: center.0 + radius * angle.cos(),
            y: center.1 + radius * angle.sin(),
        });
    }
    coords.push(coords[0]);
    Polygon::new(LineString::new(coords), vec![])
}

fn bench_encode_polygon(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode_polygon");
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(30);

    let bounds = TileBounds::new(-67.5, -66.51, -56.25, -61.61);

    for size in [100usize, 1_000, 4_000] {
        let center = (
            (bounds.lng_min + bounds.lng_max) / 2.0,
            (bounds.lat_min + bounds.lat_max) / 2.0,
        );
        let poly = generate_circle_polygon(size, center, 20.0);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &poly, |b, poly| {
            b.iter(|| black_box(encode_polygon(black_box(poly), black_box(&bounds), 4096)));
        });
    }

    if let Some(poly) = load_antarctica_polygon() {
        let verts = poly.exterior().0.len() as u64;
        group.throughput(Throughput::Elements(verts));
        group.bench_function("antarctica_316k", |b| {
            b.iter(|| black_box(encode_polygon(black_box(&poly), black_box(&bounds), 4096)));
        });
    } else {
        eprintln!("mvt_encode: Antarctica fixture not found, skipping antarctica_316k");
    }

    group.finish();
}

/// A small pool of realistic repeated tag values (the common OSM/admin-data
/// shape: a handful of distinct strings/numbers shared by many features).
fn synthetic_properties(feature_idx: usize) -> Vec<(String, PropertyValue)> {
    const HIGHWAYS: &[&str] = &["residential", "primary", "secondary", "service", "track"];
    const SURFACES: &[&str] = &["paved", "unpaved", "gravel"];
    vec![
        (
            "highway".to_string(),
            PropertyValue::String(HIGHWAYS[feature_idx % HIGHWAYS.len()].to_string()),
        ),
        (
            "surface".to_string(),
            PropertyValue::String(SURFACES[feature_idx % SURFACES.len()].to_string()),
        ),
        (
            "lanes".to_string(),
            PropertyValue::UInt((feature_idx % 5) as u64 + 1),
        ),
        (
            "oneway".to_string(),
            PropertyValue::Bool(feature_idx.is_multiple_of(3)),
        ),
        (
            "maxspeed".to_string(),
            PropertyValue::Double(30.0 + (feature_idx % 6) as f64 * 10.0),
        ),
    ]
}

fn bench_value_dedup(c: &mut Criterion) {
    let mut group = c.benchmark_group("layer_value_dedup");
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(30);

    let bounds = TileBounds::new(-1.0, -1.0, 1.0, 1.0);

    for n in [1_000usize, 10_000] {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            b.iter(|| {
                let mut builder = LayerBuilder::new("bench").with_extent(4096);
                for i in 0..n {
                    let geom = Geometry::Point(Point::new(
                        (i % 100) as f64 / 100.0 - 0.5,
                        (i / 100) as f64 / 100.0 - 0.5,
                    ));
                    let props = synthetic_properties(i);
                    builder.add_feature(Some(i as u64), &geom, &props, &bounds);
                }
                black_box(builder.build())
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_encode_polygon, bench_value_dedup);
criterion_main!(benches);
