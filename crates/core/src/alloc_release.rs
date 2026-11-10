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

/// `mi_option_allow_thp`'s index in mimalloc v3's `mi_option_e`
/// (mimalloc.h). libmimalloc-sys exports no constant for it, and v2 has it
/// at 37, so it is only trusted for the v3 line.
const MI3_OPTION_ALLOW_THP: i32 = 43;

/// A `mi_version()` whose THP option was left alone, for one debug line
/// once logging is up (the binary configures its allocator before that).
static THP_SKIPPED: std::sync::Mutex<Option<i32>> = std::sync::Mutex::new(None);

/// The `mi_option_t` that turns off transparent huge pages, if known.
///
/// `version` is `mi_version()` (major * 10000 + minor * 100 + patch).
/// `None` when the index is not known for that version: a wrong index
/// would silently set some other option.
pub fn mimalloc_allow_thp_option(version: i32) -> Option<i32> {
    let known = version / 10_000 == 3;
    if !known {
        *THP_SKIPPED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(version);
    }
    known.then_some(MI3_OPTION_ALLOW_THP)
}

/// Run the release hook on every rayon pool thread and on this one.
pub(crate) fn release_retained() {
    let skipped = THP_SKIPPED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(version) = skipped {
        log::debug!("mimalloc {version} is not v3: transparent huge pages left as configured");
    }
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
    fn thp_option_only_for_mimalloc_3() {
        assert_eq!(super::mimalloc_allow_thp_option(30302), Some(43));
        assert_eq!(super::mimalloc_allow_thp_option(30000), Some(43));
        assert_eq!(super::mimalloc_allow_thp_option(20302), None);
        assert_eq!(super::mimalloc_allow_thp_option(40000), None);
    }

    #[test]
    fn release_runs_on_every_pool_thread_and_the_caller() {
        super::set_release_hook(count);
        let before = CALLS.load(Ordering::Relaxed);
        super::release_retained();
        assert!(CALLS.load(Ordering::Relaxed) - before > rayon::current_num_threads());
    }
}
