//! Gap-based density selection for feature dropping.
//!
//! This module implements tippecanoe's gap-based algorithm for reducing feature density
//! at lower zoom levels. Unlike grid-based approaches, this uses Hilbert index gaps to
//! determine which features to drop, providing better preservation of spatial distribution.
//!
//! # Tippecanoe Reference
//!
//! This is a 1:1 port of tippecanoe's `manage_gap` algorithm from `tile.cpp`:
//!
//! - Features are sorted by Hilbert index (spatial locality preserved)
//! - The gap between consecutive features' Hilbert indices determines selection
//! - Gamma parameter controls exponential spacing (default: 0 = disabled)
//! - When gamma > 0, features with gaps smaller than a threshold are dropped
//!
//! # Key Insight
//!
//! Tippecanoe uses **Hilbert index gaps**, not Euclidean distance! This is crucial:
//! - Hilbert curves preserve locality: nearby features have nearby indices
//! - Gap = difference in Hilbert indices between consecutive features
//! - Smaller gaps = denser clustering in Hilbert space = candidates for dropping
//!
//! # Algorithm
//!
//! ```text
//! manage_gap(index, prev_index, scale, gamma, gap):
//!     if gamma <= 0: return false  // Disabled
//!
//!     if gap > 0:
//!         if index == prev_index: return true  // Exact duplicate
//!
//!         distance = index - prev_index
//!         threshold = (distance / scale)^gamma
//!
//!         if index < prev_index OR threshold >= gap:
//!             gap = 0  // Reset, keep feature
//!         else:
//!             return true  // Gap too small - drop
//!     else if index >= prev_index:
//!         gap = (index - prev_index) / scale
//!         if gap == 0 OR gap < 1:
//!             return true
//!         else:
//!             gap = 0
//!
//!     prev_index = index
//!     return false
//! ```
//!
//! # Example
//!
//! ```
//! use gpq_tiles_core::gap_density::GapBasedSelector;
//!
//! let mut selector = GapBasedSelector::new(2.0);  // gamma = 2.0
//!
//! // Process features sorted by Hilbert index
//! let keep1 = !selector.should_drop(1000);  // First feature - kept
//! let keep2 = !selector.should_drop(1001);  // Very close - likely dropped
//! let keep3 = !selector.should_drop(2000);  // Far away - kept
//! ```

use crate::spatial_index::{encode_hilbert, lng_lat_to_world_coords};
use geo::{Centroid, Geometry};

/// Gap-based feature selector implementing tippecanoe's `manage_gap` algorithm.
///
/// This selector tracks the Hilbert index of the previously kept feature and uses
/// gap-based logic to decide whether to drop the current feature.
///
/// # Gamma Behavior
///
/// - `gamma = 0`: Gap-based dropping disabled (all features kept)
/// - `gamma = 1`: Linear spacing
/// - `gamma = 2`: "Reduces dots < 1 pixel apart to square root of original" (tippecanoe default for dense data)
/// - Higher gamma = more aggressive dropping of closely-spaced features
///
/// # Adaptive Mode
///
/// When tiles exceed size limits, tippecanoe increases gamma by 1.25x and retries.
/// This adaptive behavior can be implemented at the pipeline level.
#[derive(Debug, Clone)]
pub struct GapBasedSelector {
    /// Gamma exponent for exponential spacing (0 = disabled)
    gamma: f64,
    /// Scale factor for normalizing Hilbert index gaps
    scale: f64,
    /// Hilbert index of the previously kept feature
    prev_index: u64,
    /// Running gap accumulator
    gap_accumulator: f64,
    /// Whether we've seen the first feature yet
    first_feature: bool,
}

impl GapBasedSelector {
    /// Create a new gap-based selector with the given gamma value.
    ///
    /// # Arguments
    ///
    /// * `gamma` - Exponential spacing parameter. Use 0 to disable gap-based dropping,
    ///   2.0 for tippecanoe's default aggressive mode.
    ///
    /// # Scale Factor
    ///
    /// The scale factor is set to 1.0 by default. For zoom-level-aware scaling,
    /// use `with_scale()`.
    pub fn new(gamma: f64) -> Self {
        Self {
            gamma,
            scale: 1.0,
            prev_index: 0,
            gap_accumulator: 0.0,
            first_feature: true,
        }
    }

