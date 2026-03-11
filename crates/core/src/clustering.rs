//! Point clustering for feature aggregation at lower zoom levels.
//!
//! This module implements tippecanoe-compatible point clustering, which groups
//! nearby points into clusters and averages their positions.
//!
//! # Tippecanoe Compatibility
//!
//! This is a 1:1 port of tippecanoe's point clustering algorithm. Key behaviors:
//! - Cluster distance is specified in 256-pixel tile units
//! - Clustering uses Hilbert index proximity (NOT Euclidean distance)
//! - Centroid calculation uses incremental averaging (Welford's algorithm)
//! - Only points cluster with points (no cross-geometry-type clustering)
//! - Clustering is disabled above cluster_maxzoom
//!
//! # Tippecanoe Reference
//!
//! From tippecanoe's `mvt.cpp`:
//! ```c
//! scale = (1LL << (32 - z)) / 256;
//! unsigned long long cluster_mingap = scale * cluster_distance;
//! cluster_mingap = cluster_mingap * cluster_mingap;
//! ```
//!
//! The clustering criteria (from `write_tile()`):
//! 1. Index proximity: `feature.index - cluster.index < cluster_mingap`
//! 2. Same geometry type: Points cluster with points only
//! 3. Same layer: Features must belong to same layer
//!
//! # Example
//!
//! ```
//! use gpq_tiles_core::clustering::{PointClusterer, ClusterConfig};
//! use gpq_tiles_core::accumulator::{AccumulatorConfig, AccumulatorOp};
//!
//! // Configure clustering with 50px distance, max zoom 12
//! let config = ClusterConfig::new(50, 12);
//!
//! // Optionally configure attribute accumulation
//! let mut accumulators = AccumulatorConfig::new();
//! accumulators.set_operation("count", AccumulatorOp::Sum);
//!
//! let clusterer = PointClusterer::new(config, Some(accumulators));
//! ```

use std::collections::HashMap;

use geo::Point;

use crate::accumulator::AccumulatorConfig;
use crate::spatial_index::{encode_hilbert, lng_lat_to_world_coords};
use crate::wkb::PropertyValue;

/// Configuration for point clustering.
///
/// Matches tippecanoe's `--cluster-distance` and `--cluster-maxzoom` options.
#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// Cluster distance in 256-pixel tile units.
    ///
    /// Points within this distance (in Hilbert index space) will be clustered.
    /// Typical values: 50 (default in tippecanoe), 25 (less aggressive), 100 (more aggressive).
    pub distance: u32,

    /// Maximum zoom level for clustering.
    ///
    /// At zoom levels above this, no clustering is performed and all points are kept.
    /// Typically set to max_zoom - 2 or so.
    pub max_zoom: u8,
}

impl ClusterConfig {
    /// Create a new cluster configuration.
    ///
    /// # Arguments
    ///
    /// * `distance` - Cluster distance in 256-pixel tile units (tippecanoe default: 50)
    /// * `max_zoom` - Maximum zoom level for clustering
    pub fn new(distance: u32, max_zoom: u8) -> Self {
        Self { distance, max_zoom }
    }

    /// Calculate the cluster gap threshold for a given zoom level.
    ///
    /// # Tippecanoe Reference
    ///
    /// From `mvt.cpp`:
    /// ```c
    /// scale = (1LL << (32 - z)) / 256;
    /// unsigned long long cluster_mingap = scale * cluster_distance;
    /// cluster_mingap = cluster_mingap * cluster_mingap;
    /// ```
    pub fn cluster_gap(&self, zoom: u8) -> u64 {
        // Scale factor: how many world coordinate units per 256-pixel tile
        let scale = (1u64 << (32 - zoom as u32)) / 256;
        // Gap is (scale * distance)^2 for comparison with squared Hilbert index difference
        // Use saturating multiplication to prevent overflow at low zoom levels
        let gap = scale.saturating_mul(self.distance as u64);
        gap.saturating_mul(gap)
    }
}

/// A cluster of points with accumulated properties.
///
/// Represents the result of clustering multiple points together.
#[derive(Debug, Clone)]
pub struct PointCluster {
    /// World coordinates of the cluster centroid (for Hilbert index calculation)
    pub world_x: u64,
    pub world_y: u64,

