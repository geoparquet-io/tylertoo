//! #876: the export's per-tile timeline, written when `TYLERTOO_TILE_TRACE`
//! names a file. Diagnostics only: nothing here is read by anything that
//! shapes the archive.
//!
//! Off, every event site costs one relaxed load of [`ON`]. On, each thread
//! appends JSON lines to a buffer of its own, which the export drains to
//! the file when it ends ([`Session`]); no tile waits on another's lock.
//! Every event carries `ev`, the OS thread id `tid`, and nanoseconds since
//! the export started on one monotonic clock. `context/PROFILING.md` lists
//! the events and fields; `benchmarks/tile_trace.py` reads them.
//!
//! Each thread keeps a stack of the traced work it is inside: tiles
//! ([`tile`]), the closure bodies of nested parallel calls ([`Site::body`]),
//! other jobs ([`job`]), and the parallel calls it waits in ([`wait`]).
//! Work on a tile's thread that belongs to another tile or job (rayon runs
//! other tasks while a thread waits in a join) is charged to the tile once,
//! at the outermost such frame: its time inside tile frames as `stolen_ns`,
//! the rest as `foreign_ns`. A thread counts as busy for the occupancy
//! samples while its innermost entry is a frame, not a wait.
//!
//! shortcut: one export at a time per process; a second export started
//! while one traces runs untraced, with a warning.
#![expect(
    clippy::disallowed_types,
    reason = "#876: a diagnostics-only trace sink; the export owns its lifetime through `Session`"
)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// The environment variable that names the trace file.
pub(crate) const ENV: &str = "TYLERTOO_TILE_TRACE";

/// How often the occupancy sampler emits an `occ` event.
const OCC_INTERVAL: Duration = Duration::from_millis(50);

/// A thread's buffer is written to the file once it holds this much.
const FLUSH_BYTES: usize = 1 << 20;

static ON: AtomicBool = AtomicBool::new(false);
/// Whether a trace was written earlier in this process: the first one
/// truncates the file, later ones append.
static STARTED: AtomicBool = AtomicBool::new(false);
static ORIGIN: OnceLock<Instant> = OnceLock::new();
/// The export's start, in nanoseconds since [`ORIGIN`].
static T0_NS: AtomicU64 = AtomicU64::new(0);
static THREADS: Mutex<Vec<Arc<ThreadBuf>>> = Mutex::new(Vec::new());
static FILE: Mutex<Option<std::io::BufWriter<std::fs::File>>> = Mutex::new(None);

/// Whether this export is traced.
#[inline]
pub(crate) fn on() -> bool {
    ON.load(Relaxed)
}

fn abs_ns() -> u64 {
    let origin = ORIGIN.get_or_init(Instant::now);
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// Nanoseconds since the traced export started.
pub(crate) fn now_ns() -> u64 {
    abs_ns().saturating_sub(T0_NS.load(Relaxed))
}

/// The time now when tracing, for a later [`span`].
#[inline]
pub(crate) fn mark() -> Option<u64> {
    on().then(now_ns)
}

/// Emit `ev` with `fields`, stamped `start_ns` (from [`mark`]) and `end_ns`
/// now. A no-op when `start` is `None` (tracing was off at the mark).
pub(crate) fn span(ev: &'static str, start: Option<u64>, fields: impl FnOnce() -> Value) {
    if let Some(start) = start {
        let mut v = fields();
        v["start_ns"] = start.into();
        v["end_ns"] = now_ns().into();
        emit(ev, v);
    }
}

/// Emit `ev` with `fields`, stamped `t_ns` now, when tracing.
pub(crate) fn event(ev: &'static str, fields: impl FnOnce() -> Value) {
    if on() {
        let mut v = fields();
        v["t_ns"] = now_ns().into();
        emit(ev, v);
    }
}

/// Where a frame's work sits in the export: its tile (a PMTiles tile id),
/// zoom, level, wave and partition, each when known.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ctx {
    pub(crate) tile: Option<u64>,
    pub(crate) zoom: Option<u8>,
    pub(crate) level: Option<usize>,
    pub(crate) wave: Option<usize>,
    pub(crate) part: Option<usize>,
}

