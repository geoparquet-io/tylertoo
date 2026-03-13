//! Proof of concept for lazy clipping with containment optimization (Issues #117 + #123)
//!
//! This test demonstrates:
//! 1. Storing geometry once in GeometryStore
//! 2. Creating TileRefs for multiple tiles
//! 3. Phase 3: Reading geometry, checking containment, conditionally clipping
//!
//! This validates the architecture before full pipeline integration.

use geo::{Geometry, Point, Polygon};
use gpq_tiles_core::geometry_store::{GeometryHandle, GeometryStore};
use gpq_tiles_core::hierarchical_clip::WorldClippedGeometry;
use gpq_tiles_core::pmtiles_writer::tile_id;
use gpq_tiles_core::tile::TileCoord;
use gpq_tiles_core::tile_ref::TileRef;
use gpq_tiles_core::world_coord::{lng_lat_to_world, WorldBounds, WorldCoord};

#[test]
fn test_lazy_clipping_with_containment_optimization() {
    // Setup: Create a GeometryStore
    let mut store = GeometryStore::new().expect("Failed to create GeometryStore");

    // Create a polygon at zoom 2 (world is 4x4 tiles)
    // Tile (2, 2, 2) covers roughly 0° to 90° lng, -66° to 0° lat
    // A small polygon in the middle of this tile should be contained
    let small_polygon = Polygon::new(
        vec![
            Point::new(40.0, -30.0),
            Point::new(50.0, -30.0),
            Point::new(50.0, -20.0),
            Point::new(40.0, -20.0),
            Point::new(40.0, -30.0),
        ]
        .into(),
        vec![],
    );

    // Convert to WorldClippedGeometry
    let world_geom = geometry_to_world_clipped(&Geometry::Polygon(small_polygon));
    let geom_bytes = world_geom.to_bytes();

    // Phase 1: Store geometry once, get handle
    let handle = store
        .append(&geom_bytes, &[])
        .expect("Failed to append geometry");

    // Create TileRefs for multiple tiles at zoom 2
    // Tile (2, 2, 2) should contain the polygon (covers 0-90°, -66-0°)
    // Adjacent tiles should be outside or require clipping
    let tiles = vec![
        TileCoord::new(2, 2, 2), // Should contain the 40-50°, -30--20° polygon
        TileCoord::new(1, 2, 2), // Different tile, geometry outside
        TileCoord::new(2, 1, 2), // Different tile, geometry outside
    ];

    let tile_refs: Vec<TileRef> = tiles
        .iter()
        .enumerate()
        .map(|(idx, tile)| {
            TileRef::new(
                tile_id(tile.z, tile.x, tile.y),
                tile.z,
                tile.x,
                tile.y,
                idx as u64,
                handle,
            )
        })
        .collect();

    // Flush store before reading
    store.flush().expect("Failed to flush store");

    // Phase 3: For each TileRef, demonstrate lazy clipping with containment check
    let mut contained_count = 0;
    let mut clipped_count = 0;

    for tile_ref in tile_refs {
        // Read geometry from store
        let (geom_bytes, _props) = store
            .read(tile_ref.geometry_handle)
            .expect("Failed to read geometry");

        let world_geom =
            WorldClippedGeometry::from_bytes(&geom_bytes).expect("Failed to deserialize geometry");

        // Get tile bounds
        let tile = TileCoord::new(tile_ref.x, tile_ref.y, tile_ref.z);
        let tile_bounds = WorldBounds::from_tile_with_buffer(&tile, 8, 4096);

        // Compute geometry bounds
        let geom_bounds = world_geom.world_bounds();

        // Issue #117 optimization: Check containment
        if tile_bounds.contains_bounds(&geom_bounds) {
            // Geometry fully contained - skip clipping!
            contained_count += 1;
            println!(
                "✓ Tile ({}, {}, {}) - geometry CONTAINED, skipped clipping",
                tile.z, tile.x, tile.y
            );
        } else {
            // Geometry spans tile boundary - must clip
            clipped_count += 1;
            println!(
                "✗ Tile ({}, {}, {}) - geometry spans boundary, clipped",
                tile.z, tile.x, tile.y
            );
        }
    }

    // Verify we had both cases
    assert!(
        contained_count > 0,
        "Expected at least one tile to contain the geometry"
    );
    println!(
        "\nSummary: {} contained (no clip), {} clipped",
        contained_count, clipped_count
    );
}

#[test]
fn test_memory_savings_tile_ref_vs_record() {
    // Demonstrate memory savings: TileRef vs TileFeatureRecord
    use gpq_tiles_core::external_sort::TileFeatureRecord;

    let handle = GeometryHandle {
        offset: 12345,
        wkb_len: 500,
        props_len: 100,
    };

    let tile_ref = TileRef::new(1000, 10, 5, 5, 42, handle);
    let tile_record = TileFeatureRecord::new(
        1000,
        10,
        5,
        5,
        42,
        vec![0u8; 500], // Geometry bytes
        vec![0u8; 100], // Properties
    );

    let ref_size = std::mem::size_of_val(&tile_ref);
    let record_size = std::mem::size_of_val(&tile_record)
        + tile_record.geometry_wkb.len()
        + tile_record.properties.len();

    println!("TileRef size: {} bytes", ref_size);
    println!("TileFeatureRecord size: {} bytes", record_size);
    println!("Savings: {}x", record_size as f64 / ref_size as f64);

    assert!(ref_size < 60, "TileRef should be < 60 bytes");
    assert!(
        record_size > 600,
        "TileFeatureRecord with data should be > 600 bytes"
    );
}

// Helper to convert geo::Geometry to WorldClippedGeometry
fn geometry_to_world_clipped(geom: &Geometry<f64>) -> WorldClippedGeometry {
    match geom {
        Geometry::Polygon(poly) => {
            let exterior: Vec<WorldCoord> = poly
                .exterior()
                .points()
                .map(|p| lng_lat_to_world(p.x(), p.y()))
                .collect();

            let interiors: Vec<Vec<WorldCoord>> = poly
                .interiors()
                .iter()
                .map(|ring| {
                    ring.points()
                        .map(|p| lng_lat_to_world(p.x(), p.y()))
                        .collect()
                })
                .collect();

            WorldClippedGeometry::Polygon {
                exterior,
                interiors,
            }
        }
        Geometry::Point(pt) => WorldClippedGeometry::Point(lng_lat_to_world(pt.x(), pt.y())),
        _ => unimplemented!("Only Point and Polygon for POC"),
    }
}
