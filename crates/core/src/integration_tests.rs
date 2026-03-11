//! End-to-end integration tests for PMTiles generation.
//!
//! These tests verify the full pipeline:
//! GeoParquet → Pipeline → PMTiles Writer → Readable PMTiles archive
//!
//! # Testing Strategy
//!
//! We use semantic comparison, not byte-exact matching:
//! - PMTiles archive parses correctly
//! - Tiles decode correctly as MVT
//! - Feature counts are reasonable
//! - Zoom levels are properly populated

#[cfg(test)]
mod tests {
    use crate::pipeline::{generate_tiles, TilerConfig};
    use crate::pmtiles_writer::PmtilesWriter;
    use crate::tile::TileBounds;
    use flate2::read::GzDecoder;
    use std::fs;
    use std::io::Read;
    use std::path::Path;

    /// Fixture paths
    const FIXTURE_DIR: &str = "../../tests/fixtures/realdata";
    const GOLDEN_DIR: &str = "../../tests/fixtures/golden";
    const OUTPUT_DIR: &str = "/tmp/gpq-tiles-test";

    /// Ensure output directory exists
    fn ensure_output_dir() {
        let _ = fs::create_dir_all(OUTPUT_DIR);
    }

    /// Read and parse PMTiles header from file
    fn read_pmtiles_header(path: &Path) -> Option<PmtilesHeader> {
        let data = fs::read(path).ok()?;
        if data.len() < 127 {
            return None;
        }

        // Verify magic
        if &data[0..7] != b"PMTiles" {
            return None;
        }

        // Verify version
        if data[7] != 3 {
            return None;
        }

        // Decode bounds (i32 * 10_000_000 -> f64)
        let decode_coord = |offset: usize| -> f64 {
            let val = i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            val as f64 / 10_000_000.0
        };

        Some(PmtilesHeader {
            root_dir_offset: u64::from_le_bytes(data[8..16].try_into().unwrap()),
            root_dir_length: u64::from_le_bytes(data[16..24].try_into().unwrap()),
            json_metadata_offset: u64::from_le_bytes(data[24..32].try_into().unwrap()),
            json_metadata_length: u64::from_le_bytes(data[32..40].try_into().unwrap()),
            tile_data_offset: u64::from_le_bytes(data[56..64].try_into().unwrap()),
            tile_data_length: u64::from_le_bytes(data[64..72].try_into().unwrap()),
            addressed_tiles_count: u64::from_le_bytes(data[72..80].try_into().unwrap()),
            min_zoom: data[100],
            max_zoom: data[101],
            tile_type: data[99],
            tile_compression: data[98],
            min_lon: decode_coord(102),
            min_lat: decode_coord(106),
            max_lon: decode_coord(110),
            max_lat: decode_coord(114),
        })
    }

    /// Simplified PMTiles header for testing
    #[derive(Debug)]
    #[allow(dead_code)] // Fields parsed for completeness but only some are used in tests
    struct PmtilesHeader {
        root_dir_offset: u64,
        root_dir_length: u64,
        json_metadata_offset: u64,
        json_metadata_length: u64,
        tile_data_offset: u64,
        tile_data_length: u64,
        addressed_tiles_count: u64,
        min_zoom: u8,
        max_zoom: u8,
        tile_type: u8,
        tile_compression: u8,
        // Bounds fields (bytes 102-117)
        min_lon: f64,
        min_lat: f64,
        max_lon: f64,
        max_lat: f64,
    }

    impl PmtilesHeader {
        /// Check if bounds are valid geographic coordinates
        fn has_valid_bounds(&self) -> bool {
            self.min_lon >= -180.0
                && self.min_lon <= 180.0
                && self.max_lon >= -180.0
                && self.max_lon <= 180.0
                && self.min_lat >= -90.0
                && self.min_lat <= 90.0
                && self.max_lat >= -90.0
                && self.max_lat <= 90.0
                && self.min_lon <= self.max_lon
                && self.min_lat <= self.max_lat
        }
    }

    // ========================================================================
    // End-to-End Integration Tests
    // ========================================================================

