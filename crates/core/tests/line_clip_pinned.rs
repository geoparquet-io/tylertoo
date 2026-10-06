//! Pinned line-clipping outputs (#435).
//!
//! Line clipping used to go through `geo::BooleanOps::clip` — the copy of
//! `i_overlay` 4.5.2 that `geo` 0.33 vendors — while polygon clipping already
//! ran on our direct `i_overlay` 9 (`ioverlay_clip.rs`). #435 moves the line
//! path onto the same engine. These tests pin the outputs the `geo` path
//! produced, so the move is verifiably behaviour-preserving: every expected
//! value below was captured from `main` before the switch, through the public
//! `clip::clip_geometry` entry point that export uses.
//!
//! Two layers:
//! 1. Hand-built cases with literal expected coordinates, covering the
//!    situations where two clippers could plausibly disagree (bounds crossed
//!    twice, corner touches, edge-collinear runs, degenerate segments,
//!    boundary vertices, wholly outside within the bbox).
//! 2. A digest sweep over the real `road-detections` fixture (2 zooms of
//!    tiles, buffered like export), pinned by feature/part/vertex counts and
//!    an xxh3 over every output coordinate's bits.
//!
//! If a change to the line clipper moves any of these, the PR must say why
//! (see `context/ARCHITECTURE.md`, "One boolean-ops engine").
//!
//! Re-pin history:
//! - #435 (the switch itself): every hand-built case stayed coordinate-
//!   identical. The two sweep digests moved — same kept/part/vertex counts,
//!   but ~half the vertices differ by exactly one unit of `i_overlay`'s
//!   float→integer grid (2^-34..2^-36 degrees, max 2.3e-10), four to five
//!   orders of magnitude under an MVT unit at z14. The digests below are the
//!   i_overlay-9 values; the pre-switch ones were
//!   z12 `0xbb55_0c07_303c_8bbe` and z14 `0xd28a_ce50_39c3_0db2`.
//!
//! Run with:
//!   cargo test --package tylertoo-core --test `line_clip_pinned` -- --nocapture

use std::path::PathBuf;

use geo::{CoordsIter, Geometry, LineString, MultiLineString};
use geoarrow::array::from_arrow_array;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

use tylertoo_core::batch_processor::extract_geometries_from_array;
use tylertoo_core::clip::{buffer_pixels_to_degrees, clip_geometry};
use tylertoo_core::tile::{TileBounds, TileCoord};

// ============================================================================
// Helpers
// ============================================================================

fn ls(pts: &[(f64, f64)]) -> Geometry<f64> {
    Geometry::LineString(LineString::from(pts.to_vec()))
}

fn mls(parts: &[&[(f64, f64)]]) -> Geometry<f64> {
    Geometry::MultiLineString(MultiLineString::new(
        parts.iter().map(|p| LineString::from(p.to_vec())).collect(),
    ))
}

/// Flatten a clip result to its parts, as coordinate pairs, so a single
/// `LineString` and a one-part `MultiLineString` compare by content.
fn parts(g: &Option<Geometry<f64>>) -> Option<Vec<Vec<(f64, f64)>>> {
    let g = g.as_ref()?;
    Some(match g {
        Geometry::LineString(l) => vec![l.0.iter().map(|c| (c.x, c.y)).collect()],
        Geometry::MultiLineString(m) => {
            m.0.iter()
                .map(|l| l.0.iter().map(|c| (c.x, c.y)).collect())
                .collect()
        }
        other => panic!("line clip returned a non-line geometry: {other:?}"),
    })
}

/// The unit square, the bounds every hand-built case clips against.
fn unit() -> TileBounds {
    TileBounds::new(0.0, 0.0, 10.0, 10.0)
}