impl Ctx {
    const NONE: Self = Self {
        tile: None,
        zoom: None,
        level: None,
        wave: None,
        part: None,
    };

    /// Wave `wave` of level `level` at `zoom`.
    pub(crate) fn wave(level: usize, zoom: u8, wave: usize) -> Self {
        Self {
            zoom: Some(zoom),
            level: Some(level),
            wave: Some(wave),
            ..Self::NONE
        }
    }

    /// Work at `zoom`, outside any tile or wave.
    pub(crate) fn zoom(zoom: u8) -> Self {
        Self {
            zoom: Some(zoom),
            ..Self::NONE
        }
    }

    fn fields(&self, v: &mut Value) {
        if let Some((z, x, y)) = self
            .tile
            .and_then(|t| crate::pmtiles_writer::tile_id_to_zxy(t).ok())
        {
            v["z"] = z.into();
            v["x"] = x.into();
            v["y"] = y.into();
        }
        v["zoom"] = self.zoom.into();
        v["level"] = self.level.into();
        v["wave"] = self.wave.into();
        v["part"] = self.part.into();
    }
}

/// One thread's share of the trace, reachable from [`Session`]'s drain.
struct ThreadBuf {
    tid: u64,
    /// Inside a frame, not waiting ([`occupancy`]).
    busy: AtomicBool,
    lines: Mutex<Vec<u8>>,
    /// Per (tile, zoom, site): what this thread waited and worked there.
    waits: Mutex<HashMap<WaitKey, WaitAgg>>,
}

type WaitKey = (Option<u64>, Option<u8>, &'static str);

#[derive(Default)]
struct WaitAgg {
    calls: u64,
    wall_ns: u64,
    max_wall_ns: u64,
    bodies: u64,
    core_ns: u64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(target_os = "linux")]
fn os_tid() -> u64 {
    // SAFETY: gettid has no preconditions and cannot fail.
    u64::try_from(unsafe { libc::gettid() }).unwrap_or(0)
}

#[cfg(not(target_os = "linux"))]
fn os_tid() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Tile,
    Body,
    Job,
}

struct Frame {
    kind: Kind,
    ctx: Ctx,
    start: u64,
    /// Time in tile frames nested in this one (outermost ones only).
    tile_ns: u64,
    stolen_ns: u64,
    foreign_ns: u64,
    /// The pass context and stage mask this frame replaced.
    saved: (Option<(Arc<PassAcc>, bool)>, u8),
}

enum Entry {
    Frame(Frame),
    Wait,
}

#[derive(Default)]
struct Local {
    buf: Option<Arc<ThreadBuf>>,
    stack: Vec<Entry>,
    /// The valve pass this thread works for, and whether it owns it.
    pass: Option<(Arc<PassAcc>, bool)>,
    /// The [`Stage`]s open on this thread for the current pass context.
    stages: u8,
}

thread_local! {
    static LOCAL: RefCell<Local> = RefCell::new(Local::default());
}

impl Local {
    fn buf(&mut self) -> Arc<ThreadBuf> {
        Arc::clone(self.buf.get_or_insert_with(|| {
            let buf = Arc::new(ThreadBuf {
                tid: os_tid(),
                busy: AtomicBool::new(false),
                lines: Mutex::new(Vec::new()),
                waits: Mutex::new(HashMap::new()),
            });
            lock(&THREADS).push(Arc::clone(&buf));
            buf
        }))
    }

    /// The innermost frame's context.
    fn ctx(&self) -> Ctx {
        self.stack
            .iter()
            .rev()
            .find_map(|e| match e {
                Entry::Frame(f) => Some(f.ctx),
                Entry::Wait => None,
            })
            .unwrap_or(Ctx::NONE)
    }

    fn sync_busy(&mut self) {
        let busy = matches!(self.stack.last(), Some(Entry::Frame(_)));
        let buf = self.buf();
        if buf.busy.load(Relaxed) != busy {
            buf.busy.store(busy, Relaxed);
        }
    }

