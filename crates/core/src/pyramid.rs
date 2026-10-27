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

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use tempfile::{NamedTempFile, TempPath};

use crate::archive_index::{ArchiveIndex, MergedTiles};
use crate::compression::{self, Compression, MAX_INTERNAL_BYTES, MAX_TILE_BYTES};
use crate::dedup::TileHasher;
use crate::input::url_scheme;
use crate::overview::config::ConfigError;
use crate::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use crate::overview::export::{export_pmtiles, ExportOptions};
use crate::pmtiles_metadata::{MetadataCollector, Resolved};
use crate::pmtiles_writer::{Header, StreamingPmtilesWriter, TileType, MAX_TILE_ID_ZOOM};
use crate::tile::TileBounds;
use crate::{Error, TilesetMetadata};

/// One band: an input tiled verbatim across `min_zoom..=max_zoom` into `layer`.
#[derive(Debug, Clone)]
pub struct Band {
    pub input: PathBuf,
    pub layer: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
}

impl Band {
    /// Web Mercator zoom ceiling: the shared [`crate::tile::MAX_ZOOM`] (#371),
    /// which every write path now enforces at options validation.
    const MAX_ZOOM: u8 = crate::tile::MAX_ZOOM;

    /// Parse a band spec. The layer is optional and defaults to the input's
    /// file stem. Two spellings are accepted (#482):
    ///
    /// * `LO-HI:INPUT[:LAYER]` — the usual one.
    /// * `LO-HI=INPUT[=LAYER]` — an escape form for an INPUT the colon form
    ///   cannot express, `=` being invalid in a zoom range.
    ///
    /// Which one a spec is in is decided by whichever of `:` and `=` closes
    /// the zoom range, i.e. whichever comes first (`band_separator`).
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
    ///
    /// # Errors
    ///
    /// Returns a description of the problem if the spec has no zoom range, a
    /// zoom that does not parse, a minimum above the maximum, a maximum above
    /// [`crate::tile::MAX_ZOOM`], an empty input or layer, or no layer when
    /// none can be derived from the input.
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
/// applies `remote_archive_rejection` up front to every band before any
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

/// Reject two bands claiming one zoom **for the same layer**.
///
/// Each would write the same tile ids and the merge would silently keep
/// whichever came last. Also rejects any band naming a remote `.pmtiles` archive (#482) — checked
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
///
/// # Errors
///
/// Returns a description of the problem if `bands` is empty, if two bands of
/// one layer share a zoom, or if a band names a remote `.pmtiles` archive.
pub fn validate_bands(bands: &[Band]) -> Result<(), String> {
    if bands.is_empty() {
        return Err("a pyramid needs at least one band".to_string());
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

/// Parse and check a list of `LO-HI:INPUT[:LAYER]` band specs.
///
/// Each spec goes through [`Band::parse`], the set through
/// [`validate_bands`], then each local input is checked for existence
/// (#445: shared by the CLI's `--band` and the Python `pyramid(bands=...)`).
///
/// The existence check names the missing input up front rather than letting
/// the reader fail later with a bare "No such file or directory" and no
/// path. It is a plain existence test, skipped where `exists()` has no
/// useful answer: a remote URL and a glob, which the reader resolves itself.
/// (It cannot be a [`classify_band_input`] check: that returns `Archive` only
/// when the file opens, so a missing path always classifies as `Source`.)
///
/// # Errors
///
/// The first problem found, as a message: a malformed spec, an invalid band
/// set, or a local input that does not exist.
pub fn parse_bands<S: AsRef<str>>(specs: &[S]) -> Result<Vec<Band>, String> {
    let bands = specs
        .iter()
        .map(|s| Band::parse(s.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;
    validate_bands(&bands)?;
    for (spec, band) in specs.iter().zip(&bands) {
        check_band_input_exists(spec.as_ref(), band)?;
    }
    Ok(bands)
}

/// [`parse_bands`]' existence check for one band.
fn check_band_input_exists(spec: &str, band: &Band) -> Result<(), String> {
    let spelled = band.input.to_string_lossy();
    let deferred = spelled.contains("://") || spelled.contains(['*', '?', '[']);
    if deferred || band.input.exists() {
        return Ok(());
    }
    let mut msg = format!("band input not found: {}", band.input.display());
    // A bare Hive partition dir spec like `0-13=admin:country_code=BR` (no
    // trailing filename) silently parses as INPUT=admin:country_code,
    // LAYER=BR: the trailing `=BR` looked like an explicit layer, so it was
    // stripped from the path. If that is what happened here, say so — a bare
    // "not found" gives no hint that a layer was ever peeled off.
    if let Some(suffix) = stripped_equals_layer_suffix(spec, &band.layer) {
        let candidate = format!("{}{suffix}", band.input.display());
        if Path::new(&candidate).exists() {
            msg.push_str(&format!(
                "\n  hint: {candidate:?} exists — the trailing {suffix:?} was \
                 parsed as a layer name (LO-HI=INPUT=LAYER); if it is part of \
                 the path, append an explicit =LAYER instead"
            ));
        } else {
            msg.push_str(&format!(
                "\n  hint: the trailing {suffix:?} was parsed as a layer name \
                 (LO-HI=INPUT=LAYER); if it is part of the path, append an \
                 explicit =LAYER instead"
            ));
        }
    }
    Err(msg)
}

/// If `spec` is the `LO-HI=INPUT=LAYER` escape form and `layer` is exactly
/// the segment [`Band::parse`] peeled off its end, the `=LAYER` suffix that
/// was stripped — for the "was this actually part of the path?" hint when
/// the stripped-down input then fails the existence check.
///
/// Mirrors [`Band::parse`]'s separator choice (`=` wins whichever of `:` and
/// `=` appears first): this is a best-effort hint, not a re-parse, so a false
/// negative here just means no hint is offered, not a wrong answer.
fn stripped_equals_layer_suffix(spec: &str, layer: &str) -> Option<String> {
    let colon = spec.find(':');
    let equals = spec.find('=');
    let is_equals_form = match (colon, equals) {
        (Some(c), Some(e)) => e < c,
        (None, Some(_)) => true,
        _ => false,
    };
    if !is_equals_form {
        return None;
    }
    let suffix = format!("={layer}");
    spec.ends_with(&suffix).then_some(suffix)
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

/// How a band's declared `min_zoom..=max_zoom` relates to what its archive
/// actually contains, for a pre-tiled band (#495, reworked for #514).
///
/// An archive's *declared* range can be wider than its actual tiles: #380
/// lets a build declare zooms that generalized to nothing. Between #380/#390
/// and #529/#522 tylertoo widened the header itself; since then its header
/// is always the tiles actually written (`go-pmtiles verify` requires it)
/// and the declaration lives only in `vector_layers[].minzoom`/`maxzoom`,
/// while an externally produced archive may still widen its header. The
/// declared range used here is therefore the header's range widened by the
/// archive's own `vector_layers` ([`declared_zoom_range`]). Keying this
/// contract off a declaration alone produced two opposite bugs for the very
/// same underlying tiles: a widened declaration made an effectively-empty
/// band look like a legitimate subrange (accepted silently, contributing
/// nothing, while the merged metadata still declared it); an honest, narrow
/// one made a band that fully covered the archive's real tiles look like an
/// error. #514 classifies against the archive's *actual* tile range —
/// [`ArchiveIndex::actual_zoom_range`] — instead, and falls back to the
/// declared range only for the one thing actual content cannot decide:
/// whether a band that misses every real tile was at least within what the
/// archive was declared to cover.
///
/// * [`Disjoint`](Self::Disjoint) — the band shares no zoom with what the
///   archive *declares*, let alone what it actually holds. There is no
///   interpretation under which this band could write anything; always an
///   error — including when the archive turns out to hold no tiles at all,
///   which does not make an impossible range possible and so must not
///   divert the band to a flag-downgradable classification.
/// * [`EmptyArchive`](Self::EmptyArchive) — the archive holds no tiles at
///   all, and the band's range does at least fall within what it declares
///   (or it declares nothing usable: a zero-tile tylertoo archive
///   finalizes as z0-0 however it was written, so z0-0 here is an absent
///   declaration rather than a claim about z0). Treated like
///   [`Overshoot`](Self::Overshoot): an error by default, downgradable with
///   `--allow-missing-zooms`.
/// * [`DeclaredButEmpty`](Self::DeclaredButEmpty) — the band shares no zoom
///   with what the archive actually holds, but does share one with what it
///   declares. The band contributes nothing, which is always worth a
///   warning, but is not a configuration error: the archive's own
///   declaration says this span may legitimately be empty (#380's "an empty
///   zoom is just an absent tile", applied to a `--band` range instead of a
///   whole archive). Never gated behind a flag — there is no real tile to
///   lose.
/// * [`Overshoot`](Self::Overshoot) — the band overlaps the archive's actual
///   tiles, but does not cover all of them: some zoom the archive genuinely
///   holds falls outside the band's declared range and would be dropped. An
///   error by default; downgradable to a warning for a deliberately sparse
///   pyramid.
/// * [`Subrange`](Self::Subrange) — the band's range and the archive's
///   actual range contain one another (either direction). Fully supported
///   and unremarkable: a band narrower than the archive's actual tiles is
///   the documented way to split one pre-tiled archive across several bands
///   (e.g. two `--band` entries pointing at the same z0-z13 archive, one
///   declaring z0-5 and the other z6-13); a band wider than the archive's
///   actual tiles simply declares zooms the archive never wrote, which is
///   harmless since nothing real falls outside it. The per-tile drop count
///   at the merge site still applies in the narrower direction (tiles
///   outside the declared subrange are not copied), but only at
///   `log::info`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeFit {
    Disjoint,
    EmptyArchive,
    DeclaredButEmpty,
    Overshoot,
    Subrange,
}

/// Classify `band`'s declared range against an archive's declared range
/// (`declared`: header widened by `vector_layers`, see
/// [`declared_zoom_range`]) and its actual tile range (`actual`, `None` when the
/// archive holds no tiles). See [`RangeFit`].
fn band_archive_range_fit(band: &Band, declared: (u8, u8), actual: Option<(u8, u8)>) -> RangeFit {
    let (d_lo, d_hi) = declared;
    let overlaps_declared = band.min_zoom <= d_hi && band.max_zoom >= d_lo;
    let Some((a_lo, a_hi)) = actual else {
        // An empty archive used to short-circuit to `EmptyArchive` before
        // declared-disjointness was ever considered, which let
        // `--allow-missing-zooms` wave through a band that shares no zoom
        // with even what the archive declares -- a range
        // [`check_band_zoom_range`] promises no flag can make possible.
        // Disjointness is settled first here instead.
        //
        // With one exception, and it is why the short-circuit looked right:
        // a zero-tile tylertoo archive finalizes as z0-0 in both its header
        // and its `vector_layers` *whatever* range it was told to declare
        // (`set_declared_min_zoom` only widens from a tile actually written,
        // and there is none), so `(0, 0)` alongside no tiles is the absence
        // of a declaration, not a claim about z0. #514 issue #2 turns on
        // exactly that: a band above z0 hard-errored as `Disjoint` with a
        // message falsely claiming the archive declared z0. Any other range did come from somewhere --
        // an externally produced archive -- and is a real declaration to be
        // held to.
        return if declared == (0, 0) || overlaps_declared {
            RangeFit::EmptyArchive
        } else {
            RangeFit::Disjoint
        };
    };
    // Checked against the archive's *actual* tiles first, and only then
    // against its declaration: a band overlapping real tiles can demonstrably
    // write something, so it is never `Disjoint`, even if a lying header
    // declares a range those tiles fall outside.
    let overlaps_actual = band.min_zoom <= a_hi && band.max_zoom >= a_lo;
    if !overlaps_actual {
        return if overlaps_declared {
            RangeFit::DeclaredButEmpty
        } else {
            RangeFit::Disjoint
        };
    }
    let band_covers_actual = band.min_zoom <= a_lo && band.max_zoom >= a_hi;
    let actual_covers_band = a_lo <= band.min_zoom && a_hi >= band.max_zoom;
    if band_covers_actual || actual_covers_band {
        RangeFit::Subrange
    } else {
        RangeFit::Overshoot
    }
}

/// `z{lo}` for a single zoom, `z{lo}-{hi}` for a span.
fn zoom_span(lo: u8, hi: u8) -> String {
    if lo == hi {
        format!("z{lo}")
    } else {
        format!("z{lo}-{hi}")
    }
}

/// The zoom(s) an [`RangeFit::Overshoot`] band's archive genuinely holds but
/// the band's declared range excludes -- real tiles that would be dropped --
/// for the error/warning message.
///
/// Reversed from #495's `missing_zoom_span`: that function reported the
/// zooms the *band* declared beyond the *header*, back when `Overshoot` meant
/// "band wider than archive". #514 redefines `Overshoot` as "archive's
/// actual tiles wider than band" (see [`RangeFit`]), so this reports the
/// other direction -- the archive's actual span outside the band.
fn dropped_zoom_span(band: &Band, actual_min: u8, actual_max: u8) -> String {
    let mut parts = Vec::new();
    if actual_min < band.min_zoom {
        parts.push(zoom_span(actual_min, band.min_zoom - 1));
    }
    if actual_max > band.max_zoom {
        parts.push(zoom_span(band.max_zoom + 1, actual_max));
    }
    parts.join(" and ")
}

/// Validate one band's declared zoom range against its archive's declared
/// range (header widened by `vector_layers`) and its actual tile range (#495, reworked for #514), right
/// after [`BandArchive::open`] has read both. See [`RangeFit`] for the
/// five-way contract this enforces.
///
/// `allow_missing_zooms` downgrades [`RangeFit::Overshoot`] and
/// [`RangeFit::EmptyArchive`] from an error to a warning, for a caller
/// building a deliberately sparse pyramid. It has no effect on
/// [`RangeFit::Disjoint`], which is always an error -- there is no flag that
/// makes an impossible range possible -- nor on [`RangeFit::DeclaredButEmpty`],
/// which is never an error in the first place.
///
/// Returns the classification it accepted, so the merge loop below can tell
/// *why* a band skipped tiles rather than inferring it from the count. See
/// [`log_band_skip`].
fn check_band_zoom_range(
    band: &Band,
    archive: &BandArchive,
    allow_missing_zooms: bool,
) -> Result<RangeFit, Error> {
    let actual = archive.actual_zoom_range()?;
    // Header widened by the archive's own `vector_layers`: since #529/#522 a
    // tylertoo archive's header is its actual tiles, so its declaration
    // (`tiles --min-zoom 0` over tiles at z5-14) lives only in the metadata.
    // Reading the header alone turned #514's documented `DeclaredButEmpty`
    // warning into a `Disjoint` hard error for exactly that archive.
    let declared = archive.declared_zoom_range(actual.is_some());
    let fit = band_archive_range_fit(band, declared, actual);
    match fit {
        RangeFit::Subrange => Ok(fit),
        RangeFit::Disjoint => Err(Error::PMTilesWrite(format!(
            "band {:?} ({}) declares {} but that archive declares {} (header and \
             vector_layers), entirely outside the band's range; there is no zoom this band \
             could write",
            band.layer,
            band.input.display(),
            zoom_span(band.min_zoom, band.max_zoom),
            zoom_span(declared.0, declared.1),
        ))),
        RangeFit::EmptyArchive => {
            if allow_missing_zooms {
                log::warn!(
                    "band {:?} declares {} but {} holds no tiles at all; this band will \
                     contribute nothing (allowed by --allow-missing-zooms)",
                    band.layer,
                    zoom_span(band.min_zoom, band.max_zoom),
                    band.input.display(),
                );
                Ok(fit)
            } else {
                Err(Error::PMTilesWrite(format!(
                    "band {:?} declares {} but {} holds no tiles at all; pass \
                     --allow-missing-zooms (or set PyramidOptions::allow_missing_zooms) for \
                     a deliberately sparse pyramid",
                    band.layer,
                    zoom_span(band.min_zoom, band.max_zoom),
                    band.input.display(),
                )))
            }
        }
        RangeFit::DeclaredButEmpty => {
            log::warn!(
                "band {:?} declares {} but {} holds no tiles in that span (it declares {} \
                 in its header/vector_layers); this band will contribute nothing",
                band.layer,
                zoom_span(band.min_zoom, band.max_zoom),
                band.input.display(),
                zoom_span(declared.0, declared.1),
            );
            Ok(fit)
        }
        RangeFit::Overshoot => {
            // Only reachable when `actual` is `Some` (`EmptyArchive` is
            // matched above), so this unwrap cannot fail.
            let (a_lo, a_hi) = actual.expect("Overshoot implies a non-empty actual range");
            let dropped = dropped_zoom_span(band, a_lo, a_hi);
            if allow_missing_zooms {
                log::warn!(
                    "band {:?} declares {} but {} holds {}; zooms {dropped} will be \
                     silently dropped (allowed by --allow-missing-zooms)",
                    band.layer,
                    zoom_span(band.min_zoom, band.max_zoom),
                    band.input.display(),
                    zoom_span(a_lo, a_hi),
                );
                Ok(fit)
            } else {
                Err(Error::PMTilesWrite(format!(
                    "band {:?} declares {} but {} holds {}; zooms {dropped} would be \
                     silently dropped; pass --allow-missing-zooms (or set \
                     PyramidOptions::allow_missing_zooms) for a deliberately sparse pyramid",
                    band.layer,
                    zoom_span(band.min_zoom, band.max_zoom),
                    band.input.display(),
                    zoom_span(a_lo, a_hi),
                )))
            }
        }
    }
}

/// Fold one band archive's bounds into the running union, or record it in
/// `bands_without_bounds` (and warn) when the archive carried none (#495).
fn track_band_bounds(
    band: &Band,
    archive: &BandArchive,
    union: &mut Option<TileBounds>,
    bands_without_bounds: &mut Vec<String>,
) {
    match archive.bounds() {
        Some(b) => match union.as_mut() {
            Some(u) => u.expand(&b),
            None => *union = Some(b),
        },
        None => {
            let label = band_label(band);
            log::warn!(
                "band {label}: no usable bounds; excluded from the merged \
                 archive's bounds union"
            );
            bands_without_bounds.push(label);
        }
    }
}

/// Log a band's skipped-tile count, keyed on the [`RangeFit`]
/// [`check_band_zoom_range`] accepted it under (#495, corrected for #514).
///
/// Only [`RangeFit::Subrange`] gets the benign "split one archive across
/// several bands" line, and only at `log::info`: that is the documented
/// workflow, where the tiles left behind belong to a sibling band. The doc
/// here used to claim `check_band_zoom_range` had "already ruled out every
/// case except a genuine subrange", which stopped being true the moment
/// #514 added the flag-downgraded and declared-but-empty acceptances. A
/// [`RangeFit::DeclaredButEmpty`] band in particular drops *every* tile its
/// archive holds and contributes nothing -- the #514 S1 false-accept --
/// and reporting that as ordinary subrange use is precisely the reassurance
/// that hid it. Those cases were already warned about, accurately and by
/// name, when they were classified; saying it again here in weaker words
/// would only bury the warning.
fn log_band_skip(band: &Band, skipped: usize, fit: RangeFit) {
    if skipped == 0 || fit != RangeFit::Subrange {
        return;
    }
    log::info!(
        "band {:?} ({}): dropped {skipped} tile(s) outside its declared subrange \
         z{}-{}; the archive holds a wider range and this band uses only part of it",
        band.layer,
        band.input.display(),
        band.min_zoom,
        band.max_zoom
    );
}

/// When bands share a zoom, an archive's own MVT layer must match its
/// `--band` label, or the merge would write two layers of one name into a
/// tile (#385, MVT §4.1). The `:LAYER` of a pre-tiled band is a label; the
/// layer name inside its tiles is whatever the archive was exported with.
/// An archive whose metadata declares no layers cannot be checked -- that
/// case has already been warned about.
fn check_shared_zoom_layer_labels(bands: &[Band], archives: &[BandArchive]) -> Result<(), Error> {
    for (band, archive) in bands.iter().zip(archives) {
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
    Ok(())
}

/// Refuse a tile id that does not ascend past the last one written (#403).
///
/// A band's directory is archive-supplied and nothing upstream checks its
/// order, yet both merge paths feed the writer in directory order and opt it
/// into #506's clustered check, whose `debug_assert!` a descending add trips.
/// Strictly ascending also rules out a repeat — a run overlapping the next
/// entry, say — which is why neither path keeps a per-tile map.
fn ensure_ascending(last: &mut Option<u64>, id: u64, band: &Band) -> Result<(), Error> {
    if let Some(prev) = last.filter(|&prev| id <= prev) {
        return Err(out_of_order(band, id, prev));
    }
    *last = Some(id);
    Ok(())
}

/// The error for a band directory that lists `id` after `prev` (`id <= prev`).
fn out_of_order(band: &Band, id: u64, prev: u64) -> Error {
    Error::PMTilesWrite(format!(
        "{}: directory lists tile id {id} after {prev}; a PMTiles directory \
         must address tiles in ascending id order",
        band.input.display()
    ))
}

/// Prefix an error from walking `path`'s directory with that path. The
/// shared-zoom merge interleaves every band's directory, so an unattributed
/// "tile data past end of archive" names none of the `--band` archives.
fn in_band(path: &Path, e: Error) -> Error {
    let msg = match e {
        Error::PMTilesWrite(m) => m,
        other => other.to_string(),
    };
    Error::PMTilesWrite(format!("{}: {msg}", path.display()))
}

/// A band's tile bytes, served from a one-slot cache keyed on the byte range.
///
/// A run-length directory entry hands every id of the run the same range, and
/// those ids are adjacent in the merge's id-ordered index, so without this
/// each one costs an identical positioned read. The clone is a memcpy of one
/// compressed tile; the read it replaces is a syscall.
fn read_cached(
    archive: &BandArchive,
    slot: &mut Option<(std::ops::Range<usize>, Vec<u8>)>,
    range: std::ops::Range<usize>,
) -> Result<Vec<u8>, Error> {
    if let Some((cached, bytes)) = slot {
        if *cached == range {
            return Ok(bytes.clone());
        }
    }
    let data = archive.tile(range.clone())?;
    *slot = Some((range, data.clone()));
    Ok(data)
}

/// One layer's entry in a merged archive's `vector_layers`.
///
/// Shared with [`crate::merge`], which unions shard archives' `vector_layers`
/// by the same rules the pyramid merge settled on in #492: same-id layers
/// collapse to one entry spanning `min(minzoom)..=max(maxzoom)` with
/// [`union_fields`]'d fields.
#[derive(Debug, Clone)]
pub(crate) struct LayerMeta {
    pub(crate) id: String,
    pub(crate) minzoom: u8,
    pub(crate) maxzoom: u8,
    /// The input's `fields` object, lifted verbatim from its metadata.
    pub(crate) fields: Value,
}

/// Collapse layers sharing an `id` into one entry, and serialize the result
/// as a `vector_layers` array (#492).
///
/// Several inputs may declare the same layer — the pyramid's aggregate bands
/// do, and every shard of one sharded build does — and a client seeing that
/// layer declared twice with conflicting ranges has no way to reconcile them.
/// Zooms widen to the union; fields union with the first type winning a
/// conflict (and saying so).
///
/// Serialized through `serde_json`, not `format!`: a layer id can be a
/// user-supplied name, and a `"` or `\` in one would otherwise break the JSON.
pub(crate) fn vector_layers_json(layers: Vec<LayerMeta>) -> Value {
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
    Value::Array(
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
    )
}

/// One `vector_layers` entry's declared `minzoom`/`maxzoom`, validated.
///
/// Shared by [`crate::merge`] (its layer union and its report's declared
/// range) and the pyramid's [`RangeFit`] classifier (an archive's declared
/// range), so the two read a foreign archive's layer zooms by the same
/// rules. `fallback` is the archive header's own `(min_zoom, max_zoom)`,
/// which `Header::from_bytes` has already range-checked.
///
/// * A key that is absent or not an unsigned integer falls back to the
///   header's value for that end.
/// * A value past [`MAX_TILE_ID_ZOOM`] is not a zoom any PMTiles archive can
///   address (and `as u8` on it would truncate: a `minzoom` of 256 became 0,
///   silently widening a merged layer to the whole pyramid); it falls back
///   to the header's value too, with a warning.
/// * An inverted result (`minzoom > maxzoom`) cannot describe any zoom, so
///   the whole entry falls back to the header's range, with a warning,
///   rather than inverting whatever union it is folded into.
pub(crate) fn layer_zoom_range(layer: &Value, fallback: (u8, u8), path: &Path) -> (u8, u8) {
    let id = layer.get("id").and_then(Value::as_str).unwrap_or("?");
    let zoom = |key: &str, fallback: u8| -> u8 {
        match layer.get(key).and_then(Value::as_u64) {
            Some(v) if v <= u64::from(MAX_TILE_ID_ZOOM) => v as u8,
            Some(v) => {
                log::warn!(
                    "{}: vector_layers[{id:?}].{key} = {v} is not a zoom; using the header's \
                     z{fallback}",
                    path.display()
                );
                fallback
            }
            None => fallback,
        }
    };
    let (lo, hi) = (zoom("minzoom", fallback.0), zoom("maxzoom", fallback.1));
    if lo > hi {
        log::warn!(
            "{}: vector_layers[{id:?}] declares minzoom {lo} > maxzoom {hi}; using the \
             header's {}",
            path.display(),
            zoom_span(fallback.0, fallback.1)
        );
        return fallback;
    }
    (lo, hi)
}

/// An archive's *declared* zoom range: its header's range widened by every
/// `vector_layers` entry's (see [`layer_zoom_range`]).
///
/// Since #529/#522 tylertoo's own writer stamps the header with the tiles
/// actually written, so a declaration wider than the real tiles (#380's
/// `--min-zoom`, #514's `--allow-missing-zooms` band maximum) survives only
/// in `vector_layers`. An externally produced archive can still widen its
/// header instead, so both sources count.
///
/// With `has_tiles` false, a `(0, 0)` range -- header or layer -- is the
/// writer's empty-archive sentinel (a zero-tile tylertoo archive finalizes
/// as z0..z0 in both, whatever it was told to declare), not a claim about
/// z0, so it is left out of the union; `(0, 0)` comes back only when nothing
/// else was declared.
pub(crate) fn declared_zoom_range(
    header: (u8, u8),
    has_tiles: bool,
    layers: impl IntoIterator<Item = (u8, u8)>,
) -> (u8, u8) {
    std::iter::once(header)
        .chain(layers)
        .filter(|&r| has_tiles || r != (0, 0))
        .fold(None, |acc: Option<(u8, u8)>, (lo, hi)| match acc {
            Some((a, b)) => Some((a.min(lo), b.max(hi))),
            None => Some((lo, hi)),
        })
        .unwrap_or((0, 0))
}

/// One band's archive, indexed and parsed exactly once.
///
/// Only the header, the directories and the metadata are held while that band
/// is merged — [`ArchiveIndex`] reads tile bodies by offset as they are
/// copied. The merge used to `std::fs::read` the whole file instead, which
/// meant a shared-zoom pyramid held every band's archive resident at once
/// (#498): fine for a hand-built band, hopeless for a multi-gigabyte one.
/// Tiles are handed out by [`BandArchive::for_each_tile`] one at a time, so a
/// run-length entry costs one read rather than N.
struct BandArchive {
    index: ArchiveIndex,
    /// `vector_layers[0].fields`, or `{}` when the metadata has no usable one.
    fields: Value,
    /// Every `vector_layers[*].id` the archive declares — the MVT layer
    /// name(s) actually inside its tiles, as opposed to the label `--band`
    /// gives it. Empty when the metadata has none.
    layer_ids: Vec<String>,
    /// Every `vector_layers` entry's validated `(minzoom, maxzoom)` (see
    /// [`layer_zoom_range`]). Empty when the metadata has none.
    layer_zooms: Vec<(u8, u8)>,
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
        // Header, directories and metadata — never the tile data. #417's
        // ceilings on the directory walk live inside [`ArchiveIndex::open`],
        // so a hostile band archive is refused there, with the same messages
        // this reader used to produce inline.
        let index = ArchiveIndex::open(path)?;
        let header = index.header();

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

        let internal = header.internal_compression;
        let header_zooms = (header.min_zoom, header.max_zoom);
        let parsed = parse_layers(index.metadata_raw(), internal, header_zooms, path, label)?;

        Ok(BandArchive {
            index,
            fields: parsed.fields,
            layer_ids: parsed.ids,
            layer_zooms: parsed.zooms,
        })
    }

    /// The archive's header.
    fn header(&self) -> &Header {
        self.index.header()
    }

    /// The archive's own header bounds, when they describe a real box.
    fn bounds(&self) -> Option<TileBounds> {
        self.index.bounds()
    }

    /// The archive's actual zoom range -- see
    /// [`ArchiveIndex::actual_zoom_range`].
    fn actual_zoom_range(&self) -> Result<Option<(u8, u8)>, Error> {
        self.index
            .actual_zoom_range()
            .map_err(|e| in_band(self.index.path(), e))
    }

    /// The archive's declared zoom range: header widened by its own
    /// `vector_layers` -- see [`declared_zoom_range`]. `has_tiles` is whether
    /// [`Self::actual_zoom_range`] found any tile.
    fn declared_zoom_range(&self, has_tiles: bool) -> (u8, u8) {
        let h = self.header();
        declared_zoom_range(
            (h.min_zoom, h.max_zoom),
            has_tiles,
            self.layer_zooms.iter().copied(),
        )
    }

    /// Hand every addressed tile to `f` as `(z, x, y, still-compressed bytes)`.
    ///
    /// Run-length entries are expanded into individual ids — a band's
    /// numbering is not the merged archive's, which re-derives its own runs.
    /// Every id of a run addresses the same body, and the ids of a run arrive
    /// consecutively, so a one-slot cache keyed on the byte range serves the
    /// whole run from one read: before it, an N-id run cost N `pread`s of the
    /// identical bytes. (The merged writer's dedup cache collapses the run
    /// again on the far side.)
    fn for_each_tile<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(u8, u32, u32, &[u8]) -> Result<(), Error>,
    {
        let mut cached: Option<(std::ops::Range<usize>, Vec<u8>)> = None;
        self.for_each_tile_range(|_, z, x, y, range| {
            if !matches!(&cached, Some((r, _)) if *r == range) {
                cached = Some((range.clone(), self.index.read_range(range)?));
            }
            let (_, data) = cached.as_ref().expect("just populated");
            f(z, x, y, data)
        })
    }

    /// Like [`Self::for_each_tile`], but hands out the tile id alongside
    /// `(z, x, y)` and the tile's byte range in the archive instead of the
    /// bytes, so a caller can index tiles across several archives first and
    /// read them later in id order (#385).
    fn for_each_tile_range<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(u64, u8, u32, u32, std::ops::Range<usize>) -> Result<(), Error>,
    {
        for tile in self.index.tiles() {
            let t = tile.map_err(|e| in_band(self.index.path(), e))?;
            f(t.id, t.z, t.x, t.y, t.range)?;
        }
        Ok(())
    }

    /// A tile's still-compressed bytes by range (from
    /// [`Self::for_each_tile_range`]).
    fn tile(&self, range: std::ops::Range<usize>) -> Result<Vec<u8>, Error> {
        self.index.read_range(range)
    }
}

/// What [`parse_layers`] lifts out of a band archive's metadata.
struct ParsedLayers {
    /// The matching (or fallback) entry's `fields`, or `{}`.
    fields: Value,
    /// Every `vector_layers[*].id`.
    ids: Vec<String>,
    /// Every entry's validated `(minzoom, maxzoom)`.
    zooms: Vec<(u8, u8)>,
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
///
/// Also returns every entry's validated `(minzoom, maxzoom)` (see
/// [`layer_zoom_range`], with `header_zooms` as the fallback), which
/// [`RangeFit`] folds into the archive's declared range.
fn parse_layers(
    raw: &[u8],
    internal: Compression,
    header_zooms: (u8, u8),
    path: &Path,
    label: &str,
) -> Result<ParsedLayers, Error> {
    let plain = compression::decompress_capped(raw, internal, MAX_INTERNAL_BYTES)
        .map_err(|e| Error::PMTilesWrite(format!("{}: metadata: {e}", path.display())))?;
    // Unparseable metadata is not fatal: the band's tiles are still usable,
    // they just end up declaring no field types.
    let parsed: Value = match serde_json::from_slice(&plain) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "{}: metadata is not valid JSON ({e}); merged layer will declare no fields, \
                 and no name, description or attribution is inherited from it",
                path.display()
            );
            return Ok(ParsedLayers {
                fields: json!({}),
                ids: Vec::new(),
                zooms: Vec::new(),
            });
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
    let zooms = layers
        .map(|v| {
            v.iter()
                .filter(|l| l.is_object())
                .map(|l| layer_zoom_range(l, header_zooms, path))
                .collect()
        })
        .unwrap_or_default();
    Ok(ParsedLayers { fields, ids, zooms })
}

/// What a pyramid build produced.
#[derive(Debug, Clone)]
pub struct PyramidReport {
    /// Distinct tile ids written to the merged archive.
    pub total_tiles: usize,
    /// `(layer, min_zoom, max_zoom, tiles)` for every band, in merge order:
    /// argument order when bands share a zoom, coarsest band first otherwise
    /// (#404).
    ///
    /// The counts can sum to more than [`Self::total_tiles`] (#403). A tile id
    /// several bands wrote at a shared zoom is one tile in the archive, its
    /// layers concatenated, so it counts once in `total_tiles` but once for
    /// each of those bands here.
    pub per_band_tiles: Vec<(String, u8, u8, usize)>,
    /// Tiles found in a band archive at a zoom outside that band's declared
    /// range, and therefore dropped. Non-zero means the band declares a
    /// subrange of what its archive actually holds (#495) — the documented
    /// way to split one pre-tiled archive across several bands.
    pub skipped: usize,
    /// A label (`"layer (path)"`) for every band whose archive header carried
    /// no usable bounds, and so was excluded from the merged archive's bounds
    /// union (#495). Empty when every band contributed real bounds. When this
    /// is non-empty but not every band's label appears, the missing ones did
    /// contribute; when it names every band, the merged archive's bounds are
    /// left unset entirely (see the "no band archive carried usable bounds"
    /// warning).
    pub bands_without_bounds: Vec<String>,
    /// The publication metadata written to the merged archive: the
    /// overrides over what the band archives carry.
    pub metadata: TilesetMetadata,
    /// The publication fields (`"name"`, `"description"`) the band archives
    /// disagreed on, and which the merged archive therefore leaves out.
    pub metadata_conflicts: Vec<String>,
    /// A label (`"layer (path)"`) for every band whose archive metadata is
    /// not valid JSON, and so passed on no name, description or attribution.
    pub bands_without_metadata: Vec<String>,
}

/// The `pyramid --report` JSON and the Python `pyramid()` dict: one shape for
/// both (#779), with each band as a `{layer, min_zoom, max_zoom, tiles}`
/// object and [`PyramidReport::skipped`] as `skipped_tiles`.
impl serde::Serialize for PyramidReport {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct BandReport<'a> {
            layer: &'a str,
            min_zoom: u8,
            max_zoom: u8,
            tiles: usize,
        }
        #[derive(serde::Serialize)]
        struct Report<'a> {
            total_tiles: usize,
            bands: Vec<BandReport<'a>>,
            skipped_tiles: usize,
            bands_without_bounds: &'a [String],
            metadata: &'a TilesetMetadata,
            metadata_conflicts: &'a [String],
            bands_without_metadata: &'a [String],
        }
        Report {
            total_tiles: self.total_tiles,
            bands: self
                .per_band_tiles
                .iter()
                .map(|(layer, min_zoom, max_zoom, tiles)| BandReport {
                    layer,
                    min_zoom: *min_zoom,
                    max_zoom: *max_zoom,
                    tiles: *tiles,
                })
                .collect(),
            skipped_tiles: self.skipped,
            bands_without_bounds: &self.bands_without_bounds,
            metadata: &self.metadata,
            metadata_conflicts: &self.metadata_conflicts,
            bands_without_metadata: &self.bands_without_metadata,
        }
        .serialize(serializer)
    }
}

/// Knobs for [`merge_bands_with_options`].
#[derive(Debug, Clone, Default)]
pub struct BandMergeOptions {
    /// Let a pre-tiled band's declared zoom range overshoot what its archive
    /// actually holds (#495) instead of failing. Off matches
    /// [`merge_bands`] exactly. A *disjoint* range (the band and its archive
    /// share no zoom at all) is always an error.
    pub allow_missing_zooms: bool,
    /// The merged archive's `name`, `description` and `attribution`, each
    /// overriding what the band archives carry for that field (`Some("")`
    /// included). An unset field is inherited: one value the bands agree on
    /// is kept, a name or description they disagree on is left out
    /// ([`PyramidReport::metadata_conflicts`]), and every distinct
    /// attribution credit is kept, sorted and joined with `" | "`. A band
    /// that uses only some of its archive's zooms still passes on the
    /// archive's whole credit.
    pub metadata: TilesetMetadata,
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
    /// Allow a pre-tiled band's declared zoom range to overshoot what its
    /// archive actually holds (#495). Off by default: an overshooting band
    /// declares zooms the archive does not have, and those zooms would
    /// render silently empty — almost always a `--band` range that
    /// disagrees with what the archive was tiled with, so it is a hard
    /// error unless this is set. Has no effect on a *disjoint* range (the
    /// band and archive share no zoom at all), which is always an error —
    /// see [`crate::pyramid::merge_bands_with_options`].
    pub allow_missing_zooms: bool,
    /// The final archive's `name`, `description` and `attribution`, each
    /// overriding what the pre-tiled band archives carry for that field
    /// (`Some("")` included); see [`BandMergeOptions::metadata`] for how an
    /// unset field is inherited.
    ///
    /// This is the only publication metadata `build_pyramid` writes:
    /// `export.metadata` is ignored, so a GeoParquet band's intermediate
    /// archive carries none and passes none on.
    pub metadata: TilesetMetadata,
}

impl Default for PyramidOptions {
    fn default() -> Self {
        Self {
            convert: ConvertOptions::default().verbatim(),
            // Every feature of an aggregate band must be drawn, so the default
            // per-tile size valve — which sheds features to fit a byte budget
            // — is off on both caps, matching `tiles --verbatim` (#761).
            export: ExportOptions::default().verbatim(),
            work_dir: None,
            allow_missing_zooms: false,
            metadata: TilesetMetadata::default(),
        }
    }
}

/// Export fields [`build_pyramid`] substitutes per band, or does not use,
/// by the name the [`ConfigError::OptionIgnored`] message gives them, with
/// why. A band's export cannot honour any of them.
const PYRAMID_IGNORED_EXPORT_FIELDS: [(&str, &str); 5] = [
    (
        "export.layer_name",
        "each band's layer comes from its --band spec",
    ),
    (
        "export.min_zoom",
        "each band declares its own --band zoom range",
    ),
    (
        "export.tile_range",
        "a pyramid band is not a shard; tile each band whole",
    ),
    (
        "export.zoom_ceiling",
        "a pyramid band is not a shard; each band's --band range bounds its zooms",
    ),
    (
        "export.metadata",
        "the output's name, description and attribution are PyramidOptions::metadata",
    ),
];

impl PyramidOptions {
    /// The defaults for a build that tiles its GeoParquet bands verbatim
    /// (`generalize: false`, [`PyramidOptions::default`]) or through the
    /// normal generalization ladder (`--generalize`). Neither puts a size
    /// cap on a band unless one is asked for (#761).
    pub fn new(generalize: bool) -> Self {
        let mut options = Self::default();
        if generalize {
            options.convert = ConvertOptions::default();
        }
        options
    }

    /// Check every option rule that needs no band (#779): the convert and
    /// export knobs every band runs with, and the export fields a band's
    /// export cannot honour (its layer, zoom range and the archive's
    /// metadata come from elsewhere; a band is never a shard).
    /// [`build_pyramid`] runs this first.
    ///
    /// # Errors
    ///
    /// [`Error::Config`], or [`Error::InvalidConfig`] for a convert knob.
    pub fn validate(&self) -> Result<(), Error> {
        self.convert
            .validate()
            .map_err(|e| Error::InvalidConfig(format!("band convert options: {e}")))?;
        self.export.validate()?;
        let defaults = ExportOptions::default();
        let set = [
            self.export.layer_name != defaults.layer_name,
            self.export.min_zoom.is_some(),
            self.export.tile_range.is_some(),
            self.export.zoom_ceiling.is_some(),
            self.export.metadata != defaults.metadata,
        ];
        if let Some(((option, reason), _)) = PYRAMID_IGNORED_EXPORT_FIELDS
            .iter()
            .zip(set)
            .find(|(_, set)| *set)
        {
            return Err(ConfigError::OptionIgnored {
                option,
                command: "pyramid",
                reason,
            }
            .into());
        }
        Ok(())
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
///
/// # Errors
///
/// Returns an error if the bands are invalid, if a band's input cannot be
/// classified, converted, or exported, or if the merge fails.
pub fn build_pyramid(
    bands: &[Band],
    output: &Path,
    opts: &PyramidOptions,
) -> Result<PyramidReport, Error> {
    // #779: the options every band runs with, before any band is read.
    opts.validate()?;
    // Validates every band up front, including (#482) that none names a
    // remote `.pmtiles` archive — pure string work, so it is cheap to run for
    // every band before any band is tiled, rather than interleaved with
    // tiling below where a late band's bad input would only surface after
    // every earlier band had already converted.
    validate_bands(bands).map_err(Error::PMTilesWrite)?;
    // #452: the archive is created only after every band is tiled and
    // merged; an unwritable output must not cost all of that first.
    crate::atomic_output::check_writable(output)?;

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

    // #514: validate every pre-tiled Archive band's declared zoom range now,
    // before any Source band below is tiled. The equivalent check inside
    // `merge_bands_with_options` used to run only after every GeoParquet
    // band in the build had already been fully converted and exported, so on
    // a mixed build a typo'd `--band` range on one archive surfaced only
    // after the other bands' hours of tiling were already sunk.
    //
    // KNOWN DUPLICATED I/O (#527 review): this is not free, and the earlier
    // "a few hundred bytes" note was wrong. `BandArchive::open` reads the
    // root directory, walks *every* leaf directory and parses the metadata,
    // so for a deep archive it is megabytes of directory, expanded into an
    // entry per tile run — and `merge_bands_with_options` opens the same
    // archives again moments later, because a direct `merge_bands` caller
    // that never goes through `build_pyramid` must get the same check.
    //
    // The handle is deliberately dropped rather than threaded into the
    // merge. Keeping it would hold every band's expanded directory resident
    // for the whole tiling phase, and the disjoint-zoom merge path below
    // opens one band at a time on purpose: holding them all at once is
    // exactly the residency #498 removed (fine for a hand-built band,
    // hopeless for a multi-gigabyte one). Paying one extra directory walk
    // per Archive band is the cheaper half of that trade — it is bounded by
    // the directories alone, never the tile bodies, and it buys failing
    // before any band is converted or any tile is read.
    for (band, kind) in bands.iter().zip(&kinds) {
        if *kind == BandSource::Archive {
            let archive = BandArchive::open(&band.input, &band.layer)?;
            check_band_zoom_range(band, &archive, opts.allow_missing_zooms)?;
        }
    }

    // The publication metadata too: credits from the archive bands that
    // cannot fit an archive's metadata fail here rather than after every
    // source band has been tiled. The merge resolves it again, and logs it,
    // from the archives it actually reads.
    let mut collector = MetadataCollector::new(&opts.metadata);
    for (band, kind) in bands.iter().zip(&kinds) {
        if *kind == BandSource::Archive {
            collect_band_metadata(&mut collector, band)?;
        }
    }
    collector.resolve().map_err(Error::InvalidConfig)?;

    // Keeps every intermediate alive for the merge and unlinks them on drop —
    // including the early-return paths below. TempPaths, not open
    // NamedTempFiles: convert and export publish by renaming a sibling over
    // the path, which Windows refuses while a handle to it is open (#678).
    let mut scratch: Vec<TempPath> = Vec::new();
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

        let named = |suffix: &str| -> Result<TempPath, Error> {
            let mut b = tempfile::Builder::new();
            b.prefix("tylertoo-pyramid-").suffix(suffix);
            match &opts.work_dir {
                Some(dir) => b.tempfile_in(dir),
                None => b.tempfile(),
            }
            .map(NamedTempFile::into_temp_path)
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
        convert_to_overviews(&band.input, &overview, &convert).map_err(|e| {
            Error::PMTilesWrite(format!("band {:?}: convert failed: {e}", band.layer))
        })?;

        let export = ExportOptions {
            layer_name: band.layer.clone(),
            // The band's own range, not a caller-wide value: a `min_zoom`
            // set on `opts.export` would apply to every band alike, and be
            // rejected by any band whose range starts finer (#380).
            min_zoom: Some(band.min_zoom),
            // #427: the band's export spill goes where its intermediates go,
            // unless the caller placed it explicitly.
            spill_dir: opts
                .export
                .spill_dir
                .clone()
                .or_else(|| opts.work_dir.clone()),
            // The final archive's metadata is `opts.metadata`, applied by the
            // merge; an intermediate archive passes nothing on.
            metadata: TilesetMetadata::default(),
            ..opts.export.clone()
        };
        export_pmtiles(&overview, &archive, &export).map_err(|e| {
            Error::PMTilesWrite(format!("band {:?}: export failed: {e}", band.layer))
        })?;

        tiled.push(Band {
            input: archive.to_path_buf(),
            layer: band.layer.clone(),
            min_zoom: band.min_zoom,
            max_zoom: band.max_zoom,
        });
        // The overview is only needed for the export above; the archive must
        // outlive the merge.
        drop(overview);
        scratch.push(archive);
    }

    merge_bands_with_options(
        &tiled,
        output,
        &BandMergeOptions {
            allow_missing_zooms: opts.allow_missing_zooms,
            metadata: opts.metadata.clone(),
        },
    )
}

/// Add one band archive's publication metadata to `collector`, labelled
/// `"layer (path)"` as [`PyramidReport::bands_without_bounds`] labels a band.
///
/// Reads only the header and the metadata section
/// ([`ArchiveIndex::read_metadata_only`]), never the directories: the
/// disjoint-zoom merge opens one band at a time on purpose, and the metadata
/// has to be resolved from every band before the first one is copied.
/// Returns `false` for metadata that is not valid JSON, which contributes
/// nothing ([`parse_layers`] warns about it when the band is opened for its
/// tiles).
fn collect_band_metadata(collector: &mut MetadataCollector, band: &Band) -> Result<bool, Error> {
    let plain = ArchiveIndex::read_metadata_only(&band.input)?;
    let Ok(root) = serde_json::from_slice::<Value>(&plain) else {
        return Ok(false);
    };
    collector
        .add_json(&band_label(band), &root)
        .map_err(Error::InvalidConfig)?;
    Ok(true)
}

/// How reports and messages name a band: `"layer (path)"`.
fn band_label(band: &Band) -> String {
    format!("{} ({})", band.layer, band.input.display())
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
///
/// # Errors
///
/// Returns an error in the cases [`merge_bands_with_options`] does with
/// default options.
pub fn merge_bands(bands: &[Band], output: &Path) -> Result<PyramidReport, Error> {
    merge_bands_with_options(bands, output, &BandMergeOptions::default())
}

/// [`merge_bands`] with [`BandMergeOptions`]: a band's declared zoom range
/// may overshoot its archive, and the merged archive's publication metadata
/// can be set.
///
/// The publication metadata is resolved from every band before any tile is
/// read, so credits too large for an archive's metadata fail before the
/// merge does any real work.
///
/// # Errors
///
/// Returns an error if the bands are invalid, if a band's archive cannot be
/// opened or read, if a band's declared zoom range does not fit what its
/// archive holds, if two bands write one tile for the same layer, if the
/// publication metadata is too large for an archive's metadata, or if
/// writing the output fails.
pub fn merge_bands_with_options(
    bands: &[Band],
    output: &Path,
    options: &BandMergeOptions,
) -> Result<PyramidReport, Error> {
    let allow_missing_zooms = options.allow_missing_zooms;
    // A library caller can hand over same-layer bands that overlap in zoom
    // but cover disjoint geography, which the per-tile collision check below
    // would never catch.
    validate_bands(bands).map_err(Error::PMTilesWrite)?;
    crate::atomic_output::check_writable(output)?;
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

    let mut collector = MetadataCollector::new(&options.metadata);
    let mut bands_without_metadata = Vec::new();
    for band in bands {
        if !collect_band_metadata(&mut collector, band)? {
            bands_without_metadata.push(band_label(band));
        }
    }
    let publication = collector.resolve().map_err(Error::InvalidConfig)?;
    collector.log_resolution();

    // StreamingPmtilesWriter, not PmtilesWriter: it spools tile bytes to a temp
    // file instead of holding every tile in RAM, and it deduplicates. The merge
    // is exactly the workload that needs both — an aggregate band is mostly
    // identical tiles, and re-expanding its runs without dedup turned a
    // 300-byte band archive into 164 KiB.
    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)
        .map_err(|e| Error::PMTilesWrite(format!("Failed to create streaming writer: {e}")))?;
    // The writer appends tile data in add order and stamps the header
    // `clustered`, which promises offsets monotonic in tile id. Both paths
    // below add in ascending id order — the shared-zoom path by k-way merge,
    // the disjoint path by sorting bands coarsest first (#404) — and
    // [`ensure_ascending`] refuses a band directory that breaks it, so #506's
    // check can be on.
    writer.set_expect_clustered(true);
    writer.set_metadata(publication.metadata.clone());
    // No `set_declared_min_zoom`/`set_declared_max_zoom` here: #380's
    // declared band range (a band's coarse zooms may generalize to nothing;
    // `--allow-missing-zooms` lets a band declare deeper zooms than its
    // archive holds) reaches the merged archive through each band's
    // `LayerMeta` below, which carries the band's own `min_zoom`/`max_zoom`
    // into `vector_layers` via `finish_merge`'s `set_vector_layers_json` --
    // which takes precedence over the writer's declared range anyway. The
    // merged header is always the tiles actually copied (#529, #522).
    let mut layers: Vec<LayerMeta> = Vec::new();
    let mut per_band = Vec::new();
    let mut skipped_total = 0usize;
    let mut union: Option<TileBounds> = None;
    let mut bands_without_bounds: Vec<String> = Vec::new();

    if bands_share_zooms(bands) {
        // #385: bands in different layers share zooms, so one tile id can
        // come from several bands. A k-way merge over the bands' directories
        // (#403) emits each id once, in ascending order: a tile only one band
        // wrote is copied still-compressed exactly as the single-pass path
        // below does; a tile several bands wrote is decompressed, its layer
        // messages concatenated in band order (an MVT tile is a protobuf
        // message whose only field is a repeated `layers`, so byte
        // concatenation IS layer concatenation), and recompressed. Each
        // band's index is held for the duration — header, directories and
        // metadata — plus one look-ahead tile per band. The merge used to
        // build a map entry per tile id of every band before writing one,
        // which is O(tiles) where this is O(bands).
        let archives: Vec<BandArchive> = bands
            .iter()
            .map(|b| BandArchive::open(&b.input, &b.layer))
            .collect::<Result<_, _>>()?;
        // #495: catch a band whose declared range the archive cannot honor
        // before any tile is touched. The accepted classifications are kept
        // for `log_band_skip` below, which must not describe a band that
        // contributed nothing as ordinary subrange use.
        let fits = bands
            .iter()
            .zip(&archives)
            .map(|(band, archive)| check_band_zoom_range(band, archive, allow_missing_zooms))
            .collect::<Result<Vec<_>, _>>()?;
        // validate_bands only sees the labels, not what is actually inside
        // each archive's tiles; this is the check that catches the mismatch.
        check_shared_zoom_layer_labels(bands, &archives)?;
        for (band, archive) in bands.iter().zip(&archives) {
            track_band_bounds(band, archive, &mut union, &mut bands_without_bounds);
            layers.push(LayerMeta {
                id: band.layer.clone(),
                minzoom: band.min_zoom,
                maxzoom: band.max_zoom,
                fields: archive.fields.clone(),
            });
        }
        let mut counts = vec![0usize; bands.len()];
        let mut skipped = vec![0usize; bands.len()];
        let mut total = 0usize;
        let mut combined = 0usize;
        // Every in-range band copy of the current id, in band order.
        let mut group: Vec<(usize, std::ops::Range<usize>)> = Vec::with_capacity(bands.len());
        let mut last_id: Option<u64> = None;
        let mut buf = Vec::new();
        // One cached (range, bytes) per band, for the same reason
        // `for_each_tile` caches: a run-length entry gives every id of the run
        // the same byte range, and those ids are adjacent in the merge, so
        // without this each one re-reads the identical body.
        let mut cache: Vec<Option<(std::ops::Range<usize>, Vec<u8>)>> = vec![None; archives.len()];
        let mut merged =
            MergedTiles::new(archives.iter().map(|a| a.index.tiles()).collect()).peekable();

        while let Some((first_band, tile)) = merged.next() {
            let t = tile.map_err(|e| in_band(&bands[first_band].input, e))?;
            let (id, z, x, y) = (t.id, t.z, t.x, t.y);
            ensure_ascending(&mut last_id, id, &bands[first_band])?;

            // Gather every band's copy of `id`. A peeked error is left for
            // the next turn of the outer loop to return.
            group.clear();
            let mut prev_band = None;
            let mut next = Some((first_band, t.range));
            while let Some((bi, range)) = next.take() {
                let band = &bands[bi];
                // The merge emits (id, band) ascending, so a band repeating
                // an id inside its own directory shows up right after itself.
                if prev_band.replace(bi) == Some(bi) {
                    return Err(out_of_order(band, id, id));
                }
                if z < band.min_zoom || z > band.max_zoom {
                    skipped[bi] += 1;
                } else {
                    if let Some((prev, _)) =
                        group.iter().find(|(p, _)| bands[*p].layer == band.layer)
                    {
                        return Err(Error::PMTilesWrite(format!(
                            "tile {z}/{x}/{y} claimed twice for layer {:?} (bands {} and {})",
                            band.layer,
                            bands[*prev].input.display(),
                            band.input.display()
                        )));
                    }
                    group.push((bi, range));
                    counts[bi] += 1;
                }
                if let Some((bi, Ok(n))) =
                    merged.next_if(|(_, peeked)| matches!(peeked, Ok(n) if n.id == id))
                {
                    next = Some((bi, n.range));
                }
            }

            match group.as_slice() {
                // Every band's copy fell outside that band's range.
                [] => continue,
                [(bi, range)] => {
                    let data = read_cached(&archives[*bi], &mut cache[*bi], range.clone())?;
                    let hash = TileHasher::hash(&data);
                    writer
                        .add_tile_precompressed(z, x, y, hash, &data, data.len(), 0)
                        .map_err(|e| Error::PMTilesWrite(format!("Failed to add tile: {e}")))?;
                }
                refs => {
                    buf.clear();
                    for (bi, range) in refs {
                        let raw = read_cached(&archives[*bi], &mut cache[*bi], range.clone())?;
                        let plain = compression::decompress_capped(
                            &raw,
                            archives[*bi].header().tile_compression,
                            MAX_TILE_BYTES,
                        )
                        .map_err(|e| {
                            Error::PMTilesWrite(format!(
                                "tile {z}/{x}/{y} of band {:?} failed to decompress: {e}",
                                bands[*bi].layer
                            ))
                        })?;
                        buf.extend_from_slice(&plain);
                    }
                    // Compressed here and keyed on the COMPRESSED bytes, like
                    // a tile copied through above, not handed to `add_tile`
                    // (which keys on the uncompressed bytes): one writer, one
                    // dedup keyspace (#403). Compression is deterministic, so
                    // equal combined tiles still collapse to one body.
                    let gz = compression::compress(&buf, Compression::Gzip).map_err(|e| {
                        Error::PMTilesWrite(format!("tile {z}/{x}/{y} failed to compress: {e}"))
                    })?;
                    writer
                        .add_tile_precompressed(z, x, y, TileHasher::hash(&gz), &gz, buf.len(), 0)
                        .map_err(|e| Error::PMTilesWrite(format!("Failed to add tile: {e}")))?;
                    combined += 1;
                }
            }
            total += 1;
        }
        log::info!(
            "[pyramid] {} tile id(s) shared by several layers were merged; {} tile(s) copied as-is",
            combined,
            total - combined
        );
        for (bi, band) in bands.iter().enumerate() {
            log_band_skip(band, skipped[bi], fits[bi]);
            skipped_total += skipped[bi];
            per_band.push((band.layer.clone(), band.min_zoom, band.max_zoom, counts[bi]));
        }
        return finish_merge(
            writer,
            union,
            layers,
            per_band,
            MergeTally {
                skipped_total,
                bands_without_bounds,
                total_tiles: total,
                publication,
                bands_without_metadata,
            },
            output,
        );
    }

    // Disjoint zoom ranges mean no band can overwrite another's tile, and a
    // repeat inside one band's directory fails [`ensure_ascending`]: strictly
    // ascending ids are the whole collision check, with no per-tile map (the
    // `(z, x, y)` map this replaced was O(tiles), #403).
    let mut last_id: Option<u64> = None;
    let mut total_tiles = 0usize;

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
        // #495: catch a band whose declared range the archive cannot honor
        // before any tile is touched.
        let fit = check_band_zoom_range(band, &archive, allow_missing_zooms)?;
        track_band_bounds(band, &archive, &mut union, &mut bands_without_bounds);
        let mut n = 0usize;
        let mut skipped = 0usize;
        archive.for_each_tile(|z, x, y, data| {
            if z < band.min_zoom || z > band.max_zoom {
                skipped += 1;
                return Ok(());
            }
            ensure_ascending(&mut last_id, crate::pmtiles_writer::tile_id(z, x, y), band)?;
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
        log_band_skip(band, skipped, fit);
        skipped_total += skipped;
        total_tiles += n;
        per_band.push((band.layer.clone(), band.min_zoom, band.max_zoom, n));
        layers.push(LayerMeta {
            id: band.layer.clone(),
            minzoom: band.min_zoom,
            maxzoom: band.max_zoom,
            fields: archive.fields.clone(),
        });
    }

    finish_merge(
        writer,
        union,
        layers,
        per_band,
        MergeTally {
            skipped_total,
            bands_without_bounds,
            total_tiles,
            publication,
            bands_without_metadata,
        },
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
pub(crate) fn union_fields(into: &mut Value, from: Value, layer: &str) {
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

/// Bookkeeping neither branch of [`merge_bands_with_options`] can finish
/// alone, threaded into [`finish_merge`] as one value to keep its argument
/// count down.
struct MergeTally {
    skipped_total: usize,
    bands_without_bounds: Vec<String>,
    total_tiles: usize,
    publication: Resolved,
    bands_without_metadata: Vec<String>,
}

/// The tail shared by both merge paths: bounds, `vector_layers`, finalize.
fn finish_merge(
    mut writer: StreamingPmtilesWriter,
    union: Option<TileBounds>,
    layers: Vec<LayerMeta>,
    per_band: Vec<(String, u8, u8, usize)>,
    tally: MergeTally,
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
    writer.set_vector_layers_json(vector_layers_json(layers).to_string());
    writer
        .finalize(output)
        .map_err(|e| Error::PMTilesWrite(format!("Failed to write {}: {e}", output.display())))?;

    Ok(PyramidReport {
        total_tiles: tally.total_tiles,
        per_band_tiles: per_band,
        skipped: tally.skipped_total,
        bands_without_bounds: tally.bands_without_bounds,
        metadata: tally.publication.metadata,
        metadata_conflicts: tally.publication.conflicts,
        bands_without_metadata: tally.bands_without_metadata,
    })
}

/// Fuzz hook (#424): treat `bytes` as a whole PMTiles archive and put it
/// through everything `merge_bands` does to a `--band` input short of
/// writing output: [`BandArchive::open`] (header, directory walk, metadata
/// layer parse), the zoom-range and bounds queries, and a full
/// [`BandArchive::for_each_tile`] pass reading every addressed tile body.
///
/// Returns the number of tiles handed out. The archive goes through a temp
/// file because the reader is `pread`-based by design (#498). Only the
/// `fuzz/` crate calls this; the `fuzzing` feature is never on in a normal
/// build.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_band_archive(bytes: &[u8]) -> Result<u64, Error> {
    use std::io::Write as _;
    let mut file = NamedTempFile::new().map_err(Error::Io)?;
    file.write_all(bytes).map_err(Error::Io)?;
    file.flush().map_err(Error::Io)?;

    let band = BandArchive::open(file.path(), "")?;
    let _ = band.bounds();
    let actual = band.actual_zoom_range()?;
    let _ = band.declared_zoom_range(actual.is_some());
    let mut tiles = 0u64;
    band.for_each_tile(|_z, _x, _y, _data| {
        tiles += 1;
        Ok(())
    })?;
    Ok(tiles)
}

/// Fuzz hook (#633): read `bytes` as an input archive's metadata JSON, the
/// way `pyramid` and `merge` read its `vector_layers`.
///
/// The first two bytes are the archive header's `(min_zoom, max_zoom)`,
/// folded into the range `Header::from_bytes` guarantees (`lo <= hi <=
/// MAX_TILE_ID_ZOOM`); the rest is the JSON. Runs the `--band` metadata parse
/// ([`parse_layers`], both with no label and with the first layer's id), then
/// `merge`'s layer collection: [`layer_zoom_range`] on every entry,
/// [`declared_zoom_range`], and [`vector_layers_json`] over the layers twice
/// so every id collides and [`union_fields`] runs. Panics if any range that
/// comes back is not a zoom range. Only the `fuzz/` crate calls this; the
/// `fuzzing` feature is never on in a normal build.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_layer_zooms(bytes: &[u8]) -> Result<Value, Error> {
    let check = |(lo, hi): (u8, u8)| {
        assert!(
            lo <= hi && hi <= MAX_TILE_ID_ZOOM,
            "z{lo}..z{hi} is not a zoom range"
        );
    };
    let [a, b, json @ ..] = bytes else {
        return Err(Error::PMTilesWrite("short input".into()));
    };
    let (a, b) = (a % (MAX_TILE_ID_ZOOM + 1), b % (MAX_TILE_ID_ZOOM + 1));
    let header = (a.min(b), a.max(b));
    let path = Path::new("fuzz.pmtiles");

    let parsed = parse_layers(json, Compression::None, header, path, "")?;
    parsed.zooms.iter().copied().for_each(check);
    check(declared_zoom_range(header, true, parsed.zooms.clone()));
    check(declared_zoom_range(header, false, parsed.zooms));
    if let Some(first) = parsed.ids.first() {
        parse_layers(json, Compression::None, header, path, first)?;
    }

    let Ok(meta) = serde_json::from_slice::<Value>(json) else {
        return Ok(Value::Null);
    };
    let mut layers = Vec::new();
    for l in meta
        .get("vector_layers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = l.get("id").and_then(Value::as_str) else {
            continue;
        };
        let (minzoom, maxzoom) = layer_zoom_range(l, header, path);
        check((minzoom, maxzoom));
        layers.push(LayerMeta {
            id: id.to_string(),
            minzoom,
            maxzoom,
            fields: l
                .get("fields")
                .filter(|f| f.is_object())
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        });
    }
    check(declared_zoom_range(
        header,
        true,
        layers.iter().map(|l| (l.minzoom, l.maxzoom)),
    ));
    let doubled = layers
        .iter()
        .cloned()
        .chain(layers.iter().cloned())
        .collect();
    Ok(vector_layers_json(doubled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pmtiles_writer::{
        decode_directory, offsets_are_clustered, tile_id_to_zxy, DirEntry, MAX_LEAF_DIRECTORIES,
    };
    use std::collections::HashMap;

    /// #445: `parse_bands` names a missing local input instead of letting
    /// the reader fail later without a path.
    #[test]
    fn parse_bands_names_a_missing_input() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.parquet");
        let err = parse_bands(&[format!("0-5:{}", missing.display())]).unwrap_err();
        assert!(err.starts_with("band input not found: "), "{err}");
        assert!(err.contains("nope.parquet"), "{err}");
        assert!(!err.contains("hint"), "{err}");
    }

    /// The `=LAYER` escape form strips a trailing `=X`; when the unstripped
    /// path exists, the error says so.
    #[test]
    fn parse_bands_hints_at_a_stripped_equals_layer() {
        let dir = tempfile::tempdir().unwrap();
        let hive = dir.path().join("admin:country_code=BR");
        std::fs::create_dir(&hive).unwrap();
        let spec = format!("0-13={}", hive.display());
        let err = parse_bands(&[spec]).unwrap_err();
        assert!(err.starts_with("band input not found: "), "{err}");
        assert!(err.contains("hint: "), "{err}");
        assert!(err.contains("exists"), "{err}");

        // Same spelling, nothing on disk: still the hint, without "exists".
        let gone = dir.path().join("other:country_code=FR");
        let err = parse_bands(&[format!("0-13={}", gone.display())]).unwrap_err();
        assert!(err.contains("hint: the trailing \"=FR\""), "{err}");
    }

    /// Remote URLs and globs are resolved by the reader, so they skip the
    /// existence check.
    #[test]
    fn parse_bands_defers_urls_and_globs() {
        let bands = parse_bands(&[
            "0-5:s3://bucket/a.parquet:a",
            "6-9:/nonexistent/dir/*.parquet:b",
        ])
        .unwrap();
        assert_eq!(bands.len(), 2);
        assert_eq!(bands[0].layer, "a");
        assert_eq!((bands[1].min_zoom, bands[1].max_zoom), (6, 9));
    }

    /// Spec and set errors come through unchanged, before any existence
    /// check.
    #[test]
    fn parse_bands_propagates_parse_and_validation_errors() {
        let none: [&str; 0] = [];
        assert!(parse_bands(&none).unwrap_err().contains("at least one"));
        assert!(parse_bands(&["nonsense"]).is_err());
        let err = parse_bands(&["0-5:/x/a.parquet:l", "5-9:/x/b.parquet:l"]).unwrap_err();
        assert!(err.contains("overlap"), "{err}");
    }

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
                let plain = compression::decompress_capped(data, Compression::Gzip, MAX_TILE_BYTES)
                    .unwrap();
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
        assert_eq!(
            PyramidOptions::default().export.compressed_tile_size_limit,
            None,
            "nor is one on the compressed tile (#761 made it a default)"
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

    /// #514 issue #4: a pre-tiled Archive band's zoom range is validated
    /// before any Source band is tiled, not only inside `merge_bands` after
    /// every GeoParquet band has already been fully converted and exported.
    ///
    /// Ordering observable: the Source band here points at a GeoParquet file
    /// that does not exist, so tiling it fails immediately with a "convert
    /// failed" error. The Archive band's range is disjoint from its
    /// archive's declared header. If the archive's range were only checked
    /// at the merge site (the old behavior), `build_pyramid`'s sequential
    /// loop would reach and fail on the Source band first, and the error
    /// returned would be the "convert failed" one. Getting the zoom-range
    /// error instead proves the Archive band was validated up front, before
    /// the Source band was ever touched.
    #[test]
    fn build_pyramid_validates_archive_band_ranges_before_tiling_source_bands() {
        let dir = tempfile::tempdir().unwrap();

        let archive = dir.path().join("archive.pmtiles");
        write_band(
            &archive,
            "pre",
            &[(3, 1, 1), (4, 2, 2), (5, 4, 4)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        // Never written -- if this band were tiled at all, `convert` would
        // fail on the missing file before the archive band's range is even
        // looked at.
        let missing_source = dir.path().join("does-not-exist.parquet");
        let out = dir.path().join("out.pmtiles");

        let err = build_pyramid(
            &[
                Band {
                    input: missing_source,
                    layer: "fine".to_string(),
                    min_zoom: 6,
                    max_zoom: 7,
                },
                Band {
                    input: archive,
                    layer: "pre".to_string(),
                    min_zoom: 8,
                    max_zoom: 10,
                },
            ],
            &out,
            &PyramidOptions::default(),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("z8-10") && msg.contains("z3-5") && msg.contains("declares"),
            "expected the archive band's zoom-range error, got: {msg}"
        );
        assert!(
            !msg.contains("convert failed"),
            "the Source band must not have been tiled yet: {msg}"
        );
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
        let root = compression::decompress_capped(
            &bytes[header.root_dir_offset as usize
                ..(header.root_dir_offset + header.root_dir_length) as usize],
            Compression::Gzip,
            MAX_INTERNAL_BYTES,
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

    /// `declared_min_zoom` mimics what a real per-band export does under
    /// #380 -- a band's coarse zooms may hold no tiles at all, yet the
    /// archive still *declares* the band's full range in its
    /// `vector_layers[].minzoom` (never in its header, which since
    /// #529/#522 always reflects the tiles actually written). Pass `None`
    /// when the test has no such gap.
    fn write_band_with_payload(
        path: &Path,
        layer: &str,
        tiles: &[(u8, u32, u32)],
        payload: &[u8],
        declared_min_zoom: Option<u8>,
    ) {
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name(layer);
        w.set_bounds(&TileBounds::new(-1.0, -1.0, 1.0, 1.0));
        w.set_fields(HashMap::from([(
            format!("{layer}_f"),
            "Number".to_string(),
        )]));
        if let Some(z) = declared_min_zoom {
            w.set_declared_min_zoom(z);
        }
        for (z, x, y) in tiles {
            w.add_tile(*z, *x, *y, payload).unwrap();
        }
        w.finalize(path).unwrap();
    }

    /// A tile's `(z, x, y)` and its uncompressed payload.
    type BandTile = ((u8, u32, u32), Vec<u8>);

    /// Like [`write_band_with_payload`], but each tile carries its own
    /// payload.
    fn write_band_tiles(path: &Path, layer: &str, tiles: &[BandTile]) {
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name(layer);
        w.set_bounds(&TileBounds::new(-1.0, -1.0, 1.0, 1.0));
        for ((z, x, y), payload) in tiles {
            w.add_tile(*z, *x, *y, payload).unwrap();
        }
        w.finalize(path).unwrap();
    }

    /// #403: the shared-zoom merge hands the writer one dedup keyspace. A
    /// tile copied through was keyed on its compressed bytes but a combined
    /// tile on its uncompressed bytes, so two ids whose stored bodies are
    /// byte-identical — a combined `a‖b` and a band's own `a‖b` tile — never
    /// met in the dedup cache, and the merged archive stored the body twice.
    #[test]
    fn combined_and_copied_tiles_share_one_dedup_keyspace() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, out) = (
            dir.path().join("a.pmtiles"),
            dir.path().join("b.pmtiles"),
            dir.path().join("out.pmtiles"),
        );
        let both = [layer_tile("a"), layer_tile("b")].concat();
        write_band_tiles(&a, "a", &[((0, 0, 0), layer_tile("a"))]);
        write_band_tiles(
            &b,
            "b",
            &[((0, 0, 0), layer_tile("b")), ((1, 0, 0), both.clone())],
        );
        let bands = [
            Band::parse(&format!("0-1:{}:a", a.display())).unwrap(),
            Band::parse(&format!("0-1:{}:b", b.display())).unwrap(),
        ];
        let report = merge_bands_with_options(
            &bands,
            &out,
            &BandMergeOptions {
                allow_missing_zooms: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.total_tiles, 2);
        assert_eq!(merged_layer_names(&out, 0, 0, 0), ["a", "b"], "combined");
        assert_eq!(merged_layer_names(&out, 1, 0, 0), ["a", "b"], "copied");

        let ranges: Vec<_> = ArchiveIndex::open(&out)
            .unwrap()
            .tiles()
            .map(|t| t.unwrap().range)
            .collect();
        assert_eq!(
            ranges[0], ranges[1],
            "the combined 0/0/0 and the copied 1/0/0 are the same bytes and must be stored once"
        );
    }

    /// Decode a merged tile's layer names in order.
    fn merged_layer_names(archive: &Path, z: u8, x: u32, y: u32) -> Vec<String> {
        use prost::Message;
        let a = BandArchive::open(archive, "").unwrap();
        let mut found = None;
        a.for_each_tile(|tz, tx, ty, data| {
            if (tz, tx, ty) == (z, x, y) {
                found = Some(
                    compression::decompress_capped(data, Compression::Gzip, MAX_TILE_BYTES)
                        .unwrap(),
                );
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
    /// generalized to nothing holds tiles only at z3. `vector_layers[].minzoom`
    /// still declares z0 (checked elsewhere), but the merged PMTiles header
    /// must stay the zoom that actually holds a tile (#529, #522) — on both
    /// the single-pass and the shared-zoom path — or `go-pmtiles verify`
    /// rejects the archive.
    #[test]
    fn merge_header_is_actual_tiles_even_when_the_bands_declare_coarser_zooms() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        // Both archives declare a min zoom below their coarsest actual tile
        // (#380), exactly as a real per-band export would: band `a` covers
        // z0-3, band `b` (below) covers z1-3, so their archives declare
        // z0 and z1 respectively even though both hold tiles only at z3.
        write_band_with_payload(&a, "2024", &[(3, 1, 1)], &layer_tile("2024"), Some(0));
        write_band_with_payload(&b, "2025", &[(3, 2, 2)], &layer_tile("2025"), Some(1));

        // Single band, single-pass path.
        let out = dir.path().join("one.pmtiles");
        merge_bands(
            &[Band::parse(&format!("0-3:{}:2024", a.display())).unwrap()],
            &out,
        )
        .unwrap();
        let h = Header::from_bytes(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!((h.min_zoom, h.max_zoom), (3, 3), "single-pass header");
        crate::archive_index::assert_header_zooms_match_tiles(&out);
        // The band's declared range still reaches `vector_layers` through
        // its `LayerMeta` (no `set_declared_*` call needed).
        let meta: Value = serde_json::from_str(&read_metadata(&out)).unwrap();
        assert_eq!(meta["vector_layers"][0]["minzoom"], 0, "{meta}");
        assert_eq!(meta["vector_layers"][0]["maxzoom"], 3, "{meta}");

        // Two layers over shared zooms, two-phase path; both only hold a tile
        // at z3, so the merged header is (3, 3) regardless of either band's
        // declared range.
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
        assert_eq!((h.min_zoom, h.max_zoom), (3, 3), "two-phase header");
        crate::archive_index::assert_header_zooms_match_tiles(&out);
        let meta: Value = serde_json::from_str(&read_metadata(&out)).unwrap();
        let zooms: Vec<(String, u64, u64)> = meta["vector_layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l["id"].as_str().unwrap().to_string(),
                    l["minzoom"].as_u64().unwrap(),
                    l["maxzoom"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            zooms,
            vec![("2024".into(), 2, 3), ("2025".into(), 1, 3)],
            "each band's declared range reaches vector_layers"
        );
    }

    /// #385: at a zoom two bands share, a tile both wrote carries both layers
    /// (the MVT layer messages concatenated, in band order); a tile only one
    /// band wrote passes through untouched; `vector_layers` lists both.
    #[test]
    fn merge_concatenates_layers_of_bands_sharing_a_zoom() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        let b = dir.path().join("b.pmtiles");
        write_band_with_payload(
            &a,
            "2024",
            &[(3, 1, 1), (3, 2, 2)],
            &layer_tile("2024"),
            None,
        );
        write_band_with_payload(
            &b,
            "2025",
            &[(3, 1, 1), (3, 3, 3)],
            &layer_tile("2025"),
            None,
        );

        // Both archives hold tiles only at z3, so both bands declare exactly
        // that -- the zoom range is not this test's concern.
        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[
                Band::parse(&format!("3-3:{}:2024", a.display())).unwrap(),
                Band::parse(&format!("3-3:{}:2025", b.display())).unwrap(),
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
        let root = compression::decompress_capped(
            &bytes[h.root_dir_offset as usize..(h.root_dir_offset + h.root_dir_length) as usize],
            h.internal_compression,
            MAX_INTERNAL_BYTES,
        )
        .unwrap();
        let entries: Vec<(u64, u64, u32)> = decode_directory(&root)
            .unwrap()
            .iter()
            .filter(|e| e.run_length > 0)
            .map(|e| (e.tile_id, e.offset, e.length))
            .collect();
        assert_eq!(entries.len(), 4, "{entries:?}");
        assert!(
            offsets_are_clustered(
                entries
                    .iter()
                    .map(|&(_, offset, length)| (offset, u64::from(length)))
            ),
            "out-of-order entry in clustered archive: {entries:?}"
        );
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
        write_band_with_payload(&a, "fields", &[(3, 1, 1)], &layer_tile("fields"), None);
        write_band_with_payload(&b, "fields", &[(3, 1, 1)], &layer_tile("fields"), None);

        // Both archives hold tiles only at z3; the bands declare exactly
        // that so the zoom-range contract (#495) never fires here -- this
        // test is about the layer-label mismatch, not the zoom range.
        let out = dir.path().join("merged.pmtiles");
        let err = merge_bands(
            &[
                Band::parse(&format!("3-3:{}:2024", a.display())).unwrap(),
                Band::parse(&format!("3-3:{}:2025", b.display())).unwrap(),
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

    /// Like [`write_band`], but with the header's min/max zoom bytes
    /// (100/101) patched to declare a wider range than the tiles actually
    /// written -- what an externally produced archive might do (both `None`
    /// leaves the header honest).
    ///
    /// Since #529/#522 `StreamingPmtilesWriter` never widens its own header
    /// past the tiles it wrote (`go-pmtiles verify` rejects that);
    /// `set_declared_min_zoom`/`set_declared_max_zoom` widen only
    /// `vector_layers` -- see [`write_band_declared_in_metadata`] for that
    /// tylertoo-produced shape. `RangeFit` (#514) still has to classify a
    /// genuinely widened header correctly, so this patches the raw bytes an
    /// external writer could have stamped, the same way
    /// `empty_archive_with_a_real_declaration_stays_disjoint_under_the_flag`
    /// does.
    fn write_band_declared_range(
        path: &Path,
        layer: &str,
        tiles: &[(u8, u32, u32)],
        bounds: TileBounds,
        declared_min_zoom: Option<u8>,
        declared_max_zoom: Option<u8>,
    ) {
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name(layer);
        w.set_bounds(&bounds);
        for (z, x, y) in tiles {
            w.add_tile(*z, *x, *y, &[0x1a, 0x02, 0x08, 0x01]).unwrap();
        }
        w.finalize(path).unwrap();

        if declared_min_zoom.is_some() || declared_max_zoom.is_some() {
            let mut bytes = std::fs::read(path).unwrap();
            if let Some(z) = declared_min_zoom {
                bytes[100] = z;
            }
            if let Some(z) = declared_max_zoom {
                bytes[101] = z;
            }
            std::fs::write(path, bytes).unwrap();
        }
    }

    /// Like [`write_band`], but declaring a min and/or max zoom beyond the
    /// tiles actually written through the real writer API
    /// (`set_declared_min_zoom`/`set_declared_max_zoom`) -- exactly what a
    /// tylertoo per-band export produces under #380 (`tiles --min-zoom 0`
    /// over tiles that start deeper). Since #529/#522 the declaration lands
    /// only in `vector_layers`; the header stays the tiles written.
    fn write_band_declared_in_metadata(
        path: &Path,
        layer: &str,
        tiles: &[(u8, u32, u32)],
        bounds: TileBounds,
        declared_min_zoom: Option<u8>,
        declared_max_zoom: Option<u8>,
    ) {
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name(layer);
        w.set_bounds(&bounds);
        if let Some(z) = declared_min_zoom {
            w.set_declared_min_zoom(z);
        }
        if let Some(z) = declared_max_zoom {
            w.set_declared_max_zoom(z);
        }
        for (z, x, y) in tiles {
            w.add_tile(*z, *x, *y, &[0x1a, 0x02, 0x08, 0x01]).unwrap();
        }
        w.finalize(path).unwrap();
    }

    fn read_metadata(path: &Path) -> String {
        let bytes = std::fs::read(path).unwrap();
        let h = Header::from_bytes(&bytes).unwrap();
        let raw = &bytes[h.json_metadata_offset as usize
            ..(h.json_metadata_offset + h.json_metadata_length) as usize];
        let plain = compression::decompress_capped(raw, h.internal_compression, MAX_INTERNAL_BYTES)
            .unwrap();
        String::from_utf8(plain).unwrap()
    }

    /// The merge never called `set_bounds`, so the header carried
    /// `TileBounds::empty()` -- infinities saturating to min 214.7 / max -214.7 --
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

        // Each band declares exactly the one zoom its archive holds -- this
        // test is about bounds, not the zoom-range contract (#495).
        let out = dir.path().join("merged.pmtiles");
        merge_bands(
            &[
                Band::parse(&format!("2-2:{}:agg", a.display())).unwrap(),
                Band::parse(&format!("6-6:{}:pts", b.display())).unwrap(),
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

    // ---- band zoom-range contract (#495, reworked for #514) ---------------

    /// A band whose declared range shares no zoom at all with what its
    /// archive *declares* is always an error -- there is no interpretation,
    /// and no flag, under which it could write anything. The message now
    /// says "declares", not "holds": #514 stopped treating the header as a
    /// fact about actual content.
    #[test]
    fn band_zoom_range_disjoint_from_archive_errors() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("archive.pmtiles");
        write_band(
            &src,
            "agg",
            &[(3, 1, 1), (4, 2, 2), (5, 4, 4)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let out = dir.path().join("merged.pmtiles");
        let err = merge_bands(
            &[Band::parse(&format!("8-10:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("z8-10") && msg.contains("z3-5") && msg.contains("declares"),
            "{msg}"
        );
        assert!(!out.exists(), "nothing should be written on a bad plan");
    }

    /// #514 S2: the false reject. A band that declares a wider range than
    /// its archive's header (honest here -- the header matches the actual
    /// tiles exactly) used to hard-error, on the theory that the extra zooms
    /// would render silently empty. But the band's range fully *covers* the
    /// archive's actual tiles: nothing real falls outside it, so nothing is
    /// silently lost. This is now accepted -- no error, and no flag needed --
    /// because #514 classifies against the archive's actual content, not its
    /// declared header.
    #[test]
    fn band_wider_than_archive_actual_range_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("archive.pmtiles");
        // Honest header: no declared min/max beyond the tiles actually
        // written, so header == actual == z3-5.
        write_band(
            &src,
            "agg",
            &[(3, 1, 1), (4, 2, 2), (5, 4, 4)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        // Declares z2-5; the archive only holds z3-5, but the band still
        // covers every actual tile the archive has.
        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[Band::parse(&format!("2-5:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap();
        assert_eq!(report.total_tiles, 3, "every tile the archive actually has");
        assert_eq!(report.skipped, 0, "nothing outside the band's range exists");
        assert!(out.exists());
    }

    /// #514 S1: the false accept, the core defect. A header widened over an
    /// empty span (#380) can make a band that shares no zoom with the
    /// archive's *actual* tiles look like an ordinary subrange of the
    /// *declared* header -- and the old header-keyed check accepted it
    /// silently, contributing zero tiles while the merged metadata still
    /// declared the band's full range. #514 classifies against actual
    /// content first: this is now [`RangeFit::DeclaredButEmpty`], which
    /// still proceeds (the header's own declaration says this span may
    /// legitimately be empty) but always warns.
    #[test]
    fn band_disjoint_from_actual_but_within_declared_header_warns() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("archive.pmtiles");
        // Widened header: declares z0-6, but the only tiles ever written are
        // z5-6 -- exactly #380's "coarse zooms generalized to nothing".
        write_band_declared_range(
            &src,
            "coarse",
            &[(5, 1, 1), (6, 2, 2)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            Some(0),
            Some(6),
        );

        // Declares z0-4: a legitimate subrange of the *declared* z0-6
        // header, but shares no zoom with the archive's actual z5-6 tiles.
        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[Band::parse(&format!("0-4:{}:coarse", src.display())).unwrap()],
            &out,
        )
        .unwrap();
        assert_eq!(report.total_tiles, 0, "this band contributes nothing");
        assert_eq!(
            report.skipped, 2,
            "both of the archive's tiles are outside the band's declared range"
        );
        assert!(out.exists(), "a declared-but-empty span is not an error");
    }

    /// #554 review S2-1: the same #514 case, but for an archive tylertoo
    /// itself produced -- `tiles --min-zoom 0` over tiles at z5-14, used as
    /// `--band 0-4:a.pmtiles:roads`. Since #529/#522 the writer keeps that
    /// declaration out of the header (z5-14, the tiles written) and puts it
    /// only in `vector_layers[].minzoom`. A classifier reading the header
    /// alone called this `Disjoint` -- a hard error claiming "that archive
    /// declares z5-14" -- instead of #514's documented `DeclaredButEmpty`
    /// warning. Built with the real writer API, not a byte patch.
    #[test]
    fn band_within_vector_layers_declaration_of_a_tylertoo_archive_warns() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.pmtiles");
        write_band_declared_in_metadata(
            &src,
            "roads",
            &[(5, 1, 1), (14, 2, 2)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            Some(0),
            None,
        );
        let h = Header::from_bytes(&std::fs::read(&src).unwrap()).unwrap();
        assert_eq!(
            (h.min_zoom, h.max_zoom),
            (5, 14),
            "the header is honest: the declaration lives only in vector_layers"
        );
        crate::archive_index::assert_header_zooms_match_tiles(&src);

        let band = Band::parse(&format!("0-4:{}:roads", src.display())).unwrap();
        let archive = BandArchive::open(&src, "roads").unwrap();
        assert_eq!(archive.declared_zoom_range(true), (0, 14));
        assert_eq!(
            check_band_zoom_range(&band, &archive, false).unwrap(),
            RangeFit::DeclaredButEmpty
        );

        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(&[band], &out).unwrap();
        assert_eq!(report.total_tiles, 0, "this band contributes nothing");
        assert!(out.exists(), "a declared-but-empty span is not an error");

        // Outside even the vector_layers declaration is still `Disjoint`.
        let far = dir.path().join("far.pmtiles");
        write_band_declared_in_metadata(
            &far,
            "roads",
            &[(5, 1, 1), (8, 2, 2)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            Some(3),
            None,
        );
        let band = Band::parse(&format!("0-2:{}:roads", far.display())).unwrap();
        let archive = BandArchive::open(&far, "roads").unwrap();
        let err = check_band_zoom_range(&band, &archive, true).unwrap_err();
        assert!(err.to_string().contains("declares z3-8"), "{err}");
    }

    /// A band that overlaps the archive's actual tiles but does not cover
    /// all of them -- some real, already-tiled zoom would fall outside the
    /// band's range and be dropped -- is an error by default: this is the
    /// genuine "missing zoom" case #495 exists to catch.
    #[test]
    fn band_partially_overlapping_archive_actual_range_errors() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("archive.pmtiles");
        write_band(
            &src,
            "agg",
            &[(3, 1, 1), (4, 2, 2), (5, 4, 4), (6, 8, 8)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        // Declares z0-4: overlaps the archive's actual z3-6 at z3-4, but
        // z5-6 -- real tiles -- fall outside the band and would be dropped.
        let out = dir.path().join("merged.pmtiles");
        let err = merge_bands(
            &[Band::parse(&format!("0-4:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("z0-4") && msg.contains("z3-6") && msg.contains("z5-6"),
            "{msg}"
        );
        assert!(!out.exists(), "nothing should be written on a bad plan");
    }

    /// The same partial overlap as above, but with `allow_missing_zooms`
    /// set: the build proceeds, the dropped zooms are only a warning, and
    /// the archive is written with whatever falls inside the band's range.
    #[test]
    fn band_partial_overlap_allowed_with_flag_warns() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("archive.pmtiles");
        write_band(
            &src,
            "agg",
            &[(3, 1, 1), (4, 2, 2), (5, 4, 4), (6, 8, 8)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands_with_options(
            &[Band::parse(&format!("0-4:{}:agg", src.display())).unwrap()],
            &out,
            &BandMergeOptions {
                allow_missing_zooms: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.total_tiles, 2, "only z3 and z4 fall inside the band");
        assert_eq!(report.skipped, 2, "z5 and z6 were dropped");
        assert!(out.exists());
    }

    /// A band declaring a strict subrange of what its archive actually holds
    /// is fully supported and unremarkable -- the documented way to split
    /// one pre-tiled archive across several bands. No error, and the merge
    /// proceeds normally; tiles outside the declared subrange are still
    /// dropped and counted in `skipped`, just without the old "different
    /// range" framing.
    #[test]
    fn band_subrange_of_archive_is_accepted_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("archive.pmtiles");
        write_band(
            &src,
            "agg",
            &[(3, 1, 1), (4, 2, 2), (5, 4, 4)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        // z3-4 is a strict subrange of the archive's actual z3-5.
        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[Band::parse(&format!("3-4:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap();
        assert_eq!(
            report.total_tiles, 2,
            "z5 tile outside the declared subrange"
        );
        assert_eq!(report.skipped, 1);
    }

    /// #514 issue #2: a zero-tile archive used to finalize as header z0-0,
    /// which produced an inconsistent, half-unbypassable classification --
    /// `Overshoot` (flag-clearable) for a band spanning z0, `Disjoint`
    /// (always an error, message falsely claiming "that archive holds z0")
    /// for a band starting above z0. Both are now the same
    /// [`RangeFit::EmptyArchive`]: a dedicated "holds no tiles" message,
    /// bypassable with `--allow-missing-zooms` regardless of whether the
    /// band's range happens to include z0.
    #[test]
    fn empty_archive_is_classified_consistently_and_bypassable() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("empty.pmtiles");
        write_band(&src, "agg", &[], TileBounds::new(-1.0, -1.0, 1.0, 1.0), &[]);

        // A band starting above z0 used to hard-error as `Disjoint`, with no
        // way to opt past it.
        let out = dir.path().join("merged.pmtiles");
        let err = merge_bands(
            &[Band::parse(&format!("3-7:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no tiles"), "{msg}");
        assert!(!out.exists());

        // With the flag, it is bypassable like any other `Overshoot`.
        let report = merge_bands_with_options(
            &[Band::parse(&format!("3-7:{}:agg", src.display())).unwrap()],
            &out,
            &BandMergeOptions {
                allow_missing_zooms: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(report.total_tiles, 0);
        assert!(out.exists());
    }

    /// The [`RangeFit`] matrix, straight off the pure classifier -- the
    /// value the merge loop now keys its skipped-tile logging on
    /// ([`log_band_skip`]), so a misclassification here is what would let a
    /// band that contributed nothing be reported as an ordinary subrange.
    #[test]
    fn range_fit_classifies_each_case() {
        let band = |lo: u8, hi: u8| Band {
            input: PathBuf::from("x.pmtiles"),
            layer: "agg".to_string(),
            min_zoom: lo,
            max_zoom: hi,
        };
        let fit = band_archive_range_fit;

        // No tiles, no usable declaration (z0-0 is what the writer emits for
        // a zero-tile archive whatever it was told): bypassable.
        assert_eq!(fit(&band(3, 7), (0, 0), None), RangeFit::EmptyArchive);
        // No tiles, but a real declaration the band falls inside.
        assert_eq!(fit(&band(3, 7), (0, 10), None), RangeFit::EmptyArchive);
        // No tiles and a real declaration the band falls entirely outside:
        // impossible, and no flag may bypass it (#527 review, finding 1).
        assert_eq!(fit(&band(0, 2), (8, 10), None), RangeFit::Disjoint);

        // Shares no zoom with the actual tiles, but does with the header.
        assert_eq!(
            fit(&band(0, 2), (0, 10), Some((5, 6))),
            RangeFit::DeclaredButEmpty
        );
        // Shares no zoom with either.
        assert_eq!(fit(&band(0, 2), (5, 10), Some((5, 6))), RangeFit::Disjoint);
        // Overlaps real tiles despite a header that excludes them: it can
        // write something, so never `Disjoint`.
        assert_eq!(fit(&band(5, 5), (0, 1), Some((5, 5))), RangeFit::Subrange);

        // Containment either way is a subrange; partial overlap overshoots.
        assert_eq!(fit(&band(0, 5), (0, 13), Some((0, 13))), RangeFit::Subrange);
        assert_eq!(fit(&band(0, 13), (3, 6), Some((3, 6))), RangeFit::Subrange);
        assert_eq!(fit(&band(0, 4), (3, 6), Some((3, 6))), RangeFit::Overshoot);
    }

    /// #527 review, finding 1: the empty-archive short-circuit ran *before*
    /// declared-disjointness was considered, so a band sharing no zoom with
    /// even what the archive declares was classified
    /// [`RangeFit::EmptyArchive`] -- and `--allow-missing-zooms` then waved
    /// through a range [`check_band_zoom_range`] documents as impossible
    /// under every flag. Disjointness wins now, so this stays a hard error
    /// with the flag set.
    ///
    /// The header is patched by hand because tylertoo's own writer cannot
    /// produce this: a zero-tile archive finalizes as z0-0 whatever
    /// `set_declared_min_zoom` was told (see
    /// [`empty_archive_is_classified_consistently_and_bypassable`], which
    /// pins the z0-0 case to `EmptyArchive`). An externally produced archive
    /// can declare a range it does not fill, and that declaration is real.
    #[test]
    fn empty_archive_with_a_real_declaration_stays_disjoint_under_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("empty.pmtiles");
        write_band(&src, "agg", &[], TileBounds::new(-1.0, -1.0, 1.0, 1.0), &[]);
        // Header bytes 100/101 are min_zoom/max_zoom (PMTiles v3, no
        // checksum over them): declare z8-10 while holding nothing.
        let mut bytes = std::fs::read(&src).unwrap();
        bytes[100] = 8;
        bytes[101] = 10;
        std::fs::write(&src, &bytes).unwrap();

        let band = Band::parse(&format!("0-2:{}:agg", src.display())).unwrap();
        let out = dir.path().join("merged.pmtiles");

        // Without the flag: an error, as before.
        let err = merge_bands(std::slice::from_ref(&band), &out).unwrap_err();
        assert!(err.to_string().contains("z8-10"), "{err}");

        // With the flag: still an error. This is the regression -- it used
        // to succeed, writing an archive whose only band could never have
        // contributed a tile.
        let err = merge_bands_with_options(
            std::slice::from_ref(&band),
            &out,
            &BandMergeOptions {
                allow_missing_zooms: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("z0-2") && msg.contains("z8-10") && msg.contains("no zoom"),
            "expected the disjoint error, got: {msg}"
        );
        assert!(
            !out.exists(),
            "nothing should be written on an impossible plan"
        );
    }

    /// A band overlapping the archive's *actual* tiles is never `Disjoint`,
    /// even when a lying header declares a range those tiles fall outside:
    /// it can demonstrably write something, so the "there is no zoom this
    /// band could write" verdict would be false. Guards the ordering inside
    /// [`band_archive_range_fit`] against being "simplified" to check the
    /// declaration first.
    #[test]
    fn a_band_over_real_tiles_is_never_disjoint_despite_a_lying_header() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("liar.pmtiles");
        write_band(
            &src,
            "agg",
            &[(5, 4, 4)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );
        // Declare z0-1 while actually holding z5.
        let mut bytes = std::fs::read(&src).unwrap();
        bytes[100] = 0;
        bytes[101] = 1;
        std::fs::write(&src, &bytes).unwrap();

        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[Band::parse(&format!("5-5:{}:agg", src.display())).unwrap()],
            &out,
        )
        .unwrap();
        assert_eq!(report.total_tiles, 1, "the real z5 tile is written");
    }

    /// #514 issue #3: `--allow-missing-zooms` lets a band declare deeper
    /// zooms than its archive actually has. `vector_layers[].maxzoom` uses
    /// the band's declared value (mirroring #380's minimum), but the merged
    /// PMTiles header's own `max_zoom` stays pinned to the deepest tile
    /// actually copied (#529, #522) -- `go-pmtiles verify` rejects a header
    /// claiming a zoom the directory does not address, so the two are
    /// expected to disagree here, not to agree.
    #[test]
    fn merge_vector_layers_declares_the_bands_max_zoom_but_header_is_actual() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("archive.pmtiles");
        // Actual tiles only reach z6; the band declares z3-10.
        write_band(
            &src,
            "agg",
            &[(3, 1, 1), (4, 2, 2), (5, 4, 4), (6, 8, 8)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let out = dir.path().join("merged.pmtiles");
        merge_bands_with_options(
            &[Band::parse(&format!("4-10:{}:agg", src.display())).unwrap()],
            &out,
            &BandMergeOptions {
                allow_missing_zooms: true,
                ..Default::default()
            }, // z3 falls outside the band -- a genuine `Overshoot`.
        )
        .unwrap();

        let h = Header::from_bytes(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(
            h.max_zoom, 6,
            "header must be the deepest zoom actually copied (z3 was dropped as \
             out-of-band, z4-z6 were copied), never the band's declared z10"
        );

        let meta: Value = serde_json::from_str(&read_metadata(&out)).unwrap();
        assert_eq!(
            meta["vector_layers"][0]["maxzoom"], 10,
            "vector_layers still advertises the band's declared maximum"
        );
        crate::archive_index::assert_header_zooms_match_tiles(&out);
    }

    /// A band archive with no usable bounds used to vanish from the merged
    /// union without a trace -- only a global warning fired, and only when
    /// EVERY band lacked bounds. Now the report names which band(s) were
    /// dropped, and the union still reflects every band that did carry
    /// bounds.
    #[test]
    fn band_without_usable_bounds_is_reported() {
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
        // No set_bounds call: the writer's default TileBounds::empty()
        // serializes to a header that reads back min > max -- unusable.
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name("pts");
        w.add_tile(6, 30, 30, &[0x1a, 0x02, 0x08, 0x01]).unwrap();
        w.finalize(&b).unwrap();

        let out = dir.path().join("merged.pmtiles");
        let report = merge_bands(
            &[
                Band::parse(&format!("2-2:{}:agg", a.display())).unwrap(),
                Band::parse(&format!("6-6:{}:pts", b.display())).unwrap(),
            ],
            &out,
        )
        .unwrap();

        assert_eq!(
            report.bands_without_bounds.len(),
            1,
            "{:?}",
            report.bands_without_bounds
        );
        assert!(
            report.bands_without_bounds[0].contains("pts"),
            "{:?}",
            report.bands_without_bounds
        );

        // The union still reflects the one band that did carry bounds.
        let bytes = std::fs::read(&out).unwrap();
        let h = Header::from_bytes(&bytes).unwrap();
        assert!((h.min_lon + 10.0).abs() < 1e-6, "min_lon {}", h.min_lon);
        assert!((h.max_lon - 0.0).abs() < 1e-6, "max_lon {}", h.max_lon);
    }

    /// A corrupt `json_metadata_length` used to index the file slice unchecked
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

    // ---- Hostile band archives (#417) --------------------------------------
    //
    // `merge_bands` takes archives straight from a caller — the third-party
    // input path #417 names — so the resource ceilings have to hold on the
    // pyramid reader, not just on `decode`.

    /// Append hand-built root and leaf directories to a valid band archive
    /// and repoint the header at them. Every other section stays where it
    /// was, so only the directory content is hostile.
    fn rewrite_directories(path: &Path, root: &[DirEntry], leaf: &[DirEntry]) {
        use crate::pmtiles_writer::encode_directory;

        let mut bytes = std::fs::read(path).unwrap();
        let mut header = Header::from_bytes(&bytes).unwrap();

        let leaf_enc =
            compression::compress(&encode_directory(leaf), header.internal_compression).unwrap();
        header.leaf_dirs_offset = bytes.len() as u64;
        header.leaf_dirs_length = leaf_enc.len() as u64;
        bytes.extend_from_slice(&leaf_enc);

        let root_enc =
            compression::compress(&encode_directory(root), header.internal_compression).unwrap();
        header.root_dir_offset = bytes.len() as u64;
        header.root_dir_length = root_enc.len() as u64;
        bytes.extend_from_slice(&root_enc);

        bytes[..127].copy_from_slice(&header.to_bytes());
        std::fs::write(path, &bytes).unwrap();
    }

    /// A z3 band archive whose sections are otherwise sound.
    fn hostile_band(dir: &tempfile::TempDir) -> PathBuf {
        let path = dir.path().join("band.pmtiles");
        write_band(
            &path,
            "agg",
            &[(3, 1, 1)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );
        path
    }

    /// S1-1, the big-leaf half: a root full of pointers at *one* leaf body.
    /// Each pointer costs ~5 bytes on disk and yields a whole leaf's worth of
    /// entries, so a ~50 KB file used to accumulate entries until the process
    /// died. The entry budget is spent before the entries are kept.
    #[test]
    fn hostile_leaf_flattening_is_capped_in_the_pyramid_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = hostile_band(&dir);

        // z3's whole address space is 85 tile ids, so 4 x 60 entries is
        // already past anything this archive could address.
        let leaf: Vec<DirEntry> = (0..60)
            .map(|i| DirEntry {
                tile_id: i + 1,
                offset: 0,
                length: 4,
                run_length: 1,
            })
            .collect();
        let leaf_len = compression::compress(
            &crate::pmtiles_writer::encode_directory(&leaf),
            Compression::Gzip,
        )
        .unwrap()
        .len() as u32;
        let root: Vec<DirEntry> = (0..4)
            .map(|_| DirEntry {
                tile_id: 0,
                offset: 0,
                length: leaf_len,
                run_length: 0,
            })
            .collect();
        rewrite_directories(&path, &root, &leaf);

        let err = match BandArchive::open(&path, "") {
            Ok(_) => panic!("a hostile directory must not open cleanly"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("directory entries") && err.contains("limit"),
            "expected an entry-budget error, got: {err}"
        );
    }

    /// S1-1, the many-leaves half: an *empty* leaf costs nothing against the
    /// entry budget but still costs a bounded decompression apiece, so the
    /// number of leaves visited is capped separately.
    #[test]
    fn hostile_leaf_count_is_capped_in_the_pyramid_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = hostile_band(&dir);

        let leaf: Vec<DirEntry> = Vec::new();
        let leaf_len = compression::compress(
            &crate::pmtiles_writer::encode_directory(&leaf),
            Compression::Gzip,
        )
        .unwrap()
        .len() as u32;
        let root: Vec<DirEntry> = (0..=MAX_LEAF_DIRECTORIES)
            .map(|_| DirEntry {
                tile_id: 0,
                offset: 0,
                length: leaf_len,
                run_length: 0,
            })
            .collect();
        rewrite_directories(&path, &root, &leaf);

        let err = match BandArchive::open(&path, "") {
            Ok(_) => panic!("a hostile directory must not open cleanly"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("leaf directories") && err.contains(&MAX_LEAF_DIRECTORIES.to_string()),
            "expected a leaf-count error, got: {err}"
        );
    }

    /// #403: the shared-zoom merge streams the bands' directories through a
    /// k-way merge and opts its writer into #506's ascending-id assertion, so
    /// a band whose directory goes backwards (here, a run that overlaps the
    /// next entry: 21, 22, 23, then 22 again) must be refused as an error —
    /// not reach the writer, where it would trip that `debug_assert!`.
    #[test]
    fn shared_zoom_merge_refuses_a_band_directory_out_of_id_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = hostile_band(&dir);
        let length = ArchiveIndex::open(&path).unwrap().entries()[0].length;
        let z3_first = crate::pmtiles_writer::tile_id(3, 0, 0);
        let entry = |tile_id, run_length| DirEntry {
            tile_id,
            offset: 0,
            length,
            run_length,
        };
        rewrite_directories(&path, &[entry(z3_first, 3), entry(z3_first + 1, 1)], &[]);
        let other = dir.path().join("other.pmtiles");
        write_band(
            &other,
            "other",
            &[(3, 0, 0)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let bands = [
            Band::parse(&format!("3-3:{}:agg", path.display())).unwrap(),
            Band::parse(&format!("3-3:{}:other", other.display())).unwrap(),
        ];
        let err = merge_bands(&bands, &dir.path().join("out.pmtiles"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ascending id order") && err.contains("band.pmtiles"),
            "expected an out-of-order directory error naming the band, got: {err}"
        );
    }

    /// A repeat right after its twin (a run ending at 22, then an entry at
    /// 22: ids 21, 22, 22) reaches the shared-zoom gather as two copies of
    /// one id from one band. That is a malformed directory, not two bands
    /// claiming a tile, and must be reported like the disjoint path reports
    /// it — not as "claimed twice (bands band.pmtiles and band.pmtiles)".
    #[test]
    fn shared_zoom_merge_reports_an_adjacent_repeat_as_out_of_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = hostile_band(&dir);
        let length = ArchiveIndex::open(&path).unwrap().entries()[0].length;
        let z3_first = crate::pmtiles_writer::tile_id(3, 0, 0);
        let entry = |tile_id, run_length| DirEntry {
            tile_id,
            offset: 0,
            length,
            run_length,
        };
        rewrite_directories(&path, &[entry(z3_first, 2), entry(z3_first + 1, 1)], &[]);
        let other = dir.path().join("other.pmtiles");
        write_band(
            &other,
            "other",
            &[(3, 0, 0)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let bands = [
            Band::parse(&format!("3-3:{}:agg", path.display())).unwrap(),
            Band::parse(&format!("3-3:{}:other", other.display())).unwrap(),
        ];
        let err = merge_bands(&bands, &dir.path().join("out.pmtiles"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ascending id order") && err.contains("band.pmtiles"),
            "expected an out-of-order directory error naming the band, got: {err}"
        );
        assert!(
            !err.contains("claimed twice"),
            "not a cross-band clash: {err}"
        );
    }

    /// A band's directory fails mid-walk (here an entry past the tile-data
    /// section, which only the merge's walk checks). The shared-zoom merge
    /// interleaves every band's directory, so an error that does not name
    /// its band leaves the user guessing which `--band` archive is corrupt.
    #[test]
    fn shared_zoom_merge_names_the_band_whose_directory_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = hostile_band(&dir);
        let header = Header::from_bytes(&std::fs::read(&path).unwrap()).unwrap();
        let root = [DirEntry {
            tile_id: crate::pmtiles_writer::tile_id(3, 1, 1),
            offset: header.tile_data_length, // one past the section's end
            length: 4,
            run_length: 1,
        }];
        rewrite_directories(&path, &root, &[]);
        let other = dir.path().join("other.pmtiles");
        write_band(
            &other,
            "other",
            &[(3, 0, 0)],
            TileBounds::new(-1.0, -1.0, 1.0, 1.0),
            &[],
        );

        let bands = [
            Band::parse(&format!("3-3:{}:other", other.display())).unwrap(),
            Band::parse(&format!("3-3:{}:agg", path.display())).unwrap(),
        ];
        let err = merge_bands(&bands, &dir.path().join("out.pmtiles"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("past end of") && err.contains("band.pmtiles"),
            "expected a past-end error naming the band, got: {err}"
        );
    }

    /// Replace `path`'s directories with a root pointing at `leaves`, in the
    /// order given. The root's own ids ascend (`root_ids`, one per leaf), as
    /// its delta encoding requires; what the leaves hold is up to the test.
    fn rewrite_with_leaves(path: &Path, root_ids: &[u64], leaves: &[Vec<DirEntry>]) {
        use crate::pmtiles_writer::encode_directory;

        let mut bytes = std::fs::read(path).unwrap();
        let mut header = Header::from_bytes(&bytes).unwrap();
        let mut leaf_section = Vec::new();
        let mut root = Vec::new();
        for (id, leaf) in root_ids.iter().zip(leaves) {
            let enc = compression::compress(&encode_directory(leaf), header.internal_compression)
                .unwrap();
            root.push(DirEntry {
                tile_id: *id,
                offset: leaf_section.len() as u64,
                length: enc.len() as u32,
                run_length: 0,
            });
            leaf_section.extend_from_slice(&enc);
        }
        header.leaf_dirs_offset = bytes.len() as u64;
        header.leaf_dirs_length = leaf_section.len() as u64;
        bytes.extend_from_slice(&leaf_section);
        let root_enc =
            compression::compress(&encode_directory(&root), header.internal_compression).unwrap();
        header.root_dir_offset = bytes.len() as u64;
        header.root_dir_length = root_enc.len() as u64;
        bytes.extend_from_slice(&root_enc);
        bytes[..127].copy_from_slice(&header.to_bytes());
        std::fs::write(path, &bytes).unwrap();
    }

    /// The disjoint-zoom path keeps no per-tile map: tile ids must ascend,
    /// which rules out a repeat as well. A run overlapping the next entry
    /// (21, 22, 23, then 22 again) is refused, naming the band.
    #[test]
    fn disjoint_merge_refuses_a_band_directory_repeating_an_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = hostile_band(&dir);
        let length = ArchiveIndex::open(&path).unwrap().entries()[0].length;
        let z3_first = crate::pmtiles_writer::tile_id(3, 0, 0);
        let entry = |tile_id, run_length| DirEntry {
            tile_id,
            offset: 0,
            length,
            run_length,
        };
        rewrite_directories(&path, &[entry(z3_first, 3), entry(z3_first + 1, 1)], &[]);

        let bands = [Band::parse(&format!("3-3:{}:agg", path.display())).unwrap()];
        let err = merge_bands(&bands, &dir.path().join("out.pmtiles"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ascending id order") && err.contains("band.pmtiles"),
            "expected an out-of-order directory error naming the band, got: {err}"
        );
    }

    /// No repeat at all, but the leaves resolve out of order (26, then 21):
    /// the writer would get descending adds, and #506's clustered check is
    /// on for this path too, so the merge must refuse the band instead.
    #[test]
    fn disjoint_merge_refuses_a_band_whose_leaves_go_backwards() {
        let dir = tempfile::tempdir().unwrap();
        let path = hostile_band(&dir);
        let length = ArchiveIndex::open(&path).unwrap().entries()[0].length;
        let z3_first = crate::pmtiles_writer::tile_id(3, 0, 0);
        let entry = |tile_id| DirEntry {
            tile_id,
            offset: 0,
            length,
            run_length: 1,
        };
        rewrite_with_leaves(
            &path,
            &[z3_first, z3_first + 1],
            &[vec![entry(z3_first + 5)], vec![entry(z3_first)]],
        );

        let bands = [Band::parse(&format!("3-3:{}:agg", path.display())).unwrap()];
        let err = merge_bands(&bands, &dir.path().join("out.pmtiles"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("ascending id order") && err.contains("band.pmtiles"),
            "expected an out-of-order directory error naming the band, got: {err}"
        );
    }

    /// An oversized run length through the pyramid reader: every expanded id
    /// becomes a writer directory entry in the merge, so one 0xFFFFFFFF run
    /// is tens of gigabytes. The error names the band it came from.
    #[test]
    fn hostile_run_length_is_capped_in_the_pyramid_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = hostile_band(&dir);
        let root = [DirEntry {
            tile_id: 0,
            offset: 0,
            length: 4,
            run_length: u32::MAX,
        }];
        rewrite_directories(&path, &root, &[]);

        // Opening is fine — run lengths are left intact until expansion —
        // and the merge is where the budget is spent.
        let out = dir.path().join("merged.pmtiles");
        let err = merge_bands(
            &[Band::parse(&format!("3-3:{}:agg", path.display())).unwrap()],
            &out,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("run-length expansion") && err.contains("band.pmtiles"),
            "expected a run-length expansion error naming the band, got: {err}"
        );
    }

    /// A tile entry pointing outside the declared tile-data section would be
    /// handed to the MVT decoder as a tile; the file-bounds check alone would
    /// wave it through.
    #[test]
    fn tile_entry_outside_the_tile_data_section_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = hostile_band(&dir);
        let header = Header::from_bytes(&std::fs::read(&path).unwrap()).unwrap();
        let root = [DirEntry {
            tile_id: 0,
            offset: header.tile_data_length, // one past the section's end
            length: 4,
            run_length: 1,
        }];
        rewrite_directories(&path, &root, &[]);

        let out = dir.path().join("merged.pmtiles");
        let err = merge_bands(
            &[Band::parse(&format!("3-3:{}:agg", path.display())).unwrap()],
            &out,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("past end of") && err.contains("band.pmtiles"),
            "{err}"
        );
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
        assert!(
            offsets_are_clustered(
                entries
                    .iter()
                    .map(|&(_, offset, length)| (offset, u64::from(length)))
            ),
            "out-of-order entry in clustered archive: {entries:?}"
        );
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

    // -------------------------------------------------------------------------
    // Publication metadata (name / description / attribution)
    // -------------------------------------------------------------------------

    use crate::pmtiles_writer::HEADER_BYTES;

    fn write_published_band(
        path: &Path,
        layer: &str,
        tiles: &[(u8, u32, u32)],
        meta: crate::TilesetMetadata,
    ) {
        write_published_payload(path, layer, tiles, &[0x1a, 0x02, 0x08, 0x01], meta);
    }

    fn write_published_payload(
        path: &Path,
        layer: &str,
        tiles: &[(u8, u32, u32)],
        payload: &[u8],
        meta: crate::TilesetMetadata,
    ) {
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name(layer);
        w.set_bounds(&TileBounds::new(-1.0, -1.0, 1.0, 1.0));
        w.set_metadata(meta);
        for (z, x, y) in tiles {
            w.add_tile(*z, *x, *y, payload).unwrap();
        }
        w.finalize(path).unwrap();
    }

    fn publication(path: &Path) -> crate::TilesetMetadata {
        let meta: Value = serde_json::from_str(&read_metadata(path)).unwrap();
        let get = |k: &str| meta.get(k).map(|v| v.as_str().unwrap().to_string());
        crate::TilesetMetadata {
            name: get("name"),
            description: get("description"),
            attribution: get("attribution"),
        }
    }

    fn band(input: &Path, layer: &str, min_zoom: u8, max_zoom: u8) -> Band {
        Band {
            input: input.to_path_buf(),
            layer: layer.to_string(),
            min_zoom,
            max_zoom,
        }
    }

    fn meta() -> crate::TilesetMetadata {
        crate::TilesetMetadata::new()
    }

    #[test]
    fn oversized_band_metadata_fails_before_touching_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("oversized.pmtiles");
        write_published_band(&input, "parks", &[(3, 0, 0)], meta());
        let mut raw = std::fs::read(&input).unwrap();
        let mut header = Header::from_bytes(&raw).unwrap();
        header.json_metadata_length = MAX_INTERNAL_BYTES + 1;
        raw[..HEADER_BYTES].copy_from_slice(&header.to_bytes());
        std::fs::write(&input, raw).unwrap();

        let existing = dir.path().join("existing.pmtiles");
        std::fs::write(&existing, b"previous archive").unwrap();
        let absent = dir.path().join("absent.pmtiles");
        for output in [&existing, &absent] {
            let err = merge_bands(&[band(&input, "parks", 3, 3)], output).unwrap_err();
            let msg = err.to_string();
            assert!(matches!(err, Error::PMTilesWrite(_)), "{msg}");
            assert!(msg.contains("oversized.pmtiles"), "{msg}");
            assert!(
                msg.contains(&format!("metadata claims {} bytes", MAX_INTERNAL_BYTES + 1)),
                "{msg}"
            );
            assert!(
                msg.contains(&format!("{MAX_INTERNAL_BYTES}-byte ceiling")),
                "{msg}"
            );
        }
        assert_eq!(std::fs::read(&existing).unwrap(), b"previous archive");
        assert!(!absent.exists());
    }

    #[test]
    fn unparseable_band_metadata_keeps_tiles_and_explicit_publication() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("invalid-json.pmtiles");
        let out = dir.path().join("out.pmtiles");
        write_published_band(
            &input,
            "parks",
            &[(3, 0, 0)],
            meta().with_attribution("discarded"),
        );
        let mut raw = std::fs::read(&input).unwrap();
        let mut header = Header::from_bytes(&raw).unwrap();
        let gz = compression::compress(b"{", header.internal_compression).unwrap();
        header.json_metadata_offset = raw.len() as u64;
        header.json_metadata_length = gz.len() as u64;
        raw[..HEADER_BYTES].copy_from_slice(&header.to_bytes());
        raw.extend_from_slice(&gz);
        std::fs::write(&input, raw).unwrap();

        let expected = meta().with_name("Parks").with_attribution("NPS");
        let report = merge_bands_with_options(
            &[band(&input, "parks", 3, 3)],
            &out,
            &BandMergeOptions {
                metadata: expected.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(publication(&out), expected);
        assert_eq!(report.metadata, expected);
        // Reported, since the band may have been meant to carry a credit.
        assert_eq!(
            report.bands_without_metadata,
            vec![format!("parks ({})", input.display())]
        );
        let source = crate::archive_index::ArchiveIndex::open(&input).unwrap();
        let merged = crate::archive_index::ArchiveIndex::open(&out).unwrap();
        let source_tile = source.tiles().next().unwrap().unwrap();
        let merged_tile = merged.tiles().next().unwrap().unwrap();
        assert_eq!((merged_tile.z, merged_tile.x, merged_tile.y), (3, 0, 0));
        assert_eq!(merged.tiles().count(), 1);
        assert_eq!(
            merged.read_range(merged_tile.range).unwrap(),
            source.read_range(source_tile.range).unwrap()
        );
        assert_eq!(
            merged.metadata_json().unwrap()["vector_layers"][0]["fields"],
            json!({})
        );
    }

    #[test]
    fn disjoint_bands_inherit_publication_metadata_in_any_order() {
        let dir = tempfile::tempdir().unwrap();
        let (coarse, fine) = (dir.path().join("c.pmtiles"), dir.path().join("f.pmtiles"));
        write_published_band(
            &coarse,
            "parks",
            &[(0, 0, 0)],
            meta().with_name("Parks").with_attribution("USGS"),
        );
        write_published_band(
            &fine,
            "parks",
            &[(1, 0, 0)],
            meta().with_name("Parks").with_attribution("NPS"),
        );
        let want = meta().with_name("Parks").with_attribution("NPS | USGS");
        let bands = [band(&coarse, "parks", 0, 0), band(&fine, "parks", 1, 1)];
        let (a, b) = (dir.path().join("a.pmtiles"), dir.path().join("b.pmtiles"));
        merge_bands(&bands, &a).unwrap();
        merge_bands(&[bands[1].clone(), bands[0].clone()], &b).unwrap();
        assert_eq!(publication(&a), want);
        assert_eq!(publication(&b), want);
    }

    #[test]
    fn shared_zoom_bands_keep_publication_metadata_and_every_layer() {
        let dir = tempfile::tempdir().unwrap();
        let (roads, parks) = (dir.path().join("r.pmtiles"), dir.path().join("p.pmtiles"));
        write_published_payload(
            &roads,
            "roads",
            &[(1, 0, 0)],
            &layer_tile("roads"),
            meta().with_name("Roads").with_attribution("OSM"),
        );
        write_published_payload(
            &parks,
            "parks",
            &[(1, 0, 0)],
            &layer_tile("parks"),
            meta().with_attribution("NPS").with_description("Parks"),
        );
        let out = dir.path().join("out.pmtiles");
        merge_bands_with_options(
            &[band(&roads, "roads", 1, 1), band(&parks, "parks", 1, 1)],
            &out,
            &BandMergeOptions {
                metadata: meta().with_name("Base map"),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            publication(&out),
            meta()
                .with_name("Base map")
                .with_description("Parks")
                .with_attribution("NPS | OSM")
        );
        assert_eq!(merged_layer_names(&out, 1, 0, 0), vec!["roads", "parks"]);
    }

    /// A name the bands disagree on is left out, not an error: pyramids of
    /// tippecanoe archives, each named after its own file, must keep working.
    #[test]
    fn disagreeing_names_are_left_out_unless_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let (east, west) = (dir.path().join("e.pmtiles"), dir.path().join("w.pmtiles"));
        write_published_band(
            &east,
            "regions",
            &[(0, 0, 0)],
            meta().with_name("East region").with_attribution("East"),
        );
        write_published_band(
            &west,
            "regions",
            &[(1, 0, 0)],
            meta().with_name("West region").with_attribution("West"),
        );
        let bands = [band(&east, "regions", 0, 0), band(&west, "regions", 1, 1)];
        let out = dir.path().join("out.pmtiles");
        let report = merge_bands(&bands, &out).unwrap();
        let want = meta().with_attribution("East | West");
        assert_eq!(publication(&out), want);
        assert_eq!(report.metadata, want);
        assert_eq!(report.metadata_conflicts, vec!["name"]);

        let options = BandMergeOptions {
            metadata: meta().with_name("Combined regions"),
            ..Default::default()
        };
        let report = merge_bands_with_options(&bands, &out, &options).unwrap();
        assert_eq!(
            publication(&out),
            meta()
                .with_name("Combined regions")
                .with_attribution("East | West")
        );
        assert!(report.metadata_conflicts.is_empty());
    }

    #[test]
    fn subrange_band_keeps_its_archive_attribution() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.pmtiles");
        write_published_band(
            &src,
            "parks",
            &[(0, 0, 0), (1, 0, 0), (2, 0, 0)],
            meta().with_attribution("NPS"),
        );
        let out = dir.path().join("out.pmtiles");
        merge_bands(&[band(&src, "parks", 1, 2)], &out).unwrap();
        assert_eq!(publication(&out).attribution.as_deref(), Some("NPS"));
    }

    /// Order of the `build_pyramid` preflight: a never-written source band
    /// listed first would fail with "convert failed" if it were tiled before
    /// the archive bands' credits were checked.
    #[test]
    fn build_pyramid_checks_publication_metadata_before_tiling() {
        let dir = tempfile::tempdir().unwrap();
        let (east, west) = (dir.path().join("e.pmtiles"), dir.path().join("w.pmtiles"));
        // Each fits an archive alone; joined, they cannot.
        let half = MAX_INTERNAL_BYTES as usize / 2;
        write_published_band(
            &east,
            "e",
            &[(8, 0, 0)],
            meta().with_attribution("e".repeat(half)),
        );
        write_published_band(
            &west,
            "w",
            &[(9, 0, 0)],
            meta().with_attribution("w".repeat(half)),
        );
        let missing = dir.path().join("does-not-exist.parquet");
        let out = dir.path().join("out.pmtiles");
        let bands = [
            band(&missing, "fine", 6, 7),
            band(&east, "e", 8, 8),
            band(&west, "w", 9, 9),
        ];
        let err = build_pyramid(&bands, &out, &PyramidOptions::default())
            .unwrap_err()
            .to_string();
        assert!(err.contains("attribution"), "{err}");
        assert!(err.contains("w.pmtiles"), "names the band: {err}");
        assert!(!err.contains("convert failed"), "{err}");
        assert!(!out.exists());
    }

    #[test]
    fn build_pyramid_passes_on_archive_bands_and_refuses_export_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let coarse = dir.path().join("coarse.parquet");
        write_cell_source(&coarse, 40, 0.6);
        let pre = dir.path().join("pre.pmtiles");
        write_published_band(
            &pre,
            "features",
            &[(3, 0, 0)],
            meta().with_name("Cells").with_attribution("Survey"),
        );
        let bands = [
            band(&coarse, "aggregate", 0, 2),
            band(&pre, "features", 3, 3),
        ];
        let opts = |export: crate::TilesetMetadata, last: crate::TilesetMetadata| PyramidOptions {
            work_dir: Some(dir.path().to_path_buf()),
            export: ExportOptions {
                metadata: export,
                ..PyramidOptions::default().export
            },
            metadata: last,
            ..Default::default()
        };
        // #779: `export.metadata` would be ignored (the output's metadata is
        // `PyramidOptions::metadata`), so setting it is refused up front
        // rather than silently dropped.
        let out = dir.path().join("plain.pmtiles");
        let err = build_pyramid(
            &bands,
            &out,
            &opts(meta().with_name("Grid").with_attribution("Census"), meta()),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Config(ConfigError::OptionIgnored {
                    option: "export.metadata",
                    ..
                })
            ),
            "{err:?}"
        );
        assert!(!out.exists());
        // A GeoParquet band passes nothing on; the archive band's values
        // pass through.
        let report = build_pyramid(&bands, &out, &opts(meta(), meta())).unwrap();
        let want = meta().with_name("Cells").with_attribution("Survey");
        assert_eq!(publication(&out), want);
        assert_eq!(report.metadata, want);
        assert!(report.metadata_conflicts.is_empty());
        // Final overrides apply per field.
        let out = dir.path().join("final.pmtiles");
        build_pyramid(
            &bands,
            &out,
            &opts(
                meta(),
                meta()
                    .with_description("All cells")
                    .with_attribution("Everyone"),
            ),
        )
        .unwrap();
        assert_eq!(
            publication(&out),
            meta()
                .with_name("Cells")
                .with_description("All cells")
                .with_attribution("Everyone")
        );
    }

    #[test]
    fn source_only_pyramid_writes_final_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let coarse = dir.path().join("coarse.parquet");
        write_cell_source(&coarse, 40, 0.6);
        let out = dir.path().join("out.pmtiles");
        let last = meta().with_name("Cells").with_attribution("");
        build_pyramid(
            &[band(&coarse, "aggregate", 0, 2)],
            &out,
            &PyramidOptions {
                work_dir: Some(dir.path().to_path_buf()),
                metadata: last.clone(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(publication(&out), last);
    }
}
