//! Ordered mapping with an owner that never steals work while waiting.
//!
//! Detached helpers may outlive the call, but only admitted batches may use
//! its borrowed closure. Closing admission and draining those batches is a
//! small manual scope; unlike a Rayon scope, it never joins queued jobs.

use std::any::Any;
use std::ops::Range;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

type Panic = Box<dyn Any + Send>;

/// An erased, shared borrow of a batch closure. Never dereference it without
/// an in-flight admission, and never let its closure die before closing and
/// draining admission. A dangling pointer in an unstarted job is harmless.
#[derive(Clone, Copy)]
struct Work {
    data: *const (),
    run: unsafe fn(*const (), Range<usize>),
}

// SAFETY: Work only represents &F with F: Sync. Access is further bounded by
// Shared's admission protocol and OwnerGuard; moving a pointer never reads F.
unsafe impl Send for Work {}
// SAFETY: Admitted batches only obtain shared access to that Sync closure.
unsafe impl Sync for Work {}

impl Work {
    /// # Safety
    ///
    /// The caller must keep `f` alive until admission is closed and drained.
    unsafe fn new<F: Fn(Range<usize>) + Sync>(f: &F) -> Self {
        unsafe fn run<F: Fn(Range<usize>) + Sync>(data: *const (), range: Range<usize>) {
            // SAFETY: Work::new records this exact F; the admission keeps its
            // shared borrow valid until this invocation returns or unwinds.
            let f = unsafe { &*data.cast::<F>() };
            f(range);
        }
        Self {
            data: std::ptr::from_ref(f).cast(),
            run: run::<F>,
        }
    }
}

#[derive(Default)]
struct State {
    /// Start of the next unclaimed batch.
    next: usize,
    closed: bool,
    in_flight: usize,
    panic: Option<Panic>,
}

struct Shared {
    len: usize,
    batch_size: usize,
    work: Work,
    state: Mutex<State>,
    done: Condvar,
}

impl Shared {
    fn claim(&self) -> Option<Batch<'_>> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return None;
        }
        let start = state.next;
        if start == self.len {
            return None;
        }
        let end = start.saturating_add(self.batch_size).min(self.len);
        state.next = end;
        // Closing and admission use the same lock: no helper can obtain a
        // borrowed pointer without being counted before the owner closes.
        state.in_flight += 1;
        Some(Batch {
            shared: self,
            range: start..end,
        })
    }

    fn run(&self) {
        while let Some(batch) = self.claim() {
            let result = catch_unwind(AssertUnwindSafe(|| {
                // SAFETY: batch is an admission guard. OwnerGuard keeps the
                // closure alive until every such guard has been dropped.
                unsafe { (self.work.run)(self.work.data, batch.range.clone()) };
            }));
            if let Err(panic) = result {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                state.closed = true;
                if state.panic.is_none() {
                    state.panic = Some(panic);
                }
            }
            // Release the admission before trying to claim another batch.
            drop(batch);
        }
    }

    fn close_and_wait(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.closed = true;
        while state.in_flight != 0 {
            // Deliberately block the worker; do not enter Rayon's scheduler.
            state = self
                .done
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

struct Batch<'a> {
    shared: &'a Shared,
    range: Range<usize>,
}

impl Drop for Batch<'_> {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.in_flight -= 1;
        if state.in_flight == 0 {
            self.shared.done.notify_all();
        }
    }
}

struct OwnerGuard<'a>(&'a Shared);

impl Drop for OwnerGuard<'_> {
    fn drop(&mut self) {
        self.0.close_and_wait();
    }
}

/// Like a serial map in input order, sharing short batches with the current
/// pool. The owner works too, then blocks only for already-admitted batches.
pub(crate) fn owner_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = rayon::current_num_threads();
    if items.len() <= 1 || threads == 1 {
        return items.iter().map(f).collect();
    }
    // At least four batches per worker for large inputs; cheap quantization
    // items are grouped by count rather than paying for per-item scheduling.
    let batch_size = (items.len() / threads.saturating_mul(4)).max(1);
    let batch_count = items.len().div_ceil(batch_size);
    let batches: Vec<_> = (0..batch_count).map(|_| Mutex::new(None)).collect();
    let work = |range: Range<usize>| {
        let index = range.start / batch_size;
        let values: Vec<R> = items[range].iter().map(&f).collect();
        *batches[index]
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(values);
    };
    let shared = Arc::new(Shared {
        len: items.len(),
        batch_size,
        // SAFETY: No jobs have been offered yet. The guard below is created
        // before spawning and dropped before work, batches, f, or items can
        // die, on both normal return and unwind. It closes admission and
        // drains in-flight accesses; late helpers only inspect owned state.
        work: unsafe { Work::new(&work) },
        state: Mutex::new(State::default()),
        done: Condvar::new(),
    });
    let owner = OwnerGuard(&shared);
    for _ in 0..(threads - 1).min(batch_count - 1) {
        let shared = Arc::clone(&shared);
        // On a worker this targets its current (possibly custom) pool;
        // outside a pool it uses the global pool. No scope-end join.
        rayon::spawn(move || shared.run());
    }
    shared.run();
    drop(owner);
    let panic = shared
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .panic
        .take();
    if let Some(panic) = panic {
        resume_unwind(panic);
    }
    let mut result = Vec::with_capacity(items.len());
    for batch in batches {
        result.extend(
            batch
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner)
                .expect("every batch completed"),
        );
    }
    result
}

