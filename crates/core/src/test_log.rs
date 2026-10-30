//! A capturing logger for unit tests that assert what a run logs.
//!
//! `log::set_logger` is process-global and succeeds once. Under nextest each
//! test runs in its own process, so [`install`] always gets the logger; in a
//! shared `cargo test` process another test may have installed its own first,
//! and [`install`] returns `false`: the caller then skips its log assertions.

use std::sync::{Mutex, PoisonError};
use std::thread::ThreadId;

static LINES: Mutex<Vec<(ThreadId, String)>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        LINES
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((std::thread::current().id(), record.args().to_string()));
    }
    fn flush(&self) {}
}

static CAPTURE: Capture = Capture;

/// Install the capturing logger at every level. `false` when another logger
/// was installed first.
pub(crate) fn install() -> bool {
    let _ = log::set_logger(&CAPTURE);
    let ours = std::ptr::addr_eq(log::logger(), &CAPTURE);
    if ours {
        log::set_max_level(log::LevelFilter::Trace);
    }
    ours
}

/// Remove and return the lines the calling thread logged so far.
pub(crate) fn take_thread_lines() -> Vec<String> {
    let me = std::thread::current().id();
    let mut lines = LINES.lock().unwrap_or_else(PoisonError::into_inner);
    let (mine, rest): (Vec<_>, Vec<_>) = lines.drain(..).partition(|(t, _)| *t == me);
    *lines = rest;
    mine.into_iter().map(|(_, line)| line).collect()
}
