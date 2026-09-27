//! Hostile-geometry differential harness for export clipping (#205).
//!
//! Runs every candidate clipping engine over the same hostile inputs at three
//! zooms with export's tile buffer, scores each output with the same oracles,
//! and writes a markdown scorecard. The committed snapshot and the decision it
//! led to live in `corpus/HOSTILE_GEOMETRY.md`; the decision record is in
//! `context/ARCHITECTURE.md` ("Clipping engine (#205)").
//!
//! # Corpora
//!
//! - `tests/fixtures/geometry-test-data` (chrieke/geojson-invalid-geometry):
//!   every `.geojson` under `examples/`, parsed leniently — a fixture whose
//!   JSON is structurally broken is recorded as unparseable, not skipped
//!   silently.
//! - A synthetic hostile suite: bowties, spikes, holes crossing or outside
//!   their exterior, duplicate and collinear vertices, degenerate rings,
//!   combs and U-shapes across the tile edge, antimeridian and polar rings,
//!   sub-MVT-unit slivers (the quantization class), huge coordinates.
//! - Real clipper-stressing geometry (`full_scorecard` only): the 316k-vertex
//!   Antarctica ring and the Tielt-Winge admin polygon.
//!
//! # Engines
//!
//! - `production`: `clip::clip_geometry_simple` with export's defaults
//!   (`assume_simple` from `geometry_is_simple`, fast path on) — what
//!   `overview::export` runs.
//! - `production-strict`: `clip::clip_geometry` — the same dispatch with the
//!   #239 fast path off (`--no-simple-clip-fastpath`).
//! - `sh`: raw Sutherland–Hodgman (`sutherland_hodgman::clip_polygon_sh`),
//!   polygons only, no validity gate.
//! - `ioverlay`: raw i_overlay 9 (`ioverlay_clip`), the fallback engine.
//! - `wagyu-f64` / `wagyu-i64-mvt` / `wagyu-i64-world`: wagyu-rs 0.2.1, run
//!   out of process by `corpus/hostile_wagyu` (its dead `geo 0.32` dependency
//!   cannot resolve beside our geo 0.33 — see that crate's Cargo.toml). This
//!   harness dumps the polygon cases to `target/hostile_geometry_eval/
//!   cases.jsonl`; when `wagyu_results.jsonl` is present beside it, those
//!   results are scored with the same oracles and land in the same table.
//!
//! # Oracles, per (case × engine)
//!
//! panic (caught) · empty-vs-reference (drops / phantoms) · containment (every
//! output vertex inside the buffered bounds, engine-specific snap tolerance) ·
//! `geo::Validation` on the output · proper self-crossing via the production
//! sweep (`clip::geometry_is_simple`) · ring orientation consistency ·
//! area conservation against the i_overlay reference and, for inputs that are
//! valid, `geo::BooleanOps` (geo's own vendored i_overlay 4.5 — a different
//! version of the engine) as tiebreaker · vertex count · wall time · peak
//! heap (counting global allocator). Timings here are indicative (test
//! build, single run); `benches/hostile_geometry.rs` is the authoritative
//! perf comparison.
//!
//! Run:
//!   cargo test -p tylertoo-core --test hostile_geometry_eval -- --nocapture
//!   cargo test --release -p tylertoo-core --test hostile_geometry_eval \
//!       full_scorecard -- --nocapture
//! then, for the wagyu columns,
//!   cargo run --release --manifest-path corpus/hostile_wagyu/Cargo.toml
//! and re-run the test to fold them in.

#[cfg(not(feature = "dhat-heap"))]
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use geo::winding_order::WindingOrder;
use geo::{
    Area, BooleanOps, BoundingRect, Coord, CoordsIter, Geometry, LineString, MultiLineString,
    MultiPolygon, Polygon, Validation, Winding,
};

use tylertoo_core::clip::{clip_geometry, clip_geometry_simple, geometry_is_simple};
use tylertoo_core::ioverlay_clip::{
    clip_multilinestring_ioverlay, clip_multipolygon_ioverlay, clip_polygon_ioverlay,
};
use tylertoo_core::sutherland_hodgman::{clip_multipolygon_sh, clip_polygon_sh};
use tylertoo_core::tile::{tiles_for_bbox, TileBounds, TileCoord};

// ============================================================================
// Peak-heap counting allocator
// ============================================================================

// Off under `--features dhat-heap`, which installs its own global allocator
// in `tylertoo_core` (two in one binary is a compile error); the peak-heap
// column then reads 0 B.
#[cfg(not(feature = "dhat-heap"))]
struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

#[cfg(not(feature = "dhat-heap"))]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
}

#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static ALLOC: Counting = Counting;

/// Peak heap growth (bytes above the level at entry) while `f` runs. The
/// counters are process-global, so this is exact only when nothing else
/// allocates concurrently; the tests below run their sweeps sequentially.
fn measure_peak<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = CURRENT.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let out = f();
    let peak = PEAK.load(Ordering::Relaxed);
    (out, peak.saturating_sub(base))
}

// ============================================================================
// Corpus
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Tier {
    Corpus,
    Synthetic,
    Real,
}

impl Tier {
    fn label(self) -> &'static str {
        match self {
            Tier::Corpus => "geometry-test-data",
            Tier::Synthetic => "synthetic hostile",
            Tier::Real => "real",
        }
    }
}

struct Fixture {
    name: String,
    tier: Tier,
    geom: Geometry<f64>,
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn corpus_dir() -> PathBuf {
    repo_root().join("tests/fixtures/geometry-test-data/examples")
}

fn out_dir() -> PathBuf {
    let d = repo_root().join("target/hostile_geometry_eval");
    fs::create_dir_all(&d).expect("create target/hostile_geometry_eval");
    d
}

/// Why a corpus file produced no geometry.
#[derive(Debug)]
enum CorpusSkip {
    Unparseable(String),
    NoSupportedGeometry,
}

impl std::fmt::Display for CorpusSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CorpusSkip::Unparseable(why) => write!(f, "unparseable ({why})"),
            CorpusSkip::NoSupportedGeometry => write!(f, "no polygon or line geometry"),
        }
    }
}

fn coord_of(v: &serde_json::Value) -> Result<Coord<f64>, String> {
    let a = v.as_array().ok_or("position is not an array")?;
    if a.len() < 2 {
        return Err(format!("position has {} elements", a.len()));
    }
    let x = a[0].as_f64().ok_or("x is not a number")?;
    let y = a[1].as_f64().ok_or("y is not a number")?;
    Ok(Coord { x, y })
}

fn ring_of(v: &serde_json::Value) -> Result<LineString<f64>, String> {
    let a = v.as_array().ok_or("ring is not an array")?;
    Ok(LineString::new(
        a.iter().map(coord_of).collect::<Result<Vec<_>, _>>()?,
    ))
}

fn polygon_of(v: &serde_json::Value) -> Result<Polygon<f64>, String> {
    let rings = v
        .as_array()
        .ok_or("polygon coordinates is not an array")?
        .iter()
        .map(ring_of)
        .collect::<Result<Vec<_>, _>>()?;
    let mut it = rings.into_iter();
    let ext = it.next().ok_or("polygon has no rings")?;
    Ok(Polygon::new(ext, it.collect()))
}