fn check(name: &str, input: Geometry<f64>, expected: Option<Vec<Vec<(f64, f64)>>>) {
    let got = clip_geometry(&input, &unit(), 0.0);
    let got_parts = parts(&got);
    eprintln!("{name}: {got_parts:?}");
    assert_eq!(got_parts, expected, "{name}");
    // The geometry *type* is part of the contract: one part is a LineString,
    // several parts a MultiLineString, regardless of the input type.
    if let Some(p) = &expected {
        match (&got, p.len()) {
            (Some(Geometry::LineString(_)), 1) => {}
            (Some(Geometry::MultiLineString(_)), n) if n > 1 => {}
            (Some(Geometry::MultiLineString(_)), 1)
                if matches!(input, Geometry::MultiLineString(_)) => {}
            (g, n) => panic!("{name}: {n} part(s) came back as {g:?}"),
        }
    }
}

// ============================================================================
// Hand-built cases (expected values captured from geo::BooleanOps::clip)
// ============================================================================

#[test]
fn line_crossing_both_sides_is_trimmed_to_the_bounds() {
    check(
        "crossing",
        ls(&[(-5.0, 5.0), (15.0, 5.0)]),
        Some(vec![vec![(0.0, 5.0), (10.0, 5.0)]]),
    );
}

#[test]
fn line_fully_inside_is_returned_as_is() {
    check(
        "inside",
        ls(&[(2.0, 2.0), (8.0, 8.0)]),
        Some(vec![vec![(2.0, 2.0), (8.0, 8.0)]]),
    );
}

#[test]
fn line_exiting_and_re_entering_is_split_at_the_bounds() {
    // Enters at the left edge, leaves through the top, comes back through
    // the top, leaves through the right edge: a U over the top edge.
    check(
        "re-entering",
        ls(&[
            (-5.0, 5.0),
            (3.0, 5.0),
            (3.0, 15.0),
            (7.0, 15.0),
            (7.0, 5.0),
            (15.0, 5.0),
        ]),
        // Captured quirk: the re-entering run comes back broken at its
        // interior vertex (7,5) — two parts where one would do.
        Some(vec![
            vec![(0.0, 5.0), (3.0, 5.0), (3.0, 10.0)],
            vec![(7.0, 10.0), (7.0, 5.0)],
            vec![(7.0, 5.0), (10.0, 5.0)],
        ]),
    );
}

#[test]
fn line_touching_a_corner_from_outside_yields_nothing_kept() {
    // Bbox intersects the bounds but the line only grazes corner (0,0).
    check("corner-graze", ls(&[(-5.0, 5.0), (5.0, -5.0)]), None);
}

#[test]
fn line_through_a_corner_into_the_interior() {
    check(
        "corner-through",
        ls(&[(-5.0, -5.0), (5.0, 5.0)]),
        Some(vec![vec![(0.0, 0.0), (5.0, 5.0)]]),
    );
}

#[test]
fn line_collinear_with_an_edge_is_kept_on_the_boundary() {
    // boundary_included = true in the geo path: a run along the bottom edge
    // survives, trimmed to the corners.
    check(
        "edge-collinear",
        ls(&[(-5.0, 0.0), (15.0, 0.0)]),
        Some(vec![vec![(0.0, 0.0), (10.0, 0.0)]]),
    );
}

#[test]
fn line_with_a_vertex_exactly_on_the_boundary() {
    check(
        "boundary-vertex",
        ls(&[(5.0, 5.0), (10.0, 5.0), (15.0, 5.0)]),
        Some(vec![vec![(5.0, 5.0), (10.0, 5.0)]]),
    );
}

#[test]
fn line_ending_exactly_on_the_boundary_from_inside() {
    check(
        "ends-on-boundary",
        ls(&[(5.0, 5.0), (10.0, 5.0)]),
        Some(vec![vec![(5.0, 5.0), (10.0, 5.0)]]),
    );
}

#[test]
fn zero_length_segment_inside_the_bounds() {
    check(
        "zero-length-inside",
        ls(&[(5.0, 5.0), (5.0, 5.0)]),
        // Captured: a zero-length line is dropped, not kept as a point-line.
        None,
    );
}

#[test]
fn zero_length_segment_outside_the_bounds() {
    check(
        "zero-length-outside",
        ls(&[(20.0, 20.0), (20.0, 20.0)]),
        None,
    );
}

