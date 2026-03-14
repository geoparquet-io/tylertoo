// Benchmark suite for bbox containment optimization (Issue #117)
//
// Measures the performance impact of the bbox pre-filter optimization that
// skips clipping when a feature's bounding box is fully contained within
// tile bounds.
//
// Run with: cargo bench --package gpq-tiles-core -- bbox_containment

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use geo::{BoundingRect, Coord, Geometry, LineString, MultiPolygon, Polygon};
use gpq_tiles_core::clip::clip_geometry;
use gpq_tiles_core::tile::TileBounds;

/// Generate small building-like polygons (typical size: 10-30m)
/// These should benefit greatly from bbox containment optimization at high zoom
fn generate_small_buildings(count: usize, tile_bounds: &TileBounds) -> Vec<Polygon<f64>> {
    let mut buildings = Vec::with_capacity(count);

    // Calculate approximate meters per degree at this latitude
    let center_lat = (tile_bounds.lat_min + tile_bounds.lat_max) / 2.0;
    let meters_per_deg_lng = 111_320.0 * center_lat.to_radians().cos();
    let meters_per_deg_lat = 111_320.0;

    // Building size: ~20 meters square
    let building_width_deg = 20.0 / meters_per_deg_lng;
    let building_height_deg = 20.0 / meters_per_deg_lat;

    // Place buildings in a grid within the tile bounds
    let tile_width = tile_bounds.lng_max - tile_bounds.lng_min;
    let tile_height = tile_bounds.lat_max - tile_bounds.lat_min;

    let grid_size = (count as f64).sqrt().ceil() as usize;
    let spacing_x = tile_width / (grid_size as f64 + 1.0);
    let spacing_y = tile_height / (grid_size as f64 + 1.0);

    for i in 0..count {
        let row = i / grid_size;
        let col = i % grid_size;

        let x = tile_bounds.lng_min + spacing_x * (col as f64 + 1.0);
        let y = tile_bounds.lat_min + spacing_y * (row as f64 + 1.0);

        // Create rectangular building
        let building = Polygon::new(
            LineString::from(vec![
                Coord { x, y },
                Coord {
                    x: x + building_width_deg,
                    y,
                },
                Coord {
                    x: x + building_width_deg,
                    y: y + building_height_deg,
                },
                Coord {
                    x,
                    y: y + building_height_deg,
                },
                Coord { x, y },
            ]),
            vec![],
        );

        buildings.push(building);
    }

    buildings
}

/// Generate large polygons (state/province boundaries)
/// These should NOT benefit from bbox containment - they span multiple tiles
fn generate_large_boundaries(count: usize) -> Vec<Polygon<f64>> {
    let mut boundaries = Vec::with_capacity(count);

    for i in 0..count {
        let base_lng = -180.0 + (i as f64 * 10.0);
        let base_lat = -80.0 + (i as f64 * 5.0);

        // Create large polygon spanning ~10 degrees (roughly state-sized)
        let mut coords = Vec::new();

        // Create irregular boundary with ~100 vertices
        for j in 0..100 {
            let angle = 2.0 * std::f64::consts::PI * (j as f64) / 100.0;
            let radius = 5.0 + (angle * 3.0).sin() * 1.0; // Irregular shape
            coords.push(Coord {
                x: base_lng + radius * angle.cos(),
                y: base_lat + radius * angle.sin(),
            });
        }
        coords.push(coords[0]); // Close the ring

        boundaries.push(Polygon::new(LineString::from(coords), vec![]));
    }

    boundaries
}

/// Count how many features would skip clipping due to bbox containment
fn count_fully_inside(geometries: &[Polygon<f64>], bounds: &TileBounds) -> usize {
    geometries
        .iter()
        .filter(|poly| {
            if let Some(bbox) = poly.bounding_rect() {
                bbox.min().x >= bounds.lng_min
                    && bbox.max().x <= bounds.lng_max
                    && bbox.min().y >= bounds.lat_min
                    && bbox.max().y <= bounds.lat_max
            } else {
                false
            }
        })
        .count()
}

