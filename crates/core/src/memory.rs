//! Memory tracking utilities for streaming processing.
//!
//! Provides estimation and tracking of memory usage during tile generation
//! to support memory-bounded streaming.

use std::mem::size_of;

use geo::Geometry;

/// Estimate the heap size of a geometry in bytes.
///
/// This is an approximation based on the geometry type and number of coordinates.
/// Actual memory usage may vary due to allocator overhead and alignment.
pub fn estimate_geometry_size(geom: &Geometry<f64>) -> usize {
    match geom {
        Geometry::Point(_) => size_of::<geo::Point<f64>>(),
        Geometry::Line(_) => size_of::<geo::Line<f64>>() + 2 * size_of::<geo::Coord<f64>>(),
        Geometry::LineString(ls) => {
            size_of::<geo::LineString<f64>>()
                + ls.0.len() * size_of::<geo::Coord<f64>>()
                + size_of::<Vec<geo::Coord<f64>>>() // Vec overhead
        }
        Geometry::Polygon(p) => {
            let exterior_size = p.exterior().0.len() * size_of::<geo::Coord<f64>>();
            let interior_size: usize = p
                .interiors()
                .iter()
                .map(|ring| ring.0.len() * size_of::<geo::Coord<f64>>())
                .sum();
            size_of::<geo::Polygon<f64>>()
                + exterior_size
                + interior_size
                + (1 + p.interiors().len()) * size_of::<Vec<geo::Coord<f64>>>()
        }
        Geometry::MultiPoint(mp) => {
            size_of::<geo::MultiPoint<f64>>()
                + mp.0.len() * size_of::<geo::Point<f64>>()
                + size_of::<Vec<geo::Point<f64>>>()
        }
        Geometry::MultiLineString(mls) => {
            let lines_size: usize = mls
                .0
                .iter()
                .map(|ls| ls.0.len() * size_of::<geo::Coord<f64>>())
                .sum();
            size_of::<geo::MultiLineString<f64>>()
                + lines_size
                + mls.0.len() * size_of::<Vec<geo::Coord<f64>>>()
                + size_of::<Vec<geo::LineString<f64>>>()
        }
        Geometry::MultiPolygon(mp) => {
            let polys_size: usize =
                mp.0.iter()
                    .map(|p| {
                        let exterior_size = p.exterior().0.len() * size_of::<geo::Coord<f64>>();
                        let interior_size: usize = p
                            .interiors()
                            .iter()
                            .map(|ring| ring.0.len() * size_of::<geo::Coord<f64>>())
                            .sum();
                        exterior_size + interior_size
                    })
                    .sum();
            size_of::<geo::MultiPolygon<f64>>()
                + polys_size
                + mp.0.len() * size_of::<geo::Polygon<f64>>()
                + size_of::<Vec<geo::Polygon<f64>>>()
        }
        Geometry::GeometryCollection(gc) => {
            let geoms_size: usize = gc.0.iter().map(estimate_geometry_size).sum();
            size_of::<geo::GeometryCollection<f64>>() + geoms_size + size_of::<Vec<Geometry<f64>>>()
        }
        Geometry::Rect(_) => size_of::<geo::Rect<f64>>(),
        Geometry::Triangle(_) => size_of::<geo::Triangle<f64>>(),
    }
}

/// Memory usage tracker for streaming processing.
#[derive(Debug, Default)]
pub struct MemoryTracker {
    /// Current estimated memory usage in bytes
    current_bytes: usize,
    /// Peak memory usage seen so far
    peak_bytes: usize,
    /// Memory budget (if set)
    budget: Option<usize>,
    /// Number of times budget was exceeded
    budget_exceeded_count: usize,
}

impl MemoryTracker {
    /// Create a new memory tracker without a budget.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new memory tracker with a memory budget.
    pub fn with_budget(budget: usize) -> Self {
        Self {
            budget: Some(budget),
            ..Default::default()
        }
    }

    /// Add memory usage.
    pub fn add(&mut self, bytes: usize) {
        self.current_bytes += bytes;
        if self.current_bytes > self.peak_bytes {
            self.peak_bytes = self.current_bytes;
        }
    }

    /// Remove memory usage.
    pub fn remove(&mut self, bytes: usize) {
        self.current_bytes = self.current_bytes.saturating_sub(bytes);
    }

