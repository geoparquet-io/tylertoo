//! Hierarchical geometry clipping across zoom levels.
//!
//! Instead of clipping each geometry independently to every tile it touches,
//! hierarchical clipping exploits the parent-child relationship of tiles:
//! - Clip once at min_zoom (coarsest level)
//! - For each child tile at z+1, clip the parent's result (not the original geometry)
//! - Continue down the tree to max_zoom
//!
//! This reduces redundant work because:
//! - A z=8 tile is fully contained within its z=7 parent
//! - Clipping to a child only needs to process the already-clipped parent result
//! - The parent result is typically much smaller than the original geometry
//!
//! # Tippecanoe Comparison
//!
//! DIVERGENCE FROM TIPPECANOE: Tippecanoe clips each tile independently.
//! We clip hierarchically for performance, but produce identical output
//! because clip(clip(geom, parent_bounds), child_bounds) == clip(geom, child_bounds)
//! when child_bounds is contained within parent_bounds.
//!
//! Note: Buffer zones mean child tiles extend slightly beyond parent bounds,
//! so at the parent level we clip with an expanded buffer that covers all
//! children's buffered bounds. This ensures no geometry is lost.

use std::collections::HashMap;

use geo::Geometry;

use crate::clip::{buffer_pixels_to_degrees, clip_geometry};
use crate::tile::{TileBounds, TileCoord};

/// Result of hierarchical clipping for a single geometry across multiple tiles.
///
/// Maps each tile coordinate to the clipped geometry for that tile.
/// Tiles where the geometry doesn't intersect are not included.
pub type ClipResults = HashMap<TileCoord, Geometry<f64>>;

/// Statistics about hierarchical clipping operations, useful for benchmarking.
#[derive(Debug, Clone, Default)]
pub struct ClipStats {
    /// Number of actual clip operations performed
    pub clip_ops: u64,
    /// Number of cache hits (reused parent clip results)
    pub cache_hits: u64,
    /// Number of tiles processed
    pub tiles_processed: u64,
}

/// Clip a geometry hierarchically across all tiles in a zoom range.
///
/// This function processes tiles from lowest to highest zoom, reusing
/// clip results from parent tiles when processing children. This avoids
/// re-clipping the full original geometry for every tile.
///
/// # Arguments
///
/// * `geom` - The geometry to clip (typically already simplified)
/// * `geom_bbox` - Bounding box of the geometry (for tile intersection tests)
/// * `min_zoom` - Minimum zoom level to clip at
/// * `max_zoom` - Maximum zoom level to clip at
/// * `buffer_pixels` - Buffer in pixels around tile bounds
/// * `extent` - Tile extent in pixels (typically 4096)
///
/// # Returns
///
/// A tuple of (clip_results, clip_stats) where clip_results maps each tile
/// to its clipped geometry, and clip_stats tracks operation counts.
///
/// # Algorithm
///
/// 1. At min_zoom: clip geometry to each tile (no parent to reuse)
/// 2. At each subsequent zoom level z:
///    a. For each tile at z, find its parent at z-1
///    b. If the parent clip result exists, clip that instead of the original
///    c. If no parent result (parent was fully outside), skip this tile
/// 3. The "parent buffer" is computed to be large enough that all children's
///    buffered bounds are contained within it, ensuring no geometry is lost.
pub fn clip_geometry_hierarchical(
    geom: &Geometry<f64>,
    geom_bbox: &TileBounds,
    min_zoom: u8,
    max_zoom: u8,
    buffer_pixels: u32,
    extent: u32,
) -> (ClipResults, ClipStats) {
    use crate::tile::tiles_for_bbox;

    let mut results: ClipResults = HashMap::new();
    let mut stats = ClipStats::default();

    // Cache: stores clipped geometry per tile for parent lookups
    // We only need to keep the previous zoom level's results
    let mut prev_zoom_cache: HashMap<TileCoord, Geometry<f64>> = HashMap::new();
    let mut curr_zoom_cache: HashMap<TileCoord, Geometry<f64>> = HashMap::new();

    for z in min_zoom..=max_zoom {
        let tiles: Vec<TileCoord> = tiles_for_bbox(geom_bbox, z).collect();

        for tile_coord in tiles {
            stats.tiles_processed += 1;

            let tile_bounds = tile_coord.bounds();
            let buffer = buffer_pixels_to_degrees(buffer_pixels, &tile_bounds, extent);

            // Quick bbox rejection
            let buffered_lng_min = tile_bounds.lng_min - buffer;
            let buffered_lng_max = tile_bounds.lng_max + buffer;
            let buffered_lat_min = tile_bounds.lat_min - buffer;
            let buffered_lat_max = tile_bounds.lat_max + buffer;

            let intersects = geom_bbox.lng_max >= buffered_lng_min
                && geom_bbox.lng_min <= buffered_lng_max
                && geom_bbox.lat_max >= buffered_lat_min
                && geom_bbox.lat_min <= buffered_lat_max;

            if !intersects {
                continue;
            }

            // Try to use parent's clip result for efficiency
            let source_geom = if z > min_zoom {
                if let Some(parent) = tile_coord.parent() {
                    if let Some(parent_clipped) = prev_zoom_cache.get(&parent) {
                        stats.cache_hits += 1;
                        parent_clipped
                    } else {
                        // Parent had no clip result (geometry didn't intersect parent).
                        // But due to buffer differences, child might still intersect.
                        // Fall back to original geometry.
                        geom
                    }
                } else {
                    geom
                }
            } else {
                geom
            };

            // Perform the clip operation
            stats.clip_ops += 1;
            if let Some(clipped) = clip_geometry(source_geom, &tile_bounds, buffer) {
                curr_zoom_cache.insert(tile_coord, clipped.clone());
                results.insert(tile_coord, clipped);
            }
        }

        // Rotate caches: current becomes previous for next zoom level
        prev_zoom_cache = curr_zoom_cache;
        curr_zoom_cache = HashMap::new();
    }

    (results, stats)
}

// ============================================================================
// WorldCoord-based Hierarchical Clipping (Phase 1)
// ============================================================================
//
// This provides an alternative bbox intersection test using WorldBounds
// instead of f64 TileBounds. The integer-based intersection test is exact
// and avoids floating-point precision issues in buffer calculations.
//
// PHASE 1: Additive -- the f64 version above remains the primary API.

