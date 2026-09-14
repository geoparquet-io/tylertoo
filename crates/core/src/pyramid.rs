//! Multi-band pyramids: several inputs, each owning a zoom range, one archive.
//!
//! A pyramid serves the *same* map from a different input at different zooms:
//! coarse zooms read a pre-aggregated summary, fine zooms read the raw
//! features. That is not the generalization ladder [`crate::overview`] builds.
//! The ladder derives coarse levels from the fine input by thinning and
//! simplifying, which is right for a road network and wrong for an aggregate
//! grid — the coarse representation there is a *different aggregation* (larger
//! cells with their own counts), not a thinned sample of the fine one, and
//! every cell must be drawn at every zoom of its band.
//!
//! Bands own disjoint zoom ranges, so no tile is ever claimed by two bands and
//! the merge is a concatenation rather than a per-tile layer union. That is the
//! whole reason this is tractable: each band is tiled by the ordinary pipeline,
//! and the archives are stitched by tile id.
//!
//! See issue #345.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::compression::{self, Compression};
use crate::pmtiles_writer::{decode_directory, tile_id_to_zxy, Header, PmtilesWriter};
use crate::Error;

/// One band: an input tiled verbatim across `min_zoom..=max_zoom` into `layer`.
#[derive(Debug, Clone)]
pub struct Band {
    pub input: PathBuf,
    pub layer: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
}

impl Band {
    /// Parse `LO-HI:PATH:LAYER`. The layer is optional and defaults to the
    /// input's file stem. PATH may contain `:` only if a layer is given after
    /// it, so the zoom range is split first and the layer last.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (range, rest) = spec
            .split_once(':')
            .ok_or_else(|| format!("band {spec:?}: expected LO-HI:PATH[:LAYER]"))?;
        let (lo, hi) = range
            .split_once('-')
            .ok_or_else(|| format!("band {spec:?}: zoom range must be LO-HI"))?;
        let min_zoom: u8 = lo
            .trim()
            .parse()
            .map_err(|_| format!("band {spec:?}: bad min zoom {lo:?}"))?;
        let max_zoom: u8 = hi
            .trim()
            .parse()
            .map_err(|_| format!("band {spec:?}: bad max zoom {hi:?}"))?;
        if min_zoom > max_zoom {
            return Err(format!("band {spec:?}: min zoom above max zoom"));
        }
        let (path, layer) = match rest.rsplit_once(':') {
            Some((p, l)) if !l.is_empty() && !p.is_empty() => (p.to_string(), l.to_string()),
            _ => {
                let p = rest.to_string();
                let stem = Path::new(&p)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "layer".to_string());
                (p, stem)
            }
        };
        Ok(Band {
            input: PathBuf::from(path),
            layer,
            min_zoom,
            max_zoom,
        })
    }
}

/// Reject overlapping zoom ranges. Two bands claiming one zoom would each write
/// the same tile ids, and the merge would silently keep whichever came last.
pub fn validate_bands(bands: &[Band]) -> Result<(), String> {
    if bands.is_empty() {
        return Err("a pyramid needs at least one --band".to_string());
    }
    let mut sorted: Vec<&Band> = bands.iter().collect();
    sorted.sort_by_key(|b| b.min_zoom);
    for w in sorted.windows(2) {
        if w[1].min_zoom <= w[0].max_zoom {
            return Err(format!(
                "bands overlap at zoom {}: {}-{} and {}-{}",
                w[1].min_zoom, w[0].min_zoom, w[0].max_zoom, w[1].min_zoom, w[1].max_zoom
            ));
        }
    }
    Ok(())
}

/// One layer's entry in the merged archive's `vector_layers`.
#[derive(Debug, Clone)]
struct LayerMeta {
    id: String,
    minzoom: u8,
    maxzoom: u8,
    fields: String,
}

