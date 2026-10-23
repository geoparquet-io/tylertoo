//! `--coverage-fill` closes the holes a gap-free coverage shows at coarse
//! zooms, without drawing past the coverage, and its output does not depend
//! on the thread count (#750).
//!
//! On the Madagascar admin-4 fixture (17,465 polygons that tile the island)
//! the defaults draw about 85% of the island at z4: small communes fall below
//! the visibility gate and their placeholder squares overlap one another. With
//! the fill, z4 draws what z8 draws, where every polygon is present.

use std::io::Read;
use std::path::Path;

use geo::{Area, BooleanOps, Coord, LineString, Polygon, Rect};
use prost::Message;
use tylertoo_core::archive_index::ArchiveIndex;
use tylertoo_core::vector_tile::Tile;

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

const EXTENT: f64 = 4096.0;

/// A tile's polygons in tile units: exterior rings (positive area) start a
/// polygon, holes (negative area) follow it.
fn tile_polygons(raw: &[u8]) -> Vec<Polygon<f64>> {
    let tile = Tile::decode(raw).expect("mvt");
    let mut out = Vec::new();
    for layer in &tile.layers {
        let s = EXTENT / f64::from(layer.extent.unwrap_or(4096));
        for f in layer.features.iter().filter(|f| f.r#type == Some(3)) {
            let (mut x, mut y, mut i) = (0i64, 0i64, 0usize);
            let mut rings: Vec<Vec<Coord<f64>>> = Vec::new();
            let g = &f.geometry;
            while i < g.len() {
                let (id, count) = (g[i] & 7, g[i] >> 3);
                i += 1;
                if id == 7 {
                    continue;
                }
                for _ in 0..count {
                    let zz = |v: u32| i64::from((v >> 1) as i32 ^ -((v & 1) as i32));
                    x += zz(g[i]);
                    y += zz(g[i + 1]);
                    i += 2;
                    if id == 1 {
                        rings.push(Vec::new());
                    }
                    rings.last_mut().unwrap().push(Coord {
                        x: x as f64 * s,
                        y: y as f64 * s,
                    });
                }
            }
            let mut exterior: Option<(LineString<f64>, Vec<LineString<f64>>)> = None;
            for r in rings.into_iter().filter(|r| r.len() >= 3) {
                let ls = LineString::new(r);
                let signed = Polygon::new(ls.clone(), vec![]).signed_area();
                if signed > 0.0 {
                    if let Some((e, holes)) = exterior.take() {
                        out.push(Polygon::new(e, holes));
                    }
                    exterior = Some((ls, Vec::new()));
                } else if let Some((_, holes)) = exterior.as_mut() {
                    holes.push(ls);
                }
            }
            if let Some((e, holes)) = exterior {
                out.push(Polygon::new(e, holes));
            }
        }
    }
    out
}

/// Each tile an archive draws at `zoom`: the union of its polygons clipped
/// to the tile, in tile units, keyed by (x, y).
fn drawn_tiles(
    archive: &Path,
    zoom: u8,
) -> std::collections::HashMap<(u32, u32), geo::MultiPolygon<f64>> {
    let index = ArchiveIndex::open(archive).expect("open archive");
    let tile_box = Rect::new(
        Coord { x: 0.0, y: 0.0 },
        Coord {
            x: EXTENT,
            y: EXTENT,
        },
    )
    .to_polygon();
    let mut out = std::collections::HashMap::new();
    for t in index.tiles() {
        let t = t.expect("tile entry");
        if t.z != zoom {
            continue;
        }
        let bytes = index.read_range(t.range.clone()).expect("read tile");
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut raw)
            .expect("gunzip");
        let union = geo::unary_union(&tile_polygons(&raw));
        out.insert((t.x, t.y), union.intersection(&tile_box));
    }
    out
}

