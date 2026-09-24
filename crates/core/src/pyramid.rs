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
//! Each band is tiled by the ordinary pipeline and the archives are stitched
//! by tile id. Bands of the same layer must not share a zoom — they would
//! write the same tile ids — so within a layer the merge is a plain
//! concatenation of archives. Bands in *different* layers may share zooms
//! (tippecanoe's `-L`, #385): at a shared zoom a tile several bands wrote has
//! their layer messages concatenated, and a tile only one band wrote passes
//! through untouched.
//!
//! See issues #345 and #385.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use tempfile::NamedTempFile;

use crate::compression::{self, Compression};
use crate::dedup::TileHasher;
use crate::input::url_scheme;
use crate::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use crate::overview::export::{export_pmtiles, ExportOptions};
use crate::pmtiles_writer::{
    decode_directory, tile_id_to_zxy, DirEntry, Header, StreamingPmtilesWriter, TileType,
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

    /// Parse a band spec. The layer is optional and defaults to the input's
    /// file stem. Two spellings are accepted (#482):
    ///
    /// * `LO-HI:INPUT[:LAYER]` — the usual one.
    /// * `LO-HI=INPUT[=LAYER]` — an escape form for an INPUT the colon form
    ///   cannot express, `=` being invalid in a zoom range.
    ///
    /// Which one a spec is in is decided by whichever of `:` and `=` closes
    /// the zoom range, i.e. whichever comes first ([`band_separator`]).
    ///
    /// In the `:` form the remainder is the INPUT, *unless* its last
    /// colon-separated segment is a bare layer token — no `/`, `\` or `:`.
    /// Paths legitimately contain colons: `s3://bucket/x.parquet`,
    /// `https://host/x.parquet`, `C:\data\x.parquet`. The rule is
    /// scheme-aware: in a `scheme://…` remainder the scheme's own colon and
    /// any colon inside the URL path (a port, a `2024:06` directory) are part
    /// of the URL, and only a *last* colon followed by a segment with no `/`
    /// starts a LAYER. One extra rule closes `C:data.parquet`, where the last
    /// segment *is* a bare token: a single-ASCII-letter candidate path is a
    /// Windows drive, so the whole remainder is the path.
    ///
    /// In the `=` form the range is split at the FIRST `=` and a LAYER at the
    /// LAST `=`, and again only when the segment after it is a bare layer
    /// token. That keeps a Hive path such as
    /// `admin:country_code=BR/part.parquet` whole; an input that *ends* in
    /// `=VALUE` is indistinguishable from a layer, so those need an explicit
    /// trailing `=LAYER`.
    ///
    /// All of this is pure string work — no filesystem access, so it behaves
    /// the same for a glob or a URL.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (sep, at) =
            band_separator(spec).ok_or_else(|| format!("band {spec:?}: {BAND_FORMS}"))?;
        let (range, rest) = spec.split_at(at);
        let rest = &rest[sep.len_utf8()..];
        let (lo, hi) = range.split_once('-').ok_or_else(|| {
            format!(
                "band {spec:?}: zoom range must be LO-HI; every band spec must \
                 start with a LO-HI: (or LO-HI=) zoom range, e.g. \"0-5:{spec}\""
            )
        })?;
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

        // A trailing separator is an empty layer name, not part of the path.
        // Falling through would silently keep it in the path.
        if rest.trim_end().ends_with(sep) {
            return Err(format!("band {spec:?}: empty layer name"));
        }
        let (path, layer) = match sep {
            ':' => split_colon_form(rest),
            _ => split_equals_form(rest),
        };
        if path.is_empty() {
            return Err(format!("band {spec:?}: empty input path"));
        }
        let layer = match layer {
            Some("") => return Err(format!("band {spec:?}: empty layer name")),
            Some(l) => l.to_string(),
            None => default_layer_for(path).ok_or_else(|| {
                format!(
                    "band {spec:?}: cannot derive a layer name from {path:?}; \
                     append {sep}LAYER"
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

/// Both accepted band spellings, named in every ill-formed-spec error so the
/// escape form is discoverable from the message rather than the manual.
const BAND_FORMS: &str = "expected LO-HI:INPUT[:LAYER] or LO-HI=INPUT[=LAYER]";

/// Which separator closes the zoom range, and its byte offset.
///
/// A zoom range is digits and `-`, so whichever of `:` and `=` appears first
/// is the one that closes it. That is what lets
/// `0-13=admin:country_code=BR/part.parquet` be the `=` form while
/// `0-9:/data/country_code=BR/part.parquet` stays the `:` form.
fn band_separator(spec: &str) -> Option<(char, usize)> {
    match (spec.find(':'), spec.find('=')) {
        (Some(c), Some(e)) => Some(if e < c { ('=', e) } else { (':', c) }),
        (Some(c), None) => Some((':', c)),
        (None, Some(e)) => Some(('=', e)),
        (None, None) => None,
    }
}

/// Split `INPUT[:LAYER]` — see [`Band::parse`] for the rule.
fn split_colon_form(rest: &str) -> (&str, Option<&str>) {
    let rest = rest.trim();
    if let Some(scheme) = url_scheme(rest) {
        // Past the `scheme://`, so the scheme's own colon can never split.
        //
        // This branch is provably equivalent to the generic fallback below
        // (rsplit_once(':') on the whole `rest`, minus the drive-letter
        // carve-out which cannot fire here — a scheme is never one ASCII
        // letter) for every realizable input: if `authority_and_path`
        // contains a `:`, it is the last `:` in `rest` too, since nothing
        // after the scheme prefix can contain one that isn't in it, so both
        // splits land on the same byte. If it contains none, the fallback's
        // rsplit_once(':') on `rest` finds the scheme's own colon instead,
        // splitting off `"//" + authority_and_path` as the candidate
        // layer — but that always contains the `/` from `"://"`, so
        // `is_bare_layer_token` rejects it and the fallback also keeps
        // `rest` whole. Kept as its own branch (rather than folded into the
        // fallback) because slicing by byte offset here is clearer than
        // re-deriving the scheme boundary from the split fallback would be.
        let authority_and_path = &rest[scheme.len() + 3..];
        return match authority_and_path.rsplit_once(':') {
            // `is_bare_layer_token` rejects a segment containing `/`, which is
            // what keeps a port (`host:8080/x.parquet`) and a colon inside the
            // URL path out of the layer slot.
            Some((p, l)) if is_bare_layer_token(l) => (
                rest[..scheme.len() + 3 + p.len()].trim_end(),
                Some(l.trim()),
            ),
            _ => (rest, None),
        };
    }
    match rest.rsplit_once(':') {
        // A bare final segment is a layer name, unless what precedes it is a
        // lone drive letter.
        Some((p, l)) if is_bare_layer_token(l) && !p.is_empty() && !is_drive_letter(p) => {
            (p.trim(), Some(l.trim()))
        }
        _ => (rest, None),
    }
}

/// Split `INPUT[=LAYER]` — see [`Band::parse`] for the rule.
fn split_equals_form(rest: &str) -> (&str, Option<&str>) {
    let rest = rest.trim();
    match rest.rsplit_once('=') {
        Some((p, l)) if is_bare_layer_token(l) && !p.trim().is_empty() => {
            (p.trim(), Some(l.trim()))
        }
        _ => (rest, None),
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

/// Whether a band input is a URL rather than a local path (#482).
///
/// Shape only — the same test [`crate::input::InputSource::from_path`] makes,
/// so what looks remote here is exactly what the reader will fetch. An
/// unsupported scheme is reported there, by name, rather than silently
/// becoming a path.
fn is_remote_input(path: &Path) -> bool {
    path.to_str().is_some_and(|s| url_scheme(s).is_some())
}

/// The error for a remote band input that is plainly a PMTiles archive, or
/// `None` when the input is fine.
///
/// A band archive is read as a local file — directories, then tile bytes by
/// offset — so a remote one is not supported. It cannot be *sniffed* either
/// without fetching, so this is the one place the extension is trusted: it
/// only has to be good enough to replace a parquet reader's complaint about
/// a magic number with a sentence that says what to do.
fn remote_archive_rejection(input: &Path) -> Option<String> {
    if !is_remote_input(input) {
        return None;
    }
    let s = input.to_str()?;
    // A query string or fragment is part of the URL, not of the name.
    let name = s.split(['?', '#']).next().unwrap_or(s);
    if !name.to_ascii_lowercase().ends_with(".pmtiles") {
        return None;
    }
    Some(format!(
        "band input {s:?}: a remote band must be GeoParquet; stage PMTiles \
         archives locally and point the band at the local file"
    ))
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
///
/// A `scheme://` input short-circuits to [`BandSource::Source`] (#482) without
/// touching the filesystem: a band archive has to be read as a local file, so
/// a remote input can only be a GeoParquet source, and the open below could
/// only ever fail for it.
///
/// **This function does not reject a remote `.pmtiles` input** — it has no
/// error case, only two classifications, and a `scheme://foo.pmtiles` input
/// classifies as `Source` exactly like any other remote path (it will only
/// fail later, obscurely, inside the parquet reader). [`validate_bands`]
/// applies [`remote_archive_rejection`] up front to every band before any
/// band is tiled; a caller that classifies bands without going through
/// `validate_bands` first (or `build_pyramid`, which calls it) must apply
/// `remote_archive_rejection` itself to get that check.
pub fn classify_band_input(path: &Path) -> BandSource {
    use std::io::Read;

    if is_remote_input(path) {
        return BandSource::Source;
    }

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

/// Reject two bands claiming one zoom **for the same layer**: each would write
/// the same tile ids and the merge would silently keep whichever came last.
/// Also rejects any band naming a remote `.pmtiles` archive (#482) — checked
/// for every band up front, before any band is tiled.
///
/// The remote-archive check is pure string work (no I/O), so doing it here
/// for all bands is free and catches a late band's bad input before an
/// expensive earlier band is tiled — `build_pyramid` used to run this check
/// per band interleaved with tiling, so a later band's error only surfaced
/// after every band ahead of it had already converted.
///
/// Bands naming *different* layers may share zooms (#385): that is
/// tippecanoe's `-L`, several layers in one tile, and the merge concatenates
/// their layer messages tile by tile.
pub fn validate_bands(bands: &[Band]) -> Result<(), String> {
    if bands.is_empty() {
        return Err("a pyramid needs at least one --band".to_string());
    }
    for band in bands {
        if let Some(msg) = remote_archive_rejection(&band.input) {
            return Err(msg);
        }
    }
    let mut sorted: Vec<&Band> = bands.iter().collect();
    sorted.sort_by_key(|b| (b.min_zoom, b.max_zoom));
    for (i, a) in sorted.iter().enumerate() {
        for b in &sorted[i + 1..] {
            if b.min_zoom <= a.max_zoom && a.layer == b.layer {
                return Err(format!(
                    "bands overlap at zoom {} in layer {:?}: {}-{} and {}-{}",
                    b.min_zoom.max(a.min_zoom),
                    a.layer,
                    a.min_zoom,
                    a.max_zoom,
                    b.min_zoom,
                    b.max_zoom
                ));
            }
        }
    }
    // A gap is legal — the caller may not want those zooms — but it is
    // rarely deliberate, and the merged archive cannot express it: the
    // header and `vector_layers` span min..max, so a client honouring
    // maxzoom renders the gap blank instead of overzooming the band below.
    let lo = sorted.iter().map(|b| b.min_zoom).min().unwrap_or(0);
    let hi = sorted.iter().map(|b| b.max_zoom).max().unwrap_or(0);
    let mut gap_start: Option<u8> = None;
    for z in lo..=hi {
        let covered = sorted.iter().any(|b| b.min_zoom <= z && z <= b.max_zoom);
        match (covered, gap_start) {
            (false, None) => gap_start = Some(z),
            (true, Some(g)) => {
                log::warn!(
                    "no band covers z{g}-{}: the merged archive still advertises \
                     those zooms (its range spans every band), so clients will \
                     request them and get nothing",
                    z - 1
                );
                gap_start = None;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Whether any two bands share a zoom (necessarily in different layers once
/// [`validate_bands`] has passed).
fn bands_share_zooms(bands: &[Band]) -> bool {
    bands.iter().enumerate().any(|(i, a)| {
        bands[i + 1..]
            .iter()
            .any(|b| a.min_zoom <= b.max_zoom && b.min_zoom <= a.max_zoom)
    })
}

/// Whether a band's layer name is simply its input's file stem — i.e. the
/// spec almost certainly did not spell out a `:LAYER`.
///
/// DIVERGENCE FROM THE ISSUE (#405): the issue asks `Band::parse` to record
/// the flag on `Band`. `Band`'s fields are public and the struct is
/// exhaustively constructible, so a new field — public or private — is a
/// major semver break that `cargo semver-checks` rejects without a version
/// bump. The name is derived instead, with exactly the function `parse`
/// itself would have used, which keeps the public API additive. The one
/// inaccuracy is an explicit label that repeats the stem (`x.parquet:x`); it
/// can only make the warning below appear where it was not needed, never
/// suppress it where it was.
fn layer_is_input_derived(band: &Band) -> bool {
    band.input
        .to_str()
        .and_then(default_layer_for)
        .is_some_and(|derived| derived == band.layer)
}

/// Whether two zoom ranges overlap *partially* — they share at least one
/// zoom, but neither range contains the other.
///
/// Identical ranges and strict containment are excluded: both are the
/// ordinary shape of deliberate layer stacking (tippecanoe's `-L`) — the same
/// zooms, or one band's range nested inside another's, on purpose. A partial
/// overlap (e.g. `0-5` and `5-13`, sharing only z5) is the shape an off-by-one
/// range typo produces, so it is the only case worth flagging.
fn zoom_ranges_partially_overlap(a: &Band, b: &Band) -> bool {
    let overlaps = a.min_zoom <= b.max_zoom && b.min_zoom <= a.max_zoom;
    let a_contains_b = a.min_zoom <= b.min_zoom && b.max_zoom <= a.max_zoom;
    let b_contains_a = b.min_zoom <= a.min_zoom && a.max_zoom <= b.max_zoom;
    overlaps && !a_contains_b && !b_contains_a
}

/// Index pairs into `bands` that *partially* share a zoom range — a proper,
/// non-containment overlap — where at least one of the two was never spelled
/// out in the spec (#405).
///
/// Bands in different layers are allowed to share zooms — that is
/// tippecanoe's `-L`. But a band whose layer name came from its file stem did
/// not *ask* to be its own layer, so `--band 0-5:coarse.parquet --band
/// 5-13:fine.parquet` (one zoom too wide) quietly produces two layers instead
/// of the error it used to. Identical ranges and strict containment are the
/// documented multi-layer workflow (two bands, same or nested zoom range,
/// different stem-derived layers) and are not reported — only a partial
/// overlap, which is the shape a real off-by-one produces. Indices, not
/// names, so a caller ([`build_pyramid`]) can also tell which *band* — not
/// just which layer label — is on each side.
fn implicit_overlap_pairs(bands: &[Band]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (i, a) in bands.iter().enumerate() {
        for (j, b) in bands.iter().enumerate().skip(i + 1) {
            let either_implicit = layer_is_input_derived(a) || layer_is_input_derived(b);
            if zoom_ranges_partially_overlap(a, b) && a.layer != b.layer && either_implicit {
                out.push((i, j));
            }
        }
    }
    out
}

/// Pairs of layer names for [`implicit_overlap_pairs`]. Pure so it can be
/// tested directly; the caller warns rather than errors, since stem-named
/// layers over one range are a legitimate workflow.
fn implicit_layer_overlaps(bands: &[Band]) -> Vec<(String, String)> {
    implicit_overlap_pairs(bands)
        .into_iter()
        .map(|(i, j)| (bands[i].layer.clone(), bands[j].layer.clone()))
        .collect()
}

/// Say so, once, when `pairs` is non-empty — one line naming every offending
/// pair, not one line per pair.
fn warn_pairs(pairs: &[(String, String)]) {
    if pairs.is_empty() {
        return;
    }
    let list = pairs
        .iter()
        .map(|(a, b)| format!("{a:?}/{b:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    log::warn!(
        "bands partially share a zoom range, so the archive gets two layers named after \
         their inputs: {list}; if a pair is meant to be one layer give both the same \
         explicit `:LAYER`, and if not, check the zoom ranges for an off-by-one"
    );
}

/// Say so, once, when [`implicit_layer_overlaps`] finds anything.
fn warn_implicit_layer_overlaps(bands: &[Band]) {
    warn_pairs(&implicit_layer_overlaps(bands));
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
/// archive, not the expanded tile set. With disjoint zoom ranges only one
/// band is open at a time; when bands share zooms every band's archive is
/// open together, since one tile id can then draw on several of them.
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
    /// Every `vector_layers[*].id` the archive declares — the MVT layer
    /// name(s) actually inside its tiles, as opposed to the label `--band`
    /// gives it. Empty when the metadata has none.
    layer_ids: Vec<String>,
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
    ///
    /// `label` is the band's own `--band LO-HI:PATH:LAYER` name (or, for a
    /// caller with no such label to check against, an empty string — which
    /// never matches an `id`, so `fields` falls back to `vector_layers[0]`
    /// exactly as before, still with the mismatch warning). It is used only
    /// to pick the right `vector_layers` entry when the archive's metadata
    /// carries several (see [`parse_layers`]).
    fn open(path: &Path, label: &str) -> Result<Self, Error> {
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
        // The merge concatenates tile bodies at shared zooms, which is only
        // meaningful for MVT: two PNGs glued together are not a PNG.
        if header.tile_type != TileType::Mvt {
            return Err(Error::PMTilesWrite(format!(
                "{}: tile type is {:?}, but a pyramid band must hold MVT tiles",
                path.display(),
                header.tile_type
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
        let (fields, layer_ids) = parse_layers(raw_meta, header.internal_compression, path, label)?;
        let bounds = usable_bounds(&header);

        Ok(BandArchive {
            bytes,
            header,
            entries,
            fields,
            layer_ids,
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
        self.for_each_tile_range(|_, z, x, y, range| f(z, x, y, &self.bytes[range]))
    }

    /// Like [`Self::for_each_tile`], but hands out the tile id alongside
    /// `(z, x, y)` and the tile's byte range in the archive instead of the
    /// slice, so a caller can index tiles across several archives first and
    /// read them later in id order (#385).
    fn for_each_tile_range<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(u64, u8, u32, u32, std::ops::Range<usize>) -> Result<(), Error>,
    {
        // Offsets, lengths and ids are archive-supplied: every add is checked
        // so a corrupt directory reports rather than wraps.
        let past_end = || Error::PMTilesWrite("tile data past end of archive".to_string());
        for e in &self.entries {
            let start = self
                .header
                .tile_data_offset
                .checked_add(e.offset)
                .and_then(|s| usize::try_from(s).ok())
                .ok_or_else(past_end)?;
            let end = start
                .checked_add(e.length as usize)
                .filter(|&x| x <= self.bytes.len())
                .ok_or_else(past_end)?;
            for i in 0..u64::from(e.run_length.max(1)) {
                let id = e
                    .tile_id
                    .checked_add(i)
                    .ok_or_else(|| Error::PMTilesWrite("tile id past end of range".to_string()))?;
                let (z, x, y) = tile_id_to_zxy(id)
                    .map_err(|e| Error::PMTilesWrite(format!("bad tile id: {e}")))?;
                f(id, z, x, y, start..end)?;
            }
        }
        Ok(())
    }

    /// A tile's still-compressed bytes by range (from
    /// [`Self::for_each_tile_range`]).
    fn tile(&self, range: std::ops::Range<usize>) -> &[u8] {
        &self.bytes[range]
    }
}

/// Lift the `vector_layers` entry whose `id` matches `label` — the band's
/// `--band LO-HI:PATH:LAYER` name — and every `vector_layers[*].id`, out of a
/// band archive's JSON metadata.
///
/// Parsed with `serde_json` rather than scanned for `"fields":` and brace
/// counted. The hand-rolled version had two failure modes: its depth counter
/// underflowed on a `}` seen before any `{` (a panic in debug, a wrap and a
/// silent `{}` in release), and it counted braces inside string literals, so a
/// field *named* with a `}` truncated the object into invalid JSON.
///
/// An archive can carry several `vector_layers` entries (the shared-zoom
/// path merges bands of different layers into one file, #385, and a band
/// re-read from a previous pyramid run is exactly such a file). Picking
/// `[0]` unconditionally lifted whichever layer happened to be first,
/// regardless of which one `label` actually names — silently wrong the
/// moment a band's label is not that first entry. `label` is matched
/// against `id` instead; `[0]` is kept only as a last-resort fallback, with
/// a warning naming the mismatch, for the case where nothing matches (e.g. a
/// hand-built archive whose metadata never declared the label at all).
fn parse_layers(
    raw: &[u8],
    internal: Compression,
    path: &Path,
    label: &str,
) -> Result<(Value, Vec<String>), Error> {
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
            return Ok((json!({}), Vec::new()));
        }
    };
    let layers = parsed.get("vector_layers").and_then(Value::as_array);
    let matching = layers.and_then(|v| {
        v.iter()
            .find(|l| l.get("id").and_then(Value::as_str) == Some(label))
    });
    let chosen = match (matching, layers.and_then(|v| v.first())) {
        (Some(m), _) => Some(m),
        (None, Some(first)) => {
            log::warn!(
                "{}: no vector_layers entry named {label:?}; using {:?} instead \
                 ({} field(s) may not match this band's actual layer)",
                path.display(),
                first.get("id").and_then(Value::as_str).unwrap_or("?"),
                first
                    .get("fields")
                    .and_then(Value::as_object)
                    .map_or(0, serde_json::Map::len),
            );
            Some(first)
        }
        (None, None) => None,
    };
    let fields = chosen
        .and_then(|l| l.get("fields"))
        .filter(|f| f.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let ids = layers
        .map(|v| {
            v.iter()
                .filter_map(|l| l.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok((fields, ids))
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
/// zoom range, and [`merge_bands`] stitches the archives by tile id. Bands of
/// the same layer must not share a zoom; bands in different layers may, and at
/// a shared zoom the tile's layers are concatenated (tiles only one band wrote
/// pass through untouched).
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
    // Validates every band up front, including (#482) that none names a
    // remote `.pmtiles` archive — pure string work, so it is cheap to run for
    // every band before any band is tiled, rather than interleaved with
    // tiling below where a late band's bad input would only surface after
    // every earlier band had already converted.
    validate_bands(bands).map_err(Error::PMTilesWrite)?;

    // Classified once, here, and reused below for the tiling loop: a band
    // that is about to be tiled loses the filename it was declared with —
    // `tiled` below replaces its `input` with a randomly-named temp archive
    // — so `implicit_layer_overlaps`'s file-stem heuristic can no longer see
    // it once that happens. Any overlapping pair with a to-be-tiled band on
    // either side has to be warned about now, from the pre-conversion
    // `bands`, or it is silently lost. Pairs where BOTH bands are already
    // archives keep their real filenames in `tiled` unchanged, so
    // `merge_bands`'s own check (below, via its `pub` entry point for
    // library callers too) catches those — warning about them here as well
    // would repeat the same finding twice.
    let kinds: Vec<BandSource> = bands
        .iter()
        .map(|b| classify_band_input(&b.input))
        .collect();
    let source_involved: Vec<(String, String)> = implicit_overlap_pairs(bands)
        .into_iter()
        .filter(|&(i, j)| kinds[i] == BandSource::Source || kinds[j] == BandSource::Source)
        .map(|(i, j)| (bands[i].layer.clone(), bands[j].layer.clone()))
        .collect();
    warn_pairs(&source_involved);

    // Keeps every intermediate alive for the merge and unlinks them on drop —
    // including the early-return paths below.
    let mut scratch: Vec<NamedTempFile> = Vec::new();
    let mut tiled: Vec<Band> = Vec::with_capacity(bands.len());

    for (band, kind) in bands.iter().zip(&kinds) {
        if *kind == BandSource::Archive {
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
            // The band's own range, not a caller-wide value: a `min_zoom`
            // set on `opts.export` would apply to every band alike, and be
            // rejected by any band whose range starts finer (#380).
            min_zoom: Some(band.min_zoom),
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
/// A tile only one band wrote is copied across still compressed. Only a tile
/// that several bands (in different layers) wrote at a shared zoom is
/// decompressed, so its layer messages can be concatenated, and recompressed;
/// no MVT is ever decoded. That is also why the merged metadata carries no
/// `tilestats` — reconstructing feature counts and attribute histograms would
/// mean parsing every MVT, which is exactly the cost this merge exists to
/// avoid.
pub fn merge_bands(bands: &[Band], output: &Path) -> Result<PyramidReport, Error> {
    // A library caller can hand over same-layer bands that overlap in zoom
    // but cover disjoint geography, which the per-tile collision check below
    // would never catch.
    validate_bands(bands).map_err(Error::PMTilesWrite)?;
    // The single call site for this warning: a direct library caller of this
    // `pub fn` gets it here. `build_pyramid` calls this too, but has already
    // warned (above, before conversion) about any pair involving a band it
    // is about to tile — those lose their file-stem signal once `input`
    // becomes a temp archive path, so this check would silently miss them.
    // What this check still catches for a `build_pyramid` caller is a pair
    // of pre-tiled archives passed straight through: their `input` is
    // untouched, so the heuristic still applies, and it has not been checked
    // yet at this point.
    warn_implicit_layer_overlaps(bands);

    // StreamingPmtilesWriter, not PmtilesWriter: it spools tile bytes to a temp
    // file instead of holding every tile in RAM, and it deduplicates. The merge
    // is exactly the workload that needs both — an aggregate band is mostly
    // identical tiles, and re-expanding its runs without dedup turned a
    // 300-byte band archive into 164 KiB.
    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)
        .map_err(|e| Error::PMTilesWrite(format!("Failed to create streaming writer: {e}")))?;
    // #380: a band's coarse zooms may hold no tiles (every feature generalized
    // away there), and the writer would otherwise derive the header's min
    // zoom from the coarsest tile it sees while `vector_layers[].minzoom`
    // says what the band declared. Declare the coarsest band; the writer
    // widens over empty zooms and never narrows over real tiles.
    if let Some(min_zoom) = bands.iter().map(|b| b.min_zoom).min() {
        writer.set_declared_min_zoom(min_zoom);
    }
    let mut layers: Vec<LayerMeta> = Vec::new();
    let mut per_band = Vec::new();
    let mut skipped_total = 0usize;
    let mut union: Option<TileBounds> = None;

    if bands_share_zooms(bands) {
        // #385: bands in different layers share zooms, so one tile id can
        // come from several bands. Index every band's tiles first, then emit
        // each id once: a tile only one band wrote is copied still-compressed
        // exactly as the single-pass path below does; a tile several bands
        // wrote is decompressed, its layer messages concatenated in band
        // order (an MVT tile is a protobuf message whose only field is a
        // repeated `layers`, so byte concatenation IS layer concatenation),
        // and recompressed. Every band's archive is held for the duration —
        // the compressed archives, not the tile set.
        let archives: Vec<BandArchive> = bands
            .iter()
            .map(|b| BandArchive::open(&b.input, &b.layer))
            .collect::<Result<_, _>>()?;
        // The `:LAYER` of a pre-tiled band is a label; the layer name inside
        // its tiles is whatever the archive was exported with. validate_bands
        // only sees the labels, so two archives both carrying `fields`,
        // labelled `2024` and `2025`, would pass and the concatenated tile
        // would hold two layers of one name — an MVT §4.1 violation a client
        // resolves by dropping one. An archive whose metadata declares no
        // layers cannot be checked; that case has already been warned about.
        for (band, archive) in bands.iter().zip(&archives) {
            if !archive.layer_ids.is_empty() && archive.layer_ids != [band.layer.as_str()] {
                return Err(Error::PMTilesWrite(format!(
                    "band `{}` ({}) carries layer(s) {:?} but is labelled `{}`; when bands \
                     share zooms the archive's layer must match its label so tiles do not \
                     carry two layers of one name",
                    band.layer,
                    band.input.display(),
                    archive.layer_ids,
                    band.layer
                )));
            }
        }
        type Ref = (usize, std::ops::Range<usize>);
        // One tile id's `(z, x, y)` and every band range that wrote it.
        type Slot = ((u8, u32, u32), Vec<Ref>);
        // Keyed by tile id, not (z, x, y): the writer appends tile data in
        // add order and stamps the header `clustered`, which promises that
        // offsets are monotonic in directory (tile id) order. Row-major order
        // is not Hilbert order at any zoom ≥ 1, and go-pmtiles `verify`
        // rejected the result as "out-of-order entry in clustered archive".
        let mut index: BTreeMap<u64, Slot> = BTreeMap::new();
        let mut counts = vec![0usize; bands.len()];
        for (bi, (band, archive)) in bands.iter().zip(&archives).enumerate() {
            if let Some(b) = archive.bounds {
                match union.as_mut() {
                    Some(u) => u.expand(&b),
                    None => union = Some(b),
                }
            }
            let mut skipped = 0usize;
            archive.for_each_tile_range(|id, z, x, y, range| {
                if z < band.min_zoom || z > band.max_zoom {
                    skipped += 1;
                    return Ok(());
                }
                let (_, refs) = index.entry(id).or_insert_with(|| ((z, x, y), Vec::new()));
                if let Some((prev, _)) = refs.iter().find(|(p, _)| bands[*p].layer == band.layer) {
                    return Err(Error::PMTilesWrite(format!(
                        "tile {z}/{x}/{y} claimed twice for layer {:?} (bands {} and {})",
                        band.layer,
                        bands[*prev].input.display(),
                        band.input.display()
                    )));
                }
                refs.push((bi, range));
                counts[bi] += 1;
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
            layers.push(LayerMeta {
                id: band.layer.clone(),
                minzoom: band.min_zoom,
                maxzoom: band.max_zoom,
                fields: archive.fields.clone(),
            });
        }
        let mut combined = 0usize;
        let mut buf = Vec::new();
        for ((z, x, y), refs) in index.values() {
            let (z, x, y) = (*z, *x, *y);
            if let [(bi, range)] = refs.as_slice() {
                let data = archives[*bi].tile(range.clone());
                let hash = TileHasher::hash(data);
                writer
                    .add_tile_precompressed(z, x, y, hash, data, data.len(), 0)
                    .map_err(|e| Error::PMTilesWrite(format!("Failed to add tile: {e}")))?;
                continue;
            }
            buf.clear();
            for (bi, range) in refs {
                let plain = compression::decompress(
                    archives[*bi].tile(range.clone()),
                    archives[*bi].header.tile_compression,
                )
                .map_err(|e| {
                    Error::PMTilesWrite(format!(
                        "tile {z}/{x}/{y} of band {:?} failed to decompress: {e}",
                        bands[*bi].layer
                    ))
                })?;
                buf.extend_from_slice(&plain);
            }
            writer
                .add_tile(z, x, y, &buf)
                .map_err(|e| Error::PMTilesWrite(format!("Failed to add tile: {e}")))?;
            combined += 1;
        }
        log::info!(
            "[pyramid] {} tile id(s) shared by several layers were merged; {} tile(s) copied as-is",
            combined,
            index.len() - combined
        );
        for (band, n) in bands.iter().zip(counts) {
            per_band.push((band.layer.clone(), band.min_zoom, band.max_zoom, n));
        }
        return finish_merge(
            writer,
            union,
            layers,
            per_band,
            skipped_total,
            index.len(),
            output,
        );
    }

    // Guards against a later band overwriting an earlier one's tile. Disjoint
    // zoom ranges make that impossible; this asserts it.
    let mut seen: BTreeMap<(u8, u32, u32), &str> = BTreeMap::new();

    // #404: coarsest band first, whatever order the caller listed them in.
    // The writer appends tile data in add order and stamps the header
    // `clustered`, which promises offsets monotonic in tile id — and tile ids
    // ascend with zoom. `--band 6-13:fine --band 0-5:coarse` therefore wrote
    // z6-13's bytes before z0-5's and go-pmtiles `verify` rejected the result
    // as "out-of-order entry in clustered archive". The ranges are disjoint
    // here (the shared-zoom path returned above), so sorting by `min_zoom`
    // puts every band's tiles in ascending tile-id order. `validate_bands`
    // sorts a copy of the same slice for its overlap check.
    //
    // This reordering is also what "first" means for [`union_fields`]'s
    // keep-the-first-type-on-conflict rule on *this* path: `layers` below is
    // pushed in this coarsest-first order, not listing order, so on a field
    // type conflict the coarsest band's type wins — unlike the shared-zoom
    // path above, where `layers` is pushed in the caller's own argument
    // order (that path never reorders `bands`).
    let mut ordered: Vec<&Band> = bands.iter().collect();
    ordered.sort_by_key(|b| (b.min_zoom, b.max_zoom));

    for band in ordered {
        let archive = BandArchive::open(&band.input, &band.layer)?;
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

    let total_tiles = seen.len();
    finish_merge(
        writer,
        union,
        layers,
        per_band,
        skipped_total,
        total_tiles,
        output,
    )
}

/// Fold one band's `fields` object into the layer's accumulated one (#372).
///
/// A layer's bands are meant to be the same layer at different zooms, so
/// `vector_layers[].fields` — what a client introspects to discover a layer's
/// attributes — has to be their union. Keeping the first non-empty object
/// instead dropped every other band's attributes, and #389 made even
/// identical schemas diverge legitimately (`coalesced_count` is withheld from
/// a band whose counter never went above 1).
///
/// A field two bands type differently is a genuine schema disagreement: keep
/// the first and say so, rather than resolve it silently. "First" means
/// whichever band this function was called for *first* — i.e. whichever
/// band's `fields` was folded into `into` earliest by the `for l in layers`
/// loop in [`finish_merge`] — and that loop order is **not** the same on the
/// two merge paths: on the shared-zoom path (bands share a zoom, #385) it is
/// argument order, the order `bands` was given to [`merge_bands`]; on the
/// disjoint path it is coarsest-band-first, the `min_zoom`-sorted order
/// `merge_bands` builds for tile-id ordering (#404), regardless of how the
/// caller listed the bands. Both are "first bands processed", not "first
/// bands as typed" — the disjoint path's sort runs before this ever sees a
/// field.
fn union_fields(into: &mut Value, from: Value, layer: &str) {
    let Value::Object(from) = from else { return };
    if !into.is_object() {
        *into = json!({});
    }
    let Some(target) = into.as_object_mut() else {
        return;
    };
    for (name, ty) in from {
        match target.get(&name) {
            None => {
                target.insert(name, ty);
            }
            // `{first}`/`{ty}` would print through `Value`'s JSON `Display`,
            // wrapping a string type like "Number" in literal double quotes
            // ("Number" not Number). `field_type_str` unwraps a JSON string
            // to its bare text and only falls back to the JSON form for a
            // field type that is not (or no longer) a plain string.
            Some(first) if *first != ty => log::warn!(
                "layer {layer:?}: bands disagree on the type of field {name:?} \
                 ({} and {}); keeping {}",
                field_type_str(first),
                field_type_str(&ty),
                field_type_str(first),
            ),
            Some(_) => {}
        }
    }
}

/// A `vector_layers[].fields` value's bare text, without the JSON quoting
/// `Value`'s `Display` would add around a string type like `"Number"`.
fn field_type_str(v: &Value) -> String {
    v.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| v.to_string())
}

/// The tail shared by both merge paths: bounds, `vector_layers`, finalize.
fn finish_merge(
    mut writer: StreamingPmtilesWriter,
    union: Option<TileBounds>,
    layers: Vec<LayerMeta>,
    per_band: Vec<(String, u8, u8, usize)>,
    skipped_total: usize,
    total_tiles: usize,
    output: &Path,
) -> Result<PyramidReport, Error> {
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
                union_fields(&mut m.fields, l.fields, &l.id);
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
        total_tiles,
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

    /// #385: two GeoParquet bands that name different layers may cover the
    /// same zooms — tippecanoe's `-L`. Each is tiled on its own ladder and
    /// the archive carries both layers at the shared zooms.
    #[test]
    fn build_pyramid_layers_two_sources_over_the_same_zooms() {
        use prost::Message;

        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.parquet");
        let b = dir.path().join("b.parquet");
        write_cell_source(&a, 40, 0.6);
        write_cell_source(&b, 40, 0.6);

        let out = dir.path().join("layers.pmtiles");
        let bands = vec![
            Band {
                input: a,
                layer: "2024".to_string(),
                min_zoom: 0,
                max_zoom: 3,
            },
            Band {
                input: b,
                layer: "2025".to_string(),
                min_zoom: 0,
                max_zoom: 3,
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
        assert_eq!(archive_zooms(&out), vec![0, 1, 2, 3]);

        // Identical sources ⇒ identical tile sets ⇒ every tile carries both
        // layers, in band order. Reading the merged output back like this
        // has no single band label to match, so "" and the fallback-to-
        // first-entry path are expected here.
        let archive = BandArchive::open(&out, "").unwrap();
        let mut tiles = 0;
        archive
            .for_each_tile(|_, _, _, data| {
                let plain = compression::decompress(data, Compression::Gzip).unwrap();
                let names: Vec<String> = crate::vector_tile::Tile::decode(plain.as_slice())
                    .unwrap()
                    .layers
                    .into_iter()
                    .map(|l| l.name)
                    .collect();
                assert_eq!(names, vec!["2024", "2025"]);
                tiles += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(tiles, report.total_tiles);

        let meta: Value = serde_json::from_str(&read_metadata(&out)).unwrap();
        let ids: Vec<&str> = meta["vector_layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["2024", "2025"]);
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
                    layer: "a".to_string(),
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

    /// #385: bands may share zooms when they name different layers — that is
    /// tippecanoe's `-L`, two layers in one tile. Same-layer overlap is still
    /// the silent-overwrite it always was.
    #[test]
    fn overlapping_bands_with_distinct_layers_are_accepted() {
        let bands = vec![
            Band::parse("0-13:a.parquet:2024").unwrap(),
            Band::parse("0-13:b.parquet:2025").unwrap(),
        ];
        validate_bands(&bands).unwrap();

        let bands = vec![
            Band::parse("0-13:a.parquet:2024").unwrap(),
            Band::parse("0-13:b.parquet:2025").unwrap(),
            Band::parse("10-13:c.parquet:2024").unwrap(),
        ];
        let err = validate_bands(&bands).unwrap_err();
        assert!(
            err.contains("overlap at zoom 10") && err.contains("\"2024\""),
            "{err}"
        );
    }

    /// A minimal MVT tile with one empty layer named `name` (version 2).
    fn layer_tile(name: &str) -> Vec<u8> {
        let mut layer = vec![0x0A, name.len() as u8];
        layer.extend_from_slice(name.as_bytes());
        layer.extend_from_slice(&[0x78, 0x02]); // version = 2
        let mut tile = vec![0x1A, layer.len() as u8];
        tile.extend_from_slice(&layer);
        tile
    }

    fn write_band_with_payload(path: &Path, layer: &str, tiles: &[(u8, u32, u32)], payload: &[u8]) {
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name(layer);
        w.set_bounds(&TileBounds::new(-1.0, -1.0, 1.0, 1.0));
        w.set_fields(HashMap::from([(
            format!("{layer}_f"),
            "Number".to_string(),
        )]));
        for (z, x, y) in tiles {
            w.add_tile(*z, *x, *y, payload).unwrap();
        }
        w.finalize(path).unwrap();
    }

    /// Decode a merged tile's layer names in order.
    fn merged_layer_names(archive: &Path, z: u8, x: u32, y: u32) -> Vec<String> {
        use prost::Message;
        let a = BandArchive::open(archive, "").unwrap();
        let mut found = None;
        a.for_each_tile(|tz, tx, ty, data| {
            if (tz, tx, ty) == (z, x, y) {
                found = Some(compression::decompress(data, Compression::Gzip).unwrap());
            }
            Ok(())
        })
        .unwrap();
        let plain = found.unwrap_or_else(|| panic!("tile {z}/{x}/{y} missing"));
        crate::vector_tile::Tile::decode(plain.as_slice())
            .unwrap()
            .layers
            .into_iter()
            .map(|l| l.name)
            .collect()
    }

    /// #380 on the pyramid path: a band declared `0-3` whose coarse zooms
    /// generalized to nothing holds tiles only at z3. The merged header must
    /// still declare z0, as `vector_layers[].minzoom` already does — on both
    /// the single-pass and the shared-zoom path.
    #[test]
    fn merge_declares_the_bands_min_zoom_even_when_coarse_zooms_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        write_band_with_payload(&a, "2024", &[(3, 1, 1)], &layer_tile("2024"));
        write_band_with_payload(&b, "2025", &[(3, 2, 2)], &layer_tile("2025"));

        // Single band, single-pass path.
        let out = dir.path().join("one.pmtiles");
        merge_bands(
            &[Band::parse(&format!("0-3:{}:2024", a.display())).unwrap()],
            &out,
        )
        .unwrap();
        let h = Header::from_bytes(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!((h.min_zoom, h.max_zoom), (0, 3), "single-pass header");

        // Two layers over shared zooms, two-phase path; the coarser band wins.
        let out = dir.path().join("two.pmtiles");
        merge_bands(
            &[
                Band::parse(&format!("2-3:{}:2024", a.display())).unwrap(),
                Band::parse(&format!("1-3:{}:2025", b.display())).unwrap(),
            ],
            &out,
        )
        .unwrap();
        let h = Header::from_bytes(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!((h.min_zoom, h.max_zoom), (1, 3), "two-phase header");
    }

    /// #385: at a zoom two bands share, a tile both wrote carries both layers
    /// (the MVT layer messages concatenated, in band order); a tile only one
    /// band wrote passes through untouched; `vector_layers` lists both.
    #[test]
    fn merge_concatenates_layers_of_bands_sharing_a_zoom() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        write_band_with_payload(&a, "2024", &[(3, 1, 1), (3, 2, 2)], &layer_tile("2024"));
        write_band_with_payload(&b, "2025", &[(3, 1, 1), (3, 3, 3)], &layer_tile("2025"));

        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[
                Band::parse(&format!("0-3:{}:2024", a.display())).unwrap(),
                Band::parse(&format!("0-3:{}:2025", b.display())).unwrap(),
            ],
            &out,
        )
        .unwrap();
        assert_eq!(report.total_tiles, 3, "three distinct tile ids");

        assert_eq!(merged_layer_names(&out, 3, 1, 1), vec!["2024", "2025"]);
        assert_eq!(merged_layer_names(&out, 3, 2, 2), vec!["2024"]);
        assert_eq!(merged_layer_names(&out, 3, 3, 3), vec!["2025"]);

        let meta: Value = serde_json::from_str(&read_metadata(&out)).unwrap();
        let ids: Vec<&str> = meta["vector_layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["2024", "2025"]);
        assert_eq!(
            meta["vector_layers"][1]["fields"]["2025_f"],
            json!("Number")
        );
    }

    /// The two-phase merge used to walk its index in `(z, x, y)` order and the
    /// writer appends tile data in add order, so the directory — sorted by
    /// tile id, and stamped `clustered` — pointed at non-monotonic offsets.
    /// Row-major order is not Hilbert order at any zoom ≥ 1; go-pmtiles
    /// `verify` reported "out-of-order entry in clustered archive". This is
    /// its check: walking the directory, each entry's offset is either one
    /// already seen (a dedup back-reference) or the current running end.
    #[test]
    fn merge_writes_shared_zoom_tiles_in_tile_id_order() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        // Every tile distinct, so nothing dedups and every offset is fresh.
        let z1: Vec<(u8, u32, u32)> = vec![(1, 0, 0), (1, 0, 1), (1, 1, 0), (1, 1, 1)];
        for (path, label) in [(&a, "a"), (&b, "b")] {
            let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
            w.set_layer_name(label);
            w.set_bounds(&TileBounds::new(-1.0, -1.0, 1.0, 1.0));
            for &(z, x, y) in &z1 {
                w.add_tile(z, x, y, &layer_tile(&format!("{label}-{x}-{y}")))
                    .unwrap();
            }
            w.finalize(path).unwrap();
        }

        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[
                Band::parse(&format!("1-1:{}:a", a.display())).unwrap(),
                Band::parse(&format!("1-1:{}:b", b.display())).unwrap(),
            ],
            &out,
        )
        .unwrap();
        assert_eq!(report.total_tiles, 4);

        let bytes = std::fs::read(&out).unwrap();
        let h = Header::from_bytes(&bytes).unwrap();
        assert!(h.clustered, "the writer stamps every archive clustered");
        let root = compression::decompress(
            &bytes[h.root_dir_offset as usize..(h.root_dir_offset + h.root_dir_length) as usize],
            h.internal_compression,
        )
        .unwrap();
        let entries: Vec<(u64, u64, u32)> = decode_directory(&root)
            .unwrap()
            .iter()
            .filter(|e| e.run_length > 0)
            .map(|e| (e.tile_id, e.offset, e.length))
            .collect();
        assert_eq!(entries.len(), 4, "{entries:?}");
        let mut seen = std::collections::HashSet::new();
        let mut end = 0u64;
        for &(id, offset, length) in &entries {
            if seen.contains(&offset) {
                continue;
            }
            assert_eq!(
                offset, end,
                "out-of-order entry in clustered archive at tile id {id}: {entries:?}"
            );
            seen.insert(offset);
            end = offset + u64::from(length);
        }
    }

    /// For a pre-tiled band the `:LAYER` in `--band` is a label; the MVT layer
    /// name inside each tile is whatever the archive was exported with. Two
    /// archives both carrying layer `fields`, labelled `2024` and `2025`,
    /// passed validation and produced tiles with two layers named `fields` —
    /// an MVT §4.1 violation MapLibre resolves by keeping one of them. The
    /// archive's own layer id must match its label when bands share zooms.
    #[test]
    fn merge_rejects_shared_zoom_band_whose_archive_layer_differs_from_its_label() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        write_band_with_payload(&a, "fields", &[(3, 1, 1)], &layer_tile("fields"));
        write_band_with_payload(&b, "fields", &[(3, 1, 1)], &layer_tile("fields"));

        let out = dir.path().join("merged.pmtiles");
        let err = merge_bands(
            &[
                Band::parse(&format!("0-3:{}:2024", a.display())).unwrap(),
                Band::parse(&format!("0-3:{}:2025", b.display())).unwrap(),
            ],
            &out,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("`2024`") && msg.contains("fields"),
            "must name both the label and the archive's layer: {msg}"
        );
        assert!(!out.exists(), "nothing should be written on a bad plan");
    }

    /// Only MVT tiles can be concatenated: two PNGs glued together are not a
    /// PNG. The tile type is in the header, so it is checked on open.
    #[test]
    fn non_mvt_band_archive_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("raster.pmtiles");
        write_band(
            &src,
            "agg",
            &[(3, 1, 1)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );
        let mut bytes = std::fs::read(&src).unwrap();
        // Byte 99 of the header is the tile type; 2 is PNG.
        bytes[99] = 2;
        std::fs::write(&src, &bytes).unwrap();

        let out = dir.path().join("merged.pmtiles");
        let err = merge_bands(
            &[Band::parse(&format!("3-3:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap_err();
        assert!(err.to_string().contains("Png"), "{err}");
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

    /// #404: the single-pass path copied each band's tiles in *argument*
    /// order, so `--band 2-2:fine --band 0-1:coarse` wrote z2's tile data
    /// before z0's while the writer stamps the header `clustered` — which
    /// promises offsets monotonic in tile-id order. go-pmtiles `verify`
    /// reported "out-of-order entry in clustered archive". Same clustered walk
    /// as `merge_writes_shared_zoom_tiles_in_tile_id_order` (#392), on the
    /// disjoint-band path.
    #[test]
    fn merge_writes_disjoint_bands_in_tile_id_order_given_fine_first() {
        let dir = tempfile::tempdir().unwrap();
        let coarse = dir.path().join("coarse.pmtiles");
        let fine = dir.path().join("fine.pmtiles");
        // Every tile distinct, so nothing dedups and every offset is fresh.
        for (path, label, tiles) in [
            (
                &coarse,
                "coarse",
                vec![(0u8, 0u32, 0u32), (1, 0, 0), (1, 1, 1)],
            ),
            (&fine, "fine", vec![(2, 0, 0), (2, 3, 3), (2, 1, 2)]),
        ] {
            let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
            w.set_layer_name(label);
            w.set_bounds(&TileBounds::new(-1.0, -1.0, 1.0, 1.0));
            for &(z, x, y) in &tiles {
                w.add_tile(z, x, y, &layer_tile(&format!("{label}-{z}-{x}-{y}")))
                    .unwrap();
            }
            w.finalize(path).unwrap();
        }

        let out = dir.path().join("merged.pmtiles");
        // Fine band first: legal, and the zoom ranges are disjoint.
        let report = merge_bands(
            &[
                Band::parse(&format!("2-2:{}:fine", fine.display())).unwrap(),
                Band::parse(&format!("0-1:{}:coarse", coarse.display())).unwrap(),
            ],
            &out,
        )
        .unwrap();
        assert_eq!(report.total_tiles, 6);

        let bytes = std::fs::read(&out).unwrap();
        let h = Header::from_bytes(&bytes).unwrap();
        assert!(h.clustered, "the writer stamps every archive clustered");
        let root = compression::decompress(
            &bytes[h.root_dir_offset as usize..(h.root_dir_offset + h.root_dir_length) as usize],
            h.internal_compression,
        )
        .unwrap();
        let entries: Vec<(u64, u64, u32)> = decode_directory(&root)
            .unwrap()
            .iter()
            .filter(|e| e.run_length > 0)
            .map(|e| (e.tile_id, e.offset, e.length))
            .collect();
        assert_eq!(entries.len(), 6, "{entries:?}");
        let mut seen = std::collections::HashSet::new();
        let mut end = 0u64;
        for &(id, offset, length) in &entries {
            if seen.contains(&offset) {
                continue;
            }
            assert_eq!(
                offset, end,
                "out-of-order entry in clustered archive at tile id {id}: {entries:?}"
            );
            seen.insert(offset);
            end = offset + u64::from(length);
        }
    }

    /// #372: two bands of one layer each declare their own attributes.
    /// `vector_layers[].fields` is what a client introspects to discover a
    /// layer's attributes, so it must be the union across the layer's bands —
    /// taking the first non-empty one dropped every other band's fields.
    #[test]
    fn merged_layer_fields_are_the_union_across_bands() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        let bounds = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
        write_band(&a, "cells", &[(0, 0, 0)], bounds, &[("a", "Number")]);
        write_band(&b, "cells", &[(1, 0, 0)], bounds, &[("b", "String")]);

        let out = dir.path().join("merged.pmtiles");
        merge_bands(
            &[
                Band::parse(&format!("0-0:{}:cells", a.display())).unwrap(),
                Band::parse(&format!("1-1:{}:cells", b.display())).unwrap(),
            ],
            &out,
        )
        .unwrap();

        let meta = read_metadata(&out);
        let v: Value = serde_json::from_str(&meta).unwrap();
        let layers = v["vector_layers"].as_array().unwrap();
        assert_eq!(layers.len(), 1, "{meta}");
        assert_eq!(layers[0]["fields"]["a"], json!("Number"), "{meta}");
        assert_eq!(
            layers[0]["fields"]["b"],
            json!("String"),
            "the second band's fields must survive: {meta}"
        );
    }

    /// A genuine schema disagreement between two bands of one layer keeps the
    /// first band's type (and warns); it must not drop the field or invent a
    /// union type.
    #[test]
    fn merged_layer_fields_keep_the_first_type_on_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        let bounds = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
        write_band(&a, "cells", &[(0, 0, 0)], bounds, &[("n", "Number")]);
        write_band(&b, "cells", &[(1, 0, 0)], bounds, &[("n", "String")]);

        let out = dir.path().join("merged.pmtiles");
        merge_bands(
            &[
                Band::parse(&format!("0-0:{}:cells", a.display())).unwrap(),
                Band::parse(&format!("1-1:{}:cells", b.display())).unwrap(),
            ],
            &out,
        )
        .unwrap();

        let meta = read_metadata(&out);
        let v: Value = serde_json::from_str(&meta).unwrap();
        assert_eq!(
            v["vector_layers"][0]["fields"]["n"],
            json!("Number"),
            "{meta}"
        );
    }

    /// S2-4: an archive re-read on the disjoint merge path can carry several
    /// `vector_layers` entries (e.g. it is itself a previous pyramid's
    /// output). `parse_layers` must pick the entry whose `id` matches the
    /// band's own label, not blindly lift `vector_layers[0]` — that silently
    /// attached the wrong layer's fields whenever the matching entry was not
    /// first.
    #[test]
    fn parse_layers_selects_the_entry_matching_the_band_label() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("multi.pmtiles");
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_bounds(&TileBounds::new(-1.0, -1.0, 1.0, 1.0));
        w.set_vector_layers_json(
            json!([
                {"id": "alpha", "minzoom": 0, "maxzoom": 5, "fields": {"a": "Number"}},
                {"id": "beta", "minzoom": 0, "maxzoom": 5, "fields": {"b": "String"}},
            ])
            .to_string(),
        );
        w.add_tile(0, 0, 0, &[0x1a, 0x02, 0x08, 0x01]).unwrap();
        w.finalize(&archive).unwrap();

        // "beta" is not vector_layers[0]; naive [0]-lifting would have
        // attached "alpha"'s fields to a band labelled "beta".
        let opened = BandArchive::open(&archive, "beta").unwrap();
        assert_eq!(
            opened.fields,
            json!({"b": "String"}),
            "must select the entry whose id matches the band's label, not [0]"
        );

        let opened = BandArchive::open(&archive, "alpha").unwrap();
        assert_eq!(opened.fields, json!({"a": "Number"}));

        // No entry named "gamma": falls back to [0] rather than erroring —
        // the band's tiles are still usable even if its fields cannot be
        // identified for certain — and logs the mismatch (not asserted
        // here; log content isn't test-observable without a capturing
        // logger).
        let opened = BandArchive::open(&archive, "gamma").unwrap();
        assert_eq!(opened.fields, json!({"a": "Number"}), "falls back to [0]");
    }

    /// #405: since bands in different layers may share zooms (#392), an
    /// off-by-one range between two *unlabelled* bands silently turns one
    /// intended layer into two named after the files. Nothing errors — the
    /// split is legal, and stem-named layers are a real workflow — but the
    /// condition is worth naming out loud.
    #[test]
    fn implicit_layer_overlap_is_reported() {
        let parse = |specs: &[&str]| -> Vec<Band> {
            specs.iter().map(|s| Band::parse(s).unwrap()).collect()
        };

        // The typo: 0-5 and 5-13 share z5, and neither band was labelled.
        let bands = parse(&["0-5:coarse.parquet", "5-13:fine.parquet"]);
        assert_eq!(
            implicit_layer_overlaps(&bands),
            vec![("coarse".to_string(), "fine".to_string())],
            "an unlabelled overlap must be reported"
        );

        // Explicit labels: two layers over one range is the whole point of
        // tippecanoe's -L, so say nothing.
        let bands = parse(&["0-13:a.parquet:2024", "0-13:b.parquet:2025"]);
        assert!(
            implicit_layer_overlaps(&bands).is_empty(),
            "explicitly labelled bands are intentional"
        );

        // One band labelled, one not: the unlabelled one is still the one
        // that may be a typo, so the pair is still reported.
        let bands = parse(&["0-5:coarse.parquet", "5-13:fine.parquet:features"]);
        assert_eq!(
            implicit_layer_overlaps(&bands),
            vec![("coarse".to_string(), "features".to_string())],
            "one implicit label in the pair is enough to report"
        );

        // Adjacent, not overlapping: the ordinary pyramid.
        let bands = parse(&["0-4:coarse.parquet", "5-13:fine.parquet"]);
        assert!(implicit_layer_overlaps(&bands).is_empty(), "no shared zoom");
    }

    /// S2-1: the fully-legitimate documented multi-layer workflow (the `-L`
    /// shape from #385) is two bands over the *same* zoom range, or one
    /// nested inside the other, each named after its own file — deliberate
    /// stacking, not a typo. Only a *partial* overlap (neither range
    /// contains the other) is the shape an off-by-one range produces, so
    /// only that is reported. Both cases here are implicit on both sides
    /// (unlike the explicit-label case in `implicit_layer_overlap_is_reported`),
    /// so the suppression is entirely due to the zoom-range shape, not the
    /// labels.
    #[test]
    fn identical_and_nested_ranges_are_not_reported() {
        let parse = |specs: &[&str]| -> Vec<Band> {
            specs.iter().map(|s| Band::parse(s).unwrap()).collect()
        };

        // Identical ranges: the same zooms, two stem-derived layers.
        let bands = parse(&["0-13:coarse.parquet", "0-13:fine.parquet"]);
        assert!(
            implicit_layer_overlaps(&bands).is_empty(),
            "identical zoom ranges are deliberate stacking, not a typo"
        );

        // Strict containment: one band's range nested inside the other's.
        let bands = parse(&["0-13:coarse.parquet", "5-8:fine.parquet"]);
        assert!(
            implicit_layer_overlaps(&bands).is_empty(),
            "one range containing the other is deliberate stacking, not a typo"
        );

        // The control case: a genuine partial overlap (this is the same
        // 0-5/5-13 pair as above) still reports.
        let bands = parse(&["0-5:coarse.parquet", "5-13:fine.parquet"]);
        assert_eq!(
            implicit_layer_overlaps(&bands),
            vec![("coarse".to_string(), "fine".to_string())],
            "a partial, non-containment overlap is still reported"
        );
    }

    /// What the check reads instead of a flag on `Band`: whether the layer
    /// name is exactly what the input would have derived.
    #[test]
    fn a_stem_named_layer_reads_as_implicit() {
        let band = |spec: &str| Band::parse(spec).unwrap();
        assert!(layer_is_input_derived(&band("0-5:x.parquet")), "stem");
        assert!(
            !layer_is_input_derived(&band("0-5:x.parquet:agg")),
            "explicit label"
        );
        assert!(
            layer_is_input_derived(&band("0-5:/data/cells/*.parquet")),
            "a glob derives the directory name"
        );
    }

    /// #482: a band must be able to name a remote input. The `:` form is
    /// scheme-aware — after `LO-HI:`, a `scheme://` remainder splits a layer
    /// off its LAST colon only when what follows carries no `/`.
    #[test]
    fn band_spec_names_remote_inputs() {
        let ok = |spec: &str| Band::parse(spec).unwrap_or_else(|e| panic!("{spec:?}: {e}"));

        let b = ok("0-13:https://data.source.coop/a/b.parquet");
        assert_eq!(
            b.input,
            PathBuf::from("https://data.source.coop/a/b.parquet"),
            "no layer segment: the whole remainder is the input"
        );
        assert_eq!(b.layer, "b");

        let b = ok("0-13:https://x/y.parquet:fields");
        assert_eq!(b.input, PathBuf::from("https://x/y.parquet"));
        assert_eq!(b.layer, "fields");

        let b = ok("0-13:s3://bucket/key.parquet:2024");
        assert_eq!(b.input, PathBuf::from("s3://bucket/key.parquet"));
        assert_eq!(b.layer, "2024");

        let b = ok("0-13:gs://bucket/key.parquet");
        assert_eq!(b.input, PathBuf::from("gs://bucket/key.parquet"));
        assert_eq!(b.layer, "key");

        // The last colon is inside the URL path, and the segment after it has
        // a `/`, so it is not a layer.
        let b = ok("0-13:https://host/2024:06/cells.parquet");
        assert_eq!(
            b.input,
            PathBuf::from("https://host/2024:06/cells.parquet"),
            "a colon inside the URL path is not a layer separator"
        );
        assert_eq!(b.layer, "cells");

        // A port is part of the authority, not a layer.
        let b = ok("0-13:http://localhost:8080/cells.parquet");
        assert_eq!(
            b.input,
            PathBuf::from("http://localhost:8080/cells.parquet")
        );
        assert_eq!(b.layer, "cells");
    }

    /// #482: the `=` escape form, for inputs the `:` form cannot express.
    ///
    /// `LO-HI=INPUT[=LAYER]`: the range is split at the FIRST `=`, and a LAYER
    /// at the LAST `=` only when the segment after it is a bare layer token
    /// (no `/`, `\` or `:`). That keeps a Hive directory such as
    /// `admin:country_code=BR/part.parquet` whole.
    #[test]
    fn band_spec_equals_form_escapes_colons() {
        let ok = |spec: &str| Band::parse(spec).unwrap_or_else(|e| panic!("{spec:?}: {e}"));

        let b = ok("0-13=local:odd:path.parquet=layer");
        assert_eq!(b.input, PathBuf::from("local:odd:path.parquet"));
        assert_eq!(b.layer, "layer");

        let b = ok("0-13=admin:country_code=BR/part.parquet");
        assert_eq!(
            b.input,
            PathBuf::from("admin:country_code=BR/part.parquet"),
            "a Hive `=` inside the input keeps the input whole"
        );
        assert_eq!(b.layer, "part");

        // No `=` beyond the separator: the whole remainder is the input.
        let b = ok("0-9=/data/odd:dir/cells.parquet");
        assert_eq!(b.input, PathBuf::from("/data/odd:dir/cells.parquet"));
        assert_eq!(b.layer, "cells");

        // Remote inputs work in this form too.
        let b = ok("0-9=s3://bucket/a:b.parquet=agg");
        assert_eq!(b.input, PathBuf::from("s3://bucket/a:b.parquet"));
        assert_eq!(b.layer, "agg");

        // The form is chosen by whichever separator ends the zoom range, so a
        // `=` later in a `:` spec is just part of the input.
        let b = ok("0-9:/data/country_code=BR/part.parquet");
        assert_eq!(b.input, PathBuf::from("/data/country_code=BR/part.parquet"));

        // Whitespace is trimmed on both sides of the split, as in the `:` form.
        let b = ok("0-5= x.parquet = agg ");
        assert_eq!(b.input, PathBuf::from("x.parquet"));
        assert_eq!(b.layer, "agg");

        for bad in [
            "0-5=",           // empty input
            "0-5=x.parquet=", // empty layer
            "5-0=x.parquet",  // reversed range
            "0-31=x.parquet", // above the zoom ceiling
            "x=y.parquet",    // no range
        ] {
            assert!(Band::parse(bad).is_err(), "{bad:?} must be rejected");
        }
    }

    /// An ill-formed spec names both accepted forms, so the reader learns the
    /// escape hatch from the error rather than the manual.
    #[test]
    fn band_spec_error_shows_both_forms() {
        let err = Band::parse("0-5").unwrap_err();
        assert!(err.contains("LO-HI:INPUT[:LAYER]"), "{err}");
        assert!(err.contains("LO-HI=INPUT[=LAYER]"), "{err}");
    }

    /// #482: a remote input is a GeoParquet source, decided from the URL
    /// shape alone — no local read, which for a URL could only ever fail.
    #[test]
    fn remote_band_input_is_a_source_without_a_local_read() {
        for url in [
            "https://data.source.coop/a/b.parquet",
            "s3://bucket/key.parquet",
            "gs://bucket/key.parquet",
            "http://localhost:8080/cells.parquet",
        ] {
            assert_eq!(
                classify_band_input(Path::new(url)),
                BandSource::Source,
                "{url}"
            );
        }
    }

    /// A remote PMTiles archive cannot be a band: the merge reads directories
    /// and tile bytes out of a local file. Say so, instead of handing the URL
    /// to the parquet reader.
    #[test]
    fn remote_pmtiles_band_is_rejected_with_a_clear_message() {
        let msg = remote_archive_rejection(Path::new("https://host/tiles.pmtiles"))
            .expect("a remote .pmtiles band must be rejected");
        assert!(msg.contains("remote band must be GeoParquet"), "{msg}");
        assert!(msg.contains("stage"), "{msg}");

        assert!(
            remote_archive_rejection(Path::new("https://host/cells.parquet")).is_none(),
            "a remote GeoParquet band is fine"
        );
        assert!(
            remote_archive_rejection(Path::new("/local/tiles.pmtiles")).is_none(),
            "a local archive is the ordinary two-step form"
        );
    }
}