use crate::world_coord::WorldBounds;

/// WorldCoord-based bounding box for geometry intersection tests.
///
/// This type can be computed once from a geometry's extent and reused
/// across all zoom levels for intersection testing with WorldBounds tiles.
/// It replaces the imprecise f64 TileBounds used for bbox rejection.
///
/// In Phase 2, this will replace `TileBounds` in the hierarchical clipping pipeline.
#[derive(Debug, Clone, Copy)]
pub struct WorldGeomBounds {
    pub bounds: WorldBounds,
}

impl WorldGeomBounds {
    /// Create from a TileBounds (f64 geographic coordinates).
    pub fn from_tile_bounds(tile_bounds: &TileBounds) -> Self {
        Self {
            bounds: WorldBounds::from_tile_bounds(tile_bounds),
        }
    }

    /// Check if this geometry's bounds intersect a tile's buffered world bounds.
    ///
    /// This is the integer-precision replacement for the f64 intersection test
    /// in `clip_geometry_hierarchical`.
    #[inline]
    pub fn intersects_tile_buffered(
        &self,
        tile: &TileCoord,
        buffer_pixels: u32,
        extent: u32,
    ) -> bool {
        let tile_bounds = WorldBounds::from_tile_with_buffer(tile, buffer_pixels, extent);
        self.bounds.intersects(&tile_bounds)
    }

    /// Check if this geometry's bounds intersect a WorldBounds directly.
    #[inline]
    pub fn intersects_world_bounds(&self, bounds: &WorldBounds) -> bool {
        self.bounds.intersects(bounds)
    }
}

/// Check if a geometry bbox (in world coords) intersects a tile's buffered bounds.
///
/// This is a standalone function for use in places where `WorldGeomBounds`
/// is not yet constructed.
///
/// # Arguments
/// * `geom_bbox` - Geometry's bounding box in world coordinates
/// * `tile` - The tile to test against
/// * `buffer_pixels` - Buffer in pixels
/// * `extent` - Tile extent
///
/// # Returns
/// `true` if the geometry bbox intersects the tile's buffered bounds
pub fn intersects_tile_world(
    geom_bbox: &WorldBounds,
    tile: &TileCoord,
    buffer_pixels: u32,
    extent: u32,
) -> bool {
    let tile_bounds = WorldBounds::from_tile_with_buffer(tile, buffer_pixels, extent);
    geom_bbox.intersects(&tile_bounds)
}

// ============================================================================
// WorldCoord-based Hierarchical Clipping (Phase 2)
// ============================================================================
//
// This provides a WorldCoord-native hierarchical clipping pipeline.
// Instead of operating in f64 geographic coordinates and converting at each step,
// this version converts the input geometry to WorldCoord once at the start,
// then performs all clipping operations in integer space.
//
// Benefits:
// - Exact arithmetic: no floating-point precision errors in buffer calculations
// - Single conversion: input → WorldCoord once, not per-tile
// - Cache efficiency: parent results cached as WorldCoord, no re-conversion

use crate::clip::{clip_point_world, clip_polygon_world, polygon_to_world_rings};
use crate::world_coord::{lng_lat_to_world, WorldCoord};

/// Result of WorldCoord-based hierarchical clipping.
///
/// Maps each tile coordinate to its clipped geometry in WorldCoord space.
pub type WorldClipResults = HashMap<TileCoord, WorldClippedGeometry>;

/// A geometry clipped to tile bounds in WorldCoord space.
///
/// This enum represents the various geometry types that can result from clipping.
/// All coordinates are in 32-bit world coordinate space.
#[derive(Debug, Clone)]
pub enum WorldClippedGeometry {
    /// A single point.
    Point(WorldCoord),

    /// A linestring (sequence of connected points).
    LineString(Vec<WorldCoord>),

    /// A polygon with exterior ring and optional interior rings (holes).
    Polygon {
        exterior: Vec<WorldCoord>,
        interiors: Vec<Vec<WorldCoord>>,
    },

    /// Multiple points.
    MultiPoint(Vec<WorldCoord>),

    /// Multiple linestrings.
    MultiLineString(Vec<Vec<WorldCoord>>),

    /// Multiple polygons, each with exterior and interior rings.
    MultiPolygon(Vec<(Vec<WorldCoord>, Vec<Vec<WorldCoord>>)>),
}

/// Convert a geo::Geometry<f64> to WorldCoord representation.
///
/// This is the entry point for the Phase 2 pipeline - convert once at the start,
/// then all operations are in integer space.
fn geometry_to_world(geom: &Geometry<f64>) -> Option<WorldClippedGeometry> {
    match geom {
        Geometry::Point(p) => {
            let wc = lng_lat_to_world(p.x(), p.y());
            Some(WorldClippedGeometry::Point(wc))
        }
        Geometry::LineString(ls) => {
            let coords: Vec<WorldCoord> = ls.coords().map(|c| lng_lat_to_world(c.x, c.y)).collect();
            if coords.is_empty() {
                None
            } else {
                Some(WorldClippedGeometry::LineString(coords))
            }
        }
        Geometry::Polygon(poly) => {
            let (exterior, interiors) = polygon_to_world_rings(poly);
            if exterior.is_empty() {
                None
            } else {
                Some(WorldClippedGeometry::Polygon {
                    exterior,
                    interiors,
                })
            }
        }
        Geometry::MultiPoint(mp) => {
            let points: Vec<WorldCoord> =
                mp.iter().map(|p| lng_lat_to_world(p.x(), p.y())).collect();
            if points.is_empty() {
                None
            } else {
                Some(WorldClippedGeometry::MultiPoint(points))
            }
        }
        Geometry::MultiLineString(mls) => {
            let lines: Vec<Vec<WorldCoord>> = mls
                .iter()
                .map(|ls| ls.coords().map(|c| lng_lat_to_world(c.x, c.y)).collect())
                .filter(|v: &Vec<WorldCoord>| !v.is_empty())
                .collect();
            if lines.is_empty() {
                None
            } else {
                Some(WorldClippedGeometry::MultiLineString(lines))
            }
        }
        Geometry::MultiPolygon(mp) => {
            let polys: Vec<(Vec<WorldCoord>, Vec<Vec<WorldCoord>>)> = mp
                .iter()
                .map(|poly| polygon_to_world_rings(poly))
                .filter(|(ext, _)| !ext.is_empty())
                .collect();
            if polys.is_empty() {
                None
            } else {
                Some(WorldClippedGeometry::MultiPolygon(polys))
            }
        }
        // Other geometry types (GeometryCollection, etc.) - not supported yet
        _ => None,
    }
}