/// Where a coarse zoom draws against a fine one, tile by tile: each fine
/// tile's square of the coarse tile above it is compared with what the fine
/// tile draws. Returns the area the coarse zoom misses and the area it adds,
/// each as a share of the fine zoom's total, so a hole in one place cannot
/// be offset by over-draw in another.
fn local_coverage(archive: &Path, coarse: u8, fine: u8) -> (f64, f64) {
    let coarse_tiles = drawn_tiles(archive, coarse);
    let fine_tiles = drawn_tiles(archive, fine);
    let n = 1u32 << (fine - coarse);
    let side = EXTENT / f64::from(n);
    let (mut fine_total, mut missing, mut added) = (0.0, 0.0, 0.0);
    for (&(cx, cy), drawn) in &coarse_tiles {
        for (dx, dy) in (0..n).flat_map(|dx| (0..n).map(move |dy| (dx, dy))) {
            let (x0, y0) = (f64::from(dx) * side, f64::from(dy) * side);
            let square = Rect::new(
                Coord { x: x0, y: y0 },
                Coord {
                    x: x0 + side,
                    y: y0 + side,
                },
            )
            .to_polygon();
            // In fine tile units² (the square is one fine tile).
            let a_coarse = drawn.intersection(&square).unsigned_area() * f64::from(n * n);
            let a_fine = fine_tiles
                .get(&(cx * n + dx, cy * n + dy))
                .map_or(0.0, |m| m.unsigned_area());
            fine_total += a_fine;
            missing += (a_fine - a_coarse).max(0.0);
            added += (a_coarse - a_fine).max(0.0);
        }
    }
    (missing / fine_total, added / fine_total)
}

/// The area an archive draws at `zoom`, as a share of the world: per tile,
/// the union of its polygons clipped to the tile, so overlaps and the tile
/// buffer count once.
fn drawn_area(archive: &Path, zoom: u8) -> f64 {
    let index = ArchiveIndex::open(archive).expect("open archive");
    let tile_box = Rect::new(
        Coord { x: 0.0, y: 0.0 },
        Coord {
            x: EXTENT,
            y: EXTENT,
        },
    )
    .to_polygon();
    let mut total = 0.0;
    for t in index.tiles() {
        let t = t.expect("tile entry");
        if t.z != zoom {
            continue;
        }
        let bytes = index.read_range(t.range.clone()).expect("read tile");
        let mut raw = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice())
            .read_to_end(&mut raw)
            .expect("gunzip");
        let polys = tile_polygons(&raw);
        let union = geo::unary_union(&polys);
        total += union.intersection(&tile_box).unsigned_area();
    }
    total / (EXTENT * EXTENT) / 4f64.powi(i32::from(zoom))
}

#[test]
fn coverage_fill_closes_coarse_holes_without_overdrawing() {
    use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

    let Some(fixture_path) = fixture::realdata("fieldmaps-madagascar-adm4.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = dir.path().join("overview.parquet");
    convert_to_overviews(
        &fixture_path,
        &overview,
        &ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 8,
            },
            ..Default::default()
        },
    )
    .expect("convert");

    let export = |name: &str, fill: bool, threads: usize| {
        let archive = dir.path().join(name);
        // tippecanoe's cap on the compressed tile, with a 2M raw backstop
        // (#761): the fill takes only the room the tile's own features
        // leave, and a 500K raw cap leaves z4 almost none.
        let opts = ExportOptions {
            coverage_fill: fill,
            tile_size_limit: Some(2048 * 1024),
            compressed_tile_size_limit: Some(500 * 1024),
            ..ExportOptions::default()
        };
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("thread pool")
            .install(|| export_pmtiles(&overview, &archive, &opts))
            .expect("export");
        archive
    };
    let today = export("today.pmtiles", false, 4);
    let fill = export("fill.pmtiles", true, 4);
    let fill_serial = export("fill-1.pmtiles", true, 1);

    assert!(
        std::fs::read(&fill).unwrap() == std::fs::read(&fill_serial).unwrap(),
        "the fill must not depend on the thread count"
    );

    // z8 draws every polygon: the reference for the island's area.
    let island = drawn_area(&today, 8);
    let today_z4 = drawn_area(&today, 4) / island;
    assert!(
        today_z4 < 0.95,
        "the defaults should still show the #750 holes at z4: {today_z4:.4}"
    );
    // Where z4 draws, not only how much: per z8 tile, against z8.
    let (missing, added) = local_coverage(&fill, 4, 8);
    let (today_missing, _) = local_coverage(&today, 4, 8);
    eprintln!("z4 against z8: missing {missing:.4}, added {added:.4} (defaults: missing {today_missing:.4})");
    assert!(
        missing < 0.02,
        "the fill must close the holes: z4 misses {missing:.4} of z8's area (defaults {today_missing:.4})"
    );
    assert!(
        added < 0.03,
        "and must not draw past the coverage: z4 adds {added:.4}"
    );
}