/// Every tile in `path`, as `(z, x, y, still-compressed bytes)`.
///
/// Run-length entries are expanded: the merged archive re-derives its own runs,
/// and a band's numbering is not the merged archive's.
fn read_tiles(path: &Path) -> Result<Vec<(u8, u32, u32, Vec<u8>)>, Error> {
    let bytes = std::fs::read(path)?;
    let header = Header::from_bytes(&bytes)
        .map_err(|e| Error::PMTilesWrite(format!("{}: {e}", path.display())))?;

    let slice = |off: u64, len: u64, what: &str| -> Result<&[u8], Error> {
        let start = off as usize;
        let end = start
            .checked_add(len as usize)
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| Error::PMTilesWrite(format!("{what} past end of {}", path.display())))?;
        Ok(&bytes[start..end])
    };
    let dir = |raw: &[u8], what: &str| -> Result<Vec<_>, Error> {
        let plain = compression::decompress(raw, header.internal_compression)
            .map_err(|e| Error::PMTilesWrite(format!("{what}: {e}")))?;
        decode_directory(&plain).ok_or_else(|| Error::PMTilesWrite(format!("undecodable {what}")))
    };

    let root = dir(
        slice(header.root_dir_offset, header.root_dir_length, "root dir")?,
        "root dir",
    )?;
    let mut entries = Vec::new();
    for e in root {
        if e.run_length == 0 {
            let leaf = slice(
                header.leaf_dirs_offset + e.offset,
                u64::from(e.length),
                "leaf dir",
            )?;
            entries.extend(dir(leaf, "leaf dir")?);
        } else {
            entries.push(e);
        }
    }

    let mut out = Vec::new();
    for e in &entries {
        let data = slice(
            header.tile_data_offset + e.offset,
            u64::from(e.length),
            "tile data",
        )?
        .to_vec();
        for i in 0..u64::from(e.run_length.max(1)) {
            let (z, x, y) = tile_id_to_zxy(e.tile_id + i)
                .map_err(|e| Error::PMTilesWrite(format!("bad tile id: {e}")))?;
            out.push((z, x, y, data.clone()));
        }
    }
    Ok(out)
}

/// Pull one band archive's `vector_layers` entry out of its metadata, so the
/// merged archive advertises each layer's real field set rather than a guess.
fn layer_meta(path: &Path, band: &Band) -> Result<LayerMeta, Error> {
    let bytes = std::fs::read(path)?;
    let header = Header::from_bytes(&bytes)
        .map_err(|e| Error::PMTilesWrite(format!("{}: {e}", path.display())))?;
    let raw = &bytes[header.json_metadata_offset as usize
        ..(header.json_metadata_offset + header.json_metadata_length) as usize];
    let plain = compression::decompress(raw, header.internal_compression)
        .map_err(|e| Error::PMTilesWrite(format!("metadata: {e}")))?;
    let text = String::from_utf8_lossy(&plain);
    // The band writer emits exactly one vector_layers entry; lift its `fields`
    // object verbatim rather than re-deriving types we do not have here.
    let fields = text
        .find("\"fields\":")
        .and_then(|i| {
            let rest = &text[i + 9..];
            let mut depth = 0usize;
            for (j, c) in rest.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(rest[..=j].to_string());
                        }
                    }
                    _ => {}
                }
            }
            None
        })
        .unwrap_or_else(|| "{}".to_string());
    Ok(LayerMeta {
        id: band.layer.clone(),
        minzoom: band.min_zoom,
        maxzoom: band.max_zoom,
        fields,
    })
}

/// What a pyramid build produced.
#[derive(Debug, Clone)]
pub struct PyramidReport {
    pub total_tiles: usize,
    pub per_band_tiles: Vec<(String, u8, u8, usize)>,
}