    /// Geographic centroid (longitude, latitude)
    pub centroid: (f64, f64),

    /// Number of points in this cluster
    pub count: u64,

    /// Hilbert index of the cluster (for proximity checking)
    pub hilbert_index: u64,

    /// Accumulated properties from all clustered points
    pub properties: HashMap<String, PropertyValue>,
}

impl PointCluster {
    /// Create a new cluster from a single point.
    fn from_point(
        point: &Point<f64>,
        hilbert_index: u64,
        world_x: u32,
        world_y: u32,
        properties: HashMap<String, PropertyValue>,
    ) -> Self {
        Self {
            world_x: world_x as u64,
            world_y: world_y as u64,
            centroid: (point.x(), point.y()),
            count: 1,
            hilbert_index,
            properties,
        }
    }

    /// Merge another point into this cluster using incremental centroid calculation.
    ///
    /// # Tippecanoe Reference
    ///
    /// Uses Welford's incremental mean algorithm:
    /// ```text
    /// new_mean = old_mean + (new_value - old_mean) / n
    /// ```
    ///
    /// This is numerically stable and matches tippecanoe's approach.
    fn merge_point(&mut self, point: &Point<f64>, world_x: u32, world_y: u32) {
        self.count += 1;
        let n = self.count as f64;

        // Incremental centroid update (Welford's algorithm)
        self.centroid.0 += (point.x() - self.centroid.0) / n;
        self.centroid.1 += (point.y() - self.centroid.1) / n;

        // Update world coordinates using signed arithmetic to avoid overflow
        // World coordinates are u32 but we use i64 for safe subtraction
        let wx_diff = world_x as i64 - self.world_x as i64;
        let wy_diff = world_y as i64 - self.world_y as i64;
        self.world_x = (self.world_x as i64 + wx_diff / self.count as i64) as u64;
        self.world_y = (self.world_y as i64 + wy_diff / self.count as i64) as u64;
    }
}

/// A point feature with its spatial index and properties.
#[derive(Debug, Clone)]
pub struct IndexedPoint {
    /// The point geometry
    pub point: Point<f64>,

    /// Hilbert curve index for spatial sorting/proximity
    pub hilbert_index: u64,

    /// World coordinates (for centroid calculation)
    pub world_x: u32,
    pub world_y: u32,

    /// Feature properties
    pub properties: HashMap<String, PropertyValue>,
}

impl IndexedPoint {
    /// Create a new indexed point from a geo::Point.
    pub fn new(point: Point<f64>, properties: HashMap<String, PropertyValue>) -> Self {
        let (world_x, world_y) = lng_lat_to_world_coords(point.x(), point.y());
        let hilbert_index = encode_hilbert(world_x, world_y);

        Self {
            point,
            hilbert_index,
            world_x,
            world_y,
            properties,
        }
    }
}

/// Point clusterer that groups nearby points and averages their positions.
///
/// This implements tippecanoe's point clustering algorithm, which:
/// 1. Sorts points by Hilbert index
/// 2. Iterates through points, merging nearby ones into clusters
/// 3. Uses incremental centroid calculation for position averaging
/// 4. Applies accumulator operations for property aggregation
pub struct PointClusterer {
    config: ClusterConfig,
    accumulators: Option<AccumulatorConfig>,
}

impl PointClusterer {
    /// Create a new point clusterer.
    ///
    /// # Arguments
    ///
    /// * `config` - Clustering configuration (distance, max_zoom)
    /// * `accumulators` - Optional accumulator configuration for property aggregation
    pub fn new(config: ClusterConfig, accumulators: Option<AccumulatorConfig>) -> Self {
        Self {
            config,
            accumulators,
        }
    }

