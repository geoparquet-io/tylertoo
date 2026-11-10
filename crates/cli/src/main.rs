//! The `tylertoo` binary: the process-level setup (allocator, heap profiler)
//! around [`tylertoo_cli::run`], which is the whole CLI.

/// Global allocator for static-musl builds (#480).
///
/// musl's `mallocng` deterministically fails *small* allocations once a large
/// live heap has been fragmented by many threads: a 38.7M-polygon `tiles` run
/// aborted with `memory allocation of 148448 bytes failed` at the identical
/// input row across two runs, at 28.6 GB RSS on a node with 240 GB granted,
/// over 200 GB of headroom. mimalloc's segment/page allocator does not degrade
/// that way, so the musl release binary uses it instead.
///
/// On musl it is unconditional. On every other target it is the `mimalloc`
/// Cargo feature, on by default (#739): against glibc malloc, export took
/// 0.68-0.79x the wall time and total CPU 0.83x on the #788 benchmark lane,
/// with peak RSS from +30% to -23% by dataset. `--no-default-features` builds
/// on the platform allocator instead.
/// The Python extension sets its own (`crates/python/src/lib.rs`): this one
/// lives in the binary, so the library [`tylertoo_cli::run`] adds none.
///
/// Excluded under `dhat-heap`: dhat installs its own `#[global_allocator]`
/// in tylertoo-core (it must own the allocator to count anything), and rustc
/// allows only one in the crate graph. Heap-profiling a musl binary therefore
/// runs on dhat's allocator and loses this fix for the duration.
#[cfg(all(
    any(target_env = "musl", feature = "mimalloc"),
    not(feature = "dhat-heap")
))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// #828: return this thread's freed mimalloc pages to the OS now, rather
/// than after the purge delay; core runs it when a phase ends.
#[cfg(all(
    any(target_env = "musl", feature = "mimalloc"),
    not(feature = "dhat-heap")
))]
fn release_retained() {
    // SAFETY: mi_collect takes no pointers and only touches mimalloc's state.
    unsafe { libmimalloc_sys::mi_collect(true) }
}

/// #828: no transparent huge pages for mimalloc's arenas. On a `madvise`
/// THP host they kept 0.5-1 GiB of freed 4 KiB pages resident inside
/// 2 MiB pages (Moldova z12: 3.7 -> 3.0 GiB peak, same archive). Then
/// install the phase-boundary release.
#[cfg(all(
    any(target_env = "musl", feature = "mimalloc"),
    not(feature = "dhat-heap")
))]
fn configure_allocator() {
    use tylertoo_core::internal::alloc_release::mimalloc_allow_thp_option;
    // SAFETY: mi_version and mi_option_set take no pointers.
    unsafe {
        if let Some(option) = mimalloc_allow_thp_option(libmimalloc_sys::mi_version()) {
            libmimalloc_sys::mi_option_set(option, 0);
        }
    }
    tylertoo_core::internal::alloc_release::set_release_hook(release_retained);
}

fn main() -> std::process::ExitCode {
    // Initialize dhat profiler if feature is enabled
    // This must be at the very start of main() - the profiler outputs
    // dhat-heap.json on Drop (program exit). TYLERTOO_DHAT=off skips it:
    // CI's `--all-features` test runs compile this in, and the CLI snapshot
    // tests (tests/cli_snapshots.rs) pin stderr, which dhat's exit summary
    // would change.
    #[cfg(feature = "dhat-heap")]
    let _profiler = (std::env::var_os("TYLERTOO_DHAT").as_deref()
        != Some(std::ffi::OsStr::new("off")))
    .then(dhat::Profiler::new_heap);

    #[cfg(all(
        any(target_env = "musl", feature = "mimalloc"),
        not(feature = "dhat-heap")
    ))]
    configure_allocator();

    std::process::ExitCode::from(tylertoo_cli::run(std::env::args_os()))
}

/// #739: the gnu CLI links mimalloc by default. Checked at build time
/// against the manifest's default feature list, and, in any build with the
/// feature on and dhat off, against the type of the installed allocator.
#[cfg(test)]
mod allocator_default {
    #[cfg(all(feature = "mimalloc", not(feature = "dhat-heap")))]
    const _: &mimalloc::MiMalloc = &super::GLOBAL;

    /// #828: the linked mimalloc is one whose THP option index is known,
    /// so the THP-off setting is not skipped.
    #[cfg(all(feature = "mimalloc", not(feature = "dhat-heap")))]
    #[test]
    fn bundled_mimalloc_has_a_known_thp_option() {
        // SAFETY: mi_version takes no arguments.
        let version = unsafe { libmimalloc_sys::mi_version() };
        assert_eq!(
            tylertoo_core::internal::alloc_release::mimalloc_allow_thp_option(version),
            Some(43),
            "mimalloc {version}"
        );
    }

    #[test]
    fn mimalloc_is_a_default_feature() {
        let manifest = include_str!("../Cargo.toml");
        let default = manifest
            .lines()
            .find(|l| l.trim_start().starts_with("default ="))
            .expect("crates/cli/Cargo.toml declares default features");
        assert!(
            default.contains("\"mimalloc\""),
            "mimalloc must stay a default feature of the CLI: {default}"
        );
    }
}
