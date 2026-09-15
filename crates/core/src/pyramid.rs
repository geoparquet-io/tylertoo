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

use serde_json::{json, Value};

use crate::compression::{self, Compression};
use crate::dedup::TileHasher;
use crate::pmtiles_writer::{
    decode_directory, tile_id_to_zxy, DirEntry, Header, StreamingPmtilesWriter,
};
use crate::tile::TileBounds;
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
    /// Parse `LO-HI:PATH[:LAYER]`. The layer is optional and defaults to the
    /// input's file stem. The zoom range is split off first and the layer last.
    ///
    /// A path containing `:` is ambiguous — `0-5:C:/data/x.pmtiles` splits into
    /// path `C` and layer `/data/x.pmtiles`, and nothing in the grammar can
    /// tell that apart from a genuine three-part spec. Rather than guess, a
    /// parsed layer that looks like a path fragment (it contains `/` or `\`) is
    /// rejected with an explanation. Layer names are MVT layer ids and do not
    /// contain path separators, so nothing legitimate is refused.
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
        if layer.contains('/') || layer.contains('\\') {
            return Err(format!(
                "band {spec:?}: parsed layer name {layer:?} contains a path separator, so the \
                 archive path appears to contain a ':'. A ':' in the path is not supported by \
                 --band; rename the archive or point at it through a symlink."
            ));
        }
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
    /// The band's `fields` object, lifted verbatim from its metadata.
    fields: Value,
}

/// One band's archive, read and parsed exactly once.
///
/// The whole file is held while that band is merged — it is the compressed
/// archive, not the expanded tile set, and only one band is open at a time.
/// Tiles are handed out by [`BandArchive::for_each_tile`] as borrowed slices,
/// so a run-length entry costs one slice rather than N owned copies.
struct BandArchive {
    bytes: Vec<u8>,
    header: Header,
    /// Root entries with every leaf pointer already resolved. Run lengths are
    /// left intact; expansion happens per-tile in `for_each_tile`.
    entries: Vec<DirEntry>,
    /// `vector_layers[0].fields`, or `{}` when the metadata has no usable one.
    fields: Value,
    /// The archive's own header bounds, when they describe a real box.
    bounds: Option<TileBounds>,
}

/// The bounds a band actually claims, or `None`.
///
/// A writer that was never given bounds stores `TileBounds::empty()`, whose
/// infinities saturate to ±214.7° on the way into the header and read back
/// inverted (min > max). Unioning that swallows every real band, so an
/// unusable box is dropped rather than folded in. A zero-area box is kept: it
/// cannot poison a union, and a single-tile band legitimately has one.
fn usable_bounds(header: &Header) -> Option<TileBounds> {
    let b = TileBounds::new(
        header.min_lon,
        header.min_lat,
        header.max_lon,
        header.max_lat,
    );
    let finite = b.lng_min.is_finite()
        && b.lat_min.is_finite()
        && b.lng_max.is_finite()
        && b.lat_max.is_finite();
    if finite && b.is_valid() {
        Some(b)
    } else {
        None
    }
}

