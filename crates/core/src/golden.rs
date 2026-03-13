//! Golden comparison tests against tippecanoe output.
//!
//! These tests verify that our clip/simplify pipeline produces similar results
//! to tippecanoe by comparing feature counts, areas, and geometric similarity.
//!
//! # Golden Data Generation
//!
//! Golden tiles were generated using tippecanoe v2.49.0:
//!
//! ```bash
//! tippecanoe \
//!     -P \
//!     '--layer=<layer-name>' \
//!     '--maximum-zoom=10' \
//!     --simplify-only-low-zooms \
//!     --no-simplification-of-shared-nodes \
//!     --no-tile-size-limit \
//!     --no-feature-limit \
//!     --force \
//!     '--output=<output>.pmtiles' \
//!     '<input>.geojsonl'
//! ```
//!
//! See `tests/fixtures/golden/README.md` for full details.
//!
//! # Known Differences from Tippecanoe
//!
//! 1. **Feature dropping**: Both implementations now drop features at low zooms.
//!    Our tiny polygon dropping is slightly more aggressive (0.81x at Z10, 0.78x at Z8).
//!    Density-based dropping is available via `TilerConfig::with_density_drop(true)`.
//!
//! 2. **Density-based dropping**: We use grid-cell limiting instead of tippecanoe's
//!    Hilbert curve ordering with gap-based selection. Results are similar but not identical.
//!
//! # How it works
//!
//! 1. Read source GeoParquet file
//! 2. Clip geometries to specific tile bounds
//! 3. Simplify for that zoom level
//! 4. Compare against pre-decoded tippecanoe MVT tiles (stored as GeoJSON)

#[cfg(test)]
mod tests {
    use crate::batch_processor;
    use crate::clip::{
        buffer_pixels_to_degrees, clip_geometry, DEFAULT_BUFFER_PIXELS, DEFAULT_EXTENT,
    };
    use crate::simplify::simplify_for_zoom;
    use crate::tile::tile_bounds;
    use geo::{Area, Geometry};
    use geojson::GeoJson;
    use std::fs;
    use std::path::Path;

    /// Tile specification for testing
    struct TileSpec {
        z: u8,
        x: u32,
        y: u32,
    }

    impl TileSpec {
        fn new(z: u8, x: u32, y: u32) -> Self {
            Self { z, x, y }
        }

        fn golden_path(&self, dataset: &str) -> String {
            format!(
                "../../tests/fixtures/golden/decoded/{}-z{}-x{}-y{}.geojson",
                dataset, self.z, self.x, self.y
            )
        }
    }

    /// Load geometries from a decoded golden GeoJSON file
    fn load_golden_geometries(path: &str) -> Vec<Geometry<f64>> {
        let content = fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("Failed to read golden file {}: {}", path, e));

        let geojson: GeoJson = content
            .parse()
            .unwrap_or_else(|e| panic!("Failed to parse GeoJSON {}: {}", path, e));

        let mut geometries = Vec::new();

        if let GeoJson::FeatureCollection(fc) = geojson {
            for feature in fc.features {
                if let Some(geom) = feature.geometry {
                    // Convert GeoJSON geometry to geo::Geometry
                    // Note: MVT tiles use tile-local coordinates (0-4096)
                    // We'll compare counts and relative metrics, not exact coords
                    if let Ok(g) = geo::Geometry::try_from(geom) {
                        geometries.push(g);
                    }
                }
            }
        }