/// Lenient GeoJSON walk: collects every polygon/line geometry it can read,
/// returns the first structural problem it hits otherwise.
fn collect_geojson(v: &serde_json::Value, out: &mut Vec<Geometry<f64>>) -> Result<(), String> {
    let obj = v.as_object().ok_or("node is not an object")?;
    let ty = obj
        .get("type")
        .and_then(|t| t.as_str())
        .ok_or("missing or non-string type")?;
    match ty {
        "FeatureCollection" => {
            for f in obj
                .get("features")
                .and_then(|f| f.as_array())
                .ok_or("features is not an array")?
            {
                if f.is_null() {
                    return Err("null feature".into());
                }
                collect_geojson(f, out)?;
            }
        }
        "Feature" => match obj.get("geometry") {
            Some(g) if g.is_null() => {}
            Some(g) => collect_geojson(g, out)?,
            None => return Err("feature without geometry member".into()),
        },
        "GeometryCollection" => {
            for g in obj
                .get("geometries")
                .and_then(|g| g.as_array())
                .ok_or("geometries is not an array")?
            {
                if g.is_null() {
                    return Err("null geometry in collection".into());
                }
                collect_geojson(g, out)?;
            }
        }
        "Polygon" => out.push(Geometry::Polygon(polygon_of(
            obj.get("coordinates").ok_or("missing coordinates")?,
        )?)),
        "MultiPolygon" => {
            let polys = obj
                .get("coordinates")
                .and_then(|c| c.as_array())
                .ok_or("multipolygon coordinates is not an array")?
                .iter()
                .map(polygon_of)
                .collect::<Result<Vec<_>, _>>()?;
            out.push(Geometry::MultiPolygon(MultiPolygon::new(polys)));
        }
        "LineString" => out.push(Geometry::LineString(ring_of(
            obj.get("coordinates").ok_or("missing coordinates")?,
        )?)),
        "MultiLineString" => {
            let lines = obj
                .get("coordinates")
                .and_then(|c| c.as_array())
                .ok_or("multilinestring coordinates is not an array")?
                .iter()
                .map(ring_of)
                .collect::<Result<Vec<_>, _>>()?;
            out.push(Geometry::MultiLineString(MultiLineString::new(lines)));
        }
        // Points are trivially clipped (containment test); not under evaluation.
        "Point" | "MultiPoint" => {}
        other => return Err(format!("unknown type {other:?}")),
    }
    Ok(())
}

fn load_corpus() -> (Vec<Fixture>, BTreeMap<String, CorpusSkip>) {
    let mut fixtures = Vec::new();
    let mut skipped = BTreeMap::new();
    let mut files: Vec<PathBuf> = Vec::new();
    for sub in [
        "invalid_geometries",
        "invalid_structure",
        "problematic_geometries",
        "problematic_structure",
        "valid",
    ] {
        let dir = corpus_dir().join(sub);
        let Ok(rd) = fs::read_dir(&dir) else {
            panic!(
                "{} missing — run `git submodule update --init`",
                dir.display()
            );
        };
        for e in rd {
            let p = e.unwrap().path();
            if p.extension().is_some_and(|x| x == "geojson") {
                files.push(p);
            }
        }
    }
    files.sort();
    for p in files {
        let rel = p
            .strip_prefix(corpus_dir())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let text = fs::read_to_string(&p).unwrap();
        let json: serde_json::Value = match serde_json::from_str(&text) {
            Ok(j) => j,
            Err(e) => {
                skipped.insert(rel, CorpusSkip::Unparseable(format!("json: {e}")));
                continue;
            }
        };
        let mut geoms = Vec::new();
        if let Err(e) = collect_geojson(&json, &mut geoms) {
            skipped.insert(rel, CorpusSkip::Unparseable(e));
            continue;
        }
        if geoms.is_empty() {
            skipped.insert(rel, CorpusSkip::NoSupportedGeometry);
            continue;
        }
        let many = geoms.len() > 1;
        for (i, g) in geoms.into_iter().enumerate() {
            fixtures.push(Fixture {
                name: if many {
                    format!("{rel}#{i}")
                } else {
                    rel.clone()
                },
                tier: Tier::Corpus,
                geom: g,
            });
        }
    }
    (fixtures, skipped)
}

fn poly(ext: &[(f64, f64)], holes: &[&[(f64, f64)]]) -> Geometry<f64> {
    Geometry::Polygon(Polygon::new(
        LineString::from(ext.to_vec()),
        holes.iter().map(|h| LineString::from(h.to_vec())).collect(),
    ))
}

fn circle(cx: f64, cy: f64, r: f64, n: usize) -> Vec<(f64, f64)> {
    let mut v: Vec<(f64, f64)> = (0..n)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect();
    v.push(v[0]);
    v
}