    fn push(&mut self, kind: Kind, ctx: Ctx, pass: Option<(Arc<PassAcc>, bool)>, start: u64) {
        // A body of its own pass that the pass's thread runs stays the
        // owner's work, inside the stages it already has open.
        let own =
            matches!((&self.pass, &pass), (Some((a, true)), Some((b, _))) if Arc::ptr_eq(a, b));
        let pass = pass.map(|(acc, owner)| (acc, owner || own));
        let saved = (std::mem::replace(&mut self.pass, pass), self.stages);
        if !own {
            self.stages = 0;
        }
        self.stack.push(Entry::Frame(Frame {
            kind,
            ctx,
            start,
            tile_ns: 0,
            stolen_ns: 0,
            foreign_ns: 0,
            saved,
        }));
        self.sync_busy();
    }

    /// Pop the innermost frame at `end`, charge it to the frames around it,
    /// and return it with its wall.
    fn pop(&mut self, end: u64) -> Option<(Frame, u64)> {
        let idx = self
            .stack
            .iter()
            .rposition(|e| matches!(e, Entry::Frame(_)))?;
        // A wait left open above it by a panic goes with it.
        self.stack.truncate(idx + 1);
        let Some(Entry::Frame(mut f)) = self.stack.pop() else {
            return None;
        };
        let wall = end.saturating_sub(f.start);
        (self.pass, self.stages) = std::mem::take(&mut f.saved);
        charge(&mut self.stack, &f, wall);
        self.sync_busy();
        Some((f, wall))
    }
}

/// Charge the frame `f`, just popped after `wall` ns, to what is still on
/// `stack`: its tile time to the nearest enclosing frame's `tile_ns`, and,
/// to every enclosing tile it is the outermost foreign frame of, its tile
/// time as `stolen_ns` and the rest as `foreign_ns`.
fn charge(stack: &mut [Entry], f: &Frame, wall: u64) {
    let tile_time = if f.kind == Kind::Tile {
        wall
    } else {
        f.tile_ns
    };
    if let Some(Entry::Frame(parent)) = stack
        .iter_mut()
        .rev()
        .find(|e| matches!(e, Entry::Frame(_)))
    {
        parent.tile_ns += tile_time;
    }
    for t in 0..stack.len() {
        let Entry::Frame(tile) = &stack[t] else {
            continue;
        };
        if tile.kind != Kind::Tile || f.ctx.tile == tile.ctx.tile {
            continue;
        }
        let me = tile.ctx.tile;
        // A foreign frame between them already charged the whole span.
        let nested = stack[t + 1..]
            .iter()
            .any(|e| matches!(e, Entry::Frame(g) if g.ctx.tile != me));
        if nested {
            continue;
        }
        if let Entry::Frame(tile) = &mut stack[t] {
            tile.stolen_ns += tile_time;
            tile.foreign_ns += wall - tile_time;
        }
    }
}

/// Pops its frame when dropped, panicking or not.
struct FrameGuard {
    popped: bool,
}

impl FrameGuard {
    fn push(kind: Kind, ctx: Ctx, pass: Option<(Arc<PassAcc>, bool)>) -> Self {
        let start = now_ns();
        LOCAL.with(|l| l.borrow_mut().push(kind, ctx, pass, start));
        Self { popped: false }
    }

    fn pop(mut self) -> Option<(Frame, u64)> {
        self.popped = true;
        let end = now_ns();
        LOCAL.with(|l| l.borrow_mut().pop(end))
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        if !self.popped {
            let end = now_ns();
            let _ = LOCAL.try_with(|l| l.borrow_mut().pop(end));
        }
    }
}

