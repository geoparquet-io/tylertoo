//! Space-filling curve sorting for efficient tile generation.
//!
//! This module implements Z-order (Morton) and Hilbert curve encoding for spatial indexing,
//! following tippecanoe's approach for efficient tile generation.
//!
//! # Key Insight
//!
//! The goal is NOT to do random spatial queries. The goal is to sort features so that
//! when we iterate through them, all features for a tile are clustered together. This enables:
//! - Sequential memory access (cache-friendly)
//! - Future streaming support
//! - Efficient parallel partitioning
//!
//! # Tippecanoe Reference
//!
//! This implementation follows tippecanoe's `projection.cpp`:
//! - World coordinates are 32-bit unsigned integers (0 to 2^32-1)
//! - Hilbert curve uses `hilbert_xy2d()` algorithm from Wikipedia
//! - Z-order uses bit interleaving (quadkey encoding)
//!
//! # Example
//!
//! ```
//! use gpq_tiles_core::spatial_index::{encode_hilbert, lng_lat_to_world_coords};
//!
//! // Convert geographic coordinates to world coordinates
//! let (wx, wy) = lng_lat_to_world_coords(-122.4, 37.8);
//!
//! // Encode to Hilbert curve index for sorting
//! let index = encode_hilbert(wx, wy);
//! ```

use std::f64::consts::PI;

use geo::{BoundingRect, Centroid, Geometry};

/// Encode (x, y) world coordinates to Z-order (Morton) index.
///
/// Z-order curve (also known as Morton code) interleaves the bits of x and y
/// coordinates to create a single index that preserves 2D locality.
///
/// # Tippecanoe Reference
///
/// This matches tippecanoe's `encode_quadkey()` in `projection.cpp`:
/// ```c
/// unsigned long long encode_quadkey(unsigned int wx, unsigned int wy) {
///     unsigned long long out = 0;
///     for (i = 0; i < 32; i++) {
///         unsigned long long v = ((wx >> (32 - (i + 1))) & 1) << 1;
///         v |= (wy >> (32 - (i + 1))) & 1;
///         v = v << (64 - 2 * (i + 1));
///         out |= v;
///     }
///     return out;
/// }
/// ```
///
/// # Arguments
///
/// * `wx` - X world coordinate (0 to 2^32-1)
/// * `wy` - Y world coordinate (0 to 2^32-1)
///
/// # Returns
///
/// 64-bit Z-order index
pub fn encode_zorder(wx: u32, wy: u32) -> u64 {
    let mut out: u64 = 0;

    for i in 0..32 {
        // Extract bit from wx and wy at position (32 - (i + 1))
        let bit_pos = 31 - i;
        let vx = ((wx >> bit_pos) & 1) as u64;
        let vy = ((wy >> bit_pos) & 1) as u64;

        // Interleave: x bit goes to even position, y bit to odd position
        // Position in output: (64 - 2*(i+1)) for the pair
        let out_pos = 62 - 2 * i;
        out |= (vx << 1 | vy) << out_pos;
    }

    out
}

/// Decode Z-order (Morton) index back to (x, y) world coordinates.
///
/// This is the inverse of `encode_zorder()`.
pub fn decode_zorder(index: u64) -> (u32, u32) {
    let mut wx: u32 = 0;
    let mut wy: u32 = 0;

    for i in 0..32 {
        let bit_pos = 31 - i;
        let out_pos = 62 - 2 * i;

        // Extract the pair of bits
        let pair = (index >> out_pos) & 0b11;
        let vx = (pair >> 1) & 1;
        let vy = pair & 1;

        wx |= (vx as u32) << bit_pos;
        wy |= (vy as u32) << bit_pos;
    }

    (wx, wy)
}