    /// Reset current usage (e.g., after flushing a row group).
    pub fn reset_current(&mut self) {
        self.current_bytes = 0;
    }

    /// Check if current usage exceeds the budget.
    pub fn is_over_budget(&self) -> bool {
        match self.budget {
            Some(budget) => self.current_bytes > budget,
            None => false,
        }
    }

    /// Record that budget was exceeded.
    pub fn record_budget_exceeded(&mut self) {
        self.budget_exceeded_count += 1;
    }

    /// Get current memory usage.
    pub fn current(&self) -> usize {
        self.current_bytes
    }

    /// Get peak memory usage.
    pub fn peak(&self) -> usize {
        self.peak_bytes
    }

    /// Get the memory budget if set.
    pub fn budget(&self) -> Option<usize> {
        self.budget
    }

    /// Get the number of times budget was exceeded.
    pub fn budget_exceeded_count(&self) -> usize {
        self.budget_exceeded_count
    }
}

/// Statistics about memory usage during streaming.
///
/// **Important (Issue #152)**: `peak_bytes` now reports actual peak RSS
/// (resident set size) from OS sampling, not accumulated throughput.
/// This gives accurate memory usage even when disk spills occur.
///
/// For throughput tracking (cumulative bytes processed), use `throughput_bytes`.
#[derive(Debug, Clone)]
pub struct MemoryStats {
    /// Peak actual memory usage in bytes (RSS from OS sampling).
    ///
    /// This is the true peak resident memory, bounded by physical RAM.
    /// Use this to understand actual memory requirements.
    pub peak_bytes: usize,
    /// Peak throughput estimate in bytes (accumulated bytes processed).
    ///
    /// This may exceed physical RAM as it counts all bytes flowing through
    /// the pipeline, even if later spilled to disk. Useful for understanding
    /// data volume, but NOT for memory sizing.
    pub throughput_bytes: usize,
    /// Memory budget if set
    pub budget: Option<usize>,
    /// Number of times budget was exceeded
    pub budget_exceeded_count: usize,
}

impl MemoryStats {
    /// Create stats from a throughput tracker only (legacy behavior).
    ///
    /// **Deprecated**: Use `from_trackers()` for accurate RSS-based peak memory.
    /// This method exists for backward compatibility but reports throughput
    /// as peak_bytes, which may exceed actual memory usage.
    pub fn from_tracker(tracker: &MemoryTracker) -> Self {
        Self {
            // Legacy: use throughput as peak (incorrect but backward-compatible)
            peak_bytes: tracker.peak(),
            throughput_bytes: tracker.peak(),
            budget: tracker.budget(),
            budget_exceeded_count: tracker.budget_exceeded_count(),
        }
    }

    /// Create stats from both throughput and RSS trackers.
    ///
    /// This is the recommended constructor as it provides accurate peak RSS
    /// measurement alongside throughput tracking.
    pub fn from_trackers(mem_tracker: &MemoryTracker, rss_tracker: &RssTracker) -> Self {
        Self {
            peak_bytes: rss_tracker.peak_rss() as usize,
            throughput_bytes: mem_tracker.peak(),
            budget: mem_tracker.budget(),
            budget_exceeded_count: mem_tracker.budget_exceeded_count(),
        }
    }

    /// Check if we stayed within budget.
    pub fn within_budget(&self) -> bool {
        match self.budget {
            Some(budget) => self.peak_bytes <= budget,
            None => true,
        }
    }

    /// Format peak memory as human-readable string.
    pub fn peak_formatted(&self) -> String {
        format_bytes(self.peak_bytes)
    }

    /// Format throughput as human-readable string.
    pub fn throughput_formatted(&self) -> String {
        format_bytes(self.throughput_bytes)
    }

    /// Format budget as human-readable string.
    pub fn budget_formatted(&self) -> Option<String> {
        self.budget.map(format_bytes)
    }
}