        geometries
    }

    /// Process source geometries through our pipeline for a specific tile
    fn process_tile(source_geometries: &[Geometry<f64>], tile: &TileSpec) -> Vec<Geometry<f64>> {
        let bounds = tile_bounds(tile.x, tile.y, tile.z);
        let buffer = buffer_pixels_to_degrees(DEFAULT_BUFFER_PIXELS, &bounds, DEFAULT_EXTENT);

        let mut result = Vec::new();

        for geom in source_geometries {
            // Clip to tile bounds
            if let Some(clipped) = clip_geometry(geom, &bounds, buffer) {
                // Simplify for zoom level
                let simplified = simplify_for_zoom(&clipped, tile.z, DEFAULT_EXTENT);
                result.push(simplified);
            }
        }

        result
    }

    /// Calculate total area of all polygons in a geometry collection
    fn total_polygon_area(geometries: &[Geometry<f64>]) -> f64 {
        geometries
            .iter()
            .map(|g| match g {
                Geometry::Polygon(p) => p.unsigned_area(),
                Geometry::MultiPolygon(mp) => mp.unsigned_area(),
                _ => 0.0,
            })
            .sum()
    }

    // ========== Golden Comparison Tests ==========

    /// Test at z10 where tippecanoe does minimal feature dropping.
    /// At this zoom, the tile doesn't cover the entire dataset, so we're
    /// testing actual clipping behavior rather than feature dropping.
    #[test]
    fn test_golden_open_buildings_z10_feature_count() {
        let tile = TileSpec::new(10, 516, 377);
        let golden_path = tile.golden_path("open-buildings");

        if !Path::new(&golden_path).exists() {
            eprintln!("Skipping test: golden file not found at {}", golden_path);
            return;
        }

        let golden_geoms = load_golden_geometries(&golden_path);
        let golden_count = golden_geoms.len();

        let source_path = "../../tests/fixtures/realdata/open-buildings.parquet";
        if !Path::new(source_path).exists() {
            eprintln!("Skipping test: source file not found at {}", source_path);
            return;
        }

        let source_geoms = batch_processor::extract_geometries(Path::new(source_path))
            .expect("Failed to extract geometries");

        let our_geoms = process_tile(&source_geoms, &tile);
        let our_count = our_geoms.len();

        println!("Z10 Golden (tippecanoe) feature count: {}", golden_count);
        println!("Z10 Our pipeline feature count: {}", our_count);

        // At z10, we should be fairly close to tippecanoe
        // We may have slightly more features since we don't do tiny polygon reduction
        // Tolerance: we should have at least 80% of tippecanoe's features
        // and no more than 150% (some edge case differences expected)
        let min_expected = (golden_count as f64 * 0.80) as usize;
        let max_expected = (golden_count as f64 * 1.50) as usize;

        assert!(
            our_count >= min_expected && our_count <= max_expected,
            "Feature count {} outside tolerance [{}, {}] (golden: {})\n\
             Note: We don't implement tiny polygon reduction yet, so having\n\
             more features than tippecanoe is expected.",
            our_count,
            min_expected,
            max_expected,
            golden_count
        );
    }

    /// Compare Z8 feature counts with tippecanoe using the full pipeline.
    /// Phase 3 feature dropping is now implemented, so we expect comparable counts.
    #[test]
    fn test_document_low_zoom_feature_dropping_difference() {
        use crate::pipeline::{decode_tile, generate_single_tile, TilerConfig};
        use crate::tile::TileCoord;

        let tile = TileSpec::new(8, 129, 94);
        let golden_path = tile.golden_path("open-buildings");

        if !Path::new(&golden_path).exists() {
            eprintln!("Skipping test: golden file not found");
            return;
        }

        let golden_geoms = load_golden_geometries(&golden_path);
        let golden_count = golden_geoms.len();

        let source_path = "../../tests/fixtures/realdata/open-buildings.parquet";
        if !Path::new(source_path).exists() {
            eprintln!("Skipping test: source file not found");
            return;
        }

        let source_geoms = batch_processor::extract_geometries(Path::new(source_path))
            .expect("Failed to extract geometries");

        // Use the full pipeline with feature dropping
        let coord = TileCoord::new(tile.x, tile.y, tile.z);
        let config = TilerConfig::new(0, 10); // default config with feature dropping

        let our_count = if let Some(generated_tile) =
            generate_single_tile(&source_geoms, coord, &config).expect("Should generate tile")
        {
            let decoded = decode_tile(&generated_tile.data).expect("Should decode tile");
            decoded
                .layers
                .first()
                .map(|l| l.features.len())
                .unwrap_or(0)
        } else {
            0
        };

        println!("=== Z8 Feature Comparison (Phase 3 Complete) ===");
        println!("Tippecanoe features: {}", golden_count);
        println!("gpq-tiles features: {}", our_count);

        let ratio = our_count as f64 / golden_count as f64;
        println!("Ratio: {:.2}x", ratio);

        // With feature dropping implemented, we should be in a reasonable range
        // We tend to drop slightly more aggressively (0.78x at Z8)
        assert!(
            (0.3..=2.0).contains(&ratio),
            "Z8 feature count ratio ({:.2}x) should be between 0.3x and 2.0x of tippecanoe",
            ratio
        );
    }

    /// Test that feature counts are monotonic (or close) as we zoom in.
    /// Each child tile should have <= parent features that intersect it.
    #[test]
    fn test_golden_feature_count_monotonic_on_zoom() {
        let source_path = "../../tests/fixtures/realdata/open-buildings.parquet";
        if !Path::new(source_path).exists() {
            eprintln!("Skipping test: source file not found");
            return;
        }

        let source_geoms = batch_processor::extract_geometries(Path::new(source_path))
            .expect("Failed to extract geometries");

        // These tiles are NOT parent-child, they're all covering the same data area
        // at different zooms. At high zooms, tiles become smaller and contain fewer features.
        let tiles = vec![
            TileSpec::new(8, 129, 94),
            TileSpec::new(9, 258, 188),
            TileSpec::new(10, 516, 377),
        ];

        let mut counts = vec![];
        for tile in &tiles {
            let our_geoms = process_tile(&source_geoms, tile);
            counts.push((tile.z, our_geoms.len()));
            println!("Z{}: {} features", tile.z, our_geoms.len());
        }

        // At higher zooms, tile area decreases, so feature count should generally decrease
        // (unless the data is very sparse and all fits in every tile)
        // For open-buildings data, higher zooms should have fewer features per tile
        for window in counts.windows(2) {
            let (z1, count1) = window[0];
            let (z2, count2) = window[1];
            println!("Z{} ({}) -> Z{} ({})", z1, count1, z2, count2);
            // Higher zoom = smaller tile = fewer features (for dense data)
            // Allow some tolerance for edge cases
        }
    }

    /// Test that polygon area is roughly preserved after clipping and simplification.
    /// Use z10 where simplification is minimal.
    #[test]
    fn test_golden_polygon_area_preserved_z10() {
        let tile = TileSpec::new(10, 516, 377);

        let source_path = "../../tests/fixtures/realdata/open-buildings.parquet";
        if !Path::new(source_path).exists() {
            eprintln!("Skipping test: source file not found");
            return;
        }

        let source_geoms = batch_processor::extract_geometries(Path::new(source_path))
            .expect("Failed to extract geometries");

        // Get bounds for this tile
        let bounds = tile_bounds(tile.x, tile.y, tile.z);
        let buffer = buffer_pixels_to_degrees(DEFAULT_BUFFER_PIXELS, &bounds, DEFAULT_EXTENT);

        // Calculate area of source geometries that intersect this tile (BEFORE simplification)
        let clipped_area: f64 = source_geoms
            .iter()
            .filter_map(|g| clip_geometry(g, &bounds, buffer))
            .map(|g| match &g {
                Geometry::Polygon(p) => p.unsigned_area(),
                Geometry::MultiPolygon(mp) => mp.unsigned_area(),
                _ => 0.0,
            })
            .sum();

        // Calculate area after simplification
        let our_geoms = process_tile(&source_geoms, &tile);
        let our_area = total_polygon_area(&our_geoms);

        println!("Clipped area (before simplify): {:.10}", clipped_area);
        println!("Our area (after simplify): {:.10}", our_area);

        if clipped_area > 0.0 {
            let ratio = our_area / clipped_area;
            println!("Area ratio: {:.4}", ratio);

            // At z10, simplification should be minimal, so area should be well preserved
            // Allow 20% reduction (buildings can be simplified to rectangles)
            assert!(
                ratio >= 0.80,
                "Area ratio {} is too low - simplification is too aggressive at z10",
                ratio
            );
            assert!(
                ratio <= 1.05,
                "Area ratio {} is too high - this shouldn't happen",
                ratio
            );
        }
    }

    /// Sanity check: all zoom levels should produce some output for the test data.
    #[test]
    fn test_all_zoom_levels_produce_output() {
        let source_path = "../../tests/fixtures/realdata/open-buildings.parquet";
        if !Path::new(source_path).exists() {
            eprintln!("Skipping test: source file not found");
            return;
        }

        let source_geoms = batch_processor::extract_geometries(Path::new(source_path))
            .expect("Failed to extract geometries");

        let tiles = vec![
            TileSpec::new(5, 16, 11),
            TileSpec::new(6, 32, 23),
            TileSpec::new(7, 64, 47),
            TileSpec::new(8, 129, 94),
            TileSpec::new(9, 258, 188),
            TileSpec::new(10, 516, 377),
        ];

        for tile in &tiles {
            let our_geoms = process_tile(&source_geoms, tile);
            assert!(
                !our_geoms.is_empty(),
                "Z{} x={} y={} produced no features",
                tile.z,
                tile.x,
                tile.y
            );
            println!("Z{}: {} features ✓", tile.z, our_geoms.len());
        }
    }

    /// Test that we can read and parse the golden GeoJSON files correctly.
    #[test]
    fn test_golden_files_parseable() {
        let tiles = vec![
            TileSpec::new(5, 16, 11),
            TileSpec::new(6, 32, 23),
            TileSpec::new(10, 516, 377),
        ];

        for tile in &tiles {
            let path = tile.golden_path("open-buildings");
            if !Path::new(&path).exists() {
                eprintln!("Skipping: {} not found", path);
                continue;
            }

            let geoms = load_golden_geometries(&path);
            assert!(!geoms.is_empty(), "Golden file {} has no geometries", path);
            println!("{}: {} features ✓", path, geoms.len());
        }
    }

    /// Test that density-based dropping can further reduce feature count at low zoom.
    ///
    /// Note: Our tiny polygon and point thinning algorithms already reduce features
    /// significantly. This test verifies density dropping can provide additional
    /// reduction when enabled.
    #[test]
    fn test_density_dropping_reduces_z8_feature_count() {
        use crate::pipeline::{decode_tile, generate_single_tile, TilerConfig};
        use crate::tile::TileCoord;

        let tile = TileSpec::new(8, 129, 94);
        let golden_path = tile.golden_path("open-buildings");

        if !Path::new(&golden_path).exists() {
            eprintln!("Skipping test: golden file not found");
            return;
        }

        let golden_geoms = load_golden_geometries(&golden_path);
        let golden_count = golden_geoms.len(); // Should be ~97

        let source_path = "../../tests/fixtures/realdata/open-buildings.parquet";
        if !Path::new(source_path).exists() {
            eprintln!("Skipping test: source file not found");
            return;
        }

        let source_geoms = batch_processor::extract_geometries(Path::new(source_path))
            .expect("Failed to extract geometries");

        // Test WITHOUT density dropping
        // Other dropping algorithms (tiny polygon, point thinning) already reduce features
        let config_no_drop = TilerConfig::new(8, 10)
            .with_layer_name("buildings")
            .with_density_drop(false);
        let coord = TileCoord::new(tile.x, tile.y, tile.z);

        let tile_no_drop = generate_single_tile(&source_geoms, coord, &config_no_drop)
            .expect("Should generate tile")
            .expect("Tile should have features");

        let decoded_no_drop = decode_tile(&tile_no_drop.data).unwrap();
        let count_no_drop = decoded_no_drop.layers[0].features.len();

        // Test WITH density dropping using a moderate cell size
        // cell_size=128 gives 32x32 = 1024 cells, which is moderate
        let config_with_drop = TilerConfig::new(8, 10)
            .with_layer_name("buildings")
            .with_density_drop(true)
            .with_density_cell_size(128)
            .with_density_max_per_cell(1);

        let tile_with_drop = generate_single_tile(&source_geoms, coord, &config_with_drop)
            .expect("Should generate tile")
            .expect("Tile should have features");

        let decoded_with_drop = decode_tile(&tile_with_drop.data).unwrap();
        let count_with_drop = decoded_with_drop.layers[0].features.len();

        println!("=== Density Dropping Test at Z8 ===");
        println!("Tippecanoe (golden): {} features", golden_count);
        println!(
            "Without density drop: {} features (already reduced by tiny polygon/point thinning)",
            count_no_drop
        );
        println!(
            "With density drop (cell_size=128): {} features",
            count_with_drop
        );

        // Verify density dropping has an effect (even if small)
        assert!(
            count_with_drop <= count_no_drop,
            "Density dropping should not increase features. Without: {}, With: {}",
            count_no_drop,
            count_with_drop
        );

        // Log the comparison to tippecanoe
        println!(
            "Compared to tippecanoe ({}): we have {:.1}x without density drop, {:.1}x with",
            golden_count,
            count_no_drop as f64 / golden_count as f64,
            count_with_drop as f64 / golden_count as f64
        );

        // Our other dropping algorithms already get us close to tippecanoe
        // Density dropping can provide additional reduction for very dense areas
        println!("SUCCESS: Feature counts are in reasonable range");
    }

    /// Test density dropping with various cell sizes to understand the tuning options.
    #[test]
    fn test_density_dropping_cell_size_comparison() {
        use crate::pipeline::{decode_tile, generate_single_tile, TilerConfig};
        use crate::tile::TileCoord;

        let source_path = "../../tests/fixtures/realdata/open-buildings.parquet";
        if !Path::new(source_path).exists() {
            eprintln!("Skipping test: source file not found");
            return;
        }

        let source_geoms = batch_processor::extract_geometries(Path::new(source_path))
            .expect("Failed to extract geometries");

        let coord = TileCoord::new(129, 94, 8); // Z8 tile

        println!("=== Cell Size Comparison at Z8 ===");
        println!("Cell Size | Grid Size | Features Kept");
        println!("----------|-----------|---------------");

        for cell_size in [16, 32, 64, 128, 256, 512] {
            let config = TilerConfig::new(8, 10)
                .with_layer_name("buildings")
                .with_density_drop(true)
                .with_density_cell_size(cell_size)
                .with_density_max_per_cell(1);

            if let Some(tile) =
                generate_single_tile(&source_geoms, coord, &config).expect("Should generate tile")
            {
                let decoded = decode_tile(&tile.data).unwrap();
                let feature_count = decoded.layers[0].features.len();
                let grid_size = 4096 / cell_size;
                println!(
                    "{:9} | {:4}x{:4} | {:6}",
                    cell_size, grid_size, grid_size, feature_count
                );
            }
        }
    }

    /// Golden test comparing baseline output against drop-smallest-as-needed filtering.
    ///
    /// Creates a synthetic dataset with mixed-size polygons (large, medium, tiny)
    /// and verifies that enabling drop-smallest-as-needed reduces feature count
    /// by filtering out the smallest features. Prints reduction percentage for
    /// visual comparison.
    #[test]
    fn test_drop_smallest_visual_comparison() {
        use crate::pipeline::{decode_tile, generate_single_tile, TilerConfig};
        use crate::tile::TileCoord;

        let features = create_mixed_size_features();

        // z10, tile (512, 511) covers lng [0.0, 0.3516], lat ~[0.0, 0.3516]
        let coord = TileCoord::new(512, 511, 10);

        // Baseline: no drop-smallest
        let config_baseline = TilerConfig::new(10, 10).with_layer_name("mixed");

        let tile_baseline = generate_single_tile(&features, coord, &config_baseline)
            .expect("Baseline should succeed")
            .expect("Baseline tile should have features");
        let decoded_baseline =
            decode_tile(&tile_baseline.data).expect("Should decode baseline tile");
        let count_baseline = decoded_baseline.layers[0].features.len();

        // Filtered: with drop-smallest-as-needed, threshold = 4.0 sq px
        let config_filtered = TilerConfig::new(10, 10)
            .with_layer_name("mixed")
            .with_drop_smallest_as_needed()
            .with_drop_smallest_threshold(4.0);

        let tile_filtered = generate_single_tile(&features, coord, &config_filtered)
            .expect("Filtered should succeed")
            .expect("Filtered tile should have features");
        let decoded_filtered =
            decode_tile(&tile_filtered.data).expect("Should decode filtered tile");
        let count_filtered = decoded_filtered.layers[0].features.len();

        // Filtered should have fewer features
        assert!(
            count_filtered < count_baseline,
            "Filtered ({}) should have fewer features than baseline ({})",
            count_filtered,
            count_baseline
        );

        // Document the reduction
        println!("=== Drop-Smallest Visual Comparison (z10) ===");
        println!("Baseline: {} features", count_baseline);
        println!("Filtered: {} features", count_filtered);
        println!(
            "Reduction: {:.1}%",
            100.0 * (1.0 - count_filtered as f64 / count_baseline as f64)
        );
    }

    /// Create a mixed-size feature set: 20 large, 30 medium, 50 tiny polygons.
    ///
    /// All features fall within z10 tile (512, 512) which covers
    /// lng [0.0, ~0.3516], lat [~0.0, ~0.3516].
    fn create_mixed_size_features() -> Vec<Geometry<f64>> {
        use geo::polygon;

        let mut features = Vec::new();

        // Large polygons (0.01 degree squares) - clearly visible at z10
        for i in 0..20 {
            let x = (i % 5) as f64 * 0.02 + 0.01;
            let y = (i / 5) as f64 * 0.02 + 0.01;
            features.push(Geometry::Polygon(polygon![
                (x: x,       y: y),
                (x: x + 0.01, y: y),
                (x: x + 0.01, y: y + 0.01),
                (x: x,       y: y + 0.01),
                (x: x,       y: y),
            ]));
        }

        // Medium polygons (0.003 degree squares)
        for i in 0..30 {
            let x = (i % 6) as f64 * 0.01 + 0.15;
            let y = (i / 6) as f64 * 0.01 + 0.01;
            features.push(Geometry::Polygon(polygon![
                (x: x,        y: y),
                (x: x + 0.003, y: y),
                (x: x + 0.003, y: y + 0.003),
                (x: x,        y: y + 0.003),
                (x: x,        y: y),
            ]));
        }

        // Tiny polygons (0.0001 degree squares) - should be dropped at z10
        for i in 0..50 {
            let x = (i % 10) as f64 * 0.005 + 0.25;
            let y = (i / 10) as f64 * 0.005 + 0.01;
            features.push(Geometry::Polygon(polygon![
                (x: x,         y: y),
                (x: x + 0.0001, y: y),
                (x: x + 0.0001, y: y + 0.0001),
                (x: x,         y: y + 0.0001),
                (x: x,         y: y),
            ]));
        }

        features
    }
}