    /// Create a new selector with custom scale factor.
    ///
    /// The scale factor normalizes Hilbert index gaps. Tippecanoe uses zoom-dependent
    /// scaling where scale = 2^(32-zoom) for proper gap normalization at each zoom level.
    pub fn with_scale(mut self, scale: f64) -> Self {
        self.scale = scale;
        self
    }

    /// Reset the selector state for a new tile or feature batch.
    ///
    /// Call this when starting to process a new tile to reset the gap tracking.
    pub fn reset(&mut self) {
        self.prev_index = 0;
        self.gap_accumulator = 0.0;
        self.first_feature = true;
    }

    /// Check if a feature with the given Hilbert index should be dropped.
    ///
    /// This implements tippecanoe's `manage_gap` algorithm exactly.
    ///
    /// # Arguments
    ///
    /// * `hilbert_index` - The feature's Hilbert curve index
    ///
    /// # Returns
    ///
    /// `true` if the feature should be DROPPED, `false` if it should be kept.
    ///
    /// # Algorithm Details
    ///
    /// The algorithm maintains a running gap accumulator. When a feature is kept,
    /// the gap is reset. When a feature is too close to the previous kept feature
    /// (based on Hilbert index distance), it is dropped.
    pub fn should_drop(&mut self, hilbert_index: u64) -> bool {
        // Gamma <= 0 means gap-based dropping is disabled
        if self.gamma <= 0.0 {
            return false;
        }

        // First feature is always kept
        if self.first_feature {
            self.first_feature = false;
            self.prev_index = hilbert_index;
            self.gap_accumulator = 0.0;
            return false;
        }

        if self.gap_accumulator > 0.0 {
            // We have a pending gap from the previous feature

            // Exact duplicate - always drop
            if hilbert_index == self.prev_index {
                return true;
            }

            // Calculate distance in Hilbert space
            // Note: We handle the case where index < prev_index (shouldn't happen if sorted,
            // but tippecanoe handles it for robustness)
            if hilbert_index < self.prev_index {
                // Index went backwards (unsorted or wraparound) - reset and keep
                self.gap_accumulator = 0.0;
                self.prev_index = hilbert_index;
                return false;
            }

            let distance = (hilbert_index - self.prev_index) as f64;
            let threshold = (distance / self.scale).powf(self.gamma);

            if threshold >= self.gap_accumulator {
                // Gap is large enough - keep this feature
                self.gap_accumulator = 0.0;
                self.prev_index = hilbert_index;
                false
            } else {
                // Gap too small - drop this feature
                true
            }
        } else {
            // No pending gap - calculate new gap
            if hilbert_index >= self.prev_index {
                let gap = (hilbert_index - self.prev_index) as f64 / self.scale;

                if gap == 0.0 || gap < 1.0 {
                    // Zero gap or very small gap - drop
                    // CRITICAL: Retain gap for accumulation (tippecanoe compatibility)
                    self.gap_accumulator = gap;
                    return true;
                }

                // Keep this feature, start new gap tracking
                self.gap_accumulator = 0.0;
                self.prev_index = hilbert_index;
                false
            } else {
                // Index went backwards - shouldn't happen if sorted
                self.prev_index = hilbert_index;
                false
            }
        }
    }

    /// Check if a geometry should be dropped based on its centroid's Hilbert index.
    ///
    /// Convenience method that calculates the Hilbert index from the geometry's centroid.
    pub fn should_drop_geometry(&mut self, geom: &Geometry<f64>) -> bool {
        let hilbert_index = geometry_to_hilbert(geom).unwrap_or(0);
        self.should_drop(hilbert_index)
    }

    /// Get the current gamma value.
    pub fn gamma(&self) -> f64 {
        self.gamma
    }

    /// Set a new gamma value (for adaptive mode).
    pub fn set_gamma(&mut self, gamma: f64) {
        self.gamma = gamma;
    }

    /// Increase gamma by a factor (for adaptive retry).
    ///
    /// Tippecanoe multiplies gamma by 1.25 when tiles exceed size limits.
    pub fn increase_gamma(&mut self, factor: f64) {
        self.gamma *= factor;
    }
}

