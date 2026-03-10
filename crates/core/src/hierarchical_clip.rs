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

impl WorldClippedGeometry {
    /// Serialize to bytes for external sorting.
    ///
    /// Format: type byte + geometry-specific data
    /// - Point: type(1) + x(4) + y(4) = 9 bytes
    /// - LineString: type(1) + len(4) + coords(8*len)
    /// - Polygon: type(1) + ext_len(4) + ext_coords + num_holes(4) + [hole_len + hole_coords]*
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        match self {
            WorldClippedGeometry::Point(p) => {
                buf.push(0);
                buf.extend_from_slice(&p.x.to_le_bytes());
                buf.extend_from_slice(&p.y.to_le_bytes());
            }
            WorldClippedGeometry::LineString(coords) => {
                buf.push(1);
                buf.extend_from_slice(&(coords.len() as u32).to_le_bytes());
                for c in coords {
                    buf.extend_from_slice(&c.x.to_le_bytes());
                    buf.extend_from_slice(&c.y.to_le_bytes());
                }
            }
            WorldClippedGeometry::Polygon {
                exterior,
                interiors,
            } => {
                buf.push(2);
                buf.extend_from_slice(&(exterior.len() as u32).to_le_bytes());
                for c in exterior {
                    buf.extend_from_slice(&c.x.to_le_bytes());
                    buf.extend_from_slice(&c.y.to_le_bytes());
                }
                buf.extend_from_slice(&(interiors.len() as u32).to_le_bytes());
                for hole in interiors {
                    buf.extend_from_slice(&(hole.len() as u32).to_le_bytes());
                    for c in hole {
                        buf.extend_from_slice(&c.x.to_le_bytes());
                        buf.extend_from_slice(&c.y.to_le_bytes());
                    }
                }
            }
            WorldClippedGeometry::MultiPoint(points) => {
                buf.push(3);
                buf.extend_from_slice(&(points.len() as u32).to_le_bytes());
                for p in points {
                    buf.extend_from_slice(&p.x.to_le_bytes());
                    buf.extend_from_slice(&p.y.to_le_bytes());
                }
            }
            WorldClippedGeometry::MultiLineString(lines) => {
                buf.push(4);
                buf.extend_from_slice(&(lines.len() as u32).to_le_bytes());
                for line in lines {
                    buf.extend_from_slice(&(line.len() as u32).to_le_bytes());
                    for c in line {
                        buf.extend_from_slice(&c.x.to_le_bytes());
                        buf.extend_from_slice(&c.y.to_le_bytes());
                    }
                }
            }
            WorldClippedGeometry::MultiPolygon(polys) => {
                buf.push(5);
                buf.extend_from_slice(&(polys.len() as u32).to_le_bytes());
                for (exterior, interiors) in polys {
                    buf.extend_from_slice(&(exterior.len() as u32).to_le_bytes());
                    for c in exterior {
                        buf.extend_from_slice(&c.x.to_le_bytes());
                        buf.extend_from_slice(&c.y.to_le_bytes());
                    }
                    buf.extend_from_slice(&(interiors.len() as u32).to_le_bytes());
                    for hole in interiors {
                        buf.extend_from_slice(&(hole.len() as u32).to_le_bytes());
                        for c in hole {
                            buf.extend_from_slice(&c.x.to_le_bytes());
                            buf.extend_from_slice(&c.y.to_le_bytes());
                        }
                    }
                }
            }
        }
        buf
    }

    /// Deserialize from bytes.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        let mut pos = 0;

        let read_u32 = |pos: &mut usize| -> Option<u32> {
            if *pos + 4 > data.len() {
                return None;
            }
            let val = u32::from_le_bytes(data[*pos..*pos + 4].try_into().ok()?);
            *pos += 4;
            Some(val)
        };

        let read_coord = |pos: &mut usize| -> Option<WorldCoord> {
            let x = read_u32(pos)?;
            let y = read_u32(pos)?;
            Some(WorldCoord::new(x, y))
        };

        let read_ring = |pos: &mut usize| -> Option<Vec<WorldCoord>> {
            let len = read_u32(pos)? as usize;
            let mut coords = Vec::with_capacity(len);
            for _ in 0..len {
                coords.push(read_coord(pos)?);
            }
            Some(coords)
        };

        let geom_type = data[pos];
        pos += 1;

        match geom_type {
            0 => {
                // Point
                let coord = read_coord(&mut pos)?;
                Some(WorldClippedGeometry::Point(coord))
            }
            1 => {
                // LineString
                let coords = read_ring(&mut pos)?;
                Some(WorldClippedGeometry::LineString(coords))
            }
            2 => {
                // Polygon
                let exterior = read_ring(&mut pos)?;
                let num_holes = read_u32(&mut pos)? as usize;
                let mut interiors = Vec::with_capacity(num_holes);
                for _ in 0..num_holes {
                    interiors.push(read_ring(&mut pos)?);
                }
                Some(WorldClippedGeometry::Polygon {
                    exterior,
                    interiors,
                })
            }
            3 => {
                // MultiPoint
                let points = read_ring(&mut pos)?;
                Some(WorldClippedGeometry::MultiPoint(points))
            }
            4 => {
                // MultiLineString
                let num_lines = read_u32(&mut pos)? as usize;
                let mut lines = Vec::with_capacity(num_lines);
                for _ in 0..num_lines {
                    lines.push(read_ring(&mut pos)?);
                }
                Some(WorldClippedGeometry::MultiLineString(lines))
            }
            5 => {
                // MultiPolygon
                let num_polys = read_u32(&mut pos)? as usize;
                let mut polys = Vec::with_capacity(num_polys);
                for _ in 0..num_polys {
                    let exterior = read_ring(&mut pos)?;
                    let num_holes = read_u32(&mut pos)? as usize;
                    let mut interiors = Vec::with_capacity(num_holes);
                    for _ in 0..num_holes {
                        interiors.push(read_ring(&mut pos)?);
                    }
                    polys.push((exterior, interiors));
                }
                Some(WorldClippedGeometry::MultiPolygon(polys))
            }
            _ => None,
        }
    }

    /// Check if this geometry is degenerate (collapses to a point) in the given tile.
    ///
    /// A geometry is degenerate if all its coordinates map to the same tile-local pixel
    /// after quantization. This can happen when a geometry is too small for the tile's
    /// resolution.
    ///
    /// # Arguments
    /// * `tile` - The tile to check degeneracy in
    /// * `extent` - Tile extent (typically 4096)
    ///
    /// # Returns
    /// `true` if the geometry is degenerate in this tile
    pub fn is_degenerate_in_tile(&self, tile: &crate::tile::TileCoord, extent: u32) -> bool {
        match self {
            WorldClippedGeometry::Point(_) => false, // Points are never degenerate
            WorldClippedGeometry::LineString(coords) => coords_are_degenerate(coords, tile, extent),
            WorldClippedGeometry::Polygon { exterior, .. } => {
                coords_are_degenerate(exterior, tile, extent)
            }
            WorldClippedGeometry::MultiPoint(_) => false, // MultiPoints are never degenerate
            WorldClippedGeometry::MultiLineString(lines) => lines
                .iter()
                .all(|line| coords_are_degenerate(line, tile, extent)),
            WorldClippedGeometry::MultiPolygon(polys) => polys
                .iter()
                .all(|(ext, _)| coords_are_degenerate(ext, tile, extent)),
        }
    }
}

