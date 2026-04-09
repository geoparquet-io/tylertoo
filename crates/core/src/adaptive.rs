//! Adaptive threshold tracking for per-zoom threshold management.
//!
//! Tracks initial and observed thresholds, supports zoom-level retry
//! detection and threshold propagation between zoom levels.
//!
//! # Overview
//!
//! During tile encoding, some tiles may exceed size limits even with initial
//! thresholds computed from metadata. This module tracks:
//!
//! - **Initial thresholds**: Computed from metadata scan (or defaults)
//! - **Observed thresholds**: Maximum thresholds actually used during encoding
//! - **Retry flags**: Whether any tile at each zoom needed a higher threshold
//!
//! # Thread Safety
//!
//! All operations are thread-safe using `DashMap` for concurrent access from
//! Rayon parallel iterators. Atomic operations ensure correctness when multiple
//! threads report thresholds simultaneously.
//!
//! # Threshold Propagation
//!
//! Thresholds can be propagated from one zoom level to the next. This allows
//! zoom level N+1 to start with the maximum threshold observed at zoom N,
//! reducing retry iterations at higher zoom levels.

use dashmap::DashMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// Per-zoom adaptive threshold state.
///
/// Thread-safe for parallel tile encoding via Rayon.
///
/// # Example
///
/// ```
/// use gpq_tiles_core::adaptive::AdaptiveTargets;
///
/// let targets = AdaptiveTargets::new();
///
/// // Set initial thresholds for zoom level 10
/// targets.set_initial_mingap(10, 100);
/// targets.set_initial_minextent(10, 50);
///
/// // During encoding, tiles report their final thresholds
/// targets.report_mingap(10, 150); // Higher than initial -> retry needed
///
/// assert!(targets.needs_retry(10));
/// assert_eq!(targets.get_mingap(10), 150); // Returns observed max
/// ```
pub struct AdaptiveTargets {
    /// Initial thresholds computed from metadata scan (or defaults)
    initial_mingap: DashMap<u8, u64>,
    initial_minextent: DashMap<u8, i64>,

    /// Maximum observed thresholds during encoding
    observed_mingap: DashMap<u8, u64>,
    observed_minextent: DashMap<u8, i64>,

    /// Whether any tile at each zoom increased its threshold
    zoom_needs_retry: DashMap<u8, AtomicBool>,
}

impl AdaptiveTargets {
    /// Create new adaptive targets with no initial thresholds.
    ///
    /// All zoom levels start with threshold 0 until explicitly set.
    pub fn new() -> Self {
        Self {
            initial_mingap: DashMap::new(),
            initial_minextent: DashMap::new(),
            observed_mingap: DashMap::new(),
            observed_minextent: DashMap::new(),
            zoom_needs_retry: DashMap::new(),
        }
    }

    /// Set initial mingap threshold for a zoom level.
    ///
    /// This is typically called after scanning file metadata to compute
    /// expected thresholds based on feature density.
    pub fn set_initial_mingap(&self, zoom: u8, threshold: u64) {
        self.initial_mingap.insert(zoom, threshold);
    }

    /// Set initial minextent threshold for a zoom level.
    ///
    /// This is typically called after scanning file metadata to compute
    /// expected thresholds based on feature density.
    pub fn set_initial_minextent(&self, zoom: u8, threshold: i64) {
        self.initial_minextent.insert(zoom, threshold);
    }

    /// Get current mingap threshold for a zoom level.
    ///
    /// Returns the maximum of:
    /// - Observed threshold (if any tile has reported)
    /// - Initial threshold (if set)
    /// - 0 (default)
    pub fn get_mingap(&self, zoom: u8) -> u64 {
        let observed = self.observed_mingap.get(&zoom).map(|v| *v).unwrap_or(0);
        let initial = self.initial_mingap.get(&zoom).map(|v| *v).unwrap_or(0);
        observed.max(initial)
    }

