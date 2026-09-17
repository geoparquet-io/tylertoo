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

use tempfile::NamedTempFile;

use crate::compression::{self, Compression};
use crate::dedup::TileHasher;
use crate::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use crate::overview::export::{export_pmtiles, ExportOptions};
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
    /// Web Mercator zoom ceiling. Beyond this the tile grid no longer fits the
    /// arithmetic downstream (`1u32 << zoom`), which overflows at z32 — a debug
    /// panic, and silently wrong tile buffers in release. Rejecting here costs
    /// an error message; accepting costs a full band conversion first.
    const MAX_ZOOM: u8 = 30;

    /// Parse `LO-HI:PATH[:LAYER]`. The layer is optional and defaults to the
    /// input's file stem.
    ///
    /// The zoom range is split off first. What remains is the path, *unless*
    /// its last colon-separated segment is a bare layer token — no `/`, `\` or
    /// `:` — in which case that is the layer. Paths legitimately contain
    /// colons: `s3://bucket/x.parquet`, `https://host/x.parquet`,
    /// `C:\data\x.parquet`. Splitting on the last colon unconditionally
    /// mangled every one of those, and remote sources are genuinely supported
    /// downstream, so the grammar has to admit them.
    ///
    /// One extra rule closes `C:data.parquet`, where the last segment *is* a
    /// bare token: a single-ASCII-letter candidate path is a Windows drive, so
    /// the whole remainder is the path. All of this is pure string work — no
    /// filesystem access, so it behaves the same for a glob or a URL.
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
        if max_zoom > Self::MAX_ZOOM {
            return Err(format!(
                "band {spec:?}: max zoom {max_zoom} is above the supported ceiling {}",
                Self::MAX_ZOOM
            ));
        }

        // A trailing colon is an empty layer name, not part of the path.
        // Falling through would silently keep the colon in the path.
        if rest.trim_end().ends_with(':') {
            return Err(format!("band {spec:?}: empty layer name"));
        }
        let (path, layer) = match rest.rsplit_once(':') {
            // A bare final segment is a layer name, unless what precedes it is
            // a lone drive letter.
            Some((p, l)) if is_bare_layer_token(l) && !p.is_empty() && !is_drive_letter(p) => {
                (p.trim(), Some(l.trim().to_string()))
            }
            _ => (rest.trim(), None),
        };
        if path.is_empty() {
            return Err(format!("band {spec:?}: empty input path"));
        }
        let layer = match layer {
            Some(l) if l.is_empty() => return Err(format!("band {spec:?}: empty layer name")),
            Some(l) => l,
            None => default_layer_for(path).ok_or_else(|| {
                format!(
                    "band {spec:?}: cannot derive a layer name from {path:?}; \
                     append :LAYER"
                )
            })?,
        };

        Ok(Band {
            input: PathBuf::from(path),
            layer,
            min_zoom,
            max_zoom,
        })
    }
}

/// Whether `s` could be an MVT layer id rather than the tail of a path.
/// Layer ids do not contain path separators or colons.
fn is_bare_layer_token(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty() && !s.contains(['/', '\\', ':'])
}

/// A single ASCII letter — i.e. a Windows drive designator, not a path.
fn is_drive_letter(s: &str) -> bool {
    let s = s.trim();
    s.len() == 1 && s.chars().all(|c| c.is_ascii_alphabetic())
}

/// The layer name implied by an input path: its file stem.
///
/// `None` when the stem carries glob metacharacters, which would otherwise
/// produce a layer literally called `*` — a working archive whose layer every
/// client has to reference as `"source-layer": "*"`. A glob's parent directory
/// is the useful name, and failing that the caller is asked for one.
fn default_layer_for(path: &str) -> Option<String> {
    // Strip a Windows drive prefix first: on Unix, `Path` has no notion of one,
    // so `C:data.parquet` would otherwise yield the stem `C:data` — a layer id
    // with a colon in it.
    let path = match path.split_once(':') {
        Some((drive, rest)) if is_drive_letter(drive) && !rest.is_empty() => rest,
        _ => path,
    };
    let p = Path::new(path);
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned());
    match stem {
        Some(s) if !s.is_empty() && !s.contains(['*', '?', '[']) => Some(s),
        _ => p
            .parent()
            .and_then(Path::file_name)
            .map(|s| s.to_string_lossy().into_owned())
            .filter(|s| !s.is_empty() && !s.contains(['*', '?', '['])),
    }
}

