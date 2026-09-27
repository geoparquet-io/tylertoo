// Hostile-geometry clipping bench (#205): the perf half of the differential
// evaluation in `tests/hostile_geometry_eval.rs`.
//
// Three tiers — clean, invalid, pathological — each clipped by the engine
// candidates that live in-tree: the production dispatch (`clip_geometry_simple`
// with export's defaults), raw Sutherland–Hodgman, and raw i_overlay 9. wagyu-rs
// cannot be linked into this workspace (see corpus/hostile_wagyu/Cargo.toml);
// its per-case wall times come from that runner and land in the scorecard.
//
// Run with: cargo bench --package tylertoo-core --bench hostile_geometry

#[path = "support/fixtures.rs"]
mod fixtures;

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use geo::{Coord, CoordsIter, Geometry, LineString, Polygon};
use tylertoo_core::clip::{clip_geometry_simple, geometry_is_simple};
use tylertoo_core::ioverlay_clip::clip_polygon_ioverlay;
use tylertoo_core::sutherland_hodgman::clip_polygon_sh;
use tylertoo_core::tile::{lng_lat_to_tile, TileBounds};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn poly(ext: &[(f64, f64)]) -> Polygon<f64> {
    Polygon::new(LineString::from(ext.to_vec()), vec![])
}

fn circle(cx: f64, cy: f64, r: f64, n: usize) -> Polygon<f64> {
    let mut v: Vec<Coord<f64>> = (0..n)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            Coord {
                x: cx + r * a.cos(),
                y: cy + r * a.sin(),
            }
        })
        .collect();
    v.push(v[0]);
    Polygon::new(LineString::new(v), vec![])
}

fn comb(teeth: usize) -> Polygon<f64> {
    let w = teeth as f64 * 4.0;
    let mut ext = vec![(0.0, 0.0), (w, 0.0)];
    for t in (0..teeth).rev() {
        let x = t as f64 * 4.0;
        ext.push((x + 3.0, 0.0));
        ext.push((x + 3.0, 30.0));
        ext.push((x + 1.0, 30.0));
        ext.push((x + 1.0, 0.0));
    }
    ext.push((0.0, 0.0));
    poly(&ext)
}

/// A many-lobed self-intersecting ring: a star whose edges cross.
fn star_self_crossing(n: usize) -> Polygon<f64> {
    let mut v: Vec<Coord<f64>> = (0..n)
        .map(|i| {
            // Step by 2 lobes each time so consecutive edges cross.
            let a = std::f64::consts::TAU * ((i * 2) % n) as f64 / n as f64;
            Coord {
                x: 5.0 + 4.0 * a.cos(),
                y: 5.0 + 4.0 * a.sin(),
            }
        })
        .collect();
    v.push(v[0]);
    Polygon::new(LineString::new(v), vec![])
}

fn load_corpus_polygon(rel: &str) -> Option<Polygon<f64>> {
    let path = repo_root()
        .join("tests/fixtures/geometry-test-data/examples")
        .join(rel);
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let coords = &json["features"][0]["geometry"]["coordinates"];
    let rings: Vec<LineString<f64>> = coords
        .as_array()?
        .iter()
        .map(|ring| {
            LineString::new(
                ring.as_array()
                    .unwrap()
                    .iter()
                    .map(|c| Coord {
                        x: c[0].as_f64().unwrap(),
                        y: c[1].as_f64().unwrap(),
                    })
                    .collect(),
            )
        })
        .collect();
    let mut it = rings.into_iter();
    Some(Polygon::new(it.next()?, it.collect()))
}

fn load_antarctica() -> Option<Polygon<f64>> {
    use geozero::ToGeo;
    let bytes =
        std::fs::read(repo_root().join("tests/fixtures/realdata/antarctica-polygon.wkb")).ok()?;
    match geozero::wkb::Wkb(bytes).to_geo().ok()? {
        Geometry::Polygon(p) => Some(p),
        _ => None,
    }
}

/// A tile that cuts through the polygon's middle, two zooms finer than the
/// one that holds its whole bbox, buffered like export (8 px of 256).
fn cutting_tile(p: &Polygon<f64>) -> (TileBounds, f64) {
    use geo::BoundingRect;
    let r = p.bounding_rect().unwrap();
    let span = r.width().max(r.height()).clamp(1e-9, 360.0);
    let z = ((360.0 / span).log2().floor() as i32).clamp(0, 16) as u8 + 2;
    let c = r.center();
    let tb = lng_lat_to_tile(c.x.clamp(-180.0, 180.0), c.y.clamp(-85.0, 85.0), z).bounds();
    let buf = tb.width() * 8.0 / 256.0;
    (tb, buf)
}

fn bench_tier(c: &mut Criterion, tier: &str, cases: &[(&str, Polygon<f64>)]) {
    let mut group = c.benchmark_group(format!("hostile_{tier}"));
    for (name, p) in cases {
        let (tile, buf) = cutting_tile(p);
        let buffered = TileBounds::new(
            tile.lng_min - buf,
            tile.lat_min - buf,
            tile.lng_max + buf,
            tile.lat_max + buf,
        );
        let geom = Geometry::Polygon(p.clone());
        let simple = geometry_is_simple(&geom);
        group.throughput(Throughput::Elements(p.coords_count() as u64));
        group.bench_with_input(BenchmarkId::new("production", name), &geom, |b, g| {
            b.iter(|| {
                clip_geometry_simple(black_box(g), black_box(&tile), black_box(buf), simple, true)
            });
        });
        group.bench_with_input(BenchmarkId::new("sh", name), p, |b, p| {
            b.iter(|| clip_polygon_sh(black_box(p), black_box(&buffered)));
        });
        group.bench_with_input(BenchmarkId::new("ioverlay", name), p, |b, p| {
            b.iter(|| clip_polygon_ioverlay(black_box(p), black_box(&buffered)));
        });
    }
    group.finish();
}

fn bench_clean(c: &mut Criterion) {
    bench_tier(
        c,
        "clean",
        &[
            (
                "square",
                poly(&[
                    (0.0, 0.0),
                    (10.0, 0.0),
                    (10.0, 10.0),
                    (0.0, 10.0),
                    (0.0, 0.0),
                ]),
            ),
            ("circle-1k", circle(5.0, 5.0, 4.0, 1000)),
            ("circle-100k", circle(5.0, 5.0, 4.0, 100_000)),
        ],
    );
}

fn bench_invalid(c: &mut Criterion) {
    let mut cases = vec![
        (
            "bowtie",
            poly(&[(0.0, 0.0), (4.0, 4.0), (4.0, 0.0), (0.0, 4.0), (0.0, 0.0)]),
        ),
        ("star-1k-crossing", star_self_crossing(1001)),
    ];
    match load_corpus_polygon("problematic_geometries/problematic_self_intersection_large.geojson")
    {
        Some(p) => cases.push(("corpus-self-intersection-large", p)),
        None => fixtures::missing("hostile_geometry", "geometry-test-data submodule"),
    }
    bench_tier(c, "invalid", &cases);
}

fn bench_pathological(c: &mut Criterion) {
    let mut cases = vec![("comb-64", comb(64)), ("comb-1024", comb(1024))];
    match load_antarctica() {
        Some(p) => cases.push(("antarctica-316k", p)),
        None => fixtures::missing("hostile_geometry", "antarctica-polygon.wkb"),
    }
    bench_tier(c, "pathological", &cases);
}

criterion_group!(benches, bench_clean, bench_invalid, bench_pathological);
criterion_main!(benches);
