use tylertoo_core::MemoryProfile;

/// Apply the frontend allocator policy for the lifetime of the returned guard.
///
/// Bounded trades throughput for immediate page purging while any Bounded
/// guard is alive; the last one dropped restores the prior setting, so a
/// later Auto or Speed Python call runs at full speed. Overlapping calls on
/// different threads share the setting while any Bounded call runs. Builds
/// using the system allocator or dhat do nothing.
#[must_use = "the purge setting is restored when the guard drops"]
pub fn apply_memory_profile_allocator(profile: MemoryProfile) -> AllocatorGuard {
    let active = purges_immediately(profile);
    if active {
        lock().enter(backend::get, backend::set);
    }
    AllocatorGuard { active }
}

/// Holds the Bounded purge setting; restores it on drop, even on panic.
pub struct AllocatorGuard {
    active: bool,
}

impl Drop for AllocatorGuard {
    fn drop(&mut self) {
        if self.active {
            lock().exit(backend::set);
        }
    }
}

/// The policy decision, compiled in every configuration so `--all-features`
/// (dhat) CI still tests it.
fn purges_immediately(profile: MemoryProfile) -> bool {
    profile == MemoryProfile::Bounded
}

/// Count of live Bounded guards and the setting to restore after the last.
struct Scope {
    depth: usize,
    saved: i64,
}

impl Scope {
    /// First guard in: remember the current delay and set it to 0.
    fn enter(&mut self, get: impl FnOnce() -> i64, set: impl FnOnce(i64)) {
        if self.depth == 0 {
            self.saved = get();
            set(0);
        }
        self.depth += 1;
    }

    /// Last guard out: restore the remembered delay.
    fn exit(&mut self, set: impl FnOnce(i64)) {
        self.depth -= 1;
        if self.depth == 0 {
            set(self.saved);
        }
    }
}

fn lock() -> std::sync::MutexGuard<'static, Scope> {
    static SCOPE: std::sync::Mutex<Scope> = std::sync::Mutex::new(Scope { depth: 0, saved: 0 });
    // The state stays consistent across a panic, so a poisoned lock is usable.
    SCOPE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(all(
    any(target_env = "musl", feature = "mimalloc"),
    not(feature = "dhat-heap")
))]
mod backend {
    // Retain mimalloc's native link edge in library consumers too.
    use mimalloc as _;
    extern "C" {
        fn mi_option_get(option: std::ffi::c_int) -> std::ffi::c_long;
        fn mi_option_set(option: std::ffi::c_int, value: std::ffi::c_long);
    }
    // libmimalloc-sys 0.1.49, c_src/mimalloc/{v2,v3}/include/mimalloc.h:
    // mi_option_purge_delay = 15. Local bindings avoid `extended`'s cty dep.
    const PURGE_DELAY: std::ffi::c_int = 15;

    pub(super) fn get() -> i64 {
        // SAFETY: valid option id; no pointers.
        i64::from(unsafe { mi_option_get(PURGE_DELAY) })
    }
    pub(super) fn set(value: i64) {
        // SAFETY: valid option id and delay in milliseconds; no pointers.
        unsafe { mi_option_set(PURGE_DELAY, value as std::ffi::c_long) };
    }
}

#[cfg(not(all(
    any(target_env = "musl", feature = "mimalloc"),
    not(feature = "dhat-heap")
)))]
mod backend {
    pub(super) fn get() -> i64 {
        0
    }
    pub(super) fn set(_: i64) {}
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn only_bounded_purges_immediately() {
        assert!(purges_immediately(MemoryProfile::Bounded));
        assert!(!purges_immediately(MemoryProfile::Auto));
        assert!(!purges_immediately(MemoryProfile::Speed));
    }

    #[test]
    fn overlapping_scopes_restore_after_the_last_exit() {
        let delay = Cell::new(1000);
        let mut scope = Scope { depth: 0, saved: 0 };
        scope.enter(|| delay.get(), |v| delay.set(v));
        assert_eq!(delay.get(), 0);
        // A nested or overlapping entry must not save the already-zero value.
        scope.enter(|| delay.get(), |v| delay.set(v));
        scope.exit(|v| delay.set(v));
        assert_eq!(delay.get(), 0, "one guard is still alive");
        scope.exit(|v| delay.set(v));
        assert_eq!(delay.get(), 1000);
        // Reusable after a full cycle.
        scope.enter(|| delay.get(), |v| delay.set(v));
        scope.exit(|v| delay.set(v));
        assert_eq!(delay.get(), 1000);
    }
}

#[cfg(all(
    test,
    any(target_env = "musl", feature = "mimalloc"),
    not(feature = "dhat-heap")
))]
mod tests {
    use super::*;

    #[test]
    fn bounded_guard_purges_then_restores_the_real_option() {
        // One test owns this process-global option.
        let before = backend::get();
        // Set the bundled default explicitly so an environment override cannot
        // mask a missing Bounded write (e.g. MIMALLOC_PURGE_DELAY=0).
        backend::set(1000);
        drop(apply_memory_profile_allocator(MemoryProfile::Auto));
        assert_eq!(backend::get(), 1000);
        {
            let _outer = apply_memory_profile_allocator(MemoryProfile::Bounded);
            assert_eq!(backend::get(), 0);
            drop(apply_memory_profile_allocator(MemoryProfile::Bounded));
            assert_eq!(backend::get(), 0, "outer guard still alive");
            drop(apply_memory_profile_allocator(MemoryProfile::Speed));
            assert_eq!(backend::get(), 0, "Speed leaves a live Bounded alone");
        }
        assert_eq!(backend::get(), 1000);
        backend::set(before);
    }
}
