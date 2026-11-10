//! Return freed allocator pages to the OS at phase boundaries (#828).
//!
//! mimalloc keeps the pages a phase freed in each thread's heap until its
//! purge delay passes, so a 64-thread run held ~2 GB more than its live data.
//! The allocator belongs to the binary (CLI, Python wheel), which installs a
//! per-thread release; core calls it on every pool thread when a phase ends.

#[expect(
    clippy::disallowed_types,
    reason = "#828: the process allocator's release hook, set once by the binary"
)]
static HOOK: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Install the binary's per-thread release, such as `mi_collect(true)`.
/// The first call wins.
pub fn set_release_hook(hook: fn()) {
    let _ = HOOK.set(hook);
}

/// Run the release hook on every rayon pool thread and on this one.
pub(crate) fn release_retained() {
    if let Some(&hook) = HOOK.get() {
        rayon::broadcast(|_| hook());
        hook();
    }
}

#[cfg(test)]
#[expect(
    clippy::disallowed_types,
    reason = "#828: the test counts hook calls across pool threads"
)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static CALLS: AtomicUsize = AtomicUsize::new(0);

    fn count() {
        CALLS.fetch_add(1, Ordering::Relaxed);
    }

    #[test]
    fn release_runs_on_every_pool_thread_and_the_caller() {
        super::set_release_hook(count);
        let before = CALLS.load(Ordering::Relaxed);
        super::release_retained();
        assert!(CALLS.load(Ordering::Relaxed) - before > rayon::current_num_threads());
    }
}