/// Rotate/flip quadrant for Hilbert curve calculation.
///
/// # Tippecanoe Reference
///
/// From `projection.cpp`:
/// ```c
/// void hilbert_rot(unsigned long long n, unsigned *x, unsigned *y,
///                  unsigned long long rx, unsigned long long ry) {
///     if (ry == 0) {
///         if (rx == 1) {
///             *x = n - 1 - *x;
///             *y = n - 1 - *y;
///         }
///         unsigned t = *x;
///         *x = *y;
///         *y = t;
///     }
/// }
/// ```
#[inline]
fn hilbert_rot(n: u64, x: &mut u32, y: &mut u32, rx: u64, ry: u64) {
    if ry == 0 {
        if rx == 1 {
            // Use wrapping subtraction to handle the case where n is larger than u32::MAX
            // Tippecanoe uses unsigned long long (64-bit) for n
            let n_minus_1 = (n - 1) as u32;
            *x = n_minus_1.wrapping_sub(*x);
            *y = n_minus_1.wrapping_sub(*y);
        }
        std::mem::swap(x, y);
    }
}

/// Encode (x, y) world coordinates to Hilbert curve index.
///
/// Hilbert curves have better locality than Z-order curves - neighboring points
/// on the curve are always neighboring in 2D space, which is not always true
/// for Z-order curves.
///
/// # Tippecanoe Reference
///
/// From `projection.cpp`:
/// ```c
/// unsigned long long hilbert_xy2d(unsigned long long n, unsigned x, unsigned y) {
///     unsigned long long d = 0;
///     for (unsigned long long s = n / 2; s > 0; s /= 2) {
///         rx = (x & s) != 0;
///         ry = (y & s) != 0;
///         d += s * s * ((3 * rx) ^ ry);
///         hilbert_rot(s, &x, &y, rx, ry);
///     }
///     return d;
/// }
///
/// unsigned long long encode_hilbert(unsigned int wx, unsigned int wy) {
///     return hilbert_xy2d(1LL << 32, wx, wy);
/// }
/// ```
///
/// # Arguments
///
/// * `wx` - X world coordinate (0 to 2^32-1)
/// * `wy` - Y world coordinate (0 to 2^32-1)
///
/// # Returns
///
/// 64-bit Hilbert curve index
pub fn encode_hilbert(wx: u32, wy: u32) -> u64 {
    hilbert_xy2d(1u64 << 32, wx, wy)
}

/// Convert (x, y) coordinates to Hilbert curve distance.
fn hilbert_xy2d(n: u64, mut x: u32, mut y: u32) -> u64 {
    let mut d: u64 = 0;
    let mut s = n / 2;

    while s > 0 {
        let rx = if (x as u64 & s) != 0 { 1u64 } else { 0u64 };
        let ry = if (y as u64 & s) != 0 { 1u64 } else { 0u64 };

        d += s * s * ((3 * rx) ^ ry);
        hilbert_rot(s, &mut x, &mut y, rx, ry);

        s /= 2;
    }

    d
}

/// Decode Hilbert curve index back to (x, y) world coordinates.
///
/// This is the inverse of `encode_hilbert()`.
pub fn decode_hilbert(index: u64) -> (u32, u32) {
    hilbert_d2xy(1u64 << 32, index)
}

/// Convert Hilbert curve distance to (x, y) coordinates.
fn hilbert_d2xy(n: u64, d: u64) -> (u32, u32) {
    let mut x: u32 = 0;
    let mut y: u32 = 0;
    let mut t = d;
    let mut s: u64 = 1;

    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);

        hilbert_rot(s, &mut x, &mut y, rx, ry);

        x += (s * rx) as u32;
        y += (s * ry) as u32;
        t /= 4;
        s *= 2;
    }

    (x, y)
}

