//! Polygons that share a border still share it after simplification (#802).
//!
//! A polygon simplified on its own would lose different vertices from its
//! neighbour's copy of a shared border, opening gaps and overlaps between
//! them at coarse zooms. tippecanoe keeps shared borders identical
//! (`--detect-shared-borders`, `-pn`), both opt-in, as is
//! `--no-simplification-of-shared-nodes` here. These tests turn it on, run
//! tiny coverages through convert and export and check one zoom's tiles with an MVT
//! decoder of their own: the polygons must not overlap, and their union
//! must cover the source's union.
//!
//! Every coordinate is an integer tile unit at [`Z`], so the tile grid
//! itself moves no vertex: anything off is simplification.

use std::path::Path;

use geo::{
    Area, BooleanOps, Coord, CoordsIter, Geometry, LineString, MapCoordsInPlace, MultiPolygon,
    Polygon, Rect,
};
use tylertoo_core::internal::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::internal::overview::export::{export_pmtiles, ExportOptions};

mod mvt_support;
#[expect(dead_code, reason = "only the input writer is needed here")]
#[path = "../src/overview/testutil.rs"]
mod testutil;

use mvt_support::{raw_tiles, tile_features, EXTENT};

/// The checked zoom. The finest zoom is 14, so z10 is simplified with a
/// tolerance of one GSD there: 4 tile units.
const Z: u8 = 10;
const TILE: (u32, u32) = (512, 511);
/// Gap or overlap allowed per tile unit of shared border, in tile units:
/// the mean width of a sliver. Shared simplification with integer snapping
/// gives 0.
const MAX_SLIVER_WIDTH: f64 = 0.05;

/// A z10 tile-unit coordinate of [`TILE`] (y down) in lon/lat.
fn lonlat(c: Coord<f64>) -> Coord<f64> {
    let n = f64::from(1u32 << Z) * EXTENT;
    let x = (f64::from(TILE.0) * EXTENT + c.x) / n;
    let y = (f64::from(TILE.1) * EXTENT + c.y) / n;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y))
        .sinh()
        .atan()
        .to_degrees();
    Coord {
        x: x * 360.0 - 180.0,
        y: lat,
    }
}

/// A natural-looking border from `a` to `b` (both kept): a vertex every 10
/// units on three half-waves 40 units high, each vertex jittered across the
/// line by -3..=3 units (under the tolerance), pseudo-random so no two RDP
/// splits tie. The curve makes which vertices RDP keeps depend on where the
/// chain it splits starts and ends.
fn jagged(a: (i32, i32), b: (i32, i32), seed: u64) -> Vec<Coord<f64>> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx.abs().max(dy.abs());
    let steps = len / 10;
    let mut state = seed;
    let mut out = vec![Coord {
        x: f64::from(a.0),
        y: f64::from(a.1),
    }];
    for i in 1..steps {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let wave = 40.0 * (3.0 * std::f64::consts::PI * f64::from(i) / f64::from(steps)).sin();
        let off = wave.round() as i32 + ((state >> 33) % 7) as i32 - 3;
        let (x, y) = (a.0 + dx * i / steps, a.1 + dy * i / steps);
        // Offset across the line: horizontal borders move in y, vertical in x.
        let (x, y) = if dx.abs() >= dy.abs() {
            (x, y + off)
        } else {
            (x + off, y)
        };
        out.push(Coord {
            x: f64::from(x),
            y: f64::from(y),
        });
    }
    out.push(Coord {
        x: f64::from(b.0),
        y: f64::from(b.1),
    });
    out
}

fn pt(x: i32, y: i32) -> Coord<f64> {
    Coord {
        x: f64::from(x),
        y: f64::from(y),
    }
}

/// A closed ring from chains of tile-unit coordinates, consecutive chains
/// sharing their joining vertex.
fn ring(chains: &[Vec<Coord<f64>>]) -> LineString<f64> {
    let mut out: Vec<Coord<f64>> = Vec::new();
    for c in chains {
        let skip = usize::from(out.last() == c.first());
        out.extend(&c[skip..]);
    }
    if out.first() != out.last() {
        out.push(out[0]);
    }
    LineString::new(out)
}

fn rev(mut c: Vec<Coord<f64>>) -> Vec<Coord<f64>> {
    c.reverse();
    c
}

/// The same ring starting `k` vertices later: sources rarely agree on where
/// a shared ring starts.
fn rotate(ls: &LineString<f64>, k: usize) -> LineString<f64> {
    let mut c = ls.0[..ls.0.len() - 1].to_vec();
    let n = c.len();
    c.rotate_left(k % n);
    let first = c[0];
    c.push(first);
    LineString::new(c)
}

/// Tiles `polys` (tile units at [`Z`]) to z0–14 at the default settings,
/// with `--simplify-factor` `factor` and shared-border pins, and returns each [`Z`] tile's polygon
/// features, in tile units.
fn tile(
    polys: &[Polygon<f64>],
    factor: f64,
    dir: &Path,
) -> std::collections::HashMap<(u32, u32), Vec<MultiPolygon<f64>>> {
    tile_to(
        polys,
        factor,
        14,
        &ExportOptions::default().with_shared_nodes(true),
        dir,
    )
}

