//! Count tile gzip calls in a private pool, isolated from concurrent tests.
use std::cell::Cell;

thread_local! {
    static CALLS: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn note() {
    CALLS.with(|calls| calls.set(calls.get() + 1));
}

pub(crate) fn measure<T: Send>(f: impl FnOnce() -> T + Send) -> (T, usize) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    let value = pool.install(f);
    let count = pool.broadcast(|_| CALLS.with(Cell::get)).into_iter().sum();
    (value, count)
}