#[cfg(test)]
#[expect(
    clippy::disallowed_types,
    reason = "#876: per-test thread coordination, not global diagnostic state"
)]
mod tests {
    use super::owner_map;
    use std::cell::Cell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::sync::{mpsc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    fn pool(threads: usize) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
    }

    #[test]
    fn equals_serial_for_requested_sizes_outside_a_pool() {
        assert!(rayon::current_thread_index().is_none());
        for size in [0, 1, 7, 10_000] {
            let items: Vec<_> = (0..size).collect();
            assert_eq!(
                owner_map(&items, |n| n * 3 + 1),
                items.iter().map(|n| n * 3 + 1).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn order_preserved() {
        pool(4).install(|| {
            let items: Vec<_> = (0..1_000).collect();
            let result = owner_map(&items, |n| {
                if *n == 0 {
                    thread::sleep(Duration::from_millis(10));
                }
                format!("item {n}")
            });
            assert_eq!(
                result,
                items
                    .iter()
                    .map(|n| format!("item {n}"))
                    .collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn borrowed_inputs_closure_and_non_sync_results() {
        let items = [String::from("a"), String::from("bb")];
        let suffix = String::from("!");
        let result = owner_map(&items, |s| (suffix.as_str(), Cell::new(s.len())));
        assert_eq!(result[0].0, "!");
        assert_eq!(result[1].1.get(), 2);
    }

    #[test]
    fn custom_pool_nested_two_deep() {
        let pool = pool(4);
        pool.install(|| {
            let items: Vec<_> = (0..31).collect();
            let result = owner_map(&items, |a| {
                owner_map(&items, |b| {
                    // Helpers run on this pool, not the global one.
                    assert!(pool.current_thread_index().is_some());
                    a * 100 + b
                })
            });
            let serial: Vec<Vec<_>> = items
                .iter()
                .map(|a| items.iter().map(|b| a * 100 + b).collect())
                .collect();
            assert_eq!(result, serial);
        });
    }

    #[test]
    fn single_worker_pool() {
        pool(1).install(|| assert_eq!(owner_map(&[1, 2, 3], |n| n * 2), [2, 4, 6]));
    }

    // The owner's first body waits until a helper has entered a body. This
    // makes both the sleep and panic tests exercise an actual helper batch.
    fn with_running_helper(panic_on_owner: bool, panic_on_helper: bool) {
        pool(2).install(|| {
            let owner = thread::current().id();
            let (started_tx, started_rx) = mpsc::channel();
            let started_rx = Mutex::new(Some(started_rx));
            let (ack_tx, ack_rx) = mpsc::channel();
            let ack_rx = Mutex::new(ack_rx);
            let helper_calls = AtomicUsize::new(0);
            let helper_finished = AtomicUsize::new(0);
            let items: Vec<_> = (0..128).collect();
            let start = Instant::now();
            let result = catch_unwind(AssertUnwindSafe(|| {
                owner_map(&items, |n| {
                    if thread::current().id() == owner {
                        let rx = started_rx.lock().unwrap().take();
                        if let Some(rx) = rx {
                            rx.recv_timeout(Duration::from_secs(5))
                                .expect("helper must start");
                            ack_tx.send(()).unwrap();
                            if panic_on_owner {
                                std::panic::panic_any("owner panic");
                            }
                        }
                    } else if helper_calls.fetch_add(1, Relaxed) == 0 {
                        started_tx.send(()).unwrap();
                        ack_rx
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap();
                        thread::sleep(Duration::from_millis(200));
                        helper_finished.store(1, Relaxed);
                        if panic_on_helper {
                            std::panic::panic_any("helper panic");
                        }
                    }
                    n * 2
                })
            }));
            assert_eq!(
                helper_finished.load(Relaxed),
                1,
                "wait before returning or unwinding"
            );
            assert!(start.elapsed() >= Duration::from_millis(200));
            if panic_on_owner || panic_on_helper {
                let payload = result.unwrap_err();
                assert_eq!(
                    payload.downcast_ref::<&str>(),
                    Some(&if panic_on_owner {
                        "owner panic"
                    } else {
                        "helper panic"
                    })
                );
            } else {
                assert_eq!(
                    result.unwrap(),
                    items.iter().map(|n| n * 2).collect::<Vec<_>>()
                );
            }
        });
    }

    #[test]
    fn waits_for_a_running_helper() {
        with_running_helper(false, false);
    }

    #[test]
    fn helper_panic_reaches_caller() {
        with_running_helper(false, true);
    }

    #[test]
    fn caller_panic_waits_for_running_helper() {
        with_running_helper(true, false);
    }

    #[test]
    fn queued_helper_does_not_hold_caller() {
        let pool = pool(2);
        let (blocked_tx, blocked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        pool.spawn(move || {
            blocked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        blocked_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (returned_tx, returned_rx) = mpsc::channel();
        pool.spawn(move || {
            let items = vec![String::from("borrowed"); 128];
            let result = owner_map(&items, String::len);
            assert_eq!(result, vec![8; 128]);
            // The borrowed items and closure are gone before queued jobs run.
            returned_tx.send(()).unwrap();
        });
        let returned = returned_rx.recv_timeout(Duration::from_secs(5));
        release_tx.send(()).unwrap();
        assert!(returned.is_ok(), "caller must not join queued helpers");
        // Drain queued helpers before this test's pool is destroyed.
        pool.broadcast(|_| ());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]
        #[test]
        fn equals_serial_map(threads in 1usize..9, len in 0usize..3000, salt: u64) {
            let items: Vec<u64> = (0..len as u64).map(|n| n ^ salt).collect();
            let f = |n: &u64| n.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(7);
            let pool = pool(threads);
            let got = pool.install(|| owner_map(&items, f));
            proptest::prop_assert_eq!(got, items.iter().map(f).collect::<Vec<_>>());
        }
    }

    type Map = fn(&[usize], &(dyn Fn(&usize) -> usize + Sync)) -> Vec<usize>;

    /// Runs `map` from a worker of a 4-thread pool. Its first item queues an
    /// unrelated job that blocks until this function releases it. With
    /// `gate_others`, the other three workers are blocked first, so the
    /// caller is the only thread that could run that job. True when `map`
    /// returns while the job is held; false when the caller ran the job
    /// inside the map, or the map did not return within the timeout.
    fn returns_while_unrelated_job_held(map: Map, gate_others: bool) -> bool {
        enum Event {
            Done(Vec<usize>),
            Unrelated { on_caller: bool },
        }
        let pool = pool(4);
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let gate_rx = std::sync::Arc::new(Mutex::new(gate_rx));
        let (gated_tx, gated_rx) = mpsc::channel();
        if gate_others {
            for _ in 0..3 {
                let (gate_rx, gated_tx) = (std::sync::Arc::clone(&gate_rx), gated_tx.clone());
                pool.spawn(move || {
                    gated_tx.send(()).unwrap();
                    // Hold this worker until gate_tx drops.
                    let _ = gate_rx.lock().unwrap().recv();
                });
                gated_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            }
        }
        let (event_tx, event_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let unrelated = Mutex::new(Some((event_tx.clone(), release_rx)));
        pool.spawn(move || {
            let caller = thread::current().id();
            let items: Vec<usize> = (0..1_000).collect();
            let result = map(&items, &|n| {
                if let Some((tx, rx)) = unrelated.lock().unwrap().take() {
                    rayon::spawn(move || {
                        let on_caller = thread::current().id() == caller;
                        let _ = tx.send(Event::Unrelated { on_caller });
                        let _ = rx.recv();
                    });
                }
                n * 2
            });
            let _ = event_tx.send(Event::Done(result));
        });
        let returned = loop {
            match event_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Event::Done(result)) => {
                    assert_eq!(result, (0..1_000).map(|n| n * 2).collect::<Vec<_>>());
                    break true;
                }
                Ok(Event::Unrelated { on_caller: true }) | Err(_) => break false,
                Ok(Event::Unrelated { on_caller: false }) => {}
            }
        };
        drop((release_tx, gate_tx));
        returned
    }

    // #876: a nested par_iter caller that waits runs whatever rayon hands
    // it, so one long unrelated job holds the whole map. The control shows
    // the setup does trap par_iter.
    #[test]
    fn caller_never_runs_an_unrelated_job_inside_the_map() {
        use rayon::prelude::*;
        let par_map: Map = |items, f| items.par_iter().map(f).collect();
        assert!(
            !returns_while_unrelated_job_held(par_map, true),
            "control: par_iter's caller runs the queued job before returning"
        );
        let owner: Map = |items, f| owner_map(items, f);
        assert!(returns_while_unrelated_job_held(owner, true));
        assert!(returns_while_unrelated_job_held(owner, false));
    }
}