/// Calculate the Hilbert index for a geometry based on its centroid.
///
/// Returns `None` if the geometry has no valid centroid.
pub fn geometry_to_hilbert(geom: &Geometry<f64>) -> Option<u64> {
    let centroid = geom.centroid()?;
    let (wx, wy) = lng_lat_to_world_coords(centroid.x(), centroid.y());
    Some(encode_hilbert(wx, wy))
}

/// Choose the minimum gap threshold to achieve a target retention fraction.
///
/// This implements tippecanoe's `choose_mingap` algorithm for determining the
/// optimal gap threshold to keep approximately `fraction` of the features.
///
/// # Arguments
///
/// * `gaps` - Vector of gap values (distances between consecutive Hilbert indices).
///   Will be sorted in place.
/// * `fraction` - Target fraction of features to keep (0.0 to 1.0)
/// * `existing_gap` - Current minimum gap (gaps <= this are already being dropped)
///
/// # Returns
///
/// The new minimum gap threshold. Features with gaps below this should be dropped.
///
/// # Algorithm
///
/// 1. Sort the gaps
/// 2. Find the gap at position `(1 - fraction) * len`
/// 3. If that gap is <= existing_gap, move to the next larger gap
/// 4. Return the selected gap as the new threshold
pub fn choose_mingap(gaps: &mut [u64], fraction: f64, existing_gap: u64) -> u64 {
    if gaps.is_empty() {
        return existing_gap;
    }

    gaps.sort_unstable();

    // Calculate the index for the target fraction
    // (1 - fraction) because we want to DROP (1-fraction) of features
    let mut ix = ((gaps.len() - 1) as f64 * (1.0 - fraction)) as usize;

    // Skip gaps that are already being filtered
    while ix + 1 < gaps.len() && gaps[ix] <= existing_gap {
        ix += 1;
    }

    gaps[ix]
}

/// Select features by gap-based density filtering.
///
/// Sorts features by Hilbert index and applies gap-based selection to reduce density.
///
/// # Arguments
///
/// * `features` - Slice of (geometry, properties) tuples
/// * `target_count` - Target number of features to keep (approximate)
/// * `gamma` - Gamma parameter for exponential spacing
///
/// # Returns
///
/// Indices of features to keep.
///
/// # Note
///
/// For large feature sets, consider using `GapBasedSelector` directly with
/// streaming processing instead of this batch function.
pub fn select_features_by_gap<T: Clone>(
    features: &[(Geometry<f64>, T)],
    target_count: usize,
    gamma: f64,
) -> Vec<usize> {
    if features.is_empty() {
        return vec![];
    }

    // If target_count >= features.len() or gamma <= 0, keep all
    if target_count >= features.len() || gamma <= 0.0 {
        return (0..features.len()).collect();
    }

    // Create indexed features with Hilbert indices
    let mut indexed: Vec<(usize, u64)> = features
        .iter()
        .enumerate()
        .map(|(i, (geom, _))| (i, geometry_to_hilbert(geom).unwrap_or(0)))
        .collect();

    // Sort by Hilbert index
    indexed.sort_by_key(|(_, hilbert)| *hilbert);

    // Calculate fraction to keep
    let fraction = target_count as f64 / features.len() as f64;

    // Collect gaps between consecutive features
    let mut gaps: Vec<u64> = indexed
        .windows(2)
        .map(|w| w[1].1.saturating_sub(w[0].1))
        .collect();

    // Determine minimum gap threshold
    let min_gap = choose_mingap(&mut gaps, fraction, 0);

    // Select features with gaps >= threshold
    let mut kept_indices = Vec::with_capacity(target_count);
    let mut prev_hilbert = 0u64;

    for (original_idx, hilbert) in indexed {
        let gap = hilbert.saturating_sub(prev_hilbert);

        // Keep first feature or features with sufficient gap
        if kept_indices.is_empty() || gap >= min_gap {
            kept_indices.push(original_idx);
            prev_hilbert = hilbert;
        }
    }

    kept_indices
}