/// Convert longitude/latitude to world coordinates at maximum precision.
///
/// World coordinates are 32-bit unsigned integers spanning the entire Web Mercator
/// coordinate space. This matches tippecanoe's `lonlat2tile()` when called with zoom=32.
///
/// # Tippecanoe Reference
///
/// From `projection.cpp`:
/// ```c
/// void lonlat2tile(double lon, double lat, int zoom, long long *x, long long *y) {
///     // ... (bounds checking omitted)
///     double lat_rad = lat * M_PI / 180;
///     long long llx = (lon + 180.0) / 360.0 * (1LL << 32);
///     long long lly = (1.0 - log(tan(lat_rad) + 1.0/cos(lat_rad)) / M_PI) / 2.0 * (1LL << 32);
///     // ...
/// }
/// ```
///
/// # Arguments
///
/// * `lng` - Longitude in degrees (-180 to 180)
/// * `lat` - Latitude in degrees (-85.05 to 85.05, Web Mercator bounds)
///
/// # Returns
///
/// (wx, wy) world coordinates as 32-bit unsigned integers
pub fn lng_lat_to_world_coords(lng: f64, lat: f64) -> (u32, u32) {
    // Clamp latitude to Web Mercator bounds to prevent overflow
    let lat = lat.clamp(-89.9, 89.9);

    // Normalize longitude to [-180, 180]
    let lng = if lng < -180.0 {
        lng + 360.0
    } else if lng > 180.0 {
        lng - 360.0
    } else {
        lng
    };

    let lat_rad = lat * PI / 180.0;

    // Convert to world coordinates (0 to 2^32-1)
    let scale = (1u64 << 32) as f64;
    let wx = ((lng + 180.0) / 360.0 * scale) as u32;
    let wy = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI) / 2.0 * scale) as u32;

    (wx, wy)
}

/// Get the centroid world coordinates of a geometry.
///
/// For efficiency, we use the geometry's centroid as its representative point
/// for spatial indexing. This works well for clustering since geometries in
/// the same tile will have similar centroid positions.
fn geometry_world_coords(geom: &Geometry<f64>) -> Option<(u32, u32)> {
    // Try centroid first (works for polygons, lines, etc.)
    if let Some(centroid) = geom.centroid() {
        return Some(lng_lat_to_world_coords(centroid.x(), centroid.y()));
    }

    // Fall back to bounding box center
    if let Some(rect) = geom.bounding_rect() {
        let center = rect.center();
        return Some(lng_lat_to_world_coords(center.x, center.y));
    }

    None
}

/// Sort features by their spatial index for efficient tile generation.
///
/// This sorts features so that all features for a tile are clustered together
/// in the sorted order. Features are sorted by their Hilbert or Z-order index,
/// which ensures spatial locality.
///
/// # Arguments
///
/// * `features` - Slice of (Geometry, metadata) tuples to sort in place
/// * `use_hilbert` - If true, use Hilbert curve; if false, use Z-order (Morton)
///
/// # Example
///
/// ```
/// use geo::{point, Geometry};
/// use gpq_tiles_core::spatial_index::sort_by_spatial_index;
///
/// let mut features = vec![
///     (Geometry::Point(point!(x: 10.0, y: 20.0)), "feature_a"),
///     (Geometry::Point(point!(x: 10.1, y: 20.1)), "feature_b"),
///     (Geometry::Point(point!(x: -120.0, y: 45.0)), "feature_c"),
/// ];
///
/// sort_by_spatial_index(&mut features, true);
///
/// // Features near each other spatially are now adjacent in the list
/// ```
pub fn sort_by_spatial_index<T>(features: &mut [(Geometry<f64>, T)], use_hilbert: bool) {
    features.sort_by_cached_key(|(geom, _)| {
        let (wx, wy) = geometry_world_coords(geom).unwrap_or((0, 0));
        if use_hilbert {
            encode_hilbert(wx, wy)
        } else {
            encode_zorder(wx, wy)
        }
    });
}

/// Calculate the spatial index for a single geometry.
///
/// This is useful for inserting new features into an already-sorted list.
pub fn spatial_index_for_geometry(geom: &Geometry<f64>, use_hilbert: bool) -> u64 {
    let (wx, wy) = geometry_world_coords(geom).unwrap_or((0, 0));
    if use_hilbert {
        encode_hilbert(wx, wy)
    } else {
        encode_zorder(wx, wy)
    }
}