fn emit(ev: &'static str, mut v: Value) {
    let Ok(buf) = LOCAL.try_with(|l| l.borrow_mut().buf()) else {
        return;
    };
    v["ev"] = ev.into();
    v["tid"] = buf.tid.into();
    let mut lines = lock(&buf.lines);
    if serde_json::to_writer(&mut *lines, &v).is_ok() {
        lines.push(b'\n');
    }
    if lines.len() >= FLUSH_BYTES {
        let chunk = std::mem::take(&mut *lines);
        drop(lines);
        write_file(&chunk);
    }
}

fn write_file(bytes: &[u8]) {
    if let Some(file) = lock(&FILE).as_mut() {
        if let Err(e) = file.write_all(bytes) {
            log::warn!("[tile-trace] write failed, the trace is incomplete: {e}");
        }
    }
}

/// Threads inside traced tile work and not waiting in a traced parallel
/// call, or `None` when tracing is off.
pub(crate) fn occupancy() -> Option<u32> {
    on().then(|| {
        let n = lock(&THREADS)
            .iter()
            .filter(|b| b.busy.load(Relaxed))
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    })
}

/// A traced export: opened by the export from [`ENV`], drained to the file
/// when dropped.
pub(crate) struct Session {
    stop: Option<std::sync::mpsc::Sender<()>>,
    sampler: Option<std::thread::JoinHandle<()>>,
}

impl Session {
    /// Start tracing if [`ENV`] names a file. A file that cannot be opened
    /// is a warning, and the export runs untraced.
    pub(crate) fn start(diagnostics: &crate::diagnostics::Diagnostics) -> Option<Self> {
        let path = std::env::var_os(ENV).filter(|p| !p.is_empty())?;
        if on() {
            diagnostics.warn(format!(
                "[tile-trace] {ENV}: an export is already traced in this process; \
                 this one runs untraced"
            ));
            return None;
        }
        let first = !STARTED.swap(true, Relaxed);
        let mut open = std::fs::OpenOptions::new();
        open.create(true);
        if first {
            open.write(true).truncate(true);
        } else {
            open.append(true);
        }
        let file = match open.open(&path) {
            Ok(file) => file,
            Err(e) => {
                diagnostics.warn(format!(
                    "[tile-trace] {ENV}={path:?} cannot be opened ({e}); the export runs untraced"
                ));
                return None;
            }
        };
        *lock(&FILE) = Some(std::io::BufWriter::new(file));
        T0_NS.store(abs_ns(), Relaxed);
        ON.store(true, Relaxed);
        event("start", || {
            json!({
                "pid": std::process::id(),
                "threads": rayon::current_num_threads(),
                "unix_secs": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |d| d.as_secs_f64()),
            })
        });
        let (stop, rx) = std::sync::mpsc::channel::<()>();
        let sampler = std::thread::spawn(move || loop {
            event("occ", || json!({ "busy": occupancy() }));
            match rx.recv_timeout(OCC_INTERVAL) {
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                _ => break,
            }
        });
        Some(Self {
            stop: Some(stop),
            sampler: Some(sampler),
        })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(sampler) = self.sampler.take() {
            let _ = sampler.join();
        }
        event("end", || json!({}));
        ON.store(false, Relaxed);
        let threads: Vec<Arc<ThreadBuf>> = lock(&THREADS).clone();
        for buf in &threads {
            let chunk = std::mem::take(&mut *lock(&buf.lines));
            write_file(&chunk);
        }
        // The waits, one line per thread, tile and site.
        let mut out = Vec::new();
        for buf in &threads {
            for ((tile, zoom, site), agg) in std::mem::take(&mut *lock(&buf.waits)) {
                let mut v = json!({
                    "ev": "wait",
                    "tid": buf.tid,
                    "site": site,
                    "calls": agg.calls,
                    "wall_ns": agg.wall_ns,
                    "max_wall_ns": agg.max_wall_ns,
                    "bodies": agg.bodies,
                    "core_ns": agg.core_ns,
                });
                Ctx {
                    tile,
                    zoom,
                    ..Ctx::NONE
                }
                .fields(&mut v);
                if serde_json::to_writer(&mut out, &v).is_ok() {
                    out.push(b'\n');
                }
            }
        }
        write_file(&out);
        if let Some(mut file) = lock(&FILE).take() {
            if let Err(e) = file.flush() {
                log::warn!("[tile-trace] flush failed, the trace is incomplete: {e}");
            }
        }
    }
}