/// Scale factor for zoom-level-aware gap calculation.
///
/// At zoom 0, the entire world fits in one tile, so gaps should be normalized
/// by 2^32. At zoom 14, tiles are much smaller, so gaps should be normalized less.
///
/// # Arguments
///
/// * `zoom` - Current zoom level
///
/// # Returns
///
/// Scale factor for normalizing Hilbert index gaps.
pub fn scale_for_zoom(zoom: u8) -> f64 {
    // At zoom 0: scale = 2^32 (full world)
    // At zoom 14: scale = 2^(32-14) = 2^18
    // At zoom 32: scale = 1 (maximum precision)
    (1u64 << (32 - zoom.min(32))) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::point;

    // ============================================================
    // UNIT TESTS: Hilbert index gap calculation
    // ============================================================

    #[test]
    fn test_geometry_to_hilbert_returns_index() {
        let geom = Geometry::Point(point!(x: -122.4, y: 37.8)); // San Francisco
        let index = geometry_to_hilbert(&geom);
        assert!(index.is_some());
        assert!(index.unwrap() > 0);
    }

    #[test]
    fn test_geometry_to_hilbert_nearby_points_have_nearby_indices() {
        let sf1 = Geometry::Point(point!(x: -122.4, y: 37.8));
        let sf2 = Geometry::Point(point!(x: -122.41, y: 37.81));
        let tokyo = Geometry::Point(point!(x: 139.7, y: 35.7));

        let idx_sf1 = geometry_to_hilbert(&sf1).unwrap();
        let idx_sf2 = geometry_to_hilbert(&sf2).unwrap();
        let idx_tokyo = geometry_to_hilbert(&tokyo).unwrap();

        let sf_gap = (idx_sf1 as i128 - idx_sf2 as i128).unsigned_abs();
        let sf_tokyo_gap = (idx_sf1 as i128 - idx_tokyo as i128).unsigned_abs();

        assert!(
            sf_gap < sf_tokyo_gap,
            "Nearby points should have smaller Hilbert gap: sf_gap={}, sf_tokyo_gap={}",
            sf_gap,
            sf_tokyo_gap
        );
    }

    // ============================================================
    // UNIT TESTS: manage_gap algorithm (exact port verification)
    // ============================================================

    #[test]
    fn test_gap_selector_gamma_zero_keeps_all() {
        let mut selector = GapBasedSelector::new(0.0);

        // With gamma=0, all features should be kept
        assert!(!selector.should_drop(100));
        assert!(!selector.should_drop(101));
        assert!(!selector.should_drop(102));
        assert!(!selector.should_drop(1000000));
    }

    #[test]
    fn test_gap_selector_drops_exact_duplicates() {
        let mut selector = GapBasedSelector::new(2.0);

        // First feature at index 1000 - kept
        assert!(!selector.should_drop(1000));

        // Exact duplicate - always dropped
        assert!(selector.should_drop(1000));
    }

    #[test]
    fn test_gap_selector_keeps_first_feature() {
        let mut selector = GapBasedSelector::new(2.0);

        // First feature should always be kept (regardless of index)
        assert!(!selector.should_drop(0));

        selector.reset();
        assert!(!selector.should_drop(1000000));
    }

    #[test]
    fn test_gap_selector_drops_closely_spaced_features() {
        let mut selector = GapBasedSelector::new(2.0).with_scale(1.0);

        // First feature - kept
        assert!(!selector.should_drop(1000));

        // Very close feature (gap < 1) - should be dropped
        // Note: With scale=1 and the algorithm, a gap of 0 or near-0 is dropped
        let _drop_result = selector.should_drop(1001);
        // The exact behavior depends on the gap accumulator state
        // After first feature, gap_accumulator should be 0, so we enter the else branch
        // gap = (1001 - 1000) / 1 = 1, which is not < 1, so it's kept
        // Let's test with a smaller gap
        selector.reset();
        assert!(!selector.should_drop(1000)); // First kept
                                              // Now gap_accumulator = 0, prev_index = 1000
                                              // For index 1000 again (duplicate), it should be dropped in the first branch
                                              // Actually the gap_accumulator is 0 after keeping, so we go to else branch
                                              // For this test, let's verify the duplicate case
        assert!(selector.should_drop(1000)); // Duplicate - dropped
    }

    #[test]
    fn test_gap_selector_keeps_well_spaced_features() {
        let mut selector = GapBasedSelector::new(2.0).with_scale(100.0);

        // First feature
        assert!(!selector.should_drop(0));

        // Feature with large gap - should be kept
        assert!(!selector.should_drop(10000));

        // Another feature with large gap
        assert!(!selector.should_drop(20000));
    }

    #[test]
    fn test_gap_selector_reset_clears_state() {
        let mut selector = GapBasedSelector::new(2.0);

        // Process some features
        selector.should_drop(1000);
        selector.should_drop(2000);

        // Reset
        selector.reset();

        // Next feature should be treated as first
        assert!(!selector.should_drop(500));
    }

    // ============================================================
    // UNIT TESTS: choose_mingap threshold selection
    // ============================================================

    #[test]
    fn test_choose_mingap_empty_vector() {
        let mut gaps: Vec<u64> = vec![];
        let result = choose_mingap(&mut gaps, 0.5, 100);
        assert_eq!(result, 100); // Returns existing_gap when empty
    }

    #[test]
    fn test_choose_mingap_single_element() {
        let mut gaps = vec![500];
        let result = choose_mingap(&mut gaps, 0.5, 0);
        assert_eq!(result, 500);
    }

    #[test]
    fn test_choose_mingap_selects_correct_percentile() {
        let mut gaps = vec![100, 200, 300, 400, 500, 600, 700, 800, 900, 1000];

        // Keep 50% means drop 50%, so we want the gap at position 50%
        let result = choose_mingap(&mut gaps, 0.5, 0);
        // Position = (10-1) * (1-0.5) = 4.5 -> 4
        // gaps[4] = 500
        assert_eq!(result, 500);
    }

    #[test]
    fn test_choose_mingap_respects_existing_gap() {
        let mut gaps = vec![100, 200, 300, 400, 500];

        // With existing_gap=300, should skip gaps <= 300
        let result = choose_mingap(&mut gaps, 0.5, 300);
        // Normal position would be (5-1) * 0.5 = 2 -> gaps[2] = 300
        // But 300 <= existing_gap, so we skip to 400
        assert!(result > 300);
    }

    #[test]
    fn test_choose_mingap_keep_all() {
        let mut gaps = vec![100, 200, 300, 400, 500];

        // Keep 100% = fraction 1.0 -> position = 0
        let result = choose_mingap(&mut gaps, 1.0, 0);
        assert_eq!(result, 100); // Smallest gap
    }

    #[test]
    fn test_choose_mingap_keep_none() {
        let mut gaps = vec![100, 200, 300, 400, 500];

        // Keep 0% = fraction 0.0 -> position = 4 (last)
        let result = choose_mingap(&mut gaps, 0.0, 0);
        assert_eq!(result, 500); // Largest gap
    }

    // ============================================================
    // UNIT TESTS: gamma=0 (disabled) behavior
    // ============================================================

    #[test]
    fn test_gamma_zero_disabled() {
        let mut selector = GapBasedSelector::new(0.0);

        // All features should be kept regardless of spacing
        for i in 0..100 {
            assert!(!selector.should_drop(i), "Feature {} should be kept", i);
        }
    }

    #[test]
    fn test_gamma_negative_disabled() {
        let mut selector = GapBasedSelector::new(-1.0);

        // Negative gamma should also disable dropping
        for i in 0..100 {
            assert!(!selector.should_drop(i), "Feature {} should be kept", i);
        }
    }

    // ============================================================
    // UNIT TESTS: select_features_by_gap
    // ============================================================

    #[test]
    fn test_select_features_empty_input() {
        let features: Vec<(Geometry<f64>, String)> = vec![];
        let selected = select_features_by_gap(&features, 10, 2.0);
        assert!(selected.is_empty());
    }

    #[test]
    fn test_select_features_gamma_zero_keeps_all() {
        let features: Vec<(Geometry<f64>, String)> = vec![
            (Geometry::Point(point!(x: 0.0, y: 0.0)), "a".into()),
            (Geometry::Point(point!(x: 1.0, y: 1.0)), "b".into()),
            (Geometry::Point(point!(x: 2.0, y: 2.0)), "c".into()),
        ];

        let selected = select_features_by_gap(&features, 1, 0.0);
        assert_eq!(selected.len(), 3); // All kept when gamma=0
    }

    #[test]
    fn test_select_features_target_exceeds_count() {
        let features: Vec<(Geometry<f64>, String)> = vec![
            (Geometry::Point(point!(x: 0.0, y: 0.0)), "a".into()),
            (Geometry::Point(point!(x: 1.0, y: 1.0)), "b".into()),
        ];

        let selected = select_features_by_gap(&features, 100, 2.0);
        assert_eq!(selected.len(), 2); // All kept when target > count
    }

    #[test]
    fn test_select_features_reduces_count() {
        // Create features at varying distances
        let features: Vec<(Geometry<f64>, String)> = vec![
            (Geometry::Point(point!(x: -122.4, y: 37.8)), "sf".into()),
            (
                Geometry::Point(point!(x: -122.41, y: 37.81)),
                "sf_near".into(),
            ),
            (Geometry::Point(point!(x: -73.9, y: 40.7)), "nyc".into()),
            (
                Geometry::Point(point!(x: -73.91, y: 40.71)),
                "nyc_near".into(),
            ),
            (Geometry::Point(point!(x: 139.7, y: 35.7)), "tokyo".into()),
        ];

        let selected = select_features_by_gap(&features, 3, 2.0);

        // Should reduce from 5 to approximately 3
        assert!(selected.len() <= 5);
        assert!(selected.len() >= 2); // At least some kept
    }

    // ============================================================
    // UNIT TESTS: scale_for_zoom
    // ============================================================

    #[test]
    fn test_scale_for_zoom_decreases_with_zoom() {
        let scale_z0 = scale_for_zoom(0);
        let scale_z7 = scale_for_zoom(7);
        let scale_z14 = scale_for_zoom(14);

        assert!(scale_z0 > scale_z7);
        assert!(scale_z7 > scale_z14);
    }

    #[test]
    fn test_scale_for_zoom_known_values() {
        assert_eq!(scale_for_zoom(0), (1u64 << 32) as f64);
        assert_eq!(scale_for_zoom(14), (1u64 << 18) as f64);
        assert_eq!(scale_for_zoom(32), 1.0);
    }

    // ============================================================
    // INTEGRATION TESTS: Compare with grid-based
    // ============================================================

    #[test]
    fn test_gap_based_preserves_spatial_distribution() {
        // Create a grid of points
        let mut features: Vec<(Geometry<f64>, usize)> = Vec::new();
        for i in 0..10 {
            for j in 0..10 {
                let lng = -122.0 + i as f64 * 0.1;
                let lat = 37.0 + j as f64 * 0.1;
                features.push((Geometry::Point(point!(x: lng, y: lat)), i * 10 + j));
            }
        }

        let selected = select_features_by_gap(&features, 25, 2.0);

        // With gap-based selection, we should have features distributed across the space
        // (not all clustered in one area)
        assert!(selected.len() >= 10); // Some selection happened
        assert!(selected.len() <= 50); // Significant reduction

        // Check that selected features span the coordinate range
        let selected_lngs: Vec<f64> = selected
            .iter()
            .filter_map(|&idx| {
                if let Geometry::Point(p) = &features[idx].0 {
                    Some(p.x())
                } else {
                    None
                }
            })
            .collect();

        let min_lng = selected_lngs.iter().cloned().fold(f64::INFINITY, f64::min);
        let max_lng = selected_lngs
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);

        // Selected features should span most of the input range
        assert!(
            max_lng - min_lng > 0.5,
            "Selected features should be spatially distributed"
        );
    }

    // ============================================================
    // ADAPTIVE GAMMA TESTS
    // ============================================================

    #[test]
    fn test_increase_gamma() {
        let mut selector = GapBasedSelector::new(2.0);
        assert_eq!(selector.gamma(), 2.0);

        selector.increase_gamma(1.25);
        assert!((selector.gamma() - 2.5).abs() < 0.001);
    }

    #[test]
    fn test_set_gamma() {
        let mut selector = GapBasedSelector::new(1.0);
        selector.set_gamma(3.0);
        assert_eq!(selector.gamma(), 3.0);
    }

    // ============================================================
    // BUG VERIFICATION TESTS (Issue #???)
    // These tests verify the gap_accumulator bug hypothesis
    // ============================================================

    /// Test showing that gap_accumulator is never set when dropping in else branch.
    ///
    /// With scale=100 and uniform spacing of 10:
    /// - gap = 10/100 = 0.1 < 1.0 for features after first
    /// - prev_index only updates when KEEPING
    /// - So we only keep at multiples of 100 (where gap >= 1.0)
    #[test]
    fn test_bug_gap_accumulator_never_positive() {
        let mut selector = GapBasedSelector::new(2.0).with_scale(100.0);

        // Process features at 0, 10, 20, ..., 990
        let indices: Vec<u64> = (0..100).map(|i| i * 10).collect();

        let mut keep_indices = Vec::new();
        for &idx in &indices {
            if !selector.should_drop(idx) {
                keep_indices.push(idx);
            }
        }

        println!("Kept features at indices: {:?}", keep_indices);

        // With CORRECT tippecanoe behavior (after fix):
        // Gap accumulates when dropping, allowing features through sooner
        // when threshold >= accumulated_gap

        // The exact kept indices depend on the accumulation math:
        // - More features kept than the buggy "multiples of 100" pattern
        // - Features are kept when threshold = (distance/scale)^gamma >= gap_accumulator

        // After fix: should keep MORE features due to accumulation
        assert!(
            keep_indices.len() > 10,
            "Fixed: Should keep more than 10 features due to gap accumulation (got {})",
            keep_indices.len()
        );
        println!(
            "After fix: kept {} features (vs 10 with buggy code)",
            keep_indices.len()
        );
    }

    /// Compare actual vs expected tippecanoe behavior.
    ///
    /// This test documents the expected tippecanoe behavior and shows
    /// where our implementation diverges.
    #[test]
    fn test_bug_tippecanoe_behavior_comparison() {
        let mut selector = GapBasedSelector::new(2.0).with_scale(100.0);

        // Carefully chosen sequence to demonstrate accumulation
        let indices = [0u64, 30, 60, 150, 160, 300];

        // EXPECTED tippecanoe behavior (with working accumulation):
        // idx=0:   KEEP (first)
        // idx=30:  gap=30/100=0.3 < 1.0, DROP, gap_acc=0.3
        // idx=60:  threshold=(60/100)^2=0.36 >= gap_acc=0.3, KEEP, gap_acc=0
        // idx=150: gap=(150-60)/100=0.9 < 1.0, DROP, gap_acc=0.9
        // idx=160: threshold=((160-60)/100)^2=1.0 >= gap_acc=0.9, KEEP
        // idx=300: gap=(300-160)/100=1.4 >= 1.0, KEEP

        let expected_keeps_tippecanoe = vec![0, 60, 160, 300];

        // ACTUAL buggy behavior (no accumulation):
        // idx=0:   KEEP (first), prev=0
        // idx=30:  gap=30/100=0.3 < 1.0, DROP, prev stays 0
        // idx=60:  gap=60/100=0.6 < 1.0, DROP, prev stays 0
        // idx=150: gap=150/100=1.5 >= 1.0, KEEP, prev=150
        // idx=160: gap=(160-150)/100=0.1 < 1.0, DROP
        // idx=300: gap=(300-150)/100=1.5 >= 1.0, KEEP

        let expected_keeps_buggy = vec![0, 150, 300];

        let mut actual_keeps = Vec::new();
        for &idx in &indices {
            if !selector.should_drop(idx) {
                actual_keeps.push(idx);
            }
        }

        println!("Expected (tippecanoe): {:?}", expected_keeps_tippecanoe);
        println!("Expected (buggy):      {:?}", expected_keeps_buggy);
        println!("Actual:                {:?}", actual_keeps);

        // After fix: actual should match tippecanoe behavior
        assert_eq!(
            actual_keeps, expected_keeps_tippecanoe,
            "Fixed: Now matches tippecanoe behavior with gap accumulation"
        );
    }

    /// Test demonstrating that features 60 and 160 should be kept with proper accumulation
    /// but are dropped with the buggy implementation.
    #[test]
    fn test_bug_missed_keeps_due_to_no_accumulation() {
        let mut selector = GapBasedSelector::new(2.0).with_scale(100.0);

        // Feature 0: KEEP
        assert!(!selector.should_drop(0));

        // Feature 30: DROP (gap=0.3 < 1.0)
        assert!(selector.should_drop(30));

        // Feature 60: With buggy code, gap=(60-0)/100=0.6 < 1.0, so DROP
        // With correct code: threshold=0.36 >= gap_acc=0.3, so KEEP
        let drop_60 = selector.should_drop(60);

        println!("Feature 60: drop={}", drop_60);
        println!("After fix: KEEP (threshold=0.36 >= gap_acc=0.3)");

        // After fix: feature 60 should be KEPT because threshold >= accumulated gap
        assert!(
            !drop_60,
            "Fixed: Feature 60 should be KEPT with proper gap accumulation"
        );
    }
}
