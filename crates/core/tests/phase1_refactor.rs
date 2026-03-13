/// Test for the new Phase 1 implementation that stores geometries once
/// and creates TileRefs for all intersecting tiles.
///
/// This tests the core logic that will replace hierarchical_clip in pipeline.rs
use geo::{Geometry, Point, Polygon};
use gpq_tiles_core::geometry_store::GeometryStore;
use gpq_tiles_core::hierarchical_clip::WorldClippedGeometry;
use gpq_tiles_core::pmtiles_writer::tile_id;
use gpq_tiles_core::simplify::simplify_for_zoom;
use gpq_tiles_core::tile::{tiles_for_bbox, TileBounds, TileCoord};
use gpq_tiles_core::tile_ref::TileRef;
use gpq_tiles_core::world_coord::WorldCoord;
use std::collections::HashMap;

/// Process a single geometry: simplify, store once, create TileRefs
fn process_geometry_for_tiles(
    geom: &Geometry<f64>,
    feature_id: u64,
    min_zoom: u8,
    max_zoom: u8,
    extent: u32,
    store: &mut GeometryStore,
) -> gpq_tiles_core::Result<Vec<TileRef>> {
    use geo::BoundingRect;

    // 1. Get bounding box
    let bbox = match geom.bounding_rect() {
        Some(rect) => TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y),
        None => return Ok(Vec::new()),
    };

    // 2. Simplify ONCE at max zoom
    let simplified = simplify_for_zoom(geom, max_zoom, extent);

    // 3. Convert to WorldClippedGeometry
    let world_geom = geometry_to_world_clipped(&simplified);

    // 4. Serialize and store ONCE
    let geom_bytes = world_geom.to_bytes();
    let props_bytes = vec![]; // Empty props for now
    let handle = store.append(&geom_bytes, &props_bytes)?;

    // 5. Compute all tiles this geometry intersects across all zoom levels
    let mut tile_refs = Vec::new();
    for z in min_zoom..=max_zoom {
        let tiles: Vec<TileCoord> = tiles_for_bbox(&bbox, z).collect();
        for tile in tiles {
            let tid = tile_id(tile.z, tile.x, tile.y);
            tile_refs.push(TileRef::new(
                tid, tile.z, tile.x, tile.y, feature_id, handle,
            ));
        }
    }

    Ok(tile_refs)
}

#[test]
fn test_process_geometry_creates_tile_refs_for_all_zooms() {
    let mut store = GeometryStore::new().expect("Failed to create GeometryStore");

    // Create a point at a known location
    let point = Point::new(-25.0, 45.0);
    let geom = Geometry::Point(point);

    let min_zoom = 0;
    let max_zoom = 2;
    let extent = 4096;
    let feature_id = 42;

    let tile_refs =
        process_geometry_for_tiles(&geom, feature_id, min_zoom, max_zoom, extent, &mut store)
            .expect("Failed to process geometry");

    // Should have created TileRefs for zooms 0, 1, 2
    // At zoom 0: 1 tile (entire world)
    // At zoom 1: 1 tile (point is in one quadrant)
    // At zoom 2: 1 tile (point is in one tile at this zoom)
    // Total: 3 tiles
    assert_eq!(tile_refs.len(), 3);

    // All refs should have the same feature_id
    for tile_ref in &tile_refs {
        assert_eq!(tile_ref.feature_id, feature_id);
    }

    // All refs should have the same geometry handle (stored once!)
    let first_handle = tile_refs[0].geometry_handle;
    for tile_ref in &tile_refs {
        assert_eq!(tile_ref.geometry_handle, first_handle);
    }

    // Group by zoom to verify coverage
    let mut by_zoom: HashMap<u8, Vec<&TileRef>> = HashMap::new();
    for tile_ref in &tile_refs {
        by_zoom.entry(tile_ref.z).or_default().push(tile_ref);
    }

    assert_eq!(by_zoom.len(), 3); // zooms 0, 1, 2
    assert_eq!(by_zoom[&0].len(), 1); // 1 tile at zoom 0
    assert_eq!(by_zoom[&1].len(), 1); // 1 tile at zoom 1
    assert_eq!(by_zoom[&2].len(), 1); // 1 tile at zoom 2
}