/// Synthesized hostile inputs. Every one is a shape the export clipper can
/// meet: invalid rings the convert pass carries verbatim (#188), tile-edge
/// topologies that trip Sutherland–Hodgman (#94), and quantization slivers
/// (#383).
// A fixture table, one entry per hostile shape; splitting it by category
// would hide what the suite covers.
#[allow(clippy::too_many_lines)]
fn synthetic_suite() -> Vec<Fixture> {
    let mut v: Vec<(&str, Geometry<f64>)> = Vec::new();

    v.push((
        "bowtie",
        poly(
            &[(0.0, 0.0), (4.0, 4.0), (4.0, 0.0), (0.0, 4.0), (0.0, 0.0)],
            &[],
        ),
    ));
    v.push((
        "figure-eight-3-lobes",
        poly(
            &[
                (0.0, 0.0),
                (6.0, 6.0),
                (0.0, 6.0),
                (6.0, 0.0),
                (12.0, 6.0),
                (6.0, 12.0),
                (0.0, 0.0),
            ],
            &[],
        ),
    ));
    v.push((
        "spike-zero-width",
        poly(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (5.0, 10.0),
                (5.0, 20.0),
                (5.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            &[],
        ),
    ));
    v.push((
        "hole-crosses-exterior",
        poly(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            &[&[
                (5.0, 5.0),
                (15.0, 5.0),
                (15.0, 15.0),
                (5.0, 15.0),
                (5.0, 5.0),
            ]],
        ),
    ));
    v.push((
        "hole-outside-exterior",
        poly(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            &[&[
                (20.0, 20.0),
                (25.0, 20.0),
                (25.0, 25.0),
                (20.0, 25.0),
                (20.0, 20.0),
            ]],
        ),
    ));
    v.push((
        "hole-same-winding-as-exterior",
        poly(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            &[&[(2.0, 2.0), (8.0, 2.0), (8.0, 8.0), (2.0, 8.0), (2.0, 2.0)]],
        ),
    ));
    v.push((
        "nested-holes",
        poly(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            &[
                &[(1.0, 1.0), (1.0, 9.0), (9.0, 9.0), (9.0, 1.0), (1.0, 1.0)],
                &[(4.0, 4.0), (4.0, 6.0), (6.0, 6.0), (6.0, 4.0), (4.0, 4.0)],
            ],
        ),
    ));
    v.push((
        "duplicate-consecutive-vertices",
        poly(
            &[
                (0.0, 0.0),
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            &[],
        ),
    ));
    v.push((
        "collinear-run",
        poly(
            &[
                (0.0, 0.0),
                (3.0, 0.0),
                (6.0, 0.0),
                (10.0, 0.0),
                (10.0, 10.0),
                (0.0, 10.0),
                (0.0, 0.0),
            ],
            &[],
        ),
    ));
    v.push((
        "unclosed-ring",
        poly(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0)], &[]),
    ));
    v.push(("two-vertex-ring", poly(&[(0.0, 0.0), (10.0, 10.0)], &[])));
    v.push((
        "zero-area-collinear-ring",
        poly(&[(0.0, 0.0), (5.0, 5.0), (10.0, 10.0), (0.0, 0.0)], &[]),
    ));
    v.push((
        "empty-ring",
        Geometry::Polygon(Polygon::new(LineString::new(vec![]), vec![])),
    ));
    v.push((
        "clockwise-exterior",
        poly(
            &[
                (0.0, 0.0),
                (0.0, 10.0),
                (10.0, 10.0),
                (10.0, 0.0),
                (0.0, 0.0),
            ],
            &[],
        ),
    ));
    // U-shape whose opening straddles a tile edge (#94): the S-H bridge case.
    v.push((
        "u-shape",
        poly(
            &[
                (0.0, 0.0),
                (30.0, 0.0),
                (30.0, 30.0),
                (20.0, 30.0),
                (20.0, 10.0),
                (10.0, 10.0),
                (10.0, 30.0),
                (0.0, 30.0),
                (0.0, 0.0),
            ],
            &[],
        ),
    ));
    v.push(("comb-8-teeth", {
        let mut ext = vec![(0.0, 0.0), (32.0, 0.0)];
        for t in (0..8).rev() {
            let x = t as f64 * 4.0;
            ext.push((x + 3.0, 0.0));
            ext.push((x + 3.0, 30.0));
            ext.push((x + 1.0, 30.0));
            ext.push((x + 1.0, 0.0));
        }
        ext.push((0.0, 0.0));
        poly(&ext, &[])
    }));
    v.push((
        "antimeridian-literal-wide",
        poly(
            &[
                (170.0, -10.0),
                (-170.0, -10.0),
                (-170.0, 10.0),
                (170.0, 10.0),
                (170.0, -10.0),
            ],
            &[],
        ),
    ));
    v.push((
        "antimeridian-beyond-180",
        poly(
            &[
                (170.0, -10.0),
                (190.0, -10.0),
                (190.0, 10.0),
                (170.0, 10.0),
                (170.0, -10.0),
            ],
            &[],
        ),
    ));
    v.push((
        "polar-cap",
        poly(
            &[
                (-180.0, 80.0),
                (180.0, 80.0),
                (180.0, 90.0),
                (-180.0, 90.0),
                (-180.0, 80.0),
            ],
            &[],
        ),
    ));
    v.push((
        "south-polar-ring-through-pole",
        poly(
            &[
                (-180.0, -60.0),
                (0.0, -60.0),
                (180.0, -60.0),
                (180.0, -90.0),
                (-180.0, -90.0),
                (-180.0, -60.0),
            ],
            &[],
        ),
    ));
    v.push((
        "outside-lonlat-domain",
        poly(
            &[
                (500.0, 500.0),
                (600.0, 500.0),
                (600.0, 600.0),
                (500.0, 600.0),
                (500.0, 500.0),
            ],
            &[],
        ),
    ));
    v.push((
        "huge-coordinates",
        poly(
            &[
                (-1e15, -1e15),
                (1e15, -1e15),
                (1e15, 1e15),
                (-1e15, 1e15),
                (-1e15, -1e15),
            ],
            &[],
        ),
    ));
    v.push((
        "tiny-coordinates",
        poly(
            &[
                (0.0, 0.0),
                (1e-12, 0.0),
                (1e-12, 1e-12),
                (0.0, 1e-12),
                (0.0, 0.0),
            ],
            &[],
        ),
    ));
    // Quantization class (#383): a sliver thinner than one MVT unit at the
    // zooms the sweep picks, and a bowtie whose crossing only appears once
    // vertices snap to the grid.
    v.push((
        "sliver-sub-mvt-unit",
        poly(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 1e-5),
                (0.0, 1e-5),
                (0.0, 0.0),
            ],
            &[],
        ),
    ));
    v.push((
        "near-bowtie-snap-crosses",
        poly(
            &[
                (0.0, 0.0),
                (10.0, 0.0),
                (10.0, 5.0),
                (5.0 + 1e-6, 2.5),
                (5.0 - 1e-6, 2.5 + 1e-6),
                (0.0, 5.0),
                (0.0, 0.0),
            ],
            &[],
        ),
    ));
    v.push(("circle-1k", poly(&circle(5.0, 5.0, 4.0, 1000), &[])));
    v.push(("circle-with-hole-crossing-edge", {
        let ext = circle(5.0, 5.0, 4.0, 200);
        let hole: Vec<(f64, f64)> = circle(5.0, 5.0, 2.0, 50).into_iter().rev().collect();
        poly(&ext, &[&hole])
    }));
    v.push((
        "multipolygon-overlapping-parts",
        Geometry::MultiPolygon(MultiPolygon::new(vec![
            Polygon::new(
                LineString::from(vec![
                    (0.0, 0.0),
                    (10.0, 0.0),
                    (10.0, 10.0),
                    (0.0, 10.0),
                    (0.0, 0.0),
                ]),
                vec![],
            ),
            Polygon::new(
                LineString::from(vec![
                    (5.0, 5.0),
                    (15.0, 5.0),
                    (15.0, 15.0),
                    (5.0, 15.0),
                    (5.0, 5.0),
                ]),
                vec![],
            ),
        ])),
    ));
    v.push((
        "multipolygon-touching-corner",
        Geometry::MultiPolygon(MultiPolygon::new(vec![
            Polygon::new(
                LineString::from(vec![
                    (0.0, 0.0),
                    (10.0, 0.0),
                    (10.0, 10.0),
                    (0.0, 10.0),
                    (0.0, 0.0),
                ]),
                vec![],
            ),
            Polygon::new(
                LineString::from(vec![
                    (10.0, 10.0),
                    (20.0, 10.0),
                    (20.0, 20.0),
                    (10.0, 20.0),
                    (10.0, 10.0),
                ]),
                vec![],
            ),
        ])),
    ));
    v.push((
        "line-zero-length",
        Geometry::LineString(LineString::from(vec![(1.0, 1.0), (1.0, 1.0)])),
    ));
    v.push((
        "line-self-crossing",
        Geometry::LineString(LineString::from(vec![
            (0.0, 0.0),
            (10.0, 10.0),
            (10.0, 0.0),
            (0.0, 10.0),
        ])),
    ));
    v.push((
        "line-along-tile-edge",
        Geometry::LineString(LineString::from(vec![
            (0.0, 0.0),
            (0.0, 20.0),
            (20.0, 20.0),
        ])),
    ));

    v.into_iter()
        .map(|(name, geom)| Fixture {
            name: name.to_string(),
            tier: Tier::Synthetic,
            geom,
        })
        .collect()
}

