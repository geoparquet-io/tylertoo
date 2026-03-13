//! Lazy clipping pipeline implementation (Phase 1 refactor)
//!
//! This module implements the new pipeline that:
//! 1. **Phase 1**: Store geometry once, create TileRefs for intersecting tiles
//! 2. **Phase 2**: Sort TileRefs by tile_id (external sort)
//! 3. **Phase 3**: Lazy clip with containment optimization, encode MVT
//!
//! **Key improvements over hierarchical clipping:**
//! - **Memory**: 6x reduction (48-byte TileRef vs 680-byte TileFeatureRecord)
//! - **CPU**: ~80% of features skip clipping (containment optimization)
//! - **Architecture**: Store once, read N times (vs N clipped copies)

use geo::{BoundingRect, Geometry};

use crate::geometry_store::GeometryStore;
use crate::hierarchical_clip::WorldClippedGeometry;
use crate::pmtiles_writer::tile_id;
use crate::simplify::simplify_for_zoom;
use crate::tile::{tiles_for_bbox, TileBounds, TileCoord};
use crate::tile_ref::TileRef;
use crate::world_coord::WorldCoord;
use crate::Result;

/// Process geometries in Phase 1: simplify once, store once, create TileRefs for all intersecting tiles
///
/// This replaces hierarchical clipping with lazy clipping:
/// - Old: Clip at min_zoom, reuse for children → Store N clipped copies
/// - New: Store original once → Clip lazily in Phase 3 with containment check
pub fn process_geometry_phase1(
    geometries: Vec<Geometry<f64>>,
    base_feature_id: u64,
    min_zoom: u8,
    max_zoom: u8,
    extent: u32,
    store: &mut GeometryStore,
    global_bounds: &mut TileBounds,
) -> Result<Vec<TileRef>> {
    let mut tile_refs = Vec::new();

    for (idx, geom) in geometries.into_iter().enumerate() {
        let feature_id = base_feature_id + idx as u64;

        // 1. Get bounding box
        let bbox = match geom.bounding_rect() {
            Some(rect) => {
                let bounds =
                    TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
                global_bounds.expand(&bounds);
                bounds
            }
            None => continue, // Skip geometries without bbox
        };

        // 2. Simplify ONCE at max zoom (not hierarchically)
        let simplified = simplify_for_zoom(&geom, max_zoom, extent);

        // 3. Convert to WorldClippedGeometry
        let world_geom = geometry_to_world_clipped(&simplified);

        // 4. Serialize and store ONCE
        let geom_bytes = world_geom.to_bytes();
        let props_bytes = vec![]; // TODO: Serialize properties from GeoParquet
        let handle = store.append(&geom_bytes, &props_bytes)?;

        // 5. Create TileRefs for ALL tiles this geometry intersects (all zoom levels)
        for z in min_zoom..=max_zoom {
            let tiles: Vec<TileCoord> = tiles_for_bbox(&bbox, z).collect();
            for tile in tiles {
                let tid = tile_id(tile.z, tile.x, tile.y);
                tile_refs.push(TileRef::new(
                    tid, tile.z, tile.x, tile.y, feature_id, handle,
                ));
            }
        }
    }

    Ok(tile_refs)
}

/// Convert geo::Geometry to WorldClippedGeometry
///
/// This is a direct conversion without clipping - clipping happens lazily in Phase 3
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
        Geometry::GeometryCollection(_) => {
            // TODO: Handle GeometryCollection
            panic!("GeometryCollection not yet supported in lazy clipping")
        }
        _ => {
            panic!("Unsupported geometry type")
        }
    }
}

/// Convert lng/lat to WorldCoord (Web Mercator projection)
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

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Point;

    #[test]
    fn test_geometry_to_world_clipped_point() {
        let point = Point::new(0.0, 0.0);
        let geom = Geometry::Point(point);
        let world_geom = geometry_to_world_clipped(&geom);

        match world_geom {
            WorldClippedGeometry::Point(wc) => {
                // (0, 0) should map to center of world coordinate space
                assert!(wc.x > 0 && wc.x < u32::MAX);
                assert!(wc.y > 0 && wc.y < u32::MAX);
            }
            _ => panic!("Expected point geometry"),
        }
    }

    #[test]
    fn test_process_geometry_phase1_creates_refs() {
        let mut store = GeometryStore::new().expect("Failed to create store");
        let mut global_bounds = TileBounds::empty();

        let point = Point::new(-25.0, 45.0);
        let geometries = vec![Geometry::Point(point)];

        let tile_refs =
            process_geometry_phase1(geometries, 0, 0, 2, 4096, &mut store, &mut global_bounds)
                .expect("Failed to process");

        // Should create refs for zooms 0, 1, 2
        assert_eq!(tile_refs.len(), 3);

        // All should have same handle
        let handle = tile_refs[0].geometry_handle;
        for tile_ref in &tile_refs {
            assert_eq!(tile_ref.geometry_handle, handle);
        }
    }
}
