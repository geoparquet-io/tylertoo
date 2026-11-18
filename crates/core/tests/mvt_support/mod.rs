//! Decoded MVT polygons and gunzipped tiles of an archive, shared by the
//! coverage tests (#750, #802). Decodes the protobuf directly, not through
//! tylertoo's own decoder.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use geo::{Area, Coord, LineString, Polygon};
use prost::Message;
use tylertoo_core::internal::archive_index::ArchiveIndex;
use tylertoo_core::internal::vector_tile::Tile;

pub(crate) const EXTENT: f64 = 4096.0;

/// Each polygon feature of a tile, in tile units, in layer order: exterior
/// rings (positive area) start a polygon, holes (negative area) follow it.
pub(crate) fn tile_features(raw: &[u8]) -> Vec<Vec<Polygon<f64>>> {
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
            let mut polys = Vec::new();
            let mut exterior: Option<(LineString<f64>, Vec<LineString<f64>>)> = None;
            for r in rings.into_iter().filter(|r| r.len() >= 3) {
                let ls = LineString::new(r);
                let signed = Polygon::new(ls.clone(), vec![]).signed_area();
                if signed > 0.0 {
                    if let Some((e, holes)) = exterior.take() {
                        polys.push(Polygon::new(e, holes));
                    }
                    exterior = Some((ls, Vec::new()));
                } else if let Some((_, holes)) = exterior.as_mut() {
                    holes.push(ls);
                }
            }
            if let Some((e, holes)) = exterior {
                polys.push(Polygon::new(e, holes));
            }
            out.push(polys);
        }
    }
    out
}

/// The gunzipped tiles an archive holds at `zoom`, keyed by (x, y).
pub(crate) fn raw_tiles(archive: &Path, zoom: u8) -> HashMap<(u32, u32), Vec<u8>> {
    let index = ArchiveIndex::open(archive).expect("open archive");
    let mut out = HashMap::new();
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
        out.insert((t.x, t.y), raw);
    }
    out
}
