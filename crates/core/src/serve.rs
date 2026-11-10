//! `tylertoo serve` (#825): PMTiles archives over HTTP, with a map viewer.
//!
//! Each archive is read through core's [`ArchiveIndex`]: directories
//! resident, tile bodies by positioned read, so a rebuild of the archive
//! while it is served cannot fault the server. The archive is reopened when
//! its size, modification time or inode changes.

use crate::archive_index::ArchiveIndex;
use crate::compression::{
    decompress, decompress_capped, Compression, MAX_INTERNAL_BYTES, MAX_TILE_BYTES,
};
use crate::error::{Classify, ErrorKind};
use crate::pmtiles_writer::TileType;
use crate::stats::{compute_stats, DEFAULT_LARGEST};
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tiny_http::{Header, Method, Request, Response, StatusCode};

const VIEWER: &str = include_str!("viewer.html");

/// Why `serve` could not start or keep listening.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// An archive path has no UTF-8 file stem to name it by.
    #[error("{}: an archive needs a UTF-8 file name", path.display())]
    #[non_exhaustive]
    BadName { path: PathBuf },
    /// Two archives share a file stem, so one URL would name both.
    #[error("two archives are named {name:?}; rename one")]
    #[non_exhaustive]
    DuplicateName { name: String },
    /// `index` would shadow `/index.json`, the archive list.
    #[error("{}: the name \"index\" is reserved; rename it", path.display())]
    #[non_exhaustive]
    ReservedName { path: PathBuf },
    /// An archive did not open.
    #[error("could not open {}: {error}", path.display())]
    #[non_exhaustive]
    Open { path: PathBuf, error: crate::Error },
    /// The address could not be bound.
    #[error("could not listen on {addr}: {error}")]
    #[non_exhaustive]
    Bind {
        addr: String,
        error: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The listening socket failed.
    #[error("the server socket failed: {0}")]
    Io(#[from] std::io::Error),
}

impl Classify for ServeError {
    fn kind(&self) -> ErrorKind {
        match self {
            ServeError::BadName { .. }
            | ServeError::DuplicateName { .. }
            | ServeError::ReservedName { .. } => ErrorKind::Usage,
            ServeError::Open { error, .. } => error.kind(),
            ServeError::Bind { .. } | ServeError::Io(_) => ErrorKind::Runtime,
        }
    }
}

/// What identifies one version of a file on disk: size, mtime, inode.
type Stamp = (u64, Option<SystemTime>, u64);

fn stamp(m: &std::fs::Metadata) -> Stamp {
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(m);
    #[cfg(not(unix))]
    let ino = 0;
    (m.len(), m.modified().ok(), ino)
}

/// A strong `ETag` for one version of the file, so a PMTiles client that
/// cached the archive's directory notices a rebuild.
fn etag((len, mtime, ino): Stamp) -> String {
    let nanos = mtime
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("\"{len:x}-{nanos:x}-{ino:x}\"")
}

struct Served {
    name: String,
    path: PathBuf,
    index: ArchiveIndex,
    stamp: Stamp,
}

impl Served {
    /// Reopen the archive if the file changed. A file caught mid-rewrite
    /// fails to open; the old index keeps serving and the next request
    /// tries again.
    fn refresh(&mut self) {
        let Ok(now) = std::fs::metadata(&self.path).map(|m| stamp(&m)) else {
            return;
        };
        if now == self.stamp {
            return;
        }
        match ArchiveIndex::open(&self.path) {
            Ok(index) => {
                log::info!("reloaded {}", self.path.display());
                warn_variable_depth(&index);
                self.index = index;
                self.stamp = now;
            }
            Err(e) => log::warn!("{} changed but did not reopen: {e}", self.path.display()),
        }
    }
}

// shortcut: a missing tile is a 204, never its nearest stored ancestor
// scaled up, so a variable-depth archive (#803) shows holes below each
// stopped branch. Overzoom here if serving those archives matters.
fn warn_variable_depth(index: &ArchiveIndex) {
    if index.is_variable_depth() {
        log::warn!(
            "{} is a variable-depth archive: serve does not overzoom stopped tiles, \
             so the map shows holes below them",
            index.path().display()
        );
    }
}

/// A bound server: [`Server::bind`], then [`Server::run`].
pub struct Server {
    http: tiny_http::Server,
    archives: Vec<Served>,
}

impl Server {
    /// Open every archive and bind `host:port` (port 0 picks a free port).
    ///
    /// # Errors
    ///
    /// An archive that does not open, two archives with the same file stem,
    /// or an address that cannot be bound.
    pub fn bind(archives: &[PathBuf], host: &str, port: u16) -> Result<Self, ServeError> {
        let mut served: Vec<Served> = Vec::new();
        for path in archives {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| ServeError::BadName { path: path.clone() })?
                .to_owned();
            if served.iter().any(|s| s.name == name) {
                return Err(ServeError::DuplicateName { name });
            }
            if name == "index" {
                return Err(ServeError::ReservedName { path: path.clone() });
            }
            let open = |error| ServeError::Open {
                path: path.clone(),
                error,
            };
            let stamp = std::fs::metadata(path)
                .map(|m| stamp(&m))
                .map_err(|e| open(e.into()))?;
            let index = ArchiveIndex::open(path).map_err(open)?;
            warn_variable_depth(&index);
            served.push(Served {
                name,
                path: path.clone(),
                index,
                stamp,
            });
        }
        let http = tiny_http::Server::http((host, port)).map_err(|error| ServeError::Bind {
            addr: format!("{host}:{port}"),
            error,
        })?;
        Ok(Server {
            http,
            archives: served,
        })
    }

    /// The viewer's URL.
    pub fn url(&self) -> String {
        match self.http.server_addr() {
            tiny_http::ListenAddr::IP(a) => format!("http://{a}/"),
            #[cfg(unix)]
            tiny_http::ListenAddr::Unix(a) => format!("{a:?}"),
        }
    }

    /// Serve until `keep_going` returns false; it is polled at least every
    /// 200 ms, which is how Python's Ctrl-C reaches the loop.
    ///
    /// # Errors
    ///
    /// The listening socket failed.
    pub fn run(mut self, mut keep_going: impl FnMut() -> bool) -> Result<(), ServeError> {
        while keep_going() {
            if let Some(req) = self.http.recv_timeout(Duration::from_millis(200))? {
                let host = req
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Host"))
                    .map(|h| h.value.to_string());
                let res = self
                    .respond(&req, host.as_deref())
                    .with_header(header("Access-Control-Allow-Origin", "*"));
                // Responses are built here (a lookup and a positioned read)
                // and sent on their own thread, so a whole-archive download
                // stalls neither the tiles nor the `keep_going` poll.
                // shortcut: one thread per response, unbounded, as tiny_http
                // already has one per connection; pool them if a shared
                // deployment ever needs a cap.
                std::thread::spawn(move || {
                    // A client that hung up mid-response is not the server's error.
                    let _ = req.respond(res);
                });
            }
        }
        Ok(())
    }

    fn respond(&mut self, req: &Request, host: Option<&str>) -> Response<Box<dyn Read + Send>> {
        match req.method() {
            Method::Get | Method::Head => {}
            // The CORS preflight a browser sends before a Range or
            // conditional request from another origin.
            Method::Options => {
                return status(204)
                    .with_header(header("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS"))
                    .with_header(header(
                        "Access-Control-Allow-Headers",
                        "Range, If-Match, If-Range",
                    ));
            }
            _ => return status(405),
        }
        let path = req.url().split('?').next().unwrap_or("");
        let parts: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .map(|p| percent_decode_str(p).decode_utf8_lossy().into_owned())
            .collect();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        match parts.as_slice() {
            [""] => body(VIEWER.as_bytes().to_vec(), "text/html; charset=utf-8"),
            ["index.json"] => {
                let names: Vec<&str> = self.archives.iter().map(|a| a.name.as_str()).collect();
                json(&serde_json::json!(names))
            }
            [file] => {
                let (name, ext) = file.rsplit_once('.').unwrap_or((file, ""));
                let Some(a) = self.archive(name) else {
                    return status(404);
                };
                match ext {
                    "json" => json(&tilejson(a, host.unwrap_or("localhost"))),
                    "pmtiles" => raw(&a.path, req),
                    _ => status(404),
                }
            }
            [name, "metadata.json"] => match self.archive(name) {
                Some(a) => metadata(&a.index),
                None => status(404),
            },
            [name, "stats.json"] => match self.archive(name) {
                Some(a) => match compute_stats(&a.index, DEFAULT_LARGEST, false) {
                    Ok(report) => json(&serde_json::json!(report)),
                    Err(e) => error(&e),
                },
                None => status(404),
            },
            [name, z, x, y] => {
                let y = y.split_once('.').map_or(*y, |(y, _)| y);
                let (Ok(z), Ok(x), Ok(y)) = (z.parse(), x.parse(), y.parse()) else {
                    return status(404);
                };
                match self.archive(name) {
                    Some(a) => tile(&a.index, z, x, y),
                    None => status(404),
                }
            }
            _ => status(404),
        }
    }

    fn archive(&mut self, name: &str) -> Option<&Served> {
        let a = self.archives.iter_mut().find(|a| a.name == name)?;
        a.refresh();
        Some(a)
    }
}

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes()).expect("ASCII header")
}