fn record_wait(key: WaitKey, f: impl FnOnce(&mut WaitAgg)) {
    let Ok(buf) = LOCAL.try_with(|l| l.borrow_mut().buf()) else {
        return;
    };
    f(lock(&buf.waits).entry(key).or_default());
}

/// A nested parallel call being traced ([`wait`]): its closure bodies run
/// under [`Self::body`].
pub(crate) struct Site {
    on: bool,
    name: &'static str,
    ctx: Ctx,
    pass: Option<Arc<PassAcc>>,
}

static OFF: Site = Site {
    on: false,
    name: "",
    ctx: Ctx::NONE,
    pass: None,
};

impl Site {
    fn key(&self) -> WaitKey {
        (self.ctx.tile, self.ctx.zoom, self.name)
    }

    /// Run one closure body of the call, timed into its site on this
    /// thread, as the caller's tile and pass.
    #[inline]
    pub(crate) fn body<R>(&self, f: impl FnOnce() -> R) -> R {
        if !self.on {
            return f();
        }
        self.body_in(self.ctx, f)
    }

    /// [`Self::body`] for partition `part` of the caller's wave.
    #[inline]
    pub(crate) fn part<R>(&self, part: usize, f: impl FnOnce() -> R) -> R {
        if !self.on {
            return f();
        }
        self.body_in(
            Ctx {
                part: Some(part),
                ..self.ctx
            },
            f,
        )
    }

    fn body_in<R>(&self, ctx: Ctx, f: impl FnOnce() -> R) -> R {
        let pass = self.pass.as_ref().map(|p| (Arc::clone(p), false));
        let guard = FrameGuard::push(Kind::Body, ctx, pass);
        let r = f();
        if let Some((_, wall)) = guard.pop() {
            record_wait(self.key(), |a| {
                a.bodies += 1;
                a.core_ns += wall;
            });
        }
        r
    }
}

/// Run the nested parallel call `f` as site `name`, timing its wall on the
/// calling thread. Its closure bodies time themselves through
/// [`Site::body`]; a site without bodies records the wall alone.
#[inline]
pub(crate) fn wait<R>(name: &'static str, f: impl FnOnce(&Site) -> R) -> R {
    if !on() {
        return f(&OFF);
    }
    wait_traced(name, f)
}

fn wait_traced<R>(name: &'static str, f: impl FnOnce(&Site) -> R) -> R {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            let _ = LOCAL.try_with(|l| {
                let mut l = l.borrow_mut();
                if matches!(l.stack.last(), Some(Entry::Wait)) {
                    l.stack.pop();
                }
                l.sync_busy();
            });
        }
    }
    let site = LOCAL.with(|l| {
        let mut l = l.borrow_mut();
        let site = Site {
            on: true,
            name,
            ctx: l.ctx(),
            pass: l.pass.as_ref().map(|p| Arc::clone(&p.0)),
        };
        l.stack.push(Entry::Wait);
        l.sync_busy();
        site
    });
    let start = now_ns();
    let pop = Pop;
    let r = f(&site);
    drop(pop);
    let wall = now_ns().saturating_sub(start);
    record_wait(site.key(), |a| {
        a.calls += 1;
        a.wall_ns += wall;
        a.max_wall_ns = a.max_wall_ns.max(wall);
    });
    r
}

/// Run `f` as a traced job outside any tile (a wave's encode task, a
/// collect's clip) at `ctx`, which it lends the tiles and sites inside it.
#[inline]
pub(crate) fn job<R>(ctx: impl FnOnce() -> Ctx, f: impl FnOnce() -> R) -> R {
    if !on() {
        return f();
    }
    let guard = FrameGuard::push(Kind::Job, ctx(), None);
    let r = f();
    drop(guard);
    r
}

/// An open tile ([`tile`]), closed with its facts by [`Self::close`].
pub(crate) struct TileSpan(FrameGuard);