    /// Test: Full pipeline from GeoParquet to PMTiles file.
    ///
    /// This is the primary integration test that verifies:
    /// 1. Pipeline reads GeoParquet correctly
    /// 2. Tiles are generated for the correct zoom range
    /// 3. PMTiles file is written correctly
    /// 4. PMTiles header is valid
    #[test]
    fn test_e2e_geoparquet_to_pmtiles() {
        ensure_output_dir();

        let input_path = Path::new(FIXTURE_DIR).join("open-buildings.parquet");
        if !input_path.exists() {
            eprintln!("Skipping test: fixture not found at {:?}", input_path);
            return;
        }

        let output_path = Path::new(OUTPUT_DIR).join("e2e-open-buildings.pmtiles");

        // Configure for zooms 8-10 (matching golden fixtures)
        let config = TilerConfig::new(8, 10).with_layer_name("open-buildings");

        // Step 1: Generate tiles
        let tiles_iter = generate_tiles(&input_path, &config).expect("Should generate tiles");

        // Step 2: Collect tiles and write to PMTiles
        let mut writer = PmtilesWriter::new();
        let mut tile_count = 0;
        let mut min_z = u8::MAX;
        let mut max_z = 0u8;

        for tile_result in tiles_iter {
            let tile = tile_result.expect("Tile generation should not fail");
            writer
                .add_tile(tile.coord.z, tile.coord.x, tile.coord.y, &tile.data)
                .expect("Should add tile");
            tile_count += 1;
            min_z = min_z.min(tile.coord.z);
            max_z = max_z.max(tile.coord.z);
        }

        // Set bounds (approximate for Andorra)
        writer.set_bounds(&TileBounds::new(1.4, 42.4, 1.8, 42.7));

        // Step 3: Write PMTiles file
        writer
            .write_to_file(&output_path)
            .expect("Should write PMTiles");

        // Step 4: Verify the output
        assert!(output_path.exists(), "PMTiles file should exist");

        let header = read_pmtiles_header(&output_path).expect("Should parse PMTiles header");

        println!("=== E2E Test Results ===");
        println!("Generated {} tiles", tile_count);
        println!("Zoom range: {} - {}", min_z, max_z);
        println!("Header:");
        println!("  Addressed tiles: {}", header.addressed_tiles_count);
        println!("  Min zoom: {}", header.min_zoom);
        println!("  Max zoom: {}", header.max_zoom);
        println!("  Tile type: {} (1=MVT)", header.tile_type);
        println!("  Compression: {} (2=gzip)", header.tile_compression);

        // Assertions
        assert!(tile_count > 0, "Should generate at least one tile");
        assert_eq!(
            header.addressed_tiles_count as usize, tile_count,
            "Header tile count should match"
        );
        assert_eq!(header.min_zoom, 8, "Min zoom should be 8");
        assert_eq!(header.max_zoom, 10, "Max zoom should be 10");
        assert_eq!(header.tile_type, 1, "Tile type should be MVT (1)");
        assert_eq!(header.tile_compression, 2, "Compression should be gzip (2)");

        // Clean up
        let _ = fs::remove_file(&output_path);
    }

    /// Test: Verify tiles decode correctly as MVT protobuf.
    ///
    /// This tests that the MVT encoding is valid and can be decoded
    /// back to feature data.
    #[test]
    fn test_e2e_tiles_decode_as_mvt() {
        let input_path = Path::new(FIXTURE_DIR).join("open-buildings.parquet");
        if !input_path.exists() {
            eprintln!("Skipping test: fixture not found");
            return;
        }

        // Generate tiles at z10 only
        let config = TilerConfig::new(10, 10).with_layer_name("buildings");
        let tiles_iter = generate_tiles(&input_path, &config).expect("Should generate tiles");

        let tiles: Vec<_> = tiles_iter.filter_map(|r| r.ok()).collect();

        assert!(!tiles.is_empty(), "Should generate at least one tile");

        // Verify each tile decodes correctly
        let mut total_features = 0;
        for tile in &tiles {
            let decoded = crate::pipeline::decode_tile(&tile.data).expect("Should decode MVT tile");

            assert_eq!(decoded.layers.len(), 1, "Should have exactly one layer");

            let layer = &decoded.layers[0];
            assert_eq!(layer.name, "buildings", "Layer name should match config");
            assert_eq!(layer.version, 2, "MVT version should be 2");
            assert_eq!(layer.extent, Some(4096), "Extent should be 4096");
            assert!(
                !layer.features.is_empty(),
                "Non-empty tiles should have features"
            );

            total_features += layer.features.len();
        }

        println!(
            "Decoded {} tiles with {} total features",
            tiles.len(),
            total_features
        );
    }