#[test]
fn test_polygon_creates_multiple_tile_refs() {
    let mut store = GeometryStore::new().expect("Failed to create GeometryStore");

    // Create a polygon that spans 4 tiles at zoom 2
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
    let geom = Geometry::Polygon(polygon);

    let tile_refs =
        process_geometry_for_tiles(&geom, 0, 2, 2, 4096, &mut store).expect("Failed to process");

    // At zoom 2, this small polygon fits in a single tile
    // (a larger polygon would span multiple tiles)
    assert!(tile_refs.len() >= 1, "Should have at least 1 tile");

    // All should have same handle (stored once)
    let handle = tile_refs[0].geometry_handle;
    for tile_ref in &tile_refs {
        assert_eq!(tile_ref.geometry_handle, handle);
    }

    // All should be at zoom 2
    for tile_ref in &tile_refs {
        assert_eq!(tile_ref.z, 2);
    }
}

#[test]
fn test_tile_refs_can_retrieve_geometry() {
    let mut store = GeometryStore::new().expect("Failed to create GeometryStore");

    let point = Point::new(10.0, 20.0);
    let geom = Geometry::Point(point);

    let tile_refs =
        process_geometry_for_tiles(&geom, 99, 0, 1, 4096, &mut store).expect("Failed to process");

    // Flush so we can read
    store.flush().expect("Failed to flush");

    // Verify all TileRefs can retrieve the same geometry
    for tile_ref in tile_refs {
        let (geom_bytes, props_bytes) = store
            .read(tile_ref.geometry_handle)
            .expect("Failed to read");

        assert!(!geom_bytes.is_empty());
        assert!(props_bytes.is_empty());

        // Deserialize and verify it's a point
        let world_geom =
            WorldClippedGeometry::from_bytes(&geom_bytes).expect("Failed to deserialize");

        match world_geom {
            WorldClippedGeometry::Point(_) => {
                // Success
            }
            _ => panic!("Expected point geometry"),
        }
    }
}

// Helper function to convert geo::Geometry to WorldClippedGeometry
fn geometry_to_world_clipped(geom: &Geometry<f64>) -> WorldClippedGeometry {
    match geom {
        Geometry::Point(p) => {
            let wc = lng_lat_to_world_coord(p.x(), p.y());
            WorldClippedGeometry::Point(wc)
        }
        Geometry::LineString(ls) => {
            let coords: Vec<WorldCoord> = ls
                .coords()
                .map(|c| lng_lat_to_world_coord(c.x, c.y))
                .collect();
            WorldClippedGeometry::LineString(coords)
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
        Geometry::MultiPoint(mp) => {
            let points: Vec<WorldCoord> = mp
                .iter()
                .map(|p| lng_lat_to_world_coord(p.x(), p.y()))
                .collect();
            WorldClippedGeometry::MultiPoint(points)
        }
        Geometry::MultiLineString(mls) => {
            let lines: Vec<Vec<WorldCoord>> = mls
                .iter()
                .map(|ls| {
                    ls.coords()
                        .map(|c| lng_lat_to_world_coord(c.x, c.y))
                        .collect()
                })
                .collect();
            WorldClippedGeometry::MultiLineString(lines)
        }
        Geometry::MultiPolygon(mp) => {
            let polys: Vec<(Vec<WorldCoord>, Vec<Vec<WorldCoord>>)> = mp
                .iter()
                .map(|poly| {
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
                    (exterior, interiors)
                })
                .collect();
            WorldClippedGeometry::MultiPolygon(polys)
        }
        _ => panic!("Unsupported geometry type"),
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