/// What a tile's `tile` event reports besides its timing.
pub(crate) struct TileFacts {
    pub(crate) members: usize,
    pub(crate) vertices: usize,
    pub(crate) passes: u32,
    pub(crate) merged: usize,
    pub(crate) raw_bytes: usize,
}

/// Open tile `key` (a PMTiles tile id) at `zoom`, in the level, wave and
/// partition of the work around it. `None` when tracing is off.
#[inline]
pub(crate) fn tile(key: u64, zoom: u8) -> Option<TileSpan> {
    if !on() {
        return None;
    }
    let ctx = LOCAL.with(|l| l.borrow().ctx());
    Some(TileSpan(FrameGuard::push(
        Kind::Tile,
        Ctx {
            tile: Some(key),
            zoom: Some(zoom),
            ..ctx
        },
        None,
    )))
}

impl TileSpan {
    /// Close the tile and emit its `tile` event.
    pub(crate) fn close(self, facts: &TileFacts) {
        let Some((frame, _)) = self.0.pop() else {
            return;
        };
        let mut v = json!({
            "start_ns": frame.start,
            "end_ns": now_ns(),
            "members": facts.members,
            "vertices": facts.vertices,
            "passes": facts.passes,
            "merged": facts.merged,
            "raw_bytes": facts.raw_bytes,
            "stolen_ns": frame.stolen_ns,
            "foreign_ns": frame.foreign_ns,
        });
        frame.ctx.fields(&mut v);
        emit("tile", v);
    }
}

/// The stages of a valve pass ([`stage`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Stage {
    Coalesce,
    Build,
    Quantize,
    Union,
    Finalize,
    Gzip,
}

const STAGES: [(Stage, &str); 6] = [
    (Stage::Coalesce, "coalesce"),
    (Stage::Build, "build"),
    (Stage::Quantize, "quantize"),
    (Stage::Union, "union"),
    (Stage::Finalize, "finalize"),
    (Stage::Gzip, "gzip"),
];

/// Counts a valve pass keeps beside its stage times ([`note`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Count {
    UnionHit,
    UnionMiss,
    Repaired,
}

/// One valve pass's accumulators, shared with the pool threads that run
/// its parallel work.
#[derive(Default)]
pub(crate) struct PassAcc {
    core: [AtomicU64; 6],
    wall: [AtomicU64; 6],
    counts: [AtomicU64; 3],
    largest_component: AtomicU64,
}

/// Time `f` as stage `s` of the valve pass this thread works for: its
/// core time on every thread, and its wall on the pass's own thread
/// (directly, or in a body of the pass's own parallel call). A stage
/// already open on this thread is not timed again.
#[inline]
pub(crate) fn stage<R>(s: Stage, f: impl FnOnce() -> R) -> R {
    if !on() {
        return f();
    }
    stage_traced(s, f)
}

fn stage_traced<R>(s: Stage, f: impl FnOnce() -> R) -> R {
    struct Close(u8);
    impl Drop for Close {
        fn drop(&mut self) {
            let _ = LOCAL.try_with(|l| l.borrow_mut().stages &= !self.0);
        }
    }
    let bit = 1u8 << s as u8;
    let open = LOCAL.with(|l| {
        let mut l = l.borrow_mut();
        if l.stages & bit != 0 {
            return None;
        }
        let pass = l.pass.clone()?;
        l.stages |= bit;
        Some(pass)
    });
    let Some((acc, owner)) = open else {
        return f();
    };
    let close = Close(bit);
    let t0 = Instant::now();
    let r = f();
    let ns = u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX);
    drop(close);
    acc.core[s as usize].fetch_add(ns, Relaxed);
    if owner {
        acc.wall[s as usize].fetch_add(ns, Relaxed);
    }
    r
}

fn with_pass(f: impl FnOnce(&PassAcc)) {
    if on() {
        if let Ok(Some(acc)) =
            LOCAL.try_with(|l| l.borrow().pass.as_ref().map(|p| Arc::clone(&p.0)))
        {
            f(&acc);
        }
    }
}

