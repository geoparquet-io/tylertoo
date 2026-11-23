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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

pub(crate) const ENV: &str = "TYLERTOO_VALVE_SCOPE";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    Tile,
    Zoom,
    Smooth,
}

pub(crate) fn scope() -> Scope {
    match std::env::var(ENV).as_deref() {
        Ok("zoom") => Scope::Zoom,
        Ok("smooth") => Scope::Smooth,
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

type Key = (u8, u32, u32);

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
}

/// The probe's needs, the per-tile floors, the per-zoom floors.
type State = (HashMap<Key, u64>, HashMap<Key, u64>, HashMap<u8, u64>);
static STATE: RwLock<Option<State>> = RwLock::new(None);

/// The threshold the current tile starts from: 0 unless a probe set floors.
/// Counts the tile in [`RAISED`] when that is above the tile's own need.
pub(crate) fn start_mingap() -> u64 {
    let Some(key) = CURRENT.with(std::cell::Cell::get) else {
        return 0;
    };
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
        Scope::Smooth => {
            let f = smooth_factor();
            let mut out: HashMap<Key, u64> = HashMap::new();
            for (&(z, x, y), &m) in need {
                let n = 1i64 << z;
                for dy in -SMOOTH_RINGS..=SMOOTH_RINGS {
                    for dx in -SMOOTH_RINGS..=SMOOTH_RINGS {
                        let (tx, ty) = (i64::from(x) + dx, i64::from(y) + dy);
                        if tx < 0 || ty < 0 || tx >= n || ty >= n {
                            continue;
                        }
                        let d = dx.abs().max(dy.abs());
                        let v = (m as f64 * f.powi(d as i32)) as u64;
                        let e = out.entry((z, tx as u32, ty as u32)).or_insert(0);
                        *e = (*e).max(v);
                    }
                }
            }
            out
        }
    }
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
