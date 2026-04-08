//! Integration test for geometry coalescing (#26).
//!
//! Validates the coalescing utilities work together in realistic scenarios:
//! - SpatialGrid assignment
//! - coalesce_geometries merging
//! - CoalesceTargets density tracking
//! - Full "coalesce a tile's worth of features" workflow

use geo::{point, polygon, Geometry, LineString};
use gpq_tiles_core::coalesce::{
    coalesce_geometries, CoalesceConfig, CoalesceResult, CoalesceTargets, GridSize, SpatialGrid,
};
use gpq_tiles_core::tile::TileBounds;
use std::collections::HashMap;

/// Simulate coalescing a dense tile's features using SpatialGrid.
///
/// This tests the full workflow that would happen in encode_tile_from_raw
/// once the TODO is implemented.
#[test]
fn test_coalesce_dense_tile_workflow() {
    // Simulate a tile at z14 covering a small area
    let tile_bounds = TileBounds::new(-0.01, 51.50, 0.01, 51.52);

    // Create 100 points clustered in a small area (dense tile scenario)
    let mut features: Vec<Geometry> = Vec::new();
    for i in 0..100 {
        let x = -0.005 + (i % 10) as f64 * 0.001;
        let y = 51.505 + (i / 10) as f64 * 0.001;
        features.push(Geometry::Point(point!(x: x, y: y)));
    }

    // Create spatial grid for this tile (4x4 fixed)
    let grid_config = GridSize::Fixed(4);
    let grid = SpatialGrid::new(100.0, tile_bounds, &grid_config);

    // Group features by cell
    let mut cells: HashMap<(usize, usize), Vec<Geometry>> = HashMap::new();
    for feature in features {
        if let Some(cell) = grid.assign_cell(&feature) {
            cells.entry(cell).or_default().push(feature);
        }
    }

    println!("100 features distributed into {} cells", cells.len());

    // Coalesce within each cell
    let mut coalesced_features: Vec<Geometry> = Vec::new();
    for (cell_id, cell_features) in cells {
        if cell_features.len() == 1 {
            coalesced_features.push(cell_features.into_iter().next().unwrap());
        } else {
            // Coalesce all features in this cell
            let mut iter = cell_features.into_iter();
            let mut target = iter.next().unwrap();
            for source in iter {
                match coalesce_geometries(&mut target, source) {
                    CoalesceResult::Merged => {}
                    CoalesceResult::TypeMismatch(unmerged) => {
                        // Different type - keep as separate feature
                        coalesced_features.push(unmerged);
                    }
                }
            }
            coalesced_features.push(target);
            println!(
                "Cell {:?}: coalesced into {:?}",
                cell_id,
                coalesced_features.last().unwrap()
            );
        }
    }

    // Verify reduction
    println!(
        "Reduced {} features to {} coalesced features",
        100,
        coalesced_features.len()
    );

    // Should have at most 16 features (one per cell in 4x4 grid)
    assert!(
        coalesced_features.len() <= 16,
        "Expected at most 16 coalesced features (4x4 grid), got {}",
        coalesced_features.len()
    );

    // Each coalesced feature should be a MultiPoint
    for feat in &coalesced_features {
        assert!(
            matches!(feat, Geometry::MultiPoint(_)),
            "Expected MultiPoint, got {:?}",
            feat
        );
    }
}

/// Test mixed geometry types - only same-type features coalesce.
#[test]
fn test_coalesce_mixed_types() {
    let tile_bounds = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
    let grid_config = GridSize::Fixed(4);
    let grid = SpatialGrid::new(5.0, tile_bounds, &grid_config);

    // Create mixed features all in the same cell (center)
    let features: Vec<Geometry> = vec![
        Geometry::Point(point!(x: 0.0, y: 0.0)),
        Geometry::Point(point!(x: 0.1, y: 0.1)),
        Geometry::LineString(LineString::from(vec![(0.0, 0.0), (0.5, 0.5)])),
        Geometry::LineString(LineString::from(vec![(0.1, 0.1), (0.6, 0.6)])),
        Geometry::Polygon(polygon![
            (x: -0.1, y: -0.1),
            (x: 0.1, y: -0.1),
            (x: 0.1, y: 0.1),
            (x: -0.1, y: 0.1),
            (x: -0.1, y: -0.1),
        ]),
    ];

    // Group by cell
    let mut cells: HashMap<(usize, usize), Vec<Geometry>> = HashMap::new();
    for feature in features {
        if let Some(cell) = grid.assign_cell(&feature) {
            cells.entry(cell).or_default().push(feature);
        }
    }

    // All should be in one cell (they're all near center)
    assert_eq!(cells.len(), 1, "All features should be in one cell");

    // Coalesce by type within the cell
    let cell_features = cells.into_values().next().unwrap();

    // Group by geometry type family
    let mut points: Vec<Geometry> = Vec::new();
    let mut lines: Vec<Geometry> = Vec::new();
    let mut polygons: Vec<Geometry> = Vec::new();

    for feat in cell_features {
        match &feat {
            Geometry::Point(_) | Geometry::MultiPoint(_) => points.push(feat),
            Geometry::LineString(_) | Geometry::MultiLineString(_) => lines.push(feat),
            Geometry::Polygon(_) | Geometry::MultiPolygon(_) => polygons.push(feat),
            _ => {}
        }
    }

    // Coalesce each type family
    fn coalesce_family(mut features: Vec<Geometry>) -> Option<Geometry> {
        if features.is_empty() {
            return None;
        }
        let mut target = features.remove(0);
        for source in features {
            let _ = coalesce_geometries(&mut target, source);
        }
        Some(target)
    }

    let coalesced_point = coalesce_family(points);
    let coalesced_line = coalesce_family(lines);
    let coalesced_polygon = coalesce_family(polygons);

    // Verify results
    assert!(matches!(
        coalesced_point,
        Some(Geometry::MultiPoint(ref mp)) if mp.0.len() == 2
    ));
    assert!(matches!(
        coalesced_line,
        Some(Geometry::MultiLineString(ref mls)) if mls.0.len() == 2
    ));
    assert!(matches!(coalesced_polygon, Some(Geometry::Polygon(_))));

    println!("Mixed types coalesced correctly into 3 output features");
}