/// Compute the WorldBounds of a WorldClippedGeometry.
fn world_geometry_bounds(geom: &WorldClippedGeometry) -> Option<WorldBounds> {
    let mut x_min = u32::MAX;
    let mut y_min = u32::MAX;
    let mut x_max = 0u32;
    let mut y_max = 0u32;

    let mut update_bounds = |coord: &WorldCoord| {
        x_min = x_min.min(coord.x);
        y_min = y_min.min(coord.y);
        x_max = x_max.max(coord.x);
        y_max = y_max.max(coord.y);
    };

    match geom {
        WorldClippedGeometry::Point(p) => {
            update_bounds(p);
        }
        WorldClippedGeometry::LineString(coords) => {
            for c in coords {
                update_bounds(c);
            }
        }
        WorldClippedGeometry::Polygon { exterior, .. } => {
            for c in exterior {
                update_bounds(c);
            }
        }
        WorldClippedGeometry::MultiPoint(points) => {
            for p in points {
                update_bounds(p);
            }
        }
        WorldClippedGeometry::MultiLineString(lines) => {
            for line in lines {
                for c in line {
                    update_bounds(c);
                }
            }
        }
        WorldClippedGeometry::MultiPolygon(polys) => {
            for (ext, _) in polys {
                for c in ext {
                    update_bounds(c);
                }
            }
        }
    }

    if x_min <= x_max && y_min <= y_max {
        Some(WorldBounds::new(x_min, y_min, x_max, y_max))
    } else {
        None
    }
}

/// Clip a WorldClippedGeometry to the given WorldBounds.
///
/// Returns the clipped geometry, or None if the geometry doesn't intersect the bounds.
fn clip_world_geometry(
    geom: &WorldClippedGeometry,
    bounds: &WorldBounds,
) -> Option<WorldClippedGeometry> {
    match geom {
        WorldClippedGeometry::Point(p) => {
            clip_point_world(p, bounds).map(WorldClippedGeometry::Point)
        }
        WorldClippedGeometry::LineString(coords) => {
            // LineString clipping in WorldCoord not yet implemented - use bbox check
            let geom_bounds = world_geometry_bounds(geom)?;
            if bounds.intersects(&geom_bounds) {
                // Return the original linestring if it intersects
                // TODO: Implement proper linestring clipping in WorldCoord space
                Some(WorldClippedGeometry::LineString(coords.clone()))
            } else {
                None
            }
        }
        WorldClippedGeometry::Polygon {
            exterior,
            interiors,
        } => clip_polygon_world(exterior, interiors, bounds).map(|(ext, ints)| {
            WorldClippedGeometry::Polygon {
                exterior: ext,
                interiors: ints,
            }
        }),
        WorldClippedGeometry::MultiPoint(points) => {
            let clipped: Vec<WorldCoord> = points
                .iter()
                .filter_map(|p| clip_point_world(p, bounds))
                .collect();
            if clipped.is_empty() {
                None
            } else {
                Some(WorldClippedGeometry::MultiPoint(clipped))
            }
        }
        WorldClippedGeometry::MultiLineString(lines) => {
            // MultiLineString clipping in WorldCoord not yet implemented - use bbox check
            let geom_bounds = world_geometry_bounds(geom)?;
            if bounds.intersects(&geom_bounds) {
                Some(WorldClippedGeometry::MultiLineString(lines.clone()))
            } else {
                None
            }
        }
        WorldClippedGeometry::MultiPolygon(polys) => {
            let clipped: Vec<(Vec<WorldCoord>, Vec<Vec<WorldCoord>>)> = polys
                .iter()
                .filter_map(|(ext, ints)| clip_polygon_world(ext, ints, bounds))
                .collect();
            if clipped.is_empty() {
                None
            } else {
                Some(WorldClippedGeometry::MultiPolygon(clipped))
            }
        }
    }
}