/// Benchmark small buildings at high zoom (z14) - should show significant speedup
fn bench_small_features_high_zoom(c: &mut Criterion) {
    let mut group = c.benchmark_group("bbox_containment/small_features_z14");

    // Tile at z14 (typical for building-level detail)
    // Example: San Francisco area
    let tile_bounds = TileBounds::new(-122.5, 37.7, -122.4, 37.8);
    let buffer = 0.0001; // Small buffer for z14

    for count in [100, 500, 1000, 5000].iter() {
        let buildings = generate_small_buildings(*count, &tile_bounds);
        let skip_count = count_fully_inside(&buildings, &tile_bounds);
        let skip_rate = (skip_count as f64 / *count as f64) * 100.0;

        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_buildings_{}%_skip", count, skip_rate as u32)),
            &buildings,
            |b, buildings| {
                b.iter(|| {
                    let mut clipped_count = 0;
                    for building in buildings {
                        let geom = Geometry::Polygon(building.clone());
                        if clip_geometry(
                            black_box(&geom),
                            black_box(&tile_bounds),
                            black_box(buffer),
                        )
                        .is_some()
                        {
                            clipped_count += 1;
                        }
                    }
                    clipped_count
                });
            },
        );
    }

    group.finish();
}

/// Benchmark small buildings that cross tile boundaries (lower skip rate)
fn bench_small_features_boundary_crossing(c: &mut Criterion) {
    let mut group = c.benchmark_group("bbox_containment/boundary_crossing");

    let tile_bounds = TileBounds::new(-122.5, 37.7, -122.4, 37.8);
    let buffer = 0.0001;

    // Generate buildings positioned to cross tile boundaries
    let mut buildings = Vec::new();
    for i in 0..1000 {
        let progress = i as f64 / 1000.0;

        // Position buildings along the tile edges
        let x = if i % 4 == 0 {
            tile_bounds.lng_min - 0.0001 // Crosses left edge
        } else if i % 4 == 1 {
            tile_bounds.lng_max - 0.0001 // Crosses right edge
        } else if i % 4 == 2 {
            tile_bounds.lng_min + (tile_bounds.lng_max - tile_bounds.lng_min) * progress
        } else {
            tile_bounds.lng_min + (tile_bounds.lng_max - tile_bounds.lng_min) * 0.5
        };

        let y = tile_bounds.lat_min + (tile_bounds.lat_max - tile_bounds.lat_min) * progress;

        let size = 0.0002; // Building size
        buildings.push(Polygon::new(
            LineString::from(vec![
                Coord { x, y },
                Coord { x: x + size, y },
                Coord {
                    x: x + size,
                    y: y + size,
                },
                Coord { x, y: y + size },
                Coord { x, y },
            ]),
            vec![],
        ));
    }

    let skip_count = count_fully_inside(&buildings, &tile_bounds);
    let skip_rate = (skip_count as f64 / buildings.len() as f64) * 100.0;

    group.throughput(Throughput::Elements(buildings.len() as u64));
    group.bench_function(format!("1000_buildings_{}%_skip", skip_rate as u32), |b| {
        b.iter(|| {
            let mut clipped_count = 0;
            for building in &buildings {
                let geom = Geometry::Polygon(building.clone());
                if clip_geometry(black_box(&geom), black_box(&tile_bounds), black_box(buffer))
                    .is_some()
                {
                    clipped_count += 1;
                }
            }
            clipped_count
        });
    });

    group.finish();
}