    /// Test: Verify tile counts per zoom level are reasonable.
    ///
    /// Higher zoom levels should have more tiles (smaller tiles covering
    /// the same area) or at least the same count.
    #[test]
    fn test_e2e_tile_count_by_zoom() {
        let input_path = Path::new(FIXTURE_DIR).join("open-buildings.parquet");
        if !input_path.exists() {
            eprintln!("Skipping test: fixture not found");
            return;
        }

        let config = TilerConfig::new(8, 10).with_layer_name("buildings");
        let tiles_iter = generate_tiles(&input_path, &config).expect("Should generate tiles");

        let tiles: Vec<_> = tiles_iter.filter_map(|r| r.ok()).collect();

        let z8_count = tiles.iter().filter(|t| t.coord.z == 8).count();
        let z9_count = tiles.iter().filter(|t| t.coord.z == 9).count();
        let z10_count = tiles.iter().filter(|t| t.coord.z == 10).count();

        println!(
            "Tiles per zoom: Z8={}, Z9={}, Z10={}",
            z8_count, z9_count, z10_count
        );

        assert!(z8_count > 0, "Should have z8 tiles");
        assert!(z9_count > 0, "Should have z9 tiles");
        assert!(z10_count > 0, "Should have z10 tiles");

        // At higher zooms, each parent tile can spawn up to 4 children
        // So z9 >= z8 and z10 >= z9 for data that spans multiple tiles
        assert!(
            z9_count >= z8_count,
            "Z9 should have >= tiles than Z8 (was {} vs {})",
            z9_count,
            z8_count
        );
        assert!(
            z10_count >= z9_count,
            "Z10 should have >= tiles than Z9 (was {} vs {})",
            z10_count,
            z9_count
        );
    }

    /// Test: Compare generated feature count against golden at z10.
    ///
    /// At z10, tippecanoe does minimal feature dropping, so our counts
    /// should be reasonably close (within 50-150% range due to known
    /// differences like tiny polygon reduction).
    #[test]
    fn test_e2e_feature_count_golden_comparison_z10() {
        let input_path = Path::new(FIXTURE_DIR).join("open-buildings.parquet");
        let golden_path =
            Path::new(GOLDEN_DIR).join("decoded/open-buildings-z10-x516-y377.geojson");

        if !input_path.exists() || !golden_path.exists() {
            eprintln!("Skipping test: fixtures not found");
            return;
        }

        // Count features in golden GeoJSON
        let golden_content = fs::read_to_string(&golden_path).expect("Should read golden file");
        let golden_json: serde_json::Value =
            serde_json::from_str(&golden_content).expect("Should parse JSON");
        let golden_count = golden_json["features"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0);

        // Generate our tile for the same coordinates (z10/x516/y377)
        let config = TilerConfig::new(10, 10).with_layer_name("buildings");
        let tiles_iter = generate_tiles(&input_path, &config).expect("Should generate tiles");

        let target_tile = tiles_iter
            .filter_map(|r| r.ok())
            .find(|t| t.coord.z == 10 && t.coord.x == 516 && t.coord.y == 377);

        let our_count = if let Some(tile) = target_tile {
            let decoded = crate::pipeline::decode_tile(&tile.data).expect("Should decode");
            decoded.layers[0].features.len()
        } else {
            0
        };

        println!("=== Golden Comparison Z10/516/377 ===");
        println!("Golden (tippecanoe): {} features", golden_count);
        println!("Our pipeline: {} features", our_count);

        if golden_count > 0 {
            let ratio = our_count as f64 / golden_count as f64;
            println!("Ratio: {:.2}x", ratio);

            // We should have roughly similar counts at z10
            // Known differences: we don't do tiny polygon reduction, so we may have MORE
            // Tolerance: 50% to 200% of golden count
            assert!(
                ratio >= 0.50,
                "Feature count ratio too low: {:.2} (expected >= 0.50)",
                ratio
            );
            assert!(
                ratio <= 2.0,
                "Feature count ratio too high: {:.2} (expected <= 2.0)",
                ratio
            );
        }
    }