fn status(code: u16) -> Response<Box<dyn Read + Send>> {
    Response::new(
        StatusCode(code),
        vec![],
        Box::new(std::io::empty()),
        Some(0),
        None,
    )
}

fn body(bytes: Vec<u8>, content_type: &str) -> Response<Box<dyn Read + Send>> {
    let len = bytes.len();
    Response::new(
        StatusCode(200),
        vec![header("Content-Type", content_type)],
        Box::new(std::io::Cursor::new(bytes)),
        Some(len),
        None,
    )
}

fn json(value: &serde_json::Value) -> Response<Box<dyn Read + Send>> {
    body(value.to_string().into_bytes(), "application/json")
}

fn error(e: &dyn std::fmt::Display) -> Response<Box<dyn Read + Send>> {
    log::warn!("{e}");
    body(e.to_string().into_bytes(), "text/plain; charset=utf-8").with_status_code(500)
}

/// The archive's metadata JSON, decompressed, byte for byte.
fn metadata(index: &ArchiveIndex) -> Response<Box<dyn Read + Send>> {
    let h = index.header();
    match decompress_capped(
        index.metadata_raw(),
        h.internal_compression,
        MAX_INTERNAL_BYTES,
    ) {
        Ok(bytes) => body(bytes, "application/json"),
        Err(e) => error(&e),
    }
}

