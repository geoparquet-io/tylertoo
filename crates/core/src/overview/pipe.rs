//! Scoped producer→consumer plumbing for the streaming engines (#362).
//!
//! Three places stream record batches from a dedicated producer thread into a
//! consumer running on the calling thread, over a bounded channel:
//! [`super::stream::write_level_streaming`] (finest-level read+process →
//! parquet writer), [`super::pipeline`] (reader → per-level fan-out), and
//! [`super::export`]'s single-read fill (band reader → member store). All
//! three share one liveness contract that is easy to state and was easy to get
//! wrong:
//!
//! > **The receiver must be dropped before the producer is joined.**
//!
//! A bounded `Sender::send` unblocks on disconnect only when *every* receiver
//! has been **dropped** — a consumer that merely stops receiving (because it
//! errored out) leaves the producer parked forever, and the join that follows
//! never returns. That is issue #362: the writer's error was held to be
//! reported after the join, so a diagnosable failure presented as a silent
//! indefinite hang, and only on inputs large enough for the producer to still
//! have `depth` batches in hand when the consumer gave up.
//!
//! [`scoped_pipe`] encodes the contract in ownership rather than in a comment:
//! the consumer is *handed* the `Receiver`, so it is dropped when the consumer
//! returns, whatever path it returns by. Forgetting the drop is not
//! expressible.
//!
//! "Whatever path" includes an **unwind**, which is why this is a fix for all
//! three engines rather than just `stream.rs`. The two siblings dropped `rx`
//! on the consumer's *error* path only; because the channel was bound outside
//! `thread::scope`, a consumer **panic** left `rx` alive in the enclosing
//! frame, and `thread::scope`'s join-on-drop then waited on a producer parked
//! in `send` — the same indefinite hang, reached by panic instead of by error.
//! Both consumers run rayon work over hostile geometry (see `overview/hostile`
//! and the clip paths they call), so that is a live path, not a theoretical
//! one. Handing the receiver to the consumer makes the callee frame own it, so
//! the unwind drops it before the join is reached.

use crossbeam_channel::{bounded, Receiver, Sender};
use rayon::prelude::*;

use super::pipeline::ReadFlow;

/// Run `produce` on a scoped thread feeding a bounded channel of depth
/// `depth`, and `consume` on the calling thread, draining it.
///
/// Ordering and precedence match what the three call sites already relied on:
///
/// - Items arrive in send order (single producer, FIFO channel), so output —
///   and therefore row-group boundaries — stay byte-identical to a serial run.
/// - `depth` bounds run-ahead, and so peak memory, to `depth` in-flight items.
/// - A **producer** error takes precedence over a consumer error: the consumer
///   may merely be observing a truncated stream, so the upstream cause is the
///   one worth reporting.
/// - A producer panic is resumed on the calling thread rather than being
///   flattened into an error.
///
/// The consumer receives the [`Receiver`] by value. Dropping it — which
/// happens when `consume` returns, by any path including an error return or an
/// unwind — disconnects the channel and releases a producer blocked in `send`,
/// which is what makes the subsequent join finite. See the module docs.
///
/// # Contract on `produce`
///
/// **A `SendError` means the consumer hung up, and `produce` must treat it as
/// a clean stop — `break` or `return Ok(())`, never `?`.** Producer errors take
/// precedence, so a producer that maps `SendError` into its own error replaces
/// the consumer's real error with a bogus "channel closed", which is the very
/// class of masked failure #362 was about. All three call sites do this
/// correctly; the idiomatic-looking `tx.send(x).map_err(…)?` does not.
pub(super) fn scoped_pipe<T, E, R, P, C>(depth: usize, produce: P, consume: C) -> Result<R, E>
where
    T: Send,
    E: Send,
    P: FnOnce(&Sender<T>) -> Result<(), E> + Send,
    C: FnOnce(Receiver<T>) -> Result<R, E>,
{
    let (tx, rx) = bounded::<T>(depth.max(1));
    std::thread::scope(|scope| {
        // `move` transfers `tx` into the thread, so it is dropped when the
        // producer finishes — that disconnect is what fuses the consumer's
        // `recv` loop.
        let producer = scope.spawn(move || produce(&tx));
        let consumed = consume(rx);
        match producer.join() {
            Ok(produced) => produced?,
            Err(payload) => std::panic::resume_unwind(payload),
        }
        consumed
    })
}