/// Merge per-band archives into one, in band order.
///
/// `band_archives` pairs each band with the archive already tiled for it.
pub fn merge_bands(
    band_archives: &[(Band, PathBuf)],
    output: &Path,
) -> Result<PyramidReport, Error> {
    let mut writer = PmtilesWriter::with_compression(Compression::Gzip);
    let mut layers: Vec<LayerMeta> = Vec::new();
    let mut per_band = Vec::new();
    // Collected first so a later band cannot silently overwrite an earlier
    // one's tile: disjoint ranges make that impossible, and this asserts it.
    let mut seen: BTreeMap<(u8, u32, u32), &str> = BTreeMap::new();

    for (band, archive) in band_archives {
        let tiles = read_tiles(archive)?;
        let mut n = 0usize;
        for (z, x, y, data) in tiles {
            if z < band.min_zoom || z > band.max_zoom {
                continue;
            }
            if let Some(prev) = seen.insert((z, x, y), &band.layer) {
                return Err(Error::PMTilesWrite(format!(
                    "tile {z}/{x}/{y} claimed by both {prev:?} and {:?}",
                    band.layer
                )));
            }
            writer.add_tile_compressed(z, x, y, data)?;
            n += 1;
        }
        per_band.push((band.layer.clone(), band.min_zoom, band.max_zoom, n));
        layers.push(layer_meta(archive, band)?);
    }

    // Several bands may share a layer name (the aggregate bands do). Collapse
    // them into one entry spanning their combined zooms, or a client sees the
    // same layer declared twice with conflicting ranges.
    let mut merged: Vec<LayerMeta> = Vec::new();
    for l in layers {
        match merged.iter_mut().find(|m| m.id == l.id) {
            Some(m) => {
                m.minzoom = m.minzoom.min(l.minzoom);
                m.maxzoom = m.maxzoom.max(l.maxzoom);
                if m.fields == "{}" {
                    m.fields = l.fields;
                }
            }
            None => merged.push(l),
        }
    }
    let json = format!(
        "[{}]",
        merged
            .iter()
            .map(|l| format!(
                r#"{{"id":"{}","minzoom":{},"maxzoom":{},"fields":{}}}"#,
                l.id, l.minzoom, l.maxzoom, l.fields
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    writer.set_vector_layers_json(json);
    writer.write_to_file(output)?;

    Ok(PyramidReport {
        total_tiles: seen.len(),
        per_band_tiles: per_band,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_parses_range_path_and_layer() {
        let b = Band::parse("0-5:cells_r5.parquet:aggregate").unwrap();
        assert_eq!(b.min_zoom, 0);
        assert_eq!(b.max_zoom, 5);
        assert_eq!(b.layer, "aggregate");
        assert_eq!(b.input, PathBuf::from("cells_r5.parquet"));
    }

    #[test]
    fn band_layer_defaults_to_file_stem() {
        let b = Band::parse("9-14:/data/points.parquet").unwrap();
        assert_eq!(b.layer, "points");
        assert_eq!(b.min_zoom, 9);
    }

    #[test]
    fn overlapping_bands_are_rejected() {
        // Two bands claiming z5 would each write the same tile ids and the
        // merge would keep whichever ran last -- silently.
        let bands = vec![
            Band::parse("0-5:a.parquet:agg").unwrap(),
            Band::parse("5-8:b.parquet:agg").unwrap(),
        ];
        let err = validate_bands(&bands).unwrap_err();
        assert!(err.contains("overlap at zoom 5"), "{err}");
    }

    #[test]
    fn adjacent_bands_are_accepted() {
        let bands = vec![
            Band::parse("0-5:a.parquet:agg").unwrap(),
            Band::parse("6-8:b.parquet:agg").unwrap(),
            Band::parse("9-14:c.parquet:features").unwrap(),
        ];
        validate_bands(&bands).unwrap();
    }

    #[test]
    fn bad_range_is_rejected() {
        assert!(Band::parse("5-0:a.parquet:x").is_err());
        assert!(Band::parse("nope:a.parquet").is_err());
    }
}