fn load_wkb(rel: &str) -> Option<Geometry<f64>> {
    use geozero::ToGeo;
    let p = repo_root().join(rel);
    let bytes = fs::read(&p).ok()?;
    geozero::wkb::Wkb(bytes).to_geo().ok()
}

fn real_suite() -> Vec<Fixture> {
    let mut v = Vec::new();
    for (name, rel) in [
        (
            "antarctica-316k",
            "tests/fixtures/realdata/antarctica-polygon.wkb",
        ),
        (
            "tielt-winge-adm4",
            "tests/fixtures/realdata/tielt-winge-adm4.wkb",
        ),
    ] {
        match load_wkb(rel) {
            Some(geom) => v.push(Fixture {
                name: name.into(),
                tier: Tier::Real,
                geom,
            }),
            None => eprintln!("hostile_geometry_eval: {rel} missing, skipping"),
        }
    }
    v
}

// ============================================================================
// Cases: fixture × zoom × tile, buffered like export
// ============================================================================

/// Export's default: 8 px of a 256 px tile, converted from the tile's
/// longitude width (`overview::export::buffer_fraction`).
const BUFFER_PX: f64 = 8.0;
const NOMINAL_TILE_PX: f64 = 256.0;

struct Case {
    id: String,
    fixture: usize,
    zoom: u8,
    tile: TileBounds,
    bounds: TileBounds,
}

fn finite_bbox(g: &Geometry<f64>) -> Option<TileBounds> {
    let r = g.bounding_rect()?;
    let b = TileBounds::new(r.min().x, r.min().y, r.max().x, r.max().y);
    (b.lng_min.is_finite()
        && b.lat_min.is_finite()
        && b.lng_max.is_finite()
        && b.lat_max.is_finite())
    .then_some(b)
}

/// Three zooms per fixture: the coarsest at which the bbox spans about one
/// tile, then two and four finer. Tiles per zoom are capped (per tier) by a
/// stride so a world-spanning ring does not enumerate 2^(2z) tiles.
fn cases_for(fixtures: &[Fixture], tiles_per_zoom: impl Fn(Tier) -> usize) -> Vec<Case> {
    let mut cases = Vec::new();
    for (fi, f) in fixtures.iter().enumerate() {
        let Some(bbox) = finite_bbox(&f.geom) else {
            continue;
        };
        let tiles_per_zoom = tiles_per_zoom(f.tier);
        let span = bbox.width().max(bbox.height()).clamp(1e-9, 360.0);
        let z0 = ((360.0 / span).log2().floor() as i32).clamp(0, 18) as u8;
        for zoom in [z0, z0 + 2, z0 + 4] {
            let clamped = TileBounds::new(
                bbox.lng_min.clamp(-180.0, 180.0),
                bbox.lat_min.clamp(-85.0511, 85.0511),
                bbox.lng_max.clamp(-180.0, 180.0),
                bbox.lat_max.clamp(-85.0511, 85.0511),
            );
            let all: Vec<TileCoord> = tiles_for_bbox(&clamped, zoom).collect();
            let stride = all.len().div_ceil(tiles_per_zoom).max(1);
            for tc in all.into_iter().step_by(stride).take(tiles_per_zoom) {
                let tile = tc.bounds();
                let buf = tile.width() * BUFFER_PX / NOMINAL_TILE_PX;
                cases.push(Case {
                    id: format!("{}|z{}|{}|{}", f.name, zoom, tc.x, tc.y),
                    fixture: fi,
                    zoom,
                    bounds: TileBounds::new(
                        tile.lng_min - buf,
                        tile.lat_min - buf,
                        tile.lng_max + buf,
                        tile.lat_max + buf,
                    ),
                    tile,
                });
            }
        }
    }
    cases
}

// ============================================================================
// Engines
// ============================================================================

type Clipper = fn(&Geometry<f64>, &Case, bool) -> Option<Geometry<f64>>;

struct Engine {
    name: &'static str,
    clip: Clipper,
    polygons_only: bool,
}

fn engine_production(g: &Geometry<f64>, c: &Case, simple: bool) -> Option<Geometry<f64>> {
    // Buffer is folded into `bounds` already: pass the tile and the buffer
    // separately, exactly as `feature_tile_members_direct` does.
    let buf = c.tile.width() * BUFFER_PX / NOMINAL_TILE_PX;
    clip_geometry_simple(g, &c.tile, buf, simple, true)
}

fn engine_production_strict(g: &Geometry<f64>, c: &Case, _simple: bool) -> Option<Geometry<f64>> {
    let buf = c.tile.width() * BUFFER_PX / NOMINAL_TILE_PX;
    clip_geometry(g, &c.tile, buf)
}

fn engine_sh(g: &Geometry<f64>, c: &Case, _simple: bool) -> Option<Geometry<f64>> {
    match g {
        Geometry::Polygon(p) => clip_polygon_sh(p, &c.bounds),
        Geometry::MultiPolygon(mp) => {
            clip_multipolygon_sh(mp, &c.bounds).map(Geometry::MultiPolygon)
        }
        _ => None,
    }
}

fn engine_ioverlay(g: &Geometry<f64>, c: &Case, _simple: bool) -> Option<Geometry<f64>> {
    match g {
        Geometry::Polygon(p) => clip_polygon_ioverlay(p, &c.bounds),
        Geometry::MultiPolygon(mp) => clip_multipolygon_ioverlay(mp, &c.bounds),
        Geometry::LineString(l) => {
            let out =
                clip_multilinestring_ioverlay(&MultiLineString::new(vec![l.clone()]), &c.bounds);
            (!out.0.is_empty()).then_some(Geometry::MultiLineString(out))
        }
        Geometry::MultiLineString(ml) => {
            let out = clip_multilinestring_ioverlay(ml, &c.bounds);
            (!out.0.is_empty()).then_some(Geometry::MultiLineString(out))
        }
        _ => None,
    }
}

const ENGINES: &[Engine] = &[
    Engine {
        name: "production",
        clip: engine_production,
        polygons_only: false,
    },
    Engine {
        name: "production-strict",
        clip: engine_production_strict,
        polygons_only: false,
    },
    Engine {
        name: "sh",
        clip: engine_sh,
        polygons_only: true,
    },
    Engine {
        name: "ioverlay",
        clip: engine_ioverlay,
        polygons_only: false,
    },
];

// ============================================================================
// Oracles
// ============================================================================

#[derive(Default, Clone)]
struct Verdict {
    panicked: bool,
    error: Option<String>,
    empty: bool,
    vertices: usize,
    parts: usize,
    area: f64,
    /// Largest distance any output vertex lies outside the buffered bounds,
    /// in MVT units at this tile (4096 across the unbuffered tile). Below 0.5
    /// the vertex quantizes onto the boundary; above it the encoded tile
    /// carries geometry past its buffer.
    excursion_mvt: f64,
    valid: bool,
    invalid_reason: Option<String>,
    /// Output too large for `geo::Validation` (see `VALIDATION_VERTEX_CAP`).
    validity_unchecked: bool,
    simple: bool,
    orientation_consistent: bool,
    nanos: u64,
    peak_bytes: Option<usize>,
}

