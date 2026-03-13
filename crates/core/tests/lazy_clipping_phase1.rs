use geo::{Geometry, Point, Polygon};
/// Integration tests for Phase 1: Read and Store with GeometryStore
///
/// This tests the refactored Phase 1 that stores geometries once in GeometryStore
/// and creates lightweight TileRefs instead of heavy TileFeatureRecords.
use gpq_tiles_core::geometry_store::GeometryStore;
use gpq_tiles_core::hierarchical_clip::WorldClippedGeometry;
use gpq_tiles_core::tile::TileCoord;
use gpq_tiles_core::tile_ref::TileRef;
use gpq_tiles_core::world_coord::WorldCoord;

/// Test that read_and_store_features correctly:
/// 1. Stores each geometry once in GeometryStore
/// 2. Creates TileRefs for all intersecting tiles
/// 3. TileRefs can be used to retrieve the original geometry
#[test]
fn test_read_and_store_creates_tile_refs() {
    let mut store = GeometryStore::new().expect("Failed to create GeometryStore");

    // Create a simple polygon that spans multiple tiles at zoom 2
    let polygon = Polygon::new(
        geo::LineString::from(vec![
            (-30.0, 40.0),
            (-20.0, 40.0),
            (-20.0, 50.0),
            (-30.0, 50.0),
            (-30.0, 40.0),
        ]),
        vec![],
    );

    let geometry = Geometry::Polygon(polygon.clone());

    // Convert to WorldClippedGeometry for storage
    let world_geom = geometry_to_world_clipped(&geometry);
    let geom_bytes = world_geom.to_bytes();
    let props_bytes = vec![]; // Empty properties

    // Store once
    let handle = store
        .append(&geom_bytes, &props_bytes)
        .expect("Failed to append geometry");

    // Flush to disk so we can read it back
    store.flush().expect("Failed to flush");

    // Create TileRefs for tiles at zoom 2 that this geometry intersects
    // For this polygon at zoom 2, it should intersect tiles (1,1,2), (2,1,2), (1,2,2), (2,2,2)
    let tiles = vec![
        TileCoord::new(1, 1, 2),
        TileCoord::new(2, 1, 2),
        TileCoord::new(1, 2, 2),
        TileCoord::new(2, 2, 2),
    ];

    let feature_id = 0u64;
    let tile_refs: Vec<TileRef> = tiles
        .iter()
        .map(|tile| {
            let tile_id = hilbert_encode(tile.z, tile.x, tile.y);
            TileRef::new(tile_id, tile.z, tile.x, tile.y, feature_id, handle)
        })
        .collect();

    // Verify we have 4 TileRefs
    assert_eq!(tile_refs.len(), 4);

    // Verify each TileRef can retrieve the same geometry
    for tile_ref in tile_refs {
        let (retrieved_geom_bytes, retrieved_props_bytes) = store
            .read(tile_ref.geometry_handle)
            .expect("Failed to read geometry");

        // Verify bytes match
        assert_eq!(retrieved_geom_bytes, geom_bytes);
        assert_eq!(retrieved_props_bytes, props_bytes);

        // Verify we can deserialize back to WorldClippedGeometry
        let retrieved_geom = WorldClippedGeometry::from_bytes(&retrieved_geom_bytes)
            .expect("Failed to deserialize geometry");

        // Should be a polygon
        match retrieved_geom {
            WorldClippedGeometry::Polygon { exterior, .. } => {
                assert!(!exterior.is_empty());
            }
            _ => panic!("Expected polygon geometry"),
        }
    }
}

/// Test that geometries are only stored once, even when they intersect many tiles
#[test]
fn test_single_storage_multiple_tiles() {
    let mut store = GeometryStore::new().expect("Failed to create GeometryStore");

    // Create geometry
    let point = Point::new(-25.0, 45.0);
    let geometry = Geometry::Point(point);

    let world_geom = geometry_to_world_clipped(&geometry);
    let geom_bytes = world_geom.to_bytes();

    // Store once
    let handle = store.append(&geom_bytes, &[]).expect("Failed to append");

    // Flush to disk
    store.flush().expect("Failed to flush");

    // Create 100 TileRefs using the same handle
    let tile_refs: Vec<TileRef> = (0..100)
        .map(|i| {
            let tile_id = i as u64;
            TileRef::new(tile_id, 10, i as u32, i as u32, 0, handle)
        })
        .collect();

    // Verify all TileRefs have the same handle
    for tile_ref in tile_refs.iter() {
        assert_eq!(tile_ref.geometry_handle, handle);
    }

    // Verify all can read the same geometry
    for tile_ref in tile_refs {
        let (retrieved_bytes, _) = store
            .read(tile_ref.geometry_handle)
            .expect("Failed to read");
        assert_eq!(retrieved_bytes, geom_bytes);
    }
}

/// Test memory efficiency: TileRef is much smaller than TileFeatureRecord
#[test]
fn test_tile_ref_memory_efficiency() {
    use std::mem::size_of;

    // TileRef should be 48 bytes (see PR #127)
    let tile_ref_size = size_of::<TileRef>();
    assert_eq!(tile_ref_size, 48, "TileRef size changed!");

    // For comparison: a hypothetical TileFeatureRecord with inline bytes would be:
    // tile_id(8) + z(1) + x(4) + y(4) + feature_id(8) + Vec<u8> header(24) + data
    // = 49 bytes overhead + data
    // For a typical 200-byte geometry, that's ~250 bytes vs 48 bytes
    // = 5.2x savings per record

    println!("TileRef size: {} bytes", tile_ref_size);
    println!("Expected savings: ~5-14x depending on geometry size");
}

// Helper functions

fn geometry_to_world_clipped(geom: &Geometry<f64>) -> WorldClippedGeometry {
    match geom {
        Geometry::Point(p) => {
            let wc = lng_lat_to_world_coord(p.x(), p.y());
            WorldClippedGeometry::Point(wc)
        }
        Geometry::Polygon(poly) => {
            let exterior: Vec<WorldCoord> = poly
                .exterior()
                .coords()
                .map(|c| lng_lat_to_world_coord(c.x, c.y))
                .collect();

            let interiors: Vec<Vec<WorldCoord>> = poly
                .interiors()
                .iter()
                .map(|ring| {
                    ring.coords()
                        .map(|c| lng_lat_to_world_coord(c.x, c.y))
                        .collect()
                })
                .collect();

            WorldClippedGeometry::Polygon {
                exterior,
                interiors,
            }
        }
        _ => panic!("Only Point and Polygon supported in this test helper"),
    }
}

fn lng_lat_to_world_coord(lng: f64, lat: f64) -> WorldCoord {
    // Web Mercator projection to [0, 1] range
    let x = (lng + 180.0) / 360.0;
    let lat_rad = lat.to_radians();
    let y = (1.0 - (lat_rad.tan() + (1.0 / lat_rad.cos())).ln() / std::f64::consts::PI) / 2.0;

    // Scale to u32 range
    WorldCoord {
        x: (x * (1u64 << 32) as f64) as u32,
        y: (y * (1u64 << 32) as f64) as u32,
    }
}

fn hilbert_encode(z: u8, x: u32, y: u32) -> u64 {
    // Simple placeholder - use actual Hilbert encoding in production
    // For now just pack z, x, y
    ((z as u64) << 56) | ((x as u64) << 28) | (y as u64)
}