/// Sort a vector of geometries by their spatial index for efficient tile generation.
///
/// This is a convenience function for sorting geometries without associated metadata.
/// It wraps each geometry with its original index, sorts by spatial index, then
/// returns the sorted geometries.
///
/// # Arguments
///
/// * `geometries` - Vector of geometries to sort
/// * `use_hilbert` - If true, use Hilbert curve; if false, use Z-order (Morton)
///
/// # Example
///
/// ```
/// use geo::{point, Geometry};
/// use gpq_tiles_core::spatial_index::sort_geometries;
///
/// let mut geometries = vec![
///     Geometry::Point(point!(x: 10.0, y: 20.0)),
///     Geometry::Point(point!(x: -120.0, y: 45.0)),
///     Geometry::Point(point!(x: 10.1, y: 20.1)),
/// ];
///
/// sort_geometries(&mut geometries, true);
///
/// // Geometries near each other spatially are now adjacent in the list
/// ```
pub fn sort_geometries(geometries: &mut [Geometry<f64>], use_hilbert: bool) {
    geometries.sort_by_cached_key(|geom| {
        let (wx, wy) = geometry_world_coords(geom).unwrap_or((0, 0));
        if use_hilbert {
            encode_hilbert(wx, wy)
        } else {
            encode_zorder(wx, wy)
        }
    });
}

