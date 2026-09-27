//! wagyu-rs runner for the #205 hostile-geometry evaluation.
//!
//! Reads the case dump the in-tree harness writes
//! (`target/hostile_geometry_eval/cases.jsonl`), clips every polygon case with
//! wagyu-rs in two integer coordinate spaces, and writes
//! `target/hostile_geometry_eval/wagyu_results.jsonl` beside it. The harness
//! then scores those results with the SAME oracles it applies to the in-tree
//! engines, so this binary only clips — it never judges.
//!
//! Coordinate spaces (one engine column each):
//! - `wagyu-i64-mvt`: the tile's MVT grid (4096 units across the unbuffered
//!   tile), rounded to integers — the "clip in the quantization space" case
//!   #205 argues for.
//! - `wagyu-i64-world`: tippecanoe's clip space, `2^(32-z)` units across the
//!   tile (world coordinates at 32-bit precision, `clip.cpp`).
//!
//! There is deliberately no degrees-as-is `Wagyu<f64>` column: wagyu-rs
//! snap-rounds f64 coordinates to integers internally, so sub-degree input
//! collapses (pinned in `tests/adapter_sanity.rs`). Scoring that would score
//! a misuse, not the engine.
//!
//! Fill rule: EvenOdd for subject and clip, the interpretation the incumbent
//! (`ioverlay_clip.rs`) applies, so the two engines are compared on the same
//! semantics. tippecanoe runs wagyu with positive fill after forcing ring
//! orientation; that is a different contract for unoriented input (a CW
//! exterior is empty under positive fill), so it is not what is scored here.
//!
//! Run: `cargo run --release --manifest-path corpus/hostile_wagyu/Cargo.toml`
//! after `cargo test -p tylertoo-core --test hostile_geometry_eval`, then
//! re-run that test to fold the wagyu columns into the scorecard.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use wagyu_rs::{FillType, Operation, Point, PolygonType, Wagyu};

#[derive(Deserialize)]
struct Case {
    id: String,
    zoom: u8,
    /// Buffered clip bounds `[lng_min, lat_min, lng_max, lat_max]`.
    bounds: [f64; 4],
    /// Unbuffered tile bounds `[lng_min, lat_min, lng_max, lat_max]`.
    tile: [f64; 4],
    /// `Vec<polygon>`, each `Vec<ring>`, each `Vec<[x, y]>` (closed rings).
    polygons: Vec<Vec<Vec<[f64; 2]>>>,
}

#[derive(Serialize)]
struct Outcome {
    id: String,
    engine: &'static str,
    panicked: bool,
    error: Option<String>,
    nanos: u64,
    /// Same shape as `Case::polygons`, in degrees.
    polygons: Vec<Vec<Vec<[f64; 2]>>>,
}

/// Affine map degrees -> engine space and back.
struct Space {
    ox: f64,
    oy: f64,
    sx: f64,
    sy: f64,
}

impl Space {
    fn tile_grid(tile: &[f64; 4], units: f64) -> Self {
        Space {
            ox: tile[0],
            oy: tile[1],
            sx: units / (tile[2] - tile[0]),
            sy: units / (tile[3] - tile[1]),
        }
    }
    fn fwd(&self, p: [f64; 2]) -> [f64; 2] {
        [(p[0] - self.ox) * self.sx, (p[1] - self.oy) * self.sy]
    }
    fn back(&self, p: [f64; 2]) -> [f64; 2] {
        [p[0] / self.sx + self.ox, p[1] / self.sy + self.oy]
    }
}