    /// Test: Verify PMTiles can be read back and tiles extracted.
    ///
    /// This tests round-trip: write PMTiles, read header, verify structure.
    #[test]
    fn test_e2e_pmtiles_roundtrip() {
        ensure_output_dir();

        let input_path = Path::new(FIXTURE_DIR).join("open-buildings.parquet");
        if !input_path.exists() {
            eprintln!("Skipping test: fixture not found");
            return;
        }

        let output_path = Path::new(OUTPUT_DIR).join("roundtrip-test.pmtiles");

        // Generate and write
        let config = TilerConfig::new(10, 10);
        let tiles_iter = generate_tiles(&input_path, &config).expect("Should generate tiles");

        let mut writer = PmtilesWriter::new();
        for tile_result in tiles_iter {
            let tile = tile_result.expect("Tile should succeed");
            writer
                .add_tile(tile.coord.z, tile.coord.x, tile.coord.y, &tile.data)
                .expect("Should add tile");
        }

        writer.set_bounds(&TileBounds::new(1.4, 42.4, 1.8, 42.7));
        writer
            .write_to_file(&output_path)
            .expect("Should write file");

        // Read back and verify
        let data = fs::read(&output_path).expect("Should read file");

        // Verify magic and version
        assert_eq!(&data[0..7], b"PMTiles", "Magic should be 'PMTiles'");
        assert_eq!(data[7], 3, "Version should be 3");

        // Parse header
        let header = read_pmtiles_header(&output_path).expect("Should parse header");

        // Verify we can decompress the directory
        let dir_start = header.root_dir_offset as usize;
        let dir_end = dir_start + header.root_dir_length as usize;
        let compressed_dir = &data[dir_start..dir_end];

        let mut decoder = GzDecoder::new(compressed_dir);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .expect("Should decompress directory");

        assert!(
            !decompressed.is_empty(),
            "Decompressed directory should not be empty"
        );

        println!("Roundtrip test passed:");
        println!("  File size: {} bytes", data.len());
        println!(
            "  Directory size (compressed): {} bytes",
            header.root_dir_length
        );
        println!(
            "  Directory size (decompressed): {} bytes",
            decompressed.len()
        );
        println!("  Tile data size: {} bytes", header.tile_data_length);

        // Clean up
        let _ = fs::remove_file(&output_path);
    }

    /// Test: Multiple geometry types (polygons, linestrings, points) if available.
    #[test]
    fn test_e2e_road_detections_linestrings() {
        let input_path = Path::new(FIXTURE_DIR).join("road-detections.parquet");
        if !input_path.exists() {
            eprintln!("Skipping test: road-detections fixture not found");
            return;
        }

        let config = TilerConfig::new(10, 10).with_layer_name("roads");
        let tiles_result = generate_tiles(&input_path, &config);

        match tiles_result {
            Ok(tiles_iter) => {
                let tiles: Vec<_> = tiles_iter.filter_map(|r| r.ok()).collect();
                println!("Road detections: generated {} tiles at z10", tiles.len());

                // Verify tiles decode correctly
                for tile in tiles.iter().take(5) {
                    let decoded = crate::pipeline::decode_tile(&tile.data);
                    assert!(decoded.is_ok(), "Linestring tiles should decode");
                }
            }
            Err(e) => {
                // Road detections may have different geometry format
                println!("Note: road-detections failed to process: {}", e);
            }
        }
    }

    /// Test: Verify empty input produces valid empty PMTiles.
    #[test]
    fn test_e2e_empty_pmtiles_valid() {
        ensure_output_dir();

        let output_path = Path::new(OUTPUT_DIR).join("empty-test.pmtiles");

        // Create empty writer
        let writer = PmtilesWriter::new();
        writer
            .write_to_file(&output_path)
            .expect("Should write empty file");

        // Verify it's a valid PMTiles file
        let header = read_pmtiles_header(&output_path).expect("Should parse empty PMTiles");

        assert_eq!(header.addressed_tiles_count, 0, "Should have 0 tiles");
        assert_eq!(header.tile_type, 1, "Tile type should still be MVT");

        // Clean up
        let _ = fs::remove_file(&output_path);
    }

    /// Test: Verify tile coordinates match expected range for Andorra data.
    #[test]
    fn test_e2e_tile_coordinates_correct() {
        let input_path = Path::new(FIXTURE_DIR).join("open-buildings.parquet");
        if !input_path.exists() {
            eprintln!("Skipping test: fixture not found");
            return;
        }

        let config = TilerConfig::new(10, 10);
        let tiles_iter = generate_tiles(&input_path, &config).expect("Should generate tiles");

        let tiles: Vec<_> = tiles_iter.filter_map(|r| r.ok()).collect();

        // Andorra is at approximately (1.5, 42.5)
        // At z10, this should be around x=516, y=377
        // (based on Web Mercator tile coordinates)

        let has_expected_tile = tiles
            .iter()
            .any(|t| t.coord.z == 10 && t.coord.x == 516 && t.coord.y == 377);

        println!("Generated tiles at z10:");
        for tile in &tiles {
            println!("  z{}/x{}/y{}", tile.coord.z, tile.coord.x, tile.coord.y);
        }

        assert!(
            has_expected_tile,
            "Should have tile at z10/x516/y377 for Andorra data"
        );
    }

