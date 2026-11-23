//! SCRATCH PROTOTYPE (#768): where the merge valve's threshold comes from.
//!
//! `TYLERTOO_VALVE_SCOPE`:
//! - `tile` (or unset): each tile raises its own `mingap` until it fits.
//! - `zoom`: tippecanoe's rule. Every tile of a zoom starts from the largest
//!   `mingap` any tile of that zoom needed.
//! - `smooth`: a tile starts from the largest of its own need and each
//!   neighbour's need eased by [`smooth_factor`] per Chebyshev ring.
//!
//! `zoom` and `smooth` run the export twice: a probe pass with `tile` that
//! records each tile's final `mingap` (only tiles that merged), then the
//! real pass, where each tile starts from its floor. A tile whose floor is
//! its own need fits on its first encode.
//!
//! The export has no level-wide barrier: members are released bucket by
//! bucket as they encode, and a level group writes each wave as soon as it is
//! encoded (`export_level_group`). So the probe is a whole extra export.

#![allow(
    clippy::disallowed_types,
    reason = "SCRATCH PROTOTYPE: process-global probe state, to be replaced before any merge"
)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, RwLock};

pub(crate) const ENV: &str = "TYLERTOO_VALVE_SCOPE";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Tile,
    Zoom,
    Smooth,
    /// `smooth` from needs predicted in the scan, no probe (`valve_predict`).
    Predict,
}

pub(crate) fn scope() -> Scope {
    match std::env::var(ENV).as_deref() {
        Ok("zoom") => Scope::Zoom,
        Ok("smooth") => Scope::Smooth,
        Ok("predict") => Scope::Predict,
        _ => Scope::Tile,
    }
}

/// The share of a neighbour's `mingap` (a squared distance) that reaches one
/// Chebyshev ring further: `TYLERTOO_VALVE_SMOOTH`, default 0.5, i.e. the
/// linear merge distance shrinks by 1/sqrt(2) per tile, so a neighbour's need
/// fades over about 4 tiles. 0.25 (halving per tile) left NZ z7's p95 step
/// at 2.77 against 2.45 at 0.5 (#768).
pub(crate) fn smooth_factor() -> f64 {
    std::env::var("TYLERTOO_VALVE_SMOOTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.5)
}

/// Rings a neighbour's need reaches in `smooth`.
const SMOOTH_RINGS: i64 = 8;

pub(crate) type Key = (u8, u32, u32);

static RECORDING: RwLock<Option<HashMap<Key, u64>>> = RwLock::new(None);
/// Merge-valve encodes, this pass.
pub(crate) static ENCODES: AtomicU64 = AtomicU64::new(0);
/// Tiles whose first encode started from a raised floor, this pass.
pub(crate) static RAISED: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static CURRENT: std::cell::Cell<Option<Key>> = const { std::cell::Cell::new(None) };
}

/// Run `f` with `key` as the tile the merge valve is encoding.
pub(crate) fn with_tile<R>(key: Key, f: impl FnOnce() -> R) -> R {
    let prev = CURRENT.with(|c| c.replace(Some(key)));
    let r = f();
    CURRENT.with(|c| c.set(prev));
    r
}

/// The current tile ended at `mingap`.
pub(crate) fn record(mingap: u64) {
    if mingap == 0 {
        return;
    }
    let Some(key) = CURRENT.with(std::cell::Cell::get) else {
        return;
    };
    let mut rec = RECORDING
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(m) = rec.as_mut() {
        m.insert(key, mingap);
    }
}

pub(crate) fn start_recording() {
    *RECORDING.write().unwrap() = Some(HashMap::new());
    ENCODES.store(0, Ordering::Relaxed);
}

pub(crate) fn take_recording() -> HashMap<Key, u64> {
    RECORDING.write().unwrap().take().unwrap_or_default()
}

pub(crate) fn set_floors(
    need: HashMap<Key, u64>,
    floors: HashMap<Key, u64>,
    zoom: HashMap<u8, u64>,
) {
    *STATE.write().unwrap() = Some((need, floors, zoom));
    ENCODES.store(0, Ordering::Relaxed);
    RAISED.store(0, Ordering::Relaxed);
    release(Hold::Free);
}