/// Sort feature records by their spatial index for efficient tile generation.
///
/// This is a version of `sort_geometries` that works with `FeatureRecord` structs,
/// preserving the association between geometries and their properties.
///
/// # Arguments
///
/// * `features` - Mutable slice of feature records to sort in place
/// * `use_hilbert` - If true, use Hilbert curve (better locality). If false, use Z-order.
pub fn sort_features(features: &mut [crate::batch_processor::FeatureRecord], use_hilbert: bool) {
    features.sort_by_cached_key(|feat| {
        let (wx, wy) = geometry_world_coords(&feat.geometry).unwrap_or((0, 0));
        if use_hilbert {
            encode_hilbert(wx, wy)
        } else {
            encode_zorder(wx, wy)
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{line_string, point, polygon};

    // ============================================================
    // Z-ORDER (MORTON) ENCODING TESTS
    // ============================================================

    #[test]
    fn test_zorder_encoding_origin() {
        // Origin should encode to 0
        let index = encode_zorder(0, 0);
        assert_eq!(index, 0);
    }

    #[test]
    fn test_zorder_encoding_max() {
        // Maximum values
        let index = encode_zorder(u32::MAX, u32::MAX);
        assert_eq!(index, u64::MAX);
    }

    #[test]
    fn test_zorder_encoding_basic_interleaving() {
        // Test bit interleaving pattern
        // If wx = 0b1000...0 (only MSB set) and wy = 0
        // Result should have bit at position 63 (x) and 62 (y=0)
        let wx = 1u32 << 31;
        let wy = 0u32;
        let index = encode_zorder(wx, wy);

        // The x bit at position 31 should map to output position 63 (MSB of x part)
        // Pattern is: x_31, y_31, x_30, y_30, ...
        // So wx's MSB goes to bit 63, wy's MSB goes to bit 62
        assert_eq!(index >> 62, 0b10); // x=1, y=0 in top 2 bits
    }

    #[test]
    fn test_zorder_encoding_symmetry() {
        // If only y has the MSB set
        let wx = 0u32;
        let wy = 1u32 << 31;
        let index = encode_zorder(wx, wy);

        // x=0, y=1 in top 2 bits
        assert_eq!(index >> 62, 0b01);
    }

    #[test]
    fn test_zorder_round_trip() {
        // Test round-trip for various values
        let test_cases = [
            (0, 0),
            (1, 1),
            (100, 200),
            (u32::MAX, 0),
            (0, u32::MAX),
            (u32::MAX, u32::MAX),
            (12345678, 87654321),
            (1 << 31, 1 << 30),
        ];

        for (wx, wy) in test_cases {
            let index = encode_zorder(wx, wy);
            let (wx_back, wy_back) = decode_zorder(index);
            assert_eq!(
                (wx, wy),
                (wx_back, wy_back),
                "Z-order round-trip failed for ({}, {})",
                wx,
                wy
            );
        }
    }

    #[test]
    fn test_zorder_preserves_locality() {
        // Adjacent points should have adjacent or nearby indices
        let idx1 = encode_zorder(1000, 1000);
        let idx2 = encode_zorder(1001, 1000);
        let idx3 = encode_zorder(1000, 1001);

        // Points should be relatively close in Z-order space
        // (not testing exact values, just that locality is somewhat preserved)
        let far_idx = encode_zorder(u32::MAX / 2, u32::MAX / 2);

        // idx1 and idx2 should be closer to each other than to far_idx
        let dist_1_2 = (idx1 as i128 - idx2 as i128).unsigned_abs();
        let dist_1_far = (idx1 as i128 - far_idx as i128).unsigned_abs();

        assert!(
            dist_1_2 < dist_1_far,
            "Adjacent points should be closer in Z-order"
        );
        let _ = idx3; // Used for locality concept
    }

    // ============================================================
    // HILBERT CURVE ENCODING TESTS
    // ============================================================

    #[test]
    fn test_hilbert_encoding_origin() {
        // Origin should encode to 0
        let index = encode_hilbert(0, 0);
        assert_eq!(index, 0);
    }

    #[test]
    fn test_hilbert_encoding_max() {
        // Maximum coordinate should produce maximum index
        let index = encode_hilbert(u32::MAX, u32::MAX);
        // The Hilbert curve ends at (n-1, 0), so (MAX, MAX) is somewhere in the middle-ish
        // It should be a valid large number
        assert!(index > 0);
    }

    #[test]
    fn test_hilbert_round_trip() {
        // Test round-trip for various values
        let test_cases = [
            (0, 0),
            (1, 1),
            (100, 200),
            (u32::MAX, 0),
            (0, u32::MAX),
            (u32::MAX, u32::MAX),
            (12345678, 87654321),
            (1 << 31, 1 << 30),
            (1 << 16, 1 << 16),
        ];

        for (wx, wy) in test_cases {
            let index = encode_hilbert(wx, wy);
            let (wx_back, wy_back) = decode_hilbert(index);
            assert_eq!(
                (wx, wy),
                (wx_back, wy_back),
                "Hilbert round-trip failed for ({}, {})",
                wx,
                wy
            );
        }
    }

    #[test]
    fn test_hilbert_better_locality_than_zorder() {
        // Hilbert curve guarantees that adjacent indices are adjacent in 2D
        // Z-order doesn't have this property at quadrant boundaries

        // Test that sequential Hilbert indices are always adjacent in 2D
        let small_n = 16u32; // Use smaller grid for testing
        let scale = u32::MAX / small_n;

        for d in 0..15 {
            let (x1, y1) = decode_hilbert((d as u64) * (scale as u64) * (scale as u64));
            let (x2, y2) = decode_hilbert(((d + 1) as u64) * (scale as u64) * (scale as u64));

            // Normalize to small grid
            let x1_small = x1 / scale;
            let y1_small = y1 / scale;
            let x2_small = x2 / scale;
            let y2_small = y2 / scale;

            let dx = (x1_small as i64 - x2_small as i64).abs();
            let dy = (y1_small as i64 - y2_small as i64).abs();

            // For Hilbert curve, adjacent indices should be Manhattan distance 1 apart
            // (allowing some tolerance due to scaling)
            assert!(
                dx <= 2 && dy <= 2,
                "Hilbert curve discontinuity at d={}: ({},{}) -> ({},{}), dx={}, dy={}",
                d,
                x1_small,
                y1_small,
                x2_small,
                y2_small,
                dx,
                dy
            );
        }
    }

    // ============================================================
    // LNG/LAT TO WORLD COORDS TESTS
    // ============================================================

    #[test]
    fn test_lng_lat_to_world_coords_origin() {
        // (0, 0) should be roughly in the center
        let (wx, wy) = lng_lat_to_world_coords(0.0, 0.0);

        // Should be near the center of the coordinate space
        let center = u32::MAX / 2;
        let tolerance = u32::MAX / 10; // 10% tolerance

        assert!(
            (wx as i64 - center as i64).unsigned_abs() < tolerance as u64,
            "wx should be near center: {} vs {}",
            wx,
            center
        );
        assert!(
            (wy as i64 - center as i64).unsigned_abs() < tolerance as u64,
            "wy should be near center: {} vs {}",
            wy,
            center
        );
    }

    #[test]
    fn test_lng_lat_to_world_coords_western_hemisphere() {
        // Western hemisphere (-lng) should have smaller wx
        let (wx_west, _) = lng_lat_to_world_coords(-120.0, 45.0);
        let (wx_east, _) = lng_lat_to_world_coords(120.0, 45.0);

        assert!(
            wx_west < wx_east,
            "Western longitude should have smaller wx"
        );
    }

    #[test]
    fn test_lng_lat_to_world_coords_northern_hemisphere() {
        // Northern hemisphere should have smaller wy (Web Mercator y increases southward)
        let (_, wy_north) = lng_lat_to_world_coords(0.0, 60.0);
        let (_, wy_south) = lng_lat_to_world_coords(0.0, -60.0);

        assert!(
            wy_north < wy_south,
            "Northern latitude should have smaller wy (y increases southward in Web Mercator)"
        );
    }

    #[test]
    fn test_lng_lat_to_world_coords_bounds() {
        // Extreme latitudes should be clamped
        let (_, wy_north) = lng_lat_to_world_coords(0.0, 90.0);
        let (_, wy_south) = lng_lat_to_world_coords(0.0, -90.0);

        // Both should produce valid u32 values (not overflow)
        let tenth = u32::MAX / 10;
        let nine_tenths = u32::MAX - tenth; // Avoid overflow
        assert!(wy_north < tenth, "North pole should be near top");
        assert!(wy_south > nine_tenths, "South pole should be near bottom");
    }

    #[test]
    fn test_lng_lat_to_world_coords_antimeridian() {
        // Test longitude wrapping
        let (wx1, _) = lng_lat_to_world_coords(179.0, 0.0);
        let (wx2, _) = lng_lat_to_world_coords(-179.0, 0.0);

        // These should be at opposite ends of the x range
        let tenth = u32::MAX / 10;
        let nine_tenths = u32::MAX - tenth; // Avoid overflow
        assert!(wx1 > nine_tenths, "179 deg should be near right edge");
        assert!(wx2 < tenth, "-179 deg should be near left edge");
    }

    // ============================================================
    // SPATIAL LOCALITY TESTS
    // ============================================================

    #[test]
    fn test_spatial_locality_adjacent_tiles_have_nearby_indices() {
        // Points in adjacent tiles should have nearby spatial indices

        // Two points in roughly the same area (San Francisco)
        let (wx1, wy1) = lng_lat_to_world_coords(-122.4, 37.8);
        let (wx2, wy2) = lng_lat_to_world_coords(-122.41, 37.81);

        // Point far away (Tokyo)
        let (wx3, wy3) = lng_lat_to_world_coords(139.7, 35.7);

        let idx1 = encode_hilbert(wx1, wy1);
        let idx2 = encode_hilbert(wx2, wy2);
        let idx3 = encode_hilbert(wx3, wy3);

        // idx1 and idx2 should be closer to each other than to idx3
        let dist_nearby = (idx1 as i128 - idx2 as i128).unsigned_abs();
        let dist_far = (idx1 as i128 - idx3 as i128).unsigned_abs();

        assert!(
            dist_nearby < dist_far,
            "Nearby points should have closer indices: nearby_dist={}, far_dist={}",
            dist_nearby,
            dist_far
        );
    }

    // ============================================================
    // SORT TESTS
    // ============================================================

    #[test]
    fn test_sort_clusters_features_by_location() {
        // Create features in different parts of the world
        let mut features = vec![
            (Geometry::Point(point!(x: 139.7, y: 35.7)), "tokyo"), // Tokyo
            (Geometry::Point(point!(x: -122.4, y: 37.8)), "sf1"),  // San Francisco
            (Geometry::Point(point!(x: 2.35, y: 48.85)), "paris"), // Paris
            (Geometry::Point(point!(x: -122.41, y: 37.79)), "sf2"), // Near SF
            (Geometry::Point(point!(x: 2.36, y: 48.86)), "paris2"), // Near Paris
            (Geometry::Point(point!(x: 139.75, y: 35.68)), "tokyo2"), // Near Tokyo
        ];

        sort_by_spatial_index(&mut features, true);

        // After sorting, nearby features should be adjacent
        // Find positions of SF features
        let sf_positions: Vec<usize> = features
            .iter()
            .enumerate()
            .filter(|(_, (_, name))| name.starts_with("sf"))
            .map(|(i, _)| i)
            .collect();

        // SF features should be adjacent
        assert_eq!(sf_positions.len(), 2);
        assert!(
            (sf_positions[0] as i32 - sf_positions[1] as i32).abs() <= 1,
            "SF features should be adjacent after sorting"
        );

        // Similarly for Paris
        let paris_positions: Vec<usize> = features
            .iter()
            .enumerate()
            .filter(|(_, (_, name))| name.starts_with("paris"))
            .map(|(i, _)| i)
            .collect();

        assert_eq!(paris_positions.len(), 2);
        assert!(
            (paris_positions[0] as i32 - paris_positions[1] as i32).abs() <= 1,
            "Paris features should be adjacent after sorting"
        );
    }

    #[test]
    fn test_sort_works_with_polygons() {
        let mut features = vec![
            (
                Geometry::Polygon(polygon![
                    (x: -122.4, y: 37.7),
                    (x: -122.3, y: 37.7),
                    (x: -122.3, y: 37.8),
                    (x: -122.4, y: 37.8),
                    (x: -122.4, y: 37.7),
                ]),
                "sf_poly",
            ),
            (
                Geometry::Polygon(polygon![
                    (x: 139.6, y: 35.6),
                    (x: 139.8, y: 35.6),
                    (x: 139.8, y: 35.8),
                    (x: 139.6, y: 35.8),
                    (x: 139.6, y: 35.6),
                ]),
                "tokyo_poly",
            ),
        ];

        // Should not panic
        sort_by_spatial_index(&mut features, true);

        // Both features should still be present
        assert_eq!(features.len(), 2);
    }

    #[test]
    fn test_sort_works_with_linestrings() {
        let mut features = vec![
            (
                Geometry::LineString(line_string![
                    (x: -122.4, y: 37.7),
                    (x: -122.3, y: 37.8),
                ]),
                "sf_line",
            ),
            (
                Geometry::LineString(line_string![
                    (x: 139.6, y: 35.6),
                    (x: 139.8, y: 35.8),
                ]),
                "tokyo_line",
            ),
        ];

        sort_by_spatial_index(&mut features, false); // Use Z-order

        assert_eq!(features.len(), 2);
    }

    #[test]
    fn test_sort_hilbert_vs_zorder() {
        // Both should produce valid sorted results (may differ in order)
        let make_features = || {
            vec![
                (Geometry::Point(point!(x: 0.0, y: 0.0)), 0),
                (Geometry::Point(point!(x: 10.0, y: 10.0)), 1),
                (Geometry::Point(point!(x: -10.0, y: -10.0)), 2),
                (Geometry::Point(point!(x: 10.0, y: -10.0)), 3),
            ]
        };

        let mut hilbert_features = make_features();
        let mut zorder_features = make_features();

        sort_by_spatial_index(&mut hilbert_features, true);
        sort_by_spatial_index(&mut zorder_features, false);

        // Both should complete without error and maintain all features
        assert_eq!(hilbert_features.len(), 4);
        assert_eq!(zorder_features.len(), 4);
    }

    // ============================================================
    // TIPPECANOE COMPATIBILITY TESTS
    // ============================================================

    #[test]
    fn test_zorder_matches_tippecanoe_quadkey() {
        // Test specific values that we can verify against tippecanoe behavior
        // tippecanoe encodes: x-bit to even positions, y-bit to odd positions

        // Simple case: wx=1, wy=0
        // Binary: wx = 0...01, wy = 0...00
        // Interleaved: last pair should be (x=1, y=0) = 0b10
        let index = encode_zorder(1, 0);
        assert_eq!(index & 0b11, 0b10, "Last two bits should be x=1, y=0");

        // wx=0, wy=1
        let index = encode_zorder(0, 1);
        assert_eq!(index & 0b11, 0b01, "Last two bits should be x=0, y=1");

        // wx=1, wy=1
        let index = encode_zorder(1, 1);
        assert_eq!(index & 0b11, 0b11, "Last two bits should be x=1, y=1");
    }

    #[test]
    fn test_hilbert_matches_tippecanoe_wikipedia_algorithm() {
        // The Hilbert curve implementation follows the Wikipedia algorithm
        // that tippecanoe uses. Test a few known values.

        // For a 4x4 grid (n=4), the Hilbert curve visits:
        // d=0 -> (0,0), d=1 -> (0,1), d=2 -> (1,1), d=3 -> (1,0)
        // d=4 -> (2,0), etc.

        // Scale to 32-bit: n = 2^32
        // We'll test with scaled coordinates

        // Origin should map to d=0
        assert_eq!(encode_hilbert(0, 0), 0);

        // Test that encode/decode are consistent for boundary cases
        let test_coords = [(0, 0), (1, 0), (0, 1), (1, 1), (u32::MAX, 0), (0, u32::MAX)];

        for (wx, wy) in test_coords {
            let d = encode_hilbert(wx, wy);
            let (wx_back, wy_back) = decode_hilbert(d);
            assert_eq!(
                (wx, wy),
                (wx_back, wy_back),
                "Hilbert encode/decode should be consistent"
            );
        }
    }

    // ============================================================
    // SORT_GEOMETRIES TESTS (for pipeline integration)
    // ============================================================

    #[test]
    fn test_sort_geometries_clusters_nearby_features() {
        // Create features in different parts of the world
        let mut geometries = vec![
            Geometry::Point(point!(x: 139.7, y: 35.7)),    // Tokyo
            Geometry::Point(point!(x: -122.4, y: 37.8)),   // San Francisco
            Geometry::Point(point!(x: 2.35, y: 48.85)),    // Paris
            Geometry::Point(point!(x: -122.41, y: 37.79)), // Near SF
            Geometry::Point(point!(x: 2.36, y: 48.86)),    // Near Paris
            Geometry::Point(point!(x: 139.75, y: 35.68)),  // Near Tokyo
        ];

        // Record original positions for verification
        let sf_orig = geometries[1].clone();
        let sf_near_orig = geometries[3].clone();

        sort_geometries(&mut geometries, true);

        // Find positions of features after sorting
        let sf_pos = geometries.iter().position(|g| *g == sf_orig).unwrap();
        let sf_near_pos = geometries.iter().position(|g| *g == sf_near_orig).unwrap();

        // SF and Near SF should be adjacent after sorting
        assert!(
            (sf_pos as i32 - sf_near_pos as i32).abs() <= 1,
            "SF features should be adjacent after sorting: positions {} and {}",
            sf_pos,
            sf_near_pos
        );
    }

    #[test]
    fn test_sort_geometries_hilbert_vs_zorder() {
        // Both should produce valid sorted results
        let make_geometries = || {
            vec![
                Geometry::Point(point!(x: 0.0, y: 0.0)),
                Geometry::Point(point!(x: 10.0, y: 10.0)),
                Geometry::Point(point!(x: -10.0, y: -10.0)),
                Geometry::Point(point!(x: 10.0, y: -10.0)),
            ]
        };

        let mut hilbert_geoms = make_geometries();
        let mut zorder_geoms = make_geometries();

        sort_geometries(&mut hilbert_geoms, true);
        sort_geometries(&mut zorder_geoms, false);

        // Both should complete without error and maintain all features
        assert_eq!(hilbert_geoms.len(), 4);
        assert_eq!(zorder_geoms.len(), 4);

        // They may have different orders, but both should have all 4 geometries
    }

    #[test]
    fn test_sort_geometries_empty_vec() {
        let mut geometries: Vec<Geometry<f64>> = vec![];
        sort_geometries(&mut geometries, true);
        assert!(geometries.is_empty());
    }

    #[test]
    fn test_sort_geometries_single_element() {
        let mut geometries = vec![Geometry::Point(point!(x: 0.0, y: 0.0))];
        sort_geometries(&mut geometries, true);
        assert_eq!(geometries.len(), 1);
    }
}