    /// Cluster points at the given zoom level.
    ///
    /// # Arguments
    ///
    /// * `points` - Points to cluster (must be sorted by Hilbert index)
    /// * `zoom` - Current zoom level
    ///
    /// # Returns
    ///
    /// Vector of clustered points. Each cluster is represented as a single point
    /// at the cluster centroid with accumulated properties.
    ///
    /// # Tippecanoe Behavior
    ///
    /// - Points are clustered if their Hilbert indices differ by less than `cluster_gap`
    /// - Clustering is disabled above `cluster_maxzoom`
    /// - The first point in a cluster determines the initial properties
    /// - Subsequent points' properties are accumulated according to the accumulator config
    pub fn cluster(&self, mut points: Vec<IndexedPoint>, zoom: u8) -> Vec<IndexedPoint> {
        // Clustering disabled above max_zoom
        if zoom > self.config.max_zoom {
            return points;
        }

        // Empty or single point - no clustering needed
        if points.len() <= 1 {
            return points;
        }

        // Sort by Hilbert index (tippecanoe requires sorted input)
        points.sort_by_key(|p| p.hilbert_index);

        let cluster_gap = self.config.cluster_gap(zoom);
        let mut clusters: Vec<PointCluster> = Vec::new();

        // Clone accumulator config since accumulate() needs &mut self for mean tracking
        let mut accumulators = self.accumulators.clone();

        for point in points {
            // Check if this point can be merged into the last cluster
            let should_merge = if let Some(last_cluster) = clusters.last() {
                // Tippecanoe uses: feature.index - cluster.index < cluster_mingap
                // Since points are sorted, we only need to check against the last cluster
                point
                    .hilbert_index
                    .saturating_sub(last_cluster.hilbert_index)
                    < cluster_gap
            } else {
                false
            };

            if should_merge {
                // Merge into existing cluster
                let cluster = clusters.last_mut().unwrap();
                cluster.merge_point(&point.point, point.world_x, point.world_y);

                // Accumulate properties if configured
                if let Some(ref mut acc) = accumulators {
                    acc.accumulate(&mut cluster.properties, &point.properties);
                }
            } else {
                // Start a new cluster
                clusters.push(PointCluster::from_point(
                    &point.point,
                    point.hilbert_index,
                    point.world_x,
                    point.world_y,
                    point.properties,
                ));
            }
        }

        // Convert clusters back to IndexedPoints at their centroids
        clusters
            .into_iter()
            .map(|c| {
                let centroid_point = Point::new(c.centroid.0, c.centroid.1);
                let (world_x, world_y) = lng_lat_to_world_coords(c.centroid.0, c.centroid.1);
                let hilbert_index = encode_hilbert(world_x, world_y);

                // Add cluster count to properties if not already present
                let mut props = c.properties;
                if c.count > 1 {
                    props.insert("cluster_count".to_string(), PropertyValue::UInt(c.count));
                }

                IndexedPoint {
                    point: centroid_point,
                    hilbert_index,
                    world_x,
                    world_y,
                    properties: props,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accumulator::AccumulatorOp;
    use geo::point;

    // ============================================================
    // CLUSTER CONFIG TESTS
    // ============================================================

    #[test]
    fn test_cluster_gap_calculation() {
        // Test that cluster gap matches tippecanoe's formula
        // scale = (1 << (32 - z)) / 256
        // gap = (scale * distance)^2
        let config = ClusterConfig::new(50, 14);

        // At zoom 0: scale = (1 << 32) / 256 = 16777216
        // gap = (16777216 * 50)^2 = enormous
        let gap_z0 = config.cluster_gap(0);
        assert!(gap_z0 > 0);

        // At zoom 14: scale = (1 << 18) / 256 = 1024
        // gap = (1024 * 50)^2 = 2621440000
        let gap_z14 = config.cluster_gap(14);
        let expected_z14 = (1024u64 * 50).pow(2);
        assert_eq!(gap_z14, expected_z14);

        // Gap should decrease with zoom (more precision = smaller gap)
        assert!(gap_z0 > gap_z14);
    }

    // ============================================================
    // TWO POINT CLUSTERING TESTS
    // ============================================================

    #[test]
    fn test_two_points_within_threshold_cluster_to_correct_centroid() {
        // RED: Two nearby points should cluster to their average position
        let config = ClusterConfig::new(50, 14);
        let clusterer = PointClusterer::new(config, None);

        // Two points very close together (same approximate location)
        let p1 = IndexedPoint::new(point!(x: -122.4, y: 37.8), HashMap::new());
        let p2 = IndexedPoint::new(point!(x: -122.401, y: 37.801), HashMap::new());

        let points = vec![p1, p2];
        let result = clusterer.cluster(points, 10);

        // Should cluster into 1 point
        assert_eq!(result.len(), 1);

        // Centroid should be approximately the average
        let centroid = &result[0];
        assert!((centroid.point.x() - (-122.4005)).abs() < 0.001);
        assert!((centroid.point.y() - 37.8005).abs() < 0.001);

        // Should have cluster_count property
        assert_eq!(
            centroid.properties.get("cluster_count"),
            Some(&PropertyValue::UInt(2))
        );
    }

    // ============================================================
    // THREE POINT INCREMENTAL CENTROID TESTS
    // ============================================================

    #[test]
    fn test_three_points_incremental_centroid() {
        // RED: Verify INCREMENTAL centroid calculation (not batch average)
        // This tests Welford's algorithm specifically
        // Use very large distance and zoom 0 to force all points to cluster
        let config = ClusterConfig::new(10000, 14);
        let clusterer = PointClusterer::new(config, None);

        // Three points very close together (nearly identical location)
        let p1 = IndexedPoint::new(point!(x: 0.0, y: 0.0), HashMap::new());
        let p2 = IndexedPoint::new(point!(x: 0.00001, y: 0.00001), HashMap::new());
        let p3 = IndexedPoint::new(point!(x: 0.00002, y: 0.00002), HashMap::new());

        let points = vec![p1, p2, p3];
        let result = clusterer.cluster(points, 0);

        // Should cluster into 1 point
        assert_eq!(
            result.len(),
            1,
            "All three points should cluster together at zoom 0"
        );

        // Centroid should be at approximately (0.00001, 0.00001) - the average
        let centroid = &result[0];
        let expected_x = 0.00001;
        let expected_y = 0.00001;
        assert!(
            (centroid.point.x() - expected_x).abs() < 0.00001,
            "Expected x={}, got {}",
            expected_x,
            centroid.point.x()
        );
        assert!(
            (centroid.point.y() - expected_y).abs() < 0.00001,
            "Expected y={}, got {}",
            expected_y,
            centroid.point.y()
        );

        // Count should be 3
        assert_eq!(
            centroid.properties.get("cluster_count"),
            Some(&PropertyValue::UInt(3))
        );
    }

    // ============================================================
    // POINTS BEYOND THRESHOLD DON'T CLUSTER
    // ============================================================

    #[test]
    fn test_points_beyond_threshold_dont_cluster() {
        // RED: Points far apart should NOT cluster
        let config = ClusterConfig::new(1, 14); // Very small distance
        let clusterer = PointClusterer::new(config, None);

        // Two points far apart (different continents)
        let p1 = IndexedPoint::new(point!(x: -122.4, y: 37.8), HashMap::new()); // San Francisco
        let p2 = IndexedPoint::new(point!(x: 139.7, y: 35.7), HashMap::new()); // Tokyo

        let points = vec![p1, p2];
        let result = clusterer.cluster(points, 10);

        // Should NOT cluster - 2 separate points
        assert_eq!(result.len(), 2);

        // Neither should have cluster_count property
        assert!(!result[0].properties.contains_key("cluster_count"));
        assert!(!result[1].properties.contains_key("cluster_count"));
    }

    // ============================================================
    // CLUSTERING DISABLED ABOVE MAX ZOOM
    // ============================================================

    #[test]
    fn test_clustering_disabled_above_max_zoom() {
        // RED: Clustering should be disabled above max_zoom
        let config = ClusterConfig::new(100, 10); // max_zoom = 10
        let clusterer = PointClusterer::new(config, None);

        // Two nearby points that WOULD cluster at zoom 10
        let p1 = IndexedPoint::new(point!(x: -122.4, y: 37.8), HashMap::new());
        let p2 = IndexedPoint::new(point!(x: -122.401, y: 37.801), HashMap::new());

        let points = vec![p1.clone(), p2.clone()];

        // At zoom 10 (= max_zoom), should cluster
        let result_z10 = clusterer.cluster(points.clone(), 10);
        assert_eq!(result_z10.len(), 1, "Should cluster at max_zoom");

        // At zoom 11 (> max_zoom), should NOT cluster
        let result_z11 = clusterer.cluster(points.clone(), 11);
        assert_eq!(result_z11.len(), 2, "Should NOT cluster above max_zoom");

        // At zoom 14 (well above max_zoom), should NOT cluster
        let result_z14 = clusterer.cluster(points, 14);
        assert_eq!(result_z14.len(), 2, "Should NOT cluster above max_zoom");
    }

    // ============================================================
    // ATTRIBUTE ACCUMULATION TESTS
    // ============================================================

    #[test]
    fn test_attributes_accumulate_via_config() {
        // RED: Attributes should accumulate according to AccumulatorConfig
        // Use a very large cluster distance to ensure clustering at zoom 0
        let config = ClusterConfig::new(1000, 14);

        let mut accumulators = AccumulatorConfig::new();
        accumulators.set_operation("population", AccumulatorOp::Sum);
        accumulators.set_operation("names", AccumulatorOp::Comma);

        let clusterer = PointClusterer::new(config, Some(accumulators));

        // Two points with properties - very close together
        let mut props1 = HashMap::new();
        props1.insert("population".to_string(), PropertyValue::Int(100));
        props1.insert(
            "names".to_string(),
            PropertyValue::String("Alice".to_string()),
        );

        let mut props2 = HashMap::new();
        props2.insert("population".to_string(), PropertyValue::Int(200));
        props2.insert(
            "names".to_string(),
            PropertyValue::String("Bob".to_string()),
        );

        let p1 = IndexedPoint::new(point!(x: 0.0, y: 0.0), props1);
        let p2 = IndexedPoint::new(point!(x: 0.00001, y: 0.00001), props2);

        // Use zoom 0 for maximum clustering range
        let result = clusterer.cluster(vec![p1, p2], 0);

        assert_eq!(
            result.len(),
            1,
            "Points should cluster at zoom 0 with large distance"
        );
        let cluster = &result[0];

        // Population should be summed: 100 + 200 = 300
        // Note: The accumulator preserves integer type when summing integers
        match cluster.properties.get("population") {
            Some(PropertyValue::Int(val)) => assert_eq!(*val, 300),
            Some(PropertyValue::Float(val)) => assert!((val - 300.0).abs() < 0.001),
            other => panic!("Expected Int(300) or Float(300.0), got {:?}", other),
        }

        // Names should be comma-separated
        assert_eq!(
            cluster.properties.get("names"),
            Some(&PropertyValue::String("Alice,Bob".to_string()))
        );
    }

    // ============================================================
    // EMPTY AND SINGLE POINT EDGE CASES
    // ============================================================

    #[test]
    fn test_empty_points_returns_empty() {
        let config = ClusterConfig::new(50, 14);
        let clusterer = PointClusterer::new(config, None);

        let result = clusterer.cluster(vec![], 10);
        assert!(result.is_empty());
    }

    #[test]
    fn test_single_point_returns_unchanged() {
        let config = ClusterConfig::new(50, 14);
        let clusterer = PointClusterer::new(config, None);

        let p = IndexedPoint::new(point!(x: -122.4, y: 37.8), HashMap::new());
        let result = clusterer.cluster(vec![p], 10);

        assert_eq!(result.len(), 1);
        // Single point should NOT have cluster_count
        assert!(!result[0].properties.contains_key("cluster_count"));
    }

    // ============================================================
    // MIXED CLUSTER SIZES
    // ============================================================

    #[test]
    fn test_multiple_clusters_form_correctly() {
        // RED: Multiple separate clusters should form
        let config = ClusterConfig::new(50, 14);
        let clusterer = PointClusterer::new(config, None);

        // San Francisco cluster (2 points)
        let sf1 = IndexedPoint::new(point!(x: -122.4, y: 37.8), HashMap::new());
        let sf2 = IndexedPoint::new(point!(x: -122.401, y: 37.801), HashMap::new());

        // Tokyo cluster (2 points)
        let tokyo1 = IndexedPoint::new(point!(x: 139.7, y: 35.7), HashMap::new());
        let tokyo2 = IndexedPoint::new(point!(x: 139.701, y: 35.701), HashMap::new());

        let points = vec![sf1, sf2, tokyo1, tokyo2];
        let result = clusterer.cluster(points, 10);

        // Should form 2 clusters
        assert_eq!(result.len(), 2);

        // Both should have cluster_count = 2
        assert!(result
            .iter()
            .all(|c| c.properties.get("cluster_count") == Some(&PropertyValue::UInt(2))));
    }
}
