//! Performance test for Sutherland-Hodgman vs wagyu on huge polygons.
//!
//! Run: cargo test --release -p gpq-tiles-core --test huge_polygon_clip -- --ignored --nocapture
//!
//! The 316k-coord polygon should clip in <0.1s with SH (vs ~10s with wagyu).

use geo::Polygon;
use gpq_tiles_core::clip;
use gpq_tiles_core::tile::TileBounds;
use gpq_tiles_core::wagyu_clip::{clip_polygon_wagyu, DEFAULT_EXTENT};
use std::fs::File;
use std::io::Read;
use std::time::Instant;

fn load_huge_polygon() -> Option<Polygon<f64>> {
    let path = "/tmp/huge_antarctica_poly.wkb";
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

#[test]
#[ignore]
fn clip_huge_polygon_with_wagyu() {
    let poly = match load_huge_polygon() {
        Some(p) => p,
        None => {
            println!("Skipping: /tmp/huge_antarctica_poly.wkb not found");
            println!("Generate it by running the Python extraction script first.");
            return;
        }
    };

    println!("=== Huge Polygon Clip Test (Wagyu) ===");
    println!("Polygon: {} exterior coords", poly.exterior().0.len());
    println!(
        "Polygon bounds: {:?}",
        geo::BoundingRect::bounding_rect(&poly)
    );

    let tile_bounds = TileBounds::new(-67.50, -66.51, -56.25, -61.61);
    println!("\nTile bounds: {:?}", tile_bounds);

    println!("\nClipping with wagyu (this may take a while)...");
    let start = Instant::now();
    let result = clip_polygon_wagyu(&poly, &tile_bounds, DEFAULT_EXTENT);
    let elapsed = start.elapsed();

    println!("Wagyu clip time: {:.3}s", elapsed.as_secs_f64());
    println!(
        "Result: {:?}",
        result.as_ref().map(|g| match g {
            geo::Geometry::Polygon(_) => "Polygon",
            geo::Geometry::MultiPolygon(mp) => {
                println!("  {} polygons", mp.0.len());
                "MultiPolygon"
            }
            _ => "Other",
        })
    );

    if elapsed.as_secs_f64() > 10.0 {
        println!("\nCONFIRMED: Wagyu is pathologically slow on this geometry");
        println!("   Shapely baseline: 0.02s");
        println!("   Wagyu actual: {:.1}s", elapsed.as_secs_f64());
        println!("   Slowdown: {:.0}x", elapsed.as_secs_f64() / 0.02);
    }
}

#[test]
#[ignore]
fn clip_huge_polygon_with_sutherland_hodgman() {
    let poly = match load_huge_polygon() {
        Some(p) => p,
        None => {
            println!("Skipping: /tmp/huge_antarctica_poly.wkb not found");
            println!("Generate it by running the Python extraction script first.");
            return;
        }
    };

    println!("=== Huge Polygon Clip Test (Sutherland-Hodgman via clip_geometry) ===");
    println!("Polygon: {} exterior coords", poly.exterior().0.len());

    let tile_bounds = TileBounds::new(-67.50, -66.51, -56.25, -61.61);
    let buffer = 0.0;

    println!("\nClipping with Sutherland-Hodgman (should be fast)...");
    let start = Instant::now();
    let result = clip::clip_geometry(&geo::Geometry::Polygon(poly), &tile_bounds, buffer);
    let elapsed = start.elapsed();

    println!("SH clip time: {:.3}s", elapsed.as_secs_f64());
    println!("Result: {}", if result.is_some() { "Some" } else { "None" });

    assert!(
        elapsed.as_secs_f64() < 0.1,
        "Sutherland-Hodgman should clip in <0.1s, took {:.3}s",
        elapsed.as_secs_f64()
    );
}