/// Benchmark large features at low zoom (z4) - should show NO benefit
fn bench_large_features_low_zoom(c: &mut Criterion) {
    let mut group = c.benchmark_group("bbox_containment/large_features_z4");

    // Tile at z4 (continental scale)
    let tile_bounds = TileBounds::new(-112.5, 22.5, -67.5, 45.0);
    let buffer = 0.5; // Larger buffer for low zoom

    for count in [10, 50, 100].iter() {
        let boundaries = generate_large_boundaries(*count);
        let skip_count = count_fully_inside(&boundaries, &tile_bounds);
        let skip_rate = (skip_count as f64 / *count as f64) * 100.0;

        group.throughput(Throughput::Elements(*count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_boundaries_{}%_skip", count, skip_rate as u32)),
            &boundaries,
            |b, boundaries| {
                b.iter(|| {
                    let mut clipped_count = 0;
                    for boundary in boundaries {
                        let geom = Geometry::Polygon(boundary.clone());
                        if clip_geometry(
                            black_box(&geom),
                            black_box(&tile_bounds),
                            black_box(buffer),
                        )
                        .is_some()
                        {
                            clipped_count += 1;
                        }
                    }
                    clipped_count
                });
            },
        );
    }

    group.finish();
}

/// Benchmark MultiPolygon bbox filtering (like Antarctica with 7453 sub-polygons)
fn bench_multipolygon_bbox_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("bbox_containment/multipolygon");

    // Small tile that should only intersect a few sub-polygons
    let tile_bounds = TileBounds::new(-67.5, -66.51, -56.25, -61.61);
    let buffer = 0.1;

    for total_polygons in [100, 1000, 5000].iter() {
        // Create a MultiPolygon where only ~5% of sub-polygons intersect the tile
        let mut polygons = Vec::new();

        // 5% inside the tile
        let inside_count = total_polygons / 20;
        for i in 0..inside_count {
            let x = tile_bounds.lng_min
                + (i as f64 * 0.5).rem_euclid(tile_bounds.lng_max - tile_bounds.lng_min);
            let y = tile_bounds.lat_min
                + (i as f64 * 0.3).rem_euclid(tile_bounds.lat_max - tile_bounds.lat_min);
            polygons.push(Polygon::new(
                LineString::from(vec![
                    Coord { x, y },
                    Coord { x: x + 0.1, y },
                    Coord {
                        x: x + 0.1,
                        y: y + 0.1,
                    },
                    Coord { x, y: y + 0.1 },
                    Coord { x, y },
                ]),
                vec![],
            ));
        }

        // 95% outside the tile
        for i in 0..(total_polygons - inside_count) {
            let x = -180.0 + (i as f64 * 0.5);
            let y = -80.0 + (i as f64 * 0.3);
            // Make sure it's actually outside
            if x >= tile_bounds.lng_min
                && x <= tile_bounds.lng_max
                && y >= tile_bounds.lat_min
                && y <= tile_bounds.lat_max
            {
                continue;
            }
            polygons.push(Polygon::new(
                LineString::from(vec![
                    Coord { x, y },
                    Coord { x: x + 0.1, y },
                    Coord {
                        x: x + 0.1,
                        y: y + 0.1,
                    },
                    Coord { x, y: y + 0.1 },
                    Coord { x, y },
                ]),
                vec![],
            ));
        }

        let mp = MultiPolygon::new(polygons);
        let geom = Geometry::MultiPolygon(mp);

        group.throughput(Throughput::Elements(*total_polygons as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{}_subpolygons", total_polygons)),
            &geom,
            |b, geom| {
                b.iter(|| {
                    clip_geometry(black_box(geom), black_box(&tile_bounds), black_box(buffer))
                });
            },
        );
    }

    group.finish();
}

/// Comparison benchmark: small features with vs without optimization
///
/// This manually implements the clipping logic without bbox pre-filter
/// to show the performance difference
fn bench_optimization_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("bbox_containment/comparison");

    let tile_bounds = TileBounds::new(-122.5, 37.7, -122.4, 37.8);
    let buffer = 0.0001;
    let buildings = generate_small_buildings(1000, &tile_bounds);

    // Baseline: current implementation WITH bbox optimization
    group.bench_function("with_bbox_optimization", |b| {
        b.iter(|| {
            let mut clipped_count = 0;
            for building in &buildings {
                let geom = Geometry::Polygon(building.clone());
                if clip_geometry(black_box(&geom), black_box(&tile_bounds), black_box(buffer))
                    .is_some()
                {
                    clipped_count += 1;
                }
            }
            clipped_count
        });
    });

    // Comparison: force clipping even when fully inside (simulates no optimization)
    // Note: This uses the underlying clip_polygon_sh which still has some optimizations
    use gpq_tiles_core::sutherland_hodgman::clip_polygon_sh;

    group.bench_function("without_bbox_optimization", |b| {
        b.iter(|| {
            let mut clipped_count = 0;
            for building in &buildings {
                // Force clipping by using clip_polygon_sh directly
                if clip_polygon_sh(black_box(building), black_box(&tile_bounds)).is_some() {
                    clipped_count += 1;
                }
            }
            clipped_count
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_small_features_high_zoom,
    bench_small_features_boundary_crossing,
    bench_large_features_low_zoom,
    bench_multipolygon_bbox_filter,
    bench_optimization_comparison,
);
criterion_main!(benches);