/// The probe's needs, the per-tile floors, the per-zoom floors.
type State = (HashMap<Key, u64>, HashMap<Key, u64>, HashMap<u8, u64>);
static STATE: RwLock<Option<State>> = RwLock::new(None);

/// Where the floors a background predictor computes are.
enum Hold {
    Free,
    Pending,
    Failed(String),
}

static HELD: AtomicBool = AtomicBool::new(false);
static HOLD: (Mutex<Hold>, Condvar) = (Mutex::new(Hold::Free), Condvar::new());

fn release(to: Hold) {
    *HOLD
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = to;
    HELD.store(false, Ordering::Release);
    HOLD.1.notify_all();
}

/// Floors are on their way: [`wait_for_floors`] blocks until [`set_floors`]
/// or [`fail_floors`].
pub(crate) fn hold_floors() {
    *HOLD
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Hold::Pending;
    HELD.store(true, Ordering::Release);
}

/// The background predictor failed: [`wait_for_floors`] returns `message`.
pub(crate) fn fail_floors(message: String) {
    release(Hold::Failed(message));
}

/// Wait for held floors; the predictor's error if it failed. The export's
/// calling thread waits here before any tile encodes; on a pool thread it
/// runs the pool's queued work (the predictor's among it) meanwhile.
pub(crate) fn wait_for_floors() -> Result<(), String> {
    while rayon::current_thread_index().is_some() && HELD.load(Ordering::Acquire) {
        if rayon::yield_now() != Some(rayon::Yield::Executed) {
            std::thread::yield_now();
        }
    }
    if !HELD.load(Ordering::Acquire) {
        if let Hold::Failed(m) = std::mem::replace(
            &mut *HOLD
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Hold::Free,
        ) {
            return Err(m);
        }
        return Ok(());
    }
    let mut held = HOLD
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while matches!(*held, Hold::Pending) {
        held = HOLD
            .1
            .wait(held)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
    match std::mem::replace(&mut *held, Hold::Free) {
        Hold::Failed(m) => Err(m),
        _ => Ok(()),
    }
}

/// The threshold the current tile starts from: 0 unless a probe set floors.
/// Counts the tile in [`RAISED`] when that is above the tile's own need.
pub(crate) fn start_mingap() -> u64 {
    let Some(key) = CURRENT.with(std::cell::Cell::get) else {
        return 0;
    };
    // The export waited already; this only guards a tile encoded early,
    // and runs the pool's queued work (the predictor's among it) meanwhile.
    while HELD.load(Ordering::Acquire) {
        if rayon::yield_now() != Some(rayon::Yield::Executed) {
            std::thread::yield_now();
        }
    }
    let state = STATE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some((need, floors, zoom)) = state.as_ref() else {
        return 0;
    };
    let own = need.get(&key).copied().unwrap_or(0);
    let floor = floors
        .get(&key)
        .copied()
        .unwrap_or(0)
        .max(zoom.get(&key.0).copied().unwrap_or(0))
        .max(own);
    if floor > own {
        RAISED.fetch_add(1, Ordering::Relaxed);
    }
    floor
}

/// Each tile's starting threshold under `scope`, from the probe's needs.
pub(crate) fn floors(scope: Scope, need: &HashMap<Key, u64>) -> HashMap<Key, u64> {
    match scope {
        Scope::Tile => need.clone(),
        Scope::Zoom => HashMap::new(),
        Scope::Smooth => smooth_floors(need, smooth_factor(), true),
        Scope::Predict => unreachable!("predict floors come from valve_predict"),
    }
}

/// The smooth rule: each tile's floor is the largest neighbour need eased by
/// `factor` per Chebyshev ring, out to [`SMOOTH_RINGS`]; the tile's own need
/// counts (ring 0) only with `include_self`.
pub(crate) fn smooth_floors(
    need: &HashMap<Key, u64>,
    factor: f64,
    include_self: bool,
) -> HashMap<Key, u64> {
    let mut out: HashMap<Key, u64> = HashMap::new();
    for (&(z, x, y), &m) in need {
        let n = 1i64 << z;
        for dy in -SMOOTH_RINGS..=SMOOTH_RINGS {
            for dx in -SMOOTH_RINGS..=SMOOTH_RINGS {
                let (tx, ty) = (i64::from(x) + dx, i64::from(y) + dy);
                if tx < 0 || ty < 0 || tx >= n || ty >= n || (dx == 0 && dy == 0 && !include_self) {
                    continue;
                }
                let d = dx.abs().max(dy.abs());
                let v = (m as f64 * factor.powi(d as i32)) as u64;
                let e = out.entry((z, tx as u32, ty as u32)).or_insert(0);
                *e = (*e).max(v);
            }
        }
    }
    out.retain(|_, v| *v > 0);
    out
}

/// `zoom`: the largest need per zoom.
pub(crate) fn zoom_floors(need: &HashMap<Key, u64>) -> HashMap<u8, u64> {
    let mut out: HashMap<u8, u64> = HashMap::new();
    for (&(z, _, _), &m) in need {
        let e = out.entry(z).or_insert(0);
        *e = (*e).max(m);
    }
    out
}

/// Log a summary of the needs per zoom.
pub(crate) fn log_needs(need: &HashMap<Key, u64>) {
    let mut by_zoom: std::collections::BTreeMap<u8, Vec<u64>> = Default::default();
    for (&(z, _, _), &m) in need {
        by_zoom.entry(z).or_default().push(m);
    }
    for (z, mut v) in by_zoom {
        v.sort_unstable();
        log::info!(
            "[valve-scope] z{z}: {} tiles merged; mingap p50 {} max {}",
            v.len(),
            v[v.len() / 2],
            v[v.len() - 1]
        );
    }
}

static DUMP: std::sync::Mutex<Option<std::io::BufWriter<std::fs::File>>> =
    std::sync::Mutex::new(None);

/// SCRATCH (#768 predictor analysis): with `TYLERTOO_VALVE_DUMP=path`, one
/// line per merge-valve tile: `z x y members mergeable coords fit1 need
/// passes q0..q100` (mergeable gap percentiles).
pub(crate) fn dump(
    members: usize,
    coords: String,
    fit1: f64,
    need: u64,
    passes: u32,
    gaps: &mut [u64],
) {
    let Ok(path) = std::env::var("TYLERTOO_VALVE_DUMP") else {
        return;
    };
    let Some((z, x, y)) = CURRENT.with(std::cell::Cell::get) else {
        return;
    };
    gaps.sort_unstable();
    let mut line = format!(
        "{z} {x} {y} {members} {} {coords} {fit1} {need} {passes}",
        gaps.len()
    );
    for p in 0..=100usize {
        let v = if gaps.is_empty() {
            0
        } else {
            gaps[(gaps.len() - 1) * p / 100]
        };
        line.push_str(&format!(" {v}"));
    }
    line.push('\n');
    let mut d = DUMP
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if d.is_none() {
        *d = Some(std::io::BufWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .expect("dump file"),
        ));
    }
    use std::io::Write;
    d.as_mut().unwrap().write_all(line.as_bytes()).unwrap();
    d.as_mut().unwrap().flush().unwrap();
}

/// SCRATCH: the zoom of the tile being encoded.
pub(crate) fn current_zoom() -> u8 {
    CURRENT.with(std::cell::Cell::get).map_or(0, |k| k.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_floors_release_once_and_a_failure_reaches_the_waiter() {
        hold_floors();
        let setter =
            std::thread::spawn(|| set_floors(HashMap::new(), HashMap::new(), HashMap::new()));
        assert_eq!(wait_for_floors(), Ok(()));
        setter.join().unwrap();
        hold_floors();
        fail_floors("read failed".to_string());
        assert_eq!(wait_for_floors(), Err("read failed".to_string()));
        // Reported once; nothing is held afterwards.
        assert_eq!(wait_for_floors(), Ok(()));
    }
}
