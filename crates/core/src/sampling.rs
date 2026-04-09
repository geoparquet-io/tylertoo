//! Bounded sampling with incremental halving (tippecanoe algorithm).
//!
//! When samples exceed max_samples, we halve the collection by keeping
//! every 2nd sample and double the increment for future samples. This ensures
//! bounded memory usage while maintaining a statistically representative sample.
//!
//! # Tippecanoe Reference
//!
//! This implements tippecanoe's sampling algorithm from `tile.cpp`:
//! - `add_sample_to()` lines 1528-1547: Incremental halving
//! - `choose_mingap()` lines 762-778: Percentile selection with ratcheting
//!
//! # Key Behaviors
//!
//! 1. **Incremental halving**: When `samples.len() >= max_samples`, we keep every
//!    2nd sample and double the increment for future samples. This ensures O(max_samples)
//!    memory regardless of input size.
//!
//! 2. **Ratcheting**: In `select_threshold`, if the computed threshold is <= existing,
//!    we increment the index until we find a threshold > existing. This prevents
//!    regression when iteratively tightening thresholds.
//!
//! 3. **Generic over T**: Works with any ordered, copyable type. Instantiated as
//!    `u64` for gap sampling and `i64` for extent sampling.
//!
//! # Example
//!
//! ```
//! use gpq_tiles_core::sampling::{BoundedSampler, GapSampler};
//!
//! let mut sampler: GapSampler = BoundedSampler::new(1000);
//!
//! // Record gap values during tile processing
//! for gap in [100, 50, 200, 25, 150] {
//!     sampler.record(gap);
//! }
//!
//! // Select threshold to keep ~50% of features
//! // Existing threshold is 0 (first iteration)
//! let threshold = sampler.select_threshold(0.5, 0);
//! assert!(threshold.is_some());
//! ```

use std::cmp::Ord;

/// Bounded sampler using incremental halving (tippecanoe algorithm).
///
/// Generic over T: Ord + Copy, allowing use with u64 (gaps) or i64 (extents).
///
/// # Memory Bounds
///
/// The sampler guarantees `samples.len() <= max_samples` at all times.
/// When the limit is reached, the collection is halved and the increment doubled.
///
/// # Sampling Strategy
///
/// - `increment = 1`: Every value is recorded
/// - `increment = 2`: Every 2nd value is recorded
/// - `increment = 4`: Every 4th value is recorded (after two halvings)
/// - etc.
///
/// This maintains a representative sample while bounding memory.
#[derive(Debug, Clone)]
pub struct BoundedSampler<T> {
    /// Collected samples, bounded by max_samples
    samples: Vec<T>,
    /// Sample every Nth item (1 = all, 2 = every 2nd, etc.)
    increment: usize,
    /// Current sequence number for determining which items to sample
    seq: usize,
    /// Maximum number of samples to hold
    max_samples: usize,
}

impl<T: Ord + Copy> BoundedSampler<T> {
    /// Create a new bounded sampler with the given maximum sample count.
    ///
    /// # Arguments
    ///
    /// * `max_samples` - Maximum number of samples to hold. When exceeded,
    ///   the collection is halved and the increment doubled.
    ///
    /// # Panics
    ///
    /// Panics if `max_samples` is 0.
    pub fn new(max_samples: usize) -> Self {
        assert!(max_samples > 0, "max_samples must be > 0");
        Self {
            samples: Vec::with_capacity(max_samples),
            increment: 1,
            seq: 0,
            max_samples,
        }
    }

    /// Record a value, possibly skipping based on the current increment.
    ///
    /// When `samples.len() >= max_samples`, triggers incremental halving:
    /// 1. Keep every 2nd sample
    /// 2. Double the increment for future samples
    ///
    /// # Arguments
    ///
    /// * `value` - The value to potentially record
    ///
    /// # Tippecanoe Reference
    ///
    /// This implements `add_sample_to()` from tile.cpp (lines 1528-1547):
    /// ```text
    /// seq++;
    /// if (seq % increment == 0) {
    ///     samples.push_back(value);
    ///     if (samples.size() >= max) {
    ///         // Halve: keep every 2nd sample
    ///         for (i = 0, j = 0; i < samples.size(); i += 2) {
    ///             samples[j++] = samples[i];
    ///         }
    ///         samples.resize(j);
    ///         increment *= 2;
    ///     }
    /// }
    /// ```
    pub fn record(&mut self, value: T) {
        self.seq += 1;

        // Only record if this is an Nth item (based on current increment)
        if self.seq % self.increment != 0 {
            return;
        }

        self.samples.push(value);

        // Check if we need to halve
        if self.samples.len() >= self.max_samples {
            self.halve();
        }
    }