#[test]
fn repeated_consecutive_vertices_across_the_boundary() {
    check(
        "repeated-vertices",
        ls(&[
            (-5.0, 5.0),
            (-5.0, 5.0),
            (5.0, 5.0),
            (5.0, 5.0),
            (15.0, 5.0),
        ]),
        // Captured: consecutive duplicate vertices are collapsed.
        Some(vec![vec![(0.0, 5.0), (5.0, 5.0), (10.0, 5.0)]]),
    );
}

#[test]
fn line_inside_the_bbox_but_outside_the_bounds() {
    // Diagonal across the outside of corner (10,10): bbox overlaps, line
    // does not.
    check("outside-within-bbox", ls(&[(8.0, 14.0), (14.0, 8.0)]), None);
}

#[test]
fn multi_line_string_parts_are_clipped_independently() {
    check(
        "mls-mixed",
        mls(&[
            &[(-5.0, 2.0), (15.0, 2.0)],   // crossing
            &[(2.0, 4.0), (8.0, 4.0)],     // inside
            &[(20.0, 20.0), (30.0, 30.0)], // outside
            &[
                (-5.0, 6.0),
                (3.0, 6.0),
                (3.0, 15.0),
                (7.0, 15.0),
                (7.0, 6.0),
                (15.0, 6.0),
            ], // splits
        ]),
        // Captured: parts do not come back in input order (the engine
        // emits them in its own sweep order), and the re-entering run is
        // broken at its interior vertex as in the single-line case.
        Some(vec![
            vec![(0.0, 2.0), (10.0, 2.0)],
            vec![(0.0, 6.0), (3.0, 6.0), (3.0, 10.0)],
            vec![(2.0, 4.0), (8.0, 4.0)],
            vec![(7.0, 10.0), (7.0, 6.0)],
            vec![(7.0, 6.0), (10.0, 6.0)],
        ]),
    );
}

#[test]
fn multi_line_string_with_a_single_surviving_part_stays_multi() {
    check(
        "mls-one-survivor",
        mls(&[&[(20.0, 20.0), (30.0, 30.0)], &[(2.0, 4.0), (8.0, 4.0)]]),
        Some(vec![vec![(2.0, 4.0), (8.0, 4.0)]]),
    );
}

#[test]
fn multi_line_string_entirely_outside_is_dropped() {
    check(
        "mls-outside",
        mls(&[
            &[(20.0, 20.0), (30.0, 30.0)],
            &[(-20.0, -20.0), (-30.0, -30.0)],
        ]),
        None,
    );
}

#[test]
fn line_crossing_at_an_angle_interpolates_the_boundary_point() {
    // Non-axis-aligned exit: the boundary vertex is an interpolation, so
    // it exercises the engine's float round-trip, not just its topology.
    check(
        "angled-exit",
        ls(&[(2.0, 2.0), (14.0, 8.0)]),
        Some(vec![vec![(2.0, 2.0), (10.0, 6.0)]]),
    );
}

#[test]
fn buffered_bounds_are_applied_to_lines() {
    // Through the public API with a buffer: the clip box is widened by it.
    let got = clip_geometry(&ls(&[(-5.0, 5.0), (15.0, 5.0)]), &unit(), 1.0);
    assert_eq!(
        parts(&got),
        Some(vec![vec![(-1.0, 5.0), (11.0, 5.0)]]),
        "buffered"
    );
}

// ============================================================================
// Real-data sweep: road-detections
// ============================================================================

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/realdata/road-detections.parquet")
}

fn read_geometries(path: &PathBuf) -> Vec<Geometry<f64>> {
    let file = std::fs::File::open(path).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let mut geoms = Vec::new();
    for batch in builder.build().unwrap() {
        let batch = batch.unwrap();
        let schema = batch.schema();
        let gidx = schema.index_of("geometry").unwrap();
        let garr = from_arrow_array(batch.column(gidx).as_ref(), schema.field(gidx)).unwrap();
        extract_geometries_from_array(garr.as_ref(), &mut geoms).unwrap();
    }
    geoms
}

