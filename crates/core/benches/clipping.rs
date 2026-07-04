// Benchmark suite for polygon clipping performance
//
// Compares Sutherland-Hodgman (tile clipping) vs i_overlay (general boolean ops)
// on polygons of varying complexity.
//
// Run with: cargo bench --package gpq-tiles-core -- clipping

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use geo::{Coord, LineString, Polygon};
use gpq_tiles_core::clip::clip_geometry;
use gpq_tiles_core::ioverlay_clip::clip_polygon_ioverlay;
use gpq_tiles_core::sutherland_hodgman::clip_polygon_sh;
use gpq_tiles_core::tile::TileBounds;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Path to the Antarctica polygon fixture (316k coords, real-world case)
const ANTARCTICA_FIXTURE: &str = "../../tests/fixtures/realdata/antarctica-polygon.wkb";

/// Load the Antarctica polygon from the fixture file
fn load_antarctica_polygon() -> Option<Polygon<f64>> {
    let path = Path::new(ANTARCTICA_FIXTURE);
    if !path.exists() {
        return None;
    }

    let mut file = File::open(path).ok()?;
    let mut wkb_data = Vec::new();
    file.read_to_end(&mut wkb_data).ok()?;

    use geozero::ToGeo;
    let geom: geo::Geometry<f64> = geozero::wkb::Wkb(wkb_data).to_geo().ok()?;

    match geom {
        geo::Geometry::Polygon(p) => Some(p),
        _ => None,
    }
}

/// Generate a polygon with N vertices approximating a circle
fn generate_circle_polygon(n: usize, center: (f64, f64), radius: f64) -> Polygon<f64> {
    let mut coords: Vec<Coord<f64>> = Vec::with_capacity(n + 1);
    for i in 0..n {
        let angle = 2.0 * std::f64::consts::PI * (i as f64) / (n as f64);
        coords.push(Coord {
            x: center.0 + radius * angle.cos(),
            y: center.1 + radius * angle.sin(),
        });
    }
    coords.push(coords[0]); // Close the ring
    Polygon::new(LineString::new(coords), vec![])
}

/// Generate a polygon spanning a wide longitude range (like Antarctica)
/// with N vertices along a sinusoidal coastline
fn generate_wide_polygon(n: usize) -> Polygon<f64> {
    let mut coords: Vec<Coord<f64>> = Vec::with_capacity(n + 1);

    // Top edge: sinusoidal coastline from -180 to +180
    let half_n = n / 2;
    for i in 0..half_n {
        let lng = -180.0 + 360.0 * (i as f64) / (half_n as f64);
        let lat = -60.0 + 5.0 * (lng * 0.1).sin(); // Wavy coastline around -60°
        coords.push(Coord { x: lng, y: lat });
    }

    // Bottom edge: straight line back (south pole region)
    for i in (0..half_n).rev() {
        let lng = -180.0 + 360.0 * (i as f64) / (half_n as f64);
        coords.push(Coord { x: lng, y: -85.0 });
    }

    coords.push(coords[0]); // Close the ring
    Polygon::new(LineString::new(coords), vec![])
}

/// Benchmark Sutherland-Hodgman clipping at various polygon sizes
fn bench_sutherland_hodgman(c: &mut Criterion) {
    let mut group = c.benchmark_group("sutherland_hodgman");

    // Tile bounds for clipping (small tile)
    let bounds = TileBounds::new(-67.5, -66.51, -56.25, -61.61);

    for size in [100, 1_000, 10_000, 100_000, 316_000].iter() {
        // Generate polygon centered at tile bounds
        let center = (
            (bounds.lng_min + bounds.lng_max) / 2.0,
            (bounds.lat_min + bounds.lat_max) / 2.0,
        );
        let radius = 20.0; // Large enough to extend outside tile
        let poly = generate_circle_polygon(*size, center, radius);

        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &poly, |b, poly| {
            b.iter(|| clip_polygon_sh(black_box(poly), black_box(&bounds)));
        });
    }

    group.finish();
}