fn polygons_of(g: &Geometry<f64>) -> Vec<&Polygon<f64>> {
    match g {
        Geometry::Polygon(p) => vec![p],
        Geometry::MultiPolygon(mp) => mp.0.iter().collect(),
        _ => vec![],
    }
}

fn unsigned_area(g: &Geometry<f64>) -> f64 {
    polygons_of(g).iter().map(|p| p.unsigned_area()).sum()
}

/// How far outside the buffered bounds the output reaches, in MVT units of
/// this tile (0 when every vertex is inside).
fn excursion_mvt(g: &Geometry<f64>, c: &Case) -> f64 {
    let b = &c.bounds;
    let unit = c.tile.width() / 4096.0;
    let worst = g
        .coords_iter()
        .map(|p| {
            (b.lng_min - p.x)
                .max(p.x - b.lng_max)
                .max(b.lat_min - p.y)
                .max(p.y - b.lat_max)
        })
        .fold(0.0_f64, f64::max);
    worst / unit
}

/// `geo::Validation` is quadratic on a ring's self-intersection check; past
/// this many vertices (the 316k Antarctica ring returned verbatim at z0) a
/// single call runs for minutes. Such outputs are reported as unchecked
/// rather than scored, and the input's validity is taken as unknown.
const VALIDATION_VERTEX_CAP: usize = 20_000;

fn is_valid(g: &Geometry<f64>) -> Result<(), String> {
    if g.coords_iter().count() > VALIDATION_VERTEX_CAP {
        return Ok(());
    }
    match g {
        Geometry::Polygon(p) => p.check_validation().map_err(|e| e.to_string()),
        Geometry::MultiPolygon(mp) => mp.check_validation().map_err(|e| e.to_string()),
        Geometry::LineString(l) => l.check_validation().map_err(|e| e.to_string()),
        Geometry::MultiLineString(ml) => ml.check_validation().map_err(|e| e.to_string()),
        _ => Ok(()),
    }
}

/// Every exterior ring shares one winding and every hole has the opposite
/// one. Zero-area rings (no winding) are ignored.
fn orientation_consistent(g: &Geometry<f64>) -> bool {
    let polys = polygons_of(g);
    let mut ext: Option<WindingOrder> = None;
    for p in polys {
        if let Some(w) = p.exterior().winding_order() {
            match ext {
                None => ext = Some(w),
                Some(e) if e != w => return false,
                _ => {}
            }
            for h in p.interiors() {
                if let Some(hw) = h.winding_order() {
                    if hw == w {
                        return false;
                    }
                }
            }
        }
    }
    true
}

fn parts_of(g: &Geometry<f64>) -> usize {
    match g {
        Geometry::MultiPolygon(mp) => mp.0.len(),
        Geometry::MultiLineString(ml) => ml.0.len(),
        _ => 1,
    }
}

fn judge(out: Option<Geometry<f64>>, c: &Case) -> Verdict {
    let mut v = Verdict::default();
    match out {
        None => {
            v.empty = true;
            v.valid = true;
            v.simple = true;
            v.orientation_consistent = true;
        }
        Some(g) => {
            v.vertices = g.coords_iter().count();
            v.parts = parts_of(&g);
            v.area = unsigned_area(&g);
            v.excursion_mvt = excursion_mvt(&g, c);
            if v.vertices > VALIDATION_VERTEX_CAP {
                v.valid = true;
                v.validity_unchecked = true;
            } else {
                match is_valid(&g) {
                    Ok(()) => v.valid = true,
                    Err(e) => v.invalid_reason = Some(e),
                }
            }
            v.simple = geometry_is_simple(&g);
            v.orientation_consistent = orientation_consistent(&g);
        }
    }
    v
}

fn run_engine(e: &Engine, g: &Geometry<f64>, c: &Case, simple: bool) -> Verdict {
    let t0 = Instant::now();
    let (res, peak) = measure_peak(|| catch_unwind(AssertUnwindSafe(|| (e.clip)(g, c, simple))));
    let nanos = t0.elapsed().as_nanos() as u64;
    let mut v = match res {
        Ok(out) => judge(out, c),
        Err(p) => Verdict {
            panicked: true,
            error: Some(panic_message(&p)),
            ..Default::default()
        },
    };
    v.nanos = nanos;
    v.peak_bytes = Some(peak);
    v
}

fn panic_message(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "non-string panic".into())
}

/// `geo::BooleanOps` intersection with the buffered rectangle — geo 0.33's
/// vendored i_overlay 4.5, a different version of the incumbent engine — as
/// the third opinion on area. Only meaningful for valid input; it may panic
/// on invalid input, which is caught and reported as `None`.
fn geo_reference_area(g: &Geometry<f64>, c: &Case) -> Option<f64> {
    let b = &c.bounds;
    let rect = Polygon::new(
        LineString::from(vec![
            (b.lng_min, b.lat_min),
            (b.lng_max, b.lat_min),
            (b.lng_max, b.lat_max),
            (b.lng_min, b.lat_max),
            (b.lng_min, b.lat_min),
        ]),
        vec![],
    );
    let r = catch_unwind(AssertUnwindSafe(|| match g {
        Geometry::Polygon(p) => Some(p.intersection(&rect).unsigned_area()),
        Geometry::MultiPolygon(mp) => Some(mp.intersection(&rect).unsigned_area()),
        _ => None,
    }));
    r.ok().flatten()
}

// ============================================================================
// Scoring
// ============================================================================

/// An output vertex further than this outside the buffered bounds, in MVT
/// units, lands on a different quantized position than the boundary and so
/// puts geometry past the buffer into the encoded tile.
const EXCURSION_LIMIT_MVT: f64 = 0.5;

#[derive(Default, Clone)]
struct Tally {
    cases: usize,
    panics: usize,
    errors: usize,
    drops: usize,
    phantoms: usize,
    /// Cases whose output reaches past the buffered bounds by more than
    /// [`EXCURSION_LIMIT_MVT`].
    excursions: usize,
    max_excursion_mvt: f64,
    invalid: usize,
    invalid_reasons: BTreeMap<String, usize>,
    validity_unchecked: usize,
    non_simple: usize,
    mixed_orientation: usize,
    area_mismatch_vs_ref: usize,
    max_rel_area_delta: f64,
    area_over_input: usize,
    vertices: usize,
    nanos: u64,
    peak_bytes: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Panic,
    Error,
    Drop,
    Phantom,
    Excursion,
    Invalid,
    SelfCrossing,
    MixedWinding,
    AreaVsReference,
    AreaOverInput,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Panic => "panic",
            Kind::Error => "error",
            Kind::Drop => "empty where the reference has area",
            Kind::Phantom => "area where the reference is empty",
            Kind::Excursion => "vertex past the buffered bounds by > 0.5 MVT unit",
            Kind::Invalid => "invalid per geo::Validation",
            Kind::SelfCrossing => "proper self-crossing in output",
            Kind::MixedWinding => "mixed ring orientation",
            Kind::AreaVsReference => "area disagrees with the i_overlay reference",
            Kind::AreaOverInput => "output area exceeds (valid) input area",
        }
    }
}