/// Count `c` for the valve pass this thread works for.
#[inline]
pub(crate) fn note(c: Count) {
    with_pass(|acc| {
        acc.counts[c as usize].fetch_add(1, Relaxed);
    });
}

/// Note a unioned component of `vertices` vertices for the pass.
#[inline]
pub(crate) fn note_component(vertices: impl FnOnce() -> usize) {
    with_pass(|acc| {
        acc.largest_component.fetch_max(vertices() as u64, Relaxed);
    });
}

/// An open valve pass ([`pass`]), emitted by [`Self::end`].
pub(crate) struct PassSpan(Option<PassOpen>);

struct PassOpen {
    acc: Arc<PassAcc>,
    kind: &'static str,
    index: u32,
    threshold: u64,
    start: u64,
    saved: (Option<(Arc<PassAcc>, bool)>, u8),
}

/// Open pass `index` of kind `kind` (`merge`, `drop`, `halve`,
/// `drop_merged`, `fill`) at `threshold` (the merge gap, the features
/// kept, or the extent) on this thread's tile.
#[inline]
pub(crate) fn pass(kind: &'static str, index: u32, threshold: u64) -> PassSpan {
    if !on() {
        return PassSpan(None);
    }
    let acc = Arc::new(PassAcc::default());
    let saved = LOCAL.with(|l| {
        let mut l = l.borrow_mut();
        let saved = (l.pass.replace((Arc::clone(&acc), true)), l.stages);
        l.stages = 0;
        saved
    });
    PassSpan(Some(PassOpen {
        acc,
        kind,
        index,
        threshold,
        start: now_ns(),
        saved,
    }))
}

impl PassSpan {
    /// Close the pass on its result: `groups` features encoded, `merged`
    /// members merged into them, and the tile's raw and gzip bytes.
    pub(crate) fn end(mut self, groups: usize, merged: usize, raw: usize, gzip: Option<usize>) {
        let Some(open) = self.0.take() else {
            return;
        };
        let end = now_ns();
        let ctx = LOCAL.with(|l| {
            let mut l = l.borrow_mut();
            (l.pass, l.stages) = (open.saved.0.clone(), open.saved.1);
            l.ctx()
        });
        let stage_map = |a: &[AtomicU64; 6]| -> Value {
            STAGES
                .iter()
                .map(|&(s, name)| (name.to_string(), a[s as usize].load(Relaxed).into()))
                .collect::<serde_json::Map<_, _>>()
                .into()
        };
        let acc = &open.acc;
        let mut v = json!({
            "kind": open.kind,
            "pass": open.index,
            "threshold": open.threshold,
            "groups": groups,
            "merged": merged,
            "raw_bytes": raw,
            "gzip_bytes": gzip,
            "start_ns": open.start,
            "end_ns": end,
            "core_ns": stage_map(&acc.core),
            "wall_ns": stage_map(&acc.wall),
            "union_hits": acc.counts[Count::UnionHit as usize].load(Relaxed),
            "union_misses": acc.counts[Count::UnionMiss as usize].load(Relaxed),
            "repaired": acc.counts[Count::Repaired as usize].load(Relaxed),
            "largest_component_vertices": acc.largest_component.load(Relaxed),
        });
        ctx.fields(&mut v);
        emit("pass", v);
    }
}