/// How much work the fill does on scattered sub-pixel fields, held under
/// ratchets (#750). Round 2 of the fill doubled its vertices and the size
/// valve's passes on FTW Moldova, found only by a 25-minute benchmark; the
/// report's counters are exact for a given input, so a change that does
/// that again fails here, with the counts in the message.
///
/// The fixture is one z10 tile of the FTW Moldova benchmark input (10,931
/// fields), under a 200 KB cap so the valve has tiles to merge. If a change
/// lowers the counts for good, lower the bounds with it.
#[test]
fn coverage_fill_work_stays_bounded_on_scattered_fields() {
    use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
    use tylertoo_core::overview::export::{export_pmtiles, ExportOptions, ExportReport};

    let Some(fixture_path) = fixture::realdata("ftw-moldova-z10.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = dir.path().join("overview.parquet");
    convert_to_overviews(
        &fixture_path,
        &overview,
        &ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 0,
                max_zoom: 12,
            },
            ..Default::default()
        },
    )
    .expect("convert");
    let export = |fill: bool| -> ExportReport {
        let opts = ExportOptions {
            coverage_fill: fill,
            tile_size_limit: Some(200_000),
            compressed_tile_size_limit: None,
            ..ExportOptions::default()
        };
        export_pmtiles(&overview, dir.path().join("out.pmtiles"), &opts).expect("export")
    };
    let sum = |r: &ExportReport, f: fn(&tylertoo_core::overview::export::ZoomReport) -> u64| {
        r.zooms.iter().map(f).sum::<u64>()
    };

    let off = export(false);
    assert_eq!(sum(&off, |z| z.fill_polygons as u64), 0);

    let on = export(true);
    let polygons = sum(&on, |z| z.fill_polygons as u64);
    let vertices = sum(&on, |z| z.fill_vertices as u64);
    let shed = sum(&on, |z| z.fill_shed as u64);
    let passes = sum(&on, |z| z.valve_passes);
    let tiles = sum(&on, |z| z.tile_count as u64);
    let counts = format!(
        "fill polygons {polygons}, vertices {vertices}, shed {shed}; \
         valve passes {passes} for {tiles} tiles (fill off: {})",
        sum(&off, |z| z.valve_passes)
    );
    eprintln!("{counts}");
    // Measured when the bounds were set: vertices 133,900, shed 9,850,
    // passes 110 for 72 tiles (79 with the fill off; fitting the fill into
    // the room left counts its encodes). About 25% headroom.
    assert!(polygons > 0, "the fill drew nothing: {counts}");
    assert!(
        vertices <= 170_000,
        "fill vertices over the ratchet: {counts}"
    );
    assert!(shed <= 12_500, "fill shed over the ratchet: {counts}");
    assert!(passes <= 136, "valve passes over the ratchet: {counts}");
    assert_eq!(
        on.zooms.last().map(|z| z.fill_polygons),
        Some(0),
        "the finest zoom draws every feature and needs no fill: {counts}"
    );
    // The fill takes only the room the tile's own features leave: every
    // zoom keeps exactly the features, merges and drops it has without it.
    for (a, b) in off.zooms.iter().zip(&on.zooms) {
        assert_eq!(
            (
                a.tile_count,
                a.tile_feature_count,
                a.merged_features,
                a.valve_dropped_features
            ),
            (
                b.tile_count,
                b.tile_feature_count,
                b.merged_features,
                b.valve_dropped_features
            ),
            "z{}: the fill changed the tile's own features",
            a.zoom
        );
    }
}