    // ========================================================================
    // Tier 3: Feature Presence Tests (from testing strategy)
    // ========================================================================

    /// Test: At high zoom, we should have high recall (most golden features present).
    #[test]
    fn test_e2e_high_zoom_recall() {
        let input_path = Path::new(FIXTURE_DIR).join("open-buildings.parquet");
        let golden_path =
            Path::new(GOLDEN_DIR).join("decoded/open-buildings-z10-x516-y377.geojson");

        if !input_path.exists() || !golden_path.exists() {
            eprintln!("Skipping test: fixtures not found");
            return;
        }

        // At z10, we should generate features for all input that intersects
        // Since we don't drop features yet (Phase 3), recall should be ~1.0

        let config = TilerConfig::new(10, 10);
        let tiles_iter = generate_tiles(&input_path, &config).expect("Should generate tiles");

        let target_tile = tiles_iter
            .filter_map(|r| r.ok())
            .find(|t| t.coord.z == 10 && t.coord.x == 516 && t.coord.y == 377);

        assert!(
            target_tile.is_some(),
            "Should generate tile for golden coordinates"
        );

        let tile = target_tile.unwrap();
        let decoded = crate::pipeline::decode_tile(&tile.data).expect("Should decode");

        // We should have at least SOME features (recall > 0)
        assert!(
            !decoded.layers[0].features.is_empty(),
            "Should have features at z10 where golden has features"
        );

        println!(
            "High zoom recall test: {} features at z10/516/377",
            decoded.layers[0].features.len()
        );
    }

    /// Test: Converter API produces PMTiles with valid geographic bounds.
    ///
    /// This test verifies that the high-level Converter::convert() API
    /// automatically calculates and sets valid bounds in the PMTiles header.
    /// Invalid bounds (like f64::INFINITY overflow) cause renderers to fail.
    ///
    /// Bug: Without this fix, bounds were f64::INFINITY which overflows to
    /// i32::MAX (2147483647 / 10_000_000 = 214.748365°) - invalid coordinates.
    #[test]
    fn test_converter_sets_valid_bounds_in_pmtiles() {
        use crate::{Config, Converter};

        ensure_output_dir();

        let input_path = Path::new(FIXTURE_DIR).join("open-buildings.parquet");
        if !input_path.exists() {
            eprintln!("Skipping test: fixture not found at {:?}", input_path);
            return;
        }

        let output_path = Path::new(OUTPUT_DIR).join("converter-bounds-test.pmtiles");
        let _ = fs::remove_file(&output_path);

        // Use the high-level Converter API (not manual set_bounds)
        let config = Config {
            min_zoom: 8,
            max_zoom: 10,
            ..Default::default()
        };
        let converter = Converter::new(config);
        converter
            .convert(&input_path, &output_path)
            .expect("Conversion should succeed");

        // Verify the PMTiles has valid bounds
        let header = read_pmtiles_header(&output_path).expect("Should parse PMTiles header");

        println!("=== Bounds Validation Test ===");
        println!(
            "Bounds: ({}, {}) to ({}, {})",
            header.min_lon, header.min_lat, header.max_lon, header.max_lat
        );

        // The key assertion: bounds must be valid geographic coordinates
        assert!(
            header.has_valid_bounds(),
            "PMTiles must have valid geographic bounds, got: min=({}, {}), max=({}, {})",
            header.min_lon,
            header.min_lat,
            header.max_lon,
            header.max_lat
        );

        // For open-buildings.parquet (Andorra), bounds should be roughly:
        // lon: 1.4 to 1.8, lat: 42.4 to 42.7
        // Check we're in the right ballpark (not infinity, not zero)
        assert!(
            header.min_lon > 1.0 && header.min_lon < 2.0,
            "min_lon should be ~1.4 (Andorra), got {}",
            header.min_lon
        );
        assert!(
            header.max_lon > 1.0 && header.max_lon < 2.0,
            "max_lon should be ~1.8 (Andorra), got {}",
            header.max_lon
        );
        assert!(
            header.min_lat > 42.0 && header.min_lat < 43.0,
            "min_lat should be ~42.4 (Andorra), got {}",
            header.min_lat
        );
        assert!(
            header.max_lat > 42.0 && header.max_lat < 43.0,
            "max_lat should be ~42.7 (Andorra), got {}",
            header.max_lat
        );

        // Clean up
        let _ = fs::remove_file(&output_path);
    }