/// What a band's `input` actually is.
///
/// The one-shot form (#345) takes GeoParquet sources and tiles them here; the
/// two-step form takes archives already tiled for their zoom range. Both are
/// spelled `--band LO-HI:PATH[:LAYER]`, so the kind is detected rather than
/// declared — a caller should not have to tell tylertoo what its own file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandSource {
    /// A PMTiles archive, merged as-is.
    Archive,
    /// A GeoParquet source (file, directory, glob, or remote URL) to tile.
    Source,
}

/// Classify a band input by content, not by name.
///
/// A local file whose first bytes are the PMTiles v3 magic is an archive;
/// everything else is a source to tile. Sniffing beats trusting the extension
/// — `.pmtiles` is a convention, not a guarantee — and everything that is not
/// a readable local file (a glob, a directory, an `s3://` URL) can only be a
/// GeoParquet source here, since a band archive is always one local file.
pub fn classify_band_input(path: &Path) -> BandSource {
    use std::io::Read;

    let mut magic = [0u8; 7];
    match std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)) {
        Ok(()) if &magic == b"PMTiles" => BandSource::Archive,
        Ok(()) => BandSource::Source,
        Err(e) => {
            // "Cannot read it" is not the same as "it is a source". A file
            // that exists but cannot be opened would otherwise be handed to
            // the parquet reader and fail as a malformed source, hiding the
            // real cause. NotFound stays quiet — the CLI reports a missing
            // input by name before reaching here.
            if e.kind() != std::io::ErrorKind::NotFound {
                log::warn!(
                    "cannot read band input {}: {e}; treating it as a source to \
                     tile, which will fail if it is actually a PMTiles archive",
                    path.display()
                );
            }
            BandSource::Source
        }
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
        // A gap is legal — the caller may not want those zooms — but it is
        // rarely deliberate, and the merged archive cannot express it: the
        // header and `vector_layers` span min..max, so a client honouring
        // maxzoom renders the gap blank instead of overzooming the band below.
        if w[1].min_zoom > w[0].max_zoom + 1 {
            log::warn!(
                "no band covers z{}-{}: the merged archive still advertises \
                 those zooms (its range spans every band), so clients will \
                 request them and get nothing",
                w[0].max_zoom + 1,
                w[1].min_zoom - 1
            );
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

/// How the one-shot pyramid tiles each GeoParquet band.
#[derive(Debug, Clone)]
pub struct PyramidOptions {
    /// Convert knobs applied to every source band, with the band's own zoom
    /// range substituted for [`ConvertOptions::levels`].
    ///
    /// Defaults to [`ConvertOptions::verbatim`]: a pyramid band is an input
    /// that already *is* the right resolution for the zooms it owns — that is
    /// the whole premise of banding — so generalizing it would re-introduce
    /// the problem the pyramid exists to avoid. Callers that want the ladder
    /// inside a band pass a non-verbatim config.
    pub convert: ConvertOptions,
    /// Export knobs applied to every source band, with the band's own layer
    /// name substituted for [`ExportOptions::layer_name`].
    pub export: ExportOptions,
    /// Directory for the per-band intermediates (one overview GeoParquet and
    /// one PMTiles archive per source band, all removed on the way out).
    /// `None` uses the system temp directory.
    pub work_dir: Option<PathBuf>,
}

impl Default for PyramidOptions {
    fn default() -> Self {
        Self {
            convert: ConvertOptions::default().verbatim(),
            // Every feature of an aggregate band must be drawn, so the default
            // per-tile size valve — which sheds features to fit a byte budget
            // — is off, matching `tiles --verbatim`.
            export: ExportOptions {
                tile_size_limit: None,
                ..ExportOptions::default()
            },
            work_dir: None,
        }
    }
}

/// Build a pyramid in one shot: tile every GeoParquet band into its own zoom
/// range, then merge the results with any already-tiled bands (#345).
///
/// This is the form the issue asks for —
/// `--band "0-5:cells_r5.parquet:aggregate"` — and it is deliberately thin:
/// each source band runs the ordinary convert → export chain restricted to its
/// zoom range, and [`merge_bands`] stitches the archives by tile id. Bands own
/// disjoint zoom ranges, so no tile is ever claimed twice and the merge stays a
/// concatenation rather than a per-tile layer union.
///
/// Bands may mix kinds freely: a band pointing at a PMTiles archive is used
/// as-is (the two-step form), one pointing at anything else is tiled here.
/// [`classify_band_input`] decides, by content.
///
/// Intermediates live in `work_dir` and are removed whether the build succeeds
/// or fails.
pub fn build_pyramid(
    bands: &[Band],
    output: &Path,
    opts: &PyramidOptions,
) -> Result<PyramidReport, Error> {
    validate_bands(bands).map_err(Error::PMTilesWrite)?;

    // Keeps every intermediate alive for the merge and unlinks them on drop —
    // including the early-return paths below.
    let mut scratch: Vec<NamedTempFile> = Vec::new();
    let mut tiled: Vec<Band> = Vec::with_capacity(bands.len());

    for band in bands {
        if classify_band_input(&band.input) == BandSource::Archive {
            log::info!(
                "[pyramid] z{}-{} layer {:?}: using the pre-tiled archive {}",
                band.min_zoom,
                band.max_zoom,
                band.layer,
                band.input.display(),
            );
            tiled.push(band.clone());
            continue;
        }

        log::info!(
            "[pyramid] z{}-{} layer {:?}: tiling {}{}",
            band.min_zoom,
            band.max_zoom,
            band.layer,
            band.input.display(),
            if opts.convert.is_verbatim() {
                " (verbatim)"
            } else {
                ""
            },
        );

        let named = |suffix: &str| -> Result<NamedTempFile, Error> {
            let mut b = tempfile::Builder::new();
            b.prefix("tylertoo-pyramid-").suffix(suffix);
            match &opts.work_dir {
                Some(dir) => b.tempfile_in(dir),
                None => b.tempfile(),
            }
            .map_err(|e| Error::PMTilesWrite(format!("band {:?}: {e}", band.layer)))
        };

        let overview = named(".parquet")?;
        let archive = named(".pmtiles")?;

        let convert = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: band.min_zoom,
                max_zoom: band.max_zoom,
            },
            ..opts.convert.clone()
        };
        convert_to_overviews(&band.input, overview.path(), &convert).map_err(|e| {
            Error::PMTilesWrite(format!("band {:?}: convert failed: {e}", band.layer))
        })?;

        let export = ExportOptions {
            layer_name: band.layer.clone(),
            ..opts.export.clone()
        };
        export_pmtiles(overview.path(), archive.path(), &export).map_err(|e| {
            Error::PMTilesWrite(format!("band {:?}: export failed: {e}", band.layer))
        })?;

        tiled.push(Band {
            input: archive.path().to_path_buf(),
            layer: band.layer.clone(),
            min_zoom: band.min_zoom,
            max_zoom: band.max_zoom,
        });
        // The overview is only needed for the export above; the archive must
        // outlive the merge.
        drop(overview);
        scratch.push(archive);
    }

    merge_bands(&tiled, output)
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

    /// A band input is classified by content, not by name: `.pmtiles` is a
    /// convention and a source can be a glob, a directory or a URL that no
    /// extension rule would get right.
    #[test]
    fn band_input_kind_is_detected_by_content() {
        let dir = tempfile::tempdir().unwrap();

        // Real PMTiles magic, misleading extension.
        let archive = dir.path().join("looks-like-data.parquet");
        std::fs::write(&archive, b"PMTiles\x03rest-of-header").unwrap();
        assert_eq!(classify_band_input(&archive), BandSource::Archive);

        // Real parquet magic, misleading extension.
        let source = dir.path().join("looks-like-an-archive.pmtiles");
        std::fs::write(&source, b"PAR1........").unwrap();
        assert_eq!(classify_band_input(&source), BandSource::Source);

        // A glob, a directory and a remote URL are all sources: a band archive
        // is always one local file.
        assert_eq!(
            classify_band_input(Path::new("/data/cells/*.parquet")),
            BandSource::Source
        );
        assert_eq!(classify_band_input(dir.path()), BandSource::Source);
        assert_eq!(
            classify_band_input(Path::new("s3://bucket/cells.parquet")),
            BandSource::Source
        );

        // A file too short to hold the magic is not an archive.
        let stub = dir.path().join("tiny");
        std::fs::write(&stub, b"PM").unwrap();
        assert_eq!(classify_band_input(&stub), BandSource::Source);
    }

    /// The one-shot form (#345): GeoParquet in, one banded archive out. Each
    /// band contributes tiles only within its own zoom range, and the bands
    /// share a layer name — the FIRMS shape, where a coarse aggregate
    /// and a fine one are the same layer to a client.
    #[test]
    fn build_pyramid_tiles_geoparquet_bands_into_one_archive() {
        let dir = tempfile::tempdir().unwrap();
        let coarse = dir.path().join("coarse.parquet");
        let fine = dir.path().join("fine.parquet");
        write_cell_source(&coarse, 40, 0.6);
        write_cell_source(&fine, 120, 0.2);

        let out = dir.path().join("pyramid.pmtiles");
        let bands = vec![
            Band {
                input: coarse.clone(),
                layer: "aggregate".to_string(),
                min_zoom: 0,
                max_zoom: 2,
            },
            Band {
                input: fine.clone(),
                layer: "aggregate".to_string(),
                min_zoom: 3,
                max_zoom: 4,
            },
        ];
        let report = build_pyramid(
            &bands,
            &out,
            &PyramidOptions {
                work_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.per_band_tiles.len(), 2);
        assert!(report.total_tiles > 0);
        assert_eq!(
            report.skipped, 0,
            "every tile must fall inside its band's declared range"
        );

        // Each band owns its zooms exclusively.
        let zooms = archive_zooms(&out);
        assert_eq!(
            zooms,
            vec![0, 1, 2, 3, 4],
            "the merged archive must span every band's range"
        );

        // Intermediates are cleaned up: only the output remains.
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("tylertoo-pyramid-"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    /// Bands may mix kinds: an already-tiled archive alongside a source that
    /// this call tiles. Both spellings are `--band LO-HI:PATH[:LAYER]`.
    #[test]
    fn build_pyramid_mixes_pre_tiled_and_source_bands() {
        let dir = tempfile::tempdir().unwrap();

        // Band 1: tile a source the normal way, standing in for an archive a
        // previous run produced.
        let src = dir.path().join("pre.parquet");
        write_cell_source(&src, 30, 0.6);
        let pre = dir.path().join("pre.pmtiles");
        build_pyramid(
            &[Band {
                input: src,
                layer: "aggregate".to_string(),
                min_zoom: 0,
                max_zoom: 1,
            }],
            &pre,
            &PyramidOptions {
                work_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(classify_band_input(&pre), BandSource::Archive);

        // Band 2: a source, tiled now.
        let fine = dir.path().join("fine.parquet");
        write_cell_source(&fine, 90, 0.25);

        let out = dir.path().join("mixed.pmtiles");
        let report = build_pyramid(
            &[
                Band {
                    input: pre,
                    layer: "aggregate".to_string(),
                    min_zoom: 0,
                    max_zoom: 1,
                },
                Band {
                    input: fine,
                    layer: "features".to_string(),
                    min_zoom: 2,
                    max_zoom: 3,
                },
            ],
            &out,
            &PyramidOptions {
                work_dir: Some(dir.path().to_path_buf()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.skipped, 0);
        assert_eq!(archive_zooms(&out), vec![0, 1, 2, 3]);
    }

    /// The premise of a band is that its input is already the right resolution
    /// for the zooms it owns, so the default must not generalize it away —
    /// that is the #360 failure the pyramid exists to sidestep.
    #[test]
    fn build_pyramid_defaults_to_verbatim_bands() {
        assert!(
            PyramidOptions::default().convert.is_verbatim(),
            "a pyramid band must be tiled as given by default"
        );
        assert_eq!(
            PyramidOptions::default().export.tile_size_limit,
            None,
            "a size valve that sheds features is not verbatim either"
        );
    }

    /// An overlapping range is caught before any band is tiled, so a mistake
    /// costs an error rather than a full conversion.
    #[test]
    fn build_pyramid_rejects_overlap_before_doing_work() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("cells.parquet");
        write_cell_source(&src, 20, 0.5);
        let out = dir.path().join("out.pmtiles");

        let err = build_pyramid(
            &[
                Band {
                    input: src.clone(),
                    layer: "a".to_string(),
                    min_zoom: 0,
                    max_zoom: 3,
                },
                Band {
                    input: src,
                    layer: "b".to_string(),
                    min_zoom: 3,
                    max_zoom: 5,
                },
            ],
            &out,
            &PyramidOptions::default(),
        )
        .unwrap_err();
        assert!(format!("{err}").contains("overlap at zoom 3"), "{err}");
        assert!(!out.exists(), "nothing should be written on a bad plan");
    }

    /// A grid of uniform cells: the shape of a DGGS aggregate band.
    fn write_cell_source(path: &Path, n: usize, size: f64) {
        use geo::{Geometry, LineString, Polygon};

        let side = (n as f64).sqrt().ceil() as usize;
        let geoms: Vec<Option<Geometry<f64>>> = (0..n)
            .map(|i| {
                let (x, y) = (
                    -20.0 + (i % side) as f64 * size * 1.2,
                    10.0 + (i / side) as f64 * size * 1.2,
                );
                Some(Geometry::Polygon(Polygon::new(
                    LineString::from(vec![
                        (x, y),
                        (x + size, y),
                        (x + size, y + size),
                        (x, y + size),
                        (x, y),
                    ]),
                    vec![],
                )))
            })
            .collect();
        crate::overview::testutil::write_input(path, &geoms, true, None);
    }

    /// The distinct zooms present in a PMTiles archive, ascending.
    fn archive_zooms(path: &Path) -> Vec<u8> {
        let bytes = std::fs::read(path).unwrap();
        let header = Header::from_bytes(&bytes[..127]).unwrap();
        let root = compression::decompress(
            &bytes[header.root_dir_offset as usize
                ..(header.root_dir_offset + header.root_dir_length) as usize],
            Compression::Gzip,
        )
        .unwrap();
        let mut zooms: Vec<u8> = decode_directory(&root)
            .unwrap()
            .iter()
            .filter(|e| e.run_length > 0)
            .map(|e| tile_id_to_zxy(e.tile_id).unwrap().0)
            .collect();
        zooms.sort_unstable();
        zooms.dedup();
        zooms
    }

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

    /// The grammar, as a table — the thing that was missing.
    ///
    /// Every earlier test asserted on hand-built `Band` structs or on
    /// `classify_band_input` directly, so nothing exercised the real entry
    /// point. That is why `s3://` URLs and globs were broken while their unit
    /// tests passed: a colon is legal in a path, and splitting on the last one
    /// mangled every URL and drive letter.
    #[test]
    fn band_spec_grammar() {
        let ok = |spec: &str| Band::parse(spec).unwrap_or_else(|e| panic!("{spec:?}: {e}"));

        // Paths that contain colons, with and without an explicit layer.
        let b = ok("0-5:s3://bucket/cells.parquet");
        assert_eq!(b.input, PathBuf::from("s3://bucket/cells.parquet"));
        assert_eq!(b.layer, "cells");

        let b = ok("0-5:s3://bucket/cells.parquet:aggregate");
        assert_eq!(b.input, PathBuf::from("s3://bucket/cells.parquet"));
        assert_eq!(b.layer, "aggregate");

        assert_eq!(
            ok("0-5:https://host/x.parquet").input,
            PathBuf::from("https://host/x.parquet")
        );
        assert_eq!(
            ok("0-9:/data/2024:06/x.pmtiles").input,
            PathBuf::from("/data/2024:06/x.pmtiles")
        );

        // Windows: a drive letter is a path, not a layer, both spellings.
        assert_eq!(
            ok("0-5:C:/data/x.pmtiles").input,
            PathBuf::from("C:/data/x.pmtiles")
        );
        let b = ok("0-5:C:data.parquet");
        assert_eq!(b.input, PathBuf::from("C:data.parquet"), "drive-relative");
        assert_eq!(b.layer, "data");

        // The ordinary three-part form still splits.
        let b = ok("6-8:cells_r8.parquet:aggregate");
        assert_eq!(b.input, PathBuf::from("cells_r8.parquet"));
        assert_eq!(b.layer, "aggregate");

        // A glob must not become a layer called "*".
        let b = ok("0-5:/data/cells/*.parquet");
        assert_eq!(b.layer, "cells", "glob falls back to the directory name");
        assert_eq!(ok("0-5:/data/cells/").layer, "cells", "directory input");

        // Whitespace is trimmed on both sides of the split.
        let b = ok("0-5: x.parquet : agg ");
        assert_eq!(b.input, PathBuf::from("x.parquet"));
        assert_eq!(b.layer, "agg");

        // Rejections.
        for bad in [
            "0-5",             // no path
            "x:y.parquet",     // no range
            "5-0:x.parquet",   // reversed
            "0-300:x.parquet", // not a u8
            "-1-5:x.parquet",  // negative
            "0-5:",            // empty path
            "0-5:x.parquet:",  // empty layer
            "0-31:x.parquet",  // above the zoom ceiling
            "0-32:x.parquet",  // the overflow threshold itself
        ] {
            assert!(Band::parse(bad).is_err(), "{bad:?} must be rejected");
        }

        // The ceiling is inclusive at 30.
        assert_eq!(ok("0-30:x.parquet").max_zoom, 30);
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
