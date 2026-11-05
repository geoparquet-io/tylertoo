//! Test-only work accounting, isolated in a private Rayon pool like `gzip_counter`.
//! Thread locals avoid diagnostic globals and include nested Rayon work.
use std::cell::{Cell, RefCell};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum Path {
    #[default]
    Other,
    FirstPass,
    NoMerge,
    CacheRejected,
    FeatureFallback,
    Cache,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Counts {
    pub parts_interact: usize,
    /// Other, first pass, no merge, rejected cache, feature fallback, cache.
    pub parts_by_path: [usize; 6],
    pub check_polygons_from_parts: usize,
    pub quantized_new: usize,
    pub rings_quantized: usize,
    pub quantized_none: usize,
    pub candidate_pairs: usize,
    pub coalesce_calls: usize,
    pub polygon_clones: usize,
    pub draw_sorts: usize,
    pub draw_elements: usize,
    pub gap_rows: usize,
    pub builds_by_path: [usize; 6],
    pub feature_fallbacks: usize,
}

thread_local! {
    static COUNTS: RefCell<Counts> = RefCell::new(Counts::default());
    static PATH: Cell<Path> = const { Cell::new(Path::Other) };
    static IN_PARTS: Cell<bool> = const { Cell::new(false) };
    static QUANTIZING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn note(f: impl FnOnce(&mut Counts)) {
    COUNTS.with(|c| f(&mut c.borrow_mut()));
}

pub(crate) fn current_path() -> Path {
    PATH.with(Cell::get)
}

pub(crate) struct PathGuard(Path);

pub(crate) fn path(path: Path) -> PathGuard {
    PathGuard(PATH.with(|p| p.replace(path)))
}

pub(crate) fn plain_build(pass: u32) -> PathGuard {
    build(if pass == 1 {
        Path::FirstPass
    } else {
        Path::NoMerge
    })
}

pub(crate) fn build(p: Path) -> PathGuard {
    note(|c| c.builds_by_path[p as usize] += 1);
    path(p)
}

pub(crate) fn fallback() -> PathGuard {
    note(|c| c.feature_fallbacks += 1);
    path(Path::FeatureFallback)
}

/// Capture on the caller, restore inside each worker's per-feature encode.
/// Merely setting a caller's thread local would misattribute parallel work.
pub(crate) fn propagate_path<M, R>(encode: impl Fn(&M) -> R + Sync) -> impl Fn(&M) -> R + Sync {
    let p = current_path();
    move |m| {
        let _scope = path(p);
        encode(m)
    }
}

pub(crate) fn draw_sort(elements: usize) {
    note(|c| {
        c.draw_sorts += 1;
        c.draw_elements += elements;
    });
}

pub(crate) fn clone_polygons(g: &geo::Geometry<f64>) {
    note(|c| {
        c.polygon_clones += match g {
            geo::Geometry::Polygon(_) => 1,
            geo::Geometry::MultiPolygon(mp) => mp.0.len(),
            _ => 0,
        };
    });
}

impl Drop for PathGuard {
    fn drop(&mut self) {
        PATH.with(|p| p.set(self.0));
    }
}

pub(crate) struct PartsGuard(bool);

pub(crate) fn parts() -> PartsGuard {
    note(|c| {
        c.parts_interact += 1;
        c.parts_by_path[current_path() as usize] += 1;
    });
    PartsGuard(IN_PARTS.with(|p| p.replace(true)))
}

impl Drop for PartsGuard {
    fn drop(&mut self) {
        IN_PARTS.with(|p| p.set(self.0));
    }
}

pub(crate) fn check_polygons() {
    if IN_PARTS.with(Cell::get) {
        note(|c| c.check_polygons_from_parts += 1);
    }
}

pub(crate) struct NewGuard(bool);

pub(crate) fn quantized_new() -> NewGuard {
    note(|c| c.quantized_new += 1);
    NewGuard(false)
}

impl NewGuard {
    pub(crate) fn success(&mut self) {
        self.0 = true;
    }
}

impl Drop for NewGuard {
    fn drop(&mut self) {
        if !self.0 {
            note(|c| c.quantized_none += 1);
        }
    }
}

pub(crate) struct QuantizingGuard(bool);

pub(crate) fn quantizing() -> QuantizingGuard {
    QuantizingGuard(QUANTIZING.with(|q| q.replace(true)))
}

impl Drop for QuantizingGuard {
    fn drop(&mut self) {
        QUANTIZING.with(|q| q.set(self.0));
    }
}

pub(crate) fn ring() {
    if QUANTIZING.with(Cell::get) {
        note(|c| c.rings_quantized += 1);
    }
}

pub(crate) fn measure<T: Send>(f: impl FnOnce() -> T + Send) -> (T, Counts) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();
    let value = pool.install(f);
    let mut total = Counts::default();
    for c in pool.broadcast(|_| COUNTS.with(|c| c.borrow().clone())) {
        total.parts_interact += c.parts_interact;
        total.check_polygons_from_parts += c.check_polygons_from_parts;
        total.quantized_new += c.quantized_new;
        total.rings_quantized += c.rings_quantized;
        total.quantized_none += c.quantized_none;
        total.candidate_pairs += c.candidate_pairs;
        total.coalesce_calls += c.coalesce_calls;
        total.polygon_clones += c.polygon_clones;
        total.draw_sorts += c.draw_sorts;
        total.draw_elements += c.draw_elements;
        total.gap_rows += c.gap_rows;
        total.feature_fallbacks += c.feature_fallbacks;
        for i in 0..6 {
            total.parts_by_path[i] += c.parts_by_path[i];
            total.builds_by_path[i] += c.builds_by_path[i];
        }
    }
    (value, total)
}

#[test]
fn valve_work_counter_includes_workers_and_isolates_measurements() {
    use rayon::prelude::*;
    let (_, counts) = measure(|| {
        (0..100)
            .into_par_iter()
            .for_each(|_| note(|c| c.gap_rows += 1));
    });
    assert_eq!(counts.gap_rows, 100);
    let (_, counts) = measure(|| ());
    assert_eq!(counts.gap_rows, 0);
}