struct Anomaly {
    engine: String,
    case: String,
    kind: Kind,
    detail: String,
}

struct Scoreboard {
    /// engine -> tier -> tally
    tallies: BTreeMap<String, BTreeMap<Tier, Tally>>,
    anomalies: Vec<Anomaly>,
    /// Reference area per case id (i_overlay), and geo's third opinion.
    reference: BTreeMap<String, (f64, Option<f64>)>,
}

/// Relative-area tolerance beyond which two engines "disagree". Loose enough
/// to absorb the integer engines' half-unit snapping at the perimeter.
fn area_tolerance(engine: &str) -> f64 {
    if engine.contains("i64") {
        5e-3
    } else {
        1e-6
    }
}

struct Input {
    area: f64,
    valid: bool,
    /// Bbox span in degrees; sub-1e-9 shapes sit below the absolute 1e-10
    /// duplicate-vertex epsilon in `clip::has_structural_issues`, which
    /// routes them to the overlay and its grid — not an oracle failure.
    span: f64,
}

fn score(board: &mut Scoreboard, engine: &str, tier: Tier, c: &Case, input: &Input, v: &Verdict) {
    let t = board
        .tallies
        .entry(engine.to_string())
        .or_default()
        .entry(tier)
        .or_default();
    t.cases += 1;
    t.nanos += v.nanos;
    t.peak_bytes = t.peak_bytes.max(v.peak_bytes.unwrap_or(0));
    let mut note = |kind: Kind, detail: String| {
        board.anomalies.push(Anomaly {
            engine: engine.to_string(),
            case: c.id.clone(),
            kind,
            detail,
        })
    };
    if v.panicked {
        t.panics += 1;
        note(Kind::Panic, v.error.clone().unwrap_or_default());
        return;
    }
    if let Some(e) = &v.error {
        t.errors += 1;
        note(Kind::Error, e.clone());
        return;
    }
    t.vertices += v.vertices;
    let (ref_area, geo_area) = board
        .reference
        .get(&c.id)
        .copied()
        .unwrap_or((f64::NAN, None));
    let ref_nonempty = ref_area > 0.0;
    if v.empty && ref_nonempty {
        t.drops += 1;
        note(Kind::Drop, format!("reference area {ref_area:.3e}"));
    }
    if !v.empty && v.area > 0.0 && ref_area == 0.0 {
        t.phantoms += 1;
        note(Kind::Phantom, format!("area {:.3e}", v.area));
    }
    t.max_excursion_mvt = t.max_excursion_mvt.max(v.excursion_mvt);
    if v.excursion_mvt > EXCURSION_LIMIT_MVT {
        t.excursions += 1;
        note(Kind::Excursion, format!("{:.2} MVT units", v.excursion_mvt));
    }
    if v.validity_unchecked {
        t.validity_unchecked += 1;
    }
    if !v.valid {
        t.invalid += 1;
        let r = v.invalid_reason.clone().unwrap_or_default();
        *t.invalid_reasons.entry(r.clone()).or_default() += 1;
        note(Kind::Invalid, r);
    }
    if !v.simple {
        t.non_simple += 1;
        note(Kind::SelfCrossing, String::new());
    }
    if !v.orientation_consistent {
        t.mixed_orientation += 1;
        note(Kind::MixedWinding, String::new());
    }
    if !v.empty && ref_area.is_finite() && ref_nonempty {
        let rel = (v.area - ref_area).abs() / ref_area;
        t.max_rel_area_delta = t.max_rel_area_delta.max(rel);
        if rel > area_tolerance(engine) {
            t.area_mismatch_vs_ref += 1;
            let third = geo_area
                .map(|a| format!(", geo says {a:.6e}"))
                .unwrap_or_default();
            note(
                Kind::AreaVsReference,
                format!(
                    "{:.6e} vs {:.6e} (rel {:.2e}{third})",
                    v.area, ref_area, rel
                ),
            );
        }
    }
    // A clip can never add area — but only a valid input has a meaningful
    // area to compare with (a bowtie's shoelace area is zero).
    if input.valid
        && input.span >= 1e-9
        && v.area > input.area * (1.0 + area_tolerance(engine)) + 1e-300
    {
        t.area_over_input += 1;
        note(
            Kind::AreaOverInput,
            format!("{:.6e} > {:.6e}", v.area, input.area),
        );
    }
}

// ============================================================================
// wagyu results (out of process)
// ============================================================================

#[derive(serde::Serialize)]
struct CaseDump<'a> {
    id: &'a str,
    zoom: u8,
    bounds: [f64; 4],
    tile: [f64; 4],
    polygons: Vec<Vec<Vec<[f64; 2]>>>,
}

#[derive(serde::Deserialize)]
struct WagyuOutcome {
    id: String,
    engine: String,
    panicked: bool,
    error: Option<String>,
    nanos: u64,
    polygons: Vec<Vec<Vec<[f64; 2]>>>,
}

fn rings_dump(g: &Geometry<f64>) -> Vec<Vec<Vec<[f64; 2]>>> {
    polygons_of(g)
        .iter()
        .map(|p| {
            std::iter::once(p.exterior())
                .chain(p.interiors().iter())
                .map(|r| r.0.iter().map(|c| [c.x, c.y]).collect())
                .collect()
        })
        .collect()
}

fn geometry_from_dump(polys: Vec<Vec<Vec<[f64; 2]>>>) -> Option<Geometry<f64>> {
    let ps: Vec<Polygon<f64>> = polys
        .into_iter()
        .filter_map(|rings| {
            let mut it = rings
                .into_iter()
                .map(|r| LineString::new(r.into_iter().map(|[x, y]| Coord { x, y }).collect()));
            let ext = it.next()?;
            Some(Polygon::new(ext, it.collect()))
        })
        .collect();
    match ps.len() {
        0 => None,
        1 => Some(Geometry::Polygon(ps.into_iter().next().unwrap())),
        _ => Some(Geometry::MultiPolygon(MultiPolygon::new(ps))),
    }
}

fn write_case_dump(fixtures: &[Fixture], cases: &[Case]) {
    let path = out_dir().join("cases.jsonl");
    let mut f = std::io::BufWriter::new(fs::File::create(&path).unwrap());
    for c in cases {
        let g = &fixtures[c.fixture].geom;
        if polygons_of(g).is_empty() {
            continue;
        }
        let b = &c.bounds;
        let t = &c.tile;
        let d = CaseDump {
            id: &c.id,
            zoom: c.zoom,
            bounds: [b.lng_min, b.lat_min, b.lng_max, b.lat_max],
            tile: [t.lng_min, t.lat_min, t.lng_max, t.lat_max],
            polygons: rings_dump(g),
        };
        serde_json::to_writer(&mut f, &d).unwrap();
        f.write_all(b"\n").unwrap();
    }
}

