//! Shared fixture-absence policy for the criterion benches (#448).
//!
//! Included by each bench with `#[path = "support/fixtures.rs"] mod fixtures;`
//! (a subdirectory without `main.rs` is not auto-discovered as a bench target).

/// Report that a real-data fixture is missing.
///
/// Locally this prints and lets the caller skip that bench, so a fresh clone
/// can still run the synthetic ones. With `TYLERTOO_BENCH_REQUIRE_FIXTURES=1`
/// (set by CI's regression job) it panics instead: a bench that silently
/// skips produces no `new/` estimate, and the regression gate would then have
/// nothing to compare for it.
pub fn missing(bench: &str, what: &str) {
    if std::env::var("TYLERTOO_BENCH_REQUIRE_FIXTURES").as_deref() == Ok("1") {
        panic!(
            "{bench}: fixture {what} not found and TYLERTOO_BENCH_REQUIRE_FIXTURES=1 \
             (fetch with `gh release download fixtures-v1 --dir tests/fixtures/realdata/ --clobber`)"
        );
    }
    eprintln!("{bench}: fixture {what} not found, skipping");
}