/// One tile, 204 when the archive does not address it. Gzip bodies go out
/// as stored under `Content-Encoding: gzip`; other codecs are decompressed
/// here, since a browser may not accept them over plain HTTP.
fn tile(index: &ArchiveIndex, z: u8, x: u32, y: u32) -> Response<Box<dyn Read + Send>> {
    let range = match index.find(z, x, y) {
        Ok(Some(r)) => r,
        Ok(None) => return status(204),
        Err(e) => return error(&e),
    };
    // The length is the directory's claim: capped before the read allocates
    // it, as `decode` does (#510).
    if range.len() as u64 > MAX_TILE_BYTES {
        return error(&format!(
            "tile {z}/{x}/{y}: compressed body of {} bytes exceeds the {MAX_TILE_BYTES}-byte tile ceiling",
            range.len()
        ));
    }
    let bytes = match index.read_range(range) {
        Ok(b) => b,
        Err(e) => return error(&e),
    };
    let h = index.header();
    let content_type = match h.tile_type {
        TileType::Mvt => "application/vnd.mapbox-vector-tile",
        TileType::Png => "image/png",
        TileType::Jpeg => "image/jpeg",
        TileType::Webp => "image/webp",
        TileType::Avif => "image/avif",
        _ => "application/octet-stream",
    };
    match h.tile_compression {
        Compression::Gzip => {
            body(bytes, content_type).with_header(header("Content-Encoding", "gzip"))
        }
        Compression::None | Compression::Unknown => body(bytes, content_type),
        c => match decompress(&bytes, c) {
            Ok(plain) => body(plain, content_type),
            Err(e) => error(&e),
        },
    }
}

/// TileJSON 3.0 for one archive, its tile URLs on the host the client used.
/// `encoding` is the vector source's MapLibre option (`mlt` or `mvt`).
fn tilejson(a: &Served, host: &str) -> serde_json::Value {
    let h = a.index.header();
    let meta = a.index.metadata_json().unwrap_or_default();
    let (encoding, ext) = match h.tile_type {
        TileType::Mlt => ("mlt", "mlt"),
        TileType::Mvt => ("mvt", "mvt"),
        _ => ("raster", "tile"),
    };
    let mut tj = serde_json::json!({
        "tilejson": "3.0.0",
        "name": meta.get("name").and_then(|v| v.as_str()).unwrap_or(&a.name),
        "tiles": [format!(
            "http://{host}/{}/{{z}}/{{x}}/{{y}}.{ext}",
            utf8_percent_encode(&a.name, NON_ALPHANUMERIC)
        )],
        "minzoom": h.min_zoom,
        "maxzoom": h.max_zoom,
        // An archive written without bounds stores an inverted box that
        // MapLibre rejects; the whole world stands in for it.
        "bounds": a.index.bounds().map_or([-180.0, -85.051_129, 180.0, 85.051_129], |b| {
            [b.lng_min, b.lat_min, b.lng_max, b.lat_max]
        }),
        "center": [h.center_lon, h.center_lat, h.center_zoom],
        "encoding": encoding,
        "variable_depth": a.index.is_variable_depth(),
        "vector_layers": meta.get("vector_layers").cloned().unwrap_or_default(),
    });
    for key in ["attribution", "description"] {
        if let Some(v) = meta.get(key) {
            tj[key] = v.clone();
        }
    }
    tj
}

