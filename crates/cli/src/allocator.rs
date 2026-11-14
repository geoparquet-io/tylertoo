use tylertoo_core::MemoryProfile;

/// Apply the frontend allocator policy before starting pipeline work.
///
/// Bounded trades throughput for immediate page purging. Auto and Speed leave
/// the process-global setting unchanged, including after a Bounded Python call.
/// Builds using the system allocator or dhat do nothing.
pub fn apply_memory_profile_allocator(profile: MemoryProfile) {
    let purge = purges_immediately(profile);
    #[cfg(all(
        any(target_env = "musl", feature = "mimalloc"),
        not(feature = "dhat-heap")
    ))]
    if purge {
        // Retain mimalloc's native link edge in library consumers too.
        use mimalloc as _;
        extern "C" {
            fn mi_option_set(option: std::ffi::c_int, value: std::ffi::c_long);
        }
        // libmimalloc-sys 0.1.49, c_src/mimalloc/{v2,v3}/include/mimalloc.h:
        // mi_option_purge_delay = 15. Local bindings avoid `extended`'s cty dep.
        // SAFETY: valid option id and delay in milliseconds; no pointers.
        unsafe { mi_option_set(15, 0) };
    }
    let _ = purge;
}

/// The policy decision, compiled in every configuration so `--all-features`
/// (dhat) CI still tests it.
fn purges_immediately(profile: MemoryProfile) -> bool {
    profile == MemoryProfile::Bounded
}

#[cfg(test)]
mod policy_tests {
    use super::*;

    #[test]
    fn only_bounded_purges_immediately() {
        assert!(purges_immediately(MemoryProfile::Bounded));
        assert!(!purges_immediately(MemoryProfile::Auto));
        assert!(!purges_immediately(MemoryProfile::Speed));
    }
}

#[cfg(all(
    test,
    any(target_env = "musl", feature = "mimalloc"),
    not(feature = "dhat-heap")
))]
mod tests {
    use super::*;
    // Keep the native library linked even though the test harness uses the
    // system allocator. No extended bindings or additional dependency needed.
    use mimalloc as _;
    use std::ffi::{c_int, c_long};

    // libmimalloc-sys 0.1.49, c_src/mimalloc/{v2,v3}/include/mimalloc.h.
    const MI_OPTION_PURGE_DELAY: c_int = 15;
    extern "C" {
        fn mi_option_get(option: c_int) -> c_long;
        fn mi_option_set(option: c_int, value: c_long);
    }

    #[test]
    fn memory_profile_allocator_only_bounded_purges_immediately() {
        // One test owns this process-global option. Restore it even on panic.
        struct Restore(c_long);
        impl Drop for Restore {
            fn drop(&mut self) {
                // SAFETY: valid option id and a previously read C long value.
                unsafe { mi_option_set(MI_OPTION_PURGE_DELAY, self.0) };
            }
        }
        // SAFETY: both functions take the valid purge-delay option id.
        let _restore = Restore(unsafe { mi_option_get(MI_OPTION_PURGE_DELAY) });
        // Set the bundled default explicitly so an environment override cannot
        // mask a missing Bounded write (e.g. MIMALLOC_PURGE_DELAY=0).
        unsafe { mi_option_set(MI_OPTION_PURGE_DELAY, 1000) };
        for (profile, expected) in [
            (MemoryProfile::Auto, 1000),
            (MemoryProfile::Speed, 1000),
            (MemoryProfile::Bounded, 0),
            (MemoryProfile::Auto, 0),
            (MemoryProfile::Speed, 0),
        ] {
            apply_memory_profile_allocator(profile);
            // SAFETY: valid purge-delay option id; no pointers involved.
            assert_eq!(unsafe { mi_option_get(MI_OPTION_PURGE_DELAY) }, expected);
        }
    }
}