impl Drop for PassSpan {
    fn drop(&mut self) {
        if let Some(open) = self.0.take() {
            let _ = LOCAL.try_with(|l| {
                let mut l = l.borrow_mut();
                (l.pass, l.stages) = open.saved;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(tile: Option<u64>) -> Ctx {
        Ctx { tile, ..Ctx::NONE }
    }

    fn open(l: &mut Local, kind: Kind, tile: Option<u64>, start: u64) {
        l.push(kind, ctx(tile), None, start);
    }

    fn times(l: &Local, i: usize) -> (u64, u64) {
        match &l.stack[i] {
            Entry::Frame(f) => (f.stolen_ns, f.foreign_ns),
            Entry::Wait => unreachable!("a frame"),
        }
    }

    /// Tile A's thread waits, runs a body of its own call (not charged),
    /// then a job that runs tile B, which itself steals tile C: B is
    /// charged C as stolen, and A the whole job once, its time in B as
    /// stolen and the rest as foreign.
    #[test]
    fn nested_foreign_work_is_charged_once_at_the_outermost_frame() {
        let mut l = Local::default();
        open(&mut l, Kind::Tile, Some(1), 0);
        l.stack.push(Entry::Wait);
        open(&mut l, Kind::Body, Some(1), 10);
        l.pop(20);
        assert_eq!(times(&l, 0), (0, 0), "A's own body is not foreign");
        open(&mut l, Kind::Job, None, 15);
        open(&mut l, Kind::Tile, Some(2), 20);
        l.stack.push(Entry::Wait);
        open(&mut l, Kind::Tile, Some(3), 40);
        l.pop(60);
        assert_eq!(times(&l, 3), (20, 0), "B is charged C as stolen");
        assert_eq!(
            times(&l, 0),
            (0, 0),
            "A is charged at the outermost frame only"
        );
        l.pop(90);
        l.pop(100);
        assert_eq!(times(&l, 0), (70, 15), "the job's 85 ns, 70 of them in B");
        open(&mut l, Kind::Tile, Some(4), 100);
        l.pop(130);
        assert_eq!(times(&l, 0), (100, 15), "a tile stolen under A's wait");
        let (a, wall) = {
            l.stack.truncate(1);
            l.pop(200).expect("A")
        };
        assert_eq!((a.stolen_ns, a.foreign_ns, wall), (100, 15, 200));
        assert!(l.stack.is_empty());
    }

    /// A tile stolen inside a body of A's own call is charged to A.
    #[test]
    fn a_foreign_tile_inside_an_own_body_is_stolen() {
        let mut l = Local::default();
        open(&mut l, Kind::Tile, Some(1), 0);
        l.stack.push(Entry::Wait);
        open(&mut l, Kind::Body, Some(1), 5);
        l.stack.push(Entry::Wait);
        open(&mut l, Kind::Tile, Some(9), 10);
        l.pop(30);
        l.pop(40);
        assert_eq!(times(&l, 0), (20, 0));
    }

    /// A body of the pass's own call on the pass's thread stays the owner's
    /// work and keeps its open stages; one on another pass's thread does not.
    #[test]
    fn the_pass_thread_owns_its_own_bodies() {
        let acc = Arc::new(PassAcc::default());
        let mut l = Local {
            pass: Some((Arc::clone(&acc), true)),
            stages: 0b10,
            ..Local::default()
        };
        l.push(Kind::Body, ctx(Some(1)), Some((Arc::clone(&acc), false)), 0);
        assert!(matches!(&l.pass, Some((_, true))));
        assert_eq!(l.stages, 0b10);
        l.pop(1);
        let other = Arc::new(PassAcc::default());
        l.push(Kind::Body, ctx(Some(2)), Some((other, false)), 2);
        assert!(matches!(&l.pass, Some((_, false))));
        assert_eq!(l.stages, 0);
        l.pop(3);
        assert!(matches!(&l.pass, Some((p, true)) if Arc::ptr_eq(p, &acc)));
        assert_eq!(l.stages, 0b10);
    }

    /// The thread is busy inside a frame and idle while it waits.
    #[test]
    fn a_waiting_thread_is_not_busy() {
        let mut l = Local::default();
        open(&mut l, Kind::Tile, Some(1), 0);
        let buf = l.buf();
        assert!(buf.busy.load(Relaxed));
        l.stack.push(Entry::Wait);
        l.sync_busy();
        assert!(!buf.busy.load(Relaxed));
        open(&mut l, Kind::Body, Some(1), 1);
        assert!(buf.busy.load(Relaxed));
        l.pop(2);
        assert!(!buf.busy.load(Relaxed));
    }
}