/// Split a resolved `--in-flight-batches` depth into `(channel depth, compute
/// window)` for a consumer that drains with [`for_each_window`] (#494).
///
/// Before the window, `in_flight` was the channel depth alone and the
/// consumer held one more batch in hand, so roughly `in_flight + 1` input
/// batches were resident. The window takes up to `window` batches *out* of the
/// channel at once, so giving the channel only the remainder keeps the
/// resident input at about `in_flight` batches, and the memory model sized
/// against it unchanged. Both halves are at least 1, so `in_flight` 1 (and 2)
/// is exactly the old one-batch-at-a-time consumer.
pub(super) fn split_in_flight(in_flight: usize) -> (usize, usize) {
    let window = (in_flight / 2).max(1);
    let depth = in_flight.saturating_sub(window).max(1);
    (depth, window)
}

/// What [`for_each_window`] did, for the `TYLERTOO_PROFILE_JSON` dump: how
/// many items it took, in how many windows, and the widest window it ran. A
/// run whose `max_window` stays at 1 was reader-bound, not compute-bound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct WindowStats {
    pub(super) items: u64,
    pub(super) windows: u64,
    pub(super) max_window: u64,
}

/// Drain `rx` a window of up to `window` items at a time, run `process` over
/// each window concurrently on the rayon pool, and hand every `(item, result)`
/// to `emit` in **receive order** (#494).
///
/// This is how the pass-2 consumers get parallelism *across* batches as well
/// as within them. Processing one batch at a time made every batch a barrier:
/// its serial geometry decode, its slowest feature and its slowest level all
/// had to finish before the next batch started, which on a wide machine left
/// most of the pool idle. A window holds several batches' worth of work, so
/// those serial stretches overlap each other and the barrier tail is amortized
/// over `window` batches instead of one.
///
/// Output is unaffected: each item is processed independently of the others
/// (as it was), and `emit` sees the results in exactly the order the items
/// were received, so a downstream sink or writer receives the identical
/// sequence whatever the window size or composition.
///
/// - The window **never waits to fill**. It blocks for its first item, then
///   tops up with `try_recv` only from what is already queued, so a reader
///   that is slower than the compute shrinks the windows toward 1 (today's
///   behaviour) instead of idling the pool while it waits for a full one.
/// - The barrier form (collect a window, `par_iter` it, emit) is deliberate.
///   It is plain fork-join, so it cannot deadlock on a one-thread pool or when
///   the caller is itself a pool worker, and nothing blocks inside rayon.
/// - **Errors keep serial precedence**: the first `Err` in receive order is
///   returned, even if a later item in the same window failed sooner in wall
///   time. Items after it in its window were computed and are discarded;
///   nothing at or after it reaches `emit`. A panic in `process` propagates
///   out of `par_iter` to the caller, as one in the serial loop would.
/// - `emit` returning [`ReadFlow::Stop`] (its downstream hung up) ends the
///   drain cleanly after that item.
pub(super) fn for_each_window<T, R, E>(
    rx: &Receiver<T>,
    window: usize,
    process: impl Fn(&T) -> Result<R, E> + Sync,
    mut emit: impl FnMut(T, R) -> Result<ReadFlow, E>,
) -> Result<WindowStats, E>
where
    T: Send + Sync,
    R: Send,
    E: Send,
{
    let window = window.max(1);
    let mut stats = WindowStats::default();
    let mut items: Vec<T> = Vec::with_capacity(window);
    while let Ok(first) = rx.recv() {
        items.push(first);
        while items.len() < window {
            match rx.try_recv() {
                Ok(item) => items.push(item),
                Err(_) => break, // empty (don't wait) or disconnected
            }
        }
        stats.items += items.len() as u64;
        stats.windows += 1;
        stats.max_window = stats.max_window.max(items.len() as u64);
        // A one-item window skips the pool round-trip; the item's own
        // processing still parallelizes inside itself.
        let results: Vec<Result<R, E>> = if items.len() == 1 {
            vec![process(&items[0])]
        } else {
            items.par_iter().map(&process).collect()
        };
        for (item, result) in items.drain(..).zip(results) {
            if matches!(emit(item, result?)?, ReadFlow::Stop) {
                return Ok(stats);
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
#[expect(
    clippy::disallowed_types,
    reason = "#779: per-operation caches, worker coordination and profiling; no diagnostic globals"
)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Guard against the failure mode under test being an *indefinite hang*:
    /// run `f` on a helper thread and fail the test if it does not finish.
    /// Without this a regression wedges the suite instead of reporting.
    fn within<R: Send + 'static>(what: &str, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (done_tx, done_rx) = bounded::<R>(1);
        std::thread::spawn(move || {
            let _ = done_tx.send(f());
        });
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{what}: deadlocked (no result within 10s)"))
    }

    #[test]
    fn passes_items_through_in_send_order() {
        let got = scoped_pipe::<usize, (), Vec<usize>, _, _>(
            2,
            |tx| {
                for i in 0..64 {
                    tx.send(i).unwrap();
                }
                Ok(())
            },
            |rx| Ok(rx.iter().collect()),
        )
        .unwrap();
        assert_eq!(got, (0..64).collect::<Vec<_>>());
    }

    /// #362: the consumer errors out with far more than `depth` items still to
    /// come. The producer is necessarily blocked in `send`; handing the
    /// receiver to the consumer means it was dropped on that error path, so
    /// the producer sees the disconnect and the join is finite.
    ///
    /// Before the fix this deadlocked: the receiver outlived the join, so
    /// `send` never returned `Err` and the consumer's error was never reported.
    #[test]
    fn consumer_error_releases_a_blocked_producer() {
        let res = within("consumer_error_releases_a_blocked_producer", || {
            let sent = AtomicUsize::new(0);
            let out = scoped_pipe::<usize, &'static str, (), _, _>(
                2,
                |tx| {
                    // Many more than `depth`, so the producer parks in `send`.
                    for i in 0..10_000 {
                        if tx.send(i).is_err() {
                            break; // consumer hung up — expected
                        }
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(())
                },
                // Take one item, then bail — the shape of a writer that
                // errors mid-level.
                |rx| {
                    let _ = rx.recv();
                    Err("writer failed")
                },
            );
            (out, sent.load(Ordering::Relaxed))
        });
        assert_eq!(
            res.0,
            Err("writer failed"),
            "the consumer's error must be reported, not swallowed by a hang"
        );
        assert!(
            res.1 < 10_000,
            "producer should have been released early, sent {} items",
            res.1
        );
    }

    /// The other half of #362, and the half the sibling engines also had: a
    /// consumer that **panics** rather than erroring. The producer is blocked
    /// in `send`, so if the receiver outlives the unwind the join never
    /// returns and the panic is never reported — a hang in place of a
    /// diagnosable crash. Handing `rx` to the consumer means the unwind drops
    /// it, so the panic propagates.
    #[test]
    fn consumer_panic_releases_a_blocked_producer() {
        let caught = within("consumer_panic_releases_a_blocked_producer", || {
            std::panic::catch_unwind(|| {
                scoped_pipe::<usize, (), (), _, _>(
                    2,
                    |tx| {
                        for i in 0..10_000 {
                            if tx.send(i).is_err() {
                                break;
                            }
                        }
                        Ok(())
                    },
                    |rx| {
                        let _ = rx.recv();
                        panic!("consumer exploded");
                    },
                )
            })
        });
        let payload = caught.expect_err("the consumer's panic must propagate");
        let msg = payload
            .downcast_ref::<&str>()
            .copied()
            .unwrap_or_else(|| payload.downcast_ref::<String>().map_or("", String::as_str));
        assert_eq!(
            msg, "consumer exploded",
            "the consumer's own panic must be what surfaces"
        );
    }

    /// A producer error wins over a consumer error: the consumer is often just
    /// observing the truncated stream the producer's failure caused.
    #[test]
    fn producer_error_takes_precedence() {
        let out = scoped_pipe::<usize, &'static str, (), _, _>(
            4,
            |_tx| Err("read failed"),
            |rx| {
                for _ in rx.iter() {}
                Err("consumer noticed truncation")
            },
        );
        assert_eq!(out, Err("read failed"));
    }

    /// The consumer's error survives when the producer succeeds.
    #[test]
    fn consumer_error_surfaces_when_producer_is_clean() {
        let out = scoped_pipe::<usize, &'static str, (), _, _>(
            4,
            |tx| {
                let _ = tx.send(1);
                Ok(())
            },
            |_rx| Err("writer failed"),
        );
        assert_eq!(out, Err("writer failed"));
    }

    /// A producer panic is resumed on the calling thread rather than being
    /// flattened into an error or silently lost.
    #[test]
    #[should_panic(expected = "producer exploded")]
    fn producer_panic_resumes_on_the_caller() {
        let _ = scoped_pipe::<usize, (), (), _, _>(
            1,
            |_tx| panic!("producer exploded"),
            |rx| {
                for _ in rx.iter() {}
                Ok(())
            },
        );
    }

    // --- for_each_window (#494) ---------------------------------------------

    /// A disconnected channel already holding `0..n`, so every `try_recv`
    /// top-up succeeds and the window composition is deterministic.
    fn prefilled(n: usize) -> Receiver<usize> {
        let (tx, rx) = crossbeam_channel::unbounded();
        for i in 0..n {
            tx.send(i).unwrap();
        }
        rx
    }

    #[test]
    fn split_in_flight_halves_between_channel_and_window() {
        // (in_flight) -> (channel depth, compute window)
        assert_eq!(split_in_flight(1), (1, 1));
        assert_eq!(split_in_flight(2), (1, 1));
        assert_eq!(split_in_flight(3), (2, 1));
        assert_eq!(split_in_flight(4), (2, 2));
        assert_eq!(split_in_flight(5), (3, 2));
        assert_eq!(split_in_flight(16), (8, 8));
        // 0 is not a valid resolved depth, but must not produce a zero window.
        assert_eq!(split_in_flight(0), (1, 1));
    }

    /// Results reach `emit` in receive order, across window boundaries, with
    /// a count that is not a multiple of the window.
    #[test]
    fn window_emits_in_receive_order() {
        let rx = prefilled(20);
        let mut got = Vec::new();
        let stats = for_each_window::<usize, usize, ()>(
            &rx,
            3,
            |&i| Ok(i * 10),
            |i, r| {
                got.push((i, r));
                Ok(ReadFlow::Continue)
            },
        )
        .unwrap();
        assert_eq!(got, (0..20).map(|i| (i, i * 10)).collect::<Vec<_>>());
        assert_eq!(stats.items, 20);
        assert_eq!(stats.windows, 7, "20 items in windows of 3 is 6 full + 1");
        assert_eq!(stats.max_window, 3);
    }

    /// The point of the window: items in one window are processed at the same
    /// time, not one after another. The pre-#494-follow-up consumer took one
    /// batch at a time, so its peak concurrency was 1 whatever the pool size.
    #[test]
    fn window_processes_items_concurrently() {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        let peak = within("window_processes_items_concurrently", move || {
            let live = AtomicUsize::new(0);
            let peak = AtomicUsize::new(0);
            let rx = prefilled(8);
            pool.install(|| {
                for_each_window::<usize, (), ()>(
                    &rx,
                    4,
                    |_| {
                        let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        // Long enough for the pool's other workers to pick up
                        // their items while this one is still in flight.
                        std::thread::sleep(Duration::from_millis(50));
                        live.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    },
                    |_, ()| Ok(ReadFlow::Continue),
                )
                .unwrap();
            });
            peak.load(Ordering::SeqCst)
        });
        assert!(peak > 1, "window items never overlapped (peak {peak})");
    }

    /// Error precedence matches the serial loop: the first error in RECEIVE
    /// order wins, even when a later item in the same window also failed (and
    /// may well have failed first in wall-clock time), and nothing at or after
    /// the failing item is emitted.
    #[test]
    fn window_reports_the_first_error_in_receive_order() {
        let rx = prefilled(10);
        let mut emitted = Vec::new();
        let out = for_each_window::<usize, usize, String>(
            &rx,
            4,
            |&i| match i {
                5 => Err("five".to_string()),
                6 => Err("six".to_string()),
                _ => Ok(i),
            },
            |i, _| {
                emitted.push(i);
                Ok(ReadFlow::Continue)
            },
        );
        assert_eq!(out.unwrap_err(), "five");
        assert_eq!(emitted, vec![0, 1, 2, 3, 4]);
    }

    /// An `emit` error ends the drain with that error.
    #[test]
    fn window_propagates_an_emit_error() {
        let rx = prefilled(10);
        let out = for_each_window::<usize, usize, &'static str>(
            &rx,
            4,
            |&i| Ok(i),
            |i, _| {
                if i == 2 {
                    Err("sink failed")
                } else {
                    Ok(ReadFlow::Continue)
                }
            },
        );
        assert_eq!(out.unwrap_err(), "sink failed");
    }

    /// `ReadFlow::Stop` from `emit` (its downstream hung up) ends the drain
    /// cleanly: no further item is emitted or pulled.
    #[test]
    fn window_stops_when_emit_says_stop() {
        let rx = prefilled(10);
        let mut emitted = Vec::new();
        let stats = for_each_window::<usize, usize, ()>(
            &rx,
            4,
            |&i| Ok(i),
            |i, _| {
                emitted.push(i);
                Ok(if i == 5 {
                    ReadFlow::Stop
                } else {
                    ReadFlow::Continue
                })
            },
        )
        .unwrap();
        assert_eq!(emitted, vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(stats.windows, 2, "the window holding the stop is the last");
        assert!(
            !rx.is_empty(),
            "items after the stopping window stay unpulled"
        );
    }

    /// A window of 1 is the serial one-at-a-time loop.
    #[test]
    fn window_of_one_is_serial() {
        let rx = prefilled(5);
        let mut got = Vec::new();
        let stats = for_each_window::<usize, usize, ()>(
            &rx,
            1,
            |&i| Ok(i),
            |i, _| {
                got.push(i);
                Ok(ReadFlow::Continue)
            },
        )
        .unwrap();
        assert_eq!(got, vec![0, 1, 2, 3, 4]);
        assert_eq!((stats.items, stats.windows, stats.max_window), (5, 5, 1));
    }

    /// The top-up never waits: with one item available and the producer
    /// still connected, the window runs with what it has instead of blocking
    /// for `window` items (that would idle the pool behind a slow reader).
    #[test]
    fn window_does_not_wait_to_fill() {
        let got = within("window_does_not_wait_to_fill", || {
            let (tx, rx) = crossbeam_channel::unbounded::<usize>();
            tx.send(0).unwrap();
            let mut got = Vec::new();
            for_each_window::<usize, usize, ()>(
                &rx,
                8,
                |&i| Ok(i),
                |i, _| {
                    got.push(i);
                    // Stop after the first item; if the window had blocked to
                    // fill, we would never get here (tx is still alive).
                    Ok(ReadFlow::Stop)
                },
            )
            .unwrap();
            drop(tx);
            got
        });
        assert_eq!(got, vec![0]);
    }

    /// An empty, disconnected channel is zero windows, not an error.
    #[test]
    fn window_over_an_empty_stream() {
        let rx = prefilled(0);
        let stats =
            for_each_window::<usize, (), ()>(&rx, 4, |_| Ok(()), |_, ()| Ok(ReadFlow::Continue))
                .unwrap();
        assert_eq!((stats.items, stats.windows, stats.max_window), (0, 0, 0));
    }

    /// A panic inside `process` (hostile geometry in a clip path) propagates
    /// to the caller instead of being swallowed by the pool.
    #[test]
    #[should_panic(expected = "process exploded")]
    fn window_propagates_a_process_panic() {
        let rx = prefilled(4);
        let _ = for_each_window::<usize, (), ()>(
            &rx,
            4,
            |&i| {
                assert!(i != 2, "process exploded");
                Ok(())
            },
            |_, ()| Ok(ReadFlow::Continue),
        );
    }
}
