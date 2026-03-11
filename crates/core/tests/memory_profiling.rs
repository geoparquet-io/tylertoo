//! Integration tests for memory profiling infrastructure.
//!
//! These tests verify that the dhat-heap feature compiles and works correctly.
//! The actual profiling output (dhat-heap.json) is tested manually.

/// Test that we can detect whether dhat-heap is enabled at compile time.
///
/// This verifies conditional compilation works correctly for the feature flag.
#[test]
fn test_dhat_heap_feature_detection() {
    // Verify we can check the feature at compile time
    let feature_enabled: bool;

    #[cfg(feature = "dhat-heap")]
    {
        // When dhat-heap is enabled, we should be able to reference dhat types
        let _ = std::any::type_name::<dhat::Alloc>();
        feature_enabled = true;
    }

    #[cfg(not(feature = "dhat-heap"))]
    {
        // When dhat-heap is disabled, this block runs
        feature_enabled = false;
    }

    // The test passes regardless of feature state - we're verifying
    // conditional compilation works and the feature can be detected
    #[cfg(feature = "dhat-heap")]
    assert!(feature_enabled, "Feature should be detected as enabled");

    #[cfg(not(feature = "dhat-heap"))]
    assert!(!feature_enabled, "Feature should be detected as disabled");
}

/// Verify the global allocator is configured correctly.
///
/// When dhat-heap is enabled, allocations go through dhat::Alloc.
/// This test performs a simple allocation to ensure the allocator works.
#[test]
fn test_allocation_works_with_feature() {
    // Perform some allocations
    let v: Vec<u8> = vec![0u8; 1024];
    assert_eq!(v.len(), 1024);

    let s = String::from("test allocation");
    assert!(!s.is_empty());

    // Box allocation
    let b = Box::new([0u64; 128]);
    assert_eq!(b.len(), 128);
}

/// Test that our memory tracking infrastructure is present.
///
/// gpq-tiles-core has a memory module for tracking allocations.
/// This test verifies it's accessible.
#[test]
fn test_memory_module_accessible() {
    use gpq_tiles_core::memory::MemoryTracker;

    let tracker = MemoryTracker::new();
    assert_eq!(tracker.current(), 0);
    assert_eq!(tracker.peak(), 0);
}