/// Benchmark i_overlay clipping at various polygon sizes (for comparison)
fn bench_ioverlay(c: &mut Criterion) {
    let mut group = c.benchmark_group("ioverlay");

    let bounds = TileBounds::new(-67.5, -66.51, -56.25, -61.61);

    // Test various sizes for i_overlay
    for size in [100, 1_000, 10_000].iter() {
        let center = (
            (bounds.lng_min + bounds.lng_max) / 2.0,
            (bounds.lat_min + bounds.lat_max) / 2.0,
        );
        let radius = 20.0;
        let poly = generate_circle_polygon(*size, center, radius);

        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &poly, |b, poly| {
            b.iter(|| clip_polygon_ioverlay(black_box(poly), black_box(&bounds)));
        });
    }

    group.finish();
}

/// Benchmark the Antarctica-like wide polygon case
fn bench_wide_polygon(c: &mut Criterion) {
    let mut group = c.benchmark_group("wide_polygon_clip");

    // Small tile in the middle of the wide polygon
    let bounds = TileBounds::new(-67.5, -66.51, -56.25, -61.61);

    for size in [1_000, 10_000, 100_000, 316_000].iter() {
        let poly = generate_wide_polygon(*size);

        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(
            BenchmarkId::new("sutherland_hodgman", size),
            &poly,
            |b, poly| {
                b.iter(|| clip_polygon_sh(black_box(poly), black_box(&bounds)));
            },
        );
    }

    // Also test i_overlay on the smallest wide polygon for comparison
    let small_wide = generate_wide_polygon(1_000);
    group.bench_function("ioverlay/1000", |b| {
        b.iter(|| clip_polygon_ioverlay(black_box(&small_wide), black_box(&bounds)));
    });

    group.finish();
}

/// Benchmark end-to-end clip_geometry function
fn bench_clip_geometry(c: &mut Criterion) {
    let mut group = c.benchmark_group("clip_geometry");

    let bounds = TileBounds::new(-67.5, -66.51, -56.25, -61.61);
    let buffer = 0.0;

    for size in [1_000, 10_000, 100_000].iter() {
        let poly = generate_wide_polygon(*size);
        let geom = geo::Geometry::Polygon(poly);

        group.throughput(Throughput::Elements(*size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &geom, |b, geom| {
            b.iter(|| clip_geometry(black_box(geom), black_box(&bounds), black_box(buffer)));
        });
    }

    group.finish();
}

/// Benchmark the real Antarctica polygon (316k vertices)
fn bench_antarctica_polygon(c: &mut Criterion) {
    let poly = match load_antarctica_polygon() {
        Some(p) => p,
        None => {
            eprintln!("Warning: Antarctica fixture not found, skipping benchmark");
            return;
        }
    };

    let mut group = c.benchmark_group("antarctica_316k");

    // The problem tile: z5/x10/y23
    let bounds = TileBounds::new(-67.5, -66.51, -56.25, -61.61);
    let vertex_count = poly.exterior().0.len();

    group.throughput(Throughput::Elements(vertex_count as u64));

    // Benchmark Sutherland-Hodgman (the fix)
    group.bench_function("sutherland_hodgman", |b| {
        b.iter(|| clip_polygon_sh(black_box(&poly), black_box(&bounds)));
    });

    // Benchmark clip_geometry (end-to-end)
    let geom = geo::Geometry::Polygon(poly.clone());
    group.bench_function("clip_geometry", |b| {
        b.iter(|| clip_geometry(black_box(&geom), black_box(&bounds), 0.0));
    });

    // Benchmark i_overlay (for comparison)
    group.bench_function("ioverlay", |b| {
        b.iter(|| clip_polygon_ioverlay(black_box(&poly), black_box(&bounds)));
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_sutherland_hodgman,
    bench_ioverlay,
    bench_wide_polygon,
    bench_clip_geometry,
    bench_antarctica_polygon,
);
criterion_main!(benches);
