//! Debug test to find which tile(s) cause i_overlay issues (if any).
//!
//! Run with: cargo test --release -p gpq-tiles-core --test debug_antarctica -- --nocapture

use geo::MultiPolygon;
use gpq_tiles_core::ioverlay_clip::clip_multipolygon_ioverlay;
use gpq_tiles_core::tile::TileBounds;
use std::fs::File;
use std::io::Read;
use std::time::{Duration, Instant};

/// Convert tile coordinates to geographic bounds
fn tile_to_bounds(x: u32, y: u32, zoom: u32) -> TileBounds {
    let n = 2.0_f64.powi(zoom as i32);

    let lng_min = x as f64 / n * 360.0 - 180.0;
    let lng_max = (x + 1) as f64 / n * 360.0 - 180.0;

    // Y is inverted in Web Mercator
    let lat_max = (std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n))
        .sinh()
        .atan()
        .to_degrees();
    let lat_min = (std::f64::consts::PI * (1.0 - 2.0 * (y + 1) as f64 / n))
        .sinh()
        .atan()
        .to_degrees();

    TileBounds::new(lng_min, lat_min, lng_max, lat_max)
}

fn load_antarctica() -> Option<MultiPolygon<f64>> {
    let wkb_path = "/tmp/antarctica.wkb";
    let mut file = File::open(wkb_path).ok()?;
    let mut wkb_data = Vec::new();
    file.read_to_end(&mut wkb_data).ok()?;

    use geozero::ToGeo;
    let geom: geo::Geometry<f64> = geozero::wkb::Wkb(wkb_data).to_geo().ok()?;

    match geom {
        geo::Geometry::MultiPolygon(mp) => Some(mp),
        geo::Geometry::Polygon(p) => Some(MultiPolygon::new(vec![p])),
        _ => None,
    }
}

#[test]
#[ignore] // Run explicitly with: cargo test --release -p gpq-tiles-core --test debug_antarctica -- --ignored --nocapture
fn find_problematic_tiles() {
    let mp = match load_antarctica() {
        Some(mp) => mp,
        None => {
            println!("Skipping: /tmp/antarctica.wkb not found");
            return;
        }
    };

    println!("Loaded Antarctica: {} polygons", mp.0.len());

    let zoom = 5;
    let n = 2u32.pow(zoom);
    let slow_threshold = Duration::from_millis(500);

    println!(
        "\nTesting zoom {} tiles (slow threshold: {}ms)...\n",
        zoom,
        slow_threshold.as_millis()
    );

    let mut slow_tiles = Vec::new();

    // Antarctica is in y=22 to y=31 at zoom 5
    for y in 22..=31 {
        for x in 0..n {
            let bounds = tile_to_bounds(x, y, zoom);

            // Quick check - skip if tile is entirely outside Antarctica's latitude range
            if bounds.lat_max < -90.0 || bounds.lat_min > -60.35 {
                continue;
            }

            let start = Instant::now();
            let _result = clip_multipolygon_ioverlay(&mp, &bounds);
            let elapsed = start.elapsed();

            if elapsed > slow_threshold {
                println!(
                    "  SLOW: z{}/x{}/y{} took {:.2}s",
                    zoom,
                    x,
                    y,
                    elapsed.as_secs_f64()
                );
                slow_tiles.push((x, y, elapsed));
            }
        }
        print!(".");
        use std::io::Write;
        std::io::stdout().flush().ok();
    }

    println!("\n\n=== RESULTS ===");
    println!(
        "Total slow tiles (>{:.0}ms): {}",
        slow_threshold.as_millis(),
        slow_tiles.len()
    );

    if !slow_tiles.is_empty() {
        println!("\nSlowest tiles:");
        slow_tiles.sort_by(|a, b| b.2.cmp(&a.2));
        for (x, y, elapsed) in slow_tiles.iter().take(10) {
            let bounds = tile_to_bounds(*x, *y, zoom);
            println!(
                "  z{}/x{}/y{}: {:.2}s  bounds=({:.2},{:.2})->({:.2},{:.2})",
                zoom,
                x,
                y,
                elapsed.as_secs_f64(),
                bounds.lng_min,
                bounds.lat_min,
                bounds.lng_max,
                bounds.lat_max
            );
        }
    }
}

/// Test a single specific tile to reproduce the issue
#[test]
#[ignore]
fn test_single_tile() {
    let mp = match load_antarctica() {
        Some(mp) => mp,
        None => {
            println!("Skipping: /tmp/antarctica.wkb not found");
            return;
        }
    };

    // Test first tile at zoom 5
    let bounds = tile_to_bounds(0, 22, 5);
    println!("Testing z5/x0/y22: {:?}", bounds);

    let start = Instant::now();
    let result = clip_multipolygon_ioverlay(&mp, &bounds);
    let elapsed = start.elapsed();

    println!("Completed in {:.3}s", elapsed.as_secs_f64());
    println!(
        "Result: {:?}",
        result.map(|g| match g {
            geo::Geometry::Polygon(_) => "Polygon".to_string(),
            geo::Geometry::MultiPolygon(mp) => format!("MultiPolygon({} polys)", mp.0.len()),
            _ => "Other".to_string(),
        })
    );
}