impl BandArchive {
    /// Read `path` once: header, directories, metadata and bounds together.
    fn open(path: &Path) -> Result<Self, Error> {
        let bytes = std::fs::read(path)?;
        let header = Header::from_bytes(&bytes)
            .map_err(|e| Error::PMTilesWrite(format!("{}: {e}", path.display())))?;

        // The merged archive declares gzip tile compression. Copying bytes out
        // of an archive that used anything else would mislabel every tile.
        if header.tile_compression != Compression::Gzip {
            return Err(Error::PMTilesWrite(format!(
                "{}: tile compression is {:?}, but a merged pyramid is written as gzip; \
                 re-tile this band with gzip tiles",
                path.display(),
                header.tile_compression
            )));
        }

        let slice = |off: u64, len: u64, what: &str| -> Result<&[u8], Error> {
            let start = off as usize;
            let end = start
                .checked_add(len as usize)
                .filter(|&e| e <= bytes.len())
                .ok_or_else(|| {
                    Error::PMTilesWrite(format!("{what} past end of {}", path.display()))
                })?;
            Ok(&bytes[start..end])
        };
        let dir = |raw: &[u8], what: &str| -> Result<Vec<DirEntry>, Error> {
            let plain = compression::decompress(raw, header.internal_compression)
                .map_err(|e| Error::PMTilesWrite(format!("{what}: {e}")))?;
            decode_directory(&plain)
                .ok_or_else(|| Error::PMTilesWrite(format!("undecodable {what}")))
        };

        let root = dir(
            slice(header.root_dir_offset, header.root_dir_length, "root dir")?,
            "root dir",
        )?;
        let mut entries = Vec::new();
        for e in root {
            if e.run_length != 0 {
                entries.push(e);
                continue;
            }
            let leaf = slice(
                header.leaf_dirs_offset + e.offset,
                u64::from(e.length),
                "leaf dir",
            )?;
            for inner in dir(leaf, "leaf dir")? {
                // run_length 0 inside a leaf is a second-level leaf pointer.
                // The spec allows arbitrarily deep directories; this reader
                // handles one level, and falling through would emit directory
                // bytes as a tile. Say so instead of producing garbage.
                if inner.run_length == 0 {
                    return Err(Error::PMTilesWrite(format!(
                        "{}: multi-level leaf directories are not supported",
                        path.display()
                    )));
                }
                entries.push(inner);
            }
        }

        let raw_meta = slice(
            header.json_metadata_offset,
            header.json_metadata_length,
            "metadata",
        )?;
        let fields = parse_fields(raw_meta, header.internal_compression, path)?;
        let bounds = usable_bounds(&header);

        Ok(BandArchive {
            bytes,
            header,
            entries,
            fields,
            bounds,
        })
    }

    /// Hand every addressed tile to `f` as `(z, x, y, still-compressed bytes)`.
    ///
    /// Run-length entries are expanded into individual ids — a band's numbering
    /// is not the merged archive's, which re-derives its own runs — but the one
    /// data slice is passed for each id rather than copied per id. The merged
    /// writer's dedup cache collapses the run again on the far side.
    fn for_each_tile<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(u8, u32, u32, &[u8]) -> Result<(), Error>,
    {
        for e in &self.entries {
            let start = (self.header.tile_data_offset + e.offset) as usize;
            let end = start
                .checked_add(e.length as usize)
                .filter(|&x| x <= self.bytes.len())
                .ok_or_else(|| Error::PMTilesWrite("tile data past end of archive".to_string()))?;
            let data = &self.bytes[start..end];
            for i in 0..u64::from(e.run_length.max(1)) {
                let (z, x, y) = tile_id_to_zxy(e.tile_id + i)
                    .map_err(|e| Error::PMTilesWrite(format!("bad tile id: {e}")))?;
                f(z, x, y, data)?;
            }
        }
        Ok(())
    }
}

/// Lift `vector_layers[0].fields` out of a band archive's JSON metadata.
///
/// Parsed with `serde_json` rather than scanned for `"fields":` and brace
/// counted. The hand-rolled version had two failure modes: its depth counter
/// underflowed on a `}` seen before any `{` (a panic in debug, a wrap and a
/// silent `{}` in release), and it counted braces inside string literals, so a
/// field *named* with a `}` truncated the object into invalid JSON.
fn parse_fields(raw: &[u8], internal: Compression, path: &Path) -> Result<Value, Error> {
    let plain = compression::decompress(raw, internal)
        .map_err(|e| Error::PMTilesWrite(format!("{}: metadata: {e}", path.display())))?;
    // Unparseable metadata is not fatal: the band's tiles are still usable,
    // they just end up declaring no field types.
    let parsed: Value = match serde_json::from_slice(&plain) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "{}: metadata is not valid JSON ({e}); merged layer will declare no fields",
                path.display()
            );
            return Ok(json!({}));
        }
    };
    Ok(parsed
        .get("vector_layers")
        .and_then(|v| v.get(0))
        .and_then(|l| l.get("fields"))
        .filter(|f| f.is_object())
        .cloned()
        .unwrap_or_else(|| json!({})))
}

/// What a pyramid build produced.
#[derive(Debug, Clone)]
pub struct PyramidReport {
    pub total_tiles: usize,
    pub per_band_tiles: Vec<(String, u8, u8, usize)>,
    /// Tiles found in a band archive at a zoom outside that band's declared
    /// range, and therefore dropped. Non-zero means the zoom range the archive
    /// was tiled with disagrees with the range `--band` declares.
    pub skipped: usize,
}