fn read_wagyu_results() -> Option<Vec<WagyuOutcome>> {
    let path = out_dir().join("wagyu_results.jsonl");
    let text = fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("wagyu_results.jsonl line"))
            .collect(),
    )
}

// ============================================================================
// Sweep
// ============================================================================

struct Run {
    fixtures: Vec<Fixture>,
    cases: Vec<Case>,
    skipped: BTreeMap<String, CorpusSkip>,
    board: Scoreboard,
    input_status: BTreeMap<String, String>,
    engines_seen: Vec<String>,
}

fn sweep(
    fixtures: Vec<Fixture>,
    skipped: BTreeMap<String, CorpusSkip>,
    tiles_per_zoom: impl Fn(Tier) -> usize,
) -> Run {
    let cases = cases_for(&fixtures, tiles_per_zoom);
    let mut board = Scoreboard {
        tallies: BTreeMap::new(),
        anomalies: Vec::new(),
        reference: BTreeMap::new(),
    };
    let mut input_status = BTreeMap::new();
    let simple: Vec<bool> = fixtures
        .iter()
        .map(|f| geometry_is_simple(&f.geom))
        .collect();
    let inputs: Vec<Input> = fixtures
        .iter()
        .map(|f| Input {
            area: unsigned_area(&f.geom),
            // Oversized inputs are "unknown", which disables the
            // valid-input-only oracles rather than trusting the cap.
            valid: f.geom.coords_iter().count() <= VALIDATION_VERTEX_CAP
                && is_valid(&f.geom).is_ok(),
            span: finite_bbox(&f.geom)
                .map(|b| b.width().max(b.height()))
                .unwrap_or(0.0),
        })
        .collect();
    for (i, f) in fixtures.iter().enumerate() {
        let status = if f.geom.coords_iter().count() > VALIDATION_VERTEX_CAP {
            "unchecked (over the validation vertex cap)".to_string()
        } else {
            match is_valid(&f.geom) {
                Ok(()) => "valid".to_string(),
                Err(e) => format!("invalid: {e}"),
            }
        };
        input_status.insert(f.name.clone(), status);
        let _ = i;
    }

    // Reference pass: i_overlay area per case, plus geo's third opinion on
    // valid input.
    for c in &cases {
        let g = &fixtures[c.fixture].geom;
        let ref_area = catch_unwind(AssertUnwindSafe(|| engine_ioverlay(g, c, false)))
            .ok()
            .flatten()
            .map(|o| unsigned_area(&o))
            .unwrap_or(0.0);
        let geo_area = if inputs[c.fixture].valid {
            geo_reference_area(g, c)
        } else {
            None
        };
        board.reference.insert(c.id.clone(), (ref_area, geo_area));
    }

    let mut engines_seen: Vec<String> = Vec::new();
    for e in ENGINES {
        engines_seen.push(e.name.to_string());
        for c in &cases {
            let f = &fixtures[c.fixture];
            if e.polygons_only && polygons_of(&f.geom).is_empty() {
                continue;
            }
            let v = run_engine(e, &f.geom, c, simple[c.fixture]);
            score(&mut board, e.name, f.tier, c, &inputs[c.fixture], &v);
        }
    }

    write_case_dump(&fixtures, &cases);

    if let Some(results) = read_wagyu_results() {
        let by_id: BTreeMap<&str, &Case> = cases.iter().map(|c| (c.id.as_str(), c)).collect();
        let mut matched = 0usize;
        for r in results {
            let Some(c) = by_id.get(r.id.as_str()) else {
                continue;
            };
            matched += 1;
            if !engines_seen.contains(&r.engine) {
                engines_seen.push(r.engine.clone());
            }
            let f = &fixtures[c.fixture];
            let mut v = if r.panicked {
                Verdict {
                    panicked: true,
                    error: r.error.clone(),
                    ..Default::default()
                }
            } else if let Some(err) = r.error.clone() {
                Verdict {
                    error: Some(err),
                    ..Default::default()
                }
            } else {
                judge(geometry_from_dump(r.polygons), c)
            };
            v.nanos = r.nanos;
            v.peak_bytes = None;
            score(&mut board, &r.engine, f.tier, c, &inputs[c.fixture], &v);
        }
        eprintln!("hostile_geometry_eval: folded in {matched} wagyu outcomes");
    } else {
        eprintln!(
            "hostile_geometry_eval: no wagyu_results.jsonl — run \
             `cargo run --release --manifest-path corpus/hostile_wagyu/Cargo.toml` \
             and re-run to add the wagyu columns"
        );
    }

    Run {
        fixtures,
        cases,
        skipped,
        board,
        input_status,
        engines_seen,
    }
}

// ============================================================================
// Scorecard
// ============================================================================

fn fmt_bytes(b: usize) -> String {
    if b >= 1 << 20 {
        format!("{:.1} MiB", b as f64 / (1u64 << 20) as f64)
    } else if b >= 1 << 10 {
        format!("{:.1} KiB", b as f64 / 1024.0)
    } else {
        format!("{b} B")
    }
}

