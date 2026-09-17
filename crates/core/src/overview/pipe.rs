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

#[cfg(test)]
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
}