fn run_i64(case: &Case, space: &Space) -> Result<Vec<Vec<Vec<[f64; 2]>>>, String> {
    let mut w: Wagyu<i64> = Wagyu::new();
    for poly in &case.polygons {
        for ring in poly {
            let pts: Vec<Point<i64>> = open_ring(ring)
                .map(|p| {
                    let q = space.fwd(p);
                    Point::new(q[0].round() as i64, q[1].round() as i64)
                })
                .collect();
            w.add_ring(&pts, PolygonType::Subject);
        }
    }
    let b: Vec<Point<i64>> = clip_box_f64(&case.bounds, space)
        .into_iter()
        .map(|p| Point::new(p.x.round() as i64, p.y.round() as i64))
        .collect();
    w.add_ring(&b, PolygonType::Clip);
    let mp = w
        .execute(
            Operation::Intersection,
            FillType::EvenOdd,
            FillType::EvenOdd,
        )
        .map_err(|e| e.to_string())?;
    Ok(mp
        .0
        .iter()
        .map(|p| {
            std::iter::once(p.exterior())
                .chain(p.interiors().iter())
                .map(|r| {
                    r.0.iter()
                        .map(|c| space.back([c.x as f64, c.y as f64]))
                        .collect()
                })
                .collect()
        })
        .collect())
}

/// The ring without its closing vertex (wagyu closes implicitly).
fn open_ring(ring: &[[f64; 2]]) -> impl Iterator<Item = [f64; 2]> + '_ {
    let n = ring.len();
    let closed = n >= 2 && ring[0] == ring[n - 1];
    ring.iter().copied().take(if closed { n - 1 } else { n })
}

fn clip_box_f64(b: &[f64; 4], space: &Space) -> Vec<Point<f64>> {
    [[b[0], b[1]], [b[2], b[1]], [b[2], b[3]], [b[0], b[3]]]
        .into_iter()
        .map(|p| {
            let q = space.fwd(p);
            Point::new(q[0], q[1])
        })
        .collect()
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/hostile_geometry_eval")
        });
    let cases_path = dir.join("cases.jsonl");
    let out_path = dir.join("wagyu_results.jsonl");
    let reader = BufReader::new(File::open(&cases_path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}. Run `cargo test -p tylertoo-core --test hostile_geometry_eval` first.",
            cases_path.display()
        )
    }));
    let mut out = BufWriter::new(File::create(&out_path).expect("create wagyu_results.jsonl"));

    let mut n = 0usize;
    let mut panics = 0usize;
    for line in reader.lines() {
        let line = line.expect("read line");
        if line.trim().is_empty() {
            continue;
        }
        let case: Case = serde_json::from_str(&line).expect("parse case");
        let world_units = 2f64.powi(32 - i32::from(case.zoom));
        let spaces: [(&'static str, Space); 2] = [
            ("wagyu-i64-mvt", Space::tile_grid(&case.tile, 4096.0)),
            ("wagyu-i64-world", Space::tile_grid(&case.tile, world_units)),
        ];
        for (engine, space) in &spaces {
            let t0 = Instant::now();
            let res = catch_unwind(AssertUnwindSafe(|| run_i64(&case, space)));
            let nanos = t0.elapsed().as_nanos() as u64;
            let outcome = match res {
                Ok(Ok(polygons)) => Outcome {
                    id: case.id.clone(),
                    engine,
                    panicked: false,
                    error: None,
                    nanos,
                    polygons,
                },
                Ok(Err(e)) => Outcome {
                    id: case.id.clone(),
                    engine,
                    panicked: false,
                    error: Some(e),
                    nanos,
                    polygons: vec![],
                },
                Err(p) => {
                    panics += 1;
                    let msg = p
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "non-string panic".into());
                    Outcome {
                        id: case.id.clone(),
                        engine,
                        panicked: true,
                        error: Some(msg),
                        nanos,
                        polygons: vec![],
                    }
                }
            };
            serde_json::to_writer(&mut out, &outcome).expect("write outcome");
            out.write_all(b"\n").expect("write newline");
        }
        n += 1;
    }
    out.flush().expect("flush");
    eprintln!(
        "hostile-wagyu: {n} cases x 2 spaces -> {} ({panics} panics caught)",
        out_path.display()
    );
}