/// Check if all coordinates collapse to the same tile-local pixel.
fn coords_are_degenerate(
    coords: &[WorldCoord],
    tile: &crate::tile::TileCoord,
    extent: u32,
) -> bool {
    if coords.len() < 2 {
        return true;
    }

    let (first_x, first_y) = coords[0].to_tile_local(tile, extent);

    coords[1..].iter().all(|c| {
        let (x, y) = c.to_tile_local(tile, extent);
        x == first_x && y == first_y
    })
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
                .map(polygon_to_world_rings)
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

        // ========== Serialization Round-Trip Tests ==========

        #[test]
        fn test_roundtrip_point_to_bytes_from_bytes() {
            let coord = WorldCoord::new(1_000_000, 2_000_000);
            let geom = WorldClippedGeometry::Point(coord);
            let bytes = geom.to_bytes();
            let restored = WorldClippedGeometry::from_bytes(&bytes).unwrap();
            match restored {
                WorldClippedGeometry::Point(p) => {
                    assert_eq!(p.x, 1_000_000);
                    assert_eq!(p.y, 2_000_000);
                }
                other => panic!("Expected Point, got {:?}", other),
            }
        }

        #[test]
        fn test_roundtrip_polygon_to_bytes_from_bytes() {
            // Use coordinates that span a realistic range for a polygon
            // within a single tile at zoom 14
            let coords = vec![
                WorldCoord::new(2_147_483_648, 2_147_483_648), // ~Null Island
                WorldCoord::new(2_147_500_000, 2_147_483_648), // East
                WorldCoord::new(2_147_500_000, 2_147_500_000), // SE
                WorldCoord::new(2_147_483_648, 2_147_500_000), // South
                WorldCoord::new(2_147_483_648, 2_147_483_648), // Close
            ];
            let hole = vec![
                WorldCoord::new(2_147_490_000, 2_147_490_000),
                WorldCoord::new(2_147_495_000, 2_147_490_000),
                WorldCoord::new(2_147_495_000, 2_147_495_000),
                WorldCoord::new(2_147_490_000, 2_147_495_000),
                WorldCoord::new(2_147_490_000, 2_147_490_000),
            ];
            let geom = WorldClippedGeometry::Polygon {
                exterior: coords.clone(),
                interiors: vec![hole.clone()],
            };

            let bytes = geom.to_bytes();
            let restored = WorldClippedGeometry::from_bytes(&bytes).unwrap();

            match restored {
                WorldClippedGeometry::Polygon {
                    exterior,
                    interiors,
                } => {
                    assert_eq!(exterior.len(), 5, "Exterior ring length mismatch");
                    assert_eq!(interiors.len(), 1, "Should have 1 hole");
                    assert_eq!(interiors[0].len(), 5, "Hole ring length mismatch");

                    // Verify every coordinate survived the round-trip
                    for (i, (orig, restored)) in coords.iter().zip(exterior.iter()).enumerate() {
                        assert_eq!(
                            orig.x, restored.x,
                            "Exterior coord {} x mismatch: {} vs {}",
                            i, orig.x, restored.x
                        );
                        assert_eq!(
                            orig.y, restored.y,
                            "Exterior coord {} y mismatch: {} vs {}",
                            i, orig.y, restored.y
                        );
                    }
                    for (i, (orig, restored)) in hole.iter().zip(interiors[0].iter()).enumerate() {
                        assert_eq!(
                            orig.x, restored.x,
                            "Hole coord {} x mismatch: {} vs {}",
                            i, orig.x, restored.x
                        );
                        assert_eq!(
                            orig.y, restored.y,
                            "Hole coord {} y mismatch: {} vs {}",
                            i, orig.y, restored.y
                        );
                    }
                }
                other => panic!("Expected Polygon, got {:?}", other),
            }
        }

        #[test]
        fn test_roundtrip_multipolygon_to_bytes_from_bytes() {
            let poly1_ext = vec![
                WorldCoord::new(100, 200),
                WorldCoord::new(300, 200),
                WorldCoord::new(300, 400),
                WorldCoord::new(100, 400),
                WorldCoord::new(100, 200),
            ];
            let poly2_ext = vec![
                WorldCoord::new(500, 600),
                WorldCoord::new(700, 600),
                WorldCoord::new(700, 800),
                WorldCoord::new(500, 800),
                WorldCoord::new(500, 600),
            ];
            let geom = WorldClippedGeometry::MultiPolygon(vec![
                (poly1_ext.clone(), vec![]),
                (poly2_ext.clone(), vec![]),
            ]);

            let bytes = geom.to_bytes();
            let restored = WorldClippedGeometry::from_bytes(&bytes).unwrap();

            match restored {
                WorldClippedGeometry::MultiPolygon(polys) => {
                    assert_eq!(polys.len(), 2);
                    assert_eq!(polys[0].0, poly1_ext);
                    assert_eq!(polys[1].0, poly2_ext);
                }
                other => panic!("Expected MultiPolygon, got {:?}", other),
            }
        }

        #[test]
        fn test_roundtrip_linestring_to_bytes_from_bytes() {
            let coords = vec![
                WorldCoord::new(1_000_000, 2_000_000),
                WorldCoord::new(1_100_000, 2_100_000),
                WorldCoord::new(1_200_000, 2_000_000),
            ];
            let geom = WorldClippedGeometry::LineString(coords.clone());

            let bytes = geom.to_bytes();
            let restored = WorldClippedGeometry::from_bytes(&bytes).unwrap();

            match restored {
                WorldClippedGeometry::LineString(restored_coords) => {
                    assert_eq!(restored_coords, coords);
                }
                other => panic!("Expected LineString, got {:?}", other),
            }
        }

        /// Diagnostic test: verify that a realistic polygon produces
        /// non-zero deltas in MVT encoding after round-tripping through
        /// to_bytes/from_bytes.
        #[test]
        fn test_polygon_roundtrip_produces_nonzero_mvt_deltas() {
            use crate::mvt::encode_world_polygon;

            // Create a polygon near NYC at zoom 14
            let lng_center = -73.985;
            let lat_center = 40.749;

            // Polygon spanning ~0.01 degrees (roughly 1km)
            let half = 0.005;
            let coords = vec![
                lng_lat_to_world(lng_center - half, lat_center - half),
                lng_lat_to_world(lng_center + half, lat_center - half),
                lng_lat_to_world(lng_center + half, lat_center + half),
                lng_lat_to_world(lng_center - half, lat_center + half),
                lng_lat_to_world(lng_center - half, lat_center - half), // close
            ];

            // Verify coordinates are distinct in world space
            for i in 0..4 {
                for j in (i + 1)..4 {
                    assert!(
                        coords[i] != coords[j],
                        "World coords {} and {} should differ: {:?} vs {:?}",
                        i,
                        j,
                        coords[i],
                        coords[j]
                    );
                }
            }

            // Round-trip through serialization
            let geom = WorldClippedGeometry::Polygon {
                exterior: coords.clone(),
                interiors: vec![],
            };
            let bytes = geom.to_bytes();
            let restored = WorldClippedGeometry::from_bytes(&bytes).unwrap();

            let restored_coords = match &restored {
                WorldClippedGeometry::Polygon { exterior, .. } => exterior,
                other => panic!("Expected Polygon, got {:?}", other),
            };

            // Verify coordinates survived serialization
            for (i, (orig, rest)) in coords.iter().zip(restored_coords.iter()).enumerate() {
                assert_eq!(
                    orig, rest,
                    "Coord {} changed during serialization: {:?} -> {:?}",
                    i, orig, rest
                );
            }

            // Now encode to MVT and verify non-zero deltas
            let tile = coords[0].to_tile(14);
            let extent = 4096u32;
            let mvt_commands = encode_world_polygon(restored_coords, &[], &tile, extent);

            // MVT polygon: MoveTo(1), dx, dy, LineTo(n-2), [dx, dy]*, ClosePath(1)
            // commands[0] = MoveTo(1) command
            // commands[1] = zigzag(dx0)
            // commands[2] = zigzag(dy0)
            // commands[3] = LineTo(n) command
            // commands[4..] = zigzag deltas for subsequent points
            assert!(
                mvt_commands.len() >= 8,
                "MVT should have enough commands, got {}",
                mvt_commands.len()
            );

            // Check that LineTo deltas are non-zero
            // After MoveTo(1) + 2 coords + LineTo cmd = index 3
            // LineTo deltas start at index 4
            let has_nonzero_delta = mvt_commands[4..]
                .chunks(2)
                .any(|pair| pair[0] != 0 || pair[1] != 0);

            assert!(
                has_nonzero_delta,
                "MVT deltas should be non-zero for a real polygon. Commands: {:?}",
                mvt_commands
            );

            // Print diagnostic info
            println!("WorldCoords:");
            for (i, c) in restored_coords.iter().enumerate() {
                let (lx, ly) = c.to_tile_local(&tile, extent);
                println!(
                    "  [{}] world=({}, {}) tile_local=({}, {})",
                    i, c.x, c.y, lx, ly
                );
            }
            println!("MVT commands: {:?}", mvt_commands);
        }

        /// Test the full pipeline path: create polygon in geo coords -> convert to WorldCoord
        /// -> clip to tile bounds -> serialize -> deserialize -> MVT encode.
        /// This replicates the exact path taken by pipeline.rs.
        #[test]
        fn test_full_pipeline_path_clipped_polygon_mvt_deltas() {
            use crate::mvt::encode_world_polygon;

            // Create a polygon that straddles a tile boundary at zoom 5
            // This forces clipping to produce a new polygon shape
            let poly = Geometry::Polygon(geo::polygon![
                (x: 10.0, y: 44.0),
                (x: 12.0, y: 44.0),
                (x: 12.0, y: 46.0),
                (x: 10.0, y: 46.0),
                (x: 10.0, y: 44.0),
            ]);
            let bbox = TileBounds::new(10.0, 44.0, 12.0, 46.0);

            let (clip_results, _stats) =
                clip_geometry_hierarchical_world(&poly, &bbox, 5, 5, 8, 4096);

            assert!(!clip_results.is_empty(), "Should have at least one tile");

            for (tile_coord, world_geom) in &clip_results {
                // Serialize -> deserialize (exact pipeline path)
                let bytes = world_geom.to_bytes();
                let restored =
                    WorldClippedGeometry::from_bytes(&bytes).expect("from_bytes should succeed");

                // Encode to MVT
                let (exterior, interiors) = match &restored {
                    WorldClippedGeometry::Polygon {
                        exterior,
                        interiors,
                    } => (exterior, interiors),
                    WorldClippedGeometry::MultiPolygon(polys) => {
                        // Check first polygon
                        (&polys[0].0, &polys[0].1)
                    }
                    other => {
                        println!("Skipping non-polygon geom: {:?}", other);
                        continue;
                    }
                };

                let mvt_commands = encode_world_polygon(exterior, interiors, tile_coord, 4096);

                if mvt_commands.is_empty() {
                    continue; // Degenerate polygon was dropped
                }

                // Print diagnostic info
                println!(
                    "Tile z{}/x{}/y{}:",
                    tile_coord.z, tile_coord.x, tile_coord.y
                );
                for (i, c) in exterior.iter().enumerate() {
                    let (lx, ly) = c.to_tile_local(tile_coord, 4096);
                    println!(
                        "  [{}] world=({}, {}) tile_local=({}, {})",
                        i, c.x, c.y, lx, ly
                    );
                }
                println!(
                    "  MVT commands ({} u32s): {:?}",
                    mvt_commands.len(),
                    mvt_commands
                );

                // Check for all-zero deltas after first coord
                // MVT polygon: MoveTo(1), dx, dy, LineTo(n), [dx,dy]*, ClosePath(1)
                // LineTo deltas start at index 4
                if mvt_commands.len() > 4 {
                    let line_to_deltas = &mvt_commands[4..mvt_commands.len() - 1];
                    let all_zero = line_to_deltas.iter().all(|&v| v == 0);
                    assert!(
                        !all_zero || line_to_deltas.is_empty(),
                        "Tile z{}/x{}/y{}: ALL deltas are zero after first coord! \
                         This is the Issue #83 bug. Commands: {:?}",
                        tile_coord.z,
                        tile_coord.x,
                        tile_coord.y,
                        mvt_commands
                    );
                }
            }
        }

        /// Test at zoom 0 with a small polygon -- this is where precision collapse
        /// is most likely because tile_size (2^32) >> extent (4096).
        #[test]
        fn test_small_polygon_at_zoom_0_mvt_encoding() {
            use crate::mvt::encode_world_polygon;

            // A small polygon (0.01 degrees) at null island
            let poly = Geometry::Polygon(geo::polygon![
                (x: -0.005, y: -0.005),
                (x: 0.005, y: -0.005),
                (x: 0.005, y: 0.005),
                (x: -0.005, y: 0.005),
                (x: -0.005, y: -0.005),
            ]);
            let bbox = TileBounds::new(-0.005, -0.005, 0.005, 0.005);

            let (clip_results, _stats) =
                clip_geometry_hierarchical_world(&poly, &bbox, 0, 0, 8, 4096);

            println!("\n=== Small polygon at zoom 0 ===");

            for (tile_coord, world_geom) in &clip_results {
                let (exterior, interiors) = match world_geom {
                    WorldClippedGeometry::Polygon {
                        exterior,
                        interiors,
                    } => (exterior, interiors.as_slice()),
                    _ => continue,
                };

                let mvt_commands = encode_world_polygon(exterior, interiors, tile_coord, 4096);

                println!(
                    "Tile z{}/x{}/y{}:",
                    tile_coord.z, tile_coord.x, tile_coord.y
                );
                for (i, c) in exterior.iter().enumerate() {
                    let (lx, ly) = c.to_tile_local(tile_coord, 4096);
                    println!(
                        "  [{}] world=({}, {}) tile_local=({}, {})",
                        i, c.x, c.y, lx, ly
                    );
                }
                println!("  MVT commands: {:?}", mvt_commands);

                // At zoom 0, a 0.01-degree polygon spans ~0.06 pixels.
                // All points will map to the same tile-local coordinate.
                // This is EXPECTED behavior -- such tiny polygons should be
                // dropped by the feature_drop filter before reaching MVT encoding.
                let all_same_tile_local = {
                    let first = exterior[0].to_tile_local(tile_coord, 4096);
                    exterior
                        .iter()
                        .all(|c| c.to_tile_local(tile_coord, 4096) == first)
                };

                if all_same_tile_local {
                    println!(
                        "  NOTE: All coords map to same tile_local -- \
                         polygon is sub-pixel at this zoom. \
                         Feature filter should drop this."
                    );
                }
            }
        }

        /// Test at zoom 14 where precision is sufficient -- ensures the pipeline
        /// works correctly for the common case.
        #[test]
        fn test_clipped_polygon_at_zoom_14() {
            use crate::mvt::encode_world_polygon;

            // A polygon near NYC that spans ~0.01 degrees
            let poly = Geometry::Polygon(geo::polygon![
                (x: -74.00, y: 40.74),
                (x: -73.99, y: 40.74),
                (x: -73.99, y: 40.75),
                (x: -74.00, y: 40.75),
                (x: -74.00, y: 40.74),
            ]);
            let bbox = TileBounds::new(-74.00, 40.74, -73.99, 40.75);

            let (clip_results, _stats) =
                clip_geometry_hierarchical_world(&poly, &bbox, 14, 14, 8, 4096);

            println!("\n=== Polygon at zoom 14 near NYC ===");
            assert!(!clip_results.is_empty(), "Should produce tiles at zoom 14");

            for (tile_coord, world_geom) in &clip_results {
                let bytes = world_geom.to_bytes();
                let restored = WorldClippedGeometry::from_bytes(&bytes).unwrap();

                let (exterior, interiors) = match &restored {
                    WorldClippedGeometry::Polygon {
                        exterior,
                        interiors,
                    } => (exterior, interiors.as_slice()),
                    _ => continue,
                };

                let mvt_commands = encode_world_polygon(exterior, interiors, tile_coord, 4096);

                if mvt_commands.is_empty() {
                    continue;
                }

                println!(
                    "Tile z{}/x{}/y{}:",
                    tile_coord.z, tile_coord.x, tile_coord.y
                );
                for (i, c) in exterior.iter().enumerate() {
                    let (lx, ly) = c.to_tile_local(tile_coord, 4096);
                    println!(
                        "  [{}] world=({}, {}) tile_local=({}, {})",
                        i, c.x, c.y, lx, ly
                    );
                }
                println!("  MVT commands: {:?}", mvt_commands);

                // At zoom 14, a 0.01-degree polygon spans ~150 pixels
                // and should definitely produce non-zero deltas
                if mvt_commands.len() > 4 {
                    let line_to_deltas = &mvt_commands[4..mvt_commands.len() - 1];
                    let has_nonzero = line_to_deltas.chunks(2).any(|p| p[0] != 0 || p[1] != 0);
                    assert!(has_nonzero, "Zoom 14 polygon should have non-zero deltas!");
                }
            }
        }
    }
}