fn scorecard(run: &Run, title: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "## {title}\n");
    let _ = writeln!(
        s,
        "{} fixtures ({} corpus, {} synthetic, {} real), {} cases (fixture × zoom × tile), {} corpus files without usable geometry.\n",
        run.fixtures.len(),
        run.fixtures.iter().filter(|f| f.tier == Tier::Corpus).count(),
        run.fixtures.iter().filter(|f| f.tier == Tier::Synthetic).count(),
        run.fixtures.iter().filter(|f| f.tier == Tier::Real).count(),
        run.cases.len(),
        run.skipped.len(),
    );

    for tier in [Tier::Corpus, Tier::Synthetic, Tier::Real] {
        let any = run.board.tallies.values().any(|m| m.contains_key(&tier));
        if !any {
            continue;
        }
        let _ = writeln!(s, "### {}\n", tier.label());
        let _ = writeln!(
            s,
            "| engine | cases | panics | errors | drops | phantoms | past buffer >½ MVT | max excursion (MVT) | invalid (geo) | unchecked | self-crossing | mixed winding | area≠ref | max rel Δarea | area>input | vertices | time | peak heap |"
        );
        let _ = writeln!(
            s,
            "|---|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|--:|"
        );
        for name in &run.engines_seen {
            let Some(t) = run.board.tallies.get(name).and_then(|m| m.get(&tier)) else {
                continue;
            };
            let _ = writeln!(
                s,
                "| {} | {} | {} | {} | {} | {} | {} | {:.2} | {} | {} | {} | {} | {} | {:.1e} | {} | {} | {:.1} ms | {} |",
                name,
                t.cases,
                t.panics,
                t.errors,
                t.drops,
                t.phantoms,
                t.excursions,
                t.max_excursion_mvt,
                t.invalid,
                t.validity_unchecked,
                t.non_simple,
                t.mixed_orientation,
                t.area_mismatch_vs_ref,
                t.max_rel_area_delta,
                t.area_over_input,
                t.vertices,
                t.nanos as f64 / 1e6,
                if name.starts_with("wagyu") {
                    "n/a".to_string()
                } else {
                    fmt_bytes(t.peak_bytes)
                },
            );
        }
        let _ = writeln!(s);
        // Invalidity reasons per engine, this tier.
        for name in &run.engines_seen {
            let Some(t) = run.board.tallies.get(name).and_then(|m| m.get(&tier)) else {
                continue;
            };
            if t.invalid_reasons.is_empty() {
                continue;
            }
            let reasons: Vec<String> = t
                .invalid_reasons
                .iter()
                .map(|(r, n)| format!("{n}× {r}"))
                .collect();
            let _ = writeln!(s, "- `{name}` invalid outputs: {}", reasons.join("; "));
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "### Inputs\n");
    let mut n_valid = 0;
    let mut invalid: BTreeMap<String, usize> = BTreeMap::new();
    for st in run.input_status.values() {
        if st == "valid" {
            n_valid += 1;
        } else {
            *invalid.entry(st.clone()).or_default() += 1;
        }
    }
    let _ = writeln!(
        s,
        "{n_valid} inputs valid per `geo::Validation`; invalid: {}\n",
        invalid
            .iter()
            .map(|(r, n)| format!("{n}× {r}"))
            .collect::<Vec<_>>()
            .join("; ")
    );
    if !run.skipped.is_empty() {
        let _ = writeln!(s, "Corpus files without usable geometry:\n");
        for (f, why) in &run.skipped {
            let _ = writeln!(s, "- `{f}`: {why}");
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "### Anomalies\n");
    if run.board.anomalies.is_empty() {
        let _ = writeln!(s, "none\n");
    } else {
        // Group by (engine, kind) so the list stays readable; show a few
        // examples with their detail.
        let mut grouped: BTreeMap<(String, Kind), Vec<(String, String)>> = BTreeMap::new();
        for a in &run.board.anomalies {
            grouped
                .entry((a.engine.clone(), a.kind))
                .or_default()
                .push((a.case.clone(), a.detail.clone()));
        }
        for ((engine, kind), cases) in grouped {
            let shown: Vec<String> = cases
                .iter()
                .take(3)
                .map(|(c, d)| {
                    if d.is_empty() {
                        format!("`{c}`")
                    } else {
                        format!("`{c}` ({d})")
                    }
                })
                .collect();
            let more = if cases.len() > 3 {
                format!(" … +{} more", cases.len() - 3)
            } else {
                String::new()
            };
            let _ = writeln!(
                s,
                "- `{engine}` — {} — {} case(s): {}{more}",
                kind.label(),
                cases.len(),
                shown.join(", ")
            );
        }
        let _ = writeln!(s);
    }
    s
}

// ============================================================================
// Production invariants
// ============================================================================

/// What the export path must hold for every input it can meet. Anything
/// looser than this is a real bug, not an evaluation finding.
fn assert_production_invariants(run: &Run) {
    let mut failures = Vec::new();
    for tier in [Tier::Corpus, Tier::Synthetic, Tier::Real] {
        for name in ["production", "production-strict"] {
            let Some(t) = run.board.tallies.get(name).and_then(|m| m.get(&tier)) else {
                continue;
            };
            if t.panics > 0 {
                failures.push(format!("{name}/{}: {} panics", tier.label(), t.panics));
            }
            if t.excursions > 0 {
                failures.push(format!(
                    "{name}/{}: {} outputs reach past the buffered bounds by > {EXCURSION_LIMIT_MVT} MVT unit (max {:.2})",
                    tier.label(),
                    t.excursions,
                    t.max_excursion_mvt
                ));
            }
            if t.area_over_input > 0 {
                failures.push(format!(
                    "{name}/{}: {} outputs larger than their input",
                    tier.label(),
                    t.area_over_input
                ));
            }
        }
    }
    // The production output of a simple input must itself be free of proper
    // self-crossings: S-H on a simple ring against a convex window yields at
    // most self-touching output, and the fallback resolves crossings.
    let simple_inputs: Vec<&str> = run
        .fixtures
        .iter()
        .filter(|f| geometry_is_simple(&f.geom))
        .map(|f| f.name.as_str())
        .collect();
    for a in &run.board.anomalies {
        if (a.engine == "production" || a.engine == "production-strict")
            && a.kind == Kind::SelfCrossing
        {
            let fixture = a.case.split('|').next().unwrap_or("");
            if simple_inputs.contains(&fixture) {
                failures.push(format!(
                    "{}: {} on simple input {}",
                    a.engine,
                    a.kind.label(),
                    a.case
                ));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "production clip invariants violated:\n  {}",
        failures.join("\n  ")
    );
}

// ============================================================================
// Tests
// ============================================================================

/// Quick tier: corpus + synthetic suite, capped tiles, every engine.
#[test]
fn smoke_scorecard_and_production_invariants() {
    let (mut fixtures, skipped) = load_corpus();
    fixtures.extend(synthetic_suite());
    let run = sweep(fixtures, skipped, |_| 12);
    let md = scorecard(&run, "Smoke (corpus + synthetic, ≤12 tiles per zoom)");
    fs::write(out_dir().join("SCORECARD-smoke.md"), &md).unwrap();
    println!("{md}");
    assert!(
        run.cases.len() > 100,
        "sweep produced only {} cases",
        run.cases.len()
    );
    assert_production_invariants(&run);
}

/// The corpus must actually be loaded: the submodule's five folders and the
/// files the harness is written against. A silent 0-fixture sweep would pass
/// the invariants vacuously.
#[test]
fn corpus_is_present_and_parsed() {
    let (fixtures, skipped) = load_corpus();
    let names: Vec<&str> = fixtures.iter().map(|f| f.name.as_str()).collect();
    for expected in [
        "problematic_geometries/problematic_self_intersection_large.geojson",
        "problematic_geometries/problematic_self_intersection_small.geojson",
        "invalid_geometries/invalid_inner_and_exterior_ring_intersect.geojson",
        "problematic_geometries/problematic_crosses_antimeridian.geojson",
        "valid/valid_geometry_multipolygon.geojson",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} not loaded; got {names:?}"
        );
    }
    // Structural fixtures are recorded, not lost.
    assert!(
        skipped.keys().any(|k| k.starts_with("invalid_structure/")),
        "expected invalid_structure files among the skipped set: {skipped:?}"
    );
}

/// Full tier (nextest slow set): everything the smoke test runs plus the
/// real clipper-stressing fixtures, more tiles per zoom for the small
/// corpora and a handful for the 316k-vertex ring. Writes
/// `target/hostile_geometry_eval/SCORECARD.md`, the file
/// `corpus/HOSTILE_GEOMETRY.md` snapshots.
#[test]
fn full_scorecard() {
    let (mut fixtures, skipped) = load_corpus();
    fixtures.extend(synthetic_suite());
    fixtures.extend(real_suite());
    let run = sweep(fixtures, skipped, |tier| match tier {
        Tier::Real => 4,
        _ => 24,
    });
    let md = scorecard(
        &run,
        "Full (corpus + synthetic ≤24 tiles per zoom, real ≤4)",
    );
    fs::write(out_dir().join("SCORECARD.md"), &md).unwrap();
    println!("{md}");
    assert_production_invariants(&run);
}