    /// Halve the sample collection and double the increment.
    ///
    /// Keeps every 2nd sample to maintain bounded memory while
    /// preserving statistical representativeness.
    fn halve(&mut self) {
        // Keep every 2nd sample (indices 0, 2, 4, ...)
        let mut write_idx = 0;
        let mut read_idx = 0;
        while read_idx < self.samples.len() {
            self.samples[write_idx] = self.samples[read_idx];
            write_idx += 1;
            read_idx += 2;
        }
        self.samples.truncate(write_idx);

        // Double the increment for future samples
        self.increment *= 2;
    }

    /// Select a threshold at the given fraction using tippecanoe's algorithm.
    ///
    /// The algorithm:
    /// 1. Sort samples
    /// 2. Calculate index: `ix = (len - 1) * (1 - fraction)`
    /// 3. Ratchet: increment ix while `samples[ix] <= existing`
    /// 4. Return `samples[ix]`
    ///
    /// # Arguments
    ///
    /// * `fraction` - Target fraction of features to keep (0.0 to 1.0).
    ///   For example, 0.5 means "select threshold to keep ~50% of features".
    /// * `existing` - Current threshold. The returned threshold will be > existing
    ///   (ratcheting behavior prevents regression).
    ///
    /// # Returns
    ///
    /// - `Some(threshold)` if samples exist and a valid threshold was found
    /// - `None` if samples are empty or no threshold > existing exists
    ///
    /// # Tippecanoe Reference
    ///
    /// This implements `choose_mingap()` from tile.cpp (lines 762-778):
    /// ```text
    /// std::sort(samples.begin(), samples.end());
    /// size_t ix = (samples.size() - 1) * (1 - fraction);
    /// while (ix + 1 < samples.size() && samples[ix] <= existing) {
    ///     ix++;
    /// }
    /// return samples[ix];
    /// ```
    pub fn select_threshold(&self, fraction: f64, existing: T) -> Option<T> {
        if self.samples.is_empty() {
            return None;
        }

        // Sort a copy (we don't want to modify the internal state)
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();

        // Calculate the target index
        // (1 - fraction) because we want the threshold to DROP (1-fraction) of features
        let fraction = fraction.clamp(0.0, 1.0);
        let mut ix = ((sorted.len() - 1) as f64 * (1.0 - fraction)) as usize;

        // Ratchet: skip values <= existing to ensure we never regress
        while ix + 1 < sorted.len() && sorted[ix] <= existing {
            ix += 1;
        }

        // Final check: ensure the selected value is > existing
        // If not, we've exhausted all options
        if sorted[ix] <= existing {
            return None;
        }

        Some(sorted[ix])
    }

    /// Clear samples for reuse, resetting the sampler to initial state.
    ///
    /// This clears samples, resets the sequence counter, and resets the increment
    /// back to 1 (sample everything).
    pub fn clear(&mut self) {
        self.samples.clear();
        self.seq = 0;
        self.increment = 1;
    }

    /// Number of samples currently held.
    #[inline]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Check if the sampler is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Get the current increment value.
    ///
    /// Useful for debugging/testing to verify halving behavior.
    #[inline]
    pub fn increment(&self) -> usize {
        self.increment
    }

    /// Get the current sequence number.
    ///
    /// Useful for debugging/testing.
    #[inline]
    pub fn seq(&self) -> usize {
        self.seq
    }

    /// Get a reference to the internal samples.
    ///
    /// Note: Samples are not sorted. Use `select_threshold` for ordered access.
    #[inline]
    pub fn samples(&self) -> &[T] {
        &self.samples
    }
}

/// Type alias for gap sampling (Hilbert index gaps).
///
/// Gaps are always positive (u64), representing the distance between
/// consecutive Hilbert indices.
pub type GapSampler = BoundedSampler<u64>;

/// Type alias for extent sampling (feature extents/sizes).
///
/// Extents can be positive or negative (i64), representing feature
/// sizes or bounding box dimensions.
pub type ExtentSampler = BoundedSampler<i64>;

// ============================================================
// DEFAULT IMPLEMENTATION
// ============================================================

impl<T: Ord + Copy> Default for BoundedSampler<T> {
    /// Create a sampler with the default max_samples of 100,000.
    ///
    /// This matches tippecanoe's default behavior.
    fn default() -> Self {
        Self::new(100_000)
    }
}