    /// Get current minextent threshold for a zoom level.
    ///
    /// Returns the maximum of:
    /// - Observed threshold (if any tile has reported)
    /// - Initial threshold (if set)
    /// - 0 (default)
    pub fn get_minextent(&self, zoom: u8) -> i64 {
        let observed = self.observed_minextent.get(&zoom).map(|v| *v).unwrap_or(0);
        let initial = self.initial_minextent.get(&zoom).map(|v| *v).unwrap_or(0);
        observed.max(initial)
    }

    /// Report a tile's final mingap threshold after encoding.
    ///
    /// Updates the observed maximum and sets the retry flag if the threshold
    /// exceeds the initial value.
    ///
    /// # Thread Safety
    ///
    /// This method is safe to call from multiple threads. The observed
    /// threshold ratchets up (only increases, never decreases).
    pub fn report_mingap(&self, zoom: u8, threshold: u64) {
        // Update observed max (ratcheting: only increase)
        self.observed_mingap
            .entry(zoom)
            .and_modify(|v| *v = (*v).max(threshold))
            .or_insert(threshold);

        // Check if we exceeded initial threshold -> need retry
        let initial = self.initial_mingap.get(&zoom).map(|v| *v).unwrap_or(0);
        if threshold > initial {
            self.set_retry_flag(zoom);
        }
    }

    /// Report a tile's final minextent threshold after encoding.
    ///
    /// Updates the observed maximum and sets the retry flag if the threshold
    /// exceeds the initial value.
    ///
    /// # Thread Safety
    ///
    /// This method is safe to call from multiple threads. The observed
    /// threshold ratchets up (only increases, never decreases).
    pub fn report_minextent(&self, zoom: u8, threshold: i64) {
        // Update observed max (ratcheting: only increase)
        self.observed_minextent
            .entry(zoom)
            .and_modify(|v| *v = (*v).max(threshold))
            .or_insert(threshold);

        // Check if we exceeded initial threshold -> need retry
        let initial = self.initial_minextent.get(&zoom).map(|v| *v).unwrap_or(0);
        if threshold > initial {
            self.set_retry_flag(zoom);
        }
    }

    /// Check if a zoom level needs re-encoding.
    ///
    /// Returns `true` if any tile at this zoom level reported a threshold
    /// higher than the initial value.
    pub fn needs_retry(&self, zoom: u8) -> bool {
        self.zoom_needs_retry
            .get(&zoom)
            .map(|v| v.load(Ordering::Acquire))
            .unwrap_or(false)
    }

    /// Clear the retry flag for a zoom level.
    ///
    /// Called after re-encoding a zoom level with updated thresholds.
    pub fn clear_retry_flag(&self, zoom: u8) {
        if let Some(flag) = self.zoom_needs_retry.get(&zoom) {
            flag.store(false, Ordering::Release);
        }
    }

    /// Propagate thresholds to the next zoom level.
    ///
    /// Sets the initial threshold for `from_zoom + 1` to the maximum of:
    /// - Observed threshold at `from_zoom` (if any)
    /// - Initial threshold at `from_zoom`
    ///
    /// This ensures the next zoom level starts with optimal thresholds,
    /// reducing retry iterations.
    ///
    /// # Panics
    ///
    /// Panics if `from_zoom` is 255 (no next zoom level).
    pub fn propagate_to_next_zoom(&self, from_zoom: u8) {
        assert!(from_zoom < 255, "Cannot propagate from zoom 255");

        let next_zoom = from_zoom + 1;

        // Propagate mingap
        let mingap = self.get_mingap(from_zoom);
        if mingap > 0 {
            self.set_initial_mingap(next_zoom, mingap);
        }

        // Propagate minextent
        let minextent = self.get_minextent(from_zoom);
        if minextent > 0 {
            self.set_initial_minextent(next_zoom, minextent);
        }
    }

    /// Set the retry flag for a zoom level (internal helper).
    fn set_retry_flag(&self, zoom: u8) {
        self.zoom_needs_retry
            .entry(zoom)
            .and_modify(|v| v.store(true, Ordering::Release))
            .or_insert(AtomicBool::new(true));
    }
}

