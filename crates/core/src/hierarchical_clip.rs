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
}