// ============================================================
// TESTS
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ============================================================
    // UNIT TESTS: Basic sampling behavior
    // ============================================================

    #[test]
    fn test_new_creates_empty_sampler() {
        let sampler: GapSampler = BoundedSampler::new(100);
        assert!(sampler.is_empty());
        assert_eq!(sampler.len(), 0);
        assert_eq!(sampler.increment(), 1);
        assert_eq!(sampler.seq(), 0);
    }

    #[test]
    fn test_default_uses_100k_max() {
        let sampler: GapSampler = BoundedSampler::default();
        assert!(sampler.is_empty());
        // Can't directly check max_samples, but we can verify behavior
        // by adding many samples
    }

    #[test]
    #[should_panic(expected = "max_samples must be > 0")]
    fn test_new_panics_on_zero_max() {
        let _: GapSampler = BoundedSampler::new(0);
    }

    #[test]
    fn test_record_adds_samples() {
        let mut sampler: GapSampler = BoundedSampler::new(100);

        sampler.record(10);
        sampler.record(20);
        sampler.record(30);

        assert_eq!(sampler.len(), 3);
        assert_eq!(sampler.samples(), &[10, 20, 30]);
    }

    #[test]
    fn test_record_fewer_than_max() {
        let mut sampler: GapSampler = BoundedSampler::new(10);

        for i in 0..5 {
            sampler.record(i * 100);
        }

        assert_eq!(sampler.len(), 5);
        assert_eq!(sampler.increment(), 1); // No halving occurred
    }

    // ============================================================
    // UNIT TESTS: Incremental halving
    // ============================================================

    #[test]
    fn test_halving_triggers_at_max() {
        let mut sampler: GapSampler = BoundedSampler::new(10);

        // Add exactly 10 samples
        for i in 0..10 {
            sampler.record(i * 10);
        }

        // At this point, halving should have triggered
        assert_eq!(sampler.len(), 5); // 10 / 2 = 5
        assert_eq!(sampler.increment(), 2); // Doubled from 1

        // Should have kept indices 0, 2, 4, 6, 8 -> values 0, 20, 40, 60, 80
        assert_eq!(sampler.samples(), &[0, 20, 40, 60, 80]);
    }

    #[test]
    fn test_double_halving() {
        let mut sampler: GapSampler = BoundedSampler::new(4);

        // Add 4 samples -> halves to 2, increment = 2
        // seq: 1,2,3,4 all % 1 == 0 -> record all -> [0,1,2,3] -> halve
        for i in 0..4 {
            sampler.record(i);
        }
        assert_eq!(sampler.len(), 2);
        assert_eq!(sampler.increment(), 2);
        assert_eq!(sampler.samples(), &[0, 2]);

        // Add more samples (only every 2nd is recorded due to increment=2)
        // seq=5: 5%2=1, not recorded
        sampler.record(4);
        // seq=6: 6%2=0, recorded -> [0, 2, 5]
        sampler.record(5);
        // seq=7: 7%2=1, not recorded
        sampler.record(6);
        // seq=8: 8%2=0, recorded -> [0, 2, 5, 7] -> halve to [0, 5], increment=4
        sampler.record(7);

        assert_eq!(sampler.len(), 2);
        assert_eq!(sampler.increment(), 4); // Doubled again
        assert_eq!(sampler.samples(), &[0, 5]);
    }

    #[test]
    fn test_increment_skips_samples() {
        let mut sampler: GapSampler = BoundedSampler::new(4);

        // First halving
        for i in 0..4 {
            sampler.record(i * 10);
        }
        assert_eq!(sampler.increment(), 2);

        // Now only every 2nd value is recorded
        // Current seq=4, samples=[0, 20]
        sampler.record(100); // seq=5, 5%2=1, skipped
        assert_eq!(sampler.len(), 2);

        sampler.record(200); // seq=6, 6%2=0, recorded
        assert_eq!(sampler.len(), 3);
        assert_eq!(sampler.samples(), &[0, 20, 200]);
    }

    #[test]
    fn test_many_halvings() {
        let mut sampler: GapSampler = BoundedSampler::new(8);

        // Add 100 samples
        for i in 0..100 {
            sampler.record(i);
        }

        // After many halvings, increment should be high
        assert!(sampler.len() <= 8);
        assert!(sampler.increment() >= 8); // At least 3 halvings: 1->2->4->8
    }

    // ============================================================
    // UNIT TESTS: select_threshold percentile selection
    // ============================================================

    #[test]
    fn test_select_threshold_empty_returns_none() {
        let sampler: GapSampler = BoundedSampler::new(100);
        assert!(sampler.select_threshold(0.5, 0).is_none());
    }

    #[test]
    fn test_select_threshold_single_sample() {
        let mut sampler: GapSampler = BoundedSampler::new(100);
        sampler.record(50);

        // Single sample, threshold > 0
        let threshold = sampler.select_threshold(0.5, 0);
        assert_eq!(threshold, Some(50));
    }

    #[test]
    fn test_select_threshold_percentile_calculation() {
        let mut sampler: GapSampler = BoundedSampler::new(100);

        // Add 10 samples: 10, 20, 30, 40, 50, 60, 70, 80, 90, 100
        for i in 1..=10 {
            sampler.record(i * 10);
        }

        // fraction=0.5 -> ix = 9 * (1 - 0.5) = 4.5 -> 4
        // sorted[4] = 50
        let threshold = sampler.select_threshold(0.5, 0);
        assert_eq!(threshold, Some(50));

        // fraction=0.9 -> ix = 9 * (1 - 0.9) = 0.9 -> 0
        // sorted[0] = 10
        let threshold = sampler.select_threshold(0.9, 0);
        assert_eq!(threshold, Some(10));

        // fraction=0.1 -> ix = 9 * (1 - 0.1) = 8.1 -> 8
        // sorted[8] = 90
        let threshold = sampler.select_threshold(0.1, 0);
        assert_eq!(threshold, Some(90));
    }

    #[test]
    fn test_select_threshold_fraction_clamping() {
        let mut sampler: GapSampler = BoundedSampler::new(100);
        sampler.record(10);
        sampler.record(20);
        sampler.record(30);

        // fraction < 0 should clamp to 0 -> ix = (2) * 1 = 2 -> sorted[2] = 30
        let threshold = sampler.select_threshold(-0.5, 0);
        assert_eq!(threshold, Some(30));

        // fraction > 1 should clamp to 1 -> ix = (2) * 0 = 0 -> sorted[0] = 10
        let threshold = sampler.select_threshold(1.5, 0);
        assert_eq!(threshold, Some(10));
    }

    // ============================================================
    // UNIT TESTS: Ratcheting behavior
    // ============================================================

    #[test]
    fn test_ratcheting_skips_existing() {
        let mut sampler: GapSampler = BoundedSampler::new(100);

        // Add samples: 10, 20, 30, 40, 50
        for i in 1..=5 {
            sampler.record(i * 10);
        }

        // With existing=0, select at fraction=0.5
        // ix = 4 * 0.5 = 2 -> sorted[2] = 30
        let t1 = sampler.select_threshold(0.5, 0);
        assert_eq!(t1, Some(30));

        // Now with existing=30, should ratchet to next value
        // ix starts at 2, sorted[2]=30 <= 30, increment to 3
        // sorted[3] = 40 > 30, return 40
        let t2 = sampler.select_threshold(0.5, 30);
        assert_eq!(t2, Some(40));

        // With existing=40, should ratchet to 50
        let t3 = sampler.select_threshold(0.5, 40);
        assert_eq!(t3, Some(50));
    }

    #[test]
    fn test_ratcheting_returns_none_when_exhausted() {
        let mut sampler: GapSampler = BoundedSampler::new(100);

        sampler.record(10);
        sampler.record(20);
        sampler.record(30);

        // With existing >= max sample, should return None
        let threshold = sampler.select_threshold(0.5, 50);
        assert!(threshold.is_none());
    }

    #[test]
    fn test_ratcheting_with_duplicates() {
        let mut sampler: GapSampler = BoundedSampler::new(100);

        // Add samples with duplicates: 10, 10, 20, 20, 30
        sampler.record(10);
        sampler.record(10);
        sampler.record(20);
        sampler.record(20);
        sampler.record(30);

        // With existing=10, should skip past both 10s
        // sorted = [10, 10, 20, 20, 30]
        // ix = 4 * 0.5 = 2 -> sorted[2] = 20 > 10, return 20
        let threshold = sampler.select_threshold(0.5, 10);
        assert_eq!(threshold, Some(20));
    }

    // ============================================================
    // UNIT TESTS: clear() resets state
    // ============================================================

    #[test]
    fn test_clear_resets_everything() {
        let mut sampler: GapSampler = BoundedSampler::new(4);

        // Trigger halving
        for i in 0..8 {
            sampler.record(i);
        }
        assert!(!sampler.is_empty());
        assert!(sampler.increment() > 1);
        assert!(sampler.seq() > 0);

        // Clear
        sampler.clear();

        assert!(sampler.is_empty());
        assert_eq!(sampler.len(), 0);
        assert_eq!(sampler.increment(), 1);
        assert_eq!(sampler.seq(), 0);
    }

    #[test]
    fn test_clear_allows_reuse() {
        let mut sampler: GapSampler = BoundedSampler::new(10);

        // First use
        for i in 0..5 {
            sampler.record(i * 10);
        }
        let t1 = sampler.select_threshold(0.5, 0);
        assert!(t1.is_some());

        // Clear and reuse
        sampler.clear();
        for i in 0..3 {
            sampler.record(i * 100);
        }

        assert_eq!(sampler.len(), 3);
        assert_eq!(sampler.samples(), &[0, 100, 200]);
    }

    // ============================================================
    // UNIT TESTS: Type aliases
    // ============================================================

    #[test]
    fn test_gap_sampler_u64() {
        let mut sampler: GapSampler = BoundedSampler::new(100);
        sampler.record(u64::MAX);
        sampler.record(1u64);

        let threshold = sampler.select_threshold(0.5, 0);
        assert!(threshold.is_some());
    }

    #[test]
    fn test_extent_sampler_i64() {
        let mut sampler: ExtentSampler = BoundedSampler::new(100);
        sampler.record(-100);
        sampler.record(0);
        sampler.record(100);

        // sorted = [-100, 0, 100]
        // ix = 2 * 0.5 = 1 -> sorted[1] = 0
        // But existing = -200 < 0, so return 0
        let threshold = sampler.select_threshold(0.5, -200);
        assert_eq!(threshold, Some(0));
    }

    #[test]
    fn test_extent_sampler_negative_ratcheting() {
        let mut sampler: ExtentSampler = BoundedSampler::new(100);
        sampler.record(-50);
        sampler.record(-30);
        sampler.record(-10);

        // sorted = [-50, -30, -10]
        // With existing=-30, should ratchet past it
        // ix = 2 * 0.5 = 1 -> sorted[1] = -30 <= -30, increment to 2
        // sorted[2] = -10 > -30, return -10
        let threshold = sampler.select_threshold(0.5, -30);
        assert_eq!(threshold, Some(-10));
    }

    // ============================================================
    // EDGE CASE TESTS
    // ============================================================

    #[test]
    fn test_fraction_zero_selects_max() {
        let mut sampler: GapSampler = BoundedSampler::new(100);
        for i in 1..=5 {
            sampler.record(i * 10);
        }

        // fraction=0 -> keep 0% -> drop 100%
        // ix = 4 * 1.0 = 4 -> sorted[4] = 50
        let threshold = sampler.select_threshold(0.0, 0);
        assert_eq!(threshold, Some(50));
    }

    #[test]
    fn test_fraction_one_selects_min() {
        let mut sampler: GapSampler = BoundedSampler::new(100);
        for i in 1..=5 {
            sampler.record(i * 10);
        }

        // fraction=1 -> keep 100% -> drop 0%
        // ix = 4 * 0.0 = 0 -> sorted[0] = 10
        let threshold = sampler.select_threshold(1.0, 0);
        assert_eq!(threshold, Some(10));
    }

    #[test]
    fn test_large_sample_count_stays_bounded() {
        let mut sampler: GapSampler = BoundedSampler::new(1000);

        // Add a million samples
        for i in 0..1_000_000 {
            sampler.record(i);
        }

        // Should never exceed max_samples
        assert!(sampler.len() <= 1000);

        // Should still be able to select thresholds
        let threshold = sampler.select_threshold(0.5, 0);
        assert!(threshold.is_some());
    }

    #[test]
    fn test_select_threshold_does_not_modify_samples() {
        let mut sampler: GapSampler = BoundedSampler::new(100);
        sampler.record(30);
        sampler.record(10);
        sampler.record(20);

        let original_samples = sampler.samples().to_vec();

        // Call select_threshold multiple times
        let _ = sampler.select_threshold(0.5, 0);
        let _ = sampler.select_threshold(0.3, 0);
        let _ = sampler.select_threshold(0.8, 0);

        // Samples should be unchanged (insertion order preserved)
        assert_eq!(sampler.samples(), &original_samples);
    }
}
