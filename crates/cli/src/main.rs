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
    tylertoo_core::internal::alloc_release::set_release_hook(release_retained);

    std::process::ExitCode::from(tylertoo_cli::run(std::env::args_os()))
}

/// #739: the gnu CLI links mimalloc by default. Checked at build time
/// against the manifest's default feature list, and, in any build with the
/// feature on and dhat off, against the type of the installed allocator.
#[cfg(test)]
mod allocator_default {
    #[cfg(all(feature = "mimalloc", not(feature = "dhat-heap")))]
    const _: &mimalloc::MiMalloc = &super::GLOBAL;

    /// #828: every libmimalloc-sys declaration builds without THP madvise.
    #[test]
    fn mimalloc_is_built_without_thp() {
        let manifest = include_str!("../Cargo.toml");
        let decls: Vec<&str> = manifest
            .lines()
            .filter(|l| l.starts_with("libmimalloc-sys"))
            .collect();
        assert_eq!(decls.len(), 2, "the mimalloc and musl declarations");
        assert!(decls.iter().all(|l| l.contains("\"no_thp\"")), "{decls:?}");
    }
}