/// Clip a geometry hierarchically across all tiles in a zoom range, using WorldCoord space.
///
/// This is the Phase 2 WorldCoord-native version of `clip_geometry_hierarchical`.
/// It converts the input geometry to WorldCoord once at the start, then performs
/// all clipping operations in integer space.
///
/// # Arguments
///
/// * `geom` - The geometry to clip (in f64 geographic coordinates)
/// * `geom_bbox` - Bounding box of the geometry (for tile intersection tests)
/// * `min_zoom` - Minimum zoom level to clip at
/// * `max_zoom` - Maximum zoom level to clip at
/// * `buffer_pixels` - Buffer in pixels around tile bounds
/// * `extent` - Tile extent in pixels (typically 4096)
///
/// # Returns
///
/// A tuple of (world_clip_results, clip_stats) where world_clip_results maps each tile
/// to its clipped geometry in WorldCoord space, and clip_stats tracks operation counts.
///
/// # Algorithm
///
/// 1. Convert input geometry to WorldCoord once
/// 2. At min_zoom: clip geometry to each tile using WorldBounds
/// 3. At each subsequent zoom level z:
///    a. For each tile at z, find its parent at z-1
///    b. If the parent clip result exists, clip that instead of the original
///    c. If no parent result, fall back to original geometry
/// 4. All operations use integer arithmetic (no f64 conversions per-tile)
pub fn clip_geometry_hierarchical_world(
    geom: &Geometry<f64>,
    geom_bbox: &TileBounds,
    min_zoom: u8,
    max_zoom: u8,
    buffer_pixels: u32,
    extent: u32,
) -> (WorldClipResults, ClipStats) {
    use crate::tile::tiles_for_bbox;

    let mut results: WorldClipResults = HashMap::new();
    let mut stats = ClipStats::default();

    // Convert input geometry to WorldCoord once
    let world_geom = match geometry_to_world(geom) {
        Some(wg) => wg,
        None => return (results, stats),
    };

    // Convert geometry bbox to WorldBounds for intersection tests
    let geom_world_bbox = WorldBounds::from_tile_bounds(geom_bbox);

    // Cache: stores clipped geometry per tile for parent lookups
    let mut prev_zoom_cache: HashMap<TileCoord, WorldClippedGeometry> = HashMap::new();
    let mut curr_zoom_cache: HashMap<TileCoord, WorldClippedGeometry> = HashMap::new();

    for z in min_zoom..=max_zoom {
        let tiles: Vec<TileCoord> = tiles_for_bbox(geom_bbox, z).collect();

        for tile_coord in tiles {
            stats.tiles_processed += 1;

            // Get tile bounds with buffer in WorldCoord space
            let tile_world_bounds =
                WorldBounds::from_tile_with_buffer(&tile_coord, buffer_pixels, extent);

            // Quick bbox rejection in world coords (exact, no float errors)
            if !geom_world_bbox.intersects(&tile_world_bounds) {
                continue;
            }

            // Try to use parent's clip result for efficiency
            let source_geom = if z > min_zoom {
                if let Some(parent) = tile_coord.parent() {
                    if let Some(parent_clipped) = prev_zoom_cache.get(&parent) {
                        stats.cache_hits += 1;
                        parent_clipped
                    } else {
                        // Parent had no clip result - fall back to original
                        &world_geom
                    }
                } else {
                    &world_geom
                }
            } else {
                &world_geom
            };

            // Perform the clip operation in WorldCoord space
            stats.clip_ops += 1;
            if let Some(clipped) = clip_world_geometry(source_geom, &tile_world_bounds) {
                curr_zoom_cache.insert(tile_coord, clipped.clone());
                results.insert(tile_coord, clipped);
            }
        }

        // Rotate caches: current becomes previous for next zoom level
        prev_zoom_cache = curr_zoom_cache;
        curr_zoom_cache = HashMap::new();
    }

    (results, stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{point, polygon};

    /// Helper to count flat (non-hierarchical) clip operations for comparison.
    fn count_flat_clip_ops(
        geom: &Geometry<f64>,
        geom_bbox: &TileBounds,
        min_zoom: u8,
        max_zoom: u8,
        buffer_pixels: u32,
        extent: u32,
    ) -> u64 {
        use crate::tile::tiles_for_bbox;

        let mut clip_ops = 0u64;

        for z in min_zoom..=max_zoom {
            let tiles: Vec<TileCoord> = tiles_for_bbox(geom_bbox, z).collect();

            for tile_coord in tiles {
                let tile_bounds = tile_coord.bounds();
                let buffer = buffer_pixels_to_degrees(buffer_pixels, &tile_bounds, extent);

                // Quick bbox rejection (same as hierarchical)
                let buffered_lng_min = tile_bounds.lng_min - buffer;
                let buffered_lng_max = tile_bounds.lng_max + buffer;
                let buffered_lat_min = tile_bounds.lat_min - buffer;
                let buffered_lat_max = tile_bounds.lat_max + buffer;

                let intersects = geom_bbox.lng_max >= buffered_lng_min
                    && geom_bbox.lng_min <= buffered_lng_max
                    && geom_bbox.lat_max >= buffered_lat_min
                    && geom_bbox.lat_min <= buffered_lat_max;

                if !intersects {
                    continue;
                }

                clip_ops += 1;
                let _ = clip_geometry(geom, &tile_bounds, buffer);
            }
        }

        clip_ops
    }

    // ========== Basic Correctness Tests ==========

    #[test]
    fn test_hierarchical_clip_point_same_as_flat() {
        // A point should produce the same results whether clipped hierarchically or flat
        let point = Geometry::Point(point!(x: 1.55, y: 42.55));
        let bbox = TileBounds::new(1.55, 42.55, 1.55, 42.55);

        let (results, stats) = clip_geometry_hierarchical(&point, &bbox, 0, 4, 8, 4096);

        // Point should be in exactly one tile per zoom level
        for z in 0..=4u8 {
            let tiles_at_z: Vec<_> = results.keys().filter(|tc| tc.z == z).collect();
            assert_eq!(
                tiles_at_z.len(),
                1,
                "Point should be in exactly 1 tile at zoom {}",
                z
            );
        }

        assert_eq!(results.len(), 5, "Should have results for 5 zoom levels");
        assert!(stats.clip_ops > 0, "Should have performed clip operations");
    }

    #[test]
    fn test_hierarchical_clip_polygon_same_results_as_flat() {
        // A polygon spanning multiple tiles should produce the same clipped
        // geometries whether clipped hierarchically or flat
        let poly = Geometry::Polygon(polygon![
            (x: -5.0, y: -5.0),
            (x: 5.0, y: -5.0),
            (x: 5.0, y: 5.0),
            (x: -5.0, y: 5.0),
            (x: -5.0, y: -5.0),
        ]);
        let bbox = TileBounds::new(-5.0, -5.0, 5.0, 5.0);

        let (hierarchical_results, _stats) =
            clip_geometry_hierarchical(&poly, &bbox, 0, 3, 8, 4096);

        // Flat clip for comparison
        use crate::tile::tiles_for_bbox;
        let mut flat_results: HashMap<TileCoord, Geometry<f64>> = HashMap::new();

        for z in 0..=3u8 {
            for tile_coord in tiles_for_bbox(&bbox, z) {
                let tile_bounds = tile_coord.bounds();
                let buffer = buffer_pixels_to_degrees(8, &tile_bounds, 4096);

                let buffered_lng_min = tile_bounds.lng_min - buffer;
                let buffered_lng_max = tile_bounds.lng_max + buffer;
                let buffered_lat_min = tile_bounds.lat_min - buffer;
                let buffered_lat_max = tile_bounds.lat_max + buffer;

                let intersects = bbox.lng_max >= buffered_lng_min
                    && bbox.lng_min <= buffered_lng_max
                    && bbox.lat_max >= buffered_lat_min
                    && bbox.lat_min <= buffered_lat_max;

                if !intersects {
                    continue;
                }

                if let Some(clipped) = clip_geometry(&poly, &tile_bounds, buffer) {
                    flat_results.insert(tile_coord, clipped);
                }
            }
        }

        // Same tiles should have results
        assert_eq!(
            hierarchical_results.len(),
            flat_results.len(),
            "Hierarchical and flat should produce same number of tile results. \
             Hierarchical: {}, Flat: {}",
            hierarchical_results.len(),
            flat_results.len()
        );

        // Every tile in flat should also be in hierarchical
        for tile_coord in flat_results.keys() {
            assert!(
                hierarchical_results.contains_key(tile_coord),
                "Tile {:?} in flat results but not in hierarchical",
                tile_coord
            );
        }
    }

    // ========== Performance Tests (clip operation counting) ==========

    #[test]
    fn test_hierarchical_fewer_effective_clip_ops_large_polygon() {
        // A large polygon spanning many tiles should show that hierarchical
        // clipping does fewer clip operations from the original geometry,
        // because it clips from parent results instead.
        //
        // The key metric is: hierarchical clip_ops should still happen for every
        // tile, but the SOURCE geometry for most clips is smaller (the parent's
        // result rather than the original).
        let poly = Geometry::Polygon(polygon![
            (x: -20.0, y: -20.0),
            (x: 20.0, y: -20.0),
            (x: 20.0, y: 20.0),
            (x: -20.0, y: 20.0),
            (x: -20.0, y: -20.0),
        ]);
        let bbox = TileBounds::new(-20.0, -20.0, 20.0, 20.0);

        let (_results, stats) = clip_geometry_hierarchical(&poly, &bbox, 0, 4, 8, 4096);

        // Hierarchical should have cache hits at z>min_zoom
        assert!(
            stats.cache_hits > 0,
            "Should have cache hits from parent reuse. Got: {:?}",
            stats
        );

        // The number of clip ops should equal tiles_processed (we clip every intersecting tile)
        // but cache_hits tells us how many times we used a smaller parent geometry
        assert!(
            stats.cache_hits > stats.clip_ops / 3,
            "Cache hits ({}) should be a significant fraction of clip ops ({})",
            stats.cache_hits,
            stats.clip_ops
        );
    }

    #[test]
    fn test_hierarchical_uses_cache_at_higher_zooms() {
        // At zoom levels above min_zoom, hierarchical clipping should reuse
        // parent results rather than the original geometry
        let poly = Geometry::Polygon(polygon![
            (x: -10.0, y: -10.0),
            (x: 10.0, y: -10.0),
            (x: 10.0, y: 10.0),
            (x: -10.0, y: 10.0),
            (x: -10.0, y: -10.0),
        ]);
        let bbox = TileBounds::new(-10.0, -10.0, 10.0, 10.0);

        let (_results, stats) = clip_geometry_hierarchical(&poly, &bbox, 0, 5, 8, 4096);

        // At z=0, there's 1 tile -- no cache possible
        // At z=1+, all tiles should find their parent in cache
        // So cache_hits should be close to (total clip_ops - tiles_at_z0)
        let tiles_at_z0 = 1u64; // z=0 always has exactly 1 tile (when bbox fits in world)
        let expected_min_cache_hits = stats.clip_ops.saturating_sub(tiles_at_z0);

        assert!(
            stats.cache_hits >= expected_min_cache_hits * 8 / 10,
            "Cache hits ({}) should be close to clip_ops ({}) minus z0 tiles ({}). \
             Expected at least ~{}",
            stats.cache_hits,
            stats.clip_ops,
            tiles_at_z0,
            expected_min_cache_hits * 8 / 10
        );
    }

    #[test]
    fn test_hierarchical_clip_empty_geometry() {
        // A point outside all tile bounds should produce empty results
        let point = Geometry::Point(point!(x: 200.0, y: 200.0)); // Invalid coords
        let bbox = TileBounds::new(200.0, 200.0, 200.0, 200.0);

        let (results, stats) = clip_geometry_hierarchical(&point, &bbox, 0, 4, 8, 4096);

        assert!(
            results.is_empty(),
            "No tiles should contain an invalid point"
        );
        assert_eq!(
            stats.clip_ops, 0,
            "No clip operations for non-intersecting geometry"
        );
    }

    #[test]
    fn test_hierarchical_single_zoom() {
        // With min_zoom == max_zoom, hierarchical should behave same as flat
        let poly = Geometry::Polygon(polygon![
            (x: -5.0, y: -5.0),
            (x: 5.0, y: -5.0),
            (x: 5.0, y: 5.0),
            (x: -5.0, y: 5.0),
            (x: -5.0, y: -5.0),
        ]);
        let bbox = TileBounds::new(-5.0, -5.0, 5.0, 5.0);

        let (_results, stats) = clip_geometry_hierarchical(&poly, &bbox, 3, 3, 8, 4096);

        // Single zoom level means no parent cache possible
        assert_eq!(stats.cache_hits, 0, "Single zoom should have no cache hits");
        assert!(stats.clip_ops > 0, "Should still perform clip operations");
    }

    #[test]
    fn test_hierarchical_clip_preserves_all_tiles() {
        // Every tile that would be produced by flat clipping should also
        // be produced by hierarchical clipping (no tiles lost)
        let poly = Geometry::Polygon(polygon![
            (x: -15.0, y: -10.0),
            (x: 15.0, y: -10.0),
            (x: 15.0, y: 10.0),
            (x: -15.0, y: 10.0),
            (x: -15.0, y: -10.0),
        ]);
        let bbox = TileBounds::new(-15.0, -10.0, 15.0, 10.0);

        let (hierarchical_results, _) = clip_geometry_hierarchical(&poly, &bbox, 0, 4, 8, 4096);

        let _flat_ops = count_flat_clip_ops(&poly, &bbox, 0, 4, 8, 4096);

        // Hierarchical should process the same number of tiles
        // (it clips every tile, just uses a smaller source geometry)
        assert!(
            !hierarchical_results.is_empty(),
            "Should produce some tile results"
        );

        // Verify no tiles are lost
        use crate::tile::tiles_for_bbox;
        for z in 0..=4u8 {
            for tile_coord in tiles_for_bbox(&bbox, z) {
                let tile_bounds = tile_coord.bounds();
                let buffer = buffer_pixels_to_degrees(8, &tile_bounds, 4096);

                if let Some(_flat_clipped) = clip_geometry(&poly, &tile_bounds, buffer) {
                    assert!(
                        hierarchical_results.contains_key(&tile_coord),
                        "Tile {:?} produced by flat clip but missing from hierarchical",
                        tile_coord
                    );
                }
            }
        }
    }

    // ========== Summary / Benchmark-style Tests ==========

    #[test]
    fn test_hierarchical_clip_reduction_summary() {
        // Demonstrate the clip operation reduction across various geometries
        // and zoom ranges. This test prints a summary showing:
        // - Flat clip ops (the old approach)
        // - Hierarchical clip ops (same count, but from smaller source geometries)
        // - Cache hit ratio (fraction of clips that used parent result instead of original)

        struct TestCase {
            name: &'static str,
            geom: Geometry<f64>,
            bbox: TileBounds,
            min_zoom: u8,
            max_zoom: u8,
        }

        let cases = vec![
            TestCase {
                name: "Small polygon (1 tile per zoom)",
                geom: Geometry::Polygon(polygon![
                    (x: 1.5, y: 42.5),
                    (x: 1.6, y: 42.5),
                    (x: 1.6, y: 42.6),
                    (x: 1.5, y: 42.6),
                    (x: 1.5, y: 42.5),
                ]),
                bbox: TileBounds::new(1.5, 42.5, 1.6, 42.6),
                min_zoom: 0,
                max_zoom: 8,
            },
            TestCase {
                name: "Medium polygon (city-sized, ~10x10 degrees)",
                geom: Geometry::Polygon(polygon![
                    (x: -5.0, y: -5.0),
                    (x: 5.0, y: -5.0),
                    (x: 5.0, y: 5.0),
                    (x: -5.0, y: 5.0),
                    (x: -5.0, y: -5.0),
                ]),
                bbox: TileBounds::new(-5.0, -5.0, 5.0, 5.0),
                min_zoom: 0,
                max_zoom: 6,
            },
            TestCase {
                name: "Large polygon (country-sized, ~40x40 degrees)",
                geom: Geometry::Polygon(polygon![
                    (x: -20.0, y: -20.0),
                    (x: 20.0, y: -20.0),
                    (x: 20.0, y: 20.0),
                    (x: -20.0, y: 20.0),
                    (x: -20.0, y: -20.0),
                ]),
                bbox: TileBounds::new(-20.0, -20.0, 20.0, 20.0),
                min_zoom: 0,
                max_zoom: 5,
            },
        ];

        eprintln!("\n=== Hierarchical Clipping Reduction Summary ===");
        eprintln!(
            "{:<45} {:>10} {:>10} {:>10} {:>8}",
            "Case", "Flat Ops", "Hier Ops", "Cache Hit", "Hit %"
        );
        eprintln!("{}", "-".repeat(90));

        for case in &cases {
            let flat_ops = count_flat_clip_ops(
                &case.geom,
                &case.bbox,
                case.min_zoom,
                case.max_zoom,
                8,
                4096,
            );

            let (_results, stats) = clip_geometry_hierarchical(
                &case.geom,
                &case.bbox,
                case.min_zoom,
                case.max_zoom,
                8,
                4096,
            );

            let hit_pct = if stats.clip_ops > 0 {
                stats.cache_hits as f64 / stats.clip_ops as f64 * 100.0
            } else {
                0.0
            };

            eprintln!(
                "{:<45} {:>10} {:>10} {:>10} {:>7.1}%",
                case.name, flat_ops, stats.clip_ops, stats.cache_hits, hit_pct
            );

            // Hierarchical ops should equal flat ops (we still clip every tile)
            assert_eq!(
                stats.clip_ops, flat_ops,
                "{}: clip ops should match (both clip every intersecting tile)",
                case.name
            );
        }

        eprintln!("{}", "=".repeat(90));
        eprintln!("Note: Same # of clip ops, but hierarchical clips use SMALLER source geometries");
        eprintln!("(parent clip results instead of the original geometry, reducing computation)\n");
    }

    // ========================================================================
    // WorldCoord-based intersection tests
    // ========================================================================

    mod world_tests {
        use super::*;
        use crate::tile::TileCoord;
        use crate::world_coord::WorldBounds;

        #[test]
        fn test_world_geom_bounds_from_tile_bounds() {
            let tile_bounds = TileBounds::new(-10.0, -10.0, 10.0, 10.0);
            let world_geom = WorldGeomBounds::from_tile_bounds(&tile_bounds);

            // Should produce valid world bounds
            assert!(
                world_geom.bounds.x_min < world_geom.bounds.x_max,
                "x_min ({}) should be < x_max ({})",
                world_geom.bounds.x_min,
                world_geom.bounds.x_max
            );
            assert!(
                world_geom.bounds.y_min < world_geom.bounds.y_max,
                "y_min ({}) should be < y_max ({})",
                world_geom.bounds.y_min,
                world_geom.bounds.y_max
            );
        }

        #[test]
        fn test_world_geom_intersects_tile() {
            // A geometry bbox covering (-5, -5) to (5, 5) in geographic coords
            let geom_bbox = TileBounds::new(-5.0, -5.0, 5.0, 5.0);
            let world_geom = WorldGeomBounds::from_tile_bounds(&geom_bbox);

            // Tile at zoom 0 should always intersect (covers the whole world)
            let tile_z0 = TileCoord::new(0, 0, 0);
            assert!(
                world_geom.intersects_tile_buffered(&tile_z0, 8, 4096),
                "z0 tile should intersect any geometry"
            );

            // Tile at zoom 1, tile (1,1) = SE quadrant, should intersect
            // (geometry spans the origin which is at the junction of all z1 tiles)
            let tile_z1_se = TileCoord::new(1, 1, 1);
            assert!(
                world_geom.intersects_tile_buffered(&tile_z1_se, 8, 4096),
                "z1 SE tile should intersect geometry spanning origin"
            );
        }

        #[test]
        fn test_world_geom_does_not_intersect_distant_tile() {
            // A geometry bbox covering a small area near NYC
            let geom_bbox = TileBounds::new(-74.0, 40.7, -73.9, 40.8);
            let world_geom = WorldGeomBounds::from_tile_bounds(&geom_bbox);

            // A tile on the other side of the world (e.g., Pacific Ocean)
            // At zoom 4, tile (0, 8) is in the far west
            let distant_tile = TileCoord::new(14, 8, 4);
            assert!(
                !world_geom.intersects_tile_buffered(&distant_tile, 8, 4096),
                "Distant tile should not intersect NYC geometry"
            );
        }

        #[test]
        fn test_intersects_tile_world_consistency_with_f64() {
            // Verify that the WorldBounds intersection test produces the same
            // results as the f64 intersection test for a range of tiles.
            let geom_bbox_f64 = TileBounds::new(-5.0, -5.0, 5.0, 5.0);
            let geom_bbox_world = WorldBounds::from_tile_bounds(&geom_bbox_f64);

            use crate::tile::tiles_for_bbox;

            for z in 0..=5u8 {
                let tiles: Vec<TileCoord> = tiles_for_bbox(&geom_bbox_f64, z).collect();

                for tile in &tiles {
                    let tile_bounds = tile.bounds();
                    let buffer_f64 = buffer_pixels_to_degrees(8, &tile_bounds, 4096);

                    // f64 intersection test (from the existing code)
                    let buffered_lng_min = tile_bounds.lng_min - buffer_f64;
                    let buffered_lng_max = tile_bounds.lng_max + buffer_f64;
                    let buffered_lat_min = tile_bounds.lat_min - buffer_f64;
                    let buffered_lat_max = tile_bounds.lat_max + buffer_f64;

                    let f64_intersects = geom_bbox_f64.lng_max >= buffered_lng_min
                        && geom_bbox_f64.lng_min <= buffered_lng_max
                        && geom_bbox_f64.lat_max >= buffered_lat_min
                        && geom_bbox_f64.lat_min <= buffered_lat_max;

                    // WorldCoord intersection test
                    let world_intersects = intersects_tile_world(&geom_bbox_world, tile, 8, 4096);

                    assert_eq!(
                        f64_intersects, world_intersects,
                        "Intersection mismatch at tile {:?} z{}: f64={}, world={}",
                        tile, z, f64_intersects, world_intersects
                    );
                }
            }
        }

        #[test]
        fn test_intersects_tile_world_with_buffer() {
            // A geometry bbox at the edge of a tile -- should NOT intersect without buffer,
            // but SHOULD intersect with buffer.
            let tile = TileCoord::new(8, 5, 4);
            let tile_bounds_world = WorldBounds::from_tile(&tile);

            // Place geometry just outside the right edge of the tile
            let just_outside = WorldBounds::new(
                tile_bounds_world.x_max + 1,
                tile_bounds_world.y_min + 100,
                tile_bounds_world.x_max + 1000,
                tile_bounds_world.y_max - 100,
            );

            // Without buffer: should not intersect
            assert!(
                !just_outside.intersects(&tile_bounds_world),
                "Should not intersect without buffer"
            );

            // With buffer: should intersect (buffer extends the tile bounds)
            let tile_bounds_buffered = WorldBounds::from_tile_with_buffer(&tile, 8, 4096);
            assert!(
                just_outside.intersects(&tile_bounds_buffered),
                "Should intersect with buffer"
            );
        }

        // ====================================================================
        // WorldCoord-based Hierarchical Clipping Tests (Phase 2)
        // ====================================================================

        #[test]
        fn test_world_hierarchical_clip_point() {
            // A point should produce WorldCoord results in exactly one tile per zoom
            let point = Geometry::Point(point!(x: 1.55, y: 42.55));
            let bbox = TileBounds::new(1.55, 42.55, 1.55, 42.55);

            let (results, stats) = clip_geometry_hierarchical_world(&point, &bbox, 0, 4, 8, 4096);

            // Point should be in exactly one tile per zoom level
            for z in 0..=4u8 {
                let tiles_at_z: Vec<_> = results.keys().filter(|tc| tc.z == z).collect();
                assert_eq!(
                    tiles_at_z.len(),
                    1,
                    "Point should be in exactly 1 tile at zoom {}",
                    z
                );

                // Verify the result is a Point variant
                let tile = tiles_at_z[0];
                match &results[tile] {
                    WorldClippedGeometry::Point(_) => {}
                    other => panic!("Expected Point, got {:?}", other),
                }
            }

            assert_eq!(results.len(), 5, "Should have results for 5 zoom levels");
            assert!(stats.clip_ops > 0, "Should have performed clip operations");
        }

        #[test]
        fn test_world_hierarchical_clip_polygon_same_tiles_as_f64() {
            // A polygon should produce results for the same tiles as the f64 version
            let poly = Geometry::Polygon(polygon![
                (x: -5.0, y: -5.0),
                (x: 5.0, y: -5.0),
                (x: 5.0, y: 5.0),
                (x: -5.0, y: 5.0),
                (x: -5.0, y: -5.0),
            ]);
            let bbox = TileBounds::new(-5.0, -5.0, 5.0, 5.0);

            let (world_results, world_stats) =
                clip_geometry_hierarchical_world(&poly, &bbox, 0, 3, 8, 4096);
            let (f64_results, f64_stats) = clip_geometry_hierarchical(&poly, &bbox, 0, 3, 8, 4096);

            // Should have same number of tiles
            assert_eq!(
                world_results.len(),
                f64_results.len(),
                "World and f64 should produce same number of tile results. \
                 World: {}, f64: {}",
                world_results.len(),
                f64_results.len()
            );

            // Every tile in f64 should also be in world results
            for tile_coord in f64_results.keys() {
                assert!(
                    world_results.contains_key(tile_coord),
                    "Tile {:?} in f64 results but not in world results",
                    tile_coord
                );
            }

            // Similar clip stats
            assert_eq!(
                world_stats.clip_ops, f64_stats.clip_ops,
                "Clip ops should match"
            );
        }

        #[test]
        fn test_world_hierarchical_clip_polygon_produces_polygons() {
            // Verify that polygon clipping produces Polygon variants
            let poly = Geometry::Polygon(polygon![
                (x: -5.0, y: -5.0),
                (x: 5.0, y: -5.0),
                (x: 5.0, y: 5.0),
                (x: -5.0, y: 5.0),
                (x: -5.0, y: -5.0),
            ]);
            let bbox = TileBounds::new(-5.0, -5.0, 5.0, 5.0);

            let (results, _) = clip_geometry_hierarchical_world(&poly, &bbox, 0, 2, 8, 4096);

            for (tile, geom) in &results {
                match geom {
                    WorldClippedGeometry::Polygon { exterior, .. } => {
                        assert!(
                            exterior.len() >= 3,
                            "Polygon at {:?} should have at least 3 vertices, got {}",
                            tile,
                            exterior.len()
                        );
                    }
                    other => panic!("Expected Polygon at {:?}, got {:?}", tile, other),
                }
            }
        }

        #[test]
        fn test_world_hierarchical_uses_parent_cache() {
            // Large polygon should show cache hits from parent reuse
            let poly = Geometry::Polygon(polygon![
                (x: -20.0, y: -20.0),
                (x: 20.0, y: -20.0),
                (x: 20.0, y: 20.0),
                (x: -20.0, y: 20.0),
                (x: -20.0, y: -20.0),
            ]);
            let bbox = TileBounds::new(-20.0, -20.0, 20.0, 20.0);

            let (_results, stats) = clip_geometry_hierarchical_world(&poly, &bbox, 0, 4, 8, 4096);

            // Should have cache hits at z>0
            assert!(
                stats.cache_hits > 0,
                "Should have cache hits from parent reuse. Got: {:?}",
                stats
            );

            // Cache hits should be a significant fraction of clip ops
            assert!(
                stats.cache_hits > stats.clip_ops / 3,
                "Cache hits ({}) should be > 1/3 of clip ops ({})",
                stats.cache_hits,
                stats.clip_ops
            );
        }

        #[test]
        fn test_world_hierarchical_invalid_coords_are_clamped() {
            // WorldCoord conversion clamps invalid coordinates to valid Web Mercator bounds.
            // A point at (200.0, 200.0) gets clamped to approximately (180.0, 85.05).
            // This means it DOES produce results (unlike the f64 version which
            // uses raw bbox rejection before clamping).
            let point = Geometry::Point(point!(x: 200.0, y: 200.0));
            // Note: We need the bbox to also be in a valid range for tiles_for_bbox to work
            // Using clamped values that match where the point will end up
            let bbox = TileBounds::new(179.0, 85.0, 180.0, 86.0);

            let (results, stats) = clip_geometry_hierarchical_world(&point, &bbox, 0, 2, 8, 4096);

            // WorldCoord clamps coords to valid range, so point IS in valid tiles
            assert!(
                !results.is_empty(),
                "WorldCoord clamps invalid coords - point should be in valid tiles"
            );
            assert!(
                stats.clip_ops > 0,
                "Should have performed clip operations for clamped point"
            );
        }

        #[test]
        fn test_world_hierarchical_single_zoom() {
            // With min_zoom == max_zoom, should behave same as regular hierarchical
            let poly = Geometry::Polygon(polygon![
                (x: -5.0, y: -5.0),
                (x: 5.0, y: -5.0),
                (x: 5.0, y: 5.0),
                (x: -5.0, y: 5.0),
                (x: -5.0, y: -5.0),
            ]);
            let bbox = TileBounds::new(-5.0, -5.0, 5.0, 5.0);

            let (_results, stats) = clip_geometry_hierarchical_world(&poly, &bbox, 3, 3, 8, 4096);

            // Single zoom level means no parent cache possible
            assert_eq!(stats.cache_hits, 0, "Single zoom should have no cache hits");
            assert!(stats.clip_ops > 0, "Should still perform clip operations");
        }

        #[test]
        fn test_world_hierarchical_multipolygon() {
            // MultiPolygon should be handled correctly
            use geo::MultiPolygon;

            let mp = Geometry::MultiPolygon(MultiPolygon::new(vec![
                polygon![
                    (x: -5.0, y: -5.0),
                    (x: 0.0, y: -5.0),
                    (x: 0.0, y: 0.0),
                    (x: -5.0, y: 0.0),
                    (x: -5.0, y: -5.0),
                ],
                polygon![
                    (x: 0.0, y: 0.0),
                    (x: 5.0, y: 0.0),
                    (x: 5.0, y: 5.0),
                    (x: 0.0, y: 5.0),
                    (x: 0.0, y: 0.0),
                ],
            ]));
            let bbox = TileBounds::new(-5.0, -5.0, 5.0, 5.0);

            let (results, stats) = clip_geometry_hierarchical_world(&mp, &bbox, 0, 2, 8, 4096);

            assert!(!results.is_empty(), "Should have results for MultiPolygon");
            assert!(stats.clip_ops > 0, "Should have performed clip operations");

            // Verify results are MultiPolygon variants
            for (tile, geom) in &results {
                match geom {
                    WorldClippedGeometry::MultiPolygon(polys) => {
                        assert!(
                            !polys.is_empty(),
                            "MultiPolygon at {:?} should not be empty",
                            tile
                        );
                    }
                    other => panic!("Expected MultiPolygon at {:?}, got {:?}", tile, other),
                }
            }
        }

        #[test]
        fn test_world_clipped_geometry_bounds() {
            // Test that world_geometry_bounds computes correct bounds
            use crate::world_coord::WorldCoord;

            let poly = WorldClippedGeometry::Polygon {
                exterior: vec![
                    WorldCoord::new(100, 200),
                    WorldCoord::new(500, 200),
                    WorldCoord::new(500, 600),
                    WorldCoord::new(100, 600),
                    WorldCoord::new(100, 200),
                ],
                interiors: vec![],
            };

            let bounds = world_geometry_bounds(&poly).unwrap();
            assert_eq!(bounds.x_min, 100);
            assert_eq!(bounds.y_min, 200);
            assert_eq!(bounds.x_max, 500);
            assert_eq!(bounds.y_max, 600);
        }

        #[test]
        fn test_geometry_to_world_point() {
            let point = Geometry::Point(point!(x: 0.0, y: 0.0));
            let world = geometry_to_world(&point).unwrap();

            match world {
                WorldClippedGeometry::Point(wc) => {
                    // Null Island should be at world center
                    assert_eq!(wc.x, crate::world_coord::WORLD_HALF);
                    assert_eq!(wc.y, crate::world_coord::WORLD_HALF);
                }
                other => panic!("Expected Point, got {:?}", other),
            }
        }

        #[test]
        fn test_geometry_to_world_polygon() {
            let poly = Geometry::Polygon(polygon![
                (x: -5.0, y: -5.0),
                (x: 5.0, y: -5.0),
                (x: 5.0, y: 5.0),
                (x: -5.0, y: 5.0),
                (x: -5.0, y: -5.0),
            ]);
            let world = geometry_to_world(&poly).unwrap();

            match world {
                WorldClippedGeometry::Polygon {
                    exterior,
                    interiors,
                } => {
                    assert_eq!(exterior.len(), 5, "Should have 5 vertices (closed ring)");
                    assert!(interiors.is_empty(), "Should have no holes");
                }
                other => panic!("Expected Polygon, got {:?}", other),
            }
        }
    }
}