/// Test CoalesceTargets with realistic density estimation.
#[test]
fn test_coalesce_targets_density_tracking() {
    let mut targets = CoalesceTargets::new();

    // Simulate row groups with varying density
    // Row group 0: 1000 features, sparse
    // Row group 1: 50000 features, DENSE
    // Row group 2: 2000 features, sparse
    // Row group 3: 100000 features, VERY DENSE

    // Mark dense row groups at zoom 14
    targets.mark_dense(1, 14, 50000.0);
    targets.mark_dense(3, 14, 100000.0);

    // Verify lookups
    assert!(
        !targets.should_coalesce(0, 14),
        "Row group 0 should not coalesce"
    );
    assert!(
        targets.should_coalesce(1, 14),
        "Row group 1 should coalesce at z14"
    );
    assert!(
        !targets.should_coalesce(2, 14),
        "Row group 2 should not coalesce"
    );
    assert!(
        targets.should_coalesce(3, 14),
        "Row group 3 should coalesce at z14"
    );

    // Different zoom level should not match
    assert!(
        !targets.should_coalesce(1, 12),
        "Row group 1 should not coalesce at z12"
    );

    println!("CoalesceTargets correctly tracks dense row groups");
}

/// Test CoalesceConfig creation and defaults.
#[test]
fn test_coalesce_config_defaults() {
    let config = CoalesceConfig::new();

    assert_eq!(config.percentile, 90);
    assert_eq!(config.min_density_trigger, 100.0);

    // Custom percentile via builder
    let config75 = CoalesceConfig::new().with_percentile(75);
    assert_eq!(config75.percentile, 75);

    // Custom min_density_trigger via builder
    let config_dense = CoalesceConfig::new().with_min_density(500.0);
    assert_eq!(config_dense.min_density_trigger, 500.0);

    println!("CoalesceConfig defaults and builder methods work correctly");
}

/// Test that SpatialGrid handles edge cases correctly.
#[test]
fn test_spatial_grid_edge_cases() {
    let bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);

    // Test fixed grid sizes
    for size in [4, 8, 16] {
        let grid_config = GridSize::Fixed(size);
        let grid = SpatialGrid::new(100.0, bounds, &grid_config);

        // Point at exact corner
        let corner = Geometry::Point(point!(x: 0.0, y: 0.0));
        let cell = grid.assign_cell(&corner);
        assert!(cell.is_some(), "Corner point should be assigned");

        // Point at exact opposite corner
        let opposite = Geometry::Point(point!(x: 1.0, y: 1.0));
        let cell = grid.assign_cell(&opposite);
        assert!(cell.is_some(), "Opposite corner should be assigned");

        // Point outside bounds - centroid may still assign to clamped cell
        let outside = Geometry::Point(point!(x: 2.0, y: 2.0));
        let cell = grid.assign_cell(&outside);
        println!("Grid {}x{}: outside point cell = {:?}", size, size, cell);
    }

    // Test adaptive grid
    let adaptive = GridSize::Adaptive {
        low: 4,
        high: 8,
        threshold: 500.0,
    };

    // Low density should use 4x4
    let grid_low = SpatialGrid::new(100.0, bounds, &adaptive);
    assert_eq!(grid_low.size(), 4);

    // High density should use 8x8
    let grid_high = SpatialGrid::new(1000.0, bounds, &adaptive);
    assert_eq!(grid_high.size(), 8);

    println!("SpatialGrid edge cases and adaptive sizing handled correctly");
}

/// Benchmark-style test: measure reduction ratio for different densities.
#[test]
fn test_coalesce_reduction_ratios() {
    let bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
    let grid_config = GridSize::Fixed(8); // 8x8 = 64 cells
    let grid = SpatialGrid::new(1000.0, bounds, &grid_config);

    for num_features in [100, 500, 1000, 5000] {
        // Generate random-ish points using golden ratio distribution
        let features: Vec<Geometry> = (0..num_features)
            .map(|i| {
                let x = (i as f64 * 0.618033988749895) % 1.0; // Golden ratio
                let y = (i as f64 * 0.414213562373095) % 1.0; // sqrt(2) - 1
                Geometry::Point(point!(x: x, y: y))
            })
            .collect();

        // Group and count
        let mut cells: HashMap<(usize, usize), usize> = HashMap::new();
        for feature in &features {
            if let Some(cell) = grid.assign_cell(feature) {
                *cells.entry(cell).or_default() += 1;
            }
        }

        let output_features = cells.len();
        let reduction = 1.0 - (output_features as f64 / num_features as f64);

        println!(
            "{} features -> {} coalesced ({:.1}% reduction)",
            num_features,
            output_features,
            reduction * 100.0
        );

        // With 64 cells, we should see significant reduction for high density
        if num_features >= 500 {
            assert!(
                reduction >= 0.5,
                "Expected at least 50% reduction for {} features, got {:.1}%",
                num_features,
                reduction * 100.0
            );
        }
    }
}