/// Format bytes as human-readable string (KB, MB, GB).
pub fn format_bytes(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * KB;
    const GB: usize = 1024 * MB;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

// ============================================================================
// RSS (Resident Set Size) Tracker - Actual Memory Measurement
// ============================================================================

/// Tracks actual process RSS (Resident Set Size) by sampling the OS.
///
/// Unlike [`MemoryTracker`] which accumulates byte counts, this tracker
/// queries the operating system for actual memory usage. This gives accurate
/// peak memory measurements regardless of disk spills or internal memory
/// management.
///
/// # Platform Support
///
/// - **Linux**: Reads from `/proc/self/smaps` (accurate) or `/proc/self/statm` (fast)
/// - **macOS**: Uses `mach_task_info`
/// - **Windows**: Uses `GetProcessMemoryInfo`
///
/// # Example
///
/// ```
/// use gpq_tiles_core::memory::RssTracker;
///
/// let mut tracker = RssTracker::new();
///
/// // Sample periodically during processing
/// tracker.sample();
/// // ... do work ...
/// tracker.sample();
///
/// println!("Peak RSS: {} bytes", tracker.peak_rss());
/// ```
#[derive(Debug, Default)]
pub struct RssTracker {
    /// Current RSS (from last sample)
    current_rss: u64,
    /// Peak RSS seen across all samples
    peak_rss: u64,
    /// Number of samples taken
    sample_count: u64,
}

impl RssTracker {
    /// Create a new RSS tracker.
    pub fn new() -> Self {
        let mut tracker = Self::default();
        // Take initial sample
        tracker.sample();
        tracker
    }

    /// Sample current process RSS from the OS.
    ///
    /// This queries the operating system for the current resident set size.
    /// On Linux, this reads from `/proc/self/smaps` or `/proc/self/statm`.
    /// On macOS, this uses `mach_task_info`.
    pub fn sample(&mut self) {
        if let Some(stats) = memory_stats::memory_stats() {
            let rss = stats.physical_mem as u64;
            self.current_rss = rss;
            if rss > self.peak_rss {
                self.peak_rss = rss;
            }
            self.sample_count += 1;
        }
    }

    /// Get the current RSS (from last sample).
    pub fn current_rss(&self) -> u64 {
        self.current_rss
    }

    /// Get the peak RSS seen across all samples.
    pub fn peak_rss(&self) -> u64 {
        self.peak_rss
    }

    /// Get the number of samples taken.
    pub fn sample_count(&self) -> u64 {
        self.sample_count
    }

    /// Format peak RSS as human-readable string.
    pub fn peak_formatted(&self) -> String {
        format_bytes(self.peak_rss as usize)
    }

    /// Format current RSS as human-readable string.
    pub fn current_formatted(&self) -> String {
        format_bytes(self.current_rss as usize)
    }
}

/// Extended memory statistics including both throughput and actual RSS.
///
/// This provides both metrics so users can understand:
/// - `throughput_bytes`: Total bytes processed (useful for understanding data volume)
/// - `peak_rss_bytes`: Actual peak memory usage (what matters for machine sizing)
#[derive(Debug, Clone)]
pub struct ExtendedMemoryStats {
    /// Peak throughput estimate (from MemoryTracker) - cumulative bytes processed
    pub throughput_bytes: usize,
    /// Peak RSS (from RssTracker) - actual resident memory
    pub peak_rss_bytes: u64,
    /// Memory budget if set
    pub budget: Option<usize>,
    /// Number of times budget was exceeded
    pub budget_exceeded_count: usize,
    /// Number of RSS samples taken
    pub rss_sample_count: u64,
}

impl ExtendedMemoryStats {
    /// Create extended stats from both trackers.
    pub fn from_trackers(mem_tracker: &MemoryTracker, rss_tracker: &RssTracker) -> Self {
        Self {
            throughput_bytes: mem_tracker.peak(),
            peak_rss_bytes: rss_tracker.peak_rss(),
            budget: mem_tracker.budget(),
            budget_exceeded_count: mem_tracker.budget_exceeded_count(),
            rss_sample_count: rss_tracker.sample_count(),
        }
    }

    /// Format peak RSS as human-readable string.
    pub fn peak_rss_formatted(&self) -> String {
        format_bytes(self.peak_rss_bytes as usize)
    }

    /// Format throughput as human-readable string.
    pub fn throughput_formatted(&self) -> String {
        format_bytes(self.throughput_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{LineString, Point, Polygon};

    #[test]
    fn test_estimate_point_size() {
        let point = Geometry::Point(Point::new(0.0, 0.0));
        let size = estimate_geometry_size(&point);
        assert!(size > 0);
        assert!(size < 100); // Point should be small
    }

    #[test]
    fn test_estimate_polygon_size() {
        let exterior = LineString::from(vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]);
        let polygon = Geometry::Polygon(Polygon::new(exterior, vec![]));
        let size = estimate_geometry_size(&polygon);
        assert!(size > 0);
        // 4 coords * 16 bytes + overhead
        assert!(size >= 4 * 16);
    }

    #[test]
    fn test_memory_tracker_basic() {
        let mut tracker = MemoryTracker::new();
        tracker.add(1000);
        assert_eq!(tracker.current(), 1000);
        assert_eq!(tracker.peak(), 1000);

        tracker.add(500);
        assert_eq!(tracker.current(), 1500);
        assert_eq!(tracker.peak(), 1500);

        tracker.remove(1000);
        assert_eq!(tracker.current(), 500);
        assert_eq!(tracker.peak(), 1500); // Peak unchanged

        tracker.reset_current();
        assert_eq!(tracker.current(), 0);
        assert_eq!(tracker.peak(), 1500); // Peak still unchanged
    }

    #[test]
    fn test_memory_tracker_budget() {
        let mut tracker = MemoryTracker::with_budget(1000);
        tracker.add(500);
        assert!(!tracker.is_over_budget());

        tracker.add(600);
        assert!(tracker.is_over_budget());
    }

    #[test]
    fn test_format_bytes() {
        assert_eq!(format_bytes(500), "500 bytes");
        assert_eq!(format_bytes(1024), "1.00 KB");
        assert_eq!(format_bytes(1536), "1.50 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.00 GB");
        assert_eq!(format_bytes(4 * 1024 * 1024 * 1024), "4.00 GB");
    }

    // ============================================================
    // BUG VERIFICATION TESTS
    // ============================================================

    /// Demonstrates that MemoryTracker counts THROUGHPUT, not RESIDENT memory.
    ///
    /// When used with an external sorter that spills to disk, the tracker
    /// will add() for each record but never remove() when records are flushed.
    /// This causes peak to reflect cumulative bytes processed, not actual RAM.
    #[test]
    fn test_bug_tracker_counts_throughput_not_resident() {
        let mut tracker = MemoryTracker::with_budget(1000);

        // Simulate external sorter behavior:
        // - Add 500 bytes (record 1)
        // - Add 500 bytes (record 2)
        // - Sorter flushes to disk (but tracker has no remove() call!)
        // - Add 500 bytes (record 3)
        // - etc.

        // In the real pipeline, records are added but never removed
        // because the external sorter handles its own memory management

        tracker.add(500);
        tracker.add(500);
        // At this point, actual memory might be flushed to disk
        // But tracker still shows 1000 bytes

        assert_eq!(tracker.current(), 1000);
        assert_eq!(tracker.peak(), 1000);

        // More records come in, old ones are on disk
        tracker.add(500);
        tracker.add(500);

        // Tracker shows 2000 bytes, but actual RAM might be only 1000
        // (if sorter flushed the first batch)
        assert_eq!(tracker.current(), 2000);
        assert_eq!(tracker.peak(), 2000);

        // After reset_current(), peak is preserved
        tracker.reset_current();
        assert_eq!(tracker.current(), 0);
        assert_eq!(tracker.peak(), 2000); // Peak never decreases!

        println!("This demonstrates the bug:");
        println!("- Tracker peak: {} bytes", tracker.peak());
        println!("- Actual RAM could be much lower if sorter spilled to disk");
        println!("- Peak represents THROUGHPUT, not RESIDENT memory");
    }

    /// Shows that the pipeline's memory tracking pattern leads to overcounting.
    ///
    /// The pipeline does:
    /// 1. Phase 1: add(record_size) for EVERY record going to sorter
    /// 2. Sorter spills to disk (no remove() called)
    /// 3. reset_current() after Phase 1
    /// 4. Phase 3: add(geom_size) again when reading back
    ///
    /// This means records are counted TWICE in peak if Phase 3 adds exceed Phase 1.
    #[test]
    fn test_bug_pipeline_double_counting_pattern() {
        let mut tracker = MemoryTracker::new();

        // Phase 1: Add records to sorter
        for _ in 0..100 {
            tracker.add(100); // 100 bytes per record
        }
        let phase1_peak = tracker.peak();
        println!("After Phase 1: peak = {} bytes", phase1_peak);
        assert_eq!(phase1_peak, 10_000);

        // Reset current (but peak is preserved!)
        tracker.reset_current();
        assert_eq!(tracker.current(), 0);
        assert_eq!(tracker.peak(), 10_000);

        // Phase 3: Read records back and add again
        for _ in 0..100 {
            tracker.add(100);
        }

        // Peak is now max(phase1_peak, phase3_current)
        // If Phase 3 had more records, peak would be higher
        println!("After Phase 3: peak = {} bytes", tracker.peak());

        // The issue: If Phase 1 adds 97GB worth of throughput,
        // peak stays at 97GB even though:
        // 1. Records were written to disk
        // 2. reset_current() was called
        // 3. Actual RAM never exceeded buffer size

        println!("\nThis is why the pipeline reports 97GB 'peak memory'");
        println!("when actual RAM usage was ~40GB or less.");
    }

    // ============================================================
    // FIX VERIFICATION TESTS (for issue #152)
    // ============================================================

    /// Verifies that RssTracker samples actual process RSS.
    ///
    /// Key properties:
    /// 1. Returns a real RSS value (> 0)
    /// 2. Value is bounded by physical memory (can't exceed 64GB on 64GB machine)
    /// 3. Independent of add() calls - only reflects actual allocations
    #[test]
    fn test_rss_tracker_samples_actual_memory() {
        let mut tracker = RssTracker::new();

        // Sample initial RSS
        tracker.sample();
        let initial_rss = tracker.current_rss();

        // RSS should be > 0 (we're a running process)
        assert!(initial_rss > 0, "RSS should be positive");

        // RSS should be reasonable (less than 100GB - no machine has 100GB for a test process)
        const MAX_REASONABLE_RSS: u64 = 100 * 1024 * 1024 * 1024; // 100 GB
        assert!(
            initial_rss < MAX_REASONABLE_RSS,
            "RSS {} should be less than 100GB - it's sampling actual memory, not throughput",
            initial_rss
        );

        // Allocate some memory and sample again
        let _alloc: Vec<u8> = vec![0u8; 10 * 1024 * 1024]; // 10 MB
        tracker.sample();
        let after_alloc_rss = tracker.current_rss();

        // RSS should have increased (or stayed similar due to OS caching)
        // We just verify it's still reasonable
        assert!(
            after_alloc_rss < MAX_REASONABLE_RSS,
            "RSS after allocation should still be reasonable"
        );

        // Peak should be the max of samples
        let peak_rss = tracker.peak_rss();
        assert!(peak_rss >= initial_rss, "Peak RSS should be >= initial RSS");
        assert!(
            peak_rss >= after_alloc_rss || after_alloc_rss - peak_rss < 1024 * 1024,
            "Peak RSS should be >= current or very close"
        );
    }

    /// Verifies that RssTracker does NOT accumulate like MemoryTracker.
    ///
    /// This is the key difference: calling sample() 1000 times should not
    /// cause peak_rss to grow linearly - it should reflect actual memory.
    #[test]
    fn test_rss_tracker_does_not_accumulate() {
        let mut tracker = RssTracker::new();

        // Sample 1000 times (simulating the pipeline pattern)
        for _ in 0..1000 {
            tracker.sample();
        }

        let peak = tracker.peak_rss();

        // Peak should be reasonable (not 1000x initial memory)
        const MAX_REASONABLE_RSS: u64 = 100 * 1024 * 1024 * 1024; // 100 GB
        assert!(
            peak < MAX_REASONABLE_RSS,
            "Peak RSS {} should be reasonable even after 1000 samples",
            peak
        );

        // The key test: peak should be similar to current (not accumulated)
        let current = tracker.current_rss();
        let ratio = if current > 0 {
            peak as f64 / current as f64
        } else {
            1.0
        };

        assert!(
            ratio < 10.0,
            "Peak/current ratio {} should be small - not accumulating",
            ratio
        );
    }
}