/// The archive file itself, honouring one `Range: bytes=` span, so a
/// PMTiles client (pmtiles.io, the `pmtiles` JS library) can read it.
fn raw(path: &Path, req: &Request) -> Response<Box<dyn Read + Send>> {
    let (mut file, version) = match File::open(path).and_then(|f| {
        let m = f.metadata()?;
        Ok((f, stamp(&m)))
    }) {
        Ok(v) => v,
        Err(e) => return error(&e),
    };
    let len = version.0;
    let tag = etag(version);
    let get = |name: &'static str| {
        req.headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().trim())
    };
    // A client holding offsets from an older version must not get bytes
    // of this one: `If-Match` fails, and `If-Range` falls back to the whole
    // file (RFC 9110 13.1.1, 13.1.5).
    if get("If-Match").is_some_and(|m| m != "*" && m.split(',').all(|t| t.trim() != tag)) {
        return status(412).with_header(header("ETag", &tag));
    }
    let range = get("Range")
        .filter(|_| get("If-Range").is_none_or(|t| t == tag))
        .map(|v| parse_range(v, len));
    let (code, start, end) = match range {
        None | Some(RangeSpec::Ignore) => (200, 0, len),
        Some(RangeSpec::Span(s, e)) => (206, s, e),
        Some(RangeSpec::Unsatisfiable) => {
            return status(416).with_header(header("Content-Range", &format!("bytes */{len}")));
        }
    };
    if let Err(e) = file.seek(SeekFrom::Start(start)) {
        return error(&e);
    }
    let n = end - start;
    let mut headers = vec![
        header("Content-Type", "application/vnd.pmtiles"),
        header("Accept-Ranges", "bytes"),
        header("ETag", &tag),
        header("Access-Control-Expose-Headers", "ETag, Content-Range"),
    ];
    if code == 206 {
        headers.push(header(
            "Content-Range",
            &format!("bytes {start}-{}/{len}", end - 1),
        ));
    }
    let Ok(n_usize) = usize::try_from(n) else {
        return status(416);
    };
    Response::new(
        StatusCode(code),
        headers,
        Box::new(file.take(n)),
        Some(n_usize),
        None,
    )
}

#[derive(Debug, PartialEq)]
enum RangeSpec {
    /// Serve `start..end` with 206.
    Span(u64, u64),
    /// No byte of the file is in the range: 416.
    Unsatisfiable,
    /// A form this server does not do (several spans, another unit):
    /// RFC 9110 allows answering with the whole file.
    Ignore,
}

/// One `Range` header against a file of `len` bytes.
fn parse_range(value: &str, len: u64) -> RangeSpec {
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return RangeSpec::Ignore;
    };
    if spec.contains(',') {
        return RangeSpec::Ignore;
    }
    let Some((a, b)) = spec.trim().split_once('-') else {
        return RangeSpec::Ignore;
    };
    let span = match (a.parse::<u64>(), b.parse::<u64>()) {
        // bytes=a-b, inclusive; an end past the file is clamped.
        (Ok(s), Ok(e)) if s <= e => (s, e.saturating_add(1).min(len)),
        // bytes=a-
        (Ok(s), Err(_)) if b.is_empty() => (s, len),
        // bytes=-n: the last n bytes.
        (Err(_), Ok(n)) if a.is_empty() && n > 0 => (len.saturating_sub(n), len),
        _ => return RangeSpec::Ignore,
    };
    if span.0 >= len {
        RangeSpec::Unsatisfiable
    } else {
        RangeSpec::Span(span.0, span.1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_header_forms() {
        use RangeSpec::*;
        assert_eq!(parse_range("bytes=0-9", 100), Span(0, 10));
        assert_eq!(parse_range("bytes=90-200", 100), Span(90, 100));
        assert_eq!(parse_range("bytes=95-", 100), Span(95, 100));
        assert_eq!(parse_range("bytes=-5", 100), Span(95, 100));
        assert_eq!(parse_range("bytes=-500", 100), Span(0, 100));
        assert_eq!(parse_range("bytes=100-", 100), Unsatisfiable);
        assert_eq!(parse_range("bytes=0-1,5-6", 100), Ignore);
        assert_eq!(parse_range("items=0-1", 100), Ignore);
        assert_eq!(parse_range("bytes=5-1", 100), Ignore);
        assert_eq!(parse_range("bytes=-0", 100), Ignore);
    }
}