/// [`tile`] to `max_zoom`, exported with `export`.
fn tile_to(
    polys: &[Polygon<f64>],
    factor: f64,
    max_zoom: u8,
    export: &ExportOptions,
    dir: &Path,
) -> std::collections::HashMap<(u32, u32), Vec<MultiPolygon<f64>>> {
    let geoms: Vec<Option<Geometry<f64>>> = polys
        .iter()
        .map(|p| {
            let mut p = p.clone();
            p.map_coords_in_place(lonlat);
            Some(Geometry::Polygon(p))
        })
        .collect();
    let input = dir.join("input.parquet");
    let overview = dir.join("overview.parquet");
    let archive = dir.join("out.pmtiles");
    testutil::write_input(&input, &geoms, true, None);
    let mut options = ConvertOptions::default();
    options.simplify.factor = factor;
    options.simplify.shared_nodes = true;
    convert_to_overviews(
        &input,
        &overview,
        &options.with_levels(LevelPlan::ZoomRange {
            min_zoom: 0,
            max_zoom,
        }),
    )
    .expect("convert");
    export_pmtiles(&overview, &archive, export).expect("export");
    let tile_box = Rect::new(pt(0, 0), pt(4096, 4096)).to_polygon();
    raw_tiles(&archive, Z)
        .into_iter()
        .map(|(xy, raw)| {
            let feats = tile_features(&raw)
                .into_iter()
                .map(|f| MultiPolygon::new(f).intersection(&tile_box))
                .collect();
            (xy, feats)
        })
        .collect()
}

/// (overlap, missing) in tile units² for one tile: the area two or more
/// features draw, and the source area no feature draws. `source` is in the
/// tile's own units.
fn slivers(feats: &[MultiPolygon<f64>], source: &[Polygon<f64>]) -> (f64, f64) {
    let tile_box = Rect::new(pt(0, 0), pt(4096, 4096)).to_polygon();
    let drawn = geo::unary_union(feats.iter());
    let sum: f64 = feats.iter().map(|f| f.unsigned_area()).sum();
    let src = geo::unary_union(source.iter()).intersection(&tile_box);
    (
        sum - drawn.unsigned_area(),
        src.difference(&drawn).unsigned_area(),
    )
}

fn assert_no_slivers(
    case: &str,
    feats: &[MultiPolygon<f64>],
    source: &[Polygon<f64>],
    border: f64,
) {
    let (overlap, missing) = slivers(feats, source);
    eprintln!(
        "{case}: overlap {overlap:.1} u², missing {missing:.1} u² over {border} u of shared border \
         (mean width {:.3} / {:.3} u)",
        overlap / border,
        missing / border
    );
    assert!(
        overlap / border <= MAX_SLIVER_WIDTH && missing / border <= MAX_SLIVER_WIDTH,
        "{case}: neighbours must share their simplified border: overlap {overlap:.1} u² and gap \
         {missing:.1} u² along {border} u of border (allowed {MAX_SLIVER_WIDTH} u wide each)"
    );
}

fn shifted(polys: &[Polygon<f64>], dx: f64) -> Vec<Polygon<f64>> {
    polys
        .iter()
        .map(|p| {
            let mut p = p.clone();
            p.map_coords_in_place(|c| Coord {
                x: c.x - dx,
                y: c.y,
            });
            p
        })
        .collect()
}

/// A shares a 1000-unit border with B. The border is the whole left side of
/// B but only the middle of A's right side, so RDP splits the two copies at
/// different vertices.
fn jagged_pair() -> (Polygon<f64>, Polygon<f64>) {
    let border = jagged((2000, 1500), (2000, 2500), 802);
    let a = Polygon::new(
        ring(&[
            vec![pt(1000, 1000), pt(2000, 1000), pt(2000, 1500)],
            border.clone(),
            vec![pt(2000, 2500), pt(2000, 3000), pt(1000, 3000)],
        ]),
        vec![],
    );
    let b = Polygon::new(
        ring(&[
            rev(border),
            vec![pt(2000, 1500), pt(3000, 1500), pt(3000, 2500)],
        ]),
        vec![],
    );
    (a, b)
}

/// (a) Two polygons share a jagged border ([`jagged_pair`]).
#[test]
fn two_polygons_share_a_jagged_border() {
    let (a, b) = jagged_pair();
    let dir = tempfile::tempdir().unwrap();
    let tiles = tile(&[a.clone(), b.clone()], 1.0, dir.path());
    assert_no_slivers("jagged border", &tiles[&TILE], &[a, b], 1000.0);
}