impl Default for AdaptiveTargets {
    fn default() -> Self {
        Self::new()
    }
}

// Explicit Send + Sync implementation documentation
// DashMap<K, V> is Send + Sync when K and V are Send + Sync
// AtomicBool is Send + Sync
// Therefore AdaptiveTargets is automatically Send + Sync

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_get_returns_zero_for_unset_zoom() {
        let targets = AdaptiveTargets::new();

        assert_eq!(targets.get_mingap(5), 0);
        assert_eq!(targets.get_minextent(5), 0);
        assert!(!targets.needs_retry(5));
    }

    #[test]
    fn test_set_initial_and_get() {
        let targets = AdaptiveTargets::new();

        targets.set_initial_mingap(10, 100);
        targets.set_initial_minextent(10, 50);

        assert_eq!(targets.get_mingap(10), 100);
        assert_eq!(targets.get_minextent(10), 50);

        // Other zoom levels should still be 0
        assert_eq!(targets.get_mingap(11), 0);
        assert_eq!(targets.get_minextent(11), 0);
    }

    #[test]
    fn test_report_updates_observed_max() {
        let targets = AdaptiveTargets::new();

        // Report first threshold
        targets.report_mingap(10, 100);
        assert_eq!(targets.get_mingap(10), 100);

        // Report higher threshold -> should update
        targets.report_mingap(10, 150);
        assert_eq!(targets.get_mingap(10), 150);

        // Report lower threshold -> should NOT update (ratcheting)
        targets.report_mingap(10, 80);
        assert_eq!(targets.get_mingap(10), 150);
    }

    #[test]
    fn test_report_updates_observed_max_minextent() {
        let targets = AdaptiveTargets::new();

        targets.report_minextent(10, 50);
        assert_eq!(targets.get_minextent(10), 50);

        targets.report_minextent(10, 75);
        assert_eq!(targets.get_minextent(10), 75);

        targets.report_minextent(10, 30);
        assert_eq!(targets.get_minextent(10), 75);
    }

    #[test]
    fn test_report_sets_retry_flag_when_threshold_exceeds_initial() {
        let targets = AdaptiveTargets::new();

        // Set initial threshold
        targets.set_initial_mingap(10, 100);

        // Report threshold <= initial -> no retry
        targets.report_mingap(10, 80);
        assert!(!targets.needs_retry(10));

        targets.report_mingap(10, 100);
        assert!(!targets.needs_retry(10));

        // Report threshold > initial -> retry needed
        targets.report_mingap(10, 150);
        assert!(targets.needs_retry(10));
    }

    #[test]
    fn test_report_sets_retry_flag_minextent() {
        let targets = AdaptiveTargets::new();

        targets.set_initial_minextent(10, 50);

        targets.report_minextent(10, 50);
        assert!(!targets.needs_retry(10));

        targets.report_minextent(10, 60);
        assert!(targets.needs_retry(10));
    }

    #[test]
    fn test_clear_retry_flag() {
        let targets = AdaptiveTargets::new();

        targets.set_initial_mingap(10, 100);
        targets.report_mingap(10, 150);
        assert!(targets.needs_retry(10));

        targets.clear_retry_flag(10);
        assert!(!targets.needs_retry(10));
    }

    #[test]
    fn test_clear_retry_flag_for_unset_zoom_is_noop() {
        let targets = AdaptiveTargets::new();

        // Should not panic
        targets.clear_retry_flag(10);
        assert!(!targets.needs_retry(10));
    }

    #[test]
    fn test_propagate_to_next_zoom() {
        let targets = AdaptiveTargets::new();

        // Set initial and observe higher threshold at zoom 10
        targets.set_initial_mingap(10, 100);
        targets.set_initial_minextent(10, 50);
        targets.report_mingap(10, 200);
        targets.report_minextent(10, 75);

        // Propagate to zoom 11
        targets.propagate_to_next_zoom(10);

        // Zoom 11 should start with max(observed, initial)
        assert_eq!(targets.get_mingap(11), 200);
        assert_eq!(targets.get_minextent(11), 75);
    }

    #[test]
    fn test_propagate_uses_initial_when_no_observation() {
        let targets = AdaptiveTargets::new();

        targets.set_initial_mingap(10, 100);
        targets.set_initial_minextent(10, 50);

        targets.propagate_to_next_zoom(10);

        // Should propagate initial values
        assert_eq!(targets.get_mingap(11), 100);
        assert_eq!(targets.get_minextent(11), 50);
    }

    #[test]
    fn test_propagate_does_nothing_when_zero() {
        let targets = AdaptiveTargets::new();

        // No thresholds set for zoom 10
        targets.propagate_to_next_zoom(10);

        // Zoom 11 should still be 0
        assert_eq!(targets.get_mingap(11), 0);
        assert_eq!(targets.get_minextent(11), 0);
    }

    #[test]
    #[should_panic(expected = "Cannot propagate from zoom 255")]
    fn test_propagate_panics_at_max_zoom() {
        let targets = AdaptiveTargets::new();
        targets.propagate_to_next_zoom(255);
    }

    #[test]
    fn test_thread_safety_multiple_reporters() {
        let targets = Arc::new(AdaptiveTargets::new());
        targets.set_initial_mingap(10, 100);

        let mut handles = vec![];

        // Spawn 10 threads, each reporting different thresholds
        for i in 0..10 {
            let targets = Arc::clone(&targets);
            let handle = thread::spawn(move || {
                // Each thread reports its index * 50
                let threshold = (i + 1) * 50;
                targets.report_mingap(10, threshold as u64);
            });
            handles.push(handle);
        }

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // Should have the max threshold: 10 * 50 = 500
        assert_eq!(targets.get_mingap(10), 500);

        // Should need retry since 500 > 100 (initial)
        assert!(targets.needs_retry(10));
    }

    #[test]
    fn test_thread_safety_concurrent_get_and_set() {
        let targets = Arc::new(AdaptiveTargets::new());

        let mut handles = vec![];

        // Writer thread
        let targets_w = Arc::clone(&targets);
        handles.push(thread::spawn(move || {
            for i in 0..100 {
                targets_w.set_initial_mingap(5, i);
            }
        }));

        // Reader threads
        for _ in 0..5 {
            let targets_r = Arc::clone(&targets);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    let _ = targets_r.get_mingap(5);
                }
            }));
        }

        // Should complete without deadlock or panic
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_default_impl() {
        let targets = AdaptiveTargets::default();
        assert_eq!(targets.get_mingap(0), 0);
        assert_eq!(targets.get_minextent(0), 0);
    }

    #[test]
    fn test_get_returns_max_of_initial_and_observed() {
        let targets = AdaptiveTargets::new();

        // Set high initial
        targets.set_initial_mingap(10, 200);

        // Report lower observed
        targets.report_mingap(10, 100);

        // get should return max (initial in this case)
        assert_eq!(targets.get_mingap(10), 200);

        // Now report higher observed
        targets.report_mingap(10, 300);

        // get should return observed
        assert_eq!(targets.get_mingap(10), 300);
    }

    #[test]
    fn test_multiple_zoom_levels_independent() {
        let targets = AdaptiveTargets::new();

        targets.set_initial_mingap(10, 100);
        targets.set_initial_mingap(11, 200);

        targets.report_mingap(10, 150);
        targets.report_mingap(11, 180);

        // Zoom 10: observed > initial -> retry
        assert!(targets.needs_retry(10));

        // Zoom 11: observed < initial -> no retry
        assert!(!targets.needs_retry(11));

        assert_eq!(targets.get_mingap(10), 150);
        assert_eq!(targets.get_mingap(11), 200); // initial is higher
    }
}