fn lnglat_to_tile(lng: f64, lat: f64, z: u8) -> (u32, u32) {
    let n = 2_f64.powi(z as i32);
    let x = ((lng + 180.0) / 360.0 * n).floor();
    let lat_rad = lat.to_radians();
    let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n)
        .floor();
    (x.max(0.0) as u32, y.max(0.0) as u32)
}

#[derive(Debug, PartialEq, Eq)]
struct Sweep {
    /// (feature, tile) pairs whose clip returned `Some`.
    kept: u64,
    /// Line parts over all kept results.
    parts: u64,
    /// Vertices over all kept results.
    vertices: u64,
    /// xxh3-64 over every output coordinate's f64 bits, in order.
    digest: u64,
}

/// Clip every line feature against every tile of `zoom` that its bbox
/// touches, buffered by export's default 8 px at extent 4096.
fn sweep(zoom: u8) -> Sweep {
    let geoms = read_geometries(&fixture_path());
    assert!(!geoms.is_empty(), "fixture is empty");
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    let (mut kept, mut n_parts, mut vertices) = (0u64, 0u64, 0u64);
    for geom in &geoms {
        assert!(
            matches!(geom, Geometry::LineString(_) | Geometry::MultiLineString(_)),
            "road-detections is a line fixture"
        );
        let mut xs = (f64::INFINITY, f64::NEG_INFINITY);
        let mut ys = (f64::INFINITY, f64::NEG_INFINITY);
        for c in geom.coords_iter() {
            xs = (xs.0.min(c.x), xs.1.max(c.x));
            ys = (ys.0.min(c.y), ys.1.max(c.y));
        }
        let (x0, y0) = lnglat_to_tile(xs.0, ys.1, zoom);
        let (x1, y1) = lnglat_to_tile(xs.1, ys.0, zoom);
        for x in x0..=x1 {
            for y in y0..=y1 {
                let bounds = TileCoord::new(x, y, zoom).bounds();
                let buffer = buffer_pixels_to_degrees(8, &bounds, 4096);
                let Some(clipped) = clip_geometry(geom, &bounds, buffer) else {
                    continue;
                };
                kept += 1;
                let p = parts(&Some(clipped)).unwrap();
                n_parts += p.len() as u64;
                for part in &p {
                    hasher.update(&(part.len() as u64).to_le_bytes());
                    for (cx, cy) in part {
                        vertices += 1;
                        hasher.update(&cx.to_bits().to_le_bytes());
                        hasher.update(&cy.to_bits().to_le_bytes());
                    }
                }
            }
        }
    }
    let s = Sweep {
        kept,
        parts: n_parts,
        vertices,
        digest: hasher.digest(),
    };
    eprintln!("z{zoom}: {s:?} (digest {:#018x})", s.digest);
    s
}

/// Counts are pinned on every platform. The coordinate digest is pinned on
/// Linux only: tile bounds come from `sinh().atan()` (libm), which differs
/// by an ulp between platforms, and that ulp reaches the clipped boundary
/// intersections (macOS: z14 digest `0x1d03_35aa_b999_f979` with identical
/// counts). A grid-rounded digest was rejected: a 1-ulp shift straddles a
/// rounding boundary often enough over ~12k coordinates to flake.
fn assert_sweep(got: Sweep, want: Sweep) {
    assert_eq!(
        (got.kept, got.parts, got.vertices),
        (want.kept, want.parts, want.vertices),
        "counts: got {got:?}, want {want:?}"
    );
    if cfg!(target_os = "linux") {
        assert_eq!(
            got.digest, want.digest,
            "digest: got {got:?}, want {want:?}"
        );
    } else {
        eprintln!("digest not pinned on this platform: {got:?}");
    }
}

#[test]
fn road_detections_z12_sweep_is_pinned() {
    assert_sweep(
        sweep(12),
        Sweep {
            kept: 1031,
            parts: 1141,
            vertices: 5679,
            digest: 0xf12c_06cb_932b_1dfc,
        },
    );
}

#[test]
fn road_detections_z14_sweep_is_pinned() {
    assert_sweep(
        sweep(14),
        Sweep {
            kept: 1120,
            parts: 1239,
            vertices: 5957,
            digest: 0x82be_2b5a_25d7_a6ad,
        },
    );
}