/// Export's max-zoom simplification (#814) keeps the shared border too: at
/// the finest zoom it thins each feature on its own, so it needs the pins.
#[test]
fn max_zoom_simplification_shares_a_jagged_border() {
    let (a, b) = jagged_pair();
    let dir = tempfile::tempdir().unwrap();
    let export = ExportOptions::default()
        .with_max_zoom_simplify_factor(Some(1.0))
        .with_shared_nodes(true);
    let tiles = tile_to(&[a.clone(), b.clone()], 1.0, Z, &export, dir.path());
    assert_no_slivers("max-zoom jagged border", &tiles[&TILE], &[a, b], 1000.0);
}

/// The control: without simplification the same input shows no sliver, so
/// what the tests below measure is simplification, not the tile grid or the
/// oracle.
#[test]
fn unsimplified_shared_border_has_no_slivers() {
    let (a, b) = jagged_pair();
    let dir = tempfile::tempdir().unwrap();
    let tiles = tile(&[a.clone(), b.clone()], 0.0, dir.path());
    assert_no_slivers("unsimplified", &tiles[&TILE], &[a, b], 1000.0);
}

/// (b) Three polygons meet at a junction J that sits off the straight line
/// of A's lower side. A must keep J, as B and C do: per-feature RDP can
/// drop it from A, where it is a bump of 3 units (under the tolerance).
#[test]
fn three_polygons_keep_their_junction() {
    let j = (2000, 2003);
    let left = jagged((1000, 2000), j, 11);
    let right = jagged(j, (3000, 2000), 12);
    let down = jagged(j, (2000, 3000), 13);
    let a = Polygon::new(
        ring(&[
            vec![pt(1000, 1000), pt(1000, 2000)],
            left.clone(),
            right.clone(),
            vec![pt(3000, 2000), pt(3000, 1000)],
        ]),
        vec![],
    );
    let b = Polygon::new(
        ring(&[
            rev(left),
            vec![pt(1000, 2000), pt(1000, 3000), pt(2000, 3000)],
            rev(down.clone()),
        ]),
        vec![],
    );
    let c = Polygon::new(
        ring(&[
            down,
            vec![pt(2000, 3000), pt(3000, 3000), pt(3000, 2000)],
            rev(right),
        ]),
        vec![],
    );
    let dir = tempfile::tempdir().unwrap();
    let tiles = tile(&[a.clone(), b.clone(), c.clone()], 1.0, dir.path());
    let feats = &tiles[&TILE];
    let jc = pt(j.0, j.1);
    for f in feats {
        let near = f
            .coords_iter()
            .any(|c| (c.x - jc.x).abs() <= 0.5 && (c.y - jc.y).abs() <= 0.5);
        assert!(
            near,
            "every polygon meeting at the junction must keep it as a vertex; one with {} \
             vertices does not",
            f.coords_count()
        );
    }
    assert_no_slivers("junction", feats, &[a, b, c], 3000.0);
}

/// (c) A hole filled exactly by another polygon, whose ring starts at a
/// different vertex than the hole's.
#[test]
fn polygon_fills_a_hole_exactly() {
    let hole = ring(&[
        jagged((1500, 1500), (2500, 1500), 21),
        jagged((2500, 1500), (2500, 2500), 22),
        jagged((2500, 2500), (1500, 2500), 23),
        jagged((1500, 2500), (1500, 1500), 24),
    ]);
    let outer = ring(&[vec![
        pt(1000, 1000),
        pt(3000, 1000),
        pt(3000, 3000),
        pt(1000, 3000),
    ]]);
    let donut = Polygon::new(outer, vec![hole.clone()]);
    let island = Polygon::new(rotate(&hole, 137), vec![]);
    let dir = tempfile::tempdir().unwrap();
    let tiles = tile(&[donut.clone(), island.clone()], 1.0, dir.path());
    assert_no_slivers("filled hole", &tiles[&TILE], &[donut, island], 4000.0);
}

/// (d) A shared border that crosses the tile edge at x = 4096: each tile
/// clips both copies, and neither tile may show a sliver.
#[test]
fn shared_border_across_a_tile_edge() {
    let border = jagged((3000, 2000), (5200, 2000), 41);
    let a = Polygon::new(
        ring(&[
            vec![pt(3000, 1000), pt(3000, 2000)],
            border.clone(),
            vec![pt(5200, 2000), pt(5200, 1000)],
        ]),
        vec![],
    );
    let b = Polygon::new(
        ring(&[
            rev(border),
            vec![
                pt(3000, 2000),
                pt(3000, 3000),
                pt(5200, 3000),
                pt(5200, 2000),
            ],
        ]),
        vec![],
    );
    let dir = tempfile::tempdir().unwrap();
    let tiles = tile(
        &[Polygon::new(rotate(a.exterior(), 50), vec![]), b.clone()],
        1.0,
        dir.path(),
    );
    let src = [a, b];
    assert_no_slivers("left tile", &tiles[&TILE], &src, 1096.0);
    let right = (TILE.0 + 1, TILE.1);
    assert_no_slivers("right tile", &tiles[&right], &shifted(&src, EXTENT), 1104.0);
}