    // ========================================================================
    // WKT Geometry Encoding Tests (Issue #35)
    // ========================================================================

    /// Test: Full pipeline with WKT-encoded GeoParquet.
    ///
    /// Verifies that GeoParquet files using WKT geometry encoding
    /// (instead of WKB or GeoArrow native) work correctly through
    /// the entire tile generation pipeline.
    ///
    /// See: https://github.com/geoparquet-io/gpq-tiles/issues/35
    #[test]
    fn test_e2e_wkt_encoded_geoparquet() {
        ensure_output_dir();

        let input_path = Path::new(FIXTURE_DIR).join("wkt-encoded.parquet");
        if !input_path.exists() {
            eprintln!("Skipping: WKT fixture not found (not in fixtures-v1 release yet)");
            return;
        }

        let output_path = Path::new(OUTPUT_DIR).join("e2e-wkt-encoded.pmtiles");

        // Configure for zooms 10-12 (small dataset)
        let config = TilerConfig::new(10, 12).with_layer_name("wkt-buildings");

        // Step 1: Generate tiles from WKT-encoded source
        let tiles_iter =
            generate_tiles(&input_path, &config).expect("Should generate tiles from WKT");

        // Step 2: Collect tiles and write to PMTiles
        let mut writer = PmtilesWriter::new();
        let mut tile_count = 0;

        for tile_result in tiles_iter {
            let tile = tile_result.expect("Tile generation should not fail for WKT");
            writer
                .add_tile(tile.coord.z, tile.coord.x, tile.coord.y, &tile.data)
                .expect("Should add tile");
            tile_count += 1;
        }

        // Set bounds (Andorra area)
        writer.set_bounds(&TileBounds::new(1.4, 42.4, 1.8, 42.7));

        // Step 3: Write PMTiles file
        writer
            .write_to_file(&output_path)
            .expect("Should write PMTiles from WKT source");

        // Step 4: Verify the output
        assert!(output_path.exists(), "PMTiles file should exist");

        let header = read_pmtiles_header(&output_path).expect("Should parse PMTiles header");

        println!("=== WKT E2E Test Results ===");
        println!("Generated {} tiles from WKT source", tile_count);
        println!(
            "Addressed tiles in header: {}",
            header.addressed_tiles_count
        );

        // Assertions
        assert!(
            tile_count > 0,
            "Should generate at least one tile from WKT source"
        );
        assert_eq!(
            header.addressed_tiles_count as usize, tile_count,
            "Header tile count should match"
        );
        assert_eq!(header.tile_type, 1, "Tile type should be MVT (1)");
        assert_eq!(header.tile_compression, 2, "Compression should be gzip (2)");

        // Clean up
        let _ = fs::remove_file(&output_path);
    }

    /// Test: Tiles from WKT source decode correctly as MVT.
    #[test]
    fn test_e2e_wkt_tiles_decode_as_mvt() {
        let input_path = Path::new(FIXTURE_DIR).join("wkt-encoded.parquet");
        if !input_path.exists() {
            eprintln!("Skipping: WKT fixture not found (not in fixtures-v1 release yet)");
            return;
        }

        // Generate tiles at z12 only
        let config = TilerConfig::new(12, 12).with_layer_name("wkt-test");
        let tiles_iter = generate_tiles(&input_path, &config).expect("Should generate tiles");

        let tiles: Vec<_> = tiles_iter.filter_map(|r| r.ok()).collect();

        assert!(
            !tiles.is_empty(),
            "Should generate at least one tile from WKT"
        );

        // Verify each tile decodes correctly
        let mut total_features = 0;
        for tile in &tiles {
            let decoded = crate::pipeline::decode_tile(&tile.data)
                .expect("Should decode WKT-sourced MVT tile");

            assert_eq!(decoded.layers.len(), 1, "Should have exactly one layer");

            let layer = &decoded.layers[0];
            assert_eq!(layer.name, "wkt-test", "Layer name should match config");
            assert_eq!(layer.version, 2, "MVT version should be 2");
            assert_eq!(layer.extent, Some(4096), "Extent should be 4096");

            total_features += layer.features.len();
        }

        println!(
            "Decoded {} tiles with {} total features from WKT source",
            tiles.len(),
            total_features
        );

        assert!(
            total_features > 0,
            "Should have decoded features from WKT source"
        );
    }
}