/// Merge per-band archives into one, in band order.
///
/// Tiles are copied across still compressed; nothing is decoded. That is also
/// why the merged metadata carries no `tilestats` — reconstructing feature
/// counts and attribute histograms would mean parsing every MVT, which is
/// exactly the cost this merge exists to avoid.
pub fn merge_bands(bands: &[Band], output: &Path) -> Result<PyramidReport, Error> {
    // A library caller can hand over overlapping bands covering disjoint
    // geography, which the per-tile collision check below would never catch.
    validate_bands(bands).map_err(Error::PMTilesWrite)?;

    // StreamingPmtilesWriter, not PmtilesWriter: it spools tile bytes to a temp
    // file instead of holding every tile in RAM, and it deduplicates. The merge
    // is exactly the workload that needs both — an aggregate band is mostly
    // identical tiles, and re-expanding its runs without dedup turned a
    // 300-byte band archive into 164 KiB.
    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)
        .map_err(|e| Error::PMTilesWrite(format!("Failed to create streaming writer: {e}")))?;
    let mut layers: Vec<LayerMeta> = Vec::new();
    let mut per_band = Vec::new();
    let mut skipped_total = 0usize;
    let mut union: Option<TileBounds> = None;
    // Guards against a later band overwriting an earlier one's tile. Disjoint
    // zoom ranges make that impossible; this asserts it.
    let mut seen: BTreeMap<(u8, u32, u32), &str> = BTreeMap::new();

    for band in bands {
        let archive = BandArchive::open(&band.input)?;
        if let Some(b) = archive.bounds {
            match union.as_mut() {
                Some(u) => u.expand(&b),
                None => union = Some(b),
            }
        }
        let mut n = 0usize;
        let mut skipped = 0usize;
        archive.for_each_tile(|z, x, y, data| {
            if z < band.min_zoom || z > band.max_zoom {
                skipped += 1;
                return Ok(());
            }
            if let Some(prev) = seen.insert((z, x, y), band.layer.as_str()) {
                return Err(Error::PMTilesWrite(format!(
                    "tile {z}/{x}/{y} claimed by both {prev:?} and {:?}",
                    band.layer
                )));
            }
            // Hash the COMPRESSED bytes. Nothing is decompressed here, so the
            // uncompressed hash the writer's own path keys on is unavailable.
            // This is conservative: identical compressed bytes are certainly
            // identical tiles, so it can miss a duplicate (two band archives
            // written with different gzip settings) but never invents one.
            let hash = TileHasher::hash(data);
            writer
                .add_tile_precompressed(z, x, y, hash, data, data.len(), 0)
                .map_err(|e| Error::PMTilesWrite(format!("Failed to add tile: {e}")))?;
            n += 1;
            Ok(())
        })?;
        if skipped > 0 {
            log::warn!(
                "band {:?} ({}): dropped {skipped} tile(s) outside its declared zoom range \
                 z{}-{}; the archive was tiled with a different range than --band declares",
                band.layer,
                band.input.display(),
                band.min_zoom,
                band.max_zoom
            );
            skipped_total += skipped;
        }
        per_band.push((band.layer.clone(), band.min_zoom, band.max_zoom, n));
        layers.push(LayerMeta {
            id: band.layer.clone(),
            minzoom: band.min_zoom,
            maxzoom: band.max_zoom,
            fields: archive.fields.clone(),
        });
    }

    match union {
        Some(b) => writer.set_bounds(&b),
        // Leaving the header bounds unset is no worse than the degenerate box
        // an unusable union would write, and it does not invent an extent.
        None => log::warn!(
            "no band archive carried usable bounds; the merged archive's bounds are left unset"
        ),
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
                if m.fields.as_object().is_some_and(|o| o.is_empty()) {
                    m.fields = l.fields;
                }
            }
            None => merged.push(l),
        }
    }
    // Serialized through serde_json, not format!: a layer id is a user-supplied
    // --band name, and a `"` or `\` in one would otherwise break the JSON.
    let json = Value::Array(
        merged
            .iter()
            .map(|l| {
                json!({
                    "id": l.id,
                    "minzoom": l.minzoom,
                    "maxzoom": l.maxzoom,
                    "fields": l.fields,
                })
            })
            .collect(),
    );
    writer.set_vector_layers_json(json.to_string());
    writer
        .finalize(output)
        .map_err(|e| Error::PMTilesWrite(format!("Failed to write {}: {e}", output.display())))?;

    Ok(PyramidReport {
        total_tiles: seen.len(),
        per_band_tiles: per_band,
        skipped: skipped_total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    /// A ':' in the path splits in the wrong place: `0-5:C:/data/x.pmtiles`
    /// used to yield path "C" and layer "/data/x.pmtiles", failing much later
    /// with a baffling "band archive not found: C". Refuse it up front.
    #[test]
    fn colon_in_path_is_rejected_not_mangled() {
        let err = Band::parse("0-5:C:/data/x.pmtiles").unwrap_err();
        assert!(err.contains("path separator"), "{err}");
        let err = Band::parse("0-9:/data/2024:06/x.pmtiles").unwrap_err();
        assert!(err.contains("path separator"), "{err}");
    }

    // --- merge tests ---------------------------------------------------

    /// Write a band archive: `tiles` all share one payload, so the band writer
    /// dedups and run-length encodes them the way a real aggregate band does.
    fn write_band(
        path: &Path,
        layer: &str,
        tiles: &[(u8, u32, u32)],
        bounds: TileBounds,
        fields: &[(&str, &str)],
    ) {
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name(layer);
        w.set_bounds(&bounds);
        if !fields.is_empty() {
            let map: HashMap<String, String> = fields
                .iter()
                .map(|&(k, v)| (k.to_string(), v.to_string()))
                .collect();
            w.set_fields(map);
        }
        for (z, x, y) in tiles {
            // A minimal MVT-ish payload; the merge never decodes it.
            w.add_tile(*z, *x, *y, &[0x1a, 0x02, 0x08, 0x01]).unwrap();
        }
        w.finalize(path).unwrap();
    }

    fn read_metadata(path: &Path) -> String {
        let bytes = std::fs::read(path).unwrap();
        let h = Header::from_bytes(&bytes).unwrap();
        let raw = &bytes[h.json_metadata_offset as usize
            ..(h.json_metadata_offset + h.json_metadata_length) as usize];
        let plain = compression::decompress(raw, h.internal_compression).unwrap();
        String::from_utf8(plain).unwrap()
    }

    /// The merge never called set_bounds, so the header carried
    /// TileBounds::empty() -- infinities saturating to min 214.7 / max -214.7 --
    /// and go-pmtiles rejected the archive: "bounds has area <= 0: clients may
    /// not display tiles correctly".
    #[test]
    fn merged_bounds_are_the_union_of_the_bands() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        write_band(
            &a,
            "agg",
            &[(2, 1, 1)],
            TileBounds::new(-10.0, -5.0, 0.0, 5.0),
            &[],
        );
        write_band(
            &b,
            "pts",
            &[(6, 30, 30)],
            TileBounds::new(-2.0, 1.0, 12.0, 20.0),
            &[],
        );

        let out = dir.path().join("merged.pmtiles");
        merge_bands(
            &[
                Band::parse(&format!("0-5:{}:agg", a.display())).unwrap(),
                Band::parse(&format!("6-9:{}:pts", b.display())).unwrap(),
            ],
            &out,
        )
        .unwrap();

        let bytes = std::fs::read(&out).unwrap();
        let h = Header::from_bytes(&bytes).unwrap();
        // Bounds round-trip through i32 * 1e-7, so compare with a tolerance.
        assert!((h.min_lon + 10.0).abs() < 1e-6, "min_lon {}", h.min_lon);
        assert!((h.min_lat + 5.0).abs() < 1e-6, "min_lat {}", h.min_lat);
        assert!((h.max_lon - 12.0).abs() < 1e-6, "max_lon {}", h.max_lon);
        assert!((h.max_lat - 20.0).abs() < 1e-6, "max_lat {}", h.max_lat);
        assert!(h.max_lon > h.min_lon && h.max_lat > h.min_lat, "area <= 0");
    }

    /// The old merge expanded every run-length entry into an independent
    /// `data.clone()` and fed them to a writer whose dedup path
    /// `add_tile_compressed` bypassed entirely: a 300-byte band archive of
    /// 4,096 identical tiles came back out at 164,136 bytes -- a 547x blowup.
    #[test]
    fn merge_preserves_deduplication() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("dense.pmtiles");
        // z6 is 64x64 = 4,096 tiles with identical payloads: the band writer
        // dedups them to one body and one long run, and so must the merge.
        let tiles: Vec<(u8, u32, u32)> = (0..64u32)
            .flat_map(|x| (0..64u32).map(move |y| (6u8, x, y)))
            .collect();
        write_band(
            &src,
            "agg",
            &tiles,
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[Band::parse(&format!("6-6:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap();
        assert_eq!(report.total_tiles, 4096);

        let src_len = std::fs::metadata(&src).unwrap().len();
        let out_len = std::fs::metadata(&out).unwrap().len();
        // Measured after the fix: 284 bytes in, 285 out -- 1.00x. The bound is
        // loose so a directory-encoding change cannot make this flap; anything
        // like the old 547x is orders of magnitude past it.
        assert!(
            out_len < src_len * 4,
            "merged archive is {out_len} bytes against a {src_len}-byte source \
             ({:.0}x); deduplication was lost",
            out_len as f64 / src_len as f64
        );
    }

    /// Brace counting ignored string literals, so a field named with a `}`
    /// truncated the lifted object -- `{"a}b":"String"}` came out as `{"a}` --
    /// and the merged metadata was not parseable JSON at all.
    #[test]
    fn brace_in_field_name_round_trips_to_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("braced.pmtiles");
        write_band(
            &src,
            "agg",
            &[(3, 1, 1)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[("a}b", "String")],
        );

        let out = dir.path().join("merged.pmtiles");
        merge_bands(
            &[Band::parse(&format!("3-3:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap();

        let meta = read_metadata(&out);
        let v: Value = serde_json::from_str(&meta).unwrap_or_else(|e| panic!("{e}: {meta}"));
        assert_eq!(
            v["vector_layers"][0]["fields"]["a}b"],
            json!("String"),
            "{meta}"
        );
    }

    /// The layer id reached the metadata through `format!`, so a `"` or `\` in
    /// a --band layer name produced invalid JSON.
    #[test]
    fn quote_in_layer_name_produces_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("q.pmtiles");
        write_band(
            &src,
            "agg",
            &[(3, 1, 1)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        // Built directly rather than through Band::parse: a quote is a legal
        // layer name character, it is the *serialization* that used to break.
        let band = Band {
            input: src.clone(),
            layer: r#"a"b\c"#.to_string(),
            min_zoom: 3,
            max_zoom: 3,
        };
        let out = dir.path().join("merged.pmtiles");
        merge_bands(&[band], &out).unwrap();

        let meta = read_metadata(&out);
        let v: Value = serde_json::from_str(&meta).unwrap_or_else(|e| panic!("{e}: {meta}"));
        assert_eq!(v["vector_layers"][0]["id"], json!(r#"a"b\c"#), "{meta}");
    }

    /// Tiles at a zoom the band does not declare used to vanish without a word.
    /// The likeliest cause is a --minzoom/--maxzoom that disagrees with --band,
    /// so the count has to reach the user.
    #[test]
    fn out_of_band_tiles_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("wide.pmtiles");
        write_band(
            &src,
            "agg",
            &[(3, 1, 1), (4, 2, 2), (5, 4, 4)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[Band::parse(&format!("3-3:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap();
        assert_eq!(report.total_tiles, 1);
        assert_eq!(report.skipped, 2);
    }

    /// A corrupt json_metadata_length used to index the file slice unchecked
    /// and panic: "range end index 100278 out of range for slice of length
    /// 300". A library function must not panic on untrusted input.
    #[test]
    fn corrupt_metadata_length_errors_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("corrupt.pmtiles");
        write_band(
            &src,
            "agg",
            &[(3, 1, 1)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let mut bytes = std::fs::read(&src).unwrap();
        // json_metadata_length is the fourth u64 of the header: bytes 32..40.
        bytes[32..40].copy_from_slice(&100_000u64.to_le_bytes());
        std::fs::write(&src, &bytes).unwrap();

        let out = dir.path().join("merged.pmtiles");
        let err = merge_bands(
            &[Band::parse(&format!("3-3:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("past end of"), "{err}");
    }
}
